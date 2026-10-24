"""Unresolved symlink artifacts: ctx.actions.declare_symlink and
ctx.actions.symlink(target_path = ...) (StarlarkActionFactory.java:194-354,
UnresolvedSymlinkAction.java)."""

def _describe(f):
    return {
        "path": f.path,
        "short_path": f.short_path,
        "basename": f.basename,
        "dirname": f.dirname,
        "extension": f.extension,
        "is_source": f.is_source,
        "is_directory": f.is_directory,
        "is_symlink": f.is_symlink,
        "owner": str(f.owner),
        "root": f.root.path,
    }

def _link_impl(ctx):
    link = ctx.actions.declare_symlink(ctx.attr.link or ctx.label.name)
    kwargs = {}
    if ctx.attr.target_type:
        kwargs["target_type"] = ctx.attr.target_type
    ctx.actions.symlink(output = link, target_path = ctx.attr.target, **kwargs)
    return [DefaultInfo(files = depset([link]), runfiles = ctx.runfiles(files = [link]))]

# One unresolved symlink with a literal target.
link = rule(
    implementation = _link_impl,
    attrs = {
        "link": attr.string(),
        "target": attr.string(),
        "target_type": attr.string(),
    },
)

def _spawn_link_impl(ctx):
    # A spawn may create a declared symlink too.
    link = ctx.actions.declare_symlink(ctx.label.name)
    ctx.actions.run_shell(
        outputs = [link],
        command = "ln -s %s %s" % (ctx.attr.target, link.path),
        mnemonic = "SpawnLink",
    )
    return [DefaultInfo(files = depset([link]))]

spawn_link = rule(
    implementation = _spawn_link_impl,
    attrs = {"target": attr.string()},
)

def _probe_impl(ctx):
    lines = []
    for dep in ctx.attr.deps:
        for f in dep[DefaultInfo].files.to_list():
            lines.append(json.encode(_describe(f)))
    base = ctx.actions.declare_file(ctx.label.name + ".base")
    ctx.actions.write(base, "base\n")
    sibling = ctx.actions.declare_symlink(ctx.label.name + ".sibling", sibling = base)
    ctx.actions.symlink(output = sibling, target_path = base.basename, target_type = "file")
    lines.append(json.encode(_describe(sibling)))
    lines.append(json.encode(_describe(base)))
    lines.append("equal=%r distinct=%r set=%d" % (
        sibling == sibling,
        sibling == base,
        len(depset([sibling, base, sibling]).to_list()),
    ))
    out = ctx.actions.declare_file(ctx.label.name + ".txt")
    ctx.actions.write(out, "\n".join(lines) + "\n")
    return [DefaultInfo(files = depset([out]))]

# Analysis-time facts of symlink Files (the symlinks themselves are not
# built: only the probe's text file is a file to build).
probe = rule(
    implementation = _probe_impl,
    attrs = {"deps": attr.label_list()},
)

def _errors_impl(ctx):
    mode = ctx.attr.mode
    name = ctx.label.name
    data = ctx.file.data
    if mode == "file_then_symlink":
        f = ctx.actions.declare_file(name + ".x")
        ctx.actions.write(f, "")
        ctx.actions.declare_symlink(name + ".x")
    elif mode == "symlink_then_file":
        s = ctx.actions.declare_symlink(name + ".x")
        f = ctx.actions.declare_file(name + ".x")
        ctx.actions.write(f, "")
    elif mode == "target_file_on_symlink":
        s = ctx.actions.declare_symlink(name + ".x")
        ctx.actions.symlink(output = s, target_file = data)
    elif mode == "target_path_on_file":
        f = ctx.actions.declare_file(name + ".x")
        ctx.actions.symlink(output = f, target_path = "data.txt")
    elif mode == "is_executable":
        s = ctx.actions.declare_symlink(name + ".x")
        ctx.actions.symlink(output = s, target_path = "data.txt", is_executable = True)
    elif mode == "bad_target_type":
        s = ctx.actions.declare_symlink(name + ".x")
        ctx.actions.symlink(output = s, target_path = "data.txt", target_type = "bogus")
    elif mode == "both_targets":
        s = ctx.actions.declare_symlink(name + ".x")
        ctx.actions.symlink(output = s, target_file = data, target_path = "data.txt")
    elif mode == "no_action":
        s = ctx.actions.declare_symlink(name + ".x")
        return [DefaultInfo(files = depset([s]))]
    elif mode == "target_path_int":
        s = ctx.actions.declare_symlink(name + ".x")
        ctx.actions.symlink(output = s, target_path = 1)
    out = ctx.actions.declare_file(name + ".txt")
    ctx.actions.write(out, mode + "\n")
    return [DefaultInfo(files = depset([out]))]

# Each mode one error (or none).
errors = rule(
    implementation = _errors_impl,
    attrs = {
        "data": attr.label(allow_single_file = True, default = "data.txt"),
        "mode": attr.string(),
    },
)

def _consumer_impl(ctx):
    # Each dependency file's readlink target as action inputs.
    inputs = ctx.files.deps
    out = ctx.actions.declare_file(ctx.label.name + ".txt")
    ctx.actions.run_shell(
        inputs = inputs,
        outputs = [out],
        command = "for f in \"$@\"; do echo \"$f -> $(readlink \"$f\")\"; done > " + out.path,
        arguments = [f.path for f in inputs],
        mnemonic = "Readlink",
    )
    return [DefaultInfo(files = depset([out]))]

consumer = rule(
    implementation = _consumer_impl,
    attrs = {"deps": attr.label_list()},
)

def _runner_impl(ctx):
    out = ctx.actions.declare_file(ctx.label.name)
    ctx.actions.write(
        out,
        "#!/bin/sh\nfind . -mindepth 1 | LC_ALL=C sort | while read f; do if [ -L \"$f\" ]; then echo \"$f\"; fi; done\n",
        is_executable = True,
    )
    runfiles = ctx.runfiles().merge_all([dep[DefaultInfo].default_runfiles for dep in ctx.attr.data])
    return [DefaultInfo(executable = out, runfiles = runfiles)]

# `run` lists the symlinks of its runfiles tree.
runner = rule(
    implementation = _runner_impl,
    executable = True,
    attrs = {"data": attr.label_list()},
)
