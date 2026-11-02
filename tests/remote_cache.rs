//! N41: credential helpers and the HTTP remote cache (docs/REMOTE_CACHE.md).
//! Every test uses a loopback cache/registry server and local helper scripts;
//! nothing contacts a real cache, registry or credential helper.
#![allow(clippy::disallowed_methods)] // test harness: starts processes directly (clippy.toml)
use std::collections::BTreeMap;
use std::fs;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

// Scripts the CLI executes are never written through a descriptor of this
// multi-threaded, spawning test process (ETXTBSY race).
include!("support/executable_script.rs");
include!("support/verify_env.rs");
include!("support/explicit_modules.rs");

static NEXT: AtomicUsize = AtomicUsize::new(0);

#[derive(Clone, Debug)]
struct Request {
    method: String,
    path: String,
    headers: Vec<(String, String)>,
}

impl Request {
    fn header(&self, name: &str) -> Vec<&str> {
        self.headers
            .iter()
            .filter(|(key, _)| key.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.as_str())
            .collect()
    }
}

#[derive(Default)]
struct State {
    blobs: BTreeMap<String, Vec<u8>>,
    log: Vec<Request>,
    /// Transient faults: the next `unavailable` requests answer 503, the
    /// next `dropped` ones close the connection without an answer.
    unavailable: usize,
    dropped: usize,
}

/// A minimal HTTP/1.1 cache: GET/PUT `/ac/H` and `/cas/H`, other GETs from
/// the static `files`; `fail` answers every request with 500.
struct Server {
    port: u16,
    state: Arc<Mutex<State>>,
    fail: Arc<AtomicBool>,
}

fn handle(mut stream: TcpStream, state: Arc<Mutex<State>>, fail: Arc<AtomicBool>) {
    let mut reader = BufReader::new(stream.try_clone().unwrap());
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line).unwrap_or(0) == 0 {
            return;
        }
        let mut parts = line.split_whitespace();
        let (method, path) = (
            parts.next().unwrap_or("").to_owned(),
            parts.next().unwrap_or("").to_owned(),
        );
        let mut headers = Vec::new();
        loop {
            let mut header = String::new();
            reader.read_line(&mut header).unwrap();
            let header = header.trim_end();
            if header.is_empty() {
                break;
            }
            let (name, value) = header.split_once(':').unwrap();
            headers.push((name.trim().to_owned(), value.trim().to_owned()));
        }
        let length = headers
            .iter()
            .find(|(name, _)| name.eq_ignore_ascii_case("content-length"))
            .map_or(0, |(_, value)| value.parse::<usize>().unwrap());
        let mut body = vec![0; length];
        reader.read_exact(&mut body).unwrap();
        let mut state = state.lock().unwrap();
        state.log.push(Request {
            method: method.clone(),
            path: path.clone(),
            headers,
        });
        if state.dropped > 0 {
            state.dropped -= 1;
            return;
        }
        let (status, payload) = if fail.load(Ordering::SeqCst) {
            ("500 Internal Server Error", b"broken".to_vec())
        } else if state.unavailable > 0 {
            state.unavailable -= 1;
            ("503 Service Unavailable", b"busy".to_vec())
        } else if method == "PUT" {
            state.blobs.insert(path, body);
            ("200 OK", Vec::new())
        } else {
            match state.blobs.get(&path) {
                Some(bytes) => ("200 OK", bytes.clone()),
                None => ("404 Not Found", Vec::new()),
            }
        };
        drop(state);
        let head = format!(
            "HTTP/1.1 {status}\r\nContent-Length: {}\r\n\r\n",
            payload.len()
        );
        if stream.write_all(head.as_bytes()).is_err() || stream.write_all(&payload).is_err() {
            return;
        }
    }
}

impl Server {
    fn new() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let state = Arc::new(Mutex::new(State::default()));
        let fail = Arc::new(AtomicBool::new(false));
        let (accept_state, accept_fail) = (state.clone(), fail.clone());
        std::thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                let (state, fail) = (accept_state.clone(), accept_fail.clone());
                std::thread::spawn(move || handle(stream, state, fail));
            }
        });
        Self { port, state, fail }
    }
    fn url(&self) -> String {
        format!("http://127.0.0.1:{}", self.port)
    }
    fn serve(&self, path: &str, bytes: &[u8]) {
        self.state
            .lock()
            .unwrap()
            .blobs
            .insert(path.to_owned(), bytes.to_vec());
    }
    fn remove(&self, path: &str) -> bool {
        self.state.lock().unwrap().blobs.remove(path).is_some()
    }
    fn faults(&self, unavailable: usize, dropped: usize) {
        let mut state = self.state.lock().unwrap();
        state.unavailable = unavailable;
        state.dropped = dropped;
    }
    fn take_log(&self) -> Vec<Request> {
        std::mem::take(&mut self.state.lock().unwrap().log)
    }
    fn summary(&self) -> Vec<String> {
        let mut lines = self
            .take_log()
            .iter()
            .map(|request| format!("{} {}", request.method, request.path))
            .collect::<Vec<_>>();
        lines.sort();
        lines
    }
    fn keys(&self, prefix: &str) -> Vec<String> {
        self.state
            .lock()
            .unwrap()
            .blobs
            .keys()
            .filter_map(|path| path.strip_prefix(prefix).map(str::to_owned))
            .collect()
    }
}

struct Workspace(PathBuf);

