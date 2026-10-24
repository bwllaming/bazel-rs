//! `ctx.actions.declare_shareable_artifact` (StarlarkActionFactory
//! .createShareableArtifact, Bazel 9.3 analysis/starlark/
//! StarlarkActionFactory.java:1267-1276) and the rules_cc 0.2.17
//! virtual-include path it serves (cc/private/compile/cc_compilation_helper.bzl
//! `_compute_public_headers`: strip_include_prefix/include_prefix, the
//! `_virtual_includes` symlinks of `ctx.actions.symlink`, the include
//! directory of the CcCompilationContext). Every case of
//! tests/fixtures/cc_virtual_includes/reference_receipt.json — genuine Bazel
//! 9.3.0 (`--batch --ignore_all_rc_files`, a removed scratch output user
//! root) on the fixture workspace plus tests/cc_compile.rs's vendored modules
//! — runs natively here and is compared with the genuine capture: success,
//! a failing case's message (or a declared divergence's native message), the
//! top-level outputs, the `_virtual_includes` layout and every other output
//! (compiled and linked bytes when the host compiler is the capture's).
#![allow(clippy::disallowed_methods)] // test harness: starts processes directly (clippy.toml)

include!("support/executable_script.rs");
include!("support/module_archives.rs");

use std::collections::BTreeMap;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use serde_json::Value;
use sha2::{Digest, Sha256};

/// tests/cc_compile.rs's module closure: stubs and vendored genuine modules.
const STUBBED: &[&str] = &[
    "buildozer",
    "rules_java",
    "rules_python",
    "rules_shell",
    "zlib",
];
const VENDORED: &[&str] = &[
    "rules_cc",
    "bazel_skylib",
    "platforms",
    "bazel_features",
    "package_metadata",
    "apple_support",
    "hermetic_cc_toolchain",
];

/// The receipt's cases, each in exactly one test below; `true`: the group
/// needs a host C compiler (@local_config_cc).
const GROUPS: &[(&str, bool, &[&str])] = &[
    (
        "cc",
        true,
        &[
            "cc_libraries",
            "cc_app",
            "cc_app_nobuild",
            "cc_hdr_and_textual",
        ],
    ),
    (
        "rules_cc_errors",
        true,
        &[
            "err_not_under",
            "err_uplevel_include_prefix",
            "err_absolute_include_prefix",
            "err_uplevel_strip",
        ],
    ),
    (
        "probes",
        false,
        &[
            "probes_ok",
            "dup_same_writes",
            "share_same",
            "share_diff",
            "misuse_orphan",
            "misuse_two_writes",
            "consumed_external_path",
        ],
    ),
    (
        "arguments",
        false,
        &[
            "misuse_named",
            "misuse_int_path",
            "misuse_too_many",
            "denied_declare",
            "denied_int_path",
            "denied_symlink",
        ],
    ),
    (
        "bazel_crashes",
        false,
        &[
            "misuse_root_string",
            "misuse_root_none",
            "misuse_source_root",
            "misuse_uplevel",
            "misuse_absolute",
            "misuse_empty",
        ],
    ),
    (
        "sharing",
        false,
        &[
            "share_shell_same",
            "share_tool_same",
            "share_tool_diff",
            "share_args_same",
        ],
    ),
    (
        "cc_sharing",
        true,
        &["cc_compile_owners", "cc_compile_one_owner"],
    ),
];

fn manifest() -> &'static Path {
    Path::new(env!("CARGO_MANIFEST_DIR"))
}

fn fixtures() -> PathBuf {
    manifest().join("tests/fixtures")
}

fn fixture() -> PathBuf {
    fixtures().join("cc_virtual_includes/workspace")
}

fn receipt() -> Value {
    serde_json::from_slice(
        &fs::read(fixtures().join("cc_virtual_includes/reference_receipt.json")).unwrap(),
    )
    .unwrap()
}

fn host_compiler() -> Option<String> {
    let output = Command::new("gcc")
        .arg("--version")
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .output()
        .ok()?;
    let text = String::from_utf8_lossy(&output.stdout).into_owned();
    output
        .status
        .success()
        .then(|| text.lines().next().unwrap_or_default().to_owned())
}

