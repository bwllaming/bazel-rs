"""Actions.canBeShared between configured targets (MapBasedActionGraph)."""

def _tool_impl(ctx):
    executable = ctx.actions.declare_file(ctx.label.name + ".sh")
    ctx.actions.write(
        executable,
        "\n".join([
            "#!/bin/sh",
            "set -e",
            "out=\"$1\"",
            "shift",
            ": > \"$out\"",
            "for arg in \"$@\"; do",
            "  case \"$arg\" in",
            "    @*) cat \"${arg#@}\" >> \"$out\" ;;",
            "    *) echo \"$arg\" >> \"$out\" ;;",
            "  esac",
            "done",
            "echo \"PYTHONHASHSEED=$PYTHONHASHSEED X=${X-unset}\" >> \"$out\"",
            "cat \"$0.runfiles/_main/pkg/" + ctx.label.name + ".data\" >> \"$out\"",
            "",
        ]),
        is_executable = True,
    )
    data = ctx.actions.declare_file(ctx.label.name + ".data")
    ctx.actions.write(data, "runfile of " + ctx.label.name + "\n")
    return [DefaultInfo(executable = executable, runfiles = ctx.runfiles(files = [data]))]

# An executable with runfiles: its runfiles tree is a mandatory input of
# every action that runs it.
tool = rule(implementation = _tool_impl, executable = True)

def _compile_impl(ctx):
    outs = []
    for src in ctx.files.srcs:
        # rules_python's precompile: a sibling of the source, only for
        # sources of the target's own package.
        if src.owner.package != ctx.label.package:
            continue
        out = ctx.actions.declare_file(
            "__pycache__/" + src.basename[:-len(".py")] + ".tag.pyc",
            sibling = src,
        )
        args = ctx.actions.args()
        args.use_param_file("@%s", use_always = True)
        args.set_param_file_format("multiline")
        args.add("--src", src)
        args.add("--src_name", src.short_path)
        args.add("--pyc", out)
        args.add("--mode", ctx.attr.mode)
        ctx.actions.run(
            executable = ctx.executable.tool,
            arguments = [out.path, args],
            inputs = [src] + ctx.files.extra_inputs,
            outputs = [out],
            mnemonic = ctx.attr.mnemonic,
            progress_message = "Compiling %{output} for %{label}",
            env = {"PYTHONHASHSEED": "0"} | ctx.attr.env,
            execution_requirements = {"no-remote-exec": "1"},
        )
        outs.append(out)
    return [DefaultInfo(files = depset(outs))]

# Like rules_python's py_library precompiling its srcs: two targets listing
# one source declare the same output and register the same action.
compile = rule(
    implementation = _compile_impl,
    attrs = {
        "srcs": attr.label_list(allow_files = [".py"]),
        "tool": attr.label(executable = True, cfg = "exec", default = ":tool"),
        "mode": attr.string(default = "same"),
        "mnemonic": attr.string(default = "PyCompile"),
        "env": attr.string_dict(),
        "extra_inputs": attr.label_list(allow_files = True),
    },
)

def _consumer_impl(ctx):
    files = []
    lines = []
    for dep in ctx.attr.deps:
        for file in dep[DefaultInfo].files.to_list():
            files.append(file)
            lines.append("%s: path=%s short_path=%s owner=%s is_source=%s" % (
                dep.label,
                file.path,
                file.short_path,
                file.owner,
                file.is_source,
            ))
    shared = [file for file in files if file.basename == "__init__.tag.pyc"]
    lines.append("shared=%d equal=%s depset=%d" % (
        len(shared),
        len(shared) == 2 and shared[0] == shared[1],
        len(depset(shared).to_list()),
    ))
    report = ctx.actions.declare_file(ctx.label.name + ".report")
    ctx.actions.write(report, "\n".join(lines) + "\n")
    out = ctx.actions.declare_file(ctx.label.name + ".cat")
    ctx.actions.run_shell(
        inputs = files,
        outputs = [out],
        command = "cat \"$@\" > " + out.path,
        arguments = [file.path for file in files],
        mnemonic = "Cat",
    )
    return [DefaultInfo(files = depset([report, out]))]

# Consumes both targets' Files of one shared output.
consumer = rule(
    implementation = _consumer_impl,
    attrs = {"deps": attr.label_list()},
)

def _writer_impl(ctx):
    out = ctx.outputs.out if ctx.attr.out else ctx.actions.declare_file(ctx.attr.path)
    ctx.actions.write(out, ctx.attr.content)
    return [DefaultInfo(files = depset([out]))]

# A write to a declared file or to a predeclared output (an output-file
# target).
writer = rule(
    implementation = _writer_impl,
    attrs = {
        "out": attr.output(),
        "path": attr.string(),
        "content": attr.string(default = "shared write\n"),
    },
)

def _owners_impl(ctx):
    lines = ["%s: path=%s owner=%s" % (ctx.label, file.path, file.owner) for file in ctx.files.srcs]
    report = ctx.actions.declare_file(ctx.label.name + ".report")
    ctx.actions.write(report, "\n".join(lines) + "\n")
    return [DefaultInfo(files = depset([report]))]

# Reports the Files a label list resolves to.
owners = rule(
    implementation = _owners_impl,
    attrs = {"srcs": attr.label_list(allow_files = True)},
)

def _grouped_impl(ctx):
    out = ctx.actions.declare_file("grouped.out")
    ctx.actions.run_shell(
        outputs = [out],
        command = "echo grouped > " + out.path,
        mnemonic = "Grouped",
        exec_group = ctx.attr.group,
    )
    return [DefaultInfo(files = depset([out]))]

