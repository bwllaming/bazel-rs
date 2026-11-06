//! P4 (docs/PARALLEL_ANALYSIS.md, "P4 — deferred action lowering"): the
//! pure tail of an action's commit — listing its resolved inputs, the
//! structural and namespace checks, the source/generated input split
//! ([`plan_action_sources`]) — runs on lowering workers while the
//! coordinator goes on committing the next targets.
//!
//! Everything a lowering decides is a function of what the coordinator
//! hands it (the action, its resolved input artifacts, its shared input
//! groups and inputs, the main repository's ignored entries and the client
//! environment). What it produces is placed in commit order: the planned
//! nodes of a build are a queue ([`PlannedNodes`]) whose entries — nodes
//! pushed directly, lowerings in flight and the console writes made after
//! each lowering was dispatched — are applied strictly in the order they
//! were queued. A failing lowering is the first error of the walk: the
//! entries queued after it (nodes, console writes, the warnings and errors
//! recorded since) are dropped, as the sequential engine would never have
//! made them. Lowerings are deferred only inside a pipelined plan walk
//! without `--keep_going` (whose failures must skip their dependents at
//! the failing target's turn); every walk drains its queue before it
//! returns, so nothing is in flight outside a walk.

use super::*;
use std::collections::VecDeque;
use std::sync::mpsc;

/// What a lowering produced.
pub(super) enum Lowered {
    /// The planned node, with its source inputs by action id.
    Node {
        planned: Box<PlannedAction>,
        sources: Option<(String, SourceInputs)>,
    },
    /// The tool runfiles groups of the action could not stay shared: it is
    /// listed with its trees' entries where it is placed (on the
    /// coordinator, which resolves them).
    Listing(Box<Listing>),
}

/// The listed path of [`Engine::commit_action`], finished where it is
/// placed.
pub(super) struct Listing {
    pub(super) action: Action,
    pub(super) own: Vec<Artifact>,
    pub(super) runfiles_trees: Vec<Arc<crate::analysis_runtime::ToolRunfilesTree>>,
    pub(super) deferred: Option<scheduler::Deferred>,
    pub(super) owner: String,
    pub(super) main_ignored: Arc<IgnoredSubdirectories>,
    pub(super) shared: Option<(usize, Arc<SharedInputs>)>,
}

type Outcome = std::thread::Result<Result<Lowered, String>>;

/// One entry of the queue, in commit order.
enum Queued {
    Node(Box<PlannedAction>),
    Lowering {
        outcome: mpsc::Receiver<Outcome>,
        /// The engine's warnings and errors when it was dispatched (kept
        /// up to there when it fails).
        warnings: usize,
        errors: usize,
    },
    /// Console writes made after the lowering before it was dispatched.
    Console(Vec<crate::ui::Deferred>),
}

/// The planned nodes of a build in plan order, with the lowerings still in
/// flight and what was held behind them.
#[derive(Default)]
pub(super) struct PlannedNodes {
    nodes: Vec<PlannedAction>,
    queue: VecDeque<Queued>,
    /// Lowerings may be deferred (a pipelined walk without keep-going).
    pub(super) deferring: bool,
    /// The coordinator's console writes are held (crate::ui::hold_restart)
    /// since the last lowering was dispatched.
    holding: bool,
    /// Placing a queued entry: pushes go to the nodes directly (the entry
    /// being placed is the front of the queue).
    placing: bool,
    /// The nodes of one target committed ahead of its turn (P4): its
    /// console writes are held by the commit's own deferred console and
    /// split at each lowering; [`Self::splice`] joins them at its turn.
    segment: bool,
}

impl PlannedNodes {
    /// Push a node planned on the coordinator (after everything queued).
    pub(super) fn push(&mut self, node: PlannedAction) {
        if self.queue.is_empty() || self.placing {
            self.nodes.push(node);
        } else {
            self.queue.push_back(Queued::Node(Box::new(node)));
        }
    }

    /// The planned nodes, taken (nothing may be in flight).
    pub(super) fn into_vec(self) -> Vec<PlannedAction> {
        debug_assert!(self.queue.is_empty(), "internal error: lowerings in flight");
        self.nodes
    }

    /// Nothing is queued: write what was held.
    fn release(&mut self) {
        if std::mem::take(&mut self.holding) {
            crate::ui::replay(crate::ui::hold_stop());
        }
    }

    /// The segment a commit ahead of its turn plans into.
    pub(super) fn segment() -> Self {
        Self {
            deferring: true,
            segment: true,
            ..Self::default()
        }
    }

