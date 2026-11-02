//! Build Event Protocol file outputs of `build`, `test` and `run`
//! (docs/BEP.md): `--build_event_json_file`, `--build_event_binary_file`
//! and `--build_event_text_file`, written by the transports of
//! buildeventstream/transports (one event per line in JsonFormat, length-
//! delimited protos, `event { ... }` text blocks).
//!
//! The stream follows BuildEventStreamer.post (runtime/BuildEventStreamer
//! .java:284-389): every event names the events it announces (`children`);
//! an event nobody announced is linked into the stream by a Progress event
//! (`progress { opaque_count: N }` announcing the next progress event and
//! it); the event that posts the last announced id carries `last_message`.
//! Announced ids that never post when the build ends are closed by Aborted
//! events (clearAnnouncedEvents).
//!
//! Off by default: nothing here runs, and nothing is recorded, unless one of
//! the file options is given ([`active`] is one relaxed atomic load). The
//! hooks — [`action_done`] from the scheduler's workers, [`test_result`]
//! from the test runner — record what the final events need; the header
//! events stream when the command starts ([`install`]), the analysis events
//! when the request is planned ([`planned`]: from the planned request, so a
//! snapshot hit streams what its full analysis did), everything else when
//! the command ends ([`finish`]).

pub mod bes;
pub mod plan;
pub mod proto;

use crate::request::Produced;
use plan::BepPlan;
use proto::Msg;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant, SystemTime};

/// The Build Event Protocol options of build/test/run
/// (BuildEventStreamOptions, BuildEventProtocolOptions,
/// BuildEventServiceOptions.besBackend).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Options {
    pub json_file: Option<String>,
    pub binary_file: Option<String>,
    pub text_file: Option<String>,
    /// `--build_event_publish_all_actions`: every executed action posts an
    /// ActionExecuted event, not only the failed ones.
    pub publish_all_actions: bool,
    /// `--build_metadata=KEY=VALUE` (BuildMetadata event), in order.
    pub build_metadata: Vec<(String, String)>,
    /// The Build Event Service upload (`--bes_backend` and its options,
    /// crate::bep::bes).
    pub bes: bes::BesOptions,
}

/// The string-valued options this module parses (name, old name).
const STRING_OPTIONS: &[&str] = &[
    "build_event_json_file",
    "build_event_binary_file",
    "build_event_text_file",
    "build_event_json_file_upload_mode",
    "build_event_binary_file_upload_mode",
    "build_event_text_file_upload_mode",
    "build_event_max_named_set_of_file_entries",
    "build_metadata",
];

/// Boolean options accepted as no-ops: the path conversion of the file
/// transports only selects which artifact uploader turns local paths into
/// `file://` URIs (BuildEventServiceModule.java:830-910) — without a Build
/// Event Service both convert to `file://` URIs.
const BOOLEAN_OPTIONS: &[&str] = &[
    "build_event_json_file_path_conversion",
    "experimental_build_event_json_file_path_conversion",
    "build_event_binary_file_path_conversion",
    "experimental_build_event_binary_file_path_conversion",
    "build_event_text_file_path_conversion",
    "experimental_build_event_text_file_path_conversion",
];

impl Options {
    /// One option of build/test/run: `Some(Ok(used_next))` when it is one
    /// of the BEP options.
    pub fn parse(&mut self, arg: &str, next: Option<&str>) -> Option<Result<bool, String>> {
        if let Some(result) = self.bes.parse(arg, next) {
            return Some(result);
        }
        if let Some(value) =
            crate::configuration::boolean_option(arg, "build_event_publish_all_actions")
        {
            return Some(value.map(|value| {
                self.publish_all_actions = value;
                false
            }));
        }
        for name in BOOLEAN_OPTIONS {
            if let Some(value) = crate::configuration::boolean_option(arg, name) {
                return Some(value.map(|_| false));
            }
        }
        for name in STRING_OPTIONS {
            let flag = format!("--{name}");
            let (value, used) = if arg == flag {
                match next {
                    Some(value) => (value, true),
                    None => return Some(Err(format!("{flag} requires a value"))),
                }
            } else if let Some(value) = arg.strip_prefix(&format!("{flag}=")) {
                (value, false)
            } else {
                continue;
            };
            return Some(self.set(name, value).map(|()| used));
        }
        None
    }

    fn set(&mut self, name: &str, value: &str) -> Result<(), String> {
        let file = |value: &str| (!value.is_empty()).then(|| value.to_owned());
        match name {
            "build_event_json_file" => self.json_file = file(value),
            "build_event_binary_file" => self.binary_file = file(value),
            "build_event_text_file" => self.text_file = file(value),
            "build_event_json_file_upload_mode"
            | "build_event_binary_file_upload_mode"
            | "build_event_text_file_upload_mode" => {
                // BuildEventStreamOptions.UploadModeConverter.
                if !matches!(
                    value,
                    "wait_for_upload_complete" | "nowait_for_upload_complete" | "fully_async"
                ) {
                    return Err(format!(
                        "While parsing option --{name}={value}: Not a valid upload mode: '{value}' (should be wait_for_upload_complete, nowait_for_upload_complete or fully_async)"
                    ));
                }
            }
            "build_event_max_named_set_of_file_entries" => {
                value.parse::<i32>().map_err(|_| {
                    format!("While parsing option --{name}={value}: '{value}' is not an int")
                })?;
            }
            "build_metadata" => {
                // AssignmentConverter.
                let (key, assigned) = value.split_once('=').ok_or_else(|| {
                    format!(
                        "While parsing option --build_metadata={value}: Variable definitions must be in the form of a 'name=value' assignment"
                    )
                })?;
                self.build_metadata
                    .push((key.to_owned(), assigned.to_owned()));
            }
            _ => {}
        }
        Ok(())
    }

    /// Whether a file transport is requested.
    pub fn files(&self) -> bool {
        self.json_file.is_some() || self.binary_file.is_some() || self.text_file.is_some()
    }

    /// Whether a transport (a file or the Build Event Service) is requested.
    pub fn enabled(&self) -> bool {
        self.files() || self.bes.enabled()
    }

    /// The explicit errors of options this profile does not implement.
    pub fn validate(&self) -> Result<(), String> {
        self.bes.validate()
    }
}

/// One command option as the command line events show it.
#[derive(Clone, Debug, Default)]
pub struct OptionEntry {
    /// `--name=value` / `--noname` as given (after rc expansion).
    pub combined: String,
    pub name: String,
    pub value: String,
    /// `command line options`, or the rc file it came from.
    pub source: String,
    /// Given on the command line (OptionsParsed.explicit_cmd_line).
    pub explicit: bool,
}

/// What the command line events need, known when the command starts.
#[derive(Clone, Debug, Default)]
pub struct Header {
    pub command: String,
    pub working_directory: PathBuf,
    pub workspace_directory: PathBuf,
    /// Startup options (`--batch`-style, as given).
    pub startup: Vec<String>,
    /// The command's arguments as given (options and residue).
    pub arguments: Vec<String>,
    pub options: Vec<OptionEntry>,
    /// The residue: target patterns (and `run`'s arguments).
    pub residue: Vec<String>,
    /// The target patterns of the PatternExpanded event.
    pub patterns: Vec<String>,
    /// `<output base>/execroot/_main`.
    pub execution_root: PathBuf,
    /// `--profile` (listed in BuildToolLogs).
    pub profile: Option<PathBuf>,
}

/// A BuildEventId message.
mod id {
    use super::Msg;

