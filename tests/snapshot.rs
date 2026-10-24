//! L2-W5, the analysis snapshot store and hit path (PLAN-L2 §7 "Snapshot
//! fixtures" 1–18 and PLAN-L2 update 1 row W5): `BAZEL_RS_SNAPSHOT=on`
//! saves a full analysis and later skips loading and analysis when every
//! recorded input is unchanged.
//!
//! Every fixture runs each step twice, in lockstep: once with
//! `BAZEL_RS_SNAPSHOT=on` (and `BAZEL_RS_SNAPSHOT_EXPLAIN=1`) and once with
//! the variable unset, on two copies of the workspace and cache that live
//! under one path while they run (each copy is renamed into place for its
//! command, so absolute paths, cache keys and outputs are the same). The
//! exit code, stdout and stderr — without the `bazel-rs-snapshot:`
//! diagnostics and `PERF` lines — must be byte-identical, whatever the
//! snapshot did; the diagnostics say whether it missed, hit or saved.
//!
//! Fixture 19 (L2-W6): every fixture runs again with
//! `BAZEL_RS_SNAPSHOT=verify` on the snapshot side (module `verify_mode`):
//! each hit also runs the full analysis in its shadow and must agree with
//! it (a `verified` line; a mismatch would exit 70 and differ from the
//! snapshot-less run).
#![allow(clippy::disallowed_methods)] // test harness: starts processes directly (clippy.toml)

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

const ON: &str = "on";
const OFF: &str = "off";
const VERIFY: &str = "verify";

thread_local! {
    /// The `BAZEL_RS_SNAPSHOT` value of the snapshot side of this test's
    /// twins (`on`; fixture 19 runs every fixture again with `verify`).
    static MODE: std::cell::Cell<&'static str> = const { std::cell::Cell::new(ON) };
}

/// Two copies of a workspace (with its cache and home) that run under one
/// path: `<base>/run`.
struct Twin {
    base: tempfile::TempDir,
    /// The snapshot side's `BAZEL_RS_SNAPSHOT` ([`MODE`]).
    mode: &'static str,
}

/// The two runs of one step.
struct Pair {
    on: Output,
    off: Output,
    /// The snapshot side's mode.
    mode: &'static str,
}

impl Twin {
    fn new(files: &[(&str, &str)]) -> Self {
        let base = tempfile::Builder::new()
            .prefix("snapshot-w5-")
            .tempdir_in("/tmp/opencode")
            .unwrap();
        let twin = Self {
            base,
            mode: MODE.with(std::cell::Cell::get),
        };
        for side in [ON, OFF] {
            fs::create_dir_all(twin.side(side).join("ws")).unwrap();
            fs::create_dir_all(twin.side(side).join("home")).unwrap();
        }
        for (path, content) in files {
            twin.write(path, content);
        }
        twin
    }

    fn side(&self, side: &str) -> PathBuf {
        self.base.path().join(side)
    }

    /// Where every command runs: `<base>/run`.
    fn run_root(&self) -> PathBuf {
        self.base.path().join("run")
    }

    /// The workspace as the commands see it.
    fn ws(&self) -> PathBuf {
        self.run_root().join("ws")
    }

    fn cache(&self) -> PathBuf {
        self.run_root().join("cache")
    }

    /// Write `content` at `path` (below the workspace) in both copies.
    fn write(&self, path: &str, content: &str) {
        self.each(|root| {
            let path = root.join("ws").join(path);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, content).unwrap();
        });
    }

    /// `edit` on each copy where it is stored (relative edits).
    fn each(&self, edit: impl Fn(&Path)) {
        for side in [ON, OFF] {
            edit(&self.side(side));
        }
    }

    fn with_side<T>(&self, side: &str, body: impl FnOnce() -> T) -> T {
        fs::rename(self.side(side), self.run_root()).unwrap();
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(body));
        fs::rename(self.run_root(), self.side(side)).unwrap();
        match result {
            Ok(value) => value,
            Err(panic) => std::panic::resume_unwind(panic),
        }
    }

    fn command(&self, args: &[&str], env: &[(&str, &str)], snapshot: bool) -> Command {
        self.command_with(
            Path::new(env!("CARGO_BIN_EXE_bazel-rs")),
            args,
            env,
            snapshot,
        )
    }

    fn command_with(
        &self,
        binary: &Path,
        args: &[&str],
        env: &[(&str, &str)],
        snapshot: bool,
    ) -> Command {
        let home = self.run_root().join("home");
        let mut command = Command::new("timeout");
        command
            .args(["--kill-after=5s", "180s"])
            .arg(binary)
            .arg("--ignore_all_rc_files")
            .arg(format!("--cache-dir={}", self.cache().display()))
            .args(args)
            .current_dir(self.ws())
            .env_clear()
            .env("PATH", "/usr/bin:/bin")
            .env("HOME", &home)
            .env("TMPDIR", &home)
            .env("LC_ALL", "C");
        if snapshot {
            command
                .env("BAZEL_RS_SNAPSHOT", self.mode)
                .env("BAZEL_RS_SNAPSHOT_EXPLAIN", "1");
        }
        command.envs(env.iter().copied());
        command
    }

    fn run_side(&self, side: &str, args: &[&str], env: &[(&str, &str)]) -> Output {
        self.with_side(side, || {
            let output = self
                .command(args, env, side == ON)
                .output()
                .expect("bazel-rs runs");
            assert_ne!(output.status.code(), Some(124), "CLI timed out");
            output
        })
    }

    /// One step: the command with the snapshot on, then off.
    fn run(&self, args: &[&str], env: &[(&str, &str)]) -> Pair {
        let on = self.run_side(ON, args, env);
        let off = self.run_side(OFF, args, env);
        Pair {
            on,
            off,
            mode: self.mode,
        }
    }

    /// [`Twin::run`], asserting identical output.
    fn same(&self, args: &[&str], env: &[(&str, &str)]) -> Pair {
        let pair = self.run(args, env);
        pair.assert_same(args);
        pair
    }

    /// Run `args` until a run saves (at most twice: a cold run that executes
    /// a repository rule is not saved).
    #[track_caller]
    fn save(&self, args: &[&str], env: &[(&str, &str)]) {
        if self.same(args, env).line("saved ").is_none() {
            self.same(args, env).assert_saved();
        }
    }

    /// The snapshot files of the `on` copy.
    fn snapshots(&self) -> Vec<PathBuf> {
        let directory = self.side(ON).join("cache/snapshots/v1");
        let mut files: Vec<PathBuf> = fs::read_dir(directory)
            .map(|entries| {
                entries
                    .flatten()
                    .map(|entry| entry.path())
                    .filter(|path| path.extension().is_some_and(|ext| ext == "snap"))
                    .collect()
            })
            .unwrap_or_default();
        files.sort();
        files
    }
}

fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

/// stderr without the snapshot diagnostics and PERF lines, test durations
/// normalized.
fn program_stderr(output: &Output) -> String {
    let stderr = text(&output.stderr);
    let mut kept = String::new();
    for line in stderr.split_inclusive('\n') {
        if line.starts_with("bazel-rs-snapshot: ") || line.starts_with("PERF") {
            continue;
        }
        kept.push_str(line);
    }
    without_durations(&kept)
}

/// ` in <digits>.<digits>s` -> ` in <T>s`.
fn without_durations(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(at) = rest.find(" in ") {
        out.push_str(&rest[..at + 4]);
        rest = &rest[at + 4..];
        let digits = rest
            .find(|c: char| !(c.is_ascii_digit() || c == '.'))
            .unwrap_or(rest.len());
        if digits > 0 && rest[..digits].contains('.') && rest[digits..].starts_with('s') {
            out.push_str("<T>");
            rest = &rest[digits..];
        }
    }
    out.push_str(rest);
    out
}

impl Pair {
    fn assert_same(&self, args: &[&str]) {
        assert_eq!(
            (
                self.on.status.code(),
                without_durations(&text(&self.on.stdout)),
                program_stderr(&self.on)
            ),
            (
                self.off.status.code(),
                without_durations(&text(&self.off.stdout)),
                program_stderr(&self.off)
            ),
            "{args:?}: snapshot on (left) and off (right) differ; diagnostics:\n{}",
            self.explain().join("\n")
        );
    }

