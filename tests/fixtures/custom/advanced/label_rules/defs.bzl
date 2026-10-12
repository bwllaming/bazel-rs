load("//advanced/label_helper:defs.bzl", "DEFINITION_LABEL", "source_attribute")

def _capture_impl(ctx):
    out = ctx.actions.declare_file(ctx.label.name + ".txt")
    ctx.actions.write(out, ctx.file.src.short_path + "|" + ctx.file.explicit.short_path + "\n")
    return [DefaultInfo(files = depset([out]))]

capture = rule(implementation = _capture_impl, attrs = {
    "src": source_attribute(),
    "explicit": attr.label(default = DEFINITION_LABEL, allow_single_file = True),
})

def capture_via_macro(name, src):
    capture(name = name, src = src)
