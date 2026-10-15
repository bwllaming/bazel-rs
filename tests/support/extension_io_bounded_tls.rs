// Linux test-only alternative: never changes/uses the unbounded TlsServer::new.
// All fixture processes are DIRECT children of this Rust test process:
// bash execs openssl/python; the pinned Python program never forks/spawns.
// Each command has a separate process group. Cleanup signals the whole group,
// reaps the direct child, and requires group absence (including zombies).
// This is cooperative OS supervision, not a guarantee against SIGKILL/OOM of
// the test runner, group escape, kernel D-state, or hostile same-UID mutation.
mod n3_tls_process {
    use std::fs::{self, File};
    use std::io::Read;
    use std::os::unix::fs::MetadataExt;
    use std::os::unix::process::CommandExt;
    use std::path::{Path, PathBuf};
    use std::process::{Child, Command, ExitStatus, Stdio};
    use std::time::{Duration, Instant};

    const POLL: Duration = Duration::from_millis(5);
    const CLEANUP: Duration = Duration::from_secs(2);
    // $0 is n3-owned-test, $1 is the fresh owned receipt; after shift, $1 is
    // the executable and the remaining positional arguments pass through intact.
    // Builtins/redirection only before exec: no hidden shell child is created.
    const CHILD_WRAPPER: &str = "builtin umask 022 || exit 125; set -o noclobber || exit 125; builtin umask > \"$1\" || exit 125; shift; ulimit -f 128 || exit 125; exec \"$@\"";

    fn reap(child: &mut Child, deadline: Instant) -> Result<ExitStatus, String> {
        loop {
            if let Some(status) = child.try_wait().map_err(|e| e.to_string())? {
                return Ok(status);
            }
            if Instant::now() >= deadline {
                return Err("child reap/deadline exceeded".into());
            }
            std::thread::sleep(POLL);
        }
    }