    /// The `on` run's snapshot diagnostics (without the prefix).
    fn explain(&self) -> Vec<String> {
        text(&self.on.stderr)
            .lines()
            .filter_map(|line| line.strip_prefix("bazel-rs-snapshot: "))
            .map(str::to_owned)
            .collect()
    }

    fn line(&self, prefix: &str) -> Option<String> {
        self.explain()
            .into_iter()
            .find(|line| line.starts_with(prefix))
    }

    /// A hit (in verify mode: verified).
    #[track_caller]
    fn assert_hit(&self) -> &Self {
        assert!(
            self.line("hit ").is_some(),
            "expected a hit:\n{}\n--- stderr:\n{}",
            self.explain().join("\n"),
            text(&self.on.stderr)
        );
        if self.mode == VERIFY {
            assert!(
                self.line("verified ").is_some(),
                "expected the hit verified:\n{}\n--- stderr:\n{}",
                self.explain().join("\n"),
                text(&self.on.stderr)
            );
        }
        self
    }

    /// A miss whose reason contains `reason`.
    #[track_caller]
    fn assert_miss(&self, reason: &str) -> &Self {
        let line = self.line("miss ").unwrap_or_else(|| {
            panic!("expected a miss ({reason}):\n{}", self.explain().join("\n"))
        });
        assert!(line.contains(reason), "expected a miss ({reason}): {line}");
        self
    }

    /// Verify mode: the hit's shadow full analysis disagreed (exit 70,
    /// nothing executed, the snapshot removed), one difference naming
    /// `reason`; the snapshot-less run succeeded.
    #[track_caller]
    fn assert_verify_mismatch(&self, reason: &str) -> &Self {
        let explain = self.explain();
        assert_eq!(
            self.on.status.code(),
            Some(70),
            "expected a verify mismatch ({reason}):\n{}",
            explain.join("\n")
        );
        assert!(
            explain
                .iter()
                .any(|line| line.starts_with("verify ") && line.contains("MISMATCH")),
            "{}",
            explain.join("\n")
        );
        assert!(
            explain
                .iter()
                .any(|line| line.starts_with("verify ") && line.contains(reason)),
            "expected a verify mismatch ({reason}):\n{}",
            explain.join("\n")
        );
        assert!(self.off.status.success(), "{}", text(&self.off.stderr));
        self
    }

    #[track_caller]
    fn assert_saved(&self) -> &Self {
        assert!(
            self.line("saved ").is_some(),
            "expected a save:\n{}\n--- stderr:\n{}",
            self.explain().join("\n"),
            text(&self.on.stderr)
        );
        self
    }

    /// Not saved, for a reason containing `reason`.
    #[track_caller]
    fn assert_not_saved(&self, reason: &str) -> &Self {
        let line = self.line("not saved").unwrap_or_else(|| {
            panic!(
                "expected no save ({reason}):\n{}",
                self.explain().join("\n")
            )
        });
        assert!(line.contains(reason), "expected no save ({reason}): {line}");
        self
    }

    #[track_caller]
    fn assert_success(&self) -> &Self {
        assert!(
            self.on.status.success(),
            "exit {:?}:\n{}",
            self.on.status.code(),
            text(&self.on.stderr)
        );
        self
    }

    fn stderr(&self) -> String {
        program_stderr(&self.on)
    }
}

const RULES: &str = r#"print("rules.bzl loaded")

def _exe(ctx):
    out = ctx.actions.declare_file(ctx.label.name + ".sh")
    ctx.actions.write(out, ctx.attr.script, is_executable = True)
    return [
        DefaultInfo(executable = out, runfiles = ctx.runfiles(files = ctx.files.data)),
        RunEnvironmentInfo(environment = ctx.attr.env),
    ]

_ATTRS = {
    "script": attr.string(),
    "data": attr.label_list(allow_files = True),
    "env": attr.string_dict(),
}

script_test = rule(implementation = _exe, test = True, attrs = _ATTRS)
script_binary = rule(implementation = _exe, executable = True, attrs = _ATTRS)

def _stamped(ctx):
    out = ctx.actions.declare_file(ctx.label.name + ".txt")
    ctx.actions.run_shell(
        outputs = [out],
        inputs = [ctx.info_file, ctx.version_file],
        command = "grep -h '^STABLE_W5\\|^W5_' {} {} | sort > {}".format(ctx.info_file.path, ctx.version_file.path, out.path),
    )
    return [DefaultInfo(files = depset([out]))]

stamped = rule(implementation = _stamped)

def _env(ctx):
    out = ctx.actions.declare_file(ctx.label.name + ".txt")
    ctx.actions.run_shell(
        outputs = [out],
        command = "echo \"$W5_INHERITED\" > " + out.path,
        use_default_shell_env = True,
    )
    return [DefaultInfo(files = depset([out]))]

env_echo = rule(implementation = _env)

def warn_here(name):
    print("macro for " + name)
"#;

const REPOS: &str = r#"def _local(ctx):
    ctx.file("BUILD.bazel", "exports_files(['value'])\n")
    ctx.file("value", "value=%s\n" % ctx.getenv("W5_LOCAL", "unset"))

local_rule = repository_rule(implementation = _local, local = True)

def _gen(ctx):
    ctx.file("BUILD.bazel", "exports_files(['data.txt'])\n")
    ctx.file("data.txt", "generated %s\n" % ctx.attr.flavor)

gen_rule = repository_rule(implementation = _gen, attrs = {"flavor": attr.string(default = "plain")})
"#;

const MODULE: &str = r#"module(name = "snapshot_w5")

lr = use_repo_rule("//:repos.bzl", "local_rule")
lr(name = "localrepo")

gr = use_repo_rule("//:repos.bzl", "gen_rule")
gr(name = "genrepo")
"#;

const ROOT_BUILD: &str = r##"load("//:rules.bzl", "env_echo", "script_binary", "script_test", "stamped", "warn_here")

warn_here("root")

genrule(
    name = "all_inputs",
    srcs = ["//pkg:in.txt", "@localrepo//:value", "@genrepo//:data.txt"] + glob(["data/*.txt"]),
    outs = ["all.txt"],
    cmd = "cat $(SRCS) > $@",
)

script_test(name = "pass_test", size = "small", data = [":all_inputs"], script = "#!/bin/bash\necho test-ran\ncat all.txt\n")

script_binary(name = "tool", data = [":all_inputs"], script = "#!/bin/bash\necho tool-ran \"$@\"\n")

stamped(name = "stamped")

env_echo(name = "env_echo")
"##;

/// The fixture: a root package globbing `data/*.txt`, a `pkg` package, a
/// local repository rule (replayed on a hit), a generated repository (a
/// verified store tree on warm runs), test and binary rules, a status
/// consumer, an inherited-environment action and prints in a `.bzl` file
/// and a macro.
fn fixture() -> Twin {
    Twin::new(&[
        ("MODULE.bazel", MODULE),
        ("repos.bzl", REPOS),
        ("rules.bzl", RULES),
        ("BUILD.bazel", ROOT_BUILD),
        ("data/a.txt", "a\n"),
        ("pkg/BUILD.bazel", "exports_files(['in.txt'])\n"),
        ("pkg/in.txt", "input one\n"),
        (
            "other/BUILD.bazel",
            "genrule(name = 'o', outs = ['o.txt'], cmd = 'echo o > $@')\n",
        ),
        ("other/main.go", "package other\n"),
    ])
}

const BUILD_ALL: &[&str] = &["build", "//:all_inputs"];

/// A fixture whose snapshot of `args` was just saved (the cold run executes
/// the generated repository: not saved; the warm one saves).
fn warmed(args: &[&str], env: &[(&str, &str)]) -> Twin {
    let twin = fixture();
    let cold = twin.same(args, env);
    cold.assert_not_saved("repository-rule-executed");
    twin.same(args, env).assert_saved();
    twin
}

