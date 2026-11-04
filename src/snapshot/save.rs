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
//! to a temporary file while the actions execute. When the command (for
//! `test` and `run`: its build part) succeeded — or failed only in
//! execution, an action failing after a complete analysis (SURVEY3 S2,
//! crate::snapshot::Scope::execution_failed) — [`Save::finish`] renames it
//! into place (crate::snapshot::store). Analysis failures (also a
//! `--keep_going` plan that skipped some) are never saved. A save
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
    /// Incremental analysis (crate::snapshot::incremental): the previous
    /// snapshot's parts this save merges with what the partial analysis
    /// recorded.
    seed: Option<Box<Seed>>,
    /// The family key of a `build` or `test` (crate::snapshot::derive): a
    /// save with a DERIVE section is named there.
    family: Option<[u8; 32]>,
    /// The save holds a DERIVE section.
    derivable: bool,
    /// The snapshot is never looked up by its key, only derived from
    /// (`bazel-rs affected`'s scoped planning): its window's stderr is never
    /// replayed, so output the next run would not print does not keep it
    /// from being saved ([`Taint::ExtensionOutput`]).
    derive_only: bool,
}

/// What an incremental analysis's save takes from the snapshot it started
/// from (crate::snapshot::incremental).
pub struct Seed {
    /// The previous INPUTS: the observations it did not make again are
    /// still valid (the lookup validated them) and are kept; what it
    /// observed now replaces them.
    pub inputs: format::Inputs,
    /// What the full analysis prints while planning (the previous window:
    /// the partial analysis's own bytes were kept apart).
    pub window: Vec<u8>,
    /// The merged dependency index.
    pub index: crate::snapshot::incremental::Index,
}

/// Start recording at the snapshot check point (`log`: the cache, mode,
/// command and target patterns of the event log).
pub fn start(key: Key, explain: bool, log: telemetry::Context) -> Save {
    start_with(key, explain, log, capture::Mode::Tee)
}

