//! The workspace's output base (docs/COMMANDS.md "clean"): its lock and
//! `bazel-rs clean [--expunge] [--async]`.
//!
//! The native output base is the workspace's state directory
//! `<cache>/worktrees/<name>-<hash>` (crate::worktree, docs/WORKTREES.md):
//! the output tree `execroot/`, the analysis snapshots, the test result
//! index, the execution roots of `run` of tests and the hidden lockfile.
//! Everything else in the cache — repositories, the action cache and its
//! CAS, test results, the digest memo — is shared by every workspace of the
//! cache, like Bazel's output user root, and `clean` never touches it.
//!
//! The lock (`<state>/lock`) is Bazel's output base lock
//! (blaze_util_posix.cc AcquireLock): every command that uses the state
//! directory holds it shared for its whole run (concurrent native commands
//! of one workspace stay allowed), `clean` holds it exclusive, so `clean`
//! waits for running commands and commands wait for a running `clean`.
//! An exclusive holder identifies itself in the file (`pid=`, `owner=`,
//! `cwd=` lines, like WriteOwnerInformation); a waiting process prints the
//! holder once (`Another command holds the output base lock: ...`, then
//! `Waiting for it to complete...`) and retries every 500 ms, or with
//! `--noblock_for_lock` gives up. A lock file deleted or replaced while a
//! process waited (`clean --expunge`) is opened again (StillExists).

use fs2::FileExt;
use std::fs::{self, File, OpenOptions};
use std::io::{ErrorKind, Read, Seek, Write};
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};

/// The lock file's name in the state directory.
pub const LOCK_NAME: &str = "lock";

/// How a command holds the output base lock.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LockMode {
    /// A command using the state directory (build, test, run, query, fetch).
    Shared,
    /// `clean`.
    Exclusive,
}

/// The held output base lock; released when dropped (an exclusive holder
/// first clears its owner information) or when the process exits.
#[derive(Debug)]
pub struct Lock {
    file: File,
    mode: LockMode,
}

impl Drop for Lock {
    fn drop(&mut self) {
        if self.mode == LockMode::Exclusive {
            let _ = self.file.set_len(0);
        }
    }
}

/// Why the lock was not taken.
#[derive(Debug)]
pub enum LockError {
    /// Held by another command and `--noblock_for_lock` was given.
    Held,
    /// The lock file could not be created or locked.
    Io(String),
}

/// Take the output base lock of the state directory `state` (created if
/// missing). `report` receives the waiting messages (stderr lines).
pub fn acquire(
    state: &Path,
    mode: LockMode,
    owner: &str,
    block: bool,
    report: &mut dyn FnMut(&str),
) -> Result<Lock, LockError> {
    let path = state.join(LOCK_NAME);
    let mut announced: Option<String> = None;
    loop {
        fs::create_dir_all(state)
            .map_err(|e| LockError::Io(format!("cannot create {}: {e}", state.display())))?;
        let (mut file, writable) = open_lock(&path)
            .map_err(|e| LockError::Io(format!("cannot open {}: {e}", path.display())))?;
        let locked = match mode {
            LockMode::Shared => FileExt::try_lock_shared(&file),
            LockMode::Exclusive => FileExt::try_lock_exclusive(&file),
        };
        match locked {
            Ok(()) => {
                if !still_exists(&file, &path) {
                    continue; // deleted or replaced meanwhile: open it again
                }
                if mode == LockMode::Exclusive && writable {
                    let cwd = std::env::current_dir().unwrap_or_default();
                    let info = format!(
                        "pid={}\nowner={owner}\ncwd={}\n",
                        std::process::id(),
                        cwd.display()
                    );
                    let _ = file
                        .set_len(0)
                        .and_then(|()| file.rewind())
                        .and_then(|()| file.write_all(info.as_bytes()));
                }
                return Ok(Lock { file, mode });
            }
            Err(error) if error.kind() == ErrorKind::WouldBlock || is_contended(&error) => {}
            Err(error) => {
                return Err(LockError::Io(format!(
                    "cannot lock {}: {error}",
                    path.display()
                )));
            }
        }
        let mut owner_info = String::new();
        let _ = file.read_to_string(&mut owner_info);
        if owner_info.is_empty() {
            owner_info = format!(
                "a bazel-rs command using the output base {}\n",
                state.display()
            );
        }
        if announced.as_deref() != Some(owner_info.as_str()) {
            report(&format!(
                "Another command holds the output base lock: \n{}",
                owner_info.trim_end_matches('\n')
            ));
            if block {
                report("Waiting for it to complete...");
            }
            announced = Some(owner_info);
        }
        if !block {
            return Err(LockError::Held);
        }
        drop(file);
        std::thread::sleep(std::time::Duration::from_millis(500));
    }
}

