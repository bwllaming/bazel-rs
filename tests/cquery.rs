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
        self.query_command(args).output().unwrap()
    }
    fn query_command(&self, args: &[&str]) -> Command {
        let mut command = self.command(true);
        command
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
            .args(args);
        command
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
    ws.write(
        ".bazelrc",
        "common --incompatible_autoload_externally= --lockfile_mode=off --noshow_progress\n",
    );
    let output = ws
        .command(false)
        .args(["config", "--output=json", "--dump_all"])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn snapshot_verification_source_presence_and_configuration_changes() {
    let ws = Workspace::new();
    ws.ok(&["deps(//:consumer)"]);
    let verified = ws
        .query_command(&["deps(//:consumer)"])
        .env("BAZEL_RS_SNAPSHOT", "verify")
        .output()
        .unwrap();
    assert!(
        verified.status.success(),
        "{}",
        String::from_utf8_lossy(&verified.stderr)
    );
    fs::remove_file(ws.path().join("input")).unwrap();
    let removed = ws.ok(&["deps(//:consumer)"]);
    assert!(String::from_utf8_lossy(&removed.stderr).contains("snapshot: miss"));
    ws.write("input", "restored\n");
    let restored = ws.ok(&["deps(//:consumer)"]);
    assert!(String::from_utf8_lossy(&restored.stderr).contains("snapshot: miss"));
    let changed = ws.ok(&["--define=choice=no", "deps(//:consumer)"]);
    assert!(String::from_utf8_lossy(&changed.stderr).contains("snapshot: miss"));
    assert!(labels(&changed).contains(&"//:other".to_owned()));
}

#[test]
fn graph_snapshot_replay_honors_quiet_without_storing_console_markers_in_output() {
    let ws = Workspace::new();
    let definitions = fs::read_to_string(ws.path().join("defs.bzl")).unwrap();
    ws.write(
        "defs.bzl",
        &format!("{definitions}\nprint('cquery-note-sentinel')\n"),
    );
    let query = |quiet| {
        let mut command = ws.command(true);
        if quiet {
            command.arg("--quiet");
        }
        command
            .args([
                "cquery",
                "--experimental-builtin-modules=false",
                "--lockfile_mode=off",
                "--platforms=//:platform",
                "--host_platform=//:platform",
                "//:selected",
            ])
            .output()
            .unwrap()
    };
    let cold_quiet = query(true);
    assert!(cold_quiet.status.success(), "{cold_quiet:?}");
    let cold_stderr = String::from_utf8_lossy(&cold_quiet.stderr);
    assert!(cold_stderr.contains("snapshot: saved"), "{cold_stderr}");
    assert!(!cold_stderr.contains("cquery-note-sentinel"));
    for quiet in [false, true] {
        let warm = query(quiet);
        assert!(warm.status.success(), "{warm:?}");
        assert_eq!(cold_quiet.stdout, warm.stdout);
        let stderr = String::from_utf8_lossy(&warm.stderr);
        assert!(stderr.contains("snapshot: hit"), "{stderr}");
        assert_eq!(
            stderr.matches("cquery-note-sentinel").count(),
            usize::from(!quiet)
        );
        assert!(!stderr.contains("\u{1b}_bazel-rs:"), "{stderr}");
    }
}

#[test]
fn repository_environment_is_revalidated_on_graph_hits() {
    let ws = Workspace::new();
    ws.write("MODULE.bazel", "module(name='cquery_fixture')\nr=use_repo_rule('//:repo.bzl','repo')\nr(name='external')\n");
    ws.write(
        "repo.bzl",
        r#"
def _impl(ctx):
    ctx.file("BUILD.bazel", "filegroup(name = \"" + ctx.os.environ.get("CQ_NAME", "one") + "\")")
repo = repository_rule(implementation = _impl, environ = ["CQ_NAME"])
"#,
    );
    let query = |value: &str| {
        ws.query_command(&["@external//:all"])
            .env("CQ_NAME", value)
            .output()
            .unwrap()
    };
    let first = query("one");
    assert!(
        first.status.success(),
        "{}",
        String::from_utf8_lossy(&first.stderr)
    );
    let warm = query("one");
    assert!(
        warm.status.success(),
        "{}",
        String::from_utf8_lossy(&warm.stderr)
    );
    assert_eq!(labels(&first), labels(&warm));
    let changed = query("two");
    assert!(
        changed.status.success(),
        "{}",
        String::from_utf8_lossy(&changed.stderr)
    );
    assert!(String::from_utf8_lossy(&changed.stderr).contains("snapshot: miss"));
    assert!(labels(&changed).iter().any(|label| label.ends_with(":two")));
}

#[test]
fn rule_transition_delegates_keep_real_configuration_and_null_package_groups() {
    let ws = Workspace::new();
    let mut defs = fs::read_to_string(ws.path().join("defs.bzl")).unwrap();
    defs.push_str(r#"
def _transition(settings, attr):
    return {"//command_line_option:compilation_mode": "dbg"}
change = transition(implementation = _transition, inputs = [], outputs = ["//command_line_option:compilation_mode"])
transitioned = rule(implementation = _impl, cfg = change, attrs = {"_allowlist_function_transition": attr.label(default = "//tools/allowlists/function_transition_allowlist:function_transition_allowlist")})
"#);
    ws.write("defs.bzl", &defs);
    fs::create_dir_all(
        ws.path()
            .join("tools/allowlists/function_transition_allowlist"),
    )
    .unwrap();
    ws.write(
        "tools/allowlists/function_transition_allowlist/BUILD.bazel",
        "package_group(name='function_transition_allowlist', packages=['public'])\n",
    );
    ws.write(
        "BUILD.bazel",
        &(BUILD.replace(
            "\"custom\", \"bad\"",
            "\"custom\", \"bad\", \"transitioned\"",
        ) + "\ntransitioned(name='transitioned')\n"),
    );
    let output = ws.ok(&["deps(//:transitioned)"]);
    let text = String::from_utf8_lossy(&output.stdout);
    assert!(
        text.contains(
            "//tools/allowlists/function_transition_allowlist:function_transition_allowlist (null)"
        ),
        "{text}"
    );
    assert_eq!(
        labels(&output)
            .iter()
            .filter(|label| *label == "//:transitioned")
            .count(),
        1
    );
}
