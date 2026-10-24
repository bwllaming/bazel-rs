//! CLI: the C++ toolchain feature engine (docs/CC_FEATURES.md) through the
//! genuine rules_cc 0.2.17 Starlark API. The workspace is
//! tests/fixtures/cc_features: a probe rule in `tools/build_defs/cc` (on the
//! private-API allowlists) and a test-only vendored copy of the rules_cc /
//! bazel_skylib files it reaches (see that directory's PROVENANCE.md).
//! Expected values follow Bazel 9.3's CcToolchainFeatures, FeatureSelection
//! and CcToolchainVariables semantics; a paired genuine capture is pending.
#![allow(clippy::disallowed_methods)] // test harness: starts processes directly (clippy.toml)

include!("support/executable_script.rs");
include!("support/verify_env.rs");

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

const FIXTURE: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/cc_features");

/// Every scenario of tools/build_defs/cc/probe.bzl.
const SCENARIOS: [&str; 16] = [
    "selection",
    "expansion",
    "with_features",
    "legacy",
    "error_duplicate_feature",
    "error_undefined_implied",
    "error_undefined_required",
    "error_unknown_action_tool",
    "error_no_matching_tool",
    "error_missing_variable",
    "error_non_sequence",
    "error_missing_field",
    "error_malformed",
    "error_provides",
    "error_unconditional",
    "error_artifact_pattern",
];

struct Workspace(tempfile::TempDir);

impl Workspace {
    fn new() -> Self {
        let ws = Self(
            tempfile::Builder::new()
                .prefix("cc-features-cli-")
                .tempdir()
                .unwrap(),
        );
        copy_tree(Path::new(FIXTURE), ws.path());
        for directory in ["home", "tmp"] {
            fs::create_dir(ws.path().join(directory)).unwrap();
        }
        let mut build = String::from(
            "load('//tools/build_defs/cc:probe.bzl', 'cc_feature_probe')\nload('//outside:probe.bzl', 'outside_probe')\n",
        );
        for scenario in SCENARIOS {
            build.push_str(&format!(
                "cc_feature_probe(name = '{scenario}', scenario = '{scenario}')\n"
            ));
        }
        build.push_str("outside_probe(name = 'outside')\n");
        fs::write(ws.path().join("BUILD.bazel"), build).unwrap();
        ws
    }

    fn path(&self) -> &Path {
        self.0.path()
    }

    fn build(&self, targets: &[&str]) -> Output {
        let binary = PathBuf::from(env!("CARGO_BIN_EXE_bazel-rs"));
        let output = Command::new("/usr/bin/timeout")
            .args(["--kill-after=5s", "600s"])
            .arg(binary)
            .args(["--ignore_all_rc_files", "--cache-dir=cache", "build"])
            .args(targets)
            .current_dir(self.path())
            .env_clear()
            .envs(verifier_environment())
            .env("PATH", "/usr/bin:/bin")
            .env("HOME", self.path().join("home"))
            .env("TMPDIR", self.path().join("tmp"))
            .env("LC_ALL", "C")
            .output()
            .unwrap();
        assert!(
            !matches!(output.status.code(), None | Some(124 | 137)),
            "killed or timed out: {}",
            errors(&output)
        );
        assert!(!errors(&output).contains("panicked"), "{}", errors(&output));
        output
    }
}

fn copy_tree(from: &Path, to: &Path) {
    for entry in fs::read_dir(from).unwrap() {
        let entry = entry.unwrap();
        let target = to.join(entry.file_name());
        if entry.file_type().unwrap().is_dir() {
            fs::create_dir_all(&target).unwrap();
            copy_tree(&entry.path(), &target);
        } else {
            // An executable fixture is written by a separate process: no
            // descriptor of it in this (spawning) test process (ETXTBSY).
            copy_file(&entry.path(), &target);
        }
    }
}

