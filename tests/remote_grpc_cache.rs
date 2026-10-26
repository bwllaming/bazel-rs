//! The REAPI v2 gRPC remote cache (docs/REMOTE_CACHE.md, crate::remote_grpc)
//! against the in-process loopback server of tests/support. Nothing contacts
//! a real cache or credential helper. The ignored paired check runs genuine
//! Bazel 9.3 against the same server (or a local bazel-remote).
#![allow(clippy::disallowed_methods)] // test harness: starts processes directly (clippy.toml)
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::atomic::{AtomicUsize, Ordering};

include!("support/executable_script.rs");
include!("support/verify_env.rs");
include!("support/explicit_modules.rs");
include!("support/grpc_cache_server.rs");

use grpc_cache_server::{Config, Server, Tls};

static NEXT: AtomicUsize = AtomicUsize::new(0);

const DEFS: &str = r#"def _runner(ctx):
    out = ctx.actions.declare_file(ctx.label.name + ".out")
    ctx.actions.run(executable = ctx.file.tool, arguments = [ctx.file.src.path, out.path], inputs = [ctx.file.src], outputs = [out], mnemonic = "RunTool")
    return [DefaultInfo(files = depset([out]))]
runner = rule(_runner, attrs = {"src": attr.label(allow_single_file = True), "tool": attr.label(allow_single_file = True)})
def _shell(ctx):
    out = ctx.actions.declare_file(ctx.label.name + ".out")
    ctx.actions.run_shell(command = "tr a-z A-Z < $1 > $2", arguments = [ctx.file.src.path, out.path], inputs = [ctx.file.src], outputs = [out])
    return [DefaultInfo(files = depset([out]))]
shell = rule(_shell, attrs = {"src": attr.label(allow_single_file = True)})
"#;

// The genuine Bazel 9.3.0 keys of this fixture's first three actions (the
// N41 HTTP capture, tests/remote_cache.rs): gRPC caches share them.
const GEN_KEY: &str = "e99888bbd1421b4dc6fd361854c07ef76d5bb586d761766704e0063d578b4dd9";
const RUN_KEY: &str = "55a9a20019c12f38767dc9dc43875a8a327cd33249897231f72ffede7936e030";
const SH_KEY: &str = "215a0cea76512319330fd7d9ff08cee13aaca1f345089f37d42323c2d69be62e";

struct Workspace(PathBuf);

