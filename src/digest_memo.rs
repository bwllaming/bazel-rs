//! Persistent file-digest memo (PLAN-L2 E1b/E2): the SHA-256 of a file by
//! its path and stat key, kept across processes in one binary table per
//! cache, `<cache>/digests/v2.dmemo`, like Bazel's DigestUtils cache
//! (vfs/DigestUtils.java:36-60: a digest is reused while the file keeps its
//! node id, change time, modification time and size) made persistent,
//! because every bazel-rs command is a fresh process.
//!
//! The file is a 32-byte header — magic `BZRSDMEM`, format version (u32),
//! record size (u32), record count (u64) and the first 8 bytes of the
//! blake3 hash of the body (u64) — followed by fixed-width 88-byte records,
//! all little-endian:
//!
//! | offset | field |
//! |---|---|
//! | 0 | path hash: the first 8 bytes of blake3 of the path's bytes (u64) |
//! | 8 | device (u64) |
//! | 16 | inode (u64) |
//! | 24 | size (u64) |
//! | 32 | modification time, ns since the epoch (i64) |
//! | 40 | change slot, ns (i64): the change time (ctime), or for a blob or published output its birth time |
//! | 48 | SHA-256 (32 bytes) |
//! | 80 | epoch (u32): the save that wrote the record |
//! | 84 | flags (u32): [`OWNED`] (the change slot holds the birth time), [`TRUSTED`] |
//!
//! Records are sorted by (path hash, OWNED) with one record per pair, so a
//! lookup is a binary search directly on the bytes read once per process (no
//! per-entry decoding or allocation; the crate denies unsafe code, so the
//! file is read, not mapped). A path hash collision is harmless: the full
//! stat key is always compared, so a colliding record can only miss.
//!
//! The executor's own files — store blobs and published outputs, the
//! [`Area::Owned`] paths — leave the change time out of their key, exactly
//! like the executor's per-process memo, because every link() to a blob
//! moves it; their slot holds the birth time instead (statx btime: set when
//! the inode is created and never changed), so an inode number reused for a
//! new file with an identical size and modification time — a tool that
//! fixes mtimes — can never match an old record. Where the filesystem
//! reports no birth time such files are not persisted at all. Every other
//! file, including those of the repository tree stores ([`Area::Tree`]), is
//! keyed by its change time: nothing re-verifies a tree identity that a
//! command no longer materializes (an old identity that a cached symlink
//! output still points into), so only the change time catches an in-place
//! edit there that kept the size and restored the modification time.
//!
//! A record is added only for a file that was settled when it was hashed:
//! its mtime and ctime (for the executor's own files, as above, its mtime
//! and birth time) at least [`RACY_MARGIN`] older than the start of the
//! read ("racy git": a later in-place change, and a new file that reuses the
//! inode number, then necessarily carry a different timestamp), or
//! ([`TRUSTED`]) for what the executor established of that very path: a
//! blob it validated or created, an output it published linked to such a
//! blob, another path of a settled state it read — never a racy state on
//! the strength of its stat key alone, and never a repository tree file.
//! Only paths that recur across processes are
//! recorded ([`Area`]): files outside the cache, and below it published
//! action outputs (`actions/...`), store blobs (`cas/<blob>`) and the files
//! of the repository tree stores
//! (`repositories/{trees,direct/trees,git/trees,builtins/trees}/<id>/...`);
//! never execution roots (`work/`), the working areas of repository rules
//! and extensions (`modextwd/`, `direct/stages/`), stages or temporary names.
//!
//! The table is saved only when this process hashed files whose digests it
//! did not hold yet: under the fs2 lock `v2.lock` the current file is read
//! again, merged (per path and kind the newer file state wins: the larger
//! change slot, then mtime, then this save), trimmed to the oldest epochs
//! above [`RECORD_LIMIT`] records, and replaced through a temporary file and
//! a rename. Records the process derived from what it already knew — an
//! output it published (TRUSTED), another path of a settled state it read —
//! ride along with such a save but never cause one: the volatile workspace status
//! is published anew by every build, and a no-op build must write nothing.
//! Readers never lock. A
//! missing, truncated, corrupt or foreign-version file reads as empty: it
//! costs re-hashing, never a wrong digest and never a failure. The header
//! carries no bazel-rs binary identity on purpose: the payload (SHA-256 of
//! file bytes by stat key) does not depend on the binary, and an identity
//! would force a full re-hash after every upgrade.
//!
//! `BAZEL_RS_DIGEST_MEMO=off` disables the persistent table (the per-process
//! memos stay) while it is set; it repairs nothing: the records stay and
//! count again once it is unset. Deleting `<cache>/digests/` drops them all.
//! `--verify_repo_tampering` / `BAZEL_RS_VERIFY_REPOS=full` make
//! repository verification re-hash every file without consulting it
//! (crate::repository_store::verify_mode).

use fs2::FileExt;
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::MetadataExt;
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const MAGIC: [u8; 8] = *b"BZRSDMEM";
/// The format version, also in the file names (`v<VERSION>.dmemo`). Any
/// narrowing of the trusted areas or of what a record may vouch for (a
/// stronger key, a stricter rule for what is recorded or trusted) must bump
/// it: a record does not say which rule admitted it, so the records an
/// older binary wrote under the broader rule would otherwise stay trusted.
/// [`Table::lookup`] also re-checks the one rule a path shows — birth-time
/// (OWNED) keys count only for [`Area::Owned`] paths — which voids the
/// records written before L2-W1 (995d70b) narrowed the recorded areas from
/// any `repositories/<kind>/trees/<id>/` to the four tree stores, and before
/// the tree stores' files were keyed by change time (both while the format
/// was unreleased).
///
/// Version 2 (fix-digest-memo): a version-1 table can hold wrong digests.
/// The executor's per-process memo answered by stat key alone, also for a
/// new file that reused a deleted file's inode number inside the same
/// timestamp tick, and recorded such an answer as [`TRUSTED`] (for a file
/// outside the cache, for instance a source saved during a build); its
/// owned files counted as settled by their modification time alone. Since
/// then a trusted record is derived only from a settled state or from
/// content this process established on that very path (crate::action,
/// `DigestMemo`), and an owned file is settled by its birth time as well.
const VERSION: u32 = 2;
const HEADER_BYTES: usize = 32;
const RECORD_BYTES: usize = 88;
/// Records kept per cache (~88 MiB). A save above it drops the records of
/// the oldest epochs, down to 7/8 of it.
pub const RECORD_LIMIT: usize = 1 << 20;
/// "Racy git": a digest is recorded only for a file whose timestamps are at
/// least this much older than the start of its read (the same margin as
/// the executor's per-process digest memo, crate::action, and
/// repository_store's fingerprints).
pub const RACY_MARGIN: Duration = Duration::from_secs(2);
/// The table's directory below the cache root.
pub const DIRECTORY: &str = "digests";
const TABLE_NAME: &str = "v2.dmemo";
const LOCK_NAME: &str = "v2.lock";
const TEMPORARY_PREFIX: &str = ".v2.dmemo.";
/// Temporary files of interrupted saves older than this are removed.
const STALE_TEMPORARY: Duration = Duration::from_secs(3600);
const READ_BUFFER_BYTES: usize = 1 << 20;

/// Record flag: one of the executor's own files ([`Area::Owned`]); the
/// change slot holds its birth time.
pub const OWNED: u32 = 1;
/// Record flag: derived from what the process already knew rather than from
/// hashing a settled file at that path: the content the executor
/// established on that very path (a blob it validated or created, an output
/// it published linked to such a blob), or another path of a settled state
/// it read (a published output and its blob). Never a racy state on the
/// strength of its stat key alone (version 2, see [`VERSION`]). Such records
/// never cause a save by themselves.
pub const TRUSTED: u32 = 2;

const PATH_AT: usize = 0;
const DEVICE_AT: usize = 8;
const INODE_AT: usize = 16;
const SIZE_AT: usize = 24;
const MTIME_AT: usize = 32;
const CHANGE_AT: usize = 40;
const SHA_AT: usize = 48;
const EPOCH_AT: usize = 80;
const FLAGS_AT: usize = 84;

/// The stat identity of one file state.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Key {
    pub device: u64,
    pub inode: u64,
    pub size: u64,
    pub mtime_ns: i64,
    /// The change time, or the birth time of one of the executor's own
    /// files (`owned`).
    pub change_ns: i64,
    pub owned: bool,
}

/// Where a path lies, for the table: whether and how it may be recorded.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Area {
    /// Outside the cache root: recorded by its change-time key.
    Outside,
    /// A published action output (`actions/<key>/...`) or a store blob
    /// (`cas/<blob>`): read-only files the executor wrote and only ever
    /// replaces by republication, recorded by their birth-time key
    /// ([`OWNED`]: every link() moves their change time), also as
    /// [`TRUSTED`].
    Owned,
    /// A file of a repository tree store
    /// (`repositories/{trees,direct/trees,git/trees,builtins/trees}/<id>/...`,
    /// crate::action::repository_tree_rest): recorded only by its
    /// change-time key and only as read in a settled state, never
    /// [`TRUSTED`]. A command verifies the trees it materializes, but not a
    /// tree identity it no longer materializes that a cached symlink output
    /// still points into; a birth-time key would keep trusting an in-place
    /// edit there that kept the size and restored the modification time.
    Tree,
    /// Anything else below the cache root (execution roots, the working
    /// areas of repository rules and extensions, stages, temporary names,
    /// the table itself), a relative path or one with a `..` component:
    /// never recorded.
    Transient,
}

/// [`Area`] of the `/`-separated names of a path below the cache root.
fn area_below(rest: &[u8]) -> Area {
    let published = |name: &[u8]| !name.is_empty() && !name.starts_with(b".");
    let mut names = rest.split(|byte| *byte == b'/');
    match (names.next(), names.next(), names.next()) {
        (Some(b"actions"), Some(key), Some(_)) if published(key) => Area::Owned,
        (Some(b"cas"), Some(blob), None) if published(blob) => Area::Owned,
        // Files of the repository tree stores only, never the working areas
        // of repository rules and extensions next to them.
        _ if crate::action::repository_tree_rest(rest).is_some_and(|below| !below.is_empty()) => {
            Area::Tree
        }
        _ => Area::Transient,
    }
}

fn nanos(seconds: i64, nanoseconds: i64) -> Option<i64> {
    seconds.checked_mul(1_000_000_000)?.checked_add(nanoseconds)
}

fn system_nanos(time: SystemTime) -> Option<i64> {
    i64::try_from(time.duration_since(UNIX_EPOCH).ok()?.as_nanos()).ok()
}

