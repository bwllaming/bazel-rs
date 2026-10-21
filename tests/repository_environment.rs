//! Real child-process coverage of the bounded repository environment profile.
//! Reference evidence lives in the ignored repository-environ-{,name-}contract
//! receipts; these tests deliberately do not depend on those artifact files.
//! Fresh native CLIs execute callbacks even when a publication is reused.
#![allow(clippy::disallowed_methods)] // test harness: starts processes directly (clippy.toml)
use bazel_rs::label::Label;
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::ffi::OsString;
use std::fs;
use std::os::unix::ffi::OsStringExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

const A: &str = "BAZEL_RS_ENV_A";
const B: &str = "BAZEL_RS_ENV_B";
const UNUSED: &str = "BAZEL_RS_ENV_UNUSED";
const SECRET: &str = "SYNTHETIC_SECRET_42";
const DECLARED: &str = ", environ=['BAZEL_RS_ENV_A']";
const GET: &str = "ctx.getenv('BAZEL_RS_ENV_A', 'missing')";

#[derive(Clone, Copy)]
enum Route {
    Direct,
    Extension,
}
const ROUTES: [Route; 2] = [Route::Direct, Route::Extension];

struct Fixture(tempfile::TempDir, Route);
impl Fixture {
    fn new(route: Route, body: &str, value: &str, options: &str) -> Self {
        let f = Self(
            tempfile::Builder::new()
                .prefix("repository-environment-cli-")
                .tempdir_in("/tmp/opencode")
                .unwrap(),
            route,
        );
        match route {
            Route::Direct => f.file("MODULE.bazel", "module(name='main')\nmake=use_repo_rule('//:defs.bzl','make')\nmake(name='made')\n"),
            Route::Extension => {
                f.file("MODULE.bazel", "module(name='main')\ne=use_extension('//:ext.bzl','ext')\nuse_repo(e,'made')\n");
                f.file("ext.bzl", "load(':defs.bzl','make')\ndef _impl(ctx):\n    make(name='made')\next=module_extension(_impl)\n");
            }
        }
        f.file("BUILD.bazel", "genrule(name='copy',srcs=['@made//:data'],outs=['copy.txt'],cmd='/bin/cat $(SRCS) > $@')\n");
        f.definition(body, value, options);
        f
    }

    fn file(&self, name: &str, data: impl AsRef<[u8]>) {
        fs::write(self.0.path().join(name), data).unwrap();
    }

    fn definition(&self, body: &str, value: &str, options: &str) {
        self.file("defs.bzl", definition(body, value, options));
    }

    fn command(&self, args: &[&str], environment: &[(&str, &str)]) -> Command {
        let mut command = Command::new("/usr/bin/timeout");
        command
            .args(["--kill-after=5s", "60s", env!("CARGO_BIN_EXE_bazel-rs")])
            .args(["--ignore_all_rc_files", "--cache-dir=cache"])
            .args(args)
            .current_dir(self.0.path())
            .env_clear()
            .env("PATH", "/usr/bin:/bin");
        for key in [A, B, UNUSED, "?"] {
            command.env_remove(key);
        }
        for (key, value) in environment {
            // Beyond the fixture's synthetic names, only shell bookkeeping
            // variables that differ between working directories may pass.
            assert!(matches!(
                *key,
                A | B | UNUSED | "?" | "PWD" | "OLDPWD" | "SHLVL" | "_"
            ));
            command.env(key, value);
        }
        command
    }

    fn run(&self, args: &[&str], environment: &[(&str, &str)]) -> Output {
        finish(&mut self.command(args, environment))
    }

    fn build(&self, targets: &[&str], environment: &[(&str, &str)]) -> Built {
        let mut args = vec!["build"];
        args.extend_from_slice(targets);
        let expected: Vec<_> = targets
            .iter()
            .map(|target| match *target {
                "@made//:data" => self.made_label(),
                label => label,
            })
            .collect();
        Built::from_output(self.run(&args, environment), &expected)
    }

