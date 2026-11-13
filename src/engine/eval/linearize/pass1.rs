//! Pass 1 (docs/EVALUATOR.md "Determinism: pass 1 and pass 2"): replay the
//! transcripts depth first from the top-level segments — exactly the
//! sequential walk's DFS — in O(nodes + edges), without Starlark.
//!
//! **Resumable**: transcripts may still be growing (the evaluation is
//! running: a segment, a body or a memo not finished yet). Pass 1 then
//! stops at the end of what exists ([`Progress::Blocked`]) and continues
//! from there on the next [`Pass1::resume`] — the S1 driver runs it up to
//! each flush (whose phase 2 needs the plan so far) and at the end. Its
//! state is an explicit stack of frames (no recursion: chains are deep).
//!
//! What the walk decides by its order, pass 1 decides the same way:
//!
//! - **Reach** (`analyze_key`): a reached key already complete
//!   (`configured`, here `visited`, set at the body's [`Event::Done`]) is a
//!   memo hit; otherwise its frame begins, the active chain decides a cycle
//!   (`cycle_error`, the text the walk prints, through
//!   [`Transcripts::cycle_error`]) or the depth limit (256), and the body's
//!   transcript is replayed with the key pushed on the active chain. A
//!   failed body (no `Done`) leaves the chain as the walk's `?` does, and a
//!   revisit replays its transcript again (the walk re-runs it; the graph
//!   memoizes the failure).
//! - **First-visit rule**: two variants of one ConfiguredTargetKey (CtKey's
//!   `platform_tree`/`forced_exec`): the first reached is the configured
//!   node; a later reach of another variant is a memo hit whose observable
//!   value must agree ([`Transcripts::same_observable`], a divergence
//!   otherwise). A reached variant's context facts are checked against pass
//!   1's own state (inside a platform's tree; the first-visit forced
//!   execution platform).
//! - **MemoUse**: a memo node's transcript is replayed at its first use
//!   (a `Transition`'s print() lines again at every later use); a
//!   platform/registration memo's reaches each start a fresh active chain
//!   (the walk's `&mut Vec::new()` per registered target), a `Platform`
//!   memo's inside a platform's tree.
//! - **PkgCommit**, **Warn**, **Console**: first occurrence only (a
//!   package's load prints at its first commit; an identical warning is
//!   reported once, EmittedEventState, warnings reported before phase 1
//!   included; a captured console write is written once).
//! - **ToolchainImpl**: the first execution platform an implementation is
//!   needed for wins; a different one later is the walk's conflict error.
//! - **PlanPush**: the body's plan index; **Flush**: a flush marker.
//!
//! Everything else is copied to the tape at its position.

use std::collections::{BTreeMap, BTreeSet};

use crate::fast_hash::{FxHashMap, FxHashSet};

use crate::configuration::ConfigId;
use crate::label::{Label, PackageId};

use super::super::super::ConfiguredTargetKey;
use super::super::key::{CtKey, NodeId, NodeKey};
#[cfg(test)]
use super::super::transcript::ToolchainImpl;
use super::super::transcript::{Event, SegmentKind, Tape, TapeOp, Transcript};

/// The walk's dependency depth limit error (`analyze_key_body`).
pub(crate) const DEPTH_LIMIT_ERROR: &str = "native prototype dependency depth limit (256) exceeded";

/// The node of a configured target analyzed outside the transcripts (a
/// deferred target platform's tree that a flush's phase 2 analyzed with
/// the walk): its consumers read the walk's value.
pub(crate) const EXTERNAL: NodeId = NodeId(u32::MAX);

/// The walk's active-chain depth limit.
const DEPTH_LIMIT: usize = 256;

/// A CT1 body's transcript as far as it goes.
pub(crate) struct Ct1Record<'a> {
    pub(crate) events: &'a [Event],
    /// The body finished (its value exists; `events` is all of it).
    pub(crate) complete: bool,
    /// The owner a rule transition delegated the key to
    /// (`rule_delegations`; from the value).
    pub(crate) delegate: Option<&'a ConfiguredTargetKey>,
    /// The body entered the empty configuration (`empty_configuration_rules`).
    pub(crate) empty_configuration: bool,
    /// The target is incompatible (`publish_incompatible`: its plan entry
    /// records no tracker position).
    pub(crate) incompatible: bool,
}

/// A memo node's (first) fill, as far as it goes.
pub(crate) struct MemoRecord<'a> {
    pub(crate) events: &'a [Event],
    pub(crate) complete: bool,
    pub(crate) ok: bool,
}

/// A top-level segment, as far as it goes.
pub(crate) struct SegmentRecord<'a> {
    pub(crate) kind: &'a SegmentKind,
    pub(crate) events: &'a [Event],
    pub(crate) complete: bool,
}

/// What pass 1 reads: the node keys, the transcripts, and the engine's
/// error texts.
pub(crate) trait Transcripts {
    /// The key of `node`.
    fn key(&self, node: NodeId) -> &NodeKey;
    /// A node's events as far as they go and whether complete (the hot
    /// path of the replay: [`Self::ct1`] / [`Self::memo`] without their
    /// value's fields).
    fn events(&self, node: NodeId, memo: bool) -> Option<(&[Event], bool)> {
        if memo {
            self.memo(node)
                .map(|record| (record.events, record.complete))
        } else {
            self.ct1(node)
                .map(|record| (record.events, record.complete))
        }
    }
    /// A CT1 node's transcript (`None`: the node never ran — an internal
    /// error when pass 1 enters it).
    fn ct1(&self, node: NodeId) -> Option<Ct1Record<'_>>;
    /// A memo node's transcript (`None`: no effects).
    fn memo(&self, node: NodeId) -> Option<MemoRecord<'_>>;
    /// The top-level segment at `index` (`None`: none (yet)).
    fn segment(&self, index: usize) -> Option<SegmentRecord<'_>>;
    /// No further segment will come.
    fn segments_complete(&self) -> bool;
    /// The evaluation is over: a node without a transcript now never gets
    /// one (before, pass 1 waits for it: nodes finish in any order).
    fn evaluation_done(&self) -> bool {
        self.segments_complete()
    }
    /// Whether `node`'s value is done (a memo hit of another variant
    /// compares values: pass 1 waits for it).
    fn ready(&self, node: NodeId) -> bool {
        let _ = node;
        true
    }
    /// Whether the reached variant's observable value (providers,
    /// incompatibility) equals the canonical variant's; unknown: equal.
    fn same_observable(&self, canonical: NodeId, reached: NodeId) -> bool {
        let _ = (canonical, reached);
        true
    }
    /// `Engine::cycle_error` for a reach of `head` while it is active.
    fn cycle_error(
        &self,
        head: &ConfiguredTargetKey,
        path: &[ConfiguredTargetKey],
        cycle: &[ConfiguredTargetKey],
        trimmed_root: Option<&ConfiguredTargetKey>,
    ) -> String;
}

/// A finished top-level segment (tests, complete sources).
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Segment {
    pub(crate) kind: SegmentKind,
    pub(crate) transcript: Transcript,
}

/// Pass 1's inputs besides the transcripts.
#[derive(Default)]
pub(crate) struct Options {
    /// The warnings reported before phase 1 (an identical phase-1 warning
    /// is not repeated).
    pub(crate) warnings: Vec<String>,
    /// Do not record [`Linearized::edges`] (only `--keep_going` reads
    /// them; the evaluator does not plan it).
    pub(crate) no_edges: bool,
}

/// Pass 1's result.
#[derive(Debug, Default)]
pub(crate) struct Linearized {
    pub(crate) tape: Tape,
    /// The plan, in order.
    pub(crate) plan: Vec<CtKey>,
    /// `rule_delegations`.
    pub(crate) delegations: BTreeMap<ConfiguredTargetKey, ConfiguredTargetKey>,
    /// `empty_configuration_rules`.
    pub(crate) empty_configuration: BTreeSet<ConfiguredTargetKey>,
    /// Every reach's `(active parent, key)`, in order (the keep-going
    /// edges; a `None` parent ends a segment's root search).
    pub(crate) edges: Vec<(Option<ConfiguredTargetKey>, ConfiguredTargetKey)>,
    /// `toolchain_implementations`: the first-visit forced platforms.
    pub(crate) toolchain_implementations: BTreeMap<ConfiguredTargetKey, Label>,
    /// The phase-1 warnings, deduplicated, in order.
    pub(crate) warnings: Vec<String>,
    /// The errors pass 1 itself decided (cycles, the depth limit,
    /// toolchain implementation conflicts), in order.
    pub(crate) failures: Vec<String>,
    /// The active chain's length when each segment ended (keep-going: a
    /// failed root whose chain is longer than one failed below itself).
    pub(crate) segment_active: Vec<usize>,
    /// The first configuration whose registered toolchains were computed
    /// successfully (`root_checkpoint.first`).
    pub(crate) first_toolchains: Option<ConfigId>,
    /// Context divergences: a body read a value or a context the
    /// sequential walk would not have served it (internal errors).
    pub(crate) divergences: Vec<String>,
    /// The error the walk stops at, decided by pass 1 (a toolchain
    /// implementation needed for two execution platforms: the walk's
    /// second claim in DFS order fails); nothing after it is replayed.
    pub(crate) stop: Option<String>,
}

