//! Event locations of files in external repositories like Bazel 9.3.1
//! (crate::debug_print::external_path; genuine capture in
//! tests/fixtures/external_locations): `print()` DEBUG lines, WARNING
//! events at a target's location and `query --output=location` name a
//! file of another repository as `<output_base>/external/<canonical
//! name>/<path>` — never the directory bazel-rs read it from (its
//! repository store, a `local_path_override`'s path) — and a main
//! repository file by its workspace path.
#![allow(clippy::disallowed_methods)] // test harness: starts processes directly (clippy.toml)

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

const FIXTURE: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/tests/fixtures/external_locations"
);

fn copy(from: &Path, to: &Path) {
    fs::create_dir_all(to).unwrap();
    for entry in fs::read_dir(from).unwrap() {
        let entry = entry.unwrap();
        let target = to.join(entry.file_name());
        if entry.file_type().unwrap().is_dir() {
            copy(&entry.path(), &target);
        } else {
            fs::copy(entry.path(), target).unwrap();
        }
    }
}

struct Workspace {
    _temp: tempfile::TempDir,
    ws: PathBuf,
    cache: PathBuf,
}

impl Workspace {
    fn new() -> Self {
        let base = std::env::var_os("TMPDIR")
            .map(PathBuf::from)
            .unwrap_or_else(std::env::temp_dir);
        let temp = tempfile::Builder::new()
            .prefix("external-locations-")
            .tempdir_in(base)
            .unwrap();
        let root = fs::canonicalize(temp.path()).unwrap();
        let ws = root.join("ws");
        copy(&Path::new(FIXTURE).join("workspace"), &ws);
        Self {
            _temp: temp,
            ws,
            cache: root.join("cache"),
        }
    }

    fn run(&self, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_bazel-rs"))
            .arg("--ignore_all_rc_files")
            .arg(format!("--cache-dir={}", self.cache.display()))
            .args(args)
            .current_dir(&self.ws)
            .env("BAZEL_RS_MAX_MEMORY_MB", "4096")
            .stdin(Stdio::null())
            .output()
            .unwrap()
    }

    fn output_base(&self) -> String {
        let output = self.run(&["info", "output_base"]);
        assert!(output.status.success());
        String::from_utf8_lossy(&output.stdout).trim().to_owned()
    }

    /// `text` with the output base and the workspace as the fixture's
    /// placeholders.
    fn normalized(&self, text: &str) -> String {
        text.replace(&self.output_base(), "<OUTPUT_BASE>")
            .replace(&self.ws.display().to_string(), "<WORKSPACE>")
    }
}

fn genuine(name: &str) -> Vec<String> {
    fs::read_to_string(Path::new(FIXTURE).join(name))
        .unwrap()
        .lines()
        .map(str::to_owned)
        .collect()
}

#[test]
fn external_files_print_below_the_output_base_external_directory() {
    let ws = Workspace::new();
    let output = ws.run(&[
        "build",
        "//:m",
        "//:e",
        "//arcold:d",
        "//arcuse:d",
        "--curses=no",
        "--color=no",
    ]);
    let stderr = String::from_utf8_lossy(&output.stderr);
    let mut observed = vec![format!("rc={}", output.status.code().unwrap())];
    observed.extend(
        stderr
            .lines()
            .filter(|line| line.starts_with("DEBUG: ") || line.starts_with("WARNING: "))
            .map(|line| ws.normalized(line)),
    );
    // The events' order across targets is no part of this parity (the
    // analysis order differs); each line is.
    let mut want = genuine("genuine_build.txt");
    observed[1..].sort();
    want[1..].sort();
    assert_eq!(observed, want, "{stderr}");
    // Nothing names where bazel-rs keeps the repositories.
    assert!(!stderr.contains("/repositories/"), "{stderr}");
}

#[test]
fn query_location_of_an_external_target() {
    let ws = Workspace::new();
    let output = ws.run(&["query", "--output=location", "@arc//:a"]);
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let observed: Vec<String> = stdout.lines().map(|line| ws.normalized(line)).collect();
    assert_eq!(observed, genuine("genuine_query.txt"));
}
