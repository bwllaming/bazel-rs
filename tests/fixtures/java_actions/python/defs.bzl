# Loading-only declarations for unrelated @bazel_tools proguard tests. The
# fixture must fail if one ever enters analysis; no Python action is simulated.
def _unused(ctx):
    fail("Python is outside the Java header fixture")

py_binary = rule(implementation = _unused, attrs = {"srcs": attr.label_list(allow_files = True)})
py_test = rule(implementation = _unused, test = True, attrs = {"srcs": attr.label_list(allow_files = True), "deps": attr.label_list(), "data": attr.label_list(allow_files = True)})
