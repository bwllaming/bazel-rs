def _t_impl(ctx):
    out = ctx.actions.declare_file(ctx.label.name + ".sh")
    ctx.actions.write(out, "#!/bin/sh\necho hi\n", is_executable = True)
    return [
        DefaultInfo(executable = out, runfiles = ctx.runfiles(files = ctx.files.data)),
        RunEnvironmentInfo(environment = ctx.attr.env2, inherited_environment = ctx.attr.inherit),
    ]

_ATTRS = {
    "data": attr.label_list(allow_files = True),
    "env2": attr.string_dict(),
    "inherit": attr.string_list(),
}

my_test = rule(implementation = _t_impl, test = True, attrs = _ATTRS)
my_bin = rule(implementation = _t_impl, executable = True, attrs = _ATTRS)

def _x_impl(ctx):
    out = ctx.actions.declare_file(ctx.label.name + ".sh")
    ctx.actions.write(out, "#!/bin/sh\nexit 0\n", is_executable = True)
    return [
        DefaultInfo(executable = out),
        testing.ExecutionInfo({"requires-x": "1", "local": ""}),
    ]

req_test = rule(implementation = _x_impl, test = True)
