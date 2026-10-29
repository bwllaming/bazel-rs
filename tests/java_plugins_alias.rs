//! Genuine 9.3 bridge fixtures using unchanged rules_java 9.1.0 provider code.
//! No JVM/javac in tests; archives are checksum-verified before extraction.
#![allow(clippy::disallowed_methods)]
include!("support/temp_root.rs");
include!("support/cquery_current_scope.rs");
include!("support/module_archives.rs");
use serde_json::{Value, json};
use std::os::unix::fs::PermissionsExt;
use std::{
    fs,
    path::Path,
    process::{Command, Output},
};

struct Fixture(tempfile::TempDir);
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
fn empty_builds(dir: &Path) {
    for entry in fs::read_dir(dir).unwrap() {
        let entry = entry.unwrap();
        if entry.file_type().unwrap().is_dir() {
            empty_builds(&entry.path());
        } else if matches!(entry.file_name().to_str(), Some("BUILD" | "BUILD.bazel")) {
            fs::set_permissions(entry.path(), fs::Permissions::from_mode(0o644)).unwrap();
            fs::write(entry.path(), "").unwrap();
        }
    }
}
impl Fixture {
    fn new() -> Self {
        use base64::Engine as _;
        let f = Self(tempfile::tempdir_in(crate::test_temp::root()).unwrap());
        let manifest = Path::new(env!("CARGO_MANIFEST_DIR"));
        copy(
            &manifest.join("tests/fixtures/java_plugins_alias/workspace"),
            f.path(),
        );
        seed_module_archives(
            &f.path().join("cache"),
            &[
                "rules_java@9.1.0",
                "rules_cc@0.2.17",
                "platforms@1.0.0",
                "bazel_skylib@1.8.2",
                "rules_license@1.0.0",
            ],
        );
        let mut module = "module(name='plugin_fixture')\n".to_owned();
        for (name, version) in [
            ("rules_java", "9.1.0"),
            ("rules_cc", "0.2.17"),
            ("platforms", "1.0.0"),
            ("bazel_skylib", "1.8.2"),
            ("rules_license", "1.0.0"),
        ] {
            let pin: Value = serde_json::from_slice(
                &fs::read(manifest.join(format!(
                    "tests/fixtures/builtin_modules/registry/modules/{name}/{version}/source.json"
                )))
                .unwrap(),
            )
            .unwrap();
            let digest = base64::engine::general_purpose::STANDARD
                .decode(
                    pin["integrity"]
                        .as_str()
                        .unwrap()
                        .strip_prefix("sha256-")
                        .unwrap(),
                )
                .unwrap();
            let hex: String = digest.iter().map(|byte| format!("{byte:02x}")).collect();
            let archive = fs::File::open(
                f.path()
                    .join("cache/repositories/downloads/sha256")
                    .join(hex),
            )
            .unwrap();
            let staging = f.path().join(format!("extract-{name}"));
            tar::Archive::new(flate2::read::GzDecoder::new(archive))
                .unpack(&staging)
                .unwrap();
            let source = staging.join(pin["strip_prefix"].as_str().unwrap_or(""));
            fs::rename(&source, f.path().join(name)).unwrap();
            if name != "platforms" {
                empty_builds(&f.path().join(name));
                let deps = match name {
                    "rules_java" => "bazel_dep(name='rules_cc',version='0.2.17')\n",
                    "rules_cc" => {
                        "bazel_dep(name='bazel_skylib',version='1.8.2')\nbazel_dep(name='cc_compatibility_proxy',version='1')\n"
                    }
                    _ => "",
                };
                f.write(
                    &format!("{name}/MODULE.bazel"),
                    &format!("module(name='{name}',version='{version}')\n{deps}"),
                );
            }
            module += &format!(
                "bazel_dep(name='{name}',version='{version}')\nlocal_path_override(module_name='{name}',path='{name}')\n"
            );
        }
        // Exact symbols.bzl emitted by the genuine rules_cc extension's 9.x
        // branch. No replacement CcInfo/JavaInfo/provider definitions.
        let cc = fs::read_to_string(f.path().join("rules_cc/cc/extensions.bzl")).unwrap();
        let symbols = cc
            .split_once("\"symbols.bzl\",")
            .unwrap()
            .1
            .split_once("\"\"\"")
            .unwrap()
            .1
            .split_once("\"\"\"")
            .unwrap()
            .0;
        f.write("cc_proxy/symbols.bzl", symbols);
        f.write("cc_proxy/BUILD.bazel", "");
        f.write("cc_proxy/MODULE.bazel", "module(name='cc_compatibility_proxy',version='1')\nbazel_dep(name='rules_cc',version='0.2.17')\n");
        module += "local_path_override(module_name='cc_compatibility_proxy',path='cc_proxy')\n";
        for name in [
            "apple_support",
            "buildozer",
            "protobuf",
            "rules_python",
            "rules_shell",
            "zlib",
        ] {
            module += &format!(
                "local_path_override(module_name='{name}',path='{}')\n",
                manifest
                    .join("tests/fixtures/test_flags/stubs")
                    .join(name)
                    .display()
            );
        }
        f.write("MODULE.bazel", &module);
        for dir in ["home", "tmp"] {
            fs::create_dir(f.path().join(dir)).unwrap();
        }
        f
    }
    fn path(&self) -> &Path {
        self.0.path()
    }
    fn write(&self, name: &str, text: &str) {
        let path = self.path().join(name);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        if path.exists() {
            fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
        }
        fs::write(path, text).unwrap();
    }
    fn run(&self, command: &str, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_bazel-rs"))
            .current_dir(self.path())
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
                "--repository_disable_download",
                "--lockfile_mode=update",
                "--memory-limit=2048",
            ])
            .args(args)
            .output()
            .unwrap()
    }
    fn ok(&self, command: &str, args: &[&str]) -> Output {
        let out = self.run(command, args);
        assert!(
            out.status.success(),
            "{command} {args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        out
    }
    fn built(&self, args: &[&str]) -> Value {
        let out = self.ok("build", args);
        let stdout = String::from_utf8(out.stdout).unwrap();
        let path = stdout
            .lines()
            .find_map(|l| l.split_once('\t').map(|(_, p)| p))
            .unwrap();
        serde_json::from_slice(&fs::read(path).unwrap()).unwrap()
    }
}
fn empty() -> Value {
    json!({"api":{"classes":[],"data":[],"jars":[]}, "plugins":{"classes":[],"data":[],"jars":[]}, "outputs":0,"files":[],"default_runfiles":[],"data_runfiles":[]})
}

