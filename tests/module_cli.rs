//! Isolated filesystem and loopback registries: these tests never use a public registry,
//! the checked-out workspace's MODULE.bazel, or a reference Bazel executable.
#![allow(clippy::disallowed_methods)] // test harness: starts processes directly (clippy.toml)
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::fs;
use std::io::{Read, Write};
use std::net::TcpListener;
use std::path::PathBuf;
use std::process::{Command, Output};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;
include!("support/explicit_modules.rs");

static NEXT: AtomicUsize = AtomicUsize::new(0);
struct Fixture(PathBuf);
impl Fixture {
    fn new(module: &str) -> Self {
        let root = PathBuf::from("/tmp/opencode").join(format!(
            "native-module-cli-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(root.join("registry/modules")).unwrap();
        let fixture = Self(root);
        fixture.write("MODULE.bazel", module);
        fixture
    }
    fn write(&self, relative: &str, text: &str) {
        let path = self.0.join(relative);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, text).unwrap();
    }
    fn module(&self, name: &str, version: &str, extra: &str) {
        self.write(
            &format!("registry/modules/{name}/{version}/MODULE.bazel"),
            &format!("module(name={name:?}, version={version:?})\n{extra}"),
        );
    }
    fn run(&self, extra: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_bazel-rs"))
            .args(["--ignore_all_rc_files", "mod", "graph", "--output=json"])
            .arg(format!(
                "--registry=file://{}",
                self.0.join("registry").display()
            ))
            .args(extra)
            .current_dir(&self.0)
            .explicit_modules()
            .output()
            .unwrap()
    }
    fn command(&self, args: &[&str]) -> Output {
        // Never fall back to the default https://bcr.bazel.build/ registry:
        // commands without --registry use this fixture's local registry.
        let local = format!("--registry=file://{}", self.0.join("registry").display());
        let explicit = args.iter().any(|arg| arg.starts_with("--registry"));
        Command::new(env!("CARGO_BIN_EXE_bazel-rs"))
            .arg("--ignore_all_rc_files")
            .args(args)
            .args(
                (!explicit
                    && args
                        .iter()
                        .any(|arg| matches!(*arg, "mod" | "query" | "build")))
                .then_some(local),
            )
            .current_dir(&self.0)
            .explicit_modules()
            .output()
            .unwrap()
    }
    fn graph(&self, extra: &[&str]) -> Value {
        let output = self.run(extra);
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        serde_json::from_slice(&output.stdout).unwrap()
    }
    fn git(&self, args: &[&str]) -> String {
        let home = self.0.join("git-home");
        fs::create_dir_all(&home).unwrap();
        let output = Command::new("git")
            .env_clear()
            .env("PATH", "/usr/bin:/bin")
            .env("HOME", &home)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_AUTHOR_DATE", "2000-01-01T00:00:00Z")
            .env("GIT_COMMITTER_DATE", "2000-01-01T00:00:00Z")
            .args([
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
            .current_dir(self.0.join("upstream"))
            .output()
            .unwrap();
        succeeded(&output);
        String::from_utf8(output.stdout).unwrap().trim().to_owned()
    }
    fn git_source(&self, module: &str) -> String {
        self.write("upstream/pkg/MODULE.bazel", module);
        self.write("upstream/pkg/BUILD.bazel", "exports_files(['data'])\n");
        self.write("upstream/pkg/data", "pinned\n");
        self.git(&["init", "--quiet"]);
        self.git(&["add", "."]);
        self.git(&["commit", "--quiet", "-m", "pinned"]);
        let commit = self.git(&["rev-parse", "HEAD"]);
        self.write(
            "upstream/pkg/MODULE.bazel",
            "module(name='a',version='9')\n",
        );
        self.write("upstream/pkg/data", "head\n");
        self.git(&["add", "."]);
        self.git(&["commit", "--quiet", "-m", "newer head"]);
        commit
    }
    fn git_override(&self, commit: &str, extra: &str) {
        self.write("MODULE.bazel", &format!(
            "module(name='root')\nbazel_dep(name='a',version='1',repo_name='alias')\ngit_override(module_name='a',remote={:?},commit={commit:?},strip_prefix='pkg'{extra})\n",
            format!("file://{}", self.0.join("upstream").display())
        ));
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

#[test]
fn cli_json_selected_aliases_bfs_and_no_lockfile_writes() {
    let fixture = Fixture::new(
        "module(name='root', version='1.0', repo_name='root_repo')\nbazel_dep(name='a', version='1.0', repo_name='first')\nbazel_dep(name='b', version='1.0')\n",
    );
    fixture.module(
        "a",
        "1.0",
        "bazel_dep(name='b', version='2.0', repo_name='second')",
    );
    fixture.module("b", "1.0", "");
    fixture.module("b", "2.0", "");
    fixture.write("MODULE.bazel.lock", "existing lockfile sentinel\n");
    let output = fixture.graph(&[
        "--lockfile_mode=off",
        "--extension_info=hidden",
        "--noinclude_builtin",
        "--noinclude_unused",
        "--noverbose",
        "--cycles",
    ]);
    assert_eq!(output["key"], "<root>");
    assert_eq!(output["apparentName"], "root_repo");
    assert_eq!(output["dependencies"].as_array().unwrap().len(), 2);
    assert_eq!(output["dependencies"][0]["apparentName"], "first");
    assert_eq!(
        output["dependencies"][0]["dependencies"][0],
        json!({"key":"b@2.0", "name":"b", "version":"2.0", "apparentName":"second", "unexpanded":true})
    );
    assert_eq!(output["dependencies"][1]["key"], "b@2.0");
    assert_eq!(
        fs::read_to_string(fixture.0.join("MODULE.bazel.lock")).unwrap(),
        "existing lockfile sentinel\n"
    );
    assert!(!fixture.0.join("bazel-out").exists());
    assert!(!fixture.0.join(".bazel-rs").exists());
}

#[test]
fn cli_projects_local_module_patches_before_selection_without_materializing() {
    let fixture = Fixture::new(
        "module(name='root',repo_name='self')\nbazel_dep(name='a',version='1')\nbazel_dep(name='b',version='1')\nsingle_version_override(module_name='a',patches=['@self//:fix.patch'],patch_strip=1)\n",
    );
    fixture.module("a", "1", "");
    fixture.module("b", "1", "");
    fixture.module("b", "2", "");
    fixture.write("BUILD.bazel", "not evaluated !!!");
    fixture.write("fix.patch", "--- a/MODULE.bazel\n+++ b/MODULE.bazel\n@@ -1 +1,2 @@\n module(name=\"a\", version=\"1\")\n+bazel_dep(name='b',version='2')\n");
    let output = fixture.graph(&[]);
    assert_eq!(output["dependencies"][1]["key"], "b@2");
    assert_eq!(output["dependencies"][0]["dependencies"][0]["key"], "b@2");
    fixture.write(
        "fix.patch",
        "--- a/source.txt\n+++ b/source.txt\n@@ -1 +1 @@\n-old\n+new\n",
    );
    let output = fixture.graph(&[]);
    assert_eq!(output["dependencies"][1]["key"], "b@1");
    assert_eq!(output["dependencies"][0]["dependencies"], json!([]));
    assert!(!fixture.0.join("bazel-rs-out").exists());
    assert!(!fixture.0.join("MODULE.bazel.lock").exists());
}

#[test]
fn cli_graph_ignores_commands_and_canonical_external_patches_without_reading_sources() {
    for options in [
        "patch_cmds=['touch command-ran']",
        "patches=['@@unavailable+//:missing.patch']",
        "patch_strip=2",
    ] {
        let fixture = Fixture::new(&format!(
            "bazel_dep(name='a',version='1')\nsingle_version_override(module_name='a',{options})\n"
        ));
        fixture.module("a", "1", "");
        fixture.write(
            "registry/modules/a/1/source.json",
            "invalid source descriptor !!!",
        );
        assert_eq!(fixture.graph(&[])["dependencies"][0]["key"], "a@1");
        assert!(!fixture.0.join("command-ran").exists());
        assert!(!fixture.0.join("bazel-rs-out").exists());
    }
}

#[test]
fn cli_local_override_uses_empty_key_and_declared_version() {
    let fixture = Fixture::new(
        "module(name='root')\nbazel_dep(name='a', version='1.0', repo_name='alias')\nlocal_path_override(module_name='a', path='local')\n",
    );
    fixture.write(
        "local/MODULE.bazel",
        "module(name='a', version='7.0+metadata')\n",
    );
    let output = fixture.graph(&[]);
    assert_eq!(output["dependencies"][0]["key"], "a@_");
    assert_eq!(output["dependencies"][0]["version"], "7.0");
    assert_eq!(output["dependencies"][0]["apparentName"], "alias");
    assert!(!fixture.0.join("MODULE.bazel.lock").exists());
}

#[test]
fn git_graph_pins_first_commit_and_preserves_declared_version_in_private_cache() {
    use bazel_rs::module_graph::{ModuleSource, ResolverOptions, resolve_with_source_cache};
    let f = Fixture::new("");
    let commit = f.git_source("module(name='a',version='7',repo_name='self_a')\nbazel_dep(name='ignored',dev_dependency=True)\nlocal_path_override(module_name='ignored',path='/not/read')\n");
    f.git_override(&commit, "");
    let cache = f.0.join("selected/repositories");
    let graph = resolve_with_source_cache(&f.0, &ResolverOptions::default(), &cache).unwrap();
    let module = &graph.modules["a+"];
    assert_eq!(module.version, "");
    assert_eq!(module.module.module.as_ref().unwrap().version, "7");
    assert_eq!(graph.root_mapping["alias"], "a+");
    assert_eq!(module.mapping["self_a"], "a+");
    assert!(module.module.overrides.is_empty());
    assert!(!module.mapping.contains_key("ignored"));
    let ModuleSource::Git { spec, tree, .. } = &module.source else {
        panic!("not Git")
    };
    assert_eq!(spec.commit, commit);
    assert_eq!(tree.commit, commit);
    assert!(tree.root.starts_with(cache.join("git")));
    assert_eq!(fs::read(tree.root.join("data")).unwrap(), b"pinned\n");
    assert_eq!(
        fs::read(tree.root.join("MODULE.bazel")).unwrap(),
        tree.module_file
    );
    assert_eq!(fs::read(f.0.join("upstream/pkg/data")).unwrap(), b"head\n");
    assert!(!f.0.join("bazel-rs-out").exists());
}

#[test]
fn git_cli_relative_cache_is_cwd_relative_and_graph_uses_empty_key() {
    let f = Fixture::new("");
    let commit = f.git_source("module(name='a',version='7')\n");
    f.git_override(&commit, "");
    fs::create_dir_all(f.0.join("subdir")).unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_bazel-rs"))
        .args([
            "--ignore_all_rc_files",
            "--cache-dir=chosen",
            "mod",
            "graph",
            "--output=json",
        ])
        // Explicit offline registry, never the default bcr.bazel.build.
        .arg(format!(
            "--registry=file://{}",
            f.0.join("registry").display()
        ))
        .current_dir(f.0.join("subdir"))
        .explicit_modules()
        .output()
        .unwrap();
    succeeded(&output);
    let graph: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(graph["dependencies"][0]["key"], "a@_");
    assert_eq!(graph["dependencies"][0]["version"], "7");
    assert_eq!(graph["dependencies"][0]["apparentName"], "alias");
    assert!(f.0.join("subdir/chosen/repositories/git").is_dir());
    assert!(!f.0.join("chosen").exists());
    assert!(!f.0.join("bazel-rs-out").exists());
    assert!(!f.0.join("MODULE.bazel.lock").exists());
}

#[test]
fn uppercase_git_pin_is_consistent_between_graph_and_repository_query() {
    let f = Fixture::new("");
    let commit = f.git_source("module(name='a',version='7')\n");
    f.git_override(&commit.to_ascii_uppercase(), "");
    succeeded(&f.command(&["--cache-dir=chosen", "mod", "graph", "--output=json"]));
    let result = f.command(&["--cache-dir=chosen", "query", "@alias//:data"]);
    succeeded(&result);
    assert_eq!(String::from_utf8(result.stdout).unwrap(), "@alias//:data\n");
}

#[test]
fn git_module_patch_adds_dependency_before_selection_and_patches_full_tree() {
    use bazel_rs::module_graph::{ModuleSource, ResolverOptions, resolve_with_source_cache};
    let f = Fixture::new("");
    let commit = f.git_source("module(name='a',version='7')\n");
    f.git_override(&commit, ",patches=[':fix.patch'],patch_strip=1");
    f.write("BUILD.bazel", "not evaluated !!!");
    f.write("fix.patch", "diff --git a/MODULE.bazel b/MODULE.bazel\n--- a/MODULE.bazel\n+++ b/MODULE.bazel\n@@ -1 +1,2 @@\n module(name='a',version='7')\n+bazel_dep(name='leaf',version='2')\ndiff --git a/data b/data\n--- a/data\n+++ b/data\n@@ -1 +1 @@\n-pinned\n+patched\n");
    f.module("leaf", "1", "");
    f.module("leaf", "2", "");
    let mut root = fs::read_to_string(f.0.join("MODULE.bazel")).unwrap();
    root.push_str("bazel_dep(name='leaf',version='1')\n");
    f.write("MODULE.bazel", &root);
    let options = ResolverOptions {
        registries: vec![f.0.join("registry").into()],
        ..Default::default()
    };
    let graph = resolve_with_source_cache(&f.0, &options, &f.0.join("cache")).unwrap();
    assert_eq!(graph.modules["leaf+"].version, "2");
    assert_eq!(graph.modules["a+"].mapping["leaf"], "leaf+");
    let ModuleSource::Git { tree, .. } = &graph.modules["a+"].source else {
        panic!()
    };
    assert_eq!(fs::read(tree.root.join("data")).unwrap(), b"patched\n");
    assert_eq!(fs::read(f.0.join("upstream/pkg/data")).unwrap(), b"head\n");
    assert!(!f.0.join("bazel-rs-out").exists());
}

#[test]
fn git_name_mismatch_does_not_publish_a_module_tree() {
    let f = Fixture::new("");
    let commit = f.git_source("module(name='wrong',version='7')\n");
    f.git_override(&commit, "");
    let failed = f.command(&["--cache-dir=chosen", "mod", "graph", "--output=json"]);
    assert!(!failed.status.success());
    assert!(
        String::from_utf8_lossy(&failed.stderr).contains("module name mismatch"),
        "{}",
        String::from_utf8_lossy(&failed.stderr)
    );
    fn no_module(path: &std::path::Path) {
        if !path.exists() {
            return;
        }
        for entry in fs::read_dir(path).unwrap() {
            let entry = entry.unwrap();
            assert_ne!(entry.file_name(), "MODULE.bazel");
            if entry.file_type().unwrap().is_dir() {
                no_module(&entry.path());
            }
        }
    }
    no_module(&f.0.join("chosen"));
    assert!(!f.0.join("bazel-rs-out").exists());
}

#[test]
fn git_unsupported_options_and_patch_operations_fail_before_cache_creation() {
    for (selector, extra, expected) in [
        (
            "tag='v1'",
            ",patches=[':missing.patch']",
            "supports only full commit pins",
        ),
        (
            "branch='main'",
            ",patches=[':missing.patch']",
            "supports only full commit pins",
        ),
        (
            "commit='1111111111111111111111111111111111111111'",
            ",init_submodules=True,patches=[':missing.patch']",
            "init_submodules",
        ),
        (
            "commit='1111111111111111111111111111111111111111'",
            ",patch_cmds=['touch command-ran']",
            "patch_cmds",
        ),
        (
            "commit='1111111111111111111111111111111111111111'",
            ",patches=['@@external+//:missing.patch']",
            "canonical external patch",
        ),
    ] {
        let f = Fixture::new(&format!(
            "bazel_dep(name='a')\ngit_override(module_name='a',remote='file:///nonexistent-native-git-fixture',{selector}{extra})\n"
        ));
        let failed = f.run(&[]);
        assert!(!failed.status.success());
        let error = String::from_utf8_lossy(&failed.stderr);
        assert!(error.contains(expected), "{error}");
        assert!(!f.0.join("bazel-rs-out").exists());
        assert!(!f.0.join("command-ran").exists());
    }
}

#[test]
fn git_unknown_full_commit_reports_real_git_error_without_fallback() {
    let f = Fixture::new("");
    f.git_source("module(name='a',version='7')\n");
    f.git_override("1111111111111111111111111111111111111111", "");
    let failed = f.run(&[]);
    assert!(!failed.status.success());
    assert!(failed.stdout.is_empty());
    let error = String::from_utf8_lossy(&failed.stderr);
    assert!(error.to_ascii_lowercase().contains("git"), "{error}");
    assert!(!error.contains("no repository fetching"), "{error}");
    assert!(
        error.contains("fetch") || error.contains("not our ref") || error.contains("not a valid"),
        "{error}"
    );
}

#[test]
fn cli_graph_evaluates_extensions_only_for_shown_repositories() {
    // docs/MOD_COMMAND.md: the graph never needs the extension's
    // repositories with --extension_info=hidden; show_extension does.
    let fixture =
        Fixture::new("module(name='root')\next = use_extension('//:missing.bzl', 'ext')\n");
    let graph = fixture.graph(&["--extension_info=hidden"]);
    assert_eq!(graph["dependencies"], json!([]));
    let output = fixture.command(&["mod", "show_extension", "<root>//:missing.bzl%ext"]);
    assert_eq!(output.status.code(), Some(2));
    assert!(output.stdout.is_empty());
    assert!(String::from_utf8_lossy(&output.stderr).contains("ERROR: "));
}

#[test]
fn cli_dev_filter_applies_to_dependencies_and_extension_declarations() {
    let fixture = Fixture::new(
        "module(name='root')\nbazel_dep(name='a', version='1.0')\nbazel_dep(name='missing', version='1.0', dev_dependency=True)\next = use_extension('//:missing.bzl', 'ext', dev_dependency=True)\n",
    );
    fixture.module("a", "1.0", "bazel_dep(name='missing_transitive', version='1.0', dev_dependency=True)\next = use_extension('//:missing.bzl', 'ext', dev_dependency=True)\n");
    let output = fixture.graph(&["--ignore_dev_dependency=true"]);
    assert_eq!(output["dependencies"].as_array().unwrap().len(), 1);
    assert_eq!(output["dependencies"][0]["dependencies"], json!([]));
    let failed = fixture.run(&["--ignore_dev_dependency", "--noignore_dev_dependency"]);
    assert!(!failed.status.success());
}

#[test]
fn cli_selected_module_extension_usages_are_listed() {
    let fixture = Fixture::new("module(name='root')\nbazel_dep(name='a', version='1.0')\n");
    fixture.module("a", "1.0", "ext = use_extension('//:missing.bzl', 'ext')\n");
    let output = fixture.command(&["mod", "graph", "--extension_info=usages"]);
    succeeded(&output);
    assert_eq!(
        String::from_utf8_lossy(&output.stdout),
        "<root> (root@_)\n└───a@1.0 \n\n"
    );
}

#[test]
fn cli_rejects_unsupported_profiles_and_missing_registry_dependencies() {
    let fixture = Fixture::new("module(name='root')\n");
    for flags in [&["unexpected"][..], &["--output=streamed_proto"]] {
        let output = fixture.run(flags);
        assert_eq!(output.status.code(), Some(2), "{flags:?}");
        assert!(output.stdout.is_empty(), "{flags:?}");
    }
    let output = fixture.command(&["mod", "graph"]);
    succeeded(&output);
    assert_eq!(
        String::from_utf8_lossy(&output.stdout),
        "<root> (root@_)\n\n"
    );
    fixture.write(
        "MODULE.bazel",
        "module(name='root')\nbazel_dep(name='missing', version='1.0')\n",
    );
    let output = fixture.run(&[]);
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("missing"));
}

#[test]
fn cli_registry_precedence_is_configured_order() {
    let fixture = Fixture::new("module(name='root')\nbazel_dep(name='a', version='1.0')\n");
    fixture.module("a", "1.0", "");
    fixture.write(
        "second/modules/a/1.0/MODULE.bazel",
        "module(name='a', version='1.0')\nbazel_dep(name='missing', version='1.0')\n",
    );
    let registry = format!("file://{}", fixture.0.join("second").display());
    let output = fixture.graph(&["--registry", &registry]);
    assert_eq!(output["dependencies"][0]["dependencies"], json!([]));
}

// Nonblocking accept plus bounded stream I/O makes Drop join safe even when a
// test fails before issuing its expected requests. No public network is used.
struct HttpRegistry {
    url: String,
    responses: Arc<Mutex<BTreeMap<String, (u16, String)>>>,
    requests: Arc<Mutex<Vec<String>>>,
    stop: Arc<AtomicBool>,
    worker: Option<JoinHandle<()>>,
}
impl HttpRegistry {
    fn new() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let responses = Arc::new(Mutex::new(BTreeMap::<String, (u16, String)>::new()));
        let requests = Arc::new(Mutex::new(Vec::new()));
        let stop = Arc::new(AtomicBool::new(false));
        let (thread_responses, thread_requests, thread_stop) =
            (responses.clone(), requests.clone(), stop.clone());
        let worker = std::thread::spawn(move || {
            while !thread_stop.load(Ordering::Relaxed) {
                let (mut stream, _) = match listener.accept() {
                    Ok(value) => value,
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(Duration::from_millis(2));
                        continue;
                    }
                    Err(error) => panic!("loopback accept: {error}"),
                };
                stream
                    .set_read_timeout(Some(Duration::from_secs(30)))
                    .unwrap();
                stream
                    .set_write_timeout(Some(Duration::from_secs(30)))
                    .unwrap();
                let mut request = Vec::new();
                let mut byte = [0];
                while request.len() < 8192 && !request.ends_with(b"\r\n\r\n") {
                    if stream.read(&mut byte).unwrap_or(0) == 0 {
                        break;
                    }
                    request.push(byte[0]);
                }
                let text = String::from_utf8_lossy(&request);
                let path = text.split_whitespace().nth(1).unwrap_or("").to_owned();
                thread_requests.lock().unwrap().push(path.clone());
                let (status, body) = thread_responses
                    .lock()
                    .unwrap()
                    .get(&path)
                    .cloned()
                    .unwrap_or((404, "not found".to_owned()));
                let response = format!(
                    "HTTP/1.1 {status} Test\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = stream.write_all(response.as_bytes());
            }
        });
        Self {
            url,
            responses,
            requests,
            stop,
            worker: Some(worker),
        }
    }
    fn set(&self, path: &str, status: u16, body: &str) {
        self.responses
            .lock()
            .unwrap()
            .insert(path.to_owned(), (status, body.to_owned()));
    }
    fn requests(&self) -> Vec<String> {
        self.requests.lock().unwrap().clone()
    }
}
impl Drop for HttpRegistry {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(worker) = self.worker.take() {
            let result = worker.join();
            if !std::thread::panicking() {
                result.unwrap();
            }
        }
    }
}
fn succeeded(output: &Output) {
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn http_graph_fallback_preserves_origin_bytes_and_reads_metadata_fresh() {
    use bazel_rs::module_graph::{ModuleSource, ResolverOptions, resolve};
    use bazel_rs::registry::Registry;
    let fixture =
        Fixture::new("module(name='root')\nbazel_dep(name='a',version='1',repo_name='alias')\n");
    let server = HttpRegistry::new();
    let module = "# authoritative registry bytes\r\nmodule(name='a',version='1')\r\n";
    server.set("/gone/modules/a/1/MODULE.bazel", 410, "gone");
    server.set("/chosen/modules/a/1/MODULE.bazel", 200, module);
    let options = ResolverOptions {
        registries: ["missing", "gone", "chosen"]
            .into_iter()
            .map(|prefix| Registry::parse(&format!("{}/{prefix}", server.url)).unwrap())
            .collect(),
        allow_remote: true,
        ..Default::default()
    };
    let graph = resolve(&fixture.0, &options).unwrap();
    assert_eq!(graph.root_mapping["alias"], "a+");
    assert_eq!(
        graph.modules["a+"].source,
        ModuleSource::Registry {
            registry: options.registries[2].clone(),
            name: "a".into(),
            version: "1".into(),
            module_file: module.as_bytes().to_vec(),
            evaluated_module_file: module.as_bytes().to_vec(),
            local_patches: Default::default(),
        }
    );
    assert_eq!(
        server.requests(),
        [
            "/missing/modules/a/1/MODULE.bazel",
            "/gone/modules/a/1/MODULE.bazel",
            "/chosen/modules/a/1/MODULE.bazel",
            "/chosen/modules/a/metadata.json"
        ]
    );
    server.set(
        "/chosen/modules/a/metadata.json",
        200,
        "{\"yanked_versions\":{\"1\":\"newly revoked\"}}",
    );
    assert!(
        resolve(&fixture.0, &options)
            .unwrap_err()
            .contains("newly revoked")
    );
    server.set("/chosen/modules/a/metadata.json", 200, "{}");
    let registry = format!("--registry={}/chosen", server.url);
    let output = fixture.command(&[
        "mod",
        "graph",
        "--output=json",
        "--lockfile_mode=off",
        &registry,
    ]);
    succeeded(&output);
    let graph: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(graph["dependencies"][0]["key"], "a@1");
    assert_eq!(graph["dependencies"][0]["apparentName"], "alias");
    assert!(
        server
            .requests()
            .iter()
            .all(|p| !p.ends_with("source.json"))
    );
    assert!(!fixture.0.join("bazel-rs-out").exists());
    assert!(!fixture.0.join("MODULE.bazel.lock").exists());
}

#[test]
fn http_server_and_module_errors_never_fall_back() {
    let fixture = Fixture::new("bazel_dep(name='a',version='1')\n");
    fixture.module("a", "1", "");
    let server = HttpRegistry::new();
    let remote = format!("--registry={}", server.url);
    let local = format!("--registry=file://{}", fixture.0.join("registry").display());
    for (status, module, diagnostic) in [
        (500, "server broken", "500"),
        (200, "module(", "MODULE.bazel"),
        (
            200,
            "module(name='wrong',version='1')",
            "module name mismatch",
        ),
    ] {
        server.set("/modules/a/1/MODULE.bazel", status, module);
        let output = fixture.command(&[
            "mod",
            "graph",
            "--output=json",
            "--lockfile_mode=off",
            &remote,
            &local,
        ]);
        assert!(!output.status.success());
        assert!(output.stdout.is_empty());
        assert!(
            String::from_utf8_lossy(&output.stderr).contains(diagnostic),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    assert_eq!(server.requests(), vec!["/modules/a/1/MODULE.bazel"; 3]);
}

#[test]
fn http_dependency_free_commands_use_lockfile_modes_without_fetching() {
    let fixture = Fixture::new("module(name='root')\n");
    fixture.write("BUILD.bazel", "filegroup(name='empty')\n");
    fixture.write("MODULE.bazel.lock", "unchanged sentinel\n");
    let server = HttpRegistry::new();
    let registry = format!("--registry={}", server.url);
    // mod graph uses every lockfile mode like any other command.
    let mut args = vec!["mod", "graph", "--output=json", &registry];
    succeeded(&fixture.command(&args));
    args.extend(["--lockfile_mode", "off"]);
    succeeded(&fixture.command(&args));
    // query/build support every lockfile mode. The sentinel has no
    // lockFileVersion: the empty lockfile, except in error mode
    // (BazelLockFileFunction.java:321-332); an unchanged empty value is not
    // written (BazelLockFileModule.java:186).
    for command in [vec!["query", "//:empty"], vec!["build", "//:empty"]] {
        let mut args = command;
        args.push(&registry);
        succeeded(&fixture.command(&args));
        for mode in [
            "--lockfile_mode=update",
            "--lockfile_mode=refresh",
            "--lockfile_mode=off",
        ] {
            args.push(mode);
            succeeded(&fixture.command(&args));
            args.pop();
        }
        args.push("--lockfile_mode=error");
        let failed = fixture.command(&args);
        assert!(!failed.status.success());
        assert!(String::from_utf8_lossy(&failed.stderr).contains(
            "The version of MODULE.bazel.lock is not supported by this version of Bazel. Please run `bazel mod deps --lockfile_mode=update` to update your lockfile."
        ));
        args.pop();
        args.push("--lockfile_mode=bogus");
        let failed = fixture.command(&args);
        assert!(String::from_utf8_lossy(&failed.stderr).contains(
            "While parsing option --lockfile_mode=bogus: Not a valid Lockfile mode: 'bogus' (should be off, update, refresh or error)"
        ));
    }
    assert!(server.requests().is_empty());
    assert_eq!(
        fs::read_to_string(fixture.0.join("MODULE.bazel.lock")).unwrap(),
        "unchanged sentinel\n"
    );
    assert!(!fixture.0.join("bazel-rs-out/repositories").exists());
}

#[test]
fn http_registry_override_retains_selected_registry() {
    let fixture = Fixture::new("");
    let server = HttpRegistry::new();
    server.set(
        "/modules/a/1/MODULE.bazel",
        200,
        "module(name='a',version='1')\n",
    );
    fixture.write("MODULE.bazel", &format!("bazel_dep(name='a',version='1')\nsingle_version_override(module_name='a',registry={:?})\n", server.url));
    let graph = fixture.graph(&["--lockfile_mode=off"]);
    assert_eq!(graph["dependencies"][0]["key"], "a@1");
}

#[test]
fn http_query_and_build_local_sources_keep_live_paths_and_absolute_bases() {
    let fixture = Fixture::new("bazel_dep(name='a',version='1',repo_name='alias')\n");
    fixture.write(
        "live/a/MODULE.bazel",
        "must not evaluate this invalid source MODULE !!!",
    );
    fixture.write("live/a/BUILD.bazel", "exports_files(['data'])\ngenrule(name='copy',srcs=['data'],outs=['copy.txt'],cmd='cat $(SRCS) > $@')\n");
    fixture.write("live/a/data", "before");
    let server = HttpRegistry::new();
    server.set(
        "/modules/a/1/MODULE.bazel",
        200,
        "module(name='a',version='1')\n",
    );
    let registry = format!("--registry={}", server.url);
    for descriptor in [
        json!({"type":"local_path","path":fixture.0.join("live/a")}),
        json!({"type":"local_path","path":"a"}),
    ] {
        server.set("/modules/a/1/source.json", 200, &descriptor.to_string());
        server.set(
            "/bazel_registry.json",
            200,
            &json!({"module_base_path":fixture.0.join("live")}).to_string(),
        );
        let query = fixture.command(&["query", "--lockfile_mode=off", &registry, "@alias//:all"]);
        succeeded(&query);
        assert!(String::from_utf8_lossy(&query.stdout).contains("@alias//:copy"));
        assert!(!fixture.0.join("bazel-rs-out").exists());
    }
    let custom_cache = fixture.0.join("custom-cache");
    let cache = format!("--cache-dir={}", custom_cache.display());
    for contents in ["before", "after"] {
        fixture.write("live/a/data", contents);
        let built = fixture.command(&[
            &cache,
            "build",
            "--lockfile_mode=off",
            &registry,
            "@alias//:copy",
        ]);
        succeeded(&built);
        let stdout = String::from_utf8(built.stdout).unwrap();
        let (_, output_path) = stdout.trim_end().split_once('\t').unwrap();
        assert!(PathBuf::from(output_path).starts_with(&custom_cache));
        assert_eq!(fs::read_to_string(output_path).unwrap(), contents);
    }
    assert!(!custom_cache.join("repositories").exists());
    assert!(!fixture.0.join("bazel-rs-out").exists());
    server.set(
        "/bazel_registry.json",
        200,
        "{\"module_base_path\":\"relative\"}",
    );
    let failed = fixture.command(&["query", "--lockfile_mode=off", &registry, "@alias//:all"]);
    assert!(!failed.status.success());
    assert!(
        String::from_utf8_lossy(&failed.stderr)
            .contains("relative module_base_path is unsupported for remote registry")
    );
}

#[test]
fn http_selected_source_and_yanked_metadata_do_not_fall_back() {
    let fixture = Fixture::new("bazel_dep(name='a',version='1')\n");
    fixture.write("live/BUILD.bazel", "filegroup(name='x')\n");
    let server = HttpRegistry::new();
    for prefix in ["first", "second"] {
        server.set(
            &format!("/{prefix}/modules/a/1/MODULE.bazel"),
            200,
            "module(name='a',version='1')\n",
        );
    }
    server.set(
        "/second/modules/a/1/source.json",
        200,
        &json!({"type":"local_path","path":fixture.0.join("live")}).to_string(),
    );
    let first = format!("--registry={}/first", server.url);
    let second = format!("--registry={}/second", server.url);
    let failed = fixture.command(&["query", "--lockfile_mode=off", &first, &second, "@a//:all"]);
    assert!(!failed.status.success());
    assert!(String::from_utf8_lossy(&failed.stderr).contains("/first/modules/a/1/source.json"));
    server.set("/first/modules/a/metadata.json", 500, "broken metadata");
    let failed = fixture.command(&[
        "mod",
        "graph",
        "--output=json",
        "--lockfile_mode=off",
        &first,
        &second,
    ]);
    assert!(!failed.status.success());
    assert!(String::from_utf8_lossy(&failed.stderr).contains("500"));
    assert!(
        server
            .requests()
            .iter()
            .all(|path| !path.starts_with("/second/"))
    );
    assert!(!fixture.0.join("bazel-rs-out").exists());
}
