# Genuine rules_cc 0.2.17 root BUILD (github.com/bazelbuild/rules_cc at tag
# 0.2.17), the subset beyond the previously vendored cc/ subtree: the
# link_extra_lib targets cc_binary's implicit deps label (cc_binary.bzl:24).
load("//cc:cc_library.bzl", "cc_library")

package(default_visibility = ["//visibility:public"])

licenses(["notice"])

exports_files(["LICENSE"])

cc_library(
    name = "empty_lib",
    tags = [
        "__DONT_DEPEND_ON_DEF_PARSER__",
    ],
)

# Label flag for extra libraries to be linked into every binary.
# TODO(bazel-team): Support passing flag multiple times to build a list.
label_flag(
    name = "link_extra_libs",
    build_setting_default = ":empty_lib",
)

# The final extra library to be linked into every binary target. This collects
# the above flag, but may also include more libraries depending on config.
cc_library(
    name = "link_extra_lib",
    tags = [
        "__DONT_DEPEND_ON_DEF_PARSER__",
    ],
    deps = [
        ":link_extra_libs",
    ],
)
