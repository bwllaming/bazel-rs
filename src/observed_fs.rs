//! Recording filesystem and environment facade for the analysis snapshot
//! (L2-W3 of the persistent-snapshot program, PLAN-L2 §1b).
//!
//! Loading, module resolution and analysis read the filesystem and the
//! environment only through these functions (`tests/observed_fs_discipline.rs`
//! enforces it). Each function performs exactly the system call of the `std`
//! function it replaces and returns that result unchanged.
//!
//! While the process-global recording window is open (`BAZEL_RS_SNAPSHOT=record`,
//! crate::snapshot::record) a call also records `(op, path) -> outcome`: the
//! value the caller actually used (a READ's SHA-256 of the bytes read, a
//! projected directory listing, a file type, a canonical path), so a later run
//! can re-validate every input of planning without running it (§1c,
//! crate::snapshot::validate). Outside the window every function costs one
//! relaxed atomic load on top of the `std` call; nothing is allocated.
//!
//! Semantic records complete the filesystem ones: REPO records (a repository
//! tree whose integrity the run verified, with the lstat key of every entry —
//! INTEGRITY), ENV digests (never values) and TAINTs: conditions under which a
//! run must never be saved (network, executed non-local repository rules,
//! printing local ones, evaluated extensions, pending lockfile writes, symlink
//! hops into live repository roots, build-info callbacks, `map_each` over tree
//! artifacts, errors, and Unstable — the same `(op, path)` observed with two
//! outcomes, or a file that changed while it was read). Local repository rules
//! execute on every run; their executions become replay records instead
//! (crate::snapshot::replay).
//!
//! The recorder is sharded by path hash so the parallel loading workers never
//! contend on one lock; every path is stored once (interned) with the list of
//! its observations.

use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::ffi::{OsStr, OsString};
use std::fmt;
use std::fs;
use std::hash::{Hash, Hasher};
use std::io::{self, Read, Write};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, MutexGuard, OnceLock, PoisonError};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// The racy window (`action::RACY_MARGIN`, `repository_store::RACY_MARGIN`):
/// a file whose mtime or ctime is younger than this relative to the window
/// start could still be rewritten without changing its stat key, so it is
/// re-validated by content until a later check settles it.
pub const RACY_MARGIN_NS: i64 = 2_000_000_000;

/// Marker paths stat'ed when the window opens and closes, so a syscall trace
/// can bracket the window (scripts/snapshot_fs_audit.py). They never exist.
pub const MARKER_OPEN: &str = "/.bazel-rs-snapshot-marker/window-open";
pub const MARKER_CLOSE: &str = "/.bazel-rs-snapshot-marker/window-close";
/// Marker paths bracketing, on the executing thread, work whose filesystem
/// accesses need no record because it taints the run anyway or is replayed
/// and compared instead (a repository rule or module extension executing,
/// [`tainted_section`]; local repository rules, crate::snapshot::replay):
/// the syscall audit skips that thread's calls between them.
pub const MARKER_TAINTED_BEGIN: &str = "/.bazel-rs-snapshot-marker/tainted-begin";
pub const MARKER_TAINTED_END: &str = "/.bazel-rs-snapshot-marker/tainted-end";

const SHARDS: usize = 64;
/// Distinct details kept per taint kind (the dump lists them all; the
/// stderr summary the first few).
const TAINT_DETAILS: usize = 1000;

static RECORDING: AtomicBool = AtomicBool::new(false);
static RECORDER: OnceLock<Recorder> = OnceLock::new();

/// The recorder while the window is open.
#[inline]
fn active() -> Option<&'static Recorder> {
    if RECORDING.load(Ordering::Relaxed) {
        RECORDER.get()
    } else {
        None
    }
}

/// Whether the recording window is open.
#[inline]
pub fn recording() -> bool {
    RECORDING.load(Ordering::Relaxed)
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

// ---- records ---------------------------------------------------------------

/// A file's content identity for stat-based trust: node id, size, mtime and
/// ctime (Bazel's DigestUtils / FileContentsProxy; ctime because `mv` and
/// `touch -d` preserve or restore mtime).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct StatKey {
    pub dev: u64,
    pub ino: u64,
    pub size: u64,
    pub mtime_ns: i64,
    pub ctime_ns: i64,
}

impl StatKey {
    pub fn of(metadata: &fs::Metadata) -> Self {
        let ns =
            |seconds: i64, nanos: i64| seconds.saturating_mul(1_000_000_000).saturating_add(nanos);
        Self {
            dev: metadata.dev(),
            ino: metadata.ino(),
            size: metadata.size(),
            mtime_ns: ns(metadata.mtime(), metadata.mtime_nsec()),
            ctime_ns: ns(metadata.ctime(), metadata.ctime_nsec()),
        }
    }

    /// The file may still change without changing this key: its mtime or
    /// ctime is within [`RACY_MARGIN_NS`] of `reference_ns` or later (a
    /// future-dated timestamp is always racy).
    pub fn racy(&self, reference_ns: i64) -> bool {
        self.mtime_ns
            .max(self.ctime_ns)
            .saturating_add(RACY_MARGIN_NS)
            > reference_ns
    }
}

impl fmt::Display for StatKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{}:{}:{}:{}:{}",
            self.dev, self.ino, self.size, self.mtime_ns, self.ctime_ns
        )
    }
}

