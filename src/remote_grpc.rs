//! Bazel 9.3's REAPI v2 gRPC remote cache client (docs/REMOTE_CACHE.md):
//! Capabilities.GetCapabilities, ActionCache.GetActionResult and
//! UpdateActionResult, ContentAddressableStorage.FindMissingBlobs, and
//! ByteStream Read/Write (with QueryWriteStatus) for every blob, over
//! crate::grpc_channel with hand-encoded protobuf (crate::reapi).
//!
//! Pinned source (`src/main/java/com/google/devtools/build/lib/remote/`):
//! `GrpcCacheClient.java` (requests, resource names, NOT_FOUND as a miss,
//! the read size limit and resumption), `ByteStreamUploader.java` (upload
//! resource names, 16 KiB chunks (`Chunker.java:48`), QueryWriteStatus
//! resumption, early completion), `RemoteServerCapabilities.java` and
//! `GoogleChannelConnectionFactory.java` (the capability check before the
//! first call), `RemoteRetrier.java`/`Retrier.java` (retried codes and the
//! exponential backoff), `util/TracingMetadataUtils.java` (RequestMetadata
//! and the header interceptors), `util/Utils.java`
//! (`refreshIfUnauthenticated`) and `RemoteGrpcServiceConfig.java`
//! (`--remote_timeout` as every call's deadline). Like Bazel 9.3 the cache
//! never calls BatchReadBlobs or BatchUpdateBlobs. `--remote_cache_compression`
//! (off by default) reads and writes blobs of at least
//! `--experimental_remote_cache_compression_threshold` bytes as zstd
//! streams under `compressed-blobs/zstd/` resource names
//! (GrpcCacheClient.getResourceName, ByteStreamUploader
//! .buildUploadResourceName, Chunker), once the server advertised ZSTD.
//!
//! Remote execution (`--remote_executor`, docs/REMOTE_EXECUTION.md) adds
//! Execution.Execute/WaitExecution (server-streaming
//! `google.longrunning.Operation`s through
//! [`crate::grpc_channel::Channel::server_streaming`]) on the same kind of
//! endpoint, one shared endpoint when both options name the same target
//! (RemoteModule.java:742-822); the protocol is crate::remote_execution.

use crate::grpc_channel::{Call, Channel, Code, Limits, Status, Target, TlsOptions};
use crate::reapi::{self, Digest, Value};
use std::hash::{BuildHasher as _, Hasher as _};
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

const CAPABILITIES: &str = "build.bazel.remote.execution.v2.Capabilities";
const ACTION_CACHE: &str = "build.bazel.remote.execution.v2.ActionCache";
const CAS: &str = "build.bazel.remote.execution.v2.ContentAddressableStorage";
const BYTE_STREAM: &str = "google.bytestream.ByteStream";
pub const EXECUTION: &str = "build.bazel.remote.execution.v2.Execution";
/// `DigestFunction.Value.SHA256`.
const SHA256: u64 = 1;
/// Chunker.getDefaultChunkSize.
const CHUNK: usize = 16 * 1024;
/// RemoteOptions.maxOutboundMessageSize.
const MAX_OUTBOUND_MESSAGE: usize = 1024 * 1024;
/// The REAPI versions Bazel 9.3 supports (ApiVersion.java:26-41).
const CLIENT_LOW: ApiVersion = ApiVersion::new(2, 0);
const CLIENT_HIGH: ApiVersion = ApiVersion::new(2, 11);

/// The gRPC-specific options (RemoteOptions, AuthAndTLSOptions).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GrpcOptions {
    pub instance_name: String,
    /// `--remote_timeout`: every call's deadline and the connect timeout.
    pub timeout: Duration,
    /// `--remote_retries`.
    pub retries: u32,
    /// `--remote_retry_max_delay`.
    pub retry_max_delay: Duration,
    /// `--remote_header` then `--remote_cache_header`.
    pub headers: Vec<(String, String)>,
    pub limits: Limits,
    pub tls: TlsOptions,
    /// `--remote_upload_local_results` as given (the capability warning).
    pub upload_local_results: bool,
    /// What the endpoint must support (ServerCapabilitiesRequirement).
    pub requirement: Requirement,
    /// `--remote_cache_compression`.
    pub compression: bool,
    /// `--experimental_remote_cache_compression_threshold` (default 100).
    pub compression_threshold: u64,
}

/// GoogleChannelConnectionFactory's ServerCapabilitiesRequirement: what
/// checkClientServerCompatibility checks for one endpoint.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Requirement {
    Cache,
    Execution,
    ExecutionAndCache,
}

impl Requirement {
    fn execution(self) -> bool {
        matches!(
            self,
            Requirement::Execution | Requirement::ExecutionAndCache
        )
    }

    fn cache(self) -> bool {
        matches!(self, Requirement::Cache | Requirement::ExecutionAndCache)
    }
}

/// `SemVer` / ApiVersion.
#[derive(Clone, Debug, PartialEq, Eq)]
struct ApiVersion {
    major: u64,
    minor: u64,
    patch: u64,
    prerelease: String,
}

impl ApiVersion {
    const fn new(major: u64, minor: u64) -> Self {
        Self {
            major,
            minor,
            patch: 0,
            prerelease: String::new(),
        }
    }

    fn decode(bytes: &[u8]) -> Result<Self, String> {
        let mut version = Self::new(0, 0);
        for field in reapi::fields(bytes)? {
            match field {
                (1, Value::Varint(value)) => version.major = value,
                (2, Value::Varint(value)) => version.minor = value,
                (3, Value::Varint(value)) => version.patch = value,
                (4, Value::Bytes(value)) => version.prerelease = reapi::utf8(value)?,
                _ => {}
            }
        }
        Ok(version)
    }

