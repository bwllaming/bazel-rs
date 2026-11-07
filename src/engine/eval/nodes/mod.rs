//! The phase-1 node functions (docs/EVALUATOR.md, worker B).
//!
//! - [`phase1`]: the phase-1 bodies (configured-target analysis and the
//!   memo functions it calls), written once over [`host::Host`];
//! - [`walk`]: the sequential walk's host (today's behaviour);
//! - [`eval_host`]: the evaluator's host and the node bodies
//!   ([`eval_host::body`]);
//! - [`inline`]: B's S1 inline driver (depth first, one thread);
//! - [`verify_phase1`]: `BAZEL_RS_EVAL_VERIFY_PHASE1=1`, the walk compared
//!   with the nodes after phase 1.
//!
//! Owner: worker B.

pub(crate) mod eval_host;
pub(crate) mod host;
pub(crate) mod inline;
pub(crate) mod phase1;
pub(crate) mod verify_phase1;
pub(crate) mod walk;
