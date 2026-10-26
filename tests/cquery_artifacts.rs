//! Cquery collects semantic File identities, not execution-path input maps.
#![allow(clippy::disallowed_methods)]
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
        let dest = to.join(entry.file_name());
        if entry.file_type().unwrap().is_dir() {
            copy(&entry.path(), &dest);
        } else {
            fs::copy(entry.path(), dest).unwrap();
        }
    }
}
impl Workspace {
    fn new() -> Self {
        let ws = Self(
            tempfile::Builder::new()
                .prefix("cquery-artifacts-")
                .tempdir_in("/tmp/opencode")
                .unwrap(),
        );
        copy(
            &Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("tests/fixtures/cquery_artifacts/workspace"),
            ws.path(),
        );
        for dir in ["home", "tmp"] {
            fs::create_dir(ws.path().join(dir)).unwrap();
        }
        ws
    }
    fn path(&self) -> &Path {
        self.0.path()
    }
    fn command(&self, command: &str, args: &[&str]) -> Command {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_bazel-rs"));
        cmd.current_dir(self.path())
            .env("HOME", self.path().join("home"))
            .env("TMPDIR", self.path().join("tmp"))
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
        cmd
    }
    fn ok(&self, command: &str, args: &[&str]) -> Output {
        let out = self.command(command, args).output().unwrap();
        assert!(
            out.status.success(),
            "{command} {args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        out
    }
}
const PROBES: &str = "set(//:p_equal //:p_unequal //:p_forwarded //:p_ordered //:p_trees //:p_links //:p_cross_kind)";
fn observations(output: &Output) -> serde_json::Value {
    let mut values = BTreeMap::<String, serde_json::Value>::new();
    for line in String::from_utf8_lossy(&output.stderr).lines() {
        if let Some(text) = print_text(line).strip_prefix("ARTIFACTS:") {
            let (name, json) = text.split_once(':').unwrap();
            assert!(
                values
                    .insert(name.to_owned(), serde_json::from_str(json).unwrap())
                    .is_none(),
                "duplicate probe {name}"
            );
        }
    }
    serde_json::to_value(values).unwrap()
}

#[test]
fn semantic_files_preserve_owners_kinds_order_and_singleton_executable_behavior() {
    let ws = Workspace::new();
    let output = ws.ok("cquery", &[PROBES]);
    let genuine: serde_json::Value =
        serde_json::from_str(include_str!("fixtures/cquery_artifacts/genuine.json")).unwrap();
    assert_eq!(observations(&output), genuine["probes"]);
    assert!(!ws.path().join("cache/actions").exists());
}

#[test]
fn build_and_nobuild_keep_strict_global_conflict_checks_and_equal_action_sharing() {
    let ws = Workspace::new();
    let equal = ws.ok("build", &["//:equal"]);
    assert!(String::from_utf8_lossy(&equal.stderr).contains("1 actions executed"));
    for line in String::from_utf8_lossy(&equal.stdout).lines() {
        let (_, path) = line.split_once('\t').unwrap();
        assert_eq!(fs::read_to_string(path).unwrap(), "same");
    }
    for args in [&["//:unequal"][..], &["--nobuild", "//:unequal"][..]] {
        let bad = ws.command("build", args).output().unwrap();
        assert!(!bad.status.success());
        assert!(
            String::from_utf8_lossy(&bad.stderr).contains("conflicting actions"),
            "{}",
            String::from_utf8_lossy(&bad.stderr)
        );
    }
}

#[test]
fn cquery_still_rejects_invalid_single_rule_artifacts_providers_and_executable_owners() {
    let ws = Workspace::new();
    for (target, fragment) in [
        ("tree_file", "also declared as files"),
        ("orphan", "no generating action"),
        ("duplicate", "conflicting actions"),
        ("bad_provider", "mandatory providers"),
        ("bad_executable", "should be created by the same rule"),
    ] {
        let file = format!("{target}.pb");
        let label = format!("//:{target}");
        let output = ws
            .command(
                "cquery",
                &[
                    "--output=streamed_proto",
                    &format!("--output_file={file}"),
                    &label,
                ],
            )
            .output()
            .unwrap();
        assert!(!output.status.success(), "{target}");
        assert!(
            String::from_utf8_lossy(&output.stderr).contains(fragment),
            "{target}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(
            !ws.path().join(file).exists(),
            "partial output for {target}"
        );
    }
}

#[test]
fn semantic_graph_reuse_verify_and_changed_conflicting_writer_stay_correct() {
    let ws = Workspace::new();
    let first = ws.ok("cquery", &[PROBES]);
    let warm = ws.ok(
        "cquery",
        &[
            "--output=streamed_proto",
            "--transitions=lite",
            "--output_file=targets.pb",
            PROBES,
        ],
    );
    assert!(String::from_utf8_lossy(&warm.stderr).contains("cquery-snapshot: hit"));
    assert_eq!(observations(&first), observations(&warm));
    let verified = ws
        .command("cquery", &[PROBES])
        .env("BAZEL_RS_SNAPSHOT", "verify")
        .output()
        .unwrap();
    assert!(
        verified.status.success(),
        "{}",
        String::from_utf8_lossy(&verified.stderr)
    );
    let build = ws.path().join("BUILD.bazel");
    let source = fs::read_to_string(&build).unwrap();
    fs::write(
        build,
        source.replace(
            "writer(name = \"b\", path = \"same.txt\")",
            "writer(name = \"b\", path = \"same.txt\", content = \"changed\")",
        ),
    )
    .unwrap();
    let edited = ws.ok("cquery", &[PROBES]);
    assert!(String::from_utf8_lossy(&edited.stderr).contains("cquery-snapshot: miss"));
    assert_eq!(observations(&first), observations(&edited));
    let defs = ws.path().join("defs.bzl");
    let source = fs::read_to_string(&defs).unwrap();
    fs::write(defs, source + "\n# implementation closure changed\n").unwrap();
    let edited = ws.ok("cquery", &[PROBES]);
    assert!(String::from_utf8_lossy(&edited.stderr).contains("cquery-snapshot: miss"));
}

#[test]
fn cquery_runs_neither_actions_nor_workspace_status_when_collecting_conflicting_files() {
    let ws = Workspace::new();
    let build = ws.path().join("BUILD.bazel");
    let mut source =
        "load(\":defs.bzl\", \"sentinel\")\n".to_owned() + &fs::read_to_string(&build).unwrap();
    source.push_str(&format!(
        "\nsentinel(name='no_actions',sentinel={:?})\n",
        ws.path().join("ACTION_RAN").to_str().unwrap()
    ));
    fs::write(build, source).unwrap();
    ws.ok(
        "cquery",
        &[
            &format!(
                "--workspace_status_command=touch {}",
                ws.path().join("STATUS_RAN").display()
            ),
            "deps(set(//:p_unequal //:no_actions))",
        ],
    );
    assert!(!ws.path().join("ACTION_RAN").exists());
    assert!(!ws.path().join("STATUS_RAN").exists());
}

#[test]
fn runfiles_support_files_are_semantic_identities_without_manifest_or_tree_lowering() {
    let ws = Workspace::new();
    let genuine: serde_json::Value =
        serde_json::from_str(include_str!("fixtures/cquery_artifacts/genuine.json")).unwrap();
    for (name, flags) in [
        ("default", vec![]),
        ("no-tree", vec!["--enable_runfiles=no"]),
        ("no-manifests", vec!["--nobuild_runfile_manifests"]),
    ] {
        let mut args = flags;
        args.push("//:p_support");
        let output = ws.ok("cquery", &args);
        assert_eq!(observations(&output), genuine["support_probes"][name]);
        let warm = ws.ok("cquery", &args);
        assert!(String::from_utf8_lossy(&warm.stderr).contains("cquery-snapshot: hit"));
        assert_eq!(observations(&warm), genuine["support_probes"][name]);
        let verify = ws
            .command("cquery", &args)
            .env("BAZEL_RS_SNAPSHOT", "verify")
            .output()
            .unwrap();
        assert!(
            verify.status.success(),
            "{}",
            String::from_utf8_lossy(&verify.stderr)
        );
    }
    assert!(!ws.path().join("cache/actions").exists());
}

#[test]
fn predeclared_output_targets_keep_their_generating_owner_despite_dynamic_path_overlap() {
    let genuine: serde_json::Value =
        serde_json::from_str(include_str!("fixtures/cquery_artifacts/genuine.json")).unwrap();
    // Separate analyses are intentional: combining roots can mask the bug by
    // making the predeclared owner happen to register after the runtime writer.
    for name in ["p_predeclared", "p_pre_reverse"] {
        let ws = Workspace::new();
        let output = ws.ok("cquery", &[&format!("//:{name}")]);
        assert_eq!(
            observations(&output)[name],
            genuine["predeclared_probes"][name]
        );
    }
}
