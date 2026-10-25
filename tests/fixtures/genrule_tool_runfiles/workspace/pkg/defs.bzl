"""A js_binary-like launcher: an executable with runfiles support."""

_LAUNCHER = """#!/bin/bash
set -eu
if [[ -n "${RUNFILES_DIR:-}" ]]; then
  root="$RUNFILES_DIR"
  how=RUNFILES_DIR
elif [[ -d "$0.runfiles" ]]; then
  root="$0.runfiles"
  how=dollar0
else
  echo "FATAL: launcher: RUNFILES environment variable is not set" >&2
  exit 1
fi
echo "$how: $(cat "$root/_main/pkg/data.txt")"
"""

def _launcher_impl(ctx):
    executable = ctx.actions.declare_file(ctx.label.name)
    ctx.actions.write(executable, _LAUNCHER, is_executable = True)
    return [DefaultInfo(
        executable = executable,
        runfiles = ctx.runfiles(files = ctx.files.data),
    )]

launcher = rule(
    implementation = _launcher_impl,
    attrs = {"data": attr.label_list(allow_files = True)},
    executable = True,
)
