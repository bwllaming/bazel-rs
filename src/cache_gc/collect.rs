//! One collection of a cache (docs/CACHE_GC.md): scan, decide, remove.
//!
//! Runs only under the exclusive cache lock (crate::cache_gc): no other
//! bazel-rs process of this or a later version uses the cache meanwhile.
//! Earlier binaries take no such lock; the per-entry locks they do take
//! (action, test, repository publication and slot locks) are tried without
//! waiting before an entry goes, and temporaries are only removed a day
//! after their last change.
//!
//! Crash safety: an entry leaves the cache by one rename into the trash
//! (`<cache>/gc/trash`), or, for a unit of several paths, by removing the
//! paths that make it trusted first (a repository tree's specification
//! pointers, slot and manifest before the tree): whatever a killed
//! collection left is an entry that is either complete or not trusted (a
//! miss: re-executed or fetched again), and the trash and the orphans are
//! removed by the next collection.

use super::{Budget, Policy, TreeKind};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs::{self, File};
use std::io::ErrorKind;
use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

/// How long a temporary (an execution root, a staging or trash directory, a
/// `.tmp` file) must be unchanged before it counts as abandoned.
pub const STALE_TEMPORARY: Duration = Duration::from_secs(24 * 60 * 60);
/// Automatic collections start sizing new entries for at most this long
/// (an entry started is sized whole; what is left is sized by the next
/// collection, and the size budget waits until all are).
const AUTOMATIC_SIZING: Duration = Duration::from_secs(1);
/// Automatic collections empty the trash for at most this long (half of it
/// before removing, all of it after).
const AUTOMATIC_TRASH: Duration = Duration::from_secs(1);
/// `O_NOATIME`: the collector's reads never make an entry look used.
const NOATIME: i32 = 0o1_000_000;

const INDEX: &str = "index";
const INDEX_MAGIC: &[u8; 8] = b"BZRSGCIX";
const INDEX_VERSION: u32 = 1;

/// How a collection runs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    /// At the end of a command: bounded time, silent unless it removed
    /// something.
    Automatic,
    /// `bazel-rs gc`: unbounded; `dry_run` removes nothing.
    Explicit { dry_run: bool },
}

/// What one category of the cache held and lost.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Category {
    pub entries: u64,
    pub bytes: u64,
    pub deleted: u64,
    pub deleted_bytes: u64,
}

/// What a collection found and removed.
#[derive(Clone, Debug, Default)]
pub struct Report {
    pub total_entries: u64,
    pub total_bytes: u64,
    pub deleted_entries: u64,
    pub deleted_bytes: u64,
    pub categories: BTreeMap<&'static str, Category>,
    /// The size budget applied (None: none, or not all entries were sized).
    pub budget: Option<u64>,
    /// Whether every entry's size is known (else the budget waits).
    pub sized: bool,
    /// Entries left because an earlier binary held their lock.
    pub busy: u64,
    /// Whether the trash was emptied.
    pub trash_emptied: bool,
    pub dry_run: bool,
    pub elapsed: Duration,
    /// Wall time by phase (`gc` prints it; automatic collections under
    /// `BAZEL_RS_CACHE_GC_TIMING=1`).
    pub phases: Vec<(&'static str, Duration)>,
}

impl Report {
    /// The phase times: `scan 0.91 s, decide 0.01 s, ...`.
    pub fn phase_summary(&self) -> String {
        self.phases
            .iter()
            .map(|(name, time)| format!("{name} {:.2} s", time.as_secs_f64()))
            .collect::<Vec<_>>()
            .join(", ")
    }

    /// One line, like DiskCacheGarbageCollector.CollectionStats.displayString.
    pub fn summary(&self) -> String {
        format!(
            "{} {} of {} entries, {} {} of {} in {:.2} s{}",
            if self.dry_run {
                "would delete"
            } else {
                "deleted"
            },
            self.deleted_entries,
            self.total_entries,
            if self.dry_run {
                "reclaiming"
            } else {
                "reclaimed"
            },
            super::display_bytes(self.deleted_bytes),
            super::display_bytes(self.total_bytes),
            self.elapsed.as_secs_f64(),
            if self.busy > 0 {
                format!(" ({} entries in use were kept)", self.busy)
            } else {
                String::new()
            }
        )
    }

    fn count(&mut self, category: &'static str, bytes: u64) {
        let entry = self.categories.entry(category).or_default();
        entry.entries += 1;
        entry.bytes += bytes;
        self.total_entries += 1;
        self.total_bytes += bytes;
    }

    fn deleted(&mut self, category: &'static str, bytes: u64) {
        let entry = self.categories.entry(category).or_default();
        entry.deleted += 1;
        entry.deleted_bytes += bytes;
        self.deleted_entries += 1;
        self.deleted_bytes += bytes;
    }
}

/// What an entry is.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
enum Kind {
    Action,
    Test,
    Download,
    Tree(TreeKind),
    Blob,
}

impl Kind {
    fn category(self) -> &'static str {
        match self {
            Self::Action => "actions",
            Self::Test => "tests",
            Self::Download => "downloads",
            Self::Tree(_) => "repositories",
            Self::Blob => "cas",
        }
    }

    /// The index code of the entry's size record.
    fn code(self) -> u8 {
        match self {
            Self::Action => 0,
            Self::Test => 1,
            Self::Download => 2,
            Self::Tree(TreeKind::Direct) => 3,
            Self::Tree(TreeKind::Registry) => 4,
            Self::Tree(TreeKind::Git) => 5,
            Self::Tree(TreeKind::Builtin) => 6,
            Self::Blob => 9,
        }
    }
}

/// Index codes of records that are no entries.
const CODE_POINTER: u8 = 7;
const CODE_SLOT: u8 = 8;

/// A file whose state stands for an entry's: the entry is removed only when
/// it still shows the state the scan saw.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Anchor {
    path: PathBuf,
    ino: u64,
    mtime: i64,
}

impl Anchor {
    fn of(path: PathBuf, metadata: &fs::Metadata) -> Self {
        Self {
            path,
            ino: metadata.ino(),
            mtime: mtime_ns(metadata),
        }
    }

    fn unchanged(&self) -> bool {
        fs::symlink_metadata(&self.path)
            .is_ok_and(|metadata| metadata.ino() == self.ino && mtime_ns(&metadata) == self.mtime)
    }
}

/// One collectable entry.
#[derive(Clone, Debug)]
struct Item {
    kind: Kind,
    /// The action or test key, `<algorithm>/<hex>`, the tree identity or the
    /// blob name.
    name: String,
    /// Seconds since the epoch.
    last_use: i64,
    size: Option<u64>,
    anchor: Option<Anchor>,
    /// Direct trees: the store trees its bytes name (`direct/trees/<id>`).
    refs: Vec<String>,
    /// Blobs: the link count when scanned.
    links: u64,
    /// Blobs: (device, inode).
    inode: (u64, u64),
}

/// Something removed whatever the policy: an abandoned temporary, a dangling
/// pointer or link, an orphaned manifest, the state of a deleted worktree.
#[derive(Clone, Debug)]
struct Sweep {
    path: PathBuf,
    category: &'static str,
    /// For a slot (link or directory): its repository's canonical name,
    /// whose writer lock is taken first.
    slot: Option<String>,
    /// Remove this too (a worktree's record).
    also: Option<PathBuf>,
}

#[derive(Serialize, Deserialize)]
struct IndexRecord {
    code: u8,
    name: String,
    ino: u64,
    mtime: i64,
    size: u64,
    refs: Vec<String>,
}

#[derive(Default)]
struct Index {
    records: HashMap<(u8, String), IndexRecord>,
}

impl Index {
    fn read(path: &Path) -> Self {
        let Ok(bytes) = fs::read(path) else {
            return Self::default();
        };
        let header = INDEX_MAGIC.len() + 4 + 8;
        if bytes.len() < header
            || &bytes[..8] != INDEX_MAGIC
            || bytes[8..12] != INDEX_VERSION.to_le_bytes()
            || bytes[12..20] != blake3::hash(&bytes[header..]).as_bytes()[..8]
        {
            return Self::default();
        }
        let Ok(records) = postcard::from_bytes::<Vec<IndexRecord>>(&bytes[header..]) else {
            return Self::default();
        };
        Self {
            records: records
                .into_iter()
                .map(|record| ((record.code, record.name.clone()), record))
                .collect(),
        }
    }