/// A file type class (no permissions, no timestamps).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum FileKind {
    File,
    Dir,
    Symlink,
    Other,
}

impl FileKind {
    pub fn of(file_type: fs::FileType) -> Self {
        if file_type.is_symlink() {
            Self::Symlink
        } else if file_type.is_dir() {
            Self::Dir
        } else if file_type.is_file() {
            Self::File
        } else {
            Self::Other
        }
    }

    /// The dump / digest letter: `f`, `d`, `l` or `o`.
    pub fn letter(self) -> u8 {
        match self {
            Self::File => b'f',
            Self::Dir => b'd',
            Self::Symlink => b'l',
            Self::Other => b'o',
        }
    }
}

/// Which entries of a directory listing the caller uses (PLAN-L2 §1b): the
/// record digests exactly those, so e.g. a new `.go` file in a package that
/// is only walked by `//x/...` discovery does not invalidate the snapshot.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Projection {
    /// Every entry (names and types).
    All,
    /// Entries that are directories without following symlinks (package
    /// discovery, `workspace::discover_packages_skipping`).
    Directories,
    /// Entries one glob include pattern can match or descend into at
    /// `relative` (below the package), plus the BUILD markers a subpackage
    /// lookup consults and undecodable names (`glob::listing_keeps`).
    Glob { pattern: String, relative: String },
}

impl Projection {
    /// Whether the projection keeps an entry; `kind` is the entry's
    /// unfollowed type when known (an entry whose type cannot be read is kept
    /// so the caller sees the error).
    pub fn keeps(&self, name: &OsStr, kind: Option<FileKind>) -> bool {
        match self {
            Self::All => true,
            Self::Directories => kind.is_none_or(|kind| kind == FileKind::Dir),
            Self::Glob { pattern, relative } => crate::glob::listing_keeps(pattern, relative, name),
        }
    }
}

impl fmt::Display for Projection {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::All => f.write_str("all"),
            Self::Directories => f.write_str("dirs"),
            Self::Glob { pattern, relative } => {
                write!(f, "glob:{}@{}", escape_str(pattern), escape_str(relative))
            }
        }
    }
}

/// One kind of filesystem observation.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Op {
    /// Contents read (or hashed).
    Read,
    /// `metadata` (following symlinks): the type class.
    Stat,
    /// `symlink_metadata`: the type class; a symlink's target.
    Lstat,
    /// A projected directory listing.
    ReadDir(Projection),
    /// `canonicalize`: the resulting path.
    Canon,
    /// `read_link`: the target.
    ReadLink,
}

impl Op {
    pub fn name(&self) -> &'static str {
        match self {
            Self::Read => "READ",
            Self::Stat => "STAT",
            Self::Lstat => "LSTAT",
            Self::ReadDir(_) => "READDIR",
            Self::Canon => "CANON",
            Self::ReadLink => "READLINK",
        }
    }
}

/// What an observation saw.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Outcome {
    /// The call failed with this errno (-1 when the error has none).
    Error(i32),
    /// READ: the SHA-256 of the bytes read, the stat key (lstat before the
    /// read, fstat after it; equal or the run is Unstable) and whether the key
    /// was racy at the window start.
    Read {
        sha256: [u8; 32],
        key: StatKey,
        racy: bool,
    },
    /// STAT / LSTAT of a non-symlink: the type class.
    Kind(FileKind),
    /// LSTAT of a symlink: its target.
    Link(PathBuf),
    /// READDIR: the digest of the projected `(name, type)` entries, their
    /// number, the directory's stat key (before and after the listing) and
    /// whether it was racy.
    Listing {
        digest: [u8; 32],
        entries: u32,
        key: StatKey,
        racy: bool,
    },
    /// CANON / READLINK: the resulting path.
    Path(PathBuf),
}

impl fmt::Display for Outcome {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Error(errno) => write!(f, "error:{errno}"),
            Self::Read { sha256, key, racy } => write!(
                f,
                "sha256:{} key:{key}{}",
                hex(sha256),
                if *racy { " racy" } else { "" }
            ),
            Self::Kind(kind) => write!(f, "kind:{}", kind.letter() as char),
            Self::Link(target) => write!(f, "link:{}", escape(target.as_os_str())),
            Self::Listing {
                digest,
                entries,
                key,
                racy,
            } => write!(
                f,
                "entries:{entries} digest:{} key:{key}{}",
                hex(digest),
                if *racy { " racy" } else { "" }
            ),
            Self::Path(path) => write!(f, "path:{}", escape(path.as_os_str())),
        }
    }
}

/// One observation of a path.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Observation {
    pub op: Op,
    pub outcome: Outcome,
}

/// One environment variable read during the window, as a digest.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EnvObservation {
    /// SHA-256 of the value's bytes; `None` when unset.
    pub digest: Option<[u8; 32]>,
    /// The value is fixed by `--repo_env=NAME=VALUE` / `=NAME` (an option,
    /// part of the snapshot key), not read from the process environment.
    pub fixed: bool,
}

/// One entry of a verified repository tree (INTEGRITY).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IntegrityEntry {
    pub path: PathBuf,
    /// The lstat outcome: `Ok((kind, key))`, or the errno.
    pub state: Result<(FileKind, StatKey), i32>,
    pub racy: bool,
}

