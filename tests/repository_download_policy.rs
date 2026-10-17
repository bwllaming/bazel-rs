//! Genuine archive/registry inputs, private counted HTTP, and the Cargo-built CLI.
//! Download policy is not a registry-metadata, Git, or general network prohibition.
#![allow(clippy::disallowed_methods)] // test harness: starts processes directly (clippy.toml)
use base64::Engine;
use bazel_rs::{materializer, patch_inputs::LocalPatchPlan, registry::Registry};
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::fs;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener};
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Duration;

const ARCHIVE: &[u8] = include_bytes!("fixtures/build_settings/bazel-skylib-1.8.2.tar.gz");
const MODULE: &[u8] =
    include_bytes!("fixtures/builtin_modules/registry/modules/bazel_skylib/1.8.2/MODULE.bazel");
const DESCRIPTOR: &[u8] =
    include_bytes!("fixtures/builtin_modules/registry/modules/bazel_skylib/1.8.2/source.json");
const OVERLAY: &[u8] = b"owned download-policy overlay\n";
const PATCH: &[u8] = b"--- a/version.bzl\n+++ b/version.bzl\n@@ -17,1 +17,1 @@\n-version = \"1.8.2\"\n+version = \"1.8.2-policy\"\n";
const DISABLED: &str = "--repository_disable_download";
const ENTRY: &str = "modules/bazel_skylib/1.8.2";

struct Server {
    address: SocketAddr,
    counts: Arc<Mutex<BTreeMap<String, usize>>>,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl Server {
    fn new(root: PathBuf) -> Self {
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let address = listener.local_addr().unwrap();
        listener.set_nonblocking(true).unwrap();
        let counts = Arc::new(Mutex::new(BTreeMap::<String, usize>::new()));
        let stop = Arc::new(AtomicBool::new(false));
        let thread_counts = counts.clone();
        let thread_stop = stop.clone();
        let thread = thread::spawn(move || {
            while !thread_stop.load(Ordering::SeqCst) {
                let (mut stream, _) = match listener.accept() {
                    Ok(connection) => connection,
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(2));
                        continue;
                    }
                    Err(error) => panic!("private listener failed: {error}"),
                };
                stream
                    .set_read_timeout(Some(Duration::from_secs(2)))
                    .unwrap();
                stream
                    .set_write_timeout(Some(Duration::from_secs(2)))
                    .unwrap();
                let mut request = Vec::new();
                while !request.ends_with(b"\r\n\r\n") && request.len() < 8192 {
                    let mut byte = [0];
                    if stream.read_exact(&mut byte).is_err() {
                        break;
                    }
                    request.push(byte[0]);
                }
                let request = String::from_utf8(request).unwrap();
                let path = request.split_whitespace().nth(1).unwrap_or("/");
                *thread_counts
                    .lock()
                    .unwrap()
                    .entry(path.to_owned())
                    .or_default() += 1;
                assert!(path.starts_with('/') && !path.contains(".."));
                let bytes = fs::read(root.join(&path[1..])).ok();
                let status = if bytes.is_some() {
                    "200 OK"
                } else {
                    "404 Not Found"
                };
                let bytes = bytes.unwrap_or_default();
                let header = format!(
                    "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    bytes.len()
                );
                // A timed-out client may close while cleanup is in progress.
                let _ = stream.write_all(header.as_bytes());
                let _ = stream.write_all(&bytes);
            }
        });
        Self {
            address,
            counts,
            stop,
            thread: Some(thread),
        }
    }

    fn url(&self) -> String {
        format!("http://{}", self.address)
    }

    fn count(&self, path: &str) -> usize {
        self.counts.lock().unwrap().get(path).copied().unwrap_or(0)
    }

    fn acquisitions(&self) -> usize {
        self.counts
            .lock()
            .unwrap()
            .iter()
            .filter(|(path, _)| {
                path.as_str() == "/source.tar.gz"
                    || path.contains("/overlay/")
                    || path.contains("/patches/")
            })
            .map(|(_, count)| count)
            .sum()
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(thread) = self.thread.take() {
            thread.join().unwrap();
        }
    }
}

