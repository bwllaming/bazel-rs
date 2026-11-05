//! Supervised local process for `repository_ctx.execute` (Bazel 9.3
//! StarlarkBaseExternalContext.java:1970-2108, StarlarkExecutionResult.java:
//! 192-252, shell/TerminationStatus.java:134-160, and Bazel's process-wrapper:
//! src/main/tools/process-wrapper-legacy.cc:63-82,125-169 and
//! process-wrapper-options.cc:34-55,131-141, through which Bazel runs every
//! local repository command (RepositoryFetchFunction.java:99-124,644-645)).
//!
//! Security posture: repository rules are trusted code in Bazel, and so is this
//! process. It is NOT a sandbox: the program runs with the caller's credentials,
//! the client environment plus explicit overrides, and full filesystem/network
//! access, exactly like Bazel's local execution. The discipline here is only
//! about supervision: argv is passed without a shell (Bazel never inserts one;
//! a shell runs only when argv[0] names one), stdin is empty, and the command
//! runs under the native process-wrapper (crate::process_wrapper): a child
//! subreaper that starts it in its own process group, SIGKILLs the command
//! and its group at the deadline, and — after a normal exit too — kills and
//! reaps every process the command leaves, also those that left its group
//! (`setsid` daemons, double forks), exactly like Bazel's process-wrapper
//! (process-wrapper-legacy.cc:53-62,138-156, TerminateAndWaitForAll in
//! process-tools-linux.cc:24-55); if the native process itself dies (e.g.
//! Ctrl-C) the process-wrapper kills the command too. Captured output is
//! buffered without a size cap like Bazel's (StarlarkExecutionResult.java:
//! 201-203): the result is a Starlark string, so the memory is inherent.
//! Like the process-wrapper's, the supervised child starts its own session
//! (`setsid()`, crate::signals::new_session_in_child); unlike it, the
//! umask is not forced to 022. The library's in-process
//! users (unit tests) start the command directly with the former group
//! discipline: its group is SIGKILLed after the leader exits or at the
//! deadline, and a process that left the group is not reached.

#![deny(unsafe_code)]

use std::ffi::{OsStr, OsString};
use std::io::{self, Read, Write};
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use crate::process_wrapper::{Launch, Spawned};

const POLL: Duration = Duration::from_millis(5);

/// The `n`th wait of a poll loop: 50 µs doubling up to [`POLL`]. Most
/// `execute()` calls are short (`uname -m`, `mkdir -p`: aspect_rules_js
/// runs three per npm package, ~10,000 on a cold ~/code build); a fixed
/// 5 ms first wait made each take ~9 ms.
fn backoff(n: &mut u32) -> Duration {
    let wait = Duration::from_micros(50 << (*n).min(7)).min(POLL);
    *n += 1;
    wait
}
const DRAIN_TIMEOUT: Duration = Duration::from_secs(2);
const ENOENT: i32 = 2;
const EACCES: i32 = 13;
const ENOEXEC: i32 = 8;
const ENOTDIR: i32 = 20;
const ENODEV: i32 = 19;
const ESTALE: i32 = 116;
const ETIMEDOUT: i32 = 110;
/// glibc's `execvp` search path when PATH is unset (confstr _CS_PATH).
const DEFAULT_PATH: &[u8] = b"/bin:/usr/bin";

pub(crate) struct Request<'a> {
    /// argv exactly as Bazel's args list (Latin-1 bytes of each element).
    pub argv: Vec<Vec<u8>>,
    /// Complete child environment, already ordered/merged by the caller.
    /// process-wrapper's `execvp` searches ITS PATH, which is this one.
    pub environment: Vec<(Vec<u8>, Vec<u8>)>,
    pub working_directory: &'a Path,
    pub timeout: Duration,
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Outcome {
    /// `return_code`, stdout, stderr (StarlarkExecutionResult fields).
    Completed(i64, Vec<u8>, Vec<u8>),
    /// Raw status 143 (SIGTERM, or an exit with status 143) is an interrupt
    /// in Bazel (StarlarkExecutionResult.java:232-240); never a result.
    Interrupted,
}

