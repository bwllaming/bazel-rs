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
//! file is rewritten in the background: [`Session::finish`]), and a command
//! that failed — in the build part ([`Session::failed`]), in the tests or
//! after the build part ([`heal_after_failure`]) — heals a damaged tree the
//! hit trusted (the per-tree validation does not look into a tree's files,
//! a local repository's replay not into its published tree): when the
//! error names a path inside the repository store, or when a tree of the
//! snapshot no longer verifies (L2-W5g: the verified store trees as their
//! store's warm check does, the local repositories' published trees against
//! their executions), the snapshot is removed with a one-line hint and the
//! next run's full analysis repairs the tree.

use crate::request::PlannedRequest;
use crate::snapshot::format::{self, Section};
use crate::snapshot::key::Key;
use crate::snapshot::store::{self, Reader};
use crate::snapshot::validate::{self, Refresh, Validated};
use crate::workspace_status::{ComputedWorkspaceStatus, WorkspaceStatusOptions};
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
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
    /// The event log's context (a healed hit is logged there,
    /// crate::snapshot::telemetry).
    pub log: crate::snapshot::telemetry::Context,
    /// Incremental analysis may follow a miss (crate::snapshot::incremental):
    /// a failing validation does not cancel the replay and the plan's
    /// decode, and a miss whose only failures are changed observations
    /// keeps the decoded snapshot ([`attempt`]'s second result).
    pub incremental: bool,
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
    /// Checking the local repositories' published trees (beside the
    /// replay, L2-W5h).
    pub local_trees: Duration,
    /// Reading, checking and decoding the PLAN section.
    pub decode: Duration,
    /// Resolving the plan's portable `map_each` functions after the decode
    /// (crate::snapshot::mapper).
    pub mappers: Duration,
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

/// Try the snapshot of `attempt.key`. A miss with
/// [`Attempt::incremental`] whose only failing checks are observations
/// keeps the decoded snapshot and the changed observations.
pub fn attempt(
    attempt: Attempt<'_>,
) -> Result<Box<Hit>, (Miss, Option<Box<crate::snapshot::incremental::Base>>)> {
    let mut base = None;
    match attempt_keeping(attempt, &mut base) {
        Ok(hit) => Ok(hit),
        Err(miss) => Err((miss, base)),
    }
}

