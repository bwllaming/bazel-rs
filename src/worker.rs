//! Persistent workers: Bazel 9.3's `worker` spawn strategy
//! (docs/PERSISTENT_WORKERS.md; worker/WorkerSpawnRunner.java,
//! WorkerParser.java, WorkerPoolImpl.java, SingleplexWorker.java,
//! JsonWorkerProtocol.java, ProtoWorkerProtocol.java).
//!
//! A spawn whose execution requirements carry `supports-workers=1` (or
//! `supports-multiplex-workers=1`) runs as a WorkRequest sent to a
//! long-lived worker process instead of a fresh process. The worker is
//! started once per worker key — the spawn's argv minus its `@flagfile`
//! arguments plus `--persistent_worker` (and `--worker_extra_flag`s), its
//! environment, the worker key mnemonic and the protocol — and reused by
//! every later spawn of that key, up to `--worker_max_instances` processes
//! per key (default 4; multiplex keys `--worker_max_multiplex_instances`,
//! default 8).
//!
//! bazel-rs has no server: the pool lives for one command, and every worker
//! is stopped when the command ends (genuine's `--worker_quit_after_build`
//! behaviour is therefore always in effect). Each worker process owns a
//! private working directory (`<cache>/work/bazel-workers-<pid>/
//! worker-<id>-<mnemonic>/execroot`) that the executor re-stages with the
//! spawn's inputs before each request (crate::action, the same hardlinked
//! staging as a sandboxed spawn); outputs are published from there exactly
//! like a sandboxed spawn's. This is genuine's sandboxed worker
//! (SandboxedWorker/WorkerExecRoot) layout, used for every worker because
//! bazel-rs has no shared execution root; `--worker_sandboxing` additionally
//! starts the worker under the Landlock sandbox (crate::sandbox).
//!
//! Multiplex workers are modelled as processes with one request in flight
//! each: a multiplex key's requests carry non-zero request ids like
//! genuine's, but each process handles one WorkRequest at a time (bazel-rs
//! stages one spawn's inputs per working directory).

use std::collections::{BTreeMap, HashMap};
use std::ffi::OsString;
use std::fs;
use std::io::{self, BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicU32, Ordering};
use std::sync::{Arc, Condvar, Mutex, OnceLock};

use base64::Engine as _;

/// WorkerPoolImpl.DEFAULT_MAX_SINGLEPLEX_WORKERS.
const DEFAULT_MAX_SINGLEPLEX_WORKERS: usize = 4;
/// WorkerPoolImpl.DEFAULT_MAX_MULTIPLEX_WORKERS.
const DEFAULT_MAX_MULTIPLEX_WORKERS: usize = 8;
/// WorkerSpawnRunner.VERBOSE_LEVEL: the request verbosity under
/// `--worker_verbose`.
const VERBOSE_LEVEL: i32 = 10;
/// RecordingInputStream.startRecording(4096) / ErrorMessage.logSizeLimit.
const RECORD_LIMIT: usize = 4096;
const LOG_SIZE_LIMIT: u64 = 8192;

/// WorkerOptions: the worker flags this engine models.
#[derive(Clone, Debug)]
pub struct WorkerOptions {
    /// `--worker_max_instances` entries in flag order: `(mnemonic, value)`,
    /// mnemonic "" for the default, value None for "auto"
    /// (WorkerOptions.MultiResourceConverter).
    pub max_instances: Vec<(String, Option<usize>)>,
    /// `--worker_max_multiplex_instances`, the same shape.
    pub max_multiplex_instances: Vec<(String, Option<usize>)>,
    /// `--worker_quit_after_build` (always the behaviour here: no server).
    pub quit_after_build: bool,
    /// `--worker_verbose`.
    pub verbose: bool,
    /// `--worker_extra_flag=<mnemonic>=<flag>` in flag order.
    pub extra_flags: Vec<(String, String)>,
    /// `--worker_sandboxing`.
    pub sandboxing: bool,
    /// `--worker_multiplex` (default true).
    pub multiplex: bool,
    /// `--experimental_worker_multiplex_sandboxing`.
    pub multiplex_sandboxing: bool,
    /// `--experimental_worker_strict_flagfiles`.
    pub strict_flagfiles: bool,
    /// `--experimental_worker_allowlist` (None: unset).
    pub allowlist: Option<Vec<String>>,
}

impl Default for WorkerOptions {
    fn default() -> Self {
        Self {
            max_instances: Vec::new(),
            max_multiplex_instances: Vec::new(),
            quit_after_build: false,
            verbose: false,
            extra_flags: Vec::new(),
            sandboxing: false,
            multiplex: true,
            multiplex_sandboxing: false,
            strict_flagfiles: false,
            allowlist: None,
        }
    }
}

static OPTIONS: OnceLock<WorkerOptions> = OnceLock::new();

/// Installs the invocation's worker options (once, from main).
pub fn install(options: WorkerOptions) {
    let _ = OPTIONS.set(options);
}

fn options() -> &'static WorkerOptions {
    OPTIONS.get_or_init(WorkerOptions::default)
}

/// Parses one `--worker_max_instances` / `--worker_max_multiplex_instances`
/// value (WorkerOptions.MultiResourceConverter): `[name=]value` where value
/// is an integer, `auto`, or `HOST_CPUS` optionally followed by `-<n>` or
/// `*<factor>` (ResourceConverter.FLAG_SYNTAX). `auto` alone is dropped
/// (`None` here); `name=auto` selects the default for that mnemonic.
pub fn parse_max_instances(
    flag: &str,
    input: &str,
) -> Result<Option<(String, Option<usize>)>, String> {
    if input == "null" || input == "auto" {
        return Ok(None);
    }
    let (name, value) = match input.split_once('=') {
        Some((name, value)) => (name.to_owned(), value),
        None => (String::new(), input),
    };
    if value == "auto" {
        return Ok(Some((name, None)));
    }
    let parsed = resource_value(value).ok_or_else(|| {
        format!(
            "While parsing option --{flag}={input}: '{value}' is not an int or does not follow \
             the format [name=]value, where value is an integer, or a keyword (\"auto\", \
             \"HOST_CPUS\", \"HOST_RAM\"), optionally followed by an operation ([-|*]<float>) \
             eg. \"auto\", \"HOST_CPUS*.5\""
        )
    })?;
    Ok(Some((name, Some(parsed))))
}

