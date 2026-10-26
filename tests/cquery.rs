//! Configured graph identity, semantic-only analysis and persistent reuse.
#![allow(clippy::disallowed_methods)]
use std::fs;
use std::path::Path;
use std::process::{Command, Output};

struct Workspace(tempfile::TempDir);

const BUILD: &str = r#"
load(":defs.bzl", "custom", "bad")
platform(name = "platform")
config_setting(name = "chosen", values = {"define": "choice=yes"})
custom(name = "top", srcs = select({":chosen": [":selected"], "//conditions:default": [":other"]}), tool = ":tool")
filegroup(name = "tool", srcs = ["input"])
filegroup(name = "selected", srcs = ["input"])
filegroup(name = "other", srcs = ["other-input"])
alias(name = "alias", actual = ":top")
genrule(name = "gen", srcs = [":alias"], outs = ["generated"], cmd = "touch GEN-RAN; cat $(SRCS) > $@")
filegroup(name = "consumer", srcs = [":generated"])
bad(name = "bad")
"#;

impl Workspace {
    fn new() -> Self {
        let ws = Self(tempfile::tempdir().unwrap());
        ws.write("MODULE.bazel", "module(name='cquery_fixture')\n");
        ws.write("BUILD.bazel", BUILD);
        ws.write("leaf.bzl", "VALUE = 'one'\n");
        ws.write("defs.bzl", r#"
load(":leaf.bzl", "VALUE")
def _impl(ctx):
    out = ctx.actions.declare_file(ctx.label.name + ".txt")
    ctx.actions.run_shell(outputs = [out], inputs = [ctx.info_file], command = "touch ACTION-RAN; echo " + VALUE + " > " + out.path)
    return [DefaultInfo(files = depset([out]))]
custom = rule(implementation = _impl, attrs = {"srcs": attr.label_list(allow_files = True), "message": attr.string(default = "default"), "tool": attr.label(cfg = "exec", allow_files = True)})
def _bad(ctx):
    fail("SEMANTIC-ERROR-SENTINEL")
bad = rule(implementation = _bad)
"#);
        ws.write("input", "input\n");
        ws.write("other-input", "other\n");
        ws
    }

    fn path(&self) -> &Path {
        self.0.path()
    }
    fn write(&self, path: &str, text: &str) {
        fs::write(self.path().join(path), text).unwrap();
    }
    fn command(&self, ignore_rc: bool) -> Command {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_bazel-rs"));
        cmd.current_dir(self.path())
            .env("BAZEL_RS_SNAPSHOT", "on")
            .env("BAZEL_RS_SNAPSHOT_EXPLAIN", "1")
            .env_remove("BAZEL_RS_DIGEST_MEMO");
        if ignore_rc {
            cmd.arg("--ignore_all_rc_files");
        } else {
            cmd.args(["--nosystem_rc", "--nohome_rc"]);
        }
        cmd.arg(format!(
            "--cache-dir={}",
            self.path().join("cache").display()
        ));
        cmd
    }
    fn query(&self, args: &[&str]) -> Output {
        self.command(true)
            .arg("cquery")
            .args([
                "--experimental-builtin-modules=false",
                "--lockfile_mode=off",
                "--platforms=//:platform",
                "--host_platform=//:platform",
                "--define=choice=yes",
            ])
            .arg(format!(
                "--workspace_status_command=touch {}",
                self.path().join("STATUS-RAN").display()
            ))
            .args(args)
            .output()
            .unwrap()
    }
    fn ok(&self, args: &[&str]) -> Output {
        let out = self.query(args);
        assert!(
            out.status.success(),
            "{:?}: {}",
            args,
            String::from_utf8_lossy(&out.stderr)
        );
        out
    }
}

fn labels(output: &Output) -> Vec<String> {
    String::from_utf8(output.stdout.clone())
        .unwrap()
        .lines()
        .map(|l| l.split(' ').next().unwrap().to_owned())
        .collect()
}

#[test]
fn configured_analysis_runs_providers_but_not_actions_or_status() {
    let ws = Workspace::new();
    let out = ws.ok(&["deps(//:consumer)"]);
    let labels = labels(&out);
    for label in [
        "//:consumer",
        "//:gen",
        "//:generated",
        "//:alias",
        "//:top",
        "//:chosen",
        "//:selected",
        "//:tool",
        "//:input",
        "//:platform",
    ] {
        assert!(
            labels.contains(&label.to_owned()),
            "missing {label}: {labels:?}"
        );
    }
    assert!(!labels.contains(&"//:other".to_owned()));
    assert!(String::from_utf8_lossy(&out.stdout).contains("//:input (null)"));
    for sentinel in ["STATUS-RAN", "ACTION-RAN", "GEN-RAN"] {
        assert!(!ws.path().join(sentinel).exists());
    }
    assert!(!ws.path().join("cache/execroot").exists());
    let out = ws.query(&["//:bad"]);
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("SEMANTIC-ERROR-SENTINEL"));
}