    fn write(path: &Path, records: &[IndexRecord]) {
        let Ok(body) = postcard::to_stdvec(records) else {
            return;
        };
        let mut bytes = Vec::with_capacity(20 + body.len());
        bytes.extend_from_slice(INDEX_MAGIC);
        bytes.extend_from_slice(&INDEX_VERSION.to_le_bytes());
        bytes.extend_from_slice(&blake3::hash(&body).as_bytes()[..8]);
        bytes.extend_from_slice(&body);
        let temporary = path.with_file_name(format!(".tmp-index-{}", std::process::id()));
        if fs::write(&temporary, &bytes).is_err() || fs::rename(&temporary, path).is_err() {
            let _ = fs::remove_file(&temporary);
        }
    }

    /// The record of `code`/`name` if `anchor` still shows its state.
    fn get(&self, code: u8, name: &str, ino: u64, mtime: i64) -> Option<&IndexRecord> {
        self.records
            .get(&(code, name.to_owned()))
            .filter(|record| record.ino == ino && record.mtime == mtime)
    }
}

fn mtime_ns(metadata: &fs::Metadata) -> i64 {
    metadata
        .mtime()
        .saturating_mul(1_000_000_000)
        .saturating_add(metadata.mtime_nsec())
}

fn now_secs() -> i64 {
    super::now_secs()
}

fn hex64(name: &str) -> bool {
    name.len() == 64
        && name
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// The names in `directory` with whether each is a directory (never
/// following links); nothing when it cannot be read.
fn list(directory: &Path) -> Vec<(String, Option<fs::FileType>)> {
    let Ok(entries) = fs::read_dir(directory) else {
        return Vec::new();
    };
    entries
        .flatten()
        .filter_map(|entry| {
            let name = entry.file_name().into_string().ok()?;
            Some((name, entry.file_type().ok()))
        })
        .collect()
}

/// The bytes `path` occupies (never following links): every file's blocks
/// shared by its links, so that the sizes of all the cache's entries add up
/// to what the cache occupies.
/// Entries are inspected relative to their directory's descriptor (one
/// component per lookup: absolute-path lookups serialize on overlay file
/// systems, crate::dir_stamps). None when `deadline` passed first.
fn disk_size(path: &Path, deadline: Option<Instant>) -> Option<u64> {
    let Ok(metadata) = fs::symlink_metadata(path) else {
        return Some(0);
    };
    let mut total = share(&metadata);
    if !metadata.is_dir() {
        return Some(total);
    }
    let mut pending = vec![path.to_path_buf()];
    let mut seen = 0u32;
    while let Some(directory) = pending.pop() {
        let Ok(entries) = fs::read_dir(&directory) else {
            continue;
        };
        for entry in entries.flatten() {
            seen = seen.wrapping_add(1);
            if seen.is_multiple_of(1024)
                && deadline.is_some_and(|deadline| Instant::now() > deadline)
            {
                return None;
            }
            // DirEntry::metadata: fstatat on the directory, no follow.
            let Ok(metadata) = entry.metadata() else {
                continue;
            };
            total += share(&metadata);
            if metadata.is_dir() {
                pending.push(entry.path());
            }
        }
    }
    Some(total)
}

/// Read `path` without changing its access time.
fn read_quietly(path: &Path) -> std::io::Result<Vec<u8>> {
    use std::io::Read as _;
    use std::os::unix::fs::OpenOptionsExt as _;
    let mut options = fs::OpenOptions::new();
    options.read(true).custom_flags(NOATIME);
    let mut file = options.open(path).or_else(|_| File::open(path))?;
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)?;
    Ok(bytes)
}

/// One link's share of a file's blocks.
fn share(metadata: &fs::Metadata) -> u64 {
    let bytes = metadata.blocks().saturating_mul(512);
    if metadata.is_file() {
        bytes / metadata.nlink().max(1)
    } else {
        bytes
    }
}

fn last_use(metadata: &fs::Metadata) -> i64 {
    metadata.atime().max(metadata.mtime())
}

/// Read the identity a specification pointer names.
fn pointer_identity(path: &Path) -> Option<String> {
    let bytes = read_quietly(path).ok()?;
    let key = b"\"identity\":\"";
    let at = memchr::memmem::find(&bytes, key)? + key.len();
    let identity = std::str::from_utf8(bytes.get(at..at + 64)?).ok()?;
    hex64(identity).then(|| identity.to_owned())
}

/// The store trees a direct manifest's `refs` name.
fn manifest_refs(path: &Path) -> Vec<String> {
    let Ok(bytes) = read_quietly(path) else {
        return Vec::new();
    };
    let key = b"\"refs\":[";
    let Some(at) = memchr::memmem::rfind(&bytes, key) else {
        return Vec::new();
    };
    let rest = &bytes[at + key.len()..];
    let Some(end) = memchr::memchr(b']', rest) else {
        return Vec::new();
    };
    String::from_utf8_lossy(&rest[..end])
        .split(',')
        .map(|item| item.trim().trim_matches('"').to_owned())
        .filter(|item| !item.is_empty())
        .collect()
}

/// A mutable slot's marker: (identity, canonical name).
fn read_marker(path: &Path) -> Option<(String, String)> {
    let text = fs::read_to_string(path).ok()?;
    let mut lines = text.lines();
    if lines.next()? != "bazel-rs repository directory marker 1" {
        return None;
    }
    let identity = lines.next()?;
    let canonical = lines.next()?;
    hex64(identity).then(|| (identity.to_owned(), canonical.to_owned()))
}

/// Try a lock file without waiting (never creating or removing it): true
/// when it is free or does not exist. The returned file holds it.
fn try_lock(path: &Path) -> Result<Option<File>, ()> {
    match File::open(path) {
        Ok(file) => match fs2::FileExt::try_lock_exclusive(&file) {
            Ok(()) => Ok(Some(file)),
            Err(_) => Err(()),
        },
        Err(_) => Ok(None),
    }
}

/// The scan of one cache.
struct Scan {
    items: Vec<Item>,
    sweeps: Vec<Sweep>,
    /// Direct tree identity -> the specification pointers naming it.
    pointers: HashMap<String, Vec<PathBuf>>,
    /// Direct tree identity -> the slots linking it (canonical names).
    links: HashMap<String, Vec<String>>,
    /// Direct tree identity -> (canonical name, marker, slot size).
    mutable: HashMap<String, Vec<(String, PathBuf, u64)>>,
    index: Vec<IndexRecord>,
}

struct Context<'a> {
    cache: &'a Path,
    store: PathBuf,
    now: i64,
    stale: i64,
    index: Index,
    deadline: Option<Instant>,
}

impl Context<'_> {
    fn stale(&self, metadata: &fs::Metadata) -> bool {
        metadata.mtime().max(metadata.ctime()) < self.stale
    }

    /// Abandoned temporaries among `directory`'s entries named by `pick`.
    fn temporaries(
        &self,
        directory: &Path,
        category: &'static str,
        pick: impl Fn(&str) -> bool,
        sweeps: &mut Vec<Sweep>,
    ) {
        for (name, _) in list(directory) {
            if !pick(&name) {
                continue;
            }
            let path = directory.join(&name);
            if fs::symlink_metadata(&path).is_ok_and(|metadata| self.stale(&metadata)) {
                sweeps.push(Sweep {
                    path,
                    category,
                    slot: None,
                    also: None,
                });
            }
        }
    }
}

fn dotted(name: &str) -> bool {
    name.starts_with('.')
}

