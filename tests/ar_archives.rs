//! Bazel 9.3 `.ar`/`.deb` extraction (docs/AR_ARCHIVES.md: ArFunction over
//! commons-compress 1.26.1 ArArchiveInputStream, DecompressorValue type
//! inference, DownloadManager's download_and_extract file name). Every case
//! of tests/fixtures/ar_archives/reference_receipt.json — genuine Bazel
//! 9.3.0 (`--batch --ignore_all_rc_files`, a removed scratch output user
//! root) querying repositories that extract hand-built archives — runs
//! natively here and is compared with the genuine capture: the tree the
//! repository rule lists right after extraction (names, types, modes,
//! atime/mtime, sizes, digests), the labels of an `http_archive` of a
//! `.deb`, Bazel's error message, or the declared native divergence.
#![allow(clippy::disallowed_methods)] // test harness: starts processes directly (clippy.toml)

use base64::Engine as _;
use serde_json::Value;
use std::collections::BTreeMap;
use std::fs;
use std::io::{Read, Write};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::Duration;

fn manifest() -> &'static Path {
    Path::new(env!("CARGO_MANIFEST_DIR"))
}

fn receipt() -> Value {
    serde_json::from_slice(
        &fs::read(manifest().join("tests/fixtures/ar_archives/reference_receipt.json")).unwrap(),
    )
    .unwrap()
}

/// The receipt's cases, each in exactly one test below.
const GROUPS: &[(&str, &[&str])] = &[
    (
        "trees",
        &[
            "deb_extract",
            "deb_extract_root",
            "gnu_extract",
            "bsd_extract",
            "subdirs_extract",
            "dup_extract",
            "det_extract",
            "empty_extract",
            "empty_extract_nested",
            "nopad_extract",
            "truncdata_extract",
            "rename_extract",
            "strip_prefix_ignored",
            "strip_components_ignored",
            "type_ar",
            "type_deb",
            "ext_ar",
            "merge_existing",
            "module_ctx_extract",
        ],
    ),
    (
        "downloads",
        &[
            "empty_download",
            "download_deb",
            "download_type_deb",
            "download_rename",
            "download_strip_components",
            "symbol_table_download",
            "download_bad_name",
            "download_bad_type",
            "download_bad_type_ar",
            "download_unknown_type",
            "download_mirror_name",
            "http_archive_deb",
            "http_archive_type",
        ],
    ),
    (
        "corrupt",
        &[
            "bad_magic",
            "html_page",
            "gzip_as_deb",
            "short_file",
            "empty_file",
            "trunc_header",
            "trailer_short",
            "trailer_bad",
            "size_bad",
            "size_negative",
            "mode_bad",
            "mode_blank",
            "uid_bad",
            "mtime_bad",
            "mtime_blank",
            "trailing_garbage",
        ],
    ),
    (
        "names",
        &[
            "gnu_no_table",
            "gnu_bad_offset",
            "gnu_empty_at_offset",
            "gnu_offset_overflow",
            "gnu_table_short",
            "bsd_name_truncated",
            "bsd_nul_name",
            "symbol_table_root",
            "file_then_dir",
            "dir_then_file",
            "readonly_dup",
            "escape_name",
            "special_bits",
            "nonascii_name",
        ],
    ),
];

/// A loopback HTTP server for the receipt's archives (`/NAME`; anything
/// else is a 404).
struct Server {
    base: String,
    stop: Arc<AtomicBool>,
    worker: Option<thread::JoinHandle<()>>,
}

impl Server {
    fn new(files: BTreeMap<String, Vec<u8>>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let stop = Arc::new(AtomicBool::new(false));
        let stopped = stop.clone();
        let worker = thread::spawn(move || {
            while !stopped.load(Ordering::SeqCst) {
                let mut stream = match listener.accept() {
                    Ok((stream, _)) => stream,
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(2));
                        continue;
                    }
                    Err(error) => panic!("listener: {error}"),
                };
                stream.set_nonblocking(false).unwrap();
                stream
                    .set_read_timeout(Some(Duration::from_secs(5)))
                    .unwrap();
                let mut request = Vec::new();
                let mut byte = [0];
                while !request.ends_with(b"\r\n\r\n") && request.len() < 16384 {
                    if stream.read(&mut byte).unwrap_or(0) == 0 {
                        break;
                    }
                    request.push(byte[0]);
                }
                let text = String::from_utf8_lossy(&request);
                let path = text.split(' ').nth(1).unwrap_or("/");
                let response = match files.get(path.trim_start_matches('/')) {
                    Some(body) => {
                        let mut response = format!(
                            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                            body.len()
                        )
                        .into_bytes();
                        response.extend_from_slice(body);
                        response
                    }
                    None => {
                        b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                            .to_vec()
                    }
                };
                let _ = stream.write_all(&response);
            }
        });
        Self {
            base,
            stop,
            worker: Some(worker),
        }
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        self.worker.take().unwrap().join().unwrap();
    }
}

/// The fixture workspace, its private native cache and the archive server.
struct Workspace {
    root: tempfile::TempDir,
    _server: Server,
}

