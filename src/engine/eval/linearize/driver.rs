//! `BAZEL_RS_ANALYSIS=eval` phase 1 (S1, docs/EVALUATOR.md): worker B's
//! node bodies on worker A's inline driver (one thread: `need` evaluates in
//! place, the walk's DFS), their transcripts linearized by pass 1, and the
//! tape applied to the engine where the walk would have written:
//!
//! - every transcript is the running attempt's events plus what the
//!   engine's coordinator services did in between ([`Coord::drain`]: the
//!   tracker scopes and target lookups of [`super::capture`], the warnings
//!   the engine pushed, the console writes held by the evaluation's
//!   `DeferredConsole` as [`Event::Console`]); a `Tgt` lookup's belong to
//!   its user (the walk looks targets up inline);
//! - pass 1 runs as far as the transcripts go at each `Flush` (a
//!   config_setting with Starlark `flag_values`: the walk's `plan_pending`)
//!   and at the end; its tape is applied in order ([`Coord::linearize`]):
//!   console writes and `print()` lines, warnings, the incremental tracker
//!   (`TapeReplay`), the plan with each entry's configured rule, and at a
//!   `Flush` the phase 2 of the plan so far, after the done CT1 and memo
//!   values were published into the engine's maps (what phase 2 reads);
//! - after phase 1 the engine holds what the walk would: `configured`, the
//!   plan, the per-key maps, the warnings, the tracker; phase 2 continues
//!   on pass 1's plan.
//!
//! Not covered yet (the walk plans these requests, `eval::select`):
//! cquery/aquery captures, `--keep_going`, top-level aspects, the cquery
//! root checkpoint (S4).

use std::sync::Arc;

use crate::configuration::ConfigId;
use crate::fast_hash::{FxHashMap, FxHashSet};
use crate::label::Label;
use crate::platforms::PlatformInfo;
use crate::toolchains::RegisteredToolchains;

use super::super::super::incremental::{TapeReplay, TrackOp, Tracker};
use super::super::super::{
    ConfiguredNodes, ConfiguredTargetKey, Engine, ExecutionPlatforms, Plan, PlatformError,
    ResolutionError, RuleKind, Target, TransitionDetails, forwards,
};
use super::super::cycles::{CycleHandler, Resolution, WaitGraph};
use super::super::key::{CtKey, NodeId, NodeKey};
use super::super::nodes::bodies::{Coord, Phase1Bodies, Running, push_running, with_running};
use super::super::nodes::eval_host::edge_ct_key;
use super::super::nodes::loading::PkgValue;
use super::super::scheduler::{EvalConfig, Evaluator, RepoLane};
use super::super::shared::EvalShared;
use super::super::transcript::{Event, SegmentKind, TapeOp, Transcript};
use super::super::value::{Ct1Ok, Ct1Value, NodeValue};
use super::cycles::Pass1Cycles;
use super::pass1::{
    Blocker, Ct1Record, MemoRecord, Options, Pass1, Progress, SegmentRecord, Transcripts,
};

/// BAZEL_RS_EVAL_STATS=1: where the linearizer's time goes (µs).
static STATS: [std::sync::atomic::AtomicU64; 4] = [
    std::sync::atomic::AtomicU64::new(0),
    std::sync::atomic::AtomicU64::new(0),
    std::sync::atomic::AtomicU64::new(0),
    std::sync::atomic::AtomicU64::new(0),
];
const STAT_PASS1: usize = 0;
const STAT_APPLY: usize = 1;
const STAT_PUBLISH: usize = 2;
const STAT_PHASE2: usize = 3;

fn stat<R>(index: usize, f: impl FnOnce() -> R) -> R {
    let started = std::time::Instant::now();
    let result = f();
    STATS[index].fetch_add(
        started.elapsed().as_micros() as u64,
        std::sync::atomic::Ordering::Relaxed,
    );
    result
}

/// The source files the evaluation resolved, memoized as the walk's
/// lookups memoize them (S3: phase 2's prepares off the coordinator read
/// them). A memo hit observes nothing: while recording, a source is
/// memoized once what its lookup observed is recorded (a replayed attempt
/// used it), else before a later deferred walk (`Coord::catch_up`) or
/// never (phase 2 then looks it up and observes, as the walk's first
/// lookup).
fn publish_sources(engine: &mut Engine, shared: &EvalShared, lin: &mut Linearizer) {
    let sources = std::mem::take(
        &mut *shared
            .loading
            .stable_sources
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner),
    );
    let observed = shared
        .loading
        .source_observed
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    for (label, path) in sources {
        match observed.get(&label) {
            Some(captured) if !captured.is_recorded() => {
                lin.unrecorded_sources
                    .push((label, path, Arc::clone(captured)));
            }
            _ => {
                engine.source_targets.entry(label).or_insert(path);
            }
        }
    }
}

/// A memo value the engine keeps (phase 2 reads it).
#[derive(Clone)]
pub(in crate::engine) enum Published {
    Platform(
        ConfiguredTargetKey,
        Result<Arc<PlatformInfo>, PlatformError>,
    ),
    RegToolchains(ConfigId, Result<Arc<RegisteredToolchains>, ResolutionError>),
    RegExecPlatforms(ConfigId, Result<ExecutionPlatforms, ResolutionError>),
    ExecCfg(ConfigId, Label, ConfigId),
    FlagScope(Label, bool),
    TransitionDetails(Vec<Label>, Result<TransitionDetails, String>),
    Source(Label),
    Transition(
        super::super::super::TransitionCacheKey,
        (super::super::super::TransitionConfigurations, Vec<String>),
    ),
    AspectPredicate((Label, String), bool),
    AspectAttrAspects(
        (ConfiguredTargetKey, String),
        super::super::value::AspectAttrAspects,
    ),
    AspectToolchains(
        (ConfigId, String),
        Arc<super::super::super::AspectToolchains>,
    ),
    AspectAttributeConfigs(
        (ConfiguredTargetKey, String),
        Arc<std::collections::BTreeMap<String, super::super::super::TransitionConfigurations>>,
    ),
}

/// The finished transcripts, the segments and the captured console writes.
#[derive(Default)]
pub(crate) struct Store {
    pub(crate) keys: FxHashMap<NodeId, NodeKey>,
    pub(crate) transcripts: FxHashMap<NodeId, Transcript>,
    pub(crate) memo_ok: FxHashMap<NodeId, bool>,
    pub(crate) segments: Vec<(SegmentKind, Vec<Event>)>,
    pub(crate) segments_done: bool,
    pub(crate) consoles: Vec<Option<crate::ui::Deferred>>,
    /// What an abandoned attempt captured that happened anyway (console
    /// writes, warnings): its rerun's.
    pub(crate) carry: FxHashMap<NodeId, Vec<Event>>,
    /// The engine's warnings already accounted for.
    pub(crate) warnings_seen: usize,
    /// Memo values to publish, in completion order.
    pub(crate) published: Vec<Published>,
    /// Transcripts pass 1 has replayed for the last time, freed (eval's
    /// memory stays near the walk's: ~/code emits millions of events).
    pub(crate) released: FxHashSet<NodeId>,
    /// A rerun emitted other events than its abandoned attempt (an internal
    /// error: pass 1 may have replayed them).
    pub(crate) rerun_mismatch: Option<String>,
    /// The packages the evaluation loaded (S2, the Pkg nodes): what the
    /// tape's `PkgCommit` commits into the engine.
    pub(crate) packages: FxHashMap<crate::label::PackageId, Result<Arc<PkgValue>, String>>,
    /// The keys of `packages` in insertion order (a speculation's scratch
    /// engine takes the new ones).
    pub(crate) package_order: Vec<crate::label::PackageId>,
    /// What each done attempt observed (a pooled evaluation's frames),
    /// committed where the tape replays the node.
    pub(crate) observations: FxHashMap<NodeId, crate::observed_fs::Observations>,
    /// What a package's lookups and its lane materialization observed (a
    /// pooled evaluation), committed where the tape first uses its Pkg
    /// node.
    pub(crate) package_observations:
        FxHashMap<crate::label::PackageId, crate::observed_fs::Observations>,
    /// The repository roots a pool attempt obtained before the sequential
    /// engine had them: they count from where the tape replays the node.
    pub(crate) roots: FxHashMap<NodeId, Vec<String>>,
    /// The nodes the tape replayed (entered or first used) already: what
    /// they observe once done is committed at once.
    pub(crate) replayed: FxHashSet<NodeId>,
    /// The packages whose Pkg node the tape used already.
    pub(crate) replayed_packages: FxHashSet<crate::label::PackageId>,
    /// Source-file lookups' missing inputs and source directories (Tgt
    /// nodes; order-free maps, published with the values).
    pub(crate) sources_side: Vec<crate::engine::SourceSide>,
    /// Keep released transcripts until a flush planned them (a pooled
    /// evaluation: [`Coord::subtree`]).
    pub(crate) delay_release: bool,
    /// With the plan's length when pass 1 released them: every plan entry
    /// of the node's subtree is below it.
    pub(crate) release_pending: Vec<(NodeId, usize)>,
}

impl Store {
    /// Keep what a lookup in `package` observed for the tape's first use of
    /// the package (now if it was used already).
    pub(crate) fn defer_package_observations(
        &mut self,
        package: &crate::label::PackageId,
        observations: crate::observed_fs::Observations,
    ) {
        if observations.is_empty() {
            return;
        }
        if self.replayed_packages.contains(package) {
            crate::observed_fs::commit_to_recorder(observations);
        } else {
            self.package_observations
                .entry(package.clone())
                .or_default()
                .append(observations);
        }
    }

    /// Keep a console write; its [`Event::Console`] index.
    pub(crate) fn console(&mut self, write: crate::ui::Deferred) -> u32 {
        self.consoles.push(Some(write));
        (self.consoles.len() - 1) as u32
    }

    /// Free what pass 1 will not read again: on the pool, a node's
    /// transcript only once a flush planned it (a speculative phase 2 of a
    /// flag replays the subtrees not planned yet, `Coord::subtree`).
    fn release(&mut self, pass: &mut Pass1, planned: usize) {
        let (nodes, segments) = pass.take_released();
        if self.delay_release {
            self.release_pending
                .extend(nodes.into_iter().map(|node| (node, planned)));
        } else {
            for node in nodes {
                self.transcripts.remove(&node);
                self.released.insert(node);
            }
        }
        for index in segments {
            if let Some((_, events)) = self.segments.get_mut(index) {
                *events = Vec::new();
            }
        }
    }

    /// Free the transcripts released whose subtree's plan entries are all
    /// below `walked` (a walk planned them: a deferred flush's segments
    /// after it are not).
    fn free_released(&mut self, walked: usize) {
        let pending = std::mem::take(&mut self.release_pending);
        for (node, planned) in pending {
            if planned <= walked {
                self.transcripts.remove(&node);
                self.released.insert(node);
            } else {
                self.release_pending.push((node, planned));
            }
        }
    }
}

/// The linearizer's state while the evaluator runs.
pub(crate) struct Linearizer {
    pass: Pass1,
    ops: Vec<TapeOp>,
    tracker: Option<Box<Tracker>>,
    replay: TapeReplay,
    configured: ConfiguredNodes,
    plan: Plan,
    published_memos: usize,
    /// Configured targets pass 1 decided whose value is not done yet.
    unpublished: Vec<NodeId>,
    /// The evaluation's console (everything written during phase 1 is
    /// held; the tape's effects are written around it).
    console: Option<crate::ui::DeferredConsole>,
    /// A flush of the tape failed: the walk stops there (every later
    /// call fails with it; the driver reports it, [`Coord::flush_failure`]).
    failure: Option<FlushFailure>,
    /// S3 pass 2 ([`super::pass2`]): flushes may be deferred (a pooled
    /// evaluation without `--profile`).
    defer: bool,
    /// The plan position the tape's effects apply at: the plan's length at
    /// the last `Flush`.
    position: usize,
    /// The effects not applied yet (a deferred flush's walk is pending),
    /// by position.
    pending: std::collections::VecDeque<(usize, super::pass2::Op)>,
    /// The tape's tracker ops not applied yet, by position (like
    /// `pending`; kept apart: they need no lowering drain, and are many).
    track: std::collections::VecDeque<(u32, TrackOp)>,
    /// The flushes' plan segments not walked yet: `[start, end)` and
    /// where the flush happened.
    segments: std::collections::VecDeque<(usize, usize, Option<super::pass1::FlushPoint>)>,
    /// The platforms published (applied or pending).
    platforms: FxHashSet<ConfiguredTargetKey>,
    /// The platforms whose memo's first use the tape applied: a platform
    /// is published at the first flush after it (the walk resolves it
    /// there), never earlier, however early the pool computed it.
    used_platforms: FxHashSet<ConfiguredTargetKey>,
    /// Finished platform memos not used by the tape yet (published at
    /// their first use's flush, or at the end of phase 1).
    held_platforms: Vec<Published>,
    /// The flushes so far (BAZEL_RS_EVAL_PLATFORM_LOG).
    flushes: usize,
    /// The target platform of each configuration (the deferral check;
    /// `None`: the no-config configuration).
    target_platforms: FxHashMap<ConfigId, Option<ConfiguredTargetKey>>,
    /// Source files resolved while recording whose lookup's observations
    /// were not recorded yet at their flush: memoized before a deferred
    /// walk if they are by then ([`Coord::catch_up`]).
    unrecorded_sources: Vec<(Label, std::path::PathBuf, Arc<crate::observed_fs::Captured>)>,
}

