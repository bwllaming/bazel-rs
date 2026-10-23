"""Test rules, a library and a consumer of test_suite targets."""

def _fake_test_impl(ctx):
    out = ctx.actions.declare_file(ctx.label.name + ".sh")
    ctx.actions.write(out, "#!/bin/sh\necho %s\n" % ctx.label.name, is_executable = True)
    return [DefaultInfo(executable = out, runfiles = ctx.runfiles(files = ctx.files.data))]

fake_test = rule(
    implementation = _fake_test_impl,
    test = True,
    attrs = {"data": attr.label_list(allow_files = True)},
)

def _lib_impl(ctx):
    out = ctx.actions.declare_file(ctx.label.name + ".txt")
    ctx.actions.write(out, ctx.label.name + "\n")
    return [DefaultInfo(files = depset([out]))]

lib = rule(implementation = _lib_impl)

def _paths(files):
    return [f.short_path for f in files]

def _consumer_impl(ctx):
    lines = ["ctx.files.deps=" + repr(_paths(ctx.files.deps))]
    for dep in ctx.attr.deps:
        info = dep[DefaultInfo]
        lines.extend([
            "label=" + str(dep.label),
            "  files=" + repr(_paths(info.files.to_list())),
            "  default_runfiles=" + repr(_paths(info.default_runfiles.files.to_list())),
            "  data_runfiles=" + repr(_paths(info.data_runfiles.files.to_list())),
            "  executable=" + repr(info.files_to_run.executable),
            "  output_groups=" + repr(OutputGroupInfo in dep),
        ])
    out = ctx.actions.declare_file(ctx.label.name + ".txt")
    ctx.actions.write(out, "\n".join(lines) + "\n")
    return [DefaultInfo(files = depset([out]))]

consumer = rule(
    implementation = _consumer_impl,
    attrs = {"deps": attr.label_list()},
)
