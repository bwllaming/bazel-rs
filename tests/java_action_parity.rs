//! `aquery` of the native Java actions against genuine Bazel 9.3.1
//! (tests/fixtures/java_action_parity, reference_receipt.json): Javac with
//! deps/exports/resources/annotation processors, Turbine and the
//! annotation-processing JavacTurbine header compilation, GenClass,
//! JavaResourceJar, JavaSourceJar and JavaIjar, with their descriptions,
//! command lines, inputs, environment, execution info and ActionKeys.
//! The genuine capture used the real BCR rules_java 9.1.0; this test the
//! same rules_java Starlark offline (tests/support/java_scaffold.rs) with the
//! fixture's main-repository java_toolchain of fail-if-executed tools.
//! Normalized: the configuration checksum (native ConfigIds), block order.
#![allow(clippy::disallowed_methods)] // test harness: starts processes directly (clippy.toml)
include!("support/temp_root.rs");
include!("support/module_archives.rs");
include!("support/java_scaffold.rs");

use std::fs;
use std::path::Path;
use std::process::Command;

fn fixture() -> std::path::PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/java_action_parity")
}

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

/// The text output's action blocks with the configuration checksum
/// replaced, sorted.
fn blocks(text: &str) -> Vec<String> {
    let mut blocks: Vec<String> = text
        .split("\n\n")
        .filter(|block| !block.trim().is_empty())
        .map(|block| {
            block
                .lines()
                .map(|line| {
                    if line.starts_with("# Configuration: ") {
                        "# Configuration: <checksum>"
                    } else {
                        line
                    }
                })
                .collect::<Vec<_>>()
                .join("\n")
        })
        .collect();
    blocks.sort();
    blocks
}

struct Workspace(tempfile::TempDir);

impl Workspace {
    fn new() -> Self {
        let ws = Self(tempfile::tempdir_in(crate::test_temp::root()).unwrap());
        copy(&fixture().join("ws"), ws.0.path());
        let module = "module(name='java_action_parity')\n".to_owned() + &java_scaffold(ws.0.path());
        fs::write(ws.0.path().join("MODULE.bazel"), module).unwrap();
        for dir in ["home", "tmp"] {
            fs::create_dir_all(ws.0.path().join(dir)).unwrap();
        }
        ws
    }

    fn aquery(&self, args: &[&str]) -> String {
        let output = Command::new(env!("CARGO_BIN_EXE_bazel-rs"))
            .current_dir(self.0.path())
            .env_clear()
            .env("PATH", "/usr/bin:/bin")
            .env("HOME", self.0.path().join("home"))
            .env("TMPDIR", self.0.path().join("tmp"))
            .args([
                "--ignore_all_rc_files",
                "--cache-dir=cache",
                "aquery",
                "--lockfile_mode=update",
                "--repository_disable_download",
                "--extra_toolchains=//:registered",
                "--memory-limit=2048",
            ])
            .args(args)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout).unwrap()
    }
}

fn assert_parity(native: &str, genuine: &str) {
    let (native, genuine) = (blocks(native), blocks(genuine));
    let mnemonic = |block: &String| {
        block
            .lines()
            .find_map(|line| line.strip_prefix("  Mnemonic: "))
            .unwrap_or_default()
            .to_owned()
    };
    assert_eq!(
        native.iter().map(mnemonic).collect::<Vec<_>>(),
        genuine.iter().map(mnemonic).collect::<Vec<_>>()
    );
    for (native, genuine) in native.iter().zip(&genuine) {
        assert_eq!(native, genuine);
    }
}

#[test]
fn java_actions_match_genuine_aquery_including_action_keys() {
    let ws = Workspace::new();
    let genuine = |name: &str| fs::read_to_string(fixture().join("genuine").join(name)).unwrap();
    assert_parity(&ws.aquery(&["//pkg:all"]), &genuine("all.txt"));
}

/// `--strict_java_deps` (its old name; JavaOptions.strictJavaDeps) in every
/// StrictDepsMode: rules_java's `ctx.fragments.java.strict_java_deps`
/// selects JavaBuilder's `--strict_java_deps`/`--direct_dependencies` and
/// the header compilation's direct classpath; genuine captures
/// genuine/strict_MODE.txt (reference_receipt.json).
#[test]
fn strict_java_deps_modes_match_genuine_aquery() {
    let ws = Workspace::new();
    let genuine = |name: &str| fs::read_to_string(fixture().join("genuine").join(name)).unwrap();
    for (mode, capture) in [
        ("off", "strict_off.txt"),
        ("warn", "strict_warn.txt"),
        ("error", "strict_error.txt"),
        ("strict", "strict_strict.txt"),
        // `default` spelled out is genuine's default run (all.txt).
        ("default", "all.txt"),
    ] {
        let flag = format!("--strict_java_deps={mode}");
        assert_parity(&ws.aquery(&["//pkg:all", &flag]), &genuine(capture));
    }
    let flag = "--experimental_strict_java_deps=OFF";
    assert_parity(&ws.aquery(&["//pkg:all", flag]), &genuine("strict_off.txt"));
}

