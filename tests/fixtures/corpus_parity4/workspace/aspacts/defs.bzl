# B33: Target.actions of a target merged with the aspects it requires
# (rules_testing's make_testing_aspect(aspects = [...]) over protobuf's
# java_proto_aspect): the aspects' actions, then the target's.
Seen = provider(fields = ["mnemonics"])

def _gen_impl(target, ctx):
    out = ctx.actions.declare_file(ctx.label.name + ".gen")
    ctx.actions.run_shell(outputs = [out], command = "touch $1", arguments = [out.path], mnemonic = "GenProto")
    return []

gen = aspect(implementation = _gen_impl)

def _seen_impl(target, ctx):
    return [Seen(mnemonics = [a.mnemonic for a in target.actions])]

seen = aspect(implementation = _seen_impl, requires = [gen])

def _lib_impl(ctx):
    out = ctx.actions.declare_file(ctx.label.name + ".txt")
    ctx.actions.write(out, "lib\n")
    return [DefaultInfo(files = depset([out]))]

lib = rule(implementation = _lib_impl)

def _check_impl(ctx):
    return [AnalysisTestResultInfo(success = True, message = repr(ctx.attr.target[Seen].mnemonics))]

def check(name, target):
    testing.analysis_test(
        name = name,
        implementation = _check_impl,
        attrs = {"target": attr.label(aspects = [seen])},
        attr_values = {"target": target},
    )

# A rule under test handing its dependency's actions on (protobuf's
# rust_proto_library_unit_test `attach_upb_aspect`).
ActionsInfo = provider(fields = ["actions"])

def _attach_impl(ctx):
    return [ActionsInfo(actions = ctx.attr.dep.actions)]

attach = rule(implementation = _attach_impl, attrs = {"dep": attr.label(aspects = [gen])})

def _check_attached_impl(ctx):
    mnemonics = [a.mnemonic for a in ctx.attr.target[ActionsInfo].actions]
    return [AnalysisTestResultInfo(success = True, message = repr(mnemonics))]

def check_attached(name, target):
    testing.analysis_test(
        name = name,
        implementation = _check_attached_impl,
        attrs = {"target": attr.label()},
        attr_values = {"target": target},
    )
