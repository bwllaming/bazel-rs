_SCRIPT = """#!/bin/sh
for a in "$@"; do printf '[%s]\\n' "$a"; done
"""

def _impl(ctx):
    out = ctx.actions.declare_file(ctx.label.name + ".sh")
    ctx.actions.write(out, _SCRIPT, is_executable = True)
    files = ctx.files.data + ctx.files.srcs
    return [DefaultInfo(executable = out, runfiles = ctx.runfiles(files = files))]

_ATTRS = {
    "data": attr.label_list(allow_files = True),
    "srcs": attr.label_list(allow_files = True),
    "deps": attr.label_list(),
    "tools": attr.label_list(cfg = "exec", allow_files = True),
}

arg_test = rule(implementation = _impl, test = True, attrs = _ATTRS)
argbin = rule(implementation = _impl, executable = True, attrs = _ATTRS)

def _gen_impl(ctx):
    outs = []
    for name in ctx.attr.names:
        out = ctx.actions.declare_file(name)
        ctx.actions.write(out, name)
        outs.append(out)
    return [DefaultInfo(files = depset(outs))]

gen = rule(implementation = _gen_impl, attrs = {"names": attr.string_list()})

def _vars_impl(ctx):
    return [platform_common.TemplateVariableInfo({"MY_TOOLCHAIN_VAR": "from toolchain"})]

vars_rule = rule(implementation = _vars_impl)
