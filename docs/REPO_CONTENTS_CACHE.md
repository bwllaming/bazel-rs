# Repository contents caches (`--repo_contents_cache`, `--experimental_remote_repo_contents_cache`)

A CI runner with an empty local cache re-fetches every repository of a
workspace from the network (on ~/code: ~210 s of a cold
`build --nobuild //src/...` on this host, 8.75 GB downloaded, 8,851
repository trees). Bazel 9.3.1 can instead take the contents of reproducible
repositories from the remote cache the CI already uses. bazel-rs implements
that remote repository contents cache with genuine's option and semantics,
adapted to its content-addressed repository store. Implementation:
`src/remote_repo_cache.rs` (keys, entries, transfers),
`src/repository_store.rs` (`remote_hit`, `upload_publication`, the hook in
`materialize_inner`), the `RemoteCache::repo_contents_*` methods of
`src/remote_cache.rs`, the startup option in `src/bazelrc.rs`. Tests:
`tests/remote_repo_cache.rs` (a local fake HTTP cache, a paired genuine
capture in `tests/fixtures/remote_repo_cache/`). Pinned source:
`/tmp/opencode/bazel-9.3.1-reference`, paths below relative to
`src/main/java/com/google/devtools/build/lib/`.

## Genuine Bazel 9.3.1

### When a cache is consulted (`bazel/repository/RepositoryFetchFunction.java`)

`computeInternal` (:213-420) for a repository defined by a Starlark rule:

1. `shouldUseCachedRepoContents` (:502-525): false for a `local = True` rule
   while fetching is enabled, and under `bazel fetch --force` (and
   `--configure` for configure rules). Otherwise:
2. the marker file `external/@<name>.marker` is consistent (the
   predeclared inputs hash and every recorded input still current): the
   existing `external/<name>` is used;
3. the local repo contents cache (below), then
4. the remote repo contents cache: `lookupCache(repo, repoRoot,
   predeclaredInputHash, env)`; an `IOException` is a warning `Remote repo
   contents cache lookup failed for <repo>: <message>` and the repository is
   fetched.
