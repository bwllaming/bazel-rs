//! Repository download transport shared by `repository_ctx`, `module_ctx` and
//! the registry archive materializer. Deliberately not registry metadata HTTP.
//!
//! Bodies are streamed: every caller writes through [`copy_hashed`] into a file
//! while hashing incrementally, so memory use is independent of the body size.
//! There are no artificial size, count or cumulative caps (genuine Bazel has
//! none). Timeouts follow Bazel's HttpConnector (HttpConnector.java:57-61,
//! 155-157): a 10 s connect timeout and a 20 s read timeout that stays in
//! effect for every subsequent body read, so a slow but progressing transfer
//! never times out. Redirects are bounded like Bazel's (`MAX_REDIRECTS = 40`,
//! "Redirect loop detected", HttpConnector.java:205-208).

use base64::Engine;
use reqwest::Url;
use reqwest::blocking::{Client, Response};
use sha2::{Digest, Sha256};
use std::fmt;
use std::fs::{File, OpenOptions};
use std::io::{self, Read, Write};
use std::path::Path;
use std::time::Duration;

/// Streaming copy buffer.
pub(crate) const BUFFER: usize = 64 * 1024;
/// HttpConnector.MAX_REDIRECTS (HttpConnector.java:57).
const MAX_REDIRECTS: usize = 40;
/// HttpConnector.MAX_CONNECT_TIMEOUT_MS (HttpConnector.java:60).
pub(crate) const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
/// HttpConnector.READ_TIMEOUT_MS (HttpConnector.java:61): per read, not total.
pub(crate) const READ_TIMEOUT: Duration = Duration::from_secs(20);

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum TypedIoError {
    Completion(String),
    Validation(&'static str),
    Unsupported(&'static str),
    Policy(&'static str),
    Interruption,
    Internal(&'static str),
}

impl fmt::Display for TypedIoError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Completion(s) => write!(f, "module I/O completion: {s}"),
            Self::Validation(s) => write!(f, "module I/O validation: {s}"),
            Self::Unsupported(s) => write!(f, "unsupported module I/O profile: {s}"),
            Self::Policy(s) => write!(f, "module I/O policy: {s}"),
            Self::Interruption => f.write_str("module I/O interrupted"),
            Self::Internal(s) => write!(f, "module I/O internal: {s}"),
        }
    }
}
impl std::error::Error for TypedIoError {}
pub(crate) type IoResult<T> = Result<T, TypedIoError>;

pub(crate) fn io_error(error: io::Error) -> TypedIoError {
    if let Some(typed) = error
        .get_ref()
        .and_then(|e| e.downcast_ref::<TypedIoError>())
    {
        return typed.clone();
    }
    if error.kind() == io::ErrorKind::Interrupted {
        return TypedIoError::Interruption;
    }
    if let Some(code) = error.raw_os_error() {
        // A real local resource failure (ENOSPC, EDQUOT, EFBIG, EMFILE, ...):
        // the OS reason, as Java's IOException reports it. Filesystem errors
        // carry no URL/body text, so the reason is safe to show.
        let text = io::Error::from_raw_os_error(code).to_string();
        let reason = text
            .rsplit_once(" (os error ")
            .map_or(text.as_str(), |(reason, _)| reason)
            .to_owned();
        return TypedIoError::Completion(reason);
    }
    // Deliberately no URL/body/error-chain strings (which may contain secrets).
    TypedIoError::Completion(format!("filesystem or response stream {:?}", error.kind()))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Algorithm {
    Sha1,
    Sha256,
    Sha384,
    Sha512,
    Blake3,
}
impl Algorithm {
    pub(crate) fn name(self) -> &'static str {
        match self {
            Self::Sha1 => "sha1",
            Self::Sha256 => "sha256",
            Self::Sha384 => "sha384",
            Self::Sha512 => "sha512",
            Self::Blake3 => "blake3",
        }
    }
    fn size(self) -> usize {
        match self {
            Self::Sha1 => 20,
            Self::Sha256 | Self::Blake3 => 32,
            Self::Sha384 => 48,
            Self::Sha512 => 64,
        }
    }
    #[cfg(test)]
    pub(crate) fn hash(self, bytes: &[u8]) -> Vec<u8> {
        match self {
            Self::Sha1 => sha1::Sha1::digest(bytes).to_vec(),
            Self::Sha256 => Sha256::digest(bytes).to_vec(),
            Self::Sha384 => sha2::Sha384::digest(bytes).to_vec(),
            Self::Sha512 => sha2::Sha512::digest(bytes).to_vec(),
            Self::Blake3 => blake3::hash(bytes).as_bytes().to_vec(),
        }
    }
    /// Hash a file incrementally (constant memory).
    pub(crate) fn hash_file(self, path: &Path) -> io::Result<(Vec<u8>, u64)> {
        let mut file = File::open(path)?;
        let mut hash = Hash::new(self);
        let size = copy_hashed(&mut file, &mut io::sink(), &mut hash)?;
        Ok((hash.finish(), size))
    }
}
pub(crate) fn hex(bytes: &[u8]) -> String {
    const HEX: &[u8] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push(HEX[(byte >> 4) as usize] as char);
        out.push(HEX[(byte & 15) as usize] as char);
    }
    out
}
pub(crate) fn sha256(bytes: &[u8]) -> String {
    hex(&Sha256::digest(bytes))
}

