//! P3: rule implementations on analysis workers (docs/PARALLEL_ANALYSIS.md,
//! "P3 — parallel rule implementations").
//!
//! Phase 2 walks the plan in its order (phase 1's post-order) and commits
//! every target in that order on the coordinator, exactly as the sequential
//! engine does: every state mutation and every console write of a planned
//! target happens at its turn. What changes is *when* a Starlark rule's
//! implementation runs. Its compute ([`Engine::compute_target`]) is a pure
//! function of an owned snapshot; a worker runs it while the coordinator
//! goes on:
//!
//! * a target whose dependencies are all committed may be **prepared ahead
//!   of its turn** (its snapshot built: dependency views, toolchain
//!   contexts, ctx surface) and its compute dispatched; its turn then only
//!   waits for the outcome and commits it;
//! * the target at its turn, when it was not prepared ahead, is prepared
//!   there and computed on a worker too, the coordinator preparing others
//!   ahead while it waits.
//!
//! What a target's prepare or compute would have written where the turn
//! order is observable is held and applied at its turn: console notes
//! (crate::ui::DeferredConsole), what it observed for the analysis snapshot
//! (crate::observed_fs::unit_scope), the repository roots it obtained
//! (crate::repository::UnitRoots), the incremental-analysis tracker's
//! events (Engine::track_defer_start). Work ahead of its turn never decides
//! anything the order decides: it runs in no-demand mode (an unfetched
//! repository aborts it with the sentinel) and aborts on analyzing a
//! configured target or loading a package (Engine::ahead_abort); an aborted
//! or invalidated prepare is done again at the turn, an aborted compute
//! again inline at the commit (P1's retry).
//!
//! `--loading_phase_threads=1` (and `BAZEL_RS_DEBUG_NO_PARALLEL_ANALYSIS`)
//! is the sequential engine.

use super::*;
use std::collections::BTreeSet as OrderedSet;
use std::sync::mpsc;

/// Marker of the abort of work ahead of its turn (never reported: the turn
/// does the work again).
const AHEAD_ABORT: &str = "\u{1}parallel-analysis-ahead-abort";

/// How many plan entries past the turn the coordinator classifies.
const HORIZON: usize = 4096;

type Job = Box<dyn FnOnce() + Send>;

/// A finished compute: its plan index and the outputs its commit registers
/// (`None`: not known, e.g. it raised a sentinel and runs again inline).
type Finished = (usize, Option<Arc<OutputSummary>>);

/// The output paths the commit of a finished compute registers, hashed
/// ([`path_hash`]), and their proper ancestors: what decides whether two
/// uncommitted entries' commits may meet in the producers (an exact path:
/// a shared action or a conflict; a path above or below another: a prefix
/// conflict). Hash collisions only make two entries look related (the later
/// one then waits), never unrelated.
#[derive(Default, Debug)]
pub(super) struct OutputSummary {
    pub(super) paths: Vec<u64>,
    pub(super) ancestors: Vec<u64>,
}

pub(super) fn path_hash(path: &Path) -> u64 {
    use std::hash::BuildHasher;
    crate::path_key::FastState::default().hash_one(crate::path_key::PathKey(path))
}

impl OutputSummary {
    /// Every output path the commit of `analyzed` may register (a superset:
    /// its actions' outputs, the runfiles support's executable, manifests
    /// and tree; Engine::plan_runfiles_support).
    pub(super) fn of(analyzed: &analysis::AnalysisResult) -> Self {
        let mut paths: Vec<PathBuf> = analyzed
            .actions
            .iter()
            .flat_map(|action| &action.outputs)
            .map(|file| PathBuf::from(&file.path))
            .collect();
        if let Some(files_to_run) = analyzed.runfiles.files_to_run.as_ref() {
            for file in files_to_run
                .executable()
                .into_iter()
                .chain(files_to_run.repo_mapping_manifest())
                .chain(files_to_run.runfiles_manifest())
            {
                paths.push(PathBuf::from(&file.path));
            }
            if let Some(executable) = files_to_run.executable() {
                paths.push(PathBuf::from(format!(
                    "{}.runfiles_manifest",
                    executable.path
                )));
                paths.push(PathBuf::from(format!(
                    "{}.runfiles/MANIFEST",
                    executable.path
                )));
                paths.push(PathBuf::from(format!("{}.repo_mapping", executable.path)));
            }
        }
        let mut hashes: Vec<u64> = paths.iter().map(|path| path_hash(path)).collect();
        let mut ancestors: Vec<u64> = paths
            .iter()
            .flat_map(|path| path.ancestors().skip(1))
            .filter(|ancestor| !ancestor.as_os_str().is_empty())
            .map(path_hash)
            .collect();
        hashes.sort_unstable();
        hashes.dedup();
        ancestors.sort_unstable();
        ancestors.dedup();
        Self {
            paths: hashes,
            ancestors,
        }
    }
}

thread_local! {
    /// BAZEL_RS_VERIFY_AHEAD: the output paths the commit being verified
    /// registered (crate::engine::register_output).
    static REGISTERED: std::cell::RefCell<Option<Vec<u64>>> = const { std::cell::RefCell::new(None) };
}

/// Note an output path a commit registers (BAZEL_RS_VERIFY_AHEAD records
/// them to check the commit's [`OutputSummary`]).
pub(super) fn record_registered_output(output: &Path) {
    // The workspace status files belong to no target: whichever commit
    // needs them first registers them (Engine::ensure_workspace_status), and
    // a commit that needs them never commits ahead of its turn.
    if matches!(
        output.to_str(),
        Some("bazel-out/stable-status.txt" | "bazel-out/volatile-status.txt")
    ) {
        return;
    }
    REGISTERED.with(|registered| {
        if let Some(registered) = registered.borrow_mut().as_mut() {
            registered.push(path_hash(output));
        }
    });
}

/// BAZEL_RS_VERIFY_AHEAD: run `commit`, checking that every output path it
/// registers is in the summary its compute's outputs give (which decided
/// whether later entries committed ahead of its turn).
fn verify_registered<T>(
    key: &ConfiguredTargetKey,
    summary: Option<OutputSummary>,
    commit: impl FnOnce() -> T,
) -> T {
    let Some(summary) = summary else {
        return commit();
    };
    let outer = REGISTERED.with(|registered| registered.borrow_mut().replace(Vec::new()));
    let result = commit();
    let registered = REGISTERED
        .with(|registered| std::mem::replace(&mut *registered.borrow_mut(), outer))
        .unwrap_or_default();
    if let Some(missing) = registered
        .iter()
        .find(|hash| summary.paths.binary_search(hash).is_err())
    {
        panic!(
            "internal error: BAZEL_RS_VERIFY_AHEAD: the commit of {} registered an output (hash {missing:x}) outside the outputs of its compute",
            key.label
        );
    }
    bump(&stats().verified_outputs, 1);
    result
}

/// The analysis workers (16 MiB stacks: Starlark evaluation and depset
/// walks recurse; P2's loading workers use the same).
pub(super) struct AnalysisPool {
    /// The plan index of each finished compute (P4: the coordinator commits
    /// finished entries ahead of their turn), with the outputs its commit
    /// registers when they are known (plan B: no sentinel).
    finished: (mpsc::Sender<Finished>, mpsc::Receiver<Finished>),
    jobs: Option<mpsc::Sender<Job>>,
    workers: Vec<std::thread::JoinHandle<()>>,
    size: usize,
    /// Set when the pool stops: queued computes are skipped (their
    /// results would be for turns that never come).
    cancelled: Arc<std::sync::atomic::AtomicBool>,
}

thread_local! {
    /// The message and location of a panic on this analysis worker (the
    /// panic hook records it instead of printing: the sequential engine
    /// may never run that compute; the coordinator panics with it at the
    /// target's turn).
    static WORKER_PANIC: std::cell::RefCell<Option<String>> = const { std::cell::RefCell::new(None) };
    /// This thread is an analysis worker.
    static IS_WORKER: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Install (once) the panic hook that keeps analysis workers' panics
/// quiet ([`WORKER_PANIC`]); other threads keep the previous hook.
fn install_worker_panic_hook() {
    static INSTALLED: std::sync::Once = std::sync::Once::new();
    INSTALLED.call_once(|| {
        let previous = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            if IS_WORKER.with(std::cell::Cell::get) {
                let message = info
                    .payload()
                    .downcast_ref::<&str>()
                    .map(|text| (*text).to_owned())
                    .or_else(|| info.payload().downcast_ref::<String>().cloned())
                    .unwrap_or_else(|| "Box<dyn Any>".to_owned());
                let location = info
                    .location()
                    .map(|location| {
                        format!(
                            "{}:{}:{}",
                            location.file(),
                            location.line(),
                            location.column()
                        )
                    })
                    .unwrap_or_default();
                WORKER_PANIC.with(|slot| {
                    *slot.borrow_mut() =
                        Some(format!("{message} (at {location}, on an analysis worker)"));
                });
            } else {
                previous(info);
            }
        }));
    });
}

impl AnalysisPool {
    fn start(size: usize) -> Option<Self> {
        install_worker_panic_hook();
        // The workers allocate in the coordinator's jemalloc arena: the
        // snapshots, providers and actions they make are kept and freed by
        // the coordinator (src/alloc_arena.rs).
        let coordinator_arena = crate::alloc_arena::current();
        let (sender, receiver) = mpsc::channel::<Job>();
        let receiver = Arc::new(std::sync::Mutex::new(receiver));
        let mut workers = Vec::with_capacity(size);
        for index in 0..size {
            let receiver = Arc::clone(&receiver);
            let spawned = std::thread::Builder::new()
                .name(format!("analysis-{index}"))
                .stack_size(LOADING_WORKER_STACK)
                .spawn(move || {
                    IS_WORKER.with(|worker| worker.set(true));
                    if let Some(arena) = coordinator_arena {
                        crate::alloc_arena::bind(arena);
                    }
                    loop {
                        let job = {
                            let receiver = receiver.lock().unwrap_or_else(|e| e.into_inner());
                            receiver.recv()
                        };
                        match job {
                            Ok(job) => job(),
                            Err(_) => break,
                        }
                    }
                });
            match spawned {
                Ok(handle) => workers.push(handle),
                Err(_) => break,
            }
        }
        if workers.is_empty() {
            return None;
        }
        Some(Self {
            finished: mpsc::channel(),
            jobs: Some(sender),
            size: workers.len(),
            workers,
            cancelled: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        })
    }

    fn submit(&self, job: Job) -> Result<(), Job> {
        match &self.jobs {
            Some(jobs) => jobs.send(job).map_err(|error| error.0),
            None => Err(job),
        }
    }
}

impl Drop for AnalysisPool {
    fn drop(&mut self) {
        self.cancelled
            .store(true, std::sync::atomic::Ordering::SeqCst);
        self.jobs = None;
        for worker in self.workers.drain(..) {
            let _ = worker.join();
        }
    }
}

/// What work off its turn would have written where the turn order is
/// observable, applied at the turn ([`Effects::apply`]).
#[derive(Default)]
struct Effects {
    console: Vec<crate::ui::Deferred>,
    observations: crate::observed_fs::Observations,
    roots: Vec<String>,
}

impl Effects {
    /// Apply at the sequential point: notes, observations, roots.
    fn apply(self, engine: &Engine, console: bool) {
        if console {
            crate::ui::replay(self.console);
        }
        crate::observed_fs::commit(self.observations);
        engine.repositories.note_committed_roots(&self.roots);
    }
}

/// The scopes of work off its turn (a worker's compute, a prepare ahead):
/// console held, observations and obtained roots kept, no repository
/// demand. [`Isolation::finish`] returns what it held and whether a
/// no-demand sentinel was raised (possibly swallowed by a caller).
struct Isolation {
    console: crate::ui::DeferredConsole,
    roots: crate::repository::UnitRoots,
    scope: crate::observed_fs::UnitScope,
    reuse: crate::repository_demand::ReuseOnlyGuard,
    no_demand: crate::repository_demand::NoDemandGuard,
    sentinels: u64,
    /// The caller's order-dependent record, put back by finish.
    order_dependent: bool,
}

impl Isolation {
    fn enter() -> Self {
        let console = crate::ui::DeferredConsole::start();
        let roots = crate::repository::UnitRoots::record();
        let scope = crate::observed_fs::unit_scope();
        let no_demand = crate::repository_demand::no_demand();
        let reuse = crate::repository_demand::reuse_only();
        Self {
            console,
            roots,
            scope,
            reuse,
            no_demand,
            sentinels: crate::repository_demand::sentinels_raised(),
            order_dependent: crate::repository_demand::take_order_dependent(),
        }
    }

