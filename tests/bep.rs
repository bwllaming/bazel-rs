//! Build Event Protocol file outputs, `--profile` and the execution logs of
//! `build`, `test` and `run` (docs/BEP.md, docs/PROFILE.md).
//!
//! The fixture constants are shared verbatim with the paired genuine Bazel
//! 9.3.0 capture (/tmp/opencode/perf-research/bep-capture.py extracts them);
//! its receipt is tests/fixtures/bep/reference_receipt.json and the scrubbed
//! genuine streams beside it are what these tests compare the native
//! streams with, structurally.
#![allow(clippy::disallowed_methods)] // test harness: starts processes directly (clippy.toml)

const MODULE: &str = r#"module(name = "bep")
"#;

const DEFS: &str = r##"def _t(ctx):
    out = ctx.actions.declare_file(ctx.label.name + ".sh")
    ctx.actions.write(out, ctx.attr.script, is_executable = True)
    return [DefaultInfo(executable = out, runfiles = ctx.runfiles(files = ctx.files.data))]

sh_like_test = rule(
    implementation = _t,
    test = True,
    attrs = {"script": attr.string(), "data": attr.label_list(allow_files = True)},
)
"##;

const PKG_BUILD: &str = r##"load("//:defs.bzl", "sh_like_test")

genrule(name = "a", srcs = ["in.txt"], outs = ["a.txt"], cmd = "cat $< > $@")

genrule(
    name = "b",
    srcs = [":a"],
    outs = ["b.txt", "b2.txt"],
    cmd = "cat $(location :a) > $(location b.txt); echo x > $(location b2.txt)",
)

sh_like_test(name = "passes_test", script = "#!/bin/bash\necho pass\n", data = [":a"])

sh_like_test(name = "fails_test", script = "#!/bin/bash\necho fail\nexit 1\n", tags = ["manual"])

genrule(name = "broken", outs = ["broken.txt"], cmd = "echo boom >&2; exit 3", tags = ["manual"])
"##;

const IN_TXT: &str = r#"hello
"#;

/// The scenarios (name, command and arguments; `{CAP}` is the capture
/// directory), in the capture's order.
const SCENARIOS: &[(&str, &[&str])] = &[
    (
        "build",
        &[
            "build",
            "--build_event_json_file={CAP}/bep.json",
            "--build_event_text_file={CAP}/bep.txt",
            "--build_event_binary_file={CAP}/bep.bin",
            "--profile={CAP}/profile.gz",
            "--execution_log_json_file={CAP}/exec.json",
            "//pkg:a",
            "//pkg:b",
            "//pkg:passes_test",
        ],
    ),
    (
        "test",
        &[
            "test",
            "--build_event_json_file={CAP}/bep.json",
            "//pkg:passes_test",
            "//pkg:fails_test",
        ],
    ),
    (
        "action_failure",
        &[
            "build",
            "--build_event_json_file={CAP}/bep.json",
            "//pkg:broken",
            "//pkg:a",
        ],
    ),
    (
        "keep_going_all_actions",
        &[
            "build",
            "-k",
            "--build_event_publish_all_actions",
            "--build_event_json_file={CAP}/bep.json",
            "//pkg:broken",
            "//pkg:a",
        ],
    ),
    (
        "nobuild",
        &[
            "build",
            "--nobuild",
            "--build_event_json_file={CAP}/bep.json",
            "//pkg:a",
        ],
    ),
    (
        "run",
        &[
            "run",
            "--build_event_json_file={CAP}/bep.json",
            "//pkg:passes_test",
        ],
    ),
    (
        "compact_log",
        &[
            "build",
            "--execution_log_compact_file={CAP}/exec.compact",
            "//pkg:b",
        ],
    ),
];

include!("support/verify_env.rs");
include!("support/explicit_modules.rs");

