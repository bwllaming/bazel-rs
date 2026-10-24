//! `BAZEL_RS_SNAPSHOT=record` (L2-W3, PLAN-L2 §1b/§7): open the
//! crate::observed_fs recording window at the snapshot check point (main,
//! after the bootstrap values that form the snapshot key and before the
//! registry session opens), close it at the end of planning (the
//! `plan.end` mark, crate::perf_mark; explicitly for `query`), then report:
//!
//! * `bazel-rs-snapshot:` diagnostics lines on stderr — observation counts by
//!   op, distinct paths, environment reads, verified repositories and their
//!   integrity entries, every taint with its first reasons, whether the run
//!   would be eligible to save, the measured time of the validation a hit
//!   would run under the command's policy (crate::snapshot::validate: per
//!   tree unless `--verify_repo_tampering`), and the replay dry run of every
//!   local repository rule the window executed (crate::snapshot::replay; a
//!   mismatch taints the run);
//! * with `BAZEL_RS_SNAPSHOT_DUMP=<file>`, a text dump of every observation,
//!   the replay's pinned tables and records, plus the same summary
//!   (scripts/snapshot_fs_audit.py reads it).
//!
//! Nothing else changes: stdout, the rest of stderr and every output are
//! byte-identical with recording on and off. The diagnostics appear only
//! in the record mode (or under `BAZEL_RS_SNAPSHOT_EXPLAIN`).

use crate::observed_fs::{self, Op, Outcome, Recording, Taint, escape};
use crate::registry_session::SessionHandle;
use std::collections::BTreeMap;
use std::ffi::OsString;
use std::fmt::Write as _;
use std::io::Write as _;
use std::path::PathBuf;

/// The mode variable (crate::snapshot::Mode): `record` here.
pub const MODE_VAR: &str = crate::snapshot::MODE_VAR;
/// Where the text dump goes (nothing is written without it).
pub const DUMP_VAR: &str = "BAZEL_RS_SNAPSHOT_DUMP";

/// Test and debug hooks (besides every `BAZEL_RS_DEBUG_*`): a run with one
/// of them set is never saved (PLAN-L2 §1a, taint TestHook). Loading-order
/// jitter and the no-demand switch, the analysis self-checks of the
/// memoized dependency views and of the artifact index, the persistent
/// digest table's switch and the retained-memory breakdown
/// (integration-20; tests/observed_fs_discipline.rs lists them as
/// diagnostic environment).
pub const TEST_HOOKS: &[&str] = &[
    "BAZEL_RS_LOADING_JITTER",
    "BAZEL_RS_LOADING_NO_DEMAND",
    "BAZEL_RS_VERIFY_VIEWS",
    "BAZEL_RS_VERIFY_ARTIFACTS",
    "BAZEL_RS_DIGEST_MEMO",
    "BAZEL_RS_TIMING_ALLOC",
];

/// Whether this process runs the record mode (`BAZEL_RS_SNAPSHOT=record`
/// opened the window; open or closed). `BAZEL_RS_SNAPSHOT=on` records
/// too (crate::snapshot::save) but has no report.
pub fn active() -> bool {
    RECORD_MODE.load(std::sync::atomic::Ordering::Relaxed)
}

static RECORD_MODE: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// The planned request's encoding as the record mode measured it
/// (crate::snapshot::codec::hand_over): its sizes and times, or why it
/// cannot be encoded.
static PLAN: std::sync::Mutex<Option<Result<crate::snapshot::codec::Stats, String>>> =
    std::sync::Mutex::new(None);

/// Keep the planned request's encoding measurement for the report (record
/// mode only).
pub fn note_plan(stats: Result<crate::snapshot::codec::Stats, String>) {
    if active() {
        *PLAN
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(stats);
    }
}

fn noted_plan() -> Option<Result<crate::snapshot::codec::Stats, String>> {
    PLAN.lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone()
}

/// One command's recording; reports when finished or dropped.
pub struct Session {
    command: String,
    dump: Option<PathBuf>,
    registry: Option<SessionHandle>,
    /// The command's repository environment (the validation dry run's
    /// ENV checks, crate::snapshot::validate::Environment).
    environment: Vec<(OsString, OsString)>,
    /// The event log's context: the report appends a `kind=recorded` line
    /// (crate::snapshot::telemetry, L2-W5h).
    log: crate::snapshot::telemetry::Context,
    reported: bool,
}

