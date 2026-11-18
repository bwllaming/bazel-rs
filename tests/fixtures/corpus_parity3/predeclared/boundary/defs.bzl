# B8: rules_js js_run_binary(out_dirs = ["build", "public/build"]) beside
# public/BUILD.bazel: a declared file or directory may lie below a
# subpackage's directory.
def _impl(ctx):
    f = ctx.actions.declare_file("public/build/index.txt")
    ctx.actions.write(f, "index\n")
    d = ctx.actions.declare_directory("public/assets")
    ctx.actions.run_shell(
        outputs = [d],
        command = "mkdir -p $1 && echo asset > $1/a.txt",
        arguments = [d.path],
    )
    listing = ctx.actions.declare_file(ctx.label.name + ".txt")
    ctx.actions.write(listing, "\n".join([f.short_path, d.short_path]) + "\n")
    return [DefaultInfo(files = depset([f, d, listing]))]

out_dirs = rule(implementation = _impl)

def _pre_impl(ctx):
    ctx.actions.write(ctx.outputs.out, "pre\n")

predeclared = rule(implementation = _pre_impl, attrs = {"out": attr.output()})
