//! `aquery` (docs/AQUERY.md) against the genuine Bazel 9.3.1 captures of
//! tests/fixtures/aquery/kitchen (reference_receipt.json): genrules, a
//! Starlark rule with run/run_shell/Args/param files/write/expand_template/
//! symlinks, an attribute aspect and ctx.actions.write(Args). Normalized:
//! the configuration checksum (native ConfigIds), the order of one label's
//! configurations, and depset structure (compared flattened).
#![allow(clippy::disallowed_methods)] // test harness: starts processes directly (clippy.toml)

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
include!("support/explicit_modules.rs");
include!("support/module_archives.rs");

fn fixture() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/aquery/kitchen")
}

fn genuine(name: &str) -> String {
    fs::read_to_string(fixture().join("genuine").join(name)).unwrap()
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
                .prefix("aquery-")
                .tempdir_in(crate::test_temp::root())
                .unwrap(),
        );
        copy_tree(&fixture().join("ws"), ws.0.path());
        seed_module_archives(&ws.0.path().join("cache"), &["platforms@1.0.0"]);
        ws
    }

    fn aquery(&self, args: &[&str]) -> Output {
        let registry = builtin_registry_flag();
        let output = Command::new("timeout")
            .args(["--kill-after=5s", "600s", env!("CARGO_BIN_EXE_bazel-rs")])
            .args(["--ignore_all_rc_files", "--cache-dir=cache", "aquery"])
            .args(["--experimental-builtin-modules", registry.as_str()])
            .args(args)
            .current_dir(self.0.path())
            .output()
            .unwrap();
        assert_ne!(
            output.status.code(),
            Some(124),
            "aquery timed out: {args:?}"
        );
        output
    }

    fn stdout(&self, args: &[&str]) -> String {
        let output = self.aquery(args);
        assert!(
            output.status.success(),
            "{args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout).unwrap()
    }
}

/// The text output's action blocks with the configuration checksum
/// replaced, sorted (one label's configurations may be listed in another
/// order).
fn text_blocks(text: &str) -> Vec<String> {
    let mut blocks: Vec<String> = text
        .split("\n\n")
        .filter(|block| !block.trim().is_empty())
        .map(|block| {
            block
                .lines()
                .map(|line| {
                    if line.starts_with("# Configuration: ") {
                        "# Configuration: <checksum>"
                    } else {
                        line
                    }
                })
                .collect::<Vec<_>>()
                .join("\n")
        })
        .collect();
    blocks.sort();
    blocks
}

fn assert_text(native: &str, expected: &str) {
    let (native, expected) = (text_blocks(native), text_blocks(expected));
    assert_eq!(native.len(), expected.len(), "{native:#?}");
    for (native, expected) in native.iter().zip(&expected) {
        assert_eq!(native, expected);
    }
}

