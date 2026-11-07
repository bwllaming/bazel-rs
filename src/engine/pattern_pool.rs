//! The work queue of a pattern's package loads (P2; docs/COLD_ANALYSIS.md,
//! "Pattern loading: blocked units re-run on the workers").
//!
//! Loading workers evaluate the units in no-demand mode; the coordinator
//! commits them in pattern order. A unit aborted by the no-demand sentinel
//! names the demand it was blocked on (a module root, an extension group,
//! a repository). The coordinator satisfies those demands itself, at their
//! sequential points: the first blocked unit of a demand re-runs inline at
//! its commit slot. Every later unit blocked on a demand satisfied by then
//! is queued again on the workers ([`PatternPool::retry_ready`]) instead of
//! re-running inline at its own slot, one after another: the workers re-run
//! them while the coordinator commits, exactly as phase-1 prefetch re-runs
//! its aborted units (crate::prefetch's `retry`). A re-run aborted again on
//! another demand waits for that one; one that never gets ready re-runs
//! inline at its slot as before. The commits — and so every print, error
//! and recording — stay in pattern order.

use std::collections::{HashMap, VecDeque};
use std::sync::{Condvar, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

/// How often one unit is queued again (a re-run blocked once more on a
/// demand satisfied in the meantime goes back; past this it re-runs
/// inline at its slot).
pub(super) const MAX_RETRIES: u8 = 8;

/// How long the coordinator waits for a unit before it looks for blocked
/// units again (a speculation may have published a root meanwhile).
const POLL: Duration = Duration::from_millis(2);

const UNFETCHED: &str = " is unfetched";

/// The blocking demand a no-demand sentinel names (`runtime N:KEY` or
/// `repository @@NAME`): the text `RepositorySet::sentinel_ready` decides
/// on, without the context a load may have wrapped around it. `None` for
/// an order-dependent decision or a wait-cycle abort, which only the
/// coordinator's inline re-run settles.
pub(super) fn blocker(error: &str) -> Option<&str> {
    if crate::repository_demand::is_parallel_bzl_cycle(error) {
        return None;
    }
    let start = error.find(crate::repository_demand::NEEDS_REPOSITORY)?;
    let rest =
        error[start + crate::repository_demand::NEEDS_REPOSITORY.len()..].strip_prefix(": ")?;
    let (subject, _) = rest.split_once(UNFETCHED)?;
    Some(
        &error[start
            ..start
                + crate::repository_demand::NEEDS_REPOSITORY.len()
                + 2
                + subject.len()
                + UNFETCHED.len()],
    )
}

#[derive(Default)]
struct Counts {
    retried: u64,
    withdrawn: u64,
    waited: u64,
    wait_micros: u64,
}

struct State<T> {
    queue: VecDeque<usize>,
    /// A finished attempt of each unit not taken yet.
    results: Vec<Option<T>>,
    /// The aborted attempts of each unit that were queued again (their
    /// print events are the coordinator's to replay first).
    aborted: Vec<Vec<T>>,
    retries: Vec<u8>,
    /// Units a worker is evaluating now.
    running: Vec<bool>,
    /// Finished units aborted on a demand, by that demand.
    blocked: HashMap<String, Vec<usize>>,
    /// The batches of units queued again in waves ([`Batch`]).
    batches: Vec<Batch>,
    /// The batch of each unit of a wave running now.
    wave_of: HashMap<usize, usize>,
    /// Units below this one are taken (never queued again).
    taken: usize,
    shutdown: bool,
    panicked: bool,
    counts: Counts,
}

/// The units of one demand queued again together, in waves of 1, 2, 4, … units in pattern order, each wave once the previous
/// one finished: units blocked on one demand mostly load the same files
/// (an extension's hub and the thousands of repositories it loads), and a
/// later wave finds them in the shared `.bzl` cache instead of every unit
/// walking the same graph at once.
struct Batch {
    waiting: VecDeque<usize>,
    running: usize,
    wave: usize,
}

/// Queue the next wave of `batch` (whether any unit was queued).
fn release_wave<T>(state: &mut State<T>, batch: usize) -> bool {
    let wave = &mut state.batches[batch];
    let size = wave.wave.min(wave.waiting.len());
    wave.wave = wave.wave.saturating_mul(2);
    wave.running = size;
    let released: Vec<usize> = wave.waiting.drain(..size).collect();
    for &index in &released {
        state.wave_of.insert(index, batch);
        state.queue.push_back(index);
    }
    !released.is_empty()
}

/// Take `index` back from the queue or its batch's waiting units (the
/// coordinator reached it first); whether a next wave was queued.
fn withdraw<T>(state: &mut State<T>, index: usize) -> bool {
    state.queue.retain(|queued| *queued != index);
    for batch in &mut state.batches {
        batch.waiting.retain(|waiting| *waiting != index);
    }
    match state.wave_of.remove(&index) {
        Some(batch) => {
            state.batches[batch].running -= 1;
            state.batches[batch].running == 0 && release_wave(state, batch)
        }
        None => false,
    }
}

/// The shared queue of one pattern's loading workers and its coordinator.
pub(super) struct PatternPool<T> {
    state: Mutex<State<T>>,
    work: Condvar,
    done: Condvar,
}

impl<T> PatternPool<T> {
    /// A pool with units `0..units` queued in order.
    pub(super) fn new(units: usize) -> Self {
        Self {
            state: Mutex::new(State {
                queue: (0..units).collect(),
                results: (0..units).map(|_| None).collect(),
                aborted: (0..units).map(|_| Vec::new()).collect(),
                retries: vec![0; units],
                running: vec![false; units],
                blocked: HashMap::new(),
                batches: Vec::new(),
                wave_of: HashMap::new(),
                taken: 0,
                shutdown: false,
                panicked: false,
                counts: Counts::default(),
            }),
            work: Condvar::new(),
            done: Condvar::new(),
        }
    }

    fn lock(&self) -> MutexGuard<'_, State<T>> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// A worker's next unit; `None` once the pool shuts down.
    pub(super) fn next(&self) -> Option<usize> {
        let mut state = self.lock();
        loop {
            if state.shutdown {
                return None;
            }
            if let Some(index) = state.queue.pop_front() {
                state.running[index] = true;
                return Some(index);
            }
            state = self
                .work
                .wait(state)
                .unwrap_or_else(PoisonError::into_inner);
        }
    }

    /// A worker finished `index`; `blocked` is the demand that aborted it.
    pub(super) fn finish(&self, index: usize, unit: T, blocked: Option<&str>) {
        let mut state = self.lock();
        if let Some(blocked) = blocked
            && index >= state.taken
            && state.retries[index] < MAX_RETRIES
        {
            state
                .blocked
                .entry(blocked.to_owned())
                .or_default()
                .push(index);
        }
        state.results[index] = Some(unit);
        state.running[index] = false;
        let queued = match state.wave_of.remove(&index) {
            Some(batch) => {
                state.batches[batch].running -= 1;
                state.batches[batch].running == 0 && release_wave(&mut *state, batch)
            }
            None => false,
        };
        drop(state);
        if queued {
            self.work.notify_all();
        }
        self.done.notify_all();
    }

    /// Queue again every finished unit past the ones taken whose blocking
    /// demand `ready` reports satisfied now.
    pub(super) fn retry_ready(&self, ready: impl Fn(&str) -> bool) {
        let mut state = self.lock();
        let state = &mut *state;
        if state.shutdown || state.blocked.is_empty() {
            return;
        }
        let satisfied: Vec<String> = state
            .blocked
            .keys()
            .filter(|blocked| ready(blocked))
            .cloned()
            .collect();
        // One batch per demand: its units mostly load the same files.
        let mut queued = false;
        for blocked in satisfied {
            let Some(mut indexes) = state.blocked.remove(&blocked) else {
                continue;
            };
            indexes.sort_unstable();
            let mut batch = Batch {
                waiting: VecDeque::new(),
                running: 0,
                wave: 1,
            };
            for index in indexes {
                if index < state.taken {
                    continue;
                }
                let Some(unit) = state.results[index].take() else {
                    continue;
                };
                state.aborted[index].push(unit);
                state.retries[index] += 1;
                state.counts.retried += 1;
                batch.waiting.push_back(index);
            }
            if batch.waiting.is_empty() {
                continue;
            }
            state.batches.push(batch);
            queued |= release_wave(state, state.batches.len() - 1);
        }
        if queued {
            self.work.notify_all();
        }
    }

    /// The coordinator takes unit `index` (in order): its aborted attempts
    /// and its last attempt, waiting for a worker still on it. Blocked
    /// units whose demand got ready meanwhile are queued again while it
    /// waits.
    pub(super) fn take(
        &self,
        index: usize,
        ready: impl Fn(&str) -> bool,
    ) -> Result<(Vec<T>, T), String> {
        let started = std::time::Instant::now();
        let mut waited = false;
        loop {
            {
                let mut state = self.lock();
                state.taken = state.taken.max(index + 1);
                if state.panicked {
                    return Err("parallel package loading worker panicked".to_owned());
                }
                if let Some(unit) = state.results[index].take() {
                    if waited {
                        state.counts.waited += 1;
                        state.counts.wait_micros += started.elapsed().as_micros() as u64;
                    }
                    let aborted = std::mem::take(&mut state.aborted[index]);
                    return Ok((aborted, unit));
                }
                // Queued again but not running yet (behind other units,
                // or in a later wave of its batch): the coordinator does
                // not wait for it — its last aborted attempt goes to the
                // inline re-run at its slot, as before the re-runs.
                if !state.running[index]
                    && let Some(unit) = state.aborted[index].pop()
                {
                    let queued = withdraw(&mut state, index);
                    let aborted = std::mem::take(&mut state.aborted[index]);
                    state.counts.withdrawn += 1;
                    drop(state);
                    if queued {
                        self.work.notify_all();
                    }
                    return Ok((aborted, unit));
                }
                waited = true;
                let (state, _) = self
                    .done
                    .wait_timeout(state, POLL)
                    .unwrap_or_else(PoisonError::into_inner);
                if state.results[index].is_some() || state.panicked {
                    continue;
                }
            }
            self.retry_ready(&ready);
        }
    }

    /// Stop the workers (each finishes the unit it is on).
    pub(super) fn shutdown(&self) {
        let mut state = self.lock();
        state.shutdown = true;
        state.queue.clear();
        drop(state);
        self.work.notify_all();
        self.done.notify_all();
    }

    /// A worker unwound: the coordinator stops waiting.
    pub(super) fn note_panic(&self) {
        let mut state = self.lock();
        state.panicked = true;
        drop(state);
        self.done.notify_all();
    }

    /// `(retried, withdrawn, waited, wait seconds)` for the timing report.
    pub(super) fn counts(&self) -> (u64, u64, u64, f64) {
        let state = self.lock();
        (
            state.counts.retried,
            state.counts.withdrawn,
            state.counts.waited,
            state.counts.wait_micros as f64 / 1e6,
        )
    }
}

/// Shuts the pool down when the coordinator leaves (an error included), so
/// the scoped workers exit and the scope can join them.
pub(super) struct ShutdownOnDrop<'a, T>(pub(super) &'a PatternPool<T>);

