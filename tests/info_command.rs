//! `bazel-rs info` (runtime/commands/InfoCommand.java; docs/INFO.md) against
//! genuine Bazel 9.3.1 captures (tests/fixtures/info/genuine_capture.json,
//! written by tests/fixtures/info/capture.py): item order, key set, value
//! shapes, single-key and multi-key output, unknown keys, `--show_make_env`.
//! Genuine `<output_base>` maps to the workspace's state directory in the
//! native cache, `<cache>/worktrees/<name>-<hash>` (docs/WORKTREES.md): the
//! paths below it (`execroot/_main/bazel-out/<cfg>/bin`, `command.log`) are
//! the same, so they compare exactly after substitution. And the output tree
//! holds what a native build built.
#![allow(clippy::disallowed_methods)] // test harness: starts processes directly (clippy.toml)

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

fn manifest() -> &'static Path {
    Path::new(env!("CARGO_MANIFEST_DIR"))
}

fn capture() -> serde_json::Value {
    let text = fs::read_to_string(manifest().join("tests/fixtures/info/genuine_capture.json"))
        .expect("genuine capture");
    serde_json::from_str(&text).expect("capture JSON")
}

struct Workspace {
    _temp: tempfile::TempDir,
    root: PathBuf,
    cache: PathBuf,
    home: PathBuf,
}

impl Workspace {
    /// The capture's workspace: `module(name="scratch")` and a genrule.
    fn new() -> Self {
        let base = std::env::var_os("TMPDIR")
            .map(PathBuf::from)
            .unwrap_or_else(std::env::temp_dir);
        let temp = tempfile::Builder::new()
            .prefix("info-")
            .tempdir_in(base)
            .unwrap();
        let top = fs::canonicalize(temp.path()).unwrap();
        let root = top.join("ws");
        fs::create_dir_all(root.join("pkg")).unwrap();
        fs::write(root.join("MODULE.bazel"), "module(name=\"scratch\")\n").unwrap();
        fs::write(
            root.join("pkg/BUILD"),
            "genrule(name=\"g\", outs=[\"g.txt\"], cmd=\"echo hi > $@\")\n",
        )
        .unwrap();
        let home = top.join("home");
        fs::create_dir_all(&home).unwrap();
        Self {
            _temp: temp,
            cache: top.join("cache"),
            root,
            home,
        }
    }

    fn run(&self, arguments: &[&str]) -> Output {
        self.run_startup(&["--ignore_all_rc_files"], arguments)
    }

    fn run_startup(&self, startup: &[&str], arguments: &[&str]) -> Output {
        let output = Command::new("timeout")
            .args(["--kill-after=5s", "600s", env!("CARGO_BIN_EXE_bazel-rs")])
            .args(startup)
            .arg(format!("--cache-dir={}", self.cache.display()))
            .args(arguments)
            .env_clear()
            .env("PATH", "/usr/bin:/bin")
            .env("HOME", &self.home)
            .env("BAZEL_RS_MAX_MEMORY_MB", "4096")
            .current_dir(&self.root)
            .output()
            .unwrap();
        assert_ne!(output.status.code(), Some(124), "CLI timed out");
        output
    }

    fn info(&self, arguments: &[&str]) -> Output {
        let mut all = vec!["info"];
        all.extend_from_slice(arguments);
        self.run(&all)
    }

    /// The output base: the workspace's state directory in the cache.
    fn output_base(&self) -> PathBuf {
        self.cache
            .join("worktrees")
            .join(bazel_rs::worktree::state_name(&self.root))
    }

    /// A genuine value with the genuine layout mapped to this workspace.
    fn expected(&self, text: &str) -> String {
        text.replace("<OUTPUT_BASE>", &self.output_base().display().to_string())
            .replace("<WORKSPACE>", &self.root.display().to_string())
            .replace("<HOME>", &self.home.display().to_string())
    }
}

fn stdout(output: &Output) -> String {
    String::from_utf8(output.stdout.clone()).unwrap()
}

fn stderr(output: &Output) -> String {
    String::from_utf8(output.stderr.clone()).unwrap()
}

fn case<'a>(capture: &'a serde_json::Value, name: &str) -> &'a serde_json::Value {
    &capture["cases"][name]
}

fn args(case: &serde_json::Value) -> Vec<&str> {
    case["args"]
        .as_array()
        .unwrap()
        .iter()
        .map(|arg| arg.as_str().unwrap())
        .collect()
}

