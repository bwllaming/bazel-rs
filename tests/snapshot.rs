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
include!("support/explicit_modules.rs");
include!("support/console.rs");

const ON: &str = "on";
const OFF: &str = "off";
const VERIFY: &str = "verify";

thread_local! {
    /// The `BAZEL_RS_SNAPSHOT` value of the snapshot side of this test's
    /// twins (`on`; fixture 19 runs every fixture again with `verify`).
    static MODE: std::cell::Cell<&'static str> = const { std::cell::Cell::new(ON) };
    /// Verify mode with `BAZEL_RS_SNAPSHOT_VERIFY_STRICT=1` (a mismatch
    /// exits 70; the fixtures' default), else the soak default (a mismatch
    /// is logged and the full analysis's plan executes).
    static STRICT: std::cell::Cell<bool> = const { std::cell::Cell::new(true) };
}

/// Two copies of a workspace (with its cache and home) that run under one
/// path: `<base>/run`.
struct Twin {
    base: tempfile::TempDir,
    /// The snapshot side's `BAZEL_RS_SNAPSHOT` ([`MODE`]; the mutation fuzz
    /// switches it per step).
    mode: std::cell::Cell<&'static str>,
    /// [`STRICT`] (the mutation fuzz switches it per step).
    strict: std::cell::Cell<bool>,
    /// Read the workspace `.bazelrc` (`--nosystem_rc --nohome_rc`) instead
    /// of `--ignore_all_rc_files` (the mutation fuzz).
    workspace_rc: std::cell::Cell<bool>,
    /// `BAZEL_RS_SNAPSHOT_EXPLAIN=1` on the snapshot side (else
    /// `BAZEL_RS_TIMING=1`: the outcome from the `PERF-SNAPSHOT` lines; the
    /// mutation fuzz switches it per step, L2-W5g).
    explain: std::cell::Cell<bool>,
    /// The working directory below the workspace (the mutation fuzz's
    /// relative patterns), else the workspace root.
    cwd: std::cell::RefCell<Option<&'static str>>,
    /// The `healed` events seen so far ([`Twin::assert_healed`]).
    heals: std::cell::Cell<usize>,
}

/// The two runs of one step.
struct Pair {
    on: Output,
    off: Output,
    /// The snapshot side's mode.
    mode: &'static str,
}

impl Drop for Twin {
    /// Make every directory writable first: a read-only tree artifact in a
    /// cache would keep the temporary directory from being removed.
    fn drop(&mut self) {
        fn writable(path: &Path) {
            let Ok(metadata) = fs::symlink_metadata(path) else {
                return;
            };
            if metadata.is_dir() {
                let mode = metadata.permissions().mode();
                if mode & 0o700 != 0o700 {
                    let _ = fs::set_permissions(path, fs::Permissions::from_mode(mode | 0o700));
                }
                for entry in fs::read_dir(path).into_iter().flatten().flatten() {
                    writable(&entry.path());
                }
            }
        }
        writable(self.base.path());
    }
}