    /// ApiVersion.compareTo: prereleases sort first.
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        match (self.prerelease.is_empty(), other.prerelease.is_empty()) {
            (false, true) => std::cmp::Ordering::Less,
            (false, false) => self.prerelease.cmp(&other.prerelease),
            (true, false) => std::cmp::Ordering::Greater,
            (true, true) => {
                (self.major, self.minor, self.patch).cmp(&(other.major, other.minor, other.patch))
            }
        }
    }
}

impl std::fmt::Display for ApiVersion {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if !self.prerelease.is_empty() {
            return f.write_str(&self.prerelease);
        }
        write!(f, "{}.{}", self.major, self.minor)?;
        if self.patch != 0 {
            write!(f, ".{}", self.patch)?;
        }
        Ok(())
    }
}

/// ClientApiVersion.highestSupportedVersion.
fn highest_supported(low: &ApiVersion, high: &ApiVersion) -> Option<ApiVersion> {
    let highest_low = if CLIENT_LOW.cmp(low).is_ge() {
        &CLIENT_LOW
    } else {
        low
    };
    let lowest_high = if CLIENT_HIGH.cmp(high).is_le() {
        &CLIENT_HIGH
    } else {
        high
    };
    highest_low
        .cmp(lowest_high)
        .is_le()
        .then(|| lowest_high.clone())
}

/// The `ServerCapabilities` fields the cache checks.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ServerCapabilities {
    digest_functions: Vec<u64>,
    update_enabled: bool,
    /// CacheCapabilities.supported_compressors (`Compressor.Value`).
    compressors: Vec<u64>,
    /// ExecutionCapabilities: `exec_enabled`, `digest_function` (singular)
    /// and `digest_functions`.
    exec_enabled: bool,
    exec_digest_function: u64,
    exec_digest_functions: Vec<u64>,
    deprecated: Option<ApiVersion>,
    low: Option<ApiVersion>,
    high: Option<ApiVersion>,
}

/// A repeated enum, packed or not.
fn enums(value: &Value<'_>, out: &mut Vec<u64>) -> Result<(), String> {
    match value {
        Value::Varint(value) => out.push(*value),
        Value::Bytes(packed) => {
            let mut position = 0;
            while position < packed.len() {
                let mut value = 0u64;
                let mut shift = 0;
                loop {
                    let byte = *packed.get(position).ok_or("truncated packed enum")?;
                    position += 1;
                    value |= u64::from(byte & 0x7f) << shift;
                    if byte & 0x80 == 0 {
                        break;
                    }
                    shift += 7;
                    if shift >= 64 {
                        return Err("malformed packed enum".into());
                    }
                }
                out.push(value);
            }
        }
        Value::Fixed => {}
    }
    Ok(())
}

impl ServerCapabilities {
    pub fn decode(bytes: &[u8]) -> Result<Self, String> {
        let mut capabilities = Self::default();
        for field in reapi::fields(bytes)? {
            match field {
                (1, Value::Bytes(cache)) => {
                    for field in reapi::fields(cache)? {
                        match field {
                            (1, value) => enums(&value, &mut capabilities.digest_functions)?,
                            (6, value) => enums(&value, &mut capabilities.compressors)?,
                            (2, Value::Bytes(update)) => {
                                for field in reapi::fields(update)? {
                                    if let (1, Value::Varint(value)) = field {
                                        capabilities.update_enabled = value != 0;
                                    }
                                }
                            }
                            _ => {}
                        }
                    }
                }
                (2, Value::Bytes(execution)) => {
                    for field in reapi::fields(execution)? {
                        match field {
                            (1, Value::Varint(value)) => capabilities.exec_digest_function = value,
                            (2, Value::Varint(value)) => capabilities.exec_enabled = value != 0,
                            (5, value) => enums(&value, &mut capabilities.exec_digest_functions)?,
                            _ => {}
                        }
                    }
                }
                (3, Value::Bytes(value)) => {
                    capabilities.deprecated = Some(ApiVersion::decode(value)?)
                }
                (4, Value::Bytes(value)) => capabilities.low = Some(ApiVersion::decode(value)?),
                (5, Value::Bytes(value)) => capabilities.high = Some(ApiVersion::decode(value)?),
                _ => {}
            }
        }
        Ok(capabilities)
    }
}

/// `DigestFunction.Value` names (for the capability error).
fn digest_function_name(value: u64) -> String {
    const NAMES: [&str; 10] = [
        "UNKNOWN",
        "SHA256",
        "SHA1",
        "MD5",
        "VSO",
        "SHA384",
        "SHA512",
        "MURMUR3",
        "SHA256TREE",
        "BLAKE3",
    ];
    NAMES
        .get(value as usize)
        .map_or_else(|| "UNRECOGNIZED".to_owned(), |name| (*name).to_owned())
}

/// The outcome of RemoteServerCapabilities.checkClientServerCompatibility
/// for a cache endpoint, and the highest common API version.
#[derive(Debug, PartialEq, Eq)]
pub struct Compatibility {
    pub warnings: Vec<String>,
    pub errors: Vec<String>,
    /// RemoteExecutionService.initUseOutputPaths: `Command.output_paths`
    /// from REAPI 2.1 on, `output_files` before.
    pub output_paths: bool,
}

pub fn check_compatibility(
    capabilities: &ServerCapabilities,
    upload_local_results: bool,
) -> Compatibility {
    check_requirement(
        capabilities,
        upload_local_results,
        false,
        Requirement::Cache,
    )
}

/// `Compressor.Value.ZSTD`.
const ZSTD: u64 = 1;

/// `DigestFunction.Value` list as Java's `List.toString`.
fn digest_function_names(values: &[u64]) -> String {
    values
        .iter()
        .map(|value| digest_function_name(*value))
        .collect::<Vec<_>>()
        .join(", ")
}

