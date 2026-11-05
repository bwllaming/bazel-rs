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

/// The analysis workers (16 MiB stacks: Starlark evaluation and depset
/// walks recurse; P2's loading workers use the same).
pub(super) struct AnalysisPool {
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
}

use std::sync::atomic::AtomicU64;

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
    }

    /// Stop the analysis workers (the end of a build's planning).
    pub(super) fn stop_analysis_pool(&mut self) {
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
        let result = self.plan_targets_pipelined(state, configured, &mut pipe);
        state.delegated_keys.clear();
        state.artifact_index.walk_done();
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
            let index = state.done;
            state.done += 1;
            // Registrations made ahead of their turn become visible at it.
            state.artifact_index.at_turn(index);
            crate::progress::analysis_configured(state.done);
            self.flush_delegations(state);
            self.pipeline_wake(state, pipe);
            let ahead = pipe.ahead.remove(&index);
            pipe.ready.remove(&index);
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
                timer.finish();
                return self.commit_turn(
                    state, configured, key, rule, index, class, parts, snapshot, outcome, profiled,
                );
            }
        };
        let waited = std::time::Instant::now();
        let received = pending.recv();
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
        let committed = self.commit_target(state, configured, rule, prepared, Some(outcome));
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
        let (ahead_values, turn_values) =
            (toolchains(&snapshot.rule), toolchains(&turn_snapshot.rule));
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
    ) -> Pending {
        let (sender, receiver) = mpsc::sync_channel(1);
        let jitter = jitter_millis();
        let cancelled = self
            .analysis_pool
            .as_ref()
            .map(|pool| Arc::clone(&pool.cancelled));
        let job: Job = Box::new(move || {
            if cancelled
                .as_ref()
                .is_some_and(|flag| flag.load(std::sync::atomic::Ordering::SeqCst))
            {
                // The pool stopped (a failing build): nobody waits for it.
                return;
            }
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
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
                let elapsed = started.elapsed();
                let (effects, sentinel) = isolation.finish();
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
            let _ = sender.send(result);
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
                match classified {
                    Classified::Never => bump(&stats().never, 1),
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
            if let Some(ahead) =
                self.prepare_ahead(state, configured, &key, &rule, index, &mut stash)
            {
                pipe.ahead.insert(index, ahead);
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
        let Some(custom) = rule.custom.as_ref() else {
            return Classified::Never;
        };
        if !matches!(rule.kind, RuleKind::Custom)
            || configured
                .get(key)
                .is_none_or(|node| node.incompatible.is_some())
            || custom
                .schema
                .values()
                .any(|spec| !spec.declaration.aspects.is_empty())
            || (self.keep_going && self.keep_going_state.dependency_failed(key))
        {
            return Classified::Never;
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
            _ => Classified::Never,
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
                    self.ensure_analysis_pool();
                    let pending = self.dispatch(snapshot, parts.transitive.clone(), index, class);
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