fn resource_value(value: &str) -> Option<usize> {
    if let Ok(number) = value.parse::<usize>() {
        return Some(number);
    }
    let rest = value.strip_prefix("HOST_CPUS")?;
    let cpus = std::thread::available_parallelism().map_or(1, |n| n.get()) as f64;
    let result = if rest.is_empty() {
        cpus
    } else if let Some(factor) = rest.strip_prefix('*') {
        cpus * factor.parse::<f64>().ok()?
    } else {
        cpus - rest.strip_prefix('-')?.parse::<f64>().ok()?
    };
    (result >= 0.0).then(|| result.round() as usize)
}

/// WorkerPoolImpl.getMaxInstances + getMaxWorkerInstances.
fn max_instances(entries: &[(String, Option<usize>)], mnemonic: &str, default: usize) -> usize {
    let mut map: HashMap<&str, usize> = HashMap::new();
    for (name, value) in entries {
        map.insert(name, value.unwrap_or(default));
    }
    if let Some(value) = map.get(mnemonic) {
        return *value;
    }
    map.get("").copied().unwrap_or(default)
}

/// ExecutionRequirements.WorkerProtocolFormat.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Protocol {
    Json,
    Proto,
}

/// Spawns.getWorkerKeyMnemonic: `worker-key-mnemonic`, else the mnemonic.
fn key_mnemonic<'a>(mnemonic: &'a str, execution_info: &'a BTreeMap<String, String>) -> &'a str {
    execution_info
        .get("worker-key-mnemonic")
        .map_or(mnemonic, String::as_str)
}

fn requirement(execution_info: &BTreeMap<String, String>, name: &str) -> bool {
    execution_info.get(name).is_some_and(|value| value == "1")
}

/// WorkerSpawnRunner.canExec: the spawn supports (multiplex) workers
/// (`"1"` exactly, Spawns.supportsWorkers), passes
/// `--experimental_worker_allowlist`, and has tool files — here: its
/// executable is a File (a FilesToRunProvider's or a File `executable`;
/// run_shell's `tools` are not tracked separately, docs/PERSISTENT_WORKERS.md).
pub fn can_exec(
    mnemonic: &str,
    execution_info: &BTreeMap<String, String>,
    has_tools: bool,
) -> bool {
    if !requirement(execution_info, "supports-workers")
        && !requirement(execution_info, "supports-multiplex-workers")
    {
        return false;
    }
    if let Some(allowlist) = &options().allowlist
        && !allowlist.is_empty()
        && !allowlist
            .iter()
            .any(|entry| entry == key_mnemonic(mnemonic, execution_info))
    {
        return false;
    }
    has_tools
}

/// The identity of a worker process (WorkerKey): processes are shared by
/// spawns with equal keys. Genuine also keys the combined digest of the tool
/// files; within one bazel-rs command an exec path names one content, so
/// the argv identifies the tools.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct WorkerKey {
    /// The worker's argv: the spawn's arguments without the flag files, plus
    /// `--persistent_worker` and the mnemonic's extra flags.
    pub args: Vec<String>,
    /// The worker's environment (the spawn's, TMPDIR included).
    pub env: BTreeMap<String, OsString>,
    /// Spawns.getWorkerKeyMnemonic.
    pub mnemonic: String,
    pub sandboxed: bool,
    pub multiplex: bool,
    pub protocol: Protocol,
}

impl WorkerKey {
    /// WorkerKey.makeWorkerTypeName.
    fn type_name(&self) -> &'static str {
        if self.multiplex {
            "multiplex-worker"
        } else {
            "worker"
        }
    }

    /// A stable 32-bit hash for `--worker_verbose` messages.
    fn hash_code(&self) -> i32 {
        use std::hash::{Hash, Hasher};
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        self.hash(&mut hasher);
        hasher.finish() as i32
    }
}

/// The parsed spawn: its worker key and its flag files (WorkerParser
/// .WorkerConfig).
pub struct WorkerConfig {
    pub key: WorkerKey,
    pub flag_files: Vec<String>,
}

/// Java's `.` (any character but a line terminator).
fn java_dot(c: char) -> bool {
    !matches!(c, '\n' | '\r' | '\u{85}' | '\u{2028}' | '\u{2029}')
}

/// WorkerParser.FLAG_FILE_PATTERN `(?:@|--?flagfile=)(.+)`, matched in full.
fn flag_file_pattern(arg: &str) -> bool {
    let rest = arg
        .strip_prefix('@')
        .or_else(|| arg.strip_prefix("--flagfile="))
        .or_else(|| arg.strip_prefix("-flagfile="));
    rest.is_some_and(|rest| !rest.is_empty() && rest.chars().all(java_dot))
}

/// WorkerParser.isFlagFileArg.
fn is_flag_file_arg(arg: &str) -> bool {
    flag_file_pattern(arg) && !arg.starts_with("@@")
}

/// A Java `List.toString()` of strings.
fn java_list(items: &[String]) -> String {
    format!("[{}]", items.join(", "))
}

/// A spawn the worker strategy cannot execute: the action's failure
/// message; `environmental` for genuine's IOException class (exit code 36).
#[derive(Debug)]
pub struct ParseError {
    pub message: String,
    pub environmental: bool,
}

