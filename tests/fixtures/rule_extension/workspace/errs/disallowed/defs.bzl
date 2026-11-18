load("//allow:defs.bzl", "guarded")

def _impl(ctx):
    return ctx.super()

child = rule(_impl, parent = guarded)
