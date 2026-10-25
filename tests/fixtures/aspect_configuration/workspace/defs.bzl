"""Aspect attribute configurations (docs/ASPECTS.md "Aspect attribute configurations").

Every label attribute of an aspect is resolved in its own configuration: the
base target's for `cfg = "target"`, the exec transition for the aspect's own
execution platform for `cfg = "exec"`/`config.exec()`, the transitioned one
for a Starlark transition. The aspect context's configuration surface
(`ctx.bin_dir`, `ctx.var`) is the base target's.
"""

ReaderInfo = provider(fields = ["value"])

def _flag_impl(ctx):
    return [ReaderInfo(value = ctx.build_setting_value)]

string_flag = rule(_flag_impl, build_setting = config.string(flag = True))

def _tool_impl(ctx):
    exe = ctx.actions.declare_file(ctx.label.name + ".sh")
    ctx.actions.write(exe, "#!/bin/sh\necho \"tool=$0 base=$2\" > \"$1\"\n", is_executable = True)
    return [DefaultInfo(executable = exe)]

tool = rule(_tool_impl, executable = True)

def _reader_impl(ctx):
    out = ctx.actions.declare_file(ctx.label.name + ".txt")
    ctx.actions.write(out, ctx.attr._flag[ReaderInfo].value + "\n")
    return [DefaultInfo(files = depset([out])), ReaderInfo(value = ctx.attr._flag[ReaderInfo].value)]

reader = rule(_reader_impl, attrs = {"_flag": attr.label(default = "//:flag")})

def _to_flag_impl(settings, attr):
    return {"//:flag": "via-aspect"}

_to_flag = transition(implementation = _to_flag_impl, inputs = [], outputs = ["//:flag"])

def _path(target):
    return target[DefaultInfo].files_to_run.executable.path

def _cfg_aspect_impl(target, ctx):
    out = ctx.actions.declare_file(ctx.label.name + ".cfg")
    ctx.actions.run(
        outputs = [out],
        executable = ctx.attr._exec_tool[DefaultInfo].files_to_run,
        arguments = [out.path, ctx.label.name],
        mnemonic = "AspectTool",
    )
    # A Starlark-transitioned label attribute is a list of its prerequisites
    # (StarlarkAttributesCollection.convertAttributeValue: isSplit()).
    reader = ctx.attr._transitioned[0]
    print("CFG {} bin={} mode={} exec={} default_exec={} target={} transitioned={}:{}".format(
        ctx.label.name,
        ctx.bin_dir.path,
        ctx.var["COMPILATION_MODE"],
        _path(ctx.attr._exec_tool),
        _path(ctx.attr._default_exec_tool),
        _path(ctx.attr._target_tool),
        reader[ReaderInfo].value,
        reader[DefaultInfo].files.to_list()[0].path,
    ))
    transitive = [dep[OutputGroupInfo].cfg_out for dep in ctx.rule.attr.deps]
    return [OutputGroupInfo(cfg_out = depset([out], transitive = transitive))]

cfg_aspect = aspect(
    implementation = _cfg_aspect_impl,
    attr_aspects = ["deps"],
    attrs = {
        "_exec_tool": attr.label(default = "//:tool", executable = True, cfg = "exec"),
        "_default_exec_tool": attr.label(default = "//:tool", executable = True, cfg = config.exec()),
        "_target_tool": attr.label(default = "//:tool", executable = True, cfg = "target"),
        "_transitioned": attr.label(default = "//:reader", cfg = _to_flag),
    },
)

def _bad_group_impl(target, ctx):
    return []

bad_group_aspect = aspect(
    implementation = _bad_group_impl,
    attrs = {
        "_tool": attr.label(default = "//:tool", executable = True, cfg = config.exec("nope")),
    },
)

def _lib_impl(ctx):
    return [DefaultInfo()]

lib = rule(_lib_impl, attrs = {"deps": attr.label_list()})

def _use_impl(ctx):
    groups = [dep[OutputGroupInfo].cfg_out for dep in ctx.attr.deps + ctx.attr.tools]
    return [DefaultInfo(files = depset(transitive = groups))]

use = rule(_use_impl, attrs = {
    "deps": attr.label_list(aspects = [cfg_aspect]),
    "tools": attr.label_list(cfg = "exec", aspects = [cfg_aspect]),
})
