# --explicit_java_test_deps (rules_jvm_external's .bazelrc), read through
# the private ctx.fragments.java getter (this package is on its allowlist).
def _impl(ctx):
    out = ctx.actions.declare_file(ctx.label.name + ".txt")
    ctx.actions.write(out, repr(ctx.fragments.java.enforce_explicit_java_test_deps()) + "\n")
    return [DefaultInfo(files = depset([out]))]

show = rule(implementation = _impl, fragments = ["java"])
