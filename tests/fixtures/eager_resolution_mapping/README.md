# Eager resolution mapping fixture (N51)

These inputs show that every module's repository mapping includes
`@bazel_tools` (docs/BUILTIN_MODULES.md). They are used by
`tests/eager_resolution_mapping.rs` and by the paired capture
`artifacts/eager_mapping-capture.py`.

- `registry/`: contains `eagerdep@1.0` only. Its MODULE.bazel names a
  `@bazel_tools` extension and a `@bazel_tools` repository rule, the shape used
  by rules_jvm_external 6.7. The `git_repository` source is never fetched. The
  capture overlays this registry on `tests/fixtures/builtin_modules/registry`,
  and the Rust tests serve it alone.
- `localdep/`: a `local_path_override` module with the same usages, plus two
  `.bzl` files. `labels.bzl` calls `Label("@bazel_tools//...")`, and `loads.bzl`
  loads `@bazel_tools//tools/build_defs/repo:utils.bzl`.
- `workspace/`: the root module. It provides three queries:
  - `//:plain`
  - `//labels:labels` (root and non-root `Label` strings)
  - `//loads:loads` (a non-root `.bzl` loading `@bazel_tools` contents)
- `reference_receipt.json`: evidence comparing genuine Bazel 9.3.0 with native,
  in both lockfile modes `update` and `off`. No test reads it.

Source URLs use `example.invalid` and are never downloaded.