/// Fixture 1: a no-op build hits, and replays what planning printed (the
/// `.bzl` file's top-level print and the macro's).
#[test]
fn a_no_op_build_hits_and_replays_what_planning_printed() {
    let twin = warmed(BUILD_ALL, &[]);
    for _ in 0..2 {
        let hit = twin.same(BUILD_ALL, &[]);
        hit.assert_hit().assert_success();
        let stderr = hit.stderr();
        assert!(stderr.contains("rules.bzl loaded"), "{stderr}");
        assert!(stderr.contains("macro for root"), "{stderr}");
        assert!(
            stderr.contains("0 actions executed, 1 cache hits"),
            "{stderr}"
        );
        // A hit saves nothing new.
        assert!(hit.line("saved").is_none());
    }
    assert_eq!(twin.snapshots().len(), 1);
}

/// Fixture 2: editing a source file's contents (same size, at once) is
/// still a hit: only the action that reads it runs again.
#[test]
fn a_source_edit_hits_and_reruns_the_affected_action() {
    let twin = warmed(BUILD_ALL, &[]);
    twin.same(BUILD_ALL, &[]).assert_hit();
    twin.write("pkg/in.txt", "input two\n");
    let edited = twin.same(BUILD_ALL, &[]);
    edited.assert_hit();
    assert!(
        edited.stderr().contains("1 actions executed, 0 cache hits"),
        "{}",
        edited.stderr()
    );
    twin.same(BUILD_ALL, &[]).assert_hit();
}

/// Fixture 3: a BUILD edit misses, saves, then hits; a `.bzl` edit misses.
#[test]
fn build_and_bzl_edits_miss_then_hit_again() {
    let twin = warmed(BUILD_ALL, &[]);
    twin.write(
        "BUILD.bazel",
        &format!(
            "{ROOT_BUILD}\ngenrule(name = 'extra', outs = ['extra.txt'], cmd = 'echo > $@')\n"
        ),
    );
    twin.same(BUILD_ALL, &[])
        .assert_miss("BUILD.bazel")
        .assert_saved();
    twin.same(BUILD_ALL, &[]).assert_hit();
    twin.write("rules.bzl", &format!("{RULES}\n# edited\n"));
    twin.same(BUILD_ALL, &[])
        .assert_miss("rules.bzl")
        .assert_saved();
    twin.same(BUILD_ALL, &[]).assert_hit();
}

/// Fixture 4: a new file matching a glob of a globbed directory misses; an
/// editor's swap file there (no glob matches it) still hits.
#[test]
fn glob_listings_see_matching_files_only() {
    let twin = warmed(BUILD_ALL, &[]);
    twin.write("data/.a.txt.swp", "swap");
    twin.same(BUILD_ALL, &[]).assert_hit();
    twin.write("data/b.txt", "b\n");
    let miss = twin.same(BUILD_ALL, &[]);
    miss.assert_miss("data");
    assert!(
        miss.stderr().contains("1 actions executed"),
        "{}",
        miss.stderr()
    );
    twin.same(BUILD_ALL, &[]).assert_hit();
}

/// Fixture 5: under `//...`, a new `.go` file in a package without globs
/// still hits; a new subdirectory with a BUILD file misses.
#[test]
fn recursive_patterns_see_new_packages_only() {
    let twin = warmed(&["build", "--nobuild", "//..."], &[]);
    let args = &["build", "--nobuild", "//..."];
    twin.same(args, &[]).assert_hit();
    twin.write("other/extra.go", "package other\n");
    twin.same(args, &[]).assert_hit();
    twin.write(
        "other/sub/BUILD.bazel",
        "genrule(name = 's', outs = ['s.txt'], cmd = 'echo > $@')\n",
    );
    twin.same(args, &[]).assert_miss("other").assert_saved();
    twin.same(args, &[]).assert_hit();
}

/// Fixture 6: an environment variable a local repository rule reads misses
/// (its replay differs); an inherited `--action_env` value is re-resolved
/// on a hit, and the action that uses it runs again.
#[test]
fn environment_changes_miss_or_rekey() {
    let args = &[
        "build",
        "--action_env=W5_INHERITED",
        "//:all_inputs",
        "//:env_echo",
    ];
    let one = [("W5_INHERITED", "one"), ("W5_LOCAL", "a")];
    let twin = warmed(args, &one);
    twin.same(args, &one).assert_hit();
    let two = [("W5_INHERITED", "two"), ("W5_LOCAL", "a")];
    let rekeyed = twin.same(args, &two);
    rekeyed.assert_hit();
    // Both actions use the default shell environment.
    assert!(
        rekeyed
            .stderr()
            .contains("2 actions executed, 0 cache hits"),
        "{}",
        rekeyed.stderr()
    );
    let local = [("W5_INHERITED", "two"), ("W5_LOCAL", "b")];
    // The variable is an input the window recorded (and the local rule's
    // replay would differ too).
    let miss = twin.same(args, &local);
    // EXPLAIN names the first failing check and the local repository
    // whose replay differs too.
    miss.assert_miss("ENV W5_LOCAL: value changed; and local repository +local_rule+localrepo: the identity inputs differ");
    miss.assert_saved();
    twin.same(args, &local).assert_hit();
}

/// Fixture 7: another option is another key (a miss); switching back hits.
#[test]
fn options_select_their_own_snapshot() {
    let twin = warmed(BUILD_ALL, &[]);
    let defined = &["build", "--define=w5=1", "//:all_inputs"];
    twin.same(defined, &[])
        .assert_miss("no snapshot")
        .assert_saved();
    twin.same(defined, &[]).assert_hit();
    twin.same(BUILD_ALL, &[]).assert_hit();
    // Execution-only options keep the key.
    twin.same(
        &["build", "--jobs=3", "--show_result=5", "//:all_inputs"],
        &[],
    )
    .assert_hit();
    assert_eq!(twin.snapshots().len(), 2);
}

/// Fixture 8: a same-size edit right after the recording is caught by the
/// content check of a racy read; a same-size edit that restores the old
/// mtime (`touch -d`) of a settled file is caught through its ctime.
#[test]
fn racy_and_mtime_restoring_edits_are_detected() {
    let twin = warmed(BUILD_ALL, &[]);
    let edited = ROOT_BUILD.replace("warn_here(\"root\")", "warn_here(\"toor\")");
    assert_eq!(edited.len(), ROOT_BUILD.len());
    twin.write("BUILD.bazel", &edited);
    twin.same(BUILD_ALL, &[])
        .assert_miss("BUILD.bazel: content changed")
        .assert_saved();
    // Settle every input, record again (no racy key), then edit in place
    // and restore the modification time.
    std::thread::sleep(std::time::Duration::from_millis(2200));
    twin.write("pkg/BUILD.bazel", "exports_files(['in.txt'])\n# x\n");
    std::thread::sleep(std::time::Duration::from_millis(2200));
    twin.same(BUILD_ALL, &[])
        .assert_miss("pkg/BUILD.bazel")
        .assert_saved();
    twin.same(BUILD_ALL, &[]).assert_hit();
    twin.each(|root| {
        let path = root.join("ws/pkg/BUILD.bazel");
        let modified = fs::metadata(&path).unwrap().modified().unwrap();
        fs::write(&path, "exports_files(['in.txt'])\n# y\n").unwrap();
        fs::File::options()
            .write(true)
            .open(&path)
            .unwrap()
            .set_modified(modified)
            .unwrap();
    });
    twin.same(BUILD_ALL, &[])
        .assert_miss("pkg/BUILD.bazel: content changed")
        .assert_saved();
}

const EXTENSION: &str = r#"def _repo(ctx):
    ctx.file("BUILD.bazel", "exports_files(['v.txt'])\n")
    ctx.file("v.txt", "from the extension\n")

_repo_rule = repository_rule(implementation = _repo)

def _ext(module_ctx):
    _repo_rule(name = "extrepo")

ext = module_extension(implementation = _ext)
"#;

