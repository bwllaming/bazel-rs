//! Native startup lockfile isolation, including TD's unadorned mod companion.
#![allow(clippy::disallowed_methods)]
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
include!("support/temp_root.rs");

// Valid but deliberately stale: even a local graph must update this value.
const CANARY: &str = "{\"lockFileVersion\":28,\"registryFileHashes\":{\"https://stale.invalid/MODULE.bazel\":\"not found\"}}\n";

struct Fixture {
    temp: tempfile::TempDir,
    root: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let temp = tempfile::Builder::new()
            .prefix("lock-start-")
            .tempdir_in(crate::test_temp::root())
            .unwrap();
        let root = temp.path().join("ws");
        fs::create_dir(&root).unwrap();
        fs::create_dir(temp.path().join("home")).unwrap();
        fs::write(root.join("MODULE.bazel"), "module(name='isolation')\ne = use_extension('//:ext.bzl', 'ext')\nuse_repo(e, 'generated')\n").unwrap();
        fs::write(root.join("ext.bzl"), "def _repo(ctx):\n    fail('must not materialize')\nr = repository_rule(implementation=_repo)\ndef _ext(ctx):\n    r(name='generated')\next = module_extension(implementation=_ext)\n").unwrap();
        fs::write(root.join("defs.bzl"), "def _impl(ctx):\n    out = ctx.actions.declare_file(ctx.label.name + '.sh')\n    ctx.actions.write(out, '#!/bin/sh\\nexit 0\\n', is_executable=True)\n    return [DefaultInfo(executable=out)]\nsmall_test = rule(implementation=_impl, test=True)\n").unwrap();
        fs::write(root.join("BUILD.bazel"), "load(':defs.bzl', 'small_test')\nplatform(name='platform')\nfilegroup(name='leaf', srcs=['input'])\nsmall_test(name='small_test')\n").unwrap();
        fs::write(root.join("input"), "one\n").unwrap();
        fs::write(root.join("MODULE.bazel.lock"), CANARY).unwrap();
        Self { temp, root }
    }

    fn path(&self, name: &str) -> PathBuf {
        self.temp.path().join(name)
    }

    fn flag(&self, name: &str) -> String {
        format!("--native-lockfile={}", self.path(name).display())
    }

    fn run(&self, startup: &[&str], args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_bazel-rs"))
            .args(["--nosystem_rc", "--nohome_rc"])
            .arg(format!("--cache-dir={}", self.path("cache").display()))
            .args(startup)
            .args(args)
            .current_dir(&self.root)
            .env_clear()
            .env("HOME", self.path("home"))
            .env("TMPDIR", self.temp.path())
            .env("PATH", "/usr/bin:/bin")
            .env("BAZEL_RS_MAX_MEMORY_MB", "8192")
            .env("BAZEL_RS_SNAPSHOT_EXPLAIN", "1")
            .output()
            .unwrap()
    }

    fn ok(&self, startup: &[&str], args: &[&str]) -> Output {
        let out = self.run(startup, args);
        assert!(out.status.success(), "{args:?}: {out:?}");
        out
    }

    fn unchanged(&self) {
        assert_eq!(
            fs::read_to_string(self.root.join("MODULE.bazel.lock")).unwrap(),
            CANARY
        );
    }
}

fn updated(path: &Path) {
    let text = fs::read_to_string(path).unwrap();
    let value: serde_json::Value = serde_json::from_str(&text).unwrap();
    assert_eq!(value["lockFileVersion"], 28);
    assert!(!text.contains("stale.invalid"), "{text}");
}