use serde_json::{Value, json};
use std::collections::{BTreeMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

/// The genuine exit codes (the receipt's rows).
fn expected_exit(name: &str) -> i32 {
    match name {
        "test" => 3,
        "action_failure" | "keep_going_all_actions" => 1,
        _ => 0,
    }
}

fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/bep")
        .join(name)
}

struct Workspace(tempfile::TempDir);

impl Workspace {
    fn new() -> Self {
        let ws = Self(
            tempfile::Builder::new()
                .prefix("bep-")
                .tempdir_in(crate::test_temp::root())
                .unwrap(),
        );
        for (name, text) in [
            ("MODULE.bazel", MODULE),
            ("BUILD.bazel", ""),
            ("defs.bzl", DEFS),
            ("pkg/BUILD", PKG_BUILD),
            ("pkg/in.txt", IN_TXT),
        ] {
            let path = ws.path().join(name);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(&path, text).unwrap();
        }
        fs::create_dir_all(ws.capture()).unwrap();
        ws
    }

    fn path(&self) -> &Path {
        self.0.path()
    }

    fn capture(&self) -> PathBuf {
        self.path().join("cap")
    }

    fn run(&self, args: &[&str], environment: &[(&str, &str)]) -> Output {
        let capture = self.capture().display().to_string();
        let args: Vec<String> = args
            .iter()
            .map(|arg| arg.replace("{CAP}", &capture))
            .collect();
        let args: Vec<&str> = args.iter().map(String::as_str).collect();
        let output = Command::new("timeout")
            .args(["--kill-after=5s", "600s", env!("CARGO_BIN_EXE_bazel-rs")])
            .args(["--ignore_all_rc_files", "--cache-dir=private-cache"])
            .args(with_explicit_modules(&args))
            .current_dir(self.path())
            .env_clear()
            .envs(verifier_environment())
            .env("PATH", "/usr/bin:/bin")
            .env("HOME", self.path())
            .envs(environment.iter().copied())
            .output()
            .unwrap();
        assert_ne!(output.status.code(), Some(124), "CLI timed out: {args:?}");
        output
    }
}

fn scenario(name: &str) -> &'static [&'static str] {
    SCENARIOS
        .iter()
        .find(|(scenario, _)| *scenario == name)
        .map(|(_, args)| *args)
        .unwrap()
}

fn events(path: &Path) -> Vec<Value> {
    fs::read_to_string(path)
        .unwrap_or_else(|error| panic!("{}: {error}", path.display()))
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| serde_json::from_str(line).unwrap())
        .collect()
}

fn payload_kind(event: &Value) -> String {
    event
        .as_object()
        .unwrap()
        .keys()
        .find(|key| !matches!(key.as_str(), "id" | "children" | "lastMessage"))
        .unwrap()
        .clone()
}

/// The structural form of a stream (the capture's `normalize`, mirrored):
/// non-progress events as (id, sorted children, payload kind, structural
/// payload) with configuration checksums renamed by first appearance and
/// named sets named by their files, sorted; the progress event count; the
/// ids progress events linked, sorted.
#[derive(Debug, PartialEq)]
struct Normalized {
    events: Vec<String>,
    progress: usize,
    linked: Vec<String>,
}