/// The argv of a Javac block of genuine's aquery text: the `Command Line:`
/// lines, shell quoting undone.
fn genuine_argv(text: &str, description: &str) -> Vec<String> {
    let block = text
        .split("\n\n")
        .find(|block| block.starts_with(&format!("action '{description}")))
        .unwrap_or_else(|| panic!("no block {description}"));
    let mut argv = Vec::new();
    let mut inside = false;
    for line in block.lines() {
        let line = if let Some(rest) = line.strip_prefix("  Command Line: (exec ") {
            inside = true;
            rest
        } else if inside {
            line.trim_start()
        } else {
            continue;
        };
        let (word, done) = match line.strip_suffix(" \\") {
            Some(word) => (word, false),
            None => (line.strip_suffix(')').unwrap(), true),
        };
        argv.push(
            match word.strip_prefix('\'').and_then(|w| w.strip_suffix('\'')) {
                Some(inner) => inner.replace("'\\''", "'"),
                None => word.to_owned(),
            },
        );
        if done {
            break;
        }
    }
    argv
}

/// `Target.actions` argv of a Java compilation is JavaCompileAction
/// .getArguments (:697-700), the whole command line genuine's aquery prints
/// (flags and classpath, no parameter file; B46, docs/ANALYSIS_TESTS.md).
#[test]
fn target_actions_argv_of_javac_is_the_aquery_command_line() {
    let ws = Workspace::new();
    fs::write(
        ws.0.path().join("pkg/argv.bzl"),
        r#"
def _impl(ctx):
    lines = [json.encode(a.argv) for a in ctx.attr.target.actions if a.mnemonic == "Javac"]
    return [AnalysisTestResultInfo(success = True, message = "\n".join(lines))]

def check(name, target):
    testing.analysis_test(
        name = name,
        implementation = _impl,
        attrs = {"target": attr.label()},
        attr_values = {"target": target},
    )
"#,
    )
    .unwrap();
    let build = ws.0.path().join("pkg/BUILD.bazel");
    let mut text = fs::read_to_string(&build).unwrap();
    text.insert_str(0, "load(':argv.bzl', 'check')\n");
    text.push_str("check(name = 'c_test', target = ':c')\ncheck(name = 'b_test', target = ':b')\n");
    fs::write(&build, text).unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_bazel-rs"))
        .current_dir(ws.0.path())
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .env("HOME", ws.0.path().join("home"))
        .env("TMPDIR", ws.0.path().join("tmp"))
        .env("BAZEL_RS_RESULT_LINES", "1")
        .args([
            "--ignore_all_rc_files",
            "--cache-dir=cache",
            "build",
            "--lockfile_mode=update",
            "--repository_disable_download",
            "--extra_toolchains=//:registered",
            "--memory-limit=2048",
            "//pkg:c_test",
            "//pkg:b_test",
        ])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let genuine = fs::read_to_string(fixture().join("genuine/all.txt")).unwrap();
    for (test, description) in [
        ("c_test", "Building pkg/libc.jar"),
        ("b_test", "Building pkg/libb.jar"),
    ] {
        // `label\tpath` result lines.
        let stdout = String::from_utf8_lossy(&output.stdout);
        let path = stdout
            .lines()
            .filter_map(|line| line.split_once('\t'))
            .find(|(_, path)| path.ends_with(&format!("/{test}.sh")))
            .map(|(_, path)| ws.0.path().join(path))
            .unwrap_or_else(|| panic!("no {test}.sh in {stdout}"));
        let script = fs::read_to_string(path).unwrap();
        // `echo <message with every character backslash-escaped>`.
        let line = script
            .lines()
            .find_map(|line| line.strip_prefix("echo "))
            .unwrap();
        let mut message = String::new();
        let mut chars = line.chars();
        while let Some(c) = chars.next() {
            message.push(if c == '\\' { chars.next().unwrap() } else { c });
        }
        let native: Vec<String> = serde_json::from_str(&message).unwrap();
        assert_eq!(native, genuine_argv(&genuine, description), "{test}");
    }
}
