//! W6R: phase-1 prefetch while the analysis snapshot records
//! (docs/ANALYSIS_SNAPSHOT_RECORDING.md "Recording with phase-1 prefetch").
//! The recording of a run with prefetch workers — what a snapshot saves
//! and a hit validates — must be the one the threads-1 run makes: the same
//! `INPUTS-CANONICAL` digest of the record mode's dump, the same
//! observations, environment digests, repository trees, replay records,
//! pinned tables and taints (but the test hooks'), never Unstable, at
//! `--loading_phase_threads` 2/8/32 and under worker jitter
//! (`BAZEL_RS_LOADING_JITTER` turns the `on` mode off, so these tests
//! compare record-mode dumps). The targeted fixtures each need one of the
//! hooks that carry a worker's observations to the sequential point: with
//! it switched off (`BAZEL_RS_DEBUG_W6R_OFF`) a threaded recording
//! differs. Verify mode is strict across thread counts, and an `on` hit
//! replays no prefetch telemetry.

#![allow(clippy::disallowed_methods)] // test harness: starts processes directly (clippy.toml)
use std::fs;
use std::path::PathBuf;
use std::process::{Command, Output};
use std::sync::atomic::{AtomicU64, Ordering};
include!("support/explicit_modules.rs");
include!("support/console.rs");
include!("support/sealed_trees.rs");

static NEXT: AtomicU64 = AtomicU64::new(0);

struct Workspace {
    root: PathBuf,
    /// Dumps, HOME and TMPDIR live outside the workspace: creating them
    /// must not change what is observed.
    scratch: PathBuf,
}

impl Workspace {
    fn new(module: &str) -> Self {
        let unique = format!(
            "bazel-rs-snapshot-prefetch-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        );
        let root = crate::test_temp::root().join(&unique);
        let scratch = crate::test_temp::root().join(format!("{unique}-scratch"));
        fs::create_dir(&root).unwrap();
        fs::create_dir(&scratch).unwrap();
        let ws = Self { root, scratch };
        ws.file("MODULE.bazel", module);
        ws
    }

    /// The snapshot store: in the workspace's state directory of the
    /// cache (docs/WORKTREES.md).
    fn snapshots(&self) -> PathBuf {
        self.root
            .join("cache/worktrees")
            .join(bazel_rs::worktree::state_name(&self.root))
            .join("snapshots")
    }

    fn file(&self, name: &str, content: &str) {
        let path = self.root.join(name);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, content).unwrap();
    }

    /// One bazel-rs command. The analysis snapshot's mode is always
    /// explicit: `off` unless `env` sets `BAZEL_RS_SNAPSHOT` (record, on,
    /// verify) — warm-ups and reference runs must never save or hit a
    /// snapshot, whatever the default of the binary.
    fn run_env(&self, env: &[(&str, &str)], args: &[&str]) -> Output {
        let home = self.scratch.join("home");
        let tmp = self.scratch.join("tmp");
        fs::create_dir_all(&home).unwrap();
        fs::create_dir_all(&tmp).unwrap();
        let mut command = Command::new(env!("CARGO_BIN_EXE_bazel-rs"));
        command
            .env_clear()
            // The native result lines this harness reads (crate::build_results).
            .env("BAZEL_RS_RESULT_LINES", "1")
            .env("PATH", "/usr/bin:/bin")
            .env("HOME", &home)
            .env("TMPDIR", &tmp)
            .env("BAZEL_RS_SNAPSHOT", "off")
            // Phase-1 prefetch is the walk's (the evaluator loads in its
            // Pkg nodes; `assert_identical_recordings` checks it too).
            .env("BAZEL_RS_ANALYSIS", "walk")
            .arg("--ignore_all_rc_files")
            .arg("--cache-dir=cache")
            .args(args)
            .current_dir(&self.root);
        for (key, value) in env {
            command.env(key, value);
        }
        command.explicit_modules().output().unwrap()
    }
}

impl Drop for Workspace {
    fn drop(&mut self) {
        let _ = sealed_trees::remove_tree(&self.root);
        let _ = sealed_trees::remove_tree(&self.scratch);
    }
}

fn stderr(output: &Output) -> String {
    without_elapsed(&String::from_utf8_lossy(&output.stderr))
}

/// What a command printed without the diagnostics (PERF lines, the
/// snapshot's).
fn program_stderr(output: &Output) -> Vec<String> {
    stderr(output)
        .lines()
        .filter(|line| !line.starts_with("PERF") && !line.starts_with("bazel-rs-snapshot:"))
        .map(str::to_owned)
        .collect()
}

/// The thread counts and worker jitter compared with the threads-1 run.
const THREADED: [(usize, u64); 6] = [(2, 0), (8, 0), (32, 0), (8, 40), (32, 25), (8, 7)];

/// One record-mode run.
struct Recorded {
    output: Output,
    canonical: String,
    /// The comparable part of the dump: every observation, environment
    /// digest, repository and integrity entry (without the `racy` flags),
    /// the pinned tables, the replay records (without times) and every
    /// taint but the test hooks'.
    lines: Vec<String>,
    /// `TAINT unstable` lines.
    unstable: Vec<String>,
    /// PERF-PREFETCH's `loaded=` (None: prefetch did not run).
    loaded: Option<u64>,
}

impl Recorded {
    fn differs_from(&self, other: &Recorded) -> Option<String> {
        if self.lines == other.lines && self.canonical != other.canonical {
            return Some(format!(
                "INPUTS-CANONICAL {} vs {}",
                self.canonical, other.canonical
            ));
        }
        if self.lines != other.lines {
            let only = |a: &[String], b: &[String]| -> Vec<String> {
                a.iter()
                    .filter(|line| !b.contains(line))
                    .take(12)
                    .cloned()
                    .collect()
            };
            return Some(format!(
                "dump lines differ\nonly here: {:#?}\nonly there: {:#?}",
                only(&self.lines, &other.lines),
                only(&other.lines, &self.lines)
            ));
        }
        None
    }
}

