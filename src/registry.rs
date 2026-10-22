//! Registry identity and uncached metadata/source transport. Metadata is
//! returned as bytes (inherently in memory); source downloads stream into a
//! caller-owned writer ([`download_to`]). Neither has an artificial size cap.
//! Per-invocation caching, lockfile hashes and the registry file cache live
//! in registry_session.
//!
//! Local registry trees are trusted not to change concurrently with reads;
//! symlink checks have the same boundary as the resolver's original reader.

use reqwest::{Url, blocking::Client, redirect::Policy};
use serde_json::{Map, Value};
use std::fmt;
use std::io::{Read, Write};
use std::path::{Component, Path, PathBuf};
use std::sync::LazyLock;
use std::time::Duration;

/// Bazel 9.3's registry list when no --registry is given
/// (BazelRepositoryModule.java:133, used when RepositoryOptions.registries
/// is null, RepositoryOptions.java:100-110).
pub const DEFAULT_REGISTRY: &str = "https://bcr.bazel.build/";

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Registry {
    LocalPath(PathBuf),
    Url(String),
}

impl Registry {
    pub fn parse(input: &str) -> Result<Self, String> {
        if input.starts_with("file://") {
            return file_path(input).map(Self::LocalPath);
        }
        let mut url = http_url(input)?;
        if url.query().is_some() || url.fragment().is_some() {
            return Err("registry URL must not contain a query or fragment".into());
        }
        // Append rather than replace a path segment: /registry is a base path,
        // not a resource whose name should disappear when joining a resource.
        if !url.path().ends_with('/') {
            url.path_segments_mut()
                .map_err(|_| "registry URL cannot be a base URL")?
                .push("");
        }
        Ok(Self::Url(url.into()))
    }

    /// Bazel keys lockfile entries by the registry URL as given plus
    /// `/modules/...` (IndexRegistry.constructUrl, :134-143). Native keys use
    /// the normalized URL, so a lockfile requires the two to agree.
    pub fn check_lockfile_spelling(&self, raw: &str) -> Result<(), String> {
        match self {
            Self::LocalPath(_) => Ok(()),
            Self::Url(base) if base == raw || *base == format!("{raw}/") => Ok(()),
            Self::Url(base) => Err(format!(
                "registry URL {raw:?} is not in normalized form ({base:?}); lockfile keys would differ from Bazel's, so spell the registry as {:?} or use --lockfile_mode=off",
                base.trim_end_matches('/')
            )),
        }
    }

    /// The registry as Bazel's messages spell IndexRegistry.getUrl() for the
    /// usual spelling without a trailing slash.
    pub fn bazel_url(&self) -> String {
        match self {
            Self::LocalPath(path) => format!("file://{}", path.display()),
            Self::Url(base) => base.trim_end_matches('/').to_owned(),
        }
    }

    pub fn local_path(&self) -> Option<&Path> {
        match self {
            Self::LocalPath(path) => Some(path),
            Self::Url(_) => None,
        }
    }

    /// A diagnostic location, never an unchecked input to a transport.
    pub fn location(&self, relative: &str) -> String {
        if validate_relative(relative).is_err() {
            return format!("{self}/<invalid registry resource>");
        }
        match self {
            Self::LocalPath(path) => path.join(relative).display().to_string(),
            Self::Url(base) => resource_url(base, relative)
                .map(|url| url.to_string())
                .unwrap_or_else(|_| "<invalid registry URL>".into()),
        }
    }

    pub fn read(&self, relative: &str) -> Result<Option<Vec<u8>>, String> {
        validate_relative(relative)?;
        match self {
            Self::LocalPath(root) => {
                if !no_symlinks(root)? {
                    return Ok(None);
                }
                let metadata = crate::observed_fs::metadata(root).map_err(|e| {
                    format!("cannot inspect local registry {}: {e}", root.display())
                })?;
                if !metadata.is_dir() {
                    return Err(format!(
                        "local registry is not a directory: {}",
                        root.display()
                    ));
                }
                read_local(&root.join(relative))
            }
            Self::Url(base) => read_http(resource_url(base, relative)?),
        }
    }

