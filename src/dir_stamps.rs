//! Directory stamps: a directory's identity and times, which change with
//! every entry added to it, removed from it or renamed in it (and with every
//! change of its own mode or times). While a directory still shows the stamp
//! taken after a check of its entries, it still has exactly the names, and
//! each name exactly the inode, that the check found. The executor's checks
//! of published trees (crate::action, docs/HARDLINK_STAGING.md "Cache checks
//! of published trees") and the output tree (crate::output_tree) use them to
//! re-inspect directories instead of every entry.
//!
//! [`walk`] inspects many directories below one root through descriptors
//! opened relative to their parents (`O_PATH`): an absolute path's lookup
//! walks every component from `/`, and on overlay filesystems such walks
//! serialize across threads (measured on this host: ~2.5 µs per `lstat` of
//! a deep cache path whatever the number of threads, ~0.3 µs per
//! one-component lookup from a parent's descriptor with 64 threads).

use serde::{Deserialize, Serialize};
use std::fs;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

/// One directory's identity and times, as its lstat showed them.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DirectoryStamp {
    pub device: u64,
    pub inode: u64,
    /// The full `st_mode` (type and permissions).
    pub mode: u32,
    pub mtime: (i64, i64),
    pub ctime: (i64, i64),
}

impl DirectoryStamp {
    /// The stamp of a real directory (`None` for anything else).
    pub fn of(metadata: &fs::Metadata) -> Option<Self> {
        (metadata.is_dir() && !metadata.file_type().is_symlink()).then(|| Self {
            device: metadata.dev(),
            inode: metadata.ino(),
            mode: metadata.mode(),
            mtime: (metadata.mtime(), metadata.mtime_nsec()),
            ctime: (metadata.ctime(), metadata.ctime_nsec()),
        })
    }

    /// The stamp of the real directory at `path` (lstat).
    pub fn at(path: &Path) -> Option<Self> {
        Self::of(&fs::symlink_metadata(path).ok()?)
    }

    /// Settled at `start`: modification and change time at least
    /// crate::digest_memo::RACY_MARGIN older ("racy git",
    /// crate::digest_memo::settled). A change made after `start` then
    /// necessarily gives the directory a later change time, so no later
    /// state can show this stamp again.
    pub fn settled(&self, start: SystemTime) -> bool {
        #[cfg(test)]
        if SETTLE_ALL.with(std::cell::Cell::get) {
            return true;
        }
        let Some(limit) = start
            .checked_sub(crate::digest_memo::RACY_MARGIN)
            .and_then(|limit| limit.duration_since(SystemTime::UNIX_EPOCH).ok())
        else {
            return false;
        };
        [self.mtime, self.ctime]
            .into_iter()
            .all(|(seconds, nanos)| {
                u64::try_from(seconds).is_ok_and(|seconds| {
                    let nanos = u32::try_from(nanos).unwrap_or(u32::MAX).min(999_999_999);
                    Duration::new(seconds, nanos) <= limit
                })
            })
    }
}

#[cfg(test)]
thread_local! {
    /// Test hook: every directory counts as settled on this thread.
    pub static SETTLE_ALL: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// `path` (relative to `parent` when given) opened as a directory for
/// lookups and `fstat` only (`O_PATH`), never through a symlink at its last
/// component.
pub fn open_directory(parent: Option<&fs::File>, path: &Path) -> std::io::Result<fs::File> {
    use rustix::fs::{CWD, Mode, OFlags, openat};
    let flags = OFlags::PATH | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC;
    let fd = match parent {
        Some(parent) => openat(parent, path, flags, Mode::empty()),
        None => openat(CWD, path, flags, Mode::empty()),
    }?;
    Ok(fs::File::from(fd))
}

/// Visit the directories `names` (relative, normal components) below the
/// directory open as `root` in component order (each after its parent),
/// with the stamp each shows now (`None`: missing, no real directory, or
/// below such a path); stops when `visit` returns false. `visit` gets the
/// index of the name in `names`. Every lookup is one component relative to
/// the parent's descriptor, and at most one descriptor per level is open.
pub fn walk(
    root: fs::File,
    names: &[PathBuf],
    mut visit: impl FnMut(usize, Option<DirectoryStamp>) -> bool,
) {
    let mut order: Vec<usize> = (0..names.len()).collect();
    order.sort_by(|&a, &b| names[a].cmp(&names[b]));
    // The open ancestors of the current directory (`None`: it could not be
    // opened, and nothing below it can).
    let mut open: Vec<(&Path, Option<fs::File>)> = vec![(Path::new(""), Some(root))];
    for index in order {
        let name = names[index].as_path();
        let (Some(parent), Some(leaf)) = (name.parent(), name.file_name()) else {
            if !visit(index, None) {
                return;
            }
            continue;
        };
        while open.last().is_some_and(|(open, _)| *open != parent) {
            open.pop();
        }
        let file = match open.last() {
            Some((_, Some(parent))) => open_directory(Some(parent), Path::new(leaf)).ok(),
            _ => None,
        };
        let stamp = file
            .as_ref()
            .and_then(|file| file.metadata().ok())
            .and_then(|metadata| DirectoryStamp::of(&metadata));
        let file = file.filter(|_| stamp.is_some());
        if !visit(index, stamp) {
            return;
        }
        // A name whose parent is missing from the walk (never after its
        // parent) cannot be pushed in order: its descendants then fail too.
        if open.last().is_some_and(|(open, _)| *open == parent) {
            open.push((name, file));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn walks_report_real_directories_only_in_any_name_order() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        for directory in ["a/b/c", "a/d", "e"] {
            fs::create_dir_all(root.join(directory)).unwrap();
        }
        std::os::unix::fs::symlink(root.join("a"), root.join("link")).unwrap();
        fs::write(root.join("file"), b"").unwrap();
        let names: Vec<PathBuf> = [
            "e",
            "a/d",
            "link",
            "a",
            "a/b/c",
            "link/b",
            "missing",
            "missing/x",
            "file",
            "a/b",
        ]
        .iter()
        .map(PathBuf::from)
        .collect();
        let mut found = vec![None; names.len()];
        walk(
            open_directory(None, root).unwrap(),
            &names,
            |index, stamp| {
                found[index] = Some(stamp);
                true
            },
        );
        for (name, stamp) in names.iter().zip(found) {
            let stamp = stamp.expect("every name is visited");
            let expected = DirectoryStamp::at(&root.join(name)).filter(|_| {
                // Below a symlink nothing counts, whatever it resolves to.
                !name.starts_with("link")
            });
            assert_eq!(stamp, expected, "{}", name.display());
        }
        // A directory's stamp moves with an entry added, removed or renamed.
        let before = DirectoryStamp::at(&root.join("a/b")).unwrap();
        std::thread::sleep(Duration::from_millis(30));
        fs::write(root.join("a/b/new"), b"").unwrap();
        assert_ne!(DirectoryStamp::at(&root.join("a/b")).unwrap(), before);
        // Fresh directories are not settled; old enough ones are.
        let stamp = DirectoryStamp::at(&root.join("e")).unwrap();
        assert!(!stamp.settled(SystemTime::now()));
        assert!(stamp.settled(SystemTime::now() + Duration::from_secs(3)));
        // Stopping early visits nothing more.
        let mut visited = 0;
        walk(open_directory(None, root).unwrap(), &names, |_, _| {
            visited += 1;
            false
        });
        assert_eq!(visited, 1);
    }
}
