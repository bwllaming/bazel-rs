def _r(ctx):
    return []
r = rule(implementation = _r, attrs = {"deps": attr.label_list(), "srcs": attr.label_list(allow_files = [".x"])})

_tr = analysis_test_transition(settings = {"//command_line_option:allow_analysis_failures": True})

def _t(ctx):
    target = ctx.attr.target_under_test[0]
    if AnalysisFailureInfo in target:
        for cause in target[AnalysisFailureInfo].causes.to_list():
            print("CAUSE", cause.label, cause.message)
    else:
        print("NO FAILURE")
    return [AnalysisTestResultInfo(success = True, message = "")]
t_test = rule(implementation = _t, analysis_test = True, attrs = {"target_under_test": attr.label(cfg = _tr)})