/// The walk's conflict error of `toolchain_implementation`.
pub(crate) fn toolchain_conflict(
    implementation: &Label,
    existing: &Label,
    exec_platform: &Label,
) -> String {
    format!(
        "toolchain implementation {} is needed for the execution platforms {} and {} in one configuration: configured targets keyed by their execution platform (ConfiguredTargetKey.executionPlatformLabel) are not implemented in the native profile",
        crate::rule_declarations::canonical_form(implementation),
        crate::rule_declarations::canonical_form(existing),
        crate::rule_declarations::canonical_form(exec_platform),
    )
}

/// Where pass 1 writes its tape, op by op (a [`Tape`], or a streaming
/// consumer: the S1 driver's applier, pass 2).
pub(crate) trait TapeSink {
    fn op(&mut self, op: TapeOp);
}

impl TapeSink for Tape {
    fn op(&mut self, op: TapeOp) {
        match op {
            TapeOp::Plan {
                key,
                node,
                position,
            } => {
                self.plan(key, node, position);
            }
            op => self.push(op),
        }
    }
}

impl TapeSink for Vec<TapeOp> {
    fn op(&mut self, op: TapeOp) {
        self.push(op);
    }
}

/// How far [`Pass1::resume`] got.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Progress {
    /// Every segment is replayed to its end.
    Finished,
    /// A transcript still being written ends here: resume later.
    Blocked,
    /// The walk fails here ([`Linearized::stop`]): the replay ends.
    Stopped,
}

/// What [`Pass1::resume`] stopped at (the evaluator's frontier: the
/// driver boosts it).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Blocker {
    /// The next top-level segment, or the current one's next event.
    Segment,
    /// A node still running, or not run yet.
    Node(NodeId),
}

/// One replay in progress.
enum Frame {
    Segment {
        index: usize,
        pos: usize,
    },
    Body {
        node: NodeId,
        ct: CtKey,
        pos: usize,
        /// Its `Done` passed: never replayed again (its transcript can go).
        done: bool,
    },
    Memo {
        node: NodeId,
        pos: usize,
        /// A platform/registration memo (fresh chain per reach).
        fresh: bool,
        platform: bool,
    },
}

/// Pass 1's state.
pub(crate) struct Pass1 {
    out: Linearized,
    /// Complete configured keys and their canonical node (`configured`).
    visited: FxHashMap<ConfiguredTargetKey, NodeId>,
    /// `visited` by the node reached (its variant): the canonical node of
    /// the node's key once complete, read without hashing the key.
    canonical_by_node: Vec<Option<NodeId>>,
    /// Whether `canonical_by_node` is kept ([`Pass1::index_by_node`]).
    by_node: bool,
    /// By node (with `by_node`): a variant found to observe what its key's
    /// canonical node observes (the comparison is not made again).
    same_by_node: Vec<bool>,
    /// The plan's length.
    planned: usize,
    /// [`Pass1::plan_on_tape`].
    tape_plan: bool,
    /// The active chains, innermost (a memo's fresh chain) last.
    chains: Vec<Vec<ConfiguredTargetKey>>,
    memos: FxHashSet<NodeId>,
    packages: FxHashSet<PackageId>,
    consoles: FxHashSet<u32>,
    warned: FxHashSet<String>,
    /// [`Options::no_edges`].
    no_edges: bool,
    /// The toolchain implementations added since the driver last took them.
    new_implementations: Vec<ConfiguredTargetKey>,
    /// At most this many events per [`Pass1::resume`] (then it returns
    /// `Blocked` with no blocker: the caller resumes later).
    budget: Option<usize>,
    /// With a budget: checked every [`YIELD_EVERY`] events, a resume
    /// pauses early when it answers true (another thread waits for the
    /// caller's lock).
    yield_to: Option<fn() -> bool>,
    /// The last resume stopped at its budget.
    paused: bool,
    /// Platform memos being replayed (`resolving_platforms`).
    platform_depth: usize,
    trimmed_root: Option<ConfiguredTargetKey>,
    frames: Vec<Frame>,
    next_segment: usize,
    /// Where the last [`Pass1::resume`] stopped.
    blocked: Option<Blocker>,
    /// Transcripts replayed for the last time (a body past its `Done`, a
    /// memo): the source may free them ([`Pass1::take_released`]).
    released: Vec<NodeId>,
    /// Segments replayed to their end (likewise).
    released_segments: Vec<usize>,
    /// Nodes that became their key's configured target (first reach in
    /// DFS order, at their `Done`), since the last [`Pass1::take_canonical`].
    canonical: Vec<NodeId>,
    /// The memo scopes open (`MemoBegin`): their memo and whether skipped.
    scopes: Vec<(u64, bool)>,
    /// Skipped scopes open (a later use of a computed memo).
    skipping: usize,
    /// The memos computed (first use), with their value digest.
    memo_values: FxHashMap<u64, u64>,
    /// Where each `Flush` of the tape not taken yet happens
    /// ([`Pass1::take_flush_point`]).
    flush_points: std::collections::VecDeque<FlushPoint>,
}

/// Where a `Flush` happens in the walk: its segment and the replayed
/// chain (configured targets and memos, outermost first; the last one
/// emitted it).
#[derive(Clone, Debug)]
pub(crate) struct FlushPoint {
    pub(crate) kind: Option<SegmentKind>,
    pub(crate) chain: Vec<NodeId>,
}

/// How often a budgeted resume asks its `yield_to` (events).
pub(crate) const YIELD_EVERY: usize = 512;

/// Run pass 1 over finished `segments` (tests and complete sources).
#[cfg(test)]
pub(crate) fn run<T: Transcripts + ?Sized>(
    source: &T,
    segments: &[Segment],
    options: Options,
) -> Result<Linearized, String> {
    struct Complete<'a, T: ?Sized> {
        source: &'a T,
        segments: &'a [Segment],
    }
    impl<T: Transcripts + ?Sized> Transcripts for Complete<'_, T> {
        fn key(&self, node: NodeId) -> &NodeKey {
            self.source.key(node)
        }
        fn ct1(&self, node: NodeId) -> Option<Ct1Record<'_>> {
            self.source.ct1(node)
        }
        fn memo(&self, node: NodeId) -> Option<MemoRecord<'_>> {
            self.source.memo(node)
        }
        fn segment(&self, index: usize) -> Option<SegmentRecord<'_>> {
            self.segments.get(index).map(|segment| SegmentRecord {
                kind: &segment.kind,
                events: &segment.transcript.events,
                complete: true,
            })
        }
        fn segments_complete(&self) -> bool {
            true
        }
        fn same_observable(&self, canonical: NodeId, reached: NodeId) -> bool {
            self.source.same_observable(canonical, reached)
        }
        fn cycle_error(
            &self,
            head: &ConfiguredTargetKey,
            path: &[ConfiguredTargetKey],
            cycle: &[ConfiguredTargetKey],
            trimmed_root: Option<&ConfiguredTargetKey>,
        ) -> String {
            self.source.cycle_error(head, path, cycle, trimmed_root)
        }
    }
    let source = Complete { source, segments };
    let mut pass = Pass1::new(options);
    let mut tape = Tape::default();
    match pass.resume(&source, &mut tape)? {
        Progress::Finished | Progress::Stopped => {}
        Progress::Blocked => {
            return Err("internal error: pass 1 blocked on a complete source".into());
        }
    }
    let mut out = pass.finish();
    out.tape = tape;
    Ok(out)
}

impl Pass1 {
    pub(crate) fn new(options: Options) -> Self {
        Self {
            out: Linearized::default(),
            visited: FxHashMap::default(),
            canonical_by_node: Vec::new(),
            by_node: false,
            same_by_node: Vec::new(),
            planned: 0,
            tape_plan: false,
            chains: Vec::new(),
            memos: FxHashSet::default(),
            packages: FxHashSet::default(),
            consoles: FxHashSet::default(),
            no_edges: options.no_edges,
            new_implementations: Vec::new(),
            budget: None,
            yield_to: None,
            paused: false,
            warned: options.warnings.into_iter().collect(),
            platform_depth: 0,
            trimmed_root: None,
            frames: Vec::new(),
            next_segment: 0,
            blocked: None,
            released: Vec::new(),
            released_segments: Vec::new(),
            canonical: Vec::new(),
            scopes: Vec::new(),
            skipping: 0,
            memo_values: FxHashMap::default(),
            flush_points: std::collections::VecDeque::new(),
        }
    }

    /// Where the tape's next `Flush` (in order) happens.
    pub(crate) fn take_flush_point(&mut self) -> Option<FlushPoint> {
        self.flush_points.pop_front()
    }

    /// The nodes that became configured targets since the last call (the
    /// driver publishes their values), in DFS order.
    pub(crate) fn take_canonical(&mut self) -> Vec<NodeId> {
        std::mem::take(&mut self.canonical)
    }

