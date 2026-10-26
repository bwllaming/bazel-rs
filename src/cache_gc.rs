//! Garbage collection of the native cache (docs/CACHE_GC.md).
//!
//! Bazel 9.3 collects its disk cache (`--disk_cache`) when the server has
//! been idle for `--experimental_disk_cache_gc_idle_delay`, by size
//! (`--experimental_disk_cache_gc_max_size`) and by age
//! (`--experimental_disk_cache_gc_max_age`), least recently used first
//! (remote/disk/DiskCacheGarbageCollector.java,
//! DiskCacheGarbageCollectorIdleTask.java), and its repo contents cache by
//! age (`--repo_contents_cache_gc_max_age`, 14 days), under a cache-wide
//! lock that every command holds shared and the collector takes exclusively
//! without waiting, moving garbage into a trash directory that is emptied
//! afterwards (bazel/repository/cache/LocalRepoContentsCache.java:62-78,
//! 187-275).
//!
//! The native cache is all of these at once (action cache, CAS, repository
//! trees and downloads, test results, per-worktree state) and has no server:
//!
//! * every command holds a flock of the cache directory shared from the moment it knows
//!   its cache until it exits ([`install`]);
//! * at the end of a command ([`finish`]), when the last collection is at
//!   least the idle delay old, the collector takes the lock exclusively
//!   without waiting — only when no other bazel-rs process uses the cache —
//!   and collects (crate::cache_gc::collect), in bounded time;
//! * hits record when an entry was last used ([`note_used`],
//!   [`note_tree_used`]), at most once per [`Policy::recent`] per entry;
//! * `bazel-rs gc` collects now and reports what it freed.

pub mod collect;

use std::collections::HashSet;
use std::fs::{self, File};
use std::io::{ErrorKind, Write as _};
use std::os::unix::fs::MetadataExt as _;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// `<cache>/gc`: everything the collector owns.
pub const GC_DIRECTORY: &str = "gc";
/// The time of the last collection (its modification time) and whether work
/// remains (plain text).
pub const STAMP: &str = "stamp";
/// The trash: what a collection removed, until it is deleted.
pub const TRASH: &str = "trash";
/// Last-use stamps of repository trees (`<kind>-<identity>`, empty files).
pub const USED: &str = "used";
/// Workspace records (`<state name>`: the canonical workspace path).
pub const WORKSPACES: &str = "worktrees";

const DAY: Duration = Duration::from_secs(24 * 60 * 60);
/// The default `--experimental_disk_cache_gc_max_age` (genuine: 0, off).
pub const DEFAULT_MAX_AGE: Duration = Duration::from_secs(60 * 24 * 60 * 60);
/// The default `--repo_contents_cache_gc_max_age` (as genuine, 14 days:
/// RepositoryOptions.java:75-85).
pub const DEFAULT_REPO_MAX_AGE: Duration = Duration::from_secs(14 * 24 * 60 * 60);
/// The default idle delay: the least time between two automatic
/// collections (genuine: 5 minutes of server idleness).
pub const DEFAULT_IDLE_DELAY: Duration = Duration::from_secs(60 * 60);
/// The automatic size budget: 10% of what the cache's file system has
/// available plus the cache's own size, within these bounds.
pub const BUDGET_PERCENT: u64 = 10;
pub const BUDGET_FLOOR: u64 = 10 << 30;
pub const BUDGET_CAP: u64 = 64 << 30;

/// The garbage collection options (all commands, like genuine's).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct GcOptions {
    /// `--experimental_disk_cache_gc_max_size` (bytes; 0: no size limit).
    pub max_size: Option<u64>,
    /// `--experimental_disk_cache_gc_max_age` (zero: no age limit).
    pub max_age: Option<Duration>,
    /// `--experimental_disk_cache_gc_idle_delay`.
    pub idle_delay: Option<Duration>,
    /// `--repo_contents_cache_gc_max_age` (zero: only unreferenced trees).
    pub repo_max_age: Option<Duration>,
    /// `--repo_contents_cache_gc_idle_delay`.
    pub repo_idle_delay: Option<Duration>,
}