/// WorkerParser.compute: splits the spawn's argv into the worker's startup
/// argv and its flag files, and builds the worker key. Errors are the
/// action's failure messages (UserExecException / IOException texts).
pub fn parse(
    mnemonic: &str,
    argv: &[String],
    env: BTreeMap<String, OsString>,
    execution_info: &BTreeMap<String, String>,
) -> Result<WorkerConfig, ParseError> {
    let options = options();
    let failure = |reason: &str| ParseError {
        message: format!(
            "Worker strategy cannot execute this {mnemonic} action, {reason}\n{}",
            java_list(argv)
        ),
        environmental: false,
    };
    let mut worker_args = Vec::new();
    let mut flag_files = Vec::new();
    let Some(last) = argv.last() else {
        return Err(failure(
            "because the command-line arguments do not contain exactly one @flagfile or --flagfile=",
        ));
    };
    if options.strict_flagfiles {
        if !is_flag_file_arg(last) {
            return Err(failure(
                "because the command-line arguments does not end with a @flagfile or --flagfile= argument",
            ));
        }
        flag_files.push(last.clone());
        for arg in &argv[..argv.len() - 1] {
            if is_flag_file_arg(arg) {
                return Err(failure(
                    "because the command-line arguments has a @flagfile or --flagfile= argument before the end",
                ));
            }
            worker_args.push(arg.clone());
        }
    } else {
        for arg in argv {
            // isLegacyFlagFileArg: the same pattern, @@ not excluded.
            if flag_file_pattern(arg) {
                flag_files.push(arg.clone());
            } else {
                worker_args.push(arg.clone());
            }
        }
        if flag_files.is_empty() {
            return Err(failure(
                "because the command-line arguments do not contain exactly one @flagfile or --flagfile=",
            ));
        }
    }
    let key_mnemonic = key_mnemonic(mnemonic, execution_info).to_owned();
    worker_args.push("--persistent_worker".to_owned());
    for (name, flag) in &options.extra_flags {
        if *name == key_mnemonic {
            worker_args.push(flag.clone());
        }
    }
    // Spawns.getWorkerProtocolFormat (an IOException: "Exec failed due to
    // IOException: ..." in genuine's report).
    let protocol = match execution_info
        .get("requires-worker-protocol")
        .map(String::as_str)
    {
        None | Some("proto") => Protocol::Proto,
        Some("json") => Protocol::Json,
        Some(_) => {
            return Err(ParseError {
                message: "Exec failed due to IOException: requires-worker-protocol must be set \
                          to a valid worker protocol format: json or proto"
                    .to_owned(),
                environmental: true,
            });
        }
    };
    // WorkerParser.createWorkerKey (no dynamic execution, no path mapping).
    let should_multiplex =
        options.multiplex && requirement(execution_info, "supports-multiplex-workers");
    let (sandboxed, multiplex) = if should_multiplex {
        (
            options.multiplex_sandboxing
                && requirement(execution_info, "supports-multiplex-sandboxing"),
            true,
        )
    } else {
        (options.sandboxing, false)
    };
    Ok(WorkerConfig {
        key: WorkerKey {
            args: worker_args,
            env,
            mnemonic: key_mnemonic,
            sandboxed,
            multiplex,
            protocol,
        },
        flag_files,
    })
}

/// WorkerSpawnRunner.isExternalRepositoryLabel: `^@.*//.*`.
fn is_external_repository_label(arg: &str) -> bool {
    arg.strip_prefix('@')
        .is_some_and(|rest| rest.chars().all(java_dot) && rest.contains("//"))
}

/// WorkerSpawnRunner.expandArgument: replaces `@file` arguments (not `@@`,
/// not external labels) by the lines of the declared input `file`,
/// recursively; `read` returns the input's bytes when `file` is a declared
/// input.
pub fn expand_argument(
    arg: &str,
    read: &dyn Fn(&str) -> Option<io::Result<Vec<u8>>>,
    into: &mut Vec<String>,
) -> Result<(), String> {
    if let Some(path) = arg.strip_prefix('@')
        && !arg.starts_with("@@")
        && !is_external_repository_label(arg)
    {
        let bytes = match read(path) {
            None => {
                return Err(format!(
                    "Failed to read @-argument '{path}': file is not a declared input"
                ));
            }
            Some(Err(error)) => {
                return Err(format!("Failed to read @-argument '{path}': {error}"));
            }
            Some(Ok(bytes)) => bytes,
        };
        let text = String::from_utf8_lossy(&bytes);
        for line in text.lines() {
            expand_argument(line, read, into)?;
        }
    } else {
        into.push(arg.to_owned());
    }
    Ok(())
}

/// One WorkRequest (worker_protocol.proto).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct WorkRequest {
    pub arguments: Vec<String>,
    /// `(path, digest)`: the digest is the lowercase hex SHA-256 as UTF-8
    /// bytes (WorkerSpawnRunner.createWorkRequest), empty when unknown.
    pub inputs: Vec<(String, String)>,
    pub request_id: i32,
    pub verbosity: i32,
}

/// One WorkResponse (worker_protocol.proto).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct WorkResponse {
    pub exit_code: i32,
    pub output: String,
    pub request_id: i32,
    pub was_cancelled: bool,
}

fn put_varint(out: &mut Vec<u8>, mut value: u64) {
    loop {
        let byte = (value & 0x7f) as u8;
        value >>= 7;
        if value == 0 {
            out.push(byte);
            return;
        }
        out.push(byte | 0x80);
    }
}

fn put_bytes(out: &mut Vec<u8>, field: u32, bytes: &[u8]) {
    put_varint(out, u64::from(field << 3 | 2));
    put_varint(out, bytes.len() as u64);
    out.extend_from_slice(bytes);
}

/// proto3 int32 on the wire: negative values sign-extend to 64 bits.
fn put_int32(out: &mut Vec<u8>, field: u32, value: i32) {
    if value != 0 {
        put_varint(out, u64::from(field << 3));
        put_varint(out, i64::from(value) as u64);
    }
}

impl WorkRequest {
    /// The protobuf encoding (without the length prefix).
    pub fn encode_proto(&self) -> Vec<u8> {
        let mut out = Vec::new();
        for argument in &self.arguments {
            put_bytes(&mut out, 1, argument.as_bytes());
        }
        for (path, digest) in &self.inputs {
            let mut input = Vec::new();
            if !path.is_empty() {
                put_bytes(&mut input, 1, path.as_bytes());
            }
            if !digest.is_empty() {
                put_bytes(&mut input, 2, digest.as_bytes());
            }
            put_bytes(&mut out, 2, &input);
        }
        put_int32(&mut out, 3, self.request_id);
        put_int32(&mut out, 5, self.verbosity);
        out
    }

