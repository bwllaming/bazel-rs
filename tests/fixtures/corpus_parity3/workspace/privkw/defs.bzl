# rules_android's attrs_metadata (android_library/rule.bzl:203-247): a
# macro sets private attributes by their native names (`$x` for `_x`).
def _outputs(name, _defined):
    outputs = {"lib": "lib%{name}.txt"}
    if _defined:
        outputs["extra"] = "%{name}_extra.txt"
    return outputs

def _impl(ctx):
    lines = [repr(ctx.attr._defined), repr(ctx.attr._pkg), repr(ctx.attr._other)]
    for out in [ctx.outputs.lib] + ([ctx.outputs.extra] if ctx.attr._defined else []):
        ctx.actions.write(out, "\n".join(lines) + "\n")

lib = rule(
    implementation = _impl,
    outputs = _outputs,
    attrs = {
        "_defined": attr.bool(),
        "_pkg": attr.string(default = "none"),
        "_other": attr.string(default = "default"),
    },
)

def lib_macro(**attrs):
    attrs["$defined"] = bool(attrs.pop("resources", None))
    attrs["$pkg"] = native.package_name()
    lib(**attrs)
