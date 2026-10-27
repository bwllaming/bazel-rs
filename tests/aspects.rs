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
include!("support/explicit_modules.rs");
include!("support/console.rs");
include!("support/module_archives.rs");

struct Workspace(tempfile::TempDir);

impl Workspace {
    fn path(&self) -> &Path {
        self.0.path()
    }

    fn new() -> Self {
        Self::fixture("aspects")
    }

    /// A fresh copy of tests/fixtures/NAME/workspace.
    fn fixture(name: &str) -> Self {
        let ws = Self(
            tempfile::Builder::new()
                .prefix("aspects-")
                .tempdir_in(crate::test_temp::root())
                .unwrap(),
        );
        let fixture = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures")
            .join(name)
            .join("workspace");
        copy(&fixture, ws.path());
        ws
    }

    /// tests/fixtures/aspect_toolchains: the genuine platforms 1.0.0 module
    /// extracted into `platforms` (its MODULE.bazel's local_path_override),
    /// and N30's builtin-closure stubs as `local_path_override`s, which keep
    /// the built-in profile offline (no closure module registers a
    /// toolchain of the fixture's types; tests/exec_groups.rs's approach).
    fn toolchains_fixture() -> Self {
        Self::toolchains_fixture_named("aspect_toolchains")
    }

    /// [`Self::toolchains_fixture`] over tests/fixtures/NAME/workspace.
    fn toolchains_fixture_named(name: &str) -> Self {
        use std::io::Read as _;
        let ws = Self::fixture(name);
        let manifest = Path::new(env!("CARGO_MANIFEST_DIR"));
        let archive: serde_json::Value = serde_json::from_slice(
            &fs::read(manifest.join("tests/fixtures/label_repo_name/platforms_archive.json"))
                .unwrap(),
        )
        .unwrap();
        let bytes = {
            use base64::Engine as _;
            base64::engine::general_purpose::STANDARD
                .decode(archive["bytes_base64"].as_str().unwrap())
                .unwrap()
        };
        let platforms = ws.path().join("platforms");
        let mut tar = tar::Archive::new(flate2::read::GzDecoder::new(bytes.as_slice()));
        for entry in tar.entries().unwrap() {
            let mut entry = entry.unwrap();
            let path = platforms.join(entry.path().unwrap());
            if entry.header().entry_type().is_dir() {
                fs::create_dir_all(&path).unwrap();
                continue;
            }
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            let mut content = Vec::new();
            entry.read_to_end(&mut content).unwrap();
            fs::write(path, content).unwrap();
        }
        let mut module = fs::read_to_string(ws.path().join("MODULE.bazel")).unwrap();
        for name in [
            "apple_support",
            "buildozer",
            "protobuf",
            "rules_cc",
            "rules_java",
            "rules_python",
            "rules_shell",
            "zlib",
        ] {
            module.push_str(&format!(
                "local_path_override(module_name = '{name}', path = '{}')\n",
                manifest
                    .join("tests/fixtures/test_flags/stubs")
                    .join(name)
                    .display()
            ));
        }
        fs::write(ws.path().join("MODULE.bazel"), module).unwrap();
        ws
    }

    /// `build` under the built-in module profile (the default host
    /// platform's exec configurations, rules_cc's toolchain types) with
    /// the fixture registry, as the paired captures run it.
    fn build_builtin(&self, flags: &[&str], targets: &[&str]) -> Output {
        // Offline: the platforms module (the default host platform) comes
        // from the seeded repository download cache.
        seed_module_archives(&self.path().join("cache"), &["platforms@1.0.0"]);
        let registry = builtin_registry_flag();
        let mut all = vec!["--experimental-builtin-modules", registry.as_str()];
        all.extend_from_slice(flags);
        self.build_with(&all, targets)
    }

    /// The materialized outputs whose names end with `suffix`.
    fn outputs_ending(&self, suffix: &str) -> Vec<String> {
        self.outputs()
            .into_iter()
            .filter(|name| name.ends_with(suffix))
            .collect()
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
            .env("BAZEL_RS_VERIFY_VIEWS", "1")
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
            .env("BAZEL_RS_VERIFY_VIEWS", "1")
            .explicit_modules()
            .output()
            .unwrap()
    }

