//! The persistent analysis snapshot (PLAN-L2 §1, update 1): a `build`,
//! `test` or `run` whose recorded inputs are all unchanged skips module
//! resolution, loading and analysis — it replays what planning printed and
//! executes the saved plan.
//!
//! * L2-W3: [`record`] (`BAZEL_RS_SNAPSHOT=record`: the observations of the
//!   planning window through crate::observed_fs, an eligibility and taint
//!   report, a text dump) and [`validate`] (the §1c re-validation).
//! * L2-W4R: [`replay`], the local repository rules a run executed,
//!   recorded with the pinned repository tables their replay needs.
//! * L2-W4: [`codec`], the binary encoding of the planned request
//!   (crate::request).
//! * L2-W5: the store and the hit path, off by default:
//!   `BAZEL_RS_SNAPSHOT=on` reads and writes snapshots — the key G
//!   ([`key`], [`exe_id`]), the binary file format ([`format`]) and store
//!   ([`store`]), the hit ([`hit`]: validation ∥ replay ∥ plan decode), the
//!   save after a full analysis ([`save`]), the capture of what planning
//!   printed ([`capture`]) and the diagnostics stream that bypasses it
//!   ([`diag`]). `BAZEL_RS_SNAPSHOT_EXPLAIN=1` says on the diagnostics
//!   stream why a lookup missed or why a run was not saved.
//! * L2-W6: `BAZEL_RS_SNAPSHOT=verify` ([`verify`]): `on`, and every hit
//!   also runs the full analysis in its shadow and compares the two (L2-W5f:
//!   a mismatch is logged to `<cache>/snapshots/verify.log` and the full
//!   analysis's plan executes; `BAZEL_RS_SNAPSHOT_VERIFY_STRICT=1` exits 70).
//! * L2-W5f (the W5 review's follow-ups): ENV records validated against the
//!   repository environment, the status flag of `--nobuild` plans, the
//!   per-tree policy's directories, the store-path self-heal, a failed
//!   check cancelling the rest of the attempt.
//! * L2-W5g: the soak's logs ([`telemetry`]: `verify.log` names the first
//!   taint and differing path; `miss.log` every miss and unsaved run).
//!
//! docs/ANALYSIS_SNAPSHOT.md describes the store, the hit path and the
//! verify mode.

pub mod capture;
pub mod codec;
pub mod diag;
pub mod exe_id;
pub mod format;
pub mod hit;
pub mod key;
pub mod record;
pub mod replay;
pub mod save;
pub mod store;
pub mod telemetry;
pub mod validate;
pub mod verify;

use std::path::Path;
use std::time::Instant;

/// The mode variable: `on`, `verify`, `record` or `off` (unset).
pub const MODE_VAR: &str = "BAZEL_RS_SNAPSHOT";
/// Marker paths bracketing the hit path for the syscall audit
/// (scripts/snapshot_fs_audit.py `--hit`): stat'ed when the check point
/// starts ([`check`]) and, on a hit, right before the hit's execution starts
/// (src/main.rs). They never exist.
pub const MARKER_HIT_BEGIN: &str = "/.bazel-rs-snapshot-marker/hit-begin";
pub const MARKER_HIT_END: &str = "/.bazel-rs-snapshot-marker/hit-end";

/// Stat a marker path (see [`MARKER_HIT_BEGIN`]).
pub fn marker(path: &str) {
    let _ = std::fs::symlink_metadata(path);
}
/// Diagnostics of `on`: why a lookup missed, why a run was not saved.
pub const EXPLAIN_VAR: &str = "BAZEL_RS_SNAPSHOT_EXPLAIN";

/// What `BAZEL_RS_SNAPSHOT` asks for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    Off,
    /// Record and report, never save or load (L2-W3).
    Record,
    /// Read and write snapshots (`build`, `test`, `run`).
    On,
    /// [`Mode::On`], and every hit also runs the full analysis and compares
    /// ([`verify`]).
    Verify,
}

impl Mode {
    /// The mode of this process; any other value is reported and ignored.
    pub fn from_env() -> Self {
        let Some(value) = std::env::var_os(MODE_VAR) else {
            return Self::Off;
        };
        match value.to_str() {
            Some("on") => Self::On,
            Some("verify") => Self::Verify,
            Some("record") => Self::Record,
            Some("off" | "") => Self::Off,
            _ => {
                observed_fs_diag(&format!(
                    "ignoring {MODE_VAR}={}: expected on, verify, record or off",
                    crate::observed_fs::escape(&value)
                ));
                Self::Off
            }
        }
    }
}

fn observed_fs_diag(line: &str) {
    crate::observed_fs::diag(line);
}

/// Whether `BAZEL_RS_SNAPSHOT_EXPLAIN` is on.
pub fn explain() -> bool {
    crate::diagnostic_switch(std::env::var_os(EXPLAIN_VAR))
}

/// The snapshot sessions of one command: the record mode's report, the save
/// after a full analysis, the follow-up of a hit, or verify mode's shadow
/// full analysis of a hit (concluded at the end of planning, which hands
/// the hit back: crate::snapshot::verify::Verify::conclude).
#[derive(Default)]
pub struct Scope {
    pub record: Option<record::Session>,
    pub save: Option<Box<save::Save>>,
    pub hit: Option<hit::Session>,
    pub verify: Option<Box<verify::Verify>>,
}

impl Scope {
    /// The registry session whose pending lockfile writes taint the run.
    pub fn set_registry_session(&mut self, registry: crate::registry_session::SessionHandle) {
        if let Some(record) = &mut self.record {
            record.set_registry_session(registry.clone());
        }
        if let Some(verify) = &mut self.verify {
            verify.set_registry_session(registry.clone());
        }
        if let Some(save) = &mut self.save {
            save.set_registry_session(registry);
        }
    }

