//! Bazel 9.3's HTTP and gRPC remote caches (N41, docs/REMOTE_CACHE.md).
//!
//! Pinned source (`src/main/java/com/google/devtools/build/lib/remote/`):
//! `RemoteModule.java:429-640, 1458-1490` (setup and the http:// credential
//! check), `CombinedCacheClientFactory.java:77-150`, `http/HttpCacheClient.java`
//! and `http/Http{Download,Upload}Handler.java`/`AbstractHttpHandler.java`
//! (the `/ac/` and `/cas/` protocol), `RemoteSpawnCache.java:85-305` (lookup,
//! fallback and store), `RemoteExecutionService.java:269-591` (the action key)
//! and `UploadManifest.java` (the uploaded blobs and ActionResult).
//!
//! The action key is computed exactly as Bazel computes it
//! (crate::reapi), so a native build and genuine Bazel share entries when
//! their spawns agree (same argv, environment, inputs and output paths).
//! The gRPC protocol is crate::remote_grpc; remote execution
//! (`--remote_executor`) is crate::remote_execution over the same endpoint
//! type (docs/REMOTE_EXECUTION.md).

use crate::credentials::{Credentials, HelperError};
pub use crate::grpc_channel::TlsOptions;
use crate::reapi::{self, Digest};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// The modelled `RemoteOptions` (remote/options/RemoteOptions.java).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RemoteOptions {
    /// `--remote_cache` (old name `--remote_http_cache`).
    pub cache: String,
    /// `--remote_executor`.
    pub executor: String,
    pub accept_cached: bool,
    pub upload_local_results: bool,
    /// Not part of HTTP cache requests (HttpCacheClient ignores it).
    pub instance_name: String,
    pub timeout: Duration,
    pub headers: Vec<(String, String)>,
    pub cache_headers: Vec<(String, String)>,
    /// `--remote_retries` (gRPC; HTTP requests are not retried).
    pub retries: i32,
    pub retry_max_delay: Duration,
    pub max_connections: i32,
    pub max_concurrency_per_connection: i32,
    /// `--remote_download_outputs` (Build without the Bytes,
    /// crate::remote_outputs).
    pub download_outputs: DownloadOutputs,
    /// `--remote_download_regex` (old `--experimental_remote_download_regex`).
    pub download_regex: Vec<String>,
    /// `--experimental_remote_cache_ttl` (default 3h).
    pub cache_ttl: Duration,
    /// `--experimental_remote_cache_eviction_retries` (default 5).
    pub eviction_retries: i32,
    /// `--tls_certificate`, `--tls_client_certificate`, `--tls_client_key`,
    /// `--tls_authority_override` (AuthAndTLSOptions).
    pub tls: TlsOptions,
    /// `--remote_exec_header` (RemoteOptions.java:205-217).
    pub exec_headers: Vec<(String, String)>,
    /// `--remote_default_exec_properties` (RemoteOptions.java:600-610).
    pub default_exec_properties: Vec<(String, String)>,
    /// `--remote_local_fallback` (RemoteOptions.java:306-313).
    pub local_fallback: bool,
    /// `--remote_print_execution_messages` (default `failure`).
    pub print_execution_messages: PrintExecutionMessages,
    /// `--remote_cache_compression` (gRPC zstd ByteStream).
    pub cache_compression: bool,
    /// `--experimental_remote_cache_compression_threshold` (default 100).
    pub cache_compression_threshold: i32,
}

/// RemoteOptions.ExecutionMessagePrintMode.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PrintExecutionMessages {
    Failure,
    Success,
    All,
}

impl PrintExecutionMessages {
    pub fn should_print(self, success: bool) -> bool {
        match self {
            PrintExecutionMessages::Failure => !success,
            PrintExecutionMessages::Success => success,
            PrintExecutionMessages::All => true,
        }
    }
}

/// RemoteOutputsMode.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DownloadOutputs {
    All,
    Minimal,
    Toplevel,
}

impl Default for RemoteOptions {
    fn default() -> Self {
        Self {
            cache: String::new(),
            executor: String::new(),
            accept_cached: true,
            upload_local_results: true,
            instance_name: String::new(),
            // defaultValue = "60s" (RemoteOptions.java:234-236).
            timeout: Duration::from_secs(60),
            headers: Vec::new(),
            cache_headers: Vec::new(),
            retries: 5,
            retry_max_delay: Duration::from_secs(5),
            max_connections: 100,
            max_concurrency_per_connection: 100,
            download_outputs: DownloadOutputs::Toplevel,
            tls: TlsOptions::default(),
            exec_headers: Vec::new(),
            default_exec_properties: Vec::new(),
            local_fallback: false,
            print_execution_messages: PrintExecutionMessages::Failure,
            cache_compression: false,
            cache_compression_threshold: 100,
            download_regex: Vec::new(),
            cache_ttl: Duration::from_secs(3 * 3600),
            eviction_retries: 5,
        }
    }
}

/// CommonRemoteOptions.RemoteDurationConverter (:61-77): unitless values
/// are seconds.
fn remote_duration(option: &str, value: &str) -> Result<Duration, String> {
    let input = if !value.is_empty() && value.bytes().all(|b| b.is_ascii_digit()) {
        format!("{value}s")
    } else {
        value.to_owned()
    };
    crate::credentials::parse_duration(&input)
        .map_err(|error| format!("While parsing option --{option}={value}: {error}"))
}

/// Converters.IntegerConverter.
fn integer(option: &str, value: &str) -> Result<i32, String> {
    value
        .parse::<i32>()
        .map_err(|_| format!("While parsing option --{option}={value}: '{value}' is not an int"))
}

fn value_option<'a>(
    arg: &'a str,
    next: Option<&'a str>,
    names: &[&str],
) -> Option<Result<(&'a str, bool), String>> {
    let body = arg.strip_prefix("--")?;
    for name in names {
        if body == *name {
            return Some(
                next.map(|value| (value, true))
                    .ok_or_else(|| format!("Expected value after --{name}")),
            );
        }
        if let Some(value) = body.strip_prefix(name).and_then(|v| v.strip_prefix('=')) {
            return Some(Ok((value, false)));
        }
    }
    None
}

/// Converters.AssignmentConverter (common/options/Converters.java:483-501).
fn assignment(option: &str, input: &str) -> Result<(String, String), String> {
    match input.find('=') {
        Some(position) if position > 0 => Ok((
            input[..position].to_owned(),
            input[position + 1..].to_owned(),
        )),
        _ => Err(format!(
            "While parsing option --{option}={input}: Variable definitions must be in the form of a 'name=value' assignment"
        )),
    }
}

