//! MODULE.bazel.lock `moduleExtensions`/`facts` CLI behavior on
//! tests/fixtures/lockfile_extensions (docs/REGISTRY_LOCKFILE.md): entries
//! written like Bazel 9.3, results reused from the workspace and hidden
//! lockfiles, error-mode staleness texts, facts passed to extensions. Uses the
//! local builtin-module fixture registry only; never a reference Bazel.
#![allow(clippy::disallowed_methods)] // test harness: starts processes directly (clippy.toml)
include!("support/verify_env.rs");
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
include!("support/explicit_modules.rs");

const FIXTURE: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/tests/fixtures/lockfile_extensions"
);
const REGISTRY: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/tests/fixtures/builtin_modules/registry"
);
const ALL: &str = "@gen_alpha//:all + @gen_beta//:all + @gen_gamma//:all + @gen_plat//:all + @gen_repro//:all + @gen_meta//:all + @gen_facts//:all + @dep_gen//:all";

struct Fixture {
    dir: tempfile::TempDir,
}

fn copy_tree(from: &Path, to: &Path) {
    fs::create_dir_all(to).unwrap();
    for entry in fs::read_dir(from).unwrap() {
        let entry = entry.unwrap();
        let target = to.join(entry.file_name());
        if entry.file_type().unwrap().is_dir() {
            copy_tree(&entry.path(), &target);
        } else {
            fs::copy(entry.path(), target).unwrap();
        }
    }
}

impl Fixture {
    fn new() -> Self {
        let dir = tempfile::Builder::new()
            .prefix("lockfile-extensions-")
            .tempdir_in(crate::test_temp::root())
            .unwrap();
        copy_tree(
            &Path::new(FIXTURE).join("workspace"),
            &dir.path().join("ws"),
        );
        for name in ["home", "tmp"] {
            fs::create_dir(dir.path().join(name)).unwrap();
        }
        Self { dir }
    }

    fn ws(&self) -> PathBuf {
        self.dir.path().join("ws")
    }

    fn lockfile(&self) -> Option<String> {
        fs::read_to_string(self.ws().join("MODULE.bazel.lock")).ok()
    }

    fn run(&self, mode: &str, query: &str, dyn_value: &str) -> Output {
        self.run_with(mode, query, dyn_value, &[])
    }

    fn run_with(&self, mode: &str, query: &str, dyn_value: &str, extra: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_bazel-rs"))
            .arg("--ignore_all_rc_files")
            .arg(format!(
                "--cache-dir={}",
                self.dir.path().join("cache").display()
            ))
            .args(["query", "--experimental-builtin-modules"])
            .arg(format!("--registry=file://{REGISTRY}"))
            .arg(format!("--lockfile_mode={mode}"))
            .args(extra)
            .arg(query)
            .current_dir(self.ws())
            .env_clear()
            .envs(verifier_environment())
            .env("PATH", "/usr/bin:/bin")
            .env("HOME", self.dir.path().join("home"))
            .env("TMPDIR", self.dir.path().join("tmp"))
            .env("LC_ALL", "C.UTF-8")
            .env("LOCKEXT_STATIC", "s 1")
            .env("LOCKEXT_DYN", dyn_value)
            .explicit_modules()
            .output()
            .unwrap()
    }
}

fn evaluated(output: &Output) -> Vec<String> {
    let mut found: Vec<String> = String::from_utf8_lossy(&output.stderr)
        .lines()
        .filter_map(|line| line.find("LOCKEXT_EVAL ").map(|at| line[at..].to_owned()))
        .collect();
    found.sort();
    found
}

fn error(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr)
        .lines()
        .filter_map(|line| line.strip_prefix("bazel-rs: "))
        .next_back()
        .unwrap_or_default()
        .to_owned()
}

/// The genuine Bazel 9.3.0 lockfile of the first capture step.
fn genuine_fresh_lockfile() -> String {
    let receipt: serde_json::Value = serde_json::from_slice(
        &fs::read(Path::new(FIXTURE).join("reference_receipt.json")).unwrap(),
    )
    .unwrap();
    receipt["genuine_lockfiles"]["update_fresh"]
        .as_str()
        .unwrap()
        .to_owned()
}

#[test]
fn update_writes_bazel_entries_and_warm_runs_reuse_them() {
    let f = Fixture::new();
    let fresh = f.run("update", ALL, "a");
    assert!(fresh.status.success(), "{}", error(&fresh));
    assert_eq!(
        evaluated(&fresh),
        [
            "LOCKEXT_EVAL dep_ext",
            "LOCKEXT_EVAL ext",
            "LOCKEXT_EVAL facts previous=none",
            "LOCKEXT_EVAL meta",
            "LOCKEXT_EVAL plat",
            "LOCKEXT_EVAL repro"
        ]
    );
    let written = f.lockfile().unwrap();
    // The capture host is linux/amd64 (os:linux,arch:amd64 eval factors).
    if cfg!(all(target_os = "linux", target_arch = "x86_64")) {
        assert_eq!(written, genuine_fresh_lockfile());
    }
    // Non-reproducible results come from MODULE.bazel.lock, reproducible ones
    // from the hidden lockfile: nothing is evaluated, nothing is written.
    for mode in ["update", "refresh", "error"] {
        let warm = f.run(mode, ALL, "a");
        assert!(warm.status.success(), "{mode}: {}", error(&warm));
        assert!(
            evaluated(&warm).is_empty(),
            "{mode}: {:?}",
            evaluated(&warm)
        );
        assert_eq!(f.lockfile().unwrap(), written, "{mode}");
    }
    // Off neither reads nor writes: everything is evaluated again.
    let off = f.run("off", ALL, "a");
    assert!(off.status.success(), "{}", error(&off));
    assert_eq!(evaluated(&off).len(), 6);
    // Without the hidden lockfile (fresh cache) only the reproducible
    // extensions run again; the facts reach module_ctx.facts.
    fs::remove_dir_all(f.dir.path().join("cache")).unwrap();
    let cold = f.run("update", ALL, "a");
    assert!(cold.status.success(), "{}", error(&cold));
    assert_eq!(
        evaluated(&cold),
        ["LOCKEXT_EVAL facts previous=abc123", "LOCKEXT_EVAL repro"]
    );
    assert_eq!(f.lockfile().unwrap(), written);
}

