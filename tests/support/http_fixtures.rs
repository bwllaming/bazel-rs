// Included inside extension_io::tests: test-only HTTP(S) fixtures.
use std::fs;
use std::io::{Read, Write};
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
                    .set_read_timeout(Some(Duration::from_secs(30)))
                    .unwrap();
                stream
                    .set_write_timeout(Some(Duration::from_secs(30)))
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
            .tempdir_in(crate::test_temp::root())
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