#[test]
fn real_java_provider_default_nonempty_merge_outputs_runfiles_and_exec_reset() {
    let f = Fixture::new();
    assert_eq!(f.built(&["//:probe"]), empty());
    let direct = f.built(&["//:direct"]);
    assert_eq!(direct["outputs"], 1);
    assert_eq!(direct["files"], json!(["one.jar", "one.data"]));
    let mut one = empty();
    one["plugins"] =
        json!({"classes":["One","Common"],"jars":["one.jar"],"data":["one.data","input.txt"]});
    one["api"] = one["plugins"].clone();
    assert_eq!(f.built(&["--plugin=//:one", "//:probe"]), one);
    assert_eq!(f.built(&["--plugin=//:indirect", "//:probe"]), one);
    let mut merged = one;
    merged["plugins"] = json!({"classes":["Two","Common","One"],"jars":["two.jar","one.jar"],"data":["two.data","input.txt","one.data"]});
    assert_eq!(
        f.built(&["--plugin=//:two,//:one", "--plugin=//:two", "//:probe"]),
        merged
    );
    assert_eq!(
        f.built(&["--plugin=//:two,//:one", "//:exec_probe"]),
        empty()
    );
    assert_eq!(f.built(&["--plugin=//:input.txt", "//:probe"]), empty());
    assert_eq!(f.built(&["--plugin=", "//:probe"]), empty());
    let changed = f.built(&["--plugin=//:one", "//:changed"]);
    assert_eq!(changed["plugins"]["classes"], json!(["Two", "Common"]));
    assert_eq!(f.built(&["--plugin=//:one", "//:cleared"]), empty());
}

