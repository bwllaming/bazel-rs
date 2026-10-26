//! Parallel repository fetching (docs/PARALLEL_ANALYSIS.md, "Parallel
//! repository fetching"): while the coordinator analyzes, a small pool of
//! background workers executes the repository rules that phase-1 prefetch
//! (crate::prefetch) found the build is about to demand — the repositories
//! its load units were blocked on — so their downloads, extractions and
//! `execute()` calls overlap instead of running one after another at each
//! sequential demand.
//!
//! This module is the scheduling layer only: who executes which repository
//! when. A speculation runs the very materialization the coordinator would
//! run (crate::repository, through crate::repository_store), on a worker in
//! no-demand + reuse-only mode — a demand of anything not yet available (an
//! unprepared extension group, a repository nobody fetched, a local rule)
//! aborts it with the no-demand sentinel, and it is retried once the
//! coordinator satisfied that demand — with everything it would show held
//! back:
//! - what the rule prints (print(), the `execute(quiet=False)` echo, the
//!   reproducibility DEBUG lines, download retry warnings) is deferred into
//!   the speculation's buffer ([`defer_stderr`]);
//! - what it observes for the analysis snapshot is captured in a detached
//!   frame (crate::observed_fs::capture_detached), and the roots it obtained
//!   are recorded (crate::repository's `UnitRoots`).
//!
//! A successful speculation that printed nothing is published into the
//! repository's demand slot like a warm-reuse probe's value: its
//! observations and obtained roots count where the root begins to count for
//! the sequential engine (`DirectRuntime::mark_sequential`). One that printed
//! stays hidden from every probe until the coordinator demands the
//! repository ([`Speculations::take`]): the coordinator then replays its
//! observations, counts its roots and writes its output exactly where the
//! sequential engine would have executed the rule. A failed speculation is
//! dropped: the coordinator executes the rule itself at its demand, so its
//! errors and prints are the sequential ones.
//!
//! The pool lives as long as the phase-1 prefetcher; stopping it cancels the
//! speculations in flight (downloads and `execute()` subprocesses check
//! [`cancelled`]) and drops the queued ones.

use std::cell::RefCell;
use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};

/// At most this many repositories execute speculatively at once: genuine
/// Bazel bounds parallel downloads by `--http_max_parallel_downloads`
/// (default 8, CommonCommandOptions.java), and downloading and extracting
/// is most of what a repository rule does. The pool is also bounded by
/// `--loading_phase_threads` (the prefetcher's width).
pub(crate) const MAX_WORKERS: usize = 8;

/// The worker stack size (as the loading workers': Starlark evaluation).
const WORKER_STACK: usize = 16 << 20;

/// How often one repository is attempted (a blocked attempt is retried once
/// its blocking demand is satisfied).
const MAX_ATTEMPTS: u32 = 4;

/// How long stopping the pool waits for cancelled speculations.
const STOP_GRACE: std::time::Duration = std::time::Duration::from_secs(2);

/// The error of a cancelled speculation (never shown: a speculation's
/// errors are dropped).
pub(crate) const CANCELLED: &str = "speculative repository fetch cancelled";

thread_local! {
    /// The speculation running on this thread.
    static CURRENT: RefCell<Option<Current>> = const { RefCell::new(None) };
}

struct Current {
    cancel: Arc<AtomicBool>,
    output: Vec<u8>,
}

/// Whether this thread executes a speculation.
pub(crate) fn active() -> bool {
    CURRENT.with(|current| current.borrow().is_some())
}

/// Whether the speculation running on this thread was cancelled (the pool
/// stopped): long-running work (a download, an `execute()` subprocess)
/// gives up.
pub(crate) fn cancelled() -> bool {
    CURRENT.with(|current| {
        current
            .borrow()
            .as_ref()
            .is_some_and(|current| current.cancel.load(Ordering::Relaxed))
    }) || ADOPTED.with(|adopted| {
        adopted
            .borrow()
            .as_ref()
            .is_some_and(|cancel| cancel.load(Ordering::Relaxed))
    })
}

