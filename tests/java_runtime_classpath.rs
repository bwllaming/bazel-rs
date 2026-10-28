//! Pinned genuine helper semantics and normal generated-artifact consumers.
#![allow(clippy::disallowed_methods)]
include!("support/temp_root.rs");
use std::collections::BTreeMap;
use std::fs;
use std::path::Path;
use std::process::{Command, Output};

const PACKAGE: &str = "bazel_internal/test_rules/runtime_classpath";

fn copy(from: &Path, to: &Path) {
    fs::create_dir_all(to).unwrap();
    for entry in fs::read_dir(from).unwrap() {
        let entry = entry.unwrap();
        if entry.file_type().unwrap().is_dir() {
            copy(&entry.path(), &to.join(entry.file_name()));
        } else {
            fs::copy(entry.path(), to.join(entry.file_name())).unwrap();
        }
    }
}

struct Fixture(tempfile::TempDir);
impl Fixture {
    fn new() -> Self {
        let f = Self(tempfile::tempdir_in(crate::test_temp::root()).unwrap());
        copy(
            &Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("tests/fixtures/java_runtime_classpath/workspace"),
            f.path(),
        );
        for dir in ["home", "tmp"] {
            fs::create_dir(f.path().join(dir)).unwrap();
        }
        f
    }
    fn path(&self) -> &Path {
        self.0.path()
    }
    fn run(&self, command: &str, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_bazel-rs"))
            .current_dir(self.path())
            .env_clear()
            .env("PATH", "/usr/bin:/bin")
            .env("HOME", self.path().join("home"))
            .env("TMPDIR", self.path().join("tmp"))
            .env("BAZEL_RS_RESULT_LINES", "1")
            .env("BAZEL_RS_SNAPSHOT_EXPLAIN", "1")
            .env("BAZEL_RS_DUMP_ACTIONS", "1")
            .args([
                "--ignore_all_rc_files",
                "--cache-dir=cache",
                command,
                "--experimental-builtin-modules=false",
                "--host_platform=//:host",
                "--platforms=//:host",
                "--lockfile_mode=update",
                "--repository_disable_download",
                "--memory-limit=2048",
            ])
            .args(args)
            .output()
            .unwrap()
    }
    fn ok(&self, command: &str, args: &[&str]) -> Output {
        let out = self.run(command, args);
        assert!(out.status.success(), "{}", errors(&out));
        out
    }
    fn build(&self, flags: &[&str]) -> (BTreeMap<String, String>, String) {
        let mut args = flags.to_vec();
        let report = format!("//{PACKAGE}:report");
        let consume = format!("//{PACKAGE}:consume");
        args.extend([report.as_str(), consume.as_str()]);
        let out = self.ok("build", &args);
        let files = String::from_utf8_lossy(&out.stdout)
            .lines()
            .map(|line| {
                let (_, path) = line.split_once('\t').unwrap();
                (
                    Path::new(path)
                        .file_name()
                        .unwrap()
                        .to_str()
                        .unwrap()
                        .to_owned(),
                    fs::read_to_string(path).unwrap(),
                )
            })
            .collect();
        (files, errors(&out))
    }
}
fn errors(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).into_owned()
}
fn reference() -> serde_json::Value {
    serde_json::from_str(include_str!(
        "fixtures/java_runtime_classpath/reference.json"
    ))
    .unwrap()
}

#[test]
fn archive_classpath_matches_genuine_orders_identity_types_and_file_filtering() {
    let f = Fixture::new();
    let (files, _) = f.build(&[]);
    let reference = reference();
    let report: serde_json::Value = serde_json::from_str(&files["report.json"]).unwrap();
    assert_eq!(report, reference["outputs"]);
    assert_eq!(
        files["consume.manifest"],
        reference["manifest"].as_str().unwrap()
    );
    // The excluded generated jar has a deliberately failing producer. A
    // successful manifest build proves the returned depset controls inputs.
    assert_eq!(files["consume.manifest"], "alpha\ngenerated\n");
    let out = f.ok("build", &[&format!("//{PACKAGE}:identity")]);
    let stdout = String::from_utf8(out.stdout).unwrap();
    let (_, path) = stdout.trim().split_once('\t').unwrap();
    let identity: serde_json::Value = serde_json::from_slice(&fs::read(path).unwrap()).unwrap();
    assert_eq!(
        identity,
        serde_json::from_str::<serde_json::Value>(include_str!(
            "fixtures/java_runtime_classpath/identity_reference.json"
        ))
        .unwrap()
    );
}

#[test]
fn binding_and_runtime_type_errors_match_genuine_and_private_access_stays_checked() {
    let f = Fixture::new();
    for (case, message) in reference()["errors"].as_object().unwrap() {
        let out = f.run("cquery", &[&format!("//{PACKAGE}:bad_{case}")]);
        assert_eq!(out.status.code(), Some(1), "{case}: {}", errors(&out));
        assert!(out.stdout.is_empty());
        assert!(
            errors(&out).contains(message.as_str().unwrap()),
            "{case}: {}",
            errors(&out)
        );
        assert!(!errors(&out).contains("panicked"));
    }
    let denied = f.run("cquery", &["//:denied"]);
    assert!(!denied.status.success());
    assert!(
        errors(&denied).contains("file '//:denied.bzl' cannot use private API"),
        "{}",
        errors(&denied)
    );
}

#[test]
fn cquery_is_execution_free_and_normal_snapshot_source_flag_and_dependency_changes_work() {
    let f = Fixture::new();
    let marker = f.path().join("status-must-not-run");
    let expression = format!("deps(//{PACKAGE}:consume)");
    let out = f.ok(
        "cquery",
        &[
            &format!("--workspace_status_command=touch {}", marker.display()),
            &expression,
        ],
    );
    assert!(String::from_utf8_lossy(&out.stdout).contains(":poison"));
    assert!(!marker.exists());
    assert!(
        !f.path().join("cache/actions").exists()
            || fs::read_dir(f.path().join("cache/actions"))
                .unwrap()
                .next()
                .is_none()
    );
    f.build(&[]);
    let (_, warm) = f.build(&[]);
    assert!(warm.contains("snapshot: hit"), "{warm}");
    assert!(warm.contains("0 actions executed"), "{warm}");
    fs::write(f.path().join(format!("{PACKAGE}/a.jar")), "edited\n").unwrap();
    let (files, changed) = f.build(&[]);
    assert!(changed.contains("snapshot: hit"), "{changed}");
    assert_eq!(files["consume.manifest"], "edited\ngenerated\n");
    let (files, changed) = f.build(&["--define=CONTENT=two", "--define=EXCLUDE_B=0"]);
    assert!(changed.contains("snapshot: miss"), "{changed}");
    assert_eq!(files["consume.manifest"], "edited\ntwo\nbeta\n");
    let build = f.path().join(format!("{PACKAGE}/BUILD.bazel"));
    let contents = fs::read_to_string(&build)
        .unwrap()
        .replace("generated = \":generated\"", "generated = \":other\"")
        + "\ngenerated(name='other')\n";
    fs::write(build, contents).unwrap();
    let (files, changed) = f.build(&["--define=CONTENT=two", "--define=EXCLUDE_B=0"]);
    assert!(changed.contains("snapshot: miss"), "{changed}");
    assert_eq!(files["consume.manifest"], "edited\ntwo\nbeta\n");
    assert!(files["report.json"].contains("/other.jar"));
    assert!(!files["report.json"].contains("/generated.jar"));
}
