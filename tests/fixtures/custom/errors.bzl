"""Each rejection is isolated in its own package."""

def _fail_impl(ctx):
    fail("custom implementation deliberate failure")

impl_fail = rule(implementation = _fail_impl)

def _unproduced_impl(ctx):
    out = ctx.actions.declare_file("orphan.txt")
    return [DefaultInfo(files = depset([out]))]

unproduced = rule(implementation = _unproduced_impl)

def _duplicate_impl(ctx):
    out = ctx.actions.declare_file("duplicate.txt")
    ctx.actions.write(out, "first")
    ctx.actions.write(out, "second")
    return [DefaultInfo(files = depset([out]))]

duplicate = rule(implementation = _duplicate_impl)

def _foreign_impl(ctx):
    ctx.actions.write(ctx.file.src, "not owned")
    return [DefaultInfo(files = depset([]))]

foreign = rule(implementation = _foreign_impl, attrs = {
    "src": attr.label(allow_single_file = True),
})

def _cycle_impl(ctx):
    a = ctx.actions.declare_file("cycle-a.txt")
    b = ctx.actions.declare_file("cycle-b.txt")
    ctx.actions.run_shell(inputs = [b], outputs = [a], command = 'cat "$1" > "$2"', arguments = [b.path, a.path])
    ctx.actions.run_shell(inputs = [a], outputs = [b], command = 'cat "$1" > "$2"', arguments = [a.path, b.path])
    return [DefaultInfo(files = depset([a]))]

cycle = rule(implementation = _cycle_impl)

def _source_impl(ctx):
    return [DefaultInfo(files = ctx.attr.src[DefaultInfo].files)]

no_source = rule(implementation = _source_impl, attrs = {"src": attr.label(allow_files = False)})

def _single_impl(ctx):
    return [DefaultInfo(files = depset([ctx.file.src]))]

single = rule(implementation = _single_impl, attrs = {"src": attr.label(allow_single_file = True)})
