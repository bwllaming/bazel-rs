# Streaming repository I/O (N32)

Native repository-rule and module-extension I/O follows Bazel 9.3's production
model: bodies and archives stream to disk in constant memory, there are no
artificial size or count caps, and timeouts and redirect bounds follow genuine
Bazel. HTTP identity follows the native policy below. Pinned source: Bazel 9.3
(`/tmp/opencode/bazel-9.3-reference`).

## Design

- **Transport** (`src/extension_download.rs`). One shared HTTP(S) transport for
  `repository_ctx.download`/`download_and_extract`, `module_ctx.download` and
  the registry archive materializer's checksum discipline. `fetch_to_file`
  opens each mirror, creates/truncates the destination, and `copy_hashed`
  streams 64 KiB chunks into it while updating the incremental digest
  (SHA-1/256/384/512, BLAKE3). A checksum mismatch reports Bazel's
  `Checksum was X but wanted Y` (HashOutputStream.java:84-90) and removes the
  failed mirror's file before the next mirror. Timeouts are Bazel's
  HttpConnector values: 10 s connect (`MAX_CONNECT_TIMEOUT_MS`,
  HttpConnector.java:60) and a 20 s per-read timeout (`READ_TIMEOUT_MS`,
  HttpConnector.java:61, 155-157) - never a whole-transfer deadline. The 40th
  redirect fails with `Redirect loop detected` (HttpConnector.java:57, 205-208).
  OS resource failures (`ENOSPC`, `EDQUOT`, ...) surface as completion failures
  carrying the OS reason, like Bazel's IOException.
- **HTTP identity**. The shared download client defaults to
  `User-Agent: bazel-rs/<CARGO_PKG_VERSION>` (currently `bazel-rs/0.1.0`),
  including blocking/nonblocking repository and module downloads, retries,
  mirrors and redirect hops. This is the native package version, not
  `COMPATIBILITY_VERSION`; it does not impersonate Bazel or a browser. A
  request's explicit caller or credential-helper User-Agent overrides the
  default case-insensitively without an extra default value. Existing header
  merging is unchanged: helper credentials replace static auth, credentials
  replace common headers with the same spelling, and static auth is URL-scoped.
  The existing case-sensitive common/credential merge is not normalized here.
  This is intentionally **not full header parity**: genuine Bazel supplies
  `Bazel/<release name>` (HttpConnectorMultiplexer.java:52-57) and overwrites
  user-provided reserved base headers (:107-113) before merging credentials
  (:133-150). Registry metadata, remote cache, BES and gRPC identities are
  separate and unchanged; no cache identity or cache key changes are needed.