/// A repository root the run verified (REPO + its INTEGRITY entries).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RepositoryRecord {
    /// `registry`, `direct`, `git` or `builtin`.
    pub kind: &'static str,
    pub name: String,
    pub root: PathBuf,
    pub manifest: Option<PathBuf>,
    pub entries: Vec<IntegrityEntry>,
}

/// Conditions under which a run is never saved (PLAN-L2 §1b "Taints").
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Taint {
    /// Network access: registry files, archive, extension or repository
    /// downloads, Git fetches.
    Network,
    /// A repository rule that is not `local = True` was executed (a cold
    /// fetch or a changed specification). Local rules always execute; they
    /// are replayed and compared instead (crate::snapshot::replay).
    RepositoryRuleExecuted,
    /// A local repository rule printed (print(), `execute(quiet=False)`,
    /// the reproducibility DEBUG lines): v1 of the replay protocol does not
    /// splice live output into the replayed stderr (PLAN-L2 update 1 §(b)).
    LocalRepositoryOutput,
    /// Replaying a local repository rule did not reproduce its recorded
    /// identity inputs and tree (the record mode's dry run).
    ReplayMismatch,
    /// A module extension's implementation was evaluated (not reused from a
    /// lockfile entry).
    ExtensionEvaluated,
    /// A repository tree was (re)published: archive extraction, Git export,
    /// built-in tools staging.
    RepositoryMaterialized,
    /// MODULE.bazel.lock or the hidden lockfile would be written.
    LockfileWritePending,
    /// A symlink hop left its repository into another live (not
    /// content-addressed) repository root — a `local_path_override` module
    /// or a local registry source — whose use depends on what was
    /// materialized earlier in the run. Published store trees, the main
    /// repository and plain host paths resolve the same in any order and
    /// every hop is recorded (crate::source_links::LinkPolicy::hop_taint).
    CrossRepositorySymlink,
    /// An action uses a build-info callback (`transform_version_file` /
    /// `transform_info_file`), which runs Starlark at plan time.
    BuildInfo,
    /// An Args expansion over a tree artifact calls a Starlark `map_each`
    /// at execution time.
    MapEach,
    /// Planning failed, reported errors, or the command failed.
    Errors,
    /// An observation changed during the window.
    Unstable,
    /// A test or debug hook is set (BAZEL_RS_LOADING_JITTER,
    /// BAZEL_RS_LOADING_NO_DEMAND, BAZEL_RS_DEBUG_*).
    TestHook,
}

impl Taint {
    pub fn name(self) -> &'static str {
        match self {
            Self::Network => "network",
            Self::RepositoryRuleExecuted => "repository-rule-executed",
            Self::LocalRepositoryOutput => "local-repository-output",
            Self::ReplayMismatch => "local-repository-replay-mismatch",
            Self::ExtensionEvaluated => "extension-evaluated",
            Self::RepositoryMaterialized => "repository-materialized",
            Self::LockfileWritePending => "lockfile-write-pending",
            Self::CrossRepositorySymlink => "cross-repository-symlink",
            Self::BuildInfo => "build-info-callback",
            Self::MapEach => "map-each-deferred",
            Self::Errors => "errors",
            Self::Unstable => "unstable",
            Self::TestHook => "test-hook",
        }
    }

    /// Recorded when an action is registered during analysis, which the
    /// plan may still prune (it keeps only the actions requested outputs
    /// need): an upper bound until the saved plan itself is checked (W5).
    pub fn plan_dependent(self) -> bool {
        matches!(self, Self::BuildInfo | Self::MapEach)
    }
}

/// How often a taint occurred, with the first details (deduplicated).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TaintRecord {
    pub count: usize,
    pub details: Vec<String>,
}

// ---- the recorder ----------------------------------------------------------

type Shard = Mutex<HashMap<PathBuf, Vec<Observation>>>;

/// The process-global recorder of one window.
pub struct Recorder {
    window_start_ns: i64,
    started: Instant,
    shards: Vec<Shard>,
    env: Mutex<BTreeMap<OsString, EnvObservation>>,
    fixed_env: BTreeSet<OsString>,
    repositories: Mutex<Vec<RepositoryRecord>>,
    repository_roots: Mutex<HashSet<PathBuf>>,
    taints: Mutex<BTreeMap<Taint, TaintRecord>>,
    window: Mutex<Option<Duration>>,
}

/// Everything one window recorded, sorted (paths in byte order).
#[derive(Clone, Debug, Default)]
pub struct Recording {
    pub window_start_ns: i64,
    /// Window length; `None` while still open.
    pub window: Option<Duration>,
    pub observations: Vec<(PathBuf, Observation)>,
    pub env: Vec<(OsString, EnvObservation)>,
    pub repositories: Vec<RepositoryRecord>,
    pub taints: BTreeMap<Taint, TaintRecord>,
}

impl Recording {
    /// Number of distinct observed paths.
    pub fn distinct_paths(&self) -> usize {
        let mut count = 0;
        let mut previous: Option<&Path> = None;
        for (path, _) in &self.observations {
            if previous != Some(path.as_path()) {
                count += 1;
                previous = Some(path);
            }
        }
        count
    }
}

