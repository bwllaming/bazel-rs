//! Mode changes that never follow a link (docs/REPOSITORY_SLOTS.md, "Sealed
//! trees"): an entry is opened `O_PATH | O_NOFOLLOW` (a link at its last
//! component is reported, never opened through), its type and mode are read
//! from that descriptor, and the mode is changed on exactly that inode —
//! through `/proc/self/fd/<fd>`, as glibc's `fchmodat(AT_SYMLINK_NOFOLLOW)`
//! does (an `O_PATH` descriptor cannot be `fchmod`ed), or, without `/proc`,
//! by `fchmod` of a descriptor reopened `O_NOFOLLOW` and checked to be the
//! same inode. Walks descend through directory descriptors
//! (`O_DIRECTORY | O_NOFOLLOW`), so no path component is resolved again
//! between a check and a change: a directory replaced by a link meanwhile
//! is never followed.

use rustix::fs::{self as rfs, CWD, FileType, Mode, OFlags};
use std::ffi::CStr;
use std::io;
use std::os::fd::{AsFd, AsRawFd, BorrowedFd, OwnedFd};
use std::path::Path;

/// An entry opened without following a link at its last component.
pub(crate) struct Entry {
    fd: OwnedFd,
    /// The permission bits (`& 0o7777`).
    pub(crate) mode: u32,
    pub(crate) directory: bool,
    dev: u64,
    ino: u64,
}

/// A name below an open directory, or a path from the working directory.
#[derive(Clone, Copy)]
pub(crate) enum At<'a> {
    Dir(BorrowedFd<'a>, &'a CStr),
    Path(&'a Path),
}

impl At<'_> {
    fn open(&self, flags: OFlags) -> io::Result<OwnedFd> {
        let flags = flags | OFlags::NOFOLLOW | OFlags::CLOEXEC;
        match *self {
            At::Dir(dir, name) => rfs::openat(dir, name, flags, Mode::empty()),
            At::Path(path) => rfs::openat(CWD, path, flags, Mode::empty()),
        }
        .map_err(io::Error::from)
    }
}

/// `at` opened `O_PATH | O_NOFOLLOW`: `None` for a link (never opened
/// through) or a missing entry.
pub(crate) fn open(at: At<'_>) -> io::Result<Option<Entry>> {
    let fd = match at.open(OFlags::PATH) {
        Ok(fd) => fd,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };
    let stat = rfs::fstat(&fd)?;
    let kind = FileType::from_raw_mode(stat.st_mode);
    if kind == FileType::Symlink {
        return Ok(None);
    }
    Ok(Some(Entry {
        fd,
        mode: stat.st_mode & 0o7777,
        directory: kind == FileType::Directory,
        dev: stat.st_dev,
        ino: stat.st_ino,
    }))
}

/// Set the mode of the inode `entry` (opened at `at`) to `mode`.
pub(crate) fn chmod(at: At<'_>, entry: &Entry, mode: u32) -> io::Result<()> {
    let mode = Mode::from_raw_mode(mode);
    let proc = format!("/proc/self/fd/{}", entry.fd.as_raw_fd());
    match rfs::chmod(proc.as_str(), mode) {
        Ok(()) => return Ok(()),
        // No /proc: a real descriptor of the same inode.
        Err(rustix::io::Errno::NOENT) => {}
        Err(error) => return Err(error.into()),
    }
    let flags = if entry.directory {
        OFlags::RDONLY | OFlags::DIRECTORY
    } else {
        OFlags::RDONLY | OFlags::NONBLOCK | OFlags::NOCTTY
    };
    let fd = at.open(flags)?;
    let stat = rfs::fstat(&fd)?;
    if (stat.st_dev, stat.st_ino) != (entry.dev, entry.ino) {
        return Err(io::Error::other(
            "the entry was replaced while its mode changed",
        ));
    }
    rfs::fchmod(&fd, mode).map_err(io::Error::from)
}

