//! The evaluator's drivers (docs/EVALUATOR.md "Scheduling"): the
//! **inline driver** (one thread: `need` runs each missing node in place,
//! depth first — today's DFS) and the **pool driver** (a rayon-core pool;
//! restart-based, single-flight claims, completion wakes waiters).
//!
//! The contract with node bodies is [`Bodies`]: `run` is the body
//! (`NodeFn` through [`FnBodies`]); `begin`/`finish`/`abandon` bracket
//! every attempt (the hook for the caller's per-attempt Isolation and
//! `observed_fs` frame: a `Missing` attempt is abandoned, a done one
//! finished on the same thread). Values are `Result`s: a failed node is
//! done like any other.
//!
//! Pool driver, per attempt: claim `Queued → Running` (CAS); run the body
//! with a [`Cx`] whose `need` answers from published values only; on a
//! value, publish it and count down the waiters — the ones that reach zero
//! are re-queued (the first continues on this thread, the rest are pushed
//! LIFO on the local deque); on `Missing`, abandon the attempt and park:
//! - `Nodes(ids)`: claim and schedule the fresh ones, register as their
//!   waiter (batched: one park per attempt), re-run when the last is done;
//! - `TargetPlatform(cfg)`: the node [`Bodies::target_platform`] names, as
//!   an ordinary dependency;
//! - `Repo(name)`: parked on the repository; the coordinator's single FIFO
//!   [`RepoLane`] fetches it (never a pool thread) and re-queues the
//!   demanders; a repository fetched before the attempt began is never
//!   fetched again (the node is reported unresolved);
//! - `BackEdge(child)`: parked for the cycle handler.
//!
//! Coordinator-only nodes ([`NodeKey::coordinator_only`]) run on the
//! thread that called [`Evaluator::evaluate`]. At quiescence (no job
//! outstanding, no coordinator or repository work, roots not done) the
//! coordinator hands the wait graph to the [`CycleHandler`]; its back-edge
//! answers re-run the parents (`need` of the child answers `BackEdge`).
//!
//! Without `--keep_going`, the first value [`Bodies::stops_build`] calls
//! fatal switches the evaluator to **frontier-only** mode: queued nodes
//! nobody wants are set aside (`Deferred`) instead of run; the linearizer
//! wants nodes by [`Evaluator::boost`] (their dependencies inherit it).
//! The **boost heap** holds the linearizer's frontier: every job takes the
//! lowest boosted node before its own.
//!
//! Test hook: `BAZEL_RS_EVAL_JITTER=<µs>` sleeps each pool attempt a
//! reproducible pseudo-random time below the bound (shuffles completion
//! order). Debug builds assert that no evaluator lock is held where a
//! body or a fetch starts (crate's `graph::assert_no_locks`).
//!
//! Owner: worker A.

use std::cell::RefCell;
use std::cmp::Reverse;
use std::collections::{BinaryHeap, VecDeque};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicUsize, Ordering};

use rayon_core::Scope;

use crate::configuration::ConfigId;
use crate::fast_hash::{FxHashMap, FxHashSet};

use super::cx::{Cx, Missing, NodeFn, Vals};
use super::cycles::{CycleHandler, Resolution, WaitGraph};
use super::graph::{Graph, State, Tracked, Wait, assert_no_locks};
use super::key::{NodeId, NodeKey};
use super::shared::EvalShared;
use super::transcript::{Event, Transcript};
use super::value::NodeValue;

/// The node bodies and the per-attempt hook.
pub(crate) trait Bodies: Sync {
    /// What brackets one attempt (the caller's Isolation and
    /// `observed_fs` frame); created and consumed on the attempt's thread.
    type Attempt;

    /// An attempt of `id` starts (before the body).
    fn begin(&self, id: NodeId, key: &NodeKey) -> Self::Attempt;

    /// The body.
    fn run(&self, cx: &mut dyn Cx, key: &NodeKey) -> Result<NodeValue, Missing>;

    /// The attempt produced `value`: commit the frame; the value to
    /// publish.
    fn finish(&self, attempt: Self::Attempt, value: NodeValue) -> NodeValue {
        drop(attempt);
        value
    }

    /// The attempt returned `Missing`: drop what it did.
    fn abandon(&self, attempt: Self::Attempt) {
        drop(attempt);
    }

    /// The node a `Missing::TargetPlatform(config)` waits for.
    fn target_platform(&self, config: ConfigId) -> NodeKey;

    /// Whether `value` fails the build without `--keep_going` (switches
    /// to frontier-only mode). Default: a failed CT1, CT2 or aspect.
    fn stops_build(&self, key: &NodeKey, value: &NodeValue) -> bool {
        matches!(key, NodeKey::Ct1(_) | NodeKey::Ct2(_) | NodeKey::Aspect(_))
            && value.error().is_some()
    }

    /// The inline driver hands work to the pool (`true`) or takes it back
    /// ([`Evaluator::set_assist`]): the bodies switch to their pool mode.
    fn pool_mode(&self, _on: bool) {}

    /// The coordinator thread has nothing to do while the pool runs
    /// (called between waits, no evaluator lock held).
    fn idle(&self) {}
}

/// [`Bodies`] over a [`NodeFn`] and the shared read view (no per-attempt
/// state; B wraps it when the attempt needs a frame).
pub(crate) struct FnBodies<'s> {
    pub(crate) shared: &'s EvalShared,
    pub(crate) body: NodeFn,
    pub(crate) target_platform: fn(&EvalShared, ConfigId) -> NodeKey,
}

impl Bodies for FnBodies<'_> {
    type Attempt = ();

    fn begin(&self, _id: NodeId, _key: &NodeKey) {}

    fn run(&self, cx: &mut dyn Cx, key: &NodeKey) -> Result<NodeValue, Missing> {
        (self.body)(self.shared, cx, key)
    }

    fn target_platform(&self, config: ConfigId) -> NodeKey {
        (self.target_platform)(self.shared, config)
    }
}

/// The coordinator's repository lane (S4 fills it: repository
/// speculation, fetch stderr at the first demander's position). Called on
/// the coordinator only, one repository at a time, in demand order.
pub(crate) trait RepoLane {
    /// Make `name` available (fetched or failed: the re-run body sees
    /// which).
    fn fetch(&mut self, name: &str);
}

/// A lane for evaluations that never demand a repository.
pub(crate) struct NoRepositories;

impl RepoLane for NoRepositories {
    fn fetch(&mut self, name: &str) {
        panic!("internal error: repository @{name} demanded without a repository lane");
    }
}

/// The analysis pool's worker stack (as the loading workers').
pub(crate) const WORKER_STACK: usize = 16 << 20;

/// The pool size: `min(cores, --loading_phase_threads, memory cap / 64 MB)`
/// (at least 1; 1 is the inline driver).
pub(crate) fn worker_threads(
    cores: usize,
    loading_phase_threads: usize,
    cap_mb: Option<u64>,
) -> usize {
    let by_memory = cap_mb.map_or(usize::MAX, |cap| {
        (cap / crate::memory_limit::WORKER_HEAP_MB).max(1) as usize
    });
    cores.min(loading_phase_threads).min(by_memory).max(1)
}

/// `BAZEL_RS_EVAL_JITTER` (hidden test hook): the bound in microseconds of
/// each pool attempt's reproducible sleep (0: off).
pub(crate) fn jitter_from_env() -> u64 {
    // snapshot-unrecorded: a test hook shuffling the completion order; the output is the same either way.
    std::env::var("BAZEL_RS_EVAL_JITTER")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(0)
}

/// How an evaluation runs.
#[derive(Clone, Debug)]
pub(crate) struct EvalConfig {
    /// 1: the inline driver; more: a pool of this many workers.
    pub(crate) threads: usize,
    pub(crate) keep_going: bool,
    /// The jitter bound (µs; pool driver only).
    pub(crate) jitter_micros: u64,
    /// Pool driver: `need` of a node nobody runs yet claims and runs it in
    /// place, depth first (as the inline driver), instead of parking (S2:
    /// the phase-1 bodies restart only when a dependency runs on another
    /// worker; [`Cx::hint`] spreads the work).
    pub(crate) help_first: bool,
    /// The pool's worker stack (bytes).
    pub(crate) stack: usize,
}

impl Default for EvalConfig {
    fn default() -> Self {
        Self {
            threads: 1,
            keep_going: false,
            jitter_micros: 0,
            help_first: false,
            stack: WORKER_STACK,
        }
    }
}

