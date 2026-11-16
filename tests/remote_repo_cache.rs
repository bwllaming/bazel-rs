//! The remote repository contents cache (`--experimental_remote_repo_contents_cache`,
//! docs/REPO_CONTENTS_CACHE.md) against a local fake HTTP cache: a cold
//! cache takes eligible repositories from the remote cache without
//! fetching them and publishes exactly what a fetch publishes; real
//! fetches upload; corrupt or missing blobs fall back to a fetch; local,
//! non-reproducible and non-portable repositories never use it; the
//! policies and the startup option parse like genuine's.
#![allow(clippy::disallowed_methods)] // test harness: starts processes directly (clippy.toml)

use std::collections::BTreeMap;
use std::fs;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

include!("support/verify_env.rs");
include!("support/explicit_modules.rs");
include!("support/sealed_trees.rs");
include!("support/module_archives.rs");
include!("support/grpc_cache_server.rs");

static NEXT: AtomicUsize = AtomicUsize::new(0);

#[derive(Default)]
struct State {
    /// `/ac/<hash>`, `/cas/<hash>` and the fixture files `/files/<name>`.
    blobs: BTreeMap<String, Vec<u8>>,
    log: Vec<(String, String)>,
    /// Every PUT answers 403 (a read-only token).
    refuse_puts: bool,
}

/// A minimal HTTP/1.1 server: an HTTP cache (GET/PUT `/ac/H`, `/cas/H`)
/// that also serves the archive fixtures (GET `/files/NAME`), logging
/// every request.
struct Server {
    port: u16,
    state: Arc<Mutex<State>>,
}

fn handle(stream: TcpStream, shared: Arc<Mutex<State>>) {
    let mut writer = stream.try_clone().unwrap();
    let mut reader = BufReader::new(stream);
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
        let mut length = 0;
        loop {
            let mut header = String::new();
            if reader.read_line(&mut header).unwrap_or(0) == 0 {
                return;
            }
            let header = header.trim_end();
            if header.is_empty() {
                break;
            }
            if let Some((name, value)) = header.split_once(':')
                && name.trim().eq_ignore_ascii_case("content-length")
            {
                length = value.trim().parse().unwrap();
            }
        }
        let mut body = vec![0; length];
        if reader.read_exact(&mut body).is_err() {
            return;
        }
        let (status, payload) = {
            let mut state = shared.lock().unwrap();
            state.log.push((method.clone(), path.clone()));
            if method == "PUT" && state.refuse_puts {
                ("403 Forbidden", Vec::new())
            } else if method == "PUT" {
                state.blobs.insert(path, body);
                ("200 OK", Vec::new())
            } else {
                match state.blobs.get(&path) {
                    Some(bytes) => ("200 OK", bytes.clone()),
                    None => ("404 Not Found", Vec::new()),
                }
            }
        };
        let head = format!(
            "HTTP/1.1 {status}\r\nContent-Length: {}\r\n\r\n",
            payload.len()
        );
        if writer.write_all(head.as_bytes()).is_err() || writer.write_all(&payload).is_err() {
            return;
        }
    }
}

