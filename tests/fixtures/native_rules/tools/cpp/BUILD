# Shapes of @bazel_tools//tools/cpp:BUILD and a local_config_cc BUILD.
package(default_visibility = ["//visibility:public"])

licenses(["notice"])

toolchain_type(name = "toolchain_type")

toolchain_type(name = "cc_runtimes_toolchain_type")

[
    alias(
        name = target,
        actual = actual,
        deprecation = "Please use %s instead." % actual,
    )
    for target, actual in {
        "compiler": "//tools/cpp:empty_lib",
        "current_libc_top": ":libc_top",
    }.items()
]

filegroup(
    name = "empty_lib",
    tags = ["__DONT_DEPEND_ON_DEF_PARSER__"],
)

label_flag(
    name = "link_extra_libs",
    build_setting_default = ":empty_lib",
)

label_flag(
    name = "link_extra_lib",
    build_setting_default = "//tools/cpp:empty_lib",
)

cc_libc_top_alias(name = "libc_top")

cc_toolchain_suite(
    name = "toolchain",
    toolchains = {
        "k8|compiler": ":cc-compiler-k8",
        "k8": ":cc-compiler-k8",
    },
    tags = ["manual"],
)

filegroup(
    name = "compiler_deps",
    srcs = glob(["builtin_include_directory_paths*"], allow_empty = True),
)
