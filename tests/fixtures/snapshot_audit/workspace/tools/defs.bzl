load(":helpers.bzl", "suffix")

def text_files(name):
    native.filegroup(name = name, srcs = native.glob(["*" + suffix()], exclude = ["link.txt"]))

def _concat_impl(ctx):
    out = ctx.actions.declare_file(ctx.attr.out)
    ctx.actions.run_shell(
        inputs = ctx.files.srcs,
        outputs = [out],
        command = "cat " + " ".join([f.path for f in ctx.files.srcs]) + " > " + out.path,
    )
    return [DefaultInfo(files = depset([out]))]

concat = rule(
    implementation = _concat_impl,
    attrs = {
        "srcs": attr.label_list(allow_files = True),
        "out": attr.string(),
    },
)
