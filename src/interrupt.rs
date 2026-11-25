//! The interrupt of an execution graph's running spawns on its first failure
//! (`--nokeep_going`, docs/FAILFAST_INTERRUPT.md).
//!
//! Genuine Bazel 9.3.1 stops a build without `--keep_going` at its first
//! failed action: Skyframe interrupts the threads executing the other
//! actions (AbstractQueueVisitor's interrupt of in-flight work), and an
//! interrupted LocalSpawnRunner destroys its subprocess and waits for it
//! (`subprocess.destroyAndWait()`, LocalSpawnRunner.java:415-417): SIGTERM to
//! the process-wrapper, which SIGKILLs the command's process group at once
//! (OnAbruptSignal, process-wrapper-legacy.cc:93-102), or to linux-sandbox,
//! which forwards SIGTERM and SIGKILLs the sandbox once
//! `--local_termination_grace_seconds` have passed (OnTimeoutOrTerm,
//! linux-sandbox.cc:139-161). An interrupted persistent worker is destroyed
//! (WorkerSpawnRunner: no response is awaited for a non-sandboxed worker).
//! Nothing of an interrupted spawn is reported, cached or published; the
//! build's failure (and exit code) stay the failing action's
//! (tests/fixtures/failfast_interrupt: the genuine capture).
//!
//! [`Interrupt`] is that interrupt for one execution graph
//! (crate::scheduler): the graph's failure flag together with the spawns
//! running under it. A spawn registers its stopper while it runs
//! ([`Interrupt::register`]); [`Interrupt::fire`] sets the flag and stops
//! every registered spawn — the process-wrapper's abrupt kill, or for the
//! namespace sandbox (the linux-sandbox's role) SIGTERM and SIGKILL after
//! [`GRACE`]. A spawn registered after the interrupt is stopped at once.
//! The spawn's own thread, blocked in its wait, sees the spawn end and
//! returns [`interrupted`] instead of a result.

use crate::process_wrapper::Stopper;
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// `--local_termination_grace_seconds` (default 15): the time a gracefully
/// stopped spawn has between SIGTERM and SIGKILL.
pub const GRACE: Duration = Duration::from_secs(15);

/// The error text of an interrupted spawn ([`interrupted`],
/// [`is_interrupted`]): never printed, the scheduler drops it.
const INTERRUPTED: &str = "\u{0}interrupted: ";

/// How a registered spawn is stopped.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Stop {
    /// SIGKILL at once (the process-wrapper's OnAbruptSignal; a worker).
    Abrupt,
    /// SIGTERM, SIGKILL after [`GRACE`] (linux-sandbox's polite SIGTERM).
    Graceful,
}

struct Entry {
    stopper: Stopper,
    stop: Stop,
    /// Set once a signal was sent to it.
    signalled: Arc<AtomicBool>,
}

#[derive(Default)]
struct Registry {
    /// [`Interrupt::fire`] ran: new registrations are stopped at once.
    fired: bool,
    /// The grace period ended: graceful stops are SIGKILLs from now on.
    escalated: bool,
    next: u64,
    entries: HashMap<u64, Entry>,
}

/// One execution graph's failure flag and the spawns it stops.
pub struct Interrupt {
    flag: AtomicBool,
    registry: Mutex<Registry>,
    /// [`GRACE`] (shorter in tests).
    grace: Duration,
}

impl Default for Interrupt {
    fn default() -> Self {
        Self {
            flag: AtomicBool::new(false),
            registry: Mutex::new(Registry::default()),
            grace: GRACE,
        }
    }
}

impl std::fmt::Debug for Interrupt {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Interrupt")
            .field("set", &self.is_set())
            .finish()
    }
}

impl Interrupt {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// The failure flag (a test leaf polls it; crate::test_runner sets it on
    /// a `--notest_keep_going` abort, after which the scheduler fires).
    pub fn flag(&self) -> &AtomicBool {
        &self.flag
    }

    /// Whether the graph failed (the flag is set).
    pub fn is_set(&self) -> bool {
        self.flag.load(Ordering::Acquire)
    }

    /// Sets the flag and stops every registered spawn (once; later calls
    /// do nothing). The graceful stops get their SIGKILL after [`GRACE`]
    /// from a helper thread.
    pub fn fire(self: &Arc<Self>) {
        let mut registry = self.registry.lock().unwrap_or_else(|e| e.into_inner());
        self.flag.store(true, Ordering::Release);
        if registry.fired {
            return;
        }
        registry.fired = true;
        let mut graceful = false;
        for entry in registry.entries.values_mut() {
            graceful |= entry.stop == Stop::Graceful;
            signal(entry, false);
        }
        drop(registry);
        // A graceful spawn registered later starts the timer too
        // ([`Interrupt::register`]); one timer is enough.
        if graceful {
            self.start_timer();
        }
    }

    fn start_timer(self: &Arc<Self>) {
        let weak = Arc::downgrade(self);
        let grace = self.grace;
        let started = std::thread::Builder::new()
            .name("interrupt-grace".into())
            .spawn(move || {
                std::thread::sleep(grace);
                if let Some(interrupt) = weak.upgrade() {
                    interrupt.escalate();
                }
            });
        if started.is_err() {
            // No thread: no grace either.
            self.escalate();
        }
    }

