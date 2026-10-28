load("//bazel_internal/test_rules/runtime_classpath:defs.bzl", "f")
def _impl(ctx):
    f(depset(), depset())
    return []
denied = rule(implementation = _impl)
