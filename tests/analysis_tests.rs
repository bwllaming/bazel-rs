//! Analysis tests through the CLI (docs/ANALYSIS_TESTS.md): attribute
//! `analysis_test_transition`s, `testing.analysis_test`, `Target.actions`
//! (Action values read through an aspect, as bazel_skylib's `analysistest`
//! and rules_testing do), `--allow_analysis_failures` with
//! `AnalysisFailureInfo`, and the restrictions genuine Bazel 9.3.1
//! enforces. The expected texts are genuine 9.3.1's
//! (tests/fixtures/analysis_tests/reference_receipt.json).
#![allow(clippy::disallowed_methods)] // test harness: starts processes directly (clippy.toml)
use std::fs;
use std::path::Path;
use std::process::{Command, Output};
include!("support/explicit_modules.rs");
include!("support/module_archives.rs");

const FIXTURE: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/analysis_tests");

struct Fixture(tempfile::TempDir);

fn copy_tree(from: &Path, to: &Path) {
    fs::create_dir_all(to).unwrap();
    for entry in fs::read_dir(from).unwrap() {
        let entry = entry.unwrap();
        let target = to.join(entry.file_name());
        if entry.file_type().unwrap().is_dir() {
            copy_tree(&entry.path(), &target);
        } else {
            fs::copy(entry.path(), &target).unwrap();
        }
    }
}

impl Fixture {
    fn new() -> Self {
        let f = Self(
            tempfile::Builder::new()
                .prefix("analysis-tests-")
                .tempdir_in(crate::test_temp::root())
                .unwrap(),
        );
        fs::create_dir(f.0.path().join("ws")).unwrap();
        f
    }

    fn workspace(&self) -> std::path::PathBuf {
        self.0.path().join("ws")
    }

    fn file(&self, name: &str, text: &str) {
        let path = self.workspace().join(name);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, text).unwrap();
    }

    /// The genuine built-in profile with the captured offline registry.
    fn run(&self, command: &str, args: &[&str]) -> Output {
        let registry = format!(
            "--registry=file://{}",
            Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("tests/fixtures/builtin_modules/registry")
                .display()
        );
        let cache = self.0.path().join("cache");
        seed_module_archives(&cache, TOOLCHAIN_CLOSURE_ARCHIVES);
        let cache = format!("--cache-dir={}", cache.display());
        Command::new("timeout")
            .args([
                "600s",
                env!("CARGO_BIN_EXE_bazel-rs"),
                "--ignore_all_rc_files",
                cache.as_str(),
                command,
                "--experimental-builtin-modules",
                registry.as_str(),
            ])
            .args(args)
            .current_dir(self.workspace())
            .env("BAZEL_RS_PROGRESS", "0")
            .output()
            .unwrap()
    }
}

/// The `DEBUG:` blocks the analysis-test implementations print, the
/// workspace path replaced by `<WS>` (as in the genuine capture).
fn debug_blocks(stderr: &str, workspace: &Path) -> Vec<String> {
    let text = stderr.replace(&workspace.display().to_string(), "<WS>");
    let mut blocks: Vec<String> = Vec::new();
    let mut current: Option<String> = None;
    for line in text.lines() {
        let starts = [
            "DEBUG:",
            "INFO:",
            "ERROR:",
            "WARNING:",
            "FAIL:",
            "bazel-rs:",
        ]
        .iter()
        .any(|prefix| line.starts_with(prefix));
        if starts {
            blocks.extend(current.take());
            if line.starts_with("DEBUG:")
                && (line.contains(": CHECK ") || line.contains(": ANALYSIS_TEST "))
            {
                current = Some(line.to_owned());
            }
        } else if let Some(block) = &mut current {
            block.push('\n');
            block.push_str(line);
        }
    }
    blocks.extend(current);
    blocks.sort();
    blocks
}

/// Each test's final status line (`//pkg:x PASSED` / `FAILED`), sorted.
fn statuses(stdout: &str) -> Vec<String> {
    let mut lines: Vec<String> = stdout
        .lines()
        .filter(|line| {
            line.starts_with("//pkg:") && (line.contains(" PASSED") || line.contains(" FAILED"))
        })
        .map(|line| {
            let words: Vec<&str> = line.split_whitespace().collect();
            let status = words
                .iter()
                .find(|word| **word == "PASSED" || **word == "FAILED")
                .unwrap();
            format!("{} {status}", words[0])
        })
        .collect();
    lines.sort();
    lines
}

