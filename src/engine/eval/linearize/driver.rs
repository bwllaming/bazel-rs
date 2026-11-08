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
//! Not covered in S1 (the walk plans these requests): cquery/aquery
//! captures, `--keep_going`, top-level aspects, the cquery root
//! checkpoint (S4).

use std::sync::Arc;

use crate::configuration::ConfigId;
use crate::fast_hash::{FxHashMap, FxHashSet};
use crate::label::Label;
use crate::platforms::PlatformInfo;
use crate::toolchains::RegisteredToolchains;

use super::super::super::incremental::{TapeReplay, Tracker};
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

/// A memo value the engine keeps (phase 2 reads it).
pub(crate) enum Published {
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
    pub(crate) release_pending: Vec<NodeId>,
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
    fn release(&mut self, pass: &mut Pass1) {
        let (nodes, segments) = pass.take_released();
        if self.delay_release {
            self.release_pending.extend(nodes);
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

    /// Free the transcripts released before a flush (it planned them).
    fn free_released(&mut self) {
        for node in std::mem::take(&mut self.release_pending) {
            self.transcripts.remove(&node);
            self.released.insert(node);
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

    /// An attempt of `id` begins: what an abandoned attempt of it kept.
    pub(crate) fn begin_attempt(&mut self, id: NodeId, key: &NodeKey) -> Vec<Event> {
        self.store.keys.entry(id).or_insert_with(|| key.clone());
        self.store.carry.remove(&id).unwrap_or_default()
    }

    /// The running attempt received a `Missing`: it stops here.
    pub(crate) fn missing_here(&mut self) {
        self.drain();
        with_running(|running| {
            if let Some(running) = running.last_mut() {
                running.missing_at.get_or_insert(running.events.len());
            }
        });
    }

    /// The attempt `running` ends with `value`: its transcript is kept.
    pub(crate) fn finish_attempt(&mut self, running: Running, value: &NodeValue) {
        self.store
            .memo_ok
            .insert(running.id, value.error().is_none());
        let published = match (&running.key, value) {
            (NodeKey::Platform(key), NodeValue::Platform(result)) => {
                Some(Published::Platform(key.clone(), result.clone()))
            }
            (NodeKey::RegToolchains(config), NodeValue::RegToolchains(result)) => {
                Some(Published::RegToolchains(*config, result.clone()))
            }
            (NodeKey::RegExecPlatforms(config), NodeValue::RegExecPlatforms(result)) => {
                Some(Published::RegExecPlatforms(*config, result.clone()))
            }
            (NodeKey::ExecCfg(config, platform), NodeValue::ExecCfg(Ok(exec))) => {
                Some(Published::ExecCfg(*config, platform.clone(), *exec))
            }
            (NodeKey::FlagScope(label), NodeValue::FlagScope(Ok(scoped))) => {
                Some(Published::FlagScope(label.clone(), *scoped))
            }
            (NodeKey::TransitionDetails(settings), NodeValue::TransitionDetails(result)) => Some(
                Published::TransitionDetails(settings.clone(), result.clone()),
            ),
            (NodeKey::Src(label), NodeValue::Src(Ok(()))) => Some(Published::Source(label.clone())),
            (NodeKey::Transition(key), NodeValue::Transition(Ok(applied))) => {
                Some(Published::Transition(
                    *key,
                    (
                        Arc::clone(&applied.configurations),
                        applied.printed.to_vec(),
                    ),
                ))
            }
            _ => None,
        };
        self.store.published.extend(published);
        match (&running.key, value) {
            (NodeKey::Ct1(ct), NodeValue::Ct1(ct1)) => {
                self.ct1_done
                    .entry(ct.key.clone())
                    .or_insert_with(|| Arc::clone(ct1));
                self.ct1_values.insert(ct.clone(), Arc::clone(ct1));
            }
            (NodeKey::Src(label), NodeValue::Src(Ok(()))) => {
                self.sources.insert(label.clone());
            }
            (NodeKey::Pkg(package), NodeValue::Pkg(result)) => {
                self.store.packages.insert(package.clone(), result.clone());
            }
            _ => {}
        }
        self.store.transcripts.insert(
            running.id,
            Transcript {
                events: running.events,
            },
        );
    }

    /// The attempt `running` stopped (`Missing`): it reruns from the
    /// start, its events up to the `Missing` stay (pass 1 may have replayed
    /// them; the rerun re-emits them, `bodies::push_running`), with what
    /// happened anyway.
    pub(crate) fn abandon_attempt(&mut self, mut running: Running) {
        let keep = running
            .missing_at
            .unwrap_or(running.events.len())
            .max(running.replay);
        running.events.truncate(keep);
        if !running.events.is_empty() {
            self.store.carry.insert(running.id, running.events);
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

    fn linearize_with(&mut self, lin: &mut Linearizer, shared: &EvalShared) -> Result<(), String> {
        if let Some(failure) = &lin.failure {
            return Err(failure.error.clone());
        }
        let mismatch = super::super::nodes::bodies::rerun_mismatch();
        if let Some(mismatch) = mismatch.or_else(|| self.store.rerun_mismatch.clone()) {
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
        self.store.release(&mut lin.pass);
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
            if let Some(tracker) = lin.tracker.as_deref_mut() {
                lin.replay.apply(tracker, &op);
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
                    if let Some(roots) = self.store.roots.remove(node) {
                        commit_roots(&self.engine.repositories, &roots);
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
            match op {
                TapeOp::Effect(Event::Print(line)) => {
                    lin.direct(|_| crate::ui::note(&line));
                }
                TapeOp::Effect(Event::Console(index)) => {
                    if let Some(write) = self
                        .store
                        .consoles
                        .get_mut(index as usize)
                        .and_then(Option::take)
                    {
                        lin.direct(|_| crate::ui::replay(vec![write]));
                    }
                }
                TapeOp::Effect(Event::Info(text)) => {
                    lin.direct(|_| crate::toolchains::print_debug_event(text));
                }
                TapeOp::Effect(Event::Warn(warning)) => {
                    self.engine.warnings.push(warning);
                    self.store.warnings_seen = self.engine.warnings.len();
                }
                TapeOp::Effect(Event::LoadPrints(prints)) => {
                    let engine = &mut *self.engine;
                    lin.direct(|_| engine.emit_load_prints(prints.to_vec()));
                }
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
                    let flushed = self.flush(lin, shared);
                    STATS[STAT_APPLY].fetch_sub(
                        started.elapsed().as_micros() as u64,
                        std::sync::atomic::Ordering::Relaxed,
                    );
                    if let Err(error) = flushed {
                        lin.failure = Some(FlushFailure {
                            error: error.clone(),
                            point,
                            delivered: false,
                        });
                        return Err(error);
                    }
                }
                _ => {}
            }
        }
        STATS[STAT_APPLY].fetch_add(
            started.elapsed().as_micros() as u64,
            std::sync::atomic::Ordering::Relaxed,
        );
        match stopped {
            Some(error) => Err(error),
            None => Ok(()),
        }
    }

    /// A package the evaluation loaded commits into the engine here
    /// (`Engine::commit_loaded_package`), unless the engine loaded it
    /// itself meanwhile (a flush's phase 2).
    fn commit_package(&mut self, package: &crate::label::PackageId) {
        let engine = &mut *self.engine;
        if engine.packages.contains_key(package) {
            return;
        }
        crate::progress::package_loaded();
        let Some(Ok(loaded)) = self.store.packages.get(package) else {
            return;
        };
        let rules = &loaded.package;
        for rule in rules.values() {
            for out in &rule.outs {
                if let Ok(output) = rule.label.with_name(out) {
                    engine
                        .generated
                        .insert(output, (rule.label.clone(), out.clone()));
                }
            }
        }
        if let Some(custom) = rules.values().find_map(|rule| rule.custom.as_ref()) {
            engine
                .loaded_functions
                .register(&custom.top_level_functions);
        }
        engine.packages.insert(package.clone(), Arc::clone(rules));
    }

    /// The walk's `plan_pending`: phase 2 of the plan so far, the engine
    /// holding the published values, the configurations and the tracker.
    fn flush(&mut self, lin: &mut Linearizer, shared: &EvalShared) -> Result<(), String> {
        stat(STAT_PUBLISH, || self.publish(lin));
        let before = lin.plan.len();
        let started = std::time::Instant::now();
        let result = self.with_engine(lin, shared, |engine, lin| {
            let Some(mut state) = engine.action_planning.take() else {
                return Err(
                    "internal error: BAZEL_RS_ANALYSIS=eval: a flush without the planning state"
                        .to_owned(),
                );
            };
            let result = engine.plan_targets(&mut state, &mut lin.plan, &mut lin.configured, false);
            engine.action_planning = Some(state);
            result
        });
        // What the flush's phase 2 analyzed itself (a deferred target
        // platform's tree): complete for pass 1 from now on.
        for (key, _) in &lin.plan[before..] {
            lin.pass.external(key.clone(), super::pass1::EXTERNAL, true);
        }
        let added = lin.plan.len() - before;
        lin.replay.planned_elsewhere(added);
        STATS[STAT_PHASE2].fetch_add(
            started.elapsed().as_micros() as u64,
            std::sync::atomic::Ordering::Relaxed,
        );
        result?;
        self.store.free_released();
        // The FeatureFlagInfo values speculated for config_settings (S2,
        // the CsMatch split) against what this flush planned.
        self.verify_speculated();
        match self.store.rerun_mismatch.take() {
            Some(mismatch) => Err(format!(
                "internal error: BAZEL_RS_ANALYSIS=eval: {mismatch}; rerun with BAZEL_RS_ANALYSIS=walk"
            )),
            None => Ok(()),
        }
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
        if speculated != planned {
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
        std::mem::swap(&mut engine.configurations, &mut *configurations);
        std::mem::swap(&mut engine.aspects, &mut *aspects);
        engine.incremental = lin.tracker.take();
        let result = lin.direct(|lin| f(engine, lin));
        lin.tracker = engine.incremental.take();
        std::mem::swap(&mut engine.aspects, &mut *aspects);
        std::mem::swap(&mut engine.configurations, &mut *configurations);
        // What it reported is at its place already.
        self.store.warnings_seen = engine.warnings.len();
        super::capture::pause(paused);
        result
    }

    /// Publish the done values into the engine's maps (the walk's writes
    /// phase 2 reads).
    fn publish(&mut self, lin: &mut Linearizer) {
        let engine = &mut *self.engine;
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
        // `toolchain_implementations`: pass 1's first claims in DFS order.
        for (implementation, platform) in lin.pass.toolchain_implementations() {
            engine
                .toolchain_implementations
                .entry(implementation.clone())
                .or_insert_with(|| platform.clone());
        }
        for side in std::mem::take(&mut self.store.sources_side) {
            side.apply(&mut engine.missing_sources, &mut engine.source_directories);
        }
        for published in &self.store.published[lin.published_memos..] {
            publish_memo(engine, published);
        }
        lin.published_memos = self.store.published.len();
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
            let mut pass = Pass1::new(Options::default());
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
    fn key(&self, node: NodeId) -> NodeKey {
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
                imported.push(ct.key);
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
    fn key(&self, node: NodeId) -> NodeKey {
        self.store
            .keys
            .get(&node)
            .cloned()
            .unwrap_or(NodeKey::Status)
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
    /// Whether the evaluator plans this request (S1: not cquery/aquery
    /// captures, `--keep_going`, top-level aspects or a root checkpoint).
    pub(in crate::engine) fn eval_covers(&self, checkpoint: bool) -> bool {
        !checkpoint
            && self.cquery_capture.is_none()
            && self.aquery_capture.is_none()
            && !self.keep_going
            && self.requested_aspects.is_empty()
    }

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
        super::capture::start(tracking);
        let console = crate::ui::DeferredConsole::start();
        crate::ui::set_count_deferred(true);
        let shared = EvalShared::from_engine(self);
        let lin = Linearizer {
            pass: Pass1::new(Options { warnings }),
            ops: Vec::new(),
            tracker,
            replay: TapeReplay::new(0),
            configured: std::mem::take(configured),
            plan: std::mem::take(plan),
            published_memos: 0,
            unpublished: Vec::new(),
            console: Some(console),
            failure: None,
        };
        let (result, lin) = {
            let mut bodies = Phase1Bodies::new(&shared, self, tracking, true);
            bodies.framed = phase1_config(shared.options.loading_phase_threads).threads > 1;
            {
                let mut coord = bodies.coord();
                coord.store.warnings_seen = coord.engine.warnings.len();
                coord.store.delay_release = bodies.framed;
                coord.lin = Some(Box::new(lin));
                coord.begin_segment(SegmentKind::Setup);
            }
            let evaluator =
                Evaluator::new(&bodies, phase1_config(shared.options.loading_phase_threads));
            // S2: with more than one thread, the pool evaluates what the
            // requests need first (attempts only the sequential pass can
            // run park); the walk's order then drives the inline pass over
            // the same graph.
            let pooled_at = std::time::Instant::now();
            let pool_roots = if evaluator.pooled() {
                let roots = warm_up(&evaluator, &shared, labels, top, target_platform.clone());
                evaluator.set_inline(true);
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
            let result = drive(
                &bodies,
                &evaluator,
                &shared,
                labels,
                top,
                target_platform,
                pool_roots.as_deref(),
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
                let rounds = std::mem::take(
                    &mut *ROUNDS
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner),
                );
                let big: Vec<String> = rounds
                    .iter()
                    .enumerate()
                    .filter(|(_, (done, time))| *done >= 1000 || time.as_millis() >= 100)
                    .map(|(index, (done, time))| {
                        format!("#{} {done} in {} ms", index + 2, time.as_millis())
                    })
                    .collect();
                crate::note!(
                    "bazel-rs: eval phase 1: later pool rounds {} ({} nodes in the first 16, {} ms); with >= 1000 nodes: {}",
                    rounds.len(),
                    rounds.iter().map(|(done, _)| done).sum::<u64>(),
                    rounds
                        .iter()
                        .map(|(_, time)| time.as_millis())
                        .sum::<u128>(),
                    big.join(", ")
                );
                let drive =
                    |index: usize| DRIVE[index].load(std::sync::atomic::Ordering::Relaxed) / 1000;
                crate::note!(
                    "bazel-rs: eval phase 1: driver: pool rounds {} ms, requested targets' lookups and evaluations {} ms (the linearizer's flushes included)",
                    drive(0),
                    drive(1)
                );
                let spec = &super::super::nodes::speculate::STATS;
                let spec = |index: usize| spec[index].load(std::sync::atomic::Ordering::Relaxed);
                crate::note!(
                    "bazel-rs: eval phase 1: speculative flag phase 2s {}, left {}, scratch entries planned {}, {} ms",
                    spec(0),
                    spec(1),
                    spec(2),
                    spec(3) / 1000
                );
            }
            let mut coord = bodies.coord();
            coord.drain();
            coord.store.segments_done = true;
            let finished = coord.linearize(&shared);
            let mut lin = coord.lin.take().expect("the linearizer");
            coord.publish(&mut lin);
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
            (result, lin)
        };
        crate::ui::set_count_deferred(false);
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
        crate::perf_mark("plan.eval_phase1_done");
        if stats_enabled() {
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
pub(crate) fn publish_memo(engine: &mut Engine, published: &Published) {
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
pub(crate) fn commit_roots(repositories: &crate::repository::RepositorySet, roots: &[String]) {
    let scope = crate::observed_fs::unit_scope();
    repositories.note_committed_roots(roots);
    crate::observed_fs::commit_to_recorder(scope.finish());
}

/// The first node below `root`, in the walk's order (a depth-first walk
/// of the parked nodes in need order), that waits for the sequential pass
/// (`bodies::INLINE`).
fn first_inline_blocker(
    evaluator: &Evaluator<'_, Phase1Bodies<'_, '_>>,
    root: NodeId,
) -> Option<NodeId> {
    use super::super::graph::{State, Wait};
    use super::super::nodes::bodies::INLINE;
    let graph = evaluator.graph();
    let mut seen = FxHashSet::default();
    let mut stack = vec![root];
    while let Some(id) = stack.pop() {
        if !seen.insert(id) {
            continue;
        }
        let entry = graph.entry(id);
        if entry.state() != State::Waiting {
            continue;
        }
        let wait = entry.inner.lock().wait.clone();
        match wait {
            Wait::Repo(name) | Wait::Unresolvable(name) if name == INLINE => return Some(id),
            Wait::Nodes(nodes) => stack.extend(nodes.into_iter().rev()),
            _ => {}
        }
    }
    None
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

/// BAZEL_RS_EVAL_STATS=1.
fn stats_enabled() -> bool {
    // snapshot-unrecorded: a diagnostics switch; it changes nothing the analysis computes.
    std::env::var_os("BAZEL_RS_EVAL_STATS").is_some()
}

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
    evaluator.evaluate_round(&roots, &mut lane);
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
                NodeValue::Tgt(Ok(Target::Source(_))) => Some(NodeKey::Src(label.clone())),
                NodeValue::Tgt(Ok(Target::Rule(rule) | Target::Output(rule, _))) => {
                    super::super::super::check_macro_namespace(rule, label).ok()?;
                    let key = ConfiguredTargetKey::new(rule.label.clone(), top);
                    Some(NodeKey::Ct1(edge_ct_key(no_config, rule, key, false, None)))
                }
                _ => None,
            }
        })
        .collect();
    evaluator.evaluate_round(&roots, &mut lane);
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
    let before = counted.then(|| evaluator.stats().done);
    evaluator.evaluate_round(roots, &mut Lane { shared });
    if stats_enabled() {
        let elapsed = started.elapsed();
        let done = before.map_or(0, |before| evaluator.stats().done - before);
        ROUNDS
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push((done, elapsed));
    }
    evaluator.set_inline(true);
    pooled.store(false, std::sync::atomic::Ordering::Release);
}

/// BAZEL_RS_EVAL_STATS: the sequential driver's time (µs) in the later
/// pool rounds, and in the requested targets' lookups and evaluations.
static DRIVE: [std::sync::atomic::AtomicU64; 2] = [
    std::sync::atomic::AtomicU64::new(0),
    std::sync::atomic::AtomicU64::new(0),
];

/// BAZEL_RS_EVAL_STATS: the later pool rounds (nodes done, time).
static ROUNDS: std::sync::Mutex<Vec<(u64, std::time::Duration)>> =
    std::sync::Mutex::new(Vec::new());

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
        evaluator.evaluate(
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
            NodeValue::Tgt(result) => result.clone(),
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
        let resolving = std::time::Instant::now();
        // What only the sequential pass can run (a package blocked on an
        // extension's preparation, a flag left to its flush), one at a
        // time in the walk's order, each followed by the pool on what it
        // unblocked (S2): the sequential pass then finds the rest done.
        if pool_roots.is_some()
            && let Ok(root) = &root
        {
            let round = std::time::Instant::now();
            let root_id = evaluator.graph().intern(root);
            while let Some(blocker) = first_inline_blocker(evaluator, root_id) {
                let key = evaluator.graph().key(blocker).clone();
                evaluate(&key);
                if evaluator.graph().value(blocker).is_none() {
                    // It waits for more (the sequential pass below runs it).
                    break;
                }
                pool_round(bodies, evaluator, shared, std::slice::from_ref(root));
            }
            DRIVE[0].fetch_add(
                round.elapsed().as_micros() as u64,
                std::sync::atomic::Ordering::Relaxed,
            );
        }
        let resolved = resolving.elapsed();
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
            started.elapsed().saturating_sub(resolved).as_micros() as u64,
            std::sync::atomic::Ordering::Relaxed,
        );
        // The requested target's segment renders the chain errors.
        let analyzed = analyzed.map_err(|error| render_chains(shared, &error, trimmed.as_ref()));
        // A flush of this segment that failed where the walk would have
        // run it comes first (S2, the CsMatch split: the pool analyzed past
        // it), worded by the walk's chain.
        let analyzed = match analyzed {
            Err(error) => {
                let mut coord = bodies.coord();
                let _ = coord.linearize(shared);
                Err(coord.flush_failure(shared, top).unwrap_or(error))
            }
            ok => ok,
        };
        // The requested target's compatibility, on the published nodes.
        let analyzed = analyzed.and_then(|()| {
            let mut coord = bodies.coord();
            if let Err(error) = coord.linearize(shared) {
                return Err(coord.flush_failure(shared, top).unwrap_or(error));
            }
            let Some(mut lin) = coord.lin.take() else {
                return Ok(());
            };
            coord.publish(&mut lin);
            let checked = coord.with_engine(&mut lin, shared, |engine, lin| {
                engine.check_requested_compatibility(&lin.configured, label, top)
            });
            coord.lin = Some(lin);
            checked
        });
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
        if pool_roots.is_some() && sequential_attempts() != ran {
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