impl Workspace {
    fn new(receipt: &Value) -> Self {
        let base = std::env::var_os("TMPDIR")
            .map(PathBuf::from)
            .unwrap_or_else(std::env::temp_dir);
        let root = tempfile::Builder::new()
            .prefix("ar-archives-")
            .tempdir_in(base)
            .unwrap();
        let archives: BTreeMap<String, Vec<u8>> = receipt["archives"]
            .as_object()
            .unwrap()
            .iter()
            .map(|(name, data)| {
                let bytes = base64::engine::general_purpose::STANDARD
                    .decode(data.as_str().unwrap())
                    .unwrap();
                (name.clone(), bytes)
            })
            .collect();
        let server = Server::new(archives.clone());
        let ws = root.path().join("ws");
        for directory in ["ws", "home", "tmp"] {
            fs::create_dir_all(root.path().join(directory)).unwrap();
        }
        for (name, text) in receipt["workspace"].as_object().unwrap() {
            let text = text.as_str().unwrap().replace("@@BASE@@", &server.base);
            fs::write(ws.join(name), text).unwrap();
        }
        for (name, bytes) in &archives {
            fs::write(ws.join(name), bytes).unwrap();
        }
        Self {
            root,
            _server: server,
        }
    }

    fn ws(&self) -> PathBuf {
        self.root.path().join("ws")
    }

    fn cache(&self) -> PathBuf {
        self.root.path().join("cache")
    }

    /// The receipt's native argv in a clean environment like the capture's.
    fn run(&self, command: &str) -> Output {
        let (verb, target) = command.split_once(' ').unwrap();
        let registry = format!(
            "--registry=file://{}/tests/fixtures/builtin_modules/registry",
            manifest().display()
        );
        let mut process = Command::new("timeout");
        process
            .args(["--kill-after=5s", "180s", env!("CARGO_BIN_EXE_bazel-rs")])
            .arg("--ignore_all_rc_files")
            .arg(format!("--cache-dir={}", self.cache().display()))
            .arg(verb)
            .args(["--experimental-builtin-modules", &registry, target])
            .current_dir(self.ws())
            .env_clear()
            .env("PATH", "/usr/bin:/bin")
            .env("HOME", self.root.path().join("home"))
            .env("TMPDIR", self.root.path().join("tmp"))
            .env("LANG", "C.UTF-8")
            .env("LC_ALL", "C.UTF-8");
        // Verification switches of the gate runs (BAZEL_RS_VERIFY_*, ...).
        for (key, value) in std::env::vars_os() {
            if key.to_string_lossy().starts_with("BAZEL_RS_") {
                process.env(key, value);
            }
        }
        let output = process.output().unwrap();
        assert_ne!(output.status.code(), Some(124), "CLI timed out: {command}");
        output
    }

    /// Native stderr with the receipt's normalization (`<WS>`, `<REPO>`,
    /// `<CACHE>`, `127.0.0.1:<PORT>`, `temp<N>`).
    fn normalize(&self, stderr: &[u8]) -> String {
        // The receipt decodes both sides' bytes as Latin-1.
        let mut text: String = stderr.iter().map(|byte| char::from(*byte)).collect();
        let stages = format!("{}/repositories/direct/stages/", self.cache().display());
        text = replace_followed_by(&text, &stages, |c| c.is_ascii_hexdigit(), "<REPO>");
        text = text.replace(&self.cache().display().to_string(), "<CACHE>");
        let ws = self.ws();
        for path in [ws.clone(), fs::canonicalize(&ws).unwrap()] {
            text = text.replace(&path.display().to_string(), "<WS>");
        }
        text = replace_followed_by(
            &text,
            "127.0.0.1:",
            |c| c.is_ascii_digit(),
            "127.0.0.1:<PORT>",
        );
        replace_followed_by(
            &text,
            "/temp",
            |c| c.is_ascii_digit() || c == '-',
            "/temp<N>",
        )
    }
}

/// Replace `prefix` followed by one or more characters matching `part`.
fn replace_followed_by(
    text: &str,
    prefix: &str,
    part: impl Fn(char) -> bool,
    with: &str,
) -> String {
    let mut out = String::new();
    let mut rest = text;
    while let Some(index) = rest.find(prefix) {
        out.push_str(&rest[..index]);
        let after = &rest[index + prefix.len()..];
        let length: usize = after
            .chars()
            .take_while(|c| part(*c))
            .map(char::len_utf8)
            .sum();
        if length == 0 {
            out.push_str(prefix);
        } else {
            out.push_str(with);
        }
        rest = &after[length..];
    }
    out.push_str(rest);
    out
}

