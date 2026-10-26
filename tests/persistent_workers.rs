//! Persistent workers (docs/PERSISTENT_WORKERS.md, crate::worker): every
//! case of tests/fixtures/persistent_workers/reference_receipt.json —
//! genuine Bazel 9.3.0 (`--batch --ignore_all_rc_files`, a removed scratch
//! output user root) on the fixture workspace — runs natively and is
//! compared with the genuine capture: exit code, the built outputs (mode and
//! contents), and the fixture worker's trace of process starts and
//! WorkRequests (worker argv, environment names, TMPDIR, every request's
//! arguments, inputs with digests, request ids and verbosity, the inputs
//! visible in the worker's working directory, the number of processes).
//! Normalized: request order and pid numbering (scheduling), which spawn of
//! a multiplex key gets which non-zero request id, and the order of the
//! inputs list (native lists them by execution path; a declared
//! divergence). Failure messages are checked for genuine's message text.
//! Further native checks: worker spawns and standalone spawns share action
//! keys and produce byte-identical outputs; no worker process outlives the
//! command; a crashed worker is replaced.
#![allow(clippy::disallowed_methods)] // test harness: starts processes directly (clippy.toml)

use std::collections::BTreeMap;
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use base64::Engine as _;
use serde_json::Value;
include!("support/explicit_modules.rs");

fn manifest() -> &'static Path {
    Path::new(env!("CARGO_MANIFEST_DIR"))
}

fn receipt() -> Value {
    serde_json::from_slice(
        &fs::read(manifest().join("tests/fixtures/persistent_workers/reference_receipt.json"))
            .unwrap(),
    )
    .unwrap()
}

struct Workspace(tempfile::TempDir);

impl Workspace {
    fn new() -> Self {
        let base = std::env::var_os("TMPDIR")
            .map(PathBuf::from)
            .unwrap_or_else(std::env::temp_dir);
        let ws = Self(
            tempfile::Builder::new()
                .prefix("persistent-workers-")
                .tempdir_in(base)
                .unwrap(),
        );
        copy(
            &manifest().join("tests/fixtures/persistent_workers/workspace"),
            ws.path(),
        );
        extract_platforms(&ws.path().join("platforms"));
        ws
    }

    fn path(&self) -> &Path {
        self.0.path()
    }

    /// A fresh trace directory (`--define=trace=`).
    fn trace(&self, name: &str) -> PathBuf {
        let trace = self.path().join(format!("trace-{name}"));
        fs::create_dir_all(&trace).unwrap();
        trace
    }

    /// `build --keep_going ARGS --define=trace=TRACE` with `cache`.
    fn build(&self, cache: &str, trace: &Path, args: &[String]) -> Output {
        let registry = format!(
            "--registry=file://{}/tests/fixtures/builtin_modules/registry",
            manifest().display()
        );
        let output = Command::new("timeout")
            .args(["--kill-after=5s", "600s", env!("CARGO_BIN_EXE_bazel-rs")])
            .arg("--ignore_all_rc_files")
            .arg(format!("--cache-dir={cache}"))
            .args([
                "build",
                "--experimental-builtin-modules",
                &registry,
                "--keep_going",
            ])
            .args(args)
            .arg(format!("--define=trace={}", trace.display()))
            .current_dir(self.path())
            .explicit_modules()
            .output()
            .unwrap();
        assert_ne!(output.status.code(), Some(124), "CLI timed out: {args:?}");
        output
    }
}

fn copy(source: &Path, destination: &Path) {
    fs::create_dir_all(destination).unwrap();
    for entry in fs::read_dir(source).unwrap() {
        let entry = entry.unwrap();
        let target = destination.join(entry.file_name());
        if entry.file_type().unwrap().is_dir() {
            copy(&entry.path(), &target);
        } else {
            fs::copy(entry.path(), target).unwrap();
        }
    }
}

