def _impl(ctx):
    ctx.expand_location("$(location :x)", short_paths = True)
    return []

short = rule(_impl)
