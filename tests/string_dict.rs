//! Scalar string-dictionary CLI behavior records. No ignored research artifact is
//! a compile dependency. Outputs deliberately join keys/values, never JSON-sort.
//! NATIVE_PROFILE and INTENTIONAL_DIVERGENCE cases are not reference parity claims.
use bazel_rs::label::Label;
use std::collections::BTreeMap;
use std::fs;
use std::path::PathBuf;
use std::process::{Command, Output};

struct Fixture(tempfile::TempDir);
impl Fixture {
    fn new() -> Self {
        let f = Self(
            tempfile::Builder::new()
                .prefix("string-dict-cli-")
                .tempdir_in("/tmp/opencode")
                .unwrap(),
        );
        for directory in ["home", "tmp"] {
            fs::create_dir(f.0.path().join(directory)).unwrap();
        }
        f.file("MODULE.bazel", "module(name='main')\n");
        f.file("BUILD.bazel", "");
        f
    }

    fn file(&self, name: &str, text: impl AsRef<[u8]>) {
        fs::write(self.0.path().join(name), text).unwrap();
    }

    fn run(&self, args: &[&str]) -> Output {
        let result = Command::new("/usr/bin/timeout")
            .args(["--kill-after=5s", "60s", env!("CARGO_BIN_EXE_bazel-rs")])
            .args(["--ignore_all_rc_files", "--cache-dir=cache"])
            .args(args)
            .current_dir(self.0.path())
            .env_clear()
            .env("PATH", "/usr/bin:/bin")
            .env("HOME", self.0.path().join("home"))
            .env("TMPDIR", self.0.path().join("tmp"))
            .output()
            .unwrap();
        assert_ne!(result.status.code(), Some(124), "CLI timed out: {args:?}");
        assert_ne!(
            result.status.code(),
            Some(137),
            "CLI needed forced termination: {args:?}"
        );
        assert!(!errors(&result).contains("panicked"), "{}", errors(&result));
        result
    }

    fn ordinary(&self, constructor: &str, calls: &str) {
        self.file(
            "defs.bzl",
            format!(
                "{WRITE_RULE}\nr=rule(implementation=_impl,attrs={{'mapping':{constructor}}})\n"
            ),
        );
        self.file("BUILD.bazel", format!("load(':defs.bzl','r')\n{calls}\n"));
    }

    fn repository(&self, constructor: &str, calls: &str) {
        self.file("repo.bzl", format!("{REPOSITORY}\nrepo=repository_rule(implementation=_repo,attrs={{'mapping':{constructor}}})\n"));
        self.file(
            "MODULE.bazel",
            format!("module(name='main')\nrepo=use_repo_rule('//:repo.bzl','repo')\n{calls}\n"),
        );
    }

    fn build(&self, targets: &[&str], expected: &[(&str, &[u8])]) -> Built {
        let mut args = vec!["build"];
        args.extend_from_slice(targets);
        let result = self.run(&args);
        assert!(result.status.success(), "{}", errors(&result));
        let mut paths = BTreeMap::new();
        for line in String::from_utf8_lossy(&result.stdout).lines() {
            let (label, path) = line.split_once('\t').expect("label/path output row");
            assert_eq!(Label::parse_canonical(label).unwrap().to_string(), label);
            assert!(!path.is_empty() && !path.contains('\t'));
            assert!(
                paths
                    .insert(label.to_owned(), PathBuf::from(path))
                    .is_none(),
                "duplicate label {label}"
            );
        }
        assert_eq!(
            paths.len(),
            expected.len(),
            "unexpected output rows: {:?}",
            paths.keys()
        );
        for (label, bytes) in expected {
            assert_eq!(
                fs::read(paths.get(*label).unwrap_or_else(|| panic!(
                    "missing canonical label {label}: {:?}",
                    paths.keys()
                )))
                .unwrap(),
                *bytes,
                "bytes for {label}"
            );
        }
        Built {
            paths,
            stderr: errors(&result),
        }
    }

    fn reject(&self, args: &[&str], markers: &[&str]) -> String {
        let output = self.run(args);
        let diagnostic = errors(&output);
        assert_eq!(output.status.code(), Some(1), "{args:?}: {diagnostic}");
        assert!(output.stdout.is_empty(), "partial successful build output");
        // A missing constructor must never masquerade as a successful type test.
        assert!(
            !diagnostic.contains("has no attribute `string_dict`"),
            "{diagnostic}"
        );
        let lower = diagnostic.to_lowercase();
        for marker in markers {
            assert!(
                lower.contains(marker) || (*marker == "frozen" && lower.contains("immutable")),
                "missing {marker:?}: {diagnostic}"
            );
        }
        diagnostic
    }

