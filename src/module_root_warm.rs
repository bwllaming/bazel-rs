//! Module roots fetched ahead of their sequential demand (docs/PARALLEL_ANALYSIS.md,
//! "Module roots fetched ahead").
//!
//! Bazel evaluates the `load()` statements of a `.bzl` or BUILD file as one
//! Skyframe batch (BzlLoadFunction.getLoadLabels → `env.getValuesAndExceptions`),
//! so the repositories a file loads from are fetched in parallel, and so are
//! the patterns of `register_toolchains` (RegisteredToolchainsFunction expands
//! them together). The native loader demands roots one at a time; on a cold
//! cache each registry module root costs a download and an extraction
//! (~0.2–2.8 s on ~/code), one after another on the coordinator.
//!
//! This pool only *warms the repository store*: for a registry module the
//! sequential engine is about to demand (the loads of a file it enters, the
//! registered toolchain patterns), it publishes the module's tree into the
//! content-addressed store ahead of time (crate::materializer's
//! `warm_with_registry_config`) and follows the `load()` statements of the
//! files it was asked for, transitively, into further registry modules —
//! the load closure Bazel fetches in parallel. It never fills a root's
//! demand slot, never records a registry file for the lockfile
//! (`SessionHandle::peek_object`), and observes nothing for the analysis
//! snapshot: the sequential demand materializes the root exactly as
//! before, only finding the tree published (and then returns it as the
//! cold materialization does, crate::materializer's `WARMED`). A warm-up
//! that fails leaves nothing; the demand fails (or succeeds) on its own.
//! So output, errors, lockfile and recordings do not depend on it — only
//! when downloads happen.

use std::collections::{HashMap, HashSet, VecDeque};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use crate::label::Label;
use crate::repository::RepositorySet;

/// At most this many warm-ups run at once (Bazel's
/// `--http_max_parallel_downloads` default).
const WORKERS: usize = 8;

/// The worker stack size (parsing nests).
const WORKER_STACK: usize = 4 << 20;

/// A file of a repository: its package and name (a `.bzl` file's
/// relative loads resolve in its package).
type File = (String, String);

/// What a worker does next: warm `repository`'s root, then read the
/// `load()` statements of `files`.
struct Job {
    repository: String,
    files: Vec<File>,
}

enum Root {
    /// A worker warms it; files to read once it is there.
    Warming(Vec<File>),
    Ready(PathBuf),
    /// Not a registry module, or its warm-up failed: nothing to follow.
    Unavailable,
}

#[derive(Default)]
struct State {
    /// The worker bound; 0 while off.
    width: usize,
    queue: VecDeque<Job>,
    roots: HashMap<String, Root>,
    /// Files whose loads were (or are being) followed, by repository.
    files: HashSet<(String, File)>,
    running: usize,
    cancel: Arc<AtomicBool>,
}

type Workers = Arc<Mutex<Vec<std::thread::JoinHandle<()>>>>;

/// One repository set's warm-ups (crate::repository).
#[derive(Default)]
pub(crate) struct RootWarmer {
    state: Arc<Mutex<State>>,
    workers: Workers,
}

/// Invocation-owned independently of the intentionally leaked analysis graph.
/// Dropping it closes admission, cancels work, and joins every worker so owned
/// private stages have run their destructors before the process can exit.
pub struct Scope {
    state: Arc<Mutex<State>>,
    workers: Workers,
}

impl Drop for Scope {
    fn drop(&mut self) {
        let handles = {
            // Same lock order as grow; no worker can be spawned after this.
            let mut workers = self.workers.lock().unwrap_or_else(PoisonError::into_inner);
            let mut state = lock(&self.state);
            state.width = 0;
            state.cancel.store(true, Ordering::Release);
            state.queue.clear();
            std::mem::take(&mut *workers)
        };
        for handle in handles {
            let _ = handle.join();
        }
    }
}

thread_local! {
    static CURRENT: std::cell::RefCell<Option<Arc<AtomicBool>>> = const { std::cell::RefCell::new(None) };
}