/// `key: value` lines in order.
fn entries(text: &str) -> Vec<(String, String)> {
    text.lines()
        .map(|line| {
            let (key, value) = line.split_once(": ").unwrap_or_else(|| {
                line.strip_suffix(':')
                    .map(|key| (key, ""))
                    .unwrap_or_else(|| panic!("not a key: value line: {line:?}"))
            });
            (key.to_owned(), value.to_owned())
        })
        .collect()
}

/// The genuine items whose values are native paths or texts of their own
/// (docs/INFO.md "Item mapping") and the shape both share.
fn shape(key: &str, value: &str) -> &'static str {
    let digits = |text: &str| !text.is_empty() && text.bytes().all(|byte| byte.is_ascii_digit());
    let size = |text: &str| {
        ["GB", "MB", "KB", "B"]
            .iter()
            .any(|unit| text.strip_suffix(unit).is_some_and(digits))
    };
    match key {
        "committed-heap-size" | "max-heap-size" | "used-heap-size" if size(value) => "size",
        "gc-count" | "server_pid" if digits(value) => "count",
        "gc-time" if value.strip_suffix("ms").is_some_and(digits) => "millis",
        "local_resources"
            if value.starts_with("RAM=") && value.contains("MB, CPU=") && value.contains('.') =>
        {
            "resources"
        }
        // Genuine's values start with a placeholder of an absolute path.
        "install_base" | "repository_cache" if value.starts_with('/') || value.starts_with('<') => {
            "absolute path"
        }
        "character-encoding"
            if value.starts_with("file.encoding = ")
                && value.contains(", defaultCharset = ")
                && value.contains(", sun.jnu.encoding = ") =>
        {
            "encoding"
        }
        "release" if value.starts_with("release ") => "release",
        "java-home" | "java-runtime" | "java-vm" | "server_log" => "free text",
        _ => "mismatch",
    }
}

#[test]
fn all_items_in_genuine_order_with_mapped_values() {
    let capture = capture();
    let ws = Workspace::new();
    let genuine = case(&capture, "all");
    let output = ws.info(&[]);
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    let native = entries(&stdout(&output));
    let genuine = entries(genuine["stdout"].as_str().unwrap());
    let keys = |entries: &[(String, String)]| -> Vec<String> {
        entries.iter().map(|(key, _)| key.clone()).collect()
    };
    assert_eq!(keys(&native), keys(&genuine));
    // Paths below the output base, the workspace and the fixed texts are
    // genuine's exactly; the rest share genuine's shape.
    for ((key, native), (_, genuine)) in native.iter().zip(&genuine) {
        let expected = ws.expected(genuine);
        if !expected.contains("<OUTPUT_USER_ROOT>") && shape(key, genuine) == "mismatch" {
            assert_eq!(native, &expected, "{key}");
        } else {
            assert_eq!(shape(key, native), shape(key, genuine), "{key}: {native:?}");
        }
    }
    let native: std::collections::BTreeMap<_, _> = native.into_iter().collect();
    assert_eq!(
        native["repository_cache"],
        ws.cache
            .join("repositories/downloads")
            .display()
            .to_string()
    );
    assert_eq!(
        native["release"],
        format!("release 9.3.1+bazel-rs.{}", env!("CARGO_PKG_VERSION"))
    );
    assert_eq!(native["server_log"], "");
    // info neither creates the cache nor writes a lockfile.
    assert!(!ws.cache.exists());
    assert!(!ws.root.join("MODULE.bazel.lock").exists());
}

#[test]
fn single_and_several_keys_match_genuine() {
    let capture = capture();
    let ws = Workspace::new();
    for name in [
        "single",
        "single_opt",
        "multi_dbg",
        "cpu",
        "repeated",
        "show_make_env_keys",
        "starlark_semantics",
        "client_env",
    ] {
        let genuine = case(&capture, name);
        let output = ws.info(&args(genuine));
        assert_eq!(output.status.code(), Some(0), "{name}: {}", stderr(&output));
        assert_eq!(
            stdout(&output),
            ws.expected(genuine["stdout"].as_str().unwrap()),
            "{name}"
        );
    }
}

#[test]
fn hidden_items_match_genuine() {
    let capture = capture();
    let ws = Workspace::new();
    let genuine = case(&capture, "hidden");
    let output = ws.info(&args(genuine));
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    let native = entries(&stdout(&output));
    let genuine = entries(genuine["stdout"].as_str().unwrap());
    assert_eq!(native.len(), genuine.len());
    for ((key, native), (genuine_key, genuine)) in native.iter().zip(&genuine) {
        assert_eq!(key, genuine_key);
        if key == "install_md5" {
            assert_eq!(native.len(), 32, "{native}");
            assert!(native.bytes().all(|byte| byte.is_ascii_hexdigit()));
        } else {
            assert_eq!(native, genuine, "{key}");
        }
    }
}