impl RemoteOptions {
    /// One modelled option; `None` if `arg` is not one of them. The result
    /// says whether the separate value `next` was consumed.
    pub fn parse(&mut self, arg: &str, next: Option<&str>) -> Option<Result<bool, String>> {
        for (name, target) in [
            ("remote_accept_cached", &mut self.accept_cached),
            (
                "remote_upload_local_results",
                &mut self.upload_local_results,
            ),
            ("remote_local_fallback", &mut self.local_fallback),
        ] {
            if let Some(value) = crate::configuration::boolean_option(arg, name) {
                return Some(value.map(|value| {
                    *target = value;
                    false
                }));
            }
        }
        if let Some(found) = value_option(arg, next, &["remote_cache", "remote_http_cache"]) {
            return Some(found.map(|(value, used)| {
                self.cache = value.to_owned();
                used
            }));
        }
        if let Some(found) = value_option(arg, next, &["remote_executor"]) {
            return Some(found.map(|(value, used)| {
                self.executor = value.to_owned();
                used
            }));
        }
        if let Some(found) = value_option(arg, next, &["remote_instance_name"]) {
            return Some(found.map(|(value, used)| {
                self.instance_name = value.to_owned();
                used
            }));
        }
        if let Some(found) = value_option(arg, next, &["remote_timeout"]) {
            return Some(found.and_then(|(value, used)| {
                self.timeout = remote_duration("remote_timeout", value)?;
                Ok(used)
            }));
        }
        if let Some(found) = value_option(
            arg,
            next,
            &[
                "remote_download_regex",
                "experimental_remote_download_regex",
            ],
        ) {
            return Some(found.and_then(|(value, used)| {
                crate::remote_outputs::pattern("remote_download_regex", value)?;
                self.download_regex.push(value.to_owned());
                Ok(used)
            }));
        }
        if let Some(found) = value_option(arg, next, &["experimental_remote_cache_ttl"]) {
            return Some(found.and_then(|(value, used)| {
                self.cache_ttl = remote_duration("experimental_remote_cache_ttl", value)?;
                Ok(used)
            }));
        }
        if let Some(found) = value_option(arg, next, &["remote_retry_max_delay"]) {
            return Some(found.and_then(|(value, used)| {
                self.retry_max_delay = remote_duration("remote_retry_max_delay", value)?;
                Ok(used)
            }));
        }
        for (names, target) in [
            (
                &["remote_retries", "experimental_remote_retry_max_attempts"][..],
                &mut self.retries,
            ),
            (&["remote_max_connections"][..], &mut self.max_connections),
            (
                &["experimental_remote_cache_eviction_retries"][..],
                &mut self.eviction_retries,
            ),
            (
                &["experimental_remote_cache_compression_threshold"][..],
                &mut self.cache_compression_threshold,
            ),
            (
                &["remote_max_concurrency_per_connection"][..],
                &mut self.max_concurrency_per_connection,
            ),
        ] {
            if let Some(found) = value_option(arg, next, names) {
                return Some(found.and_then(|(value, used)| {
                    *target = integer(names[0], value)?;
                    Ok(used)
                }));
            }
        }
        if let Some(found) = value_option(
            arg,
            next,
            &[
                "remote_download_outputs",
                "experimental_remote_download_outputs",
            ],
        ) {
            return Some(found.and_then(|(value, used)| {
                self.download_outputs = match value.to_ascii_lowercase().as_str() {
                    "all" => DownloadOutputs::All,
                    "minimal" => DownloadOutputs::Minimal,
                    "toplevel" => DownloadOutputs::Toplevel,
                    _ => {
                        return Err(format!(
                            "While parsing option --remote_download_outputs={value}: Not a valid download remote outputs: '{value}' (should be all, minimal or toplevel)"
                        ));
                    }
                };
                Ok(used)
            }));
        }
        // Expansion options (RemoteOptions.java:542-578).
        for (names, mode) in [
            (
                [
                    "remote_download_minimal",
                    "experimental_remote_download_minimal",
                ],
                DownloadOutputs::Minimal,
            ),
            (
                [
                    "remote_download_toplevel",
                    "experimental_remote_download_toplevel",
                ],
                DownloadOutputs::Toplevel,
            ),
            (
                ["remote_download_all", "remote_download_all"],
                DownloadOutputs::All,
            ),
        ] {
            if let Some(name) = arg.strip_prefix("--")
                && names.contains(&name)
            {
                self.download_outputs = mode;
                return Some(Ok(false));
            }
        }
        for name in [
            "remote_cache_compression",
            "experimental_remote_cache_compression",
        ] {
            if let Some(value) = crate::configuration::boolean_option(arg, name) {
                return Some(value.map(|value| {
                    self.cache_compression = value;
                    false
                }));
            }
        }
        for (name, target) in [
            ("tls_certificate", &mut self.tls.certificate),
            ("tls_client_certificate", &mut self.tls.client_certificate),
            ("tls_client_key", &mut self.tls.client_key),
        ] {
            if let Some(found) = value_option(arg, next, &[name]) {
                return Some(found.map(|(value, used)| {
                    *target = (!value.is_empty()).then(|| PathBuf::from(value));
                    used
                }));
            }
        }
        if let Some(found) = value_option(arg, next, &["tls_authority_override"]) {
            return Some(found.map(|(value, used)| {
                self.tls.authority_override = (!value.is_empty()).then(|| value.to_owned());
                used
            }));
        }
        if let Some(found) = value_option(arg, next, &["remote_print_execution_messages"]) {
            return Some(found.and_then(|(value, used)| {
                self.print_execution_messages = match value.to_ascii_lowercase().as_str() {
                    "failure" => PrintExecutionMessages::Failure,
                    "success" => PrintExecutionMessages::Success,
                    "all" => PrintExecutionMessages::All,
                    _ => {
                        return Err(format!(
                            "While parsing option --remote_print_execution_messages={value}: Not a valid execution message print mode: '{value}' (should be failure, success or all)"
                        ));
                    }
                };
                Ok(used)
            }));
        }
        // `--remote_local_fallback_strategy` (deprecated, default `local`):
        // the standalone strategy is the only local fallback modelled.
        if let Some(found) = value_option(arg, next, &["remote_local_fallback_strategy"]) {
            return Some(found.and_then(|(value, used)| {
                if matches!(value, "local" | "standalone") {
                    Ok(used)
                } else {
                    Err(format!(
                        "unsupported option --remote_local_fallback_strategy={value}: only the default local fallback strategy (local/standalone) is modelled by the native profile"
                    ))
                }
            }));
        }
        // Priorities are server-dependent ExecuteRequest fields with a
        // capability range check; only the default 0 is modelled.
        for name in ["remote_execution_priority", "remote_result_cache_priority"] {
            if let Some(found) = value_option(arg, next, &[name]) {
                return Some(found.and_then(|(value, used)| {
                    if integer(name, value)? == 0 {
                        Ok(used)
                    } else {
                        Err(format!(
                            "unsupported option --{name}={value}: only the default priority 0 is modelled by the native profile"
                        ))
                    }
                }));
            }
        }
        for (name, list) in [
            ("remote_header", &mut self.headers),
            ("remote_cache_header", &mut self.cache_headers),
            ("remote_exec_header", &mut self.exec_headers),
            (
                "remote_default_exec_properties",
                &mut self.default_exec_properties,
            ),
        ] {
            if let Some(found) = value_option(arg, next, &[name]) {
                return Some(found.and_then(|(value, used)| {
                    list.push(assignment(name, value)?);
                    Ok(used)
                }));
            }
        }
        None
    }
}

/// The cache base URI as HttpCacheClient stores it: an absent port becomes
/// the scheme's default (HttpCacheClient.java:240-253), so credentials are
/// requested for `https://host:443`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CacheUri {
    pub scheme: String,
    pub user_info: Option<String>,
    pub host: String,
    pub port: u16,
    pub path: String,
}

impl CacheUri {
    /// java.net.URI.create for the `http(s)://[userinfo@]host[:port][/path][?q][#f]` subset.
    pub fn parse(text: &str) -> Result<Self, String> {
        let invalid = |detail: &str| format!("invalid --remote_cache URI '{text}': {detail}");
        let (scheme, rest) = text.split_once("://").ok_or_else(|| invalid("no scheme"))?;
        let scheme = scheme.to_ascii_lowercase();
        let end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
        let (authority, tail) = rest.split_at(end);
        let path = tail.split(['?', '#']).next().unwrap_or("").to_owned();
        let (user_info, host_port) = match authority.rsplit_once('@') {
            Some((user, host)) => (Some(user.to_owned()), host),
            None => (None, authority),
        };
        let (host, port) = if host_port.starts_with('[') {
            let close = host_port
                .find(']')
                .ok_or_else(|| invalid("unterminated IPv6 host"))?;
            (&host_port[..=close], &host_port[close + 1..])
        } else {
            match host_port.find(':') {
                Some(colon) => (&host_port[..colon], &host_port[colon..]),
                None => (host_port, ""),
            }
        };
        if host.is_empty() {
            return Err(invalid("no host"));
        }
        let port = match port.strip_prefix(':') {
            None if port.is_empty() => {
                if scheme == "https" {
                    443
                } else {
                    80
                }
            }
            Some(digits) if !digits.is_empty() && digits.bytes().all(|b| b.is_ascii_digit()) => {
                digits.parse().map_err(|_| invalid("port out of range"))?
            }
            _ => return Err(invalid("malformed port")),
        };
        Ok(Self {
            scheme,
            user_info,
            host: host.to_owned(),
            port,
            path,
        })
    }

    /// The URI string credentials are requested for (`URI.toString`).
    pub fn credential_uri(&self, original: &str) -> String {
        let query_fragment = original
            .find(['?', '#'])
            .filter(|index| *index > original.find("://").map_or(0, |i| i + 3))
            .map(|index| &original[index..])
            .unwrap_or("");
        let user = self
            .user_info
            .as_ref()
            .map(|user| format!("{user}@"))
            .unwrap_or_default();
        let scheme = original.split_once("://").map_or("", |(scheme, _)| scheme);
        format!(
            "{scheme}://{user}{}:{}{}{query_fragment}",
            self.host, self.port, self.path
        )
    }

    /// AbstractHttpHandler.constructPath (:95-104).
    fn request_path(&self, cas: bool, hash: &str) -> String {
        let mut path = self.path.clone();
        if !path.ends_with('/') {
            path.push('/');
        }
        path.push_str(if cas { "cas/" } else { "ac/" });
        path.push_str(hash);
        path
    }
}

/// A remote cache failure. Credential helper failures fail the action
/// (RemoteSpawnCache.java:185-190); others are `Remote Cache:` warnings and
/// the action runs locally.
#[derive(Debug)]
pub enum CacheError {
    Credentials(HelperError),
    /// The gRPC capability check failed: every action using the cache
    /// fails with this message (RemoteSpawnCache.java:189-197 without
    /// `--remote_local_fallback`).
    Capabilities(String),
    Other(String),
}

impl std::fmt::Display for CacheError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CacheError::Credentials(error) => write!(f, "{error}"),
            CacheError::Capabilities(error) | CacheError::Other(error) => f.write_str(error),
        }
    }
}

/// The HTTP protocol (HttpCacheClient).
struct HttpCache {
    uri: CacheUri,
    credential_uri: String,
    client: reqwest::blocking::Client,
    headers: Vec<(String, String)>,
}

