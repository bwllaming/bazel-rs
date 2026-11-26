VarInfo = provider(fields = ["text"])

def _var_aspect_impl(target, ctx):
    return [VarInfo(text = "%s cpu=%s mode=%s keys=%s" % (
        target.label.name,
        ctx.var.get("TARGET_CPU", "<none>"),
        ctx.var.get("COMPILATION_MODE", "<none>"),
        ",".join(sorted(ctx.var.keys())),
    ))]

var_aspect = aspect(implementation = _var_aspect_impl)

def _lib_impl(ctx):
    return []

lib = rule(implementation = _lib_impl)

def _opt_impl(settings, attr):
    return {"//command_line_option:compilation_mode": "opt"}

_opt = transition(
    implementation = _opt_impl,
    inputs = [],
    outputs = ["//command_line_option:compilation_mode"],
)

def _report_impl(ctx):
    out = ctx.actions.declare_file(ctx.label.name + ".txt")
    ctx.actions.write(out, "\n".join([dep[VarInfo].text for dep in ctx.attr.deps]) + "\n")
    return [DefaultInfo(files = depset([out]))]

# B67: the aspect reads ctx.var in a configuration (the transition's) whose
# target platform nothing analyzed before it resolved.
report = rule(
    implementation = _report_impl,
    attrs = {"deps": attr.label_list(cfg = _opt, aspects = [var_aspect])},
)

_strict = analysis_test_transition(settings = {"//command_line_option:experimental_strict_java_deps": "error"})

def _check_impl(ctx):
    return [AnalysisTestResultInfo(success = True, message = ctx.attr.target[0][VarInfo].text)]

# rules_testing's analysis_test shape (rules_java's test_strict_java_deps_error).
check_test = rule(
    implementation = _check_impl,
    attrs = {"target": attr.label(cfg = _strict, aspects = [var_aspect])},
    analysis_test = True,
)