/// A flush of the tape that failed.
#[derive(Clone, Debug)]
pub(crate) struct FlushFailure {
    error: String,
    point: Option<super::pass1::FlushPoint>,
    /// The config_setting that emitted it received the error itself (it
    /// ran in the sequential pass: the walk's propagation).
    delivered: bool,
}

impl Linearizer {
    /// Run `f` with the evaluation's console set aside.
    fn direct<R>(&mut self, f: impl FnOnce(&mut Self) -> R) -> R {
        match self.console.take() {
            Some(mut console) => {
                let result = console.bypass(|| f(self));
                self.console = Some(console);
                result
            }
            None => f(self),
        }
    }
}

impl Coord<'_> {
    /// Move what the engine's services did since the last call into the
    /// running attempt (or the current segment).
    pub(crate) fn drain(&mut self) {
        let mut events = super::capture::take();
        if self.engine.warnings.len() > self.store.warnings_seen {
            let seen = self.store.warnings_seen;
            events.extend(self.engine.warnings.drain(seen..).map(Event::Warn));
        }
        for write in crate::ui::deferred_split() {
            events.push(Event::Console(self.store.consoles.len() as u32));
            self.store.consoles.push(Some(write));
        }
        for event in events {
            self.push_event(event);
        }
    }

    /// Record `event` in this thread's running attempt (or the current
    /// segment). A rerun re-emits its abandoned attempt's events, which stay
    /// in place (pass 1 may have replayed them; `bodies::push_running`).
    pub(crate) fn push_event(&mut self, event: Event) {
        if let Err(event) = push_running(event)
            && let Some((_, events)) = self.store.segments.last_mut()
        {
            events.push(event);
        }
    }

    /// Pass 1 as far as the transcripts go, its tape applied (a `Flush`
    /// runs the plan so far's phase 2). A no-op without the linearizer
    /// (BAZEL_RS_EVAL_VERIFY_PHASE1).
    pub(crate) fn linearize(&mut self, shared: &EvalShared) -> Result<(), String> {
        let Some(mut lin) = self.lin.take() else {
            return Ok(());
        };
        self.drain();
        let result = self.linearize_with(&mut lin, shared);
        self.lin = Some(lin);
        result
    }

    /// [`Self::linearize`] for at most `events` of pass 1 (the coordinator
    /// is released between such steps while the pool serves); whether it
    /// stopped there with more to do.
    pub(crate) fn linearize_step(&mut self, shared: &EvalShared, events: usize) -> bool {
        let Some(lin) = self.lin.as_mut() else {
            return false;
        };
        lin.pass.set_budget(Some(events));
        let result = self.linearize(shared);
        let Some(lin) = self.lin.as_mut() else {
            return false;
        };
        lin.pass.set_budget(None);
        result.is_ok() && lin.pass.paused()
    }

    fn linearize_with(&mut self, lin: &mut Linearizer, shared: &EvalShared) -> Result<(), String> {
        if let Some(failure) = &lin.failure {
            return Err(failure.error.clone());
        }
        // Sticky (eval-round1): a caller that ignores this result must not
        // lose it.
        if let Some(mismatch) = super::super::nodes::bodies::rerun_mismatch() {
            self.store.rerun_mismatch.get_or_insert(mismatch);
        }
        if let Some(mismatch) = self.store.rerun_mismatch.clone() {
            return Err(format!(
                "internal error: BAZEL_RS_ANALYSIS=eval: {mismatch}; rerun with BAZEL_RS_ANALYSIS=walk"
            ));
        }
        with_running(|stack| {
            let view = View {
                store: &self.store,
                stack,
                ct1_values: &self.ct1_values,
            };
            stat(STAT_PASS1, || lin.pass.resume(&view, &mut lin.ops))
        })?;
        // The plan's length once this resume's entries are applied.
        let planned = lin.plan.len()
            + lin
                .ops
                .iter()
                .filter(|op| matches!(op, TapeOp::Plan { .. }))
                .count();
        self.store.release(&mut lin.pass, planned);
        // The walk fails at pass 1's stop: what precedes it is applied.
        let stopped = lin.pass.stopped().map(str::to_owned);
        let started = std::time::Instant::now();
        let ops = std::mem::take(&mut lin.ops);
        // snapshot-unrecorded: a diagnostics dump of the tape; it changes nothing the analysis computes.
        if let Some(path) = std::env::var_os("BAZEL_RS_EVAL_TAPE_DUMP") {
            use std::io::Write as _;
            // snapshot-unrecorded: writes the diagnostics dump requested above.
            if let Ok(mut file) = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(path)
            {
                for op in &ops {
                    let _ = match op {
                        TapeOp::Reach {
                            node, key, enter, ..
                        } => writeln!(file, "Reach {node:?} {key} enter={enter}"),
                        op => writeln!(file, "{op:?}"),
                    };
                }
            }
        }
        for op in ops {
            // The tracker's part, applied where the walk's tracker records
            // it: after the phase 2 of the flushes before it (S3: kept by
            // position while a deferred flush's walk is pending).
            if lin.tracker.is_some()
                && let Some(track) = lin.replay.compact(&op)
            {
                if lin.track.is_empty() && lin.segments.is_empty() {
                    if let Some(tracker) = lin.tracker.as_deref_mut() {
                        lin.replay.replay(tracker, track);
                    }
                } else {
                    lin.track.push_back((lin.position as u32, track));
                }
            }
            // What a node observed reaches the recording where the walk
            // runs it (S2, a pooled evaluation's attempt frames): a
            // configured target where it is entered, a memo at its first
            // use.
            match &op {
                TapeOp::Reach {
                    node, enter: true, ..
                }
                | TapeOp::Effect(Event::MemoUse(node)) => {
                    self.store.replayed.insert(*node);
                    if let Some(NodeKey::Platform(platform)) = self.store.keys.get(node) {
                        lin.used_platforms.insert(platform.clone());
                    }
                    if let Some(roots) = self.store.roots.remove(node) {
                        // An extension group's commit among them writes
                        // its output here (eval-round1), an effect at the
                        // node's position (S3: after deferred flushes'
                        // effects before it).
                        for write in commit_roots(&self.engine.repositories, &roots) {
                            self.effect(lin, super::pass2::Op::Console(write));
                        }
                    }
                    if let Some(observations) = self.store.observations.remove(node) {
                        crate::observed_fs::commit_to_recorder(observations);
                    }
                    if let Some(NodeKey::Pkg(package)) = self.store.keys.get(node) {
                        let package = package.clone();
                        self.store.replayed_packages.insert(package.clone());
                        if let Some(observations) = self.store.package_observations.remove(&package)
                        {
                            crate::observed_fs::commit_to_recorder(observations);
                        }
                    }
                }
                _ => {}
            }
            use super::pass2::Op;
            match op {
                TapeOp::Effect(Event::Print(line)) => self.effect(lin, Op::Print(line)),
                TapeOp::Effect(Event::Console(index)) => {
                    if let Some(write) = self
                        .store
                        .consoles
                        .get_mut(index as usize)
                        .and_then(Option::take)
                    {
                        self.effect(lin, Op::Console(write));
                    }
                }
                TapeOp::Effect(Event::Info(text)) => self.effect(lin, Op::Info(text)),
                TapeOp::Effect(Event::Warn(warning)) => self.effect(lin, Op::Warn(warning)),
                TapeOp::Effect(Event::LoadPrints(prints)) => {
                    self.effect(lin, Op::LoadPrints(prints));
                }
                // The engine's packages at once (work ahead of its turn
                // reads them; the walk loaded none of them itself).
                TapeOp::Effect(Event::PkgCommit(package, _)) => self.commit_package(&package),
                TapeOp::Plan { key, .. } => {
                    let rule = self
                        .ct1_values
                        .get(&key)
                        .and_then(|value| value.outcome.as_ref().ok())
                        .map(|ok| Arc::clone(&ok.rule))
                        .ok_or_else(|| {
                            format!(
                                "internal error: BAZEL_RS_ANALYSIS=eval: {} is planned without its configured rule",
                                key.key
                            )
                        })?;
                    lin.plan.push((key.key, rule));
                }
                TapeOp::Flush => {
                    STATS[STAT_APPLY].fetch_add(
                        started.elapsed().as_micros() as u64,
                        std::sync::atomic::Ordering::Relaxed,
                    );
                    let point = lin.pass.take_flush_point();
                    let flushed = self.flush(lin, shared, point);
                    STATS[STAT_APPLY].fetch_sub(
                        started.elapsed().as_micros() as u64,
                        std::sync::atomic::Ordering::Relaxed,
                    );
                    flushed?;
                }
                _ => {}
            }
        }
        STATS[STAT_APPLY].fetch_add(
            started.elapsed().as_micros() as u64,
            std::sync::atomic::Ordering::Relaxed,
        );
        // What the tape's commits found (eval-round1: an extension group
        // the pool used speculatively, prepared otherwise).
        if let Some(mismatch) = super::super::nodes::bodies::rerun_mismatch() {
            self.store.rerun_mismatch.get_or_insert(mismatch.clone());
            return Err(format!(
                "internal error: BAZEL_RS_ANALYSIS=eval: {mismatch}; rerun with BAZEL_RS_ANALYSIS=walk"
            ));
        }
        match stopped {
            // The walk stops at pass 1's stop: the flushes before it ran.
            Some(error) => {
                self.catch_up(lin, shared, lin.position)?;
                Err(error)
            }
            None => Ok(()),
        }
    }

    /// Pass 1 as far as the transcripts go, its tape applied, and every
    /// deferred flush's phase 2 run (what a body that needs the planning
    /// state, a failure or the end of phase 1 sees: the ordered walk's).
    pub(crate) fn linearize_all(&mut self, shared: &EvalShared) -> Result<(), String> {
        self.linearize(shared)?;
        let Some(mut lin) = self.lin.take() else {
            return Ok(());
        };
        if stats_enabled() && lin.tracker.is_some() {
            let (keys, labels, packages) = lin.replay.interned();
            crate::note!(
                "bazel-rs: eval pass 2: tracker ops pending {} (capacity {}), interned keys {keys}, labels {labels}, packages {packages}",
                lin.track.len(),
                lin.track.capacity()
            );
        }
        // The end of phase 1 (pass 1 is through every segment, no flag is
        // speculated any more): the transcripts are not read again, freed
        // before the deferred flushes' walk (memory).
        if self.store.segments_done && lin.pass.blocked_on().is_none() && lin.defer {
            self.store.transcripts = FxHashMap::default();
            self.store.release_pending = Vec::new();
            self.store.carry = FxHashMap::default();
            // What the nodes the tape never replayed observed (it will not:
            // no transcript is left), and the source lookups' sets (the
            // sources resolved are memoized or kept with theirs).
            self.store.observations = FxHashMap::default();
            self.store.package_observations = FxHashMap::default();
            publish_sources(self.engine, shared, &mut lin);
            *shared
                .loading
                .source_observed
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner) = FxHashMap::default();
            // The tracker's interned names are not looked up again (a
            // later tape op interns anew).
            lin.replay.forget_interned_ids();
        }
        let result = match &lin.failure {
            Some(failure) => Err(failure.error.clone()),
            None => {
                let position = lin.position;
                if lin.segments.is_empty() && lin.pending.is_empty() {
                    Ok(())
                } else {
                    super::pass2::bump(super::pass2::STAT_FORCED, 1);
                    self.catch_up(&mut lin, shared, position)
                }
            }
        };
        self.lin = Some(lin);
        result
    }

    /// Whether flushes may be deferred (S3, [`super::pass2`]): a flush is
    /// then a marker in the sequential pass too.
    pub(crate) fn defers(&self) -> bool {
        self.lin.as_ref().is_some_and(|lin| lin.defer)
    }

    /// Apply an effect of the tape now, or keep it for the walk of the
    /// deferred flushes before it (S3, [`super::pass2`]).
    fn effect(&mut self, lin: &mut Linearizer, op: super::pass2::Op) {
        if lin.pending.is_empty() && lin.segments.is_empty() {
            let engine = &mut *self.engine;
            lin.direct(|_| engine.apply_pass2_op(op));
            self.store.warnings_seen = self.engine.warnings.len();
        } else {
            lin.pending.push_back((lin.position, op));
        }
    }

    /// The phase 2 of the deferred flushes' segments up to plan position
    /// `end` (one walk), each effect of the tape applied at its position
    /// (S3, [`super::pass2`]). A failure is the flush's whose segment it
    /// is in (`Linearizer::failure`).
    fn catch_up(
        &mut self,
        lin: &mut Linearizer,
        shared: &EvalShared,
        end: usize,
    ) -> Result<(), String> {
        if let Some(failure) = &lin.failure {
            return Err(failure.error.clone());
        }
        let walked = self
            .engine
            .action_planning
            .as_ref()
            .map_or(0, |state| state.done);
        if walked < end {
            // The sources whose lookups' observations are recorded by now
            // (the tape's commits applied first).
            if !lin.unrecorded_sources.is_empty() {
                crate::observed_fs::wait_commits();
            }
            for (label, path, captured) in std::mem::take(&mut lin.unrecorded_sources) {
                if captured.is_recorded() {
                    self.engine.source_targets.entry(label).or_insert(path);
                } else {
                    lin.unrecorded_sources.push((label, path, captured));
                }
            }
            super::pass2::bump(super::pass2::STAT_WALKS, 1);
            super::pass2::bump(super::pass2::STAT_WALKED, (end - walked) as u64);
            let last_segment = lin
                .segments
                .iter()
                .rev()
                .find(|(start, _, _)| *start < end)
                .map_or(walked, |(start, _, _)| *start);
            // The ops due before the walk's end move into the engine.
            let mut due = std::collections::VecDeque::new();
            while lin
                .pending
                .front()
                .is_some_and(|(position, _)| *position < end)
            {
                due.extend(lin.pending.pop_front());
            }
            let mut track = std::collections::VecDeque::new();
            while lin
                .track
                .front()
                .is_some_and(|(position, _)| (*position as usize) < end)
            {
                track.extend(lin.track.pop_front());
            }
            let replay = std::mem::replace(&mut lin.replay, TapeReplay::new(0));
            self.engine.eval_pass2 = Some(Box::new(super::pass2::Pending {
                ops: due,
                last_segment,
                turn: walked,
                track,
                replay: Some(Box::new(replay)),
            }));
            let rest = lin.plan.split_off(end);
            let started = std::time::Instant::now();
            let result = self.with_engine(lin, shared, |engine, lin| {
                let Some(mut state) = engine.action_planning.take() else {
                    return (
                        Err("internal error: BAZEL_RS_ANALYSIS=eval: a flush without the planning state"
                            .to_owned()),
                        walked,
                    );
                };
                let result =
                    engine.plan_targets(&mut state, &mut lin.plan, &mut lin.configured, false);
                let done = state.done;
                engine.action_planning = Some(state);
                (result, done)
            });
            STATS[STAT_PHASE2].fetch_add(
                started.elapsed().as_micros() as u64,
                std::sync::atomic::Ordering::Relaxed,
            );
            super::super::timeline::mark(|| {
                format!(
                    "a pass-2 walk of plan [{walked}, {end}) in {} ms",
                    started.elapsed().as_millis()
                )
            });
            let mut pending = self.engine.eval_pass2.take();
            if let Some(replay) = pending.as_mut().and_then(|pending| pending.replay.take()) {
                lin.replay = *replay;
            }
            let (result, done) = result;
            // What the walk analyzed itself (a deferred target platform's
            // tree, in the walk's last segment): complete for pass 1 from
            // now on.
            for (key, _) in &lin.plan[end..] {
                lin.pass.external(key.clone(), super::pass1::EXTERNAL, true);
            }
            let added = lin.plan.len() - end;
            if added > 0 {
                super::super::timeline::mark(|| {
                    format!(
                        "the walk to {end} planned {added} entries itself (a deferred platform's tree)"
                    )
                });
            }
            lin.replay.planned_elsewhere(added);
            // Plan positions after `end` moved by what the walk added.
            for (_, planned) in &mut self.store.release_pending {
                if *planned > end {
                    *planned += added;
                }
            }
            lin.plan.extend(rest);
            if let Err(error) = result {
                // The flush whose segment holds the failing entry.
                super::pass2::bump(super::pass2::STAT_FAILURES, 1);
                let failing = done.saturating_sub(1).max(walked);
                let point = lin
                    .segments
                    .iter()
                    .find(|(start, end, _)| *start <= failing && failing < *end)
                    .or_else(|| lin.segments.back())
                    .and_then(|(_, _, point)| point.clone());
                lin.pending.clear();
                lin.track.clear();
                lin.segments.clear();
                lin.failure = Some(FlushFailure {
                    error: error.clone(),
                    point,
                    delivered: false,
                });
                return Err(error);
            }
            // Ops the walk did not reach (none: it ran to `end`).
            if let Some(pending) = pending {
                for op in pending.ops.into_iter().rev() {
                    lin.pending.push_front(op);
                }
                for op in pending.track.into_iter().rev() {
                    lin.track.push_front(op);
                }
            }
            while lin
                .segments
                .front()
                .is_some_and(|(_, segment_end, _)| *segment_end <= end)
            {
                lin.segments.pop_front();
            }
            if lin.position < lin.plan.len() && added > 0 {
                lin.position += added;
            }
        } else {
            while lin
                .segments
                .front()
                .is_some_and(|(_, segment_end, _)| *segment_end <= end)
            {
                lin.segments.pop_front();
            }
        }
        // The effects after the walk's last flush.
        while lin
            .pending
            .front()
            .is_some_and(|(position, _)| *position <= end)
        {
            if let Some((_, op)) = lin.pending.pop_front() {
                let engine = &mut *self.engine;
                lin.direct(|_| engine.apply_pass2_op(op));
            }
        }
        while let Some((position, op)) = lin.track.front().copied()
            && position as usize <= end
        {
            lin.track.pop_front();
            if let Some(tracker) = lin.tracker.as_deref_mut() {
                lin.replay.replay(tracker, op);
            }
        }
        self.store.warnings_seen = self.engine.warnings.len();
        self.store.free_released(end);
        // The FeatureFlagInfo values speculated for config_settings (S2,
        // the CsMatch split) against what the walk planned.
        self.verify_speculated();
        // Sticky: a caller that ignores this result (an idle step, the
        // sequential pass's begin) must not lose it; every later
        // linearization and the end of phase 1 fail with it.
        match self.sticky_mismatch() {
            Some(mismatch) => Err(mismatch),
            None => Ok(()),
        }
    }

    /// The internal error a speculated flag, a rerun or an extension
    /// adoption recorded, if any (kept: it fails the build).
    pub(crate) fn sticky_mismatch(&mut self) -> Option<String> {
        if let Some(mismatch) = super::super::nodes::bodies::rerun_mismatch() {
            self.store.rerun_mismatch.get_or_insert(mismatch);
        }
        self.store.rerun_mismatch.clone().map(|mismatch| {
            format!(
                "internal error: BAZEL_RS_ANALYSIS=eval: {mismatch}; rerun with BAZEL_RS_ANALYSIS=walk"
            )
        })
    }

    /// A package the evaluation loaded commits into the engine here
    /// (`Engine::commit_loaded_package`), unless the engine loaded it
    /// itself meanwhile (a flush's phase 2).
    fn commit_package(&mut self, package: &crate::label::PackageId) {
        let loaded = match self.store.packages.get(package) {
            Some(Ok(loaded)) => Some(Arc::clone(loaded)),
            _ => None,
        };
        super::pass2::commit_package(self.engine, package, loaded.as_deref());
    }

    /// The walk's `plan_pending` at a `Flush` of the tape: the done values
    /// published (the configured targets now, the memo values at the
    /// flush's position), then phase 2 of the plan so far — now, or later
    /// in one walk with the next flushes' (S3, [`super::pass2`]).
    fn flush(
        &mut self,
        lin: &mut Linearizer,
        shared: &EvalShared,
        point: Option<super::pass1::FlushPoint>,
    ) -> Result<(), String> {
        use super::pass2::{Op, STAT_DEFERRED, STAT_NEW_PLATFORM, STAT_ORDERED_MODE, bump};
        let memos = stat(STAT_PUBLISH, || self.publish_values(lin, shared));
        let mut new_platform = false;
        lin.flushes += 1;
        for memo in memos {
            if let Published::Platform(key, _) = &memo {
                let new = lin.platforms.insert(key.clone());
                // snapshot-unrecorded: a diagnostics log of where platforms are published; it changes nothing the analysis computes.
                if new && std::env::var_os("BAZEL_RS_EVAL_PLATFORM_LOG").is_some() {
                    crate::note!(
                        "bazel-rs: eval platform published at flush {} (plan entry {}): {}",
                        lin.flushes,
                        lin.plan.len(),
                        key
                    );
                }
                new_platform |= new;
            }
            self.effect(lin, Op::Publish(memo));
        }
        let start = lin.position;
        let end = lin.plan.len();
        lin.position = end;
        lin.segments.push_back((start, end, point));
        let reason = if !lin.defer {
            Some(STAT_ORDERED_MODE)
        } else if new_platform && platform_walks() {
            Some(STAT_NEW_PLATFORM)
        } else if self.platform_unresolved(lin, shared, start, end) {
            Some(super::pass2::STAT_PLATFORM_UNRESOLVED)
        } else {
            None
        };
        match reason {
            None => {
                bump(STAT_DEFERRED, 1);
                Ok(())
            }
            Some(reason) => {
                bump(reason, 1);
                if new_platform {
                    // The deferred segments first, without the platform
                    // (the ordered walk planned them before it).
                    self.catch_up(lin, shared, start)?;
                }
                self.catch_up(lin, shared, end)
            }
        }
    }

    /// Whether the plan segment `[start, end)` holds a Starlark rule or a
    /// genrule whose configuration's target platform is not resolved: its
    /// phase 2 may resolve it (and plan the platform's tree at the flush).
    /// The no-config configuration's rules (licenses, package metadata)
    /// have no target platform to resolve (a walk that does is the
    /// internal error of `Engine::eval_pass2_resolves_platform`).
    fn platform_unresolved(
        &self,
        lin: &mut Linearizer,
        shared: &EvalShared,
        start: usize,
        end: usize,
    ) -> bool {
        let Linearizer {
            plan,
            platforms,
            target_platforms,
            ..
        } = lin;
        for (key, rule) in &plan[start..end] {
            if !matches!(rule.kind, RuleKind::Custom | RuleKind::Genrule) {
                continue;
            }
            let platform = target_platforms.entry(key.config).or_insert_with(|| {
                let configurations = shared
                    .configurations
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                let native = &configurations.get(key.config).native;
                (!native.no_config)
                    .then(|| ConfiguredTargetKey::new(native.target_platform().clone(), key.config))
            });
            if let Some(platform) = platform
                && !platforms.contains(platform)
                && !self.engine.platforms.contains_key(platform)
            {
                return true;
            }
        }
        false
    }

    /// The providers the planning state holds for `key`, once a flush
    /// planned it (`None`: not planned yet).
    pub(crate) fn planned_providers(
        &self,
        key: &ConfiguredTargetKey,
    ) -> Option<Option<super::super::nodes::speculate::Providers>> {
        let state = self.engine.action_planning.as_ref()?;
        state
            .dry
            .contains_key(key)
            .then(|| state.custom_providers.get(key).cloned())
    }

    fn planned_flag(
        &self,
        key: &ConfiguredTargetKey,
    ) -> Option<Option<crate::feature_flags::FeatureFlagValue>> {
        self.planned_providers(key).map(|providers| {
            providers
                .as_ref()
                .and_then(|providers| providers.get(crate::feature_flags::PROVIDER_ID))
                .and_then(|provider| crate::feature_flags::value_of(provider.value()))
        })
    }

    /// A config_setting read `flag` as `key`'s FeatureFlagInfo, planned
    /// speculatively: compared with the planned one now or at the flush
    /// that plans it.
    pub(crate) fn speculated_flag(
        &mut self,
        key: ConfiguredTargetKey,
        flag: Option<crate::feature_flags::FeatureFlagValue>,
    ) {
        match self.planned_flag(&key) {
            Some(planned) => self.check_flag(&key, &flag, &planned),
            None => self.speculated.push((key, flag)),
        }
    }

    fn check_flag(
        &mut self,
        key: &ConfiguredTargetKey,
        speculated: &Option<crate::feature_flags::FeatureFlagValue>,
        planned: &Option<crate::feature_flags::FeatureFlagValue>,
    ) {
        // snapshot-unrecorded: a test hook (BAZEL_RS_EVAL_TEST_FLAG_MISMATCH=1 makes every check a mismatch); it only ever fails a build.
        let forced = std::env::var_os("BAZEL_RS_EVAL_TEST_FLAG_MISMATCH").is_some();
        if speculated != planned || forced {
            self.store.rerun_mismatch.get_or_insert(format!(
                "the FeatureFlagInfo of {key} planned speculatively ({speculated:?}) differs from the flush's ({planned:?})"
            ));
        }
    }

    fn verify_speculated(&mut self) {
        for (key, flag) in std::mem::take(&mut self.speculated) {
            match self.planned_flag(&key) {
                Some(planned) => self.check_flag(&key, &flag, &planned),
                None => self.speculated.push((key, flag)),
            }
        }
    }

    /// The running config_setting `own` received the failure of a flush
    /// (from its own `flush_pending`): when the flush was its own, the
    /// error propagates as the walk's.
    pub(crate) fn flush_failure_delivered(&mut self, own: NodeId) {
        if let Some(failure) = self.lin.as_mut().and_then(|lin| lin.failure.as_mut())
            && failure
                .point
                .as_ref()
                .is_some_and(|point| point.chain.last() == Some(&own))
        {
            failure.delivered = true;
        }
    }

    /// The walk's error for a flush of the tape that failed in a
    /// config_setting the pool analyzed (S2, the CsMatch split: it read
    /// its flags speculatively and finished): the walk re-analyzes the
    /// requested target of the flush's segment on the published values,
    /// the chain's nodes left out, and its `plan_pending` fails with the
    /// flush's error there, so every consumer on the chain words it as the
    /// walk does. Its effects are dropped (the tape wrote them already).
    /// `None`: no such failure, or reported already.
    pub(crate) fn flush_failure(&mut self, shared: &EvalShared, top: ConfigId) -> Option<String> {
        let mut lin = self.lin.take()?;
        let failure = match lin.failure.as_mut() {
            Some(failure) if !failure.delivered => {
                failure.delivered = true;
                failure.clone()
            }
            _ => {
                self.lin = Some(lin);
                return None;
            }
        };
        let text = self.walk_failure(&mut lin, shared, &failure, top);
        self.lin = Some(lin);
        Some(text)
    }

    fn walk_failure(
        &mut self,
        lin: &mut Linearizer,
        shared: &EvalShared,
        failure: &FlushFailure,
        top: ConfigId,
    ) -> String {
        let Some(point) = &failure.point else {
            return failure.error.clone();
        };
        let Some(SegmentKind::Root {
            label,
            trimmed_root,
            ..
        }) = &point.kind
        else {
            return failure.error.clone();
        };
        // The chain is being analyzed in the walk: none of it is a memo hit.
        for node in &point.chain {
            match self.store.keys.get(node) {
                Some(NodeKey::Ct1(ct)) => {
                    lin.configured.remove(&ct.key);
                }
                Some(NodeKey::Platform(key)) => {
                    self.engine.platforms.remove(key);
                }
                Some(NodeKey::RegToolchains(config)) => {
                    self.engine.registered_toolchains.remove(config);
                }
                Some(NodeKey::RegExecPlatforms(config)) => {
                    self.engine.registered_execution_platforms.remove(config);
                }
                _ => {}
            }
        }
        let tracker = lin.tracker.take();
        let warnings = self.engine.warnings.len();
        let counts = crate::progress::analysis_counts();
        self.engine.eval_flush_failure = Some(failure.error.clone());
        let result = self.with_engine(lin, shared, |engine, lin| {
            let console = crate::ui::DeferredConsole::start();
            let scope = crate::observed_fs::unit_scope();
            engine.requested_trimmed_root = trimmed_root.clone();
            let mut active = Vec::new();
            let mut plan = Plan::new();
            let result = engine.analyze(label, top, &mut active, &mut lin.configured, &mut plan);
            engine.requested_trimmed_root = None;
            drop(scope);
            drop(console.finish());
            result
        });
        self.engine.eval_flush_failure = None;
        self.engine.warnings.truncate(warnings);
        self.store.warnings_seen = self.engine.warnings.len();
        crate::progress::restore_analysis_counts(counts);
        lin.tracker = tracker;
        result.err().unwrap_or_else(|| failure.error.clone())
    }

    /// Run `f` with the engine as the walk has it: the configurations
    /// (the evaluator's shared view holds them), the tracker, a direct
    /// console.
    fn with_engine<R>(
        &mut self,
        lin: &mut Linearizer,
        shared: &EvalShared,
        f: impl FnOnce(&mut Engine, &mut Linearizer) -> R,
    ) -> R {
        let paused = super::capture::pause(true);
        let engine = &mut *self.engine;
        let mut configurations = shared
            .configurations
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut aspects = shared
            .aspects
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        // The source-directory probes too: phase 2's source lookups (a
        // frozen walk's workers' among them) hit the evaluation's (moved,
        // not locked: the pool's lookups go on; what they probe meanwhile
        // joins them after).
        let probes = std::mem::take(
            &mut *shared
                .loading
                .probes
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
        );
        std::mem::swap(&mut engine.configurations, &mut *configurations);
        std::mem::swap(&mut engine.aspects, &mut *aspects);
        let own = std::mem::replace(&mut *engine.source_dir_probes, probes);
        engine.incremental = lin.tracker.take();
        let result = lin.direct(|lin| f(engine, lin));
        lin.tracker = engine.incremental.take();
        let mut probes = std::mem::replace(&mut *engine.source_dir_probes, own);
        {
            let mut shared_probes = shared
                .loading
                .probes
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            for (repository, paths) in std::mem::take(&mut *shared_probes) {
                let known = probes.entry(repository).or_default();
                for (path, probe) in paths {
                    known.entry(path).or_insert(probe);
                }
            }
            *shared_probes = probes;
        }
        std::mem::swap(&mut engine.aspects, &mut *aspects);
        std::mem::swap(&mut engine.configurations, &mut *configurations);
        // What it reported is at its place already.
        self.store.warnings_seen = engine.warnings.len();
        super::capture::pause(paused);
        result
    }

    /// Publish the done values into the engine's maps (the walk's writes
    /// phase 2 reads): the configured targets now; the memo values are
    /// returned (they apply at the flush's plan position).
    fn publish_values(&mut self, lin: &mut Linearizer, shared: &EvalShared) -> Vec<Published> {
        let engine = &mut *self.engine;
        let started = std::time::Instant::now();
        // The configured targets pass 1 decided (the first variant reached
        // in DFS order), in its order.
        let mut canonical = std::mem::take(&mut lin.unpublished);
        canonical.extend(lin.pass.take_canonical());
        for node in canonical {
            let Some(NodeKey::Ct1(ct)) = self.store.keys.get(&node) else {
                continue;
            };
            let Some(value) = self.ct1_values.get(ct) else {
                // Its body is still running (past its `Done`).
                lin.unpublished.push(node);
                continue;
            };
            let Ok(ok) = &value.outcome else {
                continue;
            };
            publish_ct1(engine, &mut lin.configured, ct.key.clone(), ok);
        }
        super::super::timeline::add("publication: configured targets", started);
        let started = std::time::Instant::now();
        // `toolchain_implementations`: pass 1's first claims in DFS order.
        // (The new ones: every one before was published already.)
        for implementation in lin.pass.take_new_toolchain_implementations() {
            if let Some(platform) = lin.pass.toolchain_implementations().get(&implementation) {
                engine
                    .toolchain_implementations
                    .entry(implementation)
                    .or_insert_with(|| platform.clone());
            }
        }
        for side in std::mem::take(&mut self.store.sources_side) {
            side.apply(&mut engine.missing_sources, &mut engine.source_directories);
        }
        // S3: the source files the evaluation resolved, as the walk's
        // lookups memoize them (phase 2's prepares off the coordinator read
        // them). A memo hit observes nothing: while recording, a source is
        // memoized once what its lookup observed is recorded (a replayed
        // attempt used it), else later (or never: phase 2 then looks it up
        // and observes, as the walk's first lookup).
        if lin.defer {
            publish_sources(engine, shared, lin);
        }
        let fresh = self.store.published[lin.published_memos..].to_vec();
        lin.published_memos = self.store.published.len();
        // A platform waits for its first use on the tape (order
        // invariance: the pool may finish it long before the walk's
        // position resolves it).
        let mut memos = Vec::with_capacity(fresh.len());
        let held = std::mem::take(&mut lin.held_platforms);
        for memo in held.into_iter().chain(fresh) {
            match &memo {
                Published::Platform(key, _) if !lin.used_platforms.contains(key) => {
                    lin.held_platforms.push(memo);
                }
                _ => memos.push(memo),
            }
        }
        super::super::timeline::add("publication: the rest", started);
        memos
    }

    /// Publish everything done into the engine's maps (the end of phase
    /// 1, the requested target's compatibility check).
    /// `end`: the end of phase 1, where also the platforms the tape has
    /// not used are published.
    fn publish(&mut self, lin: &mut Linearizer, shared: &EvalShared, end: bool) {
        let mut memos = self.publish_values(lin, shared);
        if end {
            memos.append(&mut lin.held_platforms);
        }
        for memo in memos {
            if let Published::Platform(key, _) = &memo {
                lin.platforms.insert(key.clone());
            }
            self.effect(lin, super::pass2::Op::Publish(memo));
        }
    }

    /// The walk's phase 1 of `root` (a done CT1 node) on its own, as a
    /// requested target in a fresh walk would run it: a pass 1 over the
    /// subtree's transcripts (S2, the CsMatch split: what a speculative
    /// phase 2 of a config_setting's Starlark flag plans). `Err`: the
    /// subtree cannot be replayed now (a transcript not complete or freed,
    /// a cycle, a divergence).
    pub(crate) fn subtree(
        &self,
        root: NodeId,
        known: &dyn Fn(&ConfiguredTargetKey) -> bool,
    ) -> Result<Subtree, String> {
        with_running(|stack| {
            let view = View {
                store: &self.store,
                stack,
                ct1_values: &self.ct1_values,
            };
            let planned = |key: &ConfiguredTargetKey| {
                known(key)
                    || self
                        .engine
                        .action_planning
                        .as_ref()
                        .is_some_and(|state| state.dry.contains_key(key))
            };
            let source = SubtreeSource {
                view: &view,
                kind: SegmentKind::Setup,
                events: vec![Event::Reach(root)],
                freed: std::cell::Cell::new(false),
                planned: &planned,
                imported: std::cell::RefCell::new(Vec::new()),
            };
            let mut pass = Pass1::new(Options {
                no_edges: true,
                ..Options::default()
            });
            let mut ops = Vec::new();
            let progress = pass.resume(&source, &mut ops)?;
            if progress != Progress::Finished {
                return Err(format!("the subtree's replay {progress:?}"));
            }
            if source.freed.get() {
                return Err("a transcript of the subtree was freed".to_owned());
            }
            let mut canonical = Vec::new();
            for node in pass.take_canonical() {
                match self.store.keys.get(&node) {
                    Some(NodeKey::Ct1(ct)) => canonical.push(ct.clone()),
                    _ => return Err("a configured target without its key".to_owned()),
                }
            }
            let mut plan = Vec::new();
            for op in ops {
                match op {
                    TapeOp::Plan { key, .. } => plan.push(key),
                    TapeOp::Fail(error) => return Err(error),
                    _ => {}
                }
            }
            let out = pass.finish();
            if let Some(divergence) = out.divergences.first() {
                return Err(divergence.clone());
            }
            let imported = source
                .imported
                .into_inner()
                .into_iter()
                .filter(|key| !known(key))
                .collect();
            Ok(Subtree {
                plan,
                canonical,
                imported,
                toolchain_implementations: out.toolchain_implementations,
            })
        })
    }

    /// What pass 1 waits for (the evaluator's frontier: the pool boosts
    /// it, lowest plan position first) and its priority.
    pub(crate) fn frontier(&self) -> Option<(NodeKey, u64)> {
        let lin = self.lin.as_ref()?;
        match lin.pass.blocked_on()? {
            Blocker::Node(node) => Some((
                self.store.keys.get(&node)?.clone(),
                lin.pass.plan().len() as u64,
            )),
            Blocker::Segment => None,
        }
    }

    /// A new top-level segment.
    fn begin_segment(&mut self, kind: SegmentKind) {
        self.drain();
        self.store.segments.push((kind, Vec::new()));
    }
}

