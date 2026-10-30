# Official Bazel tools resource bundle

This directory retains the complete, unmodified `embedded_tools/` file trees from
the official **Bazel 9.3.1** distributions of each supported host platform, one
subdirectory per platform (Bazel's release names): `linux-x86_64/` and
`linux-arm64/`, each holding `embedded_tools.tar.gz`, `manifest.json` and the
`MODULE.bazel` sidecar. A bazel-rs binary compiles in only the bundle of its
compile target (`src/builtin_tools.rs`: x86_64 → `linux-x86_64`, aarch64 →
`linux-arm64`). They are development-time static resource imports, not a
runtime fallback to the Java Bazel engine. `LICENSE` and this document are
shared.

## Pinned authorities

- Distributions and SHA-256 (each also equal to the official `.sha256` file of
  the GitHub release `bazelbuild/bazel` 9.3.1):
  - linux-x86_64: <https://releases.bazel.build/9.3.1/release/bazel-9.3.1-linux-x86_64>
    (= <https://github.com/bazelbuild/bazel/releases/download/9.3.1/bazel-9.3.1-linux-x86_64>),
    `7f775b54bbec487efaf0f49ccda1d2ed02cd34f582edfdd6aeeec76e39ef585c`
  - linux-arm64: <https://releases.bazel.build/9.3.1/release/bazel-9.3.1-linux-arm64>
    (= <https://github.com/bazelbuild/bazel/releases/download/9.3.1/bazel-9.3.1-linux-arm64>),
    `10ba9cb76d4ee748d274dcc2981992bb12b74a203dc41a9c2eceeb8f29359366`
  (both downloaded from GitHub and checked 2026-10-30 against the GitHub and
  releases.bazel.build `.sha256` files)
- Embedded `build-label.txt`: exactly `9.3.1` in both.
- Upstream source revision (tag `9.3.1`, "Release 9.3.1 (2026-10-20)"):
  `fee2bf2d1c14bc2096068a4d46f1842472092044`
- `MODULE.bazel` is byte-identical to the embedded file and upstream
  `src/MODULE.tools`; SHA-256:
  `a0fab7d7a9314cfba6d73f33c50643ba26dac9be28700ff35e88306dbd69fecd`.
- `LICENSE` is the complete upstream source `LICENSE` at that revision,
  including its additional notice; SHA-256:
  `9fe6dae7684812d0117f5a0611aa773656776545797608f201fef88fa7363e0b`.
  The local source snapshot's Git HEAD and committed LICENSE were checked against
  this revision, rather than relying on an unversioned working-tree copy.

## 9.3.0 → 9.3.1

The bundles were regenerated from the 9.3.1 distributions on 2026-10-30. Of the
installed tree only `jdk/lib/security/cacerts` changed (both platforms, same
bytes: 132,316 → 127,779 bytes, SHA-256 `707312d3…` → `57bd1838…`): 9.3.1
removes the distrusted Sectigo "AAA Certificate Services" and eMudhra/emSign
root CAs from the embedded JDK truststore (upstream #31920,
`src/minimize_jdk.sh`). `MODULE.bazel`, all Starlark/BUILD/script files and the
compiled helpers are byte-identical to 9.3.0's (docs/BAZEL_9_3_1.md).

## Complete installed tree

linux-x86_64: `embedded_tools.tar.gz` contains exactly **421 regular files**,
totaling **73,322,435 uncompressed content bytes**. linux-arm64: **420 regular
files**, **71,904,827 bytes** (the arm64 JDK has no `lib/libsimdsort.so`). The
two trees have the same paths otherwise and byte-identical `MODULE.bazel`,
Starlark, BUILD and script files; only 25 JDK files and the three compiled
helpers below differ. Directories are implicit. Only the
`embedded_tools/` prefix is removed; names and contents otherwise remain exactly
as officially packaged, including already-renamed repository BUILD files.
There are no synthetic BUILD files, provider shims, or omitted JDK files.

The x86-64 JDK subtree contains 108 files / 71,307,011 bytes, including upstream
legal materials. The other 313 files include the platform's compiled helpers
`tools/cpp/modules_tools/aggregate-ddi`,
`tools/cpp/modules_tools/generate-modmap`, and `tools/zip/zipper/zipper`.
These are ordinary tool resources, not the Java Bazel engine. The distribution's
launcher and `A-server.jar` are not bundled. This packaging script does not execute
the distribution or any embedded executable. Native Rust user-runtime operation
must consume the checked-in resources, not locate, execute, or extract a reference
Bazel installation.

All archived regular files have installed POSIX mode **0755** (decimal **493**),
matching official installation, not the distribution ZIP's zero attributes.
Consumers should create implicit parent directories with mode 0755. Archive
metadata is normalized to uid/gid/mtime zero and empty user/group names.

## Reproduction and schema

From the repository root, with the pinned distribution already available
(default `.direnv/reference/bazel-9.3.1-<platform>`):

```sh
python3 scripts/package_builtin_tools.py                          # linux-x86_64
python3 scripts/package_builtin_tools.py --check
curl -fLO https://github.com/bazelbuild/bazel/releases/download/9.3.1/bazel-9.3.1-linux-arm64
python3 scripts/package_builtin_tools.py --platform linux-arm64 --distribution bazel-9.3.1-linux-arm64 [--check]
```

`--platform` selects one of the pinned distributions (`linux-x86_64`, the
default, or `linux-arm64`; its outputs go to `resources/bazel_tools/<platform>/`).
`--distribution PATH` and `--output-dir PATH` change locations only; neither can
override the pinned version, platform, hashes, file count, or content size.
`--verify` aliases `--check`: regenerate all generated outputs in memory and
compare every byte without writing. Missing or altered outputs fail. The script
uses only Python's standard library and performs no network access.

The recipe sorts relative paths, creates a USTAR tar containing only regular-file
entries, and gzip-compresses it at level 9 with mtime zero and an explicit empty
filename. All current paths fit USTAR (maximum 89 bytes), so no PAX/GNU extension
records are emitted. It rejects unsafe names, duplicate paths, file/directory
collisions, links, special files, and provenance mismatches before publication.
Outputs are staged before replacement, with rollback on replacement errors; this
development tool is not a power-loss-atomic or concurrent runtime updater.
Byte reproducibility assumes compatible Python tarfile/gzip and zlib behavior.

`manifest.json` is UTF-8, sorted JSON object keys, two-space indentation, and one
trailing newline. Its exact schema version 1 fields are:

- `schema_version`: integer 1;
- `bazel_version`, `platform`, `source_revision`, `distribution_url`,
  `distribution_sha256`: pinned provenance strings;
- `archive_sha256`: lowercase SHA-256 hex of `embedded_tools.tar.gz`;
- `file_count`, `total_size`: regular-file count and sum of content byte lengths;
- `files`: path-sorted array of objects with exactly `path` (relative POSIX path),
  `size` (byte length), `sha256` (lowercase content digest), and `mode` (493).

There is no directories array; parents are inferred from file paths.
`MODULE.bazel` beside the archive is an exact discovery sidecar, not a replacement
or synthetic module. `LICENSE` and this provenance document are outside the
archive so that its installed tree remains exact; the generator never rewrites
these two manually retained documents.

## Redistribution and platform scope

Bazel's upstream license is Apache-2.0 with the notices retained in `LICENSE`.
Bundled third-party/JDK materials retain their own notices and licenses inside
the unchanged tree; the JDK is not relicensed under Apache-2.0. Redistributors
must comply with those component-specific terms, including any applicable source
availability requirements. Each bundle is specifically its platform's release:
its compiled helpers and JDK are not portable substitutes for other platforms.
A new platform (e.g. darwin-arm64) is one more `PLATFORMS` pin in
`scripts/package_builtin_tools.py`, its generated subdirectory, and a `pinned!`
entry in `src/builtin_tools.rs`.
