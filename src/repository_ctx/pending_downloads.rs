//! Concurrent `download(block = False)` (docs/MODULE_EXTENSIONS.md,
//! "Non-blocking downloads").
//!
//! Bazel runs a non-blocking download on its downloader's executor and the
//! callback continues (StarlarkBaseExternalContext.java:585-697), bounded by
//! `--http_max_parallel_downloads` (default 8). rules_python's `pip`
//! extension issues one non-blocking Simple-API request per package and only
//! then waits for them all, so a sequential transfer costs it one round trip
//! per package.
//!
//! Natively only the network transfer (and the copy into the download cache)
//! runs on a pool of [`WORKERS`] threads, into a private file next to the
//! output. Everything the callback can observe stays where the sequential
//! engine shows it: the transfer is *committed* — its retry warnings written,
//! the file renamed onto the output, the execute bits set, its result fixed —
//! in call order, before the callback observes anything else: before every
//! other context method (`context()`), every `path` method that reads the
//! file system, every `print()`, the `wait()` of this or a later download, and
//! when the callback returns (also when it fails). A non-blocking download
//! whose output is, contains or lies in a pending one's output, or that
//! expects a pending one's checksum (the download cache could serve it),
//! commits the pending ones first. Between a call and its commit the callback
//! therefore observes nothing the sequential transfer would have changed, so
//! output, stderr, errors and recorded inputs are the sequential engine's.
//! A download that needs a credential helper (whose warnings print while the
//! request is formed), a disabled downloader, an output that is a directory,
//! or `BAZEL_RS_DEBUG_NO_ASYNC_DOWNLOAD` runs at the call as before.

use std::cell::RefCell;
use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, OnceLock, PoisonError};

use super::{DownloadFailure, DownloadOutcome, cache_put_at};
use crate::extension_download::{self as transport, Checksum, TypedIoError};

/// Concurrent transfers (`--http_max_parallel_downloads`' default,
/// CommonCommandOptions.java).
pub(super) const WORKERS: usize = 8;

/// The result of a committed download, as `wait()` returns or raises it.
pub(crate) type Outcome = Result<DownloadOutcome, String>;

/// What a queued download's `wait()` reads once the download is committed.
#[derive(Debug)]
pub(crate) struct Slot {
    id: u64,
    outcome: Mutex<Option<Outcome>>,
}

impl Slot {
    fn ready(&self) -> Option<Outcome> {
        self.outcome
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }
}

/// Whether non-blocking downloads may overlap (the debug switch keeps the
/// sequential transfer, the reference of tests/parallel_download.rs).
pub(super) fn enabled() -> bool {
    static ENABLED: OnceLock<bool> = OnceLock::new();
    *ENABLED.get_or_init(|| std::env::var_os("BAZEL_RS_DEBUG_NO_ASYNC_DOWNLOAD").is_none())
}

/// The transfer a worker runs.
struct Job {
    client: reqwest::blocking::Client,
    urls: Vec<reqwest::Url>,
    expected: Option<Checksum>,
    sri: bool,
    headers: transport::RequestHeaders,
    temp: PathBuf,
    /// The download cache root and canonical id (DownloadManager.java:381-386).
    cache: Option<PathBuf>,
    canonical_id: String,
    /// The speculation's cancellation (crate::repository_speculation), when
    /// a speculative repository fetch issued the download.
    cancel: Option<Arc<AtomicBool>>,
    done: Arc<Done>,
}

/// What a worker hands back.
struct Transfer {
    fetched: Result<transport::Fetched, TypedIoError>,
    /// Every failed mirror, in order (HttpDownloader.recordFailure).
    failures: Vec<(reqwest::Url, transport::BazelFailure)>,
    /// The download cache store of a completed transfer.
    cached: Option<Result<(), String>>,
}

#[derive(Default)]
struct Done {
    transfer: Mutex<Option<Transfer>>,
    finished: Condvar,
}

impl Done {
    fn finish(&self, transfer: Transfer) {
        *self.transfer.lock().unwrap_or_else(PoisonError::into_inner) = Some(transfer);
        self.finished.notify_all();
    }

    fn wait(&self) -> Transfer {
        let mut transfer = self.transfer.lock().unwrap_or_else(PoisonError::into_inner);
        loop {
            if let Some(done) = transfer.take() {
                return done;
            }
            transfer = self
                .finished
                .wait(transfer)
                .unwrap_or_else(PoisonError::into_inner);
        }
    }
}

struct Queue {
    jobs: Mutex<VecDeque<Job>>,
    available: Condvar,
}

