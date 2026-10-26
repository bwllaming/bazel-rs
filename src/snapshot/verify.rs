//! `BAZEL_RS_SNAPSHOT=verify` (PLAN-L2 §1b layer 3 "shadow verify", PLAN-L2
//! update 1 row W6): every snapshot hit also runs the full analysis, in its
//! shadow, and compares the two before anything executes.
//!
//! ```text
//! check point: hit (validated ∥ replayed ∥ decoded; nothing printed)
//!   -> [`start`]: recording window + replay capture + stderr captured (Mode::Shadow, not printed)
//!   -> the full analysis, exactly as a miss of `BAZEL_RS_SNAPSHOT=on` runs it
//!   -> [`Verify::conclude`] at the end of planning, comparing:
//!        * the planned request: its encoding must be the PLAN section's bytes
//!          (the encoding is deterministic; the post-planning warnings and
//!          errors are part of it and are named on their own)
//!        * what planning printed: the captured bytes must be the EVENTS section's
//!        * the recording, under the snapshot's validation policy: an
//!          observation (path and operation), environment variable or
//!          verified repository tree recorded now but not in the snapshot is
//!          an incomplete recording; an outcome recorded now that differs
//!          from the snapshot's (contents, listing, type, target; stat keys
//!          and racy flags aside) is a mismatch too
//!        * the local repository rules the full analysis executed: each
//!          execution's identity (identity-inputs and tree digests, identity,
//!          published root, definition, bound attributes, output, host
//!          facilities) must be its replay record's
//!        * the full analysis must itself be saveable: no taint, no pending
//!          lockfile write, a complete capture, no planning error
//!   -> match: the hit's own bytes print and the snapshot's plan executes
//!      (what a user of `on` gets), refreshed keys are written back;
//!      mismatch: every difference on the diagnostics stream, one line
//!      appended to `<state>/snapshots/verify.log` ([`LOG`]: time, command,
//!      key, the first difference's class, the first taint's name and the
//!      first differing path (L2-W5g), the target patterns;
//!      crate::snapshot::telemetry) and — L2-W5i — that line with every
//!      difference to `verify-details.log`, the snapshot set aside in
//!      `<state>/snapshots/mismatched/` (crate::snapshot::store::set_aside,
//!      the 3 newest kept; before, it was deleted: the next run analyzes
//!      and saves anew, so a persistent cause shows on every other run),
//!      nothing saved — then
//!      (L2-W5f, the default) what the full analysis printed while planning
//!      prints and the FULL ANALYSIS's plan executes: the command's output
//!      and exit code are those of a run without the snapshot. With
//!      `BAZEL_RS_SNAPSHOT_VERIFY_STRICT=1` ([`STRICT_VAR`]; tests and CI)
//!      nothing executes and the command exits 70.
//! ```
//!
//! A miss behaves exactly like `on` (the full analysis is saved), so a
//! verify session can run any sequence of commands. Observations the
//! snapshot holds but the full analysis did not make are reported (EXPLAIN)
//! but do not fail: they only cost a hit extra checks. A full analysis that
//! ends before planning completed (an error the hit did not reproduce) is
//! a mismatch too ([`exit_if_abandoned`], from main: the full analysis's
//! error is the command's, or exit 70 under the strict switch).

use crate::observed_fs::{self, Op, Outcome, Recording, escape};
use crate::registry_session::SessionHandle;
use crate::request::{Diagnostics, PlannedRequest};
use crate::snapshot::capture::{self, Capture, Captured};
use crate::snapshot::hit::{Evidence, Hit};
use crate::snapshot::key::Key;
use crate::snapshot::replay::ReplayRecord;
use crate::snapshot::validate::Policy;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::ffi::{OsStr, OsString};
use std::io::Write as _;
use std::os::unix::ffi::OsStrExt;
use std::path::PathBuf;
use std::sync::{Mutex, PoisonError};
use std::time::{Duration, Instant};

/// The exit code of a mismatch under [`STRICT_VAR`] (EX_SOFTWARE).
pub const EXIT_MISMATCH: i32 = 70;
/// `BAZEL_RS_SNAPSHOT_VERIFY_STRICT=1`: a mismatch exits [`EXIT_MISMATCH`]
/// before anything executes (tests, CI). Unset (the default for a soak): a
/// mismatch is reported and logged, and the full analysis's plan executes.
pub const STRICT_VAR: &str = "BAZEL_RS_SNAPSHOT_VERIFY_STRICT";
/// The mismatch log below the snapshot store root (`<state>/snapshots/`):
/// one line per mismatch, appended (crate::snapshot::telemetry: bounded,
/// rotated).
pub const LOG: &str = crate::snapshot::telemetry::VERIFY_LOG;
/// How long the end of the shadow window waits for the capture's reader.
const CAPTURE_BOUND: Duration = Duration::from_millis(500);
/// Items shown per kind of difference.
const SHOWN: usize = 10;
/// Differing actions shown with their line diff.
const ACTIONS_SHOWN: usize = 3;
/// Lines shown per side of a line diff.
const DIFF_LINES: usize = 12;

/// Whether [`STRICT_VAR`] is on.
pub fn strict() -> bool {
    crate::diagnostic_switch(std::env::var_os(STRICT_VAR))
}

/// What a mismatch's log line names besides the snapshot: the command and
/// its target patterns (crate::snapshot::verify::start's caller, main).
#[derive(Clone, Debug, Default)]
pub struct Context {
    pub command: String,
    pub targets: Vec<String>,
    pub explain: bool,
}

/// The shadow full analysis of one hit.
pub struct Verify {
    hit: Option<Box<Hit>>,
    capture: Option<Capture>,
    window: Option<Captured>,
    registry: Option<SessionHandle>,
    started: Instant,
    context: Context,
    strict: bool,
    concluded: bool,
}

/// How [`Verify::conclude`] decided.
pub enum Concluded {
    /// The full analysis agrees with the hit: the hit executes (its window
    /// bytes print first, crate::main's `planned_hit`).
    Agreed(Box<Hit>),
    /// A mismatch, reported, logged and the snapshot set aside (never under
    /// [`STRICT_VAR`], which exits): what the full analysis printed while
    /// planning is printed, and the caller executes the full analysis's
    /// own planned request, unsaved.
    Disagreed,
}

