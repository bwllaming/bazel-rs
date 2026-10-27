//! The `--quiet` startup option (docs/STARTUP_QUIET.md): only errors and
//! the command's own output reach the console.
//!
//! The fixture constants are shared verbatim with the paired genuine Bazel
//! 9.3.0 capture (/tmp/opencode/perf-research/quiet-capture.py extracts
//! them); its receipt is tests/fixtures/startup_quiet/reference_receipt.json.
//! Genuine 9.3 (server mode; `--batch` ignores `--quiet` for events) drops
//! under `--quiet` every DEBUG (`print()`), INFO (with a successful action's
//! or test's output bound to its `INFO: From …` event), WARNING and
//! PROGRESS event and the tests' FAIL lines, and keeps the ERROR events
//! (a failed action's output with them), the build result lines, the test
//! summary, `execute(quiet = False)` echoes and a `run` binary's output.
//! Each scenario below pins the native quiet stdout and stderr exactly.
#![allow(clippy::disallowed_methods)] // test harness: starts processes directly (clippy.toml)
include!("support/verify_env.rs");

use std::fs;
include!("support/temp_root.rs");
use std::io::{Read, Write};
use std::net::TcpListener;
use std::path::Path;
use std::process::{Command, Output};

const MODULE: &str = r##"module(name = "q")

print("module file print")

fetched = use_repo_rule("//:repo.bzl", "fetched")

fetched(name = "fetched")
"##;

const PAYLOAD: &str = r"payload
";

const REPO: &str = r##"def _impl(ctx):
    print("repository rule print")
    ctx.download(url = "http://127.0.0.1:PORT/payload.txt", output = "payload.txt", sha256 = "d4e4877bac978b7952f0d544fc52ebff5411d351d129f1f056fa43f11da9af2b")
    ctx.execute(["/bin/sh", "-c", "echo repo-execute-stdout; echo repo-execute-stderr >&2"], quiet = False)
    ctx.file("BUILD", 'exports_files(["payload.txt"])\n')

fetched = repository_rule(implementation = _impl)
"##;

const DEFS: &str = r##"print("bzl top-level print")

def _noisy(ctx):
    print("analysis print")
    out = ctx.actions.declare_file(ctx.label.name + ".txt")
    ctx.actions.run_shell(
        outputs = [out],
        command = "echo action-stdout; echo action-stderr >&2; echo x > " + out.path,
    )
    return [DefaultInfo(files = depset([out]))]

noisy = rule(implementation = _noisy)

def _fails(ctx):
    fail("intentional analysis failure")

fails_rule = rule(implementation = _fails)

def _exe(ctx):
    out = ctx.actions.declare_file(ctx.label.name + ".sh")
    ctx.actions.write(out, ctx.attr.script, is_executable = True)
    return [DefaultInfo(executable = out)]

sh_like_test = rule(implementation = _exe, test = True, attrs = {"script": attr.string()})

sh_like_binary = rule(implementation = _exe, executable = True, attrs = {"script": attr.string()})
"##;

const BUILD: &str = r##"load("//:defs.bzl", "fails_rule", "noisy", "sh_like_binary", "sh_like_test")

noisy(name = "noisy")

genrule(name = "old", outs = ["old.txt"], cmd = "echo > $@", deprecation = "use something else")

genrule(name = "uses_old", srcs = [":old"], outs = ["uses_old.txt"], cmd = "cat $< > $@")

genrule(name = "broken", outs = ["broken.txt"], cmd = "echo broken-stdout; echo broken-stderr >&2; exit 1")

fails_rule(name = "bad")

genrule(name = "fetched_copy", srcs = ["@fetched//:payload.txt"], outs = ["fetched_copy.txt"], cmd = "cp $< $@")

sh_like_test(name = "passes_test", script = "#!/bin/bash\necho pass-output\n")

sh_like_test(name = "fails_test", script = "#!/bin/bash\necho fail-output\nexit 1\n")

sh_like_binary(name = "bin", script = "#!/bin/bash\necho bin-stdout\necho bin-stderr >&2\n")
"##;

