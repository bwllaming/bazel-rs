//! W1b (PLAN-L1 §6): the engine resolves every File through the build's one
//! artifact index, as Bazel resolves an action input, a DefaultInfo file or a
//! runfile through its Artifact, however the File reached the consumer (a
//! provider three hops away, a closure, a forwarded DefaultInfo, a tool's
//! runfiles). Programs the former legality machinery rejected and Bazel
//! accepts now build; the paired genuine Bazel 9.3 capture is
//! tests/fixtures/artifact_identity/reference_receipt.json. Every build runs
//! with `BAZEL_RS_VERIFY_ARTIFACTS=1` (no File re-registered with a different
//! Artifact; until W1d it also re-ran the former provider-file walk after
//! each commit and asserted every File it reached was registered) and
//! `BAZEL_RS_VERIFY_VIEWS=1` (every memoized dependency view is rebuilt on
//! each later edge and compared).
#![allow(clippy::disallowed_methods)] // test harness: starts processes directly (clippy.toml)
include!("support/verify_env.rs");
include!("support/console.rs");
use std::fs;
use std::path::Path;
use std::process::{Command, Output};
include!("support/explicit_modules.rs");

struct Workspace(tempfile::TempDir);

impl Workspace {
    fn new() -> Self {
        let ws = Self(
            tempfile::Builder::new()
                .prefix("artifact-identity-")
                .tempdir_in(crate::test_temp::root())
                .unwrap(),
        );
        let source = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/artifact_identity/workspace/pkg");
        fs::create_dir_all(ws.path().join("pkg")).unwrap();
        fs::create_dir_all(ws.path().join("tmp")).unwrap();
        for entry in fs::read_dir(&source).unwrap() {
            let path = entry.unwrap().path();
            fs::copy(&path, ws.path().join("pkg").join(path.file_name().unwrap())).unwrap();
        }
        fs::write(
            ws.path().join("MODULE.bazel"),
            "module(name = \"artifact_identity\")\n",
        )
        .unwrap();
        ws
    }

    fn path(&self) -> &Path {
        self.0.path()
    }

    fn build(&self, args: &[&str]) -> Output {
        self.build_env(&[], args)
    }

    fn build_env(&self, env: &[(&str, &str)], args: &[&str]) -> Output {
        let output = Command::new("timeout")
            .args(["--kill-after=5s", "600s", env!("CARGO_BIN_EXE_bazel-rs")])
            .args([
                "--ignore_all_rc_files",
                "--cache-dir=private-cache",
                "build",
            ])
            .args(args)
            .current_dir(self.path())
            .env_clear()
            .envs(verifier_environment())
            .env("PATH", "/usr/bin:/bin")
            .env("HOME", self.path())
            .env("TMPDIR", self.path().join("tmp"))
            .env("BAZEL_RS_VERIFY_ARTIFACTS", "1")
            .env("BAZEL_RS_VERIFY_VIEWS", "1")
            .envs(env.iter().copied())
            .explicit_modules()
            .output()
            .unwrap();
        assert_ne!(output.status.code(), Some(124), "CLI timed out: {args:?}");
        output
    }
}

fn stderr(output: &Output) -> String {
    without_print_locations(&String::from_utf8_lossy(&output.stderr))
}

/// The reported outputs of a successful build: (label, root-relative path,
/// contents), in report order (by label).
fn outputs(output: &Output) -> Vec<(String, String, String)> {
    assert!(output.status.success(), "{}", stderr(output));
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .map(|line| {
            let (label, path) = line.split_once('\t').unwrap();
            let relative = path
                .split_once("/outputs/")
                .map_or(path, |(_, relative)| relative);
            let contents = fs::read_to_string(path).unwrap_or_default();
            (label.to_owned(), relative.to_owned(), contents)
        })
        .collect()
}

fn row(label: &str, path: &str, contents: &str) -> (String, String, String) {
    (label.to_owned(), path.to_owned(), contents.to_owned())
}

#[test]
fn provider_file_three_hops_away_is_an_input() {
    let ws = Workspace::new();
    // use -> forward -> hold -> leaf: the leaf's File inside a provider the
    // two middle targets wrap and forward.
    assert_eq!(
        outputs(&ws.build(&["//pkg:use"])),
        [row(
            "//pkg:use",
            "bazel-out/k8-fastbuild/bin/pkg/use.txt",
            "leaf:leaf\n"
        )]
    );
}

