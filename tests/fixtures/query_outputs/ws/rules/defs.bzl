load(":common.bzl", "GREETING")

KitchenInfo = provider(fields = ["value"])

def _impl(ctx):
    return [KitchenInfo(value = GREETING)]

_ATTRS = {
    "s": attr.string(default = "dflt"),
    "i": attr.int(default = 3),
    "b": attr.bool(),
    "lbl": attr.label(allow_files = True),
    "lbls": attr.label_list(allow_files = True),
    "tool": attr.label(cfg = "exec", allow_files = True),
    "strs": attr.string_list(),
    "ints": attr.int_list(default = [4]),
    "sd": attr.string_dict(),
    "sld": attr.string_list_dict(),
    "lksd": attr.label_keyed_string_dict(allow_files = True),
    "skld": attr.string_keyed_label_dict(allow_files = True),
    "out": attr.output(),
    "outs": attr.output_list(),
    "_private": attr.label(default = "//other:thing"),
}

kitchen = rule(
    implementation = _impl,
    attrs = _ATTRS,
)

kitchen_bin = rule(
    implementation = _impl,
    executable = True,
    attrs = {"srcs": attr.label_list(allow_files = True)},
)

kitchen_test = rule(
    implementation = _impl,
    test = True,
    attrs = {"srcs": attr.label_list(allow_files = True)},
)