impl EvalConfig {
    /// The configuration of a build: the pool size from the cores, the
    /// options and the memory cap; the jitter from the environment.
    pub(crate) fn for_build(
        loading_phase_threads: usize,
        keep_going: bool,
        cap_mb: Option<u64>,
    ) -> Self {
        let cores = std::thread::available_parallelism().map_or(1, usize::from);
        Self {
            threads: worker_threads(cores, loading_phase_threads, cap_mb),
            keep_going,
            jitter_micros: jitter_from_env(),
            ..Self::default()
        }
    }
}

/// The analysis pool: 16 MiB stacks, workers allocating in the calling
/// thread's jemalloc arena (what they make is kept and freed by the
/// coordinator, src/alloc_arena.rs).
pub(crate) fn build_pool(threads: usize, stack: usize) -> rayon_core::ThreadPool {
    let arena = crate::alloc_arena::current();
    rayon_core::ThreadPoolBuilder::new()
        .num_threads(threads)
        .stack_size(stack)
        .thread_name(|index| format!("eval-{index}"))
        .start_handler(move |_| {
            if let Some(arena) = arena {
                crate::alloc_arena::bind(arena);
            }
        })
        .build()
        .expect("internal error: the analysis pool did not start")
}

/// How an evaluation ended.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct Outcome {
    /// Every root is done.
    pub(crate) complete: bool,
    /// Parked nodes when it stopped (by id).
    pub(crate) unresolved: Vec<NodeId>,
    /// Nodes set aside in frontier-only mode (by id).
    pub(crate) deferred: Vec<NodeId>,
    /// The first value that stopped the build (frontier-only mode).
    pub(crate) first_error: Option<NodeId>,
}

/// Counters (cumulative over evaluations; `runs`, `done` and `parks` are
/// summed over the entries).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct Stats {
    /// Attempts started.
    pub(crate) runs: u64,
    /// Nodes done.
    pub(crate) done: u64,
    /// Parks (attempts that returned `Missing`).
    pub(crate) parks: u64,
    /// Repository fetches.
    pub(crate) fetches: u64,
    /// Back edges answered (on the active chain or by the handler).
    pub(crate) back_edges: u64,
    /// Quiescences handed to the cycle handler.
    pub(crate) quiescences: u64,
}

/// The rare events' counters (attempts and completions are counted per
/// entry: no shared cache line on the hot path).
#[derive(Default)]
struct Counters {
    fetches: AtomicU64,
    back_edges: AtomicU64,
    quiescences: AtomicU64,
}

/// The coordinator's queues.
#[derive(Default)]
struct Coord {
    /// Coordinator-only nodes ready to run.
    nodes: VecDeque<NodeId>,
    /// Repositories to fetch, in demand order.
    repos: VecDeque<String>,
    /// Nodes to schedule from outside a job (boosts).
    starts: Vec<NodeId>,
}

/// Repository demand state.
#[derive(Default)]
struct Repos {
    /// Parked demanders per repository, with their epoch.
    parked: FxHashMap<String, Vec<(NodeId, u32)>>,
    /// The fetch sequence number of each fetched repository.
    fetched: FxHashMap<String, u64>,
}

/// A value alone in its cache line (no false sharing with the read-mostly
/// flags beside it).
#[repr(align(128))]
struct Padded<T>(T);

impl<T> std::ops::Deref for Padded<T> {
    type Target = T;
    fn deref(&self) -> &T {
        &self.0
    }
}

/// No error yet (`first_error`).
const NO_ERROR: u32 = u32::MAX;

/// The evaluator: the graph, the bodies, and one evaluation's machinery.
pub(crate) struct Evaluator<'b, B: Bodies> {
    graph: Graph,
    bodies: &'b B,
    config: EvalConfig,
    pool: Option<rayon_core::ThreadPool>,
    frontier: AtomicBool,
    first_error: AtomicU32,
    boost: Tracked<BinaryHeap<Reverse<(u64, NodeId)>>>,
    boosted: AtomicUsize,
    coord: Tracked<Coord>,
    coord_wake: std::sync::Condvar,
    /// Jobs and coordinator nodes scheduled and not finished (written by
    /// every job: a cache line of its own).
    outstanding: Padded<AtomicUsize>,
    answered: Tracked<FxHashSet<(NodeId, NodeId)>>,
    any_answered: AtomicBool,
    repos: Tracked<Repos>,
    /// Fetches completed (an attempt records it when it begins).
    repo_seq: AtomicU64,
    counters: Counters,
    /// Evaluate with the inline driver even with a pool (S2: phase 1's
    /// sequential pass after the pool's).
    inline: AtomicBool,
    /// Nodes parked on a repository the lane fetched before their attempt
    /// began (`Wait::Unresolvable`), for [`Self::retry_unresolvable`].
    unresolvable: Tracked<Vec<NodeId>>,
    /// The pool's evaluation stops at quiescence without the cycle
    /// handler ([`Self::evaluate_round`]).
    round: AtomicBool,
    /// Pool waiters the inline driver's completions made ready (queued):
    /// scheduled when the pool evaluates next.
    woken: Tracked<Vec<NodeId>>,
    /// The inline driver of a pooled evaluation runs what it needs that
    /// the pool can run (fresh or queued, of an assisted kind) on the pool
    /// first ([`Self::set_assist`]).
    assist: AtomicBool,
    /// The pool serves while the inline driver runs ([`Self::serve`]):
    /// both claim attempts by CAS, the inline driver waits for what the
    /// pool runs.
    serving: AtomicBool,
    /// [`Self::serve`] stops once the pool is idle.
    stop_serving: AtomicBool,
    /// Repositories the serving pool's attempts demand: fetched by the
    /// inline driver's thread ([`Self::lane_work`]), the coordinator of
    /// repository demand.
    lane_queue: Tracked<VecDeque<String>>,
}

thread_local! {
    /// The nodes the inline driver runs on this thread (serving: a back
    /// edge is one of them, not any running node).
    static INLINE_STACK: RefCell<Vec<NodeId>> = const { RefCell::new(Vec::new()) };
}

impl<'b, B: Bodies> Evaluator<'b, B> {
    pub(crate) fn new(bodies: &'b B, config: EvalConfig) -> Self {
        let pool = (config.threads > 1).then(|| build_pool(config.threads, config.stack));
        Self {
            graph: Graph::default(),
            bodies,
            config,
            pool,
            frontier: AtomicBool::new(false),
            first_error: AtomicU32::new(NO_ERROR),
            boost: Tracked::default(),
            boosted: AtomicUsize::new(0),
            coord: Tracked::default(),
            coord_wake: std::sync::Condvar::new(),
            outstanding: Padded(AtomicUsize::new(0)),
            answered: Tracked::default(),
            any_answered: AtomicBool::new(false),
            repos: Tracked::default(),
            repo_seq: AtomicU64::new(0),
            counters: Counters::default(),
            inline: AtomicBool::new(false),
            unresolvable: Tracked::default(),
            round: AtomicBool::new(false),
            woken: Tracked::default(),
            assist: AtomicBool::new(false),
            serving: AtomicBool::new(false),
            stop_serving: AtomicBool::new(false),
            lane_queue: Tracked::default(),
        }
    }

    /// The pool serves from now on (call before [`Self::serve`] runs, so
    /// that nothing scheduled meanwhile is dropped): what the inline
    /// driver wakes or the pool spawns runs at once, beside it.
    pub(crate) fn begin_serving(&self) {
        if self.pool.is_some() {
            self.stop_serving.store(false, Ordering::SeqCst);
            self.serving.store(true, Ordering::SeqCst);
            // What inline completions woke before.
            let woken = std::mem::take(&mut *self.woken.lock());
            let mut coord = self.coord.lock();
            coord.starts.extend(woken);
            self.coord_wake.notify_one();
        }
    }

    /// Serving: start `keys` on the pool (those nobody started yet).
    pub(crate) fn start(&self, keys: &[NodeKey]) {
        for key in keys {
            let id = self.graph.intern(key);
            if self.graph.entry(id).transition(State::Fresh, State::Queued) {
                self.schedule(None, id);
            }
        }
    }

    /// Serving: start `keys` on the pool ahead of everything else.
    pub(crate) fn start_first(&self, keys: &[NodeKey]) {
        for key in keys {
            self.boost(key, 0);
        }
    }

    /// Whether the pool serves now.
    pub(crate) fn serving(&self) -> bool {
        self.serving.load(Ordering::SeqCst)
    }

    /// Whether [`Self::end_serving`] was called.
    pub(crate) fn serving_ends(&self) -> bool {
        self.stop_serving.load(Ordering::SeqCst)
    }

