//! Direct Turbine action contract. Uses checksum-pinned, unchanged rules_java
//! provider/toolchain/runtime implementations. The normal suite analyzes with
//! fail-if-executed tool files; the real compiler capture is documented in
//! fixtures/java_actions/README.md (actual native/genuine builds, byte equality).
#![allow(clippy::disallowed_methods)]
include!("support/temp_root.rs");
include!("support/module_archives.rs");

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::{Command, Output};

const PACKAGE: &str = "third_party/bazel_rules/rules_java";
const DEFS: &str =
    include_str!("fixtures/java_actions/workspace/third_party/bazel_rules/rules_java/headers.bzl");

struct Fixture(tempfile::TempDir);

fn copy(from: &Path, to: &Path) {
    fs::create_dir_all(to).unwrap();
    for entry in fs::read_dir(from).unwrap() {
        let entry = entry.unwrap();
        if entry.file_type().unwrap().is_dir() {
            copy(&entry.path(), &to.join(entry.file_name()));
        } else {
            fs::copy(entry.path(), to.join(entry.file_name())).unwrap();
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
            &manifest.join("tests/fixtures/java_actions/workspace"),
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
        let mut module = "module(name='java_actions_fixture')\n".to_owned();
        for (name, version) in [
            ("rules_java", "9.1.0"),
            ("rules_cc", "0.2.17"),
            ("platforms", "1.0.0"),
            ("bazel_skylib", "1.8.2"),
            ("rules_license", "1.0.0"),
        ] {
            let pin: serde_json::Value = serde_json::from_slice(
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
            let hex: String = digest.iter().map(|b| format!("{b:02x}")).collect();
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
            fs::rename(
                staging.join(pin["strip_prefix"].as_str().unwrap_or("")),
                f.path().join(name),
            )
            .unwrap();
            if name != "platforms" {
                empty_builds(&f.path().join(name));
                let deps = match name {
                    "rules_java" => {
                        "bazel_dep(name='rules_cc',version='0.2.17')\nbazel_dep(name='bazel_skylib',version='1.8.2')\nbazel_dep(name='platforms',version='1.0.0')\n"
                    }
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
        // Restrict public exports to the actual java_import implementation
        // used by @bazel_tools' otherwise unrelated TestRunner declaration.
        f.write("rules_java/java/defs.bzl", "load('//java/bazel/rules:bazel_java_import.bzl', _import='java_import')\njava_import=_import\n");
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
        copy(
            &manifest.join("tests/fixtures/java_actions/python"),
            &f.path().join("python_stub/python"),
        );
        f.write(
            "python_stub/MODULE.bazel",
            "module(name='rules_python',version='0.40.0')\n",
        );
        module += "local_path_override(module_name='rules_python',path='python_stub')\n";
        f.write("MODULE.bazel", &module);
        for name in [
            "turbine.jar",
            "javabuilder.jar",
            "singlejar",
            "jdk/bin/java",
            "jdk/lib/ct.sym",
            "sources.jar",
        ] {
            f.write(name, "#!/bin/sh\necho unexpectedly-executed >&2\nexit 97\n");
            fs::set_permissions(f.path().join(name), fs::Permissions::from_mode(0o755)).unwrap();
        }
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
            .env("BAZEL_RS_SNAPSHOT_EXPLAIN", "1")
            .env("BAZEL_RS_DUMP_ACTIONS", "1")
            .args([
                "--ignore_all_rc_files",
                "--cache-dir=cache",
                command,
                "--lockfile_mode=update",
                "--repository_disable_download",
                "--extra_toolchains=//:registered",
                "--memory-limit=2048",
            ])
            .args(args)
            .output()
            .unwrap()
    }

    fn ok(&self, command: &str, args: &[&str]) -> Output {
        let out = self.run(command, args);
        assert!(out.status.success(), "{}", errors(&out));
        out
    }

    fn rejected(&self, expected: &str) {
        let out = self.run("cquery", &[&format!("//{PACKAGE}:base")]);
        assert_eq!(out.status.code(), Some(1), "{}", errors(&out));
        assert!(out.stdout.is_empty());
        assert!(errors(&out).contains(expected), "{}", errors(&out));
        assert!(!errors(&out).contains("panicked"));
    }
}

fn errors(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).into_owned()
}

#[test]
fn turbine_plans_match_captured_argv_and_keep_header_producer_edges_without_execution() {
    let f = Fixture::new();
    let target = format!("//{PACKAGE}:top");
    let out = f.ok("build", &["--nobuild", &target]);
    let dump = errors(&out);
    let top = dump.split(&format!("ACTION {target}#0\n")).nth(1).unwrap();
    let args = top
        .lines()
        .find_map(|l| l.strip_prefix("  arguments "))
        .unwrap()
        .split('\x01')
        .collect::<Vec<_>>();
    let golden: serde_json::Value =
        serde_json::from_str(include_str!("fixtures/java_actions/header_reference.json")).unwrap();
    let expected: Vec<_> = golden["top_argv"]
        .as_array()
        .unwrap()
        .iter()
        .skip(1)
        .map(|v| v.as_str().unwrap())
        .collect();
    assert_eq!(args, expected);
    assert!(top.contains("  env LC_CTYPE=C.UTF-8"));
    assert!(top.contains(&format!("generated //{PACKAGE}:middle#0[2] -> bazel-out/k8-fastbuild/bin/{PACKAGE}/middle-tjar.jar")), "{top}");
    assert!(!top.contains("middle-hjar.jdeps"));
    assert!(!top.contains("base-tjar.jar"));
    let expression = format!("deps({target})");
    f.ok("cquery", &[&expression]);
    let warm = f.ok("cquery", &[&expression]);
    assert!(errors(&warm).contains("hit"), "{}", errors(&warm));
    // A source-content edit is safe for configured-graph reuse, while the
    // compilation still retains the source as an execution input.
    f.write(
        &format!("{PACKAGE}/Top.java"),
        "package sample; public class Top { public int changed; }\n",
    );
    let edited = f.ok("cquery", &[&expression]);
    assert_eq!(warm.stdout, edited.stdout);
    assert!(errors(&edited).contains("hit"));
    f.write(
        &format!("{PACKAGE}/headers.bzl"),
        &DEFS.replace("fixture_header", "edited_header"),
    );
    let changed = f.ok("build", &["--nobuild", &target]);
    assert!(errors(&changed).contains("edited_header"));
}

#[test]
fn turbine_rejects_bad_bindings_provider_identities_and_unsupported_branches() {
    let f = Fixture::new();
    for (from, to, expected) in [
        (
            "ctx, tc, jar",
            "ctx, struct(), jar",
            "want 'JavaToolchainInfo'",
        ),
        (
            "JavaPluginInfo(runtime_deps = [])",
            "struct()",
            "want 'JavaPluginInfo'",
        ),
        ("depset(ctx.files.srcs)", "ctx.files.srcs", "want 'depset'"),
        (
            "ctx.attr.strict_deps",
            "'BAD'",
            "StrictDepsMode BAD not allowed",
        ),
        (
            "ctx.attr.direct",
            "False",
            "non-direct classpath header compilation",
        ),
        ("ctx, tc, jar", "struct(), tc, jar", "ctx"),
        ("ctx.files.extra, tjar,", "ctx.files.extra, None,", "File"),
        (
            "JavaPluginInfo(runtime_deps = [])",
            "JavaPluginInfo(runtime_deps = [], processor_class = 'Processor', generates_api = True)",
            "API-generating annotation processors",
        ),
    ] {
        f.write(&format!("{PACKAGE}/headers.bzl"), &DEFS.replace(from, to));
        f.rejected(expected);
    }
    f.write(&format!("{PACKAGE}/headers.bzl"), &DEFS.replace("ctx, tc, jar, jdeps", "ctx, tc, ctx.file.source_output, jdeps").replace("\"srcs\": attr", "\"source_output\": attr.label(default = ':Base.java', allow_single_file = True),\n        \"srcs\": attr"));
    f.rejected("is not declared by");
    let wrong_identity = DEFS
        .replace(
            "Headers = provider",
            "JavaToolchainInfo = provider()\n\nHeaders = provider",
        )
        .replace("ctx, tc, jar", "ctx, JavaToolchainInfo(), jar");
    f.write(&format!("{PACKAGE}/headers.bzl"), &wrong_identity);
    f.rejected("want 'JavaToolchainInfo' (rules_java provider identity)");
    let undeclared = DEFS
        .replace(
            "tc = ctx.toolchains[\"@bazel_tools//tools/jdk:toolchain_type\"].java",
            "tc = struct(_javacopts = depset())",
        )
        .replace(
            "toolchains = [\"@bazel_tools//tools/jdk:toolchain_type\"]",
            "toolchains = []",
        );
    f.write(&format!("{PACKAGE}/headers.bzl"), &undeclared);
    f.rejected("must declare '@bazel_tools//tools/jdk:toolchain_type'");
}

#[test]
fn turbine_locale_follows_selected_execution_platform_including_inherited_constraints() {
    let f = Fixture::new();
    let build = fs::read_to_string(f.path().join("BUILD.bazel")).unwrap();
    f.write("BUILD.bazel", &(build + "\nplatform(name='mac',constraint_values=['@platforms//os:osx'])\nplatform(name='child',parents=[':mac'])\nplatform(name='linux',constraint_values=['@platforms//os:linux'])\n"));
    let out = f.ok(
        "build",
        &[
            "--nobuild",
            "--extra_execution_platforms=//:child",
            &format!("//{PACKAGE}:base"),
        ],
    );
    assert!(
        errors(&out).contains("env LC_CTYPE=en_US.UTF-8"),
        "{}",
        errors(&out)
    );
    let out = f.ok(
        "build",
        &[
            "--nobuild",
            "--platforms=//:mac",
            "--extra_execution_platforms=//:linux",
            &format!("//{PACKAGE}:base"),
        ],
    );
    assert!(
        errors(&out).contains("env LC_CTYPE=C.UTF-8"),
        "{}",
        errors(&out)
    );
}

#[test]
fn turbine_executable_tool_omits_runtime_and_toolchain_jvm_flags_and_spills_params() {
    let f = Fixture::new();
    let build = fs::read_to_string(f.path().join("BUILD.bazel")).unwrap();
    f.write(
        "BUILD.bazel",
        &build.replace(
            "header_compiler_direct = \"turbine.jar\"",
            "header_compiler_direct = \"singlejar\"",
        ),
    );
    let out = f.ok("build", &["--nobuild", &format!("//{PACKAGE}:base")]);
    let dump = errors(&out);
    assert!(dump.contains("  program singlejar"));
    assert!(!dump.contains("  input ") || !dump.contains(" -> jdk/bin/java"));
    assert!(!dump.contains("-Xmx256m"));
    assert!(dump.contains("-Dturbine.ctSymPath=jdk/lib/ct.sym"));
    let options = (0..400)
        .map(|i| format!("'-Akey{i}={}'", "x".repeat(100)))
        .collect::<Vec<_>>()
        .join(",");
    let defs = DEFS.replace(
        "opts = tc._javacopts",
        &format!("opts = depset([{options}])"),
    );
    f.write(&format!("{PACKAGE}/headers.bzl"), &defs);
    let out = f.ok("build", &["--nobuild", &format!("//{PACKAGE}:base")]);
    let dump = errors(&out);
    assert!(dump.contains("base-hjar.jar-0.params"), "{dump}");
}