thread_local! {
    /// The cancellation of a speculation this thread works for without
    /// executing it: a non-blocking download's transfer
    /// (crate::repository_ctx's pending downloads).
    static ADOPTED: RefCell<Option<Arc<AtomicBool>>> = const { RefCell::new(None) };
}

/// The cancellation of the speculation running on this thread, if any.
pub(crate) fn cancel_token() -> Option<Arc<AtomicBool>> {
    CURRENT.with(|current| {
        current
            .borrow()
            .as_ref()
            .map(|current| current.cancel.clone())
    })
}

/// Run `work` on this thread honouring `cancel` ([`cancelled`]).
pub(crate) fn with_cancel<R>(cancel: Option<Arc<AtomicBool>>, work: impl FnOnce() -> R) -> R {
    let previous = ADOPTED.with(|adopted| adopted.replace(cancel));
    let result = work();
    ADOPTED.with(|adopted| adopted.replace(previous));
    result
}

/// Bytes a repository rule writes to stderr as it executes: kept in the
/// running speculation's buffer (true), or for the caller to write live
/// (false, not a speculation thread).
pub(crate) fn defer_stderr(bytes: &[u8]) -> bool {
    CURRENT.with(|current| match current.borrow_mut().as_mut() {
        Some(current) => {
            current.output.extend_from_slice(bytes);
            true
        }
        None => false,
    })
}

/// Everything the speculation running on this thread deferred so far
/// (emptied).
pub(crate) fn take_output() -> Vec<u8> {
    CURRENT.with(|current| {
        current
            .borrow_mut()
            .as_mut()
            .map(|current| std::mem::take(&mut current.output))
            .unwrap_or_default()
    })
}

/// Withdraw the specification pointer(s) of `spec_key` in the repository
/// store `cache` — its own, and `workspace`'s
/// (crate::repository_store::workspace_pointer) — that name publication
/// `identity`: a speculation published it, but the invocation never
/// committed what the execution printed, so the next invocation must execute
/// the rule (and print) again instead of reusing it. The tree and manifest
/// stay (identical content may be claimed by other specifications).
/// Unrecorded and best-effort: a pointer another process rewrote in
/// between is left alone, or costs that process one re-execution — never a
/// wrong reuse (the warm probe revalidates whatever a pointer names).
pub(crate) fn withdraw_pointer(
    cache: &std::path::Path,
    spec_key: &str,
    workspace: Option<&str>,
    identity: &str,
) {
    #[derive(serde::Deserialize)]
    struct Pointer {
        identity: String,
    }
    let specs = cache.join("direct").join("specs");
    let pointers = std::iter::once(specs.join(spec_key)).chain(workspace.map(|workspace| {
        specs.join(crate::repository_store::workspace_pointer(
            spec_key, workspace,
        ))
    }));
    for pointer in pointers {
        // snapshot-unrecorded: undoes this process's own publication after
        // the outcome was dropped; nothing planned depends on the pointer.
        let names_it = std::fs::read(&pointer)
            .ok()
            .and_then(|bytes| serde_json::from_slice::<Pointer>(&bytes).ok())
            .is_some_and(|named| named.identity == identity);
        if names_it {
            let _ = std::fs::remove_file(&pointer);
        }
    }
}

/// What a successful speculation produced, for the coordinator's commit.
pub(crate) struct Outcome {
    pub root: PathBuf,
    /// Everything it wrote to stderr, in order.
    pub output: Vec<u8>,
    /// The roots it obtained and what it observed for the analysis snapshot.
    pub obtained: crate::repository_demand::Obtained,
    /// Undo the publication when the coordinator never commits this
    /// outcome ([`withdraw_pointer`]): the next
    /// invocation must print what this execution printed.
    pub withdraw: Option<Box<dyn FnOnce() + Send>>,
}