/// `flock` contention reported as EAGAIN/EWOULDBLOCK by fs2.
fn is_contended(error: &std::io::Error) -> bool {
    error.raw_os_error() == fs2::lock_contended_error().raw_os_error()
}

/// The lock file, read-write when possible (an exclusive holder writes its
/// owner information), else read-only (a read-only state directory: `flock`
/// needs no write access).
fn open_lock(path: &Path) -> std::io::Result<(File, bool)> {
    let mut options = OpenOptions::new();
    options.read(true).write(true).create(true).truncate(false);
    match options.open(path) {
        Ok(file) => Ok((file, true)),
        Err(error) if error.kind() == ErrorKind::PermissionDenied => {
            File::open(path).map(|file| (file, false))
        }
        Err(error) => Err(error),
    }
}

/// StillExists (blaze_util_posix.cc): the locked file is still the one at
/// `path`.
fn still_exists(file: &File, path: &Path) -> bool {
    match (file.metadata(), fs::metadata(path)) {
        (Ok(held), Ok(current)) => held.dev() == current.dev() && held.ino() == current.ino(),
        _ => false,
    }
}

/// `clean`'s options (CleanCommand.Options).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CleanOptions {
    pub expunge: bool,
    pub asynchronous: bool,
}

/// A failed clean: Bazel's message and exit code (FailureDetails
/// CleanCommand codes: deletion and temporary moves fail with 36).
#[derive(Debug, PartialEq, Eq)]
pub struct CleanError {
    pub message: String,
    pub exit_code: i32,
}

fn failure(error: std::io::Error, path: &Path) -> CleanError {
    CleanError {
        message: format!("{}: {error}", path.display()),
        exit_code: 36,
    }
}

/// The entries of the state directory `clean` (without `--expunge`)
/// keeps: the lock and the hidden lockfile (Bazel's
/// `<output_base>/MODULE.bazel.lock`, outside the execroot it deletes).
const KEPT: &[&str] = &[LOCK_NAME, crate::lockfile::LOCKFILE_NAME];

/// CleanCommand.actuallyClean over the state directory `state` (the caller
/// holds its lock exclusively). Without `--expunge`: everything below the
/// output tree `execroot/` and every other per-workspace state except the
/// hidden lockfile; with it: the whole state directory. `--async` first renames
/// what it deletes next to it (`<name>_tmp_<pid>_<uuid>`) and reports the
/// move like Bazel, then deletes it in this process (Bazel deletes it in
/// a daemonized `rm -rf`). `report` receives the INFO messages.
pub fn clean(
    state: &Path,
    options: CleanOptions,
    report: &mut dyn FnMut(&str),
) -> Result<(), CleanError> {
    if fs::symlink_metadata(state).is_err() {
        return Ok(()); // nothing was ever written for this workspace
    }
    if options.expunge {
        let target = if options.asynchronous {
            let moved = temporary_sibling(state);
            fs::rename(state, &moved).map_err(|e| failure(e, state))?;
            report(&format!(
                "Output base moved to {} for deletion",
                moved.display()
            ));
            moved
        } else {
            state.to_path_buf()
        };
        return remove_tree(&target).map_err(|(path, e)| failure(e, &path));
    }
    owner_access(state).map_err(|e| failure(e, state))?;
    let execroot = state.join("execroot");
    if options.asynchronous && fs::symlink_metadata(&execroot).is_ok() {
        let moved = temporary_sibling(&execroot);
        fs::rename(&execroot, &moved).map_err(|e| failure(e, &execroot))?;
        report(&format!(
            "Output tree moved to {} for deletion",
            moved.display()
        ));
        remove_tree(&moved).map_err(|(path, e)| failure(e, &path))?;
    }
    // A synchronous clean keeps the execroot directory itself
    // (Path.deleteTreesBelow).
    remove_below(&execroot)?;
    let entries = fs::read_dir(state).map_err(|e| failure(e, state))?;
    for entry in entries {
        let entry = entry.map_err(|e| failure(e, state))?;
        let name = entry.file_name();
        if name == "execroot" || KEPT.iter().any(|kept| name == *kept) {
            continue;
        }
        remove_tree(&entry.path()).map_err(|(path, e)| failure(e, &path))?;
    }
    Ok(())
}

