//! Private synchronous module download transport. Deliberately not registry HTTP.
//! Limits are a native profile, not Bazel defaults. Deadline checks are cooperative:
//! the blocking resolver cannot promise cancellation of an in-flight DNS lookup.

use base64::Engine;
use reqwest::Url;
use reqwest::blocking::{Client, Response};
use sha2::{Digest, Sha256};
use std::cell::RefCell;
use std::fmt;
use std::io::{self, Read};
use std::rc::Rc;
use std::time::{Duration, Instant};

pub(crate) const FILE_LIMIT: usize = 8 * 1024 * 1024;
pub(crate) const TOTAL_LIMIT: usize = 32 * 1024 * 1024;
pub(crate) const LIVE_LIMIT: usize = 16 * 1024 * 1024;
pub(crate) const PRECHECK: usize = 32768;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum TypedIoError {
    Completion(String),
    Validation(&'static str),
    Unsupported(&'static str),
    Policy(&'static str),
    Resource(&'static str),
    Interruption,
    Lifetime,
    Internal(&'static str),
}

impl fmt::Display for TypedIoError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Completion(s) => write!(f, "module I/O completion: {s}"),
            Self::Validation(s) => write!(f, "module I/O validation: {s}"),
            Self::Unsupported(s) => write!(f, "unsupported module I/O profile: {s}"),
            Self::Policy(s) => write!(f, "module I/O policy: {s}"),
            Self::Resource(s) => write!(f, "module I/O resource limit: {s}"),
            Self::Interruption => f.write_str("module I/O interrupted"),
            Self::Lifetime => f.write_str("module I/O receiver is foreign, frozen, or expired"),
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
        TypedIoError::Interruption
    } else {
        // Deliberately no URL/body/error-chain strings (which may contain secrets).
        TypedIoError::Completion(format!("filesystem or response stream {:?}", error.kind()))
    }
}

#[derive(Debug)]
pub(crate) struct Budget {
    start: Instant,
    calls: usize,
    requests: usize,
    encoded: usize,
    decoded: usize,
    work: usize,
}
pub(crate) type SharedBudget = Rc<RefCell<Budget>>;
impl Default for Budget {
    fn default() -> Self {
        Self {
            start: Instant::now(),
            calls: 0,
            requests: 0,
            encoded: 0,
            decoded: 0,
            work: 0,
        }
    }
}
impl Budget {
    pub(crate) fn remaining(&self) -> IoResult<Duration> {
        Duration::from_secs(120)
            .checked_sub(self.start.elapsed())
            .filter(|d| !d.is_zero())
            .ok_or(TypedIoError::Resource("120s I/O deadline"))
    }
    pub(crate) fn call(&mut self) -> IoResult<()> {
        self.remaining()?;
        Self::charge(&mut self.calls, 1, 32, "32 download/read calls")
    }
    fn charge(counter: &mut usize, amount: usize, cap: usize, label: &'static str) -> IoResult<()> {
        let next = counter
            .checked_add(amount)
            .filter(|n| *n <= cap)
            .ok_or(TypedIoError::Resource(label))?;
        *counter = next;
        Ok(())
    }
    fn request(&mut self) -> IoResult<()> {
        self.remaining()?;
        Self::charge(&mut self.requests, 1, 64, "64 HTTP requests")
    }
    pub(crate) fn decoded(&mut self, amount: usize) -> IoResult<()> {
        self.remaining()?;
        // Include the rejected one-byte EOF probe in accounting. Once over the
        // limit, read_decoded refuses further reads rather than probing again.
        self.decoded = self
            .decoded
            .checked_add(amount)
            .ok_or(TypedIoError::Resource("32MiB cumulative decoded bytes"))?;
        self.work(amount)?;
        if self.decoded > TOTAL_LIMIT {
            return Err(TypedIoError::Resource("32MiB cumulative decoded bytes"));
        }
        Ok(())
    }
    // Separate work accounting includes repeated hashing/reads, precheck copying,
    // buffer reservations and Latin-1 expansion, even if no network occurs.
    pub(crate) fn work(&mut self, amount: usize) -> IoResult<()> {
        self.remaining()?;
        Self::charge(
            &mut self.work,
            amount,
            128 * 1024 * 1024,
            "128MiB cumulative I/O work/buffers",
        )
    }
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
    fn name(self) -> &'static str {
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
    pub(crate) fn hash(self, bytes: &[u8]) -> Vec<u8> {
        match self {
            Self::Sha1 => sha1::Sha1::digest(bytes).to_vec(),
            Self::Sha256 => Sha256::digest(bytes).to_vec(),
            Self::Sha384 => sha2::Sha384::digest(bytes).to_vec(),
            Self::Sha512 => sha2::Sha512::digest(bytes).to_vec(),
            Self::Blake3 => blake3::hash(bytes).as_bytes().to_vec(),
        }
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
    pub(crate) fn matches(&self, bytes: &[u8]) -> bool {
        self.algorithm.hash(bytes) == self.bytes
    }
}
pub(crate) struct Checksums {
    pub(crate) expected: Option<Checksum>,
    pub(crate) deferred: Option<TypedIoError>,
}
pub(crate) fn checksums(sha: &str, integrity: &str) -> IoResult<Checksums> {
    if !sha.is_empty() && !integrity.is_empty() {
        return Err(TypedIoError::Validation(
            "sha256 and integrity are mutually exclusive",
        ));
    }
    if sha.len() > 256 || integrity.len() > 256 {
        return Err(TypedIoError::Resource("checksum text length"));
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
    if text.len() > 8192 {
        return Err(TypedIoError::Resource("URL length"));
    }
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
    Client::builder()
        .no_proxy()
        .referer(false)
        .redirect(reqwest::redirect::Policy::none())
        .no_gzip()
        .no_brotli()
        .no_deflate()
        .no_zstd()
        .connect_timeout(Duration::from_secs(20))
        .pool_max_idle_per_host(0)
}

struct Encoded {
    response: Response,
    budget: SharedBudget,
}
impl Read for Encoded {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let remaining = {
            let budget = self.budget.borrow();
            budget.remaining().map_err(io::Error::other)?;
            TOTAL_LIMIT.saturating_sub(budget.encoded)
        };
        if buf.is_empty() {
            return Ok(0);
        }
        // A one-byte EOF probe permits an exactly-at-limit body. The byte, if
        // present, is counted and rejected, never handed to the decoder/writer.
        let limit = buf.len().min(remaining.max(1));
        let read = self.response.read(&mut buf[..limit]);
        self.budget.borrow().remaining().map_err(io::Error::other)?;
        // flate2's header parser retries raw Interrupted errors. Preserve a
        // native interruption as a typed cause instead of silently retrying it.
        let n = read.map_err(|error| {
            if error.kind() == io::ErrorKind::Interrupted {
                io::Error::other(TypedIoError::Interruption)
            } else {
                error
            }
        })?;
        self.budget.borrow_mut().encoded += n;
        if n > remaining {
            return Err(io::Error::other(TypedIoError::Resource(
                "32MiB cumulative encoded bytes",
            )));
        }
        Ok(n)
    }
}

// Bound the slice given to the decoder, not just the bytes subsequently written.
// file_bytes is this transfer's decoded prefix during precheck, then its written
// size. Existing destination bytes are not part of the incoming transfer size.
pub(crate) fn read_decoded(
    reader: &mut dyn Read,
    buffer: &mut [u8],
    file_bytes: usize,
    budget: &SharedBudget,
) -> IoResult<usize> {
    let cumulative = {
        let budget = budget.borrow();
        budget.remaining()?;
        TOTAL_LIMIT
            .checked_sub(budget.decoded)
            .ok_or(TypedIoError::Resource("32MiB cumulative decoded bytes"))?
    };
    let per_file = FILE_LIMIT
        .checked_sub(file_bytes)
        .ok_or(TypedIoError::Resource("8MiB decoded/file"))?;
    if buffer.is_empty() {
        return Ok(0);
    }
    let remaining = cumulative.min(per_file);
    // Exactly at either cap, one byte distinguishes EOF from excess content.
    // Any excess byte is charged/rejected before hashing, staging or writing.
    let bound = buffer.len().min(PRECHECK).min(remaining.max(1));
    let read = reader.read(&mut buffer[..bound]);
    budget.borrow().remaining()?;
    let n = read.map_err(io_error)?;
    budget.borrow_mut().decoded(n)?;
    if n > per_file {
        return Err(TypedIoError::Resource("8MiB decoded/file"));
    }
    Ok(n)
}

// flate2 1.1.10 stores constructor header errors until Read. An empty read
// surfaces that error (or completes a pending header) without decoding body
// bytes or checking a trailer. Do this BEFORE transfer opens the destination.
fn checked_gzip<R: Read>(reader: R) -> IoResult<flate2::read::MultiGzDecoder<R>> {
    let mut decoder = flate2::read::MultiGzDecoder::new(reader);
    decoder.read(&mut []).map_err(io_error)?;
    Ok(decoder)
}

pub(crate) fn open(
    client: &Client,
    original: &Url,
    budget: SharedBudget,
) -> IoResult<Box<dyn Read>> {
    // Only a validated 301 updates the next attempt's origin. The immutable
    // original URL still controls original-path gzip suffix suppression.
    let mut retry_origin: Option<Url> = None;
    for attempt in 0..2 {
        let start = retry_origin.as_ref().unwrap_or(original);
        budget.borrow_mut().work(start.as_str().len())?;
        let mut current = start.clone();
        let mut redirects = 0;
        loop {
            budget.borrow_mut().request()?;
            budget.borrow_mut().work(current.as_str().len())?;
            let response = client
                .get(current.clone())
                .header("Accept-Encoding", "gzip")
                .timeout(budget.borrow().remaining()?)
                .send();
            let response = match response {
                Ok(response) => response,
                Err(error) => {
                    budget.borrow().remaining()?;
                    if attempt == 0 && (error.is_connect() || error.is_timeout()) {
                        break;
                    }
                    return Err(TypedIoError::Completion("HTTP connection failed".into()));
                }
            };
            let status = response.status().as_u16();
            if matches!(status, 301 | 302 | 303 | 307) {
                if redirects == 8 {
                    return Err(TypedIoError::Resource("8 redirects per attempt"));
                }
                redirects += 1;
                let location = response
                    .headers()
                    .get(reqwest::header::LOCATION)
                    .and_then(|s| s.to_str().ok())
                    .ok_or_else(|| TypedIoError::Completion("redirect without Location".into()))?;
                if location.len() > 8192 {
                    return Err(TypedIoError::Resource("redirect URL length"));
                }
                if !location.is_ascii()
                    || location.bytes().any(|b| b <= 32 || b == 127 || b == b'\\')
                {
                    return Err(TypedIoError::Validation("invalid redirect Location"));
                }
                budget
                    .borrow_mut()
                    .work(current.as_str().len() + location.len())?;
                let next = current
                    .join(location)
                    .map_err(|_| TypedIoError::Validation("invalid redirect Location"))?;
                let next = validate_url(next.as_str())?;
                if current.scheme() == "https" && next.scheme() == "http" {
                    return Err(TypedIoError::Policy("HTTPS downgrade redirect"));
                }
                if status == 301 {
                    budget.borrow_mut().work(next.as_str().len())?;
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
                return Err(TypedIoError::Completion(format!("HTTP status {status}")));
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
                return Err(TypedIoError::Unsupported("content encoding"));
            }
            let suffix = |url: &Url| {
                url.path().rsplit_once('.').is_some_and(|(_, extension)| {
                    extension.eq_ignore_ascii_case("gz") || extension.eq_ignore_ascii_case("tgz")
                })
            };
            let decompress =
                matches!(encoding, "gzip" | "x-gzip") && !suffix(original) && !suffix(&current);
            let stream = Encoded { response, budget };
            return Ok(if decompress {
                Box::new(checked_gzip(stream)?)
            } else {
                Box::new(stream)
            });
        }
    }
    Err(TypedIoError::Internal("attempt loop exhausted"))
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
    // Proposal: test-only setup at existing private counter/Instant boundaries.
    // No clock trait, runtime API, charge, retry or default changes.
    pub(crate) fn n3_exhaust_budget_for_callback(budget: &SharedBudget, deadline: bool) {
        if deadline {
            budget.borrow_mut().start = Instant::now() - Duration::from_secs(121);
        } else {
            budget.borrow_mut().requests = 64;
        }
    }
    use crate::extension_io::tests::{Server, response};

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
    pub(crate) fn decoded_count(budget: &SharedBudget) -> usize {
        budget.borrow().decoded
    }
    pub(crate) fn remaining_encoded_for_test(budget: &SharedBudget, remaining: usize) {
        assert!(remaining <= TOTAL_LIMIT);
        budget.borrow_mut().encoded = TOTAL_LIMIT - remaining;
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
            TypedIoError::Resource("header budget"),
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
            let mut reader = open(
                &client().unwrap(),
                &initial,
                Rc::new(RefCell::new(Budget::default())),
            )
            .unwrap();
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
            Rc::new(RefCell::new(Budget::default())),
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
    fn decoded_reads_bound_actual_returned_bytes_at_cumulative_and_file_edges() {
        for allowance in [0, 1, 7, PRECHECK - 1, PRECHECK, PRECHECK + 1] {
            for excess in [false, true] {
                let budget = Rc::new(RefCell::new(Budget::default()));
                budget
                    .borrow_mut()
                    .decoded(TOTAL_LIMIT - allowance)
                    .unwrap();
                let (mut reader, trace) =
                    Metered::new(std::io::Cursor::new(vec![
                        b'x';
                        allowance + usize::from(excess)
                    ]));
                let mut buffer = [0; PRECHECK];
                let mut accepted = 0;
                let terminal = loop {
                    match read_decoded(&mut reader, &mut buffer, accepted, &budget) {
                        Ok(0) => break Ok(()),
                        Ok(n) => accepted += n,
                        Err(e) => break Err(e),
                    }
                };
                assert_eq!(accepted, allowance);
                assert_eq!(trace.borrow().returned, allowance + usize::from(excess));
                assert_eq!(budget.borrow().decoded, TOTAL_LIMIT + usize::from(excess));
                assert_eq!(terminal.is_err(), excess);
                if let Err(error) = terminal {
                    assert!(matches!(error, TypedIoError::Resource(_)));
                }
                let mut remaining = allowance;
                for requested in &trace.borrow().requests {
                    assert_eq!(*requested, remaining.clamp(1, PRECHECK));
                    remaining = remaining.saturating_sub(*requested);
                }
                if excess {
                    let calls = trace.borrow().requests.len();
                    assert!(matches!(
                        read_decoded(&mut reader, &mut buffer, accepted, &budget),
                        Err(TypedIoError::Resource(_))
                    ));
                    assert_eq!(
                        trace.borrow().requests.len(),
                        calls,
                        "repeated excess probe"
                    );
                }
            }
        }
        for (file_remaining, cumulative_remaining) in [(1, 7), (7, 1), (0, 0)] {
            let budget = Rc::new(RefCell::new(Budget::default()));
            budget
                .borrow_mut()
                .decoded(TOTAL_LIMIT - cumulative_remaining)
                .unwrap();
            let (mut reader, trace) = Metered::new(std::io::repeat(b'x'));
            let mut size = FILE_LIMIT - file_remaining;
            let mut buffer = [0; PRECHECK];
            if file_remaining.min(cumulative_remaining) != 0 {
                size += read_decoded(&mut reader, &mut buffer, size, &budget).unwrap();
            }
            assert!(matches!(
                read_decoded(&mut reader, &mut buffer, size, &budget),
                Err(TypedIoError::Resource(_))
            ));
            assert_eq!(
                trace.borrow().returned,
                file_remaining.min(cumulative_remaining) + 1
            );
        }
    }

    #[test]
    fn gzip_buffered_decoder_obeys_output_slice_and_propagates_faults() {
        for allowance in [0, 1, 7, PRECHECK - 1, PRECHECK, PRECHECK + 1] {
            for excess in [false, true] {
                let body = vec![b'a'; allowance + usize::from(excess)];
                let (encoded, encoded_trace) =
                    Metered::new(std::io::Cursor::new(gzip_bytes(&body)));
                let decoder = checked_gzip(encoded).unwrap();
                let (mut reader, decoded_trace) = Metered::new(decoder);
                let budget = Rc::new(RefCell::new(Budget::default()));
                budget
                    .borrow_mut()
                    .decoded(TOTAL_LIMIT - allowance)
                    .unwrap();
                let mut accepted = 0;
                loop {
                    let mut buffer = [0; PRECHECK];
                    match read_decoded(&mut reader, &mut buffer, accepted, &budget) {
                        Ok(0) => {
                            assert!(!excess);
                            break;
                        }
                        Ok(n) => {
                            assert!(n <= allowance - accepted);
                            accepted += n;
                        }
                        Err(error) => {
                            assert!(excess);
                            assert!(matches!(error, TypedIoError::Resource(_)));
                            break;
                        }
                    }
                }
                assert_eq!(accepted, allowance);
                assert_eq!(
                    decoded_trace.borrow().returned,
                    allowance + usize::from(excess)
                );
                assert_eq!(budget.borrow().decoded, TOTAL_LIMIT + usize::from(excess));
                assert!(encoded_trace.borrow().returned <= gzip_bytes(&body).len());
                assert!(
                    decoded_trace
                        .borrow()
                        .requests
                        .iter()
                        .all(|n| *n <= allowance.clamp(1, PRECHECK))
                );
            }
        }
        for error in [
            TypedIoError::Completion("fault".into()),
            TypedIoError::Interruption,
            TypedIoError::Resource("fault"),
        ] {
            let decoder =
                checked_gzip(std::io::Cursor::new(gzip_bytes(&vec![b'a'; PRECHECK]))).unwrap();
            let (reader, trace) = Metered::new(decoder);
            let mut reader = reader.with_chunks(3).with_fault(3, error.clone());
            let budget = Rc::new(RefCell::new(Budget::default()));
            budget.borrow_mut().decoded(TOTAL_LIMIT - 7).unwrap();
            let mut buffer = [0; PRECHECK];
            assert_eq!(
                read_decoded(&mut reader, &mut buffer, 0, &budget).unwrap(),
                3
            );
            assert_eq!(
                read_decoded(&mut reader, &mut buffer, 3, &budget).unwrap_err(),
                error
            );
            assert_eq!(trace.borrow().requests, [7, 4]);
            assert_eq!(trace.borrow().returned, 3);
            assert_eq!(budget.borrow().decoded, TOTAL_LIMIT - 4);
        }
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
    fn typed_error_boundary_and_independent_budget_counters() {
        assert_eq!(
            io_error(io::Error::new(io::ErrorKind::Interrupted, "fixture")),
            TypedIoError::Interruption
        );
        assert_eq!(
            io_error(io::Error::other(TypedIoError::Resource("fixture"))),
            TypedIoError::Resource("fixture")
        );
        assert!(matches!(
            io_error(io::Error::other("Resource is not a classification")),
            TypedIoError::Completion(_)
        ));
        let mut budget = Budget::default();
        budget.decoded(TOTAL_LIMIT).unwrap();
        assert!(matches!(budget.decoded(1), Err(TypedIoError::Resource(_))));
        assert_eq!(budget.encoded, 0);
        let mut budget = Budget::default();
        budget.work(128 * 1024 * 1024).unwrap();
        assert!(budget.work(1).is_err());
        for _ in 0..64 {
            budget.request().unwrap();
        }
        assert!(budget.request().is_err());
        budget.start = Instant::now() - Duration::from_secs(121);
        assert!(matches!(budget.remaining(), Err(TypedIoError::Resource(_))));
    }

    #[test]
    fn redirect_and_encoded_limits_are_typed_and_no_implicit_decode() {
        for status in [301, 302] {
            let server = Server::new(
                (0..10)
                    .map(|_| response(status, b"", "Location: /loop\r\n"))
                    .collect(),
            );
            let budget = Rc::new(RefCell::new(Budget::default()));
            let result = open(
                &client().unwrap(),
                &Url::parse(&server.url).unwrap(),
                budget,
            );
            assert!(matches!(result, Err(TypedIoError::Resource(_))));
            assert_eq!(server.count(), 9);
        }
        let server = Server::new(vec![
            response(200, b"data", ""),
            response(200, b"raw", "Content-Encoding: br\r\n"),
        ]);
        let budget = Rc::new(RefCell::new(Budget::default()));
        budget.borrow_mut().encoded = TOTAL_LIMIT;
        let mut stream = open(
            &client().unwrap(),
            &Url::parse(&server.url).unwrap(),
            budget,
        )
        .unwrap();
        assert!(matches!(
            io_error(stream.read(&mut [0; 1]).unwrap_err()),
            TypedIoError::Resource(_)
        ));
        let result = open(
            &client().unwrap(),
            &Url::parse(&server.url).unwrap(),
            Rc::new(RefCell::new(Budget::default())),
        );
        assert!(matches!(result, Err(TypedIoError::Unsupported(_))));
    }
}