fn comparable(dump: &str) -> (String, Vec<String>, Vec<String>) {
    let mut canonical = String::new();
    let mut lines = Vec::new();
    let mut unstable = Vec::new();
    for line in dump.lines() {
        let kind = line.split(' ').next().unwrap_or_default();
        match kind {
            "INPUTS-CANONICAL" => canonical = line[kind.len() + 1..].to_owned(),
            "OBS" | "ENV" | "REPO" | "INT" => {
                lines.push(line.strip_suffix(" racy").unwrap_or(line).to_owned())
            }
            "ROOT" | "MAPPING" => lines.push(line.to_owned()),
            "TAINT" => {
                if line.starts_with("TAINT unstable ") {
                    unstable.push(line.to_owned());
                }
                if !line.starts_with("TAINT test-hook ") {
                    lines.push(line.to_owned());
                }
            }
            "REPLAY" => lines.push(
                line.split(' ')
                    .filter(|part| !part.starts_with("recorded:") && !part.starts_with("replay:"))
                    .collect::<Vec<_>>()
                    .join(" "),
            ),
            _ => {}
        }
    }
    (canonical, lines, unstable)
}

/// `build --nobuild --loading_phase_threads=<threads> <args>` in record
/// mode (with worker jitter and `extra` variables).
fn record(
    ws: &Workspace,
    threads: usize,
    jitter: u64,
    args: &[&str],
    extra: &[(&str, &str)],
) -> Recorded {
    let dump = ws
        .scratch
        .join(format!("dump-{}.txt", NEXT.fetch_add(1, Ordering::Relaxed)));
    let dump_text = dump.display().to_string();
    let jitter_text = jitter.to_string();
    let mut env = vec![
        ("BAZEL_RS_SNAPSHOT", "record"),
        ("BAZEL_RS_SNAPSHOT_DUMP", dump_text.as_str()),
        ("BAZEL_RS_TIMING", "1"),
    ];
    if jitter > 0 {
        env.push(("BAZEL_RS_LOADING_JITTER", jitter_text.as_str()));
    }
    env.extend_from_slice(extra);
    let threads = format!("--loading_phase_threads={threads}");
    let mut command = vec!["build", "--nobuild", threads.as_str()];
    command.extend_from_slice(args);
    let output = ws.run_env(&env, &command);
    let text = fs::read_to_string(&dump).unwrap_or_default();
    let (canonical, lines, unstable) = comparable(&text);
    assert!(
        !canonical.is_empty(),
        "{threads}: no INPUTS-CANONICAL: {}",
        stderr(&output)
    );
    let loaded = stderr(&output).lines().find_map(|line| {
        line.strip_prefix("PERF-PREFETCH ")?
            .split(' ')
            .find_map(|part| part.strip_prefix("loaded="))
            .and_then(|count| count.parse().ok())
    });
    Recorded {
        output,
        canonical,
        lines,
        unstable,
        loaded,
    }
}

/// `args` recorded at threads 1 (the reference) and at every [`THREADED`]
/// setting: the same exit code, stdout, stderr (without diagnostics) and
/// recording, never Unstable, and prefetch ran in every threaded run.
/// Returns the reference.
fn assert_identical_recordings(ws: &Workspace, args: &[&str]) -> Recorded {
    let reference = record(ws, 1, 0, args, &[]);
    assert!(
        reference.unstable.is_empty(),
        "threads 1: {:?}",
        reference.unstable
    );
    assert_eq!(reference.loaded, None, "threads 1 runs no prefetch");
    for (threads, jitter) in THREADED {
        let other = record(ws, threads, jitter, args, &[]);
        let context = format!("threads={threads} jitter={jitter}");
        assert!(other.unstable.is_empty(), "{context}: {:?}", other.unstable);
        assert_eq!(
            other.output.status.code(),
            reference.output.status.code(),
            "{context}: exit\n{}",
            stderr(&other.output)
        );
        assert_eq!(
            other.output.stdout, reference.output.stdout,
            "{context}: stdout"
        );
        assert_eq!(
            program_stderr(&other.output),
            program_stderr(&reference.output),
            "{context}: stderr"
        );
        if let Some(difference) = other.differs_from(&reference) {
            panic!("{context}: the recording differs from threads 1: {difference}");
        }
        assert!(
            other.loaded.is_some_and(|loaded| loaded > 0),
            "{context}: prefetch loaded nothing: {}",
            stderr(&other.output)
        );
    }
    reference
}

/// With `hooks` switched off (`BAZEL_RS_DEBUG_W6R_OFF`), some threaded
/// recording differs from `reference` (the threads-1 one): the fixture
/// needs the hooks.
fn assert_needs_hooks(ws: &Workspace, args: &[&str], reference: &Recorded, hooks: &str) {
    let mut differences = 0;
    for (threads, jitter) in [(8, 0), (32, 0), (8, 0), (32, 0)] {
        let other = record(
            ws,
            threads,
            jitter,
            args,
            &[("BAZEL_RS_DEBUG_W6R_OFF", hooks)],
        );
        assert_eq!(
            other.output.status.code(),
            reference.output.status.code(),
            "{hooks}: exit {}",
            stderr(&other.output)
        );
        if other.differs_from(reference).is_some() {
            differences += 1;
        }
    }
    assert!(
        differences > 0,
        "with {hooks} off, every threaded recording still equals threads 1's: the fixture does not need the hook"
    );
}

/// `build` with `args`, failing the test unless it succeeds.
fn build(ws: &Workspace, args: &[&str]) -> Output {
    let mut command = vec!["build"];
    command.extend_from_slice(args);
    let output = ws.run_env(&[], &command);
    assert!(output.status.success(), "{args:?}: {}", stderr(&output));
    output
}