/// process-wrapper's `Usage(...)` text for an empty command line
/// (process-wrapper-options.cc:34-55,135-138). Bazel prints its installed
/// wrapper path as `argv[0]`; the native profile has no install base.
pub(crate) const USAGE_PROGRAM: &str = "process-wrapper";

fn usage() -> Vec<u8> {
    let mut text = format!(
        "No command specified.\nUsage: {USAGE_PROGRAM} -- command arg1 @args\n\nPossible arguments:\n"
    );
    for line in [
        "  -g/--graceful_sigterm  propagate SIGTERM to the subprocess and delay the corresponding SIGKILL until --kill_delay has passed",
        "  -t/--timeout <timeout>  timeout after which the child process will be terminated with SIGTERM",
        "  -k/--kill_delay <timeout>  in case timeout occurs, how long to wait before killing the child with SIGKILL",
        "  -o/--stdout <file>  redirect stdout to a file",
        "  -e/--stderr <file>  redirect stderr to a file",
        "  -s/--stats <file>  if set, write stats in protobuf format to a file",
        "  -d/--debug  if set, debug info will be printed",
        "  --  command to run inside sandbox, followed by arguments",
    ] {
        text.push_str(line);
        text.push('\n');
    }
    text.into_bytes()
}

fn strerror(errno: i32) -> String {
    let text = io::Error::from_raw_os_error(errno).to_string();
    match text.rsplit_once(" (os error ") {
        Some((text, _)) => text.to_owned(),
        None => text,
    }
}

/// process-wrapper's `DIE("execvp(%s, ...)", args[0])`: file, line, quoted
/// message, then `perror` text (process-wrapper-legacy.cc:79-81, logging.h:34-40).
fn execvp_failure(program: &[u8], errno: i32) -> Vec<u8> {
    let mut text = b"src/main/tools/process-wrapper-legacy.cc:80: \"execvp(".to_vec();
    text.extend_from_slice(program);
    text.extend_from_slice(format!(", ...)\": {}\n", strerror(errno)).as_bytes());
    text
}

/// glibc `execvp` candidates: a name with a slash is used as is (relative to
/// the working directory); otherwise every PATH entry ("" means ".") is tried.
fn candidates(request: &Request<'_>, program: &[u8]) -> Vec<PathBuf> {
    let cwd = request.working_directory;
    if program.contains(&b'/') {
        return vec![cwd.join(Path::new(OsStr::from_bytes(program)))];
    }
    let path = request
        .environment
        .iter()
        .find(|(name, _)| name == b"PATH")
        .map_or(DEFAULT_PATH, |(_, value)| value.as_slice());
    path.split(|byte| *byte == b':')
        .map(|entry| {
            let directory = if entry.is_empty() {
                b".".as_slice()
            } else {
                entry
            };
            cwd.join(Path::new(OsStr::from_bytes(directory)))
                .join(Path::new(OsStr::from_bytes(program)))
        })
        .collect()
}

/// Bazel's TerminationStatus mapping of the wrapper's raw status (an exit
/// code, or 128+signal because process-wrapper re-raises the child's signal):
/// `exited()` is false for 129..=191, whose streams Bazel then reports empty
/// (StarlarkExecutionResult.java:221-246).
fn classify(raw: i64, stdout: Vec<u8>, stderr: Vec<u8>) -> Outcome {
    if (129..=191).contains(&raw) {
        if raw == 143 {
            Outcome::Interrupted
        } else {
            Outcome::Completed(raw, Vec::new(), Vec::new())
        }
    } else {
        Outcome::Completed(raw, stdout, stderr)
    }
}

struct Capture {
    data: Arc<Mutex<(Vec<u8>, Option<String>)>>,
    thread: JoinHandle<()>,
}

