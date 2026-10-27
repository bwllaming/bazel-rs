//! Build without the Bytes (`--remote_download_outputs=minimal|toplevel`,
//! docs/REMOTE_CACHE.md "Build without the Bytes"): which outputs of
//! remote cache hits and remote executions stay in the remote CAS as
//! metadata only, and the process-wide registry of such remote-only
//! outputs.
//!
//! Pinned Bazel source (`src/main/java/com/google/devtools/build/lib/`):
//! `remote/RemoteOutputChecker.java` (what to download: everything with
//! `all`; with `toplevel` the requested targets' artifacts, and in `run`
//! always; `--remote_download_regex`; a test target's own outputs never in
//! `test`), `remote/RemoteExecutionService.java:1257`
//! (`--experimental_remote_cache_ttl` as the metadata's expiration),
//! `actions/ActionCacheChecker.java` (an output that should be downloaded
//! but is missing reruns its action) and `remote/RemoteActionInputFetcher`
//! (a local consumer fetches its remote inputs first: crate::action's
//! materialization).
//!
//! A remote-only output is recorded in its action's cache entry
//! (crate::action's manifest, digest, size and expiration) without a file;
//! the registry maps its published path to its digest so consumers key and
//! stage it without reading it.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{OnceLock, RwLock};
use std::time::{Duration, SystemTime};

/// A remote-only output: its digest (SHA-256, lowercase hex) and size.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RemoteFile {
    pub hash: String,
    pub size: u64,
}

struct Registry {
    /// By published path (`<cache>/actions/<key>/outputs/<exec path>`).
    by_path: HashMap<PathBuf, RemoteFile>,
    /// By exec path (the build event files' URIs).
    by_exec: HashMap<PathBuf, RemoteFile>,
    /// The published paths of each action entry (`<cache>/actions/<key>`).
    by_entry: HashMap<PathBuf, Vec<PathBuf>>,
}

/// The action entry of a published path: the ancestor whose parent is
/// `actions`.
pub fn entry_of(path: &Path) -> Option<&Path> {
    path.ancestors().find(|ancestor| {
        ancestor
            .parent()
            .and_then(Path::file_name)
            .is_some_and(|name| name == "actions")
    })
}

static ANY: AtomicBool = AtomicBool::new(false);

fn registry() -> &'static RwLock<Registry> {
    static REGISTRY: OnceLock<RwLock<Registry>> = OnceLock::new();
    REGISTRY.get_or_init(|| {
        RwLock::new(Registry {
            by_path: HashMap::new(),
            by_exec: HashMap::new(),
            by_entry: HashMap::new(),
        })
    })
}

/// Records the remote-only output published at `path` (exec path `exec`).
pub fn register(path: PathBuf, exec: PathBuf, file: RemoteFile) {
    let mut registry = registry().write().unwrap_or_else(|e| e.into_inner());
    registry.by_exec.insert(exec, file.clone());
    if let Some(entry) = entry_of(&path) {
        registry
            .by_entry
            .entry(entry.to_path_buf())
            .or_default()
            .push(path.clone());
    }
    registry.by_path.insert(path, file);
    ANY.store(true, Ordering::Relaxed);
}

/// The remote-only output published at `path`, if it is one.
#[inline]
pub fn get(path: &Path) -> Option<RemoteFile> {
    if !ANY.load(Ordering::Relaxed) {
        return None;
    }
    registry()
        .read()
        .unwrap_or_else(|e| e.into_inner())
        .by_path
        .get(path)
        .cloned()
}

/// The remote-only output at exec path `exec` in this build.
pub fn by_exec(exec: &Path) -> Option<RemoteFile> {
    if !ANY.load(Ordering::Relaxed) {
        return None;
    }
    registry()
        .read()
        .unwrap_or_else(|e| e.into_inner())
        .by_exec
        .get(exec)
        .cloned()
}

/// `path` was materialized (downloaded into place).
pub fn materialized(path: &Path, exec: Option<&Path>) {
    let mut registry = registry().write().unwrap_or_else(|e| e.into_inner());
    registry.by_path.remove(path);
    if let Some(exec) = exec {
        registry.by_exec.remove(exec);
    }
}

/// The action entry `entry` is about to be published again: forget its
/// remote-only outputs.
pub fn forget_entry(entry: &Path) {
    if !ANY.load(Ordering::Relaxed) {
        return;
    }
    let mut registry = registry().write().unwrap_or_else(|e| e.into_inner());
    if let Some(paths) = registry.by_entry.remove(entry) {
        for path in paths {
            registry.by_path.remove(&path);
        }
    }
}

/// RemoteOutputsMode.
pub use crate::remote_cache::DownloadOutputs as Mode;

