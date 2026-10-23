//! Snapshot validation (PLAN-L2 §1c): decide, by reading only, whether every
//! input planning observed is unchanged. W3 runs it as a timed dry run over
//! the in-memory recording (crate::snapshot::record); W5 will run the same
//! checks over a stored snapshot's INPUTS section before a hit.
//!
//! Per observation, on `min(available_parallelism, 16)` scoped threads over
//! the path-sorted list (contiguous blocks keep directory locality):
//!
//! * STAT / LSTAT: the type class is unchanged; a symlink's target too.
//! * READ: lstat (a symlink: its target's stat) must be a regular file; an
//!   unchanged stat key outside the racy window is trusted, else the file is
//!   re-read (its fstat must equal the lstat, or the check fails as
//!   Unstable) and the SHA-256 compared — a match with a now-settled key is
//!   counted as refreshable.
//! * READDIR: the same pattern over the directory's stat key and the
//!   projected listing digest.
//! * CANON / READLINK: the resulting path is unchanged.
//! * ENV: the digest of the process environment's value (names an option
//!   fixes are part of the key, not checked).
//! * INTEGRITY: the lstat key of every verified repository entry; a changed
//!   or racy entry sends its repository to the existing tree verification
//!   (counted here, not run).
//!
//! A dry run never stops early: it counts every mismatch, so the report
//! shows the full cost of a hit's validation and every stale input.
//!
//! Policy (PLAN-L2 update 1, decision (1)): by default a hit validates per
//! tree ([`Policy::PerTree`]: the observations outside verified repository
//! trees and those at or below a symlink of a tree, plus each tree's root
//! and manifest — a content-addressed tree whose root and manifest are
//! unchanged stands for everything inside it, like Bazel trusting
//! repository markers, but not for what its links reach: the glob walker and
//! the BUILD lookups record a host directory listed through a tree's link
//! under the logical path inside the tree); the existing
//! `--verify_repo_tampering` / `BAZEL_RS_VERIFY_REPOS=full` switch selects
//! every INTEGRITY entry too ([`Policy::Full`]). Below
//! [`SINGLE_THREAD_CHECKS`] checks one thread runs them (thread start-up
//! dominates otherwise).

use crate::observed_fs::{
    FileKind, IntegrityEntry, Observation, Op, Outcome, Recording, RepositoryRecord, StatKey,
    escape, listing_digest,
};
use sha2::{Digest, Sha256};
use std::collections::BTreeSet;
use std::fs;
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::path::Path;
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

const BLOCK: usize = 256;
const EXAMPLES: usize = 20;

/// What a dry run found and how long it took.
#[derive(Clone, Debug, Default)]
pub struct Report {
    pub elapsed: Duration,
    pub threads: usize,
    /// Filesystem observations checked.
    pub observations: usize,
    /// READ / READDIR observations trusted by an unchanged, settled stat key.
    pub trusted_by_stat: usize,
    /// READ / READDIR observations re-read and compared by content (racy or
    /// changed key).
    pub content_checks: usize,
    /// Content checks that matched with a key settled now (a store would
    /// refresh the key).
    pub refreshable: usize,
    /// Environment variables checked (fixed names excluded).
    pub env: usize,
    /// Integrity entries checked.
    pub integrity: usize,
    /// Repositories whose tree verification a hit would have to run (a
    /// changed or racy integrity entry).
    pub repositories_to_verify: usize,
    pub mismatches: usize,
    /// The first mismatches, in path order per thread block.
    pub examples: Vec<String>,
    /// The first observations that needed a content check.
    pub content_checked: Vec<String>,
    /// Thread time spent on filesystem observations and on integrity
    /// entries (summed over the threads).
    pub observation_time: Duration,
    pub integrity_time: Duration,
    /// Wall time of the phases: building the work list, the filesystem
    /// checks (all threads), the environment checks.
    pub setup_time: Duration,
    pub checks_time: Duration,
    pub env_time: Duration,
}

#[derive(Default)]
struct Tally {
    observations: usize,
    trusted_by_stat: usize,
    content_checks: usize,
    refreshable: usize,
    integrity: usize,
    mismatches: usize,
    examples: Vec<String>,
    content_checked: Vec<String>,
    observation_time: Duration,
    integrity_time: Duration,
}

impl Tally {
    fn mismatch(&mut self, text: String) {
        self.mismatches += 1;
        if self.examples.len() < EXAMPLES {
            self.examples.push(text);
        }
    }
}

enum Checked {
    /// A type, path or errno compare.
    Plain,
    /// Trusted by an unchanged, settled stat key.
    Stat,
    /// Re-read and compared; `refresh`: the key is settled now (`key`, the
    /// key a store refreshes the observation to).
    Content { refresh: bool, key: StatKey },
}

enum Work<'a> {
    Observation(&'a Path, &'a Observation),
    Integrity(usize, &'a IntegrityEntry),
}

fn now_ns() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| {
            i64::try_from(elapsed.as_nanos()).unwrap_or(i64::MAX)
        })
}

fn errno(error: &io::Error) -> i32 {
    error.raw_os_error().unwrap_or(-1)
}

fn same(path: &Path, op: &Op, expected: &Outcome, got: Outcome) -> Result<Checked, String> {
    if *expected == got {
        Ok(Checked::Plain)
    } else {
        Err(format!(
            "{} {}: recorded {expected}, now {got}",
            op.name(),
            escape(path.as_os_str())
        ))
    }
}

