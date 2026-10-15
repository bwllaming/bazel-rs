licenses(["notice"])

package(
    default_visibility = ["//visibility:public"],
)

filegroup(
    name = "srcs",
    srcs = glob(["**"]),
)

constraint_setting(name = "os")

constraint_value(
    name = "linux",
    constraint_setting = ":os",
)

constraint_value(
    name = "osx",
    constraint_setting = ":os",
)

alias(
    name = "macos",
    actual = ":osx",
)

constraint_value(
    name = "windows",
    constraint_setting = ":os",
)
