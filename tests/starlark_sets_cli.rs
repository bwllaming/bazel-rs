//! Production integration, not another operator matrix. The three functional
//! families use the exact pinned-reference fixtures and output bytes recorded in
//! fixtures/starlark_sets/integration_reference.json, a source/output subset of
//! the reference-only integration contract. No synthetic Files/providers.
#![allow(clippy::disallowed_methods)] // test harness: starts processes directly (clippy.toml)
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
include!("support/explicit_modules.rs");

struct Workspace(tempfile::TempDir);

impl Workspace {
    fn new(defs: &str, build: &str) -> Self {
        let ws = Self(
            tempfile::Builder::new()
                .prefix("starlark-set-cli-")
                .tempdir_in(crate::test_temp::root())
                .unwrap(),
        );
        ws.file("MODULE.bazel", "module(name='sets_fixture')\n");
        ws.file("BUILD.bazel", build);
        ws.file("defs.bzl", defs);
        ws
    }

    fn path(&self) -> &Path {
        self.0.path()
    }

    fn file(&self, name: &str, text: impl AsRef<[u8]>) {
        let path = self.path().join(name);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, text).unwrap();
    }

    fn run(&self, args: &[&str]) -> Output {
        let output = Command::new("timeout")
            .args(["--kill-after=5s", "600s", env!("CARGO_BIN_EXE_bazel-rs")])
            .arg("--ignore_all_rc_files")
            .arg(format!(
                "--cache-dir={}",
                self.path().join("private-cache").display()
            ))
            .args(args)
            .current_dir(self.path())
            .explicit_modules()
            .output()
            .unwrap();
        assert_ne!(output.status.code(), Some(124), "CLI timed out: {args:?}");
        assert_ne!(output.status.code(), Some(137), "CLI killed: {args:?}");
        output
    }

    fn untouched(&self) {
        for path in [
            "executed-marker",
            "private-cache/actions",
            "private-cache/work",
            "bazel-rs-out",
        ] {
            assert!(!self.path().join(path).exists(), "unexpected {path}");
        }
    }

    fn reject_before_actions(&self, target: &str, diagnostic: &str) {
        let build = fs::read_to_string(self.path().join("BUILD.bazel")).unwrap();
        self.file("BUILD.bazel", format!(
            "{build}\ngenrule(name='first',outs=['first.out'],cmd=\"touch '{}'; printf first > $@\")\n",
            self.path().join("executed-marker").display()
        ));
        rejected(&self.run(&["build", ":first", target]), diagnostic);
        self.untouched();
    }

    fn reference(name: &str) -> (Self, serde_json::Value) {
        let contract: serde_json::Value = serde_json::from_str(include_str!(
            "fixtures/starlark_sets/integration_reference.json"
        ))
        .unwrap();
        assert_eq!(contract["status"], "REFERENCE_ONLY_PASS");
        let case = contract["cases"]
            .as_array()
            .unwrap()
            .iter()
            .find(|case| case["name"] == name)
            .unwrap()
            .clone();
        let ws = Self::new("", "");
        for (name, file) in case["fixtures"].as_object().unwrap() {
            ws.file(name, file["utf8"].as_str().unwrap());
        }
        (ws, case)
    }
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

fn success(output: &Output) {
    assert!(output.status.success(), "{}", stderr(output));
}

fn rejected(output: &Output, diagnostic: &str) {
    assert_eq!(output.status.code(), Some(1), "{}", stderr(output));
    assert!(output.stdout.is_empty(), "partial stdout on failure");
    assert!(stderr(output).contains(diagnostic), "{}", stderr(output));
    assert!(!stderr(output).contains("panicked"), "{}", stderr(output));
}

fn outputs(output: &Output) -> Vec<PathBuf> {
    success(output);
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .map(|line| PathBuf::from(line.split_once('\t').expect("native build output path").1))
        .collect()
}

fn content(output: &Output) -> Vec<Vec<u8>> {
    outputs(output)
        .iter()
        .map(|path| fs::read(path).unwrap())
        .collect()
}

