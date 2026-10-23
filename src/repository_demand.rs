//! Invocation-local, nonblocking memoization for repository demand.
//!
//! Each repository set owns fresh slots: successful values and failures are
//! cached only in that instance, never on disk. Use distinct slots and keys for
//! `prepare:<group>` and `repository:<canonical>` work. Runtime IDs are supplied
//! by the caller and must uniquely identify repository-set runtimes.

use std::cell::RefCell;
use std::sync::{Condvar, Mutex, OnceLock};
use std::thread::{self, ThreadId};
use std::time::{Duration, Instant};

const MAX_DEMAND_DEPTH: usize = 128;

thread_local! {
    static DEMAND_STACK: RefCell<Vec<(u64, String)>> = const { RefCell::new(Vec::new()) };
}

/// Marker of the no-demand guard's sentinel error (review §1.2): a thread
/// in no-demand mode asking for an unfetched repository gets this error
/// instead of fetching. Workers run in that mode; the coordinator re-runs
/// the aborted unit inline at its commit slot, where fetching is allowed.
pub const NEEDS_REPOSITORY: &str = "\u{1}needs-repository";

/// Whether `error` is the no-demand sentinel ([`NEEDS_REPOSITORY`]).
pub fn is_needs_repository(error: &str) -> bool {
    error.contains(NEEDS_REPOSITORY)
}

/// Marker of the parallel `.bzl` wait-cycle abort (N62): a worker whose
/// in-flight wait for a label would close a cycle across threads aborts
/// its unit instead of deadlocking; the coordinator re-runs the unit
/// inline, so a genuine load cycle reports with the sequential
/// traversal's message. Never published to the shared `.bzl` cache.
pub const PARALLEL_BZL_CYCLE: &str = "\u{1}parallel-bzl-cycle";

/// Whether `error` is the parallel wait-cycle abort
/// ([`PARALLEL_BZL_CYCLE`]).
pub fn is_parallel_bzl_cycle(error: &str) -> bool {
    error.contains(PARALLEL_BZL_CYCLE)
}

/// The no-demand sentinel for a decision that depends on which
/// repositories the invocation has materialized so far — a symlink a glob
/// or a `.bzl` load follows into another repository's root (or that the
/// link policy refuses): the sequential engine takes it at its own demand
/// point, so a worker (no-demand mode) aborts its unit instead and the
/// coordinator re-runs it inline there (W6 phase-1 prefetch; P2).
pub fn order_dependent(what: &str) -> String {
    format!(
        "{NEEDS_REPOSITORY}: {what} depends on the repositories materialized so far and this thread does not decide it"
    )
}

/// Whether either inline-rerun sentinel aborted a load unit: the
/// coordinator re-runs the unit inline at its commit slot (fetching
/// allowed, waits uncontended) and reproduces the sequential output.
pub fn is_inline_rerun_sentinel(error: &str) -> bool {
    is_needs_repository(error) || is_parallel_bzl_cycle(error)
}

