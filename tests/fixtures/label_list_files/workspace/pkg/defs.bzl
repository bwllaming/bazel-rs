"""ctx.files of label-list attributes: one File per artifact, in order
(StarlarkAttributesCollection over PrerequisiteArtifacts.nestedSet, a
stable-order NestedSet of the prerequisites' files to build)."""

def _gen_impl(ctx):
    outs = [ctx.actions.declare_file(name) for name in ctx.attr.outs]
    for out in outs:
        ctx.actions.write(out, out.basename + "\n")
    return [DefaultInfo(files = depset(outs))]

gen = rule(
    implementation = _gen_impl,
    attrs = {"outs": attr.string_list()},
)

def _paths(files):
    return [f.short_path for f in files]

def _probe_impl(ctx):
    lines = [
        "deps=" + repr(_paths(ctx.files.deps)),
        "deps_targets=" + repr([str(dep.label) for dep in ctx.attr.deps]),
        "srcs=" + repr(_paths(ctx.files.srcs)),
        "dict=" + repr(_paths(ctx.files.dict)),
        "one=" + repr(_paths(ctx.files.one)),
    ]
    out = ctx.actions.declare_file(ctx.label.name + ".txt")
    ctx.actions.write(out, "\n".join(lines) + "\n")

    # The files as action inputs and arguments.
    listed = ctx.actions.declare_file(ctx.label.name + ".args")
    ctx.actions.run_shell(
        inputs = ctx.files.deps,
        outputs = [listed],
        command = "printf '%s\\n' \"$@\" > " + listed.path,
        arguments = [f.short_path for f in ctx.files.deps],
        mnemonic = "ListFiles",
    )
    return [DefaultInfo(files = depset([out, listed]))]

probe = rule(
    implementation = _probe_impl,
    attrs = {
        "deps": attr.label_list(allow_files = True),
        "srcs": attr.label_list(allow_files = True),
        "dict": attr.label_keyed_string_dict(allow_files = True),
        "one": attr.label(allow_files = True),
    },
)

FilesInfo = provider(fields = ["deps"])

def _files_aspect_impl(target, ctx):
    return [FilesInfo(deps = _paths(ctx.rule.files.deps))]

files_aspect = aspect(implementation = _files_aspect_impl)

def _aspect_probe_impl(ctx):
    out = ctx.actions.declare_file(ctx.label.name + ".txt")
    ctx.actions.write(out, "rule.files.deps=" + repr(ctx.attr.target[FilesInfo].deps) + "\n")
    return [DefaultInfo(files = depset([out]))]

aspect_probe = rule(
    implementation = _aspect_probe_impl,
    attrs = {"target": attr.label(aspects = [files_aspect])},
)