/// A configured target's subtree as a fresh walk analyzes it
/// ([`Coord::subtree`]).
pub(crate) struct Subtree {
    /// The plan entries, in the walk's order.
    pub(crate) plan: Vec<CtKey>,
    /// The configured targets (the variant reached first).
    pub(crate) canonical: Vec<CtKey>,
    /// The configured targets a flush planned already (their phase 2 is
    /// the planning state's): not replayed, not in `plan`.
    pub(crate) imported: Vec<ConfiguredTargetKey>,
    /// The first-visit forced execution platforms.
    pub(crate) toolchain_implementations: std::collections::BTreeMap<ConfiguredTargetKey, Label>,
}

/// The store as the source of a subtree's pass 1: one segment reaching
/// the root; a freed transcript is noted (its replay would be empty).
struct SubtreeSource<'a, 'v> {
    view: &'a View<'v>,
    kind: SegmentKind,
    events: Vec<Event>,
    freed: std::cell::Cell<bool>,
    /// Planned already (by a flush, or by the scratch engine before): its
    /// subtree is not replayed.
    planned: &'a dyn Fn(&ConfiguredTargetKey) -> bool,
    imported: std::cell::RefCell<Vec<ConfiguredTargetKey>>,
}

/// A planned configured target's transcript for a subtree's pass 1: done.
const PLANNED: &[Event] = &[Event::Done];