enum Backend {
    Http(HttpCache),
    Grpc(Arc<crate::remote_grpc::GrpcCache>),
}

impl From<crate::grpc_channel::Status> for CacheError {
    fn from(status: crate::grpc_channel::Status) -> Self {
        CacheError::Other(status.to_string())
    }
}

/// The configured remote cache (and remote executor) of this invocation.
pub struct RemoteCache {
    backend: Backend,
    /// The `--remote_executor` endpoint (the cache's own when both options
    /// name one target).
    executor: Option<Arc<crate::remote_grpc::GrpcCache>>,
    pub accept_cached: bool,
    pub upload_local_results: bool,
    /// `--remote_local_fallback`.
    pub local_fallback: bool,
    /// `--remote_default_exec_properties` in flag order.
    pub default_exec_properties: Vec<(String, String)>,
    pub print_execution_messages: PrintExecutionMessages,
    uploaded: Mutex<HashSet<Digest>>,
    /// Blobs known to be in the CAS (RemoteExecutionCache's findMissingCache:
    /// reported present or uploaded in this invocation): not asked again.
    present: Mutex<HashSet<Digest>>,
    /// In-flight uploads: concurrent uploads of one blob run once
    /// (CombinedCache/RemoteCacheClient upload deduplication).
    uploading: Mutex<HashMap<Digest, Arc<Mutex<()>>>>,
    hits: AtomicUsize,
    /// Spawns executed remotely (ExecuteResponse without `cached_result`).
    executions: AtomicUsize,
}

impl std::fmt::Debug for RemoteCache {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match &self.backend {
            Backend::Http(http) => f
                .debug_struct("RemoteCache")
                .field("uri", &http.credential_uri)
                .finish(),
            Backend::Grpc(grpc) => f.debug_struct("RemoteCache").field("grpc", grpc).finish(),
        }
    }
}

/// A setup failure: the error lines Bazel prints, then its exit message.
#[derive(Debug, PartialEq, Eq)]
pub struct SetupError {
    pub errors: Vec<String>,
    pub exit: String,
}

fn netrc_file() -> Option<PathBuf> {
    // GoogleAuthUtils.newCredentialsFromNetrc (:393-414).
    let path = match std::env::var_os("NETRC") {
        Some(path) => PathBuf::from(path),
        None => PathBuf::from(std::env::var_os("HOME")?).join(".netrc"),
    };
    path.exists().then_some(path)
}

/// RemoteModule.setup for the modelled subset. `Ok(None)` without a cache.
/// Prints the http:// plaintext-credentials warning like
/// RemoteModule.createCredentials (:1458-1490), which also invokes the
/// helper for the cache URI once at startup.
pub fn setup(
    options: &RemoteOptions,
    credentials: Option<&Credentials>,
) -> Result<Option<RemoteCache>, SetupError> {
    let fail = |error: String| SetupError {
        errors: Vec::new(),
        exit: error,
    };
    // RemoteModule.setup: an empty --remote_cache reuses --remote_executor.
    let executor_target = options.executor.as_str();
    let cache_target = if options.cache.is_empty() {
        executor_target
    } else {
        options.cache.as_str()
    };
    if cache_target.is_empty() {
        return Ok(None);
    }
    let lower = cache_target.to_ascii_lowercase();
    let http = lower.starts_with("http://") || lower.starts_with("https://");
    if http && !executor_target.is_empty() {
        return Err(fail(
            "Cannot combine gRPC based remote execution with HTTP-based caching".into(),
        ));
    }
    // Intentional safety divergence (docs/REMOTE_CACHE.md): until native
    // execution parity with Bazel is established, native results are written
    // to a shared cache only on explicit opt-in. Reads stay Bazel-like.
    let upload_local_results = options.upload_local_results && upload_opt_in();
    let warn_upload = || {
        if options.upload_local_results && !upload_local_results {
            crate::note!(
                "WARNING: native remote cache uploads are disabled by default (a safety divergence from Bazel's --remote_upload_local_results); set {UPLOAD_OPT_IN}=1 to upload local results"
            );
        }
    };
    if !http {
        // GrpcCacheClient.isRemoteCacheOptions: anything else is gRPC.
        let endpoint = |target: &str,
                        option: &str,
                        headers: Vec<(String, String)>,
                        requirement: crate::remote_grpc::Requirement|
         -> Result<Arc<crate::remote_grpc::GrpcCache>, SetupError> {
            let grpc = crate::remote_grpc::GrpcCache::new(
                target,
                crate::remote_grpc::GrpcOptions {
                    instance_name: options.instance_name.clone(),
                    timeout: options.timeout,
                    retries: options.retries.max(0) as u32,
                    retry_max_delay: options.retry_max_delay,
                    headers,
                    limits: crate::grpc_channel::Limits {
                        max_connections: options.max_connections.max(0) as usize,
                        max_concurrency_per_connection: options
                            .max_concurrency_per_connection
                            .max(0)
                            as usize,
                        connect_timeout: options.timeout,
                    },
                    tls: options.tls.clone(),
                    upload_local_results: options.upload_local_results,
                    requirement,
                    compression: options.cache_compression,
                    compression_threshold: options.cache_compression_threshold.max(0) as u64,
                },
            )
            .map_err(|error| fail(format!("--{option}={target}: {error}")))?;
            // GoogleAuthUtils.newCredentials falls back to .netrc for gRPC
            // services too.
            let probe = grpc.credential_uri("build.bazel.remote.execution.v2.Capabilities");
            if credentials.is_none_or(|credentials| credentials.helper_for(&probe).is_none())
                && let Some(netrc) = netrc_file()
            {
                return Err(fail(format!(
                    "the remote cache {target} has no matching --credential_helper and Bazel would fall back to the .netrc file {}; .netrc credentials are not implemented by the native profile",
                    netrc.display()
                )));
            }
            Ok(Arc::new(grpc))
        };
        // newCacheHeadersInterceptor / newExecHeadersInterceptor
        // (TracingMetadataUtils.java:139-155): one endpoint for both
        // options carries the execution headers.
        let with = |extra: &[(String, String)]| -> Vec<(String, String)> {
            options.headers.iter().chain(extra).cloned().collect()
        };
        use crate::remote_grpc::Requirement;
        let (cache, executor) = if executor_target.is_empty() {
            (
                endpoint(
                    cache_target,
                    "remote_cache",
                    with(&options.cache_headers),
                    Requirement::Cache,
                )?,
                None,
            )
        } else if cache_target == executor_target {
            let shared = endpoint(
                executor_target,
                "remote_executor",
                with(&options.exec_headers),
                Requirement::ExecutionAndCache,
            )?;
            (shared.clone(), Some(shared))
        } else {
            let executor = endpoint(
                executor_target,
                "remote_executor",
                with(&options.exec_headers),
                Requirement::Execution,
            )?;
            let cache = endpoint(
                cache_target,
                "remote_cache",
                with(&options.cache_headers),
                Requirement::Cache,
            )?;
            (cache, Some(executor))
        };
        warn_upload();
        return Ok(Some(RemoteCache {
            backend: Backend::Grpc(cache),
            executor,
            accept_cached: options.accept_cached,
            upload_local_results,
            local_fallback: options.local_fallback,
            default_exec_properties: options.default_exec_properties.clone(),
            print_execution_messages: options.print_execution_messages,
            uploaded: Mutex::new(HashSet::new()),
            uploading: Mutex::new(HashMap::new()),
            present: Mutex::new(HashSet::new()),
            hits: AtomicUsize::new(0),
            executions: AtomicUsize::new(0),
        }));
    }
    let uri = CacheUri::parse(&options.cache).map_err(fail)?;
    let credential_uri = uri.credential_uri(&options.cache);
    if lower.starts_with("http://")
        && let Some(credentials) = credentials
    {
        // createCredentials: `new URI(remoteCache)`, the original string.
        match credentials.headers(&options.cache) {
            Ok(Some(headers)) if !headers.is_empty() => crate::note!(
                "WARNING: Credentials are transmitted in plaintext to {}. Please consider using an HTTPS endpoint.",
                options.cache
            ),
            Ok(_) => {}
            Err(error) => {
                return Err(SetupError {
                    errors: vec![error.0],
                    exit: "Error initializing RemoteModule".into(),
                });
            }
        }
    }
    if uri.user_info.is_none()
        && credentials.is_none_or(|credentials| credentials.helper_for(&credential_uri).is_none())
        && let Some(netrc) = netrc_file()
    {
        return Err(fail(format!(
            "the remote cache {} has no matching --credential_helper and Bazel would fall back to the .netrc file {}; .netrc credentials are not implemented by the native profile",
            options.cache,
            netrc.display()
        )));
    }
    let mut builder = reqwest::blocking::Client::builder()
        .use_rustls_tls()
        .no_proxy()
        .http1_only()
        .referer(false)
        .redirect(reqwest::redirect::Policy::none())
        .user_agent(concat!("bazel-rs/", env!("CARGO_PKG_VERSION")));
    if !options.timeout.is_zero() {
        builder = builder
            .timeout(options.timeout)
            .connect_timeout(options.timeout);
    }
    // AuthAndTLSOptions for an https:// cache (HttpCacheClient's SslContext).
    if let Some(path) = &options.tls.certificate {
        let pem = std::fs::read(path).map_err(|error| {
            fail(format!(
                "Failed to init TLS infrastructure using '{}' as root certificate: {error}",
                path.display()
            ))
        })?;
        let certificates = reqwest::Certificate::from_pem_bundle(&pem).map_err(|error| {
            fail(format!(
                "Failed to init TLS infrastructure using '{}' as root certificate: {error}",
                path.display()
            ))
        })?;
        builder = builder.tls_built_in_root_certs(false);
        for certificate in certificates {
            builder = builder.add_root_certificate(certificate);
        }
    }
    if let (Some(certificate), Some(key)) =
        (&options.tls.client_certificate, &options.tls.client_key)
    {
        let failed = |error: String| {
            fail(format!(
                "Failed to init TLS infrastructure using '{}' as client certificate: {error}",
                certificate.display()
            ))
        };
        let mut pem = std::fs::read(certificate).map_err(|error| failed(error.to_string()))?;
        pem.push(b'\n');
        pem.extend(std::fs::read(key).map_err(|error| failed(error.to_string()))?);
        builder = builder.identity(
            reqwest::Identity::from_pem(&pem).map_err(|error| failed(error.to_string()))?,
        );
    }
    let client = builder
        .build()
        .map_err(|_| fail("cannot initialize the remote cache HTTP client".into()))?;
    // CombinedCacheClientFactory.effectiveHeaders (:144-150).
    let headers = options
        .headers
        .iter()
        .chain(&options.cache_headers)
        .cloned()
        .collect();
    warn_upload();
    Ok(Some(RemoteCache {
        backend: Backend::Http(HttpCache {
            uri,
            credential_uri,
            client,
            headers,
        }),
        executor: None,
        accept_cached: options.accept_cached,
        upload_local_results,
        local_fallback: options.local_fallback,
        default_exec_properties: options.default_exec_properties.clone(),
        print_execution_messages: options.print_execution_messages,
        uploaded: Mutex::new(HashSet::new()),
        uploading: Mutex::new(HashMap::new()),
        present: Mutex::new(HashSet::new()),
        hits: AtomicUsize::new(0),
        executions: AtomicUsize::new(0),
    }))
}