/// [`start`] with the stderr capture `mode`: [`capture::Mode::Shadow`] for
/// an incremental analysis, whose partial analysis prints nothing (the
/// previous window is printed instead, or the run falls back).
pub fn start_with(key: Key, explain: bool, log: telemetry::Context, mode: capture::Mode) -> Save {
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
        seed: None,
        family: None,
        derivable: false,
        derive_only: false,
    };
    if !observed_fs::open_window() {
        save.skipped = Some("the recording window was already open".to_owned());
        return save;
    }
    crate::snapshot::replay::install();
    match capture::start(mode) {
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

    /// The family key of this command (crate::snapshot::derive).
    pub fn set_family(&mut self, family: Option<[u8; 32]>) {
        self.family = family;
    }

    /// The snapshot is never looked up by its key, only derived from
    /// (`bazel-rs affected`'s scoped planning).
    pub fn set_derive_only(&mut self) {
        self.derive_only = true;
    }

    /// This command's key.
    pub fn key(&self) -> Key {
        self.key
    }

    /// The event log's context of this command.
    pub fn log(&self) -> telemetry::Context {
        self.log.clone()
    }

    /// The family key of this command, if derived hits are recorded.
    pub fn family(&self) -> Option<[u8; 32]> {
        self.family
    }

    /// Give up this save (a derived hit serves the command): close the
    /// window, stop the capture and log why nothing is saved.
    pub fn abandon(mut self, reason: &str) {
        self.end_window();
        self.finished = true;
        self.log.log_not_saved(&self.key.short(), reason);
        if self.explain {
            observed_fs::diag(&format!("not saved: {reason}"));
        }
    }

    /// Merge this save with `seed` (an incremental analysis).
    pub fn seed(&mut self, seed: Seed) {
        self.seed = Some(Box::new(seed));
    }

    /// Stop the capture now and return what it kept (an incremental
    /// analysis that falls back: the bytes are not printed).
    pub fn stop_capture(&mut self) -> Option<Captured> {
        observed_fs::close_window();
        self.capture
            .take()
            .map(|capture| capture.stop(CAPTURE_BOUND))
    }

    /// The bytes the capture kept so far (the window is closed and the
    /// capture stopped).
    pub fn window(&mut self) -> Option<&Captured> {
        self.end_window();
        self.window.as_ref()
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
        let mut window = self
            .window
            .take()
            .ok_or("planning printed outside the capture")?;
        let seed = self.seed.take();
        if let Some(seed) = &seed {
            // The partial analysis printed into a shadow capture; the
            // window is the full analysis's (crate::snapshot::incremental
            // checked that it prints the same).
            window.bytes = seed.window.clone();
        }
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
            // The hidden lockfile is recorded by the entries this command
            // resolved, as it will stand once written: its pending write
            // keeps the run saveable, and another command's write of other
            // extensions' entries keeps the snapshot valid.
            session.note_hidden_lockfile();
        }
        if !request.errors.is_empty() {
            return Err(format!(
                "taint errors: {} analysis error(s)",
                request.errors.len()
            ));
        }
        // `--keep_going`: a plan that skipped what failed to analyze (or
        // whose inputs are missing) is incomplete; only a complete plan is
        // saved, also when its execution then fails (crate::snapshot::Scope
        // ::execution_failed).
        if let Some(plan) = &request.keep_going
            && (!plan.errors.is_empty()
                || !plan.failed.is_empty()
                || !plan.input_errors.is_empty()
                || !plan.unbuilt.is_empty())
        {
            return Err(format!(
                "taint errors: --keep_going analysis failures ({} target(s), {} input error(s))",
                plan.failed.len() + plan.unbuilt.len(),
                plan.input_errors.len()
            ));
        }
        let recorder = observed_fs::recorder().ok_or("nothing was recorded")?;
        let mut taints_now = recorder.taints();
        if self.derive_only {
            taints_now.remove(&Taint::ExtensionOutput);
        }
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
        let index = match &seed {
            Some(seed) => Some(seed.index.clone()),
            None => crate::snapshot::incremental::take_planned_index(),
        };
        // An incremental save's plan was spliced: no derivation record.
        let derive = crate::snapshot::derive::take_planned()
            .filter(|_| seed.is_none() && self.family.is_some());
        self.derivable = derive.is_some();
        let checked_at = started.elapsed();
        // The plan's parts are compressed as they are encoded, on the
        // encoding threads: a plan of GBs is never held uncompressed while
        // the planned request and the analysis are still in memory (MEM4).
        let compressing = Instant::now();
        let (plan, plan_stats) =
            crate::snapshot::codec::encode_parts_with(request, format::compress_part)
                .map_err(|error| error.to_string())?;
        let plan_len = plan_stats.bytes;
        let compressed_ms = compressing.elapsed().as_secs_f64() * 1000.0;
        let plan_at = started.elapsed();
        let events = format::encode_events(&window.bytes);
        let plan_nodes = request.nodes.len() as u64;
        let window_stderr = window.bytes.len() as u64;
        let ms = |duration: Duration| duration.as_secs_f64() * 1000.0;
        self.saved_note = format!(
            "{plan_nodes} actions, PLAN {} bytes, window stderr {window_stderr} bytes; {:.1} ms before execution (checks {:.1}, plan {:.1})",
            plan_len,
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
            // An incremental save merges two recordings the policy selected
            // already (selecting again would drop what lies below the old
            // trees' links: their selected entries keep no link).
            let merged = seed.is_some();
            let (recording, records, tables, workspace_status) = match seed {
                None => (recording, records, tables, workspace_status),
                Some(seed) => {
                    let seed = *seed;
                    let merged = crate::snapshot::incremental::merge_recordings(
                        policy.select_owned(recording),
                        seed.inputs.recording,
                    );
                    let records =
                        crate::snapshot::incremental::merge_replay(records, seed.inputs.replay);
                    let tables =
                        crate::snapshot::incremental::merge_tables(tables, seed.inputs.tables);
                    (
                        merged,
                        records,
                        tables,
                        workspace_status || seed.inputs.workspace_status,
                    )
                }
            };
            let window_us = recording
                .window
                .map_or(0, |window| window.as_micros() as u64);
            let observations = recording.observations.len();
            let integrity: usize = recording
                .repositories
                .iter()
                .map(|repository| repository.entries.len())
                .sum();
            let selected = if merged {
                recording
            } else {
                policy.select_owned(recording)
            };
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
                plan_bytes: plan_len as u64,
                inputs_bytes: inputs.len() as u64,
                encode_us: started.elapsed().as_micros() as u64,
                full_validation: policy == Policy::Full,
            };
            let inputs_ms = ms(started.elapsed());
            // P3: the plan is stored compressed (MEM4: its parts were
            // compressed as they were encoded, before execution).
            let stored = format::assemble_compressed(plan);
            let note = format!(
                "{} observations validated of {}, {} trees, {} replayed local repositories; INPUTS {} bytes encoded in {:.1} ms on the writer; PLAN encoded and compressed to {} bytes in {:.1} ms",
                stats.validated,
                stats.observations,
                stats.trees,
                stats.replay_records,
                stats.inputs_bytes,
                inputs_ms,
                stored.len(),
                compressed_ms,
            );
            let mut sections = vec![
                (Section::Inputs, inputs),
                (Section::Events, events),
                (Section::Plan, stored),
                (Section::Stats, format::encode_stats(&stats)),
            ];
            if let Some(index) = index
                .as_ref()
                .and_then(|index| crate::snapshot::incremental::encode_index(index).ok())
            {
                sections.push((Section::Index, index));
            }
            if let Some(derive) = derive
                .as_ref()
                .and_then(|derive| crate::snapshot::derive::encode(derive).ok())
            {
                sections.push((Section::Derive, derive));
            }
            Ok((sections, note))
        }))
    }

    /// The command (for `test` and `run`: its build part) succeeded, or
    /// failed only in execution: put the snapshot in place.
    pub fn finish(mut self) {
        self.end_window();
        self.finished = true;
        let pending = self.pending.take();
        let outcome = match (pending, self.skipped.take()) {
            (Some(pending), _) => match pending.logged(self.log.clone()).commit() {
                Ok((bytes, note)) => {
                    if self.derivable
                        && let Some(family) = &self.family
                    {
                        crate::snapshot::derive::mark(&self.cache, family, &self.key);
                    }
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