/// Warm the workspace: a cold run publishes, a second one reuses once the
/// publications' stat fingerprints settled (a warm reuse refreshes a
/// manifest whose entries were racy), and a threaded one lets the workers'
/// speculative probes refresh the manifests of trees threads 1 never
/// verifies (an idempotent rewrite of the manifest, whose stat key a
/// recording holds: the cache converges, the recording is threads 1's from
/// then on).
fn warm(ws: &Workspace, args: &[&str]) {
    for threads in ["1", "1", "8"] {
        let threads = format!("--loading_phase_threads={threads}");
        let mut command = vec!["--nobuild", threads.as_str()];
        command.extend_from_slice(args);
        build(ws, &command);
        std::thread::sleep(std::time::Duration::from_millis(2200));
    }
}

/// `r(name = 'f<i>')` targets of `//many`, which keep the coordinator
/// analyzing while the workers run ahead, and the `app/rules.bzl` rule.
fn many(ws: &Workspace, count: usize) -> String {
    ws.file(
        "app/rules.bzl",
        "def _impl(ctx):\n    return [DefaultInfo()]\nr = rule(implementation = _impl, attrs = {'deps': attr.label_list(allow_files = True)})\n",
    );
    let targets: String = (0..count)
        .map(|i| format!("r(name = 'f{i}', visibility = ['//visibility:public'])\n"))
        .collect();
    ws.file(
        "many/BUILD.bazel",
        &format!("load('//app:rules.bzl', 'r')\n{targets}"),
    );
    (0..count)
        .map(|i| format!("'//many:f{i}'"))
        .collect::<Vec<_>>()
        .join(", ")
}

/// `//app:app`: `//many`'s targets, then `committed`, plus `speculative`
/// on a select() branch never taken (the prefetch follows it, the build
/// never commits it).
fn app(ws: &Workspace, count: usize, committed: &[&str], speculative: &[&str]) {
    let many = many(ws, count);
    let list = |labels: &[&str]| {
        labels
            .iter()
            .map(|label| format!("'{label}'"))
            .collect::<Vec<_>>()
            .join(", ")
    };
    let mut deps = many;
    if !committed.is_empty() {
        deps.push_str(", ");
        deps.push_str(&list(committed));
    }
    ws.file(
        "app/BUILD.bazel",
        &format!(
            "load(':rules.bzl', 'r')\nconfig_setting(name = 'never', values = {{'cpu': 'nonexistent'}})\nr(name = 'app', deps = [{deps}] + select({{':never': [{}], '//conditions:default': []}}))\n",
            list(speculative)
        ),
    );
}

/// The repository rules of the fixtures: `data` (BUILD, `defs.bzl`,
/// `file.txt`, `x/`, `pkg/`, an ignored `ign/`; optionally reading
/// `read_from`, a file of another repository: a recorded input), `env`
/// (declares `W6R_X` in `environ`), `link` (a host link `inc` to
/// `target`, globbed), `local` (`local = True`, replayed).
const RULES: &str = r#"def _data(ctx):
    if ctx.attr.read_from:
        ctx.read(ctx.attr.read_from)
    ctx.file("BUILD.bazel", "exports_files(['file.txt', 'defs.bzl'])\nfilegroup(name = 'files', srcs = ['file.txt'], visibility = ['//visibility:public'])\n")
    ctx.file("REPO.bazel", "ignore_directories(['ign'])\n")
    ctx.file("defs.bzl", "X = 1\n")
    ctx.file("file.txt", "data\n")
    ctx.file("data/a.txt", "a\n")
    ctx.file("data/sub/b.txt", "b\n")
    ctx.file("x/BUILD.bazel", "filegroup(name = 'files', visibility = ['//visibility:public'])\n")
    ctx.file("pkg/BUILD.bazel", "filegroup(name = 'files', visibility = ['//visibility:public'])\n")
    ctx.file("ign/BUILD.bazel", "filegroup(name = 'files', visibility = ['//visibility:public'])\n")

data = repository_rule(_data, attrs = {"read_from": attr.label()})

def _env(ctx):
    ctx.file("BUILD.bazel", "filegroup(name = 'files', visibility = ['//visibility:public'])\n")

env = repository_rule(_env, environ = ["W6R_X"])

def _link(ctx):
    ctx.symlink(ctx.attr.target, "inc")
    ctx.file("BUILD.bazel", "filegroup(name = 'g', srcs = glob(['inc/**']), visibility = ['//visibility:public'])\n")

link = repository_rule(_link, attrs = {"target": attr.string()})

def _local(ctx):
    ctx.file("v.txt", "v\n")
    ctx.file("BUILD.bazel", "filegroup(name = 'files', srcs = ['v.txt'], visibility = ['//visibility:public'])\n")

local = repository_rule(_local, local = True)
"#;

/// The use_repo_rule declarations of [`RULES`].
const USE_RULES: &str = "data = use_repo_rule('//:rules.bzl', 'data')\nenv = use_repo_rule('//:rules.bzl', 'env')\nlink = use_repo_rule('//:rules.bzl', 'link')\nlocal = use_repo_rule('//:rules.bzl', 'local')\n";