/// The environment variable that opts in to remote cache uploads.
pub const UPLOAD_OPT_IN: &str = "BAZEL_RS_ALLOW_REMOTE_UPLOAD";

fn upload_opt_in() -> bool {
    std::env::var_os(UPLOAD_OPT_IN).is_some_and(|value| value == "1")
}

static ACTIVE: OnceLock<RemoteCache> = OnceLock::new();

/// Installs the invocation's cache (once, from main) and starts the gRPC
/// endpoints' capability checks in the background, like RemoteModule's
/// eager `channel.withChannelFuture` (RemoteModule.java:1011-1017): the
/// warnings print when the server answers, and a failure is reported by
/// the first spawn that needs the endpoint (`Failed to query remote
/// execution capabilities: …`).
pub fn install(cache: RemoteCache) {
    let _ = ACTIVE.set(cache);
    let Some(cache) = active() else {
        return;
    };
    let mut endpoints: Vec<&'static crate::remote_grpc::GrpcCache> = Vec::new();
    if let Backend::Grpc(grpc) = &cache.backend {
        endpoints.push(grpc);
    }
    if let Some(executor) = &cache.executor
        && !endpoints
            .iter()
            .any(|endpoint| std::ptr::eq(*endpoint, &**executor))
    {
        endpoints.push(executor);
    }
    for endpoint in endpoints {
        let _ = std::thread::Builder::new()
            .name("bazel-rs-capabilities".into())
            .spawn(move || {
                let _ = endpoint.capabilities();
            });
    }
}

/// Set when a spawn failed for a remote reason (ExitCode.REMOTE_ERROR: a
/// `(Exit 34)` remote execution failure, a capability check failure, a
/// lost remote input or a failed top-level output download): the command
/// then exits with 34 instead of 1, like Bazel's DetailedExitCode of
/// RemoteExecution codes (failure_details.proto: CAPABILITIES_QUERY_FAILURE,
/// TOPLEVEL_OUTPUTS_DOWNLOAD_FAILURE, …). A typed flag, never the error
/// text.
static REMOTE_FAILURE: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Records a remote failure of a spawn (see [`remote_failure_occurred`]).
pub fn note_remote_failure() {
    REMOTE_FAILURE.store(true, Ordering::Relaxed);
}

/// Whether a spawn failed for a remote reason in this invocation.
pub fn remote_failure_occurred() -> bool {
    REMOTE_FAILURE.load(Ordering::Relaxed)
}

/// The invocation's cache, if one is configured.
pub fn active() -> Option<&'static RemoteCache> {
    ACTIVE.get()
}

fn decode_body(encoding: Option<&str>, body: Vec<u8>) -> Result<Vec<u8>, String> {
    let mut out = Vec::new();
    match encoding.map(|e| e.trim().to_ascii_lowercase()).as_deref() {
        None | Some("" | "identity") => return Ok(body),
        Some("gzip" | "x-gzip") => flate2::read::MultiGzDecoder::new(body.as_slice())
            .read_to_end(&mut out)
            .map(|_| ())
            .map_err(|error| format!("cannot decode gzip response: {error}"))?,
        Some("deflate") => {
            if flate2::read::ZlibDecoder::new(body.as_slice())
                .read_to_end(&mut out)
                .is_err()
            {
                out.clear();
                flate2::read::DeflateDecoder::new(body.as_slice())
                    .read_to_end(&mut out)
                    .map_err(|error| format!("cannot decode deflate response: {error}"))?;
            }
        }
        Some(other) => {
            return Err(format!(
                "unsupported Content-Encoding '{other}' from the remote cache"
            ));
        }
    }
    Ok(out)
}

/// One output of a remote cache hit.
#[derive(Debug)]
pub struct Hit {
    /// Declared outputs in declaration order.
    pub outputs: Vec<Output>,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
}

/// One output of a spawn as the cache holds it: a file's bytes, or a
/// symlink's text (`output_symlinks`, an unresolved symlink artifact).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Output {
    File(Vec<u8>),
    Symlink(String),
    /// A file left in the remote CAS (Build without the Bytes).
    Remote(Digest),
}

/// The REAPI form of one spawn.
#[derive(Clone, Debug)]
pub struct RemoteAction {
    pub command: Vec<u8>,
    pub command_digest: Digest,
    pub action: Vec<u8>,
    pub key: Digest,
    /// Every `Directory` blob of the input root (the root last).
    pub directories: Vec<reapi::DirectoryBlob>,
}

/// What RemoteExecutionService.buildRemoteAction reads from a spawn.
#[derive(Clone, Debug, Default)]
pub struct SpawnSpec {
    pub arguments: Vec<String>,
    pub environment: BTreeMap<String, String>,
    /// Input files by execution path.
    pub inputs: BTreeMap<String, Digest>,
    /// Unresolved symlink inputs by execution path (their link text).
    pub symlinks: BTreeMap<String, String>,
    /// Directories that exist even when empty: empty tree artifact or
    /// source directory inputs and every tree artifact output
    /// (MerkleTreeComputer's EmptyInputDirectory).
    pub directories: std::collections::BTreeSet<String>,
    /// The declared outputs.
    pub outputs: Vec<String>,
    /// The outputs that are tree artifacts.
    pub output_directories: std::collections::BTreeSet<String>,
    /// The sorted `Platform` properties (PlatformUtils.getPlatformProto).
    pub platform: Vec<(String, String)>,
    /// `Action.timeout` (Spawns.getTimeout), whole seconds.
    pub timeout_seconds: Option<u64>,
    /// Spawns.mayBeCachedRemotely (`Action.do_not_cache` when false).
    pub cacheable: bool,
    pub may_be_executed_remotely: bool,
    /// `Command.output_paths` (REAPI >= 2.1) or REAPI 2.0's
    /// `output_files`/`output_directories`.
    pub use_output_paths: bool,
}

