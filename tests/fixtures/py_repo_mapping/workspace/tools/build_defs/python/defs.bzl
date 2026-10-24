"""py_internal.create_repo_mapping_manifest (Bazel 9.3
rules/python/PyBuiltins.java:157-181): a RepoMappingManifestAction of the
given runfiles, as rules_python's py_binary writes `<name>.zip.repo_mapping`.

py_internal is available to @_builtins, @bazel_tools, rules_python and the
main repository's tools/build_defs/python (BazelRuleClassProvider).
"""

def _manifest_impl(ctx):
    runfiles = ctx.runfiles(
        files = ctx.files.data,
        symlinks = {"sub/link": f for f in ctx.files.symlinked},
        root_symlinks = {"other+/root_link": f for f in ctx.files.root_symlinked},
    )
    for dep in ctx.attr.data:
        runfiles = runfiles.merge(dep[DefaultInfo].default_runfiles)
    out = ctx.actions.declare_file(ctx.label.name + ".repo_mapping")
    result = py_internal.create_repo_mapping_manifest(
        ctx = ctx,
        runfiles = runfiles,
        output = out,
    )
    info = ctx.actions.declare_file(ctx.label.name + ".info")
    ctx.actions.write(info, "result=" + repr(result) + "\n")
    return [DefaultInfo(files = depset([out, info]))]

manifest = rule(
    implementation = _manifest_impl,
    attrs = {
        "data": attr.label_list(allow_files = True),
        "root_symlinked": attr.label_list(allow_files = True),
        "symlinked": attr.label_list(allow_files = True),
    },
)

def _bad_impl(ctx):
    mode = ctx.attr.mode
    f = py_internal.create_repo_mapping_manifest
    out = ctx.actions.declare_file(ctx.label.name + ".repo_mapping")
    runfiles = ctx.runfiles()
    if mode == "positional":
        f(ctx)
    elif mode == "ctx_type":
        f(ctx = None, runfiles = runfiles, output = out)
    elif mode == "runfiles_type":
        f(ctx = ctx, runfiles = [], output = out)
    elif mode == "output_type":
        f(ctx = ctx, runfiles = runfiles, output = "x")
    elif mode == "unknown_keyword":
        f(ctx = ctx, runfile = runfiles, output = out)
    elif mode == "missing":
        f(ctx = ctx, runfiles = runfiles)
    return [DefaultInfo(files = depset([out]))]

bad = rule(
    implementation = _bad_impl,
    attrs = {"mode": attr.string()},
)
