//! S1 (docs/PARALLEL_ANALYSIS.md, "S1 — commit deltas"): a Starlark
//! target's commit built as data on the analysis worker that computed it
//! ([`Engine::build_delta`]) and merged by the coordinator in plan order
//! ([`Engine::merge_delta`]). The sequential engine runs the same two
//! functions back to back on one thread.
//!
//! What a delta reads is shared with the workers: the build's artifact
//! index ([`super::ConcurrentArtifactIndex`], read with the target's own
//! plan index), the memoized tool runfiles groups and shared input runs
//! ([`SingleFlight`]), the client environment. What it writes is a list of
//! operations in the order the sequential commit made them; the producers
//! of output paths stay coordinator-only and are written only by the
//! merge.

use super::*;

/// The shards of a [`SingleFlight`] memo (a power of two).
const MEMO_SHARDS: usize = 16;

/// One memo entry: filled once (the fill holds the entry's own lock, so a
/// second filler waits and takes the first one's value).
type Slot<V> = Arc<std::sync::Mutex<Option<Arc<V>>>>;

/// A memo filled once per key by whichever thread needs it first — the
/// coordinator or an analysis worker building a delta — and shared by
/// every later use (one value, so one `Arc` identity, per key). A fill that
/// fails (`None` or an error) leaves the entry empty for the next use.
///
/// A fill only looks artifacts up in the build's index (a workspace status
/// file, which would be registered on first need, makes a worker's delta
/// fall back to the coordinator), so it has no effect a merge must replay:
/// each delta lists the memo keys it used, in order ([`MemoUse`]), for
/// BAZEL_RS_VERIFY_DELTA's comparison.
pub(super) struct SingleFlight<K, V> {
    shards: Box<[std::sync::Mutex<MemoShard<K, V>>]>,
}

/// One shard of a [`SingleFlight`] memo.
type MemoShard<K, V> = HashMap<K, Slot<V>>;

impl<K, V> Default for SingleFlight<K, V> {
    fn default() -> Self {
        Self {
            shards: (0..MEMO_SHARDS).map(|_| Default::default()).collect(),
        }
    }
}

impl<K: std::hash::Hash + Eq + Clone, V> SingleFlight<K, V> {
    fn shard(&self, key: &K) -> &std::sync::Mutex<MemoShard<K, V>> {
        use std::hash::BuildHasher;
        let hash = crate::fast_hash::FxBuildHasher::default().hash_one(key);
        &self.shards[(hash >> 60) as usize & (MEMO_SHARDS - 1)]
    }

    fn slot(&self, key: &K) -> Slot<V> {
        let mut shard = self.shard(key).lock().unwrap_or_else(|e| e.into_inner());
        Arc::clone(shard.entry(key.clone()).or_default())
    }

    /// The value of `key` if it is filled (no wait for a fill in progress).
    pub(super) fn peek(&self, key: &K) -> Option<Arc<V>> {
        let slot = {
            let shard = self.shard(key).lock().unwrap_or_else(|e| e.into_inner());
            Arc::clone(shard.get(key)?)
        };
        slot.try_lock().ok().and_then(|value| value.clone())
    }

    /// T2: drop `key`'s entry (its value was filled from reads that are
    /// stale now): the next use fills it again.
    pub(super) fn remove(&self, key: &K) {
        self.shard(key)
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(key);
    }

    /// Whether `key` is filled (no wait for a fill in progress: `false`).
    pub(super) fn is_filled(&self, key: &K) -> bool {
        let slot = {
            let shard = self.shard(key).lock().unwrap_or_else(|e| e.into_inner());
            match shard.get(key) {
                Some(slot) => Arc::clone(slot),
                None => return false,
            }
        };
        slot.try_lock().is_ok_and(|value| value.is_some())
    }

    /// The value of `key`, filled by `fill` unless it is already (`None`
    /// when `fill` returns none: the entry stays empty).
    pub(super) fn get_or_fill<E>(
        &self,
        key: &K,
        fill: impl FnOnce() -> Result<Option<V>, E>,
    ) -> Result<Option<Arc<V>>, E> {
        let slot = self.slot(key);
        let mut value = slot.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(filled) = value.as_ref() {
            return Ok(Some(Arc::clone(filled)));
        }
        Ok(fill()?.map(|filled| {
            let filled = Arc::new(filled);
            *value = Some(Arc::clone(&filled));
            filled
        }))
    }

    /// Every filled entry, consumed (S2: a prepare batch's memo fills move
    /// into the planning state; no order is observed).
    pub(super) fn into_filled(self) -> Vec<(K, Arc<V>)> {
        let mut filled = Vec::new();
        for shard in self.shards.into_vec() {
            let shard = shard.into_inner().unwrap_or_else(|e| e.into_inner());
            for (key, slot) in shard {
                if let Some(value) = slot.lock().unwrap_or_else(|e| e.into_inner()).take() {
                    filled.push((key, value));
                }
            }
        }
        filled
    }

    /// Every filled value (reports only; no order is observed).
    pub(super) fn values(&self) -> Vec<Arc<V>> {
        let mut values = Vec::new();
        for shard in self.shards.iter() {
            let shard = shard.lock().unwrap_or_else(|e| e.into_inner());
            for slot in shard.values() {
                if let Some(value) = slot.lock().unwrap_or_else(|e| e.into_inner()).as_ref() {
                    values.push(Arc::clone(value));
                }
            }
        }
        values
    }
}

/// The hashes of every registered output path and of its proper ancestors
/// (Producers::insert), which the analysis workers read: a worker whose
/// commit would register a path that meets one (the path itself, an
/// ancestor or a path below it is registered) leaves its delta to the
/// coordinator, where the registration is checked against the producers
/// themselves. A hash collision only does that needlessly.
///
/// The workers also note the outputs of every compute they finish, with
/// its plan index ([`Self::meet_earlier`]): a compute whose outputs meet an
/// earlier one's (a shared action of two siblings computed side by side)
/// leaves its delta to the coordinator too, as the earlier one registers
/// first.
#[derive(Default)]
pub(super) struct PathHashes {
    paths: std::sync::Mutex<crate::fast_hash::FxHashSet<u64>>,
    ancestors: std::sync::Mutex<crate::fast_hash::FxHashSet<u64>>,
    /// The output paths of finished computes and their ancestors, with
    /// the lowest plan index that computed each.
    computed: std::sync::Mutex<crate::fast_hash::FxHashMap<u64, u32>>,
    computed_ancestors: std::sync::Mutex<crate::fast_hash::FxHashMap<u64, u32>>,
}

impl PathHashes {
    /// Note a registered output path, by its hash and its proper
    /// ancestors' (nearest first, [`HashedPath`]).
    pub(super) fn insert_hashed(&self, path: u64, proper_ancestors: &[u64]) {
        self.paths
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(path);
        let mut ancestors = self.ancestors.lock().unwrap_or_else(|e| e.into_inner());
        for hash in proper_ancestors {
            if !ancestors.insert(*hash) {
                break;
            }
        }
    }

    /// Whether registering `summary`'s paths would meet a registered path.
    pub(super) fn meet(&self, summary: &parallel::OutputSummary) -> bool {
        {
            let paths = self.paths.lock().unwrap_or_else(|e| e.into_inner());
            if summary
                .paths
                .iter()
                .chain(&summary.ancestors)
                .any(|hash| paths.contains(hash))
            {
                return true;
            }
        }
        let ancestors = self.ancestors.lock().unwrap_or_else(|e| e.into_inner());
        summary.paths.iter().any(|hash| ancestors.contains(hash))
    }

    /// Note the outputs of the compute at plan index `index`, and whether
    /// they meet those of an earlier compute.
    pub(super) fn meet_earlier(&self, index: u32, summary: &parallel::OutputSummary) -> bool {
        let note = |map: &std::sync::Mutex<crate::fast_hash::FxHashMap<u64, u32>>,
                    hashes: &[u64]| {
            let mut map = map.lock().unwrap_or_else(|e| e.into_inner());
            for hash in hashes {
                let lowest = map.entry(*hash).or_insert(index);
                *lowest = (*lowest).min(index);
            }
        };
        note(&self.computed, &summary.paths);
        note(&self.computed_ancestors, &summary.ancestors);
        let earlier = |map: &std::sync::Mutex<crate::fast_hash::FxHashMap<u64, u32>>,
                       hashes: &[&[u64]]| {
            let map = map.lock().unwrap_or_else(|e| e.into_inner());
            hashes
                .iter()
                .flat_map(|hashes| hashes.iter())
                .any(|hash| map.get(hash).is_some_and(|lowest| *lowest < index))
        };
        earlier(&self.computed, &[&summary.paths, &summary.ancestors])
            || earlier(&self.computed_ancestors, &[&summary.paths])
    }
}

/// The memoized tool runfiles groups of a build, by tree identity (PA-1,
/// ActionPlanning::input_groups).
pub(super) type InputGroups = SingleFlight<u64, ExpandedTree>;

/// The shared input runs of a build's memoized depset walks, by the walk's
/// address ([`SharedInputs`], ActionPlanning::input_memos).
pub(super) type InputMemos = SingleFlight<usize, SharedInputs>;

/// T2 (docs/EVALUATOR.md §8, B1): a read of the artifact index a delta
/// built ahead of its merge depends on — a provisional entry (a published
/// phase-2 result not merged yet: `artifact` with the publishing entry's
/// plan index `stamp`), or a generated File not registered at all
/// (`artifact` `None`). The delta is used at its merge only if every read
/// is what the index holds then ([`check_reads`]); else it is rebuilt there
/// (a contested reader: the shared output's winner is another target).
#[derive(Clone, Debug)]
pub(super) struct ProvisionalRead {
    pub(super) file: File,
    pub(super) artifact: Option<Artifact>,
    pub(super) stamp: u32,
}

/// What [`check_reads`] found.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum ReadsCheck {
    /// Every read is the canonical entry.
    Valid,
    /// A read entry is still provisional: its publisher (plan index) has
    /// not merged (`u32::MAX`: not known).
    Pending(u32),
    /// A read differs from the index now: rebuilt at the merge.
    Stale,
}

/// Check `reads` (of the delta of plan entry `reader`) against `index`.
/// `merging`: at the reader's merge, every earlier entry merged (a read
/// still provisional is then the canonical value, registered by no merge).
pub(super) fn check_reads(
    index: &ConcurrentArtifactIndex,
    reads: &[ProvisionalRead],
    reader: u32,
    merging: bool,
) -> ReadsCheck {
    let mut pending = None;
    for read in reads {
        match &read.artifact {
            // The entry it read, still visible before the reader (a read
            // of an entry stamped at or after the reader — inherited from a
            // memo a later entry filled — is not this reader's view).
            Some(artifact) => match index.current(&read.file) {
                Some((now, provisional, stamp))
                    if stamp < reader && same_artifact(&now, artifact) =>
                {
                    if provisional && !merging {
                        pending = Some(pending.map_or(read.stamp, |at: u32| at.max(read.stamp)));
                    }
                }
                _ => return ReadsCheck::Stale,
            },
            None => {
                // Not registered when it was built: still not visible
                // before the reader, provisional or not (its own
                // registrations are stamped with its index).
                if index
                    .current(&read.file)
                    .is_some_and(|(_, _, stamp)| stamp < reader)
                {
                    return ReadsCheck::Stale;
                }
                if !merging {
                    pending = Some(u32::MAX);
                }
            }
        }
    }
    match pending {
        Some(at) => ReadsCheck::Pending(at),
        None => ReadsCheck::Valid,
    }
}

