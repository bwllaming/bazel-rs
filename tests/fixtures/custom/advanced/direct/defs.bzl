def _direct_impl(ctx):
    script = ctx.actions.declare_file(ctx.label.name + "/worker.sh")
    out = ctx.actions.declare_file(ctx.label.name + ".txt", sibling = script)
    ctx.actions.write(
        output = script,
        content = "#!/bin/bash\nset -eu\ncat \"$1\" \"$2\" > \"$3\"\nprintf '%s\\n%s\\n' \"$4\" \"$CUSTOM_ENV\" >> \"$3\"\n",
        is_executable = True,
    )
    arguments = [ctx.file.src.path, ctx.file.tool.path, out.path, "literal $HOME; *.txt 'quoted' \"double\""]
    env = {"CUSTOM_ENV": "env $HOME; *.txt 'quoted' \"double\""}
    if ctx.attr.string_executable:
        ctx.actions.run(
            executable = "/bin/bash",
            inputs = [ctx.file.src],
            tools = [script, ctx.file.tool],
            outputs = [out],
            arguments = [script.path] + arguments,
            env = env,
        )
    else:
        ctx.actions.run(
            executable = script,
            inputs = [ctx.file.src],
            tools = [ctx.file.tool],
            outputs = [out],
            arguments = arguments,
            env = env,
        )
    return [DefaultInfo(files = depset([out]))]

direct = rule(implementation = _direct_impl, attrs = {
    "src": attr.label(allow_single_file = True, mandatory = True),
    "tool": attr.label(allow_single_file = True, mandatory = True),
    "string_executable": attr.bool(default = False),
})
