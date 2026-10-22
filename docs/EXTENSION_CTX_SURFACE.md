# repository_ctx / module_ctx surface (N43)

Bazel 9.3 implements `repository_ctx` and `module_ctx` as two subclasses of
one class, `StarlarkBaseExternalContext`
(bazel/repository/starlark/StarlarkBaseExternalContext.java). bazel-rs now does
the same: `module_ctx` evaluates every base member with the native
`repository_ctx` implementation (`src/repository_ctx.rs`, `base_methods`), with
the module-extension differences Bazel makes
(ModuleExtensionContext.java:72-101). The former narrow module I/O profile
(owned scratch files, Label-only `path()`, path values without members,
`ScratchExecutionIo`) is gone.

## Inventory (Bazel 9.3 vs native)

Line numbers refer to the pinned source `/tmp/opencode/bazel-9.3-reference`
(SBEC = StarlarkBaseExternalContext.java, SRC = StarlarkRepositoryContext.java,
MEC = bzlmod/ModuleExtensionContext.java, SP = StarlarkPath.java).

| Member | Bazel | repository_ctx before | module_ctx before | now (both, unless noted) |
|---|---|---|---|---|
| `download(url, output, sha256, executable, allow_fail, canonical_id, auth, headers, *, integrity, block)` | SBEC:661-897 | yes; `block=False`, non-empty `auth`/`headers` rejected | scratch profile; `executable`, `block=False`, `canonical_id`, `auth`, `headers` rejected | yes, all parameters; `block=False` returns `PendingDownload` with `wait()` (SBEC:585-656); `auth` basic/pattern and `headers` (SBEC:350-441) |
| `download_and_extract(...)` | SBEC:907-1197 | yes; `auth`/`headers` rejected | missing | yes, with `auth`/`headers` |
| `extract(archive, output, strip_prefix, *, rename_files, watch_archive, type, strip_components)` | SBEC:1246-1400 | yes | missing | yes |
| `file(path, content, executable, legacy_utf8)` | SBEC:1466-1527 | yes | missing | yes |
| `getenv(name, default)` | SBEC:1535-1565 | yes | yes | yes (shared) |
| `path(path)` | SBEC:1567-1601 | yes | Label/path only; strings rejected | yes; a string is relative to the working directory |
| `read(path, *, watch)` | SBEC:1603-1647 | yes | scratch strings + strict Labels | yes; Labels resolve with getPathFromLabel and reads follow symlinks |
| `watch(path)` | SBEC:1757-1787 | yes | missing | yes, with module-extension watch rules |
| `report_progress(status)` | SBEC:1797-1826 | yes (no UI) | missing | yes (no UI) |
| `os` (`name`, `arch`, `environ`) | SBEC:1828-1841 | yes | yes | yes |
| `execute(arguments, timeout, environment, quiet, working_directory)` | SBEC:1970-2108 | yes | missing | yes; runs in the working directory |
| `which(program)` | SBEC:2323-2383 | yes | missing | yes |
| `load_wasm` / `execute_wasm` | SBEC:2110-2321, `--experimental_repository_ctx_execute_wasm` (off) | absent | absent | absent (flag-gated off in Bazel too) |
| `PendingDownload.wait()` | SBEC:640-651 | missing | missing | yes |
| `name`, `original_name`, `workspace_root`, `attr` | SRC:118-172 | yes | n/a | repository_ctx only |
| `symlink`, `template`, `delete`, `rename`, `patch`, `repo_metadata` | SRC:196-656 | yes | n/a | repository_ctx only |
| `watch_tree(path)` | SRC:568-609 | stub failing "not implemented" | n/a | repository_ctx only; records the directory and a tree digest |
| `modules`, `facts`, `is_dev_dependency(tag)`, `root_module_has_non_dev_dependency`, `extension_metadata(...)` | MEC:117-321 | n/a | yes | module_ctx only (unchanged) |
| `tag_sort_key(tag)` | MEC:163-184, TypeCheckedTag.java:41-69 | n/a | missing | module_ctx only; opaque `sort_key` ordered by (module BFS index, tag index), repr `<sort_key>`, print `<sort_key module=M tag=T>`, hashable |
| `is_isolated` | MEC:186-197, `--experimental_isolated_extension_usages` | n/a | absent | absent unless that flag is modelled (isolated usages are separate) |
| `path`: `basename`, `dirname`, `exists`, `is_dir`, `realpath`, `get_child(*)`, `readdir(*, watch)`, str/repr, `==`, hash | SP:43-212 | yes | type `path` without members | one `path` type for both contexts |