/// Start recording when `BAZEL_RS_SNAPSHOT=record`. `environment` is the
/// command's repository environment (the client environment with
/// `--repo_env` applied, crate::repo_env), which the validation dry run
/// checks the ENV records against; `log` the event log's context (its
/// cache, command and target patterns).
pub fn start(
    command: &str,
    environment: &[(OsString, OsString)],
    log: crate::snapshot::telemetry::Context,
) -> Option<Session> {
    // Other values: crate::snapshot::Mode::from_env reports them.
    if std::env::var_os(MODE_VAR)? != "record" {
        return None;
    }
    if !observed_fs::open_window() {
        return None;
    }
    RECORD_MODE.store(true, std::sync::atomic::Ordering::Relaxed);
    // Local repository rules executed in the window become replay records
    // (crate::snapshot::replay), replayed as a dry run in the report.
    crate::snapshot::replay::install();
    // PLAN-L2 §1a: runs with a test or debug hook are never saved.
    for (name, _) in std::env::vars_os() {
        let name = name.to_string_lossy();
        if TEST_HOOKS.contains(&name.as_ref()) || name.starts_with("BAZEL_RS_DEBUG_") {
            observed_fs::taint(Taint::TestHook, || name.into_owned());
        }
    }
    Some(Session {
        command: command.to_owned(),
        dump: std::env::var_os(DUMP_VAR).map(PathBuf::from),
        registry: None,
        environment: environment.to_vec(),
        log,
        reported: false,
    })
}

impl Session {
    /// The registry session whose pending lockfile writes taint the run.
    pub fn set_registry_session(&mut self, registry: SessionHandle) {
        self.registry = Some(registry);
    }

    /// The command succeeded: report now.
    pub fn finish(mut self) {
        self.report(true);
    }

    fn report(&mut self, success: bool) {
        if self.reported {
            return;
        }
        self.reported = true;
        let planned = !observed_fs::recording();
        observed_fs::close_window();
        if !planned {
            observed_fs::taint_after_window(Taint::Errors, || {
                "planning did not complete (the window was still open at exit)".to_owned()
            });
        }
        if !success {
            observed_fs::taint_after_window(Taint::Errors, || "the command failed".to_owned());
        }
        if let Some(session) = self.registry.as_ref().and_then(SessionHandle::session) {
            if session.updated_lockfile().is_some() {
                observed_fs::taint_after_window(Taint::LockfileWritePending, || {
                    "MODULE.bazel.lock (workspace or --native-lockfile target)".to_owned()
                });
            }
            if let Some((path, _)) = session.updated_hidden_lockfile() {
                observed_fs::taint_after_window(Taint::LockfileWritePending, || {
                    format!("hidden lockfile {}", escape(path.as_os_str()))
                });
            }
        }
        let Some(recorder) = observed_fs::recorder() else {
            return;
        };
        // Measurement knobs of the record mode only: the validation's thread
        // count (default: one below 4k checks, else min(available
        // parallelism, 16)), how many times to repeat the validation and
        // replay dry runs (the first one pays cold caches), and whether to
        // also time the other validation policy.
        let knob = |name: &str| {
            std::env::var(name)
                .ok()
                .and_then(|value| value.parse::<usize>().ok())
                .filter(|value| *value > 0)
        };
        let repeats = knob("BAZEL_RS_SNAPSHOT_DRY_RUNS").unwrap_or(1).min(20);
        // Replay every local repository rule of the window (L2-W4R): a
        // mismatch taints the run.
        let capture = crate::snapshot::replay::global();
        let records = capture
            .as_ref()
            .map(|capture| capture.records())
            .unwrap_or_default();
        let replays: Vec<crate::snapshot::replay::Report> = capture
            .as_ref()
            .map(|capture| {
                (0..repeats)
                    .filter_map(|_| crate::snapshot::replay::dry_run(capture))
                    .collect()
            })
            .unwrap_or_default();
        for replay in &replays {
            for replayed in &replay.replayed {
                if let Some(reason) = &replayed.mismatch {
                    observed_fs::taint_after_window(Taint::ReplayMismatch, || {
                        format!("{}: {reason}", replayed.canonical)
                    });
                }
            }
        }
        let recording = recorder.snapshot();
        let policy = crate::snapshot::validate::Policy::current();
        let validated = policy.select(&recording);
        let threads = knob("BAZEL_RS_SNAPSHOT_THREADS")
            .unwrap_or_else(|| crate::snapshot::validate::threads_for(&validated));
        let environment = crate::snapshot::validate::Environment::new(&self.environment);
        let validations: Vec<_> = (0..repeats)
            .map(|_| crate::snapshot::validate::dry_run(&validated, threads, &environment))
            .collect();
        let mut summary = summary(&self.command, &recording, policy, &validations);
        self.log.log_recorded(
            eligibility(&recording),
            &crate::snapshot::save::taints(&recording.taints),
        );
        summary.push(plan_line(noted_plan()));
        if std::env::var_os("BAZEL_RS_SNAPSHOT_COMPARE").is_some() {
            // The other policy's cost, for comparison.
            let other = match policy {
                crate::snapshot::validate::Policy::PerTree => {
                    crate::snapshot::validate::Policy::Full
                }
                crate::snapshot::validate::Policy::Full => {
                    crate::snapshot::validate::Policy::PerTree
                }
            };
            let selected = other.select(&recording);
            let threads = crate::snapshot::validate::threads_for(&selected);
            let report = crate::snapshot::validate::dry_run(&selected, threads, &environment);
            summary.push(validation_line(other, &report));
        }
        summary.extend(replay_summary(&records, &replays));
        for line in &summary {
            observed_fs::diag(line);
        }
        let tables = capture.as_ref().and_then(|capture| capture.tables());
        if let Some(path) = &self.dump
            && let Err(error) = write_dump(
                path,
                &recording,
                &summary,
                &records,
                tables.as_ref(),
                replays.first(),
                Some(&canonical_inputs_digest(
                    &recording,
                    policy,
                    &records,
                    tables.as_ref(),
                )),
            )
        {
            observed_fs::diag(&format!(
                "cannot write {DUMP_VAR}={}: {error}",
                escape(path.as_os_str())
            ));
        }
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        // Reached without finish(): the command returned an error.
        self.report(false);
    }
}