#[test]
fn real_main_repo_provider_fallback_and_mixed_provider_precedence() {
    let f = Fixture::new();
    copy(&f.path().join("rules_java/java"), &f.path().join("java"));
    let module = fs::read_to_string(f.path().join("MODULE.bazel"))
        .unwrap()
        .replace(
            "module(name='plugin_fixture')",
            "module(name='plugin_fixture',repo_name='rules_java')",
        )
        .replace(
            "bazel_dep(name='rules_java',version='9.1.0')",
            "bazel_dep(name='rules_java',version='9.1.0',repo_name='external_java')",
        );
    f.write("MODULE.bazel", &module);
    let plugin = fs::read_to_string(f.path().join("plugin.bzl")).unwrap();
    f.write(
        "plugin.bzl",
        &plugin.replace("@rules_java//", "@external_java//"),
    );
    f.write("main_plugin.bzl", &plugin.replace("@rules_java//", "//"));
    let build = fs::read_to_string(f.path().join("BUILD.bazel")).unwrap();
    f.write("BUILD.bazel", &(build + "\nload(':main_plugin.bzl', main_plugin='plugin', main_probe='probe')\nmain_plugin(name='main',processor='Main',api=True)\nmain_probe(name='main_probe',dep='//tools/jdk:java_plugins_flag_alias')\n"));
    let main = f.built(&["--plugin=//:main", "//:probe"]);
    assert_eq!(main["plugins"]["classes"], json!(["Main", "Common"]));
    assert_eq!(main["plugins"]["jars"], json!(["main.jar"]));
    assert_eq!(main["outputs"], 0);
    assert_eq!(f.built(&["--plugin=//:main", "//:main_probe"]), main);
    let mixed = f.built(&["--plugin=//:main,//:two", "//:main_probe"]);
    assert_eq!(mixed["plugins"]["classes"], json!(["Two", "Common"]));
    assert_eq!(mixed["api"], empty()["api"]);
}

#[test]
fn required_provider_private_rule_and_testonly_errors_are_not_empty_fallbacks() {
    let f = Fixture::new();
    for (plugin, expected) in [
        (
            "//:wrong",
            "does not have mandatory providers: 'JavaPluginInfo' or 'JavaPluginInfo'",
        ),
        ("//:testonly", "depends on testonly target"),
        ("//:absent", "absent"),
        (
            "//:invalid_constructor",
            "at index 0 of deps, got element of type struct, want JavaInfo",
        ),
    ] {
        let flag = format!("--plugin={plugin}");
        for cmd in ["build", "cquery"] {
            let out = f.run(cmd, &[&flag, "//tools/jdk:java_plugins_flag_alias"]);
            assert!(!out.status.success());
            assert!(
                String::from_utf8_lossy(&out.stderr).contains(expected),
                "{}",
                String::from_utf8_lossy(&out.stderr)
            );
        }
    }
    f.write("bad/BUILD.bazel", "java_plugins_flag_alias(name='bad')\n");
    let out = f.run("build", &["//bad:bad"]);
    assert!(String::from_utf8_lossy(&out.stderr).contains("cannot use private rule"));
    let out = f.run("query", &["--plugin=//:one", "//:one"]);
    assert!(!out.status.success());
}