#[test]
fn analysis_tests_like_bazel() {
    let f = Fixture::new();
    copy_tree(&Path::new(FIXTURE).join("workspace"), &f.workspace());
    let output = f.run("test", &["--keep_going", "--test_output=all", "//pkg:all"]);
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    // One analysis test fails (its AnalysisTestResultInfo says so): exit 3.
    assert_eq!(output.status.code(), Some(3), "{stderr}\n{stdout}");
    let genuine = fs::read_to_string(Path::new(FIXTURE).join("genuine/test.debug.txt")).unwrap();
    let mut expected = debug_blocks(&genuine, Path::new("<WS>"));
    expected.sort();
    assert_eq!(debug_blocks(&stderr, &f.workspace()), expected, "{stderr}");
    let summary = fs::read_to_string(Path::new(FIXTURE).join("genuine/test.summary.txt")).unwrap();
    let mut expected: Vec<String> = summary.lines().map(str::to_owned).collect();
    expected.sort();
    assert_eq!(statuses(&stdout), expected, "{stdout}");
}

#[test]
fn analysis_test_restrictions_like_bazel() {
    let f = Fixture::new();
    f.file("MODULE.bazel", "module(name = \"fx2\")\n");
    f.file("BUILD", "");
    f.file(
        "defs.bzl",
        r#"def _impl(ctx):
    return [AnalysisTestResultInfo(success = True, message = "")]

def _subject(ctx):
    return []

subject = rule(_subject)

check_test = rule(_impl, analysis_test = True, attrs = {
    "target_under_test": attr.label(cfg = analysis_test_transition(settings = {"//command_line_option:compilation_mode": "opt"})),
})

def make(name, **kwargs):
    testing.analysis_test(name = name, implementation = _impl, **kwargs)
"#,
    );
    f.file(
        "nested/BUILD",
        r#"load("//:defs.bzl", "check_test", "subject")
subject(name = "s")
check_test(name = "inner_test", target_under_test = ":s")
check_test(name = "outer_test", target_under_test = ":inner_test")
"#,
    );
    f.file(
        "badname/BUILD",
        "load(\"//:defs.bzl\", \"make\")\nmake(name = \"bad-name\")\n",
    );
    f.file(
        "attrname/BUILD",
        "load(\"//:defs.bzl\", \"make\")\nmake(name = \"x\", attr_values = {\"name\": \"y\"})\n",
    );
    f.file(
        "toplevel/defs.bzl",
        "def _impl(ctx):\n    return []\ntesting.analysis_test(name = \"x\", implementation = _impl)\n",
    );
    f.file("toplevel/BUILD", "load(\":defs.bzl\", \"testing\")\n");
    let stderr = |output: &Output| String::from_utf8_lossy(&output.stderr).into_owned();
    // Genuine: `in check_test rule //nested:inner_test: analysis_test rule
    // '//nested:inner_test' cannot be transitively depended on by another
    // analysis test rule` at nested/BUILD:3:11.
    let output = f.run("build", &["--nobuild", "//nested:outer_test"]);
    assert_eq!(output.status.code(), Some(1), "{}", stderr(&output));
    assert!(
        stderr(&output).contains(&format!(
            "{}/nested/BUILD:3:11: in check_test rule //nested:inner_test: analysis_test rule '//nested:inner_test' cannot be transitively depended on by another analysis test rule",
            f.workspace().display()
        )),
        "{}",
        stderr(&output)
    );
    for (pattern, message) in [
        (
            "//badname:all",
            "'name' is limited to Starlark identifiers, got bad-name",
        ),
        (
            "//attrname:all",
            "'name' cannot be set or overridden in 'attr_values'",
        ),
        (
            "//toplevel:all",
            "analysis_test can only be called in a BUILD thread",
        ),
    ] {
        let output = f.run("build", &["--nobuild", pattern]);
        assert_eq!(output.status.code(), Some(1), "{}", stderr(&output));
        assert!(
            stderr(&output).contains(message),
            "{pattern}: {}",
            stderr(&output)
        );
    }
}
