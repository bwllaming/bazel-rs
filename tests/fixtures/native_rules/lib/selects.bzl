# Adapted from bazel_skylib lib/selects.bzl _config_setting_or_group
# (bazel_skylib 1.8.2, Apache-2.0): the sequential alias chain.
def config_setting_group(name, match_any, visibility = None):
    settings = match_any
    if len(settings) == 1:
        native.alias(
            name = name,
            actual = settings[0],
            visibility = visibility,
        )
        return

    actual = []
    for i in range(2, len(settings) + 1):
        actual.append(name + "_" + str(i))
    actual.append(settings[-1])

    for i in range(1, len(settings) + 1):
        native.alias(
            name = name if i == 1 else name + "_" + str(i),
            actual = select({
                native.package_relative_label(settings[i - 1]): settings[i - 1],
                "//conditions:default": actual[i - 1],
            }),
            visibility = visibility if i == 1 else ["//visibility:private"],
        )
