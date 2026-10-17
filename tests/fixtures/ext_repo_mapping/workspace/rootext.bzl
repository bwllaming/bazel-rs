load("@dep//:probe.bzl", "probe_source")

def _rp(rctx):
    rctx.file("defs.bzl", probe_source("rp", [
        "@//:x",
        "@fx//:x",
        "@hub//:x",
        "@dep//:x",
        "@rp//:x",
        "@rr1//:x",
        "@platforms//:x",
        "@x_linux_amd64//:x",
    ]))
    rctx.file("BUILD.bazel", 'load(":defs.bzl", "probe")\n')

rp = repository_rule(_rp)

def _rootext(mctx):
    rp(name = "rp")

rootext = module_extension(_rootext)
