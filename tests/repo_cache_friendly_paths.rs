//! Recorded-input paths are RepoCacheFriendlyPath keys (review i20 item 4):
//! what a repository rule or module extension reads or watches in another
//! repository is recorded as `@@NAME//path` (RepoRecordedInput.java:325-405,
//! formed by toRepoCacheFriendlyPath, StarlarkBaseExternalContext.java:
//! 1656-1704), never as a host path into the native repository store. The
//! genuine 9.3.0 keys, lockfile inputs and re-fetch behaviour are recorded in
//! tests/fixtures/repo_cache_friendly_paths/reference_receipt.json (scratch
//! capture with `--batch --ignore_all_rc_files` and a private output root).
#![allow(clippy::disallowed_methods)] // test harness: starts processes directly (clippy.toml)

use serde_json::Value;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

fn manifest_dir() -> &'static Path {
    Path::new(env!("CARGO_MANIFEST_DIR"))
}

fn receipt() -> Value {
    serde_json::from_slice(
        &fs::read(
            manifest_dir().join("tests/fixtures/repo_cache_friendly_paths/reference_receipt.json"),
        )
        .unwrap(),
    )
    .unwrap()
}

struct Workspace(tempfile::TempDir);

impl Workspace {
    fn new() -> Self {
        let ws = Self(
            tempfile::Builder::new()
                .prefix("repo-cache-friendly-")
                .tempdir_in("/tmp/opencode")
                .unwrap(),
        );
        let source = manifest_dir().join("tests/fixtures/repo_cache_friendly_paths/workspace");
        for entry in fs::read_dir(source).unwrap() {
            let entry = entry.unwrap();
            fs::copy(entry.path(), ws.path().join(entry.file_name())).unwrap();
        }
        ws
    }

    fn path(&self) -> &Path {
        self.0.path()
    }

    /// Fetch @reader and the extension's repository into `cache`.
    fn query(&self, cache: &Path, extra: &[&str]) {
        let output = Command::new("timeout")
            .args([
                "120s",
                env!("CARGO_BIN_EXE_bazel-rs"),
                "--ignore_all_rc_files",
            ])
            .arg(format!("--cache-dir={}", cache.display()))
            .arg("query")
            .args(extra)
            .arg("@reader//:all + @from_ext//:all")
            .current_dir(self.path())
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    fn change_data_repository(&self) {
        let path = self.path().join("data.bzl");
        let text = fs::read_to_string(&path).unwrap();
        fs::write(path, text.replace("hello {NAME}", "bye {NAME}")).unwrap();
    }
}

#[test]
fn version_1_store_probes_are_never_trusted() {
    // A publication whose probes an older binary recorded (version 1, paths
    // into another repository as host paths into its store tree) is
    // re-executed once and upgraded — not reused stale after the repository
    // it read changed, and not re-executed forever.
    let ws = Workspace::new();
    let cache = ws.path().join("bazel-rs-out");
    ws.query(&cache, &[]);
    let (identity, recorded) = reader(&cache);
    let data_tree = listing(&store(&cache).join("trees"))
        .into_iter()
        .map(|identity| store(&cache).join("trees").join(identity))
        .find(|tree| tree.join("tpl.txt").exists())
        .unwrap();
    let mut legacy = recorded.clone();
    legacy["version"] = 1.into();
    let host_path = |text: &str| -> String {
        match text.strip_prefix("@@+data_repo+data//") {
            Some(relative) => data_tree.join(relative).to_str().unwrap().to_owned(),
            None => text.to_owned(),
        }
    };
    for probe in legacy["files"].as_array_mut().unwrap() {
        probe["path"] = host_path(probe["path"].as_str().unwrap()).into();
    }
    for probe in legacy["templates"].as_array_mut().unwrap() {
        probe["source"] = host_path(probe["source"].as_str().unwrap()).into();
    }
    let manifest_path = store(&cache)
        .join("manifests")
        .join(format!("{identity}.json"));
    let mut manifest: Value = serde_json::from_slice(&fs::read(&manifest_path).unwrap()).unwrap();
    let bytes = serde_json::to_vec(&legacy).unwrap();
    manifest["recorded"] = bytes
        .into_iter()
        .map(Value::from)
        .collect::<Vec<_>>()
        .into();
    fs::write(&manifest_path, serde_json::to_vec(&manifest).unwrap()).unwrap();

    // Unchanged world: executed once more, then upgraded and reused.
    ws.query(&cache, &[]);
    let (again, upgraded) = reader(&cache);
    assert_eq!(again, identity);
    assert_eq!(upgraded, recorded);
    let trees = listing(&store(&cache).join("trees"));
    let settled = fs::read(&manifest_path).unwrap();
    ws.query(&cache, &[]);
    assert_eq!(listing(&store(&cache).join("trees")), trees);
    assert_eq!(fs::read(&manifest_path).unwrap(), settled);

    // Version-1 probes again, then the data repository changes: the probe
    // into its old tree would still match; the reader is re-fetched anyway.
    let mut manifest: Value = serde_json::from_slice(&settled).unwrap();
    let bytes = serde_json::to_vec(&legacy).unwrap();
    manifest["recorded"] = bytes
        .into_iter()
        .map(Value::from)
        .collect::<Vec<_>>()
        .into();
    fs::write(&manifest_path, serde_json::to_vec(&manifest).unwrap()).unwrap();
    ws.change_data_repository();
    ws.query(&cache, &[]);
    let (after, _) = reader(&cache);
    assert_eq!(
        fs::read_to_string(store(&cache).join("trees").join(after).join("out.txt")).unwrap(),
        "bye x\n"
    );
}

/// A directory outside every workspace for a cache.
fn outside_cache() -> tempfile::TempDir {
    tempfile::Builder::new()
        .prefix("repo-cache-friendly-cache-")
        .tempdir_in("/tmp/opencode")
        .unwrap()
}

/// `<cache>/repositories/direct`.
fn store(cache: &Path) -> PathBuf {
    cache.join("repositories/direct")
}

fn listing(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = fs::read_dir(dir)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().into_string().unwrap())
        .collect();
    names.sort();
    names
}

