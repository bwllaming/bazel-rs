//! N56 native behavioral coverage of the validation output group
//! (docs/VALIDATION_ACTIONS.md) over the shared fixture
//! tests/fixtures/validation_actions: the paired genuine-Bazel receipt
//! (reference_receipt.json) records the same phases' native columns; these
//! tests assert them executably.
#![allow(clippy::disallowed_methods)] // test harness: starts processes directly (clippy.toml)
include!("support/module_archives.rs");

use std::fs;
use std::path::Path;
use std::process::{Command, Output};
include!("support/explicit_modules.rs");

struct Workspace(tempfile::TempDir);

impl Workspace {
    fn path(&self) -> &Path {
        self.0.path()
    }

    fn new() -> Self {
        let ws = Self(
            tempfile::Builder::new()
                .prefix("validation-actions-")
                .tempdir_in("/tmp/opencode")
                .unwrap(),
        );
        let fixture =
            Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/validation_actions");
        copy(&fixture, ws.path());
        ws
    }

    fn build_with(&self, flags: &[&str], targets: &[&str]) -> Output {
        Command::new("timeout")
            .args(["--kill-after=5s", "600s", env!("CARGO_BIN_EXE_bazel-rs")])
            .arg("--ignore_all_rc_files")
            .arg("--cache-dir=cache")
            .arg("build")
            .args(flags)
            .args(targets)
            .current_dir(self.path())
            .explicit_modules()
            .output()
            .unwrap()
    }

    fn run(&self, arguments: &[&str]) -> Output {
        Command::new("timeout")
            .args(["--kill-after=5s", "600s", env!("CARGO_BIN_EXE_bazel-rs")])
            .arg("--ignore_all_rc_files")
            .arg("--cache-dir=cache")
            .args(arguments)
            .current_dir(self.path())
            .explicit_modules()
            .output()
            .unwrap()
    }

    /// The bazel-bin-relative names this workspace materialized.
    fn outputs(&self) -> Vec<String> {
        let mut names = Vec::new();
        let actions = self.path().join("cache/actions");
        let Ok(entries) = fs::read_dir(actions) else {
            return names;
        };
        for entry in entries.flatten() {
            // <action>/outputs/bazel-out/<config>/bin/<file...>
            collect(&entry.path().join("outputs"), &mut names);
        }
        // The workspace status files materialize on every executing
        // build (N63 follow-up); they are not validation outputs.
        names.retain(|name| !matches!(name.as_str(), "stable-status.txt" | "volatile-status.txt"));
        names.sort();
        names
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
            fs::copy(entry.path(), target).unwrap();
        }
    }
}

fn collect(bin: &Path, names: &mut Vec<String>) {
    let Ok(entries) = fs::read_dir(bin) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect(&path, names);
        } else {
            // The path below bazel-out (its configuration and bin segments
            // included), matching the receipt's bin keys.
            if let Some(relative) = path
                .ancestors()
                .find(|path| path.file_name().is_some_and(|part| part == "bazel-out"))
                .and_then(|base| path.strip_prefix(base).ok())
            {
                names.push(relative.to_string_lossy().into_owned());
            }
        }
    }
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

fn ok(output: &Output) -> String {
    assert!(output.status.success(), "{}", stderr(output));
    stderr(output)
}

fn rejected(output: &Output, message: &str) {
    let text = stderr(output);
    assert!(!output.status.success(), "unexpected success:\n{text}");
    assert!(text.contains(message), "expected {message:?} in:\n{text}");
}

#[test]
fn validation_outputs_run_with_the_dependent_and_report_nothing() {
    // A01: `top` builds its own output and the collected `_validation`
    // outputs of `mid` (its own and `leaf`'s); hidden groups never report.
    let ws = Workspace::new();
    let text = ok(&ws.build_with(&[], &["//:top"]));
    assert!(
        !text.contains("validated") && !text.contains("mid.mine"),
        "{text}"
    );
    assert_eq!(
        ws.outputs(),
        [
            "k8-fastbuild/bin/leaf.validated",
            "k8-fastbuild/bin/mid.mine",
            "k8-fastbuild/bin/top.out",
        ]
    );
}

#[test]
fn run_validations_false_is_the_only_way_to_skip_them() {
    // A02/A03: --run_validations=false skips exactly the validation outputs;
    // --output_groups=-_validation does not (determineOutputGroups adds the
    // group back after the subtractions, OutputGroupInfo.java:236-242).
    let ws = Workspace::new();
    ok(&ws.build_with(&["--run_validations=false"], &["//:top"]));
    assert_eq!(ws.outputs(), ["k8-fastbuild/bin/top.out"]);
    let ws = Workspace::new();
    ok(&ws.build_with(&["--output_groups=-_validation"], &["//:top"]));
    assert!(
        ws.outputs()
            .contains(&"k8-fastbuild/bin/leaf.validated".to_owned())
    );
}

#[test]
fn output_groups_select_override_add_and_missing_groups() {
    // A04/A05/A06: an unprefixed token overrides the default set, `+` adds
    // to it, and a group the target does not have builds nothing without an
    // error (TopLevelArtifactHelper skips empty groups, :221-224).
    let ws = Workspace::new();
    ok(&ws.build_with(&["--output_groups=extra"], &["//:g"]));
    assert_eq!(ws.outputs(), ["k8-fastbuild/bin/g.extra"]);
    let ws = Workspace::new();
    ok(&ws.build_with(&["--output_groups=+extra"], &["//:g"]));
    assert_eq!(
        ws.outputs(),
        ["k8-fastbuild/bin/g.extra", "k8-fastbuild/bin/g.out"]
    );
    let ws = Workspace::new();
    ok(&ws.build_with(&["--output_groups=missing"], &["//:g"]));
    assert_eq!(ws.outputs(), Vec::<String>::new());
}

