//! Which engine plans a request (docs/EVALUATOR.md "Selecting the
//! engine"): the evaluator by default (`BAZEL_RS_ANALYSIS` unset, empty or
//! `auto`, and `eval`), the walk when `BAZEL_RS_ANALYSIS=walk` forces it.
//! No request falls back to the walk any more ([`Selected`]). Both engines
//! plan identically (stdout, stderr, the action graph, the incremental
//! index, the snapshot recording; `--profile`'s content as docs/PROFILE.md
//! "Parity" defines it); only internal errors differ: the evaluator's name
//! `BAZEL_RS_ANALYSIS=walk` as the workaround ([`with_workaround`]).
//!
//! No automatic retry with the walk after an evaluator failure: by then
//! the failed planning has fetched repositories, written console output
//! (warnings, `print()` lines, fetch progress), recorded observations for
//! the analysis snapshot and possibly the lockfile; a second planning in
//! the same process would print and record twice.

use super::super::Engine;
use super::AnalysisMode;

/// The workaround an evaluator internal error names.
pub(crate) const WORKAROUND: &str = "BAZEL_RS_ANALYSIS=walk";

/// The engine one planning runs on.
///
/// (No fallback is left: the last one, `--profile`, plans with the
/// evaluator since eval-profile — its spans are recorded per thread, the
/// profile's content defined as a set, docs/PROFILE.md "Parity". A future
/// fallback — the walk, silently, for a request the evaluator does not
/// cover or where the walk is faster — is a variant here reported as
/// `walk (fallback: <reason>)`.)
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Selected {
    /// The evaluator.
    Eval,
    /// The walk, forced (`BAZEL_RS_ANALYSIS=walk`, a test-suite binary
    /// built with `BAZEL_RS_DEFAULT_ANALYSIS=walk`, or the walk side of
    /// `BAZEL_RS_EVAL_VERIFY`).
    Forced,
}

impl Selected {
    pub(crate) fn evaluates(self) -> bool {
        self == Self::Eval
    }
}

impl Engine {
    /// The engine this planning runs on in `mode` (`checkpoint`: a cquery
    /// root checkpoint is installed; the evaluator covers it).
    pub(in crate::engine) fn select_engine(
        &self,
        mode: AnalysisMode,
        checkpoint: bool,
    ) -> Selected {
        let _ = checkpoint;
        let selected = match mode {
            AnalysisMode::Walk => Selected::Forced,
            AnalysisMode::Eval | AnalysisMode::Auto => Selected::Eval,
        };
        report(selected);
        selected
    }
}

/// `BAZEL_RS_ANALYSIS_REPORT=1` (hidden, diagnostics): one stderr line per
/// planning naming the engine (`bazel-rs: analysis engine: eval`, `walk
/// (forced)`).
fn report(selected: Selected) {
    // snapshot-unrecorded: a diagnostic switch; both engines plan identically.
    if std::env::var_os("BAZEL_RS_ANALYSIS_REPORT").is_none_or(|value| value != "1") {
        return;
    }
    let text = match selected {
        Selected::Eval => "eval",
        Selected::Forced => "walk (forced)",
    };
    crate::ui::raw(format!("bazel-rs: analysis engine: {text}\n").as_bytes());
}

/// An evaluator planning's error: an internal error that does not name the
/// workaround yet gets it appended (a user error is the walk's text and
/// stays as it is).
pub(crate) fn with_workaround(error: String) -> String {
    if !error.contains("internal error") || error.contains(WORKAROUND) {
        return error;
    }
    format!(
        "{error} (the parallel analysis evaluator, the default engine; rerun with {WORKAROUND} to plan with the sequential walk)"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn internal_errors_name_the_workaround_once() {
        assert_eq!(
            with_workaround("no such target '//:x'".to_owned()),
            "no such target '//:x'"
        );
        let named = "internal error: BAZEL_RS_ANALYSIS=eval: x; rerun with BAZEL_RS_ANALYSIS=walk";
        assert_eq!(with_workaround(named.to_owned()), named);
        let plain = with_workaround("internal error: pass 1: a body vanished".to_owned());
        assert!(plain.starts_with("internal error: pass 1: a body vanished (the parallel"));
        assert_eq!(plain.matches(WORKAROUND).count(), 1, "{plain}");
    }
}
