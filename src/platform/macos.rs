//! macOS (aarch64-apple-darwin). No procfs: what Linux reads from /proc
//! comes from the safe wrappers of the system calls (rustix, nix) or, for
//! the few `sysctl` values, from one run of `/usr/sbin/sysctl` per process.
//! No `O_PATH`: directories are opened for reading instead.

use std::collections::BTreeMap;
use std::os::fd::{AsFd, OwnedFd};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::Duration;

/// The host's name in [`super::not_implemented`] messages.
pub const HOST_NAME: &str = "macOS";

/// The running binary (`_NSGetExecutablePath` through
/// std::env::current_exe), resolved once: macOS has no handle on the
/// running image like Linux's /proc/self/exe, so a binary replaced at its
/// path after this process started is the new one when re-executed.
pub fn self_exe() -> &'static Path {
    static EXE: OnceLock<PathBuf> = OnceLock::new();
    EXE.get_or_init(|| {
        std::env::current_exe()
            // snapshot-unrecorded: this binary's own path (re-execution, its identity).
            .and_then(|path| path.canonicalize())
            .unwrap_or_else(|_| PathBuf::from(std::env::args_os().next().unwrap_or_default()))
    })
}

/// An absolute root (`--cache-dir`) with the symlinks of its longest
/// existing prefix resolved, like genuine's Canonicalize of its output base
/// (blaze.cc:1317): macOS's temporary directories live below the root
/// symlinks /tmp and /var (to /private/...), which the caches' no-symlink
/// checks would refuse. Relative paths (joined to the canonical working
/// directory later) and unresolvable ones stay as given.
pub fn canonical_root(path: &Path) -> PathBuf {
    if !path.is_absolute() {
        return path.to_path_buf();
    }
    let mut existing = path;
    let mut rest = Vec::new();
    loop {
        // snapshot-unrecorded: the cache root's own location (startup).
        if let Ok(canonical) = existing.canonicalize() {
            let mut result = canonical;
            result.extend(rest.iter().rev());
            return result;
        }
        match (existing.parent(), existing.file_name()) {
            (Some(parent), Some(name)) => {
                rest.push(name.to_owned());
                existing = parent;
            }
            _ => return path.to_path_buf(),
        }
    }
}

fn pid(pid: u32) -> Option<rustix::process::Pid> {
    i32::try_from(pid)
        .ok()
        .and_then(rustix::process::Pid::from_raw)
}

/// Whether a process (or zombie) with this PID exists: kill(pid, 0)
/// succeeds or is refused (EPERM: someone else's process).
pub fn process_exists(pid_value: u32) -> bool {
    let Some(pid) = pid(pid_value) else {
        return false;
    };
    match rustix::process::test_kill_process(pid) {
        Ok(()) | Err(rustix::io::Errno::PERM) => true,
        Err(_) => false,
    }
}

/// Tests: whether `pid` is a live process (not a zombie), by `ps`.
#[cfg(test)]
pub fn test_process_alive(pid: u32) -> bool {
    let mut command = std::process::Command::new("/bin/ps");
    command.args(["-o", "stat=", "-p", &pid.to_string()]);
    crate::spawn_lock::output(&mut command).is_ok_and(|output| {
        let state = String::from_utf8_lossy(&output.stdout);
        let state = state.trim();
        !state.is_empty() && !state.starts_with('Z')
    })
}

/// The process-group id of `pid` (getpgid); None when it is gone.
pub fn process_group_of(pid_value: u32) -> Option<u32> {
    let group = rustix::process::getpgid(Some(pid(pid_value)?)).ok()?;
    u32::try_from(group.as_raw_nonzero().get()).ok()
}

/// `path` below `directory` opened as a directory for lookups and `fstat`
/// (`O_RDONLY | O_DIRECTORY`: macOS has no `O_PATH`, so an unreadable
/// directory fails to open and callers inspect it by path).
pub fn open_lookup_directory<P: rustix::path::Arg>(
    directory: impl AsFd,
    path: P,
) -> rustix::io::Result<OwnedFd> {
    use rustix::fs::{Mode, OFlags, openat};
    openat(
        directory,
        path,
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC,
        Mode::empty(),
    )
}

/// Fd-relative `lstat`s need [`open_entry_nofollow`], which macOS lacks:
/// never opened (NOTSUP), so callers inspect entries by path.
pub fn open_lstat_directory<P: rustix::path::Arg>(
    _directory: impl AsFd,
    _path: P,
) -> rustix::io::Result<OwnedFd> {
    Err(rustix::io::Errno::NOTSUP)
}