impl Capture {
    fn start(mut pipe: impl Read + Send + 'static) -> Result<Self, String> {
        let data = Arc::new(Mutex::new((Vec::new(), None)));
        let shared = data.clone();
        let thread = thread::Builder::new()
            .name("repository-execute-drain".into())
            .spawn(move || {
                let mut buffer = [0u8; 8192];
                loop {
                    match pipe.read(&mut buffer) {
                        Ok(0) => break,
                        Ok(n) => {
                            let mut data = shared.lock().unwrap_or_else(|e| e.into_inner());
                            data.0.extend_from_slice(&buffer[..n]);
                        }
                        Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                        Err(e) => {
                            shared.lock().unwrap_or_else(|e| e.into_inner()).1 =
                                Some(e.to_string());
                            break;
                        }
                    }
                }
            })
            .map_err(|e| format!("cannot spawn execute() pipe reader: {e}"))?;
        Ok(Self { data, thread })
    }

    fn finish(self, deadline: Instant, stream: &str, errors: &mut Vec<String>) -> Vec<u8> {
        let mut polls = 0;
        while !self.thread.is_finished() && Instant::now() < deadline {
            thread::sleep(backoff(&mut polls));
        }
        if !self.thread.is_finished() {
            errors.push(format!(
                "execute() {stream} pipe stayed open after process-group cleanup (a descendant escaped the group); reader detached"
            ));
        } else if self.thread.join().is_err() {
            errors.push(format!("execute() {stream} reader panicked"));
        }
        let mut data = self.data.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(error) = data.1.take() {
            errors.push(format!("cannot read execute() {stream}: {error}"));
        }
        std::mem::take(&mut data.0)
    }
}

/// glibc __execvpe's search (posix/execvpe.c) over `candidates` for the
/// program named `first`, starting each candidate with `spawn` (`true`: as a
/// script through /bin/sh, after ENOEXEC): EACCES, ENOENT, ESTALE, ENOTDIR,
/// ENODEV and ETIMEDOUT continue the search; any other error (e.g. ETXTBSY)
/// ends it with that errno. Only an exhausted search reports EACCES if it was
/// seen, otherwise the last error. The process-wrapper runs it for a
/// supervised execute (crate::process_wrapper), the executor itself for a
/// direct one.
pub(crate) fn search(
    candidates: &[PathBuf],
    first: &[u8],
    mut spawn: impl FnMut(&Path, bool) -> io::Result<Child>,
) -> Result<Child, i32> {
    let mut failure = ENOENT;
    let mut denied = false;
    let mut hard = false;
    for candidate in candidates {
        let spawned = match spawn(candidate, false) {
            Err(error) if error.raw_os_error() == Some(ENOEXEC) => spawn(candidate, true),
            result => result,
        };
        match spawned {
            Ok(child) => return Ok(child),
            Err(error) => {
                let errno = error.raw_os_error().unwrap_or(ENOENT);
                denied |= errno == EACCES;
                failure = errno;
                if !matches!(
                    errno,
                    ENOENT | ENOTDIR | EACCES | ESTALE | ENODEV | ETIMEDOUT
                ) {
                    hard = true;
                    break;
                }
                if first.contains(&b'/') {
                    break;
                }
            }
        }
    }
    Err(if denied && !hard { EACCES } else { failure })
}

