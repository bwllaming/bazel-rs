def _srcdir(rctx):
    # gazelle language/go/generate.go:130-144 (go_repository mode): the
    # repository root's parent directory decides how ${SRCDIR} is spelled.
    segments = str(rctx.path("")).split("/")
    name = segments[-1]
    if segments[-2] == "external":
        return "external/" + name
    return "../" + name

def _impl(rctx):
    rctx.file("libfoo.a", "!<arch>\n")
    srcdir = _srcdir(rctx)
    rctx.file("BUILD.bazel", """
genrule(
    name = "link_flags",
    srcs = ["libfoo.a"],
    outs = ["link_flags.txt"],
    cmd = "test -f {srcdir}/libfoo.a && echo '-L{srcdir}/ -lfoo' > $@",
)
""".format(srcdir = srcdir))

cgo_like = repository_rule(implementation = _impl)
