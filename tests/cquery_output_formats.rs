//! `cquery --output=starlark|files|graph|label_kind` and the formatter errors,
//! checked against genuine Bazel 9.3.1 captures of the same offline workspace
//! (tests/fixtures/cquery_output: cases.tsv, genuine/<case>.{out,rc,stderr}).
//!
//! Genuine prints results in Skyframe order and spells its own
//! configuration checksums; the comparison is over the sorted distinct lines
//! with 7-hex-digit checksum prefixes replaced (docs/CQUERY.md: native
//! ConfigIds are not BuildOptions checksums, and native analyzes platforms in
//! their incoming configurations). The explicit-module profile has no
//! embedded genrule setup source, so its lines are dropped on both sides.
#![allow(clippy::disallowed_methods)]
use std::path::{Path, PathBuf};
use std::process::Command;
include!("support/temp_root.rs");

fn fixtures() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/cquery_output")
}

fn copy_dir(from: &Path, to: &Path) {
    std::fs::create_dir_all(to).unwrap();
    for entry in std::fs::read_dir(from).unwrap() {
        let entry = entry.unwrap();
        let target = to.join(entry.file_name());
        if entry.file_type().unwrap().is_dir() {
            copy_dir(&entry.path(), &target);
        } else {
            std::fs::copy(entry.path(), &target).unwrap();
        }
    }
}

struct Workspace(tempfile::TempDir);

impl Workspace {
    fn new() -> Self {
        let dir = tempfile::tempdir_in(crate::test_temp::root()).unwrap();
        copy_dir(&fixtures().join("workspace"), &dir.path().join("ws"));
        Self(dir)
    }

    fn run(&self, args: &[String]) -> std::process::Output {
        Command::new(env!("CARGO_BIN_EXE_bazel-rs"))
            .current_dir(self.0.path().join("ws"))
            .env("BAZEL_RS_SNAPSHOT", "on")
            .env_remove("BAZEL_RS_DIGEST_MEMO")
            .arg("--ignore_all_rc_files")
            .arg(format!(
                "--cache-dir={}",
                self.0.path().join("cache").display()
            ))
            .args([
                "cquery",
                "--experimental-builtin-modules=false",
                "--lockfile_mode=off",
                "--platforms=//:linux",
                "--host_platform=//:linux",
            ])
            .args(args)
            .output()
            .unwrap()
    }
}

/// Sorted distinct lines, checksum prefixes and repository spellings
/// normalized (native graph labels are canonical `@@repo`).
fn normalized(text: &str) -> Vec<String> {
    let mut lines: Vec<String> = text
        .lines()
        .filter(|line| !line.contains("genrule-setup.sh"))
        .map(|line| {
            let line = line
                .replace("@@platforms//", "@platforms//")
                .replace("@@bazel_tools//", "@bazel_tools//");
            let mut out = String::new();
            let chars: Vec<char> = line.chars().collect();
            let mut index = 0;
            while index < chars.len() {
                let boundary = |i: usize| i >= chars.len() || !chars[i].is_ascii_alphanumeric();
                if (index == 0 || !chars[index - 1].is_ascii_alphanumeric())
                    && index + 7 <= chars.len()
                    && chars[index..index + 7]
                        .iter()
                        .all(|c| c.is_ascii_digit() || ('a'..='f').contains(c))
                    && boundary(index + 7)
                {
                    out.push('H');
                    index += 7;
                } else {
                    out.push(chars[index]);
                    index += 1;
                }
            }
            out
        })
        .collect();
    lines.sort();
    lines.dedup();
    lines
}

fn cases() -> Vec<(String, Vec<String>)> {
    std::fs::read_to_string(fixtures().join("cases.tsv"))
        .unwrap()
        .lines()
        .filter(|line| !line.is_empty())
        .map(|line| {
            let mut fields = line.split('\t').map(str::to_owned);
            let name = fields.next().unwrap();
            (name, fields.collect())
        })
        .collect()
}