#[test]
fn closure_files_and_unmodelled_provider_payloads_are_accepted() {
    let ws = Workspace::new();
    // A File reachable only through a closure is an input and a valid
    // `declare_file(sibling = ...)` anchor; provider payloads holding a
    // range, 200 nested lists, a cyclic list or a bound builtin method are
    // ordinary values (the former provider-file walk rejected them:
    // unsupported provider payload type range/builtin_function_or_method,
    // nesting depth limit 128, cycle in provider payload).
    let built = outputs(&ws.build(&[
        "//pkg:via_closure",
        "//pkg:via_range",
        "//pkg:via_deep",
        "//pkg:via_cyclic",
        "//pkg:via_method",
    ]));
    assert_eq!(
        built,
        [
            row(
                "//pkg:via_closure",
                "bazel-out/k8-fastbuild/bin/pkg/via_closure.txt",
                "hidden:closure\n"
            ),
            row(
                "//pkg:via_closure",
                "bazel-out/k8-fastbuild/bin/pkg/via_closure.sibling",
                "sibling of closure.hidden\n"
            ),
            row(
                "//pkg:via_cyclic",
                "bazel-out/k8-fastbuild/bin/pkg/via_cyclic.txt",
                "hidden:closure_cyclic\n"
            ),
            row(
                "//pkg:via_deep",
                "bazel-out/k8-fastbuild/bin/pkg/via_deep.txt",
                "hidden:closure_deep\n"
            ),
            row(
                "//pkg:via_method",
                "bazel-out/k8-fastbuild/bin/pkg/via_method.txt",
                "hidden:closure_method\n"
            ),
            row(
                "//pkg:via_range",
                "bazel-out/k8-fastbuild/bin/pkg/via_range.txt",
                "hidden:closure_range\n"
            ),
        ]
    );
}

#[test]
fn analysis_bound_provider_payloads_fail_explicitly() {
    // Bazel keeps a ctx, its action factory or an Args in a provider
    // (reference_receipt.json); the native profile freezes providers with
    // their analysis and rejects these values with the former walk's text,
    // and a closure capturing one with the freezer's message.
    let ws = Workspace::new();
    for (target, message) in [
        ("//pkg:bound_ctx", "unsupported provider payload type ctx"),
        (
            "//pkg:bound_actions",
            "unsupported provider payload type actions",
        ),
        ("//pkg:bound_args", "unsupported provider payload type Args"),
        (
            "//pkg:bound_closure",
            "unsupported provider payload: Value of type",
        ),
    ] {
        let output = ws.build(&[target]);
        let text = stderr(&output);
        assert_eq!(output.status.code(), Some(1), "{text}");
        assert!(text.contains(message), "{target}: {text}");
        assert!(!text.contains("panicked"), "{text}");
    }
}

#[test]
fn forwarded_default_info_files_resolve_to_their_producer() {
    let ws = Workspace::new();
    assert_eq!(
        outputs(&ws.build(&["//pkg:fwd", "//pkg:cat_fwd"])),
        [
            row(
                "//pkg:cat_fwd",
                "bazel-out/k8-fastbuild/bin/pkg/cat_fwd.txt",
                "generated data\n"
            ),
            row(
                "//pkg:fwd",
                "bazel-out/k8-fastbuild/bin/pkg/gen_data.txt",
                "generated data\n"
            ),
        ]
    );
}

#[test]
fn status_file_in_default_info_needs_no_consuming_action() {
    // The workspace status files are registered by the first target that
    // needs them, here as files to build without any action input
    // (previously `DefaultInfo file has no artifact`).
    let ws = Workspace::new();
    let planned = ws.build(&["--nobuild", "//pkg:status"]);
    assert!(planned.status.success(), "{}", stderr(&planned));
    assert_eq!(
        String::from_utf8_lossy(&planned.stdout),
        "//pkg:status\tbazel-out/volatile-status.txt\n"
    );
    let built = outputs(&ws.build(&["//pkg:status"]));
    assert_eq!(built.len(), 1);
    assert_eq!(built[0].1, "bazel-out/volatile-status.txt");
    assert!(built[0].2.starts_with("BUILD_TIMESTAMP "), "{built:?}");
}

