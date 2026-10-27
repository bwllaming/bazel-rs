//! Real split configuration analysis, provider views, actions and graph reuse.
#![allow(clippy::disallowed_methods)]
include!("support/temp_root.rs");
use std::{
    fs,
    path::Path,
    process::{Command, Output},
};

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
        let ws = Self(tempfile::tempdir_in(crate::test_temp::root()).unwrap());
        copy(
            &Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("tests/fixtures/split_transitions/workspace"),
            ws.path(),
        );
        ws
    }
    fn path(&self) -> &Path {
        self.0.path()
    }
    fn command(&self, cmd: &str, args: &[&str]) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_bazel-rs"));
        command
            .current_dir(self.path())
            .env("BAZEL_RS_SNAPSHOT", "on")
            .env("BAZEL_RS_SNAPSHOT_EXPLAIN", "1")
            .args(["--ignore_all_rc_files"])
            .arg(format!(
                "--cache-dir={}",
                self.path().join("cache").display()
            ))
            .arg(cmd)
            .args([
                "--experimental-builtin-modules=false",
                "--lockfile_mode=off",
                "--platforms=//:platform",
                "--host_platform=//:platform",
            ])
            .args(args);
        command
    }
    fn ok(&self, cmd: &str, args: &[&str]) -> Output {
        let out = self.command(cmd, args).output().unwrap();
        assert!(
            out.status.success(),
            "{cmd} {args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        out
    }
    fn edit(&self, from: &str, to: &str) {
        let p = self.path().join("defs.bzl");
        let s = fs::read_to_string(&p).unwrap();
        assert!(s.contains(from), "missing {from}");
        fs::write(p, s.replace(from, to)).unwrap();
    }
}
fn summary(out: &Output) -> serde_json::Value {
    let stderr = String::from_utf8_lossy(&out.stderr);
    let line = stderr
        .lines()
        .find_map(|l| l.split_once("SPLIT:").map(|(_, s)| s))
        .unwrap_or_else(|| panic!("no summary: {stderr}"));
    serde_json::from_str(line).unwrap()
}

#[test]
fn named_list_convergence_null_sources_and_aspects_match_genuine_contract() {
    let ws = Workspace::new();
    let out = ws.ok("build", &["//:use"]);
    let actual = summary(&out);
    let captured: serde_json::Value =
        serde_json::from_str(include_str!("fixtures/split_transitions/genuine.json")).unwrap();
    assert_eq!(actual, captured["summary"]);
    let stdout = String::from_utf8_lossy(&out.stdout);
    let proof = stdout
        .lines()
        .find_map(|line| {
            line.split_once('\t')
                .map(|(_, p)| p)
                .filter(|p| p.ends_with(".proof"))
        })
        .unwrap();
    assert_eq!(
        fs::read_to_string(proof).unwrap(),
        captured["proof"].as_str().unwrap()
    );
    let warm = ws.ok("build", &["//:use"]);
    assert!(String::from_utf8_lossy(&warm.stderr).contains("0 actions executed"));
}

#[test]
fn incoming_rule_convergence_uses_the_first_key_of_the_final_configuration() {
    let ws = Workspace::new();
    let out = ws.ok("build", &["//:incoming_use"]);
    let captured: serde_json::Value =
        serde_json::from_str(include_str!("fixtures/split_transitions/genuine.json")).unwrap();
    assert_eq!(summary(&out), captured["incoming_summary"]);
    let stdout = String::from_utf8_lossy(&out.stdout);
    let proof = stdout
        .lines()
        .find_map(|line| {
            line.split_once('\t')
                .map(|(_, path)| path)
                .filter(|path| path.ends_with(".proof"))
        })
        .unwrap();
    assert_eq!(
        fs::read_to_string(proof).unwrap(),
        captured["incoming_proof"].as_str().unwrap()
    );

    // The presentation drops redundant keys, not any distinct configured node.
    // Conversely, initially different branches sharing one final identity must
    // not create duplicate graph nodes after their incoming rule transitions.
    let graph = ws.ok("cquery", &["deps(//:incoming_use)"]);
    let labels = String::from_utf8_lossy(&graph.stdout);
    assert_eq!(
        labels
            .lines()
            .filter(|line| line.starts_with("//:collapsed ("))
            .count(),
        1,
        "{labels}"
    );
    assert_eq!(
        labels
            .lines()
            .filter(|line| line.starts_with("//:collapsed2 ("))
            .count(),
        1,
        "{labels}"
    );
    assert_eq!(
        labels
            .lines()
            .filter(|line| *line == "//:input.txt (null)")
            .count(),
        1,
        "{labels}"
    );
}

