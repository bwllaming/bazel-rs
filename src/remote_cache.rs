//! Bazel 9.3's HTTP remote cache (N41, docs/REMOTE_CACHE.md).
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
//! gRPC caches and remote execution are explicit failures.

use crate::credentials::{Credentials, HelperError};
use crate::reapi::{self, Digest};
use std::collections::{BTreeMap, HashSet};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Mutex, OnceLock};
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
        }
    }
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
                // CommonRemoteOptions.RemoteDurationConverter (:61-77):
                // unitless values are seconds.
                let input = if !value.is_empty() && value.bytes().all(|b| b.is_ascii_digit()) {
                    format!("{value}s")
                } else {
                    value.to_owned()
                };
                self.timeout = crate::credentials::parse_duration(&input).map_err(|error| {
                    format!("While parsing option --remote_timeout={value}: {error}")
                })?;
                Ok(used)
            }));
        }
        for (name, list) in [
            ("remote_header", &mut self.headers),
            ("remote_cache_header", &mut self.cache_headers),
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
    Other(String),
}

impl std::fmt::Display for CacheError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CacheError::Credentials(error) => write!(f, "{error}"),
            CacheError::Other(error) => f.write_str(error),
        }
    }
}

/// The configured HTTP cache of this invocation.
pub struct RemoteCache {
    uri: CacheUri,
    credential_uri: String,
    client: reqwest::blocking::Client,
    headers: Vec<(String, String)>,
    pub accept_cached: bool,
    pub upload_local_results: bool,
    uploaded: Mutex<HashSet<Digest>>,
    hits: AtomicUsize,
}

