_PROBE = """#!/bin/bash
cd "$1"
t=$(ls -d */bin/tool | head -1)
if [ -L "$t" ]; then echo "{kind}_is_symlink=yes"; else echo "{kind}_is_symlink=no"; fi
echo "{kind}_ran=$("./$t" 2>&1 || echo "exit $?")"
"""

def _wrapper_test(ctx):
    exe = ctx.actions.declare_file(ctx.label.name + ".sh")
    ctx.actions.write(exe, _PROBE.format(kind = "test_runfile").replace('cd "$1"', 'cd "$TEST_SRCDIR"'), is_executable = True)
    return [DefaultInfo(executable = exe, runfiles = ctx.runfiles(files = [ctx.file.tool]))]

wrapper_test = rule(
    implementation = _wrapper_test,
    test = True,
    attrs = {"tool": attr.label(allow_single_file = True)},
)

def _runner(ctx):
    exe = ctx.actions.declare_file(ctx.label.name + ".sh")
    ctx.actions.write(exe, _PROBE.format(kind = "tool_runfile").replace('cd "$1"', 'cd "$0.runfiles"'), is_executable = True)
    return [DefaultInfo(executable = exe, runfiles = ctx.runfiles(files = [ctx.file.tool]))]

runner = rule(
    implementation = _runner,
    executable = True,
    attrs = {"tool": attr.label(allow_single_file = True)},
)
