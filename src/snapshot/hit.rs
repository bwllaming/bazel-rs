//! The hit path (PLAN-L2 §1c, update 1 row W5): at the snapshot check point
//! a `build`, `test` or `run` with `BAZEL_RS_SNAPSHOT=on` looks up the
//! snapshot of its key G and, when every input it recorded is unchanged,
//! skips module resolution, loading and analysis: it replays what planning
//! printed and executes the saved plan.
//!
//! ```text
//! open G.snap, check the header; read + decode INPUTS and EVENTS
//! in parallel (fd 2 captured, discarded):
//!   validation (per tree by default)          crate::snapshot::validate
//!   replay of the local repository rules      crate::snapshot::replay
//!   read + check + decode the PLAN section    crate::snapshot::codec
//!   load the digest table (warm-up)           crate::digest_memo
//! all matched -> the workspace status (if the plan has status placeholders,
//!   its stderr must be empty) -> hit
//! ```
//!
//! Validation only reads, and nothing prints: a miss falls through to the
//! full analysis exactly as if no snapshot existed. A panic while decoding,
//! validating or replaying is caught and is a miss. The caller writes the
//! window bytes to stderr, then finalizes and executes the decoded request
//! like a planned one (crate::request).
//!
//! After a hit: keys a content check settled are written back (the snapshot
//! file is rewritten in the background: [`Session::finish`]), and an
//! execution that failed because an input inside the repository store is
//! missing removes the snapshot with a one-line hint ([`Session::failed`]):
//! the full analysis repairs such a tree, which the per-tree validation
//! does not look into.

use crate::request::PlannedRequest;
use crate::snapshot::format::{self, Section};
use crate::snapshot::key::Key;
use crate::snapshot::store::{self, Reader};
use crate::snapshot::validate::{self, Refresh, Validated};
use crate::workspace_status::{ComputedWorkspaceStatus, WorkspaceStatusOptions};
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

/// What the replay of local repository rules needs from this command line
/// (PLAN-L2 update 1 §(a): the current options and environment, never the
/// recorded ones).
pub struct ReplayInputs {
    pub options: crate::module_graph::ResolverOptions,
    /// The repository environment (the client environment with
    /// `--repo_env`), crate::repo_env.
    pub environment: Vec<(OsString, OsString)>,
    pub downloads_enabled: bool,
}

/// One hit attempt's inputs.
pub struct Attempt<'a> {
    pub cache: &'a Path,
    pub key: Key,
    pub replay: ReplayInputs,
    /// `--workspace_status_command` / `--embed_label` and the workspace
    /// (the status of a plan with status placeholders).
    pub workspace_status: WorkspaceStatusOptions,
    pub workspace: &'a Path,
    /// `BAZEL_RS_SNAPSHOT=verify`: keep what the full analysis is compared
    /// with ([`Evidence`]) and write refreshed keys back only once it
    /// agreed ([`Session::start_deferred_refresh`]).
    pub verify: bool,
}

/// What verify mode compares a hit's shadow full analysis with
/// (crate::snapshot::verify).
pub struct Evidence {
    /// The PLAN section's bytes (the encoding is deterministic: the full
    /// analysis's request must encode to exactly these).
    pub plan: Vec<u8>,
    /// The INPUTS section, decoded: the policy's recording and the replay
    /// records.
    pub inputs: format::Inputs,
}

/// Where the hit's time went.
#[derive(Clone, Debug, Default)]
pub struct Timings {
    /// Opening the file, reading and decoding INPUTS and EVENTS.
    pub inputs: Duration,
    pub validation: Duration,
    pub replay: Duration,
    /// Reading, checking and decoding the PLAN section.
    pub decode: Duration,
    pub status: Duration,
    pub total: Duration,
}

/// A confirmed hit.
pub struct Hit {
    pub request: PlannedRequest,
    /// What planning printed (the caller writes it to stderr first).
    pub window_stderr: Vec<u8>,
    /// The workspace status for the request's placeholders (computed now).
    pub status: Option<ComputedWorkspaceStatus>,
    pub session: Session,
    pub summary: String,
    pub timings: Timings,
    /// Verify mode only ([`Attempt::verify`]).
    pub evidence: Option<Evidence>,
}

/// Why there is no hit.
pub type Miss = String;

/// A miss whose reason is `reason` (formatted for EXPLAIN).
fn miss(reason: impl Into<String>) -> Result<Box<Hit>, Miss> {
    Err(reason.into())
}

