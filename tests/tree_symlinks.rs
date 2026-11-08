//! Symlinks inside tree artifacts (Bazel 9.3 TreeArtifactValue.visitTree,
//! skyframe/TreeArtifactValue.java:548-592, and ActionOutputMetadataStore
//! .constructTreeArtifactValueFromFilesystem, :245-316; docs/TREE_SYMLINKS.md):
//! every link below a tree root is followed, the regular files reached are
//! the tree's children at the links' paths, invalid links fail with Bazel's
//! validation errors. Every case of
//! tests/fixtures/tree_symlinks/reference_receipt.json — genuine Bazel 9.3.0
//! (`--batch --ignore_all_rc_files`, a removed scratch output user root) on
//! the fixture workspace — runs natively here and is compared with the
//! genuine capture: success, Bazel's error message, the consumers' listings
//! (the tree's Args expansion and the files an action reads through it), the
//! published trees' followed contents, a `run` program's and a test's
//! output through their runfiles; a declared divergence is a native failure
//! with its recorded message.
#![allow(clippy::disallowed_methods)] // test harness: starts processes directly (clippy.toml)

use std::collections::BTreeMap;
use std::fs;
use std::io::Read;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use base64::Engine as _;
use serde_json::Value;
use sha2::{Digest, Sha256};
include!("support/explicit_modules.rs");
include!("support/module_archives.rs");

fn manifest() -> &'static Path {
    Path::new(env!("CARGO_MANIFEST_DIR"))
}

fn fixture() -> PathBuf {
    manifest().join("tests/fixtures/tree_symlinks/workspace")
}

fn receipt() -> Value {
    serde_json::from_slice(
        &fs::read(manifest().join("tests/fixtures/tree_symlinks/reference_receipt.json")).unwrap(),
    )
    .unwrap()
}

/// The receipt's cases, each in exactly one test below.
const GROUPS: &[(&str, &[&str])] = &[
    (
        "resolved",
        &[
            "uplevel",
            "oci_from",
            "inside",
            "chain",
            "dir_inside",
            "dir_outside",
            "source_link",
            "empty_dir_link",
            "same_action",
            "abs_host",
            "absolute_local",
            "aliased",
            "consumer_local",
        ],
    ),
    (
        "invalid",
        &[
            "absolute_sandboxed",
            "dangling_rel",
            "dangling_abs",
            "special",
            "loop",
            "dir_cycle",
            "self_cycle",
        ],
    ),
    ("runfiles", &["run_uplevel", "test_runfiles"]),
    ("declared", &["undeclared", "undeclared_consumer"]),
];

struct Workspace(tempfile::TempDir);

impl Workspace {
    fn new() -> Self {
        let base = std::env::var_os("TMPDIR")
            .map(PathBuf::from)
            .unwrap_or_else(std::env::temp_dir);
        let ws = Self(
            tempfile::Builder::new()
                .prefix("tree-symlinks-")
                .tempdir_in(base)
                .unwrap(),
        );
        copy(&fixture(), ws.path());
        extract_platforms(&ws.path().join("platforms"));
        ws
    }

    fn path(&self) -> &Path {
        self.0.path()
    }