/// RemoteServerCapabilities.checkClientServerCompatibility
/// (remote/RemoteServerCapabilities.java:155-250) for `requirement`.
pub fn check_requirement(
    capabilities: &ServerCapabilities,
    upload_local_results: bool,
    compression: bool,
    requirement: Requirement,
) -> Compatibility {
    let mut warnings = Vec::new();
    let mut errors = Vec::new();
    let zero = ApiVersion::new(0, 0);
    let low = capabilities.low.clone().unwrap_or_else(|| zero.clone());
    let high = capabilities.high.clone().unwrap_or(zero);
    // ClientApiVersion.checkServerSupportedVersions.
    let mut highest = highest_supported(&low, &high);
    if highest.is_none() {
        let deprecated = capabilities
            .deprecated
            .as_ref()
            .and_then(|deprecated| highest_supported(deprecated, &high));
        match deprecated {
            Some(version) => {
                warnings.push(format!(
                    "The highest API version Bazel support {version} is deprecated by the server. Please upgrade to server's recommended version: {low} to {high}."
                ));
                highest = Some(version);
            }
            None => errors.push(format!(
                "The client supported API versions, {CLIENT_LOW} to {CLIENT_HIGH}, is not supported by the server, {low} to {high}. Please switch to a different server or upgrade Bazel."
            )),
        }
    }
    let output_paths = highest.is_none_or(|version| version.cmp(&ApiVersion::new(2, 1)).is_ge());
    if requirement.execution() {
        if !capabilities.exec_enabled {
            errors.push("Remote execution is not supported by the remote server, or the current account is not authorized to use remote execution.".into());
            return Compatibility {
                warnings,
                errors,
                output_paths,
            };
        }
        if capabilities.exec_digest_functions.is_empty() && capabilities.exec_digest_function != 0 {
            if capabilities.exec_digest_function != SHA256 {
                errors.push(format!(
                    "Cannot use hash function SHA256 with remote execution. Server supported function is {}",
                    digest_function_name(capabilities.exec_digest_function)
                ));
            }
        } else if !capabilities.exec_digest_functions.contains(&SHA256) {
            errors.push(format!(
                "Cannot use hash function SHA256 with remote execution. Server supported functions are: [{}]",
                digest_function_names(&capabilities.exec_digest_functions)
            ));
        }
    }
    if requirement.cache() {
        if !capabilities.digest_functions.contains(&SHA256) {
            errors.push(format!(
                "Cannot use hash function SHA256 with remote cache. Server supported functions are: [{}]",
                digest_function_names(&capabilities.digest_functions)
            ));
        }
        if upload_local_results && !capabilities.update_enabled {
            warnings.push("--remote_upload_local_results is set, but the remote cache does not support uploading action results or the current account is not authorized to write local results to the remote cache.".into());
        }
        if compression && !capabilities.compressors.contains(&ZSTD) {
            errors.push(
                "--remote_cache_compression requested but remote does not support compression"
                    .into(),
            );
        }
    }
    Compatibility {
        warnings,
        errors,
        output_paths,
    }
}

fn random_u64() -> u64 {
    let mut hasher = std::collections::hash_map::RandomState::new().build_hasher();
    hasher.write_u128(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos(),
    );
    hasher.finish()
}

/// A random (version 4) UUID.
pub fn random_uuid() -> String {
    let mut bytes = [0u8; 16];
    bytes[..8].copy_from_slice(&random_u64().to_le_bytes());
    bytes[8..].copy_from_slice(&random_u64().to_le_bytes());
    bytes[6] = (bytes[6] & 0x0f) | 0x40;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    let hex = reapi::hex(&bytes);
    format!(
        "{}-{}-{}-{}-{}",
        &hex[..8],
        &hex[8..12],
        &hex[12..16],
        &hex[16..20],
        &hex[20..]
    )
}

/// RemoteRetrier.EXPERIMENTAL_GRPC_RESULT_CLASSIFIER: transient codes.
fn retriable(status: &Status) -> bool {
    !status.local
        && !status.credential_helper
        && matches!(
            status.code,
            Code::Cancelled
                | Code::Unknown
                | Code::DeadlineExceeded
                | Code::Aborted
                | Code::Internal
                | Code::Unavailable
                | Code::ResourceExhausted
        )
}

/// RemoteRetrier.ExponentialBackoff: 100 ms, doubling up to
/// `--remote_retry_max_delay`, jitter 0.1, `--remote_retries` attempts.
pub struct Backoff {
    next: Duration,
    max: Duration,
    attempts: u32,
    max_attempts: u32,
}

impl Backoff {
    fn new(options: &GrpcOptions) -> Self {
        Self::with(options.retries, options.retry_max_delay)
    }

    /// `--remote_retries` attempts, delays capped at
    /// `--remote_retry_max_delay` (the HTTP cache's retrier too).
    pub fn with(retries: u32, retry_max_delay: Duration) -> Self {
        let initial = Duration::from_millis(100);
        Self {
            next: initial,
            max: retry_max_delay.max(initial),
            attempts: 0,
            max_attempts: retries,
        }
    }

    pub fn next_delay(&mut self) -> Option<Duration> {
        if self.attempts >= self.max_attempts {
            return None;
        }
        self.attempts += 1;
        // jitter * (nextDouble(2.0) - 1)
        let unit = (random_u64() >> 11) as f64 / (1u64 << 53) as f64;
        let ratio = 0.1 * (unit * 2.0 - 1.0);
        let delay = self.next.mul_f64(1.0 + ratio);
        self.next = (self.next * 2).min(self.max);
        Some(delay)
    }
}

/// RemoteRetrier.ProgressiveBackoff: after progress, one immediate retry
/// and a fresh exponential backoff.
struct ProgressiveBackoff<'a> {
    options: &'a GrpcOptions,
    current: Option<Backoff>,
}

impl<'a> ProgressiveBackoff<'a> {
    fn new(options: &'a GrpcOptions) -> Self {
        Self {
            options,
            current: Some(Backoff::new(options)),
        }
    }

    fn reset(&mut self) {
        self.current = None;
    }

    fn next_delay(&mut self) -> Option<Duration> {
        match &mut self.current {
            None => {
                self.current = Some(Backoff::new(self.options));
                Some(Duration::ZERO)
            }
            Some(backoff) => backoff.next_delay(),
        }
    }
}