/// Delete everything below the directory `path` (if it is one).
fn remove_below(path: &Path) -> Result<(), CleanError> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.is_dir() => {}
        Ok(_) => return remove_tree(path).map_err(|(path, e)| failure(e, &path)),
        Err(_) => return Ok(()),
    }
    owner_access(path).map_err(|e| failure(e, path))?;
    for entry in fs::read_dir(path).map_err(|e| failure(e, path))? {
        let entry = entry.map_err(|e| failure(e, path))?;
        remove_tree(&entry.path()).map_err(|(path, e)| failure(e, &path))?;
    }
    Ok(())
}

/// `<parent>/<name>_tmp_<pid>_<uuid>` (CleanCommand.asyncClean): on the
/// same filesystem, so the rename is atomic.
fn temporary_sibling(path: &Path) -> PathBuf {
    let name = path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default();
    path.with_file_name(format!(
        "{name}_tmp_{}_{}",
        std::process::id(),
        random_uuid()
    ))
}

/// A random-looking UUID (version 4 layout) from the time, the process and
/// a counter (no random number generator dependency).
fn random_uuid() -> String {
    static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let mut hasher = blake3::Hasher::new();
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_nanos())
        .unwrap_or_default();
    hasher.update(&nanos.to_le_bytes());
    hasher.update(&std::process::id().to_le_bytes());
    hasher.update(
        &COUNTER
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            .to_le_bytes(),
    );
    let mut bytes = [0u8; 16];
    bytes.copy_from_slice(&hasher.finalize().as_bytes()[..16]);
    bytes[6] = (bytes[6] & 0x0f) | 0x40;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    let hex: String = bytes.iter().map(|byte| format!("{byte:02x}")).collect();
    format!(
        "{}-{}-{}-{}-{}",
        &hex[..8],
        &hex[8..12],
        &hex[12..16],
        &hex[16..20],
        &hex[20..]
    )
}

/// Give the owner `rwx` on the directory `path` when it lacks any of them
/// (Bazel's `find -type d -not -perm -u=rwx -exec chmod u=rwx`).
fn owner_access(path: &Path) -> std::io::Result<()> {
    let metadata = fs::symlink_metadata(path)?;
    let mode = metadata.permissions().mode();
    if metadata.is_dir() && mode & 0o700 != 0o700 {
        fs::set_permissions(path, fs::Permissions::from_mode(mode | 0o700))?;
    }
    Ok(())
}

