//! Speculative module-extension preparation (docs/COLD_ANALYSIS.md,
//! "Module extensions prepared ahead"): once the module graph and the
//! lockfile are known, the extension groups the root module uses are
//! prepared on background workers — their definition `.bzl` graph loaded,
//! their lockfile entry checked and bound, their generated repositories'
//! specifications built — ahead of the coordinator's sequential demand.
//!
//! This module is the scheduling layer only: who prepares which group when
//! (crate::repository runs the preparation, `RepositorySet::
//! extension_speculation_job`). A speculation runs in no-demand +
//! reuse-only mode, like a loading worker, and everything the preparation
//! would make visible is held back for the coordinator:
//! - console writes (`.bzl` prints, the use_repo fixup warning) in a
//!   crate::ui::DeferredConsole;
//! - what it observes for the analysis snapshot, in a detached capture
//!   (crate::observed_fs::capture_detached);
//! - the direct and generated repository roots it obtained (crate::
//!   repository's `UnitRoots`) and the registry module roots it used (warm
//!   trees in the repository store, never their demand slots);
//! - the definition `.bzl` cache entries and definition modules its loads
//!   add (crate::loading's speculative definitions);
//! - the lockfile: its entry is only read; consulting the hidden lockfile
//!   and recording the result are the commit's.
//!
//! A speculation is eligible only when its lockfile entry is reusable (the
//! extension itself never runs ahead). The coordinator takes the outcome
//! where the sequential engine prepares the group — the group's demand slot
//! evaluation — demands the registry roots it used there, re-checks the
//! lockfile entry against the inputs as they are now, and then replays what
//! it held back in order. Any difference (a root that differs, an entry no
//! longer reusable, output from the check) drops the outcome and the
//! coordinator prepares the group itself. An outcome nobody takes is
//! dropped with the pool: nothing of it was ever visible.

use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};

/// At most this many groups are prepared at once.
pub(crate) const MAX_WORKERS: usize = 8;

/// The worker stack size (as the loading workers': Starlark evaluation).
const WORKER_STACK: usize = 16 << 20;

/// How often one group is attempted (a blocked attempt is retried once its
/// blocking demand is satisfied).
const MAX_ATTEMPTS: u32 = 4;

thread_local! {
    static ACTIVE: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    /// The demanded flag of the speculation running on this thread
    /// ([`Pool::take`] sets it while the coordinator waits for it).
    static DEMANDED: std::cell::RefCell<Option<Arc<AtomicBool>>> =
        const { std::cell::RefCell::new(None) };
}

/// Whether the speculation running on this thread is waited for by the
/// coordinator: its transfers are demanded, not speculative
/// (crate::repository_speculation::speculative; a speculation reads
/// registry roots offline, so today it transfers nothing).
pub(crate) fn demanded() -> bool {
    DEMANDED.with(|demanded| {
        demanded
            .borrow()
            .as_ref()
            .is_some_and(|demanded| demanded.load(Ordering::Relaxed))
    })
}

/// Whether this thread prepares an extension group speculatively.
pub(crate) fn active() -> bool {
    ACTIVE.with(std::cell::Cell::get)
}

/// Marks this thread as a speculation's while it lives.
pub(crate) struct ActiveGuard(bool);

pub(crate) fn enter() -> ActiveGuard {
    ActiveGuard(ACTIVE.with(|active| active.replace(true)))
}

impl Drop for ActiveGuard {
    fn drop(&mut self) {
        ACTIVE.with(|active| active.set(self.0));
    }
}

/// How a speculation ended.
pub(crate) enum Finished<O> {
    /// Kept for the coordinator's demand ([`Pool::take`]).
    Done(Box<O>),
    /// Aborted by the no-demand sentinel: retried once the demand it names
    /// is satisfied.
    Blocked(String),
    /// Failed or ineligible: the coordinator prepares the group itself.
    Failed,
}

/// Runs one speculation of a group id on a worker.
pub(crate) type Job<O> = Arc<dyn Fn(&str) -> Finished<O> + Send + Sync>;