/// Fixture 9: a run that writes the lockfile is not saved; a lockfile edit
/// and a MODULE.bazel edit miss.
#[test]
fn lockfile_and_module_edits_miss_and_lockfile_writes_do_not_save() {
    let twin = fixture();
    twin.write("ext.bzl", EXTENSION);
    twin.write(
        "MODULE.bazel",
        &format!("{MODULE}\next = use_extension('//:ext.bzl', 'ext')\nuse_repo(ext, 'extrepo')\n"),
    );
    twin.write(
        "ext/BUILD.bazel",
        "genrule(name = 'e', srcs = ['@extrepo//:v.txt'], outs = ['e.txt'], cmd = 'cp $< $@')\n",
    );
    let args = &["build", "//ext:e"];
    let cold = twin.same(args, &[]);
    cold.assert_success().assert_not_saved("taint");
    assert!(twin.side(ON).join("ws/MODULE.bazel.lock").exists());
    twin.same(args, &[]).assert_saved();
    twin.same(args, &[]).assert_hit();
    // An edited lockfile (still valid JSON) is read again: a miss; the run
    // that rewrites it is not saved.
    twin.each(|root| {
        let path = root.join("ws/MODULE.bazel.lock");
        let text = fs::read_to_string(&path).unwrap();
        fs::write(&path, format!("{text}\n")).unwrap();
    });
    let edited = twin.same(args, &[]);
    edited.assert_miss("MODULE.bazel.lock");
    let rewrote = edited
        .line("not saved")
        .is_some_and(|line| line.contains("lockfile-write-pending"));
    if rewrote {
        twin.same(args, &[]).assert_saved();
    } else {
        edited.assert_saved();
    }
    twin.same(args, &[]).assert_hit();
    twin.write(
        "MODULE.bazel",
        &format!(
            "{MODULE}\next = use_extension('//:ext.bzl', 'ext')\nuse_repo(ext, 'extrepo')\n# edited\n"
        ),
    );
    twin.same(args, &[]).assert_miss("MODULE.bazel");
}

/// The directory of the generated repository's published tree.
fn generated_tree(root: &Path) -> PathBuf {
    fs::read_dir(root.join("cache/repositories/direct/trees"))
        .unwrap()
        .flatten()
        .map(|entry| entry.path())
        .find(|path| path.join("sub/data.txt").exists())
        .expect("the generated repository's tree")
}

const NESTED_REPOS: &str = r#"def _local(ctx):
    ctx.file("BUILD.bazel", "exports_files(['value'])\n")
    ctx.file("value", "value\n")

local_rule = repository_rule(implementation = _local, local = True)

def _gen(ctx):
    ctx.file("BUILD.bazel", "exports_files(['sub/data.txt'])\n")
    ctx.file("sub/data.txt", "generated\n")

gen_rule = repository_rule(implementation = _gen)
"#;

/// Fixture 10: a file deleted from a repository tree. Per tree (the
/// default) the tree's root and manifest are unchanged: a hit whose action
/// fails on the missing input removes the snapshot with a one-line hint;
/// the next run repairs the tree, then saves and hits. With full
/// validation (`BAZEL_RS_VERIFY_REPOS=full`) it is a miss at once.
#[test]
fn a_file_deleted_from_a_repository_tree_heals() {
    let build = ROOT_BUILD.replace("@genrepo//:data.txt", "@genrepo//:sub/data.txt");
    for full in [false, true] {
        let twin = fixture();
        twin.write("repos.bzl", NESTED_REPOS);
        twin.write("BUILD.bazel", &build);
        let env: &[(&str, &str)] = if full {
            &[("BAZEL_RS_VERIFY_REPOS", "full")]
        } else {
            &[]
        };
        twin.same(BUILD_ALL, env)
            .assert_not_saved("repository-rule-executed");
        // A racy tree head (published, or its manifest refreshed, less than
        // two seconds before the recording) is verified against its manifest
        // on every hit, which would see the deletion at once: let the hit
        // settle the keys first (it rewrites the snapshot).
        std::thread::sleep(std::time::Duration::from_millis(2200));
        twin.same(BUILD_ALL, env).assert_saved();
        std::thread::sleep(std::time::Duration::from_millis(2200));
        twin.same(BUILD_ALL, env).assert_hit();
        // Deleted (the action's cached output stays valid by its digest:
        // edit the workspace input too, so the action must run).
        twin.write("pkg/in.txt", "input changed\n");
        twin.each(|root| {
            let file = generated_tree(root).join("sub/data.txt");
            let directory = file.parent().unwrap().to_path_buf();
            fs::set_permissions(&directory, fs::Permissions::from_mode(0o755)).unwrap();
            fs::remove_file(file).unwrap();
        });
        if full {
            let miss = twin.same(BUILD_ALL, env);
            miss.assert_miss("sub/data.txt");
            twin.same(BUILD_ALL, env).assert_saved();
            twin.same(BUILD_ALL, env).assert_hit();
            continue;
        }
        let stale = twin.run(BUILD_ALL, env);
        if twin.mode == VERIFY {
            // Verify mode compares the hit with a full analysis, which
            // repairs the tree (it executes the rule again): the documented
            // per-tree trade-off is a mismatch, and the snapshot goes.
            assert!(stale.line("hit ").is_some(), "{:?}", stale.explain());
            stale.assert_verify_mismatch("repository-rule-executed");
            assert!(twin.snapshots().is_empty());
            // Nothing executed on the snapshot side (the other side ran the
            // edited input's action): it catches up, then both agree.
            let next = twin.run(BUILD_ALL, env);
            next.assert_success()
                .assert_miss("no snapshot")
                .assert_saved();
            assert!(
                next.stderr().contains("1 actions executed"),
                "{}",
                next.stderr()
            );
            twin.same(BUILD_ALL, env).assert_hit();
            continue;
        }
        stale.assert_hit();
        assert!(!stale.on.status.success());
        let stderr = text(&stale.on.stderr);
        assert!(
            stderr.contains("bazel-rs: removed the analysis snapshot of this build"),
            "{stderr}"
        );
        assert!(stale.off.status.success(), "{}", text(&stale.off.stderr));
        assert!(twin.snapshots().is_empty());
        // The next run repairs the tree like the full analysis that ran
        // instead of the stale hit.
        let repaired = Pair {
            on: twin.run_side(ON, BUILD_ALL, env),
            off: stale.off,
            mode: twin.mode,
        };
        repaired.assert_same(BUILD_ALL);
        repaired.assert_success().assert_miss("no snapshot");
        twin.same(BUILD_ALL, env).assert_saved();
        twin.same(BUILD_ALL, env).assert_hit();
    }
}

