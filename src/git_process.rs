//! Linux-only supervision of trusted `/usr/bin/git`; no shell or Bazel fallback.
//!
//! This is NOT a sandbox. The caller owns the existing private root and cwd,
//! trusts their contents/ancestors, and prevents concurrent hostile mutation.
//! In particular, repository-local config and caller arguments are trusted:
//! callers must validate remote URLs/full commit IDs and must not override these
//! options or use arbitrary Git aliases, helpers, hooks, or external commands.
//! System Git, its HTTPS helpers, `/usr/bin/kill`, and the system CA store are
//! trusted. Children must not escape their process group. No signal handlers or
//! global state are installed: external termination of this Rust process (or an
//! abort, including OOM) does NOT guarantee cleanup. Unwinding does run cleanup.
//!
//! Deadlines bound ordinary child execution, not kernel uninterruptible sleep,
//! scheduling, filesystem operations, or a stuck kernel wait after SIGKILL.
//! A failed cleanup is never reported as success. Output is captured whole
//! (like `repository_ctx.execute`, whose result Bazel buffers without a cap);
//! error diagnostics show bounded tails.

#![deny(unsafe_code)]

#[cfg(not(target_os = "linux"))]
compile_error!("git_process supports Linux only (process groups and kill(1) required)");

use std::ffi::OsString;
use std::fs;
use std::io::{self, Read};
use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
use std::os::unix::process::CommandExt;
use std::path::{Component, Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

const DIAGNOSTIC_TAIL: usize = 64 * 1024;
const POLL: Duration = Duration::from_millis(5);
const CLEANUP_TIMEOUT: Duration = Duration::from_secs(1);

/// Only a zero exit status, within limits, with successful cleanup returns this.
#[derive(Debug)]
pub struct Output {
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
}

/// Owns fresh isolation directories until dropped; no executable override API.
pub struct GitRunner {
    isolation: PrivateDirectory,
    git: PathBuf,
    kill: PathBuf,
    allow_file: bool,
    #[cfg(test)]
    test_script: Option<PathBuf>,
}

struct PrivateDirectory(PathBuf);

impl Drop for PrivateDirectory {
    fn drop(&mut self) {
        // Only the subtree exclusively created by this instance is removed.
        if let Err(error) = fs::remove_dir_all(&self.0) {
            eprintln!("git isolation cleanup '{}': {error}", self.0.display());
        }
    }
}

impl GitRunner {
    /// Create `git-process` under an existing caller-owned, trusted private root.
    /// The root must be absolute, without `..` or symlink ancestors. An existing
    /// `git-process` is rejected, never reused or deleted. Use a separate root
    /// for each runner. `allow_file` is for explicitly authorized local sources.
    pub fn new(private_root: &Path, allow_file: bool) -> Result<Self, String> {
        let root = private_root;
        let git = "/usr/bin/git";
        let kill = "/usr/bin/kill";
        executable(Path::new(git))?;
        executable(Path::new(kill))?;
        private_directory(root)?;
        let path = root.join("git-process");
        fs::DirBuilder::new()
            .mode(0o700)
            .create(&path)
            .map_err(|e| format!("cannot create Git isolation '{}': {e}", path.display()))?;
        let isolation = PrivateDirectory(path);
        for name in ["home", "config", "tmp", "hooks", "templates"] {
            fs::DirBuilder::new()
                .mode(0o700)
                .create(isolation.0.join(name))
                .map_err(|e| format!("cannot create private Git {name}: {e}"))?;
        }
        Ok(Self {
            isolation,
            git: git.into(),
            kill: kill.into(),
            allow_file,
            #[cfg(test)]
            test_script: None,
        })
    }

    fn command(&self, cwd: &Path, args: &[&str]) -> Command {
        let mut command = Command::new(&self.git);
        #[cfg(test)]
        if let Some(script) = &self.test_script {
            command.arg(script);
        }
        command
            .current_dir(cwd)
            .env_clear()
            .env("HOME", self.isolation.0.join("home"))
            .env("XDG_CONFIG_HOME", self.isolation.0.join("config"))
            .env("TMPDIR", self.isolation.0.join("tmp"))
            .envs([
                ("PATH", "/usr/bin:/bin"),
                ("LC_ALL", "C"),
                ("LANG", "C"),
                ("TZ", "UTC"),
                ("GIT_CONFIG_NOSYSTEM", "1"),
                ("GIT_CONFIG_SYSTEM", "/dev/null"),
                ("GIT_CONFIG_GLOBAL", "/dev/null"),
                ("GIT_TERMINAL_PROMPT", "0"),
                ("GIT_PROTOCOL_FROM_USER", "0"),
                ("GIT_ATTR_NOSYSTEM", "1"),
            ])
            // Bazel runs git with the repository environment, so its proxy
            // variables reach Git (crate::download_proxy::git_environment).
            .envs(crate::download_proxy::git_environment().iter().cloned())
            .env(
                "GIT_ALLOW_PROTOCOL",
                if self.allow_file {
                    "https:file"
                } else {
                    "https"
                },
            )
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .process_group(0);
        for (key, directory) in [
            ("core.hooksPath=", "hooks"),
            ("init.templateDir=", "templates"),
        ] {
            let mut value = OsString::from(key);
            value.push(self.isolation.0.join(directory));
            command.arg("-c").arg(value);
        }
        for value in [
            "core.fsmonitor=false",
            "credential.helper=",
            "credential.interactive=false",
            "http.sslVerify=true",
            "http.followRedirects=false",
            "http.emptyAuth=false",
            "http.proactiveAuth=none",
            "core.pager=cat",
        ] {
            command.arg("-c").arg(value);
        }
        command.args(args);
        command
    }

    /// Run trusted literal Git arguments; no inherited environment or stdin.
    /// Always SIGKILL the group, even after successful leader exit, before
    /// joining drain threads. Returned errors include bounded stdout/stderr tails.
    pub fn run(&self, cwd: &Path, args: &[&str], timeout: Duration) -> Result<Output, String> {
        if timeout.is_zero() {
            return Err("Git deadline is zero; no process spawned".into());
        }
        let deadline = Instant::now()
            .checked_add(timeout)
            .ok_or("Git timeout is too large")?;
        executable(&self.git)?;
        executable(&self.kill)?;
        private_directory(cwd)?;
        if Instant::now() >= deadline {
            return Err("Git deadline expired before spawn".into());
        }
        let child = self
            .command(cwd, args)
            .spawn()
            .map_err(|e| format!("cannot spawn trusted Git '{}': {e}", self.git.display()))?;
        let mut guard = GroupGuard {
            group: child.id(),
            child,
            kill: self.kill.clone(),
            armed: true,
        };
        let stdout = Capture::start(guard.child.stdout.take().ok_or("Git stdout pipe missing")?)?;
        let stderr = Capture::start(guard.child.stderr.take().ok_or("Git stderr pipe missing")?)?;
        let mut errors = Vec::new();
        loop {
            if Instant::now() >= deadline {
                errors.push(format!("Git deadline exceeded ({timeout:?})"));
                break;
            }
            match guard.child.try_wait() {
                Ok(Some(status)) => {
                    if !status.success() {
                        errors.push(format!("Git failed ({status})"));
                    }
                    break;
                }
                Ok(None) => {
                    thread::sleep(POLL.min(deadline.saturating_duration_since(Instant::now())))
                }
                Err(e) => {
                    errors.push(format!("cannot wait for Git: {e}"));
                    break;
                }
            }
        }
        if let Err(e) = guard.cleanup() {
            errors.push(e);
        }
        // Never block indefinitely joining a pipe whose holder escaped the group
        // or survived a cleanup error. Such a reader is detached; this
        // exceptional case is an error, not a sandbox guarantee.
        let drain_deadline = Instant::now() + CLEANUP_TIMEOUT;
        let stdout = stdout.finish(drain_deadline, "stdout", &mut errors);
        let stderr = stderr.finish(drain_deadline, "stderr", &mut errors);
        if errors.is_empty() {
            Ok(Output { stdout, stderr })
        } else {
            Err(format!(
                "{}\nstdout tail:\n{}\nstderr tail:\n{}",
                errors.join("; "),
                diagnostic(&stdout),
                diagnostic(&stderr)
            ))
        }
    }
}

fn executable(path: &Path) -> Result<(), String> {
    let metadata = fs::metadata(path)
        .map_err(|e| format!("trusted executable '{}' unavailable: {e}", path.display()))?;
    if !path.is_absolute() || !metadata.is_file() || metadata.permissions().mode() & 0o111 == 0 {
        return Err(format!(
            "trusted executable '{}' is not an executable regular file",
            path.display()
        ));
    }
    Ok(())
}

fn private_directory(path: &Path) -> Result<(), String> {
    if !path.is_absolute() || path.components().any(|c| matches!(c, Component::ParentDir)) {
        return Err(format!(
            "Git private directory must be absolute without '..': '{}'",
            path.display()
        ));
    }
    let mut prefix = PathBuf::new();
    for component in path.components() {
        prefix.push(component.as_os_str());
        let metadata = fs::symlink_metadata(&prefix)
            .map_err(|e| format!("cannot inspect Git directory '{}': {e}", prefix.display()))?;
        if metadata.file_type().is_symlink() || !metadata.is_dir() {
            return Err(format!(
                "Git directory is not a real directory (symlinks forbidden): '{}'",
                prefix.display()
            ));
        }
    }
    Ok(())
}

fn helper_command(kill: &Path, group: u32) -> Command {
    let mut command = Command::new(kill);
    command
        .args(["-KILL", "--", &format!("-{group}")])
        .env_clear()
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    command
}

pub(crate) fn kill_group(kill: &Path, group: u32) -> Result<(), String> {
    // Child::id is a positive PID; never signal the caller's group or all PIDs.
    if group <= 1 {
        return Err(format!("invalid Git process group {group}"));
    }
    let mut helper = helper_command(kill, group)
        .spawn()
        .map_err(|e| format!("cannot spawn group-kill helper '{}': {e}", kill.display()))?;
    let deadline = Instant::now() + CLEANUP_TIMEOUT;
    let status = loop {
        match helper.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if Instant::now() < deadline => thread::sleep(POLL),
            result => {
                let reason = match result {
                    Err(e) => format!("cannot wait for group-kill helper: {e}"),
                    _ => "group-kill helper deadline exceeded".into(),
                };
                let killed = helper.kill();
                let waited = helper.wait();
                return Err(format!("{reason}; helper kill={killed:?}, wait={waited:?}"));
            }
        }
    };
    if status.success() {
        return Ok(());
    }
    // kill(1)'s status does not distinguish ESRCH from EPERM. Positively verify
    // group absence with a kernel probe; never blindly accept exit code 1. Any
    // probe failure other than ESRCH is a cleanup error (fail closed).
    match group_exists(kill, group) {
        Ok(false) => Ok(()),
        Ok(true) => Err(format!(
            "group-kill helper failed ({status}) for Git group {group}; group still exists"
        )),
        Err(e) => Err(format!(
            "group-kill helper failed ({status}) for Git group {group}; {e}"
        )),
    }
}