5. Otherwise the rule runs. After it, the marker file is written and, when
   the result is `Reproducibility.YES` **and** the rule is not local
   (:317), the repository's symlinks are replanted (`RepositoryUtils
   .replantSymlinks`, :98-220) and the repository is added to the remote
   cache when `safeForRemoteCache` (every symlink resolves inside the
   repository: no absolute link out of it, no link into another repository
   or the main repository) and moved into the local contents cache when
   `safeForLocalCache`.

`Reproducibility.YES` means the implementation function returned
`repository_ctx.repo_metadata(reproducible = True)` (:670-695,
`starlark/RepoMetadata.java`); a dict return value or `None` is
`NONREPRODUCIBLE`. `http_archive`, `http_file` and `http_jar` return it
whenever a `sha256`/`integrity` is given (`tools/build_defs/repo/http.bzl`
:174, :191); many rule sets do too (rules_js `npm_import`, rules_python
`whl_library` for wheels, gazelle `go_repository`, rules_rust toolchains,
...).

### The key: the predeclared inputs hash and the recorded inputs

`DigestWriter.computePredeclaredInputHash` (:170-210) fingerprints the
marker file version, the Starlark semantics, the rule's `.bzl` label, name
and transitive `.bzl` digest, the repository name, its attribute values
(JSON), the host OS and architecture, the values of the rule's `environ`
variables and its recorded repository-mapping entries. The marker file is
that hash on its first line, then one `RepoRecordedInput.WithValue` per line
(`FILE:`, `DIRENTS:`, `DIRTREE:`, `ENV:`, `REPO_MAPPING:` ... with the value
observed during the fetch; :76-89).

### The local repo contents cache (`bazel/repository/cache/LocalRepoContentsCache.java`)

`--repo_contents_cache=PATH` (old name `--repository_contents_cache`;
default `<repository_cache>/contents`, empty disables it;
`RepositoryOptions.java:56-72`). Layout `<cache>/<predeclared hash>/<uuid>`
(the repository) + `<uuid>.recorded_inputs`. A lookup tries every candidate
of the hash whose recorded inputs are current, then links
`external/<name>` to it (`setupOverride`) and touches the recorded inputs
file; collection after `--repo_contents_cache_gc_max_age` (14d) once the
server was idle `--repo_contents_cache_gc_idle_delay` (5m).

### The remote repo contents cache (`remote/RemoteRepoContentsCacheImpl.java`)

* **Option**: the *startup* option `--[no]experimental_remote_repo_contents_cache`
  (`remote/options/RemoteStartupOptions.java:27-39`, default false). It
  installs `RemoteExternalOverlayFileSystem` over `<output_base>/external`
  (`RemoteModule.getFileSystemForBuildArtifacts`); the cache exists when the
  command has a `CombinedCache` (a `--remote_cache` and/or `--disk_cache`,
  `RemoteModule.initRepoHelpersAndOverlayFs`).
* **Policies** (`buildContext`): reads use `ANY_CACHE` (the disk cache,
  then the remote cache) under `--remote_accept_cached`, else none; a
  download writes through the disk cache; uploads use `REMOTE_CACHE_ONLY`
  under `--remote_upload_local_results` ("the local repo contents cache is
  a better alternative for local caching").
* **Entries**: Action Cache entries of a synthetic action that is never
  executed: a fixed `Command` (argument: a GUID; outputs `.recorded_inputs`
  and the directory `repo_contents`), the empty input root, and the key
  hash as the `Action.salt`.
  * An *intermediate* entry (exit code 0, no outputs, a stdout blob) lists,
    one line per alternative, a batch of space-separated `RepoRecordedInput`
    keys (`RepoRecordedInput.WithValue.splitIntoBatches`: a new batch starts
    at an input that cannot be requested unconditionally). The next key is
    the rolling hash `Fingerprint(previous hash, input + " " + value)` over
    the batch with the *current* values; lines are added newest first,
    unbounded (a racy read-modify-write).
  * A *final* entry has one output file (the marker file, inlined when the
    server agrees) and one output directory (the repository's `Tree`).
* **Lookup** (`lookupCache`/`fetchFinalCacheEntry`): from the predeclared
  inputs hash, follow intermediate entries with the current values of the
  listed inputs until a final entry; read its marker file, check every
  recorded input again (`isAnyValueOutdated`: a warning `Unexpectedly
  outdated cached repo …` and a fetch when one changed), then inject the
  `Tree` into the overlay file system: file metadata only, contents
  downloaded when Bazel or a local action reads them, files read-only
  (`RemoteExternalOverlayFileSystem.java:327-331`).
* **Upload** (`addToCache`): the intermediate entries for the recorded
  input batches, then the final entry through `UploadManifest` (the marker
  file and every file of the repository as CAS blobs, the `Tree`), only for
  a repository fetched into the overlay file system; failures are warnings
  `Failed to upload repo contents to remote cache for repo …`.

## bazel-rs

### What is implemented

* `--[no]experimental_remote_repo_contents_cache` is a modelled startup
  option (command line or rc `startup` line, the last value wins). As a
  command option it is `Unrecognized option` (exit 2), with a value it is
  genuine's `does not take a value` error (exit 1 here, 2 in genuine: a
  native difference shared by every `OptionProcessor::SplitCommandLine`
  failure). It is part of neither the analysis snapshot's key
  (`snapshot::key::console_or_cache_only`) nor `bazel affected`'s digests
  key (`affected::digests_key_of`): it never changes a result, so runs with
  and without it share snapshots and remote base digests.
* With a `--remote_cache` (HTTP(S) or gRPC, with the configured headers,
  credentials and credential helper) and/or a `--disk_cache`, under the
  same policies as genuine: lookups under `--remote_accept_cached` (disk
  cache first, a download writes through it), uploads under
  `--remote_upload_local_results` to the remote cache only.
* **Eligibility**, as genuine: non-local rules only (a local rule never
  reaches the lookup, `repository_store::materialize_local`), never under
  `bazel fetch --force`. Uploaded are executions whose rule returned
  `repo_metadata(reproducible = True)` and whose publication is portable:
  no workspace dependence (recorded probes without `workspace`,
  docs/WORKTREES.md), not self-referencing and naming no other store tree
  (docs/REPOSITORY_SLOTS.md), every symlink relative and inside the tree
  (genuine's `safeForRemoteCache`), and its recorded inputs hold for the
  published tree (a later warm reuse would take it). Built-in, registry and
  Git module trees (`trees/`, `git/trees/`) are not repository-rule
  publications and are not cached (they are downloads the repository
  cache already covers). Download-dominated repositories are skipped (no
  lookup, no upload; "Skipped repositories" below).
* **Key**: the repository's native specification key (`spec_key`: rule
  definition, the SHA-256 of every `.bzl` file of its closure and the
  repository mapping entries the closure's loads and top-level `Label()`
  calls used, attributes, host markers, declared environment values — the
  counterpart of the predeclared inputs hash, docs/REPOSITORY_RULES.md),
  never the workspace path: checkouts in other directories share entries.
  See "Keys and repository mappings" below.
* **Recorded inputs**: bazel-rs records a repository's watched inputs as
  one opaque probe record (`repository_warm::RecordedInputs`), not as
  genuine's ordered list. An intermediate entry (salt `spec:<spec_key>`)
  therefore lists, newest first and at most 8, the SHA-256 of each stored
  record; each names a final entry (salt `final:<spec_key>:<sha256>`). A
  lookup tries them in order and takes the first whose probes hold now
  (`repository_warm::unchanged`, the warm reuse's own check: before the
  download without the probes of files inside the tree itself, `stage:`,
  and after it the whole record against the downloaded tree) and whose
  blobs are all there. Entries are untrusted input: the probes must have a
  form an upload can carry (version 2, 4 or 5, no `trees`, no `workspace`, no
  probe through the repository's own `@@<name>//` label), and the manifest
  must describe a writable tree (normal relative paths with listed
  parents, no special mode bits, every link inside the tree — also through
  the tree's other links: no intermediate component of a target may be a
  link of the tree).
