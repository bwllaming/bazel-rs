//! N49: depsets of `struct()` values and Starlark provider instances
//! (docs/DEPSETS.md "Struct and provider elements (N49)"): Bazel's strict
//! element check, StructImpl equality dedup within and across targets in
//! every order, nested depsets/None/Target/runfiles/global-def fields, the
//! element type errors and the explicit gaps. The expected reports are equal
//! to genuine Bazel 9.3.0's after D1 (tests/fixtures/depset_structs/reference_receipt.json).
#![allow(clippy::disallowed_methods)] // test harness: starts processes directly (clippy.toml)
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

fn manifest() -> &'static Path {
    Path::new(env!("CARGO_MANIFEST_DIR"))
}

struct Workspace(tempfile::TempDir);

impl Workspace {
    fn fixture() -> Self {
        let base = std::env::var_os("TMPDIR")
            .map(PathBuf::from)
            .unwrap_or_else(std::env::temp_dir);
        let ws = Self(
            tempfile::Builder::new()
                .prefix("n49-")
                .tempdir_in(base)
                .unwrap(),
        );
        for package in ["ds", "df"] {
            let source = manifest()
                .join("tests/fixtures/depset_structs/workspace")
                .join(package);
            fs::create_dir_all(ws.0.path().join(package)).unwrap();
            for entry in fs::read_dir(&source).unwrap() {
                let path = entry.unwrap().path();
                fs::copy(
                    &path,
                    ws.0.path().join(package).join(path.file_name().unwrap()),
                )
                .unwrap();
            }
        }
        fs::write(
            ws.0.path().join("MODULE.bazel"),
            "module(name = \"depset_structs\")\n",
        )
        .unwrap();
        ws
    }

    fn run(&self, targets: &[&str]) -> Output {
        let output = Command::new("timeout")
            .args(["--kill-after=5s", "180s", env!("CARGO_BIN_EXE_bazel-rs")])
            .args([
                "--ignore_all_rc_files",
                "--cache-dir=private-cache",
                "build",
                "--experimental-builtin-modules",
            ])
            .arg(format!(
                "--registry=file://{}/tests/fixtures/builtin_modules/registry",
                manifest().display()
            ))
            .args(targets)
            .current_dir(self.0.path())
            .output()
            .unwrap();
        assert_ne!(output.status.code(), Some(124), "CLI timed out");
        output
    }
}

#[test]
fn struct_and_provider_elements_follow_bazel() {
    let ws = Workspace::fixture();
    let mut report = String::new();
    for target in ["r_default", "r_post", "r_pre", "r_topo"] {
        let label = format!("//ds:{target}");
        let output = ws.run(&[&label]);
        assert!(
            output.status.success(),
            "{label}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let stdout = String::from_utf8_lossy(&output.stdout);
        let path = stdout.lines().next().unwrap().split_once('\t').unwrap().1;
        report.push_str(&format!("{label}\n"));
        for line in fs::read_to_string(path).unwrap().lines() {
            report.push_str(&format!("  {line}\n"));
        }
    }
    assert_eq!(
        report,
        fs::read_to_string(manifest().join("tests/fixtures/depset_structs/expected.txt")).unwrap()
    );
}

#[test]
fn element_checks_and_gaps() {
    let ws = Workspace::fixture();
    for (kind, message) in [
        (
            "mutable_field",
            "depset elements must not be mutable values",
        ),
        ("list", "depset elements must not be mutable values"),
        ("dict", "depset elements must not be mutable values"),
        ("frozen_list", "depsets cannot contain items of type 'list'"),
        ("builtin", "depset elements must not be mutable values"),
        (
            "mix",
            "cannot add an item of type 'string' to a depset of 'struct'",
        ),
        (
            "mix_transitive",
            "cannot add an item of type 'struct' to a depset of 'string'",
        ),
        (
            "mutable_provider_field",
            "depset elements must not be mutable values",
        ),
        // N49b (df): frozen re-allocated containers, top-level frozen
        // list/dict, the order-incompatibility text, type before order.
        ("//df:bad_append", "trying to mutate a frozen list value"),
        (
            "//df:bad_append_global",
            "trying to mutate a frozen list value",
        ),
        (
            "//df:bad_append_nested",
            "trying to mutate a frozen list value",
        ),
        ("//df:bad_dict_pop", "trying to mutate a frozen dict value"),
        (
            "//df:bad_dict_setdefault",
            "trying to mutate a frozen dict value",
        ),
        (
            "//df:bad_top_frozen_dict",
            "depsets cannot contain items of type 'dict'",
        ),
        (
            "//df:bad_top_frozen_list_global",
            "depsets cannot contain items of type 'list'",
        ),
        (
            "//df:bad_order_pre_post",
            "Order 'preorder' is incompatible with order 'postorder'",
        ),
        (
            "//df:bad_order_topo_pre",
            "Order 'topological' is incompatible with order 'preorder'",
        ),
        (
            "//df:bad_order_post_topo",
            "Order 'postorder' is incompatible with order 'topological'",
        ),
        (
            "//df:bad_order_after_type",
            "cannot add an item of type 'int' to a depset of 'string'",
        ),
        (
            "//df:bad_order_second",
            "Order 'preorder' is incompatible with order 'topological'",
        ),
        (
            "lambda_field",
            "a depset element containing a function that is not a global def of a loaded .bzl file is not implemented in the native profile",
        ),
    ] {
        let label = if kind.contains(':') {
            kind.to_owned()
        } else {
            format!("//ds:bad_{kind}")
        };
        let output = ws.run(&[&label]);
        assert_eq!(output.status.code(), Some(1), "{kind}");
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            stderr.contains(&format!("error: {message}\n")),
            "{kind}: {stderr}"
        );
    }
}

