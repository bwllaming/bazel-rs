//! darwin-sandbox (docs/MACOS.md, src/sandbox/darwin.rs): genuine Bazel
//! 9.3.1 darwin-arm64's behaviour captured on a macOS runner
//! (tests/fixtures/darwin_sandbox/reference_receipt.json, capture.py from
//! scripts/host_parity.py's darwin_sandbox case) replayed natively: per step
//! the exit code, the strategies of the `INFO: N processes` line, what each
//! probe genrule may read, write and reach (default, `block-network`,
//! `requires-network`, `no-sandbox`, `local`; `--sandbox_default_allow_network`,
//! `--sandbox_writable_path`, `--sandbox_block_path` on a symlink) and the
//! profiles `--sandbox_debug` keeps. Hermetic: the remote connection probe
//! is skipped (the profiles carry the network rules), the modules come from
//! the offline built-in registry. Input staging differs by design
//! (hardlinks, docs/HARDLINK_STAGING.md): the probes' input-kind lines are
//! not compared.
#![cfg(target_os = "macos")]
#![allow(clippy::disallowed_methods)] // test harness: starts processes directly (clippy.toml)

use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use base64::Engine as _;
use serde_json::Value;
include!("support/explicit_modules.rs");

fn manifest() -> &'static Path {
    Path::new(env!("CARGO_MANIFEST_DIR"))
}

fn receipt() -> Value {
    serde_json::from_slice(
        &fs::read(manifest().join("tests/fixtures/darwin_sandbox/reference_receipt.json")).unwrap(),
    )
    .unwrap()
}

struct Workspace {
    _root: tempfile::TempDir,
    root: PathBuf,
}

impl Workspace {
    fn new() -> Self {
        let base = std::env::var_os("TMPDIR")
            .map(PathBuf::from)
            .unwrap_or_else(std::env::temp_dir);
        let dir = tempfile::Builder::new()
            .prefix("darwin-sandbox-")
            .tempdir_in(base)
            .unwrap();
        let root = fs::canonicalize(dir.path()).unwrap();
        let ws = root.join("ws");
        copy(
            &manifest().join("tests/fixtures/darwin_sandbox/workspace"),
            &ws,
        );
        // The offline platforms module instead of the registry's.
        let module = fs::read_to_string(ws.join("MODULE.bazel")).unwrap();
        fs::write(
            ws.join("MODULE.bazel"),
            module
                + "bazel_dep(name = \"platforms\", version = \"1.0.0\")\n\
                   local_path_override(module_name = \"platforms\", path = \"platforms\")\n",
        )
        .unwrap();
        extract_platforms(&ws.join("platforms"));
        fs::create_dir_all(root.join("tmp")).unwrap();
        Self { _root: dir, root }
    }

    fn ws(&self) -> PathBuf {
        self.root.join("ws")
    }

    fn cache(&self) -> PathBuf {
        self.root.join("cache")
    }

    fn run(&self, args: &[String]) -> Output {
        let registry = format!(
            "--registry=file://{}/tests/fixtures/builtin_modules/registry",
            manifest().display()
        );
        let output = Command::new(TIMEOUT)
            .args(["--kill-after=5s", "600s", env!("CARGO_BIN_EXE_bazel-rs")])
            .arg("--ignore_all_rc_files")
            .arg(format!("--cache-dir={}", self.cache().display()))
            .arg(&args[0])
            .args(["--experimental-builtin-modules", &registry])
            .args(&args[1..])
            .current_dir(self.ws())
            .env("TMPDIR", self.root.join("tmp"))
            .explicit_modules()
            .output()
            .unwrap();
        assert_ne!(output.status.code(), Some(124), "CLI timed out: {args:?}");
        output
    }
}

fn copy(source: &Path, destination: &Path) {
    fs::create_dir_all(destination).unwrap();
    for entry in fs::read_dir(source).unwrap() {
        let entry = entry.unwrap();
        let target = destination.join(entry.file_name());
        if entry.file_type().unwrap().is_dir() {
            copy(&entry.path(), &target);
        } else {
            fs::copy(entry.path(), target).unwrap();
        }
    }
}

