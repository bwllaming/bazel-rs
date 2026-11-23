# B56: --apple_generate_dsym (CppOptions.appleGenerateDsym) as a
# configuration option: Starlark transitions write and read it, select()
# matches it, the output root's ST- hash covers it and
# `ctx.fragments.cpp.apple_generate_dsym` reads it (rules_zig's dsym tests).
_OPT = "//command_line_option:apple_generate_dsym"

def _set_impl(settings, attr):
    return {_OPT: attr.dsym}

_set = transition(implementation = _set_impl, inputs = [], outputs = [_OPT])

def _read_impl(settings, attr):
    print("READ", settings[_OPT])
    return {}

_read = transition(implementation = _read_impl, inputs = [_OPT], outputs = [])

def _leaf_impl(ctx):
    out = ctx.actions.declare_file(ctx.label.name + ".txt")
    ctx.actions.write(out, "%s %s %s\n" % (out.path, ctx.fragments.cpp.apple_generate_dsym, ctx.attr.flags))
    return [DefaultInfo(files = depset([out]))]

leaf = rule(
    implementation = _leaf_impl,
    attrs = {"flags": attr.string_list()},
    fragments = ["cpp"],
)

def _top_impl(ctx):
    return [DefaultInfo(files = depset(ctx.files.dep))]

top = rule(implementation = _top_impl, cfg = _read, attrs = {
    "dep": attr.label(cfg = _set),
    "dsym": attr.bool(),
})

def _check_impl(ctx):
    target = ctx.attr.target_under_test[0]
    files = [f.path for f in target[DefaultInfo].files.to_list()]
    return [AnalysisTestResultInfo(success = True, message = "FILES %s" % files)]

check_test = rule(
    implementation = _check_impl,
    analysis_test = True,
    attrs = {"target_under_test": attr.label(cfg = analysis_test_transition(settings = {_OPT: True}))},
)