impl Workspace {
    fn new() -> Self {
        let root = crate::test_temp::root().join(format!(
            "native-remote-cache-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join("ws/pkg")).unwrap();
        fs::create_dir_all(root.join("home")).unwrap();
        let ws = Self(root);
        // The genuine Bazel 9.3.0 probe fixture (docs/REMOTE_CACHE.md).
        ws.file("MODULE.bazel", "module(name = \"rc\")\n");
        ws.file(
            "pkg/BUILD.bazel",
            "load(\":defs.bzl\", \"runner\", \"shell\")\ngenrule(name = \"gen\", srcs = [\"in.txt\"], outs = [\"gen.txt\"], cmd = \"cat $< > $@; echo done >> $@\")\nrunner(name = \"run\", src = \"in.txt\", tool = \"tool.sh\")\nshell(name = \"sh\", src = \"in.txt\")\n",
        );
        ws.file(
            "pkg/defs.bzl",
            r#"def _runner(ctx):
    out = ctx.actions.declare_file(ctx.label.name + ".out")
    ctx.actions.run(executable = ctx.file.tool, arguments = [ctx.file.src.path, out.path], inputs = [ctx.file.src], outputs = [out], mnemonic = "RunTool")
    return [DefaultInfo(files = depset([out]))]
runner = rule(_runner, attrs = {"src": attr.label(allow_single_file = True), "tool": attr.label(allow_single_file = True)})
def _shell(ctx):
    out = ctx.actions.declare_file(ctx.label.name + ".out")
    ctx.actions.run_shell(command = "tr a-z A-Z < $1 > $2", arguments = [ctx.file.src.path, out.path], inputs = [ctx.file.src], outputs = [out])
    return [DefaultInfo(files = depset([out]))]
shell = rule(_shell, attrs = {"src": attr.label(allow_single_file = True)})
"#,
        );
        ws.file("pkg/in.txt", "hello\n");
        write_executable_script(
            &ws.0.join("ws/pkg/tool.sh"),
            "#!/bin/bash\ncp \"$1\" \"$2\"\n",
            0o755,
        );
        ws
    }

    fn file(&self, relative: &str, text: &str) {
        let path = self.0.join("ws").join(relative);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, text).unwrap();
    }

    /// A helper script logging `ARGV|CWD|STDIN` lines, answering `response`.
    fn helper(&self, name: &str, response: &str) -> PathBuf {
        let path = self.0.join(name);
        write_executable_script(
            &path,
            format!(
                "#!/bin/bash\nprintf '%s|%s|%s|%s\\n' \"$*\" \"$PWD\" \"$(cat)\" \"$N41_MARK\" >> {}\n{response}\n",
                self.0.join(format!("{name}.log")).display()
            ),
            0o755,
        );
        path
    }

    fn helper_log(&self, name: &str) -> Vec<String> {
        fs::read_to_string(self.0.join(format!("{name}.log")))
            .unwrap_or_default()
            .lines()
            .map(str::to_owned)
            .collect()
    }

    /// A run that opts in to remote cache uploads (BAZEL_RS_ALLOW_REMOTE_UPLOAD).
    fn run(&self, cache: &str, args: &[&str]) -> Output {
        self.run_with(cache, args, true)
    }

    fn run_with(&self, cache: &str, args: &[&str], upload_opt_in: bool) -> Output {
        let mut command = Command::new(env!("CARGO_BIN_EXE_bazel-rs"));
        command
            .arg("--ignore_all_rc_files")
            .arg(format!("--cache-dir={}", self.0.join(cache).display()))
            .args(args)
            .current_dir(self.0.join("ws"))
            .env_clear()
            .envs(verifier_environment())
            .env("PATH", "/usr/bin:/bin")
            .env("HOME", self.0.join("home"))
            .env("N41_MARK", "client-env");
        if upload_opt_in {
            command.env("BAZEL_RS_ALLOW_REMOTE_UPLOAD", "1");
        }
        command.explicit_modules().output().unwrap()
    }

    fn ws(&self) -> PathBuf {
        self.0.join("ws")
    }
}

impl Drop for Workspace {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn stderr(output: &Output) -> String {
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
    // Startup notice, not a diagnostic (crate::memory_limit).
    let stderr = stderr
        .lines()
        .filter(|line| !line.starts_with("bazel-rs: memory limit defaulted to "))
        .collect::<Vec<_>>()
        .join("\n");
    if stderr.is_empty() {
        stderr
    } else {
        stderr + "\n"
    }
}

fn outputs(output: &Output) -> BTreeMap<String, String> {
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .map(|line| {
            let (label, path) = line.split_once('\t').unwrap();
            (label.to_owned(), fs::read_to_string(path).unwrap())
        })
        .collect()
}

// Action keys genuine Bazel 9.3.0 wrote for this fixture (the N41 paired
// capture, tests/fixtures/remote_cache/reference_receipt.json).
const GEN_KEY: &str = "e99888bbd1421b4dc6fd361854c07ef76d5bb586d761766704e0063d578b4dd9";
const RUN_KEY: &str = "55a9a20019c12f38767dc9dc43875a8a327cd33249897231f72ffede7936e030";
const SH_KEY: &str = "215a0cea76512319330fd7d9ff08cee13aaca1f345089f37d42323c2d69be62e";

#[test]
fn action_keys_equal_genuine_bazel_and_entries_round_trip() {
    let ws = Workspace::new();
    let server = Server::new();
    let helper = ws.helper(
        "helper",
        "echo '{\"headers\":{\"Authorization\":[\"Bearer t\"],\"X-Multi\":[\"a\",\"b\"]}}'",
    );
    let cache = format!("--remote_cache={}", server.url());
    let scope = format!("--credential_helper=127.0.0.1={}", helper.display());
    let first = ws.run("c1", &["build", &cache, &scope, "//pkg:all"]);
    assert!(first.status.success(), "{}", stderr(&first));
    assert!(
        stderr(&first).contains(&format!(
            "WARNING: Credentials are transmitted in plaintext to {}. Please consider using an HTTPS endpoint.",
            server.url()
        )),
        "{}",
        stderr(&first)
    );
    assert!(stderr(&first).contains("3 actions executed, 0 cache hits, 0 remote cache hits"));
    let built = outputs(&first);
    assert_eq!(built["//pkg:gen"], "hello\ndone\n");
    assert_eq!(built["//pkg:run"], "hello\n");
    assert_eq!(built["//pkg:sh"], "HELLO\n");
    // The AC keys are genuine Bazel's.
    assert_eq!(server.keys("/ac/"), [SH_KEY, RUN_KEY, GEN_KEY]);
    let log = server.take_log();
    for request in &log {
        assert_eq!(request.header("authorization"), ["Bearer t"], "{request:?}");
        assert_eq!(request.header("x-multi"), ["a", "b"], "{request:?}");
    }
    let mut lines = log
        .iter()
        .map(|r| format!("{} {}", r.method, &r.path[..5]))
        .collect::<Vec<_>>();
    lines.sort();
    // Per action: AC GET (miss), Action + Command + output CAS PUTs (empty
    // stdout/stderr are not uploaded), AC PUT.
    assert_eq!(
        lines.iter().filter(|l| l.starts_with("GET /ac/")).count(),
        3
    );
    assert_eq!(
        lines.iter().filter(|l| l.starts_with("PUT /ac/")).count(),
        3
    );
    assert_eq!(
        lines.iter().filter(|l| l.starts_with("PUT /cas")).count(),
        9
    );
    // One helper invocation for the cache URI (startup), then cached.
    assert_eq!(
        ws.helper_log("helper"),
        [format!(
            "get|{}|{{\"uri\":\"{}\"}}|client-env",
            ws.ws().display(),
            server.url()
        )]
    );
    // A fresh native cache: every action is a remote cache hit.
    let second = ws.run("c2", &["build", &cache, &scope, "//pkg:all"]);
    assert!(second.status.success(), "{}", stderr(&second));
    assert!(stderr(&second).contains("0 actions executed, 0 cache hits, 3 remote cache hits"));
    assert_eq!(outputs(&second), built);
    let summary = server.summary();
    assert_eq!(
        summary.iter().filter(|l| l.starts_with("GET /ac/")).count(),
        3
    );
    assert_eq!(
        summary
            .iter()
            .filter(|l| l.starts_with("GET /cas/"))
            .count(),
        3
    );
    assert!(!summary.iter().any(|l| l.starts_with("PUT")));
    // The native action cache hits first; the remote cache is not consulted.
    let third = ws.run("c2", &["build", &cache, &scope, "//pkg:all"]);
    assert!(stderr(&third).contains("0 actions executed, 3 cache hits, 0 remote cache hits"));
    assert!(server.summary().is_empty());
}

#[test]
fn upload_and_accept_flags_and_execution_requirement_tags() {
    let ws = Workspace::new();
    let server = Server::new();
    let cache = format!("--remote_cache={}", server.url());
    let built = ws.run(
        "c1",
        &[
            "build",
            &cache,
            "--noremote_upload_local_results",
            "//pkg:gen",
        ],
    );
    assert!(built.status.success(), "{}", stderr(&built));
    assert_eq!(server.summary(), [format!("GET /ac/{GEN_KEY}")]);
    let built = ws.run(
        "c2",
        &["build", &cache, "--remote_accept_cached=false", "//pkg:gen"],
    );
    assert!(built.status.success(), "{}", stderr(&built));
    let summary = server.summary();
    assert!(!summary.iter().any(|l| l.starts_with("GET")), "{summary:?}");
    assert!(
        summary.contains(&format!("PUT /ac/{GEN_KEY}")),
        "{summary:?}"
    );
    // Both off: RemoteSpawnCache's NO_RESULT_NO_STORE.
    let built = ws.run(
        "c3",
        &[
            "build",
            &cache,
            "--noremote_accept_cached",
            "--noremote_upload_local_results",
            "//pkg:gen",
        ],
    );
    assert!(built.status.success(), "{}", stderr(&built));
    assert!(server.summary().is_empty());
    // no-remote-cache: not cached remotely; no-remote-exec: the CacheSalt
    // changes the key.
    ws.file(
        "tags/BUILD.bazel",
        "genrule(name = \"nc\", outs = [\"nc.txt\"], cmd = \"echo nc > $@\", tags = [\"no-remote-cache\"])\ngenrule(name = \"ne\", outs = [\"ne.txt\"], cmd = \"echo ne > $@\", tags = [\"no-remote-exec\"])\ngenrule(name = \"lo\", outs = [\"lo.txt\"], cmd = \"echo lo > $@\", local = True)\n",
    );
    let built = ws.run("c4", &["build", &cache, "//tags:all"]);
    assert!(built.status.success(), "{}", stderr(&built));
    let summary = server.summary();
    assert_eq!(
        summary.iter().filter(|l| l.starts_with("GET /ac/")).count(),
        1,
        "{summary:?}"
    );
}

#[test]
fn cache_failures_warn_and_build_locally() {
    let ws = Workspace::new();
    let server = Server::new();
    server.fail.store(true, Ordering::SeqCst);
    let built = ws.run(
        "c1",
        &[
            "build",
            &format!("--remote_cache={}", server.url()),
            "//pkg:gen",
        ],
    );
    assert!(built.status.success(), "{}", stderr(&built));
    let text = stderr(&built);
    assert!(
        text.contains("WARNING: Remote Cache: 500 Internal Server Error\nbroken"),
        "{text}"
    );
    assert_eq!(outputs(&built)["//pkg:gen"], "hello\ndone\n");
    // A corrupt CAS blob is a verification warning, then local execution.
    server.fail.store(false, Ordering::SeqCst);
    let ok = ws.run(
        "c2",
        &[
            "build",
            &format!("--remote_cache={}", server.url()),
            "//pkg:gen",
        ],
    );
    assert!(ok.status.success(), "{}", stderr(&ok));
    let blob = "0dcdcdbdf187330fa0484a3394c802582c28f1bb33aee24db776129c8d00dddc";
    server.serve(&format!("/cas/{blob}"), b"tampered!!!");
    let again = ws.run(
        "c3",
        &[
            "build",
            &format!("--remote_cache={}", server.url()),
            "//pkg:gen",
        ],
    );
    assert!(again.status.success(), "{}", stderr(&again));
    assert!(
        stderr(&again).contains(&format!("WARNING: Remote Cache: Output bazel-out/k8-fastbuild/bin/pkg/gen.txt download failed: Expected digest '{blob}/11' does not match received digest")),
        "{}",
        stderr(&again)
    );
    assert_eq!(outputs(&again)["//pkg:gen"], "hello\ndone\n");
}

#[test]
fn credential_helper_errors_follow_bazel() {
    let ws = Workspace::new();
    let server = Server::new();
    let cache = format!("--remote_cache={}", server.url());
    for (option, error) in [
        (
            "--credential_helper=*=x",
            "While parsing option --credential_helper=*=x: Credential helper scope '*' must be a valid domain name with an optional leading '*.' wildcard",
        ),
        (
            "--credential_helper==x",
            "While parsing option --credential_helper==x: Credential helper scope must not be empty",
        ),
        (
            "--credential_helper_timeout=5",
            "While parsing option --credential_helper_timeout=5: Illegal duration '5'.",
        ),
    ] {
        let failed = ws.run("c", &["query", option, "//pkg:gen"]);
        assert!(!failed.status.success());
        assert!(
            stderr(&failed).contains(error),
            "{option}: {}",
            stderr(&failed)
        );
    }
    let failed = ws.run(
        "c",
        &[
            "query",
            "--credential_helper=127.0.0.1=bin/helper",
            "//pkg:gen",
        ],
    );
    assert_eq!(
        stderr(&failed),
        "ERROR: Path 'bin/helper' must either be absolute or not contain any path separators\nbazel-rs: Error initializing credential helper\n"
    );
    let failed = ws.run(
        "c",
        &["build", &cache, "--credential_helper=nohelper", "//pkg:gen"],
    );
    assert_eq!(
        stderr(&failed),
        "ERROR: Could not find file with name 'nohelper' on PATH '/usr/bin:/bin'\nERROR: Could not find file with name 'nohelper' on PATH '/usr/bin:/bin'\nbazel-rs: Error initializing RemoteModule\n"
    );
    let failing = ws.helper("failing", "echo oops >&2; exit 3");
    let failed = ws.run(
        "c",
        &[
            "build",
            &cache,
            &format!("--credential_helper=127.0.0.1={}", failing.display()),
            "//pkg:gen",
        ],
    );
    assert_eq!(
        stderr(&failed),
        format!(
            "ERROR: Failed to get credentials for '{}' from helper '{}': process exited with code 3. stderr: oops\n\nbazel-rs: Error initializing RemoteModule\n",
            server.url(),
            failing.display()
        )
    );
    assert!(server.summary().is_empty());
    // A helper that matches nothing is never run; `*.` covers the domain.
    let other = ws.helper("other", "exit 1");
    let built = ws.run(
        "c",
        &[
            "build",
            &cache,
            &format!("--credential_helper=*.example.com={}", other.display()),
            "//pkg:gen",
        ],
    );
    assert!(built.status.success(), "{}", stderr(&built));
    assert!(ws.helper_log("other").is_empty());
    assert!(!stderr(&built).contains("plaintext"));
}

#[test]
fn credential_cache_duration_and_mid_build_helper_failure() {
    let ws = Workspace::new();
    let server = Server::new();
    // Succeeds once, then fails: with --credential_helper_cache_duration=0
    // every request asks the helper again, and a lookup failure fails the
    // action (RemoteSpawnCache.java:185-190).
    let counter = ws.0.join("count");
    let helper = ws.helper(
        "flaky",
        &format!(
            "if [ -e {0} ]; then echo late >&2; exit 4; fi; touch {0}; echo '{{\"headers\":{{\"A\":[\"1\"]}}}}'",
            counter.display()
        ),
    );
    let failed = ws.run(
        "c",
        &[
            "build",
            &format!("--remote_cache={}", server.url()),
            &format!("--credential_helper={}", helper.display()),
            "--credential_helper_cache_duration=0",
            "//pkg:gen",
        ],
    );
    assert!(!failed.status.success());
    assert!(
        stderr(&failed).contains(&format!(
            "Failed to get credentials for '{}' from helper '{}': process exited with code 4. stderr: late",
            server.url(),
            helper.display()
        )),
        "{}",
        stderr(&failed)
    );
    assert_eq!(ws.helper_log("flaky").len(), 2);
}

#[test]
fn http_remote_execution_and_netrc_are_explicit() {
    // gRPC caches are tests/remote_grpc_cache.rs's.
    let ws = Workspace::new();
    // Remote execution is tests/remote_execution.rs's; an HTTP cache cannot
    // serve it (RemoteModule.setup).
    let failed = ws.run(
        "c",
        &[
            "build",
            "--remote_executor=grpc://127.0.0.1:1",
            "--remote_cache=http://127.0.0.1:1",
            "//pkg:gen",
        ],
    );
    assert!(!failed.status.success());
    assert!(
        stderr(&failed)
            .contains("Cannot combine gRPC based remote execution with HTTP-based caching"),
        "{}",
        stderr(&failed)
    );
    // .netrc fallback for the cache is explicit.
    fs::write(
        ws.0.join("home/.netrc"),
        "machine 127.0.0.1 login a password b\n",
    )
    .unwrap();
    let failed = ws.run(
        "c",
        &["build", "--remote_cache=http://127.0.0.1:9", "//pkg:gen"],
    );
    assert!(
        stderr(&failed).contains(".netrc credentials are not implemented"),
        "{}",
        stderr(&failed)
    );
}

#[test]
fn registry_downloads_ask_the_helper_per_url() {
    let ws = Workspace::new();
    let server = Server::new();
    server.serve(
        "/modules/dep/1.0/MODULE.bazel",
        b"module(name = \"dep\", version = \"1.0\")\n",
    );
    ws.file(
        "MODULE.bazel",
        "module(name = \"rc\")\nbazel_dep(name = \"dep\", version = \"1.0\")\n",
    );
    let helper = ws.helper(
        "registry",
        "echo '{\"headers\":{\"Authorization\":[\"Basic cmVn\"]}}'",
    );
    let output = ws.run(
        "c",
        &[
            "mod",
            "graph",
            "--output=json",
            "--lockfile_mode=off",
            &format!("--registry={}", server.url()),
            &format!("--credential_helper=127.0.0.1={}", helper.display()),
        ],
    );
    assert!(output.status.success(), "{}", stderr(&output));
    let log = server.take_log();
    let module = log
        .iter()
        .find(|r| r.path == "/modules/dep/1.0/MODULE.bazel")
        .unwrap();
    assert_eq!(module.header("authorization"), ["Basic cmVn"]);
    let invoked = ws.helper_log("registry");
    assert!(
        invoked.contains(&format!(
            "get|{}|{{\"uri\":\"{}/modules/dep/1.0/MODULE.bazel\"}}|client-env",
            ws.ws().display(),
            server.url()
        )),
        "{invoked:?}"
    );
    // Each URL is asked once (per-URI cache).
    let mut unique = invoked.clone();
    unique.sort();
    unique.dedup();
    assert_eq!(unique.len(), invoked.len());
    assert!(Path::new(&ws.0).exists());
}

#[test]
fn ctx_downloads_prefer_the_credential_helper_over_auth() {
    // CredentialHelperCredentials with StaticCredentials(auth) as fallback
    // (BazelRepositoryModule.java:463-472): a helper that applies to the URL
    // replaces repository_ctx/module_ctx `auth` entirely; headers= stay and a
    // helper header replaces a base header of the same name.
    let ws = Workspace::new();
    let server = Server::new();
    server.serve("/helped", b"helped\n");
    server.serve("/static", b"static\n");
    server.serve("/repo-default", b"repository\n");
    ws.file(
        "MODULE.bazel",
        "module(name = \"rc\")\next = use_extension(\"//:ext.bzl\", \"ext\")\nuse_repo(ext, \"dl\")\n",
    );
    ws.file(
        "ext.bzl",
        &format!(
            r#"def _repo(rctx):
    rctx.download("http://localhost:{port}/repo-default", "repo.txt", sha256 = "c522c0aef8fd4d1b056c3c00743aba37ab783d78fd2aaf762628ec4f6e888883")
    rctx.file("BUILD.bazel", "exports_files(['data.txt'])\n")
    rctx.file("data.txt", rctx.attr.text)
repo = repository_rule(_repo, attrs = {{"text": attr.string()}})
def _ext(mctx):
    auth = {{"type": "basic", "login": "user", "password": "pw"}}
    url = "{url}"
    mctx.download(url + "/helped", "a", sha256 = "52c6ec2a332dce3710447fc481cdf15d10d4a2e7bee8e8e3943e290f2d9e99d8", headers = {{"X-Base": "base", "X-Token": "base"}}, auth = {{url + "/helped": auth}})
    mctx.download("http://localhost:{port}/static", "b", sha256 = "652cabf0de6cd70f66f72b17d6409203b84909be9864261feb614943f2e6cc62", auth = {{"http://localhost:{port}/static": auth}})
    repo(name = "dl", text = mctx.read("a") + mctx.read("b"))
ext = module_extension(_ext)
"#,
            url = server.url(),
            port = server.port
        ),
    );
    ws.file("BUILD.bazel", "");
    let helper = ws.helper(
        "dl",
        "echo '{\"headers\":{\"Authorization\":[\"Bearer helper\"],\"X-Token\":[\"helper\"],\"uSeR-aGeNt\":[\"fixture-helper/1\"]}}'",
    );
    let output = ws.run(
        "c",
        &[
            "build",
            &format!("--credential_helper=127.0.0.1={}", helper.display()),
            "@dl//:data.txt",
        ],
    );
    assert!(output.status.success(), "{}", stderr(&output));
    let log = server.take_log();
    let helped = log.iter().find(|r| r.path == "/helped").unwrap();
    assert_eq!(helped.header("authorization"), ["Bearer helper"]);
    assert_eq!(helped.header("x-token"), ["helper"]);
    assert_eq!(helped.header("x-base"), ["base"]);
    assert_eq!(helped.header("user-agent"), ["fixture-helper/1"]);
    // No helper for `localhost`: the static auth applies.
    let fallback = log.iter().find(|r| r.path == "/static").unwrap();
    assert_eq!(fallback.header("authorization"), ["Basic dXNlcjpwdw=="]);
    assert_eq!(
        fallback.header("user-agent"),
        [concat!("bazel-rs/", env!("CARGO_PKG_VERSION"))]
    );
    let repository = log.iter().find(|r| r.path == "/repo-default").unwrap();
    assert_eq!(
        repository.header("user-agent"),
        [concat!("bazel-rs/", env!("CARGO_PKG_VERSION"))]
    );
    assert!(repository.header("authorization").is_empty());
}

#[test]
fn tool_runfiles_enter_the_remote_input_root() {
    // N23f stages a tool's runfiles tree into the spawn (SpawnInputExpander);
    // the REAPI input root carries the same files, so a changed runfile is
    // a different remote key and the same runfiles hit again (N41).
    let ws = Workspace::new();
    ws.file(
        "pkg/BUILD.bazel",
        "load(\":defs.bzl\", \"tool\", \"use\")\ntool(name = \"tool\", data = [\"d.txt\"])\nuse(name = \"use\", tool = \":tool\")\n",
    );
    ws.file(
        "pkg/defs.bzl",
        r##"def _tool(ctx):
    exe = ctx.actions.declare_file(ctx.label.name)
    ctx.actions.write(exe, "#!/bin/sh\ncat \"$0.runfiles/_main/pkg/d.txt\" > \"$1\"\n", is_executable = True)
    return [DefaultInfo(executable = exe, runfiles = ctx.runfiles(files = ctx.files.data))]
tool = rule(_tool, executable = True, attrs = {"data": attr.label_list(allow_files = True)})
def _use(ctx):
    out = ctx.actions.declare_file(ctx.label.name + ".txt")
    ctx.actions.run(executable = ctx.executable.tool, arguments = [out.path], outputs = [out], mnemonic = "UseTool")
    return [DefaultInfo(files = depset([out]))]
use = rule(_use, attrs = {"tool": attr.label(executable = True, cfg = "target")})
"##,
    );
    let server = Server::new();
    let cache = format!("--remote_cache={}", server.url());
    let build = |local: &str| {
        let output = ws.run(local, &["build", &cache, "//pkg:use"]);
        assert!(output.status.success(), "{}", stderr(&output));
        (outputs(&output)["//pkg:use"].clone(), stderr(&output))
    };
    ws.file("pkg/d.txt", "one\n");
    let (first, log) = build("c1");
    assert_eq!(first, "one\n");
    assert!(log.contains(", 0 remote cache hits"), "{log}");
    // Only the runfile changes: a fresh local cache must not reuse the
    // first remote result.
    ws.file("pkg/d.txt", "two\n");
    let (second, log) = build("c2");
    assert_eq!(second, "two\n");
    assert!(log.contains(", 0 remote cache hits"), "{log}");
    // The first runfiles again: the remote entry is found.
    ws.file("pkg/d.txt", "one\n");
    let (third, log) = build("c3");
    assert_eq!(third, "one\n");
    assert!(log.contains(", 1 remote cache hits"), "{log}");
}

// The AC key genuine Bazel 9.3.0 wrote for `//pkg:ls` of the workspace below
// (parity-ts2 capture /tmp/opencode/perf-research/sd-remote-capture.py:
// genuine `--batch --ignore_all_rc_files` against a loopback HTTP cache,
// docs/SOURCE_DIRECTORIES.md).
const SOURCE_DIRECTORY_KEY: &str =
    "8631b164a32dc01eeda8bd7006705f579d3ea19f17ae7ea1657cdbcdb1d0e252";

#[test]
fn source_directory_inputs_enter_the_remote_input_root() {
    // MerkleTreeComputer.explodeDirectory: a source directory's files
    // (links followed, no empty directories, every file executable) are the
    // input root, so the key equals genuine Bazel's; a changed file inside
    // is a new key, the old contents hit again.
    let ws = Workspace::new();
    ws.file(
        "pkg/BUILD.bazel",
        "load(\":defs.bzl\", \"lsdir\")\nlsdir(name = \"ls\", src = \"dir\")\n",
    );
    ws.file(
        "pkg/defs.bzl",
        r#"def _lsdir(ctx):
    out = ctx.actions.declare_file(ctx.label.name + ".out")
    ctx.actions.run_shell(command = "find -L $1 | LC_ALL=C sort > $2 && cat $1/a.txt $1/link $1/sub/b.txt >> $2", arguments = [ctx.file.src.path, out.path], inputs = [ctx.file.src], outputs = [out])
    return [DefaultInfo(files = depset([out]))]
lsdir = rule(_lsdir, attrs = {"src": attr.label(allow_single_file = True)})
"#,
    );
    ws.file("pkg/dir/a.txt", "a\n");
    ws.file("pkg/dir/sub/b.txt", "b\n");
    write_executable_script(&ws.ws().join("pkg/dir/x.sh"), "#!/bin/sh\n", 0o755);
    std::os::unix::fs::symlink("a.txt", ws.ws().join("pkg/dir/link")).unwrap();
    fs::create_dir_all(ws.ws().join("pkg/dir/empty")).unwrap();
    let server = Server::new();
    let cache = format!("--remote_cache={}", server.url());
    let first = ws.run("c1", &["build", &cache, "//pkg:ls"]);
    assert!(first.status.success(), "{}", stderr(&first));
    // Genuine's output (the staged directory as `find -L` lists it).
    assert_eq!(
        outputs(&first)["//pkg:ls"],
        "pkg/dir\npkg/dir/a.txt\npkg/dir/empty\npkg/dir/link\npkg/dir/sub\npkg/dir/sub/b.txt\npkg/dir/x.sh\na\na\nb\n"
    );
    assert_eq!(server.keys("/ac/"), [SOURCE_DIRECTORY_KEY]);
    ws.file("pkg/dir/sub/b.txt", "c\n");
    let changed = ws.run("c2", &["build", &cache, "//pkg:ls"]);
    assert!(changed.status.success(), "{}", stderr(&changed));
    assert!(stderr(&changed).contains(", 0 remote cache hits"));
    assert_eq!(server.keys("/ac/").len(), 2);
    ws.file("pkg/dir/sub/b.txt", "b\n");
    let again = ws.run("c3", &["build", &cache, "//pkg:ls"]);
    assert!(
        stderr(&again).contains(", 1 remote cache hits"),
        "{}",
        stderr(&again)
    );
    // A dangling link: not cached remotely (Bazel fails the spawn with
    // explodeDirectory's IOException), the spawn runs locally.
    std::os::unix::fs::symlink("missing", ws.ws().join("pkg/dir/dangling")).unwrap();
    let dangling = ws.run("c4", &["build", &cache, "//pkg:ls"]);
    assert!(dangling.status.success(), "{}", stderr(&dangling));
    assert!(
        stderr(&dangling).contains(
            "WARNING: Remote Cache: not used for //pkg:ls#0: The file type of 'pkg/dir/dangling' is not supported."
        ),
        "{}",
        stderr(&dangling)
    );
}

// Genuine Bazel 9.3.0's action keys of the spawn creating the declared
// symlink `pkg/s` (`ln -s ../does/not/exist`) and of the consumer reading
// `pkg/l` (UnresolvedSymlinkAction, `data.txt`) and `pkg/s` (input root
// SymlinkNodes): /tmp/opencode/perf-research/us-remote-capture.py, a
// loopback HTTP cache.
const SPAWN_LINK_KEY: &str = "4f6054762e1500d00fc1a03109a9f632e78c4a5e9a1a4f380f76e99d23e4a48f";
const READ_LINKS_KEY: &str = "7627e67ed2df6a133791b1e495fe56763fd38d2181123c6ca57c5863ab79e701";

#[test]
fn unresolved_symlinks_enter_the_remote_cache() {
    // Unresolved symlink inputs are SymlinkNodes of the input root
    // (MerkleTreeComputer), a spawn's symlink output an OutputSymlink
    // (UploadManifest.addFileSymbolicLink): the keys equal genuine Bazel's,
    // and a fresh cache directory gets both spawns from the remote cache,
    // the symlink restored with its text (as genuine's second output base).
    let ws = Workspace::new();
    ws.file(
        "pkg/BUILD.bazel",
        "load(\":defs.bzl\", \"link\", \"rd\", \"spawn_link\")\nlink(name = \"l\")\nspawn_link(name = \"s\")\nrd(name = \"rd\", deps = [\":l\", \":s\"])\n",
    );
    ws.file(
        "pkg/defs.bzl",
        r#"def _link(ctx):
    out = ctx.actions.declare_symlink(ctx.label.name)
    ctx.actions.symlink(output = out, target_path = "data.txt")
    return [DefaultInfo(files = depset([out]))]
link = rule(_link)
def _spawn_link(ctx):
    out = ctx.actions.declare_symlink(ctx.label.name)
    ctx.actions.run_shell(command = "ln -s ../does/not/exist $1", arguments = [out.path], outputs = [out])
    return [DefaultInfo(files = depset([out]))]
spawn_link = rule(_spawn_link)
def _rd(ctx):
    out = ctx.actions.declare_file(ctx.label.name + ".out")
    ins = ctx.files.deps
    ctx.actions.run_shell(command = "readlink $1 > $3 && readlink $2 >> $3", arguments = [ins[0].path, ins[1].path, out.path], inputs = ins, outputs = [out])
    return [DefaultInfo(files = depset([out]))]
rd = rule(_rd, attrs = {"deps": attr.label_list()})
"#,
    );
    let server = Server::new();
    let cache = format!("--remote_cache={}", server.url());
    let built = |name: &str| {
        let output = ws.run(name, &["build", &cache, "//pkg:rd", "//pkg:s"]);
        assert!(output.status.success(), "{}", stderr(&output));
        let paths: BTreeMap<String, PathBuf> = String::from_utf8_lossy(&output.stdout)
            .lines()
            .map(|line| {
                let (label, path) = line.split_once('\t').unwrap();
                (label.to_owned(), PathBuf::from(path))
            })
            .collect();
        assert_eq!(
            fs::read_to_string(&paths["//pkg:rd"]).unwrap(),
            "data.txt\n../does/not/exist\n"
        );
        assert_eq!(
            fs::read_link(&paths["//pkg:s"]).unwrap(),
            Path::new("../does/not/exist")
        );
        stderr(&output)
    };
    built("c1");
    let mut keys = server.keys("/ac/");
    keys.sort();
    assert_eq!(keys, [SPAWN_LINK_KEY, READ_LINKS_KEY]);
    let again = built("c2");
    assert!(again.contains(", 2 remote cache hits"), "{again}");
}

#[test]
fn strict_action_env_is_the_modelled_default() {
    let ws = Workspace::new();
    let built = ws.run(
        "c",
        &["build", "--incompatible_strict_action_env", "//pkg:gen"],
    );
    assert!(built.status.success(), "{}", stderr(&built));
    // N42 models the non-strict environment too (docs/ACTION_ENV.md).
    let loose = ws.run(
        "c",
        &["build", "--noincompatible_strict_action_env", "//pkg:gen"],
    );
    assert!(loose.status.success(), "{}", stderr(&loose));
    // A genrule sees Bazel's strict PATH and genrule-setup.sh's `set -u`.
    ws.file(
        "env/BUILD.bazel",
        "genrule(name = \"path\", outs = [\"path.txt\"], cmd = \"echo $$PATH > $@; echo $$- >> $@\")\ngenrule(name = \"unset\", outs = [\"unset.txt\"], cmd = \"echo $$NOT_SET > $@\")\n",
    );
    let built = ws.run("c", &["build", "//env:path"]);
    assert!(built.status.success(), "{}", stderr(&built));
    let text = &outputs(&built)["//env:path"];
    assert!(text.starts_with("/bin:/usr/bin:/usr/local/bin\n"), "{text}");
    assert!(text.lines().nth(1).unwrap().contains('u'), "{text}");
    let failed = ws.run("c", &["build", "//env:unset"]);
    assert!(!failed.status.success());
    assert!(
        stderr(&failed).contains("NOT_SET: unbound variable"),
        "{}",
        stderr(&failed)
    );
}

#[test]
fn uploads_follow_remote_upload_local_results_like_bazel() {
    let ws = Workspace::new();
    let server = Server::new();
    let cache = format!("--remote_cache={}", server.url());
    // Bazel's default --remote_upload_local_results uploads the local result.
    let uploaded = ws.run_with("c1", &["build", &cache, "//pkg:gen"], false);
    assert!(uploaded.status.success(), "{}", stderr(&uploaded));
    assert!(
        !stderr(&uploaded).contains("uploads are disabled"),
        "{}",
        stderr(&uploaded)
    );
    assert!(
        server.summary().iter().any(|line| line.starts_with("PUT")),
        "{:?}",
        server.summary()
    );
    server.take_log();
    // A fresh client reads it.
    let read = ws.run_with("c2", &["build", &cache, "//pkg:gen"], false);
    assert!(stderr(&read).contains("0 actions executed, 0 cache hits, 1 remote cache hits"));
    assert!(!server.summary().iter().any(|line| line.starts_with("PUT")));
    // --noremote_upload_local_results reads but never writes.
    server.take_log();
    let no_upload = ws.run_with(
        "c3",
        &[
            "build",
            &cache,
            "--noremote_upload_local_results",
            "--remote_accept_cached=false",
            "//pkg:gen",
        ],
        false,
    );
    assert!(no_upload.status.success(), "{}", stderr(&no_upload));
    assert!(
        !server.summary().iter().any(|line| line.starts_with("PUT")),
        "{:?}",
        server.summary()
    );
}

fn exported(text: &str) -> BTreeMap<String, String> {
    // `export -p`: declare -x NAME="VALUE"; bash's own PWD, OLDPWD and SHLVL
    // and the executor's TMPDIR are not part of the spawn environment.
    text.lines()
        .filter_map(|line| line.strip_prefix("declare -x "))
        .map(|rest| match rest.split_once('=') {
            Some((name, value)) => (name.to_owned(), value.trim_matches('"').to_owned()),
            None => (rest.to_owned(), String::new()),
        })
        .filter(|(name, _)| !matches!(name.as_str(), "PWD" | "OLDPWD" | "SHLVL" | "TMPDIR"))
        .collect()
}

#[test]
fn executed_environment_equals_the_keyed_environment() {
    let ws = Workspace::new();
    let server = Server::new();
    ws.file(
        "envs/defs.bzl",
        r#"def _run(ctx):
    out = ctx.actions.declare_file(ctx.label.name + ".txt")
    ctx.actions.run(executable = ctx.file.tool, arguments = [out.path], outputs = [out], env = {"FOO": "bar", "PATH": "/opt/x"})
    return [DefaultInfo(files = depset([out]))]
envrun = rule(_run, attrs = {"tool": attr.label(allow_single_file = True)})
def _shell(ctx):
    out = ctx.actions.declare_file(ctx.label.name + ".txt")
    ctx.actions.run_shell(command = "export -p > $1", arguments = [out.path], outputs = [out], env = {"X": "1"})
    return [DefaultInfo(files = depset([out]))]
envshell = rule(_shell)
def _bare(ctx):
    out = ctx.actions.declare_file(ctx.label.name + ".txt")
    ctx.actions.run_shell(command = "export -p > " + out.path, outputs = [out])
    return [DefaultInfo(files = depset([out]))]
envbare = rule(_bare)
"#,
    );
    write_executable_script(
        &ws.ws().join("envs/tool.sh"),
        "#!/bin/bash\nexport -p > \"$1\"\n",
        0o755,
    );
    ws.file(
        "envs/BUILD.bazel",
        "load(\":defs.bzl\", \"envbare\", \"envrun\", \"envshell\")\ngenrule(name = \"gen\", outs = [\"gen.txt\"], cmd = \"export -p > $@\")\nenvrun(name = \"run\", tool = \"tool.sh\")\nenvshell(name = \"shell\")\nenvbare(name = \"bare\")\n",
    );
    let built = ws.run(
        "c",
        &[
            "build",
            &format!("--remote_cache={}", server.url()),
            "//envs:all",
        ],
    );
    assert!(built.status.success(), "{}", stderr(&built));
    let state = server.state.lock().unwrap();
    let mut checked = 0;
    for (label, path) in String::from_utf8_lossy(&built.stdout)
        .lines()
        .map(|line| line.split_once('\t').unwrap())
    {
        let executed = exported(&fs::read_to_string(path).unwrap());
        let exec_path = path.split("/outputs/").nth(1).unwrap();
        let mut keyed = None;
        for blob in state.blobs.keys() {
            let Some(key) = blob.strip_prefix("/ac/") else {
                continue;
            };
            let digest = bazel_rs::reapi::Action::decode_command_digest(
                &state.blobs[&format!("/cas/{key}")],
            )
            .unwrap();
            let command =
                bazel_rs::reapi::Command::decode(&state.blobs[&format!("/cas/{}", digest.hash)])
                    .unwrap();
            if command.output_paths == [exec_path] {
                keyed = Some(command.environment);
            }
        }
        assert_eq!(Some(executed), keyed, "{label}");
        checked += 1;
    }
    assert_eq!(checked, 4);
}

#[test]
fn inline_contents_and_exec_properties_are_checked() {
    let ws = Workspace::new();
    let server = Server::new();
    let cache = format!("--remote_cache={}", server.url());
    let result = bazel_rs::reapi::ActionResult {
        output_files: vec![bazel_rs::reapi::OutputFile {
            path: "bazel-out/k8-fastbuild/bin/pkg/gen.txt".into(),
            digest: bazel_rs::reapi::Digest::of(b"hello\ndone\n"),
            is_executable: true,
            contents: b"tampered\n".to_vec(),
        }],
        ..Default::default()
    };
    server.serve(&format!("/ac/{GEN_KEY}"), &result.encode());
    let built = ws.run("c1", &["build", &cache, "//pkg:gen"]);
    assert!(built.status.success(), "{}", stderr(&built));
    assert!(
        stderr(&built).contains("WARNING: Remote Cache: Output bazel-out/k8-fastbuild/bin/pkg/gen.txt download failed: Expected digest"),
        "{}",
        stderr(&built)
    );
    assert_eq!(outputs(&built)["//pkg:gen"], "hello\ndone\n");
    server.take_log();
    ws.file(
        "props/BUILD.bazel",
        "genrule(name = \"p\", outs = [\"p.txt\"], cmd = \"echo p > $@\", exec_properties = {\"k\": \"v\"})\n",
    );
    // Execution properties enter the key as the Command's and the Action's
    // Platform (PlatformUtils.getPlatformProto, docs/REMOTE_EXECUTION.md).
    let built = ws.run("c2", &["build", &cache, "//props:p"]);
    assert!(built.status.success(), "{}", stderr(&built));
    assert!(!stderr(&built).contains("not used"), "{}", stderr(&built));
    let state = server.state.lock().unwrap();
    let platforms = state
        .blobs
        .keys()
        .filter_map(|blob| blob.strip_prefix("/ac/"))
        .filter_map(|key| {
            let digest = bazel_rs::reapi::Action::decode_command_digest(
                &state.blobs[&format!("/cas/{key}")],
            )
            .unwrap();
            let command =
                bazel_rs::reapi::Command::decode(&state.blobs[&format!("/cas/{}", digest.hash)])
                    .unwrap();
            (command.output_paths == ["bazel-out/k8-fastbuild/bin/props/p.txt"])
                .then_some(command.platform)
        })
        .collect::<Vec<_>>();
    assert_eq!(platforms, [vec![("k".to_owned(), "v".to_owned())]]);
}

/// The N41 x N42 invariant (crate::action::resolved_environment): the
/// executed environment equals the keyed `Command.environment_variables`
/// apart from TMPDIR, also with inherited `--action_env` names (resolved
/// from the client environment) and a declared TMPDIR, which Bazel keys but
/// the executor replaces last (PosixLocalEnvProvider).
#[test]
fn executed_environment_equals_the_resolved_environment_with_inherited_values() {
    let ws = Workspace::new();
    let server = Server::new();
    ws.file(
        "envs/defs.bzl",
        r#"def _shell(ctx):
    out = ctx.actions.declare_file(ctx.label.name + ".txt")
    ctx.actions.run_shell(command = "export -p > " + out.path, outputs = [out], use_default_shell_env = True, env = {"X": "1"})
    return [DefaultInfo(files = depset([out]))]
envshell = rule(_shell)
"#,
    );
    ws.file(
        "envs/BUILD.bazel",
        "load(\":defs.bzl\", \"envshell\")\ngenrule(name = \"gen\", outs = [\"gen.txt\"], cmd = \"export -p > $@\")\nenvshell(name = \"shell\")\n",
    );
    let built = ws.run(
        "c",
        &[
            "build",
            &format!("--remote_cache={}", server.url()),
            "--action_env=N41_MARK",
            "--action_env=HOME",
            "--action_env=NOT_IN_CLIENT",
            "--action_env=FIXED=1",
            "--action_env=TMPDIR=/explicit",
            "//envs:all",
        ],
    );
    assert!(built.status.success(), "{}", stderr(&built));
    let state = server.state.lock().unwrap();
    let mut checked = 0;
    for (label, path) in String::from_utf8_lossy(&built.stdout)
        .lines()
        .map(|line| line.split_once('\t').unwrap())
    {
        let text = fs::read_to_string(path).unwrap();
        let tmpdir = text
            .lines()
            .find_map(|line| line.strip_prefix("declare -x TMPDIR="))
            .map(|value| value.trim_matches('"').to_owned())
            .unwrap();
        assert!(tmpdir.starts_with('/') && tmpdir != "/explicit", "{tmpdir}");
        let executed = exported(&text);
        let exec_path = path.split("/outputs/").nth(1).unwrap();
        let mut keyed = None;
        for blob in state.blobs.keys() {
            let Some(key) = blob.strip_prefix("/ac/") else {
                continue;
            };
            let digest = bazel_rs::reapi::Action::decode_command_digest(
                &state.blobs[&format!("/cas/{key}")],
            )
            .unwrap();
            let command =
                bazel_rs::reapi::Command::decode(&state.blobs[&format!("/cas/{}", digest.hash)])
                    .unwrap();
            if command.output_paths == [exec_path] {
                keyed = Some(command.environment);
            }
        }
        let mut keyed = keyed.unwrap_or_else(|| panic!("no Command for {label}"));
        assert_eq!(
            keyed.remove("TMPDIR").as_deref(),
            Some("/explicit"),
            "{label}"
        );
        assert_eq!(
            keyed.get("N41_MARK").map(String::as_str),
            Some("client-env")
        );
        assert_eq!(keyed.get("FIXED").map(String::as_str), Some("1"));
        assert!(!keyed.contains_key("NOT_IN_CLIENT"), "{label}");
        assert_eq!(executed, keyed, "{label}");
        checked += 1;
    }
    assert_eq!(checked, 2);
}

/// Merge recipe (N41 x N23d/N36): parameter files are input-root files, so
/// a parameter-file spawn shares its entry across caches; tree artifact
/// outputs stay local (no output_directories); a remote hit is published
/// through the content-addressed store like an executed output (one inode
/// with its `cas/<sha256>-<mode>` blob).
#[test]
fn param_files_trees_and_hits_through_the_store() {
    let ws = Workspace::new();
    let server = Server::new();
    ws.file(
        "rc/defs.bzl",
        r#"def _params(ctx):
    out = ctx.actions.declare_file(ctx.label.name + ".txt")
    args = ctx.actions.args()
    args.use_param_file("@%s", use_always = True)
    args.add("--flag").add(out.path)
    ctx.actions.run_shell(command = "cat ${1#@} > $(tail -n1 ${1#@})", arguments = [args], outputs = [out])
    return [DefaultInfo(files = depset([out]))]
params = rule(_params)
def _tree(ctx):
    out = ctx.actions.declare_directory(ctx.label.name)
    ctx.actions.run_shell(command = "echo x > " + out.path + "/f", outputs = [out])
    return [DefaultInfo(files = depset([out]))]
tree = rule(_tree)
"#,
    );
    ws.file(
        "rc/BUILD.bazel",
        "load(\":defs.bzl\", \"params\", \"tree\")\nparams(name = \"p\")\ntree(name = \"t\")\n",
    );
    let cache = format!("--remote_cache={}", server.url());
    let tree = ws.run("w", &["build", &cache, "//rc:t"]);
    assert!(tree.status.success(), "{}", stderr(&tree));
    // A tree artifact output enters the cache (output_directories and its
    // Tree, tree_outputs_and_aliased_execution_platforms_key_like_genuine).
    assert!(!stderr(&tree).contains("Remote Cache"), "{}", stderr(&tree));
    assert_eq!(server.keys("/ac/").len(), 1);
    server.take_log();
    let written = ws.run("w", &["build", &cache, "//rc:p"]);
    assert!(written.status.success(), "{}", stderr(&written));
    let params = &outputs(&written)["//rc:p"];
    assert!(params.starts_with("--flag\n"), "{params}");
    // One more ActionResult: the parameter-file spawn (its input root holds
    // the parameter file at <output>-0.params, crate::action::remote_action).
    let entries = server
        .state
        .lock()
        .unwrap()
        .blobs
        .keys()
        .filter(|key| key.starts_with("/ac/"))
        .count();
    assert_eq!(entries, 2);
    // A fresh native cache reads the entry: published through the store.
    let read = ws.run_with("r", &["build", &cache, "//rc:p"], false);
    assert!(read.status.success(), "{}", stderr(&read));
    assert!(
        stderr(&read).contains("0 actions executed, 0 cache hits, 1 remote cache hits"),
        "{}",
        stderr(&read)
    );
    let line = String::from_utf8_lossy(&read.stdout).into_owned();
    let path = PathBuf::from(line.trim_end().split_once('\t').unwrap().1);
    assert_eq!(fs::read_to_string(&path).unwrap(), *params);
    use std::os::unix::fs::MetadataExt;
    let metadata = fs::metadata(&path).unwrap();
    assert_eq!(metadata.mode() & 0o7777, 0o555);
    assert!(metadata.nlink() >= 2, "{}", metadata.nlink());
    let cas = ws.0.join("r/cas");
    let blob = fs::read_dir(&cas)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .find(|blob| fs::metadata(blob).unwrap().ino() == metadata.ino());
    assert!(blob.is_some(), "no cas blob shares the output inode");
}

/// The genuine Bazel 9.3.1 capture of
/// tests/fixtures/remote_cache/tree_platform_receipt.json.
fn tree_platform_receipt() -> serde_json::Value {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/remote_cache/tree_platform_receipt.json");
    serde_json::from_slice(&fs::read(path).unwrap()).unwrap()
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

/// The expanded execution log's spawns: (mnemonic, digest JSON).
fn logged_digests(path: &Path) -> BTreeMap<String, serde_json::Value> {
    let text = fs::read_to_string(path).unwrap();
    serde_json::Deserializer::from_str(&text)
        .into_iter::<serde_json::Value>()
        .map(|spawn| {
            let spawn = spawn.unwrap();
            (
                spawn["mnemonic"].as_str().unwrap().to_owned(),
                spawn["digest"].clone(),
            )
        })
        .collect()
}

#[test]
fn tree_outputs_and_aliased_execution_platforms_key_like_genuine() {
    // On ~/code (2026-11) bazel-rs skipped the remote cache for every
    // spawn with a tree artifact output (rules_js package extraction,
    // copy_to_directory, the Go stdlib, OCI layouts) and for every spawn
    // whose execution platform is reached through an alias (Bazel 9's
    // @bazel_tools//tools:host_platform -> @platforms//host): 660 of 3337
    // remote hits where genuine had all 3337.
    let receipt = tree_platform_receipt();
    let ws = Workspace::new();
    for (path, text) in receipt["workspace"].as_object().unwrap() {
        ws.file(path, text.as_str().unwrap());
    }
    let server = Server::new();
    let cache = format!("--remote_cache={}", server.url());
    let log = ws.0.join("exec.json");
    let args = [
        "build",
        cache.as_str(),
        "--platforms=//pkg:tp",
        "--host_platform=//pkg:tp",
        "--extra_execution_platforms=//pkg:pa",
        "//pkg:t",
        "//pkg:aliased",
    ];
    let mut logged = args.to_vec();
    let log_flag = format!("--execution_log_json_file={}", log.display());
    logged.push(&log_flag);
    let first = ws.run("c1", &logged);
    assert!(first.status.success(), "{}", stderr(&first));
    assert!(
        !stderr(&first).contains("Remote Cache"),
        "{}",
        stderr(&first)
    );
    // The AC keys and the CAS set (the Tree blob included) are genuine's.
    let genuine_keys: Vec<String> = receipt["ac"].as_object().unwrap().keys().cloned().collect();
    assert_eq!(server.keys("/ac/"), genuine_keys);
    let genuine_cas: Vec<String> = receipt["cas"]
        .as_array()
        .unwrap()
        .iter()
        .map(|hash| hash.as_str().unwrap().to_owned())
        .collect();
    assert_eq!(server.keys("/cas/"), genuine_cas);
    let tree = &receipt["tree_blob"];
    let uploaded = server.state.lock().unwrap().blobs
        [&format!("/cas/{}", tree["hash"].as_str().unwrap())]
        .clone();
    assert_eq!(hex(&uploaded), tree["bytes"].as_str().unwrap());
    // SpawnExec.digest: the action key, as genuine logs it.
    let genuine_digests: BTreeMap<String, serde_json::Value> = receipt["execution_log_spawns"]
        .as_array()
        .unwrap()
        .iter()
        .map(|spawn| {
            (
                spawn["mnemonic"].as_str().unwrap().to_owned(),
                spawn["digest"].clone(),
            )
        })
        .collect();
    assert_eq!(logged_digests(&log), genuine_digests);
    // A fresh native cache over genuine's entries: both spawns hit, the
    // tree comes back with its symlinks (one leaves the tree for an input,
    // staged for the hit like genuine's execution root holds it).
    let genuine = Server::new();
    for (key, result) in receipt["ac"].as_object().unwrap() {
        let bytes = (0..result.as_str().unwrap().len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&result.as_str().unwrap()[i..i + 2], 16).unwrap())
            .collect::<Vec<_>>();
        genuine.serve(&format!("/ac/{key}"), &bytes);
    }
    for (path, bytes) in server.state.lock().unwrap().blobs.iter() {
        if path.starts_with("/cas/") {
            genuine.serve(path, bytes);
        }
    }
    let cache = format!("--remote_cache={}", genuine.url());
    let mut args = args.to_vec();
    args[1] = &cache;
    let second = ws.run_with("c2", &args, false);
    assert!(second.status.success(), "{}", stderr(&second));
    assert!(
        stderr(&second).contains("0 actions executed, 0 cache hits, 2 remote cache hits"),
        "{}",
        stderr(&second)
    );
    let line = String::from_utf8_lossy(&second.stdout)
        .lines()
        .find(|line| line.starts_with("//pkg:t\t"))
        .unwrap()
        .to_owned();
    let root = PathBuf::from(line.split_once('\t').unwrap().1);
    assert_eq!(fs::read_to_string(root.join("f")).unwrap(), "hello\n");
    assert_eq!(fs::read_to_string(root.join("sub/g")).unwrap(), "g\n");
    assert!(root.join("empty").is_dir());
    // Published like an executed tree: its symlinks resolved (the native
    // store holds regular files).
    assert_eq!(fs::read_to_string(root.join("link")).unwrap(), "hello\n");
    assert_eq!(fs::read_to_string(root.join("up")).unwrap(), "hello\n");
}

fn sha256_hex(bytes: &[u8]) -> String {
    bazel_rs::reapi::Digest::of(bytes).hash
}

const BWOB_BUILD: &str = r#"genrule(name = "a", outs = ["a.txt"], cmd = "echo alpha > $@")
genrule(name = "b", srcs = [":a"], outs = ["b.txt"], cmd = "cat $< > $@; echo beta >> $@")
genrule(name = "c", srcs = [":a"], outs = ["c.txt"], cmd = "cat $< > $@; echo gamma >> $@", tags = ["no-remote"])
"#;

/// `bazel-bin/<relative>` of the output tree of `cache`, if it exists.
fn bin_file(ws: &Workspace, cache: &str, relative: &str) -> Option<String> {
    let info = ws.run_with(cache, &["info", "bazel-bin"], false);
    let bin = String::from_utf8_lossy(&info.stdout).trim().to_owned();
    fs::read_to_string(PathBuf::from(bin).join(relative)).ok()
}

/// Build without the Bytes with an HTTP cache (docs/REMOTE_CACHE.md):
/// like Bazel (HttpCacheClient has no FindMissingBlobs) a hit is trusted
/// and its intermediate outputs stay remote; only the requested target's
/// output is downloaded, a local consumer fetches its input on demand, and
/// a blob found missing then reruns its producer in a retried command.
#[test]
fn http_hits_download_toplevel_outputs_only() {
    let ws = Workspace::new();
    ws.file("bw/BUILD.bazel", BWOB_BUILD);
    let server = Server::new();
    let cache = format!("--remote_cache={}", server.url());
    let populated = ws.run("p", &["build", &cache, "//bw:b", "//bw:c"]);
    assert!(populated.status.success(), "{}", stderr(&populated));
    let a = format!("GET /cas/{}", sha256_hex(b"alpha\n"));
    let b = format!("GET /cas/{}", sha256_hex(b"alpha\nbeta\n"));
    server.take_log();
    let bep = ws.0.join("bep.json");
    let hit = ws.run_with(
        "r1",
        &[
            "build",
            &cache,
            &format!("--build_event_json_file={}", bep.display()),
            "//bw:b",
        ],
        false,
    );
    assert!(hit.status.success(), "{}", stderr(&hit));
    assert!(
        stderr(&hit).contains("0 actions executed, 0 cache hits, 2 remote cache hits"),
        "{}",
        stderr(&hit)
    );
    let summary = server.summary();
    assert!(summary.contains(&b), "{summary:?}");
    assert!(!summary.contains(&a), "{summary:?}");
    assert_eq!(
        bin_file(&ws, "r1", "bw/b.txt").as_deref(),
        Some("alpha\nbeta\n")
    );
    assert_eq!(bin_file(&ws, "r1", "bw/a.txt"), None);
    let events = fs::read_to_string(&bep).unwrap();
    assert!(events.contains("file://"), "{events}");
    assert!(!events.contains("bytestream://"), "{events}");
    // The native action cache keeps a's metadata: a later local consumer
    // fetches a's blob before it runs.
    let local = ws.run_with("r1", &["build", &cache, "//bw:c"], false);
    assert!(local.status.success(), "{}", stderr(&local));
    assert!(server.summary().contains(&a));
    assert_eq!(
        bin_file(&ws, "r1", "bw/c.txt").as_deref(),
        Some("alpha\ngamma\n")
    );
    // An evicted blob: the consumer's fetch finds it missing, the command
    // is retried with the blob known missing, and the producer reruns.
    assert!(server.remove(&format!("/cas/{}", sha256_hex(b"alpha\n"))));
    let lost = ws.run_with("r2", &["build", &cache, "//bw:c"], false);
    assert!(lost.status.success(), "{}", stderr(&lost));
    let text = stderr(&lost);
    assert!(
        text.contains("Lost inputs no longer available remotely: bazel-out/"),
        "{text}"
    );
    assert!(
        text.contains("Found transient remote cache error, retrying the build..."),
        "{text}"
    );
    assert!(text.contains("2 actions executed"), "{text}");
    assert_eq!(
        bin_file(&ws, "r2", "bw/c.txt").as_deref(),
        Some("alpha\ngamma\n")
    );
    // --remote_download_outputs=all downloads every output of a hit.
    server.take_log();
    let all = ws.run_with(
        "r3",
        &["build", &cache, "--remote_download_outputs=all", "//bw:b"],
        false,
    );
    assert!(all.status.success(), "{}", stderr(&all));
    assert_eq!(bin_file(&ws, "r3", "bw/a.txt").as_deref(), Some("alpha\n"));
}

/// `--remote_retries` (RemoteRetrier with HTTP_RESULT_CLASSIFIER): 503s
/// and dropped connections are retried with backoff, reads and writes;
/// with no retries the first failure is a warning and a local execution.
#[test]
fn http_cache_requests_are_retried() {
    let ws = Workspace::new();
    let server = Server::new();
    let cache = format!("--remote_cache={}", server.url());
    // Uploads: the first PUTs answer 503 and are retried.
    server.faults(2, 0);
    let built = ws.run("p", &["build", &cache, "//pkg:gen"]);
    assert!(built.status.success(), "{}", stderr(&built));
    assert!(
        !stderr(&built).contains("Remote Cache"),
        "{}",
        stderr(&built)
    );
    assert_eq!(server.keys("/ac/"), [GEN_KEY]);
    // Reads: 503s, then a dropped connection, each retried.
    for (unavailable, dropped, cache_dir) in [(3, 0, "r1"), (0, 2, "r2")] {
        server.faults(unavailable, dropped);
        server.take_log();
        let hit = ws.run_with(cache_dir, &["build", &cache, "//pkg:gen"], false);
        assert!(hit.status.success(), "{}", stderr(&hit));
        assert!(!stderr(&hit).contains("Remote Cache"), "{}", stderr(&hit));
        assert!(
            stderr(&hit).contains("1 remote cache hits"),
            "{}",
            stderr(&hit)
        );
        let ac = server
            .summary()
            .iter()
            .filter(|line| line.starts_with("GET /ac/"))
            .count();
        assert_eq!(ac, 1 + unavailable + dropped);
    }
    // --remote_retries=0: one failure, a warning and local execution.
    server.faults(1, 0);
    let once = ws.run_with(
        "r3",
        &["build", &cache, "--remote_retries=0", "//pkg:gen"],
        false,
    );
    assert!(once.status.success(), "{}", stderr(&once));
    assert!(
        stderr(&once).contains("WARNING: Remote Cache: 503 Service Unavailable\nbusy"),
        "{}",
        stderr(&once)
    );
    assert_eq!(outputs(&once)["//pkg:gen"], "hello\ndone\n");
}

// --- Test actions through the remote spawn cache (genuine receipt:
// tests/fixtures/remote_test_cache/reference_receipt.json) ---

fn test_receipt() -> serde_json::Value {
    serde_json::from_slice(
        &fs::read(
            Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("tests/fixtures/remote_test_cache/reference_receipt.json"),
        )
        .unwrap(),
    )
    .unwrap()
}

/// The receipt's workspace (the built-in module profile with the offline
/// registry, like the genuine capture).
fn test_workspace(receipt: &serde_json::Value) -> Workspace {
    let ws = Workspace::new();
    for (path, text) in receipt["files"].as_object().unwrap() {
        ws.file(path, text.as_str().unwrap());
    }
    ws
}

fn test_args<'a>(registry: &'a str, cache: &'a str, extra: &[&'a str]) -> Vec<&'a str> {
    let mut args = vec![
        "test",
        "--experimental-builtin-modules",
        registry,
        "--lockfile_mode=off",
        cache,
    ];
    args.extend_from_slice(extra);
    args.push("//pkg:all");
    args
}

