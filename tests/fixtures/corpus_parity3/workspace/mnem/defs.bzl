# B44: rules_cc writes module maps with mnemonic = "CppModuleMap"
# (cc_compilation_helper.bzl:409).
def _impl(ctx):
    a = ctx.actions.declare_file(ctx.label.name + ".cppmap")
    ctx.actions.write(a, "module \"x\" {}\n", is_executable = True, mnemonic = "CppModuleMap")
    b = ctx.actions.declare_file(ctx.label.name + ".params")
    args = ctx.actions.args()
    args.add("--flag")
    ctx.actions.write(b, args, mnemonic = "MyParams")
    c = ctx.actions.declare_file(ctx.label.name + ".plain")
    ctx.actions.write(c, "plain\n")
    d = ctx.actions.declare_file(ctx.label.name + ".none")
    ctx.actions.write(d, "none\n", mnemonic = None)
    return [DefaultInfo(files = depset([a, b, c, d]))]

writes = rule(implementation = _impl)

def _bad_impl(ctx):
    a = ctx.actions.declare_file(ctx.label.name + ".txt")
    ctx.actions.write(a, "x", mnemonic = 1)
    return [DefaultInfo(files = depset([a]))]

bad = rule(implementation = _bad_impl)
