"""Top-level declaration patterns adapted from rules_go 0.63
(go/private/rules/transition.bzl, binary.bzl, library.bzl, test.bzl)."""

load(":providers.bzl", "GoInfo")

def _go_transition_impl(settings, attr):
    return {"//command_line_option:platforms": settings["//command_line_option:platforms"]}

go_transition = transition(
    implementation = _go_transition_impl,
    inputs = ["//command_line_option:platforms", "//rules:pure"],
    outputs = ["//command_line_option:platforms"],
)

def _non_request_nogo_transition(_settings, _attr):
    return {"//rules:request_nogo": False}

non_go_transition = transition(
    implementation = _non_request_nogo_transition,
    inputs = [],
    outputs = ["//rules:request_nogo"],
)

def _go_cc_aspect_impl(target, ctx):
    return [OutputGroupInfo(compilation_outputs = depset())]

go_cc_aspect = aspect(
    implementation = _go_cc_aspect_impl,
    attr_aspects = ["deps", "embed"],
)

def _go_binary_impl(ctx):
    return [GoInfo(importpath = ctx.attr.importpath), OutputGroupInfo(cgo_exports = depset())]

def _go_binary_kwargs(test = False):
    return {
        "implementation": _go_binary_impl,
        "attrs": {
            "srcs": attr.label_list(allow_files = [".go", ".s", ".h", ".c"]),
            "data": attr.label_list(allow_files = True, cfg = non_go_transition),
            "deps": attr.label_list(providers = [GoInfo], aspects = [go_cc_aspect]),
            "embed": attr.label_list(providers = [GoInfo], aspects = [go_cc_aspect]),
            "importpath": attr.string(),
            "linkmode": attr.string(default = "normal", values = ["auto", "normal", "pie", "c-shared"]),
            "env": attr.string_dict(),
            "_go_context_data": attr.label(default = "//rules:go_context_data", cfg = "exec"),
            "_stdlib": attr.label(default = Label("//rules:stdlib"), cfg = config.exec("go_link"), executable = True),
        },
        "toolchains": ["//rules:go_toolchain_type", Label("//rules:cgo_toolchain_type")],
        "fragments": ["cpp"],
        "cfg": go_transition,
        "provides": [GoInfo],
        "test": test,
        "executable": not test,
    }

def go_rule(**kwargs):
    # rules_go go/private/rules/binary.bzl: go_binary = go_rule(executable = True, ...)
    return rule(**kwargs)

go_binary = go_rule(**_go_binary_kwargs())
go_test = rule(**_go_binary_kwargs(test = True))

go_library = rule(
    _go_binary_impl,
    attrs = {
        "srcs": attr.label_list(allow_files = True),
        "importpath": attr.string(),
        "deps": attr.label_list(providers = [[GoInfo], [OutputGroupInfo]]),
    },
    toolchains = ["//rules:go_toolchain_type"],
)

def go_select_macro(name, **kwargs):
    # A legacy macro combining select() with plain lists (rules_go def.bzl style).
    go_library(
        name = name,
        srcs = ["lib.go"] + select({
            "//rules:linux": ["linux.go"],
            "//conditions:default": [],
        }),
        importpath = select({"//conditions:default": "example.com/x"}, no_match_error = "unsupported"),
        **kwargs
    )