pub(crate) fn active() -> bool {
    CURRENT.with(|current| current.borrow().is_some())
}

pub(crate) fn cancelled() -> bool {
    CURRENT.with(|current| {
        current
            .borrow()
            .as_ref()
            .is_some_and(|cancel| cancel.load(Ordering::Acquire))
    })
}

pub(crate) fn check() -> Result<(), String> {
    if cancelled() {
        Err(crate::repository_speculation::CANCELLED.into())
    } else {
        Ok(())
    }
}

/// Cooperative local I/O boundary for a warm extraction only. It neither
/// changes archive validation nor imposes limits on foreground extraction.
pub(crate) struct Cancellable<R>(pub R);

impl<R: std::io::Read> std::io::Read for Cancellable<R> {
    fn read(&mut self, bytes: &mut [u8]) -> std::io::Result<usize> {
        check().map_err(std::io::Error::other)?;
        self.0.read(bytes)
    }
}

impl<R: std::io::Seek> std::io::Seek for Cancellable<R> {
    fn seek(&mut self, position: std::io::SeekFrom) -> std::io::Result<u64> {
        check().map_err(std::io::Error::other)?;
        self.0.seek(position)
    }
}

impl std::fmt::Debug for RootWarmer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("RootWarmer")
    }
}

fn lock(state: &Mutex<State>) -> MutexGuard<'_, State> {
    state.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Whether warm-ups are switched off for diagnostics (an A/B reference).
fn switched_off() -> bool {
    std::env::var_os("BAZEL_RS_DEBUG_NO_ROOT_WARM").is_some()
}

impl RootWarmer {
    pub(crate) fn scope(&self) -> Scope {
        Scope {
            state: self.state.clone(),
            workers: self.workers.clone(),
        }
    }
    /// Accept requests with at most `width` workers (0 or 1: off — one
    /// loading thread is the sequential engine).
    pub(crate) fn enable(&self, width: usize) {
        let mut state = lock(&self.state);
        state.width = if width <= 1 || switched_off() || state.cancel.load(Ordering::Acquire) {
            0
        } else {
            width.min(WORKERS)
        };
    }

    pub(crate) fn enabled(&self) -> bool {
        lock(&self.state).width > 0
    }

    /// Nonblocking phase ownership, not a request to finish any work. Finished
    /// handles include worker finalizers after `running` was decremented.
    pub(crate) fn quiescent(&self) -> bool {
        let Ok(workers) = self.workers.try_lock() else {
            return false;
        };
        let Ok(state) = self.state.try_lock() else {
            return false;
        };
        state.running == 0
            && state.queue.is_empty()
            && workers.iter().all(|worker| worker.is_finished())
    }

    /// Warm the roots of `files` (`.bzl` or BUILD labels) and follow their
    /// loads. Files of roots the set does not warm (the main repository,
    /// materialized roots) are still followed when their root is known.
    pub(crate) fn request(&self, set: &RepositorySet, files: impl IntoIterator<Item = Label>) {
        {
            let mut state = lock(&self.state);
            if state.width == 0 {
                return;
            }
            for label in files {
                enqueue(&mut state, set, &label);
            }
        }
        grow(&self.state, &self.workers, set);
    }
}

/// Start workers until every queued job has one (at most the width).
/// Workers end when the queue is empty, so an idle pool holds nothing.
fn grow(state: &Arc<Mutex<State>>, workers: &Workers, set: &RepositorySet) {
    let mut handles = workers.lock().unwrap_or_else(PoisonError::into_inner);
    // Workers end while idle; reap completed handles rather than retaining
    // one allocation per historical job for the whole invocation.
    let mut i = 0;
    while i < handles.len() {
        if handles[i].is_finished() {
            let _ = handles.swap_remove(i).join();
        } else {
            i += 1;
        }
    }
    let spawn = {
        let mut guard = lock(state);
        let wanted = (guard.queue.len() + guard.running).min(guard.width);
        let spawn = wanted.saturating_sub(guard.running);
        guard.running += spawn;
        spawn
    };
    for _ in 0..spawn {
        let shared = state.clone();
        let workers = workers.clone();
        let set = set.clone();
        let spawned = std::thread::Builder::new()
            .name("bazel-rs-warm".to_owned())
            .stack_size(WORKER_STACK)
            .spawn(move || {
                let cancel = lock(&shared).cancel.clone();
                CURRENT.with(|current| *current.borrow_mut() = Some(cancel.clone()));
                let _ = crate::internal_error::recoverable_asserted(|| {
                    crate::repository_speculation::with_cancel(Some(cancel), || {
                        worker(&shared, &workers, &set)
                    });
                });
                CURRENT.with(|current| *current.borrow_mut() = None);
                lock(&shared).running -= 1;
            });
        match spawned {
            Ok(handle) => handles.push(handle),
            Err(_) => lock(state).running -= 1,
        }
    }
}

