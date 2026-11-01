def _impl(ctx):
    print("ext impl")
    return []

ext_rule = rule(implementation = _impl)

print("ext top")
