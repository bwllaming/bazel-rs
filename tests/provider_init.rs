//! CLI coverage for `provider(init=...)` and its raw constructor, mirroring the
//! shape of @package_metadata//providers:package_metadata_info.bzl.
#![allow(clippy::disallowed_methods)] // test harness: starts processes directly (clippy.toml)

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
include!("support/explicit_modules.rs");

struct Workspace(tempfile::TempDir);

impl Workspace {
    fn new(providers: &str, rules: &str, build: &str) -> Self {
        let ws = Self(
            tempfile::Builder::new()
                .prefix("provider-init-cli-")
                .tempdir_in(crate::test_temp::root())
                .unwrap(),
        );
        ws.file("MODULE.bazel", "module(name='provider_init_fixture')\n");
        ws.file("BUILD.bazel", build);
        ws.file("metadata.json", "{\"name\": \"fixture\"}\n");
        ws.file("providers/BUILD.bazel", "");
        ws.file("providers/package_metadata_info.bzl", providers);
        ws.file("defs/BUILD.bazel", "");
        ws.file("defs/rules.bzl", rules);
        ws
    }

    fn path(&self) -> &Path {
        self.0.path()
    }

    fn file(&self, name: &str, text: &str) {
        let path = self.path().join(name);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, text).unwrap();
    }

    fn run(&self, args: &[&str]) -> Output {
        let output = Command::new("timeout")
            .args(["--kill-after=5s", "600s", env!("CARGO_BIN_EXE_bazel-rs")])
            .args(["--ignore_all_rc_files", "--cache-dir=private-cache"])
            .args(args)
            .current_dir(self.path())
            .explicit_modules()
            .output()
            .unwrap();
        assert_ne!(output.status.code(), Some(124), "CLI timed out: {args:?}");
        assert_ne!(output.status.code(), Some(137), "CLI killed: {args:?}");
        output
    }
}