#[test]
fn tool_and_implicit_dependencies_do_not_propagate() {
    // A07/A08: `cfg = "exec"` tool dependencies and `_`-prefixed implicit
    // attributes never contribute (Attribute.isToolDependency; the observed
    // genuine-9.3 contract of receipt A08).
    let ws = Workspace::new();
    // Offline: the genuine registry fixture, and the @platforms archive
    // that execution platform resolution materializes.
    seed_module_archives(&ws.path().join("cache"), &["platforms@1.0.0"]);
    ok(&ws.build_with(
        &["--experimental-builtin-modules", &builtin_registry_flag()],
        &["//:tool_only"],
    ));
    assert_eq!(ws.outputs(), ["k8-fastbuild/bin/tool_only.out"]);
    let ws = Workspace::new();
    ok(&ws.build_with(&[], &["//:private_dep"]));
    assert_eq!(
        ws.outputs(),
        [
            "k8-fastbuild/bin/private_dep.mine",
            "k8-fastbuild/bin/private_dep.out",
        ]
    );
}

#[test]
fn label_list_dict_and_output_file_dependencies_propagate() {
    // A09/A10/A11: DEPENDENCY-class dict attributes collect; an output-file
    // target's OutputGroupInfo is its generating rule's `_validation` group
    // (OutputFileConfiguredTarget.java:109-118), both built directly and
    // through a consumer's attribute.
    let ws = Workspace::new();
    ok(&ws.build_with(&[], &["//:dict_dep"]));
    assert!(
        ws.outputs()
            .contains(&"k8-fastbuild/bin/leaf.validated".to_owned())
    );
    let ws = Workspace::new();
    ok(&ws.build_with(&[], &["//:vout.out"]));
    assert_eq!(
        ws.outputs(),
        [
            "k8-fastbuild/bin/vout.out",
            "k8-fastbuild/bin/vout.validated"
        ]
    );
    let ws = Workspace::new();
    ok(&ws.build_with(&[], &["//:file_consumer"]));
    assert!(
        ws.outputs()
            .contains(&"k8-fastbuild/bin/vout.validated".to_owned())
    );
}

#[test]
fn native_rules_and_failing_validations() {
    // A12/A13: a genrule collects its dependencies' validation outputs; a
    // failing validation action fails the build (rc 1).
    let ws = Workspace::new();
    ok(&ws.build_with(&[], &["//:gen"]));
    assert!(
        ws.outputs()
            .contains(&"k8-fastbuild/bin/leaf.validated".to_owned())
    );
    let ws = Workspace::new();
    rejected(&ws.build_with(&[], &["//:failing"]), "action '//:failing");
}

#[test]
fn private_transitive_group_and_absent_validationprovider() {
    // A14/A17: the `_validation_transitive` private API is rejected with
    // Bazel's text; Bazel 9.3 has no Starlark ValidationProvider symbol, so
    // referencing it is a name-resolution failure at load.
    let ws = Workspace::new();
    rejected(
        &ws.build_with(&[], &["//:transitive"]),
        "//:defs.bzl cannot access the _transitive_validation private API",
    );
    let ws = Workspace::new();
    rejected(
        &ws.run(&["build", "//probe:probe"]),
        "name 'ValidationProvider' is not defined",
    );
}

#[test]
fn aspect_validation_outputs_run_with_the_target() {
    // A16 (N58): --aspects applies the aspect to the top-level target; its
    // `_validation` outputs run with the target (the merged OutputGroupInfo
    // answers the `_validation` request) and are never reported (receipt
    // A16-aspect-validation, genuine bin plain.aspect_validated + plain.out,
    // reported plain.out only).
    let ws = Workspace::new();
    let text = ok(&ws.build_with(&["--aspects=//:aspects.bzl%val_aspect"], &["//:plain"]));
    assert!(!text.contains("aspect_validated"), "{text}");
    assert_eq!(
        ws.outputs(),
        [
            "k8-fastbuild/bin/plain.aspect_validated",
            "k8-fastbuild/bin/plain.out",
        ]
    );
}

#[test]
fn skip_validations_remains_an_explicit_boundary() {
    // A15: a skip_validations attribute is a declared feature the native
    // analysis does not model (genuine Bazel gates it behind its own
    // allowlist).
    let ws = Workspace::new();
    rejected(
        &ws.build_with(&[], &["//:skipped"]),
        "attribute 'skipped' skip_validations=True",
    );
}

#[test]
fn collection_follows_rule_class_attribute_order() {
    // P01: the built-in `toolchains` attribute collects before the declared
    // attributes, and those in declaration order (`z_first` before
    // `a_second`), matching genuine Bazel's printed to_list() order
    // (receipt post_capture_changes).
    let ws = Workspace::new();
    let output = ws.run(&["build", "//order:check"]);
    assert!(output.status.success(), "{}", stderr(&output));
    let text = stderr(&output);
    let order = text
        .lines()
        .find_map(|line| line.split_once("ORDER ").map(|(_, rest)| rest.trim()))
        .unwrap_or_else(|| panic!("no ORDER line in:\n{text}"));
    assert_eq!(
        order,
        "order/t_toolchains.validated,order/t_z.validated,order/t_a.validated"
    );
}