/// Ask the kernel whether `group` has any member (including unreaped zombies)
/// via kill(2) with signal 0 on `-group`, run through trusted kill(1) with no
/// shell, an empty environment (C locale) and null stdin/stdout. Unlike a scan
/// of every host `/proc/*/stat`, foreign processes cannot make this fail.
fn group_exists(kill: &Path, group: u32) -> Result<bool, String> {
    if group <= 1 {
        return Err(format!(
            "refusing to probe non-private process group {group}"
        ));
    }
    let mut probe = Command::new(kill);
    probe
        .args(["-0", "--", &format!("-{group}")])
        .env_clear()
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped());
    let mut probe = probe
        .spawn()
        .map_err(|e| format!("cannot spawn group probe '{}': {e}", kill.display()))?;
    let stderr = match probe.stderr.take() {
        Some(pipe) => Capture::start(pipe),
        None => Err("group probe stderr pipe missing".into()),
    };
    let stderr = match stderr {
        Ok(stderr) => stderr,
        Err(e) => {
            let killed = probe.kill();
            let waited = probe.wait();
            return Err(format!("{e}; probe kill={killed:?}, wait={waited:?}"));
        }
    };
    let deadline = Instant::now() + CLEANUP_TIMEOUT;
    let status = loop {
        match probe.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if Instant::now() < deadline => thread::sleep(POLL),
            result => {
                let reason = match result {
                    Err(e) => format!("cannot wait for group probe: {e}"),
                    _ => "group probe deadline exceeded".into(),
                };
                let killed = probe.kill();
                let waited = probe.wait();
                let mut errors = Vec::new();
                stderr.finish(
                    Instant::now() + CLEANUP_TIMEOUT,
                    "group-probe stderr",
                    &mut errors,
                );
                return Err(format!(
                    "{reason}; probe kill={killed:?}, wait={waited:?}; {}",
                    errors.join("; ")
                ));
            }
        }
    };
    let mut errors = Vec::new();
    let text = stderr.finish(
        Instant::now() + CLEANUP_TIMEOUT,
        "group-probe stderr",
        &mut errors,
    );
    if !errors.is_empty() {
        return Err(format!(
            "group probe for {group} failed: {}",
            errors.join("; ")
        ));
    }
    classify_probe(group, status, &text)
}