pub(crate) enum Hash {
    Sha1(sha1::Sha1),
    Sha256(Sha256),
    Sha384(sha2::Sha384),
    Sha512(sha2::Sha512),
    Blake3(Box<blake3::Hasher>),
}
impl Hash {
    pub(crate) fn new(algorithm: Algorithm) -> Self {
        match algorithm {
            Algorithm::Sha1 => Self::Sha1(sha1::Sha1::new()),
            Algorithm::Sha256 => Self::Sha256(Sha256::new()),
            Algorithm::Sha384 => Self::Sha384(sha2::Sha384::new()),
            Algorithm::Sha512 => Self::Sha512(sha2::Sha512::new()),
            Algorithm::Blake3 => Self::Blake3(Box::new(blake3::Hasher::new())),
        }
    }
    pub(crate) fn update(&mut self, bytes: &[u8]) {
        match self {
            Self::Sha1(h) => h.update(bytes),
            Self::Sha256(h) => h.update(bytes),
            Self::Sha384(h) => h.update(bytes),
            Self::Sha512(h) => h.update(bytes),
            Self::Blake3(h) => {
                h.update(bytes);
            }
        }
    }
    pub(crate) fn finish(self) -> Vec<u8> {
        match self {
            Self::Sha1(h) => h.finalize().to_vec(),
            Self::Sha256(h) => h.finalize().to_vec(),
            Self::Sha384(h) => h.finalize().to_vec(),
            Self::Sha512(h) => h.finalize().to_vec(),
            Self::Blake3(h) => blake3::Hasher::finalize(&h).as_bytes().to_vec(),
        }
    }
}

#[derive(Debug, Clone)]
pub(crate) struct Checksum {
    pub(crate) algorithm: Algorithm,
    pub(crate) bytes: Vec<u8>,
}
impl Checksum {
    pub(crate) fn integrity(&self) -> String {
        format!(
            "{}-{}",
            self.algorithm.name(),
            base64::engine::general_purpose::STANDARD.encode(&self.bytes)
        )
    }
    #[cfg(test)]
    pub(crate) fn matches(&self, bytes: &[u8]) -> bool {
        self.algorithm.hash(bytes) == self.bytes
    }
}
pub(crate) struct Checksums {
    pub(crate) expected: Option<Checksum>,
    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) deferred: Option<TypedIoError>,
}
pub(crate) fn checksums(sha: &str, integrity: &str) -> IoResult<Checksums> {
    if !sha.is_empty() && !integrity.is_empty() {
        return Err(TypedIoError::Validation(
            "sha256 and integrity are mutually exclusive",
        ));
    }
    let parsed = if !sha.is_empty() {
        if sha.len() == 64 && sha.bytes().all(|b| b.is_ascii_hexdigit()) {
            let bytes = sha
                .as_bytes()
                .as_chunks::<2>()
                .0
                .iter()
                .map(|pair| {
                    let digit = |b: u8| {
                        if b.is_ascii_digit() {
                            b - b'0'
                        } else {
                            b.to_ascii_lowercase() - b'a' + 10
                        }
                    };
                    (digit(pair[0]) << 4) | digit(pair[1])
                })
                .collect();
            Ok(Checksum {
                algorithm: Algorithm::Sha256,
                bytes,
            })
        } else {
            Err(TypedIoError::Validation("invalid SHA256 checksum"))
        }
    } else if !integrity.is_empty() {
        let (algorithm, data) = [
            Algorithm::Sha1,
            Algorithm::Sha256,
            Algorithm::Sha384,
            Algorithm::Sha512,
            Algorithm::Blake3,
        ]
        .into_iter()
        .find_map(|a| {
            integrity
                .strip_prefix(a.name())
                .and_then(|s| s.strip_prefix('-'))
                .map(|s| (a, s))
        })
        .ok_or(TypedIoError::Unsupported(
            "checksum algorithm (single sha1/sha256/sha384/sha512/blake3 SRI required)",
        ))?;
        // Java's basic Base64 decoder allows missing padding and nonzero unused
        // trailing bits, but rejects whitespace, extra padding and URL-safe chars.
        let engine = base64::engine::general_purpose::GeneralPurpose::new(
            &base64::alphabet::STANDARD,
            base64::engine::general_purpose::GeneralPurposeConfig::new()
                .with_decode_padding_mode(base64::engine::DecodePaddingMode::Indifferent)
                .with_decode_allow_trailing_bits(true),
        );
        let padding_valid = !data.contains('=') || data.len().is_multiple_of(4);
        engine
            .decode(data)
            .ok()
            .filter(|b| padding_valid && b.len() == algorithm.size())
            .map(|bytes| Checksum { algorithm, bytes })
            .ok_or(TypedIoError::Validation("invalid SRI checksum"))
    } else {
        return Ok(Checksums {
            expected: None,
            deferred: None,
        });
    };
    Ok(match parsed {
        Ok(expected) => Checksums {
            expected: Some(expected),
            deferred: None,
        },
        Err(error) => Checksums {
            expected: None,
            deferred: Some(error),
        },
    })
}

pub(crate) fn validate_url(text: &str) -> IoResult<Url> {
    if !text.is_ascii()
        || text.bytes().any(|b| {
            b <= 32
                || b == 127
                || matches!(
                    b,
                    b'\\' | b'"' | b'<' | b'>' | b'^' | b'`' | b'{' | b'|' | b'}'
                )
        })
    {
        return Err(TypedIoError::Validation("malformed URL"));
    }
    // url::Url is deliberately forgiving about percent escapes and absent //;
    // reject these rather than silently turning invalid URI text into a request.
    for (i, b) in text.bytes().enumerate() {
        if b == b'%'
            && !text
                .as_bytes()
                .get(i + 1..i + 3)
                .is_some_and(|s| s.iter().all(u8::is_ascii_hexdigit))
        {
            return Err(TypedIoError::Validation("malformed URL escape"));
        }
    }
    let url = Url::parse(text).map_err(|_| TypedIoError::Validation("malformed URL"))?;
    if !matches!(url.scheme(), "http" | "https") {
        return Err(TypedIoError::Unsupported("only HTTP(S) URLs are supported"));
    }
    if !text
        .split_once(':')
        .is_some_and(|(_, rest)| rest.starts_with("//"))
        || url.host_str().is_none()
    {
        return Err(TypedIoError::Validation("URL requires a host"));
    }
    let authority_has_user = text.split_once("//").is_some_and(|(_, rest)| {
        rest.split(['/', '?', '#'])
            .next()
            .is_some_and(|a| a.contains('@'))
    });
    if authority_has_user
        || !url.username().is_empty()
        || url.password().is_some()
        || url.fragment().is_some()
    {
        return Err(TypedIoError::Unsupported("URL credentials and fragments"));
    }
    Ok(url)
}

