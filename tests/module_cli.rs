//! Isolated filesystem and loopback registries: these tests never use a public registry,
//! the checked-out workspace's MODULE.bazel, or a reference Bazel executable.
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
            .output()
            .unwrap()
    }
    fn command(&self, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_bazel-rs"))
            .arg("--ignore_all_rc_files")
            .args(args)
            .current_dir(&self.0)
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
fn cli_extension_usage_is_rejected_even_when_hidden_and_without_imports() {
    let fixture =
        Fixture::new("module(name='root')\next = use_extension('//:missing.bzl', 'ext')\n");
    let output = fixture.run(&["--extension_info=hidden"]);
    assert!(!output.status.success());
    assert!(output.stdout.is_empty());
    assert!(String::from_utf8_lossy(&output.stderr).contains("cannot execute module extensions"));
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
fn cli_selected_module_extension_is_not_silently_skipped() {
    let fixture = Fixture::new("module(name='root')\nbazel_dep(name='a', version='1.0')\n");
    fixture.module("a", "1.0", "ext = use_extension('//:missing.bzl', 'ext')\n");
    let output = fixture.run(&[]);
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("cannot execute module extensions"));
}

#[test]
fn cli_rejects_unsupported_profiles_and_missing_registry_dependencies() {
    let fixture = Fixture::new("module(name='root')\n");
    for flags in [
        &["--output=text"][..],
        &["--lockfile_mode=update"],
        &["--include_builtin"],
        &["--verbose"],
        &["--extension_info=all"],
        &["unexpected"],
        &["--registry=https://example.invalid"],
    ] {
        let output = fixture.run(flags);
        assert!(!output.status.success(), "{flags:?}");
        assert!(output.stdout.is_empty(), "{flags:?}");
    }
    let output = Command::new(env!("CARGO_BIN_EXE_bazel-rs"))
        .args(["--ignore_all_rc_files", "mod", "graph"])
        .current_dir(&fixture.0)
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("text formatter"));
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
                    .set_read_timeout(Some(Duration::from_secs(1)))
                    .unwrap();
                stream
                    .set_write_timeout(Some(Duration::from_secs(1)))
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
fn http_dependency_free_commands_require_explicit_off_without_fetching() {
    let fixture = Fixture::new("module(name='root')\n");
    fixture.write("BUILD.bazel", "filegroup(name='empty')\n");
    fixture.write("MODULE.bazel.lock", "unchanged sentinel\n");
    let server = HttpRegistry::new();
    let registry = format!("--registry={}", server.url);
    for command in [
        vec!["mod", "graph", "--output=json"],
        vec!["query", "//:empty"],
        vec!["build", "//:empty"],
    ] {
        let mut args = command;
        args.push(&registry);
        let rejected = fixture.command(&args);
        assert!(!rejected.status.success());
        assert!(
            String::from_utf8_lossy(&rejected.stderr).contains("--lockfile_mode=off is required")
        );
        for invalid in [
            "--lockfile_mode=update",
            "--lockfile_mode=error",
            "--lockfile_mode=refresh",
        ] {
            args.push(invalid);
            assert!(!fixture.command(&args).status.success());
            args.pop();
        }
        args.extend(["--lockfile_mode", "off"]);
        succeeded(&fixture.command(&args));
    }
    assert!(server.requests().is_empty());
    assert_eq!(
        fs::read_to_string(fixture.0.join("MODULE.bazel.lock")).unwrap(),
        "unchanged sentinel\n"
    );
    assert!(!fixture.0.join("bazel-rs-out/repositories").exists());
}

#[test]
fn http_registry_override_requires_off_and_retains_selected_registry() {
    let fixture = Fixture::new("");
    let server = HttpRegistry::new();
    server.set(
        "/modules/a/1/MODULE.bazel",
        200,
        "module(name='a',version='1')\n",
    );
    fixture.write("MODULE.bazel", &format!("bazel_dep(name='a',version='1')\nsingle_version_override(module_name='a',registry={:?})\n", server.url));
    let failed = fixture.run(&[]);
    assert!(!failed.status.success());
    assert!(String::from_utf8_lossy(&failed.stderr).contains("--lockfile_mode=off is required"));
    assert!(server.requests().is_empty());
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
        assert!(String::from_utf8_lossy(&query.stdout).contains("@@a+//:copy"));
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