    /// A validated transport URL, unlike the infallible diagnostic `location`.
    pub fn download_location(&self, relative: &str) -> Result<String, String> {
        validate_relative(relative)?;
        match self {
            Self::LocalPath(root) => {
                let path = root.join(relative);
                let absolute = if path.is_absolute() {
                    path
                } else {
                    std::env::current_dir()
                        .map_err(|e| e.to_string())?
                        .join(path)
                };
                Url::from_file_path(absolute)
                    .map(|url| url.to_string())
                    .map_err(|_| "invalid local registry resource path".into())
            }
            Self::Url(base) => resource_url(base, relative).map(|url| url.to_string()),
        }
    }

    pub fn read_text(&self, relative: &str) -> Result<Option<String>, String> {
        self.read(relative)?
            .map(|bytes| {
                String::from_utf8(bytes).map_err(|_| {
                    format!("registry entry is not UTF-8: {}", self.location(relative))
                })
            })
            .transpose()
    }

    /// Blank metadata is absent, matching IndexRegistry.grabJson.
    pub fn read_object(&self, relative: &str) -> Result<Option<Map<String, Value>>, String> {
        let Some(text) = self.read_text(relative)? else {
            return Ok(None);
        };
        if text.trim().is_empty() {
            return Ok(None);
        }
        let value: Value = serde_json::from_str(&text).map_err(|error| {
            format!("invalid registry JSON {}: {error}", self.location(relative))
        })?;
        match value {
            Value::Object(object) => Ok(Some(object)),
            _ => Err(format!(
                "registry metadata must be a JSON object: {}",
                self.location(relative)
            )),
        }
    }
}

impl From<PathBuf> for Registry {
    fn from(path: PathBuf) -> Self {
        Self::LocalPath(path)
    }
}

impl fmt::Display for Registry {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::LocalPath(path) => path.display().fmt(f),
            Self::Url(url) => match Self::parse(url) {
                Ok(Self::Url(url)) => f.write_str(&url),
                _ => f.write_str("<invalid registry URL>"),
            },
        }
    }
}

fn validate_relative(relative: &str) -> Result<(), String> {
    if relative.is_empty()
        || relative.chars().any(|c| c.is_control())
        || relative.contains(['\\', '?', '#', '%'])
        || relative
            .split('/')
            .any(|c| c.is_empty() || c == "." || c == "..")
        || Path::new(relative)
            .components()
            .any(|c| !matches!(c, Component::Normal(_)))
    {
        return Err("invalid registry resource: expected normal relative path components".into());
    }
    Ok(())
}

fn resource_url(base: &str, relative: &str) -> Result<Url, String> {
    validate_relative(relative)?;
    let Registry::Url(base) = Registry::parse(base)? else {
        return Err("expected an HTTP(S) registry URL".into());
    };
    let mut url = http_url(&base)?;
    url.path_segments_mut()
        .map_err(|_| "registry URL cannot be a base URL")?
        .pop_if_empty()
        .extend(relative.split('/'));
    Ok(url)
}

fn http_url(input: &str) -> Result<Url, String> {
    // URL parsers otherwise normalize backslashes and discard some controls.
    if input.chars().any(|c| c.is_control() || c.is_whitespace()) || input.contains('\\') {
        return Err("invalid HTTP(S) URL".into());
    }
    let url = Url::parse(input).map_err(|_| "invalid HTTP(S) URL")?;
    if !matches!(url.scheme(), "http" | "https") || url.host_str().is_none() {
        return Err("unsupported URL: expected HTTP(S) or an absolute local file:// URL".into());
    }
    // Also reject syntactically empty userinfo (http://@host).
    if !url.username().is_empty()
        || url.password().is_some()
        || input.split_once("://").is_none_or(|(_, rest)| {
            rest.split(['/', '?', '#'])
                .next()
                .is_some_and(|authority| authority.contains('@'))
        })
    {
        return Err("URL credentials are unsupported".into());
    }
    Ok(url)
}