    fn finish(self) -> (Effects, bool) {
        let Self {
            console,
            roots,
            scope,
            reuse,
            no_demand,
            sentinels,
            order_dependent,
        } = self;
        // An order-dependent link decision is a sentinel too: the work is
        // redone where the sequential engine decides it.
        let sentinel = crate::repository_demand::sentinels_raised() != sentinels
            || crate::repository_demand::take_order_dependent();
        crate::repository_demand::restore_order_dependent(order_dependent);
        drop(reuse);
        drop(no_demand);
        let observations = scope.finish();
        let roots = roots.finish();
        let console = console.finish();
        (
            Effects {
                console,
                observations,
                roots,
            },
            sentinel,
        )
    }
}

/// A compute's result from a worker.
struct WorkerOutcome {
    snapshot: ComputeSnapshot,
    outcome: ComputeOutcome,
    effects: Effects,
    /// A no-demand sentinel was raised during the compute: it is run again
    /// inline at the commit, where demand may fetch.
    sentinel: bool,
    elapsed: std::time::Duration,
}

/// A worker's compute: its outcome, or the text of its panic.
type Pending = mpsc::Receiver<Result<WorkerOutcome, String>>;

/// Where a turn's compute runs: on a worker (prepared ahead) or here.
enum Compute {
    Worker(Pending),
    Inline(Box<ComputeSnapshot>),
}

/// A prepared target without its snapshot (on a worker).
struct PreparedParts {
    key: ConfiguredTargetKey,
    config: ConfigId,
    output_root: String,
    action_env: Arc<crate::action_env::ActionEnvironment>,
    action_owner: String,
    exec_config: ConfigId,
    main_ignored: Arc<IgnoredSubdirectories>,
    transitive: BTreeSet<String>,
    action_execution: Vec<crate::exec_groups::ActionExecution>,
}

impl PreparedParts {
    fn split(prepared: PreparedTarget) -> Option<(Self, ComputeSnapshot)> {
        let PreparedTarget {
            key,
            config,
            output_root,
            action_env,
            action_owner,
            exec_config,
            main_ignored,
            transitive,
            kind,
        } = prepared;
        match kind {
            PreparedKind::Custom(custom) => {
                let CustomPrepared {
                    snapshot,
                    action_execution,
                } = *custom;
                Some((
                    Self {
                        key,
                        config,
                        output_root,
                        action_env,
                        action_owner,
                        exec_config,
                        main_ignored,
                        transitive,
                        action_execution,
                    },
                    snapshot,
                ))
            }
            _ => None,
        }
    }

    fn join(self, snapshot: ComputeSnapshot) -> PreparedTarget {
        PreparedTarget {
            key: self.key,
            config: self.config,
            output_root: self.output_root,
            action_env: self.action_env,
            action_owner: self.action_owner,
            exec_config: self.exec_config,
            main_ignored: self.main_ignored,
            transitive: self.transitive,
            kind: PreparedKind::Custom(Box::new(CustomPrepared {
                snapshot,
                action_execution: self.action_execution,
            })),
        }
    }
}

/// A plan entry prepared ahead of its turn.
enum Ahead {
    /// Prepared; its compute runs on a worker.
    Computing {
        parts: Box<PreparedParts>,
        effects: Effects,
        track: incremental::TrackLog,
        pending: Pending,
        /// The target platform of its configuration was resolved when it
        /// was prepared (a commit in between that resolves it voids the
        /// prepare: its ctx surface saw it deferred).
        platform_known: bool,
        prepare_elapsed: std::time::Duration,
    },
    /// Its prepare failed with this (reported) error.
    Failed {
        error: String,
        effects: Effects,
        track: incremental::TrackLog,
    },
    /// Committed ahead of its turn (P4): what its turn applies.
    Committed(Box<CommittedAhead>),
}

/// A target committed ahead of its turn (P4, [`Engine::commit_ahead`]):
/// what its prepare, compute and commit would have written where the turn
/// order is observable, applied at its turn in that order — and its
/// commit's result.
struct CommittedAhead {
    prepare_effects: Effects,
    prepare_track: incremental::TrackLog,
    compute_effects: Effects,
    commit_effects: Effects,
    commit_track: incremental::TrackLog,
    /// Its planned nodes and lowerings, with its console writes between
    /// them (crate::engine::lowering::PlannedNodes::splice).
    segment: lowering::PlannedNodes,
    /// The warnings and errors its commit recorded, and the lengths of the
    /// engine's lists when it started.
    warnings: Vec<String>,
    errors: Vec<String>,
    marks: (usize, usize),
    result: Result<(), String>,
    /// Its prepare, compute and commit (the tracker's costs).
    elapsed: std::time::Duration,
    /// Its prepare saw its target platform resolved (always: a deferred
    /// one commits at its turn; checked again there).
    platform_known: bool,
    /// BAZEL_RS_VERIFY_AHEAD: the prepare repeated right before the commit
    /// ahead (equal to the one it committed), compared again at its turn.
    verified: Option<Box<(PreparedParts, ComputeSnapshot)>>,
}

/// Why an entry cannot be prepared ahead yet.
enum Classified {
    /// Never ahead: prepared at its turn.
    Never,
    /// Waiting for these keys' commits (the first is the one waited on).
    Waiting(Vec<ConfiguredTargetKey>),
    Ready,
}

/// The pipeline state of one plan_targets walk.
#[derive(Default)]
struct Pipeline {
    /// Plan B S0: why the last try to commit an entry ahead of its turn
    /// failed ([`fallback`] at its turn).
    last_try: HashMap<usize, &'static str>,
    /// Entries classified never ahead (counted at their classification).
    classified_never: std::collections::HashSet<usize>,
    /// Entries whose prepare ahead aborted.
    prepare_aborted: std::collections::HashSet<usize>,
    ahead: HashMap<usize, Ahead>,
    /// Entries before this index were classified.
    scanned: usize,
    /// Entries waiting for a key's commit, with the keys after it.
    waiting: HashMap<ConfiguredTargetKey, Vec<(usize, Vec<ConfiguredTargetKey>)>>,
    ready: OrderedSet<usize>,
    /// The most entries prepared ahead at once (twice the workers).
    capacity: usize,
    /// What discarded work off an entry's turn observed or obtained,
    /// applied at its turn (where the sequential engine's lookups that
    /// filled the memos it hit happen).
    stashed: HashMap<usize, Vec<Effects>>,
    /// P4: the classified entries not committed yet whose outputs are not
    /// known, by package (repository, package): an entry commits ahead of
    /// its turn only when no earlier one of its package, an enclosing or an
    /// enclosed package is among them (they are the only ones that may
    /// register its output paths or paths above or below them).
    pending_packages: BTreeMap<(String, String), OrderedSet<usize>>,
    /// Plan B: the classified entries not committed yet whose outputs are
    /// known (a finished compute without sentinel, prepared with its target
    /// platform resolved; a native rule without outputs), with their output
    /// paths and their ancestors indexed: a later entry commits ahead of
    /// them unless its outputs meet theirs (an exact path or a prefix).
    known: HashMap<usize, Arc<OutputSummary>>,
    known_paths: crate::fast_hash::FxHashMap<u64, OrderedSet<usize>>,
    known_ancestors: crate::fast_hash::FxHashMap<u64, OrderedSet<usize>>,
    /// The classified entries not committed yet that may register outputs
    /// in other packages (aspect applications in their prepare): no entry
    /// after one commits ahead of its turn.
    barriers: OrderedSet<usize>,
    /// Entries committed ahead of their turn.
    committed: HashMap<usize, Box<CommittedAhead>>,
    /// The nodes and lowerings their segments hold ([`committed_limit`]).
    committed_entries: usize,
    /// Entries whose compute finished, not committed yet (retried as the
    /// entries before them commit).
    finished: OrderedSet<usize>,
}

impl Pipeline {
    /// `index` (of `label`) is classified and not committed; `known`: its
    /// commit registers no output (a native rule without outputs).
    fn note_pending(&mut self, index: usize, label: &Label, barrier: bool, known: bool) {
        if known {
            self.add_known(index, Arc::new(OutputSummary::default()));
        } else {
            self.pending_packages
                .entry((label.repository.clone(), label.package.clone()))
                .or_default()
                .insert(index);
        }
        if barrier {
            self.barriers.insert(index);
        }
    }

    fn remove_unknown(&mut self, index: usize, label: &Label) -> bool {
        let package = (label.repository.clone(), label.package.clone());
        let Some(pending) = self.pending_packages.get_mut(&package) else {
            return false;
        };
        let removed = pending.remove(&index);
        if pending.is_empty() {
            self.pending_packages.remove(&package);
        }
        removed
    }

    fn add_known(&mut self, index: usize, summary: Arc<OutputSummary>) {
        for path in &summary.paths {
            self.known_paths.entry(*path).or_default().insert(index);
        }
        for ancestor in &summary.ancestors {
            self.known_ancestors
                .entry(*ancestor)
                .or_default()
                .insert(index);
        }
        self.known.insert(index, summary);
    }

    /// The uncommitted `index` (of `label`) finished its compute: what its
    /// commit registers is known from now on.
    fn note_known(&mut self, index: usize, label: &Label, summary: Arc<OutputSummary>) {
        if self.remove_unknown(index, label) {
            self.add_known(index, summary);
        }
    }

    /// `index` (of `label`) is committed.
    fn note_committed(&mut self, index: usize, label: &Label) {
        self.remove_unknown(index, label);
        if let Some(summary) = self.known.remove(&index) {
            let unindex = |map: &mut crate::fast_hash::FxHashMap<u64, OrderedSet<usize>>,
                           hashes: &[u64]| {
                for hash in hashes {
                    if let Some(indexes) = map.get_mut(hash) {
                        indexes.remove(&index);
                        if indexes.is_empty() {
                            map.remove(hash);
                        }
                    }
                }
            };
            unindex(&mut self.known_paths, &summary.paths);
            unindex(&mut self.known_ancestors, &summary.ancestors);
        }
        self.barriers.remove(&index);
    }

    /// Whether an uncommitted entry before `index` with known outputs
    /// registers one of `summary`'s paths, an ancestor of one, or a path
    /// below one.
    fn meets_known(&self, index: usize, summary: &OutputSummary) -> bool {
        let earlier = |indexes: Option<&OrderedSet<usize>>| {
            indexes.is_some_and(|indexes| indexes.first().is_some_and(|first| *first < index))
        };
        summary.paths.iter().any(|path| {
            earlier(self.known_paths.get(path)) || earlier(self.known_ancestors.get(path))
        }) || summary
            .ancestors
            .iter()
            .any(|ancestor| earlier(self.known_paths.get(ancestor)))
    }

    /// Whether an uncommitted entry before `index` may register `label`'s
    /// output paths (`summary`), or paths above or below them, and why: 0
    /// one of its package, 1 of an enclosing one, 2 of an enclosed one
    /// (outputs not known yet), 3 an entry whose known outputs meet these,
    /// 4 an aspect barrier (`None`: its outputs are settled).
    fn unsettled(&self, index: usize, label: &Label, summary: &OutputSummary) -> Option<usize> {
        if self.barriers.first().is_some_and(|first| *first < index) {
            return Some(4);
        }
        if self.meets_known(index, summary) {
            return Some(3);
        }
        let earlier =
            |pending: &OrderedSet<usize>| pending.first().is_some_and(|first| *first < index);
        let repository = &label.repository;
        // The package and every enclosing one.
        let mut package = label.package.as_str();
        loop {
            if self
                .pending_packages
                .get(&(repository.clone(), package.to_owned()))
                .is_some_and(earlier)
            {
                return Some(if package == label.package { 0 } else { 1 });
            }
            if package.is_empty() {
                break;
            }
            package = package.rsplit_once('/').map_or("", |(parent, _)| parent);
        }
        // Every enclosed one: `<package>/...` (every package of the
        // repository below its root package).
        let below = if label.package.is_empty() {
            String::new()
        } else {
            format!("{}/", label.package)
        };
        let end = format!("{}\u{10FFFF}", below);
        self.pending_packages
            .range((repository.clone(), below)..(repository.clone(), end))
            .any(|((_, package), pending)| package != &label.package && earlier(pending))
            .then_some(2)
    }
}