impl Transcripts for SubtreeSource<'_, '_> {
    fn key(&self, node: NodeId) -> &NodeKey {
        self.view.key(node)
    }

    fn ct1(&self, node: NodeId) -> Option<Ct1Record<'_>> {
        let record = self.view.ct1(node)?;
        if let NodeKey::Ct1(ct) = self.view.key(node)
            && (self.planned)(&ct.key)
            && record.complete
        {
            let mut imported = self.imported.borrow_mut();
            if !imported.contains(&ct.key) {
                imported.push(ct.key.clone());
            }
            return Some(Ct1Record {
                events: PLANNED,
                ..record
            });
        }
        if self.view.store.released.contains(&node) {
            self.freed.set(true);
        }
        Some(record)
    }

    fn memo(&self, node: NodeId) -> Option<MemoRecord<'_>> {
        let record = self.view.memo(node)?;
        // The registrations and platforms analyze declarations (toolchain
        // rules, platforms, constraints) whose phase 2 nothing in the
        // subtree reads: their memo values were published already.
        if matches!(
            self.view.key(node),
            NodeKey::RegToolchains(_) | NodeKey::RegExecPlatforms(_) | NodeKey::Platform(_)
        ) && record.complete
        {
            return Some(MemoRecord {
                events: &[],
                ..record
            });
        }
        Some(record)
    }

    fn segment(&self, index: usize) -> Option<SegmentRecord<'_>> {
        (index == 0).then(|| SegmentRecord {
            kind: &self.kind,
            events: &self.events,
            complete: true,
        })
    }

    fn segments_complete(&self) -> bool {
        true
    }

    fn evaluation_done(&self) -> bool {
        // A node without a transcript may still get one: blocked, not an
        // internal error.
        false
    }

    fn ready(&self, node: NodeId) -> bool {
        self.view.ready(node)
    }

    fn same_observable(&self, canonical: NodeId, reached: NodeId) -> bool {
        self.view.same_observable(canonical, reached)
    }

    fn cycle_error(
        &self,
        head: &ConfiguredTargetKey,
        path: &[ConfiguredTargetKey],
        cycle: &[ConfiguredTargetKey],
        trimmed_root: Option<&ConfiguredTargetKey>,
    ) -> String {
        self.view.cycle_error(head, path, cycle, trimmed_root)
    }
}