    /// `toolchain_implementations` so far.
    pub(crate) fn toolchain_implementations(&self) -> &BTreeMap<ConfiguredTargetKey, Label> {
        &self.out.toolchain_implementations
    }

    /// The `toolchain_implementations` entries added since the last call.
    pub(crate) fn take_new_toolchain_implementations(&mut self) -> Vec<ConfiguredTargetKey> {
        std::mem::take(&mut self.new_implementations)
    }

    /// The walk's error pass 1 stopped at.
    pub(crate) fn stopped(&self) -> Option<&str> {
        self.out.stop.as_deref()
    }

    /// What the last [`Pass1::resume`] is waiting for (`None`: finished
    /// or not started).
    pub(crate) fn blocked_on(&self) -> Option<Blocker> {
        self.blocked
    }

    /// The nodes and segments whose transcripts pass 1 will not read
    /// again, since the last call.
    pub(crate) fn take_released(&mut self) -> (Vec<NodeId>, Vec<usize>) {
        (
            std::mem::take(&mut self.released),
            std::mem::take(&mut self.released_segments),
        )
    }

    /// The plan so far (empty with [`Self::plan_on_tape`]).
    pub(crate) fn plan(&self) -> &[CtKey] {
        &self.out.plan
    }

    /// The plan's length so far.
    pub(crate) fn planned(&self) -> usize {
        self.planned
    }

    /// The plan goes to the tape only ([`Linearized::plan`] stays empty:
    /// the driver keeps its own).
    pub(crate) fn plan_on_tape(&mut self) {
        self.tape_plan = true;
    }

    /// Room for `count` complete configured keys in all.
    pub(crate) fn reserve(&mut self, count: usize) {
        if self.visited.capacity() < count {
            self.visited.reserve(count - self.visited.len());
        }
    }

    /// A configured key the walk analyzed outside phase 1's transcripts
    /// (a deferred target platform's tree planned by a flush's phase 2):
    /// complete from now on, and in the plan where the flush put it.
    pub(crate) fn external(&mut self, key: ConfiguredTargetKey, node: NodeId, planned: bool) {
        self.visit(key.clone(), node);
        if planned {
            self.planned += 1;
            if !self.tape_plan {
                self.out.plan.push(CtKey::plain(key));
            }
        }
    }

    /// The result (the tape went to the sink).
    pub(crate) fn finish(self) -> Linearized {
        self.out
    }

    fn chain(&mut self) -> &mut Vec<ConfiguredTargetKey> {
        self.chains.last_mut().expect("a segment's chain")
    }

    /// Keep the canonical nodes by the node reached (a table as large as
    /// the node ids: for a pass over the whole evaluation, not a subtree's).
    pub(crate) fn index_by_node(&mut self) {
        self.by_node = true;
    }

    /// Bound the events of each [`Self::resume`] (`None`: no bound).
    pub(crate) fn set_budget(&mut self, budget: Option<usize>) {
        self.budget = budget;
    }

    /// With a budget, pause early when `yield_to` answers true (checked
    /// every [`YIELD_EVERY`] events).
    pub(crate) fn set_yield(&mut self, yield_to: Option<fn() -> bool>) {
        self.yield_to = yield_to;
    }

    /// Whether resumes are budgeted.
    pub(crate) fn stepping(&self) -> bool {
        self.budget.is_some()
    }

    /// Whether the last [`Self::resume`] stopped at its budget.
    pub(crate) fn paused(&self) -> bool {
        self.paused
    }

    /// Continue the replay as far as the transcripts go.
    pub(crate) fn resume<T: Transcripts + ?Sized>(
        &mut self,
        source: &T,
        sink: &mut dyn TapeSink,
    ) -> Result<Progress, String> {
        self.blocked = None;
        self.paused = false;
        let mut budget = self.budget.unwrap_or(usize::MAX);
        let yield_to = self.yield_to.filter(|_| self.budget.is_some());
        loop {
            if budget == 0
                || (budget.is_multiple_of(YIELD_EVERY)
                    && yield_to.is_some_and(|yield_to| yield_to()))
            {
                self.paused = true;
                return Ok(Progress::Blocked);
            }
            budget -= 1;
            if self.out.stop.is_some() {
                return Ok(Progress::Stopped);
            }
            let Some(frame) = self.frames.last_mut() else {
                // Between segments.
                let Some(segment) = source.segment(self.next_segment) else {
                    if source.segments_complete() {
                        return Ok(Progress::Finished);
                    }
                    self.blocked = Some(Blocker::Segment);
                    return Ok(Progress::Blocked);
                };
                sink.op(TapeOp::Segment(segment.kind.clone()));
                self.trimmed_root = match segment.kind {
                    SegmentKind::Root { trimmed_root, .. } => trimmed_root.clone(),
                    _ => None,
                };
                self.chains = vec![Vec::new()];
                self.frames.push(Frame::Segment {
                    index: self.next_segment,
                    pos: 0,
                });
                self.next_segment += 1;
                continue;
            };
            // The next event of the innermost frame, or its end.
            let (event, complete, owner) = match frame {
                Frame::Segment { index, pos } => {
                    let segment = source
                        .segment(*index)
                        .ok_or("internal error: pass 1: a segment vanished")?;
                    (segment.events.get(*pos), segment.complete, Blocker::Segment)
                }
                Frame::Body { node, pos, .. } => match source.events(*node, false) {
                    Some((events, complete)) => (events.get(*pos), complete, Blocker::Node(*node)),
                    // Between a rerun's start and its end (nodes finish in
                    // any order): wait for it.
                    None if !source.evaluation_done() => (None, false, Blocker::Node(*node)),
                    None => return Err("internal error: pass 1: a body vanished".to_owned()),
                },
                Frame::Memo { node, pos, .. } => match source.events(*node, true) {
                    Some((events, complete)) => (events.get(*pos), complete, Blocker::Node(*node)),
                    // A memo replayed in part whose rerun runs: wait for it
                    // (its effects are not all known yet).
                    None => (None, source.evaluation_done(), Blocker::Node(*node)),
                },
            };
            let depth = self.frames.len() - 1;
            match event {
                Some(event) => match self.event(source, sink, event, complete)? {
                    None => self.advance(depth),
                    Some(blocker) => {
                        self.blocked = Some(blocker);
                        return Ok(Progress::Blocked);
                    }
                },
                None if !complete => {
                    self.blocked = Some(owner);
                    return Ok(Progress::Blocked);
                }
                None => self.pop(source, sink),
            }
        }
    }

    /// The event at `frames[depth]`'s position was consumed (it may have
    /// pushed a frame above it).
    fn advance(&mut self, depth: usize) {
        match &mut self.frames[depth] {
            Frame::Segment { pos, .. } | Frame::Body { pos, .. } | Frame::Memo { pos, .. } => {
                *pos += 1;
            }
        }
    }

    /// The innermost frame's transcript is replayed to its end.
    fn pop<T: Transcripts + ?Sized>(&mut self, source: &T, sink: &mut dyn TapeSink) {
        match self.frames.pop().expect("a frame") {
            Frame::Segment { index, .. } => {
                self.out.segment_active.push(self.chains[0].len());
                self.trimmed_root = None;
                self.chains.clear();
                self.released_segments.push(index);
            }
            Frame::Body { node, ct, done, .. } => {
                if let Some(record) = source.ct1(node) {
                    if record.empty_configuration {
                        self.out.empty_configuration.insert(ct.key.clone());
                    }
                    // `rule_delegations` (the value is complete here).
                    if done && let Some(delegate) = record.delegate {
                        self.out.delegations.insert(ct.key, delegate.clone());
                    }
                }
                if done {
                    self.released.push(node);
                }
                sink.op(TapeOp::Leave);
            }
            Frame::Memo {
                node,
                fresh,
                platform,
                ..
            } => {
                // A transition's transcript prints again at every use.
                if !matches!(source.key(node), NodeKey::Transition(_)) {
                    self.released.push(node);
                }
                if fresh {
                    self.chains.pop();
                }
                self.platform_depth -= usize::from(platform);
                if let NodeKey::RegToolchains(config) = source.key(node)
                    && source.memo(node).is_some_and(|record| record.ok)
                {
                    self.out.first_toolchains.get_or_insert(*config);
                }
            }
        }
    }

    /// The body the innermost frames replay (for its `Done`/`PlanPush`).
    fn owner(&self) -> Option<(NodeId, CtKey)> {
        match self.frames.last()? {
            Frame::Body { node, ct, .. } => Some((*node, ct.clone())),
            _ => None,
        }
    }

    /// The innermost frame is a fresh-chain memo's.
    fn in_fresh_memo(&self) -> bool {
        matches!(self.frames.last(), Some(Frame::Memo { fresh: true, .. }))
    }

