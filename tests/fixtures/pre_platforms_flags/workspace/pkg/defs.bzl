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

def probes(settings):
    """One probe `p_<name>` per config_setting `:<name>`."""
    for name in settings:
        probe(
            name = "p_" + name,
            matches = select({
                ":" + name: [name],
                "//conditions:default": ["!" + name],
            }),
        )

def matched(settings):
    """The concatenated matches of `settings` (labels or `:name`)."""
    result = []
    for setting in settings:
        name = setting.split(":")[-1]
        result += select({
            setting: [name],
            "//conditions:default": ["!" + name],
        })
    return result