/// The genuine platforms 1.0.0 module archive (tests/exec_transition.rs).
fn extract_platforms(to: &Path) {
    let manifest: Value = serde_json::from_slice(
        &fs::read(manifest().join("tests/fixtures/label_repo_name/platforms_archive.json"))
            .unwrap(),
    )
    .unwrap();
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(manifest["bytes_base64"].as_str().unwrap())
        .unwrap();
    let mut archive = tar::Archive::new(flate2::read::GzDecoder::new(bytes.as_slice()));
    for entry in archive.entries().unwrap() {
        let mut entry = entry.unwrap();
        let path = to.join(entry.path().unwrap());
        if entry.header().entry_type().is_dir() {
            fs::create_dir_all(&path).unwrap();
            continue;
        }
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        let mut content = Vec::new();
        entry.read_to_end(&mut content).unwrap();
        fs::write(path, content).unwrap();
    }
}

/// host_parity's `INFO: N processes` normalization: the spawn counts per
/// strategy, without internal actions and cache hits.
fn processes(stderr: &str) -> String {
    let mut lines = String::new();
    for line in stderr.lines() {
        let Some(rest) = line.strip_prefix("INFO: ") else {
            continue;
        };
        let Some((head, counts)) = rest.split_once(": ") else {
            continue;
        };
        if !head.ends_with("processes") && !head.ends_with("process") {
            continue;
        }
        let kept: Vec<&str> = counts
            .trim_end_matches('.')
            .split(", ")
            .filter(|part| {
                ![
                    "internal",
                    "action cache hit",
                    "disk cache hit",
                    "remote cache hit",
                ]
                .iter()
                .any(|suffix| part.ends_with(suffix))
            })
            .collect();
        lines += &format!("INFO: processes: {}\n", kept.join(", "));
    }
    lines
}

/// A probe's report without the lines this test does not compare.
fn probe_lines(text: &str) -> Vec<String> {
    text.lines()
        .filter(|line| {
            !line.starts_with("network_remote=")
                && !line.starts_with("source_input_symlink=")
                && !line.starts_with("generated_input_symlink=")
        })
        .map(str::to_owned)
        .collect()
}

/// `getconf NAME`, resolved (the profile's spelling).
fn getconf(name: &str) -> Option<PathBuf> {
    let output = Command::new("/usr/bin/getconf").arg(name).output().ok()?;
    let value = String::from_utf8(output.stdout).ok()?;
    fs::canonicalize(value.trim_end()).ok()
}

/// One bazel-rs profile in the receipt's spelling.
fn normalize_profile(text: &str, ws: &Workspace) -> String {
    let work = format!("{}/work/", ws.cache().display());
    let mut out = String::new();
    for line in text.lines() {
        let mut line = line.to_owned();
        if let Some(start) = line.find(&work) {
            let rest = &line[start + work.len()..];
            let end = rest.find('/').unwrap_or(rest.len());
            line = format!("{}<SPAWN_DIR>{}", &line[..start], &rest[end..]);
            line = line.replace("<SPAWN_DIR>/execroot\"", "<SPAWN_ROOT>\"");
        }
        line = line.replace(
            &format!("\"{}\"", ws.root.join("tmp").display()),
            "\"<TMP>\"",
        );
        for (name, placeholder) in [
            ("DARWIN_USER_TEMP_DIR", "<DARWIN_USER_TEMP_DIR>"),
            ("DARWIN_USER_CACHE_DIR", "<DARWIN_USER_CACHE_DIR>"),
        ] {
            if let Some(path) = getconf(name) {
                line = line.replace(
                    &format!("\"{}\"", path.display()),
                    &format!("\"{placeholder}\""),
                );
            }
        }
        out += &line;
        out.push('\n');
    }
    out
}

