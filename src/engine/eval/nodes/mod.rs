//! The phase-1 node functions (docs/EVALUATOR.md, worker B).
//!
//! - [`phase1`]: the phase-1 bodies (configured-target analysis and the
//!   memo functions it calls), written once over [`host::Host`];
//! - [`walk`]: the sequential walk's host (today's behaviour);
//!
//! Owner: worker B.

pub(crate) mod host;
pub(crate) mod phase1;
pub(crate) mod walk;
