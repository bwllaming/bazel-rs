//! W6 phase-1 prefetch (docs/PARALLEL_ANALYSIS.md "Phase-1 prefetch"):
//! background workers load the registered toolchain packages and the
//! packages the analyzed rules name while the coordinator analyzes; every
//! unit commits at the sequential demand point. These tests pin the
//! determinism: byte-identical stdout/stderr/exit across
//! `--loading_phase_threads` and under worker jitter, `.bzl` and BUILD
//! prints exactly once in sequential order, the first analysis error in
//! sequential order, speculative loads (a select() branch not taken, the
//! implementation of a toolchain resolution never selects) that fail
//! without surfacing, and the sentinel retry path (a registry module root
//! only the coordinator materializes).

#![allow(clippy::disallowed_methods)] // test harness: starts processes directly (clippy.toml)
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::atomic::{AtomicU64, Ordering};

static NEXT: AtomicU64 = AtomicU64::new(0);

struct Workspace(PathBuf);

impl Workspace {
    fn new(module: &str) -> Self {
        let path = std::env::temp_dir().join(format!(
            "bazel-rs-phase1-prefetch-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&path).unwrap();
        let ws = Self(path);
        ws.file("MODULE.bazel", module);
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
            .arg("--cache-dir=cache")
            .args(args)
            .env_remove("BAZEL_RS_TIMING")
            .env_remove("BAZEL_RS_DUMP_ACTIONS")
            .env_remove("BAZEL_RS_LOADING_JITTER")
            .current_dir(&self.0);
        for (key, value) in env {
            command.env(key, value);
        }
        command.output().unwrap()
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

/// `args` at threads 1 (the reference), 2, 8, 32 and 8/32 with worker
/// jitter: exit code, stdout and stderr byte-identical. Returns the
/// reference run.
fn assert_identical_across_threads(ws: &Workspace, args: &[&str]) -> Output {
    let run = |threads: usize, jitter: u64| {
        let jitter_text = jitter.to_string();
        let env: Vec<(&str, &str)> = if jitter > 0 {
            vec![("BAZEL_RS_LOADING_JITTER", jitter_text.as_str())]
        } else {
            Vec::new()
        };
        let mut command = vec![
            "build".to_owned(),
            format!("--loading_phase_threads={threads}"),
        ];
        command.extend(args.iter().map(|arg| (*arg).to_owned()));
        let command: Vec<&str> = command.iter().map(String::as_str).collect();
        ws.run_env(&env, &command)
    };
    let reference = run(1, 0);
    for (threads, jitter) in [(2usize, 0u64), (8, 0), (32, 0), (8, 40), (32, 25)] {
        let other = run(threads, jitter);
        assert_eq!(
            other.status.code(),
            reference.status.code(),
            "threads={threads} jitter={jitter}: exit\n{}",
            stderr(&other)
        );
        assert_eq!(
            other.stdout, reference.stdout,
            "threads={threads} jitter={jitter}: stdout"
        );
        assert_eq!(
            stderr(&other),
            stderr(&reference),
            "threads={threads} jitter={jitter}: stderr"
        );
    }
    reference
}

/// Constraints, platforms, a printing rule library, three registered
/// toolchain packages (one `:all`, one explicit label, one whose mac
/// toolchain names an implementation package that fails to load and is
/// never selected), and a target whose select() names a broken package
/// on the branch not taken.
fn toolchain_workspace() -> Workspace {
    let ws = Workspace::new(
        "module(name = 'main')\nregister_toolchains('//tc1:all')\nregister_toolchains('//tc2:linux')\nregister_toolchains('//tc3:all')\n",
    );
    ws.file(
        "os/BUILD.bazel",
        "load('//r:print.bzl', 'P')\npackage(default_visibility = ['//visibility:public'])\nconstraint_setting(name = 'os')\nconstraint_value(name = 'linux', constraint_setting = ':os')\nconstraint_value(name = 'osx', constraint_setting = ':os')\n",
    );
    ws.file(
        "p/BUILD.bazel",
        "package(default_visibility = ['//visibility:public'])\nplatform(name = 'linux', constraint_values = ['//os:linux'])\n",
    );
    ws.file("r/BUILD.bazel", "");
    ws.file(
        "r/print.bzl",
        "print('print.bzl top-level')\nP = 1\ndef say(message):\n    print(message)\n",
    );
    ws.file(
        "r/defs.bzl",
        r#"load(":print.bzl", "P")
print("defs.bzl top-level")

def _impl_impl(ctx):
    return [platform_common.ToolchainInfo(name = ctx.attr.tool_name), DefaultInfo()]

impl = rule(implementation = _impl_impl, attrs = {"tool_name": attr.string()})

def _use_impl(ctx):
    out = ctx.actions.declare_file(ctx.label.name + ".txt")
    names = [ctx.toolchains[t].name for t in ["//tc1:type", "//tc2:type"]]
    ctx.actions.run_shell(outputs = [out], command = "echo %s > %s" % (" ".join(names), out.path))
    return [DefaultInfo(files = depset([out]))]

use = rule(
    implementation = _use_impl,
    attrs = {"deps": attr.label_list()},
    toolchains = ["//tc1:type", "//tc2:type"],
)

def _lib_impl(ctx):
    return [DefaultInfo()]

lib = rule(implementation = _lib_impl, attrs = {"deps": attr.label_list()})
"#,
    );
    for (package, os) in [("tc1", "linux"), ("tc2", "linux")] {
        ws.file(
            &format!("{package}/BUILD.bazel"),
            &format!(
                "load('//r:defs.bzl', 'impl')\nload('//r:print.bzl', 'say')\nsay('{package} BUILD')\npackage(default_visibility = ['//visibility:public'])\ntoolchain_type(name = 'type')\nimpl(name = 'impl', tool_name = '{package}-{os}')\ntoolchain(name = '{os}', toolchain_type = ':type', exec_compatible_with = ['//os:{os}'], target_compatible_with = ['//os:{os}'], target_settings = ['//cfg:always'], toolchain = ':impl')\n"
            ),
        );
    }
    ws.file(
        "tc3/BUILD.bazel",
        "load('//r:print.bzl', 'say')\nsay('tc3 BUILD')\npackage(default_visibility = ['//visibility:public'])\ntoolchain(name = 'mac', toolchain_type = '//tc1:type', target_compatible_with = ['//os:osx'], toolchain = '//broken_impl:impl')\n",
    );
    ws.file(
        "broken_impl/BUILD.bazel",
        "fail('broken_impl must never load')\n",
    );
    ws.file(
        "cfg/BUILD.bazel",
        "load('//r:print.bzl', 'P', 'say')\nsay('cfg BUILD')\npackage(default_visibility = ['//visibility:public'])\nconfig_setting(name = 'always', values = {'compilation_mode': 'fastbuild'})\nconfig_setting(name = 'is_linux', constraint_values = ['//os:linux'])\n",
    );
    for (package, deps) in [("d1", "['//d2:x']"), ("d2", "['//d3:x']"), ("d3", "[]")] {
        ws.file(
            &format!("{package}/BUILD.bazel"),
            &format!(
                "load('//r:defs.bzl', 'lib')\nload('//r:print.bzl', 'say')\nsay('{package} BUILD')\nlib(name = 'x', deps = {deps}, visibility = ['//visibility:public'])\n"
            ),
        );
    }
    ws.file(
        "badselect/BUILD.bazel",
        "fail('badselect must never load')\n",
    );
    ws.file(
        "u/BUILD.bazel",
        "load('//r:defs.bzl', 'use')\nuse(name = 'u', deps = ['//d1:x'] + select({'//cfg:is_linux': [], '//conditions:default': ['//badselect:x']}))\n",
    );
    ws
}

/// The toolchain packages, their prerequisites and the dependency chain
/// are prefetched; the build is byte-identical to the sequential one at
/// every thread count, every print appears exactly once, and neither the
/// select() branch not taken nor the unselected toolchain implementation
/// surfaces.
#[test]
fn prefetched_toolchains_and_dependencies_are_byte_identical() {
    let ws = toolchain_workspace();
    let args = ["--host_platform=//p:linux", "//u:u"];
    let warm = ws.run_env(
        &[],
        &["build", "--loading_phase_threads=1", args[0], args[1]],
    );
    assert!(warm.status.success(), "{}", stderr(&warm));
    let reference = assert_identical_across_threads(&ws, &args);
    assert!(reference.status.success(), "{}", stderr(&reference));
    let text = stderr(&reference);
    for line in [
        "print.bzl top-level",
        "defs.bzl top-level",
        "tc1 BUILD",
        "tc2 BUILD",
        "tc3 BUILD",
        "cfg BUILD",
        "d1 BUILD",
        "d2 BUILD",
        "d3 BUILD",
    ] {
        assert_eq!(text.matches(line).count(), 1, "{line}: {text}");
    }
    assert!(!text.contains("badselect"), "{text}");
    assert!(!text.contains("broken_impl"), "{text}");
    let stdout = String::from_utf8_lossy(&reference.stdout).into_owned();
    let (_, path) = stdout.trim().split_once('\t').unwrap();
    assert_eq!(
        fs::read_to_string(path).unwrap().trim(),
        "tc1-linux tc2-linux"
    );
}

/// Two failing packages deep in the dependency chains: the error of the
/// one sequential analysis reaches first wins, with the same bytes, at
/// every thread count — the other (prefetched, maybe finished first) never
/// surfaces.
#[test]
fn first_analysis_error_wins_with_prefetch() {
    let ws = toolchain_workspace();
    ws.file(
        "d3/BUILD.bazel",
        "load('//r:print.bzl', 'say')\nsay('d3 BUILD')\nfail('d3 stops first')\n",
    );
    ws.file(
        "e1/BUILD.bazel",
        "load('//r:defs.bzl', 'lib')\nlib(name = 'x', deps = ['//e2:x'], visibility = ['//visibility:public'])\n",
    );
    ws.file(
        "e2/BUILD.bazel",
        "load('//r:print.bzl', 'say')\nsay('e2 BUILD')\nfail('e2 must not surface')\n",
    );
    ws.file(
        "u/BUILD.bazel",
        "load('//r:defs.bzl', 'use')\nuse(name = 'u', deps = ['//d1:x', '//e1:x'])\n",
    );
    let reference = assert_identical_across_threads(&ws, &["--host_platform=//p:linux", "//u:u"]);
    assert!(!reference.status.success());
    let text = stderr(&reference);
    assert!(text.contains("d3 stops first"), "{text}");
    assert!(!text.contains("e2 must not surface"), "{text}");
    assert!(!text.contains("e2 BUILD"), "{text}");
}

/// A JSON string literal (no serde_json dependency in the test).
fn json_string(value: &Path) -> String {
    format!("\"{}\"", value.display())
}

/// Toolchains registered from a registry module whose root only the
/// coordinator materializes: the workers' units abort with the no-demand
/// sentinel, the coordinator loads the first package inline (materializing
/// the root), the aborted siblings are queued again and committed from
/// the workers. Cold, then warm A/B across thread counts and jitter.
#[test]
fn prefetch_retries_packages_once_the_coordinator_materializes_their_root() {
    let ws = toolchain_workspace();
    ws.file("registry/bazel_registry.json", "{\"mirrors\": []}\n");
    ws.file(
        "registry/modules/ext/1.0/MODULE.bazel",
        "module(name='ext', version='1.0')\n",
    );
    ws.file(
        "registry/modules/ext/1.0/source.json",
        &format!(
            "{{\"type\": \"local_path\", \"path\": {}}}\n",
            json_string(&ws.0.join("ext_src"))
        ),
    );
    ws.file("ext_src/MODULE.bazel", "module(name='ext')\n");
    ws.file("ext_src/BUILD.bazel", "");
    ws.file(
        "ext_src/defs.bzl",
        "print('ext defs top-level')\ndef say(message):\n    print(message)\ndef _impl(ctx):\n    return [platform_common.ToolchainInfo(name = ctx.attr.tool_name), DefaultInfo()]\nimpl = rule(implementation = _impl, attrs = {'tool_name': attr.string()})\n",
    );
    for package in ["tca", "tcb", "tcc"] {
        ws.file(
            &format!("ext_src/{package}/BUILD.bazel"),
            &format!(
                "load('//:defs.bzl', 'impl', 'say')\nsay('ext {package} BUILD')\npackage(default_visibility = ['//visibility:public'])\nimpl(name = 'impl', tool_name = 'ext-{package}')\ntoolchain(name = 'tc', toolchain_type = '@@//tc2:type', target_compatible_with = ['@@//os:osx'], toolchain = ':impl')\n"
            ),
        );
    }
    ws.file(
        "MODULE.bazel",
        "module(name = 'main')\nbazel_dep(name = 'ext', version = '1.0')\nregister_toolchains('//tc1:all')\nregister_toolchains('@ext//tca:all')\nregister_toolchains('@ext//tcb:all')\nregister_toolchains('@ext//tcc:all')\nregister_toolchains('//tc2:linux')\n",
    );
    let registry = format!("--registry=file://{}", ws.0.join("registry").display());
    let args = [
        registry.as_str(),
        "--lockfile_mode=off",
        "--host_platform=//p:linux",
        "//u:u",
    ];
    let cold = ws.run_env(
        &[],
        &[
            "build",
            "--loading_phase_threads=8",
            args[0],
            args[1],
            args[2],
            args[3],
        ],
    );
    assert!(cold.status.success(), "cold: {}", stderr(&cold));
    let reference = assert_identical_across_threads(&ws, &args);
    assert!(reference.status.success(), "{}", stderr(&reference));
    let text = stderr(&reference);
    for line in [
        "ext defs top-level",
        "ext tca BUILD",
        "ext tcb BUILD",
        "ext tcc BUILD",
    ] {
        assert_eq!(text.matches(line).count(), 1, "{line}: {text}");
    }
}
