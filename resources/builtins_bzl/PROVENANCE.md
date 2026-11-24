# Verbatim `@_builtins` Starlark sources

Files below this directory are unmodified copies of Bazel's
`src/main/starlark/builtins_bzl/` at upstream revision
`e89755babf3e941bc1e9d8120b1cd48854ab9dfa` (Bazel 9.3.0), Apache-2.0 (see
`resources/bazel_tools/LICENSE`). The native profile evaluates them with its own
Starlark interpreter under their `@@_builtins//:` labels; no Java Bazel runs.

| File | SHA-256 |
| --- | --- |
| `common/xcode/providers.bzl` | `67c02e7c2ae4793ce74f8e29e10058096439edda4f0aa32585296f4cc38f5827` |
| `common/objc/apple_env.bzl` | `f1b2c27562c47d4220022c633c67c7d756d45c22a57480832bc460ae25986ffe` |

`common/xcode/providers.bzl` loads `PLATFORM` and `PLATFORM_TYPE` from
`common/objc/apple_platform.bzl` and reads `_builtins.internal.apple_common`.
`src/apple_xcode.rs` blanks that one `load` line (keeping line numbers) and
binds the two tables and `_builtins` as globals instead.
`common/objc/apple_env.bzl` (`apple_common.apple_host_system_env` and
`apple_common.target_apple_env`) has no loads and is evaluated as it is.
