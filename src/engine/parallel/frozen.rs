//! S3, eval pass 2 (docs/EVALUATOR.md "S3"): Starlark rules prepared on
//! the analysis workers while the coordinator commits.
//!
//! The pipeline (P3/P4/S2, docs/PARALLEL_ANALYSIS.md) prepares every
//! entry on the coordinator: a prepare reads the engine and the planning
//! state, which the coordinator's commits write. In an evaluated build
//! (`BAZEL_RS_ANALYSIS=eval`, flushes deferred) phase 1 is complete when
//! phase 2 runs, so a walk can hand its workers a **frozen** view:
//!
//! - the engine as phase 1 left it, forked when the walk starts
//!   (`Engine::fork_for_verify`: every phase-1 table, the packages, the
//!   platforms; what a commit changes later — a package phase 2 loads, a
//!   platform it resolves — is missing from it, so such a prepare aborts or
//!   is redone at its turn, as a prepare ahead of its turn is), and the
//!   configured nodes;
//! - the results of the targets committed so far ([`PlannedResults`]: the
//!   planning state's per-target entries, copied when an entry commits —
//!   ahead of its turn or at it — before its dependents are woken; an
//!   entry is ready only when the keys its prepare reads are committed, so
//!   it reads final entries, as a prepare on the coordinator does);
//! - the dependency views, shared `Target` identities and native output
//!   groups: the planning state's (moved in for the walk) and those filled
//!   during it ([`prepare::BatchMemos`], single-flight, also filled by the
//!   coordinator's own views: one value per key whoever builds it).
//!
//! A worker runs the prepare ([`prepare::SharedPrepare::frozen`]), the
//! implementation and the commit's delta in one job; the coordinator
//! applies what the prepare recorded (console and aquery owners) when the
//! job reports, and commits ahead of the turn or at it as before. A prepare
//! that needs the coordinator aborts: the entry is prepared at its turn.

use super::*;

/// `BAZEL_RS_EVAL_FROZEN_PREPARES=1` (`all`: every walk, the tests').
pub(super) fn enabled() -> bool {
    walk_min().is_some()
}

/// The shortest walk that prepares on the workers (the frozen view costs
/// a fork of the engine), `None`: off.
pub(super) fn walk_min() -> Option<usize> {
    // snapshot-unrecorded: a scheduling switch; the output is the same either way.
    let value = std::env::var("BAZEL_RS_EVAL_FROZEN_PREPARES")
        .ok()
        // A binary built with it (the test suites' mode, like
        // BAZEL_RS_DEFAULT_ANALYSIS).
        .or(option_env!("BAZEL_RS_DEFAULT_EVAL_FROZEN_PREPARES").map(str::to_owned))?;
    match value.as_str() {
        "1" => Some(2048),
        "all" => Some(0),
        _ => None,
    }
}

/// What the analysis workers' prepares read during one walk.
pub(in crate::engine) struct Frozen {
    pub(in crate::engine) engine: Engine,
    pub(in crate::engine) configured: ConfiguredNodes,
    pub(in crate::engine) views: DependencyViews,
    pub(in crate::engine) identities:
        BTreeMap<TargetIdentityKey, Arc<crate::starlark_types::Target>>,
    pub(in crate::engine) output_groups:
        BTreeMap<ConfiguredTargetKey, starlark::values::OwnedFrozenValue>,
    pub(in crate::engine) results: PlannedResults,
    pub(in crate::engine) memos: prepare::BatchMemos,
    pub(in crate::engine) artifacts: Arc<ConcurrentArtifactIndex>,
    /// What a delta built on a worker reads (`None`: deltas are built on
    /// the coordinator).
    view: Option<delta::SharedView>,
    compact_repo_mapping: bool,
}

/// The shards of [`PlannedResults`].
const RESULT_SHARDS: usize = 64;

/// One committed target's per-target planning-state entries.
#[derive(Default)]
struct KeyResults {
    dry: Option<Artifacts>,
    providers: Option<BTreeMap<String, starlark::values::OwnedFrozenValue>>,
    provider_keys: Option<Vec<String>>,
    runfiles_info: Option<Arc<crate::runfiles::TargetRunfiles>>,
    validation: Option<crate::starlark_types::Depset>,
    repositories: Option<Arc<BTreeSet<String>>>,
}