enum Entry<O> {
    Queued {
        attempts: u32,
    },
    /// Its demanded flag ([`demanded`]).
    Running(Arc<AtomicBool>),
    Done(Box<O>),
    Blocked {
        error: String,
        attempts: u32,
    },
    Failed,
    /// The coordinator demanded it: never speculated (again).
    Taken,
}

/// `PERF-EXTENSION` telemetry (BAZEL_RS_TIMING=1).
#[derive(Default)]
struct Counts {
    requested: u64,
    started: u64,
    done: u64,
    blocked: u64,
    failed: u64,
    retried: u64,
    taken: u64,
    waited: u64,
    wait_micros: u128,
    missed: u64,
    /// Outcomes the coordinator took but dropped (a root or the lockfile
    /// entry changed): it prepared the group itself.
    discarded: u64,
    /// Committed outcomes that used another group's speculative
    /// preparation.
    chained: u64,
}

struct State<O> {
    /// The worker bound; 0 once the pool stopped (or before it started).
    width: usize,
    queue: VecDeque<String>,
    entries: HashMap<String, Entry<O>>,
    running: usize,
    next: u64,
    job: Option<Job<O>>,
    jitter_millis: u64,
    counts: Counts,
}

impl<O> Default for State<O> {
    fn default() -> Self {
        Self {
            width: 0,
            queue: VecDeque::new(),
            entries: HashMap::new(),
            running: 0,
            next: 0,
            job: None,
            jitter_millis: 0,
            counts: Counts::default(),
        }
    }
}

struct Shared<O> {
    state: Mutex<State<O>>,
    /// Signalled when a speculation finishes or the queue gains work.
    changed: Condvar,
    workers: Mutex<Vec<std::thread::JoinHandle<()>>>,
    /// Cancels the downloads of running speculations (a warm module root's
    /// archive) once the pool stops.
    cancel: Arc<AtomicBool>,
}

/// One repository set's extension speculations (crate::repository).
pub(crate) struct Pool<O> {
    shared: Arc<Shared<O>>,
}

impl<O> Default for Pool<O> {
    fn default() -> Self {
        Self {
            shared: Arc::new(Shared {
                state: Mutex::new(State::default()),
                changed: Condvar::new(),
                workers: Mutex::new(Vec::new()),
                cancel: Arc::new(AtomicBool::new(false)),
            }),
        }
    }
}

impl<O> std::fmt::Debug for Pool<O> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("extension speculation pool")
    }
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

