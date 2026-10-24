//! `BAZEL_RS_SNAPSHOT=verify` (PLAN-L2 §1b layer 3 "shadow verify", PLAN-L2
//! update 1 row W6): every snapshot hit also runs the full analysis, in its
//! shadow, and compares the two before the hit executes.
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
//!      mismatch: every difference on the diagnostics stream, the snapshot
//!      removed (the next run analyzes and saves anew, so a persistent
//!      cause shows on every other run), nothing saved, nothing executed,
//!      exit 70.
//! ```
//!
//! A miss behaves exactly like `on` (the full analysis is saved), so a
//! verify session can run any sequence of commands. Observations the
//! snapshot holds but the full analysis did not make are reported (EXPLAIN)
//! but do not fail: they only cost a hit extra checks. A full analysis that
//! ends before planning completed (an error the hit did not reproduce) is
//! a mismatch too ([`exit_if_abandoned`], from main).

use crate::observed_fs::{self, Op, Outcome, Recording, escape};
use crate::registry_session::SessionHandle;
use crate::repo_env::EnvVar;
use crate::request::{Diagnostics, PlannedRequest};
use crate::snapshot::capture::{self, Capture, Captured};
use crate::snapshot::hit::{Evidence, Hit};
use crate::snapshot::key::Key;
use crate::snapshot::replay::ReplayRecord;
use crate::snapshot::validate::Policy;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::os::unix::ffi::OsStrExt;
use std::path::PathBuf;
use std::sync::{Mutex, PoisonError};
use std::time::{Duration, Instant};

/// The exit code of a mismatch (EX_SOFTWARE).
pub const EXIT_MISMATCH: i32 = 70;
/// How long the end of the shadow window waits for the capture's reader.
const CAPTURE_BOUND: Duration = Duration::from_millis(500);
/// Items shown per kind of difference.
const SHOWN: usize = 10;
/// Differing actions shown with their line diff.
const ACTIONS_SHOWN: usize = 3;
/// Lines shown per side of a line diff.
const DIFF_LINES: usize = 12;

/// The shadow full analysis of one hit.
pub struct Verify {
    hit: Option<Box<Hit>>,
    capture: Option<Capture>,
    window: Option<Captured>,
    registry: Option<SessionHandle>,
    started: Instant,
    explain: bool,
    concluded: bool,
}

/// Start the shadow full analysis of `hit` at the snapshot check point:
/// open the recording window and the replay capture, capture stderr. A
/// session that cannot start is a verification failure (exit 70).
pub fn start(hit: Box<Hit>, repo_env: &[EnvVar], explain: bool) -> Box<Verify> {
    let key = hit.session.key().short();
    let mut verify = Box::new(Verify {
        hit: Some(hit),
        capture: None,
        window: None,
        registry: None,
        started: Instant::now(),
        explain,
        concluded: false,
    });
    let failure = if verify
        .hit
        .as_ref()
        .is_some_and(|hit| hit.evidence.is_none())
    {
        Some("the hit kept nothing to compare".to_owned())
    } else if !observed_fs::open_window(crate::snapshot::save::fixed_names(repo_env)) {
        Some("the recording window was already open".to_owned())
    } else {
        crate::snapshot::replay::install();
        match capture::start(capture::Mode::Shadow) {
            Ok(capture) => {
                verify.capture = Some(capture);
                None
            }
            Err(error) => Some(format!("cannot capture stderr: {error}")),
        }
    };
    if let Some(failure) = failure {
        verify.concluded = true;
        let mut report = Report::default();
        report.mismatch(format!("cannot verify: {failure}"), Vec::new());
        report.exit(&key, None);
    }
    verify
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
    /// every difference is printed on the diagnostics stream and the process
    /// exits with [`EXIT_MISMATCH`].
    pub fn conclude(
        mut self,
        full: &Result<PlannedRequest, String>,
        failed: Option<&Diagnostics>,
    ) -> Box<Hit> {
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
                    "the full analysis failed where the snapshot planned".to_owned(),
                    details,
                );
            }
        }
        compare_window(&hit.window_stderr, self.window.as_ref(), &mut report);
        let recording = observed_fs::recorder().map(|recorder| recorder.take());
        let observed = match recording {
            Some(recording) => {
                if !recording.taints.is_empty() {
                    report.mismatch(
                        "the full analysis would not be saved".to_owned(),
                        vec![crate::snapshot::save::taints(&recording.taints)],
                    );
                }
                let recording = Policy::current().select_owned(recording);
                compare_recordings(&inputs.recording, &recording, &mut report);
                recording.observations.len()
            }
            None => {
                report.mismatch("the full analysis recorded nothing".to_owned(), Vec::new());
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
                    "the full analysis would write MODULE.bazel.lock".to_owned(),
                    Vec::new(),
                );
            }
            if let Some((path, _)) = session.updated_hidden_lockfile() {
                report.mismatch(
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
            let store = (hit.session.cache().to_path_buf(), *hit.session.key());
            report.exit(&key, Some(&store));
        }
        let ms = |duration: Duration| duration.as_secs_f64() * 1000.0;
        if self.explain {
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
        hit
    }
}

/// A snapshot in a cache's store.
type Stored = (PathBuf, Key);

/// A verify session that ended without [`Verify::conclude`]: its snapshot
/// (main reports it, [`exit_if_abandoned`]).
static ABANDONED: Mutex<Option<Stored>> = Mutex::new(None);

impl Drop for Verify {
    fn drop(&mut self) {
        if self.concluded {
            return;
        }
        self.end_window();
        if let Some(hit) = &self.hit {
            *ABANDONED.lock().unwrap_or_else(PoisonError::into_inner) =
                Some((hit.session.cache().to_path_buf(), *hit.session.key()));
        }
    }
}

