//! `bazel coverage` (docs/COVERAGE.md) against the paired genuine Bazel 9.3.0
//! capture of tests/fixtures/coverage_command: the instrumentation filter
//! default, the coverage test action's environment and manifest, the
//! per-test `coverage.dat` the native coverage output generator merges, the
//! baseline coverage file and the `--combined_report=lcov` reports.
//! Offline: the builtin profile with the analysis stubs of
//! tests/fixtures/test_flags/stubs; the rules_shell stub's sh_binary links
//! its source, so `@bazel_tools//tools/test:collect_coverage` is the genuine
//! collect_coverage.sh.
#![allow(clippy::disallowed_methods)] // test harness: starts processes directly (clippy.toml)
include!("support/module_archives.rs");

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
include!("support/explicit_modules.rs");

const STUBBED: &[&str] = &[
    "apple_support",
    "buildozer",
    "protobuf",
    "rules_cc",
    "rules_java",
    "rules_python",
    "zlib",
];

const SH_BINARY: &str = r#"def _impl(ctx):
    out = ctx.actions.declare_file(ctx.label.name)
    ctx.actions.symlink(output = out, target_file = ctx.files.srcs[0], is_executable = True)
    return [DefaultInfo(executable = out)]

sh_binary = rule(
    implementation = _impl,
    executable = True,
    attrs = {"srcs": attr.label_list(allow_files = True)},
)
"#;

struct Workspace(tempfile::TempDir);

impl Drop for Workspace {
    /// Test outputs are read-only; make the tree writable so it goes.
    fn drop(&mut self) {
        let _ = Command::new("chmod")
            .args(["-R", "u+w"])
            .arg(self.0.path())
            .status();
    }
}

fn manifest() -> &'static Path {
    Path::new(env!("CARGO_MANIFEST_DIR"))
}

fn copy_tree(from: &Path, to: &Path) {
    fs::create_dir_all(to).unwrap();
    for entry in fs::read_dir(from).unwrap() {
        let path = entry.unwrap().path();
        let target = to.join(path.file_name().unwrap());
        if path.is_dir() {
            copy_tree(&path, &target);
        } else {
            fs::copy(&path, &target).unwrap();
        }
    }
}

impl Workspace {
    fn new() -> Self {
        let ws = Self(
            tempfile::Builder::new()
                .prefix("coverage-")
                .tempdir_in(crate::test_temp::root())
                .unwrap(),
        );
        copy_tree(
            &manifest().join("tests/fixtures/coverage_command/workspace"),
            ws.path(),
        );
        let stubs = manifest().join("tests/fixtures/test_flags/stubs");
        let shell = ws.path().join("stub_rules_shell");
        copy_tree(&stubs.join("rules_shell"), &shell);
        fs::write(shell.join("shell/sh_binary.bzl"), SH_BINARY).unwrap();
        let mut module = fs::read_to_string(ws.path().join("MODULE.bazel")).unwrap();
        for name in STUBBED {
            module.push_str(&format!(
                "local_path_override(module_name = \"{name}\", path = \"{}\")\n",
                stubs.join(name).display()
            ));
        }
        module.push_str(&format!(
            "local_path_override(module_name = \"rules_shell\", path = \"{}\")\n",
            shell.display()
        ));
        fs::write(ws.path().join("MODULE.bazel"), module).unwrap();
        ws
    }

    fn path(&self) -> &Path {
        self.0.path()
    }

    fn run(&self, args: &[&str]) -> Output {
        // Offline: the toolchain resolution materializes @platforms.
        seed_module_archives(&self.path().join("private-cache"), &["platforms@1.0.0"]);
        let registry = format!(
            "--registry=file://{}",
            manifest()
                .join("tests/fixtures/builtin_modules/registry")
                .display()
        );
        let mut full = vec![args[0], "--experimental-builtin-modules", registry.as_str()];
        full.extend_from_slice(&args[1..]);
        let output = Command::new("timeout")
            .args(["--kill-after=5s", "600s", env!("CARGO_BIN_EXE_bazel-rs")])
            .args(["--ignore_all_rc_files", "--cache-dir=private-cache"])
            .args(&full)
            .current_dir(self.path())
            .env_clear()
            .env("PATH", "/usr/bin:/bin")
            .env("HOME", self.path())
            .env("TMPDIR", crate::test_temp::root())
            .output()
            .unwrap();
        assert_ne!(output.status.code(), Some(124), "CLI timed out: {args:?}");
        output
    }