thread_local! {
    /// Depth of the no-demand guard on this thread (0: fetching allowed).
    static NO_DEMAND: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    /// Set when this no-demand thread took a materialization-order-dependent
    /// link decision ([`note_order_dependent`]).
    static ORDER_DEPENDENT: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Record that a link resolution on this thread depended on the
/// repositories materialized so far (a hop into another repository's root,
/// or a refusal a later materialization could lift); only no-demand
/// threads record it ([`take_order_dependent`], [`order_dependent`]).
pub fn note_order_dependent() {
    if no_demand_active() {
        ORDER_DEPENDENT.with(|flag| flag.set(true));
    }
}

/// Whether this thread recorded an order-dependent link decision since the
/// last call (clearing the record).
pub fn take_order_dependent() -> bool {
    ORDER_DEPENDENT.with(|flag| flag.replace(false))
}

/// Whether this thread runs in no-demand mode (it must not fetch).
pub fn no_demand_active() -> bool {
    NO_DEMAND.with(std::cell::Cell::get) > 0
}

/// RAII scope in which repository demand never fetches: unfetched
/// repositories fail with the [`NEEDS_REPOSITORY`] sentinel instead.
/// Scopes nest; only the outermost matters.
pub struct NoDemandGuard;

/// Enter no-demand mode on this thread.
pub fn no_demand() -> NoDemandGuard {
    NO_DEMAND.with(|depth| depth.set(depth.get() + 1));
    NoDemandGuard
}

impl Drop for NoDemandGuard {
    fn drop(&mut self) {
        NO_DEMAND.with(|depth| depth.set(depth.get().saturating_sub(1)));
    }
}

thread_local! {
    /// Depth of reuse-only mode on this thread (Step C): a demand whose
    /// warm publication validates may be served on this thread (pure
    /// reads: pointer, manifest, recorded-input check, stat verify);
    /// anything that would execute a rule, evaluate an extension or print
    /// still fails with the sentinel. Implies no-demand for execution.
    static REUSE_ONLY: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// Whether this thread may serve warm repository reuse (but never fetch).
pub fn reuse_only_active() -> bool {
    REUSE_ONLY.with(std::cell::Cell::get) > 0
}

/// RAII scope: reuse-only demand (see [`reuse_only_active`]). Workers enter
/// it inside the no-demand guard; a miss aborts with the sentinel and the
/// coordinator fetches inline at the commit slot, in demand order.
pub struct ReuseOnlyGuard;

pub fn reuse_only() -> ReuseOnlyGuard {
    REUSE_ONLY.with(|depth| depth.set(depth.get() + 1));
    ReuseOnlyGuard
}

impl Drop for ReuseOnlyGuard {
    fn drop(&mut self) {
        REUSE_ONLY.with(|depth| depth.set(depth.get().saturating_sub(1)));
    }
}

thread_local! {
    /// Depth of reuse-only probes running on this thread
    /// ([`DemandSlot::get_or_probe`]). A probe nested in another one (a
    /// recorded input in another repository) never waits for a third
    /// thread's probe, so probe waits cannot form a cycle.
    static PROBE_DEPTH: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// The longest a top-level probe waits for another thread's probe of the
/// same slot before it probes on its own (a safety valve only: probes
/// read, and nested probes never wait).
const PROBE_WAIT: Duration = Duration::from_secs(1);

/// A single demand result, allowing callers to borrow the cached value.
///
/// The evaluator never runs under a lock. Recursive demand fails with a cycle
/// chain; simultaneous demand for the same slot fails instead of waiting.
#[derive(Debug)]
pub struct DemandSlot<T> {
    ready: OnceLock<Result<T, String>>,
    running: Mutex<Option<ThreadId>>,
    /// Whether a top-level reuse-only probe of this slot is in flight
    /// ([`Self::get_or_probe`]): other top-level probers wait for it
    /// (`probed`) instead of repeating the same verification.
    probing: Mutex<bool>,
    probed: Condvar,
}

/// How a top-level prober's claim on a slot's probe ended
/// ([`DemandSlot::claim_probe`]).
enum ProbeClaim {
    /// No probe was in flight: this thread probes (and publishes).
    Owner,
    /// Another thread's probe finished: its publication (if any) serves
    /// this demand; no publication is this demand's miss too.
    Finished,
    /// The wait exceeded [`PROBE_WAIT`]: probe without the claim.
    TimedOut,
}

/// One probe on this thread ([`PROBE_DEPTH`]); releases the slot's probe
/// claim (waking its waiters) when the probe held it, after any
/// publication.
struct ProbeScope<'a, T> {
    slot: &'a DemandSlot<T>,
    claimed: bool,
}

impl<'a, T> ProbeScope<'a, T> {
    fn enter(slot: &'a DemandSlot<T>, claimed: bool) -> Self {
        PROBE_DEPTH.with(|depth| depth.set(depth.get() + 1));
        Self { slot, claimed }
    }
}

impl<T> Drop for ProbeScope<'_, T> {
    fn drop(&mut self) {
        PROBE_DEPTH.with(|depth| depth.set(depth.get().saturating_sub(1)));
        if self.claimed {
            let mut probing = match self.slot.probing.lock() {
                Ok(probing) => probing,
                Err(poisoned) => poisoned.into_inner(),
            };
            *probing = false;
            drop(probing);
            self.slot.probed.notify_all();
        }
    }
}

impl<T> Default for DemandSlot<T> {
    fn default() -> Self {
        Self::new()
    }
}

impl<T> DemandSlot<T> {
    pub fn new() -> Self {
        Self {
            ready: OnceLock::new(),
            running: Mutex::new(None),
            probing: Mutex::new(false),
            probed: Condvar::new(),
        }
    }

    /// Claim this slot's probe, or wait (at most [`PROBE_WAIT`]) for the
    /// probe another thread holds.
    fn claim_probe(&self) -> ProbeClaim {
        let mut probing = match self.probing.lock() {
            Ok(probing) => probing,
            Err(poisoned) => poisoned.into_inner(),
        };
        if !*probing {
            *probing = true;
            return ProbeClaim::Owner;
        }
        let deadline = Instant::now() + PROBE_WAIT;
        while *probing {
            let now = Instant::now();
            if now >= deadline {
                return ProbeClaim::TimedOut;
            }
            probing = match self.probed.wait_timeout(probing, deadline - now) {
                Ok((probing, _)) => probing,
                Err(poisoned) => poisoned.into_inner().0,
            };
        }
        ProbeClaim::Finished
    }

    /// The successfully computed value, if any; never evaluates.
    pub fn ready(&self) -> Option<&T> {
        self.ready.get().and_then(|result| result.as_ref().ok())
    }

    /// Evaluate once, caching either the value or its error in this slot.
    ///
    /// A panicking evaluator releases its claim without caching a result.
    /// Cycle, concurrency, poison, and depth admission errors do not themselves
    /// populate the slot (an enclosing evaluator may cache a propagated error).
    pub fn get_or_try_init(
        &self,
        runtime_id: u64,
        key: &str,
        init: impl FnOnce() -> Result<T, String>,
    ) -> Result<&T, String> {
        if let Some(result) = self.ready.get() {
            return result.as_ref().map_err(Clone::clone);
        }
        if no_demand_active() {
            // Workers never fetch (review §1.2): the sentinel aborts the
            // unit; the coordinator re-runs it inline at its commit slot.
            return Err(format!(
                "{NEEDS_REPOSITORY}: runtime {runtime_id}:{key} is unfetched and this thread does not fetch"
            ));
        }
        self.evaluate(runtime_id, key, init)
    }

    /// Reuse-only evaluation (Step C): under the no-demand guard, try
    /// `probe` first — a warm check (reads plus at most an idempotent,
    /// best-effort manifest refresh, repository_store::try_reuse) that may
    /// produce the value without fetching. On a probe miss the sentinel is returned WITHOUT
    /// populating the slot (the coordinator fetches inline later); on a
    /// probe hit the value is published so every later demand is a ready
    /// hit. Outside the guard (or with a ready value) this is exactly
    /// [`Self::get_or_try_init`]. Concurrency: one top-level probe per
    /// slot runs at a time; another top-level prober waits for it and is
    /// served by its publication (W6: phase-1 prefetch workers fan out over
    /// the packages of one repository at once), and when that probe missed
    /// the waiter misses too. A nested probe (or a wait past
    /// [`PROBE_WAIT`]) probes on its own; the loser of a publish race
    /// reports the winner's value (probes only read, or refresh a manifest
    /// idempotently, so a duplicate probe is harmless).
    pub fn get_or_probe(
        &self,
        runtime_id: u64,
        key: &str,
        probe: impl FnOnce() -> Result<Option<T>, String>,
        init: impl FnOnce() -> Result<T, String>,
    ) -> Result<&T, String> {
        if let Some(result) = self.ready.get() {
            return result.as_ref().map_err(Clone::clone);
        }
        if !no_demand_active() {
            return self.evaluate(runtime_id, key, init);
        }
        if !reuse_only_active() {
            return Err(format!(
                "{NEEDS_REPOSITORY}: runtime {runtime_id}:{key} is unfetched and this thread does not fetch"
            ));
        }
        let claimed = if PROBE_DEPTH.with(std::cell::Cell::get) == 0 {
            match self.claim_probe() {
                ProbeClaim::Owner => true,
                ProbeClaim::Finished => {
                    return match self.ready.get() {
                        Some(result) => result.as_ref().map_err(Clone::clone),
                        None => Err(format!(
                            "{NEEDS_REPOSITORY}: runtime {runtime_id}:{key} is unfetched and this thread does not fetch"
                        )),
                    };
                }
                ProbeClaim::TimedOut => false,
            }
        } else {
            false
        };
        // Released (waking waiters) after the publication below.
        let _scope = ProbeScope::enter(self, claimed);
        if let Some(result) = self.ready.get() {
            return result.as_ref().map_err(Clone::clone);
        }
        // A probe ERROR maps to a miss, never to the unit's error (review
        // R2): sequential materialize treats verification errors as
        // mismatch + re-execute, so the coordinator's sequential path must
        // own every error. The probe is a pure optimization.
        match probe().unwrap_or(None) {
            Some(value) => {
                // Publish for all later demands; a concurrent publisher
                // (another worker probing the same repo, or the
                // coordinator fetching) wins and we serve its value. The
                // publication happens under the evaluator claim lock and
                // only while no evaluator runs: the coordinator may
                // evaluate this slot while a worker probes it (phase-1
                // prefetch runs workers beside the coordinator,
                // crate::prefetch), and its own publication must never
                // find the slot occupied. A probe that loses to a running
                // evaluator is a miss: the evaluator's result stands.
                let running = match self.running.lock() {
                    Ok(running) => running,
                    Err(poisoned) => poisoned.into_inner(),
                };
                if running.is_some() && self.ready.get().is_none() {
                    drop(running);
                    return Err(format!(
                        "{NEEDS_REPOSITORY}: runtime {runtime_id}:{key} is unfetched and this thread does not fetch"
                    ));
                }
                let _ = self.ready.set(Ok(value));
                drop(running);
                self.ready
                    .get()
                    .expect("just published or concurrently published")
                    .as_ref()
                    .map_err(Clone::clone)
            }
            None => Err(format!(
                "{NEEDS_REPOSITORY}: runtime {runtime_id}:{key} is unfetched and this thread does not fetch"
            )),
        }
    }

    fn evaluate(
        &self,
        runtime_id: u64,
        key: &str,
        init: impl FnOnce() -> Result<T, String>,
    ) -> Result<&T, String> {
        if let Some(result) = self.ready.get() {
            return result.as_ref().map_err(Clone::clone);
        }

        let current_thread = thread::current().id();
        let mut running = self
            .running
            .lock()
            .map_err(|_| format!("repository demand lock poisoned: runtime {runtime_id}:{key}"))?;
        if let Some(result) = self.ready.get() {
            return result.as_ref().map_err(Clone::clone);
        }
        if let Some(owner) = *running {
            drop(running);
            return Err(if owner == current_thread {
                cycle_error(runtime_id, key)
            } else {
                format!("concurrent repository demand is unsupported: runtime {runtime_id}:{key}")
            });
        }
        *running = Some(current_thread);
        drop(running);

        let mut guard = DemandGuard {
            slot: self,
            runtime_id,
            key,
            position: None,
        };
        guard.push()?;

        let result = init();
        let published = self.ready.set(result);
        // Publish before releasing ownership, so no second evaluator can start.
        drop(guard);
        if published.is_err() {
            return Err(format!(
                "internal repository demand error: result already occupied for runtime {runtime_id}:{key}"
            ));
        }
        match self.ready.get() {
            Some(result) => result.as_ref().map_err(Clone::clone),
            None => Err(format!(
                "internal repository demand error: missing result for runtime {runtime_id}:{key}"
            )),
        }
    }
}

fn cycle_error(runtime_id: u64, key: &str) -> String {
    let mut chain = DEMAND_STACK
        .try_with(|stack| {
            stack.try_borrow().ok().map(|stack| {
                stack
                    .iter()
                    .map(|(runtime, key)| format!("runtime {runtime}:{key}"))
                    .collect::<Vec<_>>()
            })
        })
        .ok()
        .flatten()
        .unwrap_or_default();
    chain.push(format!("runtime {runtime_id}:{key}"));
    format!("repository demand cycle: {}", chain.join(" -> "))
}

struct DemandGuard<'a, T> {
    slot: &'a DemandSlot<T>,
    runtime_id: u64,
    key: &'a str,
    position: Option<usize>,
}