/// Queue `label`'s file: with its root's warm-up when nobody started it,
/// to be read once its root is there, or at once when it is.
fn enqueue(state: &mut State, set: &RepositorySet, label: &Label) {
    if state.width == 0 {
        return;
    }
    let file = (label.package.clone(), label.name.clone());
    if !state.files.insert((label.repository.clone(), file.clone())) {
        return;
    }
    match state.roots.get_mut(&label.repository) {
        Some(Root::Warming(files)) => files.push(file),
        Some(Root::Ready(_)) => state.queue.push_back(Job {
            repository: label.repository.clone(),
            files: vec![file],
        }),
        Some(Root::Unavailable) => {}
        None => {
            if !set.warmable_root(&label.repository) {
                state
                    .roots
                    .insert(label.repository.clone(), Root::Unavailable);
                return;
            }
            state
                .roots
                .insert(label.repository.clone(), Root::Warming(Vec::new()));
            state.queue.push_back(Job {
                repository: label.repository.clone(),
                files: vec![file],
            });
        }
    }
}

fn worker(state: &Arc<Mutex<State>>, workers: &Workers, set: &RepositorySet) {
    loop {
        let job = {
            let mut guard = lock(state);
            match guard.queue.pop_front() {
                Some(job) if guard.width > 0 => job,
                _ => {
                    return;
                }
            }
        };
        let ready = match lock(state).roots.get(&job.repository) {
            Some(Root::Ready(root)) => Some(root.clone()),
            _ => None,
        };
        let root = match ready {
            Some(root) => Some(root),
            None => {
                let _span = crate::timeline::span("module-root-warm", || job.repository.clone());
                set.warm_root(&job.repository)
            }
        };
        let mut files = job.files;
        {
            let mut guard = lock(state);
            let entry = match &root {
                Some(root) => Root::Ready(root.clone()),
                None => Root::Unavailable,
            };
            if let Some(Root::Warming(waiting)) = guard.roots.insert(job.repository.clone(), entry)
            {
                files.extend(waiting);
            }
        }
        let Some(root) = root else {
            continue;
        };
        let mut found = Vec::new();
        for (package, name) in &files {
            if cancelled() {
                return;
            }
            found.extend(set.crawl_loads(&job.repository, &root, package, name));
        }
        {
            let mut guard = lock(state);
            for label in found {
                enqueue(&mut guard, set, &label);
            }
        }
        grow(state, workers, set);
    }
}

