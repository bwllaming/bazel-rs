//! Callback-owned scratch authority, never stored in a Starlark value. This is
//! containment of our own outputs, not a sandbox against a hostile same-UID peer.

use crate::extension_download::{
    self as transport, Algorithm, Budget, Checksum, FILE_LIMIT, Hash, IoResult, LIVE_LIMIT,
    PRECHECK, SharedBudget, TypedIoError, io_error,
};
use serde::Serialize;
use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File, Metadata, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::atomic::{AtomicU64, Ordering};

static NEXT_EXECUTION: AtomicU64 = AtomicU64::new(1);

#[derive(Debug, Serialize)]
pub struct DownloadResult {
    pub success: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub integrity: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sha256: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub size_bytes: Option<usize>,
}
impl DownloadResult {
    fn failure() -> Self {
        Self {
            success: false,
            integrity: None,
            sha256: None,
            size_bytes: None,
        }
    }
}
#[derive(Debug, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum FileState {
    Missing,
    Directory,
    File {
        generation: u64,
        sha256: String,
        size_bytes: usize,
    },
}
#[derive(Debug, Serialize)]
#[serde(tag = "operation", rename_all = "snake_case")]
pub enum Observation {
    Download {
        output: String,
        reused: bool,
        result: DownloadResult,
        state: FileState,
    },
    Read {
        path: String,
        watch: String,
        state: FileState,
    },
}
#[derive(Debug)]
struct OwnedFile {
    generation: u64,
    size: usize,
    #[cfg(unix)]
    identity: (u64, u64),
}
impl OwnedFile {
    fn from_metadata(metadata: &Metadata, generation: u64) -> Self {
        Self {
            generation,
            size: metadata.len() as usize,
            #[cfg(unix)]
            identity: {
                use std::os::unix::fs::MetadataExt;
                (metadata.dev(), metadata.ino())
            },
        }
    }
    fn check(&self, metadata: &Metadata) -> IoResult<()> {
        regular(metadata)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            if self.identity != (metadata.dev(), metadata.ino()) {
                return Err(TypedIoError::Policy("owned file identity changed"));
            }
        }
        if metadata.len() != self.size as u64 {
            return Err(TypedIoError::Policy(
                "owned file size changed outside download",
            ));
        }
        Ok(())
    }
}
fn regular(metadata: &Metadata) -> IoResult<()> {
    if !metadata.file_type().is_file() {
        return Err(TypedIoError::Policy("not an owned regular file"));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if metadata.nlink() != 1 {
            return Err(TypedIoError::Policy("unexpected hard link"));
        }
    }
    Ok(())
}

pub(crate) struct Download<'a> {
    pub urls: &'a [&'a str],
    pub output: &'a str,
    pub sha256: &'a str,
    pub integrity: &'a str,
    pub allow_fail: bool,
}