/// T2: BAZEL_RS_EVAL_STATS / BAZEL_RS_TIMING counters: results published
/// ahead of their merge, deltas whose reads were stale at their merge
/// (rebuilt there), deltas with provisional reads, memo entries refilled,
/// native rules' results published.
pub(super) static PROVISIONAL_STATS: [std::sync::atomic::AtomicU64; 5] =
    [const { std::sync::atomic::AtomicU64::new(0) }; 5];

pub(super) fn provisional_bump(index: usize) {
    PROVISIONAL_STATS[index].fetch_add(1, std::sync::atomic::Ordering::Relaxed);
}

/// A memo a delta used, in the order of its first use (FirstUse).
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum MemoUse {
    /// A tool runfiles group, by tree identity.
    Group(u64),
    /// A shared input run, by the walk's address.
    Inputs(usize),
}

/// One operation of a commit delta, in the order the sequential commit
/// made it ([`Engine::merge_delta`] applies them in that order).
pub(super) enum DeltaOp {
    /// The target's providers and their display names.
    Providers(
        BTreeMap<String, starlark::values::OwnedFrozenValue>,
        Vec<String>,
    ),
    /// An output path registered with its producer (register_output),
    /// with its hashes (computed where the delta is built).
    Output(HashedPath, Arc<OutputProducer>),
    /// An output shared with the equal action that registered it first
    /// (stamped `registered`; plan B S0's shared counter).
    Shared(PathBuf, u32),
    /// A File's artifact registered in the build's index, with the File's
    /// hash there (computed where the delta is built).
    Artifact(File, Artifact, u64),
    /// An output-file entry of `declared` (`replace`: inserted, else kept
    /// when present; `predeclared`: an output of the rule's output-file
    /// targets — the only entries a published result makes visible ahead
    /// of the merge, `PlannedResults::publish_delta`).
    Declared {
        key: DeclaredKey,
        artifact: Artifact,
        replace: bool,
        predeclared: bool,
    },
    RunfilesInfo(Arc<crate::runfiles::TargetRunfiles>),
    RunfilesNodes(Box<RunfilesNodes>),
    /// A planned node with its source inputs.
    Node(Box<PlannedAction>, Option<(String, SourceInputs)>),
    /// A lowering in flight on a lowering worker (coordinator builds only).
    Lowering(lowering::Pending),
    ActionExecution(String, crate::exec_groups::ActionExecution),
    BuildInfoAction(String),
    Dry(Artifacts),
    /// An action whose tool runfiles groups are listed (PERF-GROUPS).
    ListedTreeAction,
}

/// A Starlark target's commit as data ([`Engine::build_delta`]): its
/// operations up to its error, if any.
pub(super) struct TargetDelta {
    pub(super) ops: Vec<DeltaOp>,
    pub(super) error: Option<String>,
    /// Built without the build's producers (on a worker): the merge checks
    /// that none of its output paths meets a producer registered by
    /// another target ([`Engine::delta_meets_producers`]).
    pub(super) unchecked: bool,
    /// The memos it used, in order (FirstUse).
    pub(super) uses: Vec<MemoUse>,
    /// The output path whose registration failed (its error, decided on a
    /// worker against the delta's own registrations only, is checked at
    /// the merge like its registered paths).
    pub(super) failed_output: Option<PathBuf>,
    /// T2: the index reads it depends on (a worker's build,
    /// [`ProvisionalRead`]).
    pub(super) reads: Vec<ProvisionalRead>,
}

/// What a delta build reads of the engine and the planning state: shared
/// with the analysis workers (`Sync`), never written but through interior
/// single-flight memos.
pub(super) struct EngineView<'a> {
    pub(super) semantic_only: bool,
    pub(super) compact_repo_mapping: bool,
    pub(super) repositories: &'a RepositorySet,
    pub(super) artifacts: &'a ConcurrentArtifactIndex,
    /// The plan index the build reads the artifact index at.
    pub(super) reader: u32,
    pub(super) input_groups: &'a InputGroups,
    pub(super) input_memos: &'a InputMemos,
    pub(super) client_environment: &'a ClientEnvironment,
    /// Lowerings go to the lowering workers (a coordinator build in a walk
    /// that defers them), else run in the build.
    pub(super) lowering: Option<&'a lowering::LoweringHandle>,
}

/// An [`EngineView`] is shared with the analysis workers.
const _: () = {
    const fn sync<T: Sync>() {}
    sync::<EngineView<'static>>();
};

/// The owned parts of an [`EngineView`] a worker job holds.
#[derive(Clone)]
pub(super) struct SharedView {
    semantic_only: bool,
    compact_repo_mapping: bool,
    repositories: Arc<RepositorySet>,
    artifacts: Arc<ConcurrentArtifactIndex>,
    input_groups: Arc<InputGroups>,
    input_memos: Arc<InputMemos>,
    client_environment: Arc<ClientEnvironment>,
    registered: Option<Arc<PathHashes>>,
    /// The lowering workers, when the walk defers lowerings (a worker's
    /// delta then holds its lowerings in flight, placed where it merges).
    lowering: Option<lowering::LoweringHandle>,
}

impl SharedView {
    pub(super) fn new(engine: &Engine, state: &ActionPlanning) -> Self {
        Self {
            semantic_only: engine.semantic_only(),
            compact_repo_mapping: engine.compact_repo_mapping,
            repositories: Arc::clone(&engine.repositories),
            artifacts: Arc::clone(&state.artifact_index.shared),
            input_groups: Arc::clone(&state.input_groups),
            input_memos: Arc::clone(&state.input_memos),
            client_environment: Arc::clone(&state.client_environment),
            registered: state.producers.hashes.clone(),
            lowering: engine
                .lowering_pool
                .as_ref()
                // BAZEL_RS_VERIFY_DELTA compares the nodes themselves.
                .filter(|_| state.nodes.deferring && !verify_delta())
                .and_then(lowering::LoweringPool::handle)
                .cloned(),
        }
    }

    /// The view of the target at plan index `reader` (lowering in the build).
    pub(super) fn view(&self, reader: u32) -> EngineView<'_> {
        EngineView {
            semantic_only: self.semantic_only,
            compact_repo_mapping: self.compact_repo_mapping,
            repositories: &self.repositories,
            artifacts: &self.artifacts,
            reader,
            input_groups: &self.input_groups,
            input_memos: &self.input_memos,
            client_environment: &self.client_environment,
            lowering: self.lowering.as_ref(),
        }
    }
}

/// What only the coordinator decides during a build: the workspace status
/// registered on first need (and the build-info actions resolved against
/// it), and the producers the output registrations are checked against.
pub(super) enum Hooks<'a> {
    /// A build off the coordinator: a first-need registration makes the
    /// delta fall back to the coordinator; registrations are checked
    /// against the delta's own only (the merge checks the rest).
    Worker,
    Coordinator {
        engine: &'a Engine,
        index: &'a ArtifactIndex,
        workspace_status: &'a mut Option<WorkspaceStatusPlan>,
        producers: &'a mut Producers,
        status_client_environment: &'a ClientEnvironment,
    },
}

/// The marker error unwinding a build that falls back to the coordinator
/// (never reported; [`DeltaBuilder::finish`] returns the reason).
const FALLBACK: &str = "\u{1}delta-fallback";

/// The REAPI platform of each spawn a target's commit plans
/// (Engine::remote_platform), decided before its build: for each entry of
/// its execution table, then without one (the configuration's host
/// platform). An entry is `final` unless its execution platform was not
/// analyzed yet (a later analysis may resolve it before the target's turn:
/// a worker's build then falls back).
pub(super) struct RemotePlatforms {
    entries: Vec<(Result<crate::remote_execution::SpawnPlatform, String>, bool)>,
    host: (Result<crate::remote_execution::SpawnPlatform, String>, bool),
}

impl Engine {
    /// [`RemotePlatforms`] of `rule`'s spawns in `config`.
    pub(super) fn remote_platforms(
        &self,
        rule: &Rule,
        config: ConfigId,
        action_execution: &[crate::exec_groups::ActionExecution],
    ) -> RemotePlatforms {
        let decided = |execution: Option<&crate::exec_groups::ActionExecution>| {
            let host = &self.configurations.get(config).native.host_platform;
            let label = execution.map_or(host, |execution| &execution.platform);
            let known = *label == crate::configuration::default_host_platform()
                || execution.is_some_and(|execution| execution.platform_properties.is_some())
                || self
                    .platforms
                    .contains_key(&ConfiguredTargetKey::new(label.clone(), config));
            (self.remote_platform(rule, config, execution, true), known)
        };
        RemotePlatforms {
            entries: action_execution
                .iter()
                .map(|execution| decided(Some(execution)))
                .collect(),
            host: decided(None),
        }
    }
}

impl RemotePlatforms {
    /// No spawn's platform decided (a worker's delta is merged as built).
    pub(super) fn undecided() -> Self {
        let unknown = || (Err(String::new()), false);
        Self {
            entries: Vec::new(),
            host: unknown(),
        }
    }

    /// Whether every spawn of `actions` has its platform decided.
    pub(super) fn all_final(
        &self,
        action_execution: &[crate::exec_groups::ActionExecution],
        actions: &[crate::analysis_runtime::RegisteredAction],
    ) -> bool {
        actions.iter().all(|registered| {
            let action = &registered.action;
            action.write.is_some()
                || action.extra.transform.is_some()
                || self.of(action_execution, &registered.exec_group).1
        })
    }

    fn of(
        &self,
        action_execution: &[crate::exec_groups::ActionExecution],
        exec_group: &str,
    ) -> &(Result<crate::remote_execution::SpawnPlatform, String>, bool) {
        match action_execution
            .iter()
            .position(|execution| execution.exec_group == exec_group)
        {
            Some(position) => &self.entries[position],
            None => &self.host,
        }
    }
}