* **Final entry**: output files `manifest.json` (every entry of the tree:
  path, directory/file/link, the mode the rule left before the store sealed
  it, size, SHA-256, link target; and the pack's chunks),
  `recorded_inputs`, `runtime_inputs` (the execution's runtime input
  provenance, part of the tree identity) and the pack chunks
  `repo_contents/NNNNNN`. The **pack** is the regular files' contents
  concatenated in manifest order, cut every 16 MiB and stored as zstd frames
  (one CAS blob each): one request per 16 MiB instead of one per file (a
  per-file layout needed 895k requests for ~/code, which latency makes
  unusable over a WAN) and compressed. A chunk is decoded no further than
  the size the manifest gives it (a small frame may claim gigabytes).
  bazel-rs's GUID, with the store's tree profile and manifest format (a
  bump of either re-keys the cache), salts every key;
  entries are never shared with genuine's (different keys and recorded
  inputs).
* **Hit**: under the repository's slot writer lock, after the warm probes
  missed (the same place genuine looks up), the tree is written into the
  slot `<cache>/repositories/external/<name>` with the modes the rule left,
  every chunk verified as a CAS blob and every file against its SHA-256 as
  it is written; then the store publishes it exactly as if the rule had
  produced that tree with those runtime inputs: the same snapshot, sealing,
  identity (local provenance + runtime inputs + snapshot), manifest,
  specification pointer and slot binding, and the same post-publication
  re-probe that lets the analysis snapshot record the publication as a
  reuse. The lockfile, the hidden lockfile, stderr (with the action graph)
  and every tree identity are byte-identical to a real fetch (tests, and
  ~/code below). The analysis snapshot's recording is a superset of a real
  fetch's: the candidate's probes are checked (through the recording
  facade) before the download and again after it, besides the re-probe; a
  later validation of the snapshot reads the same current values. A
  missing chunk falls back to the fetch silently; a corrupt one, recorded
  inputs that do not hold for the downloaded tree, or any transfer failure,
  with a warning `Remote repo contents cache lookup failed for <repo>: …`;
  after 5 such failures lookups stop for the rest of the command (no
  warning per repository when the cache is down).
* **Upload**: synchronous after the publication, like genuine's (its
  `TODO: Consider uploading asynchronously`); each file is verified against
  the manifest as it is packed; the slot's writer lock is released first.
  Failures warn `Failed to upload repo contents to remote cache for
  <repo>: …`; after 5 of them uploads stop (counted apart from lookups: a
  read-only token with the default `--remote_upload_local_results` keeps
  its hits).