/// The workspace of the combined gate: the phase-1 prefetch shapes
/// (registered toolchains, a dependency chain with `.bzl` and BUILD
/// prints, select() branches not taken that load `.bzl` modules and probe
/// warm repositories), a `.bazelignore`d package and a `/...` toolchain
/// pattern filtered out by a repository's `ignore_directories`, a chain of
/// recorded inputs in other repositories (`@a` reads `@b`, `@b` reads
/// `@c`), a repository with a declared environment (`W6R_X`), a glob
/// through a link into another repository's store tree, a local
/// repository, and a `.bzl` link into a store tree.
fn combined_workspace() -> Workspace {
    let ws = Workspace::new(&format!(
        "module(name = 'main')\n{USE_RULES}data(name = 'early')\ndata(name = 'c')\ndata(name = 'b', read_from = '@c//:file.txt')\ndata(name = 'a', read_from = '@b//:file.txt')\ndata(name = 'dep')\ndata(name = 'spec_only')\nenv(name = 'env1')\nlocal(name = 'loc')\nregister_toolchains('//tc:all')\nregister_toolchains('@dep//ign/...')\n"
    ));
    ws.file("BUILD.bazel", "");
    ws.file("rules.bzl", RULES);
    ws.file(".bazelignore", "ignored\n");
    ws.file("ignored/BUILD.bazel", "fail('ignored must never load')\n");
    ws.file(
        "r/defs.bzl",
        "load(':print.bzl', 'say')\nprint('defs.bzl top-level')\ndef _lib(ctx):\n    return [DefaultInfo()]\nlib = rule(implementation = _lib, attrs = {'deps': attr.label_list()})\ndef _impl(ctx):\n    return [platform_common.ToolchainInfo(name = ctx.attr.tool), DefaultInfo()]\nimpl = rule(implementation = _impl, attrs = {'tool': attr.string()})\ndef _needs(ctx):\n    return [DefaultInfo()]\nneeds = rule(implementation = _needs, toolchains = ['//tc:type'])\n",
    );
    ws.file(
        "r/print.bzl",
        "print('print.bzl top-level')\ndef say(message):\n    print(message)\n",
    );
    ws.file("r/BUILD.bazel", "");
    ws.file(
        "os/BUILD.bazel",
        "package(default_visibility = ['//visibility:public'])\nconstraint_setting(name = 'os')\nconstraint_value(name = 'linux', constraint_setting = ':os')\n",
    );
    ws.file(
        "p/BUILD.bazel",
        "package(default_visibility = ['//visibility:public'])\nplatform(name = 'linux', constraint_values = ['//os:linux'])\n",
    );
    ws.file(
        "tc/BUILD.bazel",
        "load('//r:defs.bzl', 'impl', 'needs')\nload('//r:print.bzl', 'say')\nsay('tc BUILD')\npackage(default_visibility = ['//visibility:public'])\ntoolchain_type(name = 'type')\nimpl(name = 'impl', tool = 'linux')\ntoolchain(name = 'linux', toolchain_type = ':type', target_compatible_with = ['//os:linux'], toolchain = ':impl')\nneeds(name = 't')\n",
    );
    for (package, deps) in [("d1", "['//d2:x']"), ("d2", "['//d3:x']"), ("d3", "[]")] {
        ws.file(
            &format!("{package}/BUILD.bazel"),
            &format!(
                "load('//r:defs.bzl', 'lib')\nload('//r:print.bzl', 'say')\nsay('{package} BUILD')\nlib(name = 'x', deps = {deps}, visibility = ['//visibility:public'])\nfilegroup(name = 'g', srcs = glob(['**/*.txt']), visibility = ['//visibility:public'])\n"
            ),
        );
        ws.file(&format!("{package}/data/f.txt"), "f\n");
    }
    // A speculative package loading a module of a warm repository the
    // build never demands otherwise, and a committed one loading the same
    // module later (a `.bzl` cache hit).
    ws.file(
        "spec/BUILD.bazel",
        "load('@spec_only//:defs.bzl', 'X')\nload('@dep//:defs.bzl', Y = 'X')\nfilegroup(name = 'x', visibility = ['//visibility:public'])\n",
    );
    ws.file(
        "late/BUILD.bazel",
        "load('@dep//:defs.bzl', 'X')\nload('//app:rules.bzl', 'r')\nr(name = 'x', deps = ['@a//pkg:files', '@env1//:files', '@loc//:files', '@dep//:files'], visibility = ['//visibility:public'])\n",
    );
    app(
        &ws,
        600,
        &["//tc:t", "//d1:x", "//d1:g", "//late:x"],
        &["//spec:x", "@dep//x:files", "@spec_only//x:files"],
    );
    ws
}

/// The combined gate (PREFETCH-RECORD-DESIGN test 6).
#[test]
fn record_mode_inputs_are_identical_with_phase1_prefetch() {
    let ws = combined_workspace();
    // Published once, never demanded by the build: only a speculative
    // unit's probe finds it.
    build(&ws, &["--nobuild", "@spec_only//:files"]);
    let args = [
        "--repo_env=W6R_X=one",
        "--host_platform=//p:linux",
        "@early//:files",
        "//app:app",
    ];
    warm(&ws, &args);
    let reference = assert_identical_recordings(&ws, &args);
    let text = stderr(&reference.output);
    assert!(reference.output.status.success(), "{text}");
    assert!(text.contains("eligible=yes"), "{text}");
    for line in ["print.bzl top-level", "defs.bzl top-level", "d3 BUILD"] {
        assert_eq!(text.matches(line).count(), 1, "{line}: {text}");
    }
    assert!(
        text.contains(
            "WARNING: Pattern '@dep//ign/...' was filtered out by ignored directory 'ign'"
        ),
        "{text}"
    );
    // The recording holds what threads 1 observes of every feature.
    let has = |prefix: &str| reference.lines.iter().any(|line| line.starts_with(prefix));
    assert!(has("ENV W6R_X sha256:"), "{:#?}", reference.lines);
    assert!(has("REPLAY +local+loc match "), "{:#?}", reference.lines);
    let repos = reference
        .lines
        .iter()
        .filter(|line| line.starts_with("REPO "))
        .count();
    assert!(repos >= 4, "{:#?}", reference.lines);
    // Nothing of the never-committed `//spec` or `@spec_only`.
    assert!(
        !reference
            .lines
            .iter()
            .any(|line| line.contains("/spec/BUILD.bazel") || line.contains("+data+spec_only")),
        "{:#?}",
        reference.lines
    );
}

