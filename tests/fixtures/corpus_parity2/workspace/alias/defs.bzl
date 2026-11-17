# B7: a transition writing a flag through its Starlark label and through
# the native alias (rules_python's _zipapp_transition).
BuildSettingInfo = provider(fields = ["value"])

def _flag_impl(ctx):
    return [BuildSettingInfo(value = ctx.build_setting_value)]

bool_flag = rule(implementation = _flag_impl, build_setting = config.bool(flag = True))

def _set_impl(settings, attr):
    return {
        "//alias:zip": attr.starlark,
        "//command_line_option:zip_alias": attr.native,
    }

_set = transition(
    implementation = _set_impl,
    inputs = [],
    outputs = ["//alias:zip", "//command_line_option:zip_alias"],
)

def _show_impl(ctx):
    out = ctx.actions.declare_file(ctx.label.name + ".txt")
    ctx.actions.write(out, repr(ctx.attr._zip[BuildSettingInfo].value) + "\n")
    return [DefaultInfo(files = depset([out]))]

show = rule(implementation = _show_impl, attrs = {"_zip": attr.label(default = "//alias:zip")})

def _forward_impl(ctx):
    return [DefaultInfo(files = depset(transitive = [d[DefaultInfo].files for d in ctx.attr.deps]))]

both = rule(
    implementation = _forward_impl,
    attrs = {
        "deps": attr.label_list(cfg = _set),
        "native": attr.string(),
        "starlark": attr.bool(),
    },
)