/// A Starlark target's commit after its prefix (prints, the
/// implementation's error, restricted_to, the transition allowlist), as
/// [`DeltaBuilder::custom_target`] builds it.
pub(super) struct CustomCommit<'r> {
    pub(super) key: &'r ConfiguredTargetKey,
    pub(super) rule: &'r Rule,
    pub(super) output_root: &'r str,
    pub(super) action_owner: &'r str,
    pub(super) main_ignored: &'r Arc<IgnoredSubdirectories>,
    pub(super) transitive: &'r BTreeSet<String>,
    pub(super) action_execution: &'r [crate::exec_groups::ActionExecution],
    pub(super) platforms: &'r RemotePlatforms,
    pub(super) runfiles_options: &'r crate::runfiles_tree::RunfilesOptions,
    pub(super) analyzed: analysis::AnalysisResult,
    pub(super) precomputed: Option<RunfilesPrecomputed>,
    pub(super) outputs_checked: Option<Result<(), String>>,
}

/// The producers a build's registrations are checked against: the build's
/// own, over the build's producers on the coordinator.
struct Combined<'b> {
    base: Option<&'b Producers>,
    local: &'b Producers,
    stamp: u32,
}

impl ProducerLookup for Combined<'_> {
    fn indexed(&self, output: &Path) -> Option<(&Arc<OutputProducer>, u32)> {
        self.local
            .get_indexed(output)
            .or_else(|| self.base.and_then(|base| base.get_indexed(output)))
    }

    fn below(&self, output: &Path) -> Option<(&PathBuf, &Arc<OutputProducer>, u32)> {
        let local = self.local.first_below(output);
        let base = self.base.and_then(|base| base.first_below(output));
        match (local, base) {
            (Some(local), Some(base)) => Some(if local.0 <= base.0 { local } else { base }),
            (local, base) => local.or(base),
        }
    }

    fn registering(&self) -> u32 {
        self.stamp
    }
}

/// Builds one commit delta ([`TargetDelta`]): the sequential commit's code,
/// writing operations instead of the planning state.
pub(super) struct DeltaBuilder<'a> {
    view: EngineView<'a>,
    hooks: Hooks<'a>,
    ops: Vec<DeltaOp>,
    /// What the build registered, visible to its own later lookups, and
    /// those Files' quick keys (most lookups are of other Files).
    index: crate::fast_hash::FxHashMap<File, Artifact>,
    keys: crate::fast_hash::FxHashSet<u64>,
    producers: Producers,
    uses: Vec<MemoUse>,
    fallback: Option<&'static str>,
    spent: Vec<Vec<File>>,
    /// The output path whose registration failed, if any.
    failed_output: Option<PathBuf>,
    /// T2: the index reads the build depends on (a worker's build only).
    reads: std::cell::RefCell<Vec<ProvisionalRead>>,
    /// T2: the memo fills in progress, each with whether it resolved a
    /// File through this build's own registrations (unpublished, recording no
    /// read: such a value is not kept for other users, [`Self::unshared`]).
    filling: std::cell::RefCell<Vec<bool>>,
}

impl<'a> DeltaBuilder<'a> {
    /// A build reading `view`, registering as plan index `stamp`.
    pub(super) fn new(view: EngineView<'a>, hooks: Hooks<'a>, stamp: u32) -> Self {
        Self {
            view,
            hooks,
            ops: Vec::new(),
            index: Default::default(),
            keys: Default::default(),
            producers: Producers::local(stamp),
            uses: Vec::new(),
            fallback: None,
            spent: Vec::new(),
            failed_output: None,
            reads: Default::default(),
            filling: Default::default(),
        }
    }

