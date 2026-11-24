def _exe(ctx):
    out = ctx.actions.declare_file(ctx.label.name + ".sh")
    ctx.actions.write(out, "#!/bin/sh\n", is_executable = True)
    return [DefaultInfo(executable = out, files = depset([out, ctx.file.extra]), runfiles = ctx.runfiles(files = [ctx.file.extra]))]

exe = rule(implementation = _exe, executable = True, attrs = {"extra": attr.label(allow_single_file = True)})

def _r(ctx):
    inputs, argv, manifests = ctx.resolve_command(
        command = ctx.attr.cmd,
        expand_locations = ctx.attr.expand,
        tools = ctx.attr.tools,
        label_dict = {ctx.attr.extra.label: ctx.files.extra} if ctx.attr.extra else {},
        make_variables = {"FOO": "bar"} if ctx.attr.make else None,
        attribute = ctx.attr.attribute or None,
    )
    out = ctx.actions.declare_file(ctx.label.name + ".txt")
    ctx.actions.write(out, repr([[f.path for f in inputs], argv, manifests]) + "\n")
    return [DefaultInfo(files = depset([out]))]

r = rule(implementation = _r, attrs = {
    "cmd": attr.string(),
    "expand": attr.bool(),
    "make": attr.bool(),
    "attribute": attr.string(),
    "tools": attr.label_list(cfg = "exec"),
    "extra": attr.label(allow_files = True),
    "srcs": attr.label_list(allow_files = True),
})
