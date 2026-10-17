"""F5: the genuine bazel_cc_build_info translator through OutputGroupInfo."""

def _dump_impl(ctx):
    info = ctx.attr.translator[OutputGroupInfo]
    doc = {
        "type": type(info),
        "groups": [g for g in info],
        "dir": dir(info),
        "files": {
            g: [[f.short_path, f.basename, f.is_source, str(f.owner)] for f in getattr(info, g).to_list()]
            for g in dir(info)
        },
        "in": ["redacted_build_info_files" in info, "nope" in info],
        "index": [f.basename for f in info["redacted_build_info_files"].to_list()],
        "has_provider": OutputGroupInfo in ctx.attr.translator,
    }
    out = ctx.actions.declare_file(ctx.label.name + ".json")
    ctx.actions.write(out, json.encode_indent(doc, indent = " ") + "\n")
    return [DefaultInfo(files = depset([out]))]

build_info_dump = rule(
    implementation = _dump_impl,
    attrs = {"translator": attr.label(providers = [OutputGroupInfo])},
)

def _group_impl(ctx):
    return [DefaultInfo(files = getattr(ctx.attr.translator[OutputGroupInfo], ctx.attr.group))]

build_info_group = rule(
    implementation = _group_impl,
    attrs = {
        "translator": attr.label(providers = [OutputGroupInfo]),
        "group": attr.string(),
    },
)
