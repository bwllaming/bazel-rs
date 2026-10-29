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
//!
//! **Frames (W6R).** Phase-1 prefetch and pattern-loading workers load
//! packages the build may never commit, and fill invocation-wide memos the
//! coordinator hits later. What they observe must reach the recording
//! exactly where the sequential engine (one loading thread) would have
//! observed it, never earlier and never not at all. So a thread records
//! into the innermost open frame instead of the recorder:
//! * a load unit's frame ([`unit_scope`]) is committed by the coordinator
//!   with the unit ([`commit`]) and dropped with an unused or aborted one;
//! * an attached capture ([`capture`]) keeps what a memo's miss observed
//!   with the memo entry ([`Captured`]) and hands it to the enclosing frame
//!   (or the recorder); every hit of the entry replays it ([`replay`]);
//! * a detached capture ([`capture_detached`]) keeps a warm-reuse probe's
//!   observations with its publication only, replayed when the published
//!   root begins to count for the sequential engine;
//! * a discard frame ([`discard`]) drops what a coordinator-side hint
//!   observes.
//!
//! The recorder is a set keyed by (path, op): the first outcome wins and a
//! second one makes the run Unstable, so neither the order nor the time of
//! a merge matters — only which events are merged. A rule follows: a
//! memo a worker can fill and whose hit skips the miss path's
//! observations must carry a [`Captured`] (docs/ANALYSIS_SNAPSHOT_RECORDING.md,
//! "Recording with phase-1 prefetch").

use sha2::{Digest, Sha256};
use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::ffi::{OsStr, OsString};
use std::fmt;
use std::fs;
use std::hash::{Hash, Hasher};
use std::io::{self, Read};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock, PoisonError};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// The racy window (`digest_memo::RACY_MARGIN`, `repository_store::RACY_MARGIN`):
/// a file whose mtime or ctime is younger than this relative to the window
/// start could still be rewritten without changing its stat key, so it is
/// re-validated by content until a later check settles it.
pub const RACY_MARGIN_NS: i64 = 2_000_000_000;

/// Marker paths stat'ed when the window opens and closes, so a syscall trace
/// can bracket the window (scripts/snapshot_fs_audit.py). They never exist.
pub const MARKER_OPEN: &str = "/.bazel-rs-snapshot-marker/window-open";
pub const MARKER_CLOSE: &str = "/.bazel-rs-snapshot-marker/window-close";
/// Marker paths bracketing, on the executing thread, work whose filesystem
/// accesses need no record because it taints the run anyway, is replayed
/// and compared instead (a repository rule or module extension executing,
/// [`tainted_section`]; local repository rules, crate::snapshot::replay),
/// or — a non-local repository rule whose publication the warm probe
/// re-reads after it (crate::repository_store, [`reprobes_executions`]) —
/// stands as the reuse of that publication the next run would make: the
/// syscall audit skips that thread's calls between them.
pub const MARKER_TAINTED_BEGIN: &str = "/.bazel-rs-snapshot-marker/tainted-begin";
pub const MARKER_TAINTED_END: &str = "/.bazel-rs-snapshot-marker/tainted-end";

const SHARDS: usize = 64;
/// Distinct details kept per taint kind (the dump lists them all; the
/// stderr summary the first few).
const TAINT_DETAILS: usize = 1000;

static RECORDING: AtomicBool = AtomicBool::new(false);
static RECORDER: OnceLock<Recorder> = OnceLock::new();
/// The window is a verify shadow ([`mark_shadow_window`]).
static SHADOW: AtomicBool = AtomicBool::new(false);

thread_local! {
    static PRIVATE_RECORDING: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// A thread-private proof recording. A stateless facade routes every event to
/// this thread's frame; its lifetime is static, but no proof data is retained
/// globally. Nested memo captures stay inside the private frame. No worker may
/// be spawned by the proof closure.
pub(crate) fn record_private<R>(f: impl FnOnce() -> R) -> (R, Recording) {
    struct Scope(bool);
    impl Drop for Scope {
        fn drop(&mut self) {
            PRIVATE_RECORDING.with(|flag| flag.set(self.0));
        }
    }
    let scope = Scope(PRIVATE_RECORDING.with(|flag| flag.replace(true)));
    let frame = FrameGuard::open().expect("private recording is active");
    let value = f();
    let events = frame.close();
    drop(scope);
    let recorder = Recorder::new(0);
    recorder.apply_events(events);
    let mut recording = recorder.take();
    // The facade has no per-proof start time. Revalidate all content, including
    // memo answers, rather than granting any stat-only trust to a proof.
    for (_, observation) in &mut recording.observations {
        match &mut observation.outcome {
            Outcome::Read { racy, .. } | Outcome::Listing { racy, .. } => *racy = true,
            _ => {}
        }
    }
    for repository in &mut recording.repositories {
        for entry in &mut repository.entries {
            entry.racy = true;
        }
    }
    (value, recording)
}

#[cfg(test)]
thread_local! {
    /// Unit tests: a window of this thread only ([`tests_support::record_this_thread`]),
    /// so a test records without opening the process-global window other
    /// tests would see.
    static THREAD_RECORDER: std::cell::Cell<Option<&'static Recorder>> =
        const { std::cell::Cell::new(None) };
}

/// The recorder while the window is open.
#[inline]
fn active() -> Option<&'static Recorder> {
    if PRIVATE_RECORDING.with(std::cell::Cell::get) {
        static FACADE: OnceLock<Recorder> = OnceLock::new();
        return Some(FACADE.get_or_init(|| {
            let mut recorder = Recorder::new(0);
            recorder.framed = true;
            recorder
        }));
    }
    #[cfg(test)]
    if let Some(recorder) = THREAD_RECORDER.with(std::cell::Cell::get) {
        return Some(recorder);
    }
    if RECORDING.load(Ordering::Relaxed) {
        RECORDER.get()
    } else {
        None
    }
}

/// The window is the shadow of a snapshot hit (crate::snapshot::verify): the
/// full analysis is compared with the hit, so a repository rule it executes
/// is a difference in itself (the hit executed none) and always taints —
/// never re-probed ([`reprobes_executions`]).
pub fn mark_shadow_window() {
    SHADOW.store(true, Ordering::SeqCst);
}

/// Whether a non-local repository rule executed now may be re-probed instead
/// of tainting the run (crate::repository_store::materialize_lazy): the
/// window is open and may be saved (not a verify shadow). The warm probe
/// then reads the publication back exactly as the next invocation's would —
/// the specification pointer, the manifest's recorded inputs, the REPO tree
/// — so a snapshot saved from this run validates what that invocation's
/// full analysis would reuse.
pub fn reprobes_executions() -> bool {
    active().is_some() && !SHADOW.load(Ordering::Relaxed)
}

/// Whether the window's recorder already holds a taint that keeps the run
/// from being saved, other than `except` (events still in a frame of some
/// thread are not seen: an optimisation only — it saves a re-probe that
/// could not make the run saveable).
pub fn tainted_otherwise(except: Taint) -> bool {
    active().is_some_and(|recorder| {
        lock(&recorder.taints)
            .keys()
            .any(|taint| *taint != except && !taint.plan_dependent())
    })
}

/// Forget what the window recorded for `path` (every operation): a store
/// file this process itself just replaced — a repository's specification
/// pointer, re-read by the warm probe after a publication
/// (crate::repository_store) — whose earlier outcome described the state
/// before the replacement. Ordered like every other event: inside a frame
/// it applies at the frame's commit, after the frame's earlier events (an
/// earlier outcome kept in a captured child set that is replayed later
/// still conflicts with the new one: Unstable, never saved).
pub fn supersede(path: &Path) {
    if let Some(recorder) = active() {
        if recorder.redirected() {
            push_event(Event::Supersede(path.to_path_buf()));
        } else {
            recorder.store_supersede(path);
        }
    }
}

/// Whether the recording window is open.
#[inline]
pub fn recording() -> bool {
    if PRIVATE_RECORDING.with(std::cell::Cell::get) {
        return true;
    }
    #[cfg(test)]
    if THREAD_RECORDER.with(std::cell::Cell::get).is_some() {
        return true;
    }
    RECORDING.load(Ordering::Relaxed)
}