    /// Run the pool's coordinator until [`Self::end_serving`] and the
    /// pool is idle (on a thread of its own: the inline driver runs on
    /// the caller's). Repository demands go to [`Self::lane_work`].
    pub(crate) fn serve(&self) {
        let Some(pool) = &self.pool else {
            return;
        };
        pool.in_place_scope(|scope| self.coordinate_serving(scope));
    }

    /// Let [`Self::serve`] return once the pool is idle.
    pub(crate) fn end_serving(&self) {
        self.stop_serving.store(true, Ordering::SeqCst);
        let _coord = self.coord.lock();
        self.coord_wake.notify_all();
    }

    /// BAZEL_RS_EVAL_TIMELINE: the queued, running and waiting nodes now
    /// (an O(nodes) scan, concurrent with the pool).
    pub(crate) fn state_counts(&self) -> [usize; 3] {
        let mut counts = [0usize; 3];
        for id in self.graph.ids() {
            let Some(entry) = self.graph.entry_if(id) else {
                continue;
            };
            match entry.state() {
                State::Queued => counts[0] += 1,
                State::Running => counts[1] += 1,
                State::Waiting => counts[2] += 1,
                _ => {}
            }
        }
        counts
    }

    /// BAZEL_RS_EVAL_WATCHDOG: the serving state.
    pub(crate) fn watchdog_report(&self) -> String {
        let mut counts = [0usize; 6];
        let mut running = Vec::new();
        for id in self.graph.ids() {
            let Some(entry) = self.graph.entry_if(id) else {
                continue;
            };
            let state = entry.state();
            counts[state as usize] += 1;
            if state == State::Running && running.len() < 30 {
                running.push(
                    format!("{:?}", self.graph.key(id))
                        .chars()
                        .take(150)
                        .collect::<String>(),
                );
            }
        }
        if counts[State::Running as usize] < 40 {
            eprintln!(
                "bazel-rs: watchdog: running {running:#?}; blocked: {}",
                super::timeline::blocked_report()
            );
        }
        let coord = self.coord.lock();
        format!(
            "states {counts:?} outstanding {} coord nodes {} starts {} repos {} lane {}",
            self.outstanding.load(Ordering::Acquire),
            coord.nodes.len(),
            coord.starts.len(),
            coord.repos.len(),
            self.lane_queue.lock().len()
        )
    }

    /// After [`Self::serve`] returned.
    pub(crate) fn serving_done(&self) {
        self.serving.store(false, Ordering::SeqCst);
    }

    /// The repositories the serving pool demanded, fetched here (the
    /// inline driver's thread) and their demanders re-queued.
    pub(crate) fn lane_work(&self, lane: &mut dyn RepoLane) {
        loop {
            let Some(name) = self.lane_queue.lock().pop_front() else {
                return;
            };
            assert_no_locks("a repository fetch");
            let started = std::time::Instant::now();
            lane.fetch(&name);
            if started.elapsed() >= std::time::Duration::from_millis(20) {
                super::timeline::mark(|| {
                    format!(
                        "a slow lane fetch ({} ms): {}",
                        started.elapsed().as_millis(),
                        name.escape_debug()
                    )
                });
            }
            self.counters.fetches.fetch_add(1, Ordering::Relaxed);
            self.fetched(name);
        }
    }

    /// `name` was fetched: record it and re-queue its parked demanders.
    fn fetched(&self, name: String) {
        let demanders = {
            let mut repos = self.repos.lock();
            let seq = self.repo_seq.fetch_add(1, Ordering::AcqRel) + 1;
            let demanders = repos.parked.remove(&name).unwrap_or_default();
            repos.fetched.insert(name, seq);
            demanders
        };
        for (id, epoch) in demanders {
            let entry = self.graph.entry(id);
            if entry.count_down(epoch, 1) && entry.transition(State::Waiting, State::Queued) {
                self.schedule(None, id);
            }
        }
    }