struct Fixture {
    // Close and join the listener before TempDir removes its owned files.
    server: Server,
    temp: tempfile::TempDir,
    source: Map<String, Value>,
}

fn write(root: &Path, relative: &str, bytes: impl AsRef<[u8]>) {
    let path = root.join(relative);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, bytes).unwrap();
}

fn sri(bytes: &[u8]) -> String {
    format!(
        "sha256-{}",
        base64::engine::general_purpose::STANDARD.encode(Sha256::digest(bytes))
    )
}

impl Fixture {
    fn new(file_source: bool, transforms: bool) -> Self {
        let temp = tempfile::Builder::new()
            .prefix("repository-download-policy-")
            .tempdir()
            .unwrap();
        let served = temp.path().join("served");
        fs::create_dir(&served).unwrap();
        let server = Server::new(served.clone());
        write(&served, "source.tar.gz", ARCHIVE);
        write(&served, "bazel_registry.json", b"{}");
        for (name, version) in [
            ("bazel_skylib", "1.8.2"),
            ("platforms", "1.0.0"),
            ("rules_license", "1.0.0"),
        ] {
            let relative = format!("modules/{name}/{version}/MODULE.bazel");
            let tracked = Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("tests/fixtures/builtin_modules/registry")
                .join(&relative);
            write(&served, &relative, fs::read(tracked).unwrap());
        }
        let mut source: Map<String, Value> = serde_json::from_slice(DESCRIPTOR).unwrap();
        let url = if file_source {
            reqwest::Url::from_file_path(served.join("source.tar.gz"))
                .unwrap()
                .to_string()
        } else {
            format!("{}/source.tar.gz", server.url())
        };
        source.insert("url".into(), json!(url));
        if transforms {
            source.insert("overlay".into(), json!({"policy.txt": sri(OVERLAY)}));
            source.insert("patches".into(), json!({"version.patch": sri(PATCH)}));
            source.insert("patch_strip".into(), json!(1));
            write(&served, &format!("{ENTRY}/overlay/policy.txt"), OVERLAY);
            write(&served, &format!("{ENTRY}/patches/version.patch"), PATCH);
        }
        write(
            &served,
            &format!("{ENTRY}/source.json"),
            serde_json::to_vec(&source).unwrap(),
        );
        write(
            temp.path(),
            "MODULE.bazel",
            "module(name='main')\nbazel_dep(name='bazel_skylib',version='1.8.2')\nsingle_version_override(module_name='platforms',version='1.0.0')\nsingle_version_override(module_name='rules_license',version='1.0.0')\n",
        );
        write(
            temp.path(),
            "BUILD.bazel",
            "load('@bazel_skylib//:version.bzl', 'version')\ngenrule(name='version',outs=['version.txt'],cmd='printf %s '+version+' > $@')\n",
        );
        fs::create_dir(temp.path().join("home")).unwrap();
        Self {
            server,
            temp,
            source,
        }
    }

    fn cache(&self) -> PathBuf {
        self.temp.path().join("cache/repositories")
    }

    fn registry(&self) -> Registry {
        Registry::parse(&self.server.url()).unwrap()
    }

    fn materialize(&self, enabled: bool) -> Result<PathBuf, String> {
        materializer::materialize_with_download_policy(
            &self.registry(),
            "bazel_skylib",
            "1.8.2",
            MODULE,
            &self.source,
            &self.cache(),
            &LocalPatchPlan::default(),
            enabled,
        )
    }

    fn blob(&self, bytes: &[u8]) -> PathBuf {
        self.cache()
            .join("downloads/sha256")
            .join(format!("{:x}", Sha256::digest(bytes)))
    }