    /// `ARGV[0] --experimental-builtin-modules --registry=... ARGV[1..]`.
    fn run(&self, argv: &[String]) -> Output {
        let registry = format!(
            "--registry=file://{}/tests/fixtures/builtin_modules/registry",
            manifest().display()
        );
        let output = Command::new("timeout")
            .args(["--kill-after=5s", "300s", env!("CARGO_BIN_EXE_bazel-rs")])
            .args(["--ignore_all_rc_files", "--cache-dir=private-cache"])
            .arg(&argv[0])
            .args(["--experimental-builtin-modules", &registry])
            .args(&argv[1..])
            .current_dir(self.path())
            .seeded_archives()
            .explicit_modules()
            .output()
            .unwrap();
        assert_ne!(output.status.code(), Some(124), "CLI timed out: {argv:?}");
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
            // Keeps the mode (pkg/tool.sh is executable).
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

/// A tree's followed view, like the capture's: every regular file reached
/// following symlinks as [path, x or -, size, sha256 prefix] (a file equal
/// to the host's /bin/sh as host/host-bin-sh), sorted.
fn followed(tree: &Path) -> Value {
    let host = fs::read("/bin/sh").ok();
    let mut found = Vec::new();
    let mut pending = vec![PathBuf::new()];
    while let Some(relative) = pending.pop() {
        for entry in fs::read_dir(tree.join(&relative)).unwrap() {
            let child = relative.join(entry.unwrap().file_name());
            let path = tree.join(&child);
            let metadata = fs::metadata(&path).unwrap();
            if metadata.is_dir() {
                pending.push(child);
                continue;
            }
            let bytes = fs::read(&path).unwrap();
            let executable = if metadata.permissions().mode() & 0o100 != 0 {
                "x"
            } else {
                "-"
            };
            let (size, digest) = if child.file_name().is_some_and(|name| name == "sh")
                && host.as_ref() == Some(&bytes)
            {
                ("host".to_owned(), "host-bin-sh".to_owned())
            } else {
                let digest = format!("{:x}", Sha256::digest(&bytes));
                (bytes.len().to_string(), digest[..16].to_owned())
            };
            found.push(serde_json::json!([
                child.display().to_string(),
                executable,
                size,
                digest
            ]));
        }
    }
    found.sort_by(|a: &Value, b: &Value| a[0].as_str().cmp(&b[0].as_str()));
    serde_json::json!({ "followed": found })
}

/// The compared top-level outputs of the native `LABEL\tPATH` result lines
/// (path below the action's outputs/ -> value): a listing (`.txt`) as
/// [mode, contents], a tree as its followed view; `.raw` listings are not
/// compared (receipt `comparison`).
fn outputs(stdout: &str) -> BTreeMap<String, Value> {
    stdout
        .lines()
        .filter_map(|line| line.split_once('\t'))
        .filter_map(|(_, path)| {
            let relative = path.split_once("/outputs/").map_or(path, |(_, rest)| rest);
            let value = if Path::new(path).is_dir() {
                followed(Path::new(path))
            } else if relative.ends_with(".txt") {
                let mode = fs::metadata(path).unwrap().permissions().mode() & 0o777;
                serde_json::json!([format!("0o{mode:o}"), fs::read_to_string(path).unwrap()])
            } else {
                return None;
            };
            Some((relative.to_owned(), value))
        })
        .collect()
}

/// Test times as `<T>`, like the receipt's.
fn times(text: &str) -> String {
    text.lines()
        .map(|line| match line.rsplit_once(" in ") {
            Some((head, time))
                if time
                    .strip_suffix('s')
                    .is_some_and(|seconds| seconds.parse::<f64>().is_ok()) =>
            {
                format!("{head} in <T>s\n")
            }
            _ => format!("{line}\n"),
        })
        .collect()
}

/// Parallel tests finish in either order in both engines. Keep each complete
/// log block and the ordered final summary exact, but not the arrival order of
/// the blocks (which the genuine capture only observed once).
fn test_output(mut text: &str) -> (Vec<&str>, &str) {
    const END: &str =
        "\n================================================================================\n";
    let mut blocks = Vec::new();
    while text.starts_with("==================== Test output for ") {
        let end = text.find(END).expect("unterminated test output block") + END.len();
        blocks.push(&text[..end]);
        text = &text[end..];
    }
    blocks.sort_unstable();
    (blocks, text)
}

#[test]
fn test_output_comparison_preserves_contents_counts_and_summary() {
    let first = "==================== Test output for //:first:\nfirst log\n================================================================================\n";
    let second = "==================== Test output for //:second:\nsecond log\n================================================================================\n";
    let forward = format!("{first}{second}summary\n");
    let reverse = format!("{second}{first}summary\n");
    assert_eq!(test_output(&forward), test_output(&reverse));
    for changed in [
        format!("{first}summary\n"),
        format!("{first}{second}{second}summary\n"),
        forward.replace("first log", "different log"),
        forward.replace("summary", "different summary"),
    ] {
        assert_ne!(test_output(&forward), test_output(&changed));
    }
}

fn argv(case: &Value) -> Vec<String> {
    case["argv"]
        .as_array()
        .unwrap()
        .iter()
        .map(|arg| arg.as_str().unwrap().to_owned())
        .collect()
}

fn check(name: &str, case: &Value) {
    let argv = argv(case);
    let ws = Workspace::new();
    let output = ws.run(&argv);
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    assert!(!stderr.contains("panicked"), "{name}: {stderr}");
    if case["verdict"] == "DECLARED_DIVERGENCE" {
        assert!(!output.status.success(), "{name}: {stderr}");
        let message = case["native_message"].as_str().unwrap();
        assert!(
            stderr.contains(message),
            "{name}: missing {message:?} in {stderr}"
        );
        return;
    }
    assert_eq!(case["verdict"], "MATCH", "{name}");
    let succeeded = case["genuine"]["exit"].as_i64() == Some(0);
    assert_eq!(
        output.status.success(),
        succeeded,
        "{name} {argv:?}: {stderr}"
    );
    if !succeeded {
        let parts: Vec<&str> = match case.get("expected_message_parts") {
            Some(parts) => parts
                .as_array()
                .unwrap()
                .iter()
                .map(|part| part.as_str().unwrap())
                .collect(),
            None => vec![case["expected_message"].as_str().unwrap()],
        };
        for part in parts {
            assert!(
                stderr.contains(part),
                "{name}: missing {part:?} in {stderr}"
            );
        }
        return;
    }
    match argv[0].as_str() {
        "build" => {
            let expected: BTreeMap<String, Value> = case["expected_outputs"]
                .as_object()
                .unwrap()
                .iter()
                .map(|(path, value)| (path.clone(), value.clone()))
                .collect();
            assert_eq!(outputs(&stdout), expected, "{name}: {stderr}");
        }
        "run" => assert_eq!(
            stdout,
            case["expected_stdout"].as_str().unwrap(),
            "{name}: {stderr}"
        ),
        "test" => assert_eq!(
            test_output(&times(&stdout)),
            test_output(case["expected_stdout"].as_str().unwrap()),
            "{name}: {stderr}"
        ),
        command => panic!("{name}: unexpected command {command}"),
    }
}

fn check_group(group: &str) {
    let receipt = receipt();
    let (_, names) = GROUPS.iter().find(|(name, _)| *name == group).unwrap();
    for name in *names {
        let case = &receipt["cases"][name];
        assert!(case.is_object(), "{name} is not in the receipt");
        check(name, case);
    }
}

#[test]
fn receipt_covers_the_fixture_and_every_case_once() {
    let receipt = receipt();
    let mut files = BTreeMap::new();
    let mut stack = vec![fixture()];
    while let Some(directory) = stack.pop() {
        for entry in fs::read_dir(&directory).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                stack.push(path);
            } else {
                let relative = path.strip_prefix(fixture()).unwrap();
                let digest = Sha256::digest(fs::read(&path).unwrap());
                files.insert(
                    relative.display().to_string(),
                    Value::String(format!("{digest:x}")),
                );
            }
        }
    }
    let recorded = receipt["fixture_files_sha256"].as_object().unwrap();
    assert_eq!(
        files,
        recorded
            .iter()
            .map(|(path, digest)| (path.clone(), digest.clone()))
            .collect::<BTreeMap<_, _>>()
    );
    let mut grouped = GROUPS
        .iter()
        .flat_map(|(_, names)| names.iter().copied())
        .collect::<Vec<_>>();
    grouped.sort_unstable();
    let mut cases = receipt["cases"]
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect::<Vec<_>>();
    cases.sort_unstable();
    assert_eq!(grouped, cases);
    let declared = GROUPS
        .iter()
        .find(|(group, _)| *group == "declared")
        .unwrap()
        .1;
    for (name, case) in receipt["cases"].as_object().unwrap() {
        let expected = if declared.contains(&name.as_str()) {
            "DECLARED_DIVERGENCE"
        } else {
            "MATCH"
        };
        assert_eq!(case["verdict"], expected, "{name}");
    }
}