/// Exit 0 => the group exists. Neither procps-ng kill (exit 1 on any failure)
/// nor util-linux kill (exit 1, or 64 for partial success) encodes errno in
/// its status, so absence is recognised only from the C-locale (empty
/// environment) strerror(ESRCH) report for exactly this one argument:
/// procps-ng `<argv0>: (-<group>): No such process`, util-linux
/// `<argv0>: sending signal to -<group> failed: No such process`. Anything
/// else (EPERM, other statuses, signals, extra or unexpected output) is an
/// error, never "absent".
fn classify_probe(group: u32, status: ExitStatus, stderr: &[u8]) -> Result<bool, String> {
    if status.success() {
        return Ok(true);
    }
    if status.code() == Some(1) && reports_esrch(group, stderr) {
        return Ok(false);
    }
    let start = stderr.len().saturating_sub(512);
    Err(format!(
        "group probe for {group} failed ({status}): {:?}",
        String::from_utf8_lossy(&stderr[start..])
    ))
}

fn reports_esrch(group: u32, stderr: &[u8]) -> bool {
    let Some(line) = std::str::from_utf8(stderr)
        .ok()
        .and_then(|text| text.strip_suffix('\n'))
    else {
        return false;
    };
    !line.contains('\n')
        && (line.ends_with(&format!(": (-{group}): No such process"))
            || line.ends_with(&format!(
                ": sending signal to -{group} failed: No such process"
            )))
}

