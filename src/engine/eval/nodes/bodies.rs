//! The phase-1 bodies as worker A's [`Bodies`]: run by A's evaluator
//! (S1: its inline driver, one thread, `need` evaluating in place = the
//! walk's DFS). [`Phase1Bodies`] holds the shared read view and the S1
//! coordinator ([`Coord`]: the engine's loading services and the
//! first-visit state pass 1 decides from S2 on); `begin`/`finish`/
//! `abandon` keep the stack of attempts (the walk's active chain for its
//! cycle texts) and the done CT1 values the bodies read without a need.
//!
//! Owner: worker B.

use std::cell::RefCell;
use std::sync::{Arc, Mutex, MutexGuard};

use crate::configuration::ConfigId;
use crate::fast_hash::{FxHashMap, FxHashSet};
use crate::label::Label;

use super::super::super::{ConfiguredTargetKey, Engine};
use super::super::cx::{Cx, Missing, Vals};
use super::super::key::{CtKey, NodeId, NodeKey};
use super::super::linearize::driver::{Linearizer, Published, Store};
use super::super::scheduler::Bodies;
use super::super::shared::EvalShared;
use super::super::transcript::{Event, Transcript};
use super::super::value::{Ct1Value, NodeValue};

/// The S1 coordinator state the bodies share (one thread in S1).
pub(crate) struct Coord<'e> {
    /// The engine's loading services (package loading, target lookups,
    /// package groups, aspect definitions, registration patterns):
    /// `Pkg`/`Tgt`/`Group` bodies over [`EvalShared`] in S2.
    pub(crate) engine: &'e mut Engine,
    /// Record `Access`/`Source` events (incremental tracking is on).
    pub(crate) tracking: bool,
    /// The finished transcripts, the segments and the captured console
    /// writes (worker C).
    pub(crate) store: Store,
    /// `BAZEL_RS_ANALYSIS=eval`'s linearizer (pass 1 and the tape's
    /// application; `None` under BAZEL_RS_EVAL_VERIFY_PHASE1).
    pub(crate) lin: Option<Box<Linearizer>>,
    /// The CT1 value of each ConfiguredTargetKey's first variant done (in
    /// completion order: the bodies' reads of a key they did not reach,
    /// and whether a key was analyzed).
    pub(crate) ct1_done: Arc<Ct1Done>,
    /// Every done CT1 value, by variant (pass 1 decides which one is the
    /// configured target).
    pub(crate) ct1_values: FxHashMap<CtKey, Arc<Ct1Value>>,
    /// The same values by node (pass 1's reads: no key hashed or compared).
    pub(crate) ct1_by_node: FxHashMap<NodeId, Arc<Ct1Value>>,
    /// The source files whose analysis succeeded (`analyzed_sources`).
    pub(crate) sources: FxHashSet<Label>,
    /// BAZEL_RS_EVAL_RESTARTS: the needs answered `Missing` once.
    pub(crate) restarted: FxHashSet<(NodeId, Vec<NodeId>)>,
    /// The FeatureFlagInfo values speculative phase 2s gave config_settings
    /// (`nodes::speculate`), compared with the planned ones once a flush
    /// planned their key.
    pub(crate) speculated: Vec<(
        ConfiguredTargetKey,
        Option<crate::feature_flags::FeatureFlagValue>,
    )>,
}

/// `BAZEL_RS_EVAL_TEST_SOURCE_SLEEP_MS=<ms>` (a hidden test hook): a
/// source lookup's finish first sleeps, widening the window between its
/// body and the publication of its observations.
fn source_test_sleep() {
    static MILLIS: std::sync::OnceLock<u64> = std::sync::OnceLock::new();
    let millis = *MILLIS.get_or_init(|| {
        // snapshot-unrecorded: a hidden test hook delaying a lookup's finish; no result depends on it.
        std::env::var("BAZEL_RS_EVAL_TEST_SOURCE_SLEEP_MS")
            .ok()
            .and_then(|value| value.parse().ok())
            .unwrap_or(0)
    });
    if millis > 0 {
        std::thread::sleep(std::time::Duration::from_millis(millis));
    }
}

/// BAZEL_RS_EVAL_RESTARTS=<seed>: the restart test hook's seed.
fn restarts_seed() -> Option<u64> {
    static SEED: std::sync::OnceLock<Option<u64>> = std::sync::OnceLock::new();
    *SEED.get_or_init(|| {
        // snapshot-unrecorded: a test hook; the analysis result must not change.
        std::env::var("BAZEL_RS_EVAL_RESTARTS")
            .ok()
            .and_then(|seed| seed.parse().ok())
    })
}

impl Coord<'_> {
    pub(crate) fn ct1(&self, key: &ConfiguredTargetKey) -> Option<Arc<Ct1Value>> {
        self.ct1_done.get(key)
    }

    pub(crate) fn source_analyzed(&self, label: &Label) -> bool {
        self.sources.contains(label)
    }
}

/// The CT1 value of each ConfiguredTargetKey's first variant done, read
/// by the bodies without the coordinator's lock (sharded).
pub(crate) struct Ct1Done {
    shards: [std::sync::RwLock<FxHashMap<ConfiguredTargetKey, Arc<Ct1Value>>>; 64],
}

impl Default for Ct1Done {
    fn default() -> Self {
        Self {
            shards: std::array::from_fn(|_| Default::default()),
        }
    }
}

impl Ct1Done {
    fn shard(
        &self,
        key: &ConfiguredTargetKey,
    ) -> &std::sync::RwLock<FxHashMap<ConfiguredTargetKey, Arc<Ct1Value>>> {
        use std::hash::BuildHasher;
        let hash = crate::fast_hash::FxBuildHasher::default().hash_one(key);
        &self.shards[(hash >> 58) as usize]
    }

    pub(crate) fn get(&self, key: &ConfiguredTargetKey) -> Option<Arc<Ct1Value>> {
        self.shard(key)
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(key)
            .cloned()
    }