    /// `writeDelimitedTo`: the varint length, then the message.
    pub fn encode_delimited(&self) -> Vec<u8> {
        let body = self.encode_proto();
        let mut out = Vec::with_capacity(body.len() + 5);
        put_varint(&mut out, body.len() as u64);
        out.extend_from_slice(&body);
        out
    }

    /// JsonFormat.printer().omittingInsignificantWhitespace() plus the ndjson
    /// newline: proto3 JSON field names, default values omitted, bytes in
    /// base64, strings escaped like Gson (HTML-safe).
    pub fn encode_json(&self) -> Vec<u8> {
        let mut fields = Vec::new();
        if !self.arguments.is_empty() {
            let items: Vec<String> = self.arguments.iter().map(|a| json_string(a)).collect();
            fields.push(format!("\"arguments\":[{}]", items.join(",")));
        }
        if !self.inputs.is_empty() {
            let items: Vec<String> = self
                .inputs
                .iter()
                .map(|(path, digest)| {
                    let mut parts = Vec::new();
                    if !path.is_empty() {
                        parts.push(format!("\"path\":{}", json_string(path)));
                    }
                    if !digest.is_empty() {
                        let encoded = base64::engine::general_purpose::STANDARD.encode(digest);
                        parts.push(format!("\"digest\":\"{encoded}\""));
                    }
                    format!("{{{}}}", parts.join(","))
                })
                .collect();
            fields.push(format!("\"inputs\":[{}]", items.join(",")));
        }
        if self.request_id != 0 {
            fields.push(format!("\"requestId\":{}", self.request_id));
        }
        if self.verbosity != 0 {
            fields.push(format!("\"verbosity\":{}", self.verbosity));
        }
        format!("{{{}}}\n", fields.join(",")).into_bytes()
    }
}

