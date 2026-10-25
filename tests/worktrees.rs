//! docs/WORKTREES.md: without `--cache-dir` the linked worktrees of a git
//! repository share their main checkout's cache (`<main>/bazel-rs-out`),
//! with each workspace's own state in `<cache>/worktrees/<name>-<hash>/`.
//! Every fixture is a scratch repository with real `git worktree`s and runs
//! with the DEFAULT cache (and the snapshot's default mode, forwarded by
//! `verifier_environment`): repositories and actions are reused, worktrees
//! of one repository build the same and different commits at the same time,
//! test results are cached per worktree (switching reruns nothing),
//! concurrent `run`s of one test keep their execution roots, analysis
//! snapshots are per worktree, the hidden lockfile is seeded from the main
//! checkout, and repositories whose content depends on a worktree's files
//! or path are never reused by another worktree — while paths into the
//! shared cache inside the main checkout are no such dependence.
#![allow(clippy::disallowed_methods)] // test harness: starts processes directly (clippy.toml)
include!("support/verify_env.rs");
include!("support/explicit_modules.rs");

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

const BUILTIN_REGISTRY: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/tests/fixtures/builtin_modules/registry"
);

/// A scratch git repository: the main checkout `<base>/main` and linked
/// worktrees beside it.
struct Repo {
    base: tempfile::TempDir,
}

impl Repo {
    /// A main checkout holding `files`, committed on branch `main`.
    fn new(files: &[(&str, &str)]) -> Self {
        let base = tempfile::Builder::new()
            .prefix("worktrees-")
            .tempdir_in("/tmp/opencode")
            .unwrap();
        let repo = Self { base };
        fs::create_dir_all(repo.home()).unwrap();
        fs::create_dir_all(repo.main()).unwrap();
        repo.git(&repo.main(), &["init", "-q", "-b", "main"]);
        repo.write(
            &repo.main(),
            ".gitignore",
            "bazel-rs-out\nMODULE.bazel.lock\n",
        );
        for (path, content) in files {
            repo.write(&repo.main(), path, content);
        }
        repo.commit(&repo.main(), "init");
        repo
    }

    fn base(&self) -> PathBuf {
        fs::canonicalize(self.base.path()).unwrap()
    }

    fn home(&self) -> PathBuf {
        self.base().join("home")
    }

    fn main(&self) -> PathBuf {
        self.base().join("main")
    }

    /// The shared default cache.
    fn cache(&self) -> PathBuf {
        self.main().join("bazel-rs-out")
    }

    fn git(&self, checkout: &Path, args: &[&str]) {
        let output = Command::new("git")
            .args(["-c", "user.name=t", "-c", "user.email=t@t"])
            .args(args)
            .current_dir(checkout)
            .env_clear()
            .env("PATH", "/usr/bin:/bin")
            .env("HOME", self.home())
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    fn commit(&self, checkout: &Path, message: &str) {
        self.git(checkout, &["add", "-A"]);
        self.git(checkout, &["commit", "-q", "--allow-empty", "-m", message]);
    }

    /// `git worktree add -b <name> <base>/<name>` from the main checkout.
    fn worktree(&self, name: &str) -> PathBuf {
        let path = self.base().join(name);
        self.git(
            &self.main(),
            &["worktree", "add", "-q", "-b", name, path.to_str().unwrap()],
        );
        path
    }

    fn write(&self, checkout: &Path, path: &str, content: &str) {
        let path = checkout.join(path);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, content).unwrap();
    }

    /// bazel-rs in `checkout` with the default cache and the snapshot's
    /// default mode (on; `env` may say otherwise), in the explicit-module
    /// profile unless `args` select one (offline fixtures).
    fn command(&self, checkout: &Path, args: &[&str], env: &[(&str, &str)]) -> Command {
        let mut command = Command::new("timeout");
        command
            .args(["--kill-after=5s", "240s"])
            .arg(env!("CARGO_BIN_EXE_bazel-rs"))
            .arg("--ignore_all_rc_files")
            .args(with_explicit_modules(args))
            .current_dir(checkout)
            .env_clear()
            .envs(verifier_environment())
            .env("PATH", "/usr/bin:/bin")
            .env("HOME", self.home())
            .env("TMPDIR", self.home())
            .env("LC_ALL", "C")
            .envs(env.iter().copied());
        command
    }

    fn run(&self, checkout: &Path, args: &[&str]) -> Output {
        self.run_with(checkout, args, &[])
    }

    fn run_with(&self, checkout: &Path, args: &[&str], env: &[(&str, &str)]) -> Output {
        let output = self.command(checkout, args, env).output().unwrap();
        assert_ne!(output.status.code(), Some(124), "CLI timed out");
        output
    }

    /// A successful run's stderr.
    fn ok(&self, checkout: &Path, args: &[&str]) -> String {
        let output = self.run(checkout, args);
        assert!(output.status.success(), "{}", stderr(&output));
        stderr(&output)
    }

    /// The per-worktree state directories in the shared cache.
    fn states(&self) -> Vec<String> {
        let mut names: Vec<String> = fs::read_dir(self.cache().join("worktrees"))
            .into_iter()
            .flatten()
            .flatten()
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        names
    }

