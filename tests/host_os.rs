//! Real isolated CLI coverage of the bounded host-metadata foundation.
//! The field expectations below are transcribed from the genuine nine-case
//! host-OS contract, not loaded from ignored receipts at test time. Only the
//! Linux/x86_64 compile target is reference-backed (getter arch is "amd64").
//! Other compile targets are explicitly unsupported, not runtime-emulated here.
//! Cross-host factor/hash injection and frozen OS ownership belong to core units.
#![cfg(all(target_os = "linux", target_arch = "x86_64"))]
#![allow(clippy::disallowed_methods)] // test harness: starts processes directly (clippy.toml)

use bazel_rs::label::Label;
use serde_json::Value;
use std::collections::BTreeMap;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

const A: &str = "BAZEL_RS_HOST_A";
const UNUSED: &str = "BAZEL_RS_HOST_UNUSED";
const SECRET: &str = "SYNTHETIC_SECRET_42";
const METADATA: &str = include_str!("fixtures/host_os/metadata.bzl");
// Exact field projection from genuine output, with only context_type substituted.
const GOLDEN: &str = r#"{"arch":"amd64","arch_json":"\"amd64\"","arch_repr":"\"amd64\"","arch_spellings":[true,false],"arch_str":"amd64","arch_type":"string","context_type":"CONTEXT","dir":["arch","environ","name"],"hashable_lookup":"safe-key","lower_platform":"linux","name":"linux","name_json":"\"linux\"","name_repr":"\"linux\"","name_str":"linux","name_type":"string","repr":"<unknown object com.google.devtools.build.lib.bazel.repository.starlark.StarlarkOS>","same_context_object":true,"short_ascii_name_ops":[true,true,true,false],"str":"<unknown object com.google.devtools.build.lib.bazel.repository.starlark.StarlarkOS>","type":"repository_os"}"#;

#[derive(Clone, Copy)]
enum Route {
    Direct,
    Extension,
}
const ROUTES: [Route; 2] = [Route::Direct, Route::Extension];

struct Fixture {
    temp: tempfile::TempDir,
    route: Route,
    symbol: &'static str,
}

impl Fixture {
    fn new(route: Route) -> Self {
        let f = Self {
            temp: tempfile::Builder::new()
                .prefix("host-os-cli-")
                .tempdir_in("/tmp/opencode")
                .unwrap(),
            route,
            symbol: "ext",
        };
        fs::create_dir(f.temp.path().join("home")).unwrap();
        fs::create_dir(f.temp.path().join("tmp")).unwrap();
        f.module();
        f.file("BUILD.bazel", "genrule(name='copy',srcs=['@made//:data'],outs=['copy.txt'],cmd='/bin/cat $(SRCS) > $@')\n");
        f.file("metadata.bzl", METADATA);
        f.repository("", "ctx.attr.text", "");
        f.extension(
            "    make(name='made',text='constant')\n",
            "module_extension(_impl)",
        );
        f
    }

    fn module(&self) {
        match self.route {
            Route::Direct => self.file("MODULE.bazel", "module(name='main')\nmake=use_repo_rule('//:defs.bzl','make')\nmake(name='made',text='constant')\n"),
            Route::Extension => self.file("MODULE.bazel", format!("module(name='main')\ne=use_extension('//:ext.bzl','{}')\nuse_repo(e,'made')\n", self.symbol)),
        }
    }

    fn file(&self, name: &str, content: impl AsRef<[u8]>) {
        fs::write(self.temp.path().join(name), content).unwrap();
    }

    fn repository(&self, body: &str, value: &str, options: &str) {
        self.file("defs.bzl", format!("load(':metadata.bzl','metadata')\ndef _repo(ctx):\n    print('HOST_REPO_CALLBACK')\n{body}    ctx.file('BUILD.bazel', \"exports_files(['data'])\\n\", executable=False)\n    ctx.file('data', {value}, executable=False)\nmake=repository_rule(_repo,attrs={{'text':attr.string()}}, {options})\n"));
    }

    fn extension(&self, body: &str, constructor: &str) {
        self.file("ext.bzl", format!("load(':defs.bzl','make')\nload(':metadata.bzl','metadata')\ndef _impl(ctx):\n    print('HOST_EXT_CALLBACK')\n{body}ext={constructor}\n"));
    }