#[test]
fn configured_plugin_edges_snapshot_invalidation_and_no_cquery_execution() {
    let f = Fixture::new();
    let args = [
        "--plugin=//:one",
        "--output=proto",
        "--transitions=lite",
        "deps(//tools/jdk:java_plugins_flag_alias)",
    ];
    // First invocation materializes the real platforms host extension; that
    // invocation is deliberately uncacheable (ExtensionEvaluated taint).
    f.ok("cquery", &args);
    let cold = f.ok("cquery", &args);
    let warm = f.ok("cquery", &args);
    assert_eq!(cold.stdout, warm.stdout);
    assert_current_loading_scope_miss(
        &warm,
        "current loading does not exactly cover the candidate's local repositories",
    );
    let proto = &cold.stdout;
    let rules: Vec<_> = messages(proto, 1)
        .into_iter()
        .flat_map(|ct| messages(ct, 1))
        .flat_map(|t| messages(t, 2))
        .collect();
    assert!(rules.iter().any(|r| text(r, 1) == "//:one"));
    assert!(!rules.iter().any(|r| text(r, 1) == "//:two"));
    // Genuine proto keeps the late-bound attribute's loading default empty;
    // actual selected plugins appear in configured_rule_input, in exec cfg.
    let alias = rules
        .iter()
        .find(|r| text(r, 1) == "//tools/jdk:java_plugins_flag_alias")
        .unwrap();
    let attrs = messages(alias, 4);
    let attr = attrs
        .iter()
        .find(|a| text(a, 1) == ":java_plugins")
        .unwrap();
    assert!(messages(attr, 6).is_empty());
    let edges = messages(alias, 15);
    let edge = edges.iter().find(|e| text(e, 1) == "//:one").unwrap();
    let configs = messages(proto, 2);
    let exec = configs
        .iter()
        .find(|c| number(c, 1) == number(edge, 3))
        .unwrap();
    assert_eq!(number(exec, 5), 1, "plugin must be exec configured");
    assert!(!f.path().join("cache/actions").exists());
    let two = f.ok(
        "cquery",
        &[
            "--plugin=//:two",
            "--output=proto",
            "--transitions=lite",
            "deps(//tools/jdk:java_plugins_flag_alias)",
        ],
    );
    assert!(!String::from_utf8_lossy(&two.stderr).contains("cquery-snapshot: hit"));
    assert!(
        !messages(&two.stdout, 1)
            .into_iter()
            .flat_map(|ct| messages(ct, 1))
            .flat_map(|t| messages(t, 2))
            .any(|r| text(r, 1) == "//:one")
    );
    f.write("input.txt", "edited source\n");
    let edited = f.ok("cquery", &args);
    // A graph snapshot contains File identity, not source contents. As in
    // docs/CQUERY.md, TD reads contents itself; this is a valid graph hit.
    assert_current_loading_scope_miss(
        &edited,
        "current loading does not exactly cover the candidate's local repositories",
    );
    assert_eq!(edited.stdout, warm.stdout);
    fs::remove_file(f.path().join("input.txt")).unwrap();
    let edited = f.ok("cquery", &args);
    assert!(String::from_utf8_lossy(&edited.stderr).contains("cquery-snapshot: miss"));
    f.write("input.txt", "restored source\n");
    let restored = f.ok("cquery", &args);
    assert_current_loading_scope_miss(
        &restored,
        "current loading does not exactly cover the candidate's local repositories",
    );
    assert_eq!(restored.stdout, cold.stdout);
    let bzl = fs::read_to_string(f.path().join("plugin.bzl")).unwrap();
    f.write("plugin.bzl", &(bzl + "\n# changed plugin implementation\n"));
    let edited = f.ok("cquery", &args);
    assert!(String::from_utf8_lossy(&edited.stderr).contains("cquery-snapshot: miss"));
    let out = f.ok(
        "cquery",
        &[
            "--plugin=//:one",
            "--workspace_status_command=touch status-was-run",
            "//tools/jdk:java_plugins_flag_alias",
        ],
    );
    assert!(!f.path().join("status-was-run").exists());
    assert!(!String::from_utf8_lossy(&out.stderr).contains("actions executed"));
}

// analysis_v2.proto ConfiguredTarget(1,3), Configuration(1,5),
// build.proto Target.rule(2), Rule.attribute(4)/configured_rule_input(15).
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
            bazel_rs::reapi::Value::Varint(nr) if n == field => Some(nr),
            _ => None,
        })
        .unwrap_or(0)
}