struct GroupGuard {
    child: Child,
    group: u32,
    kill: PathBuf,
    armed: bool,
}

impl GroupGuard {
    fn cleanup(&mut self) -> Result<ExitStatus, String> {
        let killed = kill_group(&self.kill, self.group);
        // Even a broken helper must not prevent best-effort leader cleanup.
        let fallback = if killed.is_err() {
            self.child.kill().err()
        } else {
            None
        };
        let waited = self.child.wait();
        if killed.is_ok() && waited.is_ok() {
            self.armed = false;
        }
        match (killed, waited) {
            (Ok(()), Ok(status)) => Ok(status),
            (killed, waited) => Err(format!(
                "Git group cleanup failed: kill={killed:?}; leader kill={fallback:?}; wait={waited:?}"
            )),
        }
    }
}

impl Drop for GroupGuard {
    fn drop(&mut self) {
        if self.armed
            && let Err(error) = self.cleanup()
        {
            eprintln!("{error}");
        }
    }
}

#[derive(Default)]
struct Captured {
    bytes: Vec<u8>,
    error: Option<String>,
}

struct Capture {
    data: Arc<Mutex<Captured>>,
    thread: JoinHandle<()>,
}

impl Capture {
    fn start(mut pipe: impl Read + Send + 'static) -> Result<Self, String> {
        let data = Arc::new(Mutex::new(Captured::default()));
        let shared = data.clone();
        let thread = thread::Builder::new()
            .name("git-pipe-drain".into())
            .spawn(move || {
                let mut buffer = [0u8; 8192];
                loop {
                    match pipe.read(&mut buffer) {
                        Ok(0) => break,
                        Ok(n) => {
                            let mut data = shared.lock().unwrap_or_else(|e| e.into_inner());
                            data.bytes.extend_from_slice(&buffer[..n]);
                        }
                        Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                        Err(e) => {
                            shared.lock().unwrap_or_else(|e| e.into_inner()).error =
                                Some(e.to_string());
                            break;
                        }
                    }
                }
            })
            .map_err(|e| format!("cannot spawn Git pipe reader: {e}"))?;
        Ok(Self { data, thread })
    }