pub(crate) fn client() -> IoResult<Client> {
    client_builder()
        .build()
        .map_err(|_| TypedIoError::Internal("HTTP client initialization"))
}
fn client_builder() -> reqwest::blocking::ClientBuilder {
    // The blocking client applies `timeout` to sending the request and to
    // every individual body read: Bazel's per-read socket timeout, not a
    // whole-transfer deadline. ProxyHelper over the command's repository
    // environment (crate::download_proxy; StarlarkBaseExternalContext.java:864-873).
    crate::download_proxy::configure(Client::builder().no_proxy())
        .referer(false)
        .redirect(reqwest::redirect::Policy::none())
        .no_gzip()
        .no_brotli()
        .no_deflate()
        .no_zstd()
        .connect_timeout(CONNECT_TIMEOUT)
        .timeout(READ_TIMEOUT)
        .pool_max_idle_per_host(0)
}

struct Encoded {
    response: Response,
}
impl Read for Encoded {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        // flate2's header parser retries raw Interrupted errors. Preserve a
        // native interruption as a typed cause instead of silently retrying it.
        self.response.read(buf).map_err(|error| {
            if error.kind() == io::ErrorKind::Interrupted {
                io::Error::other(TypedIoError::Interruption)
            } else {
                error
            }
        })
    }
}

// flate2 1.1.10 stores constructor header errors until Read. An empty read
// surfaces that error (or completes a pending header) without decoding body
// bytes or checking a trailer. Do this BEFORE transfer opens the destination.
fn checked_gzip<R: Read>(reader: R) -> IoResult<flate2::read::MultiGzDecoder<R>> {
    let mut decoder = flate2::read::MultiGzDecoder::new(reader);
    decoder.read(&mut []).map_err(io_error)?;
    Ok(decoder)
}

/// Bazel's view of one failed mirror: the Java exception class and message
/// HttpDownloader reports (`Download from URL failed: class C MSG`,
/// HttpDownloader.java:217-240) and puts last in `Error downloading [...]`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct BazelFailure {
    pub(crate) class: &'static str,
    pub(crate) message: String,
}

pub(crate) const UNRECOVERABLE: &str =
    "com.google.devtools.build.lib.bazel.repository.downloader.UnrecoverableHttpException";
pub(crate) const IO_EXCEPTION: &str = "java.io.IOException";

impl BazelFailure {
    fn new(class: &'static str, message: impl Into<String>) -> Self {
        Self {
            class,
            message: message.into(),
        }
    }
}

/// A typed failure with Bazel's description of it.
pub(crate) struct OpenFailure {
    pub(crate) error: TypedIoError,
    pub(crate) bazel: BazelFailure,
}

impl From<TypedIoError> for OpenFailure {
    fn from(error: TypedIoError) -> Self {
        let message = match &error {
            TypedIoError::Completion(message) => message.clone(),
            other => other.to_string(),
        };
        Self {
            error,
            bazel: BazelFailure::new(IO_EXCEPTION, message),
        }
    }
}

/// HttpConnector's exception for a connection that never produced a
/// response (java:160-190, 254-290): refused connections are
/// `java.net.ConnectException`, unresolvable hosts `Unknown host: HOST`
/// (unrecoverable) and connect timeouts an IOException `Connect timed out`.
/// The native completion text keeps its stable prefix.
fn connection_failure(error: &reqwest::Error, url: &Url) -> OpenFailure {
    let mut refused = false;
    let mut dns = false;
    let mut source: Option<&dyn std::error::Error> = Some(error);
    while let Some(current) = source {
        if let Some(io) = current.downcast_ref::<io::Error>()
            && io.kind() == io::ErrorKind::ConnectionRefused
        {
            refused = true;
        }
        if current.to_string().starts_with("dns error") {
            dns = true;
        }
        source = current.source();
    }
    let bazel = if dns {
        BazelFailure::new(
            UNRECOVERABLE,
            format!("Unknown host: {}", url.host_str().unwrap_or("")),
        )
    } else if refused {
        BazelFailure::new("java.net.ConnectException", "Connection refused")
    } else if error.is_timeout() {
        BazelFailure::new(IO_EXCEPTION, "Connect timed out")
    } else {
        BazelFailure::new(IO_EXCEPTION, "HTTP connection failed")
    };
    let detail = if bazel.message == "HTTP connection failed" {
        String::new()
    } else {
        format!(": {}", bazel.message)
    };
    OpenFailure {
        error: TypedIoError::Completion(format!("HTTP connection failed{detail}")),
        bazel,
    }
}

#[cfg(test)]
pub(crate) fn open(client: &Client, original: &Url) -> IoResult<Box<dyn Read>> {
    open_detailed(client, original).map_err(|failure| failure.error)
}

/// Extra request headers of `download(headers=, auth=)`
/// (StarlarkBaseExternalContext.getHeaderContents/getAuthHeaders, java:350-441).
/// `common` headers go on every request (HttpConnector.java:139-150 applies
/// the header function to each connection, redirects included); `auth`
/// headers only on a request whose URI equals the auth key (StaticCredentials
/// keyed by URI), so they are not forwarded to a redirect target.
/// Header names with their Latin-1 byte values.
pub(crate) type Headers = Vec<(String, Vec<u8>)>;