fn errors(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

fn parse(text: &str) -> serde_json::Value {
    serde_json::from_str(text).unwrap()
}

/// Expected `//:selection` output.
const SELECTION: &str = r#"{
    "action_is_enabled": [
        true,
        false,
        false
    ],
    "cached_order": "<FeatureConfiguration(ENABLED:a, b, c;REQUESTED:a, unknown)>",
    "configurations": [
        "<FeatureConfiguration(ENABLED:a, b, c;REQUESTED:a)>",
        "<FeatureConfiguration(ENABLED:a, b, c, d;REQUESTED:d, a)>",
        "<FeatureConfiguration(ENABLED:d, e;REQUESTED:d, e)>",
        "<FeatureConfiguration(ENABLED:;REQUESTED:d)>",
        "<FeatureConfiguration(ENABLED:;REQUESTED:implies_needs_gate)>",
        "<FeatureConfiguration(ENABLED:needs_gate, gate, implies_needs_gate;REQUESTED:implies_needs_gate, gate)>",
        "<FeatureConfiguration(ENABLED:p1;REQUESTED:p1, unknown)>",
        "<FeatureConfiguration(ENABLED:e;REQUESTED:c-compile)>",
        "<FeatureConfiguration(ENABLED:c, on, off;REQUESTED:off, on, d, c)>"
    ],
    "defaults": [
        "on",
        "c-compile"
    ],
    "is_enabled": [
        true,
        true,
        true,
        false
    ],
    "is_requested": [
        true,
        false,
        true
    ],
    "types": [
        "CcToolchainFeatures",
        "FeatureConfiguration",
        "<unknown object com.google.devtools.build.lib.rules.cpp.FeatureConfigurationForStarlark>"
    ]
}"#;

/// Expected `//:expansion` output.
const EXPANSION: &str = r#"{
    "compile": [
        "-lit",
        "%pct%",
        "preSpost",
        "a/c",
        "avail-str",
        "not-avail-missing",
        "not-avail-none",
        "true-t",
        "true-list",
        "false-f",
        "false-empty",
        "equal-str",
        "equal-bool",
        "-t=1",
        "-f=0",
        "-Ix",
        "-Iy",
        "-Tt1",
        "-Dd1",
        "-Dd2",
        "-Kk1",
        "-Kk2",
        "-lm",
        "dep=dx",
        "dep=dy",
        "z.a",
        "nested=L",
        "flat=flat",
        "label=//tools/build_defs/cc:src.txt",
        "file=tools/build_defs/cc/src.txt",
        "struct-field",
        "parent=PQ"
    ],
    "empty_variables": [
        "Variables",
        "<unknown object com.google.devtools.build.lib.rules.cpp.CcToolchainVariables$MapVariables>"
    ],
    "env": {
        "PCT": "100%",
        "STR": "S"
    },
    "link": [
        "-link-only"
    ],
    "unconfigured_action": []
}"#;

/// Expected `//:with_features` output.
const WITH_FEATURES: &str = r#"{
    "results": [
        {
            "action_is_enabled": true,
            "compile": [
                "-from-action-config",
                "-always",
                "-O0"
            ],
            "env": {},
            "execution_requirements": [
                "local",
                "no-remote"
            ],
            "link_tool": "tools/build_defs/cc/tool.sh",
            "requested": [
                "x",
                "c-compile"
            ],
            "strip_tool": "tools/build_defs/up/strip",
            "tool": "tools/build_defs/cc/bin/cc",
            "tool_requirement": [
                "local",
                "no-remote"
            ]
        },
        {
            "action_is_enabled": true,
            "compile": [
                "-from-action-config",
                "-always",
                "-O2"
            ],
            "env": {
                "MODE": "opt"
            },
            "execution_requirements": [
                "requires-opt"
            ],
            "link_tool": "tools/build_defs/cc/tool.sh",
            "requested": [
                "x",
                "c-compile",
                "opt"
            ],
            "strip_tool": "tools/build_defs/up/strip",
            "tool": "/opt/bin/cc",
            "tool_requirement": [
                "requires-opt"
            ]
        },
        {
            "action_is_enabled": true,
            "compile": [
                "-from-action-config",
                "-always",
                "-O2",
                "-g"
            ],
            "env": {
                "MODE": "opt"
            },
            "execution_requirements": [
                "requires-opt"
            ],
            "link_tool": "tools/build_defs/cc/tool.sh",
            "requested": [
                "x",
                "c-compile",
                "opt",
                "dbg"
            ],
            "strip_tool": "tools/build_defs/up/strip",
            "tool": "/opt/bin/cc",
            "tool_requirement": [
                "requires-opt"
            ]
        },
        {
            "action_is_enabled": true,
            "compile": [
                "-from-action-config",
                "-always",
                "-O0",
                "-g"
            ],
            "env": {},
            "execution_requirements": [
                "local",
                "no-remote"
            ],
            "link_tool": "tools/build_defs/cc/tool.sh",
            "requested": [
                "x",
                "c-compile",
                "dbg"
            ],
            "strip_tool": "tools/build_defs/up/strip",
            "tool": "tools/build_defs/cc/bin/cc",
            "tool_requirement": [
                "local",
                "no-remote"
            ]
        },
        {
            "action_is_enabled": false,
            "compile": [
                "-always",
                "-O0"
            ],
            "env": {},
            "execution_requirements": [
                "local",
                "no-remote"
            ],
            "link_tool": "tools/build_defs/cc/tool.sh",
            "requested": [
                "x"
            ],
            "strip_tool": "tools/build_defs/up/strip",
            "tool": "tools/build_defs/cc/bin/cc",
            "tool_requirement": [
                "local",
                "no-remote"
            ]
        }
    ]
}"#;