    /// Writes held after the segment's last entry (the end of the commit
    /// ahead of its turn).
    pub(super) fn segment_close(&mut self, writes: Vec<crate::ui::Deferred>) {
        debug_assert!(self.segment);
        if !writes.is_empty() {
            self.queue.push_back(Queued::Console(writes));
        }
    }

    /// Join `segment` (a target committed ahead of its turn, at its turn)
    /// after everything planned so far, as if it were planned here; its
    /// lowerings' warning and error marks move from `from` to `to`.
    pub(super) fn splice(
        &mut self,
        segment: PlannedNodes,
        from: (usize, usize),
        to: (usize, usize),
    ) {
        debug_assert!(segment.segment && !self.segment);
        for node in segment.nodes {
            self.push(node);
        }
        for item in segment.queue {
            match item {
                Queued::Node(node) => self.push(*node),
                Queued::Console(writes) => crate::ui::replay(writes),
                Queued::Lowering {
                    outcome,
                    warnings,
                    errors,
                } => self.enqueue(outcome, warnings - from.0 + to.0, errors - from.1 + to.1),
            }
        }
    }

    fn enqueue(&mut self, outcome: mpsc::Receiver<Outcome>, warnings: usize, errors: usize) {
        if self.segment {
            let writes = crate::ui::deferred_split();
            if !writes.is_empty() {
                self.queue.push_back(Queued::Console(writes));
            }
            self.queue.push_back(Queued::Lowering {
                outcome,
                warnings,
                errors,
            });
            return;
        }
        let writes = crate::ui::hold_restart();
        if self.holding && !writes.is_empty() {
            self.queue.push_back(Queued::Console(writes));
        }
        self.holding = true;
        self.queue.push_back(Queued::Lowering {
            outcome,
            warnings,
            errors,
        });
    }
}

/// The planned nodes once nothing is in flight (the end of planning).
impl std::ops::Deref for PlannedNodes {
    type Target = Vec<PlannedAction>;

    fn deref(&self) -> &Vec<PlannedAction> {
        debug_assert!(self.queue.is_empty(), "internal error: lowerings in flight");
        &self.nodes
    }
}

impl std::ops::DerefMut for PlannedNodes {
    fn deref_mut(&mut self) -> &mut Vec<PlannedAction> {
        debug_assert!(self.queue.is_empty(), "internal error: lowerings in flight");
        &mut self.nodes
    }
}

#[cfg(test)]
impl From<Vec<PlannedAction>> for PlannedNodes {
    fn from(nodes: Vec<PlannedAction>) -> Self {
        Self {
            nodes,
            ..Self::default()
        }
    }
}

type Job = Box<dyn FnOnce() + Send>;

/// The lowering workers.
pub(super) struct LoweringPool {
    jobs: Option<mpsc::Sender<Job>>,
    workers: Vec<std::thread::JoinHandle<()>>,
}

