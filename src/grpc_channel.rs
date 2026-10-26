//! A minimal gRPC client over HTTP/2 for the REAPI services
//! (crate::remote_grpc, docs/REMOTE_CACHE.md).
//!
//! gRPC is HTTP/2 framing: a `POST /package.Service/Method` request with
//! `content-type: application/grpc` and `te: trailers`, messages as DATA
//! with a 5-byte prefix (compressed flag, big-endian length), and the
//! status in the `grpc-status`/`grpc-message` trailers (or in the headers of
//! a trailers-only response). The client speaks it directly over `h2` on a
//! private tokio runtime; callers block (`Handle::block_on`) like the HTTP
//! cache's reqwest blocking client.
//!
//! Pinned Bazel source (`src/main/java/com/google/devtools/build/lib/`):
//! `authandtls/GoogleAuthUtils.java:73-233` (targets, TLS, unix sockets),
//! `remote/ReferenceCountedChannel.java` and `remote/options/
//! RemoteOptions.java:57-88` (connections and concurrency per connection).
//! Execution (`Execution.Execute`/`WaitExecution`, server-streaming
//! `Operation`s) uses [`Channel::server_streaming`] like ByteStream.Read.

use bytes::{Buf, Bytes, BytesMut};
use std::future::{Future, poll_fn};
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::task::Poll;
use std::time::Duration;

/// A gRPC status code (`google.rpc.Code`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Code {
    Ok,
    Cancelled,
    Unknown,
    InvalidArgument,
    DeadlineExceeded,
    NotFound,
    AlreadyExists,
    PermissionDenied,
    ResourceExhausted,
    FailedPrecondition,
    Aborted,
    OutOfRange,
    Unimplemented,
    Internal,
    Unavailable,
    DataLoss,
    Unauthenticated,
}

const CODES: [(Code, &str); 17] = [
    (Code::Ok, "OK"),
    (Code::Cancelled, "CANCELLED"),
    (Code::Unknown, "UNKNOWN"),
    (Code::InvalidArgument, "INVALID_ARGUMENT"),
    (Code::DeadlineExceeded, "DEADLINE_EXCEEDED"),
    (Code::NotFound, "NOT_FOUND"),
    (Code::AlreadyExists, "ALREADY_EXISTS"),
    (Code::PermissionDenied, "PERMISSION_DENIED"),
    (Code::ResourceExhausted, "RESOURCE_EXHAUSTED"),
    (Code::FailedPrecondition, "FAILED_PRECONDITION"),
    (Code::Aborted, "ABORTED"),
    (Code::OutOfRange, "OUT_OF_RANGE"),
    (Code::Unimplemented, "UNIMPLEMENTED"),
    (Code::Internal, "INTERNAL"),
    (Code::Unavailable, "UNAVAILABLE"),
    (Code::DataLoss, "DATA_LOSS"),
    (Code::Unauthenticated, "UNAUTHENTICATED"),
];

impl Code {
    pub fn from_value(value: u64) -> Code {
        CODES
            .get(value as usize)
            .map_or(Code::Unknown, |(code, _)| *code)
    }

    pub fn value(self) -> u32 {
        CODES
            .iter()
            .position(|(code, _)| *code == self)
            .expect("listed") as u32
    }

    /// `Status.Code.name()`.
    pub fn name(self) -> &'static str {
        CODES[self.value() as usize].1
    }
}

/// A failed call: the status and, for transport and credential failures,
/// the message of the underlying `IOException` grpc-java attaches as the
/// status cause (what Bazel shows for capability failures).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Status {
    pub code: Code,
    pub message: String,
    pub cause: Option<String>,
    /// Whether the cause is a credential helper failure
    /// (`CredentialHelperException`, appended by Utils.grpcAwareErrorMessage).
    pub credential_helper: bool,
    /// A client-side failure that is not a gRPC status (an `IOException` in
    /// Bazel): shown as its message and never retried.
    pub local: bool,
}