fn attempt_keeping(
    attempt: Attempt<'_>,
    base: &mut Option<Box<crate::snapshot::incremental::Base>>,
) -> Result<Box<Hit>, Miss> {
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
    // `BAZEL_RS_SNAPSHOT_DUMP=<file>`: the INPUTS this attempt validates and
    // replays, as text (scripts/snapshot_fs_audit.py `--hit` checks every
    // path the hit path touches against it).
    if let Some(dump) = std::env::var_os(crate::snapshot::record::DUMP_VAR) {
        let _ = crate::snapshot::record::write_dump(
            Path::new(&dump),
            &inputs.recording,
            &[format!("hit inputs of {}", attempt.key.short())],
            &inputs.replay,
            inputs.tables.as_ref(),
            None,
            None,
        );
    }
    // Validation, replay and decoding in parallel. They print nothing, but a
    // replayed definition's `.bzl` closure may `print()` (already part of
    // the recorded window bytes): swallowed. Without the capture such a
    // print would land in the wrong place: with replay records a capture
    // that cannot start is a miss (L2-W5f review F5).
    let capture = match crate::snapshot::capture::start(crate::snapshot::capture::Mode::Discard) {
        Ok(capture) => Some(capture),
        Err(error) if !inputs.replay.is_empty() => {
            return miss(format!("cannot capture stderr for the replay: {error}"));
        }
        Err(_) => None,
    };
    let tables = inputs.tables.clone();
    let tables_for_mappers = inputs.tables.clone();
    let keep_plan = attempt.verify;
    let incremental = attempt.incremental;
    // The first failure cancels what the others have not started yet (the
    // replay's definition loads and executions, the plan's chunks; L2-W5f
    // review F7): a miss's full analysis starts without waiting for the
    // ~0.1 s replay. EXPLAIN lets everything run to completion, to name
    // every mismatching local repository.
    let explain = crate::snapshot::explain();
    let cancel = AtomicBool::new(false);
    let failed = |failed: bool| {
        if failed && !explain {
            cancel.store(true, Ordering::Relaxed);
        }
    };
    let parallel = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        std::thread::scope(|scope| {
            let plan = scope.spawn(|| {
                let started = Instant::now();
                let decoded = reader.section(Section::Plan).and_then(|bytes| {
                    let size = bytes.len();
                    let request = crate::snapshot::codec::decode_cancellable(&bytes, &cancel)?;
                    let decoded_at = started.elapsed();
                    // The plan's portable `map_each` functions, from their
                    // modules loaded against the pinned tables
                    // (crate::snapshot::mapper): beside the validation,
                    // whose failure discards them with the plan.
                    let mappers = crate::snapshot::mapper::resolve_request(
                        &request,
                        tables_for_mappers.as_ref(),
                        &attempt.replay,
                    )
                    .map_err(|error| format!("{MAPPERS_MISS}{error}"))?;
                    Ok((
                        request,
                        size,
                        keep_plan.then_some(bytes),
                        decoded_at,
                        mappers,
                    ))
                });
                failed(decoded.is_err());
                decoded
            });
            let replay = (!inputs.replay.is_empty()).then(|| {
                let records = &inputs.replay;
                let replay = &attempt.replay;
                let (cancel, failed) = (&cancel, &failed);
                scope.spawn(move || {
                    let replayed = replay_records(tables, replay, records, cancel);
                    failed(replayed.is_err());
                    replayed
                })
            });
            // Beside the replay (L2-W5h): each local repository's published
            // tree still holds what its execution produced. The replay
            // reproduces an execution in a fresh stage; the build reads the
            // published tree, which nothing else on the hit looks into.
            let local = (!inputs.replay.is_empty()).then(|| {
                let records = &inputs.replay;
                let (cancel, failed) = (&cancel, &failed);
                scope.spawn(move || {
                    let checked = check_local_trees(records, cancel, explain);
                    failed(checked.is_err());
                    checked
                })
            });
            let warm = scope.spawn(|| {
                // The execution tail looks up action inputs in the digest
                // table: load it now, beside the replay.
                if let Some(table) = crate::digest_memo::installed() {
                    let _ = table.loaded();
                }
            });
            let validation = validate::validate_for_hit(
                &inputs.recording,
                &validate::Environment::new(&attempt.replay.environment),
            );
            // Incremental analysis: what changed, and whether it lies in
            // packages (decided while the plan decodes and the replay
            // runs; a change it cannot attribute cancels them as before).
            let classified = match &validation {
                Err(_) if incremental => {
                    let classified = crate::snapshot::incremental::classify_attempt(
                        &inputs,
                        &reader,
                        attempt.cache,
                        &attempt.replay.environment,
                    );
                    failed(classified.is_err());
                    Some(classified)
                }
                _ => {
                    failed(validation.is_err());
                    None
                }
            };
            let plan = plan
                .join()
                .unwrap_or_else(|_| Err("the plan decoder panicked".to_owned()));
            let replay = replay.map(|handle| {
                handle
                    .join()
                    .unwrap_or_else(|_| Err("the replay panicked".to_owned()))
            });
            let local = local.map(|handle| {
                handle.join().unwrap_or_else(|_| {
                    Err("the check of the local repositories' trees panicked".to_owned())
                })
            });
            let _ = warm.join();
            (validation, plan, replay, local, classified)
        })
    }));
    let captured = capture.map(|capture| capture.stop(Duration::from_millis(200)));
    let (validation, plan, replay, local, classified) = match parallel {
        Ok(results) => results,
        Err(_) => return miss("the validation panicked"),
    };
    // EXPLAIN names the first failing check and, when their replay or their
    // published trees differ too, the mismatching local repositories (all
    // ran to completion). A replay or tree check cancelled by another
    // failure (F7) is no reason of its own.
    let failure = |result: &Option<Result<(usize, Duration), String>>| match result {
        Some(Err(reason)) if reason != crate::snapshot::replay::CANCELLED => Some(reason.clone()),
        _ => None,
    };
    let failures: Vec<String> = [failure(&replay), failure(&local)]
        .into_iter()
        .flatten()
        .collect();
    let validated: Validated = match validation {
        Ok(validated) if failures.is_empty() => validated,
        Ok(_) => return miss(failures.join("; and ")),
        Err(first) => {
            let cancelled =
                |result: &Option<Result<(usize, Duration), String>>| matches!(result, Some(Err(_)));
            match classified {
                Some(Ok(classified))
                    if failures.is_empty() && !cancelled(&replay) && !cancelled(&local) =>
                {
                    // Only observations inside packages changed: keep what
                    // incremental analysis starts from.
                    match plan {
                        Ok((request, _, _, _, _)) => {
                            *base = Some(Box::new(crate::snapshot::incremental::Base {
                                inputs,
                                window: window_stderr,
                                request,
                                classified: Ok(classified),
                            }));
                        }
                        Err(error) => {
                            *base = Some(Box::new(crate::snapshot::incremental::Base::refused(
                                plan_miss(error),
                            )));
                        }
                    }
                }
                Some(Ok(_)) => {}
                Some(Err(reason)) => {
                    *base = Some(Box::new(crate::snapshot::incremental::Base::refused(
                        reason,
                    )));
                }
                None => {}
            }
            return miss(
                std::iter::once(first)
                    .chain(failures)
                    .collect::<Vec<_>>()
                    .join("; and "),
            );
        }
    };
    timings.validation = validated.elapsed;
    // Cancelled while the validation, the replay and the trees passed: the
    // plan failed, and its error is the reason.
    for cancelled in [&replay, &local] {
        if let Some(Err(cancelled)) = cancelled {
            plan.as_ref().map_err(|error| plan_miss(error.clone()))?;
            return miss(cancelled.clone());
        }
    }
    let replayed = match replay {
        Some(Ok((count, wall))) => {
            timings.replay = wall;
            count
        }
        _ => 0,
    };
    if let Some(Ok((_, wall))) = local {
        timings.local_trees = wall;
    }
    let (request, plan_bytes, kept_plan, decode, mappers) = plan.map_err(plan_miss)?;
    timings.decode = decode;
    timings.mappers = mappers.elapsed;
    // The workspace status of the plan's status placeholders — or of a plan
    // whose planning computed it without any (`--nobuild` with a consumer
    // of `ctx.info_file`: the command still ran; L2-W5f review F2): computed
    // now, before anything prints. On a full analysis its stderr is part of
    // the window (a status command that wrote to stderr kept the run from
    // being saved): one that writes now is a miss, never a misplaced print.
    let status_started = Instant::now();
    let needs_status = inputs.workspace_status || request.has_status_placeholders();
    let status = if needs_status {
        let client = crate::action_env::client_environment();
        let (computed, stderr) = crate::workspace_status::compute_quiet(
            &attempt.workspace_status,
            attempt.workspace,
            &client,
            crate::workspace_status::current_time_millis(&client),
        );
        if !stderr.is_empty() {
            // The full analysis prints it while planning and never saves
            // such a run, so this snapshot cannot hit while the command
            // writes to stderr: it goes, and later runs run the command
            // once, in their full analysis (this miss ran it twice).
            let _ = store::remove(attempt.cache, &attempt.key);
            return miss(format!(
                "the workspace status command wrote {} bytes to stderr (the snapshot is removed)",
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
        "hit {}: {} checks validated ({} content checks, {} trees verified) in {} ms on {} thread(s); {} local repositories replayed in {} ms, their published trees checked in {} ms; {} actions ({} bytes) decoded in {} ms, their {} map_each function(s) ({} distinct, {} module(s) loaded) resolved in {} ms; window stderr {} bytes{}; total {} ms",
        attempt.key.short(),
        validated.checks,
        validated.content_checks,
        validated.trees_verified,
        millis(timings.validation),
        validated.threads,
        replayed,
        millis(timings.replay),
        millis(timings.local_trees),
        request.nodes.len(),
        plan_bytes,
        millis(timings.decode),
        mappers.mappers,
        mappers.functions,
        mappers.modules,
        millis(timings.mappers),
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
        trees: Trees::of(&inputs),
        log: attempt.log,
    };
    // The hit uses every tree the snapshot names, and the trees of the local
    // rules it replayed: their last use (docs/CACHE_GC.md).
    for tree in &inputs.recording.repositories {
        crate::cache_gc::note_tree_used(&tree.root);
    }
    for record in &inputs.replay {
        crate::cache_gc::note_tree_used(&record.root);
    }
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
                session.log.clone(),
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

/// The workspace status of a saved plan computed now, quietly, when
/// `needed` (a hit, or an incremental analysis that kept the saved plan): a
/// status command that writes to stderr is an error (the full analysis
/// prints it while planning and never saves such a run).
pub fn quiet_status(
    options: &WorkspaceStatusOptions,
    workspace: &Path,
    needed: bool,
) -> Result<Option<ComputedWorkspaceStatus>, String> {
    if !needed {
        return Ok(None);
    }
    let client = crate::action_env::client_environment();
    let (computed, stderr) = crate::workspace_status::compute_quiet(
        options,
        workspace,
        &client,
        crate::workspace_status::current_time_millis(&client),
    );
    if !stderr.is_empty() {
        return Err(format!(
            "the workspace status command wrote {} bytes to stderr",
            stderr.len()
        ));
    }
    Ok(Some(match computed {
        Ok(status) => ComputedWorkspaceStatus::Ok(status),
        Err(error) => ComputedWorkspaceStatus::Failed(error),
    }))
}

/// The prefix of a miss whose plan's `map_each` functions did not resolve.
const MAPPERS_MISS: &str = "the plan's map_each functions do not resolve: ";

/// The miss reason of a failed plan thread.
fn plan_miss(error: String) -> String {
    if error.starts_with(MAPPERS_MISS) {
        error
    } else {
        format!("unreadable plan: {error}")
    }
}

fn millis(duration: Duration) -> String {
    format!("{:.1}", duration.as_secs_f64() * 1000.0)
}

/// Replay every local repository rule against the pinned tables with this
/// command's options and environment; every record must reproduce its
/// identity, print nothing and still have its published tree. Returns the
/// number replayed and the replay's wall time.
pub(crate) fn replay_records(
    tables: Option<crate::snapshot::replay::PinnedTables>,
    inputs: &ReplayInputs,
    records: &[crate::snapshot::replay::ReplayRecord],
    cancel: &AtomicBool,
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
    let report = crate::snapshot::replay::replay_until(tables, &context, records, cancel);
    if cancel.load(Ordering::Relaxed) {
        return Err(crate::snapshot::replay::CANCELLED.to_owned());
    }
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
    Ok((records.len(), report.wall))
}

/// Threads checking the local repositories' published trees at most.
const LOCAL_TREE_THREADS: usize = 4;

/// Check every local repository's published tree against its execution
/// (L2-W5h; crate::repository_store::verify_local_publication: the manifest
/// beside it must record the execution's identity and tree digest, and the
/// tree verify against it — content, modes, links; trusted by stat outside
/// the racy window, every file read under `--verify_repo_tampering`) on up
/// to [`LOCAL_TREE_THREADS`] threads, beside the replay. A tree that differs
/// is a miss naming its first differing entry: the full analysis repairs it
/// (re-executes the rule and republishes the tree), so a tampered local
/// repository never reaches execution — before, a hit used it as it was,
/// and only a failure it caused healed. Stops at the first mismatch (or
/// when `cancel` is set) unless `explain`, which names every mismatch.
/// Returns the number of trees checked and the wall time.
pub(crate) fn check_local_trees(
    records: &[crate::snapshot::replay::ReplayRecord],
    cancel: &AtomicBool,
    explain: bool,
) -> Result<(usize, Duration), String> {
    let started = Instant::now();
    let total = records.len();
    let next = std::sync::atomic::AtomicUsize::new(0);
    let mismatches = std::sync::Mutex::new(Vec::new());
    let threads = std::thread::available_parallelism()
        .map(usize::from)
        .unwrap_or(1)
        .clamp(1, LOCAL_TREE_THREADS)
        .min(total);
    let work = || {
        loop {
            let index = next.fetch_add(1, Ordering::Relaxed);
            if index >= total || cancel.load(Ordering::Relaxed) {
                break;
            }
            let record = &records[index];
            if let Err(mismatch) =
                crate::repository_store::verify_local_publication(&record.root, &record.execution)
            {
                if !explain {
                    // Stop the others: no index is handed out past the end.
                    next.store(total, Ordering::Relaxed);
                }
                mismatches
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .push((
                        index,
                        format!(
                            "local repository {}: {LOCAL_TREE_DIFFERS}{} {}",
                            record.canonical,
                            mismatch.class,
                            crate::observed_fs::escape(mismatch.path.as_os_str())
                        ),
                    ));
            }
        }
    };
    if threads <= 1 {
        work();
    } else {
        std::thread::scope(|scope| {
            for _ in 0..threads {
                scope.spawn(work);
            }
        });
    }
    let mut mismatches = mismatches
        .into_inner()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    mismatches.sort();
    if let Some((_, first)) = mismatches.first() {
        let mut reason = first.clone();
        if mismatches.len() > 1 {
            reason.push_str(&format!(" (and {} more)", mismatches.len() - 1));
        }
        return Err(reason);
    }
    if cancel.load(Ordering::Relaxed) && !explain {
        return Err(crate::snapshot::replay::CANCELLED.to_owned());
    }
    Ok((total, started.elapsed()))
}

/// What a local repository's tree mismatch says before its first differing
/// entry (crate::snapshot::telemetry classifies it as `local-tree`).
pub const LOCAL_TREE_DIFFERS: &str = "its published tree differs from its manifest: ";

/// Rewrite the snapshot with the keys the validation settled (PLAN-L2 §1c:
/// "if any keys were refreshed, rewrite the file in the background"): the
/// INPUTS section re-encoded, the other sections copied from the file the
/// hit read — and renamed over it only while its path still names that
/// file (crate::snapshot::store::Pending::commit_replacing, L2-W5h; the
/// reader stays open until after the rename, L2-W5i).
fn refresh_in_background(
    reader: Reader,
    mut inputs: format::Inputs,
    cache: &Path,
    refresh: Vec<Refresh>,
    log: crate::snapshot::telemetry::Context,
) -> JoinHandle<()> {
    let cache = cache.to_path_buf();
    std::thread::spawn(move || {
        validate::apply_refresh(&mut inputs.recording, &refresh);
        let mut sections = vec![(
            Section::Inputs,
            format::encode_inputs(
                &inputs.recording,
                &inputs.replay,
                inputs.tables.as_ref(),
                inputs.workspace_status,
            ),
        )];
        for kind in [
            Section::Events,
            Section::Plan,
            Section::Stats,
            Section::Index,
        ] {
            match reader.stored_section(kind) {
                Ok(bytes) => sections.push((kind, bytes)),
                Err(_) if matches!(kind, Section::Stats | Section::Index) => {}
                Err(_) => return,
            }
        }
        let key = Key {
            g: reader.header.key,
            binary: reader.header.binary,
        };
        // Only over the file the hit read (L2-W5h): a snapshot healed, set
        // aside, evicted or replaced before the check stays so. The reader
        // keeps that file open until after the rename (L2-W5i): its inode
        // cannot be freed and reused by a newer file meanwhile, which would
        // pass the identity check.
        let _ = store::write_in_background(&cache, &key, sections)
            .logged(log)
            .commit_replacing(&reader);
        drop(reader);
    })
}

/// After a hit: finish the refresh, or heal a stale snapshot.
pub struct Session {
    cache: PathBuf,
    key: Key,
    refresh: Option<JoinHandle<()>>,
    /// Verify mode: the refresh waits for the full analysis to agree.
    deferred: Option<(Reader, Vec<Refresh>)>,
    /// What a failed command verifies before the snapshot may stay.
    trees: Trees,
    log: crate::snapshot::telemetry::Context,
}

/// The repository trees a hit used without looking into their files
/// (L2-W5g), verified when its command fails ([`heal`]): the snapshot's
/// verified store trees — each once (L2-W5h): an owner's as its store's
/// warm check does, one only a link crossed into as the crossing did
/// (content only) — and the published trees of its local repositories,
/// with the execution each must still hold. Under the full policy
/// (`--verify_repo_tampering`) the store trees are left out: the lookup
/// checked every file's INTEGRITY entry already (L2-W5h).
#[derive(Debug, Default)]
struct Trees {
    /// Trees an owner recorded (a reused publication).
    store: Vec<crate::repository_store::PublishedTree>,
    /// Trees only a link crossed into (crate::source_links: verified
    /// against their manifest's content on the crossing).
    crossed: Vec<crate::repository_store::PublishedTree>,
    local: Vec<(String, PathBuf, crate::repository_store::LocalExecution)>,
}

impl Trees {
    fn of(inputs: &format::Inputs) -> Self {
        let full =
            crate::snapshot::validate::Policy::current() == crate::snapshot::validate::Policy::Full;
        let (crossed, store): (Vec<_>, Vec<_>) = inputs
            .recording
            .repositories
            .iter()
            .filter(|_| !full)
            .filter_map(|tree| {
                Some(crate::repository_store::PublishedTree {
                    kind: tree.kind,
                    id: tree.root.file_name()?.to_str()?.to_owned(),
                    root: tree.root.clone(),
                    manifest: tree.manifest.clone()?,
                })
            })
            .partition(|tree| inputs.recording.crossed.contains(&tree.root));
        Self {
            store,
            crossed,
            local: inputs
                .replay
                .iter()
                .map(|record| {
                    (
                        record.canonical.clone(),
                        record.root.clone(),
                        record.execution.clone(),
                    )
                })
                .collect(),
        }
    }

    /// A tree that no longer verifies, if any, and its first differing
    /// entry. The trees are verified on up to eight threads; the first
    /// mismatch found stops the others (which one is reported when several
    /// differ depends on the timing).
    fn first_mismatch(&self) -> Option<crate::repository_store::TreeMismatch> {
        let (store, crossed) = (self.store.len(), self.crossed.len());
        let check = |index: usize| -> Option<crate::repository_store::TreeMismatch> {
            if index < store {
                crate::repository_store::verify_as_reused(&self.store[index]).err()
            } else if index < store + crossed {
                crate::repository_store::check_published_tree(&self.crossed[index - store]).err()
            } else {
                self.check_local(index - store - crossed).err()
            }
        };
        let total = self.len();
        let threads = std::thread::available_parallelism()
            .map(usize::from)
            .unwrap_or(1)
            .clamp(1, 8)
            .min(total);
        if threads <= 1 {
            return (0..total).find_map(check);
        }
        let next = std::sync::atomic::AtomicUsize::new(0);
        let found = std::sync::Mutex::new(None);
        std::thread::scope(|scope| {
            for _ in 0..threads {
                scope.spawn(|| {
                    loop {
                        let index = next.fetch_add(1, Ordering::Relaxed);
                        if index >= total {
                            break;
                        }
                        if let Some(mismatch) = check(index) {
                            // Stop the others: no index is handed out past
                            // the end.
                            next.store(total, Ordering::Relaxed);
                            found
                                .lock()
                                .unwrap_or_else(std::sync::PoisonError::into_inner)
                                .get_or_insert(mismatch);
                            break;
                        }
                    }
                });
            }
        });
        found
            .into_inner()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// How many trees [`Trees::first_mismatch`] verifies at most.
    fn len(&self) -> usize {
        self.store.len() + self.crossed.len() + self.local.len()
    }

    /// The local repository `index`'s published tree against its execution.
    fn check_local(&self, index: usize) -> Result<(), crate::repository_store::TreeMismatch> {
        let (canonical, root, execution) = &self.local[index];
        crate::repository_store::verify_local_publication(root, execution).map_err(
            |mut mismatch| {
                mismatch.message = format!("local repository {canonical}: {}", mismatch.message);
                mismatch
            },
        )
    }

    /// The tree containing `path` — one of `spellings` of a path a failed
    /// command's error named (L2-W5h) — checked as [`Trees::first_mismatch`]
    /// checks it (a published tree of the store the snapshot did not record
    /// as its store's warm check does), or `None` when the path lies in no
    /// tree.
    fn check_named(
        &self,
        store: &Path,
        spellings: &[PathBuf],
    ) -> Option<Result<(), crate::repository_store::TreeMismatch>> {
        let within = |root: &Path| spellings.iter().any(|path| path.starts_with(root));
        if let Some(index) = self.local.iter().position(|(_, root, _)| within(root)) {
            return Some(self.check_local(index));
        }
        if let Some(tree) = self.crossed.iter().find(|tree| within(&tree.root)) {
            return Some(crate::repository_store::check_published_tree(tree));
        }
        if let Some(tree) = self.store.iter().find(|tree| within(&tree.root)) {
            return Some(crate::repository_store::verify_as_reused(tree));
        }
        let tree = spellings
            .iter()
            .find_map(|path| crate::repository_store::store_tree_of(store, path))?;
        Some(crate::repository_store::verify_as_reused(&tree))
    }
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
            self.refresh = Some(refresh_in_background(
                reader,
                inputs,
                &self.cache,
                refresh,
                self.log.clone(),
            ));
        }
    }

    /// The event log's context of this command (crate::snapshot::telemetry).
    pub fn log(&self) -> &crate::snapshot::telemetry::Context {
        &self.log
    }

    /// The build part of the command succeeded: let the background refresh
    /// complete. What runs after it (`test`'s tests, `run`'s preparation)
    /// may still fail on a damaged repository tree: such a failure heals the
    /// snapshot through [`heal_after_failure`] (main).
    pub fn finish(mut self) {
        if let Some(refresh) = self.refresh.take() {
            let _ = refresh.join();
        }
        let Self {
            cache,
            key,
            trees,
            log,
            ..
        } = self;
        *FINISHED
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(Finished {
            cache,
            key,
            trees,
            log,
        });
    }

    /// Execution failed with `error`: [`heal`] the snapshot.
    pub fn failed(mut self, error: &str) {
        if let Some(refresh) = self.refresh.take() {
            let _ = refresh.join();
        }
        heal(&self.cache, &self.key, &self.trees, &self.log, Some(error));
    }
}

/// A hit whose build part succeeded ([`Session::finish`]).
struct Finished {
    cache: PathBuf,
    key: Key,
    trees: Trees,
    log: crate::snapshot::telemetry::Context,
}

static FINISHED: std::sync::Mutex<Option<Finished>> = std::sync::Mutex::new(None);

/// main, when the command failed after a hit's build part succeeded — with
/// `error` (`test`'s test runner staging a runfile, `run`'s preparation) or
/// without one (`test`'s tests failed, L2-W5g): [`heal`] its snapshot.
pub fn heal_after_failure(error: Option<&str>) {
    let finished = FINISHED
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .take();
    if let Some(finished) = finished {
        heal(
            &finished.cache,
            &finished.key,
            &finished.trees,
            &finished.log,
            error,
        );
    }
}

/// A hit's command failed (with `error`, when there is one: a failing test
/// has none). The snapshot may have trusted a damaged tree — the per-tree
/// validation does not look into a store tree's files, and execution may
/// have changed a local repository's tree checked before it; the full
/// analysis repairs both — so it is removed, with one line naming the
/// first differing entry, when
///
/// * the tree containing a repository-store path the error names (quoted
///   or not, missing or not; L2-W5f review F4) no longer verifies — L2-W5h:
///   that tree is checked first, no longer removed outright, so a failure
///   that merely mentions an intact store path keeps the snapshot; or
/// * any tree of the snapshot no longer verifies ([`Trees::first_mismatch`],
///   L2-W5g): a byte or mode edit whose failure names no store path (an
///   action citing a tampered tool by its execution-root path, a test
///   failing on a tampered runfile) — before, every hit failed the same way.
///
/// The next run's full analysis repairs the tree. A failure with intact
/// trees keeps the snapshot (the check costs a walk of the trees, trusted
/// by stat). Returns whether the snapshot was removed.
fn heal(
    cache: &Path,
    key: &Key,
    trees: &Trees,
    log: &crate::snapshot::telemetry::Context,
    error: Option<&str>,
) -> bool {
    let started = Instant::now();
    let store = cache.join("repositories");
    let canonical_store = std::fs::canonicalize(&store).ok();
    let named = error.and_then(|error| {
        store_path_in(error, &store).or_else(|| {
            canonical_store
                .as_ref()
                .and_then(|canonical| store_path_in(error, canonical))
        })
    });
    // The tree the error names first (L2-W5h: verified, no longer removed
    // outright — a failure that merely mentions an intact store path keeps
    // the snapshot), then every tree of the snapshot.
    let named_mismatch = named.as_ref().and_then(|path| {
        let path = PathBuf::from(path);
        let mut spellings = vec![path.clone()];
        if let Some(canonical) = &canonical_store
            && let Ok(rest) = path.strip_prefix(&store)
        {
            spellings.push(canonical.join(rest));
        }
        trees
            .check_named(canonical_store.as_deref().unwrap_or(&store), &spellings)
            .and_then(Result::err)
    });
    let (rule, mismatch) = match named_mismatch {
        Some(mismatch) => ("store-path", mismatch),
        None => match trees.first_mismatch() {
            Some(mismatch) => ("tree", mismatch),
            None => {
                timing_line(trees, started, false);
                // L2-W5i: logged too, so the soak tells a failure the
                // snapshot had no part in (a compile error, a failing test)
                // from a heal, and sees what the check cost.
                log.log_heal_kept(
                    &key.short(),
                    trees.len(),
                    started.elapsed().as_secs_f64() * 1000.0,
                    named.as_deref(),
                );
                return false;
            }
        },
    };
    timing_line(trees, started, true);
    let ms = started.elapsed().as_secs_f64() * 1000.0;
    if store::remove(cache, key).is_err() {
        return false;
    }
    log.log_healed(
        &key.short(),
        rule,
        mismatch.class,
        &mismatch.path,
        ms,
        &mismatch.message,
    );
    match (rule, named) {
        ("store-path", Some(path)) => crate::note!(
            "bazel-rs: removed the analysis snapshot of this build: its failure involves {path} in the repository store, whose tree {} no longer matches what the analysis recorded ({}); the next build re-analyzes and repairs it",
            mismatch.root.display(),
            mismatch.entry_text()
        ),
        _ => crate::note!(
            "bazel-rs: removed the analysis snapshot of this build: it failed, and the repository tree {} no longer matches what the analysis recorded ({}); the next build re-analyzes and repairs it",
            mismatch.root.display(),
            mismatch.entry_text()
        ),
    }
    true
}

/// `BAZEL_RS_TIMING=1`: what a failed hit's healing cost.
fn timing_line(trees: &Trees, started: Instant, healed: bool) {
    if crate::diagnostic_switch(std::env::var_os("BAZEL_RS_TIMING")) {
        crate::snapshot::diag::line_fmt(format_args!(
            "PERF-SNAPSHOT heal trees={} healed={healed} ms={:.3}",
            trees.len(),
            started.elapsed().as_secs_f64() * 1000.0
        ));
    }
}

/// The first path below `store` that `text` names (up to the next
/// whitespace, quote or `:`), if any.
fn store_path_in(text: &str, store: &Path) -> Option<String> {
    let prefix = format!("{}/", store.display());
    let at = text.find(&prefix)?;
    let rest = &text[at..];
    let end = rest
        .find(|c: char| c.is_whitespace() || matches!(c, '\'' | '"' | ':' | ',' | '`' | ')'))
        .unwrap_or(rest.len());
    Some(rest[..end].to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn store_paths_are_found_quoted_or_not() {
        let store = Path::new("/c/repositories");
        for (error, path) in [
            (
                "action 'x' failed: cannot inspect '/c/repositories/trees/ab/f.go': No such file or directory (os error 2); see '/tmp/y'",
                Some("/c/repositories/trees/ab/f.go"),
            ),
            (
                "cannot stage /c/repositories/direct/trees/12/value: Permission denied",
                Some("/c/repositories/direct/trees/12/value"),
            ),
            ("in \"/c/repositories/x y\"", Some("/c/repositories/x")),
            ("/c/repositoriesX/a and /c/other", None),
            ("nothing here", None),
        ] {
            assert_eq!(store_path_in(error, store).as_deref(), path, "{error}");
        }
    }

    /// A published direct tree in `store` holding `tool.sh` (0755) and
    /// `data.txt`, under `key`.
    fn published(store: &Path, key: &str) -> PathBuf {
        use std::os::unix::fs::PermissionsExt;
        crate::repository_store::materialize(
            store,
            key,
            key.as_bytes(),
            b"provenance",
            |_, _| Ok(true),
            |stage: &Path| {
                let tool = stage.join("tool.sh");
                std::fs::write(&tool, "#!/bin/sh\n").map_err(|e| e.to_string())?;
                std::fs::set_permissions(&tool, std::fs::Permissions::from_mode(0o755))
                    .map_err(|e| e.to_string())?;
                std::fs::write(stage.join("data.txt"), "good\n").map_err(|e| e.to_string())?;
                Ok(crate::repository_store::Executed {
                    inputs: key.as_bytes().to_vec(),
                    recorded: br#"{"version":1}"#.to_vec(),
                })
            },
        )
        .unwrap()
        .0
    }

    /// L2-W5f review F4, narrowed by L2-W5h: a failed hit whose error names
    /// a path in the repository store — quoted or not, missing or not, as
    /// given or canonical — checks the tree containing it first and removes
    /// the snapshot when that tree no longer verifies (also after the build
    /// part, `heal_after_failure`); a named path in an intact tree, or in no
    /// tree, keeps it, like any other failure.
    #[test]
    fn a_failed_hit_naming_the_store_verifies_the_named_tree() {
        let cache = tempfile::tempdir().unwrap();
        let store_root = cache.path().join("repositories");
        let key = Key {
            g: [3; 32],
            binary: [4; 32],
        };
        let save = || {
            store::write_in_background(cache.path(), &key, vec![(Section::Inputs, Vec::new())])
                .commit()
                .unwrap();
        };
        let root = published(&store_root, "+gen_rule+named");
        save();
        let session = |cache: &Path| Session {
            cache: cache.to_path_buf(),
            key,
            refresh: None,
            deferred: None,
            trees: Trees::default(),
            log: crate::snapshot::telemetry::Context::default(),
        };
        let kept = || store::path(cache.path(), &key).exists();
        // A failure outside the store, a named path in an intact tree or in
        // no tree: kept.
        session(cache.path()).failed("cannot inspect '/surely/missing/w5': x");
        session(cache.path()).failed("compile error in //pkg:x");
        session(cache.path()).failed(&format!(
            "cannot stage {}: x",
            root.join("data.txt").display()
        ));
        session(cache.path()).failed(&format!(
            "cannot read '{}': x",
            store_root.join("direct/specs/x").display()
        ));
        assert!(kept());
        // A file of the named tree deleted: removed.
        std::fs::remove_file(root.join("data.txt")).unwrap();
        session(cache.path()).failed(&format!(
            "cannot inspect '{}': No such file or directory (os error 2)",
            root.join("data.txt").display()
        ));
        assert!(!kept());
        // Through the canonical store path (the cache reached by a link).
        save();
        let link = tempfile::tempdir().unwrap();
        let linked = link.path().join("cache");
        std::os::unix::fs::symlink(cache.path(), &linked).unwrap();
        let canonical = std::fs::canonicalize(&root).unwrap();
        session(&linked).failed(&format!(
            "cannot inspect '{}': x",
            canonical.join("data.txt").display()
        ));
        assert!(!kept());
        // After the build part succeeded (a test runner's error).
        save();
        session(cache.path()).finish();
        heal_after_failure(Some("a test failed"));
        assert!(kept());
        session(cache.path()).finish();
        heal_after_failure(Some(&format!(
            "cannot inspect '{}': No such file or directory",
            root.join("data.txt").display()
        )));
        assert!(!kept());
    }

    /// L2-W5h: a tree only a link crossed into is checked once, as the
    /// crossing checked it — content, no modes — while an owner's tree is
    /// checked as its store's warm check does (modes too).
    #[test]
    fn crossed_trees_are_checked_for_their_content_only() {
        use std::os::unix::fs::PermissionsExt;
        let cache = tempfile::tempdir().unwrap();
        let store_root = cache.path().join("repositories");
        let root = published(&store_root, "+gen_rule+crossed");
        let tree = crate::repository_store::published_tree(&store_root, &root).unwrap();
        let crossed = Trees {
            crossed: vec![tree.clone()],
            ..Trees::default()
        };
        let owned = Trees {
            store: vec![tree],
            ..Trees::default()
        };
        assert!(crossed.first_mismatch().is_none() && owned.first_mismatch().is_none());
        std::fs::set_permissions(root.join("tool.sh"), std::fs::Permissions::from_mode(0o644))
            .unwrap();
        assert!(crossed.first_mismatch().is_none());
        assert_eq!(
            owned.first_mismatch().map(|mismatch| mismatch.class),
            Some("mode")
        );
        std::fs::write(root.join("data.txt"), "bad!\n").unwrap();
        let mismatch = crossed.first_mismatch().unwrap();
        assert_eq!(
            (mismatch.class, mismatch.path),
            ("content", root.join("data.txt"))
        );
        assert_eq!(crossed.len(), 1);
    }

    /// L2-W5g: a failed hit verifies the trees it used without looking into
    /// them — a store tree as its store's warm check does, a local
    /// repository's published tree against its execution — and removes the
    /// snapshot when a byte or mode edit made one differ (with or without
    /// an error text); intact trees keep it.
    #[test]
    fn a_failed_hit_with_a_tampered_tree_removes_the_snapshot() {
        use std::os::unix::fs::PermissionsExt;
        let cache = tempfile::tempdir().unwrap();
        let store_root = cache.path().join("repositories");
        let key = Key {
            g: [5; 32],
            binary: [6; 32],
        };
        let save = || {
            store::write_in_background(cache.path(), &key, vec![(Section::Inputs, Vec::new())])
                .commit()
                .unwrap();
        };
        let execute = |stage: &Path| {
            let tool = stage.join("tool.sh");
            std::fs::write(&tool, "#!/bin/sh\n").map_err(|e| e.to_string())?;
            std::fs::set_permissions(&tool, std::fs::Permissions::from_mode(0o755))
                .map_err(|e| e.to_string())?;
            std::fs::write(stage.join("data.txt"), "good\n").map_err(|e| e.to_string())?;
            Ok(crate::repository_store::Executed {
                inputs: b"runtime inputs".to_vec(),
                recorded: br#"{"version":1}"#.to_vec(),
            })
        };
        let (local_root, execution) = crate::repository_store::materialize_local(
            &store_root,
            "+local_rule+tools",
            "local-spec",
            || Ok(b"local provenance".to_vec()),
            execute,
        )
        .unwrap();
        let (direct_root, _) = crate::repository_store::materialize(
            &store_root,
            "+gen_rule+tools",
            b"spec",
            b"provenance",
            |_, _| Ok(true),
            execute,
        )
        .unwrap();
        let direct = crate::repository_store::published_tree(&store_root, &direct_root).unwrap();
        let trees = Trees {
            store: vec![direct],
            crossed: Vec::new(),
            local: vec![(
                "+local_rule+tools".to_owned(),
                local_root.clone(),
                execution,
            )],
        };
        let log = crate::snapshot::telemetry::Context {
            cache: cache.path().to_path_buf(),
            mode: crate::snapshot::telemetry::MODE_ON,
            command: "test".to_owned(),
            targets: vec!["//:t".to_owned()],
        };
        let heals = |error: Option<&str>| heal(cache.path(), &key, &trees, &log, error);
        let kept = || store::path(cache.path(), &key).exists();
        let set_mode = |path: &Path, mode: u32| {
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).unwrap();
        };
        // An in-place edit of the same size with its modification time
        // restored (its change time moves).
        let edit = |file: &Path| {
            let metadata = std::fs::metadata(file).unwrap();
            let mode = metadata.permissions().mode();
            set_mode(file, 0o644);
            std::fs::write(file, "bad!\n").unwrap();
            let handle = std::fs::File::options().write(true).open(file).unwrap();
            handle.set_modified(metadata.modified().unwrap()).unwrap();
            drop(handle);
            set_mode(file, mode);
        };
        save();
        // Intact trees: kept, whatever failed.
        assert!(!heals(Some("action '//:t' failed: exit 1")));
        assert!(!heals(None));
        assert!(kept());
        // The local repository's tool made non-executable: removed.
        set_mode(&local_root.join("tool.sh"), 0o644);
        assert!(heals(Some(
            "external/+local_rule+tools/tool.sh: Permission denied"
        )));
        assert!(!kept());
        set_mode(&local_root.join("tool.sh"), 0o755);
        save();
        assert!(!heals(None));
        // The store tree's runfile edited in place: removed (a failing test
        // has no error text).
        edit(&direct_root.join("data.txt"));
        assert!(heals(None));
        assert!(!kept());
        let log_text = std::fs::read_to_string(cache.path().join("snapshots/events.log")).unwrap();
        // L2-W5i: each failure that kept the snapshot is logged too (the
        // three with intact trees above), naming the trees it checked.
        let kept: Vec<&str> = log_text
            .lines()
            .filter(|line| line.contains(" kind=heal-kept "))
            .collect();
        assert_eq!(kept.len(), 3, "{log_text}");
        assert!(
            kept.iter()
                .all(|line| line.contains(" reason=- path=- ") && line.contains(" detail=trees:2 ")),
            "{log_text}"
        );
        let lines: Vec<&str> = log_text
            .lines()
            .filter(|line| line.contains(" kind=healed "))
            .collect();
        assert_eq!(lines.len(), 2, "{log_text}");
        // L2-W5h: each names the first differing entry.
        assert!(
            lines[0].contains(" kind=healed mode=on ")
                && lines[0].contains(" reason=tree:mode ")
                && lines[0].contains(&format!(" path={} ", local_root.join("tool.sh").display())),
            "{log_text}"
        );
        assert!(
            lines[1].contains(" reason=tree:content ")
                && lines[1].contains(&format!(
                    " path={} ",
                    direct_root.join("data.txt").display()
                ))
                && lines[1].ends_with(" targets=//:t"),
            "{log_text}"
        );
    }
}