/// Start the shadow full analysis of `hit` at the snapshot check point:
/// open the recording window and the replay capture, capture stderr. A
/// session that cannot start cannot verify: under [`STRICT_VAR`] that is a
/// verification failure (exit 70); otherwise it is logged and `None` is
/// returned — the command runs the full analysis without comparing (the
/// snapshot stays: nothing contradicted it).
pub fn start(hit: Box<Hit>, context: Context) -> Option<Box<Verify>> {
    let key = hit.session.key().short();
    let mut verify = Box::new(Verify {
        hit: Some(hit),
        capture: None,
        window: None,
        registry: None,
        started: Instant::now(),
        context,
        strict: strict(),
        concluded: false,
    });
    let failure = if verify
        .hit
        .as_ref()
        .is_some_and(|hit| hit.evidence.is_none())
    {
        Some("the hit kept nothing to compare".to_owned())
    } else if !observed_fs::open_window() {
        Some("the recording window was already open".to_owned())
    } else {
        // A repository rule the shadow executes is a difference from the
        // hit, which executed none: never re-probed away.
        observed_fs::mark_shadow_window();
        crate::snapshot::replay::install();
        match capture::start(capture::Mode::Shadow) {
            Ok(capture) => {
                verify.capture = Some(capture);
                None
            }
            Err(error) => {
                observed_fs::close_window();
                Some(format!("cannot capture stderr: {error}"))
            }
        }
    };
    let Some(failure) = failure else {
        return Some(verify);
    };
    verify.concluded = true;
    let mut report = Report::default();
    report.mismatch(
        Class::CannotVerify,
        format!("cannot verify: {failure}"),
        Vec::new(),
    );
    let stored = verify
        .hit
        .as_ref()
        .map(|hit| (hit.session.cache().to_path_buf(), *hit.session.key()));
    // Nothing contradicts the snapshot: it stays (`false`: not set aside).
    report.conclude(&key, stored.as_ref(), false, &verify.context, verify.strict);
    None
}

impl Verify {
    /// The registry session whose pending lockfile writes the full analysis
    /// must not have.
    pub fn set_registry_session(&mut self, registry: SessionHandle) {
        self.registry = Some(registry);
    }

    /// Planning returned: the window and the capture end here.
    pub fn end_window(&mut self) {
        observed_fs::close_window();
        if let Some(capture) = self.capture.take() {
            self.window = Some(capture.stop(CAPTURE_BOUND));
        }
    }

    /// Compare the full analysis — its planned request (`full`; on failure
    /// with the diagnostics `failed`), what it printed, recorded and
    /// executed — with the hit. On a match the hit is returned for its own
    /// execution (its refreshed keys are being written back); on a mismatch
    /// every difference is printed on the diagnostics stream, logged, the
    /// snapshot set aside, and — under [`STRICT_VAR`] the process exits with
    /// [`EXIT_MISMATCH`] — what the full analysis printed while planning is
    /// written to stderr: the caller executes the full analysis's plan.
    pub fn conclude(
        mut self,
        full: &Result<PlannedRequest, String>,
        failed: Option<&Diagnostics>,
        status_computed: bool,
    ) -> Concluded {
        self.end_window();
        self.concluded = true;
        let analysis = self.started.elapsed();
        let compared = Instant::now();
        let mut hit = self.hit.take().expect("a verify session holds its hit");
        let key = hit.session.key().short();
        let Evidence { plan, inputs } = hit
            .evidence
            .take()
            .expect("a verify session's hit holds its evidence");
        let mut report = Report::default();
        match full {
            Ok(request) => compare_plan(&plan, &hit.request, request, &mut report),
            Err(error) => {
                let mut details = vec![format!("error: {error}")];
                if let Some(failed) = failed {
                    details.extend(failed.warnings.iter().map(|w| format!("warning: {w}")));
                    details.extend(failed.errors.iter().map(|e| format!("error: {e}")));
                }
                report.mismatch(
                    Class::AnalysisFailed,
                    "the full analysis failed where the snapshot planned".to_owned(),
                    details,
                );
            }
        }
        compare_window(&hit.window_stderr, self.window.as_ref(), &mut report);
        if status_computed != inputs.workspace_status {
            report.mismatch(
                Class::Status,
                format!(
                    "the full analysis {} the workspace status, the snapshot {}",
                    if status_computed {
                        "computed"
                    } else {
                        "did not compute"
                    },
                    if inputs.workspace_status {
                        "records it computed"
                    } else {
                        "records it not computed"
                    }
                ),
                Vec::new(),
            );
        }
        let recording = observed_fs::recorder().map(|recorder| recorder.take());
        let observed = match recording {
            Some(recording) => {
                if let Some(first) = recording.taints.keys().next() {
                    report.taint = Some(first.name());
                    report.mismatch(
                        Class::Taint,
                        "the full analysis would not be saved".to_owned(),
                        vec![crate::snapshot::save::taints(&recording.taints)],
                    );
                }
                let recording = Policy::current().select_owned(recording);
                compare_recordings(&inputs.recording, &recording, &mut report);
                recording.observations.len()
            }
            None => {
                report.mismatch(
                    Class::Recording,
                    "the full analysis recorded nothing".to_owned(),
                    Vec::new(),
                );
                0
            }
        };
        let executed = crate::snapshot::replay::global()
            .map(|capture| capture.records())
            .unwrap_or_default();
        compare_replays(&inputs.replay, &executed, &mut report);
        if let Some(session) = self.registry.as_ref().and_then(SessionHandle::session) {
            if session.updated_lockfile().is_some() {
                report.mismatch(
                    Class::Lockfile,
                    "the full analysis would write MODULE.bazel.lock".to_owned(),
                    Vec::new(),
                );
            }
            if let Some((path, _)) = session.updated_hidden_lockfile() {
                report.mismatch(
                    Class::Lockfile,
                    format!(
                        "the full analysis would write the hidden lockfile {}",
                        escape(path.as_os_str())
                    ),
                    Vec::new(),
                );
            }
        }
        let compare = compared.elapsed();
        if !report.mismatches.is_empty() {
            let stored = (hit.session.cache().to_path_buf(), *hit.session.key());
            report.conclude(&key, Some(&stored), true, &self.context, self.strict);
            // Not strict (it exited): what the full analysis printed while
            // planning, then its own plan executes (the caller) — unless
            // the capture forwarded it already (an overflow or an
            // incomplete stop, L2-W5g).
            if let Some(window) = self.window.as_ref().filter(|window| !window.forwarded) {
                let mut stderr = std::io::stderr().lock();
                let _ = stderr
                    .write_all(&window.bytes)
                    .and_then(|()| stderr.flush());
            }
            return Concluded::Disagreed;
        }
        let ms = |duration: Duration| duration.as_secs_f64() * 1000.0;
        hit.session.log().log_verified(
            &key,
            ms(analysis) + ms(compare),
            &format!(
                "analysis:{:.1},compare:{:.1},actions:{}",
                ms(analysis),
                ms(compare),
                hit.request.nodes.len()
            ),
        );
        if self.context.explain {
            observed_fs::diag(&format!(
                "verified {key}: the full analysis agrees with the snapshot ({} actions, PLAN {} bytes, window stderr {} bytes, {} observations, {} local repositories) — full analysis {:.1} ms, comparison {:.1} ms",
                hit.request.nodes.len(),
                plan.len(),
                hit.window_stderr.len(),
                observed,
                executed.len(),
                ms(analysis),
                ms(compare),
            ));
            for note in &report.notes {
                observed_fs::diag(&format!("verify note: {note}"));
            }
        }
        if crate::diagnostic_switch(std::env::var_os("BAZEL_RS_TIMING")) {
            crate::snapshot::diag::line_fmt(format_args!(
                "PERF-SNAPSHOT verify analysis_ms={:.3} compare_ms={:.3} plan_bytes={} observations={} replays={}",
                ms(analysis),
                ms(compare),
                plan.len(),
                observed,
                executed.len(),
            ));
        }
        drop(plan);
        hit.session.start_deferred_refresh(inputs);
        Concluded::Agreed(hit)
    }
}