fn millis(duration: std::time::Duration) -> String {
    format!("{:.3}", duration.as_secs_f64() * 1000.0)
}

/// One validation dry run's report line.
fn validation_line(
    policy: crate::snapshot::validate::Policy,
    validation: &crate::snapshot::validate::Report,
) -> String {
    format!(
        "validation dry-run [{}]: {} ms on {} thread(s) (setup {} ms, checks {} ms, env {} ms): {} observations ({} trusted by stat, {} content checks, {} refreshable; {} ms thread time), {} env, {} integrity entries ({} ms thread time); {} mismatch(es), {} repositories to re-verify",
        policy.name(),
        millis(validation.elapsed),
        validation.threads,
        millis(validation.setup_time),
        millis(validation.checks_time),
        millis(validation.env_time),
        validation.observations,
        validation.trusted_by_stat,
        validation.content_checks,
        validation.refreshable,
        millis(validation.observation_time),
        validation.env,
        validation.integrity,
        millis(validation.integrity_time),
        validation.mismatches,
        validation.repositories_to_verify,
    )
}

/// The replay dry runs' report lines (crate::snapshot::replay).
fn replay_summary(
    records: &[crate::snapshot::replay::ReplayRecord],
    replays: &[crate::snapshot::replay::Report],
) -> Vec<String> {
    let mut lines = Vec::new();
    if records.is_empty() {
        lines.push("replay: no local repository rule executed".to_owned());
        return lines;
    }
    let recorded: std::time::Duration = records.iter().map(|record| record.elapsed).sum();
    let printed = records
        .iter()
        .filter(|record| !record.output.is_empty())
        .count();
    let impure: Vec<String> = records
        .iter()
        .filter(|record| record.flags() != "-")
        .map(|record| format!("{} [{}]", record.canonical, record.flags()))
        .collect();
    lines.push(format!(
        "replay records: {} local repository rule(s), {} ms recorded execution time; {} printed; impure: {}",
        records.len(),
        millis(recorded),
        printed,
        if impure.is_empty() {
            "none".to_owned()
        } else {
            impure.join(", ")
        },
    ));
    for replay in replays {
        let optional =
            |value: Option<std::time::Duration>| value.map_or_else(|| "?".to_owned(), millis);
        let slowest = replay
            .definitions
            .iter()
            .map(|definition| definition.wall)
            .max()
            .unwrap_or_default();
        lines.push(format!(
            "replay dry-run: {}/{} matching over {} definition(s); wall {} ms (slowest definition load {} ms), CPU {} ms on replay threads + {} ms in child processes; pinned {} roots, {} mappings",
            replay.matched(),
            replay.replayed.len(),
            replay.definitions.len(),
            millis(replay.wall),
            millis(slowest),
            optional(replay.cpu),
            optional(replay.children_cpu),
            replay.pinned_roots,
            replay.pinned_mappings,
        ));
    }
    if let Some(replay) = replays.first() {
        let mut slow: Vec<&crate::snapshot::replay::Replayed> = replay.replayed.iter().collect();
        slow.sort_by_key(|replayed| std::cmp::Reverse(replayed.wall));
        lines.push(format!(
            "replay slowest: {}",
            slow.iter()
                .take(5)
                .map(|replayed| format!(
                    "{} {} ms (recorded {} ms)",
                    replayed.canonical,
                    millis(replayed.wall),
                    millis(replayed.recorded)
                ))
                .collect::<Vec<_>>()
                .join(" | ")
        ));
        for definition in &replay.definitions {
            if let Some(error) = &definition.error {
                lines.push(format!("replay definition {}: {error}", definition.source));
            }
        }
        const SHOWN: usize = 12;
        for replayed in replay
            .replayed
            .iter()
            .filter(|replayed| replayed.mismatch.is_some())
            .take(SHOWN)
        {
            lines.push(format!(
                "replay mismatch {}: {}",
                replayed.canonical,
                replayed.mismatch.as_deref().unwrap_or_default()
            ));
        }
        let differing = replay
            .replayed
            .iter()
            .filter(|replayed| replayed.output_differs)
            .count();
        if differing > 0 {
            lines.push(format!(
                "replay: {differing} replay(s) printed other output than recorded"
            ));
        }
    }
    lines
}