    /// Replay `event` of the innermost frame (`complete`: its transcript
    /// is complete); `Some`: not consumed, pass 1 waits for the blocker.
    fn event<T: Transcripts + ?Sized>(
        &mut self,
        source: &T,
        sink: &mut dyn TapeSink,
        event: &Event,
        complete: bool,
    ) -> Result<Option<Blocker>, String> {
        // A memo scope (the walk computes the memo at its first use only).
        match *event {
            Event::MemoBegin(memo) => {
                let skip = self.skipping > 0 || self.memo_values.contains_key(&memo);
                self.skipping += usize::from(skip);
                self.scopes.push((memo, skip));
                return Ok(None);
            }
            Event::MemoEnd(ref value) => {
                let Some((memo, skipped)) = self.scopes.pop() else {
                    return Err("internal error: pass 1: a memo scope ends unopened".to_owned());
                };
                if skipped {
                    self.skipping -= 1;
                    if self.skipping == 0
                        && let Some(value) = *value
                        && self.memo_values.get(&memo) != Some(&value)
                    {
                        self.out.divergences.push(format!(
                            "a first-visit memo (an aspect's, {memo:x}) computed again gives another value than at its first use"
                        ));
                    }
                } else if let Some(value) = *value {
                    self.memo_values.entry(memo).or_insert(value);
                }
                return Ok(None);
            }
            // A later use: the walk's memo hit has none of the events.
            _ if self.skipping > 0 => return Ok(None),
            _ => {}
        }
        match *event {
            Event::Reach(node) => {
                let NodeKey::Ct1(ct) = source.key(node) else {
                    return Err(format!(
                        "internal error: pass 1: a Reach of the non-CT1 node {:?}",
                        source.key(node)
                    ));
                };
                let canonical = self.canonical(node, &ct.key);
                if let Some(blocker) = self.reach_blocker(source, node, ct, canonical) {
                    return Ok(Some(blocker));
                }
                if self.in_fresh_memo() {
                    self.chain().clear();
                }
                self.reach(source, sink, node, ct, canonical)?;
            }
            Event::PkgCommit(ref package, _) => {
                if self.packages.insert(package.clone()) {
                    sink.op(TapeOp::Effect(event.clone()));
                }
            }
            Event::Console(index) => {
                if self.consoles.insert(index) {
                    sink.op(TapeOp::Effect(event.clone()));
                }
            }
            Event::MemoUse(node) => {
                if !self.memos.contains(&node)
                    && source.memo(node).is_none()
                    && !source.evaluation_done()
                {
                    // Not run yet (nodes finish in any order).
                    return Ok(Some(Blocker::Node(node)));
                }
                self.memo_use(source, sink, node);
            }
            Event::Warn(ref warning) => {
                if self.warned.insert(warning.clone()) {
                    self.out.warnings.push(warning.clone());
                    sink.op(TapeOp::Effect(event.clone()));
                }
            }
            Event::ToolchainImpl(ref needed) => {
                let (implementation, exec_platform) =
                    (&needed.implementation, &needed.exec_platform);
                match self.out.toolchain_implementations.get(implementation) {
                    Some(existing) if existing != exec_platform => {
                        // The walk's `toolchain_implementation` fails here
                        // (at the consumer's location) and the build stops.
                        let error = super::super::super::located(
                            needed.location.as_deref(),
                            toolchain_conflict(&implementation.label, existing, exec_platform),
                        );
                        self.out.failures.push(error.clone());
                        self.out.stop = Some(error);
                        return Ok(None);
                    }
                    Some(_) => {}
                    None => {
                        self.out
                            .toolchain_implementations
                            .insert(implementation.clone(), exec_platform.clone());
                        self.new_implementations.push(implementation.clone());
                    }
                }
                sink.op(TapeOp::Effect(event.clone()));
            }
            Event::PlanPush => {
                let Some((node, ct)) = self.owner() else {
                    return Err("internal error: pass 1: a plan entry outside a body".to_owned());
                };
                if !complete {
                    // The entry reads the finished value (its incompatibility).
                    return Ok(Some(Blocker::Node(node)));
                }
                let incompatible = source.ct1(node).is_some_and(|record| record.incompatible);
                self.planned += 1;
                if !self.tape_plan {
                    self.out.plan.push(ct.clone());
                }
                sink.op(TapeOp::Plan {
                    key: ct,
                    node,
                    position: !incompatible,
                });
            }
            Event::Done => {
                if let Some((node, ct)) = self.owner() {
                    // The walk pops the top of the chain (the key's own
                    // entry unless a caught failure left more).
                    self.chain().pop();
                    self.visit(ct.key, node);
                    self.canonical.push(node);
                    // Its delegation is recorded when the frame ends (the
                    // value is complete there).
                    if let Some(Frame::Body { done, .. }) = self.frames.last_mut() {
                        *done = true;
                    }
                }
                sink.op(TapeOp::Effect(Event::Done));
            }
            Event::Flush => {
                // Where the walk's flush runs (its failure is reported up
                // this chain: the driver's, S2's CsMatch split).
                let kind = self.frames.iter().find_map(|frame| match frame {
                    Frame::Segment { index, .. } => {
                        source.segment(*index).map(|segment| segment.kind.clone())
                    }
                    _ => None,
                });
                let chain = self
                    .frames
                    .iter()
                    .filter_map(|frame| match frame {
                        Frame::Body { node, .. } | Frame::Memo { node, .. } => Some(*node),
                        Frame::Segment { .. } => None,
                    })
                    .collect();
                self.flush_points.push_back(FlushPoint { kind, chain });
                sink.op(TapeOp::Flush)
            }
            Event::Access(_)
            | Event::Source(_)
            | Event::Print(_)
            | Event::CqueryEdge(_)
            | Event::ScopeEnter(_)
            | Event::ScopeExit
            | Event::Continue(_)
            | Event::Info(_)
            | Event::LoadPrints(_)
            | Event::Error(_) => sink.op(TapeOp::Effect(event.clone())),
            Event::MemoBegin(_) | Event::MemoEnd(_) => unreachable!("handled above"),
        }
        Ok(None)
    }

    /// [`Transcripts::same_observable`], remembered by node for a variant
    /// that observes the same (with `by_node`: the canonical node of a key
    /// does not change).
    fn same_observable<T: Transcripts + ?Sized>(
        &mut self,
        source: &T,
        canonical: NodeId,
        node: NodeId,
    ) -> bool {
        let slot = node.0 as usize;
        if self.by_node && self.same_by_node.get(slot).copied().unwrap_or(false) {
            return true;
        }
        let same = source.same_observable(canonical, node);
        if same && self.by_node {
            if self.same_by_node.len() <= slot {
                self.same_by_node.resize(slot + 1, false);
            }
            self.same_by_node[slot] = true;
        }
        same
    }

    /// `key` complete as `node` (its canonical node, or [`EXTERNAL`]).
    fn visit(&mut self, key: ConfiguredTargetKey, node: NodeId) {
        if self
            .visited
            .insert(key, node)
            .is_some_and(|old| old != node)
        {
            // (A key's canonical node does not change; if it did, the
            // variants' cached reads would be stale.)
            self.canonical_by_node.clear();
            self.same_by_node.clear();
        }
    }

    /// The canonical node of `node`'s key `key` once complete
    /// (`visited`), cached by `node`.
    fn canonical(&mut self, node: NodeId, key: &ConfiguredTargetKey) -> Option<NodeId> {
        let slot = node.0 as usize;
        if let Some(Some(canonical)) = self.canonical_by_node.get(slot) {
            return Some(*canonical);
        }
        let canonical = self.visited.get(key).copied()?;
        if !self.by_node {
            return Some(canonical);
        }
        if self.canonical_by_node.len() <= slot {
            self.canonical_by_node.resize(slot + 1, None);
        }
        self.canonical_by_node[slot] = Some(canonical);
        Some(canonical)
    }

    /// What a reach of `node` must wait for before it is replayed: the
    /// node's transcript when it is entered, its value when it is a memo
    /// hit of another variant (the values are compared).
    fn reach_blocker<T: Transcripts + ?Sized>(
        &self,
        source: &T,
        node: NodeId,
        ct: &CtKey,
        canonical: Option<NodeId>,
    ) -> Option<Blocker> {
        if source.evaluation_done() {
            return None;
        }
        match canonical {
            Some(canonical) => (canonical != node && canonical != EXTERNAL && !source.ready(node))
                .then_some(Blocker::Node(node)),
            None => {
                let chain = self.chains.last()?;
                let fails = !self.in_fresh_memo()
                    && (chain.contains(&ct.key) || chain.len() >= DEPTH_LIMIT);
                (!fails && source.ct1(node).is_none()).then_some(Blocker::Node(node))
            }
        }
    }