pub(crate) fn current_recording() -> Option<Recording> {
    active().map(Recorder::snapshot)
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
    /// Read owned by one repository selection, not an ordinary file consumer.
    /// Only a current proof of this canonical selection can replace its guard.
    SelectionRead(String),
    /// The shared pointer was absent before selecting a workspace pointer.
    SelectionAbsent(String),
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
            Self::SelectionRead(_) => "SELECTION_READ",
            Self::SelectionAbsent(_) => "SELECTION_ABSENT",
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

/// One environment variable read during the window, as a digest: the value
/// of the command's repository environment (the client environment with
/// `--repo_env` applied, crate::repo_env — the only environment the window
/// reads, crate::repository_environment), which a hit validates against the
/// repository environment of its own command line
/// (crate::snapshot::validate::Environment; L2-W5f review F1: an option
/// that sets a name does not fix it — a later `--repo_env=NAME` inherits
/// the client's value again).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EnvObservation {
    /// SHA-256 of the value's bytes; `None` when unset.
    pub digest: Option<[u8; 32]>,
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
    /// fetch or a changed specification) and the warm probe run again after
    /// its publication did not reuse it, or it printed
    /// (crate::repository_store's `ExecutedTaint`, [`reprobes_executions`]).
    /// Local rules always execute; they are replayed and compared instead
    /// (crate::snapshot::replay).
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
    /// The `--workspace_status_command` wrote to stderr while planning
    /// computed the status: a snapshot hit replays the window's stderr and
    /// computes the status again (L2-W5), so it would print twice.
    WorkspaceStatusStderr,
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
            Self::WorkspaceStatusStderr => "workspace-status-stderr",
            Self::Errors => "errors",
            Self::Unstable => "unstable",
            Self::TestHook => "test-hook",
        }
    }

    /// Recorded when an action is registered during analysis, which the
    /// plan may still prune (it keeps only the actions requested outputs
    /// need): an upper bound until the planned request itself is checked
    /// ([`settle_plan_taints`], L2-W4), which keeps only the plan's own.
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

/// Who recorded a repository root (see [`repository_tree`],
/// [`crossed_tree`], [`local_tree`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Claim {
    /// The repository that owns the tree verified it (a reused
    /// publication): its REPO record is final.
    Owner,
    /// A link crossed into a published tree no root of the invocation
    /// named yet: its REPO record stands until an owner claims the tree.
    Crossing,
    /// A local repository rule published the tree: replayed, never a REPO.
    Local,
}

/// The process-global recorder of one window.
pub struct Recorder {
    window_start_ns: i64,
    started: Instant,
    /// Observations made inside an open frame of the observing thread go
    /// to the frame (the window's recorder; see the module docs).
    framed: bool,
    shards: Vec<Shard>,
    env: Mutex<BTreeMap<OsString, EnvObservation>>,
    repositories: Mutex<Vec<RepositoryRecord>>,
    selections: Mutex<BTreeMap<String, crate::repository_publication::Receipt>>,
    repository_roots: Mutex<HashMap<PathBuf, Claim>>,
    taints: Mutex<BTreeMap<Taint, TaintRecord>>,
    /// The plan-dependent taints analysis registered, once the planned
    /// request decided them ([`settle_plan_taints`]).
    plan_registered: Mutex<Option<BTreeMap<Taint, TaintRecord>>>,
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
    /// Advisory non-local selection receipts. Raw pointer dependencies remain
    /// mandatory until the guarded historical-adoption protocol is enabled.
    pub selections: Vec<crate::repository_publication::Receipt>,
    /// The roots of `repositories` only a link crossing recorded: no
    /// repository of the run claimed them (sorted).
    pub crossed: Vec<PathBuf>,
    pub taints: BTreeMap<Taint, TaintRecord>,
    /// What analysis registered of the plan-dependent taints, once the
    /// planned request decided them ([`settle_plan_taints`]: `taints` then
    /// holds only the plan's own); `None` while undecided.
    pub plan_registered: Option<BTreeMap<Taint, TaintRecord>>,
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
    fn new(window_start_ns: i64) -> Self {
        Self {
            window_start_ns,
            started: Instant::now(),
            framed: false,
            shards: (0..SHARDS).map(|_| Mutex::default()).collect(),
            env: Mutex::default(),
            repositories: Mutex::default(),
            selections: Mutex::default(),
            repository_roots: Mutex::default(),
            taints: Mutex::default(),
            plan_registered: Mutex::default(),
            window: Mutex::default(),
        }
    }

    /// A recorder whose observations go to the observing thread's open
    /// frame, if any (the window's; see the module docs).
    fn framed(window_start_ns: i64) -> Self {
        Self {
            framed: true,
            ..Self::new(window_start_ns)
        }
    }

    /// Whether an event observed now goes to a frame of this thread
    /// instead of the recorder ([`Self::observe`] and its siblings). A
    /// loading worker (no-demand mode) only ever records inside its load
    /// unit's frame (crate::package::evaluate_load_unit) or a capture in
    /// it; so does the coordinator under no-demand (a discarded hint, the
    /// `BAZEL_RS_LOADING_NO_DEMAND` test hook's units).
    fn redirected(&self) -> bool {
        if !self.framed {
            return false;
        }
        let framed = in_frame();
        debug_assert!(
            framed || !crate::repository_demand::no_demand_active() || !hook(Hook::Unit),
            "a no-demand thread recorded outside a frame"
        );
        framed
    }

    fn observe(&self, path: &Path, op: Op, outcome: Outcome) {
        if self.redirected() {
            push_event(Event::Observe(path.to_path_buf(), op, outcome));
            return;
        }
        self.store_observation(path, op, outcome);
    }