/// P3 counters (`BAZEL_RS_TIMING=1`: `PERF-PARALLEL`).
#[derive(Default)]
struct Stats {
    turn: AtomicU64,
    ahead: AtomicU64,
    aborted: AtomicU64,
    invalidated: AtomicU64,
    never: AtomicU64,
    failed: AtomicU64,
    recomputed: AtomicU64,
    waited_us: AtomicU64,
    lost_notes: AtomicU64,
    verified: AtomicU64,
    /// Implementations computed at their turn on the coordinator (µs).
    turn_compute_us: AtomicU64,
    /// P4: entries committed ahead of their turn.
    committed_ahead: AtomicU64,
    /// P4: tries left for later (an earlier aspect barrier, an earlier
    /// entry of a related package) and entries left for their turn.
    ahead_barrier: AtomicU64,
    ahead_package: AtomicU64,
    ahead_ineligible: AtomicU64,
    /// Tries left for later: the segments waiting for their turn are full.
    ahead_budget: AtomicU64,
    /// Committed ahead, its target platform resolved before its turn
    /// although its prepare saw it deferred (never: such entries commit at
    /// their turn).
    committed_platform_late: AtomicU64,
    /// BAZEL_RS_VERIFY_AHEAD comparisons at the turn of entries committed
    /// ahead.
    verified_turn: AtomicU64,
    unsettled: [AtomicU64; 4],
    /// BAZEL_RS_VERIFY_AHEAD: commits whose registered outputs were checked
    /// against their compute's output summary.
    verified_outputs: AtomicU64,
    /// Plan B S0: the entries run on the coordinator (or committed at their
    /// turn), by the reason (`PERF-PARALLEL fallback`).
    fallback: std::sync::Mutex<BTreeMap<&'static str, u64>>,
    shared_total: AtomicU64,
    shared_unmerged: AtomicU64,
    /// Plan B S0: off-turn work whose held effects reached the turn, by the
    /// stage and kind (`PERF-PARALLEL effects`: what a memo's first fill
    /// observed, obtained or printed off its turn).
    effects: std::sync::Mutex<BTreeMap<String, u64>>,
    /// S1: where each Starlark commit's delta was built (`worker`,
    /// `coordinator`) or why a worker's was not used (`PERF-PARALLEL delta`).
    deltas: std::sync::Mutex<BTreeMap<&'static str, (u64, u64)>>,
    /// BAZEL_RS_VERIFY_DELTA comparisons.
    verified_deltas: AtomicU64,
    /// Implementations computed at their turn, by rule class (µs, count).
    turn_classes: std::sync::Mutex<BTreeMap<String, (u64, u64)>>,
    /// Per walk size (≤16, ≤256, ≤4096, more entries): the walks' entries
    /// and their implementations computed at their turn (µs).
    walk_entries: [AtomicU64; 4],
    walk_turn_us: [AtomicU64; 4],
}

use std::sync::atomic::AtomicU64;

/// Plan B S0: count one entry for a fallback `reason`.
fn fallback(reason: &'static str) {
    if crate::plan_timings::enabled()
        && let Ok(mut counts) = stats().fallback.lock()
    {
        *counts.entry(reason).or_default() += 1;
    }
}

/// S1: count one Starlark commit's delta by where it was built or why the
/// worker's was not used (`PERF-PARALLEL delta`).
pub(super) fn note_delta(reason: &'static str, elapsed: std::time::Duration) {
    if let Ok(mut counts) = stats().deltas.lock() {
        let entry = counts.entry(reason).or_default();
        entry.0 += 1;
        entry.1 += elapsed.as_micros() as u64;
    }
}

/// S1: one BAZEL_RS_VERIFY_DELTA comparison.
pub(super) fn note_verified_delta() {
    bump(&stats().verified_deltas, 1);
}

/// Plan B S0: count what `effects` held off the turn of a `stage`.
fn note_effects(stage: &str, effects: &Effects) {
    if !crate::plan_timings::enabled() {
        return;
    }
    let Ok(mut counts) = stats().effects.lock() else {
        return;
    };
    for (kind, held) in [
        ("observations", !effects.observations.is_empty()),
        ("roots", !effects.roots.is_empty()),
        ("console", !effects.console.is_empty()),
    ] {
        if held {
            *counts.entry(format!("{stage}_{kind}")).or_default() += 1;
        }
    }
}

/// Plan B S0: a shared action whose first owner (or the registering
/// target) is off its turn: committed ahead of it, not merged in plan order
/// yet (`PERF-PARALLEL shared`).
pub(super) fn note_shared(registering: u32, owner: u32, turn: u32) {
    if !crate::plan_timings::enabled() {
        return;
    }
    bump(&stats().shared_total, 1);
    if turn != u32::MAX && (registering > turn || owner > turn) {
        bump(&stats().shared_unmerged, 1);
    }
}

fn stats() -> &'static Stats {
    static STATS: std::sync::OnceLock<Stats> = std::sync::OnceLock::new();
    STATS.get_or_init(Stats::default)
}

/// Notes printed by work that is discarded and redone (never replayed:
/// the redo prints them). None on ~/code; debug builds (the tests) fail.
fn note_lost(notes: usize) {
    bump(&stats().lost_notes, notes as u64);
    debug_assert_eq!(notes, 0, "internal error: discarded work printed notes");
}

fn bump(counter: &AtomicU64, by: u64) {
    counter.fetch_add(by, std::sync::atomic::Ordering::Relaxed);
}

/// The `PERF-PARALLEL` line (`BAZEL_RS_TIMING=1`).
pub(super) fn report(workers: usize) {
    if !crate::plan_timings::enabled() {
        return;
    }
    let stats = stats();
    let get = |counter: &AtomicU64| counter.load(std::sync::atomic::Ordering::Relaxed);
    // Diagnostics, outside any stderr capture (crate::snapshot::diag).
    crate::snapshot::diag::line_fmt(format_args!(
        "PERF-PARALLEL workers={workers} turn={} ahead={} aborted={} invalidated={} never={} failed={} recomputed={} wait={:.3}s lost_notes={} verified={}",
        get(&stats.turn),
        get(&stats.ahead),
        get(&stats.aborted),
        get(&stats.invalidated),
        get(&stats.never),
        get(&stats.failed),
        get(&stats.recomputed),
        get(&stats.waited_us) as f64 / 1e6,
        get(&stats.lost_notes),
        get(&stats.verified),
    ));
    let buckets = (0..4)
        .map(|bucket| {
            format!(
                "{}:{:.3}s",
                get(&stats.walk_entries[bucket]),
                get(&stats.walk_turn_us[bucket]) as f64 / 1e6
            )
        })
        .collect::<Vec<_>>();
    crate::snapshot::diag::line_fmt(format_args!(
        "PERF-PARALLEL committed_ahead={} later_barrier={} later_package={} ineligible={} turn_compute={:.3}s walks(<=16,<=256,<=4096,more: entries:turn_compute)={}",
        get(&stats.committed_ahead),
        get(&stats.ahead_barrier),
        get(&stats.ahead_package),
        get(&stats.ahead_ineligible),
        get(&stats.turn_compute_us) as f64 / 1e6,
        buckets.join(" ")
    ));
    crate::snapshot::diag::line_fmt(format_args!(
        "PERF-PARALLEL unsettled package={} enclosing={} enclosed={} outputs={} budget={} platform_late={} verified_turn={} verified_outputs={}",
        get(&stats.unsettled[0]),
        get(&stats.unsettled[1]),
        get(&stats.unsettled[2]),
        get(&stats.unsettled[3]),
        get(&stats.ahead_budget),
        get(&stats.committed_platform_late),
        get(&stats.verified_turn),
        get(&stats.verified_outputs)
    ));
    let line = |counts: Vec<(String, u64)>| {
        counts
            .into_iter()
            .map(|(reason, count)| format!("{reason}={count}"))
            .collect::<Vec<_>>()
            .join(" ")
    };
    let fallbacks = stats
        .fallback
        .lock()
        .map(|counts| {
            counts
                .iter()
                .map(|(reason, count)| ((*reason).to_owned(), *count))
                .collect()
        })
        .unwrap_or_default();
    crate::snapshot::diag::line_fmt(format_args!("PERF-PARALLEL fallback {}", line(fallbacks)));
    let effects = stats
        .effects
        .lock()
        .map(|counts| counts.clone().into_iter().collect())
        .unwrap_or_default();
    crate::snapshot::diag::line_fmt(format_args!("PERF-PARALLEL effects {}", line(effects)));
    let deltas = stats
        .deltas
        .lock()
        .map(|counts| {
            counts
                .iter()
                .map(|(reason, (count, us))| format!("{reason}={count}:{:.3}s", *us as f64 / 1e6))
                .collect::<Vec<_>>()
                .join(" ")
        })
        .unwrap_or_default();
    crate::snapshot::diag::line_fmt(format_args!(
        "PERF-PARALLEL delta (count:commit) {deltas} verified={}",
        get(&stats.verified_deltas)
    ));
    crate::snapshot::diag::line_fmt(format_args!(
        "PERF-PARALLEL shared total={} unmerged_owner={}",
        get(&stats.shared_total),
        get(&stats.shared_unmerged)
    ));
    let mut classes: Vec<(String, (u64, u64))> = stats
        .turn_classes
        .lock()
        .map(|classes| classes.clone().into_iter().collect())
        .unwrap_or_default();
    classes.sort_by_key(|(_, (us, _))| std::cmp::Reverse(*us));
    for (class, (us, count)) in classes.into_iter().take(12) {
        crate::snapshot::diag::line_fmt(format_args!(
            "PERF-PARALLEL turn_class {class} count={count} compute={:.3}s",
            us as f64 / 1e6
        ));
    }
}

/// BAZEL_RS_VERIFY_AHEAD's comparison ([`Engine::verify_ahead`]): the
/// prepare made ahead of its turn (`parts`, `snapshot`) against the one
/// made again (`turn_parts`, `turn_snapshot`).
fn verify_prepared(
    key: &ConfiguredTargetKey,
    parts: &PreparedParts,
    snapshot: &ComputeSnapshot,
    turn_parts: &PreparedParts,
    turn_snapshot: &ComputeSnapshot,
) {
    let fail = |what: &str, ahead: &dyn std::fmt::Debug, turn: &dyn std::fmt::Debug| -> ! {
        panic!(
            "internal error: BAZEL_RS_VERIFY_AHEAD: the prepare of {} ahead of its turn differs in {what}:\nahead: {ahead:#?}\nturn:  {turn:#?}",
            key.label
        )
    };
    let check = |what: &str, ahead: String, turn: String| {
        if ahead != turn {
            fail(what, &ahead, &turn);
        }
    };
    check(
        "the configurations",
        format!("{:?}", (parts.config, parts.exec_config)),
        format!("{:?}", (turn_parts.config, turn_parts.exec_config)),
    );
    check(
        "the output naming",
        format!(
            "{:?}",
            (&parts.output_root, &parts.action_owner, &parts.action_env)
        ),
        format!(
            "{:?}",
            (
                &turn_parts.output_root,
                &turn_parts.action_owner,
                &turn_parts.action_env
            )
        ),
    );
    check(
        "the transitive repositories",
        format!("{:?}", parts.transitive),
        format!("{:?}", turn_parts.transitive),
    );
    check(
        "the execution table",
        format!("{:?}", parts.action_execution),
        format!("{:?}", turn_parts.action_execution),
    );
    // What prepare sets on the plan's rule (the rest is the plan's
    // own, cloned): the ctx surface, C++ view, target platform,
    // toolchain and exec-group contexts.
    let prepared = |rule: &Rule| {
        rule.custom.as_ref().map(|custom| {
            format!(
                "{:?}",
                (
                    &custom.cpp,
                    &custom.target_platform,
                    &custom.toolchains,
                    &custom.exec_groups,
                    custom.use_auto_exec_groups,
                    &custom.action_exec_groups,
                    &custom.surface,
                )
            )
        })
    };
    check(
        "the rule context",
        format!("{:?}", prepared(&snapshot.rule)),
        format!("{:?}", prepared(&turn_snapshot.rule)),
    );
    // The ToolchainInfo values: the very same frozen values.
    let toolchains = |rule: &Rule| {
        let custom = rule.custom.as_ref()?;
        let mut contexts: Vec<Arc<crate::toolchain_info::ToolchainContextData>> =
            custom.toolchains.iter().cloned().collect();
        for (_, group) in custom.exec_groups.iter().flat_map(|groups| &groups.groups) {
            if let crate::exec_groups::GroupToolchains::Resolved(data) = group {
                contexts.push(Arc::clone(data));
            }
        }
        Some(
            contexts
                .iter()
                .flat_map(|context| context.toolchains.iter())
                .map(|(label, value)| (label.clone(), value.clone()))
                .collect::<Vec<_>>(),
        )
    };
    let (ahead_values, turn_values) = (toolchains(&snapshot.rule), toolchains(&turn_snapshot.rule));
    let same = match (&ahead_values, &turn_values) {
        (Some(ahead), Some(turn)) => {
            ahead.len() == turn.len()
                && ahead
                    .iter()
                    .zip(turn)
                    .all(|((a, x), (b, y))| a == b && x.value().ptr_eq(y.value()))
        }
        (None, None) => true,
        _ => false,
    };
    if !same {
        let labels = |values: &Option<Vec<(Label, starlark::values::OwnedFrozenValue)>>| {
            values
                .iter()
                .flatten()
                .map(|(label, _)| label.to_string())
                .collect::<Vec<_>>()
        };
        fail(
            "the toolchain values",
            &labels(&ahead_values),
            &labels(&turn_values),
        );
    }
    check(
        "the runfiles options and run_under",
        format!("{:?}", (&snapshot.runfiles, &snapshot.run_under)),
        format!("{:?}", (&turn_snapshot.runfiles, &turn_snapshot.run_under)),
    );
    check(
        "the dependency keys",
        format!(
            "{:?}",
            snapshot.dependency_targets.keys().collect::<Vec<_>>()
        ),
        format!(
            "{:?}",
            turn_snapshot.dependency_targets.keys().collect::<Vec<_>>()
        ),
    );
    for (map_key, view) in &snapshot.dependency_targets {
        let turn_view = turn_snapshot.dependency_targets[map_key].clone();
        verify_view(key, view, Ok(turn_view));
    }
}