#[test]
fn startup_protects_mod_and_every_module_loading_command() {
    for command in ["mod", "query", "cquery", "build", "test", "coverage"] {
        let f = Fixture::new();
        let mut args = vec![
            command,
            "--experimental-builtin-modules=false",
            "--repository_disable_download",
        ];
        match command {
            "mod" => args.extend(["dump_repo_mapping", ""]),
            "query" => args.push("//..."),
            _ => {
                args.extend(["--platforms=//:platform", "--host_platform=//:platform"]);
                if command != "cquery" {
                    args.push("--nobuild");
                }
                args.push(if command == "test" || command == "coverage" {
                    "//:small_test"
                } else {
                    "//:leaf"
                });
            }
        }
        if command == "test" || command == "coverage" {
            // These explicit-module probes never execute tests. The test
            // tail rejects --nobuild; coverage needs the omitted built-in
            // runner. Both still publish successful module resolution.
            let out = f.run(&[&f.flag("private.lock")], &args);
            assert_eq!(out.status.code(), Some(1), "{out:?}");
            assert!(
                String::from_utf8_lossy(&out.stderr).contains(if command == "coverage" {
                    "coverage needs the @bazel_tools test runner"
                } else {
                    "Unable to run tests"
                }),
                "{out:?}"
            );
        } else {
            f.ok(&[&f.flag("private.lock")], &args);
        }
        updated(&f.path("private.lock"));
        f.unchanged();
    }
}

#[test]
fn last_startup_value_then_last_command_value_wins_without_global_fallback() {
    let f = Fixture::new();
    let args = [
        "mod",
        "dump_repo_mapping",
        "",
        "--experimental-builtin-modules=false",
    ];
    f.ok(&[&f.flag("unused.lock"), &f.flag("startup.lock")], &args);
    updated(&f.path("startup.lock"));
    assert!(!f.path("unused.lock").exists());
    f.unchanged();
    let mut override_args = args.to_vec();
    let command = f.flag("command.lock");
    override_args.extend(["--native-lockfile=workspace", &command]);
    f.ok(&[&f.flag("unused.lock")], &override_args);
    updated(&f.path("command.lock"));
    assert!(!f.path("unused.lock").exists());
    f.unchanged();
    override_args.truncate(args.len());
    override_args.push("--native-lockfile=readonly");
    f.ok(&[&f.flag("unused.lock")], &override_args);
    assert!(!f.path("unused.lock").exists());
    f.unchanged();
    // An explicit workspace override really does restore workspace writes.
    fs::write(f.path("blocked"), "parent is a file").unwrap();
    let out = f.ok(&[&f.flag("blocked/private.lock")], &args);
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("WARNING"),
        "{out:?}"
    );
    f.unchanged();
    override_args.pop();
    override_args.push("--native-lockfile=workspace");
    f.ok(&["--native-lockfile=readonly"], &override_args);
    updated(&f.root.join("MODULE.bazel.lock"));
    fs::write(f.root.join("MODULE.bazel.lock"), CANARY).unwrap();
    f.ok(&[], &args);
    updated(&f.root.join("MODULE.bazel.lock"));
}

#[test]
fn cquery_path_changes_cannot_hit_a_graph_recorded_for_another_lockfile() {
    let f = Fixture::new();
    let args = [
        "cquery",
        "--experimental-builtin-modules=false",
        "--platforms=//:platform",
        "--host_platform=//:platform",
        "//:leaf",
    ];
    for name in ["a.lock", "b.lock"] {
        for _ in 0..3 {
            f.ok(&[&f.flag(name)], &args);
        }
        updated(&f.path(name));
        f.unchanged();
    }
    let out = f.ok(&[&f.flag("b.lock")], &args);
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("bazel-rs-cquery-snapshot: hit"),
        "{out:?}"
    );
    for key in ["output_base", "release", "starlark-semantics"] {
        f.ok(
            &[&f.flag("b.lock")],
            &["info", key, "--native-lockfile=readonly"],
        );
    }
    f.ok(
        &[&f.flag("b.lock")],
        &[
            "config",
            "--output=json",
            "--dump_all",
            "--native-lockfile=readonly",
        ],
    );
    f.unchanged();
}

