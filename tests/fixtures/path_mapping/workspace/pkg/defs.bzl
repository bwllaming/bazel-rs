FlavorInfo = provider(fields = ["value"])

def _flavor_impl(ctx):
    return [FlavorInfo(value = ctx.build_setting_value)]

flavor = rule(_flavor_impl, build_setting = config.string(flag = True))

def _set_flavor_impl(settings, attr):
    return {"//pkg:flavor": attr.flavor}

_set_flavor = transition(
    implementation = _set_flavor_impl,
    inputs = [],
    outputs = ["//pkg:flavor"],
)

def _gen_impl(ctx):
    out = ctx.actions.declare_file(ctx.label.name + ".txt")
    content = "gen\n"
    if ctx.attr.flavored:
        content = "gen " + ctx.attr._flavor[FlavorInfo].value + "\n"
    ctx.actions.write(out, content)
    return [DefaultInfo(files = depset([out]))]

gen = rule(_gen_impl, attrs = {
    "flavored": attr.bool(),
    "_flavor": attr.label(default = "//pkg:flavor"),
})

def _tree_impl(ctx):
    out = ctx.actions.declare_directory(ctx.label.name + ".d")
    ctx.actions.run_shell(
        outputs = [out],
        command = "mkdir -p $1/sub && echo a > $1/a.txt && echo b > $1/sub/b.txt",
        arguments = [out.path],
    )
    return [DefaultInfo(files = depset([out]))]

tree = rule(_tree_impl)

def _tool_impl(ctx):
    out = ctx.actions.declare_file(ctx.label.name + ".sh")
    ctx.actions.write(out, """#!/bin/bash
o=$1; d=$2; shift 2
echo "tool $0" > $o
echo "argv $@" >> $o
cat "$0.runfiles/_main/pkg/data.txt" >> $o
for f in "$@"; do
  case "$f" in @*) echo "params:" >> $o; cat "${f#@}" >> $o;; esac
  if [ -f "$f" ]; then echo "file $f:" >> $o; cat "$f" >> $o; fi
  if [ -d "$f" ]; then echo "dir $f:" >> $o; (cd $f && find . | sort) >> $o; fi
done
echo "OUTPATH=$OUTPATH" >> $o
find bazel-out -maxdepth 3 | sort >> $o
mkdir -p $d/x && echo inner > $d/x/y.txt
""", is_executable = True)
    return [DefaultInfo(executable = out, runfiles = ctx.runfiles(files = ctx.files.data))]

tool = rule(_tool_impl, executable = True, attrs = {"data": attr.label_list(allow_files = True)})

def _dirname(f):
    return f.dirname

def _root(f):
    return f.root.path + "|" + f.path + "|" + f.short_path

def _use_impl(ctx):
    out = ctx.actions.declare_file(ctx.label.name + ".out")
    outdir = ctx.actions.declare_directory(ctx.label.name + ".outdir")
    args = ctx.actions.args()
    args.add(out)
    args.add_all([outdir], expand_directories = False)
    args.add_all(ctx.files.srcs)
    args.add(ctx.files.srcs[0], format = "-I%s")
    args.add_all(ctx.files.srcs, map_each = _dirname, format_each = "--dir=%s")
    args.add_all(ctx.files.srcs, map_each = _root)
    if ctx.attr.literal:
        args.add("literal=" + ctx.files.srcs[0].path)
    args.add_all(ctx.files.trees)
    args.add_all(ctx.files.trees, map_each = _root, format_each = "--tree=%s")
    pargs = ctx.actions.args()
    pargs.use_param_file("@%s", use_always = True)
    pargs.add_all(ctx.files.srcs)
    ctx.actions.run(
        outputs = [out, outdir],
        inputs = ctx.files.srcs + ctx.files.trees,
        executable = ctx.executable._tool,
        arguments = [args, pargs],
        mnemonic = "MyMnemonic",
        env = {"OUTPATH": out.path} if ctx.attr.literal else {},
        execution_requirements = {"supports-path-mapping": ""} if ctx.attr.req else {},
    )
    return [DefaultInfo(files = depset([out, outdir]))]

use = rule(_use_impl, attrs = {
    "srcs": attr.label_list(allow_files = True),
    "trees": attr.label_list(),
    "req": attr.bool(default = True),
    "literal": attr.bool(),
    "_tool": attr.label(default = "//pkg:tool", executable = True, cfg = "exec"),
})

def _both_impl(ctx):
    files = []
    for dep in ctx.attr.deps:
        files += dep[DefaultInfo].files.to_list()
    return [DefaultInfo(files = depset(files))]

both = rule(_both_impl, attrs = {"deps": attr.label_list(cfg = _set_flavor), "flavor": attr.string()})

def _coll_impl(ctx):
    out = ctx.actions.declare_file(ctx.label.name + ".out")
    ins = ctx.files.a + ctx.files.b
    args = ctx.actions.args()
    args.add(out)
    args.add_all(ins)
    ctx.actions.run_shell(
        outputs = [out],
        inputs = ins,
        command = 'o=$1; shift; echo "$@" > $o; cat "$@" >> $o',
        arguments = [args],
        mnemonic = "Coll",
        execution_requirements = {"supports-path-mapping": ""},
    )
    return [DefaultInfo(files = depset([out]))]

def _fa(settings, attr):
    return {"//pkg:flavor": "a"}

def _fb(settings, attr):
    return {"//pkg:flavor": "b"}

_ta = transition(implementation = _fa, inputs = [], outputs = ["//pkg:flavor"])
_tb = transition(implementation = _fb, inputs = [], outputs = ["//pkg:flavor"])

coll = rule(_coll_impl, attrs = {
    "a": attr.label_list(cfg = _ta, allow_files = True),
    "b": attr.label_list(cfg = _tb, allow_files = True),
})

def _t_impl(ctx):
    for t in ctx.attr.targets:
        for a in t.actions:
            print("ACT", a.mnemonic, a.argv, repr(a.content) if a.mnemonic == "FileWrite" else None, [f.path for f in a.inputs.to_list()], [f.path for f in a.outputs.to_list()], a.env)
    return [AnalysisTestResultInfo(success = True, message = "")]

_strip = analysis_test_transition(settings = {"//command_line_option:experimental_output_paths": "strip"})

pm_test = rule(_t_impl, analysis_test = True, attrs = {"targets": attr.label_list(cfg = _strip)})

def _pw_impl(ctx):
    out = ctx.actions.declare_file(ctx.label.name + ".params")
    args = ctx.actions.args()
    args.add_all(ctx.files.srcs)
    args.add_all(ctx.files.srcs, map_each = _dirname, format_each = "--dir=%s")
    if ctx.attr.trees:
        args.add_all(ctx.files.trees)
    ctx.actions.write(out, args, execution_requirements = {"supports-path-mapping": ""} if ctx.attr.req else {})
    return [DefaultInfo(files = depset([out]))]

pw = rule(_pw_impl, attrs = {
    "srcs": attr.label_list(allow_files = True),
    "trees": attr.label_list(),
    "req": attr.bool(default = True),
})

_strip_mod = analysis_test_transition(settings = {
    "//command_line_option:experimental_output_paths": "strip",
    "//command_line_option:modify_execution_info": ["MyMnemonic=+supports-path-mapping", "Other=-x,Third=+y"],
})

pm_mod_test = rule(_t_impl, analysis_test = True, attrs = {"targets": attr.label_list(cfg = _strip_mod)})