fn normalize(events: &[Value]) -> Normalized {
    let mut configurations: BTreeMap<String, String> = BTreeMap::new();
    let mut sets: BTreeMap<String, String> = BTreeMap::new();
    for event in events {
        if let Some(id) = event["id"]["configuration"]["id"].as_str()
            && !matches!(id, "none" | "system")
            && !configurations.contains_key(id)
        {
            let name = format!("cfg{}", configurations.len());
            configurations.insert(id.to_owned(), name);
        }
        if let Some(id) = event["id"]["namedSet"]["id"].as_str() {
            let mut names: Vec<&str> = event["namedSetOfFiles"]["files"]
                .as_array()
                .map(|files| {
                    files
                        .iter()
                        .filter_map(|file| file["name"].as_str())
                        .collect()
                })
                .unwrap_or_default();
            names.sort();
            sets.insert(id.to_owned(), format!("set[{}]", names.join(",")));
        }
    }
    fn rename(value: &Value, configurations: &BTreeMap<String, String>) -> Value {
        match value {
            Value::Object(map) => Value::Object(
                map.iter()
                    .map(|(key, value)| {
                        let value = match (key.as_str(), value["id"].as_str()) {
                            ("configuration", Some(id)) => json!({
                                "id": configurations.get(id).map_or(id, String::as_str)
                            }),
                            _ => rename(value, configurations),
                        };
                        (key.clone(), value)
                    })
                    .collect(),
            ),
            Value::Array(items) => Value::Array(
                items
                    .iter()
                    .map(|item| rename(item, configurations))
                    .collect(),
            ),
            other => other.clone(),
        }
    }
    let norm_id = |id: &Value| -> String {
        let mut id = rename(id, &configurations);
        if let Some(set) = id["namedSet"]["id"].as_str() {
            id = json!({"namedSet": {"id": sets.get(set).cloned().unwrap_or_else(|| "?".to_owned())}});
        }
        serde_json::to_string(&id).unwrap()
    };
    let strings = |value: &Value| -> Vec<String> {
        value
            .as_array()
            .map(|items| {
                items
                    .iter()
                    .filter_map(|item| item.as_str().map(str::to_owned))
                    .collect()
            })
            .unwrap_or_default()
    };
    let summary = |kind: &str, p: &Value| -> Value {
        match kind {
            "started" => {
                let mut fields: Vec<&String> = p.as_object().unwrap().keys().collect();
                fields.sort();
                json!({"fields": fields, "command": p["command"]})
            }
            "structuredCommandLine" => json!({
                "label": p["commandLineLabel"],
                "sections": p["sections"].as_array().map(|sections| sections
                    .iter()
                    .map(|section| section["sectionLabel"].clone())
                    .collect::<Vec<_>>()).unwrap_or_default(),
            }),
            "workspaceInfo" => {
                let fields: Vec<&String> = p.as_object().unwrap().keys().collect();
                json!({"fields": fields})
            }
            "configuration" => {
                let mut variables: Vec<&String> = p["makeVariable"]
                    .as_object()
                    .map(|map| map.keys().collect())
                    .unwrap_or_default();
                variables.sort();
                json!({"mnemonic": p["mnemonic"], "platformName": p["platformName"],
                       "cpu": p["cpu"], "makeVariable": variables})
            }
            "workspaceStatus" => json!({"keys": p["item"].as_array().map(|items| items
                .iter().map(|item| item["key"].clone()).collect::<Vec<_>>()).unwrap_or_default()}),
            "configured" => {
                json!({"targetKind": p["targetKind"], "testSize": p["testSize"], "tag": p["tag"]})
            }
            "namedSetOfFiles" => {
                let mut files: Vec<String> = p["files"]
                    .as_array()
                    .map(|files| {
                        files
                            .iter()
                            .map(|file| {
                                json!([
                                    file["name"],
                                    file.get("pathPrefix").cloned().unwrap_or(json!([])),
                                    file["digest"],
                                    file["length"]
                                ])
                                .to_string()
                            })
                            .collect()
                    })
                    .unwrap_or_default();
                files.sort();
                json!({"files": files})
            }
            "completed" => {
                let groups: Vec<Value> = p["outputGroup"]
                    .as_array()
                    .map(|groups| {
                        groups
                            .iter()
                            .map(|group| {
                                let mut ids: Vec<String> = group["fileSets"]
                                    .as_array()
                                    .map(|sets_| {
                                        sets_
                                            .iter()
                                            .map(|set| {
                                                sets.get(set["id"].as_str().unwrap_or(""))
                                                    .cloned()
                                                    .unwrap_or_else(|| "?".to_owned())
                                            })
                                            .collect()
                                    })
                                    .unwrap_or_default();
                                ids.sort();
                                json!([group["name"], ids])
                            })
                            .collect()
                    })
                    .unwrap_or_default();
                json!({"success": p["success"].as_bool().unwrap_or(false), "outputGroup": groups,
                       "tag": strings(&p["tag"]), "testTimeout": p["testTimeout"],
                       "failureDetail": p.get("failureDetail").is_some()})
            }
            "action" => json!({"success": p["success"].as_bool().unwrap_or(false),
                "exitCode": p["exitCode"].as_i64().unwrap_or(0), "type": p["type"], "label": p["label"],
                "commandLine": strings(&p["commandLine"]), "stderr": p.get("stderr").is_some(),
                "spawn": p["failureDetail"]["spawn"]}),
            "testResult" => {
                json!({"status": p["status"], "cachedLocally": p["cachedLocally"].as_bool().unwrap_or(false),
                "outputs": p["testActionOutput"].as_array().map(|outputs| outputs.iter()
                    .map(|output| output["name"].clone()).collect::<Vec<_>>()).unwrap_or_default()})
            }
            "testSummary" => {
                json!({"overallStatus": p["overallStatus"], "totalRunCount": p["totalRunCount"],
                "runCount": p["runCount"], "attemptCount": p["attemptCount"],
                "passed": p["passed"].as_array().map_or(0, Vec::len),
                "failed": p["failed"].as_array().map_or(0, Vec::len)})
            }
            "finished" => json!({"overallSuccess": p["overallSuccess"].as_bool().unwrap_or(false),
                "exitCode": p["exitCode"], "failureDetail": p.get("failureDetail").is_some()}),
            "buildToolLogs" => {
                let mut logs: Vec<String> = p["log"]
                    .as_array()
                    .map(|logs| {
                        logs.iter()
                            .filter_map(|log| log["name"].as_str())
                            .filter(|name| *name != "command.profile.gz")
                            .map(str::to_owned)
                            .collect()
                    })
                    .unwrap_or_default();
                logs.sort();
                json!({"logs": logs})
            }
            "aborted" => json!({"reason": p["reason"]}),
            "execRequest" => {
                use base64::Engine as _;
                let argv: Vec<String> = p["argv"]
                    .as_array()
                    .map(|argv| {
                        argv.iter()
                            .take(2)
                            .map(|arg| {
                                String::from_utf8(
                                    base64::engine::general_purpose::STANDARD
                                        .decode(arg.as_str().unwrap())
                                        .unwrap(),
                                )
                                .unwrap()
                            })
                            .collect()
                    })
                    .unwrap_or_default();
                json!({"argv": argv})
            }
            _ => json!({}),
        }
    };
    let mut normalized = Vec::new();
    let mut linked = Vec::new();
    let mut progress = 0;
    for event in events {
        let kind = payload_kind(event);
        let children = event["children"].as_array().cloned().unwrap_or_default();
        if event["id"].get("progress").is_some() {
            progress += 1;
            linked.extend(
                children
                    .iter()
                    .filter(|child| child.get("progress").is_none())
                    .map(norm_id),
            );
            continue;
        }
        let mut children: Vec<String> = children.iter().map(norm_id).collect();
        children.sort();
        normalized.push(
            json!({"id": serde_json::from_str::<Value>(&norm_id(&event["id"])).unwrap(),
                   "children": children, "kind": kind, "payload": summary(&kind, &event[&kind])})
            .to_string(),
        );
    }
    normalized.sort();
    linked.sort();
    Normalized {
        events: normalized,
        progress,
        linked,
    }
}