/// lstat, or a symlink's followed stat (the READ key rule of
/// crate::observed_fs).
fn read_key_metadata(path: &Path) -> io::Result<fs::Metadata> {
    let metadata = fs::symlink_metadata(path)?;
    if metadata.file_type().is_symlink() {
        fs::metadata(path)
    } else {
        Ok(metadata)
    }
}

fn check(path: &Path, observation: &Observation) -> Result<Checked, String> {
    let op = &observation.op;
    let shown = || escape(path.as_os_str());
    match (op, &observation.outcome) {
        (Op::Stat, expected) => {
            let got = match fs::metadata(path) {
                Ok(metadata) => Outcome::Kind(FileKind::of(metadata.file_type())),
                Err(error) => Outcome::Error(errno(&error)),
            };
            same(path, op, expected, got)
        }
        (Op::Lstat, expected) => {
            let got = match fs::symlink_metadata(path) {
                Ok(metadata) if metadata.file_type().is_symlink() => match fs::read_link(path) {
                    Ok(target) => Outcome::Link(target),
                    Err(error) => Outcome::Error(errno(&error)),
                },
                Ok(metadata) => Outcome::Kind(FileKind::of(metadata.file_type())),
                Err(error) => Outcome::Error(errno(&error)),
            };
            same(path, op, expected, got)
        }
        (Op::Canon, expected) => {
            let got = match fs::canonicalize(path) {
                Ok(resolved) => Outcome::Path(resolved),
                Err(error) => Outcome::Error(errno(&error)),
            };
            same(path, op, expected, got)
        }
        (Op::ReadLink, expected) => {
            let got = match fs::read_link(path) {
                Ok(target) => Outcome::Path(target),
                Err(error) => Outcome::Error(errno(&error)),
            };
            same(path, op, expected, got)
        }
        (Op::Read, Outcome::Error(code)) => match fs::read(path) {
            Err(error) if errno(&error) == *code => Ok(Checked::Plain),
            Err(error) => Err(format!(
                "READ {}: recorded error:{code}, now {error}",
                shown()
            )),
            Ok(_) => Err(format!(
                "READ {}: recorded error:{code}, now readable",
                shown()
            )),
        },
        (Op::Read, Outcome::Read { sha256, key, racy }) => {
            let metadata =
                read_key_metadata(path).map_err(|error| format!("READ {}: {error}", shown()))?;
            if !metadata.is_file() {
                return Err(format!("READ {}: no longer a regular file", shown()));
            }
            let current = StatKey::of(&metadata);
            if !racy && current == *key {
                return Ok(Checked::Stat);
            }
            let start = now_ns();
            let mut file =
                fs::File::open(path).map_err(|error| format!("READ {}: {error}", shown()))?;
            let mut digest = Sha256::new();
            io::copy(&mut file, &mut digest)
                .map_err(|error| format!("READ {}: {error}", shown()))?;
            let after = file
                .metadata()
                .map_err(|error| format!("READ {}: {error}", shown()))?;
            if StatKey::of(&after) != current {
                return Err(format!(
                    "READ {}: changed while validated (Unstable)",
                    shown()
                ));
            }
            let actual: [u8; 32] = digest.finalize().into();
            if actual != *sha256 {
                return Err(format!("READ {}: content changed", shown()));
            }
            Ok(Checked::Content {
                refresh: !current.racy(start),
                key: current,
            })
        }
        (Op::ReadDir(_), Outcome::Error(code)) => match fs::read_dir(path) {
            Err(error) if errno(&error) == *code => Ok(Checked::Plain),
            Err(error) => Err(format!(
                "READDIR {}: recorded error:{code}, now {error}",
                shown()
            )),
            Ok(entries) => match entries.collect::<Result<Vec<_>, _>>() {
                Err(error) if errno(&error) == *code => Ok(Checked::Plain),
                _ => Err(format!(
                    "READDIR {}: recorded error:{code}, now listable",
                    shown()
                )),
            },
        },
        (
            Op::ReadDir(projection),
            Outcome::Listing {
                digest, key, racy, ..
            },
        ) => {
            let metadata =
                fs::metadata(path).map_err(|error| format!("READDIR {}: {error}", shown()))?;
            let current = StatKey::of(&metadata);
            if !racy && current == *key {
                return Ok(Checked::Stat);
            }
            let start = now_ns();
            let mut entries = Vec::new();
            for entry in
                fs::read_dir(path).map_err(|error| format!("READDIR {}: {error}", shown()))?
            {
                let entry = entry.map_err(|error| format!("READDIR {}: {error}", shown()))?;
                let name = entry.file_name();
                let kind = entry.file_type().ok().map(FileKind::of);
                if projection.keeps(&name, kind) {
                    entries.push((name, kind.map_or(b'e', FileKind::letter)));
                }
            }
            let after =
                fs::metadata(path).map_err(|error| format!("READDIR {}: {error}", shown()))?;
            if StatKey::of(&after) != current {
                return Err(format!(
                    "READDIR {}: changed while validated (Unstable)",
                    shown()
                ));
            }
            if listing_digest(&mut entries) != *digest {
                return Err(format!(
                    "READDIR[{projection}] {}: listing changed",
                    shown()
                ));
            }
            Ok(Checked::Content {
                refresh: !current.racy(start),
                key: current,
            })
        }
        (op, outcome) => Err(format!(
            "{} {}: malformed record {outcome}",
            op.name(),
            shown()
        )),
    }
}

