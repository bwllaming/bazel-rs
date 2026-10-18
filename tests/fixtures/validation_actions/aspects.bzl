"""Genuine-only aspect phase (N56): aspects are not modeled by the native
profile, so nothing here is analyzed natively; the capture records genuine
Bazel 9.3's behavior for documentation (an aspect's `_validation` outputs
run with the target it is applied to through --aspects)."""

def _val_aspect_impl(target, ctx):
    out = ctx.actions.declare_file(target.label.name + ".aspect_validated")
    ctx.actions.write(out, "aspect")
    return [OutputGroupInfo(_validation = depset([out]))]

val_aspect = aspect(implementation = _val_aspect_impl)