/// Run one command. `Err` is a native-profile failure (supervision
/// failure); spawn failures and timeouts are results, like Bazel.
pub(crate) fn run(request: &Request<'_>) -> Result<Outcome, String> {
    if request.timeout.is_zero() {
        return Err("execute() timeout must be positive".into());
    }
    let deadline = Instant::now()
        .checked_add(request.timeout)
        .ok_or("execute() timeout is too large")?;
    let Some(first) = request.argv.first().cloned() else {
        // process-wrapper exits with EXIT_FAILURE after its usage text.
        return Ok(Outcome::Completed(1, Vec::new(), usage()));
    };
    let environment = || {
        request.environment.iter().map(|(name, value)| {
            (
                OsString::from_vec(name.clone()),
                OsString::from_vec(value.clone()),
            )
        })
    };
    let candidates = candidates(request, &first);
    let started = if crate::process_wrapper::enabled() {
        // Bazel runs the command through its process-wrapper, which searches
        // like execvp and kills every process the command leaves.
        let mut launch = Launch::execvp(&candidates, &request.argv);
        launch
            .command()
            .env_clear()
            .envs(environment())
            .current_dir(request.working_directory)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        launch
            .start()
            .map_err(|error| error.raw_os_error().unwrap_or(ENOENT))
    } else {
        search(&candidates, &first, |program, shell| {
            let mut command = Command::new(if shell { Path::new("/bin/sh") } else { program });
            if shell {
                // glibc execvp runs a file that fails with ENOEXEC through /bin/sh.
                command.arg(program);
            } else {
                command.arg0(OsStr::from_bytes(&first));
            }
            command
                .args(request.argv[1..].iter().map(|arg| OsStr::from_bytes(arg)))
                .env_clear()
                .envs(environment())
                .current_dir(request.working_directory)
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .process_group(0);
            crate::signals::reset_in_child(&mut command, crate::signals::Defaults::ProcessWrapper);
            crate::spawn_lock::spawn(&mut command)
        })
        .map(Spawned::direct)
    };
    let mut child = match started {
        Ok(child) => child,
        Err(errno) => {
            return Ok(Outcome::Completed(
                1,
                Vec::new(),
                execvp_failure(&first, errno),
            ));
        }
    };
    let stdout = child.take_stdout().ok_or("execute() stdout pipe missing");
    let stderr = child.take_stderr().ok_or("execute() stderr pipe missing");
    let captures = match (stdout, stderr) {
        (Ok(stdout), Ok(stderr)) => {
            Capture::start(stdout).and_then(|out| Capture::start(stderr).map(|err| (out, err)))
        }
        _ => Err("execute() pipes missing".to_owned()),
    };
    let mut errors = Vec::new();
    let mut timed_out = false;
    let mut status = None;
    let mut polls = 0;
    if let Err(error) = &captures {
        errors.push(error.clone());
    } else {
        loop {
            if Instant::now() >= deadline {
                timed_out = true;
                break;
            }
            // A speculative fetch the pool stopped (crate::repository_speculation).
            if crate::repository_speculation::cancelled() {
                errors.push(crate::repository_speculation::CANCELLED.to_owned());
                break;
            }
            match child.try_wait() {
                Ok(Some(exit)) => {
                    status = Some(exit);
                    break;
                }
                Ok(None) => thread::sleep(
                    backoff(&mut polls).min(deadline.saturating_duration_since(Instant::now())),
                ),
                Err(error) => {
                    errors.push(format!("cannot wait for execute() process: {error}"));
                    break;
                }
            }
        }
    }
    // At the deadline (or after a wait error) the command and its group are
    // killed; what it leaves is killed once it is gone, also after a normal
    // exit (the process-wrapper's TerminateAndWaitForAll, or the group kill
    // of a direct start). Then reap.
    if status.is_none() {
        child.kill();
    }
    let waited = child.wait();
    match waited {
        Ok(exit) if status.is_none() && !timed_out && errors.is_empty() => status = Some(exit),
        Ok(_) => {}
        Err(error) => errors.push(format!("cannot reap execute() process: {error}")),
    }
    let (stdout, stderr) = match captures {
        Ok((out, err)) => {
            let drain = Instant::now() + DRAIN_TIMEOUT;
            (
                out.finish(drain, "stdout", &mut errors),
                err.finish(drain, "stderr", &mut errors),
            )
        }
        Err(_) => (Vec::new(), Vec::new()),
    };
    if !errors.is_empty() {
        return Err(errors.join("; "));
    }
    if timed_out {
        // TerminationStatus.toString() for a timeout (TerminationStatus.java:229-236).
        return Ok(Outcome::Completed(256, Vec::new(), b"Timed out".to_vec()));
    }
    let status = status.ok_or("execute() process status unavailable")?;
    let raw = match (status.code(), status.signal()) {
        (Some(code), _) => i64::from(code),
        (None, Some(signal)) => 128 + i64::from(signal),
        (None, None) => return Err("execute() process ended without status".into()),
    };
    Ok(classify(raw, stdout, stderr))
}

