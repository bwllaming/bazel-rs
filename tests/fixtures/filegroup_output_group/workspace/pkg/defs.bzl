"""Rules whose output groups filegroup(output_group = ...) selects
(rules/filegroup/Filegroup.java:48-146), and consumers of such filegroups."""

def _write(ctx, name, content = None, is_executable = False):
    out = ctx.actions.declare_file(name)
    ctx.actions.write(out, content if content != None else name + "\n", is_executable = is_executable)
    return out

def _groups_impl(ctx):
    name = ctx.label.name
    main = _write(ctx, name + ".main")
    extra = _write(ctx, name + ".extra")
    extra2 = _write(ctx, name + ".extra2")
    own = _write(ctx, name + ".own")
    run = _write(ctx, name + ".run")
    single = _write(ctx, name + ".sh", "#!/bin/sh\necho %s\n" % name, is_executable = True)
    t1 = _write(ctx, name + ".t1")
    t2 = _write(ctx, name + ".t2")
    t3 = _write(ctx, name + ".t3")
    valid = ctx.actions.declare_file(name + ".valid")
    ctx.actions.run_shell(
        outputs = [valid],
        command = "echo valid > " + valid.path,
        mnemonic = "Validate",
    )
    files = [main]
    if ctx.outputs.out:
        ctx.actions.write(ctx.outputs.out, "out\n")
        files.append(ctx.outputs.out)
    shared = ctx.files.shared + [own] if ctx.attr.shared_first else [own] + ctx.files.shared
    return [
        DefaultInfo(files = depset(files), runfiles = ctx.runfiles(files = [run])),
        OutputGroupInfo(
            extra = depset([extra, extra2]),
            listed = [extra2, extra],
            shared = depset(shared),
            # Nested sets of each order.
            topo = depset([t1], transitive = [depset([t2], transitive = [depset([t3], order = "topological")], order = "topological")], order = "topological"),
            pre = depset([t1], transitive = [depset([t2]), depset([t3], order = "preorder")], order = "preorder"),
            post = depset([t1], transitive = [depset([t2, t3], order = "postorder")], order = "postorder"),
            single = depset([single]),
            empty = depset(),
            src = depset(ctx.files.srcs),
            _validation = depset([valid]),
        ),
    ]

# A rule with output groups of every shape.
groups = rule(
    implementation = _groups_impl,
    attrs = {
        "out": attr.output(),
        "shared": attr.label_list(allow_files = True),
        "shared_first": attr.bool(),
        "srcs": attr.label_list(allow_files = True),
    },
)

def _gen_impl(ctx):
    outs = [_write(ctx, name) for name in ctx.attr.outs]
    return [DefaultInfo(files = depset(outs))]

gen = rule(
    implementation = _gen_impl,
    attrs = {"outs": attr.string_list()},
)

def _tool_impl(ctx):
    out = _write(ctx, ctx.label.name, "#!/bin/sh\necho %s\n" % ctx.label.name, is_executable = True)
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

def _lib_impl(ctx):
    # aspect_rules_js's js_library (js/private/js_library.bzl): the sources
    # as files to build, the runfiles' files as the `runfiles` output group
    # (js_run_binary's `<name>_runfiles` filegroup selects it).
    runfiles = ctx.runfiles(files = ctx.files.srcs).merge_all([
        target[DefaultInfo].default_runfiles
        for target in ctx.attr.srcs
    ])
    return [
        DefaultInfo(files = depset(ctx.files.srcs), runfiles = runfiles),
        OutputGroupInfo(runfiles = runfiles.files),
    ]

lib = rule(
    implementation = _lib_impl,
    attrs = {"srcs": attr.label_list(allow_files = True)},
)

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
        "  output_groups=" + repr(sorted([name for name in dir(dep[OutputGroupInfo])])),
    ]

def _consumer_impl(ctx):
    lines = ["ctx.files.deps=" + repr(_paths(ctx.files.deps))]
    for dep in ctx.attr.deps:
        lines.extend(_describe(dep))
    out = ctx.actions.declare_file(ctx.label.name + ".txt")
    ctx.actions.write(out, "\n".join(lines) + "\n")

    # The dependencies' files to build as action inputs (aspect_bazel_lib's
    # run_binary `srcs`): each one's contents.
    arguments = []
    for f in ctx.files.deps:
        arguments.extend([f.short_path, f.path])
    contents = ctx.actions.declare_file(ctx.label.name + ".contents")
    ctx.actions.run_shell(
        inputs = ctx.files.deps,
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
