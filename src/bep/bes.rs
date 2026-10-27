//! The Build Event Service upload (`--bes_backend`, docs/BEP.md "The
//! Build Event Service"): the build event stream of crate::bep sent over
//! `google.devtools.build.v1.PublishBuildEvent` — the lifecycle events
//! (PublishLifecycleEvent) around one PublishBuildToolEventStream call that
//! carries every BuildEvent as a `bazel_event` Any, acknowledged by
//! sequence number, retried with the unacknowledged events.
//!
//! Pinned Bazel source (`src/main/java/com/google/devtools/build/lib/`):
//! `buildeventservice/BuildEventServiceOptions.java` (options),
//! `BazelBuildEventServiceModule.java` (keywords, headers, the results
//! URL), `BuildEventServiceModule.java` (transport creation, the end of the
//! command per upload mode, timeouts and messages),
//! `BuildEventServiceUploader.java` (the event loop: lifecycle events, the
//! stream, ACK checking, retries and their failure details),
//! `client/BuildEventServiceProtoUtil.java` (the request messages) and
//! `client/BuildEventServiceGrpcClient.java` (the 15 s lifecycle deadline,
//! RequestMetadata). Protos: googleapis `google/devtools/build/v1/`
//! `publish_build_event.proto`, `build_events.proto`, `build_status.proto`.

use crate::grpc_channel::{BidiPeer, Call, Channel, Code, Limits, Status, Target, TlsOptions};
use crate::reapi;
use std::collections::VecDeque;
use std::sync::{Arc, Condvar, Mutex};
use std::task::Poll;
use std::time::{Duration, Instant, SystemTime};

const SERVICE: &str = "google.devtools.build.v1.PublishBuildEvent";
/// BuildEventServiceGrpcClient.RPC_TIMEOUT.
const LIFECYCLE_DEADLINE: Duration = Duration::from_secs(15);
/// BuildEventServiceModule.getMaxWaitForPreviousInvocation: what a batch
/// command waits at shutdown for an upload it did not wait for.
const SHUTDOWN_WAIT: Duration = Duration::from_secs(5);
const TYPE_URL: &str = "type.googleapis.com/build_event_stream.BuildEvent";

/// BuildEventServiceOptions.BesUploadMode.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum UploadMode {
    #[default]
    WaitForUploadComplete,
    NowaitForUploadComplete,
    FullyAsync,
}

/// BuildEventServiceOptions (and BuildEventProtocolOptions' upload
/// retries).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BesOptions {
    pub backend: String,
    /// `--bes_timeout` (0: none).
    pub timeout: Duration,
    /// `--bes_header` in order (the last value of a name wins).
    pub headers: Vec<(String, String)>,
    pub lifecycle_events: bool,
    /// `--bes_instance_name` (old name `--project_id`).
    pub instance_name: Option<String>,
    pub keywords: Vec<String>,
    pub system_keywords: Vec<String>,
    pub results_url: String,
    pub upload_mode: UploadMode,
    pub check_preceding_lifecycle_events: bool,
    /// `--build_event_upload_max_retries` (default 4).
    pub max_retries: u32,
    /// `--experimental_build_event_upload_retry_minimum_delay` (default 1s).
    pub retry_delay: Duration,
    pub proxy: Option<String>,
}

impl Default for BesOptions {
    fn default() -> Self {
        Self {
            backend: String::new(),
            timeout: Duration::ZERO,
            headers: Vec::new(),
            lifecycle_events: true,
            instance_name: None,
            keywords: Vec::new(),
            system_keywords: Vec::new(),
            results_url: String::new(),
            upload_mode: UploadMode::WaitForUploadComplete,
            check_preceding_lifecycle_events: false,
            max_retries: 4,
            retry_delay: Duration::from_secs(1),
            proxy: None,
        }
    }
}

/// The value options this module parses (name, old name).
const VALUE_OPTIONS: &[(&str, &str)] = &[
    ("bes_backend", ""),
    ("bes_timeout", ""),
    ("bes_header", ""),
    ("bes_instance_name", "project_id"),
    ("bes_keywords", ""),
    ("bes_system_keywords", ""),
    ("bes_results_url", ""),
    ("bes_upload_mode", ""),
    ("bes_proxy", ""),
    ("bes_outerr_buffer_size", ""),
    ("bes_outerr_chunk_size", ""),
    ("bes_oom_finish_upload_timeout", ""),
    (
        "build_event_upload_max_retries",
        "experimental_build_event_upload_max_retries",
    ),
    ("experimental_build_event_upload_retry_minimum_delay", ""),
];

