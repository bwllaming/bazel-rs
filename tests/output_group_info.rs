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

const COMPOSED: &str = r#"
def _val(ctx):
    out = ctx.actions.declare_file(ctx.label.name + ".validated")
    ctx.actions.write(out, "validated:" + ctx.label.name)
    return [DefaultInfo(files = depset([out])), OutputGroupInfo(_validation = depset([out]))]

val = rule(implementation = _val)

def _val_out(ctx):
    ctx.actions.write(ctx.outputs.out, "vout")
    validated = ctx.actions.declare_file(ctx.label.name + ".validated")
    ctx.actions.write(validated, "validated:" + ctx.label.name)
    return [
        DefaultInfo(files = depset([ctx.outputs.out])),
        OutputGroupInfo(_validation = depset([validated])),
    ]

val_out = rule(implementation = _val_out, outputs = {"out": "%{name}.out"})

def _collect(ctx):
    out = ctx.actions.declare_file(ctx.label.name + ".out")
    ctx.actions.write(out, "collect")
    groups = {}
    if ctx.attr.own:
        mine = ctx.actions.declare_file(ctx.label.name + ".mine")
        ctx.actions.write(mine, "mine")
        groups["_validation"] = depset([mine])
    elif ctx.attr.empty_own:
        groups["_validation"] = depset()
    if ctx.attr.forward:
        groups["_validation"] = ctx.attr.forward[OutputGroupInfo]._validation
    return [DefaultInfo(files = depset([out])), OutputGroupInfo(**groups)]

collect = rule(implementation = _collect, attrs = {
    "z_first": attr.label_list(allow_files = True),
    "a_second": attr.label(),
    "forward": attr.label(),
    "own": attr.bool(),
    "empty_own": attr.bool(),
})

def _show(ctx):
    for target in ctx.attr.targets:
        names = [f.basename for f in target[OutputGroupInfo]._validation.to_list()]
        print("ORDER %s %s" % (target.label.name, ",".join(names)))
    return []

show = rule(implementation = _show, attrs = {"targets": attr.label_list(allow_files = True)})

def _same(ctx):
    left = ctx.attr.left[OutputGroupInfo]._validation
    right = ctx.attr.right[OutputGroupInfo]._validation
    if (left == right) != ctx.attr.expect:
        fail("%s vs %s: == is %s" % (ctx.attr.left.label, ctx.attr.right.label, left == right))
    return []

same = rule(implementation = _same, attrs = {
    "left": attr.label(allow_files = True),
    "right": attr.label(),
    "expect": attr.bool(),
})

def _link(ctx):
    mine = ctx.actions.declare_file(ctx.label.name + ".validated")
    ctx.actions.write(mine, ctx.label.name)
    return [DefaultInfo(files = depset([mine])), OutputGroupInfo(_validation = depset([mine]))]

link = rule(implementation = _link, attrs = {"dep": attr.label()})
"#;

const COMPOSED_BUILD: &str = "load(':defs.bzl', 'val', 'val_out', 'collect', 'show', 'same')\n\
     val(name = 'leaf')\n\
     val(name = 'other')\n\
     val_out(name = 'vout')\n\
     collect(name = 'single', z_first = [':leaf'])\n\
     collect(name = 'single_empty_own', z_first = [':leaf'], empty_own = True)\n\
     collect(name = 'forwarding', a_second = ':leaf', forward = ':leaf')\n\
     collect(name = 'own_first', own = True, z_first = [':other', ':leaf'], a_second = ':single')\n\
     collect(name = 'top', z_first = [':own_first', ':vout.out'], a_second = ':other')\n\
     genrule(name = 'gen', srcs = [':own_first', ':leaf'], outs = ['gen.txt'], cmd = 'echo > $@')\n\
     filegroup(name = 'fg', srcs = [':leaf'])\n\
     show(name = 'show', targets = [':single', ':own_first', ':top', ':gen', ':fg', ':vout.out'])\n\
     same(name = 'same_single', left = ':single', right = ':leaf', expect = True)\n\
     same(name = 'same_empty_own', left = ':single_empty_own', right = ':leaf', expect = True)\n\
     same(name = 'same_forwarding', left = ':forwarding', right = ':leaf', expect = True)\n\
     same(name = 'same_native', left = ':fg', right = ':leaf', expect = True)\n\
     same(name = 'same_output_file', left = ':vout.out', right = ':vout', expect = True)\n\
     same(name = 'new_own_first', left = ':own_first', right = ':other', expect = False)\n\
     same(name = 'new_gen', left = ':gen', right = ':own_first', expect = False)\n";

