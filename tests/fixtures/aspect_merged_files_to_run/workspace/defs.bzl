AspInfo = provider(fields = ["label"])

def _asp_impl(target, ctx):
    return [AspInfo(label = str(target.label))]

asp = aspect(implementation = _asp_impl)

def _empty_asp_impl(target, ctx):
    return []

empty_asp = aspect(implementation = _empty_asp_impl)

def _exe_impl(ctx):
    out = ctx.actions.declare_file(ctx.label.name + ".sh")
    ctx.actions.write(out, "#!/bin/sh\n", is_executable = True)
    return [DefaultInfo(
        executable = out,
        runfiles = ctx.runfiles(files = ctx.files.data),
    )]

exe = rule(
    implementation = _exe_impl,
    executable = True,
    attrs = {"data": attr.label_list(allow_files = True)},
)

def _script_impl(ctx):
    # Like ~/code's copyfiles: a non-executable rule naming an executable.
    out = ctx.actions.declare_file(ctx.label.name + ".sh")
    ctx.actions.write(out, "#!/bin/sh\n", is_executable = True)
    return [DefaultInfo(files = depset([out]), executable = out)]

script = rule(implementation = _script_impl)

def _wrap_impl(ctx):
    # Like sh_binary over a generated source: a symlink to it.
    out = ctx.actions.declare_file(ctx.label.name)
    ctx.actions.symlink(output = out, target_file = ctx.file.src, is_executable = True)
    return [DefaultInfo(executable = out, runfiles = ctx.runfiles(files = [ctx.file.src]))]

wrap = rule(
    implementation = _wrap_impl,
    executable = True,
    attrs = {"src": attr.label(allow_single_file = True)},
)

def _show(kind, ctx):
    for d in ctx.attr.deps:
        info = d[DefaultInfo]
        print("E %s %s exe=%s same=%s runfiles=%s files=%s asp=%s" % (
            kind,
            d.label.name,
            info.files_to_run.executable.short_path if info.files_to_run.executable else None,
            d.files_to_run.executable == info.files_to_run.executable,
            sorted([f.short_path for f in info.default_runfiles.files.to_list()]),
            sorted([f.short_path for f in info.files.to_list()]),
            d[AspInfo].label if AspInfo in d else "-",
        ))
    return []

def _plain_impl(ctx):
    return _show("plain", ctx)

def _with_asp_impl(ctx):
    return _show("asp", ctx)

def _with_empty_impl(ctx):
    return _show("empty", ctx)

_deps = {"allow_files": True}

plain = rule(implementation = _plain_impl, attrs = {"deps": attr.label_list(**_deps)})
with_asp = rule(implementation = _with_asp_impl, attrs = {"deps": attr.label_list(aspects = [asp], **_deps)})
with_empty = rule(implementation = _with_empty_impl, attrs = {"deps": attr.label_list(aspects = [empty_asp], **_deps)})
