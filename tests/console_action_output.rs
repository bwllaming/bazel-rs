//! Bazel 9.3.1's console for action output and action failures (genuine
//! captures in tests/fixtures/console_action_output, crate::action_console):
//! `INFO: From DESCRIPTION:` with a successful action's stderr (stderr) and
//! stdout (stdout), the output filters, `ERROR: LOC: DESCRIPTION failed:
//! (Exit N): …` with the abbreviated or `--verbose_failures` command, the
//! failed build's tail, missing outputs, `--keep_going`, `--subcommands`,
//! and the `INFO: N processes: …` tally.
#![allow(clippy::disallowed_methods)] // test harness: starts processes directly (clippy.toml)

use std::fs;
use std::path::Path;
use std::process::{Command, Output, Stdio};

include!("support/module_archives.rs");
include!("support/temp_root.rs");

const FIXTURE: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/tests/fixtures/console_action_output"
);

struct Workspace(tempfile::TempDir);

fn copy_tree(from: &Path, to: &Path) {
    fs::create_dir_all(to).unwrap();
    for entry in fs::read_dir(from).unwrap() {
        let entry = entry.unwrap();
        if entry.file_type().unwrap().is_dir() {
            copy_tree(&entry.path(), &to.join(entry.file_name()));
        } else {
            fs::copy(entry.path(), to.join(entry.file_name())).unwrap();
        }
    }
}

impl Workspace {
    fn new() -> Self {
        let ws = Self(
            tempfile::Builder::new()
                .prefix("console-action-output-")
                .tempdir_in(crate::test_temp::root())
                .unwrap(),
        );
        copy_tree(&Path::new(FIXTURE).join("workspace"), &ws.root());
        seed_module_archives(&ws.0.path().join("cache"), TOOLCHAIN_CLOSURE_ARCHIVES);
        ws
    }

    fn root(&self) -> std::path::PathBuf {
        self.0.path().join("ws")
    }

    fn run(&self, arguments: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_bazel-rs"))
            .arg("--ignore_all_rc_files")
            .arg(format!(
                "--cache-dir={}",
                self.0.path().join("cache").display()
            ))
            .args(arguments)
            .arg(builtin_registry_flag())
            .arg("--repository_disable_download")
            .args(["--curses=no", "--color=no"])
            .current_dir(self.root())
            .env("BAZEL_RS_MAX_MEMORY_MB", "4096")
            .env("BAZEL_RS_RESULT_LINES", "0")
            .stdin(Stdio::null())
            .output()
            .unwrap()
    }
}

/// The declared differences (fixture README) of a capture's stderr or
/// stdout: progress and analysis lines, times, the paths of execution
/// roots, results and saved streams, configuration checksums.
fn normalized(text: &str, workspace: &str) -> Vec<String> {
    let mut lines = Vec::new();
    for line in text.lines() {
        let line = line.replace(workspace, "<WS>");
        let dropped = line.starts_with("OpenJDK ")
            || line.starts_with("Computing main repo mapping:")
            || line.starts_with("Loading:")
            || line.starts_with("Analyzing:")
            || line.starts_with("    currently loading")
            || line.starts_with("INFO: Analyzed ")
            || line.starts_with("INFO: Found ")
            || line.starts_with("bazel-rs: ")
            || (line.is_empty() && lines.is_empty());
        if dropped {
            continue;
        }
        let mut line = line;
        if line.starts_with("INFO: Elapsed time: ") {
            line = "INFO: Elapsed time: <T>".to_owned();
        }
        if let Some(rest) = line
            .strip_prefix("(cd ")
            .or_else(|| line.strip_prefix("  (cd "))
        {
            let indent = if line.starts_with("  ") { "  " } else { "" };
            let tail = rest.split_once(" && ").map_or("", |(_, tail)| tail);
            line = format!("{indent}(cd <EXECROOT> && {tail}");
        }
        if line.starts_with("# Configuration: ") {
            line = "# Configuration: <C>".to_owned();
        }
        if let Some(at) = line.find(", configuration: ") {
            let end = line[at + 2..]
                .find(',')
                .map_or(line.len(), |end| at + 2 + end);
            line = format!("{}, configuration: <C>{}", &line[..at], &line[end..]);
        }
        if let Some(rest) = line.strip_prefix("  bazel-bin/") {
            line = format!("  <BIN>/{rest}");
        } else if line.starts_with("  /")
            && let Some(at) = line.find("/bin/")
            && line[..at].contains("/bazel-out/")
        {
            line = format!("  <BIN>/{}", &line[at + 5..]);
        }
        for stream in ["stdout (", "stderr ("] {
            if let Some(at) = line.find(stream)
                && let Some(end) = line[at..].find(") ")
            {
                line = format!("{}{stream}<PATH>{}", &line[..at], &line[at + end..]);
            }
        }
        lines.push(line);
    }
    lines
}

/// A capture: its command, exit code, stderr and stdout.
fn capture(name: &str) -> (Vec<String>, i32, String, String) {
    let text = fs::read_to_string(Path::new(FIXTURE).join("genuine").join(name)).unwrap();
    let (head, rest) = text.split_once("--- stderr\n").unwrap();
    let (stderr, stdout) = rest.split_once("--- stdout\n").unwrap();
    let mut head = head.lines();
    let command = head.next().unwrap().strip_prefix("### ").unwrap();
    let rc = head
        .next()
        .unwrap()
        .strip_prefix("rc=")
        .unwrap()
        .parse()
        .unwrap();
    (
        command.split(' ').map(str::to_owned).collect(),
        rc,
        stderr.to_owned(),
        stdout.to_owned(),
    )
}

#[test]
fn action_output_and_failures_match_bazel() {
    let mut names: Vec<String> = fs::read_dir(Path::new(FIXTURE).join("genuine"))
        .unwrap()
        .map(|entry| entry.unwrap().file_name().into_string().unwrap())
        .collect();
    names.sort();
    assert_eq!(names.len(), 17);
    for name in names {
        let (command, rc, stderr, stdout) = capture(&name);
        // A fresh output base and action cache, like the capture's `clean`.
        let ws = Workspace::new();
        let root = ws.root();
        let root = root.to_str().unwrap();
        let arguments: Vec<&str> = command.iter().map(String::as_str).collect();
        let output = ws.run(&arguments);
        let native_stderr = String::from_utf8_lossy(&output.stderr);
        let native_stdout = String::from_utf8_lossy(&output.stdout);
        assert_eq!(
            output.status.code(),
            Some(rc),
            "{name} {command:?}\n{native_stderr}"
        );
        assert_eq!(
            normalized(&native_stderr, root),
            normalized(&stderr, "<WS>"),
            "{name} {command:?} stderr:\n{native_stderr}"
        );
        assert_eq!(
            normalized(&native_stdout, root),
            normalized(&stdout, "<WS>"),
            "{name} {command:?} stdout:\n{native_stdout}"
        );
    }
}
