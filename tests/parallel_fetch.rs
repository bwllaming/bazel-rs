//! Parallel repository fetching (docs/PARALLEL_ANALYSIS.md, "Parallel
//! repository fetching"; crate::repository_speculation): the repositories
//! phase-1 prefetch finds the build certainly needs execute on background
//! workers ahead of their sequential demand. These tests pin that nothing
//! observable changes: cold builds of a workspace with many slow repository
//! rules (each downloads from a local HTTP fixture that answers late) —
//! direct and extension-generated, printing ones (print() and the
//! `execute(quiet=False)` echo), one that demands another repository, one
//! that reads a workspace file, one on a select() branch not taken — are
//! byte-identical in exit code, stdout, stderr, lockfile and published
//! trees at `--loading_phase_threads` 1 (the sequential engine), 8, 32 and
//! with worker jitter; the downloads overlap; the repository on the branch
//! not taken is never fetched; the first failure in sequential order is the
//! one reported; a failing build does not wait for speculative downloads,
//! and an interrupted one leaves a store the next build reuses.

#![allow(clippy::disallowed_methods)] // test harness: starts processes directly (clippy.toml)
use std::collections::BTreeMap;
use std::fs;
use std::io::{Read, Write};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
include!("support/explicit_modules.rs");

/// A loopback HTTP server answering `GET /<name>` with `payload <name>`
/// after `delay` (a `/trickle/<name>` body arrives one byte per 100 ms),
/// one thread per connection; it records the paths requested and the
/// largest number of requests in flight at once.
struct Server {
    url: String,
    paths: Arc<Mutex<Vec<String>>>,
    peak: Arc<AtomicUsize>,
    stop: Arc<AtomicBool>,
    worker: Option<std::thread::JoinHandle<()>>,
}