/// The option names [`GcOptions::set`] takes.
pub const OPTION_NAMES: [&str; 5] = [
    "experimental_disk_cache_gc_max_size",
    "experimental_disk_cache_gc_max_age",
    "experimental_disk_cache_gc_idle_delay",
    "repo_contents_cache_gc_max_age",
    "repo_contents_cache_gc_idle_delay",
];

impl GcOptions {
    /// Set option `name` (one of [`OPTION_NAMES`]) to `value`: the error is
    /// the converter's message (Converters.ByteSizeConverter,
    /// DurationConverter).
    pub fn set(&mut self, name: &str, value: &str) -> Result<(), String> {
        match name {
            "experimental_disk_cache_gc_max_size" => self.max_size = Some(parse_size(value)?),
            "experimental_disk_cache_gc_max_age" => self.max_age = Some(parse_duration(value)?),
            "experimental_disk_cache_gc_idle_delay" => {
                self.idle_delay = Some(parse_duration(value)?);
            }
            "repo_contents_cache_gc_max_age" => self.repo_max_age = Some(parse_duration(value)?),
            "repo_contents_cache_gc_idle_delay" => {
                self.repo_idle_delay = Some(parse_duration(value)?);
            }
            _ => return Err(format!("unknown garbage collection option {name}")),
        }
        Ok(())
    }
}

/// Converters.ByteSizeConverter: digits with an optional K, M, G or T
/// (powers of 1024).
pub fn parse_size(value: &str) -> Result<u64, String> {
    let invalid = || format!("Invalid size: {value}");
    let (digits, multiplier) = match value.as_bytes().last() {
        Some(b'K') => (&value[..value.len() - 1], 1u64 << 10),
        Some(b'M') => (&value[..value.len() - 1], 1 << 20),
        Some(b'G') => (&value[..value.len() - 1], 1 << 30),
        Some(b'T') => (&value[..value.len() - 1], 1 << 40),
        _ => (value, 1),
    };
    if digits.is_empty() || !digits.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(invalid());
    }
    let number: i64 = digits.parse().map_err(|_| invalid())?;
    let number = u64::try_from(number).map_err(|_| invalid())?;
    let bytes = number.checked_mul(multiplier).ok_or_else(invalid)?;
    i64::try_from(bytes).map_err(|_| invalid())?;
    Ok(bytes)
}

/// Converters.DurationConverter: `0`, or digits with d, h, m, s, ms or ns.
pub fn parse_duration(value: &str) -> Result<Duration, String> {
    if value == "0" {
        return Ok(Duration::ZERO);
    }
    let illegal = || format!("Illegal duration '{value}'.");
    let split = value
        .find(|c: char| !c.is_ascii_digit())
        .ok_or_else(illegal)?;
    let (digits, unit) = value.split_at(split);
    if digits.is_empty() {
        return Err(illegal());
    }
    let number: i64 = digits.parse().map_err(|_| illegal())?;
    let number = u64::try_from(number).map_err(|_| illegal())?;
    let seconds = |factor: u64| {
        number
            .checked_mul(factor)
            .map(Duration::from_secs)
            .ok_or_else(illegal)
    };
    match unit {
        "d" => seconds(24 * 60 * 60),
        "h" => seconds(60 * 60),
        "m" => seconds(60),
        "s" => seconds(1),
        "ms" => Ok(Duration::from_millis(number)),
        "ns" => Ok(Duration::from_nanos(number)),
        _ => Err(illegal()),
    }
}

/// The size limit of a collection.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Budget {
    /// No size limit.
    Off,
    /// At most this many bytes.
    Bytes(u64),
    /// [`automatic_budget`] of the cache's file system.
    Auto,
}

