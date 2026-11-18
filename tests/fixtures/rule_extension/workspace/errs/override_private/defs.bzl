def _impl(ctx):
    return ctx.super()

base = rule(_impl, attrs = {"_p": attr.label(default = "//ok:tool.txt")})
child = rule(_impl, parent = base, attrs = {"_p": attr.label(default = "//ok:dep")})
