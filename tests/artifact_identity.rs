//! W1b (PLAN-L1 §6): the engine resolves every File through the build's one
//! artifact index, as Bazel resolves an action input, a DefaultInfo file or a
//! runfile through its Artifact, however the File reached the consumer (a
//! provider three hops away, a closure, a forwarded DefaultInfo, a tool's
//! runfiles). Programs the former legality machinery rejected and Bazel
//! accepts now build; the paired genuine Bazel 9.3 capture is
//! tests/fixtures/artifact_identity/reference_receipt.json. Every build runs
//! with `BAZEL_RS_VERIFY_ARTIFACTS=1` (no File re-registered with a different
//! Artifact; until W1d it also re-ran the former provider-file walk after
//! each commit and asserted every File it reached was registered).
#![allow(clippy::disallowed_methods)] // test harness: starts processes directly (clippy.toml)
use std::fs;
use std::path::Path;
use std::process::{Command, Output};

struct Workspace(tempfile::TempDir);

impl Workspace {
    fn new() -> Self {
        let ws = Self(
            tempfile::Builder::new()
                .prefix("artifact-identity-")
                .tempdir_in("/tmp/opencode")
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
            .args(["--kill-after=5s", "120s", env!("CARGO_BIN_EXE_bazel-rs")])
            .args([
                "--ignore_all_rc_files",
                "--cache-dir=private-cache",
                "build",
            ])
            .args(args)
            .current_dir(self.path())
            .env_clear()
            .env("PATH", "/usr/bin:/bin")
            .env("HOME", self.path())
            .env("TMPDIR", self.path().join("tmp"))
            .env("BAZEL_RS_VERIFY_ARTIFACTS", "1")
            .envs(env.iter().copied())
            .output()
            .unwrap();
        assert_ne!(output.status.code(), Some(124), "CLI timed out: {args:?}");
        output
    }
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
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

#[test]
fn declared_output_of_a_dependency_path_is_an_output_conflict() {
    // Two targets declaring one output path conflict when the second
    // registers it (Bazel: an action conflict, `file 'pkg/same.txt' is
    // generated by these conflicting actions`); the former per-dependency
    // scan in declare_file is gone with the dependency file sets.
    let ws = Workspace::new();
    let output = ws.build(&["//pkg:second"]);
    let text = stderr(&output);
    assert!(!output.status.success(), "unexpected success:\n{text}");
    assert!(
        text.contains(
            "overlapping output paths: bazel-out/k8-fastbuild/bin/pkg/same.txt from //pkg:first conflicts with bazel-out/k8-fastbuild/bin/pkg/same.txt from //pkg:second"
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