/// The planning state's per-target entries of the committed targets, read
/// by the workers' prepares while the coordinator commits.
pub(in crate::engine) struct PlannedResults {
    keys: Box<[std::sync::RwLock<KeyMap<Arc<KeyResults>>>]>,
    declared: Box<[std::sync::RwLock<Declared>]>,
}

impl PlannedResults {
    fn new() -> Self {
        Self {
            keys: (0..RESULT_SHARDS).map(|_| Default::default()).collect(),
            declared: (0..RESULT_SHARDS).map(|_| Default::default()).collect(),
        }
    }

    fn key_shard(&self, key: &ConfiguredTargetKey) -> &std::sync::RwLock<KeyMap<Arc<KeyResults>>> {
        use std::hash::BuildHasher;
        let hash = crate::fast_hash::FxBuildHasher::default().hash_one(key);
        &self.keys[(hash >> 58) as usize & (RESULT_SHARDS - 1)]
    }

    fn declared_shard(&self, hash: u64) -> &std::sync::RwLock<Declared> {
        &self.declared[(hash >> 58) as usize & (RESULT_SHARDS - 1)]
    }

    fn get(&self, key: &ConfiguredTargetKey) -> Option<Arc<KeyResults>> {
        self.key_shard(key)
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .get(key)
            .cloned()
    }

    /// Copy `key`'s entries from the planning state (it committed).
    pub(in crate::engine) fn publish(&self, key: &ConfiguredTargetKey, state: &ActionPlanning) {
        let results = KeyResults {
            dry: state.dry.get(key).cloned(),
            providers: state.custom_providers.get(key).cloned(),
            provider_keys: state.provider_keys.get(key).cloned(),
            runfiles_info: state.runfiles_info.get(key).cloned(),
            validation: state.validation.get(key).cloned(),
            repositories: state.repositories.get(key).cloned(),
        };
        self.key_shard(key)
            .write()
            .unwrap_or_else(|e| e.into_inner())
            .insert(key.clone(), Arc::new(results));
    }

    /// T2 (docs/EVALUATOR.md §8): the results of `key`'s phase 2 built on
    /// a worker ahead of its merge — what its dependents' prepares read
    /// (`transitive`: its transitive repositories, the commit's first
    /// write) —, its declarations and its registrations (provisional in
    /// `artifacts`, visible from its plan index `stamp`). The merge
    /// publishes the planning state's entries over them ([`Self::publish`]).
    pub(in crate::engine) fn publish_delta(
        &self,
        key: &ConfiguredTargetKey,
        delta: &delta::TargetDelta,
        transitive: &BTreeSet<String>,
        artifacts: &ConcurrentArtifactIndex,
        stamp: u32,
    ) {
        let mut results = KeyResults {
            repositories: Some(Arc::new(transitive.clone())),
            ..KeyResults::default()
        };
        for op in &delta.ops {
            match op {
                delta::DeltaOp::Providers(providers, keys) => {
                    results.providers = Some(providers.clone());
                    results.provider_keys = Some(keys.clone());
                }
                delta::DeltaOp::Dry(dry) => results.dry = Some(Arc::clone(dry)),
                delta::DeltaOp::RunfilesInfo(runfiles) => {
                    results.runfiles_info = Some(Arc::clone(runfiles));
                }
                delta::DeltaOp::Declared {
                    key,
                    artifact,
                    replace,
                } => {
                    self.declared_shard(key.hash)
                        .write()
                        .unwrap_or_else(|e| e.into_inner())
                        .put(key.clone(), artifact.clone(), *replace);
                }
                delta::DeltaOp::Artifact(file, artifact, hash) => {
                    artifacts.publish_provisional(*hash, file, artifact, stamp);
                }
                _ => {}
            }
        }
        self.key_shard(key)
            .write()
            .unwrap_or_else(|e| e.into_inner())
            .insert(key.clone(), Arc::new(results));
    }

    /// Copy the declarations made since the last call.
    pub(in crate::engine) fn publish_declared(&self, state: &ActionPlanning) {
        let Some(journal) = state.declared.journal.as_ref() else {
            return;
        };
        let keys = std::mem::take(&mut *journal.lock().unwrap_or_else(|e| e.into_inner()));
        for key in keys {
            if let Some(artifact) = state.declared.get(key.config, &key.path) {
                self.declared_shard(key.hash)
                    .write()
                    .unwrap_or_else(|e| e.into_inner())
                    .put(key, artifact.clone(), true);
            }
        }
    }