    /// The delta with `result`'s error last; `Err`: the reason the build
    /// falls back to the coordinator.
    pub(super) fn finish(self, result: Result<(), String>) -> Result<TargetDelta, &'static str> {
        if let Some(reason) = self.fallback {
            return Err(reason);
        }
        drop_elsewhere(self.spent);
        // A File it looked up before registering it itself is its own.
        let mut reads = self.reads.into_inner();
        reads.retain(|read| read.artifact.is_some() || !self.index.contains_key(&read.file));
        Ok(TargetDelta {
            unchecked: matches!(self.hooks, Hooks::Worker),
            ops: self.ops,
            error: result.err(),
            uses: self.uses,
            failed_output: self.failed_output,
            reads,
        })
    }

    /// The operations so far (a coordinator build merged at once).
    pub(super) fn take_ops(&mut self) -> Vec<DeltaOp> {
        std::mem::take(&mut self.ops)
    }

    /// Fall back to the coordinator for `reason`.
    fn fall_back(&mut self, reason: &'static str) -> String {
        self.fallback.get_or_insert(reason);
        FALLBACK.to_owned()
    }

    fn note_use(&mut self, used: MemoUse) {
        if !self.uses.contains(&used) {
            self.uses.push(used);
        }
    }

    /// The artifact of `file` visible to this build.
    fn lookup(&self, file: &File) -> Option<Artifact> {
        if !self.keys.is_empty()
            && self.keys.contains(&quick_key(file))
            && let Some(artifact) = self.index.get(file)
        {
            // Every fill in progress depends on it (an outer one holds
            // the inner's value).
            for own in self.filling.borrow_mut().iter_mut() {
                *own = true;
            }
            return Some(artifact.clone());
        }
        if !matches!(self.hooks, Hooks::Worker) {
            return self.view.artifacts.get(file, self.view.reader);
        }
        // T2: what a worker's build reads ahead of its merge.
        match self.view.artifacts.get_entry(file, self.view.reader) {
            Some((artifact, None)) => Some(artifact),
            Some((artifact, Some(stamp))) => {
                self.reads.borrow_mut().push(ProvisionalRead {
                    file: file.clone(),
                    artifact: Some(artifact.clone()),
                    stamp,
                });
                Some(artifact)
            }
            None => {
                // A source File too: an earlier entry (a native rule's
                // inputs, published or merged) may register it before this
                // one's merge, where the build read it as missing.
                self.reads.borrow_mut().push(ProvisionalRead {
                    file: file.clone(),
                    artifact: None,
                    stamp: u32::MAX,
                });
                None
            }
        }
    }

    /// The reads recorded so far (a memo fill's start).
    fn reads_mark(&self) -> usize {
        self.filling.borrow_mut().push(false);
        self.reads.borrow().len()
    }

    /// A memo fill ends: whether it resolved a File through this build's
    /// own registrations, which no other user sees (its value is then
    /// dropped from the memo after this use: [`Self::reads_mark`]).
    fn fill_done(&self) -> bool {
        let own = self.filling.borrow_mut().pop().unwrap_or(false);
        // A coordinator's build merges at once: its registrations are the
        // canonical ones every later user sees.
        own && matches!(self.hooks, Hooks::Worker)
    }

    /// The reads recorded since `mark` (a memo fill's own).
    fn reads_since(&self, mark: usize) -> Box<[ProvisionalRead]> {
        self.reads.borrow()[mark..].to_vec().into_boxed_slice()
    }

    /// T2: a memo value filled from reads that are stale now (a
    /// provisional result replaced at its merge) is dropped and filled
    /// again; `reads` of a value used are this build's reads too.
    fn memo_reads<K: std::hash::Hash + Eq + Clone, V>(
        &self,
        memo: &SingleFlight<K, V>,
        key: &K,
        reads: impl Fn(&V) -> &[ProvisionalRead],
    ) {
        if let Some(value) = memo.peek(key)
            && !reads(&value).is_empty()
            && check_reads(self.view.artifacts, reads(&value), self.view.reader, true)
                == ReadsCheck::Stale
        {
            memo.remove(key);
            provisional_bump(3);
        }
    }

    /// The reads of a memo value used (filled here or before).
    fn note_memo_reads(&self, filled_here: bool, reads: &[ProvisionalRead]) {
        if !filled_here && !reads.is_empty() && matches!(self.hooks, Hooks::Worker) {
            self.reads.borrow_mut().extend(reads.iter().cloned());
        }
    }

    /// [`Engine::indexed_artifact`]: the artifact of `file`, a workspace
    /// status file registered on first need.
    fn artifact(&mut self, file: &File) -> Result<Option<Artifact>, String> {
        if let Some(artifact) = self.lookup(file) {
            return Ok(Some(artifact));
        }
        if !crate::analysis_runtime::is_workspace_status_file(file) {
            return Ok(None);
        }
        self.ensure_status()?;
        Ok(self.lookup(file))
    }

    /// [`register_artifact`] in this build.
    fn register_artifact(&mut self, file: &File, artifact: &Artifact) {
        match self.lookup(file) {
            Some(registered) => {
                if (cfg!(debug_assertions) || verify_artifacts())
                    && !same_artifact(&registered, artifact)
                {
                    panic!(
                        "internal error: File {} re-registered with a different artifact: registered {registered:?}, now {artifact:?}",
                        file.path
                    );
                }
            }
            None => {
                self.keys.insert(quick_key(file));
                self.index.insert(file.clone(), artifact.clone());
                self.ops.push(DeltaOp::Artifact(
                    file.clone(),
                    artifact.clone(),
                    file_hash(file),
                ));
            }
        }
    }

    /// [`register_output`] in this build.
    fn register_output(
        &mut self,
        output: &Path,
        producer: &Arc<OutputProducer>,
    ) -> Result<Option<String>, String> {
        let base = match &self.hooks {
            Hooks::Coordinator { producers, .. } => Some(&**producers),
            Hooks::Worker => None,
        };
        let combined = Combined {
            base,
            local: &self.producers,
            stamp: self.producers.stamp,
        };
        let checked = check_output(&combined, output, producer).inspect_err(|_| {
            if order_audit::enabled() && matches!(self.hooks, Hooks::Coordinator { .. }) {
                order_audit::note_conflict();
            }
            // Its error names what it met: on a worker the build's own
            // registrations only, so the merge checks it against the rest.
            self.failed_output
                .get_or_insert_with(|| output.to_path_buf());
        });
        match checked? {
            OutputRegistration::New => {
                let hashed = HashedPath::new(output.to_path_buf());
                self.ops
                    .push(DeltaOp::Output(hashed.clone(), Arc::clone(producer)));
                self.producers.insert_hashed(hashed, Arc::clone(producer));
                Ok(None)
            }
            OutputRegistration::Shared(id, registered) => {
                self.ops
                    .push(DeltaOp::Shared(output.to_path_buf(), registered));
                Ok(Some(id))
            }
        }
    }

    /// The workspace status registered on first need
    /// ([`Engine::ensure_workspace_status`], its nodes in this delta).
    fn ensure_status(&mut self) -> Result<[Artifact; 2], String> {
        let Self { hooks, ops, .. } = self;
        match hooks {
            Hooks::Worker => Err(self.fall_back("workspace_status")),
            Hooks::Coordinator {
                engine,
                index,
                workspace_status,
                producers,
                status_client_environment,
            } => engine.ensure_workspace_status_with(
                workspace_status,
                &mut |node| ops.push(DeltaOp::Node(Box::new(node), None)),
                producers,
                status_client_environment,
                index,
            ),
        }
    }

    /// [`Engine::resolve_build_info`] against the status planned.
    fn resolve_build_info(
        &mut self,
        registered: &mut crate::analysis_runtime::RegisteredAction,
    ) -> Result<(), String> {
        if registered.build_info.is_none() {
            return Ok(());
        }
        match &self.hooks {
            Hooks::Worker => Err(self.fall_back("build_info")),
            Hooks::Coordinator {
                engine,
                workspace_status,
                ..
            } => engine.resolve_build_info(workspace_status, registered),
        }
    }

    /// Lower `job` on a lowering worker, or here.
    fn lower(
        &mut self,
        job: impl FnOnce() -> Result<lowering::Lowered, String> + Send + 'static,
    ) -> Result<(), String> {
        let job = match self.view.lowering {
            Some(pool) => match pool.submit(job) {
                Ok(pending) => {
                    self.ops.push(DeltaOp::Lowering(pending));
                    return Ok(());
                }
                Err(job) => job,
            },
            None => Box::new(job),
        };
        let lowered = job()?;
        self.place(lowered)
    }

    /// Place a lowering's product at its position.
    pub(super) fn place(&mut self, lowered: lowering::Lowered) -> Result<(), String> {
        match lowered {
            lowering::Lowered::Node { planned, sources } => {
                self.ops.push(DeltaOp::Node(planned, sources));
                Ok(())
            }
            lowering::Lowered::Listing(listing) => self.commit_listed_action(*listing),
        }
    }

    /// The shared inputs of a memoized depset walk ([`SharedInputs`]).
    fn shared_inputs(
        &mut self,
        files: &Arc<crate::starlark_types::FlatFiles>,
    ) -> Option<Arc<SharedInputs>> {
        let address = Arc::as_ptr(files) as usize;
        self.note_use(MemoUse::Inputs(address));
        let memo: &InputMemos = self.view.input_memos;
        self.memo_reads(memo, &address, |value| &value.reads);
        let mut filled_here = false;
        let mut unshared = false;
        let value = memo
            .get_or_fill(&address, || {
                filled_here = true;
                let mark = self.reads_mark();
                let mut filled = SharedInputs::of(&|file| self.lookup(file), files);
                unshared = self.fill_done();
                if let Some(filled) = filled.as_mut() {
                    filled.reads = self.reads_since(mark);
                }
                Ok::<_, std::convert::Infallible>(filled)
            })
            .unwrap_or_else(|never| match never {});
        if unshared {
            memo.remove(&address);
        }
        if let Some(value) = &value {
            self.note_memo_reads(filled_here, &value.reads);
        }
        value
    }

    /// The node of one registered Starlark action ([`Engine::commit_action`]
    /// before S1): a rule's, an aspect application's or a native rule's
    /// action, with its own inputs `own` (with `shared`, the shared run
    /// joins them at its start where the action is lowered), its tools'
    /// runfiles trees and its execution-time completion `deferred`. `owner`
    /// names the registering target or aspect in the error of a tree entry
    /// without an artifact.
    ///
    /// N23f: the tools' runfiles trees are staged at their tree paths
    /// (SpawnInputExpander.addSingleRunfilesTreeToInputs,
    /// exec/SpawnInputExpander.java:87-135); empty files are empty
    /// parameter-file-like inputs (VirtualActionInput.EMPTY_MARKER). PA-1:
    /// each tree is expanded once per build into a shared input group that
    /// every action using the tool holds, like the one runfiles tree
    /// artifact Bazel's spawns hold; a tree listed twice is one group.
    #[allow(clippy::too_many_arguments)]
    pub(super) fn commit_action(
        &mut self,
        action: Action,
        own: Vec<Artifact>,
        runfiles_trees: &[Arc<crate::analysis_runtime::ToolRunfilesTree>],
        deferred: Option<scheduler::Deferred>,
        owner: &str,
        main_ignored: &Arc<IgnoredSubdirectories>,
        shared: Option<(usize, &Arc<SharedInputs>)>,
    ) -> Result<(), String> {
        if self.view.semantic_only {
            return Ok(());
        }
        let mut groups: Vec<Arc<scheduler::InputGroup>> = Vec::new();
        for tree in runfiles_trees {
            self.note_use(MemoUse::Group(tree.identity()));
            let memo: &InputGroups = self.view.input_groups;
            self.memo_reads(memo, &tree.identity(), |value| &value.reads);
            let mut filled_here = false;
            let mut unshared = false;
            let expanded = memo
                .get_or_fill(&tree.identity(), || {
                    filled_here = true;
                    let mark = self.reads_mark();
                    let entries =
                        runfiles_tree_entries(tree, owner, &mut |file| self.artifact(file));
                    unshared = self.fill_done();
                    let (artifacts, empty_files) = entries?;
                    Ok::<_, String>(Some(ExpandedTree {
                        tree: Arc::clone(tree),
                        group: Arc::new(runfiles_input_group(
                            Path::new(tree.directory()),
                            artifacts,
                            empty_files,
                            main_ignored,
                        )),
                        reads: self.reads_since(mark),
                    }))
                })?
                .expect("a runfiles group fill returns one");
            if unshared {
                memo.remove(&tree.identity());
            }
            self.note_memo_reads(filled_here, &expanded.reads);
            // One identity is one content
            // (crate::analysis_runtime::ToolRunfilesTree).
            debug_assert!(
                Arc::ptr_eq(&expanded.tree, tree) || expanded.tree.same_content(tree),
                "internal error: runfiles tree {} of identity {} differs from the one expanded",
                tree.directory(),
                tree.identity()
            );
            let group = Arc::clone(&expanded.group);
            if !groups.iter().any(|known| Arc::ptr_eq(known, &group)) {
                groups.push(group);
            }
        }
        // The rest is a function of what the job holds (P4: deferred to a
        // lowering worker in a pipelined walk, placed in commit order).
        let environment = (!action.extra.inherited_environment.is_empty()).then(|| {
            Arc::clone(
                self.view
                    .client_environment
                    .get_or_init(|| Arc::new(crate::action_env::client_environment())),
            )
        });
        let listing = lowering::Listing {
            action,
            own,
            runfiles_trees: runfiles_trees.to_vec(),
            deferred,
            owner: owner.to_owned(),
            main_ignored: Arc::clone(main_ignored),
            shared: shared.map(|(start, shared)| (start, Arc::clone(shared))),
        };
        self.lower(move || lower_action(listing, groups, environment))
    }

    /// The listed path of [`Self::commit_action`]: the tool runfiles groups
    /// could not stay shared, so their entries are listed in the action.
    fn commit_listed_action(&mut self, listing: lowering::Listing) -> Result<(), String> {
        let lowering::Listing {
            mut action,
            own,
            runfiles_trees,
            deferred,
            owner,
            main_ignored,
            shared,
        } = listing;
        self.ops.push(DeltaOp::ListedTreeAction);
        let mut inputs = own;
        for tree in &runfiles_trees {
            let (artifacts, empty_files) =
                runfiles_tree_entries(tree, &owner, &mut |file| self.artifact(file))?;
            inputs.extend(artifacts);
            for destination in empty_files {
                if !action
                    .extra
                    .param_files
                    .iter()
                    .any(|(existing, _)| *existing == destination)
                {
                    action.extra.param_files.push((destination, Vec::new()));
                }
            }
        }
        let id = action.id.clone();
        let empties: Vec<(PathBuf, Vec<u8>)> = action
            .extra
            .param_files
            .iter()
            .filter(|(path, content)| {
                content.is_empty()
                    && runfiles_trees
                        .iter()
                        .any(|tree| path.starts_with(tree.directory()))
            })
            .cloned()
            .collect();
        let (mut planned, sources) = plan_action_sources(
            action,
            &inputs,
            &main_ignored,
            self.view.client_environment,
            false,
            Vec::new(),
            shared.as_ref().map(|(start, shared)| (*start, &**shared)),
        )?;
        // A deferred command line recomputes the parameter files;
        // the runfiles empty files are kept.
        planned.deferred = match (deferred, empties.is_empty()) {
            (Some(deferred), false) => Some(deferred.with_extra_param_files(empties)),
            (deferred, _) => deferred,
        };
        self.ops
            .push(DeltaOp::Node(Box::new(planned), Some((id, sources))));
        Ok(())
    }

    /// [`register_action_outputs`] in this build: every action's outputs
    /// registered and indexed; per action, whether it is shared.
    pub(super) fn action_outputs(
        &mut self,
        actions: &[crate::analysis_runtime::RegisteredAction],
        (owner, label): (&str, &str),
        config: ConfigId,
        predeclared: impl Fn(&Path) -> bool,
    ) -> Result<Vec<bool>, String> {
        let mut shared = Vec::with_capacity(actions.len());
        for registered in actions {
            let producer = OutputProducer::action(owner, label, registered);
            let mut first = None;
            for (index, file) in registered.outputs.iter().enumerate() {
                let exec = PathBuf::from(&file.path);
                let sharing = self.register_output(&exec, &producer)?;
                if index > 0 && sharing != first {
                    return Err(partial_share_error(
                        registered, label, file, &sharing, &first,
                    ));
                }
                first = first.or(sharing);
                let artifact = output_artifact(registered, file, &exec, &first, index);
                verify_declared_file(file, &artifact);
                self.register_artifact(file, &artifact);
                let predeclared = predeclared(&exec);
                let replace = first.is_none() || predeclared;
                self.ops.push(DeltaOp::Declared {
                    key: DeclaredKey::new(config, exec),
                    artifact,
                    replace,
                    predeclared,
                });
            }
            shared.push(first.is_some());
        }
        Ok(shared)
    }

    /// Register the only output of a native runfiles node.
    fn runfiles_output(
        &mut self,
        key: &ConfiguredTargetKey,
        id: &str,
        file: &File,
    ) -> Result<Artifact, String> {
        let exec = PathBuf::from(&file.path);
        self.register_output(&exec, &OutputProducer::native(&key.label.to_string()))?;
        let artifact = Artifact::new(
            exec.clone(),
            exec.clone(),
            Some((id.to_owned(), 0)),
            file.owner.clone(),
            false,
            false,
        );
        self.register_artifact(file, &artifact);
        self.ops.push(DeltaOp::Declared {
            key: DeclaredKey::new(key.config, exec),
            artifact: artifact.clone(),
            replace: true,
            predeclared: false,
        });
        Ok(artifact)
    }

    /// RunfilesSupport.create (analysis/RunfilesSupport.java:280-347) for a
    /// target with runfiles support: RepoMappingManifestAction (a native
    /// write), SourceManifestAction (Transform::RunfilesManifest, with
    /// --build_runfile_manifests) and SymlinkTreeAction/RunfilesTreeAction
    /// (Transform::RunfilesTree). The tree node is always planned (`run`
    /// creates the tree without --build_runfile_links, RunCommand.java:996-1040);
    /// only reachable nodes execute.
    #[allow(clippy::too_many_arguments)]
    fn runfiles_support(
        &mut self,
        key: &ConfiguredTargetKey,
        rule: &Rule,
        target: &crate::runfiles::TargetRunfiles,
        precomputed: Option<RunfilesPrecomputed>,
        transitive: &BTreeSet<String>,
        action_owner: &str,
        main_ignored: &Arc<IgnoredSubdirectories>,
        options: &crate::runfiles_tree::RunfilesOptions,
    ) -> Result<Option<RunfilesNodes>, String> {
        let Some(files_to_run) = &target.files_to_run else {
            return Ok(None);
        };
        let (Some(runfiles), Some(executable), Some(repo_mapping)) = (
            files_to_run.runfiles(),
            files_to_run.executable(),
            files_to_run.repo_mapping_manifest(),
        ) else {
            return Ok(None);
        };
        let lookup = |builder: &mut Self, file: &File| -> Result<Artifact, String> {
            builder.artifact(file)?.ok_or_else(|| {
                format!(
                    "{}: runfiles file has no declared artifact identity: {}",
                    rule.label, file.path
                )
            })
        };
        let executable_artifact = lookup(self, executable)?;
        // The pure part: precomputed by the analysis worker, else here.
        let precomputed = match precomputed {
            Some(precomputed) => precomputed,
            None => {
                RunfilesPrecomputed::compute(target, transitive, self.view.repositories, options)
                    .ok_or_else(|| "internal error: runfiles support without runfiles".to_owned())?
            }
        };
        // RepoMappingManifestAction.
        let content = precomputed.content?;
        let id = format!("{action_owner}#repo_mapping");
        self.ops.push(DeltaOp::Node(
            Box::new(PlannedAction {
                action: Action {
                    id: id.clone(),
                    command: String::new(),
                    inputs: Vec::new(),
                    outputs: vec![PathBuf::from(&repo_mapping.path)],
                    write: Some(crate::action::WriteAction {
                        content,
                        executable: false,
                    }),
                    program: None,
                    arguments: Vec::new(),
                    environment: BTreeMap::new(),
                    shell_strict: false,
                    extra: crate::action::ActionExtra::default(),
                    ..Action::default()
                },
                dependencies: Vec::new(),
                deferred: None,
                groups: Vec::new(),
            }),
            None,
        ));
        let repo_mapping_artifact = self.runfiles_output(key, &id, repo_mapping)?;
        // The tree mapping; conflicts are reported by the manifest action.
        let conflicts = precomputed.conflicts;
        let mapping = precomputed.mapping;
        let mut inputs: Vec<Artifact> = Vec::new();
        // One copy of each runfiles path, shared by the manifest and tree
        // actions' entries and the target's runfiles entries (MEM4).
        let mut entries: Vec<(Arc<str>, Option<SharedPath>)> = Vec::new();
        let mut mapped: Vec<(Arc<str>, Option<Artifact>)> = Vec::new();
        for (path, file) in mapping {
            let path: Arc<str> = path.into();
            match file {
                Some(file) => {
                    let artifact = if file == *repo_mapping {
                        repo_mapping_artifact.clone()
                    } else {
                        lookup(self, &file)?
                    };
                    entries.push((path.clone(), Some(artifact.exec.clone())));
                    mapped.push((path, Some(artifact.clone())));
                    inputs.push(artifact);
                }
                None => {
                    entries.push((path.clone(), None));
                    mapped.push((path, None));
                }
            }
        }
        let entries: crate::action::RunfilesEntries = entries.into();
        let inputs = Arc::new(inputs);
        let action = |id: &str, outputs: Vec<PathBuf>, transform| Action {
            id: id.to_owned(),
            command: String::new(),
            inputs: Vec::new(),
            outputs,
            write: None,
            program: None,
            arguments: Vec::new(),
            environment: BTreeMap::new(),
            shell_strict: false,
            extra: crate::action::ActionExtra {
                transform: Some(transform),
                ..crate::action::ActionExtra::default()
            },
            ..Action::default()
        };
        // SourceManifestAction: the input manifest.
        let input_manifest = suffixed_file(executable, ".runfiles_manifest");
        let manifest_artifact = if options.build_runfile_manifests {
            let id = format!("{action_owner}#runfiles_manifest");
            let manifest = action(
                &id,
                vec![PathBuf::from(&input_manifest.path)],
                crate::action::Transform::RunfilesManifest {
                    entries: entries.clone(),
                },
            );
            let (inputs, ignored) = (Arc::clone(&inputs), Arc::clone(main_ignored));
            self.lower(move || lower_native_action(manifest, &inputs, &ignored))?;
            Some(self.runfiles_output(key, &id, &input_manifest)?)
        } else {
            None
        };
        // SymlinkTreeAction + RunfilesTreeAction. Without manifests (`run`
        // then fails, RunCommand.java:503-513) the input manifest is still
        // needed by the tree node, so the tree is only planned with them.
        let tree_file = suffixed_file(executable, ".runfiles/MANIFEST");
        let tree_id = format!("{action_owner}#runfiles_tree");
        let tree = match &manifest_artifact {
            Some(manifest) => {
                let mut tree_inputs = Vec::with_capacity(inputs.len() + 2);
                tree_inputs.extend(inputs.iter().cloned());
                tree_inputs.push(manifest.clone());
                tree_inputs.push(executable_artifact.clone());
                let tree = action(
                    &tree_id,
                    vec![PathBuf::from(&tree_file.path)],
                    crate::action::Transform::RunfilesTree {
                        entries,
                        manifest: manifest.exec.to_path_buf(),
                        executable: Some(executable_artifact.exec.to_path_buf()),
                        create: options.enable_runfiles,
                    },
                );
                let ignored = Arc::clone(main_ignored);
                self.lower(move || lower_native_action(tree, &tree_inputs, &ignored))?;
                Some(self.runfiles_output(key, &tree_id, &tree_file)?)
            }
            None => None,
        };
        Ok(Some(RunfilesNodes {
            tree,
            manifest: manifest_artifact,
            repo_mapping: repo_mapping_artifact,
            executable: executable_artifact,
            entries: mapped,
            conflicts,
            conflict_error: runfiles.conflict_error(),
            location: rule.location.clone(),
        }))
    }

    /// The commit of one Starlark rule after its prefix
    /// ([`Engine::commit_custom_target`]): every planning-state write of the
    /// analyzed target, as operations.
    pub(super) fn custom_target(&mut self, commit: CustomCommit<'_>) -> Result<(), String> {
        let CustomCommit {
            key,
            rule,
            output_root,
            action_owner,
            main_ignored,
            transitive,
            action_execution,
            platforms,
            runfiles_options,
            mut analyzed,
            precomputed,
            outputs_checked,
        } = commit;
        let config = key.config;
        // Moved, not copied: the analysis result is consumed here (MEM2).
        self.ops.push(DeltaOp::Providers(
            std::mem::take(&mut analyzed.providers),
            std::mem::take(&mut analyzed.provider_keys),
        ));
        let repositories = self.view.repositories;
        let check_here = || {
            validate_declared_outputs(&rule.label, output_root, &analyzed.declared, &|| {
                let repository = &rule.label.repository;
                Ok((
                    repositories.root(repository)?.to_path_buf(),
                    repositories.ignored_subdirectories(repository)?,
                ))
            })
        };
        match outputs_checked {
            Some(checked) => {
                // Debug builds (the tests): the worker's check is this one.
                debug_assert_eq!(
                    checked,
                    check_here(),
                    "internal error: the outputs of {} checked on a worker differ",
                    rule.label
                );
                checked?
            }
            None => check_here()?,
        }
        let package_output = PathBuf::from(output_root).join(rule.label.package_exec_path());
        let predeclared = |exec: &Path| {
            exec.strip_prefix(&package_output)
                .is_ok_and(|name| rule.outs.iter().any(|out| Path::new(out) == name))
        };
        // Semantic commit is distinct from execution-plan lowering. Cquery
        // needs providers, file identities and runfiles, but never action keys,
        // status commands, input digests, runfiles manifests or scheduler nodes.
        if self.view.semantic_only {
            let outputs: Vec<File> = analyzed
                .actions
                .iter()
                .flat_map(|action| action.outputs.iter().cloned())
                .collect();
            self.semantic_outputs(&outputs, config, &|file| predeclared(Path::new(&file.path)));
            // FilesToRunProvider exposes these semantic Files even though no
            // manifest/tree/mapping action is lowered or executed by cquery.
            // Their actual Option values already honor the runfiles options.
            if let Some(support) = &analyzed.runfiles.files_to_run {
                let files: Vec<File> = support
                    .runfiles_manifest()
                    .into_iter()
                    .chain(support.repo_mapping_manifest())
                    .cloned()
                    .collect();
                self.semantic_outputs(&files, config, &|_| false);
            }
            for file in &analyzed.files {
                if crate::analysis_runtime::is_workspace_status_file(file) {
                    self.artifact(file)?;
                }
            }
            let defaults = self.defaults(rule, &analyzed.files)?;
            self.ops.push(DeltaOp::Dry(Arc::new(defaults)));
            self.ops
                .push(DeltaOp::RunfilesInfo(Arc::new(analyzed.runfiles)));
            return Ok(());
        }
        // N63: plan the workspace status system nodes before indexing this
        // target's outputs, so actions consuming bazel-out/{stable,volatile}
        // -status.txt (or a transform_{version,info}_file registration)
        // resolve their artifacts, and so does a status file among the
        // target's files to build (W1b: whichever target registered the
        // status files first, PLAN-L1 §2.1).
        if Engine::workspace_status_needed(&analyzed.actions)
            || analyzed
                .files
                .iter()
                .any(crate::analysis_runtime::is_workspace_status_file)
        {
            self.ensure_status()?;
        }
        // First index EVERY action's outputs, so registration order is not
        // an execution order and forward generated dependencies are legal.
        let label = key.label.to_string();
        let shared =
            self.action_outputs(&analyzed.actions, (&label, &label), config, predeclared)?;
        let runfiles = Arc::new(std::mem::take(&mut analyzed.runfiles));
        self.ops.push(DeltaOp::RunfilesInfo(runfiles.clone()));
        if let Some(nodes) = self.runfiles_support(
            key,
            rule,
            &runfiles,
            precomputed,
            transitive,
            action_owner,
            main_ignored,
            runfiles_options,
        )? {
            self.ops.push(DeltaOp::RunfilesNodes(Box::new(nodes)));
        }
        let owner = rule.label.to_string();
        for (mut registered, shared) in std::mem::take(&mut analyzed.actions)
            .into_iter()
            .zip(shared)
        {
            // Another owner's equal action produces its outputs: no node,
            // no source inputs, no execution of its own.
            if shared {
                continue;
            }
            // py_internal.create_repo_mapping_manifest: the
            // RepoMappingManifestAction's content over the target's
            // transitive repositories (like its runfiles support's).
            resolve_repo_mapping(
                self.view.repositories,
                self.view.compact_repo_mapping,
                transitive,
                &mut registered,
            )?;
            // N63: resolve an actions.transform_{version,info}_file
            // registration into its template expansion.
            if registered.build_info.is_some() {
                self.ops
                    .push(DeltaOp::BuildInfoAction(registered.action.id.clone()));
            }
            self.resolve_build_info(&mut registered)?;
            // Inputs beginning with a memoized depset walk (the Python
            // runtime's files of every PyCompile action) reuse its
            // artifacts, resolved once per build (SharedInputs).
            let shared_inputs = match registered.input_memo.as_ref() {
                Some((start, files)) => {
                    let start = *start;
                    let files = Arc::clone(files);
                    self.shared_inputs(&files).map(|shared| (start, shared))
                }
                None => None,
            };
            let resolve = |builder: &Self, file: &File| {
                builder.lookup(file).ok_or_else(|| {
                    format!(
                        "{}: input has no declared artifact: {}",
                        rule.label, file.path
                    )
                })
            };
            let own = match &shared_inputs {
                Some((start, shared)) => {
                    let end = start + shared.artifacts.len();
                    // The action's own inputs only: the shared run joins
                    // them at `start` where the action is lowered
                    // (lower_action), not here.
                    let mut own =
                        Vec::with_capacity(registered.inputs.len() - shared.artifacts.len());
                    for file in &registered.inputs[..*start] {
                        own.push(resolve(self, file)?);
                    }
                    for file in &registered.inputs[end..] {
                        own.push(resolve(self, file)?);
                    }
                    debug_assert!(
                        with_shared_inputs(&own, Some((*start, shared)))
                            .iter()
                            .zip(&registered.inputs)
                            .all(|(artifact, file)| self
                                .lookup(file)
                                .is_some_and(|indexed| Arc::ptr_eq(&indexed.0, &artifact.0))),
                        "internal error: the shared inputs of {} resolve differently",
                        registered.action.id
                    );
                    own
                }
                None => registered
                    .inputs
                    .iter()
                    .map(|file| resolve(self, file))
                    .collect::<Result<Vec<_>, _>>()?,
            };
            // The action's exec group decides its execution platform and
            // exec properties (RuleContext.getActionOwner(execGroup)).
            if let Some(execution) = action_execution
                .iter()
                .find(|execution| execution.exec_group == registered.exec_group)
            {
                self.ops.push(DeltaOp::ActionExecution(
                    registered.action.id.clone(),
                    execution.clone(),
                ));
            }
            self.spent.push(std::mem::take(&mut registered.inputs));
            let mut action = registered.action;
            // The spawn's propagated tags are set at registration
            // (crate::analysis_runtime::Context::register_action, keyed).
            if action.write.is_none() && action.extra.transform.is_none() {
                let (platform, known) = platforms.of(action_execution, &registered.exec_group);
                if !known && matches!(self.hooks, Hooks::Worker) {
                    return Err(self.fall_back("remote_platform"));
                }
                match platform {
                    Ok(platform) => {
                        action.remote_platform = platform.clone();
                        action.remote_unsupported = None;
                    }
                    Err(reason) => action.remote_unsupported = Some(reason.clone()),
                }
            }
            self.commit_action(
                action,
                own,
                &registered.runfiles_trees,
                registered.deferred,
                &owner,
                main_ignored,
                shared_inputs
                    .as_ref()
                    .map(|(start, shared)| (*start, shared)),
            )?;
        }
        let defaults = self.defaults(rule, &analyzed.files)?;
        self.ops.push(DeltaOp::Dry(Arc::new(defaults)));
        Ok(())
    }

    /// The DefaultInfo files' artifacts.
    fn defaults(&self, rule: &Rule, files: &[File]) -> Result<Vec<Artifact>, String> {
        files
            .iter()
            .map(|file| {
                self.lookup(file).ok_or_else(|| {
                    format!(
                        "{}: DefaultInfo file has no artifact: {}",
                        rule.label, file.path
                    )
                })
            })
            .collect()
    }

    /// cquery::register_outputs in this build: only the identities later
    /// provider analysis needs.
    fn semantic_outputs(
        &mut self,
        files: &[File],
        config: ConfigId,
        predeclared: &dyn Fn(&File) -> bool,
    ) {
        for file in files {
            let artifact = Artifact::semantic(file);
            self.register_artifact(file, &artifact);
            // Only predeclared outputs have package-level output-file targets.
            // Runtime declarations with the same path must not overwrite their
            // generating-rule identity. Other consumers resolve through the index.
            if predeclared(file) {
                self.ops.push(DeltaOp::Declared {
                    key: DeclaredKey::new(config, PathBuf::from(&file.path)),
                    artifact,
                    replace: true,
                    predeclared: true,
                });
            }
        }
    }
}

