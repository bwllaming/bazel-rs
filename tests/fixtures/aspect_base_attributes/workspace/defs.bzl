BInfo = provider(fields = ["v"])
CInfo = provider(fields = ["v"])

def _base_impl(target, ctx):
    return [BInfo(v = str(target.label.name))]

# A required base aspect with implicit label attributes (an exec tool, a
# file list, a single file), a parameter and private strings.
base = aspect(
    _base_impl,
    attr_aspects = ["deps", "_mdep"],
    attrs = {
        "_btool": attr.label(default = "//:btool", cfg = "exec", executable = True),
        "_bdata": attr.label_list(default = ["//:bdata.txt", "//:gen"], allow_files = True),
        "_bfile": attr.label(default = "//:bdata.txt", allow_single_file = True),
        "_shared": attr.string(default = "from_base"),
        "_rimpl": attr.string(default = "from_base"),
        "flavor": attr.string(default = "plain", values = ["plain", "spicy"]),
    },
)

def _seen_impl(target, ctx):
    return [CInfo(v = "c")]

# Seen through required_aspect_providers only (not required).
seen = aspect(
    _seen_impl,
    attr_aspects = ["deps"],
    provides = [CInfo],
    attrs = {
        "_shared": attr.string(default = "from_seen"),
        "_cdep": attr.label(default = "//:cdep"),
    },
)

def _main_impl(target, ctx):
    r = ctx.rule.attr
    names = [n for n in ["_bdata", "_bfile", "_btool", "_cdep", "_mdep", "_rimpl", "_shared", "deps", "flavor"] if hasattr(r, n)]
    print("M", target.label.name, names)
    print("V", target.label.name,
          "btool=" + r._btool.label.name,
          "exe=" + ctx.rule.executable._btool.path,
          "bdata=" + str([f.path for f in ctx.rule.files._bdata]),
          "bfile=" + ctx.rule.file._bfile.path,
          "shared=" + r._shared,
          "rimpl=" + str(r._rimpl),
          "flavor=" + r.flavor,
          "cdep=" + (r._cdep.label.name if hasattr(r, "_cdep") else "-"),
          "own=" + str([hasattr(ctx.attr, n) for n in ["_btool", "_shared", "_mdep"]]),
          "mdep_b=" + str(BInfo in ctx.attr._mdep),
          "ids=" + str(ctx.aspect_ids))
    return []

main = aspect(
    _main_impl,
    attr_aspects = ["deps"],
    requires = [base],
    required_aspect_providers = [CInfo],
    attrs = {
        "_shared": attr.string(default = "from_main"),
        "_mdep": attr.label(default = "//:mdep"),
    },
)

def _lib_impl(ctx):
    return []

lib = rule(_lib_impl, attrs = {
    "deps": attr.label_list(),
    "_rimpl": attr.label(default = "//:rimpl"),
})

top = rule(_lib_impl, attrs = {
    "deps": attr.label_list(aspects = [seen, main]),
})

def _tool_impl(ctx):
    out = ctx.actions.declare_file(ctx.label.name + ".sh")
    ctx.actions.write(out, "#!/bin/sh\n", is_executable = True)
    return [DefaultInfo(executable = out)]

tool = rule(_tool_impl, executable = True)

def _gen_impl(ctx):
    out = ctx.actions.declare_file(ctx.label.name + ".out")
    ctx.actions.write(out, "gen")
    return [DefaultInfo(files = depset([out]))]

gen = rule(_gen_impl)