/// The genuine platforms 1.0.0 module archive (tests/exec_transition.rs).
fn extract_platforms(to: &Path) {
    let manifest: Value = serde_json::from_slice(
        &fs::read(manifest().join("tests/fixtures/label_repo_name/platforms_archive.json"))
            .unwrap(),
    )
    .unwrap();
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(manifest["bytes_base64"].as_str().unwrap())
        .unwrap();
    let mut archive = tar::Archive::new(flate2::read::GzDecoder::new(bytes.as_slice()));
    for entry in archive.entries().unwrap() {
        let mut entry = entry.unwrap();
        let path = to.join(entry.path().unwrap());
        if entry.header().entry_type().is_dir() {
            fs::create_dir_all(&path).unwrap();
            continue;
        }
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        let mut content = Vec::new();
        entry.read_to_end(&mut content).unwrap();
        fs::write(path, content).unwrap();
    }
}

/// The built `.out` outputs (file name -> [mode, contents]) of the native
/// `LABEL\tPATH` result lines.
fn outputs(stdout: &str) -> BTreeMap<String, Value> {
    use std::os::unix::fs::PermissionsExt as _;
    stdout
        .lines()
        .filter_map(|line| line.split_once('\t'))
        .map(|(_, path)| Path::new(path))
        .filter(|path| path.extension().is_some_and(|extension| extension == "out"))
        .map(|path| {
            let mode = fs::metadata(path).unwrap().permissions().mode() & 0o777;
            (
                path.file_name().unwrap().to_string_lossy().into_owned(),
                serde_json::json!([format!("0o{mode:o}"), fs::read_to_string(path).unwrap()]),
            )
        })
        .collect()
}

/// The fixture worker's trace with `trace` and `ws` replaced like the
/// receipt's, pids kept (callers normalize).
fn read_trace(trace: &Path, ws: &Path, tmp: &str) -> Vec<Value> {
    let Ok(text) = fs::read_to_string(trace.join("trace.jsonl")) else {
        return Vec::new();
    };
    text.lines()
        .map(|line| {
            let line = line
                .replace(&trace.display().to_string(), "<TRACE>")
                .replace(&ws.display().to_string(), "<WS>")
                .replace(tmp, "<TMP>");
            serde_json::from_str(&line).unwrap()
        })
        .collect()
}

/// The comparable trace: the number of processes, then every record with
/// its pid replaced by its process's startup args, the per-process request
/// counter dropped, the raw protocol bytes dropped, inputs sorted by path
/// and a multiplex request id reduced to "nonzero" (pw-compare.py).
fn normalized(trace: &[Value]) -> (usize, Vec<String>) {
    let starts: BTreeMap<String, String> = trace
        .iter()
        .filter(|record| record["event"] == "start")
        .map(|record| (record["pid"].to_string(), record["args"].to_string()))
        .collect();
    let mut records: Vec<String> = trace
        .iter()
        .map(|record| {
            let mut record = record.clone();
            let pid = record["pid"].to_string();
            let object = record.as_object_mut().unwrap();
            object.insert(
                "pid".into(),
                Value::String(starts.get(&pid).cloned().unwrap_or_default()),
            );
            object.remove("handled");
            if let Some(request) = object.get_mut("request").and_then(Value::as_object_mut) {
                request.remove("line");
                request.remove("wire");
                let nonzero = request["requestId"].as_i64().unwrap_or(0) != 0;
                request.insert(
                    "requestId".into(),
                    if nonzero {
                        Value::String("nonzero".into())
                    } else {
                        Value::from(0)
                    },
                );
                let inputs = request.get_mut("inputs").unwrap().as_array_mut().unwrap();
                inputs.sort_by_key(|input| input["path"].as_str().unwrap_or("").to_owned());
            }
            serde_json::to_string(&record).unwrap()
        })
        .collect();
    records.sort();
    (starts.len(), records)
}