/// The f2-style workspace of the targeted fixtures: `@early` (its pattern
/// prepares the `use_repo_rule` group so the workers probe warm trees),
/// `declarations` and `//app:app` as [`app`] makes it.
fn targeted(declarations: &str, committed: &[&str], speculative: &[&str]) -> Workspace {
    let ws = Workspace::new(&format!(
        "module(name = 'main')\n{USE_RULES}data(name = 'early')\n{declarations}"
    ));
    ws.file("BUILD.bazel", "");
    ws.file("rules.bzl", RULES);
    app(&ws, 1500, committed, speculative);
    ws
}

/// a. A speculative unit (a select() branch not taken) evaluates
/// `@data//:defs.bzl` first; `//p1`, committed after `//mid`'s many
/// targets, loads it from the cache: its observations reach the recording
/// with the cache entry (hook `bzl`).
#[test]
fn a_bzl_a_speculative_unit_evaluated_first_is_recorded_at_the_cache_hit() {
    let ws = targeted("data(name = 'data')\n", &["//mid:m"], &["//spec:x"]);
    ws.file(
        "spec/BUILD.bazel",
        "load('@data//:defs.bzl', 'X')\nfilegroup(name = 'x', visibility = ['//visibility:public'])\n",
    );
    let mid: String = (0..1500)
        .map(|i| format!("r(name = 'm{i}', visibility = ['//visibility:public'])\n"))
        .collect();
    ws.file(
        "mid/BUILD.bazel",
        &format!(
            "load('//app:rules.bzl', 'r')\n{mid}r(name = 'm', deps = ['//p1:p1'], visibility = ['//visibility:public'])\n"
        ),
    );
    ws.file(
        "p1/BUILD.bazel",
        "load('@data//:defs.bzl', 'X')\nload('//app:rules.bzl', 'r')\nr(name = 'p1', visibility = ['//visibility:public'])\n",
    );
    let args = ["@early//:files", "//app:app"];
    warm(&ws, &args);
    let reference = assert_identical_recordings(&ws, &args);
    assert!(reads_a_store_module(&reference), "{:#?}", reference.lines);
    assert_needs_hooks(&ws, &args, &reference, "bzl");
}

/// Whether a recording holds the READ of a `defs.bzl` in a store tree.
fn reads_a_store_module(recorded: &Recorded) -> bool {
    recorded.lines.iter().any(|line| {
        line.starts_with("OBS ")
            && line.contains("/cache/repositories/direct/trees/")
            && line.contains("/defs.bzl READ ")
    })
}

/// b. `//p1` loads `@data//:defs.bzl`, then a module of an extension
/// repository: its worker unit publishes the first module and aborts at
/// the extension; the coordinator's inline re-run reads the module from the
/// cache (hook `bzl`).
#[test]
fn b_bzl_an_aborted_units_module_is_recorded_at_the_inline_reruns_hit() {
    let ws = targeted(
        "data(name = 'data')\next = use_extension('//:ext.bzl', 'ext')\nuse_repo(ext, 'from_ext')\n",
        &["//p1:p1"],
        &[],
    );
    ws.file(
        "ext.bzl",
        "def _from_ext(ctx):\n    ctx.file('BUILD.bazel', '')\n    ctx.file('defs.bzl', 'Y = 2\\n')\n\nfrom_ext = repository_rule(_from_ext)\n\ndef _ext(module_ctx):\n    from_ext(name = 'from_ext')\n\next = module_extension(_ext)\n",
    );
    ws.file(
        "p1/BUILD.bazel",
        "load('@data//:defs.bzl', 'X')\nload('@from_ext//:defs.bzl', 'Y')\nload('//app:rules.bzl', 'r')\nr(name = 'p1', visibility = ['//visibility:public'])\n",
    );
    let args = ["@early//:files", "//app:app"];
    warm(&ws, &args);
    let reference = assert_identical_recordings(&ws, &args);
    assert!(reads_a_store_module(&reference), "{:#?}", reference.lines);
    assert_needs_hooks(&ws, &args, &reference, "bzl");
}