/// A hit skips repository verification, so the execution's digest of a
/// repository tree file must not trust a stat key that hides an in-place
/// edit: crate::digest_memo keys the files of the tree stores by their
/// change time (an edit that keeps the size and restores the modification
/// time still moves it). The hit's action reads the edited bytes and runs
/// again — the per-tree validation's documented trade-off (PLAN-L2 update 1,
/// decision (1)): the full analysis instead repairs the tree, and its action
/// is a cache hit.
#[test]
fn a_hit_hashes_an_edited_tree_file_by_its_change_time() {
    let build = ROOT_BUILD.replace("@genrepo//:data.txt", "@genrepo//:sub/data.txt");
    let twin = fixture();
    twin.write("repos.bzl", NESTED_REPOS);
    twin.write("BUILD.bazel", &build);
    twin.same(BUILD_ALL, &[])
        .assert_not_saved("repository-rule-executed");
    std::thread::sleep(std::time::Duration::from_millis(2200));
    twin.same(BUILD_ALL, &[]).assert_saved();
    std::thread::sleep(std::time::Duration::from_millis(2200));
    twin.same(BUILD_ALL, &[]).assert_hit();
    twin.each(|root| {
        let file = generated_tree(root).join("sub/data.txt");
        let metadata = fs::metadata(&file).unwrap();
        let mode = metadata.permissions().mode();
        let modified = metadata.modified().unwrap();
        fs::set_permissions(&file, fs::Permissions::from_mode(0o644)).unwrap();
        fs::write(&file, "GENERATED\n").unwrap();
        let handle = fs::File::options().write(true).open(&file).unwrap();
        handle.set_modified(modified).unwrap();
        drop(handle);
        fs::set_permissions(&file, fs::Permissions::from_mode(mode)).unwrap();
        assert_eq!(fs::metadata(&file).unwrap().len(), metadata.len());
    });
    let edited = twin.run(BUILD_ALL, &[]);
    if twin.mode == VERIFY {
        // The full analysis repairs the tree (the rule runs again): verify
        // mode reports the documented trade-off as a mismatch.
        assert!(edited.line("hit ").is_some(), "{:?}", edited.explain());
        edited.assert_verify_mismatch("repository-rule-executed");
        twin.same(BUILD_ALL, &[])
            .assert_success()
            .assert_miss("no snapshot");
        return;
    }
    edited.assert_hit().assert_success();
    assert!(
        edited.stderr().contains("1 actions executed, 0 cache hits"),
        "{}",
        edited.stderr()
    );
    let off = program_stderr(&edited.off);
    assert!(off.contains("0 actions executed, 1 cache hits"), "{off}");
    let output = |pair: &Output| {
        let stdout = text(&pair.stdout);
        let path = stdout
            .lines()
            .next()
            .unwrap()
            .split_once('\t')
            .unwrap()
            .1
            .to_owned();
        fs::read_to_string(path).unwrap_or_default()
    };
    let on_output = twin.with_side(ON, || output(&edited.on));
    let off_output = twin.with_side(OFF, || output(&edited.off));
    assert!(on_output.contains("GENERATED"), "{on_output}");
    assert!(
        off_output.contains("generated plain") || off_output.contains("generated\n"),
        "{off_output}"
    );
}

/// Fixture 11: another bazel-rs binary never reads the snapshot.
#[test]
fn another_binary_misses() {
    let twin = warmed(BUILD_ALL, &[]);
    twin.same(BUILD_ALL, &[]).assert_hit();
    let copy = twin.base.path().join("bazel-rs-other");
    // Written by another process (no descriptor of it here: ETXTBSY), one
    // byte longer: another identity.
    let status = Command::new("/bin/sh")
        .arg("-c")
        .arg(r#"cp "$1" "$2.tmp" && printf x >> "$2.tmp" && mv "$2.tmp" "$2""#)
        .arg("copy")
        .arg(env!("CARGO_BIN_EXE_bazel-rs"))
        .arg(&copy)
        .status()
        .unwrap();
    assert!(status.success());
    let output = twin.with_side(ON, || {
        twin.command_with(&copy, BUILD_ALL, &[], true)
            .output()
            .unwrap()
    });
    let pair = Pair {
        off: twin.run_side(OFF, BUILD_ALL, &[]),
        on: output,
        mode: twin.mode,
    };
    pair.assert_same(BUILD_ALL);
    pair.assert_miss("no snapshot").assert_saved();
    // Both snapshots stay (one per binary identity and key).
    assert_eq!(twin.snapshots().len(), 2);
    twin.same(BUILD_ALL, &[]).assert_hit();
}

/// Fixture 12: a truncated or corrupt snapshot is a miss, never a crash,
/// and the next save replaces it.
#[test]
fn damaged_snapshots_miss_and_are_replaced() {
    let twin = warmed(BUILD_ALL, &[]);
    let file = twin.snapshots().pop().unwrap();
    /// Flip one byte (at `at` from the start, or the PLAN section's last
    /// byte for `None`).
    fn flip(bytes: &[u8], at: Option<usize>) -> Vec<u8> {
        // The third section slot of the header is the PLAN section's: kind,
        // reserved, offset, length (src/snapshot/format.rs).
        let slot = 96 + 2 * 40;
        assert_eq!(
            u32::from_le_bytes(bytes[slot..slot + 4].try_into().unwrap()),
            3
        );
        let plan_end = u64::from_le_bytes(bytes[slot + 8..slot + 16].try_into().unwrap())
            + u64::from_le_bytes(bytes[slot + 16..slot + 24].try_into().unwrap());
        let mut flipped = bytes.to_vec();
        flipped[at.unwrap_or(plan_end as usize - 1)] ^= 0x55;
        flipped
    }
    type Damage = fn(&[u8]) -> Vec<u8>;
    let damages: [Damage; 5] = [
        |bytes| bytes[..bytes.len() / 2].to_vec(),
        |bytes| flip(bytes, None),
        |bytes| flip(bytes, Some(400)),
        |bytes| flip(bytes, Some(60)),
        |_| b"BZRSSNP1 garbage".to_vec(),
    ];
    for damage in damages {
        // The latest save's snapshot (its inputs current), damaged.
        let damaged = damage(&fs::read(&file).unwrap());
        fs::write(&file, &damaged).unwrap();
        twin.same(BUILD_ALL, &[])
            .assert_miss("unreadable")
            .assert_saved();
        twin.same(BUILD_ALL, &[]).assert_hit();
    }
}

/// Fixture 13: two processes with the same key at once: both succeed, the
/// store holds one valid snapshot, and it hits.
#[test]
fn concurrent_saves_of_one_key_leave_a_valid_snapshot() {
    let twin = warmed(BUILD_ALL, &[]);
    fs::remove_file(twin.snapshots().pop().unwrap()).unwrap();
    let off = twin.run_side(OFF, BUILD_ALL, &[]);
    let outputs = twin.with_side(ON, || {
        let children: Vec<_> = (0..2)
            .map(|_| {
                twin.command(BUILD_ALL, &[], true)
                    .stdout(std::process::Stdio::piped())
                    .stderr(std::process::Stdio::piped())
                    .spawn()
                    .unwrap()
            })
            .collect();
        children
            .into_iter()
            .map(|child| child.wait_with_output().unwrap())
            .collect::<Vec<_>>()
    });
    let mut saved = 0;
    for on in outputs {
        let pair = Pair {
            on,
            off: off.clone(),
            mode: twin.mode,
        };
        pair.assert_same(BUILD_ALL);
        // Both miss and save, unless one started after the other saved
        // (a loaded machine): then it hits.
        if pair.line("saved ").is_some() {
            saved += 1;
        } else {
            pair.assert_hit();
        }
    }
    assert!(saved >= 1);
    assert_eq!(twin.snapshots().len(), 1);
    twin.same(BUILD_ALL, &[]).assert_hit();
}

/// Fixture 14: `--nobuild`, `test` and `run --script_path` hit like a build.
#[test]
fn nobuild_test_and_run_hit() {
    let nobuild = &["build", "--nobuild", "//:all_inputs"];
    let twin = warmed(nobuild, &[]);
    twin.same(nobuild, &[]).assert_hit();
    let test = &["test", "//:pass_test"];
    twin.same(test, &[]).assert_saved();
    let hit = twin.same(test, &[]);
    hit.assert_hit().assert_success();
    assert!(
        text(&hit.on.stdout).contains("//:pass_test") && text(&hit.on.stdout).contains("PASSED"),
        "{}",
        text(&hit.on.stdout)
    );
    let script = twin.run_root().join("home/run.sh");
    let script_flag = format!("--script_path={}", script.display());
    let run = &["run", script_flag.as_str(), "//:tool", "--", "a"];
    twin.same(run, &[]).assert_saved();
    twin.same(run, &[]).assert_hit();
    // Each run has its own BUILD_ID.
    let scripts: Vec<String> = [ON, OFF]
        .iter()
        .map(|side| {
            fs::read_to_string(twin.side(side).join("home/run.sh"))
                .unwrap()
                .lines()
                .filter(|line| !line.trim_start().starts_with("BUILD_ID="))
                .collect::<Vec<_>>()
                .join("\n")
        })
        .collect();
    assert_eq!(scripts[0], scripts[1]);
    assert!(scripts[0].contains("tool.sh"), "{}", scripts[0]);
    // Neither `--script_path` nor the arguments are part of the key:
    // running it hits the same snapshot.
    let ran = twin.same(&["run", "//:tool", "--", "c"], &[]);
    ran.assert_hit();
    assert!(
        text(&ran.on.stdout).contains("tool-ran c"),
        "{}",
        text(&ran.on.stdout)
    );
}

/// Fixture 15: a retargeted source symlink misses; so does a
/// `.bazelignore` edit.
#[test]
fn symlink_retargets_and_bazelignore_edits_miss() {
    let twin = fixture();
    twin.write("pkg/other.txt", "other\n");
    twin.write(
        "pkg/BUILD.bazel",
        "exports_files(['in.txt', 'link.txt'])\ngenrule(name = 'l', srcs = ['link.txt'], outs = ['l.txt'], cmd = 'cp $< $@')\n",
    );
    twin.each(|root| std::os::unix::fs::symlink("in.txt", root.join("ws/pkg/link.txt")).unwrap());
    let args = &["build", "//pkg:l", "//..."];
    twin.same(args, &[])
        .assert_not_saved("repository-rule-executed");
    twin.same(args, &[]).assert_saved();
    twin.same(args, &[]).assert_hit();
    twin.each(|root| {
        let link = root.join("ws/pkg/link.txt");
        fs::remove_file(&link).unwrap();
        std::os::unix::fs::symlink("other.txt", &link).unwrap();
    });
    let retargeted = twin.same(args, &[]);
    retargeted.assert_miss("link.txt").assert_saved();
    twin.same(args, &[]).assert_hit();
    twin.write(".bazelignore", "other\n");
    twin.same(args, &[])
        .assert_miss(".bazelignore")
        .assert_saved();
    twin.same(args, &[]).assert_hit();
}

/// Fixture 16: a changed `--workspace_status_command` output still hits,
/// and the status consumer runs again; a status command that writes to
/// stderr keeps the run from being saved.
#[test]
fn workspace_status_changes_hit_and_rerun_the_consumer() {
    let twin = fixture();
    twin.write("stable.txt", "one\n");
    twin.each(|root| {
        write_executable_script(
            &root.join("ws/status.sh"),
            "#!/bin/sh\necho STABLE_W5 $(cat stable.txt)\necho W5_VOLATILE x\n",
            0o755,
        );
        write_executable_script(
            &root.join("ws/noisy.sh"),
            "#!/bin/sh\necho STABLE_W5 noisy\necho 'status: warning' >&2\n",
            0o755,
        );
    });
    let status = format!(
        "--workspace_status_command={}",
        twin.ws().join("status.sh").display()
    );
    let args = &["build", status.as_str(), "//:stamped"];
    let first = twin.same(args, &[]);
    first.assert_success();
    if first.line("saved").is_none() {
        twin.same(args, &[]).assert_saved();
    }
    twin.same(args, &[]).assert_hit();
    twin.write("stable.txt", "two\n");
    let changed = twin.same(args, &[]);
    changed.assert_hit();
    assert!(
        changed.stderr().contains("1 actions executed"),
        "{}",
        changed.stderr()
    );
    let noisy_status = format!(
        "--workspace_status_command={}",
        twin.ws().join("noisy.sh").display()
    );
    let noisy = &["build", noisy_status.as_str(), "//:stamped"];
    let printed = twin.same(noisy, &[]);
    printed
        .assert_success()
        .assert_not_saved("workspace-status-stderr");
    assert!(printed.stderr().contains("status: warning"));
    twin.same(noisy, &[])
        .assert_not_saved("workspace-status-stderr");
}

/// A loopback HTTP server for a registry directory (its own thread; one
/// connection at a time; requests logged).
struct Registry {
    url: String,
    requests: std::sync::Arc<std::sync::atomic::AtomicUsize>,
}

impl Registry {
    fn serve(directory: PathBuf) -> Self {
        use std::io::{BufRead, BufReader, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://127.0.0.1:{}", listener.local_addr().unwrap().port());
        let requests = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let counter = requests.clone();
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { continue };
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                loop {
                    let mut line = String::new();
                    if reader.read_line(&mut line).unwrap_or(0) == 0 {
                        break;
                    }
                    loop {
                        let mut header = String::new();
                        if reader.read_line(&mut header).unwrap_or(0) == 0 || header == "\r\n" {
                            break;
                        }
                    }
                    counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    let path = line.split(' ').nth(1).unwrap_or("/").to_owned();
                    let response = match fs::read(directory.join(path.trim_start_matches('/'))) {
                        Ok(body) => {
                            let mut response = format!(
                                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n",
                                body.len()
                            )
                            .into_bytes();
                            response.extend(body);
                            response
                        }
                        Err(_) => b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\n\r\n".to_vec(),
                    };
                    if stream.write_all(&response).is_err() {
                        break;
                    }
                }
            }
        });
        Self { url, requests }
    }

    fn requests(&self) -> usize {
        self.requests.load(std::sync::atomic::Ordering::SeqCst)
    }
}

