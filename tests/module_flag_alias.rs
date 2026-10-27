//! Paired Bazel 9.3 MODULE aliases: selected module metadata reaches real
//! build-setting, select and transition consumers. No genuine subprocesses
//! in this suite; compact independent capture receipts live beside fixture.
#![allow(clippy::disallowed_methods)]
use std::{
    fs,
    path::{Path, PathBuf},
    process::{Command, Output},
};

const FIXTURE: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/tests/fixtures/module_flag_alias"
);
struct Fixture(tempfile::TempDir);
impl Fixture {
    fn new() -> Self {
        let base = fs::canonicalize(std::env::temp_dir()).unwrap();
        let f = Self(tempfile::tempdir_in(base).unwrap());
        fs::create_dir(f.path().join("home")).unwrap();
        fs::create_dir(f.path().join("tmp")).unwrap();
        for name in ["MODULE.bazel", "BUILD.bazel", "defs.bzl"] {
            f.write(
                name,
                &fs::read_to_string(Path::new(FIXTURE).join(name)).unwrap(),
            );
        }
        let module = fs::read_to_string(Path::new(FIXTURE).join("dep.MODULE.bazel")).unwrap();
        f.dep("1", &module);
        f.write(
            "registry/bazel_registry.json",
            r#"{"module_base_path":"sources"}"#,
        );
        f.write(
            "tools/allowlists/function_transition_allowlist/BUILD.bazel",
            "package_group(name='function_transition_allowlist', packages=['public'])",
        );
        f
    }
    fn path(&self) -> &Path {
        self.0.path()
    }
    fn write(&self, name: &str, text: &str) {
        let p = self.path().join(name);
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(p, text).unwrap();
    }
    fn dep(&self, version: &str, module: &str) {
        self.write(
            &format!("registry/modules/dep/{version}/MODULE.bazel"),
            module,
        );
        self.write(
            &format!("registry/modules/dep/{version}/source.json"),
            &format!(r#"{{"type":"local_path","path":"dep-{version}"}}"#),
        );
        self.write(
            "registry/modules/dep/metadata.json",
            r#"{"versions":["1","2"],"yanked_versions":{}}"#,
        );
        let path = format!("registry/sources/dep-{version}");
        self.write(&format!("{path}/MODULE.bazel"), module);
        self.write(
            &format!("{path}/defs.bzl"),
            &fs::read_to_string(Path::new(FIXTURE).join("defs.bzl"))
                .unwrap()
                .replace("@dep//:info.bzl", ":info.bzl"),
        );
        self.write(
            &format!("{path}/info.bzl"),
            &fs::read_to_string(Path::new(FIXTURE).join("info.bzl")).unwrap(),
        );
        self.write(&format!("{path}/BUILD.bazel"), "load(\":defs.bzl\", \"string_flag\")\nstring_flag(name=\"text\", build_setting_default=\"dep\", visibility=[\"//visibility:public\"])\nstring_flag(name=\"other\", build_setting_default=\"second\", visibility=[\"//visibility:public\"])\n");
    }
    fn run(&self, command: &str, args: &[&str]) -> Output {
        self.run_env(command, args, &[])
    }
    fn run_env(&self, command: &str, args: &[&str], env: &[(&str, &str)]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_bazel-rs"))
            .args([
                "--nosystem_rc",
                "--nohome_rc",
                "--cache-dir=cache",
                command,
                "--experimental-builtin-modules=false",
                "--lockfile_mode=off",
            ])
            .arg(format!(
                "--registry=file://{}",
                self.path().join("registry").display()
            ))
            .args(if command == "query" {
                &[][..]
            } else {
                &["--platforms=//:platform", "--host_platform=//:platform"][..]
            })
            .args(args)
            .current_dir(self.path())
            .env_clear()
            .env("PATH", "/usr/bin:/bin")
            .env("HOME", self.path().join("home"))
            .env("TMPDIR", self.path().join("tmp"))
            .env("BAZEL_RS_CACHE_GC", "off")
            .envs(env.iter().copied())
            .output()
            .unwrap()
    }
    fn built(&self, args: &[&str]) -> String {
        let mut args = args.to_vec();
        args.push("//:read");
        self.built_targets(&args)
    }
    fn built_targets(&self, args: &[&str]) -> String {
        let out = self.run("build", args);
        assert!(
            out.status.success(),
            "{args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        let stdout = String::from_utf8(out.stdout).unwrap();
        let path = stdout
            .lines()
            .find_map(|line| line.split_once('\t').map(|(_, path)| PathBuf::from(path)))
            .unwrap();
        fs::read_to_string(path).unwrap()
    }
    fn rejects(&self, args: &[&str], expected: &str) {
        let mut args = args.to_vec();
        args.push("//:read");
        let out = self.run("build", &args);
        assert!(!out.status.success(), "unexpected success {args:?}");
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(
            stderr.contains(expected),
            "{args:?}: wanted {expected}: {stderr}"
        );
    }
}

#[test]
fn unsupported_command_options_fail_before_bootstrap_and_later_parse_errors() {
    let f = Fixture::new();
    let module = fs::read_to_string(f.path().join("MODULE.bazel")).unwrap();
    f.write("MODULE.bazel", &(module + "\nprint('MODULE_SEEN')\n"));
    for args in [
        vec!["--compile_one_dependency", "//:read"],
        vec![
            "--disk_cache=private",
            "--sandbox_add_mount_pair=/a::/b",
            "//:read",
        ],
    ] {
        let out = f.run("build", &args);
        let error = String::from_utf8_lossy(&out.stderr);
        assert!(!out.status.success());
        assert!(
            error.contains(&format!("unsupported option {:?}", args[0])),
            "{error}"
        );
        assert!(
            !error.contains("memory limit defaulted") && !error.contains("MODULE_SEEN"),
            "{error}"
        );
    }
    f.write(
        ".bazelrc",
        "build --disk_cache=private\nbuild:cross --sandbox_add_mount_pair=/a::/b\n",
    );
    let out = f.run("build", &["--config=cross", "//:read"]);
    let error = String::from_utf8_lossy(&out.stderr);
    assert!(!out.status.success());
    assert!(
        error.contains("--disk_cache=private") && error.contains(".bazelrc:1"),
        "{error}"
    );
    assert!(
        !error.contains("While parsing option --sandbox") && !error.contains("MODULE_SEEN"),
        "{error}"
    );
}

#[test]
fn first_pass_alias_and_native_failures_keep_parse_order_and_native_types() {
    let f = Fixture::new();
    for (args, expected, absent) in [
        (
            vec![
                "--compilation_mode=arbitrary",
                "--flag_alias=bad-name=//:text",
            ],
            "Not a valid compilation mode",
            "word characters",
        ),
        (
            vec![
                "--flag_alias=bad-name=//:text",
                "--compilation_mode=arbitrary",
            ],
            "word characters",
            "Not a valid compilation mode",
        ),
        (
            vec![
                "--compilation_mode=arbitrary",
                "--flag_alias=compilation_mode=//:text",
            ],
            "Not a valid compilation mode",
            "word characters",
        ),
        (
            vec![
                "--build_python_zip=invalid",
                "--flag_alias=build_python_zip=//:text",
            ],
            "not a boolean",
            "memory limit defaulted",
        ),
        (
            vec!["--newalias=x", "--sandbox_add_mount_pair=/a::/b"],
            "contains one or more empty paths",
            "Unrecognized option",
        ),
    ] {
        let mut args = args;
        args.push("//:read");
        let out = f.run("build", &args);
        let error = String::from_utf8_lossy(&out.stderr);
        assert!(
            !out.status.success() && error.contains(expected) && !error.contains(absent),
            "{args:?}: {error}"
        );
    }
    // A removed/unmodelled configuration option may still be a MODULE alias,
    // but its native first-pass converter cannot be silently bypassed.
    let module = fs::read_to_string(f.path().join("MODULE.bazel")).unwrap();
    f.write(
        "MODULE.bazel",
        &(module + "\nflag_alias(name='build_python_zip',starlark_flag='//:enabled')\n"),
    );
    assert_eq!(
        f.built(&["--build_python_zip"]),
        "root|other|True|1|dep|no|fastbuild"
    );
    f.rejects(&["--build_python_zip=invalid"], "not a boolean");
}

#[test]
fn canonical_snapshots_hit_and_verify_with_and_without_module_aliases() {
    for mode in ["on", "verify"] {
        for aliased in [false, true] {
            let f = Fixture::new();
            let mut module = "module(name='snapshot_probe')\n".to_owned();
            if aliased {
                module.push_str("flag_alias(name='flavor',starlark_flag='//:text')\n");
            }
            f.write("MODULE.bazel", &module);
            let defs = fs::read_to_string(Path::new(FIXTURE).join("defs.bzl")).unwrap();
            f.write(
                "defs.bzl",
                &defs.replace(
                    "load(\"@dep//:info.bzl\", \"Info\")",
                    "Info = provider(fields=['value'])",
                ),
            );
            f.write("BUILD.bazel", "load(':defs.bzl','string_flag','reader')\nplatform(name='platform')\nstring_flag(name='text',build_setting_default='root')\nstring_flag(name='other',build_setting_default='other')\nreader(name='read',flags=[':text',':other'])\n");
            let arg = if aliased {
                "--flavor=chosen"
            } else {
                "--//:text=chosen"
            };
            let env = [
                ("BAZEL_RS_SNAPSHOT", mode),
                ("BAZEL_RS_SNAPSHOT_EXPLAIN", "1"),
                ("BAZEL_RS_SNAPSHOT_VERIFY_STRICT", "1"),
            ];
            let run = || {
                let out = f.run_env("build", &[arg, "//:read"], &env);
                assert!(
                    out.status.success(),
                    "{}",
                    String::from_utf8_lossy(&out.stderr)
                );
                let stdout = String::from_utf8_lossy(&out.stdout);
                let path = stdout
                    .lines()
                    .find_map(|line| line.split_once('\t').map(|(_, p)| p))
                    .unwrap();
                (
                    fs::read_to_string(path).unwrap(),
                    String::from_utf8(out.stderr).unwrap(),
                )
            };
            let (contents, first) = run();
            assert_eq!(contents, "chosen|other||fastbuild");
            assert!(first.contains("bazel-rs-snapshot: saved "), "{first}");
            let (contents, warm) = run();
            assert_eq!(contents, "chosen|other||fastbuild");
            assert!(warm.contains("bazel-rs-snapshot: hit "), "{warm}");
            if mode == "verify" {
                assert!(warm.contains("bazel-rs-snapshot: verified "), "{warm}");
            }
            if aliased {
                f.write(
                    "MODULE.bazel",
                    &module.replace("starlark_flag='//:text'", "starlark_flag='//:other'"),
                );
                let (contents, changed) = run();
                assert_eq!(contents, "root|chosen||fastbuild");
                assert!(
                    changed.contains("bazel-rs-snapshot: miss ")
                        && changed.contains("bazel-rs-snapshot: saved "),
                    "{changed}"
                );
                let (contents, warm) = run();
                assert_eq!(contents, "root|chosen||fastbuild");
                assert!(warm.contains("bazel-rs-snapshot: hit "), "{warm}");
                if mode == "verify" {
                    assert!(warm.contains("bazel-rs-snapshot: verified "), "{warm}");
                }
            }
        }
    }
}

#[test]
fn typed_aliases_native_shadowing_and_select_reach_outputs() {
    let f = Fixture::new();
    assert_eq!(
        f.built(&[
            "--flag_alias=compilation_mode=//:text",
            "--compilation_mode=arbitrary"
        ]),
        "arbitrary|other|False|1|dep|no|fastbuild"
    );
    assert_eq!(f.built(&[]), "root|other|False|1|dep|no|fastbuild");
    assert_eq!(
        f.built(&[
            "--flavor=yes",
            "--enabled",
            "--count=0x2",
            "--depflag=custom",
            "--compilation_mode=opt"
        ]),
        "yes|opt|True|2|custom|match|fastbuild"
    );
    assert_eq!(
        f.built(&["--flavor=yes", "--enabled=false", "-c", "dbg"]),
        "yes|other|False|1|dep|match|dbg"
    );
    assert_eq!(
        f.built(&["--flag_alias=flavor=//:other", "--flavor=yes"]),
        "root|yes|False|1|dep|match|fastbuild"
    );
    assert_eq!(
        f.built(&[
            "--flavor=yes",
            "--flag_alias=flavor=//:other",
            "--flavor=late"
        ]),
        "yes|late|False|1|dep|no|fastbuild"
    );
    assert_eq!(
        f.built(&["--unknown=x", "--flag_alias=unknown=//:text"]),
        "x|other|False|1|dep|no|fastbuild"
    );
}

#[test]
fn rc_priority_and_config_expansion_use_aliases() {
    let f = Fixture::new();
    f.write(".bazelrc", "build --flavor=rc\nbuild:yes --flavor=yes\n");
    assert_eq!(
        f.built(&["--config=yes"]),
        "yes|other|False|1|dep|match|fastbuild"
    );
    assert_eq!(
        f.built(&["--config=yes", "--flavor=cli"]),
        "cli|other|False|1|dep|no|fastbuild"
    );
    f.write(
        ".bazelrc",
        "build --flag_alias=flavor=//:other --flavor=rc\n",
    );
    assert_eq!(
        f.built(&["--flavor=yes"]),
        "root|yes|False|1|dep|match|fastbuild"
    );
    f.write(".bazelrc", "test --flag_alias=x=//:text\n");
    f.rejects(&[], "--flag_alias only supports these commands");
}

#[test]
fn query_is_loading_only_and_cquery_configuration_changes() {
    let f = Fixture::new();
    assert!(f.run("query", &["//:read"]).status.success());
    assert!(
        !f.run("query", &["--flavor=yes", "//:read"])
            .status
            .success()
    );
    let first = f.run("cquery", &["//:read"]);
    assert!(
        first.status.success(),
        "{}",
        String::from_utf8_lossy(&first.stderr)
    );
    let changed = f.run("cquery", &["--flavor=yes", "//:read"]);
    assert!(
        changed.status.success(),
        "{}",
        String::from_utf8_lossy(&changed.stderr)
    );
    assert_ne!(first.stdout, changed.stdout);
}

#[test]
fn aliases_are_not_no_ops_and_invalid_forms_fail_closed() {
    let f = Fixture::new();
    for (args, expected) in [
        (vec!["--noenabled"], "Unrecognized option"),
        (vec!["--count=oops"], "not a int"),
        (
            vec!["--flag_alias=p=//:private", "--p=x"],
            "Unrecognized option",
        ),
        (vec!["--flag_alias=x=//:missing", "--x=x"], "missing"),
        (vec!["--flag_alias=bad-name=//:text"], "word characters"),
        (vec!["--flag_alias=x=//:text=value"], "value assignment"),
        (vec!["--flag_alias=x=jobs"], "only supports Starlark"),
        (
            vec!["--flag_alias=jobs=//:count", "--jobs=2"],
            "shadows a non-configuration",
        ),
    ] {
        f.rejects(&args, expected);
    }
}

#[test]
fn declaration_types_order_duplicates_and_normalization() {
    use bazel_rs::bzlmod::evaluate_source;
    let good = evaluate_source(
        "MODULE.bazel",
        "module(name='m', repo_name='own')\nflag_alias(name='x', starlark_flag=':flag')\n".into(),
    )
    .unwrap();
    assert_eq!(good.flag_aliases["x"], "@own//:flag");
    for source in [
        "flag_alias(name=1, starlark_flag='//:x')",
        "flag_alias(name='x', starlark_flag=False)",
        "flag_alias('x', '//:x')",
        "flag_alias(name='x')",
        "flag_alias(name='x', starlark_flag='//bad:')",
        "flag_alias(name='x', starlark_flag='//:x')\nmodule(name='late')",
        "flag_alias(name='x', starlark_flag='//:x')\nflag_alias(name='x', starlark_flag='//:y')",
    ] {
        assert!(
            evaluate_source("MODULE.bazel", source.into()).is_err(),
            "{source}"
        );
    }
}

#[test]
fn changed_alias_destination_invalidates_warm_build_and_config_snapshots() {
    let f = Fixture::new();
    let args = ["--flavor=yes"];
    assert_eq!(f.built(&args), f.built(&args));
    let first = f.run("cquery", &["//:read"]);
    let module = fs::read_to_string(f.path().join("MODULE.bazel")).unwrap();
    f.write(
        "MODULE.bazel",
        &module.replace("starlark_flag = \":text\"", "starlark_flag = \"//:other\""),
    );
    assert_eq!(f.built(&args), "root|yes|False|1|dep|match|fastbuild");
    let changed = f.run("cquery", &["//:read"]);
    assert!(first.status.success() && changed.status.success());
    assert_ne!(
        first.stdout, changed.stdout,
        "even unused alias destinations are configuration metadata"
    );
}

#[test]
fn selected_versions_and_overrides_supply_aliases_and_later_modules_win() {
    let f = Fixture::new();
    f.dep("2", "module(name='dep',version='2',repo_name='own')\nflag_alias(name='depflag',starlark_flag='//:other')\nflag_alias(name='flavor',starlark_flag='//:text')\n");
    assert_eq!(
        f.built(&["--depflag=custom"]),
        "root|other|False|1|custom|no|fastbuild"
    );
    let module = fs::read_to_string(f.path().join("MODULE.bazel")).unwrap();
    f.write(
        "MODULE.bazel",
        &(module.clone() + "\nsingle_version_override(module_name='dep', version='2')\n"),
    );
    assert_eq!(
        f.built(&["--depflag=custom", "--flavor=yes"]),
        "root|other|False|1|yes|match|fastbuild"
    );
    f.write(
        "MODULE.bazel",
        &(module + "\nlocal_path_override(module_name='dep', path='registry/sources/dep-2')\n"),
    );
    assert_eq!(
        f.built(&["--flavor=yes"]),
        "root|other|False|1|yes|match|fastbuild"
    );
}

#[test]
fn transitive_dependency_alias_resolves_in_its_own_namespace() {
    let f = Fixture::new();
    f.write("BUILD.bazel", "platform(name='platform')\n");
    f.write("MODULE.bazel", "module(name='root')\nbazel_dep(name='bridge')\nlocal_path_override(module_name='bridge',path='bridge')\n");
    f.write(
        "bridge/MODULE.bazel",
        "module(name='bridge')\nbazel_dep(name='dep',version='1')\n",
    );
    f.write(
        "bridge/BUILD.bazel",
        "load('@dep//:defs.bzl','reader')\nreader(name='read',flags=['@dep//:text'])\n",
    );
    assert_eq!(
        f.built_targets(&["@bridge//:read", "--depflag=transitive"]),
        "transitive||fastbuild"
    );
}

#[test]
fn transitions_read_and_write_the_alias_destination() {
    let f = Fixture::new();
    assert_eq!(
        f.built_targets(&["//:transitioned", "--flavor=yes"]),
        "transition-yes|other|False|1|dep|no|fastbuild"
    );
    assert_eq!(
        f.built_targets(&[
            "//:transitioned",
            "--flag_alias=flavor=//:other",
            "--flavor=yes"
        ]),
        "root|transition-yes|False|1|dep|no|fastbuild"
    );
}

#[test]
fn conflicting_select_and_transition_expectations_fail() {
    let f = Fixture::new();
    let build = fs::read_to_string(f.path().join("BUILD.bazel")).unwrap();
    f.write("BUILD.bazel", &(build+"\nconfig_setting(name='conflict',values={'flavor':'a'},flag_values={':text':'b'})\nreader(name='conflicting',selected=select({':conflict':'yes','//conditions:default':'no'}))\n"));
    let out = f.run("build", &["//:conflicting"]);
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("Conflicting flag value expectations"));
    let defs = fs::read_to_string(f.path().join("defs.bzl")).unwrap();
    f.write("defs.bzl", &defs.replace("return {\"//command_line_option:flavor\": \"transition-\" + settings[\"//command_line_option:flavor\"]}", "return {\"//command_line_option:flavor\": \"a\", \"//:text\": \"b\"}")
        .replace("outputs = [\"//command_line_option:flavor\"]", "outputs = [\"//command_line_option:flavor\", \"//:text\"]"));
    let out = f.run("build", &["//:transitioned"]);
    assert!(!out.status.success());
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("alias have different values"),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
}
