# `--repository_cache`, `--repo_contents_cache` and `--disk_cache`

Bazel 9.3.1's shared caches, in genuine's on-disk layouts, so that genuine
Bazel and bazel-rs on one machine (a CI runner, a developer's `~/.bazelrc`
with `common --repository_cache=…`) share them both ways. Implementation:
`src/repository_cache.rs` (repository cache), the `DiskCache` and
`CachePolicy` of `src/remote_cache.rs` (disk cache), the option parsing in
`src/rc_options.rs` and `src/remote_cache.rs`, and `install_repository_cache`
in `src/main.rs`. Pinned source: `/tmp/opencode/bazel-9.3.1-reference`;
paths below are relative to `src/main/java/com/google/devtools/build/lib/`.

## Options

| Option | Bazel 9.3.1 | bazel-rs |
| --- | --- | --- |
| `--repository_cache=PATH` (old name `--experimental_repository_cache`) | `bazel/repository/RepositoryOptions.java:40-54`, `BazelRepositoryModule.java:322-330`; the empty value disables it; default `<output_user_root>/cache/repos/v1` | the shared download cache below; the empty value disables it; unset: no shared cache (see "Default") |
| `--repo_contents_cache=PATH` (old name `--repository_contents_cache`) | `RepositoryOptions.java:56-72`; default `<repository_cache>/contents`; checked at command start (`BazelRepositoryModule.java:338-401`) | accepted, the checks are genuine's (below); nothing is written there (see "Repo contents cache") |
| `--experimental_repository_cache_hardlinks` | `RepositoryOptions.java:155-163`, `DownloadCache.get`: a hit is hard-linked instead of copied, to save disk space | accepted; hits are copied (see "Hard links") |
| `--experimental_repository_downloader_retries=N` | `RepositoryOptions.java:179-188` (default 5), `DownloadManager.shouldRetryDownload`: whole-download retries after `SocketException`, `UnknownHostException`, `ContentLengthMismatchException` | validated (`Converters.IntegerConverter`: `'x' is not an int`) and accepted; see "Known differences" |
| `--experimental_remote_downloader` | `remote/options/RemoteOptions.java:145-155`, a Remote Asset API download proxy | the empty (default) value is accepted; any other value fails explicitly (it changes where downloads come from) |
| `--[no]experimental_remote_downloader_local_fallback`, `--[no]experimental_remote_downloader_propagate_credentials` | read only with a remote downloader | accepted (no remote downloader can be set) |
| `--disk_cache=PATH` | `RemoteOptions.java:400-406`, `remote/disk/DiskCacheClient.java`, `remote/CombinedCache.java` | implemented (below), alone or with an HTTP or gRPC `--remote_cache` |
| `--experimental_disk_cache_gc_max_size`, `--experimental_disk_cache_gc_max_age`, `--experimental_disk_cache_gc_idle_delay`, `--repo_contents_cache_gc_*` | the disk cache's and repo contents cache's idle-task collection (`DiskCacheGarbageCollectorIdleTask`) | unchanged: the native cache's collection (docs/CACHE_GC.md); they do not collect `--disk_cache` (see "Known differences") |
| `--repository_cache_urls_as_default_canonical_id` | not an option of 9.3.1 (removed) | `Unrecognized option`, like genuine |

Paths follow `OptionsUtils.PathFragmentConverter` (a leading `~/` is `$HOME`)
and are normalized like a `PathFragment`. `--repository_cache` and
`--repo_contents_cache` are relative to the workspace
(`BazelRepositoryModule.toPath`), `--disk_cache` to the working directory
(`CombinedCacheClientFactory.createDiskCache`). They are common options of
every command, so `common` rc lines apply to `build`, `query`, `cquery`,
`fetch`, `mod`, `info`, `clean`, `run`, `test` and `coverage`.

## The repository cache

Layout (`bazel/repository/cache/RepositoryCache.java`, `DownloadCache.java`):

    <repository_cache>/content_addressable/<sha256|sha384|sha512|sha1|blake3>/<hex>/file
    <repository_cache>/content_addressable/<algorithm>/<hex>/id-<hex of the canonical id hashed with <algorithm>>

