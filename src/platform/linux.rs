//! Linux (the reference host): procfs and `O_PATH`, exactly as the modules
//! used them before the platform layer existed.

use std::os::fd::{AsFd, OwnedFd};
use std::path::Path;

/// The host's name in [`super::not_implemented`] messages.
pub const HOST_NAME: &str = "Linux";

/// The running binary, for re-executing it (the process-wrapper, the sandbox
/// worker, persistent workers, the incremental-analysis fallback) and for
/// its identity: `/proc/self/exe` names it even when its path was replaced
/// or deleted since it started.
pub fn self_exe() -> &'static Path {
    Path::new("/proc/self/exe")
}

/// Whether a process (or zombie) with this PID exists: its /proc entry.
pub fn process_exists(pid: u32) -> bool {
    Path::new(&format!("/proc/{pid}")).exists()
}

/// The process-group id of `pid` from /proc/<pid>/stat (field 5; the comm
/// field may contain spaces/parens, so parse after the last ')'). None when
/// the process is already gone.
pub fn process_group_of(pid: u32) -> Option<u32> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let after_comm = stat.rsplit_once(')')?.1;
    // Remaining fields: state ppid pgrp ...
    after_comm.split_whitespace().nth(2)?.parse().ok()
}

/// `path` below `directory` opened for lookups and `fstat` only
/// (`O_PATH | O_DIRECTORY`): no read permission needed, nothing read.
pub fn open_lookup_directory<P: rustix::path::Arg>(
    directory: impl AsFd,
    path: P,
) -> rustix::io::Result<OwnedFd> {
    use rustix::fs::{Mode, OFlags, openat};
    openat(
        directory,
        path,
        OFlags::PATH | OFlags::DIRECTORY | OFlags::CLOEXEC,
        Mode::empty(),
    )
}

/// A directory below `directory` opened for fd-relative `lstat`s of its
/// entries ([`open_entry_nofollow`] then `fstat`): [`open_lookup_directory`].
pub fn open_lstat_directory<P: rustix::path::Arg>(
    directory: impl AsFd,
    path: P,
) -> rustix::io::Result<OwnedFd> {
    open_lookup_directory(directory, path)
}

/// `name` below `directory` opened as a path (`O_PATH | O_NOFOLLOW`: a
/// symlink as itself), for an `fstat` that equals `lstat`.
pub fn open_entry_nofollow<P: rustix::path::Arg>(
    directory: impl AsFd,
    name: P,
) -> rustix::io::Result<OwnedFd> {
    use rustix::fs::{Mode, OFlags, openat};
    openat(
        directory,
        name,
        OFlags::PATH | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    )
}

/// The kernel-level ceiling of a resource limit that Bazel's client
/// `GetExplicitSystemLimit` consults: not implemented on Linux
/// (blaze_util_linux.cc:231 returns -1), so the soft limit becomes the hard
/// one as-is.
pub fn explicit_system_limit(_resource: rustix::process::Resource) -> Option<u64> {
    None
}
