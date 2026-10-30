//! Genuine embedded resources and captured public-registry closure, offline.
#![allow(clippy::disallowed_methods)] // test harness: starts processes directly (clippy.toml)
use bazel_rs::module_graph::{self, BuiltinProfile, ModuleSource, ResolverOptions};
include!("support/temp_root.rs");
include!("support/exit_codes.rs");
use bazel_rs::repository::RepositorySet;
use serde_json::Value;
use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

fn fixtures() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/builtin_modules")
}

fn options() -> ResolverOptions {
    ResolverOptions {
        builtin_profile: BuiltinProfile::Bazel93Builtins,
        registries: vec![fixtures().join("registry").into()],
        ..Default::default()
    }
}

fn workspace(module: &str) -> tempfile::TempDir {
    let directory = tempfile::Builder::new()
        .prefix("builtin-cli-")
        .tempdir_in(crate::test_temp::root())
        .unwrap();
    fs::write(directory.path().join("MODULE.bazel"), module).unwrap();
    fs::write(
        directory.path().join("BUILD.bazel"),
        "exports_files(['data.txt'])\n",
    )
    .unwrap();
    fs::write(directory.path().join("data.txt"), "local\n").unwrap();
    directory
}

fn run(root: &Path, command: &str, arguments: &[&str]) -> Output {
    Command::new("timeout")
        .args([
            "600s",
            env!("CARGO_BIN_EXE_bazel-rs"),
            "--ignore_all_rc_files",
            "--cache-dir=cache",
            command,
        ])
        .arg(format!(
            "--registry=file://{}",
            fixtures().join("registry").display()
        ))
        .args(arguments)
        .current_dir(root)
        .output()
        .unwrap()
}