    fn reach<T: Transcripts + ?Sized>(
        &mut self,
        source: &T,
        sink: &mut dyn TapeSink,
        node: NodeId,
        ct: &CtKey,
        canonical: Option<NodeId>,
    ) -> Result<(), String> {
        let key = &ct.key;
        if !self.no_edges {
            let parent = self.chain().last().cloned();
            self.out.edges.push((parent, key.clone()));
        }
        sink.op(TapeOp::Reach {
            node,
            enter: canonical.is_none(),
        });
        if let Some(canonical) = canonical {
            if canonical != node
                && canonical != EXTERNAL
                && !self.same_observable(source, canonical, node)
            {
                self.out.divergences.push(format!(
                    "{key}: a consumer read the variant {ct:?}, the sequential walk serves the first-visited {:?}",
                    source.key(canonical)
                ));
            }
            return Ok(());
        }
        if ct.platform_tree && self.platform_depth == 0 {
            self.out.divergences.push(format!(
                "{key}: analyzed as inside a platform's tree, reached outside every platform resolution"
            ));
        }
        if let Some(forced) = &ct.forced_exec
            && self.out.toolchain_implementations.get(key) != Some(forced)
        {
            self.out.divergences.push(format!(
                "{key}: analyzed with the forced execution platform {forced}, the first-visit one is {:?}",
                self.out.toolchain_implementations.get(key)
            ));
        }
        let chain = self.chains.last().expect("a segment's chain");
        if let Some(start) = chain.iter().position(|active| active == key) {
            let error = source.cycle_error(
                key,
                &chain[..start],
                &chain[start..],
                self.trimmed_root.as_ref(),
            );
            self.out.failures.push(error.clone());
            sink.op(TapeOp::Fail(error));
            sink.op(TapeOp::Leave);
            return Ok(());
        }
        if chain.len() >= DEPTH_LIMIT {
            // The walk fails there (the pool's bodies do not know their
            // depth): the build stops.
            self.out.stop = Some(DEPTH_LIMIT_ERROR.to_owned());
            self.out.failures.push(DEPTH_LIMIT_ERROR.to_owned());
            sink.op(TapeOp::Fail(DEPTH_LIMIT_ERROR.to_owned()));
            sink.op(TapeOp::Leave);
            return Ok(());
        }
        if source.ct1(node).is_none() {
            return Err(format!(
                "internal error: pass 1 enters {key}, which has no transcript"
            ));
        }
        if self
            .frames
            .iter()
            .any(|frame| matches!(frame, Frame::Body { node: running, .. } if *running == node))
        {
            return Err(format!(
                "internal error: pass 1 re-enters {key} while replaying it (a reach across a fresh chain)"
            ));
        }
        self.chain().push(key.clone());
        self.frames.push(Frame::Body {
            node,
            ct: ct.clone(),
            pos: 0,
            done: false,
        });
        Ok(())
    }

    fn memo_use<T: Transcripts + ?Sized>(
        &mut self,
        source: &T,
        sink: &mut dyn TapeSink,
        node: NodeId,
    ) {
        if !self.memos.insert(node) {
            // A Starlark transition applied again prints again (the walk's
            // StarlarkTransitionCache replays a hit's print() lines).
            if matches!(source.key(node), NodeKey::Transition(_))
                && let Some(record) = source.memo(node)
            {
                for event in record.events {
                    if let Event::Print(_) = event {
                        sink.op(TapeOp::Effect(event.clone()));
                    }
                }
            }
            return;
        }
        sink.op(TapeOp::Effect(Event::MemoUse(node)));
        if source.memo(node).is_none() {
            return;
        }
        let key = source.key(node);
        let platform = matches!(key, NodeKey::Platform(_));
        let fresh = matches!(
            key,
            NodeKey::Platform(_) | NodeKey::RegToolchains(_) | NodeKey::RegExecPlatforms(_)
        );
        self.platform_depth += usize::from(platform);
        if fresh {
            self.chains.push(Vec::new());
        }
        self.frames.push(Frame::Memo {
            node,
            pos: 0,
            fresh,
            platform,
        });
    }
}

#[cfg(test)]
pub(crate) mod tests {
    //! Pass 1 over synthetic transcripts.
    use super::*;
    use crate::engine::eval::key::Interner;
    use std::collections::{HashMap, HashSet};

    /// A body: transcript, delegate, empty configuration, incompatible.
    pub(crate) type FixtureBody = (Transcript, Option<ConfiguredTargetKey>, bool, bool);

    /// A map-backed transcript source.
    #[derive(Default)]
    pub(crate) struct Fixture {
        pub(crate) interner: Interner,
        pub(crate) bodies: HashMap<NodeId, Vec<FixtureBody>>,
        pub(crate) memos: HashMap<NodeId, (Transcript, bool)>,
        pub(crate) different: HashSet<(NodeId, NodeId)>,
        /// Bodies still being written (resumable runs).
        pub(crate) partial: HashSet<NodeId>,
        /// Segments for resumable runs.
        pub(crate) segments: Vec<Segment>,
        pub(crate) segments_done: bool,
        /// Streaming runs: the nodes run so far and how much of each
        /// transcript exists (`usize::MAX`: finished); others are hidden.
        pub(crate) visible: Option<HashMap<NodeId, usize>>,
        /// Transcripts pass 1 released (empty from then on).
        pub(crate) released: HashSet<NodeId>,
    }