/// Gson's string escaping with HTML-safe characters (JsonFormat's printer):
/// `"` and `\` escaped, `\t \b \n \r \f` short, other control characters,
/// U+2028/U+2029 and `< > & = '` as lowercase `\u00XX`.
fn json_string(value: &str) -> String {
    let mut out = String::with_capacity(value.len() + 2);
    out.push('"');
    for c in value.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\t' => out.push_str("\\t"),
            '\u{8}' => out.push_str("\\b"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\u{c}' => out.push_str("\\f"),
            '<' | '>' | '&' | '=' | '\'' | '\u{2028}' | '\u{2029}' => {
                out.push_str(&format!("\\u{:04x}", c as u32));
            }
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

fn get_varint(data: &[u8], position: &mut usize) -> Result<u64, String> {
    let mut result = 0u64;
    for shift in (0..64).step_by(7) {
        let byte = *data
            .get(*position)
            .ok_or("truncated message (varint)".to_owned())?;
        *position += 1;
        result |= u64::from(byte & 0x7f) << shift;
        if byte & 0x80 == 0 {
            return Ok(result);
        }
    }
    Err("malformed varint".to_owned())
}

impl WorkResponse {
    /// Parses a WorkResponse message (unknown fields skipped).
    pub fn decode_proto(data: &[u8]) -> Result<Self, String> {
        let mut response = Self::default();
        let mut position = 0;
        while position < data.len() {
            let key = get_varint(data, &mut position)?;
            let (field, wire) = (key >> 3, key & 7);
            match wire {
                0 => {
                    let value = get_varint(data, &mut position)?;
                    match field {
                        1 => response.exit_code = value as i32,
                        3 => response.request_id = value as i32,
                        4 => response.was_cancelled = value != 0,
                        _ => {}
                    }
                }
                2 => {
                    let length = usize::try_from(get_varint(data, &mut position)?)
                        .map_err(|_| "length out of range".to_owned())?;
                    let end = position
                        .checked_add(length)
                        .filter(|end| *end <= data.len())
                        .ok_or("truncated message (length-delimited field)".to_owned())?;
                    if field == 2 {
                        response.output = String::from_utf8_lossy(&data[position..end]).into();
                    }
                    position = end;
                }
                1 => position += 8,
                5 => position += 4,
                _ => return Err(format!("unsupported wire type {wire}")),
            }
            if position > data.len() {
                return Err("truncated message".to_owned());
            }
        }
        Ok(response)
    }

    /// JsonWorkerProtocol.parseResponse over one JSON object: `exitCode`,
    /// `output`, `requestId` (each at most once); unknown fields ignored.
    pub fn decode_json(text: &[u8]) -> Result<Self, String> {
        let error = || "Could not parse json work request correctly".to_owned();
        let value: serde_json::Value = serde_json::from_slice(text).map_err(|_| error())?;
        let object = value.as_object().ok_or_else(error)?;
        let mut response = Self::default();
        let int = |value: &serde_json::Value| -> Result<i32, String> {
            value
                .as_i64()
                .or_else(|| value.as_str().and_then(|text| text.parse().ok()))
                .and_then(|value| i32::try_from(value).ok())
                .ok_or_else(error)
        };
        if let Some(value) = object.get("exitCode") {
            response.exit_code = int(value)?;
        }
        if let Some(value) = object.get("output") {
            response.output = match value {
                serde_json::Value::String(text) => text.clone(),
                serde_json::Value::Number(number) => number.to_string(),
                serde_json::Value::Bool(flag) => flag.to_string(),
                _ => return Err(error()),
            };
        }
        if let Some(value) = object.get("requestId") {
            response.request_id = int(value)?;
        }
        Ok(response)
    }
}

/// Why a request produced no usable response: each is an action failure
/// with genuine's message (WorkerSpawnRunner), the worker invalidated.
#[derive(Debug)]
pub enum WorkerFailure {
    /// The worker could not be started or prepared.
    Start(String),
    /// Writing the request failed (the worker quit or closed stdin).
    Request(String),
    /// No response (end of stream before any byte).
    NoResponse,
    /// An unparseable response: the recorded bytes and the parse error.
    Unparseable { recorded: String, error: String },
}

/// A started worker process.
struct Process {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
}

/// One worker (Worker): a process started lazily on the first request, its
/// working directory and log file.
struct Worker {
    id: u32,
    key: Arc<WorkerKey>,
    /// The worker's working directory (the spawn's execution root).
    execroot: PathBuf,
    log: PathBuf,
    process: Option<Process>,
    /// A request was sent and its response not fully read: the process
    /// cannot be reused.
    in_flight: bool,
}

impl Worker {
    /// SingleplexWorker.createProcess: the argv with a relative argv[0]
    /// that has a directory made absolute against the working directory
    /// (makeExecPathAbsolute), the key's environment, stdin/stdout piped and
    /// stderr appended to the log file; under `--worker_sandboxing` started
    /// through the Landlock sandbox (crate::sandbox::wrap_worker).
    fn start(&mut self) -> Result<(), String> {
        let mut args = self.key.args.clone();
        let executable = Path::new(&args[0]);
        if !executable.is_absolute()
            && executable
                .parent()
                .is_some_and(|p| !p.as_os_str().is_empty())
        {
            args[0] = self.execroot.join(executable).display().to_string();
        }
        let log = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.log)
            .map_err(|e| format!("cannot open worker log '{}': {e}", self.log.display()))?;
        let mut command = if self.key.sandboxed {
            let tmp = self
                .key
                .env
                .get("TMPDIR")
                .map_or_else(|| PathBuf::from("/tmp"), PathBuf::from);
            let parent = self
                .execroot
                .parent()
                .unwrap_or(&self.execroot)
                .to_path_buf();
            let mut command = Command::new("/proc/self/exe");
            command.args(crate::sandbox::wrap_worker(
                &self.execroot,
                &tmp,
                &self.key.env,
                &args,
                &parent,
            ));
            command
        } else {
            let mut command = Command::new(&args[0]);
            command.args(&args[1..]);
            command
        };
        command
            .current_dir(&self.execroot)
            .env_clear()
            .envs(&self.key.env)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::from(log));
        {
            use std::os::unix::process::CommandExt;
            // Its own process group: the shutdown kills the whole group, and
            // terminal signals for bazel-rs's group never reach it.
            command.process_group(0);
        }
        crate::signals::reset_in_child(&mut command, crate::signals::Defaults::ProcessWrapper);
        let mut child = crate::spawn_lock::spawn(&mut command)
            .map_err(|e| format!("IOException while preparing the execution environment of a worker: cannot run program \"{}\": {e}", args[0]))?;
        let stdin = child.stdin.take().expect("piped stdin");
        let stdout = BufReader::new(child.stdout.take().expect("piped stdout"));
        if options().verbose {
            crate::note!(
                "INFO: Created new {} {} {} {} (id {}, key hash {}), logging to {}",
                if self.key.sandboxed {
                    "sandboxed"
                } else {
                    "non-sandboxed"
                },
                if self.key.multiplex {
                    "multiplex"
                } else {
                    "singleplex"
                },
                self.key.mnemonic,
                self.key.type_name(),
                self.id,
                self.key.hash_code(),
                self.log.display()
            );
        }
        self.process = Some(Process {
            child,
            stdin,
            stdout,
        });
        Ok(())
    }

    /// Sends one request and reads its response.
    fn exchange(&mut self, request: &WorkRequest) -> Result<WorkResponse, WorkerFailure> {
        if self.process.is_none() {
            self.start().map_err(WorkerFailure::Start)?;
        }
        let protocol = self.key.protocol;
        let process = self.process.as_mut().expect("started");
        self.in_flight = true;
        let bytes = match protocol {
            Protocol::Proto => request.encode_delimited(),
            Protocol::Json => request.encode_json(),
        };
        if let Err(error) = process
            .stdin
            .write_all(&bytes)
            .and_then(|()| process.stdin.flush())
        {
            return Err(WorkerFailure::Request(error.to_string()));
        }
        let mut recorded = Vec::new();
        let read = match protocol {
            Protocol::Proto => read_proto(&mut process.stdout, &mut recorded),
            Protocol::Json => read_json(&mut process.stdout, &mut recorded),
        };
        match read {
            Ok(response) => {
                self.in_flight = false;
                Ok(response)
            }
            Err(None) => Err(WorkerFailure::NoResponse),
            Err(Some(error)) => {
                // RecordingInputStream.readRemaining: what the worker wrote
                // after the unparseable bytes (the process is killed first,
                // so the read ends).
                kill(process);
                let mut rest = Vec::new();
                let _ = (&mut process.stdout)
                    .take((RECORD_LIMIT.saturating_sub(recorded.len())) as u64)
                    .read_to_end(&mut rest);
                recorded.extend_from_slice(&rest);
                recorded.truncate(RECORD_LIMIT);
                Err(WorkerFailure::Unparseable {
                    recorded: String::from_utf8_lossy(&recorded).into_owned(),
                    error,
                })
            }
        }
    }

    fn destroy(&mut self) {
        if let Some(mut process) = self.process.take() {
            kill(&mut process);
            let _ = process.child.wait();
        }
    }
}

/// SIGKILL to the worker's process group (and the process itself).
fn kill(process: &mut Process) {
    let _ = crate::git_process::signal_group(process.child.id(), rustix::process::Signal::KILL);
    let _ = process.child.kill();
}

