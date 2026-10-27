//! Per-config runfiles policy, captured with genuine Bazel 9.3.
#![allow(clippy::disallowed_methods)]
include!("support/temp_root.rs");
use serde_json::Value;
use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
    process::{Command, Output},
};
struct Fixture(tempfile::TempDir);
fn copy(from: &Path, to: &Path) {
    fs::create_dir_all(to).unwrap();
    for e in fs::read_dir(from).unwrap() {
        let e = e.unwrap();
        let dst = to.join(e.file_name());
        if e.file_type().unwrap().is_dir() {
            copy(&e.path(), &dst)
        } else {
            fs::copy(e.path(), dst).unwrap();
        }
    }
}

fn named(root: &Path, name: &str) -> Vec<PathBuf> {
    let mut found = Vec::new();
    if !root.is_dir() {
        return found;
    }
    for entry in fs::read_dir(root).unwrap() {
        let entry = entry.unwrap();
        if entry.file_name() == name {
            found.push(entry.path());
        } else if entry.file_type().unwrap().is_dir() {
            found.extend(named(&entry.path(), name));
        }
    }
    found
}
impl Fixture {
    fn new() -> Self {
        let f = Self(tempfile::tempdir_in(test_temp::root()).unwrap());
        copy(
            &Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("tests/fixtures/runfiles_transitions/workspace"),
            f.path(),
        );
        f.write(
            "MODULE.bazel",
            "module(name='rf_transition')\nregister_execution_platforms('//:p')\n",
        );
        for d in ["home", "tmp"] {
            fs::create_dir(f.path().join(d)).unwrap();
        }
        f
    }
    fn path(&self) -> &Path {
        self.0.path()
    }
    fn write(&self, p: &str, text: &str) {
        let p = self.path().join(p);
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(p, text).unwrap();
    }
    fn cmd(&self, command: &str, args: &[&str]) -> Command {
        let mut c = Command::new(env!("CARGO_BIN_EXE_bazel-rs"));
        c.current_dir(self.path())
            .env_clear()
            .env("PATH", "/usr/bin:/bin")
            .env("HOME", self.path().join("home"))
            .env("TMPDIR", self.path().join("tmp"))
            .env("BAZEL_RS_RESULT_LINES", "1")
            .env("BAZEL_RS_CACHE_GC", "off")
            .env("BAZEL_RS_SNAPSHOT", "on")
            .env("BAZEL_RS_SNAPSHOT_EXPLAIN", "1")
            .args([
                "--ignore_all_rc_files",
                "--cache-dir=cache",
                command,
                "--experimental-builtin-modules=false",
                "--lockfile_mode=off",
                "--memory-limit=2048",
            ]);
        if command != "query" {
            c.args(["--platforms=//:p", "--host_platform=//:p"]);
        }
        c.args(args);
        c
    }
    fn ok(&self, command: &str, args: &[&str]) -> Output {
        let o = self.cmd(command, args).output().unwrap();
        assert!(
            o.status.success(),
            "{command} {args:?}: {}",
            String::from_utf8_lossy(&o.stderr)
        );
        o
    }
    fn built(&self, args: &[&str]) -> Vec<(String, PathBuf)> {
        let o = self.ok("build", args);
        String::from_utf8(o.stdout)
            .unwrap()
            .lines()
            .filter_map(|s| s.split_once('\t'))
            .map(|(l, p)| (l.to_owned(), PathBuf::from(p)))
            .collect()
    }
}
fn views(o: &Output) -> BTreeMap<String, Value> {
    String::from_utf8_lossy(&o.stderr)
        .lines()
        .filter_map(|l| l.split_once("RF_VIEW ").map(|(_, v)| v))
        .map(|s| {
            let (n, j) = s.split_once(' ').unwrap();
            (n.to_owned(), serde_json::from_str(j).unwrap())
        })
        .collect()
}
fn manifest(v: &Value) -> Option<&str> {
    v["files"][0]["manifest"].as_str()
}