fn check_integrity(entry: &IntegrityEntry) -> Result<(), String> {
    let got = match fs::symlink_metadata(&entry.path) {
        Ok(metadata) => Ok((FileKind::of(metadata.file_type()), StatKey::of(&metadata))),
        Err(error) => Err(errno(&error)),
    };
    if got != entry.state {
        return Err(format!(
            "INTEGRITY {}: lstat changed",
            escape(entry.path.as_os_str())
        ));
    }
    if entry.racy {
        return Err(format!(
            "INTEGRITY {}: racy entry (needs the tree verification)",
            escape(entry.path.as_os_str())
        ));
    }
    Ok(())
}

/// The roots of the verified repository trees, sorted.
fn verified_roots(recording: &Recording) -> Vec<&Path> {
    let mut roots: Vec<&Path> = recording
        .repositories
        .iter()
        .map(|repository| repository.root.as_path())
        .collect();
    roots.sort();
    roots
}

/// The symlinks of the verified repository trees (their INTEGRITY entries
/// of kind symlink below the root, or a root that is a link), sorted. The
/// integrity walk never descends into a link, so no two of them nest (as
/// the trees themselves do not).
fn tree_links(recording: &Recording) -> Vec<&Path> {
    let mut links: Vec<&Path> = recording
        .repositories
        .iter()
        .flat_map(|repository| {
            repository.entries.iter().filter(|entry| {
                matches!(entry.state, Ok((FileKind::Symlink, _)))
                    && entry.path.starts_with(&repository.root)
            })
        })
        .map(|entry| entry.path.as_path())
        .collect();
    links.sort();
    links
}

/// Whether `path` lies inside (or is) one of the sorted, non-nested `roots`.
fn inside(roots: &[&Path], path: &Path) -> bool {
    let at = roots.partition_point(|root| *root <= path);
    at > 0 && path.starts_with(roots[at - 1])
}

/// How many observations lie inside verified repository trees, and how many
/// of those at or below a symlink of their tree (which the per-tree policy
/// validates too).
pub fn observations_inside_verified_trees(recording: &Recording) -> (usize, usize) {
    let roots = verified_roots(recording);
    let links = tree_links(recording);
    let mut counts = (0, 0);
    for (path, _) in &recording.observations {
        if inside(&roots, path) {
            counts.0 += 1;
            counts.1 += usize::from(inside(&links, path));
        }
    }
    counts
}

/// The recording a per-tree summary policy would validate (PLAN-L2 §7 "when
/// to escalate": repository tree checks above 20 ms on a hit): the
/// observations outside the verified repository trees — a content-addressed
/// tree whose root and manifest are unchanged stands for everything
/// recorded inside it — and those at or below a symlink of a verified tree:
/// the tree fixes the link, not what it reaches (the glob walker lists a
/// host directory through a link, and `glob::is_subpackage` stats its BUILD
/// files, under the logical path inside the tree; a header added to that
/// host directory must be a miss) — plus per tree only the root and
/// manifest integrity entries.
pub fn per_tree_summary(recording: &Recording) -> Recording {
    let roots = verified_roots(recording);
    let links = tree_links(recording);
    let observations = recording
        .observations
        .iter()
        .filter(|(path, _)| !inside(&roots, path) || inside(&links, path))
        .cloned()
        .collect();
    let repositories = recording
        .repositories
        .iter()
        .map(|repository| RepositoryRecord {
            kind: repository.kind,
            name: repository.name.clone(),
            root: repository.root.clone(),
            manifest: repository.manifest.clone(),
            entries: repository
                .entries
                .iter()
                .filter(|entry| is_head(repository, entry))
                .cloned()
                .collect(),
        })
        .collect();
    Recording {
        window_start_ns: recording.window_start_ns,
        window: recording.window,
        observations,
        env: recording.env.clone(),
        repositories,
        crossed: recording.crossed.clone(),
        taints: recording.taints.clone(),
        plan_registered: recording.plan_registered.clone(),
    }
}

/// A tree's root or manifest entry (what the per-tree policy validates).
fn is_head(repository: &RepositoryRecord, entry: &IntegrityEntry) -> bool {
    entry.path == repository.root
        || repository
            .manifest
            .as_ref()
            .is_some_and(|manifest| entry.path == *manifest)
}

/// Path order on raw bytes with `/` before every other byte: the order of
/// `Path::cmp` for normalized absolute paths, without parsing components
/// (a large recording's selection compares paths a few hundred thousand
/// times).
pub(crate) fn path_order(a: &[u8], b: &[u8]) -> std::cmp::Ordering {
    let key = |byte: u8| if byte == b'/' { 0 } else { u16::from(byte) + 1 };
    let at = mismatch(a, b);
    match (a.get(at), b.get(at)) {
        (Some(x), Some(y)) => key(*x).cmp(&key(*y)),
        _ => a.len().cmp(&b.len()),
    }
}

/// The length of the common prefix of `a` and `b`, eight bytes at a time
/// (recorded paths share long prefixes: the cache, a tree's root).
fn mismatch(a: &[u8], b: &[u8]) -> usize {
    let length = a.len().min(b.len());
    let mut at = 0;
    while at + 8 <= length {
        let x = u64::from_le_bytes(a[at..at + 8].try_into().expect("8 bytes"));
        let y = u64::from_le_bytes(b[at..at + 8].try_into().expect("8 bytes"));
        if x != y {
            return at + ((x ^ y).trailing_zeros() / 8) as usize;
        }
        at += 8;
    }
    while at < length && a[at] == b[at] {
        at += 1;
    }
    at
}