/// The most nodes and lowerings the segments of entries committed ahead of
/// their turn hold at once (P4; like lowering's queue bound);
/// `BAZEL_RS_COMMITTED_LIMIT` (hidden test hook) overrides it.
fn committed_limit() -> usize {
    static LIMIT: std::sync::OnceLock<usize> = std::sync::OnceLock::new();
    *LIMIT.get_or_init(|| {
        // snapshot-unrecorded: a hidden test hook bounding what waits for its turn; the output is the same either way.
        std::env::var("BAZEL_RS_COMMITTED_LIMIT")
            .ok()
            .and_then(|value| value.parse().ok())
            .unwrap_or(16_384)
    })
}

/// `BAZEL_RS_TEST_ABORT_AHEAD=1` (hidden test hook): every commit ahead of
/// its turn ends as if it had needed a fetch, an analysis or a package
/// (the internal error its turn then reports).
fn test_abort_ahead() -> bool {
    static ABORT: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ABORT.get_or_init(|| {
        // snapshot-unrecorded: a hidden test hook failing commits ahead of their turn; never set outside the tests.
        std::env::var_os("BAZEL_RS_TEST_ABORT_AHEAD").is_some_and(|value| value == "1")
    })
}

/// Whether [`Engine::commit_ahead`] tried an entry for good or later.
enum Retry {
    Later,
    /// Committed ahead (`true`) or left for its turn.
    Done(bool),
}

/// Whether `rule`'s commit registers no output path: every native rule but
/// genrule, genquery and starlark_doc_extract (Engine::commit_native_target;
/// the runfiles support is planned for Starlark rules only).
fn registers_no_output(rule: &Rule) -> bool {
    match &rule.kind {
        RuleKind::Filegroup | RuleKind::PackageGroup(_) => true,
        RuleKind::Native(name) => !matches!(*name, "genquery" | "starlark_doc_extract"),
        RuleKind::Genrule | RuleKind::Custom => false,
    }
}

/// BAZEL_RS_VERIFY_AHEAD: run the commit of a rule its classification
/// counted as registering no output ([`registers_no_output`]: its outputs
/// known, empty), checking that it registers none.
/// `key`: the committing target's, when BAZEL_RS_VERIFY_AHEAD is on.
pub(super) fn verify_no_outputs<T>(
    key: Option<&ConfiguredTargetKey>,
    rule: &Rule,
    commit: impl FnOnce() -> T,
) -> T {
    match key {
        Some(key) if registers_no_output(rule) => {
            verify_registered(key, Some(OutputSummary::default()), commit)
        }
        _ => commit(),
    }
}

/// Whether BAZEL_RS_VERIFY_AHEAD=1 is on.
pub(super) fn verifying_ahead() -> bool {
    verify_ahead()
}

/// BAZEL_RS_VERIFY_AHEAD: the output summary to check a commit of
/// `outcome` against.
fn summary_to_verify(outcome: &mut ComputeOutcome) -> Option<OutputSummary> {
    if !verify_ahead() {
        return None;
    }
    // S1: a worker that built the delta kept the summary.
    if let Some(worker) = outcome.delta.as_mut() {
        return worker.summary.take();
    }
    outcome.analyzed.as_ref().ok().map(OutputSummary::of)
}

/// A compute's result put back in a channel of its own.
fn refilled(result: Result<WorkerOutcome, String>) -> Pending {
    let (sender, receiver) = mpsc::sync_channel(1);
    let _ = sender.send(result);
    receiver
}

/// `BAZEL_RS_NO_COMMIT_AHEAD` (hidden): every entry commits at its turn
/// (P3's pipeline only).
fn commit_ahead_enabled() -> bool {
    static ENABLED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ENABLED.get_or_init(|| {
        // snapshot-unrecorded: a diagnostics switch choosing the scheduling only; the output is the same either way.
        std::env::var_os("BAZEL_RS_NO_COMMIT_AHEAD").is_none()
    })
}

/// `BAZEL_RS_NO_WORKER_DELTAS` (hidden): every Starlark commit's delta is
/// built on the coordinator (S1; the output is the same either way).
fn worker_deltas() -> bool {
    static ENABLED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ENABLED.get_or_init(|| {
        // snapshot-unrecorded: a diagnostics switch choosing where deltas are built; the output is the same either way.
        std::env::var_os("BAZEL_RS_NO_WORKER_DELTAS").is_none()
    })
}

/// `BAZEL_RS_VERIFY_AHEAD=1` (hidden, like BAZEL_RS_VERIFY_VIEWS): every
/// target prepared ahead of its turn is prepared again at it and the two
/// snapshots compared ([`Engine::verify_ahead`]).
fn verify_ahead() -> bool {
    static VERIFY: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *VERIFY.get_or_init(|| {
        // snapshot-unrecorded: a diagnostics check of the scheduling; it changes no result (it panics on a difference).
        std::env::var_os("BAZEL_RS_VERIFY_AHEAD").is_some_and(|value| value == "1")
    })
}

/// The `BAZEL_RS_ANALYSIS_JITTER` hidden test hook: each compute on a
/// worker first sleeps a reproducible 0..N ms (0: off).
fn jitter_millis() -> u64 {
    static JITTER: std::sync::OnceLock<u64> = std::sync::OnceLock::new();
    *JITTER.get_or_init(|| {
        // snapshot-unrecorded: a hidden test hook delaying worker computes; no analysis result depends on it.
        std::env::var("BAZEL_RS_ANALYSIS_JITTER")
            .ok()
            .and_then(|value| value.parse().ok())
            .unwrap_or(0)
    })
}

impl Engine {
    /// Record the abort of work ahead of its turn (sticky) and return the
    /// error that unwinds it.
    pub(super) fn ahead_abort(&mut self, _what: &str) -> String {
        self.ahead_aborted = true;
        AHEAD_ABORT.to_owned()
    }

    /// Whether phase 2 runs rule implementations on workers.
    fn pipelining(&self) -> bool {
        self.loading_phase_threads > 1
            // snapshot-unrecorded: a diagnostics switch choosing the scheduling only; the output is the same either way.
            && std::env::var_os("BAZEL_RS_DEBUG_NO_PARALLEL_ANALYSIS").is_none()
    }

    /// The number of analysis workers: the coordinator prepares and
    /// commits, a few workers keep it busy (computes are about a third of
    /// phase 2 on ~/code).
    fn analysis_workers(&self) -> usize {
        // snapshot-unrecorded: a diagnostics override of the scheduling only; the output is the same either way.
        std::env::var("BAZEL_RS_ANALYSIS_WORKERS")
            .ok()
            .and_then(|value| value.parse::<usize>().ok())
            .unwrap_or(8)
            .clamp(1, self.loading_phase_threads.max(1))
    }

    /// Start the workers on the first compute dispatched to them (a small
    /// build, where nothing is ever ready ahead, starts none).
    fn ensure_analysis_pool(&mut self) {
        if self.analysis_pool.is_none() {
            self.analysis_pool = AnalysisPool::start(self.analysis_workers());
        }
        self.ensure_lowering_pool();
    }

    /// Stop the analysis workers (the end of a build's planning).
    pub(super) fn stop_analysis_pool(&mut self) {
        self.stop_lowering_pool();
        if let Some(pool) = self.analysis_pool.take() {
            report(pool.size);
            drop(pool);
        }
    }

    /// Phase 2 of every planned target not planned yet, in plan order,
    /// with rule implementations on workers (the pipelined
    /// [`Self::plan_targets_timed`]).
    pub(super) fn plan_targets_walk(
        &mut self,
        state: &mut ActionPlanning,
        configured: &mut ConfiguredNodes,
    ) -> Result<(), String> {
        if !self.pipelining() {
            return self.plan_targets_timed(state, configured);
        }
        let capacity = (self.analysis_workers() * 2).max(2);
        let mut pipe = Pipeline {
            scanned: state.done,
            capacity,
            ..Pipeline::default()
        };
        state.delegated_keys.clear();
        // P4: action lowerings run on lowering workers during the walk and
        // are placed in commit order before it returns
        // (crate::engine::lowering). Under --keep_going a failure must skip
        // its dependents at its own turn: lowered at the commit instead.
        // The lowering workers start with the analysis workers (a small
        // build that computes nothing ahead starts neither).
        let deferring = state.nodes.deferring;
        state.nodes.deferring = !self.keep_going;
        let (walk_start, turn_before) = (
            state.done,
            stats()
                .turn_compute_us
                .load(std::sync::atomic::Ordering::Relaxed),
        );
        let result = self.plan_targets_pipelined(state, configured, &mut pipe);
        if crate::plan_timings::enabled() {
            let entries = state.done - walk_start;
            let bucket = match entries {
                0..=16 => 0,
                17..=256 => 1,
                257..=4096 => 2,
                _ => 3,
            };
            let turn = stats()
                .turn_compute_us
                .load(std::sync::atomic::Ordering::Relaxed)
                - turn_before;
            bump(&stats().walk_entries[bucket], entries as u64);
            bump(&stats().walk_turn_us[bucket], turn);
        }
        // A failed lowering queued before the walk's error (or its end) is
        // the first error.
        let drained = self.drain_lowerings(state, lowering::Drain::All);
        state.nodes.deferring = deferring;
        let result = drained.and(result);
        state.delegated_keys.clear();
        state.artifact_index.walk_done();
        state.producers.set_stamp(None);
        state.producers.turn = u32::MAX;
        if result.is_err() {
            // The build stops here: the computes still queued for turns
            // that never come are skipped, the running ones joined.
            drop(pipe);
            self.stop_analysis_pool();
        }
        result
    }

