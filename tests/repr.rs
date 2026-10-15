//! N22: Bazel 9.3 repr/str of builtin values observed through a real analysis
//! (docs/REPR.md). The expected lines are derived from the pinned sources cited
//! there; one probe action writes every repr so the bytes are compared exactly.
use std::fs;
use std::path::PathBuf;
use std::process::{Command, Output};

struct Workspace(tempfile::TempDir);

impl Workspace {
    fn new(files: &[(&str, &str)]) -> Self {
        let ws = Self(
            tempfile::Builder::new()
                .prefix("repr-cli-")
                .tempdir_in(std::env::temp_dir())
                .unwrap(),
        );
        for (name, text) in files {
            let path = ws.0.path().join(name);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, text).unwrap();
        }
        ws
    }

    fn run(&self, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_bazel-rs"))
            .arg("--ignore_all_rc_files")
            .arg(format!(
                "--cache-dir={}",
                self.0.path().join("private-cache").display()
            ))
            .args(args)
            .current_dir(self.0.path())
            .output()
            .unwrap()
    }
}

fn single_output(output: &Output) -> PathBuf {
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    let lines: Vec<_> = stdout.lines().collect();
    assert_eq!(lines.len(), 1, "{stdout}");
    PathBuf::from(lines[0].split_once('\t').unwrap().1)
}

const DEFS: &str = r#"
P = provider(fields = ["b", "a"])

def _gen_impl(ctx):
    out = ctx.actions.declare_file(ctx.label.name + ".out")
    ctx.actions.write(out, "gen")
    ctx.actions.write(ctx.outputs.o, "o")
    return [DefaultInfo(files = depset([out])), P(b = 1, a = Label("//:x"))]

gen = rule(implementation = _gen_impl, attrs = {"o": attr.output()})

def _sub_impl(ctx):
    return [repr(ctx), repr(ctx.actions), str(ctx)]

sub = subrule(implementation = _sub_impl)

LOAD_TIME = [
    repr(native.glob),
    repr(attr.string),
    repr(rule),
    repr(P),
    repr(DefaultInfo),
]

def _probe_impl(ctx):
    out = ctx.actions.declare_file(ctx.label.name + ".txt")
    lines = [
        repr(ctx),
        str(ctx),
        repr(ctx.actions),
        repr(ctx.actions.args()),
        repr(ctx.label),
        str(ctx.label),
        "%s" % ctx.label,
        "{}".format(ctx.label),
        "%r" % [ctx.label],
        repr(ctx.attr.rule_dep),
        str(ctx.attr.rule_dep),
        repr(ctx.attr.src_dep),
        repr(ctx.attr.out_dep),
        repr(ctx.attr.alias_dep),
        repr(ctx.attr.alias_dep.label),
        repr(ctx.file.src_dep),
        str(ctx.file.src_dep),
        repr(out),
        repr(ctx.attr.rule_dep[P]),
        repr(ctx.attr.rule_dep[DefaultInfo]),
        repr(DefaultInfo(files = depset([out]))),
        repr(struct(b = 1, a = "x")),
        repr(depset([out], order = "preorder")),
        repr(ctx.actions.write),
    ] + LOAD_TIME + sub()
    ctx.actions.write(out, "\n".join(lines) + "\n")
    return [DefaultInfo(files = depset([out]))]

probe = rule(
    implementation = _probe_impl,
    subrules = [sub],
    attrs = {
        "rule_dep": attr.label(),
        "src_dep": attr.label(allow_single_file = True),
        "out_dep": attr.label(allow_files = True),
        "alias_dep": attr.label(),
    },
)
"#;

const BUILD: &str = r#"
load(":defs.bzl", "gen", "probe")

gen(name = "g", o = "g.o")

alias(name = "al", actual = ":g")

probe(
    name = "probe",
    rule_dep = ":g",
    src_dep = "src.txt",
    out_dep = "g.o",
    alias_dep = ":al",
)
"#;

const RUNFILES: &str = "<unknown object com.google.devtools.build.lib.analysis.Runfiles>";
const FILES_TO_RUN: &str =
    "<unknown object com.google.devtools.build.lib.analysis.FilesToRunProvider>";