    /// Keep `value` unless a variant of `key` was done first.
    pub(crate) fn first(&self, key: &ConfiguredTargetKey, value: &Arc<Ct1Value>) {
        self.shard(key)
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .entry(key.clone())
            .or_insert_with(|| Arc::clone(value));
    }
}

thread_local! {
    /// This thread's running attempts, outermost first, each with what it
    /// emitted so far (worker C: the transcripts pass 1 replays). One
    /// thread's attempts nest like the walk's (the inline driver; the
    /// pool's help-first needs, S2).
    static RUNNING: RefCell<Vec<Running>> = const { RefCell::new(Vec::new()) };
    /// This thread runs pool attempts (S2).
    static POOL_THREAD: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Run `f` on this thread's running attempts.
pub(crate) fn with_running<R>(f: impl FnOnce(&mut Vec<Running>) -> R) -> R {
    RUNNING.with(|running| f(&mut running.borrow_mut()))
}

/// Record `event` in this thread's running attempt; `Err` (with it) when
/// none runs. A rerun re-emits its abandoned attempt's events, which stay
/// in place (pass 1 may have replayed them); the one-time effects of the
/// services there (a package's load output and commit, its warnings, a
/// lookup's access and scopes) are not re-emitted; another difference is
/// an internal error ([`rerun_mismatch`]).
pub(crate) fn push_running(event: Event) -> Result<(), Event> {
    with_running(|running| match running.last_mut() {
        Some(running) if running.cursor < running.replay => {
            let one_time = |event: &Event| {
                matches!(
                    event,
                    Event::Console(_)
                        | Event::PkgCommit(..)
                        | Event::LoadPrints(_)
                        | Event::Warn(_)
                        | Event::Access(_)
                        | Event::ScopeEnter(_)
                        | Event::ScopeExit
                )
            };
            while running.cursor < running.replay
                && running.events[running.cursor] != event
                && one_time(&running.events[running.cursor])
            {
                running.cursor += 1;
            }
            if running.cursor < running.replay && running.events[running.cursor] == event {
                running.cursor += 1;
                return Ok(());
            }
            if running.cursor < running.replay {
                note_mismatch(format!(
                    "{:?} re-emits {event:?} where its abandoned attempt emitted {:?}",
                    running.key, running.events[running.cursor]
                ));
                running.events.truncate(running.cursor);
                running.replay = running.cursor;
            }
            running.events.push(event);
            Ok(())
        }
        Some(top) => {
            top.events.push(event);
            Ok(())
        }
        None => Err(event),
    })
}

static RERUN_MISMATCH: Mutex<Option<String>> = Mutex::new(None);

fn note_mismatch(mismatch: String) {
    RERUN_MISMATCH
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .get_or_insert(mismatch);
}

/// A rerun emitted other events than its abandoned attempt (an internal
/// error: pass 1 may have replayed them); taken.
pub(crate) fn rerun_mismatch() -> Option<String> {
    RERUN_MISMATCH
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .take()
        // An extension group the pool used speculatively and its commit
        // dropped (docs/EVALUATOR.md "Extensions on the pool").
        .or_else(crate::repository::take_eval_adoption_mismatch)
}

/// Whether `key` is running on this thread.
pub(crate) fn in_progress(key: &NodeKey) -> bool {
    with_running(|running| running.iter().any(|running| running.key == *key))
}

/// The configured targets being analyzed, outermost first, since the
/// last detached analysis (a platform's tree, a registration: the walk's
/// fresh `active`): the inline driver's chain; empty on a pool thread
/// (cycles are back edges there, the depth limit is pass 1's).
pub(crate) fn active_ct1() -> Vec<ConfiguredTargetKey> {
    if POOL_THREAD.with(std::cell::Cell::get) {
        return Vec::new();
    }
    with_running(|running| {
        let mut active = Vec::new();
        for running in running.iter().rev() {
            match &running.key {
                NodeKey::Ct1(ct) => active.push(ct.key.clone()),
                NodeKey::Platform(_) | NodeKey::RegToolchains(_) | NodeKey::RegExecPlatforms(_) => {
                    break;
                }
                _ => {}
            }
        }
        active.reverse();
        active
    })
}

/// One running attempt.
pub(crate) struct Running {
    pub(crate) id: NodeId,
    pub(crate) key: NodeKey,
    /// What it emitted so far (a rerun: starting with the abandoned
    /// attempt's events, which it re-emits; `Coord::push_event`).
    pub(crate) events: Vec<Event>,
    /// The leading events a rerun re-emits (the abandoned attempt's, up to
    /// its `Missing`): pass 1 may have replayed them already.
    pub(crate) replay: usize,
    /// How many of them the rerun re-emitted.
    pub(crate) cursor: usize,
    /// The events when the attempt first received a `Missing` (what it
    /// emitted while it stopped is not the rerun's).
    pub(crate) missing_at: Option<usize>,
}

/// The coordinator's lock held: with `BAZEL_RS_EVAL_TIMELINE`, a hold of
/// 5 ms or more is summed by the call site that took it.
pub(crate) struct CoordGuard<'a, 'e> {
    guard: MutexGuard<'a, Coord<'e>>,
    held: Option<(std::time::Instant, &'static std::panic::Location<'static>)>,
}

/// BAZEL_RS_EVAL_TIMELINE: the call site holding the coordinator's lock.
static HOLDER: std::sync::Mutex<Option<&'static std::panic::Location<'static>>> =
    std::sync::Mutex::new(None);

impl<'a, 'e> CoordGuard<'a, 'e> {
    #[track_caller]
    fn new(guard: MutexGuard<'a, Coord<'e>>) -> Self {
        let caller = std::panic::Location::caller();
        let held = crate::eval_timeline::on().then(|| (std::time::Instant::now(), caller));
        if held.is_some() {
            *HOLDER
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(caller);
        }
        CoordGuard { guard, held }
    }
}

/// The call site holding the coordinator's lock (with the timeline).
fn holder() -> Option<&'static std::panic::Location<'static>> {
    *HOLDER
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

impl<'e> std::ops::Deref for CoordGuard<'_, 'e> {
    type Target = Coord<'e>;
    fn deref(&self) -> &Coord<'e> {
        &self.guard
    }
}