    fn group_exists(group: u32) -> Result<bool, String> {
        // Ask the kernel (kill(2) with signal 0 on the group) instead of
        // parsing every host process's /proc stat: foreign processes must not
        // be able to fail this owned-group check. Exit 0 iff the group has a
        // member. kill(1) exits 1 for ESRCH and EPERM alike, so absence is
        // only its exact C-locale (empty env) ESRCH report for this argument,
        // read from a bounded stderr capture; anything else is an error.
        // No shell or inherited env.
        if group <= 1 {
            return Err("refusing non-private group".into());
        }
        let mut probe = Command::new("/usr/bin/kill")
            .args(["-0", "--", &format!("-{group}")])
            .env_clear()
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| e.to_string())?;
        let pipe = probe.stderr.take().ok_or("group probe stderr missing")?;
        let (sender, receiver) = std::sync::mpsc::channel();
        // Detached only if the pipe never closes; storage is bounded either way.
        std::thread::spawn(move || {
            let mut bytes = Vec::new();
            let read = pipe.take(4097).read_to_end(&mut bytes);
            let _ = sender.send(read.map(|_| bytes));
        });
        let status = match reap(&mut probe, Instant::now() + CLEANUP) {
            Ok(status) => status,
            Err(first) => {
                let killed = probe.kill();
                let reaped = reap(&mut probe, Instant::now() + CLEANUP);
                return Err(format!(
                    "group probe failed: {first}; kill={killed:?}; reap={reaped:?}"
                ));
            }
        };
        let stderr = match receiver.recv_timeout(CLEANUP) {
            Ok(Ok(bytes)) if bytes.len() <= 4096 => bytes,
            other => return Err(format!("group probe stderr unusable: {other:?}")),
        };
        if status.success() {
            return Ok(true);
        }
        let esrch = std::str::from_utf8(&stderr)
            .ok()
            .and_then(|text| text.strip_suffix('\n'))
            .is_some_and(|line| {
                !line.contains('\n')
                    && (line.ends_with(&format!(": (-{group}): No such process"))
                        || line.ends_with(&format!(
                            ": sending signal to -{group} failed: No such process"
                        )))
            });
        if status.code() == Some(1) && esrch {
            Ok(false)
        } else {
            Err(format!(
                "group probe failed ({status}): {:?}",
                String::from_utf8_lossy(&stderr)
            ))
        }
    }

    fn signal_group(group: u32) -> Result<(), String> {
        if group <= 1 {
            return Err("refusing non-private group".into());
        }
        // Trusted kill(1) never spawns descendants. No shell or inherited env.
        let mut helper = Command::new("/usr/bin/kill")
            .args(["-KILL", "--", &format!("-{group}")])
            .env_clear()
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|e| e.to_string())?;
        match reap(&mut helper, Instant::now() + CLEANUP) {
            Ok(status) if status.success() => Ok(()),
            Ok(status) => {
                if group_exists(group)? {
                    Err(format!("group kill failed: {status}"))
                } else {
                    Ok(())
                }
            }
            Err(first) => {
                let killed = helper.kill();
                let reaped = reap(&mut helper, Instant::now() + CLEANUP);
                Err(format!(
                    "kill helper failed: {first}; kill={killed:?}; reap={reaped:?}"
                ))
            }
        }
    }

    pub(super) struct OwnedProcess {
        child: Option<Child>,
        group: u32,
        stdout: PathBuf,
        stderr: PathBuf,
        mask_receipt: PathBuf,
        root_identity: (u64, u64),
        mask_identity: Option<(u64, u64)>,
    }
    impl OwnedProcess {
        pub(super) fn spawn(
            root: &Path,
            stem: &str,
            program: &str,
            args: &[&str],
        ) -> Result<Self, String> {
            Self::spawn_with_env(root, stem, program, args, &[])
        }
        pub(super) fn spawn_with_env(
            root: &Path,
            stem: &str,
            program: &str,
            args: &[&str],
            env: &[(&str, &str)],
        ) -> Result<Self, String> {
            // Private TempDir root, no symlink ancestors/aliases or path-bearing stem.
            if !root.is_absolute()
                || fs::canonicalize(root).map_err(|e| e.to_string())? != root
                || stem.is_empty()
                || !stem.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
            {
                return Err("invalid owned mask receipt root/stem".into());
            }
            let root_meta = fs::symlink_metadata(root).map_err(|e| e.to_string())?;
            println!(
                "N3_PRIVATE_ROOT path={} mode={:04o} dev={} ino={}",
                root.display(),
                root_meta.mode() & 0o7777,
                root_meta.dev(),
                root_meta.ino()
            );
            if !root_meta.is_dir() || root_meta.mode() & 0o077 != 0 {
                return Err("mask receipt root is not a private real directory".into());
            }
            let mask_receipt = root.join(format!("{stem}.umask"));
            match fs::symlink_metadata(&mask_receipt) {
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                other => return Err(format!("mask receipt must be new: {other:?}")),
            }
            // Guard exists before startup; no assertion, fallible allocation, or
            // readiness operation between successful spawn and installing Child.
            let mut owned = Self {
                child: None,
                group: 0,
                stdout: root.join(format!("{stem}.stdout")),
                stderr: root.join(format!("{stem}.stderr")),
                mask_receipt,
                root_identity: (root_meta.dev(), root_meta.ino()),
                mask_identity: None,
            };
            let stdout = File::create(&owned.stdout).map_err(|e| e.to_string())?;
            let stderr = File::create(&owned.stderr).map_err(|e| e.to_string())?;
            let mut command = Command::new("/bin/bash");
            command
                .args(["-c", CHILD_WRAPPER, "n3-owned-test"])
                .arg(&owned.mask_receipt)
                .arg(program)
                .args(args)
                .current_dir(root)
                .env_clear()
                .env("PATH", "/usr/bin:/bin")
                .env("HOME", root)
                .env("TMPDIR", root)
                .env("LANG", "C")
                .envs(env.iter().copied())
                .stdin(Stdio::null())
                .stdout(Stdio::from(stdout))
                .stderr(Stdio::from(stderr))
                .process_group(0);
            owned.child = Some(command.spawn().map_err(|e| e.to_string())?);
            owned.group = owned.child.as_ref().unwrap().id();
            Ok(owned)
        }
        pub(super) fn verify_umask(&mut self) -> Result<(), String> {
            // Ordinary owned filesystem, not protection against hostile same-UID races.
            let root = self.mask_receipt.parent().ok_or("missing receipt parent")?;
            let root_meta = fs::symlink_metadata(root).map_err(|e| e.to_string())?;
            if !root_meta.is_dir()
                || root_meta.mode() & 0o077 != 0
                || (root_meta.dev(), root_meta.ino()) != self.root_identity
            {
                return Err("mask receipt root changed".into());
            }
            let named = fs::symlink_metadata(&self.mask_receipt)
                .map_err(|e| format!("missing mask receipt: {e}"))?;
            if !named.file_type().is_file() || named.nlink() != 1 || named.len() != 5 {
                return Err("mask receipt is not a five-byte single-link regular file".into());
            }
            let identity = (named.dev(), named.ino());
            if self
                .mask_identity
                .is_some_and(|previous| previous != identity)
            {
                return Err("mask receipt identity changed".into());
            }
            let file = File::open(&self.mask_receipt).map_err(|e| e.to_string())?;
            let opened = file.metadata().map_err(|e| e.to_string())?;
            if !opened.is_file()
                || opened.nlink() != 1
                || opened.len() != 5
                || (opened.dev(), opened.ino()) != identity
            {
                return Err("mask receipt open identity changed".into());
            }
            let mut bytes = Vec::new();
            (&file)
                .take(6)
                .read_to_end(&mut bytes)
                .map_err(|e| e.to_string())?;
            if bytes.as_slice() != b"0022\n" {
                return Err("mask receipt is not exact 0022 LF".into());
            }
            let after = fs::symlink_metadata(&self.mask_receipt).map_err(|e| e.to_string())?;
            let held = file.metadata().map_err(|e| e.to_string())?;
            if !after.file_type().is_file()
                || after.nlink() != 1
                || after.len() != 5
                || (after.dev(), after.ino()) != identity
                || (held.dev(), held.ino()) != identity
                || held.nlink() != 1
                || held.len() != 5
            {
                return Err("mask receipt changed during bounded read".into());
            }
            self.mask_identity = Some(identity);
            println!(
                "N3_UMASK_RECEIPT file={} bytes_hex=303032320a",
                self.mask_receipt.display()
            );
            Ok(())
        }
        pub(super) fn running(&mut self) -> Result<bool, String> {
            Ok(self
                .child
                .as_mut()
                .ok_or("child already closed")?
                .try_wait()
                .map_err(|e| e.to_string())?
                .is_none())
        }
        fn logs(&self) -> String {
            let mut out = String::new();
            for path in [&self.stdout, &self.stderr] {
                let mut bytes = Vec::new();
                if let Ok(file) = File::open(path) {
                    let _ = file.take(16384).read_to_end(&mut bytes);
                }
                out.push_str(&String::from_utf8_lossy(&bytes));
            }
            out
        }
        pub(super) fn wait_success(mut self) -> Result<(), String> {
            self.finish(Duration::from_secs(10)).map(|_| ())
        }
        pub(super) fn finish(&mut self, timeout: Duration) -> Result<String, String> {
            let status = reap(self.child.as_mut().unwrap(), Instant::now() + timeout);
            let cleanup = self.cleanup();
            match (status, cleanup) {
                (Ok(status), Ok(())) if status.success() => {
                    self.verify_umask()?;
                    let mut bytes = Vec::new();
                    File::open(&self.stdout)
                        .map_err(|e| e.to_string())?
                        .take(131073)
                        .read_to_end(&mut bytes)
                        .map_err(|e| e.to_string())?;
                    if bytes.len() > 131072 {
                        return Err("stdout limit exceeded".into());
                    }
                    String::from_utf8(bytes).map_err(|e| e.to_string())
                }
                other => Err(format!("TLS setup failed: {other:?}\n{}", self.logs())),
            }
        }
        pub(super) fn cleanup(&mut self) -> Result<(), String> {
            let Some(child) = &mut self.child else {
                return Ok(());
            };
            let signal = signal_group(self.group);
            // A signal-helper failure still attempts direct-child kill and reap.
            if signal.is_err() {
                let _ = child.kill();
            }
            let reaped = reap(child, Instant::now() + CLEANUP);
            let gone = (|| {
                let deadline = Instant::now() + CLEANUP;
                while group_exists(self.group)? {
                    if Instant::now() >= deadline {
                        return Err("owned group still present after kill/reap".to_owned());
                    }
                    std::thread::sleep(POLL);
                }
                Ok(())
            })();
            if reaped.is_ok() && gone.is_ok() {
                self.child.take();
            }
            match (signal, reaped, gone) {
                (Ok(()), Ok(_), Ok(())) => Ok(()),
                other => Err(format!("TLS process cleanup failed (NOT PASS): {other:?}")),
            }
        }
    }
    impl Drop for OwnedProcess {
        fn drop(&mut self) {
            if let Err(error) = self.cleanup() {
                if std::thread::panicking() {
                    eprintln!("{error}");
                } else {
                    panic!("{error}");
                }
            }
        }
    }
}