/// Try the snapshot of `attempt.key`.
pub fn attempt(attempt: Attempt<'_>) -> Result<Box<Hit>, Miss> {
    let started = Instant::now();
    let reader = match Reader::open(attempt.cache, &attempt.key) {
        Ok(Some(reader)) => reader,
        Ok(None) => return miss("no snapshot for this key"),
        Err(error) => return miss(format!("unreadable snapshot: {error}")),
    };
    let decoded = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let inputs = format::decode_inputs(&reader.section(Section::Inputs)?)?;
        let window = format::decode_events(&reader.section(Section::Events)?)?;
        Ok::<_, String>((inputs, window))
    }));
    let (inputs, window_stderr) = match decoded {
        Ok(Ok(decoded)) => decoded,
        Ok(Err(error)) => return miss(format!("unreadable snapshot: {error}")),
        Err(_) => return miss("unreadable snapshot: the decoder panicked"),
    };
    let mut timings = Timings {
        inputs: started.elapsed(),
        ..Timings::default()
    };
    // Validation, replay and decoding in parallel. They print nothing, but a
    // replayed definition's `.bzl` closure may `print()` (already part of
    // the recorded window bytes): swallowed.
    let capture = crate::snapshot::capture::start(crate::snapshot::capture::Mode::Discard).ok();
    let tables = inputs.tables.clone();
    let keep_plan = attempt.verify;
    let parallel = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        std::thread::scope(|scope| {
            let plan = scope.spawn(|| {
                let started = Instant::now();
                let bytes = reader.section(Section::Plan)?;
                let size = bytes.len();
                let request = crate::snapshot::codec::decode(&bytes)?;
                let kept = keep_plan.then_some(bytes);
                Ok::<_, String>((request, size, kept, started.elapsed()))
            });
            let replay = (!inputs.replay.is_empty()).then(|| {
                let records = &inputs.replay;
                let replay = &attempt.replay;
                scope.spawn(move || replay_records(tables, replay, records))
            });
            let warm = scope.spawn(|| {
                // The execution tail looks up action inputs in the digest
                // table: load it now, beside the replay.
                if let Some(table) = crate::digest_memo::installed() {
                    let _ = table.loaded();
                }
            });
            let validation = validate::validate_for_hit(&inputs.recording);
            let plan = plan
                .join()
                .unwrap_or_else(|_| Err("the plan decoder panicked".to_owned()));
            let replay = replay.map(|handle| {
                handle
                    .join()
                    .unwrap_or_else(|_| Err("the replay panicked".to_owned()))
            });
            let _ = warm.join();
            (validation, plan, replay)
        })
    }));
    let captured = capture.map(|capture| capture.stop(Duration::from_millis(200)));
    let (validation, plan, replay) = match parallel {
        Ok(results) => results,
        Err(_) => return miss("the validation panicked"),
    };
    // EXPLAIN names the first failing check and, when their replay differs
    // too, the mismatching local repositories (both ran to completion).
    let replay_failure = match &replay {
        Some(Err(reason)) => Some(reason.clone()),
        _ => None,
    };
    let validated: Validated = match (validation, replay_failure) {
        (Ok(validated), None) => validated,
        (Ok(_), Some(replay)) => return miss(replay),
        (Err(first), None) => return miss(first),
        (Err(first), Some(replay)) => return miss(format!("{first}; and {replay}")),
    };
    timings.validation = validated.elapsed;
    let replayed = match replay {
        Some(result) => {
            let (count, wall) = result?;
            timings.replay = wall;
            count
        }
        None => 0,
    };
    let (request, plan_bytes, kept_plan, decode) =
        plan.map_err(|error| format!("unreadable plan: {error}"))?;
    timings.decode = decode;
    // The workspace status of the plan's status placeholders: computed
    // now, before anything prints. On a full analysis its stderr is part of
    // the window (a status command that wrote to stderr kept the run from
    // being saved): one that writes now is a miss, never a misplaced print.
    let status_started = Instant::now();
    let needs_status = request.nodes.iter().any(|node| {
        matches!(
            node.action.extra.transform,
            Some(crate::action::Transform::WorkspaceStatus { .. })
        )
    });
    let status = if needs_status {
        let client = crate::action_env::client_environment();
        let (computed, stderr) = crate::workspace_status::compute_quiet(
            &attempt.workspace_status,
            attempt.workspace,
            &client,
            crate::workspace_status::current_time_millis(&client),
        );
        if !stderr.is_empty() {
            return miss(format!(
                "the workspace status command wrote {} bytes to stderr",
                stderr.len()
            ));
        }
        Some(match computed {
            Ok(status) => ComputedWorkspaceStatus::Ok(status),
            Err(error) => ComputedWorkspaceStatus::Failed(error),
        })
    } else {
        None
    };
    timings.status = status_started.elapsed();
    reader.touch();
    timings.total = started.elapsed();
    let summary = format!(
        "hit {}: {} checks validated ({} content checks, {} trees verified) in {} ms on {} thread(s); {} local repositories replayed in {} ms; {} actions ({} bytes) decoded in {} ms; window stderr {} bytes{}; total {} ms",
        attempt.key.short(),
        validated.checks,
        validated.content_checks,
        validated.trees_verified,
        millis(timings.validation),
        validated.threads,
        replayed,
        millis(timings.replay),
        request.nodes.len(),
        plan_bytes,
        millis(timings.decode),
        window_stderr.len(),
        captured
            .as_ref()
            .filter(|captured| !captured.bytes.is_empty())
            .map(|captured| format!(
                " (a replay printed {} bytes, swallowed)",
                captured.bytes.len()
            ))
            .unwrap_or_default(),
        millis(timings.total),
    );
    let mut session = Session {
        cache: attempt.cache.to_path_buf(),
        key: attempt.key,
        refresh: None,
        deferred: None,
    };
    let evidence = if attempt.verify {
        // Verify mode: nothing is written back before the full analysis
        // agreed (a mismatch is never saved).
        if !validated.refresh.is_empty() {
            session.deferred = Some((reader, validated.refresh));
        }
        Some(Evidence {
            plan: kept_plan.unwrap_or_default(),
            inputs,
        })
    } else {
        if !validated.refresh.is_empty() {
            session.refresh = Some(refresh_in_background(
                reader,
                inputs,
                attempt.cache,
                validated.refresh,
            ));
        }
        None
    };
    Ok(Box::new(Hit {
        request,
        window_stderr,
        status,
        session,
        summary,
        timings,
        evidence,
    }))
}

