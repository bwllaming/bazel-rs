//! Module extensions prepared ahead of their sequential demand
//! (docs/COLD_ANALYSIS.md, "Module extensions prepared ahead";
//! src/extension_speculation.rs): with a lockfile, the extension groups the
//! root module uses are prepared on background workers at startup — their
//! definition `.bzl` graph loaded from registry module trees nobody has
//! materialized yet, their lockfile entry checked and bound — and the
//! coordinator commits each where the sequential engine prepares it. These
//! tests pin that nothing observable changes: builds at
//! `--loading_phase_threads` 1 (the sequential engine), 2, 8, 32 and 8 with
//! worker jitter are byte-identical in exit code, stdout, stderr (`.bzl`
//! prints of the extension definitions, the use_repo fixup warning), the
//! lockfile, the hidden lockfile and the analysis snapshot's recording —
//! with an extension that prints, one that fails, one never demanded, one
//! whose recorded input changes between its speculation and its demand
//! (the outcome is dropped and the extension evaluates again), and one
//! whose definition loads the repositories of another (the speculations
//! chain).

#![allow(clippy::disallowed_methods)] // test harness: starts processes directly (clippy.toml)
use base64::Engine;
use sha2::{Digest, Sha256};
use std::fs;
use std::path::Path;
use std::process::Command;
include!("support/explicit_modules.rs");
include!("support/console.rs");

/// The registry module `extmod`: the extension `gen` (its definition and a
/// helper print at load; its implementation prints, reads the workspace's
/// `input.txt` and generates one repository per `repo` tag, its metadata
/// asking the root module to import them all).
const EXT_BZL: &str = r#"load("//:helper.bzl", "HELPER")
print("extmod ext.bzl loaded with " + HELPER)

def _simple_impl(rctx):
    rctx.file("BUILD.bazel", "exports_files(['data.txt'])\n")
    rctx.file("data.txt", rctx.attr.content + "\n")
    rctx.file("defs.bzl", "VALUE = %r\n" % rctx.attr.content)
    rctx.file("lib.bzl", "def lib(text):\n    return 'lib(' + text + ')'\n")

simple = repository_rule(implementation = _simple_impl, attrs = {"content": attr.string()})

def _gen_impl(mctx):
    text = mctx.read(mctx.path(Label("@@//:input.txt"))).strip()
    print("gen evaluated with input " + text)
    for module in mctx.modules:
        for repo in module.tags.repo:
            simple(name = repo.name, content = repo.content + " " + text)
    return mctx.extension_metadata(root_module_direct_deps = "all", root_module_direct_dev_deps = [])

gen = module_extension(
    implementation = _gen_impl,
    tag_classes = {"repo": tag_class(attrs = {"name": attr.string(), "content": attr.string()})},
)
"#;

fn archive(module: &str, files: &[(&str, &str)]) -> Vec<u8> {
    let mut builder = tar::Builder::new(Vec::new());
    for (name, data) in files {
        let mut header = tar::Header::new_ustar();
        header.set_mode(0o644);
        header.set_size(data.len() as u64);
        header.set_entry_type(tar::EntryType::Regular);
        header.set_path(format!("{module}-1/{name}")).unwrap();
        header.set_cksum();
        builder.append(&header, data.as_bytes()).unwrap();
    }
    let tar = builder.into_inner().unwrap();
    let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    std::io::Write::write_all(&mut encoder, &tar).unwrap();
    encoder.finish().unwrap()
}

struct Fixture(tempfile::TempDir);

#[derive(Debug)]
struct Run {
    code: Option<i32>,
    stdout: String,
    stderr: String,
    timing: String,
    lockfile: Option<String>,
    hidden: Option<String>,
    records: Vec<String>,
}