/// Delete `path` and everything below it, never following a symlink.
/// Read-only directories are made writable for their owner first; files
/// are never chmod-ed: the output tree's regular files are hardlinks of the
/// shared action cache's read-only outputs (one inode), and unlinking needs
/// write access to the directory only. A missing `path` is no error.
pub fn remove_tree(path: &Path) -> Result<(), (PathBuf, std::io::Error)> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err((path.to_path_buf(), error)),
    };
    if !metadata.is_dir() {
        return match fs::remove_file(path) {
            Err(error) if error.kind() != ErrorKind::NotFound => Err((path.to_path_buf(), error)),
            _ => Ok(()),
        };
    }
    owner_access(path).map_err(|error| (path.to_path_buf(), error))?;
    let entries = fs::read_dir(path).map_err(|error| (path.to_path_buf(), error))?;
    for entry in entries {
        let entry = entry.map_err(|error| (path.to_path_buf(), error))?;
        remove_tree(&entry.path())?;
    }
    match fs::remove_dir(path) {
        Err(error) if error.kind() != ErrorKind::NotFound => Err((path.to_path_buf(), error)),
        _ => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tree(root: &Path) {
        fs::create_dir_all(root.join("execroot/_main/bazel-out/k8-fastbuild/bin/pkg")).unwrap();
        fs::create_dir_all(root.join("snapshots")).unwrap();
        fs::create_dir_all(root.join("tests/index")).unwrap();
        fs::write(root.join("MODULE.bazel.lock"), "{}").unwrap();
        fs::write(root.join("snapshots/s"), "s").unwrap();
        fs::write(
            root.join("execroot/_main/bazel-out/k8-fastbuild/bin/pkg/out"),
            "o",
        )
        .unwrap();
    }

    fn read_only(path: &Path) {
        for entry in fs::read_dir(path).unwrap() {
            let entry = entry.unwrap();
            if entry.file_type().unwrap().is_dir() {
                read_only(&entry.path());
            }
            let mode = entry.metadata().unwrap().permissions().mode();
            fs::set_permissions(entry.path(), fs::Permissions::from_mode(mode & !0o222)).unwrap();
        }
    }

    #[test]
    fn clean_keeps_the_hidden_lockfile_and_the_lock() {
        let temp = tempfile::tempdir().unwrap();
        let state = temp.path().join("ws-0123");
        tree(&state);
        let lock = acquire(&state, LockMode::Exclusive, "clean", true, &mut |_| {}).unwrap();
        read_only(&state);
        let mut reported = Vec::new();
        clean(&state, CleanOptions::default(), &mut |line| {
            reported.push(line.to_owned())
        })
        .unwrap();
        drop(lock);
        let mut left: Vec<_> = fs::read_dir(&state)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().into_string().unwrap())
            .collect();
        left.sort();
        assert_eq!(left, ["MODULE.bazel.lock", "execroot", "lock"]);
        assert_eq!(fs::read_dir(state.join("execroot")).unwrap().count(), 0);
        assert!(reported.is_empty());
    }

    #[test]
    fn async_clean_reports_the_moved_output_tree() {
        let temp = tempfile::tempdir().unwrap();
        let state = temp.path().join("ws-0123");
        tree(&state);
        let mut reported = Vec::new();
        let options = CleanOptions {
            expunge: false,
            asynchronous: true,
        };
        clean(&state, options, &mut |line| reported.push(line.to_owned())).unwrap();
        assert_eq!(reported.len(), 1);
        let prefix = format!("Output tree moved to {}/execroot_tmp_", state.display());
        assert!(reported[0].starts_with(&prefix), "{reported:?}");
        assert!(reported[0].ends_with(" for deletion"));
        assert!(!state.join("execroot").exists());
        assert!(state.join("MODULE.bazel.lock").exists());
    }

    #[test]
    fn expunge_removes_the_state_directory_only() {
        let temp = tempfile::tempdir().unwrap();
        let state = temp.path().join("worktrees/ws-0123");
        let other = temp.path().join("worktrees/other-4567");
        tree(&state);
        tree(&other);
        read_only(&state);
        for asynchronous in [false, true] {
            if asynchronous {
                tree(&state);
            }
            let mut reported = Vec::new();
            let options = CleanOptions {
                expunge: true,
                asynchronous,
            };
            clean(&state, options, &mut |line| reported.push(line.to_owned())).unwrap();
            assert!(!state.exists());
            assert_eq!(reported.len(), usize::from(asynchronous));
            let left: Vec<_> = fs::read_dir(temp.path().join("worktrees"))
                .unwrap()
                .map(|entry| entry.unwrap().file_name().into_string().unwrap())
                .collect();
            assert_eq!(left, ["other-4567"]);
        }
        assert!(other.join("MODULE.bazel.lock").exists());
        assert!(other.join("snapshots/s").exists());
    }

    #[test]
    fn remove_tree_never_follows_symlinks_or_chmods_files() {
        let temp = tempfile::tempdir().unwrap();
        let outside = temp.path().join("outside");
        fs::create_dir(&outside).unwrap();
        fs::write(outside.join("kept"), "k").unwrap();
        fs::set_permissions(outside.join("kept"), fs::Permissions::from_mode(0o555)).unwrap();
        let doomed = temp.path().join("doomed");
        fs::create_dir(&doomed).unwrap();
        std::os::unix::fs::symlink(&outside, doomed.join("link")).unwrap();
        fs::hard_link(outside.join("kept"), doomed.join("hard")).unwrap();
        fs::set_permissions(&doomed, fs::Permissions::from_mode(0o555)).unwrap();
        remove_tree(&doomed).unwrap();
        assert!(!doomed.exists());
        assert!(outside.join("kept").exists());
        let mode = fs::metadata(outside.join("kept"))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o555);
    }

    #[test]
    fn a_shared_holder_blocks_the_exclusive_lock() {
        let temp = tempfile::tempdir().unwrap();
        let state = temp.path().join("ws-0123");
        let shared = acquire(&state, LockMode::Shared, "build", true, &mut |_| {}).unwrap();
        let also = acquire(&state, LockMode::Shared, "build", false, &mut |_| {}).unwrap();
        let mut reported = Vec::new();
        let refused = acquire(&state, LockMode::Exclusive, "clean", false, &mut |line| {
            reported.push(line.to_owned())
        });
        assert!(matches!(refused, Err(LockError::Held)));
        assert_eq!(reported.len(), 1, "{reported:?}");
        assert!(reported[0].starts_with("Another command holds the output base lock: \n"));
        drop((shared, also));
        let exclusive = acquire(&state, LockMode::Exclusive, "clean", false, &mut |_| {}).unwrap();
        let mut reported = Vec::new();
        let refused = acquire(&state, LockMode::Shared, "build", false, &mut |line| {
            reported.push(line.to_owned())
        });
        assert!(matches!(refused, Err(LockError::Held)));
        assert!(
            reported[0].contains(&format!("pid={}\nowner=clean\n", std::process::id())),
            "{reported:?}"
        );
        drop(exclusive);
        assert_eq!(fs::read(state.join(LOCK_NAME)).unwrap(), b"");
    }
}
