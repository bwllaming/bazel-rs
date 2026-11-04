//! `--repository_cache` shared with genuine Bazel 9.3.1 (docs/REPOSITORY_CACHE.md):
//! genuine's on-disk layout, hits on genuine-written entries, a second fresh
//! bazel-rs cache that downloads nothing, and the repository cache options of a
//! home rc file. Every download comes from a private counting loopback server;
//! the paired capture is tests/fixtures/repository_cache/capture.py.
#![allow(clippy::disallowed_methods)] // test harness: starts processes directly (clippy.toml)
use serde_json::Value;
use sha2::{Digest, Sha256, Sha384};
use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

include!("support/verify_env.rs");
include!("support/temp_root.rs");
include!("support/sealed_trees.rs");

const FIXTURE: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/tests/fixtures/repository_cache"
);
const QUERY: &str = "@arch//:* + @blob//file:* + @dep//:*";
static NEXT: AtomicUsize = AtomicUsize::new(0);

fn receipt() -> Value {
    serde_json::from_str(&fs::read_to_string(format!("{FIXTURE}/reference_receipt.json")).unwrap())
        .unwrap()
}

/// Serves `<root>/…` over HTTP/1.1 and logs every request path.
struct Server {
    port: u16,
    log: Arc<Mutex<Vec<String>>>,
}

impl Server {
    fn new(root: PathBuf) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let log = Arc::new(Mutex::new(Vec::new()));
        let accept_log = log.clone();
        std::thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                let (root, log) = (root.clone(), accept_log.clone());
                std::thread::spawn(move || {
                    let mut stream = stream;
                    let mut reader = BufReader::new(stream.try_clone().unwrap());
                    loop {
                        let mut line = String::new();
                        if reader.read_line(&mut line).unwrap_or(0) == 0 {
                            return;
                        }
                        loop {
                            let mut header = String::new();
                            if reader.read_line(&mut header).unwrap_or(0) == 0
                                || header.trim_end().is_empty()
                            {
                                break;
                            }
                        }
                        let path = line.split_whitespace().nth(1).unwrap_or("/").to_owned();
                        log.lock().unwrap().push(path.clone());
                        let body = (!path.contains(".."))
                            .then(|| fs::read(root.join(path.trim_start_matches('/'))).ok())
                            .flatten();
                        let head = match &body {
                            Some(body) => {
                                format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n", body.len())
                            }
                            None => {
                                "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\n\r\n".to_owned()
                            }
                        };
                        if stream.write_all(head.as_bytes()).is_err()
                            || stream
                                .write_all(body.as_deref().unwrap_or_default())
                                .is_err()
                        {
                            return;
                        }
                    }
                });
            }
        });
        Self { port, log }
    }

    fn take(&self) -> Vec<String> {
        let mut log = std::mem::take(&mut *self.log.lock().unwrap());
        log.sort();
        log
    }
}

/// The fixture's templates with `@PORT@` replaced.
fn templated(source: &Path, destination: &Path, port: u16) {
    for entry in fs::read_dir(source).unwrap() {
        let entry = entry.unwrap();
        let to = destination.join(entry.file_name());
        if entry.file_type().unwrap().is_dir() {
            templated(&entry.path(), &to, port);
            continue;
        }
        fs::create_dir_all(destination).unwrap();
        let mut bytes = fs::read(entry.path()).unwrap();
        if !entry.file_name().to_string_lossy().ends_with(".gz") {
            bytes = String::from_utf8(bytes)
                .unwrap()
                .replace("@PORT@", &port.to_string())
                .into_bytes();
        }
        fs::write(to, bytes).unwrap();
    }
}

struct Fixture {
    root: PathBuf,
    server: Server,
}

