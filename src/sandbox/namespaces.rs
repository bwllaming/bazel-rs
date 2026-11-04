//! The namespace sandbox: Bazel 9.3's `linux-sandbox` (docs/SANDBOX.md).
//!
//! Genuine `linux-sandbox` (src/main/tools/linux-sandbox.cc,
//! linux-sandbox-pid1.cc) clones its pid 1 into fresh user, mount, IPC and
//! PID namespaces — plus a network namespace when the spawn must not reach
//! the network and a UTS namespace for `--sandbox_fake_hostname` — and, in
//! that pid 1, makes the whole host filesystem read-only except the sandbox's
//! writable paths, mounts the requested tmpfs and bind mounts, a fresh
//! `/proc` (and `/sys` with a network namespace), then forks the action as
//! pid 2 and reaps everything until it exits; leaving pid 1 kills whatever
//! is left in the namespace. This module is that tool, in three parts:
//!
//! - the executor half ([`plan`]): LinuxSandboxedSpawnRunner.prepareSpawn
//!   and LinuxSandboxCommandLineBuilder — the writable directories
//!   (getWritableDirs), the bind mounts (`--sandbox_add_mount_pair`,
//!   `--sandbox_block_path` over inaccessible helpers, validateBindMounts)
//!   and the hermetic `/tmp` (useHermeticTmp: the spawn's own empty
//!   `_hermetic_tmp` mounted at `/tmp`, with the bazel-rs roots under `/tmp`
//!   mounted back into it), written as the sandbox worker's arguments;
//! - the worker half ([`enter`]), run by the `__sandbox-worker` process
//!   before it starts pid 1: unshare the namespaces, map the user, make the
//!   mount namespace private and set up the mounts exactly in
//!   MountFilesystems / MakeFilesystemMostlyReadOnly order. The worker stays
//!   outside the new PID namespace as the spawn's supervisor
//!   (crate::process_wrapper::run_supervised), so the executor's control
//!   channel, test timeouts and the memory watchdog reach pid 1 unchanged;
//! - pid 1 ([`spawn_below_pid1`]): the worker's child, the first process of
//!   the new PID namespace, mounts the fresh `/proc` between fork and exec
//!   (where it still holds the worker's capabilities), forks the action as
//!   pid 2 with umask 022, forwards SIGTERM to it, reaps every process of the
//!   namespace and exits with the action's code or 128 + its signal
//!   (WaitForChild, linux-sandbox.cc WaitForPid1).
//!
//! Whether the namespaces can be created is probed once per invocation like
//! LinuxSandboxedSpawnRunner.isSupported (the linux-sandbox runs /bin/true):
//! [`probe`] runs `/bin/true` through the same worker. Hosts that deny
//! unprivileged user namespaces (Docker's default seccomp profile, Ubuntu's
//! `apparmor_restrict_unprivileged_userns`) or that cannot mount a fresh
//! `/proc` in one fail the probe, and the `sandboxed` strategy falls back
//! (crate::sandbox::Backend).
//!
//! `--experimental_use_hermetic_linux_sandbox` (pivot_root into a hardlinked
//! tree) is not modelled.

#![allow(unsafe_code)]

use std::ffi::{CString, OsStr};
use std::fs;
use std::io;
use std::os::fd::FromRawFd;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicI32, Ordering};

/// The test-only knob (docs/SANDBOX.md): `off` never uses the namespaces
/// (the fallback sandbox runs instead), `bind-proc` lets pid 1 bind the
/// host's `/proc` (and `/sys`) when a fresh mount is refused — container
/// runtimes such as sysbox overmount `/proc` files, which the kernel's
/// mount_too_revealing check then refuses in a nested user namespace, so the
/// namespace path can be exercised there; genuine linux-sandbox (and the
/// default here) fails the probe on such hosts instead.
pub const KNOB: &str = "BAZEL_RS_SANDBOX_NAMESPACES";

/// LinuxSandboxCommandLineBuilder.NetworkNamespace.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum Netns {
    /// NO_NETNS: the host network.
    #[default]
    None,
    /// NETNS (`-n`): a fresh network namespace without any interface up.
    Plain,
    /// NETNS_WITH_LOOPBACK (`-N`): a fresh network namespace with `lo` up
    /// (`--sandbox_enable_loopback_device`, default true).
    WithLoopback,
}

/// The linux-sandbox command line for one spawn: what the worker sets up.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Plan {
    /// `-W`: the working directory (the spawn's execroot), bound onto itself.
    pub working_dir: PathBuf,
    /// `-w`: writable files and directories, in Java's TreeSet order.
    pub writable: Vec<PathBuf>,
    /// `-e`: empty tmpfs mounts.
    pub tmpfs: Vec<PathBuf>,
    /// `-M`/`-m`: bind mounts as (source, target), in mount order.
    pub binds: Vec<(PathBuf, PathBuf)>,
    /// `-n` / `-N`.
    pub netns: Netns,
    /// `-H`: hostname `localhost`, domain name `localdomain`.
    pub fake_hostname: bool,
    /// `-U`: run as `nobody`.
    pub fake_username: bool,
    /// `-R`: run as root (the `requires-fakeroot` execution requirement).
    pub fake_root: bool,
    /// Where the worker mounts the tmpfs holding the inaccessible helper
    /// file and directory that `--sandbox_block_path` entries are bound
    /// from (SandboxHelpers.INACCESSIBLE_HELPER_{FILE,DIR}); None when no
    /// path is blocked.
    pub helpers: Option<PathBuf>,
    /// Let pid 1 bind the host `/proc` and `/sys` when a fresh mount is
    /// refused ([`KNOB`] `bind-proc`).
    pub proc_fallback: bool,
}

/// The helper names below [`Plan::helpers`].
const HELPER_DIR: &str = "inaccessibleHelperDir";
const HELPER_FILE: &str = "inaccessibleHelperFile";

/// What [`plan`] needs about one spawn and the invocation.
pub struct PlanInput<'a> {
    /// The spawn's execroot (sandboxExecRoot; also the working directory).
    pub execroot: &'a Path,
    /// The spawn's sandbox directory (sandboxPath): the hermetic tmp and the
    /// helper mount point are created below it.
    pub sandbox_dir: &'a Path,
    /// The spawn's TMPDIR (PosixLocalEnvProvider: the client's, or /tmp).
    pub tmpdir: &'a Path,
    /// The spawn environment's TEST_TMPDIR.
    pub test_tmpdir: Option<&'a str>,
    /// `--sandbox_writable_path`.
    pub writable_paths: &'a [PathBuf],
    /// `--sandbox_tmpfs_path`.
    pub tmpfs_paths: &'a [PathBuf],
    /// `--sandbox_add_mount_pair` as (source, target), flag order.
    pub mount_pairs: &'a [(String, String)],
    /// `--sandbox_block_path`.
    pub block_paths: &'a [PathBuf],
    /// The paths bazel-rs manages (cache, output base, workspace): those
    /// under /tmp are mounted into the hermetic /tmp
    /// (collectPathsToMountUnderHermeticTmp).
    pub known_roots: &'a [PathBuf],
    pub netns: Netns,
    pub fake_hostname: bool,
    pub fake_username: bool,
    pub fake_root: bool,
    pub proc_fallback: bool,
}