fn keys(receipt: &serde_json::Value, field: &str) -> BTreeMap<String, String> {
    receipt[field]
        .as_object()
        .unwrap()
        .iter()
        .map(|(name, key)| (name.clone(), key.as_str().unwrap().to_owned()))
        .collect()
}

/// StandaloneTestStrategy's spawns go through RemoteSpawnCache like any
/// spawn: with only `--remote_cache`, the test and XML spawns' keys are
/// genuine Bazel's, a failing test is not uploaded, `no-remote-cache` and
/// `external` tests skip the cache, and a fresh client's unchanged tests
/// are remote cache hits — `(cached) PASSED`, `remote cache hit` in the
/// processes line, nothing executed.
#[test]
fn test_spawns_use_the_remote_cache_with_genuine_keys() {
    let receipt = test_receipt();
    let ws = test_workspace(&receipt);
    let server = Server::new();
    let cache = format!("--remote_cache={}", server.url());
    let registry = format!(
        "--registry=file://{}/tests/fixtures/builtin_modules/registry",
        env!("CARGO_MANIFEST_DIR")
    );
    let first = ws.run_with("c1", &test_args(&registry, &cache, &[]), false);
    let text = String::from_utf8_lossy(&first.stdout).into_owned() + &stderr(&first);
    assert_eq!(first.status.code(), Some(3), "{text}");
    assert!(
        text.contains(receipt["first"]["executed"].as_str().unwrap()),
        "{text}"
    );
    // Genuine's uploads exactly: every passing test spawn and every XML
    // spawn but //pkg:nrc's.
    let mut uploaded: Vec<String> = receipt["uploaded_ac_keys"]
        .as_array()
        .unwrap()
        .iter()
        .map(|key| key.as_str().unwrap().to_owned())
        .collect();
    uploaded.sort();
    let mut keys_put = server.keys("/ac/");
    keys_put.sort();
    assert_eq!(keys_put, uploaded, "{text}");
    let tests = keys(&receipt, "test_spawn_keys");
    let xmls = keys(&receipt, "xml_spawn_keys");
    assert!(!keys_put.contains(&tests["//pkg:bad"]));
    assert!(keys_put.contains(&xmls["//pkg:bad"]));
    let log = server.summary();
    // Lookups: every spawn but //pkg:nrc's two and //pkg:ext's test spawn.
    assert_eq!(
        log.iter().filter(|l| l.starts_with("GET /ac/")).count(),
        10,
        "{log:?}"
    );
    assert!(log.contains(&format!("GET /ac/{}", tests["//pkg:bad"])));
    // A fresh native cache: genuine's second run.
    let second = ws.run_with("c2", &test_args(&registry, &cache, &[]), false);
    let stdout = String::from_utf8_lossy(&second.stdout).into_owned();
    let text = stdout.clone() + &stderr(&second);
    assert_eq!(second.status.code(), Some(3), "{text}");
    for line in receipt["second_fresh_output_root"]["summary"]
        .as_array()
        .unwrap()
    {
        let line = line.as_str().unwrap();
        let (label, status) = line.split_once(' ').unwrap();
        let status = status.trim_start();
        let status = status.split(" in ").next().unwrap();
        assert!(
            stdout
                .lines()
                .any(|l| l.starts_with(label) && l.contains(status)),
            "{line}: {text}"
        );
    }
    assert!(
        text.contains(
            receipt["second_fresh_output_root"]["executed"]
                .as_str()
                .unwrap()
        ),
        "{text}"
    );
    assert!(text.contains(" 9 remote cache hit, "), "{text}");
    let log = server.summary();
    assert!(!log.iter().any(|l| l.starts_with("PUT /ac/")), "{log:?}");
    assert_eq!(
        log.iter().filter(|l| l.starts_with("GET /ac/")).count(),
        10,
        "{log:?}"
    );
    // The remote hit's logs are the test's: stdout and stderr in test.log.
    let testlogs = ws.0.join("c2/tests");
    let ok_log = fs::read_dir(&testlogs)
        .unwrap()
        .flatten()
        .map(|entry| {
            entry
                .path()
                .join("bazel-out/k8-fastbuild/testlogs/pkg/ok/test.log")
        })
        .find(|path| path.is_file())
        .unwrap();
    let ok_log = fs::read_to_string(ok_log).unwrap();
    assert!(ok_log.contains("testing\nhello\n"), "{ok_log}");
    assert!(ok_log.contains("err\n"), "{ok_log}");
    // The native test cache holds the remote result: no lookup next time.
    let third = ws.run_with("c2", &test_args(&registry, &cache, &[]), false);
    let text = String::from_utf8_lossy(&third.stdout).into_owned() + &stderr(&third);
    assert!(text.contains("Executed 2 out of 6 tests"), "{text}");
    // --nocache_test_results: every test spawn carries no-cache; the XML
    // spawns still hit (genuine: `5 remote cache hit`).
    server.take_log();
    let uncached = ws.run_with(
        "c3",
        &test_args(&registry, &cache, &["--nocache_test_results"]),
        false,
    );
    let text = String::from_utf8_lossy(&uncached.stdout).into_owned() + &stderr(&uncached);
    assert!(text.contains("Executed 6 out of 6 tests"), "{text}");
    assert!(text.contains(" 5 remote cache hit, "), "{text}");
    assert!(
        !server.summary().iter().any(|l| l.starts_with("PUT /ac/")),
        "{text}"
    );
    // --noremote_accept_cached: nothing is read; --noremote_upload_local_results:
    // nothing is written.
    let unread = ws.run_with(
        "c4",
        &test_args(
            &registry,
            &cache,
            &[
                "--noremote_accept_cached",
                "--noremote_upload_local_results",
            ],
        ),
        false,
    );
    let text = String::from_utf8_lossy(&unread.stdout).into_owned() + &stderr(&unread);
    assert!(text.contains("Executed 6 out of 6 tests"), "{text}");
    assert!(!text.contains(" remote cache hit, "), "{text}");
    let log = server.summary();
    assert!(!log.iter().any(|l| l.contains(" /ac/")), "{log:?}");
}