/// How a speculation ended (the job's result, see [`Speculations::start`]).
pub(crate) enum Finished {
    /// Published into the repository's demand slot (it printed nothing).
    Published,
    /// Kept for the coordinator's demand ([`Speculations::take`]).
    Done(Box<Outcome>),
    /// Aborted by the no-demand sentinel: retried once the demand it names
    /// is satisfied.
    Blocked(String),
    /// Failed, ineligible or nothing to do: the coordinator executes the
    /// rule itself, if it ever demands it.
    Failed,
}

/// Runs one speculation of a canonical repository name on a worker.
pub(crate) type Job = Arc<dyn Fn(&str) -> Finished + Send + Sync>;
/// Called (on a worker, no lock held) after a speculation published a root.
pub(crate) type Listener = Arc<dyn Fn() + Send + Sync>;

enum Entry {
    Queued {
        attempts: u32,
    },
    Running,
    Done(Box<Outcome>),
    Published,
    Blocked {
        error: String,
        attempts: u32,
    },
    Failed,
    /// The coordinator demanded it: never speculated (again).
    Taken,
}

/// `PERF-FETCH` telemetry (BAZEL_RS_TIMING=1).
#[derive(Default)]
struct Counts {
    requested: u64,
    started: u64,
    published: u64,
    done: u64,
    blocked: u64,
    failed: u64,
    retried: u64,
    /// Outcomes the coordinator committed, and how often it waited for a
    /// running speculation (with the total wait).
    taken: u64,
    waited: u64,
    wait_micros: u128,
    /// Demands of queued speculations nobody had started.
    missed: u64,
    /// Outcomes nobody committed, withdrawn when the pool stopped.
    withdrawn: u64,
}

#[derive(Default)]
struct State {
    /// The worker bound; 0 while the pool is stopped.
    width: usize,
    queue: VecDeque<String>,
    entries: HashMap<String, Entry>,
    running: usize,
    next: u64,
    cancel: Arc<AtomicBool>,
    job: Option<Job>,
    listener: Option<Listener>,
    jitter_millis: u64,
    counts: Counts,
}

#[derive(Default)]
struct Shared {
    state: Mutex<State>,
    /// Signalled when a speculation finishes, the queue gains work, or the
    /// pool stops.
    changed: Condvar,
    workers: Mutex<Vec<std::thread::JoinHandle<()>>>,
}

/// One repository set's speculations (crate::repository).
#[derive(Default)]
pub(crate) struct Speculations {
    shared: Arc<Shared>,
}

impl std::fmt::Debug for Speculations {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Speculations")
    }
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

impl Speculations {
    /// Start accepting requests: at most `width` workers run `job`.
    pub(crate) fn start(&self, width: usize, job: Job, listener: Listener, jitter_millis: u64) {
        let mut state = lock(&self.shared.state);
        state.width = width.min(MAX_WORKERS);
        state.cancel = Arc::new(AtomicBool::new(false));
        state.job = Some(job);
        state.listener = Some(listener);
        state.jitter_millis = jitter_millis;
    }

    /// Queue `canonical`, unless the pool is stopped or it was requested
    /// (or demanded) before.
    pub(crate) fn request(&self, canonical: &str) {
        {
            let mut state = lock(&self.shared.state);
            if state.width == 0 || state.entries.contains_key(canonical) {
                return;
            }
            state.counts.requested += 1;
            state
                .entries
                .insert(canonical.to_owned(), Entry::Queued { attempts: 0 });
            state.queue.push_back(canonical.to_owned());
        }
        self.grow();
        self.shared.changed.notify_all();
    }

    /// Whether probes must not serve `canonical` (its speculation is
    /// executing, or holds an outcome only the coordinator may commit).
    pub(crate) fn hides(&self, canonical: &str) -> bool {
        matches!(
            lock(&self.shared.state).entries.get(canonical),
            Some(Entry::Running | Entry::Done(_))
        )
    }