/// Java's Path ordering (string comparison of the path strings).
fn sort_paths(paths: &mut Vec<PathBuf>) {
    paths.sort_by(|a, b| a.as_os_str().as_bytes().cmp(b.as_os_str().as_bytes()));
    paths.dedup();
}

/// PathFragment.getRelative for an absolute or relative string.
fn relative_to(base: &Path, path: &str) -> PathBuf {
    if path.starts_with('/') {
        normalize(Path::new(path))
    } else {
        normalize(&base.join(path))
    }
}

/// PathFragment normalization: `.` dropped, `..` applied lexically.
fn normalize(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            std::path::Component::ParentDir => {
                out.pop();
            }
            std::path::Component::CurDir => {}
            other => out.push(other.as_os_str()),
        }
    }
    out
}

/// AbstractSandboxSpawnRunner.addWritablePath.
fn add_writable_path(
    execroot: &Path,
    writable: &mut Vec<PathBuf>,
    value: &str,
    variable: &str,
) -> Result<(), String> {
    let path = relative_to(execroot, value);
    if path.starts_with(execroot) {
        writable.push(path);
    } else if path.exists() {
        writable.push(fs::canonicalize(&path).unwrap_or(path));
    } else {
        return Err(format!(
            "Cannot resolve symlinks in {variable} because it doesn't exist: \"{}\"",
            path.display()
        ));
    }
    Ok(())
}

/// The first two segments of an absolute path (`/tmp/dir1/dir2` ->
/// `/tmp/dir1`).
fn first_two_segments(path: &Path) -> PathBuf {
    path.components().take(3).collect()
}

/// LinuxSandboxedSpawnRunner.prepareSpawn's sandbox layout for one spawn:
/// the writable directories, bind mounts and hermetic /tmp, with the
/// directories genuine creates on the host (the hermetic tmp, mount points
/// below it) created here. Errors are the spawn's (UserExecException /
/// IOException texts of genuine).
pub fn plan(input: &PlanInput) -> Result<Plan, String> {
    let slash_tmp = Path::new("/tmp");
    let execroot = input.execroot;
    // getWritableDirs (AbstractSandboxSpawnRunner:352-405 and
    // LinuxSandboxedSpawnRunner:390-398), a TreeSet.
    let mut writable = vec![execroot.to_path_buf()];
    if let Some(test_tmpdir) = input.test_tmpdir {
        add_writable_path(execroot, &mut writable, test_tmpdir, "TEST_TMPDIR")?;
    }
    add_writable_path(
        execroot,
        &mut writable,
        &input.tmpdir.to_string_lossy(),
        "TMPDIR",
    )?;
    for path in input.writable_paths {
        writable.push(path.clone());
        if let Ok(resolved) = fs::canonicalize(path) {
            writable.push(resolved);
        }
    }
    let shm = Path::new("/dev/shm");
    writable.push(fs::canonicalize(shm).unwrap_or_else(|_| shm.to_path_buf()));
    writable.push(slash_tmp.to_path_buf());
    sort_paths(&mut writable);

    let mut tmpfs: Vec<PathBuf> = input.tmpfs_paths.to_vec();
    sort_paths(&mut tmpfs);

    // collectPathsToMountUnderHermeticTmp.
    let mut known: Vec<PathBuf> = input
        .known_roots
        .iter()
        .map(|path| normalize(path))
        .filter(|path| path.starts_with(slash_tmp))
        .map(|path| first_two_segments(&path))
        .collect();
    sort_paths(&mut known);
    // useHermeticTmp.
    let hermetic = !input.mount_pairs.iter().any(|(source, _)| source == "/tmp")
        && !known.iter().any(|path| path == slash_tmp)
        && !tmpfs.iter().any(|path| path == slash_tmp);
    let sandbox_tmp = input.sandbox_dir.join("_hermetic_tmp");
    if hermetic {
        create_dirs(&sandbox_tmp)?;
        for path in tmpfs.iter().chain(&writable) {
            if let Ok(rest) = path.strip_prefix(slash_tmp) {
                create_dirs(&sandbox_tmp.join(rest))?;
            }
        }
    }

    // prepareAndGetBindMounts: the user mounts (keyed by source, the last
    // pair for a source winning: ImmutableMap.buildKeepingLast), then target
    // -> source in a TreeMap (a later source for the same target wins).
    let mut by_source: Vec<(String, String)> = Vec::new();
    for (source, target) in input.mount_pairs {
        if let Some(entry) = by_source.iter_mut().find(|(s, _)| s == source) {
            entry.1 = target.clone();
        } else {
            by_source.push((source.clone(), target.clone()));
        }
    }
    let mut user_binds: Vec<(PathBuf, PathBuf)> = Vec::new(); // (target, source)
    let put = |binds: &mut Vec<(PathBuf, PathBuf)>, target: PathBuf, source: PathBuf| {
        if let Some(entry) = binds.iter_mut().find(|(t, _)| *t == target) {
            entry.1 = source;
        } else {
            binds.push((target, source));
        }
    };
    for (source, target) in &by_source {
        if !target.starts_with('/') {
            return Err(format!(
                "Error occurred when analyzing bind mount pairs. {target} (expected an absolute path)"
            ));
        }
        put(
            &mut user_binds,
            normalize(Path::new(target)),
            relative_to(execroot, source),
        );
    }
    let helpers =
        (!input.block_paths.is_empty()).then(|| input.sandbox_dir.join("sandbox-helpers"));
    let mut helper_sources = Vec::new();
    if let Some(helpers) = &helpers {
        create_dirs(helpers)?;
        for blocked in input.block_paths {
            // SandboxOptions.getInaccessiblePaths: the resolved path, or the
            // path itself when it cannot be resolved.
            let path = fs::canonicalize(blocked).unwrap_or_else(|_| blocked.clone());
            if !path.exists() {
                continue;
            }
            let source = if fs::symlink_metadata(&path).is_ok_and(|m| m.is_dir()) {
                helpers.join(HELPER_DIR)
            } else {
                helpers.join(HELPER_FILE)
            };
            helper_sources.push(source.clone());
            put(&mut user_binds, path, source);
        }
    }
    user_binds.sort_by(|a, b| a.0.as_os_str().as_bytes().cmp(b.0.as_os_str().as_bytes()));
    validate_bind_mounts(&user_binds, &helper_sources)?;

    let binds = if hermetic {
        let mut binds: Vec<(PathBuf, PathBuf)> = Vec::new();
        let entries = user_binds
            .iter()
            .cloned()
            .chain(known.iter().map(|path| (path.clone(), path.clone())));
        for (target, source) in entries {
            let target = match target.strip_prefix(slash_tmp) {
                Ok(rest) => {
                    if rest.as_os_str().is_empty() {
                        return Err("Cannot mount /tmp explicitly with hermetic /tmp. Please file a bug at https://github.com/bazelbuild/bazel/issues/new/choose.".to_owned());
                    }
                    let target = sandbox_tmp.join(rest);
                    create_dirs(&target)?;
                    target
                }
                Err(_) => target,
            };
            put(&mut binds, target, source);
        }
        binds.sort_by(|a, b| a.0.as_os_str().as_bytes().cmp(b.0.as_os_str().as_bytes()));
        binds.push((slash_tmp.to_path_buf(), sandbox_tmp.clone()));
        binds
    } else {
        user_binds
    };
    Ok(Plan {
        working_dir: execroot.to_path_buf(),
        writable,
        tmpfs,
        binds: binds
            .into_iter()
            .map(|(target, source)| (source, target))
            .collect(),
        netns: input.netns,
        fake_hostname: input.fake_hostname,
        fake_username: input.fake_username && !input.fake_root,
        fake_root: input.fake_root,
        helpers,
        proc_fallback: input.proc_fallback,
    })
}

