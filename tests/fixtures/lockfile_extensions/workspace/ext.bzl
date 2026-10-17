"""A non-reproducible extension with file, directory, env and repo-mapping inputs."""

load("@bazel_tools//tools/build_defs/repo:http.bzl", "http_archive")
load("//:helpers.bzl", "TOOLS", "describe")
load("//:repo.bzl", "gen_repo")

_config = tag_class(attrs = {
    "id": attr.string(mandatory = True),
    "count": attr.int(default = 1),
    "enabled": attr.bool(),
    "target": attr.label(),
    "targets": attr.label_list(),
    "items": attr.string_list(),
    "mapping": attr.string_dict(),
})

def _ext_impl(ctx):
    print("LOCKEXT_EVAL ext")
    data = ctx.read(Label("//:data.txt")).strip()
    entries = sorted([p.basename for p in ctx.path(Label("//dir:BUILD.bazel")).dirname.readdir()])
    dyn = ctx.getenv("LOCKEXT_DYN", "unset")
    missing = ctx.getenv("LOCKEXT_MISSING")
    lib = Label("@lockdep//:lib")
    ids = []
    for mod in ctx.modules:
        for tag in mod.tags.config:
            ids.append(describe(tag))
    for mod in ctx.modules:
        for tag in mod.tags.config:
            gen_repo(
                name = "gen_" + tag.id,
                s = "@@escaped" if tag.id == "alpha" else data,
                i = tag.count,
                b = tag.enabled,
                l = tag.target,
                ll = tag.targets + [lib],
                sl = tag.items + entries + [dyn, str(missing), str(TOOLS)],
                sd = tag.mapping,
                sld = {"ids": ids},
                lksd = {"//:data.txt": "data", "@gen_beta//:x": "sibling"},
            )
    http_archive(
        name = "gen_http",
        urls = ["http://127.0.0.1:9/never.tar.gz"],
        sha256 = "0000000000000000000000000000000000000000000000000000000000000000",
        build_file_content = "filegroup(name = \"x\")\n",
        strip_prefix = "never",
    )

ext = module_extension(
    implementation = _ext_impl,
    tag_classes = {"config": _config},
    environ = ["LOCKEXT_STATIC"],
)
