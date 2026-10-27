//! Tab-only lexer parity, captured against Bazel 9.3.0 on a tiny offline workspace.
#![allow(clippy::disallowed_methods)] // isolated CLI test harness

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use serde::Deserialize;
use sha2::{Digest, Sha256};
use starlark::codemap::{Pos, Span};
use starlark::environment::{Globals, Module};
use starlark::eval::Evaluator;
use starlark::syntax::{AstModule, Dialect};

#[derive(Deserialize)]
struct Receipt {
    probes: Vec<Probe>,
}

#[derive(Deserialize)]
struct Probe {
    name: String,
    source: String,
    source_sha256: String,
    accepted: bool,
    value: Option<String>,
    diagnostics: Vec<Diagnostic>,
}

#[derive(Deserialize)]
struct Diagnostic {
    line: usize,
    column: usize,
    message: String,
}

#[test]
fn genuine_tab_matrix_preserves_source_values_and_error_columns() {
    let receipt: Receipt = serde_json::from_str(include_str!(
        "fixtures/starlark_tabs/reference_receipt.json"
    ))
    .unwrap();
    assert_eq!(receipt.probes.len(), 37);
    for probe in receipt.probes {
        assert_eq!(
            format!("{:x}", Sha256::digest(probe.source.as_bytes())),
            probe.source_sha256,
            "{}: fixture source bytes",
            probe.name
        );
        let filename = format!("{}/defs.bzl", probe.name);
        let result = AstModule::parse(&filename, probe.source.clone(), &Dialect::Standard);
        assert_eq!(result.is_ok(), probe.accepted, "{}: {result:?}", probe.name);
        if probe.accepted {
            let ast = result.unwrap();
            let original = ast.file_span(Span::new(
                Pos::new(0),
                Pos::new(probe.source.len().try_into().unwrap()),
            ));
            assert_eq!(original.source_span().as_bytes(), probe.source.as_bytes());
            Module::with_temp_heap(|module| {
                Evaluator::new(&module)
                    .eval_module(ast, &Globals::standard())
                    .unwrap_or_else(|error| panic!("{}: {error:#}", probe.name));
                // Known, independently reproduced on the unpatched registry
                // parser: native drops CR in triple strings, even without tabs.
                // Do not change general string semantics in this whitespace fix.
                let expected = if probe.name == "crlf_triple_tab" {
                    assert_eq!(probe.value.as_deref(), Some("a\r\n\tb"));
                    Some("a\n\tb")
                } else {
                    probe.value.as_deref()
                };
                assert_eq!(
                    module.get("V").unwrap().unpack_str(),
                    expected,
                    "{}: executed string bytes",
                    probe.name
                );
            });
        } else {
            let error = result.unwrap_err();
            let expected = &probe.diagnostics[0];
            // Pin the new tab diagnostic, and ordinary syntax errors *after*
            // tabs, so accepting whitespace cannot shift other token positions.
            // Existing non-tab dedent/escape diagnostic wording is out of scope.
            if expected.message.starts_with("Tab characters") || probe.name.starts_with("error_") {
                let span = error.span().unwrap().resolve_span();
                assert_eq!(
                    (span.begin.line + 1, span.begin.column + 1),
                    (expected.line, expected.column),
                    "{}: {error:#}",
                    probe.name
                );
            }
            if expected.message.starts_with("Tab characters") {
                assert_eq!(error.without_diagnostic().to_string(), expected.message);
            }
        }
    }
}

struct Fixture {
    _temp: tempfile::TempDir,
    root: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        // Canonical real paths, even if the runner's TMPDIR contains a symlink.
        // Do not weaken the production repository filesystem checks.
        let temp = tempfile::Builder::new()
            .prefix("bazel-rs-tab-parity-")
            .tempdir()
            .unwrap();
        let root = temp.path().canonicalize().unwrap();
        let fixture = Self { _temp: temp, root };
        for dir in ["home", "tmp", "registry"] {
            fs::create_dir(fixture.root.join(dir)).unwrap();
        }
        fixture.file("MODULE.bazel", "module(\n\tname\t=\t'tab_probe',\n)\n");
        fixture.file("BUILD.bazel", "");
        fixture
    }

    fn file(&self, path: &str, contents: &str) {
        let path = self.root.join(path);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, contents).unwrap();
    }

    fn run(&self, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_bazel-rs"))
            .current_dir(&self.root)
            .env_clear()
            .env("PATH", "/usr/bin:/bin")
            .env("HOME", self.root.join("home"))
            .env("TMPDIR", self.root.join("tmp"))
            .arg("--ignore_all_rc_files")
            .arg(format!("--cache-dir={}", self.root.join("cache").display()))
            .args(args)
            .arg("--experimental-builtin-modules=false")
            .arg(format!(
                "--registry=file://{}",
                self.root.join("registry").display()
            ))
            .arg("--lockfile_mode=off")
            .output()
            .unwrap()
    }
}

fn success(output: &Output) -> String {
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout.clone()).unwrap()
}

