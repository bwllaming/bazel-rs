//! CLI: `apple_common.dotted_version` and the `@_builtins` `XcodeVersionInfo`
//! provider (`apple_common.XcodeVersionConfig`) through native analysis, on the
//! F3 fixture workspace (tests/fixtures/xcode_dotted/workspace). The expected
//! outputs are the bytes genuine Bazel 9.3 writes for the same targets.
#![allow(clippy::disallowed_methods)] // test harness: starts processes directly (clippy.toml)

include!("support/executable_script.rs");
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

const FIXTURE: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/xcode_dotted");

struct Workspace(tempfile::TempDir);

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

impl Workspace {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        copy_tree(&Path::new(FIXTURE).join("workspace"), dir.path());
        for sub in ["home", "tmp"] {
            fs::create_dir_all(dir.path().join(sub)).unwrap();
        }
        Workspace(dir)
    }

    fn build(&self, targets: &[&str]) -> Output {
        let binary = PathBuf::from(env!("CARGO_BIN_EXE_bazel-rs"));
        let output = Command::new("/usr/bin/timeout")
            .args(["--kill-after=5s", "60s"])
            .arg(binary)
            .args(["--ignore_all_rc_files", "--cache-dir=cache", "build"])
            .args(targets)
            .current_dir(self.0.path())
            .env_clear()
            .env("PATH", "/usr/bin:/bin")
            .env("HOME", self.0.path().join("home"))
            .env("TMPDIR", self.0.path().join("tmp"))
            .env("LC_ALL", "C")
            .output()
            .unwrap();
        assert!(
            !matches!(output.status.code(), None | Some(124 | 137)),
            "{}",
            stderr(&output)
        );
        assert!(!stderr(&output).contains("panicked"), "{}", stderr(&output));
        output
    }
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

/// `LABEL\tPATH` lines printed by a successful native build.
fn output_of(output: &Output, label: &str) -> String {
    let stdout = String::from_utf8(output.stdout.clone()).unwrap();
    let path = stdout
        .lines()
        .find_map(|line| line.strip_prefix(&format!("{label}\t")))
        .unwrap_or_else(|| panic!("no output for {label}: {stdout}"));
    fs::read_to_string(path).unwrap()
}

#[test]
fn dotted_version_matrix_and_host_xcodes_provider_dump() {
    let workspace = Workspace::new();
    let output = workspace.build(&["//:dump", "//:consume"]);
    assert!(output.status.success(), "{}", stderr(&output));
    let expected = |name| fs::read_to_string(Path::new(FIXTURE).join(name)).unwrap();
    assert_eq!(output_of(&output, "//:dump"), expected("expected_dump.txt"));
    // The returned XcodeVersionInfo crosses targets as a provider key.
    assert_eq!(
        output_of(&output, "//:consume"),
        expected("expected_consume.txt")
    );
}

/// providers.bzl's closures capture `availability`/`xcode_version` raw, out of
/// sight of the returned-provider file walk; natively a File there fails
/// explicitly at construction instead of being silently dropped.
#[test]
fn files_cannot_hide_in_xcode_version_info_closures() {
    let workspace = Workspace::new();
    let root = workspace.0.path();
    fs::create_dir_all(root.join("hide")).unwrap();
    fs::write(root.join("hide/src.txt"), "payload\n").unwrap();
    fs::write(
        root.join("hide/defs.bzl"),
        "load('//:defs.bzl', 'info_lines')\n\
def _impl(ctx):\n    \
    f = ctx.file.src\n    \
    info = apple_common.XcodeVersionConfig(\n        \
        ios_sdk_version = '8.4', ios_minimum_os_version = '8.4',\n        \
        visionos_sdk_version = '1.0', visionos_minimum_os_version = '1.0',\n        \
        watchos_sdk_version = '2.0', watchos_minimum_os_version = '2.0',\n        \
        tvos_sdk_version = '9.0', tvos_minimum_os_version = '9.0',\n        \
        macos_sdk_version = '10.11', macos_minimum_os_version = '10.11',\n        \
        xcode_version = None, availability = f,\n        \
        xcode_version_flag = None, include_xcode_execution_info = False)\n    \
    return [info]\n\
hide = rule(implementation = _impl, attrs = {'src': attr.label(allow_single_file = True)})\n",
    )
    .unwrap();
    fs::write(
        root.join("hide/BUILD"),
        "load(':defs.bzl', 'hide')\nhide(name = 'hide', src = 'src.txt')\n",
    )
    .unwrap();
    let output = workspace.build(&["//hide:hide"]);
    assert_eq!(output.status.code(), Some(1), "{}", stderr(&output));
    assert!(
        stderr(&output).contains(
            "XcodeVersionInfo argument 'availability' of type 'File' is not supported in the native profile"
        ),
        "{}",
        stderr(&output)
    );
}

#[test]
fn dotted_version_and_provider_errors_fail_analysis() {
    let workspace = Workspace::new();
    for (target, message) in [
        (
            "none",
            "in call to dotted_version(), parameter 'version' got value of type 'NoneType', want 'string'",
        ),
        (
            "double_dot",
            "Dotted version components must all start with the form \\d+([a-z0-9]*?)?(\\d+)? but got '1..2'",
        ),
        (
            "overflow",
            "Dotted version components must all start with the form \\d+([a-z0-9]*?)?(\\d+)? but got '2147483648'",
        ),
        (
            "missing",
            "lambda() missing 1 required positional argument: version",
        ),
        (
            "compare_to_string",
            "in call to compare_to(), parameter 'other' got value of type 'string', want 'DottedVersion'",
        ),
        (
            "less_than_int",
            "unsupported comparison: DottedVersion <=> int",
        ),
        // The four interpreter-wording MISMATCH rows of the paired capture
        // (reference_receipt.json), exact since N33: genuine Bazel's messages.
        (
            "int_less_than",
            "unsupported comparison: int <=> DottedVersion",
        ),
        (
            "no_field",
            "'DottedVersion' value has no field or method 'nope'",
        ),
        (
            "info_no_args",
            "_xcode_version_info_init() missing 14 required keyword-only arguments: ios_sdk_version, ios_minimum_os_version, visionos_sdk_version, visionos_minimum_os_version, watchos_sdk_version, watchos_minimum_os_version, tvos_sdk_version, tvos_minimum_os_version, macos_sdk_version, macos_minimum_os_version, xcode_version, availability, xcode_version_flag, include_xcode_execution_info",
        ),
        (
            "info_positional",
            "_xcode_version_info_init() does not accept positional arguments, but got 1",
        ),
        ("info_bad_sdk", "but got '8..4'"),
        ("info_platform_type", "Unhandled platform type: beos"),
        ("info_platform", "Unhandled platform: struct(name = \"x\")"),
    ] {
        let output = workspace.build(&[&format!("//err:{target}")]);
        assert_eq!(
            output.status.code(),
            Some(1),
            "{target}: {}",
            stderr(&output)
        );
        assert!(
            stderr(&output).contains(message),
            "{target}: want {message:?} in {}",
            stderr(&output)
        );
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