#[test]
fn error_mode_reports_bazel_staleness_reasons() {
    let f = Fixture::new();
    let missing = f.run("error", "@gen_plat//:all", "a");
    assert!(!missing.status.success());
    assert_eq!(
        error(&missing),
        "The module extension '@@//:platform_ext.bzl%plat' for platform os:linux,arch:amd64 does not exist in the lockfile"
            .replace("os:linux,arch:amd64", &platform_key())
    );
    assert!(f.lockfile().is_none());
    assert!(f.run("update", ALL, "a").status.success());
    let changed = f.run("error", ALL, "b");
    assert!(!changed.status.success());
    assert_eq!(
        error(&changed),
        "MODULE.bazel.lock is no longer up-to-date because an input to the extension '@@//:ext.bzl%ext' changed: environment variable LOCKEXT_DYN changed: 'a' -> 'b'. Please run `bazel mod deps --lockfile_mode=update` to update your lockfile."
    );
    fs::write(f.ws().join("helpers.bzl"), {
        let mut text = fs::read_to_string(f.ws().join("helpers.bzl")).unwrap();
        text.push_str("# changed\n");
        text
    })
    .unwrap();
    let both = f.run("error", ALL, "b");
    assert_eq!(
        error(&both),
        "MODULE.bazel.lock is no longer up-to-date because the implementation of the extension '@@//:ext.bzl%ext' or one of its transitive .bzl files has changed,an input to the extension '@@//:ext.bzl%ext' changed: environment variable LOCKEXT_DYN changed: 'a' -> 'b'. Please run `bazel mod deps --lockfile_mode=update` to update your lockfile."
    );
    // Update re-evaluates only the stale extension and rewrites its entry.
    let update = f.run("update", ALL, "b");
    assert!(update.status.success(), "{}", error(&update));
    assert_eq!(evaluated(&update), ["LOCKEXT_EVAL ext"]);
    assert!(f.lockfile().unwrap().contains("\"ENV:LOCKEXT_DYN b\""));
}

#[test]
fn edited_facts_are_passed_to_the_extension_and_validated() {
    let f = Fixture::new();
    assert!(f.run("update", ALL, "a").status.success());
    let original = f.lockfile().unwrap();
    let edited = original.replace("\"hash\": \"abc123\"", "\"hash\": \"edited\"");
    assert_ne!(edited, original);
    fs::write(f.ws().join("MODULE.bazel.lock"), &edited).unwrap();
    let error_run = f.run("error", "@gen_facts//:all", "a");
    assert_eq!(
        evaluated(&error_run),
        ["LOCKEXT_EVAL facts previous=edited"]
    );
    assert!(
        error(&error_run).starts_with(
            "MODULE.bazel.lock is no longer up-to-date because the extension '@@//:facts_ext.bzl%facts_ext' has changed its facts: {\"hash\": \"abc123\""
        ),
        "{}",
        error(&error_run)
    );
    let update = f.run("update", "@gen_facts//:all", "a");
    assert!(update.status.success(), "{}", error(&update));
    assert_eq!(evaluated(&update), ["LOCKEXT_EVAL facts previous=edited"]);
    assert_eq!(f.lockfile().unwrap(), original);
}

#[test]
fn readonly_probes_write_no_hidden_lockfile_but_read_existing_ones() {
    let f = Fixture::new();
    // The hidden lockfile lives in the workspace's state directory,
    // `<cache>/worktrees/<name>-<hash>/` (docs/WORKTREES.md).
    let hidden = || {
        fs::read_dir(f.dir.path().join("cache/worktrees"))
            .into_iter()
            .flatten()
            .flatten()
            .any(|state| state.path().join("MODULE.bazel.lock").exists())
    };
    for _ in 0..2 {
        let run = f.run_with("update", ALL, "a", &["--native-lockfile=readonly"]);
        assert!(run.status.success(), "{}", error(&run));
        // Stateless: every extension runs again, nothing is persisted.
        assert_eq!(evaluated(&run).len(), 6, "{:?}", evaluated(&run));
        assert!(f.lockfile().is_none());
        assert!(!hidden());
    }
    // Lockfiles written by an update run are still read (and reused).
    assert!(f.run("update", ALL, "a").status.success());
    let lockfile = f.lockfile().unwrap();
    assert!(hidden());
    let run = f.run_with("update", ALL, "a", &["--native-lockfile=readonly"]);
    assert!(run.status.success(), "{}", error(&run));
    assert!(evaluated(&run).is_empty(), "{:?}", evaluated(&run));
    assert_eq!(f.lockfile().unwrap(), lockfile);
}

fn platform_key() -> String {
    let os = if cfg!(target_os = "linux") {
        "linux"
    } else {
        "osx"
    };
    let arch = if cfg!(target_arch = "x86_64") {
        "amd64"
    } else {
        "aarch64"
    };
    format!("os:{os},arch:{arch}")
}
