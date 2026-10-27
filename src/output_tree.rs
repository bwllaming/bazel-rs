//! The execution root's output tree (docs/INFO.md): `bazel info
//! execution_root`, `output_path` and `bazel-bin`/`bazel-genfiles`/
//! `bazel-testlogs` name directories below `<state>/execroot/_main`, the
//! native counterpart of Bazel's `<output_base>/execroot/<workspace>`
//! (BlazeDirectories.getExecRoot; the main repository's directory is `_main`
//! under Bzlmod). `<state>` is the workspace's state directory in the
//! cache, `<cache>/worktrees/<name>-<hash>` (crate::worktree,
//! docs/WORKTREES.md): the git worktrees sharing one cache each have their
//! own output tree, so one worktree's build never relinks another's
//! `bazel-bin` (the outputs themselves stay shared in the action cache).
//!
//! Native action outputs live in per-action cache entries
//! (`<cache>/actions/<key>/outputs/<exec path>`, docs/ACTION_OUTPUTS.md),
//! not in one tree. After an executing `build`, `test` or `run`
//! ([`publish`]) every output of the request's actions — executed or a
//! cache hit — is linked at its exec path below the execution root, so
//! `$(bazel-rs info bazel-bin)/pkg/tool` names the built file, as with
//! Bazel's output tree:
//!
//! * a regular file is a hardlink of the published output (one inode: the
//!   outputs are read-only, `0555`), so `/proc/self/exe` of a binary
//!   started through the tree is the tree path and its `.runfiles` sibling
//!   resolves; a symlink is the fallback when the hardlink fails (another
//!   filesystem, the inode's link limit);
//! * a directory output (a tree artifact) and an output symlink are a
//!   symlink to the published path;
//! * a runfiles tree (`Transform::RunfilesTree`, whose only declared output
//!   is `<exe>.runfiles/MANIFEST`) links the whole `<exe>.runfiles`
//!   directory.
//!
//! An entry already linking the same output is left alone (one `lstat` or
//! `readlink`), anything else at the path is replaced atomically (a
//! temporary link renamed over it). Entries of earlier builds stay until a
//! later build replaces them, like Bazel's output tree. Failing to update
//! the tree never fails the build (the caller warns).

use crate::action::ActionResult;
use crate::dir_stamps::DirectoryStamp;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::ffi::{OsStr, OsString};
use std::fs;
use std::io::ErrorKind;
use std::os::unix::ffi::OsStrExt as _;
use std::os::unix::fs::MetadataExt as _;
use std::path::{Component, Path, PathBuf};
use std::time::SystemTime;

/// The main repository's execution root directory name under Bzlmod
/// (RepositoryUtils.WORKSPACE_SYMLINK_NAME, bazel/repository/RepositoryUtils.java:41).
pub const MAIN_DIRECTORY: &str = "_main";

/// `<state>/execroot/_main` of this process's workspace
/// (crate::worktree::state_directory; without an installed workspace,
/// `<cache>/execroot/_main`).
pub fn execution_root(cache: &Path) -> PathBuf {
    execution_root_below(&crate::worktree::state_directory(cache))
}

/// `<output_base>/execroot/_main` (BlazeDirectories.getExecRoot), for the
/// native output base: a workspace's state directory.
pub fn execution_root_below(output_base: &Path) -> PathBuf {
    output_base.join("execroot").join(MAIN_DIRECTORY)
}

/// `<state>/execroot/_main/bazel-out` of this process's workspace
/// ([`execution_root`]).
pub fn output_path(cache: &Path) -> PathBuf {
    execution_root(cache).join("bazel-out")
}

/// `<output_base>/execroot/_main/bazel-out` (BlazeDirectories.getOutputPath).
pub fn output_path_below(output_base: &Path) -> PathBuf {
    execution_root_below(output_base).join("bazel-out")
}

/// The outcome of one [`publish`].
#[derive(Debug, Default)]
pub struct Published {
    /// Entries created or replaced.
    pub linked: usize,
    /// Entries that already linked their output.
    pub unchanged: usize,
    /// The first failure (the remaining outputs are still linked).
    pub error: Option<String>,
}

