//! Transcripts and the tape (docs/EVALUATOR.md "Determinism").
//!
//! A CT1 (and aspect) body does not write the engine: everything whose
//! effect the sequential walk's order decides is recorded, in body order,
//! as an [`Event`] of the node's [`Transcript`]. Pass 1 replays the
//! transcripts depth-first from the requested targets — exactly the
//! sequential walk's DFS — and produces the plan order, the first-visit
//! decisions and the [`Tape`]; pass 2 streams the tape, emitting each
//! effect at its linearized position and merging CT2 deltas at the flush
//! markers.
//!
//! Owner: worker C (pass 1, pass 2). Frozen in S0: B emits these events.

use crate::configuration::ConfigId;
use crate::label::{Label, PackageId};

use super::super::ConfiguredTargetKey;
use super::key::{CtKey, NodeId};

/// One recorded effect of a body, in body order.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Event {
    /// A dependency edge to a CT1 node (today an `analyze`/`analyze_key`
    /// call): pass 1 descends into it here (DFS order, cycle detection by
    /// the active chain, `keep_going_state.record_edge`).
    Reach(NodeId),
    /// An incremental-tracker target access (`track_access`).
    Access(Label),
    /// A source file reached (`track_source`).
    Source(Label),
    /// A package first committed here (its load prints, `generated`,
    /// progress; `commit_load_unit`).
    PkgCommit(PackageId),
    /// A memo node used here: its first linearized use emits its effects
    /// (prints of an aspect predicate, a registered-toolchains cycle
    /// report, a toolchain implementation's forced platform).
    MemoUse(NodeId),
    /// A warning (`warnings.push`, deduplicated where the walk
    /// deduplicates).
    Warn(String),
    /// An analysis print() line (`note_analysis_print`; transition
    /// applications replay theirs on every use).
    Print(String),
    /// A cquery graph edge (`capture_cquery_edge`).
    CqueryEdge {
        from: ConfiguredTargetKey,
        to: Label,
        config: ConfigId,
        attribute: String,
        /// A tool (exec) dependency.
        tool: bool,
        /// An aspect's dependency.
        aspect: bool,
    },
    /// An incremental shared scope entered/left (`SharedScope::enter`).
    ScopeEnter(&'static str),
    ScopeExit,
    /// A `plan_pending` flush point: the CT2 deltas planned so far merge
    /// here (pass 2).
    Flush,
    /// The body continued after a CsMatch node (the flush it replaces).
    Continue(NodeId),
    /// A selected toolchain implementation needed for `exec_platform`
    /// (`toolchain_implementation`): pass 1 checks conflicts and decides
    /// `forced_exec` first-visit.
    ToolchainImpl {
        implementation: ConfiguredTargetKey,
        exec_platform: Label,
    },
    /// The body failed here (its value is the error).
    Error(String),
    /// The configured target joins the plan here (`plan.push`).
    PlanPush(CtKey),
}

/// One node's events, in body order.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct Transcript {
    pub(crate) events: Vec<Event>,
}

impl Transcript {
    pub(crate) fn push(&mut self, event: Event) {
        self.events.push(event);
    }

    /// The CT1 nodes it reaches, in order (pass 1's DFS children).
    pub(crate) fn reaches(&self) -> impl Iterator<Item = NodeId> + '_ {
        self.events.iter().filter_map(|event| match event {
            Event::Reach(id) => Some(*id),
            _ => None,
        })
    }
}

/// One step of the tape pass 2 streams.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum TapeOp {
    /// Emit a phase-1 effect at its linearized position.
    Effect(Event),
    /// The plan index of a configured target (pass 1's order).
    Plan(CtKey),
    /// Merge the CT2 deltas of plan[flushed..len] here.
    Flush,
}

/// Pass 1's output: the linearized effects, plan indices and flush
/// markers, in the sequential walk's order.
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct Tape {
    pub(crate) ops: Vec<TapeOp>,
    /// The plan length so far (the index of the next `Plan`).
    planned: usize,
}

impl Tape {
    /// Append a plan entry; returns its plan index.
    pub(crate) fn plan(&mut self, key: CtKey) -> usize {
        self.ops.push(TapeOp::Plan(key));
        self.planned += 1;
        self.planned - 1
    }

    pub(crate) fn effect(&mut self, event: Event) {
        self.ops.push(TapeOp::Effect(event));
    }

    pub(crate) fn flush(&mut self) {
        self.ops.push(TapeOp::Flush);
    }

    pub(crate) fn planned(&self) -> usize {
        self.planned
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn transcript_reaches_in_order_and_tape_counts_plan_entries() {
        let mut transcript = Transcript::default();
        transcript.push(Event::Reach(NodeId(3)));
        transcript.push(Event::Warn("w".into()));
        transcript.push(Event::Reach(NodeId(1)));
        assert_eq!(
            transcript.reaches().collect::<Vec<_>>(),
            [NodeId(3), NodeId(1)]
        );
        let key = CtKey::plain(ConfiguredTargetKey::new(
            Label::parse_canonical("//a:a").unwrap(),
            crate::configuration::Configuration::default().id,
        ));
        let mut tape = Tape::default();
        tape.effect(Event::Warn("w".into()));
        assert_eq!(tape.plan(key.clone()), 0);
        tape.flush();
        assert_eq!(tape.plan(key), 1);
        assert_eq!(tape.planned(), 2);
        assert_eq!(tape.ops.len(), 4);
    }
}