/// Change the mode of `at` (never through a link) to `change(mode)` when
/// that is `Some` and differs; the entry's previous mode, `None` for a link
/// or a missing entry.
pub(crate) fn change(
    at: At<'_>,
    change: impl FnOnce(u32) -> Option<u32>,
) -> io::Result<Option<u32>> {
    let Some(entry) = open(at)? else {
        return Ok(None);
    };
    if let Some(mode) = change(entry.mode).filter(|mode| *mode != entry.mode) {
        chmod(at, &entry, mode)?;
    }
    Ok(Some(entry.mode))
}

/// Visit every entry below the directory `root` (never through a link),
/// children before their directory: `visit(at, entry)` may change the
/// entry's mode with [`chmod`]. A directory is descended into only when its
/// owner may read and search it now (`descend`'s mode, applied first when
/// `Some`): a walk that must open unreadable directories gives them the
/// permission it needs.
pub(crate) fn walk(
    root: &Path,
    descend: &dyn Fn(u32) -> Option<u32>,
    visit: &mut dyn FnMut(At<'_>, &Entry) -> io::Result<()>,
) -> io::Result<()> {
    fn below(
        dir: &OwnedFd,
        descend: &dyn Fn(u32) -> Option<u32>,
        visit: &mut dyn FnMut(At<'_>, &Entry) -> io::Result<()>,
    ) -> io::Result<()> {
        let names: Vec<std::ffi::CString> = rfs::Dir::read_from(dir)?
            .filter_map(|entry| match entry {
                Ok(entry) => {
                    let name = entry.file_name();
                    (name.to_bytes() != b"." && name.to_bytes() != b"..")
                        .then(|| Ok(name.to_owned()))
                }
                Err(error) => Some(Err(io::Error::from(error))),
            })
            .collect::<io::Result<_>>()?;
        for name in &names {
            let at = At::Dir(dir.as_fd(), name);
            let Some(mut entry) = open(at)? else {
                continue;
            };
            if entry.directory {
                if let Some(mode) = descend(entry.mode).filter(|mode| *mode != entry.mode) {
                    chmod(at, &entry, mode)?;
                    entry.mode = mode;
                }
                let sub = at.open(OFlags::RDONLY | OFlags::DIRECTORY)?;
                let stat = rfs::fstat(&sub)?;
                if (stat.st_dev, stat.st_ino) != (entry.dev, entry.ino) {
                    return Err(io::Error::other("a directory was replaced during a walk"));
                }
                below(&sub, descend, visit)?;
            }
            visit(at, &entry)?;
        }
        Ok(())
    }
    let dir = At::Path(root).open(OFlags::RDONLY | OFlags::DIRECTORY)?;
    below(&dir, descend, visit)
}

/// A directory given its owner's write and search permission while a
/// repository rule changes an entry in it; its mode is restored when this
/// is dropped (unless [`Self::keep`]), if it is still the same inode.
///
/// A repository's copy of another repository's published tree (`cp -r` of
/// a sealed tree) keeps the sealed modes, where a copy of genuine's
/// writable `external/<name>` would not: `repository_ctx.file`,
/// `template`, `symlink`, `delete`, `rename`, `patch` and `download` into
/// such a copy change entries of read-only directories. Genuine's file
/// operations delete an existing entry and create the new one (the
/// entry's own mode never matters), so they work in its copies; here the
/// directory they change is made writable for them, which makes the copy's
/// modes irrelevant, as they are in genuine (docs/REPOSITORY_SLOTS.md,
/// "Sealed trees"). A rule that made a directory of its own read-only and
/// then writes into it fails in genuine and succeeds here.
pub(crate) struct WritableDirectory(Option<(std::path::PathBuf, u32, u64, u64)>);

impl WritableDirectory {
    /// The directory holding `path`: the deepest existing directory among
    /// its ancestors at or below `root` (missing parents are created in
    /// it). Nothing outside `root`, nothing through a link; best effort (a
    /// failure leaves the operation's own error to report).
    pub(crate) fn holding(root: &Path, path: &Path) -> Self {
        let mut current = path.parent();
        while let Some(directory) = current {
            if !directory.starts_with(root) {
                break;
            }
            match open(At::Path(directory)) {
                Ok(Some(entry)) if entry.directory => return Self::open(directory, &entry),
                Ok(Some(_)) => break,
                Ok(None) if std::fs::symlink_metadata(directory).is_err() => {
                    current = directory.parent();
                }
                _ => break,
            }
        }
        Self(None)
    }

    /// `directory` itself (never through a link).
    pub(crate) fn of(directory: &Path) -> Self {
        match open(At::Path(directory)) {
            Ok(Some(entry)) if entry.directory => Self::open(directory, &entry),
            _ => Self(None),
        }
    }

    fn open(directory: &Path, entry: &Entry) -> Self {
        if entry.mode & 0o300 == 0o300
            || chmod(At::Path(directory), entry, entry.mode | 0o300).is_err()
        {
            return Self(None);
        }
        Self(Some((
            directory.to_path_buf(),
            entry.mode,
            entry.dev,
            entry.ino,
        )))
    }

    /// The directory was renamed to `to` (restored there).
    pub(crate) fn moved(mut self, to: &Path) -> Self {
        if let Some((directory, ..)) = &mut self.0 {
            *directory = to.to_path_buf();
        }
        self
    }

    /// Leave the directory writable (an operation that completes later:
    /// a pending download).
    pub(crate) fn keep(mut self) {
        self.0 = None;
    }
}

impl Drop for WritableDirectory {
    fn drop(&mut self) {
        if let Some((directory, mode, dev, ino)) = self.0.take()
            && let Ok(Some(entry)) = open(At::Path(&directory))
            && (entry.dev, entry.ino) == (dev, ino)
        {
            let _ = chmod(At::Path(&directory), &entry, mode);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::os::unix::fs::PermissionsExt;

    fn mode(path: &Path) -> u32 {
        fs::symlink_metadata(path).unwrap().permissions().mode() & 0o7777
    }

    /// Links are reported, never changed through: the walk and `change`
    /// leave a link's referent (outside the tree) as it was.
    #[test]
    fn mode_changes_never_follow_links() {
        let temp = tempfile::tempdir().unwrap();
        let outside = temp.path().join("outside");
        fs::create_dir(&outside).unwrap();
        fs::write(outside.join("f"), b"").unwrap();
        let tree = temp.path().join("tree");
        fs::create_dir_all(tree.join("d")).unwrap();
        fs::write(tree.join("d/f"), b"").unwrap();
        std::os::unix::fs::symlink(&outside, tree.join("dir-link")).unwrap();
        std::os::unix::fs::symlink(outside.join("f"), tree.join("d/file-link")).unwrap();
        let mut seen = Vec::new();
        walk(&tree, &|_| None, &mut |at, entry| {
            seen.push(entry.mode);
            chmod(at, entry, entry.mode & !0o222)
        })
        .unwrap();
        assert_eq!(seen.len(), 2);
        assert_eq!(
            (mode(&tree.join("d")), mode(&tree.join("d/f"))),
            (0o555, 0o444)
        );
        assert_eq!((mode(&outside), mode(&outside.join("f"))), (0o755, 0o644));
        assert_eq!(
            change(At::Path(&tree.join("dir-link")), |_| Some(0o500)).unwrap(),
            None
        );
        assert_eq!(mode(&outside), 0o755);
        assert_eq!(
            change(At::Path(&tree.join("d")), |m| Some(m | 0o700)).unwrap(),
            Some(0o555)
        );
        assert_eq!(mode(&tree.join("d")), 0o755);
        // An unreadable directory is opened once `descend` allows it.
        fs::set_permissions(tree.join("d"), fs::Permissions::from_mode(0o000)).unwrap();
        let mut count = 0;
        walk(&tree, &|m| Some(m | 0o700), &mut |_, _| {
            count += 1;
            Ok(())
        })
        .unwrap();
        assert_eq!(count, 2);
        assert_eq!(mode(&tree.join("d")), 0o700);
        assert_eq!(
            change(At::Path(&temp.path().join("missing")), |_| Some(0)).unwrap(),
            None
        );
    }
}
