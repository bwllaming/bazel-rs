//! N51: every module's repository mapping includes @bazel_tools
//! (tests/fixtures/eager_resolution_mapping; docs/BUILTIN_MODULES.md). A
//! loopback registry serves a module whose MODULE.bazel names @bazel_tools
//! extensions and repository rules, like rules_jvm_external 6.7. Never uses a
//! public registry or a reference Bazel executable.
#![allow(clippy::disallowed_methods)] // test harness: starts processes directly (clippy.toml)
use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

const FIXTURE: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/tests/fixtures/eager_resolution_mapping"
);
const UNAVAILABLE: &str = "cannot load '@@bazel_tools//tools/build_defs/repo:utils.bzl': the built-in repository @@bazel_tools is visible to every module but has no contents in this module profile; use --experimental-builtin-modules";

/// Serves the fixture registry on 127.0.0.1.
fn serve(root: PathBuf) -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            let root = root.clone();
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
                    let path = line.split(' ').nth(1).unwrap_or("").trim_start_matches('/');
                    let body = (!path.split('/').any(|part| part == ".."))
                        .then(|| fs::read(root.join(path)).ok())
                        .flatten();
                    let response = match body {
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
    port
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

struct Fixture {
    temp: tempfile::TempDir,
    port: u16,
}

impl Fixture {
    fn new() -> Self {
        let temp = tempfile::Builder::new()
            .prefix("native-eager-mapping-")
            .tempdir_in("/tmp/opencode")
            .unwrap();
        let fixture = Path::new(FIXTURE);
        copy_dir(&fixture.join("registry"), &temp.path().join("registry"));
        copy_dir(&fixture.join("workspace"), &temp.path().join("ws"));
        copy_dir(&fixture.join("localdep"), &temp.path().join("localdep"));
        fs::create_dir_all(temp.path().join("home")).unwrap();
        let port = serve(temp.path().join("registry"));
        Self { temp, port }
    }

    fn lock(&self) -> Option<serde_json::Value> {
        fs::read_to_string(self.temp.path().join("ws/MODULE.bazel.lock"))
            .ok()
            .map(|text| serde_json::from_str(&text).unwrap())
    }

    fn query(&self, mode: &str, query: &[&str]) -> Output {
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
            .arg(format!("--registry=http://127.0.0.1:{}", self.port))
            .arg(format!("--lockfile_mode={mode}"))
            .arg("--repository_disable_download")
            .args(query)
            .current_dir(self.temp.path().join("ws"))
            .output()
            .unwrap()
    }
}

fn stdout(output: &Output) -> String {
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout.clone()).unwrap()
}

fn last_error(output: &Output) -> String {
    assert_eq!(output.status.code(), Some(1));
    assert!(output.stdout.is_empty());
    let stderr = String::from_utf8_lossy(&output.stderr);
    stderr
        .lines()
        .rev()
        .find_map(|line| line.strip_prefix("bazel-rs: "))
        .unwrap_or_default()
        .to_owned()
}

#[test]
fn non_root_bazel_tools_usages_resolve_and_the_lockfile_is_written() {
    // Genuine Bazel writes a lockfile here; before N51 the native extension
    // usage table failed on the missing @bazel_tools entry and the lockfile
    // was silently not written.
    let f = Fixture::new();
    assert_eq!(stdout(&f.query("update", &["//:plain"])), "//:plain\n");
    let lock = f.lock().expect("lockfile written");
    let base = format!("http://127.0.0.1:{}/modules/eagerdep/1.0/", f.port);
    let hashes = lock["registryFileHashes"].as_object().unwrap();
    assert_eq!(
        hashes.keys().cloned().collect::<Vec<_>>(),
        [format!("{base}MODULE.bazel"), format!("{base}source.json")]
    );
    // The @bazel_tools extensions are in the usage table, never evaluated.
    assert_eq!(lock["moduleExtensions"], serde_json::json!({}));
    assert_eq!(lock["selectedYankedVersions"], serde_json::json!({}));
    assert_eq!(stdout(&f.query("off", &["//:plain"])), "//:plain\n");
}