/// [`inside`] over raw bytes sorted by [`path_order`]: the greatest root
/// not after `path` contains it, if any does (roots never nest).
fn inside_bytes(roots: &[&[u8]], path: &[u8]) -> bool {
    let at = roots.partition_point(|root| path_order(root, path) != std::cmp::Ordering::Greater);
    at > 0 && {
        let root = roots[at - 1];
        path.starts_with(root) && (path.len() == root.len() || path[root.len()] == b'/')
    }
}

/// [`per_tree_summary`] of an owned recording, without copying it.
fn per_tree_summary_owned(mut recording: Recording) -> Recording {
    fn sorted_bytes(paths: Vec<&Path>) -> Vec<&[u8]> {
        let mut paths: Vec<&[u8]> = paths
            .into_iter()
            .map(|path| path.as_os_str().as_bytes())
            .collect();
        paths.sort_by(|a, b| path_order(a, b));
        paths
    }
    let keep: Vec<bool> = {
        let roots = sorted_bytes(verified_roots(&recording));
        let links = sorted_bytes(tree_links(&recording));
        recording
            .observations
            .iter()
            .map(|(path, _)| {
                let path = path.as_os_str().as_bytes();
                !inside_bytes(&roots, path) || inside_bytes(&links, path)
            })
            .collect()
    };
    let mut keep = keep.into_iter();
    recording
        .observations
        .retain(|_| keep.next().unwrap_or(true));
    for repository in &mut recording.repositories {
        let entries = std::mem::take(&mut repository.entries);
        repository.entries = entries
            .into_iter()
            .filter(|entry| is_head(repository, entry))
            .collect();
    }
    recording
}

/// Which checks a hit runs (see the module docs).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Policy {
    /// Observations outside verified trees or at or below a tree's
    /// symlinks, plus each tree's root and manifest (the default,
    /// [`per_tree_summary`]).
    PerTree,
    /// Every observation and every INTEGRITY entry (`--verify_repo_tampering`
    /// / `BAZEL_RS_VERIFY_REPOS=full`).
    Full,
}

impl Policy {
    /// The policy of this command: full under the store's full verification
    /// mode (crate::repository_store::verify_mode), else per tree.
    pub fn current() -> Self {
        match crate::repository_store::verify_mode() {
            crate::repository_store::VerifyMode::Full => Self::Full,
            crate::repository_store::VerifyMode::Stat => Self::PerTree,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Self::PerTree => "per-tree",
            Self::Full => "full",
        }
    }

    /// The recording this policy validates.
    pub fn select(self, recording: &Recording) -> Recording {
        match self {
            Self::PerTree => per_tree_summary(recording),
            Self::Full => recording.clone(),
        }
    }

    /// [`Policy::select`] of an owned recording (no copy).
    pub fn select_owned(self, recording: Recording) -> Recording {
        match self {
            Self::PerTree => per_tree_summary_owned(recording),
            Self::Full => recording,
        }
    }
}

/// Below this many checks (observations plus integrity entries) a
/// validation runs on one thread.
pub const SINGLE_THREAD_CHECKS: usize = 4000;

/// The validation threads for `recording`: one below
/// [`SINGLE_THREAD_CHECKS`] checks, else min(available parallelism, 16).
pub fn threads_for(recording: &Recording) -> usize {
    let checks = recording.observations.len()
        + recording
            .repositories
            .iter()
            .map(|repository| repository.entries.len())
            .sum::<usize>();
    if checks < SINGLE_THREAD_CHECKS {
        1
    } else {
        std::thread::available_parallelism()
            .map(usize::from)
            .unwrap_or(1)
            .min(16)
    }
}

impl Tally {
    /// Check one observation or integrity entry (see [`dry_run`]); a
    /// repository with a changed or racy entry joins `stale`.
    fn check(&mut self, stale: &mut BTreeSet<usize>, item: &Work<'_>) {
        let began = Instant::now();
        match item {
            Work::Observation(path, observation) => {
                self.observations += 1;
                match check(path, observation) {
                    Ok(Checked::Plain) => {}
                    Ok(Checked::Stat) => self.trusted_by_stat += 1,
                    Ok(Checked::Content { refresh, .. }) => {
                        self.content_checks += 1;
                        self.refreshable += usize::from(refresh);
                        if self.content_checked.len() < EXAMPLES {
                            self.content_checked.push(format!(
                                "{} {}",
                                observation.op.name(),
                                escape(path.as_os_str())
                            ));
                        }
                    }
                    Err(text) => self.mismatch(text),
                }
                self.observation_time += began.elapsed();
            }
            Work::Integrity(repository, entry) => {
                self.integrity += 1;
                if let Err(text) = check_integrity(entry) {
                    // A racy entry only sends its tree to the verification;
                    // a changed one too (which repairs or confirms it):
                    // neither is a mismatch of the snapshot by itself.
                    if stale.insert(*repository) && !entry.racy {
                        self.examples.push(text);
                    }
                }
                self.integrity_time += began.elapsed();
            }
        }
    }
}