    fn seed(&self, bytes: &[u8]) {
        let path = self.blob(bytes);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, bytes).unwrap();
    }

    fn run(&self, command: &str, flags: &[&str], cache: &str) -> Output {
        let mut args = vec![command.to_owned()];
        if command == "mod" {
            args.extend(["graph".into(), "--output=json".into()]);
        } else {
            args.push("//:version".into());
        }
        args.extend([
            format!("--registry={}", self.server.url()),
            "--lockfile_mode=off".into(),
        ]);
        args.extend(flags.iter().map(|flag| (*flag).to_owned()));
        self.invoke(&args, cache)
    }

    fn invoke(&self, args: &[String], cache: &str) -> Output {
        Command::new("timeout")
            .env_clear()
            .env("PATH", "/usr/bin:/bin")
            .env("HOME", self.temp.path().join("home"))
            .env("XDG_CONFIG_HOME", self.temp.path().join("home"))
            .env("TMPDIR", self.temp.path())
            .env("LANG", "C")
            .args([
                "--kill-after=5",
                "30",
                env!("CARGO_BIN_EXE_bazel-rs"),
                "--ignore_all_rc_files",
            ])
            .arg(format!("--cache-dir={cache}"))
            .args(args)
            .current_dir(self.temp.path())
            .output()
            .unwrap()
    }

    fn graph_only(&self) {
        write(
            self.temp.path(),
            "MODULE.bazel",
            "module(name='main')\nbazel_dep(name='rules_license',version='1.0.0')\n",
        );
    }
}

