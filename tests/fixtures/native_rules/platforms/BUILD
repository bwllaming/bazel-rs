# Shape of @platforms//:BUILD (platforms 1.1.0): package metadata defaults and
# a purl built from module_name()/module_version().
package(
    default_applicable_licenses = [":license"],
    default_visibility = ["//visibility:public"],
)

filegroup(
    name = "license",
    srcs = ["LICENSE"],
    applicable_licenses = [],
)

constraint_setting(
    name = "purl_" + ("pkg:bazel/{}@{}".format(
        module_name(),
        module_version(),
    ) if module_version() else "pkg:bazel/{}".format(module_name())).replace(":", "_").replace("/", "_").replace("@", "_"),
)

exports_files([
    "LICENSE",
    "MODULE.bazel",
])

filegroup(
    name = "srcs",
    srcs = [
        "BUILD",
        "//cpu:srcs",
        "//os:srcs",
        "//host:srcs",
    ],
)

constraint_setting(name = "incompatible_setting")

constraint_value(
    name = "incompatible",
    constraint_setting = ":incompatible_setting",
)