/// Link every output of `results` at its exec path below the execution root
/// of `cache`. `runfiles_trees` are the ids of the request's
/// `Transform::RunfilesTree` actions.
///
/// O(changed), not O(outputs): the tree keeps a record of what it linked
/// ([`Record`]) and of its directories' stamps (crate::dir_stamps). A
/// directory that still shows its stamp, reached from the root through real
/// directories only, still holds exactly the entries the record lists (an entry is replaced only by
/// a rename or removal in its directory), so an output the record lists
/// there with the same source, and for a hardlink the same inode the cache
/// check found the published output to be (crate::action::published_kind),
/// is left alone without inspecting it. Everything else takes the
/// per-entry check. A directory is stamped only when it was settled
/// (crate::digest_memo::RACY_MARGIN) and unchanged while this build
/// published, so the directories a build changed are checked once more by
/// the next one.
pub fn publish(
    cache: &Path,
    results: &BTreeMap<String, ActionResult>,
    runfiles_trees: &HashSet<String>,
) -> Published {
    let start = SystemTime::now();
    let root = execution_root(cache);
    let actions = cache.join("actions");
    let mut tree = Tree {
        root: root.clone(),
        verified: HashSet::new(),
        published: Published::default(),
    };
    // (exec path, source) in result order.
    let mut wanted: Vec<(PathBuf, PathBuf)> = Vec::new();
    for (id, result) in results {
        let runfiles_tree = runfiles_trees.contains(id);
        for output in &result.outputs {
            let Some(exec) = exec_path(&actions, output) else {
                continue;
            };
            if runfiles_tree {
                // `<exe>.runfiles/MANIFEST`: the whole directory.
                if let (Some(source), Some(exec)) = (output.parent(), exec.parent()) {
                    wanted.push((exec.to_path_buf(), source.to_path_buf()));
                }
            } else {
                wanted.push((exec, output.clone()));
            }
        }
    }
    let record_path = record_path(&root);
    let mut changed = false;
    // Every directory that holds a wanted entry, with its ancestors below
    // the root, and the stamps they show now.
    let mut directories: BTreeSet<&Path> = BTreeSet::new();
    for (exec, _) in &wanted {
        for ancestor in exec.ancestors().skip(1) {
            if ancestor.as_os_str().is_empty() || !directories.insert(ancestor) {
                break;
            }
        }
    }
    let directories: Vec<&Path> = directories.into_iter().collect();
    // Only these directories (and the root) are looked up or changed below:
    // the record keeps every other one as it is.
    let mut record = Record::read_selected(
        &record_path,
        std::iter::once(Path::new(""))
            .chain(directories.iter().copied())
            .map(|directory| directory.as_os_str().as_bytes())
            .collect(),
    );
    let root_state = crate::dir_stamps::open_directory(None, &root)
        .ok()
        .and_then(|file| file.metadata().ok())
        .and_then(|metadata| DirectoryStamp::of(&metadata));
    let states: HashMap<&Path, DirectoryStamp> = match &root_state {
        Some(_) => directory_states(&root, &directories),
        None => HashMap::new(),
    };
    let state = |directory: &Path| {
        if directory.as_os_str().is_empty() {
            root_state.as_ref()
        } else {
            states.get(directory)
        }
    };
    // Clean: the directory shows its recorded stamp. The walk reached it
    // through real directories only (no symlink at any component), and a
    // directory moved elsewhere has a new change time, so its ancestors
    // need not be unchanged themselves (`bazel-out/` holds the workspace
    // status files, which every build links anew).
    let clean: HashSet<&Path> = std::iter::once(Path::new(""))
        .chain(directories.iter().copied())
        .filter(|directory| {
            let current = state(directory);
            current.is_some()
                && record.directories.get(*directory).map(|entry| &entry.stamp) == current
        })
        .collect();
    // The entries this build checked one by one (found or made), by
    // directory.
    let mut entries: HashMap<&Path, BTreeMap<OsString, Entry>> = HashMap::new();
    for (exec, source) in &wanted {
        let parent = exec.parent().unwrap_or(Path::new(""));
        let Some(name) = exec.file_name() else {
            continue;
        };
        // A remote-only output (Build without the Bytes) has no file: like
        // Bazel's output tree, nothing is at its path (an earlier build's
        // entry goes).
        if crate::remote_outputs::get(source).is_some() {
            let _ = fs::remove_file(root.join(exec));
            continue;
        }
        if clean.contains(parent)
            && let Some(kind) = crate::action::published_kind(source)
            && record
                .directories
                .get(parent)
                .and_then(|directory| directory.entries.get(name))
                .is_some_and(|entry| {
                    entry.kind == EntryKind::from(kind) && entry.source == source_hash(source)
                })
        {
            tree.published.unchanged += 1;
            continue;
        }
        match tree.link(source, &root.join(exec)) {
            Ok(Some(kind)) => {
                entries.entry(parent).or_default().insert(
                    name.to_owned(),
                    Entry {
                        source: source_hash(source),
                        kind,
                    },
                );
            }
            Ok(None) => {}
            Err(error) => {
                tree.published.error.get_or_insert(error);
            }
        }
    }
    // The next record: clean directories keep their stamps and entries;
    // every other directory this build looked at is stamped
    // when it was settled and unchanged since it was looked at, with the
    // entries found in it now. Directories of earlier builds' outputs keep
    // theirs.
    for directory in std::iter::once(Path::new("")).chain(directories.iter().copied()) {
        if clean.contains(directory) {
            // Entries found unchanged in a clean directory join its record
            // (one it lacked); one this build replaced changed the
            // directory, whose stamp then no longer matches.
            if let Some(found) = entries.remove(directory)
                && let Some(recorded) = record.directories.get_mut(directory)
            {
                for (name, entry) in found {
                    if recorded.entries.get(&name) != Some(&entry) {
                        recorded.entries.insert(name, entry);
                        changed = true;
                    }
                }
            }
            continue;
        }
        let before = state(directory);
        let after = DirectoryStamp::at(&root.join(directory));
        match (before, after) {
            (Some(before), Some(after)) if *before == after && after.settled(start) => {
                let recorded = RecordedDirectory {
                    stamp: after,
                    entries: entries.remove(directory).unwrap_or_default(),
                };
                if record.directories.get(directory) != Some(&recorded) {
                    record.directories.insert(directory.to_path_buf(), recorded);
                    changed = true;
                }
            }
            _ => {
                changed |= record.directories.remove(directory).is_some();
            }
        }
    }
    if changed {
        // Failing to keep the record only costs the next build its shortcut.
        let _ = record.write(&record_path);
    }
    tree.published
}