impl BesOptions {
    /// One option of build/test/run: `Some(Ok(used_next))` when it is one
    /// of the BES options.
    pub fn parse(&mut self, arg: &str, next: Option<&str>) -> Option<Result<bool, String>> {
        for (name, target) in [
            ("bes_lifecycle_events", &mut self.lifecycle_events),
            (
                "bes_check_preceding_lifecycle_events",
                &mut self.check_preceding_lifecycle_events,
            ),
        ] {
            if let Some(value) = crate::configuration::boolean_option(arg, name) {
                return Some(value.map(|value| {
                    *target = value;
                    false
                }));
            }
        }
        let body = arg.strip_prefix("--")?;
        for (name, old) in VALUE_OPTIONS {
            for given in [*name, *old] {
                if given.is_empty() {
                    continue;
                }
                let (value, used) = if body == given {
                    match next {
                        Some(value) => (value, true),
                        None => return Some(Err(format!("Expected value after --{given}"))),
                    }
                } else if let Some(value) = body
                    .strip_prefix(given)
                    .and_then(|rest| rest.strip_prefix('='))
                {
                    (value, false)
                } else {
                    continue;
                };
                return Some(self.set(name, given, value).map(|()| used));
            }
        }
        None
    }

    fn set(&mut self, name: &str, given: &str, value: &str) -> Result<(), String> {
        let error = |detail: String| format!("While parsing option --{given}={value}: {detail}");
        let duration =
            |value: &str| crate::credentials::parse_duration(value).map_err(|e| error(e.clone()));
        let integer = |value: &str| {
            value
                .parse::<i32>()
                .map_err(|_| error(format!("'{value}' is not an int")))
        };
        // CommaSeparatedOptionListConverter.
        let list = |value: &str| -> Vec<String> {
            value
                .split(',')
                .filter(|item| !item.is_empty())
                .map(str::to_owned)
                .collect()
        };
        match name {
            "bes_backend" => self.backend = value.to_owned(),
            "bes_timeout" => self.timeout = duration(value)?,
            "bes_header" => match value.find('=') {
                Some(position) if position > 0 => self.headers.push((
                    value[..position].to_owned(),
                    value[position + 1..].to_owned(),
                )),
                _ => {
                    return Err(error(
                        "Variable definitions must be in the form of a 'name=value' assignment"
                            .into(),
                    ));
                }
            },
            "bes_instance_name" => self.instance_name = Some(value.to_owned()),
            "bes_keywords" => self.keywords.extend(list(value)),
            "bes_system_keywords" => self.system_keywords.extend(list(value)),
            "bes_results_url" => self.results_url = value.to_owned(),
            "bes_upload_mode" => {
                self.upload_mode = match value.to_ascii_lowercase().as_str() {
                    "wait_for_upload_complete" => UploadMode::WaitForUploadComplete,
                    "nowait_for_upload_complete" => UploadMode::NowaitForUploadComplete,
                    "fully_async" => UploadMode::FullyAsync,
                    _ => {
                        return Err(error(format!(
                            "Not a valid Mode for uploading to the Build Event Service: '{value}' (should be wait_for_upload_complete, nowait_for_upload_complete or fully_async)"
                        )));
                    }
                }
            }
            "bes_proxy" => self.proxy = (!value.is_empty()).then(|| value.to_owned()),
            "bes_outerr_buffer_size" | "bes_outerr_chunk_size" => {
                integer(value)?;
            }
            "bes_oom_finish_upload_timeout" => {
                duration(value)?;
            }
            "build_event_upload_max_retries" => self.max_retries = integer(value)?.max(0) as u32,
            "experimental_build_event_upload_retry_minimum_delay" => {
                self.retry_delay = duration(value)?
            }
            _ => {}
        }
        Ok(())
    }

    pub fn enabled(&self) -> bool {
        !self.backend.is_empty()
    }

    /// The explicit errors of what this profile does not model.
    pub fn validate(&self) -> Result<(), String> {
        if let Some(proxy) = &self.proxy
            && self.enabled()
        {
            return Err(format!(
                "--bes_proxy={proxy}: connecting to the Build Event Service through a proxy is not implemented in the native profile"
            ));
        }
        Ok(())
    }

    /// BazelBuildEventServiceModule.getBesKeywords: `protocol_name=BEP`,
    /// `command_name=…`, the system keywords, then `user_keyword=…` per
    /// `--bes_keywords` (an ImmutableSet: first occurrences in order).
    fn keywords(&self, command: &str) -> Vec<String> {
        let mut out: Vec<String> = Vec::new();
        let candidates = [
            "protocol_name=BEP".to_owned(),
            format!("command_name={command}"),
        ]
        .into_iter()
        .chain(self.system_keywords.iter().cloned())
        .chain(self.keywords.iter().map(|k| format!("user_keyword={k}")));
        for keyword in candidates {
            if !out.contains(&keyword) {
                out.push(keyword);
            }
        }
        out
    }