/// Bazel prints captured output for quiet=False to its stderr
/// (StarlarkExecutionResult.java:205-207).
pub(crate) fn echo(stdout: &[u8], stderr: &[u8]) {
    if !stdout.is_empty() || !stderr.is_empty() {
        crate::ui::note_printed();
    }
    let mut sink = io::stderr().lock();
    let _ = sink.write_all(stdout);
    let _ = sink.write_all(stderr);
    let _ = sink.flush();
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request<'a>(argv: &[&str], cwd: &'a Path, path: Option<&'a [u8]>) -> Request<'a> {
        Request {
            argv: argv.iter().map(|a| a.as_bytes().to_vec()).collect(),
            environment: path
                .map(|path| vec![(b"PATH".to_vec(), path.to_vec())])
                .unwrap_or_default(),
            working_directory: cwd,
            timeout: Duration::from_secs(20),
        }
    }

    fn completed(outcome: Outcome) -> (i64, Vec<u8>, Vec<u8>) {
        match outcome {
            Outcome::Completed(code, stdout, stderr) => (code, stdout, stderr),
            Outcome::Interrupted => panic!("interrupted"),
        }
    }

    // Scripts the tests run are written by `write_executable_script`, never
    // through a descriptor of this process, so exec cannot see ETXTBSY.
    include!("../tests/support/executable_script.rs");

    fn run_completed(request: &Request<'_>) -> (i64, Vec<u8>, Vec<u8>) {
        completed(run(request).unwrap())
    }

    /// Regression guard for the test-harness ETXTBSY flake of the
    /// cc_configure-shaped repository_ctx test. A script written through a
    /// descriptor of this process cannot be executed while any copy of that
    /// descriptor is open, and a child that another test thread spawns during
    /// the write holds such a copy until its own exec. With that leaked copy
    /// simulated deterministically, exec fails like process-wrapper's execvp
    /// with empty stdout (the flake); `write_executable_script` writes from
    /// another process and renames a fresh inode into place, so the same
    /// leaked descriptor cannot touch the script it produces.
    /// Poll waits start at 50 µs and double up to POLL.
    #[test]
    fn poll_waits_back_off_to_the_poll_interval() {
        let mut n = 0;
        let waits: Vec<_> = (0..9).map(|_| backoff(&mut n).as_micros()).collect();
        assert_eq!(waits, [50, 100, 200, 400, 800, 1600, 3200, 5000, 5000]);
    }

    #[test]
    fn leaked_write_descriptors_make_exec_busy_but_never_for_written_scripts() {
        use std::os::unix::fs::PermissionsExt;
        let cwd = tempfile::tempdir_in(crate::test_temp::root()).unwrap();
        let script = cwd.path().join("tool");
        let mut leaked = std::fs::File::create(&script).unwrap();
        leaked.write_all(b"#!/bin/sh\necho stale\n").unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        let stale = cwd.path().join("stale");
        std::fs::hard_link(&script, &stale).unwrap();
        let stale = stale.to_str().unwrap();
        assert_eq!(
            run_completed(&request(&[stale], cwd.path(), None)),
            (1, Vec::new(), execvp_failure(stale.as_bytes(), 26))
        );
        write_executable_script(&script, "#!/bin/sh\necho ran\n", 0o755);
        assert_eq!(
            run_completed(&request(&[script.to_str().unwrap()], cwd.path(), None)),
            (0, b"ran\n".to_vec(), Vec::new())
        );
        drop(leaked);
    }

    #[test]
    fn exit_codes_streams_signals_and_spawn_failures_match_bazel_results() {
        let cwd = tempfile::tempdir_in(crate::test_temp::root()).unwrap();
        let path = Some(b"/usr/bin:/bin".as_slice());
        assert_eq!(
            run(&request(
                &["sh", "-c", "printf out; printf err >&2; exit 3"],
                cwd.path(),
                path
            ))
            .unwrap(),
            Outcome::Completed(3, b"out".to_vec(), b"err".to_vec())
        );
        // Exit statuses 129..=191 are not `exited()` (TerminationStatus.java:
        // 158-160): Bazel reports the raw status with empty streams, and 143
        // (an exit OR SIGTERM) is an interrupt (StarlarkExecutionResult.java:221-246).
        for (script, raw) in [
            ("printf out; printf err >&2; exit 130", 130),
            ("printf out; printf err >&2; exit 191", 191),
            ("printf out; printf err >&2; kill -9 $$", 137),
            ("printf out; printf err >&2; kill -INT $$", 130),
        ] {
            assert_eq!(
                run(&request(&["/bin/sh", "-c", script], cwd.path(), path)).unwrap(),
                Outcome::Completed(raw, Vec::new(), Vec::new()),
                "{script}"
            );
        }
        for script in ["printf out; exit 143", "printf out; kill -15 $$"] {
            assert_eq!(
                run(&request(&["/bin/sh", "-c", script], cwd.path(), path)).unwrap(),
                Outcome::Interrupted,
                "{script}"
            );
        }
        for (script, raw) in [("printf out; exit 128", 128), ("printf out; exit 192", 192)] {
            assert_eq!(
                run(&request(&["/bin/sh", "-c", script], cwd.path(), path)).unwrap(),
                Outcome::Completed(raw, b"out".to_vec(), Vec::new())
            );
        }
        // process-wrapper's execvp failure (process-wrapper-legacy.cc:79-81).
        for (argv, shown, error) in [
            (
                "no-such-program-n21",
                "no-such-program-n21",
                "No such file or directory",
            ),
            (
                "/nonexistent/prog",
                "/nonexistent/prog",
                "No such file or directory",
            ),
            ("./missing.sh", "./missing.sh", "No such file or directory"),
        ] {
            assert_eq!(
                run_completed(&request(&[argv, "x"], cwd.path(), path)),
                (
                    1,
                    Vec::new(),
                    format!(
                        "src/main/tools/process-wrapper-legacy.cc:80: \"execvp({shown}, ...)\": {error}\n"
                    )
                    .into_bytes()
                )
            );
        }
        // An unset PATH uses glibc's default /bin:/usr/bin, like execvp.
        assert_eq!(run_completed(&request(&["true"], cwd.path(), None)).0, 0);
        // The CHILD environment's PATH is searched (process-wrapper's execvp).
        assert_eq!(
            run_completed(&request(&["true"], cwd.path(), Some(b"/n21-missing"))).0,
            1
        );
    }

    #[test]
    fn empty_command_and_permission_denied_follow_process_wrapper() {
        let cwd = tempfile::tempdir_in(crate::test_temp::root()).unwrap();
        let (code, stdout, stderr) = run_completed(&request(&[], cwd.path(), None));
        assert_eq!((code, stdout.len()), (1, 0));
        let stderr = String::from_utf8(stderr).unwrap();
        assert!(
            stderr.starts_with("No command specified.\nUsage: process-wrapper -- command arg1 @args\n\nPossible arguments:\n  -g/--graceful_sigterm  propagate SIGTERM")
                && stderr.ends_with("  --  command to run inside sandbox, followed by arguments\n"),
            "{stderr}"
        );
        let denied = cwd.path().join("bin/ne.sh");
        std::fs::create_dir(cwd.path().join("bin")).unwrap();
        write_executable_script(&denied, "#!/bin/sh\necho no\n", 0o644);
        let bin = cwd.path().join("bin");
        let search = format!("{}:/usr/bin:/bin", bin.display());
        for (argv, path) in [("./bin/ne.sh", None), ("ne.sh", Some(search.as_bytes()))] {
            assert_eq!(
                run_completed(&request(&[argv], cwd.path(), path)),
                (
                    1,
                    Vec::new(),
                    format!("src/main/tools/process-wrapper-legacy.cc:80: \"execvp({argv}, ...)\": Permission denied\n")
                        .into_bytes()
                )
            );
        }
        // EACCES does not stop the search: a later executable candidate runs.
        let later = cwd.path().join("later");
        std::fs::create_dir(&later).unwrap();
        write_executable_script(&later.join("ne.sh"), "#!/bin/sh\necho later\n", 0o755);
        let search = format!("{}:{}", bin.display(), later.display());
        assert_eq!(
            run_completed(&request(&["ne.sh"], cwd.path(), Some(search.as_bytes()))),
            (0, b"later\n".to_vec(), Vec::new())
        );
    }

    #[test]
    fn a_hard_exec_error_stops_the_search_with_its_own_errno() {
        // glibc __execvpe: ETXTBSY is not a "continue" errno, so the search
        // stops there and reports it even after an earlier EACCES candidate.
        use std::os::unix::fs::PermissionsExt;
        let cwd = tempfile::tempdir_in(crate::test_temp::root()).unwrap();
        let denied = cwd.path().join("denied");
        let busy = cwd.path().join("busy");
        let later = cwd.path().join("later");
        for dir in [&denied, &busy, &later] {
            std::fs::create_dir(dir).unwrap();
        }
        write_executable_script(&denied.join("t.sh"), "#!/bin/sh\necho denied\n", 0o644);
        write_executable_script(&later.join("t.sh"), "#!/bin/sh\necho later\n", 0o755);
        // Deliberately held open for writing: this candidate is the ETXTBSY.
        let mut open = std::fs::File::create(busy.join("t.sh")).unwrap();
        std::io::Write::write_all(&mut open, b"#!/bin/sh\necho busy\n").unwrap();
        std::fs::set_permissions(busy.join("t.sh"), std::fs::Permissions::from_mode(0o755))
            .unwrap();
        let search = format!(
            "{}:{}:{}",
            denied.display(),
            busy.display(),
            later.display()
        );
        assert_eq!(
            completed(run(&request(&["t.sh"], cwd.path(), Some(search.as_bytes()))).unwrap()),
            (
                1,
                Vec::new(),
                b"src/main/tools/process-wrapper-legacy.cc:80: \"execvp(t.sh, ...)\": Text file busy\n"
                    .to_vec()
            )
        );
        drop(open);
    }

    #[test]
    fn relative_argv0_is_working_directory_relative_and_scripts_without_shebang_run_with_sh() {
        let cwd = tempfile::tempdir_in(crate::test_temp::root()).unwrap();
        let script = cwd.path().join("tool");
        write_executable_script(&script, "printf \"$0:$1:$PWD\"\n", 0o755);
        let (code, stdout, _) = run_completed(&request(&["./tool", "x"], cwd.path(), None));
        assert_eq!(code, 0);
        let text = String::from_utf8(stdout).unwrap();
        assert!(
            text.ends_with(&format!(":x:{}", cwd.path().display())),
            "{text}"
        );
    }

    #[test]
    fn environment_is_exact_and_stdin_is_empty() {
        let cwd = tempfile::tempdir_in(crate::test_temp::root()).unwrap();
        let mut value = request(&["/bin/sh", "-c", "env | sort; cat"], cwd.path(), None);
        value.environment = vec![(b"ONLY".to_vec(), b"\xff value".to_vec())];
        let Outcome::Completed(code, stdout, _) = run(&value).unwrap() else {
            panic!()
        };
        assert_eq!(code, 0);
        assert!(
            stdout.windows(12).any(|w| w == b"ONLY=\xff value"),
            "{stdout:?}"
        );
        assert!(!stdout.windows(5).any(|w| w == b"HOME="));
    }

    #[test]
    fn timeout_kills_group_and_reports_bazel_timeout_result() {
        let cwd = tempfile::tempdir_in(crate::test_temp::root()).unwrap();
        let mut value = request(&["/bin/sh", "-c", "sleep 30 & sleep 30"], cwd.path(), None);
        value.timeout = Duration::from_millis(200);
        let start = Instant::now();
        assert_eq!(
            run(&value).unwrap(),
            Outcome::Completed(256, Vec::new(), b"Timed out".to_vec())
        );
        // Below the 30 s the group sleeps: the kill, not the children's
        // exit, ended it (above a loaded host's scheduling delays).
        assert!(start.elapsed() < Duration::from_secs(20));
    }

    #[test]
    fn large_output_is_captured_whole_without_a_native_cap() {
        // Above the former 16 MiB native capture limit; Bazel buffers it all.
        let cwd = tempfile::tempdir_in(crate::test_temp::root()).unwrap();
        let value = request(
            &["/bin/sh", "-c", "head -c 17000000 /dev/zero"],
            cwd.path(),
            None,
        );
        match run(&value).unwrap() {
            Outcome::Completed(0, stdout, stderr) => {
                assert_eq!(stdout.len(), 17_000_000);
                assert!(stderr.is_empty());
            }
            other => panic!("{other:?}"),
        }
    }
}
