"""A repository other repositories' tools write into (gazelle's go_repository_cache).

`cache_dir` publishes only relocatable files. `cache_user` derives the cache
directory from a label into it, as gazelle's resolve_go_env does for GOCACHE
(`<dirname of go.env>/gocache`), and runs a tool that keeps a run counter there.
It reads `//:cache_stamp.txt`, so editing that file re-runs it, not `cache_dir`.
"""

def _cache_dir_impl(rctx):
    rctx.file("BUILD.bazel", 'exports_files(["cache.env"])\n')
    rctx.file("cache.env", "relocatable\n")

cache_dir = repository_rule(_cache_dir_impl)

_COUNT = 'mkdir -p "$TOOL_CACHE" && n=$(( $(cat "$TOOL_CACHE/runs" 2>/dev/null || echo 0) + 1 )) && echo $n > "$TOOL_CACHE/runs" && echo "runs=$n"'

def _cache_user_impl(rctx):
    rctx.read(rctx.attr.stamp)
    cache = str(rctx.path(rctx.attr.cache).dirname)
    result = rctx.execute(
        ["/bin/sh", "-c", _COUNT],
        environment = {"TOOL_CACHE": cache + "/toolcache"},
    )
    if result.return_code:
        fail(result.stderr)
    rctx.file("runs.txt", result.stdout)
    rctx.file("BUILD.bazel", 'exports_files(["runs.txt"])\n')

cache_user = repository_rule(
    _cache_user_impl,
    attrs = {
        "cache": attr.label(),
        "stamp": attr.label(),
    },
)
