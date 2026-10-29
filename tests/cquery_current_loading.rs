//! Explicit current-loading profile: ordinary loading, never historical records,
//! chooses local callbacks. Default regressions explicitly unset the opt-in.
#![allow(clippy::disallowed_methods)]
include!("support/temp_root.rs");
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

struct Fixture {
    temp: tempfile::TempDir,
    root: PathBuf,
    cache: PathBuf,
    bin: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let temp = tempfile::tempdir_in(crate::test_temp::root()).unwrap();
        let root = temp.path().join("ws");
        let cache = temp.path().join("cache");
        let bin = temp.path().join("bin");
        fs::create_dir(&root).unwrap();
        fs::create_dir(&bin).unwrap();
        let f = Self {
            temp,
            root,
            cache,
            bin,
        };
        f.tool("one");
        f.write("MODULE.bazel", "module(name='current_loading')\nl=use_repo_rule('//:repos.bzl','local_maker')\nl(name='defs')\nl(name='unused',bad=True)\nr=use_repo_rule('//:repos.bzl','payload')\nr(name='payload',src='//:.bazelignore')\n");
        f.write("MODULE.bazel.lock", "{\"lockFileVersion\":28}\n");
        f.write("BUILD.bazel", "load('@defs//:defs.bzl','targets')\nplatform(name='platform')\nexports_files(['.bazelignore'])\ntargets()\n");
        f.write(".bazelignore", "unused-a\n");
        let local =
            serde_json::to_string(f.temp.path().join("local-count").to_str().unwrap()).unwrap();
        let nonlocal =
            serde_json::to_string(f.temp.path().join("nonlocal-count").to_str().unwrap()).unwrap();
        let unused =
            serde_json::to_string(f.temp.path().join("unused-count").to_str().unwrap()).unwrap();
        f.write("repos.bzl", &format!(r#"
def _local(ctx):
    if ctx.attr.bad:
        ctx.execute(['/bin/sh','-c','echo bad >> "$1"','counter',{unused}])
        fail('unused local must not execute')
    result = ctx.execute(['/bin/sh','-c','echo run >> "$1"; pwd','counter',{local}])
    if result.return_code or result.stdout.strip() != str(ctx.path('.')):
        fail('not the ordinary real execution cwd')
    tool = ctx.which('probe-tool')
    if not tool:
        fail('current tool missing')
    value = ctx.execute([tool]).stdout.strip() + ctx.getenv('LOCAL_VALUE','one')
    ctx.file('BUILD.bazel', "exports_files(['defs.bzl'])\n")
    ctx.file('defs.bzl', '# ' + value + '\ndef targets():\n    native.filegroup(name="top", srcs=["@payload//:data"])\n')
local_maker = repository_rule(implementation=_local, local=True, attrs={{'bad':attr.bool(default=False)}})
def _payload(ctx):
    text = ctx.read(ctx.attr.src,watch='yes')
    ctx.execute(['/bin/sh','-c','echo run >> "$1"','counter',{nonlocal}])
    ctx.file('BUILD.bazel', "exports_files(['data'])\n")
    ctx.file('data', text)
payload = repository_rule(implementation=_payload, attrs={{'src':attr.label()}})
"#));
        f
    }
    fn write(&self, name: &str, text: &str) {
        fs::write(self.root.join(name), text).unwrap();
    }
    fn tool(&self, text: &str) {
        use std::os::unix::fs::PermissionsExt;
        let path = self.bin.join("probe-tool");
        fs::write(&path, format!("#!/bin/sh\necho {text}\n")).unwrap();
        fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
    }
    fn count(&self, which: &str) -> usize {
        fs::read_to_string(self.temp.path().join(format!("{which}-count")))
            .unwrap_or_default()
            .lines()
            .count()
    }
    fn command(&self) -> Command {
        self.command_with_quiet(false)
    }
    fn command_with_quiet(&self, quiet: bool) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_bazel-rs"));
        command
            .current_dir(&self.root)
            .env("PATH", format!("{}:/usr/bin:/bin", self.bin.display()))
            .env("LOCAL_VALUE", "one")
            .env("BAZEL_RS_MAX_MEMORY_MB", "1024")
            .env("BAZEL_RS_SNAPSHOT", "on")
            .env("BAZEL_RS_CQUERY_CURRENT_LOADING", "1")
            .env("BAZEL_RS_SNAPSHOT_EXPLAIN", "1")
            .args([
                "--ignore_all_rc_files",
                &format!("--cache-dir={}", self.cache.display()),
            ]);
        if quiet {
            command.arg("--quiet");
        }
        command.args([
            "cquery",
            "--experimental-builtin-modules=false",
            "--lockfile_mode=off",
            "--repository_disable_download",
            "--platforms=//:platform",
            "--host_platform=//:platform",
            "--output=proto",
            "deps(//:top)",
        ]);
        command
    }
    fn query(&self, state: &str) -> Vec<u8> {
        let result = self.command().output().unwrap();
        let stderr = String::from_utf8_lossy(&result.stderr);
        assert!(result.status.success(), "{stderr}");
        assert!(
            stderr.contains(&format!("cquery-snapshot: {state}")),
            "{stderr}"
        );
        assert_eq!(self.count("unused"), 0);
        result.stdout
    }
    fn snapshots(&self) -> Vec<PathBuf> {
        fn visit(path: &Path, paths: &mut Vec<PathBuf>) {
            for entry in fs::read_dir(path).unwrap() {
                let entry = entry.unwrap();
                if entry.file_type().unwrap().is_dir() {
                    visit(&entry.path(), paths);
                } else if entry.path().extension().is_some_and(|ext| ext == "snap") {
                    paths.push(entry.path());
                }
            }
        }
        let mut paths = Vec::new();
        visit(&self.cache, &mut paths);
        paths
    }
}