fn now_ns() -> i64 {
    match SystemTime::now().duration_since(UNIX_EPOCH) {
        Ok(elapsed) => i64::try_from(elapsed.as_nanos()).unwrap_or(i64::MAX),
        Err(_) => 0,
    }
}

fn errno(error: &io::Error) -> i32 {
    error.raw_os_error().unwrap_or(-1)
}

fn sha256(bytes: &[u8]) -> [u8; 32] {
    Sha256::digest(bytes).into()
}

fn shard_of(path: &Path) -> usize {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    path.hash(&mut hasher);
    (hasher.finish() as usize) % SHARDS
}

/// The digest of a projected listing: `(name, type letter)` pairs in byte
/// order, each NUL-framed. Shared by recording and validation.
pub fn listing_digest(entries: &mut [(OsString, u8)]) -> [u8; 32] {
    entries.sort();
    let mut digest = Sha256::new();
    for (name, kind) in entries.iter() {
        digest.update(name.as_bytes());
        digest.update([0, *kind, 0]);
    }
    digest.finalize().into()
}

impl Recorder {
    fn new(window_start_ns: i64, fixed_env: BTreeSet<OsString>) -> Self {
        Self {
            window_start_ns,
            started: Instant::now(),
            shards: (0..SHARDS).map(|_| Mutex::default()).collect(),
            env: Mutex::default(),
            fixed_env,
            repositories: Mutex::default(),
            repository_roots: Mutex::default(),
            taints: Mutex::default(),
            window: Mutex::default(),
        }
    }

    fn observe(&self, path: &Path, op: Op, outcome: Outcome) {
        let mut shard = lock(&self.shards[shard_of(path)]);
        let conflict = match shard.get_mut(path) {
            Some(observations) => match observations.iter().find(|seen| seen.op == op) {
                Some(seen) if seen.outcome == outcome => None,
                Some(seen) => Some(format!(
                    "{} {}: {} then {outcome}",
                    op.name(),
                    escape(path.as_os_str()),
                    seen.outcome
                )),
                None => {
                    observations.push(Observation { op, outcome });
                    None
                }
            },
            None => {
                shard.insert(path.to_path_buf(), vec![Observation { op, outcome }]);
                None
            }
        };
        drop(shard);
        if let Some(conflict) = conflict {
            self.taint(Taint::Unstable, conflict);
        }
    }

    fn taint(&self, kind: Taint, detail: String) {
        let mut taints = lock(&self.taints);
        let record = taints.entry(kind).or_default();
        record.count += 1;
        if record.details.len() < TAINT_DETAILS && !record.details.contains(&detail) {
            record.details.push(detail);
        }
    }

    fn racy(&self, key: &StatKey) -> bool {
        key.racy(self.window_start_ns)
    }

    fn observe_lstat(&self, path: &Path, result: &io::Result<fs::Metadata>) {
        let outcome = match result {
            Ok(metadata) if metadata.file_type().is_symlink() => match fs::read_link(path) {
                Ok(target) => Outcome::Link(target),
                Err(error) => {
                    self.taint(
                        Taint::Unstable,
                        format!("LSTAT {}: link vanished: {error}", escape(path.as_os_str())),
                    );
                    Outcome::Error(errno(&error))
                }
            },
            Ok(metadata) => Outcome::Kind(FileKind::of(metadata.file_type())),
            Err(error) => Outcome::Error(errno(error)),
        };
        self.observe(path, Op::Lstat, outcome);
    }