    pub fn progress(count: i32) -> Msg {
        Msg::new().msg(2, "progress", Msg::new().i32(1, "opaque_count", count))
    }
    pub fn started() -> Msg {
        Msg::new().msg(3, "started", Msg::new())
    }
    pub fn unstructured_command_line() -> Msg {
        Msg::new().msg(11, "unstructured_command_line", Msg::new())
    }
    pub fn structured_command_line(label: &str) -> Msg {
        Msg::new().msg(
            18,
            "structured_command_line",
            Msg::new().str(1, "command_line_label", label),
        )
    }
    pub fn workspace_status() -> Msg {
        Msg::new().msg(14, "workspace_status", Msg::new())
    }
    pub fn options_parsed() -> Msg {
        Msg::new().msg(12, "options_parsed", Msg::new())
    }
    pub fn configuration(id: &str) -> Msg {
        Msg::new().msg(15, "configuration", configuration_id(id))
    }
    pub fn configuration_id(id: &str) -> Msg {
        Msg::new().str(1, "id", id)
    }
    pub fn target_configured(label: &str) -> Msg {
        Msg::new().msg(16, "target_configured", Msg::new().str(1, "label", label))
    }
    pub fn pattern(patterns: &[String]) -> Msg {
        Msg::new().msg(
            4,
            "pattern",
            Msg::new().strs(1, "pattern", patterns.iter().cloned()),
        )
    }
    pub fn named_set(id: usize) -> Msg {
        Msg::new().msg(13, "named_set", Msg::new().str(1, "id", id.to_string()))
    }
    pub fn target_completed(label: &str, configuration: &str) -> Msg {
        Msg::new().msg(
            5,
            "target_completed",
            Msg::new().str(1, "label", label).msg(
                3,
                "configuration",
                configuration_id(configuration),
            ),
        )
    }
    pub fn action_completed(primary_output: &str, label: &str, configuration: &str) -> Msg {
        Msg::new().msg(
            6,
            "action_completed",
            Msg::new()
                .str(1, "primary_output", primary_output)
                .str(2, "label", label)
                .msg(3, "configuration", configuration_id(configuration)),
        )
    }
    pub fn test_result(
        label: &str,
        configuration: &str,
        run: i32,
        shard: i32,
        attempt: i32,
    ) -> Msg {
        Msg::new().msg(
            8,
            "test_result",
            Msg::new()
                .str(1, "label", label)
                .i32(2, "run", run)
                .i32(3, "shard", shard)
                .i32(4, "attempt", attempt)
                .msg(5, "configuration", configuration_id(configuration)),
        )
    }
    pub fn test_summary(label: &str, configuration: &str) -> Msg {
        Msg::new().msg(
            7,
            "test_summary",
            Msg::new().str(1, "label", label).msg(
                2,
                "configuration",
                configuration_id(configuration),
            ),
        )
    }
    pub fn build_finished() -> Msg {
        Msg::new().msg(9, "build_finished", Msg::new())
    }
    pub fn build_tool_logs() -> Msg {
        Msg::new().msg(20, "build_tool_logs", Msg::new())
    }
    pub fn build_metrics() -> Msg {
        Msg::new().msg(22, "build_metrics", Msg::new())
    }
    pub fn workspace() -> Msg {
        Msg::new().msg(23, "workspace", Msg::new())
    }
    pub fn build_metadata() -> Msg {
        Msg::new().msg(24, "build_metadata", Msg::new())
    }
    pub fn convenience_symlinks_identified() -> Msg {
        Msg::new().msg(25, "convenience_symlinks_identified", Msg::new())
    }
    pub fn exec_request() -> Msg {
        Msg::new().msg(28, "exec_request", Msg::new())
    }
}

/// One BuildEvent: id, children, payload (field number, name, message).
pub struct Event {
    id: Msg,
    children: Vec<Msg>,
    payload: (u32, &'static str, Msg),
}

impl Event {
    fn new(id: Msg, payload: (u32, &'static str, Msg)) -> Self {
        Self {
            id,
            children: Vec::new(),
            payload,
        }
    }

    fn children(mut self, children: Vec<Msg>) -> Self {
        self.children = children;
        self
    }

    fn message(&self, last: bool) -> Msg {
        Msg::new()
            .msg(1, "id", self.id.clone())
            .msgs(2, "children", self.children.iter().cloned())
            .bool(20, "last_message", last)
            .msg(self.payload.0, self.payload.1, self.payload.2.clone())
    }
}

/// One transport: a file, or the Build Event Service upload.
enum Transport {
    Json(std::io::BufWriter<std::fs::File>),
    Binary(std::io::BufWriter<std::fs::File>),
    Text(std::io::BufWriter<std::fs::File>),
    Bes(std::sync::Arc<bes::Uploader>),
}

impl Transport {
    fn write(&mut self, event: &Msg) -> std::io::Result<()> {
        match self {
            Transport::Json(out) => {
                out.write_all(event.to_json().as_bytes())?;
                out.write_all(b"\n")
            }
            Transport::Binary(out) => out.write_all(&proto::delimited(&event.encode())),
            // TextFormatFileTransport.serializeEvent (:52).
            Transport::Text(out) => {
                out.write_all(format!("event {{\n{}}}\n\n", event.to_text()).as_bytes())
            }
            // BuildEventServiceTransport.sendBuildEvent.
            Transport::Bes(uploader) => {
                uploader.enqueue(event.encode());
                Ok(())
            }
        }
    }

    fn flush(&mut self) -> std::io::Result<()> {
        match self {
            Transport::Json(out) | Transport::Binary(out) | Transport::Text(out) => out.flush(),
            Transport::Bes(_) => Ok(()),
        }
    }
}

/// BuildEventStreamer's announcement bookkeeping over the transports.
struct Streamer {
    transports: Vec<(PathBuf, Transport)>,
    announced: HashSet<Vec<u8>>,
    posted: HashSet<Vec<u8>>,
    progress_count: i32,
    started: bool,
    /// The first write error (reported once when the command ends).
    error: Option<String>,
}

impl Streamer {
    fn send(&mut self, event: &Event, last: bool) {
        let message = event.message(last);
        for (path, transport) in &mut self.transports {
            if let Err(error) = transport.write(&message)
                && self.error.is_none()
            {
                self.error = Some(format!("{}: {error}", path.display()));
            }
        }
    }

    /// BuildEventStreamer.post: link an unannounced event through a
    /// progress event, announce the event's children, mark the last one.
    fn post(&mut self, event: Event) {
        let key = event.id.encode();
        if !self.started {
            self.started = true;
            self.announced.insert(key.clone());
        } else if !self.announced.contains(&key) {
            let progress = Event::new(
                id::progress(self.progress_count),
                (3, "progress", Msg::new()),
            )
            .children(vec![
                id::progress(self.progress_count + 1),
                event.id.clone(),
            ]);
            self.progress_count += 1;
            for child in &progress.children {
                self.announced.insert(child.encode());
            }
            self.posted.insert(progress.id.encode());
            self.send(&progress, false);
        }
        self.posted.insert(key);
        for child in &event.children {
            self.announced.insert(child.encode());
        }
        let last = self.announced.len() == self.posted.len();
        self.send(&event, last);
    }

    fn is_posted(&self, id: &Msg) -> bool {
        self.posted.contains(&id.encode())
    }

    fn flush(&mut self) {
        for (path, transport) in &mut self.transports {
            if let Err(error) = transport.flush()
                && self.error.is_none()
            {
                self.error = Some(format!("{}: {error}", path.display()));
            }
        }
    }
}

/// One finished action, as the scheduler reported it.
struct ActionRecord {
    id: String,
    mnemonic: String,
    /// The primary output's exec path.
    primary_output: String,
    success: bool,
    cache_hit: bool,
    /// A remote cache hit / a remotely executed spawn.
    remote_hit: bool,
    remote_executed: bool,
    exit_code: i32,
    error: Option<String>,
    argv: Vec<String>,
    start: SystemTime,
    end: SystemTime,
}

/// One test action result (TestResult event).
struct TestRecord {
    label: String,
    run: i32,
    shard: i32,
    attempt: i32,
    status: crate::test_command::TestStatus,
    cached: bool,
    cached_remotely: bool,
    start: SystemTime,
    duration: Duration,
    log: Option<PathBuf>,
    xml: Option<PathBuf>,
    strategy: String,
}

/// One test summary (TestSummary event).
pub struct TestSummaryRecord {
    pub label: String,
    pub status: crate::test_command::TestStatus,
    pub total_run_count: i32,
    pub shard_count: i32,
    pub num_cached: i32,
    pub passed: Vec<PathBuf>,
    pub failed: Vec<PathBuf>,
}

/// What [`planned`] recorded of the request.
struct PlannedState {
    plan: BepPlan,
    /// The request's reported artifacts (label, produced).
    requested: Vec<Produced>,
    execute: bool,
    /// --keep_going: the requested targets whose analysis failed.
    analysis_failed: Vec<String>,
    actions_created: usize,
    created_by_mnemonic: BTreeMap<String, usize>,
}

/// `run`'s exec request (ExecRequestConstructed).
pub struct ExecRequest {
    pub working_directory: PathBuf,
    pub argv: Vec<std::ffi::OsString>,
    pub environment: Vec<(std::ffi::OsString, std::ffi::OsString)>,
}

struct State {
    streamer: Streamer,
    header: Header,
    publish_all_actions: bool,
    uuid: String,
    start: SystemTime,
    clock: Instant,
    planned: Option<PlannedState>,
    actions: Vec<ActionRecord>,
    /// Successful actions' output paths, by action id.
    outputs: HashMap<String, Vec<PathBuf>>,
    tests: Vec<TestRecord>,
    summaries: Vec<TestSummaryRecord>,
    exec_request: Option<ExecRequest>,
    /// The computed workspace status (key, value), sorted.
    status: Vec<(String, String)>,
    finished: bool,
    /// The Build Event Service upload, if any.
    bes: Option<std::sync::Arc<bes::Uploader>>,
    /// Whether a file transport was requested.
    files: bool,
}

static ACTIVE: AtomicBool = AtomicBool::new(false);
static STATE: Mutex<Option<State>> = Mutex::new(None);

/// Whether a build event file is being written (the hooks' guard).
#[inline]
pub fn active() -> bool {
    ACTIVE.load(Ordering::Relaxed)
}

fn with_state<R>(f: impl FnOnce(&mut State) -> R) -> Option<R> {
    let mut guard = STATE.lock().unwrap_or_else(|e| e.into_inner());
    guard.as_mut().filter(|state| !state.finished).map(f)
}

/// The invocation id (BuildStarted.uuid), shared with the profile's
/// `build_id`: a random version 4 UUID, made once per process.
pub fn invocation_id() -> &'static str {
    static ID: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    ID.get_or_init(random_uuid)
}

/// The build request id (CommandEnvironment.getBuildRequestId: the BES
/// StreamId.build_id and RequestMetadata.correlated_invocations_id), a
/// random version 4 UUID made once per process.
pub fn build_request_id() -> &'static str {
    static ID: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    ID.get_or_init(random_uuid)
}

