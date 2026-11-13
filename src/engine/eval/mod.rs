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
//! unset, empty or `auto`), with an automatic, silent fallback to the walk
//! for the requests it does not cover or where the walk is faster;
//! `BAZEL_RS_ANALYSIS=eval` keeps only the coverage fallbacks;
//! `BAZEL_RS_ANALYSIS=walk` forces the sequential walk; `BAZEL_RS_EVAL_VERIFY=1` plans with both engines
//! and compares.

// Interfaces of later stages (pool driver, CT2/Aspect values) are not all
// wired yet.
#![allow(dead_code)]

pub(super) mod cquery_ops;
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
    /// The parallel evaluator, asked for (`BAZEL_RS_ANALYSIS=eval`):
    /// [`select`] falls back to the walk only for what it does not cover.
    Eval,
    /// The default (`BAZEL_RS_ANALYSIS` unset or empty): the evaluator,
    /// and the walk also where it is faster ([`select::Fallback::Profile`]).
    Auto,
}

impl AnalysisMode {
    /// The mode a `BAZEL_RS_ANALYSIS` value names (unset or empty: auto).
    pub(crate) fn parse(value: Option<&str>) -> Result<Self, String> {
        match value.unwrap_or("") {
            "" | "auto" => Ok(Self::Auto),
            "eval" => Ok(Self::Eval),
            "walk" => Ok(Self::Walk),
            other => Err(format!(
                "BAZEL_RS_ANALYSIS={other}: expected `auto` (the default: the evaluator where it is covered and faster), `eval` or `walk`"
            )),
        }
    }

    /// The process's mode.
    pub(crate) fn from_env() -> Result<Self, String> {
        // snapshot-unrecorded: an internal engine switch; both engines must plan identically.
        let value = std::env::var("BAZEL_RS_ANALYSIS").ok();
        // The test-suite modes: a binary built with
        // BAZEL_RS_DEFAULT_ANALYSIS=walk (or `eval`) plans that way unless
        // the environment says otherwise (the walk-forced and the
        // eval-forced suite runs).
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
    order()?;
    AnalysisMode::from_env()
}

/// What the analysis keeps of the sequential order where genuine Bazel
/// does not (`BAZEL_RS_ANALYSIS_ORDER`, docs/EVALUATOR.md "Genuine
/// parity, task C").
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum AnalysisOrder {
    /// The default (unset, empty, `exact`): every console write in the
    /// sequential walk's order, the first error in plan order —
    /// byte-identical to the walk.
    Exact,
    /// `genuine`: as genuine Bazel's parallel analysis — a target's
    /// analysis-time lines written as one block when its job finishes,
    /// the build failing on the first confirmed error to finish. Never
    /// read from or saved as an analysis snapshot.
    Genuine,
}

impl AnalysisOrder {
    /// The order a `BAZEL_RS_ANALYSIS_ORDER` value names.
    pub(crate) fn parse(value: Option<&str>) -> Result<Self, String> {
        match value.unwrap_or("") {
            "" | "exact" => Ok(Self::Exact),
            "genuine" => Ok(Self::Genuine),
            other => Err(format!(
                "BAZEL_RS_ANALYSIS_ORDER={other}: expected `exact` (the default: the sequential order) or `genuine` (console blocks as targets finish, fail fast on the first error)"
            )),
        }
    }
}

/// The process's analysis order (`BAZEL_RS_ANALYSIS_ORDER`; a test-suite
/// binary built with `BAZEL_RS_DEFAULT_ANALYSIS_ORDER=genuine` defaults to
/// it: the genuine suite run).
pub(crate) fn order() -> Result<AnalysisOrder, String> {
    static ORDER: std::sync::LazyLock<Result<AnalysisOrder, String>> =
        std::sync::LazyLock::new(|| {
            // snapshot-unrecorded: a genuine run is never read from or saved as a snapshot (snapshot::key::test_hook).
            let value = std::env::var("BAZEL_RS_ANALYSIS_ORDER").ok();
            AnalysisOrder::parse(
                value
                    .as_deref()
                    .or(option_env!("BAZEL_RS_DEFAULT_ANALYSIS_ORDER")),
            )
        });
    ORDER.clone()
}

/// Whether the process's analysis order is this binary's default (exact,
/// or genuine for a binary built with
/// `BAZEL_RS_DEFAULT_ANALYSIS_ORDER=genuine`; `false` for an invalid
/// value): only then are analysis snapshots read and saved — every
/// snapshot a binary keeps (its key names the binary) has its default
/// order's console, which a hit replays.
pub(crate) fn order_is_default() -> bool {
    order().is_ok_and(|order| {
        AnalysisOrder::parse(option_env!("BAZEL_RS_DEFAULT_ANALYSIS_ORDER")) == Ok(order)
    })
}

/// `BAZEL_RS_ANALYSIS_ORDER=genuine` (an invalid value: exact here; the
/// planning gate [`check_mode`] reports it).
pub(crate) fn genuine() -> bool {
    order() == Ok(AnalysisOrder::Genuine)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mode_parses_eval_by_default() {
        assert_eq!(AnalysisMode::parse(None), Ok(AnalysisMode::Auto));
        assert_eq!(AnalysisMode::parse(Some("")), Ok(AnalysisMode::Auto));
        assert_eq!(AnalysisMode::parse(Some("auto")), Ok(AnalysisMode::Auto));
        assert_eq!(AnalysisMode::parse(Some("walk")), Ok(AnalysisMode::Walk));
        assert_eq!(AnalysisMode::parse(Some("eval")), Ok(AnalysisMode::Eval));
        assert!(
            AnalysisMode::parse(Some("parallel"))
                .unwrap_err()
                .contains("expected `auto`")
        );
    }

    #[test]
    fn order_parses_exact_by_default() {
        assert_eq!(AnalysisOrder::parse(None), Ok(AnalysisOrder::Exact));
        assert_eq!(AnalysisOrder::parse(Some("")), Ok(AnalysisOrder::Exact));
        assert_eq!(
            AnalysisOrder::parse(Some("exact")),
            Ok(AnalysisOrder::Exact)
        );
        assert_eq!(
            AnalysisOrder::parse(Some("genuine")),
            Ok(AnalysisOrder::Genuine)
        );
        assert!(
            AnalysisOrder::parse(Some("stream"))
                .unwrap_err()
                .contains("expected `exact`")
        );
    }
}