/// The store as pass 1's source.
struct View<'a> {
    store: &'a Store,
    stack: &'a [Running],
    ct1_values: &'a FxHashMap<CtKey, Arc<Ct1Value>>,
}

impl View<'_> {
    /// A CT1 node's value.
    fn value(&self, node: NodeId) -> Option<&Arc<Ct1Value>> {
        match self.store.keys.get(&node)? {
            NodeKey::Ct1(ct) => self.ct1_values.get(ct),
            _ => None,
        }
    }

    /// A node's events so far, and whether it finished.
    fn events(&self, node: NodeId) -> Option<(&[Event], bool)> {
        if let Some(running) = self.stack.iter().rev().find(|running| running.id == node) {
            return Some((&running.events, false));
        }
        // A parked attempt's events up to its `Missing`: partial until its
        // rerun replaces them (it re-emits them first).
        if let Some(events) = self.store.carry.get(&node) {
            return Some((events, false));
        }
        if self.store.released.contains(&node) {
            return Some((&[], true));
        }
        self.store
            .transcripts
            .get(&node)
            .map(|transcript| (transcript.events.as_slice(), true))
    }
}

impl Transcripts for View<'_> {
    fn key(&self, node: NodeId) -> &NodeKey {
        static UNKNOWN: NodeKey = NodeKey::Status;
        self.store.keys.get(&node).unwrap_or(&UNKNOWN)
    }

    fn events(&self, node: NodeId, _memo: bool) -> Option<(&[Event], bool)> {
        View::events(self, node)
    }

    fn ct1(&self, node: NodeId) -> Option<Ct1Record<'_>> {
        let (events, complete) = self.events(node)?;
        let ok = self
            .value(node)
            .and_then(|value| value.outcome.as_ref().ok());
        Some(Ct1Record {
            events,
            complete,
            delegate: ok.and_then(|ok| ok.delegate.as_ref()),
            empty_configuration: ok.is_some_and(|ok| ok.empty_configuration),
            incompatible: ok.is_some_and(|ok| ok.node.incompatible.is_some()),
        })
    }

    fn memo(&self, node: NodeId) -> Option<MemoRecord<'_>> {
        let (events, complete) = self.events(node)?;
        Some(MemoRecord {
            events,
            complete,
            ok: self.store.memo_ok.get(&node).copied().unwrap_or(false),
        })
    }

    fn segment(&self, index: usize) -> Option<SegmentRecord<'_>> {
        let (kind, events) = self.store.segments.get(index)?;
        Some(SegmentRecord {
            kind,
            events,
            complete: index + 1 < self.store.segments.len() || self.store.segments_done,
        })
    }

    fn segments_complete(&self) -> bool {
        self.store.segments_done
    }

    fn ready(&self, node: NodeId) -> bool {
        self.value(node).is_some()
    }

    /// The consumer of another variant observed what the configured
    /// target's consumers observe: its success and configured node
    /// (providers, incompatibility).
    fn same_observable(&self, canonical: NodeId, reached: NodeId) -> bool {
        match (self.value(canonical), self.value(reached)) {
            (Some(canonical), Some(reached)) => match (&canonical.outcome, &reached.outcome) {
                (Ok(canonical), Ok(reached)) => {
                    format!("{:?}", canonical.node) == format!("{:?}", reached.node)
                }
                (Err(canonical), Err(reached)) => canonical == reached,
                _ => false,
            },
            _ => false,
        }
    }

    fn cycle_error(
        &self,
        head: &ConfiguredTargetKey,
        path: &[ConfiguredTargetKey],
        cycle: &[ConfiguredTargetKey],
        _trimmed_root: Option<&ConfiguredTargetKey>,
    ) -> String {
        // S1: the body words the error the build reports (the inline
        // driver's chain is the walk's); the tape's `Fail` names the cycle.
        format!(
            "cycle in dependency graph at {head}: {} on the path, {} in the cycle",
            path.len(),
            cycle.len()
        )
    }
}

impl Engine {
    /// Whether phase 1 runs on a pool (S2: more than one thread).
    pub(in crate::engine) fn eval_pooled(&self) -> bool {
        phase1_config(self.loading_phase_threads).threads > 1
    }

