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
include!("support/explicit_modules.rs");

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
                .tempdir_in(crate::test_temp::root())
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
        self.query_stderr(cache, extra);
    }

    /// [`Self::query`], returning its stderr.
    fn query_stderr(&self, cache: &Path, extra: &[&str]) -> String {
        let (code, stderr) = self.query_output(cache, extra);
        assert_eq!(code, Some(0), "{stderr}");
        stderr
    }

    /// The query, successful or not: its exit code and stderr.
    fn query_output(&self, cache: &Path, extra: &[&str]) -> (Option<i32>, String) {
        let output = Command::new("timeout")
            .args([
                "600s",
                env!("CARGO_BIN_EXE_bazel-rs"),
                "--ignore_all_rc_files",
            ])
            .arg(format!("--cache-dir={}", cache.display()))
            .arg("query")
            .args(extra)
            .arg("@reader//:all + @from_ext//:all")
            .current_dir(self.path())
            .explicit_modules()
            .output()
            .unwrap();
        (
            output.status.code(),
            String::from_utf8_lossy(&output.stderr).into_owned(),
        )
    }

    fn change_data_repository(&self) {
        self.edit("data.bzl", "hello {NAME}", "bye {NAME}");
    }

    /// Change data/file.txt, which the module extension reads.
    fn change_data_file(&self) {
        self.edit(
            "data.bzl",
            r#"ctx.file("file.txt", "data\n")"#,
            r#"ctx.file("file.txt", "changed\n")"#,
        );
    }

    /// Make every evaluation of the module extension print a marker.
    fn announce_extension_evaluations(&self) {
        self.edit(
            "defs.bzl",
            "def _ext(module_ctx):\n",
            "def _ext(module_ctx):\n    print(\"EXTENSION EVALUATED\")\n",
        );
    }

    /// Declare the module extension reproducible: its lockfile entry then
    /// lives in the hidden lockfile ([`hidden_lockfile`]).
    fn make_extension_reproducible(&self) {
        self.edit(
            "defs.bzl",
            "    from_ext(name = \"from_ext\")\n",
            "    from_ext(name = \"from_ext\")\n    return module_ctx.extension_metadata(reproducible = True)\n",
        );
    }

    /// Make the module extension also watch the directory of the store's
    /// content-addressed trees (the parent of @data's root): a store path
    /// that no repository root contains.
    fn watch_store_directory_from_extension(&self) {
        self.edit(
            "defs.bzl",
            "    _observe(module_ctx)\n",
            "    module_ctx.watch(_observe(module_ctx).dirname)\n",
        );
    }

    fn edit(&self, name: &str, from: &str, to: &str) {
        let path = self.path().join(name);
        let text = fs::read_to_string(&path).unwrap();
        assert!(text.contains(from), "{name}: {from}");
        fs::write(path, text.replace(from, to)).unwrap();
    }
}

/// How many evaluations of the module extension a query announced
/// ([`Workspace::announce_extension_evaluations`]).
fn evaluations(stderr: &str) -> usize {
    stderr.matches("EXTENSION EVALUATED").count()
}

/// What bazel-rs before review i20 recorded in place of the module
/// extension's lockfile `inputs` with the cache inside the workspace: every
/// key in the data repository a path into its store tree `data` — except
/// the label read's, keyed by the label's path already.
fn pre_i20_keys(inputs: &Value, data: &str) -> Value {
    let old = format!("@@//bazel-rs-out/repositories/direct/trees/{data}/");
    inputs
        .as_array()
        .unwrap()
        .iter()
        .map(|input| {
            let text = input.as_str().unwrap();
            if text.contains("//label.txt ") {
                text.to_owned()
            } else {
                text.replace("@@+data_repo+data//", &old)
            }
        })
        .collect::<Vec<_>>()
        .into()
}