impl<O: Send + 'static> Pool<O> {
    /// Start accepting requests: at most `width` workers run `job`. A pool
    /// that stopped never starts again.
    pub(crate) fn start(&self, width: usize, job: Job<O>, jitter_millis: u64) {
        if self.shared.cancel.load(Ordering::SeqCst) {
            return;
        }
        let mut state = lock(&self.shared.state);
        state.width = width.min(MAX_WORKERS);
        state.job = Some(job);
        state.jitter_millis = jitter_millis;
    }

    /// Queue `id`, unless the pool is stopped or it was requested (or
    /// demanded) before.
    pub(crate) fn request(&self, id: &str) {
        {
            let mut state = lock(&self.shared.state);
            if state.width == 0 || state.entries.contains_key(id) {
                return;
            }
            state.counts.requested += 1;
            state
                .entries
                .insert(id.to_owned(), Entry::Queued { attempts: 0 });
            state.queue.push_back(id.to_owned());
        }
        self.grow();
        self.shared.changed.notify_all();
    }

    /// The coordinator demands `id`: the outcome of its speculation
    /// (waiting while it runs), or `None` when the coordinator prepares the
    /// group itself. Either way it is never speculated again.
    pub(crate) fn take(&self, id: &str) -> Option<Box<O>> {
        let mut state = lock(&self.shared.state);
        if state.entries.is_empty() {
            return None;
        }
        let mut waited = None;
        let _blocked =
            crate::eval_timeline::blocked_on(|| format!("taking the extension speculation {id}"));
        loop {
            match state.entries.get(id) {
                Some(Entry::Running(demanded)) => {
                    demanded.store(true, Ordering::Relaxed);
                    if waited.is_none() {
                        waited = Some(std::time::Instant::now());
                        state.counts.waited += 1;
                    }
                    state = self
                        .shared
                        .changed
                        .wait(state)
                        .unwrap_or_else(PoisonError::into_inner);
                }
                entry => {
                    let done = matches!(entry, Some(Entry::Done(_)));
                    let queued = matches!(entry, Some(Entry::Queued { .. }));
                    if let Some(started) = waited {
                        state.counts.wait_micros += started.elapsed().as_micros();
                    }
                    if queued {
                        state.counts.missed += 1;
                    }
                    // A queued id stays in the queue; workers skip it.
                    let previous = state.entries.insert(id.to_owned(), Entry::Taken);
                    if done {
                        state.counts.taken += 1;
                    }
                    return match previous {
                        Some(Entry::Done(outcome)) => Some(outcome),
                        _ => None,
                    };
                }
            }
        }
    }

    /// Whether the speculation of `id` finished with an outcome nobody took
    /// (`Some(true)`), or will never have one (`Some(false)`: failed, or
    /// taken); `None` when it was never requested, is queued or blocked,
    /// or still runs after waiting at most `wait` (the evaluator's
    /// pool, crate::repository's `eval_adopt_group`).
    pub(crate) fn finished(&self, id: &str, wait: std::time::Duration) -> Option<bool> {
        self.finished_waiting(id, wait, false)
    }

    /// [`Self::finished`], also waiting while it is queued (`queued`).
    pub(crate) fn finished_waiting(
        &self,
        id: &str,
        wait: std::time::Duration,
        queued: bool,
    ) -> Option<bool> {
        let deadline = std::time::Instant::now() + wait;
        let _waited = crate::eval_timeline::Interval(
            "an extension speculation waited for",
            std::time::Instant::now(),
        );
        let _blocked = crate::eval_timeline::blocked_on(|| {
            format!("the extension speculation {id} (finished)")
        });
        let mut state = lock(&self.shared.state);
        loop {
            match state.entries.get(id) {
                Some(Entry::Done(_)) => return Some(true),
                Some(Entry::Failed | Entry::Taken) => return Some(false),
                Some(Entry::Running(_)) if state.width > 0 => {
                    let now = std::time::Instant::now();
                    if now >= deadline {
                        return None;
                    }
                    state = self
                        .shared
                        .changed
                        .wait_timeout(state, deadline - now)
                        .unwrap_or_else(PoisonError::into_inner)
                        .0;
                }
                Some(Entry::Queued { .. }) if queued && state.width > 0 => {
                    let now = std::time::Instant::now();
                    if now >= deadline {
                        return None;
                    }
                    state = self
                        .shared
                        .changed
                        .wait_timeout(state, deadline - now)
                        .unwrap_or_else(PoisonError::into_inner)
                        .0;
                }
                _ => return None,
            }
        }
    }

    /// Queue the speculation of `id` again when it is blocked, whatever
    /// its attempts (the evaluator's pool satisfied what it waits for:
    /// crate::repository's `wait_eval_group`).
    pub(crate) fn retry_blocked(&self, id: &str) {
        {
            let mut state = lock(&self.shared.state);
            if state.width == 0 {
                return;
            }
            let Some(Entry::Blocked { attempts, .. }) = state.entries.get(id) else {
                return;
            };
            let attempts = attempts.saturating_sub(1);
            state.counts.retried += 1;
            state
                .entries
                .insert(id.to_owned(), Entry::Queued { attempts });
            state.queue.push_back(id.to_owned());
        }
        self.grow();
        self.shared.changed.notify_all();
    }

    /// The sentinel the speculation of `id` is blocked on, if it is.
    pub(crate) fn blocked_on(&self, id: &str) -> Option<String> {
        match lock(&self.shared.state).entries.get(id) {
            Some(Entry::Blocked { error, .. }) => Some(error.clone()),
            _ => None,
        }
    }

    /// `f` of the finished outcome of `id` nobody took yet.
    pub(crate) fn with_done<R>(&self, id: &str, f: impl FnOnce(&O) -> R) -> Option<R> {
        match lock(&self.shared.state).entries.get(id) {
            Some(Entry::Done(outcome)) => Some(f(outcome)),
            _ => None,
        }
    }

    /// Queue again every blocked speculation whose blocking demand `ready`
    /// reports satisfied.
    pub(crate) fn retry(&self, ready: &dyn Fn(&str) -> bool) {
        let mut changed = false;
        {
            let mut state = lock(&self.shared.state);
            if state.width == 0 {
                return;
            }
            let ready_ids: Vec<String> = state
                .entries
                .iter()
                .filter_map(|(id, entry)| match entry {
                    Entry::Blocked { error, attempts }
                        if *attempts < MAX_ATTEMPTS && ready(error) =>
                    {
                        Some(id.clone())
                    }
                    _ => None,
                })
                .collect();
            for id in ready_ids {
                let Some(Entry::Blocked { attempts, .. }) = state.entries.remove(&id) else {
                    continue;
                };
                state.counts.retried += 1;
                state.entries.insert(id.clone(), Entry::Queued { attempts });
                state.queue.push_back(id);
                changed = true;
            }
        }
        if changed {
            self.grow();
            self.shared.changed.notify_all();
        }
    }

    /// Count a taken outcome the coordinator dropped (`committed` false) or
    /// committed, and whether that one used another group's speculative
    /// preparation.
    pub(crate) fn note_commit(&self, committed: bool, chained: bool) {
        let mut state = lock(&self.shared.state);
        if !committed {
            state.counts.discarded += 1;
        } else if chained {
            state.counts.chained += 1;
        }
    }

    /// Wait until no speculation is queued or running (the hidden test hook
    /// `BAZEL_RS_DEBUG_EXTENSION_SPECULATION_WAIT`: the speculations finish
    /// before the command goes on).
    pub(crate) fn wait_idle(&self) {
        let mut state = lock(&self.shared.state);
        while state.width > 0
            && state
                .entries
                .values()
                .any(|entry| matches!(entry, Entry::Queued { .. } | Entry::Running(_)))
        {
            state = self
                .shared
                .changed
                .wait(state)
                .unwrap_or_else(PoisonError::into_inner);
        }
    }

    /// Whether some speculation waits for a demand to be satisfied (the
    /// coordinator then calls [`Self::retry`] after its own demands).
    pub(crate) fn any_blocked(&self) -> bool {
        let state = lock(&self.shared.state);
        state.width > 0
            && state
                .entries
                .values()
                .any(|entry| matches!(entry, Entry::Blocked { .. }))
    }

    /// Stop the pool: drop what is queued and every outcome nobody took
    /// (none was ever visible), and cancel what runs. A running
    /// speculation is not waited for (the command ends): what it finishes
    /// is dropped, and it publishes nothing anybody sees but warm module
    /// trees in the repository store, which an interrupted process leaves
    /// consistent (crate::repository_store).
    pub(crate) fn stop(&self) {
        self.shared.cancel.store(true, Ordering::SeqCst);
        let workers = {
            // Lock order: the worker list, then the state (as `grow`).
            let mut workers = lock(&self.shared.workers);
            let mut state = lock(&self.shared.state);
            state.width = 0;
            state.queue.clear();
            std::mem::take(&mut *workers).len()
        };
        self.shared.changed.notify_all();
        let mut state = lock(&self.shared.state);
        state.job = None;
        let unused = state
            .entries
            .values()
            .filter(|entry| matches!(entry, Entry::Done(_)))
            .count();
        state.entries.clear();
        if crate::plan_timings::enabled() && state.counts.requested > 0 {
            let counts = &state.counts;
            crate::snapshot::diag::line_fmt(format_args!(
                "PERF-EXTENSION workers={workers} requested={} started={} done={} blocked={} failed={} retried={} taken={} discarded={} chained={} waited={} wait={:.3}s missed={} unused={unused}",
                counts.requested,
                counts.started,
                counts.done,
                counts.blocked,
                counts.failed,
                counts.retried,
                counts.taken,
                counts.discarded,
                counts.chained,
                counts.waited,
                counts.wait_micros as f64 / 1e6,
                counts.missed,
            ));
        }
        state.counts = Counts::default();
    }

    /// Start workers until every queued or running speculation has one (at
    /// most the width). A worker that cannot be spawned leaves its work to
    /// the coordinator.
    fn grow(&self) {
        let mut workers = lock(&self.shared.workers);
        let wanted = {
            let state = lock(&self.shared.state);
            if state.width == 0 {
                return;
            }
            (state.queue.len() + state.running).min(state.width)
        };
        while workers.len() < wanted {
            let index = workers.len() as u64;
            let shared = self.shared.clone();
            let spawned = std::thread::Builder::new()
                .name("bazel-rs-extension".to_owned())
                .stack_size(WORKER_STACK)
                .spawn(move || worker(&shared, index));
            match spawned {
                Ok(handle) => workers.push(handle),
                Err(_) => break,
            }
        }
    }
}

