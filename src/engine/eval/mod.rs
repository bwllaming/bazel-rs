//! The parallel analysis evaluator (docs/EVALUATOR.md): a restart-based,
//! Skyframe-style evaluator replacing the sequential phase-1 walk and the
//! P3/S1/S2 pipeline. S1: phase 1 under `BAZEL_RS_ANALYSIS=eval` runs the
//! node bodies on the inline driver, linearized by pass 1.
//!
//! Files and owners (docs/EVALUATOR.md "Ownership"):
//! - [`key`] NodeKey, CtKey, NodeId interning — worker A;
//! - [`cx`] the body context (`need`, `Missing`) — worker A;
//! - [`graph`], [`scheduler`], [`cycles`] node storage, the inline and pool
//!   drivers, cycles at quiescence — worker A (S1);
//! - [`value`] node values, [`shared`] the shared read view — worker B;
//!   [`nodes`] the phase-1 bodies (B);
//! - [`transcript`] events and the tape, [`linearize`] pass 1, the S1
//!   driver, the cycle handler, [`verify`], [`replay_bench`] — worker C;
//!   S3 adds pass 2.
//!
//! The switch ([`select`]): the evaluator by default (`BAZEL_RS_ANALYSIS`
//! unset, empty or `eval`), with an automatic, silent fallback to the walk
//! for the requests it does not cover; `BAZEL_RS_ANALYSIS=walk` forces
//! the sequential walk; `BAZEL_RS_EVAL_VERIFY=1` plans with both engines
//! and compares.

// Interfaces of later stages (pool driver, CT2/Aspect values) are not all
// wired yet.
#![allow(dead_code)]

pub(super) mod cx;
pub(super) mod cycles;
pub(super) mod graph;
pub(super) mod key;
pub(super) mod linearize;
pub(super) mod nodes;
pub(super) mod replay_bench;
pub(super) mod scheduler;
pub(super) mod select;
pub(super) mod shared;
pub(super) use crate::eval_timeline as timeline;
pub(super) mod transcript;
pub(super) mod value;
pub(super) mod verify;

/// Which analysis engine runs phase 1 (`BAZEL_RS_ANALYSIS`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum AnalysisMode {
    /// The sequential walk (`BAZEL_RS_ANALYSIS=walk`).
    Walk,
    /// The parallel evaluator (the default; [`select`] falls back to the
    /// walk for what it does not cover).
    Eval,
}

impl AnalysisMode {
    /// The mode a `BAZEL_RS_ANALYSIS` value names (unset or empty: eval).
    pub(crate) fn parse(value: Option<&str>) -> Result<Self, String> {
        match value.unwrap_or("") {
            "" | "eval" => Ok(Self::Eval),
            "walk" => Ok(Self::Walk),
            other => Err(format!(
                "BAZEL_RS_ANALYSIS={other}: expected `eval` (the default) or `walk`"
            )),
        }
    }

    /// The process's mode.
    pub(crate) fn from_env() -> Result<Self, String> {
        // snapshot-unrecorded: an internal engine switch; both engines must plan identically.
        let value = std::env::var("BAZEL_RS_ANALYSIS").ok();
        // The test-suite mode: a binary built with
        // BAZEL_RS_DEFAULT_ANALYSIS=walk plans with the walk unless the
        // environment says otherwise (the walk-forced suite run).
        Self::parse(
            value
                .as_deref()
                .or(option_env!("BAZEL_RS_DEFAULT_ANALYSIS")),
        )
    }
}

/// The gate at the start of planning: the mode this build plans in
/// (before [`select`]'s fallback for the requests the evaluator does not
/// cover).
pub(crate) fn check_mode() -> Result<AnalysisMode, String> {
    AnalysisMode::from_env()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mode_parses_eval_by_default() {
        assert_eq!(AnalysisMode::parse(None), Ok(AnalysisMode::Eval));
        assert_eq!(AnalysisMode::parse(Some("")), Ok(AnalysisMode::Eval));
        assert_eq!(AnalysisMode::parse(Some("walk")), Ok(AnalysisMode::Walk));
        assert_eq!(AnalysisMode::parse(Some("eval")), Ok(AnalysisMode::Eval));
        assert!(
            AnalysisMode::parse(Some("parallel"))
                .unwrap_err()
                .contains("expected `eval`")
        );
    }
}
