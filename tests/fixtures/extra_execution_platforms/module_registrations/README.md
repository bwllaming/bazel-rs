# Module execution platform registrations

`dep/` is a non-root module added to `../workspace` by tests/platforms.rs
`module_execution_platform_registrations_follow_bazel`: it registers
`//:freebsd` (and, as a dev dependency, `//:netbsd`, which a non-root module's
registration never counts), while the root module adds

    bazel_dep(name = "dep")
    local_path_override(module_name = "dep", path = "dep")
    register_execution_platforms("//p:bar")

Genuine Bazel 9.3.0 (`build //:tool //:plain` with the builtin-module stubs
and the offline fixture registry, like the test) wrote

    bazel-bin/tool.out:  tc=freebsd_exec
                         leaf=bazel-out/freebsd-opt-exec/bin/exec_leaf.out
    bazel-bin/plain.out: leaf=bazel-out/bar-opt-exec/bin/exec_leaf.out

The execution platforms are the root module's `//p:bar` (openbsd), then the
dependency's `@dep//:freebsd`, then the host: `tool`'s toolchain resolves
for the first platform with the most toolchains (freebsd), the toolchain-less
`plain` runs on the first one (bar).
