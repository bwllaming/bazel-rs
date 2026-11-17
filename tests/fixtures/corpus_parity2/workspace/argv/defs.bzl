# Action.argv of a spawn with a parameter file (rules_cc's link-variable
# tests read CppLink argv): SpawnAction.getStarlarkArgv is the whole command
# line, the parameter file's arguments included.
def _gen_impl(ctx):
    out = ctx.actions.declare_file(ctx.label.name + ".out")
    args = ctx.actions.args()
    args.add_all(["plain", "a b", "c'd", "--x=1"])
    args.add(out)
    args.use_param_file(ctx.attr.flag, use_always = True)
    args.set_param_file_format(ctx.attr.format)
    ctx.actions.run(
        executable = "/bin/true",
        arguments = ["first", args],
        outputs = [out],
        mnemonic = "Gen",
    )
    return [DefaultInfo(files = depset([out]))]

gen = rule(
    implementation = _gen_impl,
    attrs = {"flag": attr.string(), "format": attr.string()},
)

def _test_impl(ctx):
    actions = [a for a in ctx.attr.target_under_test.actions if a.mnemonic == "Gen"]
    return [AnalysisTestResultInfo(success = True, message = repr(actions[0].argv))]

argv_test = rule(
    implementation = _test_impl,
    attrs = {"target_under_test": attr.label()},
    analysis_test = True,
)