# One action in one of two exec groups resolving to the same execution
# platform.
grouped = rule(
    implementation = _grouped_impl,
    attrs = {"group": attr.string()},
    exec_groups = {
        "g1": exec_group(),
        "g2": exec_group(),
    },
)

def _plain_impl(ctx):
    return []

# An action-free target an aspect applies to; `content` and `out_name` are
# what the aspect writes where.
plain = rule(
    implementation = _plain_impl,
    attrs = {
        "content": attr.string(default = "written by an aspect\n"),
        "out_name": attr.string(default = "aspect_shared.txt"),
    },
)

AspectOutInfo = provider(fields = ["files"])

def _shared_aspect_impl(target, ctx):
    out = ctx.actions.declare_file(ctx.rule.attr.out_name)
    ctx.actions.run_shell(
        outputs = [out],
        command = "printf '%s' \"$1\" > " + out.path,
        arguments = [ctx.rule.attr.content],
        mnemonic = "AspectWrite",
        progress_message = "Aspect writing %{output} for %{label}",
    )
    return [AspectOutInfo(files = depset([out]))]

# One spawn per application, on a path below the base target's package that
# two applications may share.
shared_aspect = aspect(implementation = _shared_aspect_impl)

def _aspect_consumer_impl(ctx):
    files = []
    for dep in ctx.attr.deps:
        files.extend(dep[AspectOutInfo].files.to_list())
    lines = ["%s: path=%s owner=%s" % (ctx.label, file.path, file.owner) for file in files]
    lines.append("files=%d depset=%d" % (len(files), len(depset(files).to_list())))
    report = ctx.actions.declare_file(ctx.label.name + ".report")
    ctx.actions.write(report, "\n".join(lines) + "\n")
    out = ctx.actions.declare_file(ctx.label.name + ".cat")
    ctx.actions.run_shell(
        inputs = files,
        outputs = [out],
        command = "cat \"$@\" > " + out.path,
        arguments = [file.path for file in files],
        mnemonic = "Cat",
    )
    return [DefaultInfo(files = depset([report, out]))]

# Applies shared_aspect to its deps and consumes every application's File.
aspect_consumer = rule(
    implementation = _aspect_consumer_impl,
    attrs = {"deps": attr.label_list(aspects = [shared_aspect])},
)

def _tree_impl(ctx):
    out = ctx.actions.declare_directory("tree_shared")
    ctx.actions.run_shell(
        outputs = [out],
        command = "mkdir -p {d} && echo one > {d}/one.txt && echo two > {d}/two.txt".format(d = out.path),
        mnemonic = "Tree",
    )
    return [DefaultInfo(files = depset([out]))]

# Two targets declaring one tree artifact with one action.
tree = rule(implementation = _tree_impl)

def _tree_consumer_impl(ctx):
    trees = []
    for dep in ctx.attr.deps:
        trees.extend(dep[DefaultInfo].files.to_list())
    lines = ["%s: path=%s owner=%s is_directory=%s" % (ctx.label, tree.path, tree.owner, tree.is_directory) for tree in trees]
    lines.append("trees=%d depset=%d" % (len(trees), len(depset(trees).to_list())))
    report = ctx.actions.declare_file(ctx.label.name + ".report")
    ctx.actions.write(report, "\n".join(lines) + "\n")
    out = ctx.actions.declare_file(ctx.label.name + ".cat")
    ctx.actions.run_shell(
        inputs = trees,
        outputs = [out],
        command = "for tree in \"$@\"; do cat \"$tree/one.txt\" \"$tree/two.txt\"; done > " + out.path,
        arguments = [tree.path for tree in trees],
        mnemonic = "CatTrees",
    )
    return [DefaultInfo(files = depset([report, out]))]

# Consumes both targets' Files of one shared tree artifact.
tree_consumer = rule(
    implementation = _tree_consumer_impl,
    attrs = {"deps": attr.label_list()},
)

def _runner_impl(ctx):
    # The last dependency's Files are runfiles (with //pkg:a analyzed first,
    # //pkg:b's precompiled __init__ is the shared action's second Artifact).
    files = ctx.attr.deps[-1][DefaultInfo].files.to_list()
    executable = ctx.actions.declare_file(ctx.label.name + ".sh")
    ctx.actions.write(
        executable,
        "#!/bin/sh\nset -e\n" + "".join(["echo '== %s'\ncat '%s'\n" % (file.short_path, file.short_path) for file in files]),
        is_executable = True,
    )
    return [DefaultInfo(executable = executable, runfiles = ctx.runfiles(files = files))]

# Runs with the last dependency's Files as runfiles (read from the runfiles
# tree, the working directory of `run`).
runner = rule(
    implementation = _runner_impl,
    executable = True,
    attrs = {"deps": attr.label_list()},
)

def _expand_impl(ctx):
    tree = ctx.actions.declare_directory("expand_tree")
    ctx.actions.run_shell(
        outputs = [tree],
        command = "mkdir -p {d} && echo x > {d}/x.txt".format(d = tree.path),
        mnemonic = "ExpandTree",
    )
    args = ctx.actions.args()
    args.add_all([tree])
    out = ctx.actions.declare_file("expand.args")
    ctx.actions.write(out, args)
    return [DefaultInfo(files = depset([out]))]

# Two targets writing Args content that expands a (shared) tree artifact.
expand = rule(implementation = _expand_impl)