impl<'e> std::ops::DerefMut for CoordGuard<'_, 'e> {
    fn deref_mut(&mut self) -> &mut Coord<'e> {
        &mut self.guard
    }
}

impl Drop for CoordGuard<'_, '_> {
    fn drop(&mut self) {
        if let Some((since, caller)) = self.held
            && since.elapsed() >= std::time::Duration::from_millis(5)
        {
            super::super::timeline::interval("coordinator lock held >= 5 ms", since);
            super::super::timeline::add_dyn(
                || {
                    format!(
                        "coordinator lock held >= 5 ms at {}:{}",
                        caller.file(),
                        caller.line()
                    )
                },
                since,
            );
        }
    }
}

/// What a phase-1 body needs of its context besides [`Cx`].
pub(crate) trait Phase1Cx<'e>: Cx {
    #[track_caller]
    fn coord(&self) -> CoordGuard<'_, 'e>;
    /// The coordinator if no other thread holds it now.
    fn try_coord(&self) -> Option<CoordGuard<'_, 'e>>;
    /// The CT1 value of `key`'s first variant done (without the
    /// coordinator's lock).
    fn ct1_done(&self, key: &ConfiguredTargetKey) -> Option<Arc<Ct1Value>>;
    /// The scratch engines of the speculative phase 2s
    /// (`nodes::speculate`; one is locked before the coordinator, never
    /// after it).
    fn scratch(&self) -> &[Mutex<Option<super::speculate::Scratch>>];
    /// The evaluation's fixed switches (S2: read without the
    /// coordinator's lock).
    fn flags(&self) -> Flags;
    /// Hand a store update to the coordinator (on the pool: without its
    /// lock, applied in this node's order before anybody reads the store).
    fn defer(&self, apply: Deferred<'e>);
    /// The running attempt received a `Missing` (it stops here).
    fn missing_here(&self);
}

/// An evaluation's fixed switches.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct Flags {
    /// Record `Access`/`Source` events (incremental tracking is on).
    pub(crate) tracking: bool,
    /// `BAZEL_RS_ANALYSIS=eval`'s linearizer keeps the transcripts.
    pub(crate) linearized: bool,
}

/// A body's context: A's `Cx` and the coordinator.
struct BodyCx<'a, 's, 'e> {
    cx: &'a mut dyn Cx,
    bodies: &'a Phase1Bodies<'s, 'e>,
}

impl Cx for BodyCx<'_, '_, '_> {
    fn need(&mut self, keys: &[NodeKey]) -> Result<Vals, Missing> {
        let vals = match self.cx.need(keys) {
            Ok(vals) => vals,
            Err(missing) => {
                self.bodies.missing_here();
                return Err(missing);
            }
        };
        // BAZEL_RS_EVAL_RESTARTS=<seed> (a test hook): the first need of a
        // key by a node is answered `Missing` at random (one in four), as
        // a parallel evaluation parks an attempt: the attempt is abandoned
        // and the node runs again (its transcript must not change).
        if let Some(seed) = restarts_seed() {
            let current = self.cx.current();
            let ids: Vec<NodeId> = keys.iter().map(|key| self.cx.id_of(key)).collect();
            use std::hash::BuildHasher;
            let roll = crate::fast_hash::FxBuildHasher::default().hash_one((
                seed,
                current,
                format!("{keys:?}"),
            ));
            let fresh = self.coord().restarted.insert((current, ids.clone()));
            if fresh && roll % 4 == 0 {
                self.bodies.missing_here();
                return Err(Missing::Nodes(ids));
            }
        }
        Ok(vals)
    }

    fn emit(&mut self, event: Event) {
        if event == Event::Flush {
            // Without the coordinator's lock (every config_setting with
            // Starlark flag_values emits one).
            self.bodies
                .saw_flush
                .store(true, std::sync::atomic::Ordering::Relaxed);
        }
        self.bodies.drain_local();
        // Under BAZEL_RS_ANALYSIS=eval the coordinator's store is the
        // transcript of record (one copy: ~/code emits millions of events).
        let copy = (!self.bodies.flags.linearized).then(|| event.clone());
        if let Err(event) = push_running(event) {
            self.coord().push_event(event);
        }
        if let Some(event) = copy {
            self.cx.emit(event);
        }
    }

    fn current(&self) -> NodeId {
        self.cx.current()
    }

    fn id_of(&self, key: &NodeKey) -> NodeId {
        self.cx.id_of(key)
    }

    fn hint(&mut self, keys: &[NodeKey]) {
        self.cx.hint(keys);
    }

    fn peek(&self, key: &NodeKey) -> Option<Arc<NodeValue>> {
        self.cx.peek(key)
    }
}

impl<'e> Phase1Cx<'e> for BodyCx<'_, '_, 'e> {
    #[track_caller]
    fn coord(&self) -> CoordGuard<'_, 'e> {
        self.bodies.coord()
    }

    fn try_coord(&self) -> Option<CoordGuard<'_, 'e>> {
        let mut coord = match self.bodies.coord.try_lock() {
            Ok(guard) => guard,
            Err(std::sync::TryLockError::Poisoned(poisoned)) => poisoned.into_inner(),
            Err(std::sync::TryLockError::WouldBlock) => return None,
        };
        self.bodies.inbox.absorb(&mut coord);
        Some(CoordGuard::new(coord))
    }

    fn scratch(&self) -> &[Mutex<Option<super::speculate::Scratch>>] {
        &self.bodies.scratch
    }

    fn ct1_done(&self, key: &ConfiguredTargetKey) -> Option<Arc<Ct1Value>> {
        self.bodies.ct1_done.get(key)
    }

    fn flags(&self) -> Flags {
        self.bodies.flags
    }

    fn defer(&self, apply: Deferred<'e>) {
        self.bodies.defer(apply);
    }

    fn missing_here(&self) {
        self.bodies.missing_here();
    }
}