    fn made_label(&self) -> &'static str {
        match self.1 {
            Route::Direct => "@@+make+made//:data",
            Route::Extension => "@@+ext+made//:data",
        }
    }

    fn generated(&self, environment: &[(&str, &str)]) -> Built {
        let built = self.build(&["@made//:data"], environment);
        self.identity(built.path(self.made_label()));
        assert_eq!(built.paths.len(), 1);
        built
    }

    fn store(&self) -> PathBuf {
        self.0.path().join("cache/repositories/direct")
    }

    fn identity(&self, file: &Path) -> String {
        let tree = file.parent().unwrap();
        assert_eq!(tree.parent().unwrap(), self.store().join("trees"));
        assert!(!fs::symlink_metadata(tree).unwrap().file_type().is_symlink());
        let id = tree.file_name().unwrap().to_str().unwrap();
        assert_eq!(id.len(), 64);
        assert!(id.bytes().all(|b| b.is_ascii_hexdigit()));
        id.to_owned()
    }

    fn manifest(&self, file: &Path) -> Value {
        let id = self.identity(file);
        let path = self.store().join("manifests").join(format!("{id}.json"));
        let manifest: Value = serde_json::from_slice(&fs::read(path).unwrap()).unwrap();
        let mut keys: Vec<_> = manifest
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        keys.sort_unstable();
        // N61: every publication additionally carries the pre-execution
        // specification digest and the recorded watch probes.
        assert_eq!(
            keys,
            ["files", "format", "identity", "profile", "recorded", "spec"]
        );
        assert_eq!(manifest["format"], 1);
        assert_eq!(manifest["profile"], "direct-file-template-v1");
        assert_eq!(manifest["identity"], id);
        assert!(manifest["files"].as_object().unwrap().contains_key("data"));
        manifest
    }

    /// Manifest `files` with the Step-B `stat` fingerprints removed:
    /// comparisons of tree content across publications must not depend on
    /// the staged tree's inodes/timestamps.
    fn files_content(manifest: &Value) -> Value {
        let mut files = manifest["files"].clone();
        for entry in files.as_object_mut().unwrap().values_mut() {
            entry.as_object_mut().unwrap().remove("stat");
        }
        files
    }

    fn clean_failure(&self) {
        // Deterministic stage locks persist (empty files); trees, manifests
        // and stage directories stay empty after failures.
        for name in ["trees", "manifests", "stages"] {
            let path = self.store().join(name);
            assert!(
                !path.exists() || fs::read_dir(&path).unwrap().next().is_none(),
                "{name} not empty after failure"
            );
        }
        if let Ok(entries) = fs::read_dir(self.store().join("locks")) {
            for entry in entries {
                assert!(
                    entry
                        .unwrap()
                        .file_name()
                        .to_str()
                        .unwrap()
                        .starts_with("stage-")
                );
            }
        }
        assert!(!self.0.path().join("cache/actions").exists());
        self.no_stages();
    }

    fn no_stages(&self) {
        for name in ["trees", "manifests", "locks"] {
            if let Ok(entries) = fs::read_dir(self.store().join(name)) {
                for entry in entries {
                    assert!(
                        !entry
                            .unwrap()
                            .file_name()
                            .to_string_lossy()
                            .starts_with('.')
                    );
                }
            }
        }
    }
}

fn definition(body: &str, value: &str, options: &str) -> String {
    format!(
        "def _impl(ctx):\n    print('ENV_CALLBACK')\n{body}    ctx.file('BUILD.bazel', \"exports_files(['data'])\\n\")\n    ctx.file('data', {value}, executable=False)\nmake=repository_rule(_impl{options})\n"
    )
}

fn finish(command: &mut Command) -> Output {
    let output = command.output().unwrap();
    assert_ne!(output.status.code(), Some(124), "CLI timed out");
    assert_ne!(
        output.status.code(),
        Some(137),
        "CLI needed forced termination"
    );
    assert!(!errors(&output).contains("panicked"), "{}", errors(&output));
    output
}