/// A jsonproto container as self-contained actions: ids resolved, depsets
/// flattened (sorted), the configuration checksum dropped.
fn normalized_json(text: &str) -> Vec<String> {
    let container: serde_json::Value = serde_json::from_str(text).unwrap();
    let table = |name: &str| -> BTreeMap<u64, serde_json::Value> {
        container[name]
            .as_array()
            .map(|items| {
                items
                    .iter()
                    .map(|item| (item["id"].as_u64().unwrap(), item.clone()))
                    .collect()
            })
            .unwrap_or_default()
    };
    let (fragments, artifacts, depsets) = (
        table("pathFragments"),
        table("artifacts"),
        table("depSetOfFiles"),
    );
    let (targets, rule_classes, configurations, aspects) = (
        table("targets"),
        table("ruleClasses"),
        table("configuration"),
        table("aspectDescriptors"),
    );
    fn path(fragments: &BTreeMap<u64, serde_json::Value>, id: u64) -> String {
        let fragment = &fragments[&id];
        let label = fragment["label"].as_str().unwrap();
        match fragment["parentId"].as_u64() {
            Some(parent) => format!("{}/{label}", path(fragments, parent)),
            None => label.to_owned(),
        }
    }
    let artifact = |id: u64| {
        path(
            &fragments,
            artifacts[&id]["pathFragmentId"].as_u64().unwrap(),
        )
    };
    fn flatten(depsets: &BTreeMap<u64, serde_json::Value>, id: u64, out: &mut Vec<u64>) {
        let depset = &depsets[&id];
        for child in depset["transitiveDepSetIds"]
            .as_array()
            .into_iter()
            .flatten()
        {
            flatten(depsets, child.as_u64().unwrap(), out);
        }
        for direct in depset["directArtifactIds"].as_array().into_iter().flatten() {
            out.push(direct.as_u64().unwrap());
        }
    }
    let mut actions = Vec::new();
    for action in container["actions"].as_array().unwrap() {
        let mut action = action.as_object().unwrap().clone();
        let target = &targets[&action.remove("targetId").unwrap().as_u64().unwrap()];
        action.insert("target".into(), target["label"].clone());
        let rule_class = target["ruleClassId"]
            .as_u64()
            .map(|id| rule_classes[&id]["name"].clone());
        action.insert("ruleClass".into(), rule_class.unwrap_or_default());
        let mut configuration =
            configurations[&action.remove("configurationId").unwrap().as_u64().unwrap()].clone();
        let configuration = configuration.as_object_mut().unwrap();
        configuration.remove("id");
        configuration.remove("checksum");
        action.insert("configuration".into(), configuration.clone().into());
        let aspect_names: Vec<serde_json::Value> = action
            .remove("aspectDescriptorIds")
            .and_then(|ids| ids.as_array().cloned())
            .unwrap_or_default()
            .iter()
            .map(|id| {
                let mut aspect = aspects[&id.as_u64().unwrap()].clone();
                aspect.as_object_mut().unwrap().remove("id");
                aspect
            })
            .collect();
        action.insert("aspects".into(), aspect_names.into());
        let mut inputs = Vec::new();
        for id in action
            .remove("inputDepSetIds")
            .and_then(|ids| ids.as_array().cloned())
            .unwrap_or_default()
        {
            flatten(&depsets, id.as_u64().unwrap(), &mut inputs);
        }
        let mut inputs: Vec<String> = inputs.into_iter().map(artifact).collect();
        inputs.sort();
        inputs.dedup();
        action.insert("inputs".into(), inputs.into());
        let outputs: Vec<String> = action
            .remove("outputIds")
            .and_then(|ids| ids.as_array().cloned())
            .unwrap_or_default()
            .iter()
            .map(|id| artifact(id.as_u64().unwrap()))
            .collect();
        action.insert("outputs".into(), outputs.into());
        if let Some(id) = action.remove("primaryOutputId") {
            action.insert(
                "primaryOutput".into(),
                artifact(id.as_u64().unwrap()).into(),
            );
        }
        actions.push(serde_json::Value::Object(action).to_string());
    }
    actions.sort();
    actions
}

/// The number of `actions` entries of a binary container (`proto`) or of
/// delimited single-entry containers (`streamed_proto`).
fn count_actions(bytes: &[u8], delimited: bool) -> usize {
    fn varint(bytes: &[u8], at: &mut usize) -> u64 {
        let mut value = 0u64;
        let mut shift = 0;
        loop {
            let byte = bytes[*at];
            *at += 1;
            value |= u64::from(byte & 0x7f) << shift;
            shift += 7;
            if byte < 0x80 {
                return value;
            }
        }
    }
    fn fields(bytes: &[u8]) -> usize {
        let mut at = 0;
        let mut actions = 0;
        while at < bytes.len() {
            let key = varint(bytes, &mut at);
            assert_eq!(key & 7, 2, "container fields are messages");
            let length = varint(bytes, &mut at) as usize;
            at += length;
            if key >> 3 == 2 {
                actions += 1;
            }
        }
        assert_eq!(at, bytes.len());
        actions
    }
    if !delimited {
        return fields(bytes);
    }
    let mut at = 0;
    let mut actions = 0;
    while at < bytes.len() {
        let length = varint(bytes, &mut at) as usize;
        actions += fields(&bytes[at..at + length]);
        at += length;
    }
    actions
}

