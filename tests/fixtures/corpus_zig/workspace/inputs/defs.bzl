# B57: the order of a Starlark spawn's inputs (`Target.actions`, aquery's
# primary input, print_action): SpawnAction.Builder's inputs NestedSet is
# the declared inputs, then the tools (their transitive sets, then the
# direct Files: a File executable without runfiles, the File tools).

def _tool_impl(ctx):
    out = ctx.actions.declare_file(ctx.label.name + ".sh")
    ctx.actions.write(out, "#!/bin/sh\ntouch \"$1\"\n", is_executable = True)
    return [DefaultInfo(executable = out, runfiles = ctx.runfiles(files = ctx.files.data))]

tool = rule(
    implementation = _tool_impl,
    executable = True,
    attrs = {"data": attr.label_list(allow_files = True)},
)

def _subject_impl(ctx):
    a, b, c, d = ctx.files.srcs
    script = ctx.file.script
    tool_exe = ctx.executable.tool
    other = ctx.attr.other[DefaultInfo].files_to_run
    outs = []

    def out(name):
        file = ctx.actions.declare_file(ctx.label.name + "." + name)
        outs.append(file)
        return file

    # A plain File executable (no runfiles): a direct tool, after the inputs.
    o = out("file_exe")
    ctx.actions.run(
        executable = script,
        inputs = [a, b],
        tools = [d],
        outputs = [o],
        arguments = [o.path],
        mnemonic = "FileExe",
        progress_message = "IN %{input}",
    )

    # An executable attribute's File (its FilesToRunProvider: transitive),
    # depset inputs, a depset, a File and a FilesToRunProvider as tools.
    o = out("provider_exe")
    ctx.actions.run(
        executable = tool_exe,
        inputs = depset([b, a]),
        tools = [depset([c]), d, other],
        outputs = [o],
        arguments = [o.path],
        mnemonic = "ProviderExe",
        progress_message = "IN %{input}",
    )

    # A FilesToRunProvider executable, no inputs.
    o = out("files_to_run_exe")
    ctx.actions.run(
        executable = other,
        tools = [script],
        outputs = [o],
        arguments = [o.path],
        mnemonic = "FilesToRunExe",
        progress_message = "IN %{input}",
    )

    # run_shell: File tools before and after an executable tool.
    o = out("shell")
    ctx.actions.run_shell(
        command = "touch $1",
        inputs = [b, a],
        tools = [d, tool_exe, c],
        outputs = [o],
        arguments = [o.path],
        mnemonic = "Shell",
        progress_message = "IN %{input}",
    )

    # The executable listed among the inputs too; a tool listed twice.
    o = out("duplicates")
    ctx.actions.run(
        executable = script,
        inputs = [script, a],
        tools = [a, c, c],
        outputs = [o],
        arguments = [o.path],
        mnemonic = "Duplicates",
        progress_message = "IN %{input}",
    )

    # No inputs at all: the executable is the primary input.
    o = out("bare")
    ctx.actions.run(
        executable = script,
        outputs = [o],
        arguments = [o.path],
        mnemonic = "Bare",
        progress_message = "IN %{input}",
    )
    return [DefaultInfo(files = depset(outs)), OutputGroupInfo(compilation_outputs = depset(outs))]

subject = rule(
    implementation = _subject_impl,
    attrs = {
        "srcs": attr.label_list(allow_files = True),
        "script": attr.label(allow_single_file = True),
        "tool": attr.label(executable = True, cfg = "exec"),
        "other": attr.label(executable = True, cfg = "exec"),
    },
)

def _check_impl(ctx):
    lines = []
    for action in ctx.attr.target.actions:
        lines.append("%s %s" % (action.mnemonic, [f.path for f in action.inputs.to_list()]))
    return [AnalysisTestResultInfo(success = True, message = "\n".join(lines))]

def check(name, target):
    testing.analysis_test(
        name = name,
        implementation = _check_impl,
        attrs = {"target": attr.label()},
        attr_values = {"target": target},
    )
