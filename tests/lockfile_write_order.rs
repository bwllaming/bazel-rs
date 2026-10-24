//! MODULE.bazel.lock is written only after the whole module resolution — the
//! dep graph and every module extension the command demanded — succeeded
//! (Bazel 9.3 BazelLockFileModule.java:63-100 skips the write when a
//! resolution value is missing; :105-141 collects the SingleExtensionValues
//! "evaluated in the current build"; docs/REGISTRY_LOCKFILE.md). A command
//! that fails while loading — extension evaluation included — writes
//! nothing. `--lockfile_mode=off` and `error` keep their lazy profiles.
#![allow(clippy::disallowed_methods)] // test harness: starts processes directly (clippy.toml)

use std::fs;
use std::process::{Command, Output};

const RULES: &str = "def _impl(ctx):\n    ctx.file('BUILD.bazel', \"exports_files(['data.txt', 'defs.bzl'])\\n\")\n    ctx.file('data.txt', 'hello ' + ctx.attr.text + '\\n', executable=False)\n    ctx.file('defs.bzl', 'x = 1\\n', executable=False)\nmaker=repository_rule(_impl,attrs={'text':attr.string()})\n";
const GOOD_EXT: &str = "load(':rules.bzl','maker')\ndef _impl(ctx):\n    maker(name='made',text='Ada')\nfiles=module_extension(_impl)\n";
const FAILING_EXT: &str = "load(':rules.bzl','maker')\ndef _impl(ctx):\n    fail('EVAL_BOOM')\nfiles=module_extension(_impl)\n";
const MODULE: &str = "module(name='lockorder',version='1.0')\ne=use_extension('//:ext.bzl','files')\nuse_repo(e,chosen='made')\n";
/// A package that loads from the generated repository: it demands the
/// extension's evaluation, like the workspace that exposed the bug.
const DEMANDING: &str = "load('@chosen//:defs.bzl', 'x')\nfilegroup(name='a')\n";

struct Fixture(tempfile::TempDir);
impl Fixture {
    fn new(ext: &str) -> Self {
        let f = Self(
            tempfile::Builder::new()
                .prefix("lockfile-write-order-")
                .tempdir_in("/tmp/opencode")
                .unwrap(),
        );
        f.file("MODULE.bazel", MODULE);
        f.file("BUILD.bazel", "filegroup(name='files')\n");
        f.file("a/BUILD.bazel", "filegroup(name='a')\n");
        f.file("rules.bzl", RULES);
        f.file("ext.bzl", ext);
        f
    }
    fn file(&self, name: &str, content: impl AsRef<[u8]>) {
        let path = self.0.path().join(name);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, content).unwrap();
    }
    fn run(&self, args: &[&str]) -> Output {
        Command::new("timeout")
            .args([
                "30s",
                env!("CARGO_BIN_EXE_bazel-rs"),
                "--ignore_all_rc_files",
                "--cache-dir=cache",
            ])
            .args(args)
            .current_dir(self.0.path())
            .output()
            .unwrap()
    }
    fn query(&self, mode: &str) -> Output {
        self.run(&["query", &format!("--lockfile_mode={mode}"), "//..."])
    }
    fn lock(&self) -> Option<String> {
        fs::read_to_string(self.0.path().join("MODULE.bazel.lock")).ok()
    }
}

fn stderr(result: &Output) -> String {
    String::from_utf8_lossy(&result.stderr).into_owned()
}

const MARKER: &str = "bazel-rs: resolved 1 module extension(s) for MODULE.bazel.lock";

#[test]
fn failing_extension_in_update_writes_no_lockfile_and_fails() {
    let f = Fixture::new(FAILING_EXT);
    f.file("a/BUILD.bazel", DEMANDING);
    let result = f.query("update");
    assert!(!result.status.success());
    assert!(!stderr(&result).contains("panicked"), "{}", stderr(&result));
    assert!(stderr(&result).contains("EVAL_BOOM"), "{}", stderr(&result));
    assert!(
        !stderr(&result).contains("module extension(s) for MODULE.bazel.lock"),
        "{}",
        stderr(&result)
    );
    assert!(result.stdout.is_empty());
    // Nothing is written: no lockfile appears.
    assert!(f.lock().is_none());
    // And an existing lockfile is left byte-identical, never rewritten.
    let seeded = "{\n  \"lockFileVersion\": 28,\n  \"registryFileHashes\": {},\n  \"selectedYankedVersions\": {},\n  \"moduleExtensions\": {},\n  \"facts\": {},\n  \"factsVersions\": {}\n}\n";
    f.file("MODULE.bazel.lock", seeded);
    let result = f.query("update");
    assert!(!result.status.success());
    assert_eq!(f.lock().unwrap(), seeded);
    // Refresh fails the same way: writing modes gate on the same resolution.
    let result = f.query("refresh");
    assert!(!result.status.success());
    assert!(stderr(&result).contains("EVAL_BOOM"), "{}", stderr(&result));
    assert_eq!(f.lock().unwrap(), seeded);
}

