use std::fs;
use std::path::PathBuf;
use std::process::{Command, Output};
use std::sync::atomic::{AtomicU64, Ordering};

static NEXT: AtomicU64 = AtomicU64::new(0);
struct Workspace(PathBuf);
impl Workspace {
    fn new(defs: &str, build: &str) -> Self {
        let path = std::env::temp_dir().join(format!(
            "bazel-rs-custom-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&path).unwrap();
        let ws = Self(path);
        ws.file("MODULE.bazel", "module(name='custom_fixture')\n");
        ws.file("defs.bzl", defs);
        ws.file("BUILD.bazel", build);
        ws
    }
    fn file(&self, name: &str, content: &str) {
        let path = self.0.join(name);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, content).unwrap();
    }
    fn run(&self, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_bazel-rs"))
            .arg("--ignore_all_rc_files")
            .args(args)
            .current_dir(&self.0)
            .output()
            .unwrap()
    }
}
impl Drop for Workspace {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}
fn errors(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}
fn outputs(output: &Output) -> Vec<PathBuf> {
    assert!(output.status.success(), "{}", errors(output));
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .map(|line| PathBuf::from(line.split_once('\t').unwrap().1))
        .collect()
}

const WRITE: &str = r#"
def _impl(ctx):
    out = ctx.actions.declare_file(ctx.label.name + ".txt")
    ctx.actions.write(output=out, content=ctx.attr.content)
    return [DefaultInfo(files=depset([out]))]
write = rule(implementation=_impl, attrs={"content": attr.string(mandatory=True)})
"#;

#[test]
fn custom_write_is_native_binary_safe_and_cached() {
    let ws = Workspace::new(
        WRITE,
        "load(':defs.bzl','write')\nwrite(name='greeting',content='hello\\x00世界\\n')\n",
    );
    let query = ws.run(&["query", "//:all"]);
    assert!(query.status.success(), "{}", errors(&query));
    assert_eq!(String::from_utf8_lossy(&query.stdout), "//:greeting\n");
    assert!(!ws.0.join("bazel-rs-out").exists());
    let cold = ws.run(&["build", "--jobs=4", ":greeting"]);
    let path = outputs(&cold).remove(0);
    assert_eq!(fs::read(&path).unwrap(), "hello\0世界\n".as_bytes());
    assert!(errors(&cold).contains("1 actions executed"));
    let warm = ws.run(&["build", ":greeting"]);
    assert_eq!(outputs(&warm), [path]);
    assert!(errors(&warm).contains("0 actions executed, 1 cache hits"));
    ws.file(
        "BUILD.bazel",
        "load(':defs.bzl','write')\nwrite(name='greeting',content='changed')\n",
    );
    let changed = ws.run(&["build", ":greeting"]);
    assert_eq!(fs::read(outputs(&changed).remove(0)).unwrap(), b"changed");
    assert!(errors(&changed).contains("1 actions executed"));
}

#[test]
fn query_does_not_run_rule_implementations_and_analysis_failure_is_atomic() {
    let ws = Workspace::new(
        "def _impl(ctx):\n    fail('analysis-only failure')\nbad=rule(implementation=_impl)\n",
        "load(':defs.bzl','bad')\ngenrule(name='good',outs=['good.out'],cmd='echo good > $@')\nbad(name='bad')\n",
    );
    assert!(ws.run(&["query", "//:all"]).status.success());
    let result = ws.run(&["build", ":good", ":bad"]);
    assert!(!result.status.success());
    assert!(errors(&result).contains("analysis-only failure"));
    assert!(!ws.0.join("bazel-rs-out").exists());
}

#[test]
fn native_and_custom_dependencies_keep_generated_files_and_arguments() {
    let ws = Workspace::new(
        &format!(
            "{WRITE}\n{}",
            r#"
def _join(ctx):
    out = ctx.actions.declare_file(ctx.label.name + ".out")
    files = ctx.attr.inputs[0][DefaultInfo].files.to_list() + ctx.files.inputs[1:]
    ctx.actions.run_shell(inputs=files, outputs=[out], command='cat "$1" "$2" > "$3"; printf "%s" "$TAIL" >> "$3"', arguments=[f.path for f in files] + [out.path], env={"TAIL":"$literal"})
    return [DefaultInfo(files=depset([out]))]
join = rule(implementation=_join, attrs={"inputs":attr.label_list(allow_files=True)})
"#
        ),
        "load(':defs.bzl','write','join')\nwrite(name='custom',content='C')\ngenrule(name='native',outs=['native.txt'],cmd='printf N > $@')\njoin(name='joined',inputs=[':custom',':native'])\nfilegroup(name='all_files',srcs=[':joined'])\n",
    );
    let result = ws.run(&["build", "--jobs=4", ":all_files"]);
    assert_eq!(fs::read(outputs(&result).remove(0)).unwrap(), b"CN$literal");
    assert!(errors(&result).contains("3 actions executed"));
}

#[test]
fn multiple_actions_forward_edges_and_predeclared_output_selection() {
    let ws = Workspace::new(
        r#"
def _impl(ctx):
    intermediate = ctx.actions.declare_file("intermediate.txt")
    out = ctx.outputs.out
    ctx.actions.run_shell(inputs=[intermediate],outputs=[out],command='cat "$1" > "$2"',arguments=[intermediate.path,out.path])
    ctx.actions.write(intermediate, "staged")
    return [DefaultInfo(files=depset([]))]
multi=rule(implementation=_impl,attrs={"out":attr.output(mandatory=True)})
"#,
        "load(':defs.bzl','multi')\nmulti(name='multi',out='selected.txt')\n",
    );
    let unused = ws.run(&["build", ":multi"]);
    assert!(outputs(&unused).is_empty());
    assert!(errors(&unused).contains("0 actions executed"));
    assert!(!ws.0.join("bazel-rs-out").exists());
    let selected = ws.run(&["build", "--jobs=4", ":selected.txt"]);
    assert_eq!(fs::read(outputs(&selected).remove(0)).unwrap(), b"staged");
    assert!(errors(&selected).contains("2 actions executed"));
}

#[test]
fn custom_attribute_snapshots_and_frozen_implementation_lifetimes() {
    let ws = Workspace::new(
        r#"
load(':helper.bzl', 'format_values')
def _impl(ctx):
    output=ctx.actions.declare_file(ctx.label.name)
    ctx.actions.write(output,format_values(ctx.attr.words,ctx.attr.number,ctx.attr.enabled))
    return [DefaultInfo(files=depset([output]))]
snapshot=rule(implementation=_impl,attrs={"words":attr.string_list(),"number":attr.int(default=7),"enabled":attr.bool(default=True)})
"#,
        "load(':defs.bzl','snapshot')\nwords=['first']\nsnapshot(name='snapshot',words=words)\nwords.append('later')\n",
    );
    ws.file("helper.bzl", "PREFIX='kept alive:'\ndef format_values(words, number, enabled):\n    return PREFIX + ','.join(words) + ':' + str(number) + ':' + str(enabled)\n");
    let result = ws.run(&["build", ":snapshot"]);
    assert_eq!(
        fs::read(outputs(&result).remove(0)).unwrap(),
        b"kept alive:first:7:True"
    );
}

#[test]
fn malformed_attributes_and_unproduced_files_never_execute() {
    for invocation in [
        "write(name='bad')",
        "write(name='bad',content=1)",
        "write(name='bad',content='ok',unknown=True)",
        "write('bad',content='ok')",
    ] {
        let ws = Workspace::new(WRITE, &format!("load(':defs.bzl','write')\n{invocation}\n"));
        assert!(
            !ws.run(&["query", "//:all"]).status.success(),
            "accepted {invocation}"
        );
        assert!(!ws.0.join("bazel-rs-out").exists());
    }
    for implementation in [
        "    out=ctx.actions.declare_file('missing')\n    return [DefaultInfo(files=depset([out]))]\n",
        "    out=ctx.actions.declare_file('duplicate')\n    ctx.actions.write(out,'one')\n    ctx.actions.write(out,'two')\n    return [DefaultInfo(files=depset([out]))]\n",
        "    out=ctx.actions.declare_file('../escape')\n",
    ] {
        let ws = Workspace::new(
            &format!("def _impl(ctx):\n{implementation}\nbad=rule(implementation=_impl)\n"),
            "load(':defs.bzl','bad')\ngenrule(name='good',outs=['good.out'],cmd='echo good > $@')\nbad(name='bad')\n",
        );
        let result = ws.run(&["build", ":good", ":bad"]);
        assert!(!result.status.success());
        assert!(!ws.0.join("bazel-rs-out").exists());
    }
}

#[test]
fn all_custom_label_attributes_enforce_visibility_and_file_constraints() {
    let ws = Workspace::new(
        r#"
def _impl(ctx):
    return [DefaultInfo(files=ctx.attr.dep[DefaultInfo].files)]
forward=rule(implementation=_impl,attrs={"dep":attr.label(mandatory=True,allow_files=True)})
"#,
        "load(':defs.bzl','forward')\nforward(name='bad',dep='//private:secret')\n",
    );
    ws.file(
        "private/BUILD.bazel",
        "filegroup(name='secret',srcs=['secret.txt'])\n",
    );
    ws.file("private/secret.txt", "secret");
    let result = ws.run(&["build", ":bad"]);
    assert!(!result.status.success());
    assert!(errors(&result).contains("not visible"));
    ws.file("private/BUILD.bazel", "package(default_visibility=['//visibility:public'])\nfilegroup(name='secret',srcs=['secret.txt'])\n");
    assert_eq!(
        fs::read(outputs(&ws.run(&["build", ":bad"])).remove(0)).unwrap(),
        b"secret"
    );
}

#[test]
fn declared_executable_runs_directly_with_literal_args_and_sibling_outputs() {
    let ws = Workspace::new(
        r#"
def _impl(ctx):
    tool=ctx.actions.declare_file('tools/tool.sh')
    output=ctx.actions.declare_file('result.txt',sibling=tool)
    ctx.actions.write(tool, '#!/bin/bash\nprintf "%s:%s" "$1" "$WORD" > "$2"\n',is_executable=True)
    ctx.actions.run(executable=tool,outputs=[output],arguments=['literal $value',output.path],env={'WORD':'env value'})
    return [DefaultInfo(files=depset([output]))]
direct=rule(implementation=_impl)
"#,
        "load(':defs.bzl','direct')\ndirect(name='direct')\n",
    );
    let result = ws.run(&["build", "--jobs=4", ":direct"]);
    let output = outputs(&result).remove(0);
    assert!(output.ends_with("tools/result.txt"));
    assert_eq!(fs::read(output).unwrap(), b"literal $value:env value");
    assert!(errors(&result).contains("2 actions executed"));
}

#[test]
fn custom_provider_identity_survives_separate_packages_and_carries_nondefault_files() {
    let ws = Workspace::new(
        r#"
ContentInfo=provider(fields=['file','message'])
def _producer(ctx):
    out=ctx.actions.declare_file('provided.txt')
    ctx.actions.write(out,'from provider')
    return [ContentInfo(file=out,message='retained'),DefaultInfo(files=depset([]))]
produce=rule(implementation=_producer)
def _consumer(ctx):
    if ContentInfo not in ctx.attr.dep:
        fail('provider identity lost')
    info=ctx.attr.dep[ContentInfo]
    out=ctx.actions.declare_file(ctx.label.name+'.txt')
    ctx.actions.run_shell(inputs=[info.file],outputs=[out],command='cat "$1" > "$2"; printf ":%s" "$3" >> "$2"',arguments=[info.file.path,out.path,info.message])
    return [DefaultInfo(files=depset([out]))]
consume=rule(implementation=_consumer,attrs={'dep':attr.label(mandatory=True)})
"#,
        "load(':defs.bzl','consume')\nconsume(name='consumer',dep='//other:producer')\n",
    );
    ws.file("other/BUILD.bazel", "load('//:defs.bzl','produce')\nproduce(name='producer',visibility=['//visibility:public'])\n");
    let result = ws.run(&["build", "--jobs=4", ":consumer"]);
    assert_eq!(
        fs::read(outputs(&result).remove(0)).unwrap(),
        b"from provider:retained"
    );
    assert!(errors(&result).contains("2 actions executed"));
    let unused = ws.run(&["build", "//other:producer"]);
    assert!(outputs(&unused).is_empty());
    assert!(errors(&unused).contains("0 actions executed"));
}

#[test]
fn args_builders_format_map_and_deduplicate_without_shell_interpolation() {
    let ws = Workspace::new(
        r#"
def _upper(value):
    return value.upper()
def _impl(ctx):
    out=ctx.actions.declare_file(ctx.label.name+'.txt')
    args=ctx.actions.args()
    args.add('--count',3)
    args.add_all('--items',['a','a','b'],map_each=_upper,uniquify=True,before_each='--one',format_each='x%s')
    args.add_joined('--joined',['a','b'],join_with=',',format_joined='[%s]')
    args.add('$literal')
    ctx.actions.run_shell(outputs=[out],command='out="$1"; shift; printf "%s\n" "$@" > "$out"',arguments=[out.path,args])
    return [DefaultInfo(files=depset([out]))]
arg_rule=rule(implementation=_impl)
"#,
        "load(':defs.bzl','arg_rule')\narg_rule(name='arguments')\n",
    );
    let result = ws.run(&["build", ":arguments"]);
    assert_eq!(
        fs::read(outputs(&result).remove(0)).unwrap(),
        b"--count\n3\n--items\n--one\nxA\n--one\nxB\n--joined\n[a,b]\n$literal\n"
    );
}

#[test]
fn none_uses_attribute_defaults_and_cannot_bypass_mandatory_values() {
    for kind in ["label", "output", "string", "int", "bool", "label_list"] {
        let ws = Workspace::new(
            &format!(
                "def _impl(ctx):\n    return []\nmandatory=rule(implementation=_impl,attrs={{'required':attr.{kind}(mandatory=True)}})\n"
            ),
            "load(':defs.bzl','mandatory')\nmandatory(name='bad',required=None)\n",
        );
        let result = ws.run(&["query", "//:all"]);
        assert!(!result.status.success(), "None bypassed mandatory {kind}");
        assert!(errors(&result).contains("mandatory"));
    }
    let ws = Workspace::new(
        "",
        "load('//rules:defaults.bzl','defaults')\ndefaults(name='defaults',word=None,dep=None)\n",
    );
    ws.file("rules/BUILD.bazel", "exports_files(['default.txt'])\n");
    ws.file("rules/default.txt", "definition default");
    ws.file("rules/defaults.bzl", r#"
def _impl(ctx):
    out=ctx.actions.declare_file(ctx.label.name+'.txt')
    ctx.actions.run_shell(inputs=[ctx.file.dep],outputs=[out],command='cat "$1" > "$2"; printf ":%s" "$3" >> "$2"',arguments=[ctx.file.dep.path,out.path,ctx.attr.word])
    return [DefaultInfo(files=depset([out]))]
defaults=rule(implementation=_impl,attrs={'word':attr.string(default='fallback'),'dep':attr.label(default=':default.txt',allow_single_file=True)})
"#);
    let result = ws.run(&["build", ":defaults"]);
    assert_eq!(
        fs::read(outputs(&result).remove(0)).unwrap(),
        b"definition default:fallback"
    );
}