fn file_path(input: &str) -> Result<PathBuf, String> {
    let rest = input
        .strip_prefix("file://")
        .ok_or("expected a file:// URL")?;
    let rest = rest.strip_prefix("localhost").unwrap_or(rest);
    if !rest.starts_with('/') || rest.starts_with("//") || rest.contains(['?', '#', '\\']) {
        return Err(
            "unsupported file URL: expected an absolute local path without query or fragment"
                .into(),
        );
    }
    let mut decoded = Vec::new();
    let mut bytes = rest.bytes();
    while let Some(byte) = bytes.next() {
        if byte == b'%' {
            let hi = bytes.next().and_then(|b| char::from(b).to_digit(16));
            let lo = bytes.next().and_then(|b| char::from(b).to_digit(16));
            match (hi, lo) {
                (Some(hi), Some(lo)) => decoded.push((hi * 16 + lo) as u8),
                _ => return Err("invalid percent encoding in file URL".into()),
            }
        } else {
            decoded.push(byte);
        }
    }
    let path = String::from_utf8(decoded).map_err(|_| "non-UTF-8 file URL is unsupported")?;
    if path.chars().any(|c| c.is_control()) || path.contains('\\') || path.starts_with("//") {
        return Err("unsupported character in file URL".into());
    }
    Ok(PathBuf::from(path))
}

fn no_symlinks(path: &Path) -> Result<bool, String> {
    let mut current = PathBuf::new();
    for component in path.components() {
        current.push(component.as_os_str());
        match crate::observed_fs::symlink_metadata(&current) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                return Err(format!(
                    "symlink registry paths are unsupported: {}",
                    current.display()
                ));
            }
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
            Err(error) => {
                return Err(format!(
                    "cannot inspect registry path {}: {error}",
                    current.display()
                ));
            }
        }
    }
    Ok(true)
}

/// A local registry file's bytes (`open_local` then `read_all`, recorded
/// for the analysis snapshot through crate::observed_fs).
fn read_local(path: &Path) -> Result<Option<Vec<u8>>, String> {
    if !local_regular_file(path)? {
        return Ok(None);
    }
    match crate::observed_fs::read_staged(path) {
        Ok(bytes) => Ok(Some(bytes)),
        Err(crate::observed_fs::ReadError::Open(error)) => {
            Err(format!("cannot read {}: {error}", path.display()))
        }
        Err(crate::observed_fs::ReadError::Read(_)) => {
            Err("cannot read complete resource body".to_owned())
        }
    }
}

/// `Ok(false)` when a path component is missing; an error unless every
/// component is a non-symlink and the path a regular file.
fn local_regular_file(path: &Path) -> Result<bool, String> {
    if !no_symlinks(path)? {
        return Ok(false);
    }
    let metadata = crate::observed_fs::metadata(path)
        .map_err(|error| format!("cannot inspect {}: {error}", path.display()))?;
    if !metadata.is_file() {
        return Err(format!(
            "registry entry is not a regular file: {}",
            path.display()
        ));
    }
    Ok(true)
}

fn open_local(path: &Path) -> Result<Option<std::fs::File>, String> {
    if !local_regular_file(path)? {
        return Ok(None);
    }
    // snapshot-unrecorded: a file:// archive download (download_to), which taints the run.
    std::fs::File::open(path)
        .map(Some)
        .map_err(|error| format!("cannot read {}: {error}", path.display()))
}

fn redirect_allowed(target: &Url, previous: &[Url]) -> bool {
    matches!(target.scheme(), "http" | "https")
        && target.host_str().is_some()
        && target.username().is_empty()
        && target.password().is_none()
        && !(target.scheme() == "http" && previous.iter().any(|url| url.scheme() == "https"))
}

static CLIENT: LazyLock<Result<Client, String>> =
    LazyLock::new(|| client_with(crate::extension_download::READ_TIMEOUT));