    fn metadata_flow(&self, options: &str) {
        self.repository("", "'{\"extension\":'+ctx.attr.text+',\"repository\":'+json.encode(metadata(ctx))+',\"selected\":\"'+ctx.os.name.lower()+'/'+ctx.os.arch+'\"}\\n'", "");
        self.extension(
            "    make(name='made',text=json.encode(metadata(ctx)))\n",
            &format!("module_extension(_impl{options})"),
        );
    }

    fn label(&self) -> String {
        let symbol = match self.route {
            Route::Direct => "make",
            Route::Extension => self.symbol,
        };
        format!("@@+{symbol}+made//:data")
    }

    fn run(&self, args: &[&str], environment: &[(&str, &str)]) -> Output {
        let mut command = Command::new("/usr/bin/timeout");
        command
            .args(["--kill-after=5s", "60s", env!("CARGO_BIN_EXE_bazel-rs")])
            .args(["--ignore_all_rc_files", "--cache-dir=cache"])
            .args(args)
            .current_dir(self.temp.path())
            .env_clear()
            .env("PATH", "/usr/bin:/bin")
            .env("HOME", self.temp.path().join("home"))
            .env("TMPDIR", self.temp.path().join("tmp"))
            .env("LC_ALL", "C");
        for (name, value) in environment {
            assert!(matches!(*name, A | UNUSED | "SYNTHETIC_SECRET"));
            command.env(name, value);
        }
        let output = command.output().unwrap();
        assert_ne!(output.status.code(), Some(124), "CLI timeout");
        assert_ne!(output.status.code(), Some(137), "CLI forced termination");
        assert!(!error(&output).contains("panicked"), "{}", error(&output));
        output
    }

    fn build(&self, environment: &[(&str, &str)]) -> Built {
        Built::new(
            self.run(&["build", "@made//:data", "//:copy"], environment),
            &[&self.label(), "//:copy"],
        )
    }

    fn store(&self) -> PathBuf {
        self.temp.path().join("cache/repositories/direct")
    }

    fn identity(&self, built: &Built) -> String {
        let tree = built.paths[&self.label()].parent().unwrap();
        assert_eq!(tree.parent().unwrap(), self.store().join("trees"));
        let id = tree.file_name().unwrap().to_str().unwrap();
        assert_eq!(id.len(), 64);
        assert!(id.bytes().all(|b| b.is_ascii_hexdigit()));
        id.to_owned()
    }

