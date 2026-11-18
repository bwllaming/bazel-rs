def _impl(ctx):
    return []

guarded = rule(_impl, extendable = "//allow:list")