fn action_counts(output: &Output, executed: usize, hits: usize) {
    success(output);
    assert!(
        stderr(output).contains(&format!("{executed} actions executed, {hits} cache hits")),
        "{}",
        stderr(output)
    );
}

fn reference_bytes(case: &serde_json::Value, phase: &str) -> Vec<Vec<u8>> {
    vec![
        case["phases"]
            .as_array()
            .unwrap()
            .iter()
            .find(|entry| entry["name"] == phase)
            .unwrap()["output"]["utf8"]
            .as_str()
            .unwrap()
            .as_bytes()
            .to_vec(),
    ]
}

const CHECK_BUILD: &str = "load(':defs.bzl','check')\ncheck(name='check')\n";

fn checking(body: &str, expression: &str) -> String {
    format!(
        "def _check(ctx):\n{body}\n    out=ctx.actions.declare_file(ctx.label.name+'.out')\n    ctx.actions.write(out,{expression})\n    return [DefaultInfo(files=depset([out]))]\ncheck=rule(implementation=_check)\n"
    )
}

#[test]
fn build_and_root_module_use_real_constructor_before_local_query() {
    let ws = Workspace::new(
        "",
        "S=set(['b','a','b'])\ngenrule(name='check',outs=['check.out'],cmd=\"printf '%s' '\"+','.join(list(S))+\"' > $@\")\n",
    );
    ws.file("MODULE.bazel", "module(name='sets_fixture')\nS=set(['m','o','m'])\nlist(S)==['m','o'] or fail('root set order')\nprint('G_SET_ROOT')\n");
    let query = ws.run(&["query", ":check"]);
    success(&query);
    assert_eq!(query.stdout, b"//:check\n");
    assert_eq!(stderr(&query).matches("G_SET_ROOT").count(), 1);
    ws.untouched();
    assert_eq!(content(&ws.run(&["build", ":check"])), [b"b,a".to_vec()]);
    ws.file("MODULE.bazel", "module(name='sets_fixture')\nS=set([[]])\n");
    rejected(&ws.run(&["query", ":check"]), "hashable");
}

#[test]
fn real_loader_constructor_aliases_are_builtin_not_provider_and_callable_by_index() {
    let ws = Workspace::new(
        &format!(
            "load(':one.bzl',R1='R')\nload(':two.bzl',R2='R')\n{}",
            checking(
                r#"    if type(R1)!='builtin_function_or_method' or repr(R1)!='<built-in function set>' or dir(R1)!=[] or hasattr(R1,'type'):
        fail('constructor metadata')
    if R1!=R2 or R1!=set or R1==DefaultInfo or type(R1())!='set':
        fail('constructor identity')
    constructors=[R1,R2]
    words=constructors[0](['b','a','b'])
    empty=constructors[1]()
    empty.add('c')"#,
                "','.join(list(words))+':'+','.join(list(empty))"
            )
        ),
        CHECK_BUILD,
    );
    ws.file("one.bzl", "R=set\n");
    ws.file("two.bzl", "R=set\n");
    assert_eq!(content(&ws.run(&["build", ":check"])), [b"b,a:c".to_vec()]);
}

#[test]
fn constructor_errors_are_real_loading_errors_not_unknown_global() {
    for (expression, diagnostic) in [
        ("{set:1}", "hashable"),
        ("set([set])", "hashable"),
        ("set([[]])", "hashable"),
        ("set([{}])", "hashable"),
        ("set([set()])", "hashable"),
        ("set([],[])", "argument"),
        ("set(elements=[])", "parameter"),
        ("set.type", "type"),
    ] {
        let ws = Workspace::new(
            &format!("Value={expression}\n{}", checking("    pass", "'unused'")),
            CHECK_BUILD,
        );
        let result = ws.run(&["query", ":check"]);
        rejected(&result, diagnostic);
        assert!(!stderr(&result).contains("Variable `set` not found"));
        ws.untouched();
    }
}

