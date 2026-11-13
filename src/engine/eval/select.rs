//! Which engine plans a request (docs/EVALUATOR.md "Selecting the
//! engine"): the evaluator by default (`BAZEL_RS_ANALYSIS` unset, empty or
//! `auto`), the walk when `BAZEL_RS_ANALYSIS=walk` forces it, and the walk,
//! silently, for the requests the evaluator does not cover or where the
//! walk is faster ([`Fallback`]; `BAZEL_RS_ANALYSIS=eval` keeps only the
//! coverage fallbacks). Both engines plan identically (stdout, stderr,
//! the action graph, the incremental index, the snapshot recording), so the
//! fallback is never visible; only internal errors differ: the evaluator's
//! name `BAZEL_RS_ANALYSIS=walk` as the workaround ([`with_workaround`]).
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

/// Why a request plans with the walk although the evaluator is selected:
/// a part of phase 1 the evaluator's driver does not run yet (S4), or a
/// mode in which the evaluator is slower than the walk.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Fallback {
    /// `--profile`: the evaluator keeps every flush ordered (the profile's
    /// spans of phase 2 and of the tape in the walk's order) and is slower
    /// than the walk there. (A snapshot recording and the incremental
    /// tracker plan with the evaluator since eval-record: their effects are
    /// applied in plan order by the deferred walk.)
    Profile,
}

impl Fallback {
    /// A fallback for speed (the evaluator covers the request):
    /// `BAZEL_RS_ANALYSIS=eval` plans with the evaluator anyway.
    pub(crate) fn performance(self) -> bool {
        matches!(self, Self::Profile)
    }

    /// The reason as `BAZEL_RS_ANALYSIS_REPORT` prints it.
    pub(crate) fn reason(self) -> &'static str {
        match self {
            Self::Profile => "--profile",
        }
    }
}

/// The engine one planning runs on, and why.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Selected {
    /// The evaluator.
    Eval,
    /// The walk, forced (`BAZEL_RS_ANALYSIS=walk`, a test-suite binary
    /// built with `BAZEL_RS_DEFAULT_ANALYSIS=walk`, or the walk side of
    /// `BAZEL_RS_EVAL_VERIFY`).
    Forced,
    /// The walk for a request the evaluator does not cover.
    Fallback(Fallback),
}

impl Selected {
    pub(crate) fn evaluates(self) -> bool {
        self == Self::Eval
    }
}

impl Engine {
    /// Why this request cannot plan with the evaluator (`None`: it can).
    /// `checkpoint`: a cquery root checkpoint is installed.
    pub(in crate::engine) fn eval_fallback(&self, checkpoint: bool) -> Option<Fallback> {
        let _ = checkpoint;
        if crate::profile::active() {
            Some(Fallback::Profile)
        } else {
            None
        }
    }

    /// The engine this planning runs on in `mode`.
    pub(in crate::engine) fn select_engine(
        &self,
        mode: AnalysisMode,
        checkpoint: bool,
    ) -> Selected {
        let selected = match mode {
            AnalysisMode::Walk => Selected::Forced,
            AnalysisMode::Eval | AnalysisMode::Auto => match self.eval_fallback(checkpoint) {
                // `eval` asked for: the evaluator also where it is slower.
                Some(fallback) if mode == AnalysisMode::Eval && fallback.performance() => {
                    Selected::Eval
                }
                Some(fallback) => Selected::Fallback(fallback),
                None => Selected::Eval,
            },
        };
        report(selected);
        selected
    }
}

/// `BAZEL_RS_ANALYSIS_REPORT=1` (hidden, diagnostics): one stderr line per
/// planning naming the engine (`bazel-rs: analysis engine: eval`, `walk
/// (forced)`, `walk (fallback: --profile)`).
fn report(selected: Selected) {
    // snapshot-unrecorded: a diagnostic switch; both engines plan identically.
    if std::env::var_os("BAZEL_RS_ANALYSIS_REPORT").is_none_or(|value| value != "1") {
        return;
    }
    let text = match selected {
        Selected::Eval => "eval".to_owned(),
        Selected::Forced => "walk (forced)".to_owned(),
        Selected::Fallback(fallback) => format!("walk (fallback: {})", fallback.reason()),
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

    #[test]
    fn fallback_reasons_are_distinct() {
        let all = [Fallback::Profile];
        let reasons: std::collections::BTreeSet<_> = all.iter().map(|f| f.reason()).collect();
        assert_eq!(reasons.len(), all.len());
    }
}
