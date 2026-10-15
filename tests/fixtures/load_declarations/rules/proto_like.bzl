"""Top-level declaration patterns adapted from protobuf (bazel/private/bazel_cc_proto_library.bzl,
bazel_java_proto_library_rule.bzl) and rules_java/rules_python attribute shapes."""

load(":providers.bzl", "CcLikeInfo", "ProtoInfo")

def _aspect_impl(target, ctx):
    return [CcLikeInfo(headers = []), OutputGroupInfo(temp_files_INTERNAL_ = depset())]

cc_proto_aspect = aspect(
    implementation = _aspect_impl,
    attr_aspects = ["deps"],
    fragments = ["cpp", "proto"],
    required_providers = [ProtoInfo],
    provides = [CcLikeInfo],
    attrs = {
        "_aspect_cc_proto_toolchain": attr.label(default = "//rules:cc_proto_toolchain"),
        "mode": attr.string(default = "lite", values = ["lite", "full"]),
    },
    toolchains = ["//rules:proto_toolchain_type"],
)

java_proto_aspect = aspect(
    _aspect_impl,
    ["deps", "exports"],
    requires = [cc_proto_aspect],
    required_aspect_providers = [[CcLikeInfo]],
    doc = "java proto aspect",
)

def _cc_proto_library_impl(ctx):
    return [DefaultInfo()]

cc_proto_library = rule(
    implementation = _cc_proto_library_impl,
    attrs = {
        "deps": attr.label_list(aspects = [cc_proto_aspect], providers = [ProtoInfo], allow_rules = ["proto_library"]),
    },
    provides = [CcLikeInfo],
)

def _java_impl(ctx):
    return []

java_import_like = rule(
    implementation = _java_impl,
    attrs = {
        "jars": attr.label_list(allow_files = [".jar"], mandatory = True, flags = ["DIRECT_COMPILE_TIME_INPUT"]),
        "javacopts": attr.string_list(allow_empty = True),
        "exports": attr.label_list(allow_empty = False),
        "constraints": attr.label_keyed_string_dict(allow_files = True),
        "neverlink": attr.bool(),
        "jvm_flags_index": attr.int_list(default = [1, 2]),
        "out": attr.output(),
        "extra_outs": attr.output_list(allow_empty = True),
        "_java_runtime": attr.label(default = "//rules:jdk", cfg = config.target()),
        "_data_only": attr.label(default = "//rules:data", cfg = config.none()),
        "_launcher": attr.label(default = "//rules:launcher", cfg = "exec", executable = True, allow_single_file = True),
    },
    fragments = ["java"],
)

def _py_transition_impl(settings, attr):
    return {"//command_line_option:python_version": "PY3"}

python_version_transition = transition(
    implementation = _py_transition_impl,
    inputs = [],
    outputs = ["//command_line_option:python_version"],
)

def _py_binary_impl(ctx):
    return []

py_binary_like = rule(
    implementation = _py_binary_impl,
    attrs = {
        "srcs": attr.label_list(allow_files = [".py"]),
        "python_version": attr.string(default = "PY3", values = ["PY2", "PY3"]),
        "_launcher": attr.label(default = "//rules:py_launcher", cfg = python_version_transition),
    },
    executable = True,
    cfg = python_version_transition,
)