/// A snapshot in a cache's store.
type Stored = (PathBuf, Key);

/// A verify session that ended without [`Verify::conclude`]: its snapshot
/// and log context (main reports it, [`exit_if_abandoned`]).
static ABANDONED: Mutex<Option<(Stored, Context, bool)>> = Mutex::new(None);

impl Drop for Verify {
    fn drop(&mut self) {
        if self.concluded {
            return;
        }
        self.end_window();
        if !self.strict {
            // The full analysis ends here (its error follows, from main):
            // what it printed so far is its output (unless the capture
            // forwarded it already, L2-W5g).
            if let Some(window) = self.window.as_ref().filter(|window| !window.forwarded) {
                let mut stderr = std::io::stderr().lock();
                let _ = stderr
                    .write_all(&window.bytes)
                    .and_then(|()| stderr.flush());
            }
        }
        if let Some(hit) = &self.hit {
            *ABANDONED.lock().unwrap_or_else(PoisonError::into_inner) = Some((
                (hit.session.cache().to_path_buf(), *hit.session.key()),
                std::mem::take(&mut self.context),
                self.strict,
            ));
        }
    }
}

/// main, when the command returned: a verify session whose full analysis
/// never completed planning (the command ended first, with `error` if it
/// failed) is a mismatch — the snapshot planned where the full analysis did
/// not. Prints and logs it and sets the snapshot aside; under [`STRICT_VAR`]
/// exits with [`EXIT_MISMATCH`], else returns (the full analysis's result is
/// the command's).
pub fn exit_if_abandoned(error: Option<&str>) {
    let Some((stored, context, strict)) = ABANDONED
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .take()
    else {
        return;
    };
    let mut report = Report::default();
    report.mismatch(
        Class::Abandoned,
        "the full analysis ended before planning completed".to_owned(),
        vec![match error {
            Some(error) => format!("error: {error}"),
            None => "without an error".to_owned(),
        }],
    );
    report.conclude(&stored.1.short(), Some(&stored), true, &context, strict);
}

/// The class of a difference: the verify log names the first one.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Class {
    /// The session could not start (no comparison).
    CannotVerify,
    /// The full analysis failed where the snapshot planned.
    AnalysisFailed,
    /// The full analysis ended before planning completed.
    Abandoned,
    /// The planned request differs from the PLAN section.
    Plan,
    /// What planning printed differs from the EVENTS section.
    Window,
    /// The full analysis would not be saved (a taint).
    Taint,
    /// An observation or a verified tree recorded now but not in the
    /// snapshot, or with another outcome.
    Recording,
    /// An environment variable read now with another value, or not in
    /// the snapshot.
    Env,
    /// A local repository rule's execution differs from its replay record.
    Replay,
    /// The full analysis would write a lockfile.
    Lockfile,
    /// The full analysis computed the workspace status where the snapshot
    /// did not, or the reverse.
    Status,
}

impl Class {
    fn name(self) -> &'static str {
        match self {
            Self::CannotVerify => "cannot-verify",
            Self::AnalysisFailed => "analysis-failed",
            Self::Abandoned => "abandoned",
            Self::Plan => "plan",
            Self::Window => "window",
            Self::Taint => "taint",
            Self::Recording => "recording",
            Self::Env => "env",
            Self::Replay => "replay",
            Self::Lockfile => "lockfile",
            Self::Status => "status",
        }
    }
}

/// The differences found (each with its class and detail lines) and the
/// notes; for the log line, the first taint's name and the first path a
/// difference names (an observation, a verified tree's root, a local
/// repository's published root, in the order the differences are found).
#[derive(Default)]
struct Report {
    mismatches: Vec<(Class, String, Vec<String>)>,
    notes: Vec<String>,
    taint: Option<&'static str>,
    path: Option<OsString>,
}

impl Report {
    fn mismatch(&mut self, class: Class, title: String, details: Vec<String>) {
        self.mismatches.push((class, title, details));
    }

    /// The first differing path (kept once set).
    fn path(&mut self, path: &OsStr) {
        if self.path.is_none() {
            self.path = Some(path.to_owned());
        }
    }