    /// [`Self::observe`] into the recorder itself.
    ///
    /// `Path` equality ignores a trailing separator (`root.join("")` of an
    /// empty package spells `root/`), so two spellings of one path share
    /// an entry: it keeps the byte-wise smallest spelling seen, whatever
    /// order the observations come in (W6R: workers' units reach the
    /// recorder in another order than one loading thread's loads).
    fn store_observation(&self, path: &Path, op: Op, outcome: Outcome) {
        let mut shard = lock(&self.shards[shard_of(path)]);
        if let Some((spelled, _)) = shard.get_key_value(path)
            && path.as_os_str().as_bytes() < spelled.as_os_str().as_bytes()
            && let Some((_, observations)) = shard.remove_entry(path)
        {
            shard.insert(path.to_path_buf(), observations);
        }
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
            self.store_taint(Taint::Unstable, conflict);
        }
    }

    /// The outcome recorded so far for `op` of `path` (frames not yet
    /// committed aside): incremental analysis checks that what changed
    /// was observed again (crate::snapshot::incremental).
    pub fn observed(&self, path: &Path, op: &Op) -> Option<Outcome> {
        lock(&self.shards[shard_of(path)])
            .get(path)?
            .iter()
            .find(|seen| seen.op == *op)
            .map(|seen| seen.outcome.clone())
    }

    /// [`supersede`] in the recorder itself.
    fn store_supersede(&self, path: &Path) {
        lock(&self.shards[shard_of(path)]).remove(path);
    }

    fn store_supersede_selection(&self, path: &Path, canonical: &str) {
        if let Some(observations) = lock(&self.shards[shard_of(path)]).get_mut(path) {
            observations.retain(|observation| {
                !matches!(&observation.op,
                Op::SelectionRead(owner) | Op::SelectionAbsent(owner) if owner == canonical)
            });
        }
    }

    fn taint(&self, kind: Taint, detail: String) {
        if self.redirected() {
            push_event(Event::Taint(kind, detail));
            return;
        }
        self.store_taint(kind, detail);
    }

    /// [`Self::taint`] into the recorder itself.
    fn store_taint(&self, kind: Taint, detail: String) {
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

    /// See [`settle_plan_taints`].
    fn settle_plan(&self, blockers: impl IntoIterator<Item = (Taint, String)>) {
        let mut taints = lock(&self.taints);
        let kinds: Vec<Taint> = taints
            .keys()
            .copied()
            .filter(|kind| kind.plan_dependent())
            .collect();
        let mut registered = lock(&self.plan_registered);
        let registered = registered.get_or_insert_with(BTreeMap::new);
        for kind in kinds {
            if let Some(record) = taints.remove(&kind) {
                let into = registered.entry(kind).or_default();
                into.count += record.count;
                for detail in record.details {
                    if into.details.len() < TAINT_DETAILS && !into.details.contains(&detail) {
                        into.details.push(detail);
                    }
                }
            }
        }
        drop(taints);
        for (kind, detail) in blockers {
            self.store_taint(kind, detail);
        }
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

    /// [`note_read_digest`]: READ with the key `opened` (the stat the
    /// digest memo's lookup used), and — as in [`Recorder::key_before`] —
    /// the LSTAT of a symlink at `path`.
    fn read_digest(&self, path: &Path, opened: &fs::Metadata, sha256: [u8; 32]) {
        let lstat = fs::symlink_metadata(path);
        if lstat
            .as_ref()
            .is_ok_and(|metadata| metadata.file_type().is_symlink())
        {
            self.observe_lstat(path, &lstat);
        }
        let key = StatKey::of(opened);
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
        self.observe_env_digest(name, value.map(|value| sha256(value.as_bytes())));
    }

    /// [`Self::observe_env`] of a value already digested.
    fn observe_env_digest(&self, name: &OsStr, digest: Option<[u8; 32]>) {
        let observation = EnvObservation { digest };
        if self.redirected() {
            push_event(Event::Env(name.to_owned(), observation));
            return;
        }
        self.store_env(name, observation);
    }

    /// [`Self::observe_env_digest`] into the recorder itself.
    fn store_env(&self, name: &OsStr, observation: EnvObservation) {
        let mut env = lock(&self.env);
        match env.get(name) {
            Some(seen) if *seen == observation => {}
            Some(_) => {
                drop(env);
                self.store_taint(
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
        self.restat_manifests(&mut repositories);
        let mut crossed: Vec<PathBuf> = lock(&self.repository_roots)
            .iter()
            .filter(|(_, claim)| **claim == Claim::Crossing)
            .map(|(root, _)| root.clone())
            .collect();
        crossed.sort();
        Recording {
            window_start_ns: self.window_start_ns,
            window: *lock(&self.window),
            observations,
            env: lock(&self.env)
                .iter()
                .map(|(name, observation)| (name.clone(), observation.clone()))
                .collect(),
            repositories,
            selections: lock(&self.selections).values().cloned().collect(),
            crossed,
            taints: lock(&self.taints).clone(),
            plan_registered: lock(&self.plan_registered).clone(),
        }
    }

    /// The taints recorded so far.
    pub fn taints(&self) -> BTreeMap<Taint, TaintRecord> {
        lock(&self.taints).clone()
    }

    /// Everything recorded so far, sorted, moved out of the recorder (no
    /// copy of the paths: the analysis snapshot's save after the window
    /// closed, crate::snapshot::save; later calls see only what was recorded
    /// afterwards).
    pub fn take(&self) -> Recording {
        let mut observations = Vec::new();
        for shard in &self.shards {
            let entries = std::mem::take(&mut *lock(shard));
            for (path, mut list) in entries {
                // The last observation of the path takes it (sorted below).
                let last = list.pop();
                for observation in list {
                    observations.push((path.clone(), observation));
                }
                if let Some(observation) = last {
                    observations.push((path, observation));
                }
            }
        }
        // By the paths' bytes (`Path::cmp` parses components; consumers need
        // a deterministic order with a path's observations together).
        observations.sort_by(|(a, x), (b, y)| {
            a.as_os_str()
                .as_bytes()
                .cmp(b.as_os_str().as_bytes())
                .then_with(|| x.op.cmp(&y.op))
        });
        let mut repositories = std::mem::take(&mut *lock(&self.repositories));
        repositories.sort_by(|a, b| a.root.cmp(&b.root));
        self.restat_manifests(&mut repositories);
        let mut crossed: Vec<PathBuf> = lock(&self.repository_roots)
            .iter()
            .filter(|(_, claim)| **claim == Claim::Crossing)
            .map(|(root, _)| root.clone())
            .collect();
        crossed.sort();
        Recording {
            window_start_ns: self.window_start_ns,
            window: *lock(&self.window),
            observations,
            env: lock(&self.env)
                .iter()
                .map(|(name, observation)| (name.clone(), observation.clone()))
                .collect(),
            repositories,
            selections: std::mem::take(&mut *lock(&self.selections))
                .into_values()
                .collect(),
            crossed,
            taints: lock(&self.taints).clone(),
            plan_registered: lock(&self.plan_registered).clone(),
        }
    }

    /// The REPO + INTEGRITY record of `root`: the lstat key of every entry
    /// below it (and of the manifest), walked now.
    fn walk_tree(
        &self,
        kind: &'static str,
        name: &str,
        root: &Path,
        manifest: Option<&Path>,
    ) -> RepositoryRecord {
        let start = self.window_start_ns;
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
                            Err(error) => self.taint(
                                Taint::Unstable,
                                format!("INTEGRITY {}: {error}", escape(path.as_os_str())),
                            ),
                        }
                    }
                }
                Err(error) => self.taint(
                    Taint::Unstable,
                    format!("INTEGRITY {}: {error}", escape(path.as_os_str())),
                ),
            }
        }
        entries.sort_by(|a, b| a.path.cmp(&b.path));
        RepositoryRecord {
            kind,
            name: name.to_owned(),
            root: root.to_path_buf(),
            manifest: manifest.map(Path::to_path_buf),
            entries,
        }
    }

    /// Record `root` for `claim` (see [`Claim`]): an owner's record
    /// replaces a crossing's (walked again: the owner may have refreshed
    /// the manifest), so the recording does not depend on whether the link
    /// or the repository came first; a local claim drops a crossing's. The
    /// walk runs unlocked; a claim superseded meanwhile records nothing.
    ///
    /// The manifest's entry always holds its latest key: two verifications
    /// of one publication may race (workers' warm probes,
    /// crate::repository_demand::get_or_probe), and the later one's
    /// fingerprint refresh rewrites the manifest after the first claim
    /// walked it — the manifest is stat'ed again when the record is stored
    /// and on every later owner claim (each verification claims after its
    /// refresh), so a hit never compares against a key the window replaced.
    ///
    /// Inside a frame the claim is an event: the tree is walked here, on
    /// the observing thread (unless the recorder already has the tree's
    /// owner, whose record a later claim only restats), and the claim is
    /// decided when the event reaches the recorder ([`Self::apply_claim`]).
    fn claim_tree(
        &self,
        claim: Claim,
        kind: &'static str,
        name: &str,
        root: &Path,
        manifest: Option<&Path>,
    ) {
        if self.redirected() {
            let owned = lock(&self.repository_roots).get(root) == Some(&Claim::Owner);
            let record = (claim != Claim::Local && !owned)
                .then(|| self.walk_tree(kind, name, root, manifest));
            push_event(Event::Claim(ClaimEvent {
                claim,
                kind,
                name: name.to_owned(),
                root: root.to_path_buf(),
                manifest: manifest.map(Path::to_path_buf),
                record,
            }));
            return;
        }
        self.store_claim(claim, kind, name, root, manifest);
    }

    /// [`Self::claim_tree`] into the recorder itself.
    fn store_claim(
        &self,
        claim: Claim,
        kind: &'static str,
        name: &str,
        root: &Path,
        manifest: Option<&Path>,
    ) {
        {
            let mut roots = lock(&self.repository_roots);
            match (roots.get(root), claim) {
                (None, _) | (Some(Claim::Crossing), Claim::Owner | Claim::Local) => {}
                (Some(Claim::Owner), Claim::Owner) => {
                    let mut repositories = lock(&self.repositories);
                    if let Some(record) = repositories
                        .iter_mut()
                        .find(|repository| repository.root == root)
                    {
                        restat_manifest(record, self.window_start_ns);
                    }
                    return;
                }
                (Some(_), _) => return,
            }
            roots.insert(root.to_path_buf(), claim);
        }
        let record = (claim != Claim::Local).then(|| self.walk_tree(kind, name, root, manifest));
        let roots = lock(&self.repository_roots);
        if roots.get(root) != Some(&claim) {
            return;
        }
        let mut repositories = lock(&self.repositories);
        repositories.retain(|repository| repository.root != root);
        if let Some(mut record) = record {
            restat_manifest(&mut record, self.window_start_ns);
            repositories.push(record);
        }
    }

    /// A claim event reaching the recorder: [`Self::store_claim`] with the
    /// record walked when the claim was made.
    fn apply_claim(&self, event: ClaimEvent) {
        let ClaimEvent {
            claim,
            kind,
            name,
            root,
            manifest,
            record,
        } = event;
        if record.is_none() && claim != Claim::Local {
            // Walked by nobody: the recorder had the owner when the claim
            // was made (an owner's claim is never replaced, so this is the
            // owner-again case), or unexpectedly not — then walk now.
            self.store_claim(claim, kind, &name, &root, manifest.as_deref());
            return;
        }
        let mut roots = lock(&self.repository_roots);
        match (roots.get(&root), claim) {
            (None, _) | (Some(Claim::Crossing), Claim::Owner | Claim::Local) => {}
            (Some(Claim::Owner), Claim::Owner) => {
                let mut repositories = lock(&self.repositories);
                if let Some(record) = repositories
                    .iter_mut()
                    .find(|repository| repository.root == root)
                {
                    restat_manifest(record, self.window_start_ns);
                }
                return;
            }
            (Some(_), _) => return,
        }
        roots.insert(root.clone(), claim);
        let mut repositories = lock(&self.repositories);
        repositories.retain(|repository| repository.root != root);
        if let Some(mut record) = record {
            restat_manifest(&mut record, self.window_start_ns);
            repositories.push(record);
        }
    }

    /// One event of a frame or a captured set reaching the recorder.
    fn apply_event(&self, event: Event) {
        match event {
            Event::Observe(path, op, outcome) => self.store_observation(&path, op, outcome),
            Event::Taint(kind, detail) => self.store_taint(kind, detail),
            Event::Env(name, observation) => self.store_env(&name, observation),
            Event::Claim(claim) => self.apply_claim(claim),
            Event::Supersede(path) => self.store_supersede(&path),
            Event::Selection(receipt) => self.store_selection(receipt),
            Event::SupersedeSelection(path, canonical) => {
                self.store_supersede_selection(&path, &canonical)
            }
        }
    }

    fn store_selection(&self, receipt: crate::repository_publication::Receipt) {
        let mut selections = lock(&self.selections);
        if let Some(previous) = selections.get(&receipt.canonical)
            && previous != &receipt
        {
            self.store_taint(
                Taint::Unstable,
                format!("publication selection @@{} changed", receipt.canonical),
            );
            return;
        }
        selections.insert(receipt.canonical.clone(), receipt);
    }

    /// A load unit's observations ([`commit`]): its events, and the
    /// captured sets it replayed (each at most once).
    fn apply_events(&self, events: Events) {
        for event in events.events {
            self.apply_event(event);
        }
        for child in &events.children {
            self.replay_captured(child);
        }
    }

    /// A captured set reaching the recorder ([`replay`]): its events and
    /// those of the sets it holds — once; a later replay is a no-op.
    fn replay_captured(&self, captured: &Captured) {
        if captured.recorded.swap(true, Ordering::AcqRel) {
            return;
        }
        for event in &captured.events.events {
            self.apply_event(event.clone());
        }
        for child in &captured.events.children {
            self.replay_captured(child);
        }
    }

    /// Every manifest entry of `repositories` at the manifest's current
    /// lstat: a warm-reuse probe that lost its publication race (W6R: its
    /// observations are dropped with it) may have refreshed a manifest
    /// after the owner's claim was walked, and a dropped claim no longer
    /// restats it ([`Self::claim_tree`]).
    fn restat_manifests(&self, repositories: &mut [RepositoryRecord]) {
        for record in repositories {
            restat_manifest(record, self.window_start_ns);
        }
    }
}