/// main, when the command returned: a verify session whose full analysis
/// never completed planning (the command ended first, with `error` if it
/// failed) is a mismatch — the snapshot planned where the full analysis did
/// not. Prints it and exits with [`EXIT_MISMATCH`].
pub fn exit_if_abandoned(error: Option<&str>) {
    let Some(stored) = ABANDONED
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .take()
    else {
        return;
    };
    let mut report = Report::default();
    report.mismatch(
        "the full analysis ended before planning completed".to_owned(),
        vec![match error {
            Some(error) => format!("error: {error}"),
            None => "without an error".to_owned(),
        }],
    );
    report.exit(&stored.1.short(), Some(&stored));
}

/// The differences found (each with its detail lines) and the notes.
#[derive(Default)]
struct Report {
    mismatches: Vec<(String, Vec<String>)>,
    notes: Vec<String>,
}

impl Report {
    fn mismatch(&mut self, title: String, details: Vec<String>) {
        self.mismatches.push((title, details));
    }

    /// Print every difference on the diagnostics stream, remove the
    /// snapshot (`stored`) and exit.
    fn exit(self, key: &str, stored: Option<&Stored>) -> ! {
        let removed =
            stored.is_some_and(|(cache, key)| crate::snapshot::store::remove(cache, key).is_ok());
        observed_fs::diag(&format!(
            "verify {key}: MISMATCH: the full analysis disagrees with the snapshot hit ({} difference(s)); exit {EXIT_MISMATCH}, nothing executed or saved{}",
            self.mismatches.len(),
            if removed {
                "; the snapshot is removed (the next run analyzes and saves anew)"
            } else {
                ""
            }
        ));
        for (title, details) in &self.mismatches {
            observed_fs::diag(&format!("verify mismatch: {title}"));
            for detail in details {
                observed_fs::diag(&format!("verify   {detail}"));
            }
        }
        for note in &self.notes {
            observed_fs::diag(&format!("verify note: {note}"));
        }
        crate::digest_memo::finish();
        std::process::exit(EXIT_MISMATCH);
    }
}

// ---- the planned request ---------------------------------------------------

/// The outline of a request, field by field (every field by name: a new
/// one is a compile error here).
fn outline(request: &PlannedRequest) -> Vec<(&'static str, String)> {
    let PlannedRequest {
        command,
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
    for (index, (old, new)) in snapshot.nodes.iter().zip(&full.nodes).enumerate() {
        let same = old.action.id == new.action.id
            && postcard::to_stdvec(old).ok() == postcard::to_stdvec(new).ok();
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
            "the full analysis's stderr was not captured".to_owned(),
            Vec::new(),
        );
        return;
    };
    if !window.complete {
        report.mismatch(
            "a process kept the full analysis's captured stderr open (a lingering child?)"
                .to_owned(),
            Vec::new(),
        );
    }
    if window.overflowed {
        report.mismatch(
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
    for (path, observation) in &now.observations {
        let key = (path.as_os_str().as_bytes(), &observation.op);
        seen.insert(key);
        match recorded.get(&key) {
            None => missing.push(format!(
                "{} {}",
                op_text(&observation.op),
                escape(path.as_os_str())
            )),
            Some(outcome) if !same_outcome(outcome, &observation.outcome) => changed.push(format!(
                "{} {}: snapshot {outcome}, now {}",
                op_text(&observation.op),
                escape(path.as_os_str()),
                observation.outcome
            )),
            Some(_) => {}
        }
    }
    if !missing.is_empty() {
        report.mismatch(
            format!(
                "incomplete recording: {} observation(s) recorded now but not in the snapshot",
                missing.len()
            ),
            missing.iter().take(SHOWN).cloned().collect(),
        );
    }
    if !changed.is_empty() {
        report.mismatch(
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
                details.push(format!("ENV {}: another value or origin", escape(name)))
            }
            Some(_) => {}
        }
    }
    if !details.is_empty() {
        report.mismatch(
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
    let new_trees: Vec<String> = now
        .repositories
        .iter()
        .filter(|tree| !roots.contains(tree.root.as_path()))
        .map(|tree| format!("{} {}", tree.name, escape(tree.root.as_os_str())))
        .collect();
    if !new_trees.is_empty() {
        report.mismatch(
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
            report.mismatch(
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
            report.mismatch(
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
        let titles: Vec<&str> = report.mismatches.iter().map(|(t, _)| t.as_str()).collect();
        assert_eq!(titles.len(), 2, "{titles:?}");
        assert!(titles[0].starts_with("incomplete recording: 1 observation"));
        assert_eq!(report.mismatches[0].1, ["READDIR[dirs] /ws/pkg"]);
        assert!(report.mismatches[1].1[0].starts_with("READ /ws/BUILD: snapshot sha256:0101"));
        // Environment: a variable read now only, another digest.
        let mut snapshot = recording(Vec::new());
        snapshot.env = vec![(
            OsString::from("A"),
            EnvObservation {
                digest: Some([1; 32]),
                fixed: false,
            },
        )];
        let mut now = snapshot.clone();
        now.env[0].1.digest = None;
        now.env.push((
            OsString::from("B"),
            EnvObservation {
                digest: None,
                fixed: false,
            },
        ));
        let mut report = Report::default();
        compare_recordings(&snapshot, &now, &mut report);
        assert_eq!(report.mismatches.len(), 1);
        assert_eq!(
            report.mismatches[0].1,
            [
                "ENV A: another value or origin",
                "ENV B read now but not in the snapshot (incomplete recording)"
            ]
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