    /// Every result of `state` (the targets committed before the walk).
    fn seed(state: &ActionPlanning) -> Self {
        let results = Self::new();
        for key in state.dry.keys() {
            results.publish(key, state);
        }
        for (key, artifact) in state.declared.entries.iter() {
            results
                .declared_shard(key.hash)
                .write()
                .unwrap_or_else(|e| e.into_inner())
                .put(key.clone(), artifact.clone(), true);
        }
        results
    }
}

impl PlanReads for PlannedResults {
    fn dry(&self, key: &ConfiguredTargetKey) -> Option<Artifacts> {
        self.get(key)?.dry.clone()
    }

    fn providers(
        &self,
        key: &ConfiguredTargetKey,
    ) -> Option<BTreeMap<String, starlark::values::OwnedFrozenValue>> {
        self.get(key)?.providers.clone()
    }

    fn provider(
        &self,
        key: &ConfiguredTargetKey,
        id: &str,
    ) -> Option<starlark::values::OwnedFrozenValue> {
        self.get(key)?.providers.as_ref()?.get(id).cloned()
    }

    fn provider_keys(&self, key: &ConfiguredTargetKey) -> Option<Vec<String>> {
        self.get(key)?.provider_keys.clone()
    }

    fn runfiles_info(
        &self,
        key: &ConfiguredTargetKey,
    ) -> Option<Arc<crate::runfiles::TargetRunfiles>> {
        self.get(key)?.runfiles_info.clone()
    }

    fn declared(&self, config: ConfigId, path: &Path) -> Option<Artifact> {
        let hash = declared_hash(config, path_hash(path));
        self.declared_shard(hash)
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .get(config, path)
            .cloned()
    }

    fn validation(&self, key: &ConfiguredTargetKey) -> Option<crate::starlark_types::Depset> {
        self.get(key)?.validation.clone()
    }

    fn repositories(&self, key: &ConfiguredTargetKey) -> Option<Arc<BTreeSet<String>>> {
        self.get(key)?.repositories.clone()
    }
}

/// A worker's prepare and compute of one entry ([`Engine::dispatch_frozen`]).
pub(super) struct FrozenOutcome {
    /// `Ok(None)`: the prepare aborted (prepared at its turn).
    prepared: Result<Option<Box<PreparedParts>>, String>,
    abort: Option<&'static str>,
    owner: Option<(String, crate::shared_path::SharedPath, ConfiguredTargetKey)>,
    effects: Effects,
    platform_known: bool,
    elapsed: std::time::Duration,
    /// Its compute (absent when the prepare did not give one).
    worker: Option<Result<WorkerOutcome, String>>,
}

/// An entry being prepared and computed on a worker.
pub(super) type Preparing = mpsc::Receiver<Result<FrozenOutcome, String>>;

/// BAZEL_RS_EVAL_STATS / BAZEL_RS_TIMING: prepares on workers, aborted,
/// failed, the µs the coordinator waited for one at its turn.
pub(super) static FROZEN_STATS: [AtomicU64; 4] = [
    AtomicU64::new(0),
    AtomicU64::new(0),
    AtomicU64::new(0),
    AtomicU64::new(0),
];

impl Engine {
    /// Whether this walk prepares on the workers (S3, eval pass 2;
    /// `BAZEL_RS_EVAL_FROZEN_PREPARES=1`, off by default: on ~/code the
    /// walk is bound by its turn chain, not by the coordinator's prepares,
    /// and the frozen view costs ~0.5 GB): an evaluated build whose flushes
    /// are deferred, a walk that commits ahead, no tracker (its accesses
    /// are the coordinator's scopes').
    pub(super) fn frozen_prepares(&self, state: &ActionPlanning) -> bool {
        self.eval_async_phase2
            && self.commits_ahead(state)
            && !self.tracking()
            && shared_prepares()
            && enabled()
    }

