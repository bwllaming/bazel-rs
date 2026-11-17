"""A minimal http_archive (no @bazel_tools, so the test runs offline)."""

def _archive_impl(ctx):
    ctx.download_and_extract(
        url = ctx.attr.urls,
        sha256 = ctx.attr.sha256,
        strip_prefix = ctx.attr.strip_prefix,
    )

archive = repository_rule(
    implementation = _archive_impl,
    attrs = {
        "sha256": attr.string(),
        "strip_prefix": attr.string(),
        "urls": attr.string_list(),
    },
)