/// RemoteExecutionService.buildRemoteAction (:530-591): the Command, the
/// input root and the Action of `spec`.
pub fn remote_action_of(spec: &SpawnSpec) -> Result<RemoteAction, String> {
    let mut output_paths = spec.outputs.clone();
    output_paths.sort();
    let command = reapi::Command {
        arguments: spec.arguments.clone(),
        environment: spec.environment.clone(),
        output_paths,
        output_directories: spec.output_directories.clone(),
        platform: spec.platform.clone(),
    }
    .encode_with(spec.use_output_paths);
    let command_digest = Digest::of(&command);
    let mut files: BTreeMap<String, reapi::InputFile> = spec
        .inputs
        .iter()
        .map(|(path, digest)| {
            (
                path.clone(),
                reapi::InputFile {
                    digest: digest.clone(),
                    symlink: None,
                },
            )
        })
        .collect();
    for (path, target) in &spec.symlinks {
        let symlink = reapi::InputFile {
            digest: Digest::of(b""),
            symlink: Some(target.clone()),
        };
        if files.insert(path.clone(), symlink).is_some() {
            return Err(format!("conflicting input path '{path}'"));
        }
    }
    let (input_root_digest, directories) = reapi::input_tree(&files, &spec.directories)?;
    let action = reapi::Action {
        command_digest: command_digest.clone(),
        input_root_digest,
        timeout_seconds: spec.timeout_seconds,
        do_not_cache: !spec.cacheable,
        may_be_executed_remotely: spec.may_be_executed_remotely,
        platform: spec.platform.clone(),
    }
    .encode();
    let key = Digest::of(&action);
    Ok(RemoteAction {
        command,
        command_digest,
        action,
        key,
        directories,
    })
}

/// RemoteExecutionService.buildRemoteAction for the HTTP cache: `inputs`
/// maps execution paths to digests, `outputs` are the declared output paths.
pub fn remote_action(
    arguments: Vec<String>,
    environment: &BTreeMap<String, String>,
    inputs: &BTreeMap<String, Digest>,
    symlinks: &BTreeMap<String, String>,
    outputs: &[String],
    may_be_executed_remotely: bool,
    use_output_paths: bool,
) -> Result<RemoteAction, String> {
    remote_action_of(&SpawnSpec {
        arguments,
        environment: environment.clone(),
        inputs: inputs.clone(),
        symlinks: symlinks.clone(),
        outputs: outputs.to_vec(),
        cacheable: true,
        may_be_executed_remotely,
        use_output_paths,
        ..SpawnSpec::default()
    })
}

impl HttpCache {
    /// AbstractHttpHandler.addCredentialHeaders: URI user info is Basic
    /// authentication (Guava base64Url); otherwise the helper's headers.
    fn credential_headers(&self) -> Result<Vec<(String, String)>, CacheError> {
        if let Some(user) = &self.uri.user_info {
            use base64::Engine as _;
            let value = base64::engine::general_purpose::URL_SAFE.encode(user.as_bytes());
            return Ok(vec![("Authorization".into(), format!("Basic {value}"))]);
        }
        match crate::credentials::global() {
            Some(credentials) => credentials
                .headers(&self.credential_uri)
                .map(Option::unwrap_or_default)
                .map_err(CacheError::Credentials),
            None => Ok(Vec::new()),
        }
    }

    fn url(&self, cas: bool, hash: &str) -> String {
        format!(
            "{}://{}:{}{}",
            self.uri.scheme,
            self.uri.host,
            self.uri.port,
            self.uri.request_path(cas, hash)
        )
    }

    fn request(
        &self,
        builder: reqwest::blocking::RequestBuilder,
    ) -> Result<reqwest::blocking::RequestBuilder, CacheError> {
        let mut builder = builder.header(reqwest::header::ACCEPT, "*/*");
        for (name, value) in self.credential_headers()?.iter().chain(&self.headers) {
            builder = builder.header(name.as_str(), value.as_str());
        }
        Ok(builder)
    }

    /// HttpCacheClient.authTokenExpired (:800-812): credentials always
    /// exist, so a 401 (or a Bearer `invalid_token` challenge) refreshes the
    /// helper cache and retries once.
    fn token_expired(response: &reqwest::blocking::Response) -> bool {
        let value = response
            .headers()
            .get_all(reqwest::header::WWW_AUTHENTICATE)
            .iter()
            .filter_map(|value| value.to_str().ok())
            .collect::<Vec<_>>()
            .join(",");
        if value.starts_with("Bearer") {
            let lower = value.replace(' ', "");
            lower.contains("error=\"invalid_token\"") || lower.contains("error=invalid_token")
        } else {
            response.status() == reqwest::StatusCode::UNAUTHORIZED
        }
    }

    fn failure(response: reqwest::blocking::Response) -> CacheError {
        // HttpException: the status line, a newline and the body.
        let status = response.status();
        let body = response.text().unwrap_or_default();
        CacheError::Other(format!(
            "{} {}\n{body}",
            status.as_u16(),
            status.canonical_reason().unwrap_or("")
        ))
    }

    fn transport(error: reqwest::Error) -> CacheError {
        // Never echo URLs (they may carry user info).
        let kind = if error.is_timeout() {
            "timed out"
        } else if error.is_connect() {
            "connection failed"
        } else {
            "request failed"
        };
        CacheError::Other(format!("HTTP cache {kind}: {}", error.without_url()))
    }

    /// GET `/ac/HASH` or `/cas/HASH`; `Ok(None)` for 404/204 (cacheMiss).
    pub fn get(&self, cas: bool, hash: &str) -> Result<Option<Vec<u8>>, CacheError> {
        for attempt in 0..2 {
            let request = self.request(
                self.client
                    .get(self.url(cas, hash))
                    .header(reqwest::header::ACCEPT_ENCODING, "gzip,deflate"),
            )?;
            let response = request.send().map_err(Self::transport)?;
            let status = response.status().as_u16();
            if status == 200 {
                let encoding = response
                    .headers()
                    .get(reqwest::header::CONTENT_ENCODING)
                    .and_then(|value| value.to_str().ok())
                    .map(str::to_owned);
                let body = response.bytes().map_err(Self::transport)?.to_vec();
                return decode_body(encoding.as_deref(), body)
                    .map(Some)
                    .map_err(CacheError::Other);
            }
            if attempt == 0 && Self::token_expired(&response) {
                if let Some(credentials) = crate::credentials::global() {
                    credentials.refresh();
                }
                continue;
            }
            if matches!(status, 404 | 204) {
                return Ok(None);
            }
            return Err(Self::failure(response));
        }
        unreachable!("the loop returns on its second attempt")
    }

    /// PUT `/ac/HASH` or `/cas/HASH`; 200/201/202/204 succeed.
    pub fn put(&self, cas: bool, hash: &str, body: &[u8]) -> Result<(), CacheError> {
        for attempt in 0..2 {
            let request = self.request(self.client.put(self.url(cas, hash)))?;
            let response = request
                .header(reqwest::header::CONTENT_LENGTH, body.len())
                .body(body.to_vec())
                .send()
                .map_err(Self::transport)?;
            if matches!(response.status().as_u16(), 200 | 201 | 202 | 204) {
                return Ok(());
            }
            if attempt == 0 && Self::token_expired(&response) {
                if let Some(credentials) = crate::credentials::global() {
                    credentials.refresh();
                }
                continue;
            }
            return Err(Self::failure(response));
        }
        unreachable!("the loop returns on its second attempt")
    }
}

/// A failed capability check fails the spawn that needed it with exit
/// code 34 (CAPABILITIES_QUERY_FAILURE).
fn capability_failure(error: String) -> CacheError {
    note_remote_failure();
    CacheError::Capabilities(error)
}

impl RemoteCache {
    pub fn hits(&self) -> usize {
        self.hits.load(Ordering::Relaxed)
    }

    /// Whether `Command` lists `output_paths` (REAPI >= 2.1) rather than
    /// `output_files`: always for HTTP; for gRPC it needs the capability
    /// check, whose failure fails the action (CacheError::Capabilities).
    /// With an executor, its endpoint's highest version counts too
    /// (RemoteExecutionService.initUseOutputPaths).
    pub fn output_paths(&self) -> Result<bool, CacheError> {
        let executor = match &self.executor {
            Some(executor) => executor.capabilities().map_err(capability_failure)?,
            None => true,
        };
        let cache = match &self.backend {
            Backend::Http(_) => true,
            Backend::Grpc(grpc) => grpc.capabilities().map_err(capability_failure)?,
        };
        Ok(executor && cache)
    }

    fn action_result(&self, key: &Digest) -> Result<Option<Vec<u8>>, CacheError> {
        match &self.backend {
            Backend::Http(http) => http.get(false, &key.hash),
            Backend::Grpc(grpc) => {
                grpc.capabilities().map_err(capability_failure)?;
                Ok(grpc.get_action_result(key)?.map(|bytes| bytes.to_vec()))
            }
        }
    }

    fn read(&self, digest: &Digest, key: &Digest) -> Result<Option<Vec<u8>>, CacheError> {
        match &self.backend {
            Backend::Http(http) => http.get(true, &digest.hash),
            Backend::Grpc(grpc) => Ok(grpc.read_blob(digest, &key.hash)?),
        }
    }

    fn blob(
        &self,
        digest: &Digest,
        inline: &[u8],
        what: &str,
        key: &Digest,
    ) -> Result<Option<Vec<u8>>, CacheError> {
        // Inlined contents are verified like downloaded blobs.
        let bytes = if !inline.is_empty() || digest.size == 0 {
            inline.to_vec()
        } else {
            match self.read(digest, key)? {
                Some(bytes) => bytes,
                None => return Ok(None),
            }
        };
        // --remote_verify_downloads (default true): Utils.verifyBlobContents.
        let actual = Digest::of(&bytes);
        if actual != *digest {
            return Err(CacheError::Other(format!(
                "Output {what} download failed: Expected digest '{digest}' does not match received digest '{actual}'."
            )));
        }
        Ok(Some(bytes))
    }

