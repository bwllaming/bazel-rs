"""Probes of config_setting(values = {"enable_runfiles": ...})."""

def _probe_impl(ctx):
    out = ctx.actions.declare_file(ctx.label.name + ".txt")
    lines = list(ctx.attr.matches)
    files = [out]
    if ctx.file.tool:
        lines.append("tool=" + ctx.file.tool.path)
        files.append(ctx.file.tool)
    ctx.actions.write(out, "\n".join(lines) + "\n")
    return [DefaultInfo(files = depset(files))]

# Writes the select()ed `matches`; an exec-configured `tool` is built too.
probe = rule(
    implementation = _probe_impl,
    attrs = {
        "matches": attr.string_list(),
        "tool": attr.label(cfg = "exec", allow_single_file = True),
    },
)

def matched(settings):
    """The concatenated matches of `settings` (`:name` labels)."""
    result = []
    for setting in settings:
        name = setting.split(":")[-1]
        result += select({
            setting: [name],
            "//conditions:default": ["!" + name],
        })
    return result

def _flag_impl(ctx):
    out = ctx.actions.declare_file(ctx.label.name + ".txt")
    ctx.actions.write(out, "enable_runfiles=%s\n" % ctx.attr.enable_runfiles)
    return [DefaultInfo(files = depset([out]))]

# aspect_rules_js's js_binary shape: a bool attribute selected on
# @aspect_bazel_lib//lib:enable_runfiles.
flag = rule(
    implementation = _flag_impl,
    attrs = {"enable_runfiles": attr.bool()},
)