fn errors(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

fn rejected(output: &Output, marker: &str) {
    assert_eq!(output.status.code(), Some(1), "{}", errors(output));
    assert!(output.stdout.is_empty(), "partial successful build output");
    assert!(errors(output).contains(marker), "{}", errors(output));
}

struct Built {
    labels: Vec<String>,
    paths: BTreeMap<String, PathBuf>,
    bytes: BTreeMap<String, Vec<u8>>,
    stderr: String,
}
impl Built {
    fn from_output(output: Output, expected: &[&str]) -> Self {
        assert!(output.status.success(), "{}", errors(&output));
        let mut labels = Vec::new();
        let mut paths = BTreeMap::new();
        for line in String::from_utf8(output.stdout.clone()).unwrap().lines() {
            let (label, path) = line.split_once('\t').expect("actual build output");
            assert!(
                !path.is_empty() && !path.contains('\t'),
                "invalid output row"
            );
            assert_eq!(Label::parse_canonical(label).unwrap().to_string(), label);
            labels.push(label.to_owned());
            assert!(
                paths
                    .insert(label.to_owned(), PathBuf::from(path))
                    .is_none(),
                "duplicate output label: {label}"
            );
        }
        // select() returns a BTreeSet<Label>. Check the exact ordered output
        // labels (including row count), not merely that some tree was emitted.
        let mut expected: Vec<_> = expected
            .iter()
            .map(|label| Label::parse_canonical(label).unwrap())
            .collect();
        expected.sort();
        assert_eq!(
            labels,
            expected.iter().map(ToString::to_string).collect::<Vec<_>>()
        );
        Self {
            bytes: paths
                .iter()
                .map(|(label, path)| (label.clone(), fs::read(path).unwrap()))
                .collect(),
            labels,
            paths,
            stderr: errors(&output),
        }
    }

    fn path(&self, label: &str) -> &Path {
        self.paths
            .get(label)
            .expect("expected output label")
            .as_path()
    }

    fn content(&self, label: &str) -> &[u8] {
        self.bytes.get(label).expect("expected output label")
    }

    fn assert_bytes(&self, expected: &[u8]) {
        assert!(!self.bytes.is_empty());
        for (label, bytes) in &self.bytes {
            assert_eq!(bytes, expected, "output for {label}");
        }
    }

    fn callbacks(&self, count: usize) {
        assert_eq!(
            self.stderr.matches("ENV_CALLBACK").count(),
            count,
            "{}",
            self.stderr
        );
    }
}

// Only traverse the fixture's private publication/cache directories, never host
// environment, process arguments, or unrelated filesystem roots.
fn inventory(root: &Path) -> BTreeMap<PathBuf, Vec<u8>> {
    fn visit(root: &Path, at: &Path, out: &mut BTreeMap<PathBuf, Vec<u8>>) {
        if let Ok(entries) = fs::read_dir(at) {
            for entry in entries {
                let entry = entry.unwrap();
                let kind = entry.file_type().unwrap();
                assert!(!kind.is_symlink(), "unexpected private-cache symlink");
                if kind.is_dir() {
                    visit(root, &entry.path(), out);
                } else {
                    out.insert(
                        entry.path().strip_prefix(root).unwrap().to_owned(),
                        fs::read(entry.path()).unwrap(),
                    );
                }
            }
        }
    }
    let mut result = BTreeMap::new();
    visit(root, root, &mut result);
    result
}

fn lifecycle(route: Route) {
    let f = Fixture::new(route, "", GET, DECLARED);
    let targets = ["@made//:data", "//:copy"];
    let first = f.build(&targets, &[(A, "alpha")]);
    first.assert_bytes(b"alpha");
    first.callbacks(1);
    assert!(first.stderr.contains("1 actions executed, 0 cache hits"));
    f.manifest(first.path(f.made_label()));
    let warm = f.build(&targets, &[(A, "alpha")]);
    // N61: identical specification (declared value alpha) and unchanged
    // recorded inputs reuse the verified publication without executing.
    warm.callbacks(0);
    assert_eq!(warm.paths, first.paths);
    assert_eq!(warm.bytes, first.bytes);
    assert!(warm.stderr.contains("0 actions executed, 1 cache hits"));
    let unrelated = f.build(&targets, &[(A, "alpha"), (UNUSED, "unrelated")]);
    // An UNDECLARED, unread environment change records nothing (Bazel's
    // RepoRecordedInput set): the publication is reused as-is.
    unrelated.callbacks(0);
    assert_eq!(unrelated.paths, first.paths);
    assert_eq!(unrelated.bytes, first.bytes);
    assert!(
        unrelated
            .stderr
            .contains("0 actions executed, 1 cache hits")
    );
    let changed = f.build(&targets, &[(A, "beta")]);
    changed.callbacks(1);
    assert_ne!(
        f.identity(changed.path(f.made_label())),
        f.identity(first.path(f.made_label()))
    );
    changed.assert_bytes(b"beta");
    assert!(changed.stderr.contains("1 actions executed, 0 cache hits"));
    let restored = f.build(&targets, &[(A, "alpha")]);
    // Restoring the declared value restores the first publication, which the
    // specification pointer still owns: pure reuse, no execution (N61).
    restored.callbacks(0);
    assert_eq!(restored.paths, first.paths);
    assert_eq!(restored.bytes, first.bytes);
    f.no_stages();
}

#[test]
fn undeclared_environment_changes_including_pwd_reuse_the_publication() {
    // Only DECLARED environ values (in the specification) and recorded reads
    // refetch; shell bookkeeping variables that differ between working
    // directories (PWD/OLDPWD/SHLVL/_) record nothing and must never break
    // warm reuse.
    for route in ROUTES {
        let f = Fixture::new(route, "", GET, DECLARED);
        let targets = ["@made//:data"];
        let shell = |dir: &'static str, old: &'static str, depth: &'static str| {
            vec![
                ("PWD", dir),
                ("OLDPWD", old),
                ("SHLVL", depth),
                (A, "alpha"),
            ]
        };
        let first = f.build(&targets, &shell("/one", "/old/one", "1"));
        first.assert_bytes(b"alpha");
        first.callbacks(1);
        let warm = f.build(&targets, &shell("/two", "/old/two", "5"));
        warm.callbacks(0);
        assert_eq!(warm.paths, first.paths);
        warm.assert_bytes(b"alpha");
        let changed = f.build(&targets, &[(A, "beta")]);
        changed.callbacks(1);
        changed.assert_bytes(b"beta");
        let restored = f.build(&targets, &shell("/one", "/old/one", "1"));
        restored.callbacks(0);
        assert_eq!(restored.paths, first.paths);
        restored.assert_bytes(b"alpha");
    }
}

