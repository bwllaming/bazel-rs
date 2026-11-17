# B13a: a transition on //command_line_option:minimum_os_version and
# ctx.fragments.cpp.minimum_os_version() (skylib's unittest_tests.bzl).
def _set_impl(settings, attr):
    return {"//command_line_option:minimum_os_version": attr.version or None}

_set = transition(
    implementation = _set_impl,
    inputs = [],
    outputs = ["//command_line_option:minimum_os_version"],
)

def _read_impl(settings, attr):
    return {"//command_line_option:minimum_os_version": str(settings["//command_line_option:minimum_os_version"]) + "-read"}

_read = transition(
    implementation = _read_impl,
    inputs = ["//command_line_option:minimum_os_version"],
    outputs = ["//command_line_option:minimum_os_version"],
)

def _show_impl(ctx):
    out = ctx.actions.declare_file(ctx.label.name + ".txt")
    ctx.actions.write(out, repr(ctx.fragments.cpp.minimum_os_version()) + "\n")
    return [DefaultInfo(files = depset([out]))]

show = rule(implementation = _show_impl, fragments = ["cpp"])

def _forward_impl(ctx):
    return [DefaultInfo(files = depset(transitive = [d[DefaultInfo].files for d in ctx.attr.deps]))]

set_version = rule(
    implementation = _forward_impl,
    attrs = {
        "deps": attr.label_list(cfg = _set),
        "version": attr.string(),
    },
)

read_version = rule(
    implementation = _forward_impl,
    attrs = {"deps": attr.label_list(cfg = _read)},
)