/// BuildEventStreamer's invariants: every event but the first was
/// announced by an earlier one; only the last carries `last_message`.
fn assert_announced(events: &[Value]) {
    let mut announced: HashSet<String> = HashSet::new();
    for (index, event) in events.iter().enumerate() {
        let id = event["id"].to_string();
        assert!(index == 0 || announced.contains(&id), "unannounced {id}");
        for child in event["children"].as_array().into_iter().flatten() {
            announced.insert(child.to_string());
        }
        assert_eq!(
            event["lastMessage"].as_bool().unwrap_or(false),
            index + 1 == events.len(),
            "last_message at {index}"
        );
    }
}

fn assert_structural(name: &str, native: &[Value]) {
    let genuine = events(&fixture(&format!("{name}.genuine.json")));
    assert_announced(native);
    let (expected, actual) = (normalize(&genuine), normalize(native));
    let missing: Vec<&String> = expected
        .events
        .iter()
        .filter(|e| !actual.events.contains(e))
        .collect();
    let extra: Vec<&String> = actual
        .events
        .iter()
        .filter(|e| !expected.events.contains(e))
        .collect();
    assert!(
        missing.is_empty() && extra.is_empty(),
        "{name}: missing {missing:#?}\nextra {extra:#?}"
    );
    assert_eq!(expected, actual, "{name}");
}

