"""Top-level declaration patterns adapted from rules_cc 0.2.17
(cc/private/rules_impl/cc_binary.bzl, cc_test.bzl, cc/cc_postmark.bzl)."""

load(":providers.bzl", "CcLikeInfo")

def _postmark_initializer(**kwargs):
    return kwargs

def _dynamic_deps_initializer(**kwargs):
    if kwargs.get("linkstatic") == None:
        kwargs["linkstatic"] = True
    return kwargs

def _cc_binary_initializer(**kwargs):
    kwargs = _postmark_initializer(**kwargs)
    return _dynamic_deps_initializer(**kwargs)

def _impl(ctx):
    return [CcLikeInfo(headers = []), DefaultInfo()]

cc_binary_attrs = {
    "srcs": attr.label_list(allow_files = [".cc", ".h"], flags = ["DIRECT_COMPILE_TIME_INPUT"]),
    "deps": attr.label_list(providers = [CcLikeInfo]),
    "linkstatic": attr.bool(default = False),
    "malloc": attr.label(default = "//rules:malloc", allow_rules = ["cc_library"]),
    "stamp": attr.int(default = -1, values = [-1, 0, 1]),
    "win_def_file": attr.label(allow_single_file = [".def"]),
    "additional_linker_inputs": attr.label_list(allow_files = True, flags = ["ORDER_INDEPENDENT"]),
    "link_extra_lib": attr.label(default = "//rules:link_extra_lib"),
}

cc_binary = rule(
    implementation = _impl,
    initializer = _cc_binary_initializer,
    doc = "It produces an executable binary.",
    attrs = cc_binary_attrs,
    outputs = {
        "dwp_file": "%{name}.dwp",
        "stripped_binary": "%{name}.stripped",
    },
    fragments = ["cpp"],
    toolchains = ["//rules:cc_toolchain_type"],
    provides = [CcLikeInfo],
    executable = True,
)

cc_test = rule(
    implementation = _impl,
    initializer = _cc_binary_initializer,
    attrs = dict(cc_binary_attrs, **{
        "env_inherit": attr.string_list(),
        "_lcov_merger": attr.label(default = "//rules:lcov_merger", cfg = config.exec(), executable = True),
    }),
    fragments = ["cpp", "coverage"],
    host_fragments = ["cpp"],
    exec_compatible_with = ["//rules:linux"],
    test = True,
)