    /// The invocation URL prefix (getInvocationIdPrefix): the results URL
    /// with a trailing slash, or nothing.
    pub fn results_prefix(&self) -> Option<String> {
        if self.results_url.is_empty() {
            return None;
        }
        Some(if self.results_url.ends_with('/') {
            self.results_url.clone()
        } else {
            format!("{}/", self.results_url)
        })
    }
}

/// BuildEventServiceClient.InvocationStatus.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum InvocationStatus {
    Unknown,
    Succeeded,
    Failed,
}

impl InvocationStatus {
    /// BuildStatus.Result.
    fn result(self) -> u64 {
        match self {
            InvocationStatus::Unknown => 0,
            InvocationStatus::Succeeded => 1,
            InvocationStatus::Failed => 2,
        }
    }
}

/// A failed upload: the failure detail's message and its exit code
/// (BuildProgress codes: 38 transient, 45 persistent).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UploadError {
    pub message: String,
    pub exit_code: i32,
}

enum Command {
    Event {
        sequence: i64,
        time: SystemTime,
        payload: Vec<u8>,
    },
    Last {
        sequence: i64,
        time: SystemTime,
    },
}

struct Shared {
    sender: tokio::sync::mpsc::UnboundedSender<Command>,
    next_sequence: i64,
    closed: bool,
    status: InvocationStatus,
}

/// One command's upload (BuildEventServiceTransport with its uploader
/// thread).
pub struct Uploader {
    shared: Mutex<Shared>,
    done: Mutex<Option<Result<(), UploadError>>>,
    finished: Condvar,
    pub options: BesOptions,
}

/// When the command may start talking to the backend: credential helpers
/// are installed after the build event transports (main sets this once
/// they are, or when the command ends).
static READY: (Mutex<bool>, Condvar) = (Mutex::new(false), Condvar::new());

/// The credential helpers are installed (or never will be).
pub fn credentials_ready() {
    *READY.0.lock().unwrap_or_else(|e| e.into_inner()) = true;
    READY.1.notify_all();
}

fn wait_ready() {
    let mut ready = READY.0.lock().unwrap_or_else(|e| e.into_inner());
    while !*ready {
        ready = READY.1.wait(ready).unwrap_or_else(|e| e.into_inner());
    }
}

/// The ids and keywords every request carries (CommandContext).
struct Context {
    build_id: String,
    invocation_id: String,
    keywords: Vec<String>,
    project_id: Option<String>,
    check_preceding: bool,
}

fn timestamp(time: SystemTime) -> Vec<u8> {
    let since = time
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_default();
    let mut out = Vec::new();
    reapi::varint_field(&mut out, 1, since.as_secs());
    reapi::varint_field(&mut out, 2, u64::from(since.subsec_nanos()));
    out
}

/// BuildEventServiceProtoUtil.streamId: CONTROLLER (1) for lifecycle
/// events (the invocation id for the attempt events only), TOOL (3) for
/// the stream.
fn stream_id(context: &Context, invocation: bool, component: u64) -> Vec<u8> {
    let mut out = Vec::new();
    reapi::string(&mut out, 1, &context.build_id);
    reapi::varint_field(&mut out, 3, component);
    if invocation {
        reapi::string(&mut out, 6, &context.invocation_id);
    }
    out
}

fn build_status(status: InvocationStatus) -> Vec<u8> {
    let mut out = Vec::new();
    reapi::varint_field(&mut out, 1, status.result());
    out
}

/// The lifecycle events (LifecycleEvent).
#[derive(Clone, Copy)]
enum Lifecycle {
    BuildEnqueued(SystemTime),
    InvocationStarted(SystemTime),
    InvocationFinished(SystemTime, InvocationStatus),
    BuildFinished(SystemTime, InvocationStatus),
}

/// BuildEventServiceProtoUtil.publishLifecycleEventRequest.
fn lifecycle_request(context: &Context, event: Lifecycle) -> Vec<u8> {
    let (sequence, time, field, payload, invocation, keywords) = match event {
        Lifecycle::BuildEnqueued(time) => (1, time, 53, Vec::new(), false, true),
        Lifecycle::InvocationStarted(time) => {
            let mut started = Vec::new();
            // CommandEnvironment.getAttemptNumber: always the first.
            reapi::varint_field(&mut started, 1, 1);
            (1, time, 51, started, true, true)
        }
        Lifecycle::InvocationFinished(time, status) => {
            let mut finished = Vec::new();
            reapi::message(&mut finished, 3, &build_status(status));
            (2, time, 52, finished, true, false)
        }
        Lifecycle::BuildFinished(time, status) => {
            let mut finished = Vec::new();
            reapi::message(&mut finished, 1, &build_status(status));
            (2, time, 55, finished, false, true)
        }
    };
    let mut event = Vec::new();
    reapi::message(&mut event, 1, &timestamp(time));
    reapi::message(&mut event, field, &payload);
    let mut ordered = Vec::new();
    reapi::message(&mut ordered, 1, &stream_id(context, invocation, 1));
    reapi::varint_field(&mut ordered, 2, sequence);
    reapi::message(&mut ordered, 3, &event);
    let mut out = Vec::new();
    // ServiceLevel.INTERACTIVE.
    reapi::varint_field(&mut out, 1, 1);
    reapi::message(&mut out, 2, &ordered);
    if keywords {
        for keyword in &context.keywords {
            reapi::string(&mut out, 4, keyword);
        }
    }
    if let Some(project) = &context.project_id {
        reapi::string(&mut out, 6, project);
    }
    out
}