#[test]
fn cli_and_transitions_configure_real_manifest_file_identity() {
    let f = Fixture::new();
    let o = f.ok(
        "build",
        &[
            "//:plain", "//:off", "//:on", "//:no", "//:none", "//:exec", "//:split",
        ],
    );
    let v = views(&o);
    assert_eq!(v["plain"]["links_match"], true);
    assert_eq!(v["plain"]["native_match"], true);
    assert_eq!(
        manifest(&v["plain"]),
        Some("bazel-out/k8-fastbuild/bin/exe.sh.runfiles/MANIFEST")
    );
    assert_eq!(manifest(&v["on"]), manifest(&v["plain"]));
    assert_eq!(
        manifest(&v["off"]),
        Some("bazel-out/k8-fastbuild-ST-dacff1feb035/bin/exe.sh.runfiles_manifest")
    );
    assert_eq!(
        manifest(&v["no"]),
        Some("bazel-out/k8-fastbuild-ST-20e3c3d41263/bin/exe.sh.runfiles/MANIFEST")
    );
    assert!(manifest(&v["none"]).is_none());
    assert_eq!(v["split"]["files"].as_array().unwrap().len(), 2);
    assert_ne!(v["split"]["files"][0]["exe"], v["split"]["files"][1]["exe"]);
    for dep in v["split"]["files"].as_array().unwrap() {
        assert_eq!(
            dep["mapping"].as_str().unwrap(),
            format!("{}.repo_mapping", dep["exe"].as_str().unwrap())
        );
    }
    let off = views(&f.ok("build", &["--nobuild_runfile_links", "//:plain", "//:exec"]));
    assert_eq!(off["plain"]["links_match"], false);
    assert_eq!(off["plain"]["native_match"], false);
    assert!(
        manifest(&off["exec"])
            .unwrap()
            .ends_with(".runfiles_manifest")
    );
    assert!(String::from_utf8_lossy(&o.stderr).contains(
        "(\"TriState\", \"<unknown object com.google.devtools.common.options.TriState>\")"
    ));
    let null = views(&f.ok("build", &["//:null_enum"]));
    assert_eq!(
        manifest(&null["null_enum"]),
        Some("bazel-out/k8-fastbuild-ST-7b2c9fc6dc8f/bin/exe.sh.runfiles/MANIFEST")
    );
    for (target, error) in [
        (
            "//:bad",
            "Invalid value type for option 'build_runfile_links'",
        ),
        (
            "//:null",
            "Can not set boolean field com.google.devtools.build.lib.analysis.config.CoreOptions.buildRunfileLinks to null value",
        ),
        (
            "//:bad_enum",
            "Invalid value type for option 'enable_runfiles'",
        ),
        (
            "//:bad_string",
            "OptionsParsingError for option 'build_runfile_links': 'bad' is not a boolean",
        ),
    ] {
        let o = f.cmd("build", &[target]).output().unwrap();
        assert!(!o.status.success());
        assert!(
            String::from_utf8_lossy(&o.stderr).contains(error),
            "{}",
            String::from_utf8_lossy(&o.stderr)
        );
    }
}

#[test]
fn hidden_outputs_tools_incoming_run_and_test_use_their_configuration() {
    let f = Fixture::new();
    f.built(&["//:incoming_off"]);
    let worktrees = f.path().join("cache/worktrees");
    assert!(named(&worktrees, "incoming_off.sh.runfiles").is_empty());
    assert_eq!(
        named(&worktrees, "incoming_off.sh.runfiles_manifest").len(),
        1
    );
    for name in ["//:tool_off", "//:tool_no", "//:aspect_off", "//:aspect_no"] {
        let out = f.built(&[name]);
        assert_eq!(fs::read_to_string(&out[0].1).unwrap(), "runfiles-ok\n");
    }
    for target in ["//:incoming_off", "//:incoming_no"] {
        let out = f.ok("run", &[target]);
        assert!(String::from_utf8_lossy(&out.stdout).contains("runfiles-ok"));
    }
    let out = f.cmd("run", &["//:incoming_none"]).output().unwrap();
    assert_eq!(out.status.code(), Some(2));
    assert!(
        String::from_utf8_lossy(&out.stderr)
            .contains("--nobuild_runfile_manifests is incompatible")
    );
    let test = f.ok(
        "test",
        &[
            "--test_output=all",
            "//:off_test",
            "//:no_test",
            "//:none_test",
        ],
    );
    let logs = format!(
        "{}{}",
        String::from_utf8_lossy(&test.stdout),
        String::from_utf8_lossy(&test.stderr)
    );
    assert!(logs.contains("runfiles-ok manifest_only=1"), "{logs}");
    assert!(logs.contains("runfiles-ok manifest_only=unset"), "{logs}");
    // Same settings survive snapshot replay into the run/test tail.
    let warm = f.ok(
        "test",
        &[
            "--test_output=all",
            "//:off_test",
            "//:no_test",
            "//:none_test",
        ],
    );
    assert!(String::from_utf8_lossy(&warm.stderr).contains("snapshot: hit"));
    let f = Fixture::new();
    f.built(&["--output_groups=default", "//:incoming_off"]);
    assert!(
        named(
            &f.path().join("cache/worktrees"),
            "incoming_off.sh.runfiles_manifest"
        )
        .is_empty()
    );
    assert!(
        named(
            &f.path().join("cache/worktrees"),
            "incoming_off.sh.repo_mapping"
        )
        .is_empty()
    );
    // Fresh --nobuild must not materialize hidden manifests or run tools.
    let f = Fixture::new();
    f.ok("build", &["--nobuild", "//:incoming_off"]);
    assert!(!f.path().join("cache/execroot").exists());
}

