//! W6 phase-1 prefetch (docs/PARALLEL_ANALYSIS.md "Phase-1 prefetch"):
//! background workers load the registered toolchain packages and the
//! packages the analyzed rules name while the coordinator analyzes; every
//! unit commits at the sequential demand point. These tests pin the
//! determinism: byte-identical stdout/stderr/exit across
//! `--loading_phase_threads` and under worker jitter, `.bzl` and BUILD
//! prints exactly once in sequential order, the first analysis error in
//! sequential order, speculative loads (a select() branch not taken, the
//! implementation of a toolchain resolution never selects) that fail
//! without surfacing, the sentinel retry path (a registry module root
//! only the coordinator materializes), a top-level aspect's `.bzl` a
//! worker evaluated first (one print, one char model; review F1), and the
//! recorded-input keys of a module extension's reads, which name only the
//! roots the sequential engine has obtained (review F2 and its follow-ups).

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
    assert_identical_across_threads_after(ws, args, || {})
}

/// The thread counts and worker jitter compared with the threads-1 run.
const THREADED: [(usize, u64); 5] = [(2, 0), (8, 0), (32, 0), (8, 40), (32, 25)];

/// `build --loading_phase_threads=<threads> <args>`, with worker jitter.
fn build_at(ws: &Workspace, threads: usize, jitter: u64, args: &[&str]) -> Output {
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
}

