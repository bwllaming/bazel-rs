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
    FileKind, IntegrityEntry, Observation, Op, Outcome, Recording, StatKey, escape, listing_digest,
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
    /// Re-read and compared; `refresh`: the key is settled now.
    Content { refresh: bool },
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
    let mut summary = recording.clone();
    summary
        .observations
        .retain(|(path, _)| !inside(&roots, path) || inside(&links, path));
    for repository in &mut summary.repositories {
        let root = repository.root.clone();
        let manifest = repository.manifest.clone();
        repository.entries.retain(|entry| {
            entry.path == root
                || manifest
                    .as_ref()
                    .is_some_and(|manifest| entry.path == *manifest)
        });
    }
    summary
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
                    Ok(Checked::Content { refresh }) => {
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
    }
}