    /// The grace period ended: SIGKILL to every spawn still registered.
    fn escalate(&self) {
        let mut registry = self.registry.lock().unwrap_or_else(|e| e.into_inner());
        registry.escalated = true;
        for entry in registry.entries.values_mut() {
            signal(entry, true);
        }
    }

    /// Registers a running spawn until the returned registration drops.
    /// After [`Interrupt::fire`] the spawn is stopped at once.
    pub fn register(self: &Arc<Self>, stopper: Stopper, stop: Stop) -> Registration {
        let signalled = Arc::new(AtomicBool::new(false));
        let mut entry = Entry {
            stopper,
            stop,
            signalled: Arc::clone(&signalled),
        };
        let mut registry = self.registry.lock().unwrap_or_else(|e| e.into_inner());
        let mut timer = false;
        if registry.fired {
            signal(&mut entry, registry.escalated);
            timer = stop == Stop::Graceful && !registry.escalated;
        }
        let id = registry.next;
        registry.next += 1;
        registry.entries.insert(id, entry);
        drop(registry);
        if timer {
            self.start_timer();
        }
        Registration {
            interrupt: Arc::clone(self),
            id,
            signalled,
        }
    }
}

/// Stops one spawn: SIGKILL for an abrupt stop or once escalated, else
/// SIGTERM (sent once).
fn signal(entry: &mut Entry, escalated: bool) {
    let first = !entry.signalled.swap(true, Ordering::AcqRel);
    match entry.stop {
        Stop::Abrupt => {
            if first {
                entry.stopper.kill();
            }
        }
        Stop::Graceful if escalated => entry.stopper.kill(),
        Stop::Graceful => {
            if first {
                entry.stopper.terminate();
            }
        }
    }
}

/// A running spawn's registration ([`Interrupt::register`]).
pub struct Registration {
    interrupt: Arc<Interrupt>,
    id: u64,
    signalled: Arc<AtomicBool>,
}

impl Registration {
    /// Whether the interrupt signalled the spawn: whatever it ended with is
    /// not its result.
    pub fn interrupted(&self) -> bool {
        self.signalled.load(Ordering::Acquire)
    }
}

impl Drop for Registration {
    fn drop(&mut self) {
        let mut registry = self
            .interrupt
            .registry
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        registry.entries.remove(&self.id);
    }
}

/// The error of action `id` whose spawn the interrupt stopped (or which did
/// not start because of it).
pub fn interrupted(id: &str) -> String {
    format!("{INTERRUPTED}action '{id}'")
}

/// Whether `error` is an [`interrupted`] one.
pub fn is_interrupted(error: &str) -> bool {
    error.contains(INTERRUPTED)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;
    use std::time::Instant;

    fn sleeper() -> std::process::Child {
        use std::os::unix::process::CommandExt;
        Command::new("sleep")
            .arg("30")
            .process_group(0)
            .spawn()
            .unwrap()
    }

    #[test]
    fn fire_kills_registered_and_later_spawns() {
        let interrupt = Interrupt::new();
        let mut first = sleeper();
        let registration = interrupt.register(Stopper::group(first.id()), Stop::Abrupt);
        assert!(!registration.interrupted());
        let started = Instant::now();
        interrupt.fire();
        assert!(interrupt.is_set());
        let status = first.wait().unwrap();
        assert!(!status.success());
        assert!(registration.interrupted());
        drop(registration);
        // Registered after the interrupt: stopped at once.
        let mut second = sleeper();
        let late = interrupt.register(Stopper::group(second.id()), Stop::Graceful);
        assert!(late.interrupted());
        assert!(!second.wait().unwrap().success());
        assert!(started.elapsed() < Duration::from_secs(10));
        // A second fire does nothing.
        interrupt.fire();
    }

    /// A graceful stop: SIGTERM first (ignored here), SIGKILL once the
    /// grace period ended.
    #[test]
    fn graceful_stops_escalate_after_the_grace_period() {
        use std::os::unix::process::CommandExt;
        let interrupt = Arc::new(Interrupt {
            grace: Duration::from_millis(500),
            ..Interrupt::default()
        });
        let mut child = Command::new("/bin/sh")
            .args(["-c", "trap '' TERM; while :; do sleep 0.05; done"])
            .process_group(0)
            .spawn()
            .unwrap();
        // The shell has installed its trap.
        std::thread::sleep(Duration::from_millis(200));
        let registration = interrupt.register(Stopper::group(child.id()), Stop::Graceful);
        let started = Instant::now();
        interrupt.fire();
        assert!(registration.interrupted());
        std::thread::sleep(Duration::from_millis(200));
        // The TERM sleep children die; the shell ignores it.
        assert!(child.try_wait().unwrap().is_none());
        let status = child.wait().unwrap();
        assert!(!status.success());
        assert!(started.elapsed() >= Duration::from_millis(500));
        assert!(started.elapsed() < Duration::from_secs(10));
    }

    #[test]
    fn unfired_registrations_are_not_interrupted() {
        let interrupt = Interrupt::new();
        let mut child = sleeper();
        let registration = interrupt.register(Stopper::group(child.id()), Stop::Abrupt);
        drop(registration);
        interrupt.fire();
        // Deregistered before the interrupt: untouched.
        assert!(child.try_wait().unwrap().is_none());
        let _ = child.kill();
        let _ = child.wait();
    }

    #[test]
    fn interrupted_errors_are_recognized() {
        assert!(is_interrupted(&interrupted("a")));
        assert!(!is_interrupted("action 'a' failed"));
    }
}