#[test]
fn the_seeded_store_keys_are_what_the_pre_i20_binary_recorded() {
    // `store_keys_in_the_lockfile_re_evaluate_the_extension_once` seeds
    // `pre_i20_keys`: exactly the lockfile the pre-i20 binary wrote with the
    // cache inside the workspace (reference_receipt.json, native.before) —
    // which, unlike with the cache outside it, was not genuine's.
    let receipt = receipt();
    let before = &receipt["native"]["before"];
    let recorded = &before["lockfile_recorded_inputs_cache_inside_workspace"];
    let genuine = &receipt["genuine"]["cold"]["lockfile_recorded_inputs"];
    let data = recorded[1].as_str().unwrap()
        ["FILE:@@//bazel-rs-out/repositories/direct/trees/".len()..]
        .split_once('/')
        .unwrap()
        .0;
    assert_eq!(&pre_i20_keys(genuine, data), recorded);
    assert_ne!(recorded, genuine);
    assert_eq!(
        before["lockfile_recorded_inputs_equal_genuine"],
        serde_json::json!({"cache_outside_workspace": true, "cache_inside_workspace": false})
    );
}

/// The module extension's recorded inputs in the workspace lockfile.
fn lockfile_inputs(ws: &Workspace) -> Value {
    recorded_inputs(&ws.path().join("MODULE.bazel.lock"))
}

/// The module extension's recorded inputs in lockfile `path`.
fn recorded_inputs(path: &Path) -> Value {
    let lock: Value = serde_json::from_slice(&fs::read(path).unwrap()).unwrap();
    lock["moduleExtensions"]["//:defs.bzl%ext"]["general"]["recordedInputs"].clone()
}

/// The hidden lockfile of the one workspace that used `cache`
/// (`<cache>/worktrees/<name>-<hash>/MODULE.bazel.lock`, docs/WORKTREES.md).
fn hidden_lockfile(cache: &Path) -> PathBuf {
    let [workspace] = <[String; 1]>::try_from(listing(&cache.join("worktrees"))).unwrap();
    cache
        .join("worktrees")
        .join(workspace)
        .join("MODULE.bazel.lock")
}

/// The store tree of @data: the one tree with a `tpl.txt`.
fn data_tree(cache: &Path) -> String {
    let trees = store(cache).join("trees");
    let found: Vec<String> = listing(&trees)
        .into_iter()
        .filter(|identity| trees.join(identity).join("tpl.txt").exists())
        .collect();
    let [data] = <[String; 1]>::try_from(found).unwrap();
    data
}

/// Rewrite the module extension's entry in lockfile `path` to the keys the
/// pre-i20 binary recorded with the cache inside the workspace
/// ([`pre_i20_keys`]; 4 of them name @data's store tree, returned).
fn seed_pre_i20_keys(path: &Path, cache: &Path) -> String {
    let data = data_tree(cache);
    let mut lock: Value = serde_json::from_slice(&fs::read(path).unwrap()).unwrap();
    let inputs = &mut lock["moduleExtensions"]["//:defs.bzl%ext"]["general"]["recordedInputs"];
    *inputs = pre_i20_keys(inputs, &data);
    assert_eq!(
        inputs
            .as_array()
            .unwrap()
            .iter()
            .filter(|input| input.as_str().unwrap().contains("bazel-rs-out"))
            .count(),
        4
    );
    fs::write(path, serde_json::to_vec(&lock).unwrap()).unwrap();
    data
}

/// The outdated-input reason of a seeded pre-i20 store key.
const STORE_KEY_REASON: &str =
    "is a path into the native repository store (recorded by an older bazel-rs)";