fn copy_tree(from: &Path, to: &Path) {
    fs::create_dir_all(to).unwrap();
    for entry in fs::read_dir(from).unwrap() {
        let entry = entry.unwrap();
        let target = to.join(entry.file_name());
        if entry.file_type().unwrap().is_dir() {
            copy_tree(&entry.path(), &target);
        } else {
            // An executable fixture is written by a separate process: no
            // descriptor of it in this (spawning) test process (ETXTBSY).
            let mode = fs::metadata(entry.path()).unwrap().permissions().mode() & 0o7777;
            if mode & 0o111 != 0 {
                write_executable_script(&target, fs::read(entry.path()).unwrap(), mode);
            } else {
                fs::copy(entry.path(), target).unwrap();
            }
        }
    }
}

/// The generated MODULE.bazel (the capture's, verbatim).
fn module_text() -> String {
    let mut module = String::from(
        "module(name = \"cc_virtual_includes_fixture\")\n\n\
         bazel_dep(name = \"rules_cc\", version = \"0.2.17\")\n\
         bazel_dep(name = \"platforms\", version = \"1.1.0\")\n\
         bazel_dep(name = \"bazel_skylib\", version = \"1.9.0\")\n\
         bazel_dep(name = \"hermetic_cc_toolchain\", version = \"4.1.0\")\n\
         bazel_dep(name = \"ext\", version = \"1.0\")\n\
         local_path_override(module_name = \"ext\", path = \"ext\")\n",
    );
    for name in VENDORED {
        module.push_str(&format!(
            "local_path_override(module_name = \"{name}\", path = \"vendor/{name}\")\n"
        ));
    }
    let stubs = fixtures().join("test_flags/stubs");
    for name in STUBBED {
        module.push_str(&format!(
            "local_path_override(module_name = \"{name}\", path = \"{}\")\n",
            stubs.join(name).display()
        ));
    }
    module.push_str(&format!(
        "local_path_override(module_name = \"protobuf\", path = \"{}\")\n",
        fixtures().join("cc_toolchain/stubs/protobuf").display()
    ));
    module
}

struct Workspace {
    keep: tempfile::TempDir,
}

impl Workspace {
    /// The fixture workspace plus tests/cc_compile.rs's vendored genuine
    /// modules (the N25d toolchain subset, its shared files and the
    /// cc_library/cc_binary overlay).
    fn new() -> Self {
        let base = std::env::var_os("TMPDIR")
            .map(PathBuf::from)
            .unwrap_or_else(std::env::temp_dir);
        let keep = tempfile::Builder::new()
            .prefix("cc-virtual-includes-")
            .tempdir_in(base)
            .unwrap();
        let ws = keep.path().join("ws");
        copy_tree(&fixture(), &ws);
        let toolchain = fixtures().join("cc_toolchain");
        let shared = fixtures().join("cc_features/vendor");
        for relative in fs::read_to_string(toolchain.join("vendor_shared.txt"))
            .unwrap()
            .lines()
        {
            let target = ws.join("vendor").join(relative);
            fs::create_dir_all(target.parent().unwrap()).unwrap();
            fs::copy(shared.join(relative), target).unwrap();
        }
        copy_tree(&toolchain.join("vendor"), &ws.join("vendor"));
        copy_tree(
            &fixtures().join("cc_compile/workspace/vendor"),
            &ws.join("vendor"),
        );
        fs::write(ws.join("MODULE.bazel"), module_text()).unwrap();
        for directory in ["home", "tmp"] {
            fs::create_dir(keep.path().join(directory)).unwrap();
        }
        Self { keep }
    }

    fn ws(&self) -> PathBuf {
        self.keep.path().join("ws")
    }

    fn cache(&self, name: &str) -> PathBuf {
        self.keep.path().join(format!("cache-{name}"))
    }

    /// `ARGV[0] --experimental-builtin-modules --registry=... ARGV[1..]`.
    fn run(&self, name: &str, argv: &[String]) -> Output {
        let registry = format!(
            "--registry=file://{}",
            fixtures().join("builtin_modules/registry").display()
        );
        // Offline: @rules_license (bazel_tools' dependency) is materialized.
        seed_module_archives(&self.cache(name), &["rules_license@1.0.0"]);
        let output = Command::new("timeout")
            .args(["--kill-after=5s", "900s", env!("CARGO_BIN_EXE_bazel-rs")])
            .arg("--ignore_all_rc_files")
            .arg(format!("--cache-dir={}", self.cache(name).display()))
            .arg(&argv[0])
            .args(["--experimental-builtin-modules", &registry])
            .args(&argv[1..])
            .current_dir(self.ws())
            .env_clear()
            .env("PATH", "/usr/bin:/bin")
            .env("HOME", self.keep.path().join("home"))
            .env("TMPDIR", self.keep.path().join("tmp"))
            .env("LANG", "C.UTF-8")
            .env("LC_ALL", "C.UTF-8")
            .output()
            .unwrap();
        assert_ne!(output.status.code(), Some(124), "CLI timed out: {argv:?}");
        output
    }

