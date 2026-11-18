# B35: rules_testing's testing_aspect reads target.actions on the target an
# analysis test's attribute names, through an alias (rules_kotlin's
# //kotlin/compiler:btapi_runtime).
Seen = provider(fields = ["mnemonics"])

def _seen_impl(target, ctx):
    return [Seen(mnemonics = [action.mnemonic for action in target.actions])]

seen = aspect(implementation = _seen_impl)

def _gen_impl(ctx):
    out = ctx.actions.declare_file(ctx.label.name + ".txt")
    ctx.actions.write(out, "gen\n")
    return [DefaultInfo(files = depset([out]))]

gen = rule(implementation = _gen_impl)

def _check_impl(ctx):
    mnemonics = ctx.attr.target[Seen].mnemonics
    if mnemonics != ["FileWrite"]:
        fail("unexpected actions: " + repr(mnemonics))
    return [AnalysisTestResultInfo(success = True, message = repr(mnemonics))]

def check(name, target):
    testing.analysis_test(
        name = name,
        implementation = _check_impl,
        attrs = {"target": attr.label(aspects = [seen])},
        attr_values = {"target": target},
    )
