# Args.validateMapEach: a map_each function created by the rule
# implementation (a nested def, a lambda) is not global.
def _impl(ctx):
    def nested(item):
        return item.path

    out = ctx.actions.declare_file(ctx.label.name + ".txt")
    args = ctx.actions.args()
    if ctx.attr.kind == "nested":
        args.add_all([out], map_each = nested)
    else:
        args.add_all([out], map_each = lambda item: item.path)
    ctx.actions.run_shell(outputs = [out], arguments = [args], command = "touch $1")
    return [DefaultInfo(files = depset([out]))]

mk = rule(implementation = _impl, attrs = {"kind": attr.string()})
