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
//! it into place (crate::snapshot::store). Failures are never saved.

use crate::observed_fs::{self, Taint};
use crate::registry_session::SessionHandle;
use crate::repo_env::EnvVar;
use crate::request::PlannedRequest;
use crate::snapshot::capture::{self, Capture, Captured};
use crate::snapshot::format::{self, Section, Stats};
use crate::snapshot::key::Key;
use crate::snapshot::store;
use crate::snapshot::validate::Policy;
use std::collections::BTreeSet;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
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
    finished: bool,
}

/// The environment names whose repository-environment value an option
/// fixes (`--repo_env=NAME=VALUE`, `--repo_env==NAME`): part of the key,
/// not checked (crate::observed_fs::open_window).
pub(crate) fn fixed_names(repo_env: &[EnvVar]) -> BTreeSet<OsString> {
    repo_env
        .iter()
        .filter_map(|value| match value {
            EnvVar::Set(name, _) | EnvVar::Unset(name) => Some(OsString::from(name)),
            EnvVar::Inherit(_) => None,
        })
        .collect()
}

/// Start recording at the snapshot check point. `repo_env` decides which
/// environment names an option fixes (crate::snapshot::record::start).
pub fn start(key: Key, cache: &Path, repo_env: &[EnvVar], explain: bool) -> Save {
    let fixed = fixed_names(repo_env);
    let mut save = Save {
        key,
        cache: cache.to_path_buf(),
        explain,
        capture: None,
        window: None,
        registry: None,
        pending: None,
        skipped: None,
        started: Instant::now(),
        saved_note: String::new(),
        finished: false,
    };
    if !observed_fs::open_window(fixed) {
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
    /// recording is also taken, selected and encoded.
    pub fn planned(&mut self, request: &PlannedRequest) {
        self.end_window();
        if self.skipped.is_some() {
            return;
        }
        match self.prepare(request) {
            Ok(pending) => self.pending = Some(pending),
            Err(reason) => self.skip(reason),
        }
    }

    fn prepare(&mut self, request: &PlannedRequest) -> Result<store::Pending, String> {
        let started = Instant::now();
        let window = self
            .window
            .take()
            .ok_or("planning printed outside the capture")?;
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
        if capture::broken() {
            return Err("the stderr capture ended early (a panic?)".to_owned());
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
            let inputs = format::encode_inputs(&selected, &records, tables.as_ref());
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
            let note = format!(
                "{} observations validated of {}, {} trees, {} replayed local repositories; INPUTS {} bytes encoded in {:.1} ms on the writer",
                stats.validated,
                stats.observations,
                stats.trees,
                stats.replay_records,
                stats.inputs_bytes,
                ms(started.elapsed()),
            );
            Ok((
                vec![
                    (Section::Inputs, inputs),
                    (Section::Events, events),
                    (Section::Plan, plan),
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
            (Some(pending), _) => match pending.commit() {
                Ok((bytes, note)) => format!(
                    "saved {} ({bytes} bytes; {}; {note}; {:.1} ms after the check point)",
                    self.key.short(),
                    self.saved_note,
                    self.started.elapsed().as_secs_f64() * 1000.0
                ),
                Err(error) => format!("not saved: {error}"),
            },
            (None, Some(reason)) => format!("not saved: {reason}"),
            (None, None) => "not saved: planning did not complete".to_owned(),
        };
        if self.explain {
            observed_fs::diag(&outcome);
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
        if self.explain {
            observed_fs::diag(&match &self.skipped {
                Some(reason) => format!("not saved: the command failed ({reason})"),
                None => "not saved: the command failed".to_owned(),
            });
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
