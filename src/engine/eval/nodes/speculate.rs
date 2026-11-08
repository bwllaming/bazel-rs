//! The CsMatch split (S2, docs/EVALUATOR.md "S2, the CsMatch split"): a
//! config_setting with Starlark `flag_values` keys reads each key's
//! FeatureFlagInfo, which the walk gets from a flush — phase 2 of the
//! plan so far — inside the config_setting's analysis. On the pool the
//! config_setting no longer waits for the sequential pass: its `Flush` is
//! a marker the tape applies at its position (the real phase 2, the walk's
//! errors there), and the flag's `Ct2` node is computed off the sequential
//! pass by a **speculative phase 2**: the flag's subtree as a fresh walk
//! plans it ([`Coord::subtree`](super::bodies::Coord)), prepared, computed
//! and committed on a scratch engine (a fork of the engine before
//! planning, with the subtree's phase-1 values published into it), whose
//! effects are all discarded: console writes, observations, tracker and
//! progress counters, `--toolchain_resolution_debug` events. Only the
//! providers are kept. The tape's flush plans the flag again; the
//! FeatureFlagInfo the matching used is compared with the planned one
//! there (a difference is an internal error naming
//! `BAZEL_RS_ANALYSIS=walk`).
//!
//! A speculation that cannot run (a transcript freed, a repository the
//! pool may not fetch, a failure anywhere in the subtree, the workspace
//! status, `--profile`) leaves the `Ct2` node to the sequential pass,
//! which flushes as the walk does: the walk's error wording is kept.
//!
//! S3 replaces the scratch engine by per-node CT2 deltas merged in plan
//! order (the flag's delta would then be merged at its plan position
//! instead of being planned twice).

use std::cell::Cell;
use std::collections::BTreeMap;
use std::sync::Arc;

use crate::fast_hash::FxHashSet;

use super::super::super::{ConfiguredNodes, ConfiguredTargetKey, Engine, Plan};
use super::super::key::{CtKey, NodeKey};
use super::super::linearize::driver::{publish_ct1, publish_memo};
use super::super::shared::EvalShared;
use super::bodies::Phase1Cx;

/// The providers a target's phase 2 returned.
pub(crate) type Providers = BTreeMap<String, starlark::values::OwnedFrozenValue>;

/// BAZEL_RS_EVAL_STATS: speculative phase 2s run, left to the sequential
/// pass, the scratch plan entries, the µs spent.
pub(crate) static STATS: [std::sync::atomic::AtomicU64; 5] = [
    std::sync::atomic::AtomicU64::new(0),
    std::sync::atomic::AtomicU64::new(0),
    std::sync::atomic::AtomicU64::new(0),
    std::sync::atomic::AtomicU64::new(0),
    std::sync::atomic::AtomicU64::new(0),
];
const STAT_RUNS: usize = 0;
const STAT_LEFT: usize = 1;
const STAT_ENTRIES: usize = 2;
const STAT_MICROS: usize = 3;
const STAT_COORD: usize = 4;

fn bump(index: usize, by: u64) {
    STATS[index].fetch_add(by, std::sync::atomic::Ordering::Relaxed);
}

thread_local! {
    static SPECULATING: Cell<bool> = const { Cell::new(false) };
}

/// This thread runs a speculative phase 2 (its effects are discarded:
/// the workspace status command is not run for it).
pub(crate) fn speculating() -> bool {
    SPECULATING.with(Cell::get)
}

/// The scratch engine of an evaluation's speculative phase 2s: what it
/// planned stays (the flags' subtrees overlap), so a failed run discards
/// all of it.
pub(crate) struct Scratch {
    engine: Box<Engine>,
    configured: ConfiguredNodes,
    plan: Plan,
    /// The keys in `plan`.
    planned: FxHashSet<ConfiguredTargetKey>,
    /// The packages published into it.
    packages: FxHashSet<crate::label::PackageId>,
    /// The memo values published (a prefix of the store's).
    memos: usize,
    /// The configurations it holds (their count when cloned).
    configurations: usize,
}

impl Scratch {
    /// A fork of `engine` before planning (BAZEL_RS_EVAL_VERIFY's), with
    /// the evaluation's configurations, planning sequentially.
    pub(crate) fn new(engine: &Engine, shared: &EvalShared) -> Self {
        let mut engine = Box::new(engine.fork_for_verify());
        engine.loading_phase_threads = 1;
        engine.action_planning = Some(Default::default());
        // The evaluation holds the configurations (EvalShared).
        engine.configurations = shared
            .configurations
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        let configurations = engine.configurations.len();
        Scratch {
            engine,
            configured: ConfiguredNodes::default(),
            plan: Plan::new(),
            planned: FxHashSet::default(),
            packages: FxHashSet::default(),
            memos: 0,
            configurations,
        }
    }
}

/// Why a speculation did not give the providers (counted, then the
/// sequential pass flushes).
struct Left(String);

impl From<String> for Left {
    fn from(reason: String) -> Self {
        Left(reason)
    }
}

fn left(reason: &str) -> Left {
    Left(reason.to_owned())
}

