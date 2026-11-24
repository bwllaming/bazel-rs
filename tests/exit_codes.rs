//! Bazel 9.3's exit codes (crate::exit_code; genuine matrix in
//! tests/fixtures/exit_codes): command-line errors exit 2, remote errors 34,
//! query evaluation errors 7, failed tests 3, no test targets 4, build
//! failures 1, an empty `build` 0, a registry module resolution could not
//! read 32 (tests/fixtures/registry_failure_exit_codes).
#![allow(clippy::disallowed_methods)] // test harness: starts processes directly (clippy.toml)

use std::fs;
use std::path::Path;
use std::process::{Command, Stdio};

include!("support/module_archives.rs");

const FIXTURES: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures");

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

/// `text` split into shell words (single quotes only, as genuine.txt uses).
fn words(text: &str) -> Vec<String> {
    let mut words = Vec::new();
    let mut word = String::new();
    let mut quoted = false;
    let mut any = false;
    for c in text.chars() {
        match c {
            '\'' => {
                quoted = !quoted;
                any = true;
            }
            ' ' if !quoted => {
                if any {
                    words.push(std::mem::take(&mut word));
                    any = false;
                }
            }
            c => {
                word.push(c);
                any = true;
            }
        }
    }
    if any {
        words.push(word);
    }
    words
}

#[test]
fn exit_codes_match_bazel() {
    let base = std::env::var_os("TMPDIR")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(std::env::temp_dir);
    let temp = tempfile::Builder::new()
        .prefix("exit-codes-")
        .tempdir_in(base)
        .unwrap();
    let ws = temp.path().join("ws");
    copy(&Path::new(FIXTURES).join("build_results/workspace"), &ws);
    let build = ws.join("BUILD");
    let mut text = fs::read_to_string(&build).unwrap();
    text.push_str("my_test(name = \"tf\", code = 1)\n");
    fs::write(&build, text).unwrap();
    let cache = temp.path().join("cache");
    let matrix = fs::read_to_string(Path::new(FIXTURES).join("exit_codes/genuine.txt")).unwrap();
    let mut failures = Vec::new();
    for line in matrix.lines() {
        let (args, code) = line.rsplit_once(" => ").unwrap();
        let expected: i32 = code.parse().unwrap();
        let output = Command::new(env!("CARGO_BIN_EXE_bazel-rs"))
            .arg("--ignore_all_rc_files")
            .arg(format!("--cache-dir={}", cache.display()))
            .args(with_builtin_registry(&words(args)))
            .args(["--curses=no", "--color=no"])
            .current_dir(&ws)
            .env("BAZEL_RS_MAX_MEMORY_MB", "4096")
            .stdin(Stdio::null())
            .seeded_archives()
            .output()
            .unwrap();
        let code = output.status.code();
        if code != Some(expected) {
            failures.push(format!(
                "{args}: exit {code:?}, genuine {expected}\n{}",
                String::from_utf8_lossy(&output.stderr)
            ));
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n\n"));
}

/// A registry that answers every request `501 Not Implemented` (a status
/// HttpConnector does not retry); its URL.
fn failing_registry() -> String {
    use std::io::{Read as _, Write as _};
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            let mut request = Vec::new();
            let mut buffer = [0; 1024];
            while !request.windows(4).any(|window| window == b"\r\n\r\n") {
                match stream.read(&mut buffer) {
                    Ok(0) | Err(_) => break,
                    Ok(read) => request.extend_from_slice(&buffer[..read]),
                }
            }
            let _ = stream.write_all(
                b"HTTP/1.1 501 Not Implemented\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
            );
        }
    });
    format!("http://{address}")
}

/// An HTTP server of the files under `root` (404 for anything else); its
/// URL.
fn static_registry(root: &Path) -> String {
    use std::io::{Read as _, Write as _};
    let root = root.to_path_buf();
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            let mut request = Vec::new();
            let mut buffer = [0; 1024];
            while !request.windows(4).any(|window| window == b"\r\n\r\n") {
                match stream.read(&mut buffer) {
                    Ok(0) | Err(_) => break,
                    Ok(read) => request.extend_from_slice(&buffer[..read]),
                }
            }
            let request = String::from_utf8_lossy(&request);
            let path = request
                .lines()
                .next()
                .and_then(|line| line.split(' ').nth(1))
                .unwrap_or("/");
            let file = (!path.contains(".."))
                .then(|| root.join(path.trim_start_matches('/')))
                .filter(|file| file.is_file())
                .and_then(|file| fs::read(file).ok());
            let _ = match file {
                Some(body) => stream
                    .write_all(
                        format!(
                            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                            body.len()
                        )
                        .as_bytes(),
                    )
                    .and_then(|()| stream.write_all(&body)),
                None => stream.write_all(
                    b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                ),
            };
        }
    });
    format!("http://{address}")
}

/// The first error line of `stderr` without its `ERROR: ` / `bazel-rs: `
/// prefix, with the registry URL and the failing module normalized like
/// the genuine captures.
fn first_error(stderr: &str, registry: &str) -> String {
    let line = stderr
        .lines()
        .find_map(|line| {
            line.strip_prefix("ERROR: ")
                .or_else(|| line.strip_prefix("bazel-rs: "))
        })
        .unwrap_or("");
    let line = line.replace(registry, "<REGISTRY>");
    match line.split_once("<REGISTRY>/modules/") {
        Some((head, tail)) => match tail.split_once("/MODULE.bazel") {
            Some((_, rest)) => format!("{head}<REGISTRY>/modules/<M>/<V>/MODULE.bazel{rest}"),
            None => line,
        },
        None => line,
    }
}