/// The stamps the `directories` (relative to `root`, a real directory)
/// show now; a directory missing from the map is missing, no real
/// directory, or below such a path. Walked on several threads, each over a
/// contiguous part of the directories in component order (with the
/// ancestors of its first one), each lookup relative to its parent's
/// descriptor (crate::dir_stamps::walk).
fn directory_states<'a>(
    root: &Path,
    directories: &[&'a Path],
) -> HashMap<&'a Path, DirectoryStamp> {
    const PER_THREAD: usize = 1024;
    let threads = directories
        .len()
        .div_ceil(PER_THREAD)
        .clamp(1, 16)
        .min(std::thread::available_parallelism().map_or(1, usize::from));
    let chunk = directories.len().div_ceil(threads).max(1);
    let walk_part = |part: &[&'a Path]| -> Vec<(&'a Path, DirectoryStamp)> {
        let Some(first) = part.first() else {
            return Vec::new();
        };
        // The part's directories after the ancestors of its first one,
        // so that each is opened below its parent.
        let mut names: Vec<PathBuf> = first
            .ancestors()
            .skip(1)
            .filter(|ancestor| !ancestor.as_os_str().is_empty())
            .map(Path::to_path_buf)
            .collect();
        let ancestors = names.len();
        names.extend(part.iter().map(|directory| directory.to_path_buf()));
        let Ok(root_file) = crate::dir_stamps::open_directory(None, root) else {
            return Vec::new();
        };
        let mut found = Vec::with_capacity(part.len());
        crate::dir_stamps::walk(root_file, &names, |index, stamp| {
            if index >= ancestors
                && let Some(stamp) = stamp
            {
                found.push((part[index - ancestors], stamp));
            }
            true
        });
        found
    };
    if threads == 1 {
        return walk_part(directories).into_iter().collect();
    }
    std::thread::scope(|scope| {
        let handles: Vec<_> = directories
            .chunks(chunk)
            .map(|part| scope.spawn(move || walk_part(part)))
            .collect();
        handles
            .into_iter()
            .flat_map(|handle| handle.join().unwrap_or_default())
            .collect()
    })
}

/// The output tree's record, next to the execution root (outside it).
fn record_path(root: &Path) -> PathBuf {
    root.with_file_name(RECORD_NAME)
}

const RECORD_NAME: &str = ".bazel-rs-output-tree";
const RECORD_MAGIC: &[u8; 8] = b"BZRSOTRE";
const RECORD_VERSION: u32 = 1;
/// Magic, version, the first 8 bytes of the body's blake3 hash.
const RECORD_HEADER: usize = 8 + 4 + 8;

/// What an output tree entry is: a hardlink of this inode, or a symlink.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
enum EntryKind {
    File { device: u64, inode: u64 },
    Link,
}

impl From<crate::action::PublishedKind> for EntryKind {
    fn from(kind: crate::action::PublishedKind) -> Self {
        match kind {
            crate::action::PublishedKind::File { device, inode } => Self::File { device, inode },
            crate::action::PublishedKind::Link => Self::Link,
        }
    }
}

/// One entry: the first 16 bytes of the blake3 hash of its source path,
/// and its kind.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
struct Entry {
    source: [u8; 16],
    kind: EntryKind,
}

