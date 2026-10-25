"""Rules consuming source directories: action inputs, runfiles and tools."""

# The listing every rule prints: entries as `find -L` sees them (links
# followed), with the executable bit and the contents of each file.
_LIST = r'''
list() {
  echo "== $1"
  if [ ! -d "$1" ]; then echo "(file)"; return; fi
  (cd "$1" && find -L . -mindepth 1 2>/dev/null | LC_ALL=C sort | while IFS= read -r p; do
    if [ -d "$p" ]; then echo "d $p";
    elif [ -f "$p" ]; then
      if [ -x "$p" ]; then x=x; else x=-; fi
      echo "f$x $p: $(tr '\n' '|' < "$p")"
    else echo "? $p"; fi
  done)
}
'''

def _probe_impl(ctx):
    out = ctx.actions.declare_file(ctx.label.name + ".txt")
    lines = []
    for f in ctx.files.srcs:
        lines.append("path=%s short_path=%s is_directory=%s is_source=%s is_symlink=%s basename=%s extension=%r dirname=%s owner=%s root=%r" % (
            f.path,
            f.short_path,
            f.is_directory,
            f.is_source,
            f.is_symlink,
            f.basename,
            f.extension,
            f.dirname,
            f.owner,
            f.root.path,
        ))
    ctx.actions.write(out, "\n".join(lines) + "\n")
    return [DefaultInfo(files = depset([out]))]

probe = rule(
    implementation = _probe_impl,
    attrs = {"srcs": attr.label_list(allow_files = True)},
)

def _list_impl(ctx):
    out = ctx.actions.declare_file(ctx.label.name + ".txt")

    # A fresh token per execution: whether a later build ran the action
    # again.
    stamp = ctx.actions.declare_file(ctx.label.name + ".stamp")
    ctx.actions.run_shell(
        inputs = ctx.files.srcs,
        outputs = [out, stamp],
        command = _LIST + '''
set -eu
out="$1"; stamp="$2"; shift 2
for d in "$@"; do list "$d"; done > "$out"
cat /proc/sys/kernel/random/uuid > "$stamp"
''',
        arguments = [out.path, stamp.path] + [f.path for f in ctx.files.srcs],
        mnemonic = "ListDir",
        execution_requirements = ctx.attr.reqs,
    )
    return [DefaultInfo(files = depset([out, stamp]))]

list_dir = rule(
    implementation = _list_impl,
    attrs = {
        "reqs": attr.string_dict(),
        "srcs": attr.label_list(allow_files = True),
    },
)

def _view_impl(ctx):
    out = ctx.actions.declare_file(ctx.label.name + ".txt")
    ctx.actions.run_shell(
        inputs = ctx.files.srcs,
        outputs = [out],
        command = r'''
set -eu
out="$1"; shift
for d in "$@"; do
  echo "== $d"
  if [ -L "$d" ]; then echo "root: symlink"; elif [ -d "$d" ]; then echo "root: directory"; fi
  (cd "$d" && find . -mindepth 1 | LC_ALL=C sort | while IFS= read -r p; do
    if [ -L "$p" ]; then echo "l $p -> $(readlink "$p")";
    elif [ -d "$p" ]; then echo "d $p"; else echo "f $p"; fi
  done)
done > "$out"
''',
        arguments = [out.path] + [f.path for f in ctx.files.srcs],
        mnemonic = "ViewDir",
        execution_requirements = ctx.attr.reqs,
    )
    return [DefaultInfo(files = depset([out]))]

view = rule(
    implementation = _view_impl,
    attrs = {
        "reqs": attr.string_dict(),
        "srcs": attr.label_list(allow_files = True),
    },
)

def _lister_impl(ctx):
    exe = ctx.actions.declare_file(ctx.label.name + ".sh")
    if ctx.attr.expected:
        # A test: the listing of its runfiles' directory must be `expected`.
        body = '''
actual="$(list pkg/dir)"
expected="$(cat <<'EXPECTED'
%sEXPECTED
)"
if [ "$actual" = "$expected" ]; then echo "listing OK"; else echo "listing differs:"; echo "$actual"; exit 1; fi
''' % ctx.attr.expected
    else:
        body = '''
if [ "${1:-}" = "--root" ]; then root=1; shift; else root=0; fi
for d in "$@"; do
  if [ $root = 1 ]; then
    if [ -L "$d" ]; then echo "root: symlink $d"; elif [ -d "$d" ]; then echo "root: directory $d"; fi
  fi
  list "$d"
done
'''
    ctx.actions.write(exe, "#!/bin/bash\nset -eu\n" + _LIST + body, is_executable = True)
    return [DefaultInfo(executable = exe, runfiles = ctx.runfiles(files = ctx.files.data))]

_LISTER_ATTRS = {
    "data": attr.label_list(allow_files = True),
    "expected": attr.string(),
}

lister = rule(implementation = _lister_impl, attrs = _LISTER_ATTRS, executable = True)

lister_test = rule(implementation = _lister_impl, attrs = _LISTER_ATTRS, test = True)

def _use_tool_impl(ctx):
    out = ctx.actions.declare_file(ctx.label.name + ".txt")
    tool = ctx.attr.tool[DefaultInfo].files_to_run
    ctx.actions.run_shell(
        outputs = [out],
        tools = [tool],
        command = '%s "$@" > %s' % (tool.executable.path, out.path),
        arguments = [tool.executable.path + ".runfiles/" + p for p in ctx.attr.paths],
        mnemonic = "UseTool",
    )
    return [DefaultInfo(files = depset([out]))]

use_tool = rule(
    implementation = _use_tool_impl,
    attrs = {
        "paths": attr.string_list(),
        "tool": attr.label(cfg = "exec", executable = True),
    },
)
