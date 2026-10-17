"""N53 fixture rules: TemplateVariableInfo providers, ctx.var dumps and the
~/code gogenrule shape (src/beyondwork.ai/gogenrule.bzl without rules_go)."""

def _make_vars_impl(ctx):
    return [
        platform_common.TemplateVariableInfo(ctx.attr.variables),
        DefaultInfo(files = depset(ctx.files.srcs)),
    ]

make_vars = rule(
    implementation = _make_vars_impl,
    attrs = {
        "variables": attr.string_dict(),
        "srcs": attr.label_list(allow_files = True),
    },
)

def _plain_impl(ctx):
    return [DefaultInfo()]

plain = rule(implementation = _plain_impl)

def _dump_impl(ctx):
    lines = [
        "var=" + repr(ctx.var),
        "var_type=" + type(ctx.var),
        "attr.toolchains=" + repr(ctx.attr.toolchains),
        "files.toolchains=" + repr([f.path for f in ctx.files.toolchains]),
        "expand_empty=" + ctx.expand_make_variables("cmd", ctx.attr.cmd, {}),
        "expand_var=" + ctx.expand_make_variables("cmd", ctx.attr.cmd, ctx.var),
        "expand_extra=" + ctx.expand_make_variables("cmd", ctx.attr.cmd, {"FOO": "extra"}),
    ]
    out = ctx.actions.declare_file(ctx.label.name + ".txt")
    ctx.actions.write(out, "\n".join(lines) + "\n")
    return [DefaultInfo(files = depset([out]))]

dump = rule(
    implementation = _dump_impl,
    attrs = {"cmd": attr.string()},
)

def _gogenrule_like_impl(ctx):
    ctx.actions.run_shell(
        inputs = ctx.files.srcs,
        outputs = ctx.outputs.outs,
        command = ctx.expand_make_variables("cmd", ctx.expand_location(ctx.attr.cmd), ctx.var),
        mnemonic = "GoGenrule",
        tools = ctx.files.tools,
    )
    return [DefaultInfo(files = depset(ctx.outputs.outs))]

gogenrule_like = rule(
    implementation = _gogenrule_like_impl,
    attrs = {
        "srcs": attr.label_list(allow_files = True),
        "tools": attr.label_list(cfg = "exec", allow_files = True),
        "outs": attr.output_list(),
        "cmd": attr.string(),
    },
)

def _locs_impl(ctx):
    k = ctx.attr.case
    if k == "ok":
        text = ctx.expand_location(ctx.attr.text, [ctx.attr.dep])
    elif k == "plain":
        text = ctx.expand_location(ctx.attr.text)
    elif k == "duplicate":
        text = ctx.expand_location(ctx.attr.text, [ctx.attr.dep, ctx.attr.dep])
    else:
        fail("bad case " + k)
    out = ctx.outputs.out or ctx.actions.declare_file(ctx.label.name + ".txt")
    ctx.actions.write(out, "expand_location=" + text + "\n")
    return [DefaultInfo(files = depset([out]))]

locs = rule(
    implementation = _locs_impl,
    attrs = {
        "case": attr.string(),
        "text": attr.string(),
        "dep": attr.label(allow_files = True),
        "srcs": attr.label_list(allow_files = True),
        "deps": attr.label_list(allow_files = True),
        "out": attr.output(),
    },
)