/// The stdout of a successful `--nobuild` run (the planned outputs).
fn planned(ws: &Workspace, args: &[&str]) -> String {
    let mut all = vec!["--nobuild"];
    all.extend_from_slice(args);
    let output = ws.build(&all);
    assert!(output.status.success(), "{args:?}: {}", stderr(&output));
    String::from_utf8_lossy(&output.stdout).into_owned()
}

// A workspace status file reached only through runfiles, an output group or
// a top-level aspect's outputs resolves on its own, under `build` and
// `--nobuild` alike: the lookup that reaches it registers the status files.
// Before, it resolved only when another target of the same build had
// registered them (an action input or a DefaultInfo file of its own), so the
// result depended on unrelated targets and `--nobuild` disagreed with `build`
// (`runfiles file has no declared artifact identity: bazel-out/volatile-status.txt`,
// `output group 'status' file has no declared artifact identity: ...`).
// Genuine Bazel 9.3 builds every row (reference_receipt.json, `status_*`).

#[test]
fn status_file_in_an_executables_own_runfiles_resolves_alone() {
    let ws = Workspace::new();
    assert_eq!(
        planned(&ws, &["//pkg:status_exe"]),
        "//pkg:status_exe\tbazel-out/k8-fastbuild/bin/pkg/status_exe.sh\n"
    );
    let built = ws.build_env(&[("BAZEL_RS_DUMP_ACTIONS", "1")], &["//pkg:status_exe"]);
    let text = stderr(&built);
    // The runfiles manifest takes the planned status artifact.
    assert!(
        text.contains(
            "ACTION //pkg:status_exe#runfiles_manifest\n\
             \x20 generated //pkg:status_exe#0[0] -> bazel-out/k8-fastbuild/bin/pkg/status_exe.sh\n\
             \x20 generated BazelWorkspaceStatusAction#volatile[0] -> bazel-out/volatile-status.txt\n"
        ),
        "{text}"
    );
    assert_eq!(
        outputs(&built),
        [row(
            "//pkg:status_exe",
            "bazel-out/k8-fastbuild/bin/pkg/status_exe.sh",
            "#!/bin/sh\necho status_exe\n"
        )]
    );
}

#[test]
fn status_file_in_dependency_runfiles_resolves_alone() {
    let ws = Workspace::new();
    // Merged into an executable's runfiles (its runfiles support).
    assert_eq!(
        planned(&ws, &["//pkg:status_wrap"]),
        "//pkg:status_wrap\tbazel-out/k8-fastbuild/bin/pkg/status_wrap.sh\n"
    );
    assert_eq!(
        outputs(&ws.build(&["//pkg:status_wrap"])),
        [row(
            "//pkg:status_wrap",
            "bazel-out/k8-fastbuild/bin/pkg/status_wrap.sh",
            "#!/bin/sh\necho status_wrap\n"
        )]
    );
    // The library itself: its runfiles are built with it
    // (OutputGroupInfo.HIDDEN_TOP_LEVEL).
    assert_eq!(
        planned(&ws, &["//pkg:status_lib"]),
        "//pkg:status_lib\tbazel-out/k8-fastbuild/bin/pkg/status_lib.txt\n"
    );
    assert_eq!(
        outputs(&ws.build(&["//pkg:status_lib"])),
        [row(
            "//pkg:status_lib",
            "bazel-out/k8-fastbuild/bin/pkg/status_lib.txt",
            "lib:status_lib\n"
        )]
    );
}

#[test]
fn status_file_in_an_output_group_resolves_alone() {
    let ws = Workspace::new();
    let group = ["--output_groups=status", "//pkg:status_group"];
    assert_eq!(
        planned(&ws, &group),
        "//pkg:status_group\tbazel-out/stable-status.txt\n"
    );
    let built = outputs(&ws.build(&group));
    assert_eq!(built.len(), 1, "{built:?}");
    assert_eq!(built[0].1, "bazel-out/stable-status.txt");
    assert!(built[0].2.starts_with("BUILD_EMBED_LABEL "), "{built:?}");
    // A top-level aspect's output group.
    let aspect = [
        "--aspects=//pkg:defs.bzl%status_aspect",
        "--output_groups=aspect_status",
        "//pkg:gen",
    ];
    let mut all = vec!["--nobuild"];
    all.extend_from_slice(&aspect);
    let planned = ws.build(&all);
    let text = stderr(&planned);
    assert!(planned.status.success(), "{text}");
    assert!(
        text.contains(
            "Aspect //pkg:defs.bzl%status_aspect of //pkg:gen up-to-date:\n  bazel-out/volatile-status.txt\n"
        ),
        "{text}"
    );
    let built = ws.build(&aspect);
    let text = stderr(&built);
    assert!(built.status.success(), "{text}");
    assert!(
        text.contains("Aspect //pkg:defs.bzl%status_aspect of //pkg:gen up-to-date:\n  ")
            && text.contains("/outputs/bazel-out/volatile-status.txt\n"),
        "{text}"
    );
}