    /// RemoteSpawnCache.lookup: `Ok(None)` on a miss, including a failed
    /// cached action, a missing declared output or a missing CAS blob
    /// (CacheNotFoundException). Every declared output is downloaded (the
    /// native profile always materializes outputs, like
    /// `--remote_download_outputs=all`).
    /// `outputs`: the declared outputs and whether each is an unresolved
    /// symlink artifact (read from `output_symlinks`).
    /// `stay_remote(index)`: whether a non-empty file output may stay in
    /// the remote CAS (crate::remote_outputs).
    pub fn lookup(
        &self,
        action: &RemoteAction,
        outputs: &[(String, bool)],
        stay_remote: &dyn Fn(usize) -> bool,
    ) -> Result<Option<Hit>, CacheError> {
        let Some(bytes) = self.action_result(&action.key)? else {
            return Ok(None);
        };
        let result = reapi::ActionResult::decode(&bytes).map_err(|error| {
            CacheError::Other(format!("Failed to parse the cached ActionResult: {error}"))
        })?;
        if result.exit_code != 0 {
            return Ok(None);
        }
        let remote_ok = match self.remote_only(
            outputs
                .iter()
                .enumerate()
                .filter_map(|(index, (output, symlink))| {
                    let file = result
                        .output_files
                        .iter()
                        .find(|file| &file.path == output)?;
                    (!symlink && file.digest.size != 0 && stay_remote(index))
                        .then(|| (index, file.digest.clone()))
                }),
            &action.key,
        )? {
            Some(remote_ok) => remote_ok,
            None => return Ok(None),
        };
        let mut downloaded = Vec::new();
        for (index, (output, symlink)) in outputs.iter().enumerate() {
            let linked = result
                .output_symlinks
                .iter()
                .find(|link| &link.path == output);
            match result.output_files.iter().find(|file| &file.path == output) {
                // A declared symlink output must come back as a symlink
                // (else SkyframeActionExecutor.checkOutputs fails it as `not
                // a symlink`, a miss here).
                Some(_) if *symlink => return Ok(None),
                Some(file) if remote_ok.contains(&index) => {
                    downloaded.push(Output::Remote(file.digest.clone()))
                }
                Some(file) => match self.blob(&file.digest, &file.contents, output, &action.key)? {
                    Some(bytes) => downloaded.push(Output::File(bytes)),
                    None => return Ok(None),
                },
                None if *symlink && linked.is_some() => {
                    downloaded.push(Output::Symlink(linked.expect("checked").target.clone()))
                }
                None if linked.is_some() => {
                    return Err(CacheError::Other(format!(
                        "cached output {output} is a directory or symlink, which the native profile does not materialize"
                    )));
                }
                None if result.other_outputs.contains(output) => {
                    return Err(CacheError::Other(format!(
                        "cached output {output} is a directory or symlink, which the native profile does not materialize"
                    )));
                }
                // maybeGetMissingMandatoryOutput: a miss.
                None => return Ok(None),
            }
        }
        let stream = |raw: &[u8], digest: &Option<Digest>, what: &str| match digest {
            Some(digest) => self.blob(digest, raw, what, &action.key),
            None => Ok(Some(raw.to_vec())),
        };
        let Some(stdout) = stream(&result.stdout_raw, &result.stdout_digest, "stdout")? else {
            return Ok(None);
        };
        let Some(stderr) = stream(&result.stderr_raw, &result.stderr_digest, "stderr")? else {
            return Ok(None);
        };
        self.hits.fetch_add(1, Ordering::Relaxed);
        Ok(Some(Hit {
            outputs: downloaded,
            stdout,
            stderr,
        }))
    }

    /// UploadManifest.create + upload: every non-empty blob not yet uploaded
    /// in this invocation (the Action and Command protos, outputs, stdout
    /// and stderr), then the ActionResult.
    pub fn upload(
        &self,
        action: &RemoteAction,
        outputs: &[(String, Output)],
        stdout: &[u8],
        stderr: &[u8],
        start: SystemTime,
        wall: Duration,
    ) -> Result<(), CacheError> {
        let mut blobs: Vec<(Digest, &[u8])> = vec![
            (action.key.clone(), &action.action),
            (action.command_digest.clone(), &action.command),
        ];
        let mut files = Vec::new();
        let mut symlinks = Vec::new();
        for (path, output) in outputs {
            match output {
                Output::File(bytes) => {
                    let digest = Digest::of(bytes);
                    blobs.push((digest.clone(), bytes));
                    files.push(reapi::OutputFile {
                        path: path.clone(),
                        digest,
                        is_executable: true,
                        contents: Vec::new(),
                    });
                }
                Output::Symlink(target) => symlinks.push(reapi::OutputSymlink {
                    path: path.clone(),
                    target: target.clone(),
                }),
                Output::Remote(_) => {
                    return Err(CacheError::Other(format!(
                        "remote-only output {path} cannot be uploaded"
                    )));
                }
            }
        }
        let stdout_digest = Digest::of(stdout);
        let stderr_digest = Digest::of(stderr);
        blobs.push((stdout_digest.clone(), stdout));
        blobs.push((stderr_digest.clone(), stderr));
        self.upload_blobs(&blobs, &action.key)?;
        // UploadManifest.create: timestamps only when the wall time is
        // nonzero (milliseconds).
        let millis = wall.as_millis() as u64;
        let execution = (millis != 0).then(|| {
            let since = start.duration_since(UNIX_EPOCH).unwrap_or_default();
            let end = since + Duration::from_millis(millis);
            let stamp = |d: Duration| reapi::Timestamp {
                seconds: d.as_secs() as i64,
                nanos: d.subsec_nanos() as i32,
            };
            (stamp(since), stamp(end))
        });
        let result = reapi::ActionResult {
            output_files: files,
            output_symlinks: symlinks,
            stdout_digest: Some(stdout_digest),
            stderr_digest: Some(stderr_digest),
            execution,
            ..reapi::ActionResult::default()
        };
        match &self.backend {
            Backend::Http(http) => http.put(false, &action.key.hash, &result.encode()),
            Backend::Grpc(grpc) => Ok(grpc.update_action_result(&action.key, &result.encode())?),
        }
    }

    /// The CAS part of UploadManifest.upload: HTTP PUTs every non-empty
    /// blob not yet uploaded in this invocation; gRPC asks
    /// FindMissingBlobs for all of them (empty ones included) and writes
    /// the missing non-empty blobs not yet uploaded.
    fn upload_blobs(&self, blobs: &[(Digest, &[u8])], key: &Digest) -> Result<(), CacheError> {
        let done = |digest: &Digest| self.uploaded.lock().expect("upload set").contains(digest);
        // Uploads `bytes` unless this invocation already did, waiting for a
        // concurrent upload of the same blob.
        let once = |digest: &Digest,
                    upload: &dyn Fn() -> Result<(), CacheError>|
         -> Result<(), CacheError> {
            if digest.size == 0 || done(digest) {
                return Ok(());
            }
            let lock = self
                .uploading
                .lock()
                .expect("in-flight uploads")
                .entry(digest.clone())
                .or_default()
                .clone();
            let _uploading = lock.lock().expect("in-flight upload");
            if done(digest) {
                return Ok(());
            }
            upload()?;
            self.uploaded
                .lock()
                .expect("upload set")
                .insert(digest.clone());
            Ok(())
        };
        match &self.backend {
            Backend::Http(http) => {
                for (digest, bytes) in blobs {
                    once(digest, &|| http.put(true, &digest.hash, bytes))?;
                }
            }
            Backend::Grpc(grpc) => {
                let mut unique: Vec<Digest> = Vec::new();
                for (digest, _) in blobs {
                    if !unique.contains(digest) {
                        unique.push(digest.clone());
                    }
                }
                for digest in grpc.find_missing(&unique, &key.hash)? {
                    let Some((_, bytes)) = blobs.iter().find(|(known, _)| *known == digest) else {
                        return Err(CacheError::Other(format!(
                            "FindMissingBlobs call returned an unknown digest: {digest}"
                        )));
                    };
                    once(&digest, &|| Ok(grpc.write_blob(&digest, bytes, &key.hash)?))?;
                }
            }
        }
        Ok(())
    }
}

/// Where the bytes of one input blob come from.
#[derive(Clone, Copy, Debug)]
pub enum BlobSource<'a> {
    Bytes(&'a [u8]),
    File(&'a Path),
}

/// What a declared output is (for downloads).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OutputKind {
    File,
    /// An unresolved symlink artifact.
    Symlink,
    /// A tree artifact.
    Directory,
}

/// A downloaded tree artifact: files `(path, bytes, is_executable)`,
/// symlinks `(path, target)` and every directory, by path below the tree.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DownloadedTree {
    pub files: Vec<(String, Vec<u8>, bool)>,
    pub symlinks: Vec<(String, String)>,
    pub directories: Vec<String>,
}