#[derive(Debug, Clone, Default)]
pub(crate) struct RequestHeaders {
    /// Names and Latin-1 byte values, in call order.
    pub(crate) common: Headers,
    pub(crate) auth: Vec<(Url, Headers)>,
}

impl RequestHeaders {
    /// HttpConnectorMultiplexer.getHeaderFunction (:133-151): the base
    /// headers, then the credentials' request metadata, built keeping the
    /// last value of a repeated name. The credentials are
    /// CredentialHelperCredentials with the static `auth` headers as fallback
    /// (BazelRepositoryModule.java:463-472): a helper that applies to `url`
    /// replaces them entirely.
    fn apply(
        &self,
        mut request: reqwest::blocking::RequestBuilder,
        url: &Url,
    ) -> reqwest::blocking::RequestBuilder {
        // Both sides are Map<String, List<String>>: a credential header name
        // replaces every base value of that name (buildKeepingLast).
        let credentials: Headers = match crate::credentials::download_credentials(url.as_str()) {
            Some(helper) => helper
                .into_iter()
                .map(|(name, value)| (name, value.into_bytes()))
                .collect(),
            None => self
                .auth
                .iter()
                .filter(|(key, _)| key == url)
                .flat_map(|(_, headers)| headers.iter().cloned())
                .collect(),
        };
        let merged = self
            .common
            .iter()
            .filter(|(name, _)| !credentials.iter().any(|(other, _)| other == name))
            .cloned()
            .chain(credentials.iter().cloned())
            .collect::<Headers>();
        for (name, value) in merged {
            // The transport owns Accept-Encoding (gzip, decoded per Bazel's
            // suffix rules); a user value would change what it must decode.
            if name.eq_ignore_ascii_case("Accept-Encoding") {
                continue;
            }
            request = request.header(name.as_str(), value.as_slice());
        }
        request
    }
}

/// [`open`] with Bazel's description of a failure.
#[cfg(test)]
pub(crate) fn open_detailed(client: &Client, original: &Url) -> Result<Box<dyn Read>, OpenFailure> {
    open_with_headers(client, original, &RequestHeaders::default())
}

/// [`open_detailed`] sending `headers`.
pub(crate) fn open_with_headers(
    client: &Client,
    original: &Url,
    headers: &RequestHeaders,
) -> Result<Box<dyn Read>, OpenFailure> {
    crate::observed_fs::taint(crate::observed_fs::Taint::Network, || original.to_string());
    // Only a validated 301 updates the next attempt's origin. The immutable
    // original URL still controls original-path gzip suffix suppression.
    let mut retry_origin: Option<Url> = None;
    for attempt in 0..2 {
        let start = retry_origin.as_ref().unwrap_or(original);
        let mut current = start.clone();
        let mut redirects = 0;
        loop {
            // HttpConnector asks ProxyHelper for every connection
            // (HttpConnector.java:125-136).
            crate::download_proxy::check(&current).map_err(TypedIoError::Completion)?;
            let request = client
                .get(current.clone())
                .header("Accept-Encoding", "gzip");
            // Per URL, redirect hops included (HttpConnectorMultiplexer
            // .getHeaderFunction, :133-151): base headers, then a credential
            // helper's headers (N41) or else the static `auth` ones.
            let response = headers.apply(request, &current).send();
            let response = match response {
                Ok(response) => response,
                Err(error) => {
                    if attempt == 0 && (error.is_connect() || error.is_timeout()) {
                        break;
                    }
                    return Err(connection_failure(&error, &current));
                }
            };
            let status = response.status().as_u16();
            if matches!(status, 301 | 302 | 303 | 307) {
                redirects += 1;
                if redirects == MAX_REDIRECTS {
                    // UnrecoverableHttpException (HttpConnector.java:205-208).
                    return Err(OpenFailure {
                        error: TypedIoError::Completion("Redirect loop detected".into()),
                        bazel: BazelFailure::new(UNRECOVERABLE, "Redirect loop detected"),
                    });
                }
                let location = response
                    .headers()
                    .get(reqwest::header::LOCATION)
                    .and_then(|s| s.to_str().ok())
                    .ok_or_else(|| TypedIoError::Completion("redirect without Location".into()))?;
                if !location.is_ascii()
                    || location.bytes().any(|b| b <= 32 || b == 127 || b == b'\\')
                {
                    return Err(TypedIoError::Validation("invalid redirect Location").into());
                }
                let next = current
                    .join(location)
                    .map_err(|_| TypedIoError::Validation("invalid redirect Location"))?;
                let next = validate_url(next.as_str())?;
                if current.scheme() == "https" && next.scheme() == "http" {
                    return Err(TypedIoError::Policy("HTTPS downgrade redirect").into());
                }
                if status == 301 {
                    retry_origin = Some(next.clone());
                }
                current = next;
                continue;
            }
            if !matches!(status, 200 | 206) {
                // Pinned HttpConnector: 403/408/429 and 5xx except 501/505.
                let retry = matches!(status, 403 | 408 | 429)
                    || (status >= 500 && !matches!(status, 501 | 505));
                if retry && attempt == 0 {
                    break;
                }
                // describeHttpResponse (HttpConnector.java:309-315): the
                // server's reason phrase; 404/410 are FileNotFoundException,
                // retryable codes plain IOException, others unrecoverable.
                let reason = response
                    .extensions()
                    .get::<hyper::ext::ReasonPhrase>()
                    .map(|reason| String::from_utf8_lossy(reason.as_bytes()).into_owned())
                    .or_else(|| response.status().canonical_reason().map(str::to_owned))
                    .unwrap_or_default();
                let class = if matches!(status, 404 | 410) {
                    "java.io.FileNotFoundException"
                } else if retry {
                    IO_EXCEPTION
                } else {
                    UNRECOVERABLE
                };
                return Err(OpenFailure {
                    error: TypedIoError::Completion(format!("HTTP status {status}")),
                    bazel: BazelFailure::new(class, format!("GET returned {status} {reason}")),
                });
            }
            let encoding = response
                .headers()
                .get(reqwest::header::CONTENT_ENCODING)
                .map(|s| {
                    s.to_str()
                        .map_err(|_| TypedIoError::Unsupported("content encoding"))
                })
                .transpose()?
                .unwrap_or("identity");
            if !matches!(encoding, "identity" | "gzip" | "x-gzip") {
                return Err(TypedIoError::Unsupported("content encoding").into());
            }
            let suffix = |url: &Url| {
                url.path().rsplit_once('.').is_some_and(|(_, extension)| {
                    extension.eq_ignore_ascii_case("gz") || extension.eq_ignore_ascii_case("tgz")
                })
            };
            let decompress =
                matches!(encoding, "gzip" | "x-gzip") && !suffix(original) && !suffix(&current);
            let stream = Encoded { response };
            return Ok(if decompress {
                Box::new(checked_gzip(stream)?)
            } else {
                Box::new(stream)
            });
        }
    }
    Err(TypedIoError::Internal("attempt loop exhausted").into())
}