fn success(output: &Output) {
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

fn rejected(output: &Output, expected: &str) {
    assert_eq!(
        output.status.code(),
        Some(1),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(output.stdout.is_empty());
    assert!(
        String::from_utf8_lossy(&output.stderr).contains(expected),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn cli_boolean_forms_defaults_aliases_and_repetition_all_commands() {
    let mut cases: Vec<(Vec<String>, bool)> = vec![(vec![], true)];
    for name in [
        "repository_disable_download",
        "experimental_repository_disable_download",
    ] {
        cases.push((vec![format!("--{name}")], false));
        cases.push((vec![format!("--no{name}")], true));
        for (values, enabled) in [
            (["true", "1", "YES", "t", "Y"], false),
            (["false", "0", "NO", "f", "N"], true),
        ] {
            for value in values {
                cases.push((vec![format!("--{name}={value}")], enabled));
            }
        }
    }
    cases.extend([
        (
            vec![
                DISABLED.into(),
                "--experimental_repository_disable_download=false".into(),
            ],
            true,
        ),
        (
            vec![
                "--repository_disable_download=false".into(),
                "--experimental_repository_disable_download".into(),
            ],
            false,
        ),
        (
            vec![
                DISABLED.into(),
                "--noexperimental_repository_disable_download".into(),
            ],
            true,
        ),
        (
            vec!["--norepository_disable_download".into(), DISABLED.into()],
            false,
        ),
    ]);
    for command in ["query", "build", "mod"] {
        let fixture = Fixture::new(false, false);
        if command == "mod" {
            fixture.graph_only();
        }
        for (index, (flags, enabled)) in cases.iter().enumerate() {
            let flags: Vec<_> = flags.iter().map(String::as_str).collect();
            let before = fixture.server.acquisitions();
            let output = fixture.run(command, &flags, &format!("cache-{index}"));
            if *enabled || command == "mod" {
                success(&output);
                if command == "build" {
                    let text = String::from_utf8(output.stdout).unwrap();
                    let (_, path) = text.trim().split_once('\t').unwrap();
                    assert_eq!(fs::read(path).unwrap(), b"1.8.2");
                }
            } else {
                rejected(&output, DISABLED);
            }
            assert_eq!(
                fixture.server.acquisitions() - before,
                usize::from(*enabled && command != "mod"),
                "{command} {flags:?}"
            );
        }
    }
}

#[test]
fn malformed_boolean_options_fail_before_any_metadata_or_source_execution() {
    let fixture = Fixture::new(false, false);
    for command in ["query", "build", "mod"] {
        for name in [
            "repository_disable_download",
            "experimental_repository_disable_download",
        ] {
            for suffix in [
                "=",
                "=maybe",
                "=2",
                "=on",
                "=off",
                "= true",
                "=false ",
                "=true=false",
            ] {
                rejected(
                    &fixture.run(command, &[&format!("--{name}{suffix}")], "cache"),
                    "expected a boolean",
                );
            }
            for suffix in ["=", "=true", "=false", "=0", "=YES"] {
                rejected(
                    &fixture.run(command, &[&format!("--no{name}{suffix}")], "cache"),
                    "Unexpected value after boolean option",
                );
            }
        }
    }
    assert!(fixture.server.counts.lock().unwrap().is_empty());
    assert!(!fixture.cache().exists());
}

#[test]
fn split_boolean_value_is_not_consumed_as_an_option_value() {
    let fixture = Fixture::new(false, false);
    write(fixture.temp.path(), "MODULE.bazel", "module(name='main')\n");
    write(
        fixture.temp.path(),
        "BUILD.bazel",
        "filegroup(name='false')\n",
    );
    for command in ["query", "build"] {
        let output = fixture.invoke(&[command.into(), DISABLED.into(), "false".into()], "cache");
        success(&output);
        if command == "query" {
            assert_eq!(output.stdout, b"//:false\n");
        }
    }
    rejected(
        &fixture.run("mod", &[DISABLED, "false"], "cache"),
        "argument: false",
    );
    assert!(fixture.server.counts.lock().unwrap().is_empty());
}

#[test]
fn cold_http_and_file_archives_are_blocked_while_metadata_remains_allowed() {
    for file in [false, true] {
        let fixture = Fixture::new(file, true);
        for command in ["query", "build"] {
            rejected(&fixture.run(command, &[DISABLED], "cache"), DISABLED);
            assert_eq!(fixture.server.acquisitions(), 0);
            assert!(fixture.server.count(&format!("/{ENTRY}/MODULE.bazel")) > 0);
            assert!(fixture.server.count(&format!("/{ENTRY}/source.json")) > 0);
            assert!(fixture.server.count("/bazel_registry.json") > 0);
            assert!(
                fs::read_dir(fixture.cache().join("trees"))
                    .unwrap()
                    .next()
                    .is_none()
            );
            assert!(
                fs::read_dir(fixture.cache().join("downloads/sha256"))
                    .unwrap()
                    .next()
                    .is_none()
            );
        }
        assert_eq!(
            fs::read(fixture.temp.path().join("served/source.tar.gz")).unwrap(),
            ARCHIVE
        );
    }
}

#[test]
fn enabled_default_acquires_real_archive_overlay_and_patch_once() {
    let fixture = Fixture::new(false, true);
    let output = fixture.run("build", &[], "cache");
    success(&output);
    let text = String::from_utf8(output.stdout).unwrap();
    let (_, path) = text.trim().split_once('\t').unwrap();
    assert_eq!(fs::read(path).unwrap(), b"1.8.2-policy");
    for path in [
        "/source.tar.gz".to_owned(),
        format!("/{ENTRY}/overlay/policy.txt"),
        format!("/{ENTRY}/patches/version.patch"),
    ] {
        assert_eq!(fixture.server.count(&path), 1);
    }
    success(&fixture.run("build", &[DISABLED], "cache"));
    assert_eq!(fixture.server.acquisitions(), 3);
}

#[test]
fn remote_metadata_admission_follows_lockfile_mode_not_download_policy() {
    let fixture = Fixture::new(false, false);
    for command in ["query", "build", "mod"] {
        let mut args = vec![command.to_owned()];
        if command == "mod" {
            args.extend(["graph".into(), "--output=json".into()]);
        } else {
            args.push("//:version".into());
            // Registry metadata is unaffected by the download policy; error
            // mode refuses unhashed registry files before any request
            // (IndexRegistry.java:171-179).
            args.push("--lockfile_mode=error".into());
        }
        args.extend([
            DISABLED.into(),
            format!("--registry={}", fixture.server.url()),
        ]);
        let expected = if command == "mod" {
            "--lockfile_mode=off"
        } else {
            "Missing checksum for registry file http://"
        };
        rejected(&fixture.invoke(&args, "cache"), expected);
    }
    assert!(fixture.server.counts.lock().unwrap().is_empty());
}

#[test]
fn module_io_argument_failures_are_bazels_under_both_policies() {
    let fixture = Fixture::new(false, false);
    write(
        fixture.temp.path(),
        "MODULE.bazel",
        "e=use_extension('//:extension.bzl','extension')\nuse_repo(e,'repo')\n",
    );
    write(fixture.temp.path(), "BUILD.bazel", "");
    for (operation, diagnostic) in [
        // getUrls(ensureNonEmpty) precedes the pending download (java:818-823).
        (
            "ctx.download([],block=False)",
            "java.io.IOException: urls not set",
        ),
        (
            "ctx.read('owned-input',watch='yes')",
            "attempted to watch path under working directory",
        ),
    ] {
        write(
            fixture.temp.path(),
            "extension.bzl",
            format!(
                "def implementation(ctx):\n    {operation}\nextension=module_extension(implementation)\n"
            ),
        );
        for flag in [DISABLED, "--repository_disable_download=false"] {
            let output = fixture.invoke(
                &["query".into(), "@repo//:value".into(), flag.into()],
                "cache",
            );
            eprintln!(
                "MODULE_IO_OPTION_FIXTURE_BEGIN policy={flag}\n{}MODULE_IO_OPTION_FIXTURE_END\nMODULE_IO_OPTION_STDERR_BEGIN\n{}MODULE_IO_OPTION_STDERR_END",
                fs::read_to_string(fixture.temp.path().join("extension.bzl")).unwrap(),
                String::from_utf8_lossy(&output.stderr)
            );
            rejected(&output, diagnostic);
            assert_eq!(
                String::from_utf8_lossy(&output.stderr)
                    .lines()
                    .filter(|line| line.starts_with("error: "))
                    .collect::<Vec<_>>(),
                [format!("error: {diagnostic}")]
            );
            assert!(
                String::from_utf8_lossy(&output.stderr)
                    .lines()
                    .any(|line| line == " --> //:extension.bzl:2:5")
            );
            assert!(
                String::from_utf8_lossy(&output.stderr)
                    .lines()
                    .any(|line| line == format!("2 |     {operation}"))
            );
            assert!(fixture.server.counts.lock().unwrap().is_empty());
            assert!(!fixture.cache().exists());
        }
    }
    assert!(fixture.server.counts.lock().unwrap().is_empty());
    assert!(!fixture.cache().exists());
}

#[test]
fn completed_tree_missing_blob_and_verified_blob_rebuild_work_without_repair_requests() {
    let fixture = Fixture::new(false, false);
    let tree = fixture.materialize(true).unwrap();
    assert_eq!(fixture.server.acquisitions(), 1);
    let manifest = tree.with_extension("json");
    let before = fs::read(&manifest).unwrap();
    fs::remove_file(fixture.blob(ARCHIVE)).unwrap();
    success(&fixture.run("build", &[DISABLED], "cache"));
    assert_eq!(fixture.materialize(false).unwrap(), tree);
    assert_eq!(fixture.server.acquisitions(), 1);
    fs::remove_dir_all(&tree).unwrap();
    fixture.seed(ARCHIVE);
    success(&fixture.run("query", &[DISABLED], "cache"));
    assert_eq!(fixture.materialize(false).unwrap(), tree);
    assert_eq!(fs::read(&manifest).unwrap(), before);
    assert_eq!(fs::read(tree.join("MODULE.bazel")).unwrap(), MODULE);
    assert_eq!(fixture.server.acquisitions(), 1);
    fs::remove_dir_all(&tree).unwrap();
    fs::write(fixture.blob(ARCHIVE), b"corrupt archive blob").unwrap();
    rejected(&fixture.run("build", &[DISABLED], "cache"), DISABLED);
    assert_eq!(fixture.server.acquisitions(), 1);
    assert_eq!(
        fs::read(fixture.blob(ARCHIVE)).unwrap(),
        b"corrupt archive blob"
    );
    assert!(!tree.exists());
}

#[test]
fn missing_and_corrupt_overlay_or_patch_are_blocked_but_all_verified_blobs_work() {
    for missing in [OVERLAY, PATCH] {
        for corrupt in [false, true] {
            let fixture = Fixture::new(false, true);
            fixture.seed(ARCHIVE);
            for bytes in [OVERLAY, PATCH] {
                if bytes != missing {
                    fixture.seed(bytes);
                } else if corrupt {
                    fixture.seed(bytes);
                    fs::write(fixture.blob(bytes), b"corrupt transformation").unwrap();
                }
            }
            rejected(&fixture.run("build", &[DISABLED], "cache"), DISABLED);
            assert_eq!(fixture.server.acquisitions(), 0);
            assert!(
                fs::read_dir(fixture.cache().join("trees"))
                    .unwrap()
                    .next()
                    .is_none()
            );
            if corrupt {
                assert_eq!(
                    fs::read(fixture.blob(missing)).unwrap(),
                    b"corrupt transformation"
                );
            } else {
                assert!(!fixture.blob(missing).exists());
            }
            fixture.seed(missing);
            let result = fixture.run("build", &[DISABLED], "cache");
            success(&result);
            let text = String::from_utf8(result.stdout).unwrap();
            let (_, path) = text.trim().split_once('\t').unwrap();
            assert_eq!(fs::read(path).unwrap(), b"1.8.2-policy");
            let tree = fixture.materialize(false).unwrap();
            assert_eq!(fs::read(tree.join("policy.txt")).unwrap(), OVERLAY);
            assert_eq!(fs::read(tree.join("MODULE.bazel")).unwrap(), MODULE);
            assert_eq!(fixture.server.acquisitions(), 0);
            let manifest = fs::read(tree.with_extension("json")).unwrap();
            assert_eq!(fixture.materialize(true).unwrap(), tree);
            assert_eq!(fs::read(tree.with_extension("json")).unwrap(), manifest);
            assert_eq!(fixture.server.acquisitions(), 0);
        }
    }
}

#[test]
fn policy_changes_start_fresh_registry_root_success_and_failure_slots() {
    use bazel_rs::{module_graph::ResolverOptions, repository::RepositorySet};
    let fixture = Fixture::new(false, false);
    let disabled = RepositorySet::with_options(
        fixture.temp.path().to_owned(),
        ResolverOptions {
            registries: vec![fixture.registry()],
            allow_remote: true,
            ..ResolverOptions::default()
        },
    )
    .with_repository_downloads_enabled(false)
    .with_source_cache(fixture.cache());
    let error = disabled.root("bazel_skylib+").unwrap_err();
    assert!(error.contains(DISABLED), "{error}");
    assert_eq!(fixture.server.acquisitions(), 0);
    let enabled = disabled.clone().with_repository_downloads_enabled(true);
    let tree = enabled.root("bazel_skylib+").unwrap().to_owned();
    assert_eq!(fixture.server.acquisitions(), 1);
    assert_eq!(disabled.clone().root("bazel_skylib+").unwrap_err(), error);
    assert_eq!(
        disabled
            .with_repository_downloads_enabled(false)
            .root("bazel_skylib+")
            .unwrap(),
        tree
    );
    fs::remove_dir_all(&tree).unwrap();
    fs::remove_file(fixture.blob(ARCHIVE)).unwrap();
    let fresh = enabled.clone().with_repository_downloads_enabled(false);
    assert!(fresh.root("bazel_skylib+").unwrap_err().contains(DISABLED));
    assert_eq!(fixture.server.acquisitions(), 1);
    assert_eq!(enabled.root("bazel_skylib+").unwrap(), tree);
}

#[test]
fn graph_policy_allows_metadata_without_acquiring_any_archive() {
    let fixture = Fixture::new(false, true);
    fixture.graph_only();
    let output = fixture.run("mod", &[DISABLED], "cache");
    success(&output);
    let graph: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(graph["dependencies"][0]["name"], "rules_license");
    assert!(
        fixture
            .server
            .count("/modules/rules_license/1.0.0/MODULE.bazel")
            > 0
    );
    assert_eq!(fixture.server.acquisitions(), 0);
    assert!(!fixture.cache().exists());
}

#[test]
fn local_git_override_keeps_its_existing_transport_and_admission_with_downloads_disabled() {
    let fixture = Fixture::new(false, false);
    let origin = fixture.temp.path().join("origin");
    fs::create_dir(&origin).unwrap();
    // Export the genuine pinned archive, then fetch a real local Git commit of it.
    bazel_rs::archive::extract(ARCHIVE, "tar.gz", "", &origin).unwrap();
    let git = |args: &[&str]| {
        let result = Command::new("timeout")
            .env_clear()
            .env("PATH", "/usr/bin:/bin")
            .env("HOME", fixture.temp.path().join("home"))
            .env("XDG_CONFIG_HOME", fixture.temp.path().join("home"))
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_SYSTEM", "/dev/null")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_TERMINAL_PROMPT", "0")
            .env("GIT_AUTHOR_DATE", "2000-01-01T00:00:00Z")
            .env("GIT_COMMITTER_DATE", "2000-01-01T00:00:00Z")
            .args([
                "--kill-after=5",
                "30",
                "git",
                "-c",
                "user.name=Fixture",
                "-c",
                "user.email=fixture@example.invalid",
                "-c",
                "core.hooksPath=/dev/null",
                "-c",
                "init.templateDir=",
            ])
            .args(args)
            .current_dir(&origin)
            .output()
            .unwrap();
        success(&result);
        String::from_utf8(result.stdout).unwrap().trim().to_owned()
    };
    git(&["init", "--quiet"]);
    git(&["add", "."]);
    git(&["commit", "--quiet", "-m", "genuine archive fixture"]);
    let commit = git(&["rev-parse", "HEAD"]);
    let remote = reqwest::Url::from_file_path(&origin).unwrap().to_string();
    let root_module = fs::read_to_string(fixture.temp.path().join("MODULE.bazel")).unwrap();
    write(
        fixture.temp.path(),
        "MODULE.bazel",
        format!(
            "{root_module}git_override(module_name='bazel_skylib',remote={remote:?},commit={commit:?})\n"
        ),
    );
    let registry = reqwest::Url::from_directory_path(fixture.temp.path().join("served"))
        .unwrap()
        .to_string();
    for command in ["query", "build"] {
        for allowed_remote in [false, true] {
            let mut args = vec![
                command.into(),
                "//:version".into(),
                DISABLED.into(),
                format!("--registry={registry}"),
            ];
            if allowed_remote {
                args.push("--lockfile_mode=off".into());
            }
            success(&fixture.invoke(&args, &format!("git-{command}-{allowed_remote}")));
        }
    }
    // Graph discovery rejects active extensions; use the genuine dependency-free
    // rules_license MODULE in a separate local commit for that graph profile.
    write(
        &origin,
        "MODULE.bazel",
        fs::read(
            fixture
                .temp
                .path()
                .join("served/modules/rules_license/1.0.0/MODULE.bazel"),
        )
        .unwrap(),
    );
    git(&["add", "MODULE.bazel"]);
    git(&[
        "commit",
        "--quiet",
        "-m",
        "genuine registry module graph fixture",
    ]);
    let commit = git(&["rev-parse", "HEAD"]);
    write(
        fixture.temp.path(),
        "MODULE.bazel",
        format!(
            "module(name='main')\nbazel_dep(name='rules_license',version='1.0.0')\ngit_override(module_name='rules_license',remote={remote:?},commit={commit:?})\n"
        ),
    );
    success(&fixture.invoke(
        &[
            "mod".into(),
            "graph".into(),
            "--output=json".into(),
            DISABLED.into(),
            // Explicit: never the default https://bcr.bazel.build/ registry.
            format!("--registry={registry}"),
        ],
        "git-graph",
    ));
    assert_eq!(fixture.server.acquisitions(), 0);
    assert!(fixture.server.counts.lock().unwrap().is_empty());
}