impl std::fmt::Debug for RemoteCache {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RemoteCache")
            .field("uri", &self.credential_uri)
            .finish()
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
    if !options.executor.is_empty() {
        return Err(fail(format!(
            "unsupported option --remote_executor={}: remote execution is not implemented by the native profile (only an http(s):// --remote_cache is)",
            options.executor
        )));
    }
    if options.cache.is_empty() {
        return Ok(None);
    }
    let lower = options.cache.to_ascii_lowercase();
    if !(lower.starts_with("http://") || lower.starts_with("https://")) {
        // GrpcCacheClient.isRemoteCacheOptions: anything else is gRPC.
        return Err(fail(format!(
            "unsupported option --remote_cache={}: gRPC remote caches (grpc://, grpcs://, unix:, or no scheme) are not implemented by the native profile; use an http:// or https:// cache",
            options.cache
        )));
    }
    if !options.accept_cached && !options.upload_local_results {
        // RemoteSpawnCache.lookup returns NO_RESULT_NO_STORE (:90-92), but
        // the module still initializes; keep the startup check below.
    }
    let uri = CacheUri::parse(&options.cache).map_err(fail)?;
    let credential_uri = uri.credential_uri(&options.cache);
    if lower.starts_with("http://")
        && let Some(credentials) = credentials
    {
        // createCredentials: `new URI(remoteCache)`, the original string.
        match credentials.headers(&options.cache) {
            Ok(Some(headers)) if !headers.is_empty() => eprintln!(
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
    // Intentional safety divergence (docs/REMOTE_CACHE.md): until native
    // execution parity with Bazel is established, native results are written
    // to a shared cache only on explicit opt-in. Reads stay Bazel-like.
    let upload_local_results = options.upload_local_results && upload_opt_in();
    if options.upload_local_results && !upload_local_results {
        eprintln!(
            "WARNING: native remote cache uploads are disabled by default (a safety divergence from Bazel's --remote_upload_local_results); set {UPLOAD_OPT_IN}=1 to upload local results"
        );
    }
    Ok(Some(RemoteCache {
        uri,
        credential_uri,
        client,
        headers,
        accept_cached: options.accept_cached,
        upload_local_results,
        uploaded: Mutex::new(HashSet::new()),
        hits: AtomicUsize::new(0),
    }))
}

/// The environment variable that opts in to remote cache uploads.
pub const UPLOAD_OPT_IN: &str = "BAZEL_RS_ALLOW_REMOTE_UPLOAD";

fn upload_opt_in() -> bool {
    std::env::var_os(UPLOAD_OPT_IN).is_some_and(|value| value == "1")
}

static ACTIVE: OnceLock<RemoteCache> = OnceLock::new();

/// Installs the invocation's cache (once, from main).
pub fn install(cache: RemoteCache) {
    let _ = ACTIVE.set(cache);
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
}

/// The REAPI form of one spawn.
#[derive(Clone, Debug)]
pub struct RemoteAction {
    pub command: Vec<u8>,
    pub command_digest: Digest,
    pub action: Vec<u8>,
    pub key: Digest,
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
) -> Result<RemoteAction, String> {
    let mut output_paths = outputs.to_vec();
    output_paths.sort();
    let command = reapi::Command {
        arguments,
        environment: environment.clone(),
        output_paths,
        platform: Vec::new(),
    }
    .encode();
    let command_digest = Digest::of(&command);
    let mut files: BTreeMap<String, reapi::InputFile> = inputs
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
    for (path, target) in symlinks {
        let symlink = reapi::InputFile {
            digest: Digest::of(b""),
            symlink: Some(target.clone()),
        };
        if files.insert(path.clone(), symlink).is_some() {
            return Err(format!("conflicting input path '{path}'"));
        }
    }
    let (input_root_digest, _) = reapi::input_root(&files)?;
    let action = reapi::Action {
        command_digest: command_digest.clone(),
        input_root_digest,
        may_be_executed_remotely,
        platform: Vec::new(),
    }
    .encode();
    let key = Digest::of(&action);
    Ok(RemoteAction {
        command,
        command_digest,
        action,
        key,
    })
}

impl RemoteCache {
    pub fn hits(&self) -> usize {
        self.hits.load(Ordering::Relaxed)
    }

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

    fn blob(
        &self,
        digest: &Digest,
        inline: &[u8],
        what: &str,
    ) -> Result<Option<Vec<u8>>, CacheError> {
        // Inlined contents are verified like downloaded blobs.
        let bytes = if !inline.is_empty() || digest.size == 0 {
            inline.to_vec()
        } else {
            match self.get(true, &digest.hash)? {
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
    pub fn lookup(
        &self,
        action: &RemoteAction,
        outputs: &[(String, bool)],
    ) -> Result<Option<Hit>, CacheError> {
        let Some(bytes) = self.get(false, &action.key.hash)? else {
            return Ok(None);
        };
        let result = reapi::ActionResult::decode(&bytes).map_err(|error| {
            CacheError::Other(format!("Failed to parse the cached ActionResult: {error}"))
        })?;
        if result.exit_code != 0 {
            return Ok(None);
        }
        let mut downloaded = Vec::new();
        for (output, symlink) in outputs {
            let linked = result
                .output_symlinks
                .iter()
                .find(|link| &link.path == output);
            match result.output_files.iter().find(|file| &file.path == output) {
                // A declared symlink output must come back as a symlink
                // (else SkyframeActionExecutor.checkOutputs fails it as `not
                // a symlink`, a miss here).
                Some(_) if *symlink => return Ok(None),
                Some(file) => match self.blob(&file.digest, &file.contents, output)? {
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
            Some(digest) => self.blob(digest, raw, what),
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
            }
        }
        let stdout_digest = Digest::of(stdout);
        let stderr_digest = Digest::of(stderr);
        blobs.push((stdout_digest.clone(), stdout));
        blobs.push((stderr_digest.clone(), stderr));
        for (digest, bytes) in blobs {
            if digest.size == 0 || self.uploaded.lock().expect("upload set").contains(&digest) {
                continue;
            }
            self.put(true, &digest.hash, bytes)?;
            self.uploaded.lock().expect("upload set").insert(digest);
        }
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
        self.put(false, &action.key.hash, &result.encode())
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
        eprintln!("WARNING: {message}");
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
        assert_eq!(options.parse("--remote_download_outputs=all", None), None);
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
    fn setup_rejects_grpc_and_remote_execution() {
        let options = RemoteOptions {
            cache: "grpc://127.0.0.1:1".into(),
            ..RemoteOptions::default()
        };
        assert!(
            setup(&options, None)
                .unwrap_err()
                .exit
                .contains("gRPC remote caches")
        );
        let options = RemoteOptions {
            executor: "grpc://x".into(),
            ..RemoteOptions::default()
        };
        assert!(
            setup(&options, None)
                .unwrap_err()
                .exit
                .contains("remote execution")
        );
        assert!(setup(&RemoteOptions::default(), None).unwrap().is_none());
    }
}