`dir(module_ctx)` is exactly Bazel's: `download`, `download_and_extract`,
`execute`, `extension_metadata`, `extract`, `facts`, `file`, `getenv`,
`is_dev_dependency`, `modules`, `os`, `path`, `read`, `report_progress`,
`root_module_has_non_dev_dependency`, `tag_sort_key`, `watch`, `which`.

## Module-extension semantics

* **Working directory**: `<cache>/modextwd/<unique name>` (Bazel:
  `<output_base>/modextwd/<unique name>`, RegularRunnableExtension.java:364-368),
  e.g. `.../modextwd/gazelle++go_deps`. Strings passed to `path`, `read`,
  `file`, `download`, `extract`, `execute(working_directory=)` resolve against
  it. Like Bazel it is created on first use (so `download(url)` with the
  default output creates a *file* at that path) and deleted when the
  extension finishes, successfully or not (MEC:96-101). An exclusive lock file
  next to it serializes native processes that share a cache (Bazel's
  output-base lock). Writes outside it fail with Bazel's `Cannot write outside
  of the repository directory for path ...` (SBEC:331-339); the store path
  profile of repository outputs does not apply (the directory is never
  published).
* **Watching** (`toRepoCacheFriendlyPath`, SBEC:1656-1704,
  `allowWatchingPathsOutsideWorkspace = false`, MEC:85): paths under the working
  directory are never watched (`watch='yes'`: `attempted to watch path under
  working directory`); paths in the workspace or in an external repository
  (natively: under the repository cache) are recorded; other host paths are
  skipped for `watch='auto'` and fail for `'yes'`/`watch()` with `attempted to
  watch path outside workspace, but it's prohibited in the current context`.
  `watch(Label)` records the canonical label. Recorded files, directory entries
  and host facilities (`execute`, `which`, `download`) are part of the
  extension's provenance (`extension_inputs`, version 2); downloads and
  working-directory reads are not inputs (as in Bazel). Watched paths are
  spelled as their RepoCacheFriendlyPath (`@@//rel`, `@@NAME//rel` for a path
  in external repository NAME), never as host paths into the repository store,
  so generated repositories' specifications do not depend on the cache
  location (review i20; docs/REPOSITORY_RULES.md). A store path that no
  repository root contains is never keyed to the workspace either (absolute,
  with the cache inside the workspace as outside it), and a MODULE.bazel.lock
  or hidden-lockfile key that names the store through the workspace
  (`@@//bazel-rs-out/repositories/...`) or absolutely — what bazel-rs wrote
  before review i20 — is outdated (`is a path into the native repository
  store`): the extension re-evaluates once and records `@@NAME//path` keys.
* **Labels** resolve like repository_ctx: the package must exist
  (RepositoryUtils.getRootedPathFromLabel); the file need not.
* **Identifying string**: checksum errors read `Checksum error in module
  extension LABEL%NAME: ...` (ModuleExtensionEvaluationProgress.java:26-28),
  repository rules `Checksum error in repository @@NAME: ...`.

## Repository download cache

