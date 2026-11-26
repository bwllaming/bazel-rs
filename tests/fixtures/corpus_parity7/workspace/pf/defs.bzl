BuildSettingInfo = provider(fields = ["value"])

def _string_flag_impl(ctx):
    return [BuildSettingInfo(value = ctx.build_setting_value)]

string_flag = rule(
    implementation = _string_flag_impl,
    build_setting = config.string(flag = True),
)

def _show_impl(ctx):
    out = ctx.actions.declare_file(ctx.label.name + ".txt")
    ctx.actions.write(out, "libc=%s mode=%s define=%s copts=%s\n" % (
        ctx.attr._libc[BuildSettingInfo].value,
        ctx.var["COMPILATION_MODE"],
        ctx.var.get("pf", "<unset>"),
        ",".join(ctx.fragments.cpp.copts),
    ))
    return [DefaultInfo(files = depset([out]))]

show = rule(
    implementation = _show_impl,
    attrs = {"_libc": attr.label(default = "//pf:libc")},
    fragments = ["cpp"],
)

def _to_platform_impl(settings, attr):
    return {
        "//command_line_option:platforms": [str(attr.platform)],
        "//pf:libc": attr.libc,
    }

_to_platform = transition(
    implementation = _to_platform_impl,
    inputs = [],
    outputs = ["//command_line_option:platforms", "//pf:libc"],
)

def _on_platform_impl(ctx):
    return [DefaultInfo(files = depset(ctx.files.target))]

# platform_transition_binary-like: the platform's flags apply to the
# transitioned configuration, over the transition's own value.
on_platform = rule(
    implementation = _on_platform_impl,
    attrs = {
        "target": attr.label(cfg = _to_platform),
        "platform": attr.label(),
        "libc": attr.string(default = "musl"),
    },
)