#[test]
fn store_keys_in_the_lockfile_re_evaluate_the_extension_once() {
    // Before review i20 a module extension's read of another repository's
    // file was keyed in MODULE.bazel.lock as a path into that repository's
    // content-addressed store tree: with the default cache inside the
    // workspace `@@//bazel-rs-out/repositories/direct/trees/<id>/...`
    // (reference_receipt.json, native.before, cache inside the workspace;
    // ~/code's hidden lockfile keeps 11 such keys). Store trees are never
    // rewritten, so those keys kept validating after the repository was
    // fetched anew and the extension stayed stale. Such a key is outdated:
    // exactly one re-evaluation records `@@NAME//path` keys with the new
    // values, and nothing re-evaluates after that.
    let ws = Workspace::new();
    ws.announce_extension_evaluations();
    let cache = ws.path().join("bazel-rs-out");
    assert_eq!(evaluations(&ws.query_stderr(&cache, &[])), 1);
    let lock_path = ws.path().join("MODULE.bazel.lock");
    let data = seed_pre_i20_keys(&lock_path, &cache);

    // The data repository changes a file the extension read: re-fetched
    // into a new tree, while the old tree the seeded keys name is intact.
    ws.change_data_file();

    // --lockfile_mode=error writes no lockfile, so it never migrates the
    // keys: the outdated entry of this non-reproducible extension fails the
    // command (docs/REPOSITORY_RULES.md, upgrade note).
    let seeded = fs::read_to_string(&lock_path).unwrap();
    let (code, stderr) = ws.query_output(&cache, &["--lockfile_mode=error"]);
    assert_eq!(code, Some(1), "{stderr}");
    assert!(
        stderr.contains(&format!(
            "MODULE.bazel.lock is no longer up-to-date because an input to the extension '@@//:defs.bzl%ext' changed: @@//bazel-rs-out/repositories/direct/trees/{data}/file.txt {STORE_KEY_REASON}. Please run `bazel mod deps --lockfile_mode=update` to update your lockfile."
        )),
        "{stderr}"
    );
    assert_eq!(evaluations(&stderr), 0, "{stderr}");
    assert_eq!(fs::read_to_string(&lock_path).unwrap(), seeded);

    // Update mode (the default) re-evaluates once.
    let stderr = ws.query_stderr(&cache, &[]);
    assert_eq!(evaluations(&stderr), 1, "{stderr}");
    let settled = fs::read_to_string(&lock_path).unwrap();
    assert!(!settled.contains("bazel-rs-out"), "{settled}");
    let inputs = lockfile_inputs(&ws);
    // sha256("changed\n")
    let changed = Value::from(
        "FILE:@@+data_repo+data//file.txt 7f8b1dfc466b6249f06cbe55c9174df2578e7754da793fded244ef5cba2a38f1",
    );
    assert!(inputs.as_array().unwrap().contains(&changed), "{inputs}");

    // Stable: no further evaluation, the lockfile is left as it is.
    let stderr = ws.query_stderr(&cache, &[]);
    assert_eq!(evaluations(&stderr), 0, "{stderr}");
    assert_eq!(fs::read_to_string(&lock_path).unwrap(), settled);
}

#[test]
fn store_keys_in_the_hidden_lockfile_re_evaluate_on_every_error_mode_run() {
    // A reproducible extension's entry lives in the hidden lockfile, where
    // ~/code keeps its 11 store keys (go_deps, go_sdk, pip) and where the
    // pre-i20 binary wrote the seeded keys' spelling for this extension made
    // reproducible (scratch capture
    // /tmp/opencode/perf-research/i20h-followup-claims.py, E2). Error mode
    // writes no lockfile, hidden or not (as in Bazel,
    // BazelLockFileModule.ENABLED_IN_MODES), so the outdated store keys
    // re-evaluate the extension on every run — without failing it, the
    // entry being reproducible — until one update-mode run records
    // `@@NAME//path` keys (docs/REPOSITORY_RULES.md, upgrade note).
    let ws = Workspace::new();
    ws.announce_extension_evaluations();
    ws.make_extension_reproducible();
    let cache = ws.path().join("bazel-rs-out");
    assert_eq!(evaluations(&ws.query_stderr(&cache, &[])), 1);
    let workspace_lockfile =
        fs::read_to_string(ws.path().join("MODULE.bazel.lock")).unwrap_or_default();
    assert!(
        !workspace_lockfile.contains("defs.bzl%ext"),
        "{workspace_lockfile}"
    );
    let hidden = hidden_lockfile(&cache);
    seed_pre_i20_keys(&hidden, &cache);
    let seeded = fs::read_to_string(&hidden).unwrap();
    for _ in 0..2 {
        let stderr = ws.query_stderr(&cache, &["--lockfile_mode=error"]);
        assert_eq!(evaluations(&stderr), 1, "{stderr}");
        assert_eq!(fs::read_to_string(&hidden).unwrap(), seeded);
    }

    let stderr = ws.query_stderr(&cache, &[]);
    assert_eq!(evaluations(&stderr), 1, "{stderr}");
    assert_eq!(
        recorded_inputs(&hidden),
        receipt()["genuine"]["cold"]["lockfile_recorded_inputs"]
    );
    let stderr = ws.query_stderr(&cache, &["--lockfile_mode=error"]);
    assert_eq!(evaluations(&stderr), 0, "{stderr}");
}

