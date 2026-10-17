def _impl(ctx):
    env = ctx.attr.dep[RunEnvironmentInfo].environment
    out = ctx.actions.declare_file(ctx.label.name + ".txt")
    ctx.actions.write(out, "\n".join(["%s=%s" % (k, env[k]) for k in sorted(env)]) + "\n")
    return [DefaultInfo(files = depset([out]))]

show_env = rule(_impl, attrs = {"dep": attr.label()})