/// The labels a `.bzl` or BUILD file loads, resolved with `mapping` (the
/// file's repository's base mapping): unparsable files and loads through
/// names only an extension provides are skipped.
pub(crate) fn file_loads(
    path: &std::path::Path,
    repository: &str,
    package: &str,
    mapping: &std::collections::BTreeMap<String, String>,
) -> Vec<Label> {
    use starlark::syntax::{AstModule, Dialect};
    // snapshot-unrecorded: a hint only; the loader reads (and records) the
    // file itself when the build loads it.
    let Ok(bytes) = std::fs::read(path) else {
        return Vec::new();
    };
    let Ok(source) = String::from_utf8(bytes) else {
        return Vec::new();
    };
    let dialect = Dialect {
        enable_keyword_only_arguments: true,
        enable_load_reexport: false,
        ..Dialect::Extended
    };
    let Ok(ast) = AstModule::parse(&path.to_string_lossy(), source, &dialect) else {
        return Vec::new();
    };
    ast.loads()
        .into_iter()
        .filter_map(|load| Label::parse_in(load.module_id, repository, package, mapping).ok())
        .collect()
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    #[test]
    fn phase_boundary_requires_finished_workers_and_no_pending_jobs() {
        let warmer = RootWarmer::default();
        assert!(warmer.quiescent());
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        warmer
            .workers
            .lock()
            .unwrap()
            .push(std::thread::spawn(move || {
                release_rx
                    .recv_timeout(std::time::Duration::from_secs(60))
                    .unwrap();
            }));
        assert_eq!(lock(&warmer.state).running, 0);
        assert!(!warmer.quiescent(), "worker finalization still owns output");
        release_tx.send(()).unwrap();
        for worker in std::mem::take(&mut *warmer.workers.lock().unwrap()) {
            worker.join().unwrap();
        }
        assert!(warmer.quiescent());
        lock(&warmer.state).queue.push_back(Job {
            repository: "pending".into(),
            files: Vec::new(),
        });
        assert!(!warmer.quiescent());
        lock(&warmer.state).queue.clear();
        let _guard = warmer.workers.lock().unwrap();
        assert!(
            !warmer.quiescent(),
            "busy ownership lock never blocks lookup"
        );
    }

    #[test]
    fn shutdown_cancels_stalled_overlay_headers_and_body_and_discards_queue() {
        use base64::Engine as _;
        use sha2::{Digest, Sha256};
        use std::io::{Read, Write};
        use std::time::{Duration, Instant};
        for body_started in [false, true] {
            let root = tempfile::tempdir().unwrap();
            let archive =
                include_bytes!("../tests/fixtures/build_settings/bazel-skylib-1.8.2.tar.gz");
            let digest = Sha256::digest(archive);
            let downloads = root.path().join("downloads/sha256");
            std::fs::create_dir_all(&downloads).unwrap();
            std::fs::write(downloads.join(format!("{digest:x}")), archive).unwrap();
            let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
            let url = format!("http://{}", listener.local_addr().unwrap());
            let (started, pending) = std::sync::mpsc::channel();
            let server = std::thread::spawn(move || {
                let (mut stream, _) = listener.accept().unwrap();
                stream
                    .set_read_timeout(Some(Duration::from_secs(3)))
                    .unwrap();
                let mut request = Vec::new();
                while !request.ends_with(b"\r\n\r\n") {
                    let mut byte = [0];
                    stream.read_exact(&mut byte).unwrap();
                    request.push(byte[0]);
                }
                assert!(
                    String::from_utf8_lossy(&request)
                        .starts_with("GET /modules/dep/1/overlay/pending.txt ")
                );
                if body_started {
                    stream.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 100000\r\nConnection: close\r\n\r\nx").unwrap();
                }
                started.send(()).unwrap();
                // Cancellation must drop the request/response, not detach a
                // still-active transfer until the ordinary 20s read timeout.
                assert_eq!(stream.read(&mut [0]).unwrap(), 0);
            });
            let source = serde_json::json!({
                "url": format!("{url}/source.tar.gz"),
                "integrity": format!("sha256-{}", base64::engine::general_purpose::STANDARD.encode(digest)),
                "overlay": {"pending.txt": format!("sha256-{}", base64::engine::general_purpose::STANDARD.encode(Sha256::digest(b"pending")))},
            });
            let warmer = RootWarmer::default();
            warmer.enable(2);
            let scope = warmer.scope();
            let cancel = lock(&warmer.state).cancel.clone();
            let cache = root.path().to_owned();
            let (done, finished) = std::sync::mpsc::channel();
            let worker = std::thread::spawn(move || {
                CURRENT.with(|current| *current.borrow_mut() = Some(cancel));
                let error = crate::materializer::warm_with_registry_config(
                    &crate::registry::Registry::parse(&url).unwrap(),
                    "dep",
                    "1",
                    b"module(name='dep')\n",
                    source.as_object().unwrap(),
                    &cache,
                    &crate::patch_inputs::LocalPatchPlan::default(),
                    true,
                    None,
                )
                .unwrap_err();
                assert!(
                    error.contains(crate::repository_speculation::CANCELLED),
                    "{error}"
                );
                done.send(()).unwrap();
            });
            warmer.workers.lock().unwrap().push(worker);
            pending.recv_timeout(Duration::from_secs(5)).unwrap();
            let trees = root.path().join("trees");
            let entries: Vec<_> = std::fs::read_dir(&trees)
                .unwrap()
                .map(|e| e.unwrap().file_name())
                .collect();
            assert_eq!(entries.len(), 1, "{entries:?}");
            assert!(entries[0].to_string_lossy().starts_with(".stage-"));
            lock(&warmer.state).queue.push_back(Job {
                repository: "not-admitted".into(),
                files: Vec::new(),
            });
            let start = Instant::now();
            drop(scope);
            finished.recv_timeout(Duration::from_secs(1)).unwrap();
            server.join().unwrap();
            assert!(
                start.elapsed() < Duration::from_secs(2),
                "shutdown took {:?}",
                start.elapsed()
            );
            assert!(warmer.workers.lock().unwrap().is_empty());
            assert!(lock(&warmer.state).queue.is_empty());
            // Terminal stop: late workers/callers cannot enqueue or re-enable.
            warmer.enable(8);
            let set = RepositorySet::new(root.path().to_owned());
            warmer.request(&set, [Label::parse_canonical("//:late.bzl").unwrap()]);
            assert!(!warmer.enabled());
            assert!(lock(&warmer.state).queue.is_empty());
            assert!(std::fs::read_dir(trees).unwrap().next().is_none());
            let remaining: Vec<_> = std::fs::read_dir(&downloads)
                .unwrap()
                .map(|e| e.unwrap().file_name())
                .collect();
            assert_eq!(
                remaining,
                [std::ffi::OsString::from(format!("{digest:x}"))],
                "no incomplete download blob may survive shutdown"
            );
        }
    }

    #[test]
    fn shutdown_cancels_even_an_unlimited_credential_helper() {
        use std::os::unix::fs::PermissionsExt;
        use std::time::{Duration, Instant};
        // Credentials are invocation-global. Isolate the installed helper
        // and verify that cancellation adds no warning to command stderr.
        if std::env::var_os("BAZEL_RS_UNIT_WARM_HELPER_CHILD").is_none() {
            let output = crate::spawn_lock::output(std::process::Command::new("timeout")
                .args(["--kill-after=2s", "10s"])
                .arg(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "module_root_warm::tests::shutdown_cancels_even_an_unlimited_credential_helper",
                    "--nocapture",
                ])
                .env("BAZEL_RS_UNIT_WARM_HELPER_CHILD", "1"))
                .unwrap();
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
            assert!(
                !String::from_utf8_lossy(&output.stderr).contains("WARNING:"),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
            return;
        }
        let root = tempfile::tempdir().unwrap();
        let script = root.path().join("helper");
        let pid_path = root.path().join("pid");
        std::fs::write(
            &script,
            format!(
                "#!/bin/sh\necho $$ > '{}'\nexec /bin/sleep 30\n",
                pid_path.display()
            ),
        )
        .unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        crate::credentials::install(
            crate::credentials::Credentials::new(
                &crate::credentials::CredentialOptions {
                    helpers: vec![crate::credentials::HelperOption {
                        scope: None,
                        path: script.display().to_string(),
                    }],
                    timeout: Duration::ZERO,
                    ..Default::default()
                },
                None,
                "/usr/bin:/bin",
            )
            .unwrap(),
        );
        let warmer = RootWarmer::default();
        warmer.enable(2);
        let scope = warmer.scope();
        let cancel = lock(&warmer.state).cancel.clone();
        let (done, finished) = std::sync::mpsc::channel();
        warmer
            .workers
            .lock()
            .unwrap()
            .push(std::thread::spawn(move || {
                CURRENT.with(|current| *current.borrow_mut() = Some(cancel));
                let headers = crate::credentials::download_headers("http://127.0.0.1/");
                assert!(headers.is_empty());
                assert!(cancelled());
                done.send(()).unwrap();
            }));
        let deadline = Instant::now() + Duration::from_secs(3);
        while !pid_path.exists() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(5));
        }
        let pid = std::fs::read_to_string(pid_path).unwrap();
        let start = Instant::now();
        drop(scope);
        finished.recv_timeout(Duration::from_secs(1)).unwrap();
        assert!(start.elapsed() < Duration::from_secs(2));
        assert!(
            !std::path::Path::new(&format!("/proc/{}", pid.trim())).exists(),
            "helper survived shutdown"
        );
    }

    #[derive(Default)]
    struct Trace {
        before_lock: bool,
        foreground_failed: bool,
        stage: Option<PathBuf>,
    }

    #[derive(Default)]
    struct Schedule {
        trace: Mutex<Trace>,
        changed: std::sync::Condvar,
    }

    static SCHEDULE: std::sync::OnceLock<Schedule> = std::sync::OnceLock::new();

    /// Test-binary-only scheduling: the warmer arrives before the foreground
    /// attempt, but may take the publication lock only after that attempt
    /// failed. Then hold its private stage across invocation shutdown.
    pub(crate) fn schedule(warming: bool, stage: Option<&std::path::Path>) {
        let Some(schedule) = SCHEDULE.get().filter(|_| warming) else {
            return;
        };
        let mut trace = schedule.trace.lock().unwrap();
        if let Some(stage) = stage {
            trace.stage = Some(stage.to_owned());
            schedule.changed.notify_all();
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
            while !crate::repository_speculation::cancelled()
                && std::time::Instant::now() < deadline
            {
                trace = schedule
                    .changed
                    .wait_timeout(trace, std::time::Duration::from_millis(10))
                    .unwrap()
                    .0;
            }
        } else {
            trace.before_lock = true;
            schedule.changed.notify_all();
            while !trace.foreground_failed {
                let (next, timeout) = schedule
                    .changed
                    .wait_timeout(trace, std::time::Duration::from_secs(5))
                    .unwrap();
                assert!(!timeout.timed_out(), "foreground did not release warm-up");
                trace = next;
            }
        }
    }

    #[test]
    fn failed_invocation_stage_child() {
        let Some(root) = std::env::var_os("BAZEL_RS_UNIT_WARM_STAGE_CHILD").map(PathBuf::from)
        else {
            return;
        };
        let schedule = SCHEDULE.get_or_init(Schedule::default);
        let registry = root.join("registry");
        let module_dir = registry.join("modules/dep/1");
        std::fs::create_dir_all(&module_dir).unwrap();
        std::fs::write(
            root.join("MODULE.bazel"),
            "module(name='main')\nbazel_dep(name='dep', version='1')\n",
        )
        .unwrap();
        std::fs::write(
            module_dir.join("MODULE.bazel"),
            "module(name='dep', version='1')\n",
        )
        .unwrap();
        let archive = include_bytes!("../tests/fixtures/build_settings/bazel-skylib-1.8.2.tar.gz");
        use base64::Engine as _;
        use sha2::{Digest, Sha256};
        let digest = Sha256::digest(archive);
        let cache = root.join("cache");
        let downloads = cache.join("downloads/sha256");
        std::fs::create_dir_all(&downloads).unwrap();
        std::fs::write(downloads.join(format!("{digest:x}")), archive).unwrap();
        let source = serde_json::json!({
            "url": "http://127.0.0.1:1/source.tar.gz",
            "integrity": format!("sha256-{}", base64::engine::general_purpose::STANDARD.encode(digest)),
            "overlay": {"missing.txt": format!("sha256-{}", base64::engine::general_purpose::STANDARD.encode(Sha256::digest(b"missing")))},
        });
        std::fs::write(module_dir.join("source.json"), source.to_string()).unwrap();
        let set = RepositorySet::with_options(
            root.clone(),
            crate::module_graph::ResolverOptions {
                registries: vec![crate::registry::Registry::LocalPath(registry)],
                ..Default::default()
            },
        )
        .with_source_cache(cache)
        .with_repository_downloads_enabled(false);
        set.resolve_graph().unwrap();
        let _invocation = set.root_warming_scope();
        set.enable_root_warming(2);
        set.warm_loads(&[Label::parse_canonical("@@dep+//:version.bzl").unwrap()]);
        let mut trace = schedule.trace.lock().unwrap();
        while !trace.before_lock {
            let (next, timeout) = schedule
                .changed
                .wait_timeout(trace, std::time::Duration::from_secs(5))
                .unwrap();
            assert!(
                !timeout.timed_out(),
                "warmer did not reach publication lock"
            );
            trace = next;
        }
        drop(trace);
        let error = set.root("dep+").unwrap_err();
        assert!(error.contains("--repository_disable_download"), "{error}");
        assert!(
            std::fs::read_dir(root.join("cache/trees"))
                .unwrap()
                .next()
                .is_none()
        );
        let mut trace = schedule.trace.lock().unwrap();
        trace.foreground_failed = true;
        schedule.changed.notify_all();
        while trace.stage.is_none() {
            let (next, timeout) = schedule
                .changed
                .wait_timeout(trace, std::time::Duration::from_secs(5))
                .unwrap();
            assert!(
                !timeout.timed_out(),
                "second materialization did not reach stage"
            );
            trace = next;
        }
        std::fs::write(
            root.join("stage-evidence.txt"),
            trace.stage.as_ref().unwrap().display().to_string(),
        )
        .unwrap();
        drop(trace);
        // Normal invocation return. Before the fix, the detached warm worker
        // still owns its stage when the test process exits.
    }

    #[test]
    fn failed_invocation_cleans_active_warm_stage() {
        let root = tempfile::tempdir().unwrap();
        let output = crate::spawn_lock::output(
            std::process::Command::new("timeout")
                .args(["--kill-after=2s", "15s"])
                .arg(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "module_root_warm::tests::failed_invocation_stage_child",
                    "--nocapture",
                ])
                .env("BAZEL_RS_UNIT_WARM_STAGE_CHILD", root.path())
                .env_remove("BAZEL_RS_DEBUG_NO_ROOT_WARM"),
        )
        .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let stage = std::fs::read_to_string(root.path().join("stage-evidence.txt")).unwrap();
        let remaining = std::fs::read_dir(root.path().join("cache/trees"))
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect::<Vec<_>>();
        eprintln!(
            "controlled lifecycle: foreground failed; warmer then owned {stage}; after child exit={remaining:?}"
        );
        assert!(
            remaining.is_empty(),
            "private warm stage survived normal failed invocation: {remaining:?}; stage={stage}"
        );
    }

    #[test]
    fn file_loads_resolve_with_the_base_mapping_and_skip_unknown_names() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("defs.bzl");
        std::fs::write(
            &path,
            "load(\"@dep//x:a.bzl\", \"a\")\nload(\":b.bzl\", \"b\")\nload(\"@ext//:c.bzl\", \"c\")\nload(\"//y:d.bzl\", \"d\")\n",
        )
        .unwrap();
        let mapping = std::collections::BTreeMap::from([
            ("dep".to_owned(), "dep+".to_owned()),
            ("me".to_owned(), "me+".to_owned()),
        ]);
        let loads = file_loads(&path, "me+", "p", &mapping);
        let shown: Vec<String> = loads
            .iter()
            .map(|label| format!("{}|{}|{}", label.repository, label.package, label.name))
            .collect();
        assert_eq!(
            shown,
            ["dep+|x|a.bzl", "me+|p|b.bzl", "me+|y|d.bzl"],
            "the extension-only name is skipped"
        );
        assert!(file_loads(&directory.path().join("missing.bzl"), "", "", &mapping).is_empty());
    }
}