pub(crate) fn n3_run_test_child(
    root: &Path,
    filter: &str,
    marker: &str,
    extra_env: &[(&str, &str)],
) {
    let executable = std::env::current_exe().unwrap();
    let home = root.join("home");
    let tmp = root.join("tmp");
    assert!(home.is_dir() && tmp.is_dir());
    let mut env = vec![
        ("HOME", home.to_str().unwrap()),
        ("TMPDIR", tmp.to_str().unwrap()),
    ];
    env.extend_from_slice(extra_env);
    let mut child = n3_tls_process::OwnedProcess::spawn_with_env(
        root,
        "test-child",
        executable.to_str().unwrap(),
        &[
            "--exact",
            filter,
            "--ignored",
            "--nocapture",
            "--test-threads=1",
        ],
        &env,
    )
    .unwrap();
    let stdout = child.finish(std::time::Duration::from_secs(30)).unwrap();
    assert!(stdout.contains("running 1 test"), "{filter}: {stdout}");
    assert!(
        stdout.contains("1 passed; 0 failed; 0 ignored"),
        "{filter}: {stdout}"
    );
    assert_eq!(
        stdout.lines().filter(|line| *line == marker).count(),
        1,
        "{filter}: {stdout}"
    );
    // Parent's --nocapture receipt retains exact child count, marker and mode controls.
    println!("N3_CHILD_RECEIPT filter={filter}\n{stdout}");
}