    /// `text` with the workspace and the case's cache as `<WS>`/`<CACHE>`.
    fn normalize(&self, name: &str, text: &str) -> String {
        let mut text = text.to_owned();
        for (path, token) in [(self.ws(), "<WS>"), (self.cache(name), "<CACHE>")] {
            if let Ok(real) = fs::canonicalize(&path) {
                text = text.replace(&real.display().to_string(), token);
            }
            text = text.replace(&path.display().to_string(), token);
        }
        text
    }

    /// Every entry below a target-configuration `bazel-out/<config>/bin` of
    /// the published action outputs, like the capture's `describe_tree`.
    fn outputs(&self, name: &str) -> BTreeMap<String, Entry> {
        let mut found = BTreeMap::new();
        let Ok(actions) = fs::read_dir(self.cache(name).join("actions")) else {
            return found;
        };
        for action in actions {
            let out = action.unwrap().path().join("outputs/bazel-out");
            let Ok(configs) = fs::read_dir(&out) else {
                continue;
            };
            for config in configs {
                let config = config.unwrap().file_name().to_string_lossy().into_owned();
                if config.contains("exec") {
                    continue;
                }
                let root = out.join(&config).join("bin");
                self.describe(
                    name,
                    &root,
                    &root,
                    &format!("bazel-out/{config}/bin/"),
                    &mut found,
                );
            }
        }
        found
    }

    fn describe(
        &self,
        name: &str,
        root: &Path,
        directory: &Path,
        prefix: &str,
        found: &mut BTreeMap<String, Entry>,
    ) {
        let Ok(entries) = fs::read_dir(directory) else {
            return;
        };
        for entry in entries {
            let path = entry.unwrap().path();
            let relative = path.strip_prefix(root).unwrap().display().to_string();
            let metadata = fs::symlink_metadata(&path).unwrap();
            let key = format!("{prefix}{relative}");
            if metadata.file_type().is_symlink() {
                let target = fs::read_link(&path).unwrap().display().to_string();
                found.insert(
                    key,
                    Entry {
                        target: Some(self.normalize(name, &target)),
                        sha256: fs::read(&path).ok().map(|bytes| sha256(&bytes)),
                        text: None,
                    },
                );
            } else if metadata.is_dir() {
                if !relative.ends_with(".runfiles") {
                    self.describe(name, root, &path, prefix, found);
                }
            } else {
                let bytes = fs::read(&path).unwrap();
                found.insert(
                    key,
                    Entry {
                        target: None,
                        sha256: Some(sha256(&bytes)),
                        text: String::from_utf8(bytes)
                            .ok()
                            .map(|text| self.normalize(name, &text)),
                    },
                );
            }
        }
    }
}

#[derive(Debug)]
struct Entry {
    target: Option<String>,
    sha256: Option<String>,
    text: Option<String>,
}

