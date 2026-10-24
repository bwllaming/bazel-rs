//! Process-wide spawn discipline against ETXTBSY ("Text file busy").
//!
//! The race (JavaSubprocessFactory.java:124-138 describes it for Bazel): a
//! thread writes a file it will execute; while its write descriptor is open,
//! another thread starts a process. The child's descriptor table is a copy of
//! ours, so it holds the write descriptor too, until its exec closes it
//! (O_CLOEXEC). If the writer closes and executes the file in that interval,
//! execve fails with ETXTBSY. Under load the interval is unbounded: the child
//! may not be scheduled for a long time (observed > 127 ms).
//!
//! Bazel serializes process starts with a global lock held until
//! `ProcessBuilder.start()` returns (JavaSubprocessFactory.java:139-157). For
//! the JDK that is sufficient because its jspawnhelper closes every inherited
//! descriptor in user space before it execs the command, and `start()`
//! returns only after that exec. Rust's `Command::spawn` gives no such
//! guarantee: with posix_spawn (CLONE_VFORK) the parent resumes when the
//! child's execve replaces the address space (exec_mmap), BEFORE the kernel
//! closes O_CLOEXEC descriptors (do_close_on_exec, which may reschedule); on
//! the fork path the parent resumes when the CLOEXEC error pipe closes, in
//! ascending descriptor order. A lock released when `spawn` returns therefore
//! leaves a (short, but unbounded) window.
//!
//! This module closes the window at its source instead: no child can inherit
//! a write descriptor of a file that will be executed.
//! - Every process start of this crate (`spawn`) holds [`LOCK`] SHARED for
//!   the duration of `Command::spawn` (the clone happens inside it).
//! - Every in-process write of a file that is or will be executable
//!   (`exclusive`) opens, writes and CLOSES it while holding [`LOCK`]
//!   EXCLUSIVELY. Writers that are slow per byte (network downloads,
//!   archive decompression, hashing copies) write without the lock and then
//!   `seal` the file: under the exclusive lock it is re-created as a fresh
//!   inode by a kernel copy, so a start never waits for slow work (holding
//!   the lock while decompressing made starts wait ~1 s in stress runs).
//!
//! A child inherits exactly the descriptors open at its clone; a clone runs
//! under the shared lock, when no exclusive section is active, so no such
//! write descriptor exists at that moment. Exec timing, scheduling and
//! O_CLOEXEC ordering no longer matter. Starts still run concurrently with
//! each other; only executable writes serialize, and they are short and rare.
//!
//! Scope: the guarantee covers writes made through `exclusive`/`seal` and
//! starts made through `spawn`. A file written elsewhere in this process and
//! made executable later, or written by another process, is outside it, so
//! the executor keeps its ETXTBSY retry as a last resort.

use std::fs::{self, OpenOptions};
use std::io;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::{Child, Command, Output, Stdio};
use std::sync::RwLock;

static LOCK: RwLock<()> = RwLock::new(());

// This module is the only place that starts processes (clippy.toml's
// disallowed-methods; tests/spawn_discipline.rs checks src/ for the rest).

/// `command.spawn()` while no executable is open for writing in this process.
/// Only the start is guarded; never wait for the child while holding it.
///
/// Every start also registers the child's process group with the memory
/// watchdog (crate::memory_limit::register_live_group) so a watchdog kill
/// can SIGKILL live child groups before exiting. Only children started
/// in their OWN group (callers that set `process_group(0)`) are
/// registered — signalling a shared group (e.g. this process's own, for
/// short-lived grouped-in-parent helpers) would kill bazel-rs itself.
#[allow(clippy::disallowed_methods)]
pub fn spawn(command: &mut Command) -> io::Result<Child> {
    let _shared = LOCK.read().unwrap_or_else(|e| e.into_inner());
    let child = command.spawn()?;
    if crate::memory_limit::watchdog_armed() {
        let pid = child.id();
        // The caller asked for a private group iff the child is its own
        // group leader. /proc/<pid>/stat field 5 (pgrp) decides without
        // retaining caller state.
        if process_group_of(pid) == Some(pid) {
            crate::memory_limit::register_live_group(pid);
        }
    }
    Ok(child)
}

/// [`spawn`] for a spawn supervisor (crate::process_wrapper): the start is
/// guarded the same way, but the supervisor's own process group is not
/// registered with the memory watchdog — a watchdog kill of the supervisor
/// would orphan the command it supervises. The supervisor reports the
/// command's PID and the executor registers the command's group instead.
#[allow(clippy::disallowed_methods)]
pub fn spawn_supervisor(command: &mut Command) -> io::Result<Child> {
    let _shared = LOCK.read().unwrap_or_else(|e| e.into_inner());
    command.spawn()
}