fn scan(context: &Context<'_>) -> Scan {
    let cache = context.cache;
    let store = &context.store;
    let mut sweeps = Vec::new();
    let mut entries: Vec<(Kind, String, PathBuf, PathBuf)> = Vec::new();
    // Action entries: `actions/<key>`, last used when its manifest was.
    let actions = cache.join("actions");
    for (name, _) in list(&actions) {
        if dotted(&name) {
            continue;
        }
        if hex64(&name) {
            let directory = actions.join(&name);
            let anchor = directory.join("manifest.json");
            entries.push((Kind::Action, name, directory, anchor));
        }
    }
    context.temporaries(&actions, "temporaries", dotted, &mut sweeps);
    // Test results: `tests/<key>`, last used when its status was.
    let tests = cache.join("tests");
    for (name, _) in list(&tests) {
        if hex64(&name) {
            let directory = tests.join(&name);
            let anchor = directory.join("status.json");
            entries.push((Kind::Test, name, directory, anchor));
        }
    }
    context.temporaries(&tests, "temporaries", dotted, &mut sweeps);
    context.temporaries(&tests.join("work"), "temporaries", |_| true, &mut sweeps);
    // Execution roots of actions and of module extensions.
    context.temporaries(&cache.join("work"), "temporaries", |_| true, &mut sweeps);
    context.temporaries(
        &store.join("modextwd"),
        "temporaries",
        |_| true,
        &mut sweeps,
    );
    context.temporaries(&super::directory(cache), "temporaries", dotted, &mut sweeps);
    // Downloads: `repositories/downloads/<algorithm>/<hex>`.
    let downloads = store.join("downloads");
    for (algorithm, _) in list(&downloads) {
        let directory = downloads.join(&algorithm);
        for (name, _) in list(&directory) {
            if !dotted(&name) {
                let path = directory.join(&name);
                entries.push((
                    Kind::Download,
                    format!("{algorithm}/{name}"),
                    path.clone(),
                    path,
                ));
            }
        }
        context.temporaries(&directory, "temporaries", dotted, &mut sweeps);
    }
    // Repository trees and their manifests.
    for kind in TreeKind::ALL {
        let trees = store.join(kind.trees());
        let listed = list(&trees);
        let ids: HashSet<&str> = listed
            .iter()
            .filter(|(name, kind)| hex64(name) && kind.is_some_and(|kind| kind.is_dir()))
            .map(|(name, _)| name.as_str())
            .collect();
        for id in &ids {
            let directory = trees.join(id);
            let anchor = store.join(kind.manifest(id));
            entries.push((Kind::Tree(kind), (*id).to_owned(), directory, anchor));
        }
        context.temporaries(&trees, "temporaries", dotted, &mut sweeps);
        // Manifests without their tree.
        let (manifests, suffix) = match kind {
            TreeKind::Direct => (store.join("direct/manifests"), ".json"),
            TreeKind::Git => (store.join("git/manifests"), ".json"),
            TreeKind::Registry | TreeKind::Builtin => (trees.clone(), ".json"),
        };
        let manifest_ids: Vec<(String, String)> = list(&manifests)
            .into_iter()
            .filter_map(|(name, _)| {
                let id = name.strip_suffix(suffix)?;
                hex64(id).then(|| (id.to_owned(), name.clone()))
            })
            .collect();
        for (id, name) in manifest_ids {
            if !ids.contains(id.as_str()) {
                let path = manifests.join(&name);
                if fs::symlink_metadata(&path).is_ok_and(|metadata| context.stale(&metadata)) {
                    sweeps.push(Sweep {
                        path,
                        category: "repositories",
                        slot: None,
                        also: None,
                    });
                }
            }
        }
        if manifests != trees {
            context.temporaries(&manifests, "temporaries", dotted, &mut sweeps);
        }
    }
    // Stat every entry (in parallel): its anchor's times and its size.
    let deadline = context.deadline;
    let items = Mutex::new(Vec::with_capacity(entries.len()));
    let orphans = Mutex::new(Vec::new());
    let index_records = Mutex::new(Vec::new());
    super::parallel(&entries, 16, |(kind, name, directory, anchor)| {
        let anchored = fs::symlink_metadata(anchor);
        let anchor_metadata = match (*kind, anchored) {
            (_, Ok(metadata)) => metadata,
            (Kind::Tree(_) | Kind::Action | Kind::Test, Err(error))
                if error.kind() == ErrorKind::NotFound =>
            {
                // An entry without its manifest or status is never
                // trusted: an orphan once it is old.
                if fs::symlink_metadata(directory).is_ok_and(|metadata| context.stale(&metadata)) {
                    orphans
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .push(Sweep {
                            path: directory.clone(),
                            category: kind.category(),
                            slot: None,
                            also: None,
                        });
                }
                return;
            }
            (_, Err(_)) => return,
        };
        let mut last = last_use(&anchor_metadata);
        if let Kind::Tree(tree) = *kind
            && let Ok(stamp) = fs::symlink_metadata(super::tree_stamp(cache, tree, name))
        {
            last = last.max(last_use(&stamp));
        }
        let anchor = Anchor::of(anchor.clone(), &anchor_metadata);
        let (size, refs) = if *kind == Kind::Download {
            (Some(share(&anchor_metadata)), Vec::new())
        } else if let Some(record) = context
            .index
            .get(kind.code(), name, anchor.ino, anchor.mtime)
        {
            (Some(record.size), record.refs.clone())
        } else if deadline.is_some_and(|deadline| Instant::now() > deadline) {
            (None, Vec::new())
        } else {
            let size = disk_size(directory, deadline).map(|size| {
                if matches!(kind, Kind::Tree(TreeKind::Registry | TreeKind::Builtin)) {
                    // The manifest beside the tree is part of it.
                    size + share(&anchor_metadata)
                } else {
                    size
                }
            });
            let refs = if *kind == Kind::Tree(TreeKind::Direct) && size.is_some() {
                manifest_refs(&anchor.path)
            } else {
                Vec::new()
            };
            (size, refs)
        };
        if let Some(size) = size
            && *kind != Kind::Download
        {
            index_records
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push(IndexRecord {
                    code: kind.code(),
                    name: name.clone(),
                    ino: anchor.ino,
                    mtime: anchor.mtime,
                    size,
                    refs: refs.clone(),
                });
        }
        items
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(Item {
                kind: *kind,
                name: name.clone(),
                last_use: last,
                size,
                anchor: Some(anchor),
                refs,
                links: 0,
                inode: (0, 0),
            });
    });
    let mut items = items
        .into_inner()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    sweeps.extend(
        orphans
            .into_inner()
            .unwrap_or_else(std::sync::PoisonError::into_inner),
    );
    let mut index = index_records
        .into_inner()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    // Blobs: `cas/<sha256>-<mode>[.<n>]`, last used when last linked (a
    // staging hardlinks it, which changes its ctime).
    let cas = cache.join("cas");
    let blobs: Vec<String> = list(&cas)
        .into_iter()
        .map(|(name, _)| name)
        .filter(|name| !dotted(name))
        .collect();
    context.temporaries(&cas, "temporaries", dotted, &mut sweeps);
    let scanned = Mutex::new(Vec::with_capacity(blobs.len()));
    super::parallel(&blobs, 16, |name| {
        if let Ok(metadata) = fs::symlink_metadata(cas.join(name))
            && metadata.is_file()
        {
            let path = cas.join(name);
            scanned
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push(Item {
                    kind: Kind::Blob,
                    name: name.clone(),
                    last_use: last_use(&metadata).max(metadata.ctime()),
                    size: Some(share(&metadata)),
                    anchor: Some(Anchor::of(path, &metadata)),
                    refs: Vec::new(),
                    links: metadata.nlink(),
                    inode: (metadata.dev(), metadata.ino()),
                });
        }
    });
    items.extend(
        scanned
            .into_inner()
            .unwrap_or_else(std::sync::PoisonError::into_inner),
    );
    // Last-use stamps of trees that are gone (removed by hand, or by a
    // collection killed before it removed the stamp).
    let present: HashSet<String> = items
        .iter()
        .filter_map(|item| match item.kind {
            Kind::Tree(kind) => Some(format!("{}-{}", kind.name(), item.name)),
            _ => None,
        })
        .collect();
    let used = super::directory(cache).join(super::USED);
    for (name, _) in list(&used) {
        if !present.contains(&name) {
            let path = used.join(&name);
            if fs::symlink_metadata(&path).is_ok_and(|metadata| context.stale(&metadata)) {
                sweeps.push(Sweep {
                    path,
                    category: "repositories",
                    slot: None,
                    also: None,
                });
            }
        }
    }
    // Specification pointers: which direct trees are referenced.
    let trees: HashSet<String> = items
        .iter()
        .filter(|item| item.kind == Kind::Tree(TreeKind::Direct))
        .map(|item| item.name.clone())
        .collect();
    let specs = store.join("direct/specs");
    let pointer_names: Vec<String> = list(&specs)
        .into_iter()
        .map(|(name, _)| name)
        .filter(|name| !dotted(name))
        .collect();
    context.temporaries(&specs, "temporaries", dotted, &mut sweeps);
    let named = Mutex::new(Vec::with_capacity(pointer_names.len()));
    super::parallel(&pointer_names, 16, |name| {
        let path = specs.join(name);
        let Ok(metadata) = fs::symlink_metadata(&path) else {
            return;
        };
        let (ino, mtime) = (metadata.ino(), mtime_ns(&metadata));
        let identity = match context.index.get(CODE_POINTER, name, ino, mtime) {
            Some(record) => record.refs.first().cloned(),
            None => pointer_identity(&path),
        };
        named
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push((
                name.clone(),
                path,
                ino,
                mtime,
                identity,
                context.stale(&metadata),
            ));
    });
    let mut pointers: HashMap<String, Vec<PathBuf>> = HashMap::new();
    for (name, path, ino, mtime, identity, stale) in named
        .into_inner()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
    {
        match identity {
            Some(identity) if trees.contains(&identity) => {
                index.push(IndexRecord {
                    code: CODE_POINTER,
                    name,
                    ino,
                    mtime,
                    size: 0,
                    refs: vec![identity.clone()],
                });
                pointers.entry(identity).or_default().push(path);
            }
            // A pointer to no tree (or unreadable): dangling once old.
            _ if stale => sweeps.push(Sweep {
                path,
                category: "repositories",
                slot: None,
                also: None,
            }),
            _ => {}
        }
    }
    // Slots: links to trees, and mutable directories with their markers.
    let slots = store.join("external");
    let markers_directory = store.join("direct/slots");
    let mut markers: HashMap<String, (String, String)> = HashMap::new();
    for (slug, _) in list(&markers_directory) {
        let path = markers_directory.join(&slug);
        if let Some((identity, canonical)) = read_marker(&path) {
            let name = crate::repository_slots::slot_name(&canonical).into_owned();
            markers.insert(name, (identity, canonical));
        }
    }
    let mut links: HashMap<String, Vec<String>> = HashMap::new();
    let mut mutable: HashMap<String, Vec<(String, PathBuf, u64)>> = HashMap::new();
    for (name, file_type) in list(&slots) {
        let path = slots.join(&name);
        let Some(file_type) = file_type else {
            continue;
        };
        if dotted(&name) {
            if fs::symlink_metadata(&path).is_ok_and(|metadata| context.stale(&metadata)) {
                sweeps.push(Sweep {
                    path,
                    category: "temporaries",
                    slot: None,
                    also: None,
                });
            }
            continue;
        }
        if file_type.is_symlink() {
            let target = fs::read_link(&path).ok();
            let identity = target.as_deref().and_then(|target| {
                let id = target
                    .strip_prefix("../direct/trees")
                    .ok()?
                    .to_str()?
                    .to_owned();
                hex64(&id).then_some(id)
            });
            match identity {
                Some(identity) if trees.contains(&identity) => {
                    links.entry(identity).or_default().push(name);
                }
                _ => sweeps.push(Sweep {
                    path,
                    category: "repositories",
                    slot: Some(name),
                    also: None,
                }),
            }
        } else if file_type.is_dir() {
            match markers.get(&name) {
                Some((identity, canonical)) if trees.contains(identity) => {
                    let marker = markers_directory.join(crate::repository_slots::slug(canonical));
                    let size = fs::symlink_metadata(&marker)
                        .ok()
                        .and_then(|metadata| {
                            let (ino, mtime) = (metadata.ino(), mtime_ns(&metadata));
                            let size = match context.index.get(CODE_SLOT, &name, ino, mtime) {
                                Some(record) => Some(record.size),
                                None if deadline
                                    .is_some_and(|deadline| Instant::now() > deadline) =>
                                {
                                    None
                                }
                                None => disk_size(&path, deadline),
                            }?;
                            index.push(IndexRecord {
                                code: CODE_SLOT,
                                name: name.clone(),
                                ino,
                                mtime,
                                size,
                                refs: Vec::new(),
                            });
                            Some(size)
                        })
                        .unwrap_or(0);
                    mutable.entry(identity.clone()).or_default().push((
                        canonical.clone(),
                        marker,
                        size,
                    ));
                }
                // A directory without a complete marker of an existing
                // tree: an interrupted writer's, or its tree is gone.
                _ => {
                    if fs::symlink_metadata(&path).is_ok_and(|metadata| context.stale(&metadata)) {
                        sweeps.push(Sweep {
                            path,
                            category: "repositories",
                            slot: Some(name),
                            also: None,
                        });
                    }
                }
            }
        }
    }
    // The state of deleted worktrees.
    let records = super::directory(cache).join(super::WORKSPACES);
    let states = cache.join(crate::worktree::WORKTREES);
    let state_names: HashSet<String> = list(&states)
        .into_iter()
        .filter(|(_, file_type)| file_type.is_some_and(|file_type| file_type.is_dir()))
        .map(|(name, _)| name)
        .collect();
    let mut recorded = HashSet::new();
    for (name, _) in list(&records) {
        if dotted(&name) {
            continue;
        }
        let record = records.join(&name);
        recorded.insert(name.clone());
        if !state_names.contains(&name) {
            sweeps.push(Sweep {
                path: record,
                category: "worktrees",
                slot: None,
                also: None,
            });
            continue;
        }
        let Ok(bytes) = fs::read(&record) else {
            continue;
        };
        let workspace = PathBuf::from(std::ffi::OsString::from(
            String::from_utf8_lossy(bytes.strip_suffix(b"\n").unwrap_or(&bytes)).into_owned(),
        ));
        let gone = workspace.is_absolute()
            && matches!(fs::symlink_metadata(&workspace), Err(error) if error.kind() == ErrorKind::NotFound);
        if !gone {
            continue;
        }
        // Gone: at once when its parent directory is still there (the
        // checkout was removed), else when the state is old (an unmounted
        // file system comes back).
        let parent_there = workspace.parent().is_some_and(Path::is_dir);
        let old = fs::symlink_metadata(&record)
            .is_ok_and(|metadata| context.max_age_passed(last_use(&metadata)));
        if parent_there || old {
            sweeps.push(Sweep {
                path: states.join(&name),
                category: "worktrees",
                slot: None,
                also: Some(record),
            });
        }
    }
    // States without a record (written by binaries before the collector):
    // removed when nothing in them changed for the age limit.
    for name in state_names.difference(&recorded) {
        let state = states.join(name);
        let changed = ["", "snapshots", "MODULE.bazel.lock", "execroot"]
            .iter()
            .filter_map(|part| fs::symlink_metadata(state.join(part)).ok())
            .map(|metadata| metadata.mtime())
            .max()
            .unwrap_or(0);
        if context.max_age_passed(changed) {
            sweeps.push(Sweep {
                path: state,
                category: "worktrees",
                slot: None,
                also: None,
            });
        }
    }
    Scan {
        items,
        sweeps,
        pointers,
        links,
        mutable,
        index,
    }
}

