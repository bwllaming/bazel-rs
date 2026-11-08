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
pub(crate) static STATS: [std::sync::atomic::AtomicU64; 4] = [
    std::sync::atomic::AtomicU64::new(0),
    std::sync::atomic::AtomicU64::new(0),
    std::sync::atomic::AtomicU64::new(0),
    std::sync::atomic::AtomicU64::new(0),
];
const STAT_RUNS: usize = 0;
const STAT_LEFT: usize = 1;
const STAT_ENTRIES: usize = 2;
const STAT_MICROS: usize = 3;

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

/// Why a speculation did not give the providers (counted, then the
/// sequential pass flushes).
struct Left;

impl From<String> for Left {
    fn from(_: String) -> Self {
        Left
    }
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
            Some(providers)
        }
        Err(Left) => {
            bump(STAT_LEFT, 1);
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
        return Err(Left);
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
        let subtree = coord.subtree(root)?;
        let scratch = guard.get_or_insert_with(|| {
            let mut engine = Box::new(coord.engine.fork_for_verify());
            engine.loading_phase_threads = 1;
            engine.action_planning = Some(Default::default());
            // The evaluation holds the configurations (EvalShared).
            engine.configurations = shared
                .configurations
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .clone();
            Scratch {
                engine,
                configured: ConfiguredNodes::default(),
                plan: Plan::new(),
                planned: FxHashSet::default(),
                packages: FxHashSet::default(),
                memos: 0,
                configurations: usize::MAX,
            }
        });
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
                return Err(Left);
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
        for key in subtree.plan {
            if !scratch.planned.insert(key.key.clone()) {
                continue;
            }
            let Some(Ok(ok)) = coord.ct1_values.get(&key).map(|value| &value.outcome) else {
                return Err(Left);
            };
            scratch.plan.push((key.key, Arc::clone(&ok.rule)));
        }
    }
    let scratch = guard.as_mut().ok_or(Left)?;
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
        engine.action_planning = Some(state);
        result
    });
    // A failure (the tape's flush reports the walk's), or a repository
    // the pool may not fetch (a sentinel, even one swallowed): what the
    // scratch engine planned may be partial.
    if planned.is_err() || crate::repository_demand::sentinels_raised() != sentinels {
        *guard = None;
        return Err(Left);
    }
    for (key, _) in &scratch.plan {
        scratch.planned.insert(key.clone());
    }
    let state = scratch.engine.action_planning.as_ref().ok_or(Left)?;
    if !state.dry.contains_key(&ct.key) {
        return Err(Left);
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
