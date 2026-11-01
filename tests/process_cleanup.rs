//! Spawn cleanup like Bazel's process-wrapper (docs/PROCESS_WRAPPER.md): every
//! local spawn — a test, a build action under the sandbox worker or
//! standalone, a repository rule's `execute` — runs under a supervisor that,
//! once the spawn's main process has exited (or timed out), kills and reaps
//! every process it left, also those that left its process group (`setsid`
//! daemons such as embedded PostgreSQL's `pg_ctl start`, a daemon's double
//! fork). Genuine Bazel 9.3 leaves none of them running either
//! (process-wrapper-legacy.cc:53-62,138-156; TerminateAndWaitForAll,
//! process-tools-linux.cc:24-55; linux-sandbox's PID namespace), while the
//! exit statuses, test results and failure messages stay the spawn's own.
//!
//! The daemons write their PIDs to files under the test's own directory (in
//! /tmp, writable inside the sandbox) before the spawn's main process
//! continues, so every check first proves the daemon existed. The commands
//! run with the analysis snapshot's default (on): none repeats an earlier
//! command on its cache, so each is a miss that runs the full analysis, and
//! execution — the subject — is the same with the snapshot on or off.
#![allow(clippy::disallowed_methods)] // test harness: starts processes directly (clippy.toml)

use std::fs;
use std::io::{Read, Write};
use std::os::fd::OwnedFd;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::time::{Duration, Instant};
include!("support/explicit_modules.rs");

/// Starts `sleep 300` as a daemon that escapes the caller's process group in
/// the requested way, writes the daemon's PID to PIDFILE and returns once it
/// is written. `linger` then keeps the caller running (a timeout fixture).
const DAEMON: &str = r#"#!/bin/sh
pidfile=$1
case "$2" in
  run)
    echo $$ > "$pidfile.tmp" && mv "$pidfile.tmp" "$pidfile"
    exec sleep 300 ;;
  setsid)
    setsid "$0" "$pidfile" run > /dev/null 2>&1 < /dev/null & ;;
  double)
    setsid sh -c '"$0" "$1" run > /dev/null 2>&1 < /dev/null &' "$0" "$pidfile" ;;
  group)
    "$0" "$pidfile" run > /dev/null 2>&1 < /dev/null & ;;
esac
while [ ! -s "$pidfile" ]; do sleep 0.02; done
"#;

const DEFS: &str = r#"def _t(ctx):
    out = ctx.actions.declare_file(ctx.label.name + ".sh")
    ctx.actions.write(out, ctx.attr.script, is_executable = True)
    return [DefaultInfo(executable = out, runfiles = ctx.runfiles(files = ctx.files.data))]

script_test = rule(
    implementation = _t,
    test = True,
    attrs = {"script": attr.string(), "data": attr.label_list(allow_files = True)},
)
"#;

struct Workspace {
    temp: tempfile::TempDir,
}

impl Workspace {
    fn new() -> Self {
        let ws = Self {
            temp: tempfile::Builder::new()
                .prefix("process-cleanup-")
                .tempdir_in(crate::test_temp::root())
                .unwrap(),
        };
        fs::create_dir_all(ws.pids()).unwrap();
        ws
    }

    fn path(&self) -> &Path {
        self.temp.path()
    }

    fn pids(&self) -> PathBuf {
        self.path().join("pids")
    }

    fn pid_file(&self, name: &str) -> String {
        self.pids()
            .join(format!("{name}.pid"))
            .display()
            .to_string()
    }

    fn file(&self, name: &str, text: &str) {
        let path = self.path().join(name);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, text).unwrap();
    }

    /// `pkg/daemon.sh`, executable (written by a separate process, so no
    /// descriptor of this test binary can make it busy).
    fn daemon_script(&self) -> String {
        let path = self.path().join("pkg/daemon.sh");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        write_executable_script(&path, DAEMON, 0o755);
        path.display().to_string()
    }

    fn run(&self, cache: &str, args: &[&str]) -> Output {
        let output = Command::new("timeout")
            .args(["--kill-after=5s", "180s", env!("CARGO_BIN_EXE_bazel-rs")])
            .args(["--ignore_all_rc_files", &format!("--cache-dir={cache}")])
            .args(args)
            .current_dir(self.path())
            .explicit_modules()
            .output()
            .unwrap();
        assert_ne!(output.status.code(), Some(124), "CLI timed out: {args:?}");
        output
    }

    /// Every daemon `names` started (its PID file exists) is gone.
    fn assert_daemons_gone(&self, names: &[&str]) {
        for name in names {
            let path = self.pid_file(name);
            let text = fs::read_to_string(&path)
                .unwrap_or_else(|error| panic!("daemon {name} never started ({path}): {error}"));
            let pid: u32 = text.trim().parse().unwrap();
            assert!(
                !is_daemon(pid),
                "daemon {name} (pid {pid}) outlived its spawn: {}",
                ps_line(pid)
            );
        }
    }
}

