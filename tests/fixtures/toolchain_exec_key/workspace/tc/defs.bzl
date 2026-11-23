# B13c: one toolchain implementation resolved for two execution platforms in
# one configuration (ConfiguredTargetKey.executionPlatformLabel).
ExecPlatformInfo = provider(fields = ["label"])

def _exec_platform_impl(ctx):
    return [ExecPlatformInfo(label = ctx.fragments.platform.platform)]

exec_platform = rule(implementation = _exec_platform_impl, fragments = ["platform"])

def _my_toolchain_impl(ctx):
    platform = ctx.attr._exec_platform[ExecPlatformInfo].label
    stamp = ctx.actions.declare_file("%s/%s.stamp" % (ctx.label.name, platform.name))
    ctx.actions.run_shell(
        outputs = [stamp],
        command = "echo $2 > $1",
        arguments = [stamp.path, str(platform)],
        mnemonic = "Stamp",
    )
    files = [stamp]
    if ctx.attr.clash:
        # Equal on both execution platforms, but the execution platform is
        # part of the action key: the two copies conflict.
        clash = ctx.actions.declare_file("%s.clash" % ctx.label.name)
        ctx.actions.write(clash, "same\n")
        files.append(clash)
    return [
        platform_common.ToolchainInfo(stamp = stamp, platform = platform, files = depset(files)),
        DefaultInfo(files = depset(files)),
    ]

my_toolchain = rule(
    implementation = _my_toolchain_impl,
    attrs = {
        "clash": attr.bool(),
        "_exec_platform": attr.label(default = "//tc:exec_platform", cfg = "exec"),
    },
)

def _use_impl(ctx):
    info = ctx.toolchains["//tc:type"]
    out = ctx.actions.declare_file(ctx.label.name + ".txt")
    ctx.actions.write(out, "%s %s\n" % (info.platform, info.stamp.path))
    return [DefaultInfo(files = depset([out], transitive = [info.files]))]

use_tc = rule(implementation = _use_impl, toolchains = ["//tc:type"])

def _use_clash_impl(ctx):
    info = ctx.toolchains["//tc:clash_type"]
    return [DefaultInfo(files = info.files)]

use_clash = rule(implementation = _use_clash_impl, toolchains = ["//tc:clash_type"])

def _use_alias_impl(ctx):
    info = ctx.toolchains["//tc:alias_type"]
    out = ctx.actions.declare_file(ctx.label.name + ".txt")
    ctx.actions.write(out, "%s %s\n" % (info.platform, info.stamp.path))
    return [DefaultInfo(files = depset([out], transitive = [info.files]))]

use_alias = rule(implementation = _use_alias_impl, toolchains = ["//tc:alias_type"])

# A toolchain implementation with an incoming rule transition to a
# configuration whose first execution platform is //tc:eb: its delegate key
# for //tc:eb is that configuration's plain key, which a toolchain edge in
# that configuration reaches too (one configured target).
def _to_eb_impl(_settings, _attr):
    return {"//command_line_option:extra_execution_platforms": ["//tc:eb", "//tc:ea"]}

to_eb = transition(
    implementation = _to_eb_impl,
    inputs = [],
    outputs = ["//command_line_option:extra_execution_platforms"],
)

my_toolchain_t = rule(
    implementation = _my_toolchain_impl,
    cfg = to_eb,
    attrs = {
        "clash": attr.bool(),
        "_exec_platform": attr.label(default = "//tc:exec_platform", cfg = "exec"),
    },
)

def _use_t_impl(ctx):
    info = ctx.toolchains["//tc:t_type"]
    return [DefaultInfo(files = info.files)]

use_t = rule(implementation = _use_t_impl, toolchains = ["//tc:t_type"])

use_t_in_eb = rule(implementation = _use_t_impl, toolchains = ["//tc:t_type"], cfg = to_eb)
