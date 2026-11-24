//! A build that fails fast (no `--keep_going`) exits promptly after the
//! first error. The evaluator's frontier-only mode (docs/EVALUATOR.md
//! "Frontier-only") runs only what the linearizer still needs; a wanted
//! node's pool attempt used to set aside each fresh dependency it helped
//! (not wanted yet) and restart once per dependency it needs one after
//! another — a filegroup of a repository's 10k files restarted ~10k times,
//! and ~/code's `build --nobuild //src/...` took 300-1200 s instead of ~15 s
//! to report an open-file-limit package error. Here a repository's
//! filegroup of many files is analyzed after an unrelated target failed:
//! no node restarts once per file (`BAZEL_RS_EVAL_TEST_MAX_ATTEMPTS`), and
//! the build exits soon, with and without `--keep_going`.
#![allow(clippy::disallowed_methods)] // test harness: starts processes directly (clippy.toml)
include!("support/temp_root.rs");
use std::{
    fs,
    path::Path,
    process::{Command, Output},
    time::{Duration, Instant},
};

/// Files in the repository's filegroup (one `Src` need each).
const FILES: usize = 10_000;

struct Workspace(tempfile::TempDir);

impl Workspace {
    fn new() -> Self {
        let workspace = Self(
            tempfile::Builder::new()
                .prefix("eval-fail-fast-")
                .tempdir_in(test_temp::root())
                .unwrap(),
        );
        let files = [
            (
                "MODULE.bazel",
                "module(name = 'fail_fast')\n\next = use_repo_rule('//:ext.bzl', 'ext')\n\next(name = 'ext')\n"
                    .to_owned(),
            ),
            // The repository takes a moment (the first error is published
            // meanwhile), then holds many files one filegroup globs.
            (
                "ext.bzl",
                format!(
                    "def _ext_impl(ctx):\n    ctx.execute(['sh', '-c', 'sleep 1; mkdir -p files; i=0; while [ $i -lt {FILES} ]; do : > files/f$i.txt; i=$((i+1)); done'])\n    ctx.file('BUILD.bazel', 'filegroup(name = \"big\", srcs = glob([\"files/**\"]), visibility = [\"//visibility:public\"])\\n')\n\next = repository_rule(implementation = _ext_impl)\n"
                ),
            ),
            (
                "BUILD.bazel",
                "filegroup(name = 'top', srcs = ['@ext//:big'])\n".to_owned(),
            ),
            (
                "bad/BUILD.bazel",
                "filegroup(name = 'bad', srcs = ['//missing:x'])\n".to_owned(),
            ),
        ];
        for (path, text) in files {
            let path = workspace.path().join(path);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, text).unwrap();
        }
        for directory in ["home", "tmp"] {
            fs::create_dir(workspace.path().join(directory)).unwrap();
        }
        workspace
    }

    fn path(&self) -> &Path {
        self.0.path()
    }

    /// `build --nobuild //:top //bad:bad` on a fresh cache; its output and
    /// wall time.
    fn build(&self, extra: &[&str]) -> (Output, Duration) {
        self.writable();
        let _ = fs::remove_dir_all(self.path().join("cache"));
        let started = Instant::now();
        let output = Command::new(TIMEOUT)
            .args(["--kill-after=5s", "300s"])
            .arg(env!("CARGO_BIN_EXE_bazel-rs"))
            .current_dir(self.path())
            .env_clear()
            .env("BAZEL_RS_PROGRESS", "0")
            .env("PATH", "/usr/bin:/bin")
            .env("HOME", self.path().join("home"))
            .env("TMPDIR", self.path().join("tmp"))
            .env("BAZEL_RS_CACHE_GC", "off")
            .env("BAZEL_RS_SNAPSHOT", "off")
            .env("BAZEL_RS_EVAL_TEST_MAX_ATTEMPTS", "20")
            .args([
                "--ignore_all_rc_files",
                "--cache-dir=cache",
                "build",
                "--experimental-builtin-modules=false",
                "--lockfile_mode=off",
                "--memory-limit=2048",
                "--loading_phase_threads=8",
                "--nobuild",
            ])
            .args(extra)
            .args(["//:top", "//bad:bad"])
            .output()
            .unwrap();
        (output, started.elapsed())
    }

    /// The cache's read-only repository trees made writable (removable).
    fn writable(&self) {
        let cache = self.path().join("cache");
        if cache.exists() {
            let _ = Command::new("chmod")
                .arg("-R")
                .arg("u+w")
                .arg(&cache)
                .status();
        }
    }
}

impl Drop for Workspace {
    fn drop(&mut self) {
        self.writable();
    }
}

fn check(output: &Output, elapsed: Duration, what: &str) {
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_ne!(
        output.status.code(),
        Some(124),
        "{what}: timed out\n{stderr}"
    );
    assert_eq!(output.status.code(), Some(1), "{what}\n{stderr}");
    assert!(
        stderr.contains("no such package 'missing'"),
        "{what}: the error\n{stderr}"
    );
    assert!(
        !stderr.contains("BAZEL_RS_EVAL_TEST_MAX_ATTEMPTS"),
        "{what}: a node restarted once per dependency\n{stderr}"
    );
    // ~2-4 s here (the repository's 1 s and its files included); before
    // the fix, fail fast restarted the filegroup hundreds of times.
    assert!(
        elapsed < Duration::from_secs(60),
        "{what}: {elapsed:?} to fail\n{stderr}"
    );
}

#[test]
fn fail_fast_exits_promptly_after_the_first_error() {
    let ws = Workspace::new();
    // Twice: the sequential pass may take the filegroup over early (a race).
    for _ in 0..2 {
        let (output, elapsed) = ws.build(&[]);
        check(&output, elapsed, "fail fast");
    }
    let (output, elapsed) = ws.build(&["--keep_going"]);
    check(&output, elapsed, "--keep_going");
}