// ---- the hooks that keep a recording the threads-1 one (W6R) ---------------

/// The places where what loading workers observe joins the recording at
/// the sequential engine's point (docs/ANALYSIS_SNAPSHOT_RECORDING.md,
/// "Recording with phase-1 prefetch"). Each can be switched off for tests
/// that prove a fixture needs it (`BAZEL_RS_DEBUG_W6R_OFF=bzl,probe,...`,
/// a debug switch: a run with it is never saved, the record mode reports
/// the test-hook taint).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Hook {
    /// `unit`: a loading worker's load unit records into its frame,
    /// committed with the unit (off: into the recorder at once).
    Unit,
    /// `bzl`: a shared `.bzl` cache hit replays the entry's set.
    Bzl,
    /// `ignored`: an ignored-subdirectories hit replays the entry's set.
    Ignored,
    /// `probe`: a root that begins to count replays its probe's set.
    Probe,
    /// `env`: every repository-environment lookup records the value.
    Env,
    /// `spec`: an ordinary spec-key hit replays the key's set.
    Spec,
    /// `link`: the link policy holds the sequential roots while recording.
    Link,
    /// `canon`: the link policy's canonical forms are recorded once, at
    /// the end of planning (off: where it takes them).
    Canon,
}

impl Hook {
    fn bit(self) -> u32 {
        1 << (self as u32)
    }

    fn named(name: &str) -> Option<Self> {
        Some(match name {
            "unit" => Self::Unit,
            "bzl" => Self::Bzl,
            "ignored" => Self::Ignored,
            "probe" => Self::Probe,
            "env" => Self::Env,
            "spec" => Self::Spec,
            "link" => Self::Link,
            "canon" => Self::Canon,
            _ => return None,
        })
    }
}

/// The hooks `BAZEL_RS_DEBUG_W6R_OFF` switches off.
fn hooks_off() -> u32 {
    static OFF: OnceLock<u32> = OnceLock::new();
    *OFF.get_or_init(|| {
        std::env::var("BAZEL_RS_DEBUG_W6R_OFF")
            .map(|names| {
                names
                    .split(',')
                    .filter_map(|name| Hook::named(name.trim()))
                    .fold(0, |off, hook| off | hook.bit())
            })
            .unwrap_or(0)
    })
}

#[cfg(test)]
thread_local! {
    /// Unit tests: the hooks off on this thread ([`tests_support::hook_off`]).
    static THREAD_HOOKS_OFF: std::cell::Cell<u32> = const { std::cell::Cell::new(0) };
}

/// Whether `hook` is on (always, unless a test switched it off).
#[inline]
pub fn hook(hook: Hook) -> bool {
    #[cfg(test)]
    if THREAD_HOOKS_OFF.with(std::cell::Cell::get) & hook.bit() != 0 {
        return false;
    }
    hooks_off() & hook.bit() == 0
}

// ---- frames (W6R) ----------------------------------------------------------

/// One event observed inside a frame, held until it reaches the recorder.
#[derive(Clone, Debug)]
enum Event {
    Observe(PathBuf, Op, Outcome),
    Taint(Taint, String),
    Env(OsString, EnvObservation),
    Claim(ClaimEvent),
    /// [`supersede`].
    Supersede(PathBuf),
    Selection(crate::repository_publication::Receipt),
    SupersedeSelection(PathBuf, String),
}

/// A repository tree claim observed inside a frame ([`Recorder::claim_tree`]).
#[derive(Clone, Debug)]
struct ClaimEvent {
    claim: Claim,
    kind: &'static str,
    name: String,
    root: PathBuf,
    manifest: Option<PathBuf>,
    /// The REPO record walked by the observing thread; `None` for a local
    /// claim, or when the recorder already had the tree's owner.
    record: Option<RepositoryRecord>,
}

/// The events of a frame, and the captured sets it replayed or whose
/// capture it enclosed (each once).
#[derive(Debug, Default)]
struct Events {
    events: Vec<Event>,
    children: Vec<Arc<Captured>>,
}

impl Events {
    fn is_empty(&self) -> bool {
        self.events.is_empty() && self.children.is_empty()
    }

    fn add_child(&mut self, child: Arc<Captured>) {
        if !self.children.iter().any(|known| Arc::ptr_eq(known, &child)) {
            self.children.push(child);
        }
    }

    fn append(&mut self, other: Events) {
        self.events.extend(other.events);
        for child in other.children {
            self.add_child(child);
        }
    }
}

/// What a memo's miss observed ([`capture`]), kept with the memo entry, or
/// what a warm-reuse probe observed ([`capture_detached`]), kept with its
/// publication: replayed ([`replay`]) wherever the sequential engine's
/// lookup hits the entry or obtains the published root — into the hitting
/// unit's frame, or into the recorder, where it lands at most once.
#[derive(Debug)]
pub struct Captured {
    events: Events,
    /// Its events reached the recorder.
    recorded: AtomicBool,
}

impl Captured {
    fn new(events: Events) -> Arc<Self> {
        Arc::new(Self {
            events,
            recorded: AtomicBool::new(false),
        })
    }

    /// Both sets as one (`None` when both are empty).
    pub fn join(
        first: Option<Arc<Captured>>,
        second: Option<Arc<Captured>>,
    ) -> Option<Arc<Captured>> {
        match (first, second) {
            (Some(first), Some(second)) => {
                let mut events = Events::default();
                events.add_child(first);
                events.add_child(second);
                Some(Self::new(events))
            }
            (one, None) | (None, one) => one,
        }
    }

    /// An empty set, distinct from every other (tests of who stores
    /// which set).
    #[cfg(test)]
    pub(crate) fn for_test() -> Arc<Self> {
        Self::new(Events::default())
    }
}

/// A load unit's observations ([`unit_scope`]): committed by the
/// coordinator with the unit ([`commit`]), dropped with it otherwise.
#[derive(Debug, Default)]
pub struct Observations(Events);