#[test]
fn configured_sets_keep_exec_identity_and_filter_edges() {
    let ws = Workspace::new();
    let out = ws.ok(&["--universe_scope=//:top,//:tool", "//:tool"]);
    assert_eq!(labels(&out), ["//:tool", "//:tool"]);
    let out = ws.ok(&["--universe_scope=//:top,//:tool", "config(//:tool, target)"]);
    assert_eq!(labels(&out), ["//:tool"]);
    let out = ws.ok(&["config(deps(//:top), null)"]);
    assert_eq!(labels(&out), ["//:input"]);
    let out = ws.ok(&["--notool_deps", "deps(//:top)"]);
    assert!(!labels(&out).contains(&"//:tool".to_owned()));
    let out = ws.ok(&["--noimplicit_deps", "deps(//:top)"]);
    assert!(!labels(&out).contains(&"//:chosen".to_owned()));
    let out = ws.ok(&["kind(alias, deps(//:consumer))"]);
    assert_eq!(labels(&out), ["//:alias"]);
    let out = ws.query(&["--output=starlark", "//:top"]);
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("not implemented"));
}

#[test]
fn binary_snapshot_reuses_universe_and_invalidates_loading_inputs() {
    let ws = Workspace::new();
    let first = ws.ok(&["deps(//:consumer)"]);
    assert!(String::from_utf8_lossy(&first.stderr).contains("snapshot: saved"));
    let second = ws.ok(&[
        "--output=streamed_proto",
        "--output_file=targets.pb",
        "//:consumer",
    ]);
    assert!(second.stdout.is_empty());
    assert!(String::from_utf8_lossy(&second.stderr).contains("snapshot: hit"));
    assert!(fs::metadata(ws.path().join("targets.pb")).unwrap().len() > 0);
    ws.write("leaf.bzl", "VALUE = 'changed'\n");
    let edited = ws.ok(&["deps(//:consumer)"]);
    assert!(String::from_utf8_lossy(&edited.stderr).contains("snapshot: miss"));
    ws.write(
        "BUILD.bazel",
        &BUILD.replace("[\":selected\"]", "[\":other\"]"),
    );
    let edited = ws.ok(&["deps(//:consumer)"]);
    assert!(String::from_utf8_lossy(&edited.stderr).contains("snapshot: miss"));
    assert!(labels(&edited).contains(&"//:other".to_owned()));
    assert!(!labels(&edited).contains(&"//:selected".to_owned()));
    let output = ws
        .command(true)
        .args(["config", "--output=json", "--dump_all"])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let configs: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    let configs = configs.as_array().unwrap();
    assert!(configs.len() >= 2);
    assert!(configs.iter().any(|c| c["isExec"] == true));
    assert!(configs.iter().all(|c| {
        c["fragmentOptions"][0]["options"]
            .as_object()
            .unwrap()
            .contains_key("compilation_mode")
    }));
}

#[test]
fn cquery_inherits_build_and_test_rc_configuration() {
    let ws = Workspace::new();
    ws.write(
        ".bazelrc",
        "build --define=choice=yes\ntest --compilation_mode=dbg\ncquery --output=label_kind\n",
    );
    let output = ws
        .command(false)
        .args([
            "cquery",
            "--experimental-builtin-modules=false",
            "--lockfile_mode=off",
            "--platforms=//:platform",
            "--host_platform=//:platform",
            "deps(//:top)",
        ])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let text = String::from_utf8_lossy(&output.stdout);
    assert!(text.contains("filegroup rule //:selected"), "{text}");
    assert!(!text.contains("//:other"));
}