#[test]
fn mixed_history_hits_execute_current_local_once_and_nonlocal_only_for_two_primes() {
    let f = Fixture::new();
    let a = f.query("saved");
    assert_eq!(f.count("local"), 1);
    assert_eq!(f.count("nonlocal"), 1);
    f.write(".bazelignore", "unused-b\n");
    let b = f.query("saved");
    assert_eq!(f.count("local"), 2);
    assert_eq!(f.count("nonlocal"), 2);
    for (index, (text, expected)) in [
        ("unused-a\n", &a),
        ("unused-b\n", &b),
        ("unused-a\n", &a),
        ("unused-b\n", &b),
    ]
    .into_iter()
    .enumerate()
    {
        f.write(".bazelignore", text);
        assert_eq!(&f.query("hit"), expected);
        assert_eq!(f.count("local"), index + 3);
        assert_eq!(f.count("nonlocal"), 2);
        assert_eq!(
            fs::read_to_string(f.cache.join("repositories/external/+payload+payload/data"))
                .unwrap(),
            text
        );
    }
}

#[test]
fn no_history_corrupt_history_and_changed_current_tool_resume_the_completed_local() {
    let f = Fixture::new();
    f.query("saved");
    f.query("hit");
    for path in f.snapshots() {
        fs::write(path, b"corrupt candidate").unwrap();
    }
    f.query("miss");
    assert_eq!(f.count("local"), 3);
    assert_eq!(f.count("nonlocal"), 1);
    for path in f.snapshots() {
        fs::remove_file(path).unwrap();
    }
    f.query("miss");
    assert_eq!(f.count("local"), 4);
    assert_eq!(f.count("nonlocal"), 1);
    f.tool("two");
    f.query("miss");
    assert_eq!(f.count("local"), 5);
    assert_eq!(f.count("nonlocal"), 1);
    let result = f.command().env("LOCAL_VALUE", "two").output().unwrap();
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    assert!(String::from_utf8_lossy(&result.stderr).contains("cquery-snapshot: miss"));
    assert_eq!(f.count("local"), 6);
    assert_eq!(f.count("nonlocal"), 1);
    fs::remove_file(f.bin.join("probe-tool")).unwrap();
    let failure = f.command().output().unwrap();
    assert!(!failure.status.success());
    assert!(String::from_utf8_lossy(&failure.stderr).contains("current tool missing"));
    assert_eq!(f.count("local"), 7);
    assert_eq!(f.count("unused"), 0);
}

#[test]
fn current_prefix_output_is_emitted_once_on_every_fallback() {
    let f = Fixture::new();
    f.write("BUILD.bazel","load('@defs//:defs.bzl','targets')\nprint('current-prefix-note')\nplatform(name='platform')\nexports_files(['.bazelignore'])\ntargets()\n");
    for (index, quiet) in [false, true, false].into_iter().enumerate() {
        let mut command = f.command_with_quiet(quiet);
        let out = command.output().unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert_eq!(
            stderr.matches("current-prefix-note").count(),
            usize::from(!quiet),
            "{stderr}"
        );
        assert!(
            stderr.contains("miss: current loading prefix is not silent and eligible"),
            "{stderr}"
        );
        assert!(!stderr.contains("cquery-snapshot: hit"), "{stderr}");
        assert_eq!(f.count("local"), index + 1);
        assert_eq!(f.count("nonlocal"), 1);
    }
}