#[test]
fn constructor_cannot_be_an_attribute_provider_requirement() {
    let ws = Workspace::new(
        "def _check(ctx):\n    return []\ncheck=rule(implementation=_check,attrs={'dep':attr.label(providers=[set])})\n",
        CHECK_BUILD,
    );
    rejected(&ws.run(&["query", ":check"]), "Provider constructors");
    ws.untouched();
}

#[test]
fn target_membership_and_index_require_real_provider_keys() {
    // Membership and indexing run separately so one error cannot mask the
    // other operation. BOX contains a genuine but unexported constructor.
    let build =
        "load(':defs.bzl','check')\nfilegroup(name='dep')\ncheck(name='check',dep=':dep')\n";
    for key in ["set", "len", "'not_a_provider'", "BOX[0]"] {
        for (operation, diagnostic) in [
            (
                format!("{key} in ctx.attr.dep"),
                "Target membership requires an exported provider constructor",
            ),
            (
                format!("ctx.attr.dep[{key}]"),
                "Target indexing requires an exported provider constructor",
            ),
        ] {
            let ws = Workspace::new(
                &format!(
                    "def factory():\n    return provider()\nBOX=[factory()]\ndef _check(ctx):\n    value={operation}\n    fail('invalid provider key accepted')\ncheck=rule(implementation=_check,attrs={{'dep':attr.label()}})\n"
                ),
                build,
            );
            ws.reject_before_actions(":check", diagnostic);
        }
    }
    let ws = Workspace::new(
        "P=provider()\ndef _check(ctx):\n    if P in ctx.attr.dep or DefaultInfo not in ctx.attr.dep:\n        fail('valid provider membership changed')\n    if ctx.attr.dep[DefaultInfo].files.to_list()!=[]:\n        fail('empty filegroup default files changed')\n    out=ctx.actions.declare_file('check.out')\n    ctx.actions.write(out,'valid keys')\n    return [DefaultInfo(files=depset([out]))]\ncheck=rule(implementation=_check,attrs={'dep':attr.label()})\n",
        build,
    );
    assert_eq!(
        content(&ws.run(&["build", ":check"])),
        [b"valid keys".to_vec()]
    );
}

#[test]
fn no_native_placeholder_or_silently_ignored_disabled_set_flag() {
    let ws = Workspace::new(
        &checking(
            "    if hasattr(native,'set') or hasattr(native,'legacy_globals'):\n        fail('native set shim')",
            "'no shim'",
        ),
        CHECK_BUILD,
    );
    assert_eq!(
        content(&ws.run(&["build", ":check"])),
        [b"no shim".to_vec()]
    );
    let flag = Workspace::new("", "filegroup(name='check')\n");
    rejected(
        &flag.run(&[
            "--incompatible_disable_starlark_set=true",
            "query",
            ":check",
        ]),
        "Unknown startup option: '--incompatible_disable_starlark_set=true'.",
    );
    flag.untouched();
}

#[test]
fn frozen_factory_and_all_augmented_aliases_write_reference_bytes() {
    let (ws, case) = Workspace::reference("cross-module-frozen-copy");
    let cold = ws.run(&["build", ":consume"]);
    assert_eq!(content(&cold), reference_bytes(&case, "cold"));
    action_counts(&cold, 1, 0);
    let warm = ws.run(&["build", ":consume"]);
    assert_eq!(content(&warm), reference_bytes(&case, "warm"));
    action_counts(&warm, 0, 1);
    ws.file(
        "base.bzl",
        case["source_change"]["after"]["utf8"].as_str().unwrap(),
    );
    let changed = ws.run(&["build", ":consume"]);
    assert_eq!(content(&changed), reference_bytes(&case, "source-change"));
    action_counts(&changed, 1, 0);
}

#[test]
fn loaded_base_and_provider_sets_reject_mutation_before_any_actions() {
    for family in ["cross-module-frozen-copy", "provider-generated-file-set"] {
        let (ws, _) = Workspace::reference(family);
        success(&ws.run(&["query", ":bad"]));
        ws.reject_before_actions(":bad", "Immutable");
    }
}