fn source_hash(source: &Path) -> [u8; 16] {
    let hash = blake3::hash(source.as_os_str().as_bytes());
    hash.as_bytes()[..16].try_into().expect("16 bytes")
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct RecordedDirectory {
    stamp: DirectoryStamp,
    /// The entries known to be in the directory in that state, by name.
    entries: BTreeMap<OsString, Entry>,
}

/// The record: stamped directories (relative to the execution root; the
/// root itself is the empty path) and their entries.
///
/// A build reads only the directories it looks at ([`Record::read_selected`]:
/// the record of every earlier build's outputs is megabytes, decoded without
/// copying, and only the selected directories are materialized); the
/// others stay in `kept`, the record's verified body, and are written back
/// exactly as they were.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct Record {
    directories: HashMap<PathBuf, RecordedDirectory>,
    /// The verified body the record was read from (`None`: read whole, or
    /// no record) and the directories taken from it into `directories`.
    kept: Option<(Vec<u8>, HashSet<Vec<u8>>)>,
}

/// The record's encoding: paths and names as bytes (not necessarily UTF-8).
/// Written and read as [`BorrowedRecordFile`], the same bytes.
#[cfg(test)]
#[derive(Serialize, Deserialize)]
struct RecordFile {
    directories: Vec<EncodedDirectory>,
}

/// A directory's path, stamp and entries (name, entry).
#[cfg(test)]
type EncodedDirectory = (Vec<u8>, DirectoryStamp, Vec<(Vec<u8>, Entry)>);

/// [`RecordFile`] decoded without copying names (postcard spells a byte
/// vector and a byte slice alike: a length, then the bytes).
#[derive(Serialize, Deserialize)]
struct BorrowedRecordFile<'a> {
    #[serde(borrow)]
    directories: Vec<BorrowedDirectory<'a>>,
}

type BorrowedDirectory<'a> = (&'a [u8], DirectoryStamp, Vec<(&'a [u8], Entry)>);

impl Record {
    /// The verified body of the record at `path`; `None` when it is
    /// missing, foreign or corrupt (no record).
    fn body(path: &Path) -> Option<Vec<u8>> {
        let mut bytes = fs::read(path).ok()?;
        let (header, body) = bytes.split_at_checked(RECORD_HEADER)?;
        if &header[..8] != RECORD_MAGIC
            || header[8..12] != RECORD_VERSION.to_le_bytes()
            || header[12..20] != blake3::hash(body).as_bytes()[..8]
        {
            return None;
        }
        bytes.drain(..RECORD_HEADER);
        Some(bytes)
    }

    /// The record at `path` with only the `selected` directories (by their
    /// bytes) in [`Record::directories`]; the others are kept to be written
    /// back unchanged. Empty when the record is missing, foreign or corrupt.
    fn read_selected(path: &Path, selected: HashSet<&[u8]>) -> Self {
        let Some(body) = Self::body(path) else {
            return Self::default();
        };
        let Ok(file) = postcard::from_bytes::<BorrowedRecordFile<'_>>(&body) else {
            return Self::default();
        };
        let directories: HashMap<PathBuf, RecordedDirectory> = file
            .directories
            .into_iter()
            .filter(|(directory, _, _)| selected.contains(directory))
            .map(materialize)
            .collect();
        let selected = selected.into_iter().map(<[u8]>::to_vec).collect();
        Self {
            directories,
            kept: Some((body, selected)),
        }
    }

    /// Replace the record at `path` atomically.
    fn write(&self, path: &Path) -> std::io::Result<()> {
        let kept = match &self.kept {
            Some((body, selected)) => postcard::from_bytes::<BorrowedRecordFile<'_>>(body)
                .map_err(std::io::Error::other)?
                .directories
                .into_iter()
                .filter(|(directory, _, _)| !selected.contains(*directory))
                .collect(),
            None => Vec::new(),
        };
        let mut directories: Vec<BorrowedDirectory<'_>> = self
            .directories
            .iter()
            .map(|(directory, recorded)| {
                (
                    directory.as_os_str().as_bytes(),
                    recorded.stamp.clone(),
                    recorded
                        .entries
                        .iter()
                        .map(|(name, entry)| (name.as_bytes(), entry.clone()))
                        .collect(),
                )
            })
            .chain(kept)
            .collect();
        directories.sort_by(|a, b| a.0.cmp(b.0));
        let body = postcard::to_stdvec(&BorrowedRecordFile { directories })
            .map_err(std::io::Error::other)?;
        let mut bytes = Vec::with_capacity(RECORD_HEADER + body.len());
        bytes.extend_from_slice(RECORD_MAGIC);
        bytes.extend_from_slice(&RECORD_VERSION.to_le_bytes());
        bytes.extend_from_slice(&blake3::hash(&body).as_bytes()[..8]);
        bytes.extend_from_slice(&body);
        let temporary = temporary(path);
        let written = fs::write(&temporary, &bytes).and_then(|()| fs::rename(&temporary, path));
        if written.is_err() {
            let _ = fs::remove_file(&temporary);
        }
        written
    }
}