fn sha256(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

/// Compiled and linked outputs: their bytes depend on the host compiler.
fn compiler_dependent(path: &str) -> bool {
    path.contains("/_objs/")
        || path.ends_with(".a")
        || path.ends_with(".so")
        || path.ends_with("/app/app")
}

/// `<CACHE>/actions/<key>/outputs/` as `<OUT>/` (runfiles manifests).
fn output_roots(text: &str) -> String {
    let mut result = String::new();
    let mut rest = text;
    while let Some(start) = rest.find("<CACHE>/actions/") {
        result.push_str(&rest[..start]);
        let after = &rest[start + "<CACHE>/actions/".len()..];
        match after.find("/outputs/") {
            Some(end) => {
                result.push_str("<OUT>/");
                rest = &after[end + "/outputs/".len()..];
            }
            None => {
                result.push_str("<CACHE>/actions/");
                rest = after;
            }
        }
    }
    result.push_str(rest);
    result
}

fn check(name: &str, case: &Value, same_compiler: bool) {
    let argv: Vec<String> = case["argv"]
        .as_array()
        .unwrap()
        .iter()
        .map(|arg| arg.as_str().unwrap().to_owned())
        .collect();
    let ws = Workspace::new();
    let output = ws.run(name, &argv);
    let stderr = ws.normalize(name, &String::from_utf8_lossy(&output.stderr));
    let stdout = ws.normalize(name, &String::from_utf8_lossy(&output.stdout));
    assert!(!stderr.contains("panicked"), "{name}: {stderr}");
    if case["verdict"] == "DECLARED_DIVERGENCE" {
        // Every declared divergence is a native failure with its own text.
        assert!(!output.status.success(), "{name}: {stderr}");
        let message = case["native_message"].as_str().unwrap();
        assert!(
            stderr.contains(message),
            "{name}: missing {message:?} in {stderr}"
        );
        return;
    }
    assert_eq!(case["verdict"], "MATCH", "{name}");
    let genuine_ok = case["genuine"]["exit"].as_i64() == Some(0);
    assert_eq!(
        output.status.success(),
        genuine_ok,
        "{name} {argv:?}: {stderr}"
    );
    if !genuine_ok {
        let message = case["expected_message"].as_str().unwrap();
        assert!(
            stderr.contains(message),
            "{name}: missing {message:?} in {stderr}"
        );
        // An action conflict names its owners' labels in the next line
        // (genuine orders them by analysis timing: compared as a set).
        if let Some(expected) = case["expected_labels"].as_array() {
            let line = stderr
                .lines()
                .skip_while(|line| !line.ends_with(message))
                .nth(1)
                .and_then(|line| line.strip_prefix("Label: "))
                .unwrap_or_else(|| panic!("{name}: no Label line in {stderr}"));
            let mut labels = line.split(", ").collect::<Vec<_>>();
            labels.sort_unstable();
            let expected = expected
                .iter()
                .map(|label| label.as_str().unwrap())
                .collect::<Vec<_>>();
            assert_eq!(labels, expected, "{name}: {stderr}");
        }
        return;
    }
    if argv.iter().any(|arg| arg == "--nobuild") {
        return;
    }
    // The top-level outputs: genuine --show_result, native LABEL<TAB>PATH.
    let mut top_level = stdout
        .lines()
        .filter_map(|line| line.split_once('\t'))
        .map(|(label, path)| {
            let relative = path.split_once("/outputs/").map_or(path, |(_, rest)| rest);
            (label.to_owned(), relative.to_owned())
        })
        .collect::<Vec<_>>();
    top_level.sort();
    let expected = case["top_level_outputs"]
        .as_array()
        .unwrap()
        .iter()
        .map(|pair| {
            (
                pair[0].as_str().unwrap().to_owned(),
                pair[1].as_str().unwrap().to_owned(),
            )
        })
        .collect::<Vec<_>>();
    assert_eq!(top_level, expected, "{name}: {stdout}");
    let outputs = ws.outputs(name);
    for (path, expected) in case["outputs"].as_object().unwrap() {
        let actual = outputs
            .get(path)
            .unwrap_or_else(|| panic!("{name}: no native {path} in {:?}", outputs.keys()));
        if expected["kind"] == "symlink" {
            // The `_virtual_includes` layout: where each symlink points and
            // the (source) bytes it resolves to.
            assert_eq!(
                actual.target.as_deref(),
                expected["target"].as_str(),
                "{name}: {path}"
            );
            assert_eq!(
                actual.sha256.as_deref(),
                expected["sha256"].as_str(),
                "{name}: {path}"
            );
        } else if path.ends_with(".runfiles_manifest") {
            assert_eq!(
                actual.text.as_deref().map(output_roots).as_deref(),
                expected["text"].as_str(),
                "{name}: {path}"
            );
        } else if same_compiler || !compiler_dependent(path) {
            assert_eq!(actual.target, None, "{name}: {path}");
            assert_eq!(
                actual.sha256.as_deref(),
                expected["sha256"].as_str(),
                "{name}: {path}"
            );
        } else if let Some(text) = expected["text"].as_str() {
            // Another compiler: the dependency file still names every
            // virtual header the genuine one does.
            let native = actual.text.as_deref().unwrap_or_default();
            for header in text
                .split_whitespace()
                .filter(|word| word.contains("/_virtual_includes/"))
            {
                assert!(native.contains(header), "{name}: {path}: {header}");
            }
        }
    }
}

fn check_group(group: &str) {
    let receipt = receipt();
    let (_, needs_compiler, names) = GROUPS.iter().find(|(name, ..)| *name == group).unwrap();
    let compiler = host_compiler();
    if *needs_compiler && compiler.is_none() {
        eprintln!("skipped: no host C compiler for cc_configure");
        return;
    }
    let same_compiler = compiler.as_deref() == receipt["host_compiler"].as_str();
    for name in *names {
        let case = &receipt["cases"][name];
        assert!(case.is_object(), "{name} is not in the receipt");
        check(name, case, same_compiler);
    }
}

#[test]
fn receipt_covers_the_fixture_and_every_case_once() {
    let receipt = receipt();
    let mut files = BTreeMap::new();
    let mut stack = vec![fixture()];
    while let Some(directory) = stack.pop() {
        for entry in fs::read_dir(&directory).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                stack.push(path);
            } else {
                let relative = path.strip_prefix(fixture()).unwrap();
                files.insert(
                    relative.display().to_string(),
                    Value::String(sha256(&fs::read(&path).unwrap())),
                );
            }
        }
    }
    let recorded = receipt["fixture_files_sha256"].as_object().unwrap();
    assert_eq!(
        files,
        recorded
            .iter()
            .map(|(path, digest)| (path.clone(), digest.clone()))
            .collect::<BTreeMap<_, _>>()
    );
    let mut grouped = GROUPS
        .iter()
        .flat_map(|(_, _, names)| names.iter().copied())
        .collect::<Vec<_>>();
    grouped.sort_unstable();
    let mut cases = receipt["cases"]
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect::<Vec<_>>();
    cases.sort_unstable();
    assert_eq!(grouped, cases);
    for (name, case) in receipt["cases"].as_object().unwrap() {
        assert!(
            case["verdict"] == "MATCH" || case["verdict"] == "DECLARED_DIVERGENCE",
            "{name}: {}",
            case["verdict"]
        );
    }
}

