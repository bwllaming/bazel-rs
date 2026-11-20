//! Parity cases of the open-source corpus bugs fixed on branch
//! corpus-parity5 (docs/OSS_CORPUS.md): Starlark transitions writing
//! `--instrumentation_filter` (B32/B33) and the execution platform in the
//! action key that shares or conflicts two targets' equal actions (the B13c
//! prerequisite), Args.validateMapEach's error for a map_each function the
//! rule implementation created and the CppOptions booleans `--save_temps`,
//! `--start_end_lib`, `--interface_shared_objects` and
//! `--process_headers_in_dependencies` (B32), the cases listed in
//! tests/fixtures/corpus_parity5/capture.py. Every
//! case replays the genuine Bazel 9.3.1 capture
//! tests/fixtures/corpus_parity5/reference_receipt.json (written by
//! capture.py beside it).
#![allow(clippy::disallowed_methods)] // test harness: starts processes directly (clippy.toml)
include!("support/temp_root.rs");
use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
    process::{Command, Output},
};

fn fixture() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/corpus_parity5")
}

fn copy(from: &Path, to: &Path) {
    fs::create_dir_all(to).unwrap();
    for entry in fs::read_dir(from).unwrap() {
        let entry = entry.unwrap();
        let destination = to.join(entry.file_name());
        if entry.file_type().unwrap().is_dir() {
            copy(&entry.path(), &destination)
        } else {
            fs::copy(entry.path(), destination).unwrap();
        }
    }
}

fn strings(value: &serde_json::Value) -> Vec<String> {
    value
        .as_array()
        .unwrap()
        .iter()
        .map(|item| item.as_str().unwrap().to_owned())
        .collect()
}

/// `(HH:mm:ss) ` as `(TIME) `, the workspace path as `<WS>`.
fn normalize(line: &str, workspace: &str) -> String {
    let line = line.replace(workspace, "<WS>");
    let bytes = line.as_bytes();
    let timed = bytes.len() >= 11
        && bytes[0] == b'('
        && bytes[9] == b')'
        && bytes[10] == b' '
        && line[1..9].chars().enumerate().all(|(index, c)| {
            if index % 3 == 2 {
                c == ':'
            } else {
                c.is_ascii_digit()
            }
        });
    if timed {
        format!("(TIME) {}", &line[11..])
    } else {
        line
    }
}

/// A genuine stderr line without the event and context prefixes whose
/// native presentation differs (`ERROR: `, `Error in F: `, `Skipping 'P': `,
/// `error loading package 'P': `): the message both must print.
fn core(line: &str) -> &str {
    let mut rest = line.strip_prefix("(TIME) ").unwrap_or(line);
    loop {
        let before = rest;
        rest = rest.strip_prefix("ERROR: ").unwrap_or(rest);
        // A load-time event's `<WS>/pkg/BUILD.bazel:L:C: ` location: native
        // prints the error with its Starlark traceback instead.
        if rest.starts_with("<WS>/")
            && let Some(at) = rest.find(": ")
            && rest[..at]
                .rsplitn(3, ':')
                .take(2)
                .all(|part| !part.is_empty() && part.bytes().all(|b| b.is_ascii_digit()))
        {
            rest = &rest[at + 2..];
        }
        for prefix in ["Error in ", "Skipping '", "error loading package '"] {
            if let Some(tail) = rest.strip_prefix(prefix) {
                let end = if prefix == "Error in " {
                    tail.find(": ").map(|at| at + 2)
                } else {
                    tail.find("': ").map(|at| at + 3)
                };
                if let Some(end) = end {
                    rest = &tail[end..];
                }
            }
        }
        if rest == before {
            return rest;
        }
    }
}

fn run(workspace: &Path, case: &serde_json::Value) -> Output {
    let command = case["command"].as_str().unwrap();
    Command::new(env!("CARGO_BIN_EXE_bazel-rs"))
        .current_dir(workspace)
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .env("HOME", workspace.join("home"))
        .env("TMPDIR", workspace.join("tmp"))
        .env("BAZEL_RS_RESULT_LINES", "1")
        .env("BAZEL_RS_CACHE_GC", "off")
        .arg("--ignore_all_rc_files")
        .args(strings(&case["startup"]))
        .args(["--cache-dir=cache", command])
        .args(["--lockfile_mode=off", "--memory-limit=2048"])
        .args(strings(&case["flags"]))
        .args(strings(&case["targets"]))
        .output()
        .unwrap()
}

