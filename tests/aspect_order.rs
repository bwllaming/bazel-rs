//! Aspect dependency discovery must use the same configured declarations as
//! implementation planning, including when config_setting flushes phase 2 early.
#![allow(clippy::disallowed_methods)]
include!("support/temp_root.rs");
include!("support/cquery_current_scope.rs");
use std::{
    collections::BTreeMap,
    fs,
    path::Path,
    process::{Command, Output},
};
include!("support/console.rs");

struct Workspace(tempfile::TempDir);
fn copy(from: &Path, to: &Path) {
    fs::create_dir_all(to).unwrap();
    for entry in fs::read_dir(from).unwrap() {
        let entry = entry.unwrap();
        let destination = to.join(entry.file_name());
        if entry.file_type().unwrap().is_dir() {
            copy(&entry.path(), &destination);
        } else {
            fs::copy(entry.path(), destination).unwrap();
        }
    }
}
impl Workspace {
    fn new() -> Self {
        let ws = Self(
            tempfile::Builder::new()
                .prefix("aspect-order-")
                .tempdir_in(crate::test_temp::root())
                .unwrap(),
        );
        copy(
            &Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/aspect_order/workspace"),
            ws.path(),
        );
        ws
    }
    fn path(&self) -> &Path {
        self.0.path()
    }
    fn command(&self, command: &str, args: &[&str]) -> Command {
        let mut process = Command::new(env!("CARGO_BIN_EXE_bazel-rs"));
        process
            .current_dir(self.path())
            .env("BAZEL_RS_SNAPSHOT", "on")
            .env("BAZEL_RS_SNAPSHOT_EXPLAIN", "1")
            .arg("--ignore_all_rc_files")
            .arg(format!(
                "--cache-dir={}",
                self.path().join("cache").display()
            ))
            .arg(command)
            .args([
                "--experimental-builtin-modules=false",
                "--lockfile_mode=off",
                "--platforms=//:platform",
                "--host_platform=//:platform",
            ])
            .args(args);
        process
    }
    fn ok(&self, command: &str, args: &[&str]) -> Output {
        let output = self.command(command, args).output().unwrap();
        assert!(
            output.status.success(),
            "{command} {args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        output
    }
}
const DEPS: &str = "deps(set(//:early //:twin //:z_late))";

fn markers(output: &Output) -> BTreeMap<String, usize> {
    let mut found = BTreeMap::new();
    for line in String::from_utf8_lossy(&output.stderr).lines() {
        let line = print_text(line);
        if ["RULE:", "BASE:", "MIDDLE:", "CLOSURE:"]
            .iter()
            .any(|prefix| line.starts_with(prefix))
        {
            *found.entry(line.to_owned()).or_default() += 1;
        }
    }
    found
}

#[test]
fn selected_required_aspects_are_analyzed_before_pending_provider_flushes() {
    let ws = Workspace::new();
    let output = ws.ok("build", &["//:early", "//:twin", "//:z_late"]);
    let mut expected = BTreeMap::new();
    for label in ["chosen", "branch"] {
        for kind in ["RULE", "BASE", "MIDDLE", "CLOSURE"] {
            expected.insert(format!("{kind}:{label}"), 1);
        }
    }
    expected.insert("RULE:z_late".to_owned(), 1);
    assert_eq!(markers(&output), expected);
    let mut files = BTreeMap::new();
    for line in String::from_utf8_lossy(&output.stdout).lines() {
        let (label, path) = line.split_once('\t').unwrap();
        if label == "//:early" {
            files.insert(
                Path::new(path)
                    .file_name()
                    .unwrap()
                    .to_str()
                    .unwrap()
                    .to_owned(),
                fs::read_to_string(path).unwrap(),
            );
        }
    }
    let captured: serde_json::Value =
        serde_json::from_str(include_str!("fixtures/aspect_order/genuine.json")).unwrap();
    assert_eq!(serde_json::to_value(files).unwrap(), captured["outputs"]);

    // Reusing the configured graph must not duplicate actions/implementations.
    let warm = ws.ok("build", &["//:early", "//:twin", "//:z_late"]);
    assert!(String::from_utf8_lossy(&warm.stderr).contains("0 actions executed"));
    assert_eq!(markers(&warm), expected);
}

#[test]
fn cquery_records_selected_aspect_edges_and_reuses_and_verifies_the_graph() {
    let ws = Workspace::new();
    let output = ws.ok("cquery", &[DEPS]);
    let labels = String::from_utf8_lossy(&output.stdout);
    for label in [
        "//:chosen (",
        "//:witness.txt (null)",
        "//:selected.txt (null)",
    ] {
        assert!(labels.contains(label), "{labels}");
    }
    assert!(
        !labels.contains("//:ignored (") && !labels.contains("//:unselected.txt"),
        "{labels}"
    );
    assert!(markers(&output).values().all(|count| *count == 1));
    let warm = ws.ok(
        "cquery",
        &[
            "--output=streamed_proto",
            "--transitions=lite",
            "--output_file=roots.pb",
            "set(//:early //:twin //:z_late)",
        ],
    );
    assert_current_loading_scope_miss(&warm, "current-loading history requires empty saved EVENTS");
    assert!(markers(&warm).values().all(|count| *count == 1));
    let filtered = ws.ok("cquery", &["--noinclude_aspects", DEPS]);
    assert!(!String::from_utf8_lossy(&filtered.stdout).contains("//:witness.txt"));
    let verified = ws
        .command("cquery", &[DEPS])
        .env("BAZEL_RS_SNAPSHOT", "verify")
        .output()
        .unwrap();
    assert!(
        verified.status.success(),
        "{}",
        String::from_utf8_lossy(&verified.stderr)
    );
}

#[test]
fn configured_selection_and_source_and_bzl_changes_do_not_reuse_stale_aspect_edges() {
    let ws = Workspace::new();
    let original = ws.ok("cquery", &[DEPS]);
    fs::remove_file(ws.path().join("selected.txt")).unwrap();
    let removed = ws.ok("cquery", &[DEPS]);
    assert!(String::from_utf8_lossy(&removed.stderr).contains("cquery-snapshot: miss"));
    fs::write(ws.path().join("selected.txt"), "restored\n").unwrap();
    let restored = ws.ok("cquery", &[DEPS]);
    assert_current_loading_scope_miss(
        &restored,
        "current-loading history requires empty saved EVENTS",
    );
    assert_eq!(restored.stdout, original.stdout);
    let path = ws.path().join("defs.bzl");
    let source = fs::read_to_string(&path).unwrap();
    fs::write(path, source.replace("//:witness.txt", "//:alternative.txt")).unwrap();
    let edited = ws.ok("cquery", &[DEPS]);
    assert!(String::from_utf8_lossy(&edited.stderr).contains("cquery-snapshot: miss"));
    let labels = String::from_utf8_lossy(&edited.stdout);
    assert!(labels.contains("//:alternative.txt (null)"));
    assert!(!labels.contains("//:witness.txt"));
    let other_config = ws.ok("cquery", &["--//:switch=false", DEPS]);
    let labels = String::from_utf8_lossy(&other_config.stdout);
    assert!(labels.contains("//:ignored ("));
    assert!(!labels.contains("//:chosen ("));
}

#[test]
fn resolved_aspect_edges_keep_incoming_split_and_exec_configurations_separate() {
    let ws = Workspace::new();
    let multi = ws.ok("cquery", &["deps(//:multi)"]);
    let labels = String::from_utf8_lossy(&multi.stdout);
    assert_eq!(
        labels
            .lines()
            .filter(|line| line.starts_with("//:configured ("))
            .count(),
        3,
        "{labels}"
    );
    assert_eq!(
        labels
            .lines()
            .filter(|line| line.starts_with("//:chosen ("))
            .count(),
        1,
        "{labels}"
    );
    assert_eq!(
        labels
            .lines()
            .filter(|line| line.starts_with("//:ignored ("))
            .count(),
        2,
        "{labels}"
    );
    assert_eq!(
        labels
            .lines()
            .filter(|line| *line == "//:witness.txt (null)")
            .count(),
        1,
        "{labels}"
    );
    let incoming = ws.ok("build", &["//:incoming"]);
    let prints = markers(&incoming);
    for kind in ["RULE", "BASE", "MIDDLE", "CLOSURE"] {
        assert_eq!(prints.get(&format!("{kind}:chosen")), Some(&1));
        assert_eq!(prints.get(&format!("{kind}:incoming_branch")), Some(&1));
    }
    assert!(!prints.keys().any(|key| key.ends_with(":ignored")));
}