/// Reads one varint-delimited WorkResponse. `Err(None)`: end of stream
/// before any byte; `Err(Some(error))`: malformed.
fn read_proto(
    stdout: &mut BufReader<ChildStdout>,
    recorded: &mut Vec<u8>,
) -> Result<WorkResponse, Option<String>> {
    let mut length = 0u64;
    let mut shift = 0;
    loop {
        let mut byte = [0u8; 1];
        match stdout.read(&mut byte) {
            Ok(0) if recorded.is_empty() => return Err(None),
            Ok(0) => return Err(Some("truncated message (length prefix)".into())),
            Ok(_) => {}
            Err(error) => return Err(Some(error.to_string())),
        }
        recorded.push(byte[0]);
        length |= u64::from(byte[0] & 0x7f) << shift;
        if byte[0] & 0x80 == 0 {
            break;
        }
        shift += 7;
        if shift >= 64 {
            return Err(Some("malformed varint".into()));
        }
    }
    let mut body = Vec::new();
    let read = (&mut *stdout)
        .take(length)
        .read_to_end(&mut body)
        .map_err(|e| Some(e.to_string()))?;
    let keep = RECORD_LIMIT.saturating_sub(recorded.len()).min(body.len());
    recorded.extend_from_slice(&body[..keep]);
    if (read as u64) < length {
        return Err(Some("truncated message".into()));
    }
    WorkResponse::decode_proto(&body).map_err(Some)
}

/// Reads one JSON object (lenient JsonReader: leading whitespace skipped,
/// the object may span lines). `Err(None)`: end of stream before any byte.
fn read_json(
    stdout: &mut BufReader<ChildStdout>,
    recorded: &mut Vec<u8>,
) -> Result<WorkResponse, Option<String>> {
    let error = || Some("Could not parse json work request correctly".to_owned());
    let mut object = Vec::new();
    let mut depth = 0usize;
    let mut in_string = false;
    let mut escaped = false;
    loop {
        let available = match stdout.fill_buf() {
            Ok(buffer) => buffer,
            Err(e) => return Err(Some(e.to_string())),
        };
        if available.is_empty() {
            return Err(if recorded.is_empty() { None } else { error() });
        }
        let mut consumed = 0;
        let mut done = false;
        for &byte in available {
            consumed += 1;
            // Whitespace before the object (the previous response's
            // newline) is neither data nor recorded.
            if depth == 0 && byte.is_ascii_whitespace() {
                continue;
            }
            if recorded.len() < RECORD_LIMIT {
                recorded.push(byte);
            }
            if depth == 0 {
                if byte != b'{' {
                    stdout.consume(consumed);
                    return Err(error());
                }
                depth = 1;
                object.push(byte);
                continue;
            }
            object.push(byte);
            if in_string {
                if escaped {
                    escaped = false;
                } else if byte == b'\\' {
                    escaped = true;
                } else if byte == b'"' {
                    in_string = false;
                }
                continue;
            }
            match byte {
                b'"' => in_string = true,
                b'{' | b'[' => depth += 1,
                b'}' | b']' => {
                    depth -= 1;
                    if depth == 0 {
                        done = true;
                        break;
                    }
                }
                _ => {}
            }
        }
        stdout.consume(consumed);
        if done {
            return WorkResponse::decode_json(&object).map_err(Some);
        }
    }
}

/// ErrorMessage.builder().logFile(log).logSizeLimit(8192): the log section of
/// a worker failure message.
fn log_section(log: &Path) -> String {
    let content = fs::File::open(log)
        .and_then(|file| {
            let mut bytes = Vec::new();
            file.take(LOG_SIZE_LIMIT).read_to_end(&mut bytes)?;
            Ok(bytes)
        })
        .unwrap_or_default();
    let text = String::from_utf8_lossy(&content);
    let text = text.trim_end_matches('\n');
    let body = if text.is_empty() { "(empty)" } else { text };
    format!(
        "\n\n---8<---8<--- Start of log, file at {} ---8<---8<---\n{body}\n---8<---8<--- End of log ---8<---8<---",
        log.display()
    )
}

impl WorkerFailure {
    /// The failure's message (WorkerSpawnRunner's ErrorMessage texts; the
    /// Java exception details are reduced to the exception's message).
    fn message(&self, mnemonic: &str, log: &Path) -> String {
        match self {
            WorkerFailure::Start(error) => error.clone(),
            WorkerFailure::Request(error) => format!(
                "Worker process quit or closed its stdin stream when we tried to send a \
                 WorkRequest:\n\n---8<---8<--- Exception details ---8<---8<---\n\
                 java.io.IOException: {error}\n---8<---8<--- End of exception details \
                 ---8<---8<---{}",
                log_section(log)
            ),
            WorkerFailure::NoResponse => format!(
                "Worker process did not return a WorkResponse:{}",
                log_section(log)
            ),
            WorkerFailure::Unparseable { recorded, error } => {
                let _ = mnemonic;
                format!(
                    "Worker process returned an unparseable WorkResponse!\n\n\
                     Did you try to print something to stdout? Workers aren't allowed to do \
                     this, as it breaks the protocol between Bazel and the worker process.\n\n\
                     ---8<---8<--- Start of response ---8<---8<---\n{recorded}\
                     ---8<---8<--- End of response ---8<---8<---\n\n\
                     ---8<---8<--- Exception details ---8<---8<---\n\
                     java.io.IOException: {error}\n\
                     ---8<---8<--- End of exception details ---8<---8<---{}",
                    log_section(log)
                )
            }
        }
    }
}

/// The per-key pool state (WorkerKeyPool).
#[derive(Default)]
struct KeyPool {
    idle: Vec<Worker>,
    /// Borrowed workers.
    active: usize,
}

#[derive(Default)]
struct PoolState {
    /// By (cache work directory, key).
    keys: HashMap<(PathBuf, Arc<WorkerKey>), KeyPool>,
    /// The per-process worker base directories created so far.
    bases: Vec<PathBuf>,
}

struct Pool {
    state: Mutex<PoolState>,
    changed: Condvar,
}

fn pool() -> &'static Pool {
    static POOL: OnceLock<Pool> = OnceLock::new();
    POOL.get_or_init(|| Pool {
        state: Mutex::new(PoolState::default()),
        changed: Condvar::new(),
    })
}