/// BAZEL_RS_EVAL_STATS: why pool attempts were left to the sequential
/// pass (a flush, a repository sentinel, an order-dependent decision,
/// obtained roots).
pub(crate) static INLINE_REASONS: [std::sync::atomic::AtomicU64; 4] = [
    std::sync::atomic::AtomicU64::new(0),
    std::sync::atomic::AtomicU64::new(0),
    std::sync::atomic::AtomicU64::new(0),
    std::sync::atomic::AtomicU64::new(0),
];

/// The attempts the sequential pass of a pooled evaluation ran (S2: the
/// driver runs the pool again after it ran any).
pub(crate) static SEQUENTIAL_ATTEMPTS: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);

/// BAZEL_RS_EVAL_STATS: the coordinator lock's uncontended and contended
/// acquisitions, and the µs waited.
pub(crate) static COORD_LOCKS: [std::sync::atomic::AtomicU64; 3] = [
    std::sync::atomic::AtomicU64::new(0),
    std::sync::atomic::AtomicU64::new(0),
    std::sync::atomic::AtomicU64::new(0),
];

/// BAZEL_RS_EVAL_STATS: the µs the sequential pass's attempts spent
/// applying the tape first.
pub(crate) static BEGIN_LINEARIZE: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);

/// BAZEL_RS_EVAL_PARKED_DEBUG: the sequential pass's attempts by kind.
pub(crate) static SEQUENTIAL_KINDS: Mutex<std::collections::BTreeMap<&'static str, u64>> =
    Mutex::new(std::collections::BTreeMap::new());

/// Whether the coordinator linearizes while the pool runs
/// (`BAZEL_RS_EVAL_NO_IDLE_LINEARIZE=1`: not).
static IDLE_LINEARIZE: std::sync::LazyLock<bool> = std::sync::LazyLock::new(|| {
    // snapshot-unrecorded: a diagnostics switch choosing the scheduling only; the output is the same either way.
    std::env::var_os("BAZEL_RS_EVAL_NO_IDLE_LINEARIZE").is_none()
});

/// The marker of [`Missing::Repo`] for an attempt only the sequential
/// pass can run (S2: on the pool, a flush or a repository demand): it
/// parks until the inline pass.
pub(crate) const INLINE: &str = "\u{1}inline";

/// The phase-1 node bodies for A's evaluator.
pub(crate) struct Phase1Bodies<'s, 'e> {
    pub(crate) shared: &'s EvalShared,
    pub(crate) coord: Mutex<Coord<'e>>,
    pub(crate) flags: Flags,
    /// Attempts observe in frames committed in the tape's order (S2: a
    /// pooled evaluation; the inline driver alone observes in order).
    pub(crate) framed: bool,
    /// The speculative phase 2s' scratch engines (S2, the CsMatch split;
    /// several speculations run at once, each on a scratch engine of its
    /// own).
    pub(crate) scratch: Vec<Mutex<Option<super::speculate::Scratch>>>,
    /// `Coord::ct1_done`, shared.
    pub(crate) ct1_done: Arc<Ct1Done>,
    /// What pool attempts leave for the store (eval-round1: no
    /// coordinator lock in an attempt's begin, finish or abandon).
    inbox: Inbox<'e>,
    /// A `Flush` was emitted (a config_setting with Starlark flag_values
    /// keys: its CT2 dependencies are S2).
    pub(crate) saw_flush: std::sync::atomic::AtomicBool,
}

impl<'s, 'e> Phase1Bodies<'s, 'e> {
    pub(crate) fn new(
        shared: &'s EvalShared,
        engine: &'e mut Engine,
        tracking: bool,
        linearized: bool,
    ) -> Self {
        let ct1_done = Arc::new(Ct1Done::default());
        Self {
            shared,
            ct1_done: Arc::clone(&ct1_done),
            flags: Flags {
                tracking,
                linearized,
            },
            framed: false,
            scratch: (0..super::speculate::scratch_slots())
                .map(|_| Mutex::new(None))
                .collect(),
            inbox: Inbox::default(),
            saw_flush: std::sync::atomic::AtomicBool::new(false),
            coord: Mutex::new(Coord {
                engine,
                tracking,
                store: Store::default(),
                lin: None,
                ct1_done: Arc::clone(&ct1_done),
                ct1_values: FxHashMap::default(),
                ct1_by_node: FxHashMap::default(),
                sources: FxHashSet::default(),
                restarted: FxHashSet::default(),
                speculated: Vec::new(),
            }),
        }
    }

    /// The coordinator, the pool attempts' records applied to its store.
    #[track_caller]
    pub(crate) fn coord(&self) -> CoordGuard<'_, 'e> {
        let mut coord = self.lock_coord();
        self.inbox.absorb(&mut coord);
        coord
    }

    /// Hand `apply` to the coordinator's store: at once off the pool, else
    /// with the running attempt's records (in its node's order).
    pub(crate) fn defer(&self, apply: Deferred<'e>) {
        if self.on_pool() {
            // In the running attempt's shard, after its earlier records;
            // without one (never on the pool: every body runs in an
            // attempt) any shard does, the update is order-free.
            let id = with_running(|running| running.last().map(|running| running.id));
            self.inbox
                .push(id.unwrap_or(NodeId(0)), Record::Apply(apply));
        } else {
            apply(&mut self.coord());
        }
    }

    /// The running attempt received a `Missing`.
    pub(crate) fn missing_here(&self) {
        if self.on_pool() {
            self.drain_local();
            mark_missing();
        } else {
            self.coord().missing_here();
        }
    }