#[test]
fn direct_getenv_drives_real_consumer_and_environment_keyed_publication() {
    lifecycle(Route::Direct);
}
#[test]
fn ordinary_extension_repository_getenv_drives_real_consumer_and_publication() {
    lifecycle(Route::Extension);
}

fn constant_watch(body: &str, options: &str) {
    for route in ROUTES {
        let f = Fixture::new(route, body, "'constant'", options);
        let absent = f.generated(&[]);
        let empty = f.generated(&[(A, "")]);
        let first = f.generated(&[(A, "alpha")]);
        let changed = f.generated(&[(A, "beta")]);
        for built in [&absent, &empty, &first, &changed] {
            built.assert_bytes(b"constant");
            built.callbacks(1);
            assert_eq!(
                Fixture::files_content(&f.manifest(built.path(f.made_label()))),
                Fixture::files_content(&f.manifest(first.path(f.made_label())))
            );
        }
        assert_ne!(absent.paths, empty.paths);
        assert_ne!(empty.paths, first.paths);
        assert_ne!(first.paths, changed.paths);
        assert_eq!(
            first.paths,
            f.generated(&[(A, "alpha"), (UNUSED, "changed")]).paths
        );
        assert_eq!(absent.paths, f.generated(&[]).paths);
    }
}

#[test]
fn declared_but_unread_inputs_change_identity_with_identical_manifest_files() {
    constant_watch("", DECLARED);
}

