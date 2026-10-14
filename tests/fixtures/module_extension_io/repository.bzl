def _impl(ctx):
    ctx.file("data.json", ctx.attr.observed, executable = False)
    ctx.file("BUILD.bazel", 'exports_files(["data.json"])\n', executable = False)

observed_repository = repository_rule(
    implementation = _impl,
    attrs = {"observed": attr.string(mandatory = True)},
)