fn success(output: &Output) {
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

fn rejected(output: &Output, marker: &str) {
    // Exit 2 for an option error (crate::exit_code), 1 otherwise.
    assert_eq!(
        output.status.code(),
        Some(rejection_code(&String::from_utf8_lossy(&output.stderr))),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(output.stdout.is_empty());
    assert!(
        String::from_utf8_lossy(&output.stderr).contains(marker),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn genuine_closure_matches_independent_bazel_selection_without_extracting_tools() {
    let expected: Value =
        serde_json::from_slice(&fs::read(fixtures().join("expectations.json")).unwrap()).unwrap();
    assert_eq!(
        expected["reference_sha256"],
        "7f775b54bbec487efaf0f49ccda1d2ed02cd34f582edfdd6aeeec76e39ef585c"
    );
    let cases = expected["cases"].as_array().unwrap();
    assert!(cases.len() >= 2);
    for case in cases {
        let root = workspace(case["root_module"].as_str().unwrap());
        let repos = RepositorySet::with_options(root.path().to_owned(), options())
            .with_source_cache(PathBuf::from("cache/repositories"));
        let graph = repos.resolve_graph().unwrap();
        let actual: BTreeSet<_> = graph
            .modules
            .values()
            .map(|m| (m.name.clone(), m.version.clone()))
            .collect();
        let expected: BTreeSet<_> = case["selected"]
            .as_array()
            .unwrap()
            .iter()
            .map(|m| {
                (
                    m["name"].as_str().unwrap().to_owned(),
                    m["version"].as_str().unwrap().to_owned(),
                )
            })
            .collect();
        assert_eq!(actual, expected, "{}", case["name"]);
        assert_eq!(graph.root_mapping["bazel_tools"], "bazel_tools");
        assert!(matches!(
            graph.modules["bazel_tools"].source,
            ModuleSource::Embedded(_)
        ));
        assert_eq!(
            graph.root.effective_dependencies().last().unwrap().name,
            "bazel_tools"
        );
        assert!(
            graph.modules["bazel_tools"]
                .module
                .implicit_dependencies
                .is_empty()
        );
        for module in graph.modules.values() {
            assert_eq!(module.mapping["bazel_tools"], "bazel_tools");
            if module.name != "bazel_tools" {
                assert_eq!(
                    module.module.effective_dependencies().last().unwrap().name,
                    "bazel_tools"
                );
            }
        }
        bazel_rs::repository_catalog::build(graph, false).unwrap();
        assert!(
            !root.path().join("cache").exists(),
            "metadata/catalog must not extract tools or execute extensions"
        );
        assert!(!graph.root_mapping.contains_key("rules_java"));
        if !graph
            .root
            .dependencies
            .iter()
            .any(|d| d.name == "platforms")
        {
            assert!(!graph.root_mapping.contains_key("platforms"));
        }
    }
}

#[test]
fn genuine_builtin_query_build_and_helper_loading_use_standalone_native_resources() {
    let root = workspace("module(name='main')\n");
    let query = run(
        root.path(),
        "query",
        // The default profile.
        &["@bazel_tools//tools/build_defs/repo:utils.bzl"],
    );
    success(&query);
    assert_eq!(
        query.stdout,
        b"@bazel_tools//tools/build_defs/repo:utils.bzl\n"
    );
    fs::write(
        root.path().join("BUILD.bazel"),
        "load(':copy.bzl','copy')\ncopy()\n",
    )
    .unwrap();
    fs::write(root.path().join("copy.bzl"), "load('@bazel_tools//tools/build_defs/repo:utils.bzl','update_attrs')\nprint('NATIVE_HELPER_LOADED')\ndef copy():\n    values=update_attrs(struct(name='copy'),[],{'srcs':['@bazel_tools//tools/build_defs/repo:utils.bzl'],'outs':['copy.txt'],'cmd':'cat $(SRCS) > $@'})\n    native.genrule(**values)\n").unwrap();
    let build = run(
        root.path(),
        "build",
        &["--experimental-builtin-modules=true", "//:copy"],
    );
    success(&build);
    assert!(String::from_utf8_lossy(&build.stderr).contains("NATIVE_HELPER_LOADED"));
    let text = String::from_utf8(build.stdout).unwrap();
    let (_, output) = text.lines().next().unwrap().split_once('\t').unwrap();
    let repos = RepositorySet::with_options(root.path().to_owned(), options())
        .with_source_cache(root.path().join("cache/repositories"));
    let builtin = repos.root("bazel_tools").unwrap();
    assert!(builtin.starts_with(root.path().join("cache/repositories/builtins")));
    assert_eq!(
        fs::read(output).unwrap(),
        fs::read(builtin.join("tools/build_defs/repo/utils.bzl")).unwrap()
    );
    assert_eq!(
        fs::read(builtin.join("MODULE.bazel")).unwrap(),
        bazel_rs::builtin_tools::module_bytes()
    );
    success(&run(
        root.path(),
        "build",
        &["--experimental-builtin-modules", "//:copy"],
    ));
}

#[test]
fn metadata_override_does_not_replace_canonical_builtin_filesystem() {
    let root = workspace(
        "module(name='main')\nlocal_path_override(module_name='bazel_tools',path='metadata')\n",
    );
    fs::create_dir(root.path().join("metadata")).unwrap();
    fs::write(
        root.path().join("metadata/MODULE.bazel"),
        "module(name='bazel_tools',version='8.7')\n",
    )
    .unwrap();
    fs::write(root.path().join("metadata/marker"), "not embedded tools").unwrap();
    let repos = RepositorySet::with_options(root.path().to_owned(), options())
        .with_source_cache(root.path().join("selected-cache"));
    let graph = repos.resolve_graph().unwrap();
    assert!(matches!(
        graph.modules["bazel_tools"].source,
        ModuleSource::LocalPath(_)
    ));
    assert_eq!(
        graph.modules["bazel_tools"]
            .module
            .module
            .as_ref()
            .unwrap()
            .version,
        "8.7"
    );
    let embedded = repos.root("bazel_tools").unwrap();
    assert!(embedded.starts_with(root.path().join("selected-cache/builtins")));
    assert!(!embedded.join("marker").exists());
    assert_eq!(
        fs::read(embedded.join("MODULE.bazel")).unwrap(),
        bazel_rs::builtin_tools::module_bytes()
    );
}

#[test]
fn ignore_dev_preserves_builtin_and_ignored_aliases_still_collide() {
    let root = workspace("module(name='main')\n");
    let opts = ResolverOptions {
        ignore_dev_dependencies: true,
        ..options()
    };
    let graph = module_graph::resolve(root.path(), &opts).unwrap();
    assert_eq!(graph.root_mapping["bazel_tools"], "bazel_tools");
    for module in [
        "module(name='bazel_tools')\n",
        "module(name='main')\nbazel_dep(name='bazel_tools',dev_dependency=True)\n",
    ] {
        fs::write(root.path().join("MODULE.bazel"), module).unwrap();
        let error = module_graph::resolve(root.path(), &opts).unwrap_err();
        assert!(error.contains("bazel_tools"), "{error}");
    }
}

#[test]
fn builtin_default_explicit_opt_out_and_invalid_flag_do_not_silently_switch_profiles() {
    let root = workspace("module(name='main')\n");
    // Like Bazel 9.3, the built-in modules are the default; the native-only
    // switch (and `=true`) is a no-op.
    let builtin = run(
        root.path(),
        "query",
        &["@bazel_tools//tools/build_defs/repo:utils.bzl"],
    );
    success(&builtin);
    for flag in [
        "--experimental-builtin-modules",
        "--experimental-builtin-modules=true",
    ] {
        let explicit = run(
            root.path(),
            "query",
            &[flag, "@bazel_tools//tools/build_defs/repo:utils.bzl"],
        );
        success(&explicit);
        assert_eq!(explicit.stdout, builtin.stdout);
    }
    rejected(
        &run(
            root.path(),
            "query",
            &[
                "--experimental-builtin-modules=false",
                "@bazel_tools//tools/build_defs/repo:utils.bzl",
            ],
        ),
        // The mapping entry exists in every module, like Bazel; the explicit
        // profile only lacks the contents.
        "built-in repository @@bazel_tools is visible to every module but has no contents in the explicit-module profile; drop --experimental-builtin-modules=false",
    );
    rejected(
        &run(
            root.path(),
            "query",
            &["--experimental-builtin-modules=maybe", "//:data.txt"],
        ),
        "expects true or false",
    );
    success(&run(
        root.path(),
        "query",
        &["--experimental-builtin-modules=false", "//:data.txt"],
    ));
    rejected(
        &run(
            root.path(),
            "query",
            &[
                "--experimental-builtin-modules",
                "--override_repository=bazel_tools=metadata",
                "//:data.txt",
            ],
        ),
        "unsupported option",
    );
}

#[test]
fn missing_registry_metadata_is_an_error_not_a_reduced_builtin_graph() {
    let root = workspace("module(name='main')\n");
    let empty = root.path().join("empty-registry");
    fs::create_dir(&empty).unwrap();
    fs::write(empty.join("bazel_registry.json"), "{}\n").unwrap();
    let options = ResolverOptions {
        registries: vec![empty.into()],
        ..options()
    };
    let error = module_graph::resolve(root.path(), &options).unwrap_err();
    assert!(error.contains("rules_license"), "{error}");
    assert!(!root.path().join("bazel-rs-out").exists());
}

#[test]
fn graph_command_presents_the_genuine_builtin_closure() {
    // `mod graph` uses the built-in modules by default like Bazel 9.3; with
    // --include_builtin its JSON is the genuine capture of each case
    // (cases/*/graph.json), byte for byte. Nothing is materialized: the
    // graph needs no extension's repositories.
    let expected: Value =
        serde_json::from_slice(&fs::read(fixtures().join("expectations.json")).unwrap()).unwrap();
    for case in expected["cases"].as_array().unwrap() {
        let root = workspace(case["root_module"].as_str().unwrap());
        let output = run(
            root.path(),
            "mod",
            &["graph", "--output=json", "--include_builtin"],
        );
        success(&output);
        let genuine = fs::read(fixtures().join(case["reference_graph"].as_str().unwrap())).unwrap();
        assert_eq!(
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&genuine),
            "{}",
            case["name"]
        );
        // Without --include_builtin the built-in subtree is hidden.
        let hidden = run(root.path(), "mod", &["graph", "--output=json"]);
        success(&hidden);
        let json: Value = serde_json::from_slice(&hidden.stdout).unwrap();
        assert!(
            json["dependencies"]
                .as_array()
                .unwrap()
                .iter()
                .all(|dependency| dependency["key"] != "bazel_tools@_"),
            "{json}"
        );
    }
}