impl Status {
    pub fn new(code: Code, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            cause: None,
            credential_helper: false,
            local: false,
        }
    }

    /// A client-side failure (see [`Status::local`]).
    pub fn local(message: impl Into<String>) -> Self {
        Self {
            local: true,
            ..Self::new(Code::Unknown, message)
        }
    }

    /// grpc-java's `Status.UNAVAILABLE.withDescription("io exception")`
    /// with the transport error as its cause (netty Utils.statusFromThrowable).
    pub fn io(cause: impl Into<String>) -> Self {
        Self {
            cause: Some(cause.into()),
            ..Self::new(Code::Unavailable, "io exception")
        }
    }

    /// The message Bazel reports for this failure as the cause of an
    /// IOException (GoogleChannelConnectionFactory: the IOException cause of
    /// a StatusRuntimeException, else the exception itself).
    pub fn cause_message(&self) -> String {
        self.cause.clone().unwrap_or_else(|| self.to_string())
    }
}

/// Utils.grpcAwareErrorMessage (remote/util/Utils.java:352-383): the code
/// name, the description, and a credential helper cause.
impl std::fmt::Display for Status {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.local {
            return f.write_str(&self.message);
        }
        write!(f, "{}: {}", self.code.name(), self.message)?;
        if self.credential_helper
            && let Some(cause) = &self.cause
        {
            write!(f, ": {cause}")?;
        }
        Ok(())
    }
}

/// Where a channel connects.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Address {
    Tcp { host: String, port: u16 },
    Unix(PathBuf),
}

/// A `--remote_cache`/`--remote_executor` gRPC target
/// (GoogleAuthUtils.newChannel): `grpc://` is plaintext, `grpcs://` and a
/// target without a scheme use TLS, `unix:PATH` is a plaintext Unix domain
/// socket.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Target {
    pub address: Address,
    pub tls: bool,
    /// The `:authority` (and the authority of credential helper URIs).
    pub authority: String,
}

impl Target {
    pub fn parse(text: &str) -> Result<Self, String> {
        // isTlsEnabled and convertTargetScheme are case-sensitive.
        let tls = !text.starts_with("grpc://") && !text.starts_with("unix:");
        if let Some(path) = text.strip_prefix("unix:") {
            if path.is_empty() {
                return Err(format!("invalid gRPC target '{text}': empty socket path"));
            }
            return Ok(Self {
                address: Address::Unix(PathBuf::from(path)),
                tls: false,
                authority: "localhost".into(),
            });
        }
        let rest = text.replace("grpcs://", "").replace("grpc://", "");
        if rest.contains("://") || rest.contains('/') || rest.is_empty() {
            return Err(format!(
                "invalid gRPC target '{text}': only grpc://HOST[:PORT], grpcs://HOST[:PORT], HOST[:PORT] and unix:PATH are modelled"
            ));
        }
        let (host, port) = if let Some(bracketed) = rest.strip_prefix('[') {
            let (host, tail) = bracketed
                .split_once(']')
                .ok_or_else(|| format!("invalid gRPC target '{text}': unterminated IPv6 host"))?;
            (host.to_owned(), tail.strip_prefix(':'))
        } else {
            match rest.rsplit_once(':') {
                Some((host, port)) => (host.to_owned(), Some(port)),
                None => (rest.clone(), None),
            }
        };
        // NettyChannelBuilder.getDefaultPort: 443 with TLS, else 80.
        let port = match port {
            None => {
                if tls {
                    443
                } else {
                    80
                }
            }
            Some(digits) => digits
                .parse::<u16>()
                .map_err(|_| format!("invalid gRPC target '{text}': malformed port"))?,
        };
        if host.is_empty() {
            return Err(format!("invalid gRPC target '{text}': no host"));
        }
        let authority = if host.contains(':') {
            format!("[{host}]:{port}")
        } else {
            format!("{host}:{port}")
        };
        Ok(Self {
            address: Address::Tcp { host, port },
            tls,
            authority,
        })
    }
}