/// One declared output of a downloaded result.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Downloaded {
    File(Vec<u8>),
    Symlink(String),
    Directory(DownloadedTree),
    /// Not in the result (a failed action's).
    Missing,
    /// A file left in the remote CAS (Build without the Bytes).
    Remote(Digest),
}

/// A downloaded ActionResult (RemoteExecutionService.downloadOutputs with
/// `--remote_download_outputs=all`).
#[derive(Clone, Debug)]
pub struct DownloadedResult {
    pub exit_code: i32,
    /// Per declared output, in declaration order.
    pub outputs: Vec<Downloaded>,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
}

impl RemoteCache {
    /// The `--remote_executor` endpoint, if configured.
    pub fn executor(&self) -> Option<&crate::remote_grpc::GrpcCache> {
        self.executor.as_deref()
    }

    /// Spawns executed remotely so far.
    pub fn executions(&self) -> usize {
        self.executions.load(Ordering::Relaxed)
    }

    pub fn count_execution(&self) {
        self.executions.fetch_add(1, Ordering::Relaxed);
    }

    pub fn count_hit(&self) {
        self.hits.fetch_add(1, Ordering::Relaxed);
    }

    /// `bytestream://AUTHORITY/[INSTANCE/]` of a gRPC cache (the build
    /// event files' URIs of remote-only outputs); `None` for HTTP.
    pub fn bytestream_prefix(&self) -> Option<String> {
        match &self.backend {
            Backend::Grpc(grpc) => Some(grpc.bytestream_prefix()),
            Backend::Http(_) => None,
        }
    }

    /// Which of the `candidates` (output index, digest) stay remote-only
    /// (Build without the Bytes): with a gRPC cache all of them, once one
    /// FindMissingBlobs found every blob present (`None` when one is
    /// missing: the result is unusable, a cache miss, and the producer
    /// runs instead of leaving a consumer a lost input); an HTTP cache
    /// cannot tell, so its outputs are downloaded.
    fn remote_only(
        &self,
        candidates: impl Iterator<Item = (usize, Digest)>,
        key: &Digest,
    ) -> Result<Option<HashSet<usize>>, CacheError> {
        let candidates: Vec<(usize, Digest)> = candidates.collect();
        if candidates.is_empty() {
            return Ok(Some(HashSet::new()));
        }
        let Backend::Grpc(grpc) = &self.backend else {
            return Ok(Some(HashSet::new()));
        };
        let mut digests: Vec<Digest> = candidates
            .iter()
            .map(|(_, digest)| digest.clone())
            .collect();
        digests.sort_by(|a, b| a.hash.cmp(&b.hash));
        digests.dedup();
        if !grpc.find_missing(&digests, &key.hash)?.is_empty() {
            return Ok(None);
        }
        Ok(Some(
            candidates.into_iter().map(|(index, _)| index).collect(),
        ))
    }

    /// One blob by digest, verified (a remote-only output being
    /// materialized, crate::remote_outputs); `Ok(None)` when the CAS lacks
    /// it.
    pub fn fetch_blob(&self, digest: &Digest) -> Result<Option<Vec<u8>>, CacheError> {
        self.blob(digest, &[], &digest.to_string(), digest)
    }

    /// GetActionResult, decoded; `Ok(None)` on a miss.
    pub fn cached_result(&self, key: &Digest) -> Result<Option<reapi::ActionResult>, CacheError> {
        let Some(bytes) = self.action_result(key)? else {
            return Ok(None);
        };
        reapi::ActionResult::decode(&bytes)
            .map(Some)
            .map_err(|error| {
                CacheError::Other(format!("Failed to parse the ActionResult: {error}"))
            })
    }

    /// RemoteExecutionCache.ensureInputsPresent: one FindMissingBlobs over
    /// every blob not known present (all of them with `force`, after the
    /// server reported missing inputs), then a ByteStream.Write per missing
    /// blob (once per invocation unless forced).
    pub fn ensure_inputs_present(
        &self,
        blobs: &BTreeMap<Digest, BlobSource<'_>>,
        key: &Digest,
        force: bool,
    ) -> Result<(), CacheError> {
        let Backend::Grpc(grpc) = &self.backend else {
            return Err(CacheError::Other(
                "remote execution needs a gRPC cache".into(),
            ));
        };
        let query = {
            let present = self.present.lock().expect("present blobs");
            blobs
                .keys()
                .filter(|digest| force || !present.contains(*digest))
                .cloned()
                .collect::<Vec<_>>()
        };
        if query.is_empty() {
            return Ok(());
        }
        let missing = grpc.find_missing(&query, &key.hash)?;
        {
            let mut present = self.present.lock().expect("present blobs");
            for digest in &query {
                if !missing.contains(digest) {
                    present.insert(digest.clone());
                }
            }
        }
        for digest in missing {
            let Some(source) = blobs.get(&digest) else {
                return Err(CacheError::Other(format!(
                    "findMissingDigests returned a missing digest that has not been requested: {digest}"
                )));
            };
            if digest.size != 0 {
                let lock = self
                    .uploading
                    .lock()
                    .expect("in-flight uploads")
                    .entry(digest.clone())
                    .or_default()
                    .clone();
                let _uploading = lock.lock().expect("in-flight upload");
                let done = self.uploaded.lock().expect("upload set").contains(&digest);
                if force || !done {
                    let bytes;
                    let data: &[u8] = match source {
                        BlobSource::Bytes(bytes) => bytes,
                        BlobSource::File(path) => {
                            bytes = match std::fs::read(path) {
                                Ok(bytes) => bytes,
                                // A remote-only input (Build without the
                                // Bytes) the remote CAS lost.
                                Err(_) if crate::remote_outputs::get(path).is_some() => {
                                    crate::action::lost_remote_output(path);
                                    return Err(CacheError::Other(format!(
                                        "Lost inputs no longer available remotely: {} ({digest})",
                                        path.display()
                                    )));
                                }
                                Err(error) => {
                                    return Err(CacheError::Other(format!(
                                        "cannot read input {}: {error}",
                                        path.display()
                                    )));
                                }
                            };
                            if Digest::of(&bytes) != digest {
                                return Err(CacheError::Other(format!(
                                    "input {} changed during the build (expected digest {digest})",
                                    path.display()
                                )));
                            }
                            &bytes
                        }
                    };
                    grpc.write_blob(&digest, data, &key.hash)?;
                    self.uploaded
                        .lock()
                        .expect("upload set")
                        .insert(digest.clone());
                }
            }
            self.present
                .lock()
                .expect("present blobs")
                .insert(digest.clone());
        }
        Ok(())
    }

    /// One downloaded blob, verified (`--remote_verify_downloads`);
    /// `Ok(None)` when the CAS lacks it (CacheNotFoundException).
    fn verified(
        &self,
        digest: &Digest,
        inline: &[u8],
        what: &str,
        key: &Digest,
    ) -> Result<Option<Vec<u8>>, CacheError> {
        self.blob(digest, inline, what, key)
    }

