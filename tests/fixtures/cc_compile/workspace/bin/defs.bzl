"""N63: the workspace-status surface of the fixture.

`stamped_headers` forwards the bazel_cc_build_info translator's non-redacted
group (the transform_version_file/transform_info_file outputs over
bazel-out/{volatile,stable}-status.txt); `status_cat` concatenates the two
status files an action sees.
"""

def _stamped_impl(ctx):
    return [
        DefaultInfo(
            files = ctx.attr.translator[OutputGroupInfo].non_redacted_build_info_files,
        ),
    ]

stamped_headers = rule(
    implementation = _stamped_impl,
    attrs = {
        "translator": attr.label(
            default = "@bazel_tools//tools/build_defs/build_info:cc_build_info",
            providers = [OutputGroupInfo],
        ),
    },
)

def _status_cat_impl(ctx):
    out = ctx.actions.declare_file(ctx.label.name + ".txt")
    ctx.actions.run_shell(
        outputs = [out],
        inputs = [ctx.info_file, ctx.version_file],
        command = "cat %s %s > %s" % (ctx.info_file.path, ctx.version_file.path, out.path),
    )
    return [DefaultInfo(files = depset([out]))]

status_cat = rule(_status_cat_impl)
