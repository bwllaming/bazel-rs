licenses(["notice"])

package(
    default_visibility = ["//visibility:public"],
)

filegroup(
    name = "srcs",
    srcs = glob(["**"]),
)

constraint_setting(name = "cpu")

constraint_value(
    name = "x86_64",
    constraint_setting = ":cpu",
)

constraint_value(
    name = "aarch64",
    constraint_setting = ":cpu",
)

alias(
    name = "arm64",
    actual = ":aarch64",
)