#[test]
fn nonempty_saved_analysis_events_are_not_replayed_after_current_loading() {
    let f = Fixture::new();
    // Printing in configured analysis, not in the current loading prefix.
    let marker = f.temp.path().join("analysis-action-ran");
    let command = serde_json::to_string(&format!("touch '{}'", marker.display())).unwrap();
    f.write("analysis.bzl", &format!("def impl(ctx):\n    print('analysis-note')\n    out=ctx.actions.declare_file('never-executed')\n    ctx.actions.run_shell(outputs=[out],command={command})\n    return [DefaultInfo(files=depset([out]))]\nr=rule(implementation=impl)\n"));
    f.write("BUILD.bazel", "load('@defs//:defs.bzl','targets')\nload(':analysis.bzl','r')\nplatform(name='platform')\nexports_files(['.bazelignore'])\nr(name='top')\n");
    for expected in 1..=3 {
        let out = f.command().output().unwrap();
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(out.status.success(), "{stderr}");
        assert!(stderr.contains("cquery-snapshot: miss"), "{stderr}");
        assert_eq!(stderr.matches("analysis-note").count(), 1, "{stderr}");
        assert_eq!(f.count("local"), expected);
        assert_eq!(f.count("nonlocal"), 0);
        assert!(
            !marker.exists(),
            "experimental fallback must not execute actions"
        );
    }
}

#[test]
fn nonlocal_recipe_can_load_only_a_sealed_current_local_producer() {
    let f = Fixture::new();
    let counter =
        serde_json::to_string(f.temp.path().join("nonlocal-count").to_str().unwrap()).unwrap();
    let generated = format!(
        "def impl(ctx):\n    text=ctx.read(ctx.attr.src,watch='yes')\n    ctx.execute(['/bin/sh','-c','echo run >> \"$1\"','counter',{counter}])\n    ctx.file('BUILD.bazel',\"exports_files(['data'])\")\n    ctx.file('data',text)\nremote=repository_rule(implementation=impl,attrs={{'src':attr.label()}})\n"
    );
    let original = fs::read_to_string(f.root.join("repos.bzl")).unwrap();
    f.write(
        "repos.bzl",
        &original.replace(
            "\nlocal_maker =",
            &format!(
                "\n    ctx.file('repo_rules.bzl', {})\nlocal_maker =",
                serde_json::to_string(&generated).unwrap()
            ),
        ),
    );
    let module = fs::read_to_string(f.root.join("MODULE.bazel")).unwrap();
    f.write(
        "MODULE.bazel",
        &module.replace(
            "r=use_repo_rule('//:repos.bzl','payload')",
            "r=use_repo_rule('@defs//:repo_rules.bzl','remote')",
        ),
    );
    let a = f.query("saved");
    f.write(".bazelignore", "unused-b\n");
    let b = f.query("saved");
    for (index, (text, expected)) in [
        ("unused-a\n", &a),
        ("unused-b\n", &b),
        ("unused-a\n", &a),
        ("unused-b\n", &b),
    ]
    .into_iter()
    .enumerate()
    {
        f.write(".bazelignore", text);
        assert_eq!(&f.query("hit"), expected);
        assert_eq!(f.count("local"), index + 3);
        assert_eq!(f.count("nonlocal"), 2);
        assert_eq!(
            fs::read_to_string(f.cache.join("repositories/external/+remote+payload/data")).unwrap(),
            text
        );
    }
    f.tool("changed-current-producer");
    f.query("miss");
    assert_eq!(f.count("local"), 7);
    assert_eq!(f.count("nonlocal"), 2);
}