/// The scenarios (name, command and arguments), in the capture's order.
const SCENARIOS: &[(&str, &[&str])] = &[
    ("build_ok", &["build", "//:noisy", "//:uses_old"]),
    ("build_action_failure", &["build", "//:broken"]),
    ("build_analysis_failure", &["build", "//:bad"]),
    (
        "build_keep_going",
        &[
            "build",
            "-k",
            "//:noisy",
            "//:broken",
            "//:bad",
            "//:uses_old",
        ],
    ),
    ("test", &["test", "//:passes_test", "//:fails_test"]),
    (
        "test_output_errors",
        &[
            "test",
            "--test_output=errors",
            "//:passes_test",
            "//:fails_test",
        ],
    ),
    ("run", &["run", "//:bin"]),
    ("query", &["query", "//:all"]),
    ("fetch", &["build", "//:fetched_copy"]),
    ("info", &["info", "workspace"]),
];

/// The genuine exit code of each scenario (loud and quiet alike) and the
/// native quiet stdout and stderr, normalized (workspace `<WS>`, 64-hex
/// digests `<H>`, test times `<T>`).
fn expected(name: &str) -> (i32, &'static str, &'static str) {
    match name {
        "build_ok" => (
            0,
            "//:noisy\t<WS>/cache/actions/<H>/outputs/bazel-out/k8-fastbuild/bin/noisy.txt\n//:uses_old\t<WS>/cache/actions/<H>/outputs/bazel-out/k8-fastbuild/bin/uses_old.txt\n",
            "",
        ),
        "build_action_failure" => (
            1,
            "",
            "bazel-rs: action '//:broken': action '//:broken' failed (exit status: 1):\nstdout:\nbroken-stdout\n\nstderr:\nbroken-stderr\n\n\nUse --sandbox_debug to see verbose messages from the sandbox and retain the sandbox build root for debugging\n",
        ),
        "build_analysis_failure" => (
            1,
            "",
            "bazel-rs: analysis of //:bad: Traceback (most recent call last):\n  File <builtin>, in <module>\n  * //:defs.bzl:15, in _fails\n      fail(\"intentional analysis failure\")\nerror: fail: intentional analysis failure\n  --> //:defs.bzl:15:5\n   |\n15 |     fail(\"intentional analysis failure\")\n   |     ^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^\n   |\n\n",
        ),
        "build_keep_going" => (
            1,
            "//:noisy\t<WS>/cache/actions/<H>/outputs/bazel-out/k8-fastbuild/bin/noisy.txt\n//:uses_old\t<WS>/cache/actions/<H>/outputs/bazel-out/k8-fastbuild/bin/uses_old.txt\n",
            "ERROR: analysis of //:bad: Traceback (most recent call last):\n  File <builtin>, in <module>\n  * //:defs.bzl:15, in _fails\n      fail(\"intentional analysis failure\")\nerror: fail: intentional analysis failure\n  --> //:defs.bzl:15:5\n   |\n15 |     fail(\"intentional analysis failure\")\n   |     ^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^\n   |\n\nERROR: action '//:broken': action '//:broken' failed (exit status: 1):\nstdout:\nbroken-stdout\n\nstderr:\nbroken-stderr\n\n\nUse --sandbox_debug to see verbose messages from the sandbox and retain the sandbox build root for debugging\nUse --verbose_failures to see the command lines of failed build steps.\nERROR: Build did NOT complete successfully\n",
        ),
        "test" | "test_output_errors" => (
            3,
            "//:passes_test                                                           PASSED in <T>s\n//:fails_test                                                            FAILED in <T>s\n  <WS>/cache/tests/<H>/bazel-out/k8-fastbuild/testlogs/fails_test/test.log\n\nExecuted 2 out of 2 tests: 1 test passes and 1 fails locally.\nThere were tests whose specified size is too big. Use the --test_verbose_timeout_warnings command line option to see which ones these are.\n",
            "//:fails_test\t<WS>/cache/actions/<H>/outputs/bazel-out/k8-fastbuild/bin/fails_test.sh\n//:passes_test\t<WS>/cache/actions/<H>/outputs/bazel-out/k8-fastbuild/bin/passes_test.sh\n",
        ),
        "run" => (
            0,
            "bin-stdout\n",
            "//:bin\t<WS>/cache/actions/<H>/outputs/bazel-out/k8-fastbuild/bin/bin.sh\nTarget //:bin up-to-date:\n  <WS>/cache/worktrees/<W>/execroot/_main/bazel-out/k8-fastbuild/bin/bin.sh\nbin-stderr\n",
        ),
        "query" => (
            0,
            "//:bad\n//:bin\n//:broken\n//:fails_test\n//:fetched_copy\n//:noisy\n//:old\n//:passes_test\n//:uses_old\n",
            "",
        ),
        "fetch" => (
            0,
            "//:fetched_copy\t<WS>/cache/actions/<H>/outputs/bazel-out/k8-fastbuild/bin/fetched_copy.txt\n",
            "repo-execute-stdout\nrepo-execute-stderr\nTarget //:fetched_copy up-to-date:\n  <WS>/cache/worktrees/<W>/execroot/_main/bazel-out/k8-fastbuild/bin/fetched_copy.txt\n",
        ),
        "info" => (0, "<WS>\n", ""),
        other => panic!("unknown scenario {other}"),
    }
}