impl Fixture {
    fn new() -> Self {
        let fixture = Self(
            tempfile::Builder::new()
                .prefix("extension-speculation-")
                .tempdir_in(crate::test_temp::root())
                .unwrap(),
        );
        let module = "module(name = 'extmod', version = '1')\n";
        let bytes = archive(
            "extmod",
            &[
                ("MODULE.bazel", module),
                ("BUILD.bazel", ""),
                (
                    "helper.bzl",
                    "print('extmod helper loaded')\nHELPER = 'helper'\n",
                ),
                ("ext.bzl", EXT_BZL),
            ],
        );
        let path = fixture.path().join("extmod-1.tar.gz");
        fs::write(&path, &bytes).unwrap();
        fixture.write("registry/modules/extmod/1/MODULE.bazel", module);
        let source = serde_json::json!({
            "url": format!("file://{}", path.display()),
            "integrity": format!(
                "sha256-{}",
                base64::engine::general_purpose::STANDARD.encode(Sha256::digest(&bytes))
            ),
            "strip_prefix": "extmod-1",
        });
        fixture.write("registry/modules/extmod/1/source.json", &source.to_string());
        fixture.write(
            "MODULE.bazel",
            r#"module(name = "main")
bazel_dep(name = "extmod", version = "1")
gen = use_extension("@extmod//:ext.bzl", "gen")
gen.repo(name = "gen_lib", content = "lib")
gen.repo(name = "gen_data", content = "data")
gen.repo(name = "gen_unimported", content = "unimported")
use_repo(gen, "gen_lib", "gen_data")
user = use_extension("//ext:user.bzl", "user")
use_repo(user, "user_repo")
never = use_extension("//ext:never.bzl", "never")
use_repo(never, "never_repo")
fails = use_extension("//ext:fails.bzl", "fails")
use_repo(fails, "fail_repo")
touch = use_repo_rule("//ext:touch.bzl", "touch")
touch(name = "touch")
y = use_extension("//ext:yz.bzl", "y")
use_repo(y, "y_repo")
z = use_extension("//ext:yz.bzl", "z")
use_repo(z, "z_repo")
x = use_extension("//ext:x.bzl", "x")
use_repo(x, "x_repo")
"#,
        );
        fixture.write("input.txt", "v0\n");
        fixture.write("BUILD.bazel", "exports_files(['input.txt'])\n");
        fixture.write("ext/BUILD.bazel", "");
        // An extension whose definition loads a repository another one
        // generates.
        fixture.write(
            "ext/user.bzl",
            r#"load("@gen_lib//:lib.bzl", "lib")
print("user.bzl loaded: " + lib("x"))

def _user_repo_impl(rctx):
    rctx.file("BUILD.bazel", "")
    rctx.file("defs.bzl", "USER = %r\n" % rctx.attr.text)

user_repo = repository_rule(implementation = _user_repo_impl, attrs = {"text": attr.string()})

def _user_impl(mctx):
    print("user evaluated")
    user_repo(name = "user_repo", text = lib("user"))

user = module_extension(implementation = _user_impl)
"#,
        );
        fixture.write(
            "ext/never.bzl",
            r#"def _never_repo_impl(rctx):
    rctx.file("BUILD.bazel", "")
    rctx.file("defs.bzl", "NEVER = 'never'\n")

never_repo = repository_rule(implementation = _never_repo_impl)

def _never_impl(mctx):
    print("never evaluated")
    never_repo(name = "never_repo")

never = module_extension(implementation = _never_impl)
"#,
        );
        fixture.write(
            "ext/fails.bzl",
            r#"print("fails.bzl loaded")

def _fails_impl(mctx):
    print("fails evaluating")
    fail("the fails extension fails on purpose")

fails = module_extension(implementation = _fails_impl)
"#,
        );
        // A local repository rule that rewrites the input `gen` reads.
        fixture.write(
            "ext/touch.bzl",
            r#"def _touch_impl(rctx):
    rctx.execute(["sh", "-c", "printf 'v1\\n' > '%s/input.txt'" % rctx.workspace_root])
    rctx.file("BUILD.bazel", "")
    rctx.file("defs.bzl", "TOUCHED = True\n")

touch = repository_rule(implementation = _touch_impl, local = True)
"#,
        );
        fixture.write(
            "a/BUILD.bazel",
            "load('@user_repo//:defs.bzl', 'USER')\nprint('a sees ' + USER)\nfilegroup(name = 'a')\n",
        );
        fixture.write(
            "b/BUILD.bazel",
            "load('@gen_data//:defs.bzl', 'VALUE')\nprint('b sees ' + VALUE)\nfilegroup(name = 'b', srcs = ['@gen_data//:data.txt'])\n",
        );
        fixture.write("c/BUILD.bazel", "filegroup(name = 'c')\n");
        fixture.write(
            "never/BUILD.bazel",
            "load('@never_repo//:defs.bzl', 'NEVER')\nfilegroup(name = 'never')\n",
        );
        fixture.write(
            "failing/BUILD.bazel",
            "load('@fail_repo//:defs.bzl', 'X')\nfilegroup(name = 'failing')\n",
        );
        fixture.write(
            "changing/BUILD.bazel",
            "load('@touch//:defs.bzl', 'TOUCHED')\nload('@gen_data//:defs.bzl', 'VALUE')\nprint('changing sees ' + VALUE)\nfilegroup(name = 'changing')\n",
        );
        // Two chained groups with a print between their uses: `x`'s
        // definition loads `y`'s repository, then a module that prints,
        // then `z`'s repository (`y` and `z` print when prepared).
        fixture.write(
            "ext/yz.bzl",
            r#"print("yz.bzl loaded")

def _lib_impl(rctx):
    rctx.file("BUILD.bazel", "")
    rctx.file("lib.bzl", "NAME = %r\n" % rctx.attr.name)

lib_repo = repository_rule(implementation = _lib_impl)

def _y_impl(mctx):
    lib_repo(name = "y_repo")

y = module_extension(implementation = _y_impl)

def _z_impl(mctx):
    lib_repo(name = "z_repo")

z = module_extension(implementation = _z_impl)
"#,
        );
        fixture.write("ext/mid.bzl", "print('between y and z')\nMID = 'mid'\n");
        fixture.write(
            "ext/x.bzl",
            r#"load("@y_repo//:lib.bzl", Y = "NAME")
load("//ext:mid.bzl", "MID")
load("@z_repo//:lib.bzl", Z = "NAME")

def _x_repo_impl(rctx):
    rctx.file("BUILD.bazel", "")
    rctx.file("defs.bzl", "XS = %r\n" % rctx.attr.text)

x_repo = repository_rule(implementation = _x_repo_impl, attrs = {"text": attr.string()})

def _x_impl(mctx):
    x_repo(name = "x_repo", text = Y + MID + Z)

x = module_extension(implementation = _x_impl)
"#,
        );
        fixture.write(
            "xz/BUILD.bazel",
            "load('@x_repo//:defs.bzl', 'XS')\nprint('xz sees ' + XS)\nfilegroup(name = 'xz')\n",
        );
        // Phase 1's packages (docs/EVALUATOR.md "Extensions on the pool"):
        // loaded by the evaluator's pool, on the speculative preparations
        // of `gen` and `user` and on a local repository rule executed
        // ahead (`plain`, no `execute()`), or by its sequential pass
        // (`touch` calls `execute()`; `fails` has no lockfile entry).
        fixture.write(
            "MODULE.bazel",
            &(fs::read_to_string(fixture.path().join("MODULE.bazel")).unwrap()
                + "plain = use_repo_rule(\"//ext:plain.bzl\", \"plain\")\nplain(name = \"plain\")\n"),
        );
        fixture.write(
            "ext/plain.bzl",
            r#"def _plain_impl(rctx):
    rctx.file("BUILD.bazel", "exports_files(['plain.txt'])\n")
    rctx.file("plain.txt", "plain\n")
    rctx.file("defs.bzl", "PLAIN = 'plain:' + %r\n" % rctx.os.name)

plain = repository_rule(implementation = _plain_impl, local = True)
"#,
        );
        let mut many = Vec::new();
        for i in 0..12 {
            let mut text = String::from("load('@gen_data//:defs.bzl', 'VALUE')\n");
            if i % 3 == 0 {
                text.push_str(&format!(
                    "load('@user_repo//:defs.bzl', 'USER')\nprint('many/p{i} sees ' + USER)\n"
                ));
            }
            if i % 4 == 1 {
                text.push_str("load('@plain//:defs.bzl', 'PLAIN')\nprint(PLAIN)\n");
            }
            text.push_str(&format!(
                "print('many/p{i} sees ' + VALUE)\nfilegroup(name = 't', srcs = ['@gen_data//:data.txt'{}], visibility = ['//visibility:public'])\n",
                if i % 4 == 1 { ", '@plain//:plain.txt'" } else { "" }
            ));
            fixture.write(&format!("many/p{i}/BUILD.bazel"), &text);
            many.push(format!("'//many/p{i}:t'"));
        }
        fixture.write(
            "many/BUILD.bazel",
            &format!(
                "filegroup(name = 'all', srcs = [{}], visibility = ['//visibility:public'])\n",
                many.join(", ")
            ),
        );
        // `touch` runs in the target pattern phase (`//touched:t`'s
        // package), before phase 1 loads `gen`'s users.
        fixture.write(
            "touched/BUILD.bazel",
            "load('@touch//:defs.bzl', 'TOUCHED')\nfilegroup(name = 't', srcs = ['//many:all'])\n",
        );
        fixture.write(
            "failmany/BUILD.bazel",
            "filegroup(name = 'all', srcs = ['//many:all', '//failmany/bad:t'])\n",
        );
        fixture.write(
            "failmany/bad/BUILD.bazel",
            "load('@fail_repo//:defs.bzl', 'X')\nfilegroup(name = 't', visibility = ['//visibility:public'])\n",
        );
        fixture.write(
            "chained/BUILD.bazel",
            "load('@touch//:defs.bzl', 'TOUCHED')\nload('@user_repo//:defs.bzl', 'USER')\nprint('chained sees ' + USER)\nfilegroup(name = 'chained')\n",
        );
        fixture
    }