/// The configured gRPC cache of this invocation.
pub struct GrpcCache {
    channel: Channel,
    options: GrpcOptions,
    capabilities: OnceLock<Result<bool, String>>,
    /// TracingMetadataUtils: the build request and command ids.
    build_request_id: String,
    command_id: String,
    query_write_status: AtomicBool,
}

impl std::fmt::Debug for GrpcCache {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GrpcCache")
            .field("target", self.channel.target())
            .finish()
    }
}

/// `Digest` as an embedded message.
fn digest_message(out: &mut Vec<u8>, field: u32, digest: &Digest) {
    reapi::message(out, field, &digest.encode());
}

/// The cause of a credential helper failure inside call credentials
/// (grpc-java GoogleAuthLibraryCallCredentials).
fn credential_status(error: crate::credentials::HelperError) -> Status {
    Status {
        cause: Some(error.0),
        credential_helper: true,
        ..Status::new(
            Code::Unauthenticated,
            "Failed computing credential metadata",
        )
    }
}

impl GrpcCache {
    pub fn new(target: &str, options: GrpcOptions) -> Result<Self, String> {
        let target = Target::parse(target)?;
        let channel = Channel::new(
            target,
            options.tls.clone(),
            options.limits,
            concat!("bazel-rs/", env!("CARGO_PKG_VERSION"), " grpc-h2").to_owned(),
        )?;
        Ok(Self {
            channel,
            options,
            capabilities: OnceLock::new(),
            // CommandEnvironment's ids, shared with the build event
            // stream (BuildStarted.uuid) and the BES stream ids.
            build_request_id: crate::bep::build_request_id().to_owned(),
            command_id: crate::bep::invocation_id().to_owned(),
            query_write_status: AtomicBool::new(true),
        })
    }

    /// The URI call credentials are requested for: grpc-java's
    /// `https://AUTHORITY/SERVICE`, without the default port 443.
    pub fn credential_uri(&self, service: &str) -> String {
        let authority = &self.channel.target().authority;
        let authority = authority.strip_suffix(":443").unwrap_or(authority);
        format!("https://{authority}/{service}")
    }

    /// TracingMetadataUtils.buildMetadata: RequestMetadata, binary header.
    fn request_metadata(&self, action_id: &str) -> String {
        use base64::Engine as _;
        let mut tool = Vec::new();
        reapi::string(&mut tool, 1, "bazel-rs");
        reapi::string(&mut tool, 2, env!("CARGO_PKG_VERSION"));
        let mut out = Vec::new();
        reapi::message(&mut out, 1, &tool);
        reapi::string(&mut out, 2, action_id);
        reapi::string(&mut out, 3, &self.command_id);
        reapi::string(&mut out, 4, &self.build_request_id);
        base64::engine::general_purpose::STANDARD_NO_PAD.encode(out)
    }

    /// The headers of one call: `--remote_header`/`--remote_cache_header`
    /// (newCacheHeadersInterceptor), RequestMetadata, then the credential
    /// helper's headers for the service URI.
    fn metadata(&self, service: &str, action_id: &str) -> Result<Vec<(String, String)>, Status> {
        let mut metadata = self
            .options
            .headers
            .iter()
            .map(|(name, value)| (name.to_ascii_lowercase(), value.clone()))
            .collect::<Vec<_>>();
        metadata.push((
            "build.bazel.remote.execution.v2.requestmetadata-bin".into(),
            self.request_metadata(action_id),
        ));
        if let Some(credentials) = crate::credentials::global() {
            let headers = credentials
                .headers(&self.credential_uri(service))
                .map_err(credential_status)?;
            metadata.extend(
                headers
                    .unwrap_or_default()
                    .into_iter()
                    .map(|(name, value)| (name.to_ascii_lowercase(), value)),
            );
        }
        Ok(metadata)
    }

    fn timeout(&self) -> Option<Duration> {
        (!self.options.timeout.is_zero()).then_some(self.options.timeout)
    }

    /// Retrier.execute.
    fn retry<T>(&self, mut call: impl FnMut() -> Result<T, Status>) -> Result<T, Status> {
        let mut backoff = Backoff::new(&self.options);
        loop {
            match call() {
                Err(status) if retriable(&status) => match backoff.next_delay() {
                    Some(delay) => std::thread::sleep(delay),
                    None => return Err(status),
                },
                result => return result,
            }
        }
    }

    /// Utils.refreshIfUnauthenticated: an UNAUTHENTICATED call refreshes
    /// the credentials and runs once more.
    fn refreshing<T>(&self, mut call: impl FnMut() -> Result<T, Status>) -> Result<T, Status> {
        match call() {
            Err(status) if status.code == Code::Unauthenticated => {
                if let Some(credentials) = crate::credentials::global() {
                    credentials.refresh();
                }
                call()
            }
            result => result,
        }
    }

    fn unary(
        &self,
        service: &str,
        method: &str,
        action_id: &str,
        request: &[u8],
    ) -> Result<bytes::Bytes, Status> {
        let metadata = self.metadata(service, action_id)?;
        let path = format!("/{service}/{method}");
        self.channel.unary(
            &Call {
                path: &path,
                metadata: &metadata,
                timeout: self.timeout(),
            },
            request.to_vec(),
        )
    }

    /// A fresh RemoteRetrier backoff (`--remote_retries`,
    /// `--remote_retry_max_delay`).
    pub fn backoff(&self) -> Backoff {
        Backoff::new(&self.options)
    }

    /// `--remote_retries`.
    pub fn retries(&self) -> u32 {
        self.options.retries
    }

    pub fn instance_name(&self) -> &str {
        &self.options.instance_name
    }