impl Key {
    /// The key of `metadata` (a stat of the file), by its birth time when
    /// `owned` (only for [`Area::Owned`] paths), else by its change time;
    /// `None` when it cannot be represented: a timestamp outside the i64
    /// nanosecond range, or an owned file on a filesystem that reports no
    /// birth time.
    pub fn of(metadata: &fs::Metadata, owned: bool) -> Option<Self> {
        let mtime_ns = nanos(metadata.mtime(), metadata.mtime_nsec())?;
        let change_ns = if owned {
            system_nanos(metadata.created().ok()?)?
        } else {
            nanos(metadata.ctime(), metadata.ctime_nsec())?
        };
        Some(Self {
            device: metadata.dev(),
            inode: metadata.ino(),
            size: metadata.size(),
            mtime_ns,
            change_ns,
            owned,
        })
    }
}

/// Whether a file with `metadata` was settled at `start`: its mtime and
/// ctime at least [`RACY_MARGIN`] older — for one of the executor's own
/// files (`owned`), whose ctime every link() moves, its mtime and birth time
/// (its ctime where the filesystem reports no birth time). A file changed or
/// created after `start` then necessarily carries a later timestamp, so no
/// other content can ever show a settled state, not even after its inode
/// number is reused; a modification time alone settles nothing (`touch -d`,
/// `cp -p` and archive extraction give new files old ones). A future
/// timestamp is never settled.
pub fn settled(metadata: &fs::Metadata, start: SystemTime, owned: bool) -> bool {
    let birth = owned
        .then(|| metadata.created().ok().and_then(system_nanos))
        .flatten();
    let change = match birth {
        Some(birth) => (
            birth.div_euclid(1_000_000_000),
            birth.rem_euclid(1_000_000_000),
        ),
        None => (metadata.ctime(), metadata.ctime_nsec()),
    };
    [(metadata.mtime(), metadata.mtime_nsec()), change]
        .into_iter()
        .all(|(seconds, nanoseconds)| {
            u64::try_from(seconds)
                .ok()
                .and_then(|seconds| {
                    UNIX_EPOCH.checked_add(Duration::new(
                        seconds,
                        nanoseconds.clamp(0, 999_999_999) as u32,
                    ))
                })
                .and_then(|time| time.checked_add(RACY_MARGIN))
                .is_some_and(|time| time <= start)
        })
}

/// The first 8 bytes (little-endian) of the blake3 hash of the path's bytes.
pub fn path_hash(path: &Path) -> u64 {
    let hash = blake3::hash(path.as_os_str().as_bytes());
    u64::from_le_bytes(hash.as_bytes()[..8].try_into().expect("8 bytes"))
}

fn u64_at(record: &[u8], at: usize) -> u64 {
    u64::from_le_bytes(record[at..at + 8].try_into().expect("8 bytes"))
}

fn i64_at(record: &[u8], at: usize) -> i64 {
    i64::from_le_bytes(record[at..at + 8].try_into().expect("8 bytes"))
}

fn u32_at(record: &[u8], at: usize) -> u32 {
    u32::from_le_bytes(record[at..at + 4].try_into().expect("4 bytes"))
}

fn sha_of(record: &[u8]) -> [u8; 32] {
    record[SHA_AT..SHA_AT + 32].try_into().expect("32 bytes")
}

/// One record's bytes.
pub fn encode(
    hash: u64,
    key: &Key,
    sha256: &[u8; 32],
    epoch: u32,
    flags: u32,
) -> [u8; RECORD_BYTES] {
    let mut record = [0; RECORD_BYTES];
    record[PATH_AT..PATH_AT + 8].copy_from_slice(&hash.to_le_bytes());
    record[DEVICE_AT..DEVICE_AT + 8].copy_from_slice(&key.device.to_le_bytes());
    record[INODE_AT..INODE_AT + 8].copy_from_slice(&key.inode.to_le_bytes());
    record[SIZE_AT..SIZE_AT + 8].copy_from_slice(&key.size.to_le_bytes());
    record[MTIME_AT..MTIME_AT + 8].copy_from_slice(&key.mtime_ns.to_le_bytes());
    record[CHANGE_AT..CHANGE_AT + 8].copy_from_slice(&key.change_ns.to_le_bytes());
    record[SHA_AT..SHA_AT + 32].copy_from_slice(sha256);
    record[EPOCH_AT..EPOCH_AT + 4].copy_from_slice(&epoch.to_le_bytes());
    let flags = (flags & !OWNED) | if key.owned { OWNED } else { 0 };
    record[FLAGS_AT..FLAGS_AT + 4].copy_from_slice(&flags.to_le_bytes());
    record
}

/// The fields of one record.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Record {
    pub hash: u64,
    pub key: Key,
    pub sha256: [u8; 32],
    pub epoch: u32,
    pub flags: u32,
}

/// One record's fields.
pub fn decode(record: &[u8]) -> Record {
    let flags = u32_at(record, FLAGS_AT);
    Record {
        hash: u64_at(record, PATH_AT),
        key: Key {
            device: u64_at(record, DEVICE_AT),
            inode: u64_at(record, INODE_AT),
            size: u64_at(record, SIZE_AT),
            mtime_ns: i64_at(record, MTIME_AT),
            change_ns: i64_at(record, CHANGE_AT),
            owned: flags & OWNED != 0,
        },
        sha256: sha_of(record),
        epoch: u32_at(record, EPOCH_AT),
        flags,
    }
}

/// A record's place: (path hash, OWNED), the sort and merge order. The
/// table holds at most one record per slot.
type Slot = (u64, bool);

fn slot_of(record: &[u8]) -> Slot {
    (
        u64_at(record, PATH_AT),
        u32_at(record, FLAGS_AT) & OWNED != 0,
    )
}

fn key_matches(record: &[u8], key: &Key) -> bool {
    u64_at(record, DEVICE_AT) == key.device
        && u64_at(record, INODE_AT) == key.inode
        && u64_at(record, SIZE_AT) == key.size
        && i64_at(record, MTIME_AT) == key.mtime_ns
        && i64_at(record, CHANGE_AT) == key.change_ns
        && (u32_at(record, FLAGS_AT) & OWNED != 0) == key.owned
}

fn checksum(body: &[u8]) -> u64 {
    u64::from_le_bytes(
        blake3::hash(body).as_bytes()[..8]
            .try_into()
            .expect("8 bytes"),
    )
}

/// The table file's bytes for `body` (sorted records).
pub fn encode_table(body: &[u8]) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(HEADER_BYTES + body.len());
    bytes.extend_from_slice(&MAGIC);
    bytes.extend_from_slice(&VERSION.to_le_bytes());
    bytes.extend_from_slice(&(RECORD_BYTES as u32).to_le_bytes());
    bytes.extend_from_slice(&((body.len() / RECORD_BYTES) as u64).to_le_bytes());
    bytes.extend_from_slice(&checksum(body).to_le_bytes());
    bytes.extend_from_slice(body);
    bytes
}

/// The record count of a well-formed table file: right magic, version and
/// record size, a body of exactly `count` records with the header's
/// checksum, sorted strictly by (path hash, OWNED). Anything else is `None`.
pub fn validate(bytes: &[u8]) -> Option<usize> {
    if bytes.len() < HEADER_BYTES
        || bytes[..8] != MAGIC
        || u32_at(bytes, 8) != VERSION
        || u32_at(bytes, 12) as usize != RECORD_BYTES
    {
        return None;
    }
    let count = usize::try_from(u64_at(bytes, 16)).ok()?;
    let body = &bytes[HEADER_BYTES..];
    if count.checked_mul(RECORD_BYTES)? != body.len() || u64_at(bytes, 24) != checksum(body) {
        return None;
    }
    let mut previous: Option<Slot> = None;
    for record in body.as_chunks::<RECORD_BYTES>().0 {
        let slot = slot_of(record);
        if previous.is_some_and(|previous| previous >= slot) {
            return None;
        }
        previous = Some(slot);
    }
    Some(count)
}

/// What loading found.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Loaded {
    Absent,
    Valid,
    /// Unreadable, truncated, corrupt or of another format: read as empty.
    Ignored,
}

struct Body {
    /// The whole file (header included) when it is valid, else empty.
    bytes: Vec<u8>,
    count: usize,
    outcome: Loaded,
    elapsed: Duration,
}

impl Body {
    fn read(path: &Path) -> Self {
        let start = Instant::now();
        // snapshot-unrecorded: the table's own file (<cache>/digests), no
        // input of planning (a record only spares reading a file whose
        // digest the caller records).
        let (bytes, count, outcome) = match fs::read(path) {
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                (Vec::new(), 0, Loaded::Absent)
            }
            Err(_) => (Vec::new(), 0, Loaded::Ignored),
            Ok(bytes) => match validate(&bytes) {
                Some(count) => (bytes, count, Loaded::Valid),
                None => (Vec::new(), 0, Loaded::Ignored),
            },
        };
        Self {
            bytes,
            count,
            outcome,
            elapsed: start.elapsed(),
        }
    }

    fn record(&self, index: usize) -> &[u8] {
        let at = HEADER_BYTES + index * RECORD_BYTES;
        &self.bytes[at..at + RECORD_BYTES]
    }

    fn records(&self) -> impl Iterator<Item = &[u8]> {
        self.bytes
            .get(HEADER_BYTES..)
            .unwrap_or_default()
            .as_chunks::<RECORD_BYTES>()
            .0
            .iter()
            .map(|record| record.as_slice())
    }

    /// The SHA-256 recorded for `hash` and exactly `key`.
    fn find(&self, hash: u64, key: &Key) -> Option<[u8; 32]> {
        // Lower bound of `hash` by binary search on the record bytes.
        let (mut low, mut high) = (0, self.count);
        while low < high {
            let middle = low + (high - low) / 2;
            if u64_at(self.record(middle), PATH_AT) < hash {
                low = middle + 1;
            } else {
                high = middle;
            }
        }
        (low..self.count)
            .map(|index| self.record(index))
            .take_while(|record| u64_at(record, PATH_AT) == hash)
            .find(|record| key_matches(record, key))
            .map(sha_of)
    }
}

/// A record found by this process, not yet in the loaded file.
#[derive(Clone, Copy)]
struct Fresh {
    key: Key,
    sha256: [u8; 32],
    trusted: bool,
    saved: bool,
}

/// What [`Table::save`] did.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Saved {
    /// No new records: nothing was written.
    Nothing,
    /// The table was written with `records` records (`written` new).
    Written {
        records: usize,
        written: usize,
        bytes: usize,
    },
}

/// One cache's table: the file as this process read it plus the records it
/// found since.
pub struct Table {
    directory: PathBuf,
    /// The cache root (canonicalized on first use) whose recurring paths
    /// are recorded.
    cache: PathBuf,
    root: OnceLock<PathBuf>,
    limit: usize,
    body: OnceLock<Body>,
    fresh: Mutex<HashMap<Slot, Fresh>>,
}

impl Table {
    /// The table of the cache `cache` (`<cache>/digests/v2.dmemo`), read on
    /// first use.
    pub fn new(cache: &Path) -> Self {
        Self::with_limit(cache, RECORD_LIMIT)
    }