#[test]
fn only_custom_provider_set_retains_generated_source_closure_and_cache_edges() {
    let (ws, case) = Workspace::reference("provider-generated-file-set");
    // This is a projection of the actual Target's synthesized DefaultInfo, not
    // an explicit empty DefaultInfo added to conceal an incorrect projection.
    // Preserve the existing native assertion insertion; the reference fixture
    // remains verbatim, and its provenance is not a native pass receipt.
    let defs = fs::read_to_string(ws.path().join("defs.bzl")).unwrap();
    ws.file("defs.bzl", defs.replace("    files = list(ctx.attr.dep[P].files)", "    if ctx.attr.dep[DefaultInfo].files.to_list() != []:\n        fail('implicit DefaultInfo files must be empty')\n    files = list(ctx.attr.dep[P].files)"));
    success(&ws.run(&["query", ":consume"]));
    ws.untouched();
    let cold = ws.run(&["build", ":consume"]);
    assert_eq!(content(&cold), reference_bytes(&case, "cold"));
    action_counts(&cold, 2, 0);
    let warm = ws.run(&["build", ":consume"]);
    assert_eq!(content(&warm), reference_bytes(&case, "warm"));
    assert_eq!(outputs(&cold), outputs(&warm));
    action_counts(&warm, 0, 2);
    ws.file("b.txt", "B2\n");
    let changed = ws.run(&["build", ":consume"]);
    assert_eq!(content(&changed), reference_bytes(&case, "source-change"));
    action_counts(&changed, 2, 0);
    ws.file("a.txt", "A2\n");
    let source_only = ws.run(&["build", ":consume"]);
    assert_eq!(content(&source_only), [b"B2\nA2\n".to_vec()]);
    action_counts(&source_only, 1, 1);
    // A root with only a nondefault provider has no requested default outputs.
    let producer = ws.run(&["build", ":producer"]);
    assert!(outputs(&producer).is_empty());
    assert!(stderr(&producer).contains("0 actions executed"));
}

#[test]
fn shared_nested_tuple_set_payload_retains_real_files_once_in_first_encounter_order() {
    let ws = Workspace::new(
        r#"
P=provider(fields=['payload'])
def _produce(ctx):
    b=ctx.actions.declare_file('b.generated')
    ctx.actions.run_shell(inputs=[ctx.file.b],outputs=[b],arguments=[ctx.file.b.path,b.path],command='cat "$1" > "$2"')
    pair=(b,ctx.file.a)
    children=set([pair,(ctx.file.a,b),pair])
    shared=(children,)
    return [P(payload=(shared,shared))]
produce=rule(implementation=_produce,attrs={'a':attr.label(allow_single_file=True),'b':attr.label(allow_single_file=True)})
def _check(ctx):
    payload=ctx.attr.dep[P].payload
    ordered=set()
    for shared in payload:
        for children in shared:
            for pair in children:
                ordered.update(pair)
    files=list(ordered)
    out=ctx.actions.declare_file('check.out')
    ctx.actions.run_shell(inputs=files,outputs=[out],arguments=[out.path]+[f.path for f in files],command='out="$1"; shift; cat "$@" > "$out"')
    return [DefaultInfo(files=depset([out]))]
check=rule(implementation=_check,attrs={'dep':attr.label(providers=[P])})
"#,
        "load(':defs.bzl','produce','check')\nproduce(name='producer',a='a.txt',b='b.txt')\ncheck(name='check',dep=':producer')\n",
    );
    ws.file("a.txt", "A\n");
    ws.file("b.txt", "B\n");
    let cold = ws.run(&["build", ":check"]);
    assert_eq!(content(&cold), [b"B\nA\n".to_vec()]);
    action_counts(&cold, 2, 0);
    let warm = ws.run(&["build", ":check"]);
    assert_eq!(content(&warm), [b"B\nA\n".to_vec()]);
    action_counts(&warm, 0, 2);
}