Writes go through `<hex>/tmp-<random>` renamed over `file` (atomic; a
concurrent writer of the same content may win, like
`renameToleratingConcurrentCreation`). Entries are created mode 0644.

bazel-rs keeps its own download store (`<cache>/repositories/downloads`,
which its repository store, garbage collection and warm paths rely on) as the
first tier, so warm builds and existing caches are unchanged. The shared
cache is the second tier:

* **Lookups** (`DownloadManager.java:247-277`, `DownloadCache.findCacheValue`):
  a native miss looks up the shared cache by the expected checksum (sha256 or
  SRI integrity) before the network, also under `--repository_disable_download`.
  The entry's `file` must hash to the checksum, and a non-empty canonical id
  needs its `id-` marker. A hit is touched (its mtime), like genuine, and enters
  the native store (a hard link when both are on one file system, else a copy).
  A corrupt entry or a foreign canonical id is a miss and the download's put
  repairs it.
* **Writes** (`DownloadManager.java:358-363`): every verified download enters
  the shared cache under its checksum (sha256 when none was given) with the
  canonical id's marker. A failing write is the download's IOException, like
  genuine. A native-store hit the shared cache lacks also enters it (only the
  entry's existence is checked), so genuine finds what bazel-rs downloaded
  before the option was set.
* **Consumers**: `repository_ctx.download`/`download_and_extract` and
  `module_ctx` downloads (so `http_archive`, `http_file`, `http_jar` and every
  other Starlark repository rule), module registry archives with their overlays
  and patches, and registry files whose SHA-256 the lockfile records
  (`DownloadManager.downloadAndReadOneUrlForBzlmod`: `getBytes`/`put` without a
  canonical id).
* **Canonical ids**: `ctx.download*(canonical_id=…)` as given; a registry
  archive is stored under genuine `http_archive`'s default canonical id, its
  URLs joined by spaces (`tools/build_defs/repo/cache.bzl`
  `get_default_canonical_id`; empty under
  `--repo_env=BAZEL_HTTP_RULES_URLS_AS_DEFAULT_CANONICAL_ID=0`), overlays and
  patches under the empty one (`utils.bzl` passes `ctx.attr.canonical_id`).
* `info repository_cache` prints the resolved path; `null` for the empty
  value (`RepositoryCacheInfoItem`, `print(null)`); without the option the
  native download store, as before.

### Default

Without `--repository_cache` there is no shared tier: genuine's default
(`<output_user_root>/cache/repos/v1`) belongs to one output user root, which
bazel-rs does not have, and its native store already plays that role for all
worktrees of a cache (docs/WORKTREES.md). `--repository_cache=` (empty) also
disables only the shared tier: the native store is part of bazel-rs's
repository model, and a download cache never changes results (every hit is
verified against its checksum).

### Hard links

`--experimental_repository_cache_hardlinks` only saves disk space in genuine.
bazel-rs publishes repositories as immutable, content-addressed trees and
refuses hard-linked repository files (a repository rule may rewrite a
downloaded file in place, which would corrupt the cache entry sharing its
inode), so hits are copied; the contents are the same. Between the native
store and the shared cache blobs are hard-linked whenever possible (both are
content-addressed and verified on every read).

## Repo contents cache

Genuine caches the fetched contents of reproducible repositories under
`<repository_cache>/contents` (`LocalRepoContentsCache.java`), keyed by its
own repository-definition hashes, and links `external/<repo>` into it. That
cache is an optimization of fetching: a repository fetched again is the same.
bazel-rs already keeps every fetched repository in its own content-addressed
repository store shared by all worktrees of a cache, and does not write or
read genuine's contents directories (mapping genuine's predeclared-input
hashes is not modelled); results are unaffected. The checks that make
genuine fail are applied identically (exit 2, `BAD_REPO_CONTENTS_CACHE`): the
output base inside the contents cache (`The output base [B] is inside the repo
contents cache [C]. …`), and the contents cache — `--repo_contents_cache`, or
`<repository_cache>/contents` when only `--repository_cache` is set — inside the
workspace (`The repo contents cache [C] is inside the main repo [W]. …`).

## The disk cache