impl Context<'_> {
    /// Whether a state last used at `last` is older than the general age
    /// limit (60 days when there is none).
    fn max_age_passed(&self, last: i64) -> bool {
        let age = i64::try_from(super::DEFAULT_MAX_AGE.as_secs()).unwrap_or(i64::MAX);
        last < self.now.saturating_sub(age)
    }
}

fn seconds(duration: Duration) -> i64 {
    i64::try_from(duration.as_secs()).unwrap_or(i64::MAX)
}

/// Collect `cache` under `policy`. `lock` is the exclusive cache lock (None:
/// the cache could not be locked; then only a dry run is honest, and the
/// collection runs anyway as one). Released before the trash is emptied.
pub fn collect(cache: &Path, policy: &Policy, mode: Mode, lock: Option<File>) -> Report {
    collect_at(cache, policy, mode, lock, now_secs())
}

/// The crash test hook: `BAZEL_RS_CACHE_GC_ABORT_AFTER=<n>` kills the
/// process (SIGKILL: no unwinding, no core) at the `n`th removal step
/// (tests/cache_gc.rs: a killed collection leaves a consistent cache).
fn step() {
    static LIMIT: std::sync::OnceLock<Option<u64>> = std::sync::OnceLock::new();
    static STEPS: AtomicU64 = AtomicU64::new(0);
    let limit = LIMIT.get_or_init(|| {
        std::env::var("BAZEL_RS_CACHE_GC_ABORT_AFTER")
            .ok()
            .and_then(|value| value.parse().ok())
    });
    if let Some(limit) = limit
        && STEPS.fetch_add(1, Ordering::Relaxed) + 1 >= *limit
    {
        let _ =
            rustix::process::kill_process(rustix::process::getpid(), rustix::process::Signal::KILL);
        std::process::abort();
    }
}