/// Genuine's message text each failing case must show natively (also
/// asserted against the genuine stderr, so the expectations stay pinned to
/// the receipt).
fn expected_messages(name: &str) -> &'static [&'static str] {
    match name {
        "fail" | "proto_fail" => &["boom: work failed"],
        "crash" => &[
            "Worker process did not return a WorkResponse:\n\n---8<---8<--- Start of log, file at ",
            "---8<---8<---\nworker crashing\n---8<---8<--- End of log ---8<---8<---",
        ],
        "garbage" => &[
            "Worker process returned an unparseable WorkResponse!\n\nDid you try to print \
             something to stdout? Workers aren't allowed to do this, as it breaks the protocol \
             between Bazel and the worker process.\n\n---8<---8<--- Start of response \
             ---8<---8<---\nthis is not a response\n---8<---8<--- End of response ---8<---8<---",
        ],
        "noflagfile" => &[
            "Worker strategy cannot execute this FixtureWork action, because the command-line \
             arguments do not contain exactly one @flagfile or --flagfile=\n[pkg/worker.py, \
             --startup_flag=a, --in, pkg/a.txt, --out, \
             bazel-out/k8-fastbuild/bin/pkg/json_noflagfile.out, --mode, upper]",
        ],
        "bad_protocol" => &[
            "Exec failed due to IOException: requires-worker-protocol must be set to a valid \
             worker protocol format: json or proto",
        ],
        "force_worker_plain" => &[
            "FixtureWork spawn cannot be executed with any of the available strategies: \
             [worker]. Your --spawn_strategy, --genrule_strategy, --strategy and/or \
             --allowed_strategies_by_exec_platform flags are probably too strict.",
        ],
        "noisy" => &["noisy worker says hello"],
        "verbose" => {
            &["INFO: Created new non-sandboxed singleplex FixtureWork worker (id 1, key hash "]
        }
        _ => &[],
    }
}

fn check(name: &str, case: &Value) {
    let ws = Workspace::new();
    let trace = ws.trace(name);
    let argv: Vec<String> = case["argv"]
        .as_array()
        .unwrap()
        .iter()
        .skip(1)
        .map(|arg| arg.as_str().unwrap().to_owned())
        .collect();
    let output = ws.build("cache", &trace, &argv);
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    let context = format!("case {name}:\nstdout:\n{stdout}\nstderr:\n{stderr}");
    assert_eq!(
        output.status.code(),
        case["exit"].as_i64().map(|code| code as i32),
        "{context}"
    );
    let expected: BTreeMap<String, Value> = case["outputs"]
        .as_object()
        .unwrap()
        .iter()
        .map(|(name, value)| (name.clone(), value.clone()))
        .collect();
    if output.status.success() {
        assert_eq!(outputs(&stdout), expected, "{context}");
    } else {
        assert!(expected.is_empty(), "{context}");
    }
    let tmp = std::env::var("TMPDIR").unwrap_or_else(|_| "/tmp".into());
    let native = read_trace(&trace, ws.path(), &tmp);
    let genuine: Vec<Value> = case["trace"].as_array().unwrap().clone();
    assert_eq!(normalized(&native), normalized(&genuine), "{context}");
    let genuine_stderr = case["stderr"].as_str().unwrap();
    for message in expected_messages(name) {
        assert!(
            genuine_stderr.contains(message),
            "receipt {name}: {message}"
        );
        assert!(stderr.contains(message), "{context}\nmissing: {message}");
    }
    // No worker process outlives the command (WorkerPool.close).
    for record in &native {
        if record["event"] == "start" {
            let pid = record["pid"].as_i64().unwrap();
            assert!(
                !Path::new(&format!("/proc/{pid}")).exists()
                    || fs::read_to_string(format!("/proc/{pid}/stat"))
                        .is_ok_and(|stat| stat.contains(") Z ")),
                "{context}\nworker {pid} is still running"
            );
        }
    }
}

