//! repository_ctx file operations on `cp -r` copies of another
//! repository's published tree, against the genuine 9.3.1 capture
//! `tests/fixtures/sealed_copies` (capture.py, reference_receipt.json).
//!
//! Natively the copied tree is sealed (docs/REPOSITORY_SLOTS.md, "Sealed
//! trees"), so the copies keep read-only files and directories; genuine
//! copies its writable `external/` directory. `file`, `template`, `patch`,
//! `download` (into a new directory and over an existing file), `delete`
//! (a file and a whole copy), `symlink` and `rename` (a file, and a
//! directory to another parent) produce what genuine produces: the copies'
//! modes do not matter to them (crate::nofollow_modes::WritableDirectory).

#![allow(clippy::disallowed_methods)] // isolated CLI test harness

use std::fs;
use std::io::{Read, Write};
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::Command;

include!("support/explicit_modules.rs");

fn copy_dir(from: &Path, to: &Path) {
    fs::create_dir_all(to).unwrap();
    for entry in fs::read_dir(from).unwrap() {
        let entry = entry.unwrap();
        fs::copy(entry.path(), to.join(entry.file_name())).unwrap();
    }
}

/// Serves `body` to every request (the downloads' source).
fn serve(body: Vec<u8>) -> String {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}/payload.txt", listener.local_addr().unwrap());
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            let mut request = [0u8; 4096];
            let _ = stream.read(&mut request);
            let _ = stream.write_all(
                format!(
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                )
                .as_bytes(),
            );
            let _ = stream.write_all(&body);
        }
    });
    url
}

#[test]
fn file_operations_on_copies_of_sealed_trees_match_genuine() {
    let fixture = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/sealed_copies");
    let receipt: serde_json::Value =
        serde_json::from_slice(&fs::read(fixture.join("reference_receipt.json")).unwrap()).unwrap();
    assert_eq!(receipt["reference"], "bazel 9.3.1");
    let temp = tempfile::Builder::new()
        .prefix("sealed-copies-")
        .tempdir_in(test_temp::root())
        .unwrap();
    let ws = temp.path().join("ws");
    copy_dir(&fixture.join("workspace"), &ws);
    fs::create_dir(temp.path().join("home")).unwrap();
    let url = serve(fs::read(ws.join("payload.txt")).unwrap());
    let output = Command::new(env!("CARGO_BIN_EXE_bazel-rs"))
        .args([
            "--ignore_all_rc_files",
            &format!("--cache-dir={}", temp.path().join("cache").display()),
            "build",
            &format!("--repo_env=PAYLOAD_URL={url}"),
            "//:out",
        ])
        .current_dir(&ws)
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .env("HOME", temp.path().join("home"))
        .env("BAZEL_RS_RESULT_LINES", "1")
        .explicit_modules()
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    // BAZEL_RS_RESULT_LINES: `<label>\t<path>` per built file.
    let stdout = String::from_utf8_lossy(&output.stdout);
    let out_path = stdout
        .lines()
        .find_map(|line| line.strip_prefix("//:out\t"))
        .unwrap_or_else(|| panic!("no result line: {stdout}"));
    let out = fs::read_to_string(out_path).unwrap();
    let expected: Vec<&str> = receipt["out"]
        .as_array()
        .unwrap()
        .iter()
        .map(|line| line.as_str().unwrap())
        .collect();
    assert_eq!(out.lines().collect::<Vec<_>>(), expected);
    // The copied tree was sealed: the copies started read-only.
    let trees = temp.path().join("cache/repositories/direct/trees");
    let source = fs::read_dir(&trees)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .find(|tree| tree.join("a/c.txt").is_file())
        .expect("the @src tree");
    let mode = |path: &Path| fs::metadata(path).unwrap().permissions().mode() & 0o777;
    assert_eq!(
        [
            mode(&source.join("a")),
            mode(&source.join("a/sub")),
            mode(&source.join("a/c.txt"))
        ],
        // ctx.file's default is executable = True: 0755, sealed 0555.
        [0o555, 0o555, 0o555]
    );
}
