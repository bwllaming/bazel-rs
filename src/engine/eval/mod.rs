//! The parallel analysis evaluator (docs/EVALUATOR.md): a restart-based,
//! Skyframe-style evaluator replacing the sequential phase-1 walk and the
//! P3/S1/S2 pipeline. S0 (this skeleton) freezes the interfaces the three
//! S1 workers build on; nothing here runs in a build yet.
//!
//! Files and owners (docs/EVALUATOR.md "Ownership"):
//! - [`key`] NodeKey, CtKey, NodeId interning — worker A;
//! - [`cx`] the body context (`need`, `Missing`) — worker A;
//! - [`graph`], [`scheduler`], [`cycles`] node storage, the inline and pool
//!   drivers, cycles at quiescence — worker A (S1);
//! - [`value`] node values, [`shared`] the shared read view — worker B;
//!   S1 adds the node bodies (`bodies/*.rs`, B);
//! - [`transcript`] events and the tape, [`replay_bench`] — worker C;
//!   S1 adds `pass1.rs`, S3 `pass2.rs` (C).
//!
//! The switch: `BAZEL_RS_ANALYSIS=walk` (the default: today's walk) or
//! `eval` (the evaluator; S0: an error naming the stage), and
//! `BAZEL_RS_EVAL_VERIFY=1` (both engines, compared; S0: an error).

// S0: the interfaces are not wired into the walk yet (S1 is).
#![allow(dead_code)]

pub(super) mod cx;
pub(super) mod cycles;
pub(super) mod graph;
pub(super) mod key;
pub(super) mod replay_bench;
pub(super) mod scheduler;
pub(super) mod shared;
pub(super) mod transcript;
pub(super) mod value;
pub(super) mod verify;

/// Which analysis engine runs phase 1 (`BAZEL_RS_ANALYSIS`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum AnalysisMode {
    /// Today's sequential walk (the default).
    Walk,
    /// The parallel evaluator.
    Eval,
}

impl AnalysisMode {
    /// The mode a `BAZEL_RS_ANALYSIS` value names (unset or empty: walk).
    pub(crate) fn parse(value: Option<&str>) -> Result<Self, String> {
        match value.unwrap_or("") {
            "" | "walk" => Ok(Self::Walk),
            "eval" => Ok(Self::Eval),
            other => Err(format!(
                "BAZEL_RS_ANALYSIS={other}: expected `walk` (the default) or `eval`"
            )),
        }
    }

    /// The process's mode.
    pub(crate) fn from_env() -> Result<Self, String> {
        // snapshot-unrecorded: an internal engine switch; both engines must plan identically.
        let value = std::env::var("BAZEL_RS_ANALYSIS").ok();
        Self::parse(value.as_deref())
    }
}

/// The gate at the start of planning: the walk runs unless the
/// environment asks for the evaluator or its verifier, which S0 does not
/// implement yet (an error, never a silent fallback).
pub(crate) fn check_mode() -> Result<(), String> {
    if verify::enabled() {
        return verify::run();
    }
    match AnalysisMode::from_env()? {
        AnalysisMode::Walk => Ok(()),
        AnalysisMode::Eval => Err(
            "BAZEL_RS_ANALYSIS=eval: the parallel analysis evaluator is not implemented yet (stage S0, docs/EVALUATOR.md); unset it or use BAZEL_RS_ANALYSIS=walk"
                .to_owned(),
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mode_parses_walk_by_default() {
        assert_eq!(AnalysisMode::parse(None), Ok(AnalysisMode::Walk));
        assert_eq!(AnalysisMode::parse(Some("")), Ok(AnalysisMode::Walk));
        assert_eq!(AnalysisMode::parse(Some("walk")), Ok(AnalysisMode::Walk));
        assert_eq!(AnalysisMode::parse(Some("eval")), Ok(AnalysisMode::Eval));
        assert!(
            AnalysisMode::parse(Some("parallel"))
                .unwrap_err()
                .contains("expected `walk`")
        );
    }
}