/// What //pkg:status_report says about the workspace status Files it
/// reaches through //pkg:status_files (whose DefaultInfo returns its own
/// ctx.info_file and ctx.version_file) and through a filegroup forwarding
/// it, against its own ctx.info_file / ctx.version_file: genuine Bazel
/// 9.3's report, the same under build and --nobuild
/// (reference_receipt.json `status_files_of_a_dependency*`).
fn status_report() -> String {
    let line = |how: &str, name: &str, stable: bool| {
        let (info, version) = if stable {
            ("True", "False")
        } else {
            ("False", "True")
        };
        format!(
            "{how} path=bazel-out/{name} short_path={name} is_source=False root=bazel-out owner=None equal_info_file={info} equal_version_file={version} in_own_set=True\n"
        )
    };
    let mut report = String::new();
    for how in [
        "own",
        "ctx.files.dep",
        "dep[DefaultInfo]",
        "dep.files",
        "ctx.files.group",
        "group[DefaultInfo]",
        "group.files",
    ] {
        report += &line(how, "stable-status.txt", true);
        report += &line(how, "volatile-status.txt", false);
    }
    report + "set_size=2\n"
}

/// The `status_report` lines a build printed (its analysis-time report).
fn printed_status_report(output: &Output) -> String {
    stderr(output)
        .lines()
        .filter_map(|line| line.strip_prefix("status_report "))
        .map(|line| format!("{line}\n"))
        .collect()
}

#[test]
fn status_files_of_a_dependency_are_the_consumers_own() {
    // A workspace status file a dependency returns in DefaultInfo reaches
    // the consumer as the consumer's own ctx.info_file / ctx.version_file,
    // through ctx.files, dep[DefaultInfo] and Target.files, also forwarded
    // by a filegroup: short_path `stable-status.txt`, a derived File of the
    // `bazel-out` root, equal, one depset element — under --nobuild too
    // (no status action). The dependency view used to hand out the status
    // artifact's computed File instead: short_path
    // `bazel-out/stable-status.txt`, root "", a source under --nobuild, not
    // equal to ctx.info_file (set_size=4).
    let ws = Workspace::new();
    let planned = ws.build(&["--nobuild", "//pkg:status_report"]);
    assert!(planned.status.success(), "{}", stderr(&planned));
    assert_eq!(printed_status_report(&planned), status_report());
    assert_eq!(
        String::from_utf8_lossy(&planned.stdout),
        "//pkg:status_report\tbazel-out/k8-fastbuild/bin/pkg/status_report.txt\n"
    );
    let built = ws.build_env(&[("BAZEL_RS_DUMP_ACTIONS", "1")], &["//pkg:status_report"]);
    let text = stderr(&built);
    assert_eq!(printed_status_report(&built), status_report());
    // Every view's File is an input resolving to the status action.
    assert!(
        text.contains(
            "  command printf '%s\\n' \"$1\" > \"$2\"\n\
             \x20 generated BazelWorkspaceStatusAction#stable[0] -> bazel-out/stable-status.txt\n\
             \x20 generated BazelWorkspaceStatusAction#volatile[0] -> bazel-out/volatile-status.txt\n\
             \x20 output bazel-out/k8-fastbuild/bin/pkg/status_report.txt\n"
        ),
        "{text}"
    );
    assert_eq!(
        outputs(&built),
        [row(
            "//pkg:status_report",
            "bazel-out/k8-fastbuild/bin/pkg/status_report.txt",
            &status_report()
        )]
    );
}

