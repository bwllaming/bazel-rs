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
use super::super::key::{CtKey, NodeId, NodeKey};
use super::super::nodes::bodies::{Coord, Phase1Bodies, Running};
use super::super::nodes::eval_host::edge_ct_key;
use super::super::nodes::loading::PkgValue;
use super::super::scheduler::{EvalConfig, Evaluator, NoRepositories};
use super::super::shared::EvalShared;
use super::super::transcript::{Event, SegmentKind, TapeOp, Transcript};
use super::super::value::{Ct1Value, NodeValue};
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
    /// Source-file lookups' missing inputs and source directories (Tgt
    /// nodes; order-free maps, published with the values).
    pub(crate) sources_side: Vec<crate::engine::SourceSide>,
}

impl Store {
    /// Keep a console write; its [`Event::Console`] index.
    pub(crate) fn console(&mut self, write: crate::ui::Deferred) -> u32 {
        self.consoles.push(Some(write));
        (self.consoles.len() - 1) as u32
    }

    /// Free what pass 1 will not read again.
    fn release(&mut self, pass: &mut Pass1) {
        let (nodes, segments) = pass.take_released();
        for node in nodes {
            self.transcripts.remove(&node);
            self.released.insert(node);
        }
        for index in segments {
            if let Some((_, events)) = self.segments.get_mut(index) {
                *events = Vec::new();
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

    /// Record `event` in the running attempt (or the current segment). A
    /// rerun re-emits its abandoned attempt's events, which stay in place
    /// (pass 1 may have replayed them); the one-time effects of the engine's
    /// services there (a package's load output and commit, its warnings)
    /// are not re-emitted.
    pub(crate) fn push_event(&mut self, event: Event) {
        match self.stack.last_mut() {
            Some(running) if running.cursor < running.replay => {
                // The engine services' effects an attempt's first need of a
                // `Tgt` node carries (S1 loading: a package's load output,
                // commit, warnings, its lookups and scopes) happen once.
                let one_time = |event: &Event| {
                    matches!(
                        event,
                        Event::Console(_)
                            | Event::PkgCommit(..)
                            | Event::Warn(_)
                            | Event::Access(_)
                            | Event::ScopeEnter(_)
                            | Event::ScopeExit
                    )
                };
                while running.cursor < running.replay
                    && running.events[running.cursor] != event
                    && one_time(&running.events[running.cursor])
                {
                    running.cursor += 1;
                }
                if running.cursor < running.replay && running.events[running.cursor] == event {
                    running.cursor += 1;
                    return;
                }
                if running.cursor < running.replay {
                    self.store.rerun_mismatch.get_or_insert_with(|| {
                        format!(
                            "{:?} re-emits {event:?} where its abandoned attempt emitted {:?}",
                            running.key, running.events[running.cursor]
                        )
                    });
                    running.events.truncate(running.cursor);
                    running.replay = running.cursor;
                }
                running.events.push(event);
            }
            Some(running) => running.events.push(event),
            None => {
                if let Some((_, events)) = self.store.segments.last_mut() {
                    events.push(event);
                }
            }
        }
    }

    pub(crate) fn begin_attempt(&mut self, id: NodeId, key: NodeKey) {
        self.store.keys.entry(id).or_insert_with(|| key.clone());
        let events = self.store.carry.remove(&id).unwrap_or_default();
        self.stack.push(Running {
            id,
            key,
            replay: events.len(),
            cursor: 0,
            events,
            missing_at: None,
        });
    }

    /// The running attempt received a `Missing`: it stops here.
    pub(crate) fn missing_here(&mut self) {
        self.drain();
        if let Some(running) = self.stack.last_mut() {
            running.missing_at.get_or_insert(running.events.len());
        }
    }

    /// The attempt ends with `value`: its transcript is kept (a `Tgt`
    /// lookup's goes to its user); returns its key.
    pub(crate) fn finish_attempt(&mut self, value: &NodeValue) -> Option<NodeKey> {
        let running = self.stack.pop()?;
        if let Some(event) = running.events[running.cursor.min(running.replay)..running.replay]
            .iter()
            .find(|event| {
                !matches!(
                    event,
                    Event::Console(_)
                        | Event::PkgCommit(..)
                        | Event::Warn(_)
                        | Event::Access(_)
                        | Event::ScopeEnter(_)
                        | Event::ScopeExit
                )
            })
        {
            self.store.rerun_mismatch.get_or_insert_with(|| {
                format!(
                    "{:?} finished without re-emitting {event:?} of its abandoned attempt",
                    running.key
                )
            });
        }
        if matches!(running.key, NodeKey::Tgt(_)) {
            for event in running.events {
                self.push_event(event);
            }
            return Some(running.key);
        }
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
        self.store.transcripts.insert(
            running.id,
            Transcript {
                events: running.events,
            },
        );
        Some(running.key)
    }

    /// The attempt stopped (`Missing`): it reruns from the start, its
    /// events up to the `Missing` stay (pass 1 may have replayed them; the
    /// rerun re-emits them, `push_event`), with what happened anyway.
    pub(crate) fn abandon_attempt(&mut self) {
        if let Some(mut running) = self.stack.pop() {
            let keep = running
                .missing_at
                .unwrap_or(running.events.len())
                .max(running.replay);
            running.events.truncate(keep);
            if !running.events.is_empty() {
                self.store.carry.insert(running.id, running.events);
            }
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
        if let Some(mismatch) = &self.store.rerun_mismatch {
            return Err(format!(
                "internal error: BAZEL_RS_ANALYSIS=eval: {mismatch}; rerun with BAZEL_RS_ANALYSIS=walk"
            ));
        }
        {
            let view = View {
                store: &self.store,
                stack: &self.stack,
                ct1_values: &self.ct1_values,
            };
            stat(STAT_PASS1, || lin.pass.resume(&view, &mut lin.ops))?;
        }
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
                    let flushed = self.flush(lin, shared);
                    STATS[STAT_APPLY].fetch_sub(
                        started.elapsed().as_micros() as u64,
                        std::sync::atomic::Ordering::Relaxed,
                    );
                    flushed?
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
        result
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
            let key = ct.key.clone();
            if lin.configured.contains_key(&key) {
                continue;
            }
            lin.configured.insert(key.clone(), ok.node.clone());
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
        lin.published_memos = self.store.published.len();
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
        };
        let (result, lin) = {
            let bodies = Phase1Bodies::new(&shared, self, tracking);
            {
                let mut coord = bodies.coord();
                coord.store.warnings_seen = coord.engine.warnings.len();
                coord.lin = Some(Box::new(lin));
                coord.begin_segment(SegmentKind::Setup);
            }
            let evaluator = Evaluator::new(
                &bodies,
                EvalConfig {
                    threads: 1,
                    keep_going: false,
                    jitter_micros: 0,
                },
            );
            let result = drive(&bodies, &evaluator, &shared, labels, top, target_platform);
            let mut coord = bodies.coord();
            coord.drain();
            coord.store.segments_done = true;
            let finished = coord.linearize(&shared);
            let mut lin = coord.lin.take().expect("the linearizer");
            coord.publish(&mut lin);
            let result = result.and(finished).and_then(|()| {
                let view = View {
                    store: &coord.store,
                    stack: &coord.stack,
                    ct1_values: &coord.ct1_values,
                };
                match lin.pass.resume(&view, &mut lin.ops)? {
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
        // snapshot-unrecorded: a diagnostics switch; it changes nothing the analysis computes.
        if std::env::var_os("BAZEL_RS_EVAL_STATS").is_some() {
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

/// The walk's `plan_build` phase 1 over the evaluator: the explicit
/// target platform, then each requested label (`analyze_requested`).
fn drive(
    bodies: &Phase1Bodies<'_, '_>,
    evaluator: &Evaluator<'_, Phase1Bodies<'_, '_>>,
    shared: &EvalShared,
    labels: &[Label],
    top: ConfigId,
    target_platform: Option<Label>,
) -> Result<(), String> {
    let evaluate = |key: &NodeKey| -> Arc<NodeValue> {
        evaluator.evaluate(
            std::slice::from_ref(key),
            &mut NoRepositories,
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
        // The requested target's segment renders the chain errors.
        let analyzed = analyzed.map_err(|error| render_chains(shared, &error, trimmed.as_ref()));
        // The requested target's compatibility, on the published nodes.
        let analyzed = analyzed.and_then(|()| {
            let mut coord = bodies.coord();
            coord.linearize(shared)?;
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