#[test]
fn successful_update_evaluates_all_demanded_extensions_then_writes() {
    let f = Fixture::new(GOOD_EXT);
    f.file("a/BUILD.bazel", DEMANDING);
    // Content that a local-only resolution drops: a successful update
    // rewrites the file; since N52 the extension's moduleExtensions entry
    // is computed and written with Bazel's digests.
    f.file(
        "MODULE.bazel.lock",
        "{\n  \"lockFileVersion\": 28,\n  \"registryFileHashes\": {\n    \"https://stale/a\": \"not found\"\n  }\n}\n",
    );
    let result = f.query("update");
    assert!(result.status.success(), "{}", stderr(&result));
    // The diagnostic prints exactly once, and only because a write happened.
    assert_eq!(
        stderr(&result).matches(MARKER).count(),
        1,
        "{}",
        stderr(&result)
    );
    let lock = f.lock().unwrap();
    assert!(
        lock.starts_with("{\n  \"lockFileVersion\": 28,\n"),
        "{lock}"
    );
    assert!(lock.contains("\"moduleExtensions\": {"), "{lock}");
    assert!(lock.contains("\"//:ext.bzl%files\": {"), "{lock}");
    assert!(lock.contains("\"bzlTransitiveDigest\": "), "{lock}");
    assert!(lock.contains("\"usagesDigest\": "), "{lock}");
    assert!(lock.contains("\"generatedRepoSpecs\": {"), "{lock}");
    assert!(!lock.contains("stale"), "{lock}");
    // Write-only-if-changed: an unchanged value is not rewritten and the
    // diagnostic stays silent, although the extension counts as resolved
    // (an up-to-date entry is reused, never re-evaluated).
    let warm = f.query("update");
    assert!(warm.status.success(), "{}", stderr(&warm));
    assert!(!stderr(&warm).contains(MARKER), "{}", stderr(&warm));
    assert_eq!(f.lock().unwrap(), lock);
}

#[test]
fn off_mode_stays_lazy() {
    let f = Fixture::new(FAILING_EXT);
    // Nothing demands the extension: off mode never evaluates it.
    let result = f.query("off");
    assert!(result.status.success(), "{}", stderr(&result));
    assert!(
        String::from_utf8_lossy(&result.stdout).contains("//:files"),
        "{}",
        String::from_utf8_lossy(&result.stdout)
    );
    assert!(
        !stderr(&result).contains("module extension(s) for MODULE.bazel.lock"),
        "{}",
        stderr(&result)
    );
    assert!(f.lock().is_none());
}

#[test]
fn error_mode_keeps_its_on_demand_validation() {
    // Undemanded: error mode stays lazy and does not write.
    let f = Fixture::new(GOOD_EXT);
    let result = f.query("error");
    assert!(result.status.success(), "{}", stderr(&result));
    assert!(
        !stderr(&result).contains("module extension(s) for MODULE.bazel.lock"),
        "{}",
        stderr(&result)
    );
    assert!(f.lock().is_none());
    // Demanded: the pre-existing SingleExtensionEvalFunction.java:258-286
    // error for a non-reproducible extension without a lockfile entry.
    f.file("a/BUILD.bazel", DEMANDING);
    let result = f.query("error");
    assert!(!result.status.success());
    assert!(
        stderr(&result).contains("does not exist in the lockfile"),
        "{}",
        stderr(&result)
    );
    assert!(f.lock().is_none());
}

const SCRIPT_RULES: &str = r#"def _exe(ctx):
    out = ctx.actions.declare_file(ctx.label.name + ".sh")
    ctx.actions.write(out, ctx.attr.script, is_executable = True)
    return [DefaultInfo(executable = out, runfiles = ctx.runfiles(files = ctx.files.data))]

_ATTRS = {"script": attr.string(), "data": attr.label_list(allow_files = True)}

script_test = rule(implementation = _exe, test = True, attrs = _ATTRS)
script_binary = rule(implementation = _exe, executable = True, attrs = _ATTRS)
"#;

const SCRIPT_BUILD: &str = r##"load("//:script.bzl", "script_binary", "script_test")

script_test(name = "pass_test", size = "small", data = ["@chosen//:data.txt"], script = "#!/bin/sh\necho test-ran\n")

script_test(name = "fail_test", size = "small", data = ["@chosen//:data.txt"], script = "#!/bin/sh\nexit 1\n")

script_binary(name = "tool", data = ["@chosen//:data.txt"], script = "#!/bin/sh\necho tool-ran\n")

filegroup(name = "broken", srcs = ["//missing:x", "@chosen//:data.txt"])
"##;

/// BazelLockFileModule.afterCommand runs after every command (genuine
/// Bazel 9.3: `test` passing, failing and `--nobuild`, `run`, a `query`
/// with exit code 3 all write MODULE.bazel.lock). The native commands that
/// end the process themselves — `test` exits with its code, `run` replaces
/// the process, a failing `query` exits — write it before they do.
#[test]
fn commands_that_end_the_process_write_the_lockfile_first() {
    let f = Fixture::new(GOOD_EXT);
    f.file("script.bzl", SCRIPT_RULES);
    f.file("BUILD.bazel", SCRIPT_BUILD);
    let cases: &[(&[&str], i32)] = &[
        (&["test", "//:pass_test"], 0),
        (&["test", "//:fail_test"], 3),
        (&["test", "--nobuild", "//:pass_test"], 1),
        (&["run", "//:tool"], 0),
        (&["query", "--keep_going", "deps(//:broken)"], 3),
    ];
    for (args, code) in cases {
        let _ = fs::remove_file(f.0.path().join("MODULE.bazel.lock"));
        let result = Command::new("timeout")
            .args([
                "120s",
                env!("CARGO_BIN_EXE_bazel-rs"),
                "--ignore_all_rc_files",
                "--cache-dir=cache",
            ])
            .args(*args)
            .current_dir(f.0.path())
            .env("HOME", f.0.path())
            .output()
            .unwrap();
        assert_eq!(
            result.status.code(),
            Some(*code),
            "{args:?}: {}",
            stderr(&result)
        );
        let lock = f
            .lock()
            .unwrap_or_else(|| panic!("{args:?} wrote no lockfile: {}", stderr(&result)));
        assert!(lock.contains("\"//:ext.bzl%files\": {"), "{args:?}: {lock}");
        if args[0] == "run" {
            assert_eq!(String::from_utf8_lossy(&result.stdout), "tool-ran\n");
        }
    }
}