    /// The stat key a READ of `path` trusts, taken before the read: lstat, or
    /// for a symlink the target's stat (the link itself becomes an LSTAT
    /// observation, so retargeting it is seen).
    fn key_before(&self, path: &Path) -> Option<StatKey> {
        let lstat = fs::symlink_metadata(path);
        match &lstat {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                self.observe_lstat(path, &lstat);
                fs::metadata(path)
                    .ok()
                    .map(|metadata| StatKey::of(&metadata))
            }
            Ok(metadata) => Some(StatKey::of(metadata)),
            Err(_) => None,
        }
    }

    fn finish_read(
        &self,
        path: &Path,
        before: Option<StatKey>,
        after: io::Result<fs::Metadata>,
        sha256: [u8; 32],
    ) {
        let after = after.ok().map(|metadata| StatKey::of(&metadata));
        let key = match (before, after) {
            (Some(before), Some(after)) if before == after => after,
            (_, after) => {
                self.taint(
                    Taint::Unstable,
                    format!("READ {}: changed while read", escape(path.as_os_str())),
                );
                after.or(before).unwrap_or_default()
            }
        };
        let racy = self.racy(&key);
        self.observe(path, Op::Read, Outcome::Read { sha256, key, racy });
    }

    fn read(&self, path: &Path) -> io::Result<Vec<u8>> {
        self.read_staged(path).map_err(ReadError::into_inner)
    }

    fn read_staged(&self, path: &Path) -> Result<Vec<u8>, ReadError> {
        let before = self.key_before(path);
        let mut file = match fs::File::open(path) {
            Ok(file) => file,
            Err(error) => {
                self.observe(path, Op::Read, Outcome::Error(errno(&error)));
                return Err(ReadError::Open(error));
            }
        };
        // fs::read's size hint (it ignores a failing fstat too).
        let size = file
            .metadata()
            .ok()
            .and_then(|metadata| usize::try_from(metadata.len()).ok());
        let mut bytes = Vec::with_capacity(size.unwrap_or(0));
        if let Err(error) = file.read_to_end(&mut bytes) {
            self.observe(path, Op::Read, Outcome::Error(errno(&error)));
            return Err(ReadError::Read(error));
        }
        self.finish_read(path, before, file.metadata(), sha256(&bytes));
        Ok(bytes)
    }

    fn sha256_file(&self, path: &Path) -> io::Result<[u8; 32]> {
        let before = self.key_before(path);
        let mut file = match fs::File::open(path) {
            Ok(file) => file,
            Err(error) => {
                self.observe(path, Op::Read, Outcome::Error(errno(&error)));
                return Err(error);
            }
        };
        let mut digest = Sha256::new();
        if let Err(error) = io::copy(&mut file, &mut digest) {
            self.observe(path, Op::Read, Outcome::Error(errno(&error)));
            return Err(error);
        }
        let sha256 = digest.finalize().into();
        self.finish_read(path, before, file.metadata(), sha256);
        Ok(sha256)
    }

    fn read_dir_projected(
        &self,
        directory: &Path,
        projection: &Projection,
    ) -> io::Result<Vec<io::Result<fs::DirEntry>>> {
        let op = Op::ReadDir(projection.clone());
        let before = fs::metadata(directory)
            .ok()
            .map(|metadata| StatKey::of(&metadata));
        let entries = match fs::read_dir(directory) {
            Ok(entries) => entries,
            Err(error) => {
                self.observe(directory, op, Outcome::Error(errno(&error)));
                return Err(error);
            }
        };
        let mut kept = Vec::new();
        let mut digested = Vec::new();
        let mut failure = None;
        for entry in entries {
            match entry {
                Ok(entry) => {
                    let name = entry.file_name();
                    let kind = entry.file_type().ok().map(FileKind::of);
                    if projection.keeps(&name, kind) {
                        digested.push((name, kind.map_or(b'e', FileKind::letter)));
                        kept.push(Ok(entry));
                    }
                }
                Err(error) => {
                    failure.get_or_insert(errno(&error));
                    kept.push(Err(error));
                }
            }
        }
        let after = fs::metadata(directory)
            .ok()
            .map(|metadata| StatKey::of(&metadata));
        let outcome = match failure {
            Some(errno) => Outcome::Error(errno),
            None => {
                let key = match (before, after) {
                    (Some(before), Some(after)) if before == after => after,
                    (_, after) => {
                        self.taint(
                            Taint::Unstable,
                            format!(
                                "READDIR {}: changed while listed",
                                escape(directory.as_os_str())
                            ),
                        );
                        after.or(before).unwrap_or_default()
                    }
                };
                Outcome::Listing {
                    entries: u32::try_from(digested.len()).unwrap_or(u32::MAX),
                    digest: listing_digest(&mut digested),
                    key,
                    racy: self.racy(&key),
                }
            }
        };
        self.observe(directory, op, outcome);
        Ok(kept)
    }

    fn observe_env(&self, name: &OsStr, value: Option<&OsStr>) {
        let observation = EnvObservation {
            digest: value.map(|value| sha256(value.as_bytes())),
            fixed: self.fixed_env.contains(name),
        };
        let mut env = lock(&self.env);
        match env.get(name) {
            Some(seen) if *seen == observation => {}
            Some(_) => {
                drop(env);
                self.taint(
                    Taint::Unstable,
                    format!("ENV {}: two values in one run", escape(name)),
                );
            }
            None => {
                env.insert(name.to_owned(), observation);
            }
        }
    }

    /// Everything recorded so far, sorted.
    pub fn snapshot(&self) -> Recording {
        let mut observations = Vec::new();
        for shard in &self.shards {
            for (path, list) in lock(shard).iter() {
                for observation in list {
                    observations.push((path.clone(), observation.clone()));
                }
            }
        }
        observations.sort_by(|(a, x), (b, y)| a.cmp(b).then_with(|| x.op.cmp(&y.op)));
        let mut repositories = lock(&self.repositories).clone();
        repositories.sort_by(|a, b| a.root.cmp(&b.root));
        Recording {
            window_start_ns: self.window_start_ns,
            window: *lock(&self.window),
            observations,
            env: lock(&self.env)
                .iter()
                .map(|(name, observation)| (name.clone(), observation.clone()))
                .collect(),
            repositories,
            taints: lock(&self.taints).clone(),
        }
    }
}

// ---- the window ------------------------------------------------------------

/// Open the process's recording window (once per process). `fixed_env` are
/// the environment names whose repository-environment value an option fixes
/// (`--repo_env=NAME=VALUE`, `--repo_env==NAME`). Returns false when a window
/// was already opened.
pub fn open_window(fixed_env: BTreeSet<OsString>) -> bool {
    if RECORDER.set(Recorder::new(now_ns(), fixed_env)).is_err() {
        return false;
    }
    let _ = fs::symlink_metadata(MARKER_OPEN);
    RECORDING.store(true, Ordering::SeqCst);
    true
}