/// Fixture 17: runs that fetched from the network (the cold run of a
/// remote registry, every run of `--lockfile_mode=off`) or ran a printing
/// local repository rule are never saved.
#[test]
fn network_and_printing_local_repositories_never_save() {
    let twin = fixture();
    let registry = twin.base.path().join("registry");
    for (path, content) in [
        ("modules/w5dep/metadata.json", r#"{"versions": ["1.0"]}"#),
        (
            "modules/w5dep/1.0/MODULE.bazel",
            "module(name = 'w5dep', version = '1.0')\n",
        ),
        (
            "modules/w5dep/1.0/source.json",
            r#"{"url": "https://example.invalid/w5dep.tar.gz", "integrity": "sha256-47DEQpj8HBSa+/TImW+5JCeuQeRkm5NMpJWZG3hSuFU="}"#,
        ),
    ] {
        let path = registry.join(path);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, content).unwrap();
    }
    let server = Registry::serve(registry);
    twin.write(
        "MODULE.bazel",
        &format!("{MODULE}\nbazel_dep(name = 'w5dep', version = '1.0')\n"),
    );
    let registry_flag = format!("--registry={}/", server.url);
    let update = &["build", registry_flag.as_str(), "//:all_inputs"];
    let cold = twin.same(update, &[]);
    cold.assert_success().assert_not_saved("taint");
    assert!(server.requests() > 0);
    twin.same(update, &[]).assert_saved();
    let before = server.requests();
    twin.same(update, &[]).assert_hit();
    assert_eq!(server.requests(), before, "a hit fetches nothing");
    let off = &[
        "build",
        "--lockfile_mode=off",
        registry_flag.as_str(),
        "//:all_inputs",
    ];
    for _ in 0..2 {
        twin.same(off, &[])
            .assert_success()
            .assert_not_saved("taint network");
    }
    // A local repository rule that prints (v1: no output splicing).
    twin.write(
        "repos.bzl",
        &REPOS.replace(
            "def _local(ctx):\n",
            "def _local(ctx):\n    print(\"local rule printing\")\n",
        ),
    );
    for _ in 0..2 {
        let printed = twin.same(update, &[]);
        printed
            .assert_success()
            .assert_not_saved("local-repository-output");
        assert!(printed.stderr().contains("local rule printing"));
    }
}

/// Fixture 18: failed builds — an action failure, an analysis error — are
/// never saved.
#[test]
fn failed_builds_never_save() {
    let twin = fixture();
    twin.write(
        "fail/BUILD.bazel",
        "genrule(name = 'f', outs = ['f.txt'], cmd = 'echo failing >&2; exit 1')\ngenrule(name = 'broken', srcs = [':missing'], outs = ['b.txt'], cmd = 'touch $@')\n",
    );
    for target in ["//fail:f", "//fail:broken"] {
        for _ in 0..2 {
            let failed = twin.same(&["build", target], &[]);
            assert!(!failed.on.status.success());
            failed.assert_not_saved("");
        }
    }
    assert!(twin.snapshots().is_empty());
}

const HOST_REPOS: &str = r#"def _jdk(ctx):
    ctx.symlink(ctx.attr.host, "jdk")
    ctx.file("BUILD.bazel", "filegroup(name = 'jdk', srcs = glob(['jdk/**']), visibility = ['//visibility:public'])\nexports_files(['java_home', 'swiftc'])\n")
    ctx.file("java_home", "JAVA_HOME=%s\n" % ctx.os.environ.get("JAVA_HOME", ""))
    ctx.file("swiftc", "swiftc=%s\n" % ctx.which("swiftc"))