impl LoweringPool {
    pub(super) fn start(size: usize) -> Option<Self> {
        // The workers allocate in the coordinator's jemalloc arena: what
        // they free is reusable by it (as when it lowered every action
        // itself), not held in arenas of their own until it decays.
        let coordinator_arena = arena::current();
        let (sender, receiver) = mpsc::channel::<Job>();
        let receiver = Arc::new(std::sync::Mutex::new(receiver));
        let mut workers = Vec::with_capacity(size);
        for index in 0..size {
            let receiver = Arc::clone(&receiver);
            let spawned = std::thread::Builder::new()
                .name(format!("lowering-{index}"))
                .stack_size(LOADING_WORKER_STACK)
                .spawn(move || {
                    if let Some(arena) = coordinator_arena {
                        arena::bind(arena);
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
            jobs: Some(sender),
            workers,
        })
    }
}

impl Drop for LoweringPool {
    fn drop(&mut self) {
        self.jobs = None;
        for worker in self.workers.drain(..) {
            let _ = worker.join();
        }
    }
}

/// The calling thread's jemalloc arena (`thread.arena`), with the
/// (default) jemalloc allocator; nothing without it. The only unsafe code
/// is jemalloc's documented `mallctl` call (its arena binding is not part
/// of tikv-jemallocator's API).
#[allow(unsafe_code)]
mod arena {
    #[cfg(feature = "jemalloc")]
    use tikv_jemallocator as _;

    #[cfg(feature = "jemalloc")]
    unsafe extern "C" {
        #[cfg_attr(not(target_env = "musl"), link_name = "_rjem_mallctl")]
        fn mallctl(
            name: *const std::ffi::c_char,
            oldp: *mut std::ffi::c_void,
            oldlenp: *mut usize,
            newp: *mut std::ffi::c_void,
            newlen: usize,
        ) -> std::ffi::c_int;
    }

    /// The arena this thread allocates in.
    pub(super) fn current() -> Option<u32> {
        #[cfg(feature = "jemalloc")]
        {
            let mut arena: u32 = 0;
            let mut length = std::mem::size_of::<u32>();
            // SAFETY: `thread.arena` reads an unsigned into `arena`, whose
            // size `length` gives.
            let status = unsafe {
                mallctl(
                    c"thread.arena".as_ptr(),
                    (&raw mut arena).cast(),
                    &raw mut length,
                    std::ptr::null_mut(),
                    0,
                )
            };
            (status == 0).then_some(arena)
        }
        #[cfg(not(feature = "jemalloc"))]
        None
    }

    /// Allocate in `arena` from now on.
    pub(super) fn bind(arena: u32) {
        #[cfg(feature = "jemalloc")]
        {
            let mut arena = arena;
            // SAFETY: `thread.arena` writes an unsigned arena index read
            // from `thread.arena` (an existing arena).
            unsafe {
                mallctl(
                    c"thread.arena".as_ptr(),
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                    (&raw mut arena).cast(),
                    std::mem::size_of::<u32>(),
                );
            }
        }
        #[cfg(not(feature = "jemalloc"))]
        let _ = arena;
    }
}

/// The most queued entries before a walk waits for the oldest (bounds the
/// memory of what waits to be placed).
const QUEUE_LIMIT: usize = 2_048;

/// `BAZEL_RS_LOWERING_WORKERS` (hidden): the number of lowering workers
/// (0: lower on the coordinator).
fn lowering_workers() -> usize {
    static WORKERS: std::sync::OnceLock<usize> = std::sync::OnceLock::new();
    *WORKERS.get_or_init(|| {
        // snapshot-unrecorded: a diagnostics override of the scheduling only; the output is the same either way.
        std::env::var("BAZEL_RS_LOWERING_WORKERS")
            .ok()
            .and_then(|value| value.parse().ok())
            .unwrap_or(4)
    })
}

/// `BAZEL_RS_LOWERING_FAIL=<id>,<id>…` (hidden test hook): the lowering of
/// each listed action fails (a failure only lowering can find is
/// otherwise rare: registration checks most of what it checks), wherever
/// it runs.
pub(super) fn test_failure(id: &str) -> Result<(), String> {
    static FAIL: std::sync::OnceLock<Vec<String>> = std::sync::OnceLock::new();
    let fail = FAIL.get_or_init(|| {
        // snapshot-unrecorded: a hidden test hook failing chosen action lowerings; never set outside the tests.
        std::env::var("BAZEL_RS_LOWERING_FAIL")
            .map(|ids| ids.split(',').map(str::to_owned).collect())
            .unwrap_or_default()
    });
    if fail.iter().any(|failing| failing == id) {
        return Err(format!("lowering of {id} failed (BAZEL_RS_LOWERING_FAIL)"));
    }
    Ok(())
}

impl Engine {
    /// Lower `job` on a lowering worker when the walk defers (its product
    /// is placed in commit order later: `None`), else here (`Some`: the
    /// caller places it).
    pub(super) fn lower(
        &self,
        nodes: &mut PlannedNodes,
        job: impl FnOnce() -> Result<Lowered, String> + Send + 'static,
    ) -> Result<Option<Lowered>, String> {
        // Work ahead of its turn (P3) runs in its own console scope: lowered
        // here, so the queue's console hold never interleaves with it.
        let Some(pool) = self
            .lowering_pool
            .as_ref()
            .filter(|_| nodes.deferring && (!self.ahead || nodes.segment))
        else {
            return job().map(Some);
        };
        let (sender, receiver) = mpsc::channel();
        let job: Job = Box::new(move || {
            let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(job));
            let _ = sender.send(outcome);
        });
        match pool.jobs.as_ref().map(|jobs| jobs.send(job)) {
            Some(Ok(())) => {
                nodes.enqueue(receiver, self.warnings.len(), self.errors.len());
                Ok(None)
            }
            Some(Err(error)) => {
                (error.0)();
                match receiver.recv() {
                    Ok(Ok(lowered)) => lowered.map(Some),
                    Ok(Err(panic)) => std::panic::resume_unwind(panic),
                    Err(_) => Err("internal error: a lowering worker stopped".to_owned()),
                }
            }
            None => Err("internal error: the lowering workers stopped".to_owned()),
        }
    }

    /// Place a lowering's product at its position (the end of the planned
    /// nodes).
    pub(super) fn place(
        &self,
        commit: &mut ActionCommit<'_>,
        lowered: Lowered,
    ) -> Result<(), String> {
        match lowered {
            Lowered::Node { planned, sources } => {
                if let Some((id, sources)) = sources {
                    commit.source_inputs.insert(id, sources);
                }
                commit.nodes.push(*planned);
                Ok(())
            }
            Lowered::Listing(listing) => self.commit_listed_action(*listing, commit),
        }
    }

    /// Start the lowering workers for a pipelined walk (none without
    /// workers or under `--keep_going`).
    pub(super) fn ensure_lowering_pool(&mut self) {
        if self.lowering_pool.is_none() && lowering_workers() > 0 {
            self.lowering_pool = LoweringPool::start(lowering_workers());
        }
    }

    pub(super) fn stop_lowering_pool(&mut self) {
        self.lowering_pool = None;
    }

    /// Place every queued entry whose lowering is done (all of them with
    /// `wait`, or until the queue is short enough with `bounded`); a failed
    /// lowering is the walk's first error.
    pub(super) fn drain_lowerings(
        &mut self,
        state: &mut ActionPlanning,
        wait: Drain,
    ) -> Result<(), String> {
        if state.nodes.queue.is_empty() {
            state.nodes.release();
            return Ok(());
        }
        loop {
            let ready = match state.nodes.queue.front() {
                None => break,
                Some(Queued::Lowering { outcome, .. }) => match wait {
                    Drain::All => true,
                    Drain::Bounded => {
                        state.nodes.queue.len() > QUEUE_LIMIT || {
                            // Peek: a finished lowering is placed now.
                            match outcome.try_recv() {
                                Ok(received) => {
                                    let Some(Queued::Lowering {
                                        warnings, errors, ..
                                    }) = state.nodes.queue.pop_front()
                                    else {
                                        unreachable!()
                                    };
                                    self.place_queued(state, received, warnings, errors)?;
                                    continue;
                                }
                                Err(mpsc::TryRecvError::Empty) => false,
                                Err(mpsc::TryRecvError::Disconnected) => true,
                            }
                        }
                    }
                },
                Some(_) => true,
            };
            if !ready {
                break;
            }
            match state.nodes.queue.pop_front() {
                Some(Queued::Node(node)) => state.nodes.nodes.push(*node),
                Some(Queued::Console(writes)) => crate::ui::replay_before_held(writes),
                Some(Queued::Lowering {
                    outcome,
                    warnings,
                    errors,
                }) => {
                    let received = outcome.recv().unwrap_or_else(|_| {
                        Ok(Err("internal error: a lowering worker stopped".to_owned()))
                    });
                    self.place_queued(state, received, warnings, errors)?;
                }
                None => break,
            }
        }
        if state.nodes.queue.is_empty() {
            state.nodes.release();
        }
        Ok(())
    }

    fn place_queued(
        &mut self,
        state: &mut ActionPlanning,
        received: Outcome,
        warnings: usize,
        errors: usize,
    ) -> Result<(), String> {
        let lowered = match received {
            Ok(Ok(lowered)) => lowered,
            Ok(Err(error)) => {
                self.abandon_lowerings(state, warnings, errors);
                return Err(error);
            }
            Err(panic) => {
                self.abandon_lowerings(state, warnings, errors);
                std::panic::resume_unwind(panic)
            }
        };
        let ActionPlanning {
            artifact_index,
            workspace_status,
            nodes,
            producers,
            source_inputs,
            client_environment,
            status_client_environment,
            input_groups,
            listed_tree_actions,
            ..
        } = state;
        nodes.placing = true;
        let mut commit = ActionCommit {
            artifact_index,
            workspace_status,
            nodes,
            producers,
            source_inputs,
            client_environment,
            status_client_environment,
            input_groups,
            listed_tree_actions,
        };
        let placed = self.place(&mut commit, lowered);
        state.nodes.placing = false;
        if placed.is_err() {
            self.abandon_lowerings(state, warnings, errors);
        }
        placed
    }

    /// A lowering failed: what was queued after it never happened.
    fn abandon_lowerings(&mut self, state: &mut ActionPlanning, warnings: usize, errors: usize) {
        state.nodes.queue.clear();
        if std::mem::take(&mut state.nodes.holding) {
            drop(crate::ui::hold_stop());
        }
        self.warnings.truncate(warnings);
        self.errors.truncate(errors);
    }
}

/// How much of the queue [`Engine::drain_lowerings`] places.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum Drain {
    /// Everything (waiting for the lowerings).
    All,
    /// What is done, and the oldest beyond the queue limit.
    Bounded,
}