    /// The coordinator demands `canonical`: the outcome of its speculation
    /// (waiting while it runs), or `None` when the coordinator executes the
    /// rule itself. Either way it is never speculated again.
    pub(crate) fn take(&self, canonical: &str) -> Option<Box<Outcome>> {
        let mut state = lock(&self.shared.state);
        let mut waited = None;
        loop {
            match state.entries.get(canonical) {
                Some(Entry::Running) => {
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
                Some(Entry::Done(_)) => {
                    if let Some(started) = waited {
                        state.counts.wait_micros += started.elapsed().as_micros();
                    }
                    state.counts.taken += 1;
                    let Some(Entry::Done(outcome)) =
                        state.entries.insert(canonical.to_owned(), Entry::Taken)
                    else {
                        unreachable!("matched above");
                    };
                    return Some(outcome);
                }
                entry => {
                    let queued = matches!(entry, Some(Entry::Queued { .. }));
                    if let Some(started) = waited {
                        state.counts.wait_micros += started.elapsed().as_micros();
                    }
                    if queued {
                        state.counts.missed += 1;
                    }
                    // A queued name stays in the queue; workers skip it.
                    state.entries.insert(canonical.to_owned(), Entry::Taken);
                    return None;
                }
            }
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
            let ready_names: Vec<String> = state
                .entries
                .iter()
                .filter_map(|(name, entry)| match entry {
                    Entry::Blocked { error, attempts }
                        if *attempts < MAX_ATTEMPTS && ready(error) =>
                    {
                        Some(name.clone())
                    }
                    _ => None,
                })
                .collect();
            for name in ready_names {
                let Some(Entry::Blocked { attempts, .. }) = state.entries.remove(&name) else {
                    continue;
                };
                state.counts.retried += 1;
                state
                    .entries
                    .insert(name.clone(), Entry::Queued { attempts });
                state.queue.push_back(name);
                changed = true;
            }
        }
        if changed {
            self.grow();
            self.shared.changed.notify_all();
        }
    }

    /// Stop the pool: cancel the speculations in flight (and wait for
    /// them), drop the queued and blocked ones. Outcomes kept for the
    /// coordinator stay takeable.
    pub(crate) fn stop(&self) {
        {
            let mut state = lock(&self.shared.state);
            if state.width == 0 {
                return;
            }
            state.width = 0;
            state.cancel.store(true, Ordering::SeqCst);
            state.queue.clear();
        }
        self.shared.changed.notify_all();
        let handles = std::mem::take(&mut *lock(&self.shared.workers));
        let workers = handles.len();
        // Cancelled downloads stop at their next read and `execute()`
        // subprocesses are killed at once; a worker still blocked after the
        // grace period (a server that never answers) is left to finish on
        // its own — whatever it publishes stays unseen until a demand.
        let deadline = std::time::Instant::now() + STOP_GRACE;
        for handle in handles {
            while !handle.is_finished() && std::time::Instant::now() < deadline {
                std::thread::sleep(std::time::Duration::from_millis(2));
            }
            if handle.is_finished() {
                let _ = handle.join();
            }
        }
        let mut state = lock(&self.shared.state);
        state.job = None;
        state.listener = None;
        // An outcome nobody committed during phase 1 is dropped (a later
        // demand executes the rule itself) and its publication withdrawn:
        // what it printed was never shown.
        let mut withdrawn = Vec::new();
        state.entries.retain(|_, entry| match entry {
            Entry::Queued { .. } | Entry::Running | Entry::Blocked { .. } => false,
            Entry::Done(outcome) => {
                withdrawn.extend(outcome.withdraw.take());
                false
            }
            _ => true,
        });
        state.counts.withdrawn += withdrawn.len() as u64;
        if crate::plan_timings::enabled() {
            let counts = &state.counts;
            crate::snapshot::diag::line_fmt(format_args!(
                "PERF-FETCH workers={workers} requested={} started={} published={} done={} blocked={} failed={} retried={} taken={} waited={} wait={:.3}s missed={} withdrawn={}",
                counts.requested,
                counts.started,
                counts.published,
                counts.done,
                counts.blocked,
                counts.failed,
                counts.retried,
                counts.taken,
                counts.waited,
                counts.wait_micros as f64 / 1e6,
                counts.missed,
                counts.withdrawn,
            ));
        }
        state.counts = Counts::default();
        drop(state);
        for withdraw in withdrawn {
            withdraw();
        }
    }

    /// Start workers until every queued or running speculation has one (at
    /// most the width). A worker that cannot be spawned leaves its work to
    /// the coordinator.
    fn grow(&self) {
        // Lock order: the worker list, then the state.
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
                .name("bazel-rs-fetch".to_owned())
                .stack_size(WORKER_STACK)
                .spawn(move || worker(&shared, index));
            match spawned {
                Ok(handle) => workers.push(handle),
                Err(_) => break,
            }
        }
    }
}

