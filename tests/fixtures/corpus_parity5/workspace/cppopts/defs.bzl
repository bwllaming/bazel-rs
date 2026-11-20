# B32: Starlark transitions on the CppOptions booleans rules_cc's own tests
# set: --save_temps, --start_end_lib, --interface_shared_objects and
# --process_headers_in_dependencies (output roots, select()).
_OPTS = [
    "//command_line_option:save_temps",
    "//command_line_option:start_end_lib",
    "//command_line_option:interface_shared_objects",
    "//command_line_option:process_headers_in_dependencies",
]

def _set_impl(settings, attr):
    return {
        "//command_line_option:save_temps": attr.temps,
        "//command_line_option:start_end_lib": attr.start_end,
        "//command_line_option:interface_shared_objects": attr.ifso,
        "//command_line_option:process_headers_in_dependencies": attr.headers,
    }

_set = transition(implementation = _set_impl, inputs = [], outputs = _OPTS)

def _read_impl(settings, attr):
    print("READ", [settings[option] for option in _OPTS])
    return {}

_read = transition(implementation = _read_impl, inputs = _OPTS, outputs = [])

def _leaf_impl(ctx):
    out = ctx.actions.declare_file(ctx.label.name + ".txt")
    ctx.actions.write(out, "%s %s\n" % (out.path, ctx.attr.flags))
    return [DefaultInfo(files = depset([out]))]

leaf = rule(implementation = _leaf_impl, attrs = {"flags": attr.string_list()})

def _top_impl(ctx):
    return [DefaultInfo(files = depset(ctx.files.dep))]

top = rule(implementation = _top_impl, cfg = _read, attrs = {
    "dep": attr.label(cfg = _set),
    "headers": attr.bool(),
    "ifso": attr.bool(),
    "start_end": attr.bool(),
    "temps": attr.bool(),
})