/// `--tls_certificate`, `--tls_client_certificate`, `--tls_client_key`,
/// `--tls_authority_override` (authandtls/AuthAndTLSOptions.java).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TlsOptions {
    pub certificate: Option<PathBuf>,
    pub client_certificate: Option<PathBuf>,
    pub client_key: Option<PathBuf>,
    pub authority_override: Option<String>,
}

/// GoogleAuthUtils.createSSlContext (:173-208): the root certificate file
/// replaces the default trust store; a client certificate needs its key.
fn tls_config(options: &TlsOptions) -> Result<Arc<rustls::ClientConfig>, String> {
    use rustls_pki_types::pem::PemObject as _;
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let mut roots = rustls::RootCertStore::empty();
    match &options.certificate {
        Some(path) => {
            let failed = |error: String| {
                format!(
                    "Failed to init TLS infrastructure using '{}' as root certificate: {error}",
                    path.display()
                )
            };
            let certificates = rustls_pki_types::CertificateDer::pem_file_iter(path)
                .and_then(|certificates| certificates.collect::<Result<Vec<_>, _>>())
                .map_err(|error| failed(error.to_string()))?;
            if certificates.is_empty() {
                return Err(failed("no certificates found".into()));
            }
            for certificate in certificates {
                roots
                    .add(certificate)
                    .map_err(|error| failed(error.to_string()))?;
            }
        }
        None => {
            let native = rustls_native_certs::load_native_certs();
            for certificate in native.certs {
                let _ = roots.add(certificate);
            }
        }
    }
    let builder = rustls::ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .map_err(|error| format!("Failed to init TLS infrastructure: {error}"))?
        .with_root_certificates(roots);
    let mut config = match (&options.client_certificate, &options.client_key) {
        (Some(certificate), Some(key)) => {
            let failed = |error: String| {
                format!(
                    "Failed to init TLS infrastructure using '{}' as client certificate: {error}",
                    certificate.display()
                )
            };
            let chain = rustls_pki_types::CertificateDer::pem_file_iter(certificate)
                .and_then(|certificates| certificates.collect::<Result<Vec<_>, _>>())
                .map_err(|error| failed(error.to_string()))?;
            let key = rustls_pki_types::PrivateKeyDer::from_pem_file(key)
                .map_err(|error| failed(error.to_string()))?;
            builder
                .with_client_auth_cert(chain, key)
                .map_err(|error| failed(error.to_string()))?
        }
        _ => builder.with_no_client_auth(),
    };
    config.alpn_protocols = vec![b"h2".to_vec()];
    Ok(Arc::new(config))
}

/// The channel's connection limits (RemoteModule.java:623-630).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Limits {
    /// `--remote_max_connections` (0: unlimited).
    pub max_connections: usize,
    /// `--remote_max_concurrency_per_connection`.
    pub max_concurrency_per_connection: usize,
    /// Connection timeout (`--remote_timeout`; zero: none).
    pub connect_timeout: Duration,
}

struct Connection {
    send: h2::client::SendRequest<Bytes>,
    in_flight: Arc<AtomicUsize>,
    alive: Arc<AtomicBool>,
}

/// Counts a call against its connection while it runs; `alive` retires
/// the connection after a transport error.
struct InFlight(Arc<AtomicUsize>, Arc<AtomicBool>);

impl InFlight {
    fn retire(&self) {
        self.1.store(false, Ordering::Relaxed);
    }
}

impl Drop for InFlight {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::Relaxed);
    }
}

trait Io: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send {}
impl<T: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send> Io for T {}

/// One call: the method path (`/package.Service/Method`), metadata (ASCII
/// values; `-bin` names carry base64), and the deadline.
pub struct Call<'a> {
    pub path: &'a str,
    pub metadata: &'a [(String, String)],
    pub timeout: Option<Duration>,
}

