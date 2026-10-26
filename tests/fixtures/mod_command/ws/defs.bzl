def _impl(rctx):
    rctx.file("BUILD.bazel", "")

r = repository_rule(implementation = _impl, attrs = {"value": attr.string()})
