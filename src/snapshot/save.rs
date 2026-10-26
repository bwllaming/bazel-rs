//! Saving a snapshot from a full analysis (PLAN-L2 §1b/§1d; `build`,
//! `test` and `run` with `BAZEL_RS_SNAPSHOT=on` after a miss).
//!
//! At the snapshot check point the session opens the recording window
//! (crate::observed_fs), installs the replay capture of local repository
//! rules (crate::snapshot::replay) and redirects stderr into a
//! [`Mode::Tee`](crate::snapshot::capture::Mode::Tee) capture: what planning
//! prints still reaches stderr as it happens, and is kept. When planning
//! returns ([`Save::end_window`]) the capture stops; with the planned
//! request ([`Save::planned`]) the session decides whether the run may be
//! saved — no taint (crate::observed_fs::Taint, decided on the plan for
//! build-info and `map_each`), no pending lockfile write, no analysis
//! error, a complete capture, an encodable plan below the size cap — and
//! encodes the sections on the main thread; a background thread writes them
//! to a temporary file while the actions execute. Only when the command (for
//! `test` and `run`: its build part) succeeded does [`Save::finish`] rename
//! it into place (crate::snapshot::store). Failures are never saved. A save
//! appends a `kind=saved` line to `<state>/snapshots/events.log`, a run that
//! is not saved its reason (`kind=not-saved`, crate::snapshot::telemetry;
//! L2-W5g, L2-W5h).

use crate::observed_fs::{self, Taint};
use crate::registry_session::SessionHandle;
use crate::request::PlannedRequest;
use crate::snapshot::capture::{self, Capture, Captured};
use crate::snapshot::format::{self, Section, Stats};
use crate::snapshot::key::Key;
use crate::snapshot::store;
use crate::snapshot::telemetry;
use crate::snapshot::validate::Policy;
use std::path::PathBuf;
use std::time::{Duration, Instant};

/// How long the end of the window waits for the capture's reader to see
/// the end of the pipe (a lingering child process keeps it open).
const CAPTURE_BOUND: Duration = Duration::from_millis(500);

/// The full-analysis path's snapshot session.
pub struct Save {
    key: Key,
    cache: PathBuf,
    explain: bool,
    capture: Option<Capture>,
    window: Option<Captured>,
    registry: Option<SessionHandle>,
    pending: Option<store::Pending>,
    /// Why the run is not saved, once decided.
    skipped: Option<String>,
    started: Instant,
    saved_note: String,
    /// The event log's counts of a save (`actions:<n>,window:<bytes>`).
    saved_counts: String,
    finished: bool,
    /// The event log's context (a save and a run not saved are logged).
    log: telemetry::Context,
}

/// Start recording at the snapshot check point (`log`: the cache, mode,
/// command and target patterns of the event log).
pub fn start(key: Key, explain: bool, log: telemetry::Context) -> Save {
    let mut save = Save {
        key,
        cache: log.cache.clone(),
        explain,
        capture: None,
        window: None,
        registry: None,
        pending: None,
        skipped: None,
        started: Instant::now(),
        saved_note: String::new(),
        saved_counts: String::new(),
        finished: false,
        log,
    };
    if !observed_fs::open_window() {
        save.skipped = Some("the recording window was already open".to_owned());
        return save;
    }
    crate::snapshot::replay::install();
    match capture::start(capture::Mode::Tee) {
        Ok(capture) => save.capture = Some(capture),
        Err(error) => save.skipped = Some(format!("cannot capture stderr: {error}")),
    }
    save
}

impl Save {
    /// The registry session whose pending lockfile writes keep the run from
    /// being saved.
    pub fn set_registry_session(&mut self, registry: SessionHandle) {
        self.registry = Some(registry);
    }

    /// Planning returned (or failed): close the recording window and stop
    /// the capture before anything after planning prints.
    pub fn end_window(&mut self) {
        observed_fs::close_window();
        if let Some(capture) = self.capture.take() {
            self.window = Some(capture.stop(CAPTURE_BOUND));
        }
    }

