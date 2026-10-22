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

/// Validate every observation of `recording` against the current
/// filesystem and environment on `threads` scoped threads (never stops
/// early; see the module docs).
pub fn dry_run(recording: &Recording, threads: usize) -> Report {
    let started = Instant::now();
    let threads = threads.max(1);
    let mut work: Vec<Work<'_>> = recording
        .observations
        .iter()
        .map(|(path, observation)| Work::Observation(path, observation))
        .collect();
    for (index, repository) in recording.repositories.iter().enumerate() {
        work.extend(
            repository
                .entries
                .iter()
                .map(|entry| Work::Integrity(index, entry)),
        );
    }
    let cursor = AtomicUsize::new(0);
    let tallies = Mutex::new(Vec::new());
    let stale_repositories = Mutex::new(BTreeSet::new());
    std::thread::scope(|scope| {
        for _ in 0..threads {
            scope.spawn(|| {
                let mut tally = Tally::default();
                let mut stale = BTreeSet::new();
                loop {
                    let start = cursor.fetch_add(BLOCK, Ordering::Relaxed);
                    if start >= work.len() {
                        break;
                    }
                    for item in &work[start..(start + BLOCK).min(work.len())] {
                        match item {
                            Work::Observation(path, observation) => {
                                tally.observations += 1;
                                match check(path, observation) {
                                    Ok(Checked::Plain) => {}
                                    Ok(Checked::Stat) => tally.trusted_by_stat += 1,
                                    Ok(Checked::Content { refresh }) => {
                                        tally.content_checks += 1;
                                        tally.refreshable += usize::from(refresh);
                                    }
                                    Err(text) => tally.mismatch(text),
                                }
                            }
                            Work::Integrity(repository, entry) => {
                                tally.integrity += 1;
                                if let Err(text) = check_integrity(entry) {
                                    // A racy entry only sends its tree to the
                                    // verification; a changed one too (which
                                    // repairs or confirms it): neither is a
                                    // mismatch of the snapshot by itself.
                                    if stale.insert(*repository) && !entry.racy {
                                        tally.examples.push(text);
                                    }
                                }
                            }
                        }
                    }
                }
                tallies
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .push(tally);
                stale_repositories
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .extend(stale);
            });
        }
    });
    let mut report = Report {
        threads,
        ..Report::default()
    };
    for tally in tallies
        .into_inner()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
    {
        report.observations += tally.observations;
        report.trusted_by_stat += tally.trusted_by_stat;
        report.content_checks += tally.content_checks;
        report.refreshable += tally.refreshable;
        report.integrity += tally.integrity;
        report.mismatches += tally.mismatches;
        report.examples.extend(tally.examples);
    }
    report.examples.sort();
    report.examples.truncate(EXAMPLES);
    report.repositories_to_verify = stale_repositories
        .into_inner()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .len();
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
}