/// Serves PAYLOAD to every request, for the process's lifetime.
fn serve_payload() -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            let mut request = Vec::new();
            let mut byte = [0];
            while !request.ends_with(b"\r\n\r\n") && stream.read(&mut byte).unwrap_or(0) == 1 {
                request.push(byte[0]);
            }
            let _ = stream.write_all(
                format!(
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{PAYLOAD}",
                    PAYLOAD.len()
                )
                .as_bytes(),
            );
        }
    });
    port
}

struct Workspace(tempfile::TempDir);

impl Drop for Workspace {
    /// Published test outputs are read-only; make the tree writable so the
    /// temporary directory goes.
    fn drop(&mut self) {
        let _ = Command::new("chmod")
            .args(["-R", "u+w"])
            .arg(self.0.path())
            .status();
    }
}

impl Workspace {
    fn new() -> Self {
        let port = serve_payload();
        let ws = Self(
            tempfile::Builder::new()
                .prefix("startup-quiet-")
                .tempdir_in(crate::test_temp::root())
                .unwrap(),
        );
        let repo = REPO.replace("PORT", &port.to_string());
        for (name, text) in [
            ("MODULE.bazel", MODULE),
            ("BUILD.bazel", BUILD),
            ("defs.bzl", DEFS),
            ("repo.bzl", repo.as_str()),
        ] {
            fs::write(ws.path().join(name), text).unwrap();
        }
        ws
    }

    fn path(&self) -> &Path {
        self.0.path()
    }

    /// `bazel-rs [STARTUP...] COMMAND --experimental-builtin-modules=false
    /// ARGS...` with the cache `<ws>/<cache>` and extra environment.
    fn run(&self, startup: &[&str], cache: &str, args: &[&str], env: &[(&str, &str)]) -> Output {
        let mut command = args.to_vec();
        if command[0] != "info" {
            command.insert(1, "--experimental-builtin-modules=false");
        }
        let output = Command::new("timeout")
            .args(["--kill-after=5s", "600s", env!("CARGO_BIN_EXE_bazel-rs")])
            .args(startup)
            .arg(format!("--cache-dir={}", self.path().join(cache).display()))
            .args(&command)
            .current_dir(self.path())
            .env_clear()
            .envs(verifier_environment())
            .env("PATH", "/usr/bin:/bin")
            .env("HOME", self.path())
            .envs(env.iter().copied())
            .output()
            .unwrap();
        assert_ne!(output.status.code(), Some(124), "CLI timed out: {args:?}");
        output
    }

