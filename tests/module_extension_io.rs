//! Real CLI/module callback -> checksummed local HTTP -> JSON -> generated
//! repository -> consumer bytes. No N2/passthrough rule or public network.
use sha2::{Digest, Sha256};
use std::fs;
use std::io::{Read, Write};
use std::net::TcpListener;
use std::path::PathBuf;
use std::process::{Command, Output};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
};
use std::thread;
use std::time::Duration;

struct Server {
    url: String,
    requests: Arc<Mutex<Vec<String>>>,
    stop: Arc<AtomicBool>,
    worker: Option<thread::JoinHandle<()>>,
}
impl Server {
    fn new(body: &[u8]) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let url = format!(
            "http://{}/versions.json?include=all",
            listener.local_addr().unwrap()
        );
        let body = body.to_vec();
        let requests = Arc::new(Mutex::new(Vec::new()));
        let logged = requests.clone();
        let stop = Arc::new(AtomicBool::new(false));
        let stopped = stop.clone();
        let worker = thread::spawn(move || {
            while !stopped.load(Ordering::SeqCst) {
                let (mut stream, _) = match listener.accept() {
                    Ok(pair) => pair,
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(2));
                        continue;
                    }
                    Err(e) => panic!("{e}"),
                };
                stream
                    .set_read_timeout(Some(Duration::from_secs(2)))
                    .unwrap();
                stream
                    .set_write_timeout(Some(Duration::from_secs(2)))
                    .unwrap();
                let mut header = Vec::new();
                let mut byte = [0];
                while header.len() < 16384 && !header.ends_with(b"\r\n\r\n") {
                    if stream.read(&mut byte).unwrap_or(0) == 0 {
                        break;
                    }
                    header.push(byte[0]);
                }
                logged
                    .lock()
                    .unwrap()
                    .push(String::from_utf8(header).unwrap());
                write!(
                    stream,
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                )
                .unwrap();
                let _ = stream.write_all(&body);
            }
        });
        Self {
            url,
            requests,
            stop,
            worker: Some(worker),
        }
    }
    fn count(&self) -> usize {
        self.requests.lock().unwrap().len()
    }
}
impl Drop for Server {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        self.worker.take().unwrap().join().unwrap();
    }
}
struct Fixture(tempfile::TempDir);
impl Fixture {
    fn new(url: &str, sha: &str) -> Self {
        let f = Self(
            tempfile::Builder::new()
                .prefix("module-extension-io-cli-")
                .tempdir_in("/tmp/opencode")
                .unwrap(),
        );
        for directory in ["home", "tmp", "xdg"] {
            fs::create_dir(f.0.path().join(directory)).unwrap();
        }
        f.file("MODULE.bazel", "module(name='main')\ne=use_extension('//:ext.bzl','files')\nuse_repo(e,chosen='made')\n");
        f.file("BUILD.bazel", "genrule(name='copy', srcs=['@chosen//:data.txt'], outs=['copy.txt'], cmd='cat $(SRCS) > $@')\n");
        f.file("rules.bzl", "def _impl(ctx):\n    ctx.file('BUILD.bazel', \"exports_files(['data.txt'])\\n\")\n    ctx.file('data.txt', ctx.attr.text, executable=False)\nmaker=repository_rule(_impl, attrs={'text':attr.string()})\n");
        f.extension(&format!("result = ctx.download({url:?}, 'versions.json', {sha:?})\nresult.success or fail(result)\ndata = json.decode(ctx.read('versions.json'))\nmaker(name='made', text=data['version'] + ':' + data['files'][0]['sha256'])"));
        f
    }
    fn file(&self, path: &str, contents: impl AsRef<[u8]>) {
        fs::write(self.0.path().join(path), contents).unwrap();
    }
    fn extension(&self, body: &str) {
        let indented = body
            .lines()
            .map(|line| format!("    {line}\n"))
            .collect::<String>();
        self.file("ext.bzl", format!("load(':rules.bzl','maker')\ndef _impl(ctx):\n{indented}files=module_extension(_impl)\n"));
    }
    fn run(&self, args: &[&str]) -> Output {
        Command::new("/usr/bin/timeout")
            .args([
                "--kill-after=2s",
                "30s",
                env!("CARGO_BIN_EXE_bazel-rs"),
                "--ignore_all_rc_files",
                "--cache-dir=cache",
            ])
            .args(args)
            .current_dir(self.0.path())
            .env_clear()
            .env("PATH", "/usr/bin:/bin")
            .env("HOME", self.0.path().join("home"))
            .env("TMPDIR", self.0.path().join("tmp"))
            .env("XDG_CONFIG_HOME", self.0.path().join("xdg"))
            .env("LANG", "C")
            .output()
            .unwrap()
    }
    fn build(&self, args: &[&str]) -> Vec<u8> {
        let result = self.run(args);
        assert!(
            result.status.success(),
            "{}",
            String::from_utf8_lossy(&result.stderr)
        );
        let output = String::from_utf8(result.stdout).unwrap();
        let (_, path) = output.lines().next().unwrap().split_once('\t').unwrap();
        fs::read(path).unwrap()
    }
    fn roots(&self) -> Vec<PathBuf> {
        let mut roots = fs::read_dir(self.0.path().join("cache/repositories/direct/trees"))
            .map(|entries| {
                entries
                    .map(|e| e.unwrap().path())
                    .filter(|p| !p.file_name().unwrap().to_string_lossy().starts_with('.'))
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        roots.sort();
        roots
    }
    fn assert_clean_scratch(&self) {
        assert_eq!(fs::read_dir(self.0.path().join("tmp")).unwrap().count(), 0);
    }
}
fn sha(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}
const BODY: &[u8] = br#"{"version":"go1.99.1","files":[{"sha256":"real-fixture-digest"}]}"#;

include!("support/extension_io_preflight_cases.rs");
include!("support/extension_io_boundary_cli.rs");

#[test]
fn checksummed_json_creates_real_repository_and_consumer_cache_but_fresh_cli_refetches() {
    let server = Server::new(BODY);
    let fixture = Fixture::new(&server.url, &sha(BODY));
    let expected = b"go1.99.1:real-fixture-digest";
    assert_eq!(fixture.build(&["build", "//:copy"]), expected);
    let roots = fixture.roots();
    assert_eq!(roots.len(), 1);
    assert_eq!(fs::read(roots[0].join("data.txt")).unwrap(), expected);
    assert_eq!(server.count(), 1);
    assert_eq!(fixture.build(&["build", "//:copy"]), expected);
    assert_eq!(server.count(), 2);
    assert_eq!(fixture.roots(), roots);
    assert_eq!(fixture.build(&["build", "@chosen//:data.txt"]), expected);
    assert_eq!(server.count(), 3);
    assert_eq!(fixture.roots(), roots);
    assert!(
        server
            .requests
            .lock()
            .unwrap()
            .iter()
            .all(|r| r.starts_with("GET /versions.json?include=all HTTP/1.1"))
    );
    fixture.assert_clean_scratch();
}

#[test]
fn disable_is_default_false_and_failure_result_is_only_success_false() {
    let server = Server::new(BODY);
    let fixture = Fixture::new(&server.url, &sha(BODY));
    let result = fixture.run(&["build", "--repository_disable_download", "//:copy"]);
    assert!(!result.status.success());
    assert!(String::from_utf8_lossy(&result.stderr).contains("downloads are disabled"));
    assert_eq!(server.count(), 0);
    assert!(fixture.roots().is_empty());
    fixture.extension(&format!("r = ctx.download({:?}, sha256={:?}, allow_fail=True)\ndir(r) == ['success'] and r.success == False or fail(r)\nmaker(name='made', text='tolerated-real-failure')", server.url, sha(BODY)));
    assert_eq!(
        fixture.build(&["build", "--repository_disable_download", "//:copy"]),
        b"tolerated-real-failure"
    );
    assert_eq!(server.count(), 0);
    fixture.assert_clean_scratch();
}

#[test]
fn failed_callback_invalid_declaration_and_bad_import_publish_nothing_after_network() {
    for variant in ["callback", "declaration", "import"] {
        let server = Server::new(BODY);
        let fixture = Fixture::new(&server.url, &sha(BODY));
        let mut body = format!(
            "ctx.download({:?}, 'data', {:?})\nmaker(name='made', text='not published')",
            server.url,
            sha(BODY)
        );
        let marker = match variant {
            "callback" => {
                body.push_str("\nfail('AFTER_IO')");
                "AFTER_IO"
            }
            "declaration" => {
                body.push_str("\nmaker(name='unused', unknown='bad')");
                "unknown"
            }
            _ => {
                fixture.file("MODULE.bazel", "module(name='main')\ne=use_extension('//:ext.bzl','files')\nuse_repo(e,chosen='made',bad='missing')\n");
                "missing"
            }
        };
        fixture.extension(&body);
        let result = fixture.run(&["build", "//:copy"]);
        assert!(!result.status.success());
        assert_ne!(result.status.code(), Some(124));
        assert!(
            String::from_utf8_lossy(&result.stderr).contains(marker),
            "{}",
            String::from_utf8_lossy(&result.stderr)
        );
        assert_eq!(server.count(), 1);
        assert!(fixture.roots().is_empty());
        fixture.assert_clean_scratch();
    }
}

#[test]
fn default_output_read_and_matching_destination_reuse_do_not_guess_basename() {
    let server = Server::new(BODY);
    let fixture = Fixture::new(&server.url, &sha(BODY));
    fixture.extension(&format!("ctx.download({:?}, sha256={:?})\nctx.download([], sha256={:?}, allow_fail=True).success or fail('reuse')\nmaker(name='made', text=json.decode(ctx.read(''))['version'])", server.url, sha(BODY), sha(BODY)));
    assert_eq!(fixture.build(&["build", "//:copy"]), b"go1.99.1");
    assert_eq!(server.count(), 1);
    fixture.assert_clean_scratch();
}
