//! Execution-root reuse: genuine's `--reuse_sandbox_directories` (default
//! true; SandboxStash, SandboxHelpers.cleanExisting) for native execution
//! roots (docs/HARDLINK_STAGING.md, "Execution-root reuse").
//!
//! An action's execution root is created, filled with its inputs (a link
//! per input file: a symlink into a repository tree, a hardlink of a store
//! blob) and removed after its process. A C++ compile stages thousands of
//! headers, most of them the same as the previous compile's; on an overlay
//! filesystem each link and unlink costs ~70 µs with 16 jobs, and staging
//! and removal took about as long as the compilers (OpenROAD's
//! `//:openroad`). While a scheduler executes a graph on a cache
//! ([`StashScope`]), the execution root of a successful local process
//! action is kept (stashed) with what staging placed in it, and the next
//! action of the same mnemonic takes it: [`reconcile`] removes every entry
//! that is not exactly what staging would place now — anything unknown
//! (outputs, parameter files, files the process created, directories no
//! input needs) and every placed entry whose destination is now another
//! input, a symlink whose text is not the tree file's path, a hardlink
//! whose state changed — and keeps the rest; staging then creates what is
//! missing and re-checks every kept input exactly as it checks a new one
//! (crate::action `stage_tree_link`, `stage_file`). The process sees the
//! same files at the same paths (the same inodes, link texts, modes and
//! directory modes) as in a fresh root; what differs is only the change
//! times of the kept links and directories, as in genuine's reused
//! sandboxes. Stashes are removed when the scope ends.
//! `BAZEL_RS_REUSE_EXECROOTS=0` turns reuse off.

use super::*;
use std::ffi::OsString;

/// What staging placed at one destination of a stashed execution root.
pub(super) enum Placed {
    /// A symlink whose text is this repository tree file's path.
    TreeLink(PathBuf),
    /// A hardlink of this store file, in this state (as staged, and as
    /// checked after the process).
    Blob { blob: PathBuf, state: InodeState },
}

/// A directory of an execution root as staging left it: its inode and
/// change time.
#[derive(Clone, Copy, PartialEq, Eq)]
struct DirectoryState {
    inode: u64,
    ctime: (i64, i64),
}

impl DirectoryState {
    #[cfg_attr(not(target_os = "linux"), allow(clippy::useless_conversion))]
    fn of(stat: &rustix::fs::Stat) -> Self {
        DirectoryState {
            inode: stat.st_ino,
            ctime: (
                stat.st_ctime,
                i64::try_from(stat.st_ctime_nsec).unwrap_or(i64::MAX),
            ),
        }
    }
}

/// The directories of a staged execution root as staging left them
/// ([`record`]), by path relative to the root (`""` the root itself): the
/// state [`reconcile`] compares, and for most of them the exact listing.
#[derive(Default)]
pub(super) struct Recorded {
    states: FxHashMap<PathBuf, DirectoryState>,
    /// The entries (name, type) of the recorded directories in which
    /// staging made nothing but the placed files and the directories above
    /// them, the directories it creates for trees and source directories
    /// included: a directory found unchanged lists exactly these.
    listings: FxHashMap<PathBuf, Vec<(OsString, rustix::fs::FileType)>>,
}

