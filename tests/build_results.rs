//! Bazel 9.3's build results on stderr (crate::build_results; genuine
//! capture in tests/fixtures/build_results): `Target //x up-to-date:` with
//! the displayed paths, `(nothing to build)`, `failed to build`, the
//! aspects' results, the `--show_result` budget, `Use --verbose_failures
//! …`, nothing under `--nobuild`; `INFO: N processes: …` and the critical
//! path of `INFO: Elapsed time: …`; `, N aspect applications`. Without
//! `BAZEL_RS_RESULT_LINES` a build's stdout is empty.
#![allow(clippy::disallowed_methods)] // test harness: starts processes directly (clippy.toml)

use std::fs;
use std::path::Path;
use std::process::{Command, Stdio};

const FIXTURE: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/build_results");

fn copy(from: &Path, to: &Path) {
    fs::create_dir_all(to).unwrap();
    for entry in fs::read_dir(from).unwrap() {
        let entry = entry.unwrap();
        let target = to.join(entry.file_name());
        if entry.file_type().unwrap().is_dir() {
            copy(&entry.path(), &target);
        } else {
            fs::copy(entry.path(), target).unwrap();
        }
    }
}

/// The genuine capture: each command with its lines.
fn genuine() -> Vec<(String, Vec<String>)> {
    let text = fs::read_to_string(Path::new(FIXTURE).join("genuine.txt")).unwrap();
    let mut cases: Vec<(String, Vec<String>)> = Vec::new();
    for line in text.lines() {
        match line.strip_prefix("### ") {
            Some(command) => cases.push((command.to_owned(), Vec::new())),
            None => cases.last_mut().unwrap().1.push(line.to_owned()),
        }
    }
    cases
}

/// The lines capture.sh keeps: the results (paths under an `up-to-date:`
/// header), the hint and the summary lines.
fn kept(stderr: &str) -> Vec<String> {
    let mut lines = Vec::new();
    let mut paths = false;
    for line in stderr.lines() {
        if line.starts_with("Target ") || line.starts_with("Aspect ") {
            lines.push(line.to_owned());
            paths = line.ends_with("up-to-date:");
        } else if line.starts_with("  ") {
            if paths {
                lines.push(line.to_owned());
            }
        } else {
            paths = false;
            if line.starts_with("Use --verbose_failures")
                || line.starts_with("INFO: Analyzed")
                || line.starts_with("INFO: Elapsed")
                || (line.starts_with("INFO: ") && line.contains(" process"))
            {
                lines.push(line.to_owned());
            }
        }
    }
    lines
}

/// The declared differences: the analysis counts (a warm server's
/// `0 packages loaded`, genuine's `N aspect applications` of only the
/// newly configured ones), the times, the process counts (other actions,
/// another sandbox), the order of several targets' results (genuine's is
/// a hash set's), and the paths (bazel-rs has no convenience symlinks: the
/// output tree's path stands for `bazel-bin/…`).
fn normalized(lines: &[String], output_tree: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut blocks: Vec<Vec<String>> = Vec::new();
    for line in lines {
        let line = line.replace(output_tree, "bazel-bin/");
        if let Some(rest) = line.strip_prefix("INFO: Analyzed ") {
            let what = rest.split(" (").next().unwrap();
            out.push(format!("INFO: Analyzed {what} (<counts>)."));
        } else if line.starts_with("INFO: Elapsed time: ") {
            assert!(line.contains(", Critical Path: "), "{line}");
            out.push("INFO: Elapsed time: <T>s, Critical Path: <T>s".to_owned());
        } else if line.starts_with("INFO: ") && line != "INFO: 0 processes." {
            assert!(line.contains(" process") && line.ends_with('.'), "{line}");
            out.push("INFO: <N> processes: <counts>.".to_owned());
        } else if line.starts_with("  ") {
            blocks.last_mut().unwrap().push(line);
        } else if line.starts_with("Target ") || line.starts_with("Aspect ") {
            blocks.push(vec![line]);
        } else {
            out.push(line);
        }
    }
    blocks.sort();
    // The results print between the analysis and the summary.
    let at = out
        .iter()
        .position(|line| line.starts_with("Use ") || line.starts_with("INFO: Elapsed"))
        .unwrap_or(out.len());
    out.splice(at..at, blocks.into_iter().flatten());
    out
}