#[test]
fn discarded_dynamic_undeclared_reads_change_identity_with_identical_files() {
    constant_watch("    ctx.getenv('BAZEL_RS_ENV_A')\n", "");
}

#[test]
fn absence_empty_none_defaults_and_repeated_calls_return_actual_values() {
    for route in ROUTES {
        let f = Fixture::new(
            route,
            "",
            "json.encode([ctx.getenv('BAZEL_RS_ENV_A'),ctx.getenv('BAZEL_RS_ENV_A',None),ctx.getenv('BAZEL_RS_ENV_A','first'),ctx.getenv('BAZEL_RS_ENV_A','second')])",
            "",
        );
        for (environment, expected) in [
            (vec![], json!([null, null, "first", "second"])),
            (vec![(A, "")], json!(["", "", "", ""])),
            (
                vec![(A, "alpha")],
                json!(["alpha", "alpha", "alpha", "alpha"]),
            ),
        ] {
            let built = f.generated(&environment);
            assert_eq!(
                serde_json::from_slice::<Value>(built.content(f.made_label())).unwrap(),
                expected
            );
            built.callbacks(1);
        }
    }
}

#[test]
fn list_tuple_duplicate_declarations_execute_without_creating_attributes() {
    for names in [
        "[]",
        "()",
        "['BAZEL_RS_ENV_B','BAZEL_RS_ENV_A','BAZEL_RS_ENV_A']",
        "('BAZEL_RS_ENV_A','BAZEL_RS_ENV_B','BAZEL_RS_ENV_A')",
    ] {
        let f = Fixture::new(
            Route::Direct,
            "    if hasattr(ctx.attr,'environ'):\n        fail('environ leaked into attributes')\n",
            "ctx.getenv('BAZEL_RS_ENV_A')+'|'+ctx.getenv('BAZEL_RS_ENV_B')",
            &format!(", environ={names}"),
        );
        f.generated(&[(A, "alpha"), (B, "bee")])
            .assert_bytes(b"alpha|bee");
        // Different source declarations have different source provenance; do not
        // misrepresent this as a cross-source canonical-identity assertion.
    }
}

#[test]
fn constructor_copies_declarations_before_original_list_mutation_and_freeze() {
    for route in ROUTES {
        let f = Fixture::new(route, "", "'constant'", ", environ=names");
        f.file(
            "defs.bzl",
            format!(
                "names=['BAZEL_RS_ENV_A','BAZEL_RS_ENV_A']\n{}names.append('BAZEL_RS_ENV_B')\n",
                definition("", "'constant'", ", environ=names")
            ),
        );
        let first = f.generated(&[(A, "alpha"), (B, "bee")]);
        // A later appended, invalid-profile B must not even be observed.
        let second = f.generated(&[(A, "alpha"), (B, "é")]);
        assert_eq!(first.paths, second.paths);
        assert_ne!(first.paths, f.generated(&[(A, "beta"), (B, "é")]).paths);
    }
}

#[test]
fn unused_definitions_and_prepared_undemanded_calls_do_not_observe_environment() {
    for route in ROUTES {
        let f = Fixture::new(route, "", "'constant'", "");
        f.file(
            "defs.bzl",
            format!(
                "{}\nunused=repository_rule(_impl,environ=['BAZEL_RS_ENV_A'])\n",
                definition("", "'constant'", "")
            ),
        );
        match route {
            Route::Direct => f.file("MODULE.bazel", "module(name='main')\nmake=use_repo_rule('//:defs.bzl','make')\nunused=use_repo_rule('//:defs.bzl','unused')\nmake(name='made')\nunused(name='unused')\n"),
            Route::Extension => f.file("ext.bzl", "load(':defs.bzl','make','unused')\ndef _impl(ctx):\n    make(name='made')\n    unused(name='unused')\next=module_extension(_impl)\n"),
        }
        let first = f.generated(&[(A, "é")]);
        first.assert_bytes(b"constant");
        first.callbacks(1);
        assert_eq!(first.paths, f.generated(&[(A, "alpha")]).paths);
    }
}