/// What this command downloads (RemoteOutputChecker).
pub struct Policy {
    pub mode: Mode,
    /// `--remote_download_regex` (whole exec paths, Java `matches`).
    pub patterns: Vec<fancy_regex::Regex>,
    /// `--experimental_remote_cache_ttl`.
    pub ttl: Duration,
    /// The outputs (action id, output index) to download: the requested
    /// targets' artifacts (`toplevel`, and always in `run`).
    pub download: RwLock<HashSet<(String, usize)>>,
    /// `--experimental_remote_cache_eviction_retries`.
    pub retries: u32,
}

static POLICY: OnceLock<Policy> = OnceLock::new();

/// Installs the command's policy (with a remote cache, from main).
pub fn install(policy: Policy) {
    let _ = POLICY.set(policy);
}

/// The policy, when outputs may stay remote: a remote cache is configured
/// and the mode is not `all`.
pub fn policy() -> Option<&'static Policy> {
    POLICY
        .get()
        .filter(|policy| policy.mode != Mode::All && crate::remote_cache::active().is_some())
}

/// The planned request's outputs to download (crate::request).
pub fn set_download(outputs: HashSet<(String, usize)>) {
    if let Some(policy) = POLICY.get() {
        *policy.download.write().unwrap_or_else(|e| e.into_inner()) = outputs;
    }
}

/// Whether output `index` (exec path `exec`) of action `id` may stay in the
/// remote CAS (RemoteOutputChecker.shouldDownloadOutput, negated).
pub fn may_stay_remote(id: &str, index: usize, exec: &Path) -> bool {
    let Some(policy) = policy() else {
        return false;
    };
    if policy
        .download
        .read()
        .unwrap_or_else(|e| e.into_inner())
        .contains(&(id.to_owned(), index))
    {
        return false;
    }
    let Some(text) = exec.to_str() else {
        return false;
    };
    !policy
        .patterns
        .iter()
        .any(|pattern| pattern.is_match(text).unwrap_or(false))
}

/// The expiration of metadata received now (RemoteExecutionService: now
/// plus `--experimental_remote_cache_ttl`), in Unix milliseconds.
pub fn expiration() -> u64 {
    let ttl = policy().map_or(Duration::from_secs(3 * 3600), |policy| policy.ttl);
    (SystemTime::now() + ttl)
        .duration_since(SystemTime::UNIX_EPOCH)
        .map_or(0, |since| since.as_millis() as u64)
}

/// Whether metadata expiring at `expires` (Unix milliseconds) is alive.
pub fn alive(expires: u64) -> bool {
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .is_ok_and(|now| (now.as_millis() as u64) < expires)
}

/// RegexPatternConverter for `--remote_download_regex`: the whole exec
/// path must match.
pub fn pattern(option: &str, text: &str) -> Result<fancy_regex::Regex, String> {
    fancy_regex::Regex::new(&format!("^(?:{text})$")).map_err(|error| {
        format!("While parsing option --{option}={text}: invalid regular expression: {error}")
    })
}

/// Set when a remote-only input was gone from the remote CAS: the command
/// is retried (`--experimental_remote_cache_eviction_retries`).
static LOST: AtomicBool = AtomicBool::new(false);

pub fn note_lost() {
    LOST.store(true, Ordering::Relaxed);
    crate::remote_cache::note_remote_failure();
}

pub fn lost() -> bool {
    LOST.load(Ordering::Relaxed)
}

/// The environment variable carrying the attempt number of a retried
/// command (BlazeCommandDispatcher's attemptNumber).
pub const ATTEMPT: &str = "BAZEL_RS_REMOTE_ATTEMPT";

/// After a lost remote input: whether this command may run once more
/// (`--experimental_remote_cache_eviction_retries` attempts after the
/// first), and the next attempt's number.
pub fn next_attempt() -> Option<u32> {
    if !lost() {
        return None;
    }
    let retries = POLICY.get().map_or(5, |policy| policy.retries);
    // snapshot-unrecorded: the retry counter of a failed execution, read
    // after planning.
    let attempt = std::env::var(ATTEMPT)
        .ok()
        .and_then(|value| value.parse::<u32>().ok())
        .unwrap_or(1);
    (attempt <= retries).then_some(attempt + 1)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn patterns_match_whole_exec_paths_and_the_registry_forgets_entries() {
        let pattern = pattern("remote_download_regex", ".*\\.txt").unwrap();
        assert!(pattern.is_match("bazel-out/k8/bin/a.txt").unwrap());
        assert!(!pattern.is_match("bazel-out/k8/bin/a.txt.o").unwrap());
        assert!(super::pattern("remote_download_regex", "(").is_err());
        let file = RemoteFile {
            hash: "ab".into(),
            size: 1,
        };
        register(
            PathBuf::from("/c/actions/k/outputs/x"),
            PathBuf::from("x"),
            file.clone(),
        );
        assert_eq!(get(Path::new("/c/actions/k/outputs/x")), Some(file.clone()));
        assert_eq!(by_exec(Path::new("x")), Some(file));
        forget_entry(Path::new("/c/actions/k"));
        assert_eq!(get(Path::new("/c/actions/k/outputs/x")), None);
        assert!(alive(expiration()));
        assert!(!alive(1));
    }
}