#[test]
fn cquery_graph_options_edges_and_snapshots_do_not_execute_actions() {
    let f = Fixture::new();
    let args = [
        "--workspace_status_command=touch STATUS-RAN",
        "deps(//:split)",
    ];
    let cold = f.ok("cquery", &args);
    let warm = f.ok("cquery", &args);
    assert_eq!(cold.stdout, warm.stdout);
    assert!(String::from_utf8_lossy(&warm.stderr).contains("cquery-snapshot: hit"));
    let count = String::from_utf8_lossy(&cold.stdout)
        .lines()
        .filter(|l| l.starts_with("//:exe ("))
        .count();
    assert_eq!(count, 2);
    assert!(!f.path().join("STATUS-RAN").exists());
    assert!(!f.path().join("cache/actions").exists());
    let changed = f.ok("cquery", &["--nobuild_runfile_links", "deps(//:split)"]);
    assert!(!String::from_utf8_lossy(&changed.stderr).contains("cquery-snapshot: hit"));
    let shared = f.ok(
        "cquery",
        &[
            "--workspace_status_command=touch STATUS-RAN",
            "--output=proto",
            "--transitions=lite",
            "--universe_scope=//:split",
            "//:split",
        ],
    );
    assert!(String::from_utf8_lossy(&shared.stderr).contains("cquery-snapshot: hit"));
    // Independent field walk: analysis_v2.proto CqueryResult configs=2,
    // Configuration.fragment_options=7, FragmentOptions.options=2;
    // build.proto Rule.configured_rule_input=15. No new wire fields.
    let configs = messages(&shared.stdout, 2);
    let flags: std::collections::BTreeSet<_> = configs
        .iter()
        .flat_map(|c| messages(c, 7))
        .flat_map(|f| messages(f, 2))
        .filter(|o| text(o, 1) == "build_runfile_links")
        .map(|o| text(o, 2))
        .collect();
    assert!(
        flags.contains("true") && flags.contains("false"),
        "{flags:?}"
    );
    let rules = messages(&shared.stdout, 1)
        .into_iter()
        .flat_map(|ct| messages(ct, 1))
        .flat_map(|t| messages(t, 2))
        .collect::<Vec<_>>();
    let split = rules.iter().find(|r| text(r, 1) == "//:split").unwrap();
    let ids: std::collections::BTreeSet<_> = messages(split, 15)
        .into_iter()
        .filter(|edge| text(edge, 1) == "//:exe")
        .map(|e| number(e, 3))
        .collect();
    assert_eq!(ids.len(), 2);
    let defs = fs::read_to_string(f.path().join("defs.bzl")).unwrap();
    f.write("defs.bzl", &(defs + "\n# edit\n"));
    let edited = f.ok("cquery", &args);
    assert!(String::from_utf8_lossy(&edited.stderr).contains("cquery-snapshot: miss"));
}

#[test]
fn module_alias_reparse_does_not_leave_native_runfiles_side_effects() {
    let f = Fixture::new();
    f.write("flag.bzl","def _impl(ctx):\n    return []\nflag = rule(implementation=_impl,build_setting=config.bool(flag=True))\n");
    let build = fs::read_to_string(f.path().join("BUILD.bazel")).unwrap();
    f.write(
        "BUILD.bazel",
        &(build + "\nload(':flag.bzl','flag')\nflag(name='aliased',build_setting_default=False)\n"),
    );
    f.write("MODULE.bazel","module(name='rf_transition')\nregister_execution_platforms('//:p')\nflag_alias(name='build_runfile_links',starlark_flag='//:aliased')\n");
    let o = f.ok("build", &["--build_runfile_links=false", "//:plain"]);
    assert!(
        manifest(&views(&o)["plain"])
            .unwrap()
            .ends_with(".runfiles/MANIFEST")
    );
    // --noALIAS is not rewritten: it still selects the native option.
    let o = f.ok("build", &["--nobuild_runfile_links", "//:plain"]);
    assert!(
        manifest(&views(&o)["plain"])
            .unwrap()
            .ends_with(".runfiles_manifest")
    );
    // Query may validate native flags but must not enable aliasing of
    // unrelated bootstrap/execution options (existing fail-closed boundary).
    let o = f
        .cmd(
            "query",
            &["--flag_alias=remote_cache=//:aliased", "//:plain"],
        )
        .output()
        .unwrap();
    assert!(!o.status.success());
    f.ok("query", &["--build_runfile_links=false", "//:plain"]);
}

fn messages(bytes: &[u8], number: u64) -> Vec<&[u8]> {
    bazel_rs::reapi::fields(bytes)
        .unwrap()
        .into_iter()
        .filter_map(|(n, v)| match v {
            bazel_rs::reapi::Value::Bytes(b) if n == number => Some(b),
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
fn number(bytes: &[u8], field: u64) -> u64 {
    bazel_rs::reapi::fields(bytes)
        .unwrap()
        .into_iter()
        .find_map(|(n, v)| match v {
            bazel_rs::reapi::Value::Varint(v) if n == field => Some(v),
            _ => None,
        })
        .unwrap_or(0)
}