/// [`collect`] as if it were `now` (seconds since the epoch).
fn collect_at(cache: &Path, policy: &Policy, mode: Mode, lock: Option<File>, now: i64) -> Report {
    let started = Instant::now();
    let dry_run = matches!(mode, Mode::Explicit { dry_run: true }) || lock.is_none();
    let automatic = mode == Mode::Automatic;
    let gc = super::directory(cache);
    let _ = fs::create_dir_all(&gc);
    // What a killed or bounded collection left in the trash goes first.
    let left_orphans = if dry_run {
        HashSet::new()
    } else {
        empty_trash(
            &gc.join(super::TRASH),
            automatic.then(|| Instant::now() + AUTOMATIC_TRASH / 2),
        )
        .1
    };
    let context = Context {
        cache,
        store: cache.join("repositories"),
        now,
        stale: now.saturating_sub(seconds(STALE_TEMPORARY)),
        index: Index::read(&gc.join(INDEX)),
        deadline: automatic.then(|| Instant::now() + AUTOMATIC_SIZING),
    };
    let mut report = Report {
        dry_run,
        ..Report::default()
    };
    let mut phase = Instant::now();
    let mut lap = |report: &mut Report, name: &'static str| {
        report.phases.push((name, phase.elapsed()));
        phase = Instant::now();
    };
    report.phases.push(("trash and index", started.elapsed()));
    let scan = scan(&context);
    lap(&mut report, "scan and sizing");
    let Scan {
        mut items,
        sweeps,
        pointers,
        links,
        mutable,
        index,
    } = scan;
    // Blobs that only entries an earlier collection removed linked: their
    // last use went with those entries (emptying the trash just changed
    // their ctime).
    for item in &mut items {
        if item.kind == Kind::Blob && item.links == 1 && left_orphans.contains(&item.inode) {
            item.last_use = 0;
        }
    }
    // Sizes: the trees' units include their mutable slots.
    let unit_size = |item: &Item| -> Option<u64> {
        let slots: u64 = if item.kind == Kind::Tree(TreeKind::Direct) {
            mutable
                .get(&item.name)
                .map(|slots| slots.iter().map(|(_, _, size)| size).sum())
                .unwrap_or(0)
        } else {
            0
        };
        item.size.map(|size| size + slots)
    };
    let sized = items.iter().all(|item| item.size.is_some());
    report.sized = sized;
    for item in &items {
        report.count(item.kind.category(), unit_size(item).unwrap_or(0));
    }
    let total: u64 = items.iter().filter_map(&unit_size).sum();
    // Decide.
    let recent_cutoff = now.saturating_sub(seconds(policy.recent));
    let age_cutoff = |kind: Kind| -> Option<i64> {
        let age = match kind {
            Kind::Tree(_) => policy.repo_max_age,
            _ => policy.max_age,
        }?;
        Some(now.saturating_sub(seconds(age)))
    };
    let mut victims: Vec<bool> = items
        .iter()
        .map(|item| {
            // Blobs an entry still links are never removed (that frees
            // nothing).
            if item.kind == Kind::Blob && item.links > 1 {
                return false;
            }
            age_cutoff(item.kind).is_some_and(|cutoff| item.last_use < cutoff)
        })
        .collect();
    // Unreferenced direct trees (superseded publications: no pointer, slot
    // or marker names them) unused for the recent window.
    for (index, item) in items.iter().enumerate() {
        if item.kind == Kind::Tree(TreeKind::Direct)
            && !pointers.contains_key(&item.name)
            && !links.contains_key(&item.name)
            && !mutable.contains_key(&item.name)
            && item.last_use < recent_cutoff
        {
            victims[index] = true;
        }
    }
    // The size budget, least recently used first.
    let budget = match policy.budget {
        Budget::Off => None,
        Budget::Bytes(bytes) => Some(bytes),
        Budget::Auto => {
            let available = rustix::fs::statvfs(cache)
                .map(|stat| stat.f_bavail.saturating_mul(stat.f_frsize))
                .unwrap_or(0);
            Some(super::automatic_budget(available, total))
        }
    };
    report.budget = budget.filter(|_| sized);
    let mut remaining: u64 = items
        .iter()
        .zip(&victims)
        .filter(|(_, victim)| !**victim)
        .filter_map(|(item, _)| unit_size(item))
        .sum();
    if let Some(budget) = report.budget
        && remaining > budget
    {
        let mut order: Vec<usize> = (0..items.len())
            .filter(|&index| {
                let item = &items[index];
                !victims[index]
                    && item.last_use < recent_cutoff
                    && !(item.kind == Kind::Blob && item.links > 1)
            })
            .collect();
        order.sort_by_key(|&index| (items[index].last_use, items[index].kind));
        for index in order {
            if remaining <= budget {
                break;
            }
            victims[index] = true;
            remaining = remaining.saturating_sub(unit_size(&items[index]).unwrap_or(0));
        }
    }
    // Kept direct trees keep the trees their bytes name.
    let by_name: HashMap<(Kind, &str), usize> = items
        .iter()
        .enumerate()
        .map(|(index, item)| ((item.kind, item.name.as_str()), index))
        .collect();
    let mut pending: Vec<usize> = (0..items.len())
        .filter(|&index| !victims[index] && !items[index].refs.is_empty())
        .collect();
    while let Some(index) = pending.pop() {
        for reference in &items[index].refs {
            let target = TreeKind::ALL.into_iter().find_map(|kind| {
                let id = reference.strip_prefix(kind.trees())?.strip_prefix('/')?;
                by_name.get(&(Kind::Tree(kind), id)).copied()
            });
            if let Some(target) = target
                && victims[target]
            {
                victims[target] = false;
                pending.push(target);
            }
        }
    }
    lap(&mut report, "decide");
    // Remove.
    let mut removed_names: HashSet<(Kind, String)> = HashSet::new();
    let busy = AtomicU64::new(0);
    let trash = Trash::new(&gc);
    for sweep in &sweeps {
        let bytes = disk_size(&sweep.path, None).unwrap_or(0);
        report.count(sweep.category, bytes);
        if dry_run || remove_sweep(&context, sweep, &trash) {
            report.deleted(sweep.category, bytes);
        }
    }
    // Entries before blobs: removing an entry can leave its blob
    // unlinked.
    let mut order: Vec<usize> = (0..items.len()).filter(|&index| victims[index]).collect();
    order.sort_by_key(|&index| items[index].kind == Kind::Blob);
    for index in order {
        let item = &items[index];
        let removed =
            dry_run || remove_item(&context, item, &trash, &pointers, &links, &mutable, &busy);
        if removed {
            report.deleted(item.kind.category(), unit_size(item).unwrap_or(0));
            if !dry_run {
                removed_names.insert((item.kind, item.name.clone()));
            }
        }
    }
    report.busy = busy.load(Ordering::Relaxed);
    // The index of what stays (a dry run records the sizes it measured).
    {
        let kept: Vec<IndexRecord> = index
            .into_iter()
            .filter(|record| {
                let kind = match record.code {
                    0 => Kind::Action,
                    1 => Kind::Test,
                    3 => Kind::Tree(TreeKind::Direct),
                    4 => Kind::Tree(TreeKind::Registry),
                    5 => Kind::Tree(TreeKind::Git),
                    6 => Kind::Tree(TreeKind::Builtin),
                    _ => return true,
                };
                !removed_names.contains(&(kind, record.name.clone()))
            })
            .collect();
        Index::write(&gc.join(INDEX), &kept);
    }
    lap(&mut report, "remove");
    // The cache is consistent: the lock goes, then the trash.
    drop(lock);
    if !dry_run {
        let (emptied, orphans) = empty_trash(
            &trash.root,
            automatic.then(|| Instant::now() + AUTOMATIC_TRASH),
        );
        report.trash_emptied = emptied;
        // Blobs that only the removed entries linked are unlinked now; the
        // scan saw their last use.
        let freed = collect_unlinked_blobs(
            cache,
            &items,
            &victims,
            &orphans,
            remaining,
            report.budget,
            recent_cutoff,
            age_cutoff(Kind::Blob),
        );
        for bytes in freed {
            report.deleted("cas", bytes);
        }
        super::write_stamp(cache, !sized || !emptied);
        lap(&mut report, "trash after");
    }
    report.elapsed = started.elapsed();
    report
}

/// After the trash was emptied: the blobs that only the removed entries
/// linked (`orphans`: what emptying the trash left with one link), removed
/// by the age limit and the budget like every entry, by the use the scan
/// saw. Returns the bytes of each removed.
#[allow(clippy::too_many_arguments)]
fn collect_unlinked_blobs(
    cache: &Path,
    items: &[Item],
    victims: &[bool],
    orphans: &HashSet<(u64, u64)>,
    mut remaining: u64,
    budget: Option<u64>,
    recent_cutoff: i64,
    age_cutoff: Option<i64>,
) -> Vec<u64> {
    let cas = cache.join("cas");
    let mut candidates: Vec<(i64, u64, PathBuf)> = items
        .iter()
        .zip(victims)
        .filter(|(item, victim)| {
            item.kind == Kind::Blob && !**victim && orphans.contains(&item.inode)
        })
        .filter_map(|(item, _)| {
            let path = cas.join(&item.name);
            let metadata = fs::symlink_metadata(&path).ok()?;
            (metadata.is_file() && metadata.nlink() == 1).then(|| {
                // Its share was counted; now all of it is its own.
                remaining = remaining
                    .saturating_sub(item.size.unwrap_or(0))
                    .saturating_add(share(&metadata));
                (item.last_use, share(&metadata), path)
            })
        })
        .collect();
    candidates.sort();
    let mut freed = Vec::new();
    for (last, bytes, path) in candidates {
        let expired = age_cutoff.is_some_and(|cutoff| last < cutoff);
        let over = budget.is_some_and(|budget| remaining > budget) && last < recent_cutoff;
        if (expired || over) && {
            step();
            fs::remove_file(&path).is_ok()
        } {
            remaining = remaining.saturating_sub(bytes);
            freed.push(bytes);
        }
    }
    freed
}

