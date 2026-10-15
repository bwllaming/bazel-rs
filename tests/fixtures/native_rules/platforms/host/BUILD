load(":constraints.bzl", "HOST_CONSTRAINTS")

package(default_visibility = ["//visibility:public"])

exports_files(["constraints.bzl"])

filegroup(
    name = "srcs",
    srcs = glob(["**"]),
)

platform(
    name = "host",
    constraint_values = HOST_CONSTRAINTS,
)
