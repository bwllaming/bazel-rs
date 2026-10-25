def _t(ctx):
    out = ctx.actions.declare_file(ctx.label.name + ".sh")
    ctx.actions.symlink(output = out, target_file = ctx.file.src, is_executable = True)
    return [DefaultInfo(executable = out)]

probe_test = rule(implementation = _t, test = True, attrs = {"src": attr.label(allow_single_file = True)})