    fn path(&self) -> &Path {
        self.0.path()
    }

    fn write(&self, relative: &str, text: &str) {
        let path = self.path().join(relative);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, text).unwrap();
    }

    /// The hidden lockfile (`<cache>/worktrees/<workspace>/MODULE.bazel.lock`).
    fn hidden_lockfile_path(&self) -> Option<std::path::PathBuf> {
        fs::read_dir(self.path().join("cache/worktrees"))
            .ok()?
            .flatten()
            .map(|entry| entry.path().join("MODULE.bazel.lock"))
            .find(|path| path.is_file())
    }

    fn hidden_lockfile(&self) -> Option<String> {
        fs::read_to_string(self.hidden_lockfile_path()?).ok()
    }

    /// `bazel-rs build --nobuild <patterns>` with `threads` loading threads
    /// and `env`, in `snapshot` mode (`record` dumps the recording).
    fn build(
        &self,
        threads: usize,
        snapshot: &str,
        env: &[(&str, &str)],
        patterns: &[&str],
    ) -> Run {
        self.command("build", threads, snapshot, env, patterns)
    }

    /// `bazel-rs <command> <patterns>` (`build` with `--nobuild`).
    fn command(
        &self,
        command_word: &str,
        threads: usize,
        snapshot: &str,
        env: &[(&str, &str)],
        patterns: &[&str],
    ) -> Run {
        let dump = self.path().join("dump.txt");
        let _ = fs::remove_file(&dump);
        let mut command = Command::new("timeout");
        command
            .args(["--kill-after=5s", "300s", env!("CARGO_BIN_EXE_bazel-rs")])
            .arg("--ignore_all_rc_files")
            .arg("--cache-dir=cache")
            .arg(command_word)
            .arg(format!(
                "--registry=file://{}",
                self.path().join("registry").display()
            ))
            .arg(format!("--loading_phase_threads={threads}"))
            .arg("--lockfile_mode=update")
            .args((command_word == "build").then_some("--nobuild"))
            .args(patterns)
            .current_dir(self.path())
            .env_remove("BAZEL_RS_TIMING_TIMELINE")
            .env_remove("BAZEL_RS_LOADING_JITTER")
            .env_remove("BAZEL_RS_DEBUG_NO_SPECULATION")
            .env_remove("BAZEL_RS_DEBUG_NO_EXTENSION_SPECULATION")
            .env("BAZEL_RS_TIMING", "1")
            .env("BAZEL_RS_DEBUG_EXTENSION_SPECULATION_WAIT", "1")
            .env("BAZEL_RS_SNAPSHOT", snapshot);
        if snapshot == "record" {
            command.env("BAZEL_RS_SNAPSHOT_DUMP", &dump);
        }
        for (key, value) in env {
            command.env(key, value);
        }
        let output = command.explicit_modules().output().unwrap();
        assert_ne!(output.status.code(), Some(124), "timed out: {patterns:?}");
        let text = String::from_utf8_lossy(&output.stderr).into_owned();
        let (timing, stderr): (Vec<&str>, Vec<&str>) = text
            .split_inclusive('\n')
            .partition(|line| line.starts_with("PERF") || line.starts_with("bazel-rs-snapshot:"));
        Run {
            code: output.status.code(),
            stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
            stderr: without_elapsed(&stderr.concat()),
            timing: timing.concat(),
            lockfile: fs::read_to_string(self.path().join("MODULE.bazel.lock")).ok(),
            hidden: self.hidden_lockfile(),
            records: records(&dump),
        }
    }
}