/// strip_include_prefix (package-relative, repository-absolute, in an
/// external module), include_prefix and textual headers: the
/// `_virtual_includes` symlinks, a consumer compiling against them, a
/// linked cc_binary, `--nobuild`, and one header both public and textual
/// (two identical symlink actions, shared).
#[test]
fn virtual_includes_compile_and_link_like_genuine() {
    check_group("cc");
}

/// rules_cc's own checks of the prefixes, before any artifact is declared.
#[test]
fn rules_cc_prefix_errors_match() {
    check_group("rules_cc_errors");
}

/// The API from an allowlisted package: root-relative paths, the artifact
/// roots, owner/short_path/root/repr, dedup, shared and conflicting actions
/// (within a target, and of two targets on one shareable path), orphans,
/// and a main-repository artifact below `external/` consumed by another
/// target: one File through ctx.files, DefaultInfo and a provider field,
/// short_path `../somerepo/x.txt` (review F2 of d2fd11c).
#[test]
fn shareable_artifacts_are_root_relative_and_deduplicated() {
    check_group("probes");
}

/// Positional-only parameters, the `path` type and the private-API
/// restriction (also for symlink's use_exec_root_for_source).
#[test]
fn argument_and_allowlist_errors_match() {
    check_group("arguments");
}

/// Misuse that crashes genuine Bazel 9.3 is an explicit native error.
#[test]
fn bazel_crashes_are_explicit_errors() {
    check_group("bazel_crashes");
}

/// Actions.canBeShared within one target for any output: identical spawns
/// are shared (also with a tool that has runfiles: the native key covers
/// its runfiles tree), differing ones conflict, and equal registrations with
/// a part the native key does not cover (contents expanding tree artifacts)
/// are explicit errors (a declared divergence).
#[test]
fn identical_actions_share_and_uncompared_parts_are_explicit() {
    check_group("sharing");
}

/// C++ compiles are not shareable (CppCompileActionBuilder.java:99): two
/// targets' identical compiles on one object conflict (Bazel's lines, both
/// labels), one target's identical duplicate is one action and builds the
/// genuine object.
#[test]
fn cc_compiles_share_only_within_one_owner() {
    check_group("cc_sharing");
}
