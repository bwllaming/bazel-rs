def _lint_impl(target, ctx):
    if ctx.toolchains[Label("@asptc//:tc_type")] == None:
        fail("no toolchain")
    return []

# An aspect's own dependencies (an implicit attribute in @asptool, a
# toolchain in @asptc) are transitive packages of the targets whose
# dependencies it is applied to (B62).
lint_aspect = aspect(
    _lint_impl,
    attr_aspects = ["deps"],
    attrs = {
        "_tool": attr.label(default = "@asptool//:tool.sh", allow_single_file = True, cfg = "exec"),
    },
    toolchains = [str(Label("@asptc//:tc_type"))],
)
