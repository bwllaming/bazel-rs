def _impl(ctx):
    return [DefaultInfo(runfiles = ctx.runfiles(skip_conflict_checking = True))]

skipper = rule(_impl)