/// The snapshot recording's observations, environment reads and taints, as
/// a set (as tests/module_root_warm.rs compares them).
fn records(dump: &Path) -> Vec<String> {
    let text = fs::read_to_string(dump).unwrap_or_default();
    let mut records: Vec<String> = text
        .lines()
        .filter(|line| {
            (line.starts_with("OBS ") || line.starts_with("ENV ") || line.starts_with("TAINT "))
                && !line.starts_with("TAINT test-hook ")
        })
        .map(|line| {
            line.split(' ')
                .filter(|word| !word.starts_with("key:") && *word != "racy")
                .collect::<Vec<_>>()
                .join(" ")
        })
        .collect();
    records.sort();
    records
}

fn assert_same(reference: &Run, other: &Run, what: &str) {
    assert_eq!(other.code, reference.code, "{what}: exit\n{}", other.stderr);
    assert_eq!(other.stdout, reference.stdout, "{what}: stdout");
    assert_eq!(other.stderr, reference.stderr, "{what}: stderr");
    assert_eq!(other.lockfile, reference.lockfile, "{what}: lockfile");
    assert_eq!(other.hidden, reference.hidden, "{what}: hidden lockfile");
    if other.records != reference.records {
        let only = |a: &[String], b: &[String]| {
            a.iter()
                .filter(|line| !b.contains(line))
                .cloned()
                .collect::<Vec<_>>()
        };
        panic!(
            "{what}: snapshot recording\nonly sequential: {:#?}\nonly this run: {:#?}",
            only(&reference.records, &other.records),
            only(&other.records, &reference.records)
        );
    }
}

