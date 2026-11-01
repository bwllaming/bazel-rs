def _gen_impl(rctx):
    rctx.file("defs.bzl", rctx.read(rctx.attr.defs))
    rctx.file("BUILD.bazel", rctx.read(rctx.attr.build))
    rctx.file("sub/BUILD.bazel", 'filegroup(name = "old", deprecation = "use new", visibility = ["//visibility:public"])\n')

gen = repository_rule(
    implementation = _gen_impl,
    attrs = {"build": attr.label(), "defs": attr.label()},
)