/// Without the native result lines the `bazel-rs: N actions executed, …`
/// line (no genuine counterpart) is not printed; Bazel's `INFO: N
/// processes: …` line reports the spawns.
#[test]
fn native_action_summary_needs_the_result_lines() {
    let ws = Workspace::new();
    let server = Server::new();
    let cache = format!("--remote_cache={}", server.url());
    let mut command = Command::new(env!("CARGO_BIN_EXE_bazel-rs"));
    command
        .arg("--ignore_all_rc_files")
        .arg(format!("--cache-dir={}", ws.0.join("c").display()))
        .args(["build", &cache, "//pkg:gen"])
        .current_dir(ws.0.join("ws"))
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .env("HOME", ws.0.join("home"));
    let output = command.explicit_modules().output().unwrap();
    assert!(output.status.success(), "{}", stderr(&output));
    assert!(
        !stderr(&output).contains("actions executed"),
        "{}",
        stderr(&output)
    );
    assert!(
        stderr(&output).contains("INFO: 2 processes: 1 internal, 1 "),
        "{}",
        stderr(&output)
    );
}

/// The XML spawn's duration argument of a remotely cached test is the
/// cached result's own wall time (its ExecutedActionMetadata, like
/// Utils.createSpawnResult): a test longer than a second hits both spawns.
#[test]
fn xml_spawn_of_a_cached_slow_test_hits() {
    let receipt = test_receipt();
    let ws = test_workspace(&receipt);
    ws.file(
        "pkg/BUILD.bazel",
        "load(\":tests.bzl\", \"t_test\")\nt_test(name = \"slow\", data = [\"in.txt\"], args = [\"0\"], env = {\"SLEEP\": \"1\"})\n",
    );
    ws.file(
        "pkg/tests.bzl",
        &receipt["files"]["pkg/tests.bzl"]
            .as_str()
            .unwrap()
            .replace("echo testing;", "echo testing; sleep 1.3;"),
    );
    let server = Server::new();
    let cache = format!("--remote_cache={}", server.url());
    let registry = format!(
        "--registry=file://{}/tests/fixtures/builtin_modules/registry",
        env!("CARGO_MANIFEST_DIR")
    );
    let first = ws.run_with("c1", &test_args(&registry, &cache, &[]), false);
    assert!(first.status.success(), "{}", stderr(&first));
    assert_eq!(server.keys("/ac/").len(), 2);
    server.take_log();
    let second = ws.run_with("c2", &test_args(&registry, &cache, &[]), false);
    let text = String::from_utf8_lossy(&second.stdout).into_owned() + &stderr(&second);
    assert!(second.status.success(), "{text}");
    assert!(text.contains(" 2 remote cache hit, "), "{text}");
    assert!(text.contains("(cached) PASSED"), "{text}");
    let log = server.summary();
    assert!(!log.iter().any(|l| l.starts_with("PUT /ac/")), "{log:?}");
}
