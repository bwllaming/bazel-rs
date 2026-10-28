load("//bazel_internal/test_rules/native_deps_dirs:defs.bzl", "f")
def _impl(ctx):
    f(depset())
    return []
denied = rule(implementation = _impl)
