# Registry lockfile fixture (N37)

Synthetic BCR-shaped registry inputs for `tests/registry_lockfile.rs` and the
paired capture `artifacts/lockfile-capture.py` (docs/REGISTRY_LOCKFILE.md).

- `registry_overlay/`: `lockdemo_a@1.0` (archive source, never fetched),
  `lockdemo_b@1.0` (yanked in `metadata.json`) and `@1.1` (git_repository
  sources). The capture overlays it on `tests/fixtures/builtin_modules/registry`
  (r1); the Rust tests serve it alone.
- `registry_extra/`: `lockdemo_c@1.0` only, with no `bazel_registry.json` (r2),
  so r1 records `lockdemo_c` MODULE.bazel and r2 records bazel_registry.json as
  `"not found"`.
- `workspace/`: the root module and a `filegroup`.
- `reference_receipt.json`: evidence from genuine Bazel 9.3.0 versus native. No
  test reads it.

The source archive URLs use `example.invalid` and are never downloaded.