/// The registry client. Timeouts are Bazel's HttpConnector values shared with
/// the repository transport (10 s connect, `read_timeout` per operation,
/// HttpConnector.java:60-61, 155-157). The blocking client's builder-level
/// `timeout` applies to sending the request and to each body read, never to
/// the whole transfer; no request-level `.timeout()` is set anywhere (in
/// reqwest 0.12 that would be a whole-transfer deadline, TotalTimeoutBody).
fn client_with(read_timeout: Duration) -> Result<Client, String> {
    // ProxyHelper over the command's repository environment
    // (crate::download_proxy; IndexRegistry.java:235 passes nonstrictRepoEnv).
    crate::download_proxy::configure(Client::builder().no_proxy())
        .user_agent("bazel-rs/0.1")
        .referer(false)
        .connect_timeout(crate::extension_download::CONNECT_TIMEOUT)
        .timeout(read_timeout)
        // Explicit even if another dependency enables these reqwest features.
        .no_gzip()
        .no_brotli()
        .no_deflate()
        .no_zstd()
        .redirect(Policy::custom(|attempt| {
            if attempt.previous().len() > 10 {
                attempt.error("too many HTTP redirects")
            } else if !redirect_allowed(attempt.url(), attempt.previous()) {
                attempt.error("unsafe HTTP redirect")
            } else if let Err(error) = crate::download_proxy::check(attempt.url()) {
                // HttpConnector asks ProxyHelper again for every redirect
                // (HttpConnector.java:125-136).
                attempt.error(error)
            } else {
                attempt.follow()
            }
        }))
        .build()
        .map_err(|_| "cannot initialize HTTP transport".into())
}

fn read_http(url: Url) -> Result<Option<Vec<u8>>, String> {
    open_http(url)?.map(read_all).transpose()
}

fn open_http(url: Url) -> Result<Option<reqwest::blocking::Response>, String> {
    open_http_with(CLIENT.as_ref().map_err(Clone::clone)?, url)
}

fn open_http_with(
    client: &Client,
    url: Url,
) -> Result<Option<reqwest::blocking::Response>, String> {
    crate::observed_fs::taint(crate::observed_fs::Taint::Network, || url.to_string());
    crate::download_proxy::check(&url)?;
    // Credential helper headers for the requested URL (DownloadManager
    // credentialFactory, bazel/BazelRepositoryModule.java:446-477; N41).
    // Redirect hops reuse them under reqwest's same-host rule instead of
    // asking the helper again (docs/REMOTE_CACHE.md).
    let mut request = client
        .get(url.as_str())
        .header(reqwest::header::ACCEPT_ENCODING, "identity");
    for (name, value) in crate::credentials::download_headers(url.as_str()) {
        request = request.header(name, value);
    }
    // Do not expose reqwest errors: they may contain a redirect URL with secrets.
    // No request-level timeout: it would bound the whole body transfer.
    let response = request.send().map_err(|error| {
        if error.is_timeout() {
            "HTTP request timed out".to_owned()
        } else {
            "HTTP request failed (connection, TLS, or redirect error)".to_owned()
        }
    })?;
    if matches!(
        response.status(),
        reqwest::StatusCode::NOT_FOUND | reqwest::StatusCode::GONE
    ) {
        return Ok(None);
    }
    if response.status() != reqwest::StatusCode::OK {
        return Err(format!(
            "HTTP request failed with status {}",
            response.status().as_u16()
        ));
    }
    for encoding in response
        .headers()
        .get_all(reqwest::header::CONTENT_ENCODING)
    {
        if !encoding.to_str().is_ok_and(|value| {
            value
                .split(',')
                .all(|encoding| encoding.trim().eq_ignore_ascii_case("identity"))
        }) {
            return Err("unsupported HTTP content encoding: expected identity".into());
        }
    }
    Ok(Some(response))
}

fn read_all(mut reader: impl Read) -> Result<Vec<u8>, String> {
    let mut bytes = Vec::new();
    reader
        .read_to_end(&mut bytes)
        .map_err(|_| "cannot read complete resource body".to_owned())?;
    Ok(bytes)
}