    /// Print every difference on the diagnostics stream, append the log
    /// line and — L2-W5i — the differences to `verify-details.log`, set the
    /// snapshot aside (`stored`, when `remove`: moved to
    /// `<state>/snapshots/mismatched/`, crate::snapshot::store::set_aside;
    /// before, it was deleted) and — `strict` — exit with [`EXIT_MISMATCH`].
    fn conclude(
        self,
        key: &str,
        stored: Option<&Stored>,
        remove: bool,
        context: &Context,
        strict: bool,
    ) {
        use crate::snapshot::store::SetAside;
        let aside = if remove {
            stored.and_then(|(cache, key)| crate::snapshot::store::set_aside(cache, key).ok())
        } else {
            None
        };
        let class = self
            .mismatches
            .first()
            .map_or("unknown", |(class, _, _)| class.name());
        // The differences, as the diagnostics stream shows them (without
        // its prefix) and verify-details.log keeps them.
        let mut lines = Vec::new();
        for (_, title, details) in &self.mismatches {
            lines.push(format!("verify mismatch: {title}"));
            lines.extend(details.iter().map(|detail| format!("verify   {detail}")));
        }
        lines.extend(self.notes.iter().map(|note| format!("verify note: {note}")));
        let logged = stored.and_then(|(cache, _)| {
            // The event log counts it among the hits' outcomes (L2-W5h).
            crate::snapshot::telemetry::Context {
                cache: cache.clone(),
                mode: crate::snapshot::telemetry::MODE_VERIFY,
                command: context.command.clone(),
                targets: context.targets.clone(),
            }
            .log_mismatch(
                key,
                class,
                self.path.as_deref(),
                &format!(
                    "differences:{},taint:{}",
                    self.mismatches.len(),
                    self.taint.unwrap_or("-")
                ),
            );
            let path = crate::snapshot::telemetry::log_path(cache, LOG);
            let line = crate::snapshot::telemetry::verify_line(
                key,
                class,
                self.mismatches.len(),
                self.taint,
                self.path.as_deref(),
                &context.command,
                &context.targets,
            );
            crate::snapshot::telemetry::append(&path, &line).ok()?;
            // L2-W5i: every difference beside it (before, on stderr only),
            // under that line, and where the snapshot went.
            let snapshot = match &aside {
                Some(SetAside::Moved(target)) => {
                    format!("snapshot: set aside as {}", escape(target.as_os_str()))
                }
                Some(SetAside::Removed { error }) => {
                    format!("snapshot: removed (it could not be set aside: {error})")
                }
                None if remove => "snapshot: gone already (another command removed it)".to_owned(),
                None => "snapshot: kept".to_owned(),
            };
            let block = crate::snapshot::telemetry::details_block(
                &line,
                std::iter::once(snapshot.as_str()).chain(lines.iter().map(String::as_str)),
            );
            let _ = crate::snapshot::telemetry::append(
                &crate::snapshot::telemetry::log_path(
                    cache,
                    crate::snapshot::telemetry::VERIFY_DETAILS_LOG,
                ),
                &block,
            );
            Some(path)
        });
        let outcome = if strict {
            format!("exit {EXIT_MISMATCH}, nothing executed or saved")
        } else if self
            .mismatches
            .first()
            .is_some_and(|(class, _, _)| *class == Class::CannotVerify)
        {
            "the full analysis runs unverified and unsaved".to_owned()
        } else {
            "the full analysis's result is the command's (nothing saved)".to_owned()
        };
        observed_fs::diag(&format!(
            "verify {key}: MISMATCH: the full analysis disagrees with the snapshot hit ({} difference(s), first: {class}); {outcome}{}{}",
            self.mismatches.len(),
            match &aside {
                Some(SetAside::Moved(target)) => format!(
                    "; the snapshot is set aside as {} (the next run analyzes and saves anew)",
                    escape(target.as_os_str())
                ),
                Some(SetAside::Removed { .. }) => {
                    "; the snapshot is removed (the next run analyzes and saves anew)".to_owned()
                }
                None => String::new(),
            },
            logged
                .map(|path| format!(
                    "; logged to {} (the differences: {})",
                    escape(path.as_os_str()),
                    crate::snapshot::telemetry::VERIFY_DETAILS_LOG
                ))
                .unwrap_or_default(),
        ));
        for line in &lines {
            observed_fs::diag(line);
        }
        if strict {
            crate::digest_memo::finish();
            crate::progress::abandon();
            std::process::exit(EXIT_MISMATCH);
        }
    }
}

// ---- the planned request ---------------------------------------------------

/// The outline of a request, field by field (every field by name: a new
/// one is a compile error here).
fn outline(request: &PlannedRequest) -> Vec<(&'static str, String)> {
    let PlannedRequest {
        command,
        keep_going,
        execute,
        nodes,
        requested,
        aspect_reports,
        non_empty_targets,
        run_plans,
        configurations,
        warnings,
        errors,
        build_info,
    } = request;
    vec![
        ("command", format!("{command:?}")),
        ("keep_going", format!("{keep_going:?}")),
        ("execute", execute.to_string()),
        ("actions", nodes.len().to_string()),
        ("requested", format!("{requested:?}")),
        ("aspect_reports", format!("{aspect_reports:?}")),
        ("non_empty_targets", non_empty_targets.to_string()),
        ("run_plans", format!("{run_plans:?}")),
        ("configurations", format!("{configurations:?}")),
        ("warnings", format!("{warnings:?}")),
        ("errors", format!("{errors:?}")),
        ("build_info", format!("{build_info:?}")),
    ]
}