    /// Phase 1 of `labels` in `top` by the evaluator (see the module
    /// documentation): `configured` and `plan` as the walk's
    /// `analyze_requested` leaves them.
    pub(in crate::engine) fn eval_phase1(
        &mut self,
        labels: &[Label],
        top: ConfigId,
        configured: &mut ConfiguredNodes,
        plan: &mut Plan,
    ) -> Result<(), String> {
        let tracker = self.incremental.take();
        let tracking = tracker.is_some();
        let warnings = self.warnings.clone();
        let target_platform = {
            let top = self.configurations.top_configuration();
            let target = top.native.target_platform().clone();
            (target != crate::configuration::default_host_platform()
                && target != top.native.host_platform)
                .then_some(target)
        };
        super::super::timeline::start();
        crate::repository_demand::DESCRIBE_SENTINELS.store(
            super::super::timeline::on(),
            std::sync::atomic::Ordering::Relaxed,
        );
        super::capture::start(tracking);
        let console = crate::ui::DeferredConsole::start();
        crate::ui::set_count_deferred(true);
        let shared = EvalShared::from_engine(self);
        super::super::timeline::mark(|| "the shared view built".to_owned());
        let mut lin = Linearizer {
            pass: Pass1::new(Options {
                warnings,
                no_edges: true,
            }),
            ops: Vec::new(),
            tracker,
            replay: TapeReplay::new(0),
            configured: std::mem::take(configured),
            plan: std::mem::take(plan),
            published_memos: 0,
            unpublished: Vec::new(),
            console: Some(console),
            failure: None,
            defer: false,
            position: 0,
            pending: Default::default(),
            track: Default::default(),
            segments: Default::default(),
            platforms: FxHashSet::default(),
            used_platforms: FxHashSet::default(),
            held_platforms: Vec::new(),
            flushes: 0,
            target_platforms: FxHashMap::default(),
            unrecorded_sources: Vec::new(),
        };
        let (result, lin) = {
            let mut bodies = Phase1Bodies::new(&shared, self, tracking, true);
            bodies.framed = phase1_config(shared.options.loading_phase_threads).threads > 1;
            {
                let mut coord = bodies.coord();
                coord.store.warnings_seen = coord.engine.warnings.len();
                coord.store.delay_release = bodies.framed;
                // S3 pass 2: a pooled evaluation defers flushes unless
                // --profile records phase 2 and the tape (its spans) in
                // their order. The observation recording is a set (the
                // order of its commits does not show); the tracker's tape
                // ops are kept by position like the tape's other effects.
                lin.defer = bodies.framed
                    && !crate::profile::active()
                    // snapshot-unrecorded: a diagnostics switch choosing the scheduling only; the output is the same either way.
                    && std::env::var_os("BAZEL_RS_EVAL_NO_DEFER").is_none();
                // Its phase 2 then prepares on the workers (S3,
                // parallel::frozen).
                coord.engine.eval_async_phase2 = lin.defer;
                coord.lin = Some(Box::new(lin));
                coord.begin_segment(SegmentKind::Setup);
            }
            if bodies.framed {
                // The speculative phase 2s' scratch engine (S2, the CsMatch
                // split), forked before the pool runs.
                let scratch =
                    super::super::nodes::speculate::Scratch::new(bodies.coord().engine, &shared);
                *bodies.scratch[0]
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(scratch);
            }
            let evaluator =
                Evaluator::new(&bodies, phase1_config(shared.options.loading_phase_threads));
            // S2: with more than one thread, the pool evaluates what the
            // requests need first (attempts only the sequential pass can
            // run park); the walk's order then drives the inline pass over
            // the same graph.
            super::super::timeline::mark(|| {
                "the evaluator built (scratch engine forked)".to_owned()
            });
            let pooled_at = std::time::Instant::now();
            let _ = POOL_START.set(pooled_at);
            let serve = evaluator.pooled() && *SERVE;
            let pool_roots = if evaluator.pooled() {
                let roots = warm_up(&evaluator, &shared, labels, top, target_platform.clone());
                evaluator.set_inline(true);
                // The sequential pass hands what the pool can run to it.
                // snapshot-unrecorded: a diagnostics switch choosing the scheduling only; the output is the same either way.
                evaluator.set_assist(std::env::var_os("BAZEL_RS_EVAL_NO_ASSIST").is_none());
                Some(roots)
            } else {
                None
            };
            let pooled_for = pooled_at.elapsed();
            // snapshot-unrecorded: a diagnostics switch; it changes nothing the analysis computes.
            if std::env::var_os("BAZEL_RS_EVAL_PARKED_DEBUG").is_some() {
                parked_histogram(&evaluator);
            }
            let pool_done = if stats_enabled() {
                evaluator.stats().done
            } else {
                0
            };
            let driving = std::time::Instant::now();
            let result = if serve {
                // The pool serves beside the sequential pass (this thread):
                // what it wakes runs at once, what it needs that the pool
                // runs it waits for.
                shared
                    .loading
                    .pooled
                    .store(true, std::sync::atomic::Ordering::Release);
                let was = super::super::nodes::loading::set_sequential(true);
                evaluator.begin_serving();
                // The configured roots, and the registrations' packages
                // (every toolchain resolution waits for them), on the pool
                // from the start.
                evaluator.start(&pool_roots.clone().unwrap_or_default());
                let result = std::thread::scope(|scope| {
                    if super::super::timeline::on() {
                        let evaluator = &evaluator;
                        scope.spawn(move || {
                            while !evaluator.serving_ends() {
                                super::super::timeline::graph_sample(evaluator.state_counts());
                                std::thread::sleep(std::time::Duration::from_millis(100));
                            }
                        });
                    }
                    if super::super::timeline::watchdog() {
                        let evaluator = &evaluator;
                        scope.spawn(move || {
                            for tick in 0.. {
                                std::thread::sleep(std::time::Duration::from_millis(100));
                                if evaluator.serving_ends() {
                                    break;
                                }
                                if tick % 50 != 49 {
                                    continue;
                                }
                                let seq = super::super::timeline::SEQ_STATE
                                    .lock()
                                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                                    .chars()
                                    .take(4000)
                                    .collect::<String>();
                                eprintln!(
                                    "bazel-rs: watchdog: {} | sequential: {seq}",
                                    evaluator.watchdog_report()
                                );
                            }
                        });
                    }
                    let server = std::thread::Builder::new()
                        .name("eval-serve".to_owned())
                        .stack_size(PHASE1_STACK)
                        .spawn_scoped(scope, || evaluator.serve())
                        .expect("the evaluator's serving thread");
                    let result = drive(
                        &bodies,
                        &evaluator,
                        &shared,
                        labels,
                        top,
                        target_platform,
                        pool_roots.as_deref(),
                    );
                    evaluator.end_serving();
                    if let Err(panic) = server.join() {
                        std::panic::resume_unwind(panic);
                    }
                    result
                });
                evaluator.serving_done();
                super::super::nodes::loading::set_sequential(was);
                shared
                    .loading
                    .pooled
                    .store(false, std::sync::atomic::Ordering::Release);
                result
            } else {
                drive(
                    &bodies,
                    &evaluator,
                    &shared,
                    labels,
                    top,
                    target_platform,
                    pool_roots.as_deref(),
                )
            };
            DRIVE[2].fetch_add(
                driving.elapsed().as_micros() as u64,
                std::sync::atomic::Ordering::Relaxed,
            );
            if stats_enabled() {
                let stats = evaluator.stats();
                crate::note!(
                    "bazel-rs: eval phase 1: threads {}, pool {} ms ({} nodes done), nodes {}, runs {}, parks {}, quiescences {}, loads left to the sequential pass {}",
                    evaluator.config().threads,
                    pooled_for.as_millis(),
                    pool_done,
                    stats.done,
                    stats.runs,
                    stats.parks,
                    stats.quiescences,
                    super::super::nodes::loading::POOL_INLINE_LOADS
                        .load(std::sync::atomic::Ordering::Relaxed)
                );
                let reasons = &super::super::nodes::bodies::INLINE_REASONS;
                let reason =
                    |index: usize| reasons[index].load(std::sync::atomic::Ordering::Relaxed);
                crate::note!(
                    "bazel-rs: eval phase 1: left to the sequential pass: flushes {}, repository sentinels {}, order-dependent {}, obtained roots {}",
                    reason(0),
                    reason(1),
                    reason(2),
                    reason(3)
                );
                let adoptions = &crate::repository::EVAL_ADOPTIONS;
                let adoption =
                    |index: usize| adoptions[index].load(std::sync::atomic::Ordering::Relaxed);
                crate::note!(
                    "bazel-rs: eval phase 1: speculative extension preparations on the pool: used {}, refused {}, committed by the tape {}, repositories executed ahead {}",
                    adoption(0),
                    adoption(1),
                    adoption(2),
                    adoption(3)
                );
                let rounds = std::mem::take(
                    &mut *ROUNDS
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner),
                );
                let big: Vec<String> = rounds
                    .iter()
                    .enumerate()
                    .filter(|(_, (done, time, _))| *done >= 1000 || time.as_millis() >= 100)
                    .map(|(index, (done, time, at))| {
                        format!(
                            "#{} {done} in {} ms at +{} ms",
                            index + 2,
                            time.as_millis(),
                            at.as_millis()
                        )
                    })
                    .collect();
                crate::note!(
                    "bazel-rs: eval phase 1: later pool rounds {} ({} nodes in the first 16, {} ms); with >= 1000 nodes: {}",
                    rounds.len(),
                    rounds.iter().map(|(done, _, _)| done).sum::<u64>(),
                    rounds
                        .iter()
                        .map(|(_, time, _)| time.as_millis())
                        .sum::<u128>(),
                    big.join(", ")
                );
                let drive =
                    |index: usize| DRIVE[index].load(std::sync::atomic::Ordering::Relaxed) / 1000;
                crate::note!(
                    "bazel-rs: eval phase 1: driver {} ms: pool rounds {} ms, requested targets' lookups and evaluations {} ms, their linearization {} ms (the linearizer's flushes are in both)",
                    drive(2),
                    drive(0),
                    drive(1),
                    drive(3)
                );
                crate::note!(
                    "bazel-rs: eval phase 1: the sequential attempts applied the tape for {} ms; requested-target lookups {} ms",
                    super::super::nodes::bodies::BEGIN_LINEARIZE
                        .load(std::sync::atomic::Ordering::Relaxed)
                        / 1000,
                    LOOKUPS.load(std::sync::atomic::Ordering::Relaxed) / 1000
                );
                let locks = &super::super::nodes::bodies::COORD_LOCKS;
                let locks = |index: usize| locks[index].load(std::sync::atomic::Ordering::Relaxed);
                crate::note!(
                    "bazel-rs: eval phase 1: coordinator lock: {} uncontended, {} contended, {} ms waited",
                    locks(0),
                    locks(1),
                    locks(2) / 1000
                );
                let spec = &super::super::nodes::speculate::STATS;
                let spec = |index: usize| spec[index].load(std::sync::atomic::Ordering::Relaxed);
                let pass2 = |index: usize| {
                    super::pass2::STATS[index].load(std::sync::atomic::Ordering::Relaxed)
                };
                crate::note!(
                    "bazel-rs: eval pass 2: flushes deferred {}, walked at once: new platform {}, platform unresolved {}, ordered mode {}; forced walks {}; walks {} ({} entries), failures {}",
                    pass2(super::pass2::STAT_DEFERRED),
                    pass2(super::pass2::STAT_NEW_PLATFORM),
                    pass2(super::pass2::STAT_PLATFORM_UNRESOLVED),
                    pass2(super::pass2::STAT_ORDERED_MODE),
                    pass2(super::pass2::STAT_FORCED),
                    pass2(super::pass2::STAT_WALKS),
                    pass2(super::pass2::STAT_WALKED),
                    pass2(super::pass2::STAT_FAILURES)
                );
                crate::note!(
                    "bazel-rs: eval phase 1: speculative flag phase 2s {}, left {}, scratch entries planned {}, {} ms ({} ms holding the coordinator's lock)",
                    spec(0),
                    spec(1),
                    spec(2),
                    spec(3) / 1000,
                    spec(4) / 1000
                );
            }
            // Phase 1's bodies are done: the graph (its interned keys, its
            // entries, the memo values only it holds) and the speculations'
            // scratch engine are dropped before the deferred walks run
            // (memory: the walks' peak).
            drop(evaluator);
            for slot in &bodies.scratch {
                drop(
                    slot.lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .take(),
                );
            }
            crate::alloc_arena::purge();
            crate::perf_mark("plan.eval_graph_dropped");
            super::super::timeline::mark(|| "the evaluator's graph dropped".to_owned());
            let mut coord = bodies.coord();
            coord.drain();
            coord.store.segments_done = true;
            // The deferred flushes' phase 2 (S3, pass 2): a failure is the
            // flush's, worded by the walk.
            let finished = match coord.linearize_all(&shared) {
                Err(error) if result.is_ok() => Err(coord
                    .flush_failure(&shared, top)
                    .map(|error| aborted_report(&coord, &error))
                    .unwrap_or(error)),
                finished => finished,
            };
            super::super::timeline::mark(|| {
                format!("the deferred walks done ({})", linearizer_times())
            });
            let mut lin = coord.lin.take().expect("the linearizer");
            coord.publish(&mut lin, &shared, true);
            let result = result.and(finished).and_then(|()| {
                let resumed = with_running(|stack| {
                    let view = View {
                        store: &coord.store,
                        stack,
                        ct1_values: &coord.ct1_values,
                    };
                    lin.pass.resume(&view, &mut lin.ops)
                });
                match resumed? {
                    Progress::Finished => Ok(()),
                    Progress::Stopped => Err(lin.pass.stopped().unwrap_or_default().to_owned()),
                    Progress::Blocked => Err(
                        "internal error: BAZEL_RS_ANALYSIS=eval: pass 1 did not reach the end of phase 1"
                            .to_owned(),
                    ),
                }
            });
            // A mismatch any step recorded (also one whose result a caller
            // dropped) fails a build that would succeed. A build the walk
            // fails keeps the walk's error: pass 1 stopped there, and an
            // attempt abandoned past a failing flush (its events after it
            // never replayed) may rerun otherwise.
            let result = match (result, coord.sticky_mismatch()) {
                (Ok(()), Some(mismatch)) => Err(mismatch),
                (result, _) => result,
            };
            (result, lin)
        };
        crate::ui::set_count_deferred(false);
        // What the tape committed reaches the recording before phase 2's
        // tail (crate::observed_fs::commit_to_recorder).
        crate::observed_fs::wait_commits();
        let mut lin = lin;
        let leftover = lin
            .console
            .take()
            .map(crate::ui::DeferredConsole::finish)
            .unwrap_or_default();
        super::capture::stop();
        shared.restore(self);
        let Linearizer {
            pass,
            tracker,
            replay,
            configured: done,
            plan: planned,
            ..
        } = *lin;
        if let Some(mut tracker) = tracker {
            replay.finish(&mut tracker);
            self.incremental = Some(tracker);
        }
        *configured = done;
        *plan = planned;
        crate::ui::replay(leftover);
        let linearized = pass.finish();
        // Phase 1's structures are gone (the bodies, the shared view, the
        // linearizer): their pages before the planning tail's peak.
        crate::alloc_arena::purge();
        crate::perf_mark("plan.eval_phase1_done");
        super::super::timeline::mark(|| "phase 1 done".to_owned());
        super::super::timeline::report();
        // snapshot-unrecorded: a diagnostics switch; it changes nothing the analysis computes.
        if std::env::var_os("BAZEL_RS_EVAL_PARKED_DEBUG").is_some() {
            let kinds = std::mem::take(
                &mut *super::super::nodes::bodies::SEQUENTIAL_KINDS
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner),
            );
            crate::note!("bazel-rs: eval phase 1: sequential attempts by kind: {kinds:?}");
        }
        if stats_enabled() {
            crate::note!(
                "bazel-rs: eval phase 1: waited for the recorder's background commits {} ms; frame events applied {} (tree claims {})",
                crate::observed_fs::COMMIT_WAIT_US.load(std::sync::atomic::Ordering::Relaxed)
                    / 1000,
                crate::observed_fs::APPLIED_EVENTS.load(std::sync::atomic::Ordering::Relaxed),
                crate::observed_fs::APPLIED_CLAIMS.load(std::sync::atomic::Ordering::Relaxed)
            );
            let ms = |index: usize| STATS[index].load(std::sync::atomic::Ordering::Relaxed) / 1000;
            crate::note!(
                "bazel-rs: eval phase 1: pass 1 {} ms, tape application {} ms, publication {} ms, flush phase 2 {} ms",
                ms(STAT_PASS1),
                ms(STAT_APPLY),
                ms(STAT_PUBLISH),
                ms(STAT_PHASE2)
            );
        }
        result?;
        if let Some(divergence) = linearized.divergences.first() {
            return Err(format!(
                "internal error: BAZEL_RS_ANALYSIS=eval: context divergence: {divergence}; rerun with BAZEL_RS_ANALYSIS=walk"
            ));
        }
        Ok(())
    }
}