/// The planned request's encoding line: what a snapshot's PLAN section
/// would hold (L2-W4).
fn plan_line(plan: Option<Result<crate::snapshot::codec::Stats, String>>) -> String {
    match plan {
        None => "plan: no planned request (planning did not complete)".to_owned(),
        Some(Err(reason)) => format!("plan: cannot be encoded: {reason}"),
        Some(Ok(stats)) => format!(
            "plan: {} actions, encoded {} bytes ({} outline, {} shared input group(s) in {}, {} action chunk(s)); encode {} ms, decode {} ms",
            stats.nodes,
            stats.bytes,
            stats.meta_bytes,
            stats.groups,
            stats.group_bytes,
            stats.chunks,
            millis(stats.encode),
            stats.decode.map_or_else(|| "-".to_owned(), millis),
        ),
    }
}

/// Whether the recorded run would be eligible to save: `yes`, `no` or
/// `unless-pruned` — only taints recorded when analysis registered an
/// action the plan may still prune (Taint::plan_dependent), and the planned
/// request never decided them (planning did not complete). Once decided
/// (L2-W4, observed_fs::settle_plan_taints) only the plan's own remain.
fn eligibility(recording: &Recording) -> &'static str {
    if recording.taints.is_empty() {
        "yes"
    } else if recording.plan_registered.is_none()
        && recording.taints.keys().all(|taint| taint.plan_dependent())
    {
        "unless-pruned"
    } else {
        "no"
    }
}

