# Shape of rules_python python/private/py_toolchain_suite.bzl
# (rules_python, Apache-2.0): toolchain(target_settings = select(...)).
_IS_EXEC_TOOLCHAIN_ENABLED = Label("//python:exec_tools_toolchain_enabled")

def py_toolchain_suite(prefix, target_settings, target_compatible_with):
    native.toolchain(
        name = "{prefix}_toolchain".format(prefix = prefix),
        toolchain = ":{prefix}_runtime".format(prefix = prefix),
        toolchain_type = "//python:toolchain_type",
        target_settings = target_settings,
        target_compatible_with = target_compatible_with,
    )

    native.toolchain(
        name = "{prefix}_py_exec_tools_toolchain".format(prefix = prefix),
        toolchain = ":{prefix}_exec_tools".format(prefix = prefix),
        toolchain_type = "//python:exec_tools_toolchain_type",
        target_settings = select({
            _IS_EXEC_TOOLCHAIN_ENABLED: target_settings,
            "//conditions:default": [_IS_EXEC_TOOLCHAIN_ENABLED],
        }),
        exec_compatible_with = target_compatible_with,
    )
