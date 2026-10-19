//! N58 Starlark aspects (docs/ASPECTS.md) over the shared fixture
//! tests/fixtures/aspects: definition and propagation semantics, the
//! aspect-configured target's merged providers (MergedConfiguredTarget),
//! aspect-on-aspect through `required_aspect_providers`, `--aspects` (with
//! `--aspects_parameters` and the native ValidateTarget of
//! `--experimental_use_validation_aspect`), the aspect context
//! (`ctx.attr` of the aspect's own attributes, `ctx.rule` over the base
//! target's, `ctx.aspect_ids`) and aspect-declared features that stay
//! explicit gaps. Expected texts are genuine Bazel 9.3 strings (paired
//! capture tests/fixtures/aspects/reference_receipt.json).
#![allow(clippy::disallowed_methods)] // test harness: starts processes directly (clippy.toml)
use std::fs;
use std::path::Path;
use std::process::{Command, Output};

struct Workspace(tempfile::TempDir);

impl Workspace {
    fn path(&self) -> &Path {
        self.0.path()
    }

    fn new() -> Self {
        let ws = Self(
            tempfile::Builder::new()
                .prefix("aspects-")
                .tempdir_in("/tmp/opencode")
                .unwrap(),
        );
        let fixture =
            Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/aspects/workspace");
        copy(&fixture, ws.path());
        ws
    }

    fn build_with(&self, flags: &[&str], targets: &[&str]) -> Output {
        Command::new("timeout")
            .args(["--kill-after=5s", "120s", env!("CARGO_BIN_EXE_bazel-rs")])
            .arg("--ignore_all_rc_files")
            .arg("--cache-dir=cache")
            .arg("build")
            .args(flags)
            .args(targets)
            .current_dir(self.path())
            .output()
            .unwrap()
    }

    fn run(&self, arguments: &[&str]) -> Output {
        Command::new("timeout")
            .args(["--kill-after=5s", "120s", env!("CARGO_BIN_EXE_bazel-rs")])
            .arg("--ignore_all_rc_files")
            .arg("--cache-dir=cache")
            .args(arguments)
            .current_dir(self.path())
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
            collect(&entry.path().join("outputs"), &mut names);
        }
        names.sort();
        names
    }

    /// The bytes of one bazel-bin file this workspace materialized.
    fn output(&self, name: &str) -> String {
        let actions = self.path().join("cache/actions");
        let mut stack = vec![actions];
        while let Some(dir) = stack.pop() {
            let Ok(entries) = fs::read_dir(&dir) else {
                continue;
            };
            for entry in entries.flatten() {
                let path = entry.path();
                if path.is_dir() {
                    stack.push(path);
                } else if path.file_name().is_some_and(|file| file == name) {
                    return fs::read_to_string(&path).unwrap();
                }
            }
        }
        panic!("no materialized output {name}");
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
        } else if let Some(relative) = path
            .ancestors()
            .find(|path| path.file_name().is_some_and(|part| part == "bazel-out"))
            .and_then(|base| path.strip_prefix(base).ok())
        {
            names.push(relative.to_string_lossy().into_owned());
        }
    }
}

#[allow(clippy::needless_borrow)]
fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

#[allow(clippy::needless_borrow)]
fn ok(output: &Output) -> String {
    assert!(output.status.success(), "{}", stderr(&output));
    stderr(&output)
}

#[allow(clippy::needless_borrow)]
fn rejected(output: &Output, message: &str) {
    let text = stderr(&output);
    assert!(!output.status.success(), "unexpected success:\n{text}");
    assert!(text.contains(message), "expected {message:?} in:\n{text}");
}

#[test]
fn attribute_aspects_propagate_and_merge_providers() {
    // N01: `attr.label_list(aspects = [...])` applies every aspect to the
    // attribute's dependencies, the aspects propagate along `attr_aspects`
    // attributes of the dependencies (a, b, c) and the consumer reads the
    // merged providers (aspect providers shadow the base target's).
    let ws = Workspace::new();
    let text = ok(&ws.build_with(&[], &["//:app"]));
    assert_eq!(
        ws.output("app.c"),
        // count_aspect counts 1 + the deps' counts (3 along the chain),
        // upper_aspect wraps FileInfo of its required count_aspect.
        "count=3 note=un"
    );
    // The aspects ran on the whole `deps` closure, in dependency order.
    let mut lines = text
        .lines()
        .filter(|line| line.starts_with("ASPECT ") || line.starts_with("UPPER "))
        .map(str::to_owned)
        .collect::<Vec<_>>();
    lines.sort();
    assert_eq!(
        lines,
        [
            "ASPECT @@//:a kind=lib extension=cnt rule-attr=[]",
            "ASPECT @@//:b kind=lib extension=cnt rule-attr=[\"a\"]",
            "ASPECT @@//:c kind=lib extension=cnt rule-attr=[\"b\"]",
            "UPPER @@//:c sees=n ids=[\"//:defs.bzl%count_aspect\", \"//:defs.bzl%upper_aspect\"]",
        ]
    );
    // Only the aspect outputs in requested groups are built: the aspect_out
    // group is not requested, so nothing extra materializes.
    assert_eq!(ws.outputs(), ["k8-fastbuild/bin/app.c"]);
}