/// Validate every observation of `recording` against the current
/// filesystem and environment on `threads` scoped threads (never stops
/// early; see the module docs). One thread runs inline, in place: no work
/// list is built (an allocation in a large, fragmented heap can cost
/// milliseconds — the record mode runs this at the end of a full analysis).
pub fn dry_run(recording: &Recording, threads: usize) -> Report {
    let started = Instant::now();
    let threads = threads.max(1);
    let items = || {
        let integrity =
            recording
                .repositories
                .iter()
                .enumerate()
                .flat_map(|(index, repository)| {
                    repository
                        .entries
                        .iter()
                        .map(move |entry| Work::Integrity(index, entry))
                });
        recording
            .observations
            .iter()
            .map(|(path, observation)| Work::Observation(path, observation))
            .chain(integrity)
    };
    let mut tallies = Vec::new();
    let mut stale_repositories = BTreeSet::new();
    let (setup_time, checks_time) = if threads == 1 {
        let setup_time = started.elapsed();
        let checks_started = Instant::now();
        let mut tally = Tally::default();
        for item in items() {
            tally.check(&mut stale_repositories, &item);
        }
        let checks_time = checks_started.elapsed();
        tallies.push(tally);
        (setup_time, checks_time)
    } else {
        let work: Vec<Work<'_>> = items().collect();
        let setup_time = started.elapsed();
        let checks_started = Instant::now();
        let cursor = AtomicUsize::new(0);
        let shared = Mutex::new((Vec::new(), BTreeSet::new()));
        let worker = || {
            let mut tally = Tally::default();
            let mut stale = BTreeSet::new();
            loop {
                let start = cursor.fetch_add(BLOCK, Ordering::Relaxed);
                if start >= work.len() {
                    break;
                }
                for item in &work[start..(start + BLOCK).min(work.len())] {
                    tally.check(&mut stale, item);
                }
            }
            let mut shared = shared
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            shared.0.push(tally);
            shared.1.extend(stale);
        };
        std::thread::scope(|scope| {
            for _ in 0..threads {
                scope.spawn(worker);
            }
        });
        let checks_time = checks_started.elapsed();
        (tallies, stale_repositories) = shared
            .into_inner()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        (setup_time, checks_time)
    };
    let mut report = Report {
        threads,
        setup_time,
        checks_time,
        ..Report::default()
    };
    for tally in tallies {
        report.observations += tally.observations;
        report.trusted_by_stat += tally.trusted_by_stat;
        report.content_checks += tally.content_checks;
        report.refreshable += tally.refreshable;
        report.integrity += tally.integrity;
        report.mismatches += tally.mismatches;
        report.examples.extend(tally.examples);
        report.content_checked.extend(tally.content_checked);
        report.observation_time += tally.observation_time;
        report.integrity_time += tally.integrity_time;
    }
    report.examples.sort();
    report.examples.truncate(EXAMPLES);
    report.content_checked.sort();
    report.content_checked.truncate(EXAMPLES);
    report.repositories_to_verify = stale_repositories.len();
    let env_started = Instant::now();
    for (name, observation) in &recording.env {
        if observation.fixed {
            continue;
        }
        report.env += 1;
        let digest = std::env::var_os(name)
            .map(|value| -> [u8; 32] { Sha256::digest(value.as_bytes()).into() });
        if digest != observation.digest {
            report.mismatches += 1;
            if report.examples.len() < EXAMPLES {
                report
                    .examples
                    .push(format!("ENV {}: value changed", escape(name)));
            }
        }
    }
    report.env_time = env_started.elapsed();
    report.elapsed = started.elapsed();
    report
}

// ---- the hit's validation (L2-W5) ------------------------------------------

/// A key a hit's validation settled: the snapshot can be rewritten with it
/// (crate::snapshot::hit), so later hits trust it by stat.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Refresh {
    /// `recording.observations[index]` (a READ or READDIR content check
    /// matched, and the file is settled now).
    Observation { index: usize, key: StatKey },
    /// `recording.repositories[tree].entries[entry]`, a racy entry whose
    /// tree verified against its manifest and is settled now.
    Integrity {
        tree: usize,
        entry: usize,
        key: StatKey,
    },
}

/// What a successful hit validation did.
#[derive(Clone, Debug, Default)]
pub struct Validated {
    /// Observations, environment variables and integrity entries checked.
    pub checks: usize,
    /// READ / READDIR observations re-read (racy or changed key).
    pub content_checks: usize,
    /// Repository trees verified against their manifests (a racy entry).
    pub trees_verified: usize,
    pub refresh: Vec<Refresh>,
    pub threads: usize,
    pub elapsed: Duration,
}

enum Item {
    Observation(usize),
    Integrity(usize, usize),
}

/// One check of a hit: `Ok(refresh)` or the mismatch.
fn check_for_hit(
    recording: &Recording,
    item: &Item,
    verified: &Mutex<BTreeSet<usize>>,
    tally: &mut Validated,
) -> Result<Option<Refresh>, String> {
    match *item {
        Item::Observation(index) => {
            let (path, observation) = &recording.observations[index];
            match check(path, observation)? {
                Checked::Plain | Checked::Stat => Ok(None),
                Checked::Content { refresh, key } => {
                    tally.content_checks += 1;
                    Ok(refresh.then_some(Refresh::Observation { index, key }))
                }
            }
        }
        Item::Integrity(tree, entry_index) => {
            let repository = &recording.repositories[tree];
            let entry = &repository.entries[entry_index];
            let got = match fs::symlink_metadata(&entry.path) {
                Ok(metadata) => Ok((FileKind::of(metadata.file_type()), StatKey::of(&metadata))),
                Err(error) => Err(errno(&error)),
            };
            if got != entry.state {
                return Err(format!(
                    "INTEGRITY {}: lstat changed (repository {})",
                    escape(entry.path.as_os_str()),
                    repository.name
                ));
            }
            if !entry.racy {
                return Ok(None);
            }
            // Recorded within the racy window: the tree's content decides
            // (once per tree), then the unchanged key may be settled now.
            let start = now_ns();
            let already = verified
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .contains(&tree);
            if !already {
                let manifest = repository.manifest.clone().ok_or_else(|| {
                    format!(
                        "INTEGRITY {}: racy entry of a tree without a manifest",
                        escape(entry.path.as_os_str())
                    )
                })?;
                let id = repository
                    .root
                    .file_name()
                    .map(|name| name.to_string_lossy().into_owned())
                    .unwrap_or_default();
                crate::repository_store::verify_published_tree(
                    &crate::repository_store::PublishedTree {
                        kind: repository.kind,
                        id,
                        root: repository.root.clone(),
                        manifest,
                    },
                )
                .map_err(|error| format!("repository {}: {error}", repository.name))?;
                verified
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .insert(tree);
                tally.trees_verified += 1;
            }
            Ok(match got {
                Ok((_, key)) if !key.racy(start) => Some(Refresh::Integrity {
                    tree,
                    entry: entry_index,
                    key,
                }),
                _ => None,
            })
        }
    }
}

