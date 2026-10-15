//! Invocation-local, nonblocking memoization for repository demand.
//!
//! Each repository set owns fresh slots: successful values and failures are
//! cached only in that instance, never on disk. Use distinct slots and keys for
//! `prepare:<group>` and `repository:<canonical>` work. Runtime IDs are supplied
//! by the caller and must uniquely identify repository-set runtimes.

use std::cell::RefCell;
use std::sync::{Mutex, OnceLock};
use std::thread::{self, ThreadId};

const MAX_DEMAND_DEPTH: usize = 128;

thread_local! {
    static DEMAND_STACK: RefCell<Vec<(u64, String)>> = const { RefCell::new(Vec::new()) };
}

/// A single demand result, allowing callers to borrow the cached value.
///
/// The evaluator never runs under a lock. Recursive demand fails with a cycle
/// chain; simultaneous demand for the same slot fails instead of waiting.
#[derive(Debug)]
pub struct DemandSlot<T> {
    ready: OnceLock<Result<T, String>>,
    running: Mutex<Option<ThreadId>>,
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
        }
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
}