    fn plan_targets_pipelined(
        &mut self,
        state: &mut ActionPlanning,
        configured: &mut ConfiguredNodes,
        pipe: &mut Pipeline,
    ) -> Result<(), String> {
        while let Some((key, rule)) = self
            .planning_plan
            .as_ref()
            .and_then(|plan| plan.get(state.done))
            .cloned()
        {
            // Place the lowerings done since (a failed one is the first
            // error), and keep the queue bounded.
            self.drain_lowerings(state, lowering::Drain::Bounded)?;
            let index = state.done;
            state.done += 1;
            // Registrations made ahead of their turn become visible at it.
            state.artifact_index.at_turn(index);
            state.producers.set_stamp(Some(index));
            state.producers.turn = state.producers.stamp;
            crate::progress::analysis_configured(state.done);
            self.flush_delegations(state);
            self.pipeline_wake(state, pipe);
            self.commit_finished_ahead(state, configured, pipe, index, false);
            let classified = index < pipe.scanned;
            let ahead = pipe
                .ahead
                .remove(&index)
                .or_else(|| pipe.committed.remove(&index).map(Ahead::Committed));
            let was_ready = pipe.ready.contains(&index);
            if crate::plan_timings::enabled() {
                self.note_turn_fallback(
                    pipe, index, &key, &rule, classified, was_ready, &ahead, configured, state,
                );
            }
            pipe.ready.remove(&index);
            pipe.finished.remove(&index);
            for effects in pipe.stashed.remove(&index).unwrap_or_default() {
                effects.apply(self, false);
            }
            if self.keep_going && self.keep_going_state.dependency_failed(&key) {
                // Skipped before its implementation (docs/KEEP_GOING.md):
                // what a prepare ahead held is dropped with it.
                drop(ahead);
                self.keep_going_state.fail_propagated(&key);
                continue;
            }
            let result = self.pipelined_turn(state, configured, pipe, &key, &rule, index, ahead);
            pipe.note_committed(index, &key.label);
            if !self.keep_going {
                result?;
            } else {
                match result {
                    Ok(()) => self.keep_going_state.planned(&key),
                    Err(error) => self.keep_going_state.fail_own(&key, error),
                }
            }
            self.pipeline_committed(state, pipe, &key);
        }
        self.flush_delegations(state);
        Ok(())
    }

    /// Plan B S0: why entry `index` runs (or commits) at its turn.
    #[allow(clippy::too_many_arguments)]
    fn note_turn_fallback(
        &self,
        pipe: &Pipeline,
        index: usize,
        key: &ConfiguredTargetKey,
        rule: &Rule,
        classified: bool,
        was_ready: bool,
        ahead: &Option<Ahead>,
        configured: &ConfiguredNodes,
        state: &ActionPlanning,
    ) {
        let reason = match ahead {
            Some(Ahead::Committed(_)) => "committed_ahead",
            Some(Ahead::Failed { .. }) => "prepare_failed",
            Some(Ahead::Computing { .. }) if !self.commits_ahead(state) => "commit_ahead_off",
            Some(Ahead::Computing { .. }) => pipe.last_try.get(&index).copied().unwrap_or("late"),
            None if pipe.classified_never.contains(&index) => return,
            None if !classified => {
                if !matches!(rule.kind, RuleKind::Custom) {
                    "native_unclassified"
                } else if configured
                    .get(key)
                    .is_some_and(|node| node.incompatible.is_some())
                {
                    "incompatible_unclassified"
                } else {
                    "unclassified"
                }
            }
            None if pipe.prepare_aborted.contains(&index) => "prepare_aborted",
            None if was_ready => "ready_late",
            None => "dependency_late",
        };
        fallback(reason);
    }

    /// Wake the entries waiting for the delegations flushed since.
    fn pipeline_wake(&mut self, state: &mut ActionPlanning, pipe: &mut Pipeline) {
        for key in std::mem::take(&mut state.delegated_keys) {
            self.pipeline_committed(state, pipe, &key);
        }
    }

    /// `key` is committed: recheck the entries waiting for it.
    fn pipeline_committed(
        &mut self,
        state: &ActionPlanning,
        pipe: &mut Pipeline,
        key: &ConfiguredTargetKey,
    ) {
        let Some(waiters) = pipe.waiting.remove(key) else {
            return;
        };
        for (index, rest) in waiters {
            if index < state.done {
                continue;
            }
            Self::wait_or_ready(state, pipe, index, rest);
        }
    }

    /// Park `index` on the first of `keys` not committed, or mark it ready.
    fn wait_or_ready(
        state: &ActionPlanning,
        pipe: &mut Pipeline,
        index: usize,
        mut keys: Vec<ConfiguredTargetKey>,
    ) {
        while let Some(key) = keys.pop() {
            if !state.dry.contains_key(&key) {
                pipe.waiting.entry(key).or_default().push((index, keys));
                return;
            }
        }
        pipe.ready.insert(index);
    }

    /// One plan entry at its turn.
    #[allow(clippy::too_many_arguments)]
    fn pipelined_turn(
        &mut self,
        state: &mut ActionPlanning,
        configured: &mut ConfiguredNodes,
        pipe: &mut Pipeline,
        key: &ConfiguredTargetKey,
        rule: &Arc<Rule>,
        index: usize,
        ahead: Option<Ahead>,
    ) -> Result<(), String> {
        let simple = ahead.is_none()
            && (configured
                .get(key)
                .is_some_and(|node| node.incompatible.is_some())
                || !matches!(rule.kind, RuleKind::Custom));
        if simple {
            // Prepare others while this one plans (native rules commit on
            // the coordinator, as sequentially).
            self.fill_lookahead(state, configured, pipe, index);
            return self.plan_one_target(state, key, rule, configured, index as u64);
        }
        let depth = self.track_depth();
        self.track_planning(key);
        let started = self.tracking().then(std::time::Instant::now);
        let mut off_turn = std::time::Duration::ZERO;
        let result = self.pipelined_turn_body(
            state,
            configured,
            pipe,
            key,
            rule,
            index,
            ahead,
            &mut off_turn,
        );
        if let Some(started) = started {
            self.track_cost(started.elapsed() + off_turn);
        }
        self.track_restore(depth);
        result
    }