#[test]
fn validation_groups_compose_in_collection_order_and_reuse_a_lone_set() {
    let ws = Workspace::new(COMPOSED, COMPOSED_BUILD);
    // RuleConfiguredTargetBuilder's one stable-order `_validation` builder
    // (W1c composes the prerequisites' sets instead of flattening them): the
    // own group first, then the attributes in class order, each set's
    // first occurrences; native rules (genrule, filegroup) and output-file
    // targets (the generating rule's group) alike.
    let text = ok(&ws.build_with(&["--nobuild"], &[":show"]));
    let orders = text
        .lines()
        .filter_map(|line| line.split_once("ORDER ").map(|(_, rest)| rest.trim()))
        .collect::<Vec<_>>();
    assert_eq!(
        orders,
        [
            "single leaf.validated",
            "own_first own_first.mine,other.validated,leaf.validated",
            "top own_first.mine,other.validated,leaf.validated,vout.validated",
            "gen own_first.mine,other.validated,leaf.validated",
            "fg leaf.validated",
            "vout.out vout.validated",
        ],
        "{text}"
    );
    // NestedSetBuilder.buildInternal (:181-202): a group of exactly one
    // contributed stable-order set (an empty own group does not count; a
    // forwarded own group is that set) IS that set, as an output-file
    // target's group is its generating rule's; an own group plus a
    // contributor, or two contributors, is a new set.
    ok(&ws.build_with(
        &["--nobuild"],
        &[
            ":same_single",
            ":same_empty_own",
            ":same_forwarding",
            ":same_native",
            ":same_output_file",
            ":new_own_first",
            ":new_gen",
        ],
    ));
    // The composed sets build: every validation output of `top` runs with
    // it, also through the ValidateTarget aspect, which promotes the set
    // (`_validation_top_level`).
    for flags in [&[][..], &["--experimental_use_validation_aspect"][..]] {
        let ws = Workspace::new(COMPOSED, COMPOSED_BUILD);
        ok(&ws.build_with(flags, &[":top"]));
        for (name, text) in [
            ("own_first.mine", "mine"),
            ("other.validated", "validated:other"),
            ("leaf.validated", "validated:leaf"),
            ("vout.validated", "validated:vout"),
        ] {
            assert_eq!(ws.read(name).as_deref(), Some(text), "{flags:?} {name}");
        }
    }
}

#[test]
fn validation_and_hidden_groups_deeper_than_the_depset_limit_compose() {
    // A `_validation` set 1,023 levels deep (Starlark's depset() allows
    // 1,024), also the transitive files of the target's runfiles, then 20
    // targets that each add their own validation output to their
    // predecessor's group: each composed group is one level deeper (1,043
    // at the top) and the HIDDEN_TOP_LEVEL group over the 1,024-deep
    // runfiles artifacts 1,025, past the depset() limit, which framework
    // compositions never check (Bazel's NestedSetBuilder has none). An own
    // group 1,024 deep merged with a contributor's (`deep_own`) panicked in
    // the checked merge before W1c. Genuine Bazel 9.3.0 builds all of it.
    let ws = Workspace::new(
        &format!(
            "{COMPOSED}\n\
             def _deep(ctx):\n    \
                 a = ctx.actions.declare_file(ctx.label.name + '.a.validated')\n    \
                 b = ctx.actions.declare_file(ctx.label.name + '.b.validated')\n    \
                 ctx.actions.write(a, 'a')\n    \
                 ctx.actions.write(b, 'b')\n    \
                 d = depset([a])\n    \
                 for i in range(ctx.attr.iterations):\n        \
                     d = depset([b if i % 2 == 0 else a], transitive = [d])\n    \
                 runfiles = ctx.runfiles(transitive_files = d) if ctx.attr.with_runfiles else None\n    \
                 return [DefaultInfo(files = depset([a]), runfiles = runfiles),\n            \
                         OutputGroupInfo(_validation = d)]\n\
             deep = rule(implementation = _deep, attrs = {{'iterations': attr.int(),\n    \
                 'with_runfiles': attr.bool(), 'dep': attr.label()}})\n\
             def _count(ctx):\n    \
                 print('COUNT', len(ctx.attr.dep[OutputGroupInfo]._validation.to_list()),\n          \
                       len(ctx.attr.deep[OutputGroupInfo]._hidden_top_level_INTERNAL_.to_list()))\n    \
                 return []\n\
             count = rule(implementation = _count, attrs = {{'dep': attr.label(), 'deep': attr.label()}})\n\
             def links(count, first):\n    \
                 for index in range(count):\n        \
                     link(name = 'c%d' % index, dep = ':c%d' % (index - 1) if index else first)\n"
        ),
        "load(':defs.bzl', 'count', 'deep', 'links')\n\
         deep(name = 'deep', iterations = 1022, with_runfiles = True)\n\
         links(20, ':deep')\n\
         deep(name = 'deep_own', iterations = 1023, dep = ':c0')\n\
         count(name = 'count', dep = ':c19', deep = ':deep')\n\
         count(name = 'count_own', dep = ':deep_own', deep = ':deep')\n\
         filegroup(name = 'fg', srcs = [':c19'])\n",
    );
    let text = ok(&ws.build(&[":count", ":count_own", ":fg"]));
    assert!(text.contains("COUNT 22 2"), "{text}");
    assert!(text.contains("COUNT 5 2"), "{text}");
    assert_eq!(ws.read("deep.b.validated").as_deref(), Some("b"));
    assert_eq!(ws.read("c19.validated").as_deref(), Some("c19"));
}