pub(crate) struct ScratchExecutionIo {
    identity: u64,
    active: bool,
    container: Option<tempfile::TempDir>,
    directories: BTreeSet<PathBuf>,
    files: BTreeMap<String, OwnedFile>,
    generation: u64,
    budget: SharedBudget,
    client: Option<reqwest::blocking::Client>,
    observations: Vec<Observation>,
}
impl Drop for ScratchExecutionIo {
    fn drop(&mut self) {
        self.active = false;
        // File/response handles are operation-local and already unwound. Shut
        // down the client's runtime before TempDir's best-effort error cleanup.
        self.client.take();
        self.container.take();
    }
}
impl ScratchExecutionIo {
    pub(crate) fn new() -> IoResult<Self> {
        let identity = NEXT_EXECUTION
            .try_update(Ordering::Relaxed, Ordering::Relaxed, |n| n.checked_add(1))
            .map_err(|_| TypedIoError::Resource("execution identity space exhausted"))?;
        Ok(Self {
            identity,
            active: true,
            container: None,
            directories: BTreeSet::new(),
            files: BTreeMap::new(),
            generation: 0,
            budget: Rc::new(RefCell::new(Budget::default())),
            client: None,
            observations: Vec::new(),
        })
    }
    pub(crate) fn identity(&self) -> u64 {
        self.identity
    }
    #[cfg(test)]
    pub(crate) fn with_client(mut self, client: reqwest::blocking::Client) -> Self {
        self.client = Some(client);
        self
    }
    pub(crate) fn check_receiver(&self, identity: u64) -> IoResult<()> {
        if self.active && identity == self.identity {
            Ok(())
        } else {
            Err(TypedIoError::Lifetime)
        }
    }
    pub(crate) fn close(&mut self) -> IoResult<Vec<Observation>> {
        self.active = false;
        self.client.take();
        if let Some(container) = self.container.take() {
            container
                .close()
                .map_err(|_| TypedIoError::Internal("scratch cleanup failed"))?;
        }
        Ok(std::mem::take(&mut self.observations))
    }
    fn root(&mut self) -> IoResult<PathBuf> {
        if self.container.is_none() {
            let mut builder = tempfile::Builder::new();
            builder.prefix("bazel-extension-io-");
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                builder.permissions(fs::Permissions::from_mode(0o700));
            }
            self.container = Some(builder.tempdir().map_err(io_error)?);
        }
        Ok(self
            .container
            .as_ref()
            .expect("initialized")
            .path()
            .join("work"))
    }
    fn path(text: &str) -> IoResult<()> {
        if text.len() > 1024 {
            return Err(TypedIoError::Resource("relative path length"));
        }
        if text.is_empty() {
            return Ok(());
        }
        if text.split('/').count() > 32 {
            return Err(TypedIoError::Resource("relative path depth"));
        }
        if text.split('/').any(|part| {
            part.is_empty()
                || part == "."
                || part == ".."
                || part.len() > 128
                || !part
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
        }) {
            return Err(TypedIoError::Unsupported(
                "output/read requires clean relative ASCII path components",
            ));
        }
        Ok(())
    }
    fn destination(&mut self, relative: &str) -> IoResult<PathBuf> {
        let root = self.root()?;
        Ok(if relative.is_empty() {
            root
        } else {
            root.join(relative)
        })
    }
    fn check_path(&self, path: &Path) -> IoResult<Option<Metadata>> {
        if let Some(container) = &self.container {
            let root = container.path().join("work");
            let relative = path
                .strip_prefix(&root)
                .map_err(|_| TypedIoError::Internal("scratch path escaped root"))?;
            let mut ancestor = root;
            for component in relative.components() {
                match fs::symlink_metadata(&ancestor) {
                    Ok(m) if m.is_dir() && self.directories.contains(&ancestor) => {}
                    Ok(m) if m.is_file() => {
                        let key = ancestor
                            .strip_prefix(container.path().join("work"))
                            .expect("root ancestor")
                            .to_str()
                            .ok_or(TypedIoError::Internal("non-ASCII owned path"))?;
                        self.files
                            .get(key)
                            .ok_or(TypedIoError::Policy("unowned parent file"))?
                            .check(&m)?;
                        return Ok(None);
                    }
                    Ok(_) => return Err(TypedIoError::Policy("unexpected scratch ancestor")),
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
                    Err(e) => return Err(io_error(e)),
                }
                ancestor.push(component);
            }
        }
        let metadata = match fs::symlink_metadata(path) {
            Ok(m) => m,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(io_error(e)),
        };
        if metadata.file_type().is_symlink() || (!metadata.is_dir() && !metadata.is_file()) {
            return Err(TypedIoError::Policy("symlink or special scratch entry"));
        }
        if metadata.is_dir() && !self.directories.contains(path) {
            return Err(TypedIoError::Policy("unexpected scratch directory"));
        }
        Ok(Some(metadata))
    }
    fn parents(&mut self, relative: &str) -> IoResult<()> {
        if relative.is_empty() {
            return Ok(());
        }
        let root = self.root()?;
        let mut parent = root;
        let parts: Vec<_> = relative.split('/').collect();
        for part in std::iter::once(None).chain(parts[..parts.len() - 1].iter().map(|p| Some(*p))) {
            if let Some(part) = part {
                parent.push(part);
            }
            match self.check_path(&parent)? {
                Some(metadata) if metadata.is_file() => {
                    return Err(TypedIoError::Completion("parent path is a file".into()));
                }
                Some(_) => {}
                None => {
                    fs::create_dir(&parent).map_err(io_error)?;
                    self.directories.insert(parent.clone());
                }
            }
        }
        Ok(())
    }
    fn bytes(&mut self, relative: &str) -> IoResult<Vec<u8>> {
        let path = self.destination(relative)?;
        let owned = self
            .files
            .get(relative)
            .ok_or_else(|| TypedIoError::Completion("missing owned file".into()))?;
        let metadata = self
            .check_path(&path)?
            .ok_or_else(|| TypedIoError::Completion("missing owned file".into()))?;
        owned.check(&metadata)?;
        let size = owned.size;
        if size > FILE_LIMIT {
            return Err(TypedIoError::Resource("8MiB file"));
        }
        // Charge allocation, read and hashes before allocating. Fixed-sized read
        // prevents a concurrently grown file from driving unbounded allocation.
        self.budget.borrow_mut().work(size.saturating_mul(4))?;
        let mut file = File::open(path).map_err(io_error)?;
        owned.check(&file.metadata().map_err(io_error)?)?;
        let mut bytes = vec![0; size];
        let mut offset = 0;
        while offset < size {
            self.budget.borrow().remaining()?;
            let n = file
                .read(&mut bytes[offset..size.min(offset + PRECHECK)])
                .map_err(io_error)?;
            if n == 0 {
                return Err(TypedIoError::Internal("owned file shortened while reading"));
            }
            offset += n;
        }
        owned.check(&file.metadata().map_err(io_error)?)?;
        Ok(bytes)
    }
    fn state(&mut self, relative: &str) -> IoResult<FileState> {
        if let Some(file) = self.files.get(relative) {
            let generation = file.generation;
            let bytes = self.bytes(relative)?;
            return Ok(FileState::File {
                generation,
                sha256: transport::sha256(&bytes),
                size_bytes: bytes.len(),
            });
        }
        let path = self.destination(relative)?;
        Ok(match self.check_path(&path)? {
            None => FileState::Missing,
            Some(m) if m.is_dir() => FileState::Directory,
            Some(_) => return Err(TypedIoError::Policy("unowned scratch file")),
        })
    }
    pub(crate) fn read(&mut self, identity: u64, path: &str, watch: &str) -> IoResult<String> {
        self.check_receiver(identity)?;
        Self::path(path)?;
        match watch {
            "auto" | "no" => {}
            "yes" => {
                return Err(TypedIoError::Unsupported(
                    "watch=yes on extension-owned files",
                ));
            }
            _ => return Err(TypedIoError::Validation("watch must be auto, yes, or no")),
        }
        self.budget.borrow_mut().call()?;
        let bytes = self.bytes(path)?;
        // Latin-1 can expand each byte to two UTF-8 bytes in the evaluator.
        self.budget
            .borrow_mut()
            .work(bytes.len().saturating_mul(2))?;
        let mut text = String::with_capacity(bytes.len() * 2);
        for byte in &bytes {
            text.push(char::from(*byte));
        }
        let generation = self.files[path].generation;
        self.observations.push(Observation::Read {
            path: path.into(),
            watch: watch.into(),
            state: FileState::File {
                generation,
                sha256: transport::sha256(&bytes),
                size_bytes: bytes.len(),
            },
        });
        Ok(text)
    }
    pub(crate) fn download(
        &mut self,
        identity: u64,
        enabled: bool,
        request: Download<'_>,
    ) -> IoResult<&DownloadResult> {
        self.check_receiver(identity)?;
        Self::path(request.output)?;
        if request.urls.len() > 8 {
            return Err(TypedIoError::Resource("8 mirrors per call"));
        }
        if request.urls.is_empty() && !request.allow_fail {
            return Err(TypedIoError::Validation("urls not set"));
        }
        let mut urls = Vec::with_capacity(request.urls.len());
        for url in request.urls {
            urls.push(transport::validate_url(url)?);
        }
        if request.sha256.is_empty() && request.integrity.is_empty() {
            urls.retain(|u| u.scheme() != "http");
        }
        if urls.is_empty() && !request.allow_fail {
            return Err(TypedIoError::Validation(
                "No URLs left after removing plain http URLs due to missing checksum",
            ));
        }
        let checksums = transport::checksums(request.sha256, request.integrity)?;
        self.budget.borrow_mut().call()?;
        let mut reused = false;
        let completion = (|| {
            self.parents(request.output)?;
            if let Some(expected) = &checksums.expected
                && self.files.contains_key(request.output)
                && expected.matches(&self.bytes(request.output)?)
            {
                reused = true;
                return Ok(());
            }
            // Policy is intentionally AFTER matching owned destination reuse.
            if !enabled {
                return Err(TypedIoError::Completion(
                    "repository downloads are disabled".into(),
                ));
            }
            if urls.is_empty() {
                return Err(TypedIoError::Completion("no download URLs".into()));
            }
            if self.client.is_none() {
                self.client = Some(transport::client()?);
            }
            let mut last = TypedIoError::Completion("no mirror succeeded".into());
            for url in &urls {
                let transfer = transport::open(
                    self.client.as_ref().expect("initialized"),
                    url,
                    self.budget.clone(),
                )
                .and_then(|reader| {
                    self.transfer(reader, request.output, checksums.expected.as_ref())
                });
                match transfer {
                    Ok(()) => return Ok(()),
                    Err(error @ TypedIoError::Completion(_)) => last = error,
                    Err(error) => return Err(error),
                }
            }
            Err(last)
        })();
        let result = match completion {
            Err(TypedIoError::Completion(_)) if request.allow_fail => DownloadResult::failure(),
            Err(error) => return Err(error),
            Ok(()) => {
                if let Some(error) = checksums.deferred {
                    return Err(error);
                }
                // Final hashing/stat/result construction is outside the allow_fail
                // catch, as are observation-state hashing failures below.
                let bytes = self.bytes(request.output)?;
                let algorithm = checksums
                    .expected
                    .as_ref()
                    .map_or(Algorithm::Sha256, |c| c.algorithm);
                let checksum = Checksum {
                    algorithm,
                    bytes: algorithm.hash(&bytes),
                };
                DownloadResult {
                    success: true,
                    integrity: Some(checksum.integrity()),
                    sha256: (algorithm == Algorithm::Sha256)
                        .then(|| transport::hex(&checksum.bytes)),
                    size_bytes: Some(bytes.len()),
                }
            }
        };
        let state = self.state(request.output)?;
        self.observations.push(Observation::Download {
            output: request.output.into(),
            reused,
            result,
            state,
        });
        match self.observations.last().expect("just pushed") {
            Observation::Download { result, .. } => Ok(result),
            _ => unreachable!(),
        }
    }
    fn transfer(
        &mut self,
        mut reader: Box<dyn Read>,
        relative: &str,
        expected: Option<&Checksum>,
    ) -> IoResult<()> {
        self.budget.borrow_mut().work(PRECHECK * 2)?;
        let mut precheck = [0u8; PRECHECK];
        let mut hash = Hash::new(expected.map_or(Algorithm::Sha256, |c| c.algorithm));
        let mut prefix = 0;
        if let Some(expected) = expected {
            while prefix < PRECHECK {
                let n = transport::read_decoded(
                    &mut *reader,
                    &mut precheck[prefix..],
                    prefix,
                    &self.budget,
                )?;
                if n == 0 {
                    break;
                }
                hash.update(&precheck[prefix..prefix + n]);
                prefix += n;
            }
            if prefix < PRECHECK && !expected.matches(&precheck[..prefix]) {
                return Err(TypedIoError::Completion("checksum mismatch".into()));
            }
        }
        let path = self.destination(relative)?;
        if let Some(metadata) = self.check_path(&path)?
            && metadata.is_file()
        {
            self.files
                .get(relative)
                .ok_or(TypedIoError::Policy("unowned scratch file"))?
                .check(&metadata)?;
        }
        let mut file = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .open(path)
            .map_err(io_error)?;
        let metadata = file.metadata().map_err(io_error)?;
        regular(&metadata)?;
        self.generation += 1;
        self.files.insert(
            relative.into(),
            OwnedFile::from_metadata(&metadata, self.generation),
        );
        if prefix != 0 {
            self.write(&mut file, relative, &precheck[..prefix])?;
        }
        let mut buffer = [0u8; PRECHECK];
        loop {
            let size = self.files[relative].size;
            let n = transport::read_decoded(&mut *reader, &mut buffer, size, &self.budget)?;
            if n == 0 {
                break;
            }
            hash.update(&buffer[..n]);
            self.write(&mut file, relative, &buffer[..n])?;
        }
        file.flush().map_err(io_error)?;
        if let Some(expected) = expected
            && hash.finish() != expected.bytes
        {
            return Err(TypedIoError::Completion("checksum mismatch".into()));
        }
        Ok(())
    }
    fn write(&mut self, file: &mut File, relative: &str, bytes: &[u8]) -> IoResult<()> {
        let live: usize = self.files.values().map(|f| f.size).sum();
        if bytes.len() > FILE_LIMIT - self.files[relative].size {
            return Err(TypedIoError::Resource("8MiB decoded/file"));
        }
        if bytes.len() > LIVE_LIMIT - live {
            return Err(TypedIoError::Resource("16MiB live scratch"));
        }
        self.budget.borrow_mut().work(bytes.len())?;
        // Count each successful short write so a subsequent write error leaves
        // precisely the partial file owned/readable, without rollback/chmod.
        let mut offset = 0;
        while offset < bytes.len() {
            self.budget.borrow().remaining()?;
            let n = file.write(&bytes[offset..]).map_err(io_error)?;
            if n == 0 {
                return Err(TypedIoError::Completion("zero-length file write".into()));
            }
            offset += n;
            self.files.get_mut(relative).expect("opened").size += n;
        }
        Ok(())
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    pub(crate) fn n3_exhaust_budget_for_callback(io: &ScratchExecutionIo, deadline: bool) {
        transport::tests::n3_exhaust_budget_for_callback(&io.budget, deadline);
    }
    include!("../tests/support/extension_io_boundary_unit.rs");
    include!("../tests/support/extension_io_bounded_tls.rs");
    use std::net::TcpListener;
    use std::sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    };
    use std::thread;
    use std::time::Duration;

    pub(crate) struct Server {
        pub url: String,
        pub requests: Arc<Mutex<Vec<String>>>,
        stop: Arc<AtomicBool>,
        worker: Option<thread::JoinHandle<()>>,
    }
    impl Server {
        pub(crate) fn new(responses: Vec<Vec<u8>>) -> Self {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            listener.set_nonblocking(true).unwrap();
            let url = format!("http://{}", listener.local_addr().unwrap());
            let requests = Arc::new(Mutex::new(Vec::new()));
            let stop = Arc::new(AtomicBool::new(false));
            let logged = requests.clone();
            let stopped = stop.clone();
            let worker = thread::spawn(move || {
                let mut responses = responses.into_iter();
                while !stopped.load(Ordering::SeqCst) {
                    let (mut stream, _) = match listener.accept() {
                        Ok(pair) => pair,
                        Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                            thread::sleep(Duration::from_millis(2));
                            continue;
                        }
                        Err(e) => panic!("listener: {e}"),
                    };
                    stream
                        .set_read_timeout(Some(Duration::from_secs(2)))
                        .unwrap();
                    stream
                        .set_write_timeout(Some(Duration::from_secs(2)))
                        .unwrap();
                    let mut request = Vec::new();
                    let mut byte = [0];
                    while request.len() < 16384 && !request.ends_with(b"\r\n\r\n") {
                        if stream.read(&mut byte).unwrap_or(0) == 0 {
                            break;
                        }
                        request.push(byte[0]);
                    }
                    logged
                        .lock()
                        .unwrap()
                        .push(String::from_utf8(request).unwrap());
                    let response = responses.next().unwrap_or_else(|| response(404, b"", ""));
                    let _ = stream.write_all(&response);
                }
            });
            Self {
                url,
                requests,
                stop,
                worker: Some(worker),
            }
        }
        pub(crate) fn count(&self) -> usize {
            self.requests.lock().unwrap().len()
        }
    }
    impl Drop for Server {
        fn drop(&mut self) {
            self.stop.store(true, Ordering::SeqCst);
            self.worker.take().unwrap().join().unwrap();
        }
    }
    pub(crate) fn response(status: u16, body: &[u8], headers: &str) -> Vec<u8> {
        let mut out = format!(
            "HTTP/1.1 {status} Fixture\r\nContent-Length: {}\r\nConnection: close\r\n{headers}\r\n",
            body.len()
        )
        .into_bytes();
        out.extend_from_slice(body);
        out
    }
    pub(crate) struct TlsServer {
        pub url: String,
        pub pem: Vec<u8>,
        child: std::process::Child,
        _private: tempfile::TempDir,
    }
    impl TlsServer {
        pub(crate) fn new() -> Self {
            use std::io::BufRead;
            use std::process::{Command, Stdio};
            let private = tempfile::Builder::new()
                .prefix("n3-private-tls-")
                .tempdir_in("/tmp/opencode")
                .unwrap();
            let openssl = |args: &[&str]| {
                let status = Command::new("/usr/bin/openssl")
                    .args(args)
                    .env_clear()
                    .env("PATH", "/usr/bin:/bin")
                    .env("HOME", private.path())
                    .env("TMPDIR", private.path())
                    .current_dir(private.path())
                    .stdin(Stdio::null())
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .status()
                    .unwrap();
                assert!(status.success());
            };
            openssl(&[
                "req",
                "-x509",
                "-newkey",
                "rsa:2048",
                "-nodes",
                "-days",
                "1",
                "-subj",
                "/CN=N3 private test CA",
                "-addext",
                "basicConstraints=critical,CA:TRUE",
                "-keyout",
                "ca-key.pem",
                "-out",
                "ca.pem",
            ]);
            openssl(&[
                "req",
                "-new",
                "-newkey",
                "rsa:2048",
                "-nodes",
                "-subj",
                "/CN=localhost",
                "-keyout",
                "key.pem",
                "-out",
                "server.csr",
            ]);
            fs::write(private.path().join("server.ext"), "basicConstraints=critical,CA:FALSE\nkeyUsage=critical,digitalSignature,keyEncipherment\nextendedKeyUsage=serverAuth\nsubjectAltName=DNS:localhost\n").unwrap();
            openssl(&[
                "x509",
                "-req",
                "-in",
                "server.csr",
                "-CA",
                "ca.pem",
                "-CAkey",
                "ca-key.pem",
                "-CAcreateserial",
                "-days",
                "1",
                "-extfile",
                "server.ext",
                "-out",
                "cert.pem",
            ]);
            let script = r#"import http.server, ssl, gzip
class Handler(http.server.BaseHTTPRequestHandler):
 def log_message(self, *args): pass
 def do_GET(self):
  with open('requests.log', 'a') as log: log.write(self.path + '\n')
  if self.path in ('/downgrade', '/downgrade301'):
   self.send_response(301 if self.path == '/downgrade301' else 302)
   self.send_header('Location', 'http://127.0.0.1:9/not-requested')
   self.send_header('Content-Length', '0')
   self.end_headers()
   return
  body = b'{"real":"tls bytes"}'
  encoding = None
  if self.path == '/bad-header':
   body = b'not a gzip header'
   encoding = 'gzip'
  elif self.path == '/bad-body':
   body = bytes([31,139,8,0,0,0,0,0,0,255,7])
   encoding = 'gzip'
  elif self.path == '/bad-crc':
   body = bytearray(gzip.compress(b'partial bytes\n' * 3000, mtime=0))
   body[-8] ^= 255
   encoding = 'gzip'
  self.send_response(200)
  self.send_header('Content-Length', str(len(body)))
  if encoding: self.send_header('Content-Encoding', encoding)
  self.end_headers()
  self.wfile.write(body)
server = http.server.HTTPServer(('127.0.0.1', 0), Handler)
context = ssl.SSLContext(ssl.PROTOCOL_TLS_SERVER)
context.load_cert_chain('cert.pem', 'key.pem')
server.socket = context.wrap_socket(server.socket, server_side=True)
print(server.server_port, flush=True)
server.serve_forever()
"#;
            let mut child = Command::new("/usr/bin/python3")
                .args(["-u", "-c", script])
                .env_clear()
                .env("PATH", "/usr/bin:/bin")
                .env("HOME", private.path())
                .env("TMPDIR", private.path())
                .current_dir(private.path())
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(Stdio::null())
                .spawn()
                .unwrap();
            let mut line = String::new();
            std::io::BufReader::new(child.stdout.take().unwrap())
                .read_line(&mut line)
                .unwrap();
            let port: u16 = line.trim().parse().unwrap();
            Self {
                url: format!("https://localhost:{port}"),
                pem: fs::read(private.path().join("ca.pem")).unwrap(),
                child,
                _private: private,
            }
        }
    }
    impl Drop for TlsServer {
        fn drop(&mut self) {
            let _ = self.child.kill();
            self.child.wait().unwrap();
        }
    }
    fn download(
        io: &mut ScratchExecutionIo,
        url: &str,
        output: &str,
        sha: &str,
        allow: bool,
    ) -> IoResult<bool> {
        io.download(
            io.identity(),
            true,
            Download {
                urls: &[url],
                output,
                sha256: sha,
                integrity: "",
                allow_fail: allow,
            },
        )
        .map(|r| r.success)
    }
    fn seed(io: &mut ScratchExecutionIo, output: &str, bytes: &[u8]) {
        io.parents(output).unwrap();
        io.transfer(Box::new(std::io::Cursor::new(bytes.to_vec())), output, None)
            .unwrap();
    }

    #[test]
    fn checksumless_https_bad_header_preserves_owned_destination_but_late_errors_do_not() {
        use std::os::unix::fs::PermissionsExt;
        let server = TlsServer::new();
        for (route, expected) in [
            ("/bad-header", b"old owned bytes\n".to_vec()),
            ("/bad-body", Vec::new()),
            ("/bad-crc", b"partial bytes\n".repeat(3000)),
        ] {
            let mut io = ScratchExecutionIo::new()
                .unwrap()
                .with_client(transport::trusted_client(&server.pem));
            seed(&mut io, "same", b"old owned bytes\n");
            let path = io.destination("same").unwrap();
            fs::set_permissions(&path, fs::Permissions::from_mode(0o640)).unwrap();
            assert!(
                !download(&mut io, &format!("{}{route}", server.url), "same", "", true).unwrap()
            );
            assert_eq!(
                io.bytes("same").unwrap(),
                expected,
                "stage mismatch for {route}"
            );
            assert_eq!(
                fs::metadata(path).unwrap().permissions().mode() & 0o777,
                0o640
            );
            let expected_generation = if route == "/bad-header" { 1 } else { 2 };
            assert_eq!(io.files["same"].generation, expected_generation);
            match io.observations.last().unwrap() {
                Observation::Download {
                    result,
                    state:
                        FileState::File {
                            generation,
                            sha256,
                            size_bytes,
                        },
                    ..
                } => {
                    assert_eq!(
                        serde_json::to_value(result).unwrap(),
                        serde_json::json!({"success": false})
                    );
                    assert_eq!(*generation, expected_generation);
                    assert_eq!(*size_bytes, expected.len());
                    assert_eq!(*sha256, transport::sha256(&expected));
                }
                other => panic!("incorrect failure observation: {other:?}"),
            }
            let container = io.container.as_ref().unwrap().path().to_owned();
            io.close().unwrap();
            assert!(!container.exists());
        }
        let requests = fs::read_to_string(server._private.path().join("requests.log")).unwrap();
        assert_eq!(
            requests.lines().collect::<Vec<_>>(),
            ["/bad-header", "/bad-body", "/bad-crc"]
        );
    }

    #[test]
    fn gzip_header_completion_mirrors_and_deferred_checksum_preserve_failure_ordering() {
        let server = TlsServer::new();
        let bad = format!("{}/bad-header", server.url);
        let good = format!("{}/good", server.url);
        let mut io = ScratchExecutionIo::new()
            .unwrap()
            .with_client(transport::trusted_client(&server.pem));
        seed(&mut io, "same", b"old bytes");
        assert!(matches!(
            download(&mut io, &bad, "same", "", false),
            Err(TypedIoError::Completion(_))
        ));
        assert_eq!(io.bytes("same").unwrap(), b"old bytes");
        assert!(!download(&mut io, &bad, "fresh", "", true).unwrap());
        assert!(!io.destination("fresh").unwrap().exists());
        assert!(!io.files.contains_key("fresh"));
        assert!(!download(&mut io, &bad, "same", "malformed supported SHA", true).unwrap());
        assert_eq!(io.bytes("same").unwrap(), b"old bytes");
        let result = io
            .download(
                io.identity(),
                true,
                Download {
                    urls: &[&bad, &good],
                    output: "same",
                    sha256: "",
                    integrity: "",
                    allow_fail: false,
                },
            )
            .unwrap();
        assert!(result.success);
        assert_eq!(result.size_bytes, Some(20));
        assert_eq!(
            result.sha256.as_deref(),
            Some(transport::sha256(b"{\"real\":\"tls bytes\"}").as_str())
        );
        assert_eq!(io.bytes("same").unwrap(), b"{\"real\":\"tls bytes\"}");
        let requests = fs::read_to_string(server._private.path().join("requests.log")).unwrap();
        assert_eq!(
            requests.lines().collect::<Vec<_>>(),
            [
                "/bad-header",
                "/bad-header",
                "/bad-header",
                "/bad-header",
                "/good"
            ]
        );
    }

    #[test]
    fn encoded_budget_error_during_gzip_header_is_not_tolerated_or_truncating() {
        let server = TlsServer::new();
        let mut io = ScratchExecutionIo::new()
            .unwrap()
            .with_client(transport::trusted_client(&server.pem));
        seed(&mut io, "same", b"old bytes");
        transport::tests::remaining_encoded_for_test(&io.budget, 5);
        let error = download(
            &mut io,
            &format!("{}/bad-body", server.url),
            "same",
            "",
            true,
        )
        .unwrap_err();
        assert_eq!(
            error,
            TypedIoError::Resource("32MiB cumulative encoded bytes")
        );
        assert_eq!(io.bytes("same").unwrap(), b"old bytes");
        assert_eq!(io.files["same"].generation, 1);
        assert!(io.observations.is_empty());
        assert_eq!(
            fs::read_to_string(server._private.path().join("requests.log")).unwrap(),
            "/bad-body\n"
        );
    }

    #[test]
    fn uppercase_gzip_original_and_final_suffixes_preserve_raw_bytes_not_queries() {
        let body = b"gzip suffix case";
        let gzip = transport::tests::gzip_bytes(body);
        for (original, final_path, raw) in [
            ("/start.GZ?query=1", "/final", true),
            ("/start.TGZ", "/final?query=1", true),
            ("/start.gZ", "/final", true),
            ("/start.tGz", "/final", true),
            ("/start?query=1", "/final.GZ?query=2", true),
            ("/start", "/final.TGZ", true),
            ("/start", "/final.tGz", true),
            ("/start?only_query=.GZ", "/final?only_query=.TGZ", false),
        ] {
            let server = Server::new(vec![
                response(302, b"", &format!("Location: {final_path}\r\n")),
                response(200, &gzip, "Content-Encoding: gzip\r\n"),
            ]);
            let mut io = ScratchExecutionIo::new().unwrap();
            let expected = if raw {
                gzip.as_slice()
            } else {
                body.as_slice()
            };
            let sha = transport::sha256(expected);
            let result = io
                .download(
                    io.identity(),
                    true,
                    Download {
                        urls: &[&format!("{}{original}", server.url)],
                        output: "data",
                        sha256: &sha,
                        integrity: "",
                        allow_fail: false,
                    },
                )
                .unwrap();
            assert!(result.success);
            assert_eq!(result.sha256.as_deref(), Some(sha.as_str()));
            assert_eq!(result.size_bytes, Some(expected.len()));
            assert_eq!(io.bytes("data").unwrap(), expected);
            let requests = server.requests.lock().unwrap();
            assert_eq!(
                requests
                    .iter()
                    .map(|r| r.split_whitespace().nth(1).unwrap())
                    .collect::<Vec<_>>(),
                [original, final_path]
            );
        }
    }

    fn leave_decoded_allowance(io: &mut ScratchExecutionIo, allowance: usize) {
        let used = transport::tests::decoded_count(&io.budget);
        io.budget
            .borrow_mut()
            .decoded(transport::TOTAL_LIMIT - allowance - used)
            .unwrap();
    }

    #[test]
    fn cumulative_precheck_bound_preserves_old_file_or_exact_written_prefix() {
        use std::os::unix::fs::PermissionsExt;
        for (allowance, length) in [(7, 8), (7, 7), (PRECHECK + 7, PRECHECK + 8)] {
            let mut io = ScratchExecutionIo::new().unwrap();
            seed(&mut io, "same", b"old bytes");
            let path = io.destination("same").unwrap();
            fs::set_permissions(&path, fs::Permissions::from_mode(0o640)).unwrap();
            leave_decoded_allowance(&mut io, allowance);
            let body = vec![b'x'; length];
            let expected = Checksum {
                algorithm: Algorithm::Sha256,
                bytes: Algorithm::Sha256.hash(&body),
            };
            let (reader, trace) = transport::tests::Metered::new(std::io::Cursor::new(body));
            let result = io.transfer(Box::new(reader), "same", Some(&expected));
            let excess = length > allowance;
            if excess {
                assert!(matches!(result, Err(TypedIoError::Resource(_))));
            } else {
                result.unwrap();
            }
            assert_eq!(trace.borrow().returned, allowance + usize::from(excess));
            assert_eq!(
                transport::tests::decoded_count(&io.budget),
                transport::TOTAL_LIMIT + usize::from(excess)
            );
            let expected_bytes = if excess && allowance < PRECHECK {
                b"old bytes".to_vec()
            } else {
                vec![b'x'; allowance]
            };
            assert_eq!(io.bytes("same").unwrap(), expected_bytes);
            assert_eq!(
                io.files["same"].generation,
                if excess && allowance < PRECHECK { 1 } else { 2 }
            );
            assert_eq!(
                fs::metadata(path).unwrap().permissions().mode() & 0o777,
                0o640
            );
            assert_eq!(
                trace.borrow().requests,
                match (allowance, excess) {
                    (7, true) => vec![7, 1],
                    (7, false) => vec![7, 1, 1],
                    _ => vec![PRECHECK, 7, 1],
                }
            );
            if !excess {
                // A new checksummed operation at the shared cumulative cap may
                // probe once, but cannot open/truncate the preceding output.
                let next = Checksum {
                    algorithm: Algorithm::Sha256,
                    bytes: Algorithm::Sha256.hash(b"y"),
                };
                let (reader, next_trace) =
                    transport::tests::Metered::new(std::io::Cursor::new(vec![b'y']));
                assert!(matches!(
                    io.transfer(Box::new(reader), "same", Some(&next)),
                    Err(TypedIoError::Resource(_))
                ));
                assert_eq!(next_trace.borrow().requests, [1]);
                assert_eq!(next_trace.borrow().returned, 1);
                assert_eq!(
                    transport::tests::decoded_count(&io.budget),
                    transport::TOTAL_LIMIT + 1
                );
                assert_eq!(io.bytes("same").unwrap(), expected_bytes);
                assert_eq!(io.files["same"].generation, 2);
            }
        }
    }

    #[test]
    fn cumulative_no_checksum_body_bound_counts_multiple_calls_and_one_rejected_probe() {
        for compressed in [false, true] {
            let mut io = ScratchExecutionIo::new().unwrap();
            seed(&mut io, "same", b"old bytes");
            leave_decoded_allowance(&mut io, 7);
            for (index, body, success, expected_bytes) in [
                (0, b"abc".as_slice(), true, b"abc".as_slice()),
                (1, b"defg".as_slice(), true, b"defg".as_slice()),
                (2, b"".as_slice(), true, b"".as_slice()),
                (3, b"excess".as_slice(), false, b"".as_slice()),
            ] {
                let reader: Box<dyn Read> = if compressed {
                    Box::new(flate2::read::MultiGzDecoder::new(std::io::Cursor::new(
                        transport::tests::gzip_bytes(body),
                    )))
                } else {
                    Box::new(std::io::Cursor::new(body.to_vec()))
                };
                let (reader, trace) = transport::tests::Metered::new(reader);
                let result = io.transfer(Box::new(reader), "same", None);
                if success {
                    result.unwrap();
                } else {
                    assert!(matches!(result, Err(TypedIoError::Resource(_))));
                }
                assert_eq!(io.bytes("same").unwrap(), expected_bytes);
                assert_eq!(
                    trace.borrow().returned,
                    if success { body.len() } else { 1 }
                );
                assert_eq!(io.files["same"].generation, index + 2);
                let expected_count = if index == 0 {
                    transport::TOTAL_LIMIT - 4
                } else {
                    transport::TOTAL_LIMIT + usize::from(!success)
                };
                assert_eq!(transport::tests::decoded_count(&io.budget), expected_count);
                assert!(trace.borrow().requests.iter().all(|n| *n
                    <= if index == 0 {
                        7
                    } else if index == 1 {
                        4
                    } else {
                        1
                    }));
            }
            let (reader, trace) =
                transport::tests::Metered::new(std::io::Cursor::new(vec![b'x'; PRECHECK]));
            assert!(matches!(
                io.transfer(Box::new(reader), "same", None),
                Err(TypedIoError::Resource(_))
            ));
            assert!(
                trace.borrow().requests.is_empty(),
                "a second rejected excess probe was made"
            );
            assert_eq!(
                transport::tests::decoded_count(&io.budget),
                transport::TOTAL_LIMIT + 1
            );
        }
    }

    #[test]
    fn checksumless_https_cumulative_budget_error_is_hard_and_leaves_only_allowed_bytes() {
        let server = TlsServer::new();
        let mut io = ScratchExecutionIo::new()
            .unwrap()
            .with_client(transport::trusted_client(&server.pem));
        seed(&mut io, "same", b"old bytes");
        leave_decoded_allowance(&mut io, 7);
        assert!(matches!(
            download(&mut io, &server.url, "same", "", true),
            Err(TypedIoError::Resource(_))
        ));
        assert_eq!(io.bytes("same").unwrap(), b"{\"real\"");
        assert_eq!(
            transport::tests::decoded_count(&io.budget),
            transport::TOTAL_LIMIT + 1
        );
        assert!(io.observations.is_empty());
    }

    #[test]
    fn lazy_root_file_and_child_transitions_match_request_staging() {
        let body = b"root bytes";
        let sha = transport::sha256(body);
        let server = Server::new(vec![
            response(200, body, ""),
            response(200, body, ""),
            response(200, body, ""),
        ]);
        let mut io = ScratchExecutionIo::new().unwrap();
        assert!(io.container.is_none());
        assert!(download(&mut io, &server.url, "", &sha, false).unwrap());
        assert!(!download(&mut io, &server.url, "child", &sha, true).unwrap());
        assert_eq!(server.count(), 1);
        assert_eq!(io.read(io.identity(), "", "auto").unwrap(), "root bytes");
        io.close().unwrap();
        let mut io = ScratchExecutionIo::new().unwrap();
        assert!(download(&mut io, &server.url, "child", &sha, false).unwrap());
        assert!(!download(&mut io, &server.url, "", &sha, true).unwrap());
        assert_eq!(server.count(), 3);
        assert_eq!(io.read(io.identity(), "child", "no").unwrap(), "root bytes");
    }

    #[test]
    fn owned_reuse_precedes_disable_and_empty_allow_validation_does_not() {
        let mut io = ScratchExecutionIo::new().unwrap();
        seed(&mut io, "same", b"seed");
        let sha = transport::sha256(b"seed");
        assert!(
            io.download(
                io.identity(),
                false,
                Download {
                    urls: &[],
                    output: "same",
                    sha256: &sha,
                    integrity: "",
                    allow_fail: true
                }
            )
            .unwrap()
            .success
        );
        assert!(matches!(
            io.download(
                io.identity(),
                false,
                Download {
                    urls: &[],
                    output: "same",
                    sha256: &sha,
                    integrity: "",
                    allow_fail: false
                }
            ),
            Err(TypedIoError::Validation(_))
        ));
        assert!(
            !io.download(
                io.identity(),
                false,
                Download {
                    urls: &["https://localhost/not-requested"],
                    output: "other",
                    sha256: "bad",
                    integrity: "",
                    allow_fail: true
                }
            )
            .unwrap()
            .success
        );
        assert!(io.client.is_none());
    }

    #[test]
    fn deferred_malformed_checksum_retains_http_and_only_raises_after_success() {
        let server = Server::new(vec![response(404, b"", ""), response(200, b"body", "")]);
        let mut io = ScratchExecutionIo::new().unwrap();
        assert!(!download(&mut io, &server.url, "bad", "malformed", true).unwrap());
        assert!(matches!(
            download(&mut io, &server.url, "bad", "malformed", true),
            Err(TypedIoError::Validation(_))
        ));
        assert_eq!(io.read(io.identity(), "bad", "no").unwrap(), "body");
        assert_eq!(server.count(), 2);
        assert!(matches!(
            io.download(
                io.identity(),
                true,
                Download {
                    urls: &[&server.url],
                    output: "",
                    sha256: "",
                    integrity: "md5-AAAA",
                    allow_fail: true
                }
            ),
            Err(TypedIoError::Unsupported(_))
        ));
        assert_eq!(server.count(), 2);
    }

    #[test]
    fn checksum_precheck_32767_32768_32769_preserves_exact_bytes_and_permissions() {
        use std::os::unix::fs::PermissionsExt;
        for length in [32767, 32768, 32769] {
            let body = vec![b'b'; length];
            let server = Server::new(vec![response(200, &body, "")]);
            let mut io = ScratchExecutionIo::new().unwrap();
            seed(&mut io, "same", b"owned old destination\n");
            let path = io.destination("same").unwrap();
            fs::set_permissions(&path, fs::Permissions::from_mode(0o640)).unwrap();
            assert!(!download(&mut io, &server.url, "same", &"00".repeat(32), true).unwrap());
            assert_eq!(
                io.bytes("same").unwrap(),
                if length < PRECHECK {
                    b"owned old destination\n".to_vec()
                } else {
                    body
                }
            );
            assert_eq!(
                fs::metadata(path).unwrap().permissions().mode() & 0o777,
                0o640
            );
            assert_eq!(server.count(), 1);
        }
    }

    #[test]
    fn truncated_stream_leaves_33000_bytes_without_retry_and_mirrors_still_work() {
        use std::os::unix::fs::PermissionsExt;
        let mut truncated =
            b"HTTP/1.1 200 OK\r\nContent-Length: 40000\r\nConnection: close\r\n\r\n".to_vec();
        truncated.extend_from_slice(&vec![b'x'; 33000]);
        let server = Server::new(vec![
            truncated.clone(),
            truncated,
            response(200, b"next", ""),
        ]);
        let mut io = ScratchExecutionIo::new().unwrap();
        seed(&mut io, "same", b"old");
        fs::set_permissions(
            io.destination("same").unwrap(),
            fs::Permissions::from_mode(0o640),
        )
        .unwrap();
        assert!(
            !download(
                &mut io,
                &server.url,
                "same",
                &transport::sha256(&vec![b'x'; 40000]),
                true
            )
            .unwrap()
        );
        assert_eq!(io.bytes("same").unwrap(), vec![b'x'; 33000]);
        assert_eq!(server.count(), 1);
        assert_eq!(
            fs::metadata(io.destination("same").unwrap())
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o640
        );
        let next = format!("{}/next", server.url);
        assert!(
            io.download(
                io.identity(),
                true,
                Download {
                    urls: &[&server.url, &next],
                    output: "fallback",
                    sha256: &transport::sha256(b"next"),
                    integrity: "",
                    allow_fail: false
                }
            )
            .unwrap()
            .success
        );
        assert_eq!(server.count(), 3);
    }

    #[test]
    fn result_shapes_all_five_digests_and_latin1_all256() {
        let bytes: Vec<_> = (0..=255).collect();
        let server = Server::new((0..5).map(|_| response(200, &bytes, "")).collect());
        let mut io = ScratchExecutionIo::new().unwrap();
        for (index, algorithm) in [
            Algorithm::Sha1,
            Algorithm::Sha256,
            Algorithm::Sha384,
            Algorithm::Sha512,
            Algorithm::Blake3,
        ]
        .into_iter()
        .enumerate()
        {
            let checksum = Checksum {
                algorithm,
                bytes: algorithm.hash(&bytes),
            };
            let result = io
                .download(
                    io.identity(),
                    true,
                    Download {
                        urls: &[&server.url],
                        output: &format!("file{index}"),
                        sha256: "",
                        integrity: &checksum.integrity(),
                        allow_fail: false,
                    },
                )
                .unwrap();
            assert_eq!(
                result.integrity.as_deref(),
                Some(checksum.integrity().as_str())
            );
            assert_eq!(result.sha256.is_some(), algorithm == Algorithm::Sha256);
            assert_eq!(result.size_bytes, Some(256));
        }
        assert_eq!(server.count(), 5);
        // A different digest algorithm can also reuse an owned file.
        assert!(
            io.download(
                io.identity(),
                false,
                Download {
                    urls: &[],
                    output: "file0",
                    sha256: &transport::sha256(&bytes),
                    integrity: "",
                    allow_fail: true
                }
            )
            .unwrap()
            .success
        );
        assert_eq!(server.count(), 5);
        let text = io.read(io.identity(), "file0", "auto").unwrap();
        assert_eq!(
            text.chars().map(|c| c as u32).collect::<Vec<_>>(),
            (0..=255).collect::<Vec<_>>()
        );
        assert_eq!(text.chars().next(), Some('\0'));
        assert_eq!(text, io.read(io.identity(), "file0", "no").unwrap());
        assert!(matches!(
            io.read(io.identity(), "file0", "yes"),
            Err(TypedIoError::Unsupported(_))
        ));
    }

    #[test]
    fn mirrors_query_html_statuses_and_retry_table() {
        for (status, count) in [
            (200, 1),
            (206, 1),
            (308, 1),
            (404, 1),
            (410, 1),
            (501, 1),
            (505, 1),
            (403, 2),
            (408, 2),
            (429, 2),
            (500, 2),
            (502, 2),
            (503, 2),
        ] {
            let server = Server::new(vec![
                response(status, b"<html>real bytes</html>", ""),
                response(200, b"<html>real bytes</html>", ""),
            ]);
            let mut io = ScratchExecutionIo::new().unwrap();
            let success = download(
                &mut io,
                &format!("{}/data?q=one&two=2", server.url),
                "same",
                &transport::sha256(b"<html>real bytes</html>"),
                true,
            )
            .unwrap();
            assert_eq!(
                success,
                matches!(status, 200 | 206) || count == 2,
                "{status}"
            );
            assert_eq!(server.count(), count, "{status}");
            let requests = server.requests.lock().unwrap();
            assert!(requests[0].starts_with("GET /data?q=one&two=2 HTTP/1.1"));
            assert!(!requests[0].to_lowercase().contains("authorization:"));
            assert!(!requests[0].to_lowercase().contains("referer:"));
        }
        let server = Server::new(vec![
            response(404, b"", ""),
            response(200, b"html", ""),
            response(200, b"wrong", ""),
        ]);
        let mut io = ScratchExecutionIo::new().unwrap();
        let urls = [
            format!("{}/404", server.url),
            format!("{}/html", server.url),
            format!("{}/unused", server.url),
        ];
        assert!(
            io.download(
                io.identity(),
                true,
                Download {
                    urls: &[&urls[0], &urls[1], &urls[2]],
                    output: "html",
                    sha256: &transport::sha256(b"html"),
                    integrity: "",
                    allow_fail: false
                }
            )
            .unwrap()
            .success
        );
        assert_eq!(server.count(), 2);
    }

    #[test]
    fn redirects_and_gzip_original_final_suffix_exceptions() {
        let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        encoder.write_all(b"decoded").unwrap();
        let gzip = encoder.finish().unwrap();
        for status in [301, 302, 303, 307] {
            for (original, final_path, encoding, decoded) in [
                ("/start", "/final", "gzip", true),
                ("/start", "/final", "x-gzip", true),
                ("/start.gz", "/final", "gzip", false),
                ("/start", "/final.tgz", "gzip", false),
            ] {
                let server = Server::new(vec![
                    response(status, b"", &format!("Location: {final_path}\r\n")),
                    response(200, &gzip, &format!("Content-Encoding: {encoding}\r\n")),
                ]);
                let mut io = ScratchExecutionIo::new().unwrap();
                let expected = if decoded {
                    b"decoded".as_slice()
                } else {
                    &gzip
                };
                assert!(
                    download(
                        &mut io,
                        &format!("{}{original}", server.url),
                        "file",
                        &transport::sha256(expected),
                        false
                    )
                    .unwrap()
                );
                assert_eq!(io.bytes("file").unwrap(), expected);
                assert_eq!(server.count(), 2);
            }
        }
        let server = Server::new(vec![response(
            200,
            b"bad gzip",
            "Content-Encoding: gzip\r\n",
        )]);
        let mut io = ScratchExecutionIo::new().unwrap();
        assert!(!download(&mut io, &server.url, "bad", &"00".repeat(32), true).unwrap());
    }

    #[test]
    fn all_url_and_path_preflight_is_before_io_and_http_filter_uses_raw_checksum() {
        let server = Server::new(vec![]);
        for bad in [
            "file:///tmp/no",
            "https://user:pass@localhost/file",
            "https://localhost/#fragment",
            "http:foo",
            "http://localhost/%xx",
            "not a url",
        ] {
            let mut io = ScratchExecutionIo::new().unwrap();
            assert!(
                io.download(
                    io.identity(),
                    true,
                    Download {
                        urls: &[&server.url, bad],
                        output: "out",
                        sha256: &"00".repeat(32),
                        integrity: "",
                        allow_fail: true
                    }
                )
                .is_err()
            );
            assert!(io.container.is_none());
        }
        for bad in [
            "/abs",
            "../escape",
            "a/../b",
            "a//b",
            "./a",
            "a\\b",
            "é",
            "a b",
            "a/",
            "a\0b",
        ] {
            let mut io = ScratchExecutionIo::new().unwrap();
            assert!(download(&mut io, &server.url, bad, &"00".repeat(32), true).is_err());
            assert!(io.container.is_none());
        }
        let mut io = ScratchExecutionIo::new().unwrap();
        assert!(!download(&mut io, &server.url, "out", "", true).unwrap());
        assert!(matches!(
            download(&mut io, &server.url, "out", "", false),
            Err(TypedIoError::Validation(_))
        ));
        assert_eq!(server.count(), 0);
        assert!(io.read(io.identity(), "missing", "auto").is_err());
    }

    #[test]
    fn symlink_hardlink_and_unowned_entries_are_hard_policy_errors() {
        use std::os::unix::fs::symlink;
        for kind in 0..3 {
            let mut io = ScratchExecutionIo::new().unwrap();
            seed(&mut io, "file", b"original");
            let file = io.destination("file").unwrap();
            let other = io.destination("other").unwrap();
            match kind {
                0 => symlink(&file, &other).unwrap(),
                1 => fs::hard_link(&file, &other).unwrap(),
                _ => fs::write(&other, b"alien").unwrap(),
            }
            let target = if kind == 1 { "file" } else { "other" };
            assert!(matches!(io.state(target), Err(TypedIoError::Policy(_))));
        }
        let mut io = ScratchExecutionIo::new().unwrap();
        seed(&mut io, "dir/file", b"old");
        let root = io.root().unwrap();
        fs::rename(root.join("dir"), root.join("moved")).unwrap();
        symlink(root.join("moved"), root.join("dir")).unwrap();
        assert!(matches!(io.bytes("dir/file"), Err(TypedIoError::Policy(_))));
    }

    #[test]
    fn cleanup_close_drop_unwind_and_identity_never_reused() {
        let mut io = ScratchExecutionIo::new().unwrap();
        let first = io.identity();
        seed(&mut io, "file", b"bytes");
        let container = io.container.as_ref().unwrap().path().to_owned();
        assert!(io.check_receiver(first + 1).is_err());
        io.close().unwrap();
        assert!(!container.exists());
        assert!(matches!(
            io.read(first, "file", "no"),
            Err(TypedIoError::Lifetime)
        ));
        let mut io = ScratchExecutionIo::new().unwrap();
        assert_ne!(io.identity(), first);
        seed(&mut io, "file", b"bytes");
        let container = io.container.as_ref().unwrap().path().to_owned();
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(move || {
            let _owner = io;
            panic!("fixture unwind");
        }));
        assert!(!container.exists());
    }

    #[test]
    fn calls_mirrors_file_live_and_work_limits_are_not_allow_fail() {
        let mut io = ScratchExecutionIo::new().unwrap();
        for _ in 0..32 {
            assert!(
                !io.download(
                    io.identity(),
                    false,
                    Download {
                        urls: &[],
                        output: "",
                        sha256: "",
                        integrity: "",
                        allow_fail: true
                    }
                )
                .unwrap()
                .success
            );
        }
        assert!(matches!(
            io.download(
                io.identity(),
                false,
                Download {
                    urls: &[],
                    output: "",
                    sha256: "",
                    integrity: "",
                    allow_fail: true
                }
            ),
            Err(TypedIoError::Resource(_))
        ));
        let mut io = ScratchExecutionIo::new().unwrap();
        assert!(matches!(
            io.download(
                io.identity(),
                false,
                Download {
                    urls: &["https://localhost"; 9],
                    output: "",
                    sha256: "",
                    integrity: "",
                    allow_fail: true
                }
            ),
            Err(TypedIoError::Resource(_))
        ));
        let body = vec![b'x'; FILE_LIMIT + 1];
        let server = Server::new(vec![response(200, &body, "")]);
        assert!(matches!(
            download(&mut io, &server.url, "large", &"00".repeat(32), true),
            Err(TypedIoError::Resource(_))
        ));
        assert_eq!(io.files["large"].size, FILE_LIMIT);
        let mut io = ScratchExecutionIo::new().unwrap();
        seed(&mut io, "one", &body[..FILE_LIMIT]);
        seed(&mut io, "two", &body[..FILE_LIMIT]);
        assert!(matches!(
            io.transfer(Box::new(std::io::Cursor::new(vec![b'a'])), "three", None),
            Err(TypedIoError::Resource(_))
        ));
        assert_eq!(io.files["three"].size, 0);
    }

    #[test]
    fn real_tls_private_trust_hostname_negative_and_downgrade_hard_policy() {
        let server = TlsServer::new();
        let mut trusted = ScratchExecutionIo::new()
            .unwrap()
            .with_client(transport::trusted_client(&server.pem));
        assert!(download(&mut trusted, &server.url, "data", "", false).unwrap());
        let observations = trusted.close().unwrap();
        match &observations[0] {
            Observation::Download { result, state, .. } => {
                assert_eq!(
                    result.sha256.as_deref(),
                    Some(transport::sha256(b"{\"real\":\"tls bytes\"}").as_str())
                );
                assert!(matches!(state, FileState::File { size_bytes: 20, .. }));
            }
            _ => panic!("download observation missing"),
        }
        let mut untrusted = ScratchExecutionIo::new().unwrap();
        assert!(!download(&mut untrusted, &server.url, "data", "", true).unwrap());
        let mut wrong_host = ScratchExecutionIo::new()
            .unwrap()
            .with_client(transport::trusted_client(&server.pem));
        assert!(
            !download(
                &mut wrong_host,
                &server.url.replace("localhost", "127.0.0.1"),
                "data",
                "",
                true
            )
            .unwrap()
        );
        for route in ["downgrade", "downgrade301"] {
            let mut downgrade = ScratchExecutionIo::new()
                .unwrap()
                .with_client(transport::trusted_client(&server.pem));
            assert!(matches!(
                download(
                    &mut downgrade,
                    &format!("{}/{route}", server.url),
                    "data",
                    "",
                    true
                ),
                Err(TypedIoError::Policy(_))
            ));
        }
    }

    #[test]
    fn gzip_bomb_is_resource_failure_and_new_file_uses_normal_creation_mode() {
        use std::os::unix::fs::PermissionsExt;
        let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        encoder.write_all(&vec![b'x'; FILE_LIMIT + 1]).unwrap();
        let gzip = encoder.finish().unwrap();
        let server = Server::new(vec![response(200, &gzip, "Content-Encoding: gzip\r\n")]);
        let mut io = ScratchExecutionIo::new().unwrap();
        assert!(matches!(
            download(&mut io, &server.url, "bomb", &"00".repeat(32), true),
            Err(TypedIoError::Resource(_))
        ));
        assert_eq!(io.files["bomb"].size, FILE_LIMIT);
        let comparison = io.container.as_ref().unwrap().path().join("normal-file");
        fs::write(&comparison, b"normal").unwrap();
        assert_eq!(
            fs::metadata(io.destination("bomb").unwrap())
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            fs::metadata(comparison).unwrap().permissions().mode() & 0o777
        );
    }
}
