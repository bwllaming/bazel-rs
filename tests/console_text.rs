//! Bazel 9.3's console text without the interactive display (genuine
//! captures in tests/fixtures/console_text): `print()` as `DEBUG:
//! FILE:LINE:COL: TEXT` in BUILD files, `.bzl` top levels, macros, rule,
//! aspect and transition implementations, repository rules and module
//! extensions (crate::debug_print), and the `INFO: Analyzed …`, `INFO:
//! Found …`, `INFO: Elapsed time: …` and `INFO: Build completed …` lines
//! (crate::progress), also on an analysis snapshot hit.
#![allow(clippy::disallowed_methods)] // test harness: starts processes directly (clippy.toml)

use std::fs;
use std::path::Path;
use std::process::{Command, Output, Stdio};

include!("support/module_archives.rs");
include!("support/temp_root.rs");

const FIXTURE: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/console_text");

struct Workspace(tempfile::TempDir);

impl Workspace {
    fn new() -> Self {
        let ws = Self(
            tempfile::Builder::new()
                .prefix("console-text-")
                .tempdir_in(crate::test_temp::root())
                .unwrap(),
        );
        for entry in fs::read_dir(Path::new(FIXTURE).join("workspace")).unwrap() {
            let entry = entry.unwrap();
            fs::copy(entry.path(), ws.path().join(entry.file_name())).unwrap();
        }
        // Discovery visits the builtin registered toolchains, not just
        // platforms. Reuse the genuine, checksum-verified archive helper
        // rather than letting fresh fixtures fetch releases in every run.
        seed_module_archives(&ws.path().join("cache"), TOOLCHAIN_CLOSURE_ARCHIVES);
        ws
    }

    fn path(&self) -> &Path {
        self.0.path()
    }

    fn run(&self, arguments: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_bazel-rs"))
            .arg("--ignore_all_rc_files")
            .arg("--cache-dir=cache")
            .args(arguments)
            .arg(builtin_registry_flag())
            // The seeded cache plus local registry must suffice; a missing
            // archive fails locally, never becomes a network-dependent pass.
            .arg("--repository_disable_download")
            .args(["--curses=no", "--color=no"])
            .current_dir(self.path())
            .env("BAZEL_RS_MAX_MEMORY_MB", "4096")
            .stdin(Stdio::null())
            .output()
            .unwrap()
    }

    /// `rc=N` and the `DEBUG:`/`INFO:` lines (with a multi-line print's
    /// continuation) of a run, like the captures.
    fn observed(&self, output: &Output) -> Vec<String> {
        let root = self.path().to_str().unwrap();
        let stderr = String::from_utf8_lossy(&output.stderr);
        let mut lines = vec![format!("rc={}", output.status.code().unwrap())];
        lines.extend(
            stderr
                .lines()
                .filter(|line| {
                    line.starts_with("DEBUG: ") || line.starts_with("INFO: ") || *line == "line"
                })
                .map(|line| line.replace(root, "<WS>")),
        );
        lines
    }
}

fn genuine(name: &str) -> Vec<String> {
    fs::read_to_string(Path::new(FIXTURE).join(name))
        .unwrap()
        .lines()
        .map(str::to_owned)
        .collect()
}

/// The declared differences (fixtures README): counts, times, genuine's
/// process summary.
fn normalized(lines: &[String]) -> Vec<String> {
    lines
        .iter()
        .filter(|line| {
            !(line.starts_with("INFO: ") && line.contains(" process") && line.ends_with('.'))
                || line.starts_with("INFO: Analyzed ")
        })
        .map(|line| {
            if let Some(rest) = line.strip_prefix("INFO: Analyzed ") {
                let what = rest.split(" (").next().unwrap();
                format!("INFO: Analyzed {what} (<counts>).")
            } else if line.starts_with("INFO: Elapsed time: ") {
                "INFO: Elapsed time: <T>".to_owned()
            } else if let Some(at) = line.find(", ").filter(|_| line.contains(" total action")) {
                format!("{}, <N> total actions", &line[..at])
            } else {
                line.clone()
            }
        })
        .collect()
}

/// The DEBUG lines (each with its continuation) as a sorted list, and the
/// rest in order.
fn split_prints(lines: &[String]) -> (Vec<String>, Vec<String>) {
    let mut prints: Vec<String> = Vec::new();
    let mut rest = Vec::new();
    for line in lines {
        if line.starts_with("DEBUG: ") {
            prints.push(line.clone());
        } else if line == "line" {
            let last = prints.last_mut().unwrap();
            last.push('\n');
            last.push_str(line);
        } else {
            rest.push(line.clone());
        }
    }
    prints.sort();
    (prints, rest)
}

#[test]
fn prints_and_build_summary_match_bazel() {
    let ws = Workspace::new();
    let build = ws.run(&["build", "//:top", "//:m"]);
    let observed = ws.observed(&build);
    let (prints, rest) = split_prints(&observed);
    let (genuine_prints, genuine_rest) = split_prints(&genuine("genuine_build.txt"));
    assert_eq!(
        prints,
        genuine_prints,
        "{observed:#?}\nfull stderr:\n{}",
        String::from_utf8_lossy(&build.stderr)
    );
    assert_eq!(
        normalized(&rest),
        normalized(&genuine_rest),
        "{observed:#?}\nfull stderr:\n{}",
        String::from_utf8_lossy(&build.stderr)
    );

    // An analysis snapshot hit replays the analysis report (the prints
    // and `INFO: Analyzed …` with the counts of the analysis it saved),
    // then the build summary follows like after a full analysis. The
    // extension and its repository do not run again: the lockfile has them.
    let hit = ws.run(&["build", "//:top", "//:m"]);
    let replayed = ws.observed(&hit);
    let analyzed = |lines: &[String]| -> Vec<String> {
        lines
            .iter()
            .filter(|line| {
                !line.starts_with("INFO: Elapsed time: ") && !line.contains("<WS>/ext.bzl:")
            })
            .cloned()
            .collect()
    };
    assert_eq!(
        analyzed(&replayed),
        analyzed(&observed),
        "snapshot-hit stderr:\n{}",
        String::from_utf8_lossy(&hit.stderr)
    );

    // --quiet drops every DEBUG and INFO line.
    let quiet = ws.run(&["--quiet", "build", "//:top", "//:m"]);
    assert_eq!(
        ws.observed(&quiet),
        ["rc=0"],
        "quiet stderr:\n{}",
        String::from_utf8_lossy(&quiet.stderr)
    );
}

#[test]
fn single_targets_test_targets_and_failed_tests_match_bazel() {
    let ws = Workspace::new();
    for (arguments, capture) in [
        (&["build", "--nobuild", "//:gen"][..], "genuine_nobuild.txt"),
        (&["test", "//:t", "//:gen"][..], "genuine_test.txt"),
        (&["test", "//:t", "//:f"][..], "genuine_test_failed.txt"),
    ] {
        let output = ws.run(arguments);
        // Only the INFO lines: a native run loads the `.bzl` files again (a
        // genuine server had them).
        let observed: Vec<String> = ws
            .observed(&output)
            .into_iter()
            .filter(|line| !line.starts_with("DEBUG: ") && line != "line")
            .collect();
        assert_eq!(
            normalized(&observed),
            normalized(&genuine(capture)),
            "{arguments:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
}