/// WorkerFactory.pidCounter: worker ids start at 1.
static NEXT_ID: AtomicU32 = AtomicU32::new(1);
/// WorkerSpawnRunner.requestIdCounter: multiplex request ids start at 1.
static NEXT_REQUEST_ID: AtomicI32 = AtomicI32::new(1);
/// Whether a failure message named a worker log (the logs are kept).
static KEEP_LOGS: AtomicBool = AtomicBool::new(false);

/// A borrowed worker: returned to the pool on drop, or destroyed when a
/// request is in flight (or the worker was invalidated).
pub struct Lease {
    work_parent: PathBuf,
    key: Arc<WorkerKey>,
    worker: Option<Worker>,
}

impl Lease {
    /// The worker's working directory: the spawn's execution root.
    pub fn execroot(&self) -> &Path {
        &self.worker.as_ref().expect("leased").execroot
    }

    /// Whether the worker is sandboxed.
    pub fn sandboxed(&self) -> bool {
        self.key.sandboxed
    }

    /// Builds and sends one request (WorkerSpawnRunner.createWorkRequest:
    /// the flag files expanded, the inputs listed, the request id for a
    /// multiplex key and the verbosity under `--worker_verbose`), and reads
    /// the response. A failure invalidates the worker (it is destroyed when
    /// the lease ends) and returns genuine's message.
    pub fn execute(
        &mut self,
        flag_files: &[String],
        read: &dyn Fn(&str) -> Option<io::Result<Vec<u8>>>,
        inputs: Vec<(String, String)>,
    ) -> Result<WorkResponse, String> {
        let mut arguments = Vec::new();
        for flag_file in flag_files {
            expand_argument(flag_file, read, &mut arguments)
                .map_err(|e| format!("IOException during worker execution: {e}"))?;
        }
        let request = WorkRequest {
            arguments,
            inputs,
            request_id: if self.key.multiplex {
                NEXT_REQUEST_ID.fetch_add(1, Ordering::Relaxed)
            } else {
                0
            },
            verbosity: if options().verbose { VERBOSE_LEVEL } else { 0 },
        };
        let worker = self.worker.as_mut().expect("leased");
        match worker.exchange(&request) {
            Ok(response) if response.was_cancelled => {
                worker.in_flight = true;
                Err(format!(
                    "Received cancel response for {} without having cancelled",
                    response.request_id
                ))
            }
            Ok(response) => Ok(response),
            Err(failure) => {
                worker.in_flight = true;
                if !matches!(failure, WorkerFailure::Start(_)) {
                    KEEP_LOGS.store(true, Ordering::Relaxed);
                }
                Err(failure.message(&self.key.mnemonic, &worker.log))
            }
        }
    }
}

impl Drop for Lease {
    fn drop(&mut self) {
        let mut worker = self.worker.take();
        let reusable = worker.as_mut().is_some_and(|worker| {
            !worker.in_flight
                && worker
                    .process
                    .as_mut()
                    .is_none_or(|process| matches!(process.child.try_wait(), Ok(None)))
        });
        if !reusable && let Some(worker) = worker.as_mut() {
            worker.destroy();
        }
        let pool = pool();
        let mut state = pool.state.lock().unwrap_or_else(|e| e.into_inner());
        let entry = state
            .keys
            .entry((self.work_parent.clone(), self.key.clone()))
            .or_default();
        entry.active = entry.active.saturating_sub(1);
        if reusable && let Some(worker) = worker {
            entry.idle.push(worker);
        }
        drop(state);
        pool.changed.notify_all();
    }
}

/// Borrows a worker of `key` for one spawn (WorkerPool.borrowObject): an
/// idle one, a new one while the key is below its maximum, else waits.
/// `work_parent` is the cache's `work` directory, below which the
/// per-process worker directories live.
pub fn acquire(work_parent: &Path, key: WorkerKey) -> Result<Lease, String> {
    let key = Arc::new(key);
    let options = options();
    let max = if key.multiplex {
        max_instances(
            &options.max_multiplex_instances,
            &key.mnemonic,
            DEFAULT_MAX_MULTIPLEX_WORKERS,
        )
    } else {
        max_instances(
            &options.max_instances,
            &key.mnemonic,
            DEFAULT_MAX_SINGLEPLEX_WORKERS,
        )
    }
    .max(1);
    let pool = pool();
    let map_key = (work_parent.to_path_buf(), key.clone());
    let mut state = pool.state.lock().unwrap_or_else(|e| e.into_inner());
    loop {
        let entry = state.keys.entry(map_key.clone()).or_default();
        if let Some(worker) = entry.idle.pop() {
            entry.active += 1;
            return Ok(Lease {
                work_parent: work_parent.to_path_buf(),
                key,
                worker: Some(worker),
            });
        }
        if entry.active < max {
            entry.active += 1;
            let base = work_parent.join(format!("bazel-workers-{}", std::process::id()));
            if !state.bases.contains(&base) {
                state.bases.push(base.clone());
            }
            drop(state);
            // A lease without a worker yet: its drop releases the slot.
            let mut lease = Lease {
                work_parent: work_parent.to_path_buf(),
                key: key.clone(),
                worker: None,
            };
            let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
            let name = format!("{}-{id}-{}", key.type_name(), key.mnemonic);
            let execroot = base.join(&name).join("execroot");
            let log = base.join(format!("{name}.log"));
            if let Err(error) = fs::create_dir_all(&execroot) {
                return Err(format!(
                    "cannot create worker directory '{}': {error}",
                    execroot.display()
                ));
            }
            lease.worker = Some(Worker {
                id,
                key,
                execroot,
                log,
                process: None,
                in_flight: false,
            });
            return Ok(lease);
        }
        state = pool.changed.wait(state).unwrap_or_else(|e| e.into_inner());
    }
}

impl Drop for Worker {
    fn drop(&mut self) {
        self.destroy();
    }
}