#[test]
fn analysis_values_print_bazel_reprs_and_strs() {
    let ws = Workspace::new(&[
        ("MODULE.bazel", "module(name='repr_fixture')\n"),
        ("BUILD.bazel", BUILD),
        ("defs.bzl", DEFS),
        ("src.txt", "src\n"),
    ]);
    let path = single_output(&ws.run(&["build", "//:probe"]));
    let expected = [
        "<rule context for //:probe>",
        "<rule context for //:probe>",
        "actions for<rule context for //:probe>",
        "context.args() object",
        "Label(\"//:probe\")",
        "@@//:probe",
        "@@//:probe",
        "@@//:probe",
        "[Label(\"//:probe\")]",
        "<target //:g>",
        "<target //:g>",
        "<input file target //:src.txt>",
        "<output file target //:g.o>",
        "<alias target //:al of //:g>",
        "Label(\"//:g\")",
        "<source file src.txt>",
        "<source file src.txt>",
        "<generated file probe.txt>",
        "struct(a = Label(\"//:x\"), b = 1)",
        &format!(
            "struct(data_runfiles = {RUNFILES}, default_runfiles = {RUNFILES}, files = depset([<generated file g.out>]), files_to_run = {FILES_TO_RUN})"
        ),
        "struct(data_runfiles = None, default_runfiles = None, files = depset([<generated file probe.txt>]), files_to_run = None)",
        "struct(a = \"x\", b = 1)",
        "depset([<generated file probe.txt>], order = \"preorder\")",
        "<built-in method write of actions value>",
        "<built-in method glob of native value>",
        "<built-in method string of attr value>",
        "<built-in function rule>",
        "<provider>",
        "<function DefaultInfo>",
        "<sub context for //:probe>",
        "actions for<sub context for //:probe>",
        "<sub context for //:probe>",
    ]
    .join("\n")
        + "\n";
    assert_eq!(fs::read_to_string(path).unwrap(), expected);
}

const DEBUG_DEFS: &str = r#"
P = provider()

def _gen_impl(ctx):
    out = ctx.actions.declare_file(ctx.label.name + ".out")
    ctx.actions.write(out, "gen")
    return [DefaultInfo(files = depset([out])), P()]

gen = rule(implementation = _gen_impl)

def _probe_impl(ctx):
    out = ctx.actions.declare_file(ctx.label.name + ".txt")
    print("PRINT", ctx.label, Label("@dep//pkg:x"), Label("@dep//pkg:pkg"), [ctx.label], ctx.attr.dep, ctx.attr.src, ctx.attr.fg, "s")
    a = ctx.actions.args()
    a.add(Label("@dep//pkg:x"))
    a.add(ctx.label, format = "L=%s")
    a.add_all([Label("//:x"), Label("@dep//pkg:y")])
    a.add_joined([ctx.label, Label("@dep//pkg:x")], join_with = ",")
    b = ctx.actions.args()
    b.add(ctx.label)
    b.add("v")
    print("ARGS", b)
    ctx.actions.write(out, a)
    return [DefaultInfo(files = depset([out]))]

probe = rule(implementation = _probe_impl, attrs = {"dep": attr.label(), "src": attr.label(allow_single_file = True), "fg": attr.label()})

def _failing_impl(ctx):
    fail("x", Label("//p:a"), Label("@dep//pkg:x"), [Label("//p:a")])

failing = rule(implementation = _failing_impl)
"#;