/// BuildEventServiceProtoUtil.streamRequest: a `bazel_event` (or, for
/// `payload` None, the `component_stream_finished` FINISHED event); the
/// first request carries the keywords.
fn stream_request(
    context: &Context,
    sequence: i64,
    time: SystemTime,
    payload: Option<&[u8]>,
) -> Vec<u8> {
    let mut event = Vec::new();
    reapi::message(&mut event, 1, &timestamp(time));
    match payload {
        Some(payload) => {
            let mut any = Vec::new();
            reapi::string(&mut any, 1, TYPE_URL);
            reapi::bytes_field(&mut any, 2, payload);
            reapi::message(&mut event, 60, &any);
        }
        None => {
            let mut finished = Vec::new();
            reapi::varint_field(&mut finished, 1, 1);
            reapi::message(&mut event, 59, &finished);
        }
    }
    let mut ordered = Vec::new();
    reapi::message(&mut ordered, 1, &stream_id(context, true, 3));
    reapi::varint_field(&mut ordered, 2, sequence as u64);
    reapi::message(&mut ordered, 3, &event);
    let mut out = Vec::new();
    reapi::message(&mut out, 4, &ordered);
    if sequence == 1 {
        for keyword in &context.keywords {
            reapi::string(&mut out, 5, keyword);
        }
    }
    if let Some(project) = &context.project_id {
        reapi::string(&mut out, 6, project);
    }
    if sequence == 1 && context.check_preceding {
        reapi::varint_field(&mut out, 7, 1);
    }
    out
}

/// BuildEventServiceUploader.RETRYABLE_STATUS_CODES: all but
/// INVALID_ARGUMENT and PERMISSION_DENIED.
fn retryable(status: &Status) -> bool {
    !matches!(status.code, Code::InvalidArgument | Code::PermissionDenied)
}

/// shouldStartNewInvocation.
fn start_new_invocation(status: &Status) -> bool {
    status.code == Code::FailedPrecondition
}

/// grpc-java's `Status.toString()` (the cause as its message).
fn status_text(status: &Status) -> String {
    format!(
        "Status{{code={}, description={}, cause={}}}",
        status.code.name(),
        if status.message.is_empty() {
            "null"
        } else {
            &status.message
        },
        status.cause.as_deref().unwrap_or("null")
    )
}

/// BuildEventServiceGrpcClient.userReadableError: the code and the root
/// cause's message (the status exception itself for a server status).
fn user_readable(status: &Status) -> String {
    let root = status
        .cause
        .clone()
        .unwrap_or_else(|| format!("{}: {}", status.code.name(), status.message));
    format!("{}: {root}", status.code.name())
}

/// DetailedStatusException (`The Build Event Protocol upload failed:
/// MESSAGE USER_READABLE`) as logAndSetException's failure detail, which
/// appends the user-readable error once more; with the BuildProgress
/// code's exit code.
fn failure(status: &Status, message: String, exit_code: i32) -> UploadError {
    let readable = user_readable(status);
    UploadError {
        message: format!("The Build Event Protocol upload failed: {message} {readable} {readable}"),
        exit_code,
    }
}

