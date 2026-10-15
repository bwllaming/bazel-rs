//! CLI coverage for load-time rule declarations (docs/LOAD_DECLARATIONS.md):
//! `query` loads a fixture module mirroring rules_go, rules_cc, protobuf,
//! rules_java and rules_python top-level declaration patterns; an ordinary
//! rule still analyzes and builds, and exercised-but-unanalyzed features fail
//! explicitly before any action runs.

use std::fs;
use std::path::Path;
use std::process::{Command, Output};

struct Workspace(tempfile::TempDir);

fn copy_tree(from: &Path, to: &Path) {
    fs::create_dir_all(to).unwrap();
    for entry in fs::read_dir(from).unwrap() {
        let entry = entry.unwrap();
        let target = to.join(entry.file_name());
        if entry.file_type().unwrap().is_dir() {
            copy_tree(&entry.path(), &target);
        } else {
            fs::copy(entry.path(), target).unwrap();
        }
    }
}

impl Workspace {
    fn fixture() -> Self {
        let ws = Self(
            tempfile::Builder::new()
                .prefix("load-decls-cli-")
                .tempdir_in("/tmp/opencode")
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
            .args(["--kill-after=5s", "60s", env!("CARGO_BIN_EXE_bazel-rs")])
            .args(["--ignore_all_rc_files", "--cache-dir=private-cache"])
            .args(args)
            .current_dir(self.path())
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
    for (target, diagnostics) in [
        (
            "//:golib",
            &[
                "analysis of //:golib",
                "accepted at load time but not analyzed by the native profile",
                "rule(toolchains=[//rules:go_toolchain_type]) (toolchain resolution)",
            ][..],
        ),
        (
            "//:goselect",
            &["attribute 'srcs' select() (configured attribute resolution)"][..],
        ),
        (
            "//:go_default_test",
            &[
                "rule(test=True) (test rule semantics)",
                "(configuration transition)",
            ][..],
        ),
        (
            "//:pybin",
            &[
                "rule(executable=True) (executable rule semantics)",
                "attribute 'srcs' allowed file types [.py]",
            ][..],
        ),
        (
            "//:ccbin",
            &["rule(outputs={...}) implicit outputs in ctx.outputs"][..],
        ),
        (
            "//:cc_unit_test",
            &["attribute '_lcov_merger' executable=True"][..],
        ),
        (
            "//:cc_proto",
            &[
                "rule advertised the 'CcLikeInfo' provider, but this provider was not among those returned",
            ][..],
        ),
    ] {
        let build = ws.run(&["build", target]);
        rejected(&build, diagnostics);
    }
    assert!(!ws.path().join("bazel-rs-out").exists());
}