fn create_dirs(path: &Path) -> Result<(), String> {
    fs::create_dir_all(path).map_err(|error| {
        format!(
            "cannot create sandbox directory '{}': {error}",
            path.display()
        )
    })
}

/// LinuxSandboxUtil.validateBindMounts over (target, source) pairs; the
/// inaccessible helpers only exist inside the sandbox (the worker creates
/// them), so their sources are not checked.
fn validate_bind_mounts(
    binds: &[(PathBuf, PathBuf)],
    helper_sources: &[PathBuf],
) -> Result<(), String> {
    for (target, source) in binds {
        let helper = helper_sources.contains(source);
        if !helper && !source.exists() {
            return Err(format!(
                "Mount source '{}' does not exist.",
                source.display()
            ));
        }
        if target.exists() {
            if helper {
                continue;
            }
            let both_directories = source.is_dir() && target.is_dir();
            let is_link = |path: &Path| fs::symlink_metadata(path).is_ok_and(|m| m.is_symlink());
            let source_file = source.is_file() || is_link(source);
            let target_file = target.is_file() || is_link(target);
            if !(both_directories || source_file && target_file) {
                let kind = |file: bool| if file { "file" } else { "directory" };
                return Err(format!(
                    "Mount target '{}' is a {} but mount source '{}' is a {}, they must be the same type.",
                    target.display(),
                    kind(target_file),
                    source.display(),
                    kind(source_file)
                ));
            }
        } else {
            return Err(format!(
                "Mount target '{}' does not exist. Bazel only supports bind mounting on top of existing files/directories. Please create an empty file or directory at the mount target path according to the type of mount source.",
                target.display()
            ));
        }
    }
    Ok(())
}

impl Plan {
    /// The worker's arguments for this plan (after `--namespaces`).
    pub fn push_args(&self, argv: &mut Vec<String>) {
        let display = |path: &Path| path.display().to_string();
        argv.push("-W".into());
        argv.push(display(&self.working_dir));
        for path in &self.writable {
            argv.push("-w".into());
            argv.push(display(path));
        }
        for path in &self.tmpfs {
            argv.push("-e".into());
            argv.push(display(path));
        }
        for (source, target) in &self.binds {
            argv.push("-M".into());
            argv.push(display(source));
            argv.push("-m".into());
            argv.push(display(target));
        }
        match self.netns {
            Netns::None => {}
            Netns::Plain => argv.push("-n".into()),
            Netns::WithLoopback => argv.push("-N".into()),
        }
        if self.fake_hostname {
            argv.push("-H".into());
        }
        if self.fake_root {
            argv.push("-R".into());
        }
        if self.fake_username {
            argv.push("-U".into());
        }
        if let Some(helpers) = &self.helpers {
            argv.push("--helpers".into());
            argv.push(display(helpers));
        }
        if self.proc_fallback {
            argv.push("--proc-fallback".into());
        }
    }

    /// Parses one plan option of the worker's command line at `args[*index]`
    /// (advancing past its value); Ok(false) when the argument is not one.
    pub fn parse_arg(&mut self, args: &[String], index: &mut usize) -> Result<bool, String> {
        let value = |index: &mut usize| -> Result<PathBuf, String> {
            *index += 1;
            args.get(*index)
                .map(PathBuf::from)
                .ok_or_else(|| format!("sandbox worker: {} requires a value", args[*index - 1]))
        };
        match args[*index].as_str() {
            "-W" => self.working_dir = value(index)?,
            "-w" => self.writable.push(value(index)?),
            "-e" => self.tmpfs.push(value(index)?),
            "-M" => {
                let source = value(index)?;
                if args.get(*index + 1).map(String::as_str) != Some("-m") {
                    return Err("sandbox worker: -M requires -m".into());
                }
                *index += 1;
                let target = value(index)?;
                self.binds.push((source, target));
            }
            "-n" => self.netns = Netns::Plain,
            "-N" => self.netns = Netns::WithLoopback,
            "-H" => self.fake_hostname = true,
            "-R" => self.fake_root = true,
            "-U" => self.fake_username = true,
            "--helpers" => self.helpers = Some(value(index)?),
            "--proc-fallback" => self.proc_fallback = true,
            _ => return Ok(false),
        }
        Ok(true)
    }
}

// ---------------------------------------------------------------------------
// The worker half.

fn cstring(path: &Path) -> Result<CString, String> {
    CString::new(path.as_os_str().as_bytes())
        .map_err(|_| format!("path '{}' contains a NUL byte", path.display()))
}

fn errno() -> io::Error {
    io::Error::last_os_error()
}

/// mount(2) with optional source, filesystem type and data.
fn mount(
    source: Option<&Path>,
    target: &Path,
    fstype: Option<&str>,
    flags: libc::c_ulong,
    data: Option<&str>,
) -> io::Result<()> {
    let source = source.map(cstring).transpose().map_err(io::Error::other)?;
    let target = cstring(target).map_err(io::Error::other)?;
    let fstype = fstype.map(|value| CString::new(value).expect("literal"));
    let data = data.map(|value| CString::new(value).expect("literal"));
    // SAFETY: every pointer is either null or a NUL-terminated string that
    // outlives the call.
    let result = unsafe {
        libc::mount(
            source.as_ref().map_or(std::ptr::null(), |s| s.as_ptr()),
            target.as_ptr(),
            fstype.as_ref().map_or(std::ptr::null(), |s| s.as_ptr()),
            flags,
            data.as_ref()
                .map_or(std::ptr::null(), |s| s.as_ptr().cast()),
        )
    };
    if result < 0 { Err(errno()) } else { Ok(()) }
}