#[test]
fn configured_graph_keeps_both_split_branches_and_reuses_the_universe() {
    let ws = Workspace::new();
    let out = ws.ok("cquery", &["deps(//:use)"]);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert_eq!(
        stdout.lines().filter(|l| l.starts_with("//:dep (")).count(),
        4,
        "{stdout}"
    );
    for name in [
        "//:selected.txt (null)",
        "//:other.txt (null)",
        "//:tool-a (null)",
        "//:tool-b (null)",
    ] {
        assert!(stdout.contains(name), "missing {name}: {stdout}");
    }
    let warm = ws.ok(
        "cquery",
        &[
            "--output=streamed_proto",
            "--transitions=lite",
            "--output_file=wire.pb",
            "//:use",
        ],
    );
    assert!(String::from_utf8_lossy(&warm.stderr).contains("cquery-snapshot: hit"));
    let verified = ws
        .command("cquery", &["deps(//:use)"])
        .env("BAZEL_RS_SNAPSHOT", "verify")
        .output()
        .unwrap();
    assert!(
        verified.status.success(),
        "{}",
        String::from_utf8_lossy(&verified.stderr)
    );
    ws.edit("\"//:flag\": \"dbg\"", "\"//:flag\": \"new\"");
    let edited = ws.ok("cquery", &["deps(//:use)"]);
    assert!(String::from_utf8_lossy(&edited.stderr).contains("cquery-snapshot: miss"));
    assert!(
        !summary(&edited)["one"]
            .as_array()
            .unwrap()
            .contains(&serde_json::json!("dbg"))
    );
}

#[test]
fn native_option_splits_retain_native_identity_and_exec_dependencies() {
    let ws = Workspace::new();
    ws.edit(
        "\"//:flag\": \"opt\"",
        "\"//command_line_option:compilation_mode\": \"opt\"",
    );
    ws.edit(
        "\"//:flag\": \"dbg\"",
        "\"//command_line_option:compilation_mode\": \"dbg\"",
    );
    ws.edit(
        "outputs = [\"//:flag\"])\nlisted",
        "outputs = [\"//command_line_option:compilation_mode\"])\nlisted",
    );
    ws.edit("implementation = _list, inputs = [], outputs = [\"//:flag\"]", "implementation = _list, inputs = [], outputs = [\"//command_line_option:compilation_mode\"]");
    ws.edit(
        "mode = ctx.attr._flag[Info].mode",
        "mode = ctx.var[\"COMPILATION_MODE\"]",
    );
    let out = ws.ok("build", &["//:use"]);
    let s = summary(&out);
    assert_eq!(s["one"], serde_json::json!(["dbg", "opt"]));
    assert_eq!(s["one_keys"], serde_json::json!(["a", "z"]));
}

#[test]
fn invalid_later_branch_and_multi_output_rule_transition_fail_closed() {
    let ws = Workspace::new();
    ws.edit("\"a\": {\"//:flag\": \"dbg\"}", "\"a\": {\"//:flag\": 17}");
    let out = ws
        .command("cquery", &["--output_file=bad.pb", "deps(//:use)"])
        .output()
        .unwrap();
    assert!(!out.status.success());
    assert!(!ws.path().join("bad.pb").exists());
    assert!(String::from_utf8_lossy(&out.stderr).contains("expected value of type 'string'"));
    let ws = Workspace::new();
    let p = ws.path().join("defs.bzl");
    let mut defs = fs::read_to_string(&p).unwrap();
    defs.push_str("\nincoming = rule(implementation = _setting, cfg = named, attrs = {\"_allowlist_function_transition\": attr.label(default=\"//tools/allowlists/function_transition_allowlist\")})\n");
    fs::write(p, defs).unwrap();
    let p = ws.path().join("BUILD.bazel");
    let mut build = fs::read_to_string(&p).unwrap();
    build.push_str("\nload(\":defs.bzl\", \"incoming\")\nincoming(name=\"invalid\")\n");
    build = build.replace("load(\":defs.bzl\", \"incoming\")\n", "");
    build = "load(\":defs.bzl\", \"incoming\")\n".to_owned() + &build;
    fs::write(p, build).unwrap();
    let out = ws.command("cquery", &["//:invalid"]).output().unwrap();
    assert!(!out.status.success());
    assert!(
        String::from_utf8_lossy(&out.stderr).contains(
            "Rule transition only allowed to return a single transitioned configuration."
        )
    );
}

#[test]
fn empty_returns_are_noops_single_named_results_keep_keys_and_views_are_frozen() {
    for result in ["None", "{}"] {
        let ws = Workspace::new();
        ws.edit(
            "def _empty(settings, attr):\n    return []",
            &format!("def _empty(settings, attr):\n    return {result}"),
        );
        let out = ws.ok("cquery", &["//:use"]);
        assert_eq!(summary(&out)["empty_keys"], serde_json::json!([null]));
        assert_eq!(summary(&out)["empty"], serde_json::json!(["base"]));
    }
    let ws = Workspace::new();
    ws.edit("return {\"z\": {\"//:flag\": \"opt\"}, \"a\": {\"//:flag\": \"dbg\"}, \"m\": {\"//:flag\": \"opt\"}}", "return ({\"//:flag\": \"opt\"},)");
    let out = ws.ok("cquery", &["//:use"]);
    assert_eq!(summary(&out)["one_keys"], serde_json::json!(["0"]));
    assert_eq!(summary(&out)["one"], serde_json::json!(["opt"]));
    let ws = Workspace::new();
    ws.edit(
        "def _consumer(ctx):",
        "def _consumer(ctx):\n    ctx.split_attr.one[\"z\"] = ctx.split_attr.one[\"a\"]",
    );
    let out = ws.command("cquery", &["//:use"]).output().unwrap();
    assert!(!out.status.success());
    let error = String::from_utf8_lossy(&out.stderr);
    assert!(
        error.contains("frozen") || error.contains("immutable"),
        "{error}"
    );
}