    /// One Execution `method` call (`Execute` or `WaitExecution`), every
    /// streamed Operation to `on_message`; no deadline
    /// (RemoteGrpcServiceConfig sets none for the Execution service).
    pub fn operation_stream(
        &self,
        method: &str,
        request: Vec<u8>,
        action_id: &str,
        on_message: &mut dyn FnMut(bytes::Bytes) -> Result<(), Status>,
    ) -> Result<(), Status> {
        let metadata = self.metadata(EXECUTION, action_id)?;
        let path = format!("/{EXECUTION}/{method}");
        self.channel.server_streaming(
            &Call {
                path: &path,
                metadata: &metadata,
                timeout: None,
            },
            request,
            on_message,
        )
    }

    /// The capability check before the first call
    /// (GoogleChannelConnectionFactory.getAndVerifyServerCapabilities):
    /// warnings print once; a failure is every action's
    /// `Failed to query remote execution capabilities: CAUSE`. Returns
    /// whether Command uses `output_paths`.
    pub fn capabilities(&self) -> Result<bool, String> {
        self.capabilities
            .get_or_init(|| {
                let mut request = Vec::new();
                reapi::string(&mut request, 1, &self.options.instance_name);
                let response = self
                    .retry(|| self.unary(CAPABILITIES, "GetCapabilities", "capabilities", &request))
                    .map_err(|status| status.cause_message())?;
                let capabilities = ServerCapabilities::decode(&response)?;
                let result = check_requirement(
                    &capabilities,
                    self.options.upload_local_results,
                    self.options.compression,
                    self.options.requirement,
                );
                for warning in &result.warnings {
                    crate::note!("WARNING: {warning}");
                }
                if let Some((last, rest)) = result.errors.split_last() {
                    for error in rest {
                        crate::note!("ERROR: {error}");
                    }
                    return Err(last.clone());
                }
                Ok(result.output_paths)
            })
            .clone()
            .map_err(|cause| format!("Failed to query remote execution capabilities: {cause}"))
    }

    /// GetActionResult; `Ok(None)` on NOT_FOUND (a miss).
    pub fn get_action_result(&self, key: &Digest) -> Result<Option<bytes::Bytes>, Status> {
        self.get_action_result_inlining(key, &[])
    }

    /// GetActionResult asking the server to inline the output files at
    /// `inline` (`inline_output_files`; a server may not).
    pub fn get_action_result_inlining(
        &self,
        key: &Digest,
        inline: &[&str],
    ) -> Result<Option<bytes::Bytes>, Status> {
        let mut request = Vec::new();
        reapi::string(&mut request, 1, &self.options.instance_name);
        digest_message(&mut request, 2, key);
        for path in inline {
            reapi::string(&mut request, 5, path);
        }
        reapi::varint_field(&mut request, 6, SHA256);
        self.refreshing(|| {
            self.retry(
                || match self.unary(ACTION_CACHE, "GetActionResult", &key.hash, &request) {
                    Ok(bytes) => Ok(Some(bytes)),
                    Err(status) if status.code == Code::NotFound => Ok(None),
                    Err(status) => Err(status),
                },
            )
        })
    }

    /// UpdateActionResult.
    pub fn update_action_result(&self, key: &Digest, result: &[u8]) -> Result<(), Status> {
        let mut request = Vec::new();
        reapi::string(&mut request, 1, &self.options.instance_name);
        digest_message(&mut request, 2, key);
        reapi::message(&mut request, 3, result);
        reapi::varint_field(&mut request, 5, SHA256);
        self.refreshing(|| {
            self.retry(|| self.unary(ACTION_CACHE, "UpdateActionResult", &key.hash, &request))
        })
        .map(|_| ())
    }

    /// GrpcCacheClient.computeMaxMissingBlobsDigestsPerMessage.
    fn max_digests_per_message(&self) -> usize {
        let mut overhead = Vec::new();
        reapi::string(&mut overhead, 1, &self.options.instance_name);
        reapi::varint_field(&mut overhead, 3, SHA256);
        let digest_size = Digest::of(&[1]).encode().len() + 2;
        ((MAX_OUTBOUND_MESSAGE - overhead.len()) / digest_size).max(1)
    }

    /// FindMissingBlobs, split into messages of at most 1 MiB.
    pub fn find_missing(&self, digests: &[Digest], action_id: &str) -> Result<Vec<Digest>, Status> {
        let mut missing = Vec::new();
        for batch in digests.chunks(self.max_digests_per_message()) {
            let mut request = Vec::new();
            reapi::string(&mut request, 1, &self.options.instance_name);
            for digest in batch {
                digest_message(&mut request, 2, digest);
            }
            reapi::varint_field(&mut request, 3, SHA256);
            let response = self.refreshing(|| {
                self.retry(|| self.unary(CAS, "FindMissingBlobs", action_id, &request))
            })?;
            for field in reapi::fields(&response).map_err(Status::local)? {
                if let (2, Value::Bytes(digest)) = field {
                    missing.push(Digest::decode(digest).map_err(Status::local)?);
                }
            }
        }
        Ok(missing)
    }

    /// `bytestream://AUTHORITY/[INSTANCE/]` (ByteStreamBuildEventArtifactUploader).
    pub fn bytestream_prefix(&self) -> String {
        format!(
            "bytestream://{}/{}",
            self.channel.target().authority,
            self.resource_prefix()
        )
    }

    fn resource_prefix(&self) -> String {
        if self.options.instance_name.is_empty() {
            String::new()
        } else {
            format!("{}/", self.options.instance_name)
        }
    }

    /// GrpcCacheClient.shouldCompress: `--remote_cache_compression` and at
    /// least the threshold's size.
    fn compressed(&self, digest: &Digest) -> bool {
        self.options.compression && digest.size >= self.options.compression_threshold
    }