    /// The bazel-bin-relative names this workspace materialized. The
    /// workspace status files (bazel-out/{stable,volatile}-status.txt,
    /// materialized by every executing build since the N63 follow-up)
    /// are not target outputs and are excluded.
    fn outputs(&self) -> Vec<String> {
        let mut names = Vec::new();
        let actions = self.path().join("cache/actions");
        let Ok(entries) = fs::read_dir(actions) else {
            return names;
        };
        for entry in entries.flatten() {
            collect(&entry.path().join("outputs"), &mut names);
        }
        names.retain(|name| !matches!(name.as_str(), "stable-status.txt" | "volatile-status.txt"));
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
    without_print_locations(&String::from_utf8_lossy(&output.stderr))
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
fn instrumented_files_info_of_an_aspect_wins_over_the_base_target() {
    // N14 (MergedConfiguredTarget.java:296-305): when InstrumentedFilesInfo
    // is on both the base target and an aspect, the base's copy is ignored
    // — the aspect's wins, without a duplicate-provider error (a rule and a
    // related aspect may share the instrumentation implementation). Two
    // aspects both returning it still fail the merge. Genuine-verified:
    // build //:covconsumer succeeds, //:covtwo fails
    // `BUILD.bazel:5:12: Provider InstrumentedFilesInfo provided twice`.
    let ws = Workspace::new();
    let text = ok(&ws.build_with(&[], &["//:covconsumer"]));
    assert!(!text.contains("provided twice"), "{text}");
    assert_eq!(ws.outputs(), ["k8-fastbuild/bin/covconsumer.reads"]);
    let ws = Workspace::new();
    rejected(
        &ws.build_with(&[], &["//:covtwo"]),
        "Provider InstrumentedFilesInfo provided twice",
    );
}

#[test]
fn aspect_declaration_gaps_fail_explicitly() {
    // N11: an aspect's `fragments` are its configuration fragment policy
    // (genuine: the application succeeds, //:a builds a.lib; the receipt's
    // former native gap). The former aspect gaps (`toolchains_aspects`,
    // functions, subrules) are analyzed now (their own tests).
    let ws = Workspace::new();
    ok(&ws.build_with(&["--aspects=//:defs.bzl%fragments_aspect"], &["//:a"]));
    assert_eq!(ws.output("a.lib"), "lib");
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

/// The print() lines of `text` starting with `marker`, sorted.
fn prints(text: &str, marker: &str) -> Vec<String> {
    let mut lines = text
        .lines()
        .filter(|line| line.starts_with(marker))
        .map(str::to_owned)
        .collect::<Vec<_>>();
    lines.sort();
    lines
}

/// `cfg_aspect`'s line for a base target in the target configuration
/// (tests/fixtures/aspect_configuration/reference_receipt.json).
fn target_configured(name: &str) -> String {
    format!(
        "CFG {name} bin=bazel-out/k8-fastbuild/bin mode=fastbuild exec=bazel-out/k8-opt-exec/bin/tool.sh default_exec=bazel-out/k8-opt-exec/bin/tool.sh target=bazel-out/k8-fastbuild/bin/tool.sh transitioned=via-aspect:bazel-out/k8-fastbuild-ST-d828efcc8d19/bin/reader.txt"
    )
}

#[test]
fn aspect_attributes_take_their_own_configurations() {
    // Every label attribute of an aspect is configured by its own `cfg`
    // per application (DependencyProducer over the main aspect's
    // attributes): the exec transition for the ASPECT's execution platform
    // (`cfg = "exec"` and `config.exec()`), the base target's
    // configuration (`cfg = "target"`), a Starlark transition applied to
    // the base configuration (a list in ctx.attr); the aspect's exec tool
    // runs from bazel-out/k8-opt-exec. Genuine Bazel 9.3.0 strings
    // (tests/fixtures/aspect_configuration/reference_receipt.json, MATCH).
    let ws = Workspace::fixture("aspect_configuration");
    let text = ok(&ws.build_builtin(
        &[
            "--aspects=//:defs.bzl%cfg_aspect",
            "--output_groups=cfg_out",
        ],
        &["//:b"],
    ));
    assert_eq!(
        prints(&text, "CFG "),
        [target_configured("a"), target_configured("b")]
    );
    assert_eq!(
        ws.outputs_ending(".cfg"),
        ["k8-fastbuild/bin/a.cfg", "k8-fastbuild/bin/b.cfg"]
    );
    assert_eq!(
        ws.output("b.cfg"),
        "tool=bazel-out/k8-opt-exec/bin/tool.sh base=b\n"
    );
    // Attribute aspects, also on an exec-configured base (//:x through
    // `use`'s exec `tools`): the aspect context's configuration surface is
    // the base target's (ctx.bin_dir/ctx.var), the exec transition of an
    // exec configuration is itself, and the Starlark transition applies on
    // top of it.
    let ws = Workspace::fixture("aspect_configuration");
    let text = ok(&ws.build_builtin(&[], &["//:u"]));
    assert_eq!(
        prints(&text, "CFG "),
        [
            target_configured("a"),
            target_configured("b"),
            "CFG x bin=bazel-out/k8-opt-exec/bin mode=opt exec=bazel-out/k8-opt-exec/bin/tool.sh default_exec=bazel-out/k8-opt-exec/bin/tool.sh target=bazel-out/k8-opt-exec/bin/tool.sh transitioned=via-aspect:bazel-out/k8-opt-exec-ST-d828efcc8d19/bin/reader.txt".to_owned(),
        ]
    );
    assert_eq!(
        ws.outputs_ending(".cfg"),
        [
            "k8-fastbuild/bin/a.cfg",
            "k8-fastbuild/bin/b.cfg",
            "k8-opt-exec/bin/x.cfg"
        ]
    );
    assert_eq!(
        ws.output("x.cfg"),
        "tool=bazel-out/k8-opt-exec/bin/tool.sh base=x\n"
    );
    // --nobuild analyzes the same applications and builds nothing.
    let ws = Workspace::fixture("aspect_configuration");
    let text = ok(&ws.build_builtin(
        &[
            "--nobuild",
            "--aspects=//:defs.bzl%cfg_aspect",
            "--output_groups=cfg_out",
        ],
        &["//:b"],
    ));
    assert_eq!(
        prints(&text, "CFG "),
        [target_configured("a"), target_configured("b")]
    );
    assert_eq!(ws.outputs_ending(".cfg"), Vec::<String>::new());
}

#[test]
fn undeclared_aspect_exec_group_fails_at_the_base_target() {
    // `cfg = config.exec("nope")` on an aspect without that exec group:
    // DependencyProducer.ExecGroupErrorEmitter, at the base target's
    // location, naming the attribute by its native name (genuine
    // `BUILD.bazel:12:4: Attr '$tool' declares a transition for
    // non-existent exec group 'nope'`).
    let ws = Workspace::fixture("aspect_configuration");
    rejected(
        &ws.build_builtin(&["--aspects=//:defs.bzl%bad_group_aspect"], &["//:a"]),
        "BUILD.bazel:12:4: Attr '$tool' declares a transition for non-existent exec group 'nope'",
    );
}

#[test]
fn aspect_context_lists_its_own_attributes() {
    // ctx.attr/ctx.files/ctx.file/ctx.executable of an aspect are its own
    // attributes' (StarlarkRuleContext.java:270-285,
    // StarlarkAttributesCollection.Builder.addAttribute): no `name`, a
    // `ctx.files` entry per dependency attribute (each File once), `ctx.file`
    // for allow_single_file, `ctx.executable` for executable label
    // attributes, whose runfiles a spawn running the File stages. Genuine
    // Bazel 9.3.0 strings (tests/fixtures/aspect_ctx_attributes/
    // reference_receipt.json, MATCH).
    for (flags, param) in [(vec![], "p"), (vec!["--aspects_parameters=param=q"], "q")] {
        let ws = Workspace::fixture("aspect_ctx_attributes");
        let mut all = vec![
            "--aspects=//:defs.bzl%attrs_aspect",
            "--output_groups=exe_out",
        ];
        all.extend(flags);
        let text = ok(&ws.build_builtin(&all, &["//:a"]));
        let mut expected = [
            r#"DIR attr=["_config", "_dict", "_ints", "_list", "_optional", "_target_tool", "_tool", "param"]"#.to_owned(),
            r#"DIR executable=["_optional", "_target_tool", "_tool"]"#.to_owned(),
            r#"DIR file=["_config", "_optional"]"#.to_owned(),
            r#"DIR files=["_config", "_list", "_optional", "_target_tool", "_tool"]"#.to_owned(),
            r#"VAL config=config.toml list_attr=["two", "config.toml", "two"] list_files=["bazel-out/k8-fastbuild/bin/two.1", "bazel-out/k8-fastbuild/bin/two.2", "config.toml"]"#.to_owned(),
            "VAL exec_tool=bazel-out/k8-opt-exec/bin/tool.sh target_tool=bazel-out/k8-fastbuild/bin/tool.sh optional=config.toml".to_owned(),
            format!(r#"VAL tool_files=["bazel-out/k8-opt-exec/bin/tool.sh"] config_files=["config.toml"] param={param} dict={{"k": "v"}} ints=[1, 2]"#),
        ];
        expected.sort();
        let mut found = prints(&text, "DIR ");
        found.extend(prints(&text, "VAL "));
        found.sort();
        assert_eq!(found, expected);
        assert_eq!(
            ws.output("a.exe"),
            "tool=bazel-out/k8-opt-exec/bin/tool.sh\ntool data\n"
        );
    }
}

#[test]
fn aspects_resolve_their_own_toolchains() {
    // An aspect resolves its OWN ToolchainCollection in the base target's
    // configuration (AspectFunction.getUnloadedToolchainContextsInputs):
    // its toolchains (an optional type without a toolchain is None), its
    // exec_compatible_with and exec groups, never the base rule's (`lib`
    // resolves none); ctx.toolchains, ctx.exec_groups and actions with
    // `toolchain =`/`exec_group =` read it, also through attribute aspects
    // and automatic exec groups. Genuine Bazel 9.3.0 strings
    // (tests/fixtures/aspect_toolchains/reference_receipt.json, MATCH).
    let line = |name: &str| {
        [
            format!("EG {name} g=second has_g=True copts=[]"),
            format!(
                "TC {name} t=first optional=None context=<toolchain_context.resolved_labels: //tc:t, //tc:optional_t>"
            ),
        ]
    };
    let mut expected = line("a").to_vec();
    expected.extend(line("b"));
    expected.sort();
    for (flags, target) in [
        (
            vec!["--aspects=//:defs.bzl%tc_aspect", "--output_groups=tc_out"],
            "//:b",
        ),
        (vec![], "//:u"),
    ] {
        let ws = Workspace::toolchains_fixture();
        let text = ok(&ws.build_builtin(&flags, &[target]));
        let mut found = prints(&text, "EG ");
        found.extend(prints(&text, "TC "));
        found.sort();
        assert_eq!(found, expected, "{target}");
        assert_eq!(
            ws.outputs_ending(".default"),
            ["k8-fastbuild/bin/a.default", "k8-fastbuild/bin/b.default"]
        );
        assert_eq!(ws.output("b.group"), "group\n");
    }
    let ws = Workspace::toolchains_fixture();
    let text = ok(&ws.build_builtin(
        &[
            "--aspects=//:defs.bzl%auto_aspect",
            "--output_groups=auto_out",
        ],
        &["//:a"],
    ));
    assert_eq!(prints(&text, "AUTO "), ["AUTO a t=first group=first"]);
    assert_eq!(ws.output("a.auto"), "auto\n");
    // A mandatory type without a toolchain fails the application with the
    // ToolchainException itself; an unrequested type is
    // StarlarkToolchainContext's error over the aspect's description.
    let ws = Workspace::toolchains_fixture();
    rejected(
        &ws.build_builtin(&["--aspects=//:defs.bzl%missing_aspect"], &["//:a"]),
        "No matching toolchains found for types:\n  //tc:missing_t",
    );
    let ws = Workspace::toolchains_fixture();
    rejected(
        &ws.build_builtin(&["--aspects=//:defs.bzl%plain_aspect"], &["//:a"]),
        "In aspect //:defs.bzl%plain_aspect applied to lib rule //:a, toolchain type //tc:t was requested but only types [] are configured",
    );
}

#[test]
fn aspect_fragments_are_the_aspects_own() {
    // The aspect's RuleContext takes the aspect's configuration fragment
    // policy (ConfiguredTargetFactory.createAspect) and names itself
    // `ASPECT aspect on KIND` (getRuleClassNameForLogging); Bazel 9.3
    // ignores `host_fragments`.
    let ws = Workspace::toolchains_fixture();
    rejected(
        &ws.build_builtin(&["--aspects=//:defs.bzl%nofrag_aspect"], &["//:a"]),
        "//:defs.bzl%nofrag_aspect aspect on lib has to declare 'cpp' as a required fragment in order to access it. Please update the 'fragments' argument of the rule definition (for example: fragments = [\"cpp\"])",
    );
    let ws = Workspace::toolchains_fixture();
    rejected(
        &ws.build_builtin(&["--aspects=//:defs.bzl%hostfrag_aspect"], &["//:a"]),
        "//:defs.bzl%hostfrag_aspect aspect on lib has to declare 'java' as a required fragment in order to access it.",
    );
}

#[test]
fn required_aspects_expand_like_aspects_list() {
    // aspect(requires = [...]): attr(aspects=...) and --aspects lists are
    // AspectsLists (required aspects first, each once, Bazel's texts for a
    // repeated explicit aspect); a required aspect propagates with its
    // requirer, is visible to it without required_aspect_providers, and is
    // a NonApplicableAspect where its required_providers are unmet.
    // Genuine Bazel 9.3.0 strings (tests/fixtures/aspect_requires/
    // reference_receipt.json, 11/11 MATCH).
    let marked = |text: &str, markers: &[&str]| {
        let mut found = Vec::new();
        for marker in markers {
            found.extend(prints(text, marker));
        }
        found.sort();
        found
    };
    let a = r#""//:defs.bzl%asp_a""#;
    let ab = r#""//:defs.bzl%asp_a", "//:defs.bzl%asp_b""#;
    let abe = r#""//:defs.bzl%asp_a", "//:defs.bzl%asp_b", "//:defs.bzl%asp_e""#;

    let ws = Workspace::fixture("aspect_requires");
    let text = ok(&ws.build_with(&[], &["//:b_direct", "//:b_alias", "//:e_top"]));
    assert_eq!(
        marked(&text, &["A ", "B ", "E "]),
        [
            format!("A base tag=t0 ids=[{a}]"),
            format!("A mid tag=t0 ids=[{a}]"),
            format!("B base a=base ids=[{ab}]"),
            format!("B mid a=mid ids=[{ab}]"),
            format!("E base b=b(base) a=False ids=[{abe}]"),
            format!("E mid b=b(mid) a=False ids=[{abe}]"),
        ]
    );
    assert_eq!(ws.output("b_direct.req"), "b(mid)\n");
    assert_eq!(ws.output("b_alias.req"), "b(mid)\n");
    assert_eq!(ws.output("e_top.req"), "e(b(mid))\n");

    // Visibility through required_aspect_providers alone does not carry
    // asp_p along `deps`; asp_n is kept but not applicable.
    let ws = Workspace::fixture("aspect_requires");
    let text = ok(&ws.build_with(&[], &["//:q_top", "//:m_top"]));
    assert_eq!(
        marked(&text, &["Q ", "M ", "N "]),
        [
            r#"M base pn=False ids=["//:defs.bzl%asp_n", "//:defs.bzl%asp_m"]"#,
            r#"M mid pn=False ids=["//:defs.bzl%asp_n", "//:defs.bzl%asp_m"]"#,
            r#"Q base pp=False ids=["//:defs.bzl%asp_q"]"#,
            r#"Q mid pp=True ids=["//:defs.bzl%asp_p", "//:defs.bzl%asp_q"]"#,
        ]
    );

    // Top level: the required aspect carries the top-level parameters
    // (validated over the expanded list) and their description.
    let ws = Workspace::fixture("aspect_requires");
    let text = ok(&ws.build_with(
        &["--aspects=//:defs.bzl%asp_b", "--aspects_parameters=tag=t1"],
        &["//:mid"],
    ));
    let a1 = r#""//:defs.bzl%asp_a[tag=\"t1\"]""#;
    assert_eq!(
        marked(&text, &["A ", "B "]),
        [
            format!("A base tag=t1 ids=[{a1}]"),
            format!("A mid tag=t1 ids=[{a1}]"),
            format!(r#"B base a=base ids=[{a1}, "//:defs.bzl%asp_b"]"#),
            format!(r#"B mid a=mid ids=[{a1}, "//:defs.bzl%asp_b"]"#),
        ]
    );

    for (flags, targets, message) in [
        (
            vec![],
            "//err/twice:x",
            "aspect //:defs.bzl%asp_a added more than once",
        ),
        (
            vec![],
            "//err/after:x",
            "aspect //:defs.bzl%asp_a was added before as a required aspect of aspect //:defs.bzl%asp_b",
        ),
        (
            vec!["--aspects=//:defs.bzl%asp_b", "--aspects=//:defs.bzl%asp_a"],
            "//:mid",
            "aspect //:defs.bzl%asp_a was added before as a required aspect of aspect //:defs.bzl%asp_b",
        ),
        (
            vec!["--aspects=//:defs.bzl%asp_b", "--aspects=//:defs.bzl%asp_b"],
            "//:mid",
            "aspect //:defs.bzl%asp_b added more than once",
        ),
    ] {
        let ws = Workspace::fixture("aspect_requires");
        rejected(&ws.build_with(&flags, &[targets]), message);
    }
}

/// StarlarkDefinedAspect.getDefaultParametersExtractor and AspectDescriptor
/// identity, against the genuine capture tests/fixtures/aspect_parameters:
/// an attribute's aspect takes the parameters of its rule's same-named
/// attributes (and its descriptor shows them), so two rules with different
/// values apply it twice to their shared dependencies, and the same aspect
/// from `--aspects` and an attribute is two applications when the
/// parameters differ, one when they are equal.
#[test]
fn attribute_aspects_take_their_rules_parameters() {
    let genuine = fs::read_to_string(
        Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/aspect_parameters/genuine.txt"),
    )
    .unwrap();
    let mut sections: Vec<(Vec<String>, Vec<String>)> = Vec::new();
    for line in genuine
        .lines()
        .filter(|line| !line.starts_with('#') || line.starts_with("###"))
    {
        if let Some(command) = line.strip_prefix("### build ") {
            sections.push((command.split(' ').map(str::to_owned).collect(), Vec::new()));
        } else {
            sections.last_mut().unwrap().1.push(line.to_owned());
        }
    }
    assert_eq!(sections.len(), 4);
    for (arguments, expected) in sections {
        // A fresh workspace per command, like the genuine fresh server.
        let ws = Workspace::fixture("aspect_parameters");
        let (flags, targets): (Vec<&str>, Vec<&str>) = arguments
            .iter()
            .map(String::as_str)
            .partition(|argument| argument.starts_with("--"));
        let text = ok(&ws.build_with(&flags, &targets));
        let mut printed: Vec<String> = text
            .lines()
            .map(print_text)
            .filter(|line| line.starts_with("A //"))
            .map(str::to_owned)
            .collect();
        printed.sort();
        assert_eq!(printed, expected, "{arguments:?}: {text}");
    }
}

/// The `### build ARGS` sections of a fixture's `genuine.txt` (a genuine
/// Bazel 9.3.0 capture, a fresh server per command): each command's
/// arguments and its sorted expected print texts.
fn genuine_sections(fixture: &str) -> Vec<(Vec<String>, Vec<String>)> {
    let genuine = fs::read_to_string(
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures")
            .join(fixture)
            .join("genuine.txt"),
    )
    .unwrap();
    let mut sections: Vec<(Vec<String>, Vec<String>)> = Vec::new();
    for line in genuine
        .lines()
        .filter(|line| !line.starts_with('#') || line.starts_with("###"))
    {
        if let Some(command) = line.strip_prefix("### build ") {
            sections.push((command.split(' ').map(str::to_owned).collect(), Vec::new()));
        } else {
            sections.last_mut().unwrap().1.push(line.to_owned());
        }
    }
    sections
}

/// Runs each genuine section of `fixture` under the built-in profile and
/// compares the sorted (with `dedup`, deduplicated) prints starting with one
/// of `markers`, or, for a section whose expectation is an `ERROR ` line,
/// the failure text.
fn compare_genuine_builtin(fixture: &str, markers: &[&str], dedup: bool) {
    for (arguments, expected) in genuine_sections(fixture) {
        let ws = if fixture == "aspect_toolchains_aspects" {
            Workspace::toolchains_fixture_named(fixture)
        } else {
            Workspace::fixture(fixture)
        };
        let (flags, targets): (Vec<&str>, Vec<&str>) = arguments
            .iter()
            .map(String::as_str)
            .partition(|argument| argument.starts_with("--"));
        let output = ws.build_builtin(&flags, &targets);
        if let Some(error) = expected
            .first()
            .and_then(|line| line.strip_prefix("ERROR "))
        {
            rejected(&output, error);
            continue;
        }
        let text = ok(&output);
        let mut printed: Vec<String> = text
            .lines()
            .map(print_text)
            .filter(|line| markers.iter().any(|marker| line.starts_with(marker)))
            .map(str::to_owned)
            .collect();
        printed.sort();
        if dedup {
            printed.dedup();
        }
        assert_eq!(printed, expected, "{arguments:?}: {text}");
    }
}

/// `ctx.rule.attr` of an aspect also holds the attributes of its path's
/// base aspects, label-typed ones resolved as the application's
/// dependencies; the rule's attributes win a name, then the base aspects in
/// path order (baseKeys sorted by class name); a base aspect propagates
/// along the main aspect's own attributes (tests/fixtures/
/// aspect_base_attributes/genuine.txt).
#[test]
fn ctx_rule_attr_holds_base_aspect_attributes() {
    compare_genuine_builtin("aspect_base_attributes", &["M ", "V "], false);
}

/// Function-valued `attr_aspects` and `propagation_predicate` with their
/// `propagation_ctx` (tests/fixtures/aspect_propagation_functions/
/// genuine.txt): the edge function per target from the aspect's parameters
/// and the rule's configured attributes, the predicate over the tags (also
/// of a top-level target), and Bazel's errors for invalid results.
#[test]
fn aspect_propagation_functions() {
    compare_genuine_builtin("aspect_propagation_functions", &["A ", "F ", "P "], true);
}

/// `toolchains_aspects` (a list, `'*'`, a function) propagates aspects to
/// the base target's resolved toolchains, whose providers
/// `ctx.rule.toolchains` exposes (tests/fixtures/aspect_toolchains_aspects/
/// genuine.txt).
#[test]
fn aspects_propagate_to_base_target_toolchains() {
    compare_genuine_builtin("aspect_toolchains_aspects", &["TA ", "TF "], true);
}

/// Subrules from aspects (tests/fixtures/aspect_subrules/genuine.txt): the
/// aspect's declared subrules (and theirs) run under the aspect, with
/// Bazel's undeclared-subrule texts for aspects and rules; a lifted
/// single-file attribute, which crashes Bazel 9.3, fails explicitly.
#[test]
fn aspects_call_their_subrules() {
    compare_genuine_builtin("aspect_subrules", &["A ", "D ", "O ", "S "], false);
    let ws = Workspace::fixture("aspect_subrules");
    ok(&ws.build_builtin(
        &["--aspects=//:defs.bzl%asp", "--output_groups=sub"],
        &["//:top"],
    ));
    assert_eq!(ws.output("top.sub"), "p @@//:top\n");
    let ws = Workspace::fixture("aspect_subrules");
    rejected(
        &ws.build_builtin(&["--aspects=//:defs.bzl%file_asp"], &["//:top"]),
        "(allow_single_file or executable): Bazel 9.3 crashes there",
    );
}