/// The `nobody` user's uid and gid (getpwnam_r, SetupUserNamespace).
fn nobody() -> Result<(libc::uid_t, libc::gid_t), String> {
    let name = CString::new("nobody").expect("literal");
    let mut buffer = vec![0 as libc::c_char; 4096];
    // SAFETY: `pwd` is plain data the call fills in; `buffer` outlives the
    // call and its length is passed; `result` is an out pointer.
    unsafe {
        let mut pwd: libc::passwd = std::mem::zeroed();
        let mut result: *mut libc::passwd = std::ptr::null_mut();
        let code = libc::getpwnam_r(
            name.as_ptr(),
            &mut pwd,
            buffer.as_mut_ptr(),
            buffer.len(),
            &mut result,
        );
        if code != 0 || result.is_null() {
            return Err("getpwnam_r for nobody failed".into());
        }
        Ok((pwd.pw_uid, pwd.pw_gid))
    }
}

/// One /proc/self/mounts entry's mount point and options (getmntent's
/// fields, octal escapes decoded).
fn mount_entries() -> Result<Vec<(PathBuf, Vec<String>)>, String> {
    let text = fs::read("/proc/self/mounts")
        .map_err(|error| format!("setmntent(/proc/self/mounts): {error}"))?;
    let decode = |field: &[u8]| -> Vec<u8> {
        let mut out = Vec::with_capacity(field.len());
        let mut index = 0;
        while index < field.len() {
            if field[index] == b'\\'
                && index + 3 < field.len()
                && field[index + 1..index + 4]
                    .iter()
                    .all(|b| (b'0'..=b'7').contains(b))
            {
                let value = (field[index + 1] - b'0') * 64
                    + (field[index + 2] - b'0') * 8
                    + (field[index + 3] - b'0');
                out.push(value);
                index += 4;
            } else {
                out.push(field[index]);
                index += 1;
            }
        }
        out
    };
    let mut entries = Vec::new();
    for line in text.split(|byte| *byte == b'\n') {
        let fields: Vec<&[u8]> = line.split(|byte| *byte == b' ').collect();
        if fields.len() < 4 {
            continue;
        }
        let dir = PathBuf::from(OsStr::from_bytes(&decode(fields[1])));
        let options = String::from_utf8_lossy(&decode(fields[3]))
            .split(',')
            .map(str::to_owned)
            .collect();
        entries.push((dir, options));
    }
    Ok(entries)
}

/// The worker's namespace setup for `plan` (linux-sandbox.cc SpawnPid1's
/// clone flags and linux-sandbox-pid1.cc Pid1Main up to the fork of the
/// action, minus the /proc mount, which pid 1 makes): afterwards the next
/// child of this process is pid 1 of a fresh PID namespace. `debug` receives
/// PRINT_DEBUG's lines.
pub fn enter(plan: &Plan, debug: &dyn Fn(&str)) -> Result<(), String> {
    // SAFETY: getuid/getgid cannot fail.
    let (outer_uid, outer_gid) = unsafe { (libc::getuid(), libc::getgid()) };
    let mut flags = libc::CLONE_NEWUSER | libc::CLONE_NEWNS | libc::CLONE_NEWIPC;
    if plan.netns != Netns::None {
        flags |= libc::CLONE_NEWNET;
    }
    if plan.fake_hostname {
        flags |= libc::CLONE_NEWUTS;
    }
    debug(&format!("Netns is {:?}", plan.netns));
    // SAFETY: unshare(2) takes plain flags; the worker is single-threaded
    // here (CLONE_NEWUSER requires it), it has started no thread yet.
    if unsafe { libc::unshare(flags) } < 0 {
        return Err(format!("unshare: {}", errno()));
    }
    debug("unshare done");
    // SetupUserNamespace.
    match fs::write("/proc/self/setgroups", "deny") {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(format!("write /proc/self/setgroups: {error}")),
    }
    let (inner_uid, inner_gid) = if plan.fake_root {
        (0, 0)
    } else if plan.fake_username {
        nobody()?
    } else {
        (outer_uid, outer_gid)
    };
    fs::write("/proc/self/uid_map", format!("{inner_uid} {outer_uid} 1\n"))
        .map_err(|error| format!("write /proc/self/uid_map: {error}"))?;
    fs::write("/proc/self/gid_map", format!("{inner_gid} {outer_gid} 1\n"))
        .map_err(|error| format!("write /proc/self/gid_map: {error}"))?;
    debug("user namespace mapped");
    // SetupMountNamespace.
    mount(
        None,
        Path::new("/"),
        None,
        libc::MS_REC | libc::MS_PRIVATE,
        None,
    )
    .map_err(|error| format!("mount: {error}"))?;
    if plan.fake_hostname {
        // SetupUtsNamespace.
        // SAFETY: the buffers are static literals with their lengths.
        unsafe {
            if libc::sethostname(c"localhost".as_ptr(), 9) < 0 {
                return Err(format!("sethostname: {}", errno()));
            }
            if libc::setdomainname(c"localdomain".as_ptr(), 11) < 0 {
                return Err(format!("setdomainname: {}", errno()));
            }
        }
    }
    if let Some(helpers) = &plan.helpers {
        make_helpers(helpers)?;
    }
    mount_filesystems(plan, debug)?;
    make_filesystem_mostly_read_only(plan, debug)?;
    debug("filesystem mostly read-only");
    if plan.netns != Netns::None {
        // MountProcAndSys's /sys half: sysfs belongs to the network
        // namespace of its mounter, this process.
        if let Err(error) = mount(
            Some(Path::new("none")),
            Path::new("/sys"),
            Some("sysfs"),
            libc::MS_NOEXEC | libc::MS_NOSUID | libc::MS_NODEV | libc::MS_RDONLY,
            None,
        ) {
            if !plan.proc_fallback {
                return Err(format!("mount /sys: {error}"));
            }
            debug(&format!(
                "mount /sys: {error}; binding the host /sys (proc fallback)"
            ));
        }
    }
    if plan.netns == Netns::WithLoopback {
        loopback_up()?;
    }
    // EnterWorkingDirectory: the inherited working directory still refers to
    // the mount it was entered on before the binds and the read-only
    // remount; entering it again resolves it through the new mount table.
    std::env::set_current_dir(&plan.working_dir)
        .map_err(|error| format!("chdir({}): {error}", plan.working_dir.display()))?;
    // SAFETY: as above; the next child of this process is pid 1 of the new
    // PID namespace.
    if unsafe { libc::unshare(libc::CLONE_NEWPID) } < 0 {
        return Err(format!("unshare(CLONE_NEWPID): {}", errno()));
    }
    Ok(())
}