struct Pool {
    queue: Arc<Queue>,
    /// Workers running; with none the submitting thread transfers itself.
    workers: usize,
}

fn pool() -> &'static Pool {
    static POOL: OnceLock<Pool> = OnceLock::new();
    POOL.get_or_init(|| {
        let queue = Arc::new(Queue {
            jobs: Mutex::new(VecDeque::new()),
            available: Condvar::new(),
        });
        let workers = (0..WORKERS)
            .filter(|index| {
                let queue = queue.clone();
                std::thread::Builder::new()
                    .name(format!("bazel-rs-download-{index}"))
                    .spawn(move || work(&queue))
                    .is_ok()
            })
            .count();
        Pool { queue, workers }
    })
}

fn work(queue: &Queue) {
    loop {
        let job = {
            let mut jobs = queue.jobs.lock().unwrap_or_else(PoisonError::into_inner);
            loop {
                if let Some(job) = jobs.pop_front() {
                    break job;
                }
                jobs = queue
                    .available
                    .wait(jobs)
                    .unwrap_or_else(PoisonError::into_inner);
            }
        };
        run(job);
    }
}

fn run(job: Job) {
    let mut failures = Vec::new();
    let temp = job.temp.clone();
    let fetched = crate::repository_speculation::with_cancel(job.cancel.clone(), || {
        transport::fetch_to_file(
            &job.client,
            &job.urls,
            job.expected.as_ref(),
            job.sri,
            &job.headers,
            &mut |_| temp.clone(),
            true,
            &mut |url, failure| failures.push((url.clone(), failure.clone())),
        )
    });
    let cached = fetched.as_ref().ok().map(|fetched| {
        cache_put_at(
            job.cache.as_deref(),
            &fetched.checksum,
            &job.temp,
            &job.canonical_id,
        )
        .map_err(|error| format!("{error:#}"))
    });
    job.done.finish(Transfer {
        fetched,
        failures,
        cached,
    });
}

/// A download started by [`submit`], committed by [`flush`].
struct Entry {
    slot: Arc<Slot>,
    done: Arc<Done>,
    temp: PathBuf,
    target: PathBuf,
    expected: Option<Checksum>,
    urls: Vec<reqwest::Url>,
    output: Vec<u8>,
    allow_fail: bool,
    executable: bool,
    /// The malformed-checksum error raised after the transfer.
    deferred: Option<String>,
}

thread_local! {
    /// This thread's uncommitted downloads, in call order. A callback runs
    /// on one thread; a nested callback (a repository demanded by a context
    /// method) starts only after that method committed them.
    static PENDING: RefCell<VecDeque<Entry>> = const { RefCell::new(VecDeque::new()) };
}

static NEXT_ID: AtomicU64 = AtomicU64::new(1);
static SUBMITTED: AtomicU64 = AtomicU64::new(0);

/// The request `submit` starts.
pub(super) struct Request<'a> {
    pub client: reqwest::blocking::Client,
    pub urls: &'a [reqwest::Url],
    pub expected: Option<&'a Checksum>,
    pub sri: bool,
    pub headers: &'a transport::RequestHeaders,
    pub deferred: Option<&'a str>,
    pub target: &'a Path,
    pub output: &'a [u8],
    pub allow_fail: bool,
    pub executable: bool,
    pub cache: Option<PathBuf>,
    pub canonical_id: &'a str,
}

/// Start `request`'s transfer; the caller's `wait()` reads the slot.
pub(super) fn submit(request: Request<'_>) -> Arc<Slot> {
    let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
    SUBMITTED.fetch_add(1, Ordering::Relaxed);
    // Next to the output (one file system: renamed onto it), a short name
    // whatever the output's length.
    let temp = request
        .target
        .with_file_name(format!(".bazel-rs-download-{}-{id}", std::process::id()));
    let done = Arc::new(Done::default());
    let slot = Arc::new(Slot {
        id,
        outcome: Mutex::new(None),
    });
    let job = Job {
        client: request.client,
        urls: request.urls.to_vec(),
        expected: request.expected.cloned(),
        sri: request.sri,
        headers: request.headers.clone(),
        temp: temp.clone(),
        cache: request.cache,
        canonical_id: request.canonical_id.to_owned(),
        cancel: crate::repository_speculation::cancel_token(),
        done: done.clone(),
    };
    PENDING.with(|pending| {
        pending.borrow_mut().push_back(Entry {
            slot: slot.clone(),
            done,
            temp,
            target: request.target.to_owned(),
            expected: request.expected.cloned(),
            urls: request.urls.to_vec(),
            output: request.output.to_owned(),
            allow_fail: request.allow_fail,
            executable: request.executable,
            deferred: request.deferred.map(str::to_owned),
        })
    });
    let pool = pool();
    if pool.workers == 0 {
        // No worker thread could be started: transfer right here.
        run(job);
    } else {
        pool.queue
            .jobs
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push_back(job);
        pool.queue.available.notify_one();
    }
    slot
}