/// F1/F2: print() and fail() use debugPrint (MethodLibrary.java:895-947;
/// Label.java:725-737; RuleConfiguredTarget.java:288-303); Args expand Labels to
/// their main-repository display form (StarlarkCustomCommandLine.java:967-978).
#[test]
fn print_fail_and_args_use_bazel_display_forms() {
    let ws = Workspace::new(&[
        (
            "MODULE.bazel",
            "module(name='repr_fixture')\nbazel_dep(name='dep', version='1.0')\nlocal_path_override(module_name='dep', path='dep')\n",
        ),
        ("dep/MODULE.bazel", "module(name='dep', version='1.0')\n"),
        ("dep/pkg/BUILD.bazel", ""),
        (
            "BUILD.bazel",
            "load(':defs.bzl', 'failing', 'gen', 'probe')\ngen(name = 'g')\nfilegroup(name = 'fg', srcs = ['src.txt'])\nprobe(name = 'probe', dep = ':g', src = 'src.txt', fg = ':fg')\nfailing(name = 'f')\n",
        ),
        ("defs.bzl", DEBUG_DEFS),
        ("src.txt", "src\n"),
    ]);
    let output = ws.run(&["build", "//:probe"]);
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
    let path = single_output(&output);
    assert!(
        stderr.contains(
            "PRINT //:probe @dep//pkg:x @dep//pkg [Label(\"//:probe\")] <target //:g, keys:[P, OutputGroupInfo]> <input file target //:src.txt> <target //:fg, keys:[InstrumentedFilesInfo, OutputGroupInfo]> s\n"
        ),
        "{stderr}"
    );
    assert!(stderr.contains("ARGS //:probe v\n"), "{stderr}");
    assert_eq!(
        fs::read_to_string(path).unwrap(),
        // Shell-quoted like Bazel's write(Args) (capture B/g_args_write).
        "@dep//pkg:x\n'L=//:probe'\n//:x\n@dep//pkg:y\n//:probe,@dep//pkg:x\n"
    );
    let failed = ws.run(&["build", "//:f"]);
    assert!(!failed.status.success());
    let stderr = String::from_utf8_lossy(&failed.stderr);
    assert!(
        stderr.contains("x //p:a @dep//pkg:x [Label(\"//p:a\")]"),
        "{stderr}"
    );
}

const F5_DEFS: &str = r#"
def _f():
    pass

LOAD_TIME = [
    repr(struct), type(struct), repr(config.string), type(config.string), repr(attr), type(attr),
    repr(_f), repr(lambda: None), type(len), type(native.glob), type(DefaultInfo), type("x".upper),
    repr(struct(**{"😀": 3, "�": 4, "z": 2, "Z": 5, "_": 6})),
]

def _probe_impl(ctx):
    out = ctx.actions.declare_file(ctx.label.name + ".txt")
    ctx.actions.write(ctx.outputs.o, "o")
    lines = LOAD_TIME + [
        repr(ctx.outputs), type(ctx.outputs), repr(ctx.attr.o), repr(ctx.attr.src[DefaultInfo]),
        repr(ctx.attr.dep[DefaultInfo]), "{x!r}".format(**{"x!r": "kw"}),
    ]
    ctx.actions.write(out, "\n".join(lines) + "\n")
    return [DefaultInfo(files = depset([out]))]

probe = rule(implementation = _probe_impl, attrs = {"o": attr.output(), "src": attr.label(allow_single_file = True), "dep": attr.label()})

def _bang_impl(ctx):
    "{!r}".format(ctx.label)

bang = rule(implementation = _bang_impl)

def _pos_impl(ctx):
    struct(1)

pos = rule(implementation = _pos_impl)
"#;