fn millis(duration: Duration) -> String {
    format!("{:.1}", duration.as_secs_f64() * 1000.0)
}

/// Replay every local repository rule against the pinned tables with this
/// command's options and environment; every record must reproduce its
/// identity, print nothing and still have its published tree. Returns the
/// number replayed and the replay's wall time.
fn replay_records(
    tables: Option<crate::snapshot::replay::PinnedTables>,
    inputs: &ReplayInputs,
    records: &[crate::snapshot::replay::ReplayRecord],
) -> Result<(usize, Duration), String> {
    let tables = tables.ok_or("replay records without pinned tables")?;
    let context = crate::snapshot::replay::CaptureContext {
        options: inputs.options.clone(),
        environment: crate::repository_environment::Snapshot::with_reader(Arc::new(
            crate::repository_environment::FixedReader::new(inputs.environment.iter().cloned()),
        )),
        downloads_enabled: inputs.downloads_enabled,
        main_root: tables.main_root.clone(),
        source_cache: tables.source_cache.clone(),
    };
    let report = crate::snapshot::replay::replay(tables, &context, records);
    // Every mismatching local repository, in recorded order (EXPLAIN).
    let mismatches: Vec<String> = report
        .replayed
        .iter()
        .filter_map(|replayed| match &replayed.mismatch {
            Some(reason) => Some(format!("local repository {}: {reason}", replayed.canonical)),
            None if replayed.output_bytes > 0 => Some(format!(
                "local repository {} printed {} bytes",
                replayed.canonical, replayed.output_bytes
            )),
            None => None,
        })
        .collect();
    if let Some(first) = mismatches.first() {
        const NAMED: usize = 3;
        let mut reason = first.clone();
        if mismatches.len() > 1 {
            reason.push_str(&format!(
                " (and {} more: {})",
                mismatches.len() - 1,
                mismatches[1..]
                    .iter()
                    .take(NAMED)
                    .map(|text| text.trim_start_matches("local repository "))
                    .collect::<Vec<_>>()
                    .join("; ")
            ));
            if mismatches.len() - 1 > NAMED {
                reason.push_str(", ...");
            }
        }
        return Err(reason);
    }
    if report.replayed.len() != records.len() {
        return Err(format!(
            "{} of {} local repositories replayed",
            report.replayed.len(),
            records.len()
        ));
    }
    for record in records {
        if !std::fs::symlink_metadata(&record.root).is_ok_and(|metadata| metadata.is_dir()) {
            return Err(format!(
                "local repository {}: its published tree {} is missing",
                record.canonical,
                record.root.display()
            ));
        }
    }
    Ok((records.len(), report.wall))
}

/// Rewrite the snapshot with the keys the validation settled (PLAN-L2 §1c:
/// "if any keys were refreshed, rewrite the file in the background"): the
/// INPUTS section re-encoded, the other sections copied from the file the
/// hit read.
fn refresh_in_background(
    reader: Reader,
    mut inputs: format::Inputs,
    cache: &Path,
    refresh: Vec<Refresh>,
) -> JoinHandle<()> {
    let cache = cache.to_path_buf();
    std::thread::spawn(move || {
        validate::apply_refresh(&mut inputs.recording, &refresh);
        let mut sections = vec![(
            Section::Inputs,
            format::encode_inputs(&inputs.recording, &inputs.replay, inputs.tables.as_ref()),
        )];
        for kind in [Section::Events, Section::Plan, Section::Stats] {
            match reader.section(kind) {
                Ok(bytes) => sections.push((kind, bytes)),
                Err(_) if kind == Section::Stats => {}
                Err(_) => return,
            }
        }
        let key = Key {
            g: reader.header.key,
            binary: reader.header.binary,
        };
        drop(reader);
        let _ = store::write_in_background(&cache, &key, sections).commit();
    })
}