    /// Remove `<ws>/<name>` (published outputs are read-only).
    fn remove(&self, name: &str) {
        let path = self.path().join(name);
        if path.exists() {
            let _ = Command::new("chmod")
                .args(["-R", "u+w"])
                .arg(&path)
                .status();
            fs::remove_dir_all(&path).unwrap();
        }
    }

    /// `text` with the workspace path `<WS>`, 64-hex digests `<H>` and test
    /// times `<T>` ([`test_times`]).
    fn normalize(&self, text: &[u8]) -> String {
        let text =
            String::from_utf8_lossy(text).replace(&self.path().display().to_string(), "<WS>");
        let mut out = String::new();
        let mut run = String::new();
        for c in text.chars() {
            if c.is_ascii_hexdigit() && !c.is_ascii_uppercase() {
                run.push(c);
                continue;
            }
            out.push_str(if run.len() == 64 { "<H>" } else { &run });
            run.clear();
            out.push(c);
        }
        out.push_str(if run.len() == 64 { "<H>" } else { &run });
        test_times(&worktree_names(&out))
    }
}

/// `text` with the workspace's state directory name in the cache
/// (`worktrees/<name>-<hash>`, crate::worktree) as `worktrees/<W>`.
fn worktree_names(text: &str) -> String {
    let mut out = String::new();
    let mut rest = text;
    while let Some(at) = rest.find("/worktrees/") {
        let start = at + "/worktrees/".len();
        out.push_str(&rest[..start]);
        out.push_str("<W>");
        let end = rest[start..]
            .find('/')
            .map_or(rest.len(), |slash| start + slash);
        rest = &rest[end..];
    }
    out.push_str(rest);
    out
}

/// `text` with the test summary's run times (` in 0.1s` at a line's end,
/// which vary with the load) as ` in <T>s`.
fn test_times(text: &str) -> String {
    let mut out = String::new();
    for line in text.split_inclusive('\n') {
        let (body, newline) = match line.strip_suffix('\n') {
            Some(body) => (body, "\n"),
            None => (line, ""),
        };
        match body.rsplit_once(" in ") {
            Some((head, time))
                if time
                    .strip_suffix('s')
                    .is_some_and(|seconds| seconds.parse::<f64>().is_ok()) =>
            {
                out.push_str(head);
                out.push_str(" in <T>s");
                out.push_str(newline);
            }
            _ => out.push_str(line),
        }
    }
    out
}

/// What planning printed: the lines before execution's first report (an
/// action's logs, the native summary), without the analysis snapshot's
/// diagnostics (`BAZEL_RS_SNAPSHOT_EXPLAIN`).
fn planning_lines(text: &str) -> String {
    text.lines()
        .take_while(|line| {
            !line.starts_with("action '")
                && !line.contains(" actions executed, ")
                && !line.starts_with("Target ")
        })
        .filter(|line| !line.starts_with("bazel-rs-snapshot: "))
        .map(|line| format!("{line}\n"))
        .collect()
}