fn compare_plan(
    bytes: &[u8],
    snapshot: &PlannedRequest,
    full: &PlannedRequest,
    report: &mut Report,
) {
    let encoded = match crate::snapshot::codec::encode(full) {
        Ok(encoded) => encoded,
        Err(error) => {
            report.mismatch(
                Class::Plan,
                "the full analysis's planned request cannot be encoded".to_owned(),
                vec![error.to_string()],
            );
            return;
        }
    };
    if encoded == bytes {
        return;
    }
    let mut details = vec![format!(
        "encoded: {} bytes now, {} in the snapshot",
        encoded.len(),
        bytes.len()
    )];
    // The post-planning diagnostics, by entry.
    for (name, old, new) in [
        ("warning", &snapshot.warnings, &full.warnings),
        ("error", &snapshot.errors, &full.errors),
    ] {
        for entry in old.iter().filter(|entry| !new.contains(entry)) {
            details.push(format!("{name} only in the snapshot: {entry}"));
        }
        for entry in new.iter().filter(|entry| !old.contains(entry)) {
            details.push(format!("{name} only in the full analysis: {entry}"));
        }
    }
    for ((name, old), (_, new)) in outline(snapshot).into_iter().zip(outline(full)) {
        if old != new && !matches!(name, "warnings" | "errors") {
            details.push(format!("outline {name}: {}", difference(&old, &new)));
        }
    }
    let ids = |request: &PlannedRequest| -> HashSet<String> {
        request
            .nodes
            .iter()
            .map(|node| node.action.id.clone())
            .collect()
    };
    let (old_ids, new_ids) = (ids(snapshot), ids(full));
    let only = |from: &PlannedRequest, other: &HashSet<String>| -> Vec<String> {
        from.nodes
            .iter()
            .map(|node| node.action.id.clone())
            .filter(|id| !other.contains(id))
            .collect()
    };
    let (old_only, new_only) = (only(snapshot, &new_ids), only(full, &old_ids));
    if !old_only.is_empty() {
        details.push(format!(
            "{} action(s) only in the snapshot: {}",
            old_only.len(),
            shown(&old_only)
        ));
    }
    if !new_only.is_empty() {
        details.push(format!(
            "{} action(s) only in the full analysis: {}",
            new_only.len(),
            shown(&new_only)
        ));
    }
    let mut differing = 0;
    // PA-1's shared input groups are not part of a node's own encoding:
    // compared by value too, once per pair of groups (thousands of holders
    // share a few groups).
    let mut same_groups: HashMap<(usize, usize), bool> = HashMap::new();
    let mut groups_equal = |old: &crate::scheduler::PlannedAction,
                            new: &crate::scheduler::PlannedAction| {
        old.groups.len() == new.groups.len()
            && old.groups.iter().zip(&new.groups).all(|(a, b)| {
                *same_groups
                    .entry((
                        std::sync::Arc::as_ptr(a).addr(),
                        std::sync::Arc::as_ptr(b).addr(),
                    ))
                    .or_insert_with(|| crate::snapshot::codec::same_group(a, b))
            })
    };
    for (index, (old, new)) in snapshot.nodes.iter().zip(&full.nodes).enumerate() {
        let same = old.action.id == new.action.id
            && postcard::to_stdvec(old).ok() == postcard::to_stdvec(new).ok()
            && groups_equal(old, new);
        if same {
            continue;
        }
        differing += 1;
        if differing <= ACTIONS_SHOWN {
            details.push(format!(
                "action #{index} ({} in the snapshot, {} now):",
                old.action.id, new.action.id
            ));
            details.extend(
                line_diff(&format!("{old:#?}"), &format!("{new:#?}"))
                    .into_iter()
                    .map(|line| format!("  {line}")),
            );
        }
    }
    if differing > ACTIONS_SHOWN {
        details.push(format!(
            "... {} more differing action(s)",
            differing - ACTIONS_SHOWN
        ));
    }
    report.mismatch(
        Class::Plan,
        "the planned request differs from the snapshot's plan".to_owned(),
        details,
    );
}

/// Up to [`SHOWN`] items, joined.
fn shown(items: &[String]) -> String {
    let mut text = items
        .iter()
        .take(SHOWN)
        .cloned()
        .collect::<Vec<_>>()
        .join(", ");
    if items.len() > SHOWN {
        text.push_str(&format!(", ... ({} more)", items.len() - SHOWN));
    }
    text
}

/// Where two renderings first differ, with some context from each.
fn difference(old: &str, new: &str) -> String {
    let at = old
        .chars()
        .zip(new.chars())
        .take_while(|(a, b)| a == b)
        .count();
    let start = at.saturating_sub(60);
    let excerpt = |text: &str| -> String { text.chars().skip(start).take(240).collect() };
    format!(
        "snapshot …{}… | full analysis …{}…",
        excerpt(old),
        excerpt(new)
    )
}

/// A line diff of two texts: the common head and tail trimmed, the rest
/// as `-` (snapshot) and `+` (full analysis) lines, [`DIFF_LINES`] each.
fn line_diff(old: &str, new: &str) -> Vec<String> {
    let old: Vec<&str> = old.lines().collect();
    let new: Vec<&str> = new.lines().collect();
    let head = old.iter().zip(&new).take_while(|(a, b)| a == b).count();
    let tail = old[head..]
        .iter()
        .rev()
        .zip(new[head..].iter().rev())
        .take_while(|(a, b)| a == b)
        .count();
    let mut lines = vec![format!("@@ line {} @@", head + 1)];
    for (sign, side) in [("-", &old), ("+", &new)] {
        let changed = &side[head..side.len() - tail];
        for line in changed.iter().take(DIFF_LINES) {
            lines.push(format!("{sign} {}", printable(line)));
        }
        if changed.len() > DIFF_LINES {
            lines.push(format!(
                "{sign} ... ({} more lines)",
                changed.len() - DIFF_LINES
            ));
        }
    }
    lines
}

/// A line with control characters escaped (`\xNN`).
fn printable(line: &str) -> String {
    let mut out = String::with_capacity(line.len());
    for character in line.chars() {
        if character.is_control() && character != '\t' {
            out.push_str(&format!("\\x{:02x}", u32::from(character)));
        } else {
            out.push(character);
        }
    }
    out
}

// ---- what planning printed -------------------------------------------------

fn compare_window(snapshot: &[u8], window: Option<&Captured>, report: &mut Report) {
    let Some(window) = window else {
        report.mismatch(
            Class::Window,
            "the full analysis's stderr was not captured".to_owned(),
            Vec::new(),
        );
        return;
    };
    if window.ended_early {
        // L2-W5h: a caught panic, never a lingering child.
        report.mismatch(
            Class::Window,
            "the full analysis's stderr capture ended early (a caught panic?)".to_owned(),
            Vec::new(),
        );
    } else if !window.complete {
        report.mismatch(
            Class::Window,
            "a process kept the full analysis's captured stderr open (a lingering child?)"
                .to_owned(),
            Vec::new(),
        );
    }
    if window.overflowed {
        report.mismatch(
            Class::Window,
            format!(
                "the full analysis printed more than {} bytes",
                capture::LIMIT
            ),
            Vec::new(),
        );
    }
    if window.bytes != snapshot {
        let mut details = vec![format!(
            "{} bytes now, {} in the snapshot",
            window.bytes.len(),
            snapshot.len()
        )];
        details.extend(line_diff(
            &String::from_utf8_lossy(snapshot),
            &String::from_utf8_lossy(&window.bytes),
        ));
        report.mismatch(
            Class::Window,
            "what planning printed differs from the snapshot's window stderr".to_owned(),
            details,
        );
    }
}

