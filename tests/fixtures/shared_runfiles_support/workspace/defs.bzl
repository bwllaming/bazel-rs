def _tool(ctx):
    exe = ctx.actions.declare_file(ctx.label.name + "_/" + ctx.label.name)
    ctx.actions.write(exe, "#!/bin/sh\necho hi > \"$1\"\n", is_executable = True)
    return [DefaultInfo(executable = exe, runfiles = ctx.runfiles(files = ctx.files.data))]

tool = rule(_tool, executable = True, attrs = {"data": attr.label_list(allow_files = True)})

def _gen(ctx):
    out = ctx.actions.declare_file(ctx.label.name + ".txt")
    ctx.actions.run(executable = ctx.executable.tool, arguments = [out.path], outputs = [out])
    return [DefaultInfo(files = depset([out]))]

gen = rule(_gen, attrs = {"tool": attr.label(cfg = "exec", executable = True)})

def _platform_impl(settings, attr):
    return {"//command_line_option:platforms": [str(attr.platform)]}

_platform = transition(
    implementation = _platform_impl,
    inputs = [],
    outputs = ["//command_line_option:platforms"],
)

def _on_platform(ctx):
    return [DefaultInfo(files = depset(ctx.files.srcs))]

on_platform = rule(_on_platform, attrs = {
    "srcs": attr.label_list(cfg = _platform),
    "platform": attr.label(),
})
