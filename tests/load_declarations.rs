//! CLI coverage for load-time rule declarations (docs/LOAD_DECLARATIONS.md):
//! `query` loads a fixture module mirroring rules_go, rules_cc, protobuf,
//! rules_java and rules_python top-level declaration patterns; an ordinary
//! rule still analyzes and builds, and exercised-but-unanalyzed features fail
//! explicitly before any action runs.
#![allow(clippy::disallowed_methods)] // test harness: starts processes directly (clippy.toml)

include!("support/executable_script.rs");

use std::fs;
use std::path::Path;
use std::process::{Command, Output};
include!("support/explicit_modules.rs");

struct Workspace(tempfile::TempDir);

fn copy_tree(from: &Path, to: &Path) {
    fs::create_dir_all(to).unwrap();
    for entry in fs::read_dir(from).unwrap() {
        let entry = entry.unwrap();
        let target = to.join(entry.file_name());
        if entry.file_type().unwrap().is_dir() {
            copy_tree(&entry.path(), &target);
        } else {
            // An executable fixture is written by a separate process: no
            // descriptor of it in this (spawning) test process (ETXTBSY).
            copy_file(&entry.path(), &target);
        }
    }
}

impl Workspace {
    fn fixture() -> Self {
        let ws = Self(
            tempfile::Builder::new()
                .prefix("load-decls-cli-")
                .tempdir_in(crate::test_temp::root())
                .unwrap(),
        );
        copy_tree(
            &Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/load_declarations"),
            ws.path(),
        );
        ws
    }

    fn path(&self) -> &Path {
        self.0.path()
    }

