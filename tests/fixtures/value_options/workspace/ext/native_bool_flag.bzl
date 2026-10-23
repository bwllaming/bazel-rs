# protobuf 33.4's bazel/private/native_bool_flag.bzl (a helper rule that
# reads a native boolean flag), with a local BuildSettingInfo instead of
# @bazel_skylib//rules:common_settings.bzl's.

BuildSettingInfo = provider(fields = ["value"])

def _impl(ctx):
    return [BuildSettingInfo(value = ctx.attr.value)]

_native_bool_flag_rule = rule(
    implementation = _impl,
    attrs = {"value": attr.bool()},
)

def native_bool_flag(*, name, flag, match_value = "true", result = True, **kwargs):
    _native_bool_flag_rule(
        name = name,
        value = select({
            name + "_setting": result,
            "//conditions:default": not result,
        }),
        **kwargs
    )

    native.config_setting(
        name = name + "_setting",
        values = {flag: match_value},
        visibility = ["//visibility:private"],
    )
