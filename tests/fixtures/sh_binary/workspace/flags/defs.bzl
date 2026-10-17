def _impl(ctx):
    return []

r = rule(_impl, attrs = {"deps": attr.label_list(flags = ["SKIP_CONSTRAINTS_OVERRIDE", "DIRECT_COMPILE_TIME_INPUT"])})