impl Fixture {
    fn new() -> Self {
        let root = test_temp::root().join(format!(
            "native-repository-cache-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = sealed_trees::remove_tree(&root);
        let served = root.join("served");
        let server = Server::new(served.clone());
        for name in ["files", "registry"] {
            templated(
                &Path::new(FIXTURE).join(name),
                &served.join(name),
                server.port,
            );
        }
        fs::create_dir_all(root.join("home")).unwrap();
        Self { root, server }
    }

    fn workspace(&self, name: &str) -> PathBuf {
        let path = self.root.join(name);
        if !path.exists() {
            templated(
                &Path::new(FIXTURE).join("workspace"),
                &path,
                self.server.port,
            );
        }
        path
    }

    /// `bazel-rs ARGS` in workspace `ws` with cache directory `cache`.
    fn run(&self, ws: &str, cache: &str, rc: bool, args: &[&str]) -> Output {
        let mut command = Command::new(env!("CARGO_BIN_EXE_bazel-rs"));
        if !rc {
            command.arg("--ignore_all_rc_files");
        }
        command
            .arg(format!("--cache-dir={}", self.root.join(cache).display()))
            .args(args)
            .current_dir(self.workspace(ws))
            .env_clear()
            .envs(verifier_environment())
            .env("PATH", "/usr/bin:/bin")
            .env("HOME", self.root.join("home"))
            .output()
            .unwrap()
    }

    fn query(&self, ws: &str, cache: &str, extra: &[&str]) -> Output {
        let builtin = format!(
            "--registry=file://{}/tests/fixtures/builtin_modules/registry",
            env!("CARGO_MANIFEST_DIR")
        );
        let registry = format!("--registry=http://127.0.0.1:{}/registry", self.server.port);
        let mut args = vec!["query", QUERY, &builtin, &registry];
        args.extend_from_slice(extra);
        self.run(ws, cache, false, &args)
    }

    fn served(&self, name: &str) -> Vec<u8> {
        fs::read(self.root.join("served").join(name)).unwrap()
    }

    /// The canonical ids the receipt names, as `<PORT>` templates.
    fn id_template(&self, algorithm: &str, entry: &Path, name: &str) -> String {
        for file in ["arch.tar.gz", "blob.txt", "dep.tar.gz"] {
            let id = format!("http://127.0.0.1:{}/files/{file}", self.server.port);
            if name == format!("id-{}", hash(algorithm, id.as_bytes())) {
                return format!("id-<http://127.0.0.1:<PORT>/files/{file}>");
            }
        }
        panic!("unknown marker {name} in {}", entry.display())
    }

    /// The fixture entries of a repository cache, normalized like
    /// capture.py's `layout`.
    fn layout(&self, cache: &Path) -> Value {
        let mut known = std::collections::BTreeMap::new();
        for name in FILES {
            let data = self.served(name);
            known.insert(hash("sha256", &data), name);
            known.insert(hash("sha384", &data), name);
        }
        let root = cache.join("content_addressable");
        let mut entries = Vec::new();
        for algorithm in sorted(&root) {
            for key in sorted(&root.join(&algorithm)) {
                let Some(file) = known.get(&key) else {
                    continue;
                };
                let entry = root.join(&algorithm).join(&key);
                let names: Vec<String> = sorted(&entry)
                    .into_iter()
                    .map(|name| {
                        if name.starts_with("id-") {
                            self.id_template(&algorithm, &entry, &name)
                        } else {
                            name
                        }
                    })
                    .collect();
                entries.push(
                    serde_json::json!({"algorithm": algorithm, "file": file, "names": names}),
                );
            }
        }
        entries.sort_by_key(|entry| {
            (
                entry["file"].as_str().unwrap().to_owned(),
                entry["algorithm"].as_str().unwrap().to_owned(),
            )
        });
        Value::Array(entries)
    }

    /// Writes the entries genuine wrote (the receipt's layout) for this
    /// fixture's blobs and port.
    fn seed_genuine_layout(&self, cache: &Path) {
        for entry in receipt()["genuine_layout"].as_array().unwrap() {
            let algorithm = entry["algorithm"].as_str().unwrap();
            let file = entry["file"].as_str().unwrap();
            let data = self.served(file);
            let directory = cache
                .join("content_addressable")
                .join(algorithm)
                .join(hash(algorithm, &data));
            fs::create_dir_all(&directory).unwrap();
            for name in entry["names"].as_array().unwrap() {
                let name = name.as_str().unwrap();
                if name == "file" {
                    fs::write(directory.join("file"), &data).unwrap();
                } else {
                    let id = name
                        .strip_prefix("id-<")
                        .and_then(|rest| rest.strip_suffix('>'))
                        .unwrap()
                        .replace("<PORT>", &self.server.port.to_string());
                    fs::write(
                        directory.join(format!("id-{}", hash(algorithm, id.as_bytes()))),
                        b"",
                    )
                    .unwrap();
                }
            }
        }
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = sealed_trees::remove_tree(&self.root);
    }
}

const FILES: [&str; 6] = [
    "files/arch.tar.gz",
    "files/blob.txt",
    "files/dep.tar.gz",
    "registry/modules/dep/1.0/MODULE.bazel",
    "registry/modules/dep/1.0/source.json",
    "registry/bazel_registry.json",
];

fn hash(algorithm: &str, data: &[u8]) -> String {
    let digest = match algorithm {
        "sha256" => Sha256::digest(data).to_vec(),
        "sha384" => Sha384::digest(data).to_vec(),
        other => panic!("{other}"),
    };
    digest.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn sorted(directory: &Path) -> Vec<String> {
    let mut names: Vec<String> = fs::read_dir(directory)
        .map(|entries| {
            entries
                .map(|entry| entry.unwrap().file_name().into_string().unwrap())
                .collect()
        })
        .unwrap_or_default();
    names.sort();
    names
}

fn ok(output: &Output) -> Vec<String> {
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let mut lines: Vec<String> = String::from_utf8_lossy(&output.stdout)
        .split_whitespace()
        .map(str::to_owned)
        .collect();
    lines.sort();
    lines
}

fn strings(value: &Value) -> Vec<String> {
    value
        .as_array()
        .unwrap()
        .iter()
        .map(|value| value.as_str().unwrap().to_owned())
        .collect()
}

fn archives(requests: &[String]) -> Vec<String> {
    requests
        .iter()
        .filter(|path| path.starts_with("/files/"))
        .cloned()
        .collect()
}

#[test]
fn a_second_fresh_cache_downloads_nothing_and_entries_have_genuines_layout() {
    let receipt = receipt();
    let fixture = Fixture::new();
    let shared = fixture.root.join("shared");
    let flag = format!("--repository_cache={}", shared.display());
    // Cold: every archive is downloaded once (genuine's step 4 equivalent).
    let stdout = ok(&fixture.query("ws", "c1", &[&flag]));
    let cold = fixture.server.take();
    assert_eq!(
        stdout,
        strings(&receipt["steps"]["4_native_cold"]["stdout"])
    );
    assert_eq!(
        archives(&cold),
        archives(&strings(&receipt["steps"]["1_genuine_cold"]["requests"]))
    );
    // Genuine's layout: content_addressable/<algo>/<hex>/file + id markers.
    assert_eq!(fixture.layout(&shared), receipt["genuine_layout"]);
    // A fresh native cache (same workspace and lockfile) downloads nothing,
    // registry files included, like a second genuine output base (step 2).
    let stdout = ok(&fixture.query("ws", "c2", &[&flag]));
    assert_eq!(
        stdout,
        strings(&receipt["steps"]["4_native_cold"]["stdout"])
    );
    assert_eq!(
        fixture.server.take(),
        strings(&receipt["steps"]["2_genuine_fresh_output_root_shared"]["requests"])
    );
    // Without the option a fresh cache downloads again (no shared default).
    ok(&fixture.query("ws", "c3", &[]));
    assert_eq!(archives(&fixture.server.take()), archives(&cold));
    // The empty value disables the shared cache.
    ok(&fixture.query("ws", "c4", &["--repository_cache="]));
    assert_eq!(archives(&fixture.server.take()), archives(&cold));
}

#[test]
fn entries_genuine_wrote_are_hits() {
    let receipt = receipt();
    let fixture = Fixture::new();
    let shared = fixture.root.join("genuine");
    fixture.seed_genuine_layout(&shared);
    let flag = format!("--repository_cache={}", shared.display());
    let stdout = ok(&fixture.query("ws", "c1", &[&flag]));
    assert_eq!(
        stdout,
        strings(&receipt["steps"]["4_native_cold"]["stdout"])
    );
    // Like step 3 of the capture: registry metadata of a workspace without a
    // lockfile is fetched, no archive is.
    let requests = fixture.server.take();
    assert_eq!(
        requests,
        strings(&receipt["steps"]["3_native_fresh_cache_on_genuine_written"]["requests"])
    );
    assert!(archives(&requests).is_empty(), "{requests:?}");
}

#[test]
fn a_corrupt_or_foreign_id_entry_is_downloaded_again() {
    let fixture = Fixture::new();
    let shared = fixture.root.join("genuine");
    fixture.seed_genuine_layout(&shared);
    let data = fixture.served("files/arch.tar.gz");
    let entry = shared
        .join("content_addressable/sha256")
        .join(hash("sha256", &data));
    // A corrupt blob is a miss (genuine: "Please delete the directory …",
    // caught by DownloadManager), repaired by the download's put.
    fs::write(entry.join("file"), b"corrupt").unwrap();
    let blob = fixture.served("files/blob.txt");
    let blob_entry = shared
        .join("content_addressable/sha384")
        .join(hash("sha384", &blob));
    // Another canonical id: a miss for this http_file.
    for marker in sorted(&blob_entry) {
        if marker.starts_with("id-") {
            fs::remove_file(blob_entry.join(marker)).unwrap();
        }
    }
    let flag = format!("--repository_cache={}", shared.display());
    ok(&fixture.query("ws", "c1", &[&flag]));
    assert_eq!(
        archives(&fixture.server.take()),
        ["/files/arch.tar.gz", "/files/blob.txt"]
    );
    assert_eq!(fs::read(entry.join("file")).unwrap(), data);
    assert_eq!(fixture.layout(&shared), receipt()["genuine_layout"]);
}

#[test]
fn home_rc_repository_cache_options_are_honoured_by_every_command() {
    let fixture = Fixture::new();
    let shared = fixture.root.join("home/.cache/bazel-repo");
    let disk = fixture.root.join("home/.cache/bazel-disk");
    let builtin = format!(
        "file://{}/tests/fixtures/builtin_modules/registry",
        env!("CARGO_MANIFEST_DIR")
    );
    // A CI runner's home rc (~/code's self-hosted runners).
    fs::write(
        fixture.root.join("home/.bazelrc"),
        format!(
            "common --experimental_repository_cache_hardlinks\n\
             common --repository_cache={}\n\
             common --repo_contents_cache=\n\
             common --experimental_repository_downloader_retries=3\n\
             common --disk_cache={}\n\
             common --experimental_disk_cache_gc_max_size=1G\n\
             common --experimental_disk_cache_gc_max_age=7d\n\
             common --registry={builtin}\n\
             common --registry=http://127.0.0.1:{}/registry\n",
            shared.display(),
            disk.display(),
            fixture.server.port
        ),
    )
    .unwrap();
    let output = fixture.run("ws", "c1", true, &["query", QUERY]);
    ok(&output);
    assert_eq!(archives(&fixture.server.take()).len(), 3);
    assert!(fixture.layout(&shared).as_array().unwrap().len() >= 3);
    let output = fixture.run("ws", "c2", true, &["info", "repository_cache"]);
    assert_eq!(ok(&output), [shared.display().to_string()]);
    let output = fixture.run("ws", "c2", true, &["query", QUERY]);
    ok(&output);
    assert!(fixture.server.take().is_empty());
    for args in [
        &["build", "//..."][..],
        &["fetch", "//..."],
        &["mod", "graph"],
        &["clean"],
    ] {
        let output = fixture.run("ws", "c2", true, args);
        assert!(
            output.status.success(),
            "{args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
}

#[test]
fn repo_contents_cache_locations_are_checked_like_genuine() {
    let fixture = Fixture::new();
    let ws = fixture.workspace("ws");
    let inside = format!("--repo_contents_cache={}", ws.join("contents").display());
    let output = fixture.run("ws", "c1", false, &["query", "//...", &inside]);
    assert_eq!(output.status.code(), Some(2));
    assert!(
        String::from_utf8_lossy(&output.stderr).contains(&format!(
            "The repo contents cache [{}] is inside the main repo [{}]. This can cause spurious failures. Disable the repo contents cache with `--repo_contents_cache=`, or specify `--repo_contents_cache=<path outside the main repo>`.",
            ws.join("contents").display(),
            ws.display()
        )),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    // A repository cache inside the workspace puts its default contents
    // directory there too; `--repo_contents_cache=` disables it.
    let repository = format!("--repository_cache={}", ws.join("rc").display());
    let output = fixture.run("ws", "c1", false, &["query", "//...", &repository]);
    assert_eq!(output.status.code(), Some(2));
    ok(&fixture.query("ws", "c1", &[&repository, "--repo_contents_cache="]));
    // An output base inside the contents cache.
    let outer = format!("--repo_contents_cache={}", fixture.root.display());
    let output = fixture.run("ws", "c1", false, &["query", "//...", &outer]);
    assert_eq!(output.status.code(), Some(2));
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("is inside the repo contents cache"),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}