/// The `PERF-EXTENSION` counter `name` of `run` (0 without the line).
fn counter(run: &Run, name: &str) -> u64 {
    run.timing
        .lines()
        .find(|line| line.starts_with("PERF-EXTENSION "))
        .and_then(|line| {
            line.split(' ')
                .find_map(|word| word.strip_prefix(&format!("{name}=")))
                .and_then(|value| value.parse().ok())
        })
        .unwrap_or(0)
}

/// Thread counts and worker jitter compared with the sequential engine.
const THREADED: [(usize, &str); 5] = [(2, ""), (8, ""), (32, ""), (8, "20"), (8, "off")];

fn env(jitter: &str) -> Vec<(&'static str, &str)> {
    match jitter {
        "" => Vec::new(),
        "off" => vec![("BAZEL_RS_DEBUG_NO_EXTENSION_SPECULATION", "1")],
        jitter => vec![("BAZEL_RS_LOADING_JITTER", jitter)],
    }
}

/// Every scenario from the same lockfile and input: the lockfile a
/// sequential build of everything that succeeds wrote (`gen`, `user` and
/// `never` reusable), `input.txt` as it read it.
struct Baseline {
    lockfile: String,
    hidden: Option<(std::path::PathBuf, String)>,
}

impl Baseline {
    fn new(fixture: &Fixture) -> Self {
        let first = fixture.build(1, "off", &[], &["//a", "//b", "//c", "//never", "//xz"]);
        assert_eq!(first.code, Some(0), "{}", first.stderr);
        for line in [
            "gen evaluated with input v0",
            "user evaluated",
            "never evaluated",
        ] {
            assert!(first.stderr.contains(line), "{line}:\n{}", first.stderr);
        }
        Self {
            lockfile: first.lockfile.expect("a lockfile"),
            hidden: fixture
                .hidden_lockfile_path()
                .and_then(|path| Some((path.clone(), fs::read_to_string(path).ok()?))),
        }
    }

    fn restore(&self, fixture: &Fixture) {
        fixture.write("MODULE.bazel.lock", &self.lockfile);
        fixture.write("input.txt", "v0\n");
        if let Some((path, text)) = &self.hidden {
            fs::write(path, text).unwrap();
        }
    }

    /// `patterns` at the sequential engine and at every [`THREADED`]
    /// setting from this baseline, compared; the threaded runs (speculation
    /// on) for further checks.
    fn compare(&self, fixture: &Fixture, snapshot: &str, patterns: &[&str]) -> (Run, Vec<Run>) {
        self.restore(fixture);
        let reference = fixture.build(1, snapshot, &[], patterns);
        let mut runs = Vec::new();
        for (threads, jitter) in THREADED {
            self.restore(fixture);
            let run = fixture.build(threads, snapshot, &env(jitter), patterns);
            assert_same(
                &reference,
                &run,
                &format!("{patterns:?} snapshot={snapshot} threads={threads} jitter={jitter}"),
            );
            if jitter != "off" {
                runs.push(run);
            }
        }
        self.restore(fixture);
        (reference, runs)
    }
}

#[test]
fn speculated_extensions_change_nothing_the_build_shows() {
    let fixture = Fixture::new();
    let baseline = Baseline::new(&fixture);
    for snapshot in ["off", "record"] {
        let (reference, runs) = baseline.compare(&fixture, snapshot, &["//a", "//b", "//c"]);
        assert_eq!(reference.code, Some(0), "{}", reference.stderr);
        // The definitions print where the sequential engine loads them,
        // and the fixup warning follows; the reused extensions never run.
        let mut at = 0;
        for line in [
            "extmod helper loaded",
            "extmod ext.bzl loaded with helper",
            "gen_unimported",
            "user.bzl loaded: lib(x)",
            "a sees lib(user)",
            "b sees data v0",
        ] {
            let found = reference.stderr[at..]
                .find(line)
                .unwrap_or_else(|| panic!("{line} after {at}:\n{}", reference.stderr));
            at += found + line.len();
        }
        assert!(
            !reference.stderr.contains("evaluated"),
            "{}",
            reference.stderr
        );
        assert_eq!(counter(&reference, "requested"), 0, "{}", reference.timing);
        for run in &runs {
            // gen and user were prepared ahead and committed, user on gen's
            // speculative preparation; never was prepared and dropped.
            assert_eq!(counter(run, "taken"), 2, "{}", run.timing);
            assert_eq!(counter(run, "discarded"), 0, "{}", run.timing);
            assert_eq!(counter(run, "chained"), 1, "{}", run.timing);
            // never, y and z were prepared and dropped (x is not kept:
            // it prints between y and z).
            assert_eq!(counter(run, "unused"), 3, "{}", run.timing);
        }
    }
}