/// Links that resolve: the tree's children and contents as genuine's, for
/// a sandboxed and an unsandboxed consumer and a symlink of the whole tree.
#[test]
fn links_inside_trees_resolve_like_bazel() {
    check_group("resolved");
}

/// Dangling links (also an absolute one into the action's sandbox), a link
/// to a device, a loop and directory cycles fail with Bazel's validation
/// errors.
#[test]
fn invalid_links_fail_like_bazel() {
    check_group("invalid");
}

/// A tree with links read through a `run` program's runfiles tree and in a
/// test's runfiles.
#[test]
fn trees_with_links_in_runfiles() {
    check_group("runfiles");
}

/// Explicit native errors with a recorded difference.
#[test]
fn declared_divergences_are_explicit_errors() {
    check_group("declared");
}

/// The summary's executed and cache-hit counts.
fn counts(stderr: &str) -> (usize, usize) {
    let line = stderr
        .lines()
        .find(|line| line.contains(" actions executed, "))
        .unwrap_or_else(|| panic!("no summary in {stderr}"));
    let words: Vec<&str> = line.split_whitespace().collect();
    let executed = words.iter().position(|word| *word == "actions").unwrap() - 1;
    let hits = words.iter().position(|word| *word == "cache").unwrap() - 1;
    (
        words[executed].parse().unwrap(),
        words[hits].parse().unwrap(),
    )
}

