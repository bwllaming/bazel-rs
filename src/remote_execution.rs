//! Bazel 9.3's remote execution protocol (`--remote_executor`,
//! docs/REMOTE_EXECUTION.md): `Execution.Execute`/`WaitExecution` streams of
//! `google.longrunning.Operation`s, the `ExecuteResponse` and
//! `google.rpc.Status` messages, the error texts and retry classifications,
//! and the spawn's `Platform` (PlatformUtils).
//!
//! Pinned source (`src/main/java/com/google/devtools/build/lib/`):
//! `remote/GrpcRemoteExecutor.java` (the Execute/WaitExecution loop),
//! `remote/ExecutionStatusException.java`, `remote/ExecuteRetrier.java`,
//! `remote/RemoteRetrier.java` (GRPC_RESULT_CLASSIFIER),
//! `remote/RemoteExecutionService.java:2017-2054` (the ExecuteRequest),
//! `remote/util/Utils.java:189-383` (grpcAwareErrorMessage and the error
//! detail texts) and `analysis/platform/PlatformUtils.java`.

use crate::grpc_channel::{Code, Status};
use crate::reapi::{self, Digest, Value};
use crate::remote_grpc::GrpcCache;

pub const EXECUTE_RESPONSE_TYPE: &str =
    "type.googleapis.com/build.bazel.remote.execution.v2.ExecuteResponse";
const RETRY_INFO_TYPE: &str = "type.googleapis.com/google.rpc.RetryInfo";
const DEBUG_INFO_TYPE: &str = "type.googleapis.com/google.rpc.DebugInfo";
const QUOTA_FAILURE_TYPE: &str = "type.googleapis.com/google.rpc.QuotaFailure";
const PRECONDITION_FAILURE_TYPE: &str = "type.googleapis.com/google.rpc.PreconditionFailure";
const BAD_REQUEST_TYPE: &str = "type.googleapis.com/google.rpc.BadRequest";
const REQUEST_INFO_TYPE: &str = "type.googleapis.com/google.rpc.RequestInfo";
const RESOURCE_INFO_TYPE: &str = "type.googleapis.com/google.rpc.ResourceInfo";
const HELP_TYPE: &str = "type.googleapis.com/google.rpc.Help";
const LOCALIZED_MESSAGE_TYPE: &str = "type.googleapis.com/google.rpc.LocalizedMessage";

/// `google.protobuf.Any`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Any {
    pub type_url: String,
    pub value: Vec<u8>,
}

impl Any {
    pub fn decode(bytes: &[u8]) -> Result<Self, String> {
        let mut any = Any::default();
        for field in reapi::fields(bytes)? {
            match field {
                (1, Value::Bytes(value)) => any.type_url = reapi::utf8(value)?,
                (2, Value::Bytes(value)) => any.value = value.to_vec(),
                _ => {}
            }
        }
        Ok(any)
    }

    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::new();
        reapi::string(&mut out, 1, &self.type_url);
        if !self.value.is_empty() {
            reapi::bytes_field(&mut out, 2, &self.value);
        }
        out
    }

    /// `Any.is(T)`: the type name after the last `/`.
    fn is(&self, type_url: &str) -> bool {
        let name = |url: &str| url.rsplit('/').next().unwrap_or(url).to_owned();
        name(&self.type_url) == name(type_url)
    }
}

/// `google.rpc.Status`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RpcStatus {
    pub code: u64,
    pub message: String,
    pub details: Vec<Any>,
}

impl RpcStatus {
    pub fn decode(bytes: &[u8]) -> Result<Self, String> {
        let mut status = RpcStatus::default();
        for field in reapi::fields(bytes)? {
            match field {
                (1, Value::Varint(value)) => status.code = value & 0xffff_ffff,
                (2, Value::Bytes(value)) => status.message = reapi::utf8(value)?,
                (3, Value::Bytes(value)) => status.details.push(Any::decode(value)?),
                _ => {}
            }
        }
        Ok(status)
    }

    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::new();
        reapi::varint_field(&mut out, 1, self.code);
        reapi::string(&mut out, 2, &self.message);
        for detail in &self.details {
            reapi::message(&mut out, 3, &detail.encode());
        }
        out
    }

    fn grpc_code(&self) -> Code {
        Code::from_value(self.code)
    }
}

/// `ExecuteResponse`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ExecuteResponse {
    /// The encoded `ActionResult`, if any.
    pub result: Option<Vec<u8>>,
    pub cached_result: bool,
    pub status: Option<RpcStatus>,
    /// The number of `server_logs` entries.
    pub server_logs: usize,
    pub message: String,
}