* **Concurrency**: lookups run where a repository rule would execute
  (`materialize_inner`): on the coordinator and on repository speculation
  workers, under the per-repository slot writer lock. Extension
  speculation and the reuse-only probes (`try_reuse_direct`) never reach
  it. Transfers of different repositories run in parallel (16 MiB chunks
  bound the memory of each: one chunk downloads while the previous one is
  written); a cancelled speculation stops between chunks.
* **Laziness**: none. Genuine keeps hit repositories in an overlay file
  system and downloads a file when it is read; bazel-rs's store publishes
  complete, sealed, verified trees, so a hit downloads the whole tree.
* **Speculation workers**: a lookup on a parallel-fetch worker
  (crate::repository_speculation) whose recorded-input check needs a
  repository the worker may not fetch (the no-demand sentinel, e.g. the
  `go.env` of gazelle's `go_repository_cache`, which the cache never
  stores) decides nothing: the attempt aborts like an execution demanding
  that repository and is retried once it is fetched — it is neither a miss
  nor followed by an execution of the rule. (Before, it was counted as
  `recorded inputs changed` and the rule started on the worker until it hit
  the same demand: on ~/code's CI the ~152 such `go_deps` lines per run
  were these aborted attempts, the retries hit.)
* `BAZEL_RS_REMOTE_REPO_CACHE_STATS=1` prints `REMOTE-REPO-CACHE hits=…
  downloaded_bytes=… uploads=… uploaded_bytes=…` (compressed bytes);
  `=verbose` adds one `REMOTE-REPO-CACHE miss <repo>: <reason>` line per
  miss (`no entry`, `recorded inputs changed: <input>`, `incomplete
  entry`, `missing blob`, `skipped (download-dominated)`) — a changed
  recorded input is named with its recorded and current state, e.g.
  `file @@<repo>//go.env at <path>: file:<sha256> (now file:<sha256>)`,
  `mapping @<name> from @@<repo>: …`, `watched environment <names>` — and
  one `REMOTE-REPO-CACHE not uploaded <repo>: <reason>` line per eligible
  execution that was not uploaded (`depends on the workspace`, `names a
  store path`, `a symlink leaves the tree`, `recorded inputs do not hold
  after the execution: <input>`).

### Keys and repository mappings

Bazel's predeclared inputs hash covers the repository mapping entries the
rule's `.bzl` closure recorded (`recordedRepoMappingEntries`), and the
lookups of `Label()` calls while the rule runs are REPO_MAPPING recorded
inputs of its marker file — never whole mappings. bazel-rs's specification
keys (and the content identities, which frame the same provenance) once
held the whole mapping of every `.bzl` file's repository, of the declaring
module and of the repository itself, and an extension's `use_repo`
imports: adding a dependency, a `use_repo_rule` repository or a `use_repo`
alias to the root module re-executed every repository the root module
declared and every repository of an extension defined in the root module
(and missed the remote cache for all of them). Now (direct profile
`direct-file-template-v2`, extension profile `ordinary-file-template-v3`):

* the definition closure enters a key as the SHA-256 of each `.bzl` file
  plus the sorted mapping lookups its load statements and top-level
  `Label()` calls recorded (from, apparent name, canonical result);
* `Label()` lookups while a repository rule runs are recorded probes
  (`repository_warm::RecordedInputs` version 5, `mappings`), checked on
  every warm reuse and remote hit by resolving the same apparent name from
  the same repository again (`loading::current_repo_mapping`);
* every apparent-name resolution of `Label()` and `Label.relative()` is
  recorded under the repository whose mapping resolved it — also `Label`
  called from native code without a `.bzl` frame (`sorted(names, key =
  Label)`); debug builds assert that a repository rule's run recorded every
  such resolution. `Label.relative()`'s lookups, which Bazel's recorder
  does not see, key repositories (load time) and are recorded inputs (run
  time) but never enter an extension's lockfile entry, which stays
  genuine's;
* an extension's `use_repo` imports are still checked (the import error)
  but frame nothing; its evaluation stays part of every generated
  repository's key (tags, recorded extension inputs, the lockfile entry).

