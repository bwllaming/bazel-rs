def _impl(ctx):
    return []

mandatory_label = rule(implementation = _impl, attrs = {"dep": attr.label(mandatory = True)})
mandatory_output = rule(implementation = _impl, attrs = {"out": attr.output(mandatory = True)})
