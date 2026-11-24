"""What a repository rule sees of the host: os, the client environment and
the environment of repository_ctx.execute."""

def _env_probe_impl(rctx):
    lines = [
        "os.name=%s" % rctx.os.name,
        "os.arch=%s" % rctx.os.arch,
        "os.environ keys=%s" % sorted(rctx.os.environ.keys()),
    ]
    for name in ["DARWIN_USER_TEMP_DIR", "DARWIN_USER_CACHE_DIR", "__CF_USER_TEXT_ENCODING", "TMPDIR", "HOME", "PATH"]:
        lines.append("os.environ[%s]=%r" % (name, rctx.os.environ.get(name)))
    result = rctx.execute(["/usr/bin/env"])
    lines.append("execute.return_code=%d" % result.return_code)
    lines.extend(["execute.env " + line for line in sorted(result.stdout.splitlines())])
    rctx.file("env.txt", "\n".join(lines) + "\n")
    rctx.file("BUILD.bazel", 'exports_files(["env.txt"])\n')

env_probe = repository_rule(implementation = _env_probe_impl)