impl Server {
    fn new() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let state = Arc::new(Mutex::new(State::default()));
        let shared = state.clone();
        std::thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                let shared = shared.clone();
                std::thread::spawn(move || handle(stream, shared));
            }
        });
        Self { port, state }
    }

    fn url(&self) -> String {
        format!("http://127.0.0.1:{}", self.port)
    }

    fn serve(&self, name: &str, bytes: &[u8]) {
        self.state
            .lock()
            .unwrap()
            .blobs
            .insert(format!("/files/{name}"), bytes.to_vec());
    }

    /// The requests since the last call: `(method, path)`.
    fn take_log(&self) -> Vec<(String, String)> {
        std::mem::take(&mut self.state.lock().unwrap().log)
    }

    /// The fixture downloads among `log`.
    fn fetches(log: &[(String, String)]) -> usize {
        log.iter()
            .filter(|(_, path)| path.starts_with("/files/"))
            .count()
    }

    fn puts(log: &[(String, String)]) -> usize {
        log.iter().filter(|(method, _)| method == "PUT").count()
    }

    /// The CAS path and bytes of the pack chunk (a zstd frame) holding `bytes`.
    fn chunk_holding(&self, bytes: &[u8]) -> (String, Vec<u8>) {
        let state = self.state.lock().unwrap();
        state
            .blobs
            .iter()
            .filter(|(path, blob)| {
                path.starts_with("/cas/") && blob.starts_with(&[0x28, 0xb5, 0x2f, 0xfd])
            })
            .find(|(_, blob)| {
                let mut raw = Vec::new();
                ruzstd::decoding::StreamingDecoder::new(blob.as_slice())
                    .is_ok_and(|mut decoder| decoder.read_to_end(&mut raw).is_ok())
                    && raw.windows(bytes.len()).any(|window| window == bytes)
            })
            .map(|(path, blob)| (path.clone(), blob.clone()))
            .expect("a pack chunk")
    }

    /// Rewrites the final entry whose manifest lists `path` (an untrusted
    /// cache's answer): `change` edits the manifest JSON (and may add CAS
    /// blobs); the entry then names the new manifest.
    fn rewrite_entry(&self, path: &str, change: impl FnOnce(&mut serde_json::Value, &Server)) {
        use bazel_rs::reapi::{ActionResult, Digest};
        let (key, mut result, mut manifest) = {
            let state = self.state.lock().unwrap();
            state
                .blobs
                .iter()
                .filter(|(key, _)| key.starts_with("/ac/"))
                .find_map(|(key, bytes)| {
                    let result = ActionResult::decode(bytes).ok()?;
                    let file = result
                        .output_files
                        .iter()
                        .find(|file| file.path == "manifest.json")?;
                    let manifest: serde_json::Value = serde_json::from_slice(
                        state.blobs.get(&format!("/cas/{}", file.digest.hash))?,
                    )
                    .ok()?;
                    manifest["entries"]
                        .as_array()?
                        .iter()
                        .any(|entry| entry["path"] == path)
                        .then(|| (key.clone(), result, manifest))
                })
                .expect("a final entry")
        };
        change(&mut manifest, self);
        let bytes = serde_json::to_vec(&manifest).unwrap();
        let digest = Digest::of(&bytes);
        self.set_blob(&format!("/cas/{}", digest.hash), Some(bytes));
        for file in &mut result.output_files {
            if file.path == "manifest.json" {
                file.digest = digest.clone();
            }
        }
        self.set_blob(&key, Some(result.encode()));
    }

    fn set_blob(&self, path: &str, bytes: Option<Vec<u8>>) {
        let mut state = self.state.lock().unwrap();
        match bytes {
            Some(bytes) => state.blobs.insert(path.to_owned(), bytes),
            None => state.blobs.remove(path),
        };
    }
}

fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::Digest as _;
    format!("{:x}", sha2::Sha256::digest(bytes))
}

const DATA: &[u8] = b"data of the archive\n";
const BLOB: &[u8] = b"an http_file\n";

/// The archive fixture: a BUILD file, `lib/data.txt` and an in-tree link.
fn archive(root: &Path) -> Vec<u8> {
    let source = root.join("archive-src");
    fs::create_dir_all(source.join("lib")).unwrap();
    fs::write(source.join("lib/data.txt"), DATA).unwrap();
    fs::write(
        source.join("BUILD.bazel"),
        "filegroup(name = \"data\", srcs = [\"lib/data.txt\", \"link.txt\"], visibility = [\"//visibility:public\"])\n",
    )
    .unwrap();
    std::os::unix::fs::symlink("lib/data.txt", source.join("link.txt")).unwrap();
    let out = root.join("archive.tar.gz");
    let status = Command::new("tar")
        .args([
            "--sort=name",
            "--mtime=@0",
            "--owner=0",
            "--group=0",
            "-czf",
        ])
        .arg(&out)
        .arg("-C")
        .arg(&source)
        .arg(".")
        .status()
        .unwrap();
    assert!(status.success());
    fs::read(out).unwrap()
}

const RULES: &str = r#"def _watched(ctx):
    ctx.file("BUILD.bazel", "exports_files(['value.txt'])\n")
    ctx.file("value.txt", ctx.read(ctx.attr.src))
    return ctx.repo_metadata(reproducible = True)

watched = repository_rule(_watched, attrs = {"src": attr.label()})

def _plain(ctx):
    ctx.file("BUILD.bazel", "exports_files(['value.txt'])\n")
    ctx.file("value.txt", "plain\n")

plain = repository_rule(_plain)

def _local(ctx):
    ctx.file("BUILD.bazel", "exports_files(['value.txt'])\n")
    ctx.file("value.txt", "local\n")
    return ctx.repo_metadata(reproducible = True)

local_rule = repository_rule(_local, local = True)

def _abs(ctx):
    ctx.file("BUILD.bazel", "exports_files(['value.txt'])\n")
    ctx.file("value.txt", "abs\n")
    ctx.symlink("/bin/sh", "sh")
    return ctx.repo_metadata(reproducible = True)

abs_rule = repository_rule(_abs)

def _gen(mctx):
    watched(name = "watched", src = "@@//:input.txt")
    plain(name = "plain")
    local_rule(name = "local_repo")
    abs_rule(name = "abs_link")

gen = module_extension(_gen)
"#;

struct Fixture {
    root: PathBuf,
    server: Server,
}