/// Records the directories of `execroot` as staging `inputs` left them
/// (`staged`: the files it placed), right after staging and before
/// anything else changes the root (parameter files, output directories,
/// the process). A directory's change time moves with every entry
/// created, removed or renamed in it (link, symlink, unlink, rename,
/// mkdir, rmdir), so one found in the recorded state again (inode and
/// change time) holds exactly the entries it held here: no kept link was
/// replaced and none of its files is another inode. Change times come from
/// the kernel's coarse clock (CLOCK_REALTIME_COARSE, which `current_time`
/// reads; finer multigrain stamps are never below it), so a change made in
/// the tick in which a directory was last changed could leave its change
/// time as it is: only directories whose change time is below the coarse
/// clock read after them are recorded — any later change stamps a later
/// time. The others are listed and verified entry by entry as before.
pub(super) fn record(
    execroot: &Path,
    staged: &[StagedFile],
    inputs: &BTreeMap<PathBuf, Staged>,
    link_source_directories: bool,
) -> Recorded {
    use rustix::fs::{AtFlags, FileType, statat};
    let Ok(root) = open_directory_at(None, execroot) else {
        return Recorded::default();
    };
    // Every directory staging made or kept: those above a placed file, the
    // ones it creates for trees and source directories (empty ones too),
    // and those above a link it makes; and the directories holding an
    // entry it makes besides files and directories (those links), which
    // are listed again.
    let mut directories: FxHashSet<&Path> = FxHashSet::default();
    directories.insert(Path::new(""));
    fn add<'a>(directories: &mut FxHashSet<&'a Path>, directory: &'a Path) {
        let mut next = Some(directory);
        while let Some(directory) = next.filter(|directory| !directory.as_os_str().is_empty()) {
            if !directories.insert(directory) {
                break;
            }
            next = directory.parent();
        }
    }
    let mut owned: Vec<PathBuf> = Vec::new();
    let mut unlisted: FxHashSet<PathBuf> = FxHashSet::default();
    for file in staged {
        if let Some(parent) = file.destination.parent() {
            add(&mut directories, parent);
        }
    }
    for (name, input) in inputs {
        match input {
            Staged::File(_) => {}
            Staged::Tree(tree) => {
                owned.push(name.clone());
                owned.extend(
                    tree.directories
                        .iter()
                        .map(|directory| name.join(directory)),
                );
            }
            Staged::Symlink(_) => {
                if let Some(parent) = name.parent() {
                    owned.push(parent.to_path_buf());
                    unlisted.insert(parent.to_path_buf());
                }
            }
            Staged::SourceDirectory(_) if link_source_directories => {
                if let Some(parent) = name.parent() {
                    owned.push(parent.to_path_buf());
                    unlisted.insert(parent.to_path_buf());
                }
            }
            Staged::SourceDirectory(directory) => {
                owned.push(name.clone());
                owned.extend(
                    directory
                        .directories
                        .iter()
                        .map(|directory| name.join(directory)),
                );
                for link in directory.dangling.keys() {
                    if let Some(parent) = name.join(link).parent() {
                        unlisted.insert(parent.to_path_buf());
                    }
                }
            }
        }
    }
    for directory in &owned {
        add(&mut directories, directory);
    }
    let mut states = FxHashMap::default();
    for directory in &directories {
        let stat = if directory.as_os_str().is_empty() {
            rustix::fs::fstat(&root)
        } else {
            statat(&root, *directory, AtFlags::SYMLINK_NOFOLLOW)
        };
        if let Ok(stat) = stat
            && FileType::from_raw_mode(stat.st_mode) == FileType::Directory
        {
            states.insert(directory.to_path_buf(), DirectoryState::of(&stat));
        }
    }
    match coarse_now() {
        Some(now) => states.retain(|_, state| state.ctime < now),
        None => states.clear(),
    }
    let mut listings: FxHashMap<PathBuf, Vec<(OsString, FileType)>> = FxHashMap::default();
    let mut entry = |path: &Path, kind: FileType| {
        if let (Some(parent), Some(name)) = (path.parent(), path.file_name())
            && states.contains_key(parent)
            && !unlisted.contains(parent)
        {
            listings
                .entry(parent.to_path_buf())
                .or_default()
                .push((name.to_os_string(), kind));
        }
    };
    for directory in &directories {
        if !directory.as_os_str().is_empty() {
            entry(directory, FileType::Directory);
        }
    }
    for file in staged {
        let kind = match file.tree_link {
            Some(_) => FileType::Symlink,
            None => FileType::RegularFile,
        };
        entry(&file.destination, kind);
    }
    // A recorded directory with nothing in it lists nothing.
    for directory in states.keys() {
        if !unlisted.contains(directory) && !listings.contains_key(directory) {
            listings.insert(directory.clone(), Vec::new());
        }
    }
    Recorded { states, listings }
}

/// The clock file change times are stamped from (Linux: the coarse real
/// time clock); None elsewhere (nothing is trusted then).
#[cfg(target_os = "linux")]
#[allow(clippy::useless_conversion)]
fn coarse_now() -> Option<(i64, i64)> {
    let now = rustix::time::clock_gettime(rustix::time::ClockId::RealtimeCoarse);
    Some((
        i64::try_from(now.tv_sec).ok()?,
        i64::try_from(now.tv_nsec).ok()?,
    ))
}

#[cfg(not(target_os = "linux"))]
fn coarse_now() -> Option<(i64, i64)> {
    None
}

