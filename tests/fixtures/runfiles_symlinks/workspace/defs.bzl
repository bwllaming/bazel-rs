# Prints, for the probe's own executable and every runfile: whether the
# path is a symlink, its link text and its resolved path.
_PROBE = """#!/bin/bash
self="$0"
root={root}
probe() {
  local p="$1"
  local kind=file
  if [ -L "$p" ]; then kind=symlink; fi
  echo "{kind}_$2 kind=$kind readlink=$(readlink "$p") realpath=$(realpath "$p")"
}
probe "$self" self
for f in {files}; do
  probe "$root/$f" "$f"
done
probe "$root" runfiles_root
echo "{kind}_pwd $(pwd -P)"
"""

def _probe(kind, root, files):
    return _PROBE.replace("{kind}", kind).replace("{root}", root).replace("{files}", " ".join(files))

def _expanded(files):
    return [f.short_path + "/f.txt" if f.is_directory else f.short_path for f in files]

def _tree(ctx):
    out = ctx.actions.declare_directory(ctx.label.name)
    ctx.actions.run_shell(outputs = [out], command = "echo tree > %s/f.txt" % out.path)
    return [DefaultInfo(files = depset([out]))]

tree = rule(implementation = _tree)

def _alias(ctx):
    out = ctx.actions.declare_file(ctx.label.name + ".txt")
    ctx.actions.symlink(output = out, target_file = ctx.file.src)
    return [DefaultInfo(files = depset([out]))]

alias = rule(implementation = _alias, attrs = {"src": attr.label(allow_single_file = True)})

def _impl(kind, root):
    def impl(ctx):
        exe = ctx.actions.declare_file(ctx.label.name + ".sh")
        ctx.actions.write(exe, _probe(kind, root, _expanded(ctx.files.data)), is_executable = True)
        return [DefaultInfo(executable = exe, runfiles = ctx.runfiles(files = ctx.files.data))]
    return impl

probe_test = rule(
    implementation = _impl("test", '"$TEST_SRCDIR/_main"'),
    test = True,
    attrs = {"data": attr.label_list(allow_files = True)},
)

probe_binary = rule(
    implementation = _impl("run", '"$0.runfiles/_main"'),
    executable = True,
    attrs = {"data": attr.label_list(allow_files = True)},
)