/// Native-only: a rebuild reuses the published trees with their resolved
/// files (cache hits, the same listings); a changed link target re-keys the
/// tree's producer and its consumers, which see the new content through
/// the link.
#[test]
fn trees_with_links_are_reused_and_rekeyed_by_their_targets() {
    let ws = Workspace::new();
    let argv: Vec<String> = ["build", "//pkg:oci_from", "//pkg:inspect_oci_from"]
        .map(str::to_owned)
        .to_vec();
    let first = ws.run(&argv);
    let stderr = String::from_utf8_lossy(&first.stderr).into_owned();
    assert!(first.status.success(), "{stderr}");
    let listing = outputs(&String::from_utf8_lossy(&first.stdout));
    let again = ws.run(&argv);
    let stderr = String::from_utf8_lossy(&again.stderr).into_owned();
    assert!(again.status.success(), "{stderr}");
    assert_eq!(counts(&stderr).0, 0, "{stderr}");
    assert_eq!(outputs(&String::from_utf8_lossy(&again.stdout)), listing);
    let build = ws.path().join("pkg/BUILD.bazel");
    let text = fs::read_to_string(&build).unwrap();
    fs::write(&build, text.replace("echo layer-bytes", "echo other-bytes")).unwrap();
    let changed = ws.run(&argv);
    let stderr = String::from_utf8_lossy(&changed.stderr).into_owned();
    assert!(changed.status.success(), "{stderr}");
    // The layer, the image linking it and its consumer.
    assert_eq!(counts(&stderr).0, 3, "{stderr}");
    let outputs = outputs(&String::from_utf8_lossy(&changed.stdout));
    let inspected = outputs["bazel-out/k8-fastbuild/bin/pkg/inspect_oci_from.txt"][1]
        .as_str()
        .unwrap();
    let digest = format!("{:x}", Sha256::digest(b"other-bytes\n"));
    assert!(
        inspected.contains(&format!("  blobs/sha256/layer x 12 {}\n", &digest[..16])),
        "{inspected}"
    );
}