Run 1 of the paired capture showed that genuine Bazel serves every download
whose checksum is already in its repository cache without a request
(DownloadManager.java:244-277; DownloadCache.java:146-221), even for a URL
that returns 404, and adds every completed download to the cache
(java:381-386). bazel-rs now does the same for `download` and
`download_and_extract` of both contexts: blobs live in
`<cache>/downloads/<algorithm>/<hex>` (shared with registry archives),
canonical ids as `<cache>/download_ids/<algorithm>/<hex>/id-<sha256(id)>`; a hit
is copied to the output (a matching existing output is still reused first) and
precedes the `--repository_disable_download` check, which then fails with
Bazel's `java.io.IOException: Failed to download IDENTIFYING: download is
disabled.` (java:331-333). A failing copy out of the cache is ignored (the
download proceeds, java:266-276); a failing store after a completed download
fails that download like Bazel (an uncaught IOException: `success = False`
under `allow_fail`, java:381-386). An output that is an existing directory is
still requested from every mirror and fails opening the output (`PATH (Is a
directory)`), as HttpDownloader does. Distdirs and URL rewriting are not
modelled.

## PendingDownload

`download(..., block = False)` returns a `PendingDownload` (no
`@StarlarkBuiltin`, so `type()` is `PendingDownload`; repr `<pending download
to 'PATH'>`; only member `wait()`). Argument errors (URLs, checksum syntax,
`checkInOutputDirectory`) are raised by `download()` itself; transfer failures,
`allow_fail` results and the deferred checksum error surface from `wait()`
(SBEC:658-697). The native transfer completes before `download()` returns, so a
download is never "pending after the callback finished" (SBEC:295-315); the
observable difference is only timing (no parallel downloads).

## Removed native caps

Bazel has no heap or step limit and bounds neither the inputs nor the results
of a module extension. Removed:

* evaluator limits of module extension and repository rule callbacks and of
  the definition `.bzl` files loaded for them (1 M ticks, 128 MiB heap); only
  the 128-frame call-depth bound remains (Starlark forbids recursion);
* the extension preparation host budget (64 MiB weighted bytes / 100,000
  nodes) for bound tags, recorded repository calls and generated specs
  (`HostBudget::unbounded`), the recorded-call caps (100,000 nodes and 16 MiB
  per call, 64 MiB total, 10,000 calls) and the 64 MiB provenance cap.

Real resource failures stay explicit: arithmetic overflow, allocation failure
of the provenance buffer, a value nesting deeper than 64 (cycle guard), I/O
errors. The JSON module's decode/encode budgets (docs/JSON.md) are unchanged.

## Also fixed while running ~/code's extensions

* A file that is not a zip archive fails like Bazel's ZipReader: `Error
  extracting ARCHIVE to PARENT: Zip file 'NAME' is malformed. It does not
  contain an end of central directory record.`

* Tar names, link names and ustar prefixes end at their first NUL like
  commons-compress `TarUtils.parseName`; python-build-standalone archives
  (rules_python toolchains) carry stale bytes after it in every header and were
  rejected as "embedded NUL".
* `Label.same_package_label(name)` (Label.java:576-582), `Label.relative(name)`
  (Label.java:593-634, enabled by the default
  `--incompatible_enable_deprecated_label_apis`) and `Label.repo_root`
  (Label.java:408-420).
* Label reads (`read`, `template`, `patch` with a Label) follow symlinks like
  Bazel; symlinked package directories remain a separate native package-lookup
  restriction.

## Paired capture

`artifacts/ext_ctx-capture.py` (offline fixtures, genuine Bazel 9.3.0 vs native,
`query` of extension and repository targets):
`tests/fixtures/ext_ctx_surface/reference_receipt.json`, run 2: 81 MATCH and
37 EXACT_MESSAGE, 0 mismatches (every module_ctx/repository_ctx member above,
`dir()` of both contexts, `print(sort_key)`, `print(module_ctx.facts)`, the
HTTP requests with auth/headers, 38 error texts). Run 1
(`history/run1_reference_receipt.json`, 9 mismatches) found the repository
download cache semantics described above. The N43b targeted rerun
(`targeted_rerun_n43b` in the receipt) replays the 33 module_ctx download/read
cases of the retired `scripts/validate_module_extension_io.py` (mirrors, SRI
sha1/sha256/sha384/sha512/blake3, gzip/x-gzip/redirect suffix rules, all-256
byte reads, http filtering, disabled downloads, empty URL lists, malformed
checksums, matching destinations, root-file/child outputs) on both engines:
23 MATCH, 9 EXACT_MESSAGE, 1 MISMATCH (download onto an existing directory,
fixed afterwards and listed in `post_capture_changes`).

## ~/code results

See `artifacts/PROGRESS.md` and the N43 final report: with N38 merged, the
genuine gazelle `go_deps`, rules_python `pip` and aspect_rules_js `npm`
extensions evaluate natively (`@pypi//:all`, `@npm//:all`,
`@org_golang_x_sync//errgroup:errgroup`, `@pypi//aiofiles:pkg`).
