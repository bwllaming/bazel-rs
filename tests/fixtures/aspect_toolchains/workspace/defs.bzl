"""Aspect toolchains, exec groups and fragments (docs/ASPECTS.md "Aspect toolchains").

An aspect resolves its OWN toolchain context in the base target's
configuration (AspectFunction.getUnloadedToolchainContextsInputs): its
`toolchains`, `exec_compatible_with` and `exec_groups` (automatic exec groups
with `_use_auto_exec_groups`), never the base rule's; `ctx.toolchains` and
`ctx.exec_groups` read it, and `fragments` is its configuration fragment
policy (`host_fragments` is ignored by Bazel 9.3).
"""

def _tc_impl(ctx):
    return [platform_common.ToolchainInfo(tc_name = ctx.attr.tc_name)]

my_toolchain = rule(_tc_impl, attrs = {"tc_name": attr.string()})

def _tc_aspect_impl(target, ctx):
    print("TC {} t={} optional={} context={}".format(
        ctx.label.name,
        ctx.toolchains["//tc:t"].tc_name,
        ctx.toolchains["//tc:optional_t"],
        ctx.toolchains,
    ))
    print("EG {} g={} has_g={} copts={}".format(
        ctx.label.name,
        ctx.exec_groups["g"].toolchains["//tc:t2"].tc_name,
        "g" in ctx.exec_groups,
        ctx.fragments.cpp.copts,
    ))
    default = ctx.actions.declare_file(ctx.label.name + ".default")
    ctx.actions.run_shell(outputs = [default], command = "echo default > " + default.path, toolchain = "//tc:t")
    group = ctx.actions.declare_file(ctx.label.name + ".group")
    ctx.actions.run_shell(outputs = [group], command = "echo group > " + group.path, exec_group = "g")
    transitive = [dep[OutputGroupInfo].tc_out for dep in ctx.rule.attr.deps]
    return [OutputGroupInfo(tc_out = depset([default, group], transitive = transitive))]

tc_aspect = aspect(
    implementation = _tc_aspect_impl,
    attr_aspects = ["deps"],
    toolchains = [
        "//tc:t",
        config_common.toolchain_type("//tc:optional_t", mandatory = False),
    ],
    exec_groups = {"g": exec_group(toolchains = ["//tc:t2"])},
    exec_compatible_with = ["@platforms//os:linux"],
    fragments = ["cpp"],
)

def _auto_aspect_impl(target, ctx):
    print("AUTO {} t={} group={}".format(
        ctx.label.name,
        ctx.toolchains["//tc:t"].tc_name,
        ctx.exec_groups["//tc:t"].toolchains["//tc:t"].tc_name,
    ))
    out = ctx.actions.declare_file(ctx.label.name + ".auto")
    ctx.actions.run_shell(outputs = [out], command = "echo auto > " + out.path, toolchain = "//tc:t")
    return [OutputGroupInfo(auto_out = depset([out]))]

auto_aspect = aspect(
    implementation = _auto_aspect_impl,
    toolchains = ["//tc:t"],
    attrs = {"_use_auto_exec_groups": attr.bool(default = True)},
)

def _cpp_impl(target, ctx):
    print("NOFRAG {}".format(ctx.fragments.cpp.copts))
    return []

nofrag_aspect = aspect(implementation = _cpp_impl)

def _java_impl(target, ctx):
    print("HOSTFRAG {}".format(ctx.fragments.java))
    return []

hostfrag_aspect = aspect(implementation = _java_impl, host_fragments = ["java"])

def _missing_impl(target, ctx):
    return []

missing_aspect = aspect(implementation = _missing_impl, toolchains = ["//tc:missing_t"])

def _plain_impl(target, ctx):
    print("PLAIN {}".format(ctx.toolchains))
    return [ctx.toolchains["//tc:t"]]

plain_aspect = aspect(implementation = _plain_impl)

def _lib_impl(ctx):
    return [DefaultInfo()]

lib = rule(_lib_impl, attrs = {"deps": attr.label_list()})

def _use_impl(ctx):
    return [DefaultInfo(files = depset(transitive = [dep[OutputGroupInfo].tc_out for dep in ctx.attr.deps]))]

use = rule(_use_impl, attrs = {"deps": attr.label_list(aspects = [tc_aspect])})