    /// [`Table::new`] keeping at most `limit` records (tests).
    pub fn with_limit(cache: &Path, limit: usize) -> Self {
        Self {
            directory: cache.join(DIRECTORY),
            cache: cache.to_path_buf(),
            root: OnceLock::new(),
            limit: limit.max(1),
            body: OnceLock::new(),
            fresh: Mutex::new(HashMap::new()),
        }
    }

    fn body(&self) -> &Body {
        self.body
            .get_or_init(|| Body::read(&self.directory.join(TABLE_NAME)))
    }

    /// What loading the file found, its record count and bytes.
    pub fn loaded(&self) -> (Loaded, usize, usize) {
        let body = self.body();
        (body.outcome, body.count, body.bytes.len())
    }

    fn fresh(&self) -> std::sync::MutexGuard<'_, HashMap<Slot, Fresh>> {
        self.fresh.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// The SHA-256 recorded for `path` in exactly the state `key`. A
    /// birth-time key (`owned`, no change time) is answered only for an
    /// [`Area::Owned`] path: records of other paths by such keys — written
    /// under earlier, broader rules (any `repositories/<kind>/trees/<id>/`
    /// before L2-W1; repository tree files before they were keyed by change
    /// time) — must not stay trusted (see [`VERSION`]).
    pub fn lookup(&self, path: &Path, key: &Key) -> Option<[u8; 32]> {
        if key.owned && self.area(path) != Area::Owned {
            STATS.misses.fetch_add(1, Ordering::Relaxed);
            return None;
        }
        let hash = path_hash(path);
        if let Some(sha256) = self.body().find(hash, key) {
            STATS.hits.fetch_add(1, Ordering::Relaxed);
            return Some(sha256);
        }
        let found = self
            .fresh()
            .get(&(hash, key.owned))
            .filter(|fresh| fresh.key == *key)
            .map(|fresh| fresh.sha256);
        if found.is_some() {
            STATS.fresh_hits.fetch_add(1, Ordering::Relaxed);
        } else {
            STATS.misses.fetch_add(1, Ordering::Relaxed);
        }
        found
    }

    /// Where `path` lies ([`Area`]) relative to the canonical cache root:
    /// below it only when spelled as normal components after the root.
    pub fn area(&self, path: &Path) -> Area {
        let root = match self.root.get() {
            Some(root) => root,
            // snapshot-unrecorded: the table's own cache root (it classifies
            // the paths of the table's records), no input of planning.
            None => match fs::canonicalize(&self.cache) {
                // Until the cache exists nothing below it is known.
                Err(_) => return Area::Transient,
                Ok(root) => self.root.get_or_init(|| root),
            },
        };
        let (root_bytes, bytes) = (root.as_os_str().as_bytes(), path.as_os_str().as_bytes());
        if crate::action::plain(root_bytes) && crate::action::plain(bytes) {
            // Plain spellings (the executor's paths): the components follow
            // from the bytes, without parsing or allocating.
            if crate::action::has_parent_name(bytes) {
                return Area::Transient;
            }
            return match crate::action::plain_rest(root_bytes, bytes) {
                Some(rest) => area_below(rest),
                None if bytes == root_bytes => Area::Transient,
                None => Area::Outside,
            };
        }
        if !path.is_absolute()
            || path
                .components()
                .any(|component| matches!(component, Component::CurDir | Component::ParentDir))
        {
            return Area::Transient;
        }
        let Ok(relative) = path.strip_prefix(root) else {
            return Area::Outside;
        };
        let names: Vec<&[u8]> = relative
            .components()
            .map(|component| component.as_os_str().as_bytes())
            .collect();
        area_below(&names.join(&b'/'))
    }

    /// Record that `path` in the state `key` has `sha256`. The caller
    /// vouches that the file was settled when it was hashed, or (`trusted`)
    /// that it established this content on this very path (a blob it
    /// validated or created, an output it published linked to such a blob)
    /// or that the state is another path of a settled one it read — never a
    /// racy state by its stat key alone ([`TRUSTED`]); trusted records never
    /// cause a save by themselves. Paths that do not recur ([`Area::Transient`]), a
    /// birth-time key of any path but an [`Area::Owned`] one, a trusted
    /// repository tree file ([`Area::Tree`]), records the loaded file
    /// already holds and anything beyond the record limit are ignored.
    pub fn remember(&self, path: &Path, key: Key, sha256: [u8; 32], trusted: bool) {
        let area = self.area(path);
        if area == Area::Transient
            || (key.owned && area != Area::Owned)
            || (trusted && area == Area::Tree)
        {
            return;
        }
        let hash = path_hash(path);
        if self.body().find(hash, &key) == Some(sha256) {
            return;
        }
        let mut fresh = self.fresh();
        let slot = (hash, key.owned);
        let full = fresh.len() >= self.limit;
        match fresh.get_mut(&slot) {
            Some(existing) if existing.key == key && existing.sha256 == sha256 => {
                // A hashed (untrusted) observation of a trusted record makes
                // it worth a save.
                existing.trusted &= trusted;
                return;
            }
            None if full => return,
            _ => {}
        }
        fresh.insert(
            slot,
            Fresh {
                key,
                sha256,
                trusted,
                saved: false,
            },
        );
        STATS.remembered.fetch_add(1, Ordering::Relaxed);
    }

    /// Drop this process's not yet saved records of `path`: the caller is
    /// about to remove or replace the file there (an invalid store blob, an
    /// action entry published again), so they may describe a state that a
    /// new file created in the same timestamp tick could show again.
    pub fn forget(&self, path: &Path) {
        let hash = path_hash(path);
        let mut fresh = self.fresh();
        for owned in [false, true] {
            if fresh
                .get(&(hash, owned))
                .is_some_and(|record| !record.saved)
            {
                fresh.remove(&(hash, owned));
            }
        }
    }

    /// Merge this process's new records into the file (see the module
    /// documentation). Nothing is written when there are none, or only
    /// trusted ones.
    pub fn save(&self) -> Result<Saved, String> {
        let pending: Vec<(Slot, Fresh)> = self
            .fresh()
            .iter()
            .filter(|(_, fresh)| !fresh.saved)
            .map(|(slot, fresh)| (*slot, *fresh))
            .collect();
        if pending.iter().all(|(_, fresh)| fresh.trusted) {
            return Ok(Saved::Nothing);
        }
        let error =
            |what: &str, error: io::Error| format!("{what} {}: {error}", self.directory.display());
        fs::create_dir_all(&self.directory).map_err(|e| error("cannot create", e))?;
        // snapshot-unrecorded: the table's own lock file, written when the
        // command ends.
        let lock = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(self.directory.join(LOCK_NAME))
            .map_err(|e| error("cannot open the lock in", e))?;
        FileExt::lock_exclusive(&lock).map_err(|e| error("cannot lock", e))?;
        // Merge with what other processes saved since this one read the file.
        let current = Body::read(&self.directory.join(TABLE_NAME));
        let epoch = current
            .records()
            .map(|record| u32_at(record, EPOCH_AT))
            .max()
            .unwrap_or(0)
            .saturating_add(1);
        let mut new: Vec<[u8; RECORD_BYTES]> = pending
            .iter()
            .map(|((hash, _), fresh)| {
                encode(
                    *hash,
                    &fresh.key,
                    &fresh.sha256,
                    epoch,
                    if fresh.trusted { TRUSTED } else { 0 },
                )
            })
            .collect();
        new.sort_unstable_by_key(|record| slot_of(record));
        let merged = merge(
            current.records(),
            new.iter().map(|record| record.as_slice()),
        );
        let body = evict(merged, self.limit);
        let bytes = encode_table(&body);
        let mut temporary = tempfile::Builder::new()
            .prefix(TEMPORARY_PREFIX)
            .tempfile_in(&self.directory)
            .map_err(|e| error("cannot stage the table in", e))?;
        temporary
            .write_all(&bytes)
            .map_err(|e| error("cannot write the table in", e))?;
        temporary
            .persist(self.directory.join(TABLE_NAME))
            .map_err(|e| error("cannot publish the table in", e.error))?;
        remove_stale_temporaries(&self.directory);
        drop(lock);
        let mut fresh = self.fresh();
        for (slot, saved) in &pending {
            if let Some(entry) = fresh.get_mut(slot)
                && entry.key == saved.key
                && entry.sha256 == saved.sha256
            {
                entry.saved = true;
            }
        }
        Ok(Saved::Written {
            records: body.len() / RECORD_BYTES,
            written: pending.len(),
            bytes: bytes.len(),
        })
    }
}

/// Merge two record sequences sorted by (path hash, OWNED): per slot the
/// newer file state wins (the larger change slot, then mtime); on a tie the
/// `new` record (this process's observation).
fn merge<'a>(old: impl Iterator<Item = &'a [u8]>, new: impl Iterator<Item = &'a [u8]>) -> Vec<u8> {
    let newer = |a: &[u8], b: &[u8]| {
        (i64_at(a, CHANGE_AT), i64_at(a, MTIME_AT)) > (i64_at(b, CHANGE_AT), i64_at(b, MTIME_AT))
    };
    let mut body = Vec::new();
    let mut old = old.peekable();
    let mut new = new.peekable();
    loop {
        let record = match (old.peek(), new.peek()) {
            (None, None) => break,
            (Some(_), None) => old.next(),
            (None, Some(_)) => new.next(),
            (Some(a), Some(b)) => match slot_of(a).cmp(&slot_of(b)) {
                std::cmp::Ordering::Less => old.next(),
                std::cmp::Ordering::Greater => new.next(),
                std::cmp::Ordering::Equal => {
                    let a = old.next().expect("peeked");
                    let b = new.next().expect("peeked");
                    Some(if newer(a, b) { a } else { b })
                }
            },
        };
        body.extend_from_slice(record.expect("a record"));
    }
    body
}

/// `body` (sorted records) with at most `limit` records: above it, the
/// records of the oldest epochs are dropped down to 7/8 of the limit (ties
/// within the cut epoch in record order). The order is kept.
fn evict(body: Vec<u8>, limit: usize) -> Vec<u8> {
    let count = body.len() / RECORD_BYTES;
    if count <= limit {
        return body;
    }
    let keep = (limit - limit / 8).max(1);
    let records = body.as_chunks::<RECORD_BYTES>().0;
    let mut epochs: Vec<u32> = records
        .iter()
        .map(|record| u32_at(record, EPOCH_AT))
        .collect();
    epochs.sort_unstable_by(|a, b| b.cmp(a));
    let cut = epochs[keep - 1];
    let mut at_cut = keep - epochs.iter().filter(|epoch| **epoch > cut).count();
    let mut kept = Vec::with_capacity(keep * RECORD_BYTES);
    for record in records {
        let epoch = u32_at(record, EPOCH_AT);
        if epoch > cut {
            kept.extend_from_slice(record);
        } else if epoch == cut && at_cut > 0 {
            at_cut -= 1;
            kept.extend_from_slice(record);
        }
    }
    kept
}