/// Publish a configured target's phase-1 value into `engine`'s maps (the
/// walk's writes phase 2 reads); `false` when `key` was published already.
pub(crate) fn publish_ct1(
    engine: &mut Engine,
    configured: &mut ConfiguredNodes,
    key: ConfiguredTargetKey,
    ok: &Ct1Ok,
) -> bool {
    if configured.contains_key(&key) {
        return false;
    }
    configured.insert(key.clone(), ok.node.clone());
    if let Some(context) = &ok.toolchain_context {
        engine
            .toolchain_contexts
            .insert(key.clone(), context.clone());
    }
    if let Some(groups) = &ok.exec_groups {
        engine
            .exec_group_contexts
            .insert(key.clone(), groups.clone());
    }
    if let Some(config) = ok.exec_config {
        engine.exec_configurations.insert(key.clone(), config);
    }
    for (group, config) in &ok.group_exec_configs {
        engine
            .group_exec_configurations
            .insert((key.clone(), group.clone()), *config);
    }
    if let Some(configs) = &ok.edge_configs {
        engine.edge_configs.insert(key.clone(), configs.clone());
    }
    if let Some(errors) = &ok.deferred_duplicates {
        engine
            .deferred_duplicates
            .insert(key.clone(), errors.clone());
    }
    match &ok.delegate {
        Some(owner) => {
            engine.rule_delegations.insert(key.clone(), owner.clone());
        }
        None => {
            if ok.rule.custom.is_some() || ok.node.incompatible.is_some() {
                engine
                    .configured_rules
                    .insert(key.clone(), Arc::clone(&ok.rule));
            }
        }
    }
    if ok.empty_configuration {
        engine.empty_configuration_rules.insert(key);
    }
    true
}

/// Publish a memo value into `engine`'s maps (first one wins: equal values
/// per key).
pub(in crate::engine) fn publish_memo(engine: &mut Engine, published: &Published) {
    match published {
        Published::Platform(key, result) => {
            engine
                .platforms
                .entry(key.clone())
                .or_insert_with(|| result.clone());
        }
        Published::RegToolchains(config, result) => {
            engine
                .registered_toolchains
                .entry(*config)
                .or_insert_with(|| result.clone());
        }
        Published::RegExecPlatforms(config, result) => {
            engine
                .registered_execution_platforms
                .entry(*config)
                .or_insert_with(|| result.clone());
        }
        Published::ExecCfg(config, platform, exec) => {
            engine
                .exec_transitions
                .entry((*config, platform.clone()))
                .or_insert(*exec);
        }
        Published::FlagScope(label, scoped) => {
            engine.flag_scopes.entry(label.clone()).or_insert(*scoped);
        }
        Published::TransitionDetails(settings, result) => {
            engine
                .transition_details
                .entry(settings.clone())
                .or_insert_with(|| result.clone());
        }
        Published::Source(label) => {
            engine.analyzed_sources.insert(label.clone());
        }
        // StarlarkTransitionCache (a deferred platform's tree that
        // phase 2 analyzes with the walk: a hit replays the prints
        // a miss makes).
        Published::AspectPredicate(memo, value) => {
            engine
                .aspect_predicates
                .entry(memo.clone())
                .or_insert(*value);
        }
        Published::AspectAttrAspects(memo, value) => {
            engine
                .aspect_attr_aspects
                .entry(memo.clone())
                .or_insert_with(|| value.clone());
        }
        Published::AspectToolchains(memo, value) => {
            engine
                .aspect_toolchains
                .entry(memo.clone())
                .or_insert_with(|| Arc::clone(value));
        }
        Published::AspectAttributeConfigs(memo, value) => {
            engine
                .aspect_attribute_configs
                .entry(memo.clone())
                .or_insert_with(|| Arc::clone(value));
        }
        Published::Transition(key, value) => {
            engine
                .transition_cache
                .entry(*key)
                .or_insert_with(|| value.clone());
        }
    }
}

/// Roots a pool unit obtained begin to count here: what their probes
/// observed is replayed into the recording itself (not into a frame some
/// running attempt of this thread has open).
/// Their console writes (an extension group's commit, eval-round1:
/// `RepositorySet::note_committed_roots`) belong to this tape position.
#[must_use]
pub(crate) fn commit_roots(
    repositories: &crate::repository::RepositorySet,
    roots: &[String],
) -> Vec<crate::ui::Deferred> {
    let console = crate::ui::DeferredConsole::start();
    let scope = crate::observed_fs::unit_scope();
    repositories.note_committed_roots(roots);
    crate::observed_fs::commit_to_recorder(scope.finish());
    console.finish()
}

/// BAZEL_RS_EVAL_PARKED_DEBUG: what the parked nodes wait for, by kind.
fn parked_histogram(evaluator: &Evaluator<'_, Phase1Bodies<'_, '_>>) {
    use super::super::graph::{State, Wait};
    let graph = evaluator.graph();
    let mut counts: std::collections::BTreeMap<String, usize> = Default::default();
    for id in graph.ids() {
        let entry = graph.entry(id);
        if entry.state() != State::Waiting {
            continue;
        }
        let wait = entry.inner.lock().wait.clone();
        let what = match &wait {
            Wait::Nodes(nodes) => {
                let waiting: Vec<_> = nodes
                    .iter()
                    .filter(|node| graph.entry(**node).state() != State::Done)
                    .map(|node| {
                        let state = graph.entry(*node).state();
                        format!("{}:{state:?}", graph.key(*node).kind())
                    })
                    .collect();
                format!("nodes {}", waiting.first().cloned().unwrap_or_default())
            }
            Wait::Repo(name) => format!("repo {}", name.escape_debug()),
            other => format!("{other:?} {:?}", graph.key(id))
                .chars()
                .take(160)
                .collect(),
        };
        *counts
            .entry(format!("{} on {what}", graph.key(id).kind()))
            .or_default() += 1;
    }
    for (what, count) in counts {
        crate::note!("bazel-rs: parked after the first round: {count} {what}");
    }
}

/// BAZEL_RS_EVAL_TIMELINE: the linearizer's cumulative times.
fn linearizer_times() -> String {
    let ms = |index: usize| STATS[index].load(std::sync::atomic::Ordering::Relaxed) / 1000;
    format!(
        "so far: pass 1 {} ms, tape {} ms, publication {} ms, flush phase 2 {} ms",
        ms(STAT_PASS1),
        ms(STAT_APPLY),
        ms(STAT_PUBLISH),
        ms(STAT_PHASE2)
    )
}

/// BAZEL_RS_EVAL_STATS=1.
fn stats_enabled() -> bool {
    // snapshot-unrecorded: a diagnostics switch; it changes nothing the analysis computes.
    std::env::var_os("BAZEL_RS_EVAL_STATS").is_some()
}

/// Pass 1's events per step while the pool serves.
pub(crate) const LINEARIZE_STEP: usize = 20_000;

/// Whether the pool serves beside the sequential pass
/// (`BAZEL_RS_EVAL_NO_SERVE=1`: rounds between requested targets).
static SERVE: std::sync::LazyLock<bool> = std::sync::LazyLock::new(|| {
    // snapshot-unrecorded: a diagnostics switch choosing the scheduling only; the output is the same either way.
    std::env::var_os("BAZEL_RS_EVAL_NO_SERVE").is_none()
});

/// The pool's worker stack in phase 1: attempts nest depth first on a
/// worker (help-first needs), as deep as the walk's recursion.
const PHASE1_STACK: usize = 512 << 20;

/// How phase 1 evaluates: `--loading_phase_threads` workers (bounded by
/// the cores; 1: the inline driver), help first, big stacks.
fn phase1_config(loading_phase_threads: usize) -> EvalConfig {
    EvalConfig {
        help_first: true,
        stack: PHASE1_STACK,
        ..EvalConfig::for_build(loading_phase_threads, false, None)
    }
}

/// The repository lane of phase 1: a module repository's root a pool
/// unit was blocked on, materialized on the coordinator. Any other
/// repository (fetched ahead by repository speculation), or what only the
/// sequential pass can run, waits for that pass (`bodies::INLINE`).
struct Lane<'a> {
    shared: &'a EvalShared,
}

impl RepoLane for Lane<'_> {
    fn fetch(&mut self, name: &str) {
        super::super::nodes::loading::lane_fetch(self.shared, name);
    }
}

/// At quiescence on the pool: stop (the parked attempts are the
/// sequential pass's).
struct StopCycles;

impl CycleHandler for StopCycles {
    fn at_quiescence(&mut self, _graph: &WaitGraph<'_>) -> Resolution {
        Resolution::Stop
    }
}

/// The pool's evaluation of what the requests need (S2): the explicit
/// target platform and the requested labels' lookups, then their
/// configured targets (keys as their first edge makes them; nothing is
/// decided first-visit here). Attempts only the sequential pass can run
/// park; nothing is linearized. Returns the configured roots for the
/// later rounds ([`pool_round`]).
fn warm_up(
    evaluator: &Evaluator<'_, Phase1Bodies<'_, '_>>,
    shared: &EvalShared,
    labels: &[Label],
    top: ConfigId,
    target_platform: Option<Label>,
) -> Vec<NodeKey> {
    let pooled = &shared.loading.pooled;
    pooled.store(true, std::sync::atomic::Ordering::Release);
    let mut lane = Lane { shared };
    let mut roots: Vec<NodeKey> = target_platform
        .map(|target| NodeKey::Platform(ConfiguredTargetKey::new(target, top)))
        .into_iter()
        .collect();
    roots.extend(labels.iter().map(|label| NodeKey::Tgt(label.clone())));
    super::super::timeline::mark(|| "round 1 begins: the requested lookups".to_owned());
    evaluator.evaluate_round(&roots, &mut lane);
    super::super::timeline::mark(|| "round 1: the requested lookups done".to_owned());
    let no_config = shared
        .configurations
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .get(top)
        .native
        .no_config;
    let roots: Vec<NodeKey> = labels
        .iter()
        .filter_map(|label| {
            let value = evaluator.value(&NodeKey::Tgt(label.clone()))?;
            match value.as_ref() {
                NodeValue::Tgt(Ok(Target::Source(_)), _) => Some(NodeKey::Src(label.clone())),
                NodeValue::Tgt(Ok(Target::Rule(rule) | Target::Output(rule, _)), _) => {
                    super::super::super::check_macro_namespace(rule, label).ok()?;
                    let key = ConfiguredTargetKey::new(rule.label.clone(), top);
                    Some(NodeKey::Ct1(edge_ct_key(no_config, rule, key, false, None)))
                }
                _ => None,
            }
        })
        .collect();
    evaluator.evaluate_round(&roots, &mut lane);
    super::super::timeline::mark(|| {
        format!(
            "round 1 done: {} nodes done of {}",
            evaluator.stats().done,
            evaluator.graph().len()
        )
    });
    pooled.store(false, std::sync::atomic::Ordering::Release);
    roots
}

/// Another round of the pool over `roots` (S2): what the sequential pass
/// unblocked since (an extension it prepared, a repository it fetched)
/// runs in parallel again; then the sequential pass continues.
fn pool_round(
    bodies: &Phase1Bodies<'_, '_>,
    evaluator: &Evaluator<'_, Phase1Bodies<'_, '_>>,
    shared: &EvalShared,
    roots: &[NodeKey],
) {
    // What pass 1 waits for first.
    if let Some((key, priority)) = bodies.coord().frontier() {
        evaluator.boost(&key, priority);
    }
    let pooled = &shared.loading.pooled;
    pooled.store(true, std::sync::atomic::Ordering::Release);
    evaluator.set_inline(false);
    evaluator.retry_unresolvable();
    let started = std::time::Instant::now();
    // The node counts of the first rounds only (an O(nodes) scan).
    let counted = stats_enabled()
        && ROUNDS
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .len()
            < 200;
    let before = (counted || super::super::timeline::on()).then(|| evaluator.stats().done);
    super::super::timeline::mark(|| {
        format!("pool round begins ({} nodes done)", before.unwrap_or(0))
    });
    evaluator.evaluate_round(roots, &mut Lane { shared });
    super::super::timeline::mark(|| {
        let done = evaluator.stats().done;
        format!(
            "pool round done: {} nodes ({done} done of {})",
            done - before.unwrap_or(0),
            evaluator.graph().len()
        )
    });
    if stats_enabled() {
        let elapsed = started.elapsed();
        let done = before.map_or(0, |before| evaluator.stats().done - before);
        ROUNDS
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push((
                done,
                elapsed,
                POOL_START
                    .get()
                    .map(|at| started.duration_since(*at))
                    .unwrap_or_default(),
            ));
    }
    evaluator.set_inline(true);
    pooled.store(false, std::sync::atomic::Ordering::Release);
}

