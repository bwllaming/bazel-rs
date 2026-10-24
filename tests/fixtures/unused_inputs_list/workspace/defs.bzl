def _input_list_impl(ctx):
    # The aspect_bazel_lib tar.bzl shape: a separate action writes the list,
    # which is an input of the pruned action.
    unused = ctx.actions.declare_file(ctx.label.name + ".unused")
    ctx.actions.write(unused, ctx.file.b.path + "\n")
    out = ctx.actions.declare_file(ctx.label.name + ".out")
    ctx.actions.run(
        executable = "/bin/cp",
        arguments = [ctx.file.a.path, out.path],
        inputs = depset([ctx.file.a, ctx.file.b, unused]),
        outputs = [out],
        unused_inputs_list = unused,
        mnemonic = "Pruned",
    )
    return [DefaultInfo(files = depset([out]))]

input_list = rule(
    implementation = _input_list_impl,
    attrs = {
        "a": attr.label(allow_single_file = True),
        "b": attr.label(allow_single_file = True),
    },
)

def _output_list_impl(ctx):
    # The action writes its own list (one of its outputs).
    unused = ctx.actions.declare_file(ctx.label.name + ".unused")
    out = ctx.actions.declare_file(ctx.label.name + ".out")
    ctx.actions.run(
        executable = "/bin/sh",
        arguments = ["-c", "cp \"$1\" \"$2\" && echo \"$3\" > \"$4\"", "sh", ctx.file.a.path, out.path, ctx.file.b.path, unused.path],
        inputs = [ctx.file.a, ctx.file.b],
        outputs = [out, unused],
        unused_inputs_list = unused,
        mnemonic = "SelfPruned",
    )
    return [DefaultInfo(files = depset([out]))]

output_list = rule(
    implementation = _output_list_impl,
    attrs = {
        "a": attr.label(allow_single_file = True),
        "b": attr.label(allow_single_file = True),
    },
)

def _none_impl(ctx):
    out = ctx.actions.declare_file(ctx.label.name + ".out")
    ctx.actions.run(
        executable = "/bin/cp",
        arguments = [ctx.file.a.path, out.path],
        inputs = [ctx.file.a],
        outputs = [out],
        unused_inputs_list = None,
    )
    return [DefaultInfo(files = depset([out]))]

none_list = rule(implementation = _none_impl, attrs = {"a": attr.label(allow_single_file = True)})

def _bad_impl(ctx):
    out = ctx.actions.declare_file(ctx.label.name + ".out")
    ctx.actions.run(
        executable = "/bin/cp",
        arguments = [ctx.file.a.path, out.path],
        inputs = [ctx.file.a],
        outputs = [out],
        unused_inputs_list = ctx.attr.value,
    )
    return [DefaultInfo(files = depset([out]))]

bad_list = rule(implementation = _bad_impl, attrs = {"a": attr.label(allow_single_file = True), "value": attr.string()})