    /// The serving pool's coordinator: coordinator-only nodes, outside
    /// starts; repository demands are queued for [`Self::lane_work`].
    fn coordinate_serving<'s>(&'s self, scope: &Scope<'s>) {
        loop {
            let mut coord = self.coord.lock();
            while coord.nodes.is_empty() && coord.repos.is_empty() && coord.starts.is_empty() {
                if self.stop_serving.load(Ordering::SeqCst)
                    && self.outstanding.load(Ordering::Acquire) == 0
                {
                    return;
                }
                coord = coord
                    .wait_timeout(
                        &self.coord_wake,
                        std::time::Duration::from_millis(IDLE_MILLIS),
                    )
                    .0;
            }
            if let Some(id) = coord.nodes.pop_front() {
                drop(coord);
                if let Some(ready) = self.run_one(scope, id) {
                    self.schedule(Some(scope), ready);
                }
                self.outstanding.fetch_sub(1, Ordering::AcqRel);
                continue;
            }
            if !coord.starts.is_empty() {
                let starts = std::mem::take(&mut coord.starts);
                drop(coord);
                let queued = starts
                    .into_iter()
                    .filter(|&id| self.graph.entry(id).state() == State::Queued)
                    .collect();
                self.schedule_many(scope, queued);
                continue;
            }
            let repos = std::mem::take(&mut coord.repos);
            drop(coord);
            self.lane_queue.lock().extend(repos);
        }
    }

    /// Serving: claim `id` for the inline driver (by CAS: the pool may run
    /// it), waiting while the pool makes progress on it; `false` when it
    /// is done. A fresh node of a kind the pool runs well is handed to the
    /// pool first (its subtree spreads over the workers).
    fn claim_inline(&self, id: NodeId, lane: &RefCell<&mut dyn RepoLane>) -> bool {
        let entry = self.graph.entry(id);
        let key = self.graph.key(id);
        if matches!(
            key,
            NodeKey::Ct1(_)
                | NodeKey::Pkg(_)
                | NodeKey::Transition(_)
                | NodeKey::Platform(_)
                | NodeKey::RegToolchains(_)
                | NodeKey::RegExecPlatforms(_)
        ) && entry.transition(State::Fresh, State::Queued)
        {
            self.schedule(None, id);
        }
        let mut boosted: Option<std::time::Instant> = None;
        let waiting = std::time::Instant::now();
        loop {
            if waiting.elapsed() > std::time::Duration::from_secs(120) {
                panic!(
                    "internal error: BAZEL_RS_ANALYSIS=eval: the sequential pass waited 120 s for {key:?} ({:?}); rerun with BAZEL_RS_EVAL_NO_SERVE=1",
                    entry.state()
                );
            }
            let state = entry.state();
            let wait = match state {
                State::Done => return false,
                State::Running | State::Queued => true,
                State::Waiting => self.pool_progresses(id),
                State::Fresh | State::Deferred => false,
            };
            if wait {
                super::timeline::seq_state(|| format!("waits for {key:?} {state:?}"));
                if boosted.is_none_or(|at| at.elapsed() >= std::time::Duration::from_millis(1)) {
                    // The pool runs what the inline driver waits for first:
                    // the end of its chain of first unfinished dependencies.
                    boosted = Some(std::time::Instant::now());
                    if let Some(leaf) = self.queued_leaf(id) {
                        self.boost(self.graph.key(leaf), 0);
                    }
                }
                // Meanwhile, what only this thread does (repository
                // demands, the linearizer).
                self.lane_work(&mut **lane.borrow_mut());
                self.bodies.idle();
                std::thread::sleep(std::time::Duration::from_micros(20));
                continue;
            }
            // Waiter registrations of its parked attempt die.
            entry.epoch.fetch_add(1, Ordering::AcqRel);
            if entry.transition(state, State::Running) {
                return true;
            }
        }
    }

    /// Serving: park the inline driver's `id` on `deps` as a pool attempt
    /// parks (their waiter), so that the pool runs it once they are done.
    fn park_inline_on_nodes(&self, id: NodeId, deps: Vec<NodeId>) {
        let entry = self.graph.entry(id);
        let count = deps.len() as u32;
        let epoch = self.park_as(id, Wait::Nodes(deps.clone()), count + 1);
        let mut satisfied = 0;
        for dep in deps {
            if !self.graph.entry(dep).add_waiter(id, epoch) {
                satisfied += 1;
            }
        }
        if entry.count_down(epoch, satisfied + 1) && entry.transition(State::Waiting, State::Queued)
        {
            self.schedule(None, id);
        }
    }

    /// Serving: the queued node at the end of `id`'s chain of first
    /// unfinished dependencies (what to boost), if any.
    fn queued_leaf(&self, id: NodeId) -> Option<NodeId> {
        let mut at = id;
        for _ in 0..64 {
            let entry = self.graph.entry(at);
            match entry.state() {
                State::Queued => return Some(at),
                State::Waiting => {
                    let next = match &entry.inner.lock().wait {
                        Wait::Nodes(deps) => deps
                            .iter()
                            .copied()
                            .find(|dep| self.graph.entry(*dep).state() != State::Done),
                        _ => None,
                    };
                    at = next?;
                }
                _ => return None,
            }
        }
        None
    }

    /// Serving: whether the pool makes progress on the parked `id` (the
    /// chain of its first unfinished dependencies reaches a node the pool
    /// runs or will run), so that the inline driver waits instead of
    /// taking it over.
    fn pool_progresses(&self, id: NodeId) -> bool {
        let mut at = id;
        for _ in 0..64 {
            let entry = self.graph.entry(at);
            match entry.state() {
                State::Done => return true,
                State::Queued => return true,
                State::Running => {
                    return !INLINE_STACK.with(|stack| stack.borrow().contains(&at));
                }
                State::Fresh | State::Deferred => return false,
                State::Waiting => {
                    let next = match &entry.inner.lock().wait {
                        Wait::Nodes(deps) => deps
                            .iter()
                            .copied()
                            .find(|dep| self.graph.entry(*dep).state() != State::Done),
                        _ => return false,
                    };
                    match next {
                        Some(next) => at = next,
                        // Its last dependency finished: it is re-queued.
                        None => return true,
                    }
                }
            }
        }
        false
    }

    /// From now on the inline driver (with a pool) evaluates a node it
    /// needs that nobody runs on the pool first, as a round of its own
    /// (phase 1's sequential pass: what only it can run stays its own).
    pub(crate) fn set_assist(&self, assist: bool) {
        self.assist
            .store(assist && self.pool.is_some(), Ordering::SeqCst);
    }

    /// An inline completion woke pool waiters: once enough are woken,
    /// a pool round runs them (and what they reach) before the inline
    /// driver continues (what the pool cannot run parks and stays the
    /// inline driver's).
    fn assist(&self, lane: &RefCell<&mut dyn RepoLane>) {
        if !self.assist.load(Ordering::Relaxed) || self.woken.lock().len() < *ASSIST_WOKEN {
            return;
        }
        super::timeline::mark(|| format!("assist round: {} woken", self.woken.lock().len()));
        self.bodies.pool_mode(true);
        self.set_inline(false);
        {
            let mut lane = lane.borrow_mut();
            self.evaluate_round(&[], &mut **lane);
        }
        self.set_inline(true);
        self.bodies.pool_mode(false);
        super::timeline::mark(|| "assist round done".to_owned());
    }

    /// Re-queue the nodes parked unresolvable (S2: what blocked them was
    /// done by the sequential pass since); they run at the next
    /// evaluation's start.
    pub(crate) fn retry_unresolvable(&self) {
        let parked = std::mem::take(&mut *self.unresolvable.lock());
        for id in parked {
            let entry = self.graph.entry(id);
            if entry.state() != State::Waiting {
                continue;
            }
            entry.epoch.fetch_add(1, Ordering::AcqRel);
            entry.inner.lock().wait = Wait::None;
            if entry.transition(State::Waiting, State::Queued) {
                self.schedule(None, id);
            }
        }
    }

    /// [`Self::evaluate`] on the pool until quiescence (no cycle handler,
    /// no wait graph: what is parked stays parked).
    pub(crate) fn evaluate_round(&self, roots: &[NodeKey], lane: &mut dyn RepoLane) -> Outcome {
        self.round.store(true, Ordering::SeqCst);
        let outcome = self.evaluate(roots, lane, &mut super::cycles::DfsCycles::default());
        self.round.store(false, Ordering::SeqCst);
        outcome
    }

    /// From now on, [`Self::evaluate`] uses the inline driver (`true`) or
    /// the pool, if there is one (`false`).
    pub(crate) fn set_inline(&self, inline: bool) {
        self.inline.store(inline, Ordering::SeqCst);
    }

    /// Whether evaluations run on the pool.
    pub(crate) fn pooled(&self) -> bool {
        self.pool.is_some() && !self.inline.load(Ordering::SeqCst)
    }

    pub(crate) fn graph(&self) -> &Graph {
        &self.graph
    }

    pub(crate) fn config(&self) -> &EvalConfig {
        &self.config
    }

    /// The value of `key` once done.
    pub(crate) fn value(&self, key: &NodeKey) -> Option<&Arc<NodeValue>> {
        self.graph.get(key).and_then(|id| self.graph.value(id))
    }

    /// Whether frontier-only mode is on.
    pub(crate) fn frontier_only(&self) -> bool {
        self.frontier.load(Ordering::Acquire)
    }

    /// The counters (an O(nodes) scan: between evaluations).
    pub(crate) fn stats(&self) -> Stats {
        let c = &self.counters;
        let (mut runs, mut done) = (0, 0);
        for id in self.graph.ids() {
            let entry = self.graph.entry(id);
            runs += u64::from(entry.attempts.load(Ordering::Relaxed));
            done += u64::from(entry.state() == State::Done);
        }
        Stats {
            runs,
            done,
            // Every attempt either completes or parks.
            parks: runs - done,
            fetches: c.fetches.load(Ordering::Relaxed),
            back_edges: c.back_edges.load(Ordering::Relaxed),
            quiescences: c.quiescences.load(Ordering::Relaxed),
        }
    }

    /// Favour `key` (the linearizer's frontier; lower `priority` first):
    /// it and what it waits for are wanted in frontier-only mode, and
    /// every pool job runs the lowest boosted queued node before its own.
    /// Callable from any thread; outside a job, scheduling happens at the
    /// coordinator's next wake (or when `evaluate` starts).
    pub(crate) fn boost(&self, key: &NodeKey, priority: u64) {
        let id = self.graph.intern(key);
        self.want(None, id);
        let entry = self.graph.entry(id);
        if entry.transition(State::Fresh, State::Queued) {
            self.schedule(None, id);
        }
        if self.pool.is_some() {
            self.boost.lock().push(Reverse((priority, id)));
            self.boosted.fetch_add(1, Ordering::AcqRel);
        }
    }

    /// Evaluate `roots` (and what they need) until they are done or the
    /// evaluation stops (a quiescence the handler does not resolve, or
    /// frontier-only mode with nothing wanted left).
    pub(crate) fn evaluate(
        &self,
        roots: &[NodeKey],
        lane: &mut dyn RepoLane,
        cycles: &mut dyn CycleHandler,
    ) -> Outcome {
        let roots: Vec<NodeId> = roots.iter().map(|key| self.graph.intern(key)).collect();
        match &self.pool {
            Some(pool) if !self.inline.load(Ordering::SeqCst) => {
                pool.in_place_scope(|scope| self.coordinate(scope, &roots, lane, cycles))
            }
            _ => self.evaluate_inline(&roots, lane, cycles),
        }
    }

    /// [`Self::evaluate`] for the roots' values only: the inline driver
    /// skips the outcome's scan of every node (S2: the sequential pass
    /// evaluates each requested target's lookups and node this way).
    pub(crate) fn evaluate_values(
        &self,
        roots: &[NodeKey],
        lane: &mut dyn RepoLane,
        cycles: &mut dyn CycleHandler,
    ) {
        if self.pooled() {
            self.evaluate(roots, lane, cycles);
            return;
        }
        let roots: Vec<NodeId> = roots.iter().map(|key| self.graph.intern(key)).collect();
        self.run_inline_roots(&roots, lane, cycles);
    }

    // ---- shared by both drivers -------------------------------------

    fn answered(&self, parent: NodeId, child: NodeId) -> bool {
        self.any_answered.load(Ordering::Acquire) && self.answered.lock().contains(&(parent, child))
    }

    /// Record an answered back edge; whether it is new.
    fn answer(&self, parent: NodeId, child: NodeId) -> bool {
        let new = self.answered.lock().insert((parent, child));
        if new {
            self.any_answered.store(true, Ordering::Release);
            self.counters.back_edges.fetch_add(1, Ordering::Relaxed);
        }
        new
    }

    fn jitter(&self, id: NodeId, attempt: u32) {
        if self.config.jitter_micros == 0 {
            return;
        }
        use std::hash::BuildHasher;
        let hash = crate::fast_hash::FxBuildHasher::default().hash_one(self.graph.key(id));
        let roll = crate::engine::loading_jitter_roll(hash, u64::from(attempt));
        let micros = roll % (self.config.jitter_micros + 1);
        std::thread::sleep(std::time::Duration::from_micros(micros));
    }

    /// Publish `value`: the waiters whose last dependency this was.
    fn publish(&self, id: NodeId, value: NodeValue, events: Vec<Event>) -> Vec<NodeId> {
        let key = self.graph.key(id);
        let stops = !self.config.keep_going && self.bodies.stops_build(key, &value);
        let entry = self.graph.entry(id);
        let waiters = entry.complete(Arc::new(value), Transcript { events });
        if stops {
            let _ = self.first_error.compare_exchange(
                NO_ERROR,
                id.0,
                Ordering::AcqRel,
                Ordering::Acquire,
            );
            self.frontier.store(true, Ordering::SeqCst);
        }
        let mut ready = Vec::new();
        for (waiter, epoch) in waiters {
            let waiting = self.graph.entry(waiter);
            if waiting.count_down(epoch, 1) {
                // Serving: the inline driver may have claimed it meanwhile.
                if waiting.transition(State::Waiting, State::Queued) {
                    ready.push(waiter);
                } else {
                    debug_assert!(
                        self.serving.load(Ordering::SeqCst),
                        "internal error: a ready waiter was not waiting"
                    );
                }
            }
        }
        ready
    }

    /// Park `id` on `wait` with `missing` outstanding (+1 guard); its
    /// epoch.
    fn park_as(&self, id: NodeId, wait: Wait, missing: u32) -> u32 {
        let entry = self.graph.entry(id);
        let epoch = entry.epoch.fetch_add(1, Ordering::AcqRel) + 1;
        entry.missing.store(missing, Ordering::Release);
        entry.inner.lock().wait = wait;
        let from = entry.state();
        let parked = entry.transition(from, State::Waiting);
        debug_assert!(parked && from == State::Running);
        epoch
    }

    /// Mark `id` and what it waits for wanted; un-defer them.
    fn want<'s>(&'s self, scope: Option<&Scope<'s>>, id: NodeId) {
        let mut stack = vec![id];
        while let Some(id) = stack.pop() {
            let entry = self.graph.entry(id);
            if entry.wanted.swap(true, Ordering::SeqCst) {
                continue;
            }
            if entry.transition(State::Deferred, State::Queued) {
                self.schedule(scope, id);
            }
            if entry.state() == State::Waiting {
                stack.extend_from_slice(entry.inner.lock().wait.nodes());
            }
        }
    }

    fn deduplicated(ids: Vec<NodeId>) -> Vec<NodeId> {
        if ids.len() < 2 {
            return ids;
        }
        let mut seen = FxHashSet::default();
        ids.into_iter().filter(|id| seen.insert(*id)).collect()
    }

    fn wait_graph(&self, roots: &[NodeId]) -> WaitGraph<'_> {
        let mut parked = FxHashMap::default();
        let mut deferred = Vec::new();
        for id in self.graph.ids() {
            let entry = self.graph.entry(id);
            match entry.state() {
                State::Waiting => {
                    parked.insert(id, entry.inner.lock().wait.clone());
                }
                State::Deferred => deferred.push(id),
                _ => {}
            }
        }
        WaitGraph {
            graph: &self.graph,
            roots: roots.to_vec(),
            parked,
            deferred,
            answered: self.answered.lock().clone(),
        }
    }

    fn outcome(&self, roots: &[NodeId]) -> Outcome {
        let mut outcome = Outcome {
            complete: roots.iter().all(|&id| self.graph.value(id).is_some()),
            first_error: match self.first_error.load(Ordering::Acquire) {
                NO_ERROR => None,
                id => Some(NodeId(id)),
            },
            ..Outcome::default()
        };
        for id in self.graph.ids() {
            match self.graph.entry(id).state() {
                State::Waiting => outcome.unresolved.push(id),
                State::Deferred => outcome.deferred.push(id),
                _ => {}
            }
        }
        outcome
    }

    /// The new pairs of a handler's answer (`None`: stop).
    fn new_answers(&self, resolution: Resolution) -> Option<Vec<(NodeId, NodeId)>> {
        let Resolution::Answer(pairs) = resolution else {
            return None;
        };
        let fresh: Vec<_> = pairs
            .into_iter()
            .filter(|&(parent, child)| self.answer(parent, child))
            .collect();
        (!fresh.is_empty()).then_some(fresh)
    }

    // ---- the inline driver ------------------------------------------

    fn evaluate_inline(
        &self,
        roots: &[NodeId],
        lane: &mut dyn RepoLane,
        cycles: &mut dyn CycleHandler,
    ) -> Outcome {
        self.run_inline_roots(roots, lane, cycles);
        self.outcome(roots)
    }

    /// The inline driver over `roots` (without the outcome's O(nodes)
    /// scan).
    fn run_inline_roots(
        &self,
        roots: &[NodeId],
        lane: &mut dyn RepoLane,
        cycles: &mut dyn CycleHandler,
    ) {
        let lane = RefCell::new(lane);
        loop {
            for &root in roots {
                if self.graph.entry(root).state() != State::Done {
                    self.run_inline(root, &lane);
                }
            }
            if roots.iter().all(|&id| self.graph.value(id).is_some()) {
                break;
            }
            self.counters.quiescences.fetch_add(1, Ordering::Relaxed);
            let resolution = cycles.at_quiescence(&self.wait_graph(roots));
            if self.new_answers(resolution).is_none() {
                break;
            }
        }
    }

    /// Run `id` to completion in place (re-running it after each
    /// `Missing` that can be resolved here); it is done or parked after.
    fn run_inline(&self, id: NodeId, lane: &RefCell<&mut dyn RepoLane>) {
        let entry = self.graph.entry(id);
        let key = self.graph.key(id);
        // BAZEL_RS_EVAL_TIMELINE: why the sequential pass of a pooled
        // evaluation runs this attempt.
        let timed = self.pool.is_some() && super::timeline::on();
        let mut rerun = false;
        loop {
            let cause = timed.then(|| {
                let (cause, detail) = self.inline_cause(id, rerun);
                let what = format!("{key:?}").chars().take(200).collect();
                super::timeline::seq_enter(cause, || detail, what);
                cause
            });
            rerun = true;
            let _timed = TimedAttempt(cause, key.kind());
            if self.serving.load(Ordering::SeqCst) {
                if !self.claim_inline(id, lane) {
                    return;
                }
            } else {
                // A node parked by the pool: its waiter registrations die.
                entry.epoch.fetch_add(1, Ordering::AcqRel);
                entry.set_state(State::Running);
            }
            INLINE_STACK.with(|stack| stack.borrow_mut().push(id));
            let _inline = InlineFrame;
            if self.serving.load(Ordering::Relaxed) {
                // What the serving pool demanded (this thread fetches).
                self.lane_work(&mut **lane.borrow_mut());
            }
            entry.attempts.fetch_add(1, Ordering::Relaxed);
            let attempt_seq = self.repo_seq.load(Ordering::Acquire);
            assert_no_locks("a node body");
            super::timeline::seq_state(|| format!("begins {key:?}"));
            let attempt = self.bodies.begin(id, key);
            super::timeline::seq_state(|| format!("runs {key:?}"));
            let mut cx = InlineCx {
                evaluator: self,
                lane,
                id,
                events: Vec::new(),
            };
            let result = self.bodies.run(&mut cx, key);
            let events = cx.events;
            assert_no_locks("a node body's return");
            let missing = match result {
                Ok(value) => {
                    let value = self.bodies.finish(attempt, value);
                    // Waiters are the pool's (parked before the inline
                    // driver took over): they run at its next evaluation.
                    let ready = self.publish(id, value, events);
                    if self.serving.load(Ordering::SeqCst) {
                        for ready in ready {
                            self.schedule(None, ready);
                        }
                    } else if !ready.is_empty() {
                        self.woken.lock().extend(ready);
                        self.assist(lane);
                    }
                    return;
                }
                Err(missing) => {
                    self.bodies.abandon(attempt);
                    missing
                }
            };
            match missing {
                Missing::Nodes(_) | Missing::TargetPlatform(_) => {
                    let deps = match missing {
                        Missing::Nodes(deps) => Self::deduplicated(deps),
                        Missing::TargetPlatform(config) => {
                            vec![self.graph.intern(&self.bodies.target_platform(config))]
                        }
                        _ => unreachable!(),
                    };
                    let serving = self.serving.load(Ordering::SeqCst);
                    for &dep in &deps {
                        let state = self.graph.entry(dep).state();
                        // Serving: what the pool runs or parked is waited
                        // for or taken over too.
                        let run = matches!(state, State::Fresh | State::Queued | State::Deferred)
                            || (serving
                                && state != State::Done
                                && !INLINE_STACK.with(|stack| stack.borrow().contains(&dep)));
                        if run {
                            self.run_inline(dep, lane);
                        }
                    }
                    if deps.iter().any(|&dep| self.graph.value(dep).is_none()) {
                        if serving {
                            // Registered as their waiter: the pool runs it
                            // once they are done.
                            self.park_inline_on_nodes(id, deps);
                        } else {
                            self.park_as(id, Wait::Nodes(deps), 1);
                        }
                        return;
                    }
                }
                Missing::Repo(name) => {
                    let fetched = self.repos.lock().fetched.get(&name).copied();
                    match fetched {
                        Some(seq) if seq <= attempt_seq => {
                            self.park_as(id, Wait::Unresolvable(name), 1);
                            return;
                        }
                        Some(_) => {}
                        None => {
                            assert_no_locks("a repository fetch");
                            let started = std::time::Instant::now();
                            lane.borrow_mut().fetch(&name);
                            if started.elapsed() >= std::time::Duration::from_millis(20) {
                                super::timeline::mark(|| {
                                    format!(
                                        "a slow inline fetch ({} ms): {}",
                                        started.elapsed().as_millis(),
                                        name.escape_debug()
                                    )
                                });
                            }
                            self.counters.fetches.fetch_add(1, Ordering::Relaxed);
                            // Pool attempts parked on it (serving) run again.
                            self.fetched(name);
                        }
                    }
                }
                Missing::BackEdge(child) => {
                    self.park_as(id, Wait::BackEdge(child), 1);
                    return;
                }
            }
        }
    }

    /// BAZEL_RS_EVAL_TIMELINE: why the inline driver runs `id` (its state
    /// before; for a node the pool left to it, the pool's reason).
    fn inline_cause(&self, id: NodeId, rerun: bool) -> (&'static str, String) {
        if rerun {
            return ("re-run after a fetch", String::new());
        }
        let entry = self.graph.entry(id);
        let key = || {
            format!("{:?}", self.graph.key(id))
                .chars()
                .take(200)
                .collect::<String>()
        };
        match entry.state() {
            State::Fresh => ("fresh", String::new()),
            State::Queued => ("queued (woken, not run yet)", key()),
            State::Deferred => ("deferred", key()),
            State::Waiting => match entry.inner.lock().wait.clone() {
                Wait::Repo(name) | Wait::Unresolvable(name)
                    if name == super::nodes::bodies::INLINE =>
                {
                    let reason = super::timeline::inline_reason(id.0).unwrap_or_default();
                    ("left by the pool", format!("{reason} [{}]", key()))
                }
                Wait::Repo(name) | Wait::Unresolvable(name) => (
                    "parked on a lane blocker",
                    format!("{} [{}]", name.escape_debug(), key()),
                ),
                Wait::Nodes(nodes) => {
                    // The chain of first unfinished dependencies, to what
                    // blocks it.
                    let mut chain = Vec::new();
                    let mut at = nodes
                        .iter()
                        .copied()
                        .find(|node| self.graph.entry(*node).state() != State::Done);
                    let mut end = String::from("done");
                    while let Some(node) = at.take() {
                        chain.push(self.graph.key(node).kind());
                        if chain.len() > 64 {
                            end = "a long chain".to_owned();
                            break;
                        }
                        let dependency = self.graph.entry(node);
                        let state = dependency.state();
                        if state != State::Waiting {
                            end = format!("{state:?}");
                            break;
                        }
                        let wait = dependency.inner.lock().wait.clone();
                        match wait {
                            Wait::Nodes(deps) => {
                                at = deps
                                    .into_iter()
                                    .find(|dep| self.graph.entry(*dep).state() != State::Done);
                                if at.is_none() {
                                    end = "ready".to_owned();
                                }
                            }
                            Wait::Repo(name) | Wait::Unresolvable(name)
                                if name == super::nodes::bodies::INLINE =>
                            {
                                end = format!(
                                    "left by the pool: {} [{}]",
                                    super::timeline::inline_reason(node.0).unwrap_or_default(),
                                    format!("{:?}", self.graph.key(node))
                                        .chars()
                                        .take(160)
                                        .collect::<String>()
                                );
                            }
                            other => {
                                end = format!("{other:?}").chars().take(200).collect();
                            }
                        }
                    }
                    (
                        "waiting on a node",
                        format!("{} -> {end}", chain.join(" -> ")),
                    )
                }
                Wait::BackEdge(_) => ("a back edge", key()),
                Wait::None => ("waiting (none)", key()),
            },
            State::Running | State::Done => ("running", key()),
        }
    }

    // ---- the pool driver --------------------------------------------

    /// Schedule a node just made `Queued`: a pool job, or the
    /// coordinator's queue for a coordinator-only node or outside a job.
    fn schedule<'s>(&'s self, scope: Option<&Scope<'s>>, id: NodeId) {
        if !self.pooled() && !self.serving.load(Ordering::Relaxed) {
            // The inline driver runs what it needs in place.
            return;
        }
        let coordinator = self.graph.key(id).coordinator_only();
        match scope {
            Some(scope) if !coordinator => {
                self.outstanding.fetch_add(1, Ordering::AcqRel);
                scope.spawn(move |scope| self.job(scope, id));
            }
            _ => {
                let mut coord = self.coord.lock();
                if coordinator {
                    self.outstanding.fetch_add(1, Ordering::AcqRel);
                    coord.nodes.push_back(id);
                } else {
                    coord.starts.push(id);
                }
                self.coord_wake.notify_one();
            }
        }
    }

    /// The lowest boosted node still queued.
    fn pop_boost(&self) -> Option<NodeId> {
        if self.boosted.load(Ordering::Acquire) == 0 {
            return None;
        }
        let mut heap = self.boost.lock();
        while let Some(Reverse((_, id))) = heap.pop() {
            self.boosted.fetch_sub(1, Ordering::AcqRel);
            if self.graph.entry(id).state() == State::Queued
                && !self.graph.key(id).coordinator_only()
            {
                return Some(id);
            }
        }
        None
    }

    /// Schedule many nodes just made `Queued`: pool nodes in chunks (one
    /// spawn per chunk, at least eight chunks per worker, at most 64
    /// nodes each), so that a wide batch does not cost a spawn and a
    /// wake-up per node on one thread.
    fn schedule_many<'s>(&'s self, scope: &Scope<'s>, ids: Vec<NodeId>) {
        if ids.len() < 2 {
            for id in ids {
                self.schedule(Some(scope), id);
            }
            return;
        }
        let (coordinator, pool): (Vec<NodeId>, Vec<NodeId>) = ids
            .into_iter()
            .partition(|&id| self.graph.key(id).coordinator_only());
        for id in coordinator {
            self.schedule(Some(scope), id);
        }
        let chunk = (pool.len() / (self.config.threads * 8)).clamp(1, 64);
        for ids in pool.chunks(chunk) {
            if let [id] = ids {
                self.schedule(Some(scope), *id);
                continue;
            }
            let mut ids = ids.to_vec();
            // Run in order (popped from the end).
            ids.reverse();
            self.outstanding.fetch_add(1, Ordering::AcqRel);
            scope.spawn(move |scope| {
                let _done = Outstanding(self);
                self.drive(scope, None, ids);
            });
        }
    }

    /// One pool job.
    fn job<'s>(&'s self, scope: &Scope<'s>, id: NodeId) {
        let _done = Outstanding(self);
        self.drive(scope, Some(id), Vec::new());
    }

    /// Run a job's nodes: boosted nodes first, then the ready node the
    /// last completion handed on, then its own nodes (`rest` from the
    /// end).
    fn drive<'s>(&'s self, scope: &Scope<'s>, mut own: Option<NodeId>, mut rest: Vec<NodeId>) {
        crate::debug_panic::at("evaluator");
        loop {
            let next = match self.pop_boost() {
                Some(boosted) => boosted,
                None => match own.take().or_else(|| rest.pop()) {
                    Some(id) => id,
                    None => break,
                },
            };
            if let Some(ready) = self.run_one(scope, next) {
                if own.is_none() {
                    own = Some(ready);
                } else {
                    self.schedule(Some(scope), ready);
                }
            }
        }
    }

    /// Claim and run one attempt of `id`; a ready node to continue with.
    fn run_one<'s>(&'s self, scope: &Scope<'s>, id: NodeId) -> Option<NodeId> {
        let entry = self.graph.entry(id);
        if !entry.transition(State::Queued, State::Running) {
            return None;
        }
        if self.frontier.load(Ordering::SeqCst) && !entry.wanted.load(Ordering::SeqCst) {
            entry.set_state(State::Deferred);
            // A concurrent `want` saw Running: re-check (both SeqCst).
            if entry.wanted.load(Ordering::SeqCst)
                && entry.transition(State::Deferred, State::Queued)
            {
                return Some(id);
            }
            return None;
        }
        let key = self.graph.key(id);
        let attempt_number = entry.attempts.fetch_add(1, Ordering::Relaxed);
        self.jitter(id, attempt_number);
        let attempt_seq = self.repo_seq.load(Ordering::Acquire);
        assert_no_locks("a node body");
        let timed = super::timeline::on();
        if timed {
            super::timeline::pool_enter();
        }
        let _timed = TimedPool(timed, key);
        let attempt = self.bodies.begin(id, key);
        let mut cx = PoolCx {
            evaluator: self,
            scope,
            id,
            events: Vec::new(),
        };
        let result = self.bodies.run(&mut cx, key);
        let events = cx.events;
        assert_no_locks("a node body's return");
        match result {
            Ok(value) => {
                let value = self.bodies.finish(attempt, value);
                let mut ready = self.publish(id, value, events);
                let first = ready
                    .iter()
                    .position(|&id| !self.graph.key(id).coordinator_only())
                    .map(|index| ready.remove(index));
                // Pushed in order, popped LIFO by this worker.
                self.schedule_many(scope, ready);
                first
            }
            Err(missing) => {
                self.bodies.abandon(attempt);
                self.park(scope, id, missing, attempt_seq)
            }
        }
    }

    /// Park after `Missing`; `Some(id)` when it can re-run at once.
    fn park<'s>(
        &'s self,
        scope: &Scope<'s>,
        id: NodeId,
        missing: Missing,
        attempt_seq: u64,
    ) -> Option<NodeId> {
        let entry = self.graph.entry(id);
        match missing {
            Missing::Nodes(deps) => self.park_on_nodes(scope, id, Self::deduplicated(deps)),
            Missing::TargetPlatform(config) => {
                let platform = self.graph.intern(&self.bodies.target_platform(config));
                self.park_on_nodes(scope, id, vec![platform])
            }
            Missing::BackEdge(child) => {
                self.park_as(id, Wait::BackEdge(child), 1);
                None
            }
            Missing::Repo(name) => {
                let epoch = self.park_as(id, Wait::Repo(name.clone()), 1);
                let mut repos = self.repos.lock();
                match repos.fetched.get(&name).copied() {
                    Some(seq) if seq <= attempt_seq => {
                        drop(repos);
                        entry.inner.lock().wait = Wait::Unresolvable(name);
                        self.unresolvable.lock().push(id);
                        None
                    }
                    Some(_) => {
                        drop(repos);
                        let ready = entry.count_down(epoch, 1)
                            && entry.transition(State::Waiting, State::Queued);
                        ready.then_some(id)
                    }
                    None => {
                        let demanders = repos.parked.entry(name.clone()).or_default();
                        let first = demanders.is_empty();
                        demanders.push((id, epoch));
                        drop(repos);
                        if first {
                            let mut coord = self.coord.lock();
                            coord.repos.push_back(name);
                            self.coord_wake.notify_one();
                        }
                        None
                    }
                }
            }
        }
    }

    fn park_on_nodes<'s>(
        &'s self,
        scope: &Scope<'s>,
        id: NodeId,
        deps: Vec<NodeId>,
    ) -> Option<NodeId> {
        let entry = self.graph.entry(id);
        let count = deps.len() as u32;
        let epoch = self.park_as(id, Wait::Nodes(deps.clone()), count + 1);
        let wanted = entry.wanted.load(Ordering::SeqCst);
        let mut satisfied = 0;
        let mut fresh = Vec::new();
        for &dep in &deps {
            if wanted {
                self.want(Some(scope), dep);
            }
            let dependency = self.graph.entry(dep);
            if dependency.transition(State::Fresh, State::Queued) {
                fresh.push(dep);
            }
            if !dependency.add_waiter(id, epoch) {
                satisfied += 1;
            }
        }
        self.schedule_many(scope, fresh);
        if entry.count_down(epoch, satisfied + 1) {
            // Serving: the inline driver may have claimed it meanwhile.
            let requeued = entry.transition(State::Waiting, State::Queued);
            debug_assert!(requeued || self.serving.load(Ordering::SeqCst));
            return requeued.then_some(id);
        }
        None
    }

    /// Re-queue a parked node by force (the cycle handler's answer or a
    /// new root); stale waiter registrations die with its epoch.
    fn requeue<'s>(&'s self, scope: &Scope<'s>, id: NodeId) {
        let entry = self.graph.entry(id);
        match entry.state() {
            State::Waiting => {
                entry.epoch.fetch_add(1, Ordering::AcqRel);
                entry.inner.lock().wait = Wait::None;
                if entry.transition(State::Waiting, State::Queued) {
                    self.schedule(Some(scope), id);
                }
            }
            State::Fresh => {
                if entry.transition(State::Fresh, State::Queued) {
                    self.schedule(Some(scope), id);
                }
            }
            State::Deferred => self.want(Some(scope), id),
            State::Queued | State::Running | State::Done => {}
        }
    }

    /// The coordinator: runs coordinator-only nodes and repository
    /// fetches, schedules outside starts, and handles quiescence.
    fn coordinate<'s>(
        &'s self,
        scope: &Scope<'s>,
        roots: &[NodeId],
        lane: &mut dyn RepoLane,
        cycles: &mut dyn CycleHandler,
    ) -> Outcome {
        let woken: Vec<NodeId> = std::mem::take(&mut *self.woken.lock())
            .into_iter()
            .filter(|&id| self.graph.entry(id).state() == State::Queued)
            .collect();
        self.schedule_many(scope, woken);
        let mut fresh = Vec::new();
        for &root in roots {
            let entry = self.graph.entry(root);
            if entry.transition(State::Fresh, State::Queued) {
                fresh.push(root);
            } else if entry.state() == State::Deferred {
                self.want(Some(scope), root);
            }
        }
        self.schedule_many(scope, fresh);
        loop {
            let mut coord = self.coord.lock();
            while coord.nodes.is_empty()
                && coord.repos.is_empty()
                && coord.starts.is_empty()
                && self.outstanding.load(Ordering::Acquire) != 0
            {
                let (guard, timeout) = coord.wait_timeout(
                    &self.coord_wake,
                    std::time::Duration::from_millis(IDLE_MILLIS),
                );
                coord = guard;
                if timeout
                    && coord.nodes.is_empty()
                    && coord.repos.is_empty()
                    && coord.starts.is_empty()
                {
                    drop(coord);
                    assert_no_locks("the coordinator's idle work");
                    self.bodies.idle();
                    coord = self.coord.lock();
                }
            }
            if let Some(id) = coord.nodes.pop_front() {
                drop(coord);
                if let Some(ready) = self.run_one(scope, id) {
                    self.schedule(Some(scope), ready);
                }
                self.outstanding.fetch_sub(1, Ordering::AcqRel);
                continue;
            }
            if !coord.starts.is_empty() {
                let starts = std::mem::take(&mut coord.starts);
                drop(coord);
                let queued = starts
                    .into_iter()
                    .filter(|&id| self.graph.entry(id).state() == State::Queued)
                    .collect();
                self.schedule_many(scope, queued);
                continue;
            }
            if let Some(name) = coord.repos.pop_front() {
                drop(coord);
                assert_no_locks("a repository fetch");
                lane.fetch(&name);
                self.counters.fetches.fetch_add(1, Ordering::Relaxed);
                let demanders = {
                    let mut repos = self.repos.lock();
                    let seq = self.repo_seq.fetch_add(1, Ordering::AcqRel) + 1;
                    repos.fetched.insert(name.clone(), seq);
                    repos.parked.remove(&name).unwrap_or_default()
                };
                let ready = demanders
                    .into_iter()
                    .filter(|&(id, epoch)| {
                        let entry = self.graph.entry(id);
                        entry.count_down(epoch, 1)
                            && entry.transition(State::Waiting, State::Queued)
                    })
                    .map(|(id, _)| id)
                    .collect();
                self.schedule_many(scope, ready);
                continue;
            }
            drop(coord);
            // Quiescent.
            if roots.iter().all(|&id| self.graph.value(id).is_some())
                || self.round.load(Ordering::SeqCst)
            {
                break;
            }
            self.counters.quiescences.fetch_add(1, Ordering::Relaxed);
            let resolution = cycles.at_quiescence(&self.wait_graph(roots));
            let Some(pairs) = self.new_answers(resolution) else {
                break;
            };
            for (parent, _) in pairs {
                self.requeue(scope, parent);
            }
        }
        if self.round.load(Ordering::SeqCst) {
            // No O(nodes) scan for a round.
            return Outcome {
                complete: roots.iter().all(|&id| self.graph.value(id).is_some()),
                ..Outcome::default()
            };
        }
        self.outcome(roots)
    }
}