#[test]
fn a_watched_store_path_in_no_repository_fails_closed() {
    // A store path that no repository root contains (here the directory of
    // the store's trees) is spelled absolute, never `@@//<cache>/...`, so it
    // has no lockfile key: a module extension that watches it cannot write
    // its lockfile entry — with the cache inside the workspace (the default
    // layout) exactly as with the cache outside it, where this always
    // failed. Before review i20 the inside layout silently recorded
    // `FILE:@@//bazel-rs-out/repositories/direct/trees DIR`, a key the
    // never-rewritten store kept valid forever (docs/EXTENSION_CTX_SURFACE.md).
    for inside in [false, true] {
        let ws = Workspace::new();
        ws.watch_store_directory_from_extension();
        let outside = outside_cache();
        let cache = if inside {
            ws.path().join("bazel-rs-out")
        } else {
            outside.path().join("cache")
        };
        let (code, stderr) = ws.query_output(&cache, &[]);
        assert_eq!(
            code,
            Some(1),
            "cache inside the workspace: {inside}: {stderr}"
        );
        let trees = fs::canonicalize(store(&cache).join("trees")).unwrap();
        assert!(
            stderr.contains(&format!(
                "module extension @@//:defs.bzl%ext: cannot write its MODULE.bazel.lock entry: the module extension watched {}, which is in no known repository (use --lockfile_mode=off)",
                trees.display()
            )),
            "cache inside the workspace: {inside}: {stderr}"
        );
        let lock = fs::read_to_string(ws.path().join("MODULE.bazel.lock")).unwrap_or_default();
        assert!(!lock.contains("repositories/direct/trees"), "{lock}");
        // Without a lockfile entry to write, the extension evaluates.
        ws.query(&cache, &["--lockfile_mode=off"]);
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
        .tempdir_in(crate::test_temp::root())
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
    // Both cache layouts: outside every workspace, and inside the workspace
    // (`<workspace>/bazel-rs-out`, the default layout and ~/code's), where
    // the store also lies under the main repository's root — the layout in
    // which the pre-i20 lockfile keys were not genuine's (receipt).
    let receipt = receipt();
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
    for inside in [false, true] {
        let ws = Workspace::new();
        let outside = outside_cache();
        let cache = if inside {
            ws.path().join("bazel-rs-out")
        } else {
            outside.path().join("cache")
        };
        ws.query(&cache, &[]);
        let (_, recorded) = reader(&cache);
        assert_eq!(recorded["version"], 2);
        let native: Vec<&str> = recorded["files"]
            .as_array()
            .unwrap()
            .iter()
            .map(|file| file["path"].as_str().unwrap())
            .collect();
        assert_eq!(native, genuine, "cache inside the workspace: {inside}");
        assert_eq!(
            recorded["templates"][0]["source"].as_str(),
            Some("@@+data_repo+data//tpl.txt")
        );
        // The module extension's inputs in MODULE.bazel.lock equal genuine's.
        assert_eq!(
            lockfile_inputs(&ws),
            receipt["genuine"]["cold"]["lockfile_recorded_inputs"],
            "cache inside the workspace: {inside}"
        );
    }
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
