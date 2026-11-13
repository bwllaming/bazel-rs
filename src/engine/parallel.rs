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

pub(super) mod aspect_jobs;
pub(super) mod aspects;
mod buffer;
mod frozen;
pub(super) use buffer::{
    EarlyCt2, count_in_phase_walk, early_enabled, in_phase_mode, in_phase_walks,
};
pub(super) use frozen::{Frozen, PlannedResults};
use std::collections::BTreeSet as OrderedSet;
use std::sync::mpsc;

/// Marker of the abort of work ahead of its turn (never reported: the turn
/// does the work again).
const AHEAD_ABORT: &str = "\u{1}parallel-analysis-ahead-abort";

/// The most entries prepared ahead and not committed (computing or
/// finished, waiting to commit), as a multiple of the in-flight bound.
const HELD_FACTOR: usize = 8;

/// How many plan entries past the turn the coordinator classifies.
const HORIZON: usize = 4096;

/// S3: the analysis workers of an evaluated build (they prepare too):
/// `--loading_phase_threads`, bounded by the cores.
///
/// `lowering`: the walk's jobs lower their actions too (`--keep_going`,
/// whose lowerings are not deferred: [`Engine::keep_going_buffered`]): a
/// quarter more workers, the lowering workers' share (~/code `-k`: 16
/// workers 18.7-19.3 s, 20 workers 18.1-18.5 s, +0.1 GB).
fn frozen_workers(loading_phase_threads: usize, lowering: bool) -> usize {
    // T3: the cursor's merges bound a frozen walk, not the workers; more
    // than 8 at once only raised the peak memory of their computes (~/code:
    // 48 workers 10.3 GB, 8 workers 9.95 GB, the same walk time). Under the
    // sweep (super::sweep, no rebuilds at the merges) 16 (~/code: elapsed
    // 24.1 -> 23.3 s, peak 9.4 -> 9.8 GB; 24 workers 22.9 s, 10.0 GB).
    let cores = std::thread::available_parallelism().map_or(1, usize::from);
    let cap = match frozen_worker_cap() {
        cap if lowering && !worker_cap_overridden() => cap + cap / 4,
        cap => cap,
    };
    loading_phase_threads.min(cores).clamp(2, cap.max(2))
}

/// Whether frozen walks merge through the T3 buffer (the sweep needs it:
/// super::sweep::enabled).
pub(super) fn buffered() -> bool {
    buffer::enabled()
}

/// `BAZEL_RS_T3_WORKERS=<n>`: the frozen walk's analysis workers (4 while
/// the analysis snapshot records; else 16, 8 with `BAZEL_RS_EVAL_SWEEP=0`).
fn frozen_worker_cap() -> usize {
    static CAP: std::sync::LazyLock<usize> = std::sync::LazyLock::new(|| {
        // snapshot-unrecorded: a scheduling bound; the output is the same either way.
        std::env::var("BAZEL_RS_T3_WORKERS")
            .ok()
            .and_then(|value| value.parse().ok())
            .unwrap_or(if crate::observed_fs::recording() {
                // While the analysis snapshot records (eval-record): the
                // computes' peak memory (~/code: the planning tail's
                // frozen walk 10.3 -> 10.05 GB) at the same walk time.
                4
            } else if super::sweep::enabled() {
                16
            } else {
                8
            })
    });
    *CAP
}

/// Whether `BAZEL_RS_T3_WORKERS` sets the frozen walks' workers.
fn worker_cap_overridden() -> bool {
    // snapshot-unrecorded: a scheduling bound; the output is the same either way.
    std::env::var_os("BAZEL_RS_T3_WORKERS").is_some()
}

type Job = Box<dyn FnOnce() + Send>;

/// A finished compute: its plan index, the outputs its commit registers
/// (`None`: not known, e.g. it raised a sentinel and runs again inline)
/// and whether its implementation asked for a deferred target platform.
type Finished = (usize, Option<Arc<OutputSummary>>, bool, std::time::Instant);

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

/// `BAZEL_RS_TIMING`: the requirements of the entry classified last (the
/// walk's chain depths, `PERF-PARALLEL chains`).
fn note_classified_keys(keys: &[ConfiguredTargetKey]) {
    if crate::plan_timings::enabled() {
        CLASSIFIED_KEYS.with(|slot| *slot.borrow_mut() = Some(keys.to_vec()));
    }
}

thread_local! {
    static CLASSIFIED_KEYS: std::cell::RefCell<Option<Vec<ConfiguredTargetKey>>> = const { std::cell::RefCell::new(None) };
    /// BAZEL_RS_VERIFY_AHEAD: the requirements the last native
    /// classification found (coordinator only; Pipeline::native_keys).
    static VERIFY_NATIVE_KEYS: std::cell::RefCell<Option<Vec<ConfiguredTargetKey>>> = const { std::cell::RefCell::new(None) };
    /// BAZEL_RS_VERIFY_AHEAD: the output paths the commit being verified
    /// registered (crate::engine::register_output).
    static REGISTERED: std::cell::RefCell<Option<Vec<u64>>> = const { std::cell::RefCell::new(None) };
}

/// Note an output path a commit registers (BAZEL_RS_VERIFY_AHEAD records
/// them to check the commit's [`OutputSummary`]).
pub(super) fn record_registered_output(output: &Path) {
    record_registered(output, || path_hash(output));
}

/// [`record_registered_output`] of a path hashed as `hash` ([`path_hash`]).
pub(super) fn record_registered_hashed(output: &Path, hash: u64) {
    record_registered(output, || hash);
}