/// The providers of `ct`'s phase 2, planned speculatively (see the module
/// documentation); `None`: left to the sequential pass.
pub(crate) fn flag_providers(
    shared: &EvalShared,
    cx: &mut dyn Phase1Cx<'_>,
    ct: &CtKey,
) -> Option<Option<Providers>> {
    let started = std::time::Instant::now();
    let result = run(shared, cx, ct);
    bump(STAT_MICROS, started.elapsed().as_micros() as u64);
    match result {
        Ok(providers) => {
            bump(STAT_RUNS, 1);
            // snapshot-unrecorded: a diagnostics switch; it changes nothing the analysis computes.
            if std::env::var_os("BAZEL_RS_EVAL_SPECULATE_DEBUG").is_some() {
                eprintln!(
                    "bazel-rs: speculative phase 2 of {}: {} µs, flag {:?}",
                    ct.key,
                    started.elapsed().as_micros(),
                    providers
                        .as_ref()
                        .and_then(|providers| providers.get(crate::feature_flags::PROVIDER_ID))
                        .and_then(|provider| crate::feature_flags::value_of(provider.value()))
                );
            }
            Some(providers)
        }
        Err(Left(reason)) => {
            bump(STAT_LEFT, 1);
            // snapshot-unrecorded: a diagnostics switch; it changes nothing the analysis computes.
            if std::env::var_os("BAZEL_RS_EVAL_SPECULATE_DEBUG").is_some() {
                eprintln!(
                    "bazel-rs: speculative phase 2 of {}: left: {reason}",
                    ct.key
                );
            }
            None
        }
    }
}

fn run(
    shared: &EvalShared,
    cx: &mut dyn Phase1Cx<'_>,
    ct: &CtKey,
) -> Result<Option<Providers>, Left> {
    // --profile spans and the like would show the speculative work.
    if crate::profile::active() {
        return Err(left("--profile"));
    }
    let root = cx.id_of(&NodeKey::Ct1(ct.clone()));
    let mut guard = cx
        .scratch()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    // What the scratch engine gets of the evaluation so far (the lock
    // order: the scratch, then the coordinator).
    {
        let coord = cx.coord();
        let held = std::time::Instant::now();
        let _held = Held(held);
        let subtree = {
            let planned = guard.as_ref().map(|scratch| &scratch.planned);
            coord.subtree(root, &|key| {
                planned.is_some_and(|planned| planned.contains(key))
            })?
        };
        let scratch = guard.get_or_insert_with(|| Scratch::new(coord.engine, shared));
        let engine = &mut *scratch.engine;
        for (package, loaded) in &coord.store.packages {
            if let Ok(loaded) = loaded
                && !engine.packages.contains_key(package)
                && scratch.packages.insert(package.clone())
            {
                commit_package(engine, package, &loaded.package);
            }
        }
        for published in &coord.store.published[scratch.memos..] {
            publish_memo(engine, published);
        }
        scratch.memos = coord.store.published.len();
        for key in &subtree.canonical {
            let Some(value) = coord.ct1_values.get(key) else {
                return Err(left("a configured target's value is not done"));
            };
            if let Ok(ok) = &value.outcome {
                publish_ct1(engine, &mut scratch.configured, key.key.clone(), ok);
            }
        }
        for (implementation, platform) in subtree.toolchain_implementations {
            engine
                .toolchain_implementations
                .entry(implementation)
                .or_insert(platform);
        }
        // What a flush planned already: the planning state's phase 2.
        let Some(real) = coord.engine.action_planning.as_ref() else {
            return Err(left("no planning state"));
        };
        let state = engine
            .action_planning
            .as_mut()
            .ok_or_else(|| left("no scratch planning state"))?;
        for key in subtree.imported {
            import(state, real, &key);
            scratch.planned.insert(key);
        }
        for key in subtree.plan {
            if !scratch.planned.insert(key.key.clone()) {
                continue;
            }
            let Some(Ok(ok)) = coord.ct1_values.get(&key).map(|value| &value.outcome) else {
                return Err(left("a plan entry without its configured rule"));
            };
            scratch.plan.push((key.key, Arc::clone(&ok.rule)));
        }
    }
    let scratch = guard.as_mut().ok_or_else(|| left("no scratch engine"))?;
    // The configurations and aspect definitions the pool interned and
    // loaded since (content-addressed: a superset reads the same).
    {
        let configurations = shared
            .configurations
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if configurations.len() != scratch.configurations {
            scratch.engine.configurations.absorb(&configurations);
            scratch.configurations = configurations.len();
        }
    }
    scratch.engine.aspects = shared
        .aspects
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone();
    let sentinels = crate::repository_demand::sentinels_raised();
    let planned = isolated(|| {
        let engine = &mut *scratch.engine;
        let mut state = engine.action_planning.take().unwrap_or_default();
        let before = state.done;
        let result = engine.plan_targets(
            &mut state,
            &mut scratch.plan,
            &mut scratch.configured,
            false,
        );
        bump(STAT_ENTRIES, state.done.saturating_sub(before) as u64);
        // snapshot-unrecorded: a diagnostics switch; it changes nothing the analysis computes.
        if std::env::var_os("BAZEL_RS_EVAL_SPECULATE_DEBUG").is_some() {
            let mut packages: BTreeMap<String, usize> = BTreeMap::new();
            for (key, _) in &scratch.plan[before..] {
                *packages
                    .entry(format!("{}//{}", key.label.repository, key.label.package))
                    .or_default() += 1;
            }
            let mut packages: Vec<_> = packages.into_iter().collect();
            packages.sort_by_key(|(_, count)| std::cmp::Reverse(*count));
            packages.truncate(8);
            eprintln!(
                "bazel-rs: speculative phase 2 of {}: {} entries {packages:?}",
                ct.key,
                state.done.saturating_sub(before)
            );
        }
        engine.action_planning = Some(state);
        result
    });
    // A failure (the tape's flush reports the walk's), or a repository
    // the pool may not fetch (a sentinel, even one swallowed): what the
    // scratch engine planned may be partial.
    let sentinel = crate::repository_demand::sentinels_raised() != sentinels;
    if planned.is_err() || sentinel {
        *guard = None;
        return Err(Left(match planned {
            Err(error) => format!("phase 2 failed: {error}"),
            Ok(()) => "a repository sentinel".to_owned(),
        }));
    }
    for (key, _) in &scratch.plan {
        scratch.planned.insert(key.clone());
    }
    let state = scratch
        .engine
        .action_planning
        .as_ref()
        .ok_or_else(|| left("no planning state"))?;
    if !state.dry.contains_key(&ct.key) {
        return Err(left("the flag was not planned"));
    }
    Ok(state.custom_providers.get(&ct.key).cloned())
}

