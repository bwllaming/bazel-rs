//! The evaluator's node storage (docs/EVALUATOR.md "Scheduling").
//!
//! One [`Entry`] per interned [`NodeId`], in a segmented append-only arena
//! (the interner's layout: lock-free reads by id). An entry's life:
//!
//! ```text
//! Fresh ──claim──▶ Queued ──run──▶ Running ──value──▶ Done
//!                    ▲  │             │
//!                    │  └─▶ Deferred  └──Missing──▶ Waiting{missing}
//!                    └──────────── last dependency done ──┘
//! ```
//!
//! - **Single flight**: `Fresh → Queued` and `Queued → Running` are CAS
//!   claims; exactly one thread runs an attempt.
//! - **Waiting**: a parked node's `missing` counter holds its outstanding
//!   dependencies plus one guard (subtracted by the parker last), so the
//!   dependency that brings it to zero — and only that one — re-queues it.
//!   Waiters register under the dependency's short per-entry mutex; the
//!   completion takes the waiter list under the same mutex after the value
//!   is published, so no wake-up is lost.
//! - **Epochs**: every park bumps the node's epoch; a waiter registration
//!   carries it, and a completion only counts down registrations of the
//!   current epoch (a node the cycle handler re-queued by force leaves
//!   stale registrations behind).
//! - `Deferred`: frontier-only mode (after the first error without
//!   `--keep_going`) sets aside queued nodes nobody wants.
//!
//! No lock here is ever held across a body, a fetch or Starlark: the
//! [`Tracked`] mutex counts the locks a thread holds (debug builds) and
//! [`assert_no_locks`] checks the count where bodies and fetches start.
//!
//! Owner: worker A.

use std::cell::Cell;
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU32, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, OnceLock};

use super::key::{Interner, NodeId, NodeKey, slot_of};
use super::transcript::Transcript;
use super::value::NodeValue;

/// A node's scheduling state.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub(crate) enum State {
    /// Interned, never scheduled.
    Fresh = 0,
    /// Scheduled (a job or the coordinator queue will run it).
    Queued = 1,
    /// An attempt is running.
    Running = 2,
    /// Parked: waiting for dependencies, a repository, or the cycle
    /// handler.
    Waiting = 3,
    /// Its value is published.
    Done = 4,
    /// Set aside in frontier-only mode (nobody wants it yet).
    Deferred = 5,
}

impl State {
    fn from_u8(value: u8) -> Self {
        match value {
            0 => State::Fresh,
            1 => State::Queued,
            2 => State::Running,
            3 => State::Waiting,
            4 => State::Done,
            _ => State::Deferred,
        }
    }
}

/// What a parked node waits for (the cycle handler's wait graph).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) enum Wait {
    /// Not parked.
    #[default]
    None,
    /// These dependencies, in the order the body needed them.
    Nodes(Vec<NodeId>),
    /// A repository fetch (the coordinator's repository lane).
    Repo(String),
    /// The body returned `Missing::BackEdge(child)`: only the cycle
    /// handler can move it.
    BackEdge(NodeId),
    /// A repository the lane already fetched before the attempt began was
    /// demanded again: never resolvable (a body or lane bug); reported.
    Unresolvable(String),
}

impl Wait {
    /// The nodes it waits on, in need order.
    pub(crate) fn nodes(&self) -> &[NodeId] {
        match self {
            Wait::Nodes(nodes) => nodes,
            Wait::BackEdge(child) => std::slice::from_ref(child),
            _ => &[],
        }
    }
}

/// The mutable part of an entry (under its short mutex).
#[derive(Default)]
pub(crate) struct Inner {
    /// Parked nodes to count down at completion, with their epoch.
    pub(crate) waiters: Vec<(NodeId, u32)>,
    /// What this node waits for while parked.
    pub(crate) wait: Wait,
}

