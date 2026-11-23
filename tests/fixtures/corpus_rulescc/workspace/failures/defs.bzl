# B32: prerequisite validation errors (a misplaced source file of an
# attribute with a file-type list) of a target analyzed with
# --allow_analysis_failures become its AnalysisFailureInfo, and an aspect
# applied to it is the errored aspect (ConfiguredTargetFactory
# .createAspect): rules_cc's test_wrong_cc_import_definitions_fails.
ProbeInfo = provider(fields = ["failed"])

def _probe_impl(target, ctx):
    return [ProbeInfo(failed = AnalysisFailureInfo in target)]

_probe = aspect(implementation = _probe_impl)

_allow = analysis_test_transition(
    settings = {"//command_line_option:allow_analysis_failures": True},
)

def _failure_test_impl(ctx):
    dep = ctx.attr.dep[0]
    causes = dep[AnalysisFailureInfo].causes.to_list()
    print("CAUSES", [(str(cause.label), cause.message) for cause in causes])
    print("PROBE", ProbeInfo in dep)
    return [AnalysisTestResultInfo(success = True, message = "")]

failure_test = rule(
    implementation = _failure_test_impl,
    analysis_test = True,
    attrs = {"dep": attr.label(cfg = _allow, aspects = [_probe])},
)

def _picky_impl(ctx):
    return []

picky = rule(implementation = _picky_impl, attrs = {
    "lib": attr.label(allow_single_file = [".a", ".lib"]),
    "libs": attr.label_list(allow_files = [".so"]),
})