    /// Planning returned: the capture of what it printed ends here.
    pub fn end_window(&mut self) {
        if let Some(save) = &mut self.save {
            save.end_window();
        }
        if let Some(verify) = &mut self.verify {
            verify.end_window();
        }
    }

    /// The planned request of a successful planning; `workspace_status`:
    /// planning computed the workspace status.
    pub fn planned(&mut self, request: &crate::request::PlannedRequest, workspace_status: bool) {
        if let Some(save) = &mut self.save {
            save.planned(request, workspace_status);
        }
    }

    /// The command (for `test` and `run`: its build part) succeeded.
    pub fn finish(self) {
        let Self {
            record,
            save,
            hit,
            verify,
        } = self;
        // Concluded at the end of planning; one still here never was.
        drop(verify);
        if let Some(record) = record {
            record.finish();
        }
        if let Some(save) = save {
            save.finish();
        }
        if let Some(hit) = hit {
            hit.finish();
        }
    }

    /// The command failed with `error` (the record mode reports, nothing is
    /// saved, a stale hit removes its snapshot).
    pub fn failed(self, error: &str) {
        let Self {
            record,
            save,
            hit,
            verify,
        } = self;
        drop(verify);
        drop(record);
        drop(save);
        if let Some(hit) = hit {
            hit.failed(error);
        }
    }
}

/// What the check point decided for `BAZEL_RS_SNAPSHOT=on` (or `verify`).
pub enum Checked {
    /// Execute the saved plan.
    Hit(Box<hit::Hit>),
    /// Run the full analysis; save it when the session is `Some`.
    Miss(Option<Box<save::Save>>),
}

/// The check point's inputs.
pub struct CheckInputs<'a> {
    pub key: key::Inputs<'a>,
    pub cache: &'a Path,
    /// The replay's options and the repository environment (also what the
    /// ENV records are validated against).
    pub replay: hit::ReplayInputs,
    pub workspace_status: crate::workspace_status::WorkspaceStatusOptions,
    pub workspace: &'a Path,
    /// `BAZEL_RS_SNAPSHOT=verify`: a hit keeps what its shadow full
    /// analysis is compared with (crate::snapshot::verify).
    pub verify: bool,
}

/// The snapshot check point of `BAZEL_RS_SNAPSHOT=on` (PLAN-L2 §1a): compute
/// G, try a hit, else start the session that saves the full analysis.
/// Nothing prints but the diagnostics stream. The strace audit of the hit
/// path (scripts/snapshot_fs_audit.py `--hit`) brackets everything from
/// here to the hit's execution with [`MARKER_HIT_BEGIN`] and
/// [`MARKER_HIT_END`].
pub fn check(inputs: CheckInputs<'_>) -> Checked {
    let explain = explain();
    marker(MARKER_HIT_BEGIN);
    if let Some(hook) = key::test_hook() {
        if explain {
            observed_fs_diag(&format!(
                "off for this command: the test or debug hook {hook} is set"
            ));
        }
        return Checked::Miss(None);
    }
    let started = Instant::now();
    // The cache directory exists before the recording window opens: the
    // first run of a new cache would otherwise record it missing (planning
    // canonicalizes it) and its snapshot could never hit.
    let _ = std::fs::create_dir_all(store::directory(inputs.cache));
    let binary = match exe_id::binary_id(&store::root(inputs.cache)) {
        Ok(binary) => binary,
        Err(error) => {
            if explain {
                observed_fs_diag(&format!("off: cannot identify this binary: {error}"));
            }
            return Checked::Miss(None);
        }
    };
    let key = key::compute(&inputs.key, binary);
    let key_time = started.elapsed();
    // The miss log's context (crate::snapshot::telemetry).
    let log = telemetry::Context {
        cache: inputs.cache.to_path_buf(),
        command: inputs.key.command.to_owned(),
        targets: inputs.key.targets.to_vec(),
    };
    let outcome = hit::attempt(hit::Attempt {
        cache: inputs.cache,
        key,
        replay: inputs.replay,
        workspace_status: inputs.workspace_status,
        workspace: inputs.workspace,
        verify: inputs.verify,
        log: log.clone(),
    });
    let timing = crate::diagnostic_switch(std::env::var_os("BAZEL_RS_TIMING"));
    match outcome {
        Ok(hit) => {
            if explain {
                observed_fs_diag(&hit.summary);
            }
            if timing {
                let ms = |duration: std::time::Duration| duration.as_secs_f64() * 1000.0;
                diag::line_fmt(format_args!(
                    "PERF-SNAPSHOT hit key_ms={:.3} inputs_ms={:.3} validate_ms={:.3} replay_ms={:.3} decode_ms={:.3} status_ms={:.3} attempt_ms={:.3}",
                    ms(key_time),
                    ms(hit.timings.inputs),
                    ms(hit.timings.validation),
                    ms(hit.timings.replay),
                    ms(hit.timings.decode),
                    ms(hit.timings.status),
                    ms(hit.timings.total),
                ));
            }
            Checked::Hit(hit)
        }
        Err(reason) => {
            log.log_miss(&key.short(), &reason);
            if explain {
                observed_fs_diag(&format!("miss {}: {reason}", key.short()));
            }
            if timing {
                diag::line_fmt(format_args!(
                    "PERF-SNAPSHOT miss key_ms={:.3} attempt_ms={:.3}",
                    key_time.as_secs_f64() * 1000.0,
                    started.elapsed().as_secs_f64() * 1000.0
                ));
            }
            Checked::Miss(Some(Box::new(save::start(key, explain, log))))
        }
    }
}
