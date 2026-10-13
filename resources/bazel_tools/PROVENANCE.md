# Official Bazel tools resource bundle

This directory retains the complete, unmodified `embedded_tools/` file tree from
the official **Bazel 9.3.0, Linux x86-64** distribution. It is a development-time
static resource import, not a runtime fallback to the Java Bazel engine.

## Pinned authorities

- Distribution: <https://releases.bazel.build/9.3.0/release/bazel-9.3.0-linux-x86_64>
- Distribution SHA-256:
  `d302d22ed77ee5658b87857249baa579db84300ecebe0a1e9a357b5b20f80eb8`
- Embedded `build-label.txt`: exactly `9.3.0`.
- Upstream source revision:
  `e89755babf3e941bc1e9d8120b1cd48854ab9dfa`
- `MODULE.bazel` is byte-identical to the embedded file and upstream
  `src/MODULE.tools`; SHA-256:
  `a0fab7d7a9314cfba6d73f33c50643ba26dac9be28700ff35e88306dbd69fecd`.
- `LICENSE` is the complete upstream source `LICENSE` at that revision,
  including its additional notice; SHA-256:
  `9fe6dae7684812d0117f5a0611aa773656776545797608f201fef88fa7363e0b`.
  The local source snapshot's Git HEAD and committed LICENSE were checked against
  this revision, rather than relying on an unversioned working-tree copy.

## Complete installed tree

`embedded_tools.tar.gz` contains exactly **421 regular files**, totaling
**73,326,972 uncompressed content bytes**. Directories are implicit. Only the
`embedded_tools/` prefix is removed; names and contents otherwise remain exactly
as officially packaged, including already-renamed repository BUILD files.
There are no synthetic BUILD files, provider shims, or omitted JDK files.

The JDK subtree contains 108 files / 71,311,548 bytes, including upstream legal
materials. The other 313 files include the Linux x86-64 compiled helpers
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

From the repository root, with the pinned distribution already available:

```sh
python3 scripts/package_builtin_tools.py
python3 scripts/package_builtin_tools.py --check
```

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
availability requirements. This bundle is specifically the Linux x86-64 release:
its compiled helpers and JDK are not portable substitutes for other platforms.
