def _impl(ctx):
    pass

probe_rule = rule(implementation = _impl, attrs = {"deps": attr.label_list()})