fn genuine(case: &str, extension: &str) -> String {
    std::fs::read_to_string(
        fixtures()
            .join("genuine")
            .join(format!("{case}.{extension}")),
    )
    .unwrap()
}

/// Provider values are not retained by the native graph (docs/CQUERY.md):
/// their reprs are compared separately.
const VALUE_CASES: &[&str] = &["none_options"];

#[test]
fn outputs_match_genuine_captures() {
    let ws = Workspace::new();
    let mut failures = Vec::new();
    for (case, args) in cases() {
        let out = ws.run(&args);
        let code = out.status.code().unwrap_or(-1).to_string();
        let stdout = String::from_utf8_lossy(&out.stdout);
        let stderr = String::from_utf8_lossy(&out.stderr);
        if code != genuine(&case, "rc").trim() {
            failures.push(format!(
                "{case}: exit {code}, genuine {}: {stderr}",
                genuine(&case, "rc").trim()
            ));
            continue;
        }
        if !VALUE_CASES.contains(&case.as_str()) {
            let expected = normalized(&genuine(&case, "out"));
            let actual = normalized(&stdout);
            if expected != actual {
                failures.push(format!(
                    "{case}: stdout\n  genuine {expected:?}\n  native  {actual:?}"
                ));
            }
        }
        // The error events: genuine `ERROR: …` lines; native prints a
        // terminal error with its own `bazel-rs: ` prefix.
        for line in genuine(&case, "stderr").lines() {
            let line = line.strip_prefix("ERROR: ").unwrap_or(line);
            if !stderr.contains(line) {
                failures.push(format!("{case}: stderr lacks {line:?}:\n{stderr}"));
            }
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

#[test]
fn provider_values_are_explicitly_unretained() {
    let ws = Workspace::new();
    let out = ws.run(&[
        "//:src.txt".into(),
        "--output=starlark".into(),
        "--starlark:expr=providers(target)['DefaultInfo'].files".into(),
    ]);
    // A per-target evaluation error: an error event, no line, exit 0.
    assert!(out.status.success());
    assert!(out.stdout.is_empty());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("Starlark evaluation error for //:src.txt")
            && stderr.contains("does not retain provider values"),
        "{stderr}"
    );
    let out = ws.run(&[
        "//:top".into(),
        "--output=starlark".into(),
        "--starlark:expr=target.files".into(),
    ]);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("retains only `label`"), "{stderr}");
}

#[test]
fn formats_share_one_graph_and_jsonproto_parses() {
    let ws = Workspace::new();
    let first = ws.run(&["deps(//:top)".into(), "--output=label".into()]);
    assert!(
        first.status.success(),
        "{}",
        String::from_utf8_lossy(&first.stderr)
    );
    // Presentation options never key the analysis: a graph hit answers.
    let hit = Command::new(env!("CARGO_BIN_EXE_bazel-rs"))
        .current_dir(ws.0.path().join("ws"))
        .env("BAZEL_RS_SNAPSHOT", "on")
        .env("BAZEL_RS_SNAPSHOT_EXPLAIN", "1")
        .arg("--ignore_all_rc_files")
        .arg(format!(
            "--cache-dir={}",
            ws.0.path().join("cache").display()
        ))
        .args([
            "cquery",
            "--experimental-builtin-modules=false",
            "--lockfile_mode=off",
            "--platforms=//:linux",
            "--host_platform=//:linux",
            "//:top",
            "--output=starlark",
            "--starlark:expr=str(target.label) + ' ' + str(sorted(providers(target)))",
        ])
        .output()
        .unwrap();
    assert!(hit.status.success());
    assert!(
        String::from_utf8_lossy(&hit.stderr).contains("hit"),
        "{}",
        String::from_utf8_lossy(&hit.stderr)
    );
    assert_eq!(
        String::from_utf8_lossy(&hit.stdout),
        "@@//:top [\"//:defs.bzl%MyInfo\", \"DefaultInfo\", \"FileProvider\", \"FilesToRunProvider\", \"OutputGroupInfo\"]\n"
    );
    for (format, starts) in [
        ("--output=jsonproto", "{\n  \"results\": [{"),
        ("--output=textproto", "results {\n"),
    ] {
        let out = ws.run(&["//:top + //:src.txt".into(), format.into()]);
        assert!(out.status.success());
        let text = String::from_utf8(out.stdout).unwrap();
        assert!(text.starts_with(starts), "{text}");
        if format.ends_with("jsonproto") {
            let value: serde_json::Value = serde_json::from_str(&text).unwrap();
            assert_eq!(value["results"].as_array().unwrap().len(), 2);
            assert!(!value["configurations"].as_array().unwrap().is_empty());
        }
    }
    let out = ws.run(&[
        "//:top".into(),
        "--output=jsonproto".into(),
        "--noproto:include_configurations".into(),
    ]);
    let value: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(value["target"][0]["rule"]["name"], "//:top");
}

#[test]
fn unimplemented_formatters_fail_explicitly() {
    let ws = Workspace::new();
    for args in [
        vec!["//:top", "--output=build"],
        vec!["//:top", "--output=transitions", "--transitions=lite"],
        vec!["//:top", "--show_config_fragments=direct"],
    ] {
        let out = ws.run(&args.iter().map(|s| s.to_string()).collect::<Vec<_>>());
        assert!(!out.status.success(), "{args:?}");
        assert!(
            String::from_utf8_lossy(&out.stderr).contains("not implemented in the native profile"),
            "{args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
}

#[test]
fn config_lists_shows_and_diffs_the_last_cquery_configurations() {
    let ws = Workspace::new();
    let out = ws.run(&["deps(//:top)".into(), "--output=label".into()]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let config = |args: &[&str]| {
        Command::new(env!("CARGO_BIN_EXE_bazel-rs"))
            .current_dir(ws.0.path().join("ws"))
            .arg("--ignore_all_rc_files")
            .arg(format!(
                "--cache-dir={}",
                ws.0.path().join("cache").display()
            ))
            .arg("config")
            .args(args)
            .output()
            .unwrap()
    };
    // ConfigCommandOutputFormatter.TextOutputFormatter.writeConfigurationIDs.
    let listing = String::from_utf8(config(&[]).stdout).unwrap();
    let mut lines = listing.lines();
    assert_eq!(lines.next(), Some("Available configurations:"));
    let entries: Vec<(&str, &str)> = lines.map(|line| line.split_once(' ').unwrap()).collect();
    assert!(entries.len() >= 2, "{listing}");
    assert!(
        entries.windows(2).all(|pair| pair[0].0 < pair[1].0),
        "{listing}"
    );
    assert!(
        entries.iter().any(|(_, rest)| rest.ends_with(" (exec)")),
        "{listing}"
    );
    let json: serde_json::Value =
        serde_json::from_slice(&config(&["--output=json"]).stdout).unwrap();
    assert_eq!(
        json["configuration-IDs"].as_array().unwrap().len(),
        entries.len()
    );
    let (first, _) = entries[0];
    let shown = config(&[&first[..12]]);
    let text = String::from_utf8(shown.stdout).unwrap();
    assert!(text.starts_with(&format!(
        "BuildConfigurationValue {first}:\nSkyframe Key: BuildConfigurationKey[{first}]\nFragments: "
    )));
    assert!(text.contains("\nFragmentOptions "), "{text}");
    assert!(String::from_utf8_lossy(&shown.stderr).contains("Displaying config with id"));
    let (second, _) = entries[1];
    let diff = String::from_utf8(config(&[first, second]).stdout).unwrap();
    assert!(
        diff.starts_with(&format!(
            "Displaying diff between configs {first} and {second}\n"
        )),
        "{diff}"
    );
    let missing = config(&["ffffffffffffffff0"]);
    assert!(!missing.status.success());
    assert!(
        String::from_utf8_lossy(&missing.stderr)
            .contains("No configuration found with ID prefix ffffffffffffffff0")
    );
    let many = config(&["a", "b", "c"]);
    assert!(String::from_utf8_lossy(&many.stderr).contains("Too many config ids."));
}