/// The report lines (without the `bazel-rs-snapshot:` prefix).
pub fn summary(
    command: &str,
    recording: &Recording,
    policy: crate::snapshot::validate::Policy,
    validations: &[crate::snapshot::validate::Report],
) -> Vec<String> {
    let mut lines = Vec::new();
    let eligible = eligibility(recording);
    lines.push(format!(
        "record: command={command} window={} ms eligible={eligible}",
        recording.window.map_or_else(|| "open".to_owned(), millis),
    ));
    let mut by_op: BTreeMap<&str, usize> = BTreeMap::new();
    let mut projections: BTreeMap<&str, usize> = BTreeMap::new();
    let mut racy_reads = 0usize;
    let mut errors = 0usize;
    for (_, observation) in &recording.observations {
        *by_op.entry(observation.op.name()).or_default() += 1;
        if let Op::ReadDir(projection) = &observation.op {
            let kind = match projection {
                observed_fs::Projection::All => "all",
                observed_fs::Projection::Directories => "dirs",
                observed_fs::Projection::Glob { .. } => "glob",
            };
            *projections.entry(kind).or_default() += 1;
        }
        match observation.outcome {
            Outcome::Read { racy: true, .. } | Outcome::Listing { racy: true, .. } => {
                racy_reads += 1;
            }
            Outcome::Error(_) => errors += 1,
            _ => {}
        }
    }
    let ops = ["READ", "STAT", "LSTAT", "READDIR", "CANON", "READLINK"]
        .iter()
        .map(|op| format!("{op} {}", by_op.get(op).copied().unwrap_or(0)))
        .collect::<Vec<_>>()
        .join(", ");
    let projections = projections
        .iter()
        .map(|(kind, count)| format!("{kind} {count}"))
        .collect::<Vec<_>>()
        .join(", ");
    lines.push(format!(
        "observations: {} over {} paths ({ops}; READDIR projections: {}); racy {racy_reads}; error outcomes {errors}",
        recording.observations.len(),
        recording.distinct_paths(),
        if projections.is_empty() { "none".to_owned() } else { projections },
    ));
    lines.push(format!(
        "env: {} repository environment variables read",
        recording.env.len()
    ));
    let mut kinds: BTreeMap<&str, (usize, usize)> = BTreeMap::new();
    let mut integrity_racy = 0usize;
    for repository in &recording.repositories {
        let entry = kinds.entry(repository.kind).or_default();
        entry.0 += 1;
        entry.1 += repository.entries.len();
        integrity_racy += repository.entries.iter().filter(|entry| entry.racy).count();
    }
    let (inside, below_links) =
        crate::snapshot::validate::observations_inside_verified_trees(recording);
    lines.push(format!(
        "repositories: {} verified trees ({}), {} integrity entries (racy {integrity_racy}); {inside} observations inside verified trees ({below_links} at or below a tree's symlinks)",
        recording.repositories.len(),
        kinds
            .iter()
            .map(|(kind, (trees, entries))| format!("{kind} {trees}/{entries}"))
            .collect::<Vec<_>>()
            .join(", "),
        kinds.values().map(|(_, entries)| entries).sum::<usize>(),
    ));
    // A link crossed into a store tree no repository of the run has as its
    // root: an older publication of the target repository (or one whose
    // repository the run never demanded). It resolves to that verified
    // tree, where Bazel would read the repository's current contents (a
    // hit and a miss agree). A repository the run executed or published may
    // root a tree without a REPO record, so such runs report nothing.
    if !recording.crossed.is_empty()
        && !recording
            .taints
            .contains_key(&Taint::RepositoryRuleExecuted)
        && !recording
            .taints
            .contains_key(&Taint::RepositoryMaterialized)
    {
        lines.push(format!(
            "link anchors: {} store tree(s) a link crossed into never became a repository root in this run (stale links into older publications?): {}",
            recording.crossed.len(),
            recording
                .crossed
                .iter()
                .map(|root| escape(root.as_os_str()))
                .collect::<Vec<_>>()
                .join(" | ")
        ));
    }
    match &recording.plan_registered {
        Some(registered) => {
            let kept = |taint: Taint| {
                recording
                    .taints
                    .get(&taint)
                    .map_or(0, |record| record.count)
            };
            lines.push(format!(
                "plan check: {}",
                [Taint::BuildInfo, Taint::MapEach]
                    .into_iter()
                    .map(|taint| {
                        let record = registered.get(&taint);
                        format!(
                            "{} registered {} ({} distinct), {} in the plan",
                            taint.name(),
                            record.map_or(0, |record| record.count),
                            record.map_or(0, |record| record.details.len()),
                            kept(taint)
                        )
                    })
                    .collect::<Vec<_>>()
                    .join("; ")
            ));
        }
        None => lines.push("plan check: not run (no planned request)".to_owned()),
    }
    if recording.taints.is_empty() {
        lines.push("taints: none".to_owned());
    }
    for (taint, record) in &recording.taints {
        const SHOWN: usize = 12;
        let more = record.details.len().saturating_sub(SHOWN);
        lines.push(format!(
            "taint {}{}: {} occurrence(s), {} distinct: {}{}",
            taint.name(),
            match (taint.plan_dependent(), recording.plan_registered.is_some()) {
                (true, false) => " (registered by analysis; the plan may prune it)",
                (true, true) => " (in the planned request)",
                _ => "",
            },
            record.count,
            record.details.len(),
            record.details[..record.details.len().min(SHOWN)].join(" | "),
            if more > 0 {
                format!(" | (+{more} more in the dump)")
            } else {
                String::new()
            }
        ));
    }
    for validation in validations {
        lines.push(validation_line(policy, validation));
    }
    if let Some(validation) = validations.first() {
        for example in &validation.examples {
            lines.push(format!("validation: {example}"));
        }
        for path in &validation.content_checked {
            lines.push(format!("validation content check: {path}"));
        }
    }
    lines
}