/// The user's `Library` directories a profile names only where they exist:
/// lines of the receipt's (the runner's) set missing here are dropped, and
/// the home directory becomes `<USER_HOME>`.
fn host_library_lines(text: &str) -> String {
    let home = std::env::var("HOME").unwrap_or_default();
    text.lines()
        .map(|line| line.replace(&format!("\"{home}/Library/"), "\"<USER_HOME>/Library/"))
        .filter(|line| {
            let Some(start) = line.find("<USER_HOME>/Library/") else {
                return true;
            };
            let rest = &line[start + "<USER_HOME>/".len()..];
            let sub = rest.split('"').next().unwrap_or_default();
            Path::new(&home).join(sub).exists()
        })
        .map(|line| format!("{line}\n"))
        .collect()
}

/// host_parity's sorted_blocks: the indented lines of a block sorted.
fn sorted_blocks(text: &str) -> String {
    let mut out = Vec::new();
    let mut run = Vec::new();
    for line in text.split_inclusive('\n') {
        if line.starts_with(' ') {
            run.push(line);
            continue;
        }
        run.sort_unstable();
        out.append(&mut run);
        out.push(line);
    }
    run.sort_unstable();
    out.append(&mut run);
    out.concat()
}

/// The `## profile N` sections of a receipt's profile listing.
fn receipt_profiles(text: &str) -> Vec<String> {
    let mut profiles: Vec<String> = text
        .split("## profile ")
        .filter(|part| !part.is_empty())
        .map(|part| {
            part.split_once('\n')
                .map_or("", |(_, body)| body)
                .to_owned()
        })
        .map(|body| sorted_blocks(&host_library_lines(&body)))
        .collect();
    profiles.sort();
    profiles
}

#[test]
fn darwin_sandbox_matches_genuine() {
    let receipt = receipt();
    let ws = Workspace::new();
    let info = ws.run(&["info".to_owned(), "execution_root".to_owned()]);
    assert!(
        info.status.success(),
        "{}",
        String::from_utf8_lossy(&info.stderr)
    );
    let execroot = String::from_utf8(info.stdout).unwrap().trim().to_owned();
    for step in receipt["steps"].as_array().unwrap() {
        let mut args: Vec<String> = step["args"]
            .as_array()
            .unwrap()
            .iter()
            .map(|arg| {
                arg.as_str()
                    .unwrap()
                    .replace("{WORKSPACE}", &ws.ws().display().to_string())
                    .replace("{EXECROOT}", &execroot)
            })
            .collect();
        args.push("--action_env=PROBE_NETWORK=0".to_owned());
        let output = ws.run(&args);
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(!stderr.contains("panicked"), "{stderr}");
        assert_eq!(
            i64::from(output.status.code().unwrap_or(-1)),
            step["exit"].as_i64().unwrap(),
            "{args:?}: {stderr}"
        );
        assert_eq!(
            processes(&stderr),
            step["processes"].as_str().unwrap(),
            "{args:?}"
        );
        for (name, genuine) in step["probes"].as_object().unwrap() {
            let native = fs::read_to_string(ws.ws().join(format!("bazel-bin/{name}.txt")))
                .unwrap_or_else(|e| panic!("{name}: {e}: {stderr}"));
            assert_eq!(
                probe_lines(&native),
                probe_lines(genuine.as_str().unwrap()),
                "{args:?} {name}"
            );
        }
        if let Some(genuine) = step["profiles"].as_str() {
            let mut native: Vec<String> = fs::read_dir(ws.cache().join("work"))
                .unwrap()
                .filter_map(Result::ok)
                .map(|entry| entry.path().join("sandbox.sb"))
                .filter(|path| path.exists())
                .map(|path| {
                    let text = normalize_profile(&fs::read_to_string(path).unwrap(), &ws);
                    sorted_blocks(&host_library_lines(&text))
                })
                .collect();
            native.sort();
            assert_eq!(native, receipt_profiles(genuine), "{args:?}");
        }
    }
}
