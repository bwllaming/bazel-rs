//! The phase-1 node functions (docs/EVALUATOR.md, worker B).
//!
//! - [`phase1`]: the phase-1 bodies (configured-target analysis and the
//!   memo functions it calls), written once over [`host::Host`];
//! - [`walk`]: the sequential walk's host (today's behaviour);
//! - [`eval_host`]: the evaluator's host and the node bodies
//!   ([`eval_host::body`]);
//! - [`bodies`]: the bodies as A's `Bodies` (S1: its inline driver);
//! - [`speculate`]: a Starlark flag's phase 2 off the sequential pass
//!   (S2, the CsMatch split);
//! - [`verify_phase1`]: `BAZEL_RS_EVAL_VERIFY_PHASE1=1`, the walk compared
//!   with the nodes after phase 1.
//!
//! Owner: worker B.

pub(crate) mod bodies;
pub(crate) mod eval_host;
pub(crate) mod host;
pub(crate) mod loading;
pub(crate) mod phase1;
pub(crate) mod speculate;
pub(crate) mod verify_phase1;
pub(crate) mod walk;