/// Every scenario with a build event file: the native stream is the
/// genuine one structurally, and the exit code is genuine's.
#[test]
fn build_event_streams_match_the_genuine_streams_structurally() {
    for (name, args) in SCENARIOS {
        if !args
            .iter()
            .any(|arg| arg.starts_with("--build_event_json_file"))
        {
            continue;
        }
        let ws = Workspace::new();
        let output = ws.run(args, &[]);
        assert_eq!(
            output.status.code(),
            Some(expected_exit(name)),
            "{name}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            stderr.contains("INFO: Build Event Protocol files produced successfully."),
            "{name}: {stderr}"
        );
        let native = events(&ws.capture().join("bep.json"));
        assert_structural(name, &native);
    }
}

/// The three transports carry the same events; the binary one is a
/// length-delimited stream, the text one `event { ... }` blocks.
#[test]
fn json_binary_and_text_transports_agree() {
    let ws = Workspace::new();
    let output = ws.run(scenario("build"), &[]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let json = events(&ws.capture().join("bep.json"));
    let binary = fs::read(ws.capture().join("bep.bin")).unwrap();
    let (mut index, mut count) = (0, 0);
    while index < binary.len() {
        let (mut length, mut shift) = (0usize, 0);
        loop {
            let byte = binary[index];
            index += 1;
            length |= usize::from(byte & 0x7f) << shift;
            shift += 7;
            if byte < 0x80 {
                break;
            }
        }
        index += length;
        count += 1;
    }
    assert_eq!(index, binary.len());
    assert_eq!(count, json.len());
    let text = fs::read_to_string(ws.capture().join("bep.txt")).unwrap();
    assert_eq!(text.matches("event {\nid {\n").count(), json.len());
    assert!(text.contains("  started {\n"), "{text}");
}

/// A snapshot hit streams what its full analysis did (the build event
/// plan is part of the planned request); the build event options are not
/// part of the snapshot key.
#[test]
fn a_snapshot_hit_streams_the_same_events() {
    let ws = Workspace::new();
    let args = scenario("build");
    let timing = [("BAZEL_RS_TIMING", "1"), ("BAZEL_RS_SNAPSHOT", "on")];
    let first = ws.run(args, &timing);
    assert!(
        first.status.success(),
        "{}",
        String::from_utf8_lossy(&first.stderr)
    );
    let miss = events(&ws.capture().join("bep.json"));
    let second = ws.run(args, &timing);
    assert!(second.status.success());
    let stderr = String::from_utf8_lossy(&second.stderr);
    assert!(stderr.contains(" snapshot.hit"), "{stderr}");
    let hit = events(&ws.capture().join("bep.json"));
    assert_eq!(normalize(&miss), normalize(&hit));
    assert_structural("build", &hit);
}

/// `--profile=<file>.gz`: a gzipped Chrome trace with Bazel's otherData,
/// thread metadata, the build phase markers, one span per action with its
/// mnemonic, the critical path on thread 0.
#[test]
fn the_profile_is_a_chrome_trace_with_phases_actions_and_the_critical_path() {
    let ws = Workspace::new();
    let output = ws.run(
        &[
            "build",
            "--profile={CAP}/profile.gz",
            "--experimental_profile_include_target_label",
            "//pkg:b",
        ],
        &[],
    );
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let compressed = fs::read(ws.capture().join("profile.gz")).unwrap();
    let mut text = String::new();
    std::io::Read::read_to_string(
        &mut flate2::read::GzDecoder::new(&compressed[..]),
        &mut text,
    )
    .unwrap();
    let trace: Value = serde_json::from_str(&text).unwrap();
    let mut other: Vec<&String> = trace["otherData"].as_object().unwrap().keys().collect();
    other.sort();
    assert_eq!(
        other,
        [
            "bazel_version",
            "build_id",
            "date",
            "output_base",
            "profile_start_ts"
        ]
    );
    let events = trace["traceEvents"].as_array().unwrap();
    let phases: Vec<&str> = events
        .iter()
        .filter(|event| event["cat"] == "build phase marker")
        .map(|event| event["name"].as_str().unwrap())
        .collect();
    for phase in [
        "Launch Blaze",
        "Initialize command",
        "Evaluate target patterns",
        "Load and analyze dependencies",
        "Build artifacts",
        "Complete build",
    ] {
        assert!(phases.contains(&phase), "{phase}: {phases:?}");
    }
    let genrules: Vec<&Value> = events
        .iter()
        .filter(|event| {
            event["cat"] == "action processing" && event["args"]["mnemonic"] == "Genrule"
        })
        .collect();
    assert_eq!(genrules.len(), 2, "{events:#?}");
    assert!(
        genrules
            .iter()
            .all(|event| event["ph"] == "X" && event["dur"].is_u64())
    );
    assert!(
        genrules
            .iter()
            .any(|event| event["args"]["target"] == "//pkg:b")
    );
    let critical: Vec<&Value> = events
        .iter()
        .filter(|event| event["cat"] == "critical path component")
        .collect();
    assert_eq!(critical.len(), 2, "{critical:#?}");
    assert!(critical.iter().all(|event| event["tid"] == 0));
    for event in events.iter().filter(|event| event["ph"] == "X") {
        let tid = &event["tid"];
        assert!(
            events.iter().any(|meta| meta["ph"] == "M"
                && meta["name"] == "thread_name"
                && &meta["tid"] == tid),
            "no thread metadata for {event}"
        );
    }
}

/// `--execution_log_json_file`: SpawnExec messages printed like
/// JsonFormat's default printer with every field; the spawns, their
/// arguments, outputs, digests, exit codes and labels are genuine's.
#[test]
fn the_json_execution_log_matches_genuine() {
    let ws = Workspace::new();
    let output = ws.run(scenario("build"), &[]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let parse = |text: &str| -> Vec<Value> {
        serde_json::Deserializer::from_str(text)
            .into_iter::<Value>()
            .map(Result::unwrap)
            .collect()
    };
    let native_text = fs::read_to_string(ws.capture().join("exec.json")).unwrap();
    assert!(
        native_text.starts_with("{\n  \"commandArgs\": [\"/bin/bash\", \"-c\", "),
        "{native_text}"
    );
    let genuine = parse(&fs::read_to_string(fixture("build.genuine.exec.json")).unwrap());
    let native = parse(&native_text);
    let shape = |spawn: &Value| {
        json!({
            "keys": spawn.as_object().unwrap().keys().collect::<Vec<_>>(),
            "commandArgs": spawn["commandArgs"],
            "environmentVariables": spawn["environmentVariables"],
            "inputs": spawn["inputs"].as_array().unwrap().iter().map(|input| json!([input["path"], input["digest"]["hash"]])).collect::<Vec<_>>(),
            "listedOutputs": spawn["listedOutputs"],
            "actualOutputs": spawn["actualOutputs"],
            "mnemonic": spawn["mnemonic"],
            "exitCode": spawn["exitCode"],
            "targetLabel": spawn["targetLabel"],
            "metrics": spawn["metrics"].as_object().unwrap().keys().collect::<Vec<_>>(),
        })
    };
    let genuine: Vec<Value> = genuine.iter().map(shape).collect();
    let native: Vec<Value> = native.iter().map(shape).collect();
    assert_eq!(native, genuine);
}

/// A minimal protobuf reader: (field number, varint or bytes) pairs.
fn fields(mut bytes: &[u8]) -> Vec<(u32, Result<u64, Vec<u8>>)> {
    fn varint(bytes: &mut &[u8]) -> u64 {
        let (mut value, mut shift) = (0u64, 0);
        loop {
            let byte = bytes[0];
            *bytes = &bytes[1..];
            value |= u64::from(byte & 0x7f) << shift;
            shift += 7;
            if byte < 0x80 {
                return value;
            }
        }
    }
    let mut out = Vec::new();
    while !bytes.is_empty() {
        let key = varint(&mut bytes);
        match key & 7 {
            0 => out.push(((key >> 3) as u32, Ok(varint(&mut bytes)))),
            2 => {
                let length = varint(&mut bytes) as usize;
                out.push(((key >> 3) as u32, Err(bytes[..length].to_vec())));
                bytes = &bytes[length..];
            }
            other => panic!("wire type {other}"),
        }
    }
    out
}

/// The compact log's spawns, resolved: (mnemonic, args, label, exit code,
/// inputs and outputs as (path, digest)).
fn compact_spawns(compressed: &[u8]) -> Vec<Value> {
    let mut stream = Vec::new();
    std::io::Read::read_to_end(
        &mut ruzstd::decoding::StreamingDecoder::new(compressed).unwrap(),
        &mut stream,
    )
    .unwrap();
    let text = |bytes: &[u8]| String::from_utf8(bytes.to_vec()).unwrap();
    let mut rest = &stream[..];
    let mut files: BTreeMap<u64, Value> = BTreeMap::new();
    let mut sets: BTreeMap<u64, Vec<u64>> = BTreeMap::new();
    let mut spawns = Vec::new();
    let mut kinds = Vec::new();
    while !rest.is_empty() {
        let (mut length, mut shift) = (0usize, 0);
        loop {
            let byte = rest[0];
            rest = &rest[1..];
            length |= usize::from(byte & 0x7f) << shift;
            shift += 7;
            if byte < 0x80 {
                break;
            }
        }
        let entry = fields(&rest[..length]);
        rest = &rest[length..];
        let id = entry
            .iter()
            .find_map(|(number, value)| (*number == 1).then(|| *value.as_ref().unwrap()))
            .unwrap_or(0);
        for (number, value) in entry {
            let Err(body) = value else { continue };
            kinds.push(number);
            match number {
                3 => {
                    let file = fields(&body);
                    let path = file
                        .iter()
                        .find_map(|(n, v)| (*n == 1).then(|| text(v.as_ref().unwrap_err())));
                    let digest = file
                        .iter()
                        .find_map(|(n, v)| {
                            (*n == 2).then(|| {
                                fields(v.as_ref().unwrap_err()).iter().find_map(|(n, v)| {
                                    (*n == 1).then(|| text(v.as_ref().unwrap_err()))
                                })
                            })
                        })
                        .flatten();
                    files.insert(id, json!([path, digest]));
                }
                6 => {
                    let mut ids = Vec::new();
                    for (n, v) in fields(&body) {
                        match (n, v) {
                            (5, Ok(single)) => ids.push(single),
                            (5, Err(packed)) => {
                                let mut packed = &packed[..];
                                while !packed.is_empty() {
                                    let (mut value, mut shift) = (0u64, 0);
                                    loop {
                                        let byte = packed[0];
                                        packed = &packed[1..];
                                        value |= u64::from(byte & 0x7f) << shift;
                                        shift += 7;
                                        if byte < 0x80 {
                                            break;
                                        }
                                    }
                                    ids.push(value);
                                }
                            }
                            _ => {}
                        }
                    }
                    sets.insert(id, ids);
                }
                7 => {
                    let spawn = fields(&body);
                    let strings = |field: u32| -> Vec<String> {
                        spawn
                            .iter()
                            .filter(|(n, _)| *n == field)
                            .map(|(_, v)| text(v.as_ref().unwrap_err()))
                            .collect()
                    };
                    let number = |field: u32| {
                        spawn
                            .iter()
                            .find_map(|(n, v)| (*n == field).then(|| *v.as_ref().unwrap()))
                            .unwrap_or(0)
                    };
                    let inputs: Vec<Value> = sets
                        .get(&number(4))
                        .into_iter()
                        .flatten()
                        .map(|id| files[id].clone())
                        .collect();
                    let outputs: Vec<Value> = spawn
                        .iter()
                        .filter(|(n, _)| *n == 6)
                        .map(|(_, v)| {
                            let output = fields(v.as_ref().unwrap_err());
                            let id = output
                                .iter()
                                .find_map(|(n, v)| (*n == 5).then(|| *v.as_ref().unwrap()))
                                .unwrap();
                            files[&id].clone()
                        })
                        .collect();
                    spawns.push(json!({
                        "args": strings(1), "label": strings(7), "mnemonic": strings(8),
                        "exit": number(9), "inputs": inputs, "outputs": outputs,
                    }));
                }
                _ => {}
            }
        }
    }
    assert_eq!(kinds.first(), Some(&2), "the invocation entry first");
    spawns
}

/// `--execution_log_compact_file`: a zstd-compressed ExecLogEntry stream
/// whose spawns resolve to genuine's (arguments, label, inputs and outputs
/// with their digests).
#[test]
fn the_compact_execution_log_matches_genuine() {
    let ws = Workspace::new();
    let output = ws.run(scenario("compact_log"), &[]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let native = compact_spawns(&fs::read(ws.capture().join("exec.compact")).unwrap());
    let genuine = compact_spawns(&fs::read(fixture("compact_log.genuine.compact")).unwrap());
    assert_eq!(native, genuine);
}

/// Off by default: no build event, profile or log file, and no note.
#[test]
fn nothing_is_written_unless_requested() {
    let ws = Workspace::new();
    let output = ws.run(&["build", "//pkg:b"], &[]);
    assert!(output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(!stderr.contains("Build Event Protocol"), "{stderr}");
    let profiles = walk(&ws.path().join("private-cache"))
        .into_iter()
        .filter(|path| path.to_string_lossy().ends_with(".profile.gz"))
        .count();
    assert_eq!(profiles, 0);
    assert_eq!(fs::read_dir(ws.capture()).unwrap().count(), 0);
}

fn walk(root: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    if let Ok(entries) = fs::read_dir(root) {
        for entry in entries.flatten() {
            let path = entry.path();
            if entry.file_type().is_ok_and(|kind| kind.is_dir()) {
                out.extend(walk(&path));
            } else {
                out.push(path);
            }
        }
    }
    out
}

/// The explicit errors: a Build Event Service upload, two execution logs.
#[test]
fn unsupported_and_conflicting_options_fail() {
    let ws = Workspace::new();
    let output = ws.run(
        &["build", "--bes_backend=grpc://localhost:1", "//pkg:a"],
        &[],
    );
    assert_eq!(output.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&output.stderr).contains("--bes_backend=grpc://localhost:1: uploading build events to a Build Event Service (gRPC) is not implemented"));
    let output = ws.run(
        &[
            "build",
            "--execution_log_json_file=a",
            "--execution_log_compact_file=b",
            "//pkg:a",
        ],
        &[],
    );
    assert_eq!(output.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&output.stderr).contains(
        "ERROR: Must specify at most one of --execution_log_binary_file, --execution_log_json_file and --execution_log_compact_file"
    ));
}