/// One node.
#[derive(Default)]
pub(crate) struct Entry {
    state: AtomicU8,
    /// Outstanding dependencies (+1 guard while parking).
    pub(crate) missing: AtomicU32,
    /// Bumped at every park (stale waiter registrations are ignored).
    pub(crate) epoch: AtomicU32,
    /// Attempts started (restarts = attempts - 1 once done).
    pub(crate) attempts: AtomicU32,
    /// Wanted by the linearizer's frontier (frontier-only mode).
    pub(crate) wanted: AtomicBool,
    /// The scheduling rank (0: none; higher first): `u32::MAX - i` for a
    /// node the `i`th started root needs (the earliest one), inherited by
    /// what it needs ([`Evaluator::start`](super::scheduler::Evaluator)).
    pub(crate) rank: AtomicU32,
    value: OnceLock<Arc<NodeValue>>,
    transcript: OnceLock<Transcript>,
    pub(crate) inner: Tracked<Inner>,
}

impl Entry {
    pub(crate) fn state(&self) -> State {
        State::from_u8(self.state.load(Ordering::Acquire))
    }

    pub(crate) fn set_state(&self, state: State) {
        self.state.store(state as u8, Ordering::Release);
    }

    /// CAS `from → to`; whether this thread won.
    pub(crate) fn transition(&self, from: State, to: State) -> bool {
        self.state
            .compare_exchange(from as u8, to as u8, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
    }

    /// The value once done (an acquire read; no lock).
    pub(crate) fn value(&self) -> Option<&Arc<NodeValue>> {
        self.value.get()
    }

    /// The node's transcript (the events its successful attempt emitted).
    pub(crate) fn transcript(&self) -> Option<&Transcript> {
        self.transcript.get()
    }

    /// Publish the value and transcript, mark the node done, and return
    /// the waiters to count down (taken under the entry's lock after the
    /// value is visible: a waiter registering later sees `Done`).
    pub(crate) fn complete(
        &self,
        value: Arc<NodeValue>,
        transcript: Transcript,
    ) -> Vec<(NodeId, u32)> {
        let fresh = self.value.set(value).is_ok();
        assert!(fresh, "internal error: a node completed twice");
        if !transcript.events.is_empty() {
            let _ = self.transcript.set(transcript);
        }
        let mut inner = self.inner.lock();
        inner.wait = Wait::None;
        self.set_state(State::Done);
        std::mem::take(&mut inner.waiters)
    }

    /// Register `waiter` (at `epoch`) unless done; whether it registered.
    pub(crate) fn add_waiter(&self, waiter: NodeId, epoch: u32) -> bool {
        if self.value.get().is_some() {
            return false;
        }
        let mut inner = self.inner.lock();
        if self.state() == State::Done {
            return false;
        }
        inner.waiters.push((waiter, epoch));
        true
    }

    /// Count down one dependency of the parked node at `epoch`; whether
    /// this was its last (the caller re-queues it).
    pub(crate) fn count_down(&self, epoch: u32, by: u32) -> bool {
        if self.epoch.load(Ordering::Acquire) != epoch {
            return false;
        }
        self.missing.fetch_sub(by, Ordering::AcqRel) == by
    }
}

/// Segments: as the interner's (`FIRST_SEGMENT << i` entries each).
const SEGMENTS: usize = 32;
const FIRST_SEGMENT: usize = 1024;

/// The node arena with its interner.
pub(crate) struct Graph {
    interner: Interner,
    segments: Box<[OnceLock<Box<[Entry]>>]>,
}

impl Default for Graph {
    fn default() -> Self {
        Self {
            interner: Interner::default(),
            segments: (0..SEGMENTS).map(|_| OnceLock::new()).collect(),
        }
    }
}

impl Graph {
    /// The id of `key` (its entry exists once this returns).
    pub(crate) fn intern(&self, key: &NodeKey) -> NodeId {
        let id = self.interner.intern(key);
        let (segment, _) = slot_of(id.0);
        self.segments[segment].get_or_init(|| {
            (0..FIRST_SEGMENT << segment)
                .map(|_| Entry::default())
                .collect()
        });
        id
    }

