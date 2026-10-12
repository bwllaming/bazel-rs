def _impl(ctx):
    first = ctx.actions.declare_file("first.txt")
    second = ctx.actions.declare_file("second.txt")
    args = ctx.actions.args()
    args.add("first")
    ctx.actions.run_shell(outputs = [first], command = 'out="$1"; shift; printf "%s\n" "$@" > "$out"', arguments = [first.path, args])
    args.add("second")
    ctx.actions.run_shell(outputs = [second], command = 'out="$1"; shift; printf "%s\n" "$@" > "$out"', arguments = [second.path, args])
    return [DefaultInfo(files = depset([first, second]))]

argument_rule = rule(implementation = _impl)
