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
//! - **MemoUse**: a memo node's transcript is replayed at its first use; a
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

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};

use crate::configuration::ConfigId;
use crate::label::{Label, PackageId};

use super::super::super::ConfiguredTargetKey;
use super::super::key::{CtKey, NodeId, NodeKey};
#[cfg(test)]
use super::super::transcript::ToolchainImpl;
use super::super::transcript::{Event, SegmentKind, Tape, TapeOp, Transcript};

/// The walk's dependency depth limit error (`analyze_key_body`).
pub(crate) const DEPTH_LIMIT_ERROR: &str = "native prototype dependency depth limit (256) exceeded";

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
    fn key(&self, node: NodeId) -> NodeKey;
    /// A CT1 node's transcript (`None`: the node never ran — an internal
    /// error when pass 1 enters it).
    fn ct1(&self, node: NodeId) -> Option<Ct1Record<'_>>;
    /// A memo node's transcript (`None`: no effects).
    fn memo(&self, node: NodeId) -> Option<MemoRecord<'_>>;
    /// The top-level segment at `index` (`None`: none (yet)).
    fn segment(&self, index: usize) -> Option<SegmentRecord<'_>>;
    /// No further segment will come.
    fn segments_complete(&self) -> bool;
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
            TapeOp::Plan { key, position } => {
                self.plan(key, position);
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
    visited: HashMap<ConfiguredTargetKey, NodeId>,
    /// The active chains, innermost (a memo's fresh chain) last.
    chains: Vec<Vec<ConfiguredTargetKey>>,
    memos: HashSet<NodeId>,
    packages: HashSet<PackageId>,
    consoles: HashSet<u32>,
    warned: HashSet<String>,
    /// Platform memos being replayed (`resolving_platforms`).
    platform_depth: usize,
    trimmed_root: Option<ConfiguredTargetKey>,
    frames: Vec<Frame>,
    next_segment: usize,
}

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
        fn key(&self, node: NodeId) -> NodeKey {
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
        Progress::Finished => {}
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
            visited: HashMap::new(),
            chains: Vec::new(),
            memos: HashSet::new(),
            packages: HashSet::new(),
            consoles: HashSet::new(),
            warned: options.warnings.into_iter().collect(),
            platform_depth: 0,
            trimmed_root: None,
            frames: Vec::new(),
            next_segment: 0,
        }
    }

    /// The plan so far.
    pub(crate) fn plan(&self) -> &[CtKey] {
        &self.out.plan
    }

    /// A configured key the walk analyzed outside phase 1's transcripts
    /// (a deferred target platform's tree planned by a flush's phase 2):
    /// complete from now on, and in the plan where the flush put it.
    pub(crate) fn external(&mut self, key: ConfiguredTargetKey, node: NodeId, planned: bool) {
        self.visited.insert(key.clone(), node);
        if planned {
            self.out.plan.push(CtKey::plain(key));
        }
    }

    /// The result (the tape went to the sink).
    pub(crate) fn finish(self) -> Linearized {
        self.out
    }

    fn chain(&mut self) -> &mut Vec<ConfiguredTargetKey> {
        self.chains.last_mut().expect("a segment's chain")
    }

    /// Continue the replay as far as the transcripts go.
    pub(crate) fn resume<T: Transcripts + ?Sized>(
        &mut self,
        source: &T,
        sink: &mut dyn TapeSink,
    ) -> Result<Progress, String> {
        loop {
            let Some(frame) = self.frames.last_mut() else {
                // Between segments.
                let Some(segment) = source.segment(self.next_segment) else {
                    return Ok(if source.segments_complete() {
                        Progress::Finished
                    } else {
                        Progress::Blocked
                    });
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
            let (event, complete) = match frame {
                Frame::Segment { index, pos } => {
                    let segment = source
                        .segment(*index)
                        .ok_or("internal error: pass 1: a segment vanished")?;
                    let event = segment.events.get(*pos).cloned();
                    *pos += usize::from(event.is_some());
                    (event, segment.complete)
                }
                Frame::Body { node, pos, .. } => {
                    let record = source
                        .ct1(*node)
                        .ok_or("internal error: pass 1: a body vanished")?;
                    let event = record.events.get(*pos).cloned();
                    *pos += usize::from(event.is_some());
                    (event, record.complete)
                }
                Frame::Memo { node, pos, .. } => match source.memo(*node) {
                    Some(record) => {
                        let event = record.events.get(*pos).cloned();
                        *pos += usize::from(event.is_some());
                        (event, record.complete)
                    }
                    None => (None, true),
                },
            };
            match event {
                Some(event) => self.event(source, sink, event)?,
                None if !complete => return Ok(Progress::Blocked),
                None => self.pop(source, sink),
            }
        }
    }

    /// The innermost frame's transcript is replayed to its end.
    fn pop<T: Transcripts + ?Sized>(&mut self, source: &T, sink: &mut dyn TapeSink) {
        match self.frames.pop().expect("a frame") {
            Frame::Segment { .. } => {
                self.out.segment_active.push(self.chains[0].len());
                self.trimmed_root = None;
                self.chains.clear();
            }
            Frame::Body { node, ct, .. } => {
                if let Some(record) = source.ct1(node)
                    && record.empty_configuration
                {
                    self.out.empty_configuration.insert(ct.key);
                }
                sink.op(TapeOp::Leave);
            }
            Frame::Memo {
                node,
                fresh,
                platform,
                ..
            } => {
                if fresh {
                    self.chains.pop();
                }
                self.platform_depth -= usize::from(platform);
                if let NodeKey::RegToolchains(config) = source.key(node)
                    && source.memo(node).is_some_and(|record| record.ok)
                {
                    self.out.first_toolchains.get_or_insert(config);
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

    fn event<T: Transcripts + ?Sized>(
        &mut self,
        source: &T,
        sink: &mut dyn TapeSink,
        event: Event,
    ) -> Result<(), String> {
        match event {
            Event::Reach(node) => {
                if self.in_fresh_memo() {
                    self.chain().clear();
                }
                self.reach(source, sink, node)?;
            }
            Event::PkgCommit(ref package) => {
                if self.packages.insert(package.clone()) {
                    sink.op(TapeOp::Effect(event));
                }
            }
            Event::Console(index) => {
                if self.consoles.insert(index) {
                    sink.op(TapeOp::Effect(event));
                }
            }
            Event::MemoUse(node) => self.memo_use(source, sink, node),
            Event::Warn(ref warning) => {
                if self.warned.insert(warning.clone()) {
                    self.out.warnings.push(warning.clone());
                    sink.op(TapeOp::Effect(event));
                }
            }
            Event::ToolchainImpl(ref needed) => {
                let (implementation, exec_platform) =
                    (&needed.implementation, &needed.exec_platform);
                match self.out.toolchain_implementations.get(implementation) {
                    Some(existing) if existing != exec_platform => {
                        let error =
                            toolchain_conflict(&implementation.label, existing, exec_platform);
                        self.out.failures.push(error);
                    }
                    Some(_) => {}
                    None => {
                        self.out
                            .toolchain_implementations
                            .insert(implementation.clone(), exec_platform.clone());
                    }
                }
                sink.op(TapeOp::Effect(event));
            }
            Event::PlanPush => {
                let Some((node, ct)) = self.owner() else {
                    return Err("internal error: pass 1: a plan entry outside a body".to_owned());
                };
                let incompatible = source.ct1(node).is_some_and(|record| record.incompatible);
                self.out.plan.push(ct.clone());
                sink.op(TapeOp::Plan {
                    key: ct,
                    position: !incompatible,
                });
            }
            Event::Done => {
                if let Some((node, ct)) = self.owner() {
                    // The walk pops the top of the chain (the key's own
                    // entry unless a caught failure left more).
                    self.chain().pop();
                    self.visited.insert(ct.key.clone(), node);
                    if let Some(record) = source.ct1(node)
                        && let Some(delegate) = record.delegate
                    {
                        self.out.delegations.insert(ct.key, delegate.clone());
                    }
                }
                sink.op(TapeOp::Effect(Event::Done));
            }
            Event::Flush => sink.op(TapeOp::Flush),
            Event::Access(_)
            | Event::Source(_)
            | Event::Print(_)
            | Event::CqueryEdge(_)
            | Event::ScopeEnter(_)
            | Event::ScopeExit
            | Event::Continue(_)
            | Event::Error(_) => sink.op(TapeOp::Effect(event)),
        }
        Ok(())
    }

    fn reach<T: Transcripts + ?Sized>(
        &mut self,
        source: &T,
        sink: &mut dyn TapeSink,
        node: NodeId,
    ) -> Result<(), String> {
        let NodeKey::Ct1(ct) = source.key(node) else {
            return Err(format!(
                "internal error: pass 1: a Reach of the non-CT1 node {:?}",
                source.key(node)
            ));
        };
        let key = ct.key.clone();
        let parent = self.chain().last().cloned();
        let canonical = self.visited.get(&key).copied();
        sink.op(TapeOp::Reach {
            node,
            key: key.clone(),
            parent: parent.clone(),
            enter: canonical.is_none(),
        });
        self.out.edges.push((parent, key.clone()));
        if let Some(canonical) = canonical {
            if canonical != node && !source.same_observable(canonical, node) {
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
            && self.out.toolchain_implementations.get(&key) != Some(forced)
        {
            self.out.divergences.push(format!(
                "{key}: analyzed with the forced execution platform {forced}, the first-visit one is {:?}",
                self.out.toolchain_implementations.get(&key)
            ));
        }
        let chain = self.chains.last().expect("a segment's chain");
        if let Some(start) = chain.iter().position(|active| *active == key) {
            let error = source.cycle_error(
                &key,
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
        self.chain().push(key);
        self.frames.push(Frame::Body { node, ct, pos: 0 });
        Ok(())
    }

    fn memo_use<T: Transcripts + ?Sized>(
        &mut self,
        source: &T,
        sink: &mut dyn TapeSink,
        node: NodeId,
    ) {
        if !self.memos.insert(node) {
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
    }

    impl Transcripts for Fixture {
        fn key(&self, node: NodeId) -> NodeKey {
            self.interner.key(node).clone()
        }
        fn ct1(&self, node: NodeId) -> Option<Ct1Record<'_>> {
            let (transcript, delegate, empty, incompatible) = self.bodies.get(&node)?.first()?;
            Some(Ct1Record {
                events: &transcript.events,
                complete: !self.partial.contains(&node),
                delegate: delegate.as_ref(),
                empty_configuration: *empty,
                incompatible: *incompatible,
            })
        }
        fn memo(&self, node: NodeId) -> Option<MemoRecord<'_>> {
            self.memos.get(&node).map(|(transcript, ok)| MemoRecord {
                events: &transcript.events,
                complete: true,
                ok: *ok,
            })
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
                Event::PkgCommit(package.clone()),
                Event::Warn("w1".into()),
                Event::Warn("w0".into()),
            ],
        );
        let a = f.body(
            "a",
            vec![
                Event::Reach(b),
                Event::PkgCommit(package),
                Event::Warn("w1".into()),
                Event::Warn("w2".into()),
            ],
        );
        let out = run(
            &f,
            &root(vec![Event::Reach(a)]),
            Options {
                warnings: vec!["w0".into()],
            },
        )
        .unwrap();
        assert_eq!(out.warnings, ["w1", "w2"]);
        let commits = out
            .tape
            .ops
            .iter()
            .filter(|op| matches!(op, TapeOp::Effect(Event::PkgCommit(_))))
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
                })),
                Event::Reach(implementation),
                Event::ToolchainImpl(Box::new(ToolchainImpl {
                    implementation: ctk("impl"),
                    exec_platform: exec2.clone(),
                })),
            ],
        );
        let out = run(&f, &root(vec![Event::Reach(a)]), Options::default()).unwrap();
        assert_eq!(out.delegations.get(&ctk("requested")), Some(&ctk("owner")));
        assert_eq!(
            out.toolchain_implementations.get(&ctk("impl")),
            Some(&exec1)
        );
        assert_eq!(
            out.failures,
            [toolchain_conflict(&ctk("impl").label, &exec1, &exec2)]
        );
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
}