    /// RemoteExecutionService.downloadOutputs (`all`): every declared
    /// output of `result` (files, symlinks, tree artifacts from their
    /// `Tree`), stdout and stderr. `Ok(None)` when a blob is missing.
    /// `stay_remote(index)`: whether a non-empty file output may stay in
    /// the remote CAS (crate::remote_outputs; a failed result's outputs are
    /// always downloaded); a cached result's are checked present first,
    /// a `fresh`ly executed one's are not.
    pub fn download(
        &self,
        result: &reapi::ActionResult,
        declared: &[(String, OutputKind)],
        key: &Digest,
        stay_remote: &dyn Fn(usize) -> bool,
        fresh: bool,
    ) -> Result<Option<DownloadedResult>, CacheError> {
        let remote_ok = if result.exit_code == 0 {
            let candidates = declared
                .iter()
                .enumerate()
                .filter_map(|(index, (path, kind))| {
                    let file = result.output_files.iter().find(|file| &file.path == path)?;
                    (*kind == OutputKind::File && file.digest.size != 0 && stay_remote(index))
                        .then(|| (index, file.digest.clone()))
                });
            if fresh {
                // Just executed: the executor's outputs are in its CAS.
                candidates.map(|(index, _)| index).collect()
            } else {
                match self.remote_only(candidates, key)? {
                    Some(remote_ok) => remote_ok,
                    None => return Ok(None),
                }
            }
        } else {
            HashSet::new()
        };
        let mut outputs = Vec::new();
        for (index, (path, kind)) in declared.iter().enumerate() {
            let file = result.output_files.iter().find(|file| &file.path == path);
            let link = result
                .output_symlinks
                .iter()
                .find(|link| &link.path == path);
            let directory = result
                .output_directories
                .iter()
                .find(|directory| &directory.path == path);
            let output = match (kind, file, link, directory) {
                (OutputKind::File, Some(file), _, _) if remote_ok.contains(&index) => {
                    Downloaded::Remote(file.digest.clone())
                }
                (OutputKind::File, Some(file), _, _) => {
                    match self.verified(&file.digest, &file.contents, path, key)? {
                        Some(bytes) => Downloaded::File(bytes),
                        None => return Ok(None),
                    }
                }
                (OutputKind::Symlink, None, Some(link), _) => {
                    Downloaded::Symlink(link.target.clone())
                }
                (OutputKind::Directory, None, None, Some(directory)) => {
                    let Some(bytes) = self.verified(&directory.tree_digest, &[], path, key)? else {
                        return Ok(None);
                    };
                    let tree = reapi::Tree::decode(&bytes).map_err(|error| {
                        CacheError::Other(format!("Failed to parse the Tree of {path}: {error}"))
                    })?;
                    let entries = tree.entries().map_err(CacheError::Other)?;
                    let mut downloaded = DownloadedTree {
                        symlinks: entries.symlinks,
                        directories: entries.directories,
                        ..DownloadedTree::default()
                    };
                    for (name, digest, executable) in entries.files {
                        let what = format!("{path}/{name}");
                        match self.verified(&digest, &[], &what, key)? {
                            Some(bytes) => downloaded.files.push((name, bytes, executable)),
                            None => return Ok(None),
                        }
                    }
                    Downloaded::Directory(downloaded)
                }
                (_, None, None, None) => Downloaded::Missing,
                (kind, ..) => {
                    return Err(CacheError::Other(format!(
                        "output {path} of the remote result is not a {}",
                        match kind {
                            OutputKind::File => "regular file",
                            OutputKind::Symlink => "symlink",
                            OutputKind::Directory => "directory",
                        }
                    )));
                }
            };
            outputs.push(output);
        }
        let stream = |raw: &[u8], digest: &Option<Digest>, what: &str| match digest {
            Some(digest) => self.verified(digest, raw, what, key),
            None => Ok(Some(raw.to_vec())),
        };
        let Some(stdout) = stream(&result.stdout_raw, &result.stdout_digest, "stdout")? else {
            return Ok(None);
        };
        let Some(stderr) = stream(&result.stderr_raw, &result.stderr_digest, "stderr")? else {
            return Ok(None);
        };
        Ok(Some(DownloadedResult {
            exit_code: result.exit_code,
            outputs,
            stdout,
            stderr,
        }))
    }
}

/// The `Remote Cache:` warning of a non-credential failure, once per
/// distinct message (RemoteExecutionService.report, :2158-2166).
pub fn warn(error: &CacheError) {
    static REPORTED: Mutex<Option<HashSet<String>>> = Mutex::new(None);
    let message = format!("Remote Cache: {error}");
    let mut reported = REPORTED.lock().expect("reported warnings");
    if reported
        .get_or_insert_with(HashSet::new)
        .insert(message.clone())
    {
        crate::note!("WARNING: {message}");
    }
}

/// Converts an execution path for the REAPI messages.
pub fn path_string(path: &Path) -> Result<String, String> {
    path.to_str().map(str::to_owned).ok_or_else(|| {
        format!(
            "non-UTF-8 path {} cannot enter a remote cache key",
            path.display()
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn options_follow_bazel_converters() {
        let mut options = RemoteOptions::default();
        assert_eq!(
            options.parse("--remote_cache=http://h", None),
            Some(Ok(false))
        );
        assert_eq!(
            options.parse("--remote_http_cache", Some("https://x")),
            Some(Ok(true))
        );
        assert_eq!(options.cache, "https://x");
        assert_eq!(
            options.parse("--noremote_upload_local_results", None),
            Some(Ok(false))
        );
        assert!(!options.upload_local_results);
        assert_eq!(
            options.parse("--remote_accept_cached=no", None),
            Some(Ok(false))
        );
        assert!(!options.accept_cached);
        assert_eq!(options.parse("--remote_timeout=5", None), Some(Ok(false)));
        assert_eq!(options.timeout, Duration::from_secs(5));
        assert_eq!(options.parse("--remote_timeout=2m", None), Some(Ok(false)));
        assert_eq!(options.timeout, Duration::from_secs(120));
        assert_eq!(
            options.parse("--remote_timeout=x", None),
            Some(Err(
                "While parsing option --remote_timeout=x: Illegal duration 'x'.".into()
            ))
        );
        assert_eq!(
            options.parse("--remote_header=A=b=c", None),
            Some(Ok(false))
        );
        assert_eq!(options.headers, [("A".to_owned(), "b=c".to_owned())]);
        assert_eq!(
            options.parse("--remote_cache_header==x", None),
            Some(Err("While parsing option --remote_cache_header==x: Variable definitions must be in the form of a 'name=value' assignment".into()))
        );
        assert_eq!(
            options.parse("--remote_instance_name=dev", None),
            Some(Ok(false))
        );
        assert_eq!(
            options.parse("--remote_download_outputs=ALL", None),
            Some(Ok(false))
        );
        assert_eq!(options.download_outputs, DownloadOutputs::All);
        assert_eq!(
            options.parse("--remote_download_minimal", None),
            Some(Ok(false))
        );
        assert_eq!(options.download_outputs, DownloadOutputs::Minimal);
        assert_eq!(
            options.parse("--experimental_remote_retry_max_attempts", Some("0")),
            Some(Ok(true))
        );
        assert_eq!(options.retries, 0);
        assert_eq!(
            options.parse("--remote_retry_max_delay=250ms", None),
            Some(Ok(false))
        );
        assert_eq!(options.retry_max_delay, Duration::from_millis(250));
        assert_eq!(
            options.parse("--remote_max_connections=x", None),
            Some(Err(
                "While parsing option --remote_max_connections=x: 'x' is not an int".into()
            ))
        );
        assert_eq!(
            options.parse("--tls_certificate=/ca.pem", None),
            Some(Ok(false))
        );
        assert_eq!(
            options.tls.certificate.as_deref(),
            Some(Path::new("/ca.pem"))
        );
        assert_eq!(
            options.parse("--noremote_cache_compression", None),
            Some(Ok(false))
        );
        assert_eq!(
            options.parse("--remote_cache_compression", None),
            Some(Ok(false))
        );
        assert!(options.cache_compression);
        assert_eq!(
            options.parse("--experimental_remote_cache_compression_threshold=7", None),
            Some(Ok(false))
        );
        assert_eq!(options.cache_compression_threshold, 7);
        assert_eq!(
            options.parse("--remote_download_regex=.*\\.o", None),
            Some(Ok(false))
        );
        assert_eq!(options.download_regex, [".*\\.o"]);
        assert!(matches!(
            options.parse("--remote_download_regex=(", None),
            Some(Err(_))
        ));
        assert_eq!(
            options.parse("--experimental_remote_cache_ttl=10", None),
            Some(Ok(false))
        );
        assert_eq!(options.cache_ttl, Duration::from_secs(10));
    }

    #[test]
    fn uris_get_the_default_port_like_http_cache_client() {
        let uri = CacheUri::parse("https://cache.example.com").unwrap();
        assert_eq!(uri.port, 443);
        assert_eq!(
            uri.credential_uri("https://cache.example.com"),
            "https://cache.example.com:443"
        );
        assert_eq!(uri.request_path(false, "ab"), "/ac/ab");
        let uri = CacheUri::parse("http://u:p@127.0.0.1:8080/base/?q=1").unwrap();
        assert_eq!(uri.user_info.as_deref(), Some("u:p"));
        assert_eq!(uri.request_path(true, "cd"), "/base/cas/cd");
        assert_eq!(
            uri.credential_uri("http://u:p@127.0.0.1:8080/base/?q=1"),
            "http://u:p@127.0.0.1:8080/base/?q=1"
        );
        assert!(CacheUri::parse("http://:80").is_err());
        assert!(CacheUri::parse("http://h:x").is_err());
    }

    #[test]
    fn setup_accepts_grpc_and_remote_execution() {
        let options = RemoteOptions {
            cache: "grpc://127.0.0.1:1".into(),
            ..RemoteOptions::default()
        };
        let cache = setup(&options, None).unwrap().unwrap();
        assert!(cache.executor().is_none());
        let options = RemoteOptions {
            cache: "grpc://h:1/path".into(),
            ..RemoteOptions::default()
        };
        assert!(
            setup(&options, None)
                .unwrap_err()
                .exit
                .contains("invalid gRPC target")
        );
        // The executor doubles as the cache (RemoteModule.setup).
        let options = RemoteOptions {
            executor: "grpc://127.0.0.1:1".into(),
            ..RemoteOptions::default()
        };
        let remote = setup(&options, None).unwrap().unwrap();
        assert!(remote.executor().is_some());
        let options = RemoteOptions {
            executor: "grpc://127.0.0.1:1".into(),
            cache: "http://127.0.0.1:2".into(),
            ..RemoteOptions::default()
        };
        assert_eq!(
            setup(&options, None).unwrap_err().exit,
            "Cannot combine gRPC based remote execution with HTTP-based caching"
        );
        assert!(setup(&RemoteOptions::default(), None).unwrap().is_none());
    }
}
