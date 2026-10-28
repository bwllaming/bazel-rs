//! Actionless incompatible configured targets (IncompatibleTargetChecker 9.3).
#![allow(clippy::disallowed_methods)]
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::Path;
use std::process::{Command, Output};
include!("support/temp_root.rs");

struct Workspace(tempfile::TempDir);

impl Workspace {
    fn new() -> Self {
        let ws = Self(tempfile::tempdir_in(crate::test_temp::root()).unwrap());
        ws.write("MODULE.bazel", "module(name='compatibility')\n");
        ws.write("BUILD.bazel", r#"
load(":defs.bzl", "sentinel", "consumer", "flag", "probe")
package(default_visibility = ["//visibility:public"])
constraint_setting(name = "cpu")
constraint_value(name = "host", constraint_setting = ":cpu")
constraint_value(name = "other", constraint_setting = ":cpu")
platform(name = "platform", constraint_values = [":host"])
config_setting(name = "chosen", values = {"define": "choose=yes"})
sentinel(name = "direct", target_compatible_with = [":other"], deps = ["//absent:missing"])
consumer(name = "indirect", deps = [":direct"])
alias(name = "alias", actual = ":direct")
label_flag(name = "label_flag", build_setting_default = ":direct")
label_setting(name = "label_setting", build_setting_default = ":direct")
consumer(name = "flag_consumer", deps = [":label_flag"])
genrule(name = "gen", outs = ["generated"], cmd = "false", target_compatible_with = [":other"])
consumer(name = "output_consumer", deps = [":generated"])
sentinel(name = "selected", target_compatible_with = select({":chosen": [":other"], "//conditions:default": []}), deps = select({":chosen": ["//absent:selected"], "//conditions:default": ["//absent:unselected"]}))
flag(name = "setting", build_setting_default = "yes")
config_setting(name = "flush", flag_values = {":setting": "yes"})
filegroup(name = "flush_consumer", srcs = select({":flush": [], "//conditions:default": []}))
probe(name = "compatible")
"#);
        ws.write("defs.bzl", r#"
P = provider()
def _sentinel(ctx):
    fail("INCOMPATIBLE-IMPLEMENTATION-RAN")
sentinel = rule(implementation = _sentinel, attrs = {"deps": attr.label_list(allow_files = True)})
consumer = rule(implementation = _sentinel, attrs = {"deps": attr.label_list(allow_files = True, providers = [P])})
def _flag(ctx):
    return []
flag = rule(implementation = _flag, build_setting = config.string(flag = True))
def _probe(ctx):
    out = ctx.actions.declare_file(ctx.label.name + ".out")
    ctx.actions.run_shell(outputs = [out], inputs = [ctx.info_file], command = "touch ACTION-RAN; touch " + out.path)
    return [DefaultInfo(files = depset([out]))]
probe = rule(implementation = _probe)
def _aspect(target, ctx):
    fail("INCOMPATIBLE-ASPECT-RAN")
blocked_aspect = aspect(implementation = _aspect, attr_aspects = ["*"], attrs = {"_bad": attr.label(default = "//absent:aspect")})
"#);
        ws
    }
    fn path(&self) -> &Path {
        self.0.path()
    }
    fn write(&self, path: &str, text: &str) {
        let path = self.path().join(path);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, text).unwrap();
    }
    fn command(&self, command: &str) -> Command {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_bazel-rs"));
        cmd.current_dir(self.path())
            .env("BAZEL_RS_SNAPSHOT", "on")
            .env("BAZEL_RS_SNAPSHOT_EXPLAIN", "1")
            .args([
                "--ignore_all_rc_files",
                "--cache-dir=cache",
                command,
                "--experimental-builtin-modules=false",
                "--lockfile_mode=off",
                "--platforms=//:platform",
                "--host_platform=//:platform",
                "--define=choose=yes",
            ])
            .arg(format!(
                "--workspace_status_command=touch {}",
                self.path().join("STATUS-RAN").display()
            ));
        cmd
    }
    fn query(&self, args: &[&str]) -> Output {
        let out = self.command("cquery").args(args).output().unwrap();
        assert!(
            out.status.success(),
            "{args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        for path in ["STATUS-RAN", "ACTION-RAN", "cache/execroot"] {
            assert!(!self.path().join(path).exists(), "unexpected {path}");
        }
        out
    }
    fn append(&self, path: &str, text: &str) {
        let mut previous = fs::read_to_string(self.path().join(path)).unwrap();
        previous.push_str(text);
        self.write(path, &previous);
    }
}

fn labels(out: &Output) -> Vec<String> {
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .map(|line| line.split_whitespace().next().unwrap().to_owned())
        .collect()
}

#[test]
fn direct_and_indirect_results_precede_ordinary_dependencies_and_provider_validation() {
    let ws = Workspace::new();
    let out = ws.query(&["deps(//:indirect)"]);
    let found = labels(&out);
    for label in [
        "//:indirect",
        "//:direct",
        "//:other",
        "//:cpu",
        "//:platform",
    ] {
        assert!(found.contains(&label.to_owned()), "{found:?}");
    }
    assert!(!found.iter().any(|label| label.contains("absent")));
    assert_eq!(labels(&ws.query(&["//:direct"])), ["//:direct"]);
    let wildcard = labels(&ws.query(&["//:all"]));
    assert!(wildcard.contains(&"//:direct".to_owned()));
    assert!(wildcard.contains(&"//:indirect".to_owned()));
}

#[test]
fn aliases_label_settings_and_outputs_retain_graph_identities() {
    let ws = Workspace::new();
    let out =
        ws.query(&["deps(set(//:alias //:flag_consumer //:label_setting //:output_consumer))"]);
    let labels = labels(&out);
    for label in [
        "//:alias",
        "//:direct",
        "//:label_flag",
        "//:flag_consumer",
        "//:label_setting",
        "//:output_consumer",
        "//:generated",
    ] {
        assert!(
            labels.contains(&label.to_owned()),
            "missing {label}: {labels:?}"
        );
    }
    assert!(!labels.contains(&"//:gen".to_owned()));
}

#[test]
fn selected_constraints_keep_configured_attributes_without_analyzing_srcs() {
    let ws = Workspace::new();
    let out = ws.query(&["deps(//:selected)"]);
    assert!(labels(&out).contains(&"//:chosen".to_owned()));
    assert!(!labels(&out).iter().any(|label| label.contains("absent")));
    let out = ws.query(&["--output=proto", "//:selected"]);
    assert!(!out.stdout.is_empty());
    // Exact wire decoding/differential coverage is kept with the reference
    // fixture; these sentinels prove neither branch is analyzed for formatting.
    assert!(
        out.stdout
            .windows(b"//absent:selected".len())
            .any(|w| w == b"//absent:selected")
    );
    // rule_input is declaration-level, unlike configured attributes/edges.
    assert!(
        out.stdout
            .windows(b"//absent:unselected".len())
            .any(|w| w == b"//absent:unselected")
    );
}

#[test]
fn invalid_ordinary_select_is_a_format_error_with_reusable_successful_label_analysis() {
    let ws = Workspace::new();
    let mut build = fs::read_to_string(ws.path().join("BUILD.bazel")).unwrap();
    build.push_str("\nsentinel(name='invalid_select', target_compatible_with=[':other'], deps=select({':chosen': ['//absent:never']}))\n");
    ws.write("BUILD.bazel", &build);
    let run = |format: &str| {
        ws.command("cquery")
            .args(["--define=choose=no", format, "//:invalid_select"])
            .output()
            .unwrap()
    };
    let first = run("--output=label");
    assert!(
        first.status.success(),
        "{}",
        String::from_utf8_lossy(&first.stderr)
    );
    assert_eq!(labels(&first), ["//:invalid_select"]);
    for format in ["--output=proto", "--output=streamed_proto"] {
        let proto = run(format);
        assert!(!proto.status.success());
        assert!(proto.stdout.is_empty());
        let stderr = String::from_utf8_lossy(&proto.stderr);
        assert!(stderr.contains("snapshot: hit"), "{stderr}");
        assert!(
            stderr.contains("cquery cannot format attribute deps"),
            "{stderr}"
        );
    }
    let again = run("--output=label");
    assert!(again.status.success());
    assert_eq!(first.stdout, again.stdout);
}

#[test]
fn pending_flush_and_top_level_aspects_never_run_incompatible_implementations() {
    let ws = Workspace::new();
    ws.query(&["set(//:direct //:flush_consumer //:compatible)"]);
    ws.query(&["--aspects=//:defs.bzl%blocked_aspect", "deps(//:direct)"]);
}

#[test]
fn incompatible_graph_snapshot_reuse_verification_and_constraint_invalidation() {
    let ws = Workspace::new();
    let first = ws.query(&["deps(//:indirect)"]);
    assert!(String::from_utf8_lossy(&first.stderr).contains("snapshot: saved"));
    let warm = ws.query(&["deps(//:indirect)"]);
    assert_eq!(first.stdout, warm.stdout);
    assert!(String::from_utf8_lossy(&warm.stderr).contains("snapshot: hit"));
    let roots = ws.query(&["//:indirect"]);
    assert_eq!(labels(&roots), ["//:indirect"]);
    assert!(String::from_utf8_lossy(&roots.stderr).contains("snapshot: hit"));
    let verify = ws
        .command("cquery")
        .env("BAZEL_RS_SNAPSHOT", "verify")
        .arg("deps(//:indirect)")
        .output()
        .unwrap();
    assert!(
        verify.status.success(),
        "{}",
        String::from_utf8_lossy(&verify.stderr)
    );
    assert_eq!(first.stdout, verify.stdout);
    let build = fs::read_to_string(ws.path().join("BUILD.bazel")).unwrap();
    ws.write(
        "BUILD.bazel",
        &build.replace(
            "constraint_values = [\":host\"]",
            "constraint_values = [\":other\"]",
        ),
    );
    let changed = ws
        .command("cquery")
        .arg("deps(//:indirect)")
        .output()
        .unwrap();
    assert!(!changed.status.success());
    assert!(String::from_utf8_lossy(&changed.stderr).contains("absent"));
    assert!(!String::from_utf8_lossy(&changed.stderr).contains("snapshot: hit"));
}

#[test]
fn build_and_nobuild_reject_explicit_incompatible_roots() {
    let ws = Workspace::new();
    for args in [
        vec!["//:indirect"],
        vec!["--nobuild", "//:direct"],
        vec!["--keep_going", "//:alias"],
    ] {
        let out = ws.command("build").args(args).output().unwrap();
        assert!(!out.status.success());
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(
            stderr.contains("incompatible and cannot be built, but was explicitly requested"),
            "{stderr}"
        );
        assert!(
            !stderr.contains("INCOMPATIBLE-IMPLEMENTATION-RAN"),
            "{stderr}"
        );
    }
}

#[test]
fn label_flag_retains_build_setting_while_normal_consumers_are_incompatible() {
    let ws = Workspace::new();
    ws.append("BUILD.bazel", r#"
config_setting(name = "flag_match", flag_values = {":label_flag": ":direct"})
filegroup(name = "select_flag", srcs = select({":flag_match": [], "//conditions:default": ["//absent:incorrect"]}))
"#);
    let out = ws.query(&["deps(//:select_flag)"]);
    for label in [
        "//:select_flag",
        "//:flag_match",
        "//:label_flag",
        "//:direct",
    ] {
        assert!(labels(&out).contains(&label.to_owned()));
    }
    ws.query(&["deps(//:flag_consumer)"]);
}

#[test]
fn indirect_replacement_precedes_visibility_testonly_and_native_provider_checks() {
    let ws = Workspace::new();
    ws.write(
        "hidden/BUILD.bazel",
        r#"
load("//:defs.bzl", "sentinel", "probe")
sentinel(name = "bad", target_compatible_with = ["//:other"])
probe(name = "private")
"#,
    );
    ws.append(
        "BUILD.bazel",
        r#"
filegroup(name = "private_indirect", srcs = ["//hidden:bad"])
probe(name = "testonly", testonly = True)
filegroup(name = "testonly_indirect", srcs = [":testonly", ":direct"])
filegroup(name = "private_control", srcs = ["//hidden:private"])
filegroup(name = "testonly_control", srcs = [":testonly"])
consumer(name = "provider_control", deps = [":compatible"])
platform(name = "indirect_platform", parents = [":direct"])
"#,
    );
    ws.query(&["deps(set(//:private_indirect //:testonly_indirect //:indirect_platform))"]);
    for (label, expected) in [
        ("//:private_control", "visibility"),
        ("//:testonly_control", "testonly"),
        ("//:provider_control", "mandatory providers"),
    ] {
        let out = ws.command("cquery").arg(label).output().unwrap();
        assert!(!out.status.success());
        assert!(
            String::from_utf8_lossy(&out.stderr).contains(expected),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
}

#[test]
fn aspect_own_incompatible_dependency_has_empty_views_and_required_providers_still_fail() {
    let ws = Workspace::new();
    ws.append("defs.bzl", r#"
def _empty_view(target, ctx):
    d = ctx.attr._bad
    if len(d.files.to_list()) != 0 or len(d[DefaultInfo].default_runfiles.files.to_list()) != 0 or d[DefaultInfo].files_to_run.executable != None:
        fail("BAD-INCOMPATIBLE-DEFAULTS")
    if OutputGroupInfo in d:
        fail("INVENTED-OUTPUT-GROUP")
    print("ASPECT-EMPTY-VIEW", d.label)
    return []
empty_view = aspect(implementation = _empty_view, attrs = {"_bad": attr.label(default = "//:direct")})
required_view = aspect(implementation = _empty_view, attrs = {"_bad": attr.label(default = "//:direct", providers = [P])})
"#);
    let out = ws.query(&["--aspects=//:defs.bzl%empty_view", "//:compatible"]);
    assert!(String::from_utf8_lossy(&out.stderr).contains("ASPECT-EMPTY-VIEW"));
    let out = ws
        .command("cquery")
        .args(["--aspects=//:defs.bzl%required_view", "//:compatible"])
        .output()
        .unwrap();
    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("mandatory providers"), "{stderr}");
    assert!(!stderr.contains("ASPECT-EMPTY-VIEW"));
}

// Field numbers from pinned Bazel 9.3 analysis_v2.proto/build.proto. The
// independent genuine captures used the same schemas (see fixture README).
fn messages(bytes: &[u8], number: u64) -> Vec<&[u8]> {
    bazel_rs::reapi::fields(bytes)
        .unwrap()
        .into_iter()
        .filter_map(|(n, v)| match v {
            bazel_rs::reapi::Value::Bytes(bytes) if n == number => Some(bytes),
            _ => None,
        })
        .collect()
}
fn text(bytes: &[u8], number: u64) -> String {
    String::from_utf8(
        messages(bytes, number)
            .first()
            .copied()
            .unwrap_or_default()
            .to_vec(),
    )
    .unwrap()
}
fn number(bytes: &[u8], number: u64) -> u64 {
    bazel_rs::reapi::fields(bytes)
        .unwrap()
        .into_iter()
        .find_map(|(n, v)| match v {
            bazel_rs::reapi::Value::Varint(value) if n == number => Some(value),
            _ => None,
        })
        .unwrap_or(0)
}
fn frames(mut bytes: &[u8]) -> Vec<&[u8]> {
    let mut frames = Vec::new();
    while !bytes.is_empty() {
        let (mut length, mut shift) = (0, 0);
        loop {
            let byte = bytes[0];
            bytes = &bytes[1..];
            length |= usize::from(byte & 127) << shift;
            if byte < 128 {
                break;
            }
            shift += 7;
            assert!(shift < 64);
        }
        let (frame, rest) = bytes.split_at(length);
        frames.push(frame);
        bytes = rest;
    }
    frames
}

#[test]
fn decoded_proto_and_streamed_proto_preserve_incompatible_edges_and_selected_attributes() {
    let ws = Workspace::new();
    let query = "deps(set(//:selected //:indirect //:alias //:output_consumer))";
    let expected_nodes: BTreeSet<_> = [
        "selected",
        "chosen",
        "other",
        "cpu",
        "indirect",
        "direct",
        "alias",
        "output_consumer",
        "generated",
        "platform",
        "host",
    ]
    .into_iter()
    .map(|name| format!("//:{name}"))
    .collect();
    let expected_edges: BTreeSet<_> = [
        ("selected", "chosen"),
        ("selected", "other"),
        ("direct", "other"),
        ("indirect", "direct"),
        ("indirect", "platform"),
        ("alias", "direct"),
        ("output_consumer", "generated"),
        ("output_consumer", "platform"),
        ("platform", "host"),
        ("host", "cpu"),
        ("other", "cpu"),
    ]
    .into_iter()
    .map(|(a, b)| (format!("//:{a}"), format!("//:{b}")))
    .collect();
    for format in ["--output=proto", "--output=streamed_proto"] {
        let out = ws.query(&[format, "--transitions=lite", query]);
        let frames = if format.ends_with("streamed_proto") {
            frames(&out.stdout)
        } else {
            vec![out.stdout.as_slice()]
        };
        let configs: BTreeMap<_, _> = frames
            .iter()
            .flat_map(|f| messages(f, 2))
            .map(|c| (number(c, 1), text(c, 4)))
            .collect();
        let mut nodes = BTreeSet::new();
        let mut edges = BTreeSet::new();
        let mut selected_attrs = BTreeMap::new();
        for ct in frames.iter().flat_map(|f| messages(f, 1)) {
            let target = messages(ct, 1)[0];
            let kind = number(target, 1);
            let payload = messages(target, if kind == 1 { 2 } else { 4 })[0];
            let label = text(payload, 1);
            let id = number(ct, 3);
            assert!(configs.contains_key(&id));
            nodes.insert(label.clone());
            if kind == 1 {
                for dep in messages(payload, 15) {
                    assert_eq!(text(dep, 2), configs[&number(dep, 3)]);
                    edges.insert((label.clone(), text(dep, 1)));
                }
                if label == "//:selected" {
                    for attr in messages(payload, 4) {
                        selected_attrs.insert(
                            text(attr, 1),
                            messages(attr, 6)
                                .iter()
                                .map(|b| String::from_utf8(b.to_vec()).unwrap())
                                .collect::<Vec<_>>(),
                        );
                    }
                    let inputs: Vec<_> = messages(payload, 5)
                        .iter()
                        .map(|b| String::from_utf8(b.to_vec()).unwrap())
                        .collect();
                    assert!(inputs.contains(&"//absent:unselected".to_owned()));
                }
            } else {
                assert_eq!(kind, 3);
                assert_eq!(text(payload, 2), "//:gen");
            }
        }
        assert_eq!(nodes, expected_nodes);
        assert_eq!(edges, expected_edges);
        assert_eq!(selected_attrs["deps"], ["//absent:selected"]);
    }
}

#[test]
fn split_incoming_and_exec_edges_propagate_only_the_actual_configured_prerequisites() {
    let ws = Workspace::new();
    ws.write(
        "tools/allowlists/function_transition_allowlist/BUILD.bazel",
        "package_group(name='function_transition_allowlist', packages=['//...'])\n",
    );
    ws.append("defs.bzl", r#"
def _split(settings, attr):
    return {"fast": {"//command_line_option:compilation_mode": "fastbuild"}, "opt": {"//command_line_option:compilation_mode": "opt"}}
split = transition(implementation = _split, inputs = [], outputs = ["//command_line_option:compilation_mode"])
def _incoming(settings, attr):
    return {"//command_line_option:compilation_mode": "opt"}
incoming = transition(implementation = _incoming, inputs = [], outputs = ["//command_line_option:compilation_mode"])
def _leaf(ctx):
    return [P()]
leaf = rule(implementation = _leaf)
incoming_leaf = rule(implementation = _sentinel, cfg = incoming, attrs = {"_allowlist_function_transition": attr.label(default = "//tools/allowlists/function_transition_allowlist")})
split_parent = rule(implementation = _sentinel, attrs = {"dep": attr.label(cfg = split), "_allowlist_function_transition": attr.label(default = "//tools/allowlists/function_transition_allowlist")})
exec_parent = rule(implementation = _sentinel, attrs = {"dep": attr.label(cfg = "exec")})
"#);
    ws.append("BUILD.bazel", r#"
load(":defs.bzl", "leaf", "incoming_leaf", "split_parent", "exec_parent")
config_setting(name = "opt", values = {"compilation_mode": "opt"})
leaf(name = "split_leaf", target_compatible_with = select({":opt": [":other"], "//conditions:default": []}))
split_parent(name = "split_parent", dep = ":split_leaf")
incoming_leaf(name = "incoming", target_compatible_with = select({":opt": [":other"], "//conditions:default": []}))
exec_parent(name = "exec_parent", dep = ":direct")
"#);
    let out = ws.query(&["deps(set(//:split_parent //:incoming //:exec_parent //:direct))"]);
    let rows: Vec<_> = String::from_utf8_lossy(&out.stdout)
        .lines()
        .map(str::to_owned)
        .collect();
    for name in ["//:split_leaf", "//:direct"] {
        let configs: BTreeSet<_> = rows
            .iter()
            .filter(|row| row.starts_with(&format!("{name} ")))
            .collect();
        assert_eq!(configs.len(), 2, "{name}: {rows:?}");
    }
    assert!(rows.iter().any(|row| row.starts_with("//:incoming ")));
}

#[test]
fn selected_toolchain_propagates_incompatibility_but_unselected_registration_does_not() {
    let ws = Workspace::new();
    ws.append(
        "defs.bzl",
        r#"
def _tc(ctx):
    return [platform_common.ToolchainInfo()]
tc = rule(implementation = _tc)
tc_parent = rule(implementation = _sentinel, toolchains = ["//:tc_type"])
good_tc_parent = rule(implementation = _flag, toolchains = ["//:tc_type"])
"#,
    );
    ws.append("BUILD.bazel", r#"
load(":defs.bzl", "tc", "tc_parent", "good_tc_parent")
toolchain_type(name = "tc_type")
tc(name = "good_tc")
toolchain(name = "selected_bad", toolchain_type = ":tc_type", toolchain = ":direct")
toolchain(name = "unselected_bad", toolchain_type = ":tc_type", toolchain = ":direct", target_compatible_with = [":other"])
toolchain(name = "selected_good", toolchain_type = ":tc_type", toolchain = ":good_tc")
tc_parent(name = "tc_parent")
good_tc_parent(name = "good_tc_parent")
"#);
    let bad = ws.query(&["--extra_toolchains=//:selected_bad", "deps(//:tc_parent)"]);
    assert!(labels(&bad).contains(&"//:direct".to_owned()));
    let good = ws.query(&[
        "--extra_toolchains=//:unselected_bad,//:selected_good",
        "deps(//:good_tc_parent)",
    ]);
    assert!(!labels(&good).contains(&"//:direct".to_owned()));
    let reg = ws.query(&["deps(//:selected_bad)"]);
    assert_eq!(labels(&reg), ["//:selected_bad", "//:tc_type"]);
}