- **download_and_extract / extract** (`src/repository_ctx.rs`). The archive
  streams into a private `.download-*` directory of the repository (Bazel:
  `<output>/temp<N>/<basename>`, StarlarkBaseExternalContext.java:1126; error
  texts keep Bazel's path), then `archive::extract_file` streams it through the
  decompressor into a staging directory that is merged into the output; both
  temporary directories are removed. `extract()` streams the local archive.
- **download** (StarlarkBaseExternalContext.java:799-895). With a checksum,
  an existing output whose digest matches is reused without any download,
  also offline (DownloadManager.java:247-261). Otherwise the body streams
  straight into the output path like HttpDownloader (java:90-109); an existing
  non-directory output is unlinked (never written through a symlink or hard
  link) once a mirror responds; a checksum mismatch leaves the downloaded
  bytes, also under `allow_fail`; `executable` adds the
  execute bits after success (java:676); the result is
  `calculateDownloadResult` (java:559-583). `block = False` returns a
  `PendingDownload` (N43, docs/EXTENSION_CTX_SURFACE.md).
- **Failure reporting**: each failed mirror prints `WARNING: Download from URL
  failed: class CLASS MESSAGE` (HttpDownloader.recordFailure, java:217-240);
  a total failure is `java.io.IOException: Error downloading [URLS] to PATH:
  LAST_MESSAGE` (java:243-262) with HttpConnector's messages (`GET returned
  CODE REASON`, java:309-315, using the response's reason phrase).
- **Archives** (`src/archive.rs`). Tar formats decode incrementally like the
  CompressedTarFunction subclasses: gzip (`MultiGzDecoder`,
  GzipCompressorInputStream concatenated), xz (`lzma-rust2` XzReader with
  multi-stream, TarXzFunction.java), zstd (`ruzstd` frames incl. skippable
  frames, window bounded at 2^31 like `setLongMax(Zstd.windowLogMax())`,
  TarZstFunction.java:33), bzip2 (`MultiBzDecoder`, TarBz2Function.java:34).
  Zip uses random access on the file. GNU long names/links (`L`/`K`) and local
  PAX `path`/`linkpath` name the next entry like commons-compress
  TarArchiveInputStream; inert PAX keys are ignored and semantic ones (sparse
  files, PAX `size`) fail explicitly; these records are read into memory, so
  one larger than 1 MiB fails explicitly. Type `7` (contiguous) entries are
  regular files. Prefix suggestions are collected in the
  same pass (CompressedTarFunction.java:96-99,
  DecompressorValue.java:158-163) instead of re-extracting the archive.
- **Repository cache** (`src/materializer.rs`). Registry source archives stream
  into a `NamedTempFile` inside `downloads/<algorithm>/`, hashed while written,
  verified, fsynced and published by atomic rename to
  `downloads/<algorithm>/<hex>`; cached blobs are re-verified by streaming
  hash. Extraction streams from the blob; overlays are copied from theirs.
- **Store publication** (`src/repository_store.rs`, `src/git_source.rs`). Files
  are hashed incrementally while snapshotting; no file/total/entry caps.
- **In-memory by API** (inherent, as in Bazel): `ctx.read`, template input,
  `execute` stdout/stderr, patch text, registry metadata, `module_ctx.read`.

## Cap inventory

Removed (none exists in Bazel):

| Location | Former cap |
|---|---|
| extension_download.rs | FILE_LIMIT 8 MiB, TOTAL_LIMIT 32 MiB (encoded and decoded), LIVE_LIMIT 16 MiB, 128 MiB work, 32 calls, 64 requests, 120 s total deadline, 8 redirects (now Bazel's 40), URL 8192 / checksum 256 text lengths |
| extension_io.rs | 8 MiB file, 16 MiB live scratch, 8 mirrors, path 1024 bytes / 32 components / 128-byte components, 8 MiB source read |
| repository_ctx.rs | MAX_FILE_BYTES 16 MiB, MAX_TOTAL_BYTES 64 MiB (writes and reads), MAX_OPERATIONS 10000 (writes, templates, recorded inputs), MAX_EXECUTIONS 1000, MAX_ARGV_BYTES 2 MiB, MAX_TIMEOUT_SECONDS 24 h, MAX_DIRENTS 100000, 8 mirrors |
| repository_process.rs | STREAM_LIMIT 16 MiB per execute stream |
| archive.rs | MAX_EXPANDED 1 GiB, MAX_ENTRIES 100000, MAX_PAX_RECORDS 100000, MAX_LINK_TARGET 4096 |
| materializer.rs | DOWNLOAD_LIMIT 256 MiB, OVERLAY_LIMIT 16 MiB |
| registry.rs | METADATA_LIMIT 16 MiB and download size limits |
| repository_store.rs | FILE_LIMIT 16 MiB, TOTAL_LIMIT 64 MiB, MANIFEST_LIMIT 48 MiB, ENTRY_LIMIT 10000 |
| patching.rs / patch_inputs.rs | 16 MiB per patch/file, 100000 lines, 256 MiB working set, 100 M comparisons; 16 MiB / 64 MiB patch inputs |
| git_source.rs / git_process.rs | 100000 entries, 1 GiB, 16 MiB MODULE, 64 MiB manifest, 1 MiB listing, 1 MiB capture per stream, shared 120 s deadline (now 600 s per command, the default `execute` timeout git_repository uses) |

Kept:

- Bazel limits: 40 redirects, 10 s/20 s timeouts, zip symlink target
  `< 256` bytes (ZipDecompressor `MAX_PATH_LENGTH`), zstd window 2^31,
  `execute` timeouts given by the caller, checksum/integrity verification,
  `canonical_id` keys the repository download cache (N43,
  docs/EXTENSION_CTX_SURFACE.md).
- Safety by the correct mechanism, independent of size: per-entry path
  normalization and escape checks, no writes through symlinks, in-root hard
  links only, zip link-escape check, atomic publication.
- Store path profile (not I/O size): UTF-8 names, 4096 bytes total, 255 bytes per component, no NUL, empty, `.` or `..` components (no depth cap). Note: deeply nested trees (worst case ~2048 levels) can hit EMFILE/thread-stack limits in the recursive walkers; all failures are explicit.
- GNU long-name/link and local PAX extension records: 1 MiB each (they are
  buffered in memory; real names are tiny), explicit error beyond.
- Registry metadata and source archives use the same 10 s connect / 20 s
  per-read timeouts without a total deadline (no request-level reqwest
  `.timeout()`, which would be a whole-transfer deadline).
- Not repository I/O, unchanged: the Starlark `json` module's decode/encode
  host budgets (docs/JSON.md; bound to the json validator's frozen plan), the
  extension preparation budgets and the callback evaluator tick/heap bounds
  were removed in N43 (docs/EXTENSION_CTX_SURFACE.md).
- Explicitly unsupported: single-file gz/xz/zst/bz2, 7z, and for zip/tar
  `rename_files` and `strip_components > 0` (N43 added `auth`/`headers` and
  `download(block = False)`; `.ar`/`.deb` stream like ArFunction,
  docs/AR_ARCHIVES.md).