    /// The state directory of `checkout` (its workspace) in the cache.
    fn state(&self, checkout: &Path) -> PathBuf {
        let name = checkout.file_name().unwrap().to_string_lossy().into_owned();
        let found: Vec<String> = self
            .states()
            .into_iter()
            .filter(|state| state.rsplit_once('-').is_some_and(|(n, _)| n == name))
            .collect();
        let [state] = <[String; 1]>::try_from(found).unwrap();
        self.cache().join("worktrees").join(state)
    }
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

/// The built outputs (`label<TAB>path` lines) and their contents.
fn outputs(output: &Output) -> Vec<(String, String)> {
    stdout(output)
        .lines()
        .filter_map(|line| line.split_once('\t'))
        .map(|(_, path)| {
            (
                path.to_owned(),
                fs::read_to_string(path).unwrap_or_else(|e| panic!("{path}: {e}")),
            )
        })
        .collect()
}

fn count(text: &str, needle: &str) -> usize {
    text.matches(needle).count()
}

const REPO_RULE: &str = r#"
def _impl(ctx):
    print("REPO_EXECUTED " + ctx.name)
    ctx.file("value.txt", ctx.read(Label("//:seed.txt")))
    ctx.file("BUILD.bazel", "exports_files(['value.txt'])\n")

seeded = repository_rule(_impl)
"#;

/// The linked worktree's default cache is the main checkout's: its
/// repositories and actions are reused, `--cache-dir` still overrides, and
/// a leftover `<worktree>/bazel-rs-out` gets one note per run.
#[test]
fn a_linked_worktree_shares_the_main_checkouts_cache() {
    let repo = Repo::new(&[
        (
            "MODULE.bazel",
            "module(name = 'main')\nseeded = use_repo_rule('//:repo.bzl', 'seeded')\nseeded(name = 'gen')\n",
        ),
        ("repo.bzl", REPO_RULE),
        ("seed.txt", "seed\n"),
        ("src.txt", "source\n"),
        (
            "BUILD.bazel",
            "genrule(name = 'out', srcs = ['src.txt', '@gen//:value.txt'], outs = ['out.txt'], cmd = 'cat $(SRCS) > $@')\n",
        ),
    ]);
    let main = repo.main();
    let wt = repo.worktree("wt");
    let first = repo.run(&main, &["build", "//:out"]);
    assert!(first.status.success(), "{}", stderr(&first));
    assert_eq!(count(&stderr(&first), "REPO_EXECUTED"), 1);
    assert!(stderr(&first).contains("bazel-rs: 1 actions executed, 0 cache hits"));
    let [(path, content)] = <[_; 1]>::try_from(outputs(&first)).unwrap();
    assert!(path.starts_with(repo.cache().to_str().unwrap()), "{path}");
    assert_eq!(content, "source\nseed\n");
    // The worktree on the same commit: nothing runs again.
    let second = repo.run(&wt, &["build", "//:out"]);
    assert!(second.status.success(), "{}", stderr(&second));
    assert_eq!(
        count(&stderr(&second), "REPO_EXECUTED"),
        0,
        "{}",
        stderr(&second)
    );
    assert!(
        stderr(&second).contains("bazel-rs: 0 actions executed, 1 cache hits"),
        "{}",
        stderr(&second)
    );
    assert_eq!(outputs(&second), outputs(&first));
    assert!(!wt.join("bazel-rs-out").exists());
    assert!(!stderr(&second).contains("is no longer used"));
    // A leftover cache of the worktree: one note per run, only then.
    fs::create_dir_all(wt.join("bazel-rs-out/repositories")).unwrap();
    let noted = repo.ok(&wt, &["build", "//:out"]);
    let note = format!(
        "bazel-rs: {} is no longer used: this git worktree shares the cache of its main checkout, {}; delete {} to free its space\n",
        wt.join("bazel-rs-out").display(),
        repo.cache().display(),
        wt.join("bazel-rs-out").display()
    );
    assert_eq!(count(&noted, &note), 1, "{noted}");
    // `--cache-dir` overrides the default (and is never noted).
    let private = repo.base().join("private-cache");
    let flag = format!("--cache-dir={}", private.display());
    let own = repo.ok(&wt, &[flag.as_str(), "build", "//:out"]);
    assert_eq!(count(&own, "REPO_EXECUTED"), 1, "{own}");
    assert!(!own.contains("is no longer used"));
    assert!(private.join("repositories").is_dir());
    fs::remove_dir_all(wt.join("bazel-rs-out")).unwrap();
    assert!(
        !repo
            .ok(&wt, &["build", "//:out"])
            .contains("is no longer used")
    );
    // A different seed on the worktree's branch: the repository runs for it
    // (and the main checkout keeps its own result).
    repo.write(&wt, "seed.txt", "other\n");
    repo.commit(&wt, "other seed");
    let changed = repo.run(&wt, &["build", "//:out"]);
    assert_eq!(count(&stderr(&changed), "REPO_EXECUTED"), 1);
    assert_eq!(outputs(&changed)[0].1, "source\nother\n");
    let back = repo.run(&main, &["build", "//:out"]);
    assert_eq!(outputs(&back)[0].1, "source\nseed\n");
}

/// A workspace below the repository root maps to the same directory of the
/// main checkout.
#[test]
fn a_nested_workspace_shares_the_same_directory_of_the_main_checkout() {
    let repo = Repo::new(&[
        ("README", "repository root, no workspace\n"),
        ("sub/ws/MODULE.bazel", "module(name = 'nested')\n"),
        (
            "sub/ws/BUILD.bazel",
            "genrule(name = 'out', outs = ['out.txt'], cmd = 'echo nested > $@')\n",
        ),
    ]);
    let wt = repo.worktree("wt");
    let first = repo.run(&repo.main().join("sub/ws"), &["build", "//:out"]);
    assert!(first.status.success(), "{}", stderr(&first));
    let second = repo.run(&wt.join("sub/ws"), &["build", "//:out"]);
    assert!(second.status.success(), "{}", stderr(&second));
    assert!(stderr(&second).contains("bazel-rs: 0 actions executed, 1 cache hits"));
    let shared = repo.main().join("sub/ws/bazel-rs-out");
    assert!(outputs(&second)[0].0.starts_with(shared.to_str().unwrap()));
    assert!(!wt.join("sub/ws/bazel-rs-out").exists());
}

const TEST_RULES: &str = r#"
def _t(ctx):
    out = ctx.actions.declare_file(ctx.label.name + ".sh")
    ctx.actions.write(out, ctx.attr.script, is_executable = True)
    return [DefaultInfo(executable = out, runfiles = ctx.runfiles(files = ctx.files.data))]

sh_like_test = rule(
    implementation = _t,
    test = True,
    attrs = {"script": attr.string(), "data": attr.label_list(allow_files = True)},
)
"#;

const TEST_BUILD: &str = r##"load("//:defs.bzl", "sh_like_test")

sh_like_test(
    name = "data_test",
    data = ["data.txt"],
    script = "#!/bin/bash\necho RAN_TEST\ncat data.txt\n",
)

sh_like_test(
    name = "slow_test",
    script = "#!/bin/bash\nmkdir -p \"$TEST_TMPDIR\"\necho here > \"$TEST_TMPDIR/marker\"\nsleep 3\nif [ ! -f \"$TEST_TMPDIR/marker\" ]; then echo TMPDIR_MARKER_GONE; exit 1; fi\necho MARKER_KEPT\n",
)
"##;

fn test_repo() -> Repo {
    Repo::new(&[
        ("MODULE.bazel", "module(name = 'main')\n"),
        ("defs.bzl", TEST_RULES),
        ("BUILD.bazel", TEST_BUILD),
        ("data.txt", "main data\n"),
    ])
}

fn summary(output: &Output) -> String {
    stdout(output)
        .lines()
        .find(|line| line.starts_with("//:data_test"))
        .unwrap_or_else(|| panic!("{}\n{}", stdout(output), stderr(output)))
        .split_whitespace()
        .skip(1)
        .collect::<Vec<_>>()
        .join(" ")
}

/// Each worktree has its own last test result per test (the index of its
/// output base), the results themselves are shared: two worktrees testing
/// different data alternate without rerunning, and a new worktree of a
/// tested commit starts cached.
#[test]
fn test_results_are_cached_per_worktree_without_rerunning_on_switches() {
    let repo = test_repo();
    let main = repo.main();
    let wt = repo.worktree("wt");
    repo.write(&wt, "data.txt", "worktree data\n");
    repo.commit(&wt, "worktree data");
    let test = |checkout: &Path| {
        let output = repo.run(checkout, &["test", "//:data_test", "--test_output=all"]);
        assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
        output
    };
    for (checkout, data) in [(&main, "main data"), (&wt, "worktree data")] {
        let first = test(checkout);
        assert!(stdout(&first).contains(data), "{}", stdout(&first));
        assert!(!summary(&first).contains("(cached)"), "{}", summary(&first));
    }
    for _ in 0..2 {
        for checkout in [&main, &wt] {
            let again = test(checkout);
            assert!(
                summary(&again).starts_with("(cached) PASSED"),
                "{}",
                stdout(&again)
            );
        }
    }
    // A new worktree of the main checkout's commit: cached from the start.
    let fresh = repo.worktree("fresh");
    let first = test(&fresh);
    assert!(
        summary(&first).starts_with("(cached) PASSED"),
        "{}",
        summary(&first)
    );
    // Its own index now: another configuration reruns, and so does the
    // first one afterwards (only the last key of a test is reusable).
    let other = repo.run(
        &fresh,
        &[
            "test",
            "//:data_test",
            "--test_timeout=77",
            "--test_output=all",
        ],
    );
    assert!(stdout(&other).contains("RAN_TEST"), "{}", stdout(&other));
    let rerun = test(&fresh);
    assert!(!summary(&rerun).contains("(cached)"), "{}", summary(&rerun));
    // ... while the main checkout's last key is untouched.
    assert!(summary(&test(&main)).starts_with("(cached) PASSED"));
    for checkout in [&main, &wt, &fresh] {
        assert!(repo.state(checkout).join("tests/index").is_dir());
    }
}

/// `run` of a test executes in an execution root of the worktree's own
/// state: the same test run at the same time in two worktrees keeps both
/// (one execution root per label replaced the other's TEST_TMPDIR before).
#[test]
fn concurrent_runs_of_one_test_in_two_worktrees_both_succeed() {
    let repo = test_repo();
    let main = repo.main();
    let wt = repo.worktree("wt");
    // Build first, so the runs overlap in the tests themselves.
    for checkout in [&main, &wt] {
        repo.ok(checkout, &["build", "//:slow_test"]);
    }
    let spawn = |checkout: &Path| {
        repo.command(checkout, &["run", "//:slow_test"], &[])
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap()
    };
    let first = spawn(&main);
    std::thread::sleep(std::time::Duration::from_millis(1500));
    let second = spawn(&wt);
    for child in [first, second] {
        let output = child.wait_with_output().unwrap();
        assert_eq!(
            output.status.code(),
            Some(0),
            "{}\n{}",
            stdout(&output),
            stderr(&output)
        );
        assert!(
            stdout(&output).contains("MARKER_KEPT"),
            "{}",
            stdout(&output)
        );
    }
    for checkout in [&main, &wt] {
        let roots: Vec<_> = fs::read_dir(repo.state(checkout).join("test-run"))
            .unwrap()
            .flatten()
            .collect();
        assert_eq!(roots.len(), 1);
    }
    assert!(!repo.cache().join("test-run").exists());
}

/// Analysis snapshots are kept per worktree (each with its own store and
/// budget): two worktrees on different commits alternate with hits.
#[test]
fn analysis_snapshots_are_kept_per_worktree() {
    let repo = Repo::new(&[
        ("MODULE.bazel", "module(name = 'main')\n"),
        ("src.txt", "main\n"),
        (
            "BUILD.bazel",
            "genrule(name = 'out', srcs = ['src.txt'], outs = ['out.txt'], cmd = 'cat $< > $@')\n",
        ),
    ]);
    let main = repo.main();
    let wt = repo.worktree("wt");
    repo.write(
        &wt,
        "BUILD.bazel",
        "genrule(name = 'out', srcs = ['src.txt'], outs = ['out.txt'], cmd = 'cat $< $< > $@')\n",
    );
    repo.commit(&wt, "another BUILD file");
    let env = [
        ("BAZEL_RS_SNAPSHOT", "on"),
        ("BAZEL_RS_SNAPSHOT_EXPLAIN", "1"),
    ];
    let build = |checkout: &Path| {
        let output = repo.run_with(checkout, &["build", "//:out"], &env);
        assert!(output.status.success(), "{}", stderr(&output));
        stderr(&output)
    };
    for checkout in [&main, &wt] {
        // A first run that writes the lockfile is not saved; the next is.
        if !build(checkout).contains("bazel-rs-snapshot: saved ") {
            let second = build(checkout);
            assert!(second.contains("bazel-rs-snapshot: saved "), "{second}");
        }
    }
    for _ in 0..2 {
        for checkout in [&main, &wt] {
            let hit = build(checkout);
            assert!(hit.contains("bazel-rs-snapshot: hit "), "{hit}");
        }
    }
    for checkout in [&main, &wt] {
        let snapshots: Vec<_> = fs::read_dir(repo.state(checkout).join("snapshots/v1"))
            .unwrap()
            .flatten()
            .filter(|entry| entry.path().extension().is_some_and(|ext| ext == "snap"))
            .collect();
        assert_eq!(snapshots.len(), 1);
    }
    assert_eq!(repo.states().len(), 2, "{:?}", repo.states());
    assert!(!repo.cache().join("snapshots").exists());
}

const EXTENSIONS: &str = r#"
def _repo_impl(ctx):
    ctx.file("value.txt", ctx.attr.value + "\n")
    ctx.file("BUILD.bazel", "exports_files(['value.txt'])\n")

value_repo = repository_rule(_repo_impl, attrs = {"value": attr.string()})

def _plain_impl(ctx):
    print("EXT_EVALUATED plain")
    value_repo(name = "plain", value = "plain")
    return ctx.extension_metadata(reproducible = True)

plain = module_extension(_plain_impl)

def _located_impl(ctx):
    print("EXT_EVALUATED located")
    value_repo(name = "located", value = str(ctx.path(Label("//:MODULE.bazel")).dirname))
    return ctx.extension_metadata(reproducible = True)

located = module_extension(_located_impl)
"#;

/// A new worktree's hidden lockfile (reproducible extension results) is
/// seeded from the main checkout's: its extensions are not evaluated again,
/// except one whose result names the main checkout's path.
#[test]
fn the_hidden_lockfile_is_seeded_from_the_main_checkout() {
    let repo = Repo::new(&[
        (
            "MODULE.bazel",
            "module(name = 'main')\nplain = use_extension('//:ext.bzl', 'plain')\nuse_repo(plain, 'plain')\nlocated = use_extension('//:ext.bzl', 'located')\nuse_repo(located, 'located')\n",
        ),
        ("ext.bzl", EXTENSIONS),
        (
            "BUILD.bazel",
            "genrule(name = 'out', srcs = ['@plain//:value.txt', '@located//:value.txt'], outs = ['out.txt'], cmd = 'cat $(SRCS) > $@')\n",
        ),
    ]);
    let main = repo.main();
    let build = |checkout: &Path| {
        let output = repo.run(checkout, &["build", "//:out"]);
        assert!(output.status.success(), "{}", stderr(&output));
        output
    };
    let first = build(&main);
    assert_eq!(count(&stderr(&first), "EXT_EVALUATED"), 2);
    assert_eq!(outputs(&first)[0].1, format!("plain\n{}\n", main.display()));
    assert!(repo.state(&main).join("MODULE.bazel.lock").is_file());
    let wt = repo.worktree("wt");
    let second = build(&wt);
    assert_eq!(
        stderr(&second)
            .lines()
            .filter(|line| line.contains("EXT_EVALUATED"))
            .map(|line| line.rsplit(' ').next().unwrap())
            .collect::<Vec<_>>(),
        ["located"],
        "{}",
        stderr(&second)
    );
    assert_eq!(outputs(&second)[0].1, format!("plain\n{}\n", wt.display()));
    // The worktree has its own hidden lockfile now; nothing runs again.
    let hidden = repo.state(&wt).join("MODULE.bazel.lock");
    assert!(hidden.is_file());
    assert!(
        !fs::read_to_string(&hidden)
            .unwrap()
            .contains(main.to_str().unwrap())
    );
    assert_eq!(count(&stderr(&build(&wt)), "EXT_EVALUATED"), 0);
    assert_eq!(count(&stderr(&build(&main)), "EXT_EVALUATED"), 0);
    // A cache of the flat layout (before per-worktree state) kept the
    // hidden lockfile at `hidden-lockfiles/<sha256 of the workspace>/`: it
    // seeds the workspace's own, which is written to its state directory.
    let own = repo.state(&main).join("MODULE.bazel.lock");
    let legacy = {
        use sha2::Digest as _;
        let digest = sha2::Sha256::digest(main.as_os_str().as_encoded_bytes());
        let hex: String = digest.iter().map(|byte| format!("{byte:02x}")).collect();
        repo.cache()
            .join("hidden-lockfiles")
            .join(hex)
            .join("MODULE.bazel.lock")
    };
    fs::create_dir_all(legacy.parent().unwrap()).unwrap();
    fs::rename(&own, &legacy).unwrap();
    // (The snapshot would skip the evaluation it asserts.)
    let seeded = repo.run_with(&main, &["build", "//:out"], &[("BAZEL_RS_SNAPSHOT", "off")]);
    assert!(seeded.status.success(), "{}", stderr(&seeded));
    assert_eq!(
        count(&stderr(&seeded), "EXT_EVALUATED"),
        0,
        "{}",
        stderr(&seeded)
    );
    assert_eq!(fs::read(&own).unwrap(), fs::read(&legacy).unwrap());
}

const PROBES: &str = r#"
def _impl(ctx):
    mode = ctx.attr.mode
    if mode == "workspace_root":
        ctx.file("value.txt", str(ctx.workspace_root) + "\n")
    elif mode == "symlink":
        ctx.symlink(ctx.path(Label("//:same.txt")), "value.txt")
    elif mode == "str_path":
        ctx.file("value.txt", str(ctx.path(Label("//:same.txt"))) + "\n")
    elif mode == "watch":
        ctx.file("value.txt", ctx.read(Label("//:seed.txt")))
    print("PROBE_EXECUTED " + mode)
    ctx.file("BUILD.bazel", "exports_files(['value.txt'])\n")

probe = repository_rule(_impl, attrs = {"mode": attr.string()})
"#;

const PROBE_MODULE: &str = r#"module(name = "main")
bazel_dep(name = "dep", version = "1.0")
local_path_override(module_name = "dep", path = "dep")
bazel_dep(name = "reg", version = "1.0")
new_local_repository = use_repo_rule("@bazel_tools//tools/build_defs/repo:local.bzl", "new_local_repository")
new_local_repository(name = "loc", path = "loc", build_file_content = "exports_files(['value.txt'])")
probe = use_repo_rule("//:probe.bzl", "probe")
probe(name = "wsroot", mode = "workspace_root")
probe(name = "wslink", mode = "symlink")
probe(name = "wsfile", mode = "str_path")
probe(name = "watched", mode = "watch")
"#;

const PROBE_SOURCES: &str = r#"["@dep//:value.txt", "@reg//:value.txt", "@loc//:value.txt", "@wsroot//:value.txt", "@wslink//:value.txt", "@wsfile//:value.txt", "@watched//:value.txt"]"#;

/// Repositories whose content depends on a worktree — a
/// `local_path_override` module, a `file://` registry in the worktree, a
/// `new_local_repository`, a watched workspace file, the workspace's path
/// (`workspace_root`, a link into the workspace, a written path) — are
/// never reused by another worktree: each worktree builds its own content,
/// also after switching back and forth.
#[test]
fn override_and_local_repositories_are_never_reused_across_worktrees() {
    let build = format!(
        "genrule(name = 'out', srcs = {PROBE_SOURCES}, outs = ['out.txt'], cmd = 'for f in $(SRCS); do cat $$f; done > $@')\n"
    );
    let repo = Repo::new(&[
        ("MODULE.bazel", PROBE_MODULE),
        ("probe.bzl", PROBES),
        ("BUILD.bazel", &build),
        ("same.txt", "same\n"),
        ("seed.txt", "seed main\n"),
        (
            "dep/MODULE.bazel",
            "module(name = 'dep', version = '1.0')\n",
        ),
        ("dep/BUILD.bazel", "exports_files(['value.txt'])\n"),
        ("dep/value.txt", "dep main\n"),
        ("loc/value.txt", "loc main\n"),
        (
            "registry/bazel_registry.json",
            r#"{"module_base_path": "."}"#,
        ),
        (
            "registry/modules/reg/metadata.json",
            r#"{"versions": ["1.0"]}"#,
        ),
        (
            "registry/modules/reg/1.0/MODULE.bazel",
            "module(name = 'reg', version = '1.0')\n",
        ),
        (
            "registry/modules/reg/1.0/source.json",
            r#"{"type": "local_path", "path": "src/reg"}"#,
        ),
        (
            "registry/src/reg/MODULE.bazel",
            "module(name = 'reg', version = '1.0')\n",
        ),
        (
            "registry/src/reg/BUILD.bazel",
            "exports_files(['value.txt'])\n",
        ),
        ("registry/src/reg/value.txt", "reg main\n"),
    ]);
    let main = repo.main();
    let wt = repo.worktree("wt");
    for (path, content) in [
        ("seed.txt", "seed wt\n"),
        ("dep/value.txt", "dep wt\n"),
        ("loc/value.txt", "loc wt\n"),
        ("registry/src/reg/value.txt", "reg wt\n"),
    ] {
        repo.write(&wt, path, content);
    }
    repo.commit(&wt, "worktree content");
    let expected = |checkout: &Path, name: &str| {
        let root = checkout.display();
        format!("dep {name}\nreg {name}\nloc {name}\n{root}\nsame\n{root}/same.txt\nseed {name}\n")
    };
    let builtin = format!("--registry=file://{BUILTIN_REGISTRY}");
    for round in 0..2 {
        for (checkout, name) in [(&main, "main"), (&wt, "wt")] {
            let registry = format!("--registry=file://{}/registry", checkout.display());
            let output = repo.run(
                checkout,
                &[
                    "build",
                    "--experimental-builtin-modules",
                    &registry,
                    &builtin,
                    "//:out",
                ],
            );
            assert!(output.status.success(), "{}", stderr(&output));
            assert_eq!(
                outputs(&output)[0].1,
                expected(checkout, name),
                "round {round} in {name}"
            );
            // Each worktree's workspace-dependent publication has its own
            // pointer: after the first round, switching runs none of those
            // rules again. (`watched` reads a file that differs between the
            // branches: its one pointer flips, docs/WORKTREES.md hazard 1.)
            if round > 0 {
                let executed: Vec<String> = stderr(&output)
                    .lines()
                    .filter_map(|line| line.split("PROBE_EXECUTED ").nth(1))
                    .filter(|mode| *mode != "watch")
                    .map(str::to_owned)
                    .collect();
                assert!(executed.is_empty(), "round {round} in {name}: {executed:?}");
            }
        }
    }
    // The workspace-dependent publications record their workspace
    // (version-3 probes), one per worktree, and each worktree's link names
    // its own file.
    let manifests = repo.cache().join("repositories/direct/manifests");
    let mut workspaces = std::collections::BTreeSet::new();
    let mut links = std::collections::BTreeSet::new();
    for entry in fs::read_dir(&manifests).unwrap().flatten() {
        let manifest: serde_json::Value =
            serde_json::from_slice(&fs::read(entry.path()).unwrap()).unwrap();
        if let Some(target) = manifest["files"]["value.txt"]["symlink"].as_str() {
            links.insert(target.to_owned());
        }
        // The recorded probes are stored as their bytes.
        if let Some(bytes) = manifest["recorded"].as_array() {
            let bytes: Vec<u8> = bytes.iter().map(|b| b.as_u64().unwrap() as u8).collect();
            let recorded: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
            if let Some(workspace) = recorded["workspace"].as_str() {
                assert_eq!(recorded["version"], 3);
                workspaces.insert(workspace.to_owned());
            }
        }
    }
    let both = |suffix: &str| {
        [main.display(), wt.display()]
            .map(|root| format!("{root}{suffix}"))
            .into_iter()
            .collect::<std::collections::BTreeSet<_>>()
    };
    assert_eq!(workspaces, both(""));
    assert!(links.is_superset(&both("/same.txt")), "{links:?}");
}

/// `(executed, cache hits)` of a build's action summary line.
fn action_counts(text: &str) -> (usize, usize) {
    let line = text
        .lines()
        .find(|line| line.starts_with("bazel-rs: ") && line.contains(" actions executed, "))
        .unwrap_or_else(|| panic!("no action summary: {text}"));
    let words: Vec<&str> = line.split_whitespace().collect();
    (words[1].parse().unwrap(), words[4].parse().unwrap())
}

const SLOW_RULE: &str = r#"
def _impl(ctx):
    print("REPO_EXECUTED " + ctx.attr.value)
    ctx.execute(["sleep", "2"])
    ctx.file("value.txt", ctx.attr.value + "\n")
    ctx.file("BUILD.bazel", "exports_files(['value.txt'])\n")

slow = repository_rule(_impl, attrs = {"value": attr.string()})
"#;

const SLOW_BUILD: &str = r#"genrule(name = "first", srcs = ["src.txt", "@gen//:value.txt"], outs = ["first.txt"], cmd = "sleep 1; cat $(SRCS) > $@")

genrule(name = "second", srcs = [":first"], outs = ["second.txt"], cmd = "sleep 1; cat $< $< > $@")
"#;

/// Two worktrees of one repository on the same commit and a third on
/// another commit build at the same time with the default (shared) cache:
/// every command succeeds with its own commit's outputs; the repository
/// rule and each action of the shared commit run once between the two
/// (the store's stage lock and the action cache's per-key lock, each
/// re-probed under the lock); and afterwards every worktree's build reuses
/// everything.
#[test]
fn worktrees_build_the_same_and_different_commits_concurrently() {
    let repo = Repo::new(&[
        (
            "MODULE.bazel",
            "module(name = 'main')\nslow = use_repo_rule('//:repo.bzl', 'slow')\nslow(name = 'gen', value = 'one')\n",
        ),
        ("repo.bzl", SLOW_RULE),
        ("src.txt", "source one\n"),
        ("BUILD.bazel", SLOW_BUILD),
    ]);
    let main = repo.main();
    let same = repo.worktree("same");
    let other = repo.worktree("other");
    repo.write(
        &other,
        "MODULE.bazel",
        "module(name = 'main')\nslow = use_repo_rule('//:repo.bzl', 'slow')\nslow(name = 'gen', value = 'two')\n",
    );
    repo.write(&other, "src.txt", "source two\n");
    repo.commit(&other, "another commit");
    let build = ["build", "//:second"];
    let children: Vec<_> = [&main, &same, &other]
        .into_iter()
        .map(|checkout| {
            repo.command(checkout, &build, &[])
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
                .unwrap()
        })
        .collect();
    let results: Vec<Output> = children
        .into_iter()
        .map(|child| child.wait_with_output().unwrap())
        .collect();
    let contents = |value: &str| format!("source {value}\n{value}\n").repeat(2);
    for (output, value) in results.iter().zip(["one", "one", "two"]) {
        assert!(output.status.success(), "{}", stderr(output));
        let [(path, content)] = <[_; 1]>::try_from(outputs(output)).unwrap();
        assert!(path.starts_with(repo.cache().to_str().unwrap()), "{path}");
        assert_eq!(content, contents(value));
    }
    let executed =
        |output: &Output, value: &str| count(&stderr(output), &format!("REPO_EXECUTED {value}"));
    assert_eq!(
        executed(&results[0], "one") + executed(&results[1], "one"),
        1,
        "{}\n{}",
        stderr(&results[0]),
        stderr(&results[1])
    );
    assert_eq!(executed(&results[2], "two"), 1);
    let (main_ran, main_hits) = action_counts(&stderr(&results[0]));
    let (same_ran, same_hits) = action_counts(&stderr(&results[1]));
    assert_eq!((main_ran + same_ran, main_hits + same_hits), (2, 2));
    assert_eq!(action_counts(&stderr(&results[2])), (2, 0));
    // Afterwards: nothing runs again anywhere.
    for (checkout, value) in [(&main, "one"), (&same, "one"), (&other, "two")] {
        let again = repo.run(checkout, &build);
        assert!(again.status.success(), "{}", stderr(&again));
        assert_eq!(
            count(&stderr(&again), "REPO_EXECUTED"),
            0,
            "{}",
            stderr(&again)
        );
        assert_eq!(action_counts(&stderr(&again)), (0, 2));
        assert_eq!(outputs(&again)[0].1, contents(value));
    }
    assert_eq!(repo.states().len(), 3, "{:?}", repo.states());
    // Each worktree's output tree (`info bazel-bin`, the output base) is
    // its own: the other commit's build never relinked it; the store is
    // shared.
    for (checkout, value) in [(&main, "one"), (&same, "one"), (&other, "two")] {
        let info = repo.run(
            checkout,
            &["info", "output_base", "bazel-bin", "repository_cache"],
        );
        assert!(info.status.success(), "{}", stderr(&info));
        let state = repo.state(checkout);
        let bin = state.join("execroot/_main/bazel-out/k8-fastbuild/bin");
        assert_eq!(
            stdout(&info),
            format!(
                "output_base: {}\nbazel-bin: {}\nrepository_cache: {}\n",
                state.display(),
                bin.display(),
                repo.cache().join("repositories/downloads").display()
            )
        );
        assert_eq!(
            fs::read_to_string(bin.join("second.txt")).unwrap(),
            contents(value)
        );
    }
}

const CACHE_PATHS: &str = r#"
def _base(ctx):
    ctx.file("base.txt", "base\n")
    ctx.file("BUILD.bazel", "exports_files(['base.txt'])\n")

base = repository_rule(_base)

def _impl(ctx):
    print("REPO_EXECUTED " + ctx.name)
    # The repository's own directory (in the cache inside the main
    # checkout), and an absolute link into another repository's tree there.
    ctx.file("where.txt", "stage=" + str(ctx.path(".")) + "\n")
    ctx.symlink(ctx.path(Label("@base//:base.txt")), "base.txt")
    ctx.file("BUILD.bazel", "exports_files(['where.txt', 'base.txt'])\n")

cached = repository_rule(_impl)
"#;

/// The main checkout's cache lies inside its workspace: a repository whose
/// files name a path into that cache, or link into it, depends on no
/// workspace (every worktree sharing the cache sees the same paths), so a
/// linked worktree reuses it.
#[test]
fn paths_into_the_shared_cache_are_no_workspace_dependence() {
    let repo = Repo::new(&[
        (
            "MODULE.bazel",
            "module(name = 'main')\nbase = use_repo_rule('//:repo.bzl', 'base')\nbase(name = 'base')\ncached = use_repo_rule('//:repo.bzl', 'cached')\ncached(name = 'cached')\n",
        ),
        ("repo.bzl", CACHE_PATHS),
        (
            "BUILD.bazel",
            "genrule(name = 'out', srcs = ['@cached//:where.txt', '@cached//:base.txt'], outs = ['out.txt'], cmd = 'cat $(SRCS) > $@')\n",
        ),
    ]);
    let main = repo.main();
    let wt = repo.worktree("wt");
    let first = repo.run(&main, &["build", "//:out"]);
    assert!(first.status.success(), "{}", stderr(&first));
    assert_eq!(count(&stderr(&first), "REPO_EXECUTED"), 1);
    let content = outputs(&first)[0].1.clone();
    assert!(
        content.starts_with(&format!("stage={}/", repo.cache().display()))
            && content.ends_with("\nbase\n"),
        "{content}"
    );
    let second = repo.run(&wt, &["build", "//:out"]);
    assert!(second.status.success(), "{}", stderr(&second));
    assert_eq!(
        count(&stderr(&second), "REPO_EXECUTED"),
        0,
        "{}",
        stderr(&second)
    );
    assert_eq!(action_counts(&stderr(&second)), (0, 1));
    assert_eq!(outputs(&second)[0].1, content);
    // No publication recorded a workspace, and no workspace pointer exists.
    let specs = repo.cache().join("repositories/direct/specs");
    for entry in fs::read_dir(&specs).unwrap().flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        assert!(!name.contains(".ws-"), "{name}");
    }
}