Layout (`DiskCacheClient.java`, SHA-256): `<disk_cache>/ac/<first two hex
digits>/<hash>` holds serialized `ActionResult` protos, `<disk_cache>/cas/<..>/<hash>`
blobs, `<disk_cache>/tmp/` temporary files (written, synced, chmod 0644, renamed
into place). Reads and CAS writes of an existing blob refresh its mtime; an AC
hit refreshes the blobs it references (`refreshActionResult`; the integrity
check is off because `--rewind_lost_inputs` defaults to true; a blob that is
gone when it is downloaded makes the hit a miss and the action runs). Action
keys are genuine's (docs/REMOTE_CACHE.md), so entries are shared both ways.

Policies (`RemoteExecutionService.getReadCachePolicy`/`getWriteCachePolicy`,
`Spawns.mayBeCached`): the disk cache serves and stores every spawn without
`no-cache` or `local`, whatever `--remote_accept_cached` and
`--remote_upload_local_results` say (those are the remote cache's), and tags
`no-remote`/`no-remote-cache` keep a spawn out of the remote cache only. With
a remote cache (`CombinedCache`): the disk cache is read first, then the remote
cache; a remote action result and the blobs downloaded for it enter the disk
cache; local results are uploaded to both, each under its policy. The runner of
a disk hit is `disk cache hit` (`INFO: N processes: 2 disk cache hit, …`), and
Build without the Bytes applies as with a remote cache (outputs may stay in the
disk cache and are read from there when needed). An output base inside the
disk cache fails like genuine: `The output base [B] cannot be a subdirectory of
the --disk_cache directory [D]` (exit 2).

## Known differences

* Garbage collection of `--disk_cache`: genuine collects it in an idle task of
  its server after `--experimental_disk_cache_gc_idle_delay` (never under
  `--batch`); bazel-rs has no server and does not collect it (the
  `--experimental_disk_cache_gc_*` options keep collecting the native cache,
  docs/CACHE_GC.md). A disk cache shared with a genuine server is collected by
  that server.
* `--experimental_repository_downloader_retries`: bazel-rs's downloader keeps
  its own transient-failure handling (one retry of a connection failure or a
  retryable HTTP status per URL, like `HttpConnector`) and does not add
  genuine's whole-download retries; a transient failure that genuine's retries
  would mask fails the download visibly. Downloaded contents are always
  verified, so the option never changes a result.
* Remote execution with `--disk_cache`: remotely executed spawns' results are
  not written into the disk cache (`RemoteExecutionService.java:1443`), and
  their lookups ask only the remote cache.
* The deprecation warning of the old option names (`Option
  'experimental_repository_cache' is deprecated: Use --repository_cache
  instead`) is not printed (bazel-rs prints no old-name warnings).
* `TestResult.execution_info.strategy` of a test whose result came from the
  disk cache reads `remote cache hit` (the processes line says `disk cache
  hit`); the native summary line counts disk hits as `remote cache hits`.

## Evidence

* Paired captures (genuine 9.3.1 and bazel-rs, loopback servers only):
  `tests/fixtures/repository_cache/capture.py` → `reference_receipt.json`:
  genuine and bazel-rs write identical layouts for an `http_archive`, an
  `http_file` (sha384 integrity), a registry archive and the registry files;
  a second genuine output root, and a fresh bazel-rs cache, on the other
  engine's (or their own) repository cache download no archive (none at all
  with a lockfile). `tests/fixtures/disk_cache/capture.py` →
  `reference_receipt.json` and `genuine/` (genuine's disk cache of the
  fixture): identical file lists, `2 disk cache hit` both ways.
* `tests/repository_cache.rs`: genuine's layout, a second fresh cache that
  downloads nothing, genuine-written entries as hits, corrupt and foreign-id
  entries, the empty value, a CI-style home rc (`common --repository_cache=…`,
  `--repo_contents_cache=`, `--experimental_repository_cache_hardlinks`,
  `--experimental_repository_downloader_retries`, `--disk_cache`, gc options)
  on `query`, `info`, `build`, `fetch`, `mod` and `clean`, and the repo contents
  cache checks. `tests/disk_cache.rs`: genuine's entries as hits, genuine's
  layout, the policies, the output base check. `tests/remote_cache.rs`
  `disk_cache_is_combined_with_the_remote_cache`. Unit tests in
  `src/repository_cache.rs` and `src/rc_options.rs`.