/// Validate a snapshot's INPUTS for a hit (PLAN-L2 §1c): every check of
/// [`dry_run`], stopping at the first mismatch, which is the error (the
/// reason a hit missed). A changed integrity entry misses (the full
/// analysis repairs or re-verifies its tree); a racy one verifies its tree
/// against its manifest. Below [`SINGLE_THREAD_CHECKS`] checks one thread
/// runs them in path order; otherwise min(available parallelism, 16)
/// threads stop at the first mismatch any of them finds (the earliest of
/// those in path order is reported).
pub fn validate_for_hit(recording: &Recording) -> Result<Validated, String> {
    let started = Instant::now();
    let mut validated = Validated::default();
    for (name, observation) in &recording.env {
        if observation.fixed {
            continue;
        }
        validated.checks += 1;
        let digest = std::env::var_os(name)
            .map(|value| -> [u8; 32] { Sha256::digest(value.as_bytes()).into() });
        if digest != observation.digest {
            return Err(format!("ENV {}: value changed", escape(name)));
        }
    }
    let items: Vec<Item> = (0..recording.observations.len())
        .map(Item::Observation)
        .chain(
            recording
                .repositories
                .iter()
                .enumerate()
                .flat_map(|(tree, repository)| {
                    (0..repository.entries.len()).map(move |entry| Item::Integrity(tree, entry))
                }),
        )
        .collect();
    let threads = threads_for(recording);
    validated.threads = threads;
    validated.checks += items.len();
    let verified = Mutex::new(BTreeSet::new());
    if threads <= 1 {
        for item in &items {
            if let Some(refresh) = check_for_hit(recording, item, &verified, &mut validated)? {
                validated.refresh.push(refresh);
            }
        }
    } else {
        let cursor = AtomicUsize::new(0);
        let failed = std::sync::atomic::AtomicBool::new(false);
        let results: Mutex<Vec<(usize, Result<Validated, String>)>> = Mutex::new(Vec::new());
        std::thread::scope(|scope| {
            for _ in 0..threads {
                scope.spawn(|| {
                    let mut tally = Validated::default();
                    let mut outcome = Ok(());
                    'blocks: while !failed.load(Ordering::Relaxed) {
                        let start = cursor.fetch_add(BLOCK, Ordering::Relaxed);
                        if start >= items.len() {
                            break;
                        }
                        for (offset, item) in items[start..(start + BLOCK).min(items.len())]
                            .iter()
                            .enumerate()
                        {
                            match check_for_hit(recording, item, &verified, &mut tally) {
                                Ok(Some(refresh)) => tally.refresh.push(refresh),
                                Ok(None) => {}
                                Err(error) => {
                                    failed.store(true, Ordering::Relaxed);
                                    outcome = Err((start + offset, error));
                                    break 'blocks;
                                }
                            }
                        }
                    }
                    let entry = match outcome {
                        Ok(()) => (usize::MAX, Ok(tally)),
                        Err((index, error)) => (index, Err(error)),
                    };
                    results
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .push(entry);
                });
            }
        });
        let mut results = results
            .into_inner()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        results.sort_by_key(|(index, _)| *index);
        for (_, result) in results {
            let tally = result?;
            validated.content_checks += tally.content_checks;
            validated.trees_verified += tally.trees_verified;
            validated.refresh.extend(tally.refresh);
        }
    }
    validated.elapsed = started.elapsed();
    Ok(validated)
}