    /// The frozen view of a walk starting now; the planning state's memos
    /// move into it ([`Self::thaw`] moves them back).
    pub(super) fn freeze(
        &mut self,
        state: &mut ActionPlanning,
        configured: &ConfiguredNodes,
    ) -> Arc<Frozen> {
        self.ensure_analysis_pool();
        let view = (!self.semantic_only() && self.aquery_capture.is_none() && worker_deltas())
            .then(|| delta::SharedView::new(self, state));
        state.declared.journal = Some(Default::default());
        Arc::new(Frozen {
            engine: self.fork_for_verify(),
            configured: configured.clone(),
            views: std::mem::take(&mut state.views),
            identities: std::mem::take(&mut state.target_identities),
            output_groups: std::mem::take(&mut state.native_output_groups),
            results: PlannedResults::seed(state),
            memos: prepare::BatchMemos::default(),
            artifacts: Arc::clone(&state.artifact_index.shared),
            view,
            compact_repo_mapping: self.compact_repo_mapping,
        })
    }

    /// The walk ended: the memos back into the planning state, with what
    /// the walk filled.
    pub(super) fn thaw(&mut self, state: &mut ActionPlanning, frozen: Arc<Frozen>) {
        state.declared.journal = None;
        let Frozen {
            views,
            identities,
            output_groups,
            memos,
            ..
        } = match Arc::try_unwrap(frozen) {
            Ok(frozen) => frozen,
            Err(shared) => {
                // A job still holds it (a failing walk stops its pool): the
                // memos are copied.
                state.views = shared.views.clone();
                state.target_identities = shared.identities.clone();
                state.native_output_groups = shared.output_groups.clone();
                return;
            }
        };
        state.views = views;
        state.target_identities = identities;
        state.native_output_groups = output_groups;
        self.apply_batch_memos(state, memos);
    }