impl ExecuteResponse {
    pub fn decode(bytes: &[u8]) -> Result<Self, String> {
        let mut response = ExecuteResponse::default();
        for field in reapi::fields(bytes)? {
            match field {
                (1, Value::Bytes(value)) => response.result = Some(value.to_vec()),
                (2, Value::Varint(value)) => response.cached_result = value != 0,
                (3, Value::Bytes(value)) => response.status = Some(RpcStatus::decode(value)?),
                (4, Value::Bytes(_)) => response.server_logs += 1,
                (5, Value::Bytes(value)) => response.message = reapi::utf8(value)?,
                _ => {}
            }
        }
        Ok(response)
    }

    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::new();
        if let Some(result) = &self.result {
            reapi::message(&mut out, 1, result);
        }
        reapi::varint_field(&mut out, 2, u64::from(self.cached_result));
        if let Some(status) = &self.status {
            reapi::message(&mut out, 3, &status.encode());
        }
        reapi::string(&mut out, 5, &self.message);
        out
    }
}

/// `google.longrunning.Operation`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Operation {
    pub name: String,
    pub metadata: Option<Any>,
    pub done: bool,
    pub error: Option<RpcStatus>,
    pub response: Option<Any>,
}

impl Operation {
    pub fn decode(bytes: &[u8]) -> Result<Self, String> {
        let mut operation = Operation::default();
        for field in reapi::fields(bytes)? {
            match field {
                (1, Value::Bytes(value)) => operation.name = reapi::utf8(value)?,
                (2, Value::Bytes(value)) => operation.metadata = Some(Any::decode(value)?),
                (3, Value::Varint(value)) => operation.done = value != 0,
                (4, Value::Bytes(value)) => operation.error = Some(RpcStatus::decode(value)?),
                (5, Value::Bytes(value)) => operation.response = Some(Any::decode(value)?),
                _ => {}
            }
        }
        Ok(operation)
    }

    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::new();
        reapi::string(&mut out, 1, &self.name);
        if let Some(metadata) = &self.metadata {
            reapi::message(&mut out, 2, &metadata.encode());
        }
        reapi::varint_field(&mut out, 3, u64::from(self.done));
        if let Some(error) = &self.error {
            reapi::message(&mut out, 4, &error.encode());
        }
        if let Some(response) = &self.response {
            reapi::message(&mut out, 5, &response.encode());
        }
        out
    }
}

/// RemoteExecutionService.executeRemotely's ExecuteRequest: the instance,
/// the action digest, `skip_cache_lookup` and SHA256 (no priorities, no
/// inlined outputs).
pub fn execute_request(instance_name: &str, action: &Digest, skip_cache_lookup: bool) -> Vec<u8> {
    let mut out = Vec::new();
    reapi::string(&mut out, 1, instance_name);
    reapi::varint_field(&mut out, 3, u64::from(skip_cache_lookup));
    reapi::message(&mut out, 6, &action.encode());
    reapi::varint_field(&mut out, 9, 1);
    out
}

/// A failed remote execution.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ExecError {
    /// ExecutionStatusException: an Operation's `error` or an
    /// ExecuteResponse's non-OK `status`, with the response if there was one.
    Execution {
        status: RpcStatus,
        response: Option<Box<ExecuteResponse>>,
    },
    /// A failed Execute/WaitExecution call (a StatusRuntimeException).
    Call(Status),
    /// An IOException without a gRPC status (its message).
    Io(String),
}

impl ExecError {
    /// ExecutionStatusException.isExecutionTimeout: DEADLINE_EXCEEDED as the
    /// response's own status.
    pub fn is_execution_timeout(&self) -> bool {
        match self {
            ExecError::Execution {
                status,
                response: Some(response),
            } => {
                status.grpc_code() == Code::DeadlineExceeded
                    && response.status.as_ref() == Some(status)
            }
            _ => false,
        }
    }

    /// The status code a StatusRuntimeException carries
    /// (ExecutionStatusException.convertStatus turns an execution timeout
    /// into FAILED_PRECONDITION, which is never retried).
    fn code(&self) -> Option<Code> {
        match self {
            ExecError::Execution { status, .. } if self.is_execution_timeout() => {
                let _ = status;
                Some(Code::FailedPrecondition)
            }
            ExecError::Execution { status, .. } => Some(status.grpc_code()),
            ExecError::Call(status) if status.local => None,
            ExecError::Call(status) => Some(status.code),
            ExecError::Io(_) => None,
        }
    }

    /// RemoteRetrier.GRPC_RESULT_CLASSIFIER: NOT_FOUND (a lost operation)
    /// and the transient codes of EXPERIMENTAL_GRPC_RESULT_CLASSIFIER.
    fn transient_call(&self) -> bool {
        if let ExecError::Call(status) = self
            && status.credential_helper
        {
            return false;
        }
        matches!(
            self.code(),
            Some(
                Code::NotFound
                    | Code::Cancelled
                    | Code::Unknown
                    | Code::DeadlineExceeded
                    | Code::Aborted
                    | Code::Internal
                    | Code::Unavailable
                    | Code::ResourceExhausted
            )
        )
    }