pub(crate) struct N3BoundedTlsServer {
    pub(crate) url: String,
    process: n3_tls_process::OwnedProcess,
    private: tempfile::TempDir,
}
impl N3BoundedTlsServer {
    pub(crate) fn new() -> Self {
        use std::io::Read;
        use std::os::unix::fs::PermissionsExt;
        let private = tempfile::Builder::new()
            .prefix("n3-bounded-tls-")
            .permissions(fs::Permissions::from_mode(0o700))
            .tempdir_in("/tmp/opencode")
            .unwrap();
        let openssl = |stem: &str, args: &[&str]| {
            n3_tls_process::OwnedProcess::spawn(private.path(), stem, "/usr/bin/openssl", args)
                .unwrap()
                .wait_success()
                .unwrap();
        };
        // Same private-CA/localhost certificate recipe as the retained fixture.
        openssl(
            "ca",
            &[
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
            ],
        );
        openssl(
            "csr",
            &[
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
            ],
        );
        fs::write(private.path().join("server.ext"), "basicConstraints=critical,CA:FALSE\nkeyUsage=critical,digitalSignature,keyEncipherment\nextendedKeyUsage=serverAuth\nsubjectAltName=DNS:localhost\n").unwrap();
        openssl(
            "cert",
            &[
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
            ],
        );
        // No subprocesses, threads, fork, setsid, shell, or process-group escape.
        // Readiness is an atomic bounded regular file, never a blocking pipe.
        let script = r#"import http.server, ssl, os
class Handler(http.server.BaseHTTPRequestHandler):
 def log_message(self, *args): pass
 def do_GET(self):
  with open('requests.log', 'a') as log: log.write('GET\n')
  body = b'{"real":"tls bytes"}'
  self.send_response(200)
  self.send_header('Content-Length', str(len(body)))
  self.end_headers()
  self.wfile.write(body)
server = http.server.HTTPServer(('127.0.0.1', 0), Handler)
context = ssl.SSLContext(ssl.PROTOCOL_TLS_SERVER)
context.load_cert_chain('cert.pem', 'key.pem')
server.socket = context.wrap_socket(server.socket, server_side=True)
with open('ready.tmp', 'w') as ready: ready.write(str(server.server_port) + '\n')
os.replace('ready.tmp', 'ready')
server.serve_forever()
"#;
        let mut process = n3_tls_process::OwnedProcess::spawn(
            private.path(),
            "server",
            "/usr/bin/python3",
            &["-u", "-c", script],
        )
        .unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        let port: u16 = loop {
            assert!(
                process.running().unwrap(),
                "TLS server exited before readiness"
            );
            assert!(
                std::time::Instant::now() < deadline,
                "TLS readiness deadline exceeded"
            );
            match fs::File::open(private.path().join("ready")) {
                Ok(file) => {
                    let mut ready = String::new();
                    file.take(33).read_to_string(&mut ready).unwrap();
                    assert!(
                        ready.len() <= 32 && ready.ends_with('\n'),
                        "invalid readiness record"
                    );
                    break ready.trim().parse().expect("invalid TLS port");
                }
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                    std::thread::sleep(std::time::Duration::from_millis(5))
                }
                Err(e) => panic!("TLS readiness: {e}"),
            }
        };
        assert_ne!(port, 0);
        // The live process guard already owns cleanup if receipt validation fails.
        process.verify_umask().unwrap();
        Self {
            url: format!("https://localhost:{port}"),
            process,
            private,
        }
    }
    pub(crate) fn n3_http_count(&self) -> usize {
        use std::io::Read;
        match fs::File::open(self.private.path().join("requests.log")) {
            Ok(file) => {
                let mut bytes = Vec::new();
                file.take(131073).read_to_end(&mut bytes).unwrap();
                assert!(bytes.len() <= 131072, "HTTP request log overflow");
                String::from_utf8(bytes).unwrap().lines().count()
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => 0,
            Err(e) => panic!("TLS request log: {e}"),
        }
    }
    pub(crate) fn close(mut self) {
        self.process.cleanup().unwrap();
        self.process.verify_umask().unwrap();
        let path = self.private.path().to_owned();
        self.private.close().unwrap();
        assert!(!path.exists(), "TLS fixture cleanup incomplete");
    }
}
