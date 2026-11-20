# B32/B33: Starlark transitions writing --instrumentation_filter (with
# --collect_code_coverage): the configuration's output root and the
# instrumentation decision of ctx.coverage_instrumented().
def _set_impl(settings, attr):
    return {
        "//command_line_option:collect_code_coverage": True,
        "//command_line_option:instrumentation_filter": attr.filter,
    }

_set = transition(
    implementation = _set_impl,
    inputs = [],
    outputs = [
        "//command_line_option:collect_code_coverage",
        "//command_line_option:instrumentation_filter",
    ],
)

def _leaf_impl(ctx):
    out = ctx.actions.declare_file(ctx.label.name + ".txt")
    ctx.actions.write(out, "%s instrumented=%s coverage=%s\n" % (
        out.path,
        ctx.coverage_instrumented(),
        ctx.configuration.coverage_enabled,
    ))
    return [DefaultInfo(files = depset([out]))]

leaf = rule(implementation = _leaf_impl, attrs = {"srcs": attr.label_list(allow_files = True)})

def _top_impl(ctx):
    return [DefaultInfo(files = depset(ctx.files.dep))]

top = rule(implementation = _top_impl, attrs = {
    "dep": attr.label(cfg = _set),
    "filter": attr.string(),
})
