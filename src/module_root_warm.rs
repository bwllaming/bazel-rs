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
}

/// One repository set's warm-ups (crate::repository).
#[derive(Default)]
pub(crate) struct RootWarmer {
    state: Arc<Mutex<State>>,
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
    /// Accept requests with at most `width` workers (0 or 1: off — one
    /// loading thread is the sequential engine).
    pub(crate) fn enable(&self, width: usize) {
        lock(&self.state).width = if width <= 1 || switched_off() {
            0
        } else {
            width.min(WORKERS)
        };
    }

    pub(crate) fn enabled(&self) -> bool {
        lock(&self.state).width > 0
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
        grow(&self.state, set);
    }
}

/// Start workers until every queued job has one (at most the width).
/// Workers end when the queue is empty, so an idle pool holds nothing.
fn grow(state: &Arc<Mutex<State>>, set: &RepositorySet) {
    let spawn = {
        let mut guard = lock(state);
        let wanted = (guard.queue.len() + guard.running).min(guard.width);
        let spawn = wanted.saturating_sub(guard.running);
        guard.running += spawn;
        spawn
    };
    for _ in 0..spawn {
        let shared = state.clone();
        let set = set.clone();
        let spawned = std::thread::Builder::new()
            .name("bazel-rs-warm".to_owned())
            .stack_size(WORKER_STACK)
            .spawn(move || worker(&shared, &set));
        if spawned.is_err() {
            lock(state).running -= 1;
        }
    }
}

/// Queue `label`'s file: with its root's warm-up when nobody started it,
/// to be read once its root is there, or at once when it is.
fn enqueue(state: &mut State, set: &RepositorySet, label: &Label) {
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

fn worker(state: &Arc<Mutex<State>>, set: &RepositorySet) {
    loop {
        let job = {
            let mut guard = lock(state);
            match guard.queue.pop_front() {
                Some(job) if guard.width > 0 => job,
                _ => {
                    guard.running -= 1;
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
            found.extend(set.crawl_loads(&job.repository, &root, package, name));
        }
        {
            let mut guard = lock(state);
            for label in found {
                enqueue(&mut guard, set, &label);
            }
        }
        grow(state, set);
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
mod tests {
    use super::*;

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
