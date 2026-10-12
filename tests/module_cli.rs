//! Isolated filesystem registries: these tests never use a network registry,
//! the checked-out workspace's MODULE.bazel, or a reference Bazel executable.
use serde_json::{Value, json};
use std::fs;
use std::path::PathBuf;
use std::process::{Command, Output};
use std::sync::atomic::{AtomicUsize, Ordering};

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