    #[allow(clippy::too_many_arguments)]
    fn pipelined_turn_body(
        &mut self,
        state: &mut ActionPlanning,
        configured: &mut ConfiguredNodes,
        pipe: &mut Pipeline,
        key: &ConfiguredTargetKey,
        rule: &Arc<Rule>,
        index: usize,
        ahead: Option<Ahead>,
        off_turn: &mut std::time::Duration,
    ) -> Result<(), String> {
        use crate::plan_timings::{PlanStage, StageTimer};
        let profiled = crate::profile::active().then(std::time::Instant::now);
        let class = crate::plan_timings::class_id(rule.rule_class_name());
        let (parts, pending) = match ahead {
            Some(Ahead::Failed {
                error,
                effects,
                track,
            }) => {
                self.track_replay(track);
                effects.apply(self, true);
                return Err(error);
            }
            Some(Ahead::Committed(committed)) => {
                // P4: everything it held, in the order its turn would have
                // written it; its nodes and lowerings join the plan here.
                let CommittedAhead {
                    prepare_effects,
                    prepare_track,
                    compute_effects,
                    commit_effects,
                    commit_track,
                    segment,
                    warnings,
                    errors,
                    marks,
                    result,
                    elapsed,
                    platform_known,
                    verified,
                } = *committed;
                pipe.committed_entries -= segment.entries();
                if !platform_known && self.platform_resolved(key) {
                    // Never committed ahead (Self::commit_ahead); the turn
                    // would have prepared it again.
                    bump(&stats().committed_platform_late, 1);
                    debug_assert!(
                        false,
                        "internal error: {} was committed ahead with a deferred target platform resolved before its turn",
                        key.label
                    );
                }
                if let Some(verified) = verified {
                    // BAZEL_RS_VERIFY_AHEAD: the prepare its turn repeats is
                    // the one it committed ahead.
                    let (parts, snapshot) = *verified;
                    self.verify_ahead(state, configured, key, rule, &parts, &snapshot);
                    bump(&stats().verified_turn, 1);
                }
                self.track_replay(prepare_track);
                prepare_effects.apply(self, true);
                compute_effects.apply(self, true);
                self.track_replay(commit_track);
                *off_turn += elapsed;
                let now = (self.warnings.len(), self.errors.len());
                self.warnings.extend(warnings);
                self.errors.extend(errors);
                commit_effects.apply(self, true);
                state.nodes.splice(segment, marks, now);
                return result;
            }
            Some(Ahead::Computing {
                parts,
                effects,
                track,
                pending,
                platform_known,
                prepare_elapsed,
            }) if platform_known || !self.platform_resolved(key) => {
                self.track_replay(track);
                effects.apply(self, true);
                *off_turn += prepare_elapsed;
                (*parts, Compute::Worker(pending))
            }
            Some(Ahead::Computing {
                effects, pending, ..
            }) => {
                // Invalidated: a commit since resolved the target platform
                // its ctx surface saw deferred. Prepared again here.
                bump(&stats().invalidated, 1);
                if !effects.console.is_empty() {
                    note_lost(effects.console.len());
                }
                effects.apply(self, false);
                drop(pending);
                self.prepare_at_turn(state, configured, key, rule, index, class)?
            }
            None => self.prepare_at_turn(state, configured, key, rule, index, class)?,
        };
        // Prepare others ahead while this one computes.
        self.fill_lookahead(state, configured, pipe, index);
        let pending = match pending {
            Compute::Worker(pending) => pending,
            Compute::Inline(snapshot) => {
                // At its turn: computed here, exactly as sequentially,
                // while the workers compute the targets prepared ahead.
                let timer = StageTimer::start(PlanStage::Compute, index as u64, class);
                let started = std::time::Instant::now();
                let (snapshot, outcome) = Engine::compute_target(*snapshot);
                *off_turn += started.elapsed();
                bump(
                    &stats().turn_compute_us,
                    started.elapsed().as_micros() as u64,
                );
                if crate::plan_timings::enabled()
                    && let Ok(mut classes) = stats().turn_classes.lock()
                {
                    let entry = classes
                        .entry(rule.rule_class_name().to_owned())
                        .or_default();
                    entry.0 += started.elapsed().as_micros() as u64;
                    entry.1 += 1;
                }
                timer.finish();
                return self.commit_turn(
                    state, configured, key, rule, index, class, parts, snapshot, outcome, profiled,
                );
            }
        };
        let waited = std::time::Instant::now();
        let received = loop {
            match pending.try_recv() {
                Ok(received) => break Ok(received),
                Err(mpsc::TryRecvError::Disconnected) => break Err(mpsc::RecvError),
                Err(mpsc::TryRecvError::Empty) => {
                    // P4: commit the finished entries ahead of their turn
                    // (and prepare what they unblock) while this one runs.
                    if !self.commit_finished_ahead(state, configured, pipe, index, true) {
                        break pending.recv();
                    }
                }
            }
        };
        bump(&stats().waited_us, waited.elapsed().as_micros() as u64);
        let worker = match received {
            Ok(Ok(worker)) => worker,
            // The panic the sequential engine raises at this compute.
            Ok(Err(panic)) => panic!("{panic}"),
            Err(_) => return Err("internal error: an analysis worker stopped".to_owned()),
        };
        let WorkerOutcome {
            snapshot,
            outcome,
            effects,
            sentinel,
            elapsed,
        } = worker;
        *off_turn += elapsed;
        if verify_ahead() {
            self.verify_ahead(state, configured, key, rule, &parts, &snapshot);
        }
        let (snapshot, outcome) = if sentinel
            && !outcome
                .analyzed
                .as_ref()
                .is_err_and(|error| crate::repository_demand::is_needs_repository(error))
        {
            // A sentinel some caller swallowed: the compute depended on a
            // demand it did not make. Run again here, as sequentially.
            bump(&stats().recomputed, 1);
            if !effects.console.is_empty() {
                note_lost(effects.console.len());
            }
            effects.apply(self, false);
            let timer = StageTimer::start(PlanStage::CommitRetry, index as u64, class);
            let recomputed = Engine::compute_target(snapshot);
            timer.finish();
            recomputed
        } else {
            note_effects("compute", &effects);
            effects.apply(self, true);
            (snapshot, outcome)
        };
        self.commit_turn(
            state, configured, key, rule, index, class, parts, snapshot, outcome, profiled,
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn commit_turn(
        &mut self,
        state: &mut ActionPlanning,
        configured: &mut ConfiguredNodes,
        key: &ConfiguredTargetKey,
        rule: &Arc<Rule>,
        index: usize,
        class: u32,
        parts: PreparedParts,
        snapshot: ComputeSnapshot,
        outcome: ComputeOutcome,
        profiled: Option<std::time::Instant>,
    ) -> Result<(), String> {
        use crate::plan_timings::{PlanStage, StageTimer};
        let prepared = parts.join(snapshot);
        let timer = StageTimer::start(PlanStage::Commit, index as u64, class);
        let mut outcome = outcome;
        let committed = verify_registered(key, summary_to_verify(&mut outcome), || {
            self.commit_target(state, configured, rule, prepared, Some(outcome))
        });
        timer.finish();
        committed?;
        if let Some(started) = profiled {
            crate::profile::span(
                "configured target",
                key.label.to_string(),
                started,
                vec![("rule", rule.rule_class_name().to_owned())],
            );
        }
        Ok(())
    }

    /// BAZEL_RS_VERIFY_AHEAD=1: prepare `key` again at its turn, as the
    /// sequential engine does, and check that the prepare made ahead of its
    /// turn built the same snapshot (the compute is a pure function of it):
    /// the rule with its ctx surface, toolchain contexts and platform, every
    /// dependency view (BAZEL_RS_VERIFY_VIEWS' comparison), the output
    /// naming, the transitive repositories and the execution table.
    fn verify_ahead(
        &mut self,
        state: &mut ActionPlanning,
        configured: &mut ConfiguredNodes,
        key: &ConfiguredTargetKey,
        rule: &Arc<Rule>,
        parts: &PreparedParts,
        snapshot: &ComputeSnapshot,
    ) {
        let fail = |what: &str, ahead: &dyn std::fmt::Debug, turn: &dyn std::fmt::Debug| -> ! {
            panic!(
                "internal error: BAZEL_RS_VERIFY_AHEAD: the prepare of {} ahead of its turn differs in {what}:\nahead: {ahead:#?}\nturn:  {turn:#?}",
                key.label
            )
        };
        let again = match self.prepare_target(state, key, rule, configured) {
            Ok(prepared) => prepared,
            Err(error) => fail("its outcome", &"Ok", &error),
        };
        let Some((turn_parts, turn_snapshot)) = PreparedParts::split(again) else {
            fail("its kind", &"Custom", &"another kind")
        };
        verify_prepared(key, parts, snapshot, &turn_parts, &turn_snapshot);
        bump(&stats().verified, 1);
    }

    /// Prepare the target at its turn (everything before it committed)
    /// and dispatch its compute.
    #[allow(clippy::too_many_arguments)]
    fn prepare_at_turn(
        &mut self,
        state: &mut ActionPlanning,
        configured: &mut ConfiguredNodes,
        key: &ConfiguredTargetKey,
        rule: &Arc<Rule>,
        index: usize,
        class: u32,
    ) -> Result<(PreparedParts, Compute), String> {
        use crate::plan_timings::{PlanStage, StageTimer};
        bump(&stats().turn, 1);
        let timer = StageTimer::start(PlanStage::Prepare, index as u64, class);
        let prepared = self.prepare_target(state, key, rule, configured)?;
        timer.finish();
        match PreparedParts::split(prepared) {
            Some((parts, snapshot)) => Ok((parts, Compute::Inline(Box::new(snapshot)))),
            // prepare_target prepares every RuleKind::Custom as Custom.
            None => Err(format!(
                "internal error: the Starlark rule {} prepared without a compute",
                key.label
            )),
        }
    }

    /// Run `snapshot`'s compute on a worker (inline when the pool is gone).
    fn dispatch(
        &self,
        snapshot: ComputeSnapshot,
        transitive: BTreeSet<String>,
        index: usize,
        class: u32,
        delta: Option<Box<delta::DeltaInputs>>,
    ) -> Pending {
        let (sender, receiver) = mpsc::sync_channel(1);
        let jitter = jitter_millis();
        let cancelled = self
            .analysis_pool
            .as_ref()
            .map(|pool| Arc::clone(&pool.cancelled));
        let finished = self
            .analysis_pool
            .as_ref()
            .map(|pool| pool.finished.0.clone());
        let compact_repo_mapping = self.compact_repo_mapping;
        let verify_summary = verify_ahead();
        let job: Job = Box::new(move || {
            if cancelled
                .as_ref()
                .is_some_and(|flag| flag.load(std::sync::atomic::Ordering::SeqCst))
            {
                // The pool stopped (a failing build): nobody waits for it.
                return;
            }
            let mut summary = None;
            let summary_slot = &mut summary;
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                let summary = summary_slot;
                if jitter > 0 {
                    // Interrupted when the pool stops (a failing build).
                    let roll = loading_jitter_roll(index as u64, 0x5033) % (jitter + 1);
                    let until = std::time::Instant::now() + std::time::Duration::from_millis(roll);
                    while std::time::Instant::now() < until
                        && !cancelled
                            .as_ref()
                            .is_some_and(|flag| flag.load(std::sync::atomic::Ordering::SeqCst))
                    {
                        std::thread::sleep(std::time::Duration::from_millis(5));
                    }
                }
                let isolation = Isolation::enter();
                let started = std::time::Instant::now();
                let timer = crate::plan_timings::StageTimer::start(
                    crate::plan_timings::PlanStage::Compute,
                    index as u64,
                    class,
                );
                let (snapshot, mut outcome) = Engine::compute_target(snapshot);
                timer.finish();
                // The pure part of the runfiles support too (commit's
                // plan_runfiles_support uses it), unless it would demand.
                // P4: the repo mapping manifests its actions write
                // (py_internal.create_repo_mapping_manifest), as the commit
                // would (Engine::resolve_repo_mapping), unless that demands.
                if let Ok(analyzed) = &mut outcome.analyzed {
                    for registered in &mut analyzed.actions {
                        let (Some(runfiles), Some(_)) =
                            (&registered.repo_mapping, &registered.action.write)
                        else {
                            continue;
                        };
                        let sentinels = crate::repository_demand::sentinels_raised();
                        let repositories = &snapshot.repositories;
                        let content = crate::runfiles_tree::repo_mapping_manifest(
                            runfiles,
                            &transitive,
                            &|repository: &str| repositories.mapping(repository),
                            compact_repo_mapping,
                        );
                        if let Ok(content) = content
                            && crate::repository_demand::sentinels_raised() == sentinels
                            && let Some(write) = registered.action.write.as_mut()
                        {
                            write.content = content;
                            registered.repo_mapping = None;
                        }
                    }
                }
                // Plan B: the commit's check of the declared outputs too
                // (validate_declared_outputs), unless it would demand. Its
                // lookups are the commit's (Engine::ignored is this
                // RepositorySet's ignored_subdirectories: the first fill
                // captures its observations and every hit replays them and
                // notes the root obtained), recorded in this isolation and
                // applied at the target's turn.
                if let Ok(analyzed) = &outcome.analyzed {
                    let sentinels = crate::repository_demand::sentinels_raised();
                    let label = &snapshot.rule.label;
                    let repositories = &snapshot.repositories;
                    let checked = validate_declared_outputs(
                        label,
                        &snapshot.output_root,
                        &analyzed.declared,
                        &|| {
                            Ok((
                                repositories.root(&label.repository)?.to_path_buf(),
                                repositories.ignored_subdirectories(&label.repository)?,
                            ))
                        },
                    );
                    if crate::repository_demand::sentinels_raised() == sentinels {
                        outcome.outputs_checked = Some(checked);
                    }
                }
                if let Ok(analyzed) = &outcome.analyzed {
                    let sentinels = crate::repository_demand::sentinels_raised();
                    let precomputed = RunfilesPrecomputed::compute(
                        &analyzed.runfiles,
                        &transitive,
                        &snapshot.repositories,
                        &snapshot.runfiles,
                    );
                    if crate::repository_demand::sentinels_raised() == sentinels {
                        outcome.runfiles = precomputed;
                    }
                }
                // What its commit registers (unless it runs again inline).
                let registers = match &outcome.analyzed {
                    Ok(analyzed) => OutputSummary::of(analyzed),
                    // Its commit reports the error and registers nothing.
                    Err(_) => OutputSummary::default(),
                };
                // S1: its commit, as a delta merged by the coordinator.
                if let Some(inputs) = delta
                    && outcome.analyzed.is_ok()
                {
                    let built =
                        inputs.build(&snapshot, &mut outcome, index, verify_summary, &registers);
                    outcome.delta = Some(Box::new(built));
                }
                let elapsed = started.elapsed();
                let (effects, sentinel) = isolation.finish();
                *summary = (!sentinel).then(|| Arc::new(registers));
                WorkerOutcome {
                    snapshot,
                    outcome,
                    effects,
                    sentinel,
                    elapsed,
                }
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
                let _ = finished.send((index, summary));
            }
        });
        if let Some(pool) = &self.analysis_pool
            && let Err(job) = pool.submit(job)
        {
            job();
        }
        receiver
    }

    /// Whether the target platform of `key`'s configuration is resolved
    /// (Engine::platforms, which prepare's ctx surface reads).
    fn platform_resolved(&self, key: &ConfiguredTargetKey) -> bool {
        let target = ConfiguredTargetKey::new(
            self.configurations
                .get(key.config)
                .native
                .target_platform()
                .clone(),
            key.config,
        );
        self.platforms.contains_key(&target)
    }

    /// Classify the entries entering the horizon and prepare the ready ones
    /// ahead while the pipeline has room. `turn`: the index of the entry
    /// at its turn (everything before it is committed, it is not).
    fn fill_lookahead(
        &mut self,
        state: &mut ActionPlanning,
        configured: &mut ConfiguredNodes,
        pipe: &mut Pipeline,
        turn: usize,
    ) {
        let plan_len = self.planning_plan.as_ref().map_or(0, Vec::len);
        let horizon = plan_len.min(turn + 1 + HORIZON);
        if pipe.scanned < turn + 1 {
            pipe.scanned = turn + 1;
        }
        while pipe.ahead.len() < pipe.capacity {
            if pipe.ready.is_empty() && pipe.scanned < horizon {
                let index = pipe.scanned;
                pipe.scanned += 1;
                let Some((key, rule)) = self
                    .planning_plan
                    .as_ref()
                    .and_then(|plan| plan.get(index))
                    .cloned()
                else {
                    break;
                };
                let mut stash = Vec::new();
                let classified = self.classify(state, configured, &key, &rule, &mut stash);
                if !stash.is_empty() {
                    pipe.stashed.entry(index).or_default().extend(stash);
                }
                // A Starlark rule never prepared ahead may apply aspects
                // (outputs in other packages) at its turn.
                let barrier = matches!(classified, Classified::Never)
                    && matches!(rule.kind, RuleKind::Custom);
                pipe.note_pending(index, &key.label, barrier, registers_no_output(&rule));
                match classified {
                    Classified::Never => {
                        bump(&stats().never, 1);
                        pipe.classified_never.insert(index);
                    }
                    Classified::Waiting(keys) => Self::wait_or_ready(state, pipe, index, keys),
                    Classified::Ready => {
                        pipe.ready.insert(index);
                    }
                }
                continue;
            }
            let Some(index) = pipe.ready.pop_first() else {
                break;
            };
            if index <= turn {
                continue;
            }
            let Some((key, rule)) = self
                .planning_plan
                .as_ref()
                .and_then(|plan| plan.get(index))
                .cloned()
            else {
                continue;
            };
            let mut stash = Vec::new();
            match self.prepare_ahead(state, configured, &key, &rule, index, &mut stash) {
                Some(ahead) => {
                    pipe.ahead.insert(index, ahead);
                }
                None => {
                    pipe.prepare_aborted.insert(index);
                }
            }
            if !stash.is_empty() {
                pipe.stashed.entry(index).or_default().extend(stash);
            }
        }
    }

    /// Whether `key` may be prepared ahead, and on which commits it waits:
    /// a compatible Starlark rule without attribute aspects whose every
    /// dependency prepare views (crate::engine::Engine::custom_dependencies,
    /// and the resolved toolchain implementations) is committed — its
    /// planning-state entries (`dry` last) are final.
    fn classify(
        &mut self,
        state: &ActionPlanning,
        configured: &ConfiguredNodes,
        key: &ConfiguredTargetKey,
        rule: &Arc<Rule>,
        stash: &mut Vec<Effects>,
    ) -> Classified {
        let never = |reason: &'static str| {
            fallback(reason);
            Classified::Never
        };
        let Some(custom) = rule.custom.as_ref() else {
            return never("native");
        };
        if !matches!(rule.kind, RuleKind::Custom) {
            return never("native");
        }
        if configured
            .get(key)
            .is_none_or(|node| node.incompatible.is_some())
        {
            return never("incompatible");
        }
        if custom
            .schema
            .values()
            .any(|spec| !spec.declaration.aspects.is_empty())
        {
            return never("attribute_aspects");
        }
        if self.keep_going && self.keep_going_state.dependency_failed(key) {
            return never("keep_going");
        }
        // Lookups only (every dependency is analyzed): their tracker events
        // are the prepare's own and recur there; nothing else is written.
        self.ahead = true;
        self.ahead_aborted = false;
        self.track_defer_start();
        let isolation = Isolation::enter();
        let keys = self.ahead_requirements(key, rule);
        let (effects, sentinel) = isolation.finish();
        drop(self.track_defer_finish());
        self.ahead = false;
        let aborted = std::mem::take(&mut self.ahead_aborted);
        let quiet = effects.console.is_empty()
            && effects.observations.is_empty()
            && effects.roots.is_empty();
        if !quiet {
            // Lookups that observed or obtained something: the turn's
            // prepare repeats them (a memo hit then), so what they
            // observed counts there.
            if !effects.console.is_empty() {
                note_lost(effects.console.len());
            }
            note_effects("classify", &effects);
            stash.push(effects);
        }
        match keys {
            Some(keys) if !aborted && !sentinel && quiet => {
                let missing: Vec<ConfiguredTargetKey> = keys
                    .into_iter()
                    .filter(|key| !state.dry.contains_key(key))
                    .collect();
                if missing.is_empty() {
                    Classified::Ready
                } else {
                    Classified::Waiting(missing)
                }
            }
            _ => never("requirements"),
        }
    }

