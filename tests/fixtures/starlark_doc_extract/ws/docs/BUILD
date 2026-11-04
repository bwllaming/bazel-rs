SKYLIB_LIB = [
    "@bazel_skylib//lib:collections",
    "@bazel_skylib//lib:dicts",
    "@bazel_skylib//lib:partial",
    "@bazel_skylib//lib:paths",
    "@bazel_skylib//lib:selects",
    "@bazel_skylib//lib:sets",
    "@bazel_skylib//lib:shell",
    "@bazel_skylib//lib:structs",
    "@bazel_skylib//lib:types",
    "@bazel_skylib//lib:versions",
    "@bazel_skylib//lib:unittest",
]

SKYLIB_RULES = [
    "@bazel_skylib//:bzl_library",
    "@bazel_skylib//rules:analysis_test",
    "@bazel_skylib//rules:build_test",
    "@bazel_skylib//rules:common_settings",
    "@bazel_skylib//rules:copy_directory",
    "@bazel_skylib//rules:copy_file",
    "@bazel_skylib//rules:diff_test",
    "@bazel_skylib//rules:expand_template",
    "@bazel_skylib//rules:native_binary",
    "@bazel_skylib//rules:run_binary",
    "@bazel_skylib//rules:select_file",
    "@bazel_skylib//rules:write_file",
]

starlark_doc_extract(name = "skylib_lib", src = "skylib_lib.bzl", deps = SKYLIB_LIB)

starlark_doc_extract(name = "skylib_rules", src = "skylib_rules.bzl", deps = SKYLIB_RULES)

starlark_doc_extract(
    name = "skylib_rules_main",
    src = "skylib_rules.bzl",
    render_main_repo_name = True,
    deps = SKYLIB_RULES,
)
