load("//:defs.bzl", "asp_a", "asp_b")

def _impl(ctx):
    return []

r = rule(implementation = _impl, attrs = {"deps": attr.label_list(aspects = [asp_b, asp_a])})
