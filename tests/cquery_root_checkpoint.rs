//! Serial current-demand checkpoint. Fixtures use the already-pinned toolchain,
//! flag_values/transition semantics in toolchains.rs/test_flags.rs/exec_groups.rs.
#![allow(clippy::disallowed_methods)]
include!("support/temp_root.rs");
use std::{
    fs,
    path::{Path, PathBuf},
    process::{Command, Output},
};
const ROOT: &str = "BAZEL_RS_CQUERY_ROOT_CHECKPOINT";

struct Fixture {
    temp: tempfile::TempDir,
    ws: PathBuf,
    cache: PathBuf,
}
impl Fixture {
    fn new() -> Self {
        let temp = tempfile::tempdir_in(crate::test_temp::root()).unwrap();
        let ws = temp.path().join("ws");
        let cache = temp.path().join("cache");
        fs::create_dir(&ws).unwrap();
        let f = Self { temp, ws, cache };
        let counter =
            serde_json::to_string(f.temp.path().join("local-count").to_str().unwrap()).unwrap();
        let bad =
            serde_json::to_string(f.temp.path().join("unused-count").to_str().unwrap()).unwrap();
        f.write("repos.bzl", &format!(r#"
def _reg(ctx):
    result=ctx.execute(['/bin/sh','-c','echo run >> "$1"; pwd','counter',{counter}])
    if result.stdout.strip() != str(ctx.path('.')): fail('not actual current cwd')
    value=ctx.getenv('ROOT_VALUE','A')
    ctx.file('BUILD.bazel', "toolchain(name='tc',toolchain_type='@//:type',toolchain='@//:impl')\ntoolchain(name='other',toolchain_type='@//:other',toolchain='@unused//:fail')\n# " + value)
def _bad(ctx):
    ctx.execute(['/bin/sh','-c','echo run >> "$1"','counter',{bad}])
    fail('UNSELECTED IMPLEMENTATION EXECUTED')
reg=repository_rule(implementation=_reg,local=True)
bad=repository_rule(implementation=_bad,local=True)
"#));
        f.write("MODULE.bazel", "module(name='cut')\nr=use_repo_rule('//:repos.bzl','reg')\nr(name='reg')\nb=use_repo_rule('//:repos.bzl','bad')\nb(name='unused')\nregister_toolchains('@reg//:all')\nprint('MODULE-PREFIX')\n");
        f.write("MODULE.bazel.lock", "{\"lockFileVersion\":28}\n");
        f.write("defs.bzl", r#"
def _impl(ctx):
    print('PROVIDER-' + ctx.label.name + '-' + ctx.attr.message)
    return [platform_common.ToolchainInfo(value='ok'), DefaultInfo()]
plain=rule(implementation=_impl,attrs={'message':attr.string(),'deps':attr.label_list(allow_files=True)})
typed=rule(implementation=_impl,toolchains=['//:type'],attrs={'message':attr.string(),'deps':attr.label_list()})
grouped=rule(implementation=_impl,exec_groups={'g':exec_group(toolchains=['//:type'])},attrs={'message':attr.string(),'deps':attr.label_list()})
def _flag(ctx):
    print('EARLY-FLAG')
    return [config_common.FeatureFlagInfo(value=ctx.build_setting_value)]
flag=rule(implementation=_flag,build_setting=config.string(flag=True))
def _transition(settings,attr):
    print('CURRENT-TRANSITION')
    return {'//command_line_option:compilation_mode':'opt'}
tr=transition(implementation=_transition,inputs=[],outputs=['//command_line_option:compilation_mode'])
transitioned=rule(implementation=_impl,cfg=tr,toolchains=['//:type'],attrs={'message':attr.string(),'deps':attr.label_list(),'_allowlist_function_transition':attr.label(default='//tools/allowlists/function_transition_allowlist:function_transition_allowlist')})
exec_dep=rule(implementation=_impl,attrs={'message':attr.string(),'deps':attr.label_list(cfg='exec')})
"#);
        f.write(
            "tools/allowlists/function_transition_allowlist/BUILD.bazel",
            "package_group(name='function_transition_allowlist',packages=['public'])\n",
        );
        f.build("A", "typed", "");
        f
    }
    fn write(&self, name: &str, text: &str) {
        let path = self.ws.join(name);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, text).unwrap();
    }
    fn build(&self, version: &str, kind: &str, extra: &str) {
        self.write(
            "BUILD.bazel",
            &format!(
                r#"load(':defs.bzl','plain','typed','grouped','transitioned','flag','exec_dep')
print('BUILD-PREFIX')
package(default_visibility=['//visibility:public'])
package_group(name='allow',packages=['public'])
platform(name='platform')
toolchain_type(name='type')
toolchain_type(name='other')
plain(name='impl')
plain(name='none')
plain(name='00_none')
{kind}(name='a',message='{version}')
plain(name='z',message='{version}',deps=['//warn:old'])
{extra}
"#
            ),
        );
        self.write("warn/BUILD.bazel", "load('//:defs.bzl','plain')\nplain(name='old',deprecation='SUFFIX-WARNING',visibility=['//visibility:public'])\n");
    }
    fn command(&self, expression: &str) -> Command {
        self.command_with_quiet(expression, false)
    }
    fn command_with_quiet(&self, expression: &str, quiet: bool) -> Command {
        let mut c = Command::new(env!("CARGO_BIN_EXE_bazel-rs"));
        c.current_dir(&self.ws)
            .env("PATH", "/usr/bin:/bin")
            .env("BAZEL_RS_MAX_MEMORY_MB", "1024")
            .env_remove("BAZEL_RS_CQUERY_CURRENT_LOADING")
            .env(ROOT, "1")
            .env("BAZEL_RS_SNAPSHOT", "on")
            .env("BAZEL_RS_SNAPSHOT_EXPLAIN", "1")
            .env("ROOT_VALUE", "A")
            .args([
                "--ignore_all_rc_files",
                &format!("--cache-dir={}", self.cache.display()),
            ]);
        if quiet {
            c.arg("--quiet");
        }
        c.args([
            "cquery",
            "--experimental-builtin-modules=false",
            "--lockfile_mode=off",
            "--repository_disable_download",
            "--loading_phase_threads=1",
            "--platforms=//:platform",
            "--host_platform=//:platform",
            "--output=proto",
            expression,
        ]);
        c
    }
    fn run(&self, state: &str) -> Output {
        let out = self.command("deps(//:a + //:z)").output().unwrap();
        let err = String::from_utf8_lossy(&out.stderr);
        assert!(out.status.success(), "{err}");
        assert!(err.contains(&format!("cquery-snapshot: {state}")), "{err}");
        assert_eq!(err.matches("cquery-root: cut ").count(), 1, "{err}");
        assert_eq!(self.count("unused"), 0);
        out
    }
    fn count(&self, name: &str) -> usize {
        fs::read_to_string(self.temp.path().join(format!("{name}-count")))
            .unwrap_or_default()
            .lines()
            .count()
    }
    fn snapshots(&self) -> Vec<PathBuf> {
        fn visit(path: &Path, out: &mut Vec<PathBuf>) {
            if let Ok(entries) = fs::read_dir(path) {
                for entry in entries {
                    let path = entry.unwrap().path();
                    if path.is_dir() {
                        visit(&path, out)
                    } else if path.extension().is_some_and(|e| e == "snap") {
                        out.push(path)
                    }
                }
            }
        }
        let mut out = Vec::new();
        visit(&self.cache, &mut out);
        out
    }
}
fn diagnostics(out: &Output) -> String {
    String::from_utf8(out.stderr.clone())
        .unwrap()
        .lines()
        .filter(|line| {
            !line.starts_with("bazel-rs-cquery-snapshot:")
                && !line.starts_with("bazel-rs-cquery-root:")
        })
        .map(|line| format!("{line}\n"))
        .collect()
}

#[test]
fn serial_cut_loads_only_current_registration_and_replays_nonempty_suffix_on_history_hits() {
    let f = Fixture::new();
    let mut states = Vec::new();
    for version in ["A", "B"] {
        f.build(version, "typed", "");
        let cold = f.run("saved");
        let fresh = f
            .command("deps(//:a + //:z)")
            .env("BAZEL_RS_SNAPSHOT", "off")
            .output()
            .unwrap();
        assert!(
            fresh.status.success(),
            "{}",
            String::from_utf8_lossy(&fresh.stderr)
        );
        let hit = f.run("hit");
        assert_eq!(cold.stdout, fresh.stdout);
        assert_eq!(hit.stdout, fresh.stdout);
        assert_eq!(diagnostics(&cold), diagnostics(&fresh));
        assert_eq!(diagnostics(&hit), diagnostics(&fresh));
        assert_eq!(diagnostics(&hit).matches("SUFFIX-WARNING").count(), 1);
        let diagnostic = diagnostics(&cold);
        states.push((cold.stdout, diagnostic));
    }
    assert_ne!(states[0].0, states[1].0);
    for (index, version) in [(0, "A"), (1, "B"), (0, "A"), (1, "B")] {
        f.build(version, "typed", "");
        let before = f.count("local");
        let out = f.run("hit");
        assert_eq!(out.stdout, states[index].0);
        assert_eq!(diagnostics(&out), states[index].1);
        assert_eq!(f.count("local"), before + 1);
    }
}

#[test]
fn no_types_or_direct_incompatibility_never_demand_registration_only_locals() {
    let f = Fixture::new();
    for expression in ["deps(//:none)", "deps(//:incompatible)"] {
        f.build("A","typed","constraint_setting(name='s')\nconstraint_value(name='v',constraint_setting=':s')\ntyped(name='incompatible',target_compatible_with=[':v'])");
        let fresh = f
            .command(expression)
            .env("BAZEL_RS_SNAPSHOT", "off")
            .output()
            .unwrap();
        let out = f.command(expression).output().unwrap();
        if expression.contains("none") {
            assert!(
                out.status.success(),
                "{}",
                String::from_utf8_lossy(&out.stderr)
            );
        }
        assert_eq!(out.status.code(), fresh.status.code());
        assert_eq!(out.stdout, fresh.stdout);
        assert_eq!(diagnostics(&out), diagnostics(&fresh));
        assert_eq!(f.count("local"), 0);
        assert!(!String::from_utf8_lossy(&out.stderr).contains("cquery-root: cut"));
    }
}

#[test]
fn lower_budgets_and_parallel_decline_without_restart_or_history_decision() {
    let f = Fixture::new();
    f.run("saved");
    for (control, value) in [
        ("BAZEL_RS_CQUERY_ROOT_MAX_MS", "0"),
        ("BAZEL_RS_CQUERY_ROOT_MAX_NODES", "0"),
        ("BAZEL_RS_CQUERY_ROOT_MAX_RSS_MB", "0"),
        ("parallel", "4"),
    ] {
        let before = f.count("local");
        let mut c = f.command("deps(//:a + //:z)");
        if control == "parallel" {
            c.arg("--loading_phase_threads=4");
        } else {
            c.env(control, value);
        }
        let out = c.output().unwrap();
        let err = String::from_utf8_lossy(&out.stderr);
        assert!(out.status.success(), "{err}");
        assert!(err.contains("cquery-root: declined"), "{err}");
        assert!(
            !err.contains("cquery-snapshot: hit")
                && !err.contains("cquery-snapshot: miss")
                && !err.contains("cquery-root: cut"),
            "{err}"
        );
        assert_eq!(f.count("local"), before + 1);
        let fresh = c.env("BAZEL_RS_SNAPSHOT", "off").output().unwrap();
        assert_eq!(out.stdout, fresh.stdout);
        assert_eq!(diagnostics(&out), diagnostics(&fresh));
    }
}

#[test]
fn early_provider_and_rule_transition_are_not_repeated_on_absent_or_corrupt_history() {
    let f = Fixture::new();
    let build=fs::read_to_string(f.ws.join("BUILD.bazel")).unwrap().replace("typed(name='a',message='A')","flag(name='flag',build_setting_default='on')\nconfig_setting(name='on',flag_values={':flag':'on'})\ntransitioned(name='a',message=select({':on':'yes','//conditions:default':'no'}))");
    f.write("BUILD.bazel", &build);
    let cold = f.run("saved");
    let expected = diagnostics(&cold);
    // The rule transition uses both incoming and transitioned configurations;
    // each legitimately computes this provider. Compare the full fresh trace.
    let fresh = f
        .command("deps(//:a + //:z)")
        .env("BAZEL_RS_SNAPSHOT", "off")
        .output()
        .unwrap();
    assert!(fresh.status.success());
    assert_eq!(expected, diagnostics(&fresh));
    assert_eq!(expected.matches("EARLY-FLAG").count(), 2, "{expected}");
    assert!(expected.contains("CURRENT-TRANSITION"));
    let hit = f.run("hit");
    assert_eq!(diagnostics(&hit), expected);
    for path in f.snapshots() {
        fs::write(path, b"corrupt newest").unwrap();
    }
    let before = f.count("local");
    let miss = f.run("saved");
    assert_eq!(diagnostics(&miss), expected);
    assert_eq!(miss.stdout, cold.stdout);
    assert_eq!(f.count("local"), before + 1);
}

#[test]
fn root_order_named_groups_and_exec_configurations_keep_actual_first_registration_identity() {
    for kind in ["typed", "grouped", "transitioned", "exec_dep"] {
        let f = Fixture::new();
        f.build("A", kind, "typed(name='exec_child')");
        if kind == "exec_dep" {
            let text = fs::read_to_string(f.ws.join("BUILD.bazel"))
                .unwrap()
                .replace(
                    "exec_dep(name='a',message='A')",
                    "exec_dep(name='a',message='A',deps=[':exec_child'])",
                );
            f.write("BUILD.bazel", &text);
        }
        let mut outputs = Vec::new();
        for mode in ["on", "off", "on"] {
            let before = f.count("local");
            let out = f
                .command("deps(//:a + //:z)")
                .arg("--universe_scope=//:00_none,//:a,//:z")
                .env("BAZEL_RS_SNAPSHOT", mode)
                .output()
                .unwrap();
            let err = String::from_utf8_lossy(&out.stderr);
            assert!(out.status.success(), "{kind}: {err}");
            assert!(err.contains("cquery-root: cut ordinal=1"), "{kind}: {err}");
            if outputs.len() == 2 {
                assert!(err.contains("cquery-snapshot: hit"), "{err}");
            }
            assert_eq!(f.count("local"), before + 1);
            assert_eq!(f.count("unused"), 0);
            outputs.push(out);
        }
        assert_eq!(outputs[0].stdout, outputs[1].stdout);
        assert_eq!(outputs[2].stdout, outputs[1].stdout);
        assert_eq!(diagnostics(&outputs[0]), diagnostics(&outputs[1]));
        assert_eq!(diagnostics(&outputs[2]), diagnostics(&outputs[1]));
        // Bind the saved marker to the configuration that actually requested
        // collection (the exec child for an exec edge, delegated a otherwise),
        // rather than assuming the invocation's first/top configuration.
        use bazel_rs::snapshot::format::{self, Header, Section};
        let bytes = fs::read(f.snapshots().pop().unwrap()).unwrap();
        let header = Header::decode(&bytes, bytes.len() as u64).unwrap();
        let section = |kind| {
            let s = header.slot(kind).unwrap();
            &bytes[s.offset as usize..(s.offset + s.len) as usize]
        };
        let events = format::decode_events(section(Section::Events)).unwrap();
        let digest = events[16..48]
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect::<String>();
        let graph: bazel_rs::engine::cquery::Graph = postcard::from_bytes(
            &format::decompress_plan(section(Section::Plan).to_vec()).unwrap(),
        )
        .unwrap();
        let caller = if kind == "exec_dep" {
            "//:exec_child"
        } else {
            "//:a"
        };
        assert!(graph.nodes.iter().any(|node| matches!(&node.target,bazel_rs::cquery_proto::Target::Rule(rule) if rule.name==caller)
            && node.configuration.is_some_and(|id|graph.configurations[id as usize].checksum==digest)),"{kind}: caller={caller} registration={digest}");
        if kind == "exec_dep" {
            assert_ne!(digest, graph.top_checksum);
        }
    }
}

#[test]
fn current_semantic_errors_win_before_a_completed_root_can_consult_history() {
    for failure in [
        "transition",
        "select",
        "platform",
        "execgroup",
        "type",
        "registration",
        "after-registration",
        "earlier-root",
    ] {
        let f = Fixture::new();
        f.run("saved");
        let mut build = fs::read_to_string(f.ws.join("BUILD.bazel")).unwrap();
        let mut defs = fs::read_to_string(f.ws.join("defs.bzl")).unwrap();
        let mut module = fs::read_to_string(f.ws.join("MODULE.bazel")).unwrap();
        match failure {
            "transition" => {
                build = build.replace("typed(name='a'", "transitioned(name='a'");
                defs = defs.replace(
                    "return {'//command_line_option:compilation_mode':'opt'}",
                    "fail('bad-current-transition')",
                );
            }
            "select" => build = build.replace("message='A'", "message=select({':missing':'A'})"),
            "platform" => {}
            "execgroup" => {
                build = build.replace("typed(name='a'", "grouped(name='a'");
                defs = defs.replace(
                    "exec_group(toolchains=['//:type'])",
                    "exec_group(toolchains=['//:type'],exec_compatible_with=['//:none'])",
                );
            }
            "type" => defs = defs.replace("toolchains=['//:type']", "toolchains=['//:none']"),
            "registration" => module.push_str("register_toolchains('//:none')\n"),
            "after-registration" => {
                let repos = fs::read_to_string(f.ws.join("repos.bzl"))
                    .unwrap()
                    .replace("toolchain_type='@//:type'", "toolchain_type='@//:other'");
                f.write("repos.bzl", &repos);
            }
            "earlier-root" => {
                defs.push_str("\nbad_early=rule(implementation=_impl,toolchains=['//:none'])\n");
                build = build.replace("load(':defs.bzl',", "load(':defs.bzl','bad_early',");
                build.push_str("bad_early(name='00_early')\n");
            }
            _ => unreachable!(),
        }
        f.write("BUILD.bazel", &build);
        f.write("defs.bzl", &defs);
        f.write("MODULE.bazel", &module);
        let mut outputs = Vec::new();
        for mode in ["off", "on"] {
            let before = f.count("local");
            let mut c = f.command("deps(//:a + //:z)");
            c.env("BAZEL_RS_SNAPSHOT", mode);
            if failure == "platform" {
                c.arg("--platforms=//:none");
            }
            if failure == "earlier-root" {
                c.arg("--universe_scope=//:00_early,//:a,//:z");
            }
            let out = c.output().unwrap();
            let err = String::from_utf8_lossy(&out.stderr);
            assert!(!out.status.success(), "{failure}: {err}");
            assert!(
                !err.contains("cquery-root: cut") && !err.contains("cquery-snapshot: hit"),
                "{failure}: {err}"
            );
            assert_eq!(
                f.count("local"),
                before + usize::from(matches!(failure, "registration" | "after-registration")),
                "{failure}: {err}"
            );
            assert_eq!(f.count("unused"), 0);
            outputs.push(out);
        }
        assert_eq!(outputs[0].status.code(), outputs[1].status.code());
        assert_eq!(outputs[0].stdout, outputs[1].stdout);
        assert_eq!(
            diagnostics(&outputs[0]),
            diagnostics(&outputs[1]),
            "{failure}"
        );
    }
}

#[test]
fn coverage_miss_is_attempted_once_and_later_coverage_never_selects_a_second_cut() {
    let f = Fixture::new();
    let counter =
        serde_json::to_string(f.temp.path().join("later-count").to_str().unwrap()).unwrap();
    let repos = fs::read_to_string(f.ws.join("repos.bzl")).unwrap();
    f.write("repos.bzl",&format!("{repos}\ndef _late(ctx):\n    ctx.execute(['/bin/sh','-c','echo run >> \"$1\"','counter',{counter}])\n    ctx.file('BUILD.bazel',\"exports_files(['data'])\\n\")\n    ctx.file('data','late')\nlate=repository_rule(implementation=_late,local=True)\n"));
    let module = fs::read_to_string(f.ws.join("MODULE.bazel")).unwrap();
    f.write(
        "MODULE.bazel",
        &format!("{module}\nl=use_repo_rule('//:repos.bzl','late')\nl(name='late')\n"),
    );
    let build = fs::read_to_string(f.ws.join("BUILD.bazel"))
        .unwrap()
        .replace("deps=['//warn:old']", "deps=['//warn:old','@late//:data']");
    f.write("BUILD.bazel", &build);
    let cold = f.run("saved");
    for index in 2..=4 {
        let out = f.run("saved");
        let err = String::from_utf8_lossy(&out.stderr);
        assert!(
            err.contains("miss: current loading does not exactly cover"),
            "{err}"
        );
        assert_eq!(err.matches("cquery-snapshot: miss").count(), 1);
        assert_eq!(f.count("local"), index);
        assert_eq!(f.count("later"), index);
        assert_eq!(out.stdout, cold.stdout);
        assert_eq!(diagnostics(&out), diagnostics(&cold));
    }
}

#[test]
fn queued_prefix_warnings_decline_without_early_flush_or_replay() {
    let f = Fixture::new();
    let text = fs::read_to_string(f.ws.join("BUILD.bazel"))
        .unwrap()
        .replace(
            "typed(name='a',message='A')",
            "typed(name='a',message='A',deps=['//warn:old'])",
        );
    f.write("BUILD.bazel", &text);
    let fresh = f
        .command("deps(//:a + //:z)")
        .env("BAZEL_RS_SNAPSHOT", "off")
        .output()
        .unwrap();
    for _ in 0..2 {
        let out = f.command("deps(//:a + //:z)").output().unwrap();
        let err = String::from_utf8_lossy(&out.stderr);
        assert!(out.status.success(), "{err}");
        assert!(err.contains("declined: queued diagnostics"), "{err}");
        assert!(!err.contains("cquery-snapshot: miss") && !err.contains("cquery-snapshot: hit"));
        assert_eq!(out.stdout, fresh.stdout);
        assert_eq!(diagnostics(&out), diagnostics(&fresh));
        assert!(f.snapshots().is_empty());
    }
}

fn rewrite_candidate(source: &Path, destination: &Path, damage: &str) {
    use bazel_rs::snapshot::{
        format::{self, Header, Section},
        key::Key,
    };
    let bytes = fs::read(source).unwrap();
    let header = Header::decode(&bytes, bytes.len() as u64).unwrap();
    let mut parts: Vec<_> = [
        Section::Inputs,
        Section::Events,
        Section::Plan,
        Section::Stats,
    ]
    .into_iter()
    .map(|kind| {
        let s = header.slot(kind).unwrap();
        (
            kind,
            bytes[s.offset as usize..(s.offset + s.len) as usize].to_vec(),
        )
    })
    .collect();
    let mut events = format::decode_events(&parts[1].1).unwrap();
    assert_eq!(&events[..8], b"BZCQRC01");
    assert_eq!(&events[48..56], b"BZCQEV01");
    let prefix = u64::from_le_bytes(events[56..64].try_into().unwrap()) as usize;
    assert!(!String::from_utf8_lossy(&events).contains("bazel-rs-cquery-root:"));
    assert!(!String::from_utf8_lossy(&events).contains("bazel-rs-cquery-snapshot:"));
    if damage != "graph" {
        events.truncate(72 + prefix);
        let poison = b"POISON-SAVED-SUFFIX\n";
        events[64..72].copy_from_slice(&(poison.len() as u64).to_le_bytes());
        events.extend_from_slice(poison);
    }
    match damage {
        "ordinal" => events[8] ^= 1,
        "config" => events[16] ^= 1,
        "kind" => events[7] = b'9',
        "length" => events[56..64].copy_from_slice(&u64::MAX.to_le_bytes()),
        "prefix" => events[72] ^= 1,
        "graph" => {
            let mut graph: bazel_rs::engine::cquery::Graph =
                postcard::from_bytes(&format::decompress_plan(parts[2].1.clone()).unwrap())
                    .unwrap();
            graph.patterns.insert("//:a".into(), vec![u32::MAX]);
            parts[2].1 = format::compress_plan(&postcard::to_allocvec(&graph).unwrap());
        }
        _ => unreachable!(),
    }
    parts[1].1 = format::encode_events(&events);
    let views: Vec<_> = parts
        .iter()
        .map(|(kind, part)| (*kind, part.as_slice()))
        .collect();
    let mut bytes = Header::new(
        &Key {
            g: header.key,
            binary: header.binary,
        },
        header.created_ns + 1,
        &views,
    )
    .encode()
    .to_vec();
    for (_, part) in parts {
        bytes.extend(part);
    }
    fs::write(destination, bytes).unwrap();
}

#[test]
fn bad_newest_cut_identity_or_events_cannot_emit_until_older_candidate_accepts() {
    let f = Fixture::new();
    let cold = f.run("saved");
    let original = f.snapshots().pop().unwrap();
    let newer = original.with_file_name(
        original
            .file_name()
            .unwrap()
            .to_str()
            .unwrap()
            .replace("cq0", "cq1"),
    );
    assert_ne!(original, newer);
    for damage in ["ordinal", "config", "kind", "length", "prefix", "graph"] {
        rewrite_candidate(&original, &newer, damage);
        let before = f.count("local");
        let out = f.run("hit");
        assert_eq!(out.stdout, cold.stdout);
        assert_eq!(diagnostics(&out), diagnostics(&cold));
        assert_eq!(f.count("local"), before + 1);
    }
}

#[test]
fn late_graph_rejection_retains_early_provider_transition_and_local_state_on_live_stack() {
    let f = Fixture::new();
    let build=fs::read_to_string(f.ws.join("BUILD.bazel")).unwrap().replace("typed(name='a',message='A')","flag(name='flag',build_setting_default='on')\nconfig_setting(name='on',flag_values={':flag':'on'})\ntransitioned(name='a',message=select({':on':'yes','//conditions:default':'no'}))");
    f.write("BUILD.bazel", &build);
    let cold = f.run("saved");
    let path = f.snapshots().pop().unwrap();
    rewrite_candidate(&path, &path, "graph");
    let before = f.count("local");
    let miss = f.run("saved");
    let err = String::from_utf8_lossy(&miss.stderr);
    assert!(
        err.contains("miss: invalid configured graph pattern index"),
        "{err}"
    );
    assert_eq!(miss.stdout, cold.stdout);
    assert_eq!(diagnostics(&miss), diagnostics(&cold));
    assert_eq!(f.count("local"), before + 1);
}

#[test]
fn root_profile_is_separate_and_quiet_color_output_verify_preserve_current_policy() {
    let f = Fixture::new();
    let sentinel = f.temp.path().join("ACTION-RAN");
    let status = f.temp.path().join("STATUS-RAN");
    let defs=fs::read_to_string(f.ws.join("defs.bzl")).unwrap().replace("    print('PROVIDER-'",&format!("    ctx.actions.run_shell(outputs=[ctx.actions.declare_file(ctx.label.name+'.out')],command={})\n    print('PROVIDER-'",serde_json::to_string(&format!("touch '{}'",sentinel.display())).unwrap()));
    f.write("defs.bzl", &defs);
    let query = "deps(//:a + //:z)";
    for (quiet, color, output) in [
        (false, "no", "proto"),
        (true, "yes", "proto"),
        (false, "yes", "label"),
    ] {
        let mut results = Vec::new();
        for mode in ["on", "off", "on", "verify"] {
            let before = f.count("local");
            let out = f
                .command_with_quiet(query, quiet)
                .env("BAZEL_RS_SNAPSHOT", mode)
                .args([
                    format!("--color={color}"),
                    format!("--output={output}"),
                    format!("--workspace_status_command=touch {}", status.display()),
                ])
                .output()
                .unwrap();
            let err = String::from_utf8_lossy(&out.stderr);
            assert!(out.status.success(), "{err}");
            if results.len() == 2 {
                assert!(err.contains("cquery-snapshot: hit"), "{err}");
            }
            if mode == "verify" {
                assert!(
                    !err.contains("cquery-snapshot: hit"),
                    "verify ran fresh analysis"
                );
            }
            assert_eq!(f.count("local"), before + 1);
            assert!(!sentinel.exists() && !status.exists());
            results.push(out);
        }
        for out in &results {
            assert_eq!(out.stdout, results[1].stdout);
            assert_eq!(diagnostics(out), diagnostics(&results[1]));
        }
    }
    // Independent keys even when all command options and current inputs match.
    f.run("saved");
    for profile in ["pattern", "default"] {
        let mut c = f.command(query);
        c.env_remove(ROOT);
        if profile == "pattern" {
            c.env("BAZEL_RS_CQUERY_CURRENT_LOADING", "1");
        }
        let out = c.output().unwrap();
        let err = String::from_utf8_lossy(&out.stderr);
        assert!(out.status.success(), "{err}");
        assert!(!err.contains("cquery-snapshot: hit"), "{profile}: {err}");
        assert!(!err.contains("cquery-root: cut"));
    }
    f.run("hit");
}

#[test]
fn root_local_freshness_source_and_module_changes_are_current_work_not_historical_demands() {
    let f = Fixture::new();
    f.run("saved");
    f.run("hit");
    let out = f
        .command("deps(//:a + //:z)")
        .env("ROOT_VALUE", "B")
        .output()
        .unwrap();
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "{err}");
    assert!(err.contains("cquery-snapshot: miss"), "{err}");
    f.run("hit");
    let original = fs::read_to_string(f.ws.join("MODULE.bazel")).unwrap();
    f.write(
        "MODULE.bazel",
        &format!("{original}\nprint('NEW-CURRENT-MODULE')\n"),
    );
    let out = f.run("saved");
    assert_eq!(diagnostics(&out).matches("NEW-CURRENT-MODULE").count(), 1);
    f.write("MODULE.bazel", &original);
    f.run("hit");
    let original = fs::read_to_string(f.ws.join("repos.bzl")).unwrap();
    f.write(
        "repos.bzl",
        &original.replace(
            "value=ctx.getenv",
            "fail('CURRENT-REGISTRATION-FAILED')\n    value=ctx.getenv",
        ),
    );
    let before = f.count("local");
    let out = f.command("deps(//:a + //:z)").output().unwrap();
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(!out.status.success());
    assert!(err.contains("CURRENT-REGISTRATION-FAILED"));
    assert!(!err.contains("cquery-root: cut") && !err.contains("cquery-snapshot: hit"));
    assert_eq!(f.count("local"), before + 1);
    assert_eq!(f.count("unused"), 0);
}

#[test]
fn root_profile_and_admission_controls_are_explicit_and_invalid_values_precede_demand() {
    let f = Fixture::new();
    for (name, value) in [
        (ROOT, "yes"),
        ("BAZEL_RS_CQUERY_ROOT_MAX_MS", "30001"),
        ("BAZEL_RS_CQUERY_ROOT_MAX_NODES", "-1"),
        ("BAZEL_RS_CQUERY_ROOT_MAX_RSS_MB", "1025"),
        ("BAZEL_RS_CQUERY_CURRENT_LOADING", "1"),
    ] {
        let out = f.command("deps(//:a)").env(name, value).output().unwrap();
        assert_eq!(
            out.status.code(),
            Some(2),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        assert!(!String::from_utf8_lossy(&out.stderr).contains("MODULE-PREFIX"));
        assert_eq!(f.count("local"), 0);
    }
    use std::os::unix::ffi::OsStringExt;
    let out = f
        .command("deps(//:a)")
        .env(ROOT, std::ffi::OsString::from_vec(vec![0xff]))
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(2));
    assert_eq!(f.count("local"), 0);
    for value in ["1", "on", "ON"] {
        let out = f.command("deps(//:a)").env(ROOT, value).output().unwrap();
        assert!(out.status.success());
        assert!(String::from_utf8_lossy(&out.stderr).contains("cquery-root: cut"));
    }
    // Other commands ignore this cquery-only native profile, including budgets.
    let out = Command::new(env!("CARGO_BIN_EXE_bazel-rs"))
        .current_dir(&f.ws)
        .env(ROOT, "invalid")
        .args(["--ignore_all_rc_files", "version"])
        .output()
        .unwrap();
    assert!(out.status.success());
}