#[test]
fn aquery_matches_genuine_captures() {
    let ws = Workspace::new();
    // Text: every field but the configuration checksum, byte for byte.
    let all = ws.stdout(&["//pkg:all"]);
    assert_text(&all, &genuine("all.txt"));
    // A second run is byte-identical (deterministic order and ids).
    assert_eq!(ws.stdout(&["//pkg:all"]), all);
    assert_eq!(
        ws.stdout(&["//pkg:all", "--output=summary"]),
        genuine("all.summary")
    );
    let sorted_lines = |text: &str| {
        let mut lines: Vec<String> = text.lines().map(str::to_owned).collect();
        lines.sort();
        lines
    };
    assert_eq!(
        sorted_lines(&ws.stdout(&["//pkg:all", "--output=commands"])),
        sorted_lines(&genuine("all.commands"))
    );
    assert_eq!(
        normalized_json(&ws.stdout(&["//pkg:all", "--output=jsonproto"])),
        normalized_json(&genuine("all.jsonproto"))
    );
    let actions = genuine("all.txt").matches("\n  Mnemonic: ").count();
    for (format, delimited) in [("proto", false), ("streamed_proto", true)] {
        let output = ws.aquery(&["//pkg:all", &format!("--output={format}")]);
        assert!(output.status.success());
        assert_eq!(
            count_actions(&output.stdout, delimited),
            actions,
            "{format}"
        );
    }
    let textproto = ws.stdout(&["//pkg:all", "--output=textproto"]);
    assert_eq!(
        textproto.matches("\nactions {\n").count()
            + usize::from(textproto.starts_with("actions {")),
        actions
    );
    for (name, args) in [
        ("deps.txt", vec!["deps(//pkg:u1)"]),
        (
            "filters.txt",
            vec!["mnemonic('Genrule|FileWrite', deps(//pkg:u1))"],
        ),
        (
            "io.txt",
            vec!["inputs('.*in2.*', outputs('.*k\\..*', //pkg:all))"],
        ),
        (
            "pw.txt",
            vec![
                "//pkg:pw",
                "--include_param_files",
                "--include_file_write_contents",
            ],
        ),
        (
            "noaspects.txt",
            vec!["//pkg:u1 + //pkg:g", "--noinclude_aspects"],
        ),
        (
            "noartifacts.txt",
            vec![
                "//pkg:k",
                "--noinclude_artifacts",
                "--noinclude_commandline",
            ],
        ),
    ] {
        assert_text(&ws.stdout(&args), &genuine(name));
    }
    assert_eq!(
        normalized_json(&ws.stdout(&[
            "//pkg:pw",
            "--include_param_files",
            "--include_file_write_contents",
            "--output=jsonproto"
        ])),
        normalized_json(&genuine("pw.jsonproto"))
    );
}

#[test]
fn aquery_errors_like_genuine() {
    let ws = Workspace::new();
    let failure = |args: &[&str], code: i32, message: &str| {
        let output = ws.aquery(args);
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert_eq!(output.status.code(), Some(code), "{args:?}: {stderr}");
        assert!(stderr.contains(message), "{args:?}: {stderr}");
        assert!(!stderr.contains("panicked"), "{stderr}");
    };
    failure(
        &["deps(mnemonic(Genrule, //pkg:g))"],
        1,
        "aquery filter functions (inputs, outputs, mnemonic) produce actions, and therefore can't be the input of other function types: deps\ndeps(mnemonic('Genrule', //pkg:g))",
    );
    failure(
        &["//pkg:g", "--output=xml"],
        2,
        "Invalid output format 'xml'. Valid values are: proto, streamed_proto, textproto, jsonproto, text, commands, summary",
    );
    failure(
        &["mnemonic('Gen')"],
        1,
        "Error doing post analysis query: Evaluation of query \"mnemonic('Gen')\" failed: aquery filter functions (inputs, outputs, mnemonics) must have exactly 2 arguments,except when --skyframe_state is used.",
    );
    failure(
        &["--skyframe_state", "//pkg:g"],
        1,
        "aquery --skyframe_state is not implemented",
    );
    // --output_file writes the output there instead of stdout.
    let output = ws.aquery(&["//pkg:g", "--output_file=out.txt"]);
    assert!(output.status.success());
    assert!(output.stdout.is_empty());
    let written = fs::read_to_string(ws.0.path().join("out.txt")).unwrap();
    assert!(
        written.starts_with("action 'Executing genrule //pkg:g'\n"),
        "{written}"
    );
}