fn run_cases(names: &[&str]) {
    let receipt = receipt();
    for name in names {
        check(name, &receipt["cases"][*name]);
    }
}

#[test]
fn receipt_covers_every_case_once() {
    let receipt = receipt();
    let mut covered: Vec<&str> = GROUPS
        .iter()
        .flat_map(|(_, names)| names.iter().copied())
        .collect();
    covered.sort_unstable();
    let mut cases: Vec<&str> = receipt["cases"]
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect();
    cases.sort_unstable();
    assert_eq!(covered, cases);
}

const GROUPS: &[(&str, &[&str])] = &[
    (
        "protocols",
        &[
            "json_one_instance",
            "proto_one_instance",
            "two_keys",
            "data_inputs",
            "tree_input",
            "escape",
            "keyed",
        ],
    ),
    (
        "failures",
        &[
            "noisy",
            "fail",
            "crash",
            "garbage",
            "proto_fail",
            "noflagfile",
            "bad_protocol",
        ],
    ),
    (
        "strategies",
        &[
            "not_one",
            "standalone",
            "sandboxed_strategy",
            "force_worker_plain",
            "allowlist_excludes",
        ],
    ),
    (
        "options",
        &[
            "multiplex",
            "no_multiplex",
            "worker_sandboxing",
            "extra_flag",
            "strict_flagfiles",
            "verbose",
        ],
    ),
];

fn group(name: &str) -> &'static [&'static str] {
    GROUPS.iter().find(|(group, _)| *group == name).unwrap().1
}

#[test]
fn protocols_match_genuine() {
    run_cases(group("protocols"));
}

#[test]
fn failures_match_genuine() {
    run_cases(group("failures"));
}

#[test]
fn strategies_match_genuine() {
    run_cases(group("strategies"));
}

#[test]
fn options_match_genuine() {
    run_cases(group("options"));
}

/// The strategy never enters the action key: a worker build's entries are
/// cache hits for a standalone build, and a standalone build in a fresh
/// cache produces byte-identical outputs.
#[test]
fn worker_outputs_equal_standalone_outputs_and_keys() {
    let ws = Workspace::new();
    let trace = ws.trace("keys");
    let targets: Vec<String> = ["//pkg:json_a", "//pkg:proto_b", "//pkg:json_tree"]
        .iter()
        .map(|target| (*target).to_owned())
        .collect();
    let worker = ws.build("cache-w", &trace, &targets);
    assert!(
        worker.status.success(),
        "{}",
        String::from_utf8_lossy(&worker.stderr)
    );
    let worker_stdout = String::from_utf8_lossy(&worker.stdout).into_owned();
    assert!(fs::read_to_string(trace.join("trace.jsonl")).is_ok());
    let mut standalone_args = vec!["--spawn_strategy=standalone".to_owned()];
    standalone_args.extend(targets.iter().cloned());
    let hit = ws.build("cache-w", &trace, &standalone_args);
    let hit_stderr = String::from_utf8_lossy(&hit.stderr);
    assert!(hit.status.success(), "{hit_stderr}");
    assert!(hit_stderr.contains("0 actions executed"), "{hit_stderr}");
    let fresh = ws.build("cache-s", &trace, &standalone_args);
    assert!(
        fresh.status.success(),
        "{}",
        String::from_utf8_lossy(&fresh.stderr)
    );
    let keyed = |stdout: &str| -> Vec<(String, Vec<u8>)> {
        stdout
            .lines()
            .filter_map(|line| line.split_once('\t'))
            .map(|(label, path)| {
                let key = path.split("/actions/").nth(1).unwrap_or(path);
                (format!("{label} {key}"), fs::read(path).unwrap_or_default())
            })
            .collect()
    };
    assert_eq!(
        keyed(&worker_stdout),
        keyed(&String::from_utf8_lossy(&fresh.stdout))
    );
}