    fn no_actions(&self) {
        assert!(!self.0.path().join("cache/actions").exists());
    }
}

struct Built {
    paths: BTreeMap<String, PathBuf>,
    stderr: String,
}
fn errors(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}
fn record(keys: &[&str], values: &[&str]) -> Vec<u8> {
    format!(
        "KEYS={}\nVALUES={}\nTYPE=dict\nTYPES={}\n",
        keys.join("|"),
        values.join("|"),
        vec!["string"; values.len()].join("|")
    )
    .into_bytes()
}
const WRITE_RULE: &str = r#"
def _impl(ctx):
    output = ctx.actions.declare_file(ctx.label.name + ".txt")
    value = "KEYS=" + "|".join(ctx.attr.mapping.keys()) + "\nVALUES=" + "|".join(ctx.attr.mapping.values()) + "\nTYPE=" + type(ctx.attr.mapping) + "\nTYPES=" + "|".join([type(v) for v in ctx.attr.mapping.values()]) + "\n"
    ctx.actions.write(output, value)
    return DefaultInfo(files=depset([output]))
"#;
const REPOSITORY: &str = r#"
def _repo(ctx):
    print("STRING_DICT_REPO_CALLBACK")
    ctx.file("BUILD.bazel", "filegroup(name='data',srcs=['value'],visibility=['//visibility:public'])\n")
    ctx.file("value", "KEYS=" + "|".join(ctx.attr.mapping.keys()) + "\nVALUES=" + "|".join(ctx.attr.mapping.values()) + "\nTYPE=" + type(ctx.attr.mapping) + "\nTYPES=" + "|".join([type(v) for v in ctx.attr.mapping.values()]) + "\n")
"#;

#[test]
fn string_dict_constructor_signature_and_defaults() {
    for constructor in [
        "attr.string_dict()",
        "attr.string_dict(True)",
        "attr.string_dict(allow_empty=True, default={}, doc=None, mandatory=False)",
        "attr.string_dict(doc='scalar dictionary')",
    ] {
        let f = Fixture::new();
        f.ordinary(constructor, "r(name='probe')");
        f.build(&["//:probe"], &[("//:probe", &record(&[], &[]))]);
    }
    for (constructor, markers) in [
        ("attr.string_dict(True, 'doc')", vec!["positional"]),
        ("attr.string_dict('doc')", vec!["allow_empty"]),
        ("attr.string_dict(allow_empty=0)", vec!["allow_empty"]),
        ("attr.string_dict(mandatory=1)", vec!["mandatory"]),
        ("attr.string_dict(doc=1)", vec!["doc"]),
        ("attr.string_dict(values=['x'])", vec!["values"]),
        ("attr.string_dict(default=None)", vec!["dict"]),
    ] {
        let f = Fixture::new();
        f.ordinary(constructor, "r(name='probe')");
        f.reject(&["query", "//:probe"], &markers);
        f.no_actions();
    }
}

#[test]
fn string_dict_defaults_and_build_none_have_distinct_meanings() {
    let f = Fixture::new();
    f.ordinary(
        "attr.string_dict(default={'z':'fallback'})",
        "r(name='omitted')\nr(name='none',mapping=None)\nr(name='empty',mapping={})",
    );
    let fallback = record(&["z"], &["fallback"]);
    f.build(
        &["//:none", "//:empty", "//:omitted"],
        &[
            ("//:omitted", &fallback),
            ("//:none", &fallback),
            ("//:empty", &record(&[], &[])),
        ],
    );
}

#[test]
fn string_dict_mandatory_is_presence_not_default_or_nonempty() {
    for argument in ["", ",mapping=None"] {
        let f = Fixture::new();
        f.ordinary(
            "attr.string_dict(mandatory=True,default={'z':'fallback'})",
            &format!("r(name='probe'{argument})"),
        );
        f.reject(&["query", "//:probe"], &["mandatory", "mapping"]);
    }
    let f = Fixture::new();
    f.ordinary(
        "attr.string_dict(mandatory=True)",
        "r(name='probe',mapping={})",
    );
    f.build(&["//:probe"], &[("//:probe", &record(&[], &[]))]);
    f.ordinary(
        "attr.string_dict(False,mandatory=True)",
        "r(name='probe',mapping={})",
    );
    assert!(f.run(&["query", "//:probe"]).status.success());
    f.reject(&["build", "//:probe"], &["attribute must be non empty"]);
}

