"""rule(provides = [...]) with built-in providers: checkDeclaredProviders
(StarlarkRuleConfiguredTargetUtil.java:138-148) looks every advertised key
up on the built target."""

def _toolchain_impl(_ctx):
    return [platform_common.ToolchainInfo(kind = "created")]

toolchain_rule = rule(
    implementation = _toolchain_impl,
    provides = [platform_common.ToolchainInfo],
)

def _variables_impl(ctx):
    return [platform_common.TemplateVariableInfo({"PROBE": ctx.label.name})]

variables = rule(
    implementation = _variables_impl,
    provides = [platform_common.TemplateVariableInfo],
)

# rules_java's java_host_runtime_alias (toolchains/java_toolchain_alias.bzl):
# a dependency's TemplateVariableInfo and DefaultInfo forwarded, a new
# ToolchainInfo.
def _host_alias_impl(ctx):
    runtime = ctx.attr.runtime
    template_variable_info = runtime[platform_common.TemplateVariableInfo]
    return [
        template_variable_info,
        platform_common.ToolchainInfo(kind = "host_alias"),
        runtime[DefaultInfo],
    ]

host_alias = rule(
    implementation = _host_alias_impl,
    attrs = {
        "runtime": attr.label(
            cfg = "exec",
            providers = [platform_common.TemplateVariableInfo],
        ),
    },
    provides = [
        platform_common.TemplateVariableInfo,
        platform_common.ToolchainInfo,
    ],
)

# rules_java's java_runtime_alias: the resolved toolchain's ToolchainInfo
# forwarded.
def _resolved_alias_impl(ctx):
    return [ctx.toolchains["//provides:probe_type"]]

resolved_alias = rule(
    implementation = _resolved_alias_impl,
    provides = [platform_common.ToolchainInfo],
    toolchains = ["//provides:probe_type"],
)

def _flag_impl(_ctx):
    return [config_common.FeatureFlagInfo(value = "on")]

flag = rule(
    implementation = _flag_impl,
    provides = [config_common.FeatureFlagInfo],
)

def _env_binary_impl(ctx):
    executable = ctx.actions.declare_file(ctx.label.name + ".sh")
    ctx.actions.write(executable, "#!/bin/sh\nexit 0\n", is_executable = True)
    return [
        DefaultInfo(executable = executable),
        RunEnvironmentInfo(environment = {"PROBE": "1"}),
    ]

env_binary = rule(
    implementation = _env_binary_impl,
    executable = True,
    provides = [RunEnvironmentInfo],
)

def _instrumented_impl(ctx):
    return [coverage_common.instrumented_files_info(ctx, source_attributes = ["srcs"])]

instrumented = rule(
    implementation = _instrumented_impl,
    attrs = {"srcs": attr.label_list(allow_files = True)},
    provides = [InstrumentedFilesInfo],
)

def _probe_impl(ctx):
    lines = []
    for dep in ctx.attr.deps:
        name = dep.label.name
        if platform_common.ToolchainInfo in dep:
            lines.append(name + " ToolchainInfo kind=" + dep[platform_common.ToolchainInfo].kind)
        if platform_common.TemplateVariableInfo in dep:
            lines.append(name + " TemplateVariableInfo " + repr(dep[platform_common.TemplateVariableInfo].variables))
        if config_common.FeatureFlagInfo in dep:
            lines.append(name + " FeatureFlagInfo " + repr(dep[config_common.FeatureFlagInfo].value))
        if RunEnvironmentInfo in dep:
            lines.append(name + " RunEnvironmentInfo " + repr(dep[RunEnvironmentInfo].environment))
        if InstrumentedFilesInfo in dep:
            lines.append(name + " InstrumentedFilesInfo")
    out = ctx.actions.declare_file(ctx.label.name + ".txt")
    ctx.actions.write(out, "\n".join(lines) + "\n")
    return [DefaultInfo(files = depset([out]))]

probe = rule(
    implementation = _probe_impl,
    attrs = {"deps": attr.label_list()},
)

def _nothing_impl(_ctx):
    return []

# Advertised but not returned.
missing_toolchain = rule(
    implementation = _nothing_impl,
    provides = [platform_common.ToolchainInfo],
)

missing_variables = rule(
    implementation = _nothing_impl,
    provides = [platform_common.TemplateVariableInfo],
)

missing_flag = rule(
    implementation = _nothing_impl,
    provides = [config_common.FeatureFlagInfo],
)

missing_env = rule(
    implementation = _nothing_impl,
    provides = [RunEnvironmentInfo],
)

missing_instrumented = rule(
    implementation = _nothing_impl,
    provides = [InstrumentedFilesInfo],
)