/// N49b: frozen lists/dicts inside struct elements and the single-transitive
/// identity shortcut. The expected report is genuine Bazel 9.3.0's (paired
/// capture, reference_receipt.json O/reports //df:report and
/// //ds:bad_frozen_list_field: EXACT_D1).
#[test]
fn frozen_containers_and_single_transitive_identity_follow_bazel() {
    let ws = Workspace::fixture();
    for (label, expected) in [
        ("//df:report", DF_REPORT),
        ("//ds:bad_frozen_list_field", "[struct(l = [\"frozen\"])]\n"),
    ] {
        let output = ws.run(&[label]);
        assert!(
            output.status.success(),
            "{label}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let stdout = String::from_utf8_lossy(&output.stdout);
        let path = stdout.lines().next().unwrap().split_once('\t').unwrap().1;
        assert_eq!(fs::read_to_string(path).unwrap(), expected, "{label}");
    }
}

const DF_REPORT: &str = r#"frozen_len=1
frozen_repr=[struct(l = ["x", <generated file df/leaf.f>], m = {"k": [1, 2], "j": None, "f": <generated file df/leaf.f>})]
frozen_types=list dict File
frozen_eq=True True True
frozen_in=True True
list_vs_tuple=2 [struct(l = ["g1", ("t",)]), struct(l = ("g1", ("t",)))]
same_contents=1 [struct(l = ["g1", ("t",)])]
dict_order=1 [struct(m = {"b": 2, "a": 1})] True
nested=2 [struct(n = [[1, 2], {"k": [3]}, (4,)]), struct(v = [[1], (2,), {"m": {"n": [5]}}])]
nested_types=["list", "dict", "tuple", "list", "tuple", "dict"]
top_tuple=1 [(["g1", ("t",)],)]
frozen_depset_repr=depset([struct(l = ["g1", ("t",)], m = {"b": 2, "a": 1})])
files=[<generated file df/leaf.f>]
dict_keys=["b", "a"]
single=True
empty_sibling=True
empty_sibling_after=True
empty_incompatible_sibling=True
repeated=True
chain=True
direct_equal_singleton=True
direct_dup_equal_singleton=True
direct_other_singleton=False
direct_in_multi=False
two_sets=False
compatible_order=False
default_of_preorder=False
same_preorder=True
topological=True
topological_repeated=True
topological_direct=True
preorder_direct_other_order=False
in_list=True
not_in_list=False
dict_key="v"
dict_key_other="missing"
struct_dedup=1
depset_dedup=1
empty_result=True
empty_typed_skipped=["s"]
empty_typed_result=True
repr="[depset([1, 2]), depset([1, 2], order = \"topological\"), depset([1, 2], order = \"preorder\")]"
list=[[1, 2], [1, 2, 3]]
"#;