#[test]
fn string_dict_allow_empty_is_only_an_ordinary_analysis_check() {
    for argument in ["", ",mapping={}"] {
        let f = Fixture::new();
        f.ordinary(
            "attr.string_dict(False)",
            &format!("r(name='probe'{argument})"),
        );
        assert!(f.run(&["query", "//:probe"]).status.success());
        f.reject(&["build", "//:probe"], &["attribute must be non empty"]);
        f.no_actions();
    }
    let f = Fixture::new();
    f.ordinary(
        "attr.string_dict(allow_empty=False)",
        "r(name='probe',mapping={'':''})",
    );
    f.build(&["//:probe"], &[("//:probe", &record(&[""], &[""]))]);
    f.repository("attr.string_dict(False)", "repo(name='made',mapping={})");
    f.build(
        &["@made//:data"],
        &[("@@+repo+made//:data", &record(&[], &[]))],
    );
}

#[test]
fn string_dict_strict_scalar_shape_in_defaults_and_call_inputs() {
    for value in [
        "[]",
        "()",
        "'text'",
        "1",
        "True",
        "{1:'s'}",
        "{False:'s'}",
        "{('k',):'s'}",
        "{'k':[]}",
        "{'k':('s',)}",
        "{'k':1}",
        "{'k':False}",
        "{'k':None}",
        "{'k':{}}",
    ] {
        for default in [false, true] {
            let f = Fixture::new();
            if default {
                f.ordinary(
                    &format!("attr.string_dict(default={value})"),
                    "r(name='probe')",
                );
            } else {
                f.ordinary(
                    "attr.string_dict()",
                    &format!("r(name='probe',mapping={value})"),
                );
            }
            f.reject(&["query", "//:probe"], &["string"]);
            f.no_actions();
        }
    }
    // Ordinary heterogeneous dictionaries are still legal until ascribed.
    let f = Fixture::new();
    f.file("defs.bzl", format!("{WRITE_RULE}\nordinary={{'list':[1],2:False}}\nr=rule(implementation=_impl,attrs={{'mapping':attr.string_dict()}})\n"));
    f.file(
        "BUILD.bazel",
        "load(':defs.bzl','r','ordinary')\nr(name='probe')\n",
    );
    f.build(&["//:probe"], &[("//:probe", &record(&[], &[]))]);
}

#[test]
fn string_dict_ascribed_labels_providers_and_cycles_are_not_strings() {
    for value in [
        "{Label('//:x'):'s'}",
        "{'k':Label('//:x')}",
        "{P:'s'}",
        "cycle",
    ] {
        let f = Fixture::new();
        f.file("defs.bzl", format!("{WRITE_RULE}\nP=provider(fields=[])\nr=rule(implementation=_impl,attrs={{'mapping':attr.string_dict()}})\ndef make(name):\n    cycle={{}}\n    cycle['self']=cycle\n    r(name=name,mapping={value})\n"));
        f.file(
            "BUILD.bazel",
            "load(':defs.bzl','make')\nmake(name='probe')\n",
        );
        f.reject(&["query", "//:probe"], &["mapping", "string"]);
        f.no_actions();
    }
}

#[test]
fn string_dict_constructor_and_call_snapshots_keep_callers_mutable() {
    let f = Fixture::new();
    f.file("owner.bzl", format!("{WRITE_RULE}\noriginal={{'z':'before','a':'alpha'}}\nschema=attr.string_dict(default=original)\noriginal['z']='after-constructor'\nr=rule(implementation=_impl,attrs={{'mapping':schema}})\noriginal['new']='after-rule'\ndef make(name):\n    supplied={{'z':'call','a':'alpha'}}\n    r(name=name,mapping=supplied)\n    supplied['z']='after-call'\n    supplied['new']='later'\n"));
    f.file("alias.bzl", "load(':owner.bzl', _r='r', _make='make', _schema='schema')\nr=_r\nmake=_make\nschema=_schema\n");
    f.file("BUILD.bazel", "load(':alias.bzl','r','make')\nr(name='defaults')\nmake(name='supplied')\nx={'z':'build'}\nr(name='build',mapping=x)\nx['z']='late'\n");
    let expected = [
        ("//:defaults", record(&["z", "a"], &["before", "alpha"])),
        ("//:supplied", record(&["z", "a"], &["call", "alpha"])),
        ("//:build", record(&["z"], &["build"])),
    ];
    let expected: Vec<_> = expected.iter().map(|(k, v)| (*k, v.as_slice())).collect();
    f.build(&["//:supplied", "//:build", "//:defaults"], &expected);
}