fn random_uuid() -> String {
    {
        let mut bytes = [0u8; 16];
        // snapshot-unrecorded: randomness for the invocation id.
        if let Ok(mut file) = std::fs::File::open("/dev/urandom") {
            use std::io::Read as _;
            let _ = file.read_exact(&mut bytes);
        }
        if bytes == [0; 16] {
            let nanos = SystemTime::now()
                .duration_since(SystemTime::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos();
            bytes = (nanos ^ (u128::from(std::process::id()) << 64)).to_le_bytes();
        }
        bytes[6] = (bytes[6] & 0x0f) | 0x40;
        bytes[8] = (bytes[8] & 0x3f) | 0x80;
        let hex: String = bytes.iter().map(|byte| format!("{byte:02x}")).collect();
        format!(
            "{}-{}-{}-{}-{}",
            &hex[0..8],
            &hex[8..12],
            &hex[12..16],
            &hex[16..20],
            &hex[20..32]
        )
    }
}

fn millis(time: SystemTime) -> i64 {
    time.duration_since(SystemTime::UNIX_EPOCH)
        .map(|duration| duration.as_millis() as i64)
        .unwrap_or_default()
}

fn host_name() -> String {
    // snapshot-unrecorded: the host name of BuildStarted.
    std::fs::read_to_string("/proc/sys/kernel/hostname")
        .map(|name| name.trim().to_owned())
        .unwrap_or_default()
}

/// The user name (Java's `user.name`: the password entry of this process's
/// uid, else `$USER`).
fn user_name() -> String {
    let uid = rustix::process::getuid().as_raw().to_string();
    // snapshot-unrecorded: the user name of BuildStarted.
    std::fs::read_to_string("/etc/passwd")
        .ok()
        .and_then(|passwd| {
            passwd.lines().find_map(|line| {
                let mut fields = line.split(':');
                let name = fields.next()?;
                (fields.nth(1)? == uid).then(|| name.to_owned())
            })
        })
        .or_else(|| std::env::var("USER").ok())
        .unwrap_or_default()
}

/// Open the requested files (and start the Build Event Service upload) and
/// stream the command's first events (BuildStarted and the command line
/// events). A file that cannot be created is an error of the command (exit
/// 2), a BES client that cannot be created too (exit 36).
pub fn install(options: &Options, header: Header) -> Result<(), (i32, String)> {
    if !options.enabled() {
        return Ok(());
    }
    let mut transports = Vec::new();
    let open = |path: &str| -> Result<(PathBuf, std::io::BufWriter<std::fs::File>), (i32, String)> {
        let path = header.working_directory.join(path);
        // snapshot-unrecorded: a build event file (a write).
        let file = std::fs::File::create(&path).map_err(|error| {
            (
                2,
                format!(
                    "Unable to write the build event file {}: {error}",
                    path.display()
                ),
            )
        })?;
        Ok((path, std::io::BufWriter::new(file)))
    };
    if let Some(path) = &options.text_file {
        let (path, out) = open(path)?;
        transports.push((path, Transport::Text(out)));
    }
    if let Some(path) = &options.binary_file {
        let (path, out) = open(path)?;
        transports.push((path, Transport::Binary(out)));
    }
    if let Some(path) = &options.json_file {
        let (path, out) = open(path)?;
        transports.push((path, Transport::Json(out)));
    }
    // createBesTransport (after the file transports); a client that cannot
    // be created is BES_INITIALIZATION_ERROR (exit 36).
    let bes = match options.bes.enabled() {
        true => {
            let uploader =
                bes::Uploader::start(&options.bes, &header.command).map_err(|error| (36, error))?;
            transports.push((
                PathBuf::from(&options.bes.backend),
                Transport::Bes(std::sync::Arc::clone(&uploader)),
            ));
            // constructAndMaybeReportInvocationIdUrl.
            if let Some(prefix) = options.bes.results_prefix() {
                crate::note!(
                    "INFO: Streaming build results to: {prefix}{}",
                    invocation_id()
                );
            }
            Some(uploader)
        }
        false => None,
    };
    let (clock, start) = crate::process_clock();
    let mut state = State {
        streamer: Streamer {
            transports,
            announced: HashSet::new(),
            posted: HashSet::new(),
            progress_count: 0,
            started: false,
            error: None,
        },
        header,
        publish_all_actions: options.publish_all_actions,
        uuid: invocation_id().to_owned(),
        start,
        clock,
        planned: None,
        actions: Vec::new(),
        outputs: HashMap::new(),
        tests: Vec::new(),
        summaries: Vec::new(),
        exec_request: None,
        status: Vec::new(),
        finished: false,
        bes,
        files: options.files(),
    };
    post_header(&mut state, &options.build_metadata);
    state.streamer.flush();
    *STATE.lock().unwrap_or_else(|e| e.into_inner()) = Some(state);
    ACTIVE.store(true, Ordering::Relaxed);
    Ok(())
}

/// BuildStarted, BuildMetadata, the unstructured and structured command
/// lines and OptionsParsed (BuildEventStreamer's first events,
/// BlazeCommandDispatcher / CommandLineEvent).
fn post_header(state: &mut State, metadata: &[(String, String)]) {
    let header = &state.header;
    let explicit_options: Vec<String> = header
        .options
        .iter()
        .filter(|option| option.explicit)
        .map(|option| option.combined.clone())
        .collect();
    // BuildStartingEvent's options description: the command options,
    // each `--name=value` with the value shell-quoted.
    let description = header
        .options
        .iter()
        .map(|option| match option.combined.split_once('=') {
            Some((name, value)) => format!("{name}={}", crate::args::shell_escape(value)),
            None => option.combined.clone(),
        })
        .collect::<Vec<_>>()
        .join(" ");
    let children = vec![
        id::progress(0),
        id::unstructured_command_line(),
        id::structured_command_line("original"),
        id::structured_command_line("canonical"),
        id::structured_command_line("tool"),
        id::build_metadata(),
        id::options_parsed(),
        id::workspace_status(),
        id::pattern(&header.patterns),
        id::build_finished(),
    ];
    let started = Msg::new()
        .str(1, "uuid", state.uuid.clone())
        .i64(2, "start_time_millis", millis(state.start))
        .timestamp_millis(9, "start_time", millis(state.start))
        .str(3, "build_tool_version", crate::COMPATIBILITY_VERSION)
        .str(4, "options_description", description)
        .str(5, "command", header.command.clone())
        .str(
            6,
            "working_directory",
            header.working_directory.display().to_string(),
        )
        .str(
            7,
            "workspace_directory",
            header.workspace_directory.display().to_string(),
        )
        .i64(8, "server_pid", i64::from(std::process::id()))
        .str(10, "host", host_name())
        .str(11, "user", user_name());
    let mut events = vec![
        Event::new(id::started(), (5, "started", started)).children(children),
        Event::new(
            id::build_metadata(),
            (
                26,
                "build_metadata",
                Msg::new().map(1, "metadata", metadata.iter().cloned()),
            ),
        ),
        Event::new(
            id::unstructured_command_line(),
            (
                12,
                "unstructured_command_line",
                Msg::new().strs(
                    1,
                    "args",
                    std::iter::once(header.command.clone()).chain(header.arguments.iter().cloned()),
                ),
            ),
        ),
        Event::new(
            id::options_parsed(),
            (
                13,
                "options_parsed",
                Msg::new()
                    .strs(1, "startup_options", header.startup.iter().cloned())
                    .strs(
                        2,
                        "explicit_startup_options",
                        header.startup.iter().cloned(),
                    )
                    .strs(
                        3,
                        "cmd_line",
                        header.options.iter().map(|option| option.combined.clone()),
                    )
                    .strs(4, "explicit_cmd_line", explicit_options)
                    .msg(5, "invocation_policy", Msg::new()),
            ),
        ),
        Event::new(
            id::structured_command_line("original"),
            (
                22,
                "structured_command_line",
                command_line(header, "original"),
            ),
        ),
        Event::new(
            id::structured_command_line("tool"),
            (22, "structured_command_line", Msg::new()),
        ),
    ];
    for event in events.drain(..) {
        state.streamer.post(event);
    }
}

/// A command_line.CommandLine (CommandLineEvent): the executable, startup
/// options, command, command options and residue sections.
fn command_line(header: &Header, label: &str) -> Msg {
    let chunks = |name: &str, chunks: Vec<String>| {
        Msg::new().str(1, "section_label", name).msg(
            2,
            "chunk_list",
            Msg::new().strs(1, "chunk", chunks),
        )
    };
    let startup = Msg::new().str(1, "section_label", "startup options").msg(
        3,
        "option_list",
        Msg::new().msgs(
            1,
            "option",
            header.startup.iter().map(|option| {
                let (name, value) = split_option(option);
                Msg::new()
                    .str(1, "combined_form", option.clone())
                    .str(2, "option_name", name)
                    .str(3, "option_value", value)
                    .str(6, "source", "command line")
            }),
        ),
    );
    let options = Msg::new().str(1, "section_label", "command options").msg(
        3,
        "option_list",
        Msg::new().msgs(
            1,
            "option",
            header.options.iter().map(|option| {
                Msg::new()
                    .str(1, "combined_form", option.combined.clone())
                    .str(2, "option_name", option.name.clone())
                    .str(3, "option_value", option.value.clone())
                    .str(6, "source", option.source.clone())
            }),
        ),
    );
    Msg::new().str(1, "command_line_label", label).msgs(
        2,
        "sections",
        [
            chunks("executable", vec!["bazel".to_owned()]),
            startup,
            chunks("command", vec![header.command.clone()]),
            options,
            chunks("residual", header.residue.clone()),
        ],
    )
}

/// `--name=value` → (name, value); `--noname` → (name, "0"); `--name` →
/// (name, "1") (the canonical forms of the structured command line).
pub fn split_option(option: &str) -> (String, String) {
    let text = option.trim_start_matches('-');
    match text.split_once('=') {
        Some((name, value)) => (name.to_owned(), value.to_owned()),
        None => match text.strip_prefix("no") {
            Some(name) if !name.is_empty() => (name.to_owned(), "0".to_owned()),
            _ => (text.to_owned(), "1".to_owned()),
        },
    }
}

/// The requested targets are planned: stream PatternExpanded, the
/// workspace and canonical command line events, the configurations and
/// each target's TargetConfigured (and, executing, the workspace status).
pub fn planned(request: &crate::request::PlannedRequest) {
    if !active() {
        return;
    }
    with_state(|state| {
        let mut created_by_mnemonic: BTreeMap<String, usize> = BTreeMap::new();
        for node in &request.nodes {
            *created_by_mnemonic
                .entry(crate::profile::mnemonic(&node.action).to_owned())
                .or_default() += 1;
        }
        let analysis_failed = request
            .keep_going
            .as_ref()
            .map(|plan| {
                plan.failed
                    .iter()
                    .map(|(label, _)| crate::test_runner::label_print(label))
                    .collect()
            })
            .unwrap_or_default();
        state.planned = Some(PlannedState {
            plan: request.bep.clone(),
            requested: request
                .requested
                .iter()
                .map(|(_, produced)| produced.clone())
                .collect(),
            execute: request.execute,
            analysis_failed,
            actions_created: request.nodes.len(),
            created_by_mnemonic,
        });
        post_planned(state);
        state.streamer.flush();
    });
}

/// Publish only known incompatible roots after fail-fast analysis. No ordinary
/// target completion, output files or actions are fabricated for a failed plan.
pub fn failed_compatibility(plan: plan::BepPlan) {
    if !active() || plan.targets.is_empty() {
        return;
    }
    with_state(|state| {
        state.planned = Some(PlannedState {
            plan,
            requested: Vec::new(),
            execute: false,
            analysis_failed: Vec::new(),
            actions_created: 0,
            created_by_mnemonic: BTreeMap::new(),
        });
        post_planned(state);
        state.streamer.flush();
    });
}

fn post_planned(state: &mut State) {
    let Some(planned) = &state.planned else {
        return;
    };
    let header = &state.header;
    let mut labels: Vec<String> = planned
        .plan
        .targets
        .iter()
        .map(|target| target.label.clone())
        .collect();
    for label in &planned.analysis_failed {
        if !labels.contains(label) {
            labels.push(label.clone());
        }
    }
    let mut events = vec![
        Event::new(id::pattern(&header.patterns), (6, "expanded", Msg::new())).children(
            labels
                .iter()
                .map(|label| id::target_configured(label))
                .collect(),
        ),
        Event::new(
            id::workspace(),
            (
                25,
                "workspace_info",
                Msg::new().str(
                    1,
                    "local_exec_root",
                    header.execution_root.display().to_string(),
                ),
            ),
        ),
        Event::new(
            id::structured_command_line("canonical"),
            (
                22,
                "structured_command_line",
                command_line(header, "canonical"),
            ),
        ),
    ];
    let execute = planned.execute;
    let status = state.status.as_slice();
    let mut configurations_posted = vec![false; planned.plan.configurations.len()];
    let mut status_posted = false;
    let mut post_configuration =
        |index: usize, events: &mut Vec<Event>, status_posted: &mut bool| {
            if configurations_posted.get(index).copied() != Some(false) {
                return;
            }
            configurations_posted[index] = true;
            events.push(configuration_event(&planned.plan.configurations[index]));
            if execute && !*status_posted {
                *status_posted = true;
                events.push(workspace_status_event(status));
            }
        };
    if !planned.plan.configurations.is_empty() {
        post_configuration(0, &mut events, &mut status_posted);
    }
    for target in &planned.plan.targets {
        post_configuration(target.configuration, &mut events, &mut status_posted);
        let configuration = planned
            .plan
            .configurations
            .get(target.configuration)
            .map_or("none", |configuration| configuration.id.as_str());
        let mut configured = Msg::new().str(1, "target_kind", target.kind.clone());
        if let Some((size, _)) = &target.test {
            configured = configured.enumeration(2, "test_size", test_size(size));
        }
        configured = configured.strs(3, "tag", target.tags.iter().cloned());
        events.push(
            Event::new(
                id::target_configured(&target.label),
                (18, "configured", configured),
            )
            .children(vec![id::target_completed(&target.label, configuration)]),
        );
    }
    for event in events {
        state.streamer.post(event);
    }
}

fn test_size(size: &str) -> (i32, &'static str) {
    match size {
        "SMALL" => (1, "SMALL"),
        "MEDIUM" => (2, "MEDIUM"),
        "LARGE" => (3, "LARGE"),
        "ENORMOUS" => (4, "ENORMOUS"),
        _ => (0, "UNKNOWN"),
    }
}

fn configuration_event(configuration: &plan::BepConfiguration) -> Event {
    Event::new(
        id::configuration(&configuration.id),
        (
            17,
            "configuration",
            Msg::new()
                .str(1, "mnemonic", configuration.mnemonic.clone())
                .str(2, "platform_name", configuration.platform_name.clone())
                .str(3, "cpu", configuration.cpu.clone())
                .map(
                    4,
                    "make_variable",
                    configuration.make_variables.iter().cloned(),
                )
                .bool(5, "is_tool", configuration.is_tool),
        ),
    )
}

/// The WorkspaceStatus event: the stable and volatile keys Bazel's
/// workspace status action writes (BazelWorkspaceStatusModule; empty
/// without an execution phase).
fn workspace_status_event(status: &[(String, String)]) -> Event {
    let items = status.iter().map(|(key, value)| {
        Msg::new()
            .str(1, "key", key.clone())
            .str(2, "value", value.clone())
    });
    Event::new(
        id::workspace_status(),
        (16, "workspace_status", Msg::new().msgs(1, "item", items)),
    )
}

/// The scheduler finished action `id` (any worker thread): its outcome,
/// for ActionExecuted events, TargetComplete's files and the metrics.
pub fn action_done(
    action: &crate::action::Action,
    result: &Result<crate::action::ActionResult, String>,
    start: SystemTime,
    end: SystemTime,
) {
    if !active() {
        return;
    }
    // The workspace status is one action (two native nodes).
    if action.id == crate::request::VOLATILE_STATUS_ID {
        return;
    }
    let mut guard = STATE.lock().unwrap_or_else(|e| e.into_inner());
    let Some(state) = guard.as_mut() else {
        return;
    };
    let success = result.is_ok();
    let cache_hit = result.as_ref().is_ok_and(|result| result.cache_hit);
    let keep = !success || (state.publish_all_actions && !cache_hit);
    if let Ok(result) = result {
        state
            .outputs
            .insert(action.id.clone(), result.outputs.clone());
    }
    let error = result.as_ref().err().cloned();
    state.actions.push(ActionRecord {
        id: action.id.clone(),
        mnemonic: crate::profile::mnemonic(action).to_owned(),
        primary_output: action
            .outputs
            .first()
            .map(|path| path.display().to_string())
            .unwrap_or_default(),
        success,
        cache_hit,
        remote_hit: result.as_ref().is_ok_and(|result| result.remote_hit),
        remote_executed: result.as_ref().is_ok_and(|result| result.remote_executed),
        exit_code: error.as_deref().map_or(0, exit_code_of),
        error,
        argv: if keep && action.write.is_none() && action.extra.transform.is_none() {
            crate::action::arguments(action)
        } else {
            Vec::new()
        },
        start,
        end,
    });
}

/// The exit code in a failed action's error (`failed (exit status: N)`).
fn exit_code_of(error: &str) -> i32 {
    error
        .split_once("exit status: ")
        .and_then(|(_, rest)| {
            rest.split(|c: char| !c.is_ascii_digit())
                .next()
                .and_then(|code| code.parse().ok())
        })
        .unwrap_or(1)
}

/// One test action's result (TestResult event; TestRunnerAction's
/// TestAttempt): run, shard and attempt are 1-based.
#[allow(clippy::too_many_arguments)]
pub fn test_result(
    label: &str,
    run: u32,
    shard: u32,
    result: &crate::test_command::RunResult,
    start: SystemTime,
    duration: Duration,
    log: Option<PathBuf>,
    xml: Option<PathBuf>,
    strategy: &str,
) {
    if !active() {
        return;
    }
    with_state(|state| {
        state.tests.push(TestRecord {
            label: label.to_owned(),
            run: run as i32 + 1,
            shard: shard as i32 + 1,
            attempt: result.test_times.len().max(1) as i32,
            status: result.status,
            cached: result.cached,
            cached_remotely: result.remote_cached,
            start,
            duration,
            log,
            xml,
            strategy: strategy.to_owned(),
        });
    });
}

/// The test summaries of `bazel test` (TestSummary events).
pub fn test_summaries(summaries: Vec<TestSummaryRecord>) {
    if !active() {
        return;
    }
    with_state(|state| state.summaries = summaries);
}

/// The workspace status the request's finalize computed (its stable and
/// volatile keys; the WorkspaceStatus event's items, sorted by key).
pub fn workspace_status(status: &crate::workspace_status::WorkspaceStatus) {
    if !active() {
        return;
    }
    with_state(|state| {
        let mut items: BTreeMap<String, String> = status.stable.clone();
        items.extend(status.volatile.clone());
        state.status = items.into_iter().collect();
    });
}

/// `run`: the command the client executes (ExecRequestConstructed).
pub fn exec_request(request: ExecRequest) {
    if !active() {
        return;
    }
    with_state(|state| state.exec_request = Some(request));
}

/// The ExitCode name of a Bazel exit code (util/ExitCode.java).
fn exit_code_name(code: i32) -> &'static str {
    match code {
        0 => "SUCCESS",
        1 => "BUILD_FAILURE",
        2 => "COMMAND_LINE_ERROR",
        3 => "TESTS_FAILED",
        4 => "NO_TESTS_FOUND",
        6 => "RUN_FAILURE",
        7 => "ANALYSIS_FAILURE",
        8 => "INTERRUPTED",
        9 => "LOCK_HELD_NOBLOCK_FOR_LOCK",
        33 => "OUT_OF_MEMORY",
        34 => "REMOTE_ERROR",
        36 => "LOCAL_ENVIRONMENTAL_ERROR",
        37 => "BLAZE_INTERNAL_ERROR",
        _ => "UNKNOWN",
    }
}