    /// The committed keys whose planning state `key`'s prepare reads.
    fn ahead_requirements(
        &mut self,
        key: &ConfiguredTargetKey,
        rule: &Rule,
    ) -> Option<Vec<ConfiguredTargetKey>> {
        let edges = self.rule_edges(rule, key.config);
        let exec_config = self
            .exec_configurations
            .get(key)
            .copied()
            .unwrap_or(key.config);
        let dependencies = self
            .custom_dependencies(rule, key, &edges, exec_config)
            .ok()?;
        if !dependencies.edge_aspects.is_empty() {
            return None;
        }
        let mut labels: Vec<(Label, ConfigId)> = dependencies
            .labels
            .into_iter()
            .map(|(label, config, _, _)| (label, config))
            .collect();
        let groups = self.exec_group_contexts.get(key).cloned();
        for context in std::iter::once(self.toolchain_contexts.get(key).cloned())
            .chain(
                groups
                    .iter()
                    .flat_map(|groups| groups.groups.iter().map(|group| group.context.clone())),
            )
            .flatten()
        {
            for (_, implementation) in &context.resolved {
                labels.push((implementation.clone(), key.config));
            }
        }
        let mut keys = Vec::with_capacity(labels.len());
        for (label, config) in labels {
            match self.target(&label).ok()? {
                Target::Source(_) => {}
                Target::Output(generating, _) => {
                    keys.push(ConfiguredTargetKey::new(generating.label.clone(), config));
                }
                Target::Rule(_) => keys.push(ConfiguredTargetKey::new(label, config)),
            }
        }
        // Waited for in reverse (Self::wait_or_ready pops): the first
        // dependency first.
        keys.reverse();
        Some(keys)
    }

    /// P4: whether finished entries may commit ahead of their turn (a
    /// walk that defers its lowerings, so their nodes join the plan at
    /// their turn; an execution plan without --keep_going, whose failures
    /// skip dependents at the failing turn, and without an aquery capture).
    fn commits_ahead(&self, state: &ActionPlanning) -> bool {
        state.nodes.deferring
            && !self.keep_going
            && !self.semantic_only()
            && self.aquery_capture.is_none()
            && commit_ahead_enabled()
    }

    /// P4: commit the entries whose computes finished, ahead of their turn
    /// where their outputs are settled ([`Pipeline::unsettled`]), and
    /// prepare what they unblock. `block`: wait for a compute to finish
    /// first. `false` when nothing can finish (no workers).
    fn commit_finished_ahead(
        &mut self,
        state: &mut ActionPlanning,
        configured: &mut ConfiguredNodes,
        pipe: &mut Pipeline,
        turn: usize,
        block: bool,
    ) -> bool {
        let Some(pool) = self.analysis_pool.as_ref() else {
            return false;
        };
        let mut received = Vec::new();
        if block {
            match pool.finished.1.recv() {
                Ok(finished) => received.push(finished),
                Err(_) => return false,
            }
        }
        while let Ok(finished) = pool.finished.1.try_recv() {
            received.push(finished);
        }
        for (index, summary) in received {
            if index <= turn {
                continue;
            }
            pipe.finished.insert(index);
            // Its outputs are known when its turn commits this compute: no
            // sentinel, and a prepare that saw its target platform resolved
            // (else its turn may prepare it again).
            if let (Some(summary), Some(Ahead::Computing { platform_known, .. })) =
                (summary, pipe.ahead.get(&index))
                && *platform_known
                && let Some((key, _)) = self.planning_plan.as_ref().and_then(|plan| plan.get(index))
            {
                pipe.note_known(index, &key.label, summary);
            }
        }
        if !self.commits_ahead(state) {
            pipe.finished.clear();
            return true;
        }
        let candidates: Vec<usize> = pipe.finished.iter().copied().collect();
        let mut committed = false;
        for index in candidates {
            if index <= turn {
                pipe.finished.remove(&index);
                continue;
            }
            match self.commit_ahead(state, configured, pipe, index) {
                // Settled later (an earlier entry of its packages commits).
                Retry::Later => {}
                Retry::Done(done) => {
                    committed |= done;
                    pipe.finished.remove(&index);
                }
            }
        }
        if committed {
            self.fill_lookahead(state, configured, pipe, turn);
        }
        true
    }

    /// Commit the finished entry `index` ahead of its turn
    /// ([`CommittedAhead`]), when its commit cannot depend on the turn
    /// order: its outputs are settled, it needs nothing registered on first
    /// need (workspace status, tool runfiles groups, shared input runs) and
    /// nothing its commit would fetch, analyze or load.
    fn commit_ahead(
        &mut self,
        state: &mut ActionPlanning,
        configured: &mut ConfiguredNodes,
        pipe: &mut Pipeline,
        index: usize,
    ) -> Retry {
        use crate::plan_timings::{PlanStage, StageTimer};
        let Some((key, rule)) = self
            .planning_plan
            .as_ref()
            .and_then(|plan| plan.get(index))
            .cloned()
        else {
            return Retry::Done(false);
        };
        match pipe.ahead.get(&index) {
            Some(Ahead::Computing { platform_known, .. }) => {
                // A prepare that saw its target platform deferred is
                // prepared again at its turn if a commit resolves it before
                // (Self::pipelined_turn_body): it commits at its turn.
                if !platform_known {
                    pipe.last_try.insert(index, "platform_unknown");
                    return Retry::Done(false);
                }
            }
            _ => return Retry::Done(false),
        }
        if pipe.barriers.first().is_some_and(|first| *first < index) {
            bump(&stats().ahead_barrier, 1);
            pipe.last_try.insert(index, "unsettled_barrier");
            return Retry::Later;
        }
        // What waits for its turn is bounded (nodes and lowerings of the
        // entries committed ahead), like the lowering queue.
        if pipe.committed_entries >= committed_limit() {
            bump(&stats().ahead_budget, 1);
            pipe.last_try.insert(index, "budget");
            return Retry::Later;
        }
        let Some(summary) = pipe.known.get(&index).cloned() else {
            // Its outputs are not known (a sentinel): at its turn.
            pipe.last_try.insert(index, "outputs_unknown");
            return Retry::Done(false);
        };
        if let Some(reason) = pipe.unsettled(index, &key.label, &summary) {
            bump(&stats().ahead_package, 1);
            bump(&stats().unsettled[reason.min(3)], 1);
            pipe.last_try.insert(
                index,
                [
                    "unsettled_package",
                    "unsettled_enclosing",
                    "unsettled_enclosed",
                    "unsettled_outputs",
                ][reason.min(3)],
            );
            return Retry::Later;
        }
        let Some(Ahead::Computing {
            parts,
            effects: prepare_effects,
            track: prepare_track,
            pending,
            platform_known,
            prepare_elapsed,
        }) = pipe.ahead.remove(&index)
        else {
            return Retry::Done(false);
        };
        let put_back = |pipe: &mut Pipeline,
                        parts: Box<PreparedParts>,
                        effects: Effects,
                        track: incremental::TrackLog,
                        pending: Pending| {
            pipe.ahead.insert(
                index,
                Ahead::Computing {
                    parts,
                    effects,
                    track,
                    pending,
                    platform_known,
                    prepare_elapsed,
                },
            );
        };
        let worker = match pending.try_recv() {
            Ok(Ok(worker)) => worker,
            Err(mpsc::TryRecvError::Empty) => {
                put_back(pipe, parts, prepare_effects, prepare_track, pending);
                return Retry::Later;
            }
            Ok(Err(panic)) => {
                put_back(
                    pipe,
                    parts,
                    prepare_effects,
                    prepare_track,
                    refilled(Err(panic)),
                );
                return Retry::Done(false);
            }
            Err(mpsc::TryRecvError::Disconnected) => {
                put_back(pipe, parts, prepare_effects, prepare_track, pending);
                return Retry::Done(false);
            }
        };
        // S1: a delta meeting a producer registered since it was built is
        // left to the coordinator: computed again on a worker, which then
        // keeps the analysis result for it (no compute here at the merge).
        if let Some(delta::Built::Delta(built)) =
            worker.outcome.delta.as_ref().map(|worker| &worker.built)
            && Self::delta_meets_producers(state, built)
        {
            let WorkerOutcome {
                snapshot, effects, ..
            } = worker;
            if !effects.console.is_empty() {
                note_lost(effects.console.len());
            }
            pipe.stashed.entry(index).or_default().push(effects);
            let class = crate::plan_timings::class_id(rule.rule_class_name());
            let delta = self.delta_inputs(state, &rule, &parts, class);
            let pending = self.dispatch(snapshot, parts.transitive.clone(), index, class, delta);
            put_back(pipe, parts, prepare_effects, prepare_track, pending);
            pipe.last_try.insert(index, "delta_meets_producer");
            note_delta("recomputed_on_worker", std::time::Duration::ZERO);
            return Retry::Done(false);
        }
        let unsafe_reason = if worker.sentinel {
            Some("sentinel")
        } else if let Some(built) = worker.outcome.delta.as_ref() {
            // S1: decided on the worker, before its delta took the result.
            built.ahead_unsafe
        } else {
            match worker.outcome.analyzed.as_ref() {
                Ok(analyzed) => self.commit_ahead_unsafe(&rule, &parts, analyzed, &worker.outcome),
                Err(_) => Some("compute_failed"),
            }
        };
        let eligible = unsafe_reason.is_none();
        if let Some(reason) = unsafe_reason {
            bump(&stats().ahead_ineligible, 1);
            pipe.last_try.insert(index, reason);
        }
        let verified = if eligible && verify_ahead() {
            self.verify_commit_ahead(
                state,
                configured,
                pipe,
                &key,
                &rule,
                index,
                &parts,
                &worker.snapshot,
            )
            .map(|(parts, snapshot)| Box::new((parts, snapshot)))
        } else {
            None
        };
        if !eligible || (verify_ahead() && verified.is_none()) {
            put_back(
                pipe,
                parts,
                prepare_effects,
                prepare_track,
                refilled(Ok(worker)),
            );
            return Retry::Done(false);
        }
        let WorkerOutcome {
            snapshot,
            outcome,
            effects: compute_effects,
            sentinel: _,
            elapsed: compute_elapsed,
        } = worker;
        let class = crate::plan_timings::class_id(rule.rule_class_name());
        let marks = (self.warnings.len(), self.errors.len());
        let main_nodes = std::mem::replace(&mut state.nodes, lowering::PlannedNodes::segment());
        self.ahead = true;
        self.ahead_aborted = false;
        self.track_defer_start();
        let isolation = Isolation::enter();
        // What it registers is visible from its turn on (as sequentially).
        let stamp = state.artifact_index.stamp_ahead(index);
        let producer_stamp = state.producers.set_stamp(Some(index));
        let started = std::time::Instant::now();
        let timer = StageTimer::start(PlanStage::Commit, index as u64, class);
        let mut outcome = outcome;
        let result = verify_registered(&key, summary_to_verify(&mut outcome), || {
            self.commit_target(
                state,
                configured,
                &rule,
                parts.join(snapshot),
                Some(outcome),
            )
        });
        timer.finish();
        let commit_elapsed = started.elapsed();
        state.artifact_index.restore_stamp(stamp);
        state.producers.restore_stamp(producer_stamp);
        let (mut commit_effects, sentinel) = isolation.finish();
        note_effects("compute", &compute_effects);
        note_effects("commit_ahead", &commit_effects);
        let commit_track = self.track_defer_finish();
        self.ahead = false;
        let aborted = std::mem::take(&mut self.ahead_aborted);
        let mut segment = std::mem::replace(&mut state.nodes, main_nodes);
        segment.segment_close(std::mem::take(&mut commit_effects.console));
        pipe.committed_entries += segment.entries();
        let injected = test_abort_ahead();
        let result = if sentinel || aborted || injected {
            // Excluded by commit_ahead_unsafe: its commit fetches, analyzes
            // and loads nothing.
            debug_assert!(
                injected,
                "internal error: the commit of {} ahead of its turn needed a fetch, an analysis or a package",
                key.label
            );
            Err(format!(
                "internal error: the commit of {} ahead of its turn needed a fetch, an analysis or a package (BAZEL_RS_NO_COMMIT_AHEAD=1 commits every target at its turn)",
                key.label
            ))
        } else {
            result
        };
        let warnings = self.warnings.split_off(marks.0);
        let errors = self.errors.split_off(marks.1);
        pipe.committed.insert(
            index,
            Box::new(CommittedAhead {
                prepare_effects,
                prepare_track,
                compute_effects,
                commit_effects,
                commit_track,
                segment,
                warnings,
                errors,
                marks,
                result,
                elapsed: prepare_elapsed + compute_elapsed + commit_elapsed,
                platform_known,
                verified,
            }),
        );
        pipe.note_committed(index, &key.label);
        bump(&stats().committed_ahead, 1);
        self.pipeline_committed(state, pipe, &key);
        Retry::Done(true)
    }