/// Expected `//:legacy` output.
const LEGACY: &str = r#"{
    "archive_tool": "/usr/bin/ar",
    "compile": [
        "-frandom-seed=out/a.o",
        "-DNDEBUG",
        "-iquote",
        ".",
        "-Iinc",
        "-isystem",
        "/usr/include",
        "-mine",
        "-Wall",
        "--sysroot=/sysroot",
        "-c",
        "src/a.c",
        "-o",
        "out/a.o"
    ],
    "compile_tool": "/usr/bin/gcc",
    "legacy_configuration": "<FeatureConfiguration(ENABLED:legacy_compile_flags, dependency_file, random_seed, pic, preprocessor_defines, includes, include_paths, build_interface_libraries, dynamic_library_linker_tool, shared_flag, linkstamps, output_execpath_flags, runtime_library_search_directories, library_search_directories, archiver_flags, libraries_to_link, force_pic_flags, user_link_flags, legacy_link_flags, fission_support, strip_debug_symbols, mine, user_compile_flags, sysroot, unfiltered_compile_flags, linker_param_file, compiler_input_flags, compiler_output_flags;REQUESTED:no_generate_debug_symbols, fastbuild, c-compile, c++-compile, c++-header-parsing, c++-module-compile, c++-module-codegen, c++-module-deps-scanning, c++20-module-compile, c++20-module-codegen, assemble, preprocess-assemble, clif-match, linkstamp-compile, cc-flags-make-variable, lto-backend, c++-header-analysis, lto-index-for-executable, lto-index-for-dynamic-library, lto-index-for-nodeps-dynamic-library, c++-link-executable, c++-link-dynamic-library, c++-link-nodeps-dynamic-library, c++-link-static-library, strip, mine, legacy_compile_flags, dependency_file, random_seed, pic, preprocessor_defines, includes, include_paths, user_compile_flags, sysroot, compiler_input_flags, compiler_output_flags)>",
    "legacy_defaults": [
        "legacy_compile_flags",
        "dependency_file",
        "random_seed",
        "pic",
        "preprocessor_defines",
        "includes",
        "include_paths",
        "mine",
        "user_compile_flags",
        "sysroot",
        "compiler_input_flags",
        "compiler_output_flags"
    ],
    "link_tool": "/usr/bin/gcc",
    "plain_compile": [
        "-mine"
    ],
    "plain_configuration": "<FeatureConfiguration(ENABLED:mine;REQUESTED:no_generate_debug_symbols, fastbuild, c-compile, c++-compile, c++-header-parsing, c++-module-compile, c++-module-codegen, c++-module-deps-scanning, c++20-module-compile, c++20-module-codegen, assemble, preprocess-assemble, clif-match, linkstamp-compile, cc-flags-make-variable, lto-backend, c++-header-analysis, lto-index-for-executable, lto-index-for-dynamic-library, lto-index-for-nodeps-dynamic-library, c++-link-executable, c++-link-dynamic-library, c++-link-nodeps-dynamic-library, c++-link-static-library, strip, mine)>",
    "plain_defaults": [
        "mine"
    ],
    "plain_has_compile_config": false
}"#;

