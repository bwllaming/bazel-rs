# B4: the android_common toplevel rules_android re-exports at load time
# (rules/android_common/reexport_android_common.bzl:23).
native_android_common = android_common

def _impl(ctx):
    out = ctx.actions.declare_file(ctx.label.name + ".txt")
    ctx.actions.write(out, repr(dir(native_android_common)) + "\n")
    return [DefaultInfo(files = depset([out]))]

show = rule(implementation = _impl)