impl Fixture {
    fn new() -> Self {
        let root = crate::test_temp::root().join(format!(
            "native-remote-repo-cache-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = sealed_trees::remove_tree(&root);
        fs::create_dir_all(root.join("home")).unwrap();
        let server = Server::new();
        let tarball = archive(&root);
        server.serve("archive.tar.gz", &tarball);
        server.serve("blob.txt", BLOB);
        Self { root, server }
    }

    /// A workspace `name` (its own directory: runs in other checkouts of the
    /// same sources) whose `input.txt` holds `input`.
    fn workspace(&self, name: &str, input: &str) -> PathBuf {
        let ws = self.root.join(name);
        fs::create_dir_all(&ws).unwrap();
        let tarball = fs::read(self.root.join("archive.tar.gz")).unwrap();
        let url = self.server.url();
        fs::write(
            ws.join("MODULE.bazel"),
            format!(
                r#"module(name = "rrc")
http_archive = use_repo_rule("@bazel_tools//tools/build_defs/repo:http.bzl", "http_archive")
http_file = use_repo_rule("@bazel_tools//tools/build_defs/repo:http.bzl", "http_file")
http_archive(name = "ext", urls = ["{url}/files/archive.tar.gz"], sha256 = "{}")
http_file(name = "blob", urls = ["{url}/files/blob.txt"], sha256 = "{}")
gen = use_extension("//:rules.bzl", "gen")
use_repo(gen, "watched", "plain", "local_repo", "abs_link")
"#,
                sha256_hex(&tarball),
                sha256_hex(BLOB)
            ),
        )
        .unwrap();
        fs::write(ws.join("rules.bzl"), RULES).unwrap();
        fs::write(ws.join("input.txt"), input).unwrap();
        fs::write(
            ws.join("BUILD.bazel"),
            r#"genrule(
    name = "use",
    srcs = ["@ext//:data", "@blob//file", "@watched//:value.txt", "@plain//:value.txt", "@local_repo//:value.txt", "@abs_link//:value.txt"],
    outs = ["out.txt"],
    cmd = "cat $(SRCS) > $@",
    tags = ["no-remote"],
)
"#,
        )
        .unwrap();
        ws
    }

    /// `bazel-rs [startup] build ARGS //:use` in `ws` with `--cache-dir=<cache>`.
    fn run(&self, ws: &Path, cache: &str, startup: &[&str], args: &[&str]) -> Output {
        let cache_dir = format!("--cache-dir={}", self.root.join(cache).display());
        let registry = builtin_registry_flag();
        let mut command = Command::new(env!("CARGO_BIN_EXE_bazel-rs"));
        command
            .arg("--ignore_all_rc_files")
            .arg(&cache_dir)
            .args(startup)
            .arg("build")
            .arg(&registry)
            .args(args)
            .arg("//:use")
            .current_dir(ws)
            .env_clear()
            .envs(verifier_environment())
            .env("PATH", "/usr/bin:/bin")
            .env("HOME", self.root.join("home"))
            .env("BAZEL_RS_REMOTE_REPO_CACHE_STATS", "1")
            .env("BAZEL_RS_DUMP_ACTIONS", "1");
        command.seeded_archives().output().unwrap()
    }

    fn remote(&self) -> String {
        format!("--remote_cache={}", self.server.url())
    }

    fn out(&self, cache: &str) -> String {
        let pattern = self.root.join(cache).join("worktrees");
        let mut found = None;
        for entry in fs::read_dir(pattern).unwrap() {
            let path = entry
                .unwrap()
                .path()
                .join("execroot/_main/bazel-out/k8-fastbuild/bin/out.txt");
            if path.exists() {
                found = Some(fs::read_to_string(path).unwrap());
            }
        }
        found.expect("out.txt")
    }

    /// The tree each repository's slot links (`<name>` → identity).
    fn trees(&self, cache: &str) -> BTreeMap<String, String> {
        let slots = self.root.join(cache).join("repositories/external");
        fs::read_dir(slots)
            .unwrap()
            .filter_map(|entry| {
                let entry = entry.unwrap();
                let target = fs::read_link(entry.path()).ok()?;
                Some((
                    entry.file_name().to_string_lossy().into_owned(),
                    target.file_name()?.to_string_lossy().into_owned(),
                ))
            })
            .collect()
    }

    /// The hidden lockfile of the (single) worktree of `cache`.
    fn hidden_lockfile(&self, cache: &str) -> Option<Vec<u8>> {
        let worktrees = self.root.join(cache).join("worktrees");
        fs::read_dir(worktrees)
            .ok()?
            .find_map(|entry| fs::read(entry.ok()?.path().join("MODULE.bazel.lock")).ok())
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = sealed_trees::remove_tree(&self.root);
    }
}

/// stderr without the statistics line, cache and workspace paths
/// normalized, timings dropped.
fn normalized(output: &Output, fixture: &Fixture, cache: &str, ws: &Path) -> String {
    let text = String::from_utf8_lossy(&output.stderr)
        .replace(&fixture.root.join(cache).display().to_string(), "<CACHE>")
        .replace(&ws.display().to_string(), "<WS>");
    text.lines()
        .filter(|line| {
            !line.starts_with("REMOTE-REPO-CACHE ")
                && !line.starts_with("bazel-rs: memory limit")
                && !line.starts_with("INFO: Elapsed time")
        })
        .map(|line| {
            // The worktree's directory name hashes the workspace path.
            match line.find("/worktrees/ws") {
                Some(at) => {
                    let end = line[at..].find("/execroot").map_or(line.len(), |e| at + e);
                    format!("{}/worktrees/WS{}", &line[..at], &line[end..])
                }
                None => line.to_owned(),
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn stats(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr)
        .lines()
        .find(|line| line.starts_with("REMOTE-REPO-CACHE "))
        .map(|line| {
            line.split_whitespace()
                .filter(|field| field.starts_with("hits=") || field.starts_with("uploads="))
                .collect::<Vec<_>>()
                .join(" ")
        })
        .unwrap_or_default()
}

fn assert_success(output: &Output) {
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

const FLAG: &str = "--experimental_remote_repo_contents_cache";

#[test]
fn a_cold_cache_takes_eligible_repositories_from_the_remote_cache() {
    let f = Fixture::new();
    let remote = f.remote();
    // A plain fetch (the remote cache only for actions): the reference.
    let ws_c = f.workspace("ws-c", "one\n");
    let plain = f.run(&ws_c, "c", &[], &[&remote]);
    assert_success(&plain);
    assert_eq!(Server::fetches(&f.server.take_log()), 2);
    // A fetch that uploads the reproducible, portable repositories: the
    // archive, the file and the rule reading a watched input — not the
    // non-reproducible, local or absolute-link ones.
    let ws_a = f.workspace("ws-a", "one\n");
    let uploaded = f.run(&ws_a, "a", &[FLAG], &[&remote]);
    assert_success(&uploaded);
    assert_eq!(stats(&uploaded), "hits=0 uploads=3");
    let log = f.server.take_log();
    assert_eq!(Server::fetches(&log), 2);
    assert!(Server::puts(&log) > 0);
    // Another checkout with a cold cache: nothing is fetched, the three
    // repositories come from the remote cache, the others run.
    let ws_b = f.workspace("ws-b", "one\n");
    let disk = f.root.join("disk");
    let disk_flag = format!("--disk_cache={}", disk.display());
    let hit = f.run(&ws_b, "b", &[FLAG], &[&remote, &disk_flag]);
    assert_success(&hit);
    assert_eq!(stats(&hit), "hits=3 uploads=0");
    let log = f.server.take_log();
    assert_eq!(Server::fetches(&log), 0, "{log:?}");
    assert_eq!(Server::puts(&log), 0, "{log:?}");
    // Exactly what a fetch publishes: trees, outputs, lockfiles, stderr
    // (with the action graph).
    assert_eq!(f.trees("b"), f.trees("c"));
    assert_eq!(f.trees("a"), f.trees("c"));
    assert_eq!(f.out("b"), f.out("c"));
    assert_eq!(
        fs::read(ws_b.join("MODULE.bazel.lock")).unwrap(),
        fs::read(ws_c.join("MODULE.bazel.lock")).unwrap()
    );
    assert_eq!(f.hidden_lockfile("b"), f.hidden_lockfile("c"));
    assert_eq!(
        normalized(&hit, &f, "b", &ws_b),
        normalized(&plain, &f, "c", &ws_c)
    );
    // The next command reuses the publications (no lookup at all), like
    // the fetched cache's.
    let warm = f.run(&ws_b, "b", &[FLAG], &[&remote]);
    assert_success(&warm);
    assert_eq!(stats(&warm), "hits=0 uploads=0");
    let warm_plain = f.run(&ws_c, "c", &[], &[&remote]);
    assert_eq!(
        normalized(&warm, &f, "b", &ws_b),
        normalized(&warm_plain, &f, "c", &ws_c)
    );
    let log = f.server.take_log();
    assert!(
        !log.iter().any(|(_, path)| path.starts_with("/ac/")),
        "{log:?}"
    );
    // The download wrote through the disk cache: a disk cache alone serves
    // the next cold cache, without any request.
    let ws_d = f.workspace("ws-d", "one\n");
    let disk_only = f.run(&ws_d, "d", &[FLAG], &[&disk_flag]);
    assert_success(&disk_only);
    assert_eq!(stats(&disk_only), "hits=3 uploads=0");
    assert!(f.server.take_log().is_empty());
    assert_eq!(f.trees("d"), f.trees("c"));
}

#[test]
fn recorded_inputs_select_among_the_stored_trees() {
    let f = Fixture::new();
    let remote = f.remote();
    let ws = f.workspace("ws-1", "one\n");
    assert_eq!(
        stats(&f.run(&ws, "c1", &[FLAG], &[&remote])),
        "hits=0 uploads=3"
    );
    // A changed watched input: that tree is not taken (its rule runs and
    // stores a second one), the others are.
    let ws2 = f.workspace("ws-2", "two\n");
    let changed = f.run(&ws2, "c2", &[FLAG], &[&remote]);
    assert_success(&changed);
    assert_eq!(stats(&changed), "hits=2 uploads=1");
    assert!(f.out("c2").contains("two"));
    f.server.take_log();
    // Both are served now, each where its input holds.
    for (name, input, cache) in [("ws-3", "one\n", "c3"), ("ws-4", "two\n", "c4")] {
        let ws = f.workspace(name, input);
        let output = f.run(&ws, cache, &[FLAG], &[&remote]);
        assert_success(&output);
        assert_eq!(stats(&output), "hits=3 uploads=0", "{name}");
        assert!(f.out(cache).contains(input.trim()));
        assert_eq!(Server::fetches(&f.server.take_log()), 0);
    }
    assert_eq!(f.trees("c3"), f.trees("c1"));
    assert_eq!(f.trees("c4"), f.trees("c2"));
}

#[test]
fn corrupt_and_missing_blobs_fall_back_to_a_fetch() {
    let f = Fixture::new();
    let remote = f.remote();
    let ws = f.workspace("ws-1", "one\n");
    assert_success(&f.run(&ws, "c1", &[FLAG], &[&remote]));
    let (blob, original) = f.server.chunk_holding(DATA);
    f.server.take_log();
    // A corrupt blob: a warning, then the archive is fetched.
    f.server.set_blob(&blob, Some(b"corrupt".to_vec()));
    let ws2 = f.workspace("ws-2", "one\n");
    let corrupt = f.run(&ws2, "c2", &[FLAG], &[&remote]);
    assert_success(&corrupt);
    assert_eq!(stats(&corrupt), "hits=2 uploads=1");
    let stderr = String::from_utf8_lossy(&corrupt.stderr);
    assert!(
        stderr.contains("WARNING: Remote repo contents cache lookup failed for +http_archive+ext:"),
        "{stderr}"
    );
    assert_eq!(Server::fetches(&f.server.take_log()), 1);
    assert_eq!(f.trees("c2"), f.trees("c1"));
    // The fetch's upload repaired it; a missing blob falls back silently.
    assert_eq!(
        f.server.state.lock().unwrap().blobs.get(&blob),
        Some(&original)
    );
    f.server.set_blob(&blob, None);
    let ws3 = f.workspace("ws-3", "one\n");
    let missing = f.run(&ws3, "c3", &[FLAG], &[&remote]);
    assert_success(&missing);
    assert_eq!(stats(&missing), "hits=2 uploads=1");
    assert!(!String::from_utf8_lossy(&missing.stderr).contains("WARNING: Remote repo"));
    assert_eq!(Server::fetches(&f.server.take_log()), 1);
    assert_eq!(f.trees("c3"), f.trees("c1"));
}

#[test]
fn the_option_and_the_cache_policies_gate_lookups_and_uploads() {
    let f = Fixture::new();
    let remote = f.remote();
    let ws = f.workspace("ws-1", "one\n");
    // Without the startup option (or with its negation last): the remote
    // cache is never asked for repositories.
    for startup in [
        &[][..],
        &[FLAG, "--noexperimental_remote_repo_contents_cache"][..],
    ] {
        let cache = format!("off-{}", startup.len());
        let output = f.run(&ws, &cache, startup, &[&remote]);
        assert_success(&output);
        assert_eq!(stats(&output), "hits=0 uploads=0");
        let log = f.server.take_log();
        assert_eq!(Server::puts(&log), 0, "{log:?}");
        assert!(log.iter().all(|(_, path)| path.starts_with("/files/")));
    }
    // --noremote_upload_local_results: lookups only.
    let output = f.run(
        &ws,
        "ro",
        &[FLAG],
        &[&remote, "--noremote_upload_local_results"],
    );
    assert_success(&output);
    assert_eq!(stats(&output), "hits=0 uploads=0");
    let log = f.server.take_log();
    assert_eq!(Server::puts(&log), 0);
    assert!(
        log.iter()
            .any(|(method, path)| method == "GET" && path.starts_with("/ac/"))
    );
    // A `startup` rc line enables it; --noremote_accept_cached: uploads only.
    let rc = f.root.join("rc");
    fs::write(&rc, format!("startup {FLAG}\n")).unwrap();
    let mut command = Command::new(env!("CARGO_BIN_EXE_bazel-rs"));
    let output = command
        .arg(format!("--bazelrc={}", rc.display()))
        .arg(format!("--cache-dir={}", f.root.join("wo").display()))
        .arg("build")
        .arg(builtin_registry_flag())
        .arg(&remote)
        .arg("--noremote_accept_cached")
        .arg("//:use")
        .current_dir(&ws)
        .env_clear()
        .envs(verifier_environment())
        .env("PATH", "/usr/bin:/bin")
        .env("HOME", f.root.join("home"))
        .env("BAZEL_RS_REMOTE_REPO_CACHE_STATS", "1")
        .seeded_archives()
        .output()
        .unwrap();
    assert_success(&output);
    assert_eq!(stats(&output), "hits=0 uploads=3");
    let log = f.server.take_log();
    assert!(
        !log.iter()
            .any(|(method, path)| method == "GET" && path.starts_with("/cas/")),
        "{log:?}"
    );
    // An unreachable cache: a few warnings, then the cache is off and the
    // repositories are fetched.
    let output = f.run(
        &ws,
        "down",
        &[FLAG],
        &["--remote_cache=http://127.0.0.1:1", "--remote_retries=0"],
    );
    assert_success(&output);
    let stderr = String::from_utf8_lossy(&output.stderr);
    let warnings = stderr
        .matches("WARNING: Remote repo contents cache lookup failed for ")
        .count();
    assert!((1..=5).contains(&warnings), "{stderr}");
    assert_eq!(Server::fetches(&f.server.take_log()), 2);
    // A command option it is not (genuine: `ERROR: … :: Unrecognized option`, exit 2).
    let output = f.run(&ws, "bad", &[], &[FLAG]);
    assert_eq!(output.status.code(), Some(2));
    assert!(
        String::from_utf8_lossy(&output.stderr).contains(
            "--experimental_remote_repo_contents_cache :: Unrecognized option: --experimental_remote_repo_contents_cache"
        )
    );
    // It takes no value (blaze.cc: `does not take a value`).
    let output = f.run(&ws, "bad", &[&format!("{FLAG}=yes")], &[]);
    assert!(!output.status.success());
    assert!(
        String::from_utf8_lossy(&output.stderr).contains(
            "In argument '--experimental_remote_repo_contents_cache=yes': option '--experimental_remote_repo_contents_cache' does not take a value."
        )
    );
}

/// The option's parsing against genuine 9.3.1's
/// (tests/fixtures/remote_repo_cache/reference_receipt.json, capture.py):
/// a startup option on the command line or in an rc `startup` line, its
/// negation, no value, no command option. Messages are compared without
/// the client's prefix (`ERROR: `, `[FATAL …] `, `bazel-rs: `); the exit
/// code of a startup argument with a value is 1 here, 2 in genuine (every
/// OptionProcessor::SplitCommandLine failure is BAD_ARGV there; a native
/// difference of all such failures, docs/REPO_CONTENTS_CACHE.md).
#[test]
fn the_option_parses_like_genuine() {
    let receipt: serde_json::Value = serde_json::from_str(include_str!(
        "fixtures/remote_repo_cache/reference_receipt.json"
    ))
    .unwrap();
    let root = crate::test_temp::root().join(format!(
        "native-remote-repo-cache-parse-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    let ws = root.join("ws");
    fs::create_dir_all(&ws).unwrap();
    fs::create_dir_all(root.join("home")).unwrap();
    fs::write(ws.join("MODULE.bazel"), "module(name = \"rrc\")\n").unwrap();
    fs::write(ws.join("BUILD.bazel"), "").unwrap();
    let rc = root.join("rc");
    fs::write(&rc, "startup --experimental_remote_repo_contents_cache\n").unwrap();
    let message = |line: &str| {
        let line = line
            .strip_prefix("ERROR: ")
            .or_else(|| line.strip_prefix("bazel-rs: "))
            .unwrap_or(line);
        match line.strip_prefix("[FATAL") {
            Some(rest) => rest
                .split_once("] ")
                .map_or(rest, |(_, text)| text)
                .to_owned(),
            None => line.to_owned(),
        }
    };
    for (name, case) in receipt["cases"].as_object().unwrap() {
        let args: Vec<String> = case["args"]
            .as_array()
            .unwrap()
            .iter()
            .map(|arg| {
                arg.as_str()
                    .unwrap()
                    .replace("SCRATCH/rc", &rc.display().to_string())
            })
            .collect();
        let build = args.iter().position(|arg| arg == "build").unwrap();
        let mut command = Command::new(env!("CARGO_BIN_EXE_bazel-rs"));
        command
            .arg(format!("--cache-dir={}", root.join("cache").display()))
            .args(&args[..=build])
            .arg(builtin_registry_flag())
            .args(&args[build + 1..])
            .current_dir(&ws)
            .env_clear()
            .envs(verifier_environment())
            .env("PATH", "/usr/bin:/bin")
            .env("HOME", root.join("home"));
        let output = command.seeded_archives().output().unwrap();
        let stderr = String::from_utf8_lossy(&output.stderr);
        let genuine = case["exit_code"].as_i64().unwrap() as i32;
        let expected = if name == "startup_value" { 1 } else { genuine };
        assert_eq!(output.status.code(), Some(expected), "{name}: {stderr}");
        for line in case["lines"].as_array().unwrap() {
            let wanted = message(line.as_str().unwrap());
            assert!(
                stderr.lines().any(|ours| message(ours) == wanted),
                "{name}: {wanted:?} not in {stderr}"
            );
        }
    }
    let _ = sealed_trees::remove_tree(&root);
}

/// A gRPC cache (FindMissingBlobs, ByteStream, the Action Cache): the
/// same round trip as over HTTP.
#[test]
fn a_grpc_cache_serves_and_stores_repositories() {
    let f = Fixture::new();
    let grpc = grpc_cache_server::Server::new();
    let remote = format!("--remote_cache={}", grpc.url());
    let ws_a = f.workspace("ws-a", "one\n");
    let uploaded = f.run(&ws_a, "a", &[FLAG], &[&remote]);
    assert_success(&uploaded);
    assert_eq!(stats(&uploaded), "hits=0 uploads=3");
    assert_eq!(Server::fetches(&f.server.take_log()), 2);
    let methods = grpc.method_counts(&grpc.take_log());
    assert!(
        methods
            .keys()
            .any(|method| method.contains("UpdateActionResult")),
        "{methods:?}"
    );
    let ws_b = f.workspace("ws-b", "one\n");
    let hit = f.run(&ws_b, "b", &[FLAG], &[&remote]);
    assert_success(&hit);
    assert_eq!(stats(&hit), "hits=3 uploads=0");
    assert_eq!(Server::fetches(&f.server.take_log()), 0);
    assert_eq!(f.trees("b"), f.trees("a"));
    assert_eq!(f.out("b"), f.out("a"));
}

/// An untrusted entry whose pack chunk is a zstd bomb (a few bytes that
/// decode to far more than the chunk claims): decoding stops at the claim,
/// the repository is fetched.
#[test]
fn a_chunk_decoding_past_its_size_is_rejected() {
    let f = Fixture::new();
    let remote = f.remote();
    let ws = f.workspace("ws-1", "one\n");
    assert_success(&f.run(&ws, "c1", &[FLAG], &[&remote]));
    f.server.rewrite_entry("lib/data.txt", |manifest, server| {
        let bomb = ruzstd::encoding::compress_to_vec(
            vec![0u8; 64 << 20].as_slice(),
            ruzstd::encoding::CompressionLevel::Fastest,
        );
        let chunk = &mut manifest["chunks"][0];
        chunk["sha256"] = sha256_hex(&bomb).into();
        chunk["size"] = bomb.len().into();
        server.set_blob(&format!("/cas/{}", sha256_hex(&bomb)), Some(bomb));
    });
    f.server.take_log();
    let ws2 = f.workspace("ws-2", "one\n");
    let output = f.run(
        &ws2,
        "c2",
        &[FLAG],
        &[&remote, "--noremote_upload_local_results"],
    );
    assert_success(&output);
    assert_eq!(stats(&output), "hits=2 uploads=0");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("decodes to more than"), "{stderr}");
    assert_eq!(Server::fetches(&f.server.take_log()), 1);
    assert_eq!(f.trees("c2"), f.trees("c1"));
}

/// An untrusted entry with a link that escapes through another link of
/// the tree (each lexically inside): rejected before anything is written.
#[test]
fn links_through_links_out_of_the_tree_are_rejected() {
    let f = Fixture::new();
    let remote = f.remote();
    let ws = f.workspace("ws-1", "one\n");
    assert_success(&f.run(&ws, "c1", &[FLAG], &[&remote]));
    f.server.rewrite_entry("lib/data.txt", |manifest, _| {
        let entries = manifest["entries"].as_array_mut().unwrap();
        for dir in ["a", "a/b", "a/b/c", "a/b/c/d", "a/b/c/d/e"] {
            entries.push(serde_json::json!({"path": dir, "directory": true, "mode": 493, "size": 0, "sha256": ""}));
        }
        for (path, target) in [
            ("a/b/c/d/e/u", "../../../../.."),
            ("a/b/c/d/e/t", "u/../../../../../etc/passwd"),
        ] {
            entries.push(serde_json::json!({"path": path, "directory": false, "mode": 0, "size": target.len(), "sha256": sha256_hex(target.as_bytes()), "symlink": target}));
        }
    });
    f.server.take_log();
    let ws2 = f.workspace("ws-2", "one\n");
    let output = f.run(
        &ws2,
        "c2",
        &[FLAG],
        &[&remote, "--noremote_upload_local_results"],
    );
    assert_success(&output);
    assert_eq!(stats(&output), "hits=2 uploads=0");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("a cached tree link leaves the repository"),
        "{stderr}"
    );
    assert_eq!(Server::fetches(&f.server.take_log()), 1);
    assert_eq!(f.trees("c2"), f.trees("c1"));
}

/// A read-only token (every upload refused) with the default
/// `--remote_upload_local_results`: uploads stop after five failures, the
/// hits go on.
#[test]
fn refused_uploads_never_stop_lookups() {
    let f = Fixture::new();
    let remote = f.remote();
    // Six more reproducible repositories (the root module's mapping is part
    // of every specification, so they are declared from the start).
    let ws = f.workspace("ws-1", "one\n");
    let url = f.server.url();
    let mut module = fs::read_to_string(ws.join("MODULE.bazel")).unwrap();
    let mut extra = String::new();
    for index in 0..6 {
        let bytes = format!("extra {index}\n");
        f.server
            .serve(&format!("extra{index}.txt"), bytes.as_bytes());
        module.push_str(&format!(
            "http_file(name = \"extra{index}\", urls = [\"{url}/files/extra{index}.txt\"], sha256 = \"{}\")\n",
            sha256_hex(bytes.as_bytes())
        ));
        extra.push_str(&format!(", \"@extra{index}//file\""));
    }
    fs::write(ws.join("MODULE.bazel"), &module).unwrap();
    let build = fs::read_to_string(ws.join("BUILD.bazel")).unwrap().replace(
        "\"@abs_link//:value.txt\"]",
        &format!("\"@abs_link//:value.txt\"{extra}]"),
    );
    fs::write(ws.join("BUILD.bazel"), &build).unwrap();
    assert_success(&f.run(&ws, "c1", &[FLAG], &[&remote]));
    // The extras' trees leave the cache; every upload is refused now.
    for index in 0..6 {
        let (chunk, _) = f
            .server
            .chunk_holding(format!("extra {index}\n").as_bytes());
        f.server.set_blob(&chunk, None);
    }
    f.server.state.lock().unwrap().refuse_puts = true;
    let ws2 = f.workspace("ws-2", "one\n");
    fs::write(ws2.join("MODULE.bazel"), &module).unwrap();
    fs::write(ws2.join("BUILD.bazel"), &build).unwrap();
    f.server.take_log();
    let output = f.run(&ws2, "c2", &[FLAG], &[&remote]);
    assert_success(&output);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(stats(&output), "hits=3 uploads=0", "{stderr}");
    assert_eq!(
        stderr
            .matches("WARNING: Failed to upload repo contents to remote cache for ")
            .count(),
        5,
        "{stderr}"
    );
    assert!(
        stderr.contains("its uploads stop for the rest of this command"),
        "{stderr}"
    );
    assert!(!stderr.contains("its lookups stop"), "{stderr}");
    assert_eq!(Server::fetches(&f.server.take_log()), 6);
}

/// An unrelated dependency and an unused repository added to the root
/// module change no specification (docs/REPO_CONTENTS_CACHE.md "Keys"):
/// a cold cache of that revision still takes every eligible repository
/// from the remote cache.
#[test]
fn unrelated_dependencies_keep_remote_hits() {
    let f = Fixture::new();
    let remote = f.remote();
    let ws1 = f.workspace("ws-1", "one\n");
    assert_eq!(
        stats(&f.run(&ws1, "c1", &[FLAG], &[&remote])),
        "hits=0 uploads=3"
    );
    let ws2 = f.workspace("ws-2", "one\n");
    let mut module = fs::read_to_string(ws2.join("MODULE.bazel")).unwrap();
    module.push_str(&format!(
        "bazel_dep(name = \"platforms\", version = \"0.0.4\")\nhttp_file(name = \"unused\", urls = [\"{}/files/missing.txt\"], sha256 = \"{}\")\n",
        f.server.url(),
        "0".repeat(64)
    ));
    fs::write(ws2.join("MODULE.bazel"), module).unwrap();
    f.server.take_log();
    let output = f.run(&ws2, "c2", &[FLAG], &[&remote]);
    assert_success(&output);
    assert_eq!(stats(&output), "hits=3 uploads=0");
    assert_eq!(Server::fetches(&f.server.take_log()), 0);
    assert_eq!(f.trees("c2"), f.trees("c1"));
}