/// The digest of the INPUTS section a snapshot of `recording` would hold
/// (crate::snapshot::format::encode_inputs of `policy`'s selection, the
/// replay records and their pinned tables) with what depends on when the
/// run happened zeroed: the window start, the racy flags and the replay
/// records' execution times. Two runs that observed the same inputs —
/// e.g. at any `--loading_phase_threads`, with phase-1 prefetch (W6R) —
/// have the same digest: the record mode's dump prints it as
/// `INPUTS-CANONICAL <blake3>` (`BAZEL_RS_LOADING_JITTER` turns the `on`
/// mode off, so determinism tests compare record-mode dumps).
pub fn canonical_inputs_digest(
    recording: &Recording,
    policy: crate::snapshot::validate::Policy,
    records: &[crate::snapshot::replay::ReplayRecord],
    tables: Option<&crate::snapshot::replay::PinnedTables>,
) -> String {
    let mut selected = policy.select(recording);
    selected.window_start_ns = 0;
    selected.window = None;
    for (_, observation) in &mut selected.observations {
        if let Outcome::Read { racy, .. } | Outcome::Listing { racy, .. } = &mut observation.outcome
        {
            *racy = false;
        }
    }
    for repository in &mut selected.repositories {
        for entry in &mut repository.entries {
            entry.racy = false;
        }
    }
    let records: Vec<crate::snapshot::replay::ReplayRecord> = records
        .iter()
        .cloned()
        .map(|mut record| {
            record.elapsed = std::time::Duration::ZERO;
            record
        })
        .collect();
    let bytes = crate::snapshot::format::encode_inputs(&selected, &records, tables, false);
    blake3::hash(&bytes).to_hex().to_string()
}