#[test]
fn upper_aspect_does_not_propagate_without_attr_aspects() {
    // N02: an aspect without `attr_aspects` applies where attached only
    // (upper_aspect to //:c, not to its dependencies).
    let ws = Workspace::new();
    let text = ok(&ws.build_with(&[], &["//:app"]));
    assert_eq!(
        text.lines()
            .filter(|line| line.starts_with("UPPER"))
            .count(),
        1,
        "{text}"
    );
}

#[test]
fn top_level_aspects_build_and_report_aspect_outputs() {
    // N03: `--aspects` applies the aspect to the top-level target and its
    // `attr_aspects` closure; `--output_groups` selects the aspect's group
    // from the merged OutputGroupInfo, reported per aspect.
    let ws = Workspace::new();
    let text = ok(&ws.build_with(
        &[
            "--aspects=//:defs.bzl%count_aspect",
            "--output_groups=aspect_out",
        ],
        &["//:b"],
    ));
    assert!(
        text.contains("Aspect //:defs.bzl%count_aspect of //:b up-to-date:"),
        "{text}"
    );
    assert_eq!(ws.outputs(), ["k8-fastbuild/bin/b.cnt"]);
}

#[test]
fn top_level_aspect_parameters_and_legacy_labels() {
    // N04: a relative --aspects label converts to the absolute .bzl label
    // (BuildView's legacy behavior); --aspects_parameters sets a parameter
    // attribute of the top-level aspect.
    let ws = Workspace::new();
    ok(&ws.build_with(&["--aspects=defs.bzl%count_aspect"], &["//:a"]));
    assert_eq!(ws.outputs(), ["k8-fastbuild/bin/a.lib"]);
    let ws = Workspace::new();
    let text = ok(&ws.build_with(
        &[
            "--aspects=//:defs.bzl%count_aspect",
            "--aspects_parameters=extension=out",
            "--output_groups=aspect_out",
        ],
        &["//:a"],
    ));
    assert!(
        text.contains("Aspect //:defs.bzl%count_aspect of //:a up-to-date:"),
        "{text}"
    );
    assert_eq!(ws.outputs(), ["k8-fastbuild/bin/a.out"]);
}

#[test]
fn required_providers_filter_the_application() {
    // N05: an aspect whose `required_providers` the target does not
    // advertise is silently not applied (computeAspectCollection).
    let ws = Workspace::new();
    let text = ok(&ws.build_with(
        &[
            "--aspects=//:defs.bzl%need_aspect",
            "--output_groups=need_group",
        ],
        &["//:agg"],
    ));
    assert!(!text.contains("Aspect"), "{text}");
    // The output-group override dropped the default group: nothing of
    // //:agg itself is requested (TopLevelArtifactHelper skips empty
    // groups), and the aspect did not apply to it.
    assert_eq!(ws.outputs(), Vec::<String>::new());
}

#[test]
fn provider_provided_twice_fails_the_merge() {
    // N06: an aspect returning a provider the base target (or another
    // aspect) also provides fails the merge (MergedConfiguredTarget's
    // MergingException).
    let ws = Workspace::new();
    rejected(
        &ws.build_with(&[], &["//:dupconsumer"]),
        "Provider FileInfo provided twice",
    );
}

#[test]
fn aspect_actions_declare_outputs_under_the_aspect() {
    // N07: aspect actions register under the application's own action owner
    // (label%aspect), so the base target's and several aspects' outputs
    // never collide; a second aspect may consume the first's outputs.
    let ws = Workspace::new();
    let text = ok(&ws.build_with(
        &[
            "--aspects=//:defs.bzl%count_aspect,//:defs.bzl%files_aspect",
            "--output_groups=aspect_out,files_group",
        ],
        &["//:b"],
    ));
    // Two aspects with artifacts exceed the --show_result budget (default
    // 1): no aspect result line prints at all (outputTargetsAndAspects).
    assert!(!text.contains("Aspect"), "{text}");
    assert_eq!(
        ws.output("b.files"),
        // files_aspect reads target[DefaultInfo].files of the merged
        // aspect-configured target: the base target's files to build.
        "b.lib"
    );
    assert_eq!(
        ws.outputs(),
        ["k8-fastbuild/bin/b.cnt", "k8-fastbuild/bin/b.files"]
    );
}