/// Close the window (idempotent): planning ended. Later calls record nothing.
pub fn close_window() {
    if RECORDING.swap(false, Ordering::SeqCst) {
        if let Some(recorder) = RECORDER.get() {
            *lock(&recorder.window) = Some(recorder.started.elapsed());
        }
        let _ = fs::symlink_metadata(MARKER_CLOSE);
    }
}

/// The recorder of this process's window, open or closed.
pub fn recorder() -> Option<&'static Recorder> {
    RECORDER.get()
}

// ---- the facade ------------------------------------------------------------

/// [`fs::read`], recording READ.
pub fn read(path: impl AsRef<Path>) -> io::Result<Vec<u8>> {
    let path = path.as_ref();
    match active() {
        None => fs::read(path),
        Some(recorder) => recorder.read(path),
    }
}

/// Which step of [`read_staged`] failed.
#[derive(Debug)]
pub enum ReadError {
    /// Opening the file.
    Open(io::Error),
    /// Reading its contents.
    Read(io::Error),
}

impl ReadError {
    pub fn into_inner(self) -> io::Error {
        match self {
            Self::Open(error) | Self::Read(error) => error,
        }
    }
}

/// `File::open` then `read_to_end` (no size hint), with the failing step
/// kept apart for callers whose messages distinguish them; recording READ.
pub fn read_staged(path: impl AsRef<Path>) -> Result<Vec<u8>, ReadError> {
    let path = path.as_ref();
    match active() {
        None => {
            let mut file = fs::File::open(path).map_err(ReadError::Open)?;
            let mut bytes = Vec::new();
            file.read_to_end(&mut bytes).map_err(ReadError::Read)?;
            Ok(bytes)
        }
        Some(recorder) => recorder.read_staged(path),
    }
}

/// [`fs::read_to_string`], recording READ (of the bytes, also when they are
/// not UTF-8: the same bytes give the same error).
pub fn read_to_string(path: impl AsRef<Path>) -> io::Result<String> {
    let path = path.as_ref();
    match active() {
        None => fs::read_to_string(path),
        Some(recorder) => String::from_utf8(recorder.read(path)?).map_err(|_| {
            // std's io::Error::INVALID_UTF8 (default_read_to_string).
            io::Error::new(
                io::ErrorKind::InvalidData,
                "stream did not contain valid UTF-8",
            )
        }),
    }
}

/// The SHA-256 of a file's contents, streamed (constant memory), recording
/// READ.
pub fn sha256_file(path: impl AsRef<Path>) -> io::Result<[u8; 32]> {
    let path = path.as_ref();
    match active() {
        None => {
            let mut digest = Sha256::new();
            io::copy(&mut fs::File::open(path)?, &mut digest)?;
            Ok(digest.finalize().into())
        }
        Some(recorder) => recorder.sha256_file(path),
    }
}

/// [`fs::metadata`] (following symlinks), recording STAT (the type class).
pub fn metadata(path: impl AsRef<Path>) -> io::Result<fs::Metadata> {
    let path = path.as_ref();
    let result = fs::metadata(path);
    if let Some(recorder) = active() {
        let outcome = match &result {
            Ok(metadata) => Outcome::Kind(FileKind::of(metadata.file_type())),
            Err(error) => Outcome::Error(errno(error)),
        };
        recorder.observe(path, Op::Stat, outcome);
    }
    result
}

/// [`fs::symlink_metadata`], recording LSTAT (the type class; a symlink's
/// target).
pub fn symlink_metadata(path: impl AsRef<Path>) -> io::Result<fs::Metadata> {
    let path = path.as_ref();
    let result = fs::symlink_metadata(path);
    if let Some(recorder) = active() {
        recorder.observe_lstat(path, &result);
    }
    result
}

/// [`Path::exists`] (following symlinks), recording STAT.
pub fn exists(path: impl AsRef<Path>) -> bool {
    metadata(path).is_ok()
}

/// [`Path::is_file`] (following symlinks), recording STAT.
pub fn is_file(path: impl AsRef<Path>) -> bool {
    metadata(path).is_ok_and(|metadata| metadata.is_file())
}

/// [`Path::is_dir`] (following symlinks), recording STAT.
pub fn is_dir(path: impl AsRef<Path>) -> bool {
    metadata(path).is_ok_and(|metadata| metadata.is_dir())
}

/// [`fs::canonicalize`], recording CANON.
pub fn canonicalize(path: impl AsRef<Path>) -> io::Result<PathBuf> {
    let path = path.as_ref();
    let result = fs::canonicalize(path);
    if let Some(recorder) = active() {
        let outcome = match &result {
            Ok(resolved) => Outcome::Path(resolved.clone()),
            Err(error) => Outcome::Error(errno(error)),
        };
        recorder.observe(path, Op::Canon, outcome);
    }
    result
}

/// [`fs::read_link`], recording READLINK.
pub fn read_link(path: impl AsRef<Path>) -> io::Result<PathBuf> {
    let path = path.as_ref();
    let result = fs::read_link(path);
    if let Some(recorder) = active() {
        let outcome = match &result {
            Ok(target) => Outcome::Path(target.clone()),
            Err(error) => Outcome::Error(errno(error)),
        };
        recorder.observe(path, Op::ReadLink, outcome);
    }
    result
}