impl Twin {
    fn new(files: &[(&str, &str)]) -> Self {
        let base = tempfile::Builder::new()
            .prefix("snapshot-w5-")
            .tempdir_in(crate::test_temp::root())
            .unwrap();
        let twin = Self {
            base,
            mode: std::cell::Cell::new(MODE.with(std::cell::Cell::get)),
            strict: std::cell::Cell::new(STRICT.with(std::cell::Cell::get)),
            workspace_rc: std::cell::Cell::new(false),
            explain: std::cell::Cell::new(true),
            cwd: std::cell::RefCell::new(None),
            heals: std::cell::Cell::new(0),
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

    /// Write `content` at `path` below the copies' roots (beside the
    /// workspace: `<run>/<path>` while a copy runs).
    fn write_beside(&self, path: &str, content: &str) {
        self.each(|root| {
            let path = root.join(path);
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
            .args(if self.workspace_rc.get() {
                &["--nosystem_rc", "--nohome_rc"][..]
            } else {
                &["--ignore_all_rc_files"][..]
            })
            .arg(format!("--cache-dir={}", self.cache().display()))
            .args(with_explicit_modules(args))
            .current_dir(match *self.cwd.borrow() {
                Some(directory) => self.ws().join(directory),
                None => self.ws(),
            })
            .env_clear()
            // The native result lines the harness reads (crate::build_results).
            .env("BAZEL_RS_RESULT_LINES", "1")
            .env("PATH", "/usr/bin:/bin")
            .env("HOME", &home)
            .env("TMPDIR", &home)
            .env("LC_ALL", "C");
        // The reference side turns the snapshot off explicitly: unset is
        // `on` (L2-W7, on by default).
        command.env(
            "BAZEL_RS_SNAPSHOT",
            if snapshot { self.mode.get() } else { OFF },
        );
        if snapshot {
            if self.explain.get() {
                command.env("BAZEL_RS_SNAPSHOT_EXPLAIN", "1");
            } else {
                command.env("BAZEL_RS_TIMING", "1");
            }
            if self.mode.get() == VERIFY && self.strict.get() {
                command.env("BAZEL_RS_SNAPSHOT_VERIFY_STRICT", "1");
            }
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
            mode: self.mode.get(),
        }
    }

    /// [`Twin::run`], asserting identical output — no heal line on either
    /// side (L2-W5h: [`program_stderr`] keeps it) — and no new `healed`
    /// event: a step that heals is one that tampered with a tree, which
    /// runs through [`Twin::run`] and [`Twin::assert_healed`].
    fn same(&self, args: &[&str], env: &[(&str, &str)]) -> Pair {
        let pair = self.run(args, env);
        pair.assert_same(args);
        self.assert_healed(0);
        pair
    }

    /// The `healed` events of the snapshot side's event log so far (both
    /// generations).
    fn healed_events(&self) -> Vec<String> {
        let snapshots = self.store(ON);
        ["events.log.1", "events.log"]
            .iter()
            .filter_map(|name| fs::read_to_string(snapshots.join(name)).ok())
            .flat_map(|text| {
                text.lines()
                    .filter(|line| line.contains(" kind=healed "))
                    .map(str::to_owned)
                    .collect::<Vec<_>>()
            })
            .collect()
    }

    /// Exactly `count` `healed` events since the last call (L2-W5h); returns
    /// them.
    #[track_caller]
    fn assert_healed(&self, count: usize) -> Vec<String> {
        let events = self.healed_events();
        let new: Vec<String> = events[self.heals.get().min(events.len())..].to_vec();
        self.heals.set(events.len());
        assert_eq!(new.len(), count, "unexpected heals: {new:#?}");
        new
    }

    /// Run `args` until a run saves (at most twice: a cold run that executes
    /// a repository rule is not saved).
    #[track_caller]
    fn save(&self, args: &[&str], env: &[(&str, &str)]) {
        if self.same(args, env).line("saved ").is_none() {
            self.same(args, env).assert_saved();
        }
    }

    /// After `previous` (a miss): run `args` once more unless `previous`
    /// saved; that run must save.
    #[track_caller]
    fn settle(&self, previous: &Pair, args: &[&str], env: &[(&str, &str)]) {
        if previous.line("saved ").is_none() {
            self.same(args, env).assert_saved();
        }
    }

    /// The snapshot store of the copy `side`: `snapshots/` in its
    /// workspace's state directory (docs/WORKTREES.md).
    fn store(&self, side: &str) -> PathBuf {
        state_directory(&self.side(side).join("cache")).join("snapshots")
    }

    /// The snapshot files of the `on` copy.
    fn snapshots(&self) -> Vec<PathBuf> {
        let directory = self.store(ON).join("v1");
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

/// The per-worktree state directory of the one workspace that used
/// `cache`, `<cache>/worktrees/<name>-<hash>` (docs/WORKTREES.md; a twin's
/// copies both run at `<base>/run/ws`), or where it would be.
fn state_directory(cache: &Path) -> PathBuf {
    let worktrees = cache.join("worktrees");
    let mut states: Vec<PathBuf> = fs::read_dir(&worktrees)
        .into_iter()
        .flatten()
        .flatten()
        .map(|entry| entry.path())
        .collect();
    assert!(states.len() <= 1, "{states:?}");
    states.pop().unwrap_or_else(|| worktrees.join("none"))
}

fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

/// The one line a failed hit prints when it removes its snapshot (the
/// snapshot side's intended difference, crate::snapshot::hit) — only ever
/// on a step that tampered with a tree (L2-W5h: [`program_stderr`] keeps
/// it, so on any other step it is a difference).
const HEAL_LINE: &str = "bazel-rs: removed the analysis snapshot of this build: ";

/// stderr without the snapshot diagnostics and PERF lines, test durations
/// normalized. A heal line stays (L2-W5h: before, it was dropped on every
/// step, so an unexpected heal went unnoticed).
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
    let text = without_elapsed(text);
    let text = text.as_str();
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

/// The snapshots a store keeps at most (crate::snapshot::store::KEEP).
const STORE_KEEP: usize = 64;

const BUILD_ALL: &[&str] = &["build", "//:all_inputs"];

/// A fixture whose snapshot of `args` was just saved (the cold run executes
/// the generated repository, whose warm probe — run again after the
/// publication — reuses it: saved at once) and hit once.
fn warmed(args: &[&str], env: &[(&str, &str)]) -> Twin {
    let twin = fixture();
    let cold = twin.same(args, env);
    cold.assert_saved();
    twin.same(args, env).assert_hit();
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

/// Make `path` writable for its owner without touching it when it already
/// is (a chmod moves the change time, which the per-tree validation sees).
fn writable(path: &Path) {
    let mode = fs::metadata(path).unwrap().permissions().mode();
    if mode & 0o200 == 0 {
        fs::set_permissions(path, fs::Permissions::from_mode(mode | 0o200)).unwrap();
    }
}

/// Fixture 10 (L2-W5f review F3): a file or a directory deleted from a
/// repository tree misses at once — per tree (the default) because the
/// policy keeps the tree's directories, whose lstat keys a deletion
/// changes; with full validation (`BAZEL_RS_VERIFY_REPOS=full`) through
/// the entry itself — and the full analysis repairs the tree, then saves
/// and hits.
#[test]
fn a_file_or_directory_deleted_from_a_repository_tree_misses() {
    let build = ROOT_BUILD.replace("@genrepo//:data.txt", "@genrepo//:sub/data.txt");
    for (full, directory) in [(false, false), (false, true), (true, false)] {
        let twin = fixture();
        twin.write("repos.bzl", NESTED_REPOS);
        twin.write("BUILD.bazel", &build);
        let env: &[(&str, &str)] = if full {
            &[("BAZEL_RS_VERIFY_REPOS", "full")]
        } else {
            &[]
        };
        // The cold run executes the generated repository and saves.
        twin.same(BUILD_ALL, env).assert_saved();
        // A racy tree head or directory (published, or its manifest
        // refreshed, less than two seconds before the recording) is
        // verified against its manifest on every hit, which would see the
        // deletion at once: let the hit settle the keys first (it rewrites
        // the snapshot), so the directory keys alone must catch it.
        std::thread::sleep(std::time::Duration::from_millis(2200));
        twin.same(BUILD_ALL, env).assert_hit();
        std::thread::sleep(std::time::Duration::from_millis(2200));
        twin.same(BUILD_ALL, env).assert_hit();
        // Deleted (the action's cached output stays valid by its digest:
        // edit the workspace input too, so the action must run).
        twin.write("pkg/in.txt", "input changed\n");
        twin.each(|root| {
            let file = generated_tree(root).join("sub/data.txt");
            let sub = file.parent().unwrap().to_path_buf();
            if directory {
                writable(sub.parent().unwrap());
                writable(&sub);
                fs::remove_dir_all(&sub).unwrap();
            } else {
                writable(&sub);
                fs::remove_file(file).unwrap();
            }
        });
        let miss = twin.same(BUILD_ALL, env);
        miss.assert_success();
        if full && !directory {
            miss.assert_miss("sub/data.txt");
        } else {
            miss.assert_miss("INTEGRITY ");
        }
        twin.settle(&miss, BUILD_ALL, env);
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
    twin.same(BUILD_ALL, &[]).assert_saved();
    std::thread::sleep(std::time::Duration::from_millis(2200));
    twin.same(BUILD_ALL, &[]).assert_hit();
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
    if twin.mode.get() == VERIFY {
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
        mode: twin.mode.get(),
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
            mode: twin.mode.get(),
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
    twin.same(args, &[]).assert_saved();
    twin.same(args, &[]).assert_hit();
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

/// Fixture 18 (SURVEY3 S2): a build whose analysis succeeded but an action
/// failed is saved — the next run hits, executes only the failed action
/// again and reports the same failure, exactly as the run without the
/// snapshot does (also under `--keep_going`); an analysis error, or a
/// `--keep_going` plan that skipped one, is never saved.
#[test]
fn action_failures_save_and_analysis_failures_never_do() {
    let twin = fixture();
    twin.write(
        "fail/BUILD.bazel",
        "genrule(name = 'f', outs = ['f.txt'], cmd = 'echo failing >&2; exit 1')\ngenrule(name = 'ok', outs = ['ok.txt'], cmd = 'echo ok > $@')\ngenrule(name = 'broken', srcs = [':missing'], outs = ['b.txt'], cmd = 'touch $@')\n",
    );
    for args in [
        &["build", "//fail:f", "//fail:ok"][..],
        &["build", "--keep_going", "//fail:f", "//fail:ok"],
    ] {
        let failed = twin.same(args, &[]);
        assert!(!failed.on.status.success(), "{args:?}");
        failed.assert_saved();
        for _ in 0..2 {
            let again = twin.same(args, &[]);
            assert!(!again.on.status.success(), "{args:?}");
            again.assert_hit();
            let stderr = again.stderr();
            assert!(stderr.contains("failing"), "{args:?}: {stderr}");
            // Only the failed action runs again (`--keep_going` builds the
            // other one: a cache hit).
            if args.contains(&"--keep_going") {
                assert!(
                    stderr.contains("0 actions executed, 1 cache hits"),
                    "{args:?}: {stderr}"
                );
            }
        }
    }
    let saved = twin.snapshots().len();
    for args in [
        &["build", "//fail:broken"][..],
        &["build", "--keep_going", "//fail:broken", "//fail:f"],
        &["build", "--keep_going", "//fail:broken", "//fail:ok"],
    ] {
        for _ in 0..2 {
            let failed = twin.same(args, &[]);
            assert!(!failed.on.status.success(), "{args:?}");
            failed.assert_not_saved("");
        }
    }
    assert_eq!(twin.snapshots().len(), saved);
    // Fixed: the saved plan misses on the BUILD edit and saves again.
    twin.write(
        "fail/BUILD.bazel",
        "genrule(name = 'f', outs = ['f.txt'], cmd = 'echo fixed > $@')\ngenrule(name = 'ok', outs = ['ok.txt'], cmd = 'echo ok > $@')\n",
    );
    let fixed = twin.same(&["build", "//fail:f", "//fail:ok"], &[]);
    fixed
        .assert_success()
        .assert_miss("BUILD.bazel")
        .assert_saved();
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
    twin.same(args, &[]).assert_saved();
    twin.same(args, &[]).assert_hit();
    twin.same(args, &[]).assert_hit();
    twin.same(args, &[]).assert_hit();
}

/// A non-local repository rule executed by the full analysis (cold, or
/// again because a file it watches changed — here the workspace's
/// MODULE.bazel.lock-like `watched.txt`, as aspect_tools_telemetry reads
/// MODULE.bazel.lock) no longer keeps the run from being saved when its
/// warm probe, run again after the publication, reuses what it published:
/// the next run hits instead of analysing once more. One that printed
/// (here: only when the watched file says so) is not saved — the next
/// invocation would reuse the publication and print nothing — and the run
/// after it, reusing the repository, saves.
#[test]
fn an_executed_repository_rule_saves_unless_it_printed() {
    let twin = fixture();
    twin.write(
        "watch/repos.bzl",
        r#"def _watching(ctx):
    content = ctx.read(Label("//watch:watched.txt"))
    if "print" in content:
        print("watched says print")
    ctx.file("BUILD.bazel", "exports_files(['v.txt'])\n")
    ctx.file("v.txt", content)

watching_rule = repository_rule(implementation = _watching)
"#,
    );
    twin.write("watch/watched.txt", "first\n");
    twin.write(
        "watch/BUILD.bazel",
        "exports_files(['watched.txt'])\ngenrule(name = 'w', srcs = ['@watching//:v.txt'], outs = ['w.txt'], cmd = 'cp $< $@')\n",
    );
    twin.write(
        "MODULE.bazel",
        &format!(
            "{MODULE}\nwr = use_repo_rule('//watch:repos.bzl', 'watching_rule')\nwr(name = 'watching')\n"
        ),
    );
    let args = &["build", "//watch:w"];
    twin.same(args, &[]).assert_saved();
    twin.same(args, &[]).assert_hit();
    twin.write("watch/watched.txt", "second\n");
    let edited = twin.same(args, &[]);
    edited.assert_miss("watched.txt").assert_saved();
    assert_eq!(built_output(&twin, ON, &edited.on), "second\n");
    twin.same(args, &[]).assert_hit();
    twin.write("watch/watched.txt", "print\n");
    let printed = twin.same(args, &[]);
    printed
        .assert_miss("watched.txt")
        .assert_not_saved("printed");
    assert!(
        text(&printed.on.stderr).contains("watched says print"),
        "{}",
        text(&printed.on.stderr)
    );
    let reused = twin.same(args, &[]);
    reused.assert_miss("specs/").assert_saved();
    assert!(
        !text(&reused.on.stderr).contains("watched says print"),
        "{}",
        text(&reused.on.stderr)
    );
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
        let stderr = hit.stderr();
        let warning = "depends on deprecated target '//dep:old': use //dep:new";
        assert!(stderr.contains(warning), "{stderr}");
        // Once, before `INFO: Analyzed …`, like Bazel's analysis warnings.
        assert_eq!(stderr.matches(warning).count(), 1, "{stderr}");
        assert!(
            stderr.find(warning) < stderr.find("INFO: Analyzed "),
            "{stderr}"
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

const MAPPED_RULES: &str = r#"load(":consts.bzl", "SUFFIX")

def _basename(f):
    return f.basename

_TPL = "%s:%s:%s"

def _shouter(prefix):
    return lambda f: prefix + "-" + f.basename

def _tree(ctx):
    d = ctx.actions.declare_directory(ctx.label.name + "_dir")
    names = ctx.file.names
    ctx.actions.run_shell(
        outputs = [d],
        inputs = [names],
        command = "mkdir -p {d} && for n in $(cat {names}); do echo $n > {d}/$n; done".format(d = d.path, names = names.path),
    )
    return d

def _mapped(ctx):
    d = _tree(ctx)
    prefix = ctx.attr.prefix
    table = {"k": [1, (True, None)], "f": ctx.file.names, "n": -3}
    shout = _shouter(prefix.upper())

    def entry(f, expander, sep = "/"):
        if f.is_directory:
            return [_TPL % (prefix, x.basename, SUFFIX) + sep + str(table["k"]) + table["f"].basename + str(table["n"]) + shout(x) for x in expander.expand(f)]
        return f.path

    written = ctx.actions.declare_file(ctx.label.name + ".written")
    wargs = ctx.actions.args()
    wargs.set_param_file_format("multiline")
    wargs.add_all([d], map_each = lambda f, expander: entry(f, expander, sep = "!"), allow_closure = True)
    ctx.actions.write(written, wargs)
    out = ctx.actions.declare_file(ctx.label.name + ".list")
    args = ctx.actions.args()
    args.add_all([d], map_each = _basename)
    args.add_all([d], map_each = entry, allow_closure = True, expand_directories = False)
    args.add_all([d], map_each = lambda f: prefix + f.basename, allow_closure = True)
    ctx.actions.run_shell(outputs = [out], inputs = [d, written], arguments = [args], command = "echo \"$@\" > {out} && cat {w} >> {out}".format(out = out.path, w = written.path))
    return [DefaultInfo(files = depset([out]))]

mapped = rule(implementation = _mapped, attrs = {"names": attr.label(allow_single_file = True), "prefix": attr.string()})

def _unportable(ctx):
    d = _tree(ctx)
    files = depset([ctx.file.names])
    out = ctx.actions.declare_file(ctx.label.name + ".list")
    args = ctx.actions.args()
    args.add_all([d], map_each = lambda f: str(len(files.to_list())) + f.basename, allow_closure = True)
    ctx.actions.run_shell(outputs = [out], inputs = [d], arguments = [args], command = "echo \"$@\" > " + out.path)
    return [DefaultInfo(files = depset([out]))]

unportable = rule(implementation = _unportable, attrs = {"names": attr.label(allow_single_file = True)})
"#;

const MAPPED_BUILD: &str = "load(':defs.bzl', 'mapped', 'unportable')\nmapped(name = 'm', names = 'names.txt', prefix = 'p')\nunportable(name = 'u', names = 'names.txt')\n";

/// P1 (formerly "map_each plans are never saved"): Args `map_each`
/// functions over a tree artifact — a top-level function, nested `def`s
/// and lambdas capturing strings, ints, lists, tuples, dicts, a File and
/// another closure, a parameter default, in a spawn's arguments and in an
/// `actions.write` — have portable descriptions: the plan is saved and
/// hits, the hit resolves the functions from their module and runs them
/// on the tree's current contents (a source edit that changes the tree's
/// files is a hit whose expansion sees the new files), and an edit of a
/// module the functions read misses. A closure capturing a value with no
/// portable form (a depset) keeps the plan unsaved.
#[test]
fn map_each_plans_save_and_hit() {
    let twin = fixture();
    twin.write("mapped/defs.bzl", MAPPED_RULES);
    twin.write("mapped/consts.bzl", "SUFFIX = 's1'\n");
    twin.write("mapped/names.txt", "a.txt b.txt\n");
    twin.write("mapped/BUILD.bazel", MAPPED_BUILD);
    let args = &["build", "//mapped:m"];
    let mut saved = twin.same(args, &[]);
    if saved.line("saved ").is_none() {
        saved = twin.same(args, &[]);
    }
    saved.assert_saved().assert_success();
    let expected = |names: &[&str], suffix: &str| {
        let basenames = names.join(" ");
        let entries = names
            .iter()
            .map(|name| format!("p:{name}:{suffix}/[1, (True, None)]names.txt-3P-{name}"))
            .collect::<Vec<_>>()
            .join(" ");
        let lambdas = names
            .iter()
            .map(|name| format!("p{name}"))
            .collect::<Vec<_>>()
            .join(" ");
        let written = names
            .iter()
            .map(|name| format!("bazel-out/k8-fastbuild/bin/mapped/m_dir/{name}\n"))
            .collect::<String>();
        format!("{basenames} {entries} {lambdas}\n{written}")
    };
    assert_eq!(
        built_output(&twin, ON, &saved.on),
        expected(&["a.txt", "b.txt"], "s1")
    );
    for _ in 0..2 {
        let hit = twin.same(args, &[]);
        hit.assert_hit().assert_success();
        assert!(
            hit.stderr().contains("0 actions executed"),
            "{}",
            hit.stderr()
        );
        assert_eq!(
            built_output(&twin, ON, &hit.on),
            expected(&["a.txt", "b.txt"], "s1")
        );
    }
    // The tree's files change at execution: a hit, whose functions map the
    // new files.
    twin.write("mapped/names.txt", "a.txt c.txt d.txt\n");
    let edited = twin.same(args, &[]);
    edited.assert_hit().assert_success();
    assert_eq!(
        built_output(&twin, ON, &edited.on),
        expected(&["a.txt", "c.txt", "d.txt"], "s1")
    );
    assert_eq!(
        built_output(&twin, OFF, &edited.off),
        expected(&["a.txt", "c.txt", "d.txt"], "s1")
    );
    // A module the functions read: a miss, saved, then a hit.
    twin.write("mapped/consts.bzl", "SUFFIX = 's2'\n");
    let missed = twin.same(args, &[]);
    missed
        .assert_miss("consts.bzl")
        .assert_saved()
        .assert_success();
    assert_eq!(
        built_output(&twin, ON, &missed.on),
        expected(&["a.txt", "c.txt", "d.txt"], "s2")
    );
    let hit = twin.same(args, &[]);
    hit.assert_hit();
    assert_eq!(
        built_output(&twin, ON, &hit.on),
        expected(&["a.txt", "c.txt", "d.txt"], "s2")
    );
    // A captured depset has no portable form.
    for _ in 0..2 {
        twin.same(&["build", "//mapped:u"], &[])
            .assert_success()
            .assert_not_saved("map-each-deferred");
    }
}

/// The package prints nothing while it is analyzed (no `//:rules.bzl`,
/// whose top level prints): its save runs under BAZEL_RS_TIMING, whose
/// diagnostics lines are written beside the stderr capture and could land
/// inside a printed line (the capture forwards a print's text and its
/// newline as they arrive; the known interleave of tests/snapshot.rs'
/// mutation fuzz).
const TOOLED_RULES: &str = r#"def _tool(ctx):
    out = ctx.actions.declare_file(ctx.label.name + ".sh")
    ctx.actions.write(out, ctx.attr.script, is_executable = True)
    return [DefaultInfo(executable = out, runfiles = ctx.runfiles(files = ctx.files.data))]

tool = rule(implementation = _tool, executable = True, attrs = {
    "script": attr.string(),
    "data": attr.label_list(allow_files = True),
})

def _tooled(ctx):
    outs = []
    for name in ["one", "two"]:
        out = ctx.actions.declare_file(ctx.label.name + "." + name)
        ctx.actions.run(outputs = [out], executable = ctx.executable.tool, arguments = [out.path])
        outs.append(out)
    return [DefaultInfo(files = depset(outs))]

tooled = rule(implementation = _tooled, attrs = {"tool": attr.label(executable = True, cfg = "target")})
"#;

const TOOLED_BUILD: &str = r#"load(":defs.bzl", "tool", "tooled")

tool(
    name = "tool",
    data = ["data.txt"],
    script = '#!/bin/bash\ncat "$0.runfiles/_main/tooled/data.txt" > "$1"\n',
)

tooled(name = "use", tool = ":tool")
"#;

/// PA-1 x L2-W4 (integration-21): a tool's runfiles tree is ONE shared
/// input group of every action using the tool (crate::scheduler::InputGroup);
/// the snapshot encodes it once (the plan's GROUPS table) and a hit executes
/// the decoded holders exactly like the full analysis — also after an edit
/// of a file in the tree, which reruns them with the new content.
#[test]
fn shared_tool_runfiles_groups_save_and_hit() {
    let twin = fixture();
    twin.write("tooled/defs.bzl", TOOLED_RULES);
    twin.write("tooled/BUILD.bazel", TOOLED_BUILD);
    twin.write("tooled/data.txt", "data one\n");
    let args = &["build", "//tooled:use"];
    // Saved under BAZEL_RS_TIMING: the plan holds one group, held by both
    // tool actions (PERF-GROUPS, a diagnostics line: never part of what
    // planning printed, which a hit replays and verify compares).
    let timing = [("BAZEL_RS_TIMING", "1")];
    let mut saved = twin.same(args, &timing);
    if saved.line("saved ").is_none() {
        saved = twin.same(args, &timing);
    }
    saved.assert_saved();
    for output in [&saved.on, &saved.off] {
        let groups = text(&output.stderr)
            .lines()
            .find(|line| line.starts_with("PERF-GROUPS "))
            .map(str::to_owned)
            .unwrap_or_default();
        assert!(
            groups.contains(" holders=2 refs=2 distinct=1 "),
            "{groups}\n{}",
            text(&output.stderr)
        );
    }
    for _ in 0..2 {
        let hit = twin.same(args, &[]);
        hit.assert_hit().assert_success();
        assert!(
            hit.stderr().contains("0 actions executed"),
            "{}",
            hit.stderr()
        );
        assert!(
            !text(&hit.on.stderr).contains("PERF-"),
            "{}",
            text(&hit.on.stderr)
        );
        assert_eq!(built_output(&twin, ON, &hit.on), "data one\n");
    }
    twin.write("tooled/data.txt", "data two\n");
    let edited = twin.same(args, &[]);
    edited.assert_hit().assert_success();
    assert_eq!(built_output(&twin, ON, &edited.on), "data two\n");
    assert_eq!(built_output(&twin, OFF, &edited.off), "data two\n");
    twin.same(args, &[]).assert_hit();
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

// ---- L2-W5f: review follow-ups and test gaps ------------------------------

const ENVIRON_REPOS: &str = r#"def _environ(ctx):
    ctx.file("BUILD.bazel", "exports_files(['v.txt'])\n")
    ctx.file("v.txt", "X=%s\n" % ctx.os.environ.get("W5X", "unset"))

environ_rule = repository_rule(implementation = _environ, environ = ["W5X"])
"#;

/// The single output file of a successful `build` (stdout's first line is
/// `<label>\t<path>`), read while `side` is in place.
fn built_output(twin: &Twin, side: &str, output: &Output) -> String {
    let stdout = text(&output.stdout);
    let path = stdout
        .lines()
        .next()
        .and_then(|line| line.split_once('\t'))
        .map(|(_, path)| path.to_owned())
        .unwrap_or_default();
    twin.with_side(side, || fs::read_to_string(&path).unwrap_or_default())
}

/// L2-W5f review F1 (a reproduced false hit): a non-local rule's `environ`
/// variable that `--repo_env` sets and then inherits again
/// (`--repo_env=W5X=default --repo_env=W5X`: the last entry wins, the
/// client's value) is validated against the command's repository
/// environment: a new client value misses, and the rule's output follows
/// it. A name only set (`--repo_env=W5_SET=v`) ignores the client.
#[test]
fn a_reinherited_repo_env_name_follows_the_client() {
    let twin = fixture();
    twin.write("environ/repos.bzl", ENVIRON_REPOS);
    twin.write(
        "environ/BUILD.bazel",
        "genrule(name = 'g', srcs = ['@envrepo//:v.txt'], outs = ['g.txt'], cmd = 'cp $< $@')\n",
    );
    twin.write(
        "MODULE.bazel",
        &format!(
            "{MODULE}\ner = use_repo_rule('//environ:repos.bzl', 'environ_rule')\ner(name = 'envrepo')\n"
        ),
    );
    let args = &[
        "build",
        "--repo_env=W5X=default",
        "--repo_env=W5X",
        "//environ:g",
    ];
    let one = [("W5X", "one")];
    twin.save(args, &one);
    let hit = twin.same(args, &one);
    hit.assert_hit();
    assert_eq!(built_output(&twin, ON, &hit.on), "X=one\n");
    let two = [("W5X", "two")];
    let changed = twin.same(args, &two);
    changed
        .assert_success()
        .assert_miss("ENV W5X: value changed");
    assert_eq!(built_output(&twin, ON, &changed.on), "X=two\n");
    twin.settle(&changed, args, &two);
    twin.same(args, &two).assert_hit();
    // Without a client value the set value stands (Inherit keeps it).
    let unset = twin.same(args, &[]);
    unset.assert_miss("ENV W5X: value changed");
    assert_eq!(built_output(&twin, ON, &unset.on), "X=default\n");
    // A name an option only sets: the client's value does not matter.
    let set = &["build", "--repo_env=W5X=fixed", "//environ:g"];
    twin.save(set, &one);
    let fixed = twin.same(set, &two);
    fixed.assert_hit();
    assert_eq!(built_output(&twin, ON, &fixed.on), "X=fixed\n");
}

/// L2-W5f review F2: under `--nobuild` planning runs the status command
/// for a target that consumes `ctx.info_file` although the plan holds no
/// status placeholder. A hit runs it too (quietly, before anything
/// prints), and a status command that now writes to stderr is a miss whose
/// full analysis prints it.
#[test]
fn a_nobuild_status_consumer_runs_the_status_command_on_a_hit() {
    let twin = fixture();
    twin.write("stable.txt", "one\n");
    twin.each(|root| {
        write_executable_script(
            &root.join("ws/status.sh"),
            "#!/bin/sh\necho ran >> ran.log\necho STABLE_W5 $(cat stable.txt)\n[ -e noisy ] && echo 'status: now noisy' >&2\nexit 0\n",
            0o755,
        );
    });
    let status = format!(
        "--workspace_status_command={}",
        twin.ws().join("status.sh").display()
    );
    let args = &["build", "--nobuild", status.as_str(), "//:stamped"];
    let runs = |side: &str| {
        fs::read_to_string(twin.side(side).join("ws/ran.log"))
            .unwrap_or_default()
            .lines()
            .count()
    };
    twin.save(args, &[]);
    twin.same(args, &[]).assert_hit();
    twin.write("noisy", "");
    let noisy = twin.same(args, &[]);
    noisy
        .assert_success()
        .assert_miss("the workspace status command wrote")
        .assert_not_saved("workspace-status-stderr");
    assert!(
        noisy.stderr().contains("status: now noisy"),
        "{}",
        noisy.stderr()
    );
    // The snapshot cannot hit while the command writes to stderr: removed,
    // so a noisy command runs once per command again.
    assert!(twin.snapshots().is_empty());
    twin.same(args, &[])
        .assert_miss("no snapshot")
        .assert_not_saved("workspace-status-stderr");
    twin.each(|root| fs::remove_file(root.join("ws/noisy")).unwrap());
    twin.same(args, &[]).assert_saved();
    twin.same(args, &[]).assert_hit();
    // The command ran once per command on both sides — a hit runs it, too —
    // but for the noisy miss's attempt (and, in verify mode, each hit's
    // shadow full analysis: two hits).
    let extra = if twin.mode.get() == VERIFY { 3 } else { 1 };
    assert_eq!(runs(ON), runs(OFF) + extra, "{} {}", runs(ON), runs(OFF));
}

const WATCHING_REPOS: &str = r#"def _watching(ctx):
    ctx.file("BUILD.bazel", "exports_files(['v.txt'])\n")
    ctx.file("v.txt", "%s|%s\n" % (ctx.os.environ.get("W5_GEN", "unset"), ctx.read(Label("//watch:watched.txt")).strip()))

watching_rule = repository_rule(implementation = _watching, environ = ["W5_GEN"])
"#;

/// Test gap (L2-W5f): a reused NON-local rule whose `environ` variable or
/// watched workspace file (`ctx.read` of a label) changes misses — the
/// full analysis re-executes it — and its output follows.
#[test]
fn a_reused_rule_misses_on_its_environ_variable_or_watched_file() {
    let twin = fixture();
    twin.write("watch/repos.bzl", WATCHING_REPOS);
    twin.write("watch/watched.txt", "first\n");
    twin.write(
        "watch/BUILD.bazel",
        "exports_files(['watched.txt'])\ngenrule(name = 'w', srcs = ['@watching//:v.txt'], outs = ['w.txt'], cmd = 'cp $< $@')\n",
    );
    twin.write(
        "MODULE.bazel",
        &format!(
            "{MODULE}\nwr = use_repo_rule('//watch:repos.bzl', 'watching_rule')\nwr(name = 'watching')\n"
        ),
    );
    let args = &["build", "//watch:w"];
    let one = [("W5_GEN", "one")];
    twin.save(args, &one);
    twin.same(args, &one).assert_hit();
    let two = [("W5_GEN", "two")];
    let changed = twin.same(args, &two);
    changed.assert_success().assert_miss("ENV W5_GEN");
    assert_eq!(built_output(&twin, ON, &changed.on), "two|first\n");
    twin.settle(&changed, args, &two);
    twin.same(args, &two).assert_hit();
    twin.write("watch/watched.txt", "second\n");
    let edited = twin.same(args, &two);
    edited.assert_success().assert_miss("watched.txt");
    assert_eq!(built_output(&twin, ON, &edited.on), "two|second\n");
    twin.settle(&edited, args, &two);
    twin.same(args, &two).assert_hit();
}

const INPUT_EXTENSION: &str = r#"def _repo(ctx):
    ctx.file("BUILD.bazel", "exports_files(['v.txt'])\n")
    ctx.file("v.txt", ctx.attr.content)

_repo_rule = repository_rule(implementation = _repo, attrs = {"content": attr.string()})

def _ext(module_ctx):
    data = module_ctx.read(module_ctx.path(Label("//ext:input.txt"))).strip()
    _repo_rule(name = "extrepo", content = "%s|%s\n" % (module_ctx.getenv("W5_EXT", "unset"), data))

ext = module_extension(implementation = _ext)
"#;

/// Test gap (L2-W5f): a reused module extension (its lockfile entry) whose
/// environment variable or file input changes misses — the full analysis
/// evaluates it again — and the repository it generates follows.
#[test]
fn a_reused_extension_misses_on_its_environment_or_file_input() {
    let twin = fixture();
    twin.write("ext/ext.bzl", INPUT_EXTENSION);
    twin.write("ext/input.txt", "first\n");
    twin.write(
        "MODULE.bazel",
        &format!(
            "{MODULE}\next = use_extension('//ext:ext.bzl', 'ext')\nuse_repo(ext, 'extrepo')\n"
        ),
    );
    twin.write(
        "ext/BUILD.bazel",
        "exports_files(['input.txt'])\ngenrule(name = 'e', srcs = ['@extrepo//:v.txt'], outs = ['e.txt'], cmd = 'cp $< $@')\n",
    );
    let args = &["build", "//ext:e"];
    let one = [("W5_EXT", "one")];
    // Cold: the extension is evaluated, the lockfile written; then saved.
    for _ in 0..3 {
        if twin.same(args, &one).line("saved ").is_some() {
            break;
        }
    }
    twin.same(args, &one).assert_hit();
    let two = [("W5_EXT", "two")];
    let changed = twin.same(args, &two);
    changed.assert_success().assert_miss("W5_EXT");
    assert_eq!(built_output(&twin, ON, &changed.on), "two|first\n");
    for _ in 0..3 {
        if twin.same(args, &two).line("saved ").is_some() {
            break;
        }
    }
    twin.same(args, &two).assert_hit();
    twin.write("ext/input.txt", "second\n");
    let edited = twin.same(args, &two);
    edited.assert_success().assert_miss("input.txt");
    assert_eq!(built_output(&twin, ON, &edited.on), "two|second\n");
}

/// Test gap (L2-W5f): a `print()` in MODULE.bazel is part of what planning
/// printed: a hit replays it (and a MODULE.bazel edit misses).
#[test]
fn module_file_prints_replay_on_a_hit() {
    let twin = fixture();
    twin.write(
        "MODULE.bazel",
        &format!("{MODULE}\nprint(\"module file says hi\")\n"),
    );
    twin.save(BUILD_ALL, &[]);
    for _ in 0..2 {
        let hit = twin.same(BUILD_ALL, &[]);
        hit.assert_hit().assert_success();
        assert!(
            hit.stderr().contains("module file says hi"),
            "{}",
            hit.stderr()
        );
    }
    twin.write(
        "MODULE.bazel",
        &format!("{MODULE}\nprint(\"module file says bye\")\n"),
    );
    let edited = twin.same(BUILD_ALL, &[]);
    edited.assert_miss("MODULE.bazel");
    assert!(
        edited.stderr().contains("module file says bye"),
        "{}",
        edited.stderr()
    );
}

/// Test gap (L2-W5f): edits to a `local_path_override` module (its BUILD
/// file, its MODULE.bazel) and to a `file://` registry (a module's
/// MODULE.bazel there, the `local_path` source it points at) miss.
#[test]
fn local_override_and_file_registry_edits_miss() {
    let twin = fixture();
    // A local_path_override module beside the workspace.
    twin.write_beside(
        "localdep/MODULE.bazel",
        "module(name = 'localdep', version = '1.0')\n",
    );
    twin.write_beside(
        "localdep/BUILD.bazel",
        "genrule(name = 'l', outs = ['l.txt'], cmd = 'echo one > $@', visibility = ['//visibility:public'])\n",
    );
    // A file:// registry beside the workspace, its module a local_path source.
    twin.write_beside(
        "registry/bazel_registry.json",
        r#"{"module_base_path": "."}"#,
    );
    twin.write_beside(
        "registry/modules/w5reg/metadata.json",
        r#"{"versions": ["1.0"]}"#,
    );
    twin.write_beside(
        "registry/modules/w5reg/1.0/MODULE.bazel",
        "module(name = 'w5reg', version = '1.0')\n",
    );
    twin.write_beside(
        "registry/modules/w5reg/1.0/source.json",
        r#"{"type": "local_path", "path": "src/w5reg"}"#,
    );
    twin.write_beside(
        "registry/src/w5reg/MODULE.bazel",
        "module(name = 'w5reg', version = '1.0')\n",
    );
    twin.write_beside(
        "registry/src/w5reg/BUILD.bazel",
        "genrule(name = 'r', outs = ['r.txt'], cmd = 'echo one > $@', visibility = ['//visibility:public'])\n",
    );
    twin.write(
        "MODULE.bazel",
        &format!(
            "{MODULE}\nbazel_dep(name = 'localdep', version = '1.0')\nlocal_path_override(module_name = 'localdep', path = '../localdep')\nbazel_dep(name = 'w5reg', version = '1.0')\n"
        ),
    );
    twin.write(
        "deps/BUILD.bazel",
        "genrule(name = 'd', srcs = ['@localdep//:l', '@w5reg//:r'], outs = ['d.txt'], cmd = 'cat $(SRCS) > $@')\n",
    );
    let registry = format!("--registry=file://{}/registry", twin.run_root().display());
    let args = &["build", registry.as_str(), "//deps:d"];
    twin.save(args, &[]);
    twin.same(args, &[]).assert_hit();
    let edits: [(&str, &str, &str); 4] = [
        (
            "localdep/BUILD.bazel",
            "genrule(name = 'l', outs = ['l.txt'], cmd = 'echo two > $@', visibility = ['//visibility:public'])\n",
            "localdep/BUILD.bazel",
        ),
        (
            "localdep/MODULE.bazel",
            "module(name = 'localdep', version = '1.0')\n# edited\n",
            "localdep/MODULE.bazel",
        ),
        (
            "registry/src/w5reg/BUILD.bazel",
            "genrule(name = 'r', outs = ['r.txt'], cmd = 'echo two > $@', visibility = ['//visibility:public'])\n",
            "w5reg/BUILD.bazel",
        ),
        (
            "registry/modules/w5reg/1.0/MODULE.bazel",
            "module(name = 'w5reg', version = '1.0')\n# edited\n",
            "w5reg/1.0/MODULE.bazel",
        ),
    ];
    for (path, content, reason) in edits {
        twin.write_beside(path, content);
        let edited = twin.same(args, &[]);
        edited.assert_success().assert_miss(reason);
        twin.settle(&edited, args, &[]);
        twin.same(args, &[]).assert_hit();
    }
}

/// Test gap (L2-W5f): a new BUILD file inside a directory a recursive glob
/// descends into makes it a subpackage the glob stops at: a miss, and the
/// glob's result changes.
#[test]
fn a_new_build_file_in_a_globbed_subdirectory_misses() {
    let twin = fixture();
    twin.write(
        "gl/BUILD.bazel",
        "genrule(name = 'g', srcs = glob(['**/*.txt']), outs = ['g.list'], cmd = 'echo $(SRCS) > $@')\n",
    );
    twin.write("gl/a.txt", "a\n");
    twin.write("gl/sub/b.txt", "b\n");
    let args = &["build", "//gl:g"];
    twin.save(args, &[]);
    twin.same(args, &[]).assert_hit();
    twin.write("gl/sub/BUILD.bazel", "exports_files(['b.txt'])\n");
    let edited = twin.same(args, &[]);
    edited.assert_success().assert_miss("gl/sub");
    let listed = built_output(&twin, ON, &edited.on);
    assert!(!listed.contains("b.txt"), "{listed}");
    twin.settle(&edited, args, &[]);
    twin.same(args, &[]).assert_hit();
}

/// Test gap (L2-W5f): an edit of REPO.bazel's `ignore_directories()`
/// misses (`//...` sees another set of packages).
#[test]
fn repo_file_ignore_directories_edits_miss() {
    let twin = fixture();
    twin.write("REPO.bazel", "ignore_directories(['other'])\n");
    let args = &["build", "--nobuild", "//..."];
    twin.save(args, &[]);
    twin.same(args, &[]).assert_hit();
    twin.write("REPO.bazel", "ignore_directories([])\n");
    let edited = twin.same(args, &[]);
    edited.assert_success().assert_miss("REPO.bazel");
    twin.settle(&edited, args, &[]);
    twin.same(args, &[]).assert_hit();
}

/// L2-W5f review F4: a hit whose command fails on a path in the repository
/// store — here the test runner staging a runfile of a verified store tree
/// that became unreadable (per tree, a file's own mode is not validated;
/// the failure comes after the build part succeeded), the error naming an
/// existing path — removes the snapshot with a one-line hint; the next run
/// repairs the tree like the run without the snapshot did, and passes.
/// (Verify mode's shadow full analysis repairs the tree first: the
/// documented per-tree trade-off is a mismatch there.)
#[test]
fn a_store_input_staged_by_the_test_runner_heals() {
    let twin = fixture();
    twin.write(
        "heal/BUILD.bazel",
        "load('//:rules.bzl', 'script_test')\nscript_test(name = 'gen_test', size = 'small', data = ['@genrepo//:data.txt'], script = '#!/bin/bash\\necho gen-test-ran\\n')\n",
    );
    let args = &["test", "//heal:gen_test"];
    twin.save(args, &[]);
    // Settled keys (no racy tree head verifies the tree on the hit).
    std::thread::sleep(std::time::Duration::from_millis(2200));
    twin.same(args, &[]).assert_hit().assert_success();
    std::thread::sleep(std::time::Duration::from_millis(2200));
    twin.same(args, &[]).assert_hit().assert_success();
    let tree_file = |root: &Path| -> PathBuf {
        fs::read_dir(root.join("cache/repositories/direct/trees"))
            .unwrap()
            .flatten()
            .map(|entry| entry.path().join("data.txt"))
            .find(|path| path.exists())
            .expect("the generated repository's data.txt")
    };
    twin.each(|root| {
        fs::set_permissions(tree_file(root), fs::Permissions::from_mode(0o000)).unwrap();
    });
    let stale = twin.run(args, &[]);
    assert!(stale.line("hit ").is_some(), "{:?}", stale.explain());
    assert!(stale.off.status.success(), "{}", text(&stale.off.stderr));
    if twin.mode.get() == VERIFY {
        stale.assert_verify_mismatch("repository-rule-executed");
        let next = twin.run(args, &[]);
        next.assert_success().assert_miss("no snapshot");
        twin.settle(&next, args, &[]);
        twin.same(args, &[]).assert_hit().assert_success();
        return;
    }
    assert!(!stale.on.status.success());
    let stderr = text(&stale.on.stderr);
    assert!(
        stderr.contains(
            "bazel-rs: removed the analysis snapshot of this build: its failure involves "
        ) && stderr.contains("data.txt in the repository store"),
        "{stderr}"
    );
    // The one heal of this fixture (L2-W5h).
    let healed = twin.assert_healed(1);
    assert!(
        healed[0].contains(" reason=store-path:") && healed[0].contains("/data.txt ms="),
        "{healed:?}"
    );
    assert!(twin.snapshots().is_empty());
    // The next run repairs the tree like the run without the snapshot did.
    let repaired = Pair {
        on: twin.run_side(ON, args, &[]),
        off: stale.off,
        mode: twin.mode.get(),
    };
    repaired.assert_same(args);
    repaired.assert_success().assert_miss("no snapshot");
    twin.settle(&repaired, args, &[]);
    twin.same(args, &[]).assert_hit().assert_success();
}

/// Test gap (L2-W5f): hits that rewrite their snapshot (racy keys settled)
/// racing a concurrent save of the same key: every process succeeds with
/// the snapshot-less output, and the store ends with one valid snapshot.
#[test]
fn hit_refreshes_racing_a_concurrent_save_leave_a_valid_snapshot() {
    let twin = fixture();
    twin.same(BUILD_ALL, &[]).assert_saved();
    // Each round starts with a fresh save.
    twin.with_side(ON, || {
        for file in fs::read_dir(state_directory(&twin.cache()).join("snapshots/v1"))
            .unwrap()
            .flatten()
        {
            fs::remove_file(file.path()).unwrap();
        }
    });
    let off = twin.run_side(OFF, BUILD_ALL, &[]);
    for round in 0..3 {
        // A fresh save (its keys racy: the next hits refresh them), then
        // two hits and one process whose snapshot was removed under it.
        let saved = twin.run_side(ON, BUILD_ALL, &[]);
        let saved = Pair {
            on: saved,
            off: off.clone(),
            mode: twin.mode.get(),
        };
        saved.assert_same(BUILD_ALL);
        let outputs = twin.with_side(ON, || {
            let spawn = || {
                twin.command(BUILD_ALL, &[], true)
                    .stdout(std::process::Stdio::piped())
                    .stderr(std::process::Stdio::piped())
                    .spawn()
                    .unwrap()
            };
            let mut children = vec![spawn(), spawn()];
            if round % 2 == 0 {
                for file in fs::read_dir(state_directory(&twin.cache()).join("snapshots/v1"))
                    .unwrap()
                    .flatten()
                {
                    let _ = fs::remove_file(file.path());
                }
            }
            children.push(spawn());
            children
                .into_iter()
                .map(|child| child.wait_with_output().unwrap())
                .collect::<Vec<_>>()
        });
        for on in outputs {
            let pair = Pair {
                on,
                off: off.clone(),
                mode: twin.mode.get(),
            };
            pair.assert_same(BUILD_ALL);
            assert!(
                pair.line("hit ").is_some()
                    || pair.line("saved ").is_some()
                    || pair.line("not saved").is_some(),
                "{:?}",
                pair.explain()
            );
        }
        assert!(twin.snapshots().len() <= 1, "{:?}", twin.snapshots());
    }
    // Whatever won, the next run is a hit or saves, then hits.
    let next = twin.same(BUILD_ALL, &[]);
    if next.line("hit ").is_none() {
        twin.same(BUILD_ALL, &[]).assert_hit();
    }
    assert_eq!(twin.snapshots().len(), 1);
}

/// Test gap (L2-W5f): garbage collection during a hit. A hit holds its
/// snapshot open (sections are read by offset), so another process's save
/// that collects it as the least recently used of more than the store's
/// [`STORE_KEEP`]
/// never breaks it; a refresh written after the collection is a valid
/// snapshot again.
#[test]
fn garbage_collection_during_a_hit_never_breaks_it() {
    let twin = warmed(BUILD_ALL, &[]);
    twin.same(BUILD_ALL, &[]).assert_hit();
    let off = twin.run_side(OFF, BUILD_ALL, &[]);
    for round in 0..3 {
        // STORE_KEEP - 1 newer snapshots of other keys (never read: stand-ins), the
        // hit's own made the least recently used.
        {
            let store = twin.store(ON).join("v1");
            for entry in fs::read_dir(&store).unwrap().flatten() {
                if entry.path().extension().is_some_and(|ext| ext == "snap") {
                    fs::File::options()
                        .write(true)
                        .open(entry.path())
                        .unwrap()
                        .set_modified(
                            std::time::SystemTime::now() - std::time::Duration::from_secs(600),
                        )
                        .unwrap();
                }
            }
            for n in 0..STORE_KEEP - 1 {
                fs::write(
                    store.join(format!("0000000000000000000000000000{n:04}.snap")),
                    b"x",
                )
                .unwrap();
            }
        }
        let outputs = twin.with_side(ON, || {
            let hit = twin
                .command(BUILD_ALL, &[], true)
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::piped())
                .spawn()
                .unwrap();
            // A save of another key collects the oldest snapshot meanwhile.
            let define = format!("--define=w5gc={round}");
            let save = twin
                .command(&["build", define.as_str(), "//:all_inputs"], &[], true)
                .output()
                .unwrap();
            (hit.wait_with_output().unwrap(), save)
        });
        assert!(outputs.1.status.success(), "{}", text(&outputs.1.stderr));
        let pair = Pair {
            on: outputs.0,
            off: off.clone(),
            mode: twin.mode.get(),
        };
        pair.assert_same(BUILD_ALL);
        assert!(
            twin.snapshots().len() <= STORE_KEEP,
            "{}",
            twin.snapshots().len()
        );
        // Back to one snapshot of this key (hit, or saved again).
        for entry in fs::read_dir(twin.store(ON).join("v1")).unwrap().flatten() {
            if entry
                .file_name()
                .to_string_lossy()
                .starts_with("0000000000000000")
            {
                let _ = fs::remove_file(entry.path());
            }
        }
        let next = twin.same(BUILD_ALL, &[]);
        next.assert_success();
        if next.line("hit ").is_none() {
            twin.same(BUILD_ALL, &[]).assert_hit();
        }
    }
}

/// L2-W5f, verify mode's default (no `BAZEL_RS_SNAPSHOT_VERIFY_STRICT`): a
/// mismatch — the documented in-place-edit trade-off: the full analysis
/// repairs the tree the hit trusted — prints the differences on the
/// diagnostics stream, appends one line to `<state>/snapshots/verify.log`,
/// removes the snapshot and executes the FULL ANALYSIS's plan: output and
/// exit code are those of the run without the snapshot.
#[test]
fn verify_mismatches_execute_the_full_analysis_by_default() {
    MODE.with(|mode| mode.set(VERIFY));
    STRICT.with(|strict| strict.set(false));
    let build = ROOT_BUILD.replace("@genrepo//:data.txt", "@genrepo//:sub/data.txt");
    let twin = fixture();
    twin.write("repos.bzl", NESTED_REPOS);
    twin.write("BUILD.bazel", &build);
    twin.same(BUILD_ALL, &[]).assert_saved();
    std::thread::sleep(std::time::Duration::from_millis(2200));
    twin.same(BUILD_ALL, &[]).assert_hit();
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
    });
    // Byte-identical to the run without the snapshot, exit code included.
    let mismatch = twin.same(BUILD_ALL, &[]);
    mismatch.assert_success();
    assert!(mismatch.line("hit ").is_some(), "{:?}", mismatch.explain());
    let line = mismatch.line("verify ").unwrap_or_default();
    assert!(
        line.contains("MISMATCH")
            && line.contains("first: taint")
            && line.contains("logged to")
            && line.contains("the snapshot is set aside as "),
        "{line}"
    );
    assert!(twin.snapshots().is_empty());
    // L2-W5i: set aside, not deleted, and every difference kept beside the
    // verify.log line.
    let aside: Vec<PathBuf> = fs::read_dir(twin.store(ON).join("mismatched"))
        .unwrap()
        .flatten()
        .map(|entry| entry.path())
        .collect();
    assert_eq!(aside.len(), 1, "{aside:?}");
    let log = fs::read_to_string(twin.store(ON).join("verify.log")).unwrap();
    assert_eq!(log.lines().count(), 1, "{log}");
    let details = fs::read_to_string(twin.store(ON).join("verify-details.log")).unwrap();
    // (Named where the command ran: the twins' shared path.)
    assert!(
        details.starts_with(&log)
            && details.contains(&format!(
                "\n  snapshot: set aside as {}\n",
                twin.run_root()
                    .join("cache")
                    .join(
                        twin.store(ON)
                            .strip_prefix(twin.side(ON).join("cache"))
                            .unwrap()
                    )
                    .join("mismatched")
                    .join(aside[0].file_name().unwrap())
                    .display()
            ))
            && details.contains("\n  verify mismatch: the full analysis would not be saved\n")
            && details.contains("\n  verify   taint repository-rule-executed"),
        "{details}"
    );
    // L2-W5g: the first taint's name and the first differing path (the
    // repaired tree's observations, recorded now that it was executed).
    assert!(
        log.contains(" command=build first=taint ")
            && log.contains(" taint=repository-rule-executed path=")
            && log.contains("/cache/repositories/direct/trees/")
            && log.ends_with(" targets=//:all_inputs\n"),
        "{log}"
    );
    // The next run analyzes and saves anew, then hits (verified).
    twin.same(BUILD_ALL, &[])
        .assert_miss("no snapshot")
        .assert_saved();
    twin.same(BUILD_ALL, &[]).assert_hit();
}

/// L2-W5g/L2-W5h soak telemetry: every snapshot event of a command appends
/// one line to `<state>/snapshots/events.log` (the workspace's state
/// directory, docs/WORKTREES.md) — a lookup's hit or miss (the reason's
/// class and the path it names) with the attempt's wall time, a save, a
/// full analysis that is not saved (the first taint's name, or `failed`) —
/// with the mode; the run without the snapshot logs nothing.
#[test]
fn the_event_log_names_each_outcome() {
    let twin = warmed(BUILD_ALL, &[]);
    twin.write("BUILD.bazel", &format!("{ROOT_BUILD}\n# edited\n"));
    twin.same(BUILD_ALL, &[])
        .assert_miss("BUILD.bazel")
        .assert_saved();
    twin.same(BUILD_ALL, &[]).assert_hit();
    twin.write(
        "pkg/BUILD.bazel",
        "exports_files(['in.txt'])\ngenrule(name = 'bad', srcs = [':missing'], outs = ['bad.txt'], cmd = 'exit 1')\n",
    );
    let failing = &["build", "//pkg:bad"];
    assert!(!twin.same(failing, &[]).on.status.success());
    let log = fs::read_to_string(twin.store(ON).join("events.log")).unwrap();
    let field = |line: &str, name: &str| -> String {
        line.split(' ')
            .find_map(|token| token.strip_prefix(name))
            .unwrap_or_default()
            .to_owned()
    };
    let outcomes: Vec<[String; 4]> = log
        .lines()
        .map(|line| {
            [
                field(line, "kind="),
                field(line, "mode="),
                field(line, "reason="),
                field(line, "targets="),
            ]
        })
        .collect();
    let mode = twin.mode.get();
    let expected = [
        ["miss", mode, "absent", "//:all_inputs"],
        ["saved", mode, "-", "//:all_inputs"],
        ["hit", mode, "-", "//:all_inputs"],
        ["miss", mode, "read", "//:all_inputs"],
        ["saved", mode, "-", "//:all_inputs"],
        ["hit", mode, "-", "//:all_inputs"],
        ["miss", mode, "absent", "//pkg:bad"],
        ["not-saved", mode, "failed", "//pkg:bad"],
    ];
    assert_eq!(
        outcomes,
        expected.map(|row| row.map(str::to_owned)),
        "{log}"
    );
    let read = log.lines().nth(3).unwrap();
    assert!(
        read.contains("/run/ws/BUILD.bazel ms=") && read.contains(" detail=READ\\x20"),
        "{read}"
    );
    // L2-W5i: a save names the binary (the first 8 bytes of its identity),
    // so the soak report sees an upgrade.
    let saved = log.lines().nth(4).unwrap();
    let binary = field(saved, "detail=")
        .split(',')
        .find_map(|part| part.strip_prefix("binary:"))
        .unwrap_or_default()
        .to_owned();
    assert!(
        binary.len() == 16 && binary.bytes().all(|byte| byte.is_ascii_hexdigit()),
        "{saved}"
    );
    // Lookups and saves carry their wall time.
    for line in log.lines() {
        let ms = field(line, "ms=");
        match field(line, "kind=").as_str() {
            "hit" | "miss" | "saved" => assert!(ms.parse::<f64>().is_ok(), "{line}"),
            _ => assert_eq!(ms, "-", "{line}"),
        }
    }
    assert!(!twin.store(OFF).join("events.log").exists());
}

// ---- verification-based healing (L2-W5g) -----------------------------------

const TAMPER_REPOS: &str = r##"def _tools(ctx):
    ctx.file("BUILD.bazel", "exports_files(['tool.sh', 'data.txt'])\n")
    ctx.file("tool.sh", "#!/bin/sh\necho tool-ok > $1\n", executable = True)
    ctx.file("data.txt", "good\n")

gen_rule = repository_rule(implementation = _tools)

local_rule = repository_rule(implementation = _tools, local = True)
"##;

const TAMPER_BUILD: &str = r##"load("//:rules.bzl", "script_test")

genrule(name = "t", srcs = ["@tools//:tool.sh"], outs = ["t.txt"], cmd = "$(location @tools//:tool.sh) $@")

script_test(name = "data_test", size = "small", data = ["@tools//:data.txt"], script = "#!/bin/bash\nf=$(find -L .. -name data.txt | head -1)\ncat $f\ngrep -q good $f\n")
"##;

/// The reviewer's tampering cases (L2-W5f re-review): a store tree's or a
/// local repository's published tool made non-executable (`chmod 644`), or
/// a test's runfile edited in place (same size, modification time
/// restored).
#[derive(Clone, Copy, Debug)]
enum Tamper {
    ChmodTool,
    EditRunfile,
}

/// A workspace whose `@tools` repository is generated by a store rule or
/// (`local`) a `local = True` rule.
fn tamper_fixture(local: bool) -> Twin {
    Twin::new(&[
        (
            "MODULE.bazel",
            &format!(
                "module(name = 'tamper')\n\nr = use_repo_rule('//:repos.bzl', '{}')\nr(name = 'tools')\n",
                if local { "local_rule" } else { "gen_rule" }
            ),
        ),
        ("repos.bzl", TAMPER_REPOS),
        ("rules.bzl", RULES),
        ("BUILD.bazel", TAMPER_BUILD),
    ])
}

/// The published tree of `@tools` (by its tool: the file itself may be
/// damaged).
fn tools_tree(root: &Path) -> PathBuf {
    fs::read_dir(root.join("cache/repositories/direct/trees"))
        .unwrap()
        .flatten()
        .map(|entry| entry.path())
        .find(|path| path.join("tool.sh").exists())
        .expect("the published tree of @tools")
}

fn tamper(root: &Path, case: Tamper) {
    let tree = tools_tree(root);
    match case {
        Tamper::ChmodTool => {
            fs::set_permissions(tree.join("tool.sh"), fs::Permissions::from_mode(0o644)).unwrap();
        }
        Tamper::EditRunfile => {
            let file = tree.join("data.txt");
            let metadata = fs::metadata(&file).unwrap();
            let mode = metadata.permissions().mode();
            let modified = metadata.modified().unwrap();
            fs::set_permissions(&file, fs::Permissions::from_mode(0o644)).unwrap();
            fs::write(&file, "bad!\n").unwrap();
            let handle = fs::File::options().write(true).open(&file).unwrap();
            handle.set_modified(modified).unwrap();
            drop(handle);
            fs::set_permissions(&file, fs::Permissions::from_mode(mode)).unwrap();
            assert_eq!(fs::metadata(&file).unwrap().len(), metadata.len());
        }
    }
}

/// L2-W5g, verification-based healing (the reviewer's cases): a byte or
/// mode edit of a store tree's file a hit's execution uses is invisible to
/// the hit's validation; the command fails (the tool: `Permission denied`;
/// the test: exit 3), and the failed hit verifies the snapshot's trees as
/// their store's warm check does, removes the snapshot and says so in one
/// line naming the first differing entry (L2-W5h), so the next run's full
/// analysis repairs the tree and succeeds like the run without the
/// snapshot (which repaired it at once). Verify mode: the shadow full
/// analysis repairs the tree before the hit executes — a mismatch.
///
/// L2-W5h: a local repository's published tree is checked on every hit
/// (`local`): the same tampering is a miss naming the differing entry, and
/// the full analysis repairs the tree — never a failed hit, in either mode.
fn a_failed_hit_on_a_tampered_tree_heals(local: bool) {
    for case in [Tamper::ChmodTool, Tamper::EditRunfile] {
        let twin = tamper_fixture(local);
        let args: &[&str] = match case {
            Tamper::ChmodTool => &["build", "//:t"],
            Tamper::EditRunfile => &["test", "//:data_test"],
        };
        twin.save(args, &[]);
        // Settled keys: the hit verifies no racy tree against its manifest.
        std::thread::sleep(std::time::Duration::from_millis(2200));
        twin.same(args, &[]).assert_hit().assert_success();
        std::thread::sleep(std::time::Duration::from_millis(2200));
        twin.same(args, &[]).assert_hit().assert_success();
        twin.each(|root| tamper(root, case));
        // The first differing entry the heal or the miss names (L2-W5h).
        let (entry, file) = match case {
            Tamper::ChmodTool => ("mode", "tool.sh"),
            Tamper::EditRunfile => ("content", "data.txt"),
        };
        if local {
            let repaired = twin.same(args, &[]);
            repaired.assert_success().assert_miss(&format!(
                "its published tree differs from its manifest: {entry} "
            ));
            assert!(
                repaired
                    .line("miss ")
                    .is_some_and(|line| line.ends_with(&format!("/{file}"))),
                "{case:?}: {:?}",
                repaired.explain()
            );
            assert!(!text(&repaired.on.stderr).contains(HEAL_LINE));
            let log = fs::read_to_string(twin.store(ON).join("events.log")).unwrap();
            let misses: Vec<&str> = log
                .lines()
                .filter(|line| line.contains(" kind=miss ") && line.contains(" reason=local-tree "))
                .collect();
            assert!(
                misses.len() == 1 && misses[0].contains(&format!("/{file} ms=")),
                "{case:?}: {log}"
            );
            assert!(!log.contains(" kind=healed "), "{case:?}: {log}");
            twin.settle(&repaired, args, &[]);
            twin.same(args, &[]).assert_hit().assert_success();
            continue;
        }
        let failed = twin.run(args, &[]);
        assert!(
            failed.line("hit ").is_some(),
            "{case:?}: {:?}",
            failed.explain()
        );
        assert!(failed.off.status.success(), "{}", text(&failed.off.stderr));
        if twin.mode.get() == VERIFY {
            failed.assert_verify_mismatch("repository-rule-executed");
            twin.same(args, &[])
                .assert_success()
                .assert_miss("no snapshot");
            continue;
        }
        assert!(!failed.on.status.success(), "{case:?}: the hit succeeded");
        let stderr = text(&failed.on.stderr);
        assert!(
            stderr.contains("bazel-rs: removed the analysis snapshot of this build: it failed, and the repository tree ")
                && stderr.contains("/cache/repositories/direct/trees/"),
            "{case:?}: {stderr}"
        );
        assert!(twin.snapshots().is_empty(), "{case:?}");
        // L2-W5h: the heal names the first differing entry.
        assert!(
            stderr.contains(&format!(" ({entry} {file}); the next build re-analyzes")),
            "{case:?}: {stderr}"
        );
        // Exactly one heal, naming the entry (L2-W5h).
        let healed = twin.assert_healed(1);
        assert!(
            healed[0].contains(&format!(" reason=tree:{entry} "))
                && healed[0].contains(&format!("/{file} ms=")),
            "{healed:?}"
        );
        // The next run repairs the tree like the run without the snapshot
        // did, and succeeds. (A test runs again either way: the failed
        // attempt's result replaced the cached one, which the side without
        // the snapshot never saw fail; `--cache_test_results` is no part of
        // the snapshot key.)
        let again: &[&str] = match case {
            Tamper::ChmodTool => args,
            Tamper::EditRunfile => &["test", "--nocache_test_results", "//:data_test"],
        };
        let repaired = twin.same(again, &[]);
        repaired.assert_success().assert_miss("no snapshot");
        twin.settle(&repaired, args, &[]);
        twin.same(args, &[]).assert_hit().assert_success();
    }
}

#[test]
fn a_failed_hit_on_a_tampered_store_tree_heals() {
    a_failed_hit_on_a_tampered_tree_heals(false);
}

/// L2-W5h: a local repository's tampered tree is a miss of the next
/// lookup (see [`a_failed_hit_on_a_tampered_tree_heals`]).
#[test]
fn a_tampered_local_repository_misses() {
    a_failed_hit_on_a_tampered_tree_heals(true);
}

// ---- the mutation fuzz (L2-W5f, extended by L2-W5g) ------------------------

/// A deterministic random sequence (xorshift64*).
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_f491_4f6c_dd1d)
    }

    fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }

    fn percent(&mut self, chance: u64) -> bool {
        self.next() % 100 < chance
    }
}

/// The fuzz's MODULE.bazel; `flavor`: the generated repository's attribute
/// (a semantic MODULE.bazel edit, L2-W5g).
fn fuzz_module(flavor: bool) -> String {
    format!(
        r#"module(name = "snapshot_fuzz")

lr = use_repo_rule("//:repos.bzl", "local_rule")
lr(name = "localrepo")

gr = use_repo_rule("//:repos.bzl", "gen_rule")
gr(name = "genrepo"{})

er = use_repo_rule("//environ:repos.bzl", "environ_rule")
er(name = "envrepo")

bazel_dep(name = "localdep", version = "1.0")
local_path_override(module_name = "localdep", path = "../localdep")

ext = use_extension("//ext:ext.bzl", "ext")
use_repo(ext, "extrepo")
"#,
        if flavor { r#", flavor = "spicy""# } else { "" }
    )
}

const FUZZ_REPOS: &str = r##"def _local(ctx):
    ctx.file("BUILD.bazel", "exports_files(['value'])\n")
    ctx.file("value", "value=%s\n" % ctx.getenv("W5_LOCAL", "unset"))

local_rule = repository_rule(implementation = _local, local = True)

def _gen(ctx):
    ctx.file("BUILD.bazel", "exports_files(['sub/data.txt', 'tool.sh'])\nfilegroup(name = 'all', srcs = glob(['sub/**']), visibility = ['//visibility:public'])\n")
    ctx.file("sub/data.txt", "generated %s\n" % ctx.attr.flavor)
    ctx.file("sub/deeper/more.txt", "more\n")
    ctx.file("tool.sh", "#!/bin/sh\necho tool-%s > $1\n" % ctx.attr.flavor, executable = True)

gen_rule = repository_rule(implementation = _gen, attrs = {"flavor": attr.string(default = "plain")})
"##;

/// The fuzz's root BUILD file: the fixture's (the generated repository's
/// nested file) plus a consumer of the repository's `glob(["sub/**"])` and
/// one of its tool; `extra` adds a rule (a BUILD edit), `failing` makes the
/// test fail.
fn fuzz_build(extra: bool, failing: bool) -> String {
    let mut build = ROOT_BUILD.replace("@genrepo//:data.txt", "@genrepo//:sub/data.txt");
    if failing {
        build = build.replace("cat all.txt\\n", "cat all.txt\\nexit 1\\n");
    }
    build.push_str(
        "\ngenrule(name = 'genall', srcs = ['@genrepo//:all'], outs = ['genall.txt'], cmd = 'cat $(SRCS) > $@')\n",
    );
    // A consumer of the generated repository's tool (L2-W5h: the `on`
    // tamper class makes the tool non-executable).
    build.push_str(
        "\ngenrule(name = 'tooluse', srcs = ['@genrepo//:tool.sh'], outs = ['tooluse.txt'], cmd = '$(location @genrepo//:tool.sh) $@')\n",
    );
    if extra {
        build.push_str("\ngenrule(name = 'extra', outs = ['extra.txt'], cmd = 'echo > $@')\n");
    }
    build
}

/// `pkg/BUILD.bazel`; `failing`: its genrule fails.
fn fuzz_pkg_build(failing: bool) -> String {
    format!(
        "exports_files(['in.txt', 'link.txt'])\ngenrule(name = 'l', srcs = ['link.txt'], outs = ['l.txt'], cmd = '{}')\n",
        if failing { "exit 1" } else { "cp $< $@" }
    )
}

/// The `local_path_override` module beside the workspace: its BUILD file
/// loads a `.bzl` file of its own.
fn localdep_build(variant: usize) -> String {
    format!(
        "load(':defs.bzl', 'WORD')\ngenrule(name = 'l', outs = ['l.txt'], cmd = 'echo localdep-{variant}-' + WORD + ' > $@', visibility = ['//visibility:public'])\n"
    )
}

/// The mutation fuzz's workspace: the fixture with a nested generated
/// repository (a verified store tree to tamper with, globbed by an action),
/// a `--repo_env` repository, a `local_path_override` module beside the
/// workspace (BUILD, `.bzl` and MODULE.bazel), a module extension that
/// reads an environment variable and a watched file, a status command,
/// file, directory, package and `.bzl` symlinks, `.bazelrc`,
/// `.bazelignore` and `REPO.bazel`.
/// The fuzz's `mapped` package: the portable map_each rule only (its
/// unportable twin would keep `--nobuild //...` from ever saving).
fn fuzz_mapped_build(q: bool) -> String {
    format!(
        "load(':defs.bzl', 'mapped')\nmapped(name = 'm', names = 'names.txt', prefix = '{}')\n",
        if q { "q" } else { "p" }
    )
}

fn fuzz_fixture() -> Twin {
    let twin = fixture();
    twin.write("MODULE.bazel", &fuzz_module(false));
    twin.write("repos.bzl", FUZZ_REPOS);
    twin.write("BUILD.bazel", &fuzz_build(false, false));
    twin.write("environ/repos.bzl", ENVIRON_REPOS);
    twin.write(
        "environ/BUILD.bazel",
        "genrule(name = 'g', srcs = ['@envrepo//:v.txt'], outs = ['g.txt'], cmd = 'cp $< $@')\n",
    );
    twin.write_beside(
        "localdep/MODULE.bazel",
        "module(name = 'localdep', version = '1.0')\n",
    );
    twin.write_beside("localdep/BUILD.bazel", &localdep_build(0));
    twin.write_beside("localdep/defs.bzl", "WORD = 'plain'\n");
    twin.write(
        "deps/BUILD.bazel",
        "genrule(name = 'd', srcs = ['@localdep//:l', '@extrepo//:v.txt', '//pkg:in.txt'], outs = ['d.txt'], cmd = 'cat $(SRCS) > $@')\n",
    );
    // A module extension (getenv + a watched file): the workspace has a
    // lockfile to edit, corrupt and delete.
    twin.write("ext/ext.bzl", INPUT_EXTENSION);
    twin.write("ext/input.txt", "first\n");
    twin.write("ext/BUILD.bazel", "exports_files(['input.txt'])\n");
    twin.write("pkg/other.txt", "other\n");
    twin.write("pkg/BUILD.bazel", &fuzz_pkg_build(false));
    // Portable Args map_each functions over a tree artifact (P1).
    twin.write("mapped/defs.bzl", MAPPED_RULES);
    twin.write("mapped/consts.bzl", "SUFFIX = 's0'\n");
    twin.write("mapped/names.txt", "a.txt b.txt\n");
    twin.write("mapped/BUILD.bazel", &fuzz_mapped_build(false));
    twin.each(|root| std::os::unix::fs::symlink("in.txt", root.join("ws/pkg/link.txt")).unwrap());
    // Symlink swaps (L2-W5g): a package directory, a BUILD file, a `.bzl`.
    for (variant, word) in [("a", "alpha"), ("b", "beta")] {
        twin.write(
            &format!("swap_{variant}/BUILD.bazel"),
            &format!(
                "load('//:lib.bzl', 'VALUE')\ngenrule(name = 's', outs = ['s.txt'], cmd = 'echo {word}-' + VALUE + ' > $@')\n"
            ),
        );
        twin.write(
            &format!("lib_{variant}.bzl"),
            &format!("VALUE = '{word}'\n"),
        );
        twin.write(
            &format!("pkgl/BUILD.{variant}"),
            &format!("genrule(name = 'x', outs = ['x.txt'], cmd = 'echo {word} > $@')\n"),
        );
    }
    twin.each(|root| {
        let ws = root.join("ws");
        std::os::unix::fs::symlink("swap_a", ws.join("swap")).unwrap();
        std::os::unix::fs::symlink("lib_a.bzl", ws.join("lib.bzl")).unwrap();
        std::os::unix::fs::symlink("BUILD.a", ws.join("pkgl/BUILD.bazel")).unwrap();
    });
    twin.write("stable.txt", "one\n");
    twin.each(|root| {
        write_executable_script(
            &root.join("ws/status.sh"),
            "#!/bin/sh\necho STABLE_W5 $(cat stable.txt)\n[ -e noisy ] && echo 'status: noisy' >&2\nexit 0\n",
            0o755,
        );
    });
    twin.write(".bazelrc", "");
    twin.write(".bazelignore", "");
    twin.write("REPO.bazel", "");
    // Incremental analysis (docs/INCREMENTAL_ANALYSIS.md): a package whose
    // edits splice, prune or fall back, and its consumer.
    twin.write("incr/defs.bzl", INCR_RULES);
    twin.write("incr/srcs/a.txt", "a\n");
    twin.write("incr/b.txt", "b\n");
    twin.write("incr/BUILD.bazel", &fuzz_incr_build(&FuzzState::default()));
    twin.write("incr_use/BUILD.bazel", INCR_USE_BUILD);
    twin.write("incr_t/BUILD.bazel", &incr_t_build(0, 0));
    twin.write("incr_m/macros.bzl", &incr_macros("one", false));
    twin.write("incr_m/BUILD.bazel", INCR_M_BUILD);
    twin.workspace_rc.set(true);
    twin
}

/// A Starlark rule without prints for the incremental-analysis packages:
/// its action concatenates its sources and its dependencies' files and
/// appends `flag`.
const INCR_RULES: &str = r##"def _chain(ctx):
    out = ctx.actions.declare_file(ctx.label.name + ".txt")
    inputs = list(ctx.files.srcs)
    for dep in ctx.attr.deps:
        inputs.extend(dep[DefaultInfo].files.to_list())
    ctx.actions.run_shell(
        outputs = [out],
        inputs = inputs,
        command = "cat {} > {} && echo {} >> {}".format(" ".join([f.path for f in inputs]) or "/dev/null", out.path, ctx.attr.flag, out.path),
    )
    return [DefaultInfo(files = depset([out]))]

_CHAIN_ATTRS = {
    "srcs": attr.label_list(allow_files = True),
    "deps": attr.label_list(),
    "flag": attr.string(),
}

chain = rule(implementation = _chain, attrs = _CHAIN_ATTRS)

def _chain_exe(ctx):
    out = ctx.actions.declare_file(ctx.label.name + ".sh")
    inputs = list(ctx.files.srcs)
    for dep in ctx.attr.deps:
        inputs.extend(dep[DefaultInfo].files.to_list())
    ctx.actions.write(out, "#!/bin/bash\necho {}\n".format(ctx.attr.flag), is_executable = True)
    return [DefaultInfo(executable = out, files = depset([out]), runfiles = ctx.runfiles(files = inputs))]

chain_test = rule(implementation = _chain_exe, test = True, attrs = _CHAIN_ATTRS)

chain_bin = rule(implementation = _chain_exe, executable = True, attrs = _CHAIN_ATTRS)
"##;

const INCR_USE_BUILD: &str = "load('//incr:defs.bzl', 'chain')\nchain(name = 'u', deps = ['//incr:a'], flag = 'u')\nchain(name = 'v', flag = 'v', visibility = ['//visibility:public'])\n";

/// `incr/BUILD.bazel` of the fuzz state: the flag of `:a` (an attribute
/// edit: spliced), comments (pruned), a dependency on `//incr_use:v` (the
/// graph changes: falls back), an extra target (the patterns' expansion
/// changes: falls back), a print (falls back), `:b`'s deprecation (an
/// analysis warning: falls back), `:a`'s visibility (an analysis error).
fn fuzz_incr_build(state: &FuzzState) -> String {
    let mut build = String::from("load(':defs.bzl', 'chain')\n");
    if state.on("incr-print") {
        build.push_str("print('incr loaded')\n");
    }
    build.push_str(&format!(
        "chain(name = 'a', srcs = glob(['srcs/*.txt']), deps = [':b'{}], flag = 'f{}', visibility = ['{}'])\n",
        if state.on("incr-dep") {
            ", '//incr_use:v'"
        } else {
            ""
        },
        state.incr_flag,
        if state.on("incr-private") {
            "//visibility:private"
        } else {
            "//visibility:public"
        },
    ));
    build.push_str(&format!(
        "chain(name = 'b', srcs = ['b.txt', '@genrepo//:sub/data.txt'], flag = 'b'{})\n",
        if state.on("incr-deprecated") {
            ", deprecation = 'old'"
        } else {
            ""
        }
    ));
    if state.on("incr-extra") {
        build.push_str("chain(name = 'extra', flag = 'x')\n");
    }
    for comment in 0..state.incr_comments {
        build.push_str(&format!("# comment {comment}\n"));
    }
    build
}

/// The published store tree of the generated repository, if fetched (by
/// its BUILD file: the tree may be damaged).
fn fuzz_tree(root: &Path) -> Option<PathBuf> {
    fs::read_dir(root.join("cache/repositories/direct/trees"))
        .ok()?
        .flatten()
        .map(|entry| entry.path())
        .find(|path| {
            fs::read_to_string(path.join("BUILD.bazel"))
                .is_ok_and(|build| build.contains("sub/data.txt"))
        })
}

/// Every published tree of the generated repository (a flavor change
/// leaves the other flavor's tree behind).
fn fuzz_trees(root: &Path) -> Vec<PathBuf> {
    fs::read_dir(root.join("cache/repositories/direct/trees"))
        .map(|entries| {
            entries
                .flatten()
                .map(|entry| entry.path())
                .filter(|path| {
                    fs::read_to_string(path.join("BUILD.bazel"))
                        .is_ok_and(|build| build.contains("sub/data.txt"))
                })
                .collect()
        })
        .unwrap_or_default()
}

/// Retarget the symlink `link` (below the workspace) of both copies.
fn retarget(twin: &Twin, link: &str, target: &str) {
    twin.each(|root| {
        let link = root.join("ws").join(link);
        fs::remove_file(&link).unwrap();
        std::os::unix::fs::symlink(target, &link).unwrap();
    });
}

/// Write `content` at `path` in both copies with the modification time
/// `offset` seconds from now (a future or backdated stamp, L2-W5g).
fn write_stamped(twin: &Twin, path: &str, content: &str, offset: i64) {
    let when = if offset >= 0 {
        std::time::SystemTime::now() + std::time::Duration::from_secs(offset as u64)
    } else {
        std::time::SystemTime::now() - std::time::Duration::from_secs(offset.unsigned_abs())
    };
    twin.each(|root| {
        let path = root.join("ws").join(path);
        fs::write(&path, content).unwrap();
        fs::File::options()
            .write(true)
            .open(&path)
            .unwrap()
            .set_modified(when)
            .unwrap();
    });
}

/// What the fuzz holds besides the files: the client environment and
/// the options of the next commands.
#[derive(Default)]
struct FuzzState {
    local: usize,
    inherited: usize,
    repo_env: usize,
    extension_env: usize,
    path: usize,
    define: Option<usize>,
    /// `--lockfile_mode`: update (the default, `None`), refresh, error or
    /// off (`Some(1..=3)`).
    lockfile_mode: Option<usize>,
    /// `--action_env=W5_INHERITED=<n>` instead of the inherited value.
    action_env: Option<usize>,
    test_env: Option<usize>,
    toggles: std::collections::BTreeMap<&'static str, bool>,
    added: usize,
    /// The lockfile was corrupted this step (deleted after it).
    corrupt: bool,
    /// Files tampered with this step: restored after it unless a full
    /// analysis repaired them (a command that failed before fetching the
    /// tree leaves it damaged; later `on` hits would then differ by the
    /// documented deviation).
    tampered: Vec<(PathBuf, Vec<u8>, u32)>,
    /// `incr/BUILD.bazel` (incremental analysis): `:a`'s flag, the
    /// comments appended, the globbed files added.
    incr_flag: usize,
    incr_comments: usize,
    incr_added: usize,
    /// `incr_t/BUILD.bazel`: the flag and the comments.
    incr_t_flag: usize,
    incr_t_comments: usize,
    /// `incr_m/macros.bzl`: the macro's flag.
    incr_m_flag: usize,
}

impl FuzzState {
    fn toggle(&mut self, name: &'static str) -> bool {
        let value = self.toggles.entry(name).or_default();
        *value = !*value;
        *value
    }

    fn on(&self, name: &str) -> bool {
        self.toggles.get(name).copied().unwrap_or_default()
    }

    fn env(&self) -> Vec<(&'static str, String)> {
        let mut env = vec![
            ("W5_LOCAL", ["a", "b"][self.local % 2].to_owned()),
            ("W5_INHERITED", ["x", "y"][self.inherited % 2].to_owned()),
            ("W5_EXT", ["one", "two"][self.extension_env % 2].to_owned()),
        ];
        match self.repo_env % 3 {
            0 => env.push(("W5X", "one".to_owned())),
            1 => env.push(("W5X", "two".to_owned())),
            _ => {}
        }
        if self.path % 2 == 1 {
            env.push(("PATH", "/bin:/usr/bin".to_owned()));
        }
        env
    }
}

/// What a step does besides its command.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Special {
    None,
    /// The command twice at once on the snapshot side.
    Concurrent,
    /// The command and another at once on the snapshot side (with stand-in
    /// snapshots, so a save collects: GC and refresh races).
    ConcurrentOther,
    /// A store tree tampered with (bytes or mode) under non-strict verify:
    /// a hit must log `first=taint` (L2-W5g).
    Tamper,
    /// Under `on` (L2-W5h): the consumer of the generated repository's tool
    /// is warmed, the tool made non-executable in both copies, the consumer
    /// run: when the lookup hits, exactly this step differs (the hit fails,
    /// the run without the snapshot repairs the tree), with exactly one
    /// heal naming the tool's mode; the next run is equal again.
    TamperOn,
}

/// The number of mutations of the catalogue ([`mutate`]).
const MUTATIONS: usize = 67;

/// Apply one random mutation of the catalogue (the same to both copies);
/// returns its name and what the step does besides its command.
fn mutate(twin: &Twin, rng: &mut Rng, state: &mut FuzzState, step: usize) -> (String, Special) {
    // `BAZEL_RS_SNAPSHOT_FUZZ_MUTATIONS=29,48,49`: only these (a debugging
    // aid; the default is the whole catalogue).
    let only: Vec<usize> = std::env::var("BAZEL_RS_SNAPSHOT_FUZZ_MUTATIONS")
        .map(|list| {
            list.split(',')
                .filter_map(|n| n.trim().parse().ok())
                .collect()
        })
        .unwrap_or_default();
    let choice = if only.is_empty() {
        rng.below(MUTATIONS)
    } else {
        only[rng.below(only.len())]
    };
    let name = |text: &str| format!("#{choice} {text}");
    let mut special = Special::None;
    let name = match choice {
        0 => {
            twin.write("pkg/in.txt", &format!("input {step}\n"));
            name("edit pkg/in.txt")
        }
        1 => {
            twin.write("pkg/in.txt", &format!("input {:02}\n", step % 100));
            name("same-size edit pkg/in.txt")
        }
        2 => {
            // A same-size edit that restores the modification time.
            let flip = state.toggle("mtime");
            twin.each(|root| {
                let path = root.join("ws/pkg/BUILD.bazel");
                let text = fs::read_to_string(&path).unwrap();
                let modified = fs::metadata(&path).unwrap().modified().unwrap();
                let text = text.trim_end_matches(['#', 'a', 'b', '\n']).to_owned();
                fs::write(
                    &path,
                    format!("{text}\n#{}\n", if flip { 'a' } else { 'b' }),
                )
                .unwrap();
                fs::File::options()
                    .write(true)
                    .open(&path)
                    .unwrap()
                    .set_modified(modified)
                    .unwrap();
            });
            name("mtime-restoring edit pkg/BUILD.bazel")
        }
        3 => {
            state.added += 1;
            twin.write(&format!("data/f{}.txt", state.added), "added\n");
            name("add a globbed data file")
        }
        4 => {
            if state.added > 0 {
                let added = state.added;
                twin.each(|root| {
                    let _ = fs::remove_file(root.join(format!("ws/data/f{added}.txt")));
                });
                state.added -= 1;
            }
            name("delete a globbed data file")
        }
        5 => {
            twin.write(&format!("data/.f{step}.txt.swp"), "swap");
            name("add a swap file")
        }
        6 => {
            let extra = state.toggle("extra-rule");
            twin.write("BUILD.bazel", &fuzz_build(extra, state.on("failing-test")));
            name("BUILD edit")
        }
        7 => {
            let loud = state.toggle("rules-print");
            twin.write(
                "rules.bzl",
                &if loud {
                    RULES.replace("rules.bzl loaded", "rules.bzl loaded (edited)")
                } else {
                    RULES.to_owned()
                },
            );
            name(".bzl edit")
        }
        8 => {
            twin.write(
                &format!("newpkg{step}/BUILD.bazel"),
                "genrule(name = 'n', outs = ['n.txt'], cmd = 'echo > $@')\n",
            );
            name("new package")
        }
        9 => {
            twin.each(|root| {
                for entry in fs::read_dir(root.join("ws")).unwrap().flatten() {
                    if entry.file_name().to_string_lossy().starts_with("newpkg") {
                        fs::remove_dir_all(entry.path()).unwrap();
                    }
                }
            });
            name("remove the new packages")
        }
        10 => {
            twin.write(&format!("other/extra{step}.go"), "package other\n");
            name("add a .go file")
        }
        11 => {
            let other = state.toggle("link");
            retarget(
                twin,
                "pkg/link.txt",
                if other { "other.txt" } else { "in.txt" },
            );
            name("retarget a symlink")
        }
        12 => {
            let ignore = state.toggle("bazelignore");
            twin.write(".bazelignore", if ignore { "other\n" } else { "" });
            name(".bazelignore edit")
        }
        13 => {
            let ignore = state.toggle("repo-file");
            twin.write(
                "REPO.bazel",
                if ignore {
                    "ignore_directories(['other'])\n"
                } else {
                    "ignore_directories([])\n"
                },
            );
            name("REPO.bazel ignore_directories edit")
        }
        14 => {
            state.local += 1;
            name("W5_LOCAL (a local rule's input)")
        }
        15 => {
            state.inherited += 1;
            name("W5_INHERITED (--action_env)")
        }
        16 => {
            state.repo_env += 1;
            name("W5X (--repo_env re-inherited)")
        }
        17 => {
            state.define = match state.define {
                Some(_) => None,
                None => Some(step % 3),
            };
            name("--define")
        }
        18 => {
            let rc = state.toggle("bazelrc");
            twin.write(".bazelrc", if rc { "build --define=rc=1\n" } else { "" });
            name(".bazelrc edit")
        }
        19 => {
            // Semantic (L2-W5g): the generated repository's attribute.
            let flavor = state.toggle("module");
            twin.write("MODULE.bazel", &fuzz_module(flavor));
            name("MODULE.bazel edit (genrepo's flavor)")
        }
        20 => {
            // Semantic (L2-W5g): the extension's generated repository in
            // the lockfile, its digests kept (still valid JSON).
            twin.each(|root| {
                let path = root.join("ws/MODULE.bazel.lock");
                if let Ok(text) = fs::read_to_string(&path)
                    && let Some(at) = text.find("\"content\": \"")
                {
                    let at = at + "\"content\": \"".len();
                    fs::write(
                        &path,
                        format!("{}edited-{step}-{}", &text[..at], &text[at..]),
                    )
                    .unwrap();
                }
            });
            name("lockfile edit (the extension repository's content)")
        }
        21 => {
            twin.each(|root| {
                let _ = fs::remove_file(root.join("ws/MODULE.bazel.lock"));
            });
            name("lockfile deleted")
        }
        22 => {
            twin.each(|root| {
                if let Some(tree) = fuzz_tree(root) {
                    let _ = fs::remove_file(tree.join("sub/data.txt"));
                }
            });
            name("store tree: a file deleted")
        }
        23 => {
            twin.each(|root| {
                if let Some(tree) = fuzz_tree(root) {
                    let _ = fs::remove_dir_all(tree.join("sub/deeper"));
                }
            });
            name("store tree: a directory deleted")
        }
        24 => {
            twin.each(|root| {
                if let Some(tree) = fuzz_tree(root) {
                    let _ = fs::write(tree.join(format!("sub/added{step}.txt")), "added\n");
                }
            });
            name("store tree: a file added")
        }
        25 => {
            twin.write("stable.txt", &format!("{step}\n"));
            name("status command output")
        }
        26 => {
            if state.toggle("noisy") {
                twin.write("noisy", "");
            } else {
                twin.each(|root| {
                    let _ = fs::remove_file(root.join("ws/noisy"));
                });
            }
            name("status command stderr toggled")
        }
        27 => {
            if let Some(file) = twin.snapshots().first() {
                let mut bytes = fs::read(file).unwrap();
                let at = (step * 7919) % bytes.len();
                bytes[at] ^= 0x5a;
                fs::write(file, bytes).unwrap();
            }
            name("a snapshot damaged")
        }
        28 => {
            for file in twin.snapshots() {
                let _ = fs::remove_file(file);
            }
            name("the snapshots deleted")
        }
        29 => {
            special = Special::Concurrent;
            name("concurrent runs")
        }
        30 => {
            // L2-W5g: the override module's BUILD, `.bzl` or MODULE.bazel.
            match step % 3 {
                0 => twin.write_beside("localdep/BUILD.bazel", &localdep_build(step)),
                1 => twin.write_beside("localdep/defs.bzl", &format!("WORD = 'w{step}'\n")),
                _ => twin.write_beside(
                    "localdep/MODULE.bazel",
                    &if state.toggle("localdep-module") {
                        "module(name = 'localdep', version = '1.0', compatibility_level = 1)\n"
                            .to_owned()
                    } else {
                        "module(name = 'localdep', version = '1.0')\n".to_owned()
                    },
                ),
            }
            name("local_path_override module edit (BUILD, .bzl or MODULE.bazel)")
        }
        31 => {
            std::thread::sleep(std::time::Duration::from_millis(2100));
            name("settle (2 s)")
        }
        32 => {
            state.extension_env += 1;
            name("W5_EXT (the extension's getenv)")
        }
        33 => {
            twin.write("ext/input.txt", &format!("input {step}\n"));
            name("the extension's watched file")
        }
        34 => {
            twin.each(|root| {
                let path = root.join("ws/MODULE.bazel.lock");
                if path.exists() {
                    fs::write(&path, "{ \"lockFileVersion\": ").unwrap();
                }
            });
            state.corrupt = true;
            name("lockfile corrupted (deleted after the step)")
        }
        35 => {
            // Another mode, then back to the default.
            state.lockfile_mode = match state.lockfile_mode {
                Some(_) => None,
                None => Some(1 + step % 3),
            };
            name("--lockfile_mode")
        }
        36 => {
            state.toggle("pinned-repo-env");
            name("--repo_env=W5X=pinned set/unset")
        }
        37 => {
            state.action_env = match state.action_env {
                Some(_) => None,
                None => Some(step % 2),
            };
            name("--action_env=W5_INHERITED=<value>")
        }
        38 => {
            state.test_env = match state.test_env {
                Some(_) => None,
                None => Some(step % 2),
            };
            name("--test_env")
        }
        39 => {
            state.toggle("strict-env");
            name("--incompatible_strict_action_env")
        }
        40 => {
            state.path += 1;
            name("PATH")
        }
        41 => {
            let b = state.toggle("swap-dir");
            retarget(twin, "swap", if b { "swap_b" } else { "swap_a" });
            name("package directory symlink swapped")
        }
        42 => {
            let b = state.toggle("swap-bzl");
            retarget(twin, "lib.bzl", if b { "lib_b.bzl" } else { "lib_a.bzl" });
            name(".bzl symlink swapped")
        }
        43 => {
            let b = state.toggle("swap-build");
            retarget(
                twin,
                "pkgl/BUILD.bazel",
                if b { "BUILD.b" } else { "BUILD.a" },
            );
            name("BUILD file symlink swapped")
        }
        44 => {
            write_stamped(twin, "pkg/in.txt", &format!("future {step}\n"), 86_400);
            name("edit pkg/in.txt with a future mtime")
        }
        45 => {
            let flip = state.toggle("backdated");
            write_stamped(
                twin,
                "pkg/other.txt",
                if flip { "backdated\n" } else { "other\n" },
                -366 * 86_400,
            );
            name("edit pkg/other.txt with a backdated mtime")
        }
        46 => {
            let failing = state.toggle("failing-build");
            twin.write("pkg/BUILD.bazel", &fuzz_pkg_build(failing));
            name("a failing genrule toggled (//pkg:l)")
        }
        47 => {
            let failing = state.toggle("failing-test");
            twin.write("BUILD.bazel", &fuzz_build(state.on("extra-rule"), failing));
            name("a failing test toggled (//:pass_test)")
        }
        48 => {
            special = Special::ConcurrentOther;
            name("concurrent different commands (GC stand-ins)")
        }
        49 => {
            // Bytes or mode of a file the hit's execution reads (verify
            // mode only: under `on` the documented deviation).
            let tampered = std::cell::RefCell::new(Vec::new());
            let sides = std::cell::Cell::new(0);
            twin.each(|root| {
                let mut any = false;
                for tree in fuzz_trees(root) {
                    let file = tree.join("sub/data.txt");
                    let (Ok(metadata), Ok(bytes)) = (fs::metadata(&file), fs::read(&file)) else {
                        continue;
                    };
                    any = true;
                    let mode = metadata.permissions().mode();
                    tampered.borrow_mut().push((file.clone(), bytes, mode));
                    if step.is_multiple_of(2) {
                        let modified = metadata.modified().unwrap();
                        fs::set_permissions(&file, fs::Permissions::from_mode(0o644)).unwrap();
                        fs::write(&file, "TAMPERED\n").unwrap();
                        let handle = fs::File::options().write(true).open(&file).unwrap();
                        handle.set_modified(modified).unwrap();
                        drop(handle);
                        fs::set_permissions(&file, fs::Permissions::from_mode(mode)).unwrap();
                    } else {
                        fs::set_permissions(&file, fs::Permissions::from_mode(mode ^ 0o100))
                            .unwrap();
                    }
                }
                sides.set(sides.get() + usize::from(any));
            });
            state.tampered = tampered.into_inner();
            if sides.get() == 2 {
                special = Special::Tamper;
            }
            name("store tree tampered (bytes or mode; verify mode)")
        }
        50 => {
            // L2-W5h: the tool of the generated repository made
            // non-executable under `on` (the step tampers after warming).
            special = Special::TamperOn;
            name("store tree tool made non-executable under on (a failing consumer)")
        }
        51 => {
            // P1: the tree's files change at execution (a hit's map_each
            // functions map the new files).
            twin.write("mapped/names.txt", &format!("a.txt b{step}.txt\n"));
            name("edit mapped/names.txt (map_each over other tree files)")
        }
        52 => {
            twin.write("mapped/consts.bzl", &format!("SUFFIX = 's{step}'\n"));
            name("edit mapped/consts.bzl (a global of the map_each functions)")
        }
        53 => {
            // Every function of the module moves: a description of the old
            // spans must never resolve.
            let moved = state.toggle("mapped-moved");
            let rules = if moved {
                format!("# moved\n{MAPPED_RULES}")
            } else {
                MAPPED_RULES.to_owned()
            };
            twin.write("mapped/defs.bzl", &rules);
            name("move the map_each functions in mapped/defs.bzl")
        }
        54 => {
            let q = state.toggle("mapped-prefix");
            twin.write("mapped/BUILD.bazel", &fuzz_mapped_build(q));
            name("edit a captured value of the map_each closures (mapped/BUILD.bazel)")
        }
        // Incremental analysis (docs/INCREMENTAL_ANALYSIS.md): BUILD,
        // `.bzl` and source edits of `incr/` that splice, prune or fall
        // back; each step's output must equal the full analysis's.
        56 => {
            state.incr_flag = (state.incr_flag + 1) % 3;
            twin.write("incr/BUILD.bazel", &fuzz_incr_build(state));
            name("incr: an attribute edit (spliced)")
        }
        57 => {
            state.incr_comments = (state.incr_comments + 1) % 4;
            twin.write("incr/BUILD.bazel", &fuzz_incr_build(state));
            name("incr: a comment edit (pruned)")
        }
        58 => {
            state.toggle("incr-dep");
            twin.write("incr/BUILD.bazel", &fuzz_incr_build(state));
            name("incr: a dependency toggled (the graph changes)")
        }
        59 => {
            state.toggle("incr-extra");
            twin.write("incr/BUILD.bazel", &fuzz_incr_build(state));
            name("incr: a target added or removed (the patterns change)")
        }
        60 => {
            state.toggle("incr-print");
            twin.write("incr/BUILD.bazel", &fuzz_incr_build(state));
            name("incr: a BUILD print toggled")
        }
        61 => {
            state.toggle(if step.is_multiple_of(2) {
                "incr-deprecated"
            } else {
                "incr-private"
            });
            twin.write("incr/BUILD.bazel", &fuzz_incr_build(state));
            name("incr: a deprecation or the visibility toggled")
        }
        62 => {
            if step.is_multiple_of(3) && state.incr_added > 0 {
                let added = state.incr_added;
                twin.each(|root| {
                    let _ = fs::remove_file(root.join(format!("ws/incr/srcs/f{added}.txt")));
                });
                state.incr_added -= 1;
                name("incr: a globbed source removed")
            } else {
                state.incr_added += 1;
                twin.write(&format!("incr/srcs/f{}.txt", state.incr_added), "f\n");
                name("incr: a globbed source added")
            }
        }
        63 => {
            if state.toggle("incr-b-missing") {
                twin.each(|root| {
                    let _ = fs::remove_file(root.join("ws/incr/b.txt"));
                });
                name("incr: an explicit source deleted")
            } else {
                twin.write("incr/b.txt", "b\n");
                name("incr: an explicit source restored")
            }
        }
        64 => {
            let edited = state.toggle("incr-bzl");
            twin.write(
                "incr/defs.bzl",
                &if edited {
                    INCR_RULES.replace("out.path, ctx.attr.flag", "out.path, ctx.attr.flag + '!'")
                } else {
                    INCR_RULES.to_owned()
                },
            );
            name("incr: a .bzl edit")
        }
        65 => {
            if rng.percent(50) {
                state.incr_t_flag = (state.incr_t_flag + 1) % 3;
                twin.write(
                    "incr_t/BUILD.bazel",
                    &incr_t_build(state.incr_t_flag, state.incr_t_comments),
                );
                name("incr_t: the tests' and tool's flag (spliced)")
            } else {
                state.incr_t_comments = (state.incr_t_comments + 1) % 3;
                twin.write(
                    "incr_t/BUILD.bazel",
                    &incr_t_build(state.incr_t_flag, state.incr_t_comments),
                );
                name("incr_t: a comment (pruned)")
            }
        }
        66 => {
            let flags = ["one", "uno", "eins"];
            if rng.percent(50) {
                state.incr_m_flag = (state.incr_m_flag + 1) % flags.len();
                twin.write(
                    "incr_m/macros.bzl",
                    &incr_macros(flags[state.incr_m_flag], state.on("incr-m-comment")),
                );
                name("incr_m: the macro's body (its package changes)")
            } else {
                let comment = state.toggle("incr-m-comment");
                twin.write(
                    "incr_m/macros.bzl",
                    &incr_macros(flags[state.incr_m_flag], comment),
                );
                name("incr_m: a comment in the macro's .bzl (pruned)")
            }
        }
        _ => name("no change"),
    };
    (name, special)
}

/// The commands of the fuzz ([`fuzz_command`]).
const FUZZ_COMMANDS: usize = 16;
/// The command consuming the generated repository's tool
/// ([`Special::TamperOn`]).
const TOOL_COMMAND: usize = 10;

/// The fuzz's command `command` (below [`FUZZ_COMMANDS`]) and the
/// directory it runs in below the workspace (`None`: the root).
fn fuzz_command(
    twin: &Twin,
    command: usize,
    state: &FuzzState,
) -> (Vec<String>, Option<&'static str>) {
    let mut cwd = None;
    let mut args: Vec<String> = match command {
        0 => vec!["build".into(), "//:all_inputs".into(), "//:genall".into()],
        1 => vec!["build".into(), "--nobuild".into(), "//...".into()],
        2 => vec![
            "test".into(),
            "--test_output=all".into(),
            "//:pass_test".into(),
        ],
        3 => vec![
            "build".into(),
            match state.action_env {
                Some(value) => format!("--action_env=W5_INHERITED=fixed{value}"),
                None => "--action_env=W5_INHERITED".into(),
            },
            "//:env_echo".into(),
        ],
        4 => vec![
            "build".into(),
            format!(
                "--workspace_status_command={}",
                twin.ws().join("status.sh").display()
            ),
            "//:stamped".into(),
        ],
        5 => vec![
            "run".into(),
            format!(
                "--script_path={}",
                twin.run_root().join("home/run.sh").display()
            ),
            "//:tool".into(),
        ],
        6 => {
            let mut args = vec![
                "build".into(),
                "--repo_env=W5X=default".into(),
                "--repo_env=W5X".into(),
            ];
            if state.on("pinned-repo-env") {
                args.push("--repo_env=W5X=pinned".into());
            }
            args.push("//environ:g".into());
            args
        }
        7 => vec!["build".into(), "//deps:d".into(), "//pkg:l".into()],
        TOOL_COMMAND => vec!["build".into(), "//:tooluse".into()],
        8 => {
            // From a subpackage, with a relative pattern (L2-W5g).
            cwd = Some("pkg");
            vec!["build".into(), ":l".into()]
        }
        11 => vec!["build".into(), "//mapped:m".into()],
        12 => vec!["build".into(), "//incr_use:u".into(), "//incr:all".into()],
        13 => vec![
            "build".into(),
            "--nobuild".into(),
            "//incr/...".into(),
            "//incr_use/...".into(),
            "//incr_m/...".into(),
        ],
        14 => vec![
            "test".into(),
            "--test_output=all".into(),
            "//incr_t:all".into(),
        ],
        15 => vec![
            "run".into(),
            format!(
                "--script_path={}",
                twin.run_root().join("home/run_t.sh").display()
            ),
            "//incr_t:tool".into(),
        ],
        _ => vec!["build".into(), "//swap:s".into(), "//pkgl:x".into()],
    };
    let mut options = Vec::new();
    if let Some(define) = state.define {
        options.push(format!("--define=fz={define}"));
    }
    match state.lockfile_mode {
        Some(1) => options.push("--lockfile_mode=refresh".to_owned()),
        Some(2) => options.push("--lockfile_mode=error".to_owned()),
        Some(_) => options.push("--lockfile_mode=off".to_owned()),
        None => {}
    }
    if state.on("strict-env") {
        options.push("--incompatible_strict_action_env".to_owned());
    }
    if args[0] == "test"
        && let Some(value) = state.test_env
    {
        options.push(format!("--test_env=W5T=t{value}"));
    }
    args.splice(1..1, options);
    (args, cwd)
}

/// The step's outcome on the snapshot side, from EXPLAIN's lines or —
/// without EXPLAIN — the `PERF-SNAPSHOT` lines.
fn fuzz_outcome(pair: &Pair) -> &'static str {
    let perf = |prefix: &str| {
        text(&pair.on.stderr)
            .lines()
            .any(|line| line.starts_with(&format!("PERF-SNAPSHOT {prefix}")))
    };
    if pair.line("verified ").is_some() || perf("verify ") {
        "verified hit"
    } else if pair
        .explain()
        .iter()
        .any(|line| line.starts_with("verify ") && line.contains("MISMATCH"))
    {
        "verify mismatch"
    } else if pair.line("hit ").is_some() || perf("hit ") {
        "hit"
    } else if pair
        .explain()
        .iter()
        .any(|line| line.starts_with("incremental ") && line.contains(" re-analyzed;"))
        || perf("incremental dirty=")
    {
        "miss, incremental"
    } else if pair
        .explain()
        .iter()
        .any(|line| line.starts_with("incremental") && !line.contains(" re-analyzed;"))
    {
        "miss, incremental fell back"
    } else if pair.line("saved ").is_some() {
        "miss, saved"
    } else if perf("miss ") {
        "miss (no EXPLAIN)"
    } else {
        "miss"
    }
}

/// The `verify.log` lines of the snapshot side.
fn verify_log(twin: &Twin) -> Vec<String> {
    fs::read_to_string(twin.store(ON).join("verify.log"))
        .map(|text| text.lines().map(str::to_owned).collect())
        .unwrap_or_default()
}

/// L2-W5f, extended by L2-W5g: random sequences of ~50 kinds of mutation
/// (sources with same-size, mtime-restoring, future and backdated edits,
/// globs, BUILD and `.bzl` files, packages, file, directory, package and
/// `.bzl` symlink swaps, `.bazelignore`, `REPO.bazel`, the environment of
/// local rules and of the extension, its watched file, `--action_env`
/// inherited and set, `--repo_env` re-inherited and pinned, `--test_env`,
/// `--incompatible_strict_action_env` and `PATH`, options and `.bazelrc`,
/// semantic MODULE.bazel and lockfile edits, corrupt and deleted
/// lockfiles, `--lockfile_mode`, store-tree deletions and additions, the
/// status command, damaged or deleted snapshots, concurrent runs of one
/// command or of two (with stand-in snapshots: collections and refreshes
/// racing), a `local_path_override` module's BUILD, `.bzl` and
/// MODULE.bazel, failing builds and tests, and — in non-strict verify mode
/// — store-tree byte and mode tampering, which a hit must log as
/// `first=taint`) and 10 commands (`build`, `--nobuild //...`, `test`,
/// `run --script_path`, a relative pattern from a subpackage, ...), each
/// step run with the snapshot (`on`, or `verify` with the strict switch)
/// and without: every step byte-identical (the heal line aside), built
/// outputs and MODULE.bazel.lock included. About half the steps run
/// without `BAZEL_RS_SNAPSHOT_EXPLAIN` (the production path: the first
/// failing check cancels the rest of the attempt; the outcome comes from
/// the `PERF-SNAPSHOT` lines). `BAZEL_RS_SNAPSHOT_FUZZ_STEPS` (default 50)
/// and `BAZEL_RS_SNAPSHOT_FUZZ_SEED` (default: from the clock; printed) — a
/// failure prints the seed and the steps so far.
///
/// L2-W5h: no step may heal (a heal line on stderr is a difference, a
/// `healed` event fails the step) but the `on` tamper class's
/// ([`Special::TamperOn`]: the generated repository's tool made
/// non-executable after its consumer was warmed — when the lookup hits,
/// exactly that step differs, the failing hit, with exactly one heal naming
/// the tool's mode, and the next run is equal again).
///
/// Not in the catalogue under `on` (documented deviations, fixtures of
/// their own): an in-place byte edit of a store-tree file that does not
/// fail its consumer (a hit executes with the edited file,
/// `a_hit_hashes_an_edited_tree_file_by_its_change_time`) and a store-tree
/// runfile made unreadable (`a_store_input_staged_by_the_test_runner_heals`).
#[test]
fn mutation_fuzz() {
    let steps: usize = std::env::var("BAZEL_RS_SNAPSHOT_FUZZ_STEPS")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(50);
    let seed: u64 = std::env::var("BAZEL_RS_SNAPSHOT_FUZZ_SEED")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or_else(|| {
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos() as u64
                | 1
        });
    println!("mutation fuzz: seed {seed}, {steps} steps");
    let mut rng = Rng(seed);
    let twin = fuzz_fixture();
    let mut state = FuzzState::default();
    let mut log: Vec<String> = Vec::new();
    let mut counts = std::collections::BTreeMap::<&str, usize>::new();
    let mut previous: Option<usize> = None;
    let only_commands: Vec<usize> = std::env::var("BAZEL_RS_SNAPSHOT_FUZZ_COMMANDS")
        .map(|list| {
            list.split(',')
                .filter_map(|n| n.trim().parse().ok())
                .filter(|n| *n < FUZZ_COMMANDS)
                .collect()
        })
        .unwrap_or_default();
    for step in 0..steps {
        // Mostly a mutation, sometimes none; the command mostly the previous
        // one (hits need a repeated command), sometimes another.
        let (mutation, special) = if step == 0 {
            ("#- cold".to_owned(), Special::None)
        } else if rng.percent(70) {
            mutate(&twin, &mut rng, &mut state, step)
        } else {
            ("#- no change".to_owned(), Special::None)
        };
        let command = match previous {
            // A tampered tree is read by the default build's actions.
            _ if special == Special::Tamper => 0,
            // The tool's consumer (L2-W5h).
            _ if special == Special::TamperOn => TOOL_COMMAND,
            Some(command) if rng.percent(55) => command,
            // `BAZEL_RS_SNAPSHOT_FUZZ_COMMANDS=12,13`: only these (a
            // debugging aid, like the mutations').
            _ if !only_commands.is_empty() => only_commands[rng.below(only_commands.len())],
            _ => rng.below(FUZZ_COMMANDS),
        };
        previous = Some(command);
        let (args, cwd) = fuzz_command(&twin, command, &state);
        let mode = if special == Special::TamperOn {
            ON
        } else if special == Special::Tamper || rng.percent(30) {
            VERIFY
        } else {
            ON
        };
        twin.mode.set(mode);
        twin.strict.set(special != Special::Tamper);
        // The `on` tamper class reads the warm-up's outcome from EXPLAIN.
        let explain = rng.percent(50);
        twin.explain.set(explain || special == Special::TamperOn);
        *twin.cwd.borrow_mut() = cwd;
        let env_owned = state.env();
        let env: Vec<(&str, &str)> = env_owned
            .iter()
            .map(|(name, value)| (*name, value.as_str()))
            .collect();
        let argv: Vec<&str> = args.iter().map(String::as_str).collect();
        log.push(format!(
            "step {step}: {mutation}; {mode}{}{} {argv:?}{} {env:?}",
            if twin.strict.get() || mode == ON {
                ""
            } else {
                " (not strict)"
            },
            if twin.explain.get() {
                ""
            } else {
                " (no EXPLAIN)"
            },
            cwd.map(|cwd| format!(" in {cwd}")).unwrap_or_default(),
        ));
        let verify_lines = verify_log(&twin).len();
        // What the `on` tamper class saw (L2-W5h).
        let note = std::cell::Cell::new("");
        let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let pair = match special {
                Special::TamperOn => {
                    // Warm: the consumer's snapshot is saved (or hits).
                    let mut warm = twin.same(&argv, &env);
                    if warm.line("saved ").is_none() && warm.line("hit ").is_none() {
                        warm = twin.same(&argv, &env);
                    }
                    let warmed = warm.line("saved ").is_some() || warm.line("hit ").is_some();
                    // The tree of the current flavor (a MODULE.bazel edit
                    // leaves the other flavor's tree behind, unused).
                    let flavor = if state.on("module") { "spicy" } else { "plain" };
                    twin.each(|root| {
                        for tree in fuzz_trees(root) {
                            let tool = tree.join("tool.sh");
                            if fs::read_to_string(&tool)
                                .is_ok_and(|script| script.contains(&format!("tool-{flavor} ")))
                            {
                                fs::set_permissions(&tool, fs::Permissions::from_mode(0o644))
                                    .unwrap();
                            }
                        }
                    });
                    let failing = twin.run(&argv, &env);
                    assert!(
                        failing.off.status.success(),
                        "{argv:?}: the run without the snapshot repairs the tree: {}",
                        text(&failing.off.stderr)
                    );
                    if failing.line("hit ").is_some() {
                        // The one differing step: the hit executes the
                        // non-executable tool and fails; the failed hit's
                        // check names the tool's mode and removes the
                        // snapshot.
                        assert!(!failing.on.status.success(), "{argv:?}: the hit succeeded");
                        let stderr = text(&failing.on.stderr);
                        assert!(
                            stderr.contains(HEAL_LINE) && stderr.contains(" (mode tool.sh); "),
                            "{argv:?}: {stderr}"
                        );
                        let healed = twin.assert_healed(1);
                        assert!(
                            healed[0].contains(" reason=tree:mode ")
                                && healed[0].contains("/tool.sh ms="),
                            "{healed:?}"
                        );
                        note.set("tamper-on: failing hit healed");
                    } else {
                        // A miss repairs the tree like the run without the
                        // snapshot: equal, nothing healed.
                        failing.assert_same(&argv);
                        twin.assert_healed(0);
                        note.set(if warmed {
                            "tamper-on: miss (repaired)"
                        } else {
                            "tamper-on: not warmed"
                        });
                    }
                    // Then equal again: the next run repairs the tree.
                    let repaired = twin.same(&argv, &env);
                    repaired.assert_success();
                    repaired
                }
                Special::Concurrent => {
                    // Once in sequence (concurrent runs share the action
                    // cache: which of them executes an action first is a
                    // race), then twice at once on the snapshot side.
                    twin.same(&argv, &env);
                    let off = twin.run_side(OFF, &argv, &env);
                    let outputs = twin.with_side(ON, || {
                        let children: Vec<_> = (0..2)
                            .map(|_| {
                                twin.command(&argv, &env, true)
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
                    let mut pairs: Vec<Pair> = outputs
                        .into_iter()
                        .map(|on| Pair {
                            on,
                            off: off.clone(),
                            mode,
                        })
                        .collect();
                    for pair in &pairs {
                        pair.assert_same(&argv);
                    }
                    pairs.pop().unwrap()
                }
                Special::ConcurrentOther => {
                    // Another command (from the root), warmed like the
                    // first; STORE_KEEP - 1 stand-in snapshots make any save
                    // collect, racing the other's hit and its refresh.
                    let cwd = *twin.cwd.borrow();
                    *twin.cwd.borrow_mut() = None;
                    let (other, _) = fuzz_command(&twin, (command + 1) % 8, &state);
                    let other: Vec<&str> = other.iter().map(String::as_str).collect();
                    twin.same(&other, &env);
                    let other_off = twin.run_side(OFF, &other, &env);
                    *twin.cwd.borrow_mut() = cwd;
                    twin.same(&argv, &env);
                    let off = twin.run_side(OFF, &argv, &env);
                    let store = twin.store(ON).join("v1");
                    for n in 0..STORE_KEEP - 1 {
                        let _ = fs::write(
                            store.join(format!("0000000000000000000000000000{n:04}.snap")),
                            b"x",
                        );
                    }
                    let (mine, theirs) = twin.with_side(ON, || {
                        let mine = twin
                            .command(&argv, &env, true)
                            .stdout(std::process::Stdio::piped())
                            .stderr(std::process::Stdio::piped())
                            .spawn()
                            .unwrap();
                        let cwd = *twin.cwd.borrow();
                        *twin.cwd.borrow_mut() = None;
                        let theirs = twin
                            .command(&other, &env, true)
                            .stdout(std::process::Stdio::piped())
                            .stderr(std::process::Stdio::piped())
                            .spawn()
                            .unwrap();
                        *twin.cwd.borrow_mut() = cwd;
                        (
                            mine.wait_with_output().unwrap(),
                            theirs.wait_with_output().unwrap(),
                        )
                    });
                    for entry in fs::read_dir(&store).unwrap().flatten() {
                        if entry
                            .file_name()
                            .to_string_lossy()
                            .starts_with("0000000000000000")
                        {
                            let _ = fs::remove_file(entry.path());
                        }
                    }
                    Pair {
                        on: theirs,
                        off: other_off,
                        mode,
                    }
                    .assert_same(&other);
                    let pair = Pair {
                        on: mine,
                        off,
                        mode,
                    };
                    pair.assert_same(&argv);
                    pair
                }
                Special::None | Special::Tamper => {
                    let pair = twin.same(&argv, &env);
                    // What the build wrote (stdout's `<label>\t<path>` lines).
                    if argv[0] == "build" {
                        let outputs = |side: &str, output: &Output| -> Vec<(String, Vec<u8>)> {
                            let stdout = text(&output.stdout);
                            twin.with_side(side, || {
                                stdout
                                    .lines()
                                    .filter_map(|line| line.split_once('\t'))
                                    .map(|(_, path)| {
                                        (path.to_owned(), fs::read(path).unwrap_or_default())
                                    })
                                    .collect()
                            })
                        };
                        assert_eq!(
                            outputs(ON, &pair.on),
                            outputs(OFF, &pair.off),
                            "{argv:?}: the outputs differ"
                        );
                    }
                    pair
                }
            };
            // A hit writes no lockfile: it may only hit when the full
            // analysis would not have written one either.
            let lockfile = |side: &str| fs::read(twin.side(side).join("ws/MODULE.bazel.lock")).ok();
            assert!(
                lockfile(ON) == lockfile(OFF),
                "{argv:?}: MODULE.bazel.lock differs (on: {:?} bytes, off: {:?} bytes)",
                lockfile(ON).map(|bytes| bytes.len()),
                lockfile(OFF).map(|bytes| bytes.len())
            );
            // A tampered tree under verify mode: a hit's shadow full
            // analysis repairs it, which must be logged as a taint.
            if special == Special::Tamper && !fuzz_outcome(&pair).starts_with("miss") {
                let lines = verify_log(&twin);
                assert_eq!(lines.len(), verify_lines + 1, "{argv:?}: {lines:?}");
                assert!(
                    lines
                        .last()
                        .is_some_and(|line| line.contains(" first=taint ")
                            && line.contains(" taint=repository-rule-executed ")),
                    "{argv:?}: {lines:?}"
                );
            }
            // No heal but the `on` tamper class's own (L2-W5h).
            twin.assert_healed(0);
            pair
        }));
        *twin.cwd.borrow_mut() = None;
        for (file, bytes, mode) in std::mem::take(&mut state.tampered) {
            use std::os::unix::fs::MetadataExt;
            let current = fs::symlink_metadata(&file).map(|metadata| metadata.mode() & 0o7777);
            if fs::read(&file).ok().as_ref() != Some(&bytes) || current.ok() != Some(mode & 0o7777)
            {
                let _ = fs::set_permissions(&file, fs::Permissions::from_mode(0o644));
                let _ = fs::write(&file, &bytes);
                let _ = fs::set_permissions(&file, fs::Permissions::from_mode(mode));
            }
        }
        if std::mem::take(&mut state.corrupt) {
            twin.each(|root| {
                let _ = fs::remove_file(root.join("ws/MODULE.bazel.lock"));
            });
        }
        match outcome {
            Ok(pair) => {
                let what = fuzz_outcome(&pair);
                *counts.entry(what).or_default() += 1;
                log.last_mut().unwrap().push_str(&format!(" -> {what}"));
                if what == "miss, incremental fell back"
                    && let Some(reason) = pair
                        .explain()
                        .into_iter()
                        .find(|line| line.starts_with("incremental"))
                {
                    log.last_mut().unwrap().push_str(&format!(" [{reason}]"));
                }
                if !note.get().is_empty() {
                    *counts.entry(note.get()).or_default() += 1;
                    log.last_mut()
                        .unwrap()
                        .push_str(&format!(" ({})", note.get()));
                }
            }
            Err(panic) => {
                eprintln!(
                    "mutation fuzz failed (seed {seed}, BAZEL_RS_SNAPSHOT_FUZZ_SEED={seed}):\n{}",
                    log.join("\n")
                );
                std::panic::resume_unwind(panic);
            }
        }
    }
    println!("{}", log.join("\n"));
    println!("mutation fuzz: seed {seed}: {counts:?}");
}

/// Incremental analysis (docs/INCREMENTAL_ANALYSIS.md, tier A): a miss
/// whose changed inputs lie inside main-repository packages reloads them; a
/// comment keeps the saved plan, an attribute or a globbed or explicit
/// source edit re-analyzes the requested targets reaching the package and
/// splices them in; everything else falls back to the full analysis — the
/// output always the snapshot-less run's, and the next run a hit.
#[test]
fn incremental_analysis_splices_prunes_and_falls_back() {
    let twin = fixture();
    let mut state = FuzzState::default();
    // The generated repository's store tree is an input of //incr:b (its
    // reuse is carried across the miss, not checked again).
    twin.write("repos.bzl", FUZZ_REPOS);
    twin.write("BUILD.bazel", &fuzz_build(false, false));
    twin.write("incr/defs.bzl", INCR_RULES);
    twin.write("incr/srcs/a.txt", "a\n");
    twin.write("incr/b.txt", "b\n");
    twin.write("incr/BUILD.bazel", &fuzz_incr_build(&state));
    twin.write("incr_use/BUILD.bazel", INCR_USE_BUILD);
    twin.write("pkg/BUILD.bazel", &fuzz_pkg_build(false));
    twin.write("pkg/other.txt", "other\n");
    twin.each(|root| std::os::unix::fs::symlink("in.txt", root.join("ws/pkg/link.txt")).unwrap());
    let args = &["build", "//incr/...", "//incr_use/...", "//other:o"];
    twin.save(args, &[]);
    twin.same(args, &[]).assert_hit();
    let incremental = |rerun: usize| {
        let pair = twin.same(args, &[]);
        pair.assert_miss("incr").assert_success();
        let line = pair
            .explain()
            .into_iter()
            .find(|line| line.starts_with("incremental ") && line.contains(" re-analyzed;"))
            .unwrap_or_else(|| {
                panic!(
                    "expected an incremental analysis:\n{}",
                    pair.explain().join("\n")
                )
            });
        assert!(
            line.contains(&format!(" {rerun} requested target(s) re-analyzed")),
            "{line}"
        );
        if pair.mode == VERIFY {
            assert!(
                pair.line("verified ").is_some(),
                "{}",
                pair.explain().join("\n")
            );
        } else {
            pair.assert_saved();
        }
        twin.same(args, &[]).assert_hit().assert_success();
        pair
    };
    let fallback = |reason: &str| {
        let pair = twin.same(args, &[]);
        pair.assert_miss("incr").assert_saved();
        assert!(
            pair.explain()
                .iter()
                .any(|line| line.starts_with("incremental")
                    && !line.contains(" re-analyzed;")
                    && line.contains(reason)),
            "expected a fallback ({reason}):\n{}",
            pair.explain().join("\n")
        );
        twin.same(args, &[]).assert_hit().assert_success();
    };
    // A comment: the saved plan stands.
    state.incr_comments = 1;
    twin.write("incr/BUILD.bazel", &fuzz_incr_build(&state));
    incremental(0);
    // An attribute of //incr:a: it and //incr_use:u are re-analyzed (and
    // :b, its package's other target).
    state.incr_flag = 1;
    twin.write("incr/BUILD.bazel", &fuzz_incr_build(&state));
    let spliced = incremental(3);
    assert!(
        spliced.stderr().contains("2 actions executed"),
        "{}",
        spliced.stderr()
    );
    // A globbed source added, then removed; an explicit source deleted and
    // restored (the build then fails like the full analysis's).
    twin.write("incr/srcs/new.txt", "new\n");
    incremental(3);
    twin.each(|root| fs::remove_file(root.join("ws/incr/srcs/new.txt")).unwrap());
    incremental(3);
    // Fallbacks, in this process before any analysis or in a fresh one.
    state.toggle("incr-extra");
    twin.write("incr/BUILD.bazel", &fuzz_incr_build(&state));
    fallback("expansion may change");
    state.toggle("incr-dep");
    twin.write("incr/BUILD.bazel", &fuzz_incr_build(&state));
    fallback("dependencies of //incr:a changed");
    state.toggle("incr-print");
    twin.write("incr/BUILD.bazel", &fuzz_incr_build(&state));
    fallback("prints while it loads");
    state.toggle("incr-print");
    state.toggle("incr-deprecated");
    twin.write("incr/BUILD.bazel", &fuzz_incr_build(&state));
    fallback("");
    state.toggle("incr-deprecated");
    twin.write("incr/BUILD.bazel", &fuzz_incr_build(&state));
    fallback("");
    // The rules' `.bzl` file: every target of its rules (:a, :b, :extra,
    // //incr_use:u and :v).
    twin.write("incr/defs.bzl", &format!("{INCR_RULES}\n# edited\n"));
    incremental(5);
    // A source an action reads: a hit (no analysis input changed).
    twin.write("incr/b.txt", "b2\n");
    twin.same(args, &[]).assert_hit().assert_success();
    // Back to a splice; a package other targets' visibility checks read
    // (a package group) falls back.
    state.incr_flag = 2;
    twin.write("incr/BUILD.bazel", &fuzz_incr_build(&state));
    // :a, :b, :extra and //incr_use:u.
    incremental(4);
    twin.write(
        "incr_use/BUILD.bazel",
        &format!("{INCR_USE_BUILD}package_group(name = 'friends', packages = ['//incr'])\n"),
    );
    fallback("");
    // A deleted explicit source: the splice plans the missing input, the
    // build fails like the full analysis's.
    twin.each(|root| fs::remove_file(root.join("ws/incr/b.txt")).unwrap());
    let failed = twin.same(args, &[]);
    assert!(!failed.on.status.success());
    twin.write("incr/b.txt", "b\n");
    let restored = twin.same(args, &[]);
    restored.assert_success();
}

/// A macro module loaded by `incr_m/BUILD.bazel` only.
fn incr_macros(flag: &str, comment: bool) -> String {
    format!(
        "load('//incr:defs.bzl', 'chain')\n\ndef pair(name, deps = []):\n    chain(name = name + '_1', deps = deps, flag = '{flag}')\n    chain(name = name + '_2', flag = 'two')\n{}",
        if comment { "# a comment\n" } else { "" }
    )
}

const INCR_M_BUILD: &str = "load(':macros.bzl', 'pair')\npair(name = 'p', deps = ['//incr:a'])\n";

/// Incremental analysis of `.bzl` edits (docs/INCREMENTAL_ANALYSIS.md):
/// the packages loading an edited module are loaded again (a macro's
/// edit changes their targets, a comment nothing), the targets whose
/// rules it defines are re-analyzed with what reaches them; a module a
/// repository rule loads falls back.
#[test]
fn incremental_analysis_of_bzl_edits() {
    let twin = fixture();
    let state = FuzzState::default();
    twin.write("repos.bzl", FUZZ_REPOS);
    twin.write("BUILD.bazel", &fuzz_build(false, false));
    twin.write("incr/defs.bzl", INCR_RULES);
    twin.write("incr/srcs/a.txt", "a\n");
    twin.write("incr/b.txt", "b\n");
    twin.write("incr/BUILD.bazel", &fuzz_incr_build(&state));
    twin.write("incr_use/BUILD.bazel", INCR_USE_BUILD);
    twin.write("incr_m/macros.bzl", &incr_macros("one", false));
    twin.write("incr_m/BUILD.bazel", INCR_M_BUILD);
    let args = &[
        "build",
        "//incr/...",
        "//incr_use/...",
        "//incr_m/...",
        "//other:o",
    ];
    twin.save(args, &[]);
    twin.same(args, &[]).assert_hit();
    let incremental = |rerun: usize| {
        let pair = twin.same(args, &[]);
        pair.assert_miss("").assert_success();
        let line = pair
            .explain()
            .into_iter()
            .find(|line| line.starts_with("incremental ") && line.contains(" re-analyzed;"))
            .unwrap_or_else(|| {
                panic!(
                    "expected an incremental analysis:\n{}",
                    pair.explain().join("\n")
                )
            });
        assert!(
            line.contains(&format!(" {rerun} requested target(s) re-analyzed")),
            "{line}"
        );
        if pair.mode == VERIFY {
            assert!(
                pair.line("verified ").is_some(),
                "{}",
                pair.explain().join("\n")
            );
        } else {
            pair.assert_saved();
        }
        twin.same(args, &[]).assert_hit().assert_success();
        pair
    };
    // The macro's comment: its package's targets stay.
    twin.write("incr_m/macros.bzl", &incr_macros("one", true));
    incremental(0);
    // The macro's body: its package's targets change.
    twin.write("incr_m/macros.bzl", &incr_macros("uno", true));
    let spliced = incremental(2);
    assert!(
        spliced.stderr().contains("1 action executed")
            || spliced.stderr().contains("1 actions executed"),
        "{}",
        spliced.stderr()
    );
    // The rules' module: a comment re-analyzes every target of its rules
    // (their actions unchanged), a body edit re-runs them.
    twin.write("incr/defs.bzl", &format!("{INCR_RULES}\n# edited\n"));
    incremental(6);
    twin.write(
        "incr/defs.bzl",
        &INCR_RULES.replace("out.path, ctx.attr.flag", "out.path, ctx.attr.flag + '!'"),
    );
    incremental(6);
    // A module a repository rule loads: the full analysis.
    twin.write("repos.bzl", &format!("{FUZZ_REPOS}\n# edited\n"));
    let pair = twin.same(args, &[]);
    pair.assert_miss("").assert_success();
    assert!(
        pair.explain()
            .iter()
            .any(|line| line.starts_with("incremental") && !line.contains(" re-analyzed;")),
        "{}",
        pair.explain().join("\n")
    );
    twin.same(args, &[]).assert_hit().assert_success();
}

/// `incr_t/BUILD.bazel`: two tests and a tool, `:t` and `:tool` reaching
/// `//incr:a`; `flag` the tests' and tool's flag, `comments` comments.
fn incr_t_build(flag: usize, comments: usize) -> String {
    let mut build = format!(
        "load('//incr:defs.bzl', 'chain_bin', 'chain_test')\nchain_test(name = 't', size = 'small', deps = ['//incr:a'], flag = 't{flag}')\nchain_test(name = 'u_test', size = 'small', flag = 'u')\nchain_bin(name = 'tool', deps = ['//incr:a'], flag = 'tool{flag}')\n"
    );
    for comment in 0..comments {
        build.push_str(&format!("# comment {comment}\n"));
    }
    build
}

/// Incremental analysis of `test` and `run` (docs/INCREMENTAL_ANALYSIS.md):
/// an edit of a dependency or of the tests' own package re-analyzes the
/// tests and the tool reaching it and splices them (their run plans
/// included); a comment keeps the saved plan; the tests run and the tool
/// runs like the snapshot-less run's.
#[test]
fn incremental_analysis_of_test_and_run() {
    let twin = fixture();
    let mut state = FuzzState::default();
    twin.write("repos.bzl", FUZZ_REPOS);
    twin.write("BUILD.bazel", &fuzz_build(false, false));
    twin.write("incr/defs.bzl", INCR_RULES);
    twin.write("incr/srcs/a.txt", "a\n");
    twin.write("incr/b.txt", "b\n");
    twin.write("incr/BUILD.bazel", &fuzz_incr_build(&state));
    twin.write("incr_t/BUILD.bazel", &incr_t_build(0, 0));
    let commands: [&[&str]; 3] = [
        &["test", "--test_output=all", "//incr_t:all"],
        &["test", "--test_output=all", "//incr_t:t"],
        &[
            "run",
            &format!(
                "--script_path={}",
                twin.run_root().join("home/run.sh").display()
            ),
            "//incr_t:tool",
        ],
    ];
    let incremental = |args: &[&str], rerun: usize| {
        let pair = twin.same(args, &[]);
        pair.assert_miss("incr").assert_success();
        let line = pair
            .explain()
            .into_iter()
            .find(|line| line.starts_with("incremental ") && line.contains(" re-analyzed;"))
            .unwrap_or_else(|| {
                panic!(
                    "{args:?}: expected an incremental analysis:\n{}",
                    pair.explain().join("\n")
                )
            });
        assert!(
            line.contains(&format!(" {rerun} requested target(s) re-analyzed")),
            "{args:?}: {line}"
        );
        if pair.mode == VERIFY {
            assert!(
                pair.line("verified ").is_some(),
                "{}",
                pair.explain().join("\n")
            );
        } else {
            pair.assert_saved();
        }
        twin.same(args, &[]).assert_hit().assert_success();
    };
    for args in commands {
        twin.save(args, &[]);
        twin.same(args, &[]).assert_hit().assert_success();
    }
    let all_tests = commands[0];
    let one_test = commands[1];
    let run = commands[2];
    // A dependency's attribute: the tests and the tool reaching it.
    state.incr_flag = 1;
    twin.write("incr/BUILD.bazel", &fuzz_incr_build(&state));
    incremental(all_tests, 2);
    incremental(one_test, 1);
    incremental(run, 1);
    // The tests' own package: every requested target in it.
    twin.write("incr_t/BUILD.bazel", &incr_t_build(1, 0));
    incremental(all_tests, 3);
    incremental(one_test, 1);
    incremental(run, 1);
    // A comment: the saved plan stands.
    twin.write("incr_t/BUILD.bazel", &incr_t_build(1, 1));
    incremental(all_tests, 0);
    incremental(one_test, 0);
    incremental(run, 0);
}

/// Test suites (integration-21: parity-next's target pattern phase, ed8468a
/// and 644a0b1, under L2-W5): explicit, implicit (`tests` empty: the
/// package's non-manual tests) and nested suites, a suite of one test for
/// `run`, and a suite with a non-test member in its own package.
const SUITE_BUILD: &str = r##"load("//:rules.bzl", "script_test")

script_test(name = "a_test", size = "small", script = "#!/bin/bash\necho a-ran\n")

script_test(name = "b_test", size = "small", script = "#!/bin/bash\necho b-ran\n")

script_test(name = "big_test", size = "large", script = "#!/bin/bash\necho big-ran\n")

script_test(name = "manual_test", size = "small", tags = ["manual"], script = "#!/bin/bash\necho manual-ran\n")

test_suite(name = "explicit", tests = [":a_test", ":big_test"])

test_suite(name = "implicit")

test_suite(name = "one", tests = [":b_test"])

test_suite(name = "nested", tests = [":one", ":manual_test"])
"##;

const BAD_SUITE_BUILD: &str = r##"load("//:rules.bzl", "script_test")

script_test(name = "c_test", size = "small", script = "#!/bin/bash\necho c-ran\n")

genrule(name = "lib", outs = ["lib.txt"], cmd = "echo lib > $@")

test_suite(name = "bad", tests = [":c_test", ":lib"])
"##;

/// The test labels of a `test` summary on stdout, in order.
fn summarized_tests(output: &Output) -> Vec<String> {
    text(&output.stdout)
        .lines()
        .filter_map(|line| line.split_whitespace().next())
        .filter(|word| word.starts_with("//"))
        .map(str::to_owned)
        .collect()
}

/// Integration-21 (ed8468a/644a0b1 × L2-W5): the target pattern phase
/// expands test suites before planning and the planned request carries the
/// expanded sets (`RequestCommand::Test { build, tests }`, the `Run` label),
/// so a hit tests, builds and runs exactly what the full analysis selected;
/// `--test_size_filters` and `--noexpand_test_suites` are part of G (each
/// selects its own snapshot).
#[test]
fn test_suites_hit_with_what_the_full_analysis_selected() {
    let twin = fixture();
    twin.write("suite/BUILD.bazel", SUITE_BUILD);
    let tested = |args: &[&str], expected: &[&str]| {
        let hit = twin.same(args, &[]);
        hit.assert_hit().assert_success();
        assert_eq!(summarized_tests(&hit.on), expected, "{args:?}");
    };
    let explicit = &["test", "//suite:explicit"];
    twin.save(explicit, &[]);
    tested(explicit, &["//suite:a_test", "//suite:big_test"]);
    let small = &["test", "--test_size_filters=small", "//suite:implicit"];
    twin.same(small, &[]).assert_miss("").assert_saved();
    tested(small, &["//suite:a_test", "//suite:b_test"]);
    let implicit = &["test", "//suite:implicit"];
    twin.same(implicit, &[]).assert_miss("").assert_saved();
    tested(
        implicit,
        &["//suite:a_test", "//suite:b_test", "//suite:big_test"],
    );
    tested(small, &["//suite:a_test", "//suite:b_test"]);
    let nested = &["test", "//suite:nested"];
    twin.same(nested, &[]).assert_saved();
    tested(nested, &["//suite:b_test", "//suite:manual_test"]);
    // `build` of a suite builds its tests; --noexpand_test_suites the suite.
    for build in [
        &["build", "//suite:explicit"][..],
        &["build", "--noexpand_test_suites", "//suite:explicit"],
    ] {
        twin.same(build, &[]).assert_saved();
        twin.same(build, &[]).assert_hit().assert_success();
    }
    // `run` of a suite of one test runs that test.
    let run = &["run", "//suite:one"];
    twin.same(run, &[]).assert_saved();
    let ran = twin.same(run, &[]);
    ran.assert_hit().assert_success();
    assert!(
        text(&ran.on.stdout).contains("b-ran"),
        "{}",
        text(&ran.on.stdout)
    );
}

/// Integration-21: a test suite with a non-test member makes `test`,
/// `build` and `run` fail after their build (BuildView
/// .createAnalysisFailureDetail), which a hit — it skips the target pattern
/// phase — could not reproduce: such a run is never saved (Taint::Errors).
/// A suite edited into that state misses, is not saved, and hits its
/// earlier snapshot once the edit is reverted.
#[test]
fn failing_test_suite_expansions_are_never_saved() {
    let twin = fixture();
    twin.write("badsuite/BUILD.bazel", BAD_SUITE_BUILD);
    for args in [
        &["test", "//badsuite:bad"][..],
        &["build", "//badsuite:bad"],
        &["run", "//badsuite:bad"],
    ] {
        for attempt in 0..2 {
            let failed = twin.same(args, &[]);
            assert_eq!(failed.on.status.code(), Some(1), "{args:?}");
            let stderr = failed.stderr();
            assert!(
                stderr.contains(
                    "expecting a test or a test_suite rule but '//badsuite:lib' is not one"
                ),
                "{stderr}"
            );
            assert!(
                stderr.contains("ERROR: command succeeded, but there were"),
                "{stderr}"
            );
            // The first run of the workspace may write the lockfile first.
            if attempt == 1 {
                failed.assert_not_saved("test-suite expansion error");
            } else {
                failed.assert_not_saved("");
            }
        }
    }
    assert!(twin.snapshots().is_empty());
    twin.write("suite/BUILD.bazel", SUITE_BUILD);
    let explicit = &["test", "//suite:explicit"];
    twin.save(explicit, &[]);
    twin.write(
        "suite/BUILD.bazel",
        &format!(
            "{}\ngenrule(name = 'lib', outs = ['lib.txt'], cmd = 'echo lib > $@')\n",
            SUITE_BUILD.replace(
                "tests = [\":a_test\", \":big_test\"]",
                "tests = [\":a_test\", \":big_test\", \":lib\"]"
            )
        ),
    );
    let failed = twin.same(explicit, &[]);
    failed
        .assert_miss("")
        .assert_not_saved("test-suite expansion error");
    assert_eq!(failed.on.status.code(), Some(1));
    twin.write("suite/BUILD.bazel", SUITE_BUILD);
    let hit = twin.same(explicit, &[]);
    hit.assert_hit().assert_success();
    assert_eq!(
        summarized_tests(&hit.on),
        ["//suite:a_test", "//suite:big_test"]
    );
    assert_eq!(twin.snapshots().len(), 1);
}

/// L2-W7, on by default: with `BAZEL_RS_SNAPSHOT` unset or empty (or set to
/// a value it does not know, which it reports) a build saves and hits like
/// `on`, byte-identical with `off`; `off`, `0` and `false` turn everything
/// off — no lookup, no recording, no store: a fresh cache gets no
/// `snapshots/` directory at all (none in the workspace's state directory),
/// and EXPLAIN has nothing to say.
#[test]
fn the_snapshot_is_on_by_default_and_off_turns_everything_off() {
    let twin = fixture();
    // The snapshot side with the variable unset (`None`) or set to `value`.
    let step = |value: Option<&str>| {
        let on = twin.with_side(ON, || {
            let mut command = twin.command(BUILD_ALL, &[], true);
            match value {
                Some(value) => command.env("BAZEL_RS_SNAPSHOT", value),
                None => command.env_remove("BAZEL_RS_SNAPSHOT"),
            };
            command.output().expect("bazel-rs runs")
        });
        let pair = Pair {
            on,
            off: twin.run_side(OFF, BUILD_ALL, &[]),
            mode: ON,
        };
        pair.assert_same(BUILD_ALL);
        pair
    };
    // The cold run executes the generated repository (re-probed: saved).
    step(None).assert_miss("no snapshot").assert_saved();
    step(None).assert_hit().assert_success();
    step(None).assert_hit().assert_success();
    step(Some("")).assert_hit();
    let unknown = step(Some("sometimes"));
    unknown.assert_hit();
    assert!(
        unknown
            .line("ignoring BAZEL_RS_SNAPSHOT=sometimes: expected on (the default), off")
            .is_some(),
        "{:?}",
        unknown.explain()
    );
    assert_eq!(twin.snapshots().len(), 1);
    // Off: the twin's other side has only ever run with `off`; now also
    // `0` and `false`, with EXPLAIN.
    for value in ["off", "0", "false"] {
        let output = twin.with_side(OFF, || {
            twin.command(BUILD_ALL, &[("BAZEL_RS_SNAPSHOT", value)], true)
                .output()
                .expect("bazel-rs runs")
        });
        assert!(output.status.success(), "{}", text(&output.stderr));
        assert!(
            !text(&output.stderr).contains("bazel-rs-snapshot: "),
            "{value}: {}",
            text(&output.stderr)
        );
    }
    let cache = twin.side(OFF).join("cache");
    assert!(cache.join("repositories").is_dir());
    // Neither in the workspace's state directory (docs/WORKTREES.md) nor
    // where the flat layout kept it.
    assert!(!twin.store(OFF).exists());
    assert!(!cache.join("snapshots").exists());
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
    fn incremental_analysis_splices_prunes_and_falls_back() {
        verified(super::incremental_analysis_splices_prunes_and_falls_back);
    }

    #[test]
    fn incremental_analysis_of_test_and_run() {
        verified(super::incremental_analysis_of_test_and_run);
    }

    #[test]
    fn incremental_analysis_of_bzl_edits() {
        verified(super::incremental_analysis_of_bzl_edits);
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
    fn a_file_or_directory_deleted_from_a_repository_tree_misses() {
        verified(super::a_file_or_directory_deleted_from_a_repository_tree_misses);
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
    fn shared_tool_runfiles_groups_save_and_hit() {
        verified(super::shared_tool_runfiles_groups_save_and_hit);
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
    fn action_failures_save_and_analysis_failures_never_do() {
        verified(super::action_failures_save_and_analysis_failures_never_do);
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
    fn map_each_plans_save_and_hit() {
        verified(super::map_each_plans_save_and_hit);
    }

    #[test]
    fn a_test_writes_the_lockfile_and_saves_next_time() {
        verified(super::a_test_writes_the_lockfile_and_saves_next_time);
    }

    #[test]
    fn a_reinherited_repo_env_name_follows_the_client() {
        verified(super::a_reinherited_repo_env_name_follows_the_client);
    }

    #[test]
    fn a_nobuild_status_consumer_runs_the_status_command_on_a_hit() {
        verified(super::a_nobuild_status_consumer_runs_the_status_command_on_a_hit);
    }

    #[test]
    fn a_reused_rule_misses_on_its_environ_variable_or_watched_file() {
        verified(super::a_reused_rule_misses_on_its_environ_variable_or_watched_file);
    }

    #[test]
    fn a_reused_extension_misses_on_its_environment_or_file_input() {
        verified(super::a_reused_extension_misses_on_its_environment_or_file_input);
    }

    #[test]
    fn module_file_prints_replay_on_a_hit() {
        verified(super::module_file_prints_replay_on_a_hit);
    }

    #[test]
    fn local_override_and_file_registry_edits_miss() {
        verified(super::local_override_and_file_registry_edits_miss);
    }

    #[test]
    fn a_new_build_file_in_a_globbed_subdirectory_misses() {
        verified(super::a_new_build_file_in_a_globbed_subdirectory_misses);
    }

    #[test]
    fn repo_file_ignore_directories_edits_miss() {
        verified(super::repo_file_ignore_directories_edits_miss);
    }

    #[test]
    fn a_store_input_staged_by_the_test_runner_heals() {
        verified(super::a_store_input_staged_by_the_test_runner_heals);
    }

    #[test]
    fn a_failed_hit_on_a_tampered_store_tree_heals() {
        verified(super::a_failed_hit_on_a_tampered_store_tree_heals);
    }

    #[test]
    fn a_tampered_local_repository_misses() {
        verified(super::a_tampered_local_repository_misses);
    }

    #[test]
    fn hit_refreshes_racing_a_concurrent_save_leave_a_valid_snapshot() {
        verified(super::hit_refreshes_racing_a_concurrent_save_leave_a_valid_snapshot);
    }

    #[test]
    fn garbage_collection_during_a_hit_never_breaks_it() {
        verified(super::garbage_collection_during_a_hit_never_breaks_it);
    }

    #[test]
    fn test_suites_hit_with_what_the_full_analysis_selected() {
        verified(super::test_suites_hit_with_what_the_full_analysis_selected);
    }

    #[test]
    fn failing_test_suite_expansions_are_never_saved() {
        verified(super::failing_test_suite_expansions_are_never_saved);
    }
}

include!("support/executable_script.rs");
