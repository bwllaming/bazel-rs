# B13b: `srcs = ["."]`, the package directory as a source label (skylib's
# directory_test `_directory_with_self_srcs_test`).
def _impl(ctx):
    out = ctx.actions.declare_file(ctx.label.name + ".txt")
    lines = []
    for f in ctx.files.srcs:
        lines.append("|".join([
            f.path,
            f.short_path,
            f.basename,
            f.dirname,
            f.extension,
            str(f.is_source),
            str(f.is_directory),
            str(f.owner),
            f.root.path,
        ]))
    ctx.actions.write(out, "\n".join(lines) + "\n")
    return [DefaultInfo(files = depset([out]))]

describe = rule(
    implementation = _impl,
    attrs = {"srcs": attr.label_list(allow_files = True)},
)
