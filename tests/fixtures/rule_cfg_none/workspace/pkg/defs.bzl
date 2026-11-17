Info = provider(fields = ["files", "label"])

def _leaf_impl(ctx):
    print("LEAF %s %s" % (ctx.label.name, ctx.bin_dir.path))
    return [
        Info(files = depset(ctx.files.srcs), label = str(ctx.label)),
        DefaultInfo(files = depset(ctx.files.srcs)),
    ]

leaf = rule(implementation = _leaf_impl, attrs = {"srcs": attr.label_list(allow_files = True)})

# A rule-level config.none() (NoConfigTransition), like toolchains_llvm's
# _headers_directory: analyzed once, in the no-config configuration.
def _none_impl(ctx):
    print("NONE %s %s %s" % (ctx.label.name, ctx.bin_dir.path, " ".join([f.path for f in ctx.attr.dep[Info].files.to_list()])))
    return [
        ctx.attr.dep[Info],
        DefaultInfo(files = ctx.attr.srcs[DefaultInfo].files),
    ]

none_rule = rule(
    implementation = _none_impl,
    attrs = {"dep": attr.label(), "srcs": attr.label()},
    cfg = config.none(),
)

def _none_act_impl(ctx):
    out = ctx.actions.declare_file(ctx.label.name + ".out")
    ctx.actions.write(out, "x\n")
    print("ACT %s %s" % (ctx.label.name, ctx.bin_dir.path))
    return [DefaultInfo(files = depset([out]))]

none_act = rule(implementation = _none_act_impl, cfg = config.none())

def _use_impl(ctx):
    out = ctx.actions.declare_file(ctx.label.name + ".txt")
    lines = []
    if Info in ctx.attr.dep:
        info = ctx.attr.dep[Info]
        lines = [info.label] + [f.path for f in info.files.to_list()]
    lines += [f.path for f in ctx.files.dep]
    print("USE %s %s %s" % (ctx.label.name, ctx.bin_dir.path, " ".join(lines)))
    ctx.actions.write(out, "\n".join(lines) + "\n")
    return [DefaultInfo(files = depset([out]))]

use = rule(implementation = _use_impl, attrs = {"dep": attr.label()})
