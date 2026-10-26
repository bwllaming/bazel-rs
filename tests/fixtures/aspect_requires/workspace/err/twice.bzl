load("//:defs.bzl", "asp_a", "lib")

def _impl(ctx):
    return []

r = rule(implementation = _impl, attrs = {"deps": attr.label_list(aspects = [asp_a, asp_a])})