/// The process-group id of `pid` from /proc/<pid>/stat (field 5; the
/// comm field may contain spaces/parens, so parse after the last ')').
/// None off Linux or when the process is already gone.
fn process_group_of(pid: u32) -> Option<u32> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let after_comm = stat.rsplit_once(')')?.1;
    // Remaining fields: state ppid pgrp ...
    after_comm.split_whitespace().nth(2)?.parse().ok()
}

/// `command.output()` with the start through [`spawn`]: stdout and stderr
/// captured (Command::output's defaults for them; the caller sets stdin),
/// the wait outside the lock.
pub fn output(command: &mut Command) -> io::Result<Output> {
    spawn(command.stdout(Stdio::piped()).stderr(Stdio::piped()))?.wait_with_output()
}

/// `CommandExt::exec` holding the shared lock: the new program image starts
/// while no executable is open for writing in this process. Returns only on
/// failure (the lock is released then).
#[allow(clippy::disallowed_methods)]
pub fn exec(command: &mut Command) -> io::Error {
    use std::os::unix::process::CommandExt;
    let _shared = LOCK.read().unwrap_or_else(|e| e.into_inner());
    command.exec()
}

/// Run `write` (which must open, write and close the file; no process start
/// inside) with no process start in flight, when `executable`; otherwise
/// just run it.
pub fn exclusive<T>(executable: bool, write: impl FnOnce() -> T) -> T {
    if !executable {
        return write();
    }
    let _exclusive = LOCK.write().unwrap_or_else(|e| e.into_inner());
    write()
}

/// Replace the regular file at `path` (written without the lock, e.g. by a
/// network download) by a fresh inode with the same bytes and mode, written
/// under the exclusive lock. Descriptors of the old inode that children may
/// still hold refer to the unlinked inode and cannot make `path` busy.
pub fn seal(path: &Path) -> io::Result<()> {
    let name = path
        .file_name()
        .ok_or_else(|| io::Error::other("cannot seal a path without a file name"))?;
    let mut temporary = name.to_os_string();
    temporary.push(format!(".sealing-{}", std::process::id()));
    let temporary = path.with_file_name(temporary);
    let mode = fs::metadata(path)?.permissions().mode() & 0o7777;
    exclusive(true, || -> io::Result<()> {
        let mut input = fs::File::open(path)?;
        let mut output = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temporary)?;
        let copied = io::copy(&mut input, &mut output)
            .and_then(|_| output.set_permissions(fs::Permissions::from_mode(mode)));
        drop(output);
        copied.and_then(|()| fs::rename(&temporary, path))
    })
    .inspect_err(|_| {
        let _ = fs::remove_file(&temporary);
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::MetadataExt;
    use std::sync::mpsc;
    use std::time::{Duration, Instant};

    #[test]
    fn no_process_starts_while_an_executable_is_open_for_writing() {
        let temp = tempfile::tempdir_in("/tmp/opencode").unwrap();
        let path = temp.path().join("tool");
        let (entered, inside) = mpsc::channel();
        let writer = std::thread::spawn(move || {
            exclusive(true, || {
                let file = fs::File::create(&path).unwrap();
                entered.send(()).unwrap();
                std::thread::sleep(Duration::from_millis(200));
                drop(file);
                Instant::now()
            })
        });
        inside.recv().unwrap();
        let mut child = spawn(&mut Command::new("/bin/true")).unwrap();
        let started = Instant::now();
        let closed = writer.join().unwrap();
        assert!(child.wait().unwrap().success());
        // The start waited for the write descriptor to be closed, so the
        // child cannot have inherited it.
        assert!(started >= closed);
        // A non-executable write takes no lock.
        assert_eq!(exclusive(false, || 7), 7);
    }

    #[test]
    fn seal_recreates_the_file_with_the_same_bytes_and_mode() {
        let temp = tempfile::tempdir_in("/tmp/opencode").unwrap();
        let path = temp.path().join("tool");
        fs::write(&path, b"#!/bin/sh\necho sealed\n").unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o751)).unwrap();
        let before = fs::metadata(&path).unwrap();
        // A descriptor that a child could still hold refers to the old inode.
        let stale = OpenOptions::new().append(true).open(&path).unwrap();
        seal(&path).unwrap();
        let after = fs::metadata(&path).unwrap();
        assert_ne!(after.ino(), before.ino());
        assert_eq!(after.permissions().mode() & 0o7777, 0o751);
        assert_eq!(fs::read(&path).unwrap(), b"#!/bin/sh\necho sealed\n");
        let output = spawn(
            Command::new(&path)
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::null()),
        )
        .unwrap()
        .wait_with_output()
        .unwrap();
        assert_eq!(output.stdout, b"sealed\n");
        drop(stale);
        assert_eq!(fs::read_dir(temp.path()).unwrap().count(), 1);
    }
}