fn remove_stale_temporaries(directory: &Path) {
    // snapshot-unrecorded: the table's own directory (<cache>/digests),
    // cleaned when the command saves.
    let Ok(entries) = fs::read_dir(directory) else {
        return;
    };
    for entry in entries.flatten() {
        let stale = entry
            .file_name()
            .as_bytes()
            .starts_with(TEMPORARY_PREFIX.as_bytes())
            && entry
                // snapshot-unrecorded: a temporary of the table's directory.
                .metadata()
                .and_then(|metadata| metadata.modified())
                .ok()
                .and_then(|modified| modified.elapsed().ok())
                .is_some_and(|age| age > STALE_TEMPORARY);
        if stale {
            let _ = fs::remove_file(entry.path());
        }
    }
}

/// Who asked for a digest (telemetry only).
#[derive(Clone, Copy, Debug)]
pub enum Use {
    /// Action inputs and outputs (crate::action).
    Action = 0,
    /// Registry module trees (crate::materializer).
    Registry = 1,
    /// The embedded tools tree (crate::builtin_tools).
    Builtin = 2,
    /// Recorded repository-rule inputs (crate::repository_ctx).
    Recorded = 3,
}

const USES: [&str; 4] = ["action", "registry", "builtin", "recorded"];

struct Counter {
    files: AtomicU64,
    bytes: AtomicU64,
}

impl Counter {
    const fn new() -> Self {
        Self {
            files: AtomicU64::new(0),
            bytes: AtomicU64::new(0),
        }
    }
}

struct Timer {
    calls: AtomicU64,
    nanos: AtomicU64,
}

impl Timer {
    const fn new() -> Self {
        Self {
            calls: AtomicU64::new(0),
            nanos: AtomicU64::new(0),
        }
    }
}

struct Stats {
    hits: AtomicU64,
    fresh_hits: AtomicU64,
    misses: AtomicU64,
    remembered: AtomicU64,
    hashed: [Counter; 4],
    verified: [Timer; 4],
}

static STATS: Stats = Stats {
    hits: AtomicU64::new(0),
    fresh_hits: AtomicU64::new(0),
    misses: AtomicU64::new(0),
    remembered: AtomicU64::new(0),
    hashed: [
        Counter::new(),
        Counter::new(),
        Counter::new(),
        Counter::new(),
    ],
    verified: [Timer::new(), Timer::new(), Timer::new(), Timer::new()],
};

/// Count a file that `user` actually read and hashed (telemetry).
pub fn count_hashed(user: Use, bytes: u64) {
    let counter = &STATS.hashed[user as usize];
    counter.files.fetch_add(1, Ordering::Relaxed);
    counter.bytes.fetch_add(bytes, Ordering::Relaxed);
}

/// Account one verification of `user` (a tree) that took `elapsed`.
pub fn count_verified(user: Use, elapsed: Duration) {
    let timer = &STATS.verified[user as usize];
    timer.calls.fetch_add(1, Ordering::Relaxed);
    timer
        .nanos
        .fetch_add(elapsed.as_nanos() as u64, Ordering::Relaxed);
}

static INSTALLED: OnceLock<Option<Table>> = OnceLock::new();

/// Use the table of the cache `cache` (an absolute path) in this process.
/// The first call decides: a process works on one cache. Without a call
/// (library use, unit tests) no persistent table is used.
/// `BAZEL_RS_DIGEST_MEMO=off` disables it.
pub fn install(cache: &Path) {
    INSTALLED.get_or_init(|| {
        if std::env::var_os("BAZEL_RS_DIGEST_MEMO").is_some_and(|value| value == "off") {
            return None;
        }
        Some(Table::new(cache))
    });
}

/// The process's table, if [`install`]ed.
pub fn installed() -> Option<&'static Table> {
    INSTALLED.get().and_then(Option::as_ref)
}

/// Whether repository verification may take digests from the table: not
/// under `--verify_repo_tampering` / `BAZEL_RS_VERIFY_REPOS=full`.
pub fn repository_table() -> Option<&'static Table> {
    installed().filter(|_| {
        crate::repository_store::verify_mode() == crate::repository_store::VerifyMode::Stat
    })
}

/// SHA-256 of an open regular file, read to its end.
pub fn sha256_reader(file: &mut impl Read, size_hint: u64) -> io::Result<([u8; 32], u64)> {
    let mut hash = Sha256::new();
    let capacity = usize::try_from(size_hint).map_or(READ_BUFFER_BYTES, |size| {
        size.saturating_add(1).min(READ_BUFFER_BYTES)
    });
    let mut buffer = vec![0; capacity.max(1)];
    let mut total = 0u64;
    loop {
        let count = match file.read(&mut buffer) {
            Ok(0) => break,
            Ok(count) => count,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(error),
        };
        hash.update(&buffer[..count]);
        total += count as u64;
    }
    Ok((hash.finalize().into(), total))
}

/// The SHA-256 of the regular file `path`, whose stat — taken just before
/// by the caller, following links exactly as the open below does — is
/// `metadata`, for repository verification (`user`): the table's when it
/// records this path in exactly this state ([`repository_table`]);
/// otherwise `hash` reads the opened file, and the digest is recorded when
/// the file kept its key while it was read and was settled before. The
/// result is always the digest of the bytes `hash` read, as without the
/// table.
pub fn file_sha256(
    path: &Path,
    metadata: &fs::Metadata,
    user: Use,
    hash: impl FnOnce(&mut File) -> io::Result<[u8; 32]>,
) -> io::Result<[u8; 32]> {
    file_sha256_in(
        repository_table(),
        path,
        metadata,
        user,
        hash,
        SystemTime::now,
    )
}

/// [`file_sha256`] with an explicit table and clock (tests).
fn file_sha256_in(
    table: Option<&Table>,
    path: &Path,
    metadata: &fs::Metadata,
    user: Use,
    hash: impl FnOnce(&mut File) -> io::Result<[u8; 32]>,
    now: impl Fn() -> SystemTime,
) -> io::Result<[u8; 32]> {
    let key = table.and_then(|_| Key::of(metadata, false));
    if let (Some(table), Some(key)) = (table, &key)
        && let Some(sha256) = table.lookup(path, key)
    {
        return Ok(sha256);
    }
    let start = now();
    // snapshot-unrecorded: a file of a repository tree the caller verifies
    // and records whole (materializer::tree_matches,
    // builtin_tools::verified_warm: REPO + INTEGRITY), as for a table hit.
    let mut file = File::open(path)?;
    let sha256 = hash(&mut file)?;
    count_hashed(user, metadata.size());
    if let (Some(table), Some(key)) = (table, key)
        // snapshot-unrecorded: the fstat of the file opened above.
        && let Ok(after) = file.metadata()
        && Key::of(&after, false) == Some(key)
        && settled(&after, start, false)
    {
        table.remember(path, key, sha256, false);
    }
    Ok(sha256)
}

/// The SHA-256 of `file`, a regular file the caller opened at `path`
/// (following links) and has not read from, whose own fstat
/// (`file.metadata()`) is `metadata`: the table's digest when it records
/// `path` in exactly that state ([`repository_table`]); otherwise the
/// digest of the bytes read from `file` itself — never from `path` again —
/// recorded when that state was settled before the read and is still the
/// file's after it. Key and bytes therefore always belong to one inode: a
/// stat of `path` followed by a read of `path` could pair one file's key
/// with another file's bytes when `path` (or a symlink on it) is swapped
/// between them. A file that is not regular is hashed, never looked up or
/// recorded. Read errors pass through.
///
/// This is the entry point for every in-process memo of file digests that
/// sits in front of the table, so that the file states it observes keep
/// hitting (and filling) the table: `repository_ctx::FileDigestMemo`
/// (recorded repository inputs, i20-hardening) opens and fstats the file
/// itself and hashes it through this function (`Use::Recorded`; its
/// [`opened_sha256_in`] form, which also tells a table answer from a read)
/// instead of reading the file directly; [`path_sha256`] is the same for
/// callers that start from a path.
pub fn opened_sha256(
    file: &File,
    metadata: &fs::Metadata,
    path: &Path,
    user: Use,
) -> io::Result<[u8; 32]> {
    opened_sha256_in(
        repository_table(),
        file,
        metadata,
        path,
        user,
        SystemTime::now,
    )
    .map(|(sha256, _)| sha256)
}

/// How an [`opened_sha256`] digest was answered.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Answer {
    /// The table records the path in exactly the opened file's state:
    /// nothing was read (an in-process memo in front of the table counts it
    /// as a hit).
    Table,
    /// The opened file was read.
    Read,
}

/// [`opened_sha256`] with an explicit table (`None`: none installed, or
/// `--verify_repo_tampering`) and clock, and how it was answered
/// (`repository_ctx::FileDigestMemo`, whose clock is the observation's
/// start; tests).
pub(crate) fn opened_sha256_in(
    table: Option<&Table>,
    file: &File,
    metadata: &fs::Metadata,
    path: &Path,
    user: Use,
    now: impl Fn() -> SystemTime,
) -> io::Result<([u8; 32], Answer)> {
    let key = table
        .filter(|_| metadata.is_file())
        .and_then(|_| Key::of(metadata, false));
    if let (Some(table), Some(key)) = (table, &key)
        && let Some(sha256) = table.lookup(path, key)
    {
        return Ok((sha256, Answer::Table));
    }
    let start = now();
    let mut reader = file;
    let (sha256, _) = sha256_reader(&mut reader, metadata.size())?;
    count_hashed(user, metadata.size());
    if let (Some(table), Some(key)) = (table, key)
        && settled(metadata, start, false)
        && file
            // snapshot-unrecorded: the fstat of the caller's open file; the
            // caller records the answer (repository_ctx:
            // observed_fs::note_read_digest with `metadata`).
            .metadata()
            .is_ok_and(|after| Key::of(&after, false) == Some(key))
    {
        table.remember(path, key, sha256, false);
    }
    Ok((sha256, Answer::Read))
}

/// The SHA-256 of the file at `path` (following links) for a caller with
/// its own hasher `hash` (it opens and reads `path` and computes the
/// SHA-256 of its bytes; its errors pass through unchanged). With a table
/// ([`repository_table`]) `path` is opened here and hashed through
/// [`opened_sha256`], keyed by the opened file's own fstat; `hash` runs
/// only without a table, for a path that does not open as a regular file
/// (missing, a directory, a device: `hash` reports it as before), or when
/// reading the opened file fails. A digest of another length than
/// SHA-256's from `hash` passes through.
pub fn path_sha256<E>(
    path: &Path,
    user: Use,
    hash: impl FnOnce() -> Result<Vec<u8>, E>,
) -> Result<Vec<u8>, E> {
    path_sha256_in(repository_table(), path, user, hash, SystemTime::now)
}

