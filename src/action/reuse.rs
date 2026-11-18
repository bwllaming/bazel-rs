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

/// What staging placed at one destination of a stashed execution root.
pub(super) enum Placed {
    /// A symlink whose text is this repository tree file's path.
    TreeLink(PathBuf),
    /// A hardlink of this store file, in this state (as staged, and as
    /// checked after the process).
    Blob { blob: PathBuf, state: InodeState },
}

/// A stashed work directory (its `execroot/`) and what staging placed in
/// its execution root, by destination.
pub(super) struct Stash {
    pub(super) work: PathBuf,
    placed: FxHashMap<PathBuf, Placed>,
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
    let list = stashes.get_mut(&(cache_root.to_path_buf(), mnemonic.to_owned()))?;
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
) -> bool {
    if !enabled(cache_root) {
        return false;
    }
    // Everything but the execution root goes (the logs, a sandbox's files).
    let Ok(entries) = fs::read_dir(work) else {
        return false;
    };
    for entry in entries {
        let Ok(entry) = entry else {
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
        return false;
    }
    let list = stashes
        .entry((cache_root.to_path_buf(), mnemonic.to_owned()))
        .or_default();
    if list.len() >= MAX_PER_MNEMONIC {
        return false;
    }
    list.push(Stash {
        work: work.to_path_buf(),
        placed,
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
/// ordinal, computed in staging's order.
fn wanted(
    cas: &Path,
    root: &Path,
    inputs: &BTreeMap<PathBuf, Staged>,
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
) -> io::Result<Kept> {
    let mode = directory_mode().ok_or_else(|| io::Error::other("no directory mode"))?;
    let wanted = wanted(cas, root, inputs);
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
    let mut placed = stash.placed;
    let mut kept = Kept::default();
    let directory = open_directory_at(None, execroot)?;
    reconcile_directory(
        &directory,
        execroot,
        Path::new(""),
        &Context {
            wanted: &wanted,
            needed: &needed,
            mode,
        },
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
}

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
fn stat_state(stat: &rustix::fs::Stat) -> InodeState {
    InodeState {
        inode: stat.st_ino,
        size: u64::try_from(stat.st_size).unwrap_or(u64::MAX),
        mtime: (
            stat.st_mtime,
            i64::try_from(stat.st_mtime_nsec).unwrap_or(i64::MAX),
        ),
        mode: stat.st_mode & 0o7777,
    }
}

fn reconcile_directory(
    directory: &std::os::fd::OwnedFd,
    absolute: &Path,
    relative: &Path,
    context: &Context<'_>,
    placed: &mut FxHashMap<PathBuf, Placed>,
    kept: &mut Kept,
) -> io::Result<()> {
    use rustix::fs::{AtFlags, Dir, FileType, readlinkat, statat, unlinkat};
    let mut names = Vec::new();
    {
        for entry in Dir::read_from(directory)? {
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
    }
    for (name, kind) in names {
        let path = relative.join(&name);
        // A symlink's type is all it needs from the directory listing; a
        // file's state and a directory's mode take a stat.
        let stat = match kind {
            FileType::Symlink => None,
            _ => Some(statat(directory, &name, AtFlags::SYMLINK_NOFOLLOW)?),
        };
        let kind = match (kind, &stat) {
            (FileType::Unknown, Some(stat)) => FileType::from_raw_mode(stat.st_mode),
            (kind, _) => kind,
        };
        let keep = match (kind, placed.remove(&path), context.wanted.get(&path)) {
            (FileType::Symlink, Some(Placed::TreeLink(target)), Some(Wanted::TreeLink(wanted)))
                if target == *wanted =>
            {
                let text = readlinkat(directory, &name, Vec::new())?;
                (text.as_bytes() == wanted.as_os_str().as_bytes())
                    .then_some(Placed::TreeLink(target))
            }
            (
                FileType::RegularFile,
                Some(Placed::Blob { blob, state }),
                Some(Wanted::Blob(wanted)),
            ) if blob == *wanted && stat_matches(stat.as_ref(), &state) => {
                Some(Placed::Blob { blob, state })
            }
            (FileType::Directory, _, _) if context.needed.contains(&path) => {
                if stat
                    .as_ref()
                    .is_some_and(|stat| stat.st_mode & 0o7777 == context.mode)
                {
                    let child = open_directory_at(Some(directory), Path::new(&name))?;
                    reconcile_directory(
                        &child,
                        &absolute.join(&name),
                        &path,
                        context,
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
                unlinkat(directory, &name, AtFlags::empty())?;
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
                let putting = threads.spawn(|| put(&cache, "Race", "//p:t#0", &work, &[]));
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