#[test]
fn extra_historical_local_record_never_demands_the_unused_failing_repository() {
    use bazel_rs::snapshot::{
        format::{self, Header, Section},
        key::Key,
    };
    let f = Fixture::new();
    let expected = f.query("saved");
    for path in f.snapshots() {
        let bytes = fs::read(&path).unwrap();
        let header = Header::decode(&bytes, bytes.len() as u64).unwrap();
        let mut sections: Vec<_> = [
            Section::Inputs,
            Section::Events,
            Section::Plan,
            Section::Stats,
        ]
        .into_iter()
        .map(|kind| {
            let section = header.slot(kind).unwrap();
            (
                kind,
                bytes[section.offset as usize..(section.offset + section.len) as usize].to_vec(),
            )
        })
        .collect();
        let mut inputs = format::decode_inputs(&sections[0].1).unwrap();
        let mut extra = inputs.replay[0].clone();
        extra.canonical = "+local_maker+unused".into();
        inputs.replay.push(extra);
        sections[0].1 = format::encode_inputs(
            &inputs.recording,
            &inputs.replay,
            inputs.tables.as_ref(),
            false,
        );
        let views: Vec<_> = sections
            .iter()
            .map(|(kind, bytes)| (*kind, bytes.as_slice()))
            .collect();
        let mut bytes = Header::new(
            &Key {
                g: header.key,
                binary: header.binary,
            },
            header.created_ns,
            &views,
        )
        .encode()
        .to_vec();
        for (_, data) in sections {
            bytes.extend(data);
        }
        fs::write(path, bytes).unwrap();
    }
    assert_eq!(f.query("miss"), expected);
    assert_eq!(f.count("unused"), 0);
    assert_eq!(f.count("local"), 2);
    assert_eq!(f.count("nonlocal"), 1);
}

#[test]
fn local_dependency_chain_is_current_work_and_a_new_cycle_is_not_hidden_by_history() {
    let f = Fixture::new();
    let source = fs::read_to_string(f.root.join("repos.bzl"))
        .unwrap()
        .replace(
            "    ctx.file('BUILD.bazel',",
            "    if ctx.attr.dep:\n        ctx.read(ctx.attr.dep)\n    ctx.file('BUILD.bazel',",
        )
        .replace(
            "'bad':attr.bool(default=False)",
            "'bad':attr.bool(default=False),'dep':attr.label()",
        )
        .replace(
            "attrs={'src':attr.label()}",
            "attrs={'src':attr.label(),'dep':attr.label()}",
        );
    f.write("repos.bzl", &source);
    let module = fs::read_to_string(f.root.join("MODULE.bazel"))
        .unwrap()
        .replace(
            "l(name='defs')",
            "l(name='leaf')\nl(name='defs',dep='@leaf//:defs.bzl')",
        );
    f.write("MODULE.bazel", &module);
    let graph = f.query("saved");
    assert_eq!(f.count("local"), 2);
    assert_eq!(f.query("hit"), graph);
    assert_eq!(f.count("local"), 4);
    f.write(
        "MODULE.bazel",
        &module.replace("l(name='leaf')", "l(name='leaf',dep='@defs//:defs.bzl')"),
    );
    let out = f.command().output().unwrap();
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(!out.status.success());
    assert!(stderr.contains("cycle"), "{stderr}");
    assert!(!stderr.contains("cquery-snapshot: hit"));
    assert_eq!(f.count("local"), 6);
    assert_eq!(f.count("nonlocal"), 1);
    f.write("MODULE.bazel", &module);
    assert_eq!(f.query("hit"), graph);
    assert_eq!(f.count("local"), 8);
}

#[test]
fn self_referencing_current_local_falls_back_once_without_rebinding_its_real_slot() {
    let f = Fixture::new();
    let source = fs::read_to_string(f.root.join("repos.bzl"))
        .unwrap()
        .replace(
            "\nlocal_maker =",
            "\n    ctx.file('cwd.txt',str(ctx.path('.')))\nlocal_maker =",
        );
    f.write("repos.bzl", &source);
    for count in 1..=3 {
        f.query("miss");
        assert_eq!(f.count("local"), count);
        assert_eq!(f.count("nonlocal"), 1);
        let slot = f.cache.join("repositories/external/+local_maker+defs");
        assert!(fs::symlink_metadata(&slot).unwrap().is_dir());
        assert_eq!(
            fs::read_to_string(slot.join("cwd.txt")).unwrap(),
            slot.to_str().unwrap()
        );
    }
}

#[test]
fn current_path_tool_choice_changes_reject_history_without_a_second_local_execution() {
    use std::os::unix::fs::PermissionsExt;
    let f = Fixture::new();
    f.query("saved");
    let other = f.temp.path().join("other-bin");
    fs::create_dir(&other).unwrap();
    let tool = other.join("probe-tool");
    fs::write(&tool, "#!/bin/sh\necho other-tool\n").unwrap();
    fs::set_permissions(tool, fs::Permissions::from_mode(0o755)).unwrap();
    let out = f
        .command()
        .env("PATH", format!("{}:/usr/bin:/bin", other.display()))
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(String::from_utf8_lossy(&out.stderr).contains("cquery-snapshot: miss"));
    assert_eq!(f.count("local"), 2);
    assert_eq!(f.count("nonlocal"), 1);
}

