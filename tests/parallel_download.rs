//! Concurrent `download(block = False)` (docs/MODULE_EXTENSIONS.md,
//! "Non-blocking downloads"; crate::repository_ctx's pending downloads): a
//! module extension that issues many non-blocking downloads from a loopback
//! fixture that answers late, then waits for them, overlaps the transfers,
//! while nothing it can observe changes: cold builds are byte-identical in
//! exit code, stdout, stderr (download failure warnings interleaved with
//! prints, in call order), lockfile and published trees to the sequential
//! transfer (`BAZEL_RS_DEBUG_NO_ASYNC_DOWNLOAD`) at every
//! `--loading_phase_threads`, including when the extension fails.

#![allow(clippy::disallowed_methods)] // test harness: starts processes directly (clippy.toml)
use std::collections::BTreeMap;
use std::fs;
use std::io::{Read, Write};
use std::net::TcpListener;
use std::path::Path;
use std::process::{Command, Output};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
include!("support/explicit_modules.rs");
include!("support/console.rs");
include!("support/sealed_trees.rs");

/// A loopback HTTP server answering `GET /<name>` with `payload <name>`
/// after `delay` (`/missing/...` with a 404), one thread per connection; it
/// records the paths requested and the most requests in flight at once.
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
                let (logged, peaked, current) = (logged.clone(), peaked.clone(), current.clone());
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
                    let response = if path.starts_with("/missing/") {
                        "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                            .to_owned()
                    } else {
                        let body = format!("payload {}\n", path.trim_start_matches('/'));
                        format!(
                            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                            body.len()
                        )
                    };
                    let _ = stream.write_all(response.as_bytes());
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

    /// The paths requested since the last call (sorted), and the peak
    /// concurrency (both reset).
    fn take(&self) -> (Vec<String>, usize) {
        let mut paths = std::mem::take(&mut *self.paths.lock().unwrap());
        paths.sort();
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

fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::Digest as _;
    sha2::Sha256::digest(bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

/// The extension: twelve non-blocking downloads (two missing, one to a
/// nested output whose parent another pending download's output becomes
/// the directory of, two expecting the same checksum, one never waited for)
/// with prints, `path.exists`, reads and `file()` writes in between (one
/// beside each of the first eight, as bazel-gazelle's go_deps writes, one
/// onto a pending output), then one repository
/// whose BUILD lists what it saw.
const DEFS: &str = r#"
def _repo_impl(ctx):
    ctx.file("seen.txt", ctx.attr.seen)
    ctx.file("BUILD.bazel", "exports_files(['seen.txt'])\nfilegroup(name = 'data', srcs = ['seen.txt'], visibility = ['//visibility:public'])\n")

repo = repository_rule(implementation = _repo_impl, attrs = {"seen": attr.string()})

def _ext_impl(ctx):
    tag = ctx.modules[0].tags.server[0]
    url = tag.url
    sha = tag.shas
    pending = {}
    for index in range(8):
        name = "p%d" % index
        pending[name] = ctx.download(url + "/" + name, "out/" + name + ".txt", sha256 = sha[name], block = False)
        ctx.file("out/" + name + ".info", "{}")
        if index == 3:
            print("issued four, out/p0.txt exists: %s" % ctx.path("out/p0.txt").exists)
    pending["m0"] = ctx.download(url + "/missing/m0", "out/m0.txt", sha256 = sha["m0"], block = False, allow_fail = True)
    print("issued the first missing one")
    pending["m1"] = ctx.download([url + "/missing/m1", url + "/m1"], "out/m1.txt", sha256 = sha["m1"], block = False)
    pending["s0"] = ctx.download(url + "/s0", "same0.txt", sha256 = sha["s0"], block = False, allow_fail = True)
    pending["s1"] = ctx.download(url + "/s0", "same1.txt", sha256 = sha["s0"], block = False)
    ctx.download(url + "/never-waited", "never.txt", sha256 = sha["never-waited"], block = False)
    ctx.file("same1.txt", "written over the pending same1.txt\n")
    if tag.fail:
        fail("failing with pending downloads")
    seen = []
    for name in sorted(pending):
        result = pending[name].wait()
        seen.append("%s success=%s" % (name, result.success))
        if result.success:
            seen.append(ctx.read(repr(pending[name]).split("'")[1]).strip())
    seen.append("never: " + ctx.read("never.txt").strip())
    repo(name = "seen", seen = "\n".join(seen) + "\n")

ext = module_extension(
    implementation = _ext_impl,
    tag_classes = {"server": tag_class(attrs = {"url": attr.string(), "shas": attr.string_dict(), "fail": attr.bool()})},
)
"#;

struct Workspace(tempfile::TempDir);

impl Workspace {
    fn new(server: &Server, fail: bool) -> Self {
        let ws = Self(
            tempfile::Builder::new()
                .prefix("bazel-rs-parallel-download-")
                .tempdir_in(crate::test_temp::root())
                .unwrap(),
        );
        let names = [
            "p0",
            "p1",
            "p2",
            "p3",
            "p4",
            "p5",
            "p6",
            "p7",
            "m0",
            "m1",
            "s0",
            "never-waited",
        ];
        let shas: Vec<String> = names
            .iter()
            .map(|name| {
                format!(
                    "'{name}': '{}'",
                    sha256_hex(format!("payload {name}\n").as_bytes())
                )
            })
            .collect();
        ws.file(
            "MODULE.bazel",
            &format!(
                "module(name = 'main')\next = use_extension('//:defs.bzl', 'ext')\next.server(url = '{}', shas = {{{}}}, fail = {})\nuse_repo(ext, 'seen')\n",
                server.url,
                shas.join(", "),
                if fail { "True" } else { "False" }
            ),
        );
        ws.file("defs.bzl", DEFS);
        ws.file(
            "BUILD.bazel",
            "filegroup(name = 'everything', srcs = ['@seen//:data'])\n",
        );
        ws
    }

    fn path(&self) -> &Path {
        self.0.path()
    }

    fn file(&self, name: &str, content: &str) {
        let path = self.path().join(name);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, content).unwrap();
    }

    /// A cold build (no cache, no lockfile).
    fn cold(&self, threads: usize, sequential: bool) -> Run {
        let _ = sealed_trees::remove_tree(self.path().join("cache"));
        let _ = fs::remove_file(self.path().join("MODULE.bazel.lock"));
        let mut command = Command::new("timeout");
        command
            .args([
                "600s",
                env!("CARGO_BIN_EXE_bazel-rs"),
                "--ignore_all_rc_files",
                "--cache-dir=cache",
                "build",
            ])
            .arg(format!("--loading_phase_threads={threads}"))
            .arg("//:everything")
            .env_remove("BAZEL_RS_TIMING")
            .env_remove("BAZEL_RS_LOADING_JITTER")
            .env_remove("BAZEL_RS_DEBUG_NO_ASYNC_DOWNLOAD")
            .current_dir(self.path());
        if sequential {
            command.env("BAZEL_RS_DEBUG_NO_ASYNC_DOWNLOAD", "1");
        }
        command.explicit_modules();
        let started = Instant::now();
        let output = command.output().unwrap();
        let seen = fs::read_to_string(
            self.path()
                .join("cache/repositories/external/+ext+seen/seen.txt"),
        )
        .ok();
        Run {
            elapsed: started.elapsed(),
            lockfile: fs::read(self.path().join("MODULE.bazel.lock")).ok(),
            trees: trees(&self.path().join("cache/repositories/direct/trees")),
            seen,
            output,
        }
    }
}

struct Run {
    output: Output,
    lockfile: Option<Vec<u8>>,
    trees: BTreeMap<String, BTreeMap<String, Vec<u8>>>,
    seen: Option<String>,
    elapsed: Duration,
}

fn stderr(output: &Output) -> String {
    without_elapsed(&String::from_utf8_lossy(&output.stderr))
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

fn assert_same(reference: &Run, other: &Run, what: &str) {
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
    assert_eq!(other.trees, reference.trees, "{what}: published trees");
    assert_eq!(other.seen, reference.seen, "{what}: what the extension saw");
}

#[test]
fn non_blocking_downloads_overlap_and_change_nothing_observable() {
    let server = Server::new(Duration::from_millis(300));
    let ws = Workspace::new(&server, false);
    let reference = ws.cold(1, true);
    assert!(
        reference.output.status.success(),
        "{}",
        stderr(&reference.output)
    );
    let (reference_paths, peak) = server.take();
    assert_eq!(peak, 1, "the sequential transfer fetches one at a time");
    let text = stderr(&reference.output);
    // Warnings and prints in call order.
    let order = [
        "issued four, out/p0.txt exists: True",
        "WARNING: Download from",
        "/missing/m0 failed",
        "issued the first missing one",
        "/missing/m1 failed",
    ];
    let mut at = 0;
    for needle in order {
        let found = text[at..]
            .find(needle)
            .unwrap_or_else(|| panic!("{needle} after {at}:\n{text}"));
        at += found + needle.len();
    }
    let seen = reference
        .seen
        .clone()
        .expect("the repository saw the downloads");
    assert!(seen.contains("m0 success=False"), "{seen}");
    assert!(seen.contains("m1 success=True\npayload m1"), "{seen}");
    assert!(seen.contains("p7 success=True\npayload p7"), "{seen}");
    assert!(seen.contains("never: payload never-waited"), "{seen}");
    for threads in [1, 8, 32] {
        let what = format!("threads={threads}");
        let run = ws.cold(threads, false);
        assert_same(&reference, &run, &what);
        let (paths, peak) = server.take();
        // The second same-checksum download is served by the download
        // cache, as in the sequential transfer.
        assert_eq!(paths, reference_paths, "{what}");
        assert!(peak > 1, "{what}: downloads overlap (peak {peak})");
        assert!(
            run.elapsed < reference.elapsed,
            "{what}: {:?} vs sequential {:?}",
            run.elapsed,
            reference.elapsed
        );
    }
}

#[test]
fn a_failing_extension_shows_its_pending_downloads_warnings_first() {
    let server = Server::new(Duration::from_millis(100));
    let ws = Workspace::new(&server, true);
    let reference = ws.cold(1, true);
    assert!(!reference.output.status.success());
    let text = stderr(&reference.output);
    let warning = text.find("/missing/m1 failed").expect(&text);
    let failure = text.find("failing with pending downloads").expect(&text);
    assert!(warning < failure, "{text}");
    for threads in [1, 8] {
        let run = ws.cold(threads, false);
        assert_same(&reference, &run, &format!("threads={threads}"));
    }
    // No private transfer file is left behind.
    let leftovers: Vec<_> = walkdir(&ws.path().join("cache"))
        .into_iter()
        .filter(|path| path.contains(".bazel-rs-download-"))
        .collect();
    assert!(leftovers.is_empty(), "{leftovers:?}");
}

fn walkdir(root: &Path) -> Vec<String> {
    let mut found = Vec::new();
    let Ok(entries) = fs::read_dir(root) else {
        return found;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        found.push(path.display().to_string());
        if entry.file_type().is_ok_and(|kind| kind.is_dir()) {
            found.extend(walkdir(&path));
        }
    }
    found
}
