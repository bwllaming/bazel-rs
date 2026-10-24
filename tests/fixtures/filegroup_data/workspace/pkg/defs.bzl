"""Consumers of filegroups with `data` (rules/filegroup/Filegroup.java)."""

def _gen_impl(ctx):
    outs = [ctx.actions.declare_file(name) for name in ctx.attr.outs]
    for out in outs:
        ctx.actions.write(out, out.basename + "\n")
    return [DefaultInfo(files = depset(outs))]

gen = rule(
    implementation = _gen_impl,
    attrs = {"outs": attr.string_list()},
)

def _tool_impl(ctx):
    out = ctx.actions.declare_file(ctx.label.name)
    ctx.actions.write(out, "#!/bin/sh\necho %s\n" % ctx.label.name, is_executable = True)
    return [DefaultInfo(
        executable = out,
        runfiles = ctx.runfiles(files = ctx.files.data),
    )]

# An executable with runfiles.
tool = rule(
    implementation = _tool_impl,
    executable = True,
    attrs = {"data": attr.label_list(allow_files = True)},
)

def _groups_impl(ctx):
    main = ctx.actions.declare_file(ctx.label.name + ".main")
    extra = ctx.actions.declare_file(ctx.label.name + ".extra")
    ctx.actions.write(main, "main\n")
    ctx.actions.write(extra, "extra\n")
    return [
        DefaultInfo(files = depset([main])),
        OutputGroupInfo(extra = depset([extra])),
    ]

# A rule with an output group.
groups = rule(implementation = _groups_impl)

def _paths(files):
    return [f.short_path for f in files]

def _describe(dep):
    info = dep[DefaultInfo]
    executable = info.files_to_run.executable
    return [
        "label=" + str(dep.label),
        "  files=" + repr(_paths(info.files.to_list())),
        "  default_runfiles=" + repr(_paths(info.default_runfiles.files.to_list())),
        "  data_runfiles=" + repr(_paths(info.data_runfiles.files.to_list())),
        "  executable=" + repr(executable.short_path if executable else None),
    ]

def _consumer_impl(ctx):
    lines = ["ctx.files.deps=" + repr(_paths(ctx.files.deps))]
    for dep in ctx.attr.deps:
        lines.extend(_describe(dep))
    out = ctx.actions.declare_file(ctx.label.name + ".txt")
    ctx.actions.write(out, "\n".join(lines) + "\n")

    # The dependencies' default runfiles as action inputs: each one's
    # contents (generated data files are built for it).
    inputs = depset(transitive = [dep[DefaultInfo].default_runfiles.files for dep in ctx.attr.deps])
    arguments = []
    for f in inputs.to_list():
        arguments.extend([f.short_path, f.path])
    contents = ctx.actions.declare_file(ctx.label.name + ".contents")
    ctx.actions.run_shell(
        inputs = inputs,
        outputs = [contents],
        command = "while [ $# -gt 0 ]; do echo \"$1: $(cat \"$2\")\"; shift 2; done > " + contents.path,
        arguments = arguments,
        mnemonic = "Contents",
    )
    return [DefaultInfo(files = depset([out, contents]))]

consumer = rule(
    implementation = _consumer_impl,
    attrs = {"deps": attr.label_list(allow_files = True)},
)

def _exec_consumer_impl(ctx):
    # rules_rust's rust_toolchain reads its exec `linker` filegroup like this
    # (rust/private/toolchain.bzl: files and default_runfiles).
    info = ctx.attr.tool[DefaultInfo]
    lines = [
        "files=" + repr([f.path for f in info.files.to_list()]),
        "default_runfiles=" + repr([f.path for f in info.default_runfiles.files.to_list()]),
    ]
    out = ctx.actions.declare_file(ctx.label.name + ".txt")
    ctx.actions.write(out, "\n".join(lines) + "\n")
    return [DefaultInfo(files = depset([out]))]

exec_consumer = rule(
    implementation = _exec_consumer_impl,
    attrs = {"tool": attr.label(cfg = "exec", allow_files = True)},
)

def _runner_impl(ctx):
    # The usual runfiles of a binary: its data files and their default
    # runfiles.
    out = ctx.actions.declare_file(ctx.label.name)
    ctx.actions.write(out, "#!/bin/sh\nfind . -mindepth 1 | LC_ALL=C sort\n", is_executable = True)
    runfiles = ctx.runfiles(files = ctx.files.data)
    runfiles = runfiles.merge_all([dep[DefaultInfo].default_runfiles for dep in ctx.attr.data])
    return [DefaultInfo(executable = out, runfiles = runfiles)]

# `run` lists its runfiles tree.
runner = rule(
    implementation = _runner_impl,
    executable = True,
    attrs = {"data": attr.label_list(allow_files = True)},
)

LabelsInfo = provider(fields = ["labels"])

def _labels_aspect_impl(target, ctx):
    labels = [str(target.label)]
    for name in ["srcs", "data"]:
        for dep in getattr(ctx.rule.attr, name, []):
            if LabelsInfo in dep:
                labels.extend(dep[LabelsInfo].labels)
    return [LabelsInfo(labels = labels)]

labels_aspect = aspect(
    implementation = _labels_aspect_impl,
    attr_aspects = ["srcs", "data"],
)

def _aspect_probe_impl(ctx):
    out = ctx.actions.declare_file(ctx.label.name + ".txt")
    ctx.actions.write(out, "labels=" + repr(ctx.attr.target[LabelsInfo].labels) + "\n")
    return [DefaultInfo(files = depset([out]))]

aspect_probe = rule(
    implementation = _aspect_probe_impl,
    attrs = {"target": attr.label(aspects = [labels_aspect])},
)