    fn run(&self, args: &[&str]) -> Output {
        let output = Command::new("timeout")
            .args(["--kill-after=5s", "600s", env!("CARGO_BIN_EXE_bazel-rs")])
            .args(["--ignore_all_rc_files", "--cache-dir=private-cache"])
            .args(args)
            .current_dir(self.path())
            .explicit_modules()
            .output()
            .unwrap();
        assert_ne!(output.status.code(), Some(124), "CLI timed out: {args:?}");
        assert_ne!(output.status.code(), Some(137), "CLI killed: {args:?}");
        output
    }
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn errors(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

fn rejected(output: &Output, diagnostics: &[&str]) {
    assert!(!output.status.success(), "unexpected success");
    assert!(output.stdout.is_empty(), "partial output on failure");
    for diagnostic in diagnostics {
        assert!(errors(output).contains(diagnostic), "{}", errors(output));
    }
    assert!(!errors(output).contains("panicked"), "{}", errors(output));
}

#[test]
fn query_loads_ruleset_shaped_declarations() {
    let ws = Workspace::fixture();
    let query = ws.run(&["query", "//:all"]);
    assert!(query.status.success(), "{}", errors(&query));
    assert_eq!(
        stdout(&query),
        "//:cc_proto\n//:cc_unit_test\n//:ccbin\n//:go_default_test\n//:gobin\n//:golib\n//:goselect\n//:jars\n//:pybin\n//:simple\n"
    );
    // rule(outputs={...}) implicit outputs and attr.output files are package targets.
    for target in ["//:ccbin.dwp", "//:ccbin.stripped", "//:jars.out"] {
        let query = ws.run(&["query", target]);
        assert!(query.status.success(), "{target}: {}", errors(&query));
        assert_eq!(stdout(&query), format!("{target}\n"));
    }
    let query = ws.run(&["query", "//rules:all"]);
    assert!(query.status.success(), "{}", errors(&query));
    assert!(!ws.path().join("bazel-rs-out").exists());
}

#[test]
fn ordinary_rule_still_analyzes_and_builds() {
    let ws = Workspace::fixture();
    let build = ws.run(&["build", "//:simple"]);
    assert!(build.status.success(), "{}", errors(&build));
    let line = stdout(&build);
    let (label, path) = line.trim_end().split_once('\t').unwrap();
    assert_eq!(label, "//:simple");
    assert_eq!(fs::read_to_string(path).unwrap(), "hello:b");
    assert!(
        errors(&build).contains("1 actions executed"),
        "{}",
        errors(&build)
    );
}

#[test]
fn exercised_declarations_fail_explicitly_at_analysis() {
    let ws = Workspace::fixture();
    // Toolchain resolution (docs/TOOLCHAINS.md) needs platforms: the default
    // host platform needs the built-in profile, so the builds use an explicit
    // one.
    fs::create_dir_all(ws.path().join("plat")).unwrap();
    fs::write(ws.path().join("plat/BUILD.bazel"), "platform(name = 'p')\n").unwrap();
    rejected(
        &ws.run(&["build", "//:golib"]),
        &[
            "While resolving toolchains for target //:golib (",
            "needs the built-in @bazel_tools repository",
        ],
    );
    for (target, diagnostics) in [
        // The fixture's toolchain types are not targets
        // (ToolchainTypeLookupUtil.java:139-144).
        (
            "//:golib",
            &[
                "While resolving toolchains for target //:golib (",
                "): com.google.devtools.build.lib.packages.NoSuchTargetException: no such target '//rules:go_toolchain_type': target 'go_toolchain_type' not declared in package 'rules' defined by ",
            ][..],
        ),
        // select() conditions are analyzed before toolchain resolution
        // (docs/CONFIGURATION.md): the fixture's //rules:linux key is not a
        // target (Package.getTarget, Package.java:335-362).
        (
            "//:goselect",
            &[
                "no such target '//rules:linux': target 'linux' not declared in package 'rules' defined by ",
            ][..],
        ),
        // A rule transition is applied first: it decides the configuration
        // the rest is analyzed in (docs/CONFIGURATION.md, N18c). Both match
        // the genuine Bazel 9.3 capture (reference_receipt.json: the
        // transition's settings loading error at the rule, 14:8, and the
        // invalid native output at the transition() call, 65:39).
        (
            "//:go_default_test",
            &[
                "BUILD.bazel:14:8: com.google.devtools.build.lib.packages.NoSuchTargetException: no such target '//rules:pure': target 'pure' not declared in package 'rules' defined by ",
            ][..],
        ),
        (
            "//:pybin",
            &[
                "rules/proto_like.bzl:65:39: transition outputs [//command_line_option:python_version] do not correspond to valid settings\n",
                "BUILD.bazel:30:15: Errors encountered while applying Starlark transition",
            ][..],
        ),
        (
            "//:ccbin",
            &["no such target '//rules:cc_toolchain_type'"][..],
        ),
        // Execution platform resolution (the rule's exec_compatible_with,
        // ConstraintValueLookupUtil.java:62-92) precedes attribute-level
        // gaps; the rule-class gap (test=True) follows attribute validation.
        (
            "//:cc_unit_test",
            &[
                "While resolving toolchains for target //:cc_unit_test (",
                "): com.google.devtools.build.lib.packages.NoSuchTargetException: no such target '//rules:linux'",
            ][..],
        ),
        (
            "//:cc_proto",
            &[
                "rule advertised the 'CcLikeInfo' provider, but this provider was not among those returned",
            ][..],
        ),
    ] {
        let build = ws.run(&["build", "--host_platform=//plat:p", target]);
        rejected(&build, diagnostics);
    }
    assert!(!ws.path().join("bazel-rs-out").exists());
}

/// `fs::copy`, except that a file with any execute bit is re-created through
/// `write_executable_script` (bytes and mode kept).
fn copy_file(from: &Path, to: &Path) {
    use std::os::unix::fs::PermissionsExt;
    let mode = fs::metadata(from).unwrap().permissions().mode() & 0o7777;
    if mode & 0o111 != 0 {
        write_executable_script(to, fs::read(from).unwrap(), mode);
    } else {
        fs::copy(from, to).unwrap();
    }
}