#[test]
fn private_closure_reexport_package_alias_and_external_file_paths_remain_real() {
    let ws = Workspace::new(
        "",
        "load('@alias//:public.bzl','consume')\nconsume(name='check',dep='//hop:alias')\n",
    );
    ws.file("MODULE.bazel", "module(name='sets_fixture')\nbazel_dep(name='dep',version='1',repo_name='alias')\nlocal_path_override(module_name='dep',path='dep')\n");
    ws.file("dep/MODULE.bazel", "module(name='dep',version='1')\n");
    ws.file("dep/BUILD.bazel", "load(':public.bzl','produce')\nproduce(name='producer',visibility=['//visibility:public'])\n");
    ws.file(
        "dep/p/BUILD.bazel",
        "exports_files(['data.txt'],visibility=['//visibility:public'])\n",
    );
    ws.file("dep/p/data.txt", "external\n");
    ws.file("dep/private.bzl", r#"
P=provider(fields=['files'])
BASE=set(['kept','kept'])
def _payload(file):
    if list(BASE)!=['kept']:
        fail('private frozen state lost')
    return P(files=set([file,file]))
def _produce(ctx):
    return [_payload(ctx.file.src)]
produce=rule(implementation=_produce,attrs={'src':attr.label(default='//p:data.txt',allow_single_file=True)})
def _consume(ctx):
    files=list(ctx.attr.dep[P].files)
    f=files[0]
    out=ctx.actions.declare_file('check.out')
    meta=ctx.actions.declare_file('metadata.txt')
    ctx.actions.run_shell(inputs=files,outputs=[out],arguments=[f.path,out.path],command='cat "$1" > "$2"')
    ctx.actions.write(meta,f.path+'|'+f.short_path+'|'+str(f.owner))
    return [DefaultInfo(files=depset([out,meta]))]
consume=rule(implementation=_consume,attrs={'dep':attr.label(providers=[P])})
def _forward(ctx):
    return [ctx.attr.dep[P]]
forward=rule(implementation=_forward,attrs={'dep':attr.label(providers=[P])})
"#);
    ws.file("dep/public.bzl", "load(':private.bzl',_produce='produce',_consume='consume',_forward='forward')\nproduce=_produce\nconsume=_consume\nforward=_forward\n");
    // No native alias rule in this profile: actual provider forwarding with
    // load and repository aliases preserves the cross-package frozen payload.
    ws.file(
        "hop/BUILD.bazel",
        "load('@alias//:public.bzl',alias='forward')\nalias(name='alias',dep='@alias//:producer',visibility=['//visibility:public'])\n",
    );
    let cold = ws.run(&["build", ":check"]);
    assert_eq!(
        content(&cold),
        [
            b"external\n".to_vec(),
            b"external/dep+/p/data.txt|../dep+/p/data.txt|@@dep+//p:data.txt".to_vec()
        ]
    );
    action_counts(&cold, 2, 0);
    ws.file("dep/p/data.txt", "external changed\n");
    let changed = ws.run(&["build", ":check"]);
    assert_eq!(content(&changed)[0], b"external changed\n");
    action_counts(&changed, 1, 1);
}

#[test]
fn module_extension_transitive_frozen_set_publishes_real_repository_and_tracks_tags() {
    let (ws, case) = Workspace::reference("module-extension-set-repository");
    let query = ws.run(&["query", "@generated//:data"]);
    success(&query);
    assert_eq!(query.stdout, b"@generated//:data\n");
    ws.untouched();
    let cold = ws.run(&["build", ":consume"]);
    assert_eq!(content(&cold), reference_bytes(&case, "cold"));
    action_counts(&cold, 1, 0);
    let warm = ws.run(&["build", ":consume"]);
    assert_eq!(content(&warm), reference_bytes(&case, "warm"));
    action_counts(&warm, 0, 1);
    ws.file(
        "MODULE.bazel",
        case["source_change"]["after"]["utf8"].as_str().unwrap(),
    );
    let changed = ws.run(&["build", ":consume"]);
    assert_eq!(content(&changed), reference_bytes(&case, "source-change"));
    action_counts(&changed, 1, 0);
    let generated = ws.run(&["build", "@generated//:data"]);
    assert_eq!(content(&generated), reference_bytes(&case, "source-change"));
    let data = outputs(&generated).remove(0);
    assert_eq!(
        fs::read(data.parent().unwrap().join("BUILD.bazel")).unwrap(),
        b"exports_files([\"data\"], visibility = [\"//visibility:public\"])\n"
    );
}

#[test]
fn strict_sequence_boundaries_reject_direct_sets_before_actions() {
    // NATIVE_BOUNDARY: these APIs deliberately remain list/tuple/depset APIs.
    // This is not a claim that every Bazel Java Sequence surface accepts sets.
    for (operation, diagnostic) in [
        ("ctx.actions.args().add_all(set(['a']))", "list or tuple"),
        (
            "depset(direct=set(['a']))",
            "in call to depset(), parameter 'direct' got value of type 'set', want 'sequence or NoneType'",
        ),
        (
            "ctx.actions.run_shell(inputs=set(),outputs=[out],command='true')",
            "list or tuple",
        ),
    ] {
        let ws = Workspace::new(
            &format!(
                "def _check(ctx):\n    out=ctx.actions.declare_file('check.out')\n    {operation}\n    return [DefaultInfo(files=depset([out]))]\ncheck=rule(implementation=_check)\n"
            ),
            CHECK_BUILD,
        );
        ws.reject_before_actions(":check", diagnostic);
    }
    let ws = Workspace::new(
        "def _check(ctx):\n    return []\ncheck=rule(implementation=_check,attrs={'words':attr.string_list(default=set(['a']))})\n",
        CHECK_BUILD,
    );
    rejected(&ws.run(&["query", ":check"]), "list");
    ws.untouched();
}

#[test]
fn explicit_list_bridges_args_depset_actions_and_attribute_defaults() {
    let ws = Workspace::new(
        r#"
BASE=set(['b','a','b'])
def _check(ctx):
    out=ctx.actions.declare_file('check.out')
    args=ctx.actions.args()
    args.add_all(list(set(ctx.attr.words)))
    ctx.actions.run_shell(inputs=list(set()),outputs=[out],arguments=[out.path,args],command='out="$1"; shift; printf "%s\n" "$@" > "$out"')
    return [DefaultInfo(files=depset(direct=list(set([out,out]))))]
check=rule(implementation=_check,attrs={'words':attr.string_list(default=list(BASE))})
"#,
        CHECK_BUILD,
    );
    assert_eq!(content(&ws.run(&["build", ":check"])), [b"b\na\n".to_vec()]);
}

#[test]
fn ascii_elems_method_math_and_raw_unicode_elements_are_action_data() {
    let ws = Workspace::new(
        &checking(
            r#"    ascii=set('baba'.elems()).union('cad'.elems())
    raw=set(['é','☃','é'])"#,
            "','.join(list(ascii))+'|'+','.join(list(raw))",
        ),
        CHECK_BUILD,
    );
    assert_eq!(
        content(&ws.run(&["build", ":check"])),
        ["b,a,c,d|é,☃".as_bytes().to_vec()]
    );
}

#[test]
fn actual_package_group_provider_is_forwarded_inside_a_set() {
    // An actual native package_group value, never a stub or constructor-name
    // lookalike: hashable, and file-free payload of another provider since
    // N25d5 (docs/CC_TOOLCHAIN.md).
    let ws = Workspace::new(
        r#"
Box=provider(fields=['payload'])
def _check(ctx):
    p=ctx.attr.dep[PackageSpecificationInfo]
    return [Box(payload=set([p]))]
check=rule(implementation=_check,attrs={'dep':attr.label(providers=[PackageSpecificationInfo])})
"#,
        "load(':defs.bzl','check')\npackage_group(name='group',packages=['//...'])\ncheck(name='check',dep=':group')\n",
    );
    let output = ws.run(&["build", ":check"]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}