/// F3-F5 against the genuine capture (tests/fixtures/repr/reference_receipt.json).
#[test]
fn builtin_types_functions_and_outputs_match_the_capture() {
    let ws = Workspace::new(&[
        ("MODULE.bazel", "module(name='repr_fixture')\n"),
        (
            "BUILD.bazel",
            "load(':defs.bzl', 'bang', 'pos', 'probe')\nfilegroup(name = 'fg', srcs = ['src.txt'])\nprobe(name = 'p', o = 'p.o', src = 'src.txt', dep = ':fg')\nbang(name = 'b')\npos(name = 's')\n",
        ),
        ("defs.bzl", F5_DEFS),
        ("src.txt", "src\n"),
    ]);
    let output = ws.run(&["build", "//:p"]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    let path = stdout
        .lines()
        .map(|line| PathBuf::from(line.split_once('\t').unwrap().1))
        .find(|path| path.ends_with("p.txt"))
        .unwrap();
    let expected = [
        "<function struct>",
        "Provider",
        "<built-in method string of config value>",
        "builtin_function_or_method",
        "<attr>",
        "attr",
        "<function _f from //:defs.bzl>",
        "<function lambda from //:defs.bzl>",
        "builtin_function_or_method",
        "builtin_function_or_method",
        "Provider",
        "builtin_function_or_method",
        "struct(Z = 5, _ = 6, z = 2, � = 4, 😀 = 3)",
        "ctx.outputs(o = <generated file p.o>)",
        "Outputs",
        "Label(\"//:p.o\")",
        &format!(
            "struct(data_runfiles = {RUNFILES}, default_runfiles = {RUNFILES}, files = depset([<source file src.txt>]), files_to_run = <unknown object com.google.devtools.build.lib.analysis.FilesToRunProvider$SingleExecutableFilesToRunProvider>)"
        ),
        // A one-file filegroup's file is its executable (Filegroup.java:107,117-119).
        &format!(
            "struct(data_runfiles = {RUNFILES}, default_runfiles = {RUNFILES}, files = depset([<source file src.txt>]), files_to_run = <unknown object com.google.devtools.build.lib.analysis.FilesToRunProvider$SingleExecutableFilesToRunProvider>)"
        ),
        "kw",
    ]
    .join("\n")
        + "\n";
    assert_eq!(fs::read_to_string(path).unwrap(), expected);
    // FormatParser has no conversions (FormatParser.java:110-116).
    let bang = ws.run(&["build", "//:b"]);
    assert!(!bang.status.success());
    assert!(String::from_utf8_lossy(&bang.stderr).contains("Missing argument '!r'"));
    // BuiltinFunction.java:152-154.
    let positional = ws.run(&["build", "//:s"]);
    assert!(!positional.status.success());
    assert!(
        String::from_utf8_lossy(&positional.stderr)
            .contains("struct() got unexpected positional argument")
    );
}

const OUTPUTS_DEFS: &str = r#"
O = provider(fields = ["outs"])

def _p_impl(ctx):
    ctx.actions.write(ctx.outputs.o, "o")
    inside = repr(struct(outs = ctx.outputs))
    return [DefaultInfo(files = depset([ctx.outputs.o])), O(outs = ctx.outputs)]

p = rule(implementation = _p_impl, attrs = {"o": attr.output()})

def _c_impl(ctx):
    out = ctx.actions.declare_file(ctx.label.name + ".txt")
    outs = ctx.attr.dep[O].outs
    ctx.actions.write(out, repr(outs) + "\n" + type(outs) + "\n")
    return [DefaultInfo(files = depset([out]))]

c = rule(implementation = _c_impl, attrs = {"dep": attr.label()})

def _d_impl(ctx):
    return [DefaultInfo(files = depset([ctx.attr.dep[O].outs.o]))]

d = rule(implementation = _d_impl, attrs = {"dep": attr.label()})
"#;

/// ctx.outputs may be returned inside providers. After the implementation it is
/// immutable: repr `ctx.outputs(for LABEL)` and no field access
/// (StarlarkRuleContext.java:375-377,395-397,415-421,440-447,471,586-588).
#[test]
fn returned_ctx_outputs_freeze_as_immutable_outputs() {
    let ws = Workspace::new(&[
        ("MODULE.bazel", "module(name='repr_fixture')\n"),
        (
            "BUILD.bazel",
            "load(':defs.bzl', 'c', 'd', 'p')\np(name = 'p', o = 'p.o')\nc(name = 'c', dep = ':p')\nd(name = 'd', dep = ':p')\n",
        ),
        ("defs.bzl", OUTPUTS_DEFS),
    ]);
    let path = single_output(&ws.run(&["build", "//:c"]));
    assert_eq!(
        fs::read_to_string(path).unwrap(),
        "ctx.outputs(for //:p)\nOutputs\n"
    );
    let built = ws.run(&["build", "//:p"]);
    assert!(
        built.status.success(),
        "{}",
        String::from_utf8_lossy(&built.stderr)
    );
    // Bazel: "cannot access outputs of rule '//:p' outside of its own rule
    // implementation function"; natively the immutable value has no fields.
    let failed = ws.run(&["build", "//:d"]);
    assert!(!failed.status.success());
    let stderr = String::from_utf8_lossy(&failed.stderr);
    assert!(stderr.contains("has no attribute `o`"), "{stderr}");
}