/// [`fs::read_dir`] collected in directory order, with every entry's error
/// kept in place (callers report the first error exactly where they did
/// while iterating). While recording, only the entries `projection` keeps
/// are returned and recorded (READDIR with the projected digest); outside the
/// window every entry is returned — callers skip what the projection drops.
pub fn read_dir_projected(
    directory: impl AsRef<Path>,
    projection: &Projection,
) -> io::Result<Vec<io::Result<fs::DirEntry>>> {
    let directory = directory.as_ref();
    match active() {
        None => Ok(fs::read_dir(directory)?.collect()),
        Some(recorder) => recorder.read_dir_projected(directory, projection),
    }
}

/// [`std::env::var_os`], recording ENV (a digest of the value).
pub fn env_var(name: impl AsRef<OsStr>) -> Option<OsString> {
    let name = name.as_ref();
    let value = std::env::var_os(name);
    if let Some(recorder) = active() {
        recorder.observe_env(name, value.as_deref());
    }
    value
}

// ---- semantic records ------------------------------------------------------

/// Record that `bytes` were read from `path` by code that must keep its own
/// reader (e.g. the repository store's no-follow, change-checked reads): READ
/// with the key of an lstat taken now. A change between that read and this
/// lstat is within the window, hence racy and content-checked on validation.
pub fn note_read(path: &Path, bytes: &[u8]) {
    let Some(recorder) = active() else {
        return;
    };
    let key = fs::symlink_metadata(path)
        .ok()
        .map(|metadata| StatKey::of(&metadata))
        .unwrap_or_default();
    recorder.observe(
        path,
        Op::Read,
        Outcome::Read {
            sha256: sha256(bytes),
            key,
            racy: recorder.racy(&key),
        },
    );
}

/// Record an environment value the repository environment observed (ENV,
/// digest only; crate::repository_environment).
pub fn note_env(name: &OsStr, value: Option<&OsStr>) {
    if let Some(recorder) = active() {
        recorder.observe_env(name, value);
    }
}

/// Record a taint. `detail` is only formatted while recording.
pub fn taint(kind: Taint, detail: impl FnOnce() -> String) {
    if let Some(recorder) = active() {
        recorder.taint(kind, detail());
    }
}

/// Record a taint even after the window closed (end-of-command conditions:
/// errors, a pending lockfile write).
pub fn taint_after_window(kind: Taint, detail: impl FnOnce() -> String) {
    if let Some(recorder) = RECORDER.get() {
        recorder.taint(kind, detail());
    }
}

/// While recording, brackets work that taints the run (see
/// [`MARKER_TAINTED_BEGIN`]) until the guard drops; nothing otherwise.
pub fn tainted_section() -> TaintedSection {
    let active = recording();
    if active {
        let _ = fs::symlink_metadata(MARKER_TAINTED_BEGIN);
    }
    TaintedSection(active)
}

/// The guard of [`tainted_section`].
pub struct TaintedSection(bool);

impl Drop for TaintedSection {
    fn drop(&mut self) {
        if self.0 {
            let _ = fs::symlink_metadata(MARKER_TAINTED_END);
        }
    }
}

/// Record a repository tree the run verified (REPO + INTEGRITY): the lstat
/// key of every entry below `root` (and of the manifest), walked now. Call
/// it after the tree's own verification (and fingerprint refresh)
/// succeeded; once per root and window.
pub fn repository_tree(kind: &'static str, name: &str, root: &Path, manifest: Option<&Path>) {
    let Some(recorder) = active() else {
        return;
    };
    if !lock(&recorder.repository_roots).insert(root.to_path_buf()) {
        return;
    }
    let start = recorder.window_start_ns;
    let entry = |path: PathBuf, state: &io::Result<fs::Metadata>| {
        let state = match state {
            Ok(metadata) => Ok((FileKind::of(metadata.file_type()), StatKey::of(metadata))),
            Err(error) => Err(errno(error)),
        };
        let racy = matches!(&state, Ok((_, key)) if key.racy(start));
        IntegrityEntry { path, state, racy }
    };
    let mut entries = Vec::new();
    if let Some(manifest) = manifest {
        entries.push(entry(
            manifest.to_path_buf(),
            &fs::symlink_metadata(manifest),
        ));
    }
    let mut pending = vec![root.to_path_buf()];
    while let Some(path) = pending.pop() {
        let state = fs::symlink_metadata(&path);
        let directory = state.as_ref().is_ok_and(|metadata| metadata.is_dir());
        entries.push(entry(path.clone(), &state));
        if !directory {
            continue;
        }
        match fs::read_dir(&path) {
            Ok(children) => {
                for child in children {
                    match child {
                        Ok(child) => pending.push(child.path()),
                        Err(error) => recorder.taint(
                            Taint::Unstable,
                            format!("INTEGRITY {}: {error}", escape(path.as_os_str())),
                        ),
                    }
                }
            }
            Err(error) => recorder.taint(
                Taint::Unstable,
                format!("INTEGRITY {}: {error}", escape(path.as_os_str())),
            ),
        }
    }
    entries.sort_by(|a, b| a.path.cmp(&b.path));
    lock(&recorder.repositories).push(RepositoryRecord {
        kind,
        name: name.to_owned(),
        root: root.to_path_buf(),
        manifest: manifest.map(Path::to_path_buf),
        entries,
    });
}