/// [`Engine::resolve_repo_mapping`] as a function of the repositories.
fn resolve_repo_mapping(
    repositories: &RepositorySet,
    compact: bool,
    transitive: &BTreeSet<String>,
    registered: &mut crate::analysis_runtime::RegisteredAction,
) -> Result<(), String> {
    let Some(runfiles) = registered.repo_mapping.take() else {
        return Ok(());
    };
    let content = crate::runfiles_tree::repo_mapping_manifest(
        &runfiles,
        transitive,
        &|repository: &str| repositories.mapping(repository),
        compact,
    )?;
    match registered.action.write.as_mut() {
        Some(write) => write.content = content,
        None => {
            return Err(format!(
                "internal error: the repo mapping manifest {:?} is no write",
                registered.action.outputs
            ));
        }
    }
    Ok(())
}

/// The planning state a coordinator build reads and its merge writes, for
/// the commits that merge their actions at once (native rules, aspect
/// applications, a lowering placed in commit order).
pub(super) struct CommitParts<'a> {
    pub(super) artifact_index: &'a ArtifactIndex,
    pub(super) workspace_status: &'a mut Option<WorkspaceStatusPlan>,
    pub(super) producers: &'a mut Producers,
    pub(super) nodes: &'a mut lowering::PlannedNodes,
    pub(super) source_inputs: &'a mut BTreeMap<String, SourceInputs>,
    pub(super) listed_tree_actions: &'a mut usize,
    pub(super) client_environment: &'a ClientEnvironment,
    pub(super) status_client_environment: &'a ClientEnvironment,
    pub(super) input_groups: &'a InputGroups,
    pub(super) input_memos: &'a InputMemos,
}