impl Uploader {
    /// The transport of `options` for `command` (createBesTransport):
    /// starts the upload thread, which waits for [`credentials_ready`].
    pub fn start(options: &BesOptions, command: &str) -> Result<Arc<Self>, String> {
        let target = Target::parse(&options.backend)
            .map_err(|error| format!("--bes_backend={}: {error}", options.backend))?;
        let channel = Channel::new(
            target,
            TlsOptions::default(),
            Limits {
                max_connections: 0,
                max_concurrency_per_connection: 0,
                connect_timeout: Duration::from_secs(20),
            },
            concat!("bazel-rs/", env!("CARGO_PKG_VERSION"), " grpc-h2").to_owned(),
        )?;
        let (sender, receiver) = tokio::sync::mpsc::unbounded_channel();
        let uploader = Arc::new(Self {
            shared: Mutex::new(Shared {
                sender,
                next_sequence: 1,
                closed: false,
                status: InvocationStatus::Unknown,
            }),
            done: Mutex::new(None),
            finished: Condvar::new(),
            options: options.clone(),
        });
        let context = Context {
            build_id: super::build_request_id().to_owned(),
            invocation_id: super::invocation_id().to_owned(),
            keywords: options.keywords(command),
            project_id: options.instance_name.clone(),
            check_preceding: options.check_preceding_lifecycle_events,
        };
        let (_, command_start) = crate::process_clock();
        let stream_start = SystemTime::now();
        let thread = Arc::clone(&uploader);
        std::thread::Builder::new()
            .name("bes-uploader".into())
            .spawn(move || {
                wait_ready();
                let mut run = Run {
                    channel,
                    context,
                    options: thread.options.clone(),
                    receiver,
                };
                let result = run.run(&thread, command_start, stream_start);
                *thread.done.lock().unwrap_or_else(|e| e.into_inner()) = Some(result);
                thread.finished.notify_all();
            })
            .map_err(|error| format!("cannot start the BES upload: {error}"))?;
        Ok(uploader)
    }

    /// enqueueEvent: one serialized BuildEvent with the next sequence
    /// number. A BuildFinished event decides the invocation status.
    pub fn enqueue(&self, payload: Vec<u8>) {
        let mut shared = self.shared.lock().unwrap_or_else(|e| e.into_inner());
        if shared.closed {
            return;
        }
        let sequence = shared.next_sequence;
        shared.next_sequence += 1;
        let _ = shared.sender.send(Command::Event {
            sequence,
            time: SystemTime::now(),
            payload,
        });
    }

    /// The command's exit code, from its BuildFinished event
    /// (BuildCompletingEvent).
    pub fn set_exit_code(&self, code: i32) {
        self.shared.lock().unwrap_or_else(|e| e.into_inner()).status = if code == 0 {
            InvocationStatus::Succeeded
        } else {
            InvocationStatus::Failed
        };
    }

    /// close: the last event (component_stream_finished) ends the stream.
    pub fn close(&self) {
        let mut shared = self.shared.lock().unwrap_or_else(|e| e.into_inner());
        if shared.closed {
            return;
        }
        shared.closed = true;
        let sequence = shared.next_sequence;
        shared.next_sequence += 1;
        let _ = shared.sender.send(Command::Last {
            sequence,
            time: SystemTime::now(),
        });
    }

    fn status(&self) -> InvocationStatus {
        self.shared.lock().unwrap_or_else(|e| e.into_inner()).status
    }

    /// The upload's result, waiting at most `limit` (`None`: unbounded);
    /// `None` when it did not finish in time.
    pub fn wait(&self, limit: Option<Duration>) -> Option<Result<(), UploadError>> {
        let deadline = limit.map(|limit| Instant::now() + limit);
        let mut done = self.done.lock().unwrap_or_else(|e| e.into_inner());
        loop {
            if let Some(result) = done.as_ref() {
                return Some(result.clone());
            }
            match deadline {
                None => done = self.finished.wait(done).unwrap_or_else(|e| e.into_inner()),
                Some(deadline) => {
                    let now = Instant::now();
                    if now >= deadline {
                        return None;
                    }
                    done = self
                        .finished
                        .wait_timeout(done, deadline - now)
                        .unwrap_or_else(|e| e.into_inner())
                        .0;
                }
            }
        }
    }

    /// The end of the command (BuildEventServiceModule.closeBepTransports
    /// and blazeShutdown for a batch command): the exit code the upload
    /// imposes, after printing its error. `wait_for_upload_complete` waits
    /// for the upload (at most `--bes_timeout`); the other modes leave it to
    /// the shutdown, which waits at most 5 s and ignores failures.
    pub fn finish(&self) -> Option<i32> {
        self.close();
        let timeout = (!self.options.timeout.is_zero()).then_some(self.options.timeout);
        match self.options.upload_mode {
            UploadMode::WaitForUploadComplete => match self.wait(timeout) {
                Some(Ok(())) => None,
                Some(Err(error)) => {
                    crate::note!("ERROR: {}", error.message);
                    Some(error.exit_code)
                }
                None => {
                    // BES_UPLOAD_TIMEOUT_ERROR (Futures.withTimeout's
                    // TimeoutException, wrapped).
                    crate::note!(
                        "ERROR: The Build Event Protocol upload timed out. java.util.concurrent.TimeoutException: Timed out after {}",
                        humanize(self.options.timeout)
                    );
                    Some(38)
                }
            },
            UploadMode::NowaitForUploadComplete | UploadMode::FullyAsync => {
                let limit = timeout.map_or(SHUTDOWN_WAIT, |timeout| timeout.min(SHUTDOWN_WAIT));
                let _ = self.wait(Some(limit));
                None
            }
        }
    }
}