fn errors(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

fn rejected(output: &Output, diagnostic: &str) {
    assert!(!output.status.success(), "unexpected success");
    assert!(output.stdout.is_empty(), "partial output on failure");
    assert!(errors(output).contains(diagnostic), "{}", errors(output));
    assert!(!errors(output).contains("panicked"), "{}", errors(output));
}

fn outputs(output: &Output) -> Vec<PathBuf> {
    assert!(output.status.success(), "{}", errors(output));
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .map(|line| PathBuf::from(line.split_once('\t').unwrap().1))
        .collect()
}

/// Same definition shape as the package_metadata module: documented field
/// dict, keyword-only validating/deriving init, tuple-unpacked raw constructor.
const PROVIDERS: &str = r#""""Providers for declaring package metadata."""

def _init(*, metadata, purl = None, **kwargs):
    if kwargs:
        fail("unexpected arguments: {}".format(", ".join(sorted(kwargs.keys()))))
    if type(metadata) != "File":
        fail("metadata must be a File, got {}".format(type(metadata)))
    if purl != None and not purl.startswith("pkg:"):
        fail("purl must start with 'pkg:', got {}".format(purl))
    return {
        "metadata": metadata,
        "purl": purl,
        "files": depset([metadata]),
    }

PackageMetadataInfo, _create = provider(
    doc = """Provider for declaring metadata about a Bazel package.

Fields are validated and derived by the init callback.""",
    fields = {
        "metadata": """The File containing metadata about the package.""",
        "purl": """The package URL, or None.""",
        "files": """depset of files carrying metadata.""",
    },
    init = _init,
)

# Exposes the raw constructor for the fixture's bypass rule only.
create_raw = _create
"#;

const RULES: &str = r#"
load("//providers:package_metadata_info.bzl", "PackageMetadataInfo", "create_raw")

def _public(ctx):
    return [PackageMetadataInfo(metadata = ctx.file.metadata, purl = ctx.attr.purl)]

public_metadata = rule(
    implementation = _public,
    attrs = {
        "metadata": attr.label(allow_single_file = True),
        "purl": attr.string(),
    },
)

def _raw(ctx):
    # Bypasses init: no validation and no derived "files" field.
    return [create_raw(metadata = ctx.file.metadata, purl = ctx.attr.purl)]

raw_metadata = rule(
    implementation = _raw,
    attrs = {
        "metadata": attr.label(allow_single_file = True),
        "purl": attr.string(),
    },
)

def _consume(ctx):
    lines = []
    for dep in ctx.attr.deps:
        info = dep[PackageMetadataInfo]
        files = [f.short_path for f in info.files.to_list()] if hasattr(info, "files") else None
        lines.append("{} {} {} {} {}".format(
            dep.label.name,
            PackageMetadataInfo in dep,
            info.metadata.short_path,
            info.purl,
            files,
        ))
    lines.append(str([type(PackageMetadataInfo), type(create_raw)]))
    out = ctx.actions.declare_file(ctx.label.name + ".txt")
    ctx.actions.write(out, "\n".join(lines) + "\n")
    return [DefaultInfo(files = depset([out]))]

consume = rule(
    implementation = _consume,
    attrs = {"deps": attr.label_list(providers = [PackageMetadataInfo])},
)
"#;

const BUILD: &str = r#"
load("//defs:rules.bzl", "consume", "public_metadata", "raw_metadata")

public_metadata(name = "public", metadata = "metadata.json", purl = "pkg:generic/fixture@1")
raw_metadata(name = "raw", metadata = "metadata.json", purl = "not-a-purl")
consume(name = "check", deps = [":public", ":raw"])
"#;

#[test]
fn public_init_and_raw_constructor_instances_share_the_provider_key() {
    let ws = Workspace::new(PROVIDERS, RULES, BUILD);
    let paths = outputs(&ws.run(&["build", ":check"]));
    assert_eq!(paths.len(), 1);
    assert_eq!(
        fs::read_to_string(&paths[0]).unwrap(),
        "public True metadata.json pkg:generic/fixture@1 [\"metadata.json\"]\n\
         raw True metadata.json not-a-purl None\n\
         [\"Provider\", \"RawConstructor\"]\n"
    );
}

#[test]
fn init_validation_and_schema_errors_fail_analysis() {
    for (rules, diagnostic) in [
        (
            RULES.replace(
                "purl = ctx.attr.purl)]\n\npublic_metadata",
                "purl = 'bad')]\n\npublic_metadata",
            ),
            "purl must start with 'pkg:', got bad",
        ),
        (
            RULES.replace(
                "PackageMetadataInfo(metadata = ctx.file.metadata, purl = ctx.attr.purl)",
                "PackageMetadataInfo(ctx.file.metadata)",
            ),
            // Positional arguments reach the keyword-only init unchanged
            // (StarlarkFunction.java:507-520).
            "_init() does not accept positional arguments, but got 1",
        ),
        (
            RULES.replace(
                "create_raw(metadata = ctx.file.metadata, purl = ctx.attr.purl)",
                "create_raw(metadata = ctx.file.metadata, license = 'x')",
            ),
            "got unexpected field 'license' in call to instantiate provider PackageMetadataInfo",
        ),
        (
            RULES.replace(
                "create_raw(metadata = ctx.file.metadata, purl = ctx.attr.purl)",
                "create_raw(ctx.file.metadata)",
            ),
            "<raw constructor for PackageMetadataInfo>: unexpected positional arguments",
        ),
    ] {
        let ws = Workspace::new(PROVIDERS, &rules, BUILD);
        rejected(&ws.run(&["build", ":check"]), diagnostic);
    }
}

#[test]
fn init_return_and_definition_errors_are_reported() {
    for (providers, diagnostic) in [
        (
            PROVIDERS.replace(
                "    return {\n        \"metadata\": metadata,",
                "    return [metadata]\n    return {\n        \"metadata\": metadata,",
            ),
            "got list for 'return value of provider init()', want dict",
        ),
        (
            PROVIDERS.replace("init = _init,", "init = \"_init\","),
            "in call to provider(), parameter 'init' got value of type 'string', want 'callable or NoneType'",
        ),
    ] {
        let ws = Workspace::new(&providers, RULES, BUILD);
        rejected(&ws.run(&["build", ":check"]), diagnostic);
    }
}
