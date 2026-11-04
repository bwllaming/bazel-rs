//! `dump` and `print_action` (docs/COMMANDS.md) against the genuine Bazel
//! 9.3.1 captures of tests/fixtures/dump_command (reference_receipt.json).
#![allow(clippy::disallowed_methods)] // test harness: starts processes directly (clippy.toml)

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
include!("support/explicit_modules.rs");
include!("support/module_archives.rs");

fn fixture() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/dump_command")
}

fn receipt() -> serde_json::Value {
    serde_json::from_slice(&fs::read(fixture().join("reference_receipt.json")).unwrap()).unwrap()
}

fn copy_tree(from: &Path, to: &Path) {
    fs::create_dir_all(to).unwrap();
    for entry in fs::read_dir(from).unwrap() {
        let entry = entry.unwrap();
        let target = to.join(entry.file_name());
        if entry.file_type().unwrap().is_dir() {
            copy_tree(&entry.path(), &target);
        } else {
            fs::copy(entry.path(), &target).unwrap();
            fs::set_permissions(&target, entry.metadata().unwrap().permissions()).unwrap();
        }
    }
}

struct Workspace(tempfile::TempDir);

impl Workspace {
    fn new() -> Self {
        let ws = Self(
            tempfile::Builder::new()
                .prefix("dump-")
                .tempdir_in(crate::test_temp::root())
                .unwrap(),
        );
        copy_tree(&fixture().join("ws"), ws.0.path());
        ws
    }

    fn run(&self, command: &str, args: &[&str]) -> Output {
        let registry = builtin_registry_flag();
        let mut line = vec![command];
        if command == "print_action" {
            line.push(registry.as_str());
        }
        line.extend(args);
        let output = Command::new("timeout")
            .args(["--kill-after=5s", "600s", env!("CARGO_BIN_EXE_bazel-rs")])
            .args(["--ignore_all_rc_files", "--cache-dir=cache"])
            .args(&line)
            .current_dir(self.0.path())
            .output()
            .unwrap();
        assert_ne!(output.status.code(), Some(124), "timed out: {line:?}");
        output
    }

    fn shown(&self, text: &str) -> String {
        let workspace = fs::canonicalize(self.0.path()).unwrap();
        text.replace(&workspace.display().to_string(), "<WS>")
            .replace(&self.0.path().display().to_string(), "<WS>")
    }
}

/// The stderr lines genuine prints, without bazel-rs's own notes.
fn stderr_lines(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr)
        .lines()
        .filter(|line| !line.starts_with("bazel-rs: memory limit"))
        .map(|line| format!("{line}\n"))
        .collect()
}

#[test]
fn dump_matches_a_fresh_genuine_server() {
    let ws = Workspace::new();
    let receipt = receipt();
    for (case, expected) in receipt["dump"]["cases"].as_object().unwrap() {
        let args: Vec<&str> = expected["args"]
            .as_array()
            .unwrap()
            .iter()
            .map(|arg| arg.as_str().unwrap())
            .collect();
        let output = ws.run("dump", &args);
        let genuine = |extension: &str| {
            fs::read(fixture().join(format!("genuine/dump/{case}.{extension}"))).unwrap()
        };
        assert_eq!(
            output.status.code(),
            expected["exit"].as_i64().map(|code| code as i32),
            "dump {args:?}"
        );
        assert_eq!(
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&genuine("out")),
            "dump {args:?} stdout"
        );
        assert_eq!(
            stderr_lines(&output),
            String::from_utf8_lossy(&genuine("err")),
            "dump {args:?} stderr"
        );
    }
    // --rule_classes is the genuine table.
    let output = ws.run("dump", &["--rule_classes"]);
    assert!(output.status.success());
    assert_eq!(
        String::from_utf8_lossy(&output.stdout),
        format!(
            "{}\n",
            fs::read_to_string(
                Path::new(env!("CARGO_MANIFEST_DIR")).join("resources/dump_rule_classes.txt")
            )
            .unwrap()
        )
    );
}

#[test]
fn dump_action_cache_lists_the_native_action_cache() {
    let ws = Workspace::new();
    let empty = ws.run("dump", &["--action_cache"]);
    assert!(empty.status.success());
    assert_eq!(
        String::from_utf8_lossy(&empty.stdout),
        "Action cache (0 records):\nString indexer (0 records):\n\n"
    );
    let build = ws.run("build", &[builtin_registry_flag().as_str(), "//pkg:g"]);
    assert!(build.status.success(), "{}", stderr_lines(&build));
    let output = ws.run("dump", &["--action_cache"]);
    let stdout = String::from_utf8_lossy(&output.stdout);
    let lines: Vec<&str> = stdout.lines().collect();
    let count: usize = lines[0]
        .strip_prefix("Action cache (")
        .and_then(|rest| rest.strip_suffix(" records):"))
        .and_then(|count| count.parse().ok())
        .unwrap_or_else(|| panic!("{stdout}"));
    assert!(count >= 1, "{stdout}");
    let mut outputs = Vec::new();
    for index in 0..count {
        let record = &lines[1 + 4 * index..5 + 4 * index];
        let output = record[0]
            .strip_prefix(&format!("  {index} -> "))
            .unwrap_or_else(|| panic!("{stdout}"));
        outputs.push(output);
        assert!(record[1].starts_with("  packed_len = "), "{stdout}");
        assert!(
            record[2].starts_with("  timestamp = ") && record[2].ends_with(":00Z"),
            "{stdout}"
        );
        assert!(
            record[3].starts_with("  digest = ") && record[3].len() == 75,
            "{stdout}"
        );
    }
    assert!(
        outputs.contains(&"bazel-out/k8-fastbuild/bin/pkg/g.txt"),
        "{stdout}"
    );
    assert_eq!(
        lines[1 + 4 * count],
        format!("String indexer ({count} records):")
    );
    for (index, output) in outputs.iter().enumerate() {
        assert_eq!(
            lines[2 + 4 * count + index],
            format!("  {index} <=> {output}")
        );
    }
}

#[test]
fn print_action_matches_genuine() {
    let ws = Workspace::new();
    let receipt = receipt();
    for (case, expected) in receipt["print_action"]["cases"].as_object().unwrap() {
        let args: Vec<&str> = expected["args"]
            .as_array()
            .unwrap()
            .iter()
            .map(|arg| arg.as_str().unwrap())
            .collect();
        let output = ws.run("print_action", &args);
        let stderr = ws.shown(&stderr_lines(&output));
        assert_eq!(
            output.status.code(),
            expected["exit"].as_i64().map(|code| code as i32),
            "print_action {args:?}: {stderr}"
        );
        let genuine = fs::read(fixture().join(format!("genuine/print_action/{case}.out"))).unwrap();
        assert_eq!(
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&genuine),
            "print_action {args:?} stdout"
        );
        if let Some(error) = expected["error"].as_str() {
            let (before, after) = error.split_once("<CHECKSUM>").unwrap_or((error, ""));
            assert!(
                stderr.contains(before) && stderr.contains(after),
                "print_action {args:?}: {stderr}"
            );
        }
    }
}