/// A duration as Futures.withTimeout reports it (`N ms` style; the native
/// text names milliseconds).
fn humanize(duration: Duration) -> String {
    format!("{}ms", duration.as_millis())
}

/// The upload thread's state.
struct Run {
    channel: Channel,
    context: Context,
    options: BesOptions,
    receiver: tokio::sync::mpsc::UnboundedReceiver<Command>,
}

/// The stream attempt's side of PublishBuildToolEventStream.
struct Peer<'a> {
    receiver: &'a mut tokio::sync::mpsc::UnboundedReceiver<Command>,
    context: &'a Context,
    /// Unacknowledged requests of an earlier attempt, sent first.
    resend: &'a mut VecDeque<(i64, Vec<u8>)>,
    /// Sent and not yet acknowledged, in order.
    ack_queue: VecDeque<(i64, Vec<u8>)>,
    last_sequence: &'a mut Option<i64>,
    /// The last event was sent in this attempt: half-close.
    closing: bool,
    acks: usize,
}

impl BidiPeer for Peer<'_> {
    fn poll_request(&mut self, cx: &mut std::task::Context<'_>) -> Poll<Option<Vec<u8>>> {
        if self.closing {
            return Poll::Ready(None);
        }
        if let Some((sequence, request)) = self.resend.pop_front() {
            if Some(sequence) == *self.last_sequence {
                self.closing = true;
            }
            self.ack_queue.push_back((sequence, request.clone()));
            return Poll::Ready(Some(request));
        }
        match self.receiver.poll_recv(cx) {
            Poll::Pending => Poll::Pending,
            // Every sender is gone without a last event: end the stream.
            Poll::Ready(None) => Poll::Ready(None),
            Poll::Ready(Some(command)) => {
                let (sequence, request) = match command {
                    Command::Event {
                        sequence,
                        time,
                        payload,
                    } => (
                        sequence,
                        stream_request(self.context, sequence, time, Some(&payload)),
                    ),
                    Command::Last { sequence, time } => {
                        *self.last_sequence = Some(sequence);
                        self.closing = true;
                        (sequence, stream_request(self.context, sequence, time, None))
                    }
                };
                self.ack_queue.push_back((sequence, request.clone()));
                Poll::Ready(Some(request))
            }
        }
    }

    fn on_message(&mut self, message: bytes::Bytes) -> Result<(), Status> {
        let mut sequence = 0i64;
        for field in reapi::fields(&message).map_err(Status::local)? {
            if let (2, reapi::Value::Varint(value)) = field {
                sequence = value as i64;
            }
        }
        match self.ack_queue.front() {
            Some((expected, _)) if *expected == sequence => {
                self.ack_queue.pop_front();
                self.acks += 1;
                Ok(())
            }
            Some((expected, _)) => Err(Status::new(
                Code::FailedPrecondition,
                format!(
                    "Expected ACK with seqNum={expected} but received ACK with seqNum={sequence}"
                ),
            )),
            None => Err(Status::new(
                Code::FailedPrecondition,
                format!("Received ACK (seqNum={sequence}) when no ACK was expected"),
            )),
        }
    }
}

impl Run {
    /// The headers of one call: `--bes_header` (the last value of a name),
    /// RequestMetadata (TracingMetadataUtils.buildMetadata), then the
    /// credential helper's headers for the service URI.
    fn metadata(&self, action: &str) -> Result<Vec<(String, String)>, Status> {
        use base64::Engine as _;
        let mut headers: Vec<(String, String)> = Vec::new();
        for (name, value) in &self.options.headers {
            let name = name.to_ascii_lowercase();
            match headers.iter_mut().find(|(known, _)| *known == name) {
                Some(entry) => entry.1 = value.clone(),
                None => headers.push((name, value.clone())),
            }
        }
        let mut tool = Vec::new();
        reapi::string(&mut tool, 1, "bazel-rs");
        reapi::string(&mut tool, 2, env!("CARGO_PKG_VERSION"));
        let mut metadata = Vec::new();
        reapi::message(&mut metadata, 1, &tool);
        reapi::string(&mut metadata, 2, action);
        reapi::string(&mut metadata, 3, &self.context.invocation_id);
        reapi::string(&mut metadata, 4, &self.context.build_id);
        headers.push((
            "build.bazel.remote.execution.v2.requestmetadata-bin".into(),
            base64::engine::general_purpose::STANDARD_NO_PAD.encode(metadata),
        ));
        if let Some(credentials) = crate::credentials::global() {
            let authority = &self.channel.target().authority;
            let authority = authority.strip_suffix(":443").unwrap_or(authority);
            let uri = format!("https://{authority}/{SERVICE}");
            let helper = credentials.headers(&uri).map_err(|error| Status {
                cause: Some(error.0),
                credential_helper: true,
                ..Status::new(
                    Code::Unauthenticated,
                    "Failed computing credential metadata",
                )
            })?;
            headers.extend(
                helper
                    .unwrap_or_default()
                    .into_iter()
                    .map(|(name, value)| (name.to_ascii_lowercase(), value)),
            );
        }
        Ok(headers)
    }