// ---- the recording ---------------------------------------------------------

/// An observation's operation as the diagnostics name it.
fn op_text(op: &Op) -> String {
    match op {
        Op::ReadDir(projection) => format!("READDIR[{projection}]"),
        other => other.name().to_owned(),
    }
}

/// The outcome without what time changes (stat keys, racy flags).
fn same_outcome(old: &Outcome, new: &Outcome) -> bool {
    match (old, new) {
        (Outcome::Read { sha256: a, .. }, Outcome::Read { sha256: b, .. }) => a == b,
        (
            Outcome::Listing {
                digest: a,
                entries: x,
                ..
            },
            Outcome::Listing {
                digest: b,
                entries: y,
                ..
            },
        ) => a == b && x == y,
        (a, b) => a == b,
    }
}

/// Compare the recording of the full analysis (`now`, under the snapshot's
/// policy) with the snapshot's.
fn compare_recordings(snapshot: &Recording, now: &Recording, report: &mut Report) {
    let recorded: HashMap<(&[u8], &Op), &Outcome> = snapshot
        .observations
        .iter()
        .map(|(path, observation)| {
            (
                (path.as_os_str().as_bytes(), &observation.op),
                &observation.outcome,
            )
        })
        .collect();
    let mut seen: HashSet<(&[u8], &Op)> = HashSet::with_capacity(now.observations.len());
    let mut missing = Vec::new();
    let mut changed = Vec::new();
    let (mut first_missing, mut first_changed) = (None, None);
    for (path, observation) in &now.observations {
        let key = (path.as_os_str().as_bytes(), &observation.op);
        seen.insert(key);
        match recorded.get(&key) {
            None => {
                first_missing.get_or_insert(path.as_os_str());
                missing.push(format!(
                    "{} {}",
                    op_text(&observation.op),
                    escape(path.as_os_str())
                ));
            }
            Some(outcome) if !same_outcome(outcome, &observation.outcome) => {
                first_changed.get_or_insert(path.as_os_str());
                changed.push(format!(
                    "{} {}: snapshot {outcome}, now {}",
                    op_text(&observation.op),
                    escape(path.as_os_str()),
                    observation.outcome
                ));
            }
            Some(_) => {}
        }
    }
    // The log's first differing path, in the order of the differences.
    if let Some(path) = first_missing.or(first_changed) {
        report.path(path);
    }
    if !missing.is_empty() {
        report.mismatch(
            Class::Recording,
            format!(
                "incomplete recording: {} observation(s) recorded now but not in the snapshot",
                missing.len()
            ),
            missing.iter().take(SHOWN).cloned().collect(),
        );
    }
    if !changed.is_empty() {
        report.mismatch(
            Class::Recording,
            format!(
                "{} observation(s) recorded now with another outcome than the snapshot's",
                changed.len()
            ),
            changed.iter().take(SHOWN).cloned().collect(),
        );
    }
    let extra: Vec<String> = snapshot
        .observations
        .iter()
        .filter(|(path, observation)| {
            !seen.contains(&(path.as_os_str().as_bytes(), &observation.op))
        })
        .map(|(path, observation)| {
            format!("{} {}", op_text(&observation.op), escape(path.as_os_str()))
        })
        .collect();
    if !extra.is_empty() {
        report.notes.push(format!(
            "{} snapshot observation(s) the full analysis did not make (checked by every hit, harmless): {}",
            extra.len(),
            shown(&extra)
        ));
    }
    // Environment variables.
    let variables: BTreeMap<&std::ffi::OsStr, _> = snapshot
        .env
        .iter()
        .map(|(name, observation)| (name.as_os_str(), observation))
        .collect();
    let mut details = Vec::new();
    for (name, observation) in &now.env {
        match variables.get(name.as_os_str()) {
            None => details.push(format!(
                "ENV {} read now but not in the snapshot (incomplete recording)",
                escape(name)
            )),
            Some(recorded) if *recorded != observation => {
                details.push(format!("ENV {}: another value", escape(name)))
            }
            Some(_) => {}
        }
    }
    if !details.is_empty() {
        report.mismatch(
            Class::Env,
            "environment variables differ from the snapshot's".to_owned(),
            details,
        );
    }
    // Verified repository trees (their roots: the heads' stat keys may be
    // refreshed by any run).
    let roots: HashSet<&std::path::Path> = snapshot
        .repositories
        .iter()
        .map(|tree| tree.root.as_path())
        .collect();
    let new_trees: Vec<&crate::observed_fs::RepositoryRecord> = now
        .repositories
        .iter()
        .filter(|tree| !roots.contains(tree.root.as_path()))
        .collect();
    if let Some(tree) = new_trees.first() {
        report.path(tree.root.as_os_str());
    }
    let new_trees: Vec<String> = new_trees
        .into_iter()
        .map(|tree| format!("{} {}", tree.name, escape(tree.root.as_os_str())))
        .collect();
    if !new_trees.is_empty() {
        report.mismatch(
            Class::Recording,
            format!(
                "incomplete recording: {} repository tree(s) verified now but not in the snapshot",
                new_trees.len()
            ),
            new_trees.iter().take(SHOWN).cloned().collect(),
        );
    }
}

// ---- local repository rules ------------------------------------------------

/// The first eight bytes of a digest in hex.
fn short(digest: &[u8; 32]) -> String {
    observed_fs::hex(&digest[..8])
}