Consequences: an unrelated dependency or repository leaves every key
unchanged (tests/repo_mapping_key.rs, tests/remote_repo_cache.rs
`unrelated_dependencies_keep_remote_hits`); a changed entry a rule's
closure used re-executes the rules of that closure; a changed entry a rule
used while it ran re-executes that rule only. The lockfile, the hidden
lockfile and stderr (with the action graph) of a reused repository equal a
fresh fetch's. What remains unrecorded is the same class genuine leaves
unrecorded: mapping-dependent conversions at load time that reach a
repository only through their printed form without a recorded lookup (for
example a top-level `select()` whose apparent-label keys a rule writes out
via `repr`). Upgrading re-executes every repository once (new keys and
identities; the old trees are collected by the cache GC) and re-populates
the remote cache once.

Keys never depend on the bazel-rs binary (its version, build or an
unrelated change): only the store's profiles and formats, which an
intended rekey bumps and documents (tests/repository_rules.rs
`repository_keys_are_independent_of_the_binary` pins a direct and a
generated repository's key). Checked on ~/code (`build --nobuild
//src/beyondwork.ai/...`, ~5,100 repositories, one cache): 0.4.21 → 0.4.22
and 0.4.23 → later main builds reuse every repository; 0.4.22 → 0.4.23
re-executes every one once (this section's rekey). A CI runner whose cache
was last used by 0.4.21 therefore re-fetched everything under 0.4.23 once.

### Skipped repositories

Repositories whose fetch is a checksum-pinned download about the size of
their tree gain nothing from the cache: it would move the same bytes from
another server. bazel-rs skips them — no lookup, no upload — by an
explicit rule on the repository's rule and attributes
(`remote_repo_cache::skipped`), never by sizes: rules_python's wheel
repositories, `@@rules_python+//python/private/pypi:whl_archive.bzl%whl_archive`,
and `…:whl_library.bzl%whl_library` given a wheel (`whl_file`, or `urls`
with a `filename` ending in `.whl`, the test of rules_python's own
`whl_library` macro). Their trees hold the wheel and its extraction (on
~/code 364 repositories, 12.0 GB of the 22.1 GB of trees). Sdist repositories
(built locally, not reproducible) were never stored. `verbose` statistics
list them as `skipped (download-dominated)`.

### Security

With the option on, write access to the cache is write access to the
contents of every reader's repositories: an entry is trusted like
genuine's (its recorded inputs must hold and its blobs must match their
digests, but nothing proves the tree is what the rule produces). Treat the
cache's write credentials like push rights to the repository, and give
readers that must not trust other writers a read-only token
(`--noremote_upload_local_results`) or a separate cache. Uploads carry
what the repository recorded: host paths of files it watched outside the
workspace, the digests (not values) of watched environment variables, the
digests of template sources, and the tree's bytes themselves.

### The local `--repo_contents_cache`

Unchanged (docs/REPOSITORY_CACHE.md "Repo contents cache"): accepted with
genuine's checks; bazel-rs's own content-addressed store, shared by every
worktree of a cache, plays its role.

## Measurements (~/code)

Private clone at 80d6724f3a, private lockfile copy, fresh `--cache-dir`
(the snapshot.debian.org packages seeded into its download cache as in
docs/COLD_FETCH.md), cold `build --nobuild //src/...`, release build without
LTO, `nice -n 10`, a local HTTP cache server (`/ac/`, `/cas/`, a small
threaded Rust server on loopback), fixed checkout and cache paths (as on a
CI runner), one run each, seconds:

| run | wall | eth0 received | cache traffic | hits / uploads |
| --- | ---: | ---: | --- | --- |
| plain cold fetch (no remote cache) | 207.9 | 8,754 MB | — | — |
| populating (`--remote_upload_local_results`) | 242.4 | 8,759 MB | 14.1 GB up, 52k PUTs | 0 / 8,726 |
| cold with the populated cache (read-only) | **98.3** | 501 MB | 14.8 GB down, 35k AC + 45k CAS GETs | 8,726 / 0 |

With the wheel repositories skipped (repo-mapping-key branch; a later
series of runs, the network slower that hour: the plain fetch took 275 s):

| run | wall | eth0 received | cache traffic | hits / uploads |
| --- | ---: | ---: | --- | --- |
| plain cold fetch | 275.3 | 8,757 MB | — | — |
| populating, wheels skipped | 283.1 | 8,763 MB | 6.1 GB up (5.5 GB stored), 49k PUTs | 0 / 8,362 |
| cold, wheels skipped | **110.1** | 4,614 MB | 6.2 GB down, 34k AC + 42k CAS GETs | 8,362 / 0 |
| cold, everything cached (previous binary, same hour) | 92.0 | 500 MB | 14.8 GB down | 8,726 / 0 |

stderr, the lockfile and the hidden lockfile are byte-identical across the
plain, populating and cold runs. On loopback the cache's bandwidth is
unlimited, so skipping the wheels costs ~18 s here (4.6 GB from PyPI
instead of 8.6 GB more from the cache); over a WAN to the cache it saves
those 8.6 GB.

* The fetch phase ends at 189 s (plain) vs 81 s (hit); analysis is ~18 s
  in both. stderr (6.5M lines with the action graph), the lockfile and the
  hidden lockfile are byte-identical between the plain and the hit run.
* 8,726 of 8,851 repositories were uploaded and served; 91 others have no
  entry (non-reproducible rules: pip sdists built locally, the Python
  interpreter, local_config_*, toolchain hubs, `zig_config`, ...) and run as
  before; the remaining 34 are local rules, never looked up.
* The trees are 22.1 GB uncompressed (12.0 GB pip wheels — torch and the
  CUDA libraries —, 4.7 GB npm packages, 1.7 GB Go modules, 1.3 GB Rust
  toolchains); the pack is 14.7 GB (the wheels' shared objects hardly
  compress), more bytes than the 8.75 GB of original (xz/gzip) downloads.
  An earlier per-file layout (no compression, 895k requests) took 102 s
  against the fast local server and 260 s against a single-threaded Python
  server: request count, not bandwidth, dominates small files.
* Populating costs +35 s (synchronous upload, ~390 CPU-s of zstd
  compression) once per new repository version.
* With per-run cache directories the 500 `go_repository` repositories miss
  (`recorded inputs changed`): they watch the `go.env` of
  `go_repository_cache`, a self-referencing repository whose contents name
  the cache directory. A CI whose runners use one cache path
  (`~/.cache/bazel-rs/ci-cache`) shares them.

## What ~/code's CI needs (bazel-rs only)

The PR's bazel-rs invocations already pass the remote cache (`--remote_cache`
from `.bazelrc` with the storagebox credentials) for the action cache and
the base digests. Add the startup option to bazel-rs's invocations only
(genuine 9.3.1 would accept it too, but its lazy overlay is a different
trade-off and a server restart):

