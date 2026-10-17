use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

struct Workspace(tempfile::TempDir);

impl Workspace {
    fn new(defs: &str, build: &str) -> Self {
        let ws = Self(
            tempfile::Builder::new()
                .prefix("package-specification-cli-")
                .tempdir_in("/tmp/opencode")
                .unwrap(),
        );
        ws.file("MODULE.bazel", "module(name='package_fixture')\n");
        ws.file("BUILD.bazel", build);
        ws.file("defs/BUILD.bazel", "");
        ws.file("defs/rules.bzl", defs);
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

    fn untouched(&self) {
        assert!(!self.path().join("executed-marker").exists());
        assert!(!self.path().join("private-cache/actions").exists());
        assert!(!self.path().join("bazel-rs-out").exists());
    }

    fn marker_rule(&self) -> String {
        format!(
            "genrule(name='first',outs=['first.out'],cmd=\"touch '{}'; printf first > $@\",visibility=['//visibility:public'])\n",
            self.path().join("executed-marker").display()
        )
    }

    fn reject_build(&self, roots: &[&str], diagnostic: &str) {
        let build = fs::read_to_string(self.path().join("BUILD.bazel")).unwrap();
        if !build.contains("name='first'") {
            self.file("BUILD.bazel", &(build + &self.marker_rule()));
        }
        let mut args = vec!["build", ":first"];
        args.extend_from_slice(roots);
        let output = self.run(&args);
        rejected(&output, diagnostic);
        assert!(
            !errors(&output).contains("CALLBACK_RAN"),
            "{}",
            errors(&output)
        );
        self.untouched();
    }
}

fn errors(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

fn success(output: &Output) {
    assert!(output.status.success(), "{}", errors(output));
}

fn rejected(output: &Output, diagnostic: &str) {
    assert!(!output.status.success(), "unexpected success");
    assert!(output.stdout.is_empty(), "partial output on failure");
    assert!(errors(output).contains(diagnostic), "{}", errors(output));
    assert!(!errors(output).contains("panicked"), "{}", errors(output));
}

fn outputs(output: &Output) -> Vec<PathBuf> {
    success(output);
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .map(|line| PathBuf::from(line.split_once('\t').unwrap().1))
        .collect()
}

fn content(output: &Output) -> Vec<Vec<u8>> {
    outputs(output)
        .iter()
        .map(|path| fs::read(path).unwrap())
        .collect()
}

const LOAD: &str = "load('//defs:rules.bzl','check')\n";

// Positive consumers write their own output; they never return the native Info.
const CHECK: &str = r#"
def _check(ctx):
    p = ctx.attr.g[PackageSpecificationInfo]
    answers = [str(p.contains(s)) for s in ctx.attr.targets]
    out = ctx.actions.declare_file(ctx.label.name + '.txt')
    ctx.actions.write(out, '|'.join(answers))
    return [DefaultInfo(files=depset([out]))]
check = rule(implementation=_check, attrs={
    'g': attr.label(providers=[PackageSpecificationInfo]),
    'targets': attr.string_list(),
})
"#;

fn checking(body: &str, attrs: &str) -> String {
    format!(
        "def _check(ctx):\n{body}\n    out = ctx.actions.declare_file(ctx.label.name + '.txt')\n    ctx.actions.write(out, 'checked')\n    return [DefaultInfo(files=depset([out]))]\ncheck = rule(implementation=_check, attrs={{{attrs}}})\n"
    )
}

const GROUP_ATTR: &str = "'g': attr.label(providers=[PackageSpecificationInfo])";

#[test]
fn package_group_real_consumer_writes_and_caches_own_output() {
    let ws = Workspace::new(
        CHECK,
        &format!(
            "{LOAD}package_group(name='g',packages=['//foo/...','-//foo/private'])\ncheck(name='check',g=':g',targets=['//foo:x','//foo/private:x','//foobar:x'])\n"
        ),
    );
    success(&ws.run(&["query", ":check"]));
    ws.untouched();
    let group = ws.run(&["build", ":g"]);
    assert!(outputs(&group).is_empty());
    assert!(errors(&group).contains("0 actions executed"));
    ws.untouched();
    let cold = ws.run(&["build", ":check"]);
    assert_eq!(content(&cold), [b"True|False|False".to_vec()]);
    assert!(errors(&cold).contains("1 actions executed, 0 cache hits"));
    let warm = ws.run(&["build", ":check"]);
    assert_eq!(outputs(&warm), outputs(&cold));
    assert_eq!(content(&warm), content(&cold));
    assert!(errors(&warm).contains("0 actions executed, 1 cache hits"));
}

#[test]
fn membership_exact_subtree_root_public_private_and_local_exclusions() {
    for (packages, expected) in [
        ("['//foo']", "False|True|False|False|False"),
        ("['//foo/...']", "False|True|True|False|False"),
        ("['//']", "True|False|False|False|False"),
        ("['//...']", "True|True|True|True|False"),
        ("['public']", "True|True|True|True|True"),
        ("['private']", "False|False|False|False|False"),
        ("[]", "False|False|False|False|False"),
        ("['-//foo']", "False|False|False|False|False"),
        ("['//foo/...','-//foo/sub']", "False|True|False|False|False"),
        ("['-//foo/sub','//foo/...']", "False|True|False|False|False"),
        ("['//foo:__pkg__']", "False|True|False|False|False"),
        ("['//foo:__subpackages__']", "False|True|True|False|False"),
    ] {
        let ws = Workspace::new(
            CHECK,
            &format!(
                "{LOAD}package_group(name='g',packages={packages})\ncheck(name='check',g=':g',targets=['//:x','//foo:a','//foo/sub:b','//foobar:x','@@other//foo:x'])\n"
            ),
        );
        assert_eq!(
            content(&ws.run(&["build", ":check"])),
            [expected.as_bytes().to_vec()],
            "{packages}"
        );
    }
}

#[test]
fn includes_union_preserves_group_boundaries_and_repeated_diamonds() {
    let ws = Workspace::new(CHECK, "");
    let mut build = format!(
        "{LOAD}package_group(name='leaf',packages=['//foo/private','//other'])\npackage_group(name='left',packages=['//foo/...','-//foo/private'],includes=[':leaf'])\npackage_group(name='right',packages=['-//other'],includes=[':leaf',':leaf'])\npackage_group(name='g0',packages=['-//foo/...'],includes=[':left',':right'])\n"
    );
    // This compact DAG represents far more paths than nodes. Flattening recursively
    // without canonical-node reuse becomes exponential long before this finishes.
    for n in 1..32 {
        build.push_str(&format!(
            "package_group(name='g{n}',packages=['-//other'],includes=[':g{}',':g{}',':leaf'])\n",
            n - 1,
            n - 1
        ));
    }
    build.push_str("check(name='check',g=':g31',targets=['//foo:x','//foo/private:x','//other:x','//absent:x'])\n");
    ws.file("BUILD.bazel", &build);
    assert_eq!(
        content(&ws.run(&["build", ":check"])),
        [b"True|True|True|False".to_vec()]
    );
}

#[test]
fn provider_shape_equality_keeps_distinct_membership_and_bound_callbacks() {
    let ws = Workspace::new(
        &format!(
            r#"
load('//helpers:factory.bzl', 'make_check')
Pkg = PackageSpecificationInfo
def _check_key():
    if type(Pkg) != 'Provider' or dir(Pkg) != [] or Pkg == DefaultInfo or repr(Pkg) != '<function PackageSpecificationInfo>':
        fail('provider key shape')
    if {{Pkg: 'key'}}[PackageSpecificationInfo] != 'key':
        fail('provider key identity')
_check_key()
check = rule(implementation=make_check(Pkg), attrs={{{GROUP_ATTR}, 'gs': attr.label_list(providers=[Pkg])}})
"#
        ),
        &format!(
            "{LOAD}package_group(name='g',packages=['//foo'])\npackage_group(name='other',packages=['//bar'])\ncheck(name='check',g=':g',gs=[':other'])\n"
        ),
    );
    ws.file("helpers/BUILD.bazel", "");
    ws.file(
        "helpers/factory.bzl",
        r#"
def make_check(Pkg):
    def _check(ctx):
        g = ctx.attr.g
        p = g[Pkg]
        q = ctx.attr.gs[0][Pkg]
        if type(p) != 'PackageSpecificationInfo' or dir(p) != ['contains'] or repr(p) != 'struct()':
            fail('instance shape')
        values = {p: 'first'}
        values[q] = 'second'
        if p != q or len(values) != 1 or values[p] != 'second':
            fail('upstream value equality')
        methods = [p.contains, q.contains]
        if [m(Label('//foo:one')) for m in methods] != [True, False]:
            fail('equal providers collapsed distinct package sets')
        if not methods[0]('//foo:different_target') or not methods[1]('//bar:x'):
            fail('bound method lost owner')
        if DefaultInfo not in g or g.files.to_list() != [] or g[DefaultInfo].files.to_list() != []:
            fail('group must expose empty DefaultInfo')
        out = ctx.actions.declare_file(ctx.label.name + '.txt')
        ctx.actions.write(out, 'checked')
        return [DefaultInfo(files=depset([out]))]
    return _check
"#,
    );
    assert_eq!(
        content(&ws.run(&["build", ":check"])),
        [b"checked".to_vec()]
    );
}

#[test]
fn real_external_repositories_separate_canonical_strings_from_mapped_labels() {
    let ws = Workspace::new(
        &checking(
            "    p = ctx.attr.g[PackageSpecificationInfo]\n    values = [p.contains(Label('@alias//foo:x')), p.contains('@alias//foo:x'), p.contains('@ext+//foo:x'), p.contains('@@ext+//foo:x'), p.contains('//foo:x')]\n    if values != [True, False, True, True, False]:\n        fail(str(values))",
            GROUP_ATTR,
        ),
        &format!(
            "{LOAD}package_group(name='mapped',packages=['@alias//foo:__pkg__'])\npackage_group(name='canonical',packages=['@@ext+//foo'])\npackage_group(name='g',includes=[':mapped',':canonical','@alias//:g'])\ncheck(name='check',g=':g')\n"
        ),
    );
    ws.file("MODULE.bazel", "module(name='package_fixture')\nbazel_dep(name='ext',version='1',repo_name='alias')\nlocal_path_override(module_name='ext',path='repos/ext')\n");
    ws.file("repos/ext/MODULE.bazel", "module(name='ext',version='1')\n");
    ws.file("repos/ext/BUILD.bazel", "load(':external.bzl','external')\npackage_group(name='g',packages=['//foo/...'])\nexternal(name='external',g=':g')\n");
    ws.file("repos/ext/external.bzl", &checking(
        "    p = ctx.attr.g[PackageSpecificationInfo]\n    if not p.contains(Label('//foo:x')) or p.contains('//foo:x') or not p.contains('@@ext+//foo:x'):\n        fail('external string context was incorrectly mapped')",
        GROUP_ATTR,
    ).replace("check = rule", "external = rule"));
    assert_eq!(
        content(&ws.run(&["build", ":check"])),
        [b"checked".to_vec()]
    );
    assert_eq!(
        content(&ws.run(&["build", "@alias//:external"])),
        [b"checked".to_vec()]
    );
}

#[test]
fn contains_is_positional_typed_absolute_and_immutable() {
    for expression in [
        "p.contains()",
        "p.contains('//foo:x','//foo:y')",
        "p.contains(target='//foo:x')",
        "p.contains(unknown='//foo:x')",
        "p.contains('//foo:x',target='//foo:x')",
        "p.contains(1)",
        "p.contains(None)",
        "p.contains(['//foo:x'])",
        "p.contains(':x')",
        "p.contains('foo:x')",
        "p.contains('')",
        "p.contains = 1",
    ] {
        let ws = Workspace::new(
            &checking(
                &format!("    p = ctx.attr.g[PackageSpecificationInfo]\n    {expression}"),
                GROUP_ATTR,
            ),
            &format!(
                "{LOAD}package_group(name='g',packages=['//foo'])\ncheck(name='check',g=':g')\n"
            ),
        );
        success(&ws.run(&["query", ":check"]));
        ws.reject_build(&[":check"], "");
    }
}

#[test]
fn declaration_arguments_and_grammar_fail_during_loading() {
    for declaration in [
        "package_group()",
        "package_group('g')",
        "package_group(name=1)",
        "package_group(name='g',visibility=['//visibility:public'])",
        "package_group(name='g',licenses=[])",
        "package_group(name='g',packages=None)",
        "package_group(name='g',packages='//foo')",
        "package_group(name='g',packages=[1])",
        "package_group(name='g',includes=[1])",
        "package_group(name='g',includes=None)",
    ] {
        let ws = Workspace::new("", &format!("{declaration}\n"));
        rejected(&ws.run(&["query", "//:all"]), "");
        ws.untouched();
    }
    for spec in [
        "-public",
        "-private",
        "foo",
        "",
        "//foo:ordinary",
        "//foo/",
        "//foo//bar",
    ] {
        let ws = Workspace::new(
            "",
            &format!("package_group(name='g',packages=[{spec:?}])\n"),
        );
        rejected(&ws.run(&["query", ":g"]), "");
        ws.untouched();
    }
    // Bazel's package-name grammar (LabelValidator.java:50-61) accepts any
    // printable ASCII except ':' and '\\'; non-ASCII names are invalid.
    for spec in [
        "//space here",
        "//foo/**",
        "//foo<bar",
        "//(group)/tt@x/...",
    ] {
        let ws = Workspace::new(
            "",
            &format!("package_group(name='g',packages=[{spec:?}])\n"),
        );
        success(&ws.run(&["query", ":g"]));
        ws.untouched();
    }
    let ws = Workspace::new("", "package_group(name='g',packages=['//caf\u{e9}'])\n");
    rejected(
        &ws.run(&["query", ":g"]),
        "invalid package name 'caf\u{e9}': package names may contain A-Z",
    );
    ws.untouched();
}

#[test]
fn native_legacy_declaration_accepts_tuples_and_label_includes() {
    let ws = Workspace::new(
        &format!(
            "{CHECK}\ndef declare():\n    native.package_group(name='g',packages=('//other',),includes=(Label('//:base'),))\n"
        ),
        "load('//defs:rules.bzl','check','declare')\npackage_group(name='base',packages=('//foo',),includes=())\ndeclare()\ncheck(name='check',g=':g',targets=['//foo:x','//other:x','//missing:x'])\n",
    );
    assert_eq!(
        content(&ws.run(&["build", ":check"])),
        [b"True|True|False".to_vec()]
    );
}

#[test]
fn required_provider_group_placement_uses_normalized_display_names_not_satisfaction() {
    for requirement in [
        "[PackageSpecificationInfo,PackageSpecificationInfo]",
        "(PackageSpecificationInfo,)",
        "((PackageSpecificationInfo,), (DefaultInfo,))",
        "[PackageSpecificationInfo,Custom]",
        "[[PackageSpecificationInfo,Custom]]",
        "[NotPackageSpecificationInfo]",
        "[NotPackageSpecificationProvider]",
    ] {
        for (kind, dep, targets) in [
            ("label", "':g'", "[ctx.attr.dep]"),
            // A label twice in a label list is a Bazel package error
            // (RuleClass.checkForDuplicateLabels), so the list names it once.
            ("label_list", "[':g']", "ctx.attr.dep"),
        ] {
            let defs = format!(
                "Custom=provider()\nNotPackageSpecificationInfo=provider()\nNotPackageSpecificationProvider=provider()\n{}",
                checking(
                    &format!(
                        "    for g in {targets}:\n        if Custom in g or NotPackageSpecificationInfo in g or NotPackageSpecificationProvider in g:\n            fail('fabricated custom provider')\n        if not g[PackageSpecificationInfo].contains('//foo:x'):\n            fail('missing actual group provider')"
                    ),
                    &format!("'dep':attr.{kind}(providers={requirement})"),
                )
            );
            let ws = Workspace::new(
                &defs,
                &format!(
                    "{LOAD}package_group(name='g',packages=['//foo'])\ncheck(name='check',dep={dep})\n"
                ),
            );
            assert_eq!(
                content(&ws.run(&["build", ":check"])),
                [b"checked".to_vec()],
                "{kind}: {requirement}"
            );
        }
    }
}

#[test]
fn unrestricted_defaultinfo_and_native_srcs_reject_misplaced_groups() {
    for requirement in [
        "",
        "providers=[]",
        "providers=()",
        "providers=[[]]",
        "providers=[[],[PackageSpecificationInfo]]",
        "providers=[[PackageSpecificationInfo],()]",
        "providers=[DefaultInfo]",
    ] {
        for (kind, dep) in [("label", "':g'"), ("label_list", "[':g']")] {
            let defs = checking(
                "    fail('CALLBACK_RAN')",
                &format!("'dep':attr.{kind}({requirement})"),
            );
            let ws = Workspace::new(
                &defs,
                &format!("{LOAD}package_group(name='g')\ncheck(name='check',dep={dep})\n"),
            );
            success(&ws.run(&["query", ":check"]));
            ws.reject_build(&[":check"], "misplaced");
        }
    }
    for declaration in [
        "filegroup(name='use',srcs=[':g'])",
        "genrule(name='use',srcs=[':g'],outs=['use.out'],cmd='printf forbidden > $@')",
    ] {
        let ws = Workspace::new("", &format!("package_group(name='g')\n{declaration}\n"));
        success(&ws.run(&["query", ":use"]));
        ws.reject_build(&[":use"], "misplaced");
    }
}

#[test]
fn constraint_constructor_validates_every_branch_before_any_normalization() {
    for requirement in [
        "None",
        "1",
        "'legacy'",
        "['PackageSpecificationInfo']",
        "[['legacy']]",
        "[None]",
        "[True]",
        "[1]",
        "[DefaultInfo()]",
        "[provider()]",
        "[PackageSpecificationInfo,[DefaultInfo]]",
        "[[[PackageSpecificationInfo]]]",
        "[[],[None]]",
        "[[],[provider()]]",
        "[[],[DefaultInfo()]]",
        "[[],['legacy']]",
    ] {
        for kind in ["label", "label_list"] {
            let ws = Workspace::new(
                &checking(
                    "    fail('CALLBACK_RAN')",
                    &format!("'dep':attr.{kind}(providers={requirement})"),
                ),
                &format!("{LOAD}check(name='check')\n"),
            );
            let result = ws.run(&["query", ":check"]);
            rejected(&result, "");
            assert!(!errors(&result).contains("CALLBACK_RAN"));
            ws.untouched();
        }
    }
}

#[test]
fn ordinary_rules_check_actual_and_or_providers_per_label_and_list_element() {
    const PRODUCERS: &str = r#"
Custom = provider()
Other = provider()
def _produce(ctx):
    if ctx.attr.mode == 'both':
        return [Custom(), platform_common.TemplateVariableInfo({'X':'x'})]
    if ctx.attr.mode == 'custom':
        return [Custom()]
    if ctx.attr.mode == 'template':
        return [platform_common.TemplateVariableInfo({'X':'x'})]
    if ctx.attr.mode == 'other':
        return [Other()]
    return []
produce = rule(implementation=_produce, attrs={'mode':attr.string()})
"#;
    for (requirement, mode, accepts) in [
        (
            "[Custom,platform_common.TemplateVariableInfo,DefaultInfo]",
            "both",
            true,
        ),
        (
            "(Custom,platform_common.TemplateVariableInfo)",
            "custom",
            false,
        ),
        (
            "[Custom,platform_common.TemplateVariableInfo]",
            "template",
            false,
        ),
        (
            "[[Custom,platform_common.TemplateVariableInfo],[Other]]",
            "both",
            true,
        ),
        (
            "((Custom,platform_common.TemplateVariableInfo),(Other,))",
            "other",
            true,
        ),
        (
            "[[Custom,platform_common.TemplateVariableInfo],[Other]]",
            "custom",
            false,
        ),
        ("[DefaultInfo]", "empty", true),
        ("[]", "empty", true),
        ("()", "empty", true),
        ("[[],[Custom]]", "empty", true),
        ("[PackageSpecificationInfo]", "empty", false),
    ] {
        for (kind, dep) in [
            ("label", "':tested'"),
            ("label_list", "[':both',':tested']"),
        ] {
            // Every positive producer returns actual instances, without provides=.
            // In the list case :both cannot hide the following bad prerequisite.
            let body = if accepts {
                "    pass"
            } else {
                "    fail('CALLBACK_RAN')"
            };
            let ws = Workspace::new(
                &format!(
                    "{PRODUCERS}\n{}",
                    checking(body, &format!("'dep':attr.{kind}(providers={requirement})"))
                ),
                &format!(
                    "load('//defs:rules.bzl','check','produce')\nproduce(name='both',mode='both')\nproduce(name='tested',mode='{mode}')\ncheck(name='check',dep={dep})\n"
                ),
            );
            success(&ws.run(&["query", ":check"]));
            if accepts {
                assert_eq!(
                    content(&ws.run(&["build", ":check"])),
                    [b"checked".to_vec()]
                );
            } else {
                ws.reject_build(&[":check"], "provider");
            }
        }
    }
    let ws = Workspace::new(
        &checking("    fail('CALLBACK_RAN')", GROUP_ATTR),
        &format!("{LOAD}filegroup(name='ordinary')\ncheck(name='check',g=':ordinary')\n"),
    );
    success(&ws.run(&["query", ":check"]));
    ws.reject_build(&[":check"], "provider");
}

#[test]
fn provider_alias_dedup_uses_export_identity_not_display_name() {
    for (requirement, accepts) in [("[P,Alias,P]", true), ("[P,Q]", false)] {
        let ws = Workspace::new(
            &format!(
                "load('//a:provider.bzl',P='Same')\nload('//relay:provider.bzl',Alias='Again')\nload('//b:provider.bzl',Q='Same')\ndef _produce(ctx):\n    return [P()]\nproduce=rule(implementation=_produce)\n{}",
                checking(
                    if accepts {
                        "    pass"
                    } else {
                        "    fail('CALLBACK_RAN')"
                    },
                    &format!("'dep':attr.label(providers={requirement})")
                )
            ),
            "load('//defs:rules.bzl','check','produce')\nproduce(name='produce')\ncheck(name='check',dep=':produce')\n",
        );
        for package in ["a", "b", "relay"] {
            ws.file(&format!("{package}/BUILD.bazel"), "");
        }
        ws.file("a/provider.bzl", "Same=provider()\n");
        ws.file("b/provider.bzl", "Same=provider()\n");
        ws.file(
            "relay/provider.bzl",
            "load('//a:provider.bzl',Original='Same')\nAgain=Original\n",
        );
        if accepts {
            assert_eq!(
                content(&ws.run(&["build", ":check"])),
                [b"checked".to_vec()]
            );
        } else {
            success(&ws.run(&["query", ":check"]));
            ws.reject_build(&[":check"], "provider");
        }
    }
}

#[test]
fn allowed_source_and_generated_files_skip_ordinary_provider_predicate() {
    for (kind, dep, targets) in [
        ("label", "'data'", "[ctx.attr.dep]"),
        ("label_list", "['data',':generated.out']", "ctx.attr.dep"),
        ("label", "':generated.out'", "[ctx.attr.dep]"),
    ] {
        let ws = Workspace::new(
            &format!(
                "Custom=provider()\n{}",
                checking(
                    &format!(
                        "    for dep in {targets}:\n        if Custom in dep or DefaultInfo not in dep or len(dep.files.to_list()) != 1:\n            fail('file prerequisite shape')"
                    ),
                    &format!("'dep':attr.{kind}(providers=[Custom],allow_files=True)"),
                )
            ),
            &format!(
                "{LOAD}exports_files(['data'])\ngenrule(name='generated',outs=['generated.out'],cmd='printf generated > $@')\ncheck(name='check',dep={dep})\n"
            ),
        );
        ws.file("data", "source-content");
        assert_eq!(
            content(&ws.run(&["build", ":check"])),
            [b"checked".to_vec()]
        );
    }
    for kind in ["label", "label_list"] {
        let dep = if kind == "label" {
            "'data'"
        } else {
            "['data']"
        };
        let ws = Workspace::new(
            &format!(
                "Custom=provider()\n{}",
                checking(
                    "    fail('CALLBACK_RAN')",
                    &format!("'dep':attr.{kind}(providers=[Custom],allow_files=False)")
                )
            ),
            &format!("{LOAD}exports_files(['data'])\ncheck(name='check',dep={dep})\n"),
        );
        ws.file("data", "source-content");
        ws.reject_build(&[":check"], "");
    }
}

#[test]
fn includes_validate_kind_and_cycles_without_callbacks_or_implicit_sources() {
    for (other, include, diagnostic) in [
        ("", ":missing", "not declared"),
        ("filegroup(name='wrong')\n", ":wrong", "package group"),
        ("bad(name='wrong')\n", ":wrong", "package group"),
        ("exports_files(['data'])\n", ":data", "package group"),
        ("", ":data", "not declared"),
        ("", ":g", "cycle"),
        (
            "package_group(name='other',includes=[':g'])\n",
            ":other",
            "cycle",
        ),
    ] {
        let ws = Workspace::new(
            "def _bad(ctx):\n    fail('CALLBACK_RAN')\nbad=rule(implementation=_bad)\n",
            &format!(
                "load('//defs:rules.bzl','bad')\n{other}package_group(name='g',includes=['{include}'])\n"
            ),
        );
        ws.file("data", "not an implicit includes source");
        success(&ws.run(&["query", ":g"]));
        if other.is_empty() && include == ":data" {
            rejected(&ws.run(&["query", ":data"]), "not declared");
            ws.untouched();
        }
        ws.reject_build(&[":g"], diagnostic);
    }
}

#[test]
fn group_visibility_allows_and_denies_rules_outputs_and_real_source_bytes() {
    for dependency in ["rule", "rule.out", "data"] {
        let ws = Workspace::new("", "");
        ws.file("private/BUILD.bazel", "package_group(name='included',packages=['//allowed'])\npackage_group(name='grant',packages=['//allowed/...','-//allowed'],includes=[':included'])\ngenrule(name='rule',outs=['rule.out'],cmd='printf producer-bytes > $@',visibility=[':grant'])\nexports_files(['data'],visibility=[':grant'])\n");
        ws.file("private/data", "source-bytes");
        for package in ["allowed", "denied"] {
            ws.file(
                &format!("{package}/BUILD.bazel"),
                &format!("filegroup(name='use',srcs=['//private:{dependency}'])\n"),
            );
        }
        ws.reject_build(&["//denied:use"], "not visible");
        let expected = if dependency == "data" {
            b"source-bytes".as_slice()
        } else {
            b"producer-bytes".as_slice()
        };
        assert_eq!(
            content(&ws.run(&["build", "//allowed:use"])),
            [expected.to_vec()]
        );
    }
}

#[test]
fn invalid_effective_visibility_is_validated_for_roots_sources_and_same_package() {
    for (groups, reference, diagnostic) in [
        ("", ":missing", "not declared"),
        ("filegroup(name='wrong')\n", ":wrong", "package group"),
        ("bad(name='wrong')\n", ":wrong", "package group"),
        (
            "package_group(name='cycle',includes=[':cycle'])\n",
            ":cycle",
            "cycle",
        ),
    ] {
        for (declaration, root) in [
            (
                format!(
                    "genrule(name='producer',outs=['producer.out'],cmd='printf forbidden > $@',visibility=['{reference}'])\n"
                ),
                ":producer",
            ),
            (
                format!(
                    "genrule(name='producer',outs=['producer.out'],cmd='printf forbidden > $@',visibility=['{reference}'])\n"
                ),
                ":producer.out",
            ),
            (
                format!("exports_files(['data'],visibility=['{reference}'])\n"),
                ":data",
            ),
            (
                format!(
                    "package(default_visibility=['{reference}'])\nfilegroup(name='establish',srcs=['data'])\n"
                ),
                ":data",
            ),
        ] {
            for via_consumer in [false, true] {
                let ws = Workspace::new(
                    "def _bad(ctx):\n    fail('CALLBACK_RAN')\nbad=rule(implementation=_bad)\n",
                    &format!(
                        "load('//defs:rules.bzl','bad')\n{groups}{declaration}filegroup(name='same_package',srcs=['{root}'],visibility=['//visibility:public'])\n"
                    ),
                );
                ws.file("data", "must not escape source early return");
                success(&ws.run(&["query", root]));
                ws.untouched();
                let target = if via_consumer { ":same_package" } else { root };
                ws.reject_build(&[target], diagnostic);
            }
        }
    }
}

#[test]
fn visibility_uses_both_macro_authorities_and_canonical_repository_identity() {
    for macro_producer in [false, true] {
        for grant in ["//defs", "//app"] {
            for dependency in ["producer", "producer.out", "data"] {
                let ws = Workspace::new(
                    &format!(
                        "def _m(name,visibility):\n    native.filegroup(name=name,srcs=['//private:{dependency}'],visibility=visibility)\nm=macro(implementation=_m)\n"
                    ),
                    &format!("package_group(name='grant',packages=['{grant}'])\n"),
                );
                ws.file("app/BUILD.bazel", &format!("load('//defs:rules.bzl','m')\nm(name='use')\nfilegroup(name='ordinary',srcs=['//private:{dependency}'])\n"));
                if macro_producer {
                    ws.file("producerdefs/BUILD.bazel", "");
                    ws.file("producerdefs/producer.bzl", "def _m(name,visibility):\n    native.genrule(name=name,outs=[name+'.out'],cmd='printf authority > $@',visibility=visibility)\nm=macro(implementation=_m)\n");
                    ws.file("private/BUILD.bazel", "load('//producerdefs:producer.bzl','m')\nm(name='producer',visibility=['//:grant'])\nexports_files(['data'],visibility=['//:grant'])\n");
                } else {
                    ws.file("private/BUILD.bazel", "genrule(name='producer',outs=['producer.out'],cmd='printf authority > $@',visibility=['//:grant'])\nexports_files(['data'],visibility=['//:grant'])\n");
                }
                ws.file("private/data", "authority");
                let (allowed, denied) = if grant == "//defs" {
                    ("//app:use", "//app:ordinary")
                } else {
                    ("//app:ordinary", "//app:use")
                };
                ws.reject_build(&[denied], "not visible");
                assert_eq!(
                    content(&ws.run(&["build", allowed])),
                    [b"authority".to_vec()]
                );
            }
        }
    }
    for grant in ["//defs", "@alias//defs"] {
        let ws = Workspace::new(
            "",
            &format!("package_group(name='grant',packages=['{grant}'])\n"),
        );
        ws.file("MODULE.bazel", "module(name='package_fixture')\nbazel_dep(name='ext',version='1',repo_name='alias')\nlocal_path_override(module_name='ext',path='repos/ext')\n");
        ws.file("repos/ext/MODULE.bazel", "module(name='ext',version='1')\n");
        ws.file("repos/ext/defs/BUILD.bazel", "");
        ws.file("repos/ext/defs/macros.bzl", "def _m(name,visibility):\n    native.filegroup(name=name,srcs=['//private:producer'],visibility=visibility)\nm=macro(implementation=_m)\n");
        // String srcs are resolved at the BUILD invocation, while authority is
        // the external definition's canonical repository/package identity.
        ws.file(
            "app/BUILD.bazel",
            "load('@alias//defs:macros.bzl','m')\nm(name='use')\n",
        );
        ws.file("private/BUILD.bazel", "genrule(name='producer',outs=['producer.out'],cmd='printf canonical > $@',visibility=['//:grant'])\n");
        if grant == "//defs" {
            ws.reject_build(&["//app:use"], "not visible");
        } else {
            assert_eq!(
                content(&ws.run(&["build", "//app:use"])),
                [b"canonical".to_vec()]
            );
        }
    }
}

#[test]
fn macro_groups_are_intrinsically_public_and_rules_only_queries_exclude_them() {
    let ws = Workspace::new(
        &format!(
            "{CHECK}\ndef _m(name,visibility):\n    native.package_group(name=name,packages=['//foo'])\n    native.package_group(name='outside',packages=['//foo'])\nm=macro(implementation=_m)\n"
        ),
        "",
    );
    ws.file("app/BUILD.bazel", "load('//defs:rules.bzl','m','check')\npackage(default_visibility=['//visibility:private'])\nm(name='demo')\ncheck(name='use',g=':demo',targets=['//foo:x'])\n");
    for pattern in ["//app:all", "//app/..."] {
        let query = ws.run(&["query", pattern]);
        success(&query);
        assert_eq!(String::from_utf8_lossy(&query.stdout), "//app:use\n");
    }
    for target in ["//app:demo", "//app:outside"] {
        let query = ws.run(&["query", target]);
        success(&query);
        assert_eq!(
            String::from_utf8_lossy(&query.stdout),
            format!("{target}\n")
        );
    }
    ws.untouched();
    ws.reject_build(&["//app:demo", "//app:outside"], "macro");
    let group = ws.run(&["build", "//app:demo"]);
    assert!(outputs(&group).is_empty());
    ws.untouched();
    assert_eq!(
        content(&ws.run(&["build", "//app:use"])),
        [b"True".to_vec()]
    );
}

#[test]
fn group_collisions_cover_empty_macro_reservations_outputs_and_physical_files() {
    // TargetRecorder texts (packages/TargetRecorder.java:693-715): the
    // later declaration conflicts with the existing one.
    for (other, diagnostics) in [
        ("m(name='g')", ["conflicts", "conflicts"]),
        (
            "genrule(name='r',outs=['g'],cmd='true')",
            [
                "generated file 'g' in rule 'r' conflicts with existing package group, defined at ",
                "package group 'g' conflicts with existing generated file from rule 'r', defined at ",
            ],
        ),
        (
            "filegroup(name='g')",
            [
                "filegroup rule 'g' conflicts with existing package group, defined at ",
                "package group 'g' conflicts with existing filegroup rule, defined at ",
            ],
        ),
        (
            "package_group(name='g')",
            [
                "package group 'g' conflicts with existing package group, defined at ",
                "package group 'g' conflicts with existing package group, defined at ",
            ],
        ),
        (
            "exports_files(['g'])",
            [
                "Error in exports_files: source file 'g' conflicts with existing package group, defined at ",
                "Error in package_group: package group 'g' conflicts with existing source file, defined at ",
            ],
        ),
    ] {
        for (before, diagnostic) in [false, true].into_iter().zip(diagnostics) {
            let group = "package_group(name='g')";
            let declarations = if before {
                format!("{other}\n{group}\n")
            } else {
                format!("{group}\n{other}\n")
            };
            let ws = Workspace::new(
                "def _m(name,visibility):\n    pass\nm=macro(implementation=_m)\n",
                &format!("load('//defs:rules.bzl','m')\n{declarations}"),
            );
            ws.file("g", "physical file must not override group");
            rejected(&ws.run(&["query", "//:all"]), diagnostic);
            ws.untouched();
        }
    }
    let ws = Workspace::new(
        "",
        "package_group(name='g')\nfilegroup(name='use',srcs=['g'])\n",
    );
    ws.file("g", "physical group-named file must not be synthesized");
    ws.reject_build(&[":use"], "misplaced");
}

#[test]
fn macro_public_normalization_drops_group_references_before_resolution() {
    let ws = Workspace::new(
        r#"
def _m(name,visibility):
    if visibility != [Label('//visibility:public')]:
        fail('public normalization did not discard group reference')
    native.genrule(name=name,outs=[name+'.out'],cmd='printf normalized > $@',visibility=visibility)
m=macro(implementation=_m)
"#,
        "load('//defs:rules.bzl','m')\nm(name='demo',visibility=['//visibility:public','//:missing_group'])\n",
    );
    assert_eq!(
        content(&ws.run(&["build", ":demo"])),
        [b"normalized".to_vec()]
    );
    // Ordinary rules follow RuleVisibility.validateAndSimplify too
    // (packages/RuleVisibility.java:185-200, AttributeProvider.java:277-286):
    // public subsumes the list, so the missing group is never resolved.
    let ordinary = Workspace::new(
        "",
        "filegroup(name='mixed',visibility=['//visibility:public','//:missing_group'])\n",
    );
    let query = ordinary.run(&["query", ":mixed"]);
    success(&query);
    assert_eq!(String::from_utf8_lossy(&query.stdout), "//:mixed\n");
    success(&ordinary.run(&["build", ":mixed"]));
}

#[test]
fn native_info_direct_and_nested_custom_returns_are_controlled_unsupported() {
    // Nested in another provider the info is ordinary payload (rules_cc's
    // CcToolchainInfo `_allowlist_for_layering_check`, docs/CC_TOOLCHAIN.md).
    for returned in ["[Box(value=p)]", "[Box(value=[{'nested':p}])]"] {
        let ws = Workspace::new(
            &format!(
                "Box=provider(fields=['value'])\ndef _check(ctx):\n    p=ctx.attr.g[PackageSpecificationInfo]\n    return {returned}\ncheck=rule(implementation=_check,attrs={{{GROUP_ATTR}}})\n"
            ),
            &format!(
                "{LOAD}package_group(name='g',packages=['//foo'])\ncheck(name='check',g=':g')\n"
            ),
        );
        assert!(outputs(&ws.run(&["build", ":check"])).is_empty());
    }
    for returned in ["[p]", "[Box(value=p.contains)]"] {
        let ws = Workspace::new(
            &format!(
                "Box=provider(fields=['value'])\ndef _check(ctx):\n    p=ctx.attr.g[PackageSpecificationInfo]\n    return {returned}\ncheck=rule(implementation=_check,attrs={{{GROUP_ATTR}}})\n"
            ),
            &format!(
                "{LOAD}package_group(name='g',packages=['//foo'])\ncheck(name='check',g=':g')\n"
            ),
        );
        success(&ws.run(&["query", ":check"]));
        // Upstream direct forwarding crashes; this is an intentional bounded
        // native rejection, not a claim of successful upstream forwarding.
        ws.reject_build(&[":check"], "unsupported");
    }
}

#[test]
fn graph_depth_limit_cannot_be_bypassed_by_a_previously_resolved_child() {
    for roots in [vec![":g1", ":g0"], vec![":g0", ":g1"]] {
        let ws = Workspace::new("", "");
        let mut build = String::new();
        for n in 0..257 {
            let includes = if n == 256 {
                "[]".to_owned()
            } else {
                format!("[':g{}']", n + 1)
            };
            build.push_str(&format!(
                "package_group(name='g{n}',packages=['//foo'],includes={includes})\n"
            ));
        }
        ws.file("BUILD.bazel", &build);
        success(&ws.run(&["query", ":g0"]));
        ws.reject_build(&roots, "depth");
        assert!(outputs(&ws.run(&["build", ":g1"])).is_empty());
        ws.untouched();
    }
    let ws = Workspace::new(
        "",
        "package_group(name='leaf',packages=['//foo'])\npackage_group(name='head',includes=[':leaf'])\n",
    );
    assert!(outputs(&ws.run(&["build", ":leaf", ":head"])).is_empty());
    ws.untouched();
}

#[test]
fn provider_globals_are_bzl_only_uncallable_and_module_loaded() {
    for file in ["BUILD.bazel", "MODULE.bazel"] {
        let ws = Workspace::new("", "");
        ws.file(file, "PackageSpecificationInfo\n");
        let target = if file == "MODULE.bazel" {
            "@missing//:all"
        } else {
            "//:all"
        };
        rejected(&ws.run(&["query", target]), "PackageSpecificationInfo");
        ws.untouched();
    }
    let uncallable = Workspace::new(
        "Pkg=PackageSpecificationInfo\nPkg()\n",
        "load('//defs:rules.bzl','Pkg')\n",
    );
    rejected(
        &uncallable.run(&["query", "//:all"]),
        "'Provider' object is not callable",
    );
    uncallable.untouched();
    let ws = Workspace::new(
        r#"
Pkg = PackageSpecificationInfo
def _repo(ctx):
    if type(Pkg) != 'Provider' or Pkg != PackageSpecificationInfo:
        fail('MODULE-loaded provider unavailable')
    ctx.file('BUILD.bazel', "exports_files(['data'])\n")
    ctx.file('data', 'module-provider', executable=False)
make = repository_rule(implementation=_repo)
def _extension(ctx):
    if Pkg != PackageSpecificationInfo:
        fail('extension provider key changed')
    if native.existing_rule('g') != None or native.existing_rules() != {}:
        fail('extension introspection profile changed')
    make(name='made')
ext = module_extension(implementation=_extension)
"#,
        "filegroup(name='use',srcs=['@made//:data'])\n",
    );
    ws.file("MODULE.bazel", "module(name='package_fixture')\ne=use_extension('//defs:rules.bzl','ext')\nuse_repo(e,'made')\n");
    assert_eq!(
        content(&ws.run(&["build", ":use"])),
        [b"module-provider".to_vec()]
    );
}

#[test]
fn group_declaration_rejects_bzl_initialization_module_and_repository_phases() {
    let init = Workspace::new(
        "native.package_group(name='bad')\nvalue=1\n",
        "load('//defs:rules.bzl','value')\n",
    );
    rejected(&init.run(&["query", "//:all"]), "package_group");
    init.untouched();
    let module = Workspace::new("", "");
    module.file(
        "MODULE.bazel",
        "module(name='package_fixture')\npackage_group(name='bad')\n",
    );
    rejected(&module.run(&["query", "@missing//:all"]), "package_group");
    module.untouched();
    for callback in ["repository", "extension"] {
        let defs = if callback == "repository" {
            "def _impl(ctx):\n    native.package_group(name='bad')\n    fail('CALLBACK_RAN')\nmake=repository_rule(implementation=_impl)\n"
        } else {
            "def _impl(ctx):\n    native.package_group(name='bad')\n    fail('CALLBACK_RAN')\nmake=module_extension(implementation=_impl)\n"
        };
        let ws = Workspace::new(defs, "");
        let invocation = if callback == "repository" {
            "make=use_repo_rule('//defs:rules.bzl','make')\nmake(name='made')\n"
        } else {
            "ext=use_extension('//defs:rules.bzl','make')\nuse_repo(ext,'made')\n"
        };
        ws.file(
            "MODULE.bazel",
            &format!("module(name='package_fixture')\n{invocation}"),
        );
        let result = ws.run(&["query", "@made//:all"]);
        rejected(&result, "package_group");
        assert!(!errors(&result).contains("CALLBACK_RAN"));
        ws.untouched();
    }
}
