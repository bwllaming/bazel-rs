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
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(crate) enum Event {
    /// A dependency edge to a CT1 node (today an `analyze`/`analyze_key`
    /// call): pass 1 descends into it here (DFS order, cycle detection by
    /// the active chain, `keep_going_state.record_edge`).
    ///
    /// Contract: emitted where the walk calls `track_key` (after the
    /// target lookup and the macro-namespace checks of the reached label,
    /// before its memo check). What the walk does before that point (the
    /// lookup's `Access`, a package group's resolution) belongs to the
    /// reacher. The node is a `Ct1(CtKey)` whose `platform_tree` and
    /// `forced_exec` (when kept by normalization) pass 1 checks against
    /// its own first-visit state.
    Reach(NodeId),
    /// An incremental-tracker target access (`track_access`).
    Access(Label),
    /// A source file reached (`track_source`).
    Source(Label),
    /// A package first committed here (its load prints, `generated`,
    /// progress; `commit_load_unit`); `true`: its BUILD file printed (the
    /// incremental tracker's package record, S1 addition).
    PkgCommit(PackageId, bool),
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
    /// A cquery graph edge (`capture_cquery_edge`); boxed (S1: keeps
    /// `Event` small, transcripts hold millions).
    CqueryEdge(Box<CqueryEdge>),
    /// An incremental-tracker scope entered/left: a shared computation
    /// (`SharedScope::enter(name)`), or the target pattern phase / a test
    /// suite's expansion (`PatternScope`, `SuiteScope`: the names
    /// `"\0pattern"`, `"\0suite"`).
    ScopeEnter(&'static str),
    ScopeExit,
    /// A `plan_pending` flush point: the CT2 deltas planned so far merge
    /// here (pass 2).
    Flush,
    /// The body continued after a CsMatch node (the flush it replaces).
    Continue(NodeId),
    /// A selected toolchain implementation needed for `exec_platform`
    /// (`toolchain_implementation`): pass 1 decides the first claim in DFS
    /// order (`toolchain_implementations`, the forced execution platform)
    /// and a conflict (the walk's error there: the build stops).
    ToolchainImpl(Box<ToolchainImpl>),
    /// The body failed here (its value is the error).
    Error(String),
    /// The body's configured target joins the plan here (`plan.push`; S1:
    /// always the body's own key, so the event carries none).
    PlanPush,
    /// The body's configured node is complete here (S1 interface change,
    /// docs/EVALUATOR.md "Interface changes"): the walk's
    /// `configured.insert` (main path, delegation, incompatible target),
    /// where the key leaves the active chain. Later reaches of the key are
    /// memo hits; reaches after it (the aspect edges) are no longer below
    /// the key in the active chain (their keep-going parent is the key's
    /// parent). A failed body has no `Done` (it is re-entered on a revisit,
    /// as the walk re-runs it).
    Done,
    /// A console write a coordinator service made while the evaluator
    /// ran (a package's load prints, a repository's fetch output), the
    /// index of the write the evaluation captured (S1 interface addition:
    /// written once, at its linearized position).
    Console(u32),
}

/// [`Event::CqueryEdge`]'s payload.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(crate) struct CqueryEdge {
    pub(crate) from: ConfiguredTargetKey,
    pub(crate) to: Label,
    pub(crate) config: ConfigId,
    pub(crate) attribute: String,
    /// A tool (exec) dependency.
    pub(crate) tool: bool,
    /// An aspect's dependency.
    pub(crate) aspect: bool,
}

/// [`Event::ToolchainImpl`]'s payload.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(crate) struct ToolchainImpl {
    pub(crate) implementation: ConfiguredTargetKey,
    pub(crate) exec_platform: Label,
    /// Where a conflict is reported (the consumer's location; S2: pass 1
    /// decides the conflict and words the walk's error).
    pub(crate) location: Option<String>,
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

/// A top-level part of phase 1 (`plan_build`/`analyze_requested`): each
/// starts with an empty active chain.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(crate) enum SegmentKind {
    /// Before the requested targets: the explicit `--platforms` entry's
    /// resolution.
    Setup,
    /// The requested label at `position` (`track_root_entry`,
    /// `track_root_analysis`, `keep_going_state.current`); a cycle through
    /// `trimmed_root` prints its untrimmed key first (`cycle_error`).
    Root {
        position: usize,
        label: Label,
        trimmed_root: Option<ConfiguredTargetKey>,
    },
    /// Every requested label analyzed (`track_root_entry(labels.len())`).
    RootsDone,
    /// The top-level aspects' applications on a requested label.
    TopLevelAspects { label: Label },
}

/// One step of the tape (pass 1's output; pass 2 streams it). Owned by C:
/// extended in S1 with what the linearizer-only state needs (the tracker's
/// frames, keep-going edges, segment boundaries).
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(crate) enum TapeOp {
    /// A top-level segment begins (its active chain is empty).
    Segment(SegmentKind),
    /// A CT1 node is reached (`track_key`, `record_edge(parent, key)`);
    /// `enter`: not a memo hit, its frame begins (`track_enter`,
    /// `track_entry(plan length)`) and a `Leave` ends it.
    Reach {
        node: NodeId,
        key: ConfiguredTargetKey,
        parent: Option<ConfiguredTargetKey>,
        enter: bool,
    },
    /// The entered reach failed before its body (pass 1's cycle or depth
    /// check): the error the reacher receives.
    Fail(String),
    /// The innermost entered reach ends (`track_restore`).
    Leave,
    /// Emit a phase-1 effect at its linearized position.
    Effect(Event),
    /// The plan index of a configured target (pass 1's order); `position`:
    /// the tracker records its plan position (`track_position`; not for an
    /// incompatible target).
    Plan { key: CtKey, position: bool },
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
    pub(crate) fn plan(&mut self, key: CtKey, position: bool) -> usize {
        self.ops.push(TapeOp::Plan { key, position });
        self.planned += 1;
        self.planned - 1
    }

    pub(crate) fn push(&mut self, op: TapeOp) {
        debug_assert!(!matches!(op, TapeOp::Plan { .. }), "Tape::plan counts");
        self.ops.push(op);
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

impl TapeOp {
    /// A digest of the op (the recorder keeps the walk's tape as digests).
    pub(crate) fn digest(&self) -> u64 {
        use std::hash::BuildHasher;

        crate::fast_hash::FxBuildHasher::default().hash_one(self)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn transcript_reaches_in_order_and_tape_counts_plan_entries() {
        assert!(
            std::mem::size_of::<Event>() <= 80,
            "{}",
            std::mem::size_of::<Event>()
        );
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
        assert_eq!(tape.plan(key.clone(), true), 0);
        tape.flush();
        assert_eq!(tape.plan(key, false), 1);
        assert_eq!(tape.planned(), 2);
        assert_eq!(tape.ops.len(), 4);
    }
}