/// How long the coordinator waits for pool work before it does its idle
/// work ([`Bodies::idle`]).
const IDLE_MILLIS: u64 = 2;

/// Pops the inline driver's [`INLINE_STACK`] when dropped.
struct InlineFrame;

impl Drop for InlineFrame {
    fn drop(&mut self) {
        INLINE_STACK.with(|stack| stack.borrow_mut().pop());
    }
}

/// BAZEL_RS_EVAL_TIMELINE: ends a pool attempt's frame when dropped.
struct TimedPool<'k>(bool, &'k NodeKey);

impl Drop for TimedPool<'_> {
    fn drop(&mut self) {
        if self.0 {
            super::timeline::pool_exit(self.1.kind(), || {
                format!("{:?}", self.1).chars().take(200).collect()
            });
        }
    }
}

/// How many pool waiters inline completions must have woken before the
/// inline driver hands them to the pool ([`Evaluator::set_assist`]).
static ASSIST_WOKEN: std::sync::LazyLock<usize> = std::sync::LazyLock::new(|| {
    // snapshot-unrecorded: a diagnostics switch choosing the scheduling only; the output is the same either way.
    std::env::var("BAZEL_RS_EVAL_ASSIST_WOKEN")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(8)
});

/// BAZEL_RS_EVAL_TIMELINE: ends a sequential attempt's frame when dropped.
struct TimedAttempt(Option<&'static str>, &'static str);

