def _src(ctx):
    ctx.file("BUILD.bazel", "")
    ctx.file("a/b.txt", "b\n")
    ctx.file("a/c.txt", "one\ntwo\nthree\n")
    ctx.file("a/d.txt", "d\n")
    ctx.file("a/sub/e.txt", "e\n")
    ctx.file("a/sub/f.txt", "f\n")
    ctx.file("a/tool.sh", "#!/bin/sh\n", executable = True)

src_rule = repository_rule(implementation = _src)

# Copies another repository's tree with `cp -r` (in bazel-rs a sealed,
# read-only published tree; in genuine its writable external/ directory),
# then changes the copies with every repository_ctx file operation.
def _copy(ctx):
    src = ctx.path(Label("@src//:BUILD.bazel")).dirname
    for name in ["copy", "doomed"]:
        result = ctx.execute(["cp", "-r", str(src) + "/a", name])
        if result.return_code != 0:
            fail(result.stderr)
    ctx.file("copy/b.txt", "rewritten\n")
    ctx.template("copy/d.txt", ctx.attr.template, {"%{x}": "templated"})
    ctx.patch(ctx.attr.patch, strip = 1)
    ctx.download(
        url = ctx.getenv("PAYLOAD_URL"),
        sha256 = "30031a9831674dd684c3817399acebc88a116ce5a7a3fbc0cf34d92521a534e6",
        output = "copy/sub/f.txt",
    )
    ctx.delete("copy/sub/e.txt")
    ctx.symlink("copy/b.txt", "copy/sub/link")
    ctx.rename("copy/tool.sh", "copy/sub/tool2.sh")
    ctx.rename("copy/sub", "moved_out")
    ctx.download(
        url = ctx.getenv("PAYLOAD_URL"),
        sha256 = "30031a9831674dd684c3817399acebc88a116ce5a7a3fbc0cf34d92521a534e6",
        output = "copy/new_dir/payload.txt",
    )
    ctx.delete("doomed")
    listing = ctx.execute([
        "sh",
        "-c",
        "find copy moved_out | LC_ALL=C sort; " +
        "for f in $(find copy moved_out -type f | LC_ALL=C sort); do echo \"== $f\"; cat \"$f\"; done; " +
        "echo \"link -> $(readlink moved_out/link | sed 's|.*/||')\"",
    ])
    ctx.file("BUILD.bazel", "exports_files(['result.txt'])\n")
    ctx.file(
        "result.txt",
        listing.stdout + listing.stderr + "doomed exists: " + str(ctx.path("doomed").exists) + "\n",
    )

copy_rule = repository_rule(
    implementation = _copy,
    attrs = {
        "patch": attr.label(default = "//:fix.patch"),
        "template": attr.label(default = "//:d.tpl"),
    },
)