/// Run `f` with every effect a phase 2 has outside its engine set aside
/// and dropped: console writes, observations, the evaluator's captures,
/// `--toolchain_resolution_debug` events, the analysis progress counters.
fn isolated<R>(f: impl FnOnce() -> R) -> R {
    let counts = crate::progress::analysis_counts();
    let console = crate::ui::DeferredConsole::start();
    let scope = crate::observed_fs::unit_scope();
    let was = SPECULATING.with(|cell| cell.replace(true));
    let result = super::super::linearize::capture::isolated(f);
    SPECULATING.with(|cell| cell.set(was));
    drop(scope);
    drop(console.finish());
    crate::progress::restore_analysis_counts(counts);
    result
}

/// Counts the µs the coordinator's lock is held for a speculation.
struct Held(std::time::Instant);

impl Drop for Held {
    fn drop(&mut self) {
        bump(STAT_COORD, self.0.elapsed().as_micros() as u64);
    }
}

/// A dependency's phase 2 as its consumers read it (the planning state's
/// per-key entries a dependency view reads), copied from the build's
/// planning state into the scratch engine's.
fn import(
    scratch: &mut super::super::super::ActionPlanning,
    real: &super::super::super::ActionPlanning,
    key: &ConfiguredTargetKey,
) {
    fn copy<V: Clone, M>(into: &mut M, from: &M, key: &ConfiguredTargetKey)
    where
        M: MapLike<V>,
    {
        if let Some(value) = from.get_value(key) {
            into.put(key.clone(), value.clone());
        }
    }
    copy(&mut scratch.dry, &real.dry, key);
    copy(&mut scratch.custom_providers, &real.custom_providers, key);
    copy(&mut scratch.provider_keys, &real.provider_keys, key);
    copy(&mut scratch.runfiles_info, &real.runfiles_info, key);
    copy(&mut scratch.repositories, &real.repositories, key);
    copy(&mut scratch.validation, &real.validation, key);
    copy(
        &mut scratch.native_output_groups,
        &real.native_output_groups,
        key,
    );
    copy(&mut scratch.runfiles_nodes, &real.runfiles_nodes, key);
}

/// The planning state's per-key maps (hash and B-tree maps).
trait MapLike<V> {
    fn get_value(&self, key: &ConfiguredTargetKey) -> Option<&V>;
    fn put(&mut self, key: ConfiguredTargetKey, value: V);
}

impl<V> MapLike<V> for crate::fast_hash::FxHashMap<ConfiguredTargetKey, V> {
    fn get_value(&self, key: &ConfiguredTargetKey) -> Option<&V> {
        self.get(key)
    }
    fn put(&mut self, key: ConfiguredTargetKey, value: V) {
        self.insert(key, value);
    }
}

impl<V> MapLike<V> for BTreeMap<ConfiguredTargetKey, V> {
    fn get_value(&self, key: &ConfiguredTargetKey) -> Option<&V> {
        self.get(key)
    }
    fn put(&mut self, key: ConfiguredTargetKey, value: V) {
        self.insert(key, value);
    }
}

/// `Engine::commit_loaded_package` of a package the evaluation loaded,
/// into the scratch engine.
fn commit_package(
    engine: &mut Engine,
    package: &crate::label::PackageId,
    rules: &Arc<crate::package::Package>,
) {
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
