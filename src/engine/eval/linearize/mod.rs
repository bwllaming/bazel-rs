//! The linearizer (docs/EVALUATOR.md "Determinism"; worker C).
//!
//! - [`pass1`]: the DFS replay of the transcripts from the top-level
//!   segments — the sequential walk's order — into the tape and the
//!   linearizer-only phase-1 state (plan order, delegations, keep-going
//!   edges, toolchain implementations); resumable, so it can run up to a
//!   flush while the evaluation is still running.
//! - [`capture`]: what the engine's coordinator services do while the
//!   evaluator runs (tracker scopes, the engine's own target lookups).
//! - [`driver`]: `BAZEL_RS_ANALYSIS=eval` phase 1 (S1): the node bodies on
//!   the inline driver, pass 1, the tape applied to the engine (console,
//!   warnings, tracker, plan; phase 2 at each flush).
//! - [`cycles`]: the walk-order cycle handler at quiescence
//!   (`Pass1Cycles`, replacing A's stand-in `DfsCycles`).
//! - S3 adds `pass2.rs` (streaming the tape, merging CT2 deltas).

pub(crate) mod capture;
pub(crate) mod cycles;
pub(crate) mod driver;
pub(crate) mod pass1;