    /// The id of `key` if interned (its entry exists once this returns:
    /// another thread may be between interning the key and creating its
    /// segment).
    pub(crate) fn get(&self, key: &NodeKey) -> Option<NodeId> {
        let id = self.interner.get(key)?;
        let (segment, _) = slot_of(id.0);
        self.segments[segment].get_or_init(|| {
            (0..FIRST_SEGMENT << segment)
                .map(|_| Entry::default())
                .collect()
        });
        Some(id)
    }

    /// The key of `id`.
    pub(crate) fn key(&self, id: NodeId) -> &NodeKey {
        self.interner.key(id)
    }

    /// The entry of `id` (lock-free).
    pub(crate) fn entry(&self, id: NodeId) -> &Entry {
        let (segment, offset) = slot_of(id.0);
        &self.segments[segment]
            .get()
            .expect("internal error: an id the graph never interned")[offset]
    }

    /// The entry of `id` if its segment exists (a concurrent scan).
    pub(crate) fn entry_if(&self, id: NodeId) -> Option<&Entry> {
        let (segment, offset) = slot_of(id.0);
        self.segments[segment].get().map(|entries| &entries[offset])
    }

    /// The value of `id` once done.
    pub(crate) fn value(&self, id: NodeId) -> Option<&Arc<NodeValue>> {
        self.entry(id).value()
    }

    /// The number of interned nodes.
    pub(crate) fn len(&self) -> usize {
        self.interner.len()
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Every interned id.
    pub(crate) fn ids(&self) -> impl Iterator<Item = NodeId> {
        (0..self.len() as u32).map(NodeId)
    }
}

thread_local! {
    /// Evaluator locks this thread holds (debug lock-depth assertions).
    static LOCK_DEPTH: Cell<u32> = const { Cell::new(0) };
}

/// Assert that this thread holds no evaluator lock (debug builds): where a
/// body, a repository fetch or a Starlark evaluation starts. Bodies may
/// call it before entering Starlark.
pub(crate) fn assert_no_locks(what: &str) {
    if cfg!(debug_assertions) {
        let depth = LOCK_DEPTH.with(Cell::get);
        assert_eq!(
            depth, 0,
            "internal error: {what} started holding {depth} evaluator lock(s)"
        );
    }
}

/// A mutex whose guards are counted per thread (debug builds).
#[derive(Default)]
pub(crate) struct Tracked<T>(Mutex<T>);

/// A [`Tracked`] guard.
pub(crate) struct TrackedGuard<'a, T> {
    guard: Option<MutexGuard<'a, T>>,
}

impl<T> Tracked<T> {
    pub(crate) fn new(value: T) -> Self {
        Self(Mutex::new(value))
    }

    pub(crate) fn lock(&self) -> TrackedGuard<'_, T> {
        let guard = self.0.lock().unwrap_or_else(|e| e.into_inner());
        if cfg!(debug_assertions) {
            LOCK_DEPTH.with(|depth| depth.set(depth.get() + 1));
        }
        TrackedGuard { guard: Some(guard) }
    }
}

impl<'a, T> TrackedGuard<'a, T> {
    /// Wait on `condvar` (the lock is released meanwhile, and counted so).
    pub(crate) fn wait(mut self, condvar: &Condvar) -> Self {
        let guard = self.guard.take().expect("a live guard");
        if cfg!(debug_assertions) {
            LOCK_DEPTH.with(|depth| depth.set(depth.get() - 1));
        }
        let guard = condvar.wait(guard).unwrap_or_else(|e| e.into_inner());
        if cfg!(debug_assertions) {
            LOCK_DEPTH.with(|depth| depth.set(depth.get() + 1));
        }
        Self { guard: Some(guard) }
    }

    /// [`Self::wait`] for at most `timeout`; whether it timed out.
    pub(crate) fn wait_timeout(
        mut self,
        condvar: &Condvar,
        timeout: std::time::Duration,
    ) -> (Self, bool) {
        let guard = self.guard.take().expect("a live guard");
        if cfg!(debug_assertions) {
            LOCK_DEPTH.with(|depth| depth.set(depth.get() - 1));
        }
        let (guard, result) = condvar
            .wait_timeout(guard, timeout)
            .unwrap_or_else(|e| e.into_inner());
        if cfg!(debug_assertions) {
            LOCK_DEPTH.with(|depth| depth.set(depth.get() + 1));
        }
        (Self { guard: Some(guard) }, result.timed_out())
    }
}

