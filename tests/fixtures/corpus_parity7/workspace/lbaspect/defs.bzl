Info = provider(fields = ["lines"])

def _malloc_aspect_impl(target, ctx):
    malloc = ctx.attr._malloc
    exec_malloc = ctx.attr._exec_malloc
    line = "%s malloc=%s exec_malloc=%s files=%s" % (
        ctx.label.name,
        malloc.label if malloc else None,
        exec_malloc.label if exec_malloc else None,
        [f.basename for f in ctx.files._malloc],
    )
    lines = []
    for dep in getattr(ctx.rule.attr, "deps", []):
        if Info in dep:
            lines.extend(dep[Info].lines)
    return [Info(lines = lines + [line])]

malloc_aspect = aspect(
    implementation = _malloc_aspect_impl,
    attr_aspects = ["deps"],
    attrs = {
        "_malloc": attr.label(default = configuration_field(fragment = "cpp", name = "custom_malloc")),
        "_exec_malloc": attr.label(
            cfg = "exec",
            default = configuration_field(fragment = "cpp", name = "custom_malloc"),
        ),
    },
    fragments = ["cpp"],
)

def _lib_impl(ctx):
    return []

lib = rule(implementation = _lib_impl, attrs = {"deps": attr.label_list()})

def _set_malloc_impl(settings, attr):
    return {"//command_line_option:custom_malloc": "//lbaspect:m2"}

set_malloc = transition(
    implementation = _set_malloc_impl,
    inputs = [],
    outputs = ["//command_line_option:custom_malloc"],
)

transitioned_lib = rule(
    implementation = _lib_impl,
    attrs = {"deps": attr.label_list(cfg = set_malloc)},
)

def _report_impl(ctx):
    lines = []
    for dep in ctx.attr.deps:
        lines.extend(dep[Info].lines)
    out = ctx.actions.declare_file(ctx.label.name + ".txt")
    ctx.actions.write(out, "\n".join(lines) + "\n")
    return [DefaultInfo(files = depset([out]))]

report = rule(
    implementation = _report_impl,
    attrs = {"deps": attr.label_list(aspects = [malloc_aspect])},
)