/// `<cache>/gc/trash`: what a collection removes is renamed here first.
struct Trash {
    root: PathBuf,
    next: AtomicU64,
}

impl Trash {
    fn new(gc: &Path) -> Self {
        Self {
            root: gc.join(super::TRASH),
            next: AtomicU64::new(0),
        }
    }

    /// Move `path` into the trash (one rename); a directory without write
    /// permission is made writable first (moving a directory updates its
    /// `..`). A path on another file system is removed in place.
    fn take(&self, path: &Path) -> bool {
        let _ = fs::create_dir_all(&self.root);
        let name = format!(
            "{}-{}",
            std::process::id(),
            self.next.fetch_add(1, Ordering::Relaxed)
        );
        let destination = self.root.join(name);
        step();
        match fs::rename(path, &destination) {
            Ok(()) => true,
            Err(error) if error.kind() == ErrorKind::NotFound => false,
            Err(error) if error.kind() == ErrorKind::PermissionDenied => {
                make_writable(path);
                fs::rename(path, &destination).is_ok()
            }
            Err(_) => remove_in_place(path, None, None),
        }
    }
}

fn make_writable(path: &Path) {
    if let Ok(metadata) = fs::symlink_metadata(path)
        && metadata.is_dir()
    {
        let mode = metadata.permissions().mode();
        if mode & 0o700 != 0o700 {
            let _ = fs::set_permissions(path, fs::Permissions::from_mode(mode | 0o700));
        }
    }
}

/// Files (device, inode) whose removal left them one link: a blob that
/// only removed entries linked.
type Inodes = Mutex<HashSet<(u64, u64)>>;

/// Remove `path` (a file, a link or a whole directory, never following
/// links; read-only directories are made writable), stopping at `deadline`.
/// True when it is gone. Regular files of two links note their inode in
/// `orphans` (their other link may be a blob nothing else links now).
fn remove_in_place(path: &Path, deadline: Option<Instant>, orphans: Option<&Inodes>) -> bool {
    let Ok(metadata) = fs::symlink_metadata(path) else {
        return true;
    };
    if !metadata.is_dir() {
        let removed = fs::remove_file(path).is_ok();
        if removed
            && metadata.is_file()
            && metadata.nlink() == 2
            && let Some(orphans) = orphans
        {
            orphans
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .insert((metadata.dev(), metadata.ino()));
        }
        return removed
            || fs::symlink_metadata(path).is_err_and(|error| error.kind() == ErrorKind::NotFound);
    }
    make_writable(path);
    let mut complete = true;
    if let Ok(entries) = fs::read_dir(path) {
        for entry in entries.flatten() {
            if deadline.is_some_and(|deadline| Instant::now() > deadline) {
                return false;
            }
            complete &= remove_in_place(&entry.path(), deadline, orphans);
        }
    }
    complete
        && (fs::remove_dir(path).is_ok()
            || fs::symlink_metadata(path).is_err_and(|error| error.kind() == ErrorKind::NotFound))
}

/// Delete what is in the trash (several processes may: whoever is first).
/// Whether it is empty, and the files it left with one link
/// ([`remove_in_place`]).
fn empty_trash(trash: &Path, deadline: Option<Instant>) -> (bool, HashSet<(u64, u64)>) {
    let entries = list(trash);
    let paths: Vec<PathBuf> = entries.iter().map(|(name, _)| trash.join(name)).collect();
    let complete = std::sync::atomic::AtomicBool::new(true);
    let orphans = Inodes::default();
    super::parallel(&paths, 8, |path| {
        if !remove_in_place(path, deadline, Some(&orphans)) {
            complete.store(false, Ordering::Relaxed);
        }
    });
    let orphans = orphans
        .into_inner()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    (
        complete.load(Ordering::Relaxed) && list(trash).is_empty(),
        orphans,
    )
}

fn remove_sweep(context: &Context<'_>, sweep: &Sweep, trash: &Trash) -> bool {
    // Still abandoned (nothing changed it since the scan)?
    let _guard = match &sweep.slot {
        Some(name) if !name.starts_with('#') => {
            match crate::repository_slots::SlotWriter::try_acquire(&context.store, name) {
                Ok(Some(writer)) => Some(writer),
                _ => return false,
            }
        }
        _ => None,
    };
    let removed = match fs::symlink_metadata(&sweep.path) {
        Ok(metadata) if metadata.is_dir() => trash.take(&sweep.path),
        Ok(_) => {
            step();
            fs::remove_file(&sweep.path).is_ok()
        }
        Err(_) => false,
    };
    if removed && let Some(also) = &sweep.also {
        let _ = fs::remove_file(also);
    }
    removed
}

