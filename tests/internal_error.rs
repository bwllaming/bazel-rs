//! A panic is an internal error on every thread (crate::internal_error):
//! the command prints it and exits with Bazel's internal-error exit code
//! 37 promptly, never hangs (a coordinator unwinding from a panic in a
//! frozen-prepare walk joined lowering workers a handle held further up
//! still fed) and never passes (a panic on the planning drop thread went
//! unnoticed). Each site of the hidden hook `BAZEL_RS_DEBUG_PANIC_AT`, in
//! the walk, in BAZEL_RS_ANALYSIS=eval and with
//! BAZEL_RS_EVAL_FROZEN_PREPARES=all.
#![allow(clippy::disallowed_methods)] // test harness: starts processes directly (clippy.toml)
include!("support/temp_root.rs");
use std::{
    fs,
    io::Read,
    path::Path,
    process::{Command, Stdio},
    time::{Duration, Instant},
};

/// The bound on a crashing command (a debug binary analyses this
/// workspace in well under a second).
const BOUND: Duration = Duration::from_secs(30);

/// Independent targets enough for the analysis and lowering workers to
/// run work ahead of its turn, a genrule per package for the drop thread.
fn workspace() -> tempfile::TempDir {
    let temp = tempfile::tempdir_in(test_temp::root()).unwrap();
    let root = temp.path();
    let write = |path: &str, text: &str| {
        let path = root.join(path);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, text).unwrap();
    };
    write("MODULE.bazel", "module(name = 'm')\n");
    write("BUILD", "");
    write(
        "rules.bzl",
        "def _impl(ctx):\n    out = ctx.actions.declare_file(ctx.label.name + '.txt')\n    x = 0\n    for i in range(2000):\n        x += i\n    ctx.actions.run_shell(outputs = [out], inputs = depset(transitive = [d[DefaultInfo].files for d in ctx.attr.deps]), command = 'echo %d > %s' % (x, out.path))\n    return [DefaultInfo(files = depset([out]))]\nchain = rule(implementation = _impl, attrs = {'deps': attr.label_list()})\n",
    );
    for package in 0..40 {
        let mut build = "load('//:rules.bzl', 'chain')\n".to_owned();
        for leaf in 0..10 {
            build.push_str(&format!("chain(name = 'l{leaf}')\n"));
        }
        let leaves: Vec<String> = (0..10).map(|leaf| format!("':l{leaf}'")).collect();
        build.push_str(&format!(
            "chain(name = 't', deps = [{}])\ngenrule(name = 'g', srcs = [':t'], outs = ['g.out'], cmd = 'cat $(locations :t) > $@')\n",
            leaves.join(", ")
        ));
        write(&format!("p{package}/BUILD"), &build);
    }
    for directory in ["home", "tmp"] {
        fs::create_dir(root.join(directory)).unwrap();
    }
    temp
}

/// `build --nobuild //...` with the hook at `site` (`None`: off), in
/// `mode`: its exit code and stderr, within [`BOUND`] (else the test
/// fails, the command killed).
fn build(root: &Path, mode: &str, site: Option<&str>) -> (Option<i32>, String) {
    let mut command = Command::new(env!("CARGO_BIN_EXE_bazel-rs"));
    command
        .current_dir(root)
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .env("HOME", root.join("home"))
        .env("TMPDIR", root.join("tmp"))
        .env("BAZEL_RS_CACHE_GC", "off")
        .env("BAZEL_RS_SNAPSHOT", "off")
        .args([
            "--ignore_all_rc_files",
            "--cache-dir=cache",
            "build",
            "--experimental-builtin-modules=false",
            "--lockfile_mode=off",
            "--memory-limit=2048",
            "--nobuild",
            "--loading_phase_threads=8",
            "//...",
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped());
    match mode {
        "walk" => {
            command.env("BAZEL_RS_ANALYSIS", "walk");
        }
        "eval" => {
            command.env("BAZEL_RS_ANALYSIS", "eval");
        }
        "frozen" => {
            command
                .env("BAZEL_RS_ANALYSIS", "eval")
                .env("BAZEL_RS_EVAL_FROZEN_PREPARES", "all");
        }
        other => panic!("unknown mode {other}"),
    }
    if let Some(site) = site {
        command.env("BAZEL_RS_DEBUG_PANIC_AT", site);
    }
    let mut child = command.spawn().unwrap();
    // Read stderr beside the wait: a full pipe must not stall the child.
    let mut stderr = child.stderr.take().unwrap();
    let reader = std::thread::spawn(move || {
        let mut text = String::new();
        let _ = stderr.read_to_string(&mut text);
        text
    });
    let started = Instant::now();
    let status = loop {
        if let Some(status) = child.try_wait().unwrap() {
            break status;
        }
        if started.elapsed() > BOUND {
            let _ = child.kill();
            let _ = child.wait();
            let text = reader.join().unwrap();
            panic!("{mode} {site:?}: still running after {BOUND:?} (a hang)\n{text}");
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    (status.code(), reader.join().unwrap())
}

/// The hook at `site` (`<name>:<pass>`) ends the command with exit code
/// 37 and the panic's message; without it the same build succeeds.
fn crashes(mode: &str, site: &str) {
    let temp = workspace();
    let (code, stderr) = build(temp.path(), mode, None);
    assert_eq!(
        code,
        Some(0),
        "{mode}: the build without the hook\n{stderr}"
    );
    let _ = fs::remove_dir_all(temp.path().join("cache"));
    let (code, stderr) = build(temp.path(), mode, Some(site));
    let name = site.split(':').next().unwrap();
    assert_eq!(code, Some(37), "{mode} {site}\n{stderr}");
    assert!(
        stderr.contains("FATAL: bazel-rs crashed due to an internal error"),
        "{mode} {site}\n{stderr}"
    );
    assert!(
        stderr.contains(&format!("BAZEL_RS_DEBUG_PANIC_AT: {name}")),
        "{mode} {site}\n{stderr}"
    );
}

macro_rules! sites {
    ($($test:ident: $mode:literal, $site:literal;)*) => {
        $(
            #[test]
            fn $test() {
                crashes($mode, $site);
            }
        )*
    };
}

sites! {
    walk_coordinator: "walk", "coordinator:20";
    walk_analysis_worker: "walk", "analysis-worker:3";
    walk_lowering: "walk", "lowering:3";
    walk_drop: "walk", "drop:1";
    walk_loading_worker: "walk", "loading-worker:5";
    eval_coordinator: "eval", "coordinator:20";
    eval_analysis_worker: "eval", "analysis-worker:3";
    eval_lowering: "eval", "lowering:3";
    eval_drop: "eval", "drop:1";
    eval_loading_worker: "eval", "loading-worker:5";
    eval_evaluator: "eval", "evaluator:5";
    frozen_coordinator: "frozen", "coordinator:20";
    frozen_analysis_worker: "frozen", "analysis-worker:3";
    frozen_lowering: "frozen", "lowering:3";
    frozen_drop: "frozen", "drop:1";
    frozen_loading_worker: "frozen", "loading-worker:5";
    frozen_evaluator: "frozen", "evaluator:5";
    frozen_buffer_classify: "frozen", "t3-classify:7";
    frozen_buffer_publish: "frozen", "t3-publish:7";
}