/// Commit every uncommitted download of this thread, in call order.
pub(crate) fn flush() {
    while let Some(entry) = PENDING.with(|pending| pending.borrow_mut().pop_front()) {
        commit(entry);
    }
}

/// Before a non-blocking download to `target` (expecting `expected`): commit
/// the pending downloads it could observe — an output that is, contains or
/// lies in `target`, or one that may fill the download cache entry it looks
/// up.
pub(super) fn flush_conflicting(target: &Path, expected: Option<&Checksum>) {
    let conflict = PENDING.with(|pending| {
        pending.borrow().iter().any(|entry| {
            let output = super::path_buf(&entry.output);
            entry.target.starts_with(target)
                || target.starts_with(&entry.target)
                || output.starts_with(target)
                || target.starts_with(&output)
                || expected.is_some_and(|expected| {
                    entry.expected.as_ref().is_some_and(|pending| {
                        pending.algorithm == expected.algorithm && pending.bytes == expected.bytes
                    })
                })
        })
    });
    if conflict {
        flush();
    }
}

/// `wait()`: commit through `slot`'s download, then its outcome.
pub(crate) fn wait(slot: &Slot) -> Outcome {
    if let Some(outcome) = slot.ready() {
        return outcome;
    }
    let queued = PENDING.with(|pending| {
        pending
            .borrow()
            .iter()
            .any(|entry| entry.slot.id == slot.id)
    });
    if queued {
        while let Some(entry) = PENDING.with(|pending| pending.borrow_mut().pop_front()) {
            let last = entry.slot.id == slot.id;
            commit(entry);
            if last {
                break;
            }
        }
    }
    slot.ready()
        .unwrap_or_else(|| Err("java.io.IOException: download was not completed".into()))
}

fn commit(entry: Entry) {
    let transfer = entry.done.wait();
    let mut last = None;
    for (url, failure) in &transfer.failures {
        let warning = format!(
            "WARNING: Download from {url} failed: class {} {}",
            failure.class, failure.message
        );
        if !crate::repository_speculation::defer_stderr(&crate::ui::marked(&warning)) {
            crate::ui::note(&warning);
        }
        last = Some(failure.clone());
    }
    let outcome = finish(&entry, transfer, last).map_err(|error| format!("{error:#}"));
    *entry
        .slot
        .outcome
        .lock()
        .unwrap_or_else(PoisonError::into_inner) = Some(outcome);
}

/// The rest of the sequential `download()` after its transfer.
fn finish(
    entry: &Entry,
    transfer: Transfer,
    last: Option<transport::BazelFailure>,
) -> anyhow::Result<DownloadOutcome> {
    // A mirror that responded wrote the private file: it becomes the output,
    // replacing what was there (the sequential transfer unlinks it once a
    // mirror responds and leaves a failed mirror's bytes, keep_failed).
    if std::fs::symlink_metadata(&entry.temp).is_ok() {
        std::fs::rename(&entry.temp, &entry.target)?;
    }
    let (checksum, size) = match transfer.fetched {
        Ok(fetched) => {
            match transfer.cached {
                Some(Err(_)) if entry.allow_fail => return Ok(DownloadOutcome::Failed),
                Some(Err(error)) => anyhow::bail!("{error}"),
                _ => {}
            }
            (fetched.checksum, fetched.size)
        }
        Err(error) if entry.allow_fail && matches!(error, TypedIoError::Completion(_)) => {
            return Ok(DownloadOutcome::Failed);
        }
        Err(error) => {
            return Err(DownloadFailure { error, last }.into_error(&entry.urls, &entry.output));
        }
    };
    if entry.executable {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&entry.target)?.permissions().mode();
        std::fs::set_permissions(&entry.target, std::fs::Permissions::from_mode(mode | 0o111))?;
        crate::spawn_lock::seal(&entry.target)?;
    }
    if let Some(error) = &entry.deferred {
        anyhow::bail!("{error}");
    }
    Ok(DownloadOutcome::Done(checksum, size))
}

/// Non-blocking downloads started in this process (telemetry).
pub(crate) fn submitted() -> u64 {
    SUBMITTED.load(Ordering::Relaxed)
}