/// A stashed work directory (its `execroot/`) and what staging placed in
/// its execution root, by destination.
pub(super) struct Stash {
    pub(super) work: PathBuf,
    placed: FxHashMap<PathBuf, Placed>,
    /// Its directories as staging left them ([`record`]).
    recorded: Recorded,
    /// The action's owner (its id up to `#`: the target or aspect).
    owner: String,
}

/// An action id's owner (up to `#`) and package (up to `:`).
fn owner_of(id: &str) -> (&str, &str) {
    let owner = id.split('#').next().unwrap_or(id);
    (owner, owner.split(':').next().unwrap_or(owner))
}

/// `PERF-STAGE reuse` counts: execution roots reused, entries kept,
/// entries removed (crate::action `report_staging`).
pub(super) static COUNTS: [AtomicU64; 3] = [const { AtomicU64::new(0) }; 3];

/// `PERF-STAGE reuse` misses: no stash of the mnemonic to take, a
/// reconciliation that failed (the stash was removed), a stash refused by
/// [`put`] (the scope ended or enough are stashed; its other entries could
/// not be removed).
pub(super) static MISSES: [AtomicU64; 4] = [const { AtomicU64::new(0) }; 4];

/// `PERF-STAGE reuse` directories: reconciled, of those trusted
/// ([`Context::unchanged`]).
pub(super) static DIRECTORIES: [AtomicU64; 2] = [const { AtomicU64::new(0) }; 2];
pub(super) const MISS_EMPTY: usize = 0;
pub(super) const MISS_RECONCILE: usize = 1;
pub(super) const MISS_PUT_REFUSED: usize = 2;
pub(super) const MISS_PUT_ENTRIES: usize = 3;

/// The kept entries of a reconciled execution root, by destination.
pub(super) type Kept = FxHashMap<PathBuf, Placed>;

/// Stashes per mnemonic at most (a scheduler runs `--jobs` actions at once).
const MAX_PER_MNEMONIC: usize = 64;

/// The caches a scheduler is executing a graph on, with how many.
fn scopes() -> &'static Mutex<FxHashMap<PathBuf, usize>> {
    static SCOPES: OnceLock<Mutex<FxHashMap<PathBuf, usize>>> = OnceLock::new();
    SCOPES.get_or_init(Default::default)
}

/// The stashes by cache and mnemonic.
type Stashes = FxHashMap<(PathBuf, String), Vec<Stash>>;

fn stashes() -> &'static Mutex<Stashes> {
    static STASHES: OnceLock<Mutex<Stashes>> = OnceLock::new();
    STASHES.get_or_init(Default::default)
}

/// Reuse is on for `cache_root` while a scheduler executes a graph there,
/// unless `BAZEL_RS_REUSE_EXECROOTS=0`.
pub(super) fn enabled(cache_root: &Path) -> bool {
    static OFF: std::sync::LazyLock<bool> = std::sync::LazyLock::new(|| {
        // snapshot-unrecorded: an execution switch; what an action sees is the same either way.
        std::env::var_os("BAZEL_RS_REUSE_EXECROOTS").is_some_and(|value| value == "0")
    });
    !*OFF && directory_mode().is_some() && in_scope(cache_root)
}

/// Whether a scheduler is executing a graph on `cache_root`.
fn in_scope(cache_root: &Path) -> bool {
    scopes()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(cache_root)
        .is_some_and(|count| *count > 0)
}

/// The mode of a directory this process creates (0777 without the umask,
/// read from /proc/self/status: setting the umask to read it would race
/// with other threads creating files).
fn directory_mode() -> Option<u32> {
    static MODE: OnceLock<Option<u32>> = OnceLock::new();
    *MODE.get_or_init(|| {
        let status = fs::read_to_string("/proc/self/status").ok()?;
        let umask = status
            .lines()
            .find_map(|line| line.strip_prefix("Umask:"))
            .and_then(|value| u32::from_str_radix(value.trim(), 8).ok())?;
        Some(0o777 & !umask)
    })
}

/// While alive, execution roots of `cache_root` are reused; the last scope
/// of a cache removes its stashes.
pub struct StashScope(PathBuf);

impl StashScope {
    pub fn enter(cache_root: &Path) -> Self {
        *scopes()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .entry(cache_root.to_path_buf())
            .or_default() += 1;
        StashScope(cache_root.to_path_buf())
    }
}