impl<T> DemandGuard<'_, T> {
    fn push(&mut self) -> Result<(), String> {
        DEMAND_STACK
            .try_with(|stack| {
                let mut stack = stack.try_borrow_mut().map_err(|_| {
                    "internal repository demand error: demand stack already borrowed".to_owned()
                })?;
                if stack.len() >= MAX_DEMAND_DEPTH {
                    return Err(format!(
                        "repository demand depth limit ({MAX_DEMAND_DEPTH}) exceeded: runtime {}:{}",
                        self.runtime_id, self.key
                    ));
                }
                let position = stack.len();
                stack.push((self.runtime_id, self.key.to_owned()));
                self.position = Some(position);
                Ok(())
            })
            .map_err(|_| "repository demand stack unavailable".to_owned())?
    }
}

impl<T> Drop for DemandGuard<'_, T> {
    fn drop(&mut self) {
        if let Some(position) = self.position {
            // No assertions or panicking borrows during evaluator unwinding.
            let _ = DEMAND_STACK.try_with(|stack| {
                if let Ok(mut stack) = stack.try_borrow_mut() {
                    let matches =
                        |frame: &(u64, String)| frame.0 == self.runtime_id && frame.1 == self.key;
                    let own_position = if stack.get(position).is_some_and(matches) {
                        Some(position)
                    } else {
                        stack.iter().rposition(matches)
                    };
                    if let Some(position) = own_position {
                        stack.remove(position);
                    }
                }
            });
        }
        // Poison recovery is for cleanup only; future demand still reports it.
        let mut running = match self.slot.running.lock() {
            Ok(running) => running,
            Err(poisoned) => poisoned.into_inner(),
        };
        *running = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::panic::{AssertUnwindSafe, catch_unwind};
    use std::rc::Rc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, mpsc};
    use std::time::Duration;

    const TIMEOUT: Duration = Duration::from_secs(3);

    // Always release and join workers, even if an assertion unwinds the test.
    struct Worker<T> {
        release: Option<mpsc::Sender<()>>,
        handle: Option<thread::JoinHandle<T>>,
    }

    impl<T> Worker<T> {
        fn finish(mut self) -> thread::Result<T> {
            if let Some(release) = self.release.take() {
                let _ = release.send(());
            }
            self.handle.take().unwrap().join()
        }
    }

    impl<T> Drop for Worker<T> {
        fn drop(&mut self) {
            if let Some(release) = self.release.take() {
                let _ = release.send(());
            }
            if let Some(handle) = self.handle.take() {
                let _ = handle.join();
            }
        }
    }

    fn blocked_worker(
        slot: Arc<DemandSlot<usize>>,
        key: &'static str,
        value: usize,
    ) -> (Worker<Result<usize, String>>, mpsc::Receiver<()>) {
        let (started_tx, started_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let handle = thread::spawn(move || {
            slot.get_or_try_init(1, key, || {
                started_tx.send(()).map_err(|error| error.to_string())?;
                release_rx
                    .recv_timeout(TIMEOUT)
                    .map_err(|error| format!("release failed: {error}"))?;
                Ok(value)
            })
            .copied()
        });
        (
            Worker {
                release: Some(release_tx),
                handle: Some(handle),
            },
            started_rx,
        )
    }

    #[test]
    fn cached_success_is_called_once_and_borrows_same_address() {
        let slot = DemandSlot::default();
        let calls = AtomicUsize::new(0);
        let first = slot
            .get_or_try_init(1, "repository:a", || {
                calls.fetch_add(1, Ordering::SeqCst);
                Ok(String::from("value"))
            })
            .unwrap();
        let second = slot
            .get_or_try_init(1, "repository:a", || {
                calls.fetch_add(1, Ordering::SeqCst);
                Ok(String::from("wrong"))
            })
            .unwrap();
        assert!(std::ptr::eq(first, second));
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn failures_are_cached_only_in_the_same_instance() {
        let slot = DemandSlot::<usize>::new();
        assert_eq!(
            slot.get_or_try_init(1, "repository:a", || Err("original".into())),
            Err("original".into())
        );
        assert_eq!(
            slot.get_or_try_init(1, "repository:a", || panic!("must be cached")),
            Err("original".into())
        );
        let fresh = DemandSlot::new();
        assert_eq!(fresh.get_or_try_init(2, "repository:a", || Ok(7)), Ok(&7));
    }

    #[test]
    fn same_slot_cycle_reports_key_and_releases_claim() {
        let slot = DemandSlot::<usize>::new();
        let error = slot
            .get_or_try_init(1, "repository:a", || {
                slot.get_or_try_init(1, "repository:a", || Ok(1)).copied()
            })
            .unwrap_err();
        assert!(error.contains("cycle"));
        assert!(error.contains("runtime 1:repository:a -> runtime 1:repository:a"));
        assert!(slot.running.lock().unwrap().is_none());
        DEMAND_STACK.with(|stack| assert!(stack.borrow().is_empty()));
    }

    #[test]
    fn prepare_repository_prepare_cycle_has_full_chain() {
        let group = DemandSlot::<usize>::new();
        let repository = DemandSlot::<usize>::new();
        let error = group
            .get_or_try_init(7, "prepare:group", || {
                repository
                    .get_or_try_init(7, "repository:canonical", || {
                        group.get_or_try_init(7, "prepare:group", || Ok(1)).copied()
                    })
                    .copied()
            })
            .unwrap_err();
        assert_eq!(
            error,
            "repository demand cycle: runtime 7:prepare:group -> runtime 7:repository:canonical -> runtime 7:prepare:group"
        );
    }

    #[test]
    fn actual_slot_recursion_is_detected_even_with_wrong_runtime_and_key() {
        let slot = DemandSlot::<usize>::new();
        let error = slot
            .get_or_try_init(1, "prepare:actual", || {
                slot.get_or_try_init(2, "repository:wrong", || Ok(1))
                    .copied()
            })
            .unwrap_err();
        assert!(error.contains("runtime 1:prepare:actual -> runtime 2:repository:wrong"));
    }

    #[test]
    fn identical_keys_in_distinct_runtimes_are_not_cycles() {
        let outer = DemandSlot::<usize>::new();
        let inner = DemandSlot::<usize>::new();
        assert_eq!(
            outer.get_or_try_init(1, "repository:a", || {
                inner.get_or_try_init(2, "repository:a", || Ok(5)).copied()
            }),
            Ok(&5)
        );
    }

    #[test]
    fn panic_clears_guard_and_allows_retry() {
        let slot = DemandSlot::<usize>::new();
        let panic = catch_unwind(AssertUnwindSafe(|| {
            let _ = slot.get_or_try_init(1, "repository:a", || panic!("evaluator panic"));
        }));
        assert!(panic.is_err());
        assert!(slot.running.lock().unwrap().is_none());
        assert!(slot.ready.get().is_none());
        DEMAND_STACK.with(|stack| assert!(stack.borrow().is_empty()));
        assert_eq!(slot.get_or_try_init(1, "repository:a", || Ok(9)), Ok(&9));
    }

    #[test]
    fn simultaneous_same_slot_demand_rejects_without_evaluation_or_waiting() {
        let slot = Arc::new(DemandSlot::new());
        let (worker, started) = blocked_worker(Arc::clone(&slot), "repository:a", 17);
        let started_result = started.recv_timeout(TIMEOUT);
        let calls = AtomicUsize::new(0);
        let contender = if started_result.is_ok() {
            slot.get_or_try_init(1, "repository:a", || {
                calls.fetch_add(1, Ordering::SeqCst);
                Ok(99)
            })
            .copied()
        } else {
            Err("worker never started".to_owned())
        };
        // Release only after the contender returns. Waiting implementations
        // time out the first initializer and fail, rather than stall the test.
        let owner = worker.finish();
        assert!(started_result.is_ok());
        assert!(
            contender
                .unwrap_err()
                .contains("concurrent repository demand is unsupported")
        );
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        assert_eq!(owner.unwrap(), Ok(17));
        assert_eq!(slot.get_or_try_init(1, "repository:a", || Ok(99)), Ok(&17));
    }

    #[test]
    fn independent_slots_can_evaluate_in_parallel() {
        let (first, first_started) =
            blocked_worker(Arc::new(DemandSlot::new()), "prepare:group", 3);
        let (second, second_started) =
            blocked_worker(Arc::new(DemandSlot::new()), "repository:a", 4);
        let first_ready = first_started.recv_timeout(TIMEOUT);
        let second_ready = second_started.recv_timeout(TIMEOUT);
        let first_result = first.finish();
        let second_result = second.finish();
        assert!(first_ready.is_ok());
        assert!(second_ready.is_ok());
        assert_eq!(first_result.unwrap(), Ok(3));
        assert_eq!(second_result.unwrap(), Ok(4));
    }

    #[test]
    fn demand_depth_is_bounded_before_calling_the_next_evaluator() {
        fn demand(
            slots: &[DemandSlot<usize>],
            index: usize,
            calls: &AtomicUsize,
        ) -> Result<usize, String> {
            slots[index]
                .get_or_try_init(1, &format!("repository:{index}"), || {
                    calls.fetch_add(1, Ordering::SeqCst);
                    if index + 1 == slots.len() {
                        Ok(index)
                    } else {
                        demand(slots, index + 1, calls)
                    }
                })
                .copied()
        }
        let slots: Vec<_> = (0..=MAX_DEMAND_DEPTH).map(|_| DemandSlot::new()).collect();
        let calls = AtomicUsize::new(0);
        let error = demand(&slots, 0, &calls).unwrap_err();
        assert!(error.contains("depth limit (128) exceeded"));
        assert_eq!(calls.load(Ordering::SeqCst), MAX_DEMAND_DEPTH);
        assert!(slots[MAX_DEMAND_DEPTH].ready.get().is_none());
        assert!(
            slots
                .iter()
                .all(|slot| slot.running.lock().unwrap().is_none())
        );
        DEMAND_STACK.with(|stack| assert!(stack.borrow().is_empty()));
        assert_eq!(
            slots[MAX_DEMAND_DEPTH].get_or_try_init(1, "repository:retry", || Ok(8)),
            Ok(&8)
        );
    }

    #[test]
    fn poisoned_lock_returns_an_explicit_error() {
        let slot = DemandSlot::<usize>::new();
        let panic = catch_unwind(AssertUnwindSafe(|| {
            let _lock = slot.running.lock().unwrap();
            panic!("inject poison");
        }));
        assert!(panic.is_err());
        let error = slot
            .get_or_try_init(1, "repository:a", || panic!("must not evaluate"))
            .unwrap_err();
        assert!(error.contains("repository demand lock poisoned"));
    }

    #[test]
    fn guard_cleanup_recovers_poison_without_panicking() {
        let slot = DemandSlot::<usize>::new();
        let panic = catch_unwind(AssertUnwindSafe(|| {
            let _ = slot.get_or_try_init(1, "repository:a", || {
                let _lock = slot.running.lock().unwrap();
                panic!("inject poison during evaluation");
            });
        }));
        assert!(panic.is_err());
        assert!(slot.running.lock().unwrap_err().into_inner().is_none());
        DEMAND_STACK.with(|stack| assert!(stack.borrow().is_empty()));
    }

    #[test]
    fn slots_support_borrowed_and_non_send_values() {
        let value = String::from("borrowed");
        let slot = DemandSlot::new();
        assert_eq!(
            slot.get_or_try_init(1, "repository:a", || Ok(value.as_str())),
            Ok(&"borrowed")
        );
        let local = DemandSlot::new();
        assert_eq!(
            **local
                .get_or_try_init(1, "repository:b", || Ok(Rc::new(6)))
                .unwrap(),
            6
        );
    }

    #[test]
    fn slots_are_send_and_sync_when_their_values_are() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<DemandSlot<String>>();
        assert_send_sync::<DemandSlot<&str>>();
    }

    /// The no-demand guard (review §1.2): a thread that must not fetch
    /// gets the NeedsRepository sentinel instead of running the
    /// initializer, without populating the slot; a ready value is served
    /// as usual, and a later unguarded demand evaluates normally.
    /// Review R2: a probe ERROR is a miss, not the unit's error — the
    /// sentinel comes back, the slot stays empty, and the coordinator's
    /// sequential path owns the real error later.
    #[test]
    fn probe_error_maps_to_sentinel_without_populating() {
        let slot = DemandSlot::new();
        let _guard = no_demand();
        let _reuse = reuse_only();
        let error = slot
            .get_or_probe(
                7,
                "repository:x",
                || Err::<Option<usize>, String>("manifest unreadable".into()),
                || Ok(1),
            )
            .unwrap_err();
        assert!(is_needs_repository(&error), "{error}");
        assert!(slot.ready().is_none());
        // Probe miss behaves identically.
        let error = slot
            .get_or_probe(7, "repository:x", || Ok(None), || Ok(1))
            .unwrap_err();
        assert!(is_needs_repository(&error));
        assert!(slot.ready().is_none());
        // Probe hit publishes for later demands.
        assert_eq!(
            slot.get_or_probe(7, "repository:x", || Ok(Some(5)), || Ok(1)),
            Ok(&5)
        );
        assert_eq!(slot.ready(), Some(&5));
    }

    /// A top-level prober spawned on another thread; its probe signals
    /// `started` and returns `outcome` after `hold`.
    fn holding_prober(
        slot: Arc<DemandSlot<usize>>,
        outcome: Option<usize>,
        hold: Duration,
    ) -> (
        thread::JoinHandle<Result<usize, String>>,
        mpsc::Receiver<()>,
    ) {
        let (started_tx, started_rx) = mpsc::channel();
        let handle = thread::spawn(move || {
            let _guard = no_demand();
            let _reuse = reuse_only();
            slot.get_or_probe(
                7,
                "repository:x",
                || {
                    let _ = started_tx.send(());
                    thread::sleep(hold);
                    Ok(outcome)
                },
                || Ok(1),
            )
            .copied()
        });
        (handle, started_rx)
    }

    /// W6: concurrent top-level probes of one slot run once — the waiter
    /// is served by the in-flight probe's publication without probing.
    #[test]
    fn concurrent_top_level_probe_waits_for_the_in_flight_probe() {
        let slot = Arc::new(DemandSlot::new());
        let (owner, started) =
            holding_prober(Arc::clone(&slot), Some(5), Duration::from_millis(300));
        started.recv_timeout(TIMEOUT).unwrap();
        let calls = AtomicUsize::new(0);
        let waited = {
            let _guard = no_demand();
            let _reuse = reuse_only();
            slot.get_or_probe(
                7,
                "repository:x",
                || {
                    calls.fetch_add(1, Ordering::SeqCst);
                    Ok(Some(9))
                },
                || Ok(1),
            )
            .copied()
        };
        assert_eq!(owner.join().unwrap(), Ok(5));
        assert_eq!(waited, Ok(5));
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        // The claim is released.
        assert!(!*slot.probing.lock().unwrap());
    }

    /// The waiter of a probe that missed misses too, without probing and
    /// without populating the slot; a later prober probes again.
    #[test]
    fn waiter_of_a_missed_probe_misses_without_probing() {
        let slot = Arc::new(DemandSlot::new());
        let (owner, started) = holding_prober(Arc::clone(&slot), None, Duration::from_millis(300));
        started.recv_timeout(TIMEOUT).unwrap();
        let calls = AtomicUsize::new(0);
        let _guard = no_demand();
        let _reuse = reuse_only();
        let waited = slot.get_or_probe(
            7,
            "repository:x",
            || {
                calls.fetch_add(1, Ordering::SeqCst);
                Ok(Some(9))
            },
            || Ok(1),
        );
        assert!(is_needs_repository(&owner.join().unwrap().unwrap_err()));
        assert!(is_needs_repository(&waited.unwrap_err()));
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        assert!(slot.ready().is_none());
        assert_eq!(
            slot.get_or_probe(7, "repository:x", || Ok(Some(9)), || Ok(1)),
            Ok(&9)
        );
    }

    /// A probe nested in another probe never waits for a third thread's
    /// probe (no wait cycles): it probes on its own (a wait would serve the
    /// owner's 5).
    #[test]
    fn nested_probe_does_not_wait_for_an_in_flight_probe() {
        let inner = Arc::new(DemandSlot::new());
        let (owner, started) =
            holding_prober(Arc::clone(&inner), Some(5), Duration::from_millis(600));
        started.recv_timeout(TIMEOUT).unwrap();
        let outer: DemandSlot<usize> = DemandSlot::new();
        let _guard = no_demand();
        let _reuse = reuse_only();
        let start = Instant::now();
        let mut nested = None;
        let result = outer.get_or_probe(
            7,
            "repository:outer",
            || {
                nested = Some(
                    inner
                        .get_or_probe(7, "repository:x", || Ok(Some(9)), || Ok(1))
                        .copied(),
                );
                Ok(Some(3))
            },
            || Ok(1),
        );
        let elapsed = start.elapsed();
        assert_eq!(result, Ok(&3));
        assert_eq!(nested, Some(Ok(9)));
        assert!(elapsed < PROBE_WAIT, "{elapsed:?}");
        // The owner loses the publish race and serves the nested value.
        assert_eq!(owner.join().unwrap(), Ok(9));
        assert_eq!(PROBE_DEPTH.with(std::cell::Cell::get), 0);
    }

    #[test]
    fn no_demand_guard_returns_sentinel_without_populating() {
        let slot = DemandSlot::new();
        assert!(!no_demand_active());
        let _outer = no_demand();
        assert!(no_demand_active());
        let error = slot
            .get_or_try_init(7, "repository:x", || Ok(1))
            .unwrap_err();
        assert!(is_needs_repository(&error), "{error}");
        assert!(error.contains("runtime 7:repository:x"));
        // The sentinel did not populate the slot.
        assert!(slot.ready().is_none());
        drop(_outer);
        assert!(!no_demand_active());
        // With the guard gone, the same demand evaluates normally.
        {
            assert_eq!(slot.get_or_try_init(7, "repository:x", || Ok(2)), Ok(&2));
        }
        // A ready value is served under the guard.
        {
            let _guard = no_demand();
            assert!(no_demand_active());
            assert_eq!(slot.get_or_try_init(7, "repository:x", || Ok(3)), Ok(&2));
        }
    }
}