#[test]
fn shared_snapshot_does_not_merge_other_repository_watch_sets() {
    let f = Fixture::new(Route::Direct, "", "'constant'", DECLARED);
    f.file(
        "defs.bzl",
        format!(
            "{}\naaa_other=repository_rule(_impl,environ=['BAZEL_RS_ENV_B'])\n",
            definition(
                "    print('ENV_REPOSITORY:'+ctx.original_name)\n",
                "'constant'",
                DECLARED
            )
        ),
    );
    f.file("MODULE.bazel", "module(name='main')\nmake=use_repo_rule('//:defs.bzl','make')\nother=use_repo_rule('//:defs.bzl','aaa_other')\nmake(name='made')\nother(name='other')\n");
    // B's exported rule sorts before make by canonical Label.repository. Both
    // selection and build therefore encounter B first. Prove execution order
    // with markers emitted after declared observations, not argv order alone.
    let other = "@@+aaa_other+other//:data";
    let made = f.made_label();
    let targets = [other, made];
    let first = f.build(&targets, &[(A, "alpha"), (B, "one")]);
    let second = f.build(&targets, &[(A, "alpha"), (B, "two")]);
    first.callbacks(2);
    // N61: only the repository whose DECLARED value changed re-executes; the
    // other reuses its publication (watch sets stay per-repository).
    second.callbacks(1);
    for (built, expected) in [(&first, &["other", "made"][..]), (&second, &["other"][..])] {
        assert_eq!(built.labels, [other, made]);
        let markers: Vec<_> = built
            .stderr
            .lines()
            .filter_map(|line| {
                line.split_once("ENV_REPOSITORY:")
                    .map(|(_, name)| name.trim())
            })
            .collect();
        assert_eq!(markers, expected, "{}", built.stderr);
        built.assert_bytes(b"constant");
    }
    assert_ne!(first.path(other), second.path(other));
    assert_eq!(first.path(made), second.path(made));
    assert_eq!(first.bytes, second.bytes);
    for label in [other, made] {
        assert_eq!(
            Fixture::files_content(&f.manifest(first.path(label))),
            Fixture::files_content(&f.manifest(second.path(label)))
        );
    }
}

#[test]
fn both_getenv_arguments_are_positional_only_and_arity_is_checked() {
    for call in [
        "ctx.getenv(name='BAZEL_RS_ENV_A')",
        "ctx.getenv('BAZEL_RS_ENV_A',default='fallback')",
        "ctx.getenv()",
        "ctx.getenv('BAZEL_RS_ENV_A',None,None)",
    ] {
        let f = Fixture::new(Route::Direct, &format!("    {call}\n"), "'unreachable'", "");
        rejected(&f.run(&["build", "//:copy"], &[(A, "alpha")]), "getenv");
        f.clean_failure();
    }
}

#[test]
fn invalid_name_and_default_types_fail_even_with_present_value() {
    for call in [
        "ctx.getenv(None)",
        "ctx.getenv(42)",
        "ctx.getenv(['BAZEL_RS_ENV_A'])",
        "ctx.getenv('BAZEL_RS_ENV_A',42)",
        "ctx.getenv('BAZEL_RS_ENV_A',[])",
        "ctx.getenv('BAZEL_RS_ENV_A',{})",
    ] {
        let f = Fixture::new(Route::Direct, &format!("    {call}\n"), "'unreachable'", "");
        let output = f.run(&["build", "//:copy"], &[(A, SECRET)]);
        rejected(&output, "getenv");
        assert!(!errors(&output).contains(SECRET));
        f.clean_failure();
        // Reader-count / validate-before-read assertions belong to the injected
        // runtime unit tests, not a false inference from a child-process error.
    }
}