impl Drop for TimedAttempt {
    fn drop(&mut self) {
        if let Some(cause) = self.0 {
            super::timeline::seq_exit(self.1, cause);
        }
    }
}

/// Decrements the outstanding jobs when a job ends (a panicking body
/// included) and wakes the coordinator at zero.
struct Outstanding<'e, 'b, B: Bodies>(&'e Evaluator<'b, B>);

impl<B: Bodies> Drop for Outstanding<'_, '_, B> {
    fn drop(&mut self) {
        if self.0.outstanding.fetch_sub(1, Ordering::AcqRel) == 1 {
            let _coord = self.0.coord.lock();
            self.0.coord_wake.notify_one();
        }
    }
}

/// The inline driver's context: `need` runs missing nodes in place.
struct InlineCx<'e, 'l, 'b, B: Bodies> {
    evaluator: &'e Evaluator<'b, B>,
    lane: &'e RefCell<&'l mut dyn RepoLane>,
    id: NodeId,
    events: Vec<Event>,
}

impl<B: Bodies> Cx for InlineCx<'_, '_, '_, B> {
    fn need(&mut self, keys: &[NodeKey]) -> Result<Vals, Missing> {
        let evaluator = self.evaluator;
        let graph = &evaluator.graph;
        let mut values = Vec::with_capacity(keys.len());
        let mut missing = Vec::new();
        for key in keys {
            let id = graph.intern(key);
            let entry = graph.entry(id);
            let running = if evaluator.serving.load(Ordering::Relaxed) {
                // The pool's running attempts are no back edges.
                INLINE_STACK.with(|stack| stack.borrow().contains(&id))
            } else {
                entry.state() == State::Running
            };
            let back_edge = running || evaluator.answered(self.id, id);
            if back_edge {
                evaluator.answer(self.id, id);
                if missing.is_empty() {
                    return Err(Missing::BackEdge(id));
                }
                break;
            }
            if entry.value().is_none() {
                evaluator.run_inline(id, self.lane);
            }
            match entry.value() {
                Some(value) => values.push(Arc::clone(value)),
                None => missing.push(id),
            }
        }
        if missing.is_empty() {
            Ok(Vals(values))
        } else {
            Err(Missing::Nodes(missing))
        }
    }

    fn emit(&mut self, event: Event) {
        self.events.push(event);
    }

    fn current(&self) -> NodeId {
        self.id
    }

    fn id_of(&self, key: &NodeKey) -> NodeId {
        self.evaluator.graph.intern(key)
    }

    /// Serving: what the body needs soon runs on the pool meanwhile.
    fn hint(&mut self, keys: &[NodeKey]) {
        let evaluator = self.evaluator;
        if !evaluator.serving.load(Ordering::Relaxed) {
            return;
        }
        for key in keys {
            let id = evaluator.graph.intern(key);
            if id != self.id
                && evaluator
                    .graph
                    .entry(id)
                    .transition(State::Fresh, State::Queued)
            {
                evaluator.schedule(None, id);
            }
        }
    }

    fn peek(&self, key: &NodeKey) -> Option<Arc<NodeValue>> {
        let graph = &self.evaluator.graph;
        graph.get(key).and_then(|id| graph.value(id)).cloned()
    }
}