/// BAZEL_RS_EVAL_STATS: the sequential driver's time (µs) in the later
/// pool rounds, and in the requested targets' lookups and evaluations.
static DRIVE: [std::sync::atomic::AtomicU64; 4] = [
    std::sync::atomic::AtomicU64::new(0),
    std::sync::atomic::AtomicU64::new(0),
    std::sync::atomic::AtomicU64::new(0),
    std::sync::atomic::AtomicU64::new(0),
];

/// Adds its lifetime's µs to the counter when dropped.
struct Timed<'a>(&'a std::sync::atomic::AtomicU64, std::time::Instant);

impl Drop for Timed<'_> {
    fn drop(&mut self) {
        self.0.fetch_add(
            self.1.elapsed().as_micros() as u64,
            std::sync::atomic::Ordering::Relaxed,
        );
    }
}

/// BAZEL_RS_EVAL_STATS: the µs of the requested targets' lookups.
static LOOKUPS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// BAZEL_RS_EVAL_STATS: the later pool rounds (nodes done, time).
static ROUNDS: std::sync::Mutex<Vec<(u64, std::time::Duration, std::time::Duration)>> =
    std::sync::Mutex::new(Vec::new());

/// BAZEL_RS_EVAL_STATS: when the pool's first round began.
static POOL_START: std::sync::OnceLock<std::time::Instant> = std::sync::OnceLock::new();

/// The sequential pass's attempts so far.
fn sequential_attempts() -> u64 {
    super::super::nodes::bodies::SEQUENTIAL_ATTEMPTS.load(std::sync::atomic::Ordering::Relaxed)
}

/// The walk's `plan_build` phase 1 over the evaluator: the explicit
/// target platform, then each requested label (`analyze_requested`).
fn drive(
    bodies: &Phase1Bodies<'_, '_>,
    evaluator: &Evaluator<'_, Phase1Bodies<'_, '_>>,
    shared: &EvalShared,
    labels: &[Label],
    top: ConfigId,
    target_platform: Option<Label>,
    pool_roots: Option<&[NodeKey]>,
) -> Result<(), String> {
    // S2: the pool runs again after a requested target's sequential pass
    // did real work (it may have unblocked what the pool parked).
    let mut ran = sequential_attempts();
    let evaluate = |key: &NodeKey| -> Arc<NodeValue> {
        evaluator.evaluate_values(
            std::slice::from_ref(key),
            &mut Lane { shared },
            &mut Pass1Cycles::default(),
        );
        evaluator.value(key).cloned().unwrap_or_else(|| {
            Arc::new(NodeValue::Src(Err(format!(
                "internal error: BAZEL_RS_ANALYSIS=eval: {key:?} did not finish"
            ))))
        })
    };
    if let Some(target) = target_platform {
        let key = NodeKey::Platform(ConfiguredTargetKey::new(target, top));
        let id = evaluator.graph().intern(&key);
        // Engine::platform_info: its shared scope around the memo.
        {
            let mut coord = bodies.coord();
            coord.push_event(Event::ScopeEnter("platform_info"));
            coord.push_event(Event::MemoUse(id));
        }
        let value = evaluate(&key);
        {
            let mut coord = bodies.coord();
            coord.drain();
            coord.push_event(Event::ScopeExit);
        }
        if let NodeValue::Platform(Err(error)) = value.as_ref() {
            return Err(render_chains(shared, &error.clone().target_text(), None));
        }
    }
    let no_config = shared
        .configurations
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .get(top)
        .native
        .no_config;
    // Engine::target at the top level: the lookup's `Access` (while
    // tracking) and its package's commit (a `MemoUse` of its Pkg node) in
    // the current segment, then the Tgt node.
    let lookup = |label: &Label| -> Result<Target, String> {
        let started = std::time::Instant::now();
        let _timed = Timed(&LOOKUPS, started);
        {
            let mut coord = bodies.coord();
            coord.drain();
            if coord.tracking {
                coord.push_event(Event::Access(label.clone()));
            }
            let package = label.package_id();
            if !shared.loading.preloaded.contains_key(&package) {
                let id = evaluator.graph().intern(&NodeKey::Pkg(package));
                coord.push_event(Event::MemoUse(id));
            }
        }
        let value = evaluate(&NodeKey::Tgt(label.clone()));
        bodies.coord().drain();
        match value.as_ref() {
            value @ NodeValue::Tgt(..) => value.tgt(),
            _ => Err(format!(
                "internal error: BAZEL_RS_ANALYSIS=eval: the lookup of {label} did not finish"
            )),
        }
    };
    for (position, label) in labels.iter().enumerate() {
        let started = std::time::Instant::now();
        // Engine::analyze_requested: the test-trimmed top-level key.
        let looked_up = lookup(label);
        let trimmed = match &looked_up {
            Ok(Target::Rule(rule))
                if !forwards(rule) && !matches!(rule.kind, RuleKind::PackageGroup(_)) =>
            {
                Some(ConfiguredTargetKey::new(rule.label.clone(), top))
            }
            _ => None,
        };
        {
            let mut coord = bodies.coord();
            coord.begin_segment(SegmentKind::Root {
                position,
                label: label.clone(),
                trimmed_root: trimmed.clone(),
            });
        }
        // The root edge (Engine::analyze → analyze_key_body): its lookup,
        // then the source file or the configured target.
        let root = {
            let looked_up = lookup(label);
            let mut coord = bodies.coord();
            match looked_up {
                Err(error) => Err(error),
                Ok(Target::Source(_)) => {
                    if coord.tracking {
                        coord.push_event(Event::Source(label.clone()));
                    }
                    let key = NodeKey::Src(label.clone());
                    let id = evaluator.graph().intern(&key);
                    coord.push_event(Event::MemoUse(id));
                    Ok(key)
                }
                Ok(Target::Rule(rule) | Target::Output(rule, _)) => {
                    super::super::super::check_macro_namespace(&rule, label).map(|()| {
                        let key = ConfiguredTargetKey::new(rule.label.clone(), top);
                        let key = NodeKey::Ct1(edge_ct_key(no_config, &rule, key, false, None));
                        let id = evaluator.graph().intern(&key);
                        coord.push_event(Event::Reach(id));
                        key
                    })
                }
            }
        };
        // The linearizer's frontier first (the pool: it runs before the
        // evaluator's own work).
        let frontier = bodies.coord().frontier();
        if let Some((key, priority)) = frontier {
            evaluator.boost(&key, priority);
        }
        let analyzed = root.and_then(|root| match evaluate(&root).as_ref() {
            NodeValue::Src(result) => result.clone(),
            NodeValue::Ct1(value) => value.outcome.as_ref().map(|_| ()).map_err(Clone::clone),
            _ => Err("internal error: a root with another value".to_owned()),
        });
        DRIVE[1].fetch_add(
            started.elapsed().as_micros() as u64,
            std::sync::atomic::Ordering::Relaxed,
        );
        let evaluated = started.elapsed();
        if super::super::timeline::on()
            && (sequential_attempts() != ran || evaluated.as_millis() >= 5)
        {
            super::super::timeline::mark(|| {
                format!(
                    "requested #{position} {label}: evaluated in {} ms, {} sequential attempts",
                    evaluated.as_millis(),
                    sequential_attempts() - ran
                )
            });
        }
        // The requested target's segment renders the chain errors.
        let analyzed = analyzed.map_err(|error| render_chains(shared, &error, trimmed.as_ref()));
        // A flush of this segment that failed where the walk would have
        // run it comes first (S2, the CsMatch split: the pool analyzed past
        // it), worded by the walk's chain.
        let analyzed = match analyzed {
            Err(error) => {
                let mut coord = bodies.coord();
                let _ = coord.linearize_all(shared);
                Err(coord.flush_failure(shared, top).unwrap_or(error))
            }
            ok => ok,
        };
        let linearizing = std::time::Instant::now();
        if evaluator.serving() && analyzed.is_ok() {
            // In steps, the coordinator released in between (the pool's
            // speculations need it).
            while bodies.coord().linearize_step(shared, LINEARIZE_STEP) {}
        }
        // The requested target's compatibility, on the published nodes.
        let analyzed = analyzed.and_then(|()| {
            let mut coord = bodies.coord();
            if let Err(error) = coord.linearize(shared) {
                return Err(coord.flush_failure(shared, top).unwrap_or(error));
            }
            let Some(mut lin) = coord.lin.take() else {
                return Ok(());
            };
            coord.publish(&mut lin, shared, false);
            let checked = coord.with_engine(&mut lin, shared, |engine, lin| {
                engine.check_requested_compatibility(&lin.configured, label, top)
            });
            coord.lin = Some(lin);
            // A deferred flush before it that fails is the walk's error
            // (S3: its phase 2 runs first).
            checked.map_err(|error| match coord.linearize_all(shared) {
                Err(flushed) => coord.flush_failure(shared, top).unwrap_or(flushed),
                Ok(()) => error,
            })
        });
        DRIVE[3].fetch_add(
            linearizing.elapsed().as_micros() as u64,
            std::sync::atomic::Ordering::Relaxed,
        );
        if linearizing.elapsed().as_millis() >= 5 {
            super::super::timeline::mark(|| {
                format!(
                    "requested #{position}: linearized in {} ms ({}; plan {})",
                    linearizing.elapsed().as_millis(),
                    linearizer_times(),
                    bodies.coord().lin.as_ref().map_or(0, |lin| lin.plan.len())
                )
            });
        }
        // Engine::analyze_requested's report of an aborted analysis.
        analyzed.map_err(|error| {
            match error.split_once(super::super::super::ANALYSIS_ABORTED) {
                Some((_, message)) => format!(
                    "Analysis of target '{}' failed; build aborted: {message}",
                    crate::rule_declarations::canonical_form(label)
                ),
                None => error,
            }
        })?;
        if evaluator.serving() {
            // What it ran may have unblocked what the pool left; what the
            // serving pool demanded is fetched here.
            if sequential_attempts() != ran {
                if let Some((key, priority)) = bodies.coord().frontier() {
                    evaluator.boost(&key, priority);
                }
                evaluator.retry_unresolvable();
            }
            evaluator.lane_work(&mut Lane { shared });
        } else if pool_roots.is_some() && sequential_attempts() != ran {
            // What it ran may have unblocked more for the pool.
            let round = std::time::Instant::now();
            pool_round(bodies, evaluator, shared, pool_roots.unwrap_or_default());
            DRIVE[0].fetch_add(
                round.elapsed().as_micros() as u64,
                std::sync::atomic::Ordering::Relaxed,
            );
        }
        ran = sequential_attempts();
    }
    bodies.coord().begin_segment(SegmentKind::RootsDone);
    Ok(())
}

/// Engine::analyze_requested's report of an aborted analysis, for a
/// flush failure found by the end of phase 1's walk (its requested
/// target's).
fn aborted_report(coord: &Coord<'_>, error: &str) -> String {
    let label = coord.lin.as_ref().and_then(|lin| {
        let point = lin.failure.as_ref()?.point.as_ref()?;
        match point.kind.as_ref()? {
            SegmentKind::Root { label, .. } => Some(label.clone()),
            _ => None,
        }
    });
    match (
        error.split_once(super::super::super::ANALYSIS_ABORTED),
        label,
    ) {
        (Some((_, message)), Some(label)) => format!(
            "Analysis of target '{}' failed; build aborted: {message}",
            crate::rule_declarations::canonical_form(&label)
        ),
        _ => error.to_owned(),
    }
}

/// Render the chain errors of a requested target's failure
/// (linearize::chains), with its test-trimmed top-level key.
pub(crate) fn render_chains(
    shared: &EvalShared,
    error: &str,
    trimmed: Option<&ConfiguredTargetKey>,
) -> String {
    if !super::chains::is_open(error) {
        return error.to_owned();
    }
    let root = trimmed.map(|key| super::chains::TrimmedRoot {
        key: key.clone(),
        untrimmed: format!(
            "{} ({})",
            crate::rule_declarations::canonical_form(&key.label),
            shared
                .configurations
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .get(key.config)
                .untrimmed_id()
        ),
    });
    super::chains::close(error, root.as_ref()).0
}

/// `BAZEL_RS_EVAL_PLATFORM_WALKS=1`: a flush publishing a new platform is
/// walked at once (with the deferred segments before it), as before the
/// platform became an op of the deferred walk.
fn platform_walks() -> bool {
    static WALKS: std::sync::LazyLock<bool> = std::sync::LazyLock::new(|| {
        // snapshot-unrecorded: a scheduling switch; the output is the same either way.
        std::env::var_os("BAZEL_RS_EVAL_PLATFORM_WALKS").is_some_and(|value| value == "1")
    });
    *WALKS
}