/// The inaccessible helpers (LinuxSandboxUtil.getInaccessibleHelper{File,Dir}):
/// a private tmpfs on `helpers` (it vanishes with the mount namespace) with
/// an empty directory and an empty file, both mode 000.
fn make_helpers(helpers: &Path) -> Result<(), String> {
    use std::os::unix::fs::PermissionsExt;
    mount(
        Some(Path::new("tmpfs")),
        helpers,
        Some("tmpfs"),
        libc::MS_NOSUID | libc::MS_NODEV,
        Some("mode=0755"),
    )
    .map_err(|error| format!("mount(tmpfs, {}): {error}", helpers.display()))?;
    let dir = helpers.join(HELPER_DIR);
    let file = helpers.join(HELPER_FILE);
    fs::create_dir(&dir).map_err(|error| format!("mkdir {}: {error}", dir.display()))?;
    fs::write(&file, b"").map_err(|error| format!("create {}: {error}", file.display()))?;
    for path in [&dir, &file] {
        fs::set_permissions(path, fs::Permissions::from_mode(0o000))
            .map_err(|error| format!("chmod {}: {error}", path.display()))?;
    }
    Ok(())
}

/// MountFilesystems (linux-sandbox-pid1.cc:271-336).
fn mount_filesystems(plan: &Plan, debug: &dyn Fn(&str)) -> Result<(), String> {
    let working_dir = plan.working_dir.as_os_str().as_bytes();
    for tmpfs in &plan.tmpfs {
        if working_dir.starts_with(tmpfs.as_os_str().as_bytes()) {
            return Err(format!(
                "The sandbox working directory cannot be below a path where we mount tmpfs (you requested mounting {} in {}). Is your --output_base= below one of your --sandbox_tmpfs_path values?",
                plan.working_dir.display(),
                tmpfs.display()
            ));
        }
    }
    for (source, target) in &plan.binds {
        debug(&format!(
            "bind mount: {} -> {}",
            source.display(),
            target.display()
        ));
        mount(
            Some(source),
            target,
            None,
            libc::MS_BIND | libc::MS_REC,
            None,
        )
        .map_err(|error| {
            format!(
                "mount({}, {}, nullptr, MS_BIND | MS_REC, nullptr): {error}",
                source.display(),
                target.display()
            )
        })?;
    }
    for tmpfs in &plan.tmpfs {
        debug(&format!("tmpfs: {}", tmpfs.display()));
        mount(
            Some(Path::new("tmpfs")),
            tmpfs,
            Some("tmpfs"),
            libc::MS_NOSUID | libc::MS_NODEV | libc::MS_NOATIME,
            None,
        )
        .map_err(|error| {
            format!(
                "mount(tmpfs, {}, tmpfs, MS_NOSUID | MS_NODEV | MS_NOATIME, nullptr): {error}",
                tmpfs.display()
            )
        })?;
    }
    for writable in &plan.writable {
        debug(&format!("writable: {}", writable.display()));
        if plan.binds.iter().any(|(source, _)| source == writable) {
            continue;
        }
        mount(
            Some(writable),
            writable,
            None,
            libc::MS_BIND | libc::MS_REC,
            None,
        )
        .map_err(|error| {
            format!(
                "mount({0}, {0}, nullptr, MS_BIND | MS_REC, nullptr): {error}",
                writable.display()
            )
        })?;
    }
    debug(&format!("working dir: {}", plan.working_dir.display()));
    mount(
        Some(&plan.working_dir),
        &plan.working_dir,
        None,
        libc::MS_BIND,
        None,
    )
    .map_err(|error| {
        format!(
            "mount({0}, {0}, nullptr, MS_BIND, nullptr): {error}",
            plan.working_dir.display()
        )
    })
}

/// struct mount_attr (linux/mount.h).
#[repr(C)]
struct MountAttr {
    attr_set: u64,
    attr_clr: u64,
    propagation: u64,
    userns_fd: u64,
}

const MOUNT_ATTR_RDONLY: u64 = 0x0000_0001;
const AT_RECURSIVE: libc::c_uint = 0x8000;

/// mount_setattr(2) on the mount at `path` (Linux 5.12).
fn mount_setattr(path: &Path, recursive: bool, set: u64, clear: u64) -> io::Result<()> {
    let path = cstring(path).map_err(io::Error::other)?;
    let attr = MountAttr {
        attr_set: set,
        attr_clr: clear,
        propagation: 0,
        userns_fd: 0,
    };
    let flags = if recursive { AT_RECURSIVE } else { 0 };
    // SAFETY: `path` is NUL-terminated and `attr` is a fully initialized
    // struct mount_attr whose size is passed; both outlive the call.
    let result = unsafe {
        libc::syscall(
            libc::SYS_mount_setattr,
            libc::AT_FDCWD,
            path.as_ptr(),
            flags,
            &attr as *const MountAttr,
            std::mem::size_of::<MountAttr>(),
        )
    };
    if result < 0 { Err(errno()) } else { Ok(()) }
}

/// The remount errors MakeFilesystemMostlyReadOnly ignores (a mount point
/// without access, a hidden or broken mount).
fn ignorable_remount_error(error: &io::Error) -> bool {
    matches!(
        error.raw_os_error(),
        Some(
            libc::EACCES | libc::EPERM | libc::EINVAL | libc::ENOENT | libc::ESTALE | libc::ENODEV
        )
    )
}

