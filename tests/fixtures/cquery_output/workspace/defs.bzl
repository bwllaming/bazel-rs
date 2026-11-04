MyInfo = provider(fields = ["value"])

def _flag_impl(ctx):
    return []

string_flag = rule(implementation = _flag_impl, build_setting = config.string(flag = True))

def _set_flag_impl(settings, attr):
    return {"//:flag": "transitioned"}

set_flag = transition(implementation = _set_flag_impl, inputs = [], outputs = ["//:flag"])

def _aspect_impl(target, ctx):
    return []

extra_aspect = aspect(
    implementation = _aspect_impl,
    attrs = {"_extra": attr.label(default = "//:extra.txt", allow_files = True)},
)

def _lib_impl(ctx):
    out = ctx.actions.declare_file(ctx.label.name + ".out")
    ctx.actions.write(out, "x")
    return [
        DefaultInfo(files = depset([out] + ctx.files.srcs)),
        MyInfo(value = ctx.label.name),
        OutputGroupInfo(extra = depset(ctx.files.srcs)),
    ]

lib = rule(
    implementation = _lib_impl,
    attrs = {
        "srcs": attr.label_list(allow_files = True),
        "deps": attr.label_list(),
        "tool": attr.label(cfg = "exec", allow_files = True),
        "tr_dep": attr.label(cfg = set_flag),
        "aspect_dep": attr.label(aspects = [extra_aspect]),
        "_allowlist_function_transition": attr.label(default = "//tools/allowlists/function_transition_allowlist"),
    },
)

def _tc_impl(ctx):
    return [platform_common.ToolchainInfo(name = ctx.label.name)]

tc_rule = rule(implementation = _tc_impl)

def _uses_tc_impl(ctx):
    return [DefaultInfo()]

uses_tc = rule(implementation = _uses_tc_impl, toolchains = ["//:tc_type"])