#[test]
fn build_results_match_bazel() {
    let base = std::env::var_os("TMPDIR")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(std::env::temp_dir);
    let temp = tempfile::Builder::new()
        .prefix("build-results-")
        .tempdir_in(base)
        .unwrap();
    let ws = temp.path().join("ws");
    copy(&Path::new(FIXTURE).join("workspace"), &ws);
    let cache = temp.path().join("cache");
    let mut failures = Vec::new();
    for (command, expected) in genuine() {
        let output = Command::new(env!("CARGO_BIN_EXE_bazel-rs"))
            .arg("--ignore_all_rc_files")
            .arg(format!("--cache-dir={}", cache.display()))
            .args(command.split(' '))
            .args(["--curses=no", "--color=no"])
            .current_dir(&ws)
            .env("BAZEL_RS_MAX_MEMORY_MB", "4096")
            .env_remove("BAZEL_RS_RESULT_LINES")
            .stdin(Stdio::null())
            .output()
            .unwrap();
        let stderr = String::from_utf8_lossy(&output.stderr);
        let stdout = String::from_utf8_lossy(&output.stdout);
        let mut observed = vec![format!("rc={}", output.status.code().unwrap())];
        observed.extend(kept(&stderr));
        let output_tree = Command::new(env!("CARGO_BIN_EXE_bazel-rs"))
            .arg("--ignore_all_rc_files")
            .arg(format!("--cache-dir={}", cache.display()))
            .args(["info", "bazel-bin"])
            .current_dir(&ws)
            .output()
            .unwrap();
        let output_tree = format!("{}/", String::from_utf8_lossy(&output_tree.stdout).trim());
        if normalized(&observed, &output_tree) != normalized(&expected, &output_tree)
            || (command.starts_with("build") && !stdout.is_empty())
        {
            failures.push(format!(
                "{command}\n--- native\n{}\n--- genuine\n{}\n--- stderr\n{stderr}\n--- stdout\n{stdout}",
                normalized(&observed, &output_tree).join("\n"),
                normalized(&expected, &output_tree).join("\n"),
            ));
        }
        // The aspect application of a fresh analysis.
        if command.contains("--output_groups=+asp_out") {
            assert!(
                stderr.contains("1 target configured, 1 aspect application)."),
                "{stderr}"
            );
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n\n"));
}

/// `BAZEL_RS_RESULT_LINES=1`: the native `LABEL<TAB>PATH` lines on stdout
/// as well, every artifact (also a source file's) whatever `--show_result`.
#[test]
fn native_result_lines_are_opt_in() {
    let base = std::env::var_os("TMPDIR")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(std::env::temp_dir);
    let temp = tempfile::Builder::new()
        .prefix("build-results-")
        .tempdir_in(base)
        .unwrap();
    let ws = temp.path().join("ws");
    copy(&Path::new(FIXTURE).join("workspace"), &ws);
    let output = Command::new(env!("CARGO_BIN_EXE_bazel-rs"))
        .arg("--ignore_all_rc_files")
        .arg(format!(
            "--cache-dir={}",
            temp.path().join("cache").display()
        ))
        .args(["build", "//:a", "//:src"])
        .current_dir(&ws)
        .env("BAZEL_RS_MAX_MEMORY_MB", "4096")
        .env("BAZEL_RS_RESULT_LINES", "1")
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert!(output.status.success());
    let stdout = String::from_utf8(output.stdout).unwrap();
    let rows: Vec<(&str, &str)> = stdout
        .lines()
        .map(|line| line.split_once('\t').unwrap())
        .collect();
    assert_eq!(
        rows.iter().map(|(label, _)| *label).collect::<Vec<_>>(),
        ["//:a", "//:a", "//:src"],
        "{stdout}"
    );
    assert!(rows[0].1.ends_with("/bazel-out/k8-fastbuild/bin/a.txt"));
    assert_eq!(rows[2].1, ws.join("BUILD").to_str().unwrap());
    // Bazel's results too: one non-empty target fits --show_result=1, the
    // `(nothing to build)` one is left out (two targets over the budget).
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("Target //:a up-to-date:\n  "), "{stderr}");
    assert!(!stderr.contains("//:src"), "{stderr}");
}