#[test]
fn bzl_labels_see_bazel_tools_in_root_and_non_root_modules() {
    let f = Fixture::new();
    for mode in ["off", "update"] {
        let text = stdout(&f.query(mode, &["--output=build", "//labels:labels"]));
        assert!(
            text.contains(r#"tags = ["@@bazel_tools//tools:host_platform", "@@bazel_tools//tools:host_platform", "@@localdep+//:labels.bzl", "bazel_tools", "bazel_tools"],"#),
            "{text}"
        );
    }
}

#[test]
fn loading_bazel_tools_contents_fails_explicitly_without_the_builtin_profile() {
    let f = Fixture::new();
    for mode in ["off", "update"] {
        assert_eq!(last_error(&f.query(mode, &["//loads:loads"])), UNAVAILABLE);
    }
    // Module resolution succeeded, so update still recorded the registry.
    assert!(f.lock().is_some());
}

#[test]
fn eager_lockfile_path_fails_loudly_on_a_bad_extension_label() {
    // BazelDepGraphFunction.getExtensionUsagesById (:97-137) fails the
    // command; the lockfile path must not silently skip writing instead.
    let f = Fixture::new();
    let bad = f.temp.path().join("badext");
    fs::create_dir_all(&bad).unwrap();
    fs::write(
        bad.join("MODULE.bazel"),
        "module(name = \"badext\", version = \"1.0\")\n\nnope = use_extension(\"@nope//:ext.bzl\", \"ext\")\n",
    )
    .unwrap();
    fs::write(
        f.temp.path().join("ws/MODULE.bazel"),
        "module(name = \"eager_root\", version = \"0.1\")\n\nbazel_dep(name = \"eagerdep\", version = \"1.0\")\nbazel_dep(name = \"badext\", version = \"1.0\")\nlocal_path_override(\n    module_name = \"badext\",\n    path = \"../badext\",\n)\n",
    )
    .unwrap();
    assert_eq!(
        last_error(&f.query("update", &["//:plain"])),
        "Error computing the main repository mapping: invalid label for module extension found at @@badext+//:MODULE.bazel:3:21: no repo visible as '@nope' here"
    );
    assert!(f.lock().is_none());
    // Every command that resolves the module graph fails the same way, in
    // any lockfile mode and with a local file: registry, even when it needs
    // no mapping (a canonical label of a selected module).
    let file_registry = format!(
        "--registry=file://{}",
        f.temp.path().join("registry").display()
    );
    for (registry, mode) in [
        (format!("--registry=http://127.0.0.1:{}", f.port), "off"),
        (file_registry.clone(), "off"),
        (file_registry, "update"),
    ] {
        let output = Command::new(env!("CARGO_BIN_EXE_bazel-rs"))
            .env_clear()
            .env("PATH", "/usr/bin:/bin")
            .env("HOME", f.temp.path().join("home"))
            .env("TMPDIR", f.temp.path())
            .arg("--ignore_all_rc_files")
            .arg(format!(
                "--cache-dir={}",
                f.temp.path().join("cache").display()
            ))
            .args(["query", &registry, &format!("--lockfile_mode={mode}")])
            .args(["--repository_disable_download", "@@badext+//:MODULE.bazel"])
            .current_dir(f.temp.path().join("ws"))
            .output()
            .unwrap();
        assert_eq!(
            last_error(&output),
            "Error computing the main repository mapping: invalid label for module extension found at @@badext+//:MODULE.bazel:3:21: no repo visible as '@nope' here",
            "{registry} {mode}"
        );
    }
}

#[test]
fn modules_cannot_declare_the_builtin_repository_name() {
    // ModuleThreadContext.buildModule (Bazel 9.3 :383-401), in every profile.
    let f = Fixture::new();
    let module = f.temp.path().join("ws/MODULE.bazel");
    let original = fs::read_to_string(&module).unwrap();
    for (line, how, column) in [
        (
            "bazel_dep(name = \"other_tools\", version = \"1.0\", repo_name = \"bazel_tools\")",
            "by a bazel_dep",
            10,
        ),
        (
            "x = use_extension(\"//:x.bzl\", \"x\")\nuse_repo(x, \"bazel_tools\")",
            "by a use_repo() call",
            9,
        ),
    ] {
        fs::write(&module, format!("{original}{line}\n")).unwrap();
        let row = original.lines().count() + line.lines().count();
        let expected = format!(
            "The repo name 'bazel_tools' cannot be defined as a built-in dependency at <builtin> as it is already defined {how} at {}:{row}:{column}, 'bazel_tools' is a built-in dependency and cannot be used by any 'bazel_dep' or 'use_repo' directive",
            module.display()
        );
        for mode in ["off", "update"] {
            assert_eq!(last_error(&f.query(mode, &["//:plain"])), expected);
        }
    }
}

#[test]
fn non_registry_module_locations_use_the_canonical_module_file_label() {
    // ModuleFileFunction.getModuleFile (Bazel 9.3 :752-777): a non-registry
    // override's MODULE file is located as @@<name>+//:MODULE.bazel.
    let f = Fixture::new();
    let local = f.temp.path().join("collide_local");
    fs::create_dir_all(&local).unwrap();
    fs::write(
        local.join("MODULE.bazel"),
        "module(name = \"collide_local\", version = \"1.0\")\n\nx = use_extension(\"//:x.bzl\", \"x\")\nuse_repo(x, \"bazel_tools\")\n",
    )
    .unwrap();
    fs::write(
        f.temp.path().join("ws/MODULE.bazel"),
        "module(name = \"eager_root\", version = \"0.1\")\n\nbazel_dep(name = \"collide_local\", version = \"1.0\")\nlocal_path_override(\n    module_name = \"collide_local\",\n    path = \"../collide_local\",\n)\n",
    )
    .unwrap();
    assert_eq!(
        last_error(&f.query("update", &["//:plain"])),
        "The repo name 'bazel_tools' cannot be defined as a built-in dependency at <builtin> as it is already defined by a use_repo() call at @@collide_local+//:MODULE.bazel:4:9, 'bazel_tools' is a built-in dependency and cannot be used by any 'bazel_dep' or 'use_repo' directive"
    );
}