#[test]
fn runfiles_manifest_input_resolves_to_its_planned_node() {
    // files_to_run.runfiles_manifest is the File of a planned runfiles node
    // (the tree MANIFEST under --build_runfile_links), so as an action input
    // it resolves to that node. Without runfile manifests no node is planned
    // and the field is None, as in Bazel: no File without an Artifact
    // reaches the consumer (`input has no declared artifact` stays the
    // error of a File no producer registered).
    let ws = Workspace::new();
    let planned = ws.build_env(
        &[("BAZEL_RS_DUMP_ACTIONS", "1")],
        &["--nobuild", "//pkg:use_manifest"],
    );
    let text = stderr(&planned);
    assert!(planned.status.success(), "{text}");
    assert!(
        text.contains(
            "  generated //pkg:tool#runfiles_tree[0] -> bazel-out/k8-fastbuild/bin/pkg/tool.sh.runfiles/MANIFEST\n\
             \x20 output bazel-out/k8-fastbuild/bin/pkg/use_manifest.txt\n"
        ),
        "{text}"
    );
    let built = outputs(&ws.build(&["//pkg:use_manifest"]));
    assert_eq!(built.len(), 1, "{built:?}");
    assert_eq!(
        built[0].1,
        "bazel-out/k8-fastbuild/bin/pkg/use_manifest.txt"
    );
    let entries = built[0]
        .2
        .lines()
        .map(|line| line.split(' ').next().unwrap())
        .collect::<Vec<_>>();
    assert_eq!(
        entries,
        [
            "_main/pkg/gen_data.txt",
            "_main/pkg/tool.sh",
            "_repo_mapping"
        ]
    );
    let output = ws.build(&["--nobuild_runfile_manifests", "//pkg:use_manifest"]);
    let text = stderr(&output);
    assert_eq!(output.status.code(), Some(1), "{text}");
    assert!(
        text.contains("'NoneType' value has no field or method 'path'"),
        "{text}"
    );
    assert!(!text.contains("has no declared artifact"), "{text}");
}

#[test]
fn declared_output_of_a_dependency_path_is_an_output_conflict() {
    // Two targets declaring one output path with different actions (each
    // writes its own name) conflict when the second registers it, with
    // Bazel's action conflict (the receipt's genuine first line `file
    // 'pkg/same.txt' is generated by these conflicting actions:`; equal
    // actions would be shared, tests/shared_actions.rs); the former
    // per-dependency scan in declare_file is gone with the dependency file
    // sets.
    let ws = Workspace::new();
    let output = ws.build(&["//pkg:second"]);
    let text = stderr(&output);
    assert!(!output.status.success(), "unexpected success:\n{text}");
    assert!(
        text.contains(
            "file 'pkg/same.txt' is generated by these conflicting actions:\nLabel: //pkg:second, //pkg:first"
        ),
        "{text}"
    );
    assert!(
        !text.contains("conflicts with a dependency artifact"),
        "{text}"
    );
}

#[test]
fn generated_tool_runfile_is_staged_from_its_producer() {
    let ws = Workspace::new();
    let planned = ws.build_env(
        &[("BAZEL_RS_DUMP_ACTIONS", "1")],
        &["--nobuild", "//pkg:run_tool"],
    );
    let text = stderr(&planned);
    assert!(planned.status.success(), "{text}");
    assert!(
        text.contains(
            "ACTION //pkg:run_tool#0\n\
             \x20 program bazel-out/k8-fastbuild/bin/pkg/tool.sh\n\
             \x20 arguments bazel-out/k8-fastbuild/bin/pkg/run_tool.txt\n\
             \x20 generated //pkg:tool#0[0] -> bazel-out/k8-fastbuild/bin/pkg/tool.sh\n\
             \x20 generated //pkg:gen#0[0] -> bazel-out/k8-fastbuild/bin/pkg/tool.sh.runfiles/_main/pkg/gen_data.txt\n\
             \x20 generated //pkg:tool#0[0] -> bazel-out/k8-fastbuild/bin/pkg/tool.sh.runfiles/_main/pkg/tool.sh\n\
             \x20 generated //pkg:tool#repo_mapping[0] -> bazel-out/k8-fastbuild/bin/pkg/tool.sh.runfiles/_repo_mapping\n\
             \x20 output bazel-out/k8-fastbuild/bin/pkg/run_tool.txt\n"
        ),
        "{text}"
    );
    assert_eq!(
        outputs(&ws.build(&["//pkg:run_tool"])),
        [row(
            "//pkg:run_tool",
            "bazel-out/k8-fastbuild/bin/pkg/run_tool.txt",
            "generated data\n"
        )]
    );
}