#[test]
fn string_dict_frozen_analysis_repository_and_tag_projections() {
    for argument in ["", ",mapping={'z':'explicit'}"] {
        let f = Fixture::new();
        f.file("defs.bzl", "def _impl(ctx):\n    ctx.attr.mapping['new']='bad'\n    return []\nr=rule(implementation=_impl,attrs={'mapping':attr.string_dict(default={'z':'default'})})\n");
        f.file(
            "BUILD.bazel",
            format!("load(':defs.bzl','r')\nr(name='probe'{argument})\n"),
        );
        f.reject(&["build", "//:probe"], &["frozen"]);
        f.no_actions();
    }
    let f = Fixture::new();
    f.repository(
        "attr.string_dict(default={'z':'default'})",
        "repo(name='made')",
    );
    f.file("repo.bzl", format!("{}\nrepo=repository_rule(implementation=_repo,attrs={{'mapping':attr.string_dict(default={{'z':'default'}})}})\n", REPOSITORY.replace("    print(\"STRING_DICT_REPO_CALLBACK\")", "    ctx.attr.mapping['new']='bad'")));
    f.reject(&["query", "@made//:all"], &["frozen"]);
    f.no_actions();
    let f = Fixture::new();
    f.file("ext.bzl", "def _impl(ctx):\n    for module in ctx.modules:\n        for tag in module.tags.data:\n            tag.mapping['new']='bad'\next=module_extension(implementation=_impl,tag_classes={'data':tag_class(attrs={'mapping':attr.string_dict(default={'z':'default'})})})\n");
    f.file(
        "MODULE.bazel",
        "module(name='main')\ne=use_extension('//:ext.bzl','ext')\ne.data()\nuse_repo(e,'made')\n",
    );
    f.reject(&["query", "@made//:all"], &["frozen"]);
}

#[test]
fn string_dict_unicode_order_and_literal_labels_reach_actions() {
    let f = Fixture::new();
    f.ordinary("attr.string_dict()", "r(name='probe',mapping={'z':'spaces  kept','a':'line1\\nline2','clé':'café☃','label':'@dep//foo:bar'})");
    f.build(
        &["//:probe"],
        &[(
            "//:probe",
            &record(
                &["z", "a", "clé", "label"],
                &["spaces  kept", "line1\nline2", "café☃", "@dep//foo:bar"],
            ),
        )],
    );
}

#[test]
fn string_dict_producer_consumer_cold_warm_source_change_and_regression() {
    let f = Fixture::new();
    let definitions = format!(
        "{WRITE_RULE}\nr=rule(implementation=_impl,attrs={{'mapping':attr.string_dict()}})\n"
    );
    f.file("BUILD.bazel", "load(':defs.bzl','r')\nr(name='producer',mapping={'z':'first','a':'second'})\ngenrule(name='consumer',srcs=[':producer'],outs=['consumer.txt'],cmd='/bin/cat $(SRCS) > $@')\n");
    for (source, values, cache) in [
        (definitions.clone(), vec!["first", "second"], false),
        (definitions.clone(), vec!["first", "second"], true),
        (
            definitions.replace(
                "ctx.attr.mapping.values()",
                "[v + '!' for v in ctx.attr.mapping.values()]",
            ),
            vec!["first!", "second!"],
            false,
        ),
        (definitions.clone(), vec!["first", "second"], true),
    ] {
        f.file("defs.bzl", source);
        let bytes = record(&["z", "a"], &values);
        let built = f.build(
            &["//:producer", "//:consumer"],
            &[("//:consumer", &bytes), ("//:producer", &bytes)],
        );
        if cache {
            assert!(
                built.stderr.contains("0 actions executed, 2 cache hits"),
                "{}",
                built.stderr
            );
        } else {
            assert!(
                built.stderr.contains("2 actions executed"),
                "{}",
                built.stderr
            );
        }
    }
}

