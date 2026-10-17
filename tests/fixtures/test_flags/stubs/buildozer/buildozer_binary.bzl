def _empty_impl(rctx):
    rctx.file("BUILD.bazel", "")
_empty = repository_rule(implementation = _empty_impl)
def _impl(mctx):
    _empty(name = "buildozer_binary")
buildozer_binary = module_extension(implementation = _impl)
