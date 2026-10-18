"""Order probe (N56 review): the collected `_validation` to_list() order —
the built-in `toolchains` attribute first (rule-class order), then the
declared attributes in declaration order (`z_first` before `a_second`)."""

def _tval(ctx):
    out = ctx.actions.declare_file(ctx.label.name + ".validated")
    ctx.actions.write(out, "validated:" + ctx.label.name)
    return [
        DefaultInfo(files = depset([out])),
        platform_common.TemplateVariableInfo({"VAR_" + ctx.label.name: "v"}),
        OutputGroupInfo(_validation = depset([out])),
    ]

tval = rule(implementation = _tval)

def _ordered(ctx):
    out = ctx.actions.declare_file(ctx.label.name + ".out")
    ctx.actions.write(out, "x")
    return [DefaultInfo(files = depset([out]))]

ordered = rule(
    implementation = _ordered,
    attrs = {
        "z_first": attr.label(),
        "a_second": attr.label(),
    },
)

def _check_order(ctx):
    files = ctx.attr.dep[OutputGroupInfo]._validation.to_list()
    print("ORDER " + ",".join([f.short_path for f in files]))
    out = ctx.actions.declare_file(ctx.label.name + ".out")
    ctx.actions.write(out, "x")
    return [DefaultInfo(files = depset([out]))]

check_order = rule(implementation = _check_order, attrs = {"dep": attr.label()})