/// A worker that dies without a response fails its spawn and is replaced:
/// the key's other spawns still build (on a new process when the crash
/// came first).
#[test]
fn crashed_worker_is_replaced() {
    let ws = Workspace::new();
    let trace = ws.trace("restart");
    let args: Vec<String> = [
        "--worker_max_instances=1",
        "--jobs=1",
        "//pkg:json_crash",
        "//pkg:json_a",
        "//pkg:json_b",
    ]
    .iter()
    .map(|arg| (*arg).to_owned())
    .collect();
    let output = ws.build("cache", &trace, &args);
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(output.status.code(), Some(1), "{stderr}");
    assert!(
        stderr.contains("Worker process did not return a WorkResponse:"),
        "{stderr}"
    );
    let built = outputs(&stdout);
    assert_eq!(
        built.keys().cloned().collect::<Vec<_>>(),
        ["json_a.out", "json_b.out"],
        "{stdout}\n{stderr}"
    );
    let tmp = std::env::var("TMPDIR").unwrap_or_else(|_| "/tmp".into());
    let records = read_trace(&trace, ws.path(), &tmp);
    let order: Vec<String> = records
        .iter()
        .map(|record| match record["event"].as_str().unwrap() {
            "start" => "start".to_owned(),
            _ => record["request"]["arguments"][3]
                .as_str()
                .unwrap()
                .rsplit('/')
                .next()
                .unwrap()
                .to_owned(),
        })
        .collect();
    // A start before every request that follows the crash.
    let crash = order
        .iter()
        .position(|entry| entry == "json_crash.out")
        .unwrap();
    if crash + 1 < order.len() {
        assert_eq!(order[crash + 1], "start", "{order:?}");
    }
}

/// Worker spawns feed the build event files, the profile and the execution
/// log like any spawn (docs/BEP.md, docs/PROFILE.md): an ActionExecuted per
/// executed worker action (`--build_event_publish_all_actions`), an
/// `action processing` span with its mnemonic, a SpawnExec with runner
/// `worker`.
#[test]
fn worker_spawns_reach_the_build_events_the_profile_and_the_execution_log() {
    let ws = Workspace::new();
    let trace = ws.trace("reports");
    let capture = ws.path().join("cap");
    fs::create_dir_all(&capture).unwrap();
    let args: Vec<String> = vec![
        "--build_event_publish_all_actions".to_owned(),
        format!("--build_event_json_file={}/bep.json", capture.display()),
        format!("--profile={}/profile.json", capture.display()),
        format!("--execution_log_json_file={}/exec.json", capture.display()),
        "//pkg:json_a".to_owned(),
    ];
    let output = ws.build("cache", &trace, &args);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let spawns: Vec<Value> =
        serde_json::Deserializer::from_str(&fs::read_to_string(capture.join("exec.json")).unwrap())
            .into_iter::<Value>()
            .map(Result::unwrap)
            .collect();
    let worker: Vec<&Value> = spawns
        .iter()
        .filter(|spawn| spawn["runner"] == "worker")
        .collect();
    assert_eq!(worker.len(), 1, "{spawns:#?}");
    let mnemonic = worker[0]["mnemonic"].as_str().unwrap().to_owned();
    assert_eq!(worker[0]["exitCode"], 0);
    assert!(!worker[0]["actualOutputs"].as_array().unwrap().is_empty());
    let events: Vec<Value> = fs::read_to_string(capture.join("bep.json"))
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    assert!(
        events
            .iter()
            .any(|event| event["action"]["type"] == mnemonic.as_str()
                && event["action"]["success"] == true),
        "{events:#?}"
    );
    let profile: Value =
        serde_json::from_str(&fs::read_to_string(capture.join("profile.json")).unwrap()).unwrap();
    assert!(
        profile["traceEvents"]
            .as_array()
            .unwrap()
            .iter()
            .any(|event| event["cat"] == "action processing"
                && event["args"]["mnemonic"] == mnemonic.as_str()),
        "{profile:#}"
    );
}
