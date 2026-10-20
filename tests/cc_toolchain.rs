//! N25d4/N25d5: genuine rules_cc 0.2.17 `cc_toolchain` / `cc_toolchain_alias`
//! and `cc_toolchain_config` analysis end to end (docs/CC_TOOLCHAIN.md).
//!
//! The workspace is tests/fixtures/cc_toolchain/workspace: a hermetic
//! main-repository `cc_toolchain_config` (F1) registered as the C++
//! toolchain, a probe rule json-encoding the resolved CcToolchainInfo, its
//! feature configuration, command lines and environments, the
//! cc_toolchain/alias providers (TemplateVariableInfo, ToolchainInfo,
//! DefaultInfo) in the target and exec configurations; hermetic_cc_toolchain's
//! genuine zig_cc_toolchain_config; the genuine bazel_cc_build_info
//! translator (F5); and, without the root registration, the host
//! @local_config_cc from rules_cc's genuine cc_configure (F2). The modules are
//! byte copies of the genuine ones (tests/fixtures/cc_features/vendor plus
//! tests/fixtures/cc_toolchain/vendor, PROVENANCE.md), behind
//! local_path_override with N30's builtin-closure stubs. The expected
//! documents are the native outputs; the paired genuine Bazel 9.3 capture is
//! tests/fixtures/cc_toolchain/reference_receipt.json.
#![allow(clippy::disallowed_methods)] // test harness: starts processes directly (clippy.toml)

include!("support/executable_script.rs");

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

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

fn manifest() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn fixture() -> PathBuf {
    manifest().join("tests/fixtures/cc_toolchain")
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
            copy_file(&entry.path(), &target);
        }
    }
}

struct Workspace(tempfile::TempDir);

impl Workspace {
    /// The fixture workspace; `register` adds the root registration of the
    /// fixture toolchain (F1), otherwise rules_cc's host toolchain resolves (F2).
    fn new(register: bool) -> Self {
        let ws = Self(
            tempfile::Builder::new()
                .prefix("cc-toolchain-")
                .tempdir_in("/tmp/opencode")
                .unwrap(),
        );
        let root = ws.0.path();
        copy_tree(&fixture().join("workspace"), root);
        // The genuine files already vendored for N25 (byte-identical), then
        // this fixture's own genuine files.
        let shared = manifest().join("tests/fixtures/cc_features/vendor");
        for relative in fs::read_to_string(fixture().join("vendor_shared.txt"))
            .unwrap()
            .lines()
        {
            let target = root.join("vendor").join(relative);
            fs::create_dir_all(target.parent().unwrap()).unwrap();
            fs::copy(shared.join(relative), target).unwrap();
        }
        copy_tree(&fixture().join("vendor"), &root.join("vendor"));
        let mut module = String::from(
            "module(name = \"cc_toolchain_fixture\")\n\n\
             bazel_dep(name = \"rules_cc\", version = \"0.2.17\")\n\
             bazel_dep(name = \"platforms\", version = \"1.1.0\")\n\
             bazel_dep(name = \"bazel_skylib\", version = \"1.9.0\")\n\
             bazel_dep(name = \"hermetic_cc_toolchain\", version = \"4.1.0\")\n",
        );
        if register {
            module.push_str("register_toolchains(\"//tc:cc_toolchain\")\n");
        }
        for name in VENDORED {
            module.push_str(&format!(
                "local_path_override(module_name = \"{name}\", path = \"vendor/{name}\")\n"
            ));
        }
        let stubs = manifest().join("tests/fixtures/test_flags/stubs");
        for name in STUBBED {
            module.push_str(&format!(
                "local_path_override(module_name = \"{name}\", path = \"{}\")\n",
                stubs.join(name).display()
            ));
        }
        module.push_str(&format!(
            "local_path_override(module_name = \"protobuf\", path = \"{}\")\n",
            fixture().join("stubs/protobuf").display()
        ));
        fs::write(root.join("MODULE.bazel"), module).unwrap();
        for directory in ["home", "tmp"] {
            fs::create_dir(root.join(directory)).unwrap();
        }
        ws
    }

