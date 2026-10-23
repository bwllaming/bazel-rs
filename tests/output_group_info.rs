//! N25d5 follow-ups (docs/CC_TOOLCHAIN.md): the per-target OutputGroupInfo of
//! native rule targets is one instance per configured target, the private
//! `_validation_transitive` group is restricted like Bazel's
//! RuleConfiguredTargetBuilder, and a File reachable only through a closure
//! in a provider is an action input like any other (W1b: the engine resolves
//! every File through the build's artifact index, as Bazel resolves an input
//! through its Artifact).
#![allow(clippy::disallowed_methods)] // test harness: starts processes directly (clippy.toml)
use std::fs;
use std::path::Path;
use std::process::{Command, Output};

struct Workspace(tempfile::TempDir);

impl Workspace {
    fn new(defs: &str, build: &str) -> Self {
        let ws = Self(
            tempfile::Builder::new()
                .prefix("output-group-info-")
                .tempdir_in("/tmp/opencode")
                .unwrap(),
        );
        ws.file("MODULE.bazel", "module(name='ogi_fixture')\n");
        ws.file("BUILD.bazel", build);
        ws.file("defs.bzl", defs);
        ws.file("src.txt", "source\n");
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

    fn build(&self, targets: &[&str]) -> Output {
        self.build_with(&[], targets)
    }

    fn build_with(&self, flags: &[&str], targets: &[&str]) -> Output {
        Command::new("timeout")
            .args(["--kill-after=5s", "120s", env!("CARGO_BIN_EXE_bazel-rs")])
            .arg("--ignore_all_rc_files")
            .arg(format!(
                "--cache-dir={}",
                self.path().join("private-cache").display()
            ))
            .arg("build")
            .args(flags)
            .args(targets)
            .current_dir(self.path())
            .output()
            .unwrap()
    }

    fn read(&self, target: &str) -> Option<String> {
        // Executed outputs materialize under the action cache
        // (<cache>/actions/<id>/outputs/bazel-out/<config>/bin/...).
        let mut matches = Vec::new();
        let actions = self.path().join("private-cache/actions");
        let Ok(entries) = std::fs::read_dir(actions) else {
            return None;
        };
        for entry in entries.flatten() {
            let bin = entry
                .path()
                .join("outputs/bazel-out/k8-fastbuild/bin")
                .join(target);
            if bin.is_file() {
                matches.push(bin);
            }
        }
        matches.sort();
        matches
            .iter()
            .rev()
            .map(std::fs::read_to_string)
            .next()?
            .ok()
    }
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
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

const CHECK: &str = r#"
def _check(ctx):
    a, b = ctx.attr.a, ctx.attr.b
    (OutputGroupInfo in a and a[OutputGroupInfo] == a[OutputGroupInfo]) or fail("same target")
    (a[OutputGroupInfo] == b[OutputGroupInfo]) or fail("two edges, one configured target")
    print("GROUPS", dir(a[OutputGroupInfo]), a[OutputGroupInfo]._hidden_top_level_INTERNAL_.to_list())
    return []

check = rule(implementation = _check, attrs = {"a": attr.label(), "b": attr.label()})
"#;

#[test]
fn native_rule_targets_have_one_output_group_info_per_configured_target() {
    let ws = Workspace::new(
        CHECK,
        "load(':defs.bzl', 'check')\n\
         filegroup(name = 'fg', srcs = ['src.txt'])\n\
         alias(name = 'al', actual = ':fg')\n\
         genrule(name = 'gen', outs = ['gen.txt'], cmd = 'echo > $@')\n\
         check(name = 'fg_twice', a = ':fg', b = ':fg')\n\
         check(name = 'via_alias', a = ':fg', b = ':al')\n\
         check(name = 'genrule', a = ':gen', b = ':gen')\n",
    );
    let text = ok(&ws.build(&[":fg_twice", ":via_alias", ":genrule"]));
    assert!(
        text.contains("GROUPS [\"_hidden_top_level_INTERNAL_\"]"),
        "{text}"
    );
}

const VALIDATION: &str = r#"
def _impl(ctx):
    out = ctx.actions.declare_file(ctx.label.name + ".txt")
    ctx.actions.write(out, "x")
    files = [out] if ctx.attr.nonempty else []
    return [DefaultInfo(files = depset([out])), OutputGroupInfo(**{ctx.attr.group: depset(files)})]

r = rule(implementation = _impl, attrs = {"group": attr.string(), "nonempty": attr.bool()})
"#;

#[test]
fn private_transitive_validation_group_is_restricted() {
    let ws = Workspace::new(
        VALIDATION,
        "load(':defs.bzl', 'r')\n\
         r(name = 'transitive', group = '_validation_transitive')\n\
         r(name = 'transitive_empty', group = '_validation_transitive')\n\
         r(name = 'validation', group = '_validation', nonempty = True)\n\
         r(name = 'validation_empty', group = '_validation')\n",
    );
    // RuleConfiguredTargetBuilder.java:366-377: the key alone is rejected
    // for a rule defined outside the internal allowlist.
    for target in [":transitive", ":transitive_empty"] {
        rejected(
            &ws.build(&[target]),
            "//:defs.bzl cannot access the _transitive_validation private API",
        );
    }
    // The `_validation` group is a rule's ordinary output group now: the
    // validation action runs when the target builds and the output is not
    // reported (a `_`-prefixed group is never important,
    // TopLevelArtifactHelper.java:228-236).
    let text = ok(&ws.build(&[":validation"]));
    assert!(!text.contains("validation.txt"), "{text}");
    assert_eq!(ws.read("validation.txt").as_deref(), Some("x"));
    ok(&ws.build(&[":validation_empty"]));
}

const PROPAGATION: &str = r#"
def _val(ctx):
    out = ctx.actions.declare_file(ctx.label.name + ".validated")
    ctx.actions.write(out, "validated:" + ctx.label.name)
    return [
        DefaultInfo(files = depset([out])),
        OutputGroupInfo(_validation = depset([out])),
    ]

val = rule(implementation = _val)

def _middle(ctx):
    out = ctx.actions.declare_file(ctx.label.name + ".out")
    ctx.actions.write(out, "middle")
    validation = []
    if ctx.attr.own:
        own = ctx.actions.declare_file(ctx.label.name + ".mine")
        ctx.actions.write(own, "mine")
        validation = [own]
    return [
        DefaultInfo(files = depset([out])),
        OutputGroupInfo(_validation = depset(validation)),
    ]

middle = rule(
    implementation = _middle,
    attrs = {
        "direct": attr.label(),
        "skipped": attr.label(skip_validations = True),
        "tool": attr.label(cfg = "exec"),
        "own": attr.bool(),
    },
)
"#;

#[test]
fn validation_outputs_propagate_and_run_with_the_dependent() {
    let build = "load(':defs.bzl', 'val', 'middle')\n\
         val(name = 'leaf')\n\
         middle(name = 'mid', direct = ':leaf', own = True)\n\
         middle(name = 'top', direct = ':mid')\n\
         filegroup(name = 'fg', srcs = [':top'])\n\
         genrule(name = 'gen', srcs = [':top'], outs = ['gen.txt'], cmd = 'echo > $@')\n";
    let ws = Workspace::new(PROPAGATION, build);
    // Starlark collection: `top` builds its own output and runs every
    // `_validation` output of `mid` (its own and `leaf`'s, already collected
    // at `mid`); a hidden group is never reported.
    let text = ok(&ws.build(&[":top"]));
    assert!(!text.contains("validated"), "{text}");
    assert_eq!(ws.read("leaf.validated").as_deref(), Some("validated:leaf"));
    assert_eq!(ws.read("mid.mine").as_deref(), Some("mine"));
    // Native rules collect too (RuleConfiguredTargetBuilder applies to every
    // rule): a filegroup and a genrule run their dependency's validations.
    ok(&ws.build(&[":fg", ":gen"]));
    assert_eq!(ws.read("top.out").as_deref(), Some("middle"));
    // --run_validations=false (BuildRequestOptions.java:178-190) is the only
    // way to keep validation actions from running.
    let ws_off = Workspace::new(PROPAGATION, build);
    ok(&ws_off.build_with(&["--run_validations=false"], &[":top"]));
    assert!(ws_off.read("leaf.validated").is_none());
    // `-_validation` does not disable them: the group is re-added after the
    // subtractions (OutputGroupInfo.determineOutputGroups, :236-242).
    let ws_minus = Workspace::new(PROPAGATION, build);
    ok(&ws_minus.build_with(&["--output_groups=-_validation"], &[":top"]));
    assert!(ws_minus.read("leaf.validated").is_some());
}

#[test]
fn tool_dependencies_do_not_propagate_validation() {
    let ws = Workspace::new(
        PROPAGATION,
        "load(':defs.bzl', 'val', 'middle')\n\
         val(name = 'leaf')\n\
         middle(name = 'tool_only', tool = ':leaf')\n",
    );
    // collectTransitiveValidationOutputGroups (:406-419) skips tool
    // dependencies (`cfg = "exec"`, Attribute.isToolDependency). A
    // skip_validations=True attribute never reaches the native analysis
    // (an unanalyzed declared feature), so that half of the filter is
    // fixture-verified only (tests/fixtures/validation_actions).
    let text = ok(&ws.build_with(&["--experimental-builtin-modules"], &[":tool_only"]));
    assert!(!text.contains("validated"), "{text}");
    assert!(ws.read("leaf.validated").is_none());
}

const OUTPUT_GROUPS: &str = r#"
def _g(ctx):
    out = ctx.actions.declare_file(ctx.label.name + ".out")
    ctx.actions.write(out, "default")
    extra = ctx.actions.declare_file(ctx.label.name + ".extra")
    ctx.actions.write(extra, "extra")
    return [
        DefaultInfo(files = depset([out])),
        OutputGroupInfo(extra = depset([extra])),
    ]

g = rule(implementation = _g)
"#;

#[test]
fn output_groups_flag_selects_and_reports_groups() {
    let build = "load(':defs.bzl', 'g')\n\
         g(name = 'g')\n";
    // `--output_groups=+extra`: the default set plus `extra`, reported
    // (an unprefixed group name is important).
    let ws = Workspace::new(OUTPUT_GROUPS, build);
    let text = stdout(&ws.build_with(&["--output_groups=+extra"], &[":g"]));
    assert!(
        text.contains("//:g\t") && text.contains("g.extra"),
        "{text}"
    );
    assert_eq!(ws.read("g.out").as_deref(), Some("default"));
    // `--output_groups=extra` overrides the default set: only `extra` is
    // built and reported (determineOutputGroups :213-226).
    let ws = Workspace::new(OUTPUT_GROUPS, build);
    let text = stdout(&ws.build_with(&["--output_groups=extra"], &[":g"]));
    assert!(text.contains("g.extra"), "{text}");
    assert!(ws.read("g.out").is_none());
    // A group the target does not have contributes no artifacts and no
    // error (TopLevelArtifactHelper ignores empty groups, :221-224).
    let ws = Workspace::new(OUTPUT_GROUPS, build);
    let text = stdout(&ws.build_with(&["--output_groups=missing"], &[":g"]));
    assert!(!text.contains("g.out"), "{text}");
    // Repeated flags accumulate (allowMultiple); `+` tokens keep the
    // default set (a mixed list with an unprefixed token would override it).
    let ws = Workspace::new(OUTPUT_GROUPS, build);
    ok(&ws.build_with(
        &["--output_groups=+extra", "--output_groups=+missing"],
        &[":g"],
    ));
    assert_eq!(ws.read("g.out").as_deref(), Some("default"));
}

const CLOSURE: &str = r#"
Box = provider(fields = ["get"])

def _producer(ctx):
    hidden = ctx.actions.declare_file("hidden.txt")
    ctx.actions.write(hidden, "hidden")
    def get():
        return hidden
    return [Box(get = get)]

producer = rule(implementation = _producer)

def _consumer(ctx):
    hidden = ctx.attr.dep[Box].get()
    out = ctx.actions.declare_file("out.txt")
    ctx.actions.run_shell(inputs = [hidden], outputs = [out], command = "cp $1 $2",
                          arguments = [hidden.path, out.path])
    return [DefaultInfo(files = depset([out]))]

consumer = rule(implementation = _consumer, attrs = {"dep": attr.label()})
"#;

#[test]
fn file_reachable_only_through_a_provider_closure_is_an_input() {
    let ws = Workspace::new(
        CLOSURE,
        "load(':defs.bzl', 'consumer', 'producer')\n\
         producer(name = 'producer')\n\
         consumer(name = 'consumer', dep = ':producer')\n",
    );
    // Bazel imposes no input restriction: the closure's File is the
    // producer's Artifact, so the consumer's action depends on the
    // producer's write action, which runs although the producer's own
    // files to build are empty (tests/fixtures/provider_payload_files
    // receipt: genuine Bazel 9.3 builds the same program).
    ok(&ws.build(&[":consumer"]));
    assert_eq!(ws.read("out.txt").as_deref(), Some("hidden"));
}