/// An extension that fails (no lockfile entry: it evaluates at its demand,
/// never ahead) fails the build exactly as the sequential engine does.
#[test]
fn a_failing_extension_fails_as_without_speculation() {
    let fixture = Fixture::new();
    let baseline = Baseline::new(&fixture);
    let (reference, runs) = baseline.compare(&fixture, "off", &["//b", "//failing"]);
    assert_ne!(reference.code, Some(0), "{}", reference.stderr);
    assert!(
        reference
            .stderr
            .contains("the fails extension fails on purpose"),
        "{}",
        reference.stderr
    );
    for run in &runs {
        assert_eq!(counter(run, "taken"), 1, "gen: {}", run.timing);
    }
}

/// `gen` reads `input.txt`, which a local repository rule the sequential
/// engine executes before `gen`'s demand rewrites: the speculation (which
/// read the old input and found the lockfile entry reusable) is dropped
/// at the demand, and `gen` evaluates again exactly as without it.
#[test]
fn an_input_changed_before_the_demand_drops_the_speculation() {
    let fixture = Fixture::new();
    let baseline = Baseline::new(&fixture);
    for snapshot in ["off", "record"] {
        let (reference, runs) = baseline.compare(&fixture, snapshot, &["//changing"]);
        assert_eq!(reference.code, Some(0), "{}", reference.stderr);
        assert!(
            reference.stderr.contains("gen evaluated with input v1")
                && reference.stderr.contains("changing sees data v1"),
            "{}",
            reference.stderr
        );
        for run in &runs {
            assert_eq!(counter(run, "discarded"), 1, "{}", run.timing);
        }
        // `user` used `gen`'s speculative preparation, which the demand
        // drops: `user`'s is dropped too, though its own entry is still
        // reusable (it is reused, on the `gen` evaluated anew).
        let (reference, runs) = baseline.compare(&fixture, snapshot, &["//chained"]);
        assert_eq!(reference.code, Some(0), "{}", reference.stderr);
        assert!(
            reference.stderr.contains("gen evaluated with input v1")
                && !reference.stderr.contains("user evaluated"),
            "{}",
            reference.stderr
        );
        for run in &runs {
            assert_eq!(counter(run, "discarded"), 2, "{}", run.timing);
        }
    }
}

/// `query` resolves the module graph lazily from inside the extension
/// catalog's construction: the speculations start once the catalog exists
/// (never inside its initializer, which they read), and the answer is the
/// sequential engine's.
#[test]
fn query_with_speculation_matches_the_sequential_engine() {
    let fixture = Fixture::new();
    let baseline = Baseline::new(&fixture);
    baseline.restore(&fixture);
    let reference = fixture.command("query", 1, "off", &[], &["deps(//a:all) + //b:all"]);
    assert_eq!(reference.code, Some(0), "{}", reference.stderr);
    for (threads, jitter) in THREADED {
        baseline.restore(&fixture);
        let run = fixture.command(
            "query",
            threads,
            "off",
            &env(jitter),
            &["deps(//a:all) + //b:all"],
        );
        assert_same(
            &reference,
            &run,
            &format!("query threads={threads} jitter={jitter}"),
        );
        if jitter != "off" {
            assert!(counter(&run, "requested") > 0, "{}", run.timing);
        }
    }
}

/// A speculation that wrote output between two groups' speculative
/// preparations it used is never kept: its commit would write it after
/// both groups' output, the sequential engine between them (`x` prints
/// `between y and z` after `y` was prepared and before `z` was).
#[test]
fn output_between_chained_groups_keeps_the_sequential_order() {
    let fixture = Fixture::new();
    let baseline = Baseline::new(&fixture);
    let (reference, runs) = baseline.compare(&fixture, "off", &["//xz"]);
    assert_eq!(reference.code, Some(0), "{}", reference.stderr);
    let y = reference.stderr.find("yz.bzl loaded").expect("y prepared");
    let mid = reference.stderr.find("between y and z").expect("mid");
    let z = reference.stderr[mid..]
        .find("yz.bzl loaded")
        .expect("z prepared after mid");
    assert!(y < mid && z > 0, "{}", reference.stderr);
    for run in &runs {
        // y and z were committed (each its own speculation), x was not.
        assert_eq!(counter(run, "taken"), 2, "{}", run.timing);
        assert_eq!(counter(run, "chained"), 0, "{}", run.timing);
    }
}