const LOCATING_RULE: &str = r#"
def _impl(ctx):
    print("REPO_EXECUTED " + ctx.name)
    # Finds a workspace file through workspace_root (as aspect's telemetry
    # repository does) but writes no path.
    readme = ctx.path(str(ctx.workspace_root) + "/README.md")
    ctx.file("value.txt", ctx.read(readme))
    ctx.file("BUILD.bazel", "exports_files(['value.txt'])\n")

locating = repository_rule(_impl)
"#;

/// A repository rule that read `workspace_root` depends on the workspace
/// (docs/WORKTREES.md): each worktree executes it once and keeps its own
/// publication under its own pointer — switching between the worktrees then
/// executes nothing, and each one's analysis snapshot keeps hitting.
#[test]
fn workspace_dependent_repositories_are_kept_per_worktree_without_thrashing() {
    let repo = Repo::new(&[
        (
            "MODULE.bazel",
            "module(name = 'main')\nlocating = use_repo_rule('//:repo.bzl', 'locating')\nlocating(name = 'located')\n",
        ),
        ("repo.bzl", LOCATING_RULE),
        ("README.md", "readme\n"),
        (
            "BUILD.bazel",
            "genrule(name = 'out', srcs = ['@located//:value.txt'], outs = ['out.txt'], cmd = 'cat $< > $@')\n",
        ),
    ]);
    let main = repo.main();
    let wt = repo.worktree("wt");
    let env = [
        ("BAZEL_RS_SNAPSHOT", "on"),
        ("BAZEL_RS_SNAPSHOT_EXPLAIN", "1"),
    ];
    let build = |checkout: &Path| {
        let output = repo.run_with(checkout, &["build", "//:out"], &env);
        assert!(output.status.success(), "{}", stderr(&output));
        assert_eq!(outputs(&output)[0].1, "readme\n");
        stderr(&output)
    };
    for checkout in [&main, &wt] {
        let first = build(checkout);
        assert_eq!(count(&first, "REPO_EXECUTED"), 1, "{first}");
    }
    // Settle: each worktree saves once its repositories were reused.
    for checkout in [&main, &wt] {
        let settled = build(checkout);
        assert_eq!(count(&settled, "REPO_EXECUTED"), 0, "{settled}");
        assert!(settled.contains("bazel-rs-snapshot: saved "), "{settled}");
    }
    for _ in 0..2 {
        for checkout in [&main, &wt] {
            let hit = build(checkout);
            assert!(hit.contains("bazel-rs-snapshot: hit "), "{hit}");
            assert_eq!(count(&hit, "REPO_EXECUTED"), 0, "{hit}");
        }
    }
    let specs: Vec<String> = fs::read_dir(repo.cache().join("repositories/direct/specs"))
        .unwrap()
        .flatten()
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .filter(|name| !name.starts_with('.'))
        .collect();
    assert_eq!(specs.len(), 2, "{specs:?}");
    assert!(specs.iter().all(|name| name.contains(".ws-")), "{specs:?}");
}