impl Drop for StashScope {
    fn drop(&mut self) {
        let last = {
            let mut scopes = scopes().lock().unwrap_or_else(|e| e.into_inner());
            let count = scopes.entry(self.0.clone()).or_default();
            *count = count.saturating_sub(1);
            let last = *count == 0;
            if last {
                scopes.remove(&self.0);
            }
            last
        };
        if !last {
            return;
        }
        let removed: Vec<Stash> = {
            let mut stashes = stashes().lock().unwrap_or_else(|e| e.into_inner());
            let keys: Vec<(PathBuf, String)> = stashes
                .keys()
                .filter(|(cache, _)| *cache == self.0)
                .cloned()
                .collect();
            keys.into_iter()
                .filter_map(|key| stashes.remove(&key))
                .flatten()
                .collect()
        };
        let mut clock = std::time::Instant::now();
        for stash in removed {
            let _ = remove_directory(&stash.work);
        }
        staging_part(PART_REMOVE, &mut clock);
        release_cache_work(&self.0);
    }
}

/// A stashed execution root of `mnemonic` actions of `cache_root`, the
/// latest of the action `id`'s owner, else of its package, else the latest
/// (the likeliest to hold the same inputs).
pub(super) fn take(cache_root: &Path, mnemonic: &str, id: &str) -> Option<Stash> {
    let mut stashes = stashes().lock().unwrap_or_else(|e| e.into_inner());
    let Some(list) = stashes.get_mut(&(cache_root.to_path_buf(), mnemonic.to_owned())) else {
        MISSES[MISS_EMPTY].fetch_add(1, Ordering::Relaxed);
        return None;
    };
    if list.is_empty() {
        MISSES[MISS_EMPTY].fetch_add(1, Ordering::Relaxed);
    }
    let (owner, package) = owner_of(id);
    let position = list
        .iter()
        .rposition(|stash| stash.owner == owner)
        .or_else(|| {
            list.iter()
                .rposition(|stash| owner_of(&stash.owner).1 == package)
        })
        .or_else(|| list.len().checked_sub(1))?;
    Some(list.remove(position))
}

/// Stash the work directory `work` of a successful action (its execution
/// root holding `staged`); `false` when it is not kept (the caller removes
/// it): the scope ended, enough are stashed, or its other entries (the
/// logs) cannot be removed.
pub(super) fn put(
    cache_root: &Path,
    mnemonic: &str,
    id: &str,
    work: &Path,
    staged: &[StagedFile],
    recorded: Recorded,
) -> bool {
    if !enabled(cache_root) {
        return false;
    }
    // Everything but the execution root goes (the logs, a sandbox's files).
    let Ok(entries) = fs::read_dir(work) else {
        MISSES[MISS_PUT_ENTRIES].fetch_add(1, Ordering::Relaxed);
        return false;
    };
    for entry in entries {
        let Ok(entry) = entry else {
            MISSES[MISS_PUT_ENTRIES].fetch_add(1, Ordering::Relaxed);
            return false;
        };
        if entry.file_name() == "execroot" {
            continue;
        }
        let removed = match entry.file_type() {
            Ok(kind) if kind.is_dir() => remove_directory(&entry.path()),
            Ok(_) => fs::remove_file(entry.path()),
            Err(error) => Err(error),
        };
        if removed.is_err() {
            MISSES[MISS_PUT_ENTRIES].fetch_add(1, Ordering::Relaxed);
            return false;
        }
    }
    let placed = staged
        .iter()
        .map(|file| {
            let placed = match file.tree_link {
                Some(_) => Placed::TreeLink(file.blob.clone()),
                None => Placed::Blob {
                    blob: file.blob.clone(),
                    state: file.state,
                },
            };
            (file.destination.clone(), placed)
        })
        .collect();
    let mut stashes = stashes().lock().unwrap_or_else(|e| e.into_inner());
    // The scope again, under the stashes' lock: the last scope's end
    // decrements its count before it takes this lock to remove the
    // stashes, so a stash pushed here while the count is positive is one
    // that end removes, and none is pushed after it.
    if !in_scope(cache_root) {
        MISSES[MISS_PUT_REFUSED].fetch_add(1, Ordering::Relaxed);
        return false;
    }
    let list = stashes
        .entry((cache_root.to_path_buf(), mnemonic.to_owned()))
        .or_default();
    if list.len() >= MAX_PER_MNEMONIC {
        MISSES[MISS_PUT_REFUSED].fetch_add(1, Ordering::Relaxed);
        return false;
    }
    list.push(Stash {
        work: work.to_path_buf(),
        placed,
        recorded,
        owner: owner_of(id).0.to_owned(),
    });
    true
}