/// `O_PATH | O_NOFOLLOW` has no macOS counterpart (opening a symlink itself
/// needs `O_SYMLINK`, opening an unreadable file fails): callers that would
/// `fstat` such a descriptor inspect the entry by path instead.
pub fn open_entry_nofollow<P: rustix::path::Arg>(
    _directory: impl AsFd,
    _name: P,
) -> rustix::io::Result<OwnedFd> {
    Err(rustix::io::Errno::NOTSUP)
}

/// The `sysctl` values bazel-rs reads (Bazel's client reads the same with
/// sysctlbyname): one `/usr/sbin/sysctl -n` run per process, on first use.
const SYSCTLS: [&str; 3] = ["kern.maxfilesperproc", "kern.maxprocperuid", "hw.memsize"];

/// One of [`SYSCTLS`]' values (None: unreadable).
pub fn sysctl(name: &str) -> Option<u64> {
    static VALUES: OnceLock<BTreeMap<&'static str, u64>> = OnceLock::new();
    VALUES
        .get_or_init(|| {
            let mut command = std::process::Command::new("/usr/sbin/sysctl");
            command
                .arg("-n")
                .args(SYSCTLS)
                .stdin(std::process::Stdio::null());
            let Ok(output) = crate::spawn_lock::output(&mut command) else {
                return BTreeMap::new();
            };
            if !output.status.success() {
                return BTreeMap::new();
            }
            String::from_utf8_lossy(&output.stdout)
                .lines()
                .zip(SYSCTLS)
                .filter_map(|(line, name)| Some((name, line.trim().parse().ok()?)))
                .collect()
        })
        .get(name)
        .copied()
}

/// Bazel's client `GetExplicitSystemLimit` on macOS
/// (blaze_util_darwin.cc:277-305): `kern.maxfilesperproc` for
/// RLIMIT_NOFILE, `kern.maxprocperuid` for RLIMIT_NPROC — macOS refuses a
/// soft limit above them (an unlimited hard limit included).
pub fn explicit_system_limit(resource: rustix::process::Resource) -> Option<u64> {
    use rustix::process::Resource;
    match resource {
        Resource::Nofile => sysctl("kern.maxfilesperproc"),
        Resource::Nproc => sysctl("kern.maxprocperuid"),
        _ => None,
    }
    .filter(|limit| *limit > 0)
}

/// Total machine memory in bytes (`hw.memsize`).
pub fn mem_total_bytes() -> Option<u64> {
    sysctl("hw.memsize")
}

fn rusage(who: nix::sys::resource::UsageWho) -> Option<nix::sys::resource::Usage> {
    nix::sys::resource::getrusage(who).ok()
}

/// Peak resident set size of this process in bytes (getrusage
/// `ru_maxrss`, which macOS reports in bytes).
pub fn peak_rss_bytes() -> Option<u64> {
    let usage = rusage(nix::sys::resource::UsageWho::RUSAGE_SELF)?;
    u64::try_from(usage.max_rss())
        .ok()
        .filter(|bytes| *bytes > 0)
}

fn duration(time: nix::sys::time::TimeVal) -> Duration {
    Duration::from_secs(u64::try_from(time.tv_sec()).unwrap_or(0))
        + Duration::from_micros(u64::try_from(time.tv_usec()).unwrap_or(0))
}

/// User and system CPU time of `who`.
fn cpu(who: nix::sys::resource::UsageWho) -> Option<(Duration, Duration)> {
    let usage = rusage(who)?;
    Some((duration(usage.user_time()), duration(usage.system_time())))
}

/// This process's user and system CPU time.
pub fn self_cpu_times() -> Option<(Duration, Duration)> {
    cpu(nix::sys::resource::UsageWho::RUSAGE_SELF)
}

/// User and system CPU time of this process's waited-for children.
pub fn children_cpu_times() -> Option<(Duration, Duration)> {
    cpu(nix::sys::resource::UsageWho::RUSAGE_CHILDREN)
}

/// The host name (gethostname, as the JVM's
/// InetAddress.getLocalHost().getHostName() starts from).
pub fn host_name() -> Option<String> {
    let name = rustix::system::uname()
        .nodename()
        .to_string_lossy()
        .into_owned();
    (!name.is_empty()).then_some(name)
}

/// The user database's home directory of this process's user (the JVM's
/// `user.home`, which genuine's darwin-sandbox reads instead of `$HOME`).
pub fn user_home() -> Option<PathBuf> {
    nix::unistd::User::from_uid(nix::unistd::getuid())
        .ok()
        .flatten()
        .map(|user| user.dir)
}