fn worker(shared: &Arc<Shared>, index: u64) {
    loop {
        let (canonical, attempts, job, cancel, jitter, sequence) = {
            let mut state = lock(&shared.state);
            loop {
                if state.width == 0 {
                    return;
                }
                if let Some(canonical) = state.queue.pop_front() {
                    let Some(Entry::Queued { attempts }) = state.entries.get(&canonical) else {
                        continue; // taken by the coordinator meanwhile
                    };
                    let attempts = *attempts + 1;
                    let Some(job) = state.job.clone() else {
                        return;
                    };
                    state.entries.insert(canonical.clone(), Entry::Running);
                    state.running += 1;
                    state.counts.started += 1;
                    state.next += 1;
                    break (
                        canonical,
                        attempts,
                        job,
                        state.cancel.clone(),
                        state.jitter_millis,
                        state.next,
                    );
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
        CURRENT.with(|current| {
            *current.borrow_mut() = Some(Current {
                cancel: cancel.clone(),
                output: Vec::new(),
            });
        });
        let finished = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| job(&canonical)))
            .unwrap_or(Finished::Failed);
        CURRENT.with(|current| *current.borrow_mut() = None);
        drop(job);
        let mut published = false;
        let listener = {
            let mut state = lock(&shared.state);
            state.running -= 1;
            // Once the pool stopped, an outcome is never committed (the
            // stop dropped the others): its publication is withdrawn.
            let entry = match finished {
                Finished::Blocked(_) if cancel.load(Ordering::SeqCst) => {
                    state.counts.failed += 1;
                    Entry::Failed
                }
                Finished::Done(mut outcome) if cancel.load(Ordering::SeqCst) => {
                    state.counts.withdrawn += 1;
                    if let Some(withdraw) = outcome.withdraw.take() {
                        withdraw();
                    }
                    Entry::Failed
                }
                Finished::Published => {
                    state.counts.published += 1;
                    published = true;
                    Entry::Published
                }
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
            // The coordinator never takes a running entry away (it waits),
            // so the entry is still ours.
            state.entries.insert(canonical, entry);
            state.listener.clone()
        };
        shared.changed.notify_all();
        if published && let Some(listener) = listener {
            listener();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;
    use std::time::Duration;

    fn outcome(root: &str, output: &[u8]) -> Box<Outcome> {
        Box::new(Outcome {
            root: PathBuf::from(root),
            output: output.to_vec(),
            obtained: crate::repository_demand::Obtained::default(),
            withdraw: None,
        })
    }

    /// A speculation's prints are deferred on its thread only; the
    /// coordinator takes a finished outcome, waits for a running one, and
    /// demands of a queued or unknown name are the coordinator's own (and
    /// never speculated afterwards).
    #[test]
    fn take_waits_for_running_and_claims_names() {
        let speculations = Speculations::default();
        let (release_tx, release_rx) = mpsc::channel::<()>();
        let release_rx = Mutex::new(release_rx);
        let (started_tx, started_rx) = mpsc::channel::<String>();
        let started_tx = Mutex::new(started_tx);
        let job: Job = Arc::new(move |name: &str| {
            assert!(active());
            assert!(defer_stderr(format!("{name} printed\n").as_bytes()));
            lock(&started_tx).send(name.to_owned()).unwrap();
            lock(&release_rx)
                .recv_timeout(Duration::from_secs(60))
                .unwrap();
            let output = take_output();
            match name {
                "quiet" => Finished::Published,
                "blocked" => Finished::Blocked("needs other".to_owned()),
                _ => Finished::Done(outcome(name, &output)),
            }
        });
        assert!(!active());
        assert!(!defer_stderr(b"live"));
        speculations.start(1, job, Arc::new(|| {}), 0);
        speculations.request("loud");
        assert_eq!(
            started_rx.recv_timeout(Duration::from_secs(60)).unwrap(),
            "loud"
        );
        assert!(speculations.hides("loud"));
        // A second request of the same name is ignored.
        speculations.request("loud");
        speculations.request("quiet");
        std::thread::scope(|scope| {
            let taker = scope.spawn(|| speculations.take("loud"));
            std::thread::sleep(Duration::from_millis(50));
            release_tx.send(()).unwrap();
            let taken = taker.join().unwrap().expect("an outcome");
            assert_eq!(taken.root, PathBuf::from("loud"));
            assert_eq!(taken.output, b"loud printed\n");
        });
        assert!(!speculations.hides("loud"));
        assert!(speculations.take("loud").is_none(), "taken once");
        // "quiet" was queued behind "loud" (one worker): it runs now.
        assert_eq!(
            started_rx.recv_timeout(Duration::from_secs(60)).unwrap(),
            "quiet"
        );
        release_tx.send(()).unwrap();
        // An unknown name is the coordinator's and never speculated.
        assert!(speculations.take("never").is_none());
        speculations.request("never");
        // Blocked speculations retry once ready.
        speculations.request("blocked");
        assert_eq!(
            started_rx.recv_timeout(Duration::from_secs(60)).unwrap(),
            "blocked"
        );
        release_tx.send(()).unwrap();
        let deadline = std::time::Instant::now() + Duration::from_secs(60);
        while !matches!(
            lock(&speculations.shared.state).entries.get("blocked"),
            Some(Entry::Blocked { .. })
        ) {
            assert!(std::time::Instant::now() < deadline);
            std::thread::sleep(Duration::from_millis(5));
        }
        speculations.retry(&|error| error == "needs other");
        assert_eq!(
            started_rx.recv_timeout(Duration::from_secs(60)).unwrap(),
            "blocked"
        );
        release_tx.send(()).unwrap();
        speculations.stop();
        assert!(started_rx.try_recv().is_err(), "'never' was not speculated");
        let state = lock(&speculations.shared.state);
        assert!(matches!(state.entries.get("quiet"), Some(Entry::Published)));
        assert!(
            matches!(state.entries.get("blocked"), None | Some(Entry::Failed)),
            "never retried after the stop"
        );
    }

    /// Stopping the pool cancels the speculation in flight (it sees
    /// [`cancelled`]) and drops what is queued.
    #[test]
    fn stop_cancels_running_and_drops_queued() {
        let speculations = Speculations::default();
        let (started_tx, started_rx) = mpsc::channel::<()>();
        let started_tx = Mutex::new(started_tx);
        let job: Job = Arc::new(move |_: &str| {
            lock(&started_tx).send(()).unwrap();
            let deadline = std::time::Instant::now() + Duration::from_secs(60);
            while !cancelled() {
                assert!(std::time::Instant::now() < deadline, "never cancelled");
                std::thread::sleep(Duration::from_millis(1));
            }
            // A cancelled download or subprocess fails the job.
            Finished::Failed
        });
        speculations.start(1, job, Arc::new(|| {}), 0);
        speculations.request("a");
        speculations.request("b");
        started_rx.recv_timeout(Duration::from_secs(60)).unwrap();
        speculations.stop();
        assert!(started_rx.try_recv().is_err(), "b never started");
        assert!(
            speculations.take("a").is_none(),
            "a cancelled: the coordinator's"
        );
        assert!(speculations.take("b").is_none());
    }
}