    fn manifest(&self, built: &Built) -> Value {
        let id = self.identity(built);
        let manifest: Value = serde_json::from_slice(
            &fs::read(self.store().join(format!("manifests/{id}.json"))).unwrap(),
        )
        .unwrap();
        let mut keys: Vec<_> = manifest
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        keys.sort_unstable();
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

    fn clean_failure(&self) {
        // Deterministic stage locks persist (empty files, like publication
        // locks); trees, manifests and stage directories stay empty.
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
        assert!(!self.temp.path().join("cache/actions").exists());
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

fn error(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

fn reject(output: &Output, marker: &str) {
    assert_eq!(output.status.code(), Some(1), "{}", error(output));
    assert!(output.stdout.is_empty(), "partial successful output");
    assert!(error(output).contains(marker), "{}", error(output));
    assert!(!error(output).contains(SECRET), "secret in diagnostics");
}

struct Built {
    paths: BTreeMap<String, PathBuf>,
    bytes: BTreeMap<String, Vec<u8>>,
    stderr: String,
}

impl Built {
    fn new(output: Output, expected: &[&str]) -> Self {
        assert!(output.status.success(), "{}", error(&output));
        let mut paths = BTreeMap::new();
        let mut labels = Vec::new();
        for row in String::from_utf8(output.stdout.clone()).unwrap().lines() {
            let (label, path) = row.split_once('\t').expect("label/path output");
            assert!(!path.is_empty() && !path.contains('\t'));
            assert_eq!(Label::parse_canonical(label).unwrap().to_string(), label);
            labels.push(label.to_owned());
            assert!(
                paths
                    .insert(label.to_owned(), PathBuf::from(path))
                    .is_none()
            );
        }
        let mut expected: Vec<_> = expected
            .iter()
            .map(|x| Label::parse_canonical(x).unwrap())
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
            paths,
            stderr: error(&output),
        }
    }

    fn assert_bytes(&self, expected: &[u8]) {
        assert_eq!(self.bytes.len(), 2);
        for (label, bytes) in &self.bytes {
            assert_eq!(bytes, expected, "output for {label}");
        }
    }

    fn actions(&self, executed: usize, hits: usize) {
        assert!(
            self.stderr
                .contains(&format!("{executed} actions executed, {hits} cache hits")),
            "{}",
            self.stderr
        );
    }
}

// Traverse only this fixture's private publication/cache, never the user environment.
fn inventory(root: &Path) -> BTreeMap<PathBuf, (Vec<u8>, u32)> {
    fn visit(root: &Path, at: &Path, result: &mut BTreeMap<PathBuf, (Vec<u8>, u32)>) {
        if let Ok(entries) = fs::read_dir(at) {
            for entry in entries {
                let entry = entry.unwrap();
                assert!(!entry.file_type().unwrap().is_symlink());
                if entry.file_type().unwrap().is_dir() {
                    visit(root, &entry.path(), result);
                } else {
                    result.insert(
                        entry.path().strip_prefix(root).unwrap().to_owned(),
                        (
                            fs::read(entry.path()).unwrap(),
                            entry.metadata().unwrap().permissions().mode() & 0o777,
                        ),
                    );
                }
            }
        }
    }
    let mut result = BTreeMap::new();
    visit(root, root, &mut result);
    result
}

fn golden_flow() -> String {
    format!(
        "{{\"extension\":{},\"repository\":{},\"selected\":\"linux/amd64\"}}\n",
        GOLDEN.replace("CONTEXT", "module_ctx"),
        GOLDEN.replace("CONTEXT", "repository_ctx")
    )
}

#[test]
fn direct_metadata_strings_reach_the_real_consumer_without_flags() {
    let f = Fixture::new(Route::Direct);
    f.repository("", "json.encode(metadata(ctx))", "");
    let built = f.build(&[]);
    built.assert_bytes(GOLDEN.replace("CONTEXT", "repository_ctx").as_bytes());
    built.actions(1, 0);
    f.manifest(&built);
}

#[test]
fn extension_metadata_passes_string_attrs_through_repository_to_consumer() {
    let f = Fixture::new(Route::Extension);
    f.metadata_flow("");
    let query = f.run(&["query", "@made//:data"], &[]);
    assert!(query.status.success(), "{}", error(&query));
    assert_eq!(query.stdout, b"@made//:data\n");
    f.build(&[]).assert_bytes(golden_flow().as_bytes());
}

#[test]
fn four_named_boolean_profiles_have_same_host_output_and_warm_reuse() {
    // Source changes are provenance changes, NOT cross-host factor evidence.
    // There is no user flag with which this CLI can emulate another host.
    for os in ["False", "True"] {
        for arch in ["False", "True"] {
            let f = Fixture::new(Route::Extension);
            f.metadata_flow(&format!(", os_dependent={os}, arch_dependent={arch}"));
            let first = f.build(&[]);
            first.assert_bytes(golden_flow().as_bytes());
            first.actions(1, 0);
            let warm = f.build(&[]);
            warm.assert_bytes(golden_flow().as_bytes());
            assert_eq!(first.paths, warm.paths);
            warm.actions(0, 1);
        }
    }
}

#[test]
fn frozen_factory_constructor_alias_and_keyword_unpack_execute_real_callback() {
    let f = Fixture::new(Route::Extension);
    f.file("factory.bzl", "constructor=module_extension\ndef factory(impl):\n    return constructor(implementation=impl,**{'os_dependent':True,'arch_dependent':True})\n");
    f.file("ext.bzl", "load(':defs.bzl','make')\nload(':factory.bzl','factory')\ndef original_callback(ctx):\n    make(name='made',text=ctx.os.name+'/'+ctx.os.arch)\ncallback_alias=original_callback\next=factory(callback_alias)\n");
    f.build(&[]).assert_bytes(b"linux/amd64");
}

#[test]
fn requested_extension_alias_not_callback_name_owns_canonical_repository() {
    let mut f = Fixture::new(Route::Extension);
    f.file("ext.bzl", "load(':defs.bzl','make')\ndef callback_name(ctx):\n    host=ctx.os\n    saved=host\n    make(name='made',text=str(saved == ctx.os)+':'+{host:'same'}[ctx.os])\next=module_extension(callback_name,os_dependent=True,arch_dependent=True)\nalias=ext\n");
    let original = f.build(&[]);
    original.assert_bytes(b"True:same");
    f.symbol = "alias";
    f.module();
    let alias = f.build(&[]);
    alias.assert_bytes(b"True:same");
    assert!(alias.paths.contains_key("@@+alias+made//:data"));
    assert_ne!(
        f.identity(&alias),
        original.paths["@@+ext+made//:data"]
            .parent()
            .unwrap()
            .file_name()
            .unwrap()
            .to_str()
            .unwrap()
    );
}

#[test]
fn structural_environ_presence_unknown_default_and_saved_alias_are_safe() {
    let expression = "json.encode([hasattr(ctx,'os'),hasattr(ctx.os,'environ'),hasattr(ctx.os,'absent'),getattr(ctx.os,'absent','fallback'),ctx.os == ctx.os])";
    for route in ROUTES {
        let f = Fixture::new(route);
        match route {
            Route::Direct => f.repository("", expression, ""),
            Route::Extension => f.extension(
                &format!("    make(name='made',text={expression})\n"),
                "module_extension(_impl)",
            ),
        }
        f.build(&[("SYNTHETIC_SECRET", SECRET)])
            .assert_bytes(b"[true,true,false,\"fallback\",true]");
    }
}

#[test]
fn name_and_arch_assignments_are_immutable_in_both_callbacks() {
    for route in ROUTES {
        for field in ["name", "arch"] {
            let f = Fixture::new(route);
            let body = format!("    host=ctx.os\n    host.{field}='changed'\n");
            match route {
                Route::Direct => f.repository(&body, "'unreachable'", ""),
                Route::Extension => f.extension(
                    &format!("{body}    make(name='made')\n"),
                    "module_extension(_impl)",
                ),
            }
            let output = f.run(&["build", "//:copy"], &[]);
            reject(&output, field);
            assert!(
                error(&output).contains("repository_os"),
                "{}",
                error(&output)
            );
            f.clean_failure();
        }
    }
}

#[test]
fn unknown_os_field_is_a_callback_attribute_error_not_missing_os_global() {
    for route in ROUTES {
        let f = Fixture::new(route);
        match route {
            Route::Direct => f.repository("    ctx.os.missing_host_field\n", "'unreachable'", ""),
            Route::Extension => f.extension(
                "    ctx.os.missing_host_field\n    make(name='made')\n",
                "module_extension(_impl)",
            ),
        }
        let output = f.run(&["build", "//:copy"], &[]);
        reject(&output, "missing_host_field");
        assert!(
            error(&output).contains("repository_os"),
            "{}",
            error(&output)
        );
        assert!(!error(&output).contains("not defined"));
        let callback = match route {
            Route::Direct => "HOST_REPO_CALLBACK",
            Route::Extension => "HOST_EXT_CALLBACK",
        };
        assert!(error(&output).contains(callback));
        f.clean_failure();
    }
}

#[test]
fn repository_environ_is_the_client_environment_and_never_a_recorded_dependency() {
    // repository_ctx.os.environ is the command's repoEnv: the complete client
    // environment (StarlarkOS.java:44-58, CommandEnvironment.java:342-416).
    let f = Fixture::new(Route::Direct);
    f.repository(
        "",
        &format!(
            "json.encode([ctx.os.environ.get('{A}','fallback'), ctx.os.environ.keys()])+'\\n'"
        ),
        "",
    );
    let keys = |with_a: bool| {
        let mut keys = vec!["HOME", "LC_ALL", "PATH", "TMPDIR"];
        if with_a {
            keys.insert(0, A);
        }
        serde_json::to_string(&keys).unwrap()
    };
    let alpha = f.build(&[(A, "alpha")]);
    alpha.assert_bytes(format!("[\"alpha\",{}]\n", keys(true)).as_bytes());
    // os.environ records no dependency, and no whole-client-environment probe
    // exists (N61 follow-up: repositories depend only on declared environ
    // values and recorded inputs): a changed value REUSES the publication,
    // like Bazel serving a cached external/<name> whose rule never recorded
    // an environment input.
    let beta = f.build(&[(A, "beta")]);
    beta.assert_bytes(format!("[\"alpha\",{}]\n", keys(true)).as_bytes());
    assert_eq!(f.identity(&alpha), f.identity(&beta));
    let absent = f.build(&[]);
    absent.assert_bytes(format!("[\"alpha\",{}]\n", keys(true)).as_bytes());
    assert_eq!(f.identity(&alpha), f.identity(&absent));
    let restored = f.build(&[(A, "alpha")]);
    assert_eq!(f.identity(&alpha), f.identity(&restored));
}

#[test]
fn module_ctx_environ_is_the_client_environment_too() {
    // module_ctx.os is the same StarlarkOS over repoEnv
    // (StarlarkBaseExternalContext.java:194-196); environ records nothing.
    let f = Fixture::new(Route::Extension);
    f.extension(
        &format!("    e=ctx.os.environ\n    e['MUTATED']='x'\n    make(name='made',text=json.encode([getattr(ctx.os,'environ',{{}}).get('{A}','fallback'), ctx.os.environ.keys()])+'\\n')\n"),
        "module_extension(_impl)",
    );
    let keys = serde_json::to_string(&[A, "HOME", "LC_ALL", "PATH", "TMPDIR"]).unwrap();
    f.build(&[(A, "alpha")])
        .assert_bytes(format!("[\"alpha\",{keys}]\n").as_bytes());
}

#[test]
fn repo_env_overrides_reach_os_environ_without_secret_echo() {
    // --repo_env (N40, crate::repo_env): `=NAME` removes a client value and
    // `NAME=VALUE` sets one in repository_ctx.os.environ
    // (CommandEnvironment.java:393-416); values are never printed.
    let f = Fixture::new(Route::Direct);
    f.repository(
        "",
        "ctx.os.environ.get('SYNTHETIC_SECRET', 'absent') + '|' + ctx.os.environ.get('N40_SET', 'none')",
        "",
    );
    let built = Built::new(
        f.run(
            &[
                "build",
                "@made//:data",
                "//:copy",
                "--repo_env==SYNTHETIC_SECRET",
                "--repo_env=N40_SET=set",
            ],
            &[("SYNTHETIC_SECRET", SECRET)],
        ),
        &[&f.label(), "//:copy"],
    );
    built.assert_bytes(b"absent|set");
    assert!(!built.stderr.contains(SECRET));
    let option = format!("--repo_env=N40_SET={SECRET}");
    let built = Built::new(
        f.run(&["build", "@made//:data", "//:copy", &option], &[]),
        &[&f.label(), "//:copy"],
    );
    assert!(!built.stderr.contains(SECRET), "{}", built.stderr);
}

#[test]
fn whole_os_json_rejection_is_native_profile_not_unexecuted_java_evidence() {
    // Source-derived native boundary only; the genuine nine cases did not
    // execute whole-object JSON serialization.
    for route in ROUTES {
        let f = Fixture::new(route);
        match route {
            Route::Direct => f.repository("", "json.encode(ctx.os)", ""),
            Route::Extension => f.extension(
                "    make(name='made',text=json.encode(ctx.os))\n",
                "module_extension(_impl)",
            ),
        }
        reject(
            &f.run(&["build", "//:copy"], &[]),
            "cannot encode unsupported value as JSON",
        );
        f.clean_failure();
    }
}

#[test]
fn dependency_flags_reject_none_strings_and_integers_strictly() {
    for flag in ["os_dependent", "arch_dependent"] {
        for value in ["None", "'yes'", "0", "1"] {
            let f = Fixture::new(Route::Extension);
            f.extension(
                "    make(name='made')\n",
                &format!("module_extension(_impl,{flag}={value})"),
            );
            let output = f.run(&["build", "//:copy"], &[]);
            reject(&output, flag);
            assert!(!error(&output).contains("HOST_EXT_CALLBACK"));
            f.clean_failure();
        }
    }
}

#[test]
fn dependency_flags_are_named_only_not_new_positional_parameters() {
    for arguments in ["_impl,True", "_impl,{},True,False"] {
        let f = Fixture::new(Route::Extension);
        f.extension(
            "    make(name='made')\n",
            &format!("module_extension({arguments})"),
        );
        let output = f.run(&["build", "//:copy"], &[]);
        reject(&output, "module_extension");
        assert!(!error(&output).contains("HOST_EXT_CALLBACK"));
        f.clean_failure();
    }
}

#[test]
fn extension_facts_versions_stay_unsupported_but_environ_and_getenv_execute() {
    let f = Fixture::new(Route::Extension);
    f.extension(
        "    make(name='made')\n",
        "module_extension(_impl,os_dependent=True,arch_dependent=True,facts_versions={'1':{}})",
    );
    reject(
        &f.run(&["build", "//:copy"], &[("SYNTHETIC_SECRET", SECRET)]),
        "facts_versions",
    );
    f.clean_failure();
    // module_extension(environ=[...]) and module_ctx.getenv succeed in Bazel
    // 9.3 and are recorded extension environment inputs.
    let f = Fixture::new(Route::Extension);
    f.extension(
        &format!("    make(name='made',text=ctx.getenv('{A}','unset'))\n"),
        &format!("module_extension(_impl,os_dependent=True,arch_dependent=True,environ=['{A}'])"),
    );
    f.build(&[(A, "alpha")]).assert_bytes(b"alpha");
    f.build(&[(A, "beta")]).assert_bytes(b"beta");
    f.build(&[]).assert_bytes(b"unset");
}

#[test]
fn callback_source_edit_changes_actual_consumer_bytes_and_restores_prior_key() {
    let f = Fixture::new(Route::Extension);
    let source = "load(':defs.bzl','make')\ndef _impl(ctx):\n    make(name='made',text='before:'+ctx.os.name+'/'+ctx.os.arch)\next=module_extension(_impl)\n";
    f.file("ext.bzl", source);
    let first = f.build(&[]);
    first.assert_bytes(b"before:linux/amd64");
    first.actions(1, 0);
    let warm = f.build(&[]);
    assert_eq!(warm.paths, first.paths);
    warm.actions(0, 1);
    f.file("ext.bzl", source.replace("before:", "after:"));
    let changed = f.build(&[]);
    changed.assert_bytes(b"after:linux/amd64");
    changed.actions(1, 0);
    assert_ne!(f.identity(&first), f.identity(&changed));
    f.file("ext.bzl", source);
    let restored = f.build(&[]);
    restored.assert_bytes(b"before:linux/amd64");
    assert_eq!(restored.paths, first.paths);
    restored.actions(0, 1);
    f.no_stages();
}

#[test]
fn unchanged_host_keeps_environment_watches_and_manifest_profile_without_action_miss() {
    for route in ROUTES {
        for (body, options) in [
            ("", "environ=['BAZEL_RS_HOST_A']"),
            ("    ctx.getenv('BAZEL_RS_HOST_A')\n", ""),
        ] {
            let f = Fixture::new(route);
            // No OS getter reads: repository host hashing is unconditional.
            f.repository(body, "'constant'", options);
            let first = f.build(&[(A, "alpha")]);
            first.assert_bytes(b"constant");
            first.actions(1, 0);
            let first_manifest = f.manifest(&first);
            let first_tree = inventory(first.paths[&f.label()].parent().unwrap());
            let warm = f.build(&[(A, "alpha"), (UNUSED, SECRET)]);
            assert_eq!(warm.paths, first.paths);
            warm.actions(0, 1);
            let changed = f.build(&[(A, "beta")]);
            changed.assert_bytes(b"constant");
            assert_ne!(f.identity(&changed), f.identity(&first));
            // Files match on content; `stat` fingerprints (Step B) record
            // the new tree's inodes/times and legitimately differ.
            let strip_stat = |files: &serde_json::Value| {
                let mut files = files.clone();
                for entry in files.as_object_mut().unwrap().values_mut() {
                    entry.as_object_mut().unwrap().remove("stat");
                }
                files
            };
            assert_eq!(
                strip_stat(&f.manifest(&changed)["files"]),
                strip_stat(&first_manifest["files"])
            );
            assert_eq!(
                inventory(changed.paths[&f.label()].parent().unwrap()),
                first_tree
            );
            // Content/logical action inputs are unchanged despite a new tree ID.
            changed.actions(0, 1);
            let restored = f.build(&[(A, "alpha")]);
            assert_eq!(restored.paths, first.paths);
            restored.actions(0, 1);
            for (path, (bytes, _)) in inventory(&f.temp.path().join("cache")) {
                assert!(!path.to_string_lossy().contains(SECRET));
                assert!(!bytes.windows(SECRET.len()).any(|x| x == SECRET.as_bytes()));
            }
            f.no_stages();
        }
    }
}
