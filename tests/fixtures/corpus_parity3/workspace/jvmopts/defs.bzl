# B35: rules_kotlin's kt_jvm_binary extends ctx.fragments.java.default_jvm_opts
# (a Java ImmutableList: each read is a fresh mutable copy).
def _impl(ctx):
    jvm = ctx.fragments.java.default_jvm_opts
    jvm.extend(["-Xextra"])
    javac = ctx.fragments.java.default_javac_flags
    javac.append("-Xlint")
    again = ctx.fragments.java.default_jvm_opts
    out = ctx.actions.declare_file(ctx.label.name + ".txt")
    ctx.actions.write(out, "\n".join([repr(jvm), repr(javac), repr(again), repr(jvm == again)]) + "\n")
    return [DefaultInfo(files = depset([out]))]

show = rule(implementation = _impl, fragments = ["java"])