#[test]
fn malformed_controls_fail_before_module_loading_and_cannot_be_aliased() {
    let f = Fixture::new();
    fs::write(f.root.join("MODULE.bazel"), "fail('MODULE-MUST-NOT-RUN')\n").unwrap();
    for bad in [
        "--native-lockfile",
        "--native-lockfile=",
        "--native-lockfile=relative",
        "--native-lockfile=read-only",
    ] {
        for command in [
            "mod", "info", "config", "query", "cquery", "build", "test", "coverage",
        ] {
            for (startup, args) in [
                (vec![bad], vec![command, "--native-lockfile=readonly"]),
                (vec!["--native-lockfile=readonly"], vec![command, bad]),
            ] {
                let out = f.run(&startup, &args);
                let error = String::from_utf8_lossy(&out.stderr);
                assert!(!out.status.success(), "{out:?}");
                assert!(error.contains("--native-lockfile"), "{error}");
                assert!(!error.contains("MODULE-MUST-NOT-RUN"), "{error}");
            }
        }
    }
    for name in ["native-lockfile", "lockfile_mode", "registry"] {
        let out = f.run(
            &[&f.flag("private.lock")],
            &[
                "build",
                &format!("--flag_alias={name}=//:setting"),
                "//:leaf",
            ],
        );
        assert!(!out.status.success(), "{out:?}");
        assert!(
            !String::from_utf8_lossy(&out.stderr).contains("MODULE-MUST-NOT-RUN"),
            "{out:?}"
        );
        fs::write(
            f.root.join("MODULE.bazel"),
            format!("module(name='isolation')\nflag_alias(name='{name}', actual='//:setting')\n"),
        )
        .unwrap();
        let out = f.run(
            &[&f.flag("private.lock")],
            &["cquery", "--experimental-builtin-modules=false", "//:leaf"],
        );
        assert!(!out.status.success(), "{out:?}");
        assert!(
            String::from_utf8_lossy(&out.stderr).contains("alias"),
            "{out:?}"
        );
        fs::write(f.root.join("MODULE.bazel"), "fail('MODULE-MUST-NOT-RUN')\n").unwrap();
    }
    f.unchanged();
}

#[test]
fn native_control_remains_cli_only_and_preserves_rc_quiet_and_error_order() {
    let f = Fixture::new();
    fs::write(
        f.root.join(".bazelrc"),
        "startup --native-lockfile=readonly\n",
    )
    .unwrap();
    let out = f.run(&[&f.flag("private.lock")], &["info", "release"]);
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("native-only option --native-lockfile"),
        "{out:?}"
    );
    f.ok(
        &["--ignore_all_rc_files", &f.flag("private.lock")],
        &["info", "release"],
    );
    for startup in [
        vec!["--quiet", "--native-lockfile=readonly"],
        vec!["--native-lockfile=readonly", "--quiet"],
    ] {
        fs::write(
            f.root.join(".bazelrc"),
            "startup --output_base=private-output\ncommon --announce_rc\n",
        )
        .unwrap();
        let out = f.ok(&startup, &["info", "output_base"]);
        assert!(out.stderr.is_empty(), "{out:?}");
        assert_eq!(
            String::from_utf8(out.stdout).unwrap().trim(),
            f.root.join("private-output").to_str().unwrap()
        );
    }
    fs::write(f.root.join(".bazelrc"), "build --jobs=bad\n").unwrap();
    let out = f.run(
        &["--native-lockfile=readonly"],
        &["build", "--native-lockfile=relative"],
    );
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("--jobs requires a positive integer"),
        "{out:?}"
    );
    assert!(
        !String::from_utf8_lossy(&out.stderr).contains("got \"relative\""),
        "{out:?}"
    );
    // Early/non-loading commands accept the native startup form, but their
    // genuine flag catalogs/canonicalizer never advertise it as a Bazel flag.
    f.ok(&["--native-lockfile=readonly"], &["help", "native"]);
    f.ok(
        &["--native-lockfile=readonly"],
        &[
            "canonicalize-flags",
            "--for_command=build",
            "--",
            "--jobs=1",
        ],
    );
    let out = f.run(
        &[],
        &[
            "canonicalize-flags",
            "--for_command=build",
            "--",
            "--native-lockfile=readonly",
        ],
    );
    assert!(!out.status.success(), "{out:?}");
    f.unchanged();
}