    /// Whether this thread runs pool attempts now.
    fn on_pool(&self) -> bool {
        self.pooled() && POOL_THREAD.with(std::cell::Cell::get)
    }

    #[track_caller]
    fn lock_coord(&self) -> CoordGuard<'_, 'e> {
        match self.coord.try_lock() {
            Ok(guard) => {
                COORD_LOCKS[0].fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                CoordGuard::new(guard)
            }
            Err(std::sync::TryLockError::Poisoned(poisoned)) => {
                CoordGuard::new(poisoned.into_inner())
            }
            Err(std::sync::TryLockError::WouldBlock) => {
                let started = std::time::Instant::now();
                let behind = super::super::timeline::on().then(holder).flatten();
                let _blocked =
                    super::super::timeline::blocked_on(|| "the coordinator's lock".to_owned());
                let guard = self
                    .coord
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                COORD_LOCKS[1].fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                let caller = std::panic::Location::caller();
                super::super::timeline::add_dyn(
                    || {
                        format!(
                            "coordinator lock waited at {}:{}",
                            caller.file(),
                            caller.line()
                        )
                    },
                    started,
                );
                super::super::timeline::interval("coordinator lock waited", started);
                if let Some(behind) = behind {
                    super::super::timeline::add_dyn(
                        || {
                            format!(
                                "coordinator lock waited at {}:{} behind {}:{}",
                                caller.file(),
                                caller.line(),
                                behind.file(),
                                behind.line()
                            )
                        },
                        started,
                    );
                }
                COORD_LOCKS[2].fetch_add(
                    started.elapsed().as_micros() as u64,
                    std::sync::atomic::Ordering::Relaxed,
                );
                CoordGuard::new(guard)
            }
        }
    }

    /// Whether attempts run on the pool now.
    fn pooled(&self) -> bool {
        self.shared.loading.pooled_here()
    }

    /// Move what this thread captured since the last call (tracker
    /// scopes and lookups, console writes) into its running attempt.
    fn drain_local(&self) {
        let mut events = super::super::linearize::capture::take();
        let writes = crate::ui::deferred_split();
        if events.is_empty() && writes.is_empty() {
            return;
        }
        if !writes.is_empty() {
            let mut coord = self.coord();
            for write in writes {
                let index = coord.store.console(write);
                events.push(Event::Console(index));
            }
        }
        for event in events {
            if let Err(event) = push_running(event) {
                self.coord().push_event(event);
            }
        }
    }
}

/// A pool thread's evaluation setup (once per thread): its console held
/// and its tracker captures on, as the coordinator's.
fn pool_thread_setup(tracking: bool) {
    if !POOL_THREAD.with(|pool| pool.replace(true)) {
        // Held for the thread's life (every write is drained into an
        // attempt): never dropped, so no thread-local is touched at exit.
        std::mem::forget(crate::ui::DeferredConsole::start());
        crate::ui::set_count_deferred(true);
        super::super::linearize::capture::start(tracking);
    }
}

impl Bodies for Phase1Bodies<'_, '_> {
    /// A pooled evaluation's attempt observes in its own frame (committed
    /// where the tape replays the node; an abandoned attempt's is dropped:
    /// its re-run observes again).
    type Attempt = Option<crate::observed_fs::UnitScope>;

    fn begin(&self, id: NodeId, key: &NodeKey) -> Self::Attempt {
        if self.pooled()
            && std::thread::current()
                .name()
                .is_some_and(|name| name.starts_with("eval-"))
        {
            pool_thread_setup(self.flags.tracking);
        }
        self.drain_local();
        let events = if self.on_pool() {
            // No coordinator lock on the pool: the store learns of the
            // attempt with its next records.
            self.inbox.push(id, Record::Began(id, key.clone()));
            self.inbox.take_carry(id)
        } else {
            let mut coord = self.coord();
            if self.framed && !self.pooled() {
                SEQUENTIAL_ATTEMPTS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                // snapshot-unrecorded: a diagnostics switch; it changes nothing the analysis computes.
                if std::env::var_os("BAZEL_RS_EVAL_PARKED_DEBUG").is_some() {
                    *SEQUENTIAL_KINDS
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .entry(key.kind())
                        .or_default() += 1;
                }
                // The sequential pass of a pooled evaluation: what the walk
                // did before this point (the pool's packages committed,
                // their roots counting) is applied first.
                let started = std::time::Instant::now();
                let _ = coord.linearize(self.shared);
                BEGIN_LINEARIZE.fetch_add(
                    started.elapsed().as_micros() as u64,
                    std::sync::atomic::Ordering::Relaxed,
                );
            }
            coord.drain();
            coord.begin_attempt(id, key);
            drop(coord);
            self.inbox.take_carry(id)
        };
        with_running(|running| {
            running.push(Running {
                id,
                key: key.clone(),
                replay: events.len(),
                cursor: 0,
                events,
                missing_at: None,
            })
        });
        self.framed.then(crate::observed_fs::unit_scope)
    }

