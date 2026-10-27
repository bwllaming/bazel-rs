//! RunfilesSupport.computeArgs (Bazel 9.3 analysis/RunfilesSupport.java:602-610):
//! the `args` of executable and test rules through
//! `Expander.withDataLocations().tokenized("args")` — location functions with
//! runfiles paths for `$(location)` over srcs/deps/data/tools, Make variables
//! (configuration, `--define`, `toolchains` TemplateVariableInfo), `$$`
//! escapes, ShellUtils.tokenize, and every failure as an attribute error.
//! Every row of tests/fixtures/test_args/reference_receipt.json (genuine
//! Bazel 9.3.0, `--batch`, private output root; tests/fixtures/test_args/capture.py)
//! runs natively here: `run` output and command line, the `test` output, and
//! the located `in args attribute` errors (native framing `bazel-rs: `).
#![allow(clippy::disallowed_methods)] // test harness: starts processes directly (clippy.toml)

use serde_json::Value;
include!("support/temp_root.rs");
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
include!("support/module_archives.rs");

/// The built-in closure's modules replaced by the offline stubs of
/// tests/fixtures/test_flags/stubs (as tests/bazel_test.rs): the root
/// module's overrides do not change any compared output.
const STUBBED: &[&str] = &[
    "apple_support",
    "buildozer",
    "protobuf",
    "rules_cc",
    "rules_java",
    "rules_python",
    "rules_shell",
    "zlib",
];

fn manifest() -> &'static Path {
    Path::new(env!("CARGO_MANIFEST_DIR"))
}

fn receipt() -> Value {
    serde_json::from_slice(
        &fs::read(manifest().join("tests/fixtures/test_args/reference_receipt.json")).unwrap(),
    )
    .unwrap()
}

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

struct Workspace(tempfile::TempDir);

impl Workspace {
    fn new() -> Self {
        let ws = Self(
            tempfile::Builder::new()
                .prefix("test-args-")
                .tempdir_in(crate::test_temp::root())
                .unwrap(),
        );
        copy_tree(
            &manifest().join("tests/fixtures/test_args/workspace"),
            &ws.ws(),
        );
        let stubs = manifest().join("tests/fixtures/test_flags/stubs");
        let mut module = fs::read_to_string(ws.ws().join("MODULE.bazel")).unwrap();
        for name in STUBBED {
            module.push_str(&format!(
                "local_path_override(module_name = \"{name}\", path = \"{}\")\n",
                stubs.join(name).display()
            ));
        }
        fs::write(ws.ws().join("MODULE.bazel"), module).unwrap();
        // Offline: the toolchain resolution materializes @platforms.
        seed_module_archives(&ws.path().join("cache"), &["platforms@1.0.0"]);
        ws
    }

    fn path(&self) -> &Path {
        self.0.path()
    }

    fn ws(&self) -> PathBuf {
        self.path().join("ws")
    }

    fn run(&self, args: &[&str]) -> Output {
        let cache = format!("--cache-dir={}", self.path().join("cache").display());
        let output = Command::new("timeout")
            .args(["--kill-after=5s", "600s", env!("CARGO_BIN_EXE_bazel-rs")])
            .args(["--ignore_all_rc_files", &cache])
            .args(args)
            .args([
                "--experimental-builtin-modules",
                &builtin_registry_flag(),
                "--define=MY_DEFINE=defined_value",
            ])
            .current_dir(self.ws())
            .env_clear()
            .env("PATH", "/usr/bin:/bin")
            .env("HOME", self.path())
            .output()
            .unwrap();
        assert_ne!(output.status.code(), Some(124), "CLI timed out: {args:?}");
        output
    }
}

fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

#[test]
fn run_and_test_args_follow_the_genuine_expansion_and_tokenization() {
    let receipt = receipt();
    let rows = &receipt["rows"];
    let ws = Workspace::new();
    for target in ["//pkg:b", "//pkg:exec_tool"] {
        let row = &rows[format!("run {target}")];
        let output = ws.run(&["run", target]);
        let stderr = text(&output.stderr);
        assert_eq!(output.status.code(), Some(0), "{stderr}");
        // Native `run` prints the built target's output line first.
        let stdout = text(&output.stdout);
        let program = stdout
            .lines()
            .filter(|line| !line.starts_with(target))
            .map(|line| format!("{line}\n"))
            .collect::<String>();
        assert_eq!(program, row["stdout"].as_str().unwrap(), "{target}");
        // `Running command line: <executable> <args>`: the args as Bazel
        // prints them (ShellEscaper).
        let command_line = stderr
            .lines()
            .find_map(|line| line.split_once("Running command line: "))
            .and_then(|(_, rest)| rest.split_once(' '))
            .map(|(_, args)| args.to_owned())
            .unwrap_or_else(|| panic!("no command line: {stderr}"));
        assert_eq!(
            command_line,
            row["command_line"][0].as_str().unwrap(),
            "{target}"
        );
    }

    let row = &rows["test //pkg:t"];
    let output = ws.run(&["test", "--test_output=all", "//pkg:t"]);
    let all = format!("{}{}", text(&output.stdout), text(&output.stderr));
    assert_eq!(output.status.code(), Some(0), "{all}");
    let lines = all.lines().collect::<Vec<_>>();
    let start = lines
        .iter()
        .position(|line| *line == "==================== Test output for //pkg:t:")
        .unwrap_or_else(|| panic!("no test output: {all}"));
    let end = start
        + lines[start..]
            .iter()
            .position(|line| *line == "=".repeat(80))
            .unwrap();
    let test_output = lines[start + 1..end]
        .iter()
        .map(|line| format!("{line}\n"))
        .collect::<String>();
    assert_eq!(test_output, row["test_output"].as_str().unwrap());
}

#[test]
fn args_errors_are_attribute_errors_with_bazel_texts() {
    let receipt = receipt();
    let rows = receipt["rows"].as_object().unwrap();
    let ws = Workspace::new();
    let workspace = ws.ws().display().to_string();
    let mut checked = 0;
    for (name, row) in rows {
        let Some(target) = name.strip_prefix("build --nobuild ") else {
            continue;
        };
        let output = ws.run(&["build", "--nobuild", target]);
        let stderr = text(&output.stderr).replace(&workspace, "<ws>");
        assert_eq!(output.status.code(), Some(1), "{target}: {stderr}");
        // Native framing: the first located event after `bazel-rs: `, the
        // others on their own lines.
        let errors = stderr
            .lines()
            .map(|line| line.strip_prefix("bazel-rs: ").unwrap_or(line))
            .filter(|line| line.contains(" in args attribute of "))
            .collect::<Vec<_>>();
        let expected = row["errors"]
            .as_array()
            .unwrap()
            .iter()
            .map(|error| error.as_str().unwrap())
            .collect::<Vec<_>>();
        assert_eq!(errors, expected, "{target}");
        checked += 1;
    }
    assert_eq!(checked, 11);
}
