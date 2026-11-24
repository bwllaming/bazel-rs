def _describe(target):
    return [
        (a.mnemonic, a.argv, [f.path for f in a.inputs.to_list()], [f.path for f in a.outputs.to_list()], sorted(a.env.items()))
        for a in target.actions
    ]

# B32: an analysis test reads a native genrule's actions.
def _genrule_test_impl(ctx):
    print("ACTIONS", _describe(ctx.attr.dep))
    return [AnalysisTestResultInfo(success = True, message = "")]

genrule_test = rule(_genrule_test_impl, analysis_test = True, attrs = {"dep": attr.label()})

def _tool_impl(ctx):
    out = ctx.actions.declare_file(ctx.label.name + ".sh")
    ctx.actions.run_shell(
        outputs = [out],
        inputs = ctx.files.srcs,
        command = "echo '#!/bin/sh' > $1",
        arguments = [out.path],
        mnemonic = "MakeTool",
    )
    return [DefaultInfo(executable = out)]

tool = rule(_tool_impl, executable = True, attrs = {"srcs": attr.label_list(allow_files = True), "deps": attr.label_list()})

def _leaf_impl(ctx):
    out = ctx.actions.declare_file(ctx.label.name + ".txt")
    ctx.actions.write(out, "leaf\n")
    return [DefaultInfo(files = depset([out]))]

leaf = rule(_leaf_impl)

Seen = provider(fields = ["mnemonics"])

# B32: a propagating aspect reads target.actions of an exec tool and its
# dependencies (outside the test's configuration).
def _seen_impl(target, ctx):
    return [Seen(mnemonics = [a.mnemonic for a in target.actions])]

seen_aspect = aspect(_seen_impl, attr_aspects = ["*"])

def _walk_impl(target, ctx):
    found = []
    for name in ["tools", "deps", "srcs"]:
        for dep in getattr(ctx.rule.attr, name, []) or []:
            if type(dep) == "Target" and Seen in dep:
                found.append((str(dep.label), dep[Seen].mnemonics))
            if type(dep) == "Target" and Walk in dep:
                found.extend(dep[Walk].found)
    return [Walk(found = found)]

Walk = provider(fields = ["found"])

walk_aspect = aspect(_walk_impl, attr_aspects = ["*"], requires = [seen_aspect])

def _walk_test_impl(ctx):
    print("WALK", sorted(ctx.attr.target[Walk].found))
    return [AnalysisTestResultInfo(success = True, message = "")]

walk_test = rule(
    _walk_test_impl,
    analysis_test = True,
    attrs = {"target": attr.label(aspects = [walk_aspect])},
)