#[test]
fn tabs_in_module_load_and_build_keep_input_and_attribute_bytes() {
    let fixture = Fixture::new();
    let defs = "def emit(name):\n    native.filegroup(\n\tname = name,\n \t tags = [\"a\tb\", r\"a\\tb\"],\n\t)\n";
    let build = "load(\n\t':defs.bzl',\n\t'emit',\n)\nemit(\n\tname = 'org_openapitools_openapi_generator_cli',\n)\n";
    fixture.file("defs.bzl", defs);
    fixture.file("BUILD.bazel", build);
    let output = success(&fixture.run(&["query", "--output=build", "//:all"]));
    assert!(
        output.contains("org_openapitools_openapi_generator_cli"),
        "{output}"
    );
    assert!(output.contains(r#"tags = ["a\tb", "a\\tb"]"#), "{output}");
    assert_eq!(
        fs::read(fixture.root.join("defs.bzl")).unwrap(),
        defs.as_bytes()
    );
    assert_eq!(
        fs::read(fixture.root.join("BUILD.bazel")).unwrap(),
        build.as_bytes()
    );

    fixture.file(
        "defs.bzl",
        "def emit(name):\n\tnative.filegroup(name=name)\n",
    );
    let bad = fixture.run(&["query", "//:all"]);
    assert!(!bad.status.success());
    assert!(
        String::from_utf8_lossy(&bad.stderr)
            .contains("Tab characters are not allowed for indentation")
    );
}

#[test]
fn repository_generated_build_tabs_are_not_rewritten() {
    let fixture = Fixture::new();
    fixture.file("MODULE.bazel", "module(\n\tname = 'tab_probe',\n)\nr = use_repo_rule(\n\t'//:repo.bzl',\n\t'maker',\n)\nr(\n\tname = 'generated',\n)\n");
    let generated =
        "filegroup(\n\tname = 'org_openapitools_openapi_generator_cli',\n\ttags = ['a\tb'],\n)\n";
    fixture.file("repo.bzl", &format!("def _impl(ctx):\n    ctx.file(\n\t'BUILD.bazel',\n\t{},\n    )\nmaker = repository_rule(\n\timplementation = _impl,\n)\n", serde_json::to_string(generated).unwrap()));
    let output = success(&fixture.run(&["query", "--output=build", "@generated//:all"]));
    assert!(output.contains(r#"tags = ["a\tb"]"#), "{output}");
    let trees = fixture.root.join("cache/repositories/direct/trees");
    let builds: Vec<_> = fs::read_dir(trees)
        .unwrap()
        .map(|entry| entry.unwrap().path().join("BUILD.bazel"))
        .filter(|path| path.is_file())
        .collect();
    assert_eq!(builds.len(), 1);
    assert_eq!(fs::read(&builds[0]).unwrap(), generated.as_bytes());
}

#[test]
fn module_indentation_tabs_are_rejected() {
    for source in [
        "\tmodule(name='bad')\n",
        "\t# comment\nmodule(name='bad')\n",
        "\t\nmodule(name='bad')\n",
    ] {
        let error = bazel_rs::bzlmod::evaluate_source("MODULE.bazel", source.into()).unwrap_err();
        assert!(
            error.contains("Tab characters are not allowed for indentation"),
            "{error}"
        );
    }
}

#[test]
fn syntax_vendor_inventory_is_anchored_and_only_lexer_is_patched() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let provenance: serde_json::Value = serde_json::from_slice(
        &fs::read(root.join("vendor/starlark_syntax-0.14.2.provenance.json")).unwrap(),
    )
    .unwrap();
    let vendor = root.join("vendor/starlark_syntax-0.14.2");
    let patches = provenance["declared_source_patches"].as_array().unwrap();
    assert_eq!(patches.len(), 1);
    assert_eq!(patches[0]["path"], "src/lexer.rs");
    let mut files = Vec::new();
    fn collect(root: &Path, path: &Path, files: &mut Vec<String>) {
        for entry in fs::read_dir(path).unwrap() {
            let path = entry.unwrap().path();
            let metadata = path.symlink_metadata().unwrap();
            if metadata.is_dir() {
                collect(root, &path, files);
            } else {
                assert!(metadata.is_file(), "{}", path.display());
                files.push(
                    path.strip_prefix(root)
                        .unwrap()
                        .to_str()
                        .unwrap()
                        .to_owned(),
                );
            }
        }
    }
    collect(&vendor, &vendor, &mut files);
    files.sort();
    let mut originals = Vec::new();
    for path in files {
        let data = fs::read(vendor.join(&path)).unwrap();
        let current = serde_json::json!({"sha256": format!("{:x}", Sha256::digest(&data)), "size": data.len()});
        if path == "LICENSE" {
            assert_eq!(current, provenance["license"]["content"]);
            continue;
        }
        let original = if path == "src/lexer.rs" {
            assert_eq!(current, patches[0]["current"]);
            patches[0]["original"].clone()
        } else {
            current
        };
        // Explicit key order matches the provenance's sorted-key canonical JSON.
        originals.push(serde_json::json!({"path": path, "sha256": original["sha256"], "size": original["size"]}));
    }
    assert_eq!(originals.len(), 274);
    assert_eq!(
        format!(
            "{:x}",
            Sha256::digest(serde_json::to_vec(&originals).unwrap())
        ),
        provenance["archive"]["archive_member_inventory"]["canonical_json_sha256"]
            .as_str()
            .unwrap()
    );
}