    /// retrySleepMillis: the minimum delay times 1.6^attempt.
    fn delay(&self, attempt: u32) -> Duration {
        self.options
            .retry_delay
            .mul_f64(1.6f64.powi(attempt as i32))
    }

    /// publishLifecycleEvent: retried up to `--build_event_upload_max_retries`.
    fn publish_lifecycle(&self, event: Lifecycle) -> Result<(), UploadError> {
        let request = lifecycle_request(&self.context, event);
        let path = format!("/{SERVICE}/PublishLifecycleEvent");
        let mut attempt = 0u32;
        let mut cause = None;
        while attempt <= self.options.max_retries {
            let result = self
                .metadata("publish_lifecycle_event")
                .and_then(|metadata| {
                    self.channel.unary(
                        &Call {
                            path: &path,
                            metadata: &metadata,
                            timeout: Some(LIFECYCLE_DEADLINE),
                        },
                        request.clone(),
                    )
                });
            match result {
                Ok(_) => return Ok(()),
                Err(status) => {
                    if !retryable(&status) || start_new_invocation(&status) {
                        let message = format!(
                            "Not retrying publishLifecycleEvent: status='{}'",
                            status_text(&status)
                        );
                        return Err(failure(&status, message, 45));
                    }
                    std::thread::sleep(self.delay(attempt));
                    attempt += 1;
                    cause = Some(status);
                }
            }
        }
        let status = cause.unwrap_or_else(|| Status::new(Code::Unknown, ""));
        Err(failure(
            &status,
            format!("All {} retry attempts failed.", attempt.saturating_sub(1)),
            38,
        ))
    }

    /// BuildEventServiceUploader.run.
    fn run(
        &mut self,
        uploader: &Uploader,
        command_start: SystemTime,
        stream_start: SystemTime,
    ) -> Result<(), UploadError> {
        if self.options.lifecycle_events {
            self.publish_lifecycle(Lifecycle::BuildEnqueued(command_start))?;
            self.publish_lifecycle(Lifecycle::InvocationStarted(stream_start))?;
        }
        let result = self.publish_build_events();
        if self.options.lifecycle_events {
            let status = uploader.status();
            let now = SystemTime::now();
            // Exceptions of the `finally` block replace the stream's.
            self.publish_lifecycle(Lifecycle::InvocationFinished(now, status))?;
            self.publish_lifecycle(Lifecycle::BuildFinished(now, status))?;
        }
        result
    }