#[test]
fn aspect_own_attributes_are_dependencies() {
    // N08: the aspect's implicit label attributes resolve as dependencies
    // from the aspect's .bzl package (ctx.file._tool).
    let ws = Workspace::new();
    ok(&ws.build_with(
        &[
            "--aspects=//:defs.bzl%tool_aspect",
            "--output_groups=tool_group",
        ],
        &["//:a"],
    ));
    assert_eq!(ws.output("a.withtool"), "aspect_tool.tool");
}

#[test]
fn output_file_edges_apply_generating_rules_only_when_asked() {
    // N09: an aspect along an output-file edge applies to the generating
    // rule only with apply_to_generating_rules (AspectDefinition's
    // applyToGeneratingRules); the consumer reads the aspect's providers
    // through the aspect-configured dependency. (Both edges name //:gg.gen1,
    // and the native dependency map is keyed per configured target, so both
    // attributes see the union of their aspects — the one that applies.)
    let ws = Workspace::new();
    ok(&ws.build_with(&[], &["//:use"]));
    assert_eq!(ws.output("use.which"), "dep1=generating dep2=generating");
    assert_eq!(ws.outputs(), ["k8-fastbuild/bin/use.which"]);
}

#[test]
fn validation_aspect_path_promotes_validation_outputs() {
    // N10: --experimental_use_validation_aspect requests
    // `_validation_top_level` through the native ValidateTarget (appended
    // to the top-level aspects), which promotes the merged `_validation`
    // outputs (the rule's and the aspects'), built but never reported.
    let ws = Workspace::new();
    let text = ok(&ws.build_with(
        &[
            "--aspects=//:defs.bzl%diag_aspect",
            "--experimental_use_validation_aspect",
        ],
        &["//:eat"],
    ));
    assert!(!text.contains("Aspect ValidateTarget"), "{text}");
    assert_eq!(
        ws.outputs(),
        [
            "k8-fastbuild/bin/eat.diag",
            "k8-fastbuild/bin/leafval.validated",
        ]
    );
    let ws = Workspace::new();
    let text = ok(&ws.build_with(&[], &["//:eat"]));
    assert_eq!(ws.outputs(), ["k8-fastbuild/bin/leafval.validated"]);
    assert!(!text.contains("Aspect"), "{text}");
}

#[test]
fn aspect_declaration_gaps_fail_explicitly() {
    // N11: aspect-declared features whose application is not modelled fail
    // loudly (never silently ignored).
    let ws = Workspace::new();
    rejected(
        &ws.build_with(&["--aspects=//:defs.bzl%fragments_aspect"], &["//:a"]),
        "aspect //:defs.bzl%fragments_aspect declares fragments",
    );
}

#[test]
fn top_level_aspect_errors_use_bazel_texts() {
    // N12: unknown aspects, non-aspect exports, unknown parameters and
    // invalid labels fail with Bazel's exact texts.
    let ws = Workspace::new();
    rejected(
        &ws.build_with(&["--aspects=NoSuchAspect"], &["//:a"]),
        "Aspect 'NoSuchAspect' is unknown",
    );
    let ws = Workspace::new();
    rejected(
        &ws.build_with(&["--aspects=//:defs.bzl%lib"], &["//:a"]),
        "lib from //:defs.bzl is not an aspect",
    );
    let ws = Workspace::new();
    rejected(
        &ws.build_with(
            &[
                "--aspects=//:defs.bzl%count_aspect",
                "--aspects_parameters=nosuch=1",
            ],
            &["//:a"],
        ),
        "Parameters '[nosuch]' are not parameters of any of the top-level aspects but they are specified in --aspects_parameters.",
    );
    let ws = Workspace::new();
    rejected(
        &ws.build_with(&["--aspects=//:no_such_file.bzl%x"], &["//:a"]),
        "cannot load '//:no_such_file.bzl': no such file",
    );
}

#[test]
fn query_still_lists_aspect_attribute_edges() {
    // N13: the unconfigured query keeps listing attribute aspect edges
    // (LabelVisitor.visitAspectsIfRequired, unchanged by N58's analysis).
    let output = Workspace::new().run(&["query", "--noimplicit_deps", "deps(//:app)"]);
    assert!(output.status.success(), "{}", stderr(&output));
    assert_eq!(
        String::from_utf8_lossy(&output.stdout),
        "//:a\n//:app\n//:b\n//:c\n"
    );
}