#[test]
fn quiet_keeps_only_errors_and_the_commands_output() {
    let ws = Workspace::new();
    let mut failures = Vec::new();
    for (name, args) in SCENARIOS {
        let (exit, stdout, stderr) = expected(name);
        // Each run with a fresh cache: the quiet one executes too.
        let loud = ws.run(&["--ignore_all_rc_files"], "cache", args, &[]);
        ws.remove("cache");
        let quiet = ws.run(&["--ignore_all_rc_files", "--quiet"], "cache", args, &[]);
        ws.remove("cache");
        let (quiet_stdout, quiet_stderr) =
            (ws.normalize(&quiet.stdout), ws.normalize(&quiet.stderr));
        if loud.status.code() != Some(exit)
            || quiet.status.code() != Some(exit)
            || quiet_stdout != stdout
            || quiet_stderr != stderr
        {
            failures.push(format!(
                "{name}: exit loud {:?} quiet {:?} (want {exit})\n--- quiet stdout\n{quiet_stdout:?}\n--- quiet stderr\n{quiet_stderr:?}",
                loud.status.code(),
                quiet.status.code()
            ));
        }
        // What the quiet run drops is there without --quiet (the
        // informational lines of the fixture), unless it is `info`.
        let loud_stderr = ws.normalize(&loud.stderr);
        if *name != "info"
            && !(loud_stderr.contains("bzl top-level print")
                && loud_stderr.contains("DEBUG: <WS>/MODULE.bazel:3:6: module file print"))
        {
            failures.push(format!(
                "{name}: loud stderr without the print:\n{loud_stderr}"
            ));
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n\n"));
}

#[test]
fn quiet_is_console_only_for_the_analysis_snapshot() {
    let ws = Workspace::new();
    let explain = [("BAZEL_RS_SNAPSHOT_EXPLAIN", "1")];
    let args = ["build", "--lockfile_mode=off", "//:noisy"];
    let loud = ["--ignore_all_rc_files"];
    let quiet = ["--ignore_all_rc_files", "--quiet"];
    // The full analysis without --quiet: what a hit must replay.
    let reference = ws.run(&loud, "reference", &args, &explain);
    assert!(reference.status.success());
    let reference = ws.normalize(&reference.stderr);
    assert!(
        reference.contains("bazel-rs-snapshot: saved "),
        "{reference}"
    );
    assert!(reference.contains("analysis print"), "{reference}");
    let reference = planning_lines(&reference);
    // A quiet full analysis saves the snapshot (the key is the same) ...
    let first = ws.run(&quiet, "cache", &args, &explain);
    assert!(first.status.success());
    let first = ws.normalize(&first.stderr);
    assert!(first.contains("bazel-rs-snapshot: saved "), "{first}");
    assert_eq!(planning_lines(&first), "", "{first}");
    // ... whose hit replays every note to a loud run ...
    let hit = ws.run(&loud, "cache", &args, &explain);
    assert!(hit.status.success());
    let hit = ws.normalize(&hit.stderr);
    assert!(hit.contains("bazel-rs-snapshot: hit "), "{hit}");
    assert_eq!(planning_lines(&hit), reference, "{hit}");
    // ... and none to a quiet one.
    let quiet_hit = ws.run(&quiet, "cache", &args, &explain);
    assert!(quiet_hit.status.success());
    let quiet_hit = ws.normalize(&quiet_hit.stderr);
    assert!(quiet_hit.contains("bazel-rs-snapshot: hit "), "{quiet_hit}");
    assert_eq!(planning_lines(&quiet_hit), "", "{quiet_hit}");
    // A loud recording's hit under --quiet is silent too.
    let quiet_hit = ws.run(&quiet, "reference", &args, &explain);
    let quiet_hit = ws.normalize(&quiet_hit.stderr);
    assert!(quiet_hit.contains("bazel-rs-snapshot: hit "), "{quiet_hit}");
    assert_eq!(planning_lines(&quiet_hit), "", "{quiet_hit}");
}

#[test]
fn quiet_from_rc_files_and_its_negation() {
    let ws = Workspace::new();
    let rc = ws.path().join("quiet.bazelrc");
    fs::write(&rc, "startup --quiet\n").unwrap();
    let rc_flag = format!("--bazelrc={}", rc.display());
    let args = ["query", "//:all"];
    let quiet = ws.run(
        &["--nohome_rc", "--noworkspace_rc", "--nosystem_rc", &rc_flag],
        "cache",
        &args,
        &[],
    );
    assert!(quiet.status.success());
    assert_eq!(ws.normalize(&quiet.stderr), "");
    let loud = ws.run(
        &[
            "--nohome_rc",
            "--noworkspace_rc",
            "--nosystem_rc",
            &rc_flag,
            "--noquiet",
        ],
        "cache",
        &args,
        &[],
    );
    assert!(loud.status.success());
    assert!(ws.normalize(&loud.stderr).contains("bzl top-level print"));
    // The last value wins on the command line too.
    let quiet = ws.run(
        &["--ignore_all_rc_files", "--noquiet", "--quiet"],
        "cache",
        &args,
        &[],
    );
    assert_eq!(ws.normalize(&quiet.stderr), "");
}
