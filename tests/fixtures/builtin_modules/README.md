# Genuine Bazel 9.3 builtin module closure

This offline file registry contains **unaltered public BCR response bytes**, not
synthetic public MODULE files. It was captured with the official Linux x86_64
Bazel 9.3.0 release executable and re-captured on 2026-10-30 with the official
Bazel 9.3.1 release executable, the pinned reference now:

- Source: https://releases.bazel.build/9.3.1/release/bazel-9.3.1-linux-x86_64
- SHA256: `7f775b54bbec487efaf0f49ccda1d2ed02cd34f582edfdd6aeeec76e39ef585c`

The 9.3.1 re-capture produced byte-identical `graph.json` and
`MODULE.bazel.lock` files for both cases (9.3.1 left `MODULE.tools` unchanged);
only `stderr.txt` (server version line, progress lines) and the reference
identity in `expectations.json`/`provenance.json` changed. The registry snapshot
(including the `metadata.json` files) is the original 2026-10-13 capture.

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
count, and expected official lockfile hash when present.

Supplemental registry patches (not lockfile inputs, so a capture never writes
them): seven genuine `patches/*.patch` files for apple_support 2.5.2,
bazel_features 1.42.1, rules_cc 0.2.17, rules_kotlin 1.9.6, rules_python 1.7.0,
rules_shell 0.6.1 and rules_swift 3.1.2 (2,221 bytes). Genuine Bazel fetches
these selected modules when analysis resolves the test toolchain type for a
`test = True` rule; without the files a file-registry reference fails with
`Error downloading [file:///…/patches/module_dot_bazel_version.patch]`. Each was
copied offline from an existing genuine Bazel repository-cache CAS blob whose
SHA256 equals the mirrored `source.json` SRI; `provenance.json`
`registry_patches` records URL, SHA256, size, SRI and acquisition, and
`verify_fixture` rejects any patch file not pinned that way. Patches of
unselected or not-fetched versions are deliberately absent. Source archives are
still not bundled: the reference downloads them (or uses a pre-seeded private
repository cache). The captured lockfiles
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
