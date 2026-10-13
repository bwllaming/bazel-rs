# Genuine Bazel 9.3 builtin module closure

This offline file registry contains **unaltered public BCR response bytes**, not
synthetic public MODULE files. It was captured with the official Linux x86_64
Bazel 9.3.0 release executable:

- Source: https://releases.bazel.build/9.3.0/release/bazel-9.3.0-linux-x86_64
- SHA256: `d302d22ed77ee5658b87857249baa579db84300ecebe0a1e9a357b5b20f80eb8`

`expectations.json` is the test API. Each `cases[]` entry contains `name`, the
exact `root_module` string, `selected` name/version objects (root excluded,
`bazel_tools` included with empty version), and relative `reference_graph` and
`reference_lockfile` paths. Cases are `empty-root` and `older-platforms`; the
latter requests genuine `platforms@0.0.10`, resolved by the official reference
to `platforms@1.0.0`. Both cases select 28 non-root modules.

`cases/` preserves each root, exact official JSON graph, stderr, and lockfile.
The graph's dependency/cycle arrays and apparent names are preserved verbatim.
The selected list is a sorted flattening of that official graph, **not native
resolver output**. JSON formatter ordering is not MODULE declaration ordering.
Canonical names are deliberately not inferred from apparent names.

`registry/` is a complete union of both official lockfiles' registry inputs:
156 MODULE versions, 27 `source.json` files, and registry configuration. In
addition, 32 genuine per-module `metadata.json` snapshots support offline
yanked-version checks. Unselected discovered versions are included. The 216
registry files total 316,062 bytes. No source archives, generated repositories,
SDKs, copied repository caches, or replacement builtin MODULEs are bundled.

`provenance.json` records every registry file's exact source URL, SHA256, byte
count, and expected official lockfile hash when present. The captured lockfiles
also preserve `selectedYankedVersions`. Complete reference argv, stdout/stderr,
hashes, timeouts, and private-server/scratch cleanup evidence live in
`artifacts/builtin-module-contract/registry-capture.json`.

To independently capture into a **new** destination (requires network and the
pinned binary; genuine shared source caches may be supplied explicitly):

```sh
python3 scripts/capture_builtin_modules.py --destination /tmp/opencode/new-builtin-fixture
```

The generator refuses existing destinations, validates all official registry
hashes, and only publishes after both real reference commands succeed. Every
subprocess has a deadline, and every started private server is shut down in
`finally`. It does not execute the native candidate. Integrity/projection unit
tests run offline:

```sh
python3 -m unittest discover -s tests -p test_capture_builtin_modules.py
```