/// What staging would place at a destination now: a symlink to this tree
/// file, or a hardlink of this store file.
enum Wanted {
    TreeLink(PathBuf),
    Blob(PathBuf),
}

/// What staging places for `inputs` (crate::action `stage_inputs`), by
/// destination; the store file of each hardlink by its distinct-inode
/// ordinal, computed in staging's order. A source directory staged as a
/// symlink to it (`link_source_directories`) places no file: whatever is
/// at its destination goes (staging links it again), and nothing below
/// that symlink is ever looked at.
fn wanted(
    cas: &Path,
    root: &Path,
    inputs: &BTreeMap<PathBuf, Staged>,
    link_source_directories: bool,
) -> FxHashMap<PathBuf, Wanted> {
    fn file<'a>(
        (cas, root): (&Path, &Path),
        destination: PathBuf,
        input: &'a Input,
        distinct: &mut DistinctInodes<'a>,
        wanted: &mut FxHashMap<PathBuf, Wanted>,
    ) {
        let placed = if repository_tree_file(root, &input.path) {
            Wanted::TreeLink(input.path.clone())
        } else {
            Wanted::Blob(match distinct.ordinal(input) {
                0 => blob_path(cas, input),
                ordinal => copy_path(cas, input, ordinal),
            })
        };
        wanted.insert(destination, placed);
    }
    let mut wanted = FxHashMap::default();
    let mut distinct = DistinctInodes::default();
    for (name, staged) in inputs {
        match staged {
            Staged::File(input) => {
                file((cas, root), name.clone(), input, &mut distinct, &mut wanted);
            }
            Staged::Tree(tree) => {
                for (path, input) in &tree.files {
                    file(
                        (cas, root),
                        name.join(path),
                        input,
                        &mut distinct,
                        &mut wanted,
                    );
                }
            }
            Staged::SourceDirectory(_) if link_source_directories => {}
            Staged::SourceDirectory(directory) => {
                for (path, input) in &directory.files {
                    file(
                        (cas, root),
                        name.join(path),
                        input,
                        &mut distinct,
                        &mut wanted,
                    );
                }
            }
            Staged::Symlink(_) => {}
        }
    }
    wanted
}

/// Make the stashed execution root `execroot` hold only what staging
/// `inputs` keeps of it (see the module): the kept entries, by destination.
/// An error leaves it in an unknown state (the caller removes it).
pub(super) fn reconcile(
    stash: Stash,
    execroot: &Path,
    cas: &Path,
    root: &Path,
    inputs: &BTreeMap<PathBuf, Staged>,
    link_source_directories: bool,
) -> io::Result<Kept> {
    let mode = directory_mode().ok_or_else(|| io::Error::other("no directory mode"))?;
    let mut clock = std::time::Instant::now();
    let wanted = wanted(cas, root, inputs, link_source_directories);
    // Every directory an input file lies below.
    let mut needed: FxHashSet<PathBuf> = FxHashSet::default();
    for destination in wanted.keys() {
        let mut parent = destination.parent();
        while let Some(directory) = parent.filter(|directory| !directory.as_os_str().is_empty()) {
            if !needed.insert(directory.to_path_buf()) {
                break;
            }
            parent = directory.parent();
        }
    }
    staging_part(PART_RECONCILE_PLAN, &mut clock);
    let mut placed = stash.placed;
    let mut kept = Kept::default();
    let directory = open_directory_at(None, execroot)?;
    let context = Context {
        wanted: &wanted,
        needed: &needed,
        mode,
        recorded: &stash.recorded,
    };
    let trusted = context.unchanged(Path::new(""), &rustix::fs::fstat(&directory)?);
    reconcile_directory(
        &directory,
        execroot,
        Path::new(""),
        &context,
        trusted,
        &mut placed,
        &mut kept,
    )?;
    COUNTS[0].fetch_add(1, Ordering::Relaxed);
    COUNTS[1].fetch_add(kept.len() as u64, Ordering::Relaxed);
    Ok(kept)
}

struct Context<'a> {
    wanted: &'a FxHashMap<PathBuf, Wanted>,
    needed: &'a FxHashSet<PathBuf>,
    mode: u32,
    recorded: &'a Recorded,
}

