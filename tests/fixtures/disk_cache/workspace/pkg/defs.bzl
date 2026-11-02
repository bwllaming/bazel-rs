def _shell(ctx):
    out = ctx.actions.declare_file(ctx.label.name + ".out")
    ctx.actions.run_shell(command = "tr a-z A-Z < $1 > $2", arguments = [ctx.file.src.path, out.path], inputs = [ctx.file.src], outputs = [out])
    return [DefaultInfo(files = depset([out]))]
shell = rule(_shell, attrs = {"src": attr.label(allow_single_file = True)})