// ---- text helpers ----------------------------------------------------------

pub fn hex(bytes: &[u8]) -> String {
    let mut text = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        use std::fmt::Write as _;
        let _ = write!(text, "{byte:02x}");
    }
    text
}

/// A path or name as one dump token: printable ASCII except `\` stays,
/// everything else (spaces, controls, non-ASCII and non-UTF-8 bytes) is
/// `\xNN`. Reversible (scripts/snapshot_fs_audit.py decodes it).
pub fn escape(text: &OsStr) -> String {
    let mut out = String::with_capacity(text.len());
    for &byte in text.as_bytes() {
        if byte.is_ascii_graphic() && byte != b'\\' {
            out.push(byte as char);
        } else {
            use std::fmt::Write as _;
            let _ = write!(out, "\\x{byte:02x}");
        }
    }
    out
}

fn escape_str(text: &str) -> String {
    escape(OsStr::new(text))
}

/// Write one diagnostics line (`bazel-rs-snapshot: ...`) to stderr. Snapshot
/// diagnostics only appear when `BAZEL_RS_SNAPSHOT` is set; the prefix lets
/// exact-stderr checks drop them (like `PERF` lines).
pub fn diag(line: &str) {
    let mut stderr = io::stderr().lock();
    let _ = writeln!(stderr, "bazel-rs-snapshot: {line}");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn invalid_utf8_error_matches_std() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("bad");
        fs::write(&path, [0xff, 0xfe]).unwrap();
        let std_error = fs::read_to_string(&path).unwrap_err();
        let recorder = Recorder::new(now_ns(), BTreeSet::new());
        let bytes = recorder.read(&path).unwrap();
        let ours = String::from_utf8(bytes)
            .map_err(|_| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    "stream did not contain valid UTF-8",
                )
            })
            .unwrap_err();
        assert_eq!(ours.kind(), std_error.kind());
        assert_eq!(ours.to_string(), std_error.to_string());
    }

    #[test]
    fn recorder_reads_like_std_and_records_outcomes() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        fs::write(root.join("a"), b"alpha").unwrap();
        fs::create_dir(root.join("dir")).unwrap();
        fs::write(root.join("dir/f.go"), b"x").unwrap();
        fs::create_dir(root.join("dir/sub")).unwrap();
        std::os::unix::fs::symlink("a", root.join("link")).unwrap();
        let recorder = Recorder::new(now_ns(), BTreeSet::new());
        assert_eq!(recorder.read(&root.join("a")).unwrap(), b"alpha");
        assert_eq!(recorder.read(&root.join("link")).unwrap(), b"alpha");
        let missing = recorder.read(&root.join("missing")).unwrap_err();
        assert_eq!(
            missing.to_string(),
            fs::read(root.join("missing")).unwrap_err().to_string()
        );
        let listed = recorder
            .read_dir_projected(&root.join("dir"), &Projection::Directories)
            .unwrap();
        let names: Vec<_> = listed
            .into_iter()
            .map(|entry| entry.unwrap().file_name())
            .collect();
        assert_eq!(names, vec![OsString::from("sub")]);
        let recording = recorder.snapshot();
        let ops: Vec<(String, String)> = recording
            .observations
            .iter()
            .map(|(path, observation)| {
                (
                    path.strip_prefix(root).unwrap().display().to_string(),
                    observation.op.name().to_owned(),
                )
            })
            .collect();
        assert_eq!(
            ops,
            [
                ("a", "READ"),
                ("dir", "READDIR"),
                ("link", "READ"),
                ("link", "LSTAT"),
                ("missing", "READ"),
            ]
            .map(|(a, b)| (a.to_owned(), b.to_owned()))
        );
        // Freshly written files are racy.
        assert!(matches!(
            recording.observations[0].1.outcome,
            Outcome::Read { racy: true, .. }
        ));
        assert!(recording.taints.is_empty(), "{:?}", recording.taints);
    }

    #[test]
    fn conflicting_outcomes_are_unstable() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("f");
        let recorder = Recorder::new(now_ns(), BTreeSet::new());
        recorder.observe(&path, Op::Stat, Outcome::Error(2));
        recorder.observe(&path, Op::Stat, Outcome::Error(2));
        assert!(recorder.snapshot().taints.is_empty());
        recorder.observe(&path, Op::Stat, Outcome::Kind(FileKind::File));
        let taints = recorder.snapshot().taints;
        assert_eq!(taints[&Taint::Unstable].count, 1);
    }

    #[test]
    fn racy_rule_uses_the_later_of_mtime_and_ctime() {
        let key = StatKey {
            mtime_ns: 10 * 1_000_000_000,
            ctime_ns: 20 * 1_000_000_000,
            ..StatKey::default()
        };
        assert!(key.racy(21 * 1_000_000_000));
        assert!(!key.racy(22 * 1_000_000_000));
        assert!(key.racy(5 * 1_000_000_000), "future-dated is racy");
    }

    #[test]
    fn escape_is_reversible_text() {
        assert_eq!(escape(OsStr::new("a b\\c")), "a\\x20b\\x5cc");
        assert_eq!(
            escape(OsStr::from_bytes(&[b'x', 0xff])),
            "x\\xff".to_owned()
        );
    }
}