/// A decoded directory of the record.
fn materialize((directory, stamp, entries): BorrowedDirectory<'_>) -> (PathBuf, RecordedDirectory) {
    (
        PathBuf::from(OsStr::from_bytes(directory)),
        RecordedDirectory {
            stamp,
            entries: entries
                .into_iter()
                .map(|(name, entry)| (OsStr::from_bytes(name).to_owned(), entry))
                .collect(),
        },
    )
}

/// Link one published directory at its exec path `exec` (below
/// `bazel-out/`) in the output tree of `cache`: a test action's testlogs
/// directory, published below `<cache>/tests/<key>/` (crate::test_runner).
pub fn link_directory(cache: &Path, exec: &Path, source: &Path) -> Result<(), String> {
    if exec.components().next() != Some(Component::Normal("bazel-out".as_ref()))
        || exec
            .components()
            .any(|component| !matches!(component, Component::Normal(_)))
    {
        return Err(format!(
            "internal error: {} is no output exec path",
            exec.display()
        ));
    }
    let root = execution_root(cache);
    let mut tree = Tree {
        root: root.clone(),
        verified: HashSet::new(),
        published: Published::default(),
    };
    tree.link(source, &root.join(exec)).map(drop)
}

/// The exec path of a published action output
/// `<cache>/actions/<key>/outputs/<exec path>`; only paths below
/// `bazel-out/` (every action output's root).
fn exec_path(actions: &Path, output: &Path) -> Option<PathBuf> {
    let relative = output.strip_prefix(actions).ok()?;
    let mut components = relative.components();
    let _key = components.next()?;
    if components.next()? != Component::Normal("outputs".as_ref()) {
        return None;
    }
    let exec = components.as_path();
    let mut parts = exec.components();
    if parts.next()? != Component::Normal("bazel-out".as_ref()) {
        return None;
    }
    // Exec paths are normalized; refuse anything that could leave the tree.
    if exec
        .components()
        .any(|component| !matches!(component, Component::Normal(_)))
    {
        return None;
    }
    Some(exec.to_path_buf())
}