/// Stops every worker (the end of the command: WorkerPool.close). The
/// working directories are removed; the logs too, unless a failure message
/// named one or `--worker_verbose` printed their paths.
pub fn shutdown() {
    let pool = pool();
    let mut state = pool.state.lock().unwrap_or_else(|e| e.into_inner());
    let keys = std::mem::take(&mut state.keys);
    let bases = std::mem::take(&mut state.bases);
    drop(state);
    for (_, mut entry) in keys {
        for mut worker in entry.idle.drain(..) {
            worker.destroy();
        }
    }
    let keep_logs = KEEP_LOGS.load(Ordering::Relaxed) || options().verbose;
    for base in bases {
        if keep_logs {
            if let Ok(entries) = fs::read_dir(&base) {
                for entry in entries.flatten() {
                    if entry.file_type().is_ok_and(|kind| kind.is_dir()) {
                        let _ = crate::action::remove_tree(&entry.path());
                    }
                }
            }
        } else {
            let _ = crate::action::remove_tree(&base);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn strings(items: &[&str]) -> Vec<String> {
        items.iter().map(|item| (*item).to_owned()).collect()
    }

    #[test]
    fn flag_files_split_like_worker_parser() {
        let info = BTreeMap::from([("supports-workers".to_owned(), "1".to_owned())]);
        let config = parse(
            "M",
            &strings(&["tool", "-x", "@p", "--flagfile=q", "@@e"]),
            BTreeMap::new(),
            &info,
        )
        .unwrap();
        assert_eq!(
            config.key.args,
            strings(&["tool", "-x", "--persistent_worker"])
        );
        // The legacy pattern does not exclude @@ (expansion keeps it literal).
        assert_eq!(config.flag_files, strings(&["@p", "--flagfile=q", "@@e"]));
        assert_eq!(config.key.protocol, Protocol::Proto);
        let error = parse("M", &strings(&["tool", "a"]), BTreeMap::new(), &info)
            .err()
            .unwrap();
        assert!(!error.environmental);
        assert_eq!(
            error.message,
            "Worker strategy cannot execute this M action, because the command-line arguments \
             do not contain exactly one @flagfile or --flagfile=\n[tool, a]"
        );
    }

    #[test]
    fn expansion_reads_declared_flag_files_recursively() {
        let read = |path: &str| -> Option<io::Result<Vec<u8>>> {
            match path {
                "p" => Some(Ok(b"a\n@q\n@@lit\n@repo//x:y\r\nb".to_vec())),
                "q" => Some(Ok(b"inner\n".to_vec())),
                _ => None,
            }
        };
        let mut out = Vec::new();
        expand_argument("@p", &read, &mut out).unwrap();
        assert_eq!(out, strings(&["a", "inner", "@@lit", "@repo//x:y", "b"]));
        let error = expand_argument("@missing", &read, &mut Vec::new()).unwrap_err();
        assert_eq!(
            error,
            "Failed to read @-argument 'missing': file is not a declared input"
        );
    }

    #[test]
    fn json_requests_match_protobuf_json_format() {
        // The genuine capture (tests/fixtures/persistent_workers): Gson's
        // HTML-safe escaping, base64 digests of the hex text, defaults
        // omitted.
        let request = WorkRequest {
            arguments: strings(&["--extra", "k=<v>&'q' \u{e9} \"dq\" \\ tab\t \u{2028} /"]),
            inputs: vec![
                ("pkg/a.txt".into(), "ab".into()),
                ("e".into(), String::new()),
            ],
            request_id: 0,
            verbosity: 0,
        };
        assert_eq!(
            String::from_utf8(request.encode_json()).unwrap(),
            "{\"arguments\":[\"--extra\",\"k\\u003d\\u003cv\\u003e\\u0026\\u0027q\\u0027 \u{e9} \
             \\\"dq\\\" \\\\ tab\\t \\u2028 /\"],\"inputs\":[{\"path\":\"pkg/a.txt\",\"digest\":\
             \"YWI=\"},{\"path\":\"e\"}]}\n"
        );
        let request = WorkRequest {
            request_id: 3,
            verbosity: 10,
            ..WorkRequest::default()
        };
        assert_eq!(
            String::from_utf8(request.encode_json()).unwrap(),
            "{\"requestId\":3,\"verbosity\":10}\n"
        );
    }

    #[test]
    fn proto_round_trip_shapes() {
        let request = WorkRequest {
            arguments: strings(&["a"]),
            inputs: vec![("p".into(), "d".into())],
            request_id: 1,
            verbosity: 0,
        };
        assert_eq!(
            request.encode_delimited(),
            vec![
                13, 0x0a, 1, b'a', 0x12, 6, 0x0a, 1, b'p', 0x12, 1, b'd', 0x18, 1
            ]
        );
        let response =
            WorkResponse::decode_proto(&[0x08, 1, 0x12, 2, b'h', b'i', 0x18, 7, 0x28, 5]).unwrap();
        assert_eq!(
            response,
            WorkResponse {
                exit_code: 1,
                output: "hi".into(),
                request_id: 7,
                was_cancelled: false
            }
        );
        let negative = WorkResponse::decode_proto(&[
            0x08, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0x01,
        ])
        .unwrap();
        assert_eq!(negative.exit_code, -1);
        let json =
            WorkResponse::decode_json(br#"{"exitCode":2,"output":"x","requestId":4,"z":[1]}"#)
                .unwrap();
        assert_eq!(
            (json.exit_code, json.output.as_str(), json.request_id),
            (2, "x", 4)
        );
    }

    #[test]
    fn max_instances_follow_worker_pool_impl() {
        let parsed: Vec<_> = ["2", "Javac=5", "Other=auto", "auto"]
            .iter()
            .filter_map(|value| parse_max_instances("worker_max_instances", value).unwrap())
            .collect();
        assert_eq!(max_instances(&parsed, "Javac", 4), 5);
        assert_eq!(max_instances(&parsed, "Other", 4), 4);
        assert_eq!(max_instances(&parsed, "X", 4), 2);
        assert_eq!(max_instances(&[], "X", 8), 8);
        assert!(parse_max_instances("worker_max_instances", "x").is_err());
    }
}