impl Workspace {
    fn new() -> Self {
        let root = PathBuf::from("/tmp/opencode").join(format!(
            "native-grpc-cache-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join("ws/pkg")).unwrap();
        fs::create_dir_all(root.join("home")).unwrap();
        let ws = Self(root);
        ws.file("MODULE.bazel", "module(name = \"rc\")\n");
        ws.file(
            "pkg/BUILD.bazel",
            "load(\":defs.bzl\", \"runner\", \"shell\")\ngenrule(name = \"gen\", srcs = [\"in.txt\"], outs = [\"gen.txt\"], cmd = \"cat $< > $@; echo done >> $@\")\nrunner(name = \"run\", src = \"in.txt\", tool = \"tool.sh\")\nshell(name = \"sh\", src = \"in.txt\")\n",
        );
        ws.file("pkg/defs.bzl", DEFS);
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

    /// A helper script logging `ARGV|STDIN` lines, answering `response`.
    fn helper(&self, name: &str, response: &str) -> PathBuf {
        let path = self.0.join(name);
        write_executable_script(
            &path,
            format!(
                "#!/bin/bash\nprintf '%s|%s\\n' \"$*\" \"$(cat)\" >> {}\n{response}\n",
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
            .env("BAZEL_RS_ALLOW_REMOTE_UPLOAD", "1");
        command.explicit_modules().output().unwrap()
    }
}

impl Drop for Workspace {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr)
        .lines()
        .filter(|line| !line.starts_with("bazel-rs: memory limit defaulted to "))
        .map(|line| format!("{line}\n"))
        .collect()
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

fn counts(pairs: &[(&str, usize)]) -> BTreeMap<String, usize> {
    pairs
        .iter()
        .map(|(name, count)| ((*name).to_owned(), *count))
        .collect()
}

#[test]
fn grpc_cache_round_trip_with_genuine_keys_headers_and_helpers() {
    let ws = Workspace::new();
    let server = Server::new();
    let helper = ws.helper(
        "helper",
        "echo '{\"headers\":{\"Authorization\":[\"Bearer t\"],\"X-Multi\":[\"a\",\"b\"]}}'",
    );
    let cache = format!("--remote_cache={}", server.url());
    let scope = format!("--credential_helper=127.0.0.1={}", helper.display());
    let flags = [
        "--remote_instance_name=dev",
        "--remote_header=X-Remote=r",
        "--remote_cache_header=x-cache=c",
    ];
    let mut args = vec!["build", &cache, &scope];
    args.extend(flags);
    args.push("//pkg:all");
    let first = ws.run("c1", &args);
    assert!(first.status.success(), "{}", stderr(&first));
    // No plaintext warning for grpc:// (RemoteModule.createCredentials).
    assert!(!stderr(&first).contains("plaintext"), "{}", stderr(&first));
    assert!(stderr(&first).contains("3 actions executed, 0 cache hits, 0 remote cache hits"));
    let built = outputs(&first);
    assert_eq!(built["//pkg:gen"], "hello\ndone\n");
    // The AC keys are genuine Bazel's, under the instance name.
    assert_eq!(server.ac_keys("dev"), [SH_KEY, RUN_KEY, GEN_KEY]);
    let log = server.take_log();
    assert_eq!(
        server.method_counts(&log),
        counts(&[
            ("Capabilities/GetCapabilities", 1),
            ("ActionCache/GetActionResult", 3),
            ("ContentAddressableStorage/FindMissingBlobs", 3),
            // Action, Command and output per action (empty stdout/stderr
            // are never written).
            ("ByteStream/Write", 9),
            ("ActionCache/UpdateActionResult", 3),
        ])
    );
    for call in &log {
        assert_eq!(call.header("authorization"), ["Bearer t"], "{call:?}");
        assert_eq!(call.header("x-multi"), ["a", "b"], "{call:?}");
        assert_eq!(call.header("x-remote"), ["r"], "{call:?}");
        assert_eq!(call.header("x-cache"), ["c"], "{call:?}");
        assert_eq!(call.header("content-type"), ["application/grpc"]);
        assert_eq!(call.header("te"), ["trailers"]);
        assert_eq!(call.header("grpc-timeout"), ["60000m"]);
        assert_eq!(
            call.header("build.bazel.remote.execution.v2.requestmetadata-bin")
                .len(),
            1
        );
        if call.method.ends_with("/Write") {
            assert!(call.detail.starts_with("dev/uploads/"), "{call:?}");
        } else if !call.method.ends_with("/GetCapabilities") {
            assert!(call.detail.starts_with("instance=dev"), "{call:?}");
        }
    }
    // One helper invocation per service URI (grpc-java's
    // https://AUTHORITY/SERVICE), then cached.
    let mut helper_log = ws.helper_log("helper");
    helper_log.sort();
    let uri = |service: &str| {
        format!(
            "get|{{\"uri\":\"https://127.0.0.1:{}/{service}\"}}",
            server.port
        )
    };
    assert_eq!(
        helper_log,
        [
            uri("build.bazel.remote.execution.v2.ActionCache"),
            uri("build.bazel.remote.execution.v2.Capabilities"),
            uri("build.bazel.remote.execution.v2.ContentAddressableStorage"),
            uri("google.bytestream.ByteStream"),
        ]
    );
    // A fresh native cache: every action is a remote cache hit.
    let second = ws.run("c2", &args);
    assert!(second.status.success(), "{}", stderr(&second));
    assert!(stderr(&second).contains("0 actions executed, 0 cache hits, 3 remote cache hits"));
    assert_eq!(outputs(&second), built);
    let log = server.take_log();
    assert_eq!(
        server.method_counts(&log),
        counts(&[
            ("Capabilities/GetCapabilities", 1),
            ("ActionCache/GetActionResult", 3),
            ("ByteStream/Read", 3),
        ])
    );
    // Another instance name is another cache.
    let other = ws.run(
        "c3",
        &["build", &cache, "--remote_instance_name=other", "//pkg:gen"],
    );
    assert!(
        stderr(&other).contains("1 actions executed"),
        "{}",
        stderr(&other)
    );
    // The native action cache hits first; the remote cache is not consulted.
    let third = ws.run("c2", &args);
    assert!(stderr(&third).contains("0 actions executed, 3 cache hits, 0 remote cache hits"));
    let log = server.take_log();
    assert!(
        log.iter().all(|call| call.detail.contains("other")),
        "{log:?}"
    );
}

#[test]
fn large_blobs_chunk_and_resume_reads_and_writes() {
    let ws = Workspace::new();
    let server = Server::with_config(Config {
        fail_write_after: Some(1 << 20),
        read_chunk: 100_000,
        ..Config::default()
    });
    ws.file(
        "big/BUILD.bazel",
        "genrule(name = \"big\", outs = [\"big.bin\"], cmd = \"head -c 3000000 /dev/zero | tr '\\\\0' x > $@; echo tail >> $@\")\n",
    );
    let cache = format!("--remote_cache={}", server.url());
    let first = ws.run("c1", &["build", &cache, "//big:big"]);
    assert!(first.status.success(), "{}", stderr(&first));
    let log = server.take_log();
    let writes = log
        .iter()
        .filter(|call| call.method.ends_with("/Write"))
        .map(|call| call.detail.clone())
        .collect::<Vec<_>>();
    // The 3 MB output: one interrupted Write, QueryWriteStatus, and a Write
    // resumed at the committed offset (ByteStreamUploader).
    let big = writes
        .iter()
        .filter(|detail| detail.contains("/3000005 "))
        .collect::<Vec<_>>();
    assert_eq!(big.len(), 2, "{writes:?}");
    assert!(big[0].contains(" offset=0 "), "{big:?}");
    let resumed: u64 = big[1]
        .split(" offset=")
        .nth(1)
        .unwrap()
        .split(' ')
        .next()
        .unwrap()
        .parse()
        .unwrap();
    assert!((1 << 20..3_000_005).contains(&resumed), "{big:?}");
    assert!(
        log.iter()
            .any(|call| call.method.ends_with("/QueryWriteStatus")),
        "{log:?}"
    );
    assert_eq!(server.state.lock().unwrap().max_write_chunk, 16 * 1024);
    // A read interrupted after 1 MiB resumes at the received offset.
    server.configure(|config| config.fail_read_after = Some(1 << 20));
    let second = ws.run("c2", &["build", &cache, "//big:big"]);
    assert!(second.status.success(), "{}", stderr(&second));
    assert!(
        stderr(&second).contains("1 remote cache hits"),
        "{}",
        stderr(&second)
    );
    let reads = server
        .take_log()
        .into_iter()
        .filter(|call| call.method.ends_with("/Read"))
        .map(|call| call.detail)
        .collect::<Vec<_>>();
    assert_eq!(reads.len(), 2, "{reads:?}");
    assert!(reads[0].ends_with(" offset=0"), "{reads:?}");
    assert!(!reads[1].ends_with(" offset=0"), "{reads:?}");
    let built = outputs(&second);
    assert_eq!(built["//big:big"].len(), 3_000_005);
    assert!(built["//big:big"].ends_with("xxtail\n"));
    // Without QueryWriteStatus the upload restarts from the beginning.
    server.configure(|config| {
        config.query_write_status = false;
        config.fail_write_after = Some(1 << 20);
    });
    ws.file(
        "big/BUILD.bazel",
        "genrule(name = \"big\", outs = [\"big.bin\"], cmd = \"head -c 3000000 /dev/zero | tr '\\\\0' y > $@\")\n",
    );
    let third = ws.run("c3", &["build", &cache, "//big:big"]);
    assert!(third.status.success(), "{}", stderr(&third));
    let writes = server
        .take_log()
        .into_iter()
        .filter(|call| call.method.ends_with("/Write") && call.detail.contains("/3000000 "))
        .map(|call| call.detail)
        .collect::<Vec<_>>();
    assert_eq!(writes.len(), 2, "{writes:?}");
    assert!(
        writes.iter().all(|detail| detail.contains(" offset=0 ")),
        "{writes:?}"
    );
}

#[test]
fn capability_failures_fail_actions_and_warnings_print() {
    let ws = Workspace::new();
    // A closed port: grpc-java's connect failure is every action's error.
    let closed = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = closed.local_addr().unwrap().port();
    drop(closed);
    let failed = ws.run(
        "c1",
        &[
            "build",
            &format!("--remote_cache=grpc://127.0.0.1:{port}"),
            "--remote_retries=1",
            "//pkg:gen",
        ],
    );
    assert!(!failed.status.success());
    assert!(
        stderr(&failed).contains(&format!(
            "Failed to query remote execution capabilities: finishConnect(..) failed: Connection refused: /127.0.0.1:{port}"
        )),
        "{}",
        stderr(&failed)
    );
    // No SHA256, an unsupported API range.
    let server = Server::with_config(Config {
        digest_functions: vec![2],
        high_api: (1, 0),
        low_api: (1, 0),
        ..Config::default()
    });
    let failed = ws.run(
        "c2",
        &[
            "build",
            &format!("--remote_cache={}", server.url()),
            "//pkg:gen",
        ],
    );
    assert!(!failed.status.success());
    let text = stderr(&failed);
    assert!(
        text.contains("ERROR: The client supported API versions, 2.0 to 2.11, is not supported by the server, 1.0 to 1.0. Please switch to a different server or upgrade Bazel.\n"),
        "{text}"
    );
    assert!(
        text.contains("Failed to query remote execution capabilities: Cannot use hash function SHA256 with remote cache. Server supported functions are: [SHA1]"),
        "{text}"
    );
    // Updates disabled: a warning; REAPI 2.0: Command.output_files.
    let server = Server::with_config(Config {
        update_enabled: false,
        high_api: (2, 0),
        ..Config::default()
    });
    let built = ws.run(
        "c3",
        &[
            "build",
            &format!("--remote_cache={}", server.url()),
            "//pkg:gen",
        ],
    );
    assert!(built.status.success(), "{}", stderr(&built));
    assert!(
        stderr(&built).contains("WARNING: --remote_upload_local_results is set, but the remote cache does not support uploading action results or the current account is not authorized to write local results to the remote cache.\n"),
        "{}",
        stderr(&built)
    );
    let keys = server.ac_keys("");
    assert_eq!(keys.len(), 1);
    assert_ne!(keys[0], GEN_KEY);
}

#[test]
fn call_failures_warn_retry_and_build_locally() {
    let ws = Workspace::new();
    let server = Server::with_config(Config {
        fail_method: Some((
            "build.bazel.remote.execution.v2.ActionCache/GetActionResult".into(),
            14,
            "try later".into(),
        )),
        ..Config::default()
    });
    let built = ws.run(
        "c1",
        &[
            "build",
            &format!("--remote_cache={}", server.url()),
            "--remote_retries=2",
            "--remote_retry_max_delay=10ms",
            "--noremote_upload_local_results",
            "//pkg:gen",
        ],
    );
    assert!(built.status.success(), "{}", stderr(&built));
    assert!(
        stderr(&built).contains("WARNING: Remote Cache: UNAVAILABLE: try later\n"),
        "{}",
        stderr(&built)
    );
    assert_eq!(outputs(&built)["//pkg:gen"], "hello\ndone\n");
    let log = server.take_log();
    // Two retries after the first attempt (RemoteRetrier).
    assert_eq!(
        server.method_counts(&log),
        counts(&[
            ("Capabilities/GetCapabilities", 1),
            ("ActionCache/GetActionResult", 3),
        ])
    );
    // Non-transient codes are not retried.
    server.configure(|config| {
        config.fail_method = Some((
            "build.bazel.remote.execution.v2.ActionCache/GetActionResult".into(),
            7,
            "denied".into(),
        ))
    });
    let built = ws.run(
        "c2",
        &[
            "build",
            &format!("--remote_cache={}", server.url()),
            "--noremote_upload_local_results",
            "//pkg:gen",
        ],
    );
    assert!(stderr(&built).contains("WARNING: Remote Cache: PERMISSION_DENIED: denied\n"));
    let log = server.take_log();
    assert_eq!(server.method_counts(&log)["ActionCache/GetActionResult"], 1);
}

#[test]
fn credential_helper_failures_fail_the_capability_check() {
    let ws = Workspace::new();
    let server = Server::new();
    let helper = ws.helper("bad", "echo oops >&2; exit 3");
    let failed = ws.run(
        "c1",
        &[
            "build",
            &format!("--remote_cache={}", server.url()),
            &format!("--credential_helper=127.0.0.1={}", helper.display()),
            "//pkg:gen",
        ],
    );
    assert!(!failed.status.success());
    assert!(
        stderr(&failed).contains(&format!(
            "Failed to query remote execution capabilities: Failed to get credentials for 'https://127.0.0.1:{}/build.bazel.remote.execution.v2.Capabilities' from helper '{}': process exited with code 3. stderr: oops",
            server.port,
            helper.display()
        )),
        "{}",
        stderr(&failed)
    );
    assert_eq!(ws.helper_log("bad").len(), 1);
    // A helper path error is RemoteModule's for a gRPC cache too.
    let failed = ws.run(
        "c2",
        &[
            "build",
            &format!("--remote_cache={}", server.url()),
            "--credential_helper=nope-not-on-path",
            "//pkg:gen",
        ],
    );
    assert_eq!(
        stderr(&failed)
            .lines()
            .filter(|line| line.contains("Could not find file with name 'nope-not-on-path'"))
            .count(),
        2,
        "{}",
        stderr(&failed)
    );
    assert!(stderr(&failed).contains("Error initializing RemoteModule"));
}

fn openssl(dir: &Path, args: &[&str]) {
    let status = Command::new("openssl")
        .args(args)
        .current_dir(dir)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .unwrap();
    assert!(status.success(), "openssl {args:?}");
}

/// A CA, a server certificate for 127.0.0.1 and a client certificate.
fn certificates(dir: &Path) {
    let ec = [
        "-newkey",
        "ec",
        "-pkeyopt",
        "ec_paramgen_curve:prime256v1",
        "-nodes",
    ];
    let mut ca = vec!["req", "-x509"];
    ca.extend(ec);
    ca.extend([
        "-keyout",
        "ca.key",
        "-out",
        "ca.pem",
        "-days",
        "2",
        "-subj",
        "/CN=test-ca",
    ]);
    openssl(dir, &ca);
    for (name, usage) in [("server", "serverAuth"), ("client", "clientAuth")] {
        fs::write(
            dir.join(format!("{name}.ext")),
            format!(
                "basicConstraints=CA:FALSE\nextendedKeyUsage={usage}\nsubjectAltName=IP:127.0.0.1,DNS:localhost\n"
            ),
        )
        .unwrap();
        let mut request = vec!["req"];
        request.extend(ec);
        let (key, csr, pem, ext) = (
            format!("{name}.key"),
            format!("{name}.csr"),
            format!("{name}.pem"),
            format!("{name}.ext"),
        );
        request.extend(["-keyout", &key, "-out", &csr, "-subj", "/CN=localhost"]);
        openssl(dir, &request);
        openssl(
            dir,
            &[
                "x509",
                "-req",
                "-in",
                &csr,
                "-CA",
                "ca.pem",
                "-CAkey",
                "ca.key",
                "-CAcreateserial",
                "-out",
                &pem,
                "-days",
                "2",
                "-extfile",
                &ext,
            ],
        );
    }
}

#[test]
fn tls_mutual_tls_and_unix_sockets() {
    if Command::new("openssl").arg("version").output().is_err() {
        eprintln!("skipped: no openssl");
        return;
    }
    let ws = Workspace::new();
    let certs = ws.0.join("certs");
    fs::create_dir_all(&certs).unwrap();
    certificates(&certs);
    let server = Server::tls(
        Config::default(),
        Tls {
            certificate: certs.join("server.pem"),
            key: certs.join("server.key"),
            client_ca: Some(certs.join("ca.pem")),
        },
    );
    let cache = format!("--remote_cache=grpcs://127.0.0.1:{}", server.port);
    let ca = format!("--tls_certificate={}", certs.join("ca.pem").display());
    let built = ws.run(
        "c1",
        &[
            "build",
            &cache,
            &ca,
            &format!(
                "--tls_client_certificate={}",
                certs.join("client.pem").display()
            ),
            &format!("--tls_client_key={}", certs.join("client.key").display()),
            "//pkg:gen",
        ],
    );
    assert!(built.status.success(), "{}", stderr(&built));
    assert_eq!(server.ac_keys(""), [GEN_KEY]);
    // Without the client certificate the handshake fails: the capability
    // check fails the action.
    let failed = ws.run(
        "c2",
        &["build", &cache, &ca, "--remote_retries=0", "//pkg:gen"],
    );
    assert!(!failed.status.success());
    assert!(
        stderr(&failed).contains("Failed to query remote execution capabilities: "),
        "{}",
        stderr(&failed)
    );
    // Without the CA the server certificate is untrusted.
    let failed = ws.run("c3", &["build", &cache, "--remote_retries=0", "//pkg:gen"]);
    assert!(!failed.status.success());
    // unix:PATH is plaintext.
    let socket = ws.0.join("cache.sock");
    let unix = Server::unix(Config::default(), socket.clone());
    let built = ws.run(
        "c4",
        &[
            "build",
            &format!("--remote_cache=unix:{}", socket.display()),
            "//pkg:gen",
        ],
    );
    assert!(built.status.success(), "{}", stderr(&built));
    assert_eq!(unix.ac_keys(""), [GEN_KEY]);
}

#[test]
fn options_and_their_errors() {
    let ws = Workspace::new();
    let server = Server::new();
    let cache = format!("--remote_cache={}", server.url());
    for option in [
        "--remote_download_outputs=minimal",
        "--remote_download_toplevel",
        "--remote_download_all",
        "--remote_max_connections=4",
        "--remote_max_concurrency_per_connection=2",
        "--noremote_cache_compression",
        "--remote_timeout=30",
    ] {
        let built = ws.run("c1", &["build", &cache, option, "//pkg:gen"]);
        assert!(built.status.success(), "{option}: {}", stderr(&built));
    }
    for (option, error) in [
        (
            "--remote_download_outputs=some",
            "While parsing option --remote_download_outputs=some: Not a valid download remote outputs: 'some' (should be all, minimal or toplevel)",
        ),
        (
            "--remote_retries=x",
            "While parsing option --remote_retries=x: 'x' is not an int",
        ),
        (
            "--remote_cache_compression",
            "zstd remote cache compression is not implemented",
        ),
        (
            "--remote_executor=grpc://127.0.0.1:1/path",
            "--remote_executor=grpc://127.0.0.1:1/path: invalid gRPC target",
        ),
    ] {
        let failed = ws.run("c2", &["build", &cache, option, "//pkg:gen"]);
        assert!(!failed.status.success(), "{option}");
        assert!(
            stderr(&failed).contains(error),
            "{option}: {}",
            stderr(&failed)
        );
    }
}

// --- The paired check with genuine Bazel 9.3 (ignored; run explicitly) ---
//
// BAZEL_RS_GENUINE_BAZEL=<genuine 9.3.0 ELF> [BAZEL_RS_BAZEL_REMOTE=<bazel-remote>]
// [BAZEL_RS_GRPC_RECEIPT=<out.json>] cargo test --test remote_grpc_cache -- --ignored
//
// Genuine Bazel builds the 8-action fixture against a fresh cache, native
// reads every entry; then native writes a fresh cache and genuine reads every
// entry. The cache is the in-process server, or (with BAZEL_RS_BAZEL_REMOTE)
// a bazel-remote binary on loopback.

const PAIRED_BUILD: &str = r#"load(":defs.bzl", "runner", "shell")
genrule(name = "gen", srcs = ["in.txt"], outs = ["gen.txt"], cmd = "cat $< > $@; echo done >> $@")
runner(name = "run", src = "in.txt", tool = "tool.sh")
shell(name = "sh", src = "in.txt")
genrule(name = "chain", srcs = [":gen"], outs = ["chain.txt"], cmd = "tr a-z A-Z < $< > $@")
genrule(name = "env", outs = ["env.txt"], cmd = "echo $$PATH > $@; echo $$- >> $@")
genrule(name = "mk", outs = ["mk.sh"], cmd = "printf '#!/bin/bash\\necho made > $$1\\n' > $@", executable = True)
genrule(name = "use", outs = ["use.txt"], tools = [":mk"], cmd = "$(location :mk) $@")
genrule(name = "ne", outs = ["ne.txt"], cmd = "echo ne > $@", tags = ["no-remote-exec"])
genrule(name = "nc", outs = ["nc.txt"], cmd = "echo nc > $@", tags = ["no-remote-cache"])
genrule(name = "big", outs = ["big.bin"], cmd = "head -c 3000000 /dev/zero | tr '\\0' x > $@; echo tail >> $@")
"#;

/// A cache for one phase of the paired check.
enum PairedCache {
    Local(Server),
    BazelRemote(std::process::Child, u16, PathBuf),
}

impl PairedCache {
    fn start(root: &Path, phase: &str) -> Self {
        let Ok(binary) = std::env::var("BAZEL_RS_BAZEL_REMOTE") else {
            return PairedCache::Local(Server::new());
        };
        let port = {
            let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            listener.local_addr().unwrap().port()
        };
        let http = {
            let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            listener.local_addr().unwrap().port()
        };
        let dir = root.join(format!("bazel-remote-{phase}"));
        let child = Command::new(binary)
            .args(["--dir", dir.to_str().unwrap(), "--max_size", "1"])
            .arg(format!("--grpc_address=127.0.0.1:{port}"))
            .arg(format!("--http_address=127.0.0.1:{http}"))
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .unwrap();
        for _ in 0..100 {
            if std::net::TcpStream::connect(("127.0.0.1", port)).is_ok() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        PairedCache::BazelRemote(child, port, dir)
    }

    fn url(&self) -> String {
        match self {
            PairedCache::Local(server) => server.url(),
            PairedCache::BazelRemote(_, port, _) => format!("grpc://127.0.0.1:{port}"),
        }
    }

    fn ac_keys(&self) -> Vec<String> {
        match self {
            PairedCache::Local(server) => server.ac_keys(""),
            PairedCache::BazelRemote(_, _, dir) => {
                let mut keys = Vec::new();
                for prefix in fs::read_dir(dir.join("ac.v2")).unwrap().flatten() {
                    for entry in fs::read_dir(prefix.path()).unwrap().flatten() {
                        let name = entry.file_name().to_string_lossy().into_owned();
                        keys.push(name.split('-').next().unwrap().to_owned());
                    }
                }
                keys.sort();
                keys
            }
        }
    }

    /// The sorted metadata names of each method's calls (local server).
    fn header_names(&self) -> BTreeMap<String, Vec<String>> {
        let mut out = BTreeMap::new();
        if let PairedCache::Local(server) = self {
            for call in server.state.lock().unwrap().log.iter() {
                let mut names = call
                    .metadata
                    .iter()
                    .map(|(name, value)| match name.as_str() {
                        // Deadlines vary by the elapsed microseconds.
                        "grpc-timeout" => format!("{name}={}", &value[..2]),
                        "user-agent" | "te" | "content-type" | "grpc-accept-encoding" => {
                            format!("{name}={value}")
                        }
                        _ => name.clone(),
                    })
                    .collect::<Vec<_>>();
                names.sort();
                names.dedup();
                out.insert(call.method.clone(), names);
            }
        }
        out
    }

    fn log(&self) -> Vec<String> {
        match self {
            PairedCache::Local(server) => {
                let mut lines = server
                    .take_log()
                    .into_iter()
                    .map(|call| {
                        let short = call.method.rsplit('.').next().unwrap().to_owned();
                        // Upload ids are random.
                        let detail = match call.detail.find("uploads/") {
                            Some(at) => {
                                let rest = &call.detail[at + 8..];
                                format!("uploads/<uuid>{}", &rest[rest.find('/').unwrap()..])
                            }
                            None => call.detail,
                        };
                        format!("{short} {detail}")
                    })
                    .collect::<Vec<_>>();
                lines.sort();
                lines
            }
            PairedCache::BazelRemote(..) => Vec::new(),
        }
    }
}

impl Drop for PairedCache {
    fn drop(&mut self) {
        if let PairedCache::BazelRemote(child, ..) = self {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

const PAIRED_TARGETS: [&str; 10] = [
    "//pkg:gen",
    "//pkg:run",
    "//pkg:sh",
    "//pkg:chain",
    "//pkg:env",
    "//pkg:mk",
    "//pkg:use",
    "//pkg:ne",
    "//pkg:nc",
    "//pkg:big",
];

fn genuine(binary: &str, ws: &Workspace, our: &str, registry: &str, cache: &str) -> Output {
    Command::new(binary)
        .args([
            "--batch",
            "--ignore_all_rc_files",
            &format!("--output_user_root={}", ws.0.join(our).display()),
            "build",
            &format!("--registry={registry}"),
            "--lockfile_mode=off",
            &format!("--remote_cache={cache}"),
        ])
        .args(PAIRED_TARGETS)
        .current_dir(ws.0.join("ws"))
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .env("HOME", ws.0.join("home"))
        .output()
        .unwrap()
}

fn native(ws: &Workspace, cache_dir: &str, registry: &str, cache: &str) -> Output {
    Command::new(env!("CARGO_BIN_EXE_bazel-rs"))
        .arg("--ignore_all_rc_files")
        .arg(format!("--cache-dir={}", ws.0.join(cache_dir).display()))
        .args([
            "build",
            "--experimental-builtin-modules",
            &format!("--registry={registry}"),
            "--lockfile_mode=off",
            &format!("--remote_cache={cache}"),
        ])
        .args(PAIRED_TARGETS)
        .current_dir(ws.0.join("ws"))
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .env("HOME", ws.0.join("home"))
        .env("BAZEL_RS_ALLOW_REMOTE_UPLOAD", "1")
        .output()
        .unwrap()
}

fn json<T: serde::Serialize>(value: &T) -> serde_json::Value {
    serde_json::to_value(value).unwrap()
}

fn genuine_hits(output: &Output) -> usize {
    // `INFO: 12 processes: 8 remote cache hit, ...`
    let text = String::from_utf8_lossy(&output.stderr);
    text.lines()
        .find_map(|line| {
            let at = line.find(" remote cache hit")?;
            line[..at].rsplit(' ').next()?.parse().ok()
        })
        .unwrap_or(0)
}

fn native_hits(output: &Output) -> usize {
    let text = String::from_utf8_lossy(&output.stderr);
    text.lines()
        .find_map(|line| {
            let at = line.find(" remote cache hits")?;
            line[..at].rsplit(' ').next()?.parse().ok()
        })
        .unwrap_or(0)
}

fn bazel_bin(ws: &Workspace) -> BTreeMap<String, String> {
    let bin = ws.0.join("ws/bazel-bin/pkg");
    let mut out = BTreeMap::new();
    for name in [
        "gen.txt",
        "run.out",
        "sh.out",
        "chain.txt",
        "env.txt",
        "use.txt",
        "ne.txt",
        "nc.txt",
    ] {
        out.insert(
            name.to_owned(),
            fs::read_to_string(bin.join(name)).unwrap_or_default(),
        );
    }
    out
}

#[test]
#[ignore = "needs genuine Bazel 9.3 (BAZEL_RS_GENUINE_BAZEL)"]
fn genuine_bazel_shares_grpc_entries_both_ways() {
    let binary = std::env::var("BAZEL_RS_GENUINE_BAZEL").expect("BAZEL_RS_GENUINE_BAZEL");
    let registry = format!(
        "file://{}",
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/builtin_modules/registry")
            .display()
    );
    let ws = Workspace::new();
    ws.file("pkg/BUILD.bazel", PAIRED_BUILD);
    let mut receipt = serde_json::Map::new();
    // Genuine writes, native reads.
    let cache = PairedCache::start(&ws.0, "genuine-writes");
    let written = genuine(&binary, &ws, "our-1", &registry, &cache.url());
    assert!(
        written.status.success(),
        "{}",
        String::from_utf8_lossy(&written.stderr)
    );
    let genuine_outputs = bazel_bin(&ws);
    let genuine_keys = cache.ac_keys();
    let genuine_headers = cache.header_names();
    let genuine_log = cache.log();
    let read = native(&ws, "ncache-1", &registry, &cache.url());
    assert!(read.status.success(), "{}", stderr(&read));
    let native_read_hits = native_hits(&read);
    let native_read_log = cache.log();
    let native_read_outputs = outputs(&read)
        .into_iter()
        .map(|(label, text)| (label, text.len()))
        .collect::<BTreeMap<_, _>>();
    drop(cache);
    // Native writes, genuine reads.
    let cache = PairedCache::start(&ws.0, "native-writes");
    let written = native(&ws, "ncache-2", &registry, &cache.url());
    assert!(written.status.success(), "{}", stderr(&written));
    let native_keys = cache.ac_keys();
    let native_headers = cache.header_names();
    let native_log = cache.log();
    let _ = fs::remove_dir_all(ws.0.join("our-1"));
    let read_back = genuine(&binary, &ws, "our-2", &registry, &cache.url());
    assert!(
        read_back.status.success(),
        "{}",
        String::from_utf8_lossy(&read_back.stderr)
    );
    let genuine_read_hits = genuine_hits(&read_back);
    let genuine_read_log = cache.log();
    let genuine_read_outputs = bazel_bin(&ws);
    receipt.insert(
        "server".into(),
        json(&match std::env::var("BAZEL_RS_BAZEL_REMOTE") {
            Ok(path) => format!("bazel-remote {path}"),
            Err(_) => "in-process tests/support/grpc_cache_server.rs".into(),
        }),
    );
    receipt.insert("genuine_keys".into(), json(&genuine_keys));
    receipt.insert("genuine_header_names".into(), json(&genuine_headers));
    receipt.insert("native_header_names".into(), json(&native_headers));
    receipt.insert("native_keys".into(), json(&native_keys));
    receipt.insert("native_read_hits".into(), json(&native_read_hits));
    receipt.insert("genuine_read_hits".into(), json(&genuine_read_hits));
    receipt.insert("genuine_write_log".into(), json(&genuine_log));
    receipt.insert("native_write_log".into(), json(&native_log));
    receipt.insert("native_read_log".into(), json(&native_read_log));
    receipt.insert("genuine_read_log".into(), json(&genuine_read_log));
    receipt.insert(
        "native_read_output_sizes".into(),
        json(&native_read_outputs),
    );
    receipt.insert("genuine_outputs".into(), json(&genuine_outputs));
    if let Ok(path) = std::env::var("BAZEL_RS_GRPC_RECEIPT") {
        fs::write(
            path,
            serde_json::to_string_pretty(&serde_json::Value::Object(receipt)).unwrap() + "\n",
        )
        .unwrap();
    }
    // 10 cached spawns (nc is no-remote-cache): gen, run, sh, chain, env,
    // mk (target and exec configurations), use, ne, big.
    assert_eq!(genuine_keys.len(), 10, "{genuine_keys:?}");
    assert_eq!(native_keys, genuine_keys);
    assert_eq!(native_read_hits, 10, "{}", stderr(&read));
    assert_eq!(
        genuine_read_hits,
        10,
        "{}",
        String::from_utf8_lossy(&read_back.stderr)
    );
    assert_eq!(genuine_read_outputs, genuine_outputs);
}
