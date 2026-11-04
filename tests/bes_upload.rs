//! The Build Event Service upload (`--bes_backend`, docs/BEP.md "The Build
//! Event Service", crate::bep::bes) against the in-process loopback server
//! of tests/support (which records every PublishLifecycleEvent request and
//! the requests of every PublishBuildToolEventStream call). The ignored
//! paired check runs genuine Bazel 9.3 against the same server.
#![allow(clippy::disallowed_methods)] // test harness: starts processes directly (clippy.toml)
use bazel_rs::reapi::{self, Value};
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::atomic::{AtomicUsize, Ordering};

include!("support/verify_env.rs");
include!("support/explicit_modules.rs");
include!("support/grpc_cache_server.rs");

use grpc_cache_server::{Config, Server, fs_canonical_tmp};
include!("support/sealed_trees.rs");

static NEXT: AtomicUsize = AtomicUsize::new(0);

const BUILD: &str = "genrule(name = \"gen\", outs = [\"gen.txt\"], cmd = \"echo hi > $@\")\ngenrule(name = \"bad\", outs = [\"bad.txt\"], cmd = \"exit 3\")\n";

struct Workspace(PathBuf);

impl Workspace {
    fn new() -> Self {
        let root = fs_canonical_tmp().join(format!(
            "native-bes-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = sealed_trees::remove_tree(&root);
        fs::create_dir_all(root.join("ws/pkg")).unwrap();
        fs::create_dir_all(root.join("home")).unwrap();
        fs::write(root.join("ws/MODULE.bazel"), "module(name = \"bes\")\n").unwrap();
        fs::write(root.join("ws/pkg/BUILD.bazel"), BUILD).unwrap();
        Self(root)
    }

    fn run(&self, cache: &str, args: &[&str]) -> Output {
        let mut command = Command::new(env!("CARGO_BIN_EXE_bazel-rs"));
        command
            .arg("--ignore_all_rc_files")
            .arg(format!("--cache-dir={}", self.0.join(cache).display()))
            .args(args)
            .current_dir(self.0.join("ws"))
            .env_clear()
            .envs(verifier_environment())
            .env("PATH", "/usr/bin:/bin")
            .env("HOME", self.0.join("home"));
        command.explicit_modules().output().unwrap()
    }
}

impl Drop for Workspace {
    fn drop(&mut self) {
        let _ = sealed_trees::remove_tree(&self.0);
    }
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr)
        .lines()
        .filter(|line| !line.starts_with("bazel-rs: memory limit defaulted to "))
        .map(|line| format!("{line}\n"))
        .collect()
}

fn bytes_fields(bytes: &[u8], number: u64) -> Vec<Vec<u8>> {
    reapi::fields(bytes)
        .unwrap()
        .into_iter()
        .filter_map(|field| match field {
            (n, Value::Bytes(value)) if n == number => Some(value.to_vec()),
            _ => None,
        })
        .collect()
}

fn field(bytes: &[u8], number: u64) -> Option<Vec<u8>> {
    bytes_fields(bytes, number).pop()
}

fn varint(bytes: &[u8], number: u64) -> u64 {
    reapi::fields(bytes)
        .unwrap()
        .into_iter()
        .find_map(|field| match field {
            (n, Value::Varint(value)) if n == number => Some(value),
            _ => None,
        })
        .unwrap_or(0)
}

fn strings(bytes: &[u8], number: u64) -> Vec<String> {
    bytes_fields(bytes, number)
        .into_iter()
        .map(|value| String::from_utf8(value).unwrap())
        .collect()
}

/// The field numbers of a message, in encoding order.
fn numbers(bytes: &[u8]) -> Vec<u64> {
    reapi::fields(bytes)
        .unwrap()
        .into_iter()
        .map(|(number, _)| number)
        .collect()
}

/// One decoded OrderedBuildEvent.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
struct Ordered {
    build_id: String,
    invocation_id: String,
    component: u64,
    sequence: u64,
    /// The BuildEvent oneof field number (51 started, 52 attempt finished,
    /// 53 enqueued, 55 build finished, 59 stream finished, 60 bazel event).
    kind: u64,
    /// The kind's payload (a bazel event's Any).
    payload: Vec<u8>,
    has_time: bool,
}

fn ordered(bytes: &[u8]) -> Ordered {
    let stream_id = field(bytes, 1).unwrap_or_default();
    let event = field(bytes, 3).unwrap();
    let kind = numbers(&event)
        .into_iter()
        .find(|number| *number != 1)
        .unwrap();
    Ordered {
        build_id: strings(&stream_id, 1).pop().unwrap_or_default(),
        invocation_id: strings(&stream_id, 6).pop().unwrap_or_default(),
        component: varint(&stream_id, 3),
        sequence: varint(bytes, 2),
        kind,
        payload: field(&event, kind as u32 as u64).unwrap_or_default(),
        has_time: field(&event, 1).is_some(),
    }
}

/// A PublishLifecycleEventRequest: service level, the event, keywords,
/// project id, field order.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
struct Lifecycle {
    service_level: u64,
    event: Ordered,
    keywords: Vec<String>,
    project_id: Option<String>,
    fields: Vec<u64>,
    /// BuildStatus.result of a finished event.
    result: Option<u64>,
}

fn lifecycle(bytes: &[u8]) -> Lifecycle {
    let event = ordered(&field(bytes, 2).unwrap());
    let result = match event.kind {
        52 => Some(varint(&field(&event.payload, 3).unwrap_or_default(), 1)),
        55 => Some(varint(&field(&event.payload, 1).unwrap_or_default(), 1)),
        _ => None,
    };
    Lifecycle {
        service_level: varint(bytes, 1),
        keywords: strings(bytes, 4),
        project_id: strings(bytes, 6).pop(),
        fields: numbers(bytes),
        result,
        event,
    }
}

/// A PublishBuildToolEventStreamRequest.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
struct StreamRequest {
    event: Ordered,
    keywords: Vec<String>,
    project_id: Option<String>,
    fields: Vec<u64>,
    /// A bazel event's Any: type URL and value.
    type_url: Option<String>,
    #[serde(skip)]
    value: Vec<u8>,
}

fn stream_request(bytes: &[u8]) -> StreamRequest {
    let event = ordered(&field(bytes, 4).unwrap());
    let (type_url, value) = if event.kind == 60 {
        (
            strings(&event.payload, 1).pop(),
            field(&event.payload, 2).unwrap_or_default(),
        )
    } else {
        (None, Vec::new())
    };
    StreamRequest {
        keywords: strings(bytes, 5),
        project_id: strings(bytes, 6).pop(),
        fields: numbers(bytes),
        type_url,
        value,
        event,
    }
}

/// The length-delimited events of a binary BEP file.
fn delimited(bytes: &[u8]) -> Vec<Vec<u8>> {
    let mut out = Vec::new();
    let mut position = 0;
    while position < bytes.len() {
        let mut length = 0usize;
        let mut shift = 0;
        loop {
            let byte = bytes[position];
            position += 1;
            length |= usize::from(byte & 0x7f) << shift;
            if byte & 0x80 == 0 {
                break;
            }
            shift += 7;
        }
        out.push(bytes[position..position + length].to_vec());
        position += length;
    }
    out
}

/// The BuildEventId oneof field of a BuildEvent (its `id`'s only field).
fn event_kind(event: &[u8]) -> u64 {
    numbers(&field(event, 1).unwrap())[0]
}

#[test]
fn the_stream_carries_the_build_events_between_lifecycle_events() {
    let ws = Workspace::new();
    let server = Server::with_config(Config::default());
    let backend = format!("--bes_backend={}", server.url());
    let binary = ws.0.join("events.bin");
    let built = ws.run(
        "c1",
        &[
            "build",
            &backend,
            "--bes_keywords=k1,k2",
            "--bes_system_keywords=sys=1",
            "--bes_instance_name=inst",
            "--bes_header=x-bes=v1",
            "--bes_header=x-bes=v2",
            "--bes_results_url=http://results/",
            &format!("--build_event_binary_file={}", binary.display()),
            "//pkg:gen",
        ],
    );
    assert!(built.status.success(), "{}", stderr(&built));
    let text = stderr(&built);
    // BuildStarted.uuid is the invocation id of the URL (twice: when the
    // transport starts and when it closed; no files message with a BES).
    let (lifecycle_requests, streams) = server.bes();
    let events: Vec<Lifecycle> = lifecycle_requests.iter().map(|r| lifecycle(r)).collect();
    let invocation = events[1].event.invocation_id.clone();
    let url = format!("INFO: Streaming build results to: http://results/{invocation}\n");
    assert_eq!(text.matches(&url).count(), 2, "{text}");
    assert!(
        !text.contains("Build Event Protocol files produced"),
        "{text}"
    );
    // BuildEnqueued, InvocationAttemptStarted, InvocationAttemptFinished,
    // BuildFinished (BuildEventServiceUploader.run).
    let kinds: Vec<(u64, u64, u64)> = events
        .iter()
        .map(|l| (l.event.kind, l.event.sequence, l.event.component))
        .collect();
    assert_eq!(kinds, [(53, 1, 1), (51, 1, 1), (52, 2, 1), (55, 2, 1)]);
    let keywords = [
        "protocol_name=BEP",
        "command_name=build",
        "sys=1",
        "user_keyword=k1",
        "user_keyword=k2",
    ];
    for (index, request) in events.iter().enumerate() {
        assert_eq!(request.service_level, 1);
        assert_eq!(request.project_id.as_deref(), Some("inst"));
        // Attempt events carry the invocation id; InvocationAttemptFinished
        // carries no keywords.
        assert_eq!(
            request.event.invocation_id.is_empty(),
            index == 0 || index == 3
        );
        assert_eq!(request.keywords.is_empty(), index == 2, "{request:?}");
        if !request.keywords.is_empty() {
            assert_eq!(request.keywords, keywords);
        }
        assert!(request.event.has_time);
    }
    assert_eq!(events[2].result, Some(1));
    assert_eq!(events[3].result, Some(1));
    let build_id = events[0].event.build_id.clone();
    assert!(events.iter().all(|l| l.event.build_id == build_id));
    assert_ne!(build_id, invocation);
    // One stream: sequence numbers 1.., TOOL, every BuildEvent of the binary
    // file as a bazel_event in order, then component_stream_finished.
    assert_eq!(streams.len(), 1);
    let requests: Vec<StreamRequest> = streams[0].iter().map(|r| stream_request(r)).collect();
    let file_events = delimited(&fs::read(&binary).unwrap());
    assert_eq!(requests.len(), file_events.len() + 1);
    for (index, request) in requests.iter().enumerate() {
        assert_eq!(request.event.sequence, index as u64 + 1);
        assert_eq!(request.event.component, 3);
        assert_eq!(request.event.invocation_id, invocation);
        assert_eq!(request.event.build_id, build_id);
        assert_eq!(request.project_id.as_deref(), Some("inst"));
        if index == 0 {
            assert_eq!(request.keywords, keywords);
            assert_eq!(request.fields, [4, 5, 5, 5, 5, 5, 6]);
        } else {
            assert!(request.keywords.is_empty());
        }
        match file_events.get(index) {
            Some(event) => {
                assert_eq!(request.event.kind, 60);
                assert_eq!(
                    request.type_url.as_deref(),
                    Some("type.googleapis.com/build_event_stream.BuildEvent")
                );
                assert_eq!(&request.value, event);
            }
            None => {
                assert_eq!(request.event.kind, 59);
                // BuildComponentStreamFinished.FINISHED.
                assert_eq!(varint(&request.event.payload, 1), 1);
            }
        }
    }
    // BuildStarted first, with the same uuid.
    assert_eq!(event_kind(&requests[0].value), 3);
    // Headers: the last --bes_header value, RequestMetadata.
    let log = server.take_log();
    for call in log
        .iter()
        .filter(|call| call.method.starts_with("google.devtools.build.v1."))
    {
        assert_eq!(call.header("x-bes"), ["v2"], "{call:?}");
        assert_eq!(
            call.header("build.bazel.remote.execution.v2.requestmetadata-bin")
                .len(),
            1
        );
    }
    // A failed build: FAILED statuses; the command keeps its exit code.
    let failed = ws.run("c1", &["build", &backend, "//pkg:bad"]);
    assert_eq!(failed.status.code(), Some(1), "{}", stderr(&failed));
    let (lifecycle_requests, _) = server.bes();
    let results: Vec<Option<u64>> = lifecycle_requests[4..]
        .iter()
        .map(|r| lifecycle(r).result)
        .collect();
    assert_eq!(results, [None, None, Some(2), Some(2)]);
    // Credential helpers: grpc-java's per-service URI, the helper's
    // headers on every BES call.
    let helper = ws.0.join("helper");
    let helper_log = ws.0.join("helper.log");
    fs::write(
        &helper,
        format!(
            "#!/bin/bash\ncat >> {log}\necho >> {log}\necho '{{\"headers\":{{\"Authorization\":[\"Bearer t\"]}}}}'\n",
            log = helper_log.display()
        ),
    )
    .unwrap();
    {
        use std::os::unix::fs::PermissionsExt as _;
        fs::set_permissions(&helper, fs::Permissions::from_mode(0o755)).unwrap();
    }
    let scope = format!("--credential_helper=127.0.0.1={}", helper.display());
    server.take_log();
    let authorized = ws.run("c1", &["build", &backend, &scope, "//pkg:gen"]);
    assert!(authorized.status.success(), "{}", stderr(&authorized));
    let log = server.take_log();
    let calls: Vec<_> = log
        .iter()
        .filter(|call| call.method.starts_with("google.devtools.build.v1."))
        .collect();
    assert_eq!(calls.len(), 5);
    assert!(
        calls
            .iter()
            .all(|call| call.header("authorization") == ["Bearer t"]),
        "{calls:?}"
    );
    assert_eq!(
        fs::read_to_string(&helper_log).unwrap(),
        format!(
            "{{\"uri\":\"https://127.0.0.1:{}/google.devtools.build.v1.PublishBuildEvent\"}}\n",
            server.port
        )
    );
    // --nobes_lifecycle_events: the stream only.
    let quiet = ws.run(
        "c1",
        &["build", &backend, "--nobes_lifecycle_events", "//pkg:gen"],
    );
    assert!(quiet.status.success(), "{}", stderr(&quiet));
    let (lifecycle_requests, streams) = server.bes();
    assert_eq!(lifecycle_requests.len(), 12);
    assert_eq!(streams.len(), 4);
}

#[test]
fn streams_retry_unacknowledged_events_and_failures_set_exit_codes() {
    let ws = Workspace::new();
    let server = Server::with_config(Config::default());
    let backend = format!("--bes_backend={}", server.url());
    let fast = "--experimental_build_event_upload_retry_minimum_delay=10ms";
    // A transient failure after two ACKs: the next stream resends from the
    // third event on.
    server.configure(|config| {
        config
            .bes_stream_failures
            .push_back((2, 14, "flaky".into()));
    });
    let built = ws.run("c1", &["build", &backend, fast, "//pkg:gen"]);
    assert!(built.status.success(), "{}", stderr(&built));
    let (_, streams) = server.bes();
    assert_eq!(streams.len(), 2);
    let first: Vec<u64> = streams[0]
        .iter()
        .map(|r| stream_request(r).event.sequence)
        .collect();
    let second: Vec<u64> = streams[1]
        .iter()
        .map(|r| stream_request(r).event.sequence)
        .collect();
    assert_eq!(second[0], 3, "{first:?} {second:?}");
    assert_eq!(
        stream_request(streams[1].last().unwrap()).event.kind,
        59,
        "{second:?}"
    );
    // A non-retryable status: PERSISTENT_BUILD_EVENT_SERVICE_UPLOAD_ERROR.
    server.configure(|config| {
        config.bes_stream_failures.push_back((0, 3, "bad".into()));
    });
    let failed = ws.run("c1", &["build", &backend, fast, "//pkg:gen"]);
    assert_eq!(failed.status.code(), Some(45), "{}", stderr(&failed));
    assert!(
        stderr(&failed).contains(
            "ERROR: The Build Event Protocol upload failed: Not retrying publishBuildEvents: status='Status{code=INVALID_ARGUMENT, description=bad, cause=null}' INVALID_ARGUMENT: INVALID_ARGUMENT: bad INVALID_ARGUMENT: INVALID_ARGUMENT: bad\n"
        ),
        "{}",
        stderr(&failed)
    );
    // Retries exhausted: TRANSIENT (38).
    server.configure(|config| {
        for _ in 0..3 {
            config.bes_stream_failures.push_back((0, 14, "down".into()));
        }
    });
    let failed = ws.run(
        "c1",
        &[
            "build",
            &backend,
            fast,
            "--build_event_upload_max_retries=2",
            "//pkg:gen",
        ],
    );
    assert_eq!(failed.status.code(), Some(38), "{}", stderr(&failed));
    assert!(
        stderr(&failed).contains(
            "ERROR: The Build Event Protocol upload failed: Not retrying publishBuildEvents, no more attempts left: status='Status{code=UNAVAILABLE, description=down, cause=null}' UNAVAILABLE: UNAVAILABLE: down UNAVAILABLE: UNAVAILABLE: down\n"
        ),
        "{}",
        stderr(&failed)
    );
    // The upload modes that do not wait keep the command's exit code.
    server.configure(|config| {
        config.bes_stream_failures.push_back((0, 3, "bad".into()));
    });
    let nowait = ws.run(
        "c1",
        &[
            "build",
            &backend,
            fast,
            "--bes_upload_mode=nowait_for_upload_complete",
            "//pkg:gen",
        ],
    );
    assert!(nowait.status.success(), "{}", stderr(&nowait));
    // A lifecycle failure that is not retried.
    server.configure(|config| {
        config.fail_method = Some((
            "google.devtools.build.v1.PublishBuildEvent/PublishLifecycleEvent".into(),
            7,
            "denied".into(),
        ));
    });
    let denied = ws.run("c1", &["build", &backend, fast, "//pkg:gen"]);
    assert_eq!(denied.status.code(), Some(45), "{}", stderr(&denied));
    assert!(
        stderr(&denied).contains(
            "ERROR: The Build Event Protocol upload failed: Not retrying publishLifecycleEvent: status='Status{code=PERMISSION_DENIED, description=denied, cause=null}' PERMISSION_DENIED: PERMISSION_DENIED: denied PERMISSION_DENIED: PERMISSION_DENIED: denied\n"
        ),
        "{}",
        stderr(&denied)
    );
    // Option errors.
    let bad = ws.run(
        "c1",
        &["build", &backend, "--bes_upload_mode=x", "//pkg:gen"],
    );
    assert!(!bad.status.success());
    assert!(
        stderr(&bad).contains("While parsing option --bes_upload_mode=x: Not a valid Mode for uploading to the Build Event Service: 'x'"),
        "{}",
        stderr(&bad)
    );
}

// --- The paired check with genuine Bazel 9.3 (ignored; run explicitly) ---
//
// BAZEL_RS_GENUINE_BAZEL=<genuine 9.3.0 ELF> [BAZEL_RS_BES_RECEIPT=<out.json>]
// cargo test --test bes_upload -- --ignored
//
// Both engines build //pkg:gen against a fresh in-process server with the
// same BES flags (genuine `--batch`, a private output user root, the
// offline built-in registry fixture; native the built-in profile with the
// same registry). Compared: the lifecycle requests (kinds, sequence
// numbers, components, which carry invocation ids and keywords, the
// service level, project id and field order, the BuildStatus results), the
// stream envelope (sequence numbers, components, keywords and field order
// of the first request, the Any type URL, the final
// component_stream_finished), the BuildEvent id kinds streamed, the error
// texts and exit codes of a non-retryable stream failure.

fn genuine_run(binary: &str, ws: &Workspace, root: &str, registry: &str, args: &[&str]) -> Output {
    let mut command = Command::new(binary);
    command
        .arg("--batch")
        .arg("--ignore_all_rc_files")
        .arg(format!("--output_user_root={}", ws.0.join(root).display()))
        .arg(args[0])
        .arg(format!("--registry={registry}"))
        .args(&args[1..])
        .current_dir(ws.0.join("ws"))
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .env("HOME", ws.0.join("home"));
    command.output().unwrap()
}

fn native_run(ws: &Workspace, cache: &str, registry: &str, args: &[&str]) -> Output {
    let mut all = vec![args[0], "--experimental-builtin-modules"];
    let registry = format!("--registry={registry}");
    all.push(&registry);
    all.extend(&args[1..]);
    ws.run(cache, &all)
}

/// One lifecycle request as compared: kind, sequence, component, invocation
/// id and keywords present, field order, BuildStatus result.
type LifecycleShape = (u64, u64, u64, bool, bool, Vec<u64>, Option<u64>);

/// What the comparison reads of one engine's upload.
#[derive(Debug, serde::Serialize)]
struct Upload {
    lifecycle: Vec<LifecycleShape>,
    keywords: Vec<String>,
    first_fields: Vec<u64>,
    sequences_contiguous: bool,
    components: Vec<u64>,
    type_urls: Vec<String>,
    last_kind: u64,
    event_kinds: BTreeMap<u64, usize>,
    streams: usize,
}

fn upload(server: &Server) -> Upload {
    let (lifecycle_requests, streams) = server.bes();
    let lifecycle_events: Vec<Lifecycle> =
        lifecycle_requests.iter().map(|r| lifecycle(r)).collect();
    let requests: Vec<StreamRequest> = streams
        .last()
        .map(|stream| stream.iter().map(|r| stream_request(r)).collect())
        .unwrap_or_default();
    let mut event_kinds = BTreeMap::new();
    for request in &requests {
        if request.event.kind == 60 {
            *event_kinds.entry(event_kind(&request.value)).or_insert(0) += 1;
        }
    }
    Upload {
        lifecycle: lifecycle_events
            .iter()
            .map(|l| {
                (
                    l.event.kind,
                    l.event.sequence,
                    l.event.component,
                    !l.event.invocation_id.is_empty(),
                    !l.keywords.is_empty(),
                    l.fields.clone(),
                    l.result,
                )
            })
            .collect(),
        keywords: requests
            .first()
            .map(|r| r.keywords.clone())
            .unwrap_or_default(),
        first_fields: requests
            .first()
            .map(|r| r.fields.clone())
            .unwrap_or_default(),
        sequences_contiguous: requests
            .iter()
            .enumerate()
            .all(|(index, r)| r.event.sequence == index as u64 + 1),
        components: {
            let mut components: Vec<u64> = requests.iter().map(|r| r.event.component).collect();
            components.dedup();
            components
        },
        type_urls: {
            let mut urls: Vec<String> =
                requests.iter().filter_map(|r| r.type_url.clone()).collect();
            urls.dedup();
            urls
        },
        last_kind: requests.last().map_or(0, |r| r.event.kind),
        event_kinds,
        streams: streams.len(),
    }
}

#[test]
#[ignore = "needs genuine Bazel 9.3 (BAZEL_RS_GENUINE_BAZEL)"]
fn genuine_bazel_sends_the_same_bes_stream() {
    let binary = std::env::var("BAZEL_RS_GENUINE_BAZEL").expect("BAZEL_RS_GENUINE_BAZEL");
    let registry = format!(
        "file://{}",
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/builtin_modules/registry")
            .display()
    );
    let ws = Workspace::new();
    let flags = |server: &Server| -> Vec<String> {
        vec![
            "build".into(),
            format!("--bes_backend={}", server.url()),
            "--bes_keywords=k1".into(),
            "--bes_instance_name=inst".into(),
            "--bes_results_url=http://results".into(),
            "--experimental_build_event_upload_retry_minimum_delay=10ms".into(),
            "//pkg:gen".into(),
        ]
    };
    let genuine_server = Server::new();
    let args = flags(&genuine_server);
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    let genuine = genuine_run(&binary, &ws, "g1", &registry, &args);
    assert!(
        genuine.status.success(),
        "{}",
        String::from_utf8_lossy(&genuine.stderr)
    );
    let genuine_upload = upload(&genuine_server);
    let native_server = Server::new();
    let args = flags(&native_server);
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    let native = native_run(&ws, "n1", &registry, &args);
    assert!(native.status.success(), "{}", stderr(&native));
    let native_upload = upload(&native_server);
    // A non-retryable stream failure on both.
    let failing = |server: &Server| {
        server.configure(|config| {
            config.bes_stream_failures.push_back((0, 3, "bad".into()));
        })
    };
    failing(&genuine_server);
    let args = flags(&genuine_server);
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    let genuine_failed = genuine_run(&binary, &ws, "g1", &registry, &args);
    failing(&native_server);
    let args = flags(&native_server);
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    let native_failed = native_run(&ws, "n1", &registry, &args);
    let error_line = |output: &Output| {
        String::from_utf8_lossy(&output.stderr)
            .lines()
            .find(|line| line.contains("The Build Event Protocol upload failed"))
            .map(str::to_owned)
    };
    let url_lines = |output: &Output| {
        String::from_utf8_lossy(&output.stderr)
            .lines()
            .filter(|line| line.contains("Streaming build results to"))
            .count()
    };
    let receipt = serde_json::json!({
        "genuine": genuine_upload,
        "native": native_upload,
        "genuine_url_lines": url_lines(&genuine),
        "native_url_lines": url_lines(&native),
        "genuine_failed": [genuine_failed.status.code(), error_line(&genuine_failed)],
        "native_failed": [native_failed.status.code(), error_line(&native_failed)],
    });
    if let Ok(path) = std::env::var("BAZEL_RS_BES_RECEIPT") {
        fs::write(path, serde_json::to_string_pretty(&receipt).unwrap() + "\n").unwrap();
    }
    assert_eq!(genuine_upload.lifecycle, native_upload.lifecycle);
    assert_eq!(genuine_upload.keywords, native_upload.keywords);
    assert_eq!(genuine_upload.first_fields, native_upload.first_fields);
    assert!(genuine_upload.sequences_contiguous && native_upload.sequences_contiguous);
    assert_eq!(genuine_upload.components, native_upload.components);
    assert_eq!(genuine_upload.type_urls, native_upload.type_urls);
    assert_eq!(genuine_upload.last_kind, native_upload.last_kind);
    assert_eq!(genuine_upload.streams, native_upload.streams);
    assert_eq!(url_lines(&genuine), url_lines(&native));
    assert_eq!(genuine_failed.status.code(), native_failed.status.code());
    assert_eq!(error_line(&genuine_failed), error_line(&native_failed));
}