#[test]
fn constructor_rejects_none_nonstrings_and_nonsequence_values() {
    for names in [
        "None",
        "'BAZEL_RS_ENV_A'",
        "{}",
        "[42]",
        "('BAZEL_RS_ENV_A',None)",
    ] {
        let f = Fixture::new(
            Route::Direct,
            "",
            "'unreachable'",
            &format!(", environ={names}"),
        );
        let output = f.run(&["build", "//:copy"], &[(A, SECRET)]);
        rejected(&output, "environ");
        assert!(!errors(&output).contains("ENV_CALLBACK"));
        assert!(!errors(&output).contains(SECRET));
        f.clean_failure();
    }
}

#[test]
fn invalid_posix_names_are_absent_without_equals_split_or_nul_truncation() {
    let names = "['','=','BAZEL_RS_ENV_A=TAIL','BAZEL_RS_ENV_A\\000TAIL']";
    for options in [String::new(), format!(", environ={names}")] {
        let f = Fixture::new(
            Route::Direct,
            &format!(
                "    for name in {names}:\n        if ctx.getenv(name) != None or ctx.getenv(name,'fallback') != 'fallback':\n            fail('invalid name was not absent')\n"
            ),
            "'absent'",
            &options,
        );
        let first = f.generated(&[(A, "prefix-one")]);
        let second = f.generated(&[(A, "prefix-two")]);
        first.assert_bytes(b"absent");
        assert_eq!(first.paths, second.paths);
    }
}

#[test]
fn question_mark_is_a_real_controlled_environment_name() {
    for options in ["", ", environ=['?']"] {
        let f = Fixture::new(
            Route::Direct,
            "",
            "json.encode([ctx.getenv('?'),ctx.getenv('?','fallback')])",
            options,
        );
        let absent = f.generated(&[]);
        let present = f.generated(&[("?", "question")]);
        assert_eq!(
            serde_json::from_slice::<Value>(absent.content(f.made_label())).unwrap(),
            json!([null, "fallback"])
        );
        assert_eq!(
            serde_json::from_slice::<Value>(present.content(f.made_label())).unwrap(),
            json!(["question", "question"])
        );
        assert_ne!(absent.paths, present.paths);
        assert_eq!(absent.paths, f.generated(&[]).paths);
    }
}

#[test]
fn discarded_values_never_persist_in_inventory_or_automatic_diagnostics() {
    for route in ROUTES {
        for (body, options) in [("", DECLARED), ("    ctx.getenv('BAZEL_RS_ENV_A')\n", "")] {
            let f = Fixture::new(route, body, "'constant'", options);
            let built = f.build(&["@made//:data", "//:copy"], &[(A, SECRET)]);
            built.assert_bytes(b"constant");
            assert!(!built.stderr.contains(SECRET));
            f.manifest(built.path(f.made_label()));
            for (path, bytes) in inventory(&f.0.path().join("cache")) {
                assert!(!path.to_string_lossy().contains(SECRET));
                assert!(
                    !bytes
                        .windows(SECRET.len())
                        .any(|window| window == SECRET.as_bytes()),
                    "secret persisted in {}",
                    path.display()
                );
            }
            // Callback-requested ctx.file(value) in the lifecycle tests is
            // intentionally allowed; only discarded/unread values are private.
        }
    }
}

#[test]
fn nonascii_and_non_utf8_values_are_sanitized_errors_not_absence_defaults() {
    for options in ["", DECLARED] {
        let f = Fixture::new(Route::Direct, "", GET, options);
        for value in [
            OsString::from(format!("{SECRET}é")),
            OsString::from_vec([SECRET.as_bytes(), &[0xff]].concat()),
        ] {
            let mut command = f.command(&["build", "//:copy"], &[]);
            command.env(A, value);
            let output = finish(&mut command);
            rejected(&output, "environment");
            assert!(!errors(&output).contains(SECRET));
            assert!(!output.stderr.contains(&0xff));
            assert!(!errors(&output).contains('é'));
            f.clean_failure();
        }
    }
}