/// The current publication of @reader (the one repository with a template
/// probe) among those the specification pointers name: (identity, recorded
/// inputs).
fn reader(cache: &Path) -> (String, Value) {
    let direct = store(cache);
    let mut found = None;
    for spec in listing(&direct.join("specs")) {
        let pointer: Value =
            serde_json::from_slice(&fs::read(direct.join("specs").join(spec)).unwrap()).unwrap();
        let identity = pointer["identity"].as_str().unwrap().to_owned();
        let manifest: Value = serde_json::from_slice(
            &fs::read(direct.join("manifests").join(format!("{identity}.json"))).unwrap(),
        )
        .unwrap();
        let bytes: Vec<u8> = manifest["recorded"]
            .as_array()
            .map(|bytes| bytes.iter().map(|b| b.as_u64().unwrap() as u8).collect())
            .unwrap_or_default();
        if bytes.is_empty() {
            continue;
        }
        let recorded: Value = serde_json::from_slice(&bytes).unwrap();
        if recorded["templates"]
            .as_array()
            .is_some_and(|t| !t.is_empty())
        {
            assert!(found.is_none(), "one @reader publication is pointed to");
            found = Some((identity, recorded));
        }
    }
    found.expect("@reader publication")
}

#[test]
fn recorded_input_paths_are_genuine_repo_cache_friendly_keys() {
    let receipt = receipt();
    let ws = Workspace::new();
    let outside = outside_cache();
    let cache = outside.path().join("cache");
    ws.query(&cache, &[]);
    let (_, recorded) = reader(&cache);
    assert_eq!(recorded["version"], 2);
    // The genuine marker keys of @@+reader+reader, in recording order (the
    // native probes do not model REPO_MAPPING lines).
    let genuine: Vec<&str> = receipt["genuine"]["cold"]["marker_lines"]
        .as_array()
        .unwrap()
        .iter()
        .map(|line| line.as_str().unwrap())
        .filter(|line| !line.starts_with("REPO_MAPPING:"))
        .map(|line| {
            let key = line.split_once(' ').unwrap().0;
            key.split_once(':').unwrap().1
        })
        .collect();
    let native: Vec<&str> = recorded["files"]
        .as_array()
        .unwrap()
        .iter()
        .map(|file| file["path"].as_str().unwrap())
        .collect();
    assert_eq!(native, genuine);
    assert_eq!(
        recorded["templates"][0]["source"].as_str(),
        Some("@@+data_repo+data//tpl.txt")
    );
    // The module extension's inputs in MODULE.bazel.lock equal genuine's.
    let lock: Value =
        serde_json::from_slice(&fs::read(ws.path().join("MODULE.bazel.lock")).unwrap()).unwrap();
    assert_eq!(
        lock["moduleExtensions"]["//:defs.bzl%ext"]["general"]["recordedInputs"],
        receipt["genuine"]["cold"]["lockfile_recorded_inputs"]
    );
}

#[test]
fn repository_identities_do_not_depend_on_the_cache_location() {
    // --lockfile_mode=off (the extension's recorded inputs then enter the
    // specifications of the repositories it generates): a cache inside the
    // workspace, the default layout, and one outside it publish the same
    // specifications and content identities.
    let inside = Workspace::new();
    let inside_cache = inside.path().join("bazel-rs-out");
    inside.query(&inside_cache, &["--lockfile_mode=off"]);
    let outside = Workspace::new();
    let directory = outside_cache();
    let outside_cache = directory.path().join("cache");
    outside.query(&outside_cache, &["--lockfile_mode=off"]);
    for listed in ["specs", "trees"] {
        assert_eq!(
            listing(&store(&inside_cache).join(listed)),
            listing(&store(&outside_cache).join(listed)),
            "{listed}"
        );
    }
}

#[test]
fn a_reader_is_refetched_when_the_repository_it_read_changes() {
    // Genuine re-fetches @reader once its FILE:@@+data_repo+data//tpl.txt
    // input changes. Host-path probes into the data repository's old
    // content-addressed tree used to keep validating: the stale `hello x`
    // was reused.
    let receipt = receipt();
    let ws = Workspace::new();
    let cache = ws.path().join("bazel-rs-out");
    ws.query(&cache, &[]);
    let (before, _) = reader(&cache);
    assert_eq!(
        fs::read_to_string(store(&cache).join("trees").join(&before).join("out.txt")).unwrap(),
        receipt["genuine"]["cold"]["out_txt"].as_str().unwrap()
    );
    ws.change_data_repository();
    ws.query(&cache, &[]);
    let (after, _) = reader(&cache);
    assert_ne!(before, after);
    assert_eq!(
        fs::read_to_string(store(&cache).join("trees").join(&after).join("out.txt")).unwrap(),
        receipt["genuine"]["after_data_change"]["out_txt"]
            .as_str()
            .unwrap()
    );
}
