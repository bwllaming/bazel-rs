load("@asp//:defs.bzl", "lint_aspect")

def _bin(ctx):
    exe = ctx.actions.declare_file(ctx.label.name)
    ctx.actions.write(exe, "#!/bin/sh\necho %s\n" % ctx.attr.flag, is_executable = True)
    return [DefaultInfo(executable = exe, runfiles = ctx.runfiles(files = ctx.files.data))]

my_bin = rule(_bin, executable = True, attrs = {
    "data": attr.label_list(allow_files = True),
    "flag": attr.string(),
})

def _node(ctx):
    return []

node = rule(_node, attrs = {"deps": attr.label_list()})

def _asp_bin(ctx):
    exe = ctx.actions.declare_file(ctx.label.name)
    ctx.actions.write(exe, "#!/bin/sh\n", is_executable = True)
    return [DefaultInfo(executable = exe, runfiles = ctx.runfiles(files = ctx.files.data))]

asp_bin = rule(_asp_bin, executable = True, attrs = {
    "data": attr.label_list(allow_files = True),
    "deps": attr.label_list(aspects = [lint_aspect]),
})
