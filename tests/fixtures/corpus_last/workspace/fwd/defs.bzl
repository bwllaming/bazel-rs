def _bin_impl(ctx):
    out = ctx.actions.declare_file(ctx.label.name)
    ctx.actions.write(out, "#!/bin/sh\necho hi\n", is_executable = True)
    return [DefaultInfo(executable = out, runfiles = ctx.runfiles(files = ctx.files.data))]

bin = rule(_bin_impl, executable = True, attrs = {"data": attr.label_list(allow_files = True)})

def _lib_impl(ctx):
    out = ctx.actions.declare_file(ctx.label.name + ".txt")
    ctx.actions.write(out, "lib\n")
    return [DefaultInfo(files = depset([out]), runfiles = ctx.runfiles(files = ctx.files.data))]

lib = rule(_lib_impl, attrs = {"data": attr.label_list(allow_files = True)})

def _show(files):
    return [(str(f), f.path) for f in files.to_list()]

# Forwards the dependency's OutputGroupInfo (with_cfg's frontend does).
def _fwd_impl(ctx):
    dep = ctx.attr.dep
    print("HIDDEN", ctx.label.name, _show(dep[OutputGroupInfo]._hidden_top_level_INTERNAL_))
    return [DefaultInfo(files = dep[DefaultInfo].files), dep[OutputGroupInfo]]

fwd = rule(_fwd_impl, attrs = {"dep": attr.label()})

# Only the files: the dependency's runfiles tree is not built.
def _files_impl(ctx):
    return [DefaultInfo(files = ctx.attr.dep[DefaultInfo].files)]

files_only = rule(_files_impl, attrs = {"dep": attr.label()})

# Reads a forwarding target's group (the forwarded tree plus its own runfiles).
def _show_impl(ctx):
    print("SEEN", ctx.label.name, _show(ctx.attr.dep[OutputGroupInfo]._hidden_top_level_INTERNAL_))
    return []

show = rule(_show_impl, attrs = {"dep": attr.label()})