/// A gRPC channel: a pool of HTTP/2 connections to one target.
pub struct Channel {
    runtime: tokio::runtime::Runtime,
    target: Target,
    tls_options: TlsOptions,
    tls: OnceLock<Result<Arc<rustls::ClientConfig>, String>>,
    limits: Limits,
    connections: Mutex<Vec<Connection>>,
    user_agent: String,
}

impl std::fmt::Debug for Channel {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Channel")
            .field("target", &self.target)
            .finish()
    }
}

fn h2_status(error: h2::Error) -> Status {
    if let Some(reason) = error.reason() {
        // The gRPC HTTP/2 mapping of RST_STREAM/GOAWAY error codes.
        let code = match reason {
            h2::Reason::REFUSED_STREAM => Code::Unavailable,
            h2::Reason::CANCEL => Code::Cancelled,
            h2::Reason::ENHANCE_YOUR_CALM => Code::ResourceExhausted,
            h2::Reason::INADEQUATE_SECURITY => Code::PermissionDenied,
            _ => Code::Internal,
        };
        if !error.is_io() {
            return Status::new(code, format!("HTTP/2 error code: {reason:?}: {error}"));
        }
    }
    Status::io(error.to_string())
}

/// The gRPC status of a non-200 HTTP response (gRPC's HTTP to gRPC status
/// mapping).
fn http_status(status: http::StatusCode) -> Status {
    let code = match status.as_u16() {
        400 => Code::Internal,
        401 => Code::Unauthenticated,
        403 => Code::PermissionDenied,
        404 => Code::Unimplemented,
        429 | 502..=504 => Code::Unavailable,
        _ => Code::Unknown,
    };
    Status::new(code, format!("HTTP status code {}", status.as_u16()))
}