    /// Whether the remote platform of every spawn the commit plans is
    /// decided without a memo filled on first need: its execution platform
    /// is the default host platform, carries its resolved properties, or is
    /// already in `Engine::platforms` (Engine::remote_platform; a platform
    /// analyzed between this commit and its turn would decide otherwise).
    fn remote_platforms_known(
        &self,
        parts: &PreparedParts,
        analyzed: &analysis::AnalysisResult,
    ) -> bool {
        let host = &self.configurations.get(parts.config).native.host_platform;
        analyzed.actions.iter().all(|registered| {
            let action = &registered.action;
            if action.write.is_some() || action.extra.transform.is_some() {
                return true;
            }
            let execution = parts
                .action_execution
                .iter()
                .find(|execution| execution.exec_group == registered.exec_group);
            let label = execution.map_or(host, |execution| &execution.platform);
            *label == crate::configuration::default_host_platform()
                || execution.is_some_and(|execution| execution.platform_properties.is_some())
                || self
                    .platforms
                    .contains_key(&ConfiguredTargetKey::new(label.clone(), parts.config))
        })
    }

    /// Why a finished entry's commit may depend on the order
    /// ([`ahead_unsafe`] with its remote platforms decided now).
    fn commit_ahead_unsafe(
        &self,
        rule: &Rule,
        parts: &PreparedParts,
        analyzed: &analysis::AnalysisResult,
        outcome: &ComputeOutcome,
    ) -> Option<&'static str> {
        ahead_unsafe(
            rule,
            &parts.output_root,
            analyzed,
            outcome.runfiles.as_ref(),
            self.remote_platforms_known(parts, analyzed),
        )
    }

    /// BAZEL_RS_VERIFY_AHEAD=1 for a commit ahead of its turn: its prepare
    /// is repeated now, as at its turn (ahead of it: lookups only), and
    /// compared ([`Self::verify_ahead`]); `false` when that prepare cannot
    /// run ahead (the entry then commits at its turn, verified there).
    #[allow(clippy::too_many_arguments)]
    fn verify_commit_ahead(
        &mut self,
        state: &mut ActionPlanning,
        configured: &mut ConfiguredNodes,
        pipe: &mut Pipeline,
        key: &ConfiguredTargetKey,
        rule: &Arc<Rule>,
        index: usize,
        parts: &PreparedParts,
        snapshot: &ComputeSnapshot,
    ) -> Option<(PreparedParts, ComputeSnapshot)> {
        self.ahead = true;
        self.ahead_aborted = false;
        self.track_defer_start();
        let isolation = Isolation::enter();
        let stamp = state.artifact_index.stamp_ahead(index);
        let again = self.prepare_target(state, key, rule, configured);
        state.artifact_index.restore_stamp(stamp);
        let (effects, sentinel) = isolation.finish();
        drop(self.track_defer_finish());
        self.ahead = false;
        let aborted = std::mem::take(&mut self.ahead_aborted) || sentinel;
        if !effects.console.is_empty() {
            note_lost(effects.console.len());
        }
        pipe.stashed.entry(index).or_default().push(effects);
        let Ok(again) = again else {
            return None;
        };
        if aborted {
            return None;
        }
        let (again_parts, again_snapshot) = PreparedParts::split(again)?;
        verify_prepared(key, parts, snapshot, &again_parts, &again_snapshot);
        // Compared again with the prepare its turn repeats (counted there,
        // Self::verify_ahead).
        Some((again_parts, again_snapshot))
    }

    /// S1: what the analysis worker needs to build the delta of the target
    /// prepared as `parts` (none for cquery's semantic commits and under an
    /// aquery capture, whose commits stay on the coordinator).
    fn delta_inputs(
        &self,
        state: &ActionPlanning,
        rule: &Rule,
        parts: &PreparedParts,
        class: u32,
    ) -> Option<Box<delta::DeltaInputs>> {
        if self.semantic_only() || self.aquery_capture.is_some() || !worker_deltas() {
            return None;
        }
        Some(Box::new(delta::DeltaInputs {
            view: delta::SharedView::new(self, state),
            key: parts.key.clone(),
            output_root: parts.output_root.clone(),
            action_owner: parts.action_owner.clone(),
            main_ignored: Arc::clone(&parts.main_ignored),
            transitive: parts.transitive.clone(),
            action_execution: parts.action_execution.clone(),
            platforms: self.remote_platforms(rule, parts.config, &parts.action_execution),
            class,
        }))
    }

    /// Prepare `key` ahead of its turn and dispatch its compute; `None`
    /// when the prepare aborted (prepared again at its turn).
    fn prepare_ahead(
        &mut self,
        state: &mut ActionPlanning,
        configured: &mut ConfiguredNodes,
        key: &ConfiguredTargetKey,
        rule: &Arc<Rule>,
        index: usize,
        stash: &mut Vec<Effects>,
    ) -> Option<Ahead> {
        use crate::plan_timings::{PlanStage, StageTimer};
        let class = crate::plan_timings::class_id(rule.rule_class_name());
        let plan_len = self.planning_plan.as_ref().map_or(0, Vec::len);
        let nodes = configured.len();
        let platform_known = self.platform_resolved(key);
        self.ahead = true;
        self.ahead_aborted = false;
        self.track_defer_start();
        let isolation = Isolation::enter();
        let started = std::time::Instant::now();
        let timer = StageTimer::start(PlanStage::Prepare, index as u64, class);
        // What it registers is visible from its turn on (as sequentially).
        let stamp = state.artifact_index.stamp_ahead(index);
        let prepared = self.prepare_target(state, key, rule, configured);
        state.artifact_index.restore_stamp(stamp);
        timer.finish();
        let prepare_elapsed = started.elapsed();
        let (effects, sentinel) = isolation.finish();
        let track = self.track_defer_finish();
        self.ahead = false;
        let aborted = std::mem::take(&mut self.ahead_aborted)
            || sentinel
            || prepared
                .as_ref()
                .is_err_and(|error| crate::repository_demand::is_inline_rerun_sentinel(error));
        debug_assert!(
            aborted
                || (self.planning_plan.as_ref().map_or(0, Vec::len) == plan_len
                    && configured.len() == nodes),
            "internal error: a prepare ahead of its turn analyzed {}",
            key.label
        );
        if aborted {
            // Done again at the turn; what it observed counts (memo misses
            // it filled are hits there), its notes and tracker events recur.
            bump(&stats().aborted, 1);
            note_effects("prepare_aborted", &effects);
            if !effects.console.is_empty() {
                note_lost(effects.console.len());
            }
            stash.push(effects);
            drop(track);
            return None;
        }
        match prepared {
            Err(error) => {
                bump(&stats().failed, 1);
                Some(Ahead::Failed {
                    error,
                    effects,
                    track,
                })
            }
            Ok(prepared) => match PreparedParts::split(prepared) {
                Some((parts, snapshot)) => {
                    bump(&stats().ahead, 1);
                    note_effects("prepare", &effects);
                    self.ensure_analysis_pool();
                    let delta = self.delta_inputs(state, rule, &parts, class);
                    let pending =
                        self.dispatch(snapshot, parts.transitive.clone(), index, class, delta);
                    Some(Ahead::Computing {
                        parts: Box::new(parts),
                        effects,
                        track,
                        pending,
                        platform_known,
                        prepare_elapsed,
                    })
                }
                None => {
                    // Not reached (a classified entry is a Starlark rule);
                    // its prepare is repeated at the turn.
                    bump(&stats().aborted, 1);
                    stash.push(effects);
                    None
                }
            },
        }
    }
}

/// Whether every output the commit registers lies below the target's
/// package output directory (`<output root>/<package exec path>/`): the
/// "settled" rule keys on packages, so an output elsewhere — a shareable
/// output named from another package's path (a C++ library's
/// `_solib_<cpu>/` symlink, analysis_runtime::declare_shareable), or a
/// main-repository root package output under `external/` (the output tree
/// of `@r//p`) — commits at its turn, where the first owner in plan order
/// registers it.
fn outputs_in_package(rule: &Rule, output_root: &str, analyzed: &analysis::AnalysisResult) -> bool {
    let package = Path::new(output_root).join(rule.label.package_exec_path());
    let main_root = rule.label.repository.is_empty() && rule.label.package.is_empty();
    let external = Path::new(output_root).join("external");
    let inside = |path: &str| {
        let path = Path::new(path);
        path != package && path.starts_with(&package) && !(main_root && path.starts_with(&external))
    };
    let actions = analyzed
        .actions
        .iter()
        .flat_map(|action| &action.outputs)
        .all(|file| inside(&file.path));
    let runfiles = analyzed
        .runfiles
        .files_to_run
        .as_ref()
        .is_none_or(|files_to_run| {
            files_to_run
                .executable()
                .into_iter()
                .chain(files_to_run.repo_mapping_manifest())
                .chain(files_to_run.runfiles_manifest())
                .all(|file| inside(&file.path))
        });
    actions && runfiles
}

/// Why a finished entry's commit may depend on the order (P4: no
/// transition allowlist, which loads its package; no workspace status,
/// registered on first need by whichever target comes first; its runfiles
/// support's pure part computed by the worker; no repo mapping manifest or
/// build-info action to resolve; every remote platform decided,
/// `platforms_known`): the first condition that fails, `None` when its
/// commit is a function of its compute and its committed dependencies
/// alone. (The tool runfiles groups and shared input runs are memoized
/// single-flight since S1: whoever fills them first fills the same value.)
pub(super) fn ahead_unsafe(
    rule: &Rule,
    output_root: &str,
    analyzed: &analysis::AnalysisResult,
    precomputed: Option<&RunfilesPrecomputed>,
    platforms_known: bool,
) -> Option<&'static str> {
    if !outputs_in_package(rule, output_root, analyzed) {
        return Some("ineligible_outputs_elsewhere");
    }
    if !platforms_known {
        return Some("ineligible_remote_platform");
    }
    if !rule
        .custom
        .as_ref()
        .is_some_and(|custom| custom.transitions.is_empty())
        || restricted_to_error(rule).is_err()
    {
        return Some("ineligible_transition_or_restricted");
    }
    if Engine::workspace_status_needed(&analyzed.actions)
        || analyzed
            .files
            .iter()
            .any(crate::analysis_runtime::is_workspace_status_file)
    {
        return Some("ineligible_workspace_status");
    }
    let runfiles_ready = match precomputed {
        Some(precomputed) => !precomputed
            .mapping
            .values()
            .flatten()
            .any(crate::analysis_runtime::is_workspace_status_file),
        // Without runfiles support (Engine::plan_runfiles_support plans
        // nothing), else computed at the commit.
        None => analyzed
            .runfiles
            .files_to_run
            .as_ref()
            .is_none_or(|files_to_run| {
                files_to_run.runfiles().is_none()
                    || files_to_run.executable().is_none()
                    || files_to_run.repo_mapping_manifest().is_none()
            }),
    };
    if !runfiles_ready {
        return Some("ineligible_runfiles");
    }
    if analyzed
        .actions
        .iter()
        .any(|action| action.repo_mapping.is_some() || action.build_info.is_some())
    {
        return Some("ineligible_repo_mapping_or_build_info");
    }
    None
}