/// Stream `reader` into `writer`, hashing every byte, in constant memory.
/// Read and write failures surface as `io::Error`s (see [`io_error`]).
pub(crate) fn copy_hashed(
    reader: &mut dyn Read,
    writer: &mut dyn Write,
    hash: &mut Hash,
) -> io::Result<u64> {
    let mut buffer = vec![0u8; BUFFER];
    let mut total = 0u64;
    loop {
        // A speculative fetch the pool stopped (crate::repository_speculation).
        if crate::repository_speculation::cancelled() {
            return Err(io::Error::other(crate::repository_speculation::CANCELLED));
        }
        let n = match reader.read(&mut buffer) {
            Ok(0) => return Ok(total),
            Ok(n) => n,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {
                return Err(io::Error::other(TypedIoError::Interruption));
            }
            Err(error) => return Err(error),
        };
        hash.update(&buffer[..n]);
        writer.write_all(&buffer[..n])?;
        crate::progress::downloaded(n);
        total += n as u64;
    }
}

/// HashOutputStream's mismatch text (HashOutputStream.java:84-90): "Checksum
/// was %s but wanted %s", hex unless the checksum came from `integrity`.
pub(crate) fn checksum_mismatch(actual: &Checksum, expected: &Checksum, sri: bool) -> String {
    let shown = |checksum: &Checksum| {
        if sri {
            checksum.integrity()
        } else {
            hex(&checksum.bytes)
        }
    };
    format!(
        "Checksum was {} but wanted {}",
        shown(actual),
        shown(expected)
    )
}

/// A completed streaming download.
#[derive(Debug)]
pub(crate) struct Fetched {
    /// Bytes written.
    pub(crate) size: u64,
    /// Digest of the written bytes, in the expected checksum's algorithm, or
    /// SHA-256 when no checksum was given.
    pub(crate) checksum: Checksum,
}

/// Download the first mirror that completes into the file chosen by
/// `destination(url)` (created or truncated), hashing while writing, and
/// verify the expected checksum. A failed mirror's file is removed before the
/// next is tried; only completion failures (connection, status, stream,
/// filesystem, checksum mismatch) move on to the next mirror.
///
/// `keep_failed` leaves a failed mirror's bytes in place, as Bazel's
/// HttpDownloader does for `repository_ctx.download` (it streams into the
/// destination with `getOutputStream`, and HashInputStream reports a
/// mismatch only at end of stream, HttpDownloader.java:90-109,
/// HashInputStream.java:61-91). `on_failure` sees each failed mirror with
/// Bazel's exception class and message.
#[allow(clippy::too_many_arguments)]
pub(crate) fn fetch_to_file(
    client: &Client,
    urls: &[Url],
    expected: Option<&Checksum>,
    sri: bool,
    headers: &RequestHeaders,
    destination: &mut dyn FnMut(&Url) -> std::path::PathBuf,
    keep_failed: bool,
    on_failure: &mut dyn FnMut(&Url, &BazelFailure),
) -> IoResult<Fetched> {
    if urls.is_empty() {
        return Err(TypedIoError::Completion("no download URLs".into()));
    }
    let algorithm = expected.map_or(Algorithm::Sha256, |checksum| checksum.algorithm);
    let mut last = TypedIoError::Completion("no mirror succeeded".into());
    for url in urls {
        let path = destination(url);
        let mut opened = false;
        let attempt = (|| -> Result<Fetched, OpenFailure> {
            let mut reader = open_with_headers(client, url, headers)?;
            opened = true;
            // Like ctx.file, replace (never write through) an existing
            // non-directory destination - a symlink or a hard-linked file -
            // only once a mirror has responded, as Bazel opens its output
            // stream only then (HttpDownloader.java:102-106).
            if std::fs::symlink_metadata(&path).is_ok_and(|metadata| !metadata.is_dir()) {
                std::fs::remove_file(&path).map_err(io_error)?;
            }
            let mut file = OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(true)
                .open(&path)
                .map_err(|error| {
                    if error.kind() == io::ErrorKind::IsADirectory {
                        // HttpDownloader opens the output after connecting:
                        // FileOutputStream's FileNotFoundException.
                        let message = format!("{} (Is a directory)", path.display());
                        OpenFailure {
                            error: TypedIoError::Completion(message.clone()),
                            bazel: BazelFailure::new("java.io.FileNotFoundException", message),
                        }
                    } else {
                        io_error(error).into()
                    }
                })?;
            let mut hash = Hash::new(algorithm);
            // The download's bytes in the progress display (when on).
            let _downloading = crate::progress::downloading(url.as_str());
            let size = copy_hashed(&mut *reader, &mut file, &mut hash).map_err(io_error)?;
            file.flush().map_err(io_error)?;
            let actual = Checksum {
                algorithm,
                bytes: hash.finish(),
            };
            if let Some(expected) = expected
                && actual.bytes != expected.bytes
            {
                let message = checksum_mismatch(&actual, expected, sri);
                return Err(OpenFailure {
                    error: TypedIoError::Completion(message.clone()),
                    bazel: BazelFailure::new(UNRECOVERABLE, message),
                });
            }
            Ok(Fetched {
                size,
                checksum: actual,
            })
        })();
        match attempt {
            Ok(fetched) => return Ok(fetched),
            Err(failure) => {
                if opened && !keep_failed {
                    let _ = std::fs::remove_file(&path);
                }
                match failure.error {
                    TypedIoError::Completion(_) => {
                        on_failure(url, &failure.bazel);
                        last = failure.error;
                    }
                    other => return Err(other),
                }
            }
        }
    }
    Err(last)
}

