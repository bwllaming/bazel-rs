# The B13c prerequisite: the execution platform is part of an action's key
# (ActionKeyComputer.getKey), so two targets declaring the same output with
# otherwise identical actions share it on one execution platform and
# conflict on two (MapBasedActionGraph.registerAction, Actions.canBeShared).
def _gen_impl(ctx):
    out = ctx.actions.declare_file("shared_%s.txt" % ctx.attr.kind)
    if ctx.attr.kind == "run":
        ctx.actions.run_shell(
            outputs = [out],
            command = "echo same > $1",
            arguments = [out.path],
            mnemonic = "Gen",
        )
    else:
        ctx.actions.write(out, "same\n")
    return [DefaultInfo(files = depset([out]))]

gen = rule(implementation = _gen_impl, attrs = {"kind": attr.string()})