/// What a collection removes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Policy {
    pub budget: Budget,
    /// Entries unused for longer are removed (None: no age limit).
    pub max_age: Option<Duration>,
    /// Repository trees unused for longer are removed (None: only
    /// unreferenced ones).
    pub repo_max_age: Option<Duration>,
    /// The least time between two automatic collections.
    pub idle_delay: Duration,
    /// The resolution of last-use times: an entry's stamp is refreshed when
    /// it is older than this, and the size budget never removes an entry
    /// used more recently (it may have been used by the command that just
    /// ended).
    pub recent: Duration,
}

impl Policy {
    pub fn of(options: &GcOptions) -> Self {
        let nonzero = |duration: Duration| (!duration.is_zero()).then_some(duration);
        let max_age = nonzero(options.max_age.unwrap_or(DEFAULT_MAX_AGE));
        let repo_max_age = nonzero(options.repo_max_age.unwrap_or(DEFAULT_REPO_MAX_AGE));
        let budget = match options.max_size {
            None => Budget::Auto,
            Some(0) => Budget::Off,
            Some(bytes) => Budget::Bytes(bytes),
        };
        let idle_delay = match (options.idle_delay, options.repo_idle_delay) {
            (None, None) => DEFAULT_IDLE_DELAY,
            (Some(a), Some(b)) => a.min(b),
            (Some(a), None) | (None, Some(a)) => a,
        };
        // A quarter of the shortest age limit, between a minute and a day.
        let shortest = [max_age, repo_max_age]
            .into_iter()
            .flatten()
            .min()
            .unwrap_or(DAY * 4);
        let recent = (shortest / 4).clamp(Duration::from_secs(60), DAY);
        Self {
            budget,
            max_age,
            repo_max_age,
            idle_delay,
            recent,
        }
    }
}

/// The automatic size budget for a cache of `size` bytes on a file system
/// with `available` bytes free: 10% of both, between 10 and 64 GiB.
pub fn automatic_budget(available: u64, size: u64) -> u64 {
    (available.saturating_add(size) / 100 * BUDGET_PERCENT).clamp(BUDGET_FLOOR, BUDGET_CAP)
}

/// Whether automatic collection is switched off (`BAZEL_RS_CACHE_GC=off`).
fn automatic_disabled() -> bool {
    std::env::var("BAZEL_RS_CACHE_GC").is_ok_and(|value| value == "off" || value == "0")
}

/// This process's cache ([`install`]).
struct Installed {
    cache: PathBuf,
    /// The canonical cache root (tree roots are canonical).
    canonical: PathBuf,
    /// The shared lock (None: not held; the cache is not writable).
    lock: Mutex<Option<File>>,
    options: Mutex<Option<GcOptions>>,
    /// The canonical workspace and its state directory's name
    /// ([`record_workspace`]).
    workspace: Mutex<Option<(PathBuf, String)>>,
    /// Files whose access time is to be set (`true`: known to be older than
    /// the recent window).
    files: Mutex<Vec<(PathBuf, bool)>>,
    trees: Mutex<HashSet<PathBuf>>,
    /// [`Policy::recent`] in seconds (before the options are known: a day).
    recent: std::sync::atomic::AtomicU64,
}

static INSTALLED: OnceLock<Installed> = OnceLock::new();

/// `<cache>/gc`.
pub fn directory(cache: &Path) -> PathBuf {
    cache.join(GC_DIRECTORY)
}

/// The cache lock: a flock of the cache directory itself, so that a command
/// creates nothing in a cache it does not write to (and no cache at all
/// when it fails before it needs one). A cache that does not exist yet
/// holds nothing to protect.
fn open_lock(cache: &Path) -> std::io::Result<File> {
    File::open(cache)
}

fn would_block(error: &std::io::Error) -> bool {
    error.kind() == ErrorKind::WouldBlock
}

