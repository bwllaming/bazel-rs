"""Probe: Bazel 9.3 has no Starlark-visible ValidationProvider (validation
outputs are only the `_validation` output group, OutputGroupInfo.VALIDATION).
Referencing the name must fail identically in genuine Bazel and the native
profile: a Starlark name-resolution error at load time."""

def _make_probe(ctx):
    out = ctx.actions.declare_file(ctx.label.name + ".out")
    ctx.actions.write(out, "probe")
    return [DefaultInfo(files = depset([out]))]

_validation_outputs = [ValidationProvider]

make_probe = rule(implementation = _make_probe)
