//! Native I0 subrules: real loading, analysis, scoped receivers and action outputs.
//! Reference case01 reads fragments and is deliberately NOT a positive I0 fixture.
//! The independent no-config-view scope fixture is the genuine positive control.
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

struct Workspace(tempfile::TempDir);

impl Workspace {
    fn new(defs: &str, build: &str) -> Self {
        let ws = Self(
            tempfile::Builder::new()
                .prefix("subrules-cli-")
                .tempdir_in("/tmp/opencode")
                .unwrap(),
        );
        ws.file("MODULE.bazel", "module(name='subrules_fixture')\n");
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
            .args(["--kill-after=5s", "60s", env!("CARGO_BIN_EXE_bazel-rs")])
            .arg("--ignore_all_rc_files")
            .arg(format!(
                "--cache-dir={}",
                self.path().join("private-cache").display()
            ))
            .args(args)
            .current_dir(self.path())
            .output()
            .unwrap();
        assert_ne!(
            output.status.code(),
            Some(124),
            "timeout: {}",
            stderr(&output)
        );
        assert_ne!(
            output.status.code(),
            Some(137),
            "killed: {}",
            stderr(&output)
        );
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

    fn analysis_rejection(&self, target: &str, diagnostic: &str, marker: &str) -> Output {
        success(&self.run(&["query", target]));
        self.untouched();
        let build = fs::read_to_string(self.path().join("BUILD.bazel")).unwrap();
        self.file("BUILD.bazel", format!(
            "{build}\ngenrule(name='first',outs=['first.out'],cmd=\"touch '{}'; printf first > $@\")\n",
            self.path().join("executed-marker").display()
        ));
        let output = self.run(&["build", ":first", target]);
        rejected(&output, diagnostic);
        assert!(
            stderr(&output).contains(marker),
            "callback not reached: {}",
            stderr(&output)
        );
        assert!(
            !stderr(&output).contains("GUARD_BYPASSED"),
            "{}",
            stderr(&output)
        );
        self.untouched();
        output
    }

    fn scope(name: &str) -> Self {
        Self::reference("scopes", name)
    }

    fn args_factory(name: &str) -> Self {
        Self::reference("args_factories", name)
    }

    fn reference(family: &str, name: &str) -> Self {
        let contract: serde_json::Value =
            serde_json::from_str(include_str!("fixtures/subrules/reference_scopes.json")).unwrap();
        assert_eq!(contract["kind"], "reference_fixture_subset");
        let case = contract[family]
            .as_array()
            .unwrap()
            .iter()
            .find(|entry| entry["name"] == name)
            .unwrap();
        let ws = Self::new("", "");
        for (name, text) in case["files"].as_object().unwrap() {
            ws.reference_file(name, text.as_str().unwrap());
        }
        ws
    }

    fn reference_file(&self, name: &str, text: &str) {
        // Reference-only platform bootstrapping is not a native rule. The
        // actual .bzl callbacks, target declarations and output bytes are kept.
        if name == "BUILD.bazel" {
            self.file(
                name,
                text.replace("platform(name=\"local_platform\")\n", ""),
            );
        } else {
            self.file(name, text);
        }
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
    assert!(
        stderr(output).contains(diagnostic),
        "expected {diagnostic:?}: {}",
        stderr(output)
    );
    for unexpected in ["panicked", "Variable `subrule` not found", "GUARD_BYPASSED"] {
        assert!(!stderr(output).contains(unexpected), "{}", stderr(output));
    }
}

fn paths(output: &Output) -> Vec<PathBuf> {
    success(output);
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .map(|line| PathBuf::from(line.split_once('\t').expect("native output path").1))
        .collect()
}

fn content(output: &Output) -> Vec<Vec<u8>> {
    paths(output)
        .iter()
        .map(|path| fs::read(path).unwrap())
        .collect()
}

fn counts(output: &Output, executed: usize, hits: usize) {
    success(output);
    assert!(
        stderr(output).contains(&format!("{executed} actions executed, {hits} cache hits")),
        "{}",
        stderr(output)
    );
}

fn scope_error(output: &Output, member: &str) {
    let text = stderr(output);
    assert!(
        text.contains("analysis scope receiver")
            || (text.contains(member)
                && (text.contains("outside of") || text.contains("implementation function"))),
        "not a receiver scope error for {member}: {text}"
    );
    assert!(
        !text.contains("has no attribute"),
        "missing API instead of scope error: {text}"
    );
}

const CHECK: &str = "load(':defs.bzl','check')\ncheck(name='check')\n";

#[test]
fn genuine_zeroattrs_no_config_views_writes_real_seed_data() {
    let ws = Workspace::scope("zeroattrs-no-config-views");
    let output = ws.run(&["build", ":zeroattrs"]);
    assert_eq!(content(&output), [b"seed-data\n".to_vec()]);
    counts(&output, 1, 0);
}

const NESTED: &str = r#"
P=provider(fields=['file'])
def factory(seed):
    captured=[seed]
    def implementation(ctx, word, suffix='!'):
        if dir(ctx)!=['actions','fragments','label','toolchains']:
            fail('subrule context shape')
        f=ctx.actions.declare_file(ctx.label.name+'.seed')
        if f.owner!=ctx.label:
            fail('subrule output not owned by calling target')
        ctx.actions.write(f,captured[0]+word+suffix+'\n')
        return P(file=f)
    return subrule(implementation=implementation,attrs={},toolchains=[],fragments=[],subrules=[])
Leaf=factory('seed-A')
def _outer(ctx,*args,**kwargs):
    return Leaf(*args,**kwargs)
Outer=subrule(implementation=_outer,subrules=(Leaf,))
def _produce(ctx):
    p=Outer('',**{'suffix':'!'})
    if p.file.owner!=ctx.label:
        fail('returned file owner')
    return [p,DefaultInfo(files=depset([p.file]))]
produce=rule(implementation=_produce,subrules=[Outer])
def _consume(ctx):
    f=ctx.attr.dep[P].file
    if f.owner!=ctx.attr.dep.label:
        fail('dependent target lost producer owner')
    out=ctx.actions.declare_file(ctx.label.name+'.out')
    ctx.actions.run_shell(inputs=[f],outputs=[out],arguments=[f.path,out.path],command='cat "$1" > "$2"')
    return [DefaultInfo(files=depset([out]))]
consume=rule(implementation=_consume,attrs={'dep':attr.label(providers=[P])})
"#;

#[test]
fn frozen_factory_declared_nesting_real_provider_consumer_cold_warm_and_change() {
    let ws = Workspace::new(
        NESTED,
        "load(':defs.bzl','produce','consume')\nproduce(name='producer')\nconsume(name='consumer',dep=':producer')\n",
    );
    let cold = ws.run(&["build", ":consumer"]);
    assert_eq!(content(&cold), [b"seed-A!\n".to_vec()]);
    counts(&cold, 2, 0);
    let warm = ws.run(&["build", ":consumer"]);
    assert_eq!(content(&warm), [b"seed-A!\n".to_vec()]);
    assert_eq!(paths(&cold), paths(&warm));
    counts(&warm, 0, 2);
    ws.file(
        "defs.bzl",
        NESTED.replace("factory('seed-A')", "factory('seed-B')"),
    );
    let changed = ws.run(&["build", ":consumer"]);
    assert_eq!(content(&changed), [b"seed-B!\n".to_vec()]);
    counts(&changed, 2, 0);
}

#[test]
fn exported_identity_aliases_and_unhashability_survive_freezing() {
    let defs = r#"
def _sub(ctx):
    return 'ok'
S=subrule(implementation=_sub)
Alias=S
Other=subrule(implementation=_sub)
def _check(ctx):
    print('RULE_ENTER')
    if type(subrule)!='builtin_function_or_method' or str(subrule)!='<built-in function subrule>' or repr(subrule)!='<built-in function subrule>':
        fail('subrule constructor metadata')
    if type(S)!='Subrule' or S!=Alias or S==Other or dir(S)!=[]:
        fail('subrule identity')
    if repr(S)!='<subrule S>':
        fail('first export name lost: '+repr(S))
    out=ctx.actions.declare_file('check.out')
    ctx.actions.write(out,Alias())
    return [DefaultInfo(files=depset([out]))]
check=rule(implementation=_check,subrules=[S,Alias,S])
"#;
    let ws = Workspace::new(defs, CHECK);
    assert_eq!(content(&ws.run(&["build", ":check"])), [b"ok".to_vec()]);
    for expression in ["{S:1}", "set([Alias])", "{subrule:1}"] {
        let ws = Workspace::new(
            &defs.replace(
                "    out=ctx.actions.declare_file('check.out')",
                &format!("    bad={expression}\n    out=ctx.actions.declare_file('check.out')"),
            ),
            CHECK,
        );
        ws.analysis_rejection(":check", "hashable", "RULE_ENTER");
    }
}

#[test]
fn constructor_is_keyword_only_and_requires_actual_user_function() {
    for (expression, diagnostic) in [
        ("subrule(_sub)", "implementation"),
        ("subrule()", "implementation"),
        ("subrule(implementation=len)", "function"),
        ("subrule(implementation=Label)", "function"),
        ("subrule(implementation=P)", "function"),
        ("subrule(implementation=17)", "function"),
        (
            "subrule(implementation=_sub,doc='unsupported')",
            "parameter",
        ),
    ] {
        let ws = Workspace::new(
            &format!("P=provider()\ndef _sub(ctx):\n    return []\nS={expression}\n"),
            "load(':defs.bzl','S')\nfilegroup(name='check')\n",
        );
        rejected(&ws.run(&["query", ":check"]), diagnostic);
        ws.untouched();
    }
}

#[test]
fn dependency_declarations_are_strict_real_subrule_sequences() {
    for constructor in ["subrule", "rule"] {
        for value in ["[_sub]", "[len]", "['S']", "[17]", "S", "{'x':S}"] {
            let ws = Workspace::new(
                &format!(
                    "def _sub(ctx):\n    return []\nS=subrule(implementation=_sub)\ncheck={constructor}(implementation=_sub,subrules={value})\n"
                ),
                "load(':defs.bzl','check')\nfilegroup(name='check')\n",
            );
            rejected(&ws.run(&["query", ":check"]), "subrules");
            ws.untouched();
        }
    }
}

#[test]
fn nonempty_configuration_constructor_inputs_are_explicitly_unsupported() {
    for option in [
        "attrs={'_dep':attr.label(default=':dep')}",
        "toolchains=['//:tool']",
        "fragments=['cpp']",
    ] {
        let ws = Workspace::new(
            &format!("def _sub(ctx):\n    return []\nS=subrule(implementation=_sub,{option})\n"),
            "load(':defs.bzl','S')\nfilegroup(name='check')\n",
        );
        rejected(&ws.run(&["query", ":check"]), "unsupported");
        ws.untouched();
    }
}

#[test]
fn calls_during_loading_and_anonymous_unexported_calls_are_rejected_in_their_phase() {
    let ws = Workspace::new(
        "def _sub(ctx):\n    fail('CALLBACK_SHOULD_NOT_RUN')\nS=subrule(implementation=_sub)\nS()\n",
        "load(':defs.bzl','S')\nfilegroup(name='check')\n",
    );
    let output = ws.run(&["query", ":check"]);
    rejected(&output, "analysis");
    assert!(!stderr(&output).contains("CALLBACK_SHOULD_NOT_RUN"));
    assert!(
        stderr(&output).contains("analysis") || stderr(&output).contains("export"),
        "{}",
        stderr(&output)
    );
    ws.untouched();
    let ws = Workspace::new(
        "def _sub(ctx):\n    fail('CALLBACK_SHOULD_NOT_RUN')\nBOX=[subrule(implementation=_sub)]\ndef _check(ctx):\n    print('RULE_ENTER')\n    BOX[0]()\n    return []\ncheck=rule(implementation=_check,subrules=[])\n",
        CHECK,
    );
    let output = ws.analysis_rejection(":check", "export", "RULE_ENTER");
    assert!(!stderr(&output).contains("CALLBACK_SHOULD_NOT_RUN"));
}

#[test]
fn constructor_is_absent_from_build_and_root_module_globals() {
    // Root MODULE validation must actually run before the local query.
    for file in ["BUILD.bazel", "MODULE.bazel"] {
        let ws = Workspace::new("", "filegroup(name='check')\n");
        ws.file(
            file,
            if file == "BUILD.bazel" {
                "S=subrule\nfilegroup(name='check')\n"
            } else {
                "module(name='subrules_fixture')\nS=subrule\n"
            },
        );
        let output = ws.run(&["query", ":check"]);
        assert_eq!(output.status.code(), Some(1), "{}", stderr(&output));
        assert!(
            stderr(&output).contains("Variable `subrule` not found"),
            "{}",
            stderr(&output)
        );
        assert!(stderr(&output).contains(file), "{}", stderr(&output));
        assert!(output.stdout.is_empty());
        ws.untouched();
    }
}

#[test]
fn module_loaded_bzl_exports_subrule_identity_to_real_repository_callback() {
    let ws = Workspace::new(
        "",
        "genrule(name='check',srcs=['@generated//:data'],outs=['check.out'],cmd='cat $(SRCS) > $@')\n",
    );
    ws.file("MODULE.bazel", "module(name='subrules_fixture')\ne=use_extension('//:ext.bzl','ext')\nuse_repo(e,'generated')\n");
    ws.file(
        "ext.bzl",
        r#"
def _sub(ctx):
    fail('subrule must not run outside analysis')
S=subrule(implementation=_sub)
def _repo(ctx):
    ctx.file('BUILD.bazel',"exports_files(['data'])\n")
    ctx.file('data',ctx.attr.text,executable=False)
maker=repository_rule(implementation=_repo,attrs={'text':attr.string()})
def _ext(ctx):
    maker(name='generated',text=repr(S)+'\n')
ext=module_extension(implementation=_ext)
"#,
    );
    assert_eq!(
        content(&ws.run(&["build", ":check"])),
        [b"<subrule S>\n".to_vec()]
    );
}

#[test]
fn immediate_caller_permission_is_not_transitive_or_extended_by_container_mutation() {
    for (outer_dependencies, rule_dependencies, mutation, call) in [
        ("[Leaf]", "[Outer]", "", "Leaf()"),
        ("[]", "[Outer,Leaf]", "", "Outer()"),
        ("[Leaf]", "[]", "DECLARED.append(Outer)", "Outer()"),
        ("[]", "[Outer]", "CHILDREN.append(Leaf)", "Outer()"),
    ] {
        let ws = Workspace::new(
            &format!(
                r#"
def _leaf(ctx):
    fail('LEAF_SHOULD_NOT_RUN')
Leaf=subrule(implementation=_leaf)
def _outer(ctx):
    print('OUTER_ENTER')
    return Leaf()
CHILDREN={outer_dependencies}
Outer=subrule(implementation=_outer,subrules=CHILDREN)
def _check(ctx):
    print('RULE_ENTER')
    {call}
    return []
DECLARED={rule_dependencies}
check=rule(implementation=_check,subrules=DECLARED)
{mutation}
"#
            ),
            CHECK,
        );
        let output = ws.analysis_rejection(":check", "declar", "RULE_ENTER");
        assert!(!stderr(&output).contains("LEAF_SHOULD_NOT_RUN"));
    }
}

#[test]
fn invocation_forwards_positionals_starargs_and_kwargs_and_rejects_binding_errors() {
    let defs = r#"
def _sub(ctx,word,suffix='!'):
    return word+suffix
S=subrule(implementation=_sub)
def _check(ctx):
    print('RULE_ENTER')
    text=S(*['word'],**{'suffix':'?'})
    out=ctx.actions.declare_file('check.out')
    ctx.actions.write(out,text)
    return [DefaultInfo(files=depset([out]))]
check=rule(implementation=_check,subrules=[S])
"#;
    let ws = Workspace::new(defs, CHECK);
    assert_eq!(content(&ws.run(&["build", ":check"])), [b"word?".to_vec()]);
    for (call, diagnostic) in [
        ("S()", "word"),
        ("S('a','b','c')", "argument"),
        ("S('a',unknown=True)", "unknown"),
        ("S('a',ctx=ctx)", "ctx"),
    ] {
        let ws = Workspace::new(&defs.replace("S(*['word'],**{'suffix':'?'})", call), CHECK);
        ws.analysis_rejection(":check", diagnostic, "RULE_ENTER");
    }
}

#[test]
fn locked_parent_dot_and_getattr_existing_default_fail_after_action_registration() {
    for target in [":locked_direct", ":locked_getattr"] {
        let ws = Workspace::scope("locked-parent-direct-and-getattr");
        let output = ws.analysis_rejection(target, "scope", "SUBRULE_ENTER");
        scope_error(&output, "label");
    }
}

#[test]
fn locked_parent_structural_introspection_and_unknown_default_do_not_read_properties() {
    let ws = Workspace::scope("locked-parent-structural-introspection");
    let output = ws.run(&["build", ":structural"]);
    let mut bytes = content(&output);
    bytes.sort();
    assert_eq!(
        bytes,
        [b"True|True|fallback\n".to_vec(), b"seed-data\n".to_vec()]
    );
    counts(&output, 2, 0);
}

#[test]
fn captured_parent_actions_and_bound_methods_cannot_mutate_in_child() {
    for (capture, operation, member) in [
        (
            "ctx.actions",
            "receiver.declare_file('forbidden')",
            "declare_file",
        ),
        ("ctx.actions", "receiver.write(out,'forbidden')", "write"),
        (
            "ctx.actions",
            "receiver.run_shell(outputs=[out],command='true')",
            "run_shell",
        ),
        (
            "ctx.actions",
            "receiver.run(outputs=[out],executable='/bin/true')",
            "run",
        ),
        ("ctx.actions.write", "receiver(out,'forbidden')", "write"),
    ] {
        let ws = Workspace::new(
            &format!(
                r#"
def _sub(ctx,receiver,out):
    seed=ctx.actions.declare_file('seed')
    ctx.actions.write(seed,'registered but never executed')
    print('SUBRULE_ENTER')
    {operation}
    fail('GUARD_BYPASSED')
S=subrule(implementation=_sub)
def _check(ctx):
    out=ctx.actions.declare_file('parent.out')
    receiver={capture}
    S(receiver,out)
    return []
check=rule(implementation=_check,subrules=[S])
"#
            ),
            CHECK,
        );
        let output = ws.analysis_rejection(":check", "scope", "SUBRULE_ENTER");
        scope_error(&output, member);
    }
}

#[test]
fn escaped_child_context_actions_and_bound_write_expire_on_return() {
    for (fixture, target, member) in [
        ("escaped-subrule-context", ":escaped_ctx", "label"),
        (
            "escaped-subrule-actions-and-bound-write",
            ":escaped_actions",
            "write",
        ),
        (
            "escaped-subrule-actions-and-bound-write",
            ":escaped_bound",
            "write",
        ),
    ] {
        let ws = Workspace::scope(fixture);
        let output = ws.analysis_rejection(target, "scope", "SUBRULE_ENTER");
        scope_error(&output, member);
    }
    let ws = Workspace::scope("escaped-subrule-context");
    let defs = fs::read_to_string(ws.path().join("defs.bzl")).unwrap();
    ws.file(
        "defs.bzl",
        defs.replace("x=escaped.label", "x=escaped.actions"),
    );
    let output = ws.analysis_rejection(":escaped_ctx", "scope", "SUBRULE_ENTER");
    scope_error(&output, "actions");
}

#[test]
fn invocation_tokens_not_subrule_identity_keep_previous_context_expired() {
    let ws = Workspace::scope("same-subrule-second-call-stale-context");
    let output = ws.analysis_rejection(":stale_same", "scope", "SUBRULE_ENTER");
    scope_error(&output, "label");
}

#[test]
fn captured_parent_object_and_bound_args_factories_are_allowed_inside_child() {
    // Genuine exception: args() permits a same-owner LIVE ANCESTOR receiver.
    for name in ["parent_object", "parent_bound"] {
        let ws = Workspace::args_factory(name);
        let output = ws.run(&["build", &format!(":{name}")]);
        assert_eq!(content(&output), [format!("{name}\n").into_bytes()]);
        assert!(stderr(&output).contains("BEFORE_FACTORY"));
        assert!(stderr(&output).contains("AFTER_FACTORY"));
        counts(&output, 1, 0);
    }
}

#[test]
fn child_created_args_escape_and_remain_mutable_for_parent_run_shell() {
    let ws = Workspace::new(
        r#"
def _sub(ctx):
    args=ctx.actions.args()
    args.add('from-subrule')
    return args
S=subrule(implementation=_sub)
def _check(ctx):
    args=S()
    args.add('from-parent')
    out=ctx.actions.declare_file('check.out')
    ctx.actions.run_shell(outputs=[out],arguments=[out.path,args],command='out="$1"; shift; printf "%s\n" "$@" > "$out"')
    return [DefaultInfo(files=depset([out]))]
check=rule(implementation=_check,subrules=[S])
"#,
        CHECK,
    );
    assert_eq!(
        content(&ws.run(&["build", ":check"])),
        [b"from-subrule\nfrom-parent\n".to_vec()]
    );
}

#[test]
fn genuine_returned_args_are_written_by_parent_with_cold_warm_and_source_change() {
    // Use the original reference callback byte-for-byte: this is write(out,
    // result.args), not the separate run_shell consumer tested above.
    let ws = Workspace::scope("subrule-args-escape-parent-add-and-consume");
    for (executed, hits) in [(2, 0), (0, 2)] {
        let output = ws.run(&["build", ":args_escape"]);
        let mut bytes = content(&output);
        bytes.sort();
        assert_eq!(
            bytes,
            [
                b"from-subrule\nfrom-parent\n".to_vec(),
                b"seed-data\n".to_vec()
            ]
        );
        counts(&output, executed, hits);
        for path in paths(&output) {
            assert_eq!(
                fs::metadata(path).unwrap().permissions().mode() & 0o777,
                0o555
            );
        }
    }
    let defs = fs::read_to_string(ws.path().join("defs.bzl")).unwrap();
    ws.file(
        "defs.bzl",
        defs.replace(
            "result.args.add(\"from-parent\")",
            "result.args.add(\"from-parent-B\")",
        ),
    );
    let changed = ws.run(&["build", ":args_escape"]);
    let mut bytes = content(&changed);
    bytes.sort();
    assert_eq!(
        bytes,
        [
            b"from-subrule\nfrom-parent-B\n".to_vec(),
            b"seed-data\n".to_vec()
        ]
    );
    counts(&changed, 1, 1);
}

// Exact reference .bzl source, mechanically compared against the reference-only
// actions-write-args-contract.json artifact (SHA256
// bcffab813e7f1f68b9ce45ebeea054b662a37fd20c016903edd056a75ea9f302).
// Embedded here so clean checkouts never need ignored research reports.
const WRITE_ARGS_REFERENCE_DEFS: &str = r#"def _mapper(value):
    print("MAP_CALLBACK", value)
    if value == "skip":
        return None
    if value == "two":
        return ["x y", "q'z"]
    return value

def _bad_mapper(value):
    fail("MAP_FAILURE_EXECUTION_MARKER")

def _impl(ctx):
    a = ctx.actions.args()
    name = ctx.label.name
    if name == "mixed":
        for value in ["plain", "space value", "single'quote", "double\"quote", "back\\slash", "", "line1\nline2", "雪é🙂"]:
            a.add(value)
    elif name == "formatted":
        a.add("v w", format="pre:%s:%%")
        a.add_all(["a", "b c", ""], format_each="<%s>", before_each="--value", terminate_with="--end")
    elif name == "mapped":
        a.add_all(["skip", "two", "z"], map_each=_mapper, format_each="[%s]")
    elif name == "mapper_failure":
        a.add_all(["x"], map_each=_bad_mapper)
    outputs = []
    out = ctx.actions.declare_file(name + ".out")
    ctx.actions.write(out, a)
    outputs.append(out)
    if name == "mixed":
        executable = ctx.actions.declare_file(name + ".exec")
        ctx.actions.write(executable, a, is_executable=True)
        outputs.append(executable)
    print("AFTER_WRITE", name)
    return [DefaultInfo(files=depset(outputs))]

r = rule(implementation=_impl)
"#;

const WRITE_ARGS_MIXED_BYTES: &str = r#"plain
'space value'
'single'\''quote'
'double"quote'
'back\slash'
''
'line1
line2'
'雪é🙂'
"#;

#[test]
fn write_args_default_shell_quoting_exact_bytes_empty_formatting_and_modes() {
    // Byte equivalence only: native map_each is eager; genuine Bazel defers it.
    // Deliberately do not run mapper_failure or assert callback timing/prints.
    for (name, expected, output_count) in [
        ("mixed", WRITE_ARGS_MIXED_BYTES, 2),
        ("empty", "", 1),
        (
            "formatted",
            "'pre:v w:%'\n--value\n'<a>'\n--value\n'<b c>'\n--value\n'<>'\n--end\n",
            1,
        ),
        ("mapped", "'[x y]'\n'[q'\\''z]'\n'[z]'\n", 1),
    ] {
        let ws = Workspace::new(
            WRITE_ARGS_REFERENCE_DEFS,
            &format!("load(':defs.bzl','r')\nr(name='{name}')\n"),
        );
        let output = ws.run(&["build", &format!(":{name}")]);
        assert_eq!(
            content(&output),
            vec![expected.as_bytes().to_vec(); output_count]
        );
        counts(&output, output_count, 0);
        for path in paths(&output) {
            // Existing readonly output normalization is 0555 for both requested
            // executable values. This does not claim False is ignored by write.
            assert_eq!(
                fs::metadata(path).unwrap().permissions().mode() & 0o777,
                0o555
            );
        }
    }

    // Additional pinned ShellEscaper source boundaries, not new genuine runs.
    for (values, expected) in [
        ("['']", "''\n"),
        (
            "['@%-_+:,./09azAZ','a~b','~lead','a=b','$HOME']",
            "@%-_+:,./09azAZ\na~b\n'~lead'\n'a=b'\n'$HOME'\n",
        ),
    ] {
        let ws = Workspace::new(
            &format!(
                r#"
def _sub(ctx):
    args=ctx.actions.args()
    args.add_all({values})
    return args
S=subrule(implementation=_sub)
def _check(ctx):
    args=S()
    out=ctx.actions.declare_file('check.out')
    ctx.actions.write(out,args)
    return [DefaultInfo(files=depset([out]))]
check=rule(implementation=_check,subrules=[S])
"#
            ),
            CHECK,
        );
        assert_eq!(
            content(&ws.run(&["build", ":check"])),
            [expected.as_bytes().to_vec()]
        );
    }
}

#[test]
fn write_args_rejects_nonargs_payloads_and_checks_expired_receivers_before_content() {
    for payload in [
        "['word']",
        "('word',)",
        "17",
        "None",
        "{'word':'value'}",
        "struct(values=['word'])",
    ] {
        let ws = Workspace::new(
            &format!(
                "def _check(ctx):\n    out=ctx.actions.declare_file('check.out')\n    print('BEFORE_BAD_CONTENT')\n    ctx.actions.write(out,{payload})\n    return [DefaultInfo(files=depset([out]))]\ncheck=rule(implementation=_check)\n"
            ),
            CHECK,
        );
        ws.analysis_rejection(":check", "Args", "BEFORE_BAD_CONTENT");
    }
    for (returned, invocation) in [
        ("ctx.actions", "escaped.write(out,[])"),
        ("ctx.actions.write", "escaped(out,[])"),
    ] {
        let ws = Workspace::new(
            &format!(
                r#"
def _sub(ctx):
    seed=ctx.actions.declare_file('seed')
    ctx.actions.write(seed,'must not execute')
    return {returned}
S=subrule(implementation=_sub)
def _check(ctx):
    out=ctx.actions.declare_file('check.out')
    escaped=S()
    print('BEFORE_EXPIRED_WRITE')
    {invocation}
    fail('GUARD_BYPASSED')
check=rule(implementation=_check,subrules=[S])
"#
            ),
            CHECK,
        );
        let output = ws.analysis_rejection(":check", "scope", "BEFORE_EXPIRED_WRITE");
        scope_error(&output, "write");
    }
    for operation in [
        "args.set_param_file_format('shell')",
        "args.set_param_file_format('multiline')",
        "args.use_param_file('@%s',use_always=True)",
    ] {
        let ws = Workspace::new(
            &format!(
                "def _check(ctx):\n    args=ctx.actions.args()\n    args.add('word')\n    out=ctx.actions.declare_file('check.out')\n    print('BEFORE_UNSUPPORTED_PARAMFILE')\n    {operation}\n    ctx.actions.write(out,args)\n    return [DefaultInfo(files=depset([out]))]\ncheck=rule(implementation=_check)\n"
            ),
            CHECK,
        );
        ws.analysis_rejection(":check", "unsupported", "BEFORE_UNSUPPORTED_PARAMFILE");
    }
}

#[test]
fn expired_child_args_factories_are_controlled_known_reference_defect_divergence() {
    // KNOWN_REFERENCE_DEFECT — INTENTIONAL_DIVERGENCE: genuine Bazel exits 37
    // with NullPointerException for BOTH fixtures, not normal negative parity.
    for name in ["child_object", "child_bound"] {
        let ws = Workspace::args_factory(name);
        let output = ws.analysis_rejection(&format!(":{name}"), "scope", "BEFORE_FACTORY");
        scope_error(&output, "args");
        assert!(!stderr(&output).contains("AFTER_FACTORY"));
    }
}

#[test]
fn configuration_property_reads_are_explicit_unsupported_not_dummy_views() {
    for expression in [
        "ctx.fragments",
        "ctx.toolchains",
        "getattr(ctx,'fragments',None)",
        "getattr(ctx,'toolchains',None)",
    ] {
        let ws = Workspace::new(
            &format!(
                "def _sub(ctx):\n    print('SUBRULE_ENTER')\n    value={expression}\n    fail('GUARD_BYPASSED')\nS=subrule(implementation=_sub)\ndef _check(ctx):\n    S()\n    return []\ncheck=rule(implementation=_check,subrules=[S])\n"
            ),
            CHECK,
        );
        ws.analysis_rejection(":check", "unsupported", "SUBRULE_ENTER");
    }
}

#[test]
fn ordinary_context_six_fields_and_setting_snapshots_survive_subrule_calls() {
    let defs = r#"
def _sub(ctx):
    return 'child'
S=subrule(implementation=_sub)
Info=provider(fields=['text'])
def _setting(ctx):
    if not hasattr(ctx,'build_setting_value'):
        fail('missing setting value')
    first=ctx.build_setting_value
    first.append('mutated')
    S()
    second=ctx.build_setting_value
    return [Info(text='|'.join(first)+';'+ '|'.join(second)+';'+'|'.join(ctx.attr.build_setting_default))]
setting=rule(implementation=_setting,build_setting=config.string_list(),subrules=[S])
def _check(ctx):
    if dir(ctx)!=['actions','attr','file','files','label','outputs','target_platform_has_constraint','toolchains'] or hasattr(ctx,'build_setting_value'):
        fail('ordinary context surface changed')
    out=ctx.actions.declare_file('check.out')
    ctx.actions.write(out,ctx.attr.dep[Info].text+'\n')
    return [DefaultInfo(files=depset([out]))]
check=rule(implementation=_check,attrs={'dep':attr.label()})
"#;
    let ws = Workspace::new(defs, "");
    for seed in ["seed", "changed"] {
        ws.file("BUILD.bazel", format!("load(':defs.bzl','setting','check')\nsetting(name='setting',build_setting_default=['{seed}'])\ncheck(name='check',dep=':setting')\n"));
        assert_eq!(
            content(&ws.run(&["build", ":check"])),
            [format!("{seed}|mutated;{seed};{seed}\n").into_bytes()]
        );
    }
}

#[test]
fn runtime_context_capabilities_cannot_be_published_in_provider_payloads() {
    for payload in ["ctx", "ctx.actions", "S()"] {
        let ws = Workspace::new(
            &format!(
                r#"
P=provider(fields=['payload'])
def _sub(ctx):
    return ctx
S=subrule(implementation=_sub)
def _check(ctx):
    print('RULE_ENTER')
    out=ctx.actions.declare_file('check.out')
    ctx.actions.write(out,'must not execute')
    return [P(payload={payload}),DefaultInfo(files=depset([out]))]
check=rule(implementation=_check,subrules=[S])
"#
            ),
            CHECK,
        );
        let output = ws.analysis_rejection(":check", "unsupported", "RULE_ENTER");
        assert!(
            stderr(&output).contains("payload") || stderr(&output).contains("context"),
            "{}",
            stderr(&output)
        );
    }
}

#[test]
fn private_frozen_factory_reexports_package_hops_and_canonical_external_bzl() {
    let ws = Workspace::new(
        "",
        "load('//hop:public.bzl','check')\ncheck(name='check')\n",
    );
    ws.file("MODULE.bazel", "module(name='subrules_fixture')\nbazel_dep(name='dep',version='1',repo_name='alias')\nlocal_path_override(module_name='dep',path='dep')\n");
    ws.file("dep/MODULE.bazel", "module(name='dep',version='1')\n");
    ws.file("dep/BUILD.bazel", "");
    ws.file(
        "dep/private.bzl",
        r#"
def _factory(seed):
    captured=[seed]
    def implementation(ctx,owner):
        out=ctx.actions.declare_file(ctx.label.name+'.out')
        if ctx.label!=owner or out.owner!=owner:
            fail('definition label incorrectly owns output')
        ctx.actions.write(out,captured[0]+'\n')
        return out
    return subrule(implementation=implementation)
S=_factory('private-frozen')
"#,
    );
    ws.file(
        "dep/public.bzl",
        "load(':private.bzl',_S='S')\nExported=_S\n",
    );
    ws.file("hop/BUILD.bazel", "");
    ws.file(
        "hop/public.bzl",
        r#"
load('@alias//:public.bzl',Apparent='Exported')
load('@@dep+//:public.bzl',Canonical='Exported')
def _check(ctx):
    if Apparent!=Canonical:
        fail('canonical identity split')
    out=Canonical(ctx.label)
    return [DefaultInfo(files=depset([out]))]
check=rule(implementation=_check,subrules=[Apparent])
"#,
    );
    assert_eq!(
        content(&ws.run(&["build", ":check"])),
        [b"private-frozen\n".to_vec()]
    );
}

#[test]
fn explicit_experimental_flag_is_unsupported_not_reference_allowlist_enforcement() {
    let ws = Workspace::new("", "filegroup(name='check')\n");
    for flag in [
        "--experimental_rule_extension_api=true",
        "--experimental_rule_extension_api=false",
    ] {
        rejected(&ws.run(&[flag, "query", ":check"]), "unsupported");
        ws.untouched();
    }
}