/// MakeFilesystemMostlyReadOnly (linux-sandbox-pid1.cc:363-447): every mount
/// remounted read-only except the writable ones, keeping its locked flags.
///
/// The same end state in two system calls where the kernel has
/// mount_setattr(2): the whole tree read-only at once (AT_RECURSIVE), then
/// the read-only flag cleared again on each writable mount point — as the
/// per-mount remount leaves them. One `MS_REMOUNT` per mount costs about a
/// millisecond each (a hundred mounts in a container: ~100 ms per spawn);
/// kernels without mount_setattr take genuine's loop.
fn make_filesystem_mostly_read_only(plan: &Plan, debug: &dyn Fn(&str)) -> Result<(), String> {
    let should_be_writable = |dir: &Path| {
        dir == plan.working_dir
            || plan.writable.iter().any(|path| path == dir)
            || plan.tmpfs.iter().any(|path| path == dir)
    };
    let entries = mount_entries()?;
    let mut tree = Ok(());
    for attempt in 0..5 {
        tree = mount_setattr(Path::new("/"), true, MOUNT_ATTR_RDONLY, 0);
        match &tree {
            Err(error) if error.raw_os_error() == Some(libc::EBUSY) && attempt < 4 => {
                std::thread::sleep(std::time::Duration::from_millis(100));
            }
            _ => break,
        }
    }
    match tree {
        Ok(()) => {
            debug("mount_setattr: / read-only (recursive)");
            for (dir, _) in &entries {
                if !should_be_writable(dir) {
                    continue;
                }
                debug(&format!("remount rw: {}", dir.display()));
                if let Err(error) = mount_setattr(dir, false, 0, MOUNT_ATTR_RDONLY) {
                    if !ignorable_remount_error(&error) {
                        return Err(format!(
                            "mount_setattr({}, MOUNT_ATTR_RDONLY cleared): {error}",
                            dir.display()
                        ));
                    }
                    debug(&format!(
                        "mount_setattr({}) failure ({error}) ignored",
                        dir.display()
                    ));
                }
            }
            return Ok(());
        }
        Err(error) => debug(&format!(
            "mount_setattr(/, AT_RECURSIVE): {error}; remounting each mount"
        )),
    }
    for (dir, options) in entries {
        let has = |name: &str| options.iter().any(|option| option == name);
        let mut flags = libc::MS_BIND | libc::MS_REMOUNT;
        for (name, flag) in [
            ("nodev", libc::MS_NODEV),
            ("noexec", libc::MS_NOEXEC),
            ("nosuid", libc::MS_NOSUID),
            ("noatime", libc::MS_NOATIME),
            ("nodiratime", libc::MS_NODIRATIME),
            ("relatime", libc::MS_RELATIME),
        ] {
            if has(name) {
                flags |= flag;
            }
        }
        if !should_be_writable(&dir) {
            flags |= libc::MS_RDONLY;
        }
        debug(&format!(
            "remount {}: {}",
            if flags & libc::MS_RDONLY != 0 {
                "ro"
            } else {
                "rw"
            },
            dir.display()
        ));
        let mut result = Ok(());
        for attempt in 0..5 {
            result = mount(None, &dir, None, flags, None);
            match &result {
                Err(error) if error.raw_os_error() == Some(libc::EBUSY) && attempt < 4 => {
                    std::thread::sleep(std::time::Duration::from_millis(100));
                }
                _ => break,
            }
        }
        if let Err(error) = result {
            if !ignorable_remount_error(&error) {
                return Err(format!(
                    "remount(nullptr, {}, nullptr, {flags}, nullptr): {error}",
                    dir.display()
                ));
            }
            debug(&format!(
                "remount(nullptr, {}, nullptr, {flags}, nullptr) failure ({error}) ignored",
                dir.display()
            ));
        }
    }
    Ok(())
}

/// SetupNetworking: brings `lo` up in the fresh network namespace.
fn loopback_up() -> Result<(), String> {
    // SAFETY: plain socket/ioctl/close calls on a fresh descriptor; `ifr` is
    // zero-initialized plain data with a NUL-terminated name.
    unsafe {
        let fd = libc::socket(libc::AF_INET, libc::SOCK_DGRAM, 0);
        if fd < 0 {
            return Err(format!("socket: {}", errno()));
        }
        let mut ifr: libc::ifreq = std::mem::zeroed();
        for (slot, byte) in ifr.ifr_name.iter_mut().zip(b"lo\0") {
            *slot = *byte as libc::c_char;
        }
        if libc::if_nametoindex(ifr.ifr_name.as_ptr()) == 0 {
            libc::close(fd);
            return Err(format!("if_nametoindex: {}", errno()));
        }
        ifr.ifr_ifru.ifru_flags |= libc::IFF_UP as libc::c_short;
        // The request type is `c_ulong` on glibc and `c_int` on musl.
        #[allow(clippy::useless_conversion, clippy::unnecessary_cast)]
        if libc::ioctl(fd, libc::SIOCSIFFLAGS as _, &ifr) < 0 {
            let error = errno();
            libc::close(fd);
            return Err(format!("ioctl: {error}"));
        }
        libc::close(fd);
    }
    Ok(())
}

/// Makes `command` — the action — start as pid 2 of the new PID namespace
/// below a pid 1 forked from it (linux-sandbox-pid1.cc Pid1Main from the
/// /proc mount on, SpawnChild, WaitForChild): between fork and exec the
/// first child of the worker — pid 1, still holding the worker's
/// capabilities in the user namespace — leads its own session, mounts the
/// fresh /proc (MountProcAndSys), arms PR_SET_PDEATHSIG and forks again. The
/// grandchild returns to exec the action (umask 022, then the remaining
/// pre-exec hooks: its own session, every signal at its default); pid 1
/// closes every other descriptor (the start's error pipe among them, so the
/// worker learns of the action's exec), forwards SIGTERM to the action's
/// process group, reaps every process of the namespace until the action has
/// exited and `_exit`s with its code or 128 + its signal. Leaving pid 1
/// kills whatever the action left. No second exec of this binary: pid 1 is
/// the forked child itself, like genuine's cloned pid 1.
///
/// Pid 1 also writes the action's raw wait status into a pipe before it
/// exits: the returned reader yields it (None when pid 1 died first, e.g.
/// SIGKILLed), so that the supervisor reports the action's exact status — a
/// signal included — like every other spawn (docs/PROCESS_WRAPPER.md), where
/// genuine's linux-sandbox can only exit 128 + the signal.
pub fn spawn_below_pid1(command: &mut Command, proc_fallback: bool) -> io::Result<StatusPipe> {
    let mut fds = [0; 2];
    // SAFETY: pipe2 fills the two-element array.
    if unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC | libc::O_NONBLOCK) } < 0 {
        return Err(errno());
    }
    // SAFETY: both descriptors were just created and are owned here.
    let (read, write) = unsafe {
        (
            std::os::fd::OwnedFd::from_raw_fd(fds[0]),
            std::os::fd::OwnedFd::from_raw_fd(fds[1]),
        )
    };
    let status_fd = fds[1];
    let proc_ = c"proc";
    let slash_proc = c"/proc";
    // SAFETY: runs in the child between fork and exec of the single-threaded
    // worker (its supervisor threads start after the spawn): only system
    // calls on static strings and stack values (setsid, mount, prctl, fork,
    // umask, sigaction, close_range/close, write, wait, _exit), errno read right
    // after each; pid 1 never returns from the hook.
    unsafe {
        command.pre_exec(move || {
            if libc::setsid() < 0 {
                return Err(io::Error::last_os_error());
            }
            if libc::mount(
                proc_.as_ptr(),
                slash_proc.as_ptr(),
                proc_.as_ptr(),
                libc::MS_NODEV | libc::MS_NOEXEC | libc::MS_NOSUID,
                std::ptr::null(),
            ) < 0
            {
                let error = io::Error::last_os_error();
                // The test-only fallback: the host's /proc (process listings
                // are the host's; the PID namespace still ends with pid 1).
                if !proc_fallback
                    || libc::mount(
                        slash_proc.as_ptr(),
                        slash_proc.as_ptr(),
                        std::ptr::null(),
                        libc::MS_BIND | libc::MS_REC,
                        std::ptr::null(),
                    ) < 0
                {
                    return Err(error);
                }
            }
            // SetupSelfDestruction: pid 1 dies with the worker.
            libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL);
            let child = libc::fork();
            if child < 0 {
                return Err(io::Error::last_os_error());
            }
            if child == 0 {
                // The action (pid 2): output permissions made predictable.
                libc::umask(0o022);
                return Ok(());
            }
            pid1(child, status_fd)
        });
    }
    Ok(StatusPipe {
        read: fs::File::from(read),
        _write: write,
    })
}

