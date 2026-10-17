//! MODULE.bazel.lock CLI behavior against loopback registries
//! (tests/fixtures/registry_lockfile; docs/REGISTRY_LOCKFILE.md). Never uses
//! a public registry or a reference Bazel executable.
#![allow(clippy::disallowed_methods)] // test harness: starts processes directly (clippy.toml)
use std::collections::BTreeMap;
use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::{Arc, Mutex};

const FIXTURE: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/tests/fixtures/registry_lockfile"
);

/// Serves `/r1/...` and `/r2/...` from two directories; logs request paths.
struct Server {
    port: u16,
    log: Arc<Mutex<Vec<String>>>,
}

impl Server {
    fn new(r1: PathBuf, r2: PathBuf) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let log = Arc::new(Mutex::new(Vec::new()));
        let shared = log.clone();
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { continue };
                let (log, r1, r2) = (shared.clone(), r1.clone(), r2.clone());
                std::thread::spawn(move || {
                    let mut reader = BufReader::new(stream.try_clone().unwrap());
                    loop {
                        let mut line = String::new();
                        if reader.read_line(&mut line).unwrap_or(0) == 0 {
                            return;
                        }
                        loop {
                            let mut header = String::new();
                            if reader.read_line(&mut header).unwrap_or(0) == 0 || header == "\r\n" {
                                break;
                            }
                        }
                        let path = line.split(' ').nth(1).unwrap_or("").to_owned();
                        log.lock().unwrap().push(path.clone());
                        let file = path
                            .strip_prefix("/r1/")
                            .map(|rest| r1.join(rest))
                            .or_else(|| path.strip_prefix("/r2/").map(|rest| r2.join(rest)));
                        let response = match file.and_then(|file| fs::read(file).ok()) {
                            Some(body) => {
                                let mut response = format!(
                                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n",
                                    body.len()
                                )
                                .into_bytes();
                                response.extend(body);
                                response
                            }
                            None => b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\n\r\n".to_vec(),
                        };
                        if stream.write_all(&response).is_err() {
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

struct Fixture {
    temp: tempfile::TempDir,
    server: Server,
}

fn copy_dir(from: &Path, to: &Path) {
    fs::create_dir_all(to).unwrap();
    for entry in fs::read_dir(from).unwrap() {
        let entry = entry.unwrap();
        let target = to.join(entry.file_name());
        if entry.file_type().unwrap().is_dir() {
            copy_dir(&entry.path(), &target);
        } else {
            fs::copy(entry.path(), target).unwrap();
        }
    }
}

impl Fixture {
    fn new() -> Self {
        let temp = tempfile::Builder::new()
            .prefix("native-registry-lockfile-")
            .tempdir_in("/tmp/opencode")
            .unwrap();
        let fixture = Path::new(FIXTURE);
        copy_dir(&fixture.join("registry_overlay"), &temp.path().join("r1"));
        copy_dir(&fixture.join("registry_extra"), &temp.path().join("r2"));
        copy_dir(&fixture.join("workspace"), &temp.path().join("ws"));
        fs::create_dir_all(temp.path().join("home")).unwrap();
        let server = Server::new(temp.path().join("r1"), temp.path().join("r2"));
        Self { temp, server }
    }

    fn ws(&self) -> PathBuf {
        self.temp.path().join("ws")
    }

    fn lock(&self) -> Option<String> {
        fs::read_to_string(self.ws().join("MODULE.bazel.lock")).ok()
    }

    fn query(&self, flags: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_bazel-rs"))
            .env_clear()
            .env("PATH", "/usr/bin:/bin")
            .env("HOME", self.temp.path().join("home"))
            .env("TMPDIR", self.temp.path())
            .arg("--ignore_all_rc_files")
            .arg(format!(
                "--cache-dir={}",
                self.temp.path().join("cache").display()
            ))
            .arg("query")
            .arg(format!(
                "--registry=http://127.0.0.1:{}/r1",
                self.server.port
            ))
            .arg(format!(
                "--registry=http://127.0.0.1:{}/r2",
                self.server.port
            ))
            .args(flags)
            .arg("//:all")
            .current_dir(self.ws())
            .output()
            .unwrap()
    }

    fn ok(&self, flags: &[&str]) -> Vec<String> {
        let output = self.query(flags);
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(String::from_utf8_lossy(&output.stdout), "//:files\n");
        self.server.take()
    }

    fn fails(&self, flags: &[&str], expected: &str) -> Vec<String> {
        let output = self.query(flags);
        assert!(!output.status.success());
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(stderr.contains(expected), "{stderr}");
        self.server.take()
    }
}

const ALLOW: &str = "--allow_yanked_versions=lockdemo_b@1.0";

fn hashes(lock: &str) -> BTreeMap<String, String> {
    let value: serde_json::Value = serde_json::from_str(lock).unwrap();
    value["registryFileHashes"]
        .as_object()
        .unwrap()
        .iter()
        .map(|(k, v)| (k.clone(), v.as_str().unwrap().to_owned()))
        .collect()
}

#[test]
fn update_writes_bazel_lockfile_then_serves_hashed_files_from_the_cache() {
    let f = Fixture::new();
    let first = f.ok(&[ALLOW]);
    let r = |path: &str| format!("http://127.0.0.1:{}/{path}", f.server.port);
    // Discovery, yanked metadata, repo specs; each URL once.
    assert_eq!(
        first,
        [
            "/r1/bazel_registry.json",
            "/r1/modules/lockdemo_a/1.0/MODULE.bazel",
            "/r1/modules/lockdemo_a/1.0/source.json",
            "/r1/modules/lockdemo_a/metadata.json",
            "/r1/modules/lockdemo_b/1.0/MODULE.bazel",
            "/r1/modules/lockdemo_b/1.0/source.json",
            "/r1/modules/lockdemo_b/metadata.json",
            "/r1/modules/lockdemo_c/1.0/MODULE.bazel",
            "/r2/bazel_registry.json",
            "/r2/modules/lockdemo_c/1.0/MODULE.bazel",
            "/r2/modules/lockdemo_c/1.0/source.json",
            "/r2/modules/lockdemo_c/metadata.json",
        ]
    );
    let lock = f.lock().unwrap();
    assert!(lock.starts_with("{\n  \"lockFileVersion\": 28,\n  \"registryFileHashes\": {\n"));
    assert!(lock.ends_with("  \"selectedYankedVersions\": {\n    \"lockdemo_b@1.0\": \"lockdemo yank: use 1.1\"\n  },\n  \"moduleExtensions\": {},\n  \"facts\": {},\n  \"factsVersions\": {}\n}\n"), "{lock}");
    let hashes = hashes(&lock);
    assert_eq!(hashes.len(), 9);
    assert_eq!(
        hashes[&r("r1/modules/lockdemo_c/1.0/MODULE.bazel")],
        "not found"
    );
    assert_eq!(hashes[&r("r1/bazel_registry.json")], "not found");
    assert_eq!(hashes[&r("r2/bazel_registry.json")], "not found");
    assert!(!hashes.keys().any(|url| url.ends_with("metadata.json")));
    // Warm: no request at all, and the unchanged value is not rewritten.
    let mtime = fs::metadata(f.ws().join("MODULE.bazel.lock"))
        .unwrap()
        .modified()
        .unwrap();
    assert!(f.ok(&[ALLOW]).is_empty());
    assert!(f.ok(&[ALLOW, "--lockfile_mode=error"]).is_empty());
    assert_eq!(
        fs::metadata(f.ws().join("MODULE.bazel.lock"))
            .unwrap()
            .modified()
            .unwrap(),
        mtime
    );
    // The lockfile's yanked record still needs the allowance.
    f.fails(&[], "Yanked version detected in your resolved dependency graph: lockdemo_b@1.0, for the reason: lockdemo yank: use 1.1.\nYanked versions may contain serious vulnerabilities");
    // Refresh rechecks mutable information only: metadata.json and files
    // recorded as not found.
    assert_eq!(
        f.ok(&[ALLOW, "--lockfile_mode=refresh"]),
        [
            "/r1/bazel_registry.json",
            "/r1/modules/lockdemo_a/metadata.json",
            "/r1/modules/lockdemo_b/metadata.json",
            "/r1/modules/lockdemo_c/1.0/MODULE.bazel",
            "/r2/bazel_registry.json",
            "/r2/modules/lockdemo_c/metadata.json",
        ]
    );
    assert_eq!(f.lock().unwrap(), lock);
    // Without the repository cache, known files are fetched and verified.
    fs::remove_dir_all(f.temp.path().join("cache")).unwrap();
    assert_eq!(f.ok(&[ALLOW]).len(), 6);
    // A registry file changed after locking.
    let module = f.temp.path().join("r1/modules/lockdemo_a/1.0/MODULE.bazel");
    fs::write(
        &module,
        "module(name = \"lockdemo_a\", version = \"1.0\")\n",
    )
    .unwrap();
    assert!(f.ok(&[ALLOW]).is_empty(), "cached by hash");
    fs::remove_dir_all(f.temp.path().join("cache")).unwrap();
    let wanted = &hashes[&r("r1/modules/lockdemo_a/1.0/MODULE.bazel")];
    f.fails(
        &[ALLOW],
        &format!(
            "Error accessing registry {}: Failed to fetch registry file {}: Checksum was {} but wanted {wanted}",
            r("r1"),
            r("r1/modules/lockdemo_a/1.0/MODULE.bazel"),
            sha256(&fs::read(&module).unwrap())
        ),
    );
    assert_eq!(f.lock().unwrap(), lock);
}

fn sha256(bytes: &[u8]) -> String {
    use sha2::Digest;
    sha2::Sha256::digest(bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

#[test]
fn error_and_off_modes_never_write_and_error_requires_hashes() {
    let f = Fixture::new();
    f.fails(
        &[ALLOW, "--lockfile_mode=error"],
        &format!(
            "bazel-rs: Missing checksum for registry file http://127.0.0.1:{}/r1/modules/lockdemo_a/1.0/MODULE.bazel not permitted with --lockfile_mode=error. Please run `bazel mod deps --lockfile_mode=update` to update your lockfile.",
            f.server.port
        ),
    );
    assert!(f.lock().is_none());
    // Off neither reads nor writes: an unparsable lockfile is ignored (and
    // the lazy native profile does not resolve for a main-repository query).
    fs::write(
        f.ws().join("MODULE.bazel.lock"),
        "{\"lockFileVersion\": 28,",
    )
    .unwrap();
    assert!(f.ok(&[ALLOW, "--lockfile_mode=off"]).is_empty());
    assert_eq!(f.lock().unwrap(), "{\"lockFileVersion\": 28,");
    f.fails(
        &[ALLOW],
        "Failed to read and parse the MODULE.bazel.lock file with error: ",
    );
    // An old version is ignored (and replaced) in update mode only.
    fs::write(
        f.ws().join("MODULE.bazel.lock"),
        "{\"lockFileVersion\": 26}\n",
    )
    .unwrap();
    f.fails(
        &[ALLOW, "--lockfile_mode=error"],
        "The version of MODULE.bazel.lock is not supported by this version of Bazel.",
    );
    f.ok(&[ALLOW]);
    assert!(
        f.lock()
            .unwrap()
            .starts_with("{\n  \"lockFileVersion\": 28,")
    );
    // Yanked without allowance and without lockfile information.
    fs::remove_file(f.ws().join("MODULE.bazel.lock")).unwrap();
    f.fails(
        &[],
        "Yanked version detected in your resolved dependency graph: lockdemo_b@1.0",
    );
    assert!(f.lock().is_none());
    f.ok(&["--allow_yanked_versions=all"]);
    f.fails(&["--allow_yanked_versions=lockdemo_b"], "Parsing command line flag --allow_yanked_versions=lockdemo_b failed, module versions must be of the form '<module name>@<version>'");
}

#[test]
fn native_lockfile_targets_keep_the_workspace_read_only() {
    let f = Fixture::new();
    let overlay = f.temp.path().join("private/MODULE.bazel.lock");
    fs::create_dir_all(overlay.parent().unwrap()).unwrap();
    let flag = format!("--native-lockfile={}", overlay.display());
    assert_eq!(f.ok(&[ALLOW, "--native-lockfile=readonly"]).len(), 12);
    assert!(f.lock().is_none());
    assert_eq!(f.ok(&[ALLOW, &flag]).len(), 12);
    assert!(f.lock().is_none());
    let private = fs::read_to_string(&overlay).unwrap();
    assert!(f.ok(&[ALLOW, &flag]).is_empty());
    assert!(f.ok(&[ALLOW, &flag, "--lockfile_mode=error"]).is_empty());
    assert_eq!(fs::read_to_string(&overlay).unwrap(), private);
    // The workspace lockfile seeds an absent overlay and is never written.
    fs::write(f.ws().join("MODULE.bazel.lock"), &private).unwrap();
    fs::remove_file(&overlay).unwrap();
    assert!(f.ok(&[ALLOW, &flag]).is_empty());
    assert!(!overlay.exists(), "an unchanged value is not written");
    f.fails(
        &["--native-lockfile=relative"],
        "--native-lockfile expects workspace, readonly or an absolute path",
    );
}

#[test]
fn extension_sections_are_kept_verbatim_but_pruned_by_usage() {
    let f = Fixture::new();
    let module = fs::read_to_string(f.ws().join("MODULE.bazel")).unwrap();
    fs::write(
        f.ws().join("MODULE.bazel"),
        format!("{module}ext = use_extension(\"//:ext.bzl\", \"ext\")\n"),
    )
    .unwrap();
    let entry = "{\n      \"general\": {\n        \"bzlTransitiveDigest\": \"x\",\n        \"generatedRepoSpecs\": {\n          \"r\": {\n            \"repoRuleId\": \"@@bazel_tools//tools/build_defs/repo:http.bzl%http_archive\",\n            \"attributes\": {\n              \"n\": 1.50\n            }\n          }\n        }\n      }\n    }";
    fs::write(
        f.ws().join("MODULE.bazel.lock"),
        format!(
            "{{\n  \"lockFileVersion\": 28,\n  \"moduleExtensions\": {{\n    \"//:ext.bzl%ext\": {entry},\n    \"//:gone.bzl%gone\": {entry}\n  }},\n  \"facts\": {{\n    \"//:ext.bzl%ext\": {{\n      \"k\": 1\n    }},\n    \"//:gone.bzl%gone\": {{\n      \"k\": 2\n    }}\n  }},\n  \"factsVersions\": {{\n    \"//:ext.bzl%ext\": 0,\n    \"//:gone.bzl%gone\": 4\n  }}\n}}\n"
        ),
    )
    .unwrap();
    f.ok(&[ALLOW]);
    let lock = f.lock().unwrap();
    assert!(
        lock.ends_with(&format!(
            "  \"moduleExtensions\": {{\n    \"//:ext.bzl%ext\": {entry}\n  }},\n  \"facts\": {{\n    \"//:ext.bzl%ext\": {{\n      \"k\": 1\n    }}\n  }},\n  \"factsVersions\": {{}}\n}}\n"
        )),
        "{lock}"
    );
    assert!(!lock.contains("gone"));
}