#[test]
fn corpus_parity5_matches_genuine() {
    let reference: serde_json::Value = serde_json::from_str(
        &fs::read_to_string(fixture().join("reference_receipt.json")).unwrap(),
    )
    .unwrap();
    let mut failures = Vec::new();
    for (name, case) in reference["cases"].as_object().unwrap() {
        let temp = tempfile::tempdir_in(test_temp::root()).unwrap();
        let workspace = temp.path().join("ws");
        copy(
            &fixture().join(case["workspace"].as_str().unwrap()),
            &workspace,
        );
        for directory in ["home", "tmp"] {
            fs::create_dir(workspace.join(directory)).unwrap();
        }
        let output = run(&workspace, case);
        let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
        let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
        let lines: Vec<String> = stderr
            .lines()
            .map(|line| normalize(line, workspace.to_str().unwrap()))
            .collect();
        let mut problems = Vec::new();
        if output.status.code() != case["exit"].as_i64().map(|code| code as i32) {
            problems.push(format!(
                "exit {:?}, genuine {}",
                output.status.code(),
                case["exit"]
            ));
        }
        for expected in case["stderr"].as_array().unwrap() {
            let pattern = expected["pattern"].as_str().unwrap();
            let line = expected["line"].as_str().expect("genuine printed the line");
            if let Some(regex) = pattern.strip_prefix("^\\(TIME\\) ") {
                // Times differ: the same line shape, timestamp included.
                let regex = fancy_regex::Regex::new(&format!("^\\(TIME\\) {regex}")).unwrap();
                if !lines.iter().any(|line| regex.is_match(line).unwrap()) {
                    problems.push(format!("no line like {line:?}"));
                }
            } else if let Some(labels) = core(line).strip_prefix("Label: ") {
                // The conflicting actions' labels: genuine's order follows
                // its (parallel) analysis, compared as a set.
                let expected: std::collections::BTreeSet<&str> = labels.split(", ").collect();
                if !lines.iter().any(|native| {
                    native.strip_prefix("Label: ").is_some_and(|native| {
                        native
                            .split(", ")
                            .collect::<std::collections::BTreeSet<_>>()
                            == expected
                    })
                }) {
                    problems.push(format!("no label line like {line:?}"));
                }
            } else if !lines.iter().any(|native| native.contains(core(line))) {
                problems.push(format!("missing {:?}", core(line)));
            }
        }
        if case["command"] == "build" {
            // `label\tpath` result lines: each output's path below bazel-out.
            let mut actual = BTreeMap::new();
            for line in stdout.lines() {
                let Some((_, path)) = line.split_once('\t') else {
                    continue;
                };
                // `--nobuild` prints the paths it did not build.
                let Ok(content) = fs::read_to_string(path) else {
                    continue;
                };
                // Source files (a filegroup's) are not below bazel-out.
                let Some((_, relative)) = path.split_once("bazel-out/") else {
                    continue;
                };
                actual.insert(relative.to_owned(), content);
            }
            let expected: BTreeMap<String, String> = case["outputs"]
                .as_object()
                .unwrap()
                .iter()
                .map(|(path, content)| (path.clone(), content.as_str().unwrap().to_owned()))
                .collect();
            if actual != expected {
                problems.push(format!("outputs {actual:?}, genuine {expected:?}"));
            }
        } else if stdout != case["stdout"].as_str().unwrap() {
            problems.push(format!("stdout {stdout:?}, genuine {}", case["stdout"]));
        }
        // Paths below bazel-bin (the output tree): a symlink's text, a
        // file's content through symlinks.
        if let Some(paths) = case["bin_paths"].as_object()
            && !paths.is_empty()
        {
            let info = Command::new(env!("CARGO_BIN_EXE_bazel-rs"))
                .current_dir(&workspace)
                .env_clear()
                .env("PATH", "/usr/bin:/bin")
                .env("HOME", workspace.join("home"))
                .env("TMPDIR", workspace.join("tmp"))
                .args(["--ignore_all_rc_files", "--cache-dir=cache", "info"])
                .args(strings(&case["flags"]))
                .arg("bazel-bin")
                .output()
                .unwrap();
            let bin = PathBuf::from(String::from_utf8_lossy(&info.stdout).trim());
            for (relative, expected) in paths {
                let path = bin.join(relative);
                let actual = match fs::symlink_metadata(&path) {
                    Ok(metadata) if metadata.file_type().is_symlink() => serde_json::json!([
                        "symlink",
                        fs::read_link(&path).unwrap().to_str().unwrap()
                    ]),
                    _ => match fs::read_to_string(&path) {
                        Ok(content) => serde_json::json!(["file", content]),
                        Err(_) => serde_json::Value::Null,
                    },
                };
                if &actual != expected {
                    problems.push(format!(
                        "bazel-bin/{relative}: {actual}, genuine {expected}"
                    ));
                }
            }
        }
        if !problems.is_empty() {
            failures.push(format!("{name}: {}\n{stderr}", problems.join("; ")));
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n\n"));
}