    impl Fixture {
        /// `events` as far as visible, and whether complete.
        fn visible<'a>(&self, node: NodeId, events: &'a [Event]) -> Option<(&'a [Event], bool)> {
            if self.released.contains(&node) {
                return Some((&[], true));
            }
            match &self.visible {
                None => Some((events, true)),
                Some(visible) => {
                    let len = *visible.get(&node)?;
                    Some((&events[..len.min(events.len())], len == usize::MAX))
                }
            }
        }
    }

    impl Transcripts for Fixture {
        fn key(&self, node: NodeId) -> &NodeKey {
            self.interner.key(node)
        }
        fn ct1(&self, node: NodeId) -> Option<Ct1Record<'_>> {
            let (transcript, delegate, empty, incompatible) = self.bodies.get(&node)?.first()?;
            let (events, complete) = self.visible(node, &transcript.events)?;
            Some(Ct1Record {
                events,
                complete: complete && !self.partial.contains(&node),
                delegate: delegate.as_ref(),
                empty_configuration: *empty,
                incompatible: *incompatible,
            })
        }
        fn memo(&self, node: NodeId) -> Option<MemoRecord<'_>> {
            let (transcript, ok) = self.memos.get(&node)?;
            let (events, complete) = self.visible(node, &transcript.events)?;
            Some(MemoRecord {
                events,
                complete,
                ok: *ok,
            })
        }
        fn ready(&self, node: NodeId) -> bool {
            self.visible
                .as_ref()
                .is_none_or(|visible| visible.get(&node) == Some(&usize::MAX))
        }
        fn segment(&self, index: usize) -> Option<SegmentRecord<'_>> {
            self.segments.get(index).map(|segment| SegmentRecord {
                kind: &segment.kind,
                events: &segment.transcript.events,
                complete: index + 1 < self.segments.len() || self.segments_done,
            })
        }
        fn segments_complete(&self) -> bool {
            self.segments_done
        }
        fn same_observable(&self, canonical: NodeId, reached: NodeId) -> bool {
            !self.different.contains(&(canonical, reached))
        }
        fn cycle_error(
            &self,
            head: &ConfiguredTargetKey,
            path: &[ConfiguredTargetKey],
            cycle: &[ConfiguredTargetKey],
            trimmed_root: Option<&ConfiguredTargetKey>,
        ) -> String {
            let names = |keys: &[ConfiguredTargetKey]| {
                keys.iter()
                    .map(|key| key.label.name.clone())
                    .collect::<Vec<_>>()
                    .join(",")
            };
            format!(
                "cycle at {}: path [{}] cycle [{}] root {:?}",
                head.label.name,
                names(path),
                names(cycle),
                trimmed_root.map(|root| root.label.name.clone())
            )
        }
    }

    pub(crate) fn ctk(name: &str) -> ConfiguredTargetKey {
        ConfiguredTargetKey::new(
            Label::parse_canonical(&format!("//p:{name}")).unwrap(),
            crate::configuration::Configuration::default().id,
        )
    }

    impl Fixture {
        pub(crate) fn node(&self, name: &str) -> NodeId {
            self.interner.intern(&NodeKey::Ct1(CtKey::plain(ctk(name))))
        }

        /// A successful body: `events`, then Done and PlanPush.
        pub(crate) fn body(&mut self, name: &str, mut events: Vec<Event>) -> NodeId {
            let node = self.node(name);
            events.push(Event::Done);
            events.push(Event::PlanPush);
            self.bodies
                .entry(node)
                .or_default()
                .push((Transcript { events }, None, false, false));
            node
        }

        /// A failing body: `events`, then Error.
        pub(crate) fn failing(&mut self, name: &str, mut events: Vec<Event>) -> NodeId {
            let node = self.node(name);
            events.push(Event::Error(format!("{name} failed")));
            self.bodies
                .entry(node)
                .or_default()
                .push((Transcript { events }, None, false, false));
            node
        }
    }

    fn root(events: Vec<Event>) -> Vec<Segment> {
        vec![Segment {
            kind: SegmentKind::Root {
                position: 0,
                label: ctk("root").label,
                trimmed_root: None,
            },
            transcript: Transcript { events },
        }]
    }

    fn names(plan: &[CtKey]) -> Vec<String> {
        plan.iter().map(|key| key.key.label.name.clone()).collect()
    }

    #[test]
    fn first_visit_descends_and_later_reaches_are_memo_hits() {
        let mut f = Fixture::default();
        let c = f.body("c", vec![Event::Access(ctk("c").label)]);
        let b = f.body("b", vec![Event::Reach(c)]);
        let a = f.body("a", vec![Event::Reach(b), Event::Reach(c)]);
        let out = run(&f, &root(vec![Event::Reach(a)]), Options::default()).unwrap();
        assert_eq!(names(&out.plan), ["c", "b", "a"]);
        assert_eq!(
            out.edges,
            [
                (None, ctk("a")),
                (Some(ctk("a")), ctk("b")),
                (Some(ctk("b")), ctk("c")),
                (Some(ctk("a")), ctk("c")),
            ]
        );
        let enters: Vec<bool> = out
            .tape
            .ops
            .iter()
            .filter_map(|op| match op {
                TapeOp::Reach { enter, .. } => Some(*enter),
                _ => None,
            })
            .collect();
        assert_eq!(enters, [true, true, true, false]);
        let leaves = out
            .tape
            .ops
            .iter()
            .filter(|op| **op == TapeOp::Leave)
            .count();
        assert_eq!(leaves, 3);
        assert_eq!(out.segment_active, [0]);
        assert!(out.failures.is_empty() && out.divergences.is_empty());
    }

    #[test]
    fn memo_first_use_replays_once_in_a_fresh_chain() {
        let mut f = Fixture::default();
        let platform = f.interner.intern(&NodeKey::Platform(ctk("plat")));
        let registered = f.interner.intern(&NodeKey::RegToolchains(ctk("x").config));
        let p = f.body("plat", vec![]);
        let mut in_tree = CtKey::plain(ctk("leaf"));
        in_tree.platform_tree = true;
        let leaf = f.interner.intern(&NodeKey::Ct1(in_tree.clone()));
        f.bodies.insert(
            leaf,
            vec![(
                Transcript {
                    events: vec![Event::Done, Event::PlanPush],
                },
                None,
                true,
                false,
            )],
        );
        f.memos.insert(
            platform,
            (
                Transcript {
                    events: vec![
                        Event::Print("platform print".into()),
                        Event::Reach(p),
                        Event::Reach(leaf),
                    ],
                },
                true,
            ),
        );
        f.memos.insert(registered, (Transcript::default(), true));
        let a = f.body(
            "a",
            vec![
                Event::MemoUse(platform),
                Event::MemoUse(registered),
                Event::MemoUse(platform),
            ],
        );
        let out = run(&f, &root(vec![Event::Reach(a)]), Options::default()).unwrap();
        assert_eq!(names(&out.plan), ["plat", "leaf", "a"]);
        // The memo's reaches start a fresh chain: no parent.
        assert_eq!(out.edges[1], (None, ctk("plat")));
        assert_eq!(out.edges[2], (None, ctk("leaf")));
        let prints = out
            .tape
            .ops
            .iter()
            .filter(|op| matches!(op, TapeOp::Effect(Event::Print(_))))
            .count();
        assert_eq!(prints, 1);
        assert_eq!(out.first_toolchains, Some(ctk("x").config));
        assert_eq!(out.empty_configuration.len(), 1);
        assert!(out.divergences.is_empty(), "{:?}", out.divergences);
    }

    #[test]
    fn each_reach_of_a_memo_starts_a_fresh_chain() {
        let mut f = Fixture::default();
        let registered = f.interner.intern(&NodeKey::RegToolchains(ctk("x").config));
        let failing = f.failing("bad", vec![]);
        let good = f.body("good", vec![]);
        f.memos.insert(
            registered,
            (
                Transcript {
                    events: vec![Event::Reach(failing), Event::Reach(good)],
                },
                false,
            ),
        );
        let a = f.body("a", vec![Event::MemoUse(registered)]);
        let out = run(&f, &root(vec![Event::Reach(a)]), Options::default()).unwrap();
        // The failed body stays on its chain, but the next registered
        // target's chain is fresh.
        assert_eq!(out.edges[2], (None, ctk("good")));
        assert_eq!(out.first_toolchains, None);
    }

    #[test]
    fn platform_tree_outside_a_platform_is_a_divergence() {
        let mut f = Fixture::default();
        let mut in_tree = CtKey::plain(ctk("leaf"));
        in_tree.platform_tree = true;
        let leaf = f.interner.intern(&NodeKey::Ct1(in_tree.clone()));
        f.bodies.insert(
            leaf,
            vec![(
                Transcript {
                    events: vec![Event::Done],
                },
                None,
                true,
                false,
            )],
        );
        let out = run(&f, &root(vec![Event::Reach(leaf)]), Options::default()).unwrap();
        assert_eq!(out.divergences.len(), 1);
    }

    #[test]
    fn warnings_and_package_commits_are_reported_once() {
        let mut f = Fixture::default();
        let package = ctk("b").label.package_id();
        let b = f.body(
            "b",
            vec![
                Event::PkgCommit(package.clone(), false),
                Event::Warn("w1".into()),
                Event::Warn("w0".into()),
            ],
        );
        let a = f.body(
            "a",
            vec![
                Event::Reach(b),
                Event::PkgCommit(package, true),
                Event::Warn("w1".into()),
                Event::Warn("w2".into()),
            ],
        );
        let out = run(
            &f,
            &root(vec![Event::Reach(a)]),
            Options {
                warnings: vec!["w0".into()],
                ..Options::default()
            },
        )
        .unwrap();
        assert_eq!(out.warnings, ["w1", "w2"]);
        let commits = out
            .tape
            .ops
            .iter()
            .filter(|op| matches!(op, TapeOp::Effect(Event::PkgCommit(..))))
            .count();
        assert_eq!(commits, 1);
    }

    #[test]
    fn a_cycle_is_reported_with_the_walk_text_and_leaves_the_chain() {
        let mut f = Fixture::default();
        let a = f.node("a");
        let b = f.failing("b", vec![Event::Reach(a)]);
        f.failing("a", vec![Event::Reach(b)]);
        let mut segments = root(vec![Event::Reach(a)]);
        if let SegmentKind::Root { trimmed_root, .. } = &mut segments[0].kind {
            *trimmed_root = Some(ctk("a"));
        }
        let out = run(&f, &segments, Options::default()).unwrap();
        assert_eq!(
            out.failures,
            ["cycle at a: path [] cycle [a,b] root Some(\"a\")"]
        );
        assert!(out.plan.is_empty());
        // The failed bodies stay on the chain, as the walk's `?` leaves it.
        assert_eq!(out.segment_active, [2]);
        let fail = out
            .tape
            .ops
            .iter()
            .position(|op| matches!(op, TapeOp::Fail(_)))
            .unwrap();
        assert_eq!(out.tape.ops[fail + 1], TapeOp::Leave);
    }

    #[test]
    fn the_depth_limit_fails_the_reach() {
        let mut f = Fixture::default();
        let nodes: Vec<NodeId> = (0..=DEPTH_LIMIT)
            .map(|i| f.node(&format!("n{i}")))
            .collect();
        for i in 0..DEPTH_LIMIT {
            f.failing(&format!("n{i}"), vec![Event::Reach(nodes[i + 1])]);
        }
        // Pass 1 recurses like the walk (which runs on a large stack).
        let out = std::thread::scope(|scope| {
            std::thread::Builder::new()
                .stack_size(64 << 20)
                .spawn_scoped(scope, || {
                    run(&f, &root(vec![Event::Reach(nodes[0])]), Options::default()).unwrap()
                })
                .unwrap()
                .join()
                .unwrap()
        });
        assert_eq!(out.failures, [DEPTH_LIMIT_ERROR]);
        assert_eq!(out.stop.as_deref(), Some(DEPTH_LIMIT_ERROR));
    }

    #[test]
    fn a_failed_body_is_replayed_on_each_revisit_and_keep_going_edges_repeat() {
        let mut f = Fixture::default();
        let c = f.failing(
            "c",
            vec![Event::Access(ctk("c").label), Event::Warn("w".into())],
        );
        let a = f.body("a", vec![Event::Reach(c)]);
        let b = f.body("b", vec![Event::Reach(c)]);
        let segments = vec![
            Segment {
                kind: SegmentKind::Root {
                    position: 0,
                    label: ctk("a").label,
                    trimmed_root: None,
                },
                transcript: Transcript {
                    events: vec![Event::Reach(a)],
                },
            },
            Segment {
                kind: SegmentKind::Root {
                    position: 1,
                    label: ctk("b").label,
                    trimmed_root: None,
                },
                transcript: Transcript {
                    events: vec![Event::Reach(b)],
                },
            },
        ];
        let out = run(&f, &segments, Options::default()).unwrap();
        // a's and b's bodies caught c's failure (synthetic) and completed.
        assert_eq!(names(&out.plan), ["a", "b"]);
        assert_eq!(out.warnings, ["w"]);
        let accesses = out
            .tape
            .ops
            .iter()
            .filter(|op| matches!(op, TapeOp::Effect(Event::Access(_))))
            .count();
        assert_eq!(accesses, 2, "the walk re-runs a failed body");
        assert_eq!(
            out.edges,
            [
                (None, ctk("a")),
                (Some(ctk("a")), ctk("c")),
                (None, ctk("b")),
                (Some(ctk("b")), ctk("c")),
            ]
        );
    }

    /// The evaluation is still running: pass 1 stops at the end of what
    /// exists and continues where it stopped.
    #[test]
    fn resumes_where_the_transcripts_end() {
        let mut f = Fixture::default();
        let c = f.body("c", vec![]);
        let a = f.node("a");
        f.bodies.insert(
            a,
            vec![(
                Transcript {
                    events: vec![Event::Reach(c), Event::Flush],
                },
                None,
                false,
                false,
            )],
        );
        f.partial.insert(a);
        f.segments = root(vec![Event::Reach(a)]);
        let mut pass = Pass1::new(Options::default());
        let mut ops: Vec<TapeOp> = Vec::new();
        assert_eq!(pass.resume(&f, &mut ops).unwrap(), Progress::Blocked);
        assert_eq!(ops.last(), Some(&TapeOp::Flush));
        assert_eq!(names(pass.plan()), ["c"]);
        // a finishes; the segment and the evaluation end.
        f.bodies.get_mut(&a).unwrap()[0]
            .0
            .events
            .extend([Event::Done, Event::PlanPush]);
        f.partial.clear();
        assert_eq!(pass.resume(&f, &mut ops).unwrap(), Progress::Blocked);
        f.segments_done = true;
        assert_eq!(pass.resume(&f, &mut ops).unwrap(), Progress::Finished);
        assert_eq!(names(&pass.finish().plan), ["c", "a"]);
        assert_eq!(ops.iter().filter(|op| **op == TapeOp::Leave).count(), 2);
    }

    #[test]
    fn console_writes_and_package_commits_are_written_once() {
        let mut f = Fixture::default();
        let failing = f.failing("x", vec![Event::Console(0), Event::Console(1)]);
        let a = f.body("a", vec![Event::Reach(failing), Event::Reach(failing)]);
        let out = run(&f, &root(vec![Event::Reach(a)]), Options::default()).unwrap();
        let consoles = out
            .tape
            .ops
            .iter()
            .filter(|op| matches!(op, TapeOp::Effect(Event::Console(_))))
            .count();
        assert_eq!(consoles, 2);
    }

    #[test]
    fn delegations_toolchain_conflicts_and_forced_platforms() {
        let mut f = Fixture::default();
        let owner = f.body("owner", vec![]);
        let requested = f.node("requested");
        f.bodies.insert(
            requested,
            vec![(
                Transcript {
                    events: vec![Event::Reach(owner), Event::Done],
                },
                Some(ctk("owner")),
                false,
                false,
            )],
        );
        let exec1 = Label::parse_canonical("//plat:one").unwrap();
        let exec2 = Label::parse_canonical("//plat:two").unwrap();
        let mut forced = CtKey::plain(ctk("impl"));
        forced.forced_exec = Some(exec1.clone());
        let implementation = f.interner.intern(&NodeKey::Ct1(forced.clone()));
        f.bodies.insert(
            implementation,
            vec![(
                Transcript {
                    events: vec![Event::Done, Event::PlanPush],
                },
                None,
                false,
                false,
            )],
        );
        let a = f.body(
            "a",
            vec![
                Event::Reach(requested),
                Event::ToolchainImpl(Box::new(ToolchainImpl {
                    implementation: ctk("impl"),
                    exec_platform: exec1.clone(),
                    location: None,
                })),
                Event::Reach(implementation),
                Event::ToolchainImpl(Box::new(ToolchainImpl {
                    implementation: ctk("impl"),
                    exec_platform: exec2.clone(),
                    location: Some("BUILD:3:1".into()),
                })),
            ],
        );
        let out = run(&f, &root(vec![Event::Reach(a)]), Options::default()).unwrap();
        assert_eq!(out.delegations.get(&ctk("requested")), Some(&ctk("owner")));
        assert_eq!(
            out.toolchain_implementations.get(&ctk("impl")),
            Some(&exec1)
        );
        let error = format!(
            "BUILD:3:1: {}",
            toolchain_conflict(&ctk("impl").label, &exec1, &exec2)
        );
        assert_eq!(out.failures, std::slice::from_ref(&error));
        // The walk stops there.
        assert_eq!(out.stop, Some(error));
        assert!(out.divergences.is_empty(), "{:?}", out.divergences);
    }

    #[test]
    fn a_second_variant_with_another_value_is_a_divergence() {
        let mut f = Fixture::default();
        let first = f.body("x", vec![]);
        let mut variant = CtKey::plain(ctk("x"));
        variant.platform_tree = true;
        let second = f.interner.intern(&NodeKey::Ct1(variant));
        f.different.insert((first, second));
        let a = f.body("a", vec![Event::Reach(first), Event::Reach(second)]);
        let out = run(&f, &root(vec![Event::Reach(a)]), Options::default()).unwrap();
        assert_eq!(out.divergences.len(), 1);
    }

    #[test]
    fn an_entered_node_without_a_transcript_is_an_internal_error() {
        let f = Fixture::default();
        let a = f.node("a");
        let error = run(&f, &root(vec![Event::Reach(a)]), Options::default()).unwrap_err();
        assert!(error.contains("no transcript"), "{error}");
    }

    #[test]
    fn incompatible_plan_entries_record_no_position_and_flushes_are_markers() {
        let mut f = Fixture::default();
        let node = f.node("i");
        f.bodies.insert(
            node,
            vec![(
                Transcript {
                    events: vec![Event::Done, Event::PlanPush],
                },
                None,
                false,
                true,
            )],
        );
        let a = f.body("a", vec![Event::Reach(node), Event::Flush]);
        let out = run(&f, &root(vec![Event::Reach(a)]), Options::default()).unwrap();
        let plans: Vec<bool> = out
            .tape
            .ops
            .iter()
            .filter_map(|op| match op {
                TapeOp::Plan { position, .. } => Some(*position),
                _ => None,
            })
            .collect();
        assert_eq!(plans, [false, true]);
        assert!(out.tape.ops.contains(&TapeOp::Flush));
    }

    /// A transition's memo replays its effects at the first use and its
    /// print() lines at every later one (StarlarkTransitionCache hits).
    #[test]
    fn a_transition_prints_at_every_use() {
        let mut f = Fixture::default();
        let details = f
            .interner
            .intern(&NodeKey::TransitionDetails(vec![ctk("flag").label]));
        let transition = f
            .interner
            .intern(&NodeKey::Transition((7, ctk("x").config, [0; 32])));
        f.memos.insert(
            details,
            (
                Transcript {
                    events: vec![Event::Console(0)],
                },
                true,
            ),
        );
        f.memos.insert(
            transition,
            (
                Transcript {
                    events: vec![
                        Event::MemoUse(details),
                        Event::Print("t1".into()),
                        Event::Print("t2".into()),
                    ],
                },
                true,
            ),
        );
        let b = f.body("b", vec![Event::MemoUse(transition)]);
        let a = f.body("a", vec![Event::MemoUse(transition), Event::Reach(b)]);
        let out = run(&f, &root(vec![Event::Reach(a)]), Options::default()).unwrap();
        let effects: Vec<String> = out
            .tape
            .ops
            .iter()
            .filter_map(|op| match op {
                TapeOp::Effect(Event::Print(line)) => Some(line.clone()),
                TapeOp::Effect(Event::Console(index)) => Some(format!("console {index}")),
                _ => None,
            })
            .collect();
        assert_eq!(effects, ["console 0", "t1", "t2", "t1", "t2"]);
    }

    /// An aspect memo's computation is replayed at its first linearized
    /// use only (the walk's later uses are memo hits); a later use with
    /// another value is a divergence.
    #[test]
    fn a_memo_scope_is_replayed_at_its_first_use() {
        let mut f = Fixture::default();
        let c = f.body("c", vec![]);
        let scope = |value: u64| {
            vec![
                Event::MemoBegin(9),
                Event::Print("computed".into()),
                Event::Reach(c),
                Event::MemoEnd(Some(value)),
            ]
        };
        let b = f.body("b", scope(1));
        let mut events = scope(1);
        events.push(Event::Reach(b));
        let a = f.body("a", events);
        let out = run(&f, &root(vec![Event::Reach(a)]), Options::default()).unwrap();
        let prints = out
            .tape
            .ops
            .iter()
            .filter(|op| matches!(op, TapeOp::Effect(Event::Print(_))))
            .count();
        assert_eq!(prints, 1);
        assert_eq!(names(&out.plan), ["c", "b", "a"]);
        assert!(out.divergences.is_empty());
        // A failed computation is not memoized.
        let mut f = Fixture::default();
        let failed = vec![
            Event::MemoBegin(9),
            Event::Print("computed".into()),
            Event::MemoEnd(None),
        ];
        let b = f.body("b", scope_values(&failed, 2));
        let a = f.body("a", [failed.clone(), vec![Event::Reach(b)]].concat());
        let out = run(&f, &root(vec![Event::Reach(a)]), Options::default()).unwrap();
        let prints = out
            .tape
            .ops
            .iter()
            .filter(|op| matches!(op, TapeOp::Effect(Event::Print(_))))
            .count();
        assert_eq!(prints, 3);
        assert!(out.divergences.is_empty());
        // Another value at a later use.
        let mut f = Fixture::default();
        let c = f.body("c", vec![]);
        let _ = c;
        let b = f.body("b", vec![Event::MemoBegin(9), Event::MemoEnd(Some(2))]);
        let a = f.body(
            "a",
            vec![
                Event::MemoBegin(9),
                Event::MemoEnd(Some(1)),
                Event::Reach(b),
            ],
        );
        let out = run(&f, &root(vec![Event::Reach(a)]), Options::default()).unwrap();
        assert_eq!(out.divergences.len(), 1);
    }

    /// `events` then a successful scope of value `value`.
    fn scope_values(events: &[Event], value: u64) -> Vec<Event> {
        let mut all = events.to_vec();
        all.extend([
            Event::MemoBegin(9),
            Event::Print("computed".into()),
            Event::MemoEnd(Some(value)),
        ]);
        all
    }

    /// xorshift64* (reproducible orders without a dependency).
    pub(crate) struct Rng(pub(crate) u64);

    impl Rng {
        pub(crate) fn next(&mut self) -> u64 {
            self.0 ^= self.0 >> 12;
            self.0 ^= self.0 << 25;
            self.0 ^= self.0 >> 27;
            self.0.wrapping_mul(0x2545_f491_4f6c_dd1d)
        }

        pub(crate) fn below(&mut self, n: usize) -> usize {
            (self.next() % n as u64) as usize
        }

        pub(crate) fn shuffle<T>(&mut self, items: &mut [T]) {
            for i in (1..items.len()).rev() {
                items.swap(i, self.below(i + 1));
            }
        }
    }

    /// A random phase 1: 40 configured targets (a DAG; some fail), memos
    /// (platforms: a fresh chain each; exec configurations) reached from
    /// the lower ones, warnings, prints, console writes, package commits,
    /// toolchain implementations, flushes; four segments.
    fn random_phase1(seed: u64) -> (Fixture, Vec<NodeId>) {
        const N: usize = 40;
        const HIGH: usize = 28;
        let mut rng = Rng(seed | 1);
        let mut f = Fixture::default();
        let nodes: Vec<NodeId> = (0..N).map(|i| f.node(&format!("n{i}"))).collect();
        let memos: Vec<NodeId> = (0..6)
            .map(|i| {
                let key = if i % 2 == 0 {
                    NodeKey::Platform(ctk(&format!("plat{i}")))
                } else {
                    NodeKey::ExecCfg(ctk("x").config, ctk(&format!("exec{i}")).label)
                };
                f.interner.intern(&key)
            })
            .collect();
        let package = |i: usize| {
            Label::parse_canonical(&format!("//q{i}:x"))
                .unwrap()
                .package_id()
        };
        for (i, &node) in nodes.iter().enumerate() {
            let mut events = Vec::new();
            for _ in 0..rng.below(7) {
                match rng.below(10) {
                    0..=3 if i + 1 < N => {
                        events.push(Event::Reach(nodes[i + 1 + rng.below(N - i - 1)]));
                    }
                    4 if i < HIGH => events.push(Event::MemoUse(memos[rng.below(memos.len())])),
                    5 => events.push(Event::Warn(format!("w{}", rng.below(6)))),
                    6 => events.push(Event::PkgCommit(package(rng.below(6)), rng.below(2) == 0)),
                    7 => events.push(Event::Console(rng.below(12) as u32)),
                    8 => events.push(Event::ToolchainImpl(Box::new(ToolchainImpl {
                        implementation: ctk(&format!("impl{}", rng.below(3))),
                        exec_platform: ctk(&format!("exec{}", rng.below(2))).label,
                        location: None,
                    }))),
                    _ => events.push(Event::Print(format!("p{i}"))),
                }
            }
            if rng.below(3) == 0 {
                // An aspect memo (a few keys; same value per key).
                let memo = rng.below(4) as u64;
                events.push(Event::MemoBegin(memo));
                events.push(Event::Print(format!("memo {memo}")));
                if i + 1 < N {
                    events.push(Event::Reach(nodes[i + 1 + rng.below(N - i - 1)]));
                }
                events.push(Event::MemoEnd(Some(memo * 10)));
            }
            if rng.below(8) == 0 {
                events.push(Event::Flush);
            }
            if rng.below(7) == 0 {
                events.push(Event::Error(format!("n{i} failed")));
            } else {
                events.push(Event::Done);
                events.push(Event::PlanPush);
            }
            let delegate = (rng.below(9) == 0).then(|| ctk("owner"));
            f.bodies.insert(
                node,
                vec![(
                    Transcript { events },
                    delegate,
                    rng.below(5) == 0,
                    rng.below(6) == 0,
                )],
            );
        }
        for &memo in &memos {
            let mut events = vec![Event::Print("memo".into())];
            for _ in 0..rng.below(4) {
                events.push(Event::Reach(nodes[HIGH + rng.below(N - HIGH)]));
            }
            f.memos
                .insert(memo, (Transcript { events }, rng.below(3) != 0));
        }
        let mut segments = vec![Segment {
            kind: SegmentKind::Setup,
            transcript: Transcript {
                events: vec![Event::MemoUse(memos[0])],
            },
        }];
        for (position, root) in [0, 1, 5].into_iter().enumerate() {
            segments.push(Segment {
                kind: SegmentKind::Root {
                    position,
                    label: ctk(&format!("n{root}")).label,
                    trimmed_root: None,
                },
                transcript: Transcript {
                    events: vec![Event::Access(ctk("a").label), Event::Reach(nodes[root])],
                },
            });
        }
        segments.push(Segment {
            kind: SegmentKind::RootsDone,
            transcript: Transcript::default(),
        });
        f.segments = segments;
        let mut all = nodes;
        all.extend(memos);
        (f, all)
    }

    fn outputs(out: &Linearized, ops: &[TapeOp]) -> String {
        format!(
            "{ops:?}\n{:?}\n{:?}\n{:?}\n{:?}\n{:?}\n{:?}\n{:?}\n{:?}\n{:?}\n{:?}\n{:?}",
            out.plan,
            out.delegations,
            out.empty_configuration,
            out.edges,
            out.toolchain_implementations,
            out.warnings,
            out.failures,
            out.segment_active,
            out.first_toolchains,
            out.divergences,
            out.stop
        )
    }

    /// The nodes finish in any order (the parallel evaluator), partly
    /// visible while running; pass 1 is resumed after each step and its
    /// released transcripts are emptied: the result is the sequential
    /// one, whatever the order.
    #[test]
    fn any_completion_order_gives_the_sequential_result() {
        let mut stops = 0;
        for seed in 1..=60u64 {
            let (mut f, all) = random_phase1(seed.wrapping_mul(0x9e37_79b9_7f4a_7c15));
            let segments = std::mem::take(&mut f.segments);
            let reference = run(&f, &segments, Options::default()).unwrap();
            let expected = outputs(&reference, &reference.tape.ops);
            let mut rng = Rng(seed ^ 0xdead_beef);
            for round in 0..4 {
                let mut order = all.clone();
                rng.shuffle(&mut order);
                f.segments = segments.clone();
                f.segments_done = false;
                f.visible = Some(HashMap::new());
                f.released.clear();
                let mut pass = Pass1::new(Options::default());
                // Half of the rounds read the canonical nodes by node.
                if round % 2 == 1 {
                    pass.index_by_node();
                }
                let mut ops: Vec<TapeOp> = Vec::new();
                let mut released = 0;
                for &node in &order {
                    let len = |f: &Fixture| {
                        f.bodies
                            .get(&node)
                            .map(|bodies| bodies[0].0.events.len())
                            .or_else(|| f.memos.get(&node).map(|memo| memo.0.events.len()))
                            .unwrap()
                    };
                    // Running: a prefix of its transcript exists.
                    let prefix = rng.below(len(&f) + 1);
                    f.visible.as_mut().unwrap().insert(node, prefix);
                    let progress = pass.resume(&f, &mut ops).unwrap();
                    assert_ne!(progress, Progress::Finished);
                    f.visible.as_mut().unwrap().insert(node, usize::MAX);
                    let progress = pass.resume(&f, &mut ops).unwrap();
                    assert_ne!(progress, Progress::Finished);
                    assert!(progress == Progress::Stopped || pass.blocked_on().is_some());
                    let (nodes, _) = pass.take_released();
                    released += nodes.len();
                    f.released.extend(nodes);
                }
                f.segments_done = true;
                let progress = pass.resume(&f, &mut ops).unwrap();
                let finished = if reference.stop.is_some() {
                    Progress::Stopped
                } else {
                    Progress::Finished
                };
                assert_eq!(progress, finished);
                assert_eq!(pass.blocked_on(), None);
                let out = pass.finish();
                stops += usize::from(out.stop.is_some());
                assert_eq!(
                    outputs(&out, &ops),
                    expected,
                    "seed {seed}, order {order:?}"
                );
                assert!(released > 0 || out.stop.is_some());
            }
        }
        // Some of the random graphs stop at a conflict, most do not.
        assert!(stops > 0 && stops < 200, "{stops}");
    }
}