/// Take `file` (the cache lock) shared. A collection holds it for a moment
/// (its removals are renames): a note only when the wait is long.
fn shared(cache: &Path, file: File) -> Option<File> {
    let started = std::time::Instant::now();
    loop {
        match fs2::FileExt::try_lock_shared(&file) {
            Ok(()) => return Some(file),
            Err(error) if would_block(&error) => {
                if started.elapsed() > Duration::from_secs(1) {
                    break;
                }
                std::thread::sleep(Duration::from_millis(10));
            }
            Err(_) => return None,
        }
    }
    crate::note!(
        "bazel-rs: waiting for the garbage collection of {} to finish",
        cache.display()
    );
    fs2::FileExt::lock_shared(&file).ok().map(|()| file)
}

/// Make `cache` this process's cache: take its lock shared (waiting, with a
/// note, while a collection runs) and hold it until the process exits.
/// Called once by the CLI when it knows its cache. A cache that does not
/// exist yet is used without the lock.
pub fn install(cache: &Path) {
    let canonical = fs::canonicalize(cache).unwrap_or_else(|_| cache.to_path_buf());
    let lock = open_lock(cache).ok().and_then(|file| shared(cache, file));
    let _ = INSTALLED.set(Installed {
        cache: cache.to_path_buf(),
        canonical,
        lock: Mutex::new(lock),
        options: Mutex::new(None),
        workspace: Mutex::new(None),
        files: Mutex::new(Vec::new()),
        trees: Mutex::new(HashSet::new()),
        recent: std::sync::atomic::AtomicU64::new(DAY.as_secs()),
    });
}

/// The options of this command: automatic collection follows them at its
/// end ([`finish`]). Commands that never set them never collect.
pub fn set_options(options: &GcOptions) {
    if let Some(installed) = INSTALLED.get() {
        installed.recent.store(
            Policy::of(options).recent.as_secs(),
            std::sync::atomic::Ordering::Relaxed,
        );
        *installed
            .options
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(options.clone());
    }
}

fn now_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |since| {
            i64::try_from(since.as_secs()).unwrap_or(i64::MAX)
        })
}

fn recent_secs(installed: &Installed) -> i64 {
    i64::try_from(installed.recent.load(std::sync::atomic::Ordering::Relaxed)).unwrap_or(i64::MAX)
}

/// A hit read `path` (an action's manifest), whose `metadata` it has: its
/// access time is set at the end of the command when it is older than the
/// recent window. Cheap: one comparison unless the time is old.
pub fn note_used(path: &Path, metadata: &fs::Metadata) {
    let Some(installed) = INSTALLED.get() else {
        return;
    };
    let last = metadata.atime().max(metadata.mtime());
    if last >= now_secs().saturating_sub(recent_secs(installed)) {
        return;
    }
    installed
        .files
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .push((path.to_path_buf(), true));
}

/// A hit used `path` (a test result's status, a download): its access time
/// is set at the end of the command when it is older than the recent window.
pub fn note_used_path(path: &Path) {
    if let Some(installed) = INSTALLED.get() {
        installed
            .files
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push((path.to_path_buf(), false));
    }
}

/// The command used the repository tree `root` (a published tree of the
/// store `<cache>/repositories`): its last-use stamp is refreshed at the end
/// of the command.
pub fn note_tree_used(root: &Path) {
    if let Some(installed) = INSTALLED.get() {
        let mut trees = installed
            .trees
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if !trees.contains(root) {
            trees.insert(root.to_path_buf());
        }
    }
}

/// The kind of a repository tree store, by its directory below the store.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum TreeKind {
    /// `direct/trees`: repository rule publications.
    Direct,
    /// `trees`: registry modules.
    Registry,
    /// `git/trees`: Git sources.
    Git,
    /// `builtins/trees`: the embedded `bazel_tools`.
    Builtin,
}

impl TreeKind {
    pub const ALL: [Self; 4] = [Self::Direct, Self::Registry, Self::Git, Self::Builtin];