/// The reading end of pid 1's status pipe (and the worker's copy of the
/// writing end, closed with it).
pub struct StatusPipe {
    read: fs::File,
    _write: std::os::fd::OwnedFd,
}

impl StatusPipe {
    /// The action's raw wait status, once pid 1 has exited.
    pub fn status(&mut self) -> Option<i32> {
        use std::io::Read;
        let mut bytes = [0u8; 4];
        self.read
            .read_exact(&mut bytes)
            .ok()
            .map(|()| i32::from_ne_bytes(bytes))
    }
}

/// The action's PID, for pid 1's SIGTERM forwarder.
static CHILD: AtomicI32 = AtomicI32::new(0);

extern "C" fn forward_signal(signal: libc::c_int) {
    let child = CHILD.load(Ordering::Relaxed);
    if child > 0 {
        // SAFETY: kill(2) is async-signal-safe.
        unsafe {
            libc::kill(-child, signal);
        }
    }
}

/// Pid 1's life once the action runs as `child`; never returns.
///
/// # Safety
/// Only in the forked pid 1 of [`spawn_below_pid1`]'s hook.
unsafe fn pid1(child: libc::pid_t, status_fd: libc::c_int) -> ! {
    // SAFETY: plain system calls on stack values; the handler only calls
    // kill(2) on an atomic's value.
    unsafe {
        CHILD.store(child, Ordering::Relaxed);
        let mut action: libc::sigaction = std::mem::zeroed();
        action.sa_sigaction = forward_signal as *const () as libc::sighandler_t;
        libc::sigemptyset(&mut action.sa_mask);
        libc::sigaction(libc::SIGTERM, &action, std::ptr::null_mut());
        // Every descriptor but stdin, stdout, stderr and the status pipe:
        // the start's error pipe must close with the action's exec.
        let below = if status_fd > 3 {
            libc::syscall(libc::SYS_close_range, 3u32, (status_fd - 1) as u32, 0u32)
        } else {
            0
        };
        let above = libc::syscall(
            libc::SYS_close_range,
            (status_fd + 1) as u32,
            u32::MAX,
            0u32,
        );
        if below < 0 || above < 0 {
            // Linux < 5.9: one by one.
            for fd in 3..1024 {
                if fd != status_fd {
                    libc::close(fd);
                }
            }
        }
        let report = |status: libc::c_int| {
            let bytes = status.to_ne_bytes();
            libc::write(status_fd, bytes.as_ptr().cast(), bytes.len());
        };
        loop {
            let mut status = 0;
            let reaped = libc::wait(&mut status);
            if reaped < 0 {
                if *libc::__errno_location() == libc::EINTR {
                    continue;
                }
                libc::_exit(1);
            }
            if reaped != child {
                continue;
            }
            report(status);
            if libc::WIFSIGNALED(status) {
                libc::_exit(128 + libc::WTERMSIG(status));
            }
            libc::_exit(libc::WEXITSTATUS(status));
        }
    }
}