impl ActionPlanning {
    pub(super) fn commit_parts(&mut self) -> CommitParts<'_> {
        CommitParts {
            artifact_index: &self.artifact_index,
            workspace_status: &mut self.workspace_status,
            producers: &mut self.producers,
            nodes: &mut self.nodes,
            source_inputs: &mut self.source_inputs,
            listed_tree_actions: &mut self.listed_tree_actions,
            client_environment: &self.client_environment,
            status_client_environment: &self.status_client_environment,
            input_groups: &self.input_groups,
            input_memos: &self.input_memos,
        }
    }
}

impl Engine {
    /// Run `build` on a coordinator build over `parts` and merge the nodes
    /// it planned at once (they are the only operations of an action's
    /// commit, [`DeltaBuilder::commit_action`]).
    pub(super) fn commit_with<T>(
        &self,
        parts: CommitParts<'_>,
        build: impl FnOnce(&mut DeltaBuilder<'_>) -> Result<T, String>,
    ) -> Result<T, String> {
        let CommitParts {
            artifact_index,
            workspace_status,
            producers,
            nodes,
            source_inputs,
            listed_tree_actions,
            client_environment,
            status_client_environment,
            input_groups,
            input_memos,
        } = parts;
        let stamp = producers.stamp;
        let view = EngineView {
            semantic_only: self.semantic_only(),
            compact_repo_mapping: self.compact_repo_mapping,
            repositories: &self.repositories,
            artifacts: &artifact_index.shared,
            reader: artifact_index.turn,
            input_groups,
            input_memos,
            client_environment,
            lowering: self.lowering_for(nodes),
        };
        let hooks = Hooks::Coordinator {
            engine: self,
            index: artifact_index,
            workspace_status,
            producers,
            status_client_environment,
        };
        let mut builder = DeltaBuilder::new(view, hooks, stamp);
        let result = build(&mut builder);
        let ops = builder.take_ops();
        debug_assert!(
            builder.fallback.is_none(),
            "internal error: a coordinator build fell back"
        );
        drop(builder);
        let marks = (self.warnings.len(), self.errors.len());
        for op in ops {
            match op {
                DeltaOp::Node(planned, sources) => {
                    if let Some((id, sources)) = sources {
                        source_inputs.insert(id, sources);
                    }
                    nodes.push(*planned);
                }
                DeltaOp::Lowering(pending) => nodes.enqueue(pending, marks.0, marks.1),
                DeltaOp::ListedTreeAction => *listed_tree_actions += 1,
                _ => unreachable!("internal error: an action commit wrote more than nodes"),
            }
        }
        result
    }

    /// Run `build` on a coordinator build of the whole planning state,
    /// merging its nodes at once ([`Self::commit_with`]).
    pub(super) fn commit_now<T>(
        &self,
        state: &mut ActionPlanning,
        build: impl FnOnce(&mut DeltaBuilder<'_>) -> Result<T, String>,
    ) -> Result<T, String> {
        self.commit_with(state.commit_parts(), build)
    }

    /// Build a Starlark target's commit delta on the coordinator, at its
    /// turn or ahead of it (registrations checked against the build's
    /// producers; first-need registrations made here).
    pub(super) fn build_delta(
        &self,
        state: &mut ActionPlanning,
        commit: CustomCommit<'_>,
    ) -> TargetDelta {
        let parts = state.commit_parts();
        let view = EngineView {
            semantic_only: self.semantic_only(),
            compact_repo_mapping: self.compact_repo_mapping,
            repositories: &self.repositories,
            artifacts: &parts.artifact_index.shared,
            reader: parts.artifact_index.turn,
            input_groups: parts.input_groups,
            input_memos: parts.input_memos,
            client_environment: parts.client_environment,
            lowering: self.lowering_for(parts.nodes),
        };
        let stamp = parts.producers.stamp;
        let hooks = Hooks::Coordinator {
            engine: self,
            index: parts.artifact_index,
            workspace_status: parts.workspace_status,
            producers: parts.producers,
            status_client_environment: parts.status_client_environment,
        };
        let mut builder = DeltaBuilder::new(view, hooks, stamp);
        let result = builder.custom_target(commit);
        match builder.finish(result) {
            Ok(delta) => delta,
            Err(reason) => unreachable!("internal error: a coordinator build fell back ({reason})"),
        }
    }

    /// Whether an output path of `delta`, built without the producers,
    /// meets one another target registered (S1: it is built again).
    pub(super) fn delta_meets_producers(state: &ActionPlanning, delta: &TargetDelta) -> bool {
        delta.unchecked
            && (delta.ops.iter().any(|op| match op {
                DeltaOp::Output(path, _) => meets_producer_hashed(&state.producers, path),
                _ => false,
            }) || delta
                .failed_output
                .as_ref()
                .is_some_and(|path| meets_producer(&state.producers, path)))
    }

    /// Apply `delta` (the commit of `key`) to the planning state, in its
    /// order; its error last ([`merge_ops`]).
    pub(super) fn merge_delta(
        &mut self,
        state: &mut ActionPlanning,
        key: &ConfiguredTargetKey,
        delta: TargetDelta,
    ) -> Result<(), String> {
        // BAZEL_RS_EVAL_REPLAY_BENCH (docs/EVALUATOR.md): keep a copy for
        // the stub linearizer's replay, and time the real merge.
        let bench = super::eval::replay_bench::enabled();
        if bench {
            super::eval::replay_bench::record(key, &delta);
        }
        let started = bench.then(std::time::Instant::now);
        let sink = MergeSink {
            action_executions: &mut self.action_executions,
            build_info_actions: &mut self.build_info_actions,
            warnings: self.warnings.len(),
            errors: self.errors.len(),
        };
        let merged = merge_ops(state, sink, key, delta);
        if let Some(started) = started {
            super::eval::replay_bench::note_merge(started.elapsed());
        }
        merged
    }
}

/// What a delta's merge writes outside the planning state: the engine's
/// per-action tables and the console marks of a lowering in flight.
pub(super) struct MergeSink<'a> {
    pub(super) action_executions: &'a mut ActionExecutions,
    pub(super) build_info_actions: &'a mut std::collections::BTreeSet<String>,
    /// The engine's warnings and errors so far (a lowering's marks).
    pub(super) warnings: usize,
    pub(super) errors: usize,
}

/// Apply `delta` (the commit of `key`) to `state` and `sink`, in its
/// order; its error last: the linearizer's merge ([`Engine::merge_delta`];
/// the stub linearizer of [`super::eval::replay_bench`] replays through it).
pub(super) fn merge_ops(
    state: &mut ActionPlanning,
    sink: MergeSink<'_>,
    key: &ConfiguredTargetKey,
    delta: TargetDelta,
) -> Result<(), String> {
    for op in delta.ops {
        merge_op(
            state,
            &mut *sink.action_executions,
            &mut *sink.build_info_actions,
            (sink.warnings, sink.errors),
            key,
            op,
        );
    }
    if let Some(path) = &delta.failed_output {
        // As register_output records the path it fails on.
        parallel::record_registered_output(path);
    }
    match delta.error {
        Some(error) => Err(error),
        None => Ok(()),
    }
}

/// One operation of [`merge_ops`].
pub(super) fn merge_op(
    state: &mut ActionPlanning,
    action_executions: &mut ActionExecutions,
    build_info_actions: &mut std::collections::BTreeSet<String>,
    (warnings, errors): (usize, usize),
    key: &ConfiguredTargetKey,
    op: DeltaOp,
) {
    match op {
        DeltaOp::Providers(providers, provider_keys) => {
            state.custom_providers.insert(key.clone(), providers);
            state.provider_keys.insert(key.clone(), provider_keys);
        }
        DeltaOp::Output(path, producer) => {
            parallel::record_registered_hashed(path.path(), path.hash());
            state.producers.insert_hashed(path, producer);
        }
        DeltaOp::Shared(path, registered) => {
            if order_audit::enabled() {
                let winner = state
                    .producers
                    .get_indexed(&path)
                    .and_then(|(producer, _)| producer.action.as_ref())
                    .map(|(id, _)| id.clone());
                order_audit::note_shared(
                    &path,
                    winner.as_deref(),
                    order_audit::Sharer::Target(key.clone()),
                );
            }
            parallel::record_registered_output(&path);
            parallel::note_shared(state.producers.stamp, registered, state.producers.turn)
        }
        DeltaOp::Artifact(file, artifact, hash) => {
            register_owned_artifact(&state.artifact_index, hash, file, artifact);
        }
        DeltaOp::Declared {
            key,
            artifact,
            replace,
            predeclared: _,
        } => {
            state.declared.put(key, artifact, replace);
        }
        DeltaOp::RunfilesInfo(runfiles) => {
            state.runfiles_info.insert(key.clone(), runfiles);
        }
        DeltaOp::RunfilesNodes(nodes) => {
            state.runfiles_nodes.insert(key.clone(), *nodes);
        }
        DeltaOp::Node(planned, sources) => {
            if let Some((id, sources)) = sources {
                state.source_inputs.insert(id, sources);
            }
            state.nodes.push(*planned);
        }
        DeltaOp::Lowering(pending) => {
            state.nodes.enqueue(pending, warnings, errors);
        }
        DeltaOp::ActionExecution(id, execution) => {
            action_executions.insert(id, execution);
        }
        DeltaOp::BuildInfoAction(id) => {
            build_info_actions.insert(id);
        }
        DeltaOp::Dry(defaults) => {
            state.dry.insert(key.clone(), defaults);
        }
        DeltaOp::ListedTreeAction => state.listed_tree_actions += 1,
    }
}

impl DeltaOp {
    /// The operation's kind, for the replay bench's breakdown.
    pub(super) fn kind(&self) -> &'static str {
        match self {
            DeltaOp::Providers(..) => "providers",
            DeltaOp::Output(..) => "output",
            DeltaOp::Shared(..) => "shared",
            DeltaOp::Artifact(..) => "artifact",
            DeltaOp::Declared { .. } => "declared",
            DeltaOp::RunfilesInfo(..) => "runfiles_info",
            DeltaOp::RunfilesNodes(..) => "runfiles_nodes",
            DeltaOp::Node(..) => "node",
            DeltaOp::Lowering(..) => "lowering",
            DeltaOp::ActionExecution(..) => "action_execution",
            DeltaOp::BuildInfoAction(..) => "build_info_action",
            DeltaOp::Dry(..) => "dry",
            DeltaOp::ListedTreeAction => "listed_tree_action",
        }
    }