impl Observations {
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

thread_local! {
    /// This thread's open frames, innermost last: a load unit's
    /// ([`unit_scope`]) or a capture's ([`capture`], [`capture_detached`],
    /// [`discard`]); what closes a frame decides where its events go.
    static FRAMES: RefCell<Vec<Events>> = const { RefCell::new(Vec::new()) };
}

/// Whether this thread has an open frame.
fn in_frame() -> bool {
    FRAMES.with(|frames| !frames.borrow().is_empty())
}

/// Add `event` to this thread's innermost frame (see [`Recorder::redirected`]).
fn push_event(event: Event) {
    FRAMES.with(|frames| {
        if let Some(frame) = frames.borrow_mut().last_mut() {
            frame.events.push(event);
        }
    });
}

/// Hand `captured` to this thread's innermost frame; `Err` (with it) when
/// none is open.
fn to_frame(captured: Arc<Captured>) -> Result<(), Arc<Captured>> {
    FRAMES.with(|frames| match frames.borrow_mut().last_mut() {
        Some(frame) => {
            frame.add_child(captured);
            Ok(())
        }
        None => Err(captured),
    })
}

/// One open frame of this thread; closed (with everything opened above it)
/// when dropped unfinished — a panic or an early return discards it.
struct FrameGuard {
    depth: usize,
    open: bool,
}

impl FrameGuard {
    /// Open a frame while recording; `None` otherwise.
    fn open() -> Option<Self> {
        if !recording() {
            return None;
        }
        let depth = FRAMES.with(|frames| {
            let mut frames = frames.borrow_mut();
            frames.push(Events::default());
            frames.len() - 1
        });
        Some(Self { depth, open: true })
    }

