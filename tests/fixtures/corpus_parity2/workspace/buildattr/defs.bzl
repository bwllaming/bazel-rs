# An attr.label default with a relative or apparent label, created while a
# BUILD file evaluates (rules_testing analysis tests in rules_cc:
# `"_is_linux": attr.label(default = "@platforms//os:linux")` inside a
# macro); resolved against this .bzl file (LabelConverter
# .forBzlEvaluatingThread). The implementation fails on another label.
def _impl(ctx):
    labels = [str(ctx.attr._dep.label), str(ctx.attr._tools.label)]
    if labels != ["@@//buildattr:dep", "@@bazel_tools//tools/cpp:empty"]:
        fail("unexpected labels: %s" % labels)
    return [AnalysisTestResultInfo(success = True, message = "ok")]

def make(name):
    testing.analysis_test(
        name = name,
        implementation = _impl,
        attrs = {
            "_dep": attr.label(default = ":dep"),
            "_tools": attr.label(default = "@bazel_tools//tools/cpp:empty"),
        },
    )