* in the workflow, for bazel-rs: `bazel --experimental_remote_repo_contents_cache affected …`
  / `… build …` (a startup option: before the command), or a `startup`
  line in an rc file only bazel-rs reads, e.g. the CI's
  `--bazelrc=…/ci.bazelrc` with `startup --experimental_remote_repo_contents_cache`;
* keep `--remote_upload_local_results` on (the default) on the jobs that
  should populate the cache (main-branch and PR jobs alike: every miss is
  uploaded once per repository version); jobs that must not write keep
  `--noremote_upload_local_results` (lookups only);
* use one cache path on every runner (it is already
  `~/.cache/bazel-rs/ci-cache`) so `go_repository` entries are shared;
* optionally `BAZEL_RS_REMOTE_REPO_CACHE_STATS=1` (or `=verbose`) to log
  hits, misses and bytes.

`bazel affected` passes its startup options to the planning processes, so
the option given before `affected` covers both revisions' plannings.

Volumes for this revision of ~/code (wheel repositories skipped): the first
populating run uploads ~6.1 GB (8,362 entries, ~49k requests; 5.5 GB
stored after deduplication); every cold runner then downloads ~6.2 GB from
the cache (~76k requests) plus ~4.6 GB from PyPI (the 364 wheels) and
~0.5 GB of registry files and the 91 non-reproducible repositories, instead
of ~8.8 GB from the public CDNs, registries and snapshot.debian.org. A
revision changes only the entries of the repositories it changes (an
unrelated dependency changes none).
