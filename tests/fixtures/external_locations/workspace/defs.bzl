def _impl(ctx):
    print("main impl")
    return []

main_rule = rule(implementation = _impl)
