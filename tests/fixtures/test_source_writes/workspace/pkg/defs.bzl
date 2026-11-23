_SCRIPT = """#!/bin/sh
# The runfile of a source file resolves to the live workspace file (genuine's
# runfiles and sandbox inputs are symlinks to it); write next to it like
# Python's bytecode cache does (conftest.py: Path(__file__).resolve()).
runfile="$TEST_SRCDIR/_main/pkg/mod.py"
dir=$(dirname "$(readlink -f "$runfile")")
case "$dir" in "$TEST_SRCDIR"*) echo "resolved=runfiles" ;; *) echo "resolved=workspace" ;; esac
/usr/bin/python3 -c "import sys; sys.path.insert(0, sys.argv[1]); import mod" "$dir"
if [ -d "$dir/__pycache__" ]; then echo "pycache=written"; else echo "pycache=absent"; fi
if mkdir "$dir/probe_dir" 2>/dev/null; then echo "mkdir=ok"; else echo "mkdir=denied"; fi
if touch "$dir/mod.py" 2>/dev/null && echo >> "$dir/mod.py" 2>/dev/null; then echo "append=ok"; else echo "append=denied"; fi
exit 0
"""

def _probe_test_impl(ctx):
    exe = ctx.actions.declare_file(ctx.label.name + ".sh")
    ctx.actions.write(exe, _SCRIPT, is_executable = True)
    return [DefaultInfo(executable = exe, runfiles = ctx.runfiles(files = ctx.files.data))]

probe_test = rule(
    implementation = _probe_test_impl,
    test = True,
    attrs = {"data": attr.label_list(allow_files = True)},
)