    /// ByteStream.Read of `INSTANCE/blobs/HASH/SIZE` (compressed:
    /// `INSTANCE/compressed-blobs/zstd/HASH/SIZE`, a zstd stream decoded as
    /// it arrives, ZstdDecompressingOutputStream), resumed at the
    /// (uncompressed) bytes received so far; `Ok(None)` on NOT_FOUND. A
    /// server sending more than the digest's size fails the download.
    pub fn read_blob(&self, digest: &Digest, action_id: &str) -> Result<Option<Vec<u8>>, Status> {
        if digest.size == 0 {
            return Ok(Some(Vec::new()));
        }
        let compressed = self.compressed(digest);
        let resource = format!(
            "{}{}/{}/{}",
            self.resource_prefix(),
            if compressed {
                "compressed-blobs/zstd"
            } else {
                "blobs"
            },
            digest.hash,
            digest.size
        );
        let too_many = || {
            Status::local(format!(
                "Received more bytes than expected for digest '{}/{}'. Server may have ignored read_offset.",
                digest.hash, digest.size
            ))
        };
        let path = format!("/{BYTE_STREAM}/Read");
        let mut data = Vec::with_capacity(digest.size.min(64 << 20) as usize);
        let mut backoff = ProgressiveBackoff::new(&self.options);
        let mut refreshed = false;
        loop {
            let metadata = self.metadata(BYTE_STREAM, action_id)?;
            let mut request = Vec::new();
            reapi::string(&mut request, 1, &resource);
            reapi::varint_field(&mut request, 2, data.len() as u64);
            let before = data.len();
            // One zstd stream per attempt (a fresh decompressor, like
            // requestRead's ZstdDecompressingOutputStream).
            let mut stream = Vec::new();
            let mut result = self.channel.server_streaming(
                &Call {
                    path: &path,
                    metadata: &metadata,
                    timeout: self.timeout(),
                },
                request,
                &mut |message| {
                    for field in reapi::fields(&message).map_err(Status::local)? {
                        if let (10, Value::Bytes(chunk)) = field {
                            if compressed {
                                stream.extend_from_slice(chunk);
                                continue;
                            }
                            if chunk.len() as u64 > digest.size - data.len() as u64 {
                                return Err(too_many());
                            }
                            data.extend_from_slice(chunk);
                        }
                    }
                    Ok(())
                },
            );
            if compressed {
                // What the stream decodes to (all of it, or up to where an
                // interrupted stream ends).
                let (decoded, complete) = zstd_decode(&stream);
                if decoded.len() as u64 > digest.size - data.len() as u64 {
                    return Err(too_many());
                }
                data.extend_from_slice(&decoded);
                if result.is_ok() && !complete && (data.len() as u64) < digest.size {
                    result = Err(Status::local(format!(
                        "Failed to decompress the zstd stream of '{}/{}'",
                        digest.hash, digest.size
                    )));
                }
            }
            match result {
                Ok(()) => break,
                // A fully received blob ignores a late error.
                Err(_) if data.len() as u64 == digest.size => break,
                Err(status) if status.code == Code::NotFound && !status.local => return Ok(None),
                Err(status) if status.code == Code::Unauthenticated && !refreshed => {
                    refreshed = true;
                    if let Some(credentials) = crate::credentials::global() {
                        credentials.refresh();
                    }
                }
                Err(status) => {
                    if data.len() > before {
                        backoff.reset();
                    }
                    if !retriable(&status) {
                        return Err(status);
                    }
                    match backoff.next_delay() {
                        Some(delay) => std::thread::sleep(delay),
                        None => return Err(status),
                    }
                }
            }
        }
        READ_BLOBS.fetch_add(1, Ordering::Relaxed);
        READ_BYTES.fetch_add(data.len() as u64, Ordering::Relaxed);
        if data.len() as u64 != digest.size {
            // OutputDigestMismatchException (verification also catches it).
            return Err(Status::local(format!(
                "Output download failed: Expected digest '{}/{}' does not match received digest of size {}.",
                digest.hash,
                digest.size,
                data.len()
            )));
        }
        Ok(Some(data))
    }

    /// QueryWriteStatus: `(complete, committed_size)`.
    fn query_write_status(&self, resource: &str, action_id: &str) -> Result<(bool, u64), Status> {
        let mut request = Vec::new();
        reapi::string(&mut request, 1, resource);
        let response = self.unary(BYTE_STREAM, "QueryWriteStatus", action_id, &request)?;
        let (mut complete, mut committed) = (false, 0);
        for field in reapi::fields(&response).map_err(Status::local)? {
            match field {
                (1, Value::Varint(value)) => committed = value,
                (2, Value::Varint(value)) => complete = value != 0,
                _ => {}
            }
        }
        Ok((complete, committed))
    }