/// Stream raw bytes into `writer` in constant memory; no decompression,
/// persistent cache, or missing-file fallback. HTTP query strings are
/// permitted for source URLs, but credentials are not. Returns the size.
pub fn download_to(url: &str, writer: &mut dyn Write) -> Result<u64, String> {
    validate_download_url(url)?;
    crate::observed_fs::taint(crate::observed_fs::Taint::Network, || {
        format!("download {url}")
    });
    let reader: Option<Box<dyn Read>> = if url.starts_with("file://") {
        open_local(&file_path(url)?)?.map(|file| Box::new(file) as Box<dyn Read>)
    } else {
        let url = http_url(url)?;
        if url.fragment().is_some() {
            return Err("download URL must not contain a fragment".into());
        }
        open_http(url)?.map(|response| Box::new(response) as Box<dyn Read>)
    };
    let mut reader = reader.ok_or("download resource not found")?;
    let mut buffer = vec![0u8; 64 * 1024];
    let mut total = 0u64;
    loop {
        let n = match reader.read(&mut buffer) {
            Ok(0) => return Ok(total),
            Ok(n) => n,
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(_) => return Err("cannot read complete resource body".into()),
        };
        writer
            .write_all(&buffer[..n])
            .map_err(|error| format!("cannot write downloaded resource: {error}"))?;
        total += n as u64;
    }
}

/// [`download_to`] into memory (small resources and tests).
pub fn download(url: &str) -> Result<Vec<u8>, String> {
    let mut bytes = Vec::new();
    download_to(url, &mut bytes)?;
    Ok(bytes)
}