/// c. A worker loads `@data//x` (a select() branch not taken) and computes
/// `@data`'s ignored subdirectories; `register_toolchains('@data//ign/...')`
/// is expanded later by `//tc:t`'s toolchain resolution, which finds the
/// memo filled, warns and skips the pattern (hook `ignored`).
#[test]
fn c_ignored_a_worker_filled_memo_is_recorded_at_the_toolchain_patterns_hit() {
    let ws = targeted(
        "data(name = 'data')\nregister_toolchains('@data//ign/...')\n",
        &["//tc:t"],
        &["@data//x:files"],
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
    let args = ["--host_platform=//p:linux", "@early//:files", "//app:app"];
    warm(&ws, &args);
    let reference = assert_identical_recordings(&ws, &args);
    let text = stderr(&reference.output);
    assert!(
        text.contains(
            "WARNING: Pattern '@data//ign/...' was filtered out by ignored directory 'ign'"
        ),
        "{text}"
    );
    assert_needs_hooks(&ws, &args, &reference, "ignored");
}

/// d. `@a` records a read of `@b//:file.txt`: a worker probes `@a` for
/// `//spec` (never committed) and obtains `@b` through `@a`'s check;
/// `//mid:m` demands the filled `@a` later. Both trees, their recorded
/// inputs' reads and `@b` — reached only through `@a`'s probe — reach the
/// recording with the publications (hook `probe`).
#[test]
fn d_probe_a_root_probed_for_a_never_committed_unit_records_where_it_counts() {
    let ws = targeted(
        "data(name = 'b')\ndata(name = 'a', read_from = '@b//:file.txt')\n",
        &["//mid:m"],
        &["//spec:x"],
    );
    ws.file(
        "spec/BUILD.bazel",
        "load('@a//:defs.bzl', 'X')\nfilegroup(name = 'x', visibility = ['//visibility:public'])\n",
    );
    let mid: String = (0..1500)
        .map(|i| format!("r(name = 'm{i}', visibility = ['//visibility:public'])\n"))
        .collect();
    ws.file(
        "mid/BUILD.bazel",
        &format!(
            "load('//app:rules.bzl', 'r')\n{mid}r(name = 'm', deps = ['@a//pkg:files'], visibility = ['//visibility:public'])\n"
        ),
    );
    let args = ["@early//:files", "//app:app"];
    warm(&ws, &args);
    let reference = assert_identical_recordings(&ws, &args);
    // `@early`'s, `@a`'s and `@b`'s trees.
    let repos: Vec<&String> = reference
        .lines
        .iter()
        .filter(|line| line.starts_with("REPO "))
        .collect();
    assert_eq!(repos.len(), 3, "{repos:#?}");
    assert_needs_hooks(&ws, &args, &reference, "probe");
}

/// e. Environment values a worker read first, in an extension's group
/// (prepared early by the `@early_ext` pattern, so the workers probe its
/// repositories): `@r1` and `@r2` both declare `W6R_X`; a worker probes
/// `@r1` for a select() branch not taken (its spec key reads `W6R_X`, then
/// the probe misses: `@r1` is never published), `@r2` is demanded later —
/// its spec key's lookup of `W6R_X` is an environment snapshot hit (hook
/// `env`). And `@needs_loc`, declaring `W6R_Y`, whose recorded input lies in
/// a local repository: a worker's probe computes its spec key, then misses
/// (workers never execute the local rule); the coordinator's evaluation
/// hits the spec-key memo (hook `spec`). Its warm check re-reads the
/// declared environment too (the execution watched it), so the spec-key
/// replay matters once the environment hook is off: with `env` off
/// `W6R_Y` is still recorded (the spec key's set), with both off it is
/// not.
#[test]
fn e_env_values_a_worker_read_first_are_recorded_at_the_coordinators_lookup() {
    let ws = targeted(
        "local(name = 'loc')\next = use_extension('//:ext.bzl', 'ext')\nuse_repo(ext, 'early_ext', 'r1', 'r2', 'needs_loc')\n",
        &["//late:x", "@early_ext//:files"],
        &["@r1//:files"],
    );
    ws.file(
        "ext.bzl",
        "def _plain(ctx):\n    ctx.file('BUILD.bazel', \"filegroup(name = 'files', visibility = ['//visibility:public'])\\n\")\n\nplain = repository_rule(_plain)\n\nenvx = repository_rule(_plain, environ = ['W6R_X'])\n\ndef _gen(ctx):\n    ctx.read(ctx.attr.read_from)\n    ctx.file('BUILD.bazel', \"filegroup(name = 'files', visibility = ['//visibility:public'])\\n\")\n\ngen = repository_rule(_gen, environ = ['W6R_Y'], attrs = {'read_from': attr.label()})\n\ndef _ext(module_ctx):\n    plain(name = 'early_ext')\n    envx(name = 'r1')\n    envx(name = 'r2')\n    gen(name = 'needs_loc', read_from = Label('@loc//:v.txt'))\n\next = module_extension(_ext)\n",
    );
    let late: String = (0..800)
        .map(|i| format!("r(name = 'l{i}', visibility = ['//visibility:public'])\n"))
        .collect();
    ws.file(
        "late/BUILD.bazel",
        &format!(
            "load('//app:rules.bzl', 'r')\n{late}r(name = 'x', deps = ['@r2//:files', '@needs_loc//:files'], visibility = ['//visibility:public'])\n"
        ),
    );
    let args = [
        "--repo_env=W6R_X=one",
        "--repo_env=W6R_Y=two",
        "@early_ext//:files",
        "//app:app",
    ];
    warm(&ws, &args);
    let reference = assert_identical_recordings(&ws, &args);
    for name in ["W6R_X", "W6R_Y"] {
        assert!(
            reference
                .lines
                .iter()
                .any(|line| line.starts_with(&format!("ENV {name} sha256:"))),
            "{name}: {:#?}",
            reference.lines
        );
    }
    assert_needs_hooks(&ws, &args, &reference, "env");
    let env_names = |recorded: &Recorded| -> Vec<String> {
        recorded
            .lines
            .iter()
            .filter_map(|line| line.strip_prefix("ENV "))
            .filter_map(|line| line.split(' ').next())
            .map(str::to_owned)
            .collect()
    };
    let mut lost_with_both = 0;
    for threads in [8, 32, 8, 32] {
        let env_off = record(&ws, threads, 0, &args, &[("BAZEL_RS_DEBUG_W6R_OFF", "env")]);
        assert!(
            env_names(&env_off).iter().any(|name| name == "W6R_Y"),
            "threads {threads}, env off: the spec key's set carries W6R_Y: {:?}",
            env_names(&env_off)
        );
        let both_off = record(
            &ws,
            threads,
            0,
            &args,
            &[("BAZEL_RS_DEBUG_W6R_OFF", "env,spec")],
        );
        if !env_names(&both_off).iter().any(|name| name == "W6R_Y") {
            lost_with_both += 1;
        }
    }
    assert!(
        lost_with_both > 0,
        "with env and spec off W6R_Y is still recorded: the fixture does not need the spec-key replay"
    );
}

/// f. `@linker`'s glob follows its link `inc` into `@dep`'s published
/// store tree; a worker probes `@dep` for a select() branch not taken, so
/// its root is published but never obtained by the build. The crossing
/// records `@dep`'s tree as at threads 1 (hook `link`: the policy holds
/// the sequential roots while recording; with the published root among
/// them the crossing would record nothing).
#[test]
fn f_link_a_glob_into_a_worker_published_store_tree_records_the_crossing() {
    let declarations =
        |target: &str| format!("data(name = 'dep')\nlink(name = 'linker', target = '{target}')\n");
    let ws = targeted(
        &declarations("/nonexistent"),
        &["//late:x"],
        &["@dep//x:files"],
    );
    let late: String = (0..800)
        .map(|i| format!("r(name = 'l{i}', visibility = ['//visibility:public'])\n"))
        .collect();
    ws.file(
        "late/BUILD.bazel",
        &format!(
            "load('//app:rules.bzl', 'r')\n{late}r(name = 'x', deps = ['@linker//:g'], visibility = ['//visibility:public'])\n"
        ),
    );
    // Publish `@dep` to learn its tree; the link's target changes only
    // `@linker`'s attributes.
    let published = build(&ws, &["--loading_phase_threads=1", "@dep//:files"]);
    let stdout = String::from_utf8_lossy(&published.stdout).into_owned();
    let file = stdout
        .lines()
        .find_map(|line| line.strip_prefix("@@+data+dep//:files\t"))
        .unwrap_or_else(|| panic!("{stdout}"));
    let tree = fs::canonicalize(file)
        .unwrap()
        .parent()
        .unwrap()
        .to_path_buf();
    let data = tree.join("data");
    let module = format!(
        "module(name = 'main')\n{USE_RULES}data(name = 'early')\n{}",
        declarations(&data.display().to_string())
    );
    ws.file("MODULE.bazel", &module);
    let args = ["@early//:files", "//app:app"];
    warm(&ws, &args);
    let reference = assert_identical_recordings(&ws, &args);
    let tree_text = tree.display().to_string();
    assert!(
        reference
            .lines
            .iter()
            .any(|line| line.starts_with("REPO direct ") && line.contains(&tree_text)),
        "{tree_text}: {:#?}",
        reference.lines
    );
    assert_needs_hooks(&ws, &args, &reference, "link");
}

/// g. `//loaded` (a select() branch not taken) loads `:linked.bzl`, a link
/// into a `local_path_override` module's root — a hop into another live
/// repository root, which taints a run. A worker's unit takes no such
/// decision (it aborts as order-dependent) and the build never commits
/// it: the run stays eligible as at threads 1 (hook `unit`).
#[cfg(unix)]
#[test]
fn g_unit_a_never_committed_units_cross_repository_hop_does_not_taint() {
    let lib = Workspace::new("module(name = 'lib')\n");
    lib.file("BUILD.bazel", "");
    lib.file("defs.bzl", "value = 'from_lib'\n");
    let ws = targeted(
        &format!(
            "bazel_dep(name = 'lib', version = '1')\nlocal_path_override(module_name = 'lib', path = '{}')\n",
            lib.root.display()
        ),
        &[],
        &["//loaded:from_lib"],
    );
    ws.file(
        "loaded/BUILD.bazel",
        "load(':linked.bzl', 'value')\nfilegroup(name = value, visibility = ['//visibility:public'])\n",
    );
    std::os::unix::fs::symlink(lib.root.join("defs.bzl"), ws.root.join("loaded/linked.bzl"))
        .unwrap();
    let args = ["@early//:files", "//app:app"];
    warm(&ws, &args);
    let reference = assert_identical_recordings(&ws, &args);
    let text = stderr(&reference.output);
    assert!(text.contains("eligible=yes"), "{text}");
    assert!(
        !reference
            .lines
            .iter()
            .any(|line| line.starts_with("TAINT cross-repository-symlink")),
        "{:#?}",
        reference.lines
    );
    assert_needs_hooks(&ws, &args, &reference, "unit");
}

/// h. A worker loads `//spec` (a select() branch not taken) and the build
/// never commits it: nothing it observed is in the recording (hook
/// `unit`), and neither are the canonical forms its link policy took
/// first (hook `canon`: recorded once at the end of planning).
#[test]
fn h_unit_an_unused_units_observations_are_absent() {
    let ws = targeted("data(name = 'dep')\n", &["//late:x"], &["//spec:x"]);
    ws.file(
        "spec/BUILD.bazel",
        "filegroup(name = 'x', srcs = glob(['**/*.txt']), visibility = ['//visibility:public'])\n",
    );
    ws.file("spec/data/s.txt", "s\n");
    let late: String = (0..800)
        .map(|i| format!("r(name = 'l{i}', visibility = ['//visibility:public'])\n"))
        .collect();
    ws.file(
        "late/BUILD.bazel",
        &format!(
            "load('//app:rules.bzl', 'r')\n{late}filegroup(name = 'g', srcs = glob(['**/*.txt']))\nr(name = 'x', deps = ['@dep//:files', ':g'], visibility = ['//visibility:public'])\n"
        ),
    );
    ws.file("late/data/l.txt", "l\n");
    let args = ["@early//:files", "//app:app"];
    warm(&ws, &args);
    let reference = assert_identical_recordings(&ws, &args);
    assert!(
        !reference.lines.iter().any(|line| line.contains("/spec/")),
        "{:#?}",
        reference.lines
    );
    assert_needs_hooks(&ws, &args, &reference, "unit");
    assert_needs_hooks(&ws, &args, &reference, "canon");
}

/// One `on`/`verify` run at `threads`: (output, the snapshot diagnostics).
fn snapshot_run(ws: &Workspace, mode: &str, threads: usize, args: &[&str]) -> (Output, String) {
    let threads = format!("--loading_phase_threads={threads}");
    let mut command = vec!["build", threads.as_str()];
    command.extend_from_slice(args);
    let output = ws.run_env(
        &[
            ("BAZEL_RS_SNAPSHOT", mode),
            ("BAZEL_RS_SNAPSHOT_EXPLAIN", "1"),
            ("BAZEL_RS_SNAPSHOT_VERIFY_STRICT", "1"),
            ("BAZEL_RS_TIMING", "1"),
        ],
        &command,
    );
    let diagnostics = stderr(&output)
        .lines()
        .filter(|line| line.starts_with("bazel-rs-snapshot:"))
        .collect::<Vec<_>>()
        .join("\n");
    (output, diagnostics)
}

/// PREFETCH-RECORD-DESIGN tests 8 and 9 on the combined workspace: a
/// snapshot saved at one thread count verifies strictly at the others
/// (verify mode's shadow analysis records with prefetch on: the two
/// recordings agree), an `on` snapshot saved at threads 8 hits at threads
/// 1 and 32 with the output of the run without the snapshot, and a hit
/// replays no
/// PERF-PREFETCH (it is diagnostics, never part of the captured window).
#[test]
fn snapshots_saved_at_one_thread_count_verify_and_hit_at_the_others() {
    let ws = combined_workspace();
    build(&ws, &["--nobuild", "@spec_only//:files"]);
    let args = [
        "--repo_env=W6R_X=one",
        "--host_platform=//p:linux",
        "--nobuild",
        "@early//:files",
        "//app:app",
    ];
    warm(
        &ws,
        &args[..2]
            .iter()
            .chain(&args[3..])
            .copied()
            .collect::<Vec<_>>(),
    );
    let off = ws.run_env(&[], &{
        let mut command = vec!["build"];
        command.extend_from_slice(&args);
        command
    });
    assert!(off.status.success(), "{}", stderr(&off));
    for (save, verifies) in [(1, [8, 32]), (8, [1, 32])] {
        let _ = sealed_trees::remove_tree(ws.snapshots());
        let (saved, explain) = snapshot_run(&ws, "verify", save, &args);
        assert!(saved.status.success(), "{}", stderr(&saved));
        assert!(explain.contains(" saved "), "save at {save}: {explain}");
        for threads in verifies {
            let (verified, explain) = snapshot_run(&ws, "verify", threads, &args);
            assert!(
                verified.status.success() && explain.contains("bazel-rs-snapshot: verified "),
                "saved at {save}, verified at {threads} (exit {:?}): {explain}\n{}",
                verified.status.code(),
                stderr(&verified)
            );
            assert!(
                !ws.snapshots().join("verify.log").exists(),
                "saved at {save}, verified at {threads}: {}",
                fs::read_to_string(ws.snapshots().join("verify.log")).unwrap_or_default()
            );
            assert_eq!(program_stderr(&verified), program_stderr(&off));
            assert_eq!(verified.stdout, off.stdout);
        }
    }
    // `on`: saved at threads 8 (prefetch on; its PERF-PREFETCH line goes
    // to the diagnostics stream), hits at 1 and 32 replay no PERF-PREFETCH.
    let _ = sealed_trees::remove_tree(ws.snapshots());
    let (saved, explain) = snapshot_run(&ws, "on", 8, &args);
    assert!(saved.status.success(), "{}", stderr(&saved));
    assert!(explain.contains(" saved "), "{explain}");
    assert!(
        stderr(&saved)
            .lines()
            .any(|line| line.starts_with("PERF-PREFETCH ")),
        "{}",
        stderr(&saved)
    );
    for threads in [1, 32] {
        let (hit, explain) = snapshot_run(&ws, "on", threads, &args);
        assert!(hit.status.success(), "{}", stderr(&hit));
        assert!(
            explain.contains("bazel-rs-snapshot: hit "),
            "{threads}: {explain}"
        );
        assert!(
            !stderr(&hit).contains("PERF-PREFETCH"),
            "{threads}: {}",
            stderr(&hit)
        );
        assert_eq!(program_stderr(&hit), program_stderr(&off), "{threads}");
        assert_eq!(hit.stdout, off.stdout, "{threads}");
    }
}

/// The repository store (`<cache>/repositories`, and so the cache root)
/// exists from the start of the recording window
/// (crate::observed_fs::create_with_window): planning records its CANON at
/// the end (RepositorySet::record_link_anchors), and a background worker —
/// a speculative fetch, a module-root warm-up, a registry prefetch — may
/// create it at any moment of the window. On a fresh cache whose store no
/// sequential step creates, a run where such a worker created it first
/// (`BAZEL_RS_DEBUG_STORE_RACE=create`, just before the records) records
/// what a run without one does — in the walk and the evaluator. Before,
/// the first recorded the store present and the second absent.
#[test]
fn a_store_a_background_worker_creates_first_records_the_same() {
    for analysis in ["walk", "eval"] {
        let ws = Workspace::new("module(name = 'm')\n");
        ws.file(
            "BUILD.bazel",
            "genrule(name = 'g', outs = ['g.txt'], cmd = 'echo hi > $@')\n",
        );
        let fresh = |hook: Option<&str>| {
            sealed_trees::remove_tree(ws.root.join("cache")).unwrap();
            let mut extra = vec![("BAZEL_RS_ANALYSIS", analysis)];
            if let Some(hook) = hook {
                extra.push(("BAZEL_RS_DEBUG_STORE_RACE", hook));
            }
            record(&ws, 1, 0, &["//:g"], &extra)
        };
        let reference = fresh(None);
        let raced = fresh(Some("create"));
        for recorded in [&reference, &raced] {
            assert_eq!(
                recorded.output.status.code(),
                Some(0),
                "{analysis}: {}",
                stderr(&recorded.output)
            );
        }
        if let Some(difference) = raced.differs_from(&reference) {
            panic!("{analysis}: {difference}");
        }
        let store = reference
            .lines
            .iter()
            .find(|line| line.contains("/cache/repositories CANON "))
            .unwrap_or_else(|| panic!("{analysis}: no store CANON: {:#?}", reference.lines));
        assert!(store.contains(" CANON path:"), "{analysis}: {store}");
    }
}
