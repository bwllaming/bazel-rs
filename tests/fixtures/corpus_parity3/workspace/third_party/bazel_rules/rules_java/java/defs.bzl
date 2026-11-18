# B35 (rules_kotlin through rules_java's java_common.bzl:98-162):
# java_common_internal._check_java_toolchain_is_declared_on_rule(actions),
# reachable from this allowlisted package.
_JAVA = "@bazel_tools//tools/jdk:toolchain_type"

def _impl(ctx):
    java_common.internal_DO_NOT_USE().check_java_toolchain_is_declared_on_rule(ctx.actions)
    out = ctx.actions.declare_file(ctx.label.name + ".txt")
    ctx.actions.write(out, "declared\n")
    return [DefaultInfo(files = depset([out]))]

declared = rule(implementation = _impl, toolchains = [_JAVA])

optional = rule(
    implementation = _impl,
    toolchains = [config_common.toolchain_type(_JAVA, mandatory = False)],
)

undeclared = rule(implementation = _impl)

def _fake_impl(ctx):
    return [platform_common.ToolchainInfo(name = "fake")]

fake = rule(implementation = _fake_impl)