jdk_rule = repository_rule(
    implementation = _jdk,
    local = True,
    attrs = {"host": attr.string()},
)
"#;

/// PLAN-L2 update 1, row W5: local repository rules shaped like
/// rules_java's `local_jdk` and rules_swift's autoconfiguration. Their
/// replay decides: a changed `JAVA_HOME` or a `swiftc` appearing on `PATH`
/// is a miss that prints nothing of its own (the output is the full
/// analysis's), and a file added to the host directory a glob reads
/// through the repository's link is a miss too.
#[test]
fn local_jdk_shaped_repositories_miss_on_host_changes() {
    let twin = fixture();
    let host = twin.base.path().join("host_jdk");
    fs::create_dir_all(host.join("bin")).unwrap();
    fs::write(host.join("bin/java"), "#!/bin/sh\n").unwrap();
    let tools = twin.base.path().join("tools");
    fs::create_dir_all(&tools).unwrap();
    twin.write("host/repos.bzl", HOST_REPOS);
    twin.write("host/BUILD.bazel", "");
    twin.write(
        "MODULE.bazel",
        &format!(
            "{MODULE}\njdk = use_repo_rule('//host:repos.bzl', 'jdk_rule')\njdk(name = 'local_jdk', host = {:?})\n",
            host.display().to_string()
        ),
    );
    twin.write(
        "jdk/BUILD.bazel",
        "genrule(name = 'j', srcs = ['@local_jdk//:jdk', '@local_jdk//:java_home', '@local_jdk//:swiftc'], outs = ['j.txt'], cmd = 'ls $(SRCS) > $@')\n",
    );
    let args = &["build", "//jdk:j"];
    let java = [("JAVA_HOME", "/opt/jdk-one")];
    twin.save(args, &java);
    twin.same(args, &java).assert_hit();
    let java_two = [("JAVA_HOME", "/opt/jdk-two")];
    let changed = twin.same(args, &java_two);
    changed.assert_miss("local repository");
    changed.assert_saved();
    twin.same(args, &java_two).assert_hit();
    // A swiftc on PATH (the replay's `which` finds it).
    write_executable_script(&tools.join("swiftc"), "#!/bin/sh\n", 0o755);
    let path = format!("{}:/usr/bin:/bin", tools.display());
    let with_swift = [("JAVA_HOME", "/opt/jdk-two"), ("PATH", path.as_str())];
    let swift = twin.same(args, &with_swift);
    swift.assert_miss("local repository").assert_saved();
    twin.same(args, &with_swift).assert_hit();
    // A file added to the host directory a glob lists through the link.
    fs::write(host.join("bin/javac"), "#!/bin/sh\n").unwrap();
    let added = twin.same(args, &with_swift);
    added.assert_miss("jdk").assert_saved();
    twin.same(args, &with_swift).assert_hit();
}

/// PLAN-L2 update 1 §(c): a repository whose BUILD loads a `.bzl` through a
/// symlink into another repository's published store tree hits.
#[test]
fn cross_repository_links_hit() {
    let twin = fixture();
    twin.write(
        "links.bzl",
        r#"def _dep(ctx):
    ctx.file("BUILD.bazel", "genrule(name = 'd', outs = ['d.txt'], cmd = 'echo d > $@', visibility = ['//visibility:public'])\n")
    ctx.file("defs.bzl", "VALUE = 'from-dep'\n")

d = repository_rule(_dep)

def _gen(ctx):
    ctx.symlink(Label("@dep//:defs.bzl"), "linked.bzl")
    ctx.file("BUILD.bazel", "load(':linked.bzl', 'VALUE')\ngenrule(name = 'g', outs = ['g.txt'], cmd = 'echo %s > $@' % VALUE, visibility = ['//visibility:public'])\n")

r = repository_rule(_gen)
"#,
    );
    twin.write(
        "MODULE.bazel",
        &format!(
            "{MODULE}\nr = use_repo_rule('//:links.bzl', 'r')\nd = use_repo_rule('//:links.bzl', 'd')\nd(name = 'dep')\nr(name = 'gen')\n"
        ),
    );
    let args = &["build", "@gen//:g"];
    twin.same(args, &[])
        .assert_not_saved("repository-rule-executed");
    twin.same(args, &[]).assert_saved();
    twin.same(args, &[]).assert_hit();
    twin.same(args, &[]).assert_hit();
}

const STAMP_BUILD: &str = r##"load("//:rules.bzl", "script_binary", "script_test", "stamped")

stamped(name = "stamped")

script_test(name = "stamped_test", size = "small", data = [":stamped"], script = "#!/bin/bash\necho stamped-test-ran\n")

script_binary(name = "stamped_tool", data = [":stamped"], script = "#!/bin/bash\necho stamped-tool-ran\n")
"##;

/// W4 review: a hit whose `--workspace_status_command` now fails lays the
/// failure out exactly as the full analysis does (one failing
/// BazelWorkspaceStatusAction node; the decoded request finalized with the
/// failed status the hit computed) — for `build`, `test` and `run`.
#[test]
fn a_failing_status_command_fails_a_hit_like_the_full_analysis() {
    let twin = fixture();
    twin.write("stable.txt", "one\n");
    twin.write("stamp/BUILD.bazel", STAMP_BUILD);
    twin.each(|root| {
        write_executable_script(
            &root.join("ws/status.sh"),
            "#!/bin/sh\necho STABLE_W5 $(cat stable.txt)\n[ -e failing ] && exit 1\nexit 0\n",
            0o755,
        );
    });
    let status = format!(
        "--workspace_status_command={}",
        twin.ws().join("status.sh").display()
    );
    let status = status.as_str();
    for args in [
        &["build", status, "//stamp:stamped"][..],
        &["test", status, "//stamp:stamped_test"],
        &["run", status, "//stamp:stamped_tool"],
    ] {
        twin.save(args, &[]);
        twin.same(args, &[]).assert_hit().assert_success();
        twin.write("failing", "");
        let failed = twin.same(args, &[]);
        failed.assert_hit();
        assert!(!failed.on.status.success(), "{args:?}");
        assert!(
            failed
                .stderr()
                .contains("Failed to determine workspace status"),
            "{args:?}: {}",
            failed.stderr()
        );
        twin.each(|root| fs::remove_file(root.join("ws/failing")).unwrap());
        twin.same(args, &[]).assert_hit().assert_success();
    }
}

/// W4 review: the analysis warnings printed after planning (a dependency
/// on a deprecated target) are part of the saved plan — the save point lies
/// between the hand-over and the diagnostics being taken — and print on a
/// hit exactly where the full analysis prints them.
#[test]
fn post_planning_warnings_survive_a_hit() {
    let twin = fixture();
    twin.write(
        "dep/BUILD.bazel",
        "genrule(name = 'old', outs = ['old.txt'], cmd = 'echo old > $@', deprecation = 'use //dep:new', visibility = ['//visibility:public'])\n",
    );
    twin.write(
        "use/BUILD.bazel",
        "genrule(name = 'user', srcs = ['//dep:old'], outs = ['user.txt'], cmd = 'cp $< $@')\n",
    );
    let args = &["build", "//use:user"];
    twin.save(args, &[]);
    for _ in 0..2 {
        let hit = twin.same(args, &[]);
        hit.assert_hit().assert_success();
        assert!(
            hit.stderr()
                .contains("depends on deprecated target '//dep:old': use //dep:new"),
            "{}",
            hit.stderr()
        );
    }
}

const CONFLICT_RULES: &str = r##"def _conflict(ctx):
    exe = ctx.actions.declare_file(ctx.label.name)
    ctx.actions.write(exe, "#!/bin/sh\n", is_executable = True)
    d = ctx.files.data[0]
    return [DefaultInfo(executable = exe, runfiles = ctx.runfiles(symlinks = {"dir": d, "dir/x": d}))]

conflict = rule(_conflict, executable = True, attrs = {"data": attr.label_list(allow_files = True)})
"##;