    /// Close the frame: its events.
    fn close(mut self) -> Events {
        self.open = false;
        FRAMES.with(|frames| {
            let mut frames = frames.borrow_mut();
            debug_assert_eq!(frames.len(), self.depth + 1, "frames close innermost first");
            frames.truncate(self.depth + 1);
            frames.pop().unwrap_or_default()
        })
    }
}

impl Drop for FrameGuard {
    fn drop(&mut self) {
        if self.open {
            let _ = FRAMES.try_with(|frames| {
                if let Ok(mut frames) = frames.try_borrow_mut() {
                    frames.truncate(self.depth);
                }
            });
        }
    }
}

/// A load unit's frame (crate::package::evaluate_load_unit on a loading
/// worker): what the unit observes stays in it until [`UnitScope::finish`]
/// hands it to the unit.
pub struct UnitScope(Option<FrameGuard>);

/// Open a load unit's frame on this thread while recording (an inert scope
/// otherwise).
pub fn unit_scope() -> UnitScope {
    UnitScope(FrameGuard::open())
}

impl UnitScope {
    /// Close the frame: what the unit observed, for its commit.
    pub fn finish(self) -> Observations {
        Observations(self.0.map(FrameGuard::close).unwrap_or_default())
    }
}

/// Commit a load unit's observations where the sequential engine loads the
/// package (Engine::commit_load_unit): into this thread's innermost frame,
/// else into the recorder (each captured set it holds at most once).
pub fn commit(observations: Observations) {
    if observations.is_empty() {
        return;
    }
    let events = FRAMES.with(|frames| match frames.borrow_mut().last_mut() {
        Some(frame) => {
            frame.append(observations.0);
            None
        }
        None => Some(observations.0),
    });
    if let Some(events) = events
        && let Some(recorder) = active()
    {
        recorder.apply_events(events);
    }
}

/// Run `f` capturing what it observes (a memo's miss): the set is handed to
/// the enclosing frame (or the recorder) when `f` returns, as if observed
/// there, and returned for the memo entry, whose hits [`replay`] it.
/// `None` while not recording or when `f` observed nothing.
pub fn capture<R>(f: impl FnOnce() -> R) -> (R, Option<Arc<Captured>>) {
    let Some(frame) = FrameGuard::open() else {
        return (f(), None);
    };
    let result = f();
    let events = frame.close();
    if events.is_empty() {
        return (result, None);
    }
    let captured = Captured::new(events);
    replay(&captured);
    (result, Some(captured))
}

/// [`capture`] that hands nothing to the enclosing frame: a warm-reuse
/// probe's observations travel with its publication only
/// (crate::repository_demand::DemandSlot::observations).
pub fn capture_detached<R>(f: impl FnOnce() -> R) -> (R, Option<Arc<Captured>>) {
    let Some(frame) = FrameGuard::open() else {
        return (f(), None);
    };
    let result = f();
    let events = frame.close();
    (result, (!events.is_empty()).then(|| Captured::new(events)))
}

/// Run `f` dropping what it observes (a coordinator-side hint).
pub fn discard<R>(f: impl FnOnce() -> R) -> R {
    let frame = FrameGuard::open();
    let result = f();
    if let Some(frame) = frame {
        drop(frame.close());
    }
    result
}

/// Replay what a memo entry or a publication captured where the sequential
/// engine hits it: into this thread's innermost frame, else into the
/// recorder (at most once there).
pub fn replay(captured: &Arc<Captured>) {
    if let Err(captured) = to_frame(captured.clone())
        && let Some(recorder) = active()
    {
        recorder.replay_captured(&captured);
    }
}

/// [`replay`] of an optional set.
pub fn replay_some(captured: Option<&Arc<Captured>>) {
    if let Some(captured) = captured {
        replay(captured);
    }
}

/// The manifest entry of `record` at the manifest's current lstat (see
/// `Recorder::claim_tree`).
fn restat_manifest(record: &mut RepositoryRecord, window_start_ns: i64) {
    let Some(manifest) = &record.manifest else {
        return;
    };
    let state = match fs::symlink_metadata(manifest) {
        Ok(metadata) => Ok((FileKind::of(metadata.file_type()), StatKey::of(&metadata))),
        Err(error) => Err(errno(&error)),
    };
    let racy = matches!(&state, Ok((_, key)) if key.racy(window_start_ns));
    if let Some(entry) = record
        .entries
        .iter_mut()
        .find(|entry| entry.path == *manifest)
    {
        entry.state = state;
        entry.racy = racy;
    }
}

// ---- the window ------------------------------------------------------------

/// Open the process's recording window (once per process). Returns false
/// when a window was already opened.
pub fn open_window() -> bool {
    if RECORDER.set(Recorder::framed(now_ns())).is_err() {
        return false;
    }
    let _ = fs::symlink_metadata(MARKER_OPEN);
    RECORDING.store(true, Ordering::SeqCst);
    true
}

/// The paths a repository's or a module extension's recorded inputs named
/// while the window was open (crate::repository_ctx's recorded states):
/// (path, its tree: a `watch_tree` directory, everything below counts).
/// Incremental analysis (crate::snapshot::incremental) never attributes a
/// change of one of them to a package: the repository may change.
static REPOSITORY_INPUTS: Mutex<BTreeSet<(PathBuf, bool)>> = Mutex::new(BTreeSet::new());

/// A recorded input of a repository or extension was observed (see
/// [`REPOSITORY_INPUTS`]); outside the window nothing is kept.
pub fn note_repository_input(path: &Path, tree: bool) {
    if recording() && !PRIVATE_RECORDING.with(std::cell::Cell::get) {
        lock(&REPOSITORY_INPUTS).insert((path.to_path_buf(), tree));
    }
}

/// The repository inputs noted so far (see [`note_repository_input`]).
pub fn repository_inputs() -> Vec<(PathBuf, bool)> {
    lock(&REPOSITORY_INPUTS).iter().cloned().collect()
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

// ---- semantic records ------------------------------------------------------

/// Record that `bytes` were read from `path` by code that must keep its own
/// reader (e.g. the repository store's no-follow, change-checked reads): READ
/// with the key of an lstat taken now. A change between that read and this
/// lstat is within the window, hence racy and content-checked on validation.
pub fn note_read(path: &Path, bytes: &[u8]) {
    note_read_as(path, bytes, Op::Read);
}

pub(crate) fn note_selection_read(path: &Path, bytes: &[u8], canonical: &str) {
    note_read_as(path, bytes, Op::SelectionRead(canonical.to_owned()));
}

pub(crate) fn note_selection_absent(path: &Path, canonical: &str) {
    if let Some(recorder) = active() {
        let outcome = match fs::symlink_metadata(path) {
            Err(error) => Outcome::Error(errno(&error)),
            Ok(metadata) => {
                recorder.taint(
                    Taint::Unstable,
                    format!(
                        "selection pointer {} appeared while choosing @@{canonical}",
                        escape(path.as_os_str())
                    ),
                );
                Outcome::Kind(FileKind::of(metadata.file_type()))
            }
        };
        recorder.observe(path, Op::SelectionAbsent(canonical.to_owned()), outcome);
    }
}

pub(crate) fn supersede_selection(path: &Path, canonical: &str) {
    if let Some(recorder) = active() {
        if recorder.redirected() {
            push_event(Event::SupersedeSelection(
                path.to_path_buf(),
                canonical.to_owned(),
            ));
        } else {
            recorder.store_supersede_selection(path, canonical);
        }
    }
}

fn note_read_as(path: &Path, bytes: &[u8], op: Op) {
    let Some(recorder) = active() else {
        return;
    };
    let key = fs::symlink_metadata(path)
        .ok()
        .map(|metadata| StatKey::of(&metadata))
        .unwrap_or_default();
    recorder.observe(
        path,
        op,
        Outcome::Read {
            sha256: sha256(bytes),
            key,
            racy: recorder.racy(&key),
        },
    );
}

/// Record the READ of `path` that a digest memo answered (integration-20,
/// INTEGRATION-20.md F4): crate::digest_memo's persistent table, an
/// in-process memo in front of it (repository_ctx's recorded-input
/// `FileDigestMemo`), or the bytes such a caller hashed from the file it
/// opened at `path`. `sha256` is that file's digest and `opened` its own
/// fstat — the exact stat the memo's lookup was keyed by (for a symlink the
/// target's) — which becomes the READ's key: never [`note_read`]'s fresh
/// lstat, since a hit reads nothing and a link's own lstat says nothing
/// about the bytes it leads to. As for a read through this facade
/// (`Recorder::key_before`), a symlink at `path` also becomes an LSTAT
/// observation (its target), so retargeting it is seen. Call it for every
/// answer, hit or read.
pub fn note_read_digest(path: &Path, opened: &fs::Metadata, sha256: [u8; 32]) {
    if let Some(recorder) = active() {
        recorder.read_digest(path, opened, sha256);
    }
}

/// Record an environment value the repository environment observed (ENV,
/// digest only; crate::repository_environment). The only ENV records: a
/// hit validates them against its own command's repository environment
/// (crate::snapshot::validate::Environment), so a value must come from
/// that map (the client environment with `--repo_env` applied), never from
/// the process environment directly.
pub fn note_env(name: &OsStr, value: Option<&OsStr>) {
    if let Some(recorder) = active() {
        recorder.observe_env(name, value);
    }
}

/// The digest [`note_env`] records for `value` (W6R: the repository
/// environment keeps it with its entry and records it at every lookup,
/// crate::repository_environment).
pub fn env_digest(value: Option<&OsStr>) -> Option<[u8; 32]> {
    value.map(|value| sha256(value.as_bytes()))
}

/// [`note_env`] of a value's [`env_digest`].
pub fn note_env_digest(name: &OsStr, digest: Option<[u8; 32]>) {
    if let Some(recorder) = active() {
        recorder.observe_env_digest(name, digest);
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

/// The planned request decided the plan-dependent taints (L2-W4, PLAN-L2
/// update 1 §(c)4): what analysis registered (a build-info callback, a
/// `map_each` over a tree artifact — [`Taint::plan_dependent`]) moves to
/// [`Recording::plan_registered`], and only `blockers`, the plan's own
/// (crate::request::PlannedRequest::blockers: the actions the plan kept),
/// taint the run. Recorded also after the window closed.
pub fn settle_plan_taints(blockers: impl IntoIterator<Item = (Taint, String)>) {
    if let Some(recorder) = RECORDER.get() {
        recorder.settle_plan(blockers);
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

/// Record a repository tree the run verified as a repository's root (REPO +
/// INTEGRITY): the lstat key of every entry below `root` (and of the
/// manifest), walked now. Call it after the tree's own verification (and
/// fingerprint refresh) succeeded; once per root and window (it replaces a
/// [`crossed_tree`] record of the same root).
pub fn repository_tree(kind: &'static str, name: &str, root: &Path, manifest: Option<&Path>) {
    // The tree's last use (docs/CACHE_GC.md).
    crate::cache_gc::note_tree_used(root);
    if let Some(recorder) = active() {
        recorder.claim_tree(Claim::Owner, kind, name, root, manifest);
    }
}

/// A producer verified during private proof. Unlike live reuse this never
/// updates GC last-use state. The caller must be inside `record_private` or a
/// discarded inspection scope; no public binding is implied by this claim.
pub(crate) fn proof_repository_tree(kind: &'static str, name: &str, root: &Path, manifest: &Path) {
    if let Some(recorder) = active() {
        recorder.claim_tree(Claim::Owner, kind, name, root, Some(manifest));
    }
}

/// A successful live selection. No path-based filtering or superseding is
/// performed: the pointer's READ remains an independent acceptance guard.
pub(crate) fn publication_selection(receipt: crate::repository_publication::Receipt) {
    if let Some(recorder) = active() {
        if recorder.redirected() {
            push_event(Event::Selection(receipt));
        } else {
            recorder.store_selection(receipt);
        }
    }
}

/// Record a published store tree a symlink crossed into before any
/// repository of the run claimed it (crate::source_links::StoreTrees), named
/// by its identity: [`repository_tree`] or [`local_tree`] replace it when
/// the repository is materialized later in the run; otherwise it stays in
/// [`Recording::crossed`] (a link into a tree no repository of the run has
/// as its root, e.g. an older publication).
pub fn crossed_tree(kind: &'static str, id: &str, root: &Path, manifest: &Path) {
    crate::cache_gc::note_tree_used(root);
    if let Some(recorder) = active() {
        recorder.claim_tree(Claim::Crossing, kind, id, root, Some(manifest));
    }
}

/// A local repository rule published (or kept) the tree at `root`: it is
/// replayed, never a REPO — a link that crossed into it earlier in the run
/// loses its record, so the recording is the same in either order.
pub fn local_tree(root: &Path) {
    crate::cache_gc::note_tree_used(root);
    if let Some(recorder) = active() {
        recorder.claim_tree(Claim::Local, "direct", "", root, None);
    }
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
/// diagnostics appear in the record mode, under `BAZEL_RS_SNAPSHOT_EXPLAIN`,
/// for a verify mismatch and for an unknown `BAZEL_RS_SNAPSHOT` value (the
/// default `on` is otherwise silent); the prefix lets exact-stderr checks
/// drop them (like `PERF` lines).
pub fn diag(line: &str) {
    // The diagnostics stream: outside a stderr capture (crate::snapshot::diag).
    crate::snapshot::diag::line(&format!("bazel-rs-snapshot: {line}"));
}

/// Unit tests: a recording window of one thread.
#[cfg(test)]
pub(crate) mod tests_support {
    use super::*;

    /// This thread's window, open until dropped.
    pub(crate) struct ThreadWindow {
        pub(crate) recorder: &'static Recorder,
    }

    /// Open a window of this thread only: what this thread observes goes
    /// to the returned recorder (frames included), nothing else changes
    /// for other threads.
    pub(crate) fn record_this_thread() -> ThreadWindow {
        let recorder: &'static Recorder = Box::leak(Box::new(Recorder::framed(now_ns())));
        THREAD_RECORDER.with(|cell| cell.set(Some(recorder)));
        ThreadWindow { recorder }
    }

    impl Drop for ThreadWindow {
        fn drop(&mut self) {
            THREAD_RECORDER.with(|cell| cell.set(None));
        }
    }

    /// Switch `hook` off on this thread until the guard drops (a kill
    /// switch: the test proves it needs the hook).
    pub(crate) fn hook_off(hook: Hook) -> HookOff {
        THREAD_HOOKS_OFF.with(|off| off.set(off.get() | hook.bit()));
        HookOff(hook)
    }

    pub(crate) struct HookOff(Hook);

    impl Drop for HookOff {
        fn drop(&mut self) {
            THREAD_HOOKS_OFF.with(|off| off.set(off.get() & !self.0.bit()));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::tests_support::record_this_thread;
    use super::*;

    fn paths_of(recording: &Recording) -> Vec<(String, &'static str)> {
        recording
            .observations
            .iter()
            .map(|(path, observation)| (path.display().to_string(), observation.op.name()))
            .collect()
    }

    /// W6R: a load unit's frame reaches the recorder only through its
    /// commit; a dropped frame (an unused or aborted unit), and the
    /// captures inside it, record nothing.
    #[test]
    fn a_frame_reaches_the_recorder_only_through_its_commit() {
        let temp = tempfile::tempdir().unwrap();
        let file = temp.path().join("f");
        fs::write(&file, b"x").unwrap();
        let window = record_this_thread();
        let unit = unit_scope();
        read(&file).unwrap();
        let (_, captured) = capture(|| exists(temp.path().join("g")));
        assert!(captured.is_some());
        assert!(window.recorder.snapshot().observations.is_empty());
        let observations = unit.finish();
        assert!(window.recorder.snapshot().observations.is_empty());
        commit(observations);
        let file_text = file.display().to_string();
        let missing = temp.path().join("g").display().to_string();
        assert_eq!(
            paths_of(&window.recorder.snapshot()),
            [(file_text, "READ"), (missing.clone(), "STAT")]
        );
        // Dropped: nothing, captures included.
        let window = record_this_thread();
        {
            let _unit = unit_scope();
            read(&file).unwrap();
            let _ = capture(|| exists(temp.path().join("h")));
        }
        assert!(!in_frame());
        assert!(window.recorder.snapshot().observations.is_empty());
        // Outside every frame: at once.
        exists(temp.path().join("g"));
        assert_eq!(paths_of(&window.recorder.snapshot()), [(missing, "STAT")]);
    }

    /// The recorder is a set: an outcome a frame brings that conflicts
    /// with the recorded one makes the run Unstable at the commit, like
    /// two observations on one thread.
    #[test]
    fn a_conflicting_outcome_at_the_commit_is_unstable() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("late");
        let window = record_this_thread();
        let unit = unit_scope();
        assert!(!exists(&path));
        let observations = unit.finish();
        fs::write(&path, b"now").unwrap();
        assert!(exists(&path));
        assert!(window.recorder.taints().is_empty());
        commit(observations);
        let taints = window.recorder.taints();
        assert_eq!(taints[&Taint::Unstable].count, 1, "{taints:?}");
        // The same outcome again: no conflict.
        let unit = unit_scope();
        assert!(exists(&path));
        commit(unit.finish());
        assert_eq!(window.recorder.taints()[&Taint::Unstable].count, 1);
    }

    /// A superseded path's earlier outcome is forgotten, in order: the
    /// replacement's outcome conflicts with nothing — directly and inside a
    /// frame (where the supersede applies at the commit, after the frame's
    /// earlier events) — while an outcome observed before the supersede but
    /// committed after it still conflicts (Unstable).
    #[test]
    fn a_superseded_outcome_is_forgotten_in_order() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("pointer");
        let window = record_this_thread();
        fs::write(&path, b"old").unwrap();
        note_read(&path, b"old");
        fs::write(&path, b"new!").unwrap();
        supersede(&path);
        note_read(&path, b"new!");
        assert!(window.recorder.taints().is_empty());
        let unit = unit_scope();
        fs::write(&path, b"newer").unwrap();
        supersede(&path);
        note_read(&path, b"newer");
        commit(unit.finish());
        assert!(window.recorder.taints().is_empty());
        let recorded = window.recorder.snapshot();
        assert_eq!(recorded.observations.len(), 1, "{recorded:?}");
        // An outcome from a frame that saw the file before the supersede.
        let stale = unit_scope();
        note_read(&path, b"newer");
        let stale = stale.finish();
        fs::write(&path, b"newest").unwrap();
        supersede(&path);
        note_read(&path, b"newest");
        commit(stale);
        assert_eq!(window.recorder.taints()[&Taint::Unstable].count, 1);
    }

    /// Claims made inside frames merge to the recording the same claims
    /// make in any order outside them: the owner's record wins over a
    /// crossing's, whether the crossing came first or reached the recorder
    /// first.
    #[test]
    fn crossing_and_owner_claims_merge_alike_in_either_order() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("trees/abc");
        fs::create_dir_all(&root).unwrap();
        fs::write(root.join("BUILD.bazel"), b"").unwrap();
        let manifest = temp.path().join("manifests/abc.json");
        fs::create_dir_all(manifest.parent().unwrap()).unwrap();
        fs::write(&manifest, b"{}").unwrap();
        let other = temp.path().join("trees/def");
        fs::create_dir_all(&other).unwrap();
        let other_manifest = temp.path().join("manifests/def.json");
        fs::write(&other_manifest, b"{}").unwrap();
        let shape = |recording: Recording| {
            (
                recording
                    .repositories
                    .iter()
                    .map(|record| (record.kind, record.name.clone(), record.entries.clone()))
                    .collect::<Vec<_>>(),
                recording.crossed,
            )
        };
        let owner = || repository_tree("direct", "abc", &root, Some(&manifest));
        let crossing = || {
            crossed_tree("direct", "abc", &root, &manifest);
            crossed_tree("direct", "def", &other, &other_manifest);
        };
        let reference = {
            let window = record_this_thread();
            owner();
            crossing();
            shape(window.recorder.snapshot())
        };
        assert_eq!(reference.0.len(), 2);
        assert_eq!(reference.1, std::slice::from_ref(&other));
        type Order = fn(&dyn Fn(), &dyn Fn());
        let orders: [Order; 4] = [
            // The crossing in a frame committed first, the owner after.
            |owner, crossing| {
                let unit = unit_scope();
                crossing();
                commit(unit.finish());
                owner();
            },
            // The owner first, a crossing's frame after.
            |owner, crossing| {
                owner();
                let unit = unit_scope();
                crossing();
                commit(unit.finish());
            },
            // Both in frames, the owner's committed last.
            |owner, crossing| {
                let first = unit_scope();
                crossing();
                let first = first.finish();
                let second = unit_scope();
                owner();
                let second = second.finish();
                commit(first);
                commit(second);
            },
            // The owner's frame committed first.
            |owner, crossing| {
                let first = unit_scope();
                owner();
                let first = first.finish();
                commit(first);
                let second = unit_scope();
                crossing();
                commit(second.finish());
            },
        ];
        for (index, order) in orders.iter().enumerate() {
            let window = record_this_thread();
            order(&owner, &crossing);
            assert_eq!(
                shape(window.recorder.snapshot()),
                reference,
                "order {index}"
            );
        }
    }

    #[test]
    fn publication_receipts_survive_detached_capture_without_erasing_reads() {
        use crate::repository_publication::{Origin, Receipt, Scope};
        let temp = tempfile::tempdir().unwrap();
        let pointer = temp.path().join("pointer");
        fs::write(&pointer, "owned selection bytes").unwrap();
        let receipt = Receipt {
            canonical: "+maker+r".into(),
            origin: Origin::DirectV2,
            spec: "a".repeat(64),
            publication: "b".repeat(64),
            scope: Scope::Shared,
            manifest_digest: [7; 32],
        };
        let window = record_this_thread();
        let (_, captured) = capture_detached(|| {
            read(&pointer).unwrap();
            publication_selection(receipt.clone());
        });
        assert!(window.recorder.snapshot().selections.is_empty());
        assert!(window.recorder.snapshot().observations.is_empty());
        let captured = captured.unwrap();
        let unit = unit_scope();
        replay(&captured);
        commit(unit.finish());
        replay(&captured);
        let recorded = window.recorder.snapshot();
        assert_eq!(recorded.selections, [receipt]);
        assert_eq!(recorded.observations.len(), 1);
        assert_eq!(recorded.observations[0].0, pointer);
        assert_eq!(recorded.observations[0].1.op, Op::Read);
        assert!(recorded.taints.is_empty());
        assert_eq!(window.recorder.take().selections, recorded.selections);
    }

    #[test]
    fn selection_reprobe_supersedes_only_its_own_operations() {
        let temp = tempfile::tempdir().unwrap();
        let pointer = temp.path().join("pointer");
        fs::write(&pointer, "old").unwrap();
        let window = record_this_thread();
        note_read(&pointer, b"old");
        note_selection_read(&pointer, b"old", "r");
        let unit = unit_scope();
        fs::write(&pointer, "new").unwrap();
        supersede_selection(&pointer, "r");
        note_selection_read(&pointer, b"new", "r");
        commit(unit.finish());
        let recording = window.recorder.snapshot();
        assert_eq!(recording.observations.len(), 2);
        assert!(recording.observations.iter().any(|(_, observation)| matches!(&observation.outcome,
            Outcome::Read { sha256: digest, .. } if observation.op == Op::Read && *digest == sha256(b"old"))));
        assert!(recording.observations.iter().any(|(_, observation)| matches!(&observation.outcome,
            Outcome::Read { sha256: digest, .. } if observation.op == Op::SelectionRead("r".into()) && *digest == sha256(b"new"))));
        assert!(recording.taints.is_empty());
    }

    #[test]
    fn private_probe_does_not_publish_incremental_repository_inputs() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("private-runtime-input");
        let (_, recording) = record_private(|| note_repository_input(&path, true));
        assert!(recording.observations.is_empty());
        assert!(!repository_inputs().iter().any(|(input, _)| input == &path));
    }

    /// A captured set lands in the recorder at most once, however many
    /// frames hold it and in whatever order they commit (a taint inside it
    /// counts once, as on one thread); replays into frames stay there.
    #[test]
    fn a_captured_set_replays_once_per_sink() {
        let temp = tempfile::tempdir().unwrap();
        let window = record_this_thread();
        let first = unit_scope();
        let (_, captured) = capture(|| {
            exists(temp.path().join("a"));
            taint(Taint::CrossRepositorySymlink, || "inside".to_owned());
        });
        let captured = captured.unwrap();
        let first = first.finish();
        let second = unit_scope();
        replay(&captured);
        replay(&captured);
        let second = second.finish();
        assert_eq!(second.0.children.len(), 1);
        assert!(window.recorder.snapshot().observations.is_empty());
        commit(second);
        commit(first);
        replay(&captured);
        let recording = window.recorder.snapshot();
        assert_eq!(recording.observations.len(), 1);
        assert_eq!(
            recording.taints[&Taint::CrossRepositorySymlink].count,
            1,
            "{:?}",
            recording.taints
        );
        // Joined sets replay their parts once too.
        let window = record_this_thread();
        let unit = unit_scope();
        let (_, one) = capture(|| exists(temp.path().join("one")));
        let (_, two) = capture(|| exists(temp.path().join("two")));
        drop(unit);
        let joined = Captured::join(one.clone(), two).unwrap();
        replay(&joined);
        replay(one.as_ref().unwrap());
        assert_eq!(window.recorder.snapshot().observations.len(), 2);
    }

    /// A detached capture (a warm-reuse probe) hands nothing to the frame
    /// around it: what it observed reaches the recorder only where its
    /// set is replayed.
    #[test]
    fn detached_captures_do_not_forward() {
        let temp = tempfile::tempdir().unwrap();
        let window = record_this_thread();
        let unit = unit_scope();
        exists(temp.path().join("unit"));
        let (_, detached) = capture_detached(|| {
            exists(temp.path().join("probe"));
            // An attached capture inside it goes to the detached set.
            let _ = capture(|| exists(temp.path().join("nested")));
        });
        let detached = detached.unwrap();
        commit(unit.finish());
        let unit_path = temp.path().join("unit").display().to_string();
        assert_eq!(
            paths_of(&window.recorder.snapshot()),
            [(unit_path.clone(), "STAT")]
        );
        replay(&detached);
        let mut seen: Vec<String> = paths_of(&window.recorder.snapshot())
            .into_iter()
            .map(|(path, _)| path)
            .collect();
        seen.sort();
        let mut expected: Vec<String> = ["nested", "probe", "unit"]
            .iter()
            .map(|name| temp.path().join(name).display().to_string())
            .collect();
        expected.sort();
        assert_eq!(seen, expected);
        // A discard frame drops everything, its captures' forwarding too.
        let window = record_this_thread();
        let kept = discard(|| {
            exists(temp.path().join("hint"));
            capture(|| exists(temp.path().join("memo"))).1
        });
        assert!(window.recorder.snapshot().observations.is_empty());
        replay(kept.as_ref().unwrap());
        assert_eq!(window.recorder.snapshot().observations.len(), 1);
    }

    /// Two spellings of one path (`Path` equality ignores a trailing
    /// separator) share one entry with the smallest spelling, in either
    /// order, so the recording does not depend on which came first.
    #[test]
    fn a_path_spelled_two_ways_keeps_its_smallest_spelling_in_any_order() {
        let temp = tempfile::tempdir().unwrap();
        let dir = temp.path().join("d");
        fs::create_dir(&dir).unwrap();
        let slashed = PathBuf::from(format!("{}/", dir.display()));
        assert_eq!(dir, slashed);
        for order in [[&dir, &slashed], [&slashed, &dir]] {
            let recorder = Recorder::new(now_ns());
            for path in order {
                recorder.observe(path, Op::Stat, Outcome::Kind(FileKind::Dir));
                recorder.observe(path, Op::Canon, Outcome::Path(dir.clone()));
            }
            for recording in [recorder.snapshot(), recorder.take()] {
                assert_eq!(recording.observations.len(), 2);
                for (path, _) in &recording.observations {
                    assert_eq!(path.as_os_str(), dir.as_os_str());
                }
            }
        }
    }

    /// Outside a window every frame operation is inert: nothing is
    /// captured, committed or replayed, and no frame stays open.
    #[test]
    fn frames_are_inert_when_not_recording() {
        let temp = tempfile::tempdir().unwrap();
        let unit = unit_scope();
        exists(temp.path().join("a"));
        let (_, captured) = capture(|| exists(temp.path().join("b")));
        let (_, detached) = capture_detached(|| exists(temp.path().join("c")));
        discard(|| exists(temp.path().join("d")));
        assert!(captured.is_none() && detached.is_none());
        assert!(!in_frame());
        let observations = unit.finish();
        assert!(observations.is_empty());
        commit(observations);
        assert!(!in_frame());
    }

    #[test]
    fn invalid_utf8_error_matches_std() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("bad");
        fs::write(&path, [0xff, 0xfe]).unwrap();
        let std_error = fs::read_to_string(&path).unwrap_err();
        let recorder = Recorder::new(now_ns());
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
        let recorder = Recorder::new(now_ns());
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

    /// A digest memo's answer (integration-20 F4) is recorded as the READ a
    /// read through the facade records: the opened file's stat — for a
    /// symlink the target's, plus the link's LSTAT — never the link's own
    /// lstat.
    #[test]
    fn digest_memo_answers_are_reads_keyed_by_the_opened_file() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        fs::write(root.join("a"), b"alpha").unwrap();
        std::os::unix::fs::symlink("a", root.join("link")).unwrap();
        let digest = sha256(b"alpha");
        let opened = fs::File::open(root.join("link"))
            .unwrap()
            .metadata()
            .unwrap();
        let memo = Recorder::new(now_ns());
        memo.read_digest(
            &root.join("a"),
            &fs::metadata(root.join("a")).unwrap(),
            digest,
        );
        memo.read_digest(&root.join("link"), &opened, digest);
        let facade = Recorder::new(now_ns());
        facade.read(&root.join("a")).unwrap();
        facade.read(&root.join("link")).unwrap();
        let (memo, facade) = (memo.snapshot(), facade.snapshot());
        assert_eq!(memo.observations, facade.observations);
        let key = StatKey::of(&opened);
        assert_ne!(
            key,
            StatKey::of(&fs::symlink_metadata(root.join("link")).unwrap())
        );
        let link_read = memo
            .observations
            .iter()
            .find(|(path, observation)| path.ends_with("link") && observation.op == Op::Read)
            .unwrap();
        assert!(matches!(
            link_read.1.outcome,
            Outcome::Read { sha256, key: read_key, .. } if sha256 == digest && read_key == key
        ));
        assert!(memo.observations.iter().any(|(path, observation)| {
            path.ends_with("link")
                && observation.op == Op::Lstat
                && observation.outcome == Outcome::Link(PathBuf::from("a"))
        }));
        assert!(memo.taints.is_empty(), "{:?}", memo.taints);
    }

    #[test]
    fn conflicting_outcomes_are_unstable() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("f");
        let recorder = Recorder::new(now_ns());
        recorder.observe(&path, Op::Stat, Outcome::Error(2));
        recorder.observe(&path, Op::Stat, Outcome::Error(2));
        assert!(recorder.snapshot().taints.is_empty());
        recorder.observe(&path, Op::Stat, Outcome::Kind(FileKind::File));
        let taints = recorder.snapshot().taints;
        assert_eq!(taints[&Taint::Unstable].count, 1);
    }

    /// Two verifications of one publication race (workers' warm probes):
    /// the later one's fingerprint refresh rewrites the manifest after the
    /// first claim walked it. The record keeps the manifest's latest key.
    #[test]
    fn a_later_owner_claim_records_the_refreshed_manifest() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("trees/abc");
        fs::create_dir_all(&root).unwrap();
        fs::write(root.join("BUILD.bazel"), b"").unwrap();
        let manifest = temp.path().join("manifests/abc.json");
        fs::create_dir_all(manifest.parent().unwrap()).unwrap();
        fs::write(&manifest, b"{\"v\": 1}").unwrap();
        let recorder = Recorder::new(now_ns());
        let manifest_key = |recording: &Recording| {
            recording.repositories[0]
                .entries
                .iter()
                .find(|entry| entry.path == manifest)
                .map(|entry| entry.state)
                .unwrap()
        };
        recorder.claim_tree(Claim::Owner, "direct", "abc", &root, Some(&manifest));
        let first = manifest_key(&recorder.snapshot());
        // The refresh: a new file renamed over the manifest (a new inode).
        let staged = temp.path().join("manifests/.staged");
        fs::write(&staged, b"{\"v\": 2}").unwrap();
        fs::rename(&staged, &manifest).unwrap();
        recorder.claim_tree(Claim::Owner, "direct", "abc", &root, Some(&manifest));
        let recording = recorder.snapshot();
        assert_eq!(recording.repositories.len(), 1);
        let second = manifest_key(&recording);
        assert_ne!(first, second);
        let now = fs::symlink_metadata(&manifest).unwrap();
        assert_eq!(second, Ok((FileKind::File, StatKey::of(&now))));
        // The tree's entries were walked once; the root entry is unchanged.
        assert!(
            recording.repositories[0]
                .entries
                .iter()
                .any(|entry| entry.path == root.join("BUILD.bazel"))
        );
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