impl Context<'_> {
    /// Whether the directory at `path` (inspected: `stat`) is in the state
    /// [`record`] recorded: its entries are the ones staging left.
    fn unchanged(&self, path: &Path, stat: &rustix::fs::Stat) -> bool {
        self.recorded
            .states
            .get(path)
            .is_some_and(|state| *state == DirectoryState::of(stat))
    }

    /// The entries of the open `directory`.
    fn read(
        &self,
        directory: &std::os::fd::OwnedFd,
    ) -> io::Result<Vec<(OsString, rustix::fs::FileType)>> {
        let mut names = Vec::new();
        for entry in rustix::fs::Dir::read_from(directory)? {
            let entry = entry?;
            let name = entry.file_name().to_bytes();
            if name == b"." || name == b".." {
                continue;
            }
            names.push((
                std::ffi::OsStr::from_bytes(name).to_os_string(),
                entry.file_type(),
            ));
        }
        Ok(names)
    }
}

/// `PERF-STAGE reuse` directories whose recorded listing was used.
pub(super) static LISTED: AtomicU64 = AtomicU64::new(0);

fn open_directory_at(
    parent: Option<&std::os::fd::OwnedFd>,
    path: &Path,
) -> io::Result<std::os::fd::OwnedFd> {
    use rustix::fs::{CWD, Mode, OFlags, openat};
    let flags = OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC;
    Ok(match parent {
        Some(parent) => openat(parent, path, flags, Mode::empty())?,
        None => openat(CWD, path, flags, Mode::empty())?,
    })
}

/// Whether `stat` shows the file in `state`.
fn stat_matches(stat: Option<&rustix::fs::Stat>, state: &InodeState) -> bool {
    stat.is_some_and(|stat| stat_state(stat) == *state)
}

/// The state [`inode_state`] gives for `stat`.
#[cfg_attr(not(target_os = "linux"), allow(clippy::useless_conversion))]
fn stat_state(stat: &rustix::fs::Stat) -> InodeState {
    InodeState {
        inode: stat.st_ino,
        size: u64::try_from(stat.st_size).unwrap_or(u64::MAX),
        mtime: (
            stat.st_mtime,
            i64::try_from(stat.st_mtime_nsec).unwrap_or(i64::MAX),
        ),
        mode: crate::platform::stat_mode(stat) & 0o7777,
    }
}

/// A directory being reconciled: the execution root's descriptor and the
/// directory's path relative to it; the directory itself is opened only
/// when it is read (listed, or an entry inspected) — on an overlay
/// filesystem opening a directory costs about ten `stat`s of a path.
struct At<'a> {
    root: &'a std::os::fd::OwnedFd,
    relative: &'a Path,
    opened: Option<std::os::fd::OwnedFd>,
}

impl At<'_> {
    /// The directory's descriptor, opened without following a link.
    fn fd(&mut self) -> io::Result<&std::os::fd::OwnedFd> {
        if self.relative.as_os_str().is_empty() {
            return Ok(self.root);
        }
        if self.opened.is_none() {
            self.opened = Some(open_directory_at(Some(self.root), self.relative)?);
        }
        Ok(self.opened.as_ref().expect("opened above"))
    }

    /// `name` in the directory, for a call relative to [`Self::base`]: the
    /// name itself in an opened directory (or the root), else its path
    /// relative to the root.
    fn name(&self, name: &std::ffi::OsStr) -> PathBuf {
        if self.relative.as_os_str().is_empty() || self.opened.is_some() {
            PathBuf::from(name)
        } else {
            self.relative.join(name)
        }
    }

    /// The descriptor [`Self::name`]'s paths are relative to.
    fn base(&self) -> &std::os::fd::OwnedFd {
        self.opened.as_ref().unwrap_or(self.root)
    }
}