    /// publishBuildEvents: one stream per attempt; a failed attempt resends
    /// the unacknowledged events after the backoff, the attempt counter
    /// starting over once the server acknowledged something.
    fn publish_build_events(&mut self) -> Result<(), UploadError> {
        let path = format!("/{SERVICE}/PublishBuildToolEventStream");
        let mut resend: VecDeque<(i64, Vec<u8>)> = VecDeque::new();
        let mut last_sequence: Option<i64> = None;
        let mut attempt = 0u32;
        loop {
            let metadata = self.metadata("publish_build_tool_event_stream");
            let (status, unacked, acks, last_sent) = {
                let mut peer = Peer {
                    receiver: &mut self.receiver,
                    context: &self.context,
                    resend: &mut resend,
                    ack_queue: VecDeque::new(),
                    last_sequence: &mut last_sequence,
                    closing: false,
                    acks: 0,
                };
                let status = match metadata {
                    Ok(metadata) => {
                        let call = Call {
                            path: &path,
                            metadata: &metadata,
                            timeout: None,
                        };
                        self.channel.bidi_streaming(&call, &mut peer)
                    }
                    Err(status) => Err(status),
                };
                let last_sent = peer.closing;
                (status, peer.ack_queue, peer.acks, last_sent)
            };
            let status = match status {
                Ok(()) => Status::new(Code::Ok, ""),
                Err(status) => status,
            };
            if status.code == Code::Ok {
                if last_sent && unacked.is_empty() {
                    return Ok(());
                }
                let (description, code) = if last_sent {
                    (
                        format!(
                            "Server closed stream with status OK but not all ACKs have been received (ackQueue={})",
                            unacked.len()
                        ),
                        45,
                    )
                } else {
                    (
                        "Server closed stream with status OK but not all events have been sent"
                            .to_owned(),
                        45,
                    )
                };
                let status = Status::new(Code::FailedPrecondition, description.clone());
                return Err(failure(&status, description, code));
            }
            if last_sent && unacked.is_empty() {
                return Err(failure(&status, status.message.clone(), 45));
            }
            if !retryable(&status) || start_new_invocation(&status) {
                let message = format!(
                    "Not retrying publishBuildEvents: status='{}'",
                    status_text(&status)
                );
                let code = if start_new_invocation(&status) {
                    38
                } else {
                    45
                };
                return Err(failure(&status, message, code));
            }
            if attempt == self.options.max_retries {
                let message = format!(
                    "Not retrying publishBuildEvents, no more attempts left: status='{}'",
                    status_text(&status)
                );
                return Err(failure(&status, message, 38));
            }
            for request in unacked.into_iter().rev() {
                resend.push_front(request);
            }
            std::thread::sleep(self.delay(attempt));
            if acks > 0 {
                attempt = 0;
            } else {
                attempt += 1;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn options_follow_build_event_service_options() {
        let mut options = BesOptions::default();
        assert_eq!(
            options.parse("--bes_backend=grpc://127.0.0.1:1", None),
            Some(Ok(false))
        );
        assert!(options.enabled());
        assert_eq!(options.parse("--project_id", Some("p")), Some(Ok(true)));
        assert_eq!(options.instance_name.as_deref(), Some("p"));
        assert_eq!(options.parse("--bes_keywords=a,,b", None), Some(Ok(false)));
        assert_eq!(options.parse("--bes_keywords=a", None), Some(Ok(false)));
        assert_eq!(
            options.keywords("build"),
            [
                "protocol_name=BEP",
                "command_name=build",
                "user_keyword=a",
                "user_keyword=b"
            ]
        );
        assert_eq!(
            options.parse("--bes_upload_mode=FULLY_ASYNC", None),
            Some(Ok(false))
        );
        assert_eq!(options.upload_mode, UploadMode::FullyAsync);
        assert_eq!(
            options.parse("--bes_upload_mode=x", None),
            Some(Err("While parsing option --bes_upload_mode=x: Not a valid Mode for uploading to the Build Event Service: 'x' (should be wait_for_upload_complete, nowait_for_upload_complete or fully_async)".into()))
        );
        assert_eq!(
            options.parse("--bes_timeout=5", None),
            Some(Err(
                "While parsing option --bes_timeout=5: Illegal duration '5'.".into()
            ))
        );
        assert_eq!(
            options.parse("--nobes_lifecycle_events", None),
            Some(Ok(false))
        );
        assert!(!options.lifecycle_events);
        assert_eq!(
            options.parse("--bes_results_url=http://r", None),
            Some(Ok(false))
        );
        assert_eq!(options.results_prefix().as_deref(), Some("http://r/"));
        assert!(options.parse("--bes_header=a", None).unwrap().is_err());
        assert_eq!(options.parse("--other", None), None);
    }

    #[test]
    fn requests_follow_build_event_service_proto_util() {
        let context = Context {
            build_id: "b".into(),
            invocation_id: "i".into(),
            keywords: vec!["k".into()],
            project_id: Some("p".into()),
            check_preceding: true,
        };
        let time = SystemTime::UNIX_EPOCH + Duration::new(5, 6);
        let first = stream_request(&context, 1, time, Some(b"x"));
        let fields = reapi::fields(&first).unwrap();
        let numbers: Vec<u64> = fields.iter().map(|(number, _)| *number).collect();
        assert_eq!(numbers, [4, 5, 6, 7]);
        let second = stream_request(&context, 2, time, None);
        let numbers: Vec<u64> = reapi::fields(&second)
            .unwrap()
            .iter()
            .map(|(number, _)| *number)
            .collect();
        assert_eq!(numbers, [4, 6]);
        let enqueued = lifecycle_request(&context, Lifecycle::BuildEnqueued(time));
        let numbers: Vec<u64> = reapi::fields(&enqueued)
            .unwrap()
            .iter()
            .map(|(number, _)| *number)
            .collect();
        assert_eq!(numbers, [1, 2, 4, 6]);
        let finished = lifecycle_request(
            &context,
            Lifecycle::InvocationFinished(time, InvocationStatus::Failed),
        );
        let numbers: Vec<u64> = reapi::fields(&finished)
            .unwrap()
            .iter()
            .map(|(number, _)| *number)
            .collect();
        assert_eq!(numbers, [1, 2, 6]);
        assert_eq!(
            status_text(&Status::new(Code::InvalidArgument, "bad")),
            "Status{code=INVALID_ARGUMENT, description=bad, cause=null}"
        );
        assert_eq!(
            user_readable(&Status::new(Code::InvalidArgument, "bad")),
            "INVALID_ARGUMENT: INVALID_ARGUMENT: bad"
        );
    }
}