    /// Prepare, compute and build the delta of entry `index` on a worker
    /// over `frozen`.
    pub(super) fn dispatch_frozen(
        &self,
        frozen: &Arc<Frozen>,
        index: usize,
        key: ConfiguredTargetKey,
        rule: Arc<Rule>,
    ) -> Preparing {
        let (sender, receiver) = mpsc::sync_channel(1);
        let frozen = Arc::clone(frozen);
        let cancelled = self
            .analysis_pool
            .as_ref()
            .map(|pool| Arc::clone(&pool.cancelled));
        let finished = self
            .analysis_pool
            .as_ref()
            .map(|pool| pool.finished.0.clone());
        let verify_summary = verify_ahead();
        let jitter = jitter_millis();
        let job: Job = Box::new(move || {
            if cancelled
                .as_ref()
                .is_some_and(|flag| flag.load(std::sync::atomic::Ordering::SeqCst))
            {
                return;
            }
            let mut summary = None;
            let mut platform_read = true;
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                if jitter > 0 {
                    let roll = loading_jitter_roll(index as u64, 0x5034) % (jitter + 1);
                    let until = std::time::Instant::now() + std::time::Duration::from_millis(roll);
                    while std::time::Instant::now() < until
                        && !cancelled
                            .as_ref()
                            .is_some_and(|flag| flag.load(std::sync::atomic::Ordering::SeqCst))
                    {
                        std::thread::sleep(std::time::Duration::from_millis(5));
                    }
                }
                let class = crate::plan_timings::class_id(rule.rule_class_name());
                let platform_known = frozen.engine.platform_resolved(&key);
                let isolation = Isolation::enter();
                let started = std::time::Instant::now();
                let timer = crate::plan_timings::StageTimer::start(
                    crate::plan_timings::PlanStage::Prepare,
                    index as u64,
                    class,
                );
                let mut cx = prepare::SharedPrepare::frozen(&frozen, index);
                let prepared = Engine::prepare_custom(&mut cx, &key, &rule);
                timer.finish();
                let elapsed = started.elapsed();
                let (effects, sentinel) = isolation.finish();
                let prepare::SharedRecord { owner, abort, .. } = cx.record;
                let aborted = sentinel
                    || abort.is_some()
                    || prepared.as_ref().is_err_and(|error| {
                        crate::repository_demand::is_inline_rerun_sentinel(error)
                    });
                let mut outcome = FrozenOutcome {
                    prepared: Ok(None),
                    abort: abort.or(aborted.then_some("sentinel")),
                    owner,
                    effects,
                    platform_known,
                    elapsed,
                    worker: None,
                };
                if aborted {
                    return outcome;
                }
                let prepared = match prepared {
                    Err(error) => {
                        outcome.prepared = Err(error);
                        return outcome;
                    }
                    Ok(prepared) => prepared,
                };
                let Some((parts, snapshot)) = PreparedParts::split(prepared) else {
                    outcome.abort = Some("not_starlark");
                    return outcome;
                };
                let delta = frozen.view.as_ref().map(|view| {
                    Box::new(delta::DeltaInputs {
                        view: view.clone(),
                        key: parts.key.clone(),
                        output_root: parts.output_root.clone(),
                        action_owner: parts.action_owner.clone(),
                        main_ignored: Arc::clone(&parts.main_ignored),
                        transitive: parts.transitive.clone(),
                        action_execution: parts.action_execution.clone(),
                        platforms: frozen.engine.remote_platforms(
                            &rule,
                            parts.config,
                            &parts.action_execution,
                        ),
                        class,
                    })
                });
                let (worker, registers) = run_compute(
                    snapshot,
                    &parts.transitive,
                    index,
                    class,
                    delta,
                    frozen.compact_repo_mapping,
                    verify_summary,
                );
                summary = registers;
                platform_read = worker.sentinel || worker.outcome.platform_read;
                outcome.prepared = Ok(Some(Box::new(parts)));
                outcome.worker = Some(Ok(worker));
                outcome
            }));
            let result = result.map_err(|_| {
                WORKER_PANIC
                    .with(|slot| slot.borrow_mut().take())
                    .unwrap_or_else(|| "an analysis worker panicked".to_owned())
            });
            if result.is_err() {
                summary = None;
            }
            let _ = sender.send(result);
            if let Some(finished) = finished {
                let _ = finished.send((index, summary, platform_read, std::time::Instant::now()));
            }
        });
        if let Some(pool) = &self.analysis_pool
            && let Err(job) = pool.submit(index, job)
        {
            job();
        }
        receiver
    }

    /// What the worker's job of entry `index` gave, as the pipeline's
    /// prepared entry (`None`: prepared at its turn).
    pub(super) fn frozen_prepared(
        &mut self,
        pipe: &mut Pipeline,
        index: usize,
        preparing: Preparing,
    ) -> Option<Ahead> {
        let received = preparing.recv();
        self.frozen_received(pipe, index, received)
    }

    /// Entry `index` at its turn, prepared and computed on a worker: the
    /// coordinator commits others ahead of their turn while it runs.
    pub(super) fn frozen_at_turn(
        &mut self,
        state: &mut ActionPlanning,
        configured: &mut ConfiguredNodes,
        pipe: &mut Pipeline,
        index: usize,
        preparing: Preparing,
    ) -> Option<Ahead> {
        let started = std::time::Instant::now();
        let received = loop {
            match preparing.try_recv() {
                Ok(received) => break Ok(received),
                Err(mpsc::TryRecvError::Disconnected) => break Err(mpsc::RecvError),
                Err(mpsc::TryRecvError::Empty) => {
                    if !self.commit_finished_ahead(state, configured, pipe, index, true) {
                        break preparing.recv();
                    }
                }
            }
        };
        FROZEN_STATS[3].fetch_add(
            started.elapsed().as_micros() as u64,
            std::sync::atomic::Ordering::Relaxed,
        );
        self.frozen_received(pipe, index, received)
    }

    fn frozen_received(
        &mut self,
        pipe: &mut Pipeline,
        index: usize,
        received: Result<Result<FrozenOutcome, String>, mpsc::RecvError>,
    ) -> Option<Ahead> {
        let outcome = match received {
            Ok(Ok(outcome)) => outcome,
            Ok(Err(panic)) => {
                // Raised at its turn (P3's panic hook), like a compute's.
                return Some(Ahead::Failed {
                    error: panic,
                    effects: Effects::default(),
                    track: incremental::TrackLog::default(),
                });
            }
            Err(_) => {
                // Cancelled (a failing build): prepared at its turn.
                pipe.prepare_aborted.insert(index);
                return None;
            }
        };
        let FrozenOutcome {
            prepared,
            abort,
            owner,
            effects,
            platform_known,
            elapsed,
            worker,
        } = outcome;
        if let Some(why) = abort {
            FROZEN_STATS[1].fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            prepare_fallback(why);
            if !effects.console.is_empty() {
                note_lost(effects.console.len());
            }
            pipe.stashed.entry(index).or_default().push(effects);
            pipe.prepare_aborted.insert(index);
            return None;
        }
        FROZEN_STATS[0].fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        if let Some((action_owner, console, owner_key)) = owner {
            self.console_owners.insert(action_owner.clone(), console);
            self.aquery_owner(&action_owner, &owner_key, Vec::new);
        }
        let track = incremental::TrackLog::default();
        match (prepared, worker) {
            (Err(error), _) => {
                FROZEN_STATS[2].fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                bump(&stats().failed, 1);
                Some(Ahead::Failed {
                    error,
                    effects,
                    track,
                })
            }
            (Ok(Some(parts)), Some(worker)) => {
                bump(&stats().ahead, 1);
                note_effects("prepare", &effects);
                if let Ok(worker) = &worker {
                    self.publish_ahead(pipe, index, &parts, worker, platform_known);
                }
                Some(Ahead::Computing {
                    parts,
                    effects,
                    track,
                    pending: refilled(worker),
                    platform_known,
                    prepare_elapsed: elapsed,
                })
            }
            _ => {
                pipe.stashed.entry(index).or_default().push(effects);
                pipe.prepare_aborted.insert(index);
                None
            }
        }
    }
}