    pub fn name(self) -> &'static str {
        match self {
            Self::Direct => "direct",
            Self::Registry => "registry",
            Self::Git => "git",
            Self::Builtin => "builtin",
        }
    }

    /// The tree directory below the store.
    pub fn trees(self) -> &'static str {
        match self {
            Self::Direct => "direct/trees",
            Self::Registry => "trees",
            Self::Git => "git/trees",
            Self::Builtin => "builtins/trees",
        }
    }

    /// The manifest of tree `id` below the store.
    pub fn manifest(self, id: &str) -> String {
        match self {
            Self::Direct => format!("direct/manifests/{id}.json"),
            Self::Registry => format!("trees/{id}.json"),
            Self::Git => format!("git/manifests/{id}.json"),
            Self::Builtin => format!("builtins/trees/{id}.json"),
        }
    }

    /// The publication lock of tree `id` below the store (never removed).
    pub fn lock(self, id: &str) -> String {
        match self {
            Self::Direct => format!("direct/locks/{id}"),
            Self::Registry => format!("locks/{id}"),
            Self::Git => format!("git/locks/{id}"),
            Self::Builtin => format!("builtins/locks/{id}"),
        }
    }
}

/// The store, kind and identity of the tree whose root is `root`.
fn tree_of(store: &Path, root: &Path) -> Option<(TreeKind, String)> {
    let rest = root.strip_prefix(store).ok()?;
    let id = rest.file_name()?.to_str()?;
    if id.len() != 64 || !id.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    let parent = rest.parent()?;
    TreeKind::ALL
        .into_iter()
        .find(|kind| parent == Path::new(kind.trees()))
        .map(|kind| (kind, id.to_owned()))
}

/// The last-use stamp of a tree.
pub fn tree_stamp(cache: &Path, kind: TreeKind, id: &str) -> PathBuf {
    directory(cache)
        .join(USED)
        .join(format!("{}-{id}", kind.name()))
}

/// Set `path`'s access time (and with `modified` its modification time) to
/// now.
fn touch(path: &Path, modified: bool) -> std::io::Result<()> {
    use rustix::fs::{AtFlags, CWD, Timespec, Timestamps, UTIME_NOW, UTIME_OMIT};
    let now = Timespec {
        tv_sec: 0,
        tv_nsec: UTIME_NOW,
    };
    let times = Timestamps {
        last_access: now,
        last_modification: if modified {
            now
        } else {
            Timespec {
                tv_sec: 0,
                tv_nsec: UTIME_OMIT,
            }
        },
    };
    rustix::fs::utimensat(CWD, path, &times, AtFlags::SYMLINK_NOFOLLOW).map_err(Into::into)
}

/// Record the last uses noted so far ([`note_used`], [`note_tree_used`]).
fn flush(installed: &Installed) {
    let files = std::mem::take(
        &mut *installed
            .files
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner),
    );
    let trees = std::mem::take(
        &mut *installed
            .trees
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner),
    );
    if files.is_empty() && trees.is_empty() {
        return;
    }
    let cutoff = now_secs().saturating_sub(recent_secs(installed));
    let old = |metadata: &fs::Metadata| metadata.atime().max(metadata.mtime()) < cutoff;
    let store = installed.cache.join("repositories");
    let canonical_store = installed.canonical.join("repositories");
    let used = directory(&installed.cache).join(USED);
    let mut stamps = Vec::new();
    for root in &trees {
        if let Some((kind, id)) = tree_of(&canonical_store, root).or_else(|| tree_of(&store, root))
        {
            stamps.push(tree_stamp(&installed.cache, kind, &id));
        }
    }
    stamps.sort();
    stamps.dedup();
    let work = |item: Work<'_>| match item {
        Work::File(path, known) => {
            if known || fs::symlink_metadata(path).is_ok_and(|metadata| old(&metadata)) {
                let _ = touch(path, false);
            }
        }
        Work::Stamp(path) => match fs::symlink_metadata(path) {
            Ok(metadata) if !old(&metadata) => {}
            Ok(_) => {
                let _ = touch(path, true);
            }
            Err(error) if error.kind() == ErrorKind::NotFound => {
                let created = File::create(path).or_else(|_| {
                    fs::create_dir_all(&used)?;
                    File::create(path)
                });
                drop(created);
            }
            Err(_) => {}
        },
    };
    let items: Vec<Work<'_>> = files
        .iter()
        .map(|(path, known)| Work::File(path, *known))
        .chain(stamps.iter().map(|path| Work::Stamp(path)))
        .collect();
    parallel(&items, 8, |item| work(*item));
}