/// grpc-message is percent-encoded UTF-8.
fn percent_decode(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%'
            && index + 2 < bytes.len()
            && let Some(value) = std::str::from_utf8(&bytes[index + 1..index + 3])
                .ok()
                .and_then(|hex| u8::from_str_radix(hex, 16).ok())
        {
            out.push(value);
            index += 3;
            continue;
        }
        out.push(bytes[index]);
        index += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// The status of trailers (or of a trailers-only response's headers);
/// `None` without `grpc-status`.
fn trailer_status(headers: &http::HeaderMap) -> Option<Status> {
    let code = headers.get("grpc-status")?.to_str().ok()?;
    let code = code
        .parse::<u64>()
        .map(Code::from_value)
        .unwrap_or(Code::Unknown);
    let message = headers
        .get("grpc-message")
        .and_then(|value| value.to_str().ok())
        .map(percent_decode)
        .unwrap_or_default();
    Some(Status::new(code, message))
}

/// A length-prefixed gRPC message.
fn frame(message: &[u8]) -> Bytes {
    let mut out = BytesMut::with_capacity(message.len() + 5);
    out.extend_from_slice(&[0]);
    out.extend_from_slice(&(message.len() as u32).to_be_bytes());
    out.extend_from_slice(message);
    out.freeze()
}

/// The next complete message of `buffer`, if any.
fn take_message(buffer: &mut BytesMut) -> Result<Option<Bytes>, Status> {
    if buffer.len() < 5 {
        return Ok(None);
    }
    if buffer[0] != 0 {
        return Err(Status::new(
            Code::Internal,
            "Can't decode compressed gRPC message as compression not configured",
        ));
    }
    let length = u32::from_be_bytes([buffer[1], buffer[2], buffer[3], buffer[4]]) as usize;
    if buffer.len() < 5 + length {
        return Ok(None);
    }
    buffer.advance(5);
    Ok(Some(buffer.split_to(length).freeze()))
}

enum SendEvent {
    Capacity,
    Response(Result<http::Response<h2::RecvStream>, h2::Error>),
}

/// The deadline format of the `grpc-timeout` header (milliseconds).
fn grpc_timeout(timeout: Duration) -> String {
    format!("{}m", timeout.as_millis().clamp(1, 99_999_999))
}

impl Channel {
    pub fn new(
        target: Target,
        tls_options: TlsOptions,
        limits: Limits,
        user_agent: String,
    ) -> Result<Self, String> {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .thread_name("bazel-rs-grpc")
            .enable_all()
            .build()
            .map_err(|error| format!("cannot start the gRPC runtime: {error}"))?;
        Ok(Self {
            runtime,
            target,
            tls_options,
            tls: OnceLock::new(),
            limits,
            connections: Mutex::new(Vec::new()),
            user_agent,
        })
    }

    pub fn target(&self) -> &Target {
        &self.target
    }

    async fn connect_io(&self) -> Result<Box<dyn Io>, Status> {
        match &self.target.address {
            Address::Unix(path) => tokio::net::UnixStream::connect(path)
                .await
                .map(|stream| Box::new(stream) as Box<dyn Io>)
                .map_err(|error| {
                    Status::io(format!(
                        "connect(..) failed: {}: {}",
                        error.kind(),
                        path.display()
                    ))
                }),
            Address::Tcp { host, port } => {
                let addresses = tokio::net::lookup_host((host.as_str(), *port))
                    .await
                    .map_err(|error| Status::io(format!("Unable to resolve host {host}: {error}")))?
                    .collect::<Vec<_>>();
                let mut last = Status::io(format!("Unable to resolve host {host}"));
                let mut connected = None;
                for address in addresses {
                    match tokio::net::TcpStream::connect(address).await {
                        Ok(stream) => {
                            connected = Some(stream);
                            break;
                        }
                        Err(error) => {
                            // Netty's AbstractEpollChannel: `finishConnect(..)
                            // failed: Connection refused: HOST/IP:PORT`.
                            let shown = if host.parse::<std::net::IpAddr>().is_ok() {
                                format!("/{address}")
                            } else {
                                format!("{host}/{address}")
                            };
                            let reason = match error.kind() {
                                std::io::ErrorKind::ConnectionRefused => {
                                    "Connection refused".to_owned()
                                }
                                _ => error.to_string(),
                            };
                            last =
                                Status::io(format!("finishConnect(..) failed: {reason}: {shown}"));
                        }
                    }
                }
                let stream = connected.ok_or(last)?;
                let _ = stream.set_nodelay(true);
                if !self.target.tls {
                    return Ok(Box::new(stream));
                }
                let config = self
                    .tls
                    .get_or_init(|| tls_config(&self.tls_options))
                    .clone()
                    .map_err(Status::io)?;
                let name = self
                    .tls_options
                    .authority_override
                    .clone()
                    .unwrap_or_else(|| host.clone());
                let server_name =
                    rustls_pki_types::ServerName::try_from(name.clone()).map_err(|error| {
                        Status::io(format!("invalid TLS server name {name}: {error}"))
                    })?;
                let stream = tokio_rustls::TlsConnector::from(config)
                    .connect(server_name, stream)
                    .await
                    .map_err(|error| Status::io(error.to_string()))?;
                if stream.get_ref().1.alpn_protocol() != Some(b"h2") {
                    return Err(Status::io(
                        "Failed ALPN negotiation: Unable to find compatible protocol",
                    ));
                }
                Ok(Box::new(stream))
            }
        }
    }

    async fn connect(&self) -> Result<Connection, Status> {
        let connecting = self.connect_io();
        let io = if self.limits.connect_timeout.is_zero() {
            connecting.await?
        } else {
            tokio::time::timeout(self.limits.connect_timeout, connecting)
                .await
                .map_err(|_| {
                    Status::io(format!(
                        "connection timed out after {} ms: {}",
                        self.limits.connect_timeout.as_millis(),
                        self.target.authority
                    ))
                })??
        };
        let (send, connection) = h2::client::Builder::new()
            .initial_window_size(1 << 20)
            .initial_connection_window_size(1 << 24)
            .max_frame_size(1 << 16)
            .handshake::<_, Bytes>(io)
            .await
            .map_err(h2_status)?;
        let alive = Arc::new(AtomicBool::new(true));
        let watch = alive.clone();
        self.runtime.spawn(async move {
            let _ = connection.await;
            watch.store(false, Ordering::Relaxed);
        });
        Ok(Connection {
            send,
            in_flight: Arc::new(AtomicUsize::new(0)),
            alive,
        })
    }

    /// A connection with spare concurrency, else a new one within
    /// `--remote_max_connections`, else the least loaded.
    async fn acquire(&self) -> Result<(h2::client::SendRequest<Bytes>, InFlight), Status> {
        let pick = |connections: &mut Vec<Connection>, any: bool| {
            connections.retain(|connection| connection.alive.load(Ordering::Relaxed));
            let full = self.limits.max_connections != 0
                && connections.len() >= self.limits.max_connections;
            let chosen = connections
                .iter()
                .filter(|connection| {
                    any || full
                        || self.limits.max_concurrency_per_connection == 0
                        || connection.in_flight.load(Ordering::Relaxed)
                            < self.limits.max_concurrency_per_connection
                })
                .min_by_key(|connection| connection.in_flight.load(Ordering::Relaxed))?;
            chosen.in_flight.fetch_add(1, Ordering::Relaxed);
            Some((
                chosen.send.clone(),
                InFlight(chosen.in_flight.clone(), chosen.alive.clone()),
            ))
        };
        if let Some(found) = pick(&mut self.connections.lock().expect("connections"), false) {
            return Ok(found);
        }
        let connection = self.connect().await?;
        let mut connections = self.connections.lock().expect("connections");
        connections.push(connection);
        Ok(pick(&mut connections, true).expect("a live connection was just added"))
    }

    fn request(&self, call: &Call<'_>) -> Result<http::Request<()>, Status> {
        let scheme = if self.target.tls { "https" } else { "http" };
        let authority = self
            .tls_options
            .authority_override
            .as_deref()
            .filter(|_| self.target.tls)
            .unwrap_or(&self.target.authority);
        let mut builder = http::Request::builder()
            .method("POST")
            .uri(format!("{scheme}://{authority}{}", call.path))
            .header("content-type", "application/grpc")
            .header("te", "trailers")
            .header("user-agent", &self.user_agent);
        if let Some(timeout) = call.timeout {
            builder = builder.header("grpc-timeout", grpc_timeout(timeout));
        }
        for (name, value) in call.metadata {
            builder = builder.header(name.to_ascii_lowercase(), value);
        }
        builder
            .body(())
            .map_err(|error| Status::new(Code::Internal, format!("invalid gRPC request: {error}")))
    }

    /// Sends `requests` (stopping early if the server answers first), then
    /// hands every response message to `on_message`. Returns whether every
    /// request was sent.
    async fn exchange(
        &self,
        call: &Call<'_>,
        requests: &mut dyn Iterator<Item = Vec<u8>>,
        on_message: &mut dyn FnMut(Bytes) -> Result<(), Status>,
    ) -> Result<bool, Status> {
        let request = self.request(call)?;
        let (send, in_flight) = self.acquire().await?;
        // A connection that cannot start a stream (GOAWAY, a broken
        // transport) is not used again; the retry opens a new one.
        let retire = |error: h2::Error| {
            in_flight.retire();
            h2_status(error)
        };
        let mut send = send.ready().await.map_err(retire)?;
        let (response, mut stream) = send.send_request(request, false).map_err(retire)?;
        let mut response = std::pin::pin!(response);
        let mut early = None;
        let mut next = requests.next();
        if next.is_none() {
            stream.send_data(Bytes::new(), true).map_err(h2_status)?;
        }
        let mut all_sent = true;
        while let Some(message) = next.take() {
            let following = requests.next();
            // Bounded buffering (like hyper's PipeToSendStream): wait for
            // send capacity, then hand h2 one message.
            stream.reserve_capacity(1);
            let event = poll_fn(|cx| {
                if let Poll::Ready(result) = response.as_mut().poll(cx) {
                    return Poll::Ready(Ok(SendEvent::Response(result)));
                }
                if stream.capacity() > 0 {
                    return Poll::Ready(Ok(SendEvent::Capacity));
                }
                loop {
                    match stream.poll_capacity(cx) {
                        Poll::Ready(Some(Ok(0))) => continue,
                        Poll::Ready(Some(Ok(_))) => return Poll::Ready(Ok(SendEvent::Capacity)),
                        Poll::Ready(Some(Err(error))) => return Poll::Ready(Err(error)),
                        // The stream closed: its response explains why.
                        Poll::Ready(None) => return Poll::Pending,
                        Poll::Pending => return Poll::Pending,
                    }
                }
            })
            .await
            .map_err(h2_status)?;
            match event {
                SendEvent::Capacity => {
                    if let Err(error) = stream.send_data(frame(&message), following.is_none()) {
                        if error.is_reset() || error.is_remote() || error.is_go_away() {
                            // The server ended the call; read its status.
                            all_sent = false;
                            break;
                        }
                        return Err(h2_status(error));
                    }
                }
                SendEvent::Response(result) => {
                    early = Some(result);
                    all_sent = false;
                    break;
                }
            }
            next = following;
        }
        let response = match early {
            Some(result) => result,
            None => response.await,
        }
        .map_err(h2_status)?;
        let (parts, mut body) = response.into_parts();
        if parts.status != http::StatusCode::OK {
            if let Some(status) = trailer_status(&parts.headers)
                && status.code != Code::Ok
            {
                return Err(status);
            }
            return Err(http_status(parts.status));
        }
        if let Some(status) = trailer_status(&parts.headers) {
            // Trailers-only.
            if !all_sent {
                stream.send_reset(h2::Reason::CANCEL);
            }
            return if status.code == Code::Ok {
                Ok(all_sent)
            } else {
                Err(status)
            };
        }
        let mut buffer = BytesMut::new();
        while let Some(chunk) = body.data().await {
            let chunk = chunk.map_err(h2_status)?;
            let _ = body.flow_control().release_capacity(chunk.len());
            buffer.extend_from_slice(&chunk);
            while let Some(message) = take_message(&mut buffer)? {
                if let Err(status) = on_message(message) {
                    stream.send_reset(h2::Reason::CANCEL);
                    return Err(status);
                }
            }
        }
        let trailers = body.trailers().await.map_err(h2_status)?;
        if !all_sent {
            stream.send_reset(h2::Reason::CANCEL);
        }
        let status = trailers
            .as_ref()
            .and_then(trailer_status)
            .unwrap_or_else(|| Status::new(Code::Internal, "missing grpc-status trailer"));
        if status.code != Code::Ok {
            return Err(status);
        }
        if !buffer.is_empty() {
            return Err(Status::new(Code::Internal, "truncated gRPC message"));
        }
        Ok(all_sent)
    }

    fn run(
        &self,
        call: &Call<'_>,
        requests: &mut dyn Iterator<Item = Vec<u8>>,
        on_message: &mut dyn FnMut(Bytes) -> Result<(), Status>,
    ) -> Result<bool, Status> {
        let exchange = self.exchange(call, requests, on_message);
        let exchange: Pin<&mut dyn Future<Output = Result<bool, Status>>> =
            std::pin::pin!(exchange);
        match call.timeout {
            None => self.runtime.block_on(exchange),
            Some(timeout) => self
                .runtime
                .block_on(async { tokio::time::timeout(timeout, exchange).await })
                .unwrap_or_else(|_| {
                    Err(Status::new(
                        Code::DeadlineExceeded,
                        format!("deadline exceeded after {}s", timeout.as_secs_f64()),
                    ))
                }),
        }
    }

    /// A unary call: one request, one response.
    pub fn unary(&self, call: &Call<'_>, request: Vec<u8>) -> Result<Bytes, Status> {
        let mut response = None;
        let mut extra = false;
        self.run(call, &mut std::iter::once(request), &mut |message| {
            if response.is_some() {
                extra = true;
            }
            response = Some(message);
            Ok(())
        })?;
        if extra {
            return Err(Status::new(Code::Internal, "Too many responses"));
        }
        response.ok_or_else(|| Status::new(Code::Internal, "No value received for unary call"))
    }

    /// A server-streaming call (ByteStream.Read; Execution.Execute and
    /// WaitExecution): `on_message` may stop the call with a status.
    pub fn server_streaming(
        &self,
        call: &Call<'_>,
        request: Vec<u8>,
        on_message: &mut dyn FnMut(Bytes) -> Result<(), Status>,
    ) -> Result<(), Status> {
        self.run(call, &mut std::iter::once(request), on_message)
            .map(|_| ())
    }

    /// A client-streaming call (ByteStream.Write): the response, if the
    /// server sent one, and whether every request was sent before the
    /// server completed the call.
    pub fn client_streaming(
        &self,
        call: &Call<'_>,
        requests: &mut dyn Iterator<Item = Vec<u8>>,
    ) -> Result<(Option<Bytes>, bool), Status> {
        let mut response = None;
        let all_sent = self.run(call, requests, &mut |message| {
            response = Some(message);
            Ok(())
        })?;
        Ok((response, all_sent))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn targets_follow_google_auth_utils() {
        let plain = Target::parse("grpc://127.0.0.1:8980").unwrap();
        assert!(!plain.tls);
        assert_eq!(plain.authority, "127.0.0.1:8980");
        let tls = Target::parse("grpcs://cache.example.com").unwrap();
        assert!(tls.tls);
        assert_eq!(tls.authority, "cache.example.com:443");
        let bare = Target::parse("cache.example.com:9092").unwrap();
        assert!(bare.tls);
        assert_eq!(
            bare.address,
            Address::Tcp {
                host: "cache.example.com".into(),
                port: 9092
            }
        );
        assert_eq!(
            Target::parse("grpc://[::1]:7").unwrap().authority,
            "[::1]:7"
        );
        assert_eq!(Target::parse("grpc://h").unwrap().authority, "h:80");
        let unix = Target::parse("unix:/run/cache.sock").unwrap();
        assert_eq!(unix.address, Address::Unix("/run/cache.sock".into()));
        assert!(!unix.tls);
        assert!(Target::parse("grpc://h:1/x").is_err());
        assert!(Target::parse("dns:///h:1").is_err());
        assert!(Target::parse("grpc://h:x").is_err());
    }

    #[test]
    fn status_texts_follow_grpc_aware_error_message() {
        assert_eq!(
            Status::new(Code::NotFound, "missing").to_string(),
            "NOT_FOUND: missing"
        );
        assert_eq!(Code::from_value(16), Code::Unauthenticated);
        assert_eq!(Code::from_value(99), Code::Unknown);
        let helper = Status {
            cause: Some("helper failed".into()),
            credential_helper: true,
            ..Status::new(
                Code::Unauthenticated,
                "Failed computing credential metadata",
            )
        };
        assert_eq!(
            helper.to_string(),
            "UNAUTHENTICATED: Failed computing credential metadata: helper failed"
        );
        assert_eq!(helper.cause_message(), "helper failed");
        assert_eq!(percent_decode("a%20b%zz%2"), "a b%zz%2");
    }

    #[test]
    fn messages_are_length_prefixed() {
        let mut buffer = BytesMut::from(&frame(b"abc")[..]);
        buffer.extend_from_slice(&frame(b"")[..2]);
        assert_eq!(take_message(&mut buffer).unwrap().unwrap(), &b"abc"[..]);
        assert_eq!(take_message(&mut buffer).unwrap(), None);
        let mut compressed = BytesMut::from(&[1u8, 0, 0, 0, 0][..]);
        assert_eq!(
            take_message(&mut compressed).unwrap_err().code,
            Code::Internal
        );
        assert_eq!(grpc_timeout(Duration::from_secs(60)), "60000m");
    }
}
