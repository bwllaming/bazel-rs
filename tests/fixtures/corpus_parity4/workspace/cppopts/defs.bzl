# B32: Starlark transitions on CppOptions rules_cc's own tests set:
# --fission, --dynamic_mode, --build_test_dwp, --legacy_whole_archive and
# --incompatible_remove_legacy_whole_archive (output roots, select()).
_OPTS = [
    "//command_line_option:fission",
    "//command_line_option:dynamic_mode",
    "//command_line_option:build_test_dwp",
    "//command_line_option:legacy_whole_archive",
]

def _set_impl(settings, attr):
    return {
        "//command_line_option:fission": attr.fission,
        "//command_line_option:dynamic_mode": attr.dynamic_mode,
        "//command_line_option:build_test_dwp": attr.dwp,
        "//command_line_option:legacy_whole_archive": attr.legacy,
    }

_set = transition(implementation = _set_impl, inputs = [], outputs = _OPTS)

def _leaf_impl(ctx):
    out = ctx.actions.declare_file(ctx.label.name + ".txt")
    ctx.actions.write(out, "%s %s\n" % (out.path, ctx.attr.flags))
    return [DefaultInfo(files = depset([out]))]

leaf = rule(implementation = _leaf_impl, attrs = {"flags": attr.string_list()})

def _top_impl(ctx):
    return [DefaultInfo(files = depset(ctx.files.dep))]

top = rule(implementation = _top_impl, attrs = {
    "dep": attr.label(cfg = _set),
    "dwp": attr.bool(),
    "dynamic_mode": attr.string(),
    "fission": attr.string_list(),
    "legacy": attr.bool(),
})
