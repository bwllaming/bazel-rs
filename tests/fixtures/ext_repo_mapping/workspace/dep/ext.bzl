load(":probe.bzl", "probe_source")

def _plat(rctx):
    print("N45|fetch|" + rctx.original_name)
    rctx.file("BUILD.bazel", 'load("@dep//:rules.bzl", "tool")\ntool(name = "impl", v = "%s", visibility = ["//visibility:public"])\n' % rctx.original_name)

plat = repository_rule(_plat)

_TOOLCHAIN = 'toolchain(name = "{p}_toolchain", exec_compatible_with = ["@platforms//cpu:{cpu}"], toolchain = "@x_{p}//:impl", toolchain_type = "@dep//:tt")\n'

def _hub(rctx):
    build = ""
    # Like rules_oci's toolchains_repo: every platform gets a toolchain, even
    # one whose per-platform repository the extension does not create.
    for p, cpu in [("linux_amd64", "x86_64"), ("linux_armv6", "arm64"), ("unused", "s390x")]:
        build += _TOOLCHAIN.format(p = p, cpu = cpu)
    build += 'config_setting(name = "cfg", values = {"compilation_mode": "fastbuild"}, visibility = ["//visibility:public"])\n'
    rctx.file("BUILD.bazel", build)

hub = repository_rule(_hub)

def _probe_repo(rctx):
    rctx.file("defs.bzl", probe_source(rctx.attr.prefix, rctx.attr.texts))
    build = 'load(":defs.bzl", "probe")\nload("@dep//:rules.bzl", "holder")\n'
    build += 'holder(name = "ok", deps = ["@x_linux_amd64//:impl"])\n'
    build += 'holder(name = "mixed", deps = ["@x_linux_amd64//:impl"], lazy = ["@x_linux_armv6//:impl", "@nope//:x"])\n'
    build += 'holder(name = "bad", deps = ["@x_linux_armv6//:impl"])\n'
    build += 'filegroup(name = "fg", srcs = ["@nope//:x"])\n'
    rctx.file("BUILD.bazel", build)
    rctx.file("getter/defs.bzl", 'print("N45|getter|" + Label("@x_linux_armv6//:impl").%s)\n' % rctx.attr.getter)
    rctx.file("getter/BUILD.bazel", 'load(":defs.bzl", "unused")\n')

probe_repo = repository_rule(_probe_repo, attrs = {"prefix": attr.string(), "texts": attr.string_list(), "getter": attr.string(default = "repo_name")})

_PROBE_TEXTS = [
    "@x_linux_amd64//:impl",
    "@probe//:x",
    "@hub//:x",
    "@platforms//:x",
    "@dep//:x",
    "@fx//:x",
    "@x_linux_armv6//:impl",
    "@//:x",
    "@@dep++ext+hub//:x",
    "@@nope+//:x",
    "//:x",
    ":x",
]

def _ext(mctx):
    plat(name = "x_linux_amd64")
    plat(name = "x_unused")
    hub(name = "hub")
    probe_repo(name = "probe", prefix = "probe", texts = _PROBE_TEXTS)

ext = module_extension(_ext)

def _ext2(mctx):
    # A generated name equal to an apparent name of the hosting module's
    # mapping: inside the extension's repositories the generated one wins.
    plat(name = "platforms")
    probe_repo(name = "probe2", prefix = "probe2", texts = ["@platforms//:x", "@dep//:x", "@probe2//:x", "@probe//:x"])

ext2 = module_extension(_ext2)

def _probe_rule(rctx):
    rctx.file("defs.bzl", probe_source(rctx.original_name, [
        "@%s//:x" % rctx.attr.peer,
        "@hub//:x",
        "@//:x",
        "@fx//:x",
        "@dep//:x",
        "@platforms//:x",
        "@ext//:x",
    ]))
    rctx.file("BUILD.bazel", 'load(":defs.bzl", "probe")\n')

probe_rule = repository_rule(_probe_rule, attrs = {"peer": attr.string()})
