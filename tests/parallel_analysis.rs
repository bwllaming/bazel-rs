//! P1 of the parallel-analysis program (artifacts/PARALLEL_ANALYSIS.md,
//! oracle review §3): loading runs as commit-ordered units with buffered
//! print output — `.bzl` lines replayed once in load-DFS order — and the
//! no-demand guard's inline fallback reproduces sequential output.
//! Byte-identity of the whole suite is the P1 gate; these tests pin the
//! ordering invariants that make it hold. (BUILD files have no `print` in
//! this profile — it is not among the BUILD globals — so print output
//! comes from `.bzl` top levels and rule implementations only.)

#![allow(clippy::disallowed_methods)] // test harness: starts processes directly (clippy.toml)
use std::fs;
use std::path::PathBuf;
use std::process::{Command, Output};
use std::sync::atomic::{AtomicU64, Ordering};
include!("support/explicit_modules.rs");

static NEXT: AtomicU64 = AtomicU64::new(0);

struct Workspace(PathBuf);

impl Workspace {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!(
            "bazel-rs-parallel-analysis-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&path).unwrap();
        let ws = Self(path);
        ws.file("MODULE.bazel", "module(name='parallel_analysis')\n");
        ws
    }

    fn file(&self, name: &str, content: &str) {
        let path = self.0.join(name);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, content).unwrap();
    }

    fn run_env(&self, env: &[(&str, &str)], args: &[&str]) -> Output {
        let mut command = Command::new(env!("CARGO_BIN_EXE_bazel-rs"));
        command
            .arg("--ignore_all_rc_files")
            .args(args)
            .env_remove("BAZEL_RS_TIMING")
            .current_dir(&self.0);
        for (key, value) in env {
            command.env(key, value);
        }
        command.explicit_modules().output().unwrap()
    }

    fn run(&self, args: &[&str]) -> Output {
        self.run_env(&[], args)
    }
}

impl Drop for Workspace {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

fn prints(output: &Output) -> Vec<String> {
    stderr(output)
        .lines()
        .filter(|line| {
            !line.starts_with("WARNING:")
                && !line.starts_with("INFO:")
                && !line.starts_with("bazel-rs: ")
        })
        .map(str::to_owned)
        .collect()
}

/// One `.bzl` graph with top-level prints, loaded by many packages: the
/// lines appear exactly once, in the first loading package's load-DFS
/// order (dependencies before dependents; review §1.4).
#[test]
fn bzl_prints_emit_once_in_load_dfs_order() {
    let ws = Workspace::new();
    ws.file("rules/BUILD", "");
    ws.file(
        "rules/helper.bzl",
        "print('helper top-level')\ndef helper():\n    return 1\n",
    );
    ws.file(
        "rules/lib.bzl",
        "load(':helper.bzl', 'helper')\nprint('lib top-level')\ndef _impl(ctx):\n    return DefaultInfo()\nmy_rule = rule(implementation = _impl)\n",
    );
    for package in ["a", "b", "c"] {
        ws.file(
            &format!("{package}/BUILD"),
            &format!("load('//rules:lib.bzl', 'my_rule')\nmy_rule(name = '{package}')\n"),
        );
    }
    let output = ws.run(&["build", "//a/...", "//b/...", "//c/..."]);
    let stderr = stderr(&output);
    assert!(output.status.success(), "{stderr}");
    // helper evaluates before lib (postorder); packages b and c reuse the
    // cached modules and print nothing.
    assert_eq!(
        prints(&output),
        vec!["helper top-level".to_owned(), "lib top-level".to_owned()],
        "{stderr}"
    );
}

/// The no-demand guard's inline fallback (review §1.2) reproduces the
/// sequential loading output on a warm workspace: the guard aborts
/// nothing when every repository is materialized, and a guard abort
/// replays module prints and re-runs the unit inline.
#[test]
fn no_demand_guard_reproduces_sequential_output() {
    let ws = Workspace::new();
    ws.file("rules/BUILD", "");
    ws.file(
        "rules/def.bzl",
        "print('def top-level')\ndef _impl(ctx):\n    print('impl runs')\n    return DefaultInfo()\nmy_rule = rule(implementation = _impl)\n",
    );
    ws.file(
        "a/BUILD",
        "load('//rules:def.bzl', 'my_rule')\nmy_rule(name = 'a')\n",
    );
    let sequential = ws.run(&["build", "//a:a"]);
    assert!(sequential.status.success(), "{}", stderr(&sequential));
    let guarded = ws.run_env(&[("BAZEL_RS_LOADING_NO_DEMAND", "1")], &["build", "//a:a"]);
    assert!(guarded.status.success(), "{}", stderr(&guarded));
    assert_eq!(
        prints(&sequential),
        prints(&guarded),
        "guard-on and guard-off loading must be byte-identical (warm)"
    );
    // Each buffered source emits exactly once.
    let stderr = stderr(&guarded);
    assert_eq!(stderr.matches("def top-level").count(), 1);
    assert_eq!(stderr.matches("impl runs").count(), 1);
}

/// A failing `.bzl` prints its partial top-level lines once per
/// invocation; every loading package in it sees the stored error
/// (BzlLoadValue failure replay).
#[test]
fn failing_bzl_prints_once_and_replays_its_error() {
    let ws = Workspace::new();
    ws.file("rules/BUILD", "");
    ws.file("rules/bad.bzl", "print('bad evaluates')\nfail('boom')\n");
    ws.file("a/BUILD", "load('//rules:bad.bzl', 'x')\nx = 1\n");
    ws.file("b/BUILD", "load('//rules:bad.bzl', 'x')\nx = 1\n");
    let output = ws.run(&["build", "//a:a", "//b:b"]);
    assert!(!output.status.success());
    let stderr = stderr(&output);
    assert_eq!(stderr.matches("bad evaluates").count(), 1, "{stderr}");
    // The stored error replays for the second loading package too; the
    // reported failure is the first one in loading order (the formatted
    // error echoes the failing line, so count the headline).
    assert!(stderr.contains("error: fail: boom"), "{stderr}");
    assert_eq!(
        stderr.matches("cannot evaluate //rules:bad.bzl").count(),
        1,
        "{stderr}"
    );
}

/// Implementation print output emits from the commit, once per analyzed
/// target, in plan order (review §1.8).
#[test]
fn implementation_prints_commit_in_plan_order() {
    let ws = Workspace::new();
    ws.file("rules/BUILD", "");
    ws.file(
        "rules/def.bzl",
        "def _impl(ctx):\n    print('impl ' + ctx.label.name)\n    return DefaultInfo()\nmy_rule = rule(implementation = _impl)\n",
    );
    for package in ["a", "b", "c"] {
        ws.file(
            &format!("{package}/BUILD"),
            &format!("load('//rules:def.bzl', 'my_rule')\nmy_rule(name = '{package}')\n"),
        );
    }
    let output = ws.run(&["build", "//a:a", "//b:b", "//c:c"]);
    assert!(output.status.success(), "{}", stderr(&output));
    assert_eq!(
        prints(&output),
        vec![
            "impl a".to_owned(),
            "impl b".to_owned(),
            "impl c".to_owned()
        ],
        "{}",
        stderr(&output)
    );
}