#[cfg(test)]
thread_local! {
    /// Test hook: output tree entries checked one by one on this thread.
    static ENTRY_CHECKS: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

struct Tree {
    root: PathBuf,
    /// Directories of the tree known to be real directories in this run.
    verified: HashSet<PathBuf>,
    published: Published,
}

impl Tree {
    /// Link `source` at `destination`; what the entry is now (`None`:
    /// nothing is published at `source`).
    fn link(&mut self, source: &Path, destination: &Path) -> Result<Option<EntryKind>, String> {
        #[cfg(test)]
        ENTRY_CHECKS.with(|checks| checks.set(checks.get() + 1));
        let metadata = match fs::symlink_metadata(source) {
            Ok(metadata) => metadata,
            // Nothing published at this path (never expected): nothing to link.
            Err(error) if error.kind() == ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(io("cannot inspect", source, &error)),
        };
        let file = EntryKind::File {
            device: metadata.dev(),
            inode: metadata.ino(),
        };
        let parent = destination
            .parent()
            .ok_or_else(|| format!("output tree path {} has no parent", destination.display()))?;
        self.real_directory(parent)?;
        let current = fs::symlink_metadata(destination).ok();
        if metadata.file_type().is_file() {
            if current.as_ref().is_some_and(|current| {
                current.file_type().is_file()
                    && current.dev() == metadata.dev()
                    && current.ino() == metadata.ino()
            }) {
                self.published.unchanged += 1;
                return Ok(Some(file));
            }
            let temporary = temporary(destination);
            let _ = fs::remove_file(&temporary);
            if fs::hard_link(source, &temporary).is_ok() {
                return self.replace(&temporary, destination).map(|()| Some(file));
            }
        } else if current
            .as_ref()
            .is_some_and(|current| current.file_type().is_symlink())
            && fs::read_link(destination).is_ok_and(|target| target == source)
        {
            self.published.unchanged += 1;
            return Ok(Some(EntryKind::Link));
        }
        // A directory, a symlink, or a file that cannot be hardlinked.
        let temporary = temporary(destination);
        let _ = fs::remove_file(&temporary);
        std::os::unix::fs::symlink(source, &temporary)
            .map_err(|error| io("cannot link", &temporary, &error))?;
        self.replace(&temporary, destination)
            .map(|()| Some(EntryKind::Link))
    }

    /// Rename `temporary` over `destination`; a real directory there (an
    /// earlier build's tree below a path that is now one output) is removed
    /// first.
    fn replace(&mut self, temporary: &Path, destination: &Path) -> Result<(), String> {
        let renamed = fs::rename(temporary, destination).or_else(|error| {
            let directory =
                fs::symlink_metadata(destination).is_ok_and(|current| current.file_type().is_dir());
            if !directory {
                return Err(error);
            }
            fs::remove_dir_all(destination)?;
            fs::rename(temporary, destination)
        });
        match renamed {
            Ok(()) => {
                self.published.linked += 1;
                Ok(())
            }
            Err(error) => {
                let _ = fs::remove_file(temporary);
                Err(io("cannot publish", destination, &error))
            }
        }
    }

    /// Make `directory` (below the tree root) a real directory: never write
    /// through a symlink an earlier build left (a tree artifact or runfiles
    /// directory link), which points into the action cache.
    fn real_directory(&mut self, directory: &Path) -> Result<(), String> {
        if self.verified.contains(directory) {
            return Ok(());
        }
        if directory == self.root || !directory.starts_with(&self.root) {
            fs::create_dir_all(directory)
                .map_err(|error| io("cannot create directory", directory, &error))?;
            self.verified.insert(directory.to_path_buf());
            return Ok(());
        }
        if let Some(parent) = directory.parent() {
            self.real_directory(parent)?;
        }
        match fs::symlink_metadata(directory) {
            Ok(metadata) if metadata.file_type().is_dir() => {}
            Ok(_) => {
                fs::remove_file(directory)
                    .map_err(|error| io("cannot replace", directory, &error))?;
                create_directory(directory)?;
            }
            Err(error) if error.kind() == ErrorKind::NotFound => create_directory(directory)?,
            Err(error) => return Err(io("cannot inspect", directory, &error)),
        }
        self.verified.insert(directory.to_path_buf());
        Ok(())
    }
}

fn create_directory(directory: &Path) -> Result<(), String> {
    match fs::create_dir(directory) {
        // A concurrent command created it.
        Err(error) if error.kind() != ErrorKind::AlreadyExists => {
            Err(io("cannot create directory", directory, &error))
        }
        _ => Ok(()),
    }
}

/// A sibling temporary name for `destination`, private to this process.
fn temporary(destination: &Path) -> PathBuf {
    let name = destination
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default();
    destination.with_file_name(format!(".{name}.bazel-rs-tmp.{}", std::process::id()))
}

fn io(what: &str, path: &Path, error: &std::io::Error) -> String {
    format!("{what} '{}': {error}", path.display())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn result(outputs: &[&Path]) -> ActionResult {
        ActionResult {
            outputs: outputs.iter().map(|path| path.to_path_buf()).collect(),
            cache_hit: false,
            remote_hit: false,
            remote_executed: false,
            trees: BTreeMap::new(),
        }
    }

    fn write(path: &Path, text: &str) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, text).unwrap();
    }

    fn stamp(inode: u64) -> DirectoryStamp {
        DirectoryStamp {
            device: 1,
            inode,
            mode: 0o40755,
            mtime: (1, 2),
            ctime: (3, 4),
        }
    }

    /// The borrowed encoding is the owned one byte for byte, and a record
    /// read for some directories writes every other one back unchanged.
    #[test]
    fn selected_records_keep_the_other_directories_byte_for_byte() {
        let entry = |source: u8, kind: EntryKind| Entry {
            source: [source; 16],
            kind,
        };
        let owned = RecordFile {
            directories: vec![
                (
                    Vec::new(),
                    stamp(1),
                    vec![(b"bazel-out".to_vec(), entry(1, EntryKind::Link))],
                ),
                (
                    b"bazel-out/a".to_vec(),
                    stamp(2),
                    vec![
                        (
                            b"f".to_vec(),
                            entry(
                                2,
                                EntryKind::File {
                                    device: 1,
                                    inode: 9,
                                },
                            ),
                        ),
                        (b"g\xff".to_vec(), entry(3, EntryKind::Link)),
                    ],
                ),
                (b"bazel-out/b".to_vec(), stamp(3), Vec::new()),
            ],
        };
        let body = postcard::to_stdvec(&owned).unwrap();
        let borrowed = postcard::from_bytes::<BorrowedRecordFile<'_>>(&body).unwrap();
        assert_eq!(postcard::to_stdvec(&borrowed).unwrap(), body);

        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join(RECORD_NAME);
        let mut bytes = Vec::new();
        bytes.extend_from_slice(RECORD_MAGIC);
        bytes.extend_from_slice(&RECORD_VERSION.to_le_bytes());
        bytes.extend_from_slice(&blake3::hash(&body).as_bytes()[..8]);
        bytes.extend_from_slice(&body);
        fs::write(&path, &bytes).unwrap();

        let selected: HashSet<&[u8]> = [&b"bazel-out/a"[..], b"bazel-out/new"].into();
        let mut record = Record::read_selected(&path, selected);
        assert_eq!(
            record.directories.keys().collect::<Vec<_>>(),
            [&PathBuf::from("bazel-out/a")]
        );
        // Unchanged: the same file again.
        record.write(&path).unwrap();
        assert_eq!(fs::read(&path).unwrap(), bytes);
        // A selected directory removed, another added: the rest is kept.
        record.directories.remove(Path::new("bazel-out/a"));
        record.directories.insert(
            PathBuf::from("bazel-out/new"),
            RecordedDirectory {
                stamp: stamp(4),
                entries: BTreeMap::new(),
            },
        );
        record.write(&path).unwrap();
        let reread = Record::read_selected(
            &path,
            [&b""[..], b"bazel-out/a", b"bazel-out/b", b"bazel-out/new"].into(),
        );
        let mut names: Vec<_> = reread.directories.keys().cloned().collect();
        names.sort();
        assert_eq!(
            names,
            [
                PathBuf::from(""),
                PathBuf::from("bazel-out/b"),
                PathBuf::from("bazel-out/new")
            ]
        );
        assert_eq!(reread.directories[Path::new("")].entries.len(), 1);
        // A corrupt record reads as none.
        fs::write(&path, b"garbage").unwrap();
        assert!(
            Record::read_selected(&path, HashSet::new())
                .directories
                .is_empty()
        );
    }

    #[test]
    fn exec_paths_are_below_bazel_out_only() {
        let actions = Path::new("/c/actions");
        assert_eq!(
            exec_path(
                actions,
                Path::new("/c/actions/k/outputs/bazel-out/k8-fastbuild/bin/p/f")
            ),
            Some(PathBuf::from("bazel-out/k8-fastbuild/bin/p/f"))
        );
        assert_eq!(
            exec_path(actions, Path::new("/c/actions/k/outputs/elsewhere/f")),
            None
        );
        assert_eq!(
            exec_path(actions, Path::new("/c/actions/k/other/bazel-out/f")),
            None
        );
        assert_eq!(exec_path(actions, Path::new("/d/bazel-out/f")), None);
    }

    #[test]
    fn publishes_files_trees_and_runfiles_directories() {
        let temp = tempfile::tempdir().unwrap();
        let cache = temp.path();
        let bin = "bazel-out/k8-fastbuild/bin";
        let file = cache.join(format!("actions/a/outputs/{bin}/pkg/tool"));
        write(&file, "tool");
        let tree = cache.join(format!("actions/b/outputs/{bin}/pkg/tree"));
        write(&tree.join("x"), "x");
        let manifest = cache.join(format!(
            "actions/c/outputs/{bin}/pkg/tool.runfiles/MANIFEST"
        ));
        write(&manifest, "");
        write(&manifest.with_file_name("entry"), "e");
        let mut results = BTreeMap::new();
        results.insert("a".to_owned(), result(&[&file]));
        results.insert("b".to_owned(), result(&[&tree]));
        results.insert("c".to_owned(), result(&[&manifest]));
        let trees = HashSet::from(["c".to_owned()]);
        let published = publish(cache, &results, &trees);
        assert_eq!(published.error, None);
        assert_eq!(published.linked, 3);
        let out = output_path(cache).join("k8-fastbuild/bin/pkg");
        let linked = fs::symlink_metadata(out.join("tool")).unwrap();
        assert!(linked.file_type().is_file());
        assert_eq!(linked.ino(), fs::metadata(&file).unwrap().ino());
        assert_eq!(fs::read_link(out.join("tree")).unwrap(), tree);
        assert_eq!(fs::read_to_string(out.join("tree/x")).unwrap(), "x");
        assert_eq!(
            fs::read_to_string(out.join("tool.runfiles/entry")).unwrap(),
            "e"
        );
        // Unchanged entries are left alone.
        let again = publish(cache, &results, &trees);
        assert_eq!((again.linked, again.unchanged), (0, 3));
    }

    /// [`publish`] is O(changed): once its directories are stamped, an
    /// entry the record lists with the output's source and inode is left
    /// alone without inspecting it; anything that changed a directory, or a
    /// source published again (another inode), is checked and relinked.
    #[test]
    fn unchanged_directories_are_not_checked_entry_by_entry() {
        let temp = tempfile::tempdir().unwrap();
        let cache = fs::canonicalize(temp.path()).unwrap();
        let cache = cache.as_path();
        let bin = "bazel-out/k8-fastbuild/bin";
        let file = cache.join(format!("actions/a/outputs/{bin}/pkg/tool"));
        write(&file, "tool");
        let other = cache.join(format!("actions/a/outputs/{bin}/pkg/sub/data"));
        write(&other, "data");
        let mut results = BTreeMap::new();
        results.insert("a".to_owned(), result(&[&file, &other]));
        let checks = || ENTRY_CHECKS.with(std::cell::Cell::get);
        let publish = || {
            // Directory times have a coarse granularity: a change and the
            // relink after it must not fall into one tick, which the settled
            // rule guarantees outside this test (SETTLE_ALL).
            std::thread::sleep(std::time::Duration::from_millis(30));
            let before = checks();
            let published = publish(cache, &results, &HashSet::new());
            assert_eq!(published.error, None);
            (published.linked, published.unchanged, checks() - before)
        };
        let out = output_path(cache).join("k8-fastbuild/bin/pkg");
        assert_eq!(publish(), (2, 0, 2));
        crate::dir_stamps::SETTLE_ALL.with(|settle| settle.set(true));
        // The cache check's view of the outputs (crate::action::published_kind).
        crate::action::remember_published_file(cache, &file);
        crate::action::remember_published_file(cache, &other);
        // Unchanged since the last build, but not stamped yet: checked once.
        assert_eq!(publish(), (0, 2, 2));
        assert!(record_path(&execution_root(cache)).exists());
        assert_eq!(publish(), (0, 2, 0));
        // An entry replaced in the tree is found through its directory.
        fs::remove_file(out.join("tool")).unwrap();
        write(&out.join("tool"), "edited");
        assert_eq!(publish(), (1, 1, 1));
        assert_eq!(fs::read_to_string(out.join("tool")).unwrap(), "tool");
        // The output published again (a new inode at the same path).
        assert_eq!(publish(), (0, 2, 1));
        fs::remove_file(&file).unwrap();
        write(&file, "tool v2");
        crate::action::remember_published_file(cache, &file);
        assert_eq!(publish(), (1, 1, 1));
        assert_eq!(fs::read_to_string(out.join("tool")).unwrap(), "tool v2");
        assert_eq!(publish(), (0, 2, 1));
        assert_eq!(publish(), (0, 2, 0));
        // A directory of the tree replaced by a symlink is never trusted.
        let moved = out.with_file_name("moved");
        fs::rename(out.join("sub"), &moved).unwrap();
        std::os::unix::fs::symlink(&moved, out.join("sub")).unwrap();
        // (Its parent changed too: both entries are checked.)
        assert_eq!(publish(), (1, 1, 2));
        assert!(!fs::symlink_metadata(out.join("sub")).unwrap().is_symlink());
        assert_eq!(fs::read_to_string(out.join("sub/data")).unwrap(), "data");
        // A corrupt record is no record.
        fs::write(record_path(&execution_root(cache)), b"BZRSOTRE").unwrap();
        assert_eq!(publish(), (0, 2, 2));
        crate::dir_stamps::SETTLE_ALL.with(|settle| settle.set(false));
    }

    #[test]
    fn replaces_earlier_entries_without_writing_through_links() {
        let temp = tempfile::tempdir().unwrap();
        let cache = temp.path();
        let bin = "bazel-out/k8-fastbuild/bin";
        // An earlier build's tree artifact at pkg/d, now a package
        // directory holding a file output.
        let tree = cache.join(format!("actions/old/outputs/{bin}/pkg/d"));
        write(&tree.join("keep"), "old");
        let mut results = BTreeMap::new();
        results.insert("old".to_owned(), result(&[&tree]));
        assert_eq!(publish(cache, &results, &HashSet::new()).error, None);
        let file = cache.join(format!("actions/new/outputs/{bin}/pkg/d/f"));
        write(&file, "new");
        // An earlier real directory where a file output now is.
        let replaced = cache.join(format!("actions/new/outputs/{bin}/pkg/g"));
        write(&replaced, "g");
        let stale = output_path(cache).join("k8-fastbuild/bin/pkg/g/stale");
        write(&stale, "stale");
        let mut results = BTreeMap::new();
        results.insert("new".to_owned(), result(&[&file, &replaced]));
        let published = publish(cache, &results, &HashSet::new());
        assert_eq!(published.error, None);
        let out = output_path(cache).join("k8-fastbuild/bin/pkg");
        assert!(!fs::symlink_metadata(out.join("d")).unwrap().is_symlink());
        assert_eq!(fs::read_to_string(out.join("d/f")).unwrap(), "new");
        // The old action entry was not written through.
        assert!(!tree.join("f").exists());
        assert_eq!(fs::read_to_string(out.join("g")).unwrap(), "g");
    }
}
