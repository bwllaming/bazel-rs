//! CLI coverage for Bazel 9.3's built-in attributes of Starlark rules
//! (docs/COMMON_RULE_ATTRIBUTES.md): StarlarkRuleClassFunctions.baseRule /
//! binaryBaseRule / getTestBaseRule inherit
//! BaseRuleClasses.commonCoreAndStarlarkAttributes. Covers the rules_go
//! `nogo` macro shape (tags/visibility through **kwargs), ctx.attr exposure,
//! package defaults, the testonly prerequisite error, deprecation warnings,
//! select() on configurable common attributes and explicit analysis gaps.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

struct Workspace(tempfile::TempDir);

impl Workspace {
    fn new(files: &[(&str, &str)]) -> Self {
        let base = std::env::var_os("TMPDIR")
            .map(PathBuf::from)
            .unwrap_or_else(std::env::temp_dir);
        let ws = Self(
            tempfile::Builder::new()
                .prefix("common-attrs-")
                .tempdir_in(base)
                .unwrap(),
        );
        ws.file("MODULE.bazel", "module(name='common_attrs')\n");
        for (name, content) in files {
            ws.file(name, content);
        }
        ws
    }

    fn path(&self) -> &Path {
        self.0.path()
    }

    fn file(&self, name: &str, content: &str) {
        let path = self.path().join(name);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, content).unwrap();
    }

    fn run(&self, args: &[&str]) -> Output {
        let output = Command::new("timeout")
            .args(["--kill-after=5s", "60s", env!("CARGO_BIN_EXE_bazel-rs")])
            .args(["--ignore_all_rc_files", "--cache-dir=private-cache"])
            .args(args)
            .current_dir(self.path())
            .output()
            .unwrap();
        assert_ne!(output.status.code(), Some(124), "CLI timed out: {args:?}");
        assert_ne!(output.status.code(), Some(137), "CLI killed: {args:?}");
        output
    }
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn errors(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

/// stderr with the `/abs/BUILD:L:C: ` event location removed from WARNING
/// lines (asserted separately where it matters).
fn warnings(output: &Output) -> String {
    errors(output)
        .lines()
        .map(|line| {
            let Some(rest) = line.strip_prefix("WARNING: ") else {
                return format!("{line}\n");
            };
            let located = rest.split_once(": ").filter(|(location, _)| {
                let mut parts = location.rsplitn(3, ':');
                let column = parts.next().unwrap_or_default();
                let line = parts.next().unwrap_or_default();
                parts.next().is_some_and(|file| !file.contains(' '))
                    && !column.is_empty()
                    && column.bytes().all(|b| b.is_ascii_digit())
                    && !line.is_empty()
                    && line.bytes().all(|b| b.is_ascii_digit())
            });
            match located {
                Some((_, message)) => format!("WARNING: {message}\n"),
                None => format!("{line}\n"),
            }
        })
        .collect()
}

fn rejected(output: &Output, diagnostic: &str) {
    assert!(!output.status.success(), "unexpected success");
    assert!(output.stdout.is_empty(), "partial output on failure");
    assert!(errors(output).contains(diagnostic), "{}", errors(output));
    assert!(!errors(output).contains("panicked"), "{}", errors(output));
}

/// Built outputs' contents, in output order.
fn contents(output: &Output) -> Vec<String> {
    assert!(output.status.success(), "{}", errors(output));
    stdout(output)
        .lines()
        .map(|line| fs::read_to_string(line.split_once('\t').unwrap().1).unwrap())
        .collect()
}

/// A rule that writes its built-in `ctx.attr` view.
const DEFS: &str = r#"
def _show_impl(ctx):
    out = ctx.actions.declare_file(ctx.label.name + ".txt")
    a = ctx.attr
    fields = [
        "tags=" + repr(a.tags),
        "testonly=" + repr(a.testonly),
        "deprecation=" + repr(a.deprecation),
        "features=" + repr(a.features),
        "visibility=" + repr([str(label) for label in a.visibility]),
        "exec_properties=" + repr(a.exec_properties),
        "expect_failure=" + repr(a.expect_failure),
        "dependency_lists=" + repr([
            a.toolchains,
            a.compatible_with,
            a.restricted_to,
            a.package_metadata,
            a.exec_compatible_with,
            a.target_compatible_with,
            a._action_listener,
            a._config_dependencies,
            a.transitive_configs,
        ]),
        "files=" + repr([ctx.files.toolchains, ctx.files.target_compatible_with, ctx.files._config_dependencies]),
        "absent=" + repr([hasattr(a, name) for name in ["aspect_hints", "exec_group_compatible_with", "licenses", "distribs", "size", "args"]]),
        "deps=" + repr([dep.label.name for dep in a.deps]),
    ]
    ctx.actions.write(output = out, content = "\n".join(fields) + "\n")
    return [DefaultInfo(files = depset([out]))]

show = rule(implementation = _show_impl, attrs = {"deps": attr.label_list(allow_files = True)})

def _fail_impl(ctx):
    fail("analysis must not reach " + ctx.label.name)

never = rule(implementation = _fail_impl)

# rules_go go/private/rules/nogo.bzl:122-147 (0.63): the macro forwards
# visibility and **kwargs, appending "manual" to tags.
_nogo = rule(implementation = _show_impl, attrs = {"deps": attr.label_list()})

def nogo(name, visibility = None, **kwargs):
    actual_name = "%s_actual" % name
    native.alias(
        name = name,
        actual = select({
            "//conditions:default": actual_name,
        }),
        visibility = visibility,
    )
    kwargs.setdefault("tags", [])
    if "manual" not in kwargs["tags"]:
        kwargs["tags"].append("manual")
    _nogo(
        name = actual_name,
        visibility = visibility,
        **kwargs
    )
"#;

fn workspace(build: &str) -> Workspace {
    Workspace::new(&[("defs.bzl", DEFS), ("BUILD.bazel", build)])
}

#[test]
fn rules_go_nogo_macro_forwards_tags_and_visibility_through_kwargs() {
    let ws = workspace(
        "load(':defs.bzl', 'nogo', 'show')\nshow(name = 'dep')\nnogo(name = 'nogo', deps = [':dep'], tags = ['lint'], visibility = ['//visibility:public'], testonly = False, features = ['-layering_check'])\n",
    );
    let query = ws.run(&["query", "//:all"]);
    assert!(query.status.success(), "{}", errors(&query));
    assert_eq!(stdout(&query), "//:dep\n//:nogo\n//:nogo_actual\n");
    let built = contents(&ws.run(&["build", "//:nogo_actual"]));
    assert_eq!(
        built,
        [
            "tags=[\"lint\", \"manual\"]\ntestonly=False\ndeprecation=None\nfeatures=[\"-layering_check\"]\nvisibility=[\"@@//visibility:public\"]\nexec_properties={}\nexpect_failure=\"\"\ndependency_lists=[[], [], [], [], [], [], [], [], []]\nfiles=[[], [], []]\nabsent=[False, False, False, False, False, False]\ndeps=[\"dep\"]\n"
        ]
    );
    // `manual` custom targets are skipped by build wildcards; explicit
    // labels still build (FilteringPolicies.FILTER_MANUAL).
    let ws = workspace(
        "load(':defs.bzl', 'never', 'show')\nnever(name = 'skipped', tags = ['manual'])\nshow(name = 'built', tags = ['custom'])\n",
    );
    let built = contents(&ws.run(&["build", "//:all"]));
    assert_eq!(built.len(), 1);
    assert!(built[0].starts_with("tags=[\"custom\"]\n"), "{}", built[0]);
    rejected(
        &ws.run(&["build", "//:skipped"]),
        "analysis must not reach skipped",
    );
}

#[test]
fn package_defaults_set_testonly_and_deprecation_unless_explicit() {
    let ws = workspace(
        "load(':defs.bzl', 'show')\npackage(default_testonly = True, default_deprecation = 'pkg message', features = ['pkg_feature'])\nshow(name = 'defaulted')\nshow(name = 'explicit', testonly = 0, deprecation = 'own', exec_properties = {'k': 'v'}, visibility = ['//visibility:private', '//:__pkg__'])\n",
    );
    let output = ws.run(&["build", "//:defaulted", "//:explicit"]);
    // Requested targets with an explicit deprecation warn; package defaults
    // do not (TargetPatternPhaseFunction.java:255-275).
    let stderr = warnings(&output);
    assert!(
        stderr.contains("WARNING: target '//:explicit' is deprecated: own\n")
            && !stderr.contains("//:defaulted' is deprecated"),
        "{stderr}"
    );
    assert!(
        errors(&output).contains(&format!(
            "WARNING: {}/BUILD.bazel:4:5: target '//:explicit' is deprecated: own\n",
            ws.path().display()
        )),
        "{}",
        errors(&output)
    );
    let built = contents(&output);
    assert!(
        built[0]
            .contains("testonly=True\ndeprecation=\"pkg message\"\nfeatures=[]\nvisibility=[]\n"),
        "{}",
        built[0]
    );
    assert!(
        built[1].contains("testonly=False\ndeprecation=\"own\"\n"),
        "{}",
        built[1]
    );
    assert!(
        built[1].contains("visibility=[\"@@//:__pkg__\"]\nexec_properties={\"k\": \"v\"}\n"),
        "{}",
        built[1]
    );
}

#[test]
fn testonly_prerequisites_require_testonly_consumers() {
    let ws = Workspace::new(&[
        ("defs.bzl", DEFS),
        (
            "helpers/BUILD.bazel",
            "load('//:defs.bzl', 'show')\nshow(name = 'helper', testonly = True, visibility = ['//visibility:public'])\ngenrule(name = 'gen', outs = ['gen.out'], cmd = 'echo > $@', testonly = True, visibility = ['//visibility:public'])\nfilegroup(name = 'group', srcs = [':helper'])\n",
        ),
        (
            "defaulted/BUILD.bazel",
            "load('//:defs.bzl', 'show')\npackage(default_testonly = True)\nshow(name = 'helper', visibility = ['//visibility:public'])\n",
        ),
        (
            "BUILD.bazel",
            "load(':defs.bzl', 'show')\nshow(name = 'lib', deps = ['//helpers:helper'])\nshow(name = 'test_lib', deps = ['//helpers:helper'], testonly = True)\nshow(name = 'defaulted', deps = ['//defaulted:helper'])\nshow(name = 'output', deps = ['//helpers:gen.out'])\n",
        ),
    ]);
    // CommonPrerequisiteValidator.java:741-771 through RuleContext.ruleError.
    rejected(
        &ws.run(&["build", "//:lib"]),
        "in show rule //:lib: non-test target '//:lib' depends on testonly target '//helpers:helper' and doesn't have testonly attribute set",
    );
    rejected(
        &ws.run(&["build", "//:defaulted"]),
        "in show rule //:defaulted: non-test target '//:defaulted' depends on testonly target '//defaulted:helper' and doesn't have testonly attribute set",
    );
    // Same-package edges are checked too, also for native consumers.
    rejected(
        &ws.run(&["build", "//helpers:group"]),
        "in filegroup rule //helpers:group: non-test target '//helpers:group' depends on testonly target '//helpers:helper' and doesn't have testonly attribute set",
    );
    let built = contents(&ws.run(&["build", "//:test_lib"]));
    assert!(built[0].contains("testonly=True\n"), "{}", built[0]);
    // Output files of testonly rules are not checked by default
    // (--incompatible_check_testonly_for_output_files=false).
    let built = contents(&ws.run(&["build", "//:output"]));
    assert!(built[0].contains("deps=[\"gen.out\"]"), "{}", built[0]);
}

#[test]
fn deprecated_prerequisites_warn_across_packages() {
    let ws = Workspace::new(&[
        ("defs.bzl", DEFS),
        (
            "old/BUILD.bazel",
            "load('//:defs.bzl', 'show')\nshow(name = 'old', deprecation = 'use //new', visibility = ['//visibility:public'])\nshow(name = 'local_user', deps = [':old'])\ngenrule(name = 'gen', outs = ['gen.out'], cmd = 'echo > $@', deprecation = 'gen is old', visibility = ['//visibility:public'])\n",
        ),
        (
            "BUILD.bazel",
            "load(':defs.bzl', 'show')\nshow(name = 'user', deps = ['//old:old'])\nshow(name = 'deprecated_user', deps = ['//old:old'], deprecation = '')\nshow(name = 'output_user', deps = ['//old:gen.out'])\nfilegroup(name = 'group', srcs = ['//old:old'])\n",
        ),
    ]);
    let user = ws.run(&["build", "//:user"]);
    assert!(user.status.success(), "{}", errors(&user));
    assert!(
        warnings(&user).contains(
            "WARNING: in show rule //:user: target '//:user' depends on deprecated target '//old:old': use //new\n"
        ),
        "{}",
        errors(&user)
    );
    let group = ws.run(&["build", "//:group"]);
    assert!(
        warnings(&group).contains(
            "WARNING: in filegroup rule //:group: target '//:group' depends on deprecated target '//old:old': use //new\n"
        ),
        "{}",
        errors(&group)
    );
    let output = ws.run(&["build", "//:output_user"]);
    assert!(
        warnings(&output).contains(
            "WARNING: in show rule //:output_user: target '//:output_user' depends on the output file //old:gen.out of a deprecated rule //old:gen': gen is old\n"
        ),
        "{}",
        errors(&output)
    );
    // No warning within a package.
    let quiet = ws.run(&["build", "//old:local_user"]);
    assert!(quiet.status.success(), "{}", errors(&quiet));
    assert!(!errors(&quiet).contains("WARNING"), "{}", errors(&quiet));
    // A deprecated consumer (even "") gets no dependency warning, but as a
    // requested target it is itself reported as deprecated.
    let consumer = ws.run(&["build", "//:deprecated_user"]);
    assert!(consumer.status.success(), "{}", errors(&consumer));
    assert_eq!(
        warnings(&consumer)
            .lines()
            .filter(|line| line.starts_with("WARNING"))
            .collect::<Vec<_>>(),
        ["WARNING: target '//:deprecated_user' is deprecated: "]
    );
    // Dependency warnings carry the consumer's BUILD location.
    assert!(
        errors(&user).contains(&format!(
            "WARNING: {}/BUILD.bazel:2:5: in show rule //:user: ",
            ws.path().display()
        )),
        "{}",
        errors(&user)
    );
}

#[test]
fn select_follows_attribute_configurability() {
    let ws = workspace(
        "load(':defs.bzl', 'show')\nconfig_setting(name = 'opt', values = {'compilation_mode': 'opt'})\nshow(name = 'simplified', features = select({'//conditions:default': ['a']}) + ['b'])\nshow(name = 'retained', features = select({':opt': ['fast'], '//conditions:default': []}))\nshow(name = 'tcw', target_compatible_with = select({':opt': [], '//conditions:default': []}))\n",
    );
    let built = contents(&ws.run(&["build", "//:simplified"]));
    assert!(
        built[0].contains("features=[\"a\", \"b\"]\n"),
        "{}",
        built[0]
    );
    let query = ws.run(&["query", "//:all"]);
    assert!(query.status.success(), "{}", errors(&query));
    // Retained selects resolve against the configuration (docs/CONFIGURATION.md).
    for (args, features) in [
        (&["build", "//:retained"][..], "features=[]\n"),
        (
            &["build", "-c", "opt", "//:retained"][..],
            "features=[\"fast\"]\n",
        ),
    ] {
        let built = contents(&ws.run(args));
        assert!(built[0].contains(features), "{args:?}: {}", built[0]);
        // ctx.attr._config_dependencies stays [] with select() conditions
        // (DependencyResolutionHelpers.java:408-416).
        assert!(
            built[0].contains("dependency_lists=[[], [], [], [], [], [], [], [], []]\n"),
            "{}",
            built[0]
        );
    }
    // An empty resolved target_compatible_with needs no constraint analysis.
    let tcw = contents(&ws.run(&["build", "-c", "opt", "//:tcw"]));
    assert!(tcw[0].contains("features=[]\n"), "{}", tcw[0]);
    for (argument, message) in [
        (
            "tags = select({'//conditions:default': []})",
            "//:x: attribute \"tags\" is not configurable",
        ),
        (
            "testonly = select({'//conditions:default': True})",
            "//:x: attribute \"testonly\" is not configurable",
        ),
        (
            "visibility = select({'//conditions:default': []})",
            "//:x: attribute \"visibility\" is not configurable",
        ),
        (
            "exec_compatible_with = select({'//conditions:default': []})",
            "//:x: attribute \"exec_compatible_with\" is not configurable",
        ),
    ] {
        let ws = workspace(&format!(
            "load(':defs.bzl', 'show')\nshow(name = 'x', {argument})\n"
        ));
        rejected(&ws.run(&["query", "//:all"]), message);
    }
}

#[test]
fn conversion_and_unknown_attribute_errors_match_bazel() {
    for (argument, message) in [
        (
            "tags = 1",
            "//:x: expected value of type 'list(string)' for attribute 'tags' of 'show', but got 1 (int)",
        ),
        (
            "testonly = 'yes'",
            "//:x: expected one of [False, True, 0, 1] for attribute 'testonly' of 'show', but got \"yes\" (string)",
        ),
        (
            "deprecation = []",
            "//:x: expected value of type 'string' for attribute 'deprecation' of 'show', but got [] (list)",
        ),
        (
            "exec_properties = {'k': 1}",
            "//:x: expected value of type 'string' for dict value element, but got 1 (int)",
        ),
        (
            "tagz = []",
            "//:x: no such attribute 'tagz' in 'show' rule (did you mean 'tags'?)",
        ),
        (
            "licenses = ['notice']",
            "//:x: no such attribute 'licenses' in 'show' rule",
        ),
        (
            "distribs = None",
            "//:x: no such attribute 'distribs' in 'show' rule",
        ),
        (
            "visibility = ['//visibility:pubic']",
            "//:x Invalid visibility label '//visibility:pubic'; did you mean //visibility:public or //visibility:private?",
        ),
    ] {
        let ws = workspace(&format!(
            "load(':defs.bzl', 'show')\nshow(name = 'x', {argument})\n"
        ));
        rejected(&ws.run(&["query", "//:all"]), message);
    }
}

#[test]
fn unmodeled_builtin_values_fail_analysis_explicitly() {
    for (argument, feature) in [
        (
            "target_compatible_with = ['//:c']",
            "attribute 'target_compatible_with' = [\"//:c\"] (incompatible target skipping (constraint analysis))",
        ),
        (
            "compatible_with = ['//:c']",
            "attribute 'compatible_with' = [\"//:c\"] (constraint environments)",
        ),
        (
            "exec_compatible_with = ['//:c']",
            "attribute 'exec_compatible_with' = [\"//:c\"] (execution platform resolution)",
        ),
        (
            "toolchains = ['//:c']",
            "attribute 'toolchains' = [\"//:c\"] (TemplateVariableInfo Make variables)",
        ),
        (
            "applicable_licenses = ['//:c']",
            "attribute 'package_metadata' = [\"//:c\"] (package metadata dependencies)",
        ),
        (
            "aspect_hints = ['//:c']",
            "attribute 'aspect_hints' = [\"//:c\"] (aspect hints)",
        ),
        (
            "expect_failure = 'boom'",
            "attribute 'expect_failure' = \"boom\" (expected analysis failure)",
        ),
    ] {
        let ws = workspace(&format!(
            "load(':defs.bzl', 'show')\nshow(name = 'c')\nshow(name = 'x', {argument})\n"
        ));
        let query = ws.run(&["query", "//:all"]);
        assert!(query.status.success(), "{argument}: {}", errors(&query));
        rejected(&ws.run(&["build", "//:x"]), feature);
    }
    // Explicit empty values are modeled; package metadata defaults are not.
    let ws = workspace(
        "load(':defs.bzl', 'show')\npackage(default_package_metadata = ['//:m'])\nshow(name = 'm', package_metadata = [])\nshow(name = 'x')\n",
    );
    assert!(ws.run(&["build", "//:m"]).status.success());
    rejected(
        &ws.run(&["build", "//:x"]),
        "package default_package_metadata (package metadata dependencies)",
    );
}

#[test]
fn test_and_executable_rule_builtins() {
    let defs = r#"
def _impl(ctx):
    return []
my_test = rule(implementation = _impl, test = True)
my_binary = rule(implementation = _impl, executable = True)
plain = rule(implementation = _impl, attrs = {"size": attr.int(), "licenses": attr.string_list()})
"#;
    let ws = Workspace::new(&[
        ("defs.bzl", defs),
        (
            "BUILD.bazel",
            "load(':defs.bzl', 'my_test', 'my_binary', 'plain')\nmy_test(name = 'ok_test', size = 'small', timeout = 'long', flaky = True, shard_count = 2, local = True, args = ['-v'], tags = ['manual'])\nmy_binary(name = 'bin', args = ['x'], output_licenses = ['notice'])\nplain(name = 'p', size = 3, licenses = ['x'])\n",
        ),
    ]);
    let query = ws.run(&["query", "//:all"]);
    assert!(query.status.success(), "{}", errors(&query));
    assert_eq!(stdout(&query), "//:bin\n//:ok_test\n//:p\n");
    for (call, message) in [
        (
            "my_test(name = 'x_test', size = 'huge')",
            "In rule 'x_test', size 'huge' is not a valid size.",
        ),
        (
            "my_test(name = 'x_test', timeout = 'forever')",
            "In rule 'x_test', timeout 'forever' is not a valid timeout.",
        ),
        (
            "my_test(name = 'x_test', size = select({'//conditions:default': 'small'}))",
            "//:x_test: attribute \"size\" is not configurable",
        ),
        (
            "my_binary(name = 'x', size = 'small')",
            "//:x: no such attribute 'size' in 'my_binary' rule",
        ),
    ] {
        ws.file(
            "BUILD.bazel",
            &format!("load(':defs.bzl', 'my_test', 'my_binary', 'plain')\n{call}\n"),
        );
        rejected(&ws.run(&["query", "//:all"]), message);
    }
    // Built-in attributes cannot be overridden (RuleClass.java:1301-1306);
    // `licenses`/`distribs` and test attributes of non-test rules are free.
    for (definition, message) in [
        (
            "rule(implementation = _impl, test = True, attrs = {'size': attr.string()})",
            "attribute `size`: built-in attributes cannot be overridden.",
        ),
        (
            "rule(implementation = _impl, executable = True, attrs = {'args': attr.string_list()})",
            "attribute `args`: built-in attributes cannot be overridden.",
        ),
        (
            "rule(implementation = _impl, attrs = {'features': attr.string_list()})",
            "attribute `features`: built-in attributes cannot be overridden.",
        ),
        (
            "rule(implementation = _impl, attrs = {'_config_dependencies': attr.label_list()})",
            "attribute `_config_dependencies`: built-in attributes cannot be overridden.",
        ),
    ] {
        ws.file(
            "defs.bzl",
            &format!("def _impl(ctx):\n    return []\nbad = {definition}\n"),
        );
        ws.file("BUILD.bazel", "load(':defs.bzl', 'bad')\n");
        rejected(&ws.run(&["query", "//:all"]), message);
    }
}

#[test]
fn order_independent_lists_and_generator_attributes_match_bazel() {
    let defs = r#"
def _impl(ctx):
    out = ctx.actions.declare_file(ctx.label.name + ".txt")
    a = ctx.attr
    ctx.actions.write(out, repr([a.tags, a.features, [str(label) for label in a.visibility], a.generator_name, a.generator_function, a.generator_location]) + "\n")
    return [DefaultInfo(files = depset([out]))]
dump = rule(implementation = _impl)
def dump_macro(name, **kwargs):
    dump(name = name + "_x", **kwargs)
"#;
    // tags/features/visibility are orderIndependent: sorted, duplicates kept
    // (BuildType.java:248-253); generator_* follow Rule.java:395-405.
    let ws = Workspace::new(&[
        ("defs.bzl", defs),
        ("sub/BUILD.bazel", ""),
        (
            "BUILD.bazel",
            "load(':defs.bzl', 'dump', 'dump_macro')\ndump(name = 'p', tags = ['z', 'a', 'z'], features = ['b', 'a', 'b', '-a'] + select({'//conditions:default': ['0']}), visibility = ['//sub:__pkg__', '//:__pkg__'])\ndump_macro  (name = 'm')\ndump(name = 'g', generator_name = 'n', generator_function = 'f', generator_location = 'l')\n",
        ),
    ]);
    let built = contents(&ws.run(&["build", "//:p", "//:m_x", "//:g"]));
    assert_eq!(
        built,
        [
            // Outputs are listed in label order: //:g, //:m_x, //:p.
            "[[], [], [], \"n\", \"f\", \"l\"]\n".to_owned(),
            format!(
                "[[], [], [], \"m\", \"dump_macro\", \"{}/BUILD.bazel:3:13\"]\n",
                ws.path().display()
            ),
            "[[\"a\", \"z\", \"z\"], [\"-a\", \"0\", \"a\", \"b\", \"b\"], [\"@@//:__pkg__\", \"@@//sub:__pkg__\"], \"\", \"\", \"\"]\n".to_owned(),
        ]
    );
}

#[test]
fn constraint_and_spelling_checks_match_bazel() {
    // RuleContextConstraintSemantics.validateAttributes: explicit empty
    // restricted_to fails after the rule's implementation.
    let ws = workspace(
        "load(':defs.bzl', 'show')\nshow(name = 'p', restricted_to = [])\nfilegroup(name = 'fg', restricted_to = [])\nshow(name = 'ok', compatible_with = [])\n",
    );
    rejected(
        &ws.run(&["build", "//:p"]),
        "in restricted_to attribute of show rule //:p: attribute cannot be empty",
    );
    rejected(
        &ws.run(&["build", "//:fg"]),
        "in restricted_to attribute of filegroup rule //:fg: attribute cannot be empty",
    );
    assert!(ws.run(&["build", "//:ok"]).status.success());
    // Private attributes are UNDOCUMENTED and never suggested.
    for (argument, message) in [
        (
            "config_dependencies = []",
            "//:x: no such attribute 'config_dependencies' in 'show' rule\n",
        ),
        (
            "_action_listener = []",
            "//:x: no such attribute '_action_listener' in 'show' rule\n",
        ),
    ] {
        let ws = workspace(&format!(
            "load(':defs.bzl', 'show')\nshow(name = 'x', {argument})\n"
        ));
        rejected(&ws.run(&["query", "//:all"]), message);
    }
    // A Starlark `_action_listener` ($action_listener) has the public name of
    // the built-in :action_listener (RuleClass.java:1905-1918).
    let ws = Workspace::new(&[
        (
            "defs.bzl",
            "def _impl(ctx):\n    return []\nbad = rule(implementation = _impl, attrs = {'_action_listener': attr.label_list()})\n",
        ),
        ("BUILD.bazel", "load(':defs.bzl', 'bad')\nbad(name = 'b')\n"),
    ]);
    rejected(
        &ws.run(&["query", "//:all"]),
        "Rule bad: Attributes $action_listener and :action_listener have an identical public name: _action_listener",
    );
    // An edge that is both testonly and deprecated warns before failing.
    let ws = Workspace::new(&[
        ("defs.bzl", DEFS),
        (
            "old/BUILD.bazel",
            "load('//:defs.bzl', 'show')\nshow(name = 'both', testonly = True, deprecation = 'gone', visibility = ['//visibility:public'])\n",
        ),
        (
            "BUILD.bazel",
            "load(':defs.bzl', 'show')\nshow(name = 'user', deps = ['//old:both'])\n",
        ),
    ]);
    let output = ws.run(&["build", "//:user"]);
    rejected(
        &output,
        "in show rule //:user: non-test target '//:user' depends on testonly target '//old:both' and doesn't have testonly attribute set",
    );
    assert!(
        warnings(&output).contains(
            "WARNING: in show rule //:user: target '//:user' depends on deprecated target '//old:both': gone\n"
        ),
        "{}",
        errors(&output)
    );
}