    fn skip(&mut self, reason: String) {
        if self.skipped.is_none() {
            self.skipped = Some(reason);
        }
    }

    /// The planned request of a successful planning (after the hand-over):
    /// decide, encode the plan, and write in the background — where the
    /// recording is also taken, selected and encoded. `workspace_status`:
    /// planning computed the workspace status (a hit computes it too,
    /// crate::snapshot::format::Inputs::workspace_status).
    pub fn planned(&mut self, request: &PlannedRequest, workspace_status: bool) {
        self.end_window();
        if self.skipped.is_some() {
            return;
        }
        match self.prepare(request, workspace_status) {
            Ok(pending) => self.pending = Some(pending),
            Err(reason) => self.skip(reason),
        }
    }

    fn prepare(
        &mut self,
        request: &PlannedRequest,
        workspace_status: bool,
    ) -> Result<store::Pending, String> {
        let started = Instant::now();
        let window = self
            .window
            .take()
            .ok_or("planning printed outside the capture")?;
        // An early end first: its capture is incomplete too, but no child
        // holds the pipe (L2-W5h: a caught panic was reported as one).
        if window.ended_early || capture::broken() {
            return Err("the stderr capture ended early (a caught panic?)".to_owned());
        }
        if !window.complete {
            return Err(
                "a process kept the captured stderr open after planning (a lingering child?)"
                    .to_owned(),
            );
        }
        if window.overflowed {
            return Err(format!(
                "planning printed more than {} bytes",
                capture::LIMIT
            ));
        }
        if let Some(session) = self.registry.as_ref().and_then(SessionHandle::session) {
            if session.updated_lockfile().is_some() {
                return Err("taint lockfile-write-pending: MODULE.bazel.lock".to_owned());
            }
            if let Some((path, _)) = session.updated_hidden_lockfile() {
                return Err(format!(
                    "taint lockfile-write-pending: hidden lockfile {}",
                    path.display()
                ));
            }
        }
        if !request.errors.is_empty() {
            return Err(format!(
                "taint errors: {} analysis error(s)",
                request.errors.len()
            ));
        }
        let recorder = observed_fs::recorder().ok_or("nothing was recorded")?;
        let taints_now = recorder.taints();
        if !taints_now.is_empty() {
            return Err(taints(&taints_now));
        }
        let capture = crate::snapshot::replay::global();
        let records = capture
            .as_ref()
            .map(|capture| capture.records())
            .unwrap_or_default();
        let tables = capture.as_ref().and_then(|capture| capture.tables());
        if !records.is_empty() && tables.is_none() {
            return Err("local repository replay records without pinned tables".to_owned());
        }
        let checked_at = started.elapsed();
        let plan = crate::snapshot::codec::encode(request).map_err(|error| error.to_string())?;
        let plan_at = started.elapsed();
        let events = format::encode_events(&window.bytes);
        let plan_nodes = request.nodes.len() as u64;
        let window_stderr = window.bytes.len() as u64;
        let ms = |duration: Duration| duration.as_secs_f64() * 1000.0;
        self.saved_note = format!(
            "{plan_nodes} actions, PLAN {} bytes, window stderr {window_stderr} bytes; {:.1} ms before execution (checks {:.1}, plan {:.1})",
            plan.len(),
            ms(started.elapsed()),
            ms(checked_at),
            ms(plan_at - checked_at),
        );
        // L2-W5i: the binary's identity too (its first 8 bytes), so the soak
        // report sees a bazel-rs upgrade (every key moves with the binary).
        self.saved_counts = format!(
            "actions:{plan_nodes},replays:{},binary:{}",
            records.len(),
            observed_fs::hex(&self.key.binary[..8])
        );
        // The recording is no input of execution: taken, selected and
        // encoded on the writer thread while the actions run.
        Ok(store::write_with(&self.cache, &self.key, move || {
            let started = Instant::now();
            let recording = recorder.take();
            let policy = Policy::current();
            let window_us = recording
                .window
                .map_or(0, |window| window.as_micros() as u64);
            let observations = recording.observations.len();
            let integrity: usize = recording
                .repositories
                .iter()
                .map(|repository| repository.entries.len())
                .sum();
            let selected = policy.select_owned(recording);
            let inputs =
                format::encode_inputs(&selected, &records, tables.as_ref(), workspace_status);
            let stats = Stats {
                observations: observations as u64,
                validated: selected.observations.len() as u64,
                env: selected.env.len() as u64,
                trees: selected.repositories.len() as u64,
                integrity_entries: integrity as u64,
                replay_records: records.len() as u64,
                window_stderr,
                window_us,
                plan_nodes,
                plan_bytes: plan.len() as u64,
                inputs_bytes: inputs.len() as u64,
                encode_us: started.elapsed().as_micros() as u64,
                full_validation: policy == Policy::Full,
            };
            let inputs_ms = ms(started.elapsed());
            // P3: the plan is stored compressed, here on the writer while
            // the actions execute.
            let compressing = Instant::now();
            let stored = format::compress_plan(&plan);
            let note = format!(
                "{} observations validated of {}, {} trees, {} replayed local repositories; INPUTS {} bytes encoded in {:.1} ms on the writer; PLAN compressed to {} bytes in {:.1} ms",
                stats.validated,
                stats.observations,
                stats.trees,
                stats.replay_records,
                stats.inputs_bytes,
                inputs_ms,
                stored.len(),
                ms(compressing.elapsed()),
            );
            drop(plan);
            Ok((
                vec![
                    (Section::Inputs, inputs),
                    (Section::Events, events),
                    (Section::Plan, stored),
                    (Section::Stats, format::encode_stats(&stats)),
                ],
                note,
            ))
        }))
    }