/// `BAZEL_RS_ANALYSIS=eval` at 1, 2, 8 and 32 threads, with evaluator
/// jitter and with attempts abandoned at random, against the walk at one
/// thread (the sequential engine), each from `baseline`.
fn compare_eval(fixture: &Fixture, baseline: &Baseline, snapshot: &str, patterns: &[&str]) -> Run {
    baseline.restore(fixture);
    let reference = fixture.build(1, snapshot, &[("BAZEL_RS_ANALYSIS", "walk")], patterns);
    for (threads, extra) in [
        (1, None),
        (2, None),
        (8, None),
        (32, None),
        (8, Some(("BAZEL_RS_EVAL_JITTER", "300"))),
        (32, Some(("BAZEL_RS_EVAL_JITTER", "100"))),
        (8, Some(("BAZEL_RS_EVAL_RESTARTS", "5"))),
    ] {
        baseline.restore(fixture);
        let mut env = vec![("BAZEL_RS_ANALYSIS", "eval")];
        env.extend(extra);
        let run = fixture.build(threads, snapshot, &env, patterns);
        assert_same(
            &reference,
            &run,
            &format!("eval {patterns:?} snapshot={snapshot} threads={threads} {extra:?}"),
        );
    }
    baseline.restore(fixture);
    reference
}

/// The counter `name` of the evaluator's statistics line on the pool's use
/// of speculative extension preparations (`BAZEL_RS_EVAL_STATS=1`).
fn pool_counter(fixture: &Fixture, baseline: &Baseline, patterns: &[&str], name: &str) -> u64 {
    baseline.restore(fixture);
    let run = fixture.build(
        8,
        "off",
        &[("BAZEL_RS_ANALYSIS", "eval"), ("BAZEL_RS_EVAL_STATS", "1")],
        patterns,
    );
    baseline.restore(fixture);
    let line = run
        .stderr
        .lines()
        .find(|line| line.contains("speculative extension preparations on the pool"))
        .unwrap_or_else(|| panic!("no statistics:\n{}", run.stderr))
        .to_owned();
    line.split(", ")
        .find_map(|part| {
            part.rsplit_once(' ')
                .filter(|(label, _)| label.ends_with(name))
                .and_then(|(_, value)| value.parse().ok())
        })
        .unwrap_or_else(|| panic!("{name} in {line}"))
}

/// Phase 1's packages loading the repositories of `gen` and `user` (both
/// prepared ahead, `user` on `gen`'s speculative preparation) and of a
/// local repository rule: the evaluator's pool loads them on the
/// speculative preparations, committed where the walk prepares the groups
/// (docs/EVALUATOR.md "Extensions on the pool"); stdout, stderr, the
/// lockfiles and the snapshot recording are the walk's at every thread
/// count.
#[test]
fn the_evaluators_pool_loads_packages_on_speculated_extensions() {
    let fixture = Fixture::new();
    let baseline = Baseline::new(&fixture);
    for snapshot in ["off", "record"] {
        let reference = compare_eval(&fixture, &baseline, snapshot, &["//many:all"]);
        assert_eq!(reference.code, Some(0), "{}", reference.stderr);
        assert!(
            reference.stderr.contains("many/p11 sees data v0")
                && reference.stderr.contains("many/p9 sees lib(user)")
                && reference.stderr.contains("plain:")
                && !reference.stderr.contains("evaluated"),
            "{}",
            reference.stderr
        );
    }
    assert!(pool_counter(&fixture, &baseline, &["//many:all"], "used") > 0);
    assert!(
        pool_counter(
            &fixture,
            &baseline,
            &["//many:all"],
            "committed by the tape"
        ) > 0
    );
}

/// `touch` (a local rule) rewrites `gen`'s input in the target pattern
/// phase, after `gen` was prepared ahead: the pool's check of the
/// speculation's lockfile entry fails, so phase 1's packages wait for the
/// sequential pass, which evaluates `gen` anew where the walk does.
#[test]
fn the_pool_does_not_use_an_extension_whose_input_changed() {
    let fixture = Fixture::new();
    let baseline = Baseline::new(&fixture);
    let reference = compare_eval(&fixture, &baseline, "off", &["//touched:t"]);
    assert_eq!(reference.code, Some(0), "{}", reference.stderr);
    assert!(
        reference.stderr.contains("gen evaluated with input v1")
            && reference.stderr.contains("many/p0 sees data v1"),
        "{}",
        reference.stderr
    );
}

