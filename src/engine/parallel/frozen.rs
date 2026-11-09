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

/// Frozen walks (an evaluated build's pass-2 walks, T3: through the merge
/// buffer) are on unless `BAZEL_RS_EVAL_FROZEN_PREPARES=0` (`all`: every
/// walk, the tests').
pub(super) fn enabled() -> bool {
    walk_min().is_some()
}

/// The shortest walk that prepares on the workers (shorter ones walk with
/// the pipeline), `None`: off.
pub(super) fn walk_min() -> Option<usize> {
    // snapshot-unrecorded: a scheduling switch; the output is the same either way.
    let value = std::env::var("BAZEL_RS_EVAL_FROZEN_PREPARES")
        .ok()
        // A binary built with it (the test suites' mode, like
        // BAZEL_RS_DEFAULT_ANALYSIS).
        .or(option_env!("BAZEL_RS_DEFAULT_EVAL_FROZEN_PREPARES").map(str::to_owned));
    match value.as_deref() {
        Some("0" | "off") => None,
        Some("all") => Some(0),
        _ => Some(2048),
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
    pub(in crate::engine) results: Arc<PlannedResults>,
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
pub(super) struct KeyResults {
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

impl KeyResults {
    /// Its files to build, providers, runfiles, validation group and
    /// transitive repositories.
    #[allow(clippy::type_complexity)]
    pub(super) fn parts(
        &self,
    ) -> (
        Option<Artifacts>,
        Option<BTreeMap<String, starlark::values::OwnedFrozenValue>>,
        Option<Arc<crate::runfiles::TargetRunfiles>>,
        Option<crate::starlark_types::Depset>,
        Option<Arc<BTreeSet<String>>>,
    ) {
        (
            self.dry.clone(),
            self.providers.clone(),
            self.runfiles_info.clone(),
            self.validation.clone(),
            self.repositories.clone(),
        )
    }
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

    pub(super) fn get(&self, key: &ConfiguredTargetKey) -> Option<Arc<KeyResults>> {
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
    /// publishes the planning state's entries over them ([`Self::publish`])
    /// unless they are the very same (T3: a delta merged as built).
    ///
    /// T3: the Files in `shared` are outputs shared with earlier entries:
    /// their artifacts there (naming the earlier entry's action, as the
    /// merge will) replace the delta's own in what is published, provisional
    /// registrations included (a read of them is checked at the reader's
    /// merge, as any provisional read).
    pub(in crate::engine) fn publish_shared(
        &self,
        key: &ConfiguredTargetKey,
        delta: &delta::TargetDelta,
        transitive: &BTreeSet<String>,
        artifacts: &ConcurrentArtifactIndex,
        stamp: u32,
        shared: &HashMap<File, Artifact>,
    ) {
        let mut results = KeyResults {
            repositories: Some(Arc::new(transitive.clone())),
            ..KeyResults::default()
        };
        let resolve = |artifact: &Artifact| {
            shared
                .get(&artifact.file)
                .cloned()
                .unwrap_or_else(|| artifact.clone())
        };
        for op in &delta.ops {
            match op {
                delta::DeltaOp::Providers(providers, keys) => {
                    results.providers = Some(providers.clone());
                    results.provider_keys = Some(keys.clone());
                }
                delta::DeltaOp::Dry(dry) if shared.is_empty() => {
                    results.dry = Some(Arc::clone(dry));
                }
                delta::DeltaOp::Dry(dry) => {
                    results.dry = Some(Arc::new(dry.iter().map(resolve).collect()));
                }
                delta::DeltaOp::RunfilesInfo(runfiles) => {
                    results.runfiles_info = Some(Arc::clone(runfiles));
                }
                // Only its output-file targets' outputs: what a reader of
                // an output-file label asks for (B2). Another declaration
                // of the path (a same-package rule sharing the generating
                // rule's action) is decided by the merges, in plan order.
                delta::DeltaOp::Declared {
                    key,
                    artifact,
                    predeclared: true,
                    ..
                } => {
                    self.declared_shard(key.hash)
                        .write()
                        .unwrap_or_else(|e| e.into_inner())
                        .put(key.clone(), resolve(artifact), true);
                }
                delta::DeltaOp::Artifact(file, artifact, hash) => {
                    artifacts.publish_provisional(*hash, file, &resolve(artifact), stamp);
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
    ///
    /// Only an output-file target's declaration (its generating rule's
    /// predeclared output, what a reader of the label asks for) replaces
    /// one published: another target's declaration of the path merged
    /// earlier in plan order must not hide the generating rule's published
    /// ahead of its merge (a reader of the label is ready once that is).
    pub(in crate::engine) fn publish_declared(&self, state: &ActionPlanning, engine: &Engine) {
        let Some(journal) = state.declared.journal.as_ref() else {
            return;
        };
        let keys = std::mem::take(&mut *journal.lock().unwrap_or_else(|e| e.into_inner()));
        for key in keys {
            if let Some(artifact) = state.declared.get(key.config, &key.path) {
                let mut shard = self
                    .declared_shard(key.hash)
                    .write()
                    .unwrap_or_else(|e| e.into_inner());
                // Absent, or the same owner's: put either way; another
                // owner's is replaced only by an output-file target's.
                let replace = match shard.get(key.config, &key.path) {
                    None => true,
                    Some(known) if known.owner == artifact.owner => true,
                    Some(_) => predeclared(engine, &key, artifact),
                };
                shard.put(key, artifact.clone(), replace);
            }
        }
    }

    /// The results committed since the last walk (`state.dry` only grows;
    /// the declarations by their journal).
    fn catch_up(&self, state: &ActionPlanning, engine: &Engine) {
        for key in state.dry.keys() {
            if self.get(key).is_none() {
                self.publish(key, state);
            }
        }
        self.publish_declared(state, engine);
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

/// Whether `artifact`, declared at `key`, is an output-file target's: a
/// predeclared output of the rule owning it.
fn predeclared(engine: &Engine, key: &DeclaredKey, artifact: &Artifact) -> bool {
    let Ok(owner) = Label::parse_canonical(&artifact.owner) else {
        return false;
    };
    let Some(Ok(Target::Rule(rule))) = engine.target_readonly(&owner) else {
        return false;
    };
    let output_root = engine.configurations.output_root(key.config);
    rule.outs.iter().any(|out| {
        rule.label
            .with_name(out)
            .is_ok_and(|label| label.generated_exec_path(&output_root) == key.path)
    })
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
    pub(super) prepared: Result<Option<Box<PreparedParts>>, String>,
    pub(super) abort: Option<&'static str>,
    pub(super) owner: Option<(String, crate::shared_path::SharedPath, ConfiguredTargetKey)>,
    pub(super) effects: Effects,
    pub(super) platform_known: bool,
    pub(super) elapsed: std::time::Duration,
    /// Its compute (absent when the prepare did not give one).
    pub(super) worker: Option<Result<WorkerOutcome, String>>,
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
        let engine = self.fork_frozen();
        crate::perf_mark("pass2.freeze.engine");
        let configured = configured.clone();
        crate::perf_mark("pass2.freeze.configured");
        let results = match state.walk_results.take() {
            // What was committed since the last frozen walk.
            Some(results) => {
                results.catch_up(state, self);
                results
            }
            None => Arc::new(PlannedResults::seed(state)),
        };
        crate::perf_mark("pass2.freeze.results");
        Arc::new(Frozen {
            engine,
            configured,
            views: std::mem::take(&mut state.views),
            identities: std::mem::take(&mut state.target_identities),
            output_groups: std::mem::take(&mut state.native_output_groups),
            results,
            memos: prepare::BatchMemos::default(),
            artifacts: Arc::clone(&state.artifact_index.shared),
            view,
            compact_repo_mapping: self.compact_repo_mapping,
        })
    }

    /// The walk ended: the memos back into the planning state, with what
    /// the walk filled.
    pub(super) fn thaw(&mut self, state: &mut ActionPlanning, frozen: Arc<Frozen>) {
        // The journal stays on: the next frozen walk's results catch up
        // with what is declared until then.
        state.walk_results = Some(Arc::clone(&frozen.results));
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
        self.apply_memos(state, memos, false);
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
            let stopped = || {
                cancelled
                    .as_ref()
                    .is_some_and(|flag| flag.load(std::sync::atomic::Ordering::SeqCst))
            };
            if stopped() {
                return;
            }
            let (result, summary, platform_read) = run_frozen(
                &frozen,
                index,
                &key,
                &rule,
                jitter,
                verify_summary,
                false,
                &stopped,
            );
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
            // Raised at its turn (P3's panic hook), like a compute's.
            Ok(Err(panic)) => return Some(Ahead::Panicked(panic)),
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

/// A worker's prepare, compute and delta of plan entry `index` over
/// `frozen` ([`Engine::dispatch_frozen`], the merge buffer's Starlark
/// job): the outcome (`Err`: the text of a panic), the outputs its commit
/// registers, and whether its implementation asked for a deferred target
/// platform.
#[allow(clippy::too_many_arguments)]
pub(super) fn run_frozen(
    frozen: &Frozen,
    index: usize,
    key: &ConfiguredTargetKey,
    rule: &Arc<Rule>,
    jitter: u64,
    verify_summary: bool,
    keep_on_meet: bool,
    stopped: &dyn Fn() -> bool,
) -> (
    Result<FrozenOutcome, String>,
    Option<Arc<OutputSummary>>,
    bool,
) {
    let mut summary = None;
    let mut platform_read = true;
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        crate::debug_panic::at("analysis-worker");
        if jitter > 0 {
            let roll = loading_jitter_roll(index as u64, 0x5034) % (jitter + 1);
            let until = std::time::Instant::now() + std::time::Duration::from_millis(roll);
            while std::time::Instant::now() < until && !stopped() {
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
        }
        let class = crate::plan_timings::class_id(rule.rule_class_name());
        let platform_known = frozen.engine.platform_resolved(key);
        let isolation = Isolation::enter();
        let started = std::time::Instant::now();
        let timer = crate::plan_timings::StageTimer::start(
            crate::plan_timings::PlanStage::Prepare,
            index as u64,
            class,
        );
        let mut cx = prepare::SharedPrepare::frozen(frozen, index);
        let prepared = Engine::prepare_custom(&mut cx, key, rule);
        timer.finish();
        let elapsed = started.elapsed();
        let (effects, sentinel) = isolation.finish();
        let prepare::SharedRecord { owner, abort, .. } = cx.record;
        let aborted = sentinel
            || abort.is_some()
            || prepared
                .as_ref()
                .is_err_and(|error| crate::repository_demand::is_inline_rerun_sentinel(error));
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
                    rule,
                    parts.config,
                    &parts.action_execution,
                ),
                class,
                keep_on_meet,
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
    (result, summary, platform_read)
}

/// A native rule's results computed on a worker over `frozen`
/// ([`Engine::dispatch_native`], the merge buffer's native job; `None`:
/// not computable there).
pub(super) fn run_native(
    frozen: &Frozen,
    index: usize,
    key: &ConfiguredTargetKey,
    rule: &Arc<Rule>,
) -> Option<Box<NativePublished>> {
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let isolation = Isolation::enter();
        let engine = &frozen.engine;
        let reads: &dyn PlanReads = &*frozen.results;
        let mut cx = prepare::SharedPrepare::frozen(frozen, index);
        // T3: its declared source files resolved here (applied at its
        // merge, NativePublished::sources).
        cx.resolve_sources = true;
        let mut transitive = BTreeSet::from([rule.label.repository.clone()]);
        let edges = engine.rule_edges(rule, key.config);
        engine.native_transitive_in(reads, rule, key, &edges, &mut transitive);
        let validation = Engine::native_validation_in(&mut cx, reads, rule, key.config);
        let output_group = super::native_results::selected_output_group(rule);
        let mut declared = Vec::new();
        let genrule = matches!(rule.kind, RuleKind::Genrule);
        let plain = Engine::inputs_in(&mut cx, reads, rule, key.config).and_then(|inputs| {
            if genrule {
                // T3: a genrule's results are its outputs (its commit's
                // `dry`, `declared`, data runfiles; their producer its
                // action), whatever its command expands to.
                let mut registered = inputs;
                let output_root = engine.configurations.output_root(key.config);
                let action_owner = key.action_owner(engine.configurations.top());
                let mut artifacts = Vec::with_capacity(rule.outs.len());
                for (index, out) in rule.outs.iter().enumerate() {
                    let exec = rule.label.with_name(out)?.generated_exec_path(&output_root);
                    artifacts.push(Artifact::new(
                        exec.clone(),
                        exec,
                        Some((action_owner.clone(), index)),
                        rule.label.to_string(),
                        false,
                        false,
                    ));
                }
                if artifacts.is_empty() {
                    return Err("a genrule without outputs".to_owned());
                }
                let files = artifacts.iter().map(Artifact::file).collect::<Vec<_>>();
                let runfiles = crate::runfiles::TargetRunfiles::genrule(
                    &files,
                    super::super::genrule_is_executable(rule),
                )
                .map_err(|error| error.to_string())?;
                registered.extend(artifacts.iter().cloned());
                declared = artifacts.clone();
                return Ok((
                    super::native_results::NativePlain {
                        dry: artifacts,
                        providers: None,
                        runfiles_info: Some(Arc::new(runfiles)),
                    },
                    registered,
                ));
            }
            let mut registered = inputs.clone();
            // A filegroup's output group: its `srcs`' views.
            let inputs = match &output_group {
                None => inputs,
                Some(group) => {
                    let mut views = Vec::new();
                    for text in &rule.srcs {
                        let label = engine.resolve_rule_src(rule, text)?;
                        views.push(cx.plan_view(&label, key.config)?);
                    }
                    let reader = u32::try_from(index).unwrap_or(u32::MAX - 1);
                    let mut selected = Vec::new();
                    for file in super::native_results::output_group_files(&views, group) {
                        // A workspace status file is registered on
                        // first need, by its commit.
                        let (artifact, _) = frozen
                            .artifacts
                            .get_entry(&file, reader)
                            .ok_or_else(|| "unregistered".to_owned())?;
                        selected.push(artifact);
                    }
                    selected
                }
            };
            Engine::native_plain_in(
                &mut cx,
                reads,
                &frozen.configured,
                rule,
                key,
                key.config,
                inputs,
                &mut |artifact| registered.push(artifact.clone()),
            )
            .map(|plain| (plain, registered))
        });
        let (effects, sentinel) = isolation.finish();
        let quiet = effects.console.is_empty()
            && effects.observations.is_empty()
            && effects.roots.is_empty();
        if sentinel || !quiet || cx.record.abort.is_some() {
            return None;
        }
        let (plain, registered) = plain.ok()?;
        Some(Box::new(NativePublished {
            results: KeyResults {
                dry: Some(Arc::new(plain.dry)),
                providers: plain.providers,
                provider_keys: None,
                runfiles_info: plain.runfiles_info,
                validation,
                repositories: Some(Arc::new(transitive)),
            },
            registered,
            declared: declared
                .into_iter()
                .map(|artifact| (key.config, artifact))
                .collect(),
            sources: std::mem::take(&mut cx.record.sources),
        }))
    }))
    .ok()
    .flatten()
}

/// Publish a native rule's results computed on a worker (plan index
/// `index`): its results for its dependents, its inputs' registrations
/// provisionally (returned: what its commit registers).
pub(super) fn publish_native_results(
    frozen: &Frozen,
    index: usize,
    key: &ConfiguredTargetKey,
    published: NativePublished,
) -> NativeMerge {
    let stamp = u32::try_from(index).unwrap_or(u32::MAX - 1);
    let NativePublished {
        results,
        registered,
        declared,
        sources,
    } = published;
    for artifact in &registered {
        frozen.artifacts.publish_provisional(
            file_hash(&artifact.file),
            &artifact.file,
            artifact,
            stamp,
        );
    }
    for (config, artifact) in declared {
        let key = DeclaredKey::new(config, artifact.exec.to_path_buf());
        frozen
            .results
            .declared_shard(key.hash)
            .write()
            .unwrap_or_else(|e| e.into_inner())
            .put(key, artifact, true);
    }
    frozen
        .results
        .key_shard(key)
        .write()
        .unwrap_or_else(|e| e.into_inner())
        .insert(key.clone(), Arc::new(results));
    delta::provisional_bump(4);
    profile(|profile| profile.publish(index));
    NativeMerge {
        registered,
        sources,
    }
}

/// What the merge of a native rule's published results applies besides
/// them: what its commit registers, the source files its worker resolved.
pub(super) struct NativeMerge {
    pub(super) registered: Vec<Artifact>,
    pub(super) sources: Vec<(Label, PathBuf, bool, SourceSide)>,
}

/// Publish entry `index`'s phase-2 results from its worker's delta
/// ([`Engine::publish_ahead`]'s rule); whether they were.
pub(super) fn publish_starlark_results(
    frozen: &Frozen,
    index: usize,
    parts: &PreparedParts,
    worker: &WorkerOutcome,
    platform_known: bool,
) -> bool {
    profile(|profile| profile.publish(index));
    if !publishes()
        || worker.sentinel
        || !(platform_known || !worker.outcome.platform_read)
        || worker.outcome.analyzed.is_err()
    {
        return false;
    }
    let Some(delta::Built::Delta(built)) = worker.outcome.delta.as_ref().map(|delta| &delta.built)
    else {
        return false;
    };
    if built.error.is_some() || built.failed_output.is_some() {
        return false;
    }
    let stamp = u32::try_from(index).unwrap_or(u32::MAX - 1);
    // T3: outputs shared with earlier entries name their artifact (the
    // index's now); one not registered yet leaves it unpublished.
    let keep = worker
        .outcome
        .delta
        .as_ref()
        .is_some_and(|delta| delta.keep);
    let mut shared = HashMap::new();
    if keep {
        let Some(view) = frozen.view.as_ref() else {
            return false;
        };
        for op in &built.ops {
            let delta::DeltaOp::Output(path, _) = op else {
                continue;
            };
            if !view.meets_earlier(path.hash(), stamp) {
                continue;
            }
            // Its own artifact of the path (its registration, declaration
            // or files), and the earlier entry's (its own File: another
            // owner's is a distinct entry): the merge names that one's
            // action as the producer (output_artifact's `first`).
            let Some(own) = built.ops.iter().find_map(|op| match op {
                delta::DeltaOp::Artifact(_, artifact, _)
                | delta::DeltaOp::Declared { artifact, .. }
                    if artifact.exec == *path.path() =>
                {
                    Some(artifact.clone())
                }
                delta::DeltaOp::Dry(dry) => dry
                    .iter()
                    .find(|artifact| artifact.exec == *path.path())
                    .cloned(),
                _ => None,
            }) else {
                delta::provisional_bump(6);
                return false;
            };
            let winner = frozen
                .artifacts
                .entry_at(&own.file.path, stamp)
                .and_then(|(_, artifact)| artifact.producer.clone());
            let Some((id, _)) = winner else {
                delta::provisional_bump(6);
                return false;
            };
            shared.insert(own.file.clone(), own.with_producer_id(&id));
        }
        delta::provisional_bump(5);
    }
    frozen.results.publish_shared(
        &parts.key,
        built,
        &parts.transitive,
        &frozen.artifacts,
        stamp,
        &shared,
    );
    delta::provisional_bump(0);
    if !built.reads.is_empty() {
        delta::provisional_bump(2);
    }
    true
}

/// T2, `BAZEL_RS_TIMING`: when each phase-2 result of the frozen walks
/// was published and its entry's turn came (µs since its walk started),
/// by plan index: the Ct2 completion profile and the merge's lag.
#[derive(Default)]
pub(super) struct Ct2Profile {
    started: Option<std::time::Instant>,
    published: HashMap<usize, u64>,
    /// (publication, turn) of the published entries whose turn came.
    lags: Vec<(u64, u64)>,
    /// Every turn of a frozen walk (µs), the walks' total.
    turns: u64,
    walked_us: u64,
}

pub(super) static CT2_PROFILE: std::sync::Mutex<Option<Ct2Profile>> = std::sync::Mutex::new(None);

pub(super) fn profile(each: impl FnOnce(&mut Ct2Profile)) {
    if crate::plan_timings::enabled() {
        let mut profile = CT2_PROFILE.lock().unwrap_or_else(|e| e.into_inner());
        each(profile.get_or_insert_with(Ct2Profile::default));
    }
}

impl Ct2Profile {
    fn now(&self) -> u64 {
        self.started
            .map_or(0, |started| started.elapsed().as_micros() as u64)
    }

    pub(super) fn walk_start(&mut self) {
        self.started = Some(std::time::Instant::now());
        self.published.clear();
    }

    pub(super) fn walk_end(&mut self) {
        self.walked_us += self.now();
        self.started = None;
    }

    pub(super) fn publish(&mut self, index: usize) {
        if self.started.is_some() {
            let now = self.now();
            self.published.insert(index, now);
        }
    }

    pub(super) fn turn(&mut self, index: usize) {
        if self.started.is_none() {
            return;
        }
        self.turns += 1;
        if let Some(published) = self.published.remove(&index) {
            let now = self.now();
            self.lags.push((published, now));
        }
    }

    /// The report line (`PERF-PARALLEL ct2`).
    pub(super) fn line(&mut self) -> String {
        let pct = |values: &mut Vec<u64>, p: usize| -> f64 {
            if values.is_empty() {
                return 0.0;
            }
            values.sort_unstable();
            values[(values.len() - 1) * p / 100] as f64 / 1e6
        };
        let mut published: Vec<u64> = self.lags.iter().map(|(at, _)| *at).collect();
        let mut turned: Vec<u64> = self.lags.iter().map(|(_, at)| *at).collect();
        let mut lag: Vec<u64> = self
            .lags
            .iter()
            .map(|(published, turn)| turn.saturating_sub(*published))
            .collect();
        format!(
            "PERF-PARALLEL ct2 walks={:.3}s turns={} published={} published_at(p50/p90/p99/max)={:.3}/{:.3}/{:.3}/{:.3}s their_turn_at(p50/p90/max)={:.3}/{:.3}/{:.3}s merge_lag(p50/p90/max)={:.3}/{:.3}/{:.3}s",
            self.walked_us as f64 / 1e6,
            self.turns,
            self.lags.len(),
            pct(&mut published, 50),
            pct(&mut published, 90),
            pct(&mut published, 99),
            pct(&mut published, 100),
            pct(&mut turned, 50),
            pct(&mut turned, 90),
            pct(&mut turned, 100),
            pct(&mut lag, 50),
            pct(&mut lag, 90),
            pct(&mut lag, 100),
        )
    }
}

/// T2: a native rule's results computed on a worker
/// ([`Engine::dispatch_native`]): what its dependents read and what its
/// commit registers in the index (published provisionally).
pub(super) struct NativePublished {
    results: KeyResults,
    registered: Vec<Artifact>,
    /// A genrule's outputs, declared in their configuration.
    declared: Vec<(ConfigId, Artifact)>,
    /// The source files it resolved itself ([`prepare::SharedRecord`]).
    pub(super) sources: Vec<(Label, PathBuf, bool, SourceSide)>,
}

/// A native rule's results being computed on a worker (`None`: not
/// computable there; its dependents wait for its commit).
pub(super) type NativeJob = mpsc::Receiver<Option<Box<NativePublished>>>;

/// [`Engine::check_published`]'s comparison: `Err(what)` differs.
/// `values`: provider values compared in release builds too.
fn compare_published(
    published: &KeyResults,
    state: &ActionPlanning,
    key: &ConfiguredTargetKey,
    native: bool,
    values: bool,
) -> Result<(), &'static str> {
    let files =
        |dry: Option<&Artifacts>| dry.map(|dry| dry.iter().map(Artifact::file).collect::<Vec<_>>());
    let same_dry = match (published.dry.as_ref(), state.dry.get(key)) {
        (Some(a), Some(b)) if Arc::ptr_eq(a, b) => true,
        (a, b) => files(a) == files(b),
    };
    if !same_dry {
        return Err("files to build");
    }
    let values = values || cfg!(debug_assertions) || delta::verify_delta();
    let same_providers = match (
        published.providers.as_ref(),
        state.custom_providers.get(key),
    ) {
        (None, None) => true,
        (Some(published), Some(committed)) => {
            published.len() == committed.len()
                && published
                    .iter()
                    .zip(committed)
                    .all(|((id, value), (other_id, other))| {
                        id == other_id
                            && (value.value().ptr_eq(other.value())
                                || !values
                                || value.value().to_repr() == other.value().to_repr())
                    })
        }
        _ => false,
    };
    if !same_providers {
        return Err("providers");
    }
    if let Some(keys) = published.provider_keys.as_ref()
        && state.provider_keys.get(key) != Some(keys)
    {
        return Err("provider keys");
    }
    let same_runfiles = match (
        published.runfiles_info.as_ref(),
        state.runfiles_info.get(key),
    ) {
        (None, None) => true,
        (Some(a), Some(b)) => crate::runfiles::TargetRunfiles::same_published(a, b),
        _ => false,
    };
    if !same_runfiles {
        return Err("runfiles");
    }
    // A Starlark rule's published results carry no validation group.
    if native && published.validation.is_some() != state.validation.contains_key(key) {
        return Err("validation group");
    }
    if published.repositories.as_deref() != state.repositories.get(key).map(|known| &**known) {
        return Err("transitive repositories");
    }
    Ok(())
}

/// T2: whether a frozen walk publishes `rule`'s results from a worker
/// (`native_results::plain`, no coverage instrumentation, an execution
/// plan).
pub(super) fn publishes_native(engine: &Engine, rule: &Rule, config: ConfigId) -> bool {
    publishes()
        && (super::native_results::plain(rule)
            || super::native_results::selected_output_group(rule).is_some()
            || matches!(rule.kind, RuleKind::Genrule))
        && !engine.semantic_only()
        && !engine
            .configurations
            .get(config)
            .native
            .collect_code_coverage
}

impl Engine {
    /// T2: compute the results of the native rule at plan index `index` on
    /// a worker over `frozen` (its dependencies' results published or
    /// committed), as its commit would (`native_results`).
    pub(super) fn dispatch_native(
        &self,
        frozen: &Arc<Frozen>,
        index: usize,
        key: ConfiguredTargetKey,
        rule: Arc<Rule>,
    ) -> NativeJob {
        let (sender, receiver) = mpsc::sync_channel(1);
        let frozen = Arc::clone(frozen);
        let finished = self
            .analysis_pool
            .as_ref()
            .map(|pool| pool.finished.0.clone());
        let job: Job = Box::new(move || {
            let published = run_native(&frozen, index, &key, &rule);
            let _ = sender.send(published);
            if let Some(finished) = finished {
                let _ = finished.send((index, None, true, std::time::Instant::now()));
            }
        });
        if let Some(pool) = &self.analysis_pool
            && let Err(job) = pool.submit(index, job)
        {
            job();
        }
        receiver
    }

    /// T2: at the commit of `key` whose results a worker published (a
    /// native rule's computed by `native_results`, a Starlark rule's from
    /// its delta), they are the committed ones: its dependents' phase 2s
    /// read them. The Files, not the Artifacts: a dependency's provisional
    /// producer is checked by its readers' merges. Provider values are
    /// compared by identity, else by contents: always for a native rule
    /// forwarding another target's providers (an alias), in debug builds
    /// and under BAZEL_RS_VERIFY_DELTA otherwise (a platform provider is
    /// frozen again by each computation, a Starlark rule's rebuilt delta
    /// computes its providers again).
    pub(super) fn check_published(
        &self,
        state: &ActionPlanning,
        key: &ConfiguredTargetKey,
        rule: &Rule,
    ) {
        let Some(frozen) = self.frozen_walk.as_ref() else {
            return;
        };
        let Some(published) = frozen.results.get(key) else {
            return;
        };
        // A failed commit (its error is the build's at its turn) leaves no
        // files to build: nothing was committed to compare.
        if !state.dry.contains_key(key) {
            return;
        }
        let native = !matches!(rule.kind, RuleKind::Custom);
        // Its output-file targets' declarations (what a reader of their
        // labels got), against the committed ones (the File).
        let output_root = self.configurations.output_root(key.config);
        for out in &rule.outs {
            let Ok(label) = rule.label.with_name(out) else {
                continue;
            };
            let exec = label.generated_exec_path(&output_root);
            if let Some(published) = frozen.results.declared(key.config, &exec)
                && state
                    .declared
                    .get(key.config, &exec)
                    .is_none_or(|committed| committed.file() != published.file())
            {
                panic!(
                    "internal error: BAZEL_RS_ANALYSIS=eval: the declaration of {label} published ahead of its merge differs from the committed one (BAZEL_RS_EVAL_NO_PUBLISH=1 waits for the merges)"
                )
            }
        }
        if let Err(what) = compare_published(
            &published,
            state,
            key,
            native,
            native && super::super::forwards(rule),
        ) {
            panic!(
                "internal error: BAZEL_RS_ANALYSIS=eval: the {what} of {} published ahead of its merge differ from the committed ones (BAZEL_RS_EVAL_NO_PUBLISH=1 waits for the merges)",
                key.label
            )
        }
    }

    /// T2: the results of native rule `key` (plan index `index`) computed
    /// on a worker, published unless it committed meanwhile.
    pub(super) fn publish_native(
        &self,
        state: &ActionPlanning,
        pipe: &mut Pipeline,
        index: usize,
        key: &ConfiguredTargetKey,
        published: NativePublished,
    ) {
        let Some(frozen) = self.frozen_walk.as_ref() else {
            return;
        };
        if state.dry.contains_key(key) || pipe.published.contains_key(key) {
            return;
        }
        publish_native_results(frozen, index, key, published);
        pipe.published.insert(key.clone(), index);
        pipe.newly_published.push(key.clone());
    }
}

/// T2: how far past the turn a frozen walk classifies, and how many
/// entries it holds prepared ahead (computing or finished, not committed).
pub(super) fn bounds() -> (usize, usize) {
    static BOUNDS: std::sync::LazyLock<(usize, usize)> = std::sync::LazyLock::new(|| {
        let get = |name: &str, default: usize| {
            // snapshot-unrecorded: a scheduling bound; the output is the same either way.
            std::env::var(name)
                .ok()
                .and_then(|value| value.parse().ok())
                .unwrap_or(default)
        };
        (
            get("BAZEL_RS_T2_HORIZON", HORIZON),
            get("BAZEL_RS_T2_HELD", 768),
        )
    });
    *BOUNDS
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
        if !publish_starlark_results(frozen, index, parts, worker, platform_known) {
            return;
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
pub(super) const SPLIT_NAMES: [&str; 17] = [
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
    "cursor_claim",
    "merge_starlark",
    "merge_native",
    "at_turn",
    "fast_native",
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

#[cfg(test)]
mod tests {
    use super::*;

    fn out_txt(producer: &str) -> Artifact {
        let path = PathBuf::from("bazel-out/k8-fastbuild/bin/pkg/p.txt");
        Artifact::new(
            path.clone(),
            path,
            Some((producer.into(), 0)),
            "//pkg:p.txt".into(),
            false,
            false,
        )
    }

    fn declared_delta(artifact: &Artifact, predeclared: bool) -> delta::TargetDelta {
        delta::TargetDelta {
            ops: vec![delta::DeltaOp::Declared {
                key: DeclaredKey::new(
                    crate::config_registry::no_config().id,
                    artifact.exec.to_path_buf(),
                ),
                artifact: artifact.clone(),
                // On a worker no earlier producer is known: always inserted.
                replace: true,
                predeclared,
            }],
            error: None,
            unchecked: true,
            uses: Vec::new(),
            failed_output: None,
            reads: Vec::new(),
        }
    }

    /// T2 review: W predeclares `p`; L of the same package (not the
    /// generating rule) declares `p` too, sharing the action, and
    /// publishes after W. A reader of the output-file label `//pkg:p.txt`
    /// still gets W's declaration: a published result makes only its
    /// predeclared outputs visible (the merges decide the rest).
    #[test]
    fn only_predeclared_outputs_are_published_as_declared() {
        let (generating, sharer) = (out_txt("//pkg:w#0"), out_txt("//pkg:l#0"));
        let path = generating.exec.to_path_buf();
        let results = PlannedResults::new();
        let index = ConcurrentArtifactIndex::default();
        let transitive = BTreeSet::new();
        let w = ConfiguredTargetKey::new(
            Label::parse_canonical("//pkg:w").unwrap(),
            crate::config_registry::no_config().id,
        );
        let l = ConfiguredTargetKey::new(
            Label::parse_canonical("//pkg:l").unwrap(),
            crate::config_registry::no_config().id,
        );
        // L alone, published late: not a declaration of `p`.
        results.publish_shared(
            &l,
            &declared_delta(&sharer, false),
            &transitive,
            &index,
            7,
            &HashMap::new(),
        );
        assert!(
            results
                .declared(crate::config_registry::no_config().id, &path)
                .is_none()
        );
        results.publish_shared(
            &w,
            &declared_delta(&generating, true),
            &transitive,
            &index,
            3,
            &HashMap::new(),
        );
        results.publish_shared(
            &l,
            &declared_delta(&sharer, false),
            &transitive,
            &index,
            7,
            &HashMap::new(),
        );
        let declared = results
            .declared(crate::config_registry::no_config().id, &path)
            .unwrap();
        assert!(same_artifact(&declared, &generating));
    }

    /// T2 review: a Starlark rule's published results are compared with
    /// what its merge committed.
    #[test]
    fn published_results_are_compared_with_the_committed_ones() {
        let key = ConfiguredTargetKey::new(
            Label::parse_canonical("//pkg:w").unwrap(),
            crate::config_registry::no_config().id,
        );
        let mut state = ActionPlanning::default();
        let (a, b) = (out_txt("//pkg:w#0"), {
            let path = PathBuf::from("bazel-out/k8-fastbuild/bin/pkg/q.txt");
            Artifact::new(
                path.clone(),
                path,
                Some(("//pkg:w#1".into(), 0)),
                "//pkg:q.txt".into(),
                false,
                false,
            )
        });
        state.dry.insert(key.clone(), Arc::new(vec![a.clone()]));
        state
            .provider_keys
            .insert(key.clone(), vec!["DefaultInfo".to_owned()]);
        state.runfiles_info.insert(key.clone(), Arc::default());
        let published = |dry: Artifact, keys: &[&str]| KeyResults {
            dry: Some(Arc::new(vec![dry])),
            providers: None,
            provider_keys: Some(keys.iter().map(|key| (*key).to_owned()).collect()),
            runfiles_info: Some(Arc::default()),
            validation: None,
            repositories: None,
        };
        assert_eq!(
            compare_published(
                &published(a.clone(), &["DefaultInfo"]),
                &state,
                &key,
                false,
                false
            ),
            Ok(())
        );
        // Another producer of the same File: the same files to build.
        assert_eq!(
            compare_published(
                &published(out_txt("//pkg:l#0"), &["DefaultInfo"]),
                &state,
                &key,
                false,
                false
            ),
            Ok(())
        );
        assert_eq!(
            compare_published(&published(b, &["DefaultInfo"]), &state, &key, false, false),
            Err("files to build")
        );
        assert_eq!(
            compare_published(&published(a, &["OtherInfo"]), &state, &key, false, false),
            Err("provider keys")
        );
    }
}