fn worker<O>(shared: &Arc<Shared<O>>, index: u64) {
    loop {
        let demanded = Arc::new(AtomicBool::new(false));
        let (id, attempts, job, jitter, sequence) = {
            let mut state = lock(&shared.state);
            loop {
                if state.width == 0 {
                    return;
                }
                if let Some(id) = state.queue.pop_front() {
                    let Some(Entry::Queued { attempts }) = state.entries.get(&id) else {
                        continue; // taken by the coordinator meanwhile
                    };
                    let attempts = *attempts + 1;
                    let Some(job) = state.job.clone() else {
                        return;
                    };
                    state
                        .entries
                        .insert(id.clone(), Entry::Running(demanded.clone()));
                    state.running += 1;
                    state.counts.started += 1;
                    state.next += 1;
                    break (id, attempts, job, state.jitter_millis, state.next);
                }
                state = shared
                    .changed
                    .wait(state)
                    .unwrap_or_else(PoisonError::into_inner);
            }
        };
        if jitter > 0 {
            // BAZEL_RS_LOADING_JITTER (hidden test hook): reproducible
            // delays reorder speculations; results must not change.
            let roll = crate::engine::loading_jitter_roll(sequence, index);
            std::thread::sleep(std::time::Duration::from_millis(roll % (jitter + 1)));
        }
        let finished =
            crate::repository_speculation::with_cancel(Some(shared.cancel.clone()), || {
                let _active = enter();
                DEMANDED.with(|current| *current.borrow_mut() = Some(demanded.clone()));
                let finished = crate::internal_error::recoverable_asserted(|| job(&id))
                    .unwrap_or(Finished::Failed);
                DEMANDED.with(|current| *current.borrow_mut() = None);
                finished
            });
        drop(job);
        {
            let mut state = lock(&shared.state);
            state.running -= 1;
            if state.width == 0 {
                // Stopped meanwhile: nobody takes it.
                continue;
            }
            let entry = match finished {
                Finished::Done(outcome) => {
                    state.counts.done += 1;
                    Entry::Done(outcome)
                }
                Finished::Blocked(error) => {
                    state.counts.blocked += 1;
                    Entry::Blocked { error, attempts }
                }
                Finished::Failed => {
                    state.counts.failed += 1;
                    Entry::Failed
                }
            };
            // The coordinator never takes a running entry away (it waits).
            state.entries.insert(id, entry);
        }
        shared.changed.notify_all();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;
    use std::time::Duration;

    /// The coordinator takes a finished outcome, waits for a running one,
    /// and a demand of a queued or unknown id is its own (never speculated
    /// afterwards); blocked speculations retry once ready.
    #[test]
    fn take_waits_claims_and_retries() {
        let pool = Pool::<String>::default();
        let (release_tx, release_rx) = mpsc::channel::<()>();
        let release_rx = Mutex::new(release_rx);
        let (started_tx, started_rx) = mpsc::channel::<String>();
        let started_tx = Mutex::new(started_tx);
        let job: Job<String> = Arc::new(move |id: &str| {
            assert!(active());
            assert!(!demanded());
            lock(&started_tx).send(id.to_owned()).unwrap();
            lock(&release_rx)
                .recv_timeout(Duration::from_secs(60))
                .unwrap();
            // "npm" is released while the coordinator waits for it.
            assert_eq!(demanded(), id == "npm", "{id}");
            match id {
                "blocked" => Finished::Blocked("needs other".to_owned()),
                "failing" => Finished::Failed,
                _ => Finished::Done(Box::new(format!("{id} prepared"))),
            }
        });
        assert!(!active());
        assert!(pool.take("anything").is_none(), "no speculation yet");
        pool.start(1, job, 0);
        pool.request("npm");
        assert_eq!(
            started_rx.recv_timeout(Duration::from_secs(60)).unwrap(),
            "npm"
        );
        pool.request("npm");
        pool.request("failing");
        std::thread::scope(|scope| {
            let taker = scope.spawn(|| pool.take("npm"));
            std::thread::sleep(Duration::from_millis(50));
            release_tx.send(()).unwrap();
            assert_eq!(*taker.join().unwrap().expect("an outcome"), "npm prepared");
        });
        assert!(pool.take("npm").is_none(), "taken once");
        assert_eq!(
            started_rx.recv_timeout(Duration::from_secs(60)).unwrap(),
            "failing"
        );
        release_tx.send(()).unwrap();
        // A never requested id is the coordinator's and never speculated.
        assert!(pool.take("never").is_none());
        pool.request("never");
        pool.request("blocked");
        assert_eq!(
            started_rx.recv_timeout(Duration::from_secs(60)).unwrap(),
            "blocked"
        );
        release_tx.send(()).unwrap();
        let deadline = std::time::Instant::now() + Duration::from_secs(60);
        while !pool.any_blocked() {
            assert!(std::time::Instant::now() < deadline);
            std::thread::sleep(Duration::from_millis(5));
        }
        pool.retry(&|error| error == "needs other");
        assert_eq!(
            started_rx.recv_timeout(Duration::from_secs(60)).unwrap(),
            "blocked"
        );
        release_tx.send(()).unwrap();
        // The failed one is the coordinator's.
        let deadline = std::time::Instant::now() + Duration::from_secs(60);
        loop {
            let failed = matches!(
                lock(&pool.shared.state).entries.get("failing"),
                Some(Entry::Failed)
            );
            if failed {
                break;
            }
            assert!(std::time::Instant::now() < deadline);
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(pool.take("failing").is_none());
        pool.stop();
        assert!(started_rx.try_recv().is_err(), "'never' was not speculated");
        assert!(lock(&pool.shared.state).entries.is_empty());
        // A stopped pool accepts nothing.
        pool.start(1, Arc::new(|_| Finished::Failed), 0);
        pool.request("late");
        assert!(lock(&pool.shared.state).entries.is_empty());
    }

    /// Stopping drops queued speculations and outcomes nobody took.
    #[test]
    fn stop_drops_queued_and_untaken() {
        let pool = Pool::<String>::default();
        let (release_tx, release_rx) = mpsc::channel::<()>();
        let release_rx = Mutex::new(release_rx);
        pool.start(
            1,
            Arc::new(move |id: &str| {
                lock(&release_rx)
                    .recv_timeout(Duration::from_secs(60))
                    .unwrap();
                Finished::Done(Box::new(id.to_owned()))
            }),
            0,
        );
        pool.request("a");
        pool.request("b");
        release_tx.send(()).unwrap();
        drop(release_tx);
        pool.stop();
        assert!(pool.take("a").is_none());
        assert!(pool.take("b").is_none());
    }
}