/// [`path_sha256`] with an explicit table and clock (tests).
fn path_sha256_in<E>(
    table: Option<&Table>,
    path: &Path,
    user: Use,
    hash: impl FnOnce() -> Result<Vec<u8>, E>,
    now: impl Fn() -> SystemTime,
) -> Result<Vec<u8>, E> {
    // The files of a tree a repository rule watches while it executes
    // (repository_ctx::file_sha256 for watch_tree: a tainted or replayed
    // section of the snapshot window); the warm revalidation inside the
    // window opens them itself and records READs
    // (repository_ctx::observed_tree_digest).
    if let Some(table) = table
        // snapshot-unrecorded: a watched tree's file (above).
        && let Ok(file) = File::open(path)
        // snapshot-unrecorded: its fstat.
        && let Ok(metadata) = file.metadata()
        && metadata.is_file()
        && let Ok((sha256, _)) = opened_sha256_in(Some(table), &file, &metadata, path, user, now)
    {
        return Ok(sha256.to_vec());
    }
    // snapshot-unrecorded: a watched tree's file (above).
    let size = fs::metadata(path)
        .ok()
        .filter(fs::Metadata::is_file)
        .map_or(0, |metadata| metadata.size());
    let digest = hash()?;
    count_hashed(user, size);
    Ok(digest)
}

/// Parse a lowercase hex SHA-256.
pub fn parse_hex(text: &str) -> Option<[u8; 32]> {
    let bytes = text.as_bytes();
    if bytes.len() != 64 {
        return None;
    }
    let digit = |byte: u8| match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        _ => None,
    };
    let mut sha256 = [0; 32];
    for (index, pair) in bytes.as_chunks::<2>().0.iter().enumerate() {
        sha256[index] = (digit(pair[0])? << 4) | digit(pair[1])?;
    }
    Some(sha256)
}

/// Lowercase hex of a SHA-256.
pub fn hex(sha256: &[u8; 32]) -> String {
    const DIGITS: &[u8] = b"0123456789abcdef";
    let mut text = String::with_capacity(64);
    for &byte in sha256 {
        text.push(DIGITS[(byte >> 4) as usize] as char);
        text.push(DIGITS[(byte & 15) as usize] as char);
    }
    text
}

static FINISHED: std::sync::Once = std::sync::Once::new();

/// Save the installed table (only when this process found new records)
/// and, under `BAZEL_RS_TIMING`, print its `PERF-DMEMO` summary once. Call
/// at the end of a command, before exiting or exec'ing; later calls save
/// records found since. Failures cost re-hashing later and are never
/// reported except in the summary.
pub fn finish() {
    let start = Instant::now();
    let saved = installed().map(Table::save);
    let elapsed = start.elapsed();
    FINISHED.call_once(|| {
        if crate::repository_store::timing::enabled() {
            report(saved.as_ref(), elapsed);
        }
    });
}