impl<T> std::ops::Deref for TrackedGuard<'_, T> {
    type Target = T;
    fn deref(&self) -> &T {
        self.guard.as_ref().expect("a live guard")
    }
}

impl<T> std::ops::DerefMut for TrackedGuard<'_, T> {
    fn deref_mut(&mut self) -> &mut T {
        self.guard.as_mut().expect("a live guard")
    }
}

impl<T> Drop for TrackedGuard<'_, T> {
    fn drop(&mut self) {
        if self.guard.take().is_some() && cfg!(debug_assertions) {
            LOCK_DEPTH.with(|depth| depth.set(depth.get() - 1));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(i: u32) -> NodeKey {
        NodeKey::Tgt(crate::label::Label {
            repository: String::new(),
            package: "g".into(),
            name: i.to_string(),
        })
    }

    #[test]
    fn states_round_trip_and_claims_are_single_flight() {
        let graph = Graph::default();
        let id = graph.intern(&key(0));
        let entry = graph.entry(id);
        assert_eq!(entry.state(), State::Fresh);
        let wins: usize = std::thread::scope(|scope| {
            let handles: Vec<_> = (0..16)
                .map(|_| scope.spawn(|| entry.transition(State::Fresh, State::Queued) as usize))
                .collect();
            handles.into_iter().map(|h| h.join().unwrap()).sum()
        });
        assert_eq!(wins, 1);
        assert_eq!(entry.state(), State::Queued);
        for state in [
            State::Fresh,
            State::Queued,
            State::Running,
            State::Waiting,
            State::Done,
            State::Deferred,
        ] {
            assert_eq!(State::from_u8(state as u8), state);
        }
    }

    #[test]
    fn waiters_registered_before_completion_are_returned_after_it_never() {
        let graph = Graph::default();
        let (a, b) = (graph.intern(&key(1)), graph.intern(&key(2)));
        assert!(graph.entry(a).add_waiter(b, 7));
        let waiters = graph
            .entry(a)
            .complete(Arc::new(NodeValue::Src(Ok(()))), Transcript::default());
        assert_eq!(waiters, [(b, 7)]);
        assert!(!graph.entry(a).add_waiter(b, 8));
        assert_eq!(graph.entry(a).state(), State::Done);
        assert!(graph.value(a).is_some());
        assert!(graph.entry(a).transcript().is_none());
    }

    #[test]
    fn count_down_ignores_stale_epochs() {
        let graph = Graph::default();
        let entry = graph.entry(graph.intern(&key(3)));
        entry.epoch.store(2, Ordering::Release);
        entry.missing.store(2, Ordering::Release);
        assert!(!entry.count_down(1, 1));
        assert!(!entry.count_down(2, 1));
        assert!(entry.count_down(2, 1));
    }

    #[test]
    fn entries_exist_across_segments() {
        let graph = Graph::default();
        let ids: Vec<NodeId> = (0..5000).map(|i| graph.intern(&key(i))).collect();
        for id in ids {
            assert_eq!(graph.entry(id).state(), State::Fresh);
        }
        assert_eq!(graph.len(), 5000);
        assert_eq!(graph.ids().count(), 5000);
    }

    #[test]
    fn tracked_locks_are_counted() {
        let lock = Tracked::new(1);
        assert_no_locks("test");
        {
            let guard = lock.lock();
            assert_eq!(*guard, 1);
            assert_eq!(
                LOCK_DEPTH.with(Cell::get),
                u32::from(cfg!(debug_assertions))
            );
        }
        assert_no_locks("test");
    }

    #[test]
    #[cfg(debug_assertions)]
    #[should_panic(expected = "evaluator lock")]
    fn a_body_started_under_a_lock_panics() {
        let lock = Tracked::new(());
        let _guard = lock.lock();
        assert_no_locks("a body");
    }
}
