//! N59 CLI coverage: sandboxed local execution (docs/SANDBOX.md). The
//! `sandboxed` strategy (the default, like Bazel 9.3 on Linux) executes
//! actions through the Landlock sandbox worker; `standalone` runs them
//! directly. Out-of-sandbox writes fail with EACCES plus Bazel's
//! --sandbox_debug suggestion; action keys are strategy-independent; tests
//! run sandboxed like any action; --sandbox_writable_path grants writes;
//! --sandbox_block_path removes reads; the `requires-network` tag lifts the
//! TCP restriction. Paired capture:
//! tests/fixtures/sandbox/reference_receipt.json.
//!
//! The fixtures' outside-sandbox targets live under $HOME: /tmp is writable
//! in the sandbox by design (LinuxSandboxedSpawnRunner.getWritableDirs keeps
//! /tmp writable), so a workspace under /tmp cannot demonstrate a denial.
#![allow(clippy::disallowed_methods)] // test harness: starts processes directly (clippy.toml)

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

const DEFS: &str = r##"def _t(ctx):
    out = ctx.actions.declare_file(ctx.label.name + ".sh")
    ctx.actions.write(out, ctx.attr.script, is_executable = True)
    return [DefaultInfo(executable = out, runfiles = ctx.runfiles(files = ctx.files.data))]

_ATTRS = {"script": attr.string(), "data": attr.label_list(allow_files = True)}
sh_like_test = rule(implementation = _t, test = True, attrs = _ATTRS)
"##;

/// AbstractSandboxSpawnRunner.SANDBOX_DEBUG_SUGGESTION, printed after a
/// sandboxed action failure unless --sandbox_debug is on.
const SUGGESTION: &str = "Use --sandbox_debug to see verbose messages from the sandbox and retain the sandbox build root for debugging";

struct Workspace(tempfile::TempDir);

impl Workspace {
    fn new() -> Self {
        let ws = Self(
            tempfile::Builder::new()
                .prefix("sandbox-cli-")
                .tempdir_in("/tmp/opencode")
                .unwrap(),
        );
        ws.file("MODULE.bazel", "module(name = 'sandbox_fixture')\n");
        ws.file("defs.bzl", DEFS);
        ws
    }

    fn path(&self) -> &Path {
        self.0.path()
    }