impl<T> Drop for ShutdownOnDrop<'_, T> {
    fn drop(&mut self) {
        self.0.shutdown();
    }
}

/// Notes a worker's unwinding in the pool (the coordinator would otherwise
/// wait for the unit forever).
pub(super) struct PanicNote<'a, T>(pub(super) &'a PatternPool<T>);

impl<T> Drop for PanicNote<'_, T> {
    fn drop(&mut self) {
        if std::thread::panicking() {
            self.0.note_panic();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, Ordering};

    fn sentinel(key: &str) -> String {
        format!(
            "error loading package: {}: runtime 4:{key} is unfetched and this thread does not fetch",
            crate::repository_demand::NEEDS_REPOSITORY
        )
    }

    #[test]
    fn blocker_strips_the_context_around_the_demand() {
        let wrapped = format!("in //a: {}", sentinel("prepare:ordinary:x"));
        let bare = sentinel("prepare:ordinary:x");
        assert_eq!(blocker(&wrapped), blocker(&bare));
        assert_eq!(
            blocker(&bare),
            Some(
                format!(
                    "{}: runtime 4:prepare:ordinary:x is unfetched",
                    crate::repository_demand::NEEDS_REPOSITORY
                )
                .as_str()
            )
        );
        // What sentinel_ready parses: the key is a sentinel of its own.
        let key = blocker(&bare).unwrap();
        assert_eq!(blocker(key), Some(key));
        let module = format!(
            "{}: repository @@rules_go+ is unfetched and this thread does not fetch",
            crate::repository_demand::NEEDS_REPOSITORY
        );
        assert!(
            blocker(&module)
                .unwrap()
                .ends_with("repository @@rules_go+ is unfetched")
        );
        assert_eq!(
            blocker(&crate::repository_demand::order_dependent("a link")),
            None
        );
        let cycle = format!("{}: //a", crate::repository_demand::PARALLEL_BZL_CYCLE);
        assert_eq!(blocker(&cycle), None);
        assert_eq!(blocker("plain error"), None);
    }

    /// A unit blocked on a demand goes back to the queue once the demand is
    /// ready, with its aborted attempt kept; units of other demands stay.
    #[test]
    fn ready_demands_requeue_their_units_in_order() {
        let pool = PatternPool::new(4);
        for _ in 0..4 {
            assert!(pool.next().is_some());
        }
        pool.finish(0, "ok0", None);
        pool.finish(3, "x3", Some("X"));
        pool.finish(1, "x1", Some("X"));
        pool.finish(2, "y2", Some("Y"));
        let (aborted, unit) = pool.take(0, |_| false).unwrap();
        assert!(aborted.is_empty());
        assert_eq!(unit, "ok0");
        pool.retry_ready(|blocked| blocked == "X");
        // Unit 1 leads; unit 3 follows once it finished.
        assert_eq!(pool.next(), Some(1));
        pool.finish(1, "ok1", None);
        assert_eq!(pool.next(), Some(3));
        // Blocked on another demand this time.
        pool.finish(3, "z3", Some("Z"));
        let (aborted, unit) = pool.take(1, |_| false).unwrap();
        assert_eq!((aborted, unit), (vec!["x1"], "ok1"));
        // Y never gets ready: unit 2's own attempt is taken (inline re-run).
        let (aborted, unit) = pool.take(2, |_| false).unwrap();
        assert_eq!((aborted, unit), (vec![], "y2"));
        let (aborted, unit) = pool.take(3, |_| false).unwrap();
        assert_eq!((aborted, unit), (vec!["x3"], "z3"));
        assert_eq!(pool.counts().0, 2);
    }

    /// The units of one demand run in waves of 1, 2, 4, … in order.
    #[test]
    fn batches_run_in_doubling_waves() {
        let pool = PatternPool::new(6);
        for _ in 0..6 {
            assert!(pool.next().is_some());
        }
        for index in 0..6 {
            pool.finish(index, index, Some("X"));
        }
        let (_, unit) = pool.take(0, |_| false).unwrap();
        assert_eq!(unit, 0);
        pool.retry_ready(|_| true);
        // Units 1..=5: wave [1], then [2, 3], then [4, 5].
        assert_eq!(pool.next(), Some(1));
        pool.finish(1, 11, None);
        assert_eq!(pool.next(), Some(2));
        assert_eq!(pool.next(), Some(3));
        pool.finish(3, 13, None);
        pool.finish(2, 12, None);
        assert_eq!(pool.next(), Some(4));
        assert_eq!(pool.next(), Some(5));
        pool.finish(4, 14, None);
        pool.finish(5, 15, None);
        for index in 1..6 {
            let (aborted, unit) = pool.take(index, |_| false).unwrap();
            assert_eq!((aborted, unit), (vec![index], index + 10));
        }
    }

    /// Units of two demands ready at once form two batches, each starting
    /// with its first unit.
    #[test]
    fn each_demand_is_its_own_batch() {
        let pool = PatternPool::new(5);
        for _ in 0..5 {
            assert!(pool.next().is_some());
        }
        pool.finish(0, 0, None);
        for index in 1..5 {
            pool.finish(index, index, Some(if index % 2 == 0 { "X" } else { "Y" }));
        }
        let (_, unit) = pool.take(0, |_| false).unwrap();
        assert_eq!(unit, 0);
        pool.retry_ready(|_| true);
        let mut first = vec![pool.next().unwrap(), pool.next().unwrap()];
        first.sort_unstable();
        assert_eq!(first, vec![1, 2]);
        pool.finish(2, 12, None);
        assert_eq!(pool.next(), Some(4));
        pool.finish(1, 11, None);
        assert_eq!(pool.next(), Some(3));
    }

    /// The coordinator reaching a unit still waiting in a later wave (or
    /// queued, not running) takes it back with its last aborted attempt
    /// (the inline re-run) instead of waiting; the wave accounting goes on.
    #[test]
    fn a_waiting_unit_the_coordinator_reaches_is_withdrawn() {
        let pool = PatternPool::new(4);
        for _ in 0..4 {
            assert!(pool.next().is_some());
        }
        pool.finish(0, 0, None);
        for index in 1..4 {
            pool.finish(index, index, Some("X"));
        }
        let (_, unit) = pool.take(0, |_| false).unwrap();
        assert_eq!(unit, 0);
        pool.retry_ready(|_| true);
        // Wave [1] is queued, [2, 3] wait; nothing runs yet.
        let (aborted, unit) = pool.take(1, |_| false).unwrap();
        assert_eq!((aborted, unit), (vec![], 1));
        // Withdrawing the wave's only unit released the next wave.
        assert_eq!(pool.next(), Some(2));
        let (aborted, unit) = pool.take(3, |_| false).unwrap();
        assert_eq!((aborted, unit), (vec![], 3));
        pool.finish(2, 12, None);
        let (aborted, unit) = pool.take(2, |_| false).unwrap();
        assert_eq!((aborted, unit), (vec![2], 12));
        pool.shutdown();
        assert_eq!(pool.next(), None);
        assert_eq!(pool.counts().1, 2);
    }

    /// A unit already taken is never queued again, whatever gets ready.
    #[test]
    fn taken_units_are_never_requeued() {
        let pool = PatternPool::new(2);
        assert_eq!(pool.next(), Some(0));
        assert_eq!(pool.next(), Some(1));
        pool.finish(0, "x0", Some("X"));
        pool.finish(1, "x1", Some("X"));
        let (_, unit) = pool.take(0, |_| false).unwrap();
        assert_eq!(unit, "x0");
        pool.retry_ready(|_| true);
        assert_eq!(pool.counts().0, 1);
        pool.shutdown();
        // Only unit 1 was queued; the shut-down pool hands out nothing.
        assert_eq!(pool.next(), None);
    }

    /// A unit is queued again at most MAX_RETRIES times.
    #[test]
    fn retries_are_bounded() {
        let pool = PatternPool::new(1);
        assert_eq!(pool.next(), Some(0));
        pool.finish(0, 0u8, Some("X"));
        for attempt in 1..=MAX_RETRIES {
            pool.retry_ready(|_| true);
            assert_eq!(pool.next(), Some(0));
            pool.finish(0, attempt, Some("X"));
        }
        pool.retry_ready(|_| true);
        let (aborted, unit) = pool.take(0, |_| true).unwrap();
        assert_eq!(aborted.len(), MAX_RETRIES as usize);
        assert_eq!(unit, MAX_RETRIES);
    }

    /// The coordinator waiting for a unit queues ready units meanwhile, and
    /// a worker that unwinds stops its wait.
    #[test]
    fn waiting_coordinator_retries_and_sees_panics() {
        let pool = PatternPool::<u8>::new(2);
        let ready = AtomicBool::new(false);
        std::thread::scope(|scope| {
            assert_eq!(pool.next(), Some(0));
            assert_eq!(pool.next(), Some(1));
            pool.finish(1, 1, Some("X"));
            let worker = scope.spawn(|| {
                ready.store(true, Ordering::SeqCst);
                // Unit 1 comes back once the coordinator saw X ready.
                assert_eq!(pool.next(), Some(1));
                pool.finish(1, 2, None);
                pool.finish(0, 0, None);
            });
            let (_, unit) = pool.take(0, |_| ready.load(Ordering::SeqCst)).unwrap();
            assert_eq!(unit, 0);
            worker.join().unwrap();
            let (aborted, unit) = pool.take(1, |_| false).unwrap();
            assert_eq!((aborted, unit), (vec![1], 2));
        });
        let pool = PatternPool::<u8>::new(1);
        std::thread::scope(|scope| {
            let _ = scope
                .spawn(|| {
                    let _note = PanicNote(&pool);
                    panic!("worker");
                })
                .join();
        });
        assert!(pool.take(0, |_| false).is_err());
    }
}