/// Validate before looking in a cache as well as before issuing a request.
pub fn validate_download_url(url: &str) -> Result<(), String> {
    if url.starts_with("file://") {
        file_path(url)?;
    } else if http_url(url)?.fragment().is_some() {
        return Err("download URL must not contain a fragment".into());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::net::{TcpListener, TcpStream};
    use std::sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
    };
    use std::thread::{self, JoinHandle};

    struct Fixture(PathBuf);
    impl Fixture {
        fn new() -> Self {
            static NEXT: AtomicU64 = AtomicU64::new(0);
            let root = PathBuf::from("/tmp/opencode").join(format!(
                "registry-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            std::fs::create_dir_all(&root).unwrap();
            Self(root)
        }
        fn write(&self, relative: &str, data: &[u8]) {
            let path = self.0.join(relative);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, data).unwrap();
        }
        fn registry(&self) -> Registry {
            self.0.clone().into()
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    // The task owns the listener and joins its worker even when a test panics.
    struct Server {
        address: std::net::SocketAddr,
        stop: Arc<AtomicBool>,
        worker: Option<JoinHandle<()>>,
        requests: Arc<Mutex<Vec<String>>>,
    }
    impl Server {
        fn new(responses: Vec<Vec<u8>>) -> Self {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            let address = listener.local_addr().unwrap();
            let stop = Arc::new(AtomicBool::new(false));
            let stopping = stop.clone();
            let requests = Arc::new(Mutex::new(Vec::new()));
            let seen = requests.clone();
            let worker = thread::spawn(move || {
                let mut responses = responses.into_iter();
                for stream in listener.incoming() {
                    if stopping.load(Ordering::SeqCst) {
                        break;
                    }
                    let mut stream = stream.unwrap();
                    stream
                        .set_read_timeout(Some(Duration::from_secs(2)))
                        .unwrap();
                    stream
                        .set_write_timeout(Some(Duration::from_secs(2)))
                        .unwrap();
                    let mut request = Vec::new();
                    while !request.ends_with(b"\r\n\r\n") && request.len() < 65536 {
                        let mut byte = [0];
                        if stream.read_exact(&mut byte).is_err() {
                            break;
                        }
                        request.push(byte[0]);
                    }
                    seen.lock()
                        .unwrap()
                        .push(String::from_utf8_lossy(&request).into());
                    let response = responses
                        .next()
                        .unwrap_or_else(|| response(500, "", b"unexpected request"));
                    let _ = stream.write_all(&response);
                }
            });
            Self {
                address,
                stop,
                worker: Some(worker),
                requests,
            }
        }
        fn url(&self, path: &str) -> String {
            format!("http://{}{path}", self.address)
        }
        fn registry(&self) -> Registry {
            Registry::parse(&self.url("/prefix")).unwrap()
        }
    }
    impl Drop for Server {
        fn drop(&mut self) {
            self.stop.store(true, Ordering::SeqCst);
            let _ = TcpStream::connect(self.address);
            let _ = self.worker.take().unwrap().join();
        }
    }
    fn response(status: u16, headers: &str, body: &[u8]) -> Vec<u8> {
        let mut response = format!(
            "HTTP/1.1 {status} Test\r\nContent-Length: {}\r\nConnection: close\r\n{headers}\r\n",
            body.len()
        )
        .into_bytes();
        response.extend_from_slice(body);
        response
    }

    #[test]
    fn identity_and_strict_url_validation() {
        let registry = Registry::parse("https://example.test/registry").unwrap();
        assert_eq!(registry.to_string(), "https://example.test/registry/");
        assert_eq!(
            registry.location("modules/a/1/MODULE.bazel"),
            "https://example.test/registry/modules/a/1/MODULE.bazel"
        );
        assert_eq!(
            Registry::parse("file://localhost/tmp/a%20b")
                .unwrap()
                .local_path(),
            Some(Path::new("/tmp/a b"))
        );
        assert_eq!(
            Registry::parse("file:///tmp/a%23b").unwrap().local_path(),
            Some(Path::new("/tmp/a#b"))
        );
        for url in [
            "/tmp/registry",
            "ftp://host/",
            "https://host/?q",
            "https://host/#f",
            "https://user:secret@host/",
            "http://@host/",
            "file://remote/tmp",
            "file:relative",
            "file:////tmp",
            "file:///tmp/%00",
            "file:///tmp/%GG",
            "file:///tmp/%ff",
            "https://host\\other/",
            "https://host/\n",
        ] {
            let error = Registry::parse(url).unwrap_err();
            assert!(!error.contains("secret"));
        }
        let invalid = Registry::Url("https://user:secret@host/".into());
        assert!(!invalid.to_string().contains("secret"));
        assert!(invalid.read("x").is_err());
    }

    #[test]
    fn invalid_resources_fail_before_io() {
        let registry = Registry::Url("http://127.0.0.1:1/".into());
        for relative in [
            "", "/a", "a//b", "a/", ".", "a/./b", "..", "a/../b", "a\\b", "a?b", "a#b", "%2e",
            "a\0b", "a\nb",
        ] {
            assert!(
                registry
                    .read(relative)
                    .unwrap_err()
                    .contains("invalid registry resource")
            );
        }
        assert_eq!(
            registry.location("scheme:part/file"),
            "http://127.0.0.1:1/scheme:part/file"
        );
    }

    #[test]
    fn local_absence_objects_utf8_and_limits() {
        let fixture = Fixture::new();
        let registry = fixture.registry();
        assert_eq!(registry.read("missing").unwrap(), None);
        assert_eq!(
            Registry::from(fixture.0.join("missing/root"))
                .read("file")
                .unwrap(),
            None
        );
        fixture.write("object.json", br#"{"a":1}"#);
        assert_eq!(
            registry.read_object("object.json").unwrap().unwrap()["a"],
            1
        );
        fixture.write("blank.json", b" \n\t");
        assert_eq!(registry.read_object("blank.json").unwrap(), None);
        fixture.write("bad.json", b"[]");
        assert!(
            registry
                .read_object("bad.json")
                .unwrap_err()
                .contains("JSON object")
        );
        fixture.write("bad.json", b"{");
        assert!(
            registry
                .read_object("bad.json")
                .unwrap_err()
                .contains("invalid registry JSON")
        );
        fixture.write("bad-utf8", &[255]);
        assert!(
            registry
                .read_text("bad-utf8")
                .unwrap_err()
                .contains("UTF-8")
        );
        assert!(
            Registry::from(fixture.0.join("object.json"))
                .read("file")
                .unwrap_err()
                .contains("not a directory")
        );
        std::fs::create_dir(fixture.0.join("directory")).unwrap();
        assert!(
            registry
                .read("directory")
                .unwrap_err()
                .contains("not a regular file")
        );
        let url = format!("file://{}", fixture.0.join("object.json").display());
        assert_eq!(download(&url).unwrap(), br#"{"a":1}"#);
        // No metadata size cap (the former 16 MiB native limit).
        let large = std::fs::File::create(fixture.0.join("large")).unwrap();
        large.set_len(17 * 1024 * 1024).unwrap();
        assert_eq!(
            registry.read("large").unwrap().unwrap().len(),
            17 * 1024 * 1024
        );
    }

    #[cfg(unix)]
    #[test]
    fn local_symlink_ancestors_and_entries_are_hard_errors() {
        use std::os::unix::fs::symlink;
        let fixture = Fixture::new();
        fixture.write("real/file", b"data");
        symlink(fixture.0.join("real"), fixture.0.join("link")).unwrap();
        symlink(fixture.0.join("real/file"), fixture.0.join("entry")).unwrap();
        symlink(fixture.0.join("absent"), fixture.0.join("dangling")).unwrap();
        for relative in ["link/file", "entry", "dangling"] {
            assert!(
                fixture
                    .registry()
                    .read(relative)
                    .unwrap_err()
                    .contains("symlink registry paths")
            );
        }
        assert!(Registry::from(fixture.0.join("link")).read("file").is_err());
    }

    #[test]
    fn http_prefix_fresh_reads_status_and_utf8() {
        let server = Server::new(vec![
            response(200, "", b"first"),
            response(200, "", b"second"),
            response(404, "", b"missing"),
            response(410, "", b"gone"),
            response(500, "", b"failure"),
            response(206, "", b"partial"),
            response(200, "", &[255]),
        ]);
        let registry = server.registry();
        assert_eq!(
            registry
                .read_text("modules/a/1/MODULE.bazel")
                .unwrap()
                .unwrap(),
            "first"
        );
        assert_eq!(
            registry
                .read_text("modules/a/1/MODULE.bazel")
                .unwrap()
                .unwrap(),
            "second"
        );
        assert_eq!(registry.read("missing").unwrap(), None);
        assert_eq!(registry.read("gone").unwrap(), None);
        assert!(registry.read("error").unwrap_err().contains("500"));
        assert!(registry.read("partial").unwrap_err().contains("206"));
        assert!(registry.read_text("utf8").unwrap_err().contains("UTF-8"));
        let requests = server.requests.lock().unwrap();
        assert_eq!(requests.len(), 7);
        assert!(requests[0].starts_with("GET /prefix/modules/a/1/MODULE.bazel HTTP/1.1\r\n"));
        assert!(
            requests[0]
                .to_lowercase()
                .contains("user-agent: bazel-rs/0.1\r\n")
        );
        assert!(
            requests[0]
                .to_lowercase()
                .contains("accept-encoding: identity\r\n")
        );
        assert!(!requests[0].to_lowercase().contains("referer:"));
    }

    #[test]
    fn http_redirects_preserve_identity_and_fail_closed() {
        let server = Server::new(vec![
            response(302, "Location: /target\r\n", b""),
            response(200, "", b"target"),
            response(302, "Location: http://user:secret@127.0.0.1:1/\r\n", b""),
        ]);
        let registry = server.registry();
        let identity = registry.to_string();
        assert_eq!(registry.read_text("start").unwrap().unwrap(), "target");
        assert_eq!(registry.to_string(), identity);
        let error = registry.read("bad").unwrap_err();
        assert!(!error.contains("secret"));
        assert_eq!(server.requests.lock().unwrap().len(), 3);
        assert!(!redirect_allowed(
            &Url::parse("http://host/").unwrap(),
            &[Url::parse("https://host/").unwrap()]
        ));
        assert!(!redirect_allowed(
            &Url::parse("file:///tmp/file").unwrap(),
            &[]
        ));
    }

    #[test]
    fn http_redirect_limit() {
        let server = Server::new(
            (0..12)
                .map(|_| response(302, "Location: /again\r\n", b""))
                .collect(),
        );
        assert!(server.registry().read("start").is_err());
        assert_eq!(server.requests.lock().unwrap().len(), 11);
    }

    #[test]
    fn http_limits_truncation_and_raw_binary_downloads() {
        let server = Server::new(vec![
            response(200, "Content-Encoding: identity\r\n", &[0, 255, 42]),
            response(200, "Content-Encoding: gzip\r\n", &[0, 255, 42]),
            response(404, "", b""),
            response(200, "", b"1234"),
            b"HTTP/1.1 200 OK\r\nConnection: close\r\n\r\n1234".to_vec(),
            b"HTTP/1.1 200 OK\r\nContent-Length: 10\r\nConnection: close\r\n\r\nshort".to_vec(),
            format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                16 * 1024 * 1024 + 1
            )
            .into_bytes(),
        ]);
        assert_eq!(download(&server.url("/raw")).unwrap(), [0, 255, 42]);
        assert!(
            download(&server.url("/encoded"))
                .unwrap_err()
                .contains("content encoding")
        );
        assert!(
            download(&server.url("/missing"))
                .unwrap_err()
                .contains("not found")
        );
        // No size caps: declared-length and close-delimited bodies are read whole.
        assert_eq!(download(&server.url("/length")).unwrap(), b"1234");
        assert_eq!(download(&server.url("/stream")).unwrap(), b"1234");
        assert!(
            download(&server.url("/truncated"))
                .unwrap_err()
                .contains("complete resource body")
        );
        // A huge declared length with a missing body is truncation, not a cap.
        assert!(
            server
                .registry()
                .read("large")
                .unwrap_err()
                .contains("complete resource body")
        );
    }

    /// A server that sends `body` one byte at a time with `gap` between bytes.
    fn dribble(body: &'static [u8], gap: std::time::Duration) -> (String, JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/slow", listener.local_addr().unwrap());
        let worker = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = Vec::new();
            let mut byte = [0];
            while !request.ends_with(b"\r\n\r\n") {
                if std::io::Read::read(&mut stream, &mut byte).unwrap_or(0) == 0 {
                    return;
                }
                request.push(byte[0]);
            }
            let head = format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            );
            if stream.write_all(head.as_bytes()).is_err() {
                return;
            }
            for byte in body {
                thread::sleep(gap);
                if stream
                    .write_all(&[*byte])
                    .and_then(|()| stream.flush())
                    .is_err()
                {
                    return;
                }
            }
        });
        (url, worker)
    }

    #[test]
    fn source_downloads_have_a_per_read_timeout_but_no_total_deadline() {
        // Production uses the 20 s per-read timeout with no total deadline;
        // a 1 s per-read client shows the same semantics quickly: a body
        // whose transfer takes ~3 s (3x the timeout) with 250 ms gaps
        // completes, while one 1.6 s gap fails.
        let client = client_with(Duration::from_secs(1)).unwrap();
        let (url, worker) = dribble(b"0123456789AB", Duration::from_millis(250));
        let started = std::time::Instant::now();
        let mut response = open_http_with(&client, Url::parse(&url).unwrap())
            .unwrap()
            .unwrap();
        let mut body = Vec::new();
        std::io::Read::read_to_end(&mut response, &mut body).unwrap();
        assert_eq!(body, b"0123456789AB");
        assert!(started.elapsed() > Duration::from_millis(2500));
        worker.join().unwrap();
        let (url, worker) = dribble(b"01", Duration::from_millis(1600));
        let result = open_http_with(&client, Url::parse(&url).unwrap())
            .and_then(|response| read_all(response.unwrap()));
        assert_eq!(result.unwrap_err(), "cannot read complete resource body");
        worker.join().unwrap();
    }
}