/// Compare the local repository rules the full analysis executed with the
/// snapshot's replay records (by canonical name).
fn compare_replays(snapshot: &[ReplayRecord], executed: &[ReplayRecord], report: &mut Report) {
    let records: BTreeMap<&str, &ReplayRecord> = snapshot
        .iter()
        .map(|record| (record.canonical.as_str(), record))
        .collect();
    for now in executed {
        let Some(old) = records.get(now.canonical.as_str()) else {
            report.path(now.root.as_os_str());
            report.mismatch(
                Class::Replay,
                format!(
                    "local repository {}: executed now but not replayed by the snapshot (incomplete recording)",
                    now.canonical
                ),
                Vec::new(),
            );
            continue;
        };
        let mut details = Vec::new();
        if old.execution.inputs_digest != now.execution.inputs_digest {
            details.push(format!(
                "the identity inputs differ (snapshot {}…, now {}…)",
                short(&old.execution.inputs_digest),
                short(&now.execution.inputs_digest)
            ));
        }
        if old.execution.snapshot_digest != now.execution.snapshot_digest {
            details.push(format!(
                "the repository tree differs (snapshot {}…, now {}…)",
                short(&old.execution.snapshot_digest),
                short(&now.execution.snapshot_digest)
            ));
        }
        if old.execution.identity != now.execution.identity {
            details.push(format!(
                "the identity differs (snapshot {}, now {})",
                old.execution.identity, now.execution.identity
            ));
        }
        if old.root != now.root {
            details.push(format!(
                "the published root differs (snapshot {}, now {})",
                escape(old.root.as_os_str()),
                escape(now.root.as_os_str())
            ));
        }
        if old.definition != now.definition
            || old.identity != now.identity
            || old.definition_label != now.definition_label
            || old.original_name != now.original_name
        {
            details.push(format!(
                "the definition differs (snapshot {} {}, now {} {})",
                old.definition, old.identity, now.definition, now.identity
            ));
        }
        if old.attributes != now.attributes {
            details.push("the bound attributes differ".to_owned());
        }
        if old.output != now.output {
            details.push(format!(
                "the printed output differs ({} bytes in the snapshot, {} now)",
                old.output.len(),
                now.output.len()
            ));
        }
        if old.environ_read != now.environ_read || old.facilities != now.facilities {
            details.push(format!(
                "the host facilities differ (snapshot {}, now {})",
                old.flags(),
                now.flags()
            ));
        }
        if !details.is_empty() {
            report.path(now.root.as_os_str());
            report.mismatch(
                Class::Replay,
                format!(
                    "local repository {}: its execution differs from the replay record",
                    now.canonical
                ),
                details,
            );
        }
    }
    let executed_names: HashSet<&str> = executed
        .iter()
        .map(|record| record.canonical.as_str())
        .collect();
    let extra: Vec<String> = snapshot
        .iter()
        .filter(|record| !executed_names.contains(record.canonical.as_str()))
        .map(|record| record.canonical.clone())
        .collect();
    if !extra.is_empty() {
        report.notes.push(format!(
            "{} local repository rule(s) the snapshot replays but the full analysis did not execute: {}",
            extra.len(),
            shown(&extra)
        ));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::observed_fs::{EnvObservation, FileKind, Observation, Projection, StatKey};
    use std::ffi::OsString;

    fn observation(path: &str, op: Op, outcome: Outcome) -> (PathBuf, Observation) {
        (PathBuf::from(path), Observation { op, outcome })
    }

    fn read(sha: u8, mtime: i64) -> Outcome {
        Outcome::Read {
            sha256: [sha; 32],
            key: StatKey {
                mtime_ns: mtime,
                ..StatKey::default()
            },
            racy: mtime == 0,
        }
    }

    fn recording(observations: Vec<(PathBuf, Observation)>) -> Recording {
        Recording {
            observations,
            ..Recording::default()
        }
    }

    #[test]
    fn recordings_compare_by_path_operation_and_outcome() {
        let snapshot = recording(vec![
            observation("/ws/BUILD", Op::Read, read(1, 0)),
            observation("/ws/pkg", Op::Stat, Outcome::Kind(FileKind::Dir)),
            observation("/ws/old", Op::Lstat, Outcome::Error(2)),
        ]);
        // The same observations, another stat key: agree (a note for the
        // observation the full analysis did not make).
        let now = recording(vec![
            observation("/ws/BUILD", Op::Read, read(1, 7)),
            observation("/ws/pkg", Op::Stat, Outcome::Kind(FileKind::Dir)),
        ]);
        let mut report = Report::default();
        compare_recordings(&snapshot, &now, &mut report);
        assert!(report.mismatches.is_empty(), "{:?}", report.mismatches);
        assert_eq!(report.notes.len(), 1);
        assert!(
            report.notes[0].contains("LSTAT /ws/old"),
            "{:?}",
            report.notes
        );
        // An observation the snapshot lacks (even of a known path), another
        // content, another listing projection: mismatches.
        let now = recording(vec![
            observation("/ws/BUILD", Op::Read, read(2, 7)),
            observation("/ws/pkg", Op::Stat, Outcome::Kind(FileKind::Dir)),
            observation("/ws/pkg", Op::ReadDir(Projection::Directories), {
                Outcome::Error(13)
            }),
            observation("/ws/old", Op::Lstat, Outcome::Error(2)),
        ]);
        let mut report = Report::default();
        compare_recordings(&snapshot, &now, &mut report);
        let titles: Vec<&str> = report
            .mismatches
            .iter()
            .map(|(_, title, _)| title.as_str())
            .collect();
        assert_eq!(titles.len(), 2, "{titles:?}");
        assert!(titles[0].starts_with("incomplete recording: 1 observation"));
        assert_eq!(report.mismatches[0].2, ["READDIR[dirs] /ws/pkg"]);
        assert!(report.mismatches[1].2[0].starts_with("READ /ws/BUILD: snapshot sha256:0101"));
        // The log's first differing path: the first difference's.
        assert_eq!(report.path.as_deref(), Some(OsStr::new("/ws/pkg")));
        // Environment: a variable read now only, another digest.
        let mut snapshot = recording(Vec::new());
        snapshot.env = vec![(
            OsString::from("A"),
            EnvObservation {
                digest: Some([1; 32]),
            },
        )];
        let mut now = snapshot.clone();
        now.env[0].1.digest = None;
        now.env
            .push((OsString::from("B"), EnvObservation { digest: None }));
        let mut report = Report::default();
        compare_recordings(&snapshot, &now, &mut report);
        assert_eq!(report.mismatches.len(), 1);
        assert_eq!(report.mismatches[0].0, Class::Env);
        assert_eq!(
            report.mismatches[0].2,
            [
                "ENV A: another value",
                "ENV B read now but not in the snapshot (incomplete recording)"
            ]
        );
    }

    /// The verify log (crate::snapshot::telemetry): one line per mismatch
    /// naming the first difference's class, the first taint and the first
    /// differing path (L2-W5g), appended.
    #[test]
    fn a_mismatch_appends_its_log_line() {
        let temp = tempfile::tempdir().unwrap();
        let context = Context {
            command: "build".to_owned(),
            targets: vec!["//a:b".to_owned()],
            explain: false,
        };
        let stored = (
            temp.path().to_path_buf(),
            Key {
                g: [1; 32],
                binary: [2; 32],
            },
        );
        let mut report = Report {
            taint: Some("repository-rule-executed"),
            ..Report::default()
        };
        report.mismatch(Class::Taint, "taint".to_owned(), Vec::new());
        report.path(OsStr::new("/c/repositories/direct/trees/a b"));
        report.path(OsStr::new("/second"));
        report.conclude("0011", Some(&stored), false, &context, false);
        let mut report = Report::default();
        report.mismatch(Class::Window, "window".to_owned(), Vec::new());
        report.conclude("0022", Some(&stored), false, &context, false);
        let text = std::fs::read_to_string(temp.path().join("snapshots").join(LOG)).unwrap();
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 2, "{text}");
        // L2-W5h: the event log counts each as a mismatch of verify mode.
        let events = std::fs::read_to_string(
            temp.path()
                .join("snapshots")
                .join(crate::snapshot::telemetry::EVENTS_LOG),
        )
        .unwrap();
        let events: Vec<&str> = events.lines().collect();
        assert_eq!(events.len(), 2, "{events:?}");
        assert!(
            events[0].contains(" kind=mismatch mode=verify key=0011 command=build reason=taint path=/c/repositories/direct/trees/a\\x20b ms=- detail=differences:1,taint:repository-rule-executed targets=//a:b"),
            "{events:?}"
        );
        assert!(
            events[1]
                .contains(" kind=mismatch mode=verify key=0022 command=build reason=window path=-"),
            "{events:?}"
        );
        assert!(
            lines[0].ends_with(" key=0011 command=build first=taint differences=1 taint=repository-rule-executed path=/c/repositories/direct/trees/a\\x20b targets=//a:b"),
            "{text}"
        );
        assert!(
            lines[1].ends_with(
                " key=0022 command=build first=window differences=1 taint=- path=- targets=//a:b"
            ),
            "{text}"
        );
        // L2-W5i: every difference to verify-details.log, under its
        // verify.log line, with where the snapshot went.
        let details = std::fs::read_to_string(
            temp.path()
                .join("snapshots")
                .join(crate::snapshot::telemetry::VERIFY_DETAILS_LOG),
        )
        .unwrap();
        assert_eq!(
            details,
            format!(
                "{}\n  snapshot: kept\n  verify mismatch: taint\n{}\n  snapshot: kept\n  verify mismatch: window\n",
                lines[0], lines[1]
            )
        );
    }

    /// L2-W5i: a mismatch sets the snapshot aside — moved to
    /// `<state>/snapshots/mismatched/`, the lookup misses — and its
    /// verify-details.log block holds every difference (a detail with
    /// newlines and control characters stays inside the block) and where
    /// the snapshot went.
    #[test]
    fn a_mismatch_sets_the_snapshot_aside_with_its_details() {
        let temp = tempfile::tempdir().unwrap();
        let key = Key {
            g: [3; 32],
            binary: [4; 32],
        };
        crate::snapshot::store::write_in_background(
            temp.path(),
            &key,
            vec![(crate::snapshot::format::Section::Plan, b"plan".to_vec())],
        )
        .commit()
        .unwrap();
        let stored = (temp.path().to_path_buf(), key);
        let mut report = Report::default();
        report.mismatch(
            Class::Plan,
            "the planned request differs from the snapshot's plan".to_owned(),
            vec![
                "encoded: 10 bytes now, 12 in the snapshot".to_owned(),
                "action #3 (a in the snapshot, b now):\n  @@ line 2 @@\n  - x\u{1b}".to_owned(),
            ],
        );
        report
            .notes
            .push("1 snapshot observation(s) ...".to_owned());
        let context = Context {
            command: "test".to_owned(),
            targets: vec!["//t:t".to_owned()],
            explain: false,
        };
        report.conclude(&key.short(), Some(&stored), true, &context, false);
        assert!(
            crate::snapshot::store::Reader::open(temp.path(), &key)
                .unwrap()
                .is_none()
        );
        let aside: Vec<PathBuf> =
            std::fs::read_dir(crate::snapshot::store::mismatched_directory(temp.path()))
                .unwrap()
                .flatten()
                .map(|entry| entry.path())
                .collect();
        assert_eq!(aside.len(), 1, "{aside:?}");
        assert!(std::fs::read(&aside[0]).unwrap().ends_with(b"plan"));
        let verify_line = std::fs::read_to_string(temp.path().join("snapshots").join(LOG)).unwrap();
        let details = std::fs::read_to_string(
            temp.path()
                .join("snapshots")
                .join(crate::snapshot::telemetry::VERIFY_DETAILS_LOG),
        )
        .unwrap();
        assert_eq!(
            details,
            format!(
                "{verify_line}  snapshot: set aside as {}\n  verify mismatch: the planned request differs from the snapshot's plan\n  verify   encoded: 10 bytes now, 12 in the snapshot\n  verify   action #3 (a in the snapshot, b now):\n    @@ line 2 @@\n    - x\\x1b\n  verify note: 1 snapshot observation(s) ...\n",
                aside[0].display()
            )
        );
    }

    #[test]
    fn line_diffs_show_the_differing_middle() {
        let diff = line_diff("a\nb\nc\nd\n", "a\nB\nC\nd\n");
        assert_eq!(diff, ["@@ line 2 @@", "- b", "- c", "+ B", "+ C"]);
        let diff = line_diff("same\n", "same\nextra \u{1b}[0m\n");
        assert_eq!(diff, ["@@ line 2 @@", "+ extra \\x1b[0m"]);
        assert!(
            difference("abcdef", "abcxef").contains("snapshot …abcdef… | full analysis …abcxef…")
        );
    }
}
