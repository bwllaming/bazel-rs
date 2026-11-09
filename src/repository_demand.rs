//! Invocation-local, nonblocking memoization for repository demand.
//!
//! Each repository set owns fresh slots: successful values and failures are
//! cached only in that instance, never on disk. Use distinct slots and keys for
//! `prepare:<group>` and `repository:<canonical>` work. Runtime IDs are supplied
//! by the caller and must uniquely identify repository-set runtimes.

use std::cell::RefCell;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Condvar, Mutex, OnceLock};
use std::thread::{self, ThreadId};
use std::time::{Duration, Instant};

const MAX_DEMAND_DEPTH: usize = 128;

thread_local! {
    static DEMAND_STACK: RefCell<Vec<(u64, String)>> = const { RefCell::new(Vec::new()) };
}

/// Witness of an actual current evaluator demand, not a declaration or a
/// historical lookup. Only successful normal local materialization uses it.
pub(crate) fn evaluating(runtime: u64, key: &str) -> bool {
    DEMAND_STACK.with(|stack| {
        stack
            .borrow()
            .iter()
            .any(|(id, active)| *id == runtime && active == key)
    })
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
    note_sentinel_about(|| format!("order-dependent: {what}"));
    format!(
        "{NEEDS_REPOSITORY}: {what} depends on the repositories materialized so far and this thread does not decide it"
    )
}

