Required = provider()

def _writer(ctx):
    if ctx.attr.kind == "tree":
        out = ctx.actions.declare_directory(ctx.attr.path)
        ctx.actions.run_shell(outputs = [out], command = "mkdir -p " + out.path + "; echo " + ctx.attr.content + " > " + out.path + "/entry")
    elif ctx.attr.kind == "symlink":
        out = ctx.actions.declare_symlink(ctx.attr.path)
        ctx.actions.symlink(output = out, target_path = ctx.attr.content)
    else:
        out = ctx.actions.declare_file(ctx.attr.path)
        ctx.actions.write(out, ctx.attr.content)
    return [DefaultInfo(files = depset([out]))]

writer = rule(implementation = _writer, attrs = {"path": attr.string(mandatory = True), "content": attr.string(default = "same"), "kind": attr.string(default = "file")})

def _probe(ctx):
    files = ctx.attr.dep[DefaultInfo].files.to_list()
    executable = ctx.attr.dep[DefaultInfo].files_to_run.executable
    result = {"files": [(f.short_path, str(f.owner), f.is_directory, f.is_source) for f in files], "executable_owner": str(executable.owner) if executable else None}
    print("ARTIFACTS:" + ctx.label.name + ":" + json.encode(result))
    return []

probe = rule(implementation = _probe, attrs = {"dep": attr.label(mandatory = True)})

def _invalid(ctx):
    out = ctx.actions.declare_file(ctx.label.name + ".out")
    if ctx.attr.mode == "orphan":
        return [DefaultInfo(files = depset([out]))]
    ctx.actions.write(out, "first")
    if ctx.attr.mode == "duplicate":
        ctx.actions.write(out, "second")
    elif ctx.attr.mode == "tree_file":
        tree = ctx.actions.declare_directory(ctx.label.name + ".out")
        ctx.actions.run_shell(outputs = [tree], command = "mkdir -p " + tree.path)
    return [DefaultInfo(files = depset([out]))]

invalid = rule(implementation = _invalid, attrs = {"mode": attr.string()})

def _required(ctx):
    return []

required = rule(implementation = _required, attrs = {"dep": attr.label(providers = [Required])})

def _wrong_owner(ctx):
    return [DefaultInfo(executable = ctx.file.dep)]

wrong_owner = rule(implementation = _wrong_owner, executable = True, attrs = {"dep": attr.label(allow_single_file = True)})

def _sentinel(ctx):
    out = ctx.actions.declare_file(ctx.label.name + ".out")
    ctx.actions.run_shell(inputs = [ctx.info_file], outputs = [out], command = "touch " + ctx.attr.sentinel + "; echo ran > " + out.path)
    return [DefaultInfo(files = depset([out]))]

sentinel = rule(implementation = _sentinel, attrs = {"sentinel": attr.string()})
