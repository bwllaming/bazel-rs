//! Captured Java native-library directory semantics and ordinary action edges.
#![allow(clippy::disallowed_methods)]
include!("support/temp_root.rs");
use std::collections::BTreeMap;
use std::fs;
use std::path::Path;
use std::process::{Command, Output};

const PACKAGE: &str = "bazel_internal/test_rules/native_deps_dirs";
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
                .join("tests/fixtures/java_native_deps_dirs/workspace"),
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
        "fixtures/java_native_deps_dirs/reference.json"
    ))
    .unwrap()
}

#[test]
fn directories_match_genuine_selection_runfiles_paths_order_and_duplicates() {
    let f = Fixture::new();
    let (files, _) = f.build(&[]);
    let reference = reference();
    let report: serde_json::Value = serde_json::from_str(&files["report.json"]).unwrap();
    assert_eq!(report, reference["outputs"]);
    assert_eq!(
        files["consume.manifest"],
        reference["manifest"].as_str().unwrap()
    );
}

#[test]
fn binding_types_private_access_frozen_lists_and_invalid_selection_fail_truthfully() {
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
    }
    for case in ["invalid_dynamic", "invalid_interface"] {
        let out = f.run("cquery", &[&format!("//{PACKAGE}:bad_{case}")]);
        // Genuine throws IllegalArgumentException and exits37. Native must
        // fail analysis, not crash, ignore the File, or fall back to dynamic.
        assert_eq!(out.status.code(), Some(1), "{}", errors(&out));
        assert!(
            errors(&out).contains(
                reference()["invalid_selection"]["message"]
                    .as_str()
                    .unwrap()
            ),
            "{}",
            errors(&out)
        );
        assert!(!errors(&out).contains("panicked"));
    }
    let out = f.run("cquery", &["//:denied"]);
    assert!(!out.status.success());
    assert!(
        errors(&out).contains("file '//:denied.bzl' cannot use private API"),
        "{}",
        errors(&out)
    );
}

#[test]
fn cquery_is_execution_free_and_consumer_reuses_or_invalidates_normal_snapshots() {
    let f = Fixture::new();
    let marker = f.path().join("status-must-not-run");
    f.ok(
        "cquery",
        &[
            &format!("--workspace_status_command=touch {}", marker.display()),
            &format!("deps(//{PACKAGE}:consume)"),
        ],
    );
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
    fs::write(
        f.path().join(format!("{PACKAGE}/real/libresolved.so")),
        "edited\n",
    )
    .unwrap();
    let (files, changed) = f.build(&[]);
    assert!(changed.contains("snapshot: hit"), "{changed}");
    assert!(files["consume.manifest"].ends_with("edited\ngenerated\n"));
    let flags = ["--define=CONTENT=two", "--define=EXTERNAL=b"];
    let (files, changed) = f.build(&flags);
    assert!(changed.contains("snapshot: miss"), "{changed}");
    assert!(files["consume.manifest"].starts_with("../native_dep_b+/same:"));
    assert!(files["consume.manifest"].ends_with("external b fixture\nedited\ntwo\n"));
    fs::write(
        f.path().join(format!("{PACKAGE}/real/libsecond.so")),
        "second dependency\n",
    )
    .unwrap();
    let build = f.path().join(format!("{PACKAGE}/BUILD.bazel"));
    fs::write(
        &build,
        fs::read_to_string(&build)
            .unwrap()
            .replace("real/libresolved.so", "real/libsecond.so"),
    )
    .unwrap();
    let (files, changed) = f.build(&flags);
    assert!(changed.contains("snapshot: miss"), "{changed}");
    assert!(files["consume.manifest"].ends_with("external b fixture\nsecond dependency\ntwo\n"));
}
