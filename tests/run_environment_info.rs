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
                .prefix("run-environment-cli-")
                .tempdir_in("/tmp/opencode")
                .unwrap(),
        );
        ws.file("MODULE.bazel", "module(name='runenv_fixture')\n");
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
        self.run_env(args, &[])
    }

    fn run_env(&self, args: &[&str], environment: &[(&str, &str)]) -> Output {
        // Only synthetic fixture variables, never captured process environment.
        for (key, _) in environment {
            assert!(matches!(
                *key,
                "RUNENV_NATIVE_LITERAL" | "RUNENV_NATIVE_HOST_ONLY"
            ));
        }
        let output = Command::new("timeout")
            .args(["--kill-after=5s", "600s", env!("CARGO_BIN_EXE_bazel-rs")])
            .args(["--ignore_all_rc_files", "--cache-dir=private-cache"])
            .args(args)
            .envs(environment.iter().copied())
            .current_dir(self.path())
            .explicit_modules()
            .output()
            .unwrap();
        assert_ne!(output.status.code(), Some(124), "CLI timed out: {args:?}");
        assert_ne!(output.status.code(), Some(137), "CLI killed: {args:?}");
        output
    }

    fn untouched(&self) {
        for path in ["executed-marker", "bazel-rs-out"] {
            assert!(!self.path().join(path).exists(), "unexpected {path}");
        }
        // Nothing of the fixture's executed; the only tolerated cache
        // entries are the two workspace status write actions, which run
        // on every executing build since the N63 follow-up.
        if let Ok(entries) = fs::read_dir(self.path().join("private-cache/actions")) {
            for entry in entries.flatten() {
                let mut stack = vec![entry.path().join("outputs")];
                while let Some(dir) = stack.pop() {
                    for child in fs::read_dir(&dir).unwrap().flatten() {
                        if child.file_type().unwrap().is_dir() {
                            stack.push(child.path());
                        } else {
                            let name = child.file_name();
                            let name = name.to_string_lossy();
                            assert!(
                                matches!(
                                    name.as_ref(),
                                    "stable-status.txt" | "volatile-status.txt"
                                ),
                                "unexpected materialized output: {name}"
                            );
                        }
                    }
                }
            }
        }
    }

    fn first_root(&self) {
        let build = fs::read_to_string(self.path().join("BUILD.bazel")).unwrap();
        self.file(
            "BUILD.bazel",
            &format!(
                "{build}\ngenrule(name='first',outs=['first.out'],cmd=\"touch '{}'; printf first > $@\")\n",
                self.path().join("executed-marker").display()
            ),
        );
    }

    fn reject_build(&self, diagnostic: &str) -> Output {
        self.first_root();
        let result = self.run(&["build", ":first", ":check"]);
        rejected(&result, diagnostic);
        self.untouched();
        result
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

const CHECK_BUILD: &str = "load('//defs:rules.bzl','check')\ncheck(name='check')\n";
const TRANSPORT_BUILD: &str = "load('//defs:rules.bzl','produce','check')\nproduce(name='producer')\ncheck(name='check',dep=':producer')\n";

fn checking(body: &str, expression: &str) -> String {
    format!(
        "def _check(ctx):\n{body}\n    out=ctx.actions.declare_file(ctx.label.name+'.txt')\n    ctx.actions.write(out,{expression})\n    return [DefaultInfo(files=depset([out]))]\ncheck=rule(implementation=_check)\n"
    )
}

const PRODUCER: &str = r#"
P=provider(fields=['info'])
def _produce(ctx):
    return [P(info=RunEnvironmentInfo({'Z':'z','A':'a'},['Z','Z']))]
produce=rule(implementation=_produce)
"#;

#[test]
fn ordinary_nested_metadata_matches_genuine_case_22_and_caches_consumer_output() {
    let ws = Workspace::new(
        &format!(
            r#"{PRODUCER}
def _check(ctx):
    p=ctx.attr.dep[P].info
    out=ctx.actions.declare_file('check.out')
    ctx.actions.write(out,str([RunEnvironmentInfo in ctx.attr.dep,p.environment,p.inherited_environment,p==RunEnvironmentInfo({{'Z':'z','A':'a'}},['Z','Z'])]))
    return [DefaultInfo(files=depset([out]))]
check=rule(implementation=_check,attrs={{'dep':attr.label(providers=[P])}})
"#
        ),
        TRANSPORT_BUILD,
    );
    success(&ws.run(&["query", ":check"]));
    ws.untouched();
    let producer = ws.run(&["build", ":producer"]);
    assert!(outputs(&producer).is_empty());
    assert!(errors(&producer).contains("0 actions executed"));
    ws.untouched();
    let cold = ws.run(&["build", ":check"]);
    assert_eq!(
        content(&cold),
        [br#"[False, {"Z": "z", "A": "a"}, ["Z", "Z"], True]"#.to_vec()]
    );
    assert!(outputs(&cold)[0].ends_with("check.out"));
    assert!(errors(&cold).contains("1 actions executed, 0 cache hits"));
    let warm = ws.run(&["build", ":check"]);
    assert_eq!(outputs(&warm), outputs(&cold));
    assert_eq!(content(&warm), content(&cold));
    assert!(errors(&warm).contains("0 actions executed, 1 cache hits"));
}

#[test]
fn constructor_defaults_bindings_frozen_inputs_and_provider_instance_shapes() {
    let ws = Workspace::new(
        &format!(
            r#"
R=RunEnvironmentInfo
FrozenDict={{'K':'v'}}
FrozenList=['K','K']
FrozenTuple=('K','K')
def _shape():
    if type(R)!='Provider' or repr(R)!='<function RunEnvironmentInfo>' or dir(R)!=[]:
        fail('constructor shape')
    if R==DefaultInfo or {{R:'key'}}[RunEnvironmentInfo]!='key':
        fail('constructor identity')
    p=R()
    if type(p)!='RunEnvironmentInfo' or dir(p)!=['environment','inherited_environment']:
        fail('instance shape')
    if type(p.environment)!='dict' or type(p.inherited_environment)!='list':
        fail('property types')
    if repr(p)!='struct(environment = {{}}, inherited_environment = [])':
        fail(repr(p))
_shape()
{}
"#,
            checking(
                r#"    _shape()
    for p in [R(),R({}),R({},[]),R({},()),R(environment={}),R(inherited_environment=[]),R(environment={},inherited_environment=())]:
        if p.environment!={} or p.inherited_environment!=[]:
            fail('nonempty defaults')
    for p in [R(FrozenDict,FrozenList),R(FrozenDict,FrozenTuple),R(FrozenDict,inherited_environment=FrozenList),R(inherited_environment=FrozenTuple,environment=FrozenDict)]:
        if p.environment!={'K':'v'} or p.inherited_environment!=['K','K']:
            fail('binding or frozen input')"#,
                "'checked'"
            )
        ),
        CHECK_BUILD,
    );
    assert_eq!(
        content(&ws.run(&["build", ":check"])),
        [b"checked".to_vec()]
    );
}

#[test]
fn invalid_constructor_arguments_reject_at_initialization_and_ordinary_analysis() {
    for expression in [
        "RunEnvironmentInfo({},[],False)",
        "RunEnvironmentInfo(unknown=True)",
        "RunEnvironmentInfo(error_on_non_executable=False)",
        "RunEnvironmentInfo({},environment={})",
        "RunEnvironmentInfo({},[],inherited_environment=[])",
        "RunEnvironmentInfo(environment=None)",
        "RunEnvironmentInfo(inherited_environment=None)",
        "RunEnvironmentInfo(environment='K')",
        "RunEnvironmentInfo(environment=[])",
        "RunEnvironmentInfo(environment=())",
        "RunEnvironmentInfo({1:'v'})",
        "RunEnvironmentInfo({'K':1})",
        "RunEnvironmentInfo({'K':None})",
        "RunEnvironmentInfo({'K':Label('//:check')})",
        "RunEnvironmentInfo(inherited_environment='K')",
        "RunEnvironmentInfo(inherited_environment={})",
        "RunEnvironmentInfo(inherited_environment=[1])",
        "RunEnvironmentInfo(inherited_environment=('K',None))",
    ] {
        let init = Workspace::new(
            &format!("Value={expression}\n{}", checking("    pass", "'unused'")),
            CHECK_BUILD,
        );
        rejected(&init.run(&["query", ":check"]), "");
        init.untouched();
        let analysis = Workspace::new(
            &checking(&format!("    {expression}"), "'unused'"),
            CHECK_BUILD,
        );
        success(&analysis.run(&["query", ":check"]));
        analysis.reject_build("");
    }
}

#[test]
fn constructor_and_each_property_are_independent_snapshots_even_after_freezing() {
    let ws = Workspace::new(
        &format!(
            r#"
OriginalDict={{'Z':'before','A':'a'}}
OriginalList=['Z','Z']
Saved=RunEnvironmentInfo(OriginalDict,OriginalList)
SavedDict=Saved.environment
SavedList=Saved.inherited_environment
OriginalDict['Z']='after'
OriginalList.append('A')
{}
"#,
            checking(
                r#"    e=Saved.environment
    l=Saved.inherited_environment
    e['NEW']='mutable'
    l.append('NEW')
    if SavedDict!={'Z':'before','A':'a'} or SavedList!=['Z','Z']:
        fail('saved projections changed')
    d={'K':'original'}
    names=['K']
    p=RunEnvironmentInfo(d,names)
    d['K']='changed'
    names.append('later')
    first=p.environment
    first['K']='first projection'
    second=p.environment
    second['new']='second projection'
    first_names=p.inherited_environment
    first_names.append('projection')
    if p.environment!={'K':'original'} or p.inherited_environment!=['K'] or first!={'K':'first projection'}:
        fail('local snapshots alias')"#,
                "str([Saved.environment,Saved.inherited_environment,OriginalDict,OriginalList,e,l])"
            )
        ),
        CHECK_BUILD,
    );
    assert_eq!(
        content(&ws.run(&["build", ":check"])),
        [br#"[{"Z": "before", "A": "a"}, ["Z", "Z"], {"Z": "after", "A": "a"}, ["Z", "Z", "A"], {"Z": "before", "A": "a", "NEW": "mutable"}, ["Z", "Z", "NEW"]]"#.to_vec()]
    );
}

#[test]
fn structural_equality_and_hash_ignore_map_order_but_preserve_list_order_duplicates() {
    let ws = Workspace::new(
        &format!(
            "Frozen=RunEnvironmentInfo({{'Z':'z','A':'a'}},['X','Y','X'])\nKeys={{Frozen:'frozen'}}\n{}",
            checking(
                r#"    a=RunEnvironmentInfo({'A':'a','Z':'z'},['X','Y','X'])
    b=RunEnvironmentInfo({'Z':'z','A':'a'},('X','Y','X'))
    if a!=b or a!=Frozen or Keys[a]!='frozen':
        fail('map order affected equality or hash')
    keys={a:'first'}
    keys[b]='second'
    keys[Frozen]='third'
    if len(keys)!=1 or keys[a]!='third':
        fail('equal keys did not replace')
    for other in [RunEnvironmentInfo({'Z':'z','A':'a'},['Y','X','X']),RunEnvironmentInfo({'Z':'z','A':'a'},['X','Y']),RunEnvironmentInfo({'Z':'z','A':'changed'},['X','Y','X']),RunEnvironmentInfo({'Z':'z'},['X','Y','X']),struct(environment=a.environment,inherited_environment=a.inherited_environment)]:
        if a==other:
            fail('different data compared equal')
    if RunEnvironmentInfo()!=RunEnvironmentInfo({},()):
        fail('default equality')
    if list(a.environment)!=['A','Z'] or list(b.environment)!=['Z','A']:
        fail('equality canonicalized visible insertion order')"#,
                "str([a.environment,b.environment,a.inherited_environment,keys[a]])"
            )
        ),
        CHECK_BUILD,
    );
    assert_eq!(
        content(&ws.run(&["build", ":check"])),
        [br#"[{"A": "a", "Z": "z"}, {"Z": "z", "A": "a"}, ["X", "Y", "X"], "third"]"#.to_vec()]
    );
}

#[test]
fn unicode_empty_and_nul_are_metadata_bytes_not_operating_system_environment() {
    let ws = Workspace::new(
        r#"
def _check(ctx):
    p=RunEnvironmentInfo({'Z':'last','A':'first','':'','é':'☃','nul':'a\000b'},('Z','Z','','☃'))
    representation=ctx.actions.declare_file('representation.txt')
    raw=ctx.actions.declare_file('raw.txt')
    ctx.actions.write(representation,repr(p))
    ctx.actions.write(raw,p.environment['é']+'|'+p.environment['']+'|'+p.environment['nul'])
    return [DefaultInfo(files=depset([representation,raw]))]
check=rule(implementation=_check)
"#,
        CHECK_BUILD,
    );
    let result = ws.run(&["build", ":check"]);
    let paths = outputs(&result);
    assert_eq!(paths.len(), 2);
    for path in paths {
        let expected = match path.file_name().unwrap().to_str().unwrap() {
            "representation.txt" => r#"struct(environment = {"Z": "last", "A": "first", "": "", "é": "☃", "nul": "a\x00b"}, inherited_environment = ["Z", "Z", "", "☃"])"#.as_bytes(),
            "raw.txt" => "☃||a\0b".as_bytes(),
            name => panic!("unexpected output {name}"),
        };
        assert_eq!(fs::read(path).unwrap(), expected);
    }
}

#[test]
fn fields_are_immutable_and_saved_module_projections_freeze_normally() {
    for mutation in [
        "p.environment={}",
        "p.inherited_environment=[]",
        "p.extra='no'",
        "SavedDict['K']='no'",
        "SavedList.append('no')",
    ] {
        let ws = Workspace::new(
            &format!(
                "Saved=RunEnvironmentInfo({{'K':'v'}},['K'])\nSavedDict=Saved.environment\nSavedList=Saved.inherited_environment\n{}",
                checking(&format!("    p=Saved\n    {mutation}"), "'unused'")
            ),
            CHECK_BUILD,
        );
        success(&ws.run(&["query", ":check"]));
        ws.reject_build("");
    }
}

#[test]
fn public_alias_reexports_work_in_build_module_extension_and_repository_callbacks() {
    let ws = Workspace::new(
        r#"
load('//relay:alias.bzl','R')
Initialized=R({'K':'initialization'})
def _repo(ctx):
    p=R({'K':'repo'})
    if R!=RunEnvironmentInfo or Initialized.environment!={'K':'initialization'}:
        fail('module-loaded alias')
    ctx.file('BUILD.bazel', "exports_files(['data'])\n")
    ctx.file('data',repr(p)+'|'+ctx.attr.extension,executable=False)
make=repository_rule(implementation=_repo,attrs={'extension':attr.string()})
def _extension(ctx):
    p=R({'K':'extension'},('K','K'))
    if R!=RunEnvironmentInfo or p.inherited_environment!=['K','K']:
        fail('extension constructor')
    make(name='made',extension=p.environment['K'])
ext=module_extension(implementation=_extension)
def _check(ctx):
    p=R({'K':'analysis'})
    out=ctx.actions.declare_file('check.txt')
    ctx.actions.write(out,p.environment['K']+'|'+Initialized.environment['K'])
    return [DefaultInfo(files=depset([out]))]
check=rule(implementation=_check)
"#,
        &format!("{CHECK_BUILD}filegroup(name='external',srcs=['@made//:data'])\n"),
    );
    ws.file("keys/BUILD.bazel", "");
    ws.file("keys/key.bzl", "Public=RunEnvironmentInfo\n");
    ws.file("relay/BUILD.bazel", "");
    ws.file(
        "relay/alias.bzl",
        "load('//keys:key.bzl',Original='Public')\nR=Original\n",
    );
    ws.file("MODULE.bazel", "module(name='runenv_fixture')\ne=use_extension('//defs:rules.bzl','ext')\nuse_repo(e,'made')\n");
    assert_eq!(
        content(&ws.run(&["build", ":check"])),
        [b"analysis|initialization".to_vec()]
    );
    assert_eq!(
        content(&ws.run(&["build", ":external"])),
        [
            br#"struct(environment = {"K": "repo"}, inherited_environment = [])|extension"#
                .to_vec()
        ]
    );
}

#[test]
fn predeclared_global_is_not_in_build_module_or_an_unassigned_bzl_export() {
    for file in ["BUILD.bazel", "MODULE.bazel"] {
        let ws = Workspace::new("", "");
        ws.file(file, "RunEnvironmentInfo()\n");
        let target = if file == "MODULE.bazel" {
            "@missing//:all"
        } else {
            "//:all"
        };
        rejected(&ws.run(&["query", target]), "RunEnvironmentInfo");
        ws.untouched();
        if file == "MODULE.bazel" {
            // Root recording also rejects local demand, without graph resolution.
            for command in ["query", "build"] {
                rejected(&ws.run(&[command, "//:all"]), "RunEnvironmentInfo");
                ws.untouched();
            }
        }
    }
    let ws = Workspace::new(
        "Public=RunEnvironmentInfo\n",
        "load('//defs:rules.bzl','RunEnvironmentInfo')\n",
    );
    rejected(&ws.run(&["query", "//:all"]), "RunEnvironmentInfo");
    ws.untouched();
}

#[test]
fn frozen_nested_provider_survives_private_closure_and_multiple_package_hops() {
    let ws = Workspace::new(
        r#"
load('//producer:rules.bzl','P')
load('//relay:alias.bzl','R')
def _check(ctx):
    target=ctx.attr.dep
    p=target[P].info
    if R in target or DefaultInfo not in target or target[DefaultInfo].files.to_list()!=[]:
        fail('nested metadata advertised as native provider')
    env=p.environment
    env['RUNENV_NATIVE_LITERAL']='caller literal'
    inherited=p.inherited_environment
    inherited.append('CALLER_ONLY')
    if p.environment!={'RUNENV_NATIVE_LITERAL':'$literal value'} or p.inherited_environment!=['RUNENV_NATIVE_LITERAL','RUNENV_NATIVE_HOST_ONLY','RUNENV_NATIVE_LITERAL']:
        fail('frozen payload mutated')
    literal=ctx.actions.declare_file('literal.txt')
    names=ctx.actions.declare_file('names.txt')
    ctx.actions.run_shell(outputs=[literal],command='printf "%s|%s" "$RUNENV_NATIVE_LITERAL" "${RUNENV_NATIVE_HOST_ONLY-unset}" > "$1"',arguments=[literal.path],env=p.environment)
    ctx.actions.write(names,'|'.join(p.inherited_environment))
    return [DefaultInfo(files=depset([literal,names]))]
check=rule(implementation=_check,attrs={'dep':attr.label(providers=[P])})
"#,
        "load('//defs:rules.bzl','check')\ncheck(name='check',dep='//hop:forward')\n",
    );
    ws.file("keys/BUILD.bazel", "");
    ws.file("keys/key.bzl", "Public=RunEnvironmentInfo\n");
    ws.file("relay/BUILD.bazel", "");
    ws.file(
        "relay/alias.bzl",
        "load('//keys:key.bzl',Original='Public')\nR=Original\n",
    );
    ws.file("producer/BUILD.bazel", "load(':rules.bzl','produce')\nproduce(name='produce',visibility=['//visibility:public'])\n");
    ws.file("producer/rules.bzl", r#"
load('//relay:alias.bzl','R')
P=provider(fields=['info'])
def _factory():
    private=R({'RUNENV_NATIVE_LITERAL':'$literal value'},['RUNENV_NATIVE_LITERAL','RUNENV_NATIVE_HOST_ONLY','RUNENV_NATIVE_LITERAL'])
    def _produce(ctx):
        return [P(info=private)]
    return _produce
produce=rule(implementation=_factory())
"#);
    ws.file("hop/BUILD.bazel", "load(':rules.bzl','forward')\nforward(name='forward',dep='//producer:produce',visibility=['//visibility:public'])\n");
    ws.file(
        "hop/rules.bzl",
        r#"
load('//producer:rules.bzl','P')
def _forward(ctx):
    p=ctx.attr.dep[P].info
    if RunEnvironmentInfo in ctx.attr.dep:
        fail('hidden native key')
    return [P(info=p)]
forward=rule(implementation=_forward,attrs={'dep':attr.label(providers=[P])})
"#,
    );
    let producer = ws.run(&["build", "//hop:forward"]);
    assert!(outputs(&producer).is_empty());
    assert!(errors(&producer).contains("0 actions executed"));
    ws.untouched();
    let host = [
        ("RUNENV_NATIVE_LITERAL", "host must not win"),
        ("RUNENV_NATIVE_HOST_ONLY", "host must not leak"),
    ];
    let cold = ws.run_env(&["build", ":check"], &host);
    let paths = outputs(&cold);
    assert_eq!(paths.len(), 2);
    for path in &paths {
        let expected = match path.file_name().unwrap().to_str().unwrap() {
            "literal.txt" => "$literal value|unset",
            "names.txt" => "RUNENV_NATIVE_LITERAL|RUNENV_NATIVE_HOST_ONLY|RUNENV_NATIVE_LITERAL",
            name => panic!("unexpected output {name}"),
        };
        assert_eq!(fs::read(path).unwrap(), expected.as_bytes());
    }
    assert!(errors(&cold).contains("2 actions executed, 0 cache hits"));
    let warm = ws.run_env(&["build", ":check"], &host);
    assert_eq!(outputs(&warm), paths);
    assert_eq!(content(&warm), content(&cold));
    assert!(errors(&warm).contains("0 actions executed, 2 cache hits"));
}

#[test]
fn metadata_never_applies_implicitly_and_explicit_action_env_is_a_snapshot() {
    let ws = Workspace::new(
        r#"
def _check(ctx):
    original={'RUNENV_NATIVE_LITERAL':'literal before registration'}
    inherited=['RUNENV_NATIVE_LITERAL','RUNENV_NATIVE_HOST_ONLY']
    p=RunEnvironmentInfo(original,inherited)
    original['RUNENV_NATIVE_LITERAL']='input changed'
    inherited.append('LATER')
    automatic=ctx.actions.declare_file('automatic.txt')
    explicit=ctx.actions.declare_file('explicit.txt')
    command='printf "%s|%s" "${RUNENV_NATIVE_LITERAL-unset}" "${RUNENV_NATIVE_HOST_ONLY-unset}" > "$1"'
    ctx.actions.run_shell(outputs=[automatic],command=command,arguments=[automatic.path])
    env=p.environment
    ctx.actions.run_shell(outputs=[explicit],command=command,arguments=[explicit.path],env=env)
    env['RUNENV_NATIVE_LITERAL']='projection changed after registration'
    original['RUNENV_NATIVE_LITERAL']='input changed after registration'
    p.environment['RUNENV_NATIVE_LITERAL']='discarded projection'
    p.inherited_environment.append('DISCARDED')
    if p.environment!={'RUNENV_NATIVE_LITERAL':'literal before registration'} or p.inherited_environment!=['RUNENV_NATIVE_LITERAL','RUNENV_NATIVE_HOST_ONLY']:
        fail('metadata snapshot changed')
    return [DefaultInfo(files=depset([automatic,explicit]))]
check=rule(implementation=_check)
"#,
        CHECK_BUILD,
    );
    let host = [
        ("RUNENV_NATIVE_LITERAL", "synthetic host literal"),
        ("RUNENV_NATIVE_HOST_ONLY", "synthetic host only"),
    ];
    let cold = ws.run_env(&["build", ":check"], &host);
    let paths = outputs(&cold);
    assert_eq!(paths.len(), 2);
    for path in &paths {
        let expected = match path.file_name().unwrap().to_str().unwrap() {
            "automatic.txt" => "unset|unset",
            "explicit.txt" => "literal before registration|unset",
            name => panic!("unexpected output {name}"),
        };
        assert_eq!(fs::read(path).unwrap(), expected.as_bytes());
    }
    assert!(errors(&cold).contains("2 actions executed, 0 cache hits"));
    let warm = ws.run_env(&["build", ":check"], &host);
    assert_eq!(outputs(&warm), paths);
    assert_eq!(content(&warm), content(&cold));
    assert!(errors(&warm).contains("0 actions executed, 2 cache hits"));
}

#[test]
fn ordinary_direct_returns_fail_after_callback_before_any_root_action_executes() {
    for returned in ["p", "[p]", "[DefaultInfo(files=depset([out])),p]", "[p,p]"] {
        let ws = Workspace::new(
            &format!(
                r#"
def _check(ctx):
    p=RunEnvironmentInfo({{'K':'v'}})
    out=ctx.actions.declare_file('forbidden.txt')
    ctx.actions.run_shell(outputs=[out],command='printf forbidden > "$1"',arguments=[out.path])
    print('RUNENV_CALLBACK_FINISHED')
    return {returned}
check=rule(implementation=_check)
"#
            ),
            CHECK_BUILD,
        );
        success(&ws.run(&["query", ":check"]));
        ws.untouched();
        // Duplicate instances may fail duplicate validation before the ordinary
        // return guard; both must be analysis errors, never an executable feature.
        let result = ws.reject_build("RunEnvironmentInfo");
        assert!(
            errors(&result).contains("RUNENV_CALLBACK_FINISHED"),
            "{}",
            errors(&result)
        );
        if returned != "[p,p]" {
            assert!(
                errors(&result).contains("non-executable"),
                "{}",
                errors(&result)
            );
        }
    }
}

#[test]
fn target_indexing_and_requirements_use_real_keys_without_nested_native_inference() {
    for key in [
        "RunEnvironmentInfo",
        "RunEnvironmentInfo()",
        "'RunEnvironmentInfo'",
    ] {
        let ws = Workspace::new(
            &format!(
                "{PRODUCER}\ndef _check(ctx):\n    value=ctx.attr.dep[{key}]\n    fail('UNEXPECTED_LOOKUP_SUCCESS')\ncheck=rule(implementation=_check,attrs={{'dep':attr.label(providers=[P])}})\n"
            ),
            TRANSPORT_BUILD,
        );
        success(&ws.run(&["query", ":check"]));
        let diagnostic = if key == "RunEnvironmentInfo" {
            "does not provide"
        } else {
            "provider"
        };
        let result = ws.reject_build(diagnostic);
        assert!(!errors(&result).contains("UNEXPECTED_LOOKUP_SUCCESS"));
    }
    for kind in ["label", "label_list"] {
        for requirement in ["[RunEnvironmentInfo]", "[[P,RunEnvironmentInfo]]"] {
            let ws = Workspace::new(
                &format!(
                    "{PRODUCER}\ndef _check(ctx):\n    fail('CONSUMER_RAN')\ncheck=rule(implementation=_check,attrs={{'dep':attr.{kind}(providers={requirement})}})\n"
                ),
                &if kind == "label" {
                    TRANSPORT_BUILD.to_owned()
                } else {
                    TRANSPORT_BUILD.replace("dep=':producer'", "dep=[':producer']")
                },
            );
            success(&ws.run(&["query", ":check"]));
            let result = ws.reject_build("provider");
            assert!(!errors(&result).contains("CONSUMER_RAN"));
        }
        for requirement in [
            "[RunEnvironmentInfo()]",
            "['RunEnvironmentInfo']",
            "[[],[RunEnvironmentInfo()]]",
        ] {
            let ws = Workspace::new(
                &format!(
                    "def _check(ctx):\n    return []\ncheck=rule(implementation=_check,attrs={{'dep':attr.{kind}(providers={requirement})}})\n"
                ),
                CHECK_BUILD,
            );
            rejected(&ws.run(&["query", ":check"]), "provider");
            ws.untouched();
        }
    }
}

#[test]
fn other_provider_returns_default_files_and_predeclared_selection_still_work() {
    for explicit_empty in [false, true] {
        let extra = if explicit_empty {
            ",DefaultInfo(files=depset([]))"
        } else {
            ""
        };
        let ws = Workspace::new(
            &format!(
                r#"
P=provider(fields=['info'])
def _check(ctx):
    p=RunEnvironmentInfo({{'K':'nested'}})
    ctx.actions.write(ctx.outputs.out,p.environment['K'])
    return [P(info=p),platform_common.TemplateVariableInfo({{'X':'x'}}){extra}]
check=rule(implementation=_check,attrs={{'out':attr.output(mandatory=True)}})
"#
            ),
            "load('//defs:rules.bzl','check')\ncheck(name='check',out='selected.txt')\n",
        );
        let root = ws.run(&["build", ":check"]);
        if explicit_empty {
            assert!(outputs(&root).is_empty());
            assert!(errors(&root).contains("0 actions executed"));
            ws.untouched();
        } else {
            assert_eq!(content(&root), [b"nested".to_vec()]);
        }
        let selected = ws.run(&["build", ":selected.txt"]);
        assert_eq!(content(&selected), [b"nested".to_vec()]);
        assert!(outputs(&selected)[0].ends_with("selected.txt"));
    }
}