/// T2: whether a frozen walk publishes the phase-2 results its workers
/// built ahead of their merge (`BAZEL_RS_EVAL_NO_PUBLISH=1`: dependents
/// wait for the merge, as before).
fn publishes() -> bool {
    static PUBLISH: std::sync::LazyLock<bool> = std::sync::LazyLock::new(|| {
        // snapshot-unrecorded: a scheduling switch; the output is the same either way.
        std::env::var_os("BAZEL_RS_EVAL_NO_PUBLISH").is_none()
    });
    *PUBLISH
}

impl Engine {
    /// T2 (docs/EVALUATOR.md §8): entry `index`'s phase 2 finished on a
    /// worker with a delta: its results are published for its dependents'
    /// prepares (Starlark rules: they become ready on it, not on its
    /// merge), its outputs' artifacts provisionally in the index. Not
    /// published: a compute that raised a sentinel or failed, a delta not
    /// built or carrying an error, a prepare whose deferred target platform
    /// the implementation asked for (it is computed again at its turn).
    fn publish_ahead(
        &self,
        pipe: &mut Pipeline,
        index: usize,
        parts: &PreparedParts,
        worker: &WorkerOutcome,
        platform_known: bool,
    ) {
        let Some(frozen) = self.frozen_walk.as_ref() else {
            return;
        };
        if !publishes()
            || worker.sentinel
            || !(platform_known || !worker.outcome.platform_read)
            || worker.outcome.analyzed.is_err()
        {
            return;
        }
        let Some(delta::Built::Delta(built)) =
            worker.outcome.delta.as_ref().map(|delta| &delta.built)
        else {
            return;
        };
        if built.error.is_some() || built.failed_output.is_some() {
            return;
        }
        let stamp = u32::try_from(index).unwrap_or(u32::MAX - 1);
        frozen.results.publish_delta(
            &parts.key,
            built,
            &parts.transitive,
            &frozen.artifacts,
            stamp,
        );
        delta::provisional_bump(0);
        if !built.reads.is_empty() {
            delta::provisional_bump(2);
        }
        pipe.published.insert(parts.key.clone(), index);
        pipe.newly_published.push(parts.key.clone());
    }