#[test]
fn callback_failure_after_getenv_and_file_write_rolls_back_only_private_stage() {
    for route in ROUTES {
        let f = Fixture::new(route, "", "'constant'", DECLARED);
        let first = f.generated(&[(A, "alpha")]);
        let before = inventory(&f.store());
        f.definition("    ctx.getenv('BAZEL_RS_ENV_A')\n    ctx.file('partial','not published')\n    fail('controlled callback failure')\n", "'unreachable'", DECLARED);
        let output = f.run(&["build", "//:copy"], &[(A, SECRET)]);
        rejected(&output, "controlled callback failure");
        assert!(!errors(&output).contains(SECRET));
        assert_eq!(inventory(&f.store()), before);
        assert_eq!(fs::read(first.path(f.made_label())).unwrap(), b"constant");
        // No target action of the failed build materialized; the only
        // tolerated cache entries are the workspace status actions of
        // the executing build (N63 follow-up).
        if let Ok(entries) = fs::read_dir(f.0.path().join("cache/actions")) {
            for entry in entries.flatten() {
                let mut stack = vec![entry.path().join("outputs")];
                while let Some(dir) = stack.pop() {
                    for child in fs::read_dir(&dir).unwrap().flatten() {
                        if child.file_type().unwrap().is_dir() {
                            stack.push(child.path());
                        } else {
                            let name = child.file_name();
                            let name = name.to_string_lossy();
                            assert!(
                                matches!(
                                    name.as_ref(),
                                    "stable-status.txt" | "volatile-status.txt"
                                ),
                                "unexpected materialized output: {name}"
                            );
                        }
                    }
                }
            }
        }
        f.no_stages();
        f.definition("", "'constant'", DECLARED);
        assert_eq!(f.generated(&[(A, "alpha")]).paths, first.paths);
    }
}

#[test]
fn repository_environ_untracked_and_extension_environ_and_module_getenv_supported() {
    // repository_ctx.os.environ is the whole client environment and records
    // no dependency (StarlarkOS.java:49-54); only the declared name is watched.
    let os = Fixture::new(
        Route::Direct,
        "",
        "ctx.os.environ.get('BAZEL_RS_ENV_B','none')+'|'+','.join(ctx.os.environ.keys())",
        DECLARED,
    );
    let first = os.generated(&[(A, "alpha"), (B, "beta")]);
    assert_eq!(
        first.bytes[os.made_label()],
        b"beta|BAZEL_RS_ENV_A,BAZEL_RS_ENV_B,PATH"
    );
    let manifest = os.manifest(first.path(os.made_label()));
    assert!(!serde_json::to_string(&manifest).unwrap().contains("beta"));
    let second = os.generated(&[(A, "alpha")]);
    // os.environ records NO dependency (and no client-environment probe
    // exists), so the untracked B change reuses the first publication and
    // keeps serving its bytes — exactly like Bazel serving a cached
    // external/<name> whose rule never recorded that input.
    assert_eq!(
        second.bytes[os.made_label()],
        b"beta|BAZEL_RS_ENV_A,BAZEL_RS_ENV_B,PATH"
    );
    assert_eq!(second.path(os.made_label()), first.path(os.made_label()));
    // module_extension(environ=...) and module_ctx.getenv are supported
    // extension environment inputs (StarlarkBaseExternalContext.java:1535-1565).
    for (source, expected) in [
        (
            "load(':defs.bzl','make')\ndef _impl(ctx):\n    make(name='made')\next=module_extension(_impl,environ=['BAZEL_RS_ENV_A'])\n",
            "unreachable",
        ),
        (
            "load(':defs.bzl','make')\ndef _impl(ctx):\n    ctx.getenv('BAZEL_RS_ENV_A')\n    make(name='made')\next=module_extension(_impl)\n",
            "unreachable",
        ),
    ] {
        let f = Fixture::new(Route::Extension, "", "'unreachable'", "");
        f.file("ext.bzl", source);
        let built = f.build(&["//:copy"], &[(A, SECRET)]);
        assert_eq!(built.bytes["//:copy"], expected.as_bytes());
        assert!(!built.stderr.contains(SECRET));
    }
}