/// The layouts git itself writes: a worktree added with relative paths
/// shares the main checkout's cache, a worktree reached through a symlink
/// is the same worktree (its cache and its state), and the worktrees of a
/// bare repository — no main checkout — keep their own caches.
#[test]
fn real_git_layouts_choose_the_cache() {
    let repo = Repo::new(&[
        ("MODULE.bazel", "module(name = 'main')\n"),
        (
            "BUILD.bazel",
            "genrule(name = 'out', outs = ['out.txt'], cmd = 'echo out > $@')\n",
        ),
    ]);
    let built_in = |checkout: &Path| {
        let output = repo.run(checkout, &["build", "//:out"]);
        assert!(output.status.success(), "{}", stderr(&output));
        PathBuf::from(&outputs(&output)[0].0)
    };
    let relative = repo.base().join("relative");
    repo.git(
        &repo.main(),
        &[
            "worktree",
            "add",
            "-q",
            "--relative-paths",
            "-b",
            "relative",
            relative.to_str().unwrap(),
        ],
    );
    let dot_git = fs::read_to_string(relative.join(".git")).unwrap();
    assert!(dot_git.starts_with("gitdir: ../"), "{dot_git}");
    assert!(built_in(&relative).starts_with(repo.cache()));
    // Through a symlink: the same state directory as the real path.
    let link = repo.base().join("link-to-relative");
    std::os::unix::fs::symlink(&relative, &link).unwrap();
    assert!(built_in(&link).starts_with(repo.cache()));
    assert_eq!(repo.states().len(), 1, "{:?}", repo.states());
    repo.state(&relative);
    // A bare repository's worktree: its own cache.
    let bare = repo.base().join("bare.git");
    repo.git(
        &repo.base(),
        &[
            "clone",
            "-q",
            "--bare",
            repo.main().to_str().unwrap(),
            bare.to_str().unwrap(),
        ],
    );
    let of_bare = repo.base().join("of-bare");
    repo.git(
        &bare,
        &["worktree", "add", "-q", of_bare.to_str().unwrap(), "main"],
    );
    assert!(built_in(&of_bare).starts_with(of_bare.join("bazel-rs-out")));
    assert!(!repo.cache().join("worktrees").join("of-bare").exists());
}