    /// A copy of the operation for the replay bench; `None` for a lowering
    /// in flight (it owns its worker's outcome).
    pub(super) fn replay_copy(&self) -> Option<DeltaOp> {
        Some(match self {
            DeltaOp::Providers(providers, keys) => {
                DeltaOp::Providers(providers.clone(), keys.clone())
            }
            DeltaOp::Output(path, producer) => DeltaOp::Output(path.clone(), Arc::clone(producer)),
            DeltaOp::Shared(path, registered) => DeltaOp::Shared(path.clone(), *registered),
            DeltaOp::Artifact(file, artifact, hash) => {
                DeltaOp::Artifact(file.clone(), artifact.clone(), *hash)
            }
            DeltaOp::Declared {
                key,
                artifact,
                replace,
                predeclared,
            } => DeltaOp::Declared {
                key: key.clone(),
                artifact: artifact.clone(),
                replace: *replace,
                predeclared: *predeclared,
            },
            DeltaOp::RunfilesInfo(runfiles) => DeltaOp::RunfilesInfo(Arc::clone(runfiles)),
            DeltaOp::RunfilesNodes(nodes) => DeltaOp::RunfilesNodes(nodes.clone()),
            DeltaOp::Node(planned, sources) => DeltaOp::Node(planned.clone(), sources.clone()),
            DeltaOp::Lowering(_) => return None,
            DeltaOp::ActionExecution(id, execution) => {
                DeltaOp::ActionExecution(id.clone(), execution.clone())
            }
            DeltaOp::BuildInfoAction(id) => DeltaOp::BuildInfoAction(id.clone()),
            DeltaOp::Dry(defaults) => DeltaOp::Dry(Arc::clone(defaults)),
            DeltaOp::ListedTreeAction => DeltaOp::ListedTreeAction,
        })
    }
}

/// What the analysis worker made of a compute's commit (S1).
pub(super) struct WorkerDelta {
    pub(super) built: Built,
    /// Why its commit may not merge ahead of its turn (the part of
    /// Engine::commit_ahead_unsafe decided by the analysis result), if any.
    pub(super) ahead_unsafe: Option<&'static str>,
    /// BAZEL_RS_VERIFY_AHEAD: the outputs its commit may register.
    pub(super) summary: Option<parallel::OutputSummary>,
}