    /// ExecuteRetrier.resultClassifier: retried when the status details
    /// carry a RetryInfo, or for a FAILED_PRECONDITION whose
    /// PreconditionFailure violations are all `MISSING` (the inputs are
    /// uploaded again). Statuses without details are final. Returns the
    /// recommended delay.
    pub fn retry_delay(&self) -> Option<std::time::Duration> {
        let ExecError::Execution { status, .. } = self else {
            return None;
        };
        if status.details.is_empty() {
            return None;
        }
        let mut failed_precondition = self.code() == Some(Code::FailedPrecondition);
        for detail in &status.details {
            if detail.is(RETRY_INFO_TYPE) {
                return Some(retry_info_delay(&detail.value));
            } else if failed_precondition {
                if detail.is(PRECONDITION_FAILURE_TYPE) {
                    let violations = precondition_violations(&detail.value);
                    if violations.is_empty()
                        || violations.iter().any(|(kind, _, _)| kind != "MISSING")
                    {
                        failed_precondition = false;
                    }
                } else if ![
                    DEBUG_INFO_TYPE,
                    HELP_TYPE,
                    LOCALIZED_MESSAGE_TYPE,
                    REQUEST_INFO_TYPE,
                    RESOURCE_INFO_TYPE,
                ]
                .iter()
                .any(|benign| detail.is(benign))
                {
                    failed_precondition = false;
                }
            }
        }
        failed_precondition.then_some(std::time::Duration::ZERO)
    }

    /// Whether the cause is UNAVAILABLE (a catastrophic failure in
    /// RemoteSpawnRunner.handleError).
    pub fn unavailable(&self) -> bool {
        self.code() == Some(Code::Unavailable)
    }

    pub fn unauthenticated(&self) -> bool {
        self.code() == Some(Code::Unauthenticated)
    }

    /// Utils.grpcAwareErrorMessage, then RemoteSpawnRunner.handleError's
    /// `"\n" + response.message`.
    pub fn message(&self) -> String {
        match self {
            ExecError::Execution { status, response } => {
                let mut text = format!(
                    "Remote Execution Failure:\n{}: {}\n{}",
                    status_name(status.code),
                    status.message,
                    details_message(&status.details)
                );
                if let Some(response) = response
                    && !response.message.is_empty()
                {
                    text.push('\n');
                    text.push_str(&response.message);
                }
                text
            }
            ExecError::Call(status) => status.to_string(),
            ExecError::Io(message) => message.clone(),
        }
    }
}

