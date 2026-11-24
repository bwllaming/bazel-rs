"""What Starlark rules see of the host-dependent configuration."""

def _xcode_lines(info):
    pt = apple_common.platform_type
    p = apple_common.platform
    lines = ["xcode type=%s dir=%s" % (type(info), dir(info))]
    for t in dir(pt):
        lines.append("xcode minimum_os_for_platform_type(%s)=%s" % (
            t,
            info.minimum_os_for_platform_type(getattr(pt, t)),
        ))
    for n in dir(p):
        lines.append("xcode sdk_version_for_platform(%s)=%s" % (
            n,
            info.sdk_version_for_platform(getattr(p, n)),
        ))
    lines.append("xcode availability=%r xcode_version=%r" % (info.availability(), info.xcode_version()))
    lines.append("xcode execution_info=%r" % sorted(info.execution_info().items()))
    return lines

def _probe_impl(ctx):
    apple = ctx.fragments.apple
    cpp = ctx.fragments.cpp
    xcode = ctx.attr._xcode_config[apple_common.XcodeVersionConfig]
    constraints = {
        name: ctx.target_platform_has_constraint(target[platform_common.ConstraintValueInfo])
        for name, target in [
            ("macos", ctx.attr._macos),
            ("linux", ctx.attr._linux),
            ("arm64", ctx.attr._arm64),
            ("x86_64", ctx.attr._x86_64),
        ]
    }
    items = [
        ("label", str(ctx.label)),
        ("bin_dir", ctx.bin_dir.path),
        ("genfiles_dir", ctx.genfiles_dir.path),
        ("var", sorted(ctx.var.items())),
        ("host_path_separator", ctx.configuration.host_path_separator),
        ("default_shell_env", sorted(ctx.configuration.default_shell_env.items())),
        ("coverage_enabled", ctx.configuration.coverage_enabled),
        ("is_tool_configuration", ctx.configuration.is_tool_configuration()),
        ("target_platform_has_constraint", sorted(constraints.items())),
        ("platform.platform", str(ctx.fragments.platform.platform)),
        ("platform.host_platform", str(ctx.fragments.platform.host_platform)),
        ("sh_toolchain.path", ctx.toolchains["@bazel_tools//tools/sh:toolchain_type"].path),
        ("apple.single_arch_cpu", apple.single_arch_cpu),
        ("apple.single_arch_platform", repr(apple.single_arch_platform)),
        ("apple.single_arch_platform.name_in_plist", apple.single_arch_platform.name_in_plist),
        ("apple.single_arch_platform.is_device", apple.single_arch_platform.is_device),
        ("apple.apple_platform_type", apple.apple_platform_type),
        ("apple.apple_cpus", repr(apple.apple_cpus)),
        ("apple.xcode_version_flag", apple.xcode_version_flag),
        ("apple.macos_sdk_version_flag", str(apple.macos_sdk_version_flag)),
        ("apple.macos_minimum_os_flag", str(apple.macos_minimum_os_flag)),
        ("apple.prefer_mutual_xcode", apple.prefer_mutual_xcode),
        ("apple.include_xcode_exec_requirements", apple.include_xcode_exec_requirements),
        ("cpp.copts", cpp.copts),
        ("cpp.cxxopts", cpp.cxxopts),
        ("cpp.conlyopts", cpp.conlyopts),
        ("cpp.linkopts", cpp.linkopts),
        ("apple_host_system_env", sorted(apple_common.apple_host_system_env(xcode).items())),
        ("target_apple_env", sorted(apple_common.target_apple_env(xcode, apple.single_arch_platform).items())),
    ]
    lines = ["%s=%r" % item for item in items] + _xcode_lines(xcode)
    out = ctx.actions.declare_file(ctx.label.name + ".txt")
    ctx.actions.write(out, "\n".join(lines) + "\n")
    return [DefaultInfo(files = depset([out]))]

probe = rule(
    implementation = _probe_impl,
    attrs = {
        "_xcode_config": attr.label(default = configuration_field(fragment = "apple", name = "xcode_config_label")),
        "_macos": attr.label(default = "@platforms//os:macos"),
        "_linux": attr.label(default = "@platforms//os:linux"),
        "_arm64": attr.label(default = "@platforms//cpu:arm64"),
        "_x86_64": attr.label(default = "@platforms//cpu:x86_64"),
    },
    fragments = ["apple", "cpp", "platform"],
    toolchains = ["@bazel_tools//tools/sh:toolchain_type"],
)

def _exec_probe_impl(ctx):
    # The probe's report in the exec configuration, copied to this target's.
    src = ctx.file.dep
    out = ctx.actions.declare_file(ctx.label.name + ".txt")
    ctx.actions.run_shell(
        inputs = [src],
        outputs = [out],
        command = "{ echo \"exec_path=$1\"; cat \"$1\"; } > \"$2\"",
        arguments = [src.path, out.path],
    )
    return [DefaultInfo(files = depset([out]))]

exec_probe = rule(
    implementation = _exec_probe_impl,
    attrs = {"dep": attr.label(cfg = "exec", allow_single_file = True)},
)