/// W4 review: a plan with analysis errors — here a runfiles conflict under
/// ConflictPolicy.ERROR, which still builds — is never saved.
#[test]
fn plans_with_analysis_errors_are_never_saved() {
    let twin = fixture();
    twin.write("conflict/defs.bzl", CONFLICT_RULES);
    twin.write("conflict/d.txt", "d\n");
    twin.write(
        "conflict/BUILD.bazel",
        "load(':defs.bzl', 'conflict')\nconflict(name = 'c', data = ['d.txt'])\n",
    );
    for _ in 0..2 {
        let built = twin.same(&["build", "//conflict:c"], &[]);
        built.assert_success().assert_not_saved("taint errors");
        assert!(
            built.stderr().contains("ERROR: ") && built.stderr().contains("obscured by"),
            "{}",
            built.stderr()
        );
    }
    assert!(twin.snapshots().is_empty());
}

const MAPPED_RULES: &str = r#"def _basename(f):
    return f.basename

def _mapped(ctx):
    d = ctx.actions.declare_directory(ctx.label.name + "_dir")
    ctx.actions.run_shell(outputs = [d], command = "mkdir -p {d} && echo a > {d}/a.txt && echo b > {d}/b.txt".format(d = d.path))
    out = ctx.actions.declare_file(ctx.label.name + ".list")
    args = ctx.actions.args()
    args.add_all([d], map_each = _basename)
    ctx.actions.run_shell(outputs = [out], inputs = [d], arguments = [args], command = "echo \"$@\" > " + out.path)
    return [DefaultInfo(files = depset([out]))]

mapped = rule(implementation = _mapped)
"#;

/// W4 review: an Args `map_each` over a tree artifact (a Starlark call at
/// execution time) keeps the plan from being saved.
#[test]
fn map_each_plans_are_never_saved() {
    let twin = fixture();
    twin.write("mapped/defs.bzl", MAPPED_RULES);
    twin.write(
        "mapped/BUILD.bazel",
        "load(':defs.bzl', 'mapped')\nmapped(name = 'm')\n",
    );
    for _ in 0..2 {
        twin.same(&["build", "//mapped:m"], &[])
            .assert_success()
            .assert_not_saved("map-each-deferred");
    }
    assert!(twin.snapshots().is_empty());
}

/// L2-W6: `test` writes MODULE.bazel.lock like every command (genuine
/// Bazel 9.3 does; it exited before the write): the run that wrote it is
/// not saved, the next one is.
#[test]
fn a_test_writes_the_lockfile_and_saves_next_time() {
    let twin = fixture();
    twin.write("ext.bzl", EXTENSION);
    twin.write(
        "MODULE.bazel",
        &format!("{MODULE}\next = use_extension('//:ext.bzl', 'ext')\nuse_repo(ext, 'extrepo')\n"),
    );
    twin.write(
        "ext/BUILD.bazel",
        "load('//:rules.bzl', 'script_test')\nscript_test(name = 'e_test', size = 'small', data = ['@extrepo//:v.txt'], script = '#!/bin/bash\\necho e-test-ran\\n')\n",
    );
    let args = &["test", "//ext:e_test"];
    let cold = twin.same(args, &[]);
    cold.assert_success().assert_not_saved("taint");
    assert!(twin.side(ON).join("ws/MODULE.bazel.lock").exists());
    assert!(twin.side(OFF).join("ws/MODULE.bazel.lock").exists());
    twin.same(args, &[]).assert_success().assert_saved();
    twin.same(args, &[]).assert_hit().assert_success();
}

/// Fixture 19 (PLAN-L2 §7, L2-W6): every fixture again with
/// `BAZEL_RS_SNAPSHOT=verify` on the snapshot side. A miss saves as with
/// `on`; every hit also runs the full analysis in its shadow, which must
/// agree with the snapshot (`Pair::assert_hit` wants the `verified` line;
/// a mismatch exits 70, which differs from the snapshot-less run).
mod verify_mode {
    /// Run `fixture` with the verify mode on its twins' snapshot side.
    fn verified(fixture: fn()) {
        super::MODE.with(|mode| mode.set(super::VERIFY));
        fixture();
    }

    #[test]
    fn a_no_op_build_hits_and_replays_what_planning_printed() {
        verified(super::a_no_op_build_hits_and_replays_what_planning_printed);
    }

    #[test]
    fn a_source_edit_hits_and_reruns_the_affected_action() {
        verified(super::a_source_edit_hits_and_reruns_the_affected_action);
    }

    #[test]
    fn build_and_bzl_edits_miss_then_hit_again() {
        verified(super::build_and_bzl_edits_miss_then_hit_again);
    }

    #[test]
    fn glob_listings_see_matching_files_only() {
        verified(super::glob_listings_see_matching_files_only);
    }

    #[test]
    fn recursive_patterns_see_new_packages_only() {
        verified(super::recursive_patterns_see_new_packages_only);
    }

    #[test]
    fn environment_changes_miss_or_rekey() {
        verified(super::environment_changes_miss_or_rekey);
    }

    #[test]
    fn options_select_their_own_snapshot() {
        verified(super::options_select_their_own_snapshot);
    }

    #[test]
    fn racy_and_mtime_restoring_edits_are_detected() {
        verified(super::racy_and_mtime_restoring_edits_are_detected);
    }

    #[test]
    fn lockfile_and_module_edits_miss_and_lockfile_writes_do_not_save() {
        verified(super::lockfile_and_module_edits_miss_and_lockfile_writes_do_not_save);
    }

    #[test]
    fn a_file_deleted_from_a_repository_tree_heals() {
        verified(super::a_file_deleted_from_a_repository_tree_heals);
    }

    #[test]
    fn a_hit_hashes_an_edited_tree_file_by_its_change_time() {
        verified(super::a_hit_hashes_an_edited_tree_file_by_its_change_time);
    }

    #[test]
    fn another_binary_misses() {
        verified(super::another_binary_misses);
    }

    #[test]
    fn damaged_snapshots_miss_and_are_replaced() {
        verified(super::damaged_snapshots_miss_and_are_replaced);
    }

    #[test]
    fn concurrent_saves_of_one_key_leave_a_valid_snapshot() {
        verified(super::concurrent_saves_of_one_key_leave_a_valid_snapshot);
    }

    #[test]
    fn nobuild_test_and_run_hit() {
        verified(super::nobuild_test_and_run_hit);
    }

    #[test]
    fn symlink_retargets_and_bazelignore_edits_miss() {
        verified(super::symlink_retargets_and_bazelignore_edits_miss);
    }

    #[test]
    fn workspace_status_changes_hit_and_rerun_the_consumer() {
        verified(super::workspace_status_changes_hit_and_rerun_the_consumer);
    }

    #[test]
    fn network_and_printing_local_repositories_never_save() {
        verified(super::network_and_printing_local_repositories_never_save);
    }

    #[test]
    fn failed_builds_never_save() {
        verified(super::failed_builds_never_save);
    }

    #[test]
    fn local_jdk_shaped_repositories_miss_on_host_changes() {
        verified(super::local_jdk_shaped_repositories_miss_on_host_changes);
    }

    #[test]
    fn cross_repository_links_hit() {
        verified(super::cross_repository_links_hit);
    }

    #[test]
    fn a_failing_status_command_fails_a_hit_like_the_full_analysis() {
        verified(super::a_failing_status_command_fails_a_hit_like_the_full_analysis);
    }

    #[test]
    fn post_planning_warnings_survive_a_hit() {
        verified(super::post_planning_warnings_survive_a_hit);
    }

    #[test]
    fn plans_with_analysis_errors_are_never_saved() {
        verified(super::plans_with_analysis_errors_are_never_saved);
    }

    #[test]
    fn map_each_plans_are_never_saved() {
        verified(super::map_each_plans_are_never_saved);
    }

    #[test]
    fn a_test_writes_the_lockfile_and_saves_next_time() {
        verified(super::a_test_writes_the_lockfile_and_saves_next_time);
    }
}

include!("support/executable_script.rs");