    fn build(&self, args: &[&str]) -> Output {
        let registry = format!(
            "--registry=file://{}",
            manifest()
                .join("tests/fixtures/builtin_modules/registry")
                .display()
        );
        Command::new("/usr/bin/timeout")
            .args(["--kill-after=5s", "600s"])
            .arg(env!("CARGO_BIN_EXE_bazel-rs"))
            .args(["--ignore_all_rc_files", "--cache-dir=cache", "build"])
            .args(["--experimental-builtin-modules", registry.as_str()])
            .args(args)
            .current_dir(self.0.path())
            .env_clear()
            .env("PATH", "/usr/bin:/bin")
            .env("HOME", self.0.path().join("home"))
            .env("TMPDIR", self.0.path().join("tmp"))
            .env("LC_ALL", "C")
            .output()
            .unwrap()
    }
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

/// `label\tpath` lines of a successful build.
fn outputs(output: &Output) -> Vec<(String, PathBuf)> {
    assert!(output.status.success(), "{}", stderr(output));
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter_map(|line| line.split_once('\t'))
        .map(|(label, path)| (label.to_owned(), PathBuf::from(path)))
        .collect()
}

fn output_of(outputs: &[(String, PathBuf)], label: &str) -> String {
    let path = &outputs
        .iter()
        .find(|(seen, _)| seen == label)
        .unwrap_or_else(|| panic!("no output for {label} in {outputs:?}"))
        .1;
    fs::read_to_string(path).unwrap()
}

fn expected(name: &str) -> String {
    fs::read_to_string(fixture().join("expected").join(name)).unwrap()
}

fn json(text: &str) -> serde_json::Value {
    serde_json::from_str(text).unwrap()
}

#[test]
fn fixture_toolchain_target_exec_zig_and_build_info() {
    let ws = Workspace::new(true);
    let par = (0..8)
        .map(|i| format!("//probe:par{i}"))
        .collect::<Vec<_>>();
    let mut targets = vec![
        "//probe:target",
        "//probe:exec",
        "//zig:probe",
        "//zig:exec",
        "//bi:dump",
        "//bi:redacted",
    ];
    targets.extend(par.iter().map(String::as_str));
    let built = outputs(&ws.build(&targets));
    for (label, name) in [
        ("//probe:target", "target.json"),
        ("//probe:exec", "exec.json"),
        ("//zig:probe", "zig_probe.json"),
        ("//zig:exec", "zig_exec.json"),
        ("//bi:dump", "bi_dump.json"),
    ] {
        assert_eq!(output_of(&built, label), expected(name), "{label}");
    }
    // Concurrent analyses of one frozen toolchain: same feature requests,
    // same documents.
    for (index, label) in par.iter().enumerate() {
        let name = if index % 2 == 1 {
            "par_odd.json"
        } else {
            "par_even.json"
        };
        assert_eq!(output_of(&built, label), expected(name), "{label}");
    }
    // The translator's redacted header is a symlink to the genuine template.
    let redacted = output_of(&built, "//bi:redacted");
    assert!(redacted.contains("#define BUILD_TIMESTAMP 0"), "{redacted}");

    let target = json(&expected("target.json"));
    let exec = json(&expected("exec.json"));
    assert_eq!(target["configuration"]["is_tool_configuration"], false);
    assert_eq!(exec["configuration"]["is_tool_configuration"], true);
    assert_eq!(exec["configuration"]["compilation_mode"], "opt");
    assert_eq!(
        target["deps"][0]["TemplateVariableInfo"]["CC_FLAGS"],
        "-legacy-cc-flags --sysroot=/fixture/sysroot"
    );
    // cc_toolchain_alias forwards the toolchain without CC_FLAGS.
    assert!(
        target["deps"][2]["TemplateVariableInfo"]
            .get("CC_FLAGS")
            .is_none()
    );
}

#[test]
fn compilation_mode_dbg_selects_the_dbg_tool_and_features() {
    let ws = Workspace::new(true);
    let built = outputs(&ws.build(&["-c", "dbg", "//probe:target", "//probe:exec"]));
    assert_eq!(
        output_of(&built, "//probe:target"),
        expected("dbg_target.json")
    );
    // The exec configuration is always -c opt.
    assert_eq!(output_of(&built, "//probe:exec"), expected("exec.json"));
}

#[test]
fn stamped_build_info_translates_the_workspace_status() {
    let ws = Workspace::new(true);
    // N63: the translator's stamped group builds — the
    // transform_version_file/transform_info_file actions execute as template
    // expansions of the workspace status (TranslateBuildInfo). The volatile
    // header carries the built-in BUILD_TIMESTAMP; the stable one the
    // built-in user/host pair (machine-dependent values, keyed by name).
    let built = outputs(&ws.build(&["//bi:stamped", "//bi:redacted"]));
    let volatile = built
        .iter()
        .find(|(_, path)| path.ends_with("volatile_file.h"))
        .map(|(_, path)| fs::read_to_string(path).unwrap())
        .unwrap();
    let stable = built
        .iter()
        .find(|(_, path)| path.ends_with("non_volatile_file.h"))
        .map(|(_, path)| fs::read_to_string(path).unwrap())
        .unwrap();
    assert!(
        volatile.starts_with("#define BUILD_SCM_REVISION \"0\"\n"),
        "{volatile}"
    );
    assert!(
        volatile
            .lines()
            .any(|line| line.starts_with("#define BUILD_TIMESTAMP "))
            && volatile.ends_with("\n"),
        "{volatile}"
    );
    let stable_lines: Vec<&str> = stable.lines().collect();
    assert_eq!(stable_lines.len(), 3, "{stable}");
    assert!(
        stable_lines[0].starts_with("#define BUILD_EMBED_LABEL \""),
        "{stable}"
    );
    assert!(
        stable_lines[1].starts_with("#define BUILD_HOST \""),
        "{stable}"
    );
    assert!(
        stable_lines[2].starts_with("#define BUILD_USER \""),
        "{stable}"
    );

    // A --workspace_status_command script's keys land in both headers, and
    // SOURCE_DATE_EPOCH pins the timestamp (byte-identical rebuilds).
    let script = ws.0.path().join("status.sh");
    fs::write(
        &script,
        "#!/bin/sh\necho 'STABLE_COMMIT deadbeef'\necho 'BUILD_USER stampuser'\necho 'BUILD_SCM_REVISION deadbeef'\necho 'BUILD_SCM_STATUS clean'\n",
    )
    .unwrap();
    let mut permissions = fs::metadata(&script).unwrap().permissions();
    use std::os::unix::fs::PermissionsExt;
    permissions.set_mode(0o755);
    fs::set_permissions(&script, permissions).unwrap();
    let flag = format!("--workspace_status_command={}", script.display());
    let built = outputs(&ws.build(&[&flag, "--embed_label=n63", "//bi:stamped"]));
    let volatile = built
        .iter()
        .find(|(_, path)| path.ends_with("volatile_file.h"))
        .map(|(_, path)| fs::read_to_string(path).unwrap())
        .unwrap();
    let stable = built
        .iter()
        .find(|(_, path)| path.ends_with("non_volatile_file.h"))
        .map(|(_, path)| fs::read_to_string(path).unwrap())
        .unwrap();
    let volatile_lines: Vec<&str> = volatile.lines().collect();
    assert_eq!(
        &volatile_lines[..2],
        &[
            "#define BUILD_SCM_REVISION \"deadbeef\"",
            "#define BUILD_SCM_STATUS \"clean\""
        ],
        "{volatile}"
    );
    assert!(
        volatile_lines[2]
            .trim_start_matches("#define BUILD_TIMESTAMP ")
            .parse::<i64>()
            .is_ok(),
        "{volatile}"
    );
    assert_eq!(
        stable.lines().next(),
        Some("#define BUILD_EMBED_LABEL \"n63\""),
        "{stable}"
    );
    assert!(
        stable.ends_with("#define BUILD_USER \"stampuser\"\n"),
        "{stable}"
    );

    // A failing script is an infrastructure failure with Bazel's text.
    let output = ws.build(&["--workspace_status_command=true; exit 9", "//bi:stamped"]);
    let text = stderr(&output);
    assert!(!output.status.success(), "{text}");
    assert!(
        text.contains("Failed to determine workspace status: Process exited with status 9"),
        "{text}"
    );

    // N53: the genrule `toolchains` attribute expands the cc_toolchain_alias
    // TemplateVariableInfo Make variables; the output equals the genuine
    // Bazel 9.3 reference row gen/toolchains_attribute.
    let built = outputs(&ws.build(&["//gen:vars"]));
    assert_eq!(
        output_of(&built, "//gen:vars"),
        "/usr/bin/fixture-gcc|/usr/bin/fixture-ar|tc|fixture-value|\n"
    );
}

/// F2: rules_cc's genuine cc_configure on this host. The values depend on the
/// host compiler, so only host-independent facts are asserted (the paired
/// capture compares the whole documents on the capture machine).
#[test]
fn host_local_config_cc_toolchain_analyzes_in_target_and_exec() {
    if !Path::new("/usr/bin/gcc").exists() && !Path::new("/usr/bin/cc").exists() {
        eprintln!("skipped: no host C compiler for cc_configure");
        return;
    }
    let ws = Workspace::new(false);
    let built = outputs(&ws.build(&[
        "//probe:host",
        "//probe:host_exec",
        "@@rules_cc++cc_configure_extension+local_config_cc//:local",
        "@@rules_cc++cc_configure_extension+local_config_cc//:cc-compiler-armeabi-v7a",
        "@@bazel_tools+xcode_configure_extension+local_config_xcode//:host_xcodes",
    ]));
    for (label, tool) in [("//probe:host", false), ("//probe:host_exec", true)] {
        let doc = json(&output_of(&built, label));
        let fields = &doc["fields"];
        assert_eq!(fields["toolchain_id"], "local", "{label}");
        assert_eq!(fields["cpu"], "k8", "{label}");
        assert_eq!(
            fields["_toolchain_label"],
            "Label:@@rules_cc++cc_configure_extension+local_config_cc//:cc-compiler-k8"
        );
        assert_eq!(
            fields["_crosstool_top_path"],
            "external/rules_cc++cc_configure_extension+local_config_cc"
        );
        assert_eq!(doc["configuration"]["is_tool_configuration"], tool);
        assert_eq!(fields["_is_tool_configuration"], tool);
        assert_eq!(doc["var"]["CC"], fields["compiler_executable"]);
        let compile = &doc["actions"]["c-compile"];
        assert_eq!(compile["enabled"], true);
        assert_eq!(compile["tool"], fields["compiler_executable"]);
        let line = compile["command_line"].as_array().unwrap();
        assert!(line.iter().any(|flag| flag == "-c"), "{line:?}");
        let opt = line.iter().any(|flag| flag == "-O2");
        assert_eq!(opt, tool, "{label}: {line:?}");
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