/// The pool driver's context: `need` answers from published values (with
/// `help_first`, it runs a node nobody runs yet in place first).
struct PoolCx<'r, 's, 'b, B: Bodies> {
    evaluator: &'s Evaluator<'b, B>,
    scope: &'r Scope<'s>,
    id: NodeId,
    events: Vec<Event>,
}

impl<'s, B: Bodies> PoolCx<'_, 's, '_, B> {
    /// Claim `id` if nobody runs it and run it here (help first).
    fn help(&self, id: NodeId) {
        let evaluator = self.evaluator;
        if evaluator.graph.key(id).coordinator_only() {
            // Parked for: it runs on the coordinator.
            return;
        }
        let entry = evaluator.graph.entry(id);
        let claimed = entry.transition(State::Fresh, State::Queued)
            || entry.state() == State::Queued
            || (entry.state() == State::Deferred && {
                evaluator.want(Some(self.scope), id);
                entry.state() == State::Queued
            });
        if claimed && let Some(ready) = evaluator.run_one(self.scope, id) {
            evaluator.schedule(Some(self.scope), ready);
        }
    }
}

impl<B: Bodies> Cx for PoolCx<'_, '_, '_, B> {
    fn need(&mut self, keys: &[NodeKey]) -> Result<Vals, Missing> {
        let evaluator = self.evaluator;
        let graph = &evaluator.graph;
        let mut values = Vec::with_capacity(keys.len());
        let mut missing = Vec::new();
        for key in keys {
            let id = graph.intern(key);
            if id == self.id || evaluator.answered(self.id, id) {
                evaluator.answer(self.id, id);
                if missing.is_empty() {
                    return Err(Missing::BackEdge(id));
                }
                break;
            }
            if evaluator.config.help_first && missing.is_empty() && graph.value(id).is_none() {
                self.help(id);
            }
            match graph.value(id) {
                Some(value) => values.push(Arc::clone(value)),
                None => missing.push(id),
            }
        }
        if missing.is_empty() {
            Ok(Vals(values))
        } else {
            Err(Missing::Nodes(missing))
        }
    }

    fn emit(&mut self, event: Event) {
        self.events.push(event);
    }

    fn current(&self) -> NodeId {
        self.id
    }

    fn id_of(&self, key: &NodeKey) -> NodeId {
        self.evaluator.graph.intern(key)
    }

    fn hint(&mut self, keys: &[NodeKey]) {
        let evaluator = self.evaluator;
        let fresh: Vec<NodeId> = keys
            .iter()
            .map(|key| evaluator.graph.intern(key))
            .filter(|&id| {
                id != self.id
                    && evaluator
                        .graph
                        .entry(id)
                        .transition(State::Fresh, State::Queued)
            })
            .collect();
        evaluator.schedule_many(self.scope, fresh);
    }

    fn peek(&self, key: &NodeKey) -> Option<Arc<NodeValue>> {
        let graph = &self.evaluator.graph;
        graph.get(key).and_then(|id| graph.value(id)).cloned()
    }
}

#[cfg(test)]
mod tests;