#[test]
fn feature_selection_expansion_tools_and_legacy_features_match_bazel_semantics() {
    let ws = Workspace::new();
    let output = ws.build(&[
        "//:selection",
        "//:expansion",
        "//:with_features",
        "//:legacy",
    ]);
    assert!(output.status.success(), "{}", errors(&output));
    let mut seen = Vec::new();
    for line in String::from_utf8(output.stdout).unwrap().lines() {
        let (label, path) = line.split_once('\t').unwrap();
        let expected = match label {
            "//:selection" => SELECTION,
            "//:expansion" => EXPANSION,
            "//:with_features" => WITH_FEATURES,
            "//:legacy" => LEGACY,
            other => panic!("unexpected output line {other}"),
        };
        let actual = fs::read_to_string(path).unwrap();
        assert!(actual.ends_with("}\n"), "{actual}");
        assert_eq!(parse(&actual), parse(expected), "{label}: {actual}");
        seen.push(label.to_owned());
    }
    seen.sort();
    assert_eq!(
        seen,
        [
            "//:expansion",
            "//:legacy",
            "//:selection",
            "//:with_features"
        ]
    );
}

#[test]
fn every_error_path_fails_the_target_with_bazel_messages() {
    let ws = Workspace::new();
    for (target, message) in [
        (
            "//:error_duplicate_feature",
            "Invalid toolchain configuration: feature or action config 'dup' was specified multiple times.",
        ),
        (
            "//:error_undefined_implied",
            "Invalid toolchain configuration: feature 'ghost', which is referenced from feature 'a', is not defined.",
        ),
        (
            "//:error_undefined_required",
            "Invalid toolchain configuration: feature 'ghost', which is referenced from feature 'a', is not defined.",
        ),
        (
            "//:error_unknown_action_tool",
            "Action nope does not have an enabled configuration in the toolchain.",
        ),
        (
            "//:error_no_matching_tool",
            "Matching tool for action c-compile not found for given feature configuration",
        ),
        (
            "//:error_missing_variable",
            "Invalid toolchain configuration: Cannot find variable named 'missing'.",
        ),
        (
            "//:error_non_sequence",
            "Invalid toolchain configuration: Cannot expand variable 'v': expected sequence, found string",
        ),
        (
            // Bazel reports the outermost structure path for nested fields.
            "//:error_missing_field",
            "Invalid toolchain configuration: Cannot expand variable 's.nope': structure s doesn't have a field named 'nope'",
        ),
        (
            "//:error_malformed",
            "Invalid toolchain configuration: expected variable name at position 3 while parsing a flag containing '-%{'",
        ),
        (
            "//:error_provides",
            "Symbol sym is provided by all of the following features: p1 p2",
        ),
        (
            "//:error_unconditional",
            "The C++ toolchain '@@//tools/build_defs/cc:stub_toolchain' unconditionally implies feature 'on', which is unsupported by this rule. This is most likely a misconfiguration in the C++ toolchain.",
        ),
        (
            // infoError: Bazel's creation location is the provider call's
            // opening parenthesis; the file is the native repository path.
            "//:error_artifact_pattern",
            "in ArtifactNamePatternInfo instantiated at <WS>/vendor/rules_cc/cc/cc_toolchain_config_lib.bzl:611:35: Unrecognized file extension '.bin', allowed extensions are '', '.exe', or '.wasm', please check artifact_name_pattern configuration for executable in your rule.",
        ),
        (
            "//:outside",
            "file '//outside:probe.bzl' cannot use private API",
        ),
    ] {
        let root = fs::canonicalize(ws.path()).unwrap();
        let message = message.replace("<WS>", &root.display().to_string());
        let output = ws.build(&[target]);
        let diagnostic = errors(&output);
        assert_eq!(output.status.code(), Some(1), "{target}: {diagnostic}");
        assert!(output.stdout.is_empty(), "{target}: partial output");
        assert!(diagnostic.contains(&message), "{target}: {diagnostic}");
    }
}

/// `fs::copy`, except that a file with any execute bit is re-created through
/// `write_executable_script` (bytes and mode kept).
fn copy_file(from: &Path, to: &Path) {
    use std::os::unix::fs::PermissionsExt;
    let mode = fs::metadata(from).unwrap().permissions().mode() & 0o7777;
    if mode & 0o111 != 0 {
        write_executable_script(to, fs::read(from).unwrap(), mode);
    } else {
        fs::copy(from, to).unwrap();
    }
}
