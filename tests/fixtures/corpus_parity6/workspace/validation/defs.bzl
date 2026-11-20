def _subject(ctx):
    out = ctx.actions.declare_file(ctx.label.name + ".out")
    ctx.actions.write(out, "subject")
    validated = ctx.actions.declare_file(ctx.label.name + ".validated")
    ctx.actions.write(validated, "validated")
    return [
        DefaultInfo(files = depset([out])),
        OutputGroupInfo(_validation = depset([validated])),
    ]

subject = rule(implementation = _subject)

def _consumer(ctx):
    out = ctx.actions.declare_file(ctx.label.name + ".out")
    ctx.actions.write(out, "consumer")
    return [DefaultInfo(files = depset([out]))]

consumer = rule(implementation = _consumer, attrs = {"dep": attr.label()})

def _check(ctx):
    return [AnalysisTestResultInfo(success = True, message = "")]

starlark_analysis_test = rule(
    implementation = _check,
    attrs = {"target": attr.label()},
    analysis_test = True,
)

def _testing_impl(ctx):
    return [AnalysisTestResultInfo(success = True, message = "")]

def testing_analysis_test(name, target):
    testing.analysis_test(
        name,
        _testing_impl,
        attrs = {"target": attr.label()},
        attr_values = {"target": target},
    )