    /// ByteStream.Write of `INSTANCE/uploads/UUID/blobs/HASH/SIZE` in 16 KiB
    /// requests; a retry resumes at QueryWriteStatus's committed size (from
    /// the start without QueryWriteStatus); a server completing early, or
    /// ALREADY_EXISTS, means the blob exists. Compressed
    /// (`uploads/UUID/compressed-blobs/zstd/HASH/SIZE`): the zstd stream of
    /// the bytes from the resume offset on, its requests' `write_offset`s
    /// counting from that offset (Chunker.seek re-initializes a compressed
    /// stream at the uncompressed offset), and the server's `committed_size`
    /// must be the stream's end or -1 (checkCommittedSize).
    pub fn write_blob(&self, digest: &Digest, bytes: &[u8], action_id: &str) -> Result<(), Status> {
        if digest.size == 0 {
            return Ok(());
        }
        let compressed = self.compressed(digest);
        let resource = format!(
            "{}uploads/{}/{}/{}/{}",
            self.resource_prefix(),
            random_uuid(),
            if compressed {
                "compressed-blobs/zstd"
            } else {
                "blobs"
            },
            digest.hash,
            digest.size
        );
        let path = format!("/{BYTE_STREAM}/Write");
        let mut backoff = ProgressiveBackoff::new(&self.options);
        let mut last_committed: Option<u64> = None;
        let mut refreshed = false;
        loop {
            let attempt = (|| -> Result<bool, Status> {
                let offset = match last_committed {
                    None => 0,
                    Some(_) if !self.query_write_status.load(Ordering::Relaxed) => 0,
                    Some(last) => match self.query_write_status(&resource, action_id) {
                        Ok((true, _)) => return Ok(true),
                        Ok((false, committed)) => {
                            if committed > last {
                                backoff.reset();
                            }
                            committed.min(digest.size)
                        }
                        Err(status) if status.code == Code::Unimplemented => {
                            self.query_write_status.store(false, Ordering::Relaxed);
                            0
                        }
                        Err(status) => return Err(status),
                    },
                };
                last_committed = Some(offset);
                let metadata = self.metadata(BYTE_STREAM, action_id)?;
                // The bytes sent and the write_offset of their first byte.
                let stream;
                let (data, base): (&[u8], usize) = if compressed {
                    stream = zstd_encode(&bytes[offset as usize..]);
                    (&stream, offset as usize)
                } else {
                    (bytes, 0)
                };
                let mut position = if compressed { 0 } else { offset as usize };
                let mut first = true;
                let mut requests = std::iter::from_fn(|| {
                    if !first && position >= data.len() {
                        return None;
                    }
                    let end = (position + CHUNK).min(data.len());
                    let mut request = Vec::new();
                    if first {
                        reapi::string(&mut request, 1, &resource);
                    }
                    first = false;
                    reapi::varint_field(&mut request, 2, (base + position) as u64);
                    reapi::varint_field(&mut request, 3, u64::from(end == data.len()));
                    if end > position {
                        reapi::bytes_field(&mut request, 10, &data[position..end]);
                    }
                    position = end.max(position + 1);
                    Some(request)
                });
                let (response, all_sent) = self.channel.client_streaming(
                    &Call {
                        path: &path,
                        metadata: &metadata,
                        timeout: self.timeout(),
                    },
                    &mut requests,
                )?;
                if !all_sent {
                    return Ok(true);
                }
                let mut committed = 0i64;
                if let Some(response) = response {
                    for field in reapi::fields(&response).map_err(Status::local)? {
                        if let (1, Value::Varint(value)) = field {
                            committed = value as i64;
                        }
                    }
                }
                // checkCommittedSize.
                if compressed {
                    let expected = (base + data.len()) as i64;
                    if committed != expected && committed != -1 {
                        return Err(Status::local(format!(
                            "compressed write incomplete: committed_size {committed} is neither -1 nor total {expected} - {resource}"
                        )));
                    }
                    return Ok(true);
                }
                if committed != digest.size as i64 {
                    return Err(Status::local(format!(
                        "write incomplete: committed_size {committed} for {} total - {resource}",
                        digest.size
                    )));
                }
                Ok(true)
            })();
            match attempt {
                Ok(_) => return Ok(()),
                Err(status) if status.code == Code::AlreadyExists && !status.local => {
                    return Ok(());
                }
                Err(status) if status.code == Code::Unauthenticated && !refreshed => {
                    refreshed = true;
                    if let Some(credentials) = crate::credentials::global() {
                        credentials.refresh();
                    }
                }
                Err(status) => {
                    if !retriable(&status) {
                        return Err(status);
                    }
                    match backoff.next_delay() {
                        Some(delay) => std::thread::sleep(delay),
                        None => return Err(status),
                    }
                }
            }
        }
    }
}

static READ_BLOBS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
static READ_BYTES: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// The blobs and (uncompressed) bytes ByteStream.Read delivered so far.
pub fn read_totals() -> (u64, u64) {
    (
        READ_BLOBS.load(Ordering::Relaxed),
        READ_BYTES.load(Ordering::Relaxed),
    )
}

/// One zstd frame of `bytes` (ZstdCompressingInputStream).
pub fn zstd_encode(bytes: &[u8]) -> Vec<u8> {
    ruzstd::encoding::compress_to_vec(bytes, ruzstd::encoding::CompressionLevel::Fastest)
}