/// An extension without a lockfile entry that fails (never prepared
/// ahead): its user waits for the sequential pass, which fails as the walk
/// does, beside packages the pool loaded on `gen`'s speculation.
#[test]
fn a_failing_extension_fails_on_the_pool_as_in_the_walk() {
    let fixture = Fixture::new();
    let baseline = Baseline::new(&fixture);
    let reference = compare_eval(&fixture, &baseline, "off", &["//failmany:all"]);
    assert_ne!(reference.code, Some(0), "{}", reference.stderr);
    assert!(
        reference
            .stderr
            .contains("the fails extension fails on purpose"),
        "{}",
        reference.stderr
    );
}

/// A loopback HTTP server serving `body` for every request and recording
/// the paths requested.
struct ArchiveServer {
    url: String,
    paths: std::sync::Arc<std::sync::Mutex<Vec<String>>>,
    stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
    worker: Option<std::thread::JoinHandle<()>>,
}

impl ArchiveServer {
    fn new(body: Vec<u8>) -> Self {
        use std::io::{Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let paths = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let (logged, stopped) = (paths.clone(), stop.clone());
        let worker = std::thread::spawn(move || {
            while !stopped.load(std::sync::atomic::Ordering::SeqCst) {
                let mut stream = match listener.accept() {
                    Ok((stream, _)) => stream,
                    Err(_) => {
                        std::thread::sleep(std::time::Duration::from_millis(2));
                        continue;
                    }
                };
                stream.set_nonblocking(false).unwrap();
                let mut request = Vec::new();
                let mut byte = [0];
                while !request.ends_with(b"\r\n\r\n") {
                    if stream.read(&mut byte).unwrap_or(0) == 0 {
                        break;
                    }
                    request.push(byte[0]);
                }
                let request = String::from_utf8_lossy(&request).into_owned();
                let path = request
                    .split_whitespace()
                    .nth(1)
                    .unwrap_or_default()
                    .to_owned();
                logged.lock().unwrap().push(path);
                let header = format!(
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                );
                let _ = stream.write_all(header.as_bytes());
                let _ = stream.write_all(&body);
            }
        });
        Self {
            url,
            paths,
            stop,
            worker: Some(worker),
        }
    }
}

impl Drop for ArchiveServer {
    fn drop(&mut self) {
        self.stop.store(true, std::sync::atomic::Ordering::SeqCst);
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

/// On a cold cache a speculation never downloads a registry module's
/// archive: `farmod`'s extension is the root module's, but the command
/// never demands it, and its archive (served over HTTP) is never
/// requested — the speculation reads registry roots offline only.
#[test]
fn a_speculation_never_downloads_registry_archives() {
    let module = "module(name = 'farmod', version = '1')\n";
    let bytes = archive(
        "farmod",
        &[
            ("MODULE.bazel", module),
            ("BUILD.bazel", ""),
            (
                "ext.bzl",
                "def _impl(mctx):\n    pass\n\nfar = module_extension(implementation = _impl)\n",
            ),
        ],
    );
    let server = ArchiveServer::new(bytes.clone());
    let fixture = Fixture(
        tempfile::Builder::new()
            .prefix("extension-speculation-offline-")
            .tempdir_in(crate::test_temp::root())
            .unwrap(),
    );
    fixture.write("registry/modules/farmod/1/MODULE.bazel", module);
    let source = serde_json::json!({
        "url": format!("{}/farmod-1.tar.gz", server.url),
        "integrity": format!(
            "sha256-{}",
            base64::engine::general_purpose::STANDARD.encode(Sha256::digest(&bytes))
        ),
        "strip_prefix": "farmod-1",
    });
    fixture.write("registry/modules/farmod/1/source.json", &source.to_string());
    fixture.write(
        "MODULE.bazel",
        "module(name = 'main')\nbazel_dep(name = 'farmod', version = '1')\nfar = use_extension('@farmod//:ext.bzl', 'far')\n",
    );
    fixture.write("BUILD.bazel", "filegroup(name = 'c')\n");
    let cold = |threads: usize| {
        let _ = fs::remove_dir_all(fixture.path().join("cache"));
        let _ = fs::remove_file(fixture.path().join("MODULE.bazel.lock"));
        fixture.build(threads, "off", &[], &["//:c"])
    };
    let reference = cold(1);
    assert_eq!(reference.code, Some(0), "{}", reference.stderr);
    for threads in [8, 32] {
        let run = cold(threads);
        assert_same(&reference, &run, &format!("cold threads={threads}"));
        assert!(counter(&run, "requested") > 0, "{}", run.timing);
    }
    assert!(
        server.paths.lock().unwrap().is_empty(),
        "{:?}",
        server.paths.lock().unwrap()
    );
}
