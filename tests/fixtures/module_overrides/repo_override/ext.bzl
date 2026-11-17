def _plain(rctx):
    print("OVR|fetch|" + rctx.original_name + "|" + rctx.attr.tag)
    rctx.file("BUILD.bazel", 'filegroup(name = "x", visibility = ["//visibility:public"])\n')
    rctx.file("defs.bzl", "")

plain = repository_rule(_plain, attrs = {"tag": attr.string(), "ref": attr.label()})

def _probe(rctx):
    print("OVR|ref|" + rctx.original_name + "|" + str(rctx.attr.ref))
    texts = ["@gen_a//:x", "@mine//:x", "@injected//:x", "@dep2//:x", "@gen_c//:x", "@dep//:x"]
    src = ""
    for t in texts:
        src += 'print("OVR|label|%s|%%s" %% str(Label(%r)))\n' % (t, t)
    rctx.file("defs.bzl", src + "x = 1\n")
    rctx.file("BUILD.bazel", 'load(":defs.bzl", "x")\nfilegroup(name = "x", visibility = ["//visibility:public"])\n')

probe = repository_rule(_probe, attrs = {"ref": attr.label()})

def _ext(mctx):
    plain(name = "gen_a", tag = "ext")
    probe(name = "gen_b", ref = "@gen_a//:x")
    plain(name = "gen_c", tag = "ext", ref = "@injected//:x")
    plain(name = "mine", tag = "ext")
    plain(name = "gen_d", tag = "unused")

ext = module_extension(_ext)