/// The bytes a zstd stream decodes to, and whether it decoded completely
/// (a truncated stream yields what its complete blocks hold).
pub fn zstd_decode(stream: &[u8]) -> (Vec<u8>, bool) {
    use std::io::Read as _;
    let mut out = Vec::new();
    let mut rest = stream;
    while !rest.is_empty() {
        // Skippable frames (magic 0x184D2A50..=0x184D2A5F; bazel-remote
        // stores its blobs behind one) carry no data.
        if rest.len() >= 4 && rest[0] & 0xf0 == 0x50 && rest[1..4] == [0x2a, 0x4d, 0x18] {
            if rest.len() < 8 {
                return (out, false);
            }
            let length = u32::from_le_bytes([rest[4], rest[5], rest[6], rest[7]]) as usize;
            if rest.len() < 8 + length {
                return (out, false);
            }
            rest = &rest[8 + length..];
            continue;
        }
        let mut decoder = match ruzstd::decoding::StreamingDecoder::new(&mut rest) {
            Ok(decoder) => decoder,
            Err(_) => return (out, false),
        };
        let mut buffer = [0u8; 64 * 1024];
        loop {
            match decoder.read(&mut buffer) {
                Ok(0) => break,
                Ok(count) => out.extend_from_slice(&buffer[..count]),
                Err(_) => return (out, false),
            }
        }
    }
    (out, true)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn zstd_streams_round_trip_and_truncate() {
        let bytes: Vec<u8> = (0..200_000u32).map(|i| (i % 251) as u8).collect();
        let stream = zstd_encode(&bytes);
        assert!(stream.len() < bytes.len());
        assert_eq!(zstd_decode(&stream), (bytes.clone(), true));
        let (partial, complete) = zstd_decode(&stream[..stream.len() / 2]);
        assert!(!complete);
        assert!(bytes.starts_with(&partial));
        // A skippable frame first (bazel-remote's stored blobs), then
        // two frames.
        let mut two = vec![0x50, 0x2a, 0x4d, 0x18, 3, 0, 0, 0, 1, 2, 3];
        two.extend(zstd_encode(&bytes[..10]));
        two.extend(zstd_encode(&bytes[10..20]));
        assert_eq!(zstd_decode(&two), (bytes[..20].to_vec(), true));
    }

    fn version(major: u64, minor: u64) -> Option<ApiVersion> {
        Some(ApiVersion::new(major, minor))
    }

    #[test]
    fn compatibility_follows_remote_server_capabilities() {
        let good = ServerCapabilities {
            digest_functions: vec![SHA256],
            update_enabled: true,
            compressors: Vec::new(),
            exec_enabled: false,
            exec_digest_function: 0,
            exec_digest_functions: Vec::new(),
            deprecated: None,
            low: version(2, 0),
            high: version(2, 3),
        };
        let result = check_compatibility(&good, true);
        assert_eq!(result.errors, Vec::<String>::new());
        assert_eq!(result.warnings, Vec::<String>::new());
        assert!(result.output_paths);
        let old = ServerCapabilities {
            high: version(2, 0),
            update_enabled: false,
            ..good.clone()
        };
        let result = check_compatibility(&old, true);
        assert!(!result.output_paths);
        assert_eq!(
            result.warnings,
            [
                "--remote_upload_local_results is set, but the remote cache does not support uploading action results or the current account is not authorized to write local results to the remote cache."
            ]
        );
        assert!(check_compatibility(&old, false).warnings.is_empty());
        let wrong = ServerCapabilities {
            digest_functions: vec![2, 3],
            low: None,
            high: None,
            ..good
        };
        assert_eq!(
            check_compatibility(&wrong, false).errors,
            [
                "The client supported API versions, 2.0 to 2.11, is not supported by the server, 0.0 to 0.0. Please switch to a different server or upgrade Bazel.",
                "Cannot use hash function SHA256 with remote cache. Server supported functions are: [SHA1, MD5]"
            ]
        );
    }

    #[test]
    fn execution_compatibility_follows_remote_server_capabilities() {
        let caps = ServerCapabilities {
            digest_functions: vec![SHA256],
            update_enabled: true,
            compressors: Vec::new(),
            exec_enabled: false,
            exec_digest_function: 0,
            exec_digest_functions: Vec::new(),
            deprecated: None,
            low: version(2, 0),
            high: version(2, 3),
        };
        let result = check_requirement(&caps, true, false, Requirement::ExecutionAndCache);
        assert_eq!(
            result.errors,
            [
                "Remote execution is not supported by the remote server, or the current account is not authorized to use remote execution."
            ]
        );
        // A cache-only check ignores the execution capabilities.
        assert!(
            check_requirement(&caps, true, false, Requirement::Cache)
                .errors
                .is_empty()
        );
        let enabled = ServerCapabilities {
            exec_enabled: true,
            exec_digest_function: 2,
            ..caps.clone()
        };
        assert_eq!(
            check_requirement(&enabled, true, false, Requirement::Execution).errors,
            [
                "Cannot use hash function SHA256 with remote execution. Server supported function is SHA1"
            ]
        );
        let listed = ServerCapabilities {
            exec_enabled: true,
            exec_digest_functions: vec![3],
            digest_functions: vec![],
            ..caps.clone()
        };
        assert_eq!(
            check_requirement(&listed, true, false, Requirement::ExecutionAndCache).errors,
            [
                "Cannot use hash function SHA256 with remote execution. Server supported functions are: [MD5]",
                "Cannot use hash function SHA256 with remote cache. Server supported functions are: []"
            ]
        );
        let good = ServerCapabilities {
            exec_enabled: true,
            exec_digest_functions: vec![SHA256],
            ..caps
        };
        let result = check_requirement(&good, true, false, Requirement::ExecutionAndCache);
        assert!(result.errors.is_empty() && result.warnings.is_empty());
    }

    #[test]
    fn capabilities_decode_packed_and_unpacked_enums() {
        let mut cache = Vec::new();
        reapi::bytes_field(&mut cache, 1, &[1, 2]);
        reapi::varint_field(&mut cache, 1, 9);
        let mut update = Vec::new();
        reapi::varint_field(&mut update, 1, 1);
        reapi::message(&mut cache, 2, &update);
        let mut high = Vec::new();
        reapi::varint_field(&mut high, 1, 2);
        reapi::varint_field(&mut high, 2, 11);
        reapi::varint_field(&mut high, 3, 1);
        let mut out = Vec::new();
        reapi::message(&mut out, 1, &cache);
        reapi::message(&mut out, 5, &high);
        let decoded = ServerCapabilities::decode(&out).unwrap();
        assert_eq!(decoded.digest_functions, [1, 2, 9]);
        assert!(decoded.update_enabled);
        assert_eq!(decoded.high.unwrap().to_string(), "2.11.1");
    }

    #[test]
    fn backoff_and_uuids() {
        let options = GrpcOptions {
            instance_name: String::new(),
            timeout: Duration::from_secs(60),
            retries: 2,
            retry_max_delay: Duration::from_millis(150),
            headers: Vec::new(),
            limits: Limits {
                max_connections: 100,
                max_concurrency_per_connection: 100,
                connect_timeout: Duration::from_secs(60),
            },
            tls: TlsOptions::default(),
            upload_local_results: true,
            requirement: Requirement::Cache,
            compression: false,
            compression_threshold: 100,
        };
        let mut backoff = Backoff::new(&options);
        let first = backoff.next_delay().unwrap();
        assert!((90..=110).contains(&first.as_millis()), "{first:?}");
        let second = backoff.next_delay().unwrap();
        assert!((135..=165).contains(&second.as_millis()), "{second:?}");
        assert_eq!(backoff.next_delay(), None);
        let mut progressive = ProgressiveBackoff::new(&options);
        progressive.reset();
        assert_eq!(progressive.next_delay(), Some(Duration::ZERO));
        assert!(progressive.next_delay().unwrap() > Duration::ZERO);
        let uuid = random_uuid();
        assert_eq!(uuid.len(), 36);
        assert_eq!(&uuid[14..15], "4");
        assert_ne!(uuid, random_uuid());
    }
}