fn record_registered(output: &Path, hash: impl FnOnce() -> u64) {
    REGISTERED.with(|registered| {
        if let Some(registered) = registered.borrow_mut().as_mut() {
            // The workspace status files belong to no target: whichever
            // commit needs them first registers them
            // (Engine::ensure_workspace_status), and a commit that needs
            // them never commits ahead of its turn.
            if !matches!(
                output.to_str(),
                Some("bazel-out/stable-status.txt" | "bazel-out/volatile-status.txt")
            ) {
                registered.push(hash());
            }
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
    finished: (
        mpsc::Sender<Finished>,
        std::sync::Mutex<mpsc::Receiver<Finished>>,
    ),
    /// S2: the queued computes, the earliest plan index first (the entry at
    /// its turn, then the ones closest to it: the walk waits for them).
    jobs: Arc<JobQueue>,
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

/// Make this thread an analysis worker: its panics are recorded for the
/// coordinator ([`WORKER_PANIC`], the hook installed), it allocates in the
/// coordinator's jemalloc arena (`arena`: the coordinator's, taken on its
/// thread) — the pool's workers and the merge buffer's classifiers.
pub(super) fn enter_worker_thread(arena: Option<u32>) {
    install_worker_panic_hook();
    IS_WORKER.with(|worker| worker.set(true));
    if let Some(arena) = arena {
        crate::alloc_arena::bind(arena);
    }
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

/// One queued compute: its plan index and submission order.
struct Queued {
    index: usize,
    order: u64,
    job: Job,
}

impl PartialEq for Queued {
    fn eq(&self, other: &Self) -> bool {
        (self.index, self.order) == (other.index, other.order)
    }
}

impl Eq for Queued {}

impl PartialOrd for Queued {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for Queued {
    /// The greatest is the earliest plan index, then the first submitted
    /// (std's BinaryHeap pops the greatest).
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        (other.index, other.order).cmp(&(self.index, self.order))
    }
}

/// The analysis workers' queue ([`AnalysisPool::jobs`]).
#[derive(Default)]
struct JobQueue {
    state: std::sync::Mutex<JobState>,
    ready: std::sync::Condvar,
}

#[derive(Default)]
struct JobState {
    queued: std::collections::BinaryHeap<Queued>,
    order: u64,
    closed: bool,
}

impl JobQueue {
    fn push(&self, index: usize, job: Job) -> Result<(), Job> {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if state.closed {
            return Err(job);
        }
        state.order += 1;
        let order = state.order;
        state.queued.push(Queued { index, order, job });
        drop(state);
        self.ready.notify_one();
        Ok(())
    }

    /// The next job (`None`: the queue is closed and empty).
    fn pop(&self) -> Option<Job> {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        loop {
            if let Some(queued) = state.queued.pop() {
                return Some(queued.job);
            }
            if state.closed {
                return None;
            }
            state = self.ready.wait(state).unwrap_or_else(|e| e.into_inner());
        }
    }

    fn close(&self) {
        self.state.lock().unwrap_or_else(|e| e.into_inner()).closed = true;
        self.ready.notify_all();
    }
}

impl AnalysisPool {
    fn start(size: usize) -> Option<Self> {
        install_worker_panic_hook();
        // The workers allocate in the coordinator's jemalloc arena: the
        // snapshots, providers and actions they make are kept and freed by
        // the coordinator (src/alloc_arena.rs).
        let coordinator_arena = crate::alloc_arena::current();
        let queue = Arc::new(JobQueue::default());
        let mut workers = Vec::with_capacity(size);
        for index in 0..size {
            let queue = Arc::clone(&queue);
            let spawned = std::thread::Builder::new()
                .name(format!("analysis-{index}"))
                .stack_size(LOADING_WORKER_STACK)
                .spawn(move || {
                    enter_worker_thread(coordinator_arena);
                    while let Some(job) = queue.pop() {
                        job();
                    }
                });
            match spawned {
                Ok(handle) => workers.push(handle),
                Err(_) => break,
            }
        }
        if workers.is_empty() {
            queue.close();
            return None;
        }
        Some(Self {
            finished: {
                let (sender, receiver) = mpsc::channel();
                (sender, std::sync::Mutex::new(receiver))
            },
            jobs: queue,
            size: workers.len(),
            workers,
            cancelled: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        })
    }

    /// Queue the compute of plan index `index`.
    fn submit(&self, index: usize, job: Job) -> Result<(), Job> {
        self.jobs.push(index, job)
    }
}

impl AnalysisPool {
    /// Stop without waiting for the computes still running
    /// (BAZEL_RS_ANALYSIS_ORDER=genuine's fail fast: the walk ended with a
    /// later job's error while earlier jobs compute on; their results are
    /// dropped, the threads detached and ended with the process).
    fn abandon(mut self) {
        self.workers.clear();
    }
}

impl Drop for AnalysisPool {
    fn drop(&mut self) {
        self.cancelled
            .store(true, std::sync::atomic::Ordering::SeqCst);
        self.jobs.close();
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
        // Discarded work (its console dropped) is done again: its action
        // registrations count there, once.
        let observations = if console {
            self.observations
        } else {
            self.observations.without_registrations()
        };
        crate::observed_fs::commit(observations);
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

/// A worker's compute of a prepared target (P3), with the pure parts of
/// its commit: the repo mapping manifests, the declared outputs' check,
/// the runfiles support, the outputs it registers and (S1) its commit as a
/// delta, in an [`Isolation`] of its own; the summary is `None` after a
/// sentinel.
#[allow(clippy::too_many_arguments)]
fn run_compute(
    snapshot: ComputeSnapshot,
    transitive: &BTreeSet<String>,
    index: usize,
    class: u32,
    delta: Option<Box<delta::DeltaInputs>>,
    compact_repo_mapping: bool,
    verify_summary: bool,
) -> (WorkerOutcome, Option<Arc<OutputSummary>>) {
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
            let (Some(runfiles), Some(_)) = (&registered.repo_mapping, &registered.action.write)
            else {
                continue;
            };
            let sentinels = crate::repository_demand::sentinels_raised();
            let repositories = &snapshot.repositories;
            let content = crate::runfiles_tree::repo_mapping_manifest(
                runfiles,
                transitive,
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
        let checked =
            validate_declared_outputs(label, &snapshot.output_root, &analyzed.declared, &|| {
                Ok((
                    repositories.root(&label.repository)?.to_path_buf(),
                    repositories.ignored_subdirectories(&label.repository)?,
                ))
            });
        if crate::repository_demand::sentinels_raised() == sentinels {
            outcome.outputs_checked = Some(checked);
        }
    }
    if let Ok(analyzed) = &outcome.analyzed {
        let sentinels = crate::repository_demand::sentinels_raised();
        let precomputed = RunfilesPrecomputed::compute(
            &analyzed.runfiles,
            transitive,
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
        let built = inputs.build(&snapshot, &mut outcome, index, verify_summary, &registers);
        outcome.delta = Some(Box::new(built));
    }
    let elapsed = started.elapsed();
    let (effects, sentinel) = isolation.finish();
    let summary = (!sentinel).then(|| Arc::new(registers));
    (
        WorkerOutcome {
            snapshot,
            outcome,
            effects,
            sentinel,
            elapsed,
        },
        summary,
    )
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
    /// S3 (eval pass 2): prepared and computed on a worker
    /// ([`frozen::Frozen`]), not reported yet.
    Preparing(frozen::Preparing),
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
    /// Its prepare panicked on a worker (S3) with this: the panic the
    /// sequential engine raises at its turn (an internal error).
    Panicked(String),
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
    /// BAZEL_RS_VERIFY_AHEAD, a native rule: the keys it waited for (all
    /// committed when it committed ahead), compared at its turn with
    /// [`Engine::native_requirements`] again.
    native_requirements: Option<Vec<ConfiguredTargetKey>>,
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
    /// S2: the classified entries not committed yet that may resolve the
    /// deferred target platform of their configuration at their turn (a
    /// Starlark rule whose compute has not finished without asking for it;
    /// a genrule, `$(TARGET_CPU)`), by configuration: a target prepared with
    /// that platform deferred commits ahead of its turn only when none is
    /// before it (its prepare is then the one its turn would make).
    platform_pending: HashMap<ConfigId, OrderedSet<usize>>,
    platform_config: HashMap<usize, ConfigId>,
    /// The entries prepared ahead whose computes finished (the rest are in
    /// flight on the workers).
    computed: std::collections::HashSet<usize>,
    /// BAZEL_RS_VERIFY_AHEAD: the keys each native rule classified to commit
    /// ahead waits for ([`Engine::native_requirements`]).
    native_keys: HashMap<usize, Vec<ConfiguredTargetKey>>,
    /// S2: finished entries waiting for an earlier one ([`Self::park`]).
    parked: HashMap<usize, Vec<usize>>,
    /// Latency diagnostics: when each entry became ready, was dispatched,
    /// finished on its worker and was noticed by the coordinator.
    lat: HashMap<usize, [Option<std::time::Instant>; 4]>,
    /// S2: the native rules classified to commit ahead of their turn (once
    /// their dependencies are committed, [`Engine::commit_native_ahead`]),
    /// and those whose dependencies are.
    natives: std::collections::HashSet<usize>,
    native_ready: OrderedSet<usize>,
    /// S2 diagnostics (`BAZEL_RS_TIMING`): what woke each ready entry
    /// (`PERF-PARALLEL diag ready_late_by_<cause>`): `classify` (empty),
    /// a commit ahead, a delegation, or the commit at its turn of a native
    /// rule class or of a Starlark rule (with its fallback reason).
    woken_by: HashMap<usize, String>,
    waking: String,
    /// The fallback reason of the entry at its turn.
    turn_reason: &'static str,
    /// `BAZEL_RS_TIMING`: the chain depth of each classified key (Starlark
    /// implementations along its deepest requirement path within the walk)
    /// and the walk's deepest.
    depth: HashMap<ConfiguredTargetKey, u32>,
    max_depth: u32,
    /// T2 (a frozen walk): the keys whose phase-2 results a worker
    /// published ahead of their merge, with their plan index (a Starlark
    /// entry waiting for one is ready on it, [`Engine::wait_or_ready`]), and
    /// those not woken yet.
    published: HashMap<ConfiguredTargetKey, usize>,
    newly_published: Vec<ConfiguredTargetKey>,
    /// T2: the keys each entry became ready on before their merge (it
    /// commits ahead of its turn only once they are merged: what its commit
    /// reads of them, and of their dependencies, is then canonical).
    early: HashMap<usize, Vec<ConfiguredTargetKey>>,
    /// T2: the native rules whose results a worker computes once their
    /// requirements are published or committed: waiting (by key, as
    /// `waiting`), ready, computing.
    native_waiting: HashMap<ConfiguredTargetKey, Vec<(usize, Vec<ConfiguredTargetKey>)>>,
    native_publish_ready: Vec<usize>,
    native_jobs: HashMap<usize, frozen::NativeJob>,
}

impl Pipeline {
    /// S2 diagnostics: what wakes the entries readied next (`BAZEL_RS_TIMING`).
    fn set_waking(&mut self, cause: &str) {
        if crate::plan_timings::enabled() {
            self.waking.clear();
            self.waking.push_str(cause);
        }
    }

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
        self.unpark(index);
    }

    /// `index` (in `config`) may resolve its configuration's deferred
    /// target platform at its turn.
    fn note_platform_pending(&mut self, index: usize, config: ConfigId) {
        self.platform_pending
            .entry(config)
            .or_default()
            .insert(index);
        self.platform_config.insert(index, config);
    }

    /// `index` resolves no target platform (any more).
    fn platform_settled(&mut self, index: usize) {
        self.unpark(index);
        if let Some(config) = self.platform_config.remove(&index)
            && let Some(pending) = self.platform_pending.get_mut(&config)
        {
            pending.remove(&index);
            if pending.is_empty() {
                self.platform_pending.remove(&config);
            }
        }
    }

    /// Whether an entry before `index` may still resolve the deferred
    /// target platform of `config`.
    fn platform_resolver_before(&self, index: usize, config: ConfigId) -> Option<usize> {
        self.platform_pending
            .get(&config)
            .and_then(|pending| pending.range(..index).next_back().copied())
    }

    /// `index` (of `label`) is committed.
    fn note_committed(&mut self, index: usize, label: &Label) {
        self.platform_settled(index);
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
        self.unpark(index);
    }

    /// Whether an uncommitted entry before `index` with known outputs
    /// registers one of `summary`'s paths, an ancestor of one, or a path
    /// below one.
    fn meets_known(&self, index: usize, summary: &OutputSummary) -> Option<usize> {
        let earlier = |indexes: Option<&OrderedSet<usize>>| {
            indexes
                .and_then(|indexes| indexes.first().copied())
                .filter(|first| *first < index)
        };
        summary
            .paths
            .iter()
            .find_map(|path| {
                earlier(self.known_paths.get(path))
                    .or_else(|| earlier(self.known_ancestors.get(path)))
            })
            .or_else(|| {
                summary
                    .ancestors
                    .iter()
                    .find_map(|ancestor| earlier(self.known_paths.get(ancestor)))
            })
    }

    /// Whether an uncommitted entry before `index` may register `label`'s
    /// output paths (`summary`), or paths above or below them, and why: 0
    /// one of its package, 1 of an enclosing one, 2 of an enclosed one
    /// (outputs not known yet), 3 an entry whose known outputs meet these,
    /// 4 an aspect barrier (`None`: its outputs are settled); with the
    /// earlier entry it waits for (S2: it is retried when that one's
    /// outputs become known or it commits, [`Self::park`]).
    fn unsettled(
        &self,
        index: usize,
        label: &Label,
        summary: &OutputSummary,
    ) -> Option<(usize, usize)> {
        if let Some(last) = self.barriers.range(..index).next_back() {
            return Some((4, *last));
        }
        if let Some(blocker) = self.meets_known(index, summary) {
            return Some((3, blocker));
        }
        // The last earlier one (retried when it settles; the earlier ones
        // mostly settled before it).
        let earlier = |pending: &OrderedSet<usize>| pending.range(..index).next_back().copied();
        let repository = &label.repository;
        // The package and every enclosing one.
        let mut package = label.package.as_str();
        loop {
            if let Some(blocker) = self
                .pending_packages
                .get(&(repository.clone(), package.to_owned()))
                .and_then(earlier)
            {
                return Some((if package == label.package { 0 } else { 1 }, blocker));
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
            .filter(|((_, package), _)| package != &label.package)
            .find_map(|(_, pending)| earlier(pending))
            .map(|blocker| (2, blocker))
    }

    /// S2: the finished entry `index` waits for `blocker` (an earlier entry
    /// whose outputs are not known, or meet its own, or a barrier, or one
    /// that may resolve its deferred target platform): it is retried when
    /// that one's outputs become known, its platform settles or it commits
    /// ([`Self::unpark`]), not on every poll.
    fn park(&mut self, index: usize, blocker: usize) {
        self.finished.remove(&index);
        self.parked.entry(blocker).or_default().push(index);
    }

    /// The entries parked on `index` are retried.
    fn unpark(&mut self, index: usize) {
        if let Some(waiters) = self.parked.remove(&index) {
            self.finished.extend(waiters);
        }
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
    /// BAZEL_RS_VERIFY_AHEAD: native rules committed ahead whose
    /// requirements were compared at their turn.
    verified_native: AtomicU64,
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
    /// The sum of the walks' deepest chains (`PERF-PARALLEL chains`) and
    /// the deepest of all.
    walk_depth: [AtomicU64; 4],
    deepest: AtomicU64,
    /// S2 diagnostics (`PERF-PARALLEL diag`).
    diag: std::sync::Mutex<BTreeMap<String, u64>>,
    lat: std::sync::Mutex<BTreeMap<String, (u64, u64)>>,
    /// S2: prepares ahead of their turn off the coordinator's state, the
    /// batches they ran in, the batches' entries and helper threads, the
    /// coordinator's time in the batches (with their fallbacks and
    /// dispatches), in running them, and in the prepares at the turn (µs),
    /// and the prepares that needed the coordinator, by the reason.
    shared_prepares: AtomicU64,
    batches: AtomicU64,
    batch_entries: AtomicU64,
    batch_threads: AtomicU64,
    coordinator_prepare_us: AtomicU64,
    batch_run_us: AtomicU64,
    coordinator_turn_prepare_us: AtomicU64,
    prepare_fallback: std::sync::Mutex<BTreeMap<&'static str, u64>>,
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

/// S2 diagnostics: count one event (`PERF-PARALLEL diag`).
fn diag(what: &str) {
    if crate::plan_timings::enabled()
        && let Ok(mut counts) = stats().diag.lock()
    {
        *counts.entry(what.to_owned()).or_default() += 1;
    }
}

/// Latency diagnostics: add `us` microseconds under `what`.
fn lat_add(what: &str, us: u64) {
    if let Ok(mut counts) = stats().lat.lock() {
        let entry = counts.entry(what.to_owned()).or_default();
        entry.0 += 1;
        entry.1 += us;
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
    crate::snapshot::diag::line_fmt(format_args!(
        "PERF-PARALLEL coordinator split {}",
        frozen::SPLIT_NAMES
            .iter()
            .zip(&frozen::SPLIT)
            .map(|(name, us)| format!("{name}={:.3}s", get(us) as f64 / 1e6))
            .collect::<Vec<_>>()
            .join(" ")
    ));
    crate::snapshot::diag::line_fmt(format_args!(
        "PERF-PARALLEL frozen prepared={} aborted={} failed={} turn_wait={:.3}s",
        get(&frozen::FROZEN_STATS[0]),
        get(&frozen::FROZEN_STATS[1]),
        get(&frozen::FROZEN_STATS[2]),
        get(&frozen::FROZEN_STATS[3]) as f64 / 1e6,
    ));
    buffer::report();
    super::sweep::report();
    aspects::report();
    aspect_jobs::report();
    if let Some(profile) = frozen::CT2_PROFILE
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .as_mut()
    {
        crate::snapshot::diag::line_fmt(format_args!("{}", profile.line()));
    }
    crate::snapshot::diag::line_fmt(format_args!(
        "PERF-PARALLEL provisional published={} natives={} stale_rebuilt={} with_reads={} memo_refilled={} shared_published={} shared_unpublished={}",
        get(&delta::PROVISIONAL_STATS[0]),
        get(&delta::PROVISIONAL_STATS[4]),
        get(&delta::PROVISIONAL_STATS[1]),
        get(&delta::PROVISIONAL_STATS[2]),
        get(&delta::PROVISIONAL_STATS[3]),
        get(&delta::PROVISIONAL_STATS[5]),
        get(&delta::PROVISIONAL_STATS[6]),
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
        "PERF-PARALLEL chains (<=16,<=256,<=4096,more: sum of the walks' deepest Starlark chains) {} {} {} {} deepest={}",
        get(&stats.walk_depth[0]),
        get(&stats.walk_depth[1]),
        get(&stats.walk_depth[2]),
        get(&stats.walk_depth[3]),
        get(&stats.deepest)
    ));
    crate::snapshot::diag::line_fmt(format_args!(
        "PERF-PARALLEL unsettled package={} enclosing={} enclosed={} outputs={} budget={} platform_late={} verified_turn={} verified_outputs={} verified_native={}",
        get(&stats.unsettled[0]),
        get(&stats.unsettled[1]),
        get(&stats.unsettled[2]),
        get(&stats.unsettled[3]),
        get(&stats.ahead_budget),
        get(&stats.committed_platform_late),
        get(&stats.verified_turn),
        get(&stats.verified_outputs),
        get(&stats.verified_native)
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
    let prepare_fallbacks = stats
        .prepare_fallback
        .lock()
        .map(|counts| {
            counts
                .iter()
                .map(|(reason, count)| ((*reason).to_owned(), *count))
                .collect()
        })
        .unwrap_or_default();
    crate::snapshot::diag::line_fmt(format_args!(
        "PERF-PARALLEL prepare shared={} batches={} batch_entries={} batch_threads={} coordinator_ahead={:.3}s batch_run={:.3}s coordinator_turn={:.3}s fallback {}",
        get(&stats.shared_prepares),
        get(&stats.batches),
        get(&stats.batch_entries),
        get(&stats.batch_threads),
        get(&stats.coordinator_prepare_us) as f64 / 1e6,
        get(&stats.batch_run_us) as f64 / 1e6,
        get(&stats.coordinator_turn_prepare_us) as f64 / 1e6,
        line(prepare_fallbacks)
    ));
    let effects = stats
        .effects
        .lock()
        .map(|counts| counts.clone().into_iter().collect())
        .unwrap_or_default();
    crate::snapshot::diag::line_fmt(format_args!("PERF-PARALLEL effects {}", line(effects)));
    let diags = stats
        .diag
        .lock()
        .map(|counts| counts.clone().into_iter().collect())
        .unwrap_or_default();
    crate::snapshot::diag::line_fmt(format_args!("PERF-PARALLEL diag {}", line(diags)));
    if let Ok(lat) = stats.lat.lock() {
        for (what, (count, us)) in lat.iter() {
            crate::snapshot::diag::line_fmt(format_args!(
                "PERF-PARALLEL lat {what} count={count} total={:.3}s avg={:.3}ms",
                *us as f64 / 1e6,
                *us as f64 / 1e3 / (*count).max(1) as f64
            ));
        }
    }
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
    // S2: the coordinator thread's CPU time so far (user, system; stable
    // under host load, unlike the wall time).
    // snapshot-unrecorded: a BAZEL_RS_TIMING diagnostic of this process's own CPU time; no analysis result depends on it.
    if let Ok(stat) = std::fs::read_to_string("/proc/thread-self/stat")
        && let Some(fields) = stat.rsplit_once(')').map(|(_, rest)| {
            rest.split_whitespace()
                .filter_map(|field| field.parse::<i64>().ok())
                .collect::<Vec<_>>()
        })
        && fields.len() > 12
    {
        // utime and stime: fields 14 and 15 of stat(5), after the state
        // field (not numeric) — the 11th and 12th numeric ones here.
        crate::snapshot::diag::line_fmt(format_args!(
            "PERF-PARALLEL coordinator_cpu user={:.2}s sys={:.2}s",
            fields[10] as f64 / 100.0,
            fields[11] as f64 / 100.0
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

/// `BAZEL_RS_NO_NATIVE_AHEAD` (hidden): native rules commit at their turn
/// (S2; the output is the same either way).
fn native_ahead_enabled() -> bool {
    static ENABLED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ENABLED.get_or_init(|| {
        // snapshot-unrecorded: a diagnostics switch choosing the scheduling only; the output is the same either way.
        std::env::var_os("BAZEL_RS_NO_NATIVE_AHEAD").is_none()
    })
}

/// S2: whether the native rule `rule` may commit ahead of its turn once its
/// dependencies are committed: one of the reviewed classes, which register
/// no output ([`registers_no_output`]) and need nothing registered on first
/// need (a filegroup's `output_group` files may name the workspace status,
/// so such a filegroup commits at its turn). Any other class — genquery
/// and starlark_doc_extract (outputs; their commit queries or loads), a
/// class added later — commits at its turn.
fn native_ahead_eligible(rule: &Rule) -> bool {
    let reviewed = match (&rule.kind, &rule.native) {
        (RuleKind::Filegroup, Some(native)) => native
            .string("output_group")
            .is_none_or(|group| group.is_empty()),
        (RuleKind::Filegroup, None) | (RuleKind::PackageGroup(_), _) => true,
        (RuleKind::Native(name), _) => matches!(
            *name,
            "alias"
                | "label_flag"
                | "label_setting"
                | "test_suite"
                | "config_setting"
                | "constraint_setting"
                | "constraint_value"
                | "platform"
                | "toolchain"
                | "toolchain_type"
                | "java_plugins_flag_alias"
        ),
        (RuleKind::Genrule | RuleKind::Custom, _) => false,
    };
    reviewed && registers_no_output(rule)
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

    /// Whether this build's eval pass-2 walks may be frozen (prepares on
    /// the workers, the merge buffer).
    pub(super) fn frozen_walks(&self) -> bool {
        self.eval_async_phase2 && frozen::enabled()
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
        let overridden = std::env::var_os("BAZEL_RS_ANALYSIS_WORKERS").is_some();
        if self.eval_async_phase2 && !overridden && frozen::enabled() {
            return frozen_workers(self.loading_phase_threads, self.keep_going_buffered());
        }
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
        // The analysis workers first: their delta builds hold the lowering
        // workers' handle (S1).
        if let Some(pool) = self.analysis_pool.take() {
            report(pool.size);
            if std::mem::take(&mut self.abandon_analysis_workers) {
                pool.abandon();
            } else {
                drop(pool);
            }
        }
        self.stop_lowering_pool();
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
        // S3 (eval pass 2): a walk long enough prepares on the workers.
        let remaining = self
            .planning_plan
            .as_ref()
            .map_or(0, |plan| plan.len().saturating_sub(state.done));
        // While the snapshot records, the planning tail after the deferred
        // walk (~/code: 14k entries) walks on the pipeline: the frozen
        // walk's view and its computes at once peak higher (10.0 -> 9.8 GB
        // on a miss, +1-2 s).
        let tail_while_recording = self.eval_pass2.is_none()
            && crate::observed_fs::recording()
            // snapshot-unrecorded: a scheduling switch (`frozen`: the tail frozen too); the output is the same either way.
            && std::env::var_os("BAZEL_RS_EVAL_RECORDING_TAIL").is_none_or(|value| value != "frozen");
        // (Task D: a walk taking early boards' jobs is a buffered one.)
        if (frozen::walk_min().is_some_and(|min| remaining >= min) || self.early_ct2.is_some())
            && !tail_while_recording
            && self.frozen_prepares(state)
        {
            crate::perf_mark("pass2.freeze");
            let frozen = self.freeze(state, configured);
            crate::perf_mark("pass2.frozen");
            self.frozen_walk = Some(frozen);
            pipe.capacity =
                (frozen_workers(self.loading_phase_threads, self.keep_going_buffered()) * 2).max(2);
        } else if self.eval_async_phase2 {
            // A walk on the pipeline after the deferred walk (the planning
            // tail): the frozen walk's results are not read again (memory;
            // a later frozen walk would seed them anew).
            state.walk_results = None;
            frozen::profile(frozen::Ct2Profile::walk_start);
        }
        let result = {
            let _split = frozen::Split::enter(0);
            if crate::plan_timings::enabled() {
                super::IN_FROZEN_WALK.store(
                    self.frozen_walk.is_some(),
                    std::sync::atomic::Ordering::Relaxed,
                );
            }
            if self.frozen_walk.is_some() && buffer::enabled() {
                // T3: the plan-ordered merge buffer.
                self.plan_targets_buffered(state, configured)
            } else {
                self.plan_targets_pipelined(state, configured, &mut pipe)
            }
        };
        let mut result = result;
        if let Some(frozen) = self.frozen_walk.take() {
            frozen::profile(frozen::Ct2Profile::walk_end);
            crate::perf_mark("pass2.walked");
            // The jobs still running hold it (a failing walk).
            pipe.ahead.clear();
            self.thaw(state, frozen);
            crate::perf_mark("pass2.thawed");
            if result.is_err() {
                // A failed walk merged only part of what it published: no
                // later walk reads those results.
                state.walk_results = None;
            }
            // T2: every published result merged, so no entry of the index
            // is provisional any more (one left would name a producer no
            // merge chose).
            if result.is_ok()
                && let Some((path, stamp)) = state
                    .artifact_index
                    .shared
                    .first_provisional(u32::try_from(state.done).unwrap_or(u32::MAX))
            {
                result = Err(format!(
                    "internal error: BAZEL_RS_ANALYSIS=eval: the artifact of {path} published by plan entry {stamp} was never merged (BAZEL_RS_EVAL_NO_PUBLISH=1 waits for the merges)"
                ));
            }
        }
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
            bump(&stats().walk_depth[bucket], u64::from(pipe.max_depth));
            stats().deepest.fetch_max(
                u64::from(pipe.max_depth),
                std::sync::atomic::Ordering::Relaxed,
            );
        }
        // A failed lowering queued before the walk's error (or its end) is
        // the first error.
        let drained = self.drain_lowerings(state, lowering::Drain::All);
        state.nodes.deferring = deferring;
        let result = drained.and(result);
        if result.is_ok() && super::sweep::verify() && !self.semantic_only() {
            super::sweep::check_ids(state, &self.action_executions);
        }
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
            {
                let _split = frozen::Split::enter(1);
                self.drain_lowerings(state, lowering::Drain::Bounded)?;
            }
            // S3 pass 2: what the tape applied before this turn, after
            // everything before it is placed (a deferred flush's walk
            // ended there: its failures are its own).
            if self
                .eval_pass2
                .as_ref()
                .is_some_and(|pending| pending.due(state.done))
            {
                let _split = frozen::Split::enter(2);
                self.drain_lowerings(state, lowering::Drain::All)?;
                self.eval_pass2_turn(state.done);
            }
            self.eval_pass2_track(state.done);
            let index = state.done;
            state.done += 1;
            crate::debug_panic::at("coordinator");
            if self.frozen_walk.is_some() {
                frozen::profile(|profile| profile.turn(index));
            }
            // Registrations made ahead of their turn become visible at it.
            state.artifact_index.at_turn(index);
            state.producers.set_stamp(Some(index));
            state.producers.turn = state.producers.stamp;
            crate::progress::analysis_configured(state.done);
            {
                let _split = frozen::Split::enter(3);
                self.flush_delegations(state);
                self.pipeline_wake(state, pipe);
            }
            {
                let _split = frozen::Split::enter(4);
                self.commit_finished_ahead(state, configured, pipe, index, false);
            }
            let classified = index < pipe.scanned;
            let ahead = pipe
                .ahead
                .remove(&index)
                .or_else(|| pipe.committed.remove(&index).map(Ahead::Committed));
            // S3: a worker still preparing it (its turn waits for it, the
            // coordinator committing others ahead meanwhile).
            let ahead = match ahead {
                Some(Ahead::Preparing(preparing)) => {
                    pipe.computed.insert(index);
                    self.frozen_at_turn(state, configured, pipe, index, preparing)
                }
                ahead => ahead,
            };
            let was_ready = pipe.ready.contains(&index);
            if crate::plan_timings::enabled() {
                pipe.turn_reason = self.note_turn_fallback(
                    pipe, index, &key, &rule, classified, was_ready, &ahead, configured, state,
                );
            }
            if crate::plan_timings::enabled()
                && let Some([Some(ready), Some(dispatched), finished, noticed]) =
                    pipe.lat.remove(&index)
            {
                let now = std::time::Instant::now();
                let kind = match &ahead {
                    Some(Ahead::Committed(_)) => "ahead",
                    _ => "turn",
                };
                let us = |a: std::time::Instant, b: std::time::Instant| {
                    b.saturating_duration_since(a).as_micros() as u64
                };
                lat_add(&format!("{kind}_ready_to_dispatch"), us(ready, dispatched));
                if let (Some(finished), Some(noticed)) = (finished, noticed) {
                    lat_add(
                        &format!("{kind}_dispatch_to_finish"),
                        us(dispatched, finished),
                    );
                    lat_add(&format!("{kind}_finish_to_notice"), us(finished, noticed));
                    lat_add(&format!("{kind}_notice_to_turn"), us(noticed, now));
                } else {
                    lat_add(
                        &format!("{kind}_dispatch_to_turn_unfinished"),
                        us(dispatched, now),
                    );
                }
            }
            pipe.ready.remove(&index);
            pipe.early.remove(&index);
            pipe.native_jobs.remove(&index);
            pipe.native_ready.remove(&index);
            pipe.finished.remove(&index);
            pipe.computed.remove(&index);
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
            let result = {
                let bucket = match &ahead {
                    Some(Ahead::Committed(_)) => 5,
                    _ if !matches!(rule.kind, RuleKind::Custom) => 6,
                    _ => 7,
                };
                let _split = frozen::Split::enter(bucket);
                self.pipelined_turn(state, configured, pipe, &key, &rule, index, ahead)
            };
            pipe.note_committed(index, &key.label);
            if !self.keep_going {
                result?;
            } else {
                match result {
                    Ok(()) => self.keep_going_state.planned(&key),
                    Err(error) => self.keep_going_state.fail_own(&key, error),
                }
            }
            if crate::plan_timings::enabled() {
                pipe.waking = match &rule.kind {
                    RuleKind::Custom => format!(
                        "turn_starlark_{}_{}",
                        pipe.turn_reason,
                        rule.rule_class_name()
                    ),
                    RuleKind::Native(name) => (*name).to_owned(),
                    RuleKind::Filegroup => "filegroup".to_owned(),
                    RuleKind::Genrule => "genrule".to_owned(),
                    RuleKind::PackageGroup(_) => "package_group".to_owned(),
                };
            }
            {
                let _split = frozen::Split::enter(11);
                self.pipeline_committed(state, pipe, &key);
            }
            pipe.waking.clear();
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
    ) -> &'static str {
        let reason = match ahead {
            Some(Ahead::Committed(_)) if !matches!(rule.kind, RuleKind::Custom) => {
                "native_committed_ahead"
            }
            Some(Ahead::Committed(_)) => "committed_ahead",
            Some(Ahead::Failed { .. }) => "prepare_failed",
            Some(Ahead::Panicked(_)) => "prepare_panicked",
            Some(Ahead::Preparing(_)) => "preparing",
            Some(Ahead::Computing { .. }) if !self.commits_ahead(state) => "commit_ahead_off",
            Some(Ahead::Computing { .. }) => match pipe.last_try.get(&index).copied() {
                Some(reason) => reason,
                None => {
                    diag(&format!(
                        "late_{}_by_{}",
                        rule.rule_class_name(),
                        pipe.woken_by
                            .get(&index)
                            .map_or("?", |cause| if cause.is_empty() {
                                "classify"
                            } else {
                                cause
                            })
                    ));
                    "late"
                }
            },
            None if pipe.classified_never.contains(&index) => return "never",
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
            None if pipe.natives.contains(&index) => {
                if was_ready || pipe.native_ready.contains(&index) {
                    "native_ready_late"
                } else {
                    "native_dependency_late"
                }
            }
            None if was_ready => {
                diag(&format!(
                    "ready_late_{}_by_{}",
                    rule.rule_class_name(),
                    pipe.woken_by
                        .get(&index)
                        .map_or("?", |cause| if cause.is_empty() {
                            "classify"
                        } else {
                            cause
                        })
                ));
                "ready_late"
            }
            None => "dependency_late",
        };
        fallback(reason);
        reason
    }

    /// Wake the entries waiting for the delegations flushed since.
    fn pipeline_wake(&mut self, state: &mut ActionPlanning, pipe: &mut Pipeline) {
        pipe.set_waking("delegation");
        for key in std::mem::take(&mut state.delegated_keys) {
            self.pipeline_committed(state, pipe, &key);
        }
        pipe.waking.clear();
    }

    /// T2: wait for the first of `keys` neither committed nor published,
    /// or mark the native rule `index` ready to have its results computed.
    fn native_publish_wait(
        state: &ActionPlanning,
        pipe: &mut Pipeline,
        index: usize,
        mut keys: Vec<ConfiguredTargetKey>,
    ) {
        while let Some(key) = keys.pop() {
            if !state.dry.contains_key(&key) && !pipe.published.contains_key(&key) {
                pipe.native_waiting
                    .entry(key)
                    .or_default()
                    .push((index, keys));
                return;
            }
        }
        pipe.native_publish_ready.push(index);
    }

    /// The native rules waiting for `key` (committed or published).
    fn native_publish_wake(state: &ActionPlanning, pipe: &mut Pipeline, key: &ConfiguredTargetKey) {
        if let Some(waiters) = pipe.native_waiting.remove(key) {
            for (index, rest) in waiters {
                if index >= state.done {
                    Self::native_publish_wait(state, pipe, index, rest);
                }
            }
        }
    }

    /// T2: compute the results of the ready native rules on the workers.
    fn dispatch_natives(&mut self, pipe: &mut Pipeline, turn: usize) {
        let Some(frozen) = self.frozen_walk.clone() else {
            return;
        };
        for index in std::mem::take(&mut pipe.native_publish_ready) {
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
            let job = self.dispatch_native(&frozen, index, key, rule);
            pipe.native_jobs.insert(index, job);
        }
    }

    /// T2: the Starlark entries waiting for the results published since
    /// (natives keep waiting for the merge); whether one woke.
    fn pipeline_published(&mut self, state: &ActionPlanning, pipe: &mut Pipeline) -> bool {
        let mut woke = false;
        for key in std::mem::take(&mut pipe.newly_published) {
            Self::native_publish_wake(state, pipe, &key);
            let Some(waiters) = pipe.waiting.remove(&key) else {
                continue;
            };
            let mut natives = Vec::new();
            for (index, rest) in waiters {
                if index < state.done {
                    continue;
                }
                if pipe.natives.contains(&index) {
                    natives.push((index, rest));
                    continue;
                }
                woke = true;
                pipe.set_waking("published");
                Self::wait_or_ready(state, pipe, index, rest);
            }
            if !natives.is_empty() {
                pipe.waiting.entry(key).or_default().extend(natives);
            }
        }
        pipe.waking.clear();
        woke || !pipe.native_publish_ready.is_empty()
    }

    /// `key` is committed: recheck the entries waiting for it.
    fn pipeline_committed(
        &mut self,
        state: &ActionPlanning,
        pipe: &mut Pipeline,
        key: &ConfiguredTargetKey,
    ) {
        // S3: what the workers' prepares read of it, before they may run.
        if let Some(index) = pipe.published.remove(key)
            && let Some((_, rule)) = self.planning_plan.as_ref().and_then(|plan| plan.get(index))
        {
            let rule = Arc::clone(rule);
            self.check_published(state, key, &rule);
        }
        if let Some(frozen) = &self.frozen_walk {
            frozen.results.publish(key, state);
            frozen.results.publish_declared(state, self);
            Self::native_publish_wake(state, pipe, key);
        }
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
        // T2: a Starlark rule's prepare (on a worker) reads published
        // results; a native rule's commit reads the planning state.
        let starlark = !pipe.natives.contains(&index);
        while let Some(key) = keys.pop() {
            if !state.dry.contains_key(&key) {
                if starlark && pipe.published.contains_key(&key) {
                    pipe.early.entry(index).or_default().push(key);
                    continue;
                }
                pipe.waiting.entry(key).or_default().push((index, keys));
                return;
            }
        }
        if crate::plan_timings::enabled() {
            pipe.woken_by.insert(index, pipe.waking.clone());
        }
        if crate::plan_timings::enabled() {
            pipe.lat.entry(index).or_default()[0] = Some(std::time::Instant::now());
        }
        if pipe.natives.contains(&index) {
            pipe.native_ready.insert(index);
        } else {
            pipe.ready.insert(index);
        }
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
            Some(Ahead::Panicked(panic)) => panic!("{panic}"),
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
                    native_requirements,
                } = *committed;
                pipe.committed_entries -= segment.entries();
                if !platform_known && self.platform_resolved(key) {
                    // Never committed ahead (Self::commit_ahead); the turn
                    // would have prepared it again.
                    bump(&stats().committed_platform_late, 1);
                    assert!(
                        !verify_ahead(),
                        "internal error: BAZEL_RS_VERIFY_AHEAD: {} was committed ahead with a deferred target platform resolved before its turn",
                        key.label
                    );
                    debug_assert!(
                        false,
                        "internal error: {} was committed ahead with a deferred target platform resolved before its turn",
                        key.label
                    );
                }
                if let Some(keys) = native_requirements {
                    self.verify_native_requirements(key, rule, keys);
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
            // Reported before its turn (Self::plan_targets_pipelined).
            Some(Ahead::Preparing(_)) | None => {
                self.prepare_at_turn(state, configured, key, rule, index, class)?
            }
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
        let mut pending = pending;
        let worker = loop {
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
            // S1: a delta meeting a producer registered since it was built
            // (an earlier sibling's shared action) is computed again on a
            // worker, which keeps the analysis result for the build here,
            // while this turn commits others ahead.
            match worker.outcome.delta.as_ref().map(|worker| &worker.built) {
                Some(delta::Built::Delta(built))
                    if Self::delta_meets_producers(state, built)
                        && self.analysis_pool.is_some() =>
                {
                    let WorkerOutcome {
                        snapshot, effects, ..
                    } = worker;
                    if !effects.console.is_empty() {
                        note_lost(effects.console.len());
                    }
                    effects.apply(self, false);
                    let delta = self.delta_inputs(state, rule, &parts, class);
                    pending =
                        self.dispatch(snapshot, parts.transitive.clone(), index, class, delta);
                    note_delta("recomputed_on_worker", std::time::Duration::ZERO);
                }
                _ => break worker,
            }
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
        let started = std::time::Instant::now();
        let prepared = self.prepare_target(state, key, rule, configured);
        bump(
            &stats().coordinator_turn_prepare_us,
            started.elapsed().as_micros() as u64,
        );
        let prepared = prepared?;
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
                crate::debug_panic::at("analysis-worker");
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
                let (worker, registers) = run_compute(
                    snapshot,
                    &transitive,
                    index,
                    class,
                    delta,
                    compact_repo_mapping,
                    verify_summary,
                );
                *summary = registers;
                worker
            }));
            let result = result.map_err(|_| {
                WORKER_PANIC
                    .with(|slot| slot.borrow_mut().take())
                    .unwrap_or_else(|| "an analysis worker panicked".to_owned())
            });
            if result.is_err() {
                summary = None;
            }
            let platform_read = result.as_ref().map_or(true, |worker| {
                worker.sentinel || worker.outcome.platform_read
            });
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

    /// Whether the target platform of `key`'s configuration is resolved
    /// (Engine::platforms, which prepare's ctx surface reads).
    /// Whether a deferred flush's op of the walk publishes the target
    /// platform of `key`'s configuration before entry `index`'s turn
    /// (`eval::linearize::pass2`): its turn sees it resolved.
    fn pass2_publishes_platform(&self, key: &ConfiguredTargetKey, index: usize) -> bool {
        let Some(pending) = &self.eval_pass2 else {
            return false;
        };
        let target = ConfiguredTargetKey::new(
            self.configurations
                .get(key.config)
                .native
                .target_platform()
                .clone(),
            key.config,
        );
        pending.ops.iter().any(|(position, op)| {
            *position <= index
                && matches!(
                    op,
                    eval::linearize::pass2::Op::Publish(
                        eval::linearize::driver::Published::Platform(published, _)
                    ) if *published == target
                )
        })
    }

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
        // S2: the native rules whose dependencies are committed commit ahead
        // of their turn first (waking their dependents), then the Starlark
        // rules are prepared, until no native is ready.
        loop {
            self.commit_natives_ahead(state, configured, pipe, turn);
            self.fill_lookahead_once(state, configured, pipe, turn);
            if pipe.native_ready.range(turn + 1..).next().is_none()
                || pipe.committed_entries >= committed_limit()
            {
                break;
            }
        }
    }

    /// [`Self::fill_lookahead`]'s classification and Starlark prepares.
    fn fill_lookahead_once(
        &mut self,
        state: &mut ActionPlanning,
        configured: &mut ConfiguredNodes,
        pipe: &mut Pipeline,
        turn: usize,
    ) {
        self.dispatch_natives(pipe, turn);
        let plan_len = self.planning_plan.as_ref().map_or(0, Vec::len);
        let (horizon, held) = if self.frozen_walk.is_some() {
            frozen::bounds()
        } else {
            (HORIZON, pipe.capacity * HELD_FACTOR)
        };
        let horizon = plan_len.min(turn + 1 + horizon);
        if pipe.scanned < turn + 1 {
            pipe.scanned = turn + 1;
        }
        // In flight: the entries prepared ahead whose computes have not
        // finished (a finished one waiting to commit takes no worker); the
        // finished ones waiting are bounded too (their snapshots and
        // results are held: memory).
        while pipe.ahead.len().saturating_sub(pipe.computed.len()) < pipe.capacity
            && pipe.ahead.len() < held
        {
            // S2: the ready entries the pipeline has room for are prepared
            // together (Self::prepare_ahead_batch); more are classified
            // only when none is ready (the ready ones go first).
            let room = (pipe.capacity - pipe.ahead.len().saturating_sub(pipe.computed.len()))
                .min(held - pipe.ahead.len());
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
                let classified = {
                    let _split = frozen::Split::enter(8);
                    self.classify(state, configured, &key, &rule, &mut stash)
                };
                if crate::plan_timings::enabled() {
                    // The walk's chains: an entry's depth is its deepest
                    // requirement's plus one for a Starlark implementation.
                    let keys = CLASSIFIED_KEYS.with(|slot| slot.borrow_mut().take());
                    let deepest = keys
                        .iter()
                        .flatten()
                        .filter_map(|key| pipe.depth.get(key))
                        .max()
                        .copied()
                        .unwrap_or(0);
                    let own = u32::from(matches!(rule.kind, RuleKind::Custom));
                    let depth = deepest + own;
                    pipe.depth.insert(key.clone(), depth);
                    pipe.max_depth = pipe.max_depth.max(depth);
                }
                if let Some(keys) = VERIFY_NATIVE_KEYS.with(|slot| slot.borrow_mut().take()) {
                    pipe.native_keys.insert(index, keys);
                }
                if !stash.is_empty() {
                    pipe.stashed.entry(index).or_default().extend(stash);
                }
                // A Starlark rule never prepared ahead may apply aspects
                // (outputs in other packages) at its turn.
                let barrier = matches!(classified, Classified::Never)
                    && matches!(rule.kind, RuleKind::Custom);
                pipe.note_pending(index, &key.label, barrier, registers_no_output(&rule));
                if matches!(rule.kind, RuleKind::Custom | RuleKind::Genrule)
                    && configured
                        .get(&key)
                        .is_some_and(|node| node.incompatible.is_none())
                {
                    pipe.note_platform_pending(index, key.config);
                }
                if !matches!(classified, Classified::Never)
                    && !matches!(rule.kind, RuleKind::Custom)
                {
                    pipe.natives.insert(index);
                }
                if self.frozen_walk.is_some()
                    && !matches!(rule.kind, RuleKind::Custom)
                    && configured
                        .get(&key)
                        .is_some_and(|node| node.incompatible.is_none())
                    && frozen::publishes_native(self, &rule, key.config)
                {
                    // T2: its results computed on a worker once what it
                    // reads is published (a native rule committed at its
                    // turn too: a filegroup selecting an output group).
                    let keys = match &classified {
                        Classified::Waiting(keys) => Some(keys.clone()),
                        Classified::Ready => Some(Vec::new()),
                        Classified::Never if !verify_ahead() => {
                            let mut lookups = Vec::new();
                            let keys = match self.classify_native(state, &key, &rule, &mut lookups)
                            {
                                Classified::Waiting(keys) => Some(keys),
                                Classified::Ready => Some(Vec::new()),
                                Classified::Never => None,
                            };
                            if !lookups.is_empty() {
                                pipe.stashed.entry(index).or_default().extend(lookups);
                            }
                            keys
                        }
                        Classified::Never => None,
                    };
                    if let Some(keys) = keys {
                        Self::native_publish_wait(state, pipe, index, keys);
                    }
                }
                match classified {
                    Classified::Never => {
                        bump(&stats().never, 1);
                        pipe.classified_never.insert(index);
                    }
                    Classified::Waiting(keys) => Self::wait_or_ready(state, pipe, index, keys),
                    Classified::Ready => Self::wait_or_ready(state, pipe, index, Vec::new()),
                }
                continue;
            }
            // S2: the ready entries the pipeline has room for, prepared
            // together off the coordinator (Self::prepare_ahead_batch).
            let mut batch = Vec::new();
            while batch.len() < room {
                let Some(index) = pipe.ready.pop_first() else {
                    break;
                };
                if index <= turn {
                    continue;
                }
                if let Some((key, rule)) = self
                    .planning_plan
                    .as_ref()
                    .and_then(|plan| plan.get(index))
                    .cloned()
                {
                    batch.push((index, key, rule));
                }
            }
            if batch.is_empty() {
                if pipe.ready.is_empty() {
                    break;
                }
                continue;
            }
            if let Some(frozen) = self.frozen_walk.clone() {
                // S3: prepared and computed on the workers.
                let _split = frozen::Split::enter(10);
                for (index, key, rule) in batch {
                    if crate::plan_timings::enabled() {
                        pipe.lat.entry(index).or_default()[1] = Some(std::time::Instant::now());
                    }
                    let preparing = self.dispatch_frozen(&frozen, index, key, rule);
                    pipe.ahead.insert(index, Ahead::Preparing(preparing));
                }
                continue;
            }
            self.prepare_ahead_batch(state, configured, pipe, batch);
        }
    }

    /// S2: prepare the ready entries of `batch` ahead of their turn on
    /// prepare threads while the coordinator waits ([`prepare::SharedPrepare`]:
    /// the engine and the planning state read through shared references,
    /// what a prepare writes recorded and applied here in plan order), and
    /// dispatch their computes. An entry whose prepare needs the coordinator
    /// is prepared here ([`Self::prepare_ahead`], `PERF-PARALLEL
    /// prepare_fallback`).
    fn prepare_ahead_batch(
        &mut self,
        state: &mut ActionPlanning,
        configured: &mut ConfiguredNodes,
        pipe: &mut Pipeline,
        batch: Vec<(usize, ConfiguredTargetKey, Arc<Rule>)>,
    ) {
        let coordinator_started = std::time::Instant::now();
        if !shared_prepares() {
            for (index, key, rule) in batch {
                self.prepare_ahead_one(state, configured, pipe, index, &key, &rule);
            }
            bump(
                &stats().coordinator_prepare_us,
                coordinator_started.elapsed().as_micros() as u64,
            );
            return;
        }
        let platform_known: Vec<bool> = batch
            .iter()
            .map(|(_, key, _)| self.platform_resolved(key))
            .collect();
        let context = incremental::ScopeContext::current();
        let memos = prepare::BatchMemos::default();
        let outcomes = {
            let engine: &Engine = self;
            let shared_state: &ActionPlanning = state;
            let shared_configured: &ConfiguredNodes = configured;
            let run = |(index, key, rule): &(usize, ConfiguredTargetKey, Arc<Rule>)| {
                let class = crate::plan_timings::class_id(rule.rule_class_name());
                let isolation = Isolation::enter();
                let started = std::time::Instant::now();
                let timer = crate::plan_timings::StageTimer::start(
                    crate::plan_timings::PlanStage::Prepare,
                    *index as u64,
                    class,
                );
                let mut cx = prepare::SharedPrepare::new(
                    engine,
                    shared_state,
                    shared_configured,
                    &memos,
                    *index,
                );
                let prepared = Engine::prepare_custom(&mut cx, key, rule);
                timer.finish();
                let elapsed = started.elapsed();
                let (effects, sentinel) = isolation.finish();
                SharedOutcome {
                    prepared,
                    record: cx.record,
                    effects,
                    sentinel,
                    elapsed,
                }
            };
            let started = std::time::Instant::now();
            let outcomes = run_batch(&batch, &run);
            bump(&stats().batch_run_us, started.elapsed().as_micros() as u64);
            outcomes
        };
        self.apply_batch_memos(state, memos);
        bump(&stats().batches, 1);
        bump(&stats().batch_entries, batch.len() as u64);
        for (((index, key, rule), outcome), platform_known) in
            batch.into_iter().zip(outcomes).zip(platform_known)
        {
            let SharedOutcome {
                prepared,
                record,
                effects,
                sentinel,
                elapsed,
            } = outcome;
            let prepare::SharedRecord {
                accesses,
                owner,
                abort,
                ..
            } = record;
            if let Some(why) = abort {
                // The coordinator prepares it (what the attempt observed
                // counts: a memo it filled is a hit there).
                prepare_fallback(why);
                if !effects.console.is_empty() {
                    note_lost(effects.console.len());
                }
                pipe.stashed.entry(index).or_default().push(effects);
                self.prepare_ahead_one(state, configured, pipe, index, &key, &rule);
                continue;
            }
            bump(&stats().shared_prepares, 1);
            if let Some((action_owner, console, owner_key)) = owner {
                self.console_owners.insert(action_owner.clone(), console);
                self.aquery_owner(&action_owner, &owner_key, Vec::new);
            }
            let track = if self.tracking() {
                incremental::TrackLog::accesses(accesses, context)
            } else {
                incremental::TrackLog::default()
            };
            let aborted = sentinel
                || prepared
                    .as_ref()
                    .is_err_and(|error| crate::repository_demand::is_inline_rerun_sentinel(error));
            let ahead = self.prepared_ahead(
                state,
                &rule,
                index,
                prepared,
                effects,
                track,
                aborted,
                platform_known,
                elapsed,
                &mut |stash| pipe.stashed.entry(index).or_default().push(stash),
            );
            self.note_prepared_ahead(pipe, index, ahead);
        }
        bump(
            &stats().coordinator_prepare_us,
            coordinator_started.elapsed().as_micros() as u64,
        );
    }

    /// Prepare one ready entry ahead of its turn on the coordinator
    /// ([`Self::prepare_ahead`]).
    fn prepare_ahead_one(
        &mut self,
        state: &mut ActionPlanning,
        configured: &mut ConfiguredNodes,
        pipe: &mut Pipeline,
        index: usize,
        key: &ConfiguredTargetKey,
        rule: &Arc<Rule>,
    ) {
        let mut stash = Vec::new();
        let ahead = self.prepare_ahead(state, configured, key, rule, index, &mut stash);
        if !stash.is_empty() {
            pipe.stashed.entry(index).or_default().extend(stash);
        }
        self.note_prepared_ahead(pipe, index, ahead);
    }

    fn note_prepared_ahead(&mut self, pipe: &mut Pipeline, index: usize, ahead: Option<Ahead>) {
        match ahead {
            Some(ahead) => {
                if crate::plan_timings::enabled() {
                    pipe.lat.entry(index).or_default()[1] = Some(std::time::Instant::now());
                }
                pipe.ahead.insert(index, ahead);
            }
            None => {
                pipe.prepare_aborted.insert(index);
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
        if !matches!(rule.kind, RuleKind::Custom) {
            if self.commits_ahead(state)
                && native_ahead_enabled()
                && native_ahead_eligible(rule)
                && configured
                    .get(key)
                    .is_some_and(|node| node.incompatible.is_none())
            {
                return self.classify_native(state, key, rule, stash);
            }
            return never("native");
        }
        let Some(custom) = rule.custom.as_ref() else {
            return never("native");
        };
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
                note_classified_keys(&keys);
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

    /// S2: classify the native rule `key` to commit ahead of its turn once
    /// the keys its prepare and commit read are committed
    /// ([`Self::native_requirements`], lookups only, as [`Self::classify`]).
    fn classify_native(
        &mut self,
        state: &ActionPlanning,
        key: &ConfiguredTargetKey,
        rule: &Rule,
        stash: &mut Vec<Effects>,
    ) -> Classified {
        self.ahead = true;
        self.ahead_aborted = false;
        self.track_defer_start();
        let isolation = Isolation::enter();
        let keys = self.native_requirements(key, rule);
        if verify_ahead() {
            // Checked at the commit ahead and at the turn.
            VERIFY_NATIVE_KEYS.with(|slot| *slot.borrow_mut() = keys.clone());
        }
        let (effects, sentinel) = isolation.finish();
        drop(self.track_defer_finish());
        self.ahead = false;
        let aborted = std::mem::take(&mut self.ahead_aborted);
        let quiet = effects.console.is_empty()
            && effects.observations.is_empty()
            && effects.roots.is_empty();
        if !quiet {
            if !effects.console.is_empty() {
                note_lost(effects.console.len());
            }
            note_effects("classify", &effects);
            stash.push(effects);
        }
        match keys {
            Some(keys) if !aborted && !sentinel && quiet => {
                note_classified_keys(&keys);
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
            _ => {
                fallback("native");
                Classified::Never
            }
        }
    }

    /// The committed keys whose planning state a native rule's prepare and
    /// commit read (Engine::commit_native_target): its every dependency
    /// edge (an output file's generating rule) in the edge's configuration,
    /// and its resolved toolchains.
    fn native_requirements(
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
        let mut labels = Vec::with_capacity(edges.len());
        for edge in &edges {
            let label = self.resolve_rule_src(rule, &edge.label).ok()?;
            let config = match &edge.cfg {
                EdgeCfg::Target => key.config,
                EdgeCfg::Exec(None) => exec_config,
                EdgeCfg::Exec(Some(group)) => self
                    .group_exec_configurations
                    .get(&(key.clone(), group.clone()))
                    .copied()
                    .unwrap_or(key.config),
                EdgeCfg::NoConfig => self.configurations.intern_no_config(),
                EdgeCfg::Starlark(_) => return None,
            };
            labels.push((label, config));
        }
        if let Some(context) = self.toolchain_contexts.get(key) {
            for implementation in context.resolved_labels() {
                labels.push((implementation, key.config));
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
        // Waited for in reverse (Self::wait_or_ready pops).
        keys.reverse();
        Some(keys)
    }

    /// BAZEL_RS_VERIFY_AHEAD: at the turn of a native rule committed ahead,
    /// the keys its prepare and commit read are the ones it waited for
    /// (lookups only, as its classification).
    fn verify_native_requirements(
        &mut self,
        key: &ConfiguredTargetKey,
        rule: &Rule,
        waited: Vec<ConfiguredTargetKey>,
    ) {
        self.ahead = true;
        self.ahead_aborted = false;
        self.track_defer_start();
        let isolation = Isolation::enter();
        let again = self.native_requirements(key, rule);
        let (effects, _) = isolation.finish();
        drop(self.track_defer_finish());
        self.ahead = false;
        self.ahead_aborted = false;
        effects.apply(self, false);
        let set = |keys: &[ConfiguredTargetKey]| keys.iter().cloned().collect::<BTreeSet<_>>();
        if again.as_deref().map(set) != Some(set(&waited)) {
            panic!(
                "internal error: BAZEL_RS_VERIFY_AHEAD: the native rule {} committed ahead of its turn waited for {:?}, its turn reads {:?}",
                key.label, waited, again
            );
        }
        bump(&stats().verified_native, 1);
    }

    /// S2: commit the native rules whose dependencies are committed ahead of
    /// their turn ([`Self::commit_native_ahead`]).
    fn commit_natives_ahead(
        &mut self,
        state: &mut ActionPlanning,
        configured: &mut ConfiguredNodes,
        pipe: &mut Pipeline,
        turn: usize,
    ) {
        while pipe.committed_entries < committed_limit() {
            let Some(index) = pipe.native_ready.pop_first() else {
                break;
            };
            if index <= turn {
                continue;
            }
            let _split = frozen::Split::enter(9);
            self.commit_native_ahead(state, configured, pipe, index);
        }
    }

    /// S2: prepare and commit the native rule at plan index `index` ahead of
    /// its turn, as a Starlark commit ahead ([`Self::commit_ahead`]): it
    /// registers no output (its outputs are settled), every key it reads is
    /// committed; its console, observations, obtained roots, tracker events,
    /// warnings and errors are held for its turn, its registrations stamped
    /// with its index. A prepare or commit that needed a fetch, an analysis
    /// or a package is the internal error naming the workaround at its turn
    /// (as a Starlark commit ahead).
    fn commit_native_ahead(
        &mut self,
        state: &mut ActionPlanning,
        configured: &mut ConfiguredNodes,
        pipe: &mut Pipeline,
        index: usize,
    ) {
        let Some((key, rule)) = self
            .planning_plan
            .as_ref()
            .and_then(|plan| plan.get(index))
            .cloned()
        else {
            return;
        };
        let native_requirements = pipe.native_keys.remove(&index);
        if let Some(keys) = &native_requirements
            && let Some(missing) = keys.iter().find(|key| !state.dry.contains_key(*key))
        {
            panic!(
                "internal error: BAZEL_RS_VERIFY_AHEAD: the native rule {} committed ahead of its turn before {} it reads",
                key.label, missing.label
            );
        }
        let marks = (self.warnings.len(), self.errors.len());
        let main_nodes = std::mem::replace(&mut state.nodes, lowering::PlannedNodes::segment());
        self.ahead = true;
        self.ahead_aborted = false;
        self.track_defer_start();
        let isolation = Isolation::enter();
        let stamp = state.artifact_index.stamp_ahead(index);
        let producer_stamp = state.producers.set_stamp(Some(index));
        let started = std::time::Instant::now();
        let result = self.plan_one_target_body(state, &key, &rule, configured, index as u64);
        let elapsed = started.elapsed();
        state.artifact_index.restore_stamp(stamp);
        state.producers.restore_stamp(producer_stamp);
        let (mut commit_effects, sentinel) = isolation.finish();
        note_effects("native_ahead", &commit_effects);
        let commit_track = self.track_defer_finish();
        self.ahead = false;
        let aborted = std::mem::take(&mut self.ahead_aborted);
        let mut segment = std::mem::replace(&mut state.nodes, main_nodes);
        segment.segment_close(std::mem::take(&mut commit_effects.console));
        pipe.committed_entries += segment.entries();
        let injected = test_abort_ahead();
        let result = if sentinel || aborted || injected {
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
                prepare_effects: Effects::default(),
                prepare_track: incremental::TrackLog::default(),
                compute_effects: Effects::default(),
                commit_effects,
                commit_track,
                segment,
                warnings,
                errors,
                marks,
                result,
                elapsed,
                // A native rule's commit reads no target platform.
                platform_known: true,
                verified: None,
                native_requirements,
            }),
        );
        pipe.note_committed(index, &key.label);
        pipe.set_waking("native_ahead");
        self.pipeline_committed(state, pipe, &key);
        pipe.waking.clear();
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
    /// Task D: [`Self::commits_ahead`] of the walk to come (which defers
    /// its lowerings unless --keep_going).
    pub(super) fn commits_ahead_when_walking(&self) -> bool {
        !self.keep_going
            && !self.semantic_only()
            && self.aquery_capture.is_none()
            && commit_ahead_enabled()
    }

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
        let finished = pool.finished.1.lock().unwrap_or_else(|e| e.into_inner());
        if block {
            match finished.recv() {
                Ok(finished) => received.push(finished),
                Err(_) => return false,
            }
        }
        while let Ok(finished) = finished.try_recv() {
            received.push(finished);
        }
        drop(finished);
        let woke = self.pipeline_published(state, pipe);

        for (index, summary, platform_read, at) in received {
            if crate::plan_timings::enabled() {
                let entry = pipe.lat.entry(index).or_default();
                entry[2] = Some(at);
                entry[3] = Some(std::time::Instant::now());
            }
            if let Some(job) = pipe.native_jobs.remove(&index) {
                // T2: a native rule's results.
                if index > turn
                    && let Ok(Some(published)) = job.recv()
                    && let Some((key, _)) = self
                        .planning_plan
                        .as_ref()
                        .and_then(|plan| plan.get(index))
                        .cloned()
                {
                    self.publish_native(state, pipe, index, &key, *published);
                }
                continue;
            }
            if index <= turn {
                continue;
            }
            // S3: the worker prepared it too.
            if let Some(Ahead::Preparing(_)) = pipe.ahead.get(&index)
                && let Some(Ahead::Preparing(preparing)) = pipe.ahead.remove(&index)
            {
                match self.frozen_prepared(pipe, index, preparing) {
                    Some(ahead) => {
                        pipe.ahead.insert(index, ahead);
                    }
                    None => continue,
                }
            }
            pipe.finished.insert(index);
            pipe.computed.insert(index);
            if !platform_read {
                pipe.platform_settled(index);
            }
            // Its outputs are known when its turn commits this compute: no
            // sentinel, and a prepare that saw its target platform resolved
            // or an implementation that did not ask for it (its turn may
            // prepare it again with the platform resolved: the same
            // declared outputs).
            if let (Some(summary), Some(Ahead::Computing { platform_known, .. })) =
                (summary, pipe.ahead.get(&index))
                && (*platform_known || !platform_read)
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
        let woke = woke | self.pipeline_published(state, pipe);
        if committed || woke {
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
        if !matches!(pipe.ahead.get(&index), Some(Ahead::Computing { .. })) {
            return Retry::Done(false);
        }
        if let Some(first) = pipe.barriers.range(..index).next_back().copied() {
            bump(&stats().ahead_barrier, 1);
            pipe.last_try.insert(index, "unsettled_barrier");
            pipe.park(index, first);
            return Retry::Later;
        }
        // What waits for its turn is bounded (nodes and lowerings of the
        // entries committed ahead), like the lowering queue.
        if pipe.committed_entries >= committed_limit() {
            bump(&stats().ahead_budget, 1);
            pipe.last_try.insert(index, "budget");
            return Retry::Later;
        }
        // T2: what it became ready on is merged (its closure's artifacts
        // are canonical).
        if let Some(early) = pipe.early.get_mut(&index) {
            early.retain(|key| !state.dry.contains_key(key));
            if let Some(blocker) = early
                .first()
                .and_then(|key| pipe.published.get(key))
                .copied()
                && blocker < index
            {
                pipe.last_try.insert(index, "provisional_dependency");
                pipe.park(index, blocker);
                return Retry::Later;
            }
            if early.is_empty() {
                pipe.early.remove(&index);
            } else {
                pipe.last_try.insert(index, "provisional_dependency");
                return Retry::Done(false);
            }
        }
        let Some(summary) = pipe.known.get(&index).cloned() else {
            // Its outputs are not known (a sentinel): at its turn.
            pipe.last_try.insert(index, "outputs_unknown");
            return Retry::Done(false);
        };
        if let Some((reason, blocker)) = pipe.unsettled(index, &key.label, &summary) {
            pipe.park(index, blocker);
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
        // A prepare that saw its target platform deferred is prepared again
        // at its turn if a commit resolves it before
        // (Self::pipelined_turn_body): it commits ahead only when nothing
        // before it can resolve it any more (and its own implementation
        // did not ask for it), else at its turn.
        if !platform_known {
            let retry = if worker.outcome.platform_read
                || self.platform_resolved(&key)
                || self.pass2_publishes_platform(&key, index)
            {
                Retry::Done(false)
            } else if let Some(blocker) = pipe.platform_resolver_before(index, key.config) {
                pipe.park(index, blocker);
                Retry::Later
            } else {
                Retry::Done(true)
            };
            if matches!(retry, Retry::Done(true)) {
                diag("committed_ahead_platform_deferred");
            } else {
                put_back(
                    pipe,
                    parts,
                    prepare_effects,
                    prepare_track,
                    refilled(Ok(worker)),
                );
                pipe.last_try.insert(index, "platform_unknown");
                return retry;
            }
        }
        // T2: a delta built over provisional results commits ahead once
        // their publishers merged (parked on the last one) and its reads are
        // the canonical artifacts; a stale one is rebuilt at its turn.
        if let Some(delta::Built::Delta(built)) =
            worker.outcome.delta.as_ref().map(|worker| &worker.built)
            && !built.reads.is_empty()
        {
            let check = delta::check_reads(
                &state.artifact_index.shared,
                &built.reads,
                u32::try_from(index).unwrap_or(u32::MAX - 1),
                false,
            );
            let retry = match check {
                delta::ReadsCheck::Valid => None,
                delta::ReadsCheck::Pending(at) if (at as usize) < index => {
                    pipe.park(index, at as usize);
                    pipe.last_try.insert(index, "provisional_pending");
                    Some(Retry::Later)
                }
                delta::ReadsCheck::Pending(_) => {
                    pipe.last_try.insert(index, "provisional_unknown");
                    Some(Retry::Done(false))
                }
                delta::ReadsCheck::Stale => {
                    pipe.last_try.insert(index, "provisional_stale");
                    Some(Retry::Done(false))
                }
            };
            if let Some(retry) = retry {
                put_back(
                    pipe,
                    parts,
                    prepare_effects,
                    prepare_track,
                    refilled(Ok(worker)),
                );
                return retry;
            }
        }
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
            pipe.computed.remove(&index);
            pipe.last_try.insert(index, "delta_meets_producer");
            note_delta("recomputed_on_worker", std::time::Duration::ZERO);
            return Retry::Done(false);
        }
        let unsafe_reason = if worker.sentinel {
            Some("sentinel")
        } else if let Some(built) = worker.outcome.delta.as_ref() {
            match built.built {
                // Fell back during its build (a first-need registration
                // such as a status file met filling a tool's runfiles
                // group): its commit is built at its turn.
                delta::Built::Consumed(_) => Some("delta_consumed"),
                // S1: decided on the worker, before its delta took the
                // result.
                _ => built.ahead_unsafe,
            }
        } else {
            match worker.outcome.analyzed.as_ref() {
                Ok(analyzed) => {
                    self.commit_ahead_unsafe(state, &rule, &parts, analyzed, &worker.outcome)
                }
                Err(_) => Some("compute_failed"),
            }
        };
        // The transition allowlist check (the commit's prefix) commits ahead
        // when it needs no package load or analysis now.
        let unsafe_reason = match unsafe_reason {
            Some(INELIGIBLE_TRANSITION)
                if self.transition_allowlist_settled(pipe, index, &rule, &key, configured) =>
            {
                None
            }
            reason => reason,
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
        pipe.computed.remove(&index);
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
                native_requirements: None,
            }),
        );
        pipe.note_committed(index, &key.label);
        bump(&stats().committed_ahead, 1);
        pipe.set_waking("ahead");
        self.pipeline_committed(state, pipe, &key);
        pipe.waking.clear();
        Retry::Done(true)
    }

    /// Whether `rule`'s transition allowlist check (the prefix of its commit,
    /// Engine::check_transition_allowlist) runs ahead of its turn on
    /// lookups alone: no package to load, nothing to analyze, nothing
    /// observed or obtained (what it did is stashed for its turn otherwise,
    /// where the check runs again).
    fn transition_allowlist_settled(
        &mut self,
        pipe: &mut Pipeline,
        index: usize,
        rule: &Rule,
        key: &ConfiguredTargetKey,
        configured: &ConfiguredNodes,
    ) -> bool {
        self.ahead = true;
        self.ahead_aborted = false;
        self.track_defer_start();
        let isolation = Isolation::enter();
        let checked = self.check_transition_allowlist(rule, key, configured);
        let (effects, sentinel) = isolation.finish();
        drop(self.track_defer_finish());
        self.ahead = false;
        let aborted = std::mem::take(&mut self.ahead_aborted)
            || checked
                .as_ref()
                .is_err_and(|error| crate::repository_demand::is_inline_rerun_sentinel(error));
        let quiet = effects.console.is_empty()
            && effects.observations.is_empty()
            && effects.roots.is_empty();
        if !quiet {
            if !effects.console.is_empty() {
                note_lost(effects.console.len());
            }
            note_effects("transition_allowlist", &effects);
            pipe.stashed.entry(index).or_default().push(effects);
        }
        !aborted && !sentinel && quiet
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
        state: &ActionPlanning,
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
            delta::first_fill_meets_status(&state.input_groups, analyzed),
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
            keep_on_meet: false,
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
        self.prepared_ahead(
            state,
            rule,
            index,
            prepared,
            effects,
            track,
            aborted,
            platform_known,
            prepare_elapsed,
            &mut |effects| stash.push(effects),
        )
    }

    /// What a prepare ahead of its turn made of `index` (on the coordinator
    /// or a prepare thread): `None` when it aborted (prepared again at its
    /// turn; its effects stashed), else its compute dispatched or its error.
    #[allow(clippy::too_many_arguments)]
    fn prepared_ahead(
        &mut self,
        state: &ActionPlanning,
        rule: &Arc<Rule>,
        index: usize,
        prepared: Result<PreparedTarget, String>,
        effects: Effects,
        track: incremental::TrackLog,
        aborted: bool,
        platform_known: bool,
        prepare_elapsed: std::time::Duration,
        stash: &mut dyn FnMut(Effects),
    ) -> Option<Ahead> {
        let class = crate::plan_timings::class_id(rule.rule_class_name());
        if aborted {
            // Done again at the turn; what it observed counts (memo misses
            // it filled are hits there), its notes and tracker events recur.
            bump(&stats().aborted, 1);
            note_effects("prepare_aborted", &effects);
            if !effects.console.is_empty() {
                note_lost(effects.console.len());
            }
            stash(effects);
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
                    stash(effects);
                    None
                }
            },
        }
    }
}

/// A prepare's result from a prepare thread (S2).
struct SharedOutcome {
    prepared: Result<PreparedTarget, String>,
    record: prepare::SharedRecord,
    effects: Effects,
    sentinel: bool,
    elapsed: std::time::Duration,
}

/// Run `run` over every item of `batch` on the prepare threads
/// ([`prepare_pool`]; the caller runs the first), the results in `batch`'s
/// order.
fn run_batch<I: Sync, T: Send>(batch: &[I], run: &(dyn Fn(&I) -> T + Sync)) -> Vec<T> {
    let pool = match prepare_pool() {
        Some(pool) if batch.len() >= 2 => pool,
        _ => return batch.iter().map(run).collect(),
    };
    let results: Vec<std::sync::Mutex<Option<T>>> =
        batch.iter().map(|_| std::sync::Mutex::new(None)).collect();
    // Every thread claims the next item until none is left: the caller
    // does not wait for a helper still waking up to start one.
    let next = std::sync::atomic::AtomicUsize::new(0);
    let work = || {
        loop {
            let at = next.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let Some(item) = batch.get(at) else {
                break;
            };
            let result = run(item);
            *results[at].lock().unwrap_or_else(|e| e.into_inner()) = Some(result);
        }
    };
    pool.in_place_scope(|scope| {
        let helpers = batch.len().min(prepare_threads() + 1) - 1;
        bump(&stats().batch_threads, helpers as u64);
        for _ in 0..helpers {
            scope.spawn(|_| work());
        }
        work();
    });
    results
        .into_iter()
        .map(|slot| {
            slot.into_inner()
                .unwrap_or_else(|e| e.into_inner())
                .unwrap_or_else(|| panic!("internal error: a prepare of a batch did not run"))
        })
        .collect()
}

/// The prepare threads (S2; 16 MiB stacks like the analysis workers,
/// allocating in the coordinator's jemalloc arena: what they make is kept
/// and freed by the coordinator, src/alloc_arena.rs). `None` without
/// helper threads.
fn prepare_pool() -> Option<&'static rayon_core::ThreadPool> {
    static POOL: std::sync::OnceLock<Option<rayon_core::ThreadPool>> = std::sync::OnceLock::new();
    POOL.get_or_init(|| {
        let threads = prepare_threads();
        if threads == 0 {
            return None;
        }
        let arena = crate::alloc_arena::current();
        rayon_core::ThreadPoolBuilder::new()
            .num_threads(threads)
            .stack_size(LOADING_WORKER_STACK)
            .thread_name(|index| format!("prepare-{index}"))
            .start_handler(move |_| {
                if let Some(arena) = arena {
                    crate::alloc_arena::bind(arena);
                }
            })
            .build()
            .ok()
    })
    .as_ref()
}

/// The prepare threads of a batch besides the coordinator
/// (`BAZEL_RS_PREPARE_THREADS`, hidden). 0 by default: measured on ~/code,
/// a prepare off the coordinator's core takes about twice its CPU (the
/// planning state it reads was just written there) and the coordinator
/// waits for the batch's slowest, so helper threads did not shorten the
/// coordinator's time in prepares (docs/COLD_ANALYSIS.md, "S2, second
/// part"); the tests run with helpers.
fn prepare_threads() -> usize {
    static THREADS: std::sync::OnceLock<usize> = std::sync::OnceLock::new();
    *THREADS.get_or_init(|| {
        // snapshot-unrecorded: a diagnostics override of the scheduling only; the output is the same either way.
        std::env::var("BAZEL_RS_PREPARE_THREADS")
            .ok()
            .and_then(|value| value.parse().ok())
            .unwrap_or(0)
    })
}

/// `BAZEL_RS_NO_SHARED_PREPARE` (hidden): every prepare ahead of its turn
/// runs on the coordinator (S2; the output is the same either way).
fn shared_prepares() -> bool {
    static ENABLED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ENABLED.get_or_init(|| {
        // snapshot-unrecorded: a diagnostics switch choosing where prepares run; the output is the same either way.
        std::env::var_os("BAZEL_RS_NO_SHARED_PREPARE").is_none()
    })
}

/// S2: count a prepare ahead of its turn that needed the coordinator, by
/// the reason (`PERF-PARALLEL prepare_fallback`).
fn prepare_fallback(reason: &'static str) {
    if crate::plan_timings::enabled()
        && let Ok(mut counts) = stats().prepare_fallback.lock()
    {
        *counts.entry(reason).or_default() += 1;
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
/// single-flight since S1: whoever fills them first fills the same value;
/// only a group whose fill would register the workspace status keeps the
/// commit at its turn, `first_fill_status`.)
pub(super) fn ahead_unsafe(
    rule: &Rule,
    output_root: &str,
    analyzed: &analysis::AnalysisResult,
    precomputed: Option<&RunfilesPrecomputed>,
    platforms_known: bool,
    first_fill_status: bool,
) -> Option<&'static str> {
    if !outputs_in_package(rule, output_root, analyzed) {
        return Some("ineligible_outputs_elsewhere");
    }
    if !platforms_known {
        return Some("ineligible_remote_platform");
    }
    if rule.custom.is_none() || restricted_to_error(rule).is_err() {
        return Some("ineligible_restricted");
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
    // A tool runfiles group filled by this commit would register the
    // workspace status on first need.
    if first_fill_status {
        return Some("ineligible_first_fill");
    }
    if analyzed
        .actions
        .iter()
        .any(|action| action.repo_mapping.is_some() || action.build_info.is_some())
    {
        return Some("ineligible_repo_mapping_or_build_info");
    }
    // Last: the coordinator lifts it when the allowlist check needs no
    // package or analysis (Engine::transition_allowlist_settled).
    if rule
        .custom
        .as_ref()
        .is_some_and(|custom| !custom.transitions.is_empty())
    {
        return Some(INELIGIBLE_TRANSITION);
    }
    None
}

/// [`ahead_unsafe`]'s reason for a rule with a Starlark transition: its
/// commit checks the transition allowlist (a package group's package, loaded
/// on first need).
const INELIGIBLE_TRANSITION: &str = "ineligible_transition";

/// S2: the engine, the planning state and the configured nodes are read by
/// the prepares off the coordinator ([`Engine::prepare_batch`]).
const _: () = {
    const fn sync<T: Sync>() {}
    sync::<Engine>();
    sync::<ActionPlanning>();
    sync::<ConfiguredNodes>();
};

/// `BAZEL_RS_T3_DEBUG`.
pub(super) fn debug_t3() -> bool {
    buffer::debug()
}