#[derive(Clone, Copy)]
enum Work<'a> {
    File(&'a Path, bool),
    Stamp(&'a Path),
}

/// Run `work` over `items` on up to `threads` threads (one below 256 items).
pub(crate) fn parallel<T: Sync>(items: &[T], threads: usize, work: impl Fn(&T) + Sync) {
    let threads = if items.len() < 256 {
        1
    } else {
        std::thread::available_parallelism()
            .map(usize::from)
            .unwrap_or(1)
            .clamp(1, threads)
    };
    if threads == 1 {
        items.iter().for_each(work);
        return;
    }
    let next = std::sync::atomic::AtomicUsize::new(0);
    std::thread::scope(|scope| {
        for _ in 0..threads {
            scope.spawn(|| {
                loop {
                    let index = next.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    let Some(item) = items.get(index) else {
                        break;
                    };
                    work(item);
                }
            });
        }
    });
}

/// This command's workspace: at its end, when the workspace has a state
/// directory in the cache, `<cache>/gc/worktrees/<state name>` records its
/// canonical path, so that the state of a deleted worktree can be removed
/// ([`write_workspace_record`]).
pub fn record_workspace(workspace: &Path) {
    if let Some(installed) = INSTALLED.get() {
        let canonical = fs::canonicalize(workspace).unwrap_or_else(|_| workspace.to_path_buf());
        let name = crate::worktree::state_name(workspace);
        *installed
            .workspace
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some((canonical, name));
    }
}

/// Write the workspace record when missing or different and the state
/// directory holds anything; its access time is the state's last use.
fn write_workspace_record(installed: &Installed) {
    let Some((canonical, name)) = installed
        .workspace
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone()
    else {
        return;
    };
    let cache = &installed.cache;
    let state = cache.join(crate::worktree::WORKTREES).join(&name);
    if !fs::read_dir(&state).is_ok_and(|mut entries| entries.next().is_some()) {
        return;
    }
    let records = directory(cache).join(WORKSPACES);
    let path = records.join(&name);
    let mut content = canonical.as_os_str().as_encoded_bytes().to_vec();
    content.push(b'\n');
    if fs::read(&path).is_ok_and(|existing| existing == content) {
        installed
            .files
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push((path, false));
        return;
    }
    let write = || -> std::io::Result<()> {
        fs::create_dir_all(&records)?;
        let temporary = records.join(format!(".tmp-{name}-{}", std::process::id()));
        let mut file = File::create(&temporary)?;
        file.write_all(&content)?;
        drop(file);
        fs::rename(&temporary, &path)
    };
    let _ = write();
}

/// The stamp of the last collection: its time and whether work remains.
fn read_stamp(cache: &Path) -> Option<(SystemTime, bool)> {
    let path = directory(cache).join(STAMP);
    let metadata = fs::metadata(&path).ok()?;
    let pending = fs::read(&path).is_ok_and(|bytes| bytes.ends_with(b"pending\n"));
    Some((metadata.modified().ok()?, pending))
}

/// When `cache` was created: its birth time, else (a file system without
/// one) its last change.
fn created(cache: &Path) -> Option<SystemTime> {
    let metadata = fs::metadata(cache).ok()?;
    metadata.created().or_else(|_| metadata.modified()).ok()
}

/// Write the stamp of a collection that ended now.
pub(crate) fn write_stamp(cache: &Path, pending: bool) {
    let directory = directory(cache);
    let temporary = directory.join(format!(".tmp-stamp-{}", std::process::id()));
    let text = if pending {
        "bazel-rs cache gc 1\npending\n"
    } else {
        "bazel-rs cache gc 1\n"
    };
    if fs::write(&temporary, text).is_ok() && fs::rename(&temporary, directory.join(STAMP)).is_err()
    {
        let _ = fs::remove_file(&temporary);
    }
}

/// The end of a command: record the last uses noted, then — when the command
/// set its options, automatic collection is on and the last collection is
/// at least the idle delay old (or left work) — collect without waiting for
/// anyone, in bounded time. `collect` false (a `run` about to exec its
/// target) only records.
pub fn finish(collect: bool) {
    let Some(installed) = INSTALLED.get() else {
        return;
    };
    write_workspace_record(installed);
    flush(installed);
    if !collect || automatic_disabled() {
        return;
    }
    let Some(options) = installed
        .options
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone()
    else {
        return;
    };
    let policy = Policy::of(&options);
    // The last collection; a cache never collected counts from its
    // creation (a new cache is not collected at once).
    let (last, pending) = match read_stamp(&installed.cache) {
        Some(stamp) => stamp,
        None => match created(&installed.cache) {
            Some(created) => (created, false),
            None => return,
        },
    };
    if !pending
        && SystemTime::now()
            .duration_since(last)
            .is_ok_and(|age| age < policy.idle_delay)
    {
        return;
    }
    // Our shared lock goes (the command is over); the exclusive one is
    // taken only when no other process holds the lock.
    let held = installed
        .lock
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .take();
    if held.is_none() {
        return;
    }
    drop(held);
    let Ok(lock) = open_lock(&installed.cache) else {
        return;
    };
    if fs2::FileExt::try_lock_exclusive(&lock).is_err() {
        return;
    }
    // Background priority for what remains of this process.
    let _ = rustix::process::setpriority_process(None, 10);
    let report = collect::collect(
        &installed.cache,
        &policy,
        collect::Mode::Automatic,
        Some(lock),
    );
    if report.deleted_entries > 0 {
        crate::note!("bazel-rs: cache garbage collection: {}", report.summary());
    }
}

/// `bazel-rs gc`: collect `cache` now under `options`, waiting for the
/// processes that use it; `dry_run` reports without removing anything.
pub fn collect_now(cache: &Path, options: &GcOptions, dry_run: bool) -> collect::Report {
    let policy = Policy::of(options);
    let lock = match open_lock(cache) {
        Ok(lock) => lock,
        Err(_) => {
            return collect::collect(cache, &policy, collect::Mode::Explicit { dry_run }, None);
        }
    };
    if let Err(error) = fs2::FileExt::try_lock_exclusive(&lock) {
        if would_block(&error) {
            crate::note!(
                "bazel-rs: waiting for the other bazel-rs processes using {} to finish",
                cache.display()
            );
        }
        let _ = fs2::FileExt::lock_exclusive(&lock);
    }
    collect::collect(
        cache,
        &policy,
        collect::Mode::Explicit { dry_run },
        Some(lock),
    )
}

/// Bytes for people: `1.5 GiB`, `12.0 MiB`, `512 B`.
pub fn display_bytes(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["KiB", "MiB", "GiB", "TiB", "PiB"];
    if bytes < 1024 {
        return format!("{bytes} B");
    }
    #[allow(clippy::cast_precision_loss)]
    let mut value = bytes as f64 / 1024.0;
    let mut unit = 0;
    while value >= 1024.0 && unit + 1 < UNITS.len() {
        value /= 1024.0;
        unit += 1;
    }
    format!("{value:.1} {}", UNITS[unit])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sizes_and_durations_parse_like_bazel() {
        assert_eq!(parse_size("0"), Ok(0));
        assert_eq!(parse_size("123"), Ok(123));
        assert_eq!(parse_size("2K"), Ok(2048));
        assert_eq!(parse_size("3G"), Ok(3 << 30));
        assert_eq!(parse_size("1T"), Ok(1 << 40));
        for bad in ["", "G", "1g", "1.5G", "-1", "1KB", "99999999999999999999"] {
            assert_eq!(
                parse_size(bad),
                Err(format!("Invalid size: {bad}")),
                "{bad}"
            );
        }
        assert_eq!(parse_duration("0"), Ok(Duration::ZERO));
        assert_eq!(parse_duration("14d"), Ok(DAY * 14));
        assert_eq!(parse_duration("5m"), Ok(Duration::from_secs(300)));
        assert_eq!(parse_duration("10ms"), Ok(Duration::from_millis(10)));
        assert_eq!(parse_duration("0s"), Ok(Duration::ZERO));
        for bad in ["", "d", "1", "1x", "1.5h", "-1d", "1 d"] {
            assert_eq!(
                parse_duration(bad),
                Err(format!("Illegal duration '{bad}'.")),
                "{bad}"
            );
        }
    }

    #[test]
    fn the_default_policy_and_its_resolution() {
        let policy = Policy::of(&GcOptions::default());
        assert_eq!(policy.budget, Budget::Auto);
        assert_eq!(policy.max_age, Some(DEFAULT_MAX_AGE));
        assert_eq!(policy.repo_max_age, Some(DEFAULT_REPO_MAX_AGE));
        assert_eq!(policy.idle_delay, DEFAULT_IDLE_DELAY);
        assert_eq!(policy.recent, DAY);
        let mut options = GcOptions::default();
        for (name, value) in [
            ("experimental_disk_cache_gc_max_size", "0"),
            ("experimental_disk_cache_gc_max_age", "0"),
            ("repo_contents_cache_gc_max_age", "1h"),
            ("experimental_disk_cache_gc_idle_delay", "5m"),
            ("repo_contents_cache_gc_idle_delay", "1m"),
        ] {
            options.set(name, value).unwrap();
        }
        let policy = Policy::of(&options);
        assert_eq!(policy.budget, Budget::Off);
        assert_eq!(policy.max_age, None);
        assert_eq!(policy.repo_max_age, Some(Duration::from_secs(3600)));
        assert_eq!(policy.idle_delay, Duration::from_secs(60));
        assert_eq!(policy.recent, Duration::from_secs(900));
        assert_eq!(automatic_budget(0, 0), BUDGET_FLOOR);
        assert_eq!(automatic_budget(3 << 40, 30 << 30), BUDGET_CAP);
        assert_eq!(automatic_budget(150 << 30, 50 << 30), 20 << 30);
    }

    #[test]
    fn tree_roots_name_their_stamps() {
        let store = Path::new("/c/repositories");
        let id = "a".repeat(64);
        assert_eq!(
            tree_of(store, &store.join("direct/trees").join(&id)),
            Some((TreeKind::Direct, id.clone()))
        );
        assert_eq!(
            tree_of(store, &store.join("trees").join(&id)),
            Some((TreeKind::Registry, id.clone()))
        );
        assert_eq!(
            tree_of(store, &store.join("git/trees").join(&id)),
            Some((TreeKind::Git, id.clone()))
        );
        assert_eq!(tree_of(store, &store.join("external").join(&id)), None);
        assert_eq!(tree_of(store, &store.join("direct/trees/short")), None);
        assert_eq!(
            tree_stamp(Path::new("/c"), TreeKind::Direct, &id),
            PathBuf::from(format!("/c/gc/used/direct-{id}"))
        );
    }

    #[test]
    fn display_bytes_reads_like_bazel() {
        assert_eq!(display_bytes(0), "0 B");
        assert_eq!(display_bytes(1536), "1.5 KiB");
        assert_eq!(display_bytes(3 << 30), "3.0 GiB");
    }
}