    fn run(&self, cx: &mut dyn Cx, key: &NodeKey) -> Result<NodeValue, Missing> {
        let mut cx = BodyCx { cx, bodies: self };
        if !self.pooled() {
            return super::eval_host::body(self.shared, &mut cx, key);
        }
        // On the pool: no repository demand (a repository a body needs, an
        // order-dependent decision: the attempt runs in the sequential
        // pass, where demand may fetch), what it observes in its own frame.
        let sentinels = crate::repository_demand::sentinels_raised();
        let order_dependent = crate::repository_demand::take_order_dependent();
        let roots = crate::repository::UnitRoots::record();
        let no_demand = crate::repository_demand::no_demand();
        let reuse = crate::repository_demand::reuse_only();
        let result = super::eval_host::body(self.shared, &mut cx, key);
        drop(reuse);
        drop(no_demand);
        // Roots obtained before the sequential engine had them count from
        // where the tape replays the node.
        let mut obtained = roots.finish();
        if crate::repository_demand::sentinels_raised() != sentinels
            && !matches!(key, NodeKey::Pkg(_))
        {
            // Left to the sequential pass (below), which prepares an
            // extension group it needs itself, where the walk does.
            obtained.retain(|root| !root.starts_with(crate::repository::EVAL_GROUP_KEY));
        }
        if !obtained.is_empty() && result.is_ok() {
            let current = cx.current();
            self.defer(Box::new(move |coord: &mut Coord<'_>| {
                coord
                    .store
                    .roots
                    .entry(current)
                    .or_default()
                    .extend(obtained);
            }));
            INLINE_REASONS[3].fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        }
        let sentinel = crate::repository_demand::sentinels_raised() != sentinels;
        let order = crate::repository_demand::take_order_dependent();
        for (counted, reason) in [(sentinel, 1), (order, 2)] {
            if counted && result.is_ok() {
                INLINE_REASONS[reason].fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            }
        }
        let raised = sentinel || order;
        if raised && result.is_ok() && !matches!(key, NodeKey::Pkg(_)) {
            super::super::timeline::note_inline(cx.current().0, || {
                let what = if sentinel {
                    crate::repository_demand::last_sentinel().unwrap_or_default()
                } else {
                    "an order-dependent decision".to_owned()
                };
                format!("a swallowed sentinel ({what})")
            });
        }
        crate::repository_demand::restore_order_dependent(order_dependent);
        match result {
            Ok(_) if raised && !matches!(key, NodeKey::Pkg(_)) => {
                // What it emitted after the swallowed sentinel is not the
                // sequential pass's: only a rerun's re-emitted prefix stays.
                with_running(|running| {
                    if let Some(top) = running.last_mut() {
                        top.missing_at = Some(top.replay);
                    }
                });
                Err(Missing::Repo(INLINE.to_owned()))
            }
            result => result,
        }
    }

    fn finish(&self, attempt: Self::Attempt, value: NodeValue) -> NodeValue {
        let observations = attempt.map(crate::observed_fs::UnitScope::finish);
        self.drain_local();
        let Some(running) = with_running(Vec::pop) else {
            return value;
        };
        if let Some(event) = running.events[running.cursor.min(running.replay)..running.replay]
            .iter()
            .find(|event| {
                !matches!(
                    event,
                    Event::Console(_)
                        | Event::PkgCommit(..)
                        | Event::LoadPrints(_)
                        | Event::Warn(_)
                        | Event::Access(_)
                        | Event::ScopeEnter(_)
                        | Event::ScopeExit
                )
            })
        {
            note_mismatch(format!(
                "{:?} finished without re-emitting {event:?} of its abandoned attempt",
                running.key
            ));
        }
        if matches!(running.key, NodeKey::Tgt(_)) {
            // A lookup's observations are its user's (the enclosing
            // attempt's frame on this thread); a hinted lookup's own job's
            // are its package's (where the tape first uses the package; a
            // package loaded before phase 1: now). The value keeps them
            // too, and every use replays them ([`NodeValue::tgt`]): the
            // first user's attempt may be abandoned (its frame dropped)
            // while the lookup stays done.
            let captured = observations.and_then(crate::observed_fs::Observations::into_captured);
            if let Some(captured) = &captured {
                let nested = with_running(|running| !running.is_empty());
                match &running.key {
                    NodeKey::Tgt(label)
                        if !nested
                            && !self
                                .shared
                                .loading
                                .preloaded
                                .contains_key(&label.package_id()) =>
                    {
                        let package = label.package_id();
                        let observations = crate::observed_fs::Observations::of_captured(captured);
                        self.defer(Box::new(move |coord: &mut Coord<'_>| {
                            coord
                                .store
                                .defer_package_observations(&package, observations);
                        }));
                    }
                    _ => crate::observed_fs::replay(captured),
                }
            }
            if let (
                Some(captured),
                NodeKey::Tgt(label),
                NodeValue::Tgt(Ok(crate::engine::Target::Source(_)), _),
            ) = (&captured, &running.key, &value)
            {
                self.shared
                    .loading
                    .source_observed
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .insert(label.clone(), Arc::clone(captured));
            }
            // Its source, publishable from now on (its set is found).
            if let NodeKey::Tgt(label) = &running.key {
                source_test_sleep();
                let mut pending = self
                    .shared
                    .loading
                    .stable_pending
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                if let Some(at) = pending.iter().position(|(source, _)| source == label) {
                    let source = pending.swap_remove(at);
                    drop(pending);
                    self.shared
                        .loading
                        .stable_sources
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .push(source);
                }
            }
            let value = match value {
                NodeValue::Tgt(result, _) if captured.is_some() => NodeValue::Tgt(result, captured),
                value => value,
            };
            // A lookup's events belong to its user (the walk looks
            // targets up inline).
            for event in running.events {
                if let Err(event) = push_running(event)
                    && !self.pooled()
                {
                    self.coord().push_event(event);
                }
            }
            return value;
        }
        let parts = FinishParts::of(&running.key, &value);
        if self.on_pool() {
            // Read by other bodies at once (no lock).
            if let Some((ct, ct1)) = &parts.ct1 {
                self.ct1_done.first(&ct.key, ct1);
            }
            self.inbox.push(
                running.id,
                Record::Finished(Box::new(Finished {
                    running,
                    parts,
                    observations,
                })),
            );
            return value;
        }
        let mut coord = self.coord();
        coord.drain();
        apply_finish(&mut coord, running, parts, observations);
        value
    }

    fn abandon(&self, attempt: Self::Attempt) {
        drop(attempt.map(crate::observed_fs::UnitScope::finish));
        self.drain_local();
        if let Some(running) = with_running(Vec::pop) {
            let running = running.kept();
            self.inbox.keep_carry(running.id, &running.events);
            if self.on_pool() {
                self.inbox
                    .push(running.id, Record::Abandoned(Box::new(running)));
            } else {
                self.coord().abandon_attempt(running);
            }
        }
    }

    /// The coordinator thread is idle while the pool runs: pass 1 and the
    /// tape as far as the transcripts go (what the sequential pass's next
    /// attempt would apply first), unless another thread holds the
    /// coordinator.
    fn idle(&self) {
        if !self.framed || !*IDLE_LINEARIZE {
            return;
        }
        if super::super::linearize::driver::yields()
            && super::super::linearize::driver::priority_waiting()
        {
            return;
        }
        let Ok(mut coord) = self.coord.try_lock() else {
            return;
        };
        self.inbox.absorb(&mut coord);
        let started = std::time::Instant::now();
        let _ = coord.linearize_step(self.shared, super::super::linearize::driver::LINEARIZE_STEP);
        super::super::timeline::add("idle linearization", started);
    }

    fn pool_mode(&self, on: bool) {
        self.shared
            .loading
            .pooled
            .store(on, std::sync::atomic::Ordering::Release);
    }

    fn target_platform(&self, config: ConfigId) -> NodeKey {
        let label = self
            .shared
            .configurations
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(config)
            .native
            .target_platform()
            .clone();
        NodeKey::Platform(ConfiguredTargetKey::new(label, config))
    }
}

