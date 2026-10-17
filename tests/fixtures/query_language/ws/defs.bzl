load("//macros:inner.bzl", "INNER")

MyInfo = provider(fields = ["value"])

def _impl(ctx):
    return [MyInfo(value = INNER)]

my_library = rule(
    implementation = _impl,
    attrs = {
        "srcs": attr.label_list(allow_files = True),
        "deps": attr.label_list(),
        "data": attr.label_list(allow_files = True, cfg = "exec"),
        "_tool": attr.label(default = "//lib:tool"),
        "flavor": attr.string(default = "plain"),
        "stripped": attr.bool(),
    },
)

my_test = rule(
    implementation = _impl,
    test = True,
    attrs = {
        "srcs": attr.label_list(allow_files = True),
        "deps": attr.label_list(),
    },
)