#[test]
fn string_dict_native_profile_run_shell_env_is_a_real_scalar_map() {
    // NATIVE_PROFILE: real native API coverage, not a reference-executed case.
    let f = Fixture::new();
    f.file("defs.bzl", r#"
def _impl(ctx):
    out = ctx.actions.declare_file(ctx.label.name + ".txt")
    ctx.actions.run_shell(outputs=[out], command="printf '%s\\n%s\\n' \"$SYNTHETIC_DICT_A\" \"$SYNTHETIC_DICT_B\" > " + out.path, env=ctx.attr.mapping)
    return DefaultInfo(files=depset([out]))
r=rule(implementation=_impl,attrs={"mapping":attr.string_dict()})
"#);
    f.file("BUILD.bazel", "load(':defs.bzl','r')\nr(name='probe',mapping={'SYNTHETIC_DICT_A':'spaces  kept','SYNTHETIC_DICT_B':'@dep//foo:bar'})\n");
    f.build(
        &["//:probe"],
        &[("//:probe", b"spaces  kept\n@dep//foo:bar\n")],
    );
}

#[test]
fn string_dict_direct_repository_module_end_snapshot_and_default_snapshot() {
    let f = Fixture::new();
    f.repository("attr.string_dict()", "values={'z':'before','a':'alpha'}\nrepo(name='made',mapping=values)\nvalues['z']='after'\nvalues['later']='new'");
    f.build(
        &["@made//:data"],
        &[(
            "@@+repo+made//:data",
            &record(&["z", "a", "later"], &["after", "alpha", "new"]),
        )],
    );
    f.file("repo.bzl", format!("{REPOSITORY}\nvalues={{'z':'before','a':'alpha'}}\nschema=attr.string_dict(default=values)\nvalues['z']='after-constructor'\nrepo=repository_rule(implementation=_repo,attrs={{'mapping':schema}})\nvalues['later']='after-schema'\n"));
    f.file("MODULE.bazel", "module(name='main')\nrepo=use_repo_rule('//:repo.bzl','repo')\nrepo(name='made',mapping=None)\n");
    f.build(
        &["@made//:data"],
        &[(
            "@@+repo+made//:data",
            &record(&["z", "a"], &["before", "alpha"]),
        )],
    );
}

#[test]
fn string_dict_tags_capture_module_end_but_extension_repo_calls_capture_each_call() {
    let f = Fixture::new();
    f.repository("attr.string_dict()", "");
    f.file("ext.bzl", r#"
load(':repo.bzl','repo')
def _impl(ctx):
    for module in ctx.modules:
        for tag in module.tags.data:
            repo(name='tagged',mapping=tag.mapping)
    values={'z':'before','a':'alpha'}
    repo(name='first',mapping=values)
    values['z']='after'
    values['later']='new'
    repo(name='second',mapping=values)
ext=module_extension(implementation=_impl,tag_classes={'data':tag_class(attrs={'mapping':attr.string_dict()})})
"#);
    f.file("MODULE.bazel", "module(name='main')\ne=use_extension('//:ext.bzl','ext')\nvalues={'z':'tag','a':'alpha'}\ne.data(mapping=values)\nvalues['z']='tag-after'\nvalues['later']='new'\nuse_repo(e,'tagged','first','second')\n");
    f.build(
        &["@tagged//:data", "@second//:data", "@first//:data"],
        &[
            (
                "@@+ext+tagged//:data",
                &record(&["z", "a", "later"], &["tag-after", "alpha", "new"]),
            ),
            (
                "@@+ext+first//:data",
                &record(&["z", "a"], &["before", "alpha"]),
            ),
            (
                "@@+ext+second//:data",
                &record(&["z", "a", "later"], &["after", "alpha", "new"]),
            ),
        ],
    );
}

#[test]
fn string_dict_repository_template_order_identity_and_root_consumer() {
    let f = Fixture::new();
    f.file("template", "A");
    f.file("BUILD.bazel", "exports_files(['template'])\ngenrule(name='consumer',srcs=['@made//:data'],outs=['copy.txt'],cmd='/bin/cat $(SRCS) > $@')\n");
    let repository = r#"
def _repo(ctx):
    ctx.file('BUILD.bazel', "filegroup(name='data',srcs=['value'],visibility=['//visibility:public'])\n")
    ctx.template('value', ctx.attr.template, substitutions=ctx.attr.mapping)
repo=repository_rule(implementation=_repo,attrs={'mapping':attr.string_dict(), 'template':attr.label(default='//:template',allow_single_file=True)})
"#;
    f.file("repo.bzl", repository);
    let mut paths = Vec::new();
    for (mapping, expected) in [
        ("{'A':'B','B':'C'}", b"C".as_slice()),
        ("{'A':'B','B':'C'}", b"C"),
        ("{'B':'C','A':'B'}", b"B"),
        ("{'A':'B','B':'C'}", b"C"),
    ] {
        f.file("MODULE.bazel", format!("module(name='main')\nrepo=use_repo_rule('//:repo.bzl','repo')\nrepo(name='made',mapping={mapping})\n"));
        let built = f.build(
            &["//:consumer", "@made//:data"],
            &[("//:consumer", expected), ("@@+repo+made//:data", expected)],
        );
        paths.push(built.paths["@@+repo+made//:data"].clone());
    }
    assert_eq!(paths[0], paths[1]);
    assert_ne!(paths[0], paths[2]);
    assert_eq!(paths[0], paths[3]);
    f.file("template", "AA");
    f.build(
        &["//:consumer", "@made//:data"],
        &[("//:consumer", b"CC"), ("@@+repo+made//:data", b"CC")],
    );
}

#[test]
fn string_dict_repository_and_tag_binding_none_empty_mandatory_and_invalid_values() {
    for tag in [false, true] {
        for (schema, input, expected, markers) in [
            (
                "attr.string_dict(False)",
                "{}",
                Some(record(&[], &[])),
                vec![],
            ),
            (
                "attr.string_dict(default={'z':'fallback'})",
                "None",
                Some(record(&["z"], &["fallback"])),
                vec![],
            ),
            (
                "attr.string_dict(mandatory=True,default={'z':'fallback'})",
                "None",
                None,
                vec!["mandatory", "mapping"],
            ),
            (
                "attr.string_dict()",
                "{'k':['s']}",
                None,
                vec!["mapping", "string"],
            ),
            (
                "attr.string_dict()",
                "{'k':{'nested':'s'}}",
                None,
                vec!["mapping", "string"],
            ),
            // NATIVE_PROFILE: repository transport still has its ASCII guard.
            ("attr.string_dict()", "{'k':'café'}", None, vec!["ascii"]),
        ] {
            let f = Fixture::new();
            f.repository(
                if tag { "attr.string_dict()" } else { schema },
                &format!("repo(name='made',mapping={input})"),
            );
            if tag {
                f.file("ext.bzl", format!("load(':repo.bzl','repo')\ndef _impl(ctx):\n    for module in ctx.modules:\n        for tag in module.tags.data:\n            repo(name='made',mapping=tag.mapping)\next=module_extension(implementation=_impl,tag_classes={{'data':tag_class(attrs={{'mapping':{schema}}})}})\n"));
                f.file("MODULE.bazel", format!("module(name='main')\ne=use_extension('//:ext.bzl','ext')\ne.data(mapping={input})\nuse_repo(e,'made')\n"));
            }
            if let Some(bytes) = expected {
                f.build(
                    &["@made//:data"],
                    &[(
                        if tag {
                            "@@+ext+made//:data"
                        } else {
                            "@@+repo+made//:data"
                        },
                        &bytes,
                    )],
                );
            } else {
                let diagnostic = f.reject(&["build", "@made//:data"], &markers);
                assert!(!diagnostic.contains("STRING_DICT_REPO_CALLBACK"));
                f.no_actions();
            }
        }
    }
}

#[test]
fn string_dict_native_profile_configurable_and_select_boundaries() {
    // Reference repository/tag classes accept explicit bools. Native intentionally
    // retains the narrower existing descriptor profile in every schema context.
    for value in ["None", "False", "True"] {
        for schema in ["rule", "repository_rule", "tag_class"] {
            let f = Fixture::new();
            let declaration = if schema == "tag_class" {
                format!(
                    "schema=tag_class(attrs={{'mapping':attr.string_dict(configurable={value})}})"
                )
            } else {
                format!(
                    "schema={schema}(implementation=_impl,attrs={{'mapping':attr.string_dict(configurable={value})}})"
                )
            };
            f.file(
                "defs.bzl",
                format!("def _impl(ctx):\n    return []\n{declaration}\n"),
            );
            f.file("BUILD.bazel", "load(':defs.bzl','schema')\n");
            f.reject(&["query", "//:all"], &["configurable", "unsupported"]);
        }
    }
    let f = Fixture::new();
    f.ordinary(
        "attr.string_dict(default=select({'//conditions:default':{'k':'v'}}))",
        "r(name='probe')",
    );
    f.reject(&["query", "//:probe"], &["select"]);
    f.ordinary(
        "attr.string_dict()",
        "r(name='probe',mapping=select({'//conditions:default':{'k':'v'}}))",
    );
    f.reject(&["query", "//:probe"], &["select"]);
}

#[test]
fn string_dict_loaded_aliases_descriptors_and_intentional_build_constructor_divergence() {
    let f = Fixture::new();
    f.file("owner.bzl", "Ctor=attr.string_dict\nAll=attr\nDescriptor=attr.string_dict(default={'k':'v'})\ndef helper():\n    return Ctor(default={'k':'v'})\ndef check(value):\n    if type(value) != type(Descriptor):\n        fail('not genuine descriptor')\ncheck(attr.string_list_dict())\n");
    f.file("defs.bzl", format!("load(':owner.bzl','Ctor','All','Descriptor','helper')\n{WRITE_RULE}\nr=rule(implementation=_impl,attrs={{'mapping':Descriptor}})\n"));
    // INTENTIONAL_DIVERGENCE: pinned reference crashes (37) on the direct BUILD
    // constructor alias. Native constructs the genuine descriptor; no phase guard.
    f.file("BUILD.bazel", "load(':owner.bzl','Ctor','All','Descriptor','helper','check')\nload(':defs.bzl','r')\ncheck(Ctor(default={'k':'v'}))\ncheck(All.string_dict())\ncheck(helper())\nr(name='probe')\n");
    f.build(&["//:probe"], &[("//:probe", &record(&["k"], &["v"]))]);
    f.file(
        "BUILD.bazel",
        "load(':owner.bzl','Ctor')\na=Ctor(default={'k':['not-a-scalar']})\n",
    );
    f.reject(&["query", "//:all"], &["string"]);
    for file in ["BUILD.bazel", "MODULE.bazel"] {
        let f = Fixture::new();
        f.file(file, "a=attr.string_dict()\n");
        f.reject(&["query", "//:all"], &["attr"]);
    }
}

#[test]
fn string_dict_repeated_shared_default_and_input_clones_are_budgeted() {
    // NATIVE_PROFILE: shared scalar bytes are charged per owned expansion. The
    // source fixture is small; no arbitrary dictionary length limit is assumed.
    for defaults in [false, true] {
        for count in [1, 3] {
            let f = Fixture::new();
            let schema = if defaults {
                "attr.string_dict(default={'key':'x' * (3 * 1024 * 1024)})"
            } else {
                "attr.string_dict()"
            };
            f.file("repo.bzl", format!("def _repo(ctx):\n    print('STRING_DICT_REPO_CALLBACK')\n    ctx.file('BUILD.bazel', \"exports_files(['value'])\\n\")\n    ctx.file('value', str(len(ctx.attr.mapping['key'])))\nrepo=repository_rule(implementation=_repo,attrs={{'mapping':{schema}}})\n"));
            let mut module = "module(name='main')\nrepo=use_repo_rule('//:repo.bzl','repo')\nshared={'key':'x' * (3 * 1024 * 1024)}\n".to_owned();
            for index in 0..count {
                module.push_str(&format!(
                    "repo(name='made{index}'{})\n",
                    if defaults { "" } else { ",mapping=shared" }
                ));
            }
            f.file("MODULE.bazel", module);
            if count == 1 {
                f.build(
                    &["@made0//:value"],
                    &[("@@+repo+made0//:value", b"3145728")],
                );
            } else {
                let diagnostic = f.reject(
                    &["build", "@made0//:value"],
                    &["host preparation weighted byte limit"],
                );
                assert!(!diagnostic.contains("STRING_DICT_REPO_CALLBACK"));
                f.no_actions();
                let trees = f.0.path().join("cache/repositories/direct/trees");
                assert!(!trees.exists() || fs::read_dir(trees).unwrap().next().is_none());
            }
        }
    }
}