/// Utils.statusName: `DEADLINE_EXCEEDED` as `Deadline Exceeded`.
fn status_name(code: u64) -> String {
    Code::from_value(code)
        .name()
        .split('_')
        .map(|word| {
            let mut chars = word.chars();
            match chars.next() {
                Some(first) => {
                    first.to_ascii_uppercase().to_string() + &chars.as_str().to_ascii_lowercase()
                }
                None => String::new(),
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

fn strings(bytes: &[u8], number: u64) -> Vec<String> {
    reapi::fields(bytes)
        .unwrap_or_default()
        .into_iter()
        .filter_map(|field| match field {
            (n, Value::Bytes(value)) if n == number => {
                Some(String::from_utf8_lossy(value).into_owned())
            }
            _ => None,
        })
        .collect()
}

fn string_of(bytes: &[u8], number: u64) -> String {
    strings(bytes, number).pop().unwrap_or_default()
}

fn messages(bytes: &[u8], number: u64) -> Vec<Vec<u8>> {
    reapi::fields(bytes)
        .unwrap_or_default()
        .into_iter()
        .filter_map(|field| match field {
            (n, Value::Bytes(value)) if n == number => Some(value.to_vec()),
            _ => None,
        })
        .collect()
}

/// `google.protobuf.Duration` (seconds, nanos).
fn duration(bytes: &[u8]) -> (i64, i32) {
    let (mut seconds, mut nanos) = (0i64, 0i32);
    for field in reapi::fields(bytes).unwrap_or_default() {
        match field {
            (1, Value::Varint(value)) => seconds = value as i64,
            (2, Value::Varint(value)) => nanos = value as i64 as i32,
            _ => {}
        }
    }
    (seconds, nanos)
}

fn retry_info_delay(bytes: &[u8]) -> std::time::Duration {
    let (seconds, nanos) = messages(bytes, 1)
        .first()
        .map(|value| duration(value))
        .unwrap_or_default();
    std::time::Duration::new(seconds.max(0) as u64, nanos.max(0) as u32)
}

/// Durations.toString: seconds, then 3, 6 or 9 fractional digits, then `s`.
fn duration_text((seconds, nanos): (i64, i32)) -> String {
    let negative = seconds < 0 || nanos < 0;
    let (seconds, nanos) = (seconds.unsigned_abs(), nanos.unsigned_abs());
    let mut text = String::new();
    if negative {
        text.push('-');
    }
    text.push_str(&seconds.to_string());
    if nanos != 0 {
        let fraction = if nanos % 1_000_000 == 0 {
            format!("{:03}", nanos / 1_000_000)
        } else if nanos % 1_000 == 0 {
            format!("{:06}", nanos / 1_000)
        } else {
            format!("{nanos:09}")
        };
        text.push('.');
        text.push_str(&fraction);
    }
    text.push('s');
    text
}

/// PreconditionFailure violations `(type, subject, description)`.
fn precondition_violations(bytes: &[u8]) -> Vec<(String, String, String)> {
    messages(bytes, 1)
        .iter()
        .map(|violation| {
            (
                string_of(violation, 1),
                string_of(violation, 2),
                string_of(violation, 3),
            )
        })
        .collect()
}

/// protobuf TextFormat's escaping of a bytes value.
fn escape_bytes(bytes: &[u8]) -> String {
    let mut out = String::new();
    for byte in bytes {
        match byte {
            b'\n' => out.push_str("\\n"),
            b'\r' => out.push_str("\\r"),
            b'\t' => out.push_str("\\t"),
            b'"' => out.push_str("\\\""),
            b'\'' => out.push_str("\\'"),
            b'\\' => out.push_str("\\\\"),
            0x20..=0x7e => out.push(*byte as char),
            _ => out.push_str(&format!("\\{byte:03o}")),
        }
    }
    out
}

/// Utils.errorDetailMessage.
fn detail_message(detail: &Any) -> String {
    let value = &detail.value;
    if detail.is(RETRY_INFO_TYPE) {
        let delay = messages(value, 1)
            .first()
            .map(|value| duration(value))
            .unwrap_or_default();
        return format!("Retry delay recommendation of {}", duration_text(delay));
    }
    if detail.is(DEBUG_INFO_TYPE) {
        let stack = strings(value, 1);
        let detail = string_of(value, 2);
        let mut message = String::new();
        if !stack.is_empty() {
            message.push_str("Debug Stack Information:\n  ");
            message.push_str(&stack.join("\n  "));
        }
        if !detail.is_empty() {
            if !stack.is_empty() {
                message.push('\n');
            }
            message.push_str("Debug Details: ");
            message.push_str(&detail);
        }
        return message;
    }
    if detail.is(QUOTA_FAILURE_TYPE) {
        let violations = messages(value, 1);
        let mut message = "Quota Failure".to_owned();
        if !violations.is_empty() {
            message.push(':');
        }
        for violation in violations {
            message.push_str(&format!(
                "\n    {}: {}",
                string_of(&violation, 1),
                string_of(&violation, 2)
            ));
        }
        return message;
    }
    if detail.is(PRECONDITION_FAILURE_TYPE) {
        let violations = precondition_violations(value);
        let mut message = "Precondition Failure".to_owned();
        if !violations.is_empty() {
            message.push(':');
        }
        for (kind, subject, description) in violations {
            message.push_str(&format!("\n    ({kind}) {subject}: {description}"));
        }
        return message;
    }
    if detail.is(BAD_REQUEST_TYPE) {
        let violations = messages(value, 1);
        let mut message = "Bad Request".to_owned();
        if !violations.is_empty() {
            message.push(':');
        }
        for violation in violations {
            message.push_str(&format!(
                "\n    {}: {}",
                string_of(&violation, 1),
                string_of(&violation, 2)
            ));
        }
        return message;
    }
    if detail.is(REQUEST_INFO_TYPE) {
        return format!(
            "Request Info: {} => {}",
            string_of(value, 1),
            string_of(value, 2)
        );
    }
    if detail.is(RESOURCE_INFO_TYPE) {
        let mut message = format!(
            "Resource Info: {}: name='{}', owner='{}'",
            string_of(value, 1),
            string_of(value, 2),
            string_of(value, 3)
        );
        let description = string_of(value, 4);
        if !description.is_empty() {
            message.push_str(&format!(", description: {description}"));
        }
        return message;
    }
    if detail.is(HELP_TYPE) {
        let links = messages(value, 1);
        let mut message = "Help".to_owned();
        if !links.is_empty() {
            message.push(':');
        }
        for link in links {
            message.push_str(&format!(
                "\n    {}: {}",
                string_of(&link, 1),
                string_of(&link, 2)
            ));
        }
        return message;
    }
    // Any.toString (TextFormat).
    let mut text = format!(
        "type_url: \"{}\"\n",
        escape_bytes(detail.type_url.as_bytes())
    );
    if !detail.value.is_empty() {
        text.push_str(&format!("value: \"{}\"\n", escape_bytes(&detail.value)));
    }
    format!("Unrecognized error detail: {text}")
}

/// Utils.errorDetailsMessage: one `  DETAIL\n` line per detail.
fn details_message(details: &[Any]) -> String {
    details
        .iter()
        .map(|detail| format!("  {}\n", detail_message(detail)))
        .collect()
}

/// GrpcRemoteExecutor.getOperationResponse: the response of a done
/// operation, an ExecutionStatusException for an error or a non-OK status.
fn operation_response(operation: &Operation) -> Result<Option<ExecuteResponse>, ExecError> {
    if let Some(error) = &operation.error
        && error.code != 0
    {
        return Err(ExecError::Execution {
            status: error.clone(),
            response: None,
        });
    }
    if !operation.done {
        return Ok(None);
    }
    let Some(any) = &operation.response else {
        return Err(ExecError::Io(
            "Unexpected result of remote execution: result not set".into(),
        ));
    };
    if !any.is(EXECUTE_RESPONSE_TYPE) {
        return Err(ExecError::Io(format!(
            "Type of the Any message does not match the given class: {}",
            any.type_url
        )));
    }
    let response = ExecuteResponse::decode(&any.value).map_err(ExecError::Io)?;
    if let Some(status) = &response.status
        && status.code != 0
    {
        return Err(ExecError::Execution {
            status: status.clone(),
            response: Some(Box::new(response.clone())),
        });
    }
    if response.result.is_none() {
        return Err(ExecError::Io(
            "Unexpected result of remote execution: no result".into(),
        ));
    }
    Ok(Some(response))
}

/// GrpcRemoteExecutor.executeRemotely: Execute, then WaitExecution on the
/// last operation while the stream ends before it is done (an OK end or a
/// retried failure after an operation); a NOT_FOUND WaitExecution starts
/// over with Execute. Calls carry no deadline (RemoteGrpcServiceConfig
/// covers the cache services only), are retried by the RemoteRetrier
/// (`--remote_retries`, GRPC_RESULT_CLASSIFIER) and refreshed once when
/// UNAUTHENTICATED (Utils.refreshIfUnauthenticated).
pub fn execute(
    endpoint: &GrpcCache,
    request: &[u8],
    action_id: &str,
) -> Result<ExecuteResponse, ExecError> {
    let mut name = String::new();
    let mut wait = false;
    let attempt = |name: &mut String, wait: &mut bool| -> Result<ExecuteResponse, ExecError> {
        let mut backoff = endpoint.backoff();
        loop {
            let result: Result<ExecuteResponse, ExecError> = (|| loop {
                if let Some(response) = operation_stream(endpoint, request, action_id, name, wait)?
                {
                    return Ok(response);
                }
            })();
            match result {
                Err(error) if error.transient_call() => match backoff.next_delay() {
                    Some(delay) => std::thread::sleep(delay),
                    None => return Err(error),
                },
                result => return result,
            }
        }
    };
    match attempt(&mut name, &mut wait) {
        Err(error) if error.unauthenticated() => {
            if let Some(credentials) = crate::credentials::global() {
                credentials.refresh();
            }
            attempt(&mut name, &mut wait)
        }
        result => result,
    }
}

/// GrpcRemoteExecutor.handleOperationStream: one Execute or WaitExecution
/// call; `Ok(None)` when it ended before the operation was done.
fn operation_stream(
    endpoint: &GrpcCache,
    request: &[u8],
    action_id: &str,
    name: &mut String,
    wait: &mut bool,
) -> Result<Option<ExecuteResponse>, ExecError> {
    let (method, body) = if *wait {
        let mut body = Vec::new();
        reapi::string(&mut body, 1, name);
        ("WaitExecution", body)
    } else {
        ("Execute", request.to_vec())
    };
    let mut outcome: Option<Result<ExecuteResponse, ExecError>> = None;
    let streamed = endpoint.operation_stream(method, body, action_id, &mut |message| {
        // The rest of the stream is drained, its errors ignored.
        if outcome.is_some() {
            return Ok(());
        }
        let operation = Operation::decode(&message).map_err(Status::local)?;
        name.clone_from(&operation.name);
        *wait = !operation.done;
        match operation_response(&operation) {
            Ok(Some(response)) => outcome = Some(Ok(response)),
            Ok(None) => {}
            Err(error) => outcome = Some(Err(error)),
        }
        Ok(())
    });
    if let Some(outcome) = outcome {
        return outcome.map(Some);
    }
    match streamed {
        Err(status) => {
            if status.code == Code::NotFound && !status.local {
                // The operation was lost on the server: Execute again.
                *wait = false;
            }
            Err(ExecError::Call(status))
        }
        Ok(()) if !*wait => Err(ExecError::Io(format!(
            "Remote server error: execution request for {name} terminated with no result."
        ))),
        Ok(()) => Ok(None),
    }
}

/// Where a spawn runs, as PlatformUtils.getPlatformProto reads it: the
/// spawn's combined exec properties (Spawn.getCombinedExecProperties: its
/// exec group's platform and target properties, crate::exec_groups) and its
/// execution platform's label, `exec_properties` and
/// `remote_execution_properties`. Recorded at analysis; the
/// `--remote_default_exec_properties` merge happens at execution.
#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct SpawnPlatform {
    pub combined: Vec<(String, String)>,
    pub label: String,
    pub exec_properties: Vec<(String, String)>,
    pub remote_execution_properties: String,
}

/// PlatformUtils.getPlatformProto: the sorted `Platform` properties of a
/// spawn (empty: no Platform message).
pub fn platform_properties(
    platform: &SpawnPlatform,
    defaults: &[(String, String)],
) -> Result<Vec<(String, String)>, String> {
    use std::collections::BTreeMap;
    // getRemoteDefaultExecProperties: the last value of a name wins.
    let defaults: BTreeMap<&str, &str> = defaults
        .iter()
        .map(|(name, value)| (name.as_str(), value.as_str()))
        .collect();
    let produce = !platform.exec_properties.is_empty()
        || !platform.remote_execution_properties.is_empty()
        || !platform.combined.is_empty()
        || !defaults.is_empty();
    if !produce {
        return Ok(Vec::new());
    }
    let mut properties: BTreeMap<String, String> = BTreeMap::new();
    if !platform.combined.is_empty() {
        if platform.exec_properties.is_empty() {
            for (name, value) in &defaults {
                properties.insert((*name).to_owned(), (*value).to_owned());
            }
        }
        for (name, value) in &platform.combined {
            properties.insert(name.clone(), value.clone());
        }
    } else if !platform.remote_execution_properties.is_empty() {
        let parsed = parse_platform_text(&platform.remote_execution_properties).map_err(|_| {
            format!(
                "Failed to parse remote_execution_properties from platform {}",
                platform.label
            )
        })?;
        for (name, value) in parsed {
            properties.insert(name, value);
        }
    } else {
        for (name, value) in &platform.exec_properties {
            properties.insert(name.clone(), value.clone());
        }
    }
    if properties.is_empty() {
        for (name, value) in &defaults {
            properties.insert((*name).to_owned(), (*value).to_owned());
        }
    }
    Ok(properties.into_iter().collect())
}

/// protobuf TextFormat for a `Platform`: `properties { name: "N" value: "V" }`
/// entries (`:` before a message and `<…>` brackets allowed, `,`/`;`
/// separators, `#` comments, adjacent strings concatenated).
pub fn parse_platform_text(text: &str) -> Result<Vec<(String, String)>, String> {
    #[derive(Debug, PartialEq)]
    enum Token {
        Ident(String),
        Str(String),
        Punct(char),
    }
    let bytes = text.as_bytes();
    let mut tokens = Vec::new();
    let mut at = 0;
    while at < bytes.len() {
        let byte = bytes[at];
        if byte.is_ascii_whitespace() {
            at += 1;
        } else if byte == b'#' {
            while at < bytes.len() && bytes[at] != b'\n' {
                at += 1;
            }
        } else if byte.is_ascii_alphabetic() || byte == b'_' {
            let start = at;
            while at < bytes.len() && (bytes[at].is_ascii_alphanumeric() || bytes[at] == b'_') {
                at += 1;
            }
            tokens.push(Token::Ident(text[start..at].to_owned()));
        } else if byte == b'"' || byte == b'\'' {
            let quote = byte;
            at += 1;
            let mut value = Vec::new();
            loop {
                let Some(&next) = bytes.get(at) else {
                    return Err("unterminated string".into());
                };
                at += 1;
                if next == quote {
                    break;
                }
                if next == b'\n' {
                    return Err("string literals cannot cross lines".into());
                }
                if next != b'\\' {
                    value.push(next);
                    continue;
                }
                let escaped = *bytes.get(at).ok_or("unterminated escape")?;
                at += 1;
                match escaped {
                    b'n' => value.push(b'\n'),
                    b'r' => value.push(b'\r'),
                    b't' => value.push(b'\t'),
                    b'a' => value.push(7),
                    b'b' => value.push(8),
                    b'f' => value.push(12),
                    b'v' => value.push(11),
                    b'\\' | b'\'' | b'"' | b'?' => value.push(escaped),
                    b'0'..=b'7' => {
                        let mut code = u32::from(escaped - b'0');
                        for _ in 0..2 {
                            match bytes.get(at) {
                                Some(digit @ b'0'..=b'7') => {
                                    code = code * 8 + u32::from(digit - b'0');
                                    at += 1;
                                }
                                _ => break,
                            }
                        }
                        value.push(code as u8);
                    }
                    b'x' => {
                        let mut code = 0u32;
                        let mut digits = 0;
                        while digits < 2
                            && let Some(digit) =
                                bytes.get(at).and_then(|b| (*b as char).to_digit(16))
                        {
                            code = code * 16 + digit;
                            at += 1;
                            digits += 1;
                        }
                        if digits == 0 {
                            return Err("invalid \\x escape".into());
                        }
                        value.push(code as u8);
                    }
                    _ => return Err("invalid escape sequence".into()),
                }
            }
            let value = String::from_utf8(value).map_err(|_| "invalid UTF-8 in string")?;
            match tokens.last_mut() {
                Some(Token::Str(previous)) => previous.push_str(&value),
                _ => tokens.push(Token::Str(value)),
            }
        } else if b"{}<>:,;".contains(&byte) {
            tokens.push(Token::Punct(byte as char));
            at += 1;
        } else {
            return Err(format!("unexpected character '{}'", byte as char));
        }
    }
    let mut properties = Vec::new();
    let mut position = 0;
    let separator = |position: &mut usize, tokens: &[Token]| {
        if matches!(tokens.get(*position), Some(Token::Punct(',' | ';'))) {
            *position += 1;
        }
    };
    while position < tokens.len() {
        match &tokens[position] {
            Token::Ident(name) if name == "properties" => position += 1,
            other => return Err(format!("unexpected {other:?}")),
        }
        if tokens.get(position) == Some(&Token::Punct(':')) {
            position += 1;
        }
        let close = match tokens.get(position) {
            Some(Token::Punct('{')) => '}',
            Some(Token::Punct('<')) => '>',
            _ => return Err("expected '{'".into()),
        };
        position += 1;
        let (mut name, mut value) = (String::new(), String::new());
        loop {
            match tokens.get(position) {
                Some(Token::Punct(found)) if *found == close => {
                    position += 1;
                    break;
                }
                Some(Token::Ident(field)) if field == "name" || field == "value" => {
                    let field = field.clone();
                    position += 1;
                    if tokens.get(position) != Some(&Token::Punct(':')) {
                        return Err("expected ':'".into());
                    }
                    position += 1;
                    let Some(Token::Str(text)) = tokens.get(position) else {
                        return Err("expected string".into());
                    };
                    if field == "name" {
                        name = text.clone();
                    } else {
                        value = text.clone();
                    }
                    position += 1;
                    separator(&mut position, &tokens);
                }
                other => return Err(format!("unexpected {other:?}")),
            }
        }
        separator(&mut position, &tokens);
        properties.push((name, value));
    }
    Ok(properties)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn any(type_url: &str, value: Vec<u8>) -> Any {
        Any {
            type_url: type_url.into(),
            value,
        }
    }

    #[test]
    fn execution_failures_read_like_grpc_aware_error_message() {
        let mut violation = Vec::new();
        reapi::string(&mut violation, 1, "MISSING");
        reapi::string(&mut violation, 2, "blobs/abc/3");
        reapi::string(&mut violation, 3, "gone");
        let mut failure = Vec::new();
        reapi::message(&mut failure, 1, &violation);
        let error = ExecError::Execution {
            status: RpcStatus {
                code: 9,
                message: "missing inputs".into(),
                details: vec![any(PRECONDITION_FAILURE_TYPE, failure)],
            },
            response: Some(Box::new(ExecuteResponse {
                message: "see the worker log".into(),
                ..ExecuteResponse::default()
            })),
        };
        assert_eq!(
            error.message(),
            "Remote Execution Failure:\nFailed Precondition: missing inputs\n  Precondition Failure:\n    (MISSING) blobs/abc/3: gone\n\nsee the worker log"
        );
        // All violations MISSING: retried at once (inputs re-uploaded).
        assert_eq!(error.retry_delay(), Some(std::time::Duration::ZERO));
        let mut delay = Vec::new();
        reapi::varint_field(&mut delay, 1, 2);
        reapi::varint_field(&mut delay, 2, 500_000_000);
        let mut retry = Vec::new();
        reapi::message(&mut retry, 1, &delay);
        let error = ExecError::Execution {
            status: RpcStatus {
                code: 14,
                message: "busy".into(),
                details: vec![any(RETRY_INFO_TYPE, retry)],
            },
            response: None,
        };
        assert_eq!(
            error.message(),
            "Remote Execution Failure:\nUnavailable: busy\n  Retry delay recommendation of 2.500s\n"
        );
        assert_eq!(
            error.retry_delay(),
            Some(std::time::Duration::from_millis(2500))
        );
        assert!(error.unavailable() && error.transient_call());
        // No details: never retried by ExecuteRetrier.
        let plain = ExecError::Execution {
            status: RpcStatus {
                code: 3,
                message: "bad".into(),
                details: Vec::new(),
            },
            response: None,
        };
        assert_eq!(plain.retry_delay(), None);
        assert_eq!(
            plain.message(),
            "Remote Execution Failure:\nInvalid Argument: bad\n"
        );
    }

    #[test]
    fn deadline_exceeded_responses_are_execution_timeouts() {
        let status = RpcStatus {
            code: 4,
            message: "timed out".into(),
            details: Vec::new(),
        };
        let timeout = ExecError::Execution {
            status: status.clone(),
            response: Some(Box::new(ExecuteResponse {
                status: Some(status.clone()),
                ..ExecuteResponse::default()
            })),
        };
        assert!(timeout.is_execution_timeout());
        // Converted to FAILED_PRECONDITION: not retried.
        assert!(!timeout.transient_call());
        let operation_error = ExecError::Execution {
            status,
            response: None,
        };
        assert!(!operation_error.is_execution_timeout());
        assert!(operation_error.transient_call());
    }

    #[test]
    fn operations_round_trip_and_yield_responses() {
        let response = ExecuteResponse {
            result: Some(Vec::new()),
            cached_result: true,
            message: "m".into(),
            ..ExecuteResponse::default()
        };
        let operation = Operation {
            name: "operations/1".into(),
            done: true,
            response: Some(any(EXECUTE_RESPONSE_TYPE, response.encode())),
            ..Operation::default()
        };
        let decoded = Operation::decode(&operation.encode()).unwrap();
        assert_eq!(decoded, operation);
        assert_eq!(operation_response(&decoded).unwrap(), Some(response));
        let pending = Operation {
            name: "operations/1".into(),
            ..Operation::default()
        };
        assert_eq!(operation_response(&pending).unwrap(), None);
        let failed = Operation {
            error: Some(RpcStatus {
                code: 13,
                message: "boom".into(),
                details: Vec::new(),
            }),
            done: true,
            ..Operation::default()
        };
        assert!(matches!(
            operation_response(&failed),
            Err(ExecError::Execution { response: None, .. })
        ));
    }

    #[test]
    fn platform_properties_follow_platform_utils() {
        let defaults = vec![
            ("OSFamily".to_owned(), "linux".to_owned()),
            ("pool".to_owned(), "a".to_owned()),
            ("pool".to_owned(), "b".to_owned()),
        ];
        // Nothing set: the defaults (last value wins), sorted.
        assert_eq!(
            platform_properties(&SpawnPlatform::default(), &defaults).unwrap(),
            [
                ("OSFamily".to_owned(), "linux".to_owned()),
                ("pool".to_owned(), "b".to_owned())
            ]
        );
        assert!(
            platform_properties(&SpawnPlatform::default(), &[])
                .unwrap()
                .is_empty()
        );
        // Target properties over the defaults when the platform sets none.
        let target = SpawnPlatform {
            combined: vec![("pool".into(), "big".into())],
            ..SpawnPlatform::default()
        };
        assert_eq!(
            platform_properties(&target, &defaults).unwrap(),
            [
                ("OSFamily".to_owned(), "linux".to_owned()),
                ("pool".to_owned(), "big".to_owned())
            ]
        );
        // A platform with exec_properties replaces the defaults.
        let platform = SpawnPlatform {
            combined: vec![("pool".into(), "big".into())],
            label: "//:p".into(),
            exec_properties: vec![("pool".into(), "p".into())],
            remote_execution_properties: String::new(),
        };
        assert_eq!(
            platform_properties(&platform, &defaults).unwrap(),
            [("pool".to_owned(), "big".to_owned())]
        );
        let legacy = SpawnPlatform {
            label: "//:legacy".into(),
            remote_execution_properties:
                "properties: { name: \"container-image\" value: \"docker://x\" }\nproperties { name: 'a', value: \"b\" \"c\" }"
                    .into(),
            ..SpawnPlatform::default()
        };
        assert_eq!(
            platform_properties(&legacy, &defaults).unwrap(),
            [
                ("a".to_owned(), "bc".to_owned()),
                ("container-image".to_owned(), "docker://x".to_owned())
            ]
        );
        let broken = SpawnPlatform {
            label: "//:broken".into(),
            remote_execution_properties: "properties { nam: \"x\" }".into(),
            ..SpawnPlatform::default()
        };
        assert_eq!(
            platform_properties(&broken, &[]).unwrap_err(),
            "Failed to parse remote_execution_properties from platform //:broken"
        );
    }

    #[test]
    fn execute_requests_and_durations() {
        let digest = Digest::of(b"a");
        let request = execute_request("dev", &digest, true);
        let fields = reapi::fields(&request).unwrap();
        assert_eq!(fields.len(), 4);
        assert_eq!(duration_text((1, 0)), "1s");
        assert_eq!(duration_text((0, 1_000)), "0.000001s");
        assert_eq!(duration_text((3, 7)), "3.000000007s");
        assert_eq!(status_name(4), "Deadline Exceeded");
    }
}
