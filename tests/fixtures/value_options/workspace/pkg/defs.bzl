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

def _coverage_impl(settings, attr):
    return {"//command_line_option:collect_code_coverage": attr.coverage}

# Sets --collect_code_coverage (like `bazel coverage` does for every target).
_coverage_transition = transition(
    implementation = _coverage_impl,
    inputs = [],
    outputs = ["//command_line_option:collect_code_coverage"],
)

def _with_coverage_impl(ctx):
    return [DefaultInfo(files = depset(transitive = [
        dep[DefaultInfo].files
        for dep in ctx.attr.deps
    ]))]

# Builds `deps` in a configuration with --collect_code_coverage=`coverage`.
with_coverage = rule(
    implementation = _with_coverage_impl,
    attrs = {
        "coverage": attr.bool(),
        "deps": attr.label_list(cfg = _coverage_transition),
    },
)

def _strict_deps_impl(settings, attr):
    return {"//command_line_option:strict_proto_deps": "off"}

_strict_deps_transition = transition(
    implementation = _strict_deps_impl,
    inputs = [],
    outputs = ["//command_line_option:strict_proto_deps"],
)

def _with_strict_deps_off_impl(ctx):
    return [DefaultInfo(files = depset(transitive = [
        dep[DefaultInfo].files
        for dep in ctx.attr.deps
    ]))]

# Builds `deps` with --strict_proto_deps=off set by a Starlark transition
# (not modelled natively: an explicit error).
with_strict_deps_off = rule(
    implementation = _with_strict_deps_off_impl,
    attrs = {"deps": attr.label_list(cfg = _strict_deps_transition)},
)
