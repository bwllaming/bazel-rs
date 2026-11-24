def _tr(settings, attr):
    print("READ", settings["//command_line_option:compiler"], settings["//command_line_option:host_compiler"])
    return {"//command_line_option:compiler": "gcc-x"}
tr = transition(implementation = _tr, inputs = ["//command_line_option:compiler", "//command_line_option:host_compiler"], outputs = ["//command_line_option:compiler"])
def _fwd(ctx):
    return [DefaultInfo(files = depset(ctx.files.dep + ctx.files.tool))]
fwd = rule(implementation = _fwd, attrs = {"dep": attr.label(cfg = tr), "tool": attr.label(cfg = "exec")})