#[test]
fn cancelling_required_current_loading_cannot_fall_through_to_a_saved_graph() {
    use std::process::Stdio;
    use std::time::Duration;
    let f = Fixture::new();
    f.query("saved");
    let started = f.temp.path().join("tool-started");
    fs::write(
        f.bin.join("probe-tool"),
        format!(
            "#!/bin/sh\necho started > '{}'\nsleep 30\n",
            started.display()
        ),
    )
    .unwrap();
    let mut child = f
        .command()
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    for _ in 0..500 {
        if started.exists() {
            break;
        }
        if let Some(status) = child.try_wait().unwrap() {
            panic!("query skipped required current helper: {status}");
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    if !started.exists() {
        child.kill().unwrap();
        child.wait().unwrap();
        panic!("current helper did not start");
    }
    assert!(
        Command::new("/bin/kill")
            .args(["-INT", &child.id().to_string()])
            .status()
            .unwrap()
            .success()
    );
    let result = child.wait_with_output().unwrap();
    assert!(!result.status.success());
    assert!(!String::from_utf8_lossy(&result.stderr).contains("cquery-snapshot: hit"));
    assert_eq!(f.count("local"), 2);
    assert_eq!(f.count("nonlocal"), 1);
    f.tool("one");
    f.query("hit");
    assert_eq!(f.count("local"), 3);
    assert_eq!(f.count("nonlocal"), 1);
}

const CURRENT_LOADING: &str = "BAZEL_RS_CQUERY_CURRENT_LOADING";

#[test]
fn default_warm_hit_replays_diagnostics_once_without_reloading_the_package_prefix() {
    let f = Fixture::new();
    let build = fs::read_to_string(f.root.join("BUILD.bazel")).unwrap();
    f.write(
        "BUILD.bazel",
        &format!("{build}\nprint('default-prefix-note')\n"),
    );
    let run = |quiet| {
        let mut command = f.command_with_quiet(quiet);
        command
            .env_remove(CURRENT_LOADING)
            .env("BAZEL_RS_TIMING_MODULES", "1");
        let out = command.output().unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        out
    };
    let cold = run(false);
    let cold_stderr = String::from_utf8_lossy(&cold.stderr);
    assert!(
        cold_stderr.contains("cquery-snapshot: saved"),
        "{cold_stderr}"
    );
    assert!(
        cold_stderr.contains("PERF build parse="),
        "cold must actually evaluate BUILD: {cold_stderr}"
    );
    for (index, quiet) in [false, true, false].into_iter().enumerate() {
        let warm = run(quiet);
        let stderr = String::from_utf8_lossy(&warm.stderr);
        assert!(stderr.contains("cquery-snapshot: hit"), "{stderr}");
        assert!(
            !stderr.contains("PERF build parse="),
            "default warm lookup must not evaluate a package: {stderr}"
        );
        assert_eq!(
            stderr.matches("default-prefix-note").count(),
            usize::from(!quiet),
            "{stderr}"
        );
        assert!(!stderr.contains("\u{1b}_bazel-rs:"), "{stderr}");
        assert_eq!(warm.stdout, cold.stdout);
        // The established ordinary local replay runs once; a new prefix plus
        // replay would execute it twice and fail this count.
        assert_eq!(f.count("local"), index + 2);
        assert_eq!(f.count("nonlocal"), 1);
    }
}

#[test]
fn default_analysis_only_local_hit_is_preserved_while_opt_in_refuses_missing_coverage() {
    let f = Fixture::new();
    let module = fs::read_to_string(f.root.join("MODULE.bazel")).unwrap();
    f.write(
        "MODULE.bazel",
        &module.replace("l(name='defs')", "l(name='defs')\nl(name='late')"),
    );
    let source = fs::read_to_string(f.root.join("repos.bzl")).unwrap();
    f.write(
        "repos.bzl",
        &source.replace(
            "srcs=[\"@payload//:data\"]",
            "srcs=[\"@payload//:data\",\"@late//:defs.bzl\"]",
        ),
    );
    let cold = f.command().env_remove(CURRENT_LOADING).output().unwrap();
    assert!(
        cold.status.success(),
        "{}",
        String::from_utf8_lossy(&cold.stderr)
    );
    assert!(String::from_utf8_lossy(&cold.stderr).contains("cquery-snapshot: saved"));
    assert_eq!(f.count("local"), 2);
    let warm = f.command().env_remove(CURRENT_LOADING).output().unwrap();
    assert!(
        warm.status.success(),
        "{}",
        String::from_utf8_lossy(&warm.stderr)
    );
    assert!(String::from_utf8_lossy(&warm.stderr).contains("cquery-snapshot: hit"));
    assert_eq!(warm.stdout, cold.stdout);
    assert_eq!(f.count("local"), 4);
    assert_eq!(f.query("miss"), cold.stdout); // separate opt-in key, cold save
    assert_eq!(f.count("local"), 6);
    let experimental = f.command().output().unwrap();
    let stderr = String::from_utf8_lossy(&experimental.stderr);
    assert!(experimental.status.success(), "{stderr}");
    assert!(
        stderr.contains(
            "miss: current loading does not exactly cover the candidate's local repositories"
        ),
        "{stderr}"
    );
    assert_eq!(experimental.stdout, cold.stdout);
    assert_eq!(
        f.count("local"),
        8,
        "fallback must not replay the completed prefix local"
    );
    assert_eq!(f.count("nonlocal"), 1);
    assert_eq!(f.count("unused"), 0);
}

#[test]
fn explicit_profile_has_separate_history_and_keeps_snapshot_off_and_verify_semantics() {
    let f = Fixture::new();
    let default = f.command().env_remove(CURRENT_LOADING).output().unwrap();
    assert!(
        default.status.success(),
        "{}",
        String::from_utf8_lossy(&default.stderr)
    );
    assert!(String::from_utf8_lossy(&default.stderr).contains("cquery-snapshot: saved"));
    assert_eq!(f.snapshots().len(), 1);
    let opt_in = f.query("miss");
    assert_eq!(opt_in, default.stdout);
    assert_eq!(
        f.snapshots().len(),
        2,
        "default and experimental keys must not alias"
    );
    let warm = f.command().env_remove(CURRENT_LOADING).output().unwrap();
    assert!(
        warm.status.success(),
        "{}",
        String::from_utf8_lossy(&warm.stderr)
    );
    assert!(String::from_utf8_lossy(&warm.stderr).contains("cquery-snapshot: hit"));
    assert_eq!(warm.stdout, default.stdout);
    assert_eq!(f.query("hit"), opt_in);
    let before = f.count("local");
    let off = f
        .command()
        .env("BAZEL_RS_SNAPSHOT", "off")
        .output()
        .unwrap();
    assert!(
        off.status.success(),
        "{}",
        String::from_utf8_lossy(&off.stderr)
    );
    assert!(!String::from_utf8_lossy(&off.stderr).contains("cquery-snapshot:"));
    assert_eq!(off.stdout, opt_in);
    assert_eq!(f.snapshots().len(), 2);
    assert_eq!(f.count("local"), before + 1);
    let verified = f
        .command()
        .env("BAZEL_RS_SNAPSHOT", "verify")
        .output()
        .unwrap();
    assert!(
        verified.status.success(),
        "{}",
        String::from_utf8_lossy(&verified.stderr)
    );
    assert_eq!(verified.stdout, opt_in);
    assert_eq!(f.count("local"), before + 2);
    assert_eq!(f.count("nonlocal"), 1);
}

#[test]
fn invalid_opt_in_is_an_error_before_any_package_or_repository_demand() {
    let f = Fixture::new();
    for mode in ["on", "off", "verify"] {
        let out = f
            .command()
            .env("BAZEL_RS_SNAPSHOT", mode)
            .env(CURRENT_LOADING, "auto")
            .output()
            .unwrap();
        assert_eq!(
            out.status.code(),
            Some(2),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        assert!(
            String::from_utf8_lossy(&out.stderr)
                .contains("BAZEL_RS_CQUERY_CURRENT_LOADING must be")
        );
    }
    use std::os::unix::ffi::OsStringExt;
    let out = f
        .command()
        .env(CURRENT_LOADING, std::ffi::OsString::from_vec(vec![0xff]))
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(2));
    assert_eq!(f.count("local"), 0);
    assert_eq!(f.count("nonlocal"), 0);
    assert_eq!(f.count("unused"), 0);
}