#[test]
fn unknown_keys_fail_like_genuine() {
    let capture = capture();
    let ws = Workspace::new();
    for name in ["unknown", "make_env_unknown"] {
        let genuine = case(&capture, name);
        let output = ws.info(&args(genuine));
        assert_eq!(
            output.status.code(),
            genuine["exit"].as_i64().map(|code| code as i32),
            "{name}"
        );
        assert_eq!(
            stdout(&output),
            ws.expected(genuine["stdout"].as_str().unwrap()),
            "{name}"
        );
        let errors: Vec<String> = stderr(&output)
            .lines()
            .filter(|line| line.starts_with("ERROR"))
            .map(str::to_owned)
            .collect();
        let expected: Vec<String> = genuine["stderr"]
            .as_array()
            .unwrap()
            .iter()
            .map(|line| line.as_str().unwrap().to_owned())
            .collect();
        assert_eq!(errors, expected, "{name}");
    }
}

#[test]
fn show_make_env_adds_the_make_variables_in_key_order() {
    let capture = capture();
    let ws = Workspace::new();
    let genuine = case(&capture, "show_make_env");
    let output = ws.info(&args(genuine));
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    let native = entries(&stdout(&output));
    let genuine = entries(genuine["stdout"].as_str().unwrap());
    let keys = |entries: &[(String, String)]| -> Vec<String> {
        entries.iter().map(|(key, _)| key.clone()).collect()
    };
    assert_eq!(keys(&native), keys(&genuine));
    for (key, native) in &native[..4] {
        let genuine = &genuine.iter().find(|(other, _)| other == key).unwrap().1;
        assert_eq!(native, genuine, "{key}");
    }
    // TARGET_CPU needs an explicit platform loaded: explicit failure.
    let output = ws.info(&["--show_make_env", "--platforms=//pkg:p"]);
    assert_eq!(output.status.code(), Some(1));
    assert!(
        stderr(&output).contains("not implemented"),
        "{}",
        stderr(&output)
    );
    // bazel-bin does not depend on the target platform.
    let output = ws.info(&["--platforms=//pkg:p", "bazel-bin"]);
    assert_eq!(
        stdout(&output),
        format!(
            "{}/execroot/_main/bazel-out/k8-fastbuild/bin\n",
            ws.output_base().display()
        )
    );
}

#[test]
fn info_inherits_rc_build_options() {
    let ws = Workspace::new();
    fs::write(
        ws.root.join(".bazelrc"),
        "build -c opt\ninfo --define=X=1\n",
    )
    .unwrap();
    let output = ws.run_startup(
        &["--nosystem_rc", "--nohome_rc"],
        &["info", "--show_make_env", "bazel-bin", "X"],
    );
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    assert_eq!(
        stdout(&output),
        format!(
            "bazel-bin: {}/execroot/_main/bazel-out/k8-opt/bin\nX: 1\n",
            ws.output_base().display()
        )
    );
}

#[test]
fn bazel_bin_holds_the_built_outputs() {
    let ws = Workspace::new();
    for mode in ["fastbuild", "opt"] {
        let flag = format!("--compilation_mode={mode}");
        let built = ws.run(&["build", &flag, "//pkg:g"]);
        assert_eq!(built.status.code(), Some(0), "{}", stderr(&built));
        let output = ws.info(&[&flag, "bazel-bin"]);
        let bin = PathBuf::from(stdout(&output).trim_end());
        assert_eq!(fs::read_to_string(bin.join("pkg/g.txt")).unwrap(), "hi\n");
        // A cache hit links the same output again.
        let rebuilt = ws.run(&["build", &flag, "//pkg:g"]);
        assert!(
            stderr(&rebuilt).contains("INFO: 1 process: 1 action cache hit, 1 internal."),
            "{}",
            stderr(&rebuilt)
        );
        assert_eq!(fs::read_to_string(bin.join("pkg/g.txt")).unwrap(), "hi\n");
        let root = ws.info(&["execution_root"]);
        let root = PathBuf::from(stdout(&root).trim_end());
        assert_eq!(
            fs::read_to_string(root.join(format!("bazel-out/k8-{mode}/bin/pkg/g.txt"))).unwrap(),
            "hi\n"
        );
    }
}

#[test]
fn build_language_fails_explicitly() {
    let ws = Workspace::new();
    let output = ws.info(&["build-language"]);
    assert_eq!(output.status.code(), Some(1));
    assert!(stderr(&output).contains("build-language is not implemented"));
}