    fn file(&self, name: &str, text: &str) {
        let path = self.path().join(name);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, text).unwrap();
    }

    /// A coder-owned directory outside every sandbox-writable path (not under
    /// /tmp, the action tree or TMPDIR): the only writes a sandboxed action
    /// can attempt and fail, as on any developer host where $HOME is the
    /// writable target.
    /// The outside-sandbox target of the running test (each test's own name:
    /// the tests run in parallel and must not observe each other's markers).
    fn outside(&self) -> PathBuf {
        let test = std::thread::current()
            .name()
            .unwrap_or("test")
            .replace("::", "-");
        let home = std::env::var("HOME").unwrap_or_else(|_| "/home/coder".to_owned());
        let dir = PathBuf::from(home).join(format!("n59-sandbox-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        dir.join(test)
    }

    /// The genrule BUILD fixture: `ok` copies an input, `leak` writes to
    /// `marker` (outside every sandbox-writable directory), `net` connects
    /// to localhost, `net2` the same with `requires-network`.
    fn build_fixture(&self, marker: &Path) {
        let marker = marker.display().to_string();
        self.file("pkg/in.txt", "content\n");
        self.file(
            "pkg/BUILD.bazel",
            &format!(
                r#"genrule(name = 'ok', srcs = ['in.txt'], outs = ['ok.txt'], cmd = 'cp $(location in.txt) $@')
genrule(name = 'leak', outs = ['leak.txt'], cmd = 'echo leaked > {marker} && touch $@')
genrule(name = 'net', outs = ['net.txt'], cmd = '(echo > /dev/tcp/127.0.0.1/9) 2> $@ || true')
genrule(name = 'net2', tags = ['requires-network'], outs = ['net2.txt'], cmd = '(echo > /dev/tcp/127.0.0.1/9) 2> $@ || true')
genrule(name = 'blocked', outs = ['blocked.txt'], cmd = 'cat /etc/hostname > $@ 2>&1 || true')
genrule(name = 'tmpwrite', outs = ['tmpwrite.txt'], cmd = 'echo t > /tmp/n59-tmpwrite-marker && echo done > $@')
"#
            ),
        );
    }

    fn leak_rule(&self, marker: &Path) {
        self.file("pkg/in.txt", "content\n");
        self.file(
            "pkg/BUILD.bazel",
            &format!(
                "genrule(name = 'leak', outs = ['leak.txt'], cmd = 'echo leaked > {} && touch $@')",
                marker.display()
            ),
        );
    }

    fn run(&self, cache: &str, args: &[&str]) -> Output {
        let output = Command::new("timeout")
            .args(["--kill-after=5s", "120s", env!("CARGO_BIN_EXE_bazel-rs")])
            .args(["--ignore_all_rc_files", &format!("--cache-dir={cache}")])
            .args(args)
            .current_dir(self.path())
            .output()
            .unwrap();
        assert_ne!(output.status.code(), Some(124), "CLI timed out: {args:?}");
        output
    }

    fn text(output: &Output) -> String {
        String::from_utf8_lossy(&output.stderr).into_owned()
    }
}

#[test]
fn default_strategy_is_sandboxed_and_builds() {
    let ws = Workspace::new();
    let marker = ws.outside();
    ws.build_fixture(&marker);
    let output = ws.run("cache", &["build", "//pkg:ok"]);
    assert!(output.status.success(), "{}", Workspace::text(&output));
    let text = Workspace::text(&output);
    assert!(
        text.contains("(experimental local execution; sandboxed)"),
        "summary names the sandboxed mode: {text}"
    );
}

#[test]
fn outside_write_fails_sandboxed_with_bazels_suggestion() {
    let ws = Workspace::new();
    let marker = ws.outside();
    ws.build_fixture(&marker);
    let output = ws.run("cache", &["build", "//pkg:leak"]);
    assert!(!output.status.success());
    let text = Workspace::text(&output);
    assert!(
        text.contains("Permission denied"),
        "the sandbox denies the write: {text}"
    );
    assert!(
        text.contains(SUGGESTION),
        "Bazel's sandbox failure suggestion follows: {text}"
    );
    assert!(!marker.exists(), "the write never happened");
}

#[test]
fn outside_write_passes_under_standalone() {
    let ws = Workspace::new();
    let marker = ws.outside();
    ws.build_fixture(&marker);
    let output = ws.run(
        "cache",
        &["build", "--spawn_strategy=standalone", "//pkg:leak"],
    );
    assert!(output.status.success(), "{}", Workspace::text(&output));
    assert!(marker.exists());
    assert!(
        Workspace::text(&output).contains("(experimental local execution; not sandboxed)"),
        "summary names the standalone mode"
    );
}

#[test]
fn linux_sandbox_is_the_sandboxed_identifier_alias() {
    // SandboxModule.java:326 registers "sandboxed" and "linux-sandbox" for
    // the same strategy.
    let ws = Workspace::new();
    let marker = ws.outside();
    ws.build_fixture(&marker);
    let output = ws.run(
        "cache",
        &["build", "--spawn_strategy=linux-sandbox", "//pkg:leak"],
    );
    assert!(!output.status.success());
    assert!(Workspace::text(&output).contains(SUGGESTION));
    assert!(!marker.exists());
}

#[test]
fn local_is_the_standalone_identifier_alias() {
    // StandaloneModule.java:98 registers "standalone" and "local" for the
    // same strategy.
    let ws = Workspace::new();
    let marker = ws.outside();
    ws.build_fixture(&marker);
    let output = ws.run("cache", &["build", "--spawn_strategy=local", "//pkg:leak"]);
    assert!(output.status.success(), "{}", Workspace::text(&output));
    assert!(marker.exists());
}

#[test]
fn strategy_by_mnemonic_overrides_spawn_strategy() {
    // SpawnStrategyRegistry.getStrategies:142-143: the --strategy list for
    // the mnemonic beats --spawn_strategy.
    let ws = Workspace::new();
    let marker = ws.outside();
    ws.build_fixture(&marker);
    let output = ws.run(
        "cache",
        &[
            "build",
            "--spawn_strategy=sandboxed",
            "--strategy=Genrule=standalone",
            "//pkg:leak",
        ],
    );
    assert!(output.status.success(), "{}", Workspace::text(&output));
    assert!(marker.exists());
}

#[test]
fn genrule_strategy_selects_genrules() {
    // ExecutionOptions.java:77-87: --genrule_strategy applies to Genrule
    // only (deprecated in favor of --strategy=Genrule=).
    let ws = Workspace::new();
    let marker = ws.outside();
    ws.build_fixture(&marker);
    let output = ws.run(
        "cache",
        &["build", "--genrule_strategy=standalone", "//pkg:leak"],
    );
    assert!(output.status.success(), "{}", Workspace::text(&output));
    assert!(marker.exists());
}

#[test]
fn action_keys_do_not_change_with_the_strategy() {
    // The strategy wraps execution only; the same action key must hit the
    // cache under either strategy.
    let ws = Workspace::new();
    let marker = ws.outside();
    ws.build_fixture(&marker);
    let sandboxed = ws.run("cache", &["build", "//pkg:ok"]);
    assert!(
        sandboxed.status.success(),
        "{}",
        Workspace::text(&sandboxed)
    );
    assert!(Workspace::text(&sandboxed).contains("1 actions executed"));
    let standalone = ws.run(
        "cache",
        &["build", "--spawn_strategy=standalone", "//pkg:ok"],
    );
    assert!(
        standalone.status.success(),
        "{}",
        Workspace::text(&standalone)
    );
    assert!(
        Workspace::text(&standalone).contains("1 cache hits"),
        "standalone rebuild hits the sandboxed execution's cache entry: {}",
        Workspace::text(&standalone)
    );
}

#[test]
fn network_is_denied_by_default_and_required_by_tag() {
    let ws = Workspace::new();
    let marker = ws.outside();
    ws.build_fixture(&marker);
    let output = ws.run("cache", &["build", "//pkg:net"]);
    assert!(output.status.success(), "{}", Workspace::text(&output));
    let net = find_output(ws.path(), "cache", "net.txt");
    assert!(
        net.contains("Permission denied"),
        "TCP connect is denied by the sandbox: {net}"
    );
    let output = ws.run("cache2", &["build", "//pkg:net2"]);
    assert!(output.status.success(), "{}", Workspace::text(&output));
    let net2 = find_output(ws.path(), "cache2", "net2.txt");
    assert!(
        net2.contains("Connection refused") && !net2.contains("Permission denied"),
        "the requires-network tag lifts the restriction (the connection reaches the stack): {net2}"
    );
}

#[test]
fn tmp_stays_writable_like_the_linux_sandbox() {
    // LinuxSandboxedSpawnRunner.getWritableDirs:391-398 keeps /tmp writable.
    let ws = Workspace::new();
    let marker = ws.outside();
    ws.build_fixture(&marker);
    let output = ws.run("cache", &["build", "//pkg:tmpwrite"]);
    assert!(output.status.success(), "{}", Workspace::text(&output));
    assert!(fs::read_to_string("/tmp/n59-tmpwrite-marker").is_ok_and(|text| text.trim() == "t"));
    fs::remove_file("/tmp/n59-tmpwrite-marker").unwrap();
}

#[test]
fn sandbox_writable_path_grants_writes() {
    let ws = Workspace::new();
    let marker = ws.outside();
    ws.leak_rule(&marker);
    let denied = ws.run("cache", &["build", "//pkg:leak"]);
    assert!(
        !denied.status.success() && !marker.exists(),
        "without the grant the sandbox denies the write: {}",
        Workspace::text(&denied)
    );
    let parent = marker.parent().unwrap();
    let output = ws.run(
        "cache",
        &[
            "build",
            &format!("--sandbox_writable_path={}", parent.display()),
            "//pkg:leak",
        ],
    );
    assert!(output.status.success(), "{}", Workspace::text(&output));
    assert!(marker.exists());
}

#[test]
fn sandbox_block_path_removes_reads() {
    let ws = Workspace::new();
    let marker = ws.outside();
    ws.build_fixture(&marker);
    let output = ws.run(
        "cache",
        &[
            "build",
            "--sandbox_block_path=/etc/hostname",
            "//pkg:blocked",
        ],
    );
    assert!(output.status.success(), "{}", Workspace::text(&output));
    let blocked = find_output(ws.path(), "cache", "blocked.txt");
    assert!(
        blocked.contains("Permission denied"),
        "--sandbox_block_path denies the read: {blocked}"
    );
}

#[test]
fn sandbox_debug_retains_the_sandbox_build_root() {
    let ws = Workspace::new();
    let marker = ws.outside();
    ws.build_fixture(&marker);
    let output = ws.run("cache", &["build", "--sandbox_debug", "//pkg:leak"]);
    assert!(!output.status.success());
    let text = Workspace::text(&output);
    assert!(!text.contains(SUGGESTION), "no suggestion while debugging");
    assert!(
        text.contains("(sandbox build root retained at '"),
        "the retention note names the sandbox root: {text}"
    );
    assert!(!marker.exists());
    let root = text
        .split("(sandbox build root retained at '")
        .nth(1)
        .and_then(|rest| rest.split('\'').next())
        .map(PathBuf::from)
        .expect("retention note");
    assert!(root.join("sandbox-debug.log").is_file());
    assert!(root.join("execroot").is_dir());
    // The touch of $@ never ran (the write before it failed): the retained
    // sandbox shows exactly the failed action's partial state.
    // A fresh build (same cache) is unaffected by the retained root.
    let output = ws.run("cache", &["build", "//pkg:ok"]);
    assert!(output.status.success(), "{}", Workspace::text(&output));
}

#[test]
fn unknown_strategy_fails_with_bazels_error() {
    // SpawnStrategyRegistry.toStrategy:718-727: '...' was requested for ...
    // but no strategy with that identifier was registered.
    let ws = Workspace::new();
    let marker = ws.outside();
    ws.build_fixture(&marker);
    let output = ws.run("cache", &["build", "--spawn_strategy=bogus", "//pkg:ok"]);
    assert!(!output.status.success());
    let text = Workspace::text(&output);
    assert!(
        text.contains(
            "'bogus' was requested for --spawn_strategy but no strategy with that identifier \
             was registered. Valid values are: standalone, local, sandboxed, linux-sandbox"
        ),
        "{text}"
    );
}

#[test]
fn no_sandbox_tag_falls_through_to_standalone() {
    // Spawns.mayBeSandboxed (Spawns.java:63-67): the no-sandbox execution
    // requirement makes the sandboxed strategy skip the spawn; the next
    // strategy of the list runs it.
    let ws = Workspace::new();
    let marker = ws.outside();
    ws.file("pkg/in.txt", "content\n");
    ws.file(
        "pkg/BUILD.bazel",
        &format!(
            "genrule(name = 'leak', tags = ['no-sandbox'], outs = ['leak.txt'], cmd = 'echo leaked > {} && touch $@')",
            marker.display()
        ),
    );
    let output = ws.run("cache", &["build", "//pkg:leak"]);
    assert!(output.status.success(), "{}", Workspace::text(&output));
    assert!(marker.exists());
    assert!(
        Workspace::text(&output).contains("(experimental local execution; not sandboxed)"),
        "the summary reports the standalone execution"
    );
}

#[test]
fn tests_run_under_the_sandbox() {
    // N23g note: test actions stay local-only; under N59 they run through
    // the same strategy selection (the TestRunner mnemonic).
    let ws = Workspace::new();
    let marker = ws.outside();
    ws.file(
        "BUILD.bazel",
        &format!(
            "load(\"//:defs.bzl\", \"sh_like_test\")\nsh_like_test(name = \"leaky\", script = \"#!/bin/bash\\necho hi > {}\\n\")\n",
            marker.display()
        ),
    );
    let output = ws.run("cache", &["test", "//:leaky"]);
    assert!(
        !output.status.success(),
        "the sandboxed write fails the test"
    );
    assert!(!marker.exists());
    let output = ws.run(
        "cache",
        &["test", "--spawn_strategy=standalone", "//:leaky"],
    );
    assert!(output.status.success(), "{}", Workspace::text(&output));
    assert!(marker.exists());
}

fn find_output(root: &Path, cache: &str, name: &str) -> String {
    let actions = root.join(cache).join("actions");
    let mut found = None;
    visit(&actions, &mut |path| {
        if path.is_file() && path.file_name().is_some_and(|file| file == name) {
            found = Some(fs::read_to_string(path).unwrap_or_default());
        }
    });
    found.unwrap_or_else(|| {
        panic!(
            "no published output named {name} under {}",
            actions.display()
        )
    })
}

fn visit(directory: &Path, inspect: &mut dyn FnMut(&PathBuf)) {
    let entries = match fs::read_dir(directory) {
        Ok(entries) => entries,
        Err(_) => return,
    };
    for entry in entries.filter_map(|entry| entry.ok()) {
        let path = entry.path();
        if path.is_dir() {
            visit(&path, inspect);
        } else {
            inspect(&path);
        }
    }
}