/// What a done attempt leaves in the coordinator's store, computed on its
/// thread ([`Coord::finish_attempt`]).
pub(crate) struct FinishParts {
    memo_ok: bool,
    published: Option<Published>,
    ct1: Option<(CtKey, Arc<Ct1Value>)>,
    source: Option<Label>,
    package: Option<(
        crate::label::PackageId,
        Result<Arc<super::loading::PkgValue>, String>,
    )>,
}

impl FinishParts {
    /// The parts of `value`, the value of `key`'s attempt.
    pub(crate) fn of(key: &NodeKey, value: &NodeValue) -> Self {
        let published = match (key, value) {
            (NodeKey::Platform(key), NodeValue::Platform(result)) => {
                Some(Published::Platform(key.clone(), result.clone()))
            }
            (NodeKey::RegToolchains(config), NodeValue::RegToolchains(result)) => {
                Some(Published::RegToolchains(*config, result.clone()))
            }
            (NodeKey::RegExecPlatforms(config), NodeValue::RegExecPlatforms(result)) => {
                Some(Published::RegExecPlatforms(*config, result.clone()))
            }
            (NodeKey::ExecCfg(config, platform), NodeValue::ExecCfg(Ok(exec))) => {
                Some(Published::ExecCfg(*config, platform.clone(), *exec))
            }
            (NodeKey::FlagScope(label), NodeValue::FlagScope(Ok(scoped))) => {
                Some(Published::FlagScope(label.clone(), *scoped))
            }
            (NodeKey::TransitionDetails(settings), NodeValue::TransitionDetails(result)) => Some(
                Published::TransitionDetails(settings.clone(), result.clone()),
            ),
            (NodeKey::Src(label), NodeValue::Src(Ok(()))) => Some(Published::Source(label.clone())),
            (NodeKey::Transition(key), NodeValue::Transition(Ok(applied))) => {
                Some(Published::Transition(
                    *key,
                    (
                        Arc::clone(&applied.configurations),
                        applied.printed.to_vec(),
                    ),
                ))
            }
            _ => None,
        };
        let mut parts = Self {
            memo_ok: value.error().is_none(),
            published,
            ct1: None,
            source: None,
            package: None,
        };
        match (key, value) {
            (NodeKey::Ct1(ct), NodeValue::Ct1(ct1)) => {
                parts.ct1 = Some((ct.clone(), Arc::clone(ct1)))
            }
            (NodeKey::Src(label), NodeValue::Src(Ok(()))) => parts.source = Some(label.clone()),
            (NodeKey::Pkg(package), NodeValue::Pkg(result)) => {
                parts.package = Some((package.clone(), result.clone()));
            }
            _ => {}
        }
        parts
    }
}

impl Coord<'_> {
    /// An attempt of `id` begins: its key is known (its carried events
    /// are the [`Inbox`]'s).
    pub(crate) fn begin_attempt(&mut self, id: NodeId, key: &NodeKey) {
        self.store.keys.entry(id).or_insert_with(|| key.clone());
        // The abandoned attempt's events stay visible to pass 1 while the
        // rerun runs (on a pool thread nothing else shows them; the rerun
        // re-emits them first): replaced when it finishes or stops.
    }

    /// The running attempt received a `Missing`: it stops here.
    pub(crate) fn missing_here(&mut self) {
        self.drain();
        mark_missing();
    }

    /// The attempt `running` ends with a value whose parts are `parts`:
    /// its transcript is kept.
    pub(crate) fn finish_attempt(&mut self, running: Running, parts: FinishParts) {
        let FinishParts {
            memo_ok,
            published,
            ct1,
            source,
            package,
        } = parts;
        self.store.carry.remove(&running.id);
        self.store.memo_ok.insert(running.id, memo_ok);
        self.store.published.extend(published);
        if let Some((ct, ct1)) = ct1 {
            self.ct1_done.first(&ct.key, &ct1);
            self.ct1_by_node.insert(running.id, Arc::clone(&ct1));
            self.ct1_values.insert(ct, ct1);
        }
        if let Some(label) = source {
            self.sources.insert(label);
        }
        if let Some((package, result)) = package {
            self.store.package_order.push(package.clone());
            self.store.packages.insert(package, result);
        }
        self.store.transcripts.insert(
            running.id,
            Transcript {
                events: running.events,
            },
        );
    }

    /// The attempt `running` stopped (`Missing`; its events were cut to
    /// what stays, [`Running::kept`]): they stay visible to pass 1 until
    /// its rerun replaces them.
    pub(crate) fn abandon_attempt(&mut self, running: Running) {
        if !running.events.is_empty() {
            self.store.carry.insert(running.id, running.events);
        }
    }
}

