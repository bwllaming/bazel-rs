# Prints, for every listed file (relative to the directory it runs in), the
# first listed file with the same (device, inode) after following links,
# and whether the path itself is a symlink.
_PROBE = """#!/bin/bash
cd "$1"
for f in {files}; do
  echo "$(stat -L -c %d:%i "$f") $f $(if [ -L "$f" ]; then echo symlink; else echo file; fi)"
done | awk '{ if (!($1 in first)) first[$1] = $2; print "{kind}_" $2 "=" first[$1] " " $3 }'
"""

def _probe(kind, directory, files):
    return _PROBE.replace("{kind}", kind).replace('cd "$1"', "cd " + directory).replace("{files}", " ".join(files))

def _runfiles_paths(files):
    return [f.short_path for f in files]

def _tree(ctx):
    out = ctx.actions.declare_directory(ctx.label.name)
    ctx.actions.run_shell(outputs = [out], command = "mkdir -p %s/sub && echo same > %s/sub/f.txt" % (out.path, out.path))
    return [DefaultInfo(files = depset([out]))]

tree = rule(implementation = _tree)

def _alias(ctx):
    out = ctx.actions.declare_file(ctx.label.name + ".txt")
    ctx.actions.symlink(output = out, target_file = ctx.file.src)
    return [DefaultInfo(files = depset([out]))]

alias = rule(implementation = _alias, attrs = {"src": attr.label(allow_single_file = True)})

def _expanded(files):
    # The tree artifact's one file, by its known name.
    return [f.short_path + "/sub/f.txt" if f.is_directory else f.short_path for f in files]

def _probe_test(ctx):
    exe = ctx.actions.declare_file(ctx.label.name + ".sh")
    ctx.actions.write(exe, _probe("test_runfile", '"$TEST_SRCDIR/_main"', _expanded(ctx.files.data)), is_executable = True)
    return [DefaultInfo(executable = exe, runfiles = ctx.runfiles(files = ctx.files.data))]

probe_test = rule(
    implementation = _probe_test,
    test = True,
    attrs = {"data": attr.label_list(allow_files = True)},
)

def _runner(ctx):
    exe = ctx.actions.declare_file(ctx.label.name + ".sh")
    ctx.actions.write(exe, _probe("tool_runfile", '"$0.runfiles/_main"', _expanded(ctx.files.data)), is_executable = True)
    return [DefaultInfo(executable = exe, runfiles = ctx.runfiles(files = ctx.files.data))]

runner = rule(
    implementation = _runner,
    executable = True,
    attrs = {"data": attr.label_list(allow_files = True)},
)

def _inputs(ctx):
    out = ctx.actions.declare_file(ctx.label.name + ".txt")
    script = ctx.actions.declare_file(ctx.label.name + ".sh")
    paths = [f.path + "/sub/f.txt" if f.is_directory else f.path for f in ctx.files.data]
    ctx.actions.write(script, _probe("input", ".", paths), is_executable = True)
    ctx.actions.run_shell(
        inputs = ctx.files.data,
        tools = [script],
        outputs = [out],
        command = "%s > %s" % (script.path, out.path),
    )
    return [DefaultInfo(files = depset([out]))]

inputs = rule(implementation = _inputs, attrs = {"data": attr.label_list(allow_files = True)})