impl Drop for Workspace {
    fn drop(&mut self) {
        // A failed check must not leave its daemons running for minutes.
        if let Ok(entries) = fs::read_dir(self.pids()) {
            for entry in entries.flatten() {
                if let Ok(text) = fs::read_to_string(entry.path())
                    && let Ok(pid) = text.trim().parse::<u32>()
                    && is_daemon(pid)
                {
                    let _ = Command::new("kill").args(["-9", &pid.to_string()]).status();
                }
            }
        }
    }
}

include!("support/executable_script.rs");

/// Whether `pid` is still one of the fixture's `sleep 300` daemons (not a
/// zombie, not a reused PID).
fn is_daemon(pid: u32) -> bool {
    fs::read(format!("/proc/{pid}/cmdline")).is_ok_and(|cmdline| cmdline == b"sleep\x00300\x00")
}

fn ps_line(pid: u32) -> String {
    fs::read_to_string(format!("/proc/{pid}/stat")).unwrap_or_default()
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

/// The test fixture: tests whose script starts the three kinds of daemons
/// and then passes, fails or times out.
fn test_fixture(ws: &Workspace, prefix: &str) {
    ws.daemon_script();
    let script = |name: &str, tail: &str| {
        format!(
            "#!/bin/sh\npkg/daemon.sh {} setsid\npkg/daemon.sh {} double\npkg/daemon.sh {} group\necho started\n{tail}\n",
            ws.pid_file(&format!("{prefix}{name}-setsid")),
            ws.pid_file(&format!("{prefix}{name}-double")),
            ws.pid_file(&format!("{prefix}{name}-group")),
        )
    };
    ws.file("MODULE.bazel", "module(name = 'process_cleanup')\n");
    ws.file("pkg/defs.bzl", DEFS);
    ws.file(
        "pkg/BUILD.bazel",
        &format!(
            r#"load(":defs.bzl", "script_test")
exports_files(["daemon.sh"])
script_test(name = "pass_test", data = ["daemon.sh"], script = """{}""")
script_test(name = "fail_test", data = ["daemon.sh"], script = """{}""")
script_test(name = "slow_test", data = ["daemon.sh"], script = """{}""")
"#,
            script("pass", "exit 0"),
            script("fail", "exit 3"),
            script("slow", "sleep 60"),
        ),
    );
}

fn kinds(prefix: &str, name: &str) -> Vec<String> {
    ["setsid", "double", "group"]
        .iter()
        .map(|kind| format!("{prefix}{name}-{kind}"))
        .collect()
}

fn check_tests(strategy: &str) {
    let ws = Workspace::new();
    test_fixture(&ws, strategy);
    let output = ws.run(
        "cache",
        &[
            "test",
            &format!("--spawn_strategy={strategy}"),
            "--test_output=errors",
            "//pkg:pass_test",
            "//pkg:fail_test",
        ],
    );
    let out = stdout(&output);
    // The results are the tests' own: one passes, one fails with its code.
    assert_eq!(output.status.code(), Some(3), "{out}\n{}", stderr(&output));
    assert!(
        out.lines()
            .any(|line| line.starts_with("//pkg:pass_test") && line.contains("PASSED")),
        "{out}"
    );
    assert!(
        out.lines()
            .any(|line| line.starts_with("//pkg:fail_test") && line.contains("FAILED")),
        "{out}"
    );
    assert!(stderr(&output).contains("(Exit 3)"), "{}", stderr(&output));
    let names: Vec<String> = [kinds(strategy, "pass"), kinds(strategy, "fail")].concat();
    ws.assert_daemons_gone(&names.iter().map(String::as_str).collect::<Vec<_>>());
}

#[test]
fn test_daemons_are_killed_after_the_test_sandboxed() {
    check_tests("sandboxed");
}

#[test]
fn test_daemons_are_killed_after_the_test_standalone() {
    check_tests("standalone");
}

#[test]
fn test_daemons_are_killed_after_a_timeout() {
    let ws = Workspace::new();
    test_fixture(&ws, "t");
    let start = Instant::now();
    let output = ws.run("cache", &["test", "--test_timeout=2", "//pkg:slow_test"]);
    let out = stdout(&output);
    assert_eq!(output.status.code(), Some(3), "{out}\n{}", stderr(&output));
    assert!(
        out.lines()
            .any(|line| line.starts_with("//pkg:slow_test") && line.contains("TIMEOUT")),
        "{out}"
    );
    // SIGTERM ended the script well before its 60 s sleep.
    assert!(start.elapsed() < Duration::from_secs(50), "{out}");
    let names = kinds("t", "slow");
    ws.assert_daemons_gone(&names.iter().map(String::as_str).collect::<Vec<_>>());
}

#[test]
fn interrupting_bazel_rs_kills_its_running_test_and_daemons() {
    use std::os::unix::process::{CommandExt, ExitStatusExt};
    let ws = Workspace::new();
    test_fixture(&ws, "i");
    // bazel-rs leads its own process group, like a shell job: Ctrl-C sends
    // SIGINT to that group only (the spawns run in groups of their own).
    let mut bazel = Command::new(env!("CARGO_BIN_EXE_bazel-rs"))
        .args([
            "--ignore_all_rc_files",
            "--cache-dir=cache",
            "test",
            "//pkg:slow_test",
        ])
        .current_dir(ws.path())
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .process_group(0)
        .explicit_modules()
        .spawn()
        .unwrap();
    let names = kinds("i", "slow");
    let deadline = Instant::now() + Duration::from_secs(150);
    while !names
        .iter()
        .all(|name| fs::metadata(ws.pid_file(name)).is_ok_and(|meta| meta.len() > 0))
    {
        assert!(
            bazel.try_wait().unwrap().is_none(),
            "bazel-rs ended before the test ran"
        );
        assert!(Instant::now() < deadline, "the test never started");
        std::thread::sleep(Duration::from_millis(50));
    }
    let interrupted = Command::new("kill")
        .args(["-INT", "--", &format!("-{}", bazel.id())])
        .status()
        .unwrap();
    assert!(interrupted.success());
    // SIGINT still ends bazel-rs at once, with Bazel's INTERRUPTED exit
    // code 8.
    let status = bazel.wait().unwrap();
    assert_eq!((status.code(), status.signal()), (Some(8), None));
    // Its supervisors saw it go: the test and every daemon it started are
    // killed (shortly after, not 60 s later when the test would end).
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        let alive: Vec<&String> = names
            .iter()
            .filter(|name| {
                fs::read_to_string(ws.pid_file(name))
                    .ok()
                    .and_then(|text| text.trim().parse().ok())
                    .is_some_and(is_daemon)
            })
            .collect();
        if alive.is_empty() {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "daemons outlived the interrupted bazel-rs: {alive:?}"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// The session and process group of `pid` (/proc/<pid>/stat fields 6, 5).
fn session_and_group(pid: u32) -> (u32, u32) {
    let stat = fs::read_to_string(format!("/proc/{pid}/stat")).unwrap();
    let fields: Vec<&str> = stat.rsplit_once(") ").unwrap().1.split(' ').collect();
    (fields[3].parse().unwrap(), fields[2].parse().unwrap())
}

/// A test runs in a session and process group of its own, like under Bazel's
/// process-wrapper (whose child calls setsid(), process-wrapper-legacy.cc:
/// 68-71): `kill -TERM 0` in a test (a cleanup trap, a Go test killing its
/// group) ends the test only — never bazel-rs, the shell that started it, or
/// that shell's job (the CI step) — and the test shares no session, and so
/// no controlling terminal, with them.
fn check_test_isolation(strategy: &str) {
    let ws = Workspace::new();
    let ids = ws.pid_file(&format!("{strategy}-ids"));
    ws.file("MODULE.bazel", "module(name = 'process_cleanup')\n");
    ws.file("pkg/defs.bzl", DEFS);
    ws.file(
        "pkg/BUILD.bazel",
        &format!(
            r##"load(":defs.bzl", "script_test")
script_test(name = "group_kill_test", script = """#!/bin/sh
set -- $(sed 's/.*) //' /proc/$$/stat)
echo $4 $3 > {ids}.tmp && mv {ids}.tmp {ids}
kill -TERM 0
sleep 30
"""
)
"##
        ),
    );
    let output = ws.run(
        "cache",
        &[
            "test",
            &format!("--spawn_strategy={strategy}"),
            "--test_output=errors",
            "//pkg:group_kill_test",
        ],
    );
    let out = stdout(&output);
    // bazel-rs survived the test's group kill and reports the test's own
    // failure (killed by its SIGTERM), well before the 30 s sleep.
    assert_eq!(output.status.code(), Some(3), "{out}\n{}", stderr(&output));
    assert!(
        out.lines()
            .any(|line| line.starts_with("//pkg:group_kill_test") && line.contains("FAILED")),
        "{out}"
    );
    let text = fs::read_to_string(&ids).unwrap_or_else(|error| panic!("{ids}: {error}"));
    let (session, group) = text
        .trim()
        .split_once(' ')
        .map(|(session, group)| {
            (
                session.parse::<u32>().unwrap(),
                group.parse::<u32>().unwrap(),
            )
        })
        .unwrap();
    // bazel-rs ran in this process's session and group (no setsid).
    let (own_session, own_group) = session_and_group(std::process::id());
    assert_ne!(session, own_session, "the test shares bazel-rs's session");
    assert_ne!(group, own_group, "the test shares bazel-rs's process group");
}

#[test]
fn a_test_killing_its_process_group_ends_only_itself_sandboxed() {
    check_test_isolation("sandboxed");
}

#[test]
fn a_test_killing_its_process_group_ends_only_itself_standalone() {
    check_test_isolation("standalone");
}

#[test]
fn action_and_repository_daemons_are_killed_and_statuses_are_exact() {
    let ws = Workspace::new();
    let daemon = ws.daemon_script();
    ws.file(
        "MODULE.bazel",
        "module(name = 'process_cleanup')\nprobe = use_repo_rule('//pkg:repo.bzl', 'probe')\nprobe(name = 'probe')\n",
    );
    ws.file(
        "pkg/repo.bzl",
        &format!(
            r#"def _probe_impl(rctx):
    first = rctx.execute(["{daemon}", "{setsid}", "setsid"])
    second = rctx.execute(["/bin/sh", "-c", '"$0" "$1" double && echo executed', "{daemon}", "{double}"])
    late = rctx.execute(["/bin/sh", "-c", '"$0" "$1" setsid && sleep 60', "{daemon}", "{timeout}"], timeout = 2)
    rctx.file("marker", "rc=%d,%d out=%s timeout_rc=%d timeout_err=%s" % (
        first.return_code, second.return_code, second.stdout.strip(), late.return_code, late.stderr))
    rctx.file("BUILD.bazel", 'exports_files(["marker"])\n')

probe = repository_rule(implementation = _probe_impl)
"#,
            setsid = ws.pid_file("repo-setsid"),
            double = ws.pid_file("repo-double"),
            timeout = ws.pid_file("repo-timeout"),
        ),
    );
    let genrule = |name: &str, tags: &str| {
        format!(
            r#"genrule(name = "{name}", srcs = ["daemon.sh"], outs = ["{name}.txt"], tags = [{tags}], cmd = "$(location daemon.sh) {setsid} setsid && $(location daemon.sh) {double} double && $(location daemon.sh) {group} group && echo ok > $@")
"#,
            setsid = ws.pid_file(&format!("{name}-setsid")),
            double = ws.pid_file(&format!("{name}-double")),
            group = ws.pid_file(&format!("{name}-group")),
        )
    };
    ws.file(
        "pkg/BUILD.bazel",
        &format!(
            r#"exports_files(["daemon.sh"])
{}{}genrule(name = "uses_repo", srcs = ["@probe//:marker"], outs = ["uses_repo.txt"], cmd = "cp $< $@")
genrule(name = "killed", outs = ["killed.txt"], cmd = "kill -9 $$$$")
genrule(name = "piped", outs = ["piped.txt"], cmd = "kill -PIPE $$$$")
genrule(name = "exits", outs = ["exits.txt"], cmd = "echo out; echo err >&2; exit 3")
"#,
            genrule("gen", ""),
            genrule("gen_local", "\"local\""),
        ),
    );
    let output = ws.run(
        "cache",
        &["build", "//pkg:gen", "//pkg:gen_local", "//pkg:uses_repo"],
    );
    assert!(output.status.success(), "{}", stderr(&output));
    // The build prints `label<TAB>published output` per target.
    let listing = stdout(&output);
    let published = listing
        .lines()
        .find_map(|line| line.strip_prefix("//pkg:uses_repo\t"))
        .unwrap_or_else(|| panic!("{listing}"));
    let marker = fs::read_to_string(published).unwrap();
    assert_eq!(
        marker,
        "rc=0,0 out=executed timeout_rc=256 timeout_err=Timed out",
        "{}",
        stderr(&output)
    );
    let mut names: Vec<String> = [kinds("", "gen"), kinds("", "gen_local")].concat();
    names.extend(["repo-setsid", "repo-double", "repo-timeout"].map(String::from));
    ws.assert_daemons_gone(&names.iter().map(String::as_str).collect::<Vec<_>>());
    // A spawn's own status survives the supervisor exactly: a signal it
    // could not re-raise (SIGPIPE) as much as SIGKILL or an exit code.
    for (target, status) in [
        ("killed", "(signal: 9 (SIGKILL))"),
        ("piped", "(signal: 13 (SIGPIPE))"),
        ("exits", "(exit status: 3)"),
    ] {
        let output = ws.run("cache", &["build", &format!("//pkg:{target}")]);
        let text = stderr(&output);
        assert_eq!(output.status.code(), Some(1), "{text}");
        assert!(
            text.contains(&format!("action '//pkg:{target}' failed {status}:")),
            "{target}: {text}"
        );
    }
    let output = ws.run(
        "cache",
        &["build", "--spawn_strategy=standalone", "//pkg:exits"],
    );
    let text = stderr(&output);
    assert!(
        text.contains("failed (exit status: 3):\nstdout:\nout\n\nstderr:\nerr\n"),
        "{text}"
    );
}

/// The supervisor itself, driven through its control channel the way the
/// executor drives it.
struct Supervisor {
    child: Child,
    control: UnixStream,
    pending: Vec<u8>,
}

impl Supervisor {
    fn start(args: &[&str]) -> Self {
        let (control, theirs) = UnixStream::pair().unwrap();
        let child = Command::new(env!("CARGO_BIN_EXE_bazel-rs"))
            .arg("__process-wrapper")
            .args(args)
            .stdin(Stdio::from(OwnedFd::from(theirs)))
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .explicit_modules()
            .spawn()
            .unwrap();
        control
            .set_read_timeout(Some(Duration::from_secs(60)))
            .unwrap();
        Self {
            child,
            control,
            pending: Vec::new(),
        }
    }

    fn line(&mut self) -> String {
        loop {
            if let Some(end) = self.pending.iter().position(|byte| *byte == b'\n') {
                let line = String::from_utf8(self.pending[..end].to_vec()).unwrap();
                self.pending.drain(..=end);
                return line;
            }
            let mut buffer = [0u8; 64];
            let read = self.control.read(&mut buffer).unwrap();
            if read == 0 {
                return String::new();
            }
            self.pending.extend_from_slice(&buffer[..read]);
        }
    }

    fn finish(mut self) -> (String, Option<i32>) {
        let line = self.line();
        let status = self.child.wait().unwrap();
        (line, status.code())
    }
}

#[test]
fn the_supervisor_reports_exact_statuses_and_obeys_its_executor() {
    // The PID files live in a workspace's pids directory: its drop kills a
    // daemon that a failed check left running.
    let ws = Workspace::new();
    let pid_of = |name: &str| -> u32 {
        let path = ws.pids().join(name);
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            if let Ok(text) = fs::read_to_string(&path)
                && let Ok(pid) = text.trim().parse()
            {
                return pid;
            }
            assert!(Instant::now() < deadline, "{name} never written");
            std::thread::sleep(Duration::from_millis(10));
        }
    };
    let file = |name: &str| ws.pids().join(name).display().to_string();

    // An exit code, with a setsid daemon left behind: X carries the raw
    // status (3 << 8) and the daemon is gone once it arrives.
    let script = format!(
        "setsid sh -c 'echo $$ > {}; exec sleep 300' > /dev/null 2>&1 < /dev/null & while [ ! -s {} ]; do sleep 0.02; done; exit 3",
        file("exit.pid"),
        file("exit.pid")
    );
    let mut supervisor = Supervisor::start(&["--", "/bin/sh", "-c", &script]);
    assert!(supervisor.line().starts_with('S'));
    let (end, code) = supervisor.finish();
    assert_eq!((end.as_str(), code), ("X768", Some(3)));
    let daemon = pid_of("exit.pid");
    assert!(!is_daemon(daemon), "{}", ps_line(daemon));

    // A reparented daemon that ends while the command still runs is reaped at
    // once, so a command that waits for its end with kill(pid, 0) sees it
    // gone (the process-wrapper's wait() loop; an unreaped zombie would keep
    // `kill -0` succeeding and the script would exit 42).
    let daemon_script = ws.daemon_script();
    let script = format!(
        "{daemon_script} {file} double; pid=$(cat {file}); kill $pid; i=0; while kill -0 $pid 2>/dev/null; do i=$((i+1)); if [ $i -gt 300 ]; then exit 42; fi; sleep 0.01; done; exit 0",
        file = file("reaped.pid")
    );
    let mut supervisor = Supervisor::start(&["--", "/bin/sh", "-c", &script]);
    assert!(supervisor.line().starts_with('S'));
    assert_eq!(supervisor.finish(), ("X0".to_owned(), Some(0)));

    // A signal the supervisor could not re-raise itself: SIGPIPE (13).
    let mut supervisor = Supervisor::start(&["--", "/bin/sh", "-c", "kill -PIPE $$"]);
    assert!(supervisor.line().starts_with('S'));
    assert_eq!(supervisor.finish(), ("X13".to_owned(), Some(141)));

    // `T`: SIGTERM to the command's group (a trap turns it into exit 9).
    let mut supervisor = Supervisor::start(&[
        "--",
        "/bin/sh",
        "-c",
        &format!(
            "trap 'exit 9' TERM; echo $$ > {}; while :; do sleep 0.05; done",
            file("term.pid")
        ),
    ]);
    assert!(supervisor.line().starts_with('S'));
    pid_of("term.pid");
    supervisor.control.write_all(b"T").unwrap();
    assert_eq!(supervisor.finish(), ("X2304".to_owned(), Some(9)));

    // `K`: SIGKILL, also for a command that ignores SIGTERM.
    let mut supervisor = Supervisor::start(&[
        "--",
        "/bin/sh",
        "-c",
        &format!("trap '' TERM; echo $$ > {}; sleep 300", file("kill.pid")),
    ]);
    assert!(supervisor.line().starts_with('S'));
    pid_of("kill.pid");
    supervisor.control.write_all(b"T").unwrap();
    std::thread::sleep(Duration::from_millis(200));
    supervisor.control.write_all(b"K").unwrap();
    assert_eq!(supervisor.finish(), ("X9".to_owned(), Some(137)));

    // End-of-file (the executor died): the command and what it started go.
    let mut supervisor = Supervisor::start(&[
        "--",
        "/bin/sh",
        "-c",
        &format!(
            "setsid sh -c 'echo $$ > {}; exec sleep 300' > /dev/null 2>&1 < /dev/null & exec sleep 300",
            file("eof.pid")
        ),
    ]);
    assert!(supervisor.line().starts_with('S'));
    let daemon = pid_of("eof.pid");
    let Supervisor {
        mut child, control, ..
    } = supervisor;
    drop(control);
    assert_eq!(child.wait().unwrap().code(), Some(137));
    assert!(!is_daemon(daemon), "{}", ps_line(daemon));

    // A command that cannot start is reported with its errno.
    let supervisor = Supervisor::start(&["--", "/nonexistent/process-wrapper-tool"]);
    assert_eq!(supervisor.finish(), ("E2".to_owned(), Some(1)));

    // glibc execvp's search over the given candidates (repository execute).
    let mut supervisor = Supervisor::start(&[
        "--execvp",
        "--candidate",
        "/nonexistent/true",
        "--candidate",
        "/bin/true",
        "--",
        "true",
    ]);
    assert!(supervisor.line().starts_with('S'));
    assert_eq!(supervisor.finish(), ("X0".to_owned(), Some(0)));
}
