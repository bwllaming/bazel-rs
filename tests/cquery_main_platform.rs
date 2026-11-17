//! `cquery --platforms=//:p` with a target platform of the main repository
//! (tests/fixtures/cquery_main_platform/README.md). The evaluator's output
//! must be the walk's byte for byte at several thread counts and under
//! `BAZEL_RS_EVAL_RESTARTS`. Before the fix, a pool attempt of the genrule
//! swallowed the `@bazel_tools` sentinel and then stopped on a `Missing`,
//! and its rerun failed with "re-emits MemoUse". The output must also be
//! genuine Bazel 9.3.1's, compared as in tests/cquery_output_formats.rs.
#![allow(clippy::disallowed_methods)] // test harness: starts processes directly (clippy.toml)
include!("support/temp_root.rs");
include!("support/module_archives.rs");
use std::{
    fs,
    path::{Path, PathBuf},
    process::{Command, Output},
};

fn fixture() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/cquery_main_platform")
}

struct Workspace(tempfile::TempDir);

impl Workspace {
    fn new() -> Self {
        let workspace = Self(tempfile::tempdir_in(test_temp::root()).unwrap());
        for name in ["MODULE.bazel", "BUILD.bazel"] {
            fs::copy(
                fixture().join("workspace").join(name),
                workspace.0.path().join(name),
            )
            .unwrap();
        }
        for directory in ["home", "tmp"] {
            fs::create_dir(workspace.0.path().join(directory)).unwrap();
        }
        workspace
    }

    /// `cquery --platforms=//:p 'deps(//:gt)'` on a fresh cache (so
    /// `@bazel_tools` is not fetched yet when the analysis starts).
    fn cquery(&self, mode: &str, threads: usize, env: &[(&str, &str)]) -> Output {
        let root = self.0.path();
        let _ = fs::remove_dir_all(root.join("cache"));
        Command::new(env!("CARGO_BIN_EXE_bazel-rs"))
            .current_dir(root)
            .env_clear()
            .env("BAZEL_RS_PROGRESS", "0")
            .env("PATH", "/usr/bin:/bin")
            .env("HOME", root.join("home"))
            .env("TMPDIR", root.join("tmp"))
            .env("BAZEL_RS_CACHE_GC", "off")
            .env("BAZEL_RS_SNAPSHOT", "off")
            .env("BAZEL_RS_ANALYSIS", mode)
            .envs(env.iter().copied())
            .args(with_builtin_registry(&[
                "--ignore_all_rc_files".to_owned(),
                "--cache-dir=cache".to_owned(),
                "cquery".to_owned(),
                "--lockfile_mode=off".to_owned(),
                "--memory-limit=2048".to_owned(),
                format!("--loading_phase_threads={threads}"),
                "--platforms=//:p".to_owned(),
                "deps(//:gt)".to_owned(),
            ]))
            .seeded_archives()
            .output()
            .unwrap()
    }
}

/// stderr without the timing and memory lines.
fn stable(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr)
        .lines()
        .filter(|line| {
            !line.starts_with("INFO: Elapsed time")
                && !line.starts_with("bazel-rs: memory limit")
                && !line.starts_with("PERF")
                && !line.starts_with("bazel-rs: peak RSS")
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Sorted distinct lines, written with the `@bazel_tools`/`@platforms`
/// spelling, with 7-hex configuration checksums replaced by `H`.
fn normalized(text: &str) -> Vec<String> {
    let mut lines: Vec<String> = text
        .lines()
        .map(|line| {
            let line = line
                .replace("@@platforms//", "@platforms//")
                .replace("@@bazel_tools//", "@bazel_tools//");
            match line.rsplit_once(" (") {
                Some((label, config))
                    if config.len() == 8
                        && config.ends_with(')')
                        && config[..7].bytes().all(|b| b.is_ascii_hexdigit()) =>
                {
                    format!("{label} (H)")
                }
                _ => line,
            }
        })
        .collect();
    lines.sort();
    lines.dedup();
    lines
}

#[test]
fn cquery_with_a_main_repository_platform_is_the_walks_and_genuines() {
    let ws = Workspace::new();
    let walk = ws.cquery("walk", 8, &[]);
    assert!(walk.status.success(), "{}", stable(&walk));
    let runs: [(usize, &[(&str, &str)]); 5] = [
        (1, &[]),
        (8, &[]),
        (32, &[]),
        (8, &[("BAZEL_RS_EVAL_RESTARTS", "5")]),
        (8, &[("BAZEL_RS_EVAL_JITTER", "300")]),
    ];
    for (threads, env) in runs {
        let eval = ws.cquery("eval", threads, env);
        let what = format!("at {threads} threads, {env:?}");
        assert_eq!(
            eval.status.code(),
            walk.status.code(),
            "{what}: {}",
            stable(&eval)
        );
        assert_eq!(stable(&eval), stable(&walk), "{what}");
        assert_eq!(eval.stdout, walk.stdout, "{what}");
    }

    let genuine = fs::read_to_string(fixture().join("genuine-deps.out")).unwrap();
    // The known divergence (README.md): genuine also lists the execution
    // platform among the deps.
    let expected: Vec<String> = normalized(&genuine)
        .into_iter()
        .filter(|line| line != "@bazel_tools//tools:host_platform (H)")
        .collect();
    assert_eq!(normalized(&String::from_utf8_lossy(&walk.stdout)), expected);
}