/// Reconciles the directory `relative` to the root (`absolute`), whose
/// state is `stat`. `trusted`: it is in the state staging recorded
/// ([`Context::unchanged`]), so each entry staging placed is still that
/// very link or inode — its recorded listing stands for reading it, a kept
/// link's text is not read again and a kept file is not inspected here
/// (staging inspects its store file, the same inode, crate::action
/// `stage_file`); subdirectories are inspected by path.
fn reconcile_directory(
    root: &std::os::fd::OwnedFd,
    absolute: &Path,
    relative: &Path,
    context: &Context<'_>,
    trusted: bool,
    placed: &mut FxHashMap<PathBuf, Placed>,
    kept: &mut Kept,
) -> io::Result<()> {
    use rustix::fs::{AtFlags, FileType, readlinkat, statat, unlinkat};
    DIRECTORIES[0].fetch_add(1, Ordering::Relaxed);
    if trusted {
        DIRECTORIES[1].fetch_add(1, Ordering::Relaxed);
    }
    let mut at = At {
        root,
        relative,
        opened: None,
    };
    let entries = match trusted
        .then(|| context.recorded.listings.get(relative))
        .flatten()
    {
        Some(listing) => {
            LISTED.fetch_add(1, Ordering::Relaxed);
            listing.clone()
        }
        None => context.read(at.fd()?)?,
    };
    for (name, kind) in entries {
        let path = relative.join(&name);
        // The listing's type, else a stat's.
        let kind = match kind {
            FileType::Unknown => {
                FileType::from_raw_mode(statat(at.fd()?, &name, AtFlags::SYMLINK_NOFOLLOW)?.st_mode)
            }
            kind => kind,
        };
        let keep = match (kind, placed.remove(&path), context.wanted.get(&path)) {
            (FileType::Symlink, Some(Placed::TreeLink(target)), Some(Wanted::TreeLink(wanted)))
                if target == *wanted =>
            {
                let same = trusted || {
                    let text = readlinkat(at.fd()?, &name, Vec::new())?;
                    text.as_bytes() == wanted.as_os_str().as_bytes()
                };
                same.then_some(Placed::TreeLink(target))
            }
            (
                FileType::RegularFile,
                Some(Placed::Blob { blob, state }),
                Some(Wanted::Blob(wanted)),
            ) if blob == *wanted
                && (trusted
                    || stat_matches(
                        Some(&statat(at.fd()?, &name, AtFlags::SYMLINK_NOFOLLOW)?),
                        &state,
                    )) =>
            {
                Some(Placed::Blob { blob, state })
            }
            (FileType::Directory, _, _) if context.needed.contains(&path) => {
                // Inspected without following a link: its type, mode and
                // change time.
                let stat = statat(at.base(), at.name(&name), AtFlags::SYMLINK_NOFOLLOW)?;
                if FileType::from_raw_mode(stat.st_mode) == FileType::Directory
                    && crate::platform::stat_mode(&stat) & 0o7777 == context.mode
                {
                    let trusted = context.unchanged(&path, &stat);
                    reconcile_directory(
                        root,
                        &absolute.join(&name),
                        &path,
                        context,
                        trusted,
                        placed,
                        kept,
                    )?;
                } else {
                    remove_directory(&absolute.join(&name))?;
                }
                continue;
            }
            (FileType::Directory, _, _) => {
                remove_directory(&absolute.join(&name))?;
                continue;
            }
            _ => None,
        };
        match keep {
            Some(placed) => {
                kept.insert(path, placed);
            }
            None => {
                COUNTS[2].fetch_add(1, Ordering::Relaxed);
                unlinkat(at.base(), at.name(&name), AtFlags::empty())?;
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A stash put while the cache's last scope ends is either refused or
    /// removed by that end: none outlives the scope.
    #[test]
    fn no_stash_outlives_the_last_scope() {
        let root = crate::test_temp::root().join(format!("reuse-race-{}", std::process::id()));
        let _ = remove_directory(&root);
        fs::create_dir_all(&root).unwrap();
        let cache = root.join("cache");
        for round in 0..200 {
            let work = root.join(format!("work-{round}"));
            fs::create_dir_all(work.join("execroot")).unwrap();
            let scope = StashScope::enter(&cache);
            let put = std::thread::scope(|threads| {
                let putting = threads
                    .spawn(|| put(&cache, "Race", "//p:t#0", &work, &[], Recorded::default()));
                drop(scope);
                putting.join().unwrap()
            });
            assert!(
                !stashes()
                    .lock()
                    .unwrap()
                    .get(&(cache.clone(), "Race".to_owned()))
                    .is_some_and(|list| !list.is_empty()),
                "round {round}: a stash outlived its scope (put: {put})"
            );
            if put {
                assert!(
                    !work.exists(),
                    "round {round}: the stashed root was not removed"
                );
            } else {
                remove_directory(&work).unwrap();
            }
        }
        let _ = remove_directory(&root);
    }
}