/// After a hit: finish the refresh, or heal a stale snapshot.
pub struct Session {
    cache: PathBuf,
    key: Key,
    refresh: Option<JoinHandle<()>>,
    /// Verify mode: the refresh waits for the full analysis to agree.
    deferred: Option<(Reader, Vec<Refresh>)>,
}

impl Session {
    /// The snapshot's key.
    pub fn key(&self) -> &Key {
        &self.key
    }

    /// The cache whose store holds the snapshot.
    pub fn cache(&self) -> &Path {
        &self.cache
    }

    /// Verify mode, once the full analysis agreed with the hit: write the
    /// keys the validation settled back (in the background, as a hit of
    /// `BAZEL_RS_SNAPSHOT=on` does at once).
    pub fn start_deferred_refresh(&mut self, inputs: format::Inputs) {
        if let Some((reader, refresh)) = self.deferred.take() {
            self.refresh = Some(refresh_in_background(reader, inputs, &self.cache, refresh));
        }
    }

    /// The command succeeded: let the background refresh complete.
    pub fn finish(mut self) {
        if let Some(refresh) = self.refresh.take() {
            let _ = refresh.join();
        }
    }

    /// Execution failed with `error`. When it names an input inside the
    /// repository store that no longer exists, the per-tree validation
    /// missed a damaged tree (the full analysis repairs it): remove the
    /// snapshot and say so in one line.
    pub fn failed(mut self, error: &str) {
        if let Some(refresh) = self.refresh.take() {
            let _ = refresh.join();
        }
        let store = self.cache.join("repositories");
        let canonical = std::fs::canonicalize(&store).ok();
        let missing = quoted_paths(error).find(|path| {
            (path.starts_with(&store) || canonical.as_ref().is_some_and(|c| path.starts_with(c)))
                && std::fs::symlink_metadata(path)
                    .is_err_and(|error| error.kind() == std::io::ErrorKind::NotFound)
        });
        if let Some(path) = missing
            && store::remove(&self.cache, &self.key).is_ok()
        {
            eprintln!(
                "bazel-rs: removed the analysis snapshot of this build: its input {} is missing from the repository store; the next build re-analyzes and repairs it",
                path.display()
            );
        }
    }
}

/// The `'...'`-quoted paths of an error text (crate::action's I/O errors
/// quote the path: `cannot inspect '<path>': ...`).
fn quoted_paths(text: &str) -> impl Iterator<Item = PathBuf> + '_ {
    text.split('\'')
        .skip(1)
        .step_by(2)
        .filter(|candidate| candidate.starts_with('/'))
        .map(PathBuf::from)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quoted_paths_are_the_absolute_ones_between_quotes() {
        let error = "action 'x' failed: cannot inspect '/c/repositories/trees/ab/f.go': No such file or directory (os error 2); see '/tmp/y'";
        let paths: Vec<PathBuf> = quoted_paths(error).collect();
        assert_eq!(
            paths,
            [
                PathBuf::from("/c/repositories/trees/ab/f.go"),
                PathBuf::from("/tmp/y")
            ]
        );
    }

    #[test]
    fn a_failed_hit_on_a_missing_store_input_removes_the_snapshot() {
        let cache = tempfile::tempdir().unwrap();
        let key = Key {
            g: [3; 32],
            binary: [4; 32],
        };
        store::write_in_background(cache.path(), &key, vec![(Section::Inputs, Vec::new())])
            .commit()
            .unwrap();
        let session = |cache: &Path| Session {
            cache: cache.to_path_buf(),
            key,
            refresh: None,
            deferred: None,
        };
        let present = cache.path().join("repositories/trees/ab");
        std::fs::create_dir_all(&present).unwrap();
        // An existing path, or a missing one outside the store: kept.
        session(cache.path()).failed(&format!("cannot inspect '{}': x", present.display()));
        session(cache.path()).failed("cannot inspect '/surely/missing/w5': x");
        assert!(store::path(cache.path(), &key).exists());
        session(cache.path()).failed(&format!(
            "cannot inspect '{}': No such file or directory",
            present.join("gone.go").display()
        ));
        assert!(!store::path(cache.path(), &key).exists());
    }
}