/// The worker's delta, or why it has none.
pub(super) enum Built {
    Delta(TargetDelta),
    /// Not built (this reason): the analysis result is kept for the
    /// coordinator's build.
    Fallback(&'static str),
    /// Fell back during the build (this reason), which consumed the
    /// analysis result: the coordinator computes it again.
    Consumed(&'static str),
}

/// What a worker needs to build a target's delta after its compute.
pub(super) struct DeltaInputs {
    pub(super) view: SharedView,
    pub(super) key: ConfiguredTargetKey,
    pub(super) output_root: String,
    pub(super) action_owner: String,
    pub(super) main_ignored: Arc<IgnoredSubdirectories>,
    pub(super) transitive: BTreeSet<String>,
    pub(super) action_execution: Vec<crate::exec_groups::ActionExecution>,
    pub(super) platforms: RemotePlatforms,
    pub(super) class: u32,
}

/// An analysis result whose parts a delta took.
fn spent_analysis() -> analysis::AnalysisResult {
    analysis::AnalysisResult {
        files: Vec::new(),
        declared: Vec::new(),
        actions: Vec::new(),
        providers: BTreeMap::new(),
        provider_keys: Vec::new(),
        runfiles: Default::default(),
    }
}

impl DeltaInputs {
    /// Why a worker does not build the delta of `analyzed`: something its
    /// commit registers on first need (the workspace status, a build-info
    /// action resolved against it), a repo mapping manifest left to
    /// resolve, a spawn whose execution platform is not analyzed yet.
    fn unsafe_reason(
        &self,
        analyzed: &analysis::AnalysisResult,
        precomputed: Option<&RunfilesPrecomputed>,
    ) -> Option<&'static str> {
        if self.view.semantic_only {
            return Some("delta_semantic");
        }
        if Engine::workspace_status_needed(&analyzed.actions)
            || analyzed
                .files
                .iter()
                .any(crate::analysis_runtime::is_workspace_status_file)
            || precomputed.is_some_and(|precomputed| {
                precomputed
                    .mapping
                    .values()
                    .flatten()
                    .any(crate::analysis_runtime::is_workspace_status_file)
            })
        {
            return Some("delta_workspace_status");
        }
        if analyzed
            .actions
            .iter()
            .any(|action| action.repo_mapping.is_some() || action.build_info.is_some())
        {
            return Some("delta_repo_mapping_or_build_info");
        }
        if !self
            .platforms
            .all_final(&self.action_execution, &analyzed.actions)
        {
            return Some("delta_remote_platform");
        }
        None
    }

    /// Build the delta of `outcome` (a successful compute of the target at
    /// plan index `index` from `snapshot`) on this worker.
    pub(super) fn build(
        self,
        snapshot: &ComputeSnapshot,
        outcome: &mut ComputeOutcome,
        index: usize,
        verify_summary: bool,
        registers: &parallel::OutputSummary,
    ) -> WorkerDelta {
        let Ok(analyzed) = outcome.analyzed.as_ref() else {
            unreachable!("internal error: a delta of a failed compute");
        };
        let rule = &snapshot.rule;
        let ahead_unsafe = parallel::ahead_unsafe(
            rule,
            &self.output_root,
            analyzed,
            outcome.runfiles.as_ref(),
            self.platforms
                .all_final(&self.action_execution, &analyzed.actions),
            first_fill_meets_status(&self.view.input_groups, analyzed),
        );
        let summary = verify_summary.then(|| parallel::OutputSummary::of(analyzed));
        let reader = u32::try_from(index).unwrap_or(u32::MAX - 1);
        let meets = self.view.registered.as_ref().is_some_and(|registered| {
            // Both noted: the earlier computes first.
            let earlier = registered.meet_earlier(reader, registers);
            registered.meet(registers) || earlier
        });
        if let Some(reason) = self
            .unsafe_reason(analyzed, outcome.runfiles.as_ref())
            .or(meets.then_some("delta_meets_producer"))
        {
            return WorkerDelta {
                built: Built::Fallback(reason),
                ahead_unsafe,
                summary,
            };
        }
        let analyzed = std::mem::replace(&mut outcome.analyzed, Ok(spent_analysis()))
            .unwrap_or_else(|_| unreachable!());
        let timer = crate::plan_timings::StageTimer::start(
            crate::plan_timings::PlanStage::Delta,
            index as u64,
            self.class,
        );
        let mut builder = DeltaBuilder::new(self.view.view(reader), Hooks::Worker, reader);
        let result = builder.custom_target(CustomCommit {
            key: &self.key,
            rule,
            output_root: &self.output_root,
            action_owner: &self.action_owner,
            main_ignored: &self.main_ignored,
            transitive: &self.transitive,
            action_execution: &self.action_execution,
            platforms: &self.platforms,
            runfiles_options: &snapshot.runfiles,
            analyzed,
            precomputed: outcome.runfiles.take(),
            outputs_checked: outcome.outputs_checked.take(),
        });
        let built = match builder.finish(result) {
            Ok(delta) => Built::Delta(delta),
            Err(reason) => Built::Consumed(reason),
        };
        timer.finish();
        WorkerDelta {
            built,
            ahead_unsafe,
            summary,
        }
    }
}

/// Whether filling a tool runfiles group the actions of `analyzed` use (one
/// not filled yet) would meet a workspace status file, which registers the
/// status on first need: such a commit stays at its turn (review of S1,
/// item A; main's `ineligible_first_fill`).
pub(super) fn first_fill_meets_status(
    groups: &InputGroups,
    analyzed: &analysis::AnalysisResult,
) -> bool {
    analyzed
        .actions
        .iter()
        .flat_map(|action| &action.runfiles_trees)
        .any(|tree| {
            !groups.is_filled(&tree.identity())
                && tree
                    .mapping()
                    .values()
                    .flatten()
                    .any(crate::analysis_runtime::is_workspace_status_file)
        })
}

/// `BAZEL_RS_VERIFY_DELTA=1` (hidden, like BAZEL_RS_VERIFY_AHEAD): every
/// delta a worker built is built again where it merges, on the
/// coordinator, and the two compared ([`compare_deltas`]).
pub(super) fn verify_delta() -> bool {
    static VERIFY: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *VERIFY.get_or_init(|| {
        // snapshot-unrecorded: a diagnostics check of the scheduling; it changes no result (it panics on a difference).
        std::env::var_os("BAZEL_RS_VERIFY_DELTA").is_some_and(|value| value == "1")
    })
}

impl Engine {
    /// BAZEL_RS_VERIFY_DELTA: build `commit`'s delta here and now, as a
    /// worker builds it (nothing registered on first need, lowered in the
    /// build), without merging it.
    pub(super) fn verify_build(
        &self,
        state: &ActionPlanning,
        commit: CustomCommit<'_>,
    ) -> TargetDelta {
        let view = EngineView {
            semantic_only: self.semantic_only(),
            compact_repo_mapping: self.compact_repo_mapping,
            repositories: &self.repositories,
            artifacts: &state.artifact_index.shared,
            reader: state.artifact_index.turn,
            input_groups: &state.input_groups,
            input_memos: &state.input_memos,
            client_environment: &state.client_environment,
            lowering: None,
        };
        let mut builder = DeltaBuilder::new(view, Hooks::Worker, state.producers.stamp);
        let result = builder.custom_target(commit);
        match builder.finish(result) {
            Ok(delta) => delta,
            Err(reason) => panic!(
                "internal error: BAZEL_RS_VERIFY_DELTA: the delta built at the merge fell back ({reason})"
            ),
        }
    }
}

/// A comparable rendering of one delta operation (BAZEL_RS_VERIFY_DELTA):
/// values by content; the providers by id, type and display name (a
/// compute run again allocates its own values, and a repr would walk —
/// and memoize — every depset they hold).
fn rendered(op: &DeltaOp) -> String {
    match op {
        DeltaOp::Providers(providers, keys) => format!(
            "providers {:?} {keys:?}",
            providers
                .iter()
                .map(|(id, value)| (id, value.value().get_type()))
                .collect::<Vec<_>>()
        ),
        DeltaOp::Output(path, producer) => format!(
            "output {} {} {:?} {:?}",
            path.path().display(),
            producer.owner,
            producer.label,
            producer.action.as_ref().map(|(id, _)| id)
        ),
        DeltaOp::Shared(path, registered) => format!("shared {} {registered}", path.display()),
        DeltaOp::Artifact(file, artifact, _) => format!("artifact {file:?} {artifact:?}"),
        DeltaOp::Declared {
            key,
            artifact,
            replace,
            predeclared,
        } => format!("declared {key:?} {artifact:?} {replace} {predeclared}"),
        // The runfiles' files are compared through the runfiles support's
        // mapping (RunfilesNodes) and the files to build (Dry): listing them
        // here would flatten every target's runfiles.
        DeltaOp::RunfilesInfo(runfiles) => format!(
            "runfiles {:?}",
            runfiles
                .files_to_run
                .as_ref()
                .map(|files_to_run| files_to_run.executable().cloned())
        ),
        DeltaOp::RunfilesNodes(nodes) => format!("runfiles nodes {nodes:?}"),
        DeltaOp::Node(planned, sources) => format!(
            "node {planned:?} {:?}",
            sources.as_ref().map(|(id, sources)| (
                id,
                &sources.own,
                sources.groups.iter().map(Arc::as_ptr).collect::<Vec<_>>()
            ))
        ),
        DeltaOp::Lowering(_) => "lowering".to_owned(),
        DeltaOp::ActionExecution(id, execution) => format!("execution {id} {execution:?}"),
        DeltaOp::BuildInfoAction(id) => format!("build info {id}"),
        DeltaOp::Dry(defaults) => format!("dry {defaults:?}"),
        DeltaOp::ListedTreeAction => "listed".to_owned(),
    }
}

/// BAZEL_RS_VERIFY_DELTA's comparison: the delta a worker built for `key`
/// against the one built where it merges — every operation in order, the
/// error and the memos used.
pub(super) fn compare_deltas(key: &ConfiguredTargetKey, worker: &TargetDelta, here: &TargetDelta) {
    let fail = |what: &str, worker: &dyn std::fmt::Debug, here: &dyn std::fmt::Debug| -> ! {
        panic!(
            "internal error: BAZEL_RS_VERIFY_DELTA: the delta of {} built on a worker differs in {what}:\nworker: {worker:#?}\nmerge:  {here:#?}",
            key.label
        )
    };
    if worker.ops.len() != here.ops.len() {
        fail(
            "its operations",
            &worker.ops.iter().map(rendered).collect::<Vec<_>>(),
            &here.ops.iter().map(rendered).collect::<Vec<_>>(),
        );
    }
    for (position, (a, b)) in worker.ops.iter().zip(&here.ops).enumerate() {
        let (a, b) = (rendered(a), rendered(b));
        if a != b {
            fail(&format!("operation {position}"), &a, &b);
        }
    }
    if worker.error != here.error {
        fail("its error", &worker.error, &here.error);
    }
    // The tool runfiles groups it used, in order. (Whether an action lists
    // a shared input run depends on the depset walks memoized so far in
    // the process — a compute run again may find one the first did not —
    // so those uses are not compared; the nodes are.)
    let groups = |delta: &TargetDelta| {
        delta
            .uses
            .iter()
            .filter(|used| matches!(used, MemoUse::Group(_)))
            .cloned()
            .collect::<Vec<_>>()
    };
    if groups(worker) != groups(here) {
        fail("the tool runfiles groups it used", &worker.uses, &here.uses);
    }
    parallel::note_verified_delta();
}

#[cfg(test)]
mod tests {
    use super::*;

    /// T2 review: a memo fill that resolves a File through the worker
    /// build's own (unpublished) registrations records no read, so its
    /// value is not kept for other users; a fill through the index is.
    #[test]
    fn memo_fills_through_own_registrations_are_not_shared() {
        let repositories = RepositorySet::new(std::env::temp_dir());
        let artifacts = ConcurrentArtifactIndex::default();
        let (groups, memos, environment) = (
            InputGroups::default(),
            InputMemos::default(),
            ClientEnvironment::default(),
        );
        let view = EngineView {
            semantic_only: false,
            compact_repo_mapping: false,
            repositories: &repositories,
            artifacts: &artifacts,
            reader: 5,
            input_groups: &groups,
            input_memos: &memos,
            client_environment: &environment,
            lowering: None,
        };
        let path = PathBuf::from("bazel-out/k8-fastbuild/bin/pkg/own.txt");
        let own = Artifact::new(
            path.clone(),
            path,
            Some(("//pkg:own#0".into(), 0)),
            "//pkg:own".into(),
            false,
            false,
        );
        let mut builder = DeltaBuilder::new(view, Hooks::Worker, 5);
        builder.register_artifact(&own.file, &own);
        builder.reads_mark();
        assert!(builder.lookup(&own.file).is_some());
        assert!(builder.fill_done(), "resolved through its own registration");
        builder.reads_mark();
        assert!(builder.lookup(&own.file.clone()).is_some());
        builder.reads_mark();
        assert!(
            !builder.fill_done(),
            "the inner fill read nothing of its own"
        );
        assert!(builder.fill_done(), "the outer one did");
        builder.reads_mark();
        let other = Artifact::new(
            PathBuf::from("pkg/src.txt"),
            PathBuf::from("pkg/src.txt"),
            None,
            "//pkg:src.txt".into(),
            false,
            false,
        );
        assert!(builder.lookup(&other.file).is_none());
        assert!(!builder.fill_done());
    }
}
