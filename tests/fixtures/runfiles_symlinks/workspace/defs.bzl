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

# B15 follow-up: unresolved symlinks (rules_js `declare_symlink` package
# links), runfiles symlinks and root symlinks, an empty tree and a source
# directory; and a node-like lookup of a sibling next to a file's realpath.
def _store(ctx):
    f = ctx.actions.declare_file("a/f.txt")
    ctx.actions.write(f, "hi\n")
    link = ctx.actions.declare_symlink("a/link")
    ctx.actions.symlink(output = link, target_path = "f.txt")
    empty = ctx.actions.declare_directory("empty_tree")
    ctx.actions.run_shell(outputs = [empty], command = "mkdir -p " + empty.path)
    return [DefaultInfo(files = depset([f, link, empty]))]

store = rule(implementation = _store)

_EXTRA = """#!/bin/bash
root={root}
probe() {
  local p="$1"
  local kind=file
  if [ -L "$p" ]; then kind=symlink; elif [ -d "$p" ]; then kind=dir; elif [ ! -e "$p" ]; then kind=missing; fi
  echo "{kind}_$2 kind=$kind readlink=$(readlink "$p") realpath=$(realpath "$p")"
}
probe "$0" self
probe "$root/a/f.txt" a/f.txt
probe "$root/a/link" a/link
d=$(dirname "$(realpath "$root/a/f.txt")")
echo "{kind}_sibling_dir $(ls -A "$d" | sort | tr '\\n' ' ')"
echo "{kind}_sibling_link kind=$(if [ -L "$d/link" ]; then echo symlink; else echo missing; fi) readlink=$(readlink "$d/link") content=$(cat "$d/link" 2>&1 | head -1)"
probe "$root/empty_tree" empty_tree
echo "{kind}_empty_tree_listing [$(ls -A "$root/empty_tree" 2>&1)]"
probe "$root/../rootsym.txt" rootsym.txt
probe "$root/sym/gen.txt" sym/gen.txt
probe "$root/srcdir" srcdir
probe "$root/srcdir/inner/s.txt" srcdir/inner/s.txt
"""

def _extra_impl(kind, root):
    def impl(ctx):
        exe = ctx.actions.declare_file(ctx.label.name + ".sh")
        ctx.actions.write(exe, _EXTRA.replace("{kind}", kind).replace("{root}", root), is_executable = True)
        runfiles = ctx.runfiles(
            files = ctx.files.data,
            symlinks = {"sym/gen.txt": ctx.file.gen},
            root_symlinks = {"rootsym.txt": ctx.file.src},
        )
        return [DefaultInfo(executable = exe, runfiles = runfiles)]
    return impl

_EXTRA_ATTRS = {
    "data": attr.label_list(allow_files = True),
    "gen": attr.label(allow_single_file = True),
    "src": attr.label(allow_single_file = True),
}

extra_test = rule(implementation = _extra_impl("extra_test", '"$TEST_SRCDIR/_main"'), test = True, attrs = _EXTRA_ATTRS)

extra_binary = rule(implementation = _extra_impl("extra_run", '"$0.runfiles/_main"'), executable = True, attrs = _EXTRA_ATTRS)