/// [`assert_identical_across_threads`], calling `before` ahead of every
/// run.
fn assert_identical_across_threads_after(
    ws: &Workspace,
    args: &[&str],
    before: impl Fn(),
) -> Output {
    let run = |threads: usize, jitter: u64| {
        before();
        build_at(ws, threads, jitter, args)
    };
    let reference = run(1, 0);
    for (threads, jitter) in THREADED {
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

/// The `--aspects` workspace of W6 review F1: `//app:app` names
/// `//tools:unused` only on a select() branch not taken (the prefetch
/// follows every branch, the build never commits `//tools` for it), many
/// targets keep the coordinator analyzing while a worker loads `//tools`,
/// and the top-level aspect is defined in that package's `lint.bzl` — a
/// module with a top-level print and a non-ASCII string its aspect writes
/// into an action output. With `dep`, the aspect's implicit `_dep` names
/// `//tools:unused`, so the build commits `//tools` after the aspect load.
fn aspect_workspace(dep: bool) -> Workspace {
    let ws = Workspace::new("module(name = 'main')\n");
    ws.file(
        "app/rules.bzl",
        "def _impl(ctx):\n    return [DefaultInfo()]\nr = rule(implementation = _impl, attrs = {'srcs': attr.label_list(allow_files = True)})\n",
    );
    ws.file(
        "app/BUILD.bazel",
        "load(':rules.bzl', 'r')\nconfig_setting(name = 'never', values = {'cpu': 'nonexistent'})\nr(name = 'app', srcs = select({':never': ['//tools:unused'], '//conditions:default': []}))\n",
    );
    let many: String = (0..1500).map(|i| format!("r(name = 'f{i}')\n")).collect();
    ws.file(
        "many/BUILD.bazel",
        &format!("load('//app:rules.bzl', 'r')\n{many}"),
    );
    ws.file(
        "tools/BUILD.bazel",
        "load(':lint.bzl', 'lint_macro')\nlint_macro(name = 'unused')\n",
    );
    let attrs = if dep {
        ", attrs = {'_dep': attr.label(default = '//tools:unused')}"
    } else {
        ""
    };
    ws.file(
        "tools/lint.bzl",
        &format!(
            r#"NAME = "é"
print("lint.bzl loaded, len(NAME) = %d" % len(NAME))

def _impl(target, ctx):
    if ctx.label.name != "app":
        return []
    out = ctx.actions.declare_file(ctx.label.name + ".lint.txt")
    ctx.actions.write(out, NAME + "\n")
    return [OutputGroupInfo(lint = depset([out]))]

lint = aspect(implementation = _impl{attrs})

def lint_macro(name):
    native.filegroup(name = name, visibility = ["//visibility:public"])
"#
        ),
    );
    ws
}

/// The genuine Bazel 9.3.0 capture of [`aspect_workspace`]
/// (tests/fixtures/phase1_prefetch/aspect_reference_receipt.json).
fn aspect_receipt() -> serde_json::Value {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/phase1_prefetch/aspect_reference_receipt.json");
    serde_json::from_slice(&fs::read(path).unwrap()).unwrap()
}

/// W6 review F1 (reproduced on d707764): a prefetch worker loading
/// `//tools` for the select() branch not taken evaluated `lint.bzl` first,
/// and the coordinator's `--aspects` load then hit the shared `.bzl` cache
/// without a print sink: the module's print was lost (printed only if the
/// build committed `//tools` later), and the aspect's strings depended on
/// who evaluated the module first — the definition loader's Latin-1 char
/// model (`len(NAME) = 2`, the action output `c3 83 c2 a9`) at threads 1,
/// the BUILD loader's at 8 or 32 (the receipt's `native_before_review_f1`).
/// Now the aspect load is a BUILD-phase load like every reader of the
/// shared cache: the line prints once, at the aspect load, and the
/// aspect's strings are the analysis model's at every thread count.
/// Genuine Bazel 9.3.0 prints the line once too — `DEBUG: <WS>/tools/
/// lint.bzl:2:6: lint.bzl loaded, len(NAME) = 2`: its strings are the
/// Latin-1 chars of the UTF-8 bytes, so `len` counts bytes, the analysis
/// model's documented `len` divergence (docs/STRING_METHODS.md) — and the
/// aspect's action writes the same bytes, `c3 a9 0a`.
#[test]
fn aspect_bzl_prints_once_in_the_build_char_model_whoever_loads_it_first() {
    use sha2::Digest;
    let receipt = aspect_receipt();
    for dep in [false, true] {
        let ws = aspect_workspace(dep);
        let variant = format!("dep={dep}");
        // The receipt was captured on exactly this fixture.
        let files = receipt["fixture_files_sha256"][&variant]
            .as_object()
            .unwrap();
        assert_eq!(files.len(), 6);
        for (name, sha256) in files {
            let bytes = fs::read(ws.0.join(name)).unwrap();
            assert_eq!(
                format!("{:x}", sha2::Sha256::digest(&bytes)),
                sha256.as_str().unwrap(),
                "{variant}: {name}"
            );
        }
        let genuine = &receipt["genuine"][&variant]["build"];
        let args = [
            "--aspects=//tools:lint.bzl%lint",
            "--output_groups=lint",
            "//app:app",
            "//many:all",
        ];
        let warm = ws.run_env(
            &[],
            &[
                "build",
                "--loading_phase_threads=1",
                args[0],
                args[1],
                args[2],
                args[3],
            ],
        );
        assert!(warm.status.success(), "{variant}: {}", stderr(&warm));
        let reference = assert_identical_across_threads(&ws, &args);
        assert!(
            reference.status.success(),
            "{variant}: {}",
            stderr(&reference)
        );
        let text = stderr(&reference);
        // Printed once, like genuine's one DEBUG line (the analysis model's
        // `len` divergence aside).
        let genuine_lines = genuine["lint_lines"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|line| line.as_str().unwrap().contains("lint.bzl loaded"))
            .count();
        assert_eq!(genuine_lines, 1);
        assert_eq!(
            text.matches("lint.bzl loaded").count(),
            genuine_lines,
            "{variant}: {text}"
        );
        assert!(
            text.contains("lint.bzl loaded, len(NAME) = 1\n"),
            "{variant}: {text}"
        );
        // The aspect's action output: genuine's bytes.
        let stdout = String::from_utf8_lossy(&reference.stdout).into_owned();
        let (_, path) = stdout
            .lines()
            .find_map(|line| line.split_once('\t'))
            .unwrap_or_else(|| panic!("{variant}: {stdout}"));
        let written: Vec<String> = fs::read(path)
            .unwrap()
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect();
        assert_eq!(
            written.join(" "),
            genuine["output_bytes"].as_str().unwrap(),
            "{variant}: {path}"
        );
    }
}

/// The recorded-input workspace of W6 review F2: `@early` and `@data` are
/// two repositories of one `use_repo_rule` group; the module extension
/// that generates `@from_ext` reads the host path `$DATA_PATH` (a file of
/// `@data`'s store tree) without demanding `@data`. `//app:app` names
/// `@data` and `@from_ext` — `@data` first with `data_first` — after
/// many targets that keep the coordinator analyzing while a worker probes
/// the warm `@data` (its group is prepared by the `@early` pattern).
fn recorded_input_workspace(data_first: bool) -> Workspace {
    let ws = Workspace::new(
        "module(name = 'main')\ndata_repo = use_repo_rule('//:data.bzl', 'data_repo')\ndata_repo(name = 'early')\ndata_repo(name = 'data')\next = use_extension('//:ext.bzl', 'ext')\nuse_repo(ext, 'from_ext')\n",
    );
    ws.file("BUILD.bazel", "");
    ws.file(
        "data.bzl",
        "def _data_repo(ctx):\n    ctx.file('BUILD.bazel', 'filegroup(name = \"files\", srcs = [\"file.txt\"], visibility = [\"//visibility:public\"])\\n')\n    ctx.file('file.txt', 'data\\n')\n\ndata_repo = repository_rule(_data_repo)\n",
    );
    ws.file(
        "ext.bzl",
        "def _from_ext(ctx):\n    ctx.file('BUILD.bazel', 'filegroup(name = \"files\", visibility = [\"//visibility:public\"])\\n')\n\nfrom_ext = repository_rule(_from_ext)\n\ndef _ext(module_ctx):\n    path = module_ctx.getenv('DATA_PATH')\n    if path:\n        module_ctx.read(path)\n    from_ext(name = 'from_ext')\n\next = module_extension(_ext)\n",
    );
    ws.file(
        "app/rules.bzl",
        "def _impl(ctx):\n    return [DefaultInfo()]\nr = rule(implementation = _impl, attrs = {'deps': attr.label_list(allow_files = True)})\n",
    );
    let many: String = (0..1500)
        .map(|i| format!("r(name = 'f{i}', visibility = ['//visibility:public'])\n"))
        .collect();
    ws.file(
        "many/BUILD.bazel",
        &format!("load('//app:rules.bzl', 'r')\n{many}"),
    );
    let mut deps: Vec<String> = (0..1500).map(|i| format!("'//many:f{i}'")).collect();
    deps.push("'@from_ext//:files'".to_owned());
    if data_first {
        deps.insert(0, "'@data//:files'".to_owned());
    } else {
        deps.push("'@data//:files'".to_owned());
    }
    ws.file(
        "app/BUILD.bazel",
        &format!(
            "load(':rules.bzl', 'r')\nr(name = 'app', deps = [{}])\n",
            deps.join(", ")
        ),
    );
    ws
}

/// W6 review F2: a recorded input's key named a repository whose root a
/// phase-1 prefetch worker's warm-reuse probe had published. The module
/// extension reads a file of `@data`'s store tree; the sequential engine
/// has not demanded `@data` yet when the extension runs (`@from_ext`
/// comes first), so the path is in no known repository and update mode
/// cannot write the extension's lockfile entry. At threads 8 and 32 a
/// worker had usually probed `@data` by then: the key became
/// `@@+data_repo+data//file.txt` and the build succeeded. A root now
/// counts for keys only once the coordinator demanded it or committed a
/// unit that did: the sequential outcome at every thread count — the
/// failure, and with `@data` demanded first the `@@+data_repo+data//` key
/// (the worker's unit committed before the extension runs).
#[test]
fn recorded_input_keys_name_only_roots_the_coordinator_has() {
    for data_first in [false, true] {
        let ws = recorded_input_workspace(data_first);
        // Publish @early and @data (warm for the workers' probes).
        let publish = ws.run_env(
            &[],
            &[
                "build",
                "--loading_phase_threads=1",
                "@early//:files",
                "@data//:files",
            ],
        );
        assert!(publish.status.success(), "{}", stderr(&publish));
        let stdout = String::from_utf8_lossy(&publish.stdout).into_owned();
        let data = stdout
            .lines()
            .find_map(|line| line.strip_prefix("@@+data_repo+data//:files\t"))
            .unwrap_or_else(|| panic!("{stdout}"));
        let data = fs::canonicalize(data).unwrap();
        assert_eq!(fs::read_to_string(&data).unwrap(), "data\n");
        let repo_env = format!("--repo_env=DATA_PATH={}", data.display());
        let args = [
            repo_env.as_str(),
            "--nobuild",
            "@early//:files",
            "//app:app",
        ];
        let (reference, lock) = assert_identical_lockfiles_across_threads(&ws, &args);
        let text = stderr(&reference);
        if data_first {
            assert!(reference.status.success(), "{text}");
            assert!(
                lock.contains("\"FILE:@@+data_repo+data//file.txt "),
                "{lock}"
            );
        } else {
            assert_eq!(reference.status.code(), Some(1), "{text}");
            assert!(
                text.contains(&format!(
                    "module extension @@//:ext.bzl%ext: cannot write its MODULE.bazel.lock entry: the module extension watched {}, which is in no known repository (use --lockfile_mode=off)",
                    data.display()
                )),
                "{text}"
            );
        }
    }
}

/// [`assert_identical_across_threads`] with a fresh `MODULE.bazel.lock`
/// for every run: the lockfile each run writes is byte-identical to the
/// threads-1 run's too (none: empty). Returns the reference run and its
/// lockfile.
fn assert_identical_lockfiles_across_threads(ws: &Workspace, args: &[&str]) -> (Output, String) {
    let lockfile = ws.0.join("MODULE.bazel.lock");
    let run = |threads: usize, jitter: u64| {
        let _ = fs::remove_file(&lockfile);
        let output = build_at(ws, threads, jitter, args);
        (output, fs::read_to_string(&lockfile).unwrap_or_default())
    };
    let (reference, reference_lock) = run(1, 0);
    for (threads, jitter) in THREADED {
        let (other, lock) = run(threads, jitter);
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
        assert_eq!(
            lock, reference_lock,
            "threads={threads} jitter={jitter}: MODULE.bazel.lock"
        );
    }
    (reference, reference_lock)
}

/// How the sequential engine obtains, before the module extension runs,
/// the repository whose store file the extension reads, in the
/// workspaces of the W6 review F2 follow-up (the reviewer's f2a, f2b,
/// f2c). None of them is a demand on the coordinator or in a committed
/// unit, the only obtains d3f2582 counted.
#[derive(Clone, Copy, Debug)]
enum ObtainedBy {
    /// `@a` read `@b//:file.txt`, so `@a`'s warm-reuse check demands
    /// `@b`. A worker probes `@a` first, for `//spec` (a select() branch
    /// not taken, never committed); `//mid:m` demands the filled `@a`
    /// later. (f2a)
    AnotherRootsCheck,
    /// `//p1` (through `//mid:m`) loads `@data//:defs.bzl`, which a worker
    /// evaluated first for `//spec` (a select() branch not taken): `//p1`'s
    /// load is a cache hit that demands no root. (f2b)
    CachedBzl,
    /// `//p1` loads `@data//:defs.bzl`, then `@from_ext//:defs.bzl`: its
    /// worker unit aborts at the extension, and the coordinator's inline
    /// re-run reads `@data//:defs.bzl` from the cache right before it runs
    /// the extension. (f2c)
    InlineRerunCachedBzl,
    /// `register_toolchains('@data//ign/...', '@from_ext//:all')` with
    /// `ign` ignored by `@data`'s REPO.bazel: `//tc:t`'s toolchain
    /// resolution expands the patterns, and the `/...` expansion asks for
    /// `@data`'s ignored subdirectories, warns and skips the pattern
    /// without demanding `@data`'s root itself; only their miss demands
    /// it. A worker computed them first, loading `@data//x` (`//app:app`
    /// names it after `//tc:t`): the expansion's lookup is a hit. (Review
    /// FU1 of 6474766.)
    IgnoredMemo,
}

/// The [`ObtainedBy`] workspace: `@early` and the repositories of the
/// extension's file are one `use_repo_rule` group (the `@early` pattern
/// prepares it, so the workers probe their warm trees); the module
/// extension that generates `@from_ext` reads the host path `$DATA_PATH`
/// without demanding its repository; `//app:app` analyzes 1,500 targets
/// of `//many` before anything else, while the workers run ahead. Returns
/// the workspace, the repository holding the file and the targets that
/// publish the trees.
fn obtained_root_workspace(obtained: ObtainedBy) -> (Workspace, &'static str, Vec<&'static str>) {
    let (declarations, repository, publish) = match obtained {
        ObtainedBy::AnotherRootsCheck => (
            "data_repo(name = 'early')\ndata_repo(name = 'b')\ndata_repo(name = 'a', read_from = '@b//:file.txt')\n",
            "b",
            vec!["@early//:files", "@a//:files", "@b//:files"],
        ),
        ObtainedBy::CachedBzl | ObtainedBy::InlineRerunCachedBzl | ObtainedBy::IgnoredMemo => (
            "data_repo(name = 'early')\ndata_repo(name = 'data')\n",
            "data",
            vec!["@early//:files", "@data//:files"],
        ),
    };
    let registrations = match obtained {
        ObtainedBy::IgnoredMemo => {
            "register_toolchains('@data//ign/...')\nregister_toolchains('@from_ext//:all')\n"
        }
        _ => "",
    };
    let ws = Workspace::new(&format!(
        "module(name = 'main')\ndata_repo = use_repo_rule('//:data.bzl', 'data_repo')\n{declarations}ext = use_extension('//:ext.bzl', 'ext')\nuse_repo(ext, 'from_ext')\n{registrations}"
    ));
    ws.file("BUILD.bazel", "");
    ws.file(
        "data.bzl",
        "def _data_repo(ctx):\n    if ctx.attr.read_from:\n        ctx.read(ctx.attr.read_from)\n    ctx.file('BUILD.bazel', 'filegroup(name = \"files\", srcs = [\"file.txt\"], visibility = [\"//visibility:public\"])\\n')\n    ctx.file('pkg/BUILD.bazel', 'filegroup(name = \"files\", visibility = [\"//visibility:public\"])\\n')\n    ctx.file('file.txt', 'data\\n')\n    ctx.file('defs.bzl', 'X = 1\\n')\n\ndata_repo = repository_rule(_data_repo, attrs = {'read_from': attr.label()})\n",
    );
    ws.file(
        "ext.bzl",
        "def _from_ext(ctx):\n    ctx.file('BUILD.bazel', 'filegroup(name = \"files\", visibility = [\"//visibility:public\"])\\n')\n    ctx.file('defs.bzl', 'Y = 2\\n')\n\nfrom_ext = repository_rule(_from_ext)\n\ndef _ext(module_ctx):\n    path = module_ctx.getenv('DATA_PATH')\n    if path:\n        module_ctx.read(path)\n    from_ext(name = 'from_ext')\n\next = module_extension(_ext)\n",
    );
    ws.file(
        "app/rules.bzl",
        "def _impl(ctx):\n    return [DefaultInfo()]\nr = rule(implementation = _impl, attrs = {'deps': attr.label_list(allow_files = True)})\n",
    );
    let rules = |prefix: &str| -> String {
        (0..1500)
            .map(|i| format!("r(name = '{prefix}{i}', visibility = ['//visibility:public'])\n"))
            .collect()
    };
    ws.file(
        "many/BUILD.bazel",
        &format!("load('//app:rules.bzl', 'r')\n{}", rules("f")),
    );
    let many: Vec<String> = (0..1500).map(|i| format!("'//many:f{i}'")).collect();
    let many = many.join(", ");
    if let ObtainedBy::IgnoredMemo = obtained {
        // The trees ignore `ign`; `@data//x` is the package a worker loads.
        ws.file(
            "data.bzl",
            "def _data_repo(ctx):\n    ctx.file('BUILD.bazel', 'filegroup(name = \"files\", srcs = [\"file.txt\"], visibility = [\"//visibility:public\"])\\n')\n    ctx.file('REPO.bazel', 'ignore_directories([\"ign\"])\\n')\n    ctx.file('x/BUILD.bazel', 'filegroup(name = \"files\", visibility = [\"//visibility:public\"])\\n')\n    ctx.file('ign/BUILD.bazel', 'filegroup(name = \"files\", visibility = [\"//visibility:public\"])\\n')\n    ctx.file('file.txt', 'data\\n')\n\ndata_repo = repository_rule(_data_repo)\n",
        );
        ws.file(
            "os/BUILD.bazel",
            "package(default_visibility = ['//visibility:public'])\nconstraint_setting(name = 'os')\nconstraint_value(name = 'linux', constraint_setting = ':os')\n",
        );
        ws.file(
            "p/BUILD.bazel",
            "package(default_visibility = ['//visibility:public'])\nplatform(name = 'linux', constraint_values = ['//os:linux'])\n",
        );
        ws.file(
            "tc/defs.bzl",
            "def _impl(ctx):\n    return [DefaultInfo()]\nneeds = rule(implementation = _impl, toolchains = [config_common.toolchain_type('//tc:tt', mandatory = False)])\n",
        );
        ws.file(
            "tc/BUILD.bazel",
            "load(':defs.bzl', 'needs')\ntoolchain_type(name = 'tt', visibility = ['//visibility:public'])\nneeds(name = 't', visibility = ['//visibility:public'])\n",
        );
        ws.file(
            "app/BUILD.bazel",
            &format!(
                "load(':rules.bzl', 'r')\nr(name = 'app', deps = [{many}, '//tc:t', '@data//x:files'])\n"
            ),
        );
        return (ws, repository, publish);
    }
    if let ObtainedBy::InlineRerunCachedBzl = obtained {
        ws.file(
            "app/BUILD.bazel",
            &format!("load(':rules.bzl', 'r')\nr(name = 'app', deps = [{many}, '//p1:p1'])\n"),
        );
        ws.file(
            "p1/BUILD.bazel",
            "load('@data//:defs.bzl', 'X')\nload('@from_ext//:defs.bzl', 'Y')\nload('//app:rules.bzl', 'r')\nr(name = 'p1', visibility = ['//visibility:public'])\n",
        );
        return (ws, repository, publish);
    }
    // `//spec` is reached only through the select() branch not taken: a
    // worker loads it, the build never commits it.
    ws.file(
        "app/BUILD.bazel",
        &format!(
            "load(':rules.bzl', 'r')\nconfig_setting(name = 'never', values = {{'cpu': 'nonexistent'}})\nr(name = 'app', deps = [{many}, '//mid:m', '@from_ext//:files'] + select({{':never': ['//spec:x'], '//conditions:default': []}}))\n"
        ),
    );
    let (spec_load, m_dep) = match obtained {
        ObtainedBy::AnotherRootsCheck => ("@a//:defs.bzl", "@a//pkg:files"),
        _ => ("@data//:defs.bzl", "//p1:p1"),
    };
    ws.file(
        "spec/BUILD.bazel",
        &format!(
            "load('{spec_load}', 'X')\nfilegroup(name = 'x', visibility = ['//visibility:public'])\n"
        ),
    );
    ws.file(
        "mid/BUILD.bazel",
        &format!(
            "load('//app:rules.bzl', 'r')\n{}r(name = 'm', deps = ['{m_dep}'], visibility = ['//visibility:public'])\n",
            rules("m")
        ),
    );
    ws.file(
        "p1/BUILD.bazel",
        "load('@data//:defs.bzl', 'X')\nload('//app:rules.bzl', 'r')\nr(name = 'p1', visibility = ['//visibility:public'])\n",
    );
    (ws, repository, publish)
}

/// W6 review F2 follow-ups: the sequential engine obtains a root at more
/// points than its demands on the coordinator and in committed units —
/// through another root's warm-reuse check, through a `.bzl` module the
/// shared cache serves (also in an aborted unit's inline re-run, just
/// before the module extension runs), and through ignored subdirectories
/// a worker computed first (a filtered-out toolchain pattern asks for
/// them without demanding the root). d3f2582 counted only the demands,
/// 6474766 not the ignored-subdirectories hit, so in each workspace
/// ([`ObtainedBy`]) the extension's read of the warm store file was in
/// no known repository at threads 2/8/32 (update mode failed) where
/// threads 1 records `@@+data_repo+NAME//file.txt`. Now every thread
/// count records the threads-1 key: byte-identical exit, stdout, stderr
/// and lockfile, a fresh lockfile per run.
#[test]
fn recorded_input_keys_count_every_root_the_sequential_engine_obtains() {
    for obtained in [
        ObtainedBy::AnotherRootsCheck,
        ObtainedBy::CachedBzl,
        ObtainedBy::InlineRerunCachedBzl,
        ObtainedBy::IgnoredMemo,
    ] {
        let (ws, repository, publish) = obtained_root_workspace(obtained);
        // Publish the warm trees the workers probe.
        let mut command = vec!["build", "--loading_phase_threads=1"];
        command.extend(&publish);
        let warm = ws.run_env(&[], &command);
        assert!(warm.status.success(), "{obtained:?}: {}", stderr(&warm));
        let stdout = String::from_utf8_lossy(&warm.stdout).into_owned();
        let prefix = format!("@@+data_repo+{repository}//:files\t");
        let file = stdout
            .lines()
            .find_map(|line| line.strip_prefix(prefix.as_str()))
            .unwrap_or_else(|| panic!("{obtained:?}: {stdout}"));
        let file = fs::canonicalize(file).unwrap();
        let repo_env = format!("--repo_env=DATA_PATH={}", file.display());
        let mut args = vec![repo_env.as_str(), "@early//:files", "//app:app"];
        if let ObtainedBy::IgnoredMemo = obtained {
            // The workspace's own platform for the toolchain resolution.
            args.insert(1, "--host_platform=//p:linux");
        }
        let (reference, lock) = assert_identical_lockfiles_across_threads(&ws, &args);
        assert!(
            reference.status.success(),
            "{obtained:?}: {}",
            stderr(&reference)
        );
        assert!(
            lock.contains(&format!("\"FILE:@@+data_repo+{repository}//file.txt ")),
            "{obtained:?}: {lock}"
        );
        if let ObtainedBy::IgnoredMemo = obtained {
            assert!(
                stderr(&reference).contains(
                    "WARNING: Pattern '@data//ign/...' was filtered out by ignored directory 'ign'"
                ),
                "{}",
                stderr(&reference)
            );
        }
    }
}

/// Invariant (b) of the prefetch overlap through the CLI: `//globbed`'s
/// glob and `//loaded`'s `.bzl` load follow symlinks into a
/// local_path_override module's root. A worker never decides such a link
/// (its unit aborts as order-dependent, src/prefetch.rs
/// `worker_links_into_another_live_root_abort_as_order_dependent`); the
/// coordinator re-runs the unit at its demand point, so the build is
/// byte-identical at every thread count.
#[cfg(unix)]
#[test]
fn links_into_another_live_root_are_the_coordinators_decisions() {
    // The module lives outside the workspace: a link into it leaves the
    // main repository's tree.
    let lib_ws = Workspace::new("module(name = 'lib')\n");
    lib_ws.file("BUILD.bazel", "");
    lib_ws.file("data/x.txt", "x\n");
    lib_ws.file("defs.bzl", "print('lib defs')\nvalue = 'from_lib'\n");
    let lib = lib_ws.0.clone();
    let ws = Workspace::new(&format!(
        "module(name = 'main')\nbazel_dep(name = 'lib', version = '1')\nlocal_path_override(module_name = 'lib', path = {})\n",
        json_string(&lib)
    ));
    ws.file(
        "globbed/BUILD.bazel",
        "filegroup(name = 'g', srcs = glob(['link/**']), visibility = ['//visibility:public'])\n",
    );
    std::os::unix::fs::symlink(lib.join("data"), ws.0.join("globbed/link")).unwrap();
    ws.file(
        "loaded/BUILD.bazel",
        "load(':linked.bzl', 'value')\nfilegroup(name = value, visibility = ['//visibility:public'])\n",
    );
    std::os::unix::fs::symlink(lib.join("defs.bzl"), ws.0.join("loaded/linked.bzl")).unwrap();
    ws.file(
        "app/rules.bzl",
        "def _impl(ctx):\n    return [DefaultInfo(files = depset(ctx.files.deps))]\nr = rule(implementation = _impl, attrs = {'deps': attr.label_list(allow_files = True)})\n",
    );
    let many: String = (0..1000)
        .map(|i| format!("r(name = 'f{i}', visibility = ['//visibility:public'])\n"))
        .collect();
    ws.file(
        "many/BUILD.bazel",
        &format!("load('//app:rules.bzl', 'r')\n{many}"),
    );
    let mut deps: Vec<String> = (0..1000).map(|i| format!("'//many:f{i}'")).collect();
    deps.push("'//globbed:g'".to_owned());
    deps.push("'//loaded:from_lib'".to_owned());
    ws.file(
        "app/BUILD.bazel",
        &format!(
            "load(':rules.bzl', 'r')\nr(name = 'app', deps = [{}])\n",
            deps.join(", ")
        ),
    );
    let warm = ws.run_env(&[], &["build", "--loading_phase_threads=1", "//app:app"]);
    assert!(warm.status.success(), "{}", stderr(&warm));
    let reference = assert_identical_across_threads(&ws, &["//app:app"]);
    assert!(reference.status.success(), "{}", stderr(&reference));
    let text = stderr(&reference);
    assert_eq!(text.matches("lib defs").count(), 1, "{text}");
    // The glob's file, read through the link.
    let stdout = String::from_utf8_lossy(&reference.stdout).into_owned();
    assert_eq!(
        stdout,
        format!("//app:app\t{}\n", lib.join("data/x.txt").display())
    );
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