#[cfg(test)]
pub(crate) fn trusted_client(pem: &[u8]) -> Client {
    client_builder()
        .tls_built_in_root_certs(false)
        .add_root_certificate(reqwest::Certificate::from_pem(pem).unwrap())
        .build()
        .unwrap()
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::extension_io::tests::{Server, response};
    use std::cell::RefCell;
    use std::rc::Rc;

    #[derive(Default, Debug)]
    pub(crate) struct ReadTrace {
        pub requests: Vec<usize>,
        pub returned: usize,
    }

    #[derive(Debug)]
    pub(crate) struct Metered<R> {
        inner: R,
        trace: Rc<RefCell<ReadTrace>>,
        max_chunk: usize,
        fault: Option<(usize, TypedIoError)>,
    }
    impl<R> Metered<R> {
        pub(crate) fn new(inner: R) -> (Self, Rc<RefCell<ReadTrace>>) {
            let trace = Rc::new(RefCell::new(ReadTrace::default()));
            (
                Self {
                    inner,
                    trace: trace.clone(),
                    max_chunk: usize::MAX,
                    fault: None,
                },
                trace,
            )
        }
        fn with_chunks(mut self, size: usize) -> Self {
            self.max_chunk = size;
            self
        }
        fn with_fault(mut self, after: usize, error: TypedIoError) -> Self {
            self.fault = Some((after, error));
            self
        }
    }
    impl<R: Read> Read for Metered<R> {
        fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
            self.trace.borrow_mut().requests.push(buffer.len());
            if let Some((after, error)) = &self.fault
                && self.trace.borrow().returned >= *after
            {
                return Err(io::Error::other(error.clone()));
            }
            let bound = buffer.len().min(self.max_chunk);
            let n = self.inner.read(&mut buffer[..bound])?;
            self.trace.borrow_mut().returned += n;
            Ok(n)
        }
    }
    pub(crate) fn gzip_bytes(body: &[u8]) -> Vec<u8> {
        use std::io::Write;
        let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        encoder.write_all(body).unwrap();
        encoder.finish().unwrap()
    }

    #[test]
    fn first_gzip_header_validation_never_requests_decoded_body_and_preserves_typed_errors() {
        let header = vec![0x1f, 0x8b, 8, 0, 0, 0, 0, 0, 0, 255];
        for error in [
            TypedIoError::Validation("header budget"),
            TypedIoError::Policy("header policy"),
            TypedIoError::Interruption,
        ] {
            let (reader, trace) = Metered::new(std::io::Cursor::new(header.clone()));
            let reader = reader.with_chunks(10).with_fault(10, error.clone());
            let mut decoder = checked_gzip(reader).unwrap();
            assert!(decoder.header().is_some());
            assert_eq!(trace.borrow().returned, 10);
            assert_eq!(
                trace.borrow().requests.len(),
                1,
                "body requested during admission"
            );
            assert_eq!(io_error(decoder.read(&mut [0; 1]).unwrap_err()), error);

            let (reader, trace) = Metered::new(std::io::Cursor::new(header.clone()));
            let returned = checked_gzip(reader.with_fault(0, error.clone())).unwrap_err();
            assert_eq!(returned, error);
            assert_eq!(trace.borrow().returned, 0);
        }
    }

    #[test]
    fn malformed_gzip_headers_fail_admission_but_valid_header_with_bad_body_does_not() {
        let header = vec![0x1f, 0x8b, 8, 0, 0, 0, 0, 0, 0, 255];
        let mut bad = vec![vec![], b"invalid header".to_vec(), header[..9].to_vec()];
        for (index, byte) in [(2, 7), (3, 0x80)] {
            let mut bytes = header.clone();
            bytes[index] = byte;
            bad.push(bytes);
        }
        for flag in [4, 8, 16, 2] {
            let mut bytes = header.clone();
            bytes[3] = flag;
            bytes.extend_from_slice(if flag == 4 { &[5, 0, 1, 2] } else { &[1] });
            bad.push(bytes);
        }
        let mut bad_crc = header.clone();
        bad_crc[3] = 2;
        bad_crc.extend_from_slice(&[0, 0]);
        bad.push(bad_crc);
        for bytes in bad {
            assert!(matches!(
                checked_gzip(std::io::Cursor::new(bytes)),
                Err(TypedIoError::Completion(_))
            ));
        }
        let mut valid_header_bad_body = header;
        valid_header_bad_body.push(7); // Reserved deflate block type, AFTER a valid gzip header.
        let mut decoder = checked_gzip(std::io::Cursor::new(valid_header_bad_body)).unwrap();
        assert!(matches!(
            io_error(decoder.read(&mut [0; 1]).unwrap_err()),
            TypedIoError::Completion(_)
        ));
    }

    #[test]
    fn permanent_redirect_retry_origin_is_distinct_from_initial_gzip_url() {
        for status in [301, 302, 303, 307] {
            let body = b"redirect bytes";
            let gzip = gzip_bytes(body);
            let mut responses = vec![
                response(status, b"", "Location: /final?y=2\r\n"),
                response(503, b"", ""),
            ];
            if status != 301 {
                responses.push(response(status, b"", "Location: /final?y=2\r\n"));
            }
            responses.push(response(200, &gzip, "Content-Encoding: gzip\r\n"));
            let server = Server::new(responses);
            let initial = Url::parse(&format!("{}/start.GZ?x=1", server.url)).unwrap();
            let mut reader = open(&client().unwrap(), &initial).unwrap();
            let mut result = Vec::new();
            reader.read_to_end(&mut result).unwrap();
            assert_eq!(
                result, gzip,
                "immutable original suffix was lost, status {status}"
            );
            let requests = server.requests.lock().unwrap();
            let paths: Vec<_> = requests
                .iter()
                .map(|r| r.split_whitespace().nth(1).unwrap())
                .collect();
            assert_eq!(
                paths,
                if status == 301 {
                    vec!["/start.GZ?x=1", "/final?y=2", "/final?y=2"]
                } else {
                    vec!["/start.GZ?x=1", "/final?y=2", "/start.GZ?x=1", "/final?y=2"]
                }
            );
        }
        let server = Server::new(vec![
            response(301, b"", "Location: /stable\r\n"),
            response(302, b"", "Location: /temporary\r\n"),
            response(503, b"", ""),
            response(302, b"", "Location: /temporary\r\n"),
            response(200, b"body", ""),
        ]);
        let mut reader = open(
            &client().unwrap(),
            &Url::parse(&format!("{}/start", server.url)).unwrap(),
        )
        .unwrap();
        let mut result = Vec::new();
        reader.read_to_end(&mut result).unwrap();
        assert_eq!(result, b"body");
        let requests = server.requests.lock().unwrap();
        assert_eq!(
            requests
                .iter()
                .map(|r| r.split_whitespace().nth(1).unwrap())
                .collect::<Vec<_>>(),
            ["/start", "/stable", "/temporary", "/stable", "/temporary"]
        );
    }

    #[test]
    fn checksum_grammar_five_algorithms_padding_case_and_delayed_invalid() {
        for algorithm in [
            Algorithm::Sha1,
            Algorithm::Sha256,
            Algorithm::Sha384,
            Algorithm::Sha512,
            Algorithm::Blake3,
        ] {
            let checksum = Checksum {
                algorithm,
                bytes: algorithm.hash(b"fixture"),
            };
            for text in [
                checksum.integrity(),
                checksum.integrity().trim_end_matches('=').to_owned(),
            ] {
                let parsed = checksums("", &text).unwrap();
                assert!(parsed.deferred.is_none());
                assert!(parsed.expected.unwrap().matches(b"fixture"));
            }
            for bad in [
                format!("{} ", checksum.integrity()),
                format!("{}=", checksum.integrity()),
                format!("{} {}", checksum.integrity(), checksum.integrity()),
                format!("{}-AAAA", algorithm.name()),
            ] {
                assert!(checksums("", &bad).unwrap().deferred.is_some(), "{bad}");
            }
        }
        let sha = sha256(b"fixture");
        assert!(
            checksums(&sha.to_uppercase(), "")
                .unwrap()
                .expected
                .unwrap()
                .matches(b"fixture")
        );
        assert!(checksums("bad", "").unwrap().deferred.is_some());
        assert!(matches!(
            checksums(&sha, "sha256-AAAA"),
            Err(TypedIoError::Validation(_))
        ));
        for text in ["SHA256-AAAA", "sha-256-AAAA", "md5-AAAA", " sha256-AAAA"] {
            assert!(matches!(
                checksums("", text),
                Err(TypedIoError::Unsupported(_))
            ));
        }
    }

    #[test]
    fn typed_error_boundary_maps_os_resource_failures_to_completion() {
        assert_eq!(
            io_error(io::Error::new(io::ErrorKind::Interrupted, "fixture")),
            TypedIoError::Interruption
        );
        assert_eq!(
            io_error(io::Error::other(TypedIoError::Policy("fixture"))),
            TypedIoError::Policy("fixture")
        );
        assert!(matches!(
            io_error(io::Error::other("Resource is not a classification")),
            TypedIoError::Completion(_)
        ));
        // ENOSPC is a real resource failure: Bazel's IOException reason text,
        // a completion failure like any other download I/O error.
        assert_eq!(
            io_error(io::Error::from_raw_os_error(28)),
            TypedIoError::Completion("No space left on device".into())
        );
    }

    #[test]
    fn redirect_loop_is_bazels_forty_redirect_completion_and_no_implicit_decode() {
        for status in [301, 302] {
            let server = Server::new(
                (0..41)
                    .map(|_| response(status, b"", "Location: /loop\r\n"))
                    .collect(),
            );
            let result = open(&client().unwrap(), &Url::parse(&server.url).unwrap());
            assert_eq!(
                result.err(),
                Some(TypedIoError::Completion("Redirect loop detected".into()))
            );
            // `++redirects == MAX_REDIRECTS` throws on the 40th redirect.
            assert_eq!(server.count(), 40);
        }
        let server = Server::new(vec![response(200, b"raw", "Content-Encoding: br\r\n")]);
        let result = open(&client().unwrap(), &Url::parse(&server.url).unwrap());
        assert!(matches!(result, Err(TypedIoError::Unsupported(_))));
    }

    #[test]
    fn streaming_fetch_hashes_while_writing_and_has_no_size_cap() {
        // 40 MiB: above every former module/repository I/O cap (8, 16, 32 MiB).
        let body: Vec<u8> = (0..40 * 1024 * 1024).map(|i| (i % 251) as u8).collect();
        let digest = Algorithm::Sha256.hash(&body);
        let server = Server::new(vec![response(200, b"wrong", ""), response(200, &body, "")]);
        let directory = tempfile::tempdir().unwrap();
        let expected = Checksum {
            algorithm: Algorithm::Sha256,
            bytes: digest.clone(),
        };
        let urls = [
            Url::parse(&format!("{}/first", server.url)).unwrap(),
            Url::parse(&format!("{}/second", server.url)).unwrap(),
        ];
        let mut targets = Vec::new();
        let mut failures = Vec::new();
        let fetched = fetch_to_file(
            &client().unwrap(),
            &urls,
            Some(&expected),
            false,
            &RequestHeaders::default(),
            &mut |url| {
                let path = directory.path().join(url.path().trim_start_matches('/'));
                targets.push(path.clone());
                path
            },
            false,
            &mut |url, failure| failures.push((url.clone(), failure.clone())),
        )
        .unwrap();
        assert_eq!(failures.len(), 1);
        assert_eq!(failures[0].0, urls[0]);
        assert_eq!(failures[0].1.class, UNRECOVERABLE);
        assert!(failures[0].1.message.starts_with("Checksum was "));
        // The second mirror produced the bytes (its file holds them below).
        assert_eq!(fetched.size, body.len() as u64);
        assert_eq!(fetched.checksum.bytes, digest);
        assert!(!targets[0].exists(), "failed mirror file must be removed");
        assert_eq!(
            Algorithm::Sha256.hash_file(&targets[1]).unwrap(),
            (digest, body.len() as u64)
        );
        let server = Server::new(vec![response(200, b"other", "")]);
        let error = fetch_to_file(
            &client().unwrap(),
            &[Url::parse(&server.url).unwrap()],
            Some(&expected),
            true,
            &RequestHeaders::default(),
            &mut |_| directory.path().join("mismatch"),
            false,
            &mut |_, _| {},
        )
        .unwrap_err();
        assert_eq!(
            error,
            TypedIoError::Completion(format!(
                "Checksum was {} but wanted {}",
                Checksum {
                    algorithm: Algorithm::Sha256,
                    bytes: Algorithm::Sha256.hash(b"other")
                }
                .integrity(),
                expected.integrity()
            ))
        );
        assert!(!directory.path().join("mismatch").exists());
        // keep_failed (repository_ctx.download): the mismatching bytes stay,
        // like Bazel's HttpDownloader writing straight to the destination.
        let server = Server::new(vec![response(200, b"other", "")]);
        fetch_to_file(
            &client().unwrap(),
            &[Url::parse(&server.url).unwrap()],
            Some(&expected),
            false,
            &RequestHeaders::default(),
            &mut |_| directory.path().join("kept"),
            true,
            &mut |_, _| {},
        )
        .unwrap_err();
        assert_eq!(
            std::fs::read(directory.path().join("kept")).unwrap(),
            b"other"
        );
    }

    /// A loopback port nothing listens on and nothing CAN listen on while
    /// the returned sockets live. Binding port 0 and dropping the listener
    /// (the old recipe) freed the port at once: any other test server binding
    /// port 0 could get it, and the "refused" request then reached that
    /// server, failed this test (a status instead of a refusal, or a reset)
    /// and consumed a response the other test expected. Here the port is the
    /// local end of an established connection: its socket has no
    /// SO_REUSEADDR, so the kernel refuses every bind of the port (explicit or
    /// port 0) while it lives, and a SYN to it finds no listener (RST, i.e.
    /// ECONNREFUSED). The peer listener must stay open too, or it would reset
    /// the connection and release the port.
    fn refused_port() -> ((std::net::TcpListener, std::net::TcpStream), u16) {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let stream = std::net::TcpStream::connect(listener.local_addr().unwrap()).unwrap();
        let port = stream.local_addr().unwrap().port();
        assert_eq!(
            std::net::TcpListener::bind(("127.0.0.1", port))
                .unwrap_err()
                .kind(),
            io::ErrorKind::AddrInUse
        );
        ((listener, stream), port)
    }

    #[test]
    fn bazel_failure_classes_and_messages_for_status_and_connection_errors() {
        let server = Server::new(vec![
            b"HTTP/1.1 404 File not found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                .to_vec(),
            response(500, b"", ""),
            response(500, b"", ""),
            response(418, b"", ""),
        ]);
        let failure = |path: &str| {
            open_detailed(
                &client().unwrap(),
                &Url::parse(&format!("{}{path}", server.url)).unwrap(),
            )
            .err()
            .unwrap()
        };
        let missing = failure("/missing");
        assert_eq!(
            missing.error,
            TypedIoError::Completion("HTTP status 404".into())
        );
        assert_eq!(
            missing.bazel,
            BazelFailure::new(
                "java.io.FileNotFoundException",
                "GET returned 404 File not found"
            )
        );
        assert_eq!(
            failure("/flaky").bazel,
            BazelFailure::new(IO_EXCEPTION, "GET returned 500 Fixture")
        );
        assert_eq!(
            failure("/teapot").bazel,
            BazelFailure::new(UNRECOVERABLE, "GET returned 418 Fixture")
        );
        // A closed port: java.net.ConnectException "Connection refused".
        let (_held, port) = refused_port();
        let refused = open_detailed(
            &client().unwrap(),
            &Url::parse(&format!("http://127.0.0.1:{port}/x")).unwrap(),
        )
        .err()
        .unwrap();
        assert_eq!(
            refused.error,
            TypedIoError::Completion("HTTP connection failed: Connection refused".into())
        );
        assert_eq!(
            refused.bazel,
            BazelFailure::new("java.net.ConnectException", "Connection refused")
        );
    }
}