fn remove_item(
    context: &Context<'_>,
    item: &Item,
    trash: &Trash,
    pointers: &HashMap<String, Vec<PathBuf>>,
    links: &HashMap<String, Vec<String>>,
    mutable: &HashMap<String, Vec<(String, PathBuf, u64)>>,
    busy: &AtomicU64,
) -> bool {
    let cache = context.cache;
    let store = &context.store;
    // Used since the scan (an earlier binary meanwhile)?
    if item
        .anchor
        .as_ref()
        .is_some_and(|anchor| !anchor.unchanged())
    {
        return false;
    }
    let held = |path: PathBuf| -> Result<Option<File>, ()> {
        try_lock(&path).inspect_err(|()| {
            busy.fetch_add(1, Ordering::Relaxed);
        })
    };
    match item.kind {
        Kind::Action => {
            let Ok(_lock) = held(cache.join("locks").join(format!("{}.lock", item.name))) else {
                return false;
            };
            trash.take(&cache.join("actions").join(&item.name))
        }
        Kind::Test => {
            let Ok(_lock) = held(
                cache
                    .join("tests")
                    .join("locks")
                    .join(format!("{}.lock", item.name)),
            ) else {
                return false;
            };
            trash.take(&cache.join("tests").join(&item.name))
        }
        Kind::Download => {
            step();
            let removed = fs::remove_file(store.join("downloads").join(&item.name)).is_ok();
            let ids = store.join("download_ids").join(&item.name);
            if removed && ids.is_dir() {
                trash.take(&ids);
            }
            removed
        }
        Kind::Blob => {
            // Never one an entry links (now).
            let path = cache.join("cas").join(&item.name);
            fs::symlink_metadata(&path).is_ok_and(|metadata| metadata.nlink() == 1) && {
                step();
                fs::remove_file(&path).is_ok()
            }
        }
        Kind::Tree(kind) => {
            let Ok(_lock) = held(store.join(kind.lock(&item.name))) else {
                return false;
            };
            // The slots that use it, all or none.
            let mut writers = Vec::new();
            let slot_names: Vec<&String> = links
                .get(&item.name)
                .into_iter()
                .flatten()
                .chain(
                    mutable
                        .get(&item.name)
                        .into_iter()
                        .flatten()
                        .map(|(canonical, _, _)| canonical),
                )
                .collect();
            for name in &slot_names {
                if name.starts_with('#') {
                    continue;
                }
                match crate::repository_slots::SlotWriter::try_acquire(store, name) {
                    Ok(Some(writer)) => writers.push(writer),
                    _ => {
                        busy.fetch_add(1, Ordering::Relaxed);
                        return false;
                    }
                }
            }
            // What makes it trusted first: the pointers naming it (still
            // naming it), its slots, its marker, its manifest; then the
            // tree.
            for pointer in pointers.get(&item.name).into_iter().flatten() {
                if pointer_identity(pointer).as_deref() == Some(item.name.as_str()) {
                    step();
                    let _ = fs::remove_file(pointer);
                }
            }
            let slots = store.join("external");
            for name in links.get(&item.name).into_iter().flatten() {
                let path = slots.join(name);
                if fs::read_link(&path)
                    .is_ok_and(|target| target == crate::repository_slots::link_target(&item.name))
                {
                    step();
                    let _ = fs::remove_file(&path);
                }
            }
            for (canonical, marker, _) in mutable.get(&item.name).into_iter().flatten() {
                if read_marker(marker).is_some_and(|(identity, _)| identity == item.name) {
                    step();
                    let _ = fs::remove_file(marker);
                    trash.take(&crate::repository_slots::slot_path(store, canonical));
                }
            }
            let manifest = store.join(kind.manifest(&item.name));
            step();
            let _ = fs::remove_file(&manifest);
            let removed = trash.take(&store.join(kind.trees()).join(&item.name));
            let _ = fs::remove_file(super::tree_stamp(cache, kind, &item.name));
            drop(writers);
            removed
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const DAY: i64 = 24 * 60 * 60;

    /// A fake cache whose collections run 100 days in the future, so that
    /// entries can be dated by setting their (future) times: what is not
    /// dated is 100 days old, like every ctime (which cannot be set).
    struct Fixture {
        dir: tempfile::TempDir,
        now: i64,
    }

    fn set_times(path: &Path, secs: i64) {
        use rustix::fs::{AtFlags, CWD, Timespec, Timestamps};
        let time = Timespec {
            tv_sec: secs,
            tv_nsec: 0,
        };
        rustix::fs::utimensat(
            CWD,
            path,
            &Timestamps {
                last_access: time,
                last_modification: time,
            },
            AtFlags::SYMLINK_NOFOLLOW,
        )
        .unwrap();
    }

    fn policy(budget: Budget, max_age: Option<i64>, repo_max_age: Option<i64>) -> Policy {
        let days = |days: i64| Duration::from_secs(u64::try_from(days * DAY).unwrap());
        Policy {
            budget,
            max_age: max_age.map(days),
            repo_max_age: repo_max_age.map(days),
            idle_delay: Duration::ZERO,
            recent: days(1),
        }
    }

    fn id(n: u8) -> String {
        format!("{n:02x}").repeat(32)
    }

    impl Fixture {
        fn new() -> Self {
            Self {
                dir: tempfile::tempdir().unwrap(),
                now: now_secs() + 100 * DAY,
            }
        }

        fn cache(&self) -> &Path {
            self.dir.path()
        }

        fn store(&self) -> PathBuf {
            self.cache().join("repositories")
        }

        /// An action entry with one output of `bytes`, last used `ago`
        /// seconds before the fixture's now (None: 100 days).
        fn action(&self, key: &str, bytes: usize, ago: Option<i64>) -> PathBuf {
            let entry = self.cache().join("actions").join(key);
            fs::create_dir_all(entry.join("outputs")).unwrap();
            fs::write(entry.join("outputs/out"), vec![b'x'; bytes]).unwrap();
            fs::write(entry.join("manifest.json"), b"{}").unwrap();
            if let Some(ago) = ago {
                set_times(&entry.join("manifest.json"), self.now - ago);
            }
            entry
        }

        /// A direct tree with its manifest (`refs` named), last used `ago`.
        fn direct_tree(&self, n: u8, refs: &[String], ago: Option<i64>) -> PathBuf {
            let store = self.store();
            let tree = store.join("direct/trees").join(id(n));
            fs::create_dir_all(tree.join("sub")).unwrap();
            fs::write(tree.join("sub/file"), b"content").unwrap();
            fs::create_dir_all(store.join("direct/manifests")).unwrap();
            let manifest = store
                .join("direct/manifests")
                .join(format!("{}.json", id(n)));
            let refs: Vec<String> = refs.iter().map(|r| format!("\"{r}\"")).collect();
            fs::write(
                &manifest,
                format!("{{\"format\":1,\"refs\":[{}]}}", refs.join(",")),
            )
            .unwrap();
            if let Some(ago) = ago {
                set_times(&manifest, self.now - ago);
            }
            tree
        }

        fn pointer(&self, spec: &str, n: u8) -> PathBuf {
            let specs = self.store().join("direct/specs");
            fs::create_dir_all(&specs).unwrap();
            let path = specs.join(spec);
            fs::write(
                &path,
                format!("{{\"format\":1,\"identity\":\"{}\"}}\n", id(n)),
            )
            .unwrap();
            path
        }

        fn registry_tree(&self, n: u8, ago: Option<i64>) -> PathBuf {
            let trees = self.store().join("trees");
            fs::create_dir_all(trees.join(id(n))).unwrap();
            fs::write(trees.join(id(n)).join("MODULE.bazel"), b"module()").unwrap();
            let manifest = trees.join(format!("{}.json", id(n)));
            fs::write(&manifest, b"{}").unwrap();
            if let Some(ago) = ago {
                set_times(&manifest, self.now - ago);
            }
            trees.join(id(n))
        }

        fn collect(&self, policy: &Policy) -> Report {
            self.run(policy, Mode::Explicit { dry_run: false })
        }

        fn run(&self, policy: &Policy, mode: Mode) -> Report {
            let directory = super::super::directory(self.cache());
            fs::create_dir_all(&directory).unwrap();
            let lock = File::create(directory.join("lock")).unwrap();
            collect_at(self.cache(), policy, mode, Some(lock), self.now)
        }
    }

    #[test]
    fn the_size_budget_keeps_the_most_recently_used_entries() {
        let fixture = Fixture::new();
        let entries: Vec<PathBuf> = (0..5)
            .map(|n| fixture.action(&id(n), 64 << 10, Some((10 - 2 * i64::from(n)) * DAY)))
            .collect();
        // Used within the recent window: never removed by the budget.
        let fresh = fixture.action(&id(9), 64 << 10, Some(3600));
        let dry = fixture.run(
            &policy(Budget::Off, None, None),
            Mode::Explicit { dry_run: true },
        );
        assert_eq!(dry.deleted_entries, 0);
        let per_entry = dry.total_bytes / 6;
        let report = fixture.collect(&policy(
            Budget::Bytes(per_entry * 4 + per_entry / 2),
            None,
            None,
        ));
        assert_eq!(report.deleted_entries, 2, "{report:?}");
        assert!(!entries[0].exists() && !entries[1].exists());
        assert!(entries[2..].iter().all(|entry| entry.exists()));
        assert!(fresh.exists());
        // Even a budget of one byte leaves what was used recently.
        fixture.collect(&policy(Budget::Bytes(1), None, None));
        assert!(entries.iter().all(|entry| !entry.exists()));
        assert!(fresh.exists());
        assert!(list(&fixture.cache().join("gc/trash")).is_empty());
    }

    #[test]
    fn age_limits_remove_unused_entries_and_whole_trees() {
        let fixture = Fixture::new();
        let old = fixture.action(&id(1), 10, Some(61 * DAY));
        let kept = fixture.action(&id(2), 10, Some(59 * DAY));
        let old_tree = fixture.direct_tree(3, &[], Some(15 * DAY));
        let old_pointer = fixture.pointer("spec-a", 3);
        let kept_tree = fixture.direct_tree(4, &[], Some(13 * DAY));
        let kept_pointer = fixture.pointer("spec-b", 4);
        // An ordinary slot linking the old tree goes with it.
        let slots = fixture.store().join("external");
        fs::create_dir_all(&slots).unwrap();
        fs::create_dir_all(fixture.store().join("direct/locks")).unwrap();
        std::os::unix::fs::symlink(
            crate::repository_slots::link_target(&id(3)),
            slots.join("old_repo"),
        )
        .unwrap();
        std::os::unix::fs::symlink(
            crate::repository_slots::link_target(&id(4)),
            slots.join("kept_repo"),
        )
        .unwrap();
        // A stamp newer than the manifest is the tree's last use.
        let stamped = fixture.direct_tree(5, &[], Some(30 * DAY));
        fixture.pointer("spec-c", 5);
        let stamp = super::super::tree_stamp(fixture.cache(), TreeKind::Direct, &id(5));
        fs::create_dir_all(stamp.parent().unwrap()).unwrap();
        fs::write(&stamp, b"").unwrap();
        set_times(&stamp, fixture.now - DAY);
        let report = fixture.collect(&policy(Budget::Off, Some(60), Some(14)));
        assert!(!old.exists() && kept.exists());
        assert!(!old_tree.exists() && !old_pointer.exists());
        assert!(
            !fixture
                .store()
                .join(format!("direct/manifests/{}.json", id(3)))
                .exists()
        );
        assert!(fs::symlink_metadata(slots.join("old_repo")).is_err());
        assert!(kept_tree.exists() && kept_pointer.exists());
        assert!(slots.join("kept_repo").exists());
        assert!(stamped.exists());
        assert_eq!(report.categories["actions"].deleted, 1);
        assert_eq!(report.categories["repositories"].deleted, 1);
    }

    #[test]
    fn unreferenced_trees_go_after_the_recent_window() {
        let fixture = Fixture::new();
        let orphan = fixture.direct_tree(1, &[], Some(2 * DAY));
        let new_orphan = fixture.direct_tree(2, &[], Some(3600));
        let referenced = fixture.direct_tree(3, &[], Some(2 * DAY));
        fixture.pointer("spec", 3);
        fixture.collect(&policy(Budget::Off, None, None));
        assert!(!orphan.exists());
        assert!(new_orphan.exists() && referenced.exists());
    }

    #[test]
    fn kept_trees_keep_the_trees_their_bytes_name() {
        let fixture = Fixture::new();
        let named = fixture.registry_tree(1, Some(30 * DAY));
        let unnamed = fixture.registry_tree(2, Some(30 * DAY));
        let old_named = fixture.direct_tree(3, &[], Some(30 * DAY));
        fixture.pointer("old", 3);
        let user = fixture.direct_tree(
            4,
            &[
                format!("trees/{}", id(1)),
                format!("direct/trees/{}", id(3)),
            ],
            Some(DAY / 2),
        );
        fixture.pointer("user", 4);
        fixture.collect(&policy(Budget::Off, None, Some(14)));
        assert!(user.exists() && named.exists() && old_named.exists());
        assert!(!unnamed.exists());
    }

    #[test]
    fn blobs_stay_while_an_entry_links_them() {
        let fixture = Fixture::new();
        let cas = fixture.cache().join("cas");
        fs::create_dir_all(&cas).unwrap();
        // Linked by a recently used entry: stays.
        let kept_entry = fixture.action(&id(1), 100, Some(3600));
        let linked = cas.join(format!("{}-555", id(1)));
        fs::hard_link(kept_entry.join("outputs/out"), &linked).unwrap();
        // Linked by an entry the age limit removes: goes with it.
        let old_entry = fixture.action(&id(2), 100, Some(90 * DAY));
        let orphaned = cas.join(format!("{}-555", id(2)));
        fs::hard_link(old_entry.join("outputs/out"), &orphaned).unwrap();
        // A distinct-inode copy nobody links, unused for 100 days: goes.
        let copy = cas.join(format!("{}-444.1", id(3)));
        fs::write(&copy, b"copy").unwrap();
        let report = fixture.collect(&policy(Budget::Off, Some(60), None));
        assert!(linked.exists() && kept_entry.exists());
        assert!(!old_entry.exists() && !orphaned.exists());
        assert!(!copy.exists());
        assert_eq!(report.categories["cas"].deleted, 2, "{report:?}");
        // Without an age limit an unlinked blob stays (the budget is off).
        let copy = cas.join(format!("{}-444.2", id(3)));
        fs::write(&copy, b"copy").unwrap();
        fixture.collect(&policy(Budget::Off, None, None));
        assert!(copy.exists());
    }

    #[test]
    fn blobs_only_removed_entries_linked_go_by_the_entries_last_use() {
        let fixture = Fixture::new();
        let cas = fixture.cache().join("cas");
        fs::create_dir_all(&cas).unwrap();
        // Every blob was last linked 100 days ago (its ctime); the age limit
        // is 200 days.
        let old_entry = fixture.action(&id(1), 100, Some(300 * DAY));
        let removed_now = cas.join(format!("{}-555", id(1)));
        fs::hard_link(old_entry.join("outputs/out"), &removed_now).unwrap();
        // Left in the trash by an earlier collection: its blob's last use
        // went with it.
        let left = fixture.cache().join("gc/trash/1-0");
        fs::create_dir_all(&left).unwrap();
        fs::write(left.join("out"), b"left").unwrap();
        let removed_earlier = cas.join(format!("{}-555", id(2)));
        fs::hard_link(left.join("out"), &removed_earlier).unwrap();
        // Unlinked all along, used 100 days ago: within the limit.
        let unlinked = cas.join(format!("{}-444", id(3)));
        fs::write(&unlinked, b"source").unwrap();
        fixture.collect(&policy(Budget::Off, Some(200), None));
        assert!(!old_entry.exists());
        assert!(fs::symlink_metadata(&removed_now).is_ok());
        assert!(fs::symlink_metadata(&removed_earlier).is_err());
        assert!(unlinked.exists());
    }

    #[test]
    fn abandoned_temporaries_and_deleted_worktrees_are_swept() {
        let fixture = Fixture::new();
        let cache = fixture.cache();
        let old_work = cache.join("work/key-1-1");
        fs::create_dir_all(old_work.join("execroot")).unwrap();
        let new_work = cache.join("work/key-2-1");
        fs::create_dir_all(&new_work).unwrap();
        set_times(&new_work, fixture.now - 3600);
        let tmp = cache.join("cas/.tmp-x-1-2");
        fs::create_dir_all(tmp.parent().unwrap()).unwrap();
        fs::write(&tmp, b"partial").unwrap();
        // A read-only staging directory of a killed repository rule.
        let stage = fixture.store().join("direct/trees/.stage-abc");
        fs::create_dir_all(stage.join("ro")).unwrap();
        fs::write(stage.join("ro/file"), b"x").unwrap();
        fs::set_permissions(stage.join("ro"), fs::Permissions::from_mode(0o555)).unwrap();
        // Worktrees: one deleted, one alive.
        let workspaces = tempfile::tempdir().unwrap();
        let alive = workspaces.path().join("alive");
        fs::create_dir_all(&alive).unwrap();
        let gone = workspaces.path().join("gone");
        let records = cache.join("gc/worktrees");
        fs::create_dir_all(&records).unwrap();
        for (name, path) in [("alive-1", &alive), ("gone-2", &gone)] {
            fs::create_dir_all(cache.join("worktrees").join(name).join("snapshots")).unwrap();
            fs::write(records.join(name), format!("{}\n", path.display())).unwrap();
        }
        fixture.collect(&policy(Budget::Off, None, None));
        assert!(!old_work.exists() && new_work.exists());
        assert!(!tmp.exists() && !stage.exists());
        assert!(cache.join("worktrees/alive-1").exists() && records.join("alive-1").exists());
        assert!(!cache.join("worktrees/gone-2").exists() && !records.join("gone-2").exists());
        assert!(list(&cache.join("gc/trash")).is_empty());
    }

    #[test]
    fn a_killed_collection_leaves_untrusted_leftovers_that_the_next_one_removes() {
        let fixture = Fixture::new();
        let store = fixture.store();
        // Killed after the pointer and manifest went: a tree nothing
        // trusts (no manifest), removed once old.
        let tree = fixture.direct_tree(1, &[], Some(30 * DAY));
        fs::remove_file(store.join(format!("direct/manifests/{}.json", id(1)))).unwrap();
        // Killed after the manifest of another went, its pointer left:
        // dangling, removed once old.
        let pointer = fixture.pointer("spec", 2);
        // Killed after a rename into the trash (read-only inside).
        let trash = fixture.cache().join("gc/trash/1-0");
        fs::create_dir_all(trash.join("outputs")).unwrap();
        fs::write(trash.join("outputs/out"), b"x").unwrap();
        fs::set_permissions(trash.join("outputs"), fs::Permissions::from_mode(0o555)).unwrap();
        fixture.collect(&policy(Budget::Off, None, None));
        assert!(!tree.exists() && !pointer.exists());
        assert!(list(&fixture.cache().join("gc/trash")).is_empty());
    }

    #[test]
    fn a_dry_run_removes_nothing_and_the_index_remembers_sizes() {
        let fixture = Fixture::new();
        let entry = fixture.action(&id(1), 1000, Some(90 * DAY));
        let tree = fixture.direct_tree(2, &[], Some(3600));
        fixture.pointer("spec", 2);
        let dry = fixture.run(
            &policy(Budget::Off, Some(60), None),
            Mode::Explicit { dry_run: true },
        );
        assert_eq!(dry.deleted_entries, 1);
        assert!(entry.exists());
        // An automatic run sizes, records the sizes, removes the old entry.
        let report = fixture.run(&policy(Budget::Off, Some(60), None), Mode::Automatic);
        assert!(report.sized && !entry.exists() && tree.exists());
        let index = Index::read(&fixture.cache().join("gc/index"));
        assert!(index.records.contains_key(&(3, id(2))));
        assert!(index.records.keys().all(|(code, _)| *code != 0));
        let stamp = fs::read_to_string(fixture.cache().join("gc/stamp")).unwrap();
        assert_eq!(stamp, "bazel-rs cache gc 1\n");
    }
}