/// The running attempt on this thread received a `Missing`: what it
/// emits from now on is not the rerun's.
fn mark_missing() {
    with_running(|running| {
        if let Some(running) = running.last_mut() {
            running.missing_at.get_or_insert(running.events.len());
        }
    });
}

impl Running {
    /// What an abandoned attempt keeps: its events up to its `Missing`
    /// (at least the rerun prefix), with what happened anyway.
    fn kept(mut self) -> Self {
        let keep = self
            .missing_at
            .unwrap_or(self.events.len())
            .max(self.replay);
        self.events.truncate(keep);
        self
    }
}

/// A store update a pool attempt hands to the coordinator.
pub(crate) type Deferred<'e> = Box<dyn FnOnce(&mut Coord<'e>) + Send + 'e>;

/// A done pool attempt's record.
struct Finished {
    running: Running,
    parts: FinishParts,
    observations: Option<crate::observed_fs::Observations>,
}

/// What a pool attempt leaves for the coordinator's store (eval-round1:
/// begin, finish and abandon take no lock on the pool).
enum Record<'e> {
    Began(NodeId, NodeKey),
    Finished(Box<Finished>),
    Abandoned(Box<Running>),
    Apply(Deferred<'e>),
}

const INBOX_SHARDS: usize = 64;

/// The pool attempts' records, sharded by node (one node's in its order:
/// its attempts never overlap), applied to the store whenever the
/// coordinator's lock is taken ([`Phase1Bodies::coord`]); and the events
/// abandoned attempts keep for their reruns.
pub(crate) struct Inbox<'e> {
    shards: [Mutex<Vec<Record<'e>>>; INBOX_SHARDS],
    pending: std::sync::atomic::AtomicUsize,
    carry: [Mutex<FxHashMap<NodeId, Vec<Event>>>; INBOX_SHARDS],
}

impl Default for Inbox<'_> {
    fn default() -> Self {
        Self {
            shards: std::array::from_fn(|_| Mutex::new(Vec::new())),
            pending: std::sync::atomic::AtomicUsize::new(0),
            carry: std::array::from_fn(|_| Mutex::new(FxHashMap::default())),
        }
    }
}

fn lock_shard<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

impl<'e> Inbox<'e> {
    fn shard(id: NodeId) -> usize {
        (id.0 as usize).wrapping_mul(0x9E37_79B9) % INBOX_SHARDS
    }

    fn push(&self, id: NodeId, record: Record<'e>) {
        self.pending
            .fetch_add(1, std::sync::atomic::Ordering::AcqRel);
        lock_shard(&self.shards[Self::shard(id)]).push(record);
    }

    /// The events an abandoned attempt of `id` kept (its rerun's prefix).
    fn take_carry(&self, id: NodeId) -> Vec<Event> {
        lock_shard(&self.carry[Self::shard(id)])
            .remove(&id)
            .unwrap_or_default()
    }

    fn keep_carry(&self, id: NodeId, events: &[Event]) {
        let mut carry = lock_shard(&self.carry[Self::shard(id)]);
        if events.is_empty() {
            carry.remove(&id);
        } else {
            carry.insert(id, events.to_vec());
        }
    }

    /// Apply every record to `coord`'s store.
    fn absorb(&self, coord: &mut Coord<'e>) {
        if self.pending.load(std::sync::atomic::Ordering::Acquire) == 0 {
            return;
        }
        for shard in &self.shards {
            let records = std::mem::take(&mut *lock_shard(shard));
            self.pending
                .fetch_sub(records.len(), std::sync::atomic::Ordering::AcqRel);
            for record in records {
                match record {
                    Record::Began(id, key) => coord.begin_attempt(id, &key),
                    Record::Finished(finished) => {
                        let Finished {
                            running,
                            parts,
                            observations,
                        } = *finished;
                        apply_finish(coord, running, parts, observations);
                    }
                    Record::Abandoned(running) => coord.abandon_attempt(*running),
                    Record::Apply(apply) => apply(coord),
                }
            }
        }
    }
}

/// A done attempt's store updates: what it observed and the roots it
/// obtained count now when the tape replayed it already, else where it
/// does; its transcript and value parts are kept.
fn apply_finish(
    coord: &mut Coord<'_>,
    running: Running,
    parts: FinishParts,
    observations: Option<crate::observed_fs::Observations>,
) {
    if coord.store.replayed.contains(&running.id)
        && let Some(roots) = coord.store.roots.remove(&running.id)
    {
        if roots
            .iter()
            .any(|root| root.starts_with(crate::repository::EVAL_GROUP_KEY))
        {
            // An extension group is prepared where the tape replays its
            // user (a Pkg node's first use, after it finished; a lookup in
            // that package uses only the groups the package's load used,
            // committed by it, and records none itself; any other attempt
            // that would need one leaves itself to the sequential pass):
            // here, on whichever thread applies the record, it would be
            // too late and maybe without demand. Fail closed.
            crate::repository::note_eval_adoption_mismatch(|| {
                format!(
                    "{:?} obtained an extension group's repository after the tape replayed it",
                    running.key
                )
            });
        } else {
            let writes =
                super::super::linearize::driver::commit_roots(&coord.engine.repositories, &roots);
            // A pool thread applies the records of nodes the tape has not
            // replayed: their roots' commits write at the replay instead.
            debug_assert!(
                !POOL_THREAD.with(std::cell::Cell::get) || writes.is_empty(),
                "internal error: a pool thread committed roots that write ({:?})",
                running.key
            );
            crate::ui::replay(writes);
        }
    }
    if let Some(observations) = observations.filter(|observations| !observations.is_empty()) {
        if coord.store.replayed.contains(&running.id) {
            crate::observed_fs::commit_to_recorder(observations);
        } else {
            coord.store.observations.insert(running.id, observations);
        }
    }
    coord.finish_attempt(running, parts);
}