    fn finish(self, deadline: Instant, stream: &str, errors: &mut Vec<String>) -> Vec<u8> {
        while !self.thread.is_finished() && Instant::now() < deadline {
            thread::sleep(POLL);
        }
        if self.thread.is_finished() {
            if self.thread.join().is_err() {
                errors.push(format!("Git {stream} reader panicked"));
            }
        } else {
            errors.push(format!(
                "Git {stream} pipe did not close after group cleanup; reader detached"
            ));
        }
        let mut data = self.data.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(error) = data.error.take() {
            errors.push(format!("cannot read Git {stream}: {error}"));
        }
        std::mem::take(&mut data.bytes)
    }
}

fn diagnostic(bytes: &[u8]) -> String {
    let start = bytes.len().saturating_sub(DIAGNOSTIC_TAIL);
    format!(
        "{}{}",
        if start > 0 { "[truncated]\n" } else { "" },
        String::from_utf8_lossy(&bytes[start..])
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::os::unix::process::ExitStatusExt;

    fn sleeper() -> Child {
        Command::new("/bin/sleep")
            .arg("30")
            .env_clear()
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .process_group(0)
            .spawn()
            .unwrap()
    }

    #[test]
    fn kernel_probe_sees_live_and_zombie_group_then_absence_after_reap() {
        let kill = Path::new("/usr/bin/kill");
        let mut child = sleeper();
        let group = child.id();
        assert_eq!(group_exists(kill, group), Ok(true));
        child.kill().unwrap();
        // An unreaped (zombie) leader is still a group member.
        assert_eq!(group_exists(kill, group), Ok(true));
        child.wait().unwrap();
        assert_eq!(group_exists(kill, group), Ok(false));
    }

    #[test]
    fn kernel_probe_refuses_shared_groups_and_fails_closed() {
        for group in [0, 1] {
            // Refused before any spawn: even a missing helper is not reached.
            let error = group_exists(Path::new("/nonexistent/kill"), group).unwrap_err();
            assert!(error.contains("refusing"), "{error}");
        }
        let mut child = sleeper();
        let group = child.id();
        let error = group_exists(Path::new("/nonexistent/kill"), group).unwrap_err();
        assert!(error.contains("cannot spawn group probe"), "{error}");
        // Exit 1 without the ESRCH report (e.g. a broken helper) is not absence.
        let error = group_exists(Path::new("/usr/bin/false"), group).unwrap_err();
        assert!(error.contains("exit status: 1"), "{error}");
        child.kill().unwrap();
        child.wait().unwrap();
    }

    #[test]
    fn probe_classification_accepts_only_exact_esrch_as_absent() {
        let exited = |code: i32| ExitStatus::from_raw(code << 8);
        assert_eq!(classify_probe(42, exited(0), b""), Ok(true));
        for stderr in [
            &b"/usr/bin/kill: (-42): No such process\n"[..],
            b"kill: sending signal to -42 failed: No such process\n",
        ] {
            assert_eq!(classify_probe(42, exited(1), stderr), Ok(false));
        }
        for (status, stderr) in [
            (
                exited(1),
                &b"/usr/bin/kill: (-42): Operation not permitted\n"[..],
            ),
            (
                exited(1),
                b"kill: sending signal to -42 failed: Operation not permitted\n",
            ),
            (exited(1), b""),
            (exited(1), b"/usr/bin/kill: (-42): No such process"),
            (exited(1), b"/usr/bin/kill: (-142): No such process\n"),
            (exited(1), b"/usr/bin/kill: (-421): No such process\n"),
            (exited(1), b"x\n/usr/bin/kill: (-42): No such process\n"),
            (
                exited(64),
                b"kill: sending signal to -42 failed: No such process\n",
            ),
            (exited(2), b"/usr/bin/kill: (-42): No such process\n"),
            (
                ExitStatus::from_raw(9),
                b"/usr/bin/kill: (-42): No such process\n",
            ),
            (exited(1), b"/usr/bin/kill: (-42): No such process\xff\n"),
        ] {
            assert!(
                classify_probe(42, status, stderr).is_err(),
                "{status} {stderr:?}"
            );
        }
    }

    use std::os::unix::fs::symlink;
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT: AtomicU64 = AtomicU64::new(0);
    const TEST_TIMEOUT: Duration = Duration::from_secs(5);

    struct Temp(PathBuf);
    impl Temp {
        fn new() -> Self {
            loop {
                let root = std::env::temp_dir().join(format!(
                    "git-process-test-{}-{}",
                    std::process::id(),
                    NEXT.fetch_add(1, Ordering::Relaxed)
                ));
                match fs::DirBuilder::new().mode(0o700).create(&root) {
                    Ok(()) => return Self(root),
                    Err(e) if e.kind() == io::ErrorKind::AlreadyExists => continue,
                    Err(e) => panic!("{e}"),
                }
            }
        }
        // Private test-only executable injection; never exposed by the API.
        fn fake(&self, script: &str) -> GitRunner {
            let script_path = self.0.join("fake-git");
            fs::write(&script_path, format!("#!/bin/sh\n{script}\n")).unwrap();
            // Execute the stable interpreter, not a just-written executable
            // inode that another concurrent fork can transiently hold open.
            fs::set_permissions(&script_path, fs::Permissions::from_mode(0o600)).unwrap();
            let mut runner = GitRunner::new(&self.0, false).unwrap();
            runner.git = "/bin/sh".into();
            runner.test_script = Some(script_path);
            runner
        }
    }
    impl Drop for Temp {
        fn drop(&mut self) {
            fs::remove_dir_all(&self.0).unwrap();
        }
    }

    fn not_live(pid: &str) -> bool {
        match fs::read_to_string(format!("/proc/{pid}/stat")) {
            Ok(stat) => matches!(
                stat.rsplit_once(')').unwrap().1.split_whitespace().next(),
                Some("Z" | "X")
            ),
            // procfs returns ESRCH (not ENOENT) when the task exits mid-read.
            Err(e) if e.kind() == io::ErrorKind::NotFound || e.raw_os_error() == Some(3) => true,
            Err(e) => panic!("{e}"),
        }
    }
    fn assert_descendants_dead(root: &Path) {
        let pids = fs::read_to_string(root.join("pids")).unwrap();
        let deadline = Instant::now() + Duration::from_secs(1);
        while !pids.split_whitespace().all(not_live) && Instant::now() < deadline {
            thread::sleep(POLL);
        }
        assert!(
            pids.split_whitespace().all(not_live),
            "live fixture processes: {pids}"
        );
    }

    #[test]
    fn real_git_version_init_and_configuration() {
        let temp = Temp::new();
        let runner = GitRunner::new(&temp.0, false).unwrap();
        let output = runner.run(&temp.0, &["--version"], TEST_TIMEOUT).unwrap();
        assert!(output.stdout.starts_with(b"git version "));
        // Even private HOME/XDG files are ignored, not merely parent HOME.
        fs::write(
            runner.isolation.0.join("home/.gitconfig"),
            "[leak]\nvalue = home-secret\n",
        )
        .unwrap();
        fs::create_dir(runner.isolation.0.join("config/git")).unwrap();
        fs::write(
            runner.isolation.0.join("config/git/config"),
            "[leak]\nvalue = xdg-secret\n",
        )
        .unwrap();
        let output = runner
            .run(
                &temp.0,
                &["config", "--show-origin", "--list"],
                TEST_TIMEOUT,
            )
            .unwrap();
        let config = String::from_utf8(output.stdout).unwrap();
        assert!(!config.contains("secret"), "{config}");
        for expected in [
            "core.fsmonitor=false",
            "credential.helper=",
            "http.sslverify=true",
            "http.followredirects=false",
            "http.proactiveauth=none",
        ] {
            assert!(config.contains(expected), "{config}");
        }
        for repo in ["one", "two"] {
            runner
                .run(
                    &temp.0,
                    &["init", "--quiet", "--initial-branch=main", repo],
                    TEST_TIMEOUT,
                )
                .unwrap();
            assert!(!temp.0.join(repo).join(".git/hooks").exists());
            runner
                .run(
                    &temp.0.join(repo),
                    &["config", "test.local", "written"],
                    TEST_TIMEOUT,
                )
                .unwrap();
            assert_eq!(
                runner
                    .run(&temp.0.join(repo), &["config", "test.local"], TEST_TIMEOUT)
                    .unwrap()
                    .stdout,
                b"written\n"
            );
        }
        assert_eq!(
            fs::read(temp.0.join("one/.git/config")).unwrap(),
            fs::read(temp.0.join("two/.git/config")).unwrap()
        );
    }

    #[test]
    fn closed_stdin_and_exact_environment_allowlist() {
        let temp = Temp::new();
        let runner = temp.fake("if read line; then exit 9; fi\nexec /usr/bin/env -0");
        let output = runner.run(&temp.0, &[], TEST_TIMEOUT).unwrap();
        let actual: std::collections::BTreeMap<_, _> = output
            .stdout
            .split(|b| *b == 0)
            .filter(|s| !s.is_empty())
            .map(|s| {
                let s = String::from_utf8(s.to_vec()).unwrap();
                let (k, v) = s.split_once('=').unwrap();
                (k.to_string(), v.to_string())
            })
            .collect();
        let mut expected: std::collections::BTreeMap<_, _> = runner
            .command(&temp.0, &[])
            .get_envs()
            .map(|(k, v)| {
                (
                    k.to_str().unwrap().to_string(),
                    v.unwrap().to_str().unwrap().to_string(),
                )
            })
            .collect();
        // /bin/sh itself inserts PWD; Git's direct spawn environment has no PWD.
        expected.insert("PWD".into(), temp.0.to_str().unwrap().into());
        assert_eq!(actual, expected);
        assert_eq!(actual["GIT_ALLOW_PROTOCOL"], "https");
        assert_eq!(actual["GIT_TERMINAL_PROMPT"], "0");
    }

    #[test]
    fn file_protocol_requires_explicit_opt_in() {
        for allow in [false, true] {
            let temp = Temp::new();
            let runner = GitRunner::new(&temp.0, allow).unwrap();
            runner
                .run(
                    &temp.0,
                    &["init", "--quiet", "--bare", "source"],
                    TEST_TIMEOUT,
                )
                .unwrap();
            let url = format!("file://{}", temp.0.join("source").display());
            let result = runner.run(&temp.0, &["ls-remote", &url], TEST_TIMEOUT);
            if allow {
                result.unwrap();
            } else {
                assert!(result.unwrap_err().contains("transport 'file' not allowed"));
            }
        }
    }

    #[test]
    fn successful_leader_exit_kills_pipe_holding_descendant() {
        let temp = Temp::new();
        let runner =
            temp.fake("/bin/sleep 30 &\nprintf '%s\\n' \"$!\" > pids\nprintf done\nexit 0");
        let start = Instant::now();
        assert_eq!(
            runner.run(&temp.0, &[], TEST_TIMEOUT).unwrap().stdout,
            b"done"
        );
        assert!(start.elapsed() < TEST_TIMEOUT);
        assert_descendants_dead(&temp.0);
    }

    #[test]
    fn deadline_kills_leader_and_descendant() {
        let temp = Temp::new();
        let runner = temp.fake("/bin/sleep 30 &\nprintf '%s %s\\n' \"$$\" \"$!\" > pids\nwait");
        let start = Instant::now();
        let error = runner
            .run(&temp.0, &[], Duration::from_millis(150))
            .unwrap_err();
        assert!(error.contains("deadline exceeded"), "{error}");
        assert!(start.elapsed() < TEST_TIMEOUT);
        assert_descendants_dead(&temp.0);
    }

    #[test]
    fn large_output_on_either_or_both_streams_is_captured_whole() {
        // Above the former 1 MiB per-stream native capture limit.
        for (redirect, out, err) in [
            ("", 4194304, 0),
            (">&2", 0, 4194304),
            ("| /usr/bin/tee /dev/stderr", 4194304, 4194304),
        ] {
            let temp = Temp::new();
            let runner = temp.fake(&format!("/usr/bin/head -c 4194304 /dev/zero {redirect}"));
            let start = Instant::now();
            let output = runner.run(&temp.0, &[], TEST_TIMEOUT).unwrap();
            assert_eq!((output.stdout.len(), output.stderr.len()), (out, err));
            assert!(start.elapsed() < TEST_TIMEOUT);
        }
    }

    #[test]
    fn capture_keeps_every_byte() {
        let length = 3 * 1024 * 1024;
        let mut bytes = vec![b'x'; length];
        bytes[length - 4..].copy_from_slice(b"tail");
        let capture = Capture::start(io::Cursor::new(bytes)).unwrap();
        let mut errors = Vec::new();
        let output = capture.finish(Instant::now() + TEST_TIMEOUT, "test", &mut errors);
        assert!(errors.is_empty(), "{errors:?}");
        assert_eq!(output.len(), length);
        assert!(output.ends_with(b"tail"));
    }

    #[test]
    fn nonzero_exit_has_bounded_tail_diagnostics() {
        let temp = Temp::new();
        let runner = temp.fake("/usr/bin/head -c 100000 /dev/zero\nprintf stdout-end\n/usr/bin/head -c 100000 /dev/zero >&2\nprintf stderr-end >&2\nexit 7");
        let error = runner.run(&temp.0, &[], TEST_TIMEOUT).unwrap_err();
        assert!(
            error.contains("exit status: 7")
                && error.contains("stdout-end")
                && error.contains("stderr-end")
        );
        assert_eq!(error.matches("[truncated]").count(), 2);
        assert!(error.len() < 2 * DIAGNOSTIC_TAIL + 2048);
    }

    #[test]
    fn missing_helpers_and_zero_deadline_never_spawn_target() {
        let temp = Temp::new();
        let mut runner = temp.fake("touch spawned");
        assert!(
            runner
                .run(&temp.0, &[], Duration::ZERO)
                .unwrap_err()
                .contains("zero")
        );
        runner.kill = temp.0.join("missing-kill");
        assert!(
            runner
                .run(&temp.0, &[], TEST_TIMEOUT)
                .unwrap_err()
                .contains("missing-kill")
        );
        runner.kill = "/usr/bin/kill".into();
        runner.git = temp.0.join("missing-git");
        assert!(
            runner
                .run(&temp.0, &[], TEST_TIMEOUT)
                .unwrap_err()
                .contains("missing-git")
        );
        assert!(!temp.0.join("spawned").exists());
    }

    #[test]
    fn invalid_executable_spawn_fails_cleanly() {
        let temp = Temp::new();
        let mut runner = GitRunner::new(&temp.0, false).unwrap();
        runner.git = temp.0.join("bad-executable");
        fs::write(&runner.git, "#!/missing/interpreter\n").unwrap();
        fs::set_permissions(&runner.git, fs::Permissions::from_mode(0o700)).unwrap();
        assert!(
            runner
                .run(&temp.0, &[], TEST_TIMEOUT)
                .unwrap_err()
                .contains("cannot spawn trusted Git")
        );
    }

    #[test]
    fn directories_reject_symlink_ancestors_and_existing_isolation() {
        let temp = Temp::new();
        fs::create_dir(temp.0.join("real")).unwrap();
        symlink(temp.0.join("real"), temp.0.join("link")).unwrap();
        assert!(
            GitRunner::new(&temp.0.join("link"), false)
                .err()
                .unwrap()
                .contains("symlinks forbidden")
        );
        fs::create_dir(temp.0.join("real/child")).unwrap();
        assert!(GitRunner::new(&temp.0.join("link/child"), false).is_err());
        let runner = GitRunner::new(&temp.0, false).unwrap();
        assert!(GitRunner::new(&temp.0, false).is_err());
        assert!(
            runner
                .run(&temp.0.join("link"), &["--version"], TEST_TIMEOUT)
                .is_err()
        );
        drop(runner);
        assert!(!temp.0.join("git-process").exists());
        assert!(temp.0.join("real").exists());
    }

    #[test]
    fn guard_unwind_kills_and_waits() {
        let temp = Temp::new();
        let result = std::panic::catch_unwind(|| {
            let child = Command::new("/bin/sleep")
                .arg("30")
                .env_clear()
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .process_group(0)
                .spawn()
                .unwrap();
            fs::write(temp.0.join("pids"), child.id().to_string()).unwrap();
            let _guard = GroupGuard {
                group: child.id(),
                child,
                kill: "/usr/bin/kill".into(),
                armed: true,
            };
            panic!("exercise cleanup on unwind");
        });
        assert!(result.is_err());
        assert_descendants_dead(&temp.0);
    }

    #[test]
    fn kill_failure_is_not_success() {
        let temp = Temp::new();
        let child = Command::new("/bin/sleep")
            .arg("30")
            .env_clear()
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .process_group(0)
            .spawn()
            .unwrap();
        let mut guard = GroupGuard {
            group: child.id(),
            child,
            kill: "/usr/bin/false".into(),
            armed: true,
        };
        // The same broken helper also serves as the probe: its bare exit 1
        // must be a cleanup error, never "absent" (fail closed).
        let error = guard.cleanup().unwrap_err();
        assert!(
            error.contains("group-kill helper failed") && error.contains("group probe"),
            "{error}"
        );
        assert!(not_live(&guard.group.to_string()));
        guard.kill = "/usr/bin/kill".into();
        drop(guard);
        drop(temp);
    }
}
