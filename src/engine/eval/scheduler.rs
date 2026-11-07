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
        }
    }
}

/// The analysis pool: 16 MiB stacks, workers allocating in the calling
/// thread's jemalloc arena (what they make is kept and freed by the
/// coordinator, src/alloc_arena.rs).
pub(crate) fn build_pool(threads: usize) -> rayon_core::ThreadPool {
    let arena = crate::alloc_arena::current();
    rayon_core::ThreadPoolBuilder::new()
        .num_threads(threads)
        .stack_size(WORKER_STACK)
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
}

impl<'b, B: Bodies> Evaluator<'b, B> {
    pub(crate) fn new(bodies: &'b B, config: EvalConfig) -> Self {
        let pool = (config.threads > 1).then(|| build_pool(config.threads));
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
        }
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
            None => self.evaluate_inline(&roots, lane, cycles),
            Some(pool) => pool.in_place_scope(|scope| self.coordinate(scope, &roots, lane, cycles)),
        }
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
                let requeued = waiting.transition(State::Waiting, State::Queued);
                debug_assert!(requeued, "internal error: a ready waiter was not waiting");
                ready.push(waiter);
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
        self.outcome(roots)
    }

    /// Run `id` to completion in place (re-running it after each
    /// `Missing` that can be resolved here); it is done or parked after.
    fn run_inline(&self, id: NodeId, lane: &RefCell<&mut dyn RepoLane>) {
        let entry = self.graph.entry(id);
        let key = self.graph.key(id);
        loop {
            entry.set_state(State::Running);
            entry.attempts.fetch_add(1, Ordering::Relaxed);
            let attempt_seq = self.repo_seq.load(Ordering::Acquire);
            assert_no_locks("a node body");
            let attempt = self.bodies.begin(id, key);
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
                    // Inline nodes have no registered waiters.
                    let _ = self.publish(id, value, events);
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
                    for &dep in &deps {
                        if matches!(
                            self.graph.entry(dep).state(),
                            State::Fresh | State::Queued | State::Deferred
                        ) {
                            self.run_inline(dep, lane);
                        }
                    }
                    if deps.iter().any(|&dep| self.graph.value(dep).is_none()) {
                        self.park_as(id, Wait::Nodes(deps), 1);
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
                            lane.borrow_mut().fetch(&name);
                            self.counters.fetches.fetch_add(1, Ordering::Relaxed);
                            let seq = self.repo_seq.fetch_add(1, Ordering::AcqRel) + 1;
                            self.repos.lock().fetched.insert(name, seq);
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

    // ---- the pool driver --------------------------------------------

    /// Schedule a node just made `Queued`: a pool job, or the
    /// coordinator's queue for a coordinator-only node or outside a job.
    fn schedule<'s>(&'s self, scope: Option<&Scope<'s>>, id: NodeId) {
        if self.pool.is_none() {
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
        let attempt = self.bodies.begin(id, key);
        let mut cx = PoolCx {
            evaluator: self,
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
            let requeued = entry.transition(State::Waiting, State::Queued);
            debug_assert!(requeued);
            return Some(id);
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
                coord = coord.wait(&self.coord_wake);
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
            if roots.iter().all(|&id| self.graph.value(id).is_some()) {
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
        self.outcome(roots)
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
            let back_edge = entry.state() == State::Running || evaluator.answered(self.id, id);
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
}

/// The pool driver's context: `need` answers from published values.
struct PoolCx<'e, 'b, B: Bodies> {
    evaluator: &'e Evaluator<'b, B>,
    id: NodeId,
    events: Vec<Event>,
}

impl<B: Bodies> Cx for PoolCx<'_, '_, B> {
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
}

#[cfg(test)]
mod tests;