/// The repository rules' listings (`AR-LISTING-BEGIN CASE` .. `AR-LISTING-END`)
/// as `[CASE]\n` + body, timestamps from 1750000000 on as `<NOW>`.
fn listings(stderr: &str) -> Option<String> {
    let mut found = String::new();
    let mut rest = stderr;
    while let Some(start) = rest.find("AR-LISTING-BEGIN ") {
        let after = &rest[start + "AR-LISTING-BEGIN ".len()..];
        let (case, body) = after.split_once('\n').unwrap();
        let end = body.find("AR-LISTING-END").unwrap();
        found.push_str(&format!("[{case}]\n"));
        for line in body[..end].lines() {
            let mut fields: Vec<String> = line.split('|').map(str::to_owned).collect();
            if fields.len() == 6 {
                for field in &mut fields[3..5] {
                    if field
                        .parse::<f64>()
                        .is_ok_and(|time| time >= 1_750_000_000.0)
                    {
                        *field = "<NOW>".to_owned();
                    }
                }
            }
            found.push_str(&fields.join("|"));
            found.push('\n');
        }
        rest = &body[end..];
    }
    (!found.is_empty()).then_some(found)
}

/// Labels without their repository part (Bazel prints apparent labels).
fn label_names(stdout: &str) -> Vec<String> {
    let mut names: Vec<String> = stdout
        .lines()
        .filter_map(|line| line.split_once("//").map(|(_, name)| name.to_owned()))
        .collect();
    names.sort();
    names
}

fn check_group(group: &str) {
    let receipt = receipt();
    let cases = GROUPS.iter().find(|(name, _)| *name == group).unwrap().1;
    let workspace = Workspace::new(&receipt);
    let mut failures = Vec::new();
    for case in cases {
        let expected = &receipt["cases"][*case];
        let genuine = &expected["genuine"];
        let verdict = expected["verdict"].as_str().unwrap();
        let output = workspace.run(expected["command"].as_str().unwrap());
        let stderr = workspace.normalize(&output.stderr);
        let ok = output.status.success();
        let listing = listings(&stderr);
        let problem = match verdict {
            "EXACT" if !ok => Some("native failed".to_owned()),
            "EXACT" if listing.as_deref() != genuine["listing"].as_str() => Some(format!(
                "listing differs:\n--- genuine\n{}\n--- native\n{}",
                genuine["listing"].as_str().unwrap_or("<none>"),
                listing.as_deref().unwrap_or("<none>")
            )),
            "EXACT_LABELS" if !ok => Some("native failed".to_owned()),
            "EXACT_LABELS"
                if label_names(&String::from_utf8_lossy(&output.stdout))
                    != label_names(genuine["stdout"].as_str().unwrap()) =>
            {
                Some(format!(
                    "labels differ: {:?}",
                    String::from_utf8_lossy(&output.stdout)
                ))
            }
            "EXACT_MESSAGE" if ok => Some("native succeeded".to_owned()),
            "EXACT_MESSAGE" if !stderr.contains(genuine["message"].as_str().unwrap()) => {
                Some(format!(
                    "genuine message {:?} not in native stderr",
                    genuine["message"]
                ))
            }
            "EXPECTED_NATIVE_UNSUPPORTED" | "REFERENCE_CRASH" if ok => {
                Some("native succeeded".to_owned())
            }
            "EXPECTED_NATIVE_UNSUPPORTED" | "REFERENCE_CRASH"
                if !stderr.contains(expected["native_message"].as_str().unwrap()) =>
            {
                Some(format!(
                    "declared native message {:?} not in native stderr",
                    expected["native_message"]
                ))
            }
            // A declared divergence that lists before failing (special
            // bits) lists what Bazel lists.
            "EXPECTED_NATIVE_UNSUPPORTED"
                if listing.is_some() && listing.as_deref() != genuine["listing"].as_str() =>
            {
                Some(format!("listing differs: {listing:?}"))
            }
            "EXACT"
            | "EXACT_LABELS"
            | "EXACT_MESSAGE"
            | "EXPECTED_NATIVE_UNSUPPORTED"
            | "REFERENCE_CRASH" => None,
            other => Some(format!("unknown verdict {other}")),
        };
        if let Some(problem) = problem {
            failures.push(format!("{case} ({verdict}): {problem}\n{stderr}"));
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n\n"));
}

#[test]
fn every_receipt_case_is_checked_exactly_once() {
    let receipt = receipt();
    let mut listed: Vec<&str> = GROUPS
        .iter()
        .flat_map(|(_, cases)| cases.iter().copied())
        .collect();
    listed.sort_unstable();
    let before = listed.len();
    listed.dedup();
    assert_eq!(before, listed.len(), "a case is listed twice");
    let mut recorded: Vec<&str> = receipt["cases"]
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect();
    recorded.sort_unstable();
    assert_eq!(listed, recorded);
    // No case was a mismatch when the receipt was captured.
    for (case, entry) in receipt["cases"].as_object().unwrap() {
        assert_ne!(entry["verdict"], "MISMATCH", "{case}");
    }
}

#[test]
fn extracted_trees_match_genuine_listings() {
    check_group("trees");
}

#[test]
fn download_and_extract_names_types_and_http_archive_match_genuine() {
    check_group("downloads");
}

#[test]
fn corrupt_and_truncated_archives_fail_with_commons_compress_texts() {
    check_group("corrupt");
}

#[test]
fn member_names_and_filesystem_conflicts_match_genuine_or_declared() {
    check_group("names");
}
