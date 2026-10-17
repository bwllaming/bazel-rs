"""A repository rule defined in the non-root module."""

def _dep_repo_impl(rctx):
    rctx.file("BUILD.bazel", "filegroup(name = \"x\", visibility = [\"//visibility:public\"])\n")

dep_repo = repository_rule(
    implementation = _dep_repo_impl,
    attrs = {
        "values": attr.string_list(),
        "root": attr.label(),
    },
)