/// Which failure wins when module resolution cannot read the registry
/// (genuine captures in tests/fixtures/registry_failure_exit_codes): native
/// option-value errors and an undefined `--config` first (exit 2), then the
/// main repository mapping's `Error accessing registry` (exit 32) before an
/// analyzing command's unknown option, target patterns and query errors;
/// `mod` validates its subcommand first and reports the failure with 37.
#[test]
fn registry_failures_match_bazel() {
    let base = std::env::var_os("TMPDIR")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(std::env::temp_dir);
    let temp = tempfile::Builder::new()
        .prefix("registry-failure-exit-codes-")
        .tempdir_in(base)
        .unwrap();
    let fixture = Path::new(FIXTURES).join("registry_failure_exit_codes");
    let ws = temp.path().join("ws");
    copy(&fixture.join("workspace"), &ws);
    let cache = temp.path().join("cache");
    let failing = failing_registry();
    let mut failures = Vec::new();
    for (matrix, registry) in [
        ("genuine_501.tsv", failing.as_str()),
        ("genuine_refused.tsv", "http://127.0.0.1:1"),
    ] {
        let text = fs::read_to_string(fixture.join(matrix)).unwrap();
        for line in text.lines() {
            let mut fields = line.split('\t');
            let (args, code, message) = (
                fields.next().unwrap(),
                fields.next().unwrap(),
                fields.next().unwrap(),
            );
            let expected: i32 = code.parse().unwrap();
            let output = Command::new(env!("CARGO_BIN_EXE_bazel-rs"))
                .arg("--ignore_all_rc_files")
                .arg(format!("--cache-dir={}", cache.display()))
                .args(words(args))
                .arg(format!("--registry={registry}"))
                .args(["--curses=no", "--color=no"])
                .current_dir(&ws)
                .env("BAZEL_RS_MAX_MEMORY_MB", "4096")
                .stdin(Stdio::null())
                .output()
                .unwrap();
            let stderr = String::from_utf8_lossy(&output.stderr);
            let actual = first_error(&stderr, registry);
            let expected_message = message
                .strip_prefix("ERROR: ")
                .unwrap_or(message)
                .to_owned();
            if output.status.code() != Some(expected) || actual != expected_message {
                failures.push(format!(
                    "{matrix}: {args}: exit {:?}, genuine {expected}\n  native:  {actual}\n  genuine: {expected_message}\n{stderr}",
                    output.status.code()
                ));
            }
        }
        // A failed resolution writes no lockfile (genuine writes none).
        assert!(!ws.join("MODULE.bazel.lock").exists(), "{matrix}");
    }
    assert!(failures.is_empty(), "{}", failures.join("\n\n"));
}

/// Exit codes when module resolution or a module extension fails (genuine
/// captures in tests/fixtures/module_failure_exit_codes): 48 for a module
/// resolution failure in every command that computes the main repository
/// mapping (`fetch` too), 37 in `mod` and for a `local_path_override` whose
/// repository cannot be fetched, 0 in `info`; extension failures fail only
/// what needs their repositories.
#[test]
fn module_failures_match_bazel() {
    let base = std::env::var_os("TMPDIR")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(std::env::temp_dir);
    let temp = tempfile::Builder::new()
        .prefix("module-failure-exit-codes-")
        .tempdir_in(base)
        .unwrap();
    let fixture = Path::new(FIXTURES).join("module_failure_exit_codes");
    // The offline built-in registry over HTTP: like Bazel's default
    // registry, a remote one makes every command resolve the module graph
    // (docs/REGISTRY_LOCKFILE.md "Eager resolution boundary").
    let registry = static_registry(&Path::new(FIXTURES).join("builtin_modules/registry"));
    let text = fs::read_to_string(fixture.join("genuine.tsv")).unwrap();
    let mut failures = Vec::new();
    for line in text.lines() {
        let mut fields = line.split('\t');
        let (case, args, code) = (
            fields.next().unwrap(),
            fields.next().unwrap(),
            fields.next().unwrap(),
        );
        // `mod graph` evaluates every extension in Bazel
        // (BazelModuleInspectorFunction; exit 2 when one fails): not
        // modelled natively (B80 of docs/OSS_CORPUS.md).
        if args == "mod graph" && case.starts_with("ext_") {
            continue;
        }
        let expected: i32 = code.parse().unwrap();
        // A fresh copy per command (a success writes MODULE.bazel.lock).
        let ws = temp.path().join("ws");
        let _ = fs::remove_dir_all(&ws);
        copy(&fixture.join("cases").join(case), &ws);
        let cache = temp.path().join(format!("cache-{case}"));
        seed_module_archives(&cache, TOOLCHAIN_CLOSURE_ARCHIVES);
        // A case's `extra_args` follow the command's (genuine.tsv's ARGS do
        // not show them).
        let extra = ws.join("extra_args");
        let mut argv = words(args);
        if let Ok(text) = fs::read_to_string(&extra) {
            fs::remove_file(&extra).unwrap();
            argv.extend(text.split_whitespace().map(str::to_owned));
        }
        // Not `info` (Bazel's takes `--registry` but resolves nothing).
        if args != "info workspace" {
            argv.insert(1, format!("--registry={registry}"));
        }
        let output = Command::new(env!("CARGO_BIN_EXE_bazel-rs"))
            .arg("--ignore_all_rc_files")
            .arg(format!("--cache-dir={}", cache.display()))
            .args(argv)
            .args(["--curses=no", "--color=no"])
            .current_dir(&ws)
            .env("BAZEL_RS_MAX_MEMORY_MB", "4096")
            .stdin(Stdio::null())
            .output()
            .unwrap();
        if output.status.code() != Some(expected) {
            failures.push(format!(
                "{case}: {args}: exit {:?}, genuine {expected}\n{}",
                output.status.code(),
                String::from_utf8_lossy(&output.stderr)
            ));
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n\n"));
}