    fn execution_root(&self) -> PathBuf {
        let output = self.run(&["info", "execution_root"]);
        assert!(output.status.success(), "{}", stderr(&output));
        PathBuf::from(String::from_utf8_lossy(&output.stdout).trim())
    }
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn genuine(name: &str) -> String {
    fs::read_to_string(
        manifest()
            .join("tests/fixtures/coverage_command/genuine")
            .join(name),
    )
    .unwrap()
}

fn read(path: &Path) -> String {
    fs::read_to_string(path).unwrap_or_else(|error| panic!("{}: {error}", path.display()))
}

/// Integration regression: coverage must look up the resolved configuration,
/// including when a top-level library changes runfiles policy on its incoming edge.
#[test]
fn coverage_keeps_top_level_baselines_after_incoming_runfiles_transition() {
    let ws = Workspace::new();
    let definitions = fs::read_to_string(ws.path().join("defs.bzl")).unwrap();
    let definitions = definitions
        .replace(
            "    implementation = _library,",
            "    implementation = _library,\n    cfg = no_links,",
        )
        .replace(
            "        \"srcs\": attr.label_list(allow_files = True),",
            "        \"srcs\": attr.label_list(allow_files = True),\n        \"_allowlist_function_transition\": attr.label(default = \"//tools/allowlists/function_transition_allowlist:function_transition_allowlist\"),",
        );
    fs::write(
        ws.path().join("defs.bzl"),
        format!(
            "def _no_links(settings, attr):\n    return {{\"//command_line_option:build_runfile_links\": False}}\nno_links = transition(implementation = _no_links, inputs = [], outputs = [\"//command_line_option:build_runfile_links\"])\n{definitions}"
        ),
    )
    .unwrap();
    let allowlist = ws
        .path()
        .join("tools/allowlists/function_transition_allowlist");
    fs::create_dir_all(&allowlist).unwrap();
    fs::write(
        allowlist.join("BUILD.bazel"),
        "package_group(name = 'function_transition_allowlist', packages = ['public'])\n",
    )
    .unwrap();
    let extra = ws.path().join("extra");
    fs::create_dir(&extra).unwrap();
    fs::write(extra.join("extra.txt"), "a\nb\n").unwrap();
    fs::write(
        extra.join("BUILD.bazel"),
        "load('//:defs.bzl', 'cov_library')\ncov_library(name = 'extra', srcs = ['extra.txt'])\n",
    )
    .unwrap();
    // extra is not a test dependency: its baseline must come from its own
    // post-transition top-level provider, not the untransitioned request key.
    let output = ws.run(&[
        "coverage",
        "--instrumentation_filter=//",
        "//lib/tests:t_test",
        "//extra:extra",
    ]);
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    let baseline = read(
        &ws.execution_root()
            .join("bazel-out/_coverage/_baseline_report.dat"),
    );
    assert!(baseline.contains("SF:extra/extra.txt\n"), "{baseline}");
}

/// `coverage //...` and an explicit filter without the combined report,
/// byte for byte against the paired capture.
#[test]
fn coverage_follows_bazel() {
    let ws = Workspace::new();
    let output = ws.run(&["coverage", "--test_output=all", "//..."]);
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    let errors = stderr(&output);
    assert!(
        errors.contains(
            "INFO: Using default value for --instrumentation_filter: \"^//lib[/:]\".\nINFO: Override the above default with --instrumentation_filter\n"
        ),
        "{errors}"
    );
    assert!(
        stdout(&output).contains(&genuine("t_test.output.txt")),
        "{}",
        stdout(&output)
    );
    let root = ws.execution_root();
    let lcov = root.join("bazel-out/_coverage");
    assert!(
        errors.contains(&format!(
            "INFO: LCOV coverage report is located at {}\n and execpath is bazel-out/_coverage/_baseline_report.dat\nINFO: LCOV coverage report is located at {}\n and execpath is bazel-out/_coverage/_coverage_report.dat\n",
            lcov.join("_baseline_report.dat").display(),
            lcov.join("_coverage_report.dat").display()
        )),
        "{errors}"
    );
    let testlogs = root.join("bazel-out/k8-fastbuild/testlogs");
    for (path, expected) in [
        (lcov.join("_baseline_report.dat"), "_baseline_report.dat"),
        (lcov.join("_coverage_report.dat"), "_coverage_report.dat"),
        (
            lcov.join("baseline_lcov_files.tmp"),
            "baseline_lcov_files.tmp",
        ),
        (
            testlogs.join("lib/tests/t_test/coverage.dat"),
            "t_test.coverage.dat",
        ),
        (
            testlogs.join("lib/tests/plain_test/coverage.dat"),
            "plain_test.coverage.dat",
        ),
        (
            testlogs.join("lib/lib/baseline_coverage.dat"),
            "lib.baseline_coverage.dat",
        ),
    ] {
        assert_eq!(read(&path), genuine(expected), "{}", path.display());
    }
    // The tests' order in the list is not deterministic in Bazel either
    // (CoverageReportActionBuilder iterates a set of targets).
    let sorted = |text: String| {
        let mut lines: Vec<String> = text.lines().map(str::to_owned).collect();
        lines.sort();
        lines
    };
    assert_eq!(
        sorted(read(&lcov.join("coverage_lcov_files.tmp"))),
        sorted(genuine("coverage_lcov_files.tmp"))
    );

    let output = ws.run(&[
        "coverage",
        "--test_output=all",
        "--combined_report=none",
        "--instrumentation_filter=//other",
        "//lib/tests:t_test",
    ]);
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    assert!(
        !stderr(&output).contains("--instrumentation_filter: "),
        "{}",
        stderr(&output)
    );
    assert!(
        !stderr(&output).contains("LCOV coverage report"),
        "{}",
        stderr(&output)
    );
    assert!(
        stdout(&output).contains("manifest: other/o.txt \n"),
        "{}",
        stdout(&output)
    );
    assert_eq!(
        read(&testlogs.join("lib/tests/t_test/coverage.dat")),
        genuine("t_test.other_filter.coverage.dat")
    );

    // A coverage generator the native profile does not run fails the command.
    let output = ws.run(&[
        "coverage",
        "--coverage_report_generator=//lib:lib",
        "//lib/tests:t_test",
    ]);
    assert_ne!(output.status.code(), Some(0));
    assert!(
        stderr(&output).contains(
            "--coverage_report_generator=//lib:lib is not implemented in the native profile"
        ),
        "{}",
        stderr(&output)
    );
}
