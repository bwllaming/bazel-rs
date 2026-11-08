//! BAZEL_RS_ANALYSIS=eval on the pool (docs/EVALUATOR.md, S2 part 1): phase
//! 1's loading nodes and configured targets evaluated in parallel, then
//! linearized, give the walk's output byte for byte at 1, 2, 8 and 32
//! threads and under `BAZEL_RS_EVAL_JITTER`: print() order across packages
//! loaded in parallel (BUILD lines, a shared `.bzl` module's lines once, at
//! its first sequential load), a local repository a package needs (the
//! sequential pass loads it), a Starlark flag's config_setting (its flush
//! runs in the sequential pass), the incremental-tracking stdout, and the
//! first error (a missing target, a dependency cycle, a package group
//! include cycle) with the same exit code.
#![allow(clippy::disallowed_methods)] // test harness: starts processes directly (clippy.toml)
include!("support/temp_root.rs");
use std::{
    fs,
    path::Path,
    process::{Command, Output},
};

fn files() -> Vec<(String, String)> {
    let mut files = vec![
        (
            "MODULE.bazel".to_owned(),
            "bazel_dep(name = 'local')\nlocal_path_override(module_name = 'local', path = 'local')\n"
                .to_owned(),
        ),
        (
            "local/MODULE.bazel".to_owned(),
            "module(name = 'local')\n".to_owned(),
        ),
        (
            "local/BUILD.bazel".to_owned(),
            "load('//:printing.bzl', 'say')\nsay('local BUILD')\nfilegroup(name = 'files', srcs = ['data.txt'], visibility = ['//visibility:public'])\n"
                .to_owned(),
        ),
        ("local/data.txt".to_owned(), "data\n".to_owned()),
        (
            "local/printing.bzl".to_owned(),
            "print('local printing.bzl top-level')\ndef say(text):\n    print(text)\n".to_owned(),
        ),
        (
            "shared.bzl".to_owned(),
            "print('shared.bzl top-level')\ndef say(text):\n    print(text)\n".to_owned(),
        ),
        (
            "flag.bzl".to_owned(),
            "def _impl(ctx):\n    return []\nstring_flag = rule(implementation = _impl, build_setting = config.string(flag = True))\n"
                .to_owned(),
        ),
    ];
    let mut root_srcs = Vec::new();
    for i in 0..24 {
        let package = format!("p{i}");
        let mut deps: Vec<String> = Vec::new();
        if i + 1 < 24 {
            deps.push(format!("'//p{}:t'", i + 1));
        }
        if i % 5 == 0 {
            deps.push("'@local//:files'".to_owned());
        }
        if i % 3 == 0 {
            files.push((
                format!("{package}/own.bzl"),
                format!("print('own {i} top-level')\nOWN = {i}\n"),
            ));
        }
        let own = if i % 3 == 0 {
            "load(':own.bzl', 'OWN')\n".to_owned()
        } else {
            String::new()
        };
        files.push((
            format!("{package}/BUILD.bazel"),
            format!(
                "load('//:shared.bzl', 'say')\n{own}say('BUILD {i}')\nfilegroup(name = 't', srcs = ['f.txt'] + select({{'//:flag_on': ['g.txt'], '//conditions:default': []}}), data = [{}], visibility = ['//visibility:public'])\n",
                deps.join(", ")
            ),
        ));
        files.push((format!("{package}/f.txt"), "f\n".to_owned()));
        files.push((format!("{package}/g.txt"), "g\n".to_owned()));
        root_srcs.push(format!("'//p{i}:t'"));
    }
    files.push((
        "BUILD.bazel".to_owned(),
        format!(
            "load(':flag.bzl', 'string_flag')\nstring_flag(name = 'flag', build_setting_default = 'off')\nconfig_setting(name = 'flag_on', flag_values = {{':flag': 'on'}})\nfilegroup(name = 'all', srcs = [{}])\nfilegroup(name = 'missing', srcs = ['//p3:t', '//p4:nope'])\nfilegroup(name = 'cycle_a', srcs = [':cycle_b'])\nfilegroup(name = 'cycle_b', srcs = [':cycle_a'])\npackage_group(name = 'ga', includes = [':gb'])\npackage_group(name = 'gb', includes = [':ga'])\nfilegroup(name = 'grouped', srcs = ['//p2:t'], visibility = [':ga'])\nfilegroup(name = 'uses_grouped', srcs = [':grouped'])\n",
            root_srcs.join(", ")
        ),
    ));
    files
}

struct Workspace(tempfile::TempDir);

impl Workspace {
    fn new() -> Self {
        let workspace = Self(tempfile::tempdir_in(test_temp::root()).unwrap());
        for (path, text) in files() {
            let path = workspace.0.path().join(path);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, text).unwrap();
        }
        for directory in ["home", "tmp"] {
            fs::create_dir(workspace.0.path().join(directory)).unwrap();
        }
        workspace
    }

    fn path(&self) -> &Path {
        self.0.path()
    }

    fn build(&self, mode: &str, threads: usize, jitter: u64, args: &[&str]) -> Output {
        // A fresh cache per run: every package loads (and prints) again.
        let _ = fs::remove_dir_all(self.path().join("cache"));
        Command::new(env!("CARGO_BIN_EXE_bazel-rs"))
            .current_dir(self.path())
            .env_clear()
            .env("PATH", "/usr/bin:/bin")
            .env("HOME", self.path().join("home"))
            .env("TMPDIR", self.path().join("tmp"))
            .env("BAZEL_RS_CACHE_GC", "off")
            .env("BAZEL_RS_SNAPSHOT", "off")
            .env("BAZEL_RS_ANALYSIS", mode)
            .env("BAZEL_RS_EVAL_JITTER", jitter.to_string())
            .args([
                "--ignore_all_rc_files",
                "--cache-dir=cache",
                "build",
                "--experimental-builtin-modules=false",
                "--lockfile_mode=off",
                "--memory-limit=2048",
                "--nobuild",
            ])
            .arg(format!("--loading_phase_threads={threads}"))
            .args(args)
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

#[test]
fn the_pool_gives_the_walks_output_at_every_thread_count() {
    let ws = Workspace::new();
    for args in [
        &["//:all"][..],
        &["//:all", "--//:flag=on"],
        &["//:missing"],
        &["//:cycle_a"],
        &["//:uses_grouped"],
    ] {
        let walk = ws.build("walk", 1, 0, args);
        let text = stable(&walk);
        if args == ["//:all"] {
            assert!(walk.status.success(), "{text}");
            assert_eq!(text.matches("shared.bzl top-level").count(), 1, "{text}");
            assert_eq!(text.matches("local BUILD").count(), 1, "{text}");
            assert!(text.contains("BUILD 23"), "{text}");
        }
        for (threads, jitter) in [(1, 0), (2, 0), (8, 0), (32, 0), (8, 300), (32, 100)] {
            let eval = ws.build("eval", threads, jitter, args);
            let what = format!("{args:?} at {threads} threads, jitter {jitter}");
            assert_eq!(
                eval.status.code(),
                walk.status.code(),
                "{what}: {}",
                stable(&eval)
            );
            assert_eq!(stable(&eval), text, "{what}");
            assert_eq!(eval.stdout, walk.stdout, "{what}");
        }
    }
}