/// Apply settled keys to the recording (a racy observation or entry that a
/// hit's content check confirmed becomes trusted by stat).
pub fn apply_refresh(recording: &mut Recording, refresh: &[Refresh]) {
    for refresh in refresh {
        match *refresh {
            Refresh::Observation { index, key } => {
                if let Some((_, observation)) = recording.observations.get_mut(index) {
                    match &mut observation.outcome {
                        Outcome::Read { key: old, racy, .. }
                        | Outcome::Listing { key: old, racy, .. } => {
                            *old = key;
                            *racy = false;
                        }
                        _ => {}
                    }
                }
            }
            Refresh::Integrity { tree, entry, key } => {
                if let Some(entry) = recording
                    .repositories
                    .get_mut(tree)
                    .and_then(|repository| repository.entries.get_mut(entry))
                {
                    if let Ok((_, old)) = &mut entry.state {
                        *old = key;
                    }
                    entry.racy = false;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::observed_fs::Projection;
    use std::collections::BTreeMap;

    fn observation(op: Op, outcome: Outcome) -> Observation {
        Observation { op, outcome }
    }

    fn key(path: &Path) -> StatKey {
        StatKey::of(&fs::symlink_metadata(path).unwrap())
    }

    #[test]
    fn unchanged_inputs_validate_and_edits_are_found() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        fs::write(root.join("BUILD"), b"x").unwrap();
        fs::create_dir(root.join("sub")).unwrap();
        let sha: [u8; 32] = Sha256::digest(b"x").into();
        let mut listing = vec![(std::ffi::OsString::from("sub"), b'd')];
        let recording = Recording {
            window_start_ns: 0,
            window: None,
            observations: vec![
                (
                    root.join("BUILD"),
                    observation(
                        Op::Read,
                        Outcome::Read {
                            sha256: sha,
                            key: key(&root.join("BUILD")),
                            racy: true,
                        },
                    ),
                ),
                (
                    root.to_path_buf(),
                    observation(
                        Op::ReadDir(Projection::Directories),
                        Outcome::Listing {
                            digest: listing_digest(&mut listing),
                            entries: 1,
                            key: StatKey::default(),
                            racy: false,
                        },
                    ),
                ),
                (
                    root.join("missing"),
                    observation(Op::Stat, Outcome::Error(2)),
                ),
            ],
            env: Vec::new(),
            repositories: Vec::new(),
            crossed: Vec::new(),
            taints: BTreeMap::new(),
            plan_registered: None,
        };
        let report = dry_run(&recording, 4);
        assert_eq!(report.mismatches, 0, "{:?}", report.examples);
        assert_eq!(report.content_checks, 2);
        // A new file does not change a directories-only listing; a new
        // subdirectory and a content edit do.
        fs::write(root.join("new.go"), b"package x").unwrap();
        assert_eq!(dry_run(&recording, 2).mismatches, 0);
        fs::create_dir(root.join("sub2")).unwrap();
        fs::write(root.join("BUILD"), b"y").unwrap();
        fs::write(root.join("missing"), b"").unwrap();
        let report = dry_run(&recording, 1);
        assert_eq!(report.mismatches, 3, "{:?}", report.examples);
    }

    #[test]
    fn per_tree_summary_keeps_outside_observations_and_tree_heads() {
        use crate::observed_fs::{FileKind, IntegrityEntry, RepositoryRecord};
        let entry = |path: &str, kind: FileKind| IntegrityEntry {
            path: path.into(),
            state: Ok((kind, StatKey::default())),
            racy: false,
        };
        let stat = |path: &str| (path.into(), observation(Op::Stat, Outcome::Error(2)));
        let recording = Recording {
            observations: vec![
                (
                    "/c/trees/a".into(),
                    observation(Op::Canon, Outcome::Error(2)),
                ),
                stat("/c/trees/a/BUILD"),
                // Read through the tree's links: a host directory listed
                // (and its BUILD looked up) under the logical path.
                stat("/c/trees/a/inc"),
                stat("/c/trees/a/inc/sub/BUILD"),
                stat("/c/trees/a/include"),
                stat("/c/trees/a/lib/link"),
                stat("/c/trees/a/lib/linked"),
                stat("/c/trees/ab/x"),
                // A tree whose root is a link: everything below it.
                stat("/c/trees/r/BUILD"),
                stat("/ws/BUILD"),
            ],
            repositories: vec![
                RepositoryRecord {
                    kind: "registry",
                    name: "a".into(),
                    root: "/c/trees/a".into(),
                    manifest: Some("/c/trees/a.json".into()),
                    entries: vec![
                        entry("/c/trees/a.json", FileKind::File),
                        entry("/c/trees/a", FileKind::Dir),
                        entry("/c/trees/a/BUILD", FileKind::File),
                        entry("/c/trees/a/inc", FileKind::Symlink),
                        entry("/c/trees/a/lib", FileKind::Dir),
                        entry("/c/trees/a/lib/link", FileKind::Symlink),
                    ],
                },
                RepositoryRecord {
                    kind: "direct",
                    name: "r".into(),
                    root: "/c/trees/r".into(),
                    manifest: Some("/c/manifests/r.json".into()),
                    entries: vec![
                        entry("/c/manifests/r.json", FileKind::Symlink),
                        entry("/c/trees/r", FileKind::Symlink),
                    ],
                },
            ],
            ..Recording::default()
        };
        assert_eq!(observations_inside_verified_trees(&recording), (8, 4));
        let summary = per_tree_summary(&recording);
        let kept: Vec<_> = summary
            .observations
            .iter()
            .map(|(path, _)| path.to_str().unwrap())
            .collect();
        assert_eq!(
            kept,
            [
                "/c/trees/a/inc",
                "/c/trees/a/inc/sub/BUILD",
                "/c/trees/a/lib/link",
                "/c/trees/ab/x",
                "/c/trees/r/BUILD",
                "/ws/BUILD"
            ]
        );
        let entries: Vec<_> = summary.repositories[0]
            .entries
            .iter()
            .map(|entry| entry.path.to_str().unwrap())
            .collect();
        assert_eq!(entries, ["/c/trees/a.json", "/c/trees/a"]);
        assert_eq!(Policy::Full.select(&recording).observations.len(), 10);
        // The owned selection (the save's) selects the same.
        let owned = Policy::PerTree.select_owned(recording.clone());
        assert_eq!(owned.observations, summary.observations);
        assert_eq!(owned.repositories, summary.repositories);
        assert_eq!(
            Policy::Full.select_owned(recording.clone()).observations,
            recording.observations
        );
    }

    /// A hit's validation: racy reads are compared by content (a fresh file
    /// is not settled, so nothing is refreshed), and the first change of a
    /// read, a variable or an integrity entry is the miss's reason.
    #[test]
    fn a_hit_validation_compares_racy_reads_and_names_the_first_change() {
        use crate::observed_fs::{EnvObservation, IntegrityEntry, RepositoryRecord};
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        fs::write(root.join("BUILD"), b"x").unwrap();
        fs::create_dir(root.join("tree")).unwrap();
        let sha: [u8; 32] = Sha256::digest(b"x").into();
        let tree_state = Ok((FileKind::Dir, key(&root.join("tree"))));
        let recording = Recording {
            observations: vec![(
                root.join("BUILD"),
                observation(
                    Op::Read,
                    Outcome::Read {
                        sha256: sha,
                        key: key(&root.join("BUILD")),
                        racy: true,
                    },
                ),
            )],
            env: vec![(
                "BAZEL_RS_SURELY_UNSET_W5".into(),
                EnvObservation {
                    digest: None,
                    fixed: false,
                },
            )],
            repositories: vec![RepositoryRecord {
                kind: "direct",
                name: "r".into(),
                root: root.join("tree"),
                manifest: None,
                entries: vec![IntegrityEntry {
                    path: root.join("tree"),
                    state: tree_state,
                    racy: false,
                }],
            }],
            ..Recording::default()
        };
        let validated = validate_for_hit(&recording).unwrap();
        assert_eq!(validated.checks, 3);
        assert_eq!(validated.content_checks, 1);
        assert!(validated.refresh.is_empty(), "{:?}", validated.refresh);
        fs::write(root.join("BUILD"), b"y").unwrap();
        let error = validate_for_hit(&recording).unwrap_err();
        assert!(error.contains("content changed"), "{error}");
        fs::write(root.join("BUILD"), b"x").unwrap();
        // A changed entry (a change within the timestamp granularity keeps
        // the key: such an entry is recorded as racy, see below).
        let mut changed = recording.clone();
        if let Ok((_, key)) = &mut changed.repositories[0].entries[0].state {
            key.size += 1;
        }
        let error = validate_for_hit(&changed).unwrap_err();
        assert!(
            error.starts_with("INTEGRITY ") && error.contains("(repository r)"),
            "{error}"
        );
        // A racy entry of a tree without a manifest cannot be verified.
        let mut racy = recording.clone();
        racy.repositories[0].entries[0].racy = true;
        let error = validate_for_hit(&racy).unwrap_err();
        assert!(error.contains("without a manifest"), "{error}");
        let mut changed = recording.clone();
        changed.env[0].1.digest = Some([0; 32]);
        let error = validate_for_hit(&changed).unwrap_err();
        assert_eq!(error, "ENV BAZEL_RS_SURELY_UNSET_W5: value changed");
    }

    /// The byte order agrees with `Path::cmp` on normalized absolute paths,
    /// and the byte containment test with [`inside`].
    #[test]
    fn the_byte_path_order_is_the_component_order() {
        let paths = [
            "/a",
            "/a/b",
            "/a-c",
            "/a.b",
            "/a/b/c",
            "/a0",
            "/ab",
            "/a/\u{e9}",
            "/b",
            "/a/b-c",
            "/a/b/",
        ];
        for a in paths {
            for b in paths {
                let (pa, pb) = (Path::new(a), Path::new(b));
                if a.ends_with('/') || b.ends_with('/') {
                    continue;
                }
                assert_eq!(
                    path_order(a.as_bytes(), b.as_bytes()),
                    pa.cmp(pb),
                    "{a} vs {b}"
                );
            }
        }
        let mut roots: Vec<&[u8]> = vec![b"/c/trees/a", b"/c/trees/a-x", b"/c/trees/b"];
        roots.sort_by(|a, b| path_order(a, b));
        let paths: Vec<&Path> = roots
            .iter()
            .map(|root| Path::new(std::ffi::OsStr::from_bytes(root)))
            .collect();
        for path in [
            "/c/trees/a",
            "/c/trees/a/f",
            "/c/trees/a-x/f",
            "/c/trees/ab",
            "/c/trees",
            "/c/trees/b/x/y",
            "/d",
        ] {
            assert_eq!(
                inside_bytes(&roots, path.as_bytes()),
                inside(&paths, Path::new(path)),
                "{path}"
            );
        }
    }

    #[test]
    fn refreshed_keys_settle_their_records() {
        use crate::observed_fs::{IntegrityEntry, RepositoryRecord};
        let fresh = StatKey {
            ino: 9,
            ..StatKey::default()
        };
        let mut recording = Recording {
            observations: vec![(
                "/a".into(),
                observation(
                    Op::Read,
                    Outcome::Read {
                        sha256: [1; 32],
                        key: StatKey::default(),
                        racy: true,
                    },
                ),
            )],
            repositories: vec![RepositoryRecord {
                kind: "registry",
                name: "r".into(),
                root: "/t".into(),
                manifest: None,
                entries: vec![IntegrityEntry {
                    path: "/t".into(),
                    state: Ok((FileKind::Dir, StatKey::default())),
                    racy: true,
                }],
            }],
            ..Recording::default()
        };
        apply_refresh(
            &mut recording,
            &[
                Refresh::Observation {
                    index: 0,
                    key: fresh,
                },
                Refresh::Integrity {
                    tree: 0,
                    entry: 0,
                    key: fresh,
                },
            ],
        );
        assert_eq!(
            recording.observations[0].1.outcome,
            Outcome::Read {
                sha256: [1; 32],
                key: fresh,
                racy: false
            }
        );
        assert_eq!(
            recording.repositories[0].entries[0].state,
            Ok((FileKind::Dir, fresh))
        );
        assert!(!recording.repositories[0].entries[0].racy);
    }
}
