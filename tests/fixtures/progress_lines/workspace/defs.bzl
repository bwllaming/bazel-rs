def _t_impl(ctx):
    out = ctx.actions.declare_file(ctx.label.name + ".sh")
    ctx.actions.write(out, "#!/bin/sh\nsleep %s\nexit %s\n" % (ctx.attr.sleep, ctx.attr.code), is_executable = True)
    return [DefaultInfo(executable = out)]

slow_test = rule(implementation = _t_impl, test = True, attrs = {"sleep": attr.string(), "code": attr.string(default = "0")})
