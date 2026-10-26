def _aspect_impl(target, ctx):
    return []

extra_aspect = aspect(
    implementation = _aspect_impl,
    attrs = {"_extra": attr.label(default = "//:extra.txt", allow_files = True)},
)

def _impl(ctx):
    ctx.actions.write(ctx.outputs.out, "must not execute during cquery")
    return [DefaultInfo(files = depset([ctx.outputs.out]))]

fixture_rule = rule(
    implementation = _impl,
    attrs = {
        "srcs": attr.label_list(allow_files = True),
        "dep": attr.label(allow_files = True),
        "aspect_dep": attr.label(aspects = [extra_aspect]),
        "unset_dep": attr.label(),
        "integer": attr.int(default = 0),
        "integers": attr.int_list(),
        "text": attr.string(default = ""),
        "texts": attr.string_list(),
        "enabled": attr.bool(default = False),
        "default_enabled": attr.bool(default = False),
        "dict": attr.string_dict(),
        "list_dict": attr.string_list_dict(),
        "label_dict": attr.label_keyed_string_dict(allow_files = True),
        "unary_dict": attr.string_keyed_label_dict(allow_files = True),
        "labels_dict": attr.label_list_dict(allow_files = True),
        "out": attr.output(mandatory = True),
    },
)