    /// The command (for `test` and `run`: its build part) succeeded: put the
    /// snapshot in place.
    pub fn finish(mut self) {
        self.end_window();
        self.finished = true;
        let pending = self.pending.take();
        let outcome = match (pending, self.skipped.take()) {
            (Some(pending), _) => match pending.logged(self.log.clone()).commit() {
                Ok((bytes, note)) => {
                    let ms = self.started.elapsed().as_secs_f64() * 1000.0;
                    self.log.log_saved(
                        &self.key.short(),
                        ms,
                        &format!("bytes:{bytes},{}", self.saved_counts),
                    );
                    Ok(format!(
                        "saved {} ({bytes} bytes; {}; {note}; {ms:.1} ms after the check point)",
                        self.key.short(),
                        self.saved_note,
                    ))
                }
                Err(error) => Err(error.to_string()),
            },
            (None, Some(reason)) => Err(reason),
            (None, None) => Err("planning did not complete".to_owned()),
        };
        match outcome {
            Ok(saved) => {
                if self.explain {
                    observed_fs::diag(&saved);
                }
            }
            Err(reason) => {
                self.log.log_not_saved(&self.key.short(), &reason);
                if self.explain {
                    observed_fs::diag(&format!("not saved: {reason}"));
                }
            }
        }
    }
}

impl Drop for Save {
    /// The command failed (or ended without [`Save::finish`]): nothing is
    /// saved; a temporary file is removed (crate::snapshot::store::Pending).
    fn drop(&mut self) {
        self.end_window();
        if self.finished {
            return;
        }
        let reason = match &self.skipped {
            Some(reason) => format!("the command failed ({reason})"),
            None => "the command failed".to_owned(),
        };
        self.log.log_not_saved(&self.key.short(), &reason);
        if self.explain {
            observed_fs::diag(&format!("not saved: {reason}"));
        }
    }
}

/// The taints of a recording, for EXPLAIN (the first details of each).
pub(crate) fn taints(
    taints: &std::collections::BTreeMap<Taint, observed_fs::TaintRecord>,
) -> String {
    taints
        .iter()
        .map(|(taint, record)| {
            format!(
                "taint {} ({}): {}",
                taint.name(),
                record.count,
                record
                    .details
                    .iter()
                    .take(3)
                    .cloned()
                    .collect::<Vec<_>>()
                    .join(" | ")
            )
        })
        .collect::<Vec<_>>()
        .join("; ")
}