    /// [`Engine::plan_dependency_view`] during a frozen walk: the memos
    /// before the walk (as the planning state's), else the walk's,
    /// single-flight with the workers' prepares (one view, one `Target`
    /// identity per key whoever builds it first).
    #[allow(clippy::too_many_arguments)]
    pub(in crate::engine) fn frozen_dependency_view(
        &mut self,
        frozen: &Frozen,
        configured: &mut ConfiguredNodes,
        dry: &mut KeyMap<Artifacts>,
        custom_providers: &mut KeyMap<BTreeMap<String, starlark::values::OwnedFrozenValue>>,
        provider_keys: &mut KeyMap<Vec<String>>,
        runfiles_info: &mut KeyMap<Arc<crate::runfiles::TargetRunfiles>>,
        artifact_index: &mut ArtifactIndex,
        declared: &mut Declared,
        native_output_groups: &mut BTreeMap<
            ConfiguredTargetKey,
            starlark::values::OwnedFrozenValue,
        >,
        validation: &mut BTreeMap<ConfiguredTargetKey, crate::starlark_types::Depset>,
        target_identities: &mut BTreeMap<
            TargetIdentityKey,
            std::sync::Arc<crate::starlark_types::Target>,
        >,
        label: &Label,
        edge_config: ConfigId,
    ) -> Result<DependencyTarget, String> {
        let key = ConfiguredTargetKey::new(label.clone(), edge_config);
        if let Some(view) = frozen.views.get(&key).cloned() {
            crate::plan_timings::note_dependency_view(&(label, edge_config), false);
            // As a hit of the planning state's memo (Self::plan_dependency_view).
            let ahead_built = if !artifact_index.walking() {
                None
            } else if self.ahead {
                self.view_artifacts.get(&key).cloned()
            } else {
                self.view_artifacts.remove(&key)
            };
            if let Some(artifacts) = ahead_built {
                for artifact in artifacts.iter() {
                    register_artifact(artifact_index, &artifact.file, artifact);
                }
            }
            return Ok(view);
        }
        let mut filled_here = false;
        let filled = frozen.memos.views.get_or_fill(&key, || {
            filled_here = true;
            crate::plan_timings::note_dependency_view(&(label, edge_config), true);
            let view = self.dependency_view(
                configured,
                dry,
                custom_providers,
                provider_keys,
                runfiles_info,
                artifact_index,
                declared,
                native_output_groups,
                validation,
                target_identities,
                label,
                edge_config,
            )?;
            // Registered ahead of its turn: a hit at an earlier index
            // registers them again.
            let artifacts = if self.ahead {
                self.view_artifacts.get(&key).cloned()
            } else {
                None
            };
            Ok::<_, String>(Some(prepare::ViewFill {
                view,
                artifacts,
                accesses: None,
            }))
        })?;
        let filled =
            filled.ok_or_else(|| "internal error: a dependency view was not filled".to_owned())?;
        if filled_here {
            return Ok(filled.view.clone());
        }
        crate::plan_timings::note_dependency_view(&(label, edge_config), false);
        // What its builder registered is visible from this index on.
        if let Some(artifacts) = &filled.artifacts {
            for artifact in artifacts.iter() {
                register_artifact(artifact_index, &artifact.file, artifact);
            }
        }
        Ok(filled.view.clone())
    }
}

/// BAZEL_RS_TIMING: where the coordinator's time goes in a walk (µs,
/// exclusive of nested buckets).
pub(super) static SPLIT: [AtomicU64; SPLIT_NAMES.len()] =
    [const { AtomicU64::new(0) }; SPLIT_NAMES.len()];
pub(super) const SPLIT_NAMES: [&str; 12] = [
    "other",
    "drain_lowerings",
    "hooks",
    "delegations_wake",
    "finished_ahead",
    "turn_replay",
    "turn_native",
    "turn_starlark",
    "classify",
    "natives_ahead",
    "dispatch",
    "committed_wake",
];

thread_local! {
    static SPLIT_STACK: std::cell::RefCell<Vec<(usize, std::time::Instant)>> =
        const { std::cell::RefCell::new(Vec::new()) };
}

/// Time the coordinator spends in bucket `bucket` until dropped (the
/// enclosing bucket is paused meanwhile).
pub(super) struct Split(bool);

impl Split {
    pub(super) fn enter(bucket: usize) -> Self {
        if !crate::plan_timings::enabled() {
            return Split(false);
        }
        let now = std::time::Instant::now();
        SPLIT_STACK.with(|stack| {
            let mut stack = stack.borrow_mut();
            if let Some((outer, started)) = stack.last_mut() {
                SPLIT[*outer].fetch_add(
                    now.duration_since(*started).as_micros() as u64,
                    std::sync::atomic::Ordering::Relaxed,
                );
            }
            stack.push((bucket, now));
        });
        Split(true)
    }
}

impl Drop for Split {
    fn drop(&mut self) {
        if !self.0 {
            return;
        }
        let now = std::time::Instant::now();
        SPLIT_STACK.with(|stack| {
            let mut stack = stack.borrow_mut();
            if let Some((bucket, started)) = stack.pop() {
                SPLIT[bucket].fetch_add(
                    now.duration_since(started).as_micros() as u64,
                    std::sync::atomic::Ordering::Relaxed,
                );
            }
            if let Some((_, started)) = stack.last_mut() {
                *started = now;
            }
        });
    }
}