/// A BEP File of an artifact: name relative to its root, the root's path
/// prefix, its URI, digest and length.
fn artifact_file(exec: &str, location: &Path, execution_root: &Path, workspace: &Path) -> Msg {
    let (prefix, name): (Vec<String>, String) = {
        let parts: Vec<&str> = exec.split('/').collect();
        if parts.len() > 3 && parts[0] == "bazel-out" {
            (
                parts[..3].iter().map(|part| (*part).to_owned()).collect(),
                parts[3..].join("/"),
            )
        } else {
            (Vec::new(), exec.to_owned())
        }
    };
    let uri = if location.is_absolute() {
        location.to_path_buf()
    } else if prefix.is_empty() {
        workspace.join(location)
    } else {
        execution_root.join(location)
    };
    // A remote-only output (Build without the Bytes): its remote CAS URI
    // (ByteStreamBuildEventArtifactUploader's `bytestream://`) with a gRPC
    // cache; with an HTTP cache Bazel keeps LocalFilesArtifactUploader
    // (`file://`), the digest and length from the output's metadata.
    if !uri.exists()
        && let Some(file) = crate::remote_outputs::by_exec(Path::new(exec))
    {
        let location = match crate::remote_cache::active().and_then(|r| r.bytestream_prefix()) {
            Some(remote) => format!("{remote}blobs/{}/{}", file.hash, file.size),
            None => file_uri(&uri),
        };
        return Msg::new()
            .str(1, "name", name)
            .str(2, "uri", location)
            .strs(4, "path_prefix", prefix)
            .str(5, "digest", file.hash)
            .i64(6, "length", file.size as i64);
    }
    let (digest, length) = file_digest(&uri);
    Msg::new()
        .str(1, "name", name)
        .str(2, "uri", file_uri(&uri))
        .strs(4, "path_prefix", prefix)
        .str(5, "digest", digest)
        .i64(6, "length", length)
}

