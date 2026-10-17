def _empty_impl(rctx):
    rctx.file("BUILD.bazel", "")
_empty = repository_rule(implementation = _empty_impl)
def _toolchains_impl(mctx):
    _empty(name = "local_jdk")
    _empty(name = "remote_java_tools")
toolchains = module_extension(implementation = _toolchains_impl)