impl Server {
    fn new(delay: Duration) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let paths = Arc::new(Mutex::new(Vec::new()));
        let peak = Arc::new(AtomicUsize::new(0));
        let stop = Arc::new(AtomicBool::new(false));
        let (logged, peaked, stopped) = (paths.clone(), peak.clone(), stop.clone());
        let worker = std::thread::spawn(move || {
            let current = Arc::new(AtomicUsize::new(0));
            while !stopped.load(Ordering::SeqCst) {
                let mut stream = match listener.accept() {
                    Ok((stream, _)) => stream,
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(Duration::from_millis(2));
                        continue;
                    }
                    Err(error) => panic!("listener: {error}"),
                };
                let (logged, peaked, current, stopped) = (
                    logged.clone(),
                    peaked.clone(),
                    current.clone(),
                    stopped.clone(),
                );
                std::thread::spawn(move || {
                    stream.set_nonblocking(false).unwrap();
                    stream
                        .set_read_timeout(Some(Duration::from_secs(30)))
                        .unwrap();
                    let mut request = Vec::new();
                    let mut byte = [0];
                    while request.len() < 16384 && !request.ends_with(b"\r\n\r\n") {
                        if stream.read(&mut byte).unwrap_or(0) == 0 {
                            return;
                        }
                        request.push(byte[0]);
                    }
                    let request = String::from_utf8_lossy(&request).into_owned();
                    let path = request
                        .split_whitespace()
                        .nth(1)
                        .unwrap_or_default()
                        .to_owned();
                    logged.lock().unwrap().push(path.clone());
                    let now = current.fetch_add(1, Ordering::SeqCst) + 1;
                    peaked.fetch_max(now, Ordering::SeqCst);
                    std::thread::sleep(delay);
                    let name = path.trim_start_matches("/trickle").trim_start_matches('/');
                    let body = format!("payload {name}\n");
                    let header = format!(
                        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                        body.len()
                    );
                    let _ = stream.write_all(header.as_bytes());
                    if path.starts_with("/trickle/") {
                        for byte in body.as_bytes() {
                            if stopped.load(Ordering::SeqCst) || stream.write_all(&[*byte]).is_err()
                            {
                                break;
                            }
                            let _ = stream.flush();
                            std::thread::sleep(Duration::from_millis(100));
                        }
                    } else {
                        let _ = stream.write_all(body.as_bytes());
                    }
                    current.fetch_sub(1, Ordering::SeqCst);
                });
            }
        });
        Self {
            url,
            paths,
            peak,
            stop,
            worker: Some(worker),
        }
    }

    /// The paths requested since the last call, and the peak concurrency
    /// (both reset).
    fn take(&self) -> (Vec<String>, usize) {
        let paths = std::mem::take(&mut *self.paths.lock().unwrap());
        (paths, self.peak.swap(0, Ordering::SeqCst))
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

struct Workspace(tempfile::TempDir);

impl Workspace {
    fn new() -> Self {
        Self(
            tempfile::Builder::new()
                .prefix("bazel-rs-parallel-fetch-")
                .tempdir_in("/tmp/opencode")
                .unwrap(),
        )
    }

    fn path(&self) -> &Path {
        self.0.path()
    }

    fn file(&self, name: &str, content: &str) {
        let path = self.path().join(name);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, content).unwrap();
    }

    fn command(&self, threads: usize, jitter: u64, args: &[&str]) -> Command {
        let mut command = Command::new("timeout");
        command
            .args([
                "600s",
                env!("CARGO_BIN_EXE_bazel-rs"),
                "--ignore_all_rc_files",
                "--cache-dir=cache",
                args[0],
            ])
            .arg(format!("--loading_phase_threads={threads}"))
            .args(&args[1..])
            .env_remove("BAZEL_RS_TIMING")
            .env_remove("BAZEL_RS_DUMP_ACTIONS")
            .env_remove("BAZEL_RS_LOADING_JITTER")
            .env_remove("BAZEL_RS_DEBUG_NO_SPECULATION")
            .env("FETCH_FIXTURE_ENV", "from-the-client")
            .current_dir(self.path());
        if jitter > 0 {
            command.env("BAZEL_RS_LOADING_JITTER", jitter.to_string());
        }
        command.explicit_modules();
        command
    }

    /// A cold build: no cache, no lockfile.
    fn cold(&self, threads: usize, jitter: u64, args: &[&str]) -> Run {
        let _ = fs::remove_dir_all(self.path().join("cache"));
        let _ = fs::remove_file(self.path().join("MODULE.bazel.lock"));
        let started = Instant::now();
        let output = self.command(threads, jitter, args).output().unwrap();
        Run {
            elapsed: started.elapsed(),
            lockfile: fs::read(self.path().join("MODULE.bazel.lock")).ok(),
            trees: trees(&self.path().join("cache/repositories/direct/trees")),
            output,
        }
    }
}

struct Run {
    output: Output,
    lockfile: Option<Vec<u8>>,
    /// Every published tree: identity -> (relative path -> content).
    trees: BTreeMap<String, BTreeMap<String, Vec<u8>>>,
    elapsed: Duration,
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

/// The published repository trees under `root` (identity -> files).
fn trees(root: &Path) -> BTreeMap<String, BTreeMap<String, Vec<u8>>> {
    fn walk(base: &Path, path: &Path, files: &mut BTreeMap<String, Vec<u8>>) {
        let Ok(entries) = fs::read_dir(path) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let relative = path.strip_prefix(base).unwrap().display().to_string();
            if path.is_dir() {
                walk(base, &path, files);
            } else {
                files.insert(relative, fs::read(&path).unwrap_or_default());
            }
        }
    }
    let mut trees = BTreeMap::new();
    let Ok(entries) = fs::read_dir(root) else {
        return trees;
    };
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if name.starts_with('.') {
            continue;
        }
        let mut files = BTreeMap::new();
        walk(&entry.path(), &entry.path(), &mut files);
        trees.insert(name, files);
    }
    trees
}

/// The `url` and `sha256` attributes of the payload `name` on `server`.
fn source(server: &Server, name: &str) -> String {
    format!(
        "url = '{}/{name}', sha256 = '{}'",
        server.url,
        sha256_hex(format!("payload {name}\n").as_bytes())
    )
}

fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::Digest as _;
    sha2::Sha256::digest(bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

/// The repository rule of every fixture: optionally demands another
/// repository's file, reads a workspace file and an environment variable,
/// downloads its payload, prints, and fails on request.
const DEFS: &str = r#"
def _slow_impl(ctx):
    extra = ""
    if ctx.attr.dep:
        extra += ctx.read(ctx.path(ctx.attr.dep))
    if ctx.attr.input:
        extra += ctx.read(ctx.attr.input)
        extra += ctx.getenv("FETCH_FIXTURE_ENV", "unset") + "\n"
    ctx.download(url = ctx.attr.url, output = "payload.txt", sha256 = ctx.attr.sha256)
    if ctx.attr.message:
        print("fetching " + ctx.attr.message)
        ctx.execute(["echo", "echoed " + ctx.attr.message], quiet = False)
    if ctx.attr.fail_with:
        fail(ctx.attr.fail_with)
    ctx.file("extra.txt", extra)
    ctx.file("BUILD.bazel", "exports_files(['payload.txt', 'extra.txt'])\nfilegroup(name = 'data', srcs = ['payload.txt', 'extra.txt'], visibility = ['//visibility:public'])\n")

slow = repository_rule(
    implementation = _slow_impl,
    attrs = {
        "url": attr.string(),
        "sha256": attr.string(),
        "dep": attr.label(),
        "input": attr.label(),
        "message": attr.string(),
        "fail_with": attr.string(),
    },
)

def _ext_impl(module_ctx):
    for module in module_ctx.modules:
        for repo in module.tags.repo:
            slow(name = repo.name, url = repo.url, sha256 = repo.sha256, message = repo.message)

ext = module_extension(
    implementation = _ext_impl,
    tag_classes = {"repo": tag_class(attrs = {"name": attr.string(), "url": attr.string(), "sha256": attr.string(), "message": attr.string()})},
)
"#;

/// The many-slow-repositories workspace: eight direct repositories (one
/// printing, one demanding another's file, one reading a workspace file and
/// the client environment), six extension-generated ones (one printing),
/// and a repository only a select() branch not taken names.
fn fixture(server: &Server) -> (Workspace, Vec<String>) {
    let ws = Workspace::new();
    let url = |name: &str| source(server, name);
    let mut module = String::from(
        "module(name = 'main')\nslow = use_repo_rule('//:defs.bzl', 'slow')\next = use_extension('//:defs.bzl', 'ext')\n",
    );
    let mut labels = Vec::new();
    for index in 0..8 {
        let name = format!("d{index}");
        let extra = match index {
            2 => ", message = 'direct two'".to_owned(),
            5 => ", dep = '@d6//:payload.txt'".to_owned(),
            6 => ", input = '//:input.txt'".to_owned(),
            _ => String::new(),
        };
        module.push_str(&format!("slow(name = '{name}', {}{extra})\n", url(&name)));
        labels.push(format!("@{name}//:data"));
    }
    module.push_str(&format!("slow(name = 'other', {})\n", url("other")));
    let mut imports = Vec::new();
    for index in 0..6 {
        let name = format!("e{index}");
        let message = if index == 3 { "generated three" } else { "" };
        module.push_str(&format!(
            "ext.repo(name = '{name}', {}, message = '{message}')\n",
            url(&name)
        ));
        imports.push(format!("'{name}'"));
        labels.push(format!("@{name}//:data"));
    }
    module.push_str(&format!("use_repo(ext, {})\n", imports.join(", ")));
    ws.file("MODULE.bazel", &module);
    ws.file("defs.bzl", DEFS);
    ws.file("input.txt", "workspace input\n");
    let srcs: Vec<String> = labels.iter().map(|label| format!("'{label}'")).collect();
    ws.file(
        "BUILD.bazel",
        &format!(
            "exports_files(['input.txt'])\nconfig_setting(name = 'never', values = {{'define': 'never=1'}})\nfilegroup(name = 'everything', srcs = [{}] + select({{':never': ['@other//:data'], '//conditions:default': []}}))\n",
            srcs.join(", ")
        ),
    );
    (ws, labels)
}

/// Thread counts and worker jitter compared with the sequential engine.
const THREADED: [(usize, u64); 3] = [(8, 0), (32, 0), (8, 40)];

fn assert_same(reference: &Run, other: &Run, what: &str) {
    assert_same_output(reference, other, what);
    assert_eq!(
        other.trees.keys().collect::<Vec<_>>(),
        reference.trees.keys().collect::<Vec<_>>(),
        "{what}: published tree identities"
    );
    assert_eq!(other.trees, reference.trees, "{what}: published trees");
}

/// Exit code, stdout, stderr and lockfile.
fn assert_same_output(reference: &Run, other: &Run, what: &str) {
    assert_eq!(
        other.output.status.code(),
        reference.output.status.code(),
        "{what}: exit\n{}",
        stderr(&other.output)
    );
    assert_eq!(
        other.output.stdout, reference.output.stdout,
        "{what}: stdout"
    );
    assert_eq!(
        stderr(&other.output),
        stderr(&reference.output),
        "{what}: stderr"
    );
    assert_eq!(other.lockfile, reference.lockfile, "{what}: lockfile");
}

#[test]
fn cold_builds_are_identical_at_every_thread_count_and_downloads_overlap() {
    let server = Server::new(Duration::from_millis(300));
    let (ws, _) = fixture(&server);
    let reference = ws.cold(1, 0, &["build", "//:everything"]);
    assert!(
        reference.output.status.success(),
        "{}",
        stderr(&reference.output)
    );
    let (paths, peak) = server.take();
    assert_eq!(peak, 1, "the sequential engine fetches one at a time");
    assert!(!paths.iter().any(|path| path == "/other"), "{paths:?}");
    assert_eq!(paths.len(), 14, "{paths:?}");
    let text = stderr(&reference.output);
    for line in [
        "fetching direct two",
        "echoed direct two",
        "fetching generated three",
        "echoed generated three",
    ] {
        assert_eq!(text.matches(line).count(), 1, "{line}:\n{text}");
    }
    for (threads, jitter) in THREADED {
        let what = format!("threads={threads} jitter={jitter}");
        let run = ws.cold(threads, jitter, &["build", "//:everything"]);
        assert_same(&reference, &run, &what);
        let (paths, peak) = server.take();
        assert!(
            !paths.iter().any(|path| path == "/other"),
            "{what}: {paths:?}"
        );
        assert_eq!(
            paths.len(),
            14,
            "{what}: every repository fetched once: {paths:?}"
        );
        if jitter == 0 {
            assert!(peak > 1, "{what}: downloads overlap (peak {peak})");
            assert!(
                run.elapsed < reference.elapsed,
                "{what}: {:?} vs sequential {:?}",
                run.elapsed,
                reference.elapsed
            );
        }
    }
    // Without speculation the threaded engine is the sequential one too.
    let mut command = ws.command(8, 0, &["build", "//:everything"]);
    let _ = fs::remove_dir_all(ws.path().join("cache"));
    let _ = fs::remove_file(ws.path().join("MODULE.bazel.lock"));
    let output = command
        .env("BAZEL_RS_DEBUG_NO_SPECULATION", "1")
        .output()
        .unwrap();
    assert_eq!(output.stdout, reference.output.stdout);
    assert_eq!(stderr(&output), stderr(&reference.output));
    let (_, peak) = server.take();
    assert_eq!(peak, 1, "speculation off");
    // A warm build after a threaded cold one: nothing executes, nothing
    // prints, the same trees.
    let warm = ws
        .command(8, 0, &["build", "//:everything"])
        .output()
        .unwrap();
    assert!(warm.status.success(), "{}", stderr(&warm));
    assert_eq!(warm.stdout, reference.output.stdout);
    assert!(!stderr(&warm).contains("fetching"), "{}", stderr(&warm));
    let (paths, _) = server.take();
    assert!(paths.is_empty(), "{paths:?}");
}

/// The first failure in sequential order is the one reported. Repositories
/// after it that speculations executed are published (as in genuine
/// Bazel, whose parallel fetches complete), but one whose output the failed
/// build never showed is withdrawn: once the failure is fixed, the next
/// build prints exactly what the sequential engine's next build prints.
#[test]
fn the_first_failure_in_sequential_order_is_reported() {
    let server = Server::new(Duration::from_millis(200));
    let ws = Workspace::new();
    let names = ["a0", "a1", "bad1", "a2", "bad2", "a3"];
    let module = |failing: bool| {
        let mut module =
            String::from("module(name = 'main')\nslow = use_repo_rule('//:defs.bzl', 'slow')\n");
        for (name, extra) in names.iter().zip([
            "",
            ", message = 'before the failure'",
            ", message = 'first bad', fail_with = 'first failure'",
            "",
            ", message = 'second bad', fail_with = 'second failure'",
            ", message = 'after the failure'",
        ]) {
            let extra = if failing {
                extra
            } else {
                extra.split(", fail_with").next().unwrap()
            };
            module.push_str(&format!(
                "slow(name = '{name}', {}{extra})\n",
                source(&server, name)
            ));
        }
        module
    };
    let srcs: Vec<String> = names
        .iter()
        .map(|name| format!("'@{name}//:data'"))
        .collect();
    ws.file("defs.bzl", DEFS);
    ws.file(
        "BUILD.bazel",
        &format!(
            "filegroup(name = 'everything', srcs = [{}])\n",
            srcs.join(", ")
        ),
    );
    let failed_then_fixed = |threads: usize, jitter: u64| {
        ws.file("MODULE.bazel", &module(true));
        let failed = ws.cold(threads, jitter, &["build", "//:everything"]);
        ws.file("MODULE.bazel", &module(false));
        let fixed = ws
            .command(threads, jitter, &["build", "//:everything"])
            .output()
            .unwrap();
        (failed, fixed)
    };
    let (reference, reference_fixed) = failed_then_fixed(1, 0);
    assert!(!reference.output.status.success());
    let text = stderr(&reference.output);
    assert!(text.contains("first failure"), "{text}");
    assert!(!text.contains("second failure"), "{text}");
    assert!(!text.contains("after the failure"), "{text}");
    assert!(
        reference_fixed.status.success(),
        "{}",
        stderr(&reference_fixed)
    );
    assert!(
        stderr(&reference_fixed).contains("fetching after the failure"),
        "{}",
        stderr(&reference_fixed)
    );
    for (threads, jitter) in THREADED {
        let what = format!("threads={threads} jitter={jitter}");
        let (failed, fixed) = failed_then_fixed(threads, jitter);
        assert_same_output(&reference, &failed, &what);
        assert_eq!(fixed.stdout, reference_fixed.stdout, "{what}: fixed stdout");
        assert_eq!(
            stderr(&fixed),
            stderr(&reference_fixed),
            "{what}: fixed stderr"
        );
    }
    let _ = server.take();
}

/// A failing build stops the speculations in flight instead of waiting for
/// them: here a repository after the failure trickles its payload for
/// seconds, and only a speculation ever starts it.
#[test]
fn a_failing_build_cancels_speculative_fetches() {
    let server = Server::new(Duration::from_millis(0));
    let ws = Workspace::new();
    let url = |name: &str| source(&server, name);
    ws.file(
        "MODULE.bazel",
        &format!(
            "module(name = 'main')\nslow = use_repo_rule('//:defs.bzl', 'slow')\nslow(name = 'bad', {}, fail_with = 'early failure')\nslow(name = 'crawl', url = '{}/trickle/crawl', sha256 = '{}')\n",
            url("bad"),
            server.url,
            sha256_hex(b"payload crawl\n"),
        ),
    );
    ws.file("defs.bzl", DEFS);
    ws.file(
        "BUILD.bazel",
        "filegroup(name = 'everything', srcs = ['@bad//:data', '@crawl//:data'])\n",
    );
    let reference = ws.cold(1, 0, &["build", "//:everything"]);
    assert!(stderr(&reference.output).contains("early failure"));
    let (paths, _) = server.take();
    assert!(
        !paths.iter().any(|path| path.contains("crawl")),
        "{paths:?}"
    );
    let run = ws.cold(8, 0, &["build", "//:everything"]);
    assert_same(&reference, &run, "threads=8");
    // "payload crawl\n" takes 1.4 s to trickle.
    assert!(
        run.elapsed < reference.elapsed + Duration::from_millis(1000),
        "{:?} vs {:?}",
        run.elapsed,
        reference.elapsed
    );
}

/// An interrupted build (SIGINT while speculative downloads run) leaves a
/// store the next build completes from, with the sequential engine's
/// output.
#[test]
fn an_interrupted_build_leaves_a_consistent_store() {
    let server = Server::new(Duration::from_millis(400));
    let (ws, _) = fixture(&server);
    let reference = ws.cold(1, 0, &["build", "//:everything"]);
    assert!(reference.output.status.success());
    let _ = fs::remove_dir_all(ws.path().join("cache"));
    let _ = fs::remove_file(ws.path().join("MODULE.bazel.lock"));
    let mut child = ws
        .command(8, 0, &["build", "//:everything"])
        .spawn()
        .unwrap();
    // Wait until downloads are in flight, then interrupt.
    let deadline = Instant::now() + Duration::from_secs(60);
    while server.paths.lock().unwrap().len() < 3 {
        assert!(Instant::now() < deadline, "no download started");
        std::thread::sleep(Duration::from_millis(10));
    }
    // `timeout` forwards SIGINT to bazel-rs.
    let _ = Command::new("kill")
        .args(["-INT", &child.id().to_string()])
        .status();
    let status = child.wait().unwrap();
    assert!(!status.success(), "interrupted");
    let _ = server.take();
    let resumed = ws
        .command(8, 0, &["build", "//:everything"])
        .output()
        .unwrap();
    assert!(resumed.status.success(), "{}", stderr(&resumed));
    assert_eq!(resumed.stdout, reference.output.stdout);
    let after: PathBuf = ws.path().join("cache/repositories/direct/trees");
    assert_eq!(
        trees(&after).len(),
        reference.trees.len(),
        "every repository published once"
    );
}

/// What the analysis snapshot records (`BAZEL_RS_SNAPSHOT=record` with a
/// dump) is the sequential engine's: every observation (with its outcome;
/// stat keys and the racy flag depend on when files were written), every
/// environment read and every taint (in any order: the recorder is a set).
#[test]
fn the_snapshot_recording_is_the_sequential_engines() {
    let server = Server::new(Duration::from_millis(300));
    let (ws, _) = fixture(&server);
    let record = |threads: usize| {
        let _ = fs::remove_dir_all(ws.path().join("cache"));
        let _ = fs::remove_file(ws.path().join("MODULE.bazel.lock"));
        let dump = ws.path().join(format!("dump-{threads}.txt"));
        let output = ws
            .command(threads, 0, &["build", "//:everything"])
            .env("BAZEL_RS_SNAPSHOT", "record")
            .env("BAZEL_RS_SNAPSHOT_DUMP", &dump)
            .output()
            .unwrap();
        assert!(output.status.success(), "{}", stderr(&output));
        let text = fs::read_to_string(&dump).unwrap();
        let mut records: Vec<String> = text
            .lines()
            .filter(|line| {
                line.starts_with("OBS ") || line.starts_with("ENV ") || line.starts_with("TAINT ")
            })
            .map(|line| {
                line.split(' ')
                    .filter(|word| !word.starts_with("key:") && *word != "racy")
                    .collect::<Vec<_>>()
                    .join(" ")
            })
            // Execution times in the taint details.
            .map(|line| match line.find(" (") {
                Some(at) if line.starts_with("TAINT ") && line.ends_with(" ms)") => {
                    line[..at].to_owned()
                }
                _ => line,
            })
            .collect();
        records.sort();
        records
    };
    let sequential = record(1);
    assert!(
        sequential
            .iter()
            .any(|line| line.starts_with("TAINT network")),
        "{sequential:?}"
    );
    assert_eq!(record(8), sequential);
}