/// PathConverter.FileUriPathConverter: `file://` and the absolute path,
/// percent-encoded like java.net.URI's path component.
pub fn file_uri(path: &Path) -> String {
    use std::os::unix::ffi::OsStrExt as _;
    let mut out = String::from("file://");
    for &byte in path.as_os_str().as_bytes() {
        if byte.is_ascii_alphanumeric() || b"/-._~!$&'()*+,;=:@".contains(&byte) {
            out.push(byte as char);
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}

fn file_digest(path: &Path) -> (String, i64) {
    // snapshot-unrecorded: digests of built outputs for the build event file.
    let Ok(mut file) = std::fs::File::open(path) else {
        return (String::new(), 0);
    };
    let size = file.metadata().map(|metadata| metadata.len()).unwrap_or(0);
    match crate::digest_memo::sha256_reader(&mut file, size) {
        Ok((digest, length)) => (
            digest.iter().map(|byte| format!("{byte:02x}")).collect(),
            length as i64,
        ),
        Err(_) => (String::new(), 0),
    }
}

/// The owner label of an action id (`//pkg:name#suffix`).
fn action_label(id: &str) -> String {
    if id.starts_with(crate::request::STATUS_ID_PREFIX) {
        return "@@bazel_tools//tools:internal_platform".to_owned();
    }
    let label = match id.find("#") {
        Some(index) if id[..index].contains(':') => &id[..index],
        _ => id,
    };
    label.to_owned()
}

/// The command ends with `exit_code` (`failure`: the error it reports):
/// stream the execution's events, BuildFinished, the final progress
/// event, Aborted events for what never posted, BuildToolLogs and
/// BuildMetrics; close the files and end the Build Event Service upload.
/// Returns the exit code a failed (or timed out) upload imposes instead of
/// `exit_code` (BlazeRuntime.afterCommand: a module's AbruptExitException
/// replaces a command result that is not an infrastructure failure).
/// Idempotent.
pub fn finish(exit_code: i32, failure: Option<&str>) -> Option<i32> {
    // A command ending before main installed the credential helpers.
    bes::credentials_ready();
    if !active() {
        return None;
    }
    let mut guard = STATE.lock().unwrap_or_else(|e| e.into_inner());
    let state = guard.as_mut()?;
    if state.finished {
        return None;
    }
    if let Some(bes) = &state.bes {
        bes.set_exit_code(exit_code);
    }
    post_execution(state, exit_code);
    post_final(state, exit_code, failure);
    state.streamer.flush();
    state.finished = true;
    ACTIVE.store(false, Ordering::Relaxed);
    let bes = state.bes.clone();
    let files = state.files;
    let error = state.streamer.error.clone();
    drop(guard);
    if let Some(error) = error {
        crate::note!("WARNING: Build Event Protocol upload failed: {error}");
    }
    // closeBepTransports, then the results URL (a BES backend) or the files'
    // message.
    let Some(bes) = bes else {
        if files {
            crate::note!("INFO: Build Event Protocol files produced successfully.");
        }
        return None;
    };
    let imposed = bes.finish();
    if imposed.is_none()
        && let Some(prefix) = bes.options.results_prefix()
    {
        crate::note!(
            "INFO: Streaming build results to: {prefix}{}",
            invocation_id()
        );
    }
    // Infrastructure failures (ExitCode.isInfrastructureFailure) keep
    // their code.
    imposed.filter(|_| !matches!(exit_code, 32..=34 | 36..=39))
}

/// The targets' completion, the failed (or, publishing all actions, every
/// executed) actions, the tests and the convenience symlinks.
fn post_execution(state: &mut State, exit_code: i32) {
    let Some(planned) = state.planned.take() else {
        return;
    };
    let execution_root = state.header.execution_root.clone();
    let workspace = state.header.workspace_directory.clone();
    let by_id: HashMap<&str, usize> = state
        .actions
        .iter()
        .enumerate()
        .map(|(index, action)| (action.id.as_str(), index))
        .collect();
    let mut events: Vec<Event> = Vec::new();
    let configuration_of = |index: usize| -> String {
        planned.plan.configurations.get(index).map_or_else(
            || "none".to_owned(),
            |configuration| configuration.id.clone(),
        )
    };
    // ActionExecuted: failed actions always, executed ones with
    // --build_event_publish_all_actions (cache hits never).
    let mut action_events: Vec<(usize, Event)> = Vec::new();
    // An action's configuration: its owner's when the owner is a requested
    // target, else the (trimmed) top-level one.
    let default_configuration =
        configuration_of(planned.plan.configurations.len().min(2).saturating_sub(1));
    let target_configurations: HashMap<&str, String> = planned
        .plan
        .targets
        .iter()
        .map(|target| {
            (
                target.label.as_str(),
                configuration_of(target.configuration),
            )
        })
        .collect();
    let mut none_posted = false;
    for (index, action) in state.actions.iter().enumerate() {
        if action.success && !(state.publish_all_actions && !action.cache_hit) {
            continue;
        }
        let label = action_label(&action.id);
        let configuration = if action.id.starts_with(crate::request::STATUS_ID_PREFIX) {
            "none".to_owned()
        } else {
            target_configurations
                .get(label.as_str())
                .cloned()
                .unwrap_or_else(|| default_configuration.clone())
        };
        // ActionExecutedEvent's exit code is the build's (BUILD_FAILURE);
        // the spawn's own is in the failure detail.
        let mut payload = Msg::new()
            .bool(1, "success", action.success)
            .i32(2, "exit_code", i32::from(!action.success))
            .str(5, "label", label.clone())
            .msg(7, "configuration", id::configuration_id(&configuration))
            .str(8, "type", action.mnemonic.clone())
            .strs(9, "command_line", action.argv.iter().cloned());
        if action.success {
            payload = payload.msg(
                6,
                "primary_output",
                Msg::new().str(
                    2,
                    "uri",
                    file_uri(&execution_root.join(&action.primary_output)),
                ),
            );
        }
        if let Some(error) = &action.error {
            if let Some((_, stderr)) = error.split_once("\nstderr:\n") {
                let stderr = stderr
                    .strip_suffix(crate::sandbox::SANDBOX_DEBUG_SUGGESTION)
                    .unwrap_or(stderr);
                if let Some(path) = write_stderr(&execution_root, index, stderr) {
                    payload = payload.msg(
                        4,
                        "stderr",
                        Msg::new()
                            .str(1, "name", "stderr")
                            .str(2, "uri", file_uri(&path)),
                    );
                }
            }
            payload = payload.msg(
                11,
                "failure_detail",
                failure_detail(error, action.exit_code),
            );
        }
        // NullConfiguration: the configuration event of an action without
        // one (the workspace status), before the first event naming it.
        if configuration == "none" && !none_posted {
            none_posted = true;
            events.push(Event::new(
                id::configuration("none"),
                (17, "configuration", Msg::new()),
            ));
        }
        let id_configuration = if action.id.starts_with(crate::request::STATUS_ID_PREFIX) {
            "system".to_owned()
        } else {
            configuration
        };
        action_events.push((
            index,
            Event::new(
                id::action_completed(&action.primary_output, &label, &id_configuration),
                (7, "action", payload),
            ),
        ));
    }
    // Successful published actions first (they complete before targets).
    let mut failed_events: HashMap<usize, Event> = HashMap::new();
    for (index, event) in action_events {
        if state.actions[index].success {
            events.push(event);
        } else {
            failed_events.insert(index, event);
        }
    }
    let mut named_set = 0usize;
    let tests_by_label: BTreeMap<&str, Vec<&TestRecord>> =
        state.tests.iter().fold(BTreeMap::new(), |mut map, test| {
            map.entry(test.label.as_str()).or_default().push(test);
            map
        });
    let summaries: HashMap<&str, &TestSummaryRecord> = state
        .summaries
        .iter()
        .map(|summary| (summary.label.as_str(), summary))
        .collect();
    let testing = state.header.command == "test";
    for target in &planned.plan.targets {
        let configuration = configuration_of(target.configuration);
        let completed_id = id::target_completed(&target.label, &configuration);
        match target.compatibility {
            plan::Compatibility::Skipped => {
                events.push(Event::new(
                    completed_id,
                    (
                        4,
                        "aborted",
                        Msg::new().enumeration(1, "reason", (7, "SKIPPED")).str(
                            2,
                            "description",
                            format!("Target {} build was skipped.", target.label),
                        ),
                    ),
                ));
                continue;
            }
            plan::Compatibility::Rejected => {
                events.push(Event::new(
                    id::target_configured(&target.label),
                    (
                        4,
                        "aborted",
                        Msg::new().enumeration(1, "reason", (6, "ANALYSIS_FAILURE")),
                    ),
                ));
                events.push(Event::new(completed_id, (4, "aborted", Msg::new())));
                continue;
            }
            plan::Compatibility::Compatible => {}
        }
        if !planned.execute {
            continue;
        }
        // The producers of the target's reported files: built when each
        // succeeded.
        let mut producers: Vec<&str> = Vec::new();
        for (_, files) in &target.groups {
            for &file in files {
                if let Some(produced) = planned.requested.get(file)
                    && let Some((producer, _)) = &produced.producer
                    && !producers.contains(&producer.as_str())
                {
                    producers.push(producer);
                }
            }
        }
        let failed: Vec<usize> = producers
            .iter()
            .filter_map(|producer| by_id.get(producer).copied())
            .filter(|index| !state.actions[*index].success)
            .collect();
        let missing = producers
            .iter()
            .any(|producer| !by_id.contains_key(producer));
        if !failed.is_empty() {
            let mut children = Vec::new();
            for index in &failed {
                if let Some(event) = failed_events.remove(index) {
                    children.push(event.id.clone());
                    events.push(event);
                }
            }
            let error = state.actions[failed[0]].error.clone().unwrap_or_default();
            let payload = Msg::new()
                .strs(3, "tag", target.completion_tags.iter().cloned())
                .msg(
                    9,
                    "failure_detail",
                    failure_detail(&error, state.actions[failed[0]].exit_code),
                );
            events.push(Event::new(completed_id, (8, "completed", payload)).children(children));
            continue;
        }
        if missing {
            // Never built (a failure elsewhere stopped the build): closed by
            // an Aborted event at the end.
            continue;
        }
        let mut groups = Vec::new();
        for (group, files) in &target.groups {
            let mut entries = Vec::new();
            for &file in files {
                let Some(produced) = planned.requested.get(file) else {
                    continue;
                };
                // A generated file at its place in the output tree below
                // the execution root (crate::output_tree), else in its
                // action's cache entry.
                let location = match &produced.producer {
                    Some((producer, output)) => {
                        let published = execution_root.join(&produced.real);
                        if published.is_file() {
                            published
                        } else {
                            state
                                .outputs
                                .get(producer)
                                .and_then(|outputs| outputs.get(*output))
                                .cloned()
                                .unwrap_or_else(|| produced.real.clone())
                        }
                    }
                    None => produced.real.clone(),
                };
                let exec = if produced.producer.is_some() {
                    produced.real.display().to_string()
                } else {
                    produced
                        .real
                        .strip_prefix(&workspace)
                        .unwrap_or(&produced.real)
                        .display()
                        .to_string()
                };
                entries.push(artifact_file(&exec, &location, &execution_root, &workspace));
            }
            let set = named_set;
            named_set += 1;
            events.push(Event::new(
                id::named_set(set),
                (
                    15,
                    "named_set_of_files",
                    Msg::new().msgs(1, "files", entries),
                ),
            ));
            groups.push(Msg::new().str(1, "name", group.clone()).msgs(
                3,
                "file_sets",
                [Msg::new().str(1, "id", set.to_string())],
            ));
        }
        let mut payload = Msg::new()
            .bool(1, "success", true)
            .msgs(2, "output_group", groups)
            .strs(3, "tag", target.completion_tags.iter().cloned());
        let mut children = Vec::new();
        let tests = tests_by_label.get(target.label.as_str());
        if testing && let Some((_, timeout)) = &target.test {
            payload = payload
                .i64(7, "test_timeout_seconds", *timeout as i64)
                .duration(10, "test_timeout", Duration::from_secs(*timeout));
            for test in tests.into_iter().flatten() {
                children.push(id::test_result(
                    &test.label,
                    &configuration,
                    test.run,
                    test.shard,
                    test.attempt,
                ));
            }
            if summaries.contains_key(target.label.as_str()) {
                children.push(id::test_summary(&target.label, &configuration));
            }
        }
        events.push(Event::new(completed_id, (8, "completed", payload)).children(children));
        if testing {
            for test in tests.into_iter().flatten() {
                events.push(test_result_event(test, &configuration));
            }
            if let Some(summary) = summaries.get(target.label.as_str()) {
                let records: Vec<&TestRecord> = tests.into_iter().flatten().copied().collect();
                events.push(test_summary_event(summary, &records, &configuration));
            }
        }
    }
    // Failed actions no completed target named (a hidden artifact's).
    let mut rest: Vec<(usize, Event)> = failed_events.into_iter().collect();
    rest.sort_by_key(|(index, _)| *index);
    events.extend(rest.into_iter().map(|(_, event)| event));
    if planned.execute {
        events.push(Event::new(
            id::convenience_symlinks_identified(),
            (27, "convenience_symlinks_identified", Msg::new()),
        ));
    }
    let _ = exit_code;
    for event in events {
        state.streamer.post(event);
    }
    state.planned = Some(planned);
}

/// The stderr of a failed action, saved where Bazel keeps it
/// (`bazel-out/_tmp/actions/stderr-N`) for ActionExecuted.stderr.
fn write_stderr(execution_root: &Path, index: usize, stderr: &str) -> Option<PathBuf> {
    let directory = execution_root.join("bazel-out/_tmp/actions");
    // snapshot-unrecorded: a failed action's stderr for the build event file.
    std::fs::create_dir_all(&directory).ok()?;
    let path = directory.join(format!("stderr-{index}"));
    std::fs::write(&path, stderr).ok()?;
    Some(path)
}

/// A FailureDetail: the message, and for a spawn's non-zero exit its
/// Spawn category (failure_details.proto Spawn.Code NON_ZERO_EXIT).
fn failure_detail(error: &str, exit_code: i32) -> Msg {
    let message = error
        .split_once("\nstdout:\n")
        .map_or(error, |(head, _)| head)
        .to_owned();
    let mut detail = Msg::new().str(1, "message", message);
    if error.contains("exit status: ") {
        detail = detail.msg(
            123,
            "spawn",
            Msg::new().enumeration(1, "code", (1, "NON_ZERO_EXIT")).i32(
                3,
                "spawn_exit_code",
                exit_code,
            ),
        );
    }
    detail
}

fn test_status(status: crate::test_command::TestStatus) -> (i32, &'static str) {
    (status as i32, status.name())
}

fn test_result_event(test: &TestRecord, configuration: &str) -> Event {
    let mut outputs = Vec::new();
    for (name, path) in [("test.log", &test.log), ("test.xml", &test.xml)] {
        if let Some(path) = path {
            outputs.push(
                Msg::new()
                    .str(1, "name", name)
                    .str(2, "uri", file_uri(path)),
            );
        }
    }
    let payload = Msg::new()
        .msgs(2, "test_action_output", outputs)
        .i64(
            3,
            "test_attempt_duration_millis",
            test.duration.as_millis() as i64,
        )
        .bool(4, "cached_locally", test.cached)
        .enumeration(5, "status", test_status(test.status))
        .i64(6, "test_attempt_start_millis_epoch", millis(test.start))
        .msg(
            8,
            "execution_info",
            Msg::new().str(2, "strategy", test.strategy.clone()).bool(
                6,
                "cached_remotely",
                test.cached_remotely,
            ),
        )
        .timestamp_millis(10, "test_attempt_start", millis(test.start))
        .duration(11, "test_attempt_duration", test.duration);
    Event::new(
        id::test_result(
            &test.label,
            configuration,
            test.run,
            test.shard,
            test.attempt,
        ),
        (10, "test_result", payload),
    )
}

fn test_summary_event(
    summary: &TestSummaryRecord,
    tests: &[&TestRecord],
    configuration: &str,
) -> Event {
    let first = tests.iter().map(|test| test.start).min();
    let last = tests.iter().map(|test| test.start + test.duration).max();
    let total: Duration = tests.iter().map(|test| test.duration).sum();
    let file = |path: &PathBuf| Msg::new().str(2, "uri", file_uri(path));
    let mut payload = Msg::new()
        .i32(1, "total_run_count", summary.total_run_count)
        .msgs(3, "passed", summary.passed.iter().map(file))
        .msgs(4, "failed", summary.failed.iter().map(file))
        .enumeration(5, "overall_status", test_status(summary.status))
        .i32(6, "total_num_cached", summary.num_cached)
        .i32(10, "run_count", summary.total_run_count)
        .i32(
            11,
            "shard_count",
            if summary.shard_count > 1 {
                summary.shard_count
            } else {
                0
            },
        )
        .i32(
            15,
            "attempt_count",
            tests.iter().map(|test| test.attempt).max().unwrap_or(0),
        );
    if let (Some(first), Some(last)) = (first, last) {
        payload = payload
            .i64(7, "first_start_time_millis", millis(first))
            .i64(8, "last_stop_time_millis", millis(last))
            .i64(9, "total_run_duration_millis", total.as_millis() as i64)
            .duration(12, "total_run_duration", total)
            .timestamp_millis(13, "first_start_time", millis(first))
            .timestamp_millis(14, "last_stop_time", millis(last));
    }
    Event::new(
        id::test_summary(&summary.label, configuration),
        (9, "test_summary", payload),
    )
}

/// BuildFinished, the final progress event, Aborted events for every
/// announced event that never posted, BuildToolLogs, `run`'s exec
/// request and BuildMetrics (the last message).
fn post_final(state: &mut State, exit_code: i32, failure: Option<&str>) {
    let now = SystemTime::now();
    // A command that failed before its workspace status posted: the empty
    // event (no execution phase).
    let status_id = id::workspace_status();
    if !state.streamer.is_posted(&status_id)
        && state
            .planned
            .as_ref()
            .is_some_and(|planned| !planned.execute)
    {
        state.streamer.post(workspace_status_event(&[]));
    }
    let mut children = vec![id::build_tool_logs(), id::build_metrics()];
    if state.exec_request.is_some() {
        children.push(id::exec_request());
    }
    let mut finished = Msg::new()
        .bool(1, "overall_success", exit_code == 0)
        .i64(2, "finish_time_millis", millis(now))
        .msg(
            3,
            "exit_code",
            Msg::new()
                .str(1, "name", exit_code_name(exit_code))
                .i32(2, "code", exit_code),
        )
        .timestamp_millis(5, "finish_time", millis(now));
    if exit_code != 0 {
        let message = state
            .actions
            .iter()
            .find(|action| !action.success)
            .and_then(|action| action.error.clone())
            .or_else(|| failure.map(str::to_owned));
        if let Some(message) = message {
            let code = state
                .actions
                .iter()
                .find(|action| !action.success)
                .map_or(1, |action| action.exit_code);
            finished = finished.msg(6, "failure_detail", failure_detail(&message, code));
        }
    }
    state
        .streamer
        .post(Event::new(id::build_finished(), (14, "finished", finished)).children(children));
    // ProgressEvent.finalProgressUpdate: the chain's last link.
    let final_progress = Event::new(
        id::progress(state.streamer.progress_count),
        (3, "progress", Msg::new()),
    );
    state.streamer.post(final_progress);
    // clearAnnouncedEvents: every announced, unposted event is aborted.
    let reason = match &state.planned {
        Some(planned) if !planned.execute => (9, "NO_BUILD"),
        Some(_) => (10, "INCOMPLETE"),
        None if exit_code == 0 => (10, "INCOMPLETE"),
        None => (5, "LOADING_FAILURE"),
    };
    let final_ids = [
        id::build_tool_logs().encode(),
        id::build_metrics().encode(),
        id::exec_request().encode(),
    ];
    let mut pending: Vec<Vec<u8>> = state
        .streamer
        .announced
        .difference(&state.streamer.posted)
        .filter(|key| !final_ids.contains(key))
        .cloned()
        .collect();
    pending.sort();
    let pending_ids = aborted_ids(state, &pending);
    for aborted in pending_ids {
        state.streamer.post(Event::new(
            aborted,
            (4, "aborted", Msg::new().enumeration(1, "reason", reason)),
        ));
    }
    let elapsed = state.clock.elapsed();
    let mut logs = vec![
        Msg::new().str(1, "name", "elapsed time").bytes(
            3,
            "contents",
            format!("{:.6}", elapsed.as_secs_f64()).into_bytes(),
        ),
        Msg::new().str(1, "name", "critical path").bytes(
            3,
            "contents",
            critical_path_text(&state.actions).into_bytes(),
        ),
        Msg::new().str(1, "name", "process stats").bytes(
            3,
            "contents",
            process_stats(&state.actions).into_bytes(),
        ),
    ];
    if let Some(profile) = &state.header.profile {
        logs.push(
            Msg::new()
                .str(1, "name", "command.profile.gz")
                .str(2, "uri", file_uri(profile)),
        );
    }
    state.streamer.post(Event::new(
        id::build_tool_logs(),
        (23, "build_tool_logs", Msg::new().msgs(1, "log", logs)),
    ));
    if let Some(request) = state.exec_request.take() {
        use std::os::unix::ffi::OsStrExt as _;
        let payload = Msg::new()
            .bytes(
                1,
                "working_directory",
                request.working_directory.as_os_str().as_bytes().to_vec(),
            )
            .bytes_list(
                2,
                "argv",
                request.argv.iter().map(|arg| arg.as_bytes().to_vec()),
            )
            .msgs(
                3,
                "environment_variable",
                request.environment.iter().map(|(name, value)| {
                    Msg::new().bytes(1, "name", name.as_bytes().to_vec()).bytes(
                        2,
                        "value",
                        value.as_bytes().to_vec(),
                    )
                }),
            )
            .bool(5, "should_exec", true);
        state.streamer.post(Event::new(
            id::exec_request(),
            (29, "exec_request", payload),
        ));
    }
    let metrics = build_metrics(state, elapsed);
    state.streamer.post(Event::new(
        id::build_metrics(),
        (24, "build_metrics", metrics),
    ));
}

/// The announced, unposted ids among the encodings `pending`, as messages.
/// Every announcement this stream makes is one of a known set, so the
/// encodings are mapped back through the ids the stream built.
fn aborted_ids(state: &State, pending: &[Vec<u8>]) -> Vec<Msg> {
    let mut known: Vec<Msg> = vec![
        id::unstructured_command_line(),
        id::structured_command_line("original"),
        id::structured_command_line("canonical"),
        id::structured_command_line("tool"),
        id::build_metadata(),
        id::options_parsed(),
        id::workspace_status(),
        id::pattern(&state.header.patterns),
        id::workspace(),
    ];
    if let Some(planned) = &state.planned {
        for target in &planned.plan.targets {
            known.push(id::target_configured(&target.label));
            let configuration = planned
                .plan
                .configurations
                .get(target.configuration)
                .map_or("none", |configuration| configuration.id.as_str());
            known.push(id::target_completed(&target.label, configuration));
        }
        for label in &planned.analysis_failed {
            known.push(id::target_configured(label));
        }
    }
    for count in 0..=state.streamer.progress_count + 1 {
        known.push(id::progress(count));
    }
    let by_key: HashMap<Vec<u8>, Msg> = known
        .into_iter()
        .map(|message| (message.encode(), message))
        .collect();
    pending
        .iter()
        .filter_map(|key| by_key.get(key).cloned())
        .collect()
}

/// The critical path log (CriticalPathSummary-like): the longest chain of
/// finished actions by wall time, approximated by the actions' own spans.
fn critical_path_text(actions: &[ActionRecord]) -> String {
    let path = crate::profile::critical_path();
    let total: Duration = path.iter().map(|(_, duration)| *duration).sum();
    let mut text = format!("Critical Path: {:.2}s", total.as_secs_f64());
    for (description, duration) in &path {
        text.push_str(&format!(
            "\n  {:.2}s action '{description}'",
            duration.as_secs_f64()
        ));
    }
    let _ = actions;
    text
}

/// `N processes: X action cache hit, R remote cache hit, Y internal,
/// E remote, Z <strategy>.`
fn process_stats(actions: &[ActionRecord]) -> String {
    let hits = actions.iter().filter(|action| action.cache_hit).count();
    let executed: Vec<&ActionRecord> = actions
        .iter()
        .filter(|action| !action.cache_hit && action.success)
        .collect();
    let remote_hits = executed.iter().filter(|action| action.remote_hit).count();
    let remote = executed
        .iter()
        .filter(|action| action.remote_executed)
        .count();
    let spawns = executed
        .iter()
        .filter(|action| !action.remote_hit && !action.remote_executed)
        .filter(|action| !action.argv.is_empty() || action.mnemonic == "Genrule")
        .count();
    let internal = executed.len() - spawns - remote_hits - remote;
    let total = executed.len();
    let mut parts = Vec::new();
    if hits > 0 {
        parts.push(format!("{hits} action cache hit"));
    }
    if remote_hits > 0 {
        parts.push(format!("{remote_hits} remote cache hit"));
    }
    if internal > 0 {
        parts.push(format!("{internal} internal"));
    }
    if remote > 0 {
        parts.push(format!("{remote} remote"));
    }
    if spawns > 0 {
        let strategy = if crate::sandbox::all_sandboxed() {
            "linux-sandbox"
        } else {
            "local"
        };
        parts.push(format!("{spawns} {strategy}"));
    }
    let noun = if total == 1 { "process" } else { "processes" };
    if parts.is_empty() {
        format!("{total} {noun}.")
    } else {
        format!("{total} {noun}: {}.", parts.join(", "))
    }
}

/// BuildMetrics: the action summary (per mnemonic), target and timing
/// metrics; the JVM- and Skyframe-specific ones are left out.
fn build_metrics(state: &State, elapsed: Duration) -> Msg {
    let (created, created_by_mnemonic) = state
        .planned
        .as_ref()
        .map(|planned| (planned.actions_created, planned.created_by_mnemonic.clone()))
        .unwrap_or_default();
    let mut by_mnemonic: BTreeMap<String, (i64, i64, i64)> = BTreeMap::new();
    let mut executed = 0i64;
    for action in &state.actions {
        if action.cache_hit || !action.success {
            continue;
        }
        executed += 1;
        let entry = by_mnemonic
            .entry(action.mnemonic.clone())
            .or_insert((0, i64::MAX, 0));
        entry.0 += 1;
        entry.1 = entry.1.min(millis(action.start));
        entry.2 = entry.2.max(millis(action.end));
    }
    let mut mnemonics: Vec<&String> = created_by_mnemonic.keys().collect();
    for mnemonic in by_mnemonic.keys() {
        if !created_by_mnemonic.contains_key(mnemonic) {
            mnemonics.push(mnemonic);
        }
    }
    let action_data = mnemonics.into_iter().map(|mnemonic| {
        let mut data = Msg::new().str(1, "mnemonic", mnemonic.clone());
        if let Some((count, first, last)) = by_mnemonic.get(mnemonic) {
            data = data
                .i64(2, "actions_executed", *count)
                .i64(3, "first_started_ms", *first)
                .i64(4, "last_ended_ms", *last);
        }
        data.i64(
            7,
            "actions_created",
            created_by_mnemonic.get(mnemonic).copied().unwrap_or(0) as i64,
        )
    });
    let action_summary = Msg::new()
        .i64(1, "actions_created", created as i64)
        .i64(2, "actions_executed", executed)
        .i64(3, "actions_created_not_including_aspects", created as i64)
        .msgs(4, "action_data", action_data)
        .msgs(
            6,
            "runner_count",
            [Msg::new()
                .str(1, "name", "total")
                .i32(2, "count", executed as i32)],
        );
    let targets = state
        .planned
        .as_ref()
        .map_or(0, |planned| planned.plan.targets.len() as i64);
    let (user, system) = crate::profile::cpu_times();
    let timing = Msg::new()
        .i64(1, "cpu_time_in_ms", (user + system).as_millis() as i64)
        .i64(2, "wall_time_in_ms", elapsed.as_millis() as i64)
        .i64(
            3,
            "analysis_phase_time_in_ms",
            crate::profile::phase_millis("plan.start", "plan.end"),
        )
        .i64(
            4,
            "execution_phase_time_in_ms",
            crate::profile::phase_millis("plan.end", "execute.end"),
        )
        .duration(
            6,
            "critical_path_time",
            crate::profile::critical_path()
                .iter()
                .map(|(_, duration)| *duration)
                .sum(),
        );
    Msg::new()
        .msg(1, "action_summary", action_summary)
        .msg(
            3,
            "target_metrics",
            Msg::new().i64(2, "targets_configured", targets).i64(
                3,
                "targets_configured_not_including_aspects",
                targets,
            ),
        )
        .msg(5, "timing_metrics", timing)
        .msg(
            6,
            "cumulative_metrics",
            Msg::new()
                .i32(11, "num_analyses", 1)
                .i32(12, "num_builds", 1),
        )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn streamer() -> Streamer {
        Streamer {
            transports: Vec::new(),
            announced: HashSet::new(),
            posted: HashSet::new(),
            progress_count: 0,
            started: false,
            error: None,
        }
    }

    #[test]
    fn unannounced_events_link_through_the_progress_chain() {
        let mut streamer = streamer();
        streamer.post(
            Event::new(id::started(), (5, "started", Msg::new()))
                .children(vec![id::progress(0), id::build_finished()]),
        );
        streamer.post(Event::new(
            id::workspace(),
            (25, "workspace_info", Msg::new()),
        ));
        assert_eq!(streamer.progress_count, 1);
        assert!(streamer.is_posted(&id::progress(0)));
        streamer.post(Event::new(
            id::build_finished(),
            (14, "finished", Msg::new()),
        ));
        // progress 1 is still announced.
        assert_ne!(streamer.announced.len(), streamer.posted.len());
        streamer.post(Event::new(id::progress(1), (3, "progress", Msg::new())));
        assert_eq!(streamer.announced.len(), streamer.posted.len());
    }

    #[test]
    fn options_parse_files_and_reject_the_backend() {
        let mut options = Options::default();
        assert_eq!(
            options.parse("--build_event_json_file=/x.json", None),
            Some(Ok(false))
        );
        assert_eq!(
            options.parse("--build_event_text_file", Some("t")),
            Some(Ok(true))
        );
        assert_eq!(
            options.parse("--nobuild_event_json_file_path_conversion", None),
            Some(Ok(false))
        );
        assert_eq!(
            options.parse("--build_event_publish_all_actions", None),
            Some(Ok(false))
        );
        assert_eq!(options.parse("--build_metadata=A=b", None), Some(Ok(false)));
        assert_eq!(options.parse("--jobs=2", None), None);
        assert!(options.enabled() && options.publish_all_actions);
        assert_eq!(
            options.build_metadata,
            vec![("A".to_owned(), "b".to_owned())]
        );
        assert!(options.validate().is_ok());
        options.parse("--bes_backend=grpc://x", None);
        assert!(options.validate().is_ok() && options.bes.enabled());
        options.parse("--bes_proxy=unix:/s", None);
        assert!(options.validate().unwrap_err().contains("not implemented"));
        assert!(
            options
                .parse("--build_event_json_file_upload_mode=bogus", None)
                .unwrap()
                .is_err()
        );
    }

    #[test]
    fn helpers() {
        assert_eq!(
            exit_code_of("action 'x' failed (exit status: 3):\nstdout:"),
            3
        );
        assert_eq!(action_label("//pkg:a#0"), "//pkg:a");
        assert_eq!(action_label("@@r+//p:t#runfiles_tree"), "@@r+//p:t");
        assert_eq!(file_uri(Path::new("/a b/c")), "file:///a%20b/c");
        assert_eq!(
            split_option("--noshow_progress"),
            ("show_progress".to_owned(), "0".to_owned())
        );
        assert_eq!(
            split_option("--color=no"),
            ("color".to_owned(), "no".to_owned())
        );
        let id = invocation_id();
        assert_eq!(id.len(), 36);
        assert_eq!(&id[14..15], "4");
    }
}