fn report(saved: Option<&Result<Saved, String>>, elapsed: Duration) {
    match INSTALLED.get() {
        // Never installed: a command that touched no cache.
        None => return,
        Some(None) => eprintln!("PERF-DMEMO table=off"),
        // Installed but never consulted: the file was not even read.
        Some(Some(table)) if table.body.get().is_none() => {
            eprintln!("PERF-DMEMO load=unused");
        }
        Some(Some(table)) => {
            let body = table.body();
            let saved = match saved {
                None | Some(Ok(Saved::Nothing)) => "nothing".to_owned(),
                Some(Ok(Saved::Written {
                    records,
                    written,
                    bytes,
                })) => format!("records={records} new={written} bytes={bytes}"),
                Some(Err(error)) => format!("failed ({error})"),
            };
            eprintln!(
                "PERF-DMEMO load={:?} records={} bytes={} load_ms={:.3} lookups: hits={} fresh_hits={} misses={} remembered={} save: {saved} save_ms={:.3}",
                body.outcome,
                body.count,
                body.bytes.len(),
                body.elapsed.as_secs_f64() * 1e3,
                STATS.hits.load(Ordering::Relaxed),
                STATS.fresh_hits.load(Ordering::Relaxed),
                STATS.misses.load(Ordering::Relaxed),
                STATS.remembered.load(Ordering::Relaxed),
                elapsed.as_secs_f64() * 1e3,
            );
        }
    }
    let mut hashed = String::new();
    for (index, name) in USES.iter().enumerate() {
        let counter = &STATS.hashed[index];
        let timer = &STATS.verified[index];
        hashed.push_str(&format!(
            " {name}={}/{}B",
            counter.files.load(Ordering::Relaxed),
            counter.bytes.load(Ordering::Relaxed)
        ));
        let calls = timer.calls.load(Ordering::Relaxed);
        if calls > 0 {
            hashed.push_str(&format!(
                " ({:.3}s over {calls} trees)",
                timer.nanos.load(Ordering::Relaxed) as f64 / 1e9
            ));
        }
    }
    let phases = &crate::repository_store::timing::PHASES;
    eprintln!(
        "PERF-DMEMO hashed:{hashed} repository-store: recorded_check={:.3}s/{} snapshot_check={:.3}s/{} manifest_read={:.3}s/{} files_hashed_during_verify={}",
        phases.recorded_check.nanos.load(Ordering::Relaxed) as f64 / 1e9,
        phases.recorded_check.calls.load(Ordering::Relaxed),
        phases.snapshot_check.nanos.load(Ordering::Relaxed) as f64 / 1e9,
        phases.snapshot_check.calls.load(Ordering::Relaxed),
        phases.manifest_read.nanos.load(Ordering::Relaxed) as f64 / 1e9,
        phases.manifest_read.calls.load(Ordering::Relaxed),
        phases.files_hashed.load(Ordering::Relaxed),
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp() -> tempfile::TempDir {
        tempfile::Builder::new()
            .prefix("digest-memo-test-")
            .tempdir()
            .unwrap()
    }

    /// A synthetic key; odd seeds are owned (birth-time keys).
    fn key(seed: u64) -> Key {
        Key {
            device: 0x0102_0304_0506_0708 ^ seed,
            inode: 1000 + seed,
            size: 77 + seed,
            mtime_ns: 1_700_000_000_000_000_000 + seed as i64,
            change_ns: 1_700_000_000_500_000_000 + seed as i64,
            owned: seed % 2 == 1,
        }
    }

    fn sha(seed: u8) -> [u8; 32] {
        [seed; 32]
    }

    fn table_path(cache: &Path) -> PathBuf {
        cache.join(DIRECTORY).join(TABLE_NAME)
    }

    fn records(cache: &Path) -> Vec<Record> {
        fs::read(table_path(cache)).unwrap()[HEADER_BYTES..]
            .as_chunks::<RECORD_BYTES>()
            .0
            .iter()
            .map(|record| decode(record))
            .collect()
    }

    fn sha256_all(file: &mut File) -> io::Result<[u8; 32]> {
        sha256_reader(file, 0).map(|(sha256, _)| sha256)
    }

    /// Write a table of exactly these records, `(path, key, sha256, flags)`
    /// at epoch 1, as an older binary could have written them.
    fn write_records(cache: &Path, records: &[(&Path, Key, [u8; 32], u32)]) {
        let mut encoded: Vec<[u8; RECORD_BYTES]> = records
            .iter()
            .map(|(path, key, sha256, flags)| encode(path_hash(path), key, sha256, 1, *flags))
            .collect();
        encoded.sort_unstable_by_key(|record| slot_of(record));
        fs::create_dir_all(cache.join(DIRECTORY)).unwrap();
        fs::write(table_path(cache), encode_table(&encoded.concat())).unwrap();
    }

    #[test]
    fn record_layout_is_fixed_width_little_endian() {
        let key = Key {
            device: 0x1122_3344_5566_7788,
            inode: 0x0102_0304_0506_0708,
            size: 0x0a0b_0c0d,
            mtime_ns: -2,
            change_ns: 0x7fff_ffff_ffff_fff0,
            owned: true,
        };
        let mut sha256 = [0u8; 32];
        for (index, byte) in sha256.iter_mut().enumerate() {
            *byte = index as u8;
        }
        let record = encode(0xdead_beef_0bad_f00d, &key, &sha256, 0x0403_0201, TRUSTED);
        assert_eq!(record.len(), 88);
        assert_eq!(
            record[0..8],
            [0x0d, 0xf0, 0xad, 0x0b, 0xef, 0xbe, 0xad, 0xde]
        );
        assert_eq!(
            record[8..16],
            [0x88, 0x77, 0x66, 0x55, 0x44, 0x33, 0x22, 0x11]
        );
        assert_eq!(record[16..24], [8, 7, 6, 5, 4, 3, 2, 1]);
        assert_eq!(record[24..32], [0x0d, 0x0c, 0x0b, 0x0a, 0, 0, 0, 0]);
        assert_eq!(
            record[32..40],
            [0xfe, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff]
        );
        assert_eq!(
            record[40..48],
            [0xf0, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0x7f]
        );
        assert_eq!(record[48..80], sha256);
        assert_eq!(record[80..84], [1, 2, 3, 4]);
        assert_eq!(record[84..88], [3, 0, 0, 0]);
        assert_eq!(
            decode(&record),
            Record {
                hash: 0xdead_beef_0bad_f00d,
                key,
                sha256,
                epoch: 0x0403_0201,
                flags: OWNED | TRUSTED,
            }
        );
        // OWNED follows the key, whatever the flags say.
        let external = Key {
            owned: false,
            ..key
        };
        assert_eq!(decode(&encode(1, &external, &sha256, 1, OWNED)).flags, 0);
        let table = encode_table(&record);
        assert_eq!(table.len(), 32 + 88);
        assert_eq!(&table[0..8], b"BZRSDMEM");
        assert_eq!(table[8..12], [2, 0, 0, 0]);
        assert_eq!(table[12..16], [88, 0, 0, 0]);
        assert_eq!(table[16..24], [1, 0, 0, 0, 0, 0, 0, 0]);
        assert_eq!(table[24..32], blake3::hash(&record).as_bytes()[..8]);
        assert_eq!(&table[32..], &record);
        assert_eq!(validate(&table), Some(1));
        assert_eq!(validate(&encode_table(&[])), Some(0));
        // A version bump renames the files (binaries of two versions keep
        // their own tables instead of replacing each other's).
        assert_eq!(TABLE_NAME, format!("v{VERSION}.dmemo"));
        assert_eq!(LOCK_NAME, format!("v{VERSION}.lock"));
        assert_eq!(TEMPORARY_PREFIX, format!(".v{VERSION}.dmemo."));
    }

    #[test]
    fn saved_records_are_found_by_path_and_exact_key_only() {
        let cache = temp();
        let outside = temp();
        // An external file, and a blob by its birth time.
        let a = outside.path().join("a");
        let b = fs::canonicalize(cache.path()).unwrap().join("cas/0123-444");
        let writer = Table::new(cache.path());
        writer.remember(&a, key(2), sha(1), false);
        writer.remember(&b, key(3), sha(2), true);
        assert_eq!(
            writer.save().unwrap(),
            Saved::Written {
                records: 2,
                written: 2,
                bytes: 32 + 2 * 88
            }
        );
        let reader = Table::new(cache.path());
        assert_eq!(reader.loaded(), (Loaded::Valid, 2, 32 + 2 * 88));
        assert_eq!(reader.lookup(&a, &key(2)), Some(sha(1)));
        assert_eq!(reader.lookup(&b, &key(3)), Some(sha(2)));
        assert_eq!(reader.lookup(&b, &key(2)), None);
        // Any differing stat field (or kind) misses.
        let base = key(2);
        let stale = [
            Key {
                device: base.device + 1,
                ..base
            },
            Key {
                inode: base.inode + 1,
                ..base
            },
            Key {
                size: base.size + 1,
                ..base
            },
            Key {
                mtime_ns: base.mtime_ns + 1,
                ..base
            },
            Key {
                change_ns: base.change_ns + 1,
                ..base
            },
            Key {
                owned: !base.owned,
                ..base
            },
        ];
        for stale in stale {
            assert_eq!(reader.lookup(&a, &stale), None, "{stale:?}");
        }
        // Same key, another path: the path is part of the record.
        assert_eq!(reader.lookup(&outside.path().join("c"), &key(2)), None);
    }

    #[test]
    fn cache_areas_that_do_not_recur_are_never_recorded() {
        let cache = temp();
        let table = Table::new(cache.path());
        let root = fs::canonicalize(cache.path()).unwrap();
        let recorded = [
            "actions/k1/outputs/bin/out",
            "cas/0123-444",
            "repositories/trees/abc/BUILD.bazel",
            "repositories/direct/trees/abc/sub/file",
            "repositories/git/trees/abc/file",
            "repositories/builtins/trees/id/tools/x.sh",
        ];
        let ignored = [
            "work/k1-1-0/execroot/out",
            "cas/.tmp-0123-1-2",
            "repositories/trees/.stage-x/file",
            "repositories/direct/trees/.trash-x/tree/file",
            "repositories/direct/stages/slug/file",
            // A repository's directory (its slot, crate::repository_slots):
            // a rule's working area, a link to its tree, or mutable.
            "repositories/external/+r+n/file",
            "repositories/external/trees/x/file",
            "repositories/modextwd/trees/x/file",
            "repositories/downloads/trees/x/file",
            "repositories/trees/abc.json",
            "actions/k1",
            "locks/k1.lock",
            "digests/v2.dmemo",
        ];
        for (index, name) in recorded.iter().chain(&ignored).enumerate() {
            table.remember(
                &root.join(name),
                key(index as u64 * 2),
                sha(index as u8),
                false,
            );
        }
        table.remember(&root.join("actions/../cas/x"), key(40), sha(40), false);
        table.remember(Path::new("relative/path"), key(42), sha(42), false);
        assert_eq!(
            table.save().unwrap(),
            Saved::Written {
                records: recorded.len(),
                written: recorded.len(),
                bytes: 32 + recorded.len() * 88
            }
        );
        let reader = Table::new(cache.path());
        for (index, name) in recorded.iter().enumerate() {
            assert_eq!(
                reader.lookup(&root.join(name), &key(index as u64 * 2)),
                Some(sha(index as u8))
            );
        }
        let areas = [
            (Area::Owned, 2),
            (Area::Tree, recorded.len() - 2),
            (Area::Transient, ignored.len()),
        ];
        for (area, count) in areas {
            let names = recorded.iter().chain(&ignored);
            let found = names
                .filter(|name| reader.area(&root.join(name)) == area)
                .count();
            assert_eq!(found, count, "{area:?}");
        }
        assert_eq!(reader.area(&root), Area::Transient);
        assert_eq!(reader.area(&root.join("actions/../cas/x")), Area::Transient);
        assert_eq!(reader.area(Path::new("relative/path")), Area::Transient);
        assert_eq!(reader.area(outside_path()), Area::Outside);
        // Other spellings of the same paths classify alike.
        let spelled = |name: &str| PathBuf::from(format!("{}//{name}/", root.display()));
        assert_eq!(reader.area(&spelled("cas/0123-444")), Area::Owned);
        assert_eq!(
            reader.area(&spelled("repositories/trees/abc/BUILD.bazel")),
            Area::Tree
        );
        assert_eq!(reader.area(&spelled("work/k1-1-0/x")), Area::Transient);
    }

    /// A path outside every test cache.
    fn outside_path() -> &'static Path {
        Path::new("/nonexistent-digest-memo-test/file")
    }

    #[test]
    fn repository_tree_files_are_recorded_by_change_time_only() {
        let cache = temp();
        let root = fs::canonicalize(cache.path()).unwrap();
        let tree_file = root.join("repositories/direct/trees/abc/value");
        let blob = root.join("cas/0123-444");
        let (owned, external) = (key(1), key(2));
        let table = Table::new(cache.path());
        // Not by its birth time, not as trusted: nothing to save.
        table.remember(&tree_file, owned, sha(1), false);
        table.remember(&tree_file, external, sha(2), true);
        assert_eq!(table.save().unwrap(), Saved::Nothing);
        // By its change time, as read; a blob by its birth time, trusted.
        table.remember(&tree_file, external, sha(3), false);
        table.remember(&blob, owned, sha(4), true);
        assert!(matches!(
            table.save().unwrap(),
            Saved::Written {
                records: 2,
                written: 2,
                ..
            }
        ));
        let reader = Table::new(cache.path());
        assert_eq!(reader.lookup(&tree_file, &external), Some(sha(3)));
        assert_eq!(reader.lookup(&blob, &owned), Some(sha(4)));
        // Birth-time records of tree files, as the executor wrote them
        // before tree files were keyed by change time, never answer.
        write_records(
            cache.path(),
            &[
                (&tree_file, owned, sha(5), 0),
                (&tree_file, external, sha(6), 0),
                (&blob, owned, sha(7), TRUSTED),
            ],
        );
        let reader = Table::new(cache.path());
        assert_eq!(reader.loaded().1, 3);
        assert_eq!(reader.lookup(&tree_file, &owned), None);
        assert_eq!(reader.lookup(&tree_file, &external), Some(sha(6)));
        assert_eq!(reader.lookup(&blob, &owned), Some(sha(7)));
    }

    /// Records written under earlier, broader rules — 46c2d6b recorded the
    /// executor's birth-time keys for any `repositories/<kind>/trees/<id>/`
    /// path, 46c2d6b..8a532e7 for the tree stores' files — never answer: a
    /// birth-time key counts only for blobs and published outputs.
    #[test]
    fn birth_time_keys_count_only_for_blobs_and_published_outputs() {
        let cache = temp();
        let root = fs::canonicalize(cache.path()).unwrap();
        let owned = key(1);
        let valid = [
            root.join("cas/0123-444"),
            root.join("actions/k1/outputs/out"),
        ];
        let stale = [
            root.join("repositories/modextwd/trees/x/file"),
            root.join("repositories/downloads/trees/x/file"),
            root.join("repositories/direct/stages/slug/file"),
            root.join("repositories/external/+r+n/file"),
            root.join("repositories/trees/abc/BUILD.bazel"),
            root.join("repositories/git/trees/abc/file"),
            root.join("work/k1-1-0/execroot/out"),
            outside_path().to_path_buf(),
        ];
        let paths: Vec<&PathBuf> = valid.iter().chain(&stale).collect();
        let records: Vec<(&Path, Key, [u8; 32], u32)> = paths
            .iter()
            .enumerate()
            .map(|(index, path)| (path.as_path(), owned, sha(index as u8), TRUSTED))
            .collect();
        write_records(cache.path(), &records);
        let reader = Table::new(cache.path());
        assert_eq!(reader.loaded().1, valid.len() + stale.len());
        for (index, path) in valid.iter().enumerate() {
            assert_eq!(reader.lookup(path, &owned), Some(sha(index as u8)));
        }
        for path in &stale {
            assert_eq!(reader.lookup(path, &owned), None, "{}", path.display());
        }
        // Nor are such records made any more.
        for path in &stale {
            reader.remember(path, key(3), sha(9), false);
            reader.remember(path, key(3), sha(9), true);
        }
        assert_eq!(reader.save().unwrap(), Saved::Nothing);
    }

    #[test]
    fn settled_means_old_timestamps() {
        let dir = temp();
        let path = dir.path().join("file");
        fs::write(&path, b"bytes").unwrap();
        let metadata = fs::symlink_metadata(&path).unwrap();
        let now = SystemTime::now();
        // Just written: mtime and ctime are inside the racy window.
        assert!(!settled(&metadata, now, false));
        assert!(!settled(&metadata, now, true));
        assert!(settled(
            &metadata,
            now + RACY_MARGIN + Duration::from_secs(1),
            false
        ));
        assert!(settled(
            &metadata,
            now + RACY_MARGIN + Duration::from_secs(1),
            true
        ));
        // An old mtime settles nothing: a fresh ctime (also what every
        // link() to a store blob leaves) or a fresh birth time (`touch -d`,
        // `cp -p`, an extracted archive: a new file, maybe on the inode
        // number of one deleted a moment ago).
        File::options()
            .write(true)
            .open(&path)
            .unwrap()
            .set_modified(now - Duration::from_secs(60))
            .unwrap();
        let metadata = fs::symlink_metadata(&path).unwrap();
        assert!(!settled(&metadata, SystemTime::now(), true));
        assert!(!settled(&metadata, SystemTime::now(), false));
        // A future timestamp is never settled.
        File::options()
            .write(true)
            .open(&path)
            .unwrap()
            .set_modified(now + Duration::from_secs(3600))
            .unwrap();
        let metadata = fs::symlink_metadata(&path).unwrap();
        assert!(!settled(&metadata, SystemTime::now(), true));
        assert!(!settled(&metadata, SystemTime::now(), false));
    }

    #[test]
    fn only_settled_and_stable_reads_are_recorded() {
        let cache = temp();
        let outside = temp();
        let table = Table::new(cache.path());
        let path = outside.path().join("input");
        fs::write(&path, b"content").unwrap();
        // An old mtime, so that the same-size rewrite below necessarily
        // moves it (two writes in one timestamp tick could keep every time).
        File::options()
            .write(true)
            .open(&path)
            .unwrap()
            .set_modified(SystemTime::now() - Duration::from_secs(60))
            .unwrap();
        let expected: [u8; 32] = Sha256::digest(b"content").into();
        let metadata = fs::metadata(&path).unwrap();
        let read = |now: SystemTime| {
            file_sha256_in(
                Some(&table),
                &path,
                &metadata,
                Use::Recorded,
                sha256_all,
                || now,
            )
        };
        // Racy (just written): hashed, not recorded.
        assert_eq!(read(SystemTime::now()).unwrap(), expected);
        assert_eq!(
            table.lookup(&path, &Key::of(&metadata, false).unwrap()),
            None
        );
        assert_eq!(table.save().unwrap(), Saved::Nothing);
        assert!(!cache.path().join(DIRECTORY).exists());
        // Settled (the read starts later than the racy margin): recorded.
        let later = SystemTime::now() + RACY_MARGIN + Duration::from_secs(1);
        assert_eq!(read(later).unwrap(), expected);
        assert!(matches!(
            table.save().unwrap(),
            Saved::Written { written: 1, .. }
        ));
        // A later process takes the digest from the table without reading:
        // the file is not even opened.
        let reader = Table::new(cache.path());
        let digest = file_sha256_in(
            Some(&reader),
            &path,
            &metadata,
            Use::Recorded,
            |_| panic!("read"),
            || later,
        );
        assert_eq!(digest.unwrap(), expected);
        // A changed file (new key) is read again.
        fs::write(&path, b"changed").unwrap();
        let changed = fs::metadata(&path).unwrap();
        let digest = file_sha256_in(
            Some(&reader),
            &path,
            &changed,
            Use::Recorded,
            sha256_all,
            || later,
        );
        assert_eq!(
            digest.unwrap(),
            <[u8; 32]>::from(Sha256::digest(b"changed"))
        );
        // A file that changes while it is read is never recorded, and the
        // digest of what was read is returned.
        let unstable = outside.path().join("unstable");
        fs::write(&unstable, b"first").unwrap();
        let before = fs::metadata(&unstable).unwrap();
        let table = Table::new(cache.path());
        let digest = file_sha256_in(
            Some(&table),
            &unstable,
            &before,
            Use::Recorded,
            |file| {
                let digest = sha256_all(file)?;
                fs::OpenOptions::new()
                    .append(true)
                    .open(&unstable)?
                    .write_all(b" and more")?;
                Ok(digest)
            },
            || later,
        );
        assert_eq!(digest.unwrap(), <[u8; 32]>::from(Sha256::digest(b"first")));
        assert_eq!(
            table.lookup(&unstable, &Key::of(&before, false).unwrap()),
            None
        );
        // Without a table the file is simply hashed.
        let digest = file_sha256_in(None, &path, &changed, Use::Recorded, sha256_all, || later);
        assert_eq!(
            digest.unwrap(),
            <[u8; 32]>::from(Sha256::digest(b"changed"))
        );
    }

    #[test]
    fn path_hashers_take_settled_digests_from_the_table() {
        let cache = temp();
        let outside = temp();
        let path = outside.path().join("tool");
        fs::write(&path, b"binary").unwrap();
        File::options()
            .write(true)
            .open(&path)
            .unwrap()
            .set_modified(SystemTime::now() - Duration::from_secs(60))
            .unwrap();
        let expected = Sha256::digest(b"binary").to_vec();
        // With a table the file is hashed through opened_sha256: the
        // caller's hasher is only the fallback.
        let unused = || -> Result<Vec<u8>, String> { panic!("hashed by the caller") };
        let table = Table::new(cache.path());
        // Racy (the file's ctime is fresh): hashed, not recorded.
        let now = SystemTime::now();
        assert_eq!(
            path_sha256_in(Some(&table), &path, Use::Recorded, unused, || now),
            Ok(expected.clone())
        );
        assert_eq!(table.save().unwrap(), Saved::Nothing);
        // Settled: recorded; a later process takes it from the table.
        let later = SystemTime::now() + RACY_MARGIN + Duration::from_secs(1);
        assert_eq!(
            path_sha256_in(Some(&table), &path, Use::Recorded, unused, || later),
            Ok(expected.clone())
        );
        assert!(matches!(
            table.save().unwrap(),
            Saved::Written { written: 1, .. }
        ));
        let key = Key::of(&fs::metadata(&path).unwrap(), false).unwrap();
        let reader = Table::new(cache.path());
        assert_eq!(
            reader.lookup(&path, &key),
            Some(<[u8; 32]>::from(Sha256::digest(b"binary")))
        );
        assert_eq!(
            path_sha256_in(Some(&reader), &path, Use::Recorded, unused, || later),
            Ok(expected)
        );
        // A link to the file is another path: a record of its own.
        let link = outside.path().join("link");
        std::os::unix::fs::symlink(&path, &link).unwrap();
        assert_eq!(reader.lookup(&link, &key), None);
        assert!(path_sha256_in(Some(&reader), &link, Use::Recorded, unused, || later).is_ok());
        assert!(reader.lookup(&link, &key).is_some());
        // A changed file misses and is hashed again.
        fs::write(&path, b"rebuilt binary").unwrap();
        assert_eq!(
            path_sha256_in(Some(&reader), &path, Use::Recorded, unused, || later),
            Ok(Sha256::digest(b"rebuilt binary").to_vec())
        );
        // What does not open as a regular file goes to the caller's hasher,
        // whose errors and digests pass through, unrecorded.
        let failing = || -> Result<Vec<u8>, String> { Err("unreadable".to_owned()) };
        let missing = outside.path().join("missing");
        assert_eq!(
            path_sha256_in(Some(&reader), &missing, Use::Recorded, failing, || later),
            Err("unreadable".to_owned())
        );
        let twenty = || -> Result<Vec<u8>, String> { Ok(vec![7; 20]) };
        assert_eq!(
            path_sha256_in(Some(&reader), outside.path(), Use::Recorded, twenty, || {
                later
            }),
            Ok(vec![7; 20])
        );
        // Without a table (--verify_repo_tampering) the caller's hasher
        // always runs.
        let short = outside.path().join("short");
        fs::write(&short, b"x").unwrap();
        assert_eq!(
            path_sha256_in(None, &short, Use::Recorded, twenty, || later),
            Ok(vec![7; 20])
        );
    }

    /// The symlink-swap race of a stat-then-read-by-path hasher: a path
    /// that names file A when it is stat'ed and file B while it is read (a
    /// symlink swapped and swapped back; A itself is untouched, so its key
    /// is the same before and after) must never record B's digest under
    /// A's key. The key comes from the opened file's own fstat and the
    /// bytes from that file.
    #[test]
    fn digests_belong_to_the_opened_file() {
        let cache = temp();
        let outside = temp();
        let (a, b) = (outside.path().join("a"), outside.path().join("b"));
        fs::write(&a, b"file a").unwrap();
        fs::write(&b, b"file b").unwrap();
        let link = outside.path().join("link");
        let point = |target: &Path| {
            let temporary = outside.path().join("link.tmp");
            std::os::unix::fs::symlink(target, &temporary).unwrap();
            fs::rename(&temporary, &link).unwrap();
        };
        point(&a);
        let later = SystemTime::now() + RACY_MARGIN + Duration::from_secs(1);
        let (digest_a, digest_b) = (
            <[u8; 32]>::from(Sha256::digest(b"file a")),
            <[u8; 32]>::from(Sha256::digest(b"file b")),
        );
        // A path hasher that observes the swap: it reads B by path.
        let table = Table::new(cache.path());
        let swapping = || -> Result<Vec<u8>, String> {
            point(&b);
            let bytes = fs::read(&link).map_err(|e| e.to_string())?;
            point(&a);
            Ok(Sha256::digest(bytes).to_vec())
        };
        assert_eq!(
            path_sha256_in(Some(&table), &link, Use::Recorded, swapping, || later),
            Ok(digest_a.to_vec())
        );
        let key_a = Key::of(&fs::metadata(&link).unwrap(), false).unwrap();
        assert_eq!(table.lookup(&link, &key_a), Some(digest_a));
        // Swapped between the open and the read: the opened file's digest,
        // recorded under its own key.
        let opened = File::open(&link).unwrap();
        let metadata = opened.metadata().unwrap();
        point(&b);
        let fresh_cache = temp();
        let fresh = Table::new(fresh_cache.path());
        assert_eq!(
            opened_sha256_in(
                Some(&fresh),
                &opened,
                &metadata,
                &link,
                Use::Recorded,
                || later
            )
            .unwrap(),
            (digest_a, Answer::Read)
        );
        assert_eq!(fresh.lookup(&link, &key_a), Some(digest_a));
        let key_b = Key::of(&fs::metadata(&link).unwrap(), false).unwrap();
        assert_eq!(fresh.lookup(&link, &key_b), None);
        // A hit does not read the file: an opened file read to its end
        // still gets the recorded digest.
        let mut consumed = File::open(&a).unwrap();
        io::copy(&mut consumed, &mut io::sink()).unwrap();
        assert_eq!(
            opened_sha256_in(
                Some(&fresh),
                &consumed,
                &metadata,
                &link,
                Use::Recorded,
                || later
            )
            .unwrap(),
            (digest_a, Answer::Table)
        );
        assert_ne!(digest_a, digest_b);
    }

    #[test]
    fn a_save_without_new_records_writes_nothing() {
        let cache = temp();
        let outside = temp();
        let path = outside.path().join("file");
        let first = Table::new(cache.path());
        assert_eq!(first.save().unwrap(), Saved::Nothing);
        assert!(!cache.path().join(DIRECTORY).exists());
        // Trusted records alone (an output published again by every build)
        // never cause a write...
        let published = fs::canonicalize(cache.path())
            .unwrap()
            .join("actions/k1/outputs/published");
        first.remember(&published, key(1), sha(8), true);
        assert_eq!(first.save().unwrap(), Saved::Nothing);
        assert!(!cache.path().join(DIRECTORY).exists());
        // ...they ride along with a hashed one.
        first.remember(&path, key(0), sha(9), false);
        assert_eq!(
            first.save().unwrap(),
            Saved::Written {
                records: 2,
                written: 2,
                bytes: 32 + 2 * 88
            }
        );
        // Saved once: a second save in the same process writes nothing.
        assert_eq!(first.save().unwrap(), Saved::Nothing);
        let before = fs::symlink_metadata(table_path(cache.path())).unwrap();
        // A later process that only finds what the file holds writes nothing.
        let second = Table::new(cache.path());
        assert_eq!(second.lookup(&path, &key(0)), Some(sha(9)));
        second.remember(&path, key(0), sha(9), false);
        assert_eq!(second.save().unwrap(), Saved::Nothing);
        let after = fs::symlink_metadata(table_path(cache.path())).unwrap();
        assert_eq!(
            (before.ino(), before.mtime(), before.mtime_nsec()),
            (after.ino(), after.mtime(), after.mtime_nsec())
        );
    }

    #[test]
    fn truncated_corrupt_and_foreign_tables_read_as_empty() {
        let cache = temp();
        let outside = temp();
        let path = outside.path().join("file");
        let writer = Table::new(cache.path());
        writer.remember(&path, key(0), sha(1), false);
        writer.remember(&outside.path().join("other"), key(2), sha(2), false);
        writer.save().unwrap();
        let table = table_path(cache.path());
        let good = fs::read(&table).unwrap();
        let mut corrupt_body = good.clone();
        *corrupt_body.last_mut().unwrap() ^= 1;
        let mut foreign_version = good.clone();
        foreign_version[8..12].copy_from_slice(&(VERSION + 1).to_le_bytes());
        // A version-1 table (it can hold wrong digests: see VERSION).
        let mut older_version = good.clone();
        older_version[8..12].copy_from_slice(&(VERSION - 1).to_le_bytes());
        let mut foreign_size = good.clone();
        foreign_size[12] = 96;
        let mut wrong_count = good.clone();
        wrong_count[16] = 3;
        let mut swapped = good[32..].to_vec();
        let (first, second) = swapped.split_at_mut(88);
        first.swap_with_slice(second);
        let cases: Vec<(&str, Vec<u8>)> = vec![
            ("empty", Vec::new()),
            ("short header", good[..20].to_vec()),
            ("truncated body", good[..good.len() - 1].to_vec()),
            ("truncated record", good[..32 + 88 + 40].to_vec()),
            ("corrupt body", corrupt_body),
            ("magic", [b"BZRSDMEN".as_slice(), &good[8..]].concat()),
            ("version", foreign_version),
            ("older version", older_version),
            ("record size", foreign_size),
            ("count", wrong_count),
            ("unsorted", encode_table(&swapped)),
            (
                "duplicate slot",
                encode_table(&[&good[32..120], &good[32..120]].concat()),
            ),
            ("garbage", vec![0xff; 1000]),
        ];
        for (name, bytes) in cases {
            fs::write(&table, &bytes).unwrap();
            let reader = Table::new(cache.path());
            assert_eq!(reader.loaded().0, Loaded::Ignored, "{name}");
            assert_eq!(reader.lookup(&path, &key(0)), None, "{name}");
            // A save replaces the ignored file with a valid one.
            reader.remember(&path, key(0), sha(1), false);
            assert!(
                matches!(reader.save().unwrap(), Saved::Written { records: 1, .. }),
                "{name}"
            );
            assert_eq!(
                Table::new(cache.path()).lookup(&path, &key(0)),
                Some(sha(1)),
                "{name}"
            );
        }
        // A directory in place of the file is ignored as well, and the
        // failing save reports an error instead of panicking.
        fs::remove_file(&table).unwrap();
        fs::create_dir(&table).unwrap();
        let reader = Table::new(cache.path());
        assert_eq!(reader.loaded().0, Loaded::Ignored);
        reader.remember(&path, key(0), sha(1), false);
        assert!(reader.save().is_err());
    }

    #[test]
    fn two_processes_merge_their_records() {
        let cache = temp();
        let outside = temp();
        let path = |name: &str| outside.path().join(name);
        let base = Table::new(cache.path());
        base.remember(&path("shared"), key(0), sha(1), false);
        base.remember(&path("changes"), key(2), sha(2), false);
        base.save().unwrap();
        // Two processes read the same file, then each finds other records.
        let a = Table::new(cache.path());
        let b = Table::new(cache.path());
        assert_eq!(a.lookup(&path("shared"), &key(0)), Some(sha(1)));
        assert_eq!(b.lookup(&path("shared"), &key(0)), Some(sha(1)));
        a.remember(&path("only-a"), key(4), sha(4), false);
        // The file changed: its newer state (larger change time) replaces
        // the old record, also against a process that still saw the old one.
        let changed = Key {
            change_ns: key(2).change_ns + 10,
            ..key(2)
        };
        a.remember(&path("changes"), changed, sha(5), false);
        b.remember(&path("only-b"), key(6), sha(6), false);
        let older = Key {
            change_ns: key(2).change_ns - 10,
            ..key(2)
        };
        b.remember(&path("changes"), older, sha(3), false);
        assert!(matches!(
            a.save().unwrap(),
            Saved::Written {
                records: 3,
                written: 2,
                ..
            }
        ));
        assert!(matches!(
            b.save().unwrap(),
            Saved::Written {
                records: 4,
                written: 2,
                ..
            }
        ));
        let merged = Table::new(cache.path());
        assert_eq!(merged.loaded().1, 4);
        assert_eq!(merged.lookup(&path("shared"), &key(0)), Some(sha(1)));
        assert_eq!(merged.lookup(&path("only-a"), &key(4)), Some(sha(4)));
        assert_eq!(merged.lookup(&path("only-b"), &key(6)), Some(sha(6)));
        assert_eq!(merged.lookup(&path("changes"), &changed), Some(sha(5)));
        assert_eq!(merged.lookup(&path("changes"), &older), None);
        assert_eq!(merged.lookup(&path("changes"), &key(2)), None);
        // An owned and an external record of one path (a published output,
        // the only kind of path both can occur for) are separate slots.
        let output = fs::canonicalize(cache.path())
            .unwrap()
            .join("actions/k1/outputs/out");
        let owned = Key {
            owned: true,
            ..key(0)
        };
        merged.remember(&output, key(0), sha(6), false);
        merged.remember(&output, owned, sha(7), true);
        merged.remember(&path("new"), key(8), sha(8), false);
        merged.save().unwrap();
        let both = Table::new(cache.path());
        assert_eq!(both.lookup(&output, &key(0)), Some(sha(6)));
        assert_eq!(both.lookup(&output, &owned), Some(sha(7)));
        assert_eq!(both.lookup(&path("shared"), &key(0)), Some(sha(1)));
        let saved = records(cache.path());
        assert_eq!(saved.len(), 7);
        assert!(
            saved
                .windows(2)
                .all(|pair| (pair[0].hash, pair[0].key.owned) < (pair[1].hash, pair[1].key.owned))
        );
        assert_eq!(saved.iter().map(|record| record.epoch).max(), Some(4));
        assert!(saved.iter().any(|record| record.flags == OWNED | TRUSTED));
    }

    #[test]
    fn concurrent_saves_lose_no_records() {
        let cache = temp();
        let outside = temp();
        let name = |thread: u64, index: u64| outside.path().join(format!("{thread}-{index}"));
        std::thread::scope(|scope| {
            for thread in 0..8u64 {
                let name = &name;
                let cache = cache.path();
                scope.spawn(move || {
                    let table = Table::new(cache);
                    for index in 0..50u64 {
                        let seed = thread * 1000 + index * 2;
                        table.remember(&name(thread, index), key(seed), sha(index as u8), false);
                    }
                    table.save().unwrap();
                });
            }
        });
        let reader = Table::new(cache.path());
        assert_eq!(reader.loaded(), (Loaded::Valid, 400, 32 + 400 * 88));
        for thread in 0..8u64 {
            for index in 0..50u64 {
                let seed = thread * 1000 + index * 2;
                assert_eq!(
                    reader.lookup(&name(thread, index), &key(seed)),
                    Some(sha(index as u8))
                );
            }
        }
    }

    #[test]
    fn eviction_drops_the_oldest_epochs() {
        let cache = temp();
        let outside = temp();
        let name = |epoch: u64, index: u64| outside.path().join(format!("{epoch}-{index}"));
        let limit = 16;
        // Five saves (epochs 1..=5) of 4 records each: 20 > 16.
        for epoch in 0..5u64 {
            let table = Table::with_limit(cache.path(), limit);
            for index in 0..4u64 {
                table.remember(
                    &name(epoch, index),
                    key(epoch * 100 + index * 2),
                    sha(epoch as u8),
                    false,
                );
            }
            table.save().unwrap();
        }
        // Down to 7/8 of the limit: 14 records, epochs 5, 4, 3 and two of 2.
        let saved = records(cache.path());
        assert_eq!(saved.len(), 14);
        let in_epoch = |epoch: u32| saved.iter().filter(|record| record.epoch == epoch).count();
        assert_eq!(
            (
                in_epoch(1),
                in_epoch(2),
                in_epoch(3),
                in_epoch(4),
                in_epoch(5)
            ),
            (0, 2, 4, 4, 4)
        );
        let reader = Table::with_limit(cache.path(), limit);
        for index in 0..4u64 {
            assert_eq!(
                reader.lookup(&name(4, index), &key(400 + index * 2)),
                Some(sha(4))
            );
            assert_eq!(reader.lookup(&name(0, index), &key(index * 2)), None);
        }
        // A process never holds more new records than the limit.
        let table = Table::with_limit(cache.path(), 4);
        for index in 0..10u64 {
            table.remember(&name(9, index), key(10_000 + index * 2), sha(1), false);
        }
        assert!(matches!(
            table.save().unwrap(),
            Saved::Written {
                written: 4,
                records: 4,
                ..
            }
        ));
    }

    #[test]
    fn keys_follow_the_file_state() {
        let dir = temp();
        let path = dir.path().join("file");
        fs::write(&path, b"one").unwrap();
        // An old mtime, so that the rewrite below necessarily moves it (two
        // writes within one timestamp tick could leave every time equal).
        File::options()
            .write(true)
            .open(&path)
            .unwrap()
            .set_modified(SystemTime::now() - Duration::from_secs(60))
            .unwrap();
        let metadata = fs::symlink_metadata(&path).unwrap();
        let external = Key::of(&metadata, false).unwrap();
        assert_eq!((external.size, external.owned), (3, false));
        assert_eq!(
            external.change_ns,
            metadata.ctime() * 1_000_000_000 + metadata.ctime_nsec()
        );
        if let Some(owned) = Key::of(&metadata, true) {
            // The birth time does not move when the file is linked.
            fs::hard_link(&path, dir.path().join("link")).unwrap();
            let linked = fs::symlink_metadata(&path).unwrap();
            assert_eq!(Key::of(&linked, true), Some(owned));
            assert!(owned.owned);
        }
        fs::write(&path, b"two").unwrap();
        let rewritten = fs::symlink_metadata(&path).unwrap();
        assert_ne!(Key::of(&rewritten, false), Some(external));
        assert_eq!(parse_hex(&hex(&sha(0xab))), Some(sha(0xab)));
        assert_eq!(parse_hex("ab"), None);
        assert_eq!(parse_hex(&"A".repeat(64)), None);
    }

    #[test]
    fn lookups_find_records_among_many() {
        let cache = temp();
        let outside = temp();
        let name = |index: u64| outside.path().join(index.to_string());
        let writer = Table::new(cache.path());
        for index in 0..2000u64 {
            writer.remember(
                &name(index),
                key(index * 2),
                sha((index % 251) as u8),
                false,
            );
        }
        writer.save().unwrap();
        let reader = Table::new(cache.path());
        assert_eq!(reader.loaded().1, 2000);
        for index in 0..2000u64 {
            assert_eq!(
                reader.lookup(&name(index), &key(index * 2)),
                Some(sha((index % 251) as u8))
            );
            assert_eq!(reader.lookup(&name(index), &key(index * 2 + 2)), None);
        }
    }
}
