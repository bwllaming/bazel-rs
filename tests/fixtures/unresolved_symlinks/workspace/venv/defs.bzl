"""A rules_python-like venv (python/private/venv_runfiles.bzl, py_executable
.bzl): `_NAME.venv/bin/python3` is an unresolved symlink whose relative
target reaches the interpreter repository from the runfiles tree, next to a
declared `pyvenv.cfg`; the launcher runs the venv's python3 from its
runfiles."""

def _runfiles_path(f):
    # `_main/pkg/x` for the main repository, `repo/x` for an external one.
    if f.short_path.startswith("../"):
        return f.short_path[3:]
    return "_main/" + f.short_path

def _venv_impl(ctx):
    name = ctx.label.name
    venv = "_%s.venv" % name
    python = ctx.actions.declare_symlink(venv + "/bin/python3")
    up = "/".join([".."] * (len(_runfiles_path(python).split("/")) - 1))
    ctx.actions.symlink(
        output = python,
        target_path = up + "/" + _runfiles_path(ctx.file.interpreter),
    )
    cfg = ctx.actions.declare_file(venv + "/pyvenv.cfg")
    ctx.actions.write(cfg, "home = venv-of-" + name)
    launcher = ctx.actions.declare_file(name)
    ctx.actions.write(
        launcher,
        "\n".join([
            "#!/bin/sh",
            "if [ -d \"$0.runfiles\" ]; then R=\"$0.runfiles\"; else R=\"$RUNFILES_DIR\"; fi",
            "exec \"$R/%s\" \"$@\"" % _runfiles_path(python),
            "",
        ]),
        is_executable = True,
    )
    runfiles = ctx.runfiles(files = [python, cfg]).merge(
        ctx.attr.runtime[DefaultInfo].default_runfiles,
    ).merge(ctx.runfiles(files = ctx.files.runtime))
    return [DefaultInfo(executable = launcher, runfiles = runfiles)]

_ATTRS = {
    "interpreter": attr.label(allow_single_file = True, default = "@pyrt//:bin/python3"),
    "runtime": attr.label(default = "@pyrt//:runtime"),
}

venv_binary = rule(
    implementation = _venv_impl,
    executable = True,
    attrs = _ATTRS,
)

venv_test = rule(
    implementation = _venv_impl,
    test = True,
    attrs = _ATTRS,
)

def _use_impl(ctx):
    # The venv binary as a tool of a spawn (its runfiles tree staged).
    out = ctx.actions.declare_file(ctx.label.name + ".txt")
    ctx.actions.run_shell(
        outputs = [out],
        tools = [ctx.attr.tool[DefaultInfo].files_to_run],
        command = "%s tool > %s" % (ctx.executable.tool.path, out.path),
        mnemonic = "UseVenv",
    )
    return [DefaultInfo(files = depset([out]))]

venv_use = rule(
    implementation = _use_impl,
    attrs = {"tool": attr.label(executable = True, cfg = "exec")},
)

def _execroot_link_impl(ctx):
    # A link whose relative target resolves in the execution root.
    link = ctx.actions.declare_symlink(ctx.label.name + ".link")
    up = "/".join([".."] * len(link.dirname.split("/")))
    ctx.actions.symlink(output = link, target_path = up + "/" + ctx.file.target.path)
    out = ctx.actions.declare_file(ctx.label.name + ".txt")
    ctx.actions.run_shell(
        inputs = [link, ctx.file.target],
        outputs = [out],
        command = "readlink %s > %s && cat %s >> %s" % (link.path, out.path, link.path, out.path),
        mnemonic = "ThroughLink",
    )
    return [DefaultInfo(files = depset([out]))]

execroot_link = rule(
    implementation = _execroot_link_impl,
    attrs = {"target": attr.label(allow_single_file = True)},
)

def _not_a_symlink_impl(ctx):
    # A spawn that leaves a regular file at a declared symlink's path.
    link = ctx.actions.declare_symlink(ctx.label.name)
    ctx.actions.run_shell(
        outputs = [link],
        command = "echo text > " + link.path,
        mnemonic = "NotALink",
    )
    return [DefaultInfo(files = depset([link]))]

not_a_symlink = rule(implementation = _not_a_symlink_impl)

def _manifest_line_impl(ctx):
    # The tool's runfiles manifest lines of its venv (SourceManifestAction
    # writes an unresolved symlink's link text).
    manifest = ctx.attr.tool[DefaultInfo].files_to_run.runfiles_manifest
    out = ctx.actions.declare_file(ctx.label.name + ".txt")
    ctx.actions.run_shell(
        inputs = [manifest],
        outputs = [out],
        command = "grep '.venv/bin/python3 ' %s > %s" % (manifest.path, out.path),
        mnemonic = "ManifestLine",
    )
    return [DefaultInfo(files = depset([out]))]

manifest_line = rule(
    implementation = _manifest_line_impl,
    attrs = {"tool": attr.label(executable = True, cfg = "target")},
)