thread_local! {
    /// How many no-demand sentinels this thread raised (P3: a sentinel a
    /// caller swallowed still voids the work it aborted).
    static SENTINELS: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

/// Count one sentinel raised on this thread ([`sentinels_raised`]).
pub fn note_sentinel() {
    SENTINELS.with(|count| count.set(count.get().wrapping_add(1)));
}

/// Whether [`note_sentinel_about`] keeps what each sentinel was about
/// (`BAZEL_RS_EVAL_TIMELINE`: why the evaluator's pool left work to its
/// sequential pass).
pub static DESCRIBE_SENTINELS: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

thread_local! {
    static LAST_SENTINEL: std::cell::RefCell<Option<String>> =
        const { std::cell::RefCell::new(None) };
}

/// [`note_sentinel`], remembering `what` it was about while
/// [`DESCRIBE_SENTINELS`] is on ([`last_sentinel`]).
pub fn note_sentinel_about(what: impl FnOnce() -> String) {
    note_sentinel();
    if DESCRIBE_SENTINELS.load(std::sync::atomic::Ordering::Relaxed) {
        let what = what();
        LAST_SENTINEL.with(|last| *last.borrow_mut() = Some(what));
    }
}

/// What the last sentinel this thread raised was about (diagnostics).
pub fn last_sentinel() -> Option<String> {
    LAST_SENTINEL.with(|last| last.borrow().clone())
}

/// How many no-demand sentinels ([`NEEDS_REPOSITORY`]) this thread has
/// raised so far: work during which the count moved depended on a demand
/// it did not make, even when a caller swallowed the error.
pub fn sentinels_raised() -> u64 {
    SENTINELS.with(std::cell::Cell::get)
}

/// The sentinel of the unfetched value `key` of runtime `runtime_id`.
fn unfetched(runtime_id: u64, key: &str) -> String {
    note_sentinel_about(|| format!("unfetched runtime value {key}"));
    format!(
        "{NEEDS_REPOSITORY}: runtime {runtime_id}:{key} is unfetched and this thread does not fetch"
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

/// Put back a record [`take_order_dependent`] took (a scope that inspected
/// its own work restores its caller's).
pub fn restore_order_dependent(recorded: bool) {
    if recorded {
        ORDER_DEPENDENT.with(|flag| flag.set(true));
    }
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
    /// Depth of committed-unit mode on this thread: the no-demand load
    /// running here is one the build commits (a target pattern's package,
    /// crate::engine::pattern_pool), not a guess, so the repositories its
    /// files load from may be fetched (reused) ahead like the sequential
    /// engine's (crate::loading's `warm_load_roots`).
    static COMMITTED_UNIT: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// Whether the no-demand load on this thread is one the build commits
/// ([`committed_unit`]).
pub fn committed_unit_active() -> bool {
    COMMITTED_UNIT.with(std::cell::Cell::get) > 0
}

/// RAII scope: committed-unit mode (see [`committed_unit_active`]).
pub struct CommittedUnitGuard;

pub fn committed_unit() -> CommittedUnitGuard {
    COMMITTED_UNIT.with(|depth| depth.set(depth.get() + 1));
    CommittedUnitGuard
}

impl Drop for CommittedUnitGuard {
    fn drop(&mut self) {
        COMMITTED_UNIT.with(|depth| depth.set(depth.get().saturating_sub(1)));
    }
}

thread_local! {
    /// The slots whose reuse-only probe runs on this thread, outermost
    /// first ([`DemandSlot::get_or_probe`]). A probe nested in another one
    /// (a recorded input in another repository) never waits for a third
    /// thread's probe, so probe waits cannot form a cycle; and a probe that
    /// reaches a slot this thread is already probing — recorded inputs of
    /// repositories that name each other — is a miss, never a recursion
    /// without end (W6 review F4).
    static PROBING: RefCell<Vec<usize>> = const { RefCell::new(Vec::new()) };
}

/// The identity of `slot` in [`PROBING`].
fn probe_key<T>(slot: &DemandSlot<T>) -> usize {
    std::ptr::from_ref(slot).cast::<()>() as usize
}

/// The longest a top-level probe waits for another thread's probe of the
/// same slot before it probes on its own (a safety valve only: probes
/// read, and nested probes never wait).
const PROBE_WAIT: Duration = Duration::from_secs(1);

#[cfg(test)]
thread_local! {
    /// Test hook: this thread's bound on a probe wait instead of
    /// [`PROBE_WAIT`], so a test controls which side of the bound a wait
    /// ends on instead of racing it.
    static PROBE_WAIT_OVERRIDE: std::cell::Cell<Option<Duration>> =
        const { std::cell::Cell::new(None) };
}

/// This thread's bound on a probe wait ([`PROBE_WAIT`]).
fn probe_wait() -> Duration {
    #[cfg(test)]
    if let Some(wait) = PROBE_WAIT_OVERRIDE.with(std::cell::Cell::get) {
        return wait;
    }
    PROBE_WAIT
}

/// What a reuse-only probe obtained on its thread besides its value
/// ([`DemandSlot::get_or_probe_obtaining`]): stored with the value exactly
/// when the probe's value is the slot's publication.
#[derive(Debug, Default)]
pub struct Obtained {
    /// The keys of the values it obtained while it probed (W6 review F2
    /// follow-up: the roots a warm-reuse probe's recorded-input check
    /// demanded).
    pub keys: Vec<String>,
    /// What it observed for the analysis snapshot, in a detached capture
    /// (W6R, crate::observed_fs::capture_detached): replayed where the
    /// published value begins to count for the sequential engine.
    pub observations: Option<std::sync::Arc<crate::observed_fs::Captured>>,
}

impl From<Vec<String>> for Obtained {
    fn from(keys: Vec<String>) -> Self {
        Self {
            keys,
            observations: None,
        }
    }
}

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
    /// Whether the sequential engine has obtained the value: a thread
    /// outside no-demand mode demanded it, or the coordinator noted it
    /// otherwise ([`Self::note_sequential`]: the commit of a load unit
    /// that obtained it on a worker, a shared-cache hit on one of its
    /// `.bzl` modules or an ignored-subdirectories hit on its repository,
    /// the obtains of a probe that published a value that counts). A value
    /// a worker's probe published stays invisible to [`Self::sequential`]
    /// until then (W6 review F2 and its follow-ups).
    sequential: AtomicBool,
    /// The keys of the values the probe that published this value
    /// obtained on its thread while it probed ([`Self::get_or_probe_obtaining`]):
    /// the evaluation the sequential engine runs instead of the probe
    /// obtains them too, so they count wherever this value does (W6
    /// review F2 follow-up). Set before the value is published; empty for
    /// a value an evaluator computed.
    obtained: OnceLock<Vec<String>>,
    /// What the probe that published this value observed for the
    /// analysis snapshot ([`Obtained::observations`]; W6R). Set before the
    /// value is published, with `obtained`; never for an evaluator's value
    /// (the evaluating thread recorded what it observed).
    observations: OnceLock<std::sync::Arc<crate::observed_fs::Captured>>,
    /// Test instrumentation: how many top-level probers started waiting
    /// for another thread's probe of this slot.
    #[cfg(test)]
    waits: std::sync::atomic::AtomicUsize,
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

/// One probe on this thread ([`PROBING`]); releases the slot's probe
/// claim (waking its waiters) when the probe held it, after any
/// publication.
struct ProbeScope<'a, T> {
    slot: &'a DemandSlot<T>,
    claimed: bool,
}

impl<'a, T> ProbeScope<'a, T> {
    fn enter(slot: &'a DemandSlot<T>, claimed: bool) -> Self {
        PROBING.with(|probing| probing.borrow_mut().push(probe_key(slot)));
        Self { slot, claimed }
    }
}

impl<T> Drop for ProbeScope<'_, T> {
    fn drop(&mut self) {
        // No panicking borrows while unwinding.
        let key = probe_key(self.slot);
        let _ = PROBING.try_with(|probing| {
            if let Ok(mut probing) = probing.try_borrow_mut()
                && let Some(position) = probing.iter().rposition(|probed| *probed == key)
            {
                probing.remove(position);
            }
        });
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
            sequential: AtomicBool::new(false),
            obtained: OnceLock::new(),
            observations: OnceLock::new(),
            #[cfg(test)]
            waits: std::sync::atomic::AtomicUsize::new(0),
        }
    }

    /// The keys the probe that published this value obtained while it
    /// probed (see the `obtained` field); empty when no probe published
    /// it, or the probe obtained nothing.
    pub fn obtained(&self) -> &[String] {
        self.obtained.get().map_or(&[], Vec::as_slice)
    }

    /// What the probe that published this value observed (see the
    /// `observations` field); `None` when no probe published it, or the
    /// probe observed nothing (not recording).
    pub fn observations(&self) -> Option<&std::sync::Arc<crate::observed_fs::Captured>> {
        self.observations.get()
    }

    /// The successfully computed value once the sequential engine has
    /// obtained it (see the `sequential` field): what a decision that must
    /// not depend on worker timing may treat as materialized.
    pub fn sequential(&self) -> Option<&T> {
        if self.sequential.load(Ordering::Relaxed) {
            self.ready()
        } else {
            None
        }
    }

    /// Record that the sequential engine obtained the value (see the
    /// `sequential` field; for repository roots,
    /// crate::repository's `DirectRuntime::mark_sequential`).
    pub fn note_sequential(&self) {
        if !self.sequential.load(Ordering::Relaxed) {
            self.sequential.store(true, Ordering::Relaxed);
        }
    }

    /// [`Self::note_sequential`] for a value obtained on this thread,
    /// unless it runs in no-demand mode (a worker: its unit's commit
    /// notes it).
    fn note_demanded<'a>(&self, result: Result<&'a T, String>) -> Result<&'a T, String> {
        if result.is_ok() && !no_demand_active() {
            self.note_sequential();
        }
        result
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
        #[cfg(test)]
        self.waits.fetch_add(1, Ordering::SeqCst);
        let deadline = Instant::now() + probe_wait();
        let _waited = crate::eval_timeline::Interval("a probe claim waited for", Instant::now());
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

    /// Whether an evaluator of this slot runs now (on any thread).
    pub fn evaluating(&self) -> bool {
        match self.running.lock() {
            Ok(running) => running.is_some(),
            Err(poisoned) => poisoned.into_inner().is_some(),
        }
    }

    /// The successfully computed value, if any; never evaluates.
    pub fn ready(&self) -> Option<&T> {
        self.ready.get().and_then(|result| result.as_ref().ok())
    }

    /// Publish a value a speculation computed on a worker
    /// (crate::repository_speculation) with what it obtained, exactly like
    /// a probe's publication ([`Self::get_or_probe_obtaining`]): the keys
    /// and observations count where the value begins to count for the
    /// sequential engine. Only into an empty slot and only while no
    /// evaluator runs (the coordinator demanding the slot takes the
    /// speculation's outcome itself): otherwise the value and what it
    /// obtained are handed back.
    pub fn publish_speculated(&self, value: T, obtained: Obtained) -> Result<(), (T, Obtained)> {
        let running = match self.running.lock() {
            Ok(running) => running,
            Err(poisoned) => poisoned.into_inner(),
        };
        if running.is_some() || self.ready.get().is_some() {
            return Err((value, obtained));
        }
        if !obtained.keys.is_empty() {
            let _ = self.obtained.set(obtained.keys);
        }
        if let Some(observations) = obtained.observations {
            let _ = self.observations.set(observations);
        }
        let _ = self.ready.set(Ok(value));
        Ok(())
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
            return self.note_demanded(result.as_ref().map_err(Clone::clone));
        }
        if no_demand_active() {
            // Workers never fetch (review §1.2): the sentinel aborts the
            // unit; the coordinator re-runs it inline at its commit slot.
            return Err(unfetched(runtime_id, key));
        }
        self.note_demanded(self.evaluate(runtime_id, key, init))
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
    /// idempotently, so a duplicate probe is harmless). A probe of a slot
    /// this thread is already probing (recorded inputs that name each
    /// other's repositories) is a miss.
    pub fn get_or_probe(
        &self,
        runtime_id: u64,
        key: &str,
        probe: impl FnOnce() -> Result<Option<T>, String>,
        init: impl FnOnce() -> Result<T, String>,
    ) -> Result<&T, String> {
        self.get_or_probe_obtaining(runtime_id, key, || (probe(), Obtained::default()), init)
    }

    /// [`Self::get_or_probe`] whose probe also returns what it obtained on
    /// this thread while it probed ([`Obtained`]): the keys of the values
    /// it obtained (W6 review F2 follow-up: the roots a warm-reuse probe's
    /// recorded-input check demanded) and what it observed for the
    /// analysis snapshot (W6R). They are stored with the value exactly
    /// when this probe's value is the one published ([`Self::obtained`],
    /// [`Self::observations`]); a miss, an error or a lost publication race
    /// drops them, and a waiter served by another probe's publication gets
    /// that probe's.
    pub fn get_or_probe_obtaining<O: Into<Obtained>>(
        &self,
        runtime_id: u64,
        key: &str,
        probe: impl FnOnce() -> (Result<Option<T>, String>, O),
        init: impl FnOnce() -> Result<T, String>,
    ) -> Result<&T, String> {
        if let Some(result) = self.ready.get() {
            return self.note_demanded(result.as_ref().map_err(Clone::clone));
        }
        if !no_demand_active() {
            return self.note_demanded(self.evaluate(runtime_id, key, init));
        }
        if !reuse_only_active() {
            return Err(unfetched(runtime_id, key));
        }
        let (nested, reentered) = PROBING.with(|probing| {
            let probing = probing.borrow();
            (!probing.is_empty(), probing.contains(&probe_key(self)))
        });
        if reentered {
            // This thread is probing this very slot further up its stack:
            // a miss (the coordinator's evaluation reports a real cycle).
            return Err(unfetched(runtime_id, key));
        }
        let claimed = if !nested {
            match self.claim_probe() {
                ProbeClaim::Owner => true,
                ProbeClaim::Finished => {
                    return match self.ready.get() {
                        Some(result) => result.as_ref().map_err(Clone::clone),
                        None => Err(unfetched(runtime_id, key)),
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
        let (probed, obtained) = probe();
        let obtained: Obtained = obtained.into();
        match probed.unwrap_or(None) {
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
                    return Err(unfetched(runtime_id, key));
                }
                // Under this lock no evaluator runs (none can start) and
                // every other probe publishes under it too: the value and
                // what its probe obtained publish together, the keys and
                // observations first, so whoever sees the value sees them.
                if self.ready.get().is_none() {
                    if !obtained.keys.is_empty() {
                        let _ = self.obtained.set(obtained.keys);
                    }
                    if let Some(observations) = obtained.observations {
                        let _ = self.observations.set(observations);
                    }
                    let _ = self.ready.set(Ok(value));
                }
                drop(running);
                self.ready
                    .get()
                    .expect("just published or concurrently published")
                    .as_ref()
                    .map_err(Clone::clone)
            }
            None => Err(unfetched(runtime_id, key)),
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

    // Only bounds how long a wrong (waiting) implementation stalls a test
    // before it fails; a correct one never reaches it, also on a loaded host.
    const TIMEOUT: Duration = Duration::from_secs(60);

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

    /// A top-level prober spawned on another thread: its probe signals
    /// `started`, holds until the returned worker is released (no sleeps:
    /// the test decides when the probe ends), then returns `outcome`.
    fn released_prober(
        slot: Arc<DemandSlot<usize>>,
        outcome: Option<usize>,
    ) -> (Worker<Result<usize, String>>, mpsc::Receiver<()>) {
        let (started_tx, started_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let handle = thread::spawn(move || {
            let _guard = no_demand();
            let _reuse = reuse_only();
            slot.get_or_probe(
                7,
                "repository:x",
                || {
                    started_tx.send(()).map_err(|error| error.to_string())?;
                    release_rx
                        .recv_timeout(TIMEOUT)
                        .map_err(|error| format!("release failed: {error}"))?;
                    Ok(outcome)
                },
                || Ok(1),
            )
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

    /// A wait bound no passing test reaches: the wait ends when its probe
    /// does (a broken wakeup fails the test after it, instead of hanging).
    const UNBOUNDED: Duration = Duration::from_secs(60);

    /// A top-level prober on another thread whose wait for an in-flight
    /// probe is bounded by `wait`; probing itself counts in `calls` and
    /// finds 9.
    fn waiting_prober(
        slot: Arc<DemandSlot<usize>>,
        calls: Arc<AtomicUsize>,
        wait: Duration,
    ) -> thread::JoinHandle<Result<usize, String>> {
        thread::spawn(move || {
            PROBE_WAIT_OVERRIDE.with(|bound| bound.set(Some(wait)));
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
        })
    }

    /// Whether `count` probers started waiting on `slot` (within TIMEOUT).
    fn waiters_arrive(slot: &DemandSlot<usize>, count: usize) -> bool {
        let deadline = Instant::now() + TIMEOUT;
        while slot.waits.load(Ordering::SeqCst) < count {
            if Instant::now() > deadline {
                return false;
            }
            thread::sleep(Duration::from_millis(1));
        }
        true
    }

    /// W6: concurrent top-level probes of one slot run once — the waiter
    /// is served by the in-flight probe's publication without probing.
    #[test]
    fn concurrent_top_level_probe_waits_for_the_in_flight_probe() {
        let slot = Arc::new(DemandSlot::new());
        let (owner, started) = released_prober(Arc::clone(&slot), Some(5));
        started.recv_timeout(TIMEOUT).unwrap();
        let calls = Arc::new(AtomicUsize::new(0));
        let waiter = waiting_prober(Arc::clone(&slot), Arc::clone(&calls), UNBOUNDED);
        assert!(waiters_arrive(&slot, 1));
        assert_eq!(owner.finish().unwrap(), Ok(5));
        assert_eq!(waiter.join().unwrap(), Ok(5));
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        // The claim is released.
        assert!(!*slot.probing.lock().unwrap());
    }

    /// The waiter of a probe that missed misses too, without probing and
    /// without populating the slot; a later prober probes again.
    #[test]
    fn waiter_of_a_missed_probe_misses_without_probing() {
        let slot = Arc::new(DemandSlot::new());
        let (owner, started) = released_prober(Arc::clone(&slot), None);
        started.recv_timeout(TIMEOUT).unwrap();
        let calls = Arc::new(AtomicUsize::new(0));
        let waiter = waiting_prober(Arc::clone(&slot), Arc::clone(&calls), UNBOUNDED);
        assert!(waiters_arrive(&slot, 1));
        assert!(is_needs_repository(&owner.finish().unwrap().unwrap_err()));
        assert!(is_needs_repository(&waiter.join().unwrap().unwrap_err()));
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        assert!(slot.ready().is_none());
        let _guard = no_demand();
        let _reuse = reuse_only();
        assert_eq!(
            slot.get_or_probe(7, "repository:x", || Ok(Some(9)), || Ok(1)),
            Ok(&9)
        );
    }

    /// The wait is a safety valve ([`PROBE_WAIT`]): a waiter whose owner
    /// is still probing at the bound probes on its own, and the owner then
    /// loses the publish race and serves the waiter's value.
    #[test]
    fn a_probe_wait_past_its_bound_probes_on_its_own() {
        let slot = Arc::new(DemandSlot::new());
        let (owner, started) = released_prober(Arc::clone(&slot), Some(5));
        started.recv_timeout(TIMEOUT).unwrap();
        let calls = Arc::new(AtomicUsize::new(0));
        let waiter = waiting_prober(
            Arc::clone(&slot),
            Arc::clone(&calls),
            Duration::from_millis(20),
        );
        // The owner is held until after the waiter returned.
        assert_eq!(waiter.join().unwrap(), Ok(9));
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert_eq!(slot.waits.load(Ordering::SeqCst), 1);
        assert_eq!(owner.finish().unwrap(), Ok(9));
        assert!(!*slot.probing.lock().unwrap());
    }

    /// A probe nested in another probe never waits for a third thread's
    /// probe (no wait cycles): it probes on its own (a wait would serve the
    /// owner's 5 — or, the owner being held, time out after waiting).
    #[test]
    fn nested_probe_does_not_wait_for_an_in_flight_probe() {
        let inner = Arc::new(DemandSlot::new());
        let (owner, started) = released_prober(Arc::clone(&inner), Some(5));
        started.recv_timeout(TIMEOUT).unwrap();
        let outer: DemandSlot<usize> = DemandSlot::new();
        let mut nested = None;
        let result = {
            let _guard = no_demand();
            let _reuse = reuse_only();
            outer
                .get_or_probe(
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
                )
                .copied()
        };
        assert_eq!(result, Ok(3));
        assert_eq!(nested, Some(Ok(9)));
        assert_eq!(inner.waits.load(Ordering::SeqCst), 0);
        // The owner loses the publish race and serves the nested value.
        assert_eq!(owner.finish().unwrap(), Ok(9));
        assert!(PROBING.with(|probing| probing.borrow().is_empty()));
    }

    /// W6 review F4: probes whose recorded inputs name each other's
    /// repositories (a's validation demands b, b's demands a) used to
    /// recurse until the stack overflowed. The re-entered probe is a miss,
    /// each enclosing probe sees its input's miss and misses too, and
    /// nothing is published; a slot whose input names itself likewise.
    #[test]
    fn a_probe_reentering_a_slot_it_is_probing_is_a_miss() {
        /// The probe of `slots[index]`: its recorded input is the root of
        /// `slots[input(index)]`. Bounded, so a regression fails instead of
        /// overflowing the test's stack.
        fn probe(
            slots: &[DemandSlot<usize>],
            index: usize,
            input: fn(usize) -> usize,
            calls: &AtomicUsize,
        ) -> Result<usize, String> {
            slots[index]
                .get_or_probe(
                    7,
                    &format!("repository:{index}"),
                    || {
                        if calls.fetch_add(1, Ordering::SeqCst) > 64 {
                            return Err("unbounded probe recursion".to_owned());
                        }
                        probe(slots, input(index), input, calls).map(|root| Some(root + 1))
                    },
                    || Ok(0),
                )
                .copied()
        }
        let _guard = no_demand();
        let _reuse = reuse_only();
        for (slots, input, probes) in [
            (2, (|index| 1 - index) as fn(usize) -> usize, 2),
            (1, (|index| index) as fn(usize) -> usize, 1),
        ] {
            let slots: Vec<DemandSlot<usize>> = (0..slots).map(|_| DemandSlot::new()).collect();
            let calls = AtomicUsize::new(0);
            let error = probe(&slots, 0, input, &calls).unwrap_err();
            assert!(is_needs_repository(&error), "{error}");
            assert_eq!(calls.load(Ordering::SeqCst), probes);
            assert!(slots.iter().all(|slot| slot.ready().is_none()));
            assert!(PROBING.with(|probing| probing.borrow().is_empty()));
            // Released: the next probe of the slot runs again.
            assert_eq!(
                slots[0].get_or_probe(7, "repository:0", || Ok(Some(4)), || Ok(0)),
                Ok(&4)
            );
        }
    }

    /// Invariant (a) of the prefetch overlap (docs/PARALLEL_ANALYSIS.md):
    /// a worker's probe that hits while the coordinator evaluates the same
    /// slot is a miss — the sentinel, nothing published — and the
    /// evaluator's own publication never finds the slot occupied
    /// (`result already occupied`).
    #[test]
    fn a_probe_hit_during_an_evaluation_is_a_miss_and_the_evaluator_publishes() {
        let slot = Arc::new(DemandSlot::new());
        let (evaluator, evaluating) = blocked_worker(Arc::clone(&slot), "repository:x", 17);
        evaluating.recv_timeout(TIMEOUT).unwrap();
        let calls = AtomicUsize::new(0);
        let probed = {
            let _guard = no_demand();
            let _reuse = reuse_only();
            slot.get_or_probe(
                1,
                "repository:x",
                || {
                    calls.fetch_add(1, Ordering::SeqCst);
                    Ok(Some(5))
                },
                || Ok(1),
            )
            .copied()
        };
        assert!(is_needs_repository(&probed.unwrap_err()));
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert!(slot.ready().is_none());
        assert_eq!(evaluator.finish().unwrap(), Ok(17));
        assert_eq!(slot.ready(), Some(&17));
        // A later worker demand is served the evaluator's value.
        let _guard = no_demand();
        let _reuse = reuse_only();
        assert_eq!(
            slot.get_or_probe(1, "repository:x", || Ok(Some(5)), || Ok(1)),
            Ok(&17)
        );
    }

    /// Invariant (a), the other interleavings: a probe that started before
    /// the evaluation and hits while it runs is a miss; a probe that hits
    /// after the evaluator published serves the evaluator's value.
    #[test]
    fn a_probe_overlapping_an_evaluation_never_publishes_over_it() {
        // The probe ends while the evaluator runs.
        let slot = Arc::new(DemandSlot::new());
        let (prober, probing) = released_prober(Arc::clone(&slot), Some(5));
        probing.recv_timeout(TIMEOUT).unwrap();
        let (evaluator, evaluating) = blocked_worker(Arc::clone(&slot), "repository:x", 17);
        evaluating.recv_timeout(TIMEOUT).unwrap();
        assert!(is_needs_repository(&prober.finish().unwrap().unwrap_err()));
        assert!(slot.ready().is_none());
        assert_eq!(evaluator.finish().unwrap(), Ok(17));
        assert_eq!(slot.ready(), Some(&17));

        // The probe ends after the evaluator published.
        let slot = Arc::new(DemandSlot::new());
        let (prober, probing) = released_prober(Arc::clone(&slot), Some(5));
        probing.recv_timeout(TIMEOUT).unwrap();
        assert_eq!(slot.get_or_try_init(1, "repository:x", || Ok(17)), Ok(&17));
        assert_eq!(prober.finish().unwrap(), Ok(17));
        assert_eq!(slot.ready(), Some(&17));
    }

    /// W6 review F2: a value a worker's probe published is the sequential
    /// engine's only once a thread outside no-demand mode demands it, or
    /// the coordinator notes it at a unit's commit; a worker's demand of a
    /// ready value never marks it.
    #[test]
    fn probed_values_are_sequential_only_once_demanded_or_noted() {
        let demanded = DemandSlot::new();
        {
            let _guard = no_demand();
            let _reuse = reuse_only();
            assert_eq!(
                demanded.get_or_probe(7, "repository:x", || Ok(Some(5)), || Ok(1)),
                Ok(&5)
            );
            assert_eq!(
                demanded.get_or_try_init(7, "repository:x", || Ok(1)),
                Ok(&5)
            );
        }
        assert_eq!(demanded.ready(), Some(&5));
        assert_eq!(demanded.sequential(), None);
        assert_eq!(
            demanded.get_or_probe(7, "repository:x", || Ok(Some(9)), || Ok(1)),
            Ok(&5)
        );
        assert_eq!(demanded.sequential(), Some(&5));

        let committed = DemandSlot::new();
        {
            let _guard = no_demand();
            let _reuse = reuse_only();
            assert_eq!(
                committed.get_or_probe(7, "repository:y", || Ok(Some(6)), || Ok(1)),
                Ok(&6)
            );
        }
        assert_eq!(committed.sequential(), None);
        committed.note_sequential();
        assert_eq!(committed.sequential(), Some(&6));

        // Evaluated outside no-demand mode: sequential at once; a failure
        // never is.
        let evaluated = DemandSlot::new();
        assert_eq!(
            evaluated.get_or_try_init(7, "repository:z", || Ok(3)),
            Ok(&3)
        );
        assert_eq!(evaluated.sequential(), Some(&3));
        let failed = DemandSlot::<usize>::new();
        assert!(
            failed
                .get_or_try_init(7, "repository:w", || Err("no".into()))
                .is_err()
        );
        failed.note_sequential();
        assert_eq!(failed.sequential(), None);
    }

    /// W6 review F2 follow-up: what a probe obtained is stored with the
    /// value exactly when that probe's value is the slot's publication —
    /// never for a miss, a probe error, a probe that lost to an earlier
    /// publication, or a value an evaluator computed.
    #[test]
    fn a_probe_stores_what_it_obtained_only_with_its_own_publication() {
        let obtained = || vec!["b".to_owned(), "c".to_owned()];
        let probe = |value: Result<Option<usize>, String>| move || (value, obtained());
        let guard = no_demand();
        let reuse = reuse_only();

        let published = DemandSlot::new();
        assert_eq!(
            published.get_or_probe_obtaining(7, "repository:a", probe(Ok(Some(5))), || Ok(1)),
            Ok(&5)
        );
        assert_eq!(published.obtained(), ["b", "c"]);
        // A later probe is served by the publication and stores nothing.
        assert_eq!(
            published.get_or_probe_obtaining(
                7,
                "repository:a",
                || (Ok(Some(9)), vec!["d".to_owned()]),
                || Ok(1)
            ),
            Ok(&5)
        );
        assert_eq!(published.obtained(), ["b", "c"]);

        for missed in [Ok(None), Err("probe error".to_owned())] {
            let slot = DemandSlot::new();
            assert!(
                slot.get_or_probe_obtaining(7, "repository:a", probe(missed), || Ok(1))
                    .is_err_and(|error| is_needs_repository(&error))
            );
            assert_eq!(slot.ready(), None);
            assert!(slot.obtained().is_empty());
        }

        // The coordinator evaluated and published the slot while the probe
        // ran: its value stands and the probe's keys are dropped.
        let raced = std::sync::Arc::new(DemandSlot::new());
        let coordinator = raced.clone();
        assert_eq!(
            raced.get_or_probe_obtaining(
                7,
                "repository:a",
                move || {
                    std::thread::spawn(move || {
                        coordinator
                            .get_or_try_init(7, "repository:a", || Ok(3))
                            .copied()
                    })
                    .join()
                    .unwrap()
                    .unwrap();
                    (Ok(Some(4)), vec!["late".to_owned()])
                },
                || Ok(1)
            ),
            Ok(&3)
        );
        assert!(raced.obtained().is_empty());
        drop(reuse);
        drop(guard);

        // An evaluator's value obtained nothing through a probe.
        let evaluated = DemandSlot::new();
        assert_eq!(
            evaluated.get_or_probe_obtaining(7, "repository:a", probe(Ok(Some(5))), || Ok(2)),
            Ok(&2)
        );
        assert!(evaluated.obtained().is_empty());
    }

    /// A speculation (crate::repository_speculation) publishes its value
    /// with what it obtained only into an empty slot no evaluator is
    /// running; otherwise it gets its value back for the coordinator.
    #[test]
    fn a_speculation_publishes_only_into_an_idle_empty_slot() {
        let speculated = |keys: &[&str]| Obtained {
            keys: keys.iter().map(|key| (*key).to_owned()).collect(),
            observations: None,
        };
        let slot = DemandSlot::new();
        assert!(slot.publish_speculated(5, speculated(&["b"])).is_ok());
        assert_eq!(slot.ready(), Some(&5));
        assert_eq!(slot.obtained(), ["b"]);
        assert_eq!(slot.sequential(), None, "counts only once noted");
        let (value, obtained) = slot.publish_speculated(6, speculated(&["c"])).unwrap_err();
        assert_eq!((value, obtained.keys), (6, vec!["c".to_owned()]));
        assert_eq!(slot.obtained(), ["b"]);

        // The coordinator evaluates the slot: the speculation is refused
        // and the evaluator's value stands.
        let slot = Arc::new(DemandSlot::new());
        let (evaluator, evaluating) = blocked_worker(Arc::clone(&slot), "repository:x", 17);
        evaluating.recv_timeout(TIMEOUT).unwrap();
        assert!(slot.publish_speculated(4, speculated(&[])).is_err());
        assert_eq!(evaluator.finish().unwrap(), Ok(17));
        assert_eq!(slot.ready(), Some(&17));
    }

    /// W6R: what a probe observed for the analysis snapshot is stored with
    /// the value exactly when that probe's value is the slot's publication
    /// — never for a miss, a probe error, a probe that lost to an earlier
    /// publication or to an evaluator — and a waiter served by an in-flight
    /// probe's publication gets that probe's set (its own probe never
    /// runs).
    #[test]
    fn a_probe_stores_its_observations_only_with_its_own_publication() {
        use crate::observed_fs::Captured;
        let probed = |value: Result<Option<usize>, String>, set: &Arc<Captured>| {
            let set = set.clone();
            move || {
                (
                    value,
                    Obtained {
                        keys: vec!["b".to_owned()],
                        observations: Some(set),
                    },
                )
            }
        };
        let same = |slot: &DemandSlot<usize>, set: &Arc<Captured>| {
            slot.observations()
                .is_some_and(|stored| Arc::ptr_eq(stored, set))
        };
        let (first, second) = (Captured::for_test(), Captured::for_test());
        let guard = no_demand();
        let reuse = reuse_only();
        let published = DemandSlot::new();
        assert_eq!(
            published.get_or_probe_obtaining(
                7,
                "repository:a",
                probed(Ok(Some(5)), &first),
                || { Ok(1) }
            ),
            Ok(&5)
        );
        assert!(same(&published, &first));
        // A later probe is served by the publication and stores nothing.
        assert_eq!(
            published.get_or_probe_obtaining(
                7,
                "repository:a",
                probed(Ok(Some(9)), &second),
                || { Ok(1) }
            ),
            Ok(&5)
        );
        assert!(same(&published, &first));
        for missed in [Ok(None), Err("probe error".to_owned())] {
            let slot = DemandSlot::new();
            assert!(
                slot.get_or_probe_obtaining(7, "repository:a", probed(missed, &first), || Ok(1))
                    .is_err_and(|error| is_needs_repository(&error))
            );
            assert!(slot.observations().is_none() && slot.ready().is_none());
        }
        // The coordinator published while the probe ran: the probe's set
        // is dropped with its value.
        let raced = Arc::new(DemandSlot::new());
        let coordinator = raced.clone();
        let late = first.clone();
        assert_eq!(
            raced.get_or_probe_obtaining(
                7,
                "repository:a",
                move || {
                    thread::spawn(move || {
                        coordinator
                            .get_or_try_init(7, "repository:a", || Ok(3))
                            .copied()
                    })
                    .join()
                    .unwrap()
                    .unwrap();
                    (
                        Ok(Some(4)),
                        Obtained {
                            keys: Vec::new(),
                            observations: Some(late),
                        },
                    )
                },
                || Ok(1)
            ),
            Ok(&3)
        );
        assert!(raced.observations().is_none());
        drop(reuse);
        drop(guard);
        // An evaluator's value carries no probe's set.
        let evaluated = DemandSlot::new();
        assert_eq!(
            evaluated.get_or_probe_obtaining(
                7,
                "repository:a",
                probed(Ok(Some(5)), &first),
                || { Ok(2) }
            ),
            Ok(&2)
        );
        assert!(evaluated.observations().is_none());

        // A waiter served by the in-flight probe's publication: the owner's
        // set, and the waiter's probe never ran.
        let slot = Arc::new(DemandSlot::new());
        let (started_tx, started_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel::<()>();
        let owner = {
            let slot = slot.clone();
            let first = first.clone();
            thread::spawn(move || {
                let _guard = no_demand();
                let _reuse = reuse_only();
                slot.get_or_probe_obtaining(
                    7,
                    "repository:x",
                    move || {
                        started_tx.send(()).unwrap();
                        release_rx.recv_timeout(TIMEOUT).unwrap();
                        (
                            Ok(Some(5)),
                            Obtained {
                                keys: Vec::new(),
                                observations: Some(first),
                            },
                        )
                    },
                    || Ok(1),
                )
                .copied()
            })
        };
        started_rx.recv_timeout(TIMEOUT).unwrap();
        let ran = Arc::new(AtomicUsize::new(0));
        let waiter = {
            let slot = slot.clone();
            let second = second.clone();
            let ran = ran.clone();
            thread::spawn(move || {
                PROBE_WAIT_OVERRIDE.with(|bound| bound.set(Some(UNBOUNDED)));
                let _guard = no_demand();
                let _reuse = reuse_only();
                slot.get_or_probe_obtaining(
                    7,
                    "repository:x",
                    move || {
                        ran.fetch_add(1, Ordering::SeqCst);
                        (
                            Ok(Some(9)),
                            Obtained {
                                keys: Vec::new(),
                                observations: Some(second),
                            },
                        )
                    },
                    || Ok(1),
                )
                .copied()
            })
        };
        assert!(waiters_arrive(&slot, 1));
        release_tx.send(()).unwrap();
        assert_eq!(owner.join().unwrap(), Ok(5));
        assert_eq!(waiter.join().unwrap(), Ok(5));
        assert_eq!(ran.load(Ordering::SeqCst), 0);
        assert!(same(&slot, &first));
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