/// Write the text dump (`BAZEL_RS_SNAPSHOT_DUMP`): the record mode's, and
/// a hit's INPUTS (crate::snapshot::hit, for the syscall audit of the hit
/// path). `canonical`: the record mode's [`canonical_inputs_digest`].
pub(crate) fn write_dump(
    path: &std::path::Path,
    recording: &Recording,
    summary: &[String],
    records: &[crate::snapshot::replay::ReplayRecord],
    tables: Option<&crate::snapshot::replay::PinnedTables>,
    replay: Option<&crate::snapshot::replay::Report>,
    canonical: Option<&str>,
) -> std::io::Result<()> {
    let mut text = String::new();
    let _ = writeln!(text, "# bazel-rs snapshot recording (L2-W3, W4R) v2");
    let _ = writeln!(text, "window_start_ns {}", recording.window_start_ns);
    if let Some(canonical) = canonical {
        let _ = writeln!(text, "INPUTS-CANONICAL {canonical}");
    }
    for (path, observation) in &recording.observations {
        let op = match &observation.op {
            Op::ReadDir(projection) => format!("READDIR[{projection}]"),
            op => op.name().to_owned(),
        };
        let _ = writeln!(
            text,
            "OBS {} {op} {}",
            escape(path.as_os_str()),
            observation.outcome
        );
    }
    for (name, env) in &recording.env {
        let _ = writeln!(
            text,
            "ENV {} {}",
            escape(name),
            env.digest.map_or_else(
                || "unset".to_owned(),
                |digest| format!("sha256:{}", observed_fs::hex(&digest))
            ),
        );
    }
    for repository in &recording.repositories {
        let _ = writeln!(
            text,
            "REPO {} {} {} manifest:{} entries:{}",
            repository.kind,
            escape(std::ffi::OsStr::new(&repository.name)),
            escape(repository.root.as_os_str()),
            repository
                .manifest
                .as_ref()
                .map_or_else(|| "-".to_owned(), |path| escape(path.as_os_str())),
            repository.entries.len()
        );
        for entry in &repository.entries {
            let state = match &entry.state {
                Ok((kind, key)) => format!("{} key:{key}", kind.letter() as char),
                Err(errno) => format!("error:{errno}"),
            };
            let _ = writeln!(
                text,
                "INT {} {state}{}",
                escape(entry.path.as_os_str()),
                if entry.racy { " racy" } else { "" }
            );
        }
    }
    for (taint, record) in &recording.taints {
        for detail in &record.details {
            let _ = writeln!(
                text,
                "TAINT {} count:{} {detail}",
                taint.name(),
                record.count
            );
        }
    }
    // What analysis registered of the plan-dependent taints, once the
    // planned request decided them (only the plan's own are TAINTs).
    for (taint, record) in recording.plan_registered.iter().flatten() {
        for detail in &record.details {
            let _ = writeln!(
                text,
                "REGISTERED {} count:{} {detail}",
                taint.name(),
                record.count
            );
        }
    }
    // The replay's pinned tables and records (crate::snapshot::replay).
    if let Some(tables) = tables {
        for (name, root) in &tables.roots {
            let _ = writeln!(
                text,
                "ROOT {} {} generated:{}",
                escape(std::ffi::OsStr::new(name)),
                escape(root.path.as_os_str()),
                u8::from(root.generated)
            );
        }
        for (name, mapping) in &tables.mappings {
            let _ = writeln!(
                text,
                "MAPPING {} entries:{} isolated:{}",
                escape(std::ffi::OsStr::new(name)),
                mapping.mapping.len(),
                mapping.isolated.len()
            );
        }
    }
    for definition in replay.map_or(&[][..], |replay| &replay.definitions[..]) {
        let _ = writeln!(
            text,
            "DEFINITION {} records:{} load:{} thread-cpu:{}{}",
            escape(std::ffi::OsStr::new(&definition.source)),
            definition.records,
            millis(definition.wall),
            definition.cpu.map_or_else(|| "-".to_owned(), millis),
            definition
                .error
                .as_deref()
                .map(|error| format!(" error:{}", escape(std::ffi::OsStr::new(error))))
                .unwrap_or_default(),
        );
    }
    let replayed: BTreeMap<&str, &crate::snapshot::replay::Replayed> = replay
        .map(|replay| {
            replay
                .replayed
                .iter()
                .map(|replayed| (replayed.canonical.as_str(), replayed))
                .collect()
        })
        .unwrap_or_default();
    for record in records {
        let outcome = replayed.get(record.canonical.as_str());
        let _ = writeln!(
            text,
            "REPLAY {} {} definition:{} flags:{} output:{} recorded:{} replay:{} root:{}{}",
            escape(std::ffi::OsStr::new(&record.canonical)),
            match outcome {
                None => "not-replayed",
                Some(replayed) if replayed.mismatch.is_none() => "match",
                Some(_) => "mismatch",
            },
            escape(std::ffi::OsStr::new(&record.definition.to_string())),
            record.flags(),
            record.output.len(),
            millis(record.elapsed),
            outcome.map_or_else(|| "-".to_owned(), |replayed| millis(replayed.wall)),
            escape(record.root.as_os_str()),
            outcome
                .and_then(|replayed| replayed.mismatch.as_deref())
                .map(|reason| format!(" reason:{}", escape(std::ffi::OsStr::new(reason))))
                .unwrap_or_default(),
        );
    }
    for line in summary {
        let _ = writeln!(text, "SUMMARY {line}");
    }
    let mut file = std::fs::File::create(path)?;
    file.write_all(text.as_bytes())?;
    file.flush()
}