/// Whether the namespace sandbox works on this host
/// (LinuxSandboxedSpawnRunner.isSupported/computeIsSupported: the
/// linux-sandbox runs /bin/true with no environment; any failure means
/// unsupported). `cwd` stands in for genuine's execroot: the working
/// directory is bound onto itself non-recursively, which a user namespace
/// refuses for a directory with (locked) mounts below it.
pub fn probe(cwd: &Path, proc_fallback: bool) -> bool {
    let cwd = cwd.to_path_buf();
    let mut command = Command::new("/proc/self/exe");
    command.arg(super::WORKER_ARG).arg("--namespaces");
    let plan = Plan {
        working_dir: cwd.clone(),
        proc_fallback,
        ..Plan::default()
    };
    let mut argv = Vec::new();
    plan.push_args(&mut argv);
    command
        .args(argv)
        .arg("--")
        .arg("/bin/true")
        .current_dir(&cwd)
        .env_clear()
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    crate::spawn_lock::spawn(&mut command)
        .and_then(|mut child| child.wait())
        .is_ok_and(|status| status.success())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn root(name: &str) -> PathBuf {
        let root = crate::test_temp::root().join(format!("ns-plan-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        root
    }

    fn input<'a>(execroot: &'a Path, sandbox: &'a Path, tmpdir: &'a Path) -> PlanInput<'a> {
        PlanInput {
            execroot,
            sandbox_dir: sandbox,
            tmpdir,
            test_tmpdir: None,
            writable_paths: &[],
            tmpfs_paths: &[],
            mount_pairs: &[],
            block_paths: &[],
            known_roots: &[],
            netns: Netns::None,
            fake_hostname: false,
            fake_username: false,
            fake_root: false,
            proc_fallback: false,
        }
    }

    #[test]
    fn writable_dirs_follow_get_writable_dirs() {
        let root = root("writable");
        let sandbox = root.join("sandbox");
        let execroot = sandbox.join("execroot");
        fs::create_dir_all(&execroot).unwrap();
        let tmpdir = PathBuf::from("/tmp");
        let mut input = input(&execroot, &sandbox, &tmpdir);
        input.test_tmpdir = Some("_tmp/t");
        let plan = plan(&input).unwrap();
        let shm = fs::canonicalize("/dev/shm").unwrap_or_else(|_| "/dev/shm".into());
        let mut expected = vec![
            execroot.clone(),
            execroot.join("_tmp/t"),
            PathBuf::from("/tmp"),
            shm,
        ];
        sort_paths(&mut expected);
        assert_eq!(plan.writable, expected);
        assert_eq!(plan.working_dir, execroot);
        // A TMPDIR outside the execroot must exist.
        let missing = root.join("missing");
        let mut input = super::tests::input(&execroot, &sandbox, &missing);
        input.test_tmpdir = None;
        assert_eq!(
            plan_error(&input),
            format!(
                "Cannot resolve symlinks in TMPDIR because it doesn't exist: \"{}\"",
                missing.display()
            )
        );
        fs::remove_dir_all(&root).unwrap();
    }

    fn plan_error(input: &PlanInput) -> String {
        plan(input).unwrap_err()
    }

    #[test]
    fn hermetic_tmp_mounts_the_spawn_tmp_last_with_known_roots_inside() {
        let root = root("hermetic");
        let sandbox = root.join("sandbox");
        let execroot = sandbox.join("execroot");
        fs::create_dir_all(&execroot).unwrap();
        let tmpdir = PathBuf::from("/tmp");
        let known = [
            PathBuf::from("/tmp/opencode/w/cache"),
            PathBuf::from("/home/x/ws"),
        ];
        let mut input = input(&execroot, &sandbox, &tmpdir);
        input.known_roots = &known;
        let plan = plan(&input).unwrap();
        let hermetic = sandbox.join("_hermetic_tmp");
        assert!(hermetic.is_dir());
        // /tmp/opencode (the first two segments) inside the hermetic tmp,
        // then the hermetic tmp onto /tmp as the final mount.
        assert_eq!(
            plan.binds,
            vec![
                (PathBuf::from("/tmp/opencode"), hermetic.join("opencode")),
                (hermetic.clone(), PathBuf::from("/tmp")),
            ]
        );
        assert!(hermetic.join("opencode").is_dir());
        // An explicit /tmp mount pair disables the hermetic tmp.
        let pairs = [("/tmp".to_owned(), "/tmp".to_owned())];
        let mut input = super::tests::input(&execroot, &sandbox, &tmpdir);
        input.mount_pairs = &pairs;
        input.known_roots = &known;
        let plan = super::plan(&input).unwrap();
        assert_eq!(
            plan.binds,
            vec![(PathBuf::from("/tmp"), PathBuf::from("/tmp"))]
        );
        // So does a /tmp tmpfs.
        let tmpfs = [PathBuf::from("/tmp")];
        let mut input = super::tests::input(&execroot, &sandbox, &tmpdir);
        input.tmpfs_paths = &tmpfs;
        let plan = super::plan(&input).unwrap();
        assert!(plan.binds.is_empty());
        assert_eq!(plan.tmpfs, tmpfs);
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn mount_pairs_are_validated_and_relative_sources_join_the_execroot() {
        let root = root("pairs");
        let sandbox = root.join("sandbox");
        let execroot = sandbox.join("execroot");
        fs::create_dir_all(execroot.join("data")).unwrap();
        fs::create_dir_all(root.join("target")).unwrap();
        fs::write(root.join("file"), b"x").unwrap();
        let tmpdir = PathBuf::from("/tmp");
        let target = root.join("target").display().to_string();
        let pairs = [("data".to_owned(), target.clone())];
        let mut input = input(&execroot, &sandbox, &tmpdir);
        input.mount_pairs = &pairs;
        // Under /tmp the target would move into the hermetic tmp; keep the
        // plain view with an explicit /tmp tmpfs.
        let tmpfs = [PathBuf::from("/tmp")];
        input.tmpfs_paths = &tmpfs;
        let plan = plan(&input).unwrap();
        assert_eq!(
            plan.binds,
            vec![(execroot.join("data"), PathBuf::from(&target))]
        );
        let missing = [("/nonexistent-source".to_owned(), target.clone())];
        input.mount_pairs = &missing;
        assert_eq!(
            plan_error(&input),
            "Mount source '/nonexistent-source' does not exist."
        );
        let mismatch = [(root.join("file").display().to_string(), target.clone())];
        input.mount_pairs = &mismatch;
        assert_eq!(
            plan_error(&input),
            format!(
                "Mount target '{target}' is a directory but mount source '{}' is a file, they must be the same type.",
                root.join("file").display()
            )
        );
        let no_target = [("data".to_owned(), "/nonexistent-target".to_owned())];
        input.mount_pairs = &no_target;
        assert!(plan_error(&input).starts_with(
            "Mount target '/nonexistent-target' does not exist. Bazel only supports bind mounting"
        ));
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn block_paths_bind_the_inaccessible_helpers() {
        let root = root("block");
        let sandbox = root.join("sandbox");
        let execroot = sandbox.join("execroot");
        fs::create_dir_all(&execroot).unwrap();
        fs::create_dir_all(root.join("secret-dir")).unwrap();
        fs::write(root.join("secret-file"), b"s").unwrap();
        let tmpdir = PathBuf::from("/tmp");
        let blocked = [
            root.join("secret-dir"),
            root.join("secret-file"),
            root.join("absent"),
        ];
        let tmpfs = [PathBuf::from("/tmp")];
        let mut input = input(&execroot, &sandbox, &tmpdir);
        input.block_paths = &blocked;
        input.tmpfs_paths = &tmpfs;
        let plan = plan(&input).unwrap();
        let helpers = sandbox.join("sandbox-helpers");
        assert_eq!(plan.helpers.as_deref(), Some(helpers.as_path()));
        let resolved = |path: &Path| fs::canonicalize(path).unwrap();
        assert_eq!(
            plan.binds,
            vec![
                (helpers.join(HELPER_DIR), resolved(&root.join("secret-dir"))),
                (
                    helpers.join(HELPER_FILE),
                    resolved(&root.join("secret-file"))
                ),
            ]
        );
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn plan_round_trips_through_the_worker_arguments() {
        let plan = Plan {
            working_dir: "/w".into(),
            writable: vec!["/a".into(), "/b".into()],
            tmpfs: vec!["/t".into()],
            binds: vec![("/s".into(), "/d".into())],
            netns: Netns::WithLoopback,
            fake_hostname: true,
            fake_username: true,
            fake_root: false,
            helpers: Some("/h".into()),
            proc_fallback: true,
        };
        let mut argv = Vec::new();
        plan.push_args(&mut argv);
        let mut parsed = Plan::default();
        let mut index = 0;
        while index < argv.len() {
            assert!(parsed.parse_arg(&argv, &mut index).unwrap(), "{argv:?}");
            index += 1;
        }
        assert_eq!(parsed, plan);
    }

    #[test]
    fn mount_entries_decode_octal_escapes() {
        // The parser runs on this process's own table; every entry has an
        // absolute mount point.
        let entries = mount_entries().unwrap();
        assert!(entries.iter().all(|(dir, _)| dir.is_absolute()));
        assert!(entries.iter().any(|(dir, _)| dir == Path::new("/")));
    }
}
