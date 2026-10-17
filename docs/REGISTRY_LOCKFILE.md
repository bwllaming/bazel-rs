# MODULE.bazel.lock, registry file cache and parallel registry fetching (N37)

Native `query` and `build` read and write the workspace `MODULE.bazel.lock` the
way Bazel 9.3 does for the parts that registry resolution determines, keep
hashed registry files in the repository cache, and fetch registry files in
parallel. The pinned source is `/tmp/opencode/bazel-9.3-reference`; paths below
are relative to `src/main/java/com/google/devtools/build/lib/bazel/`. `mod graph`
keeps its previous profile and still requires `--lockfile_mode=off` for remote
registries.

## Lockfile format and reading

- `lockFileVersion` 28 (`bzlmod/BazelLockFileValue.java:50`). The fields are
  written in this order: `lockFileVersion`, `registryFileHashes`,
  `selectedYankedVersions`, `moduleExtensions`, `facts`, `factsVersions`
  (`:119-154`). Formatting is Gson pretty printing (`bzlmod/GsonTypeAdapterUtil.java:401-414`,
  `disableHtmlEscaping`): two-space indent, `": "`, empty maps `{}`, Gson string
  escapes, followed by `"\n"` (`bzlmod/BazelLockFileModule.java:336-356`).
  `registryFileHashes` is sorted in Java `String` order (`:163-167`). A file that
  could not be found is recorded as `"not found"` (`GsonTypeAdapterUtil.java:375-395`).
- Reading follows `bzlmod/BazelLockFileFunction.java:246-356`. A missing file is
  the empty lockfile. The version is located with the regex
  `"lockFileVersion":\s*(\d+)`. If the version differs, or the file has no
  version, the lockfile is empty in `update` and `refresh` mode. In `error` mode
  that is an error: `The version of MODULE.bazel.lock is not supported by this version of Bazel. Please run `bazel mod deps --lockfile_mode=update` to update your lockfile.`
  A version-28 lockfile whose extension entries fail `isValidLockfile`
  (`:339-356`) is treated the same way. Parse failures produce
  `Failed to read and parse the MODULE.bazel.lock file with error: <msg>.` followed
  by Bazel's suffix: either the merge-conflict suffix or ` Try deleting it and rerun the build.`
- `--lockfile_mode=off` never reads or writes the lockfile (`:250-252`,
  `BazelLockFileModule.java:53-54`).
- **The lockfile is written only when its value changes** (`BazelLockFileModule.java:186`).
  Value equality ignores map order, so a semantically equal file is left
  byte-for-byte untouched. Writes happen after the command, including a command
  that failed after resolution succeeded, but never in `error` or `off` mode. As
  in Bazel, a write failure does not fail the command (native prints a warning).
- Only remote URLs are recorded. `file:` registries are never written
  (`BazelLockFileModule.java:158-167`) and they ignore hashes
  (`bzlmod/RegistryFactoryImpl.java:59-71`).

## Registry files per mode (`bzlmod/IndexRegistry.java:166-243`)

`MODULE.bazel`, `source.json` and `bazel_registry.json` are fetched "with a
checksum" and recorded like `RegistryFileDownloadEvent` (`:157-163`).
`metadata.json` is mutable: it is never hashed and never recorded (`:586-592`).

| mode | file known with a hash | known `not found` | unknown |
|---|---|---|---|
| `off` | not used: fetched | not used: fetched | fetched |
| `update` (default) | repository cache, else fetched and verified | not found (`… previously not found (as recorded in MODULE.bazel.lock, refresh with --lockfile_mode=refresh)`) | fetched |
| `refresh` | repository cache, else fetched and verified | fetched again | fetched |
| `error` | repository cache, else fetched and verified | not found | error `Missing checksum for registry file URL not permitted with --lockfile_mode=error. …` |

- A hash mismatch fails with `Failed to fetch registry file URL: Checksum was X but wanted Y`
  (`repository/downloader/HashInputStream.java:82-93`). For `MODULE.bazel` it is
  wrapped as `Error accessing registry R: …` (`bzlmod/ModuleFileFunction.java:846-848`).
- Yanked versions (`IndexRegistry.java:612-650`):
  - `refresh` always fetches `metadata.json`.
  - A module in `selectedYankedVersions` reuses that recorded reason.
  - `error` never fetches.
  - A selected module whose `source.json` URL is in the lockfile counts as not
    yanked.
  - Otherwise `metadata.json` is fetched.

  `checkNoYankedVersions` (`bzlmod/BazelModuleResolutionFunction.java:340-375`)
  reports Bazel's `Yanked version detected in your resolved dependency graph: …`
  unless the module is allowed by `--allow_yanked_versions` (repeatable) or
  `BZLMOD_ALLOW_YANKED_VERSIONS`. Both are parsed like
  `bzlmod/YankedVersionsUtil.java:30-108`, including `all`.
  Allowed selected yanked versions are recorded in `selectedYankedVersions`.
- With a lockfile in use, every selected remote registry module's `source.json`
  is read after selection. So is `bazel_registry.json` for `archive`/`local_path`
  sources. This mirrors the repo-spec loop at
  `BazelModuleResolutionFunction.java:113-131` and `IndexRegistry.java:345-391`,
  so the lockfile records the same files Bazel records. Errors use Bazel's
  `Unable to get module repo spec for M from registry: …` text.

## Repository cache, memo and parallel fetching

- Hashed registry files are stored in `<cache-dir>/repositories/downloads/sha256/<hex>`.
  That is the same content-addressed directory the archive downloader uses. A
  stored file is used only when its SHA-256 matches the lockfile hash. This
  matches `DownloadManager.downloadAndReadOneUrlForBzlmod`
  (`repository/downloader/DownloadManager.java:414-497`): look up a known
  checksum first, and put every download into the cache.
- Each URL is fetched at most once per invocation, so `bazel_registry.json` is
  read once per registry.
- Discovery fetches every horizon in parallel, as Bazel's
  `bzlmod/Discovery.java:113-137` does through Skyframe. The `metadata.json` and
  `source.json` passes are parallel too. Prefetching is bounded to 32 concurrent
  requests and follows registry fallback order (`ModuleFileFunction.java:820-855`).
  Results, including errors, are consumed in the original sequential order, so
  diagnostics stay deterministic.

## Module extensions and facts (native scope)

The native profile does not compute `moduleExtensions` entries: it has no
`bzlTransitiveDigest`, `usagesDigest` or recorded inputs. **Native writes
version-28 lockfiles without the extension entries Bazel would regenerate.**
For a fresh or old-version lockfile, Bazel would add an entry for every
non-reproducible extension it evaluated; native writes `"moduleExtensions": {}`.
Genuine Bazel re-evaluates those extensions and adds the entries on its next
write.

- Existing `moduleExtensions`, `facts` and `factsVersions` values are kept
  verbatim. The original number spellings and member order are preserved, so
  they re-serialize exactly as Gson does. Extensions are always re-evaluated, and
  no entry is ever added.
- Before writing, the sections are pruned like `BazelLockFileModule.afterCommand`
  does when there are no new results:
  - Extension entries are kept only for extension IDs in the current usage
    table (`combineModuleExtensions`, `:270-280`).
  - Facts are kept only for used extensions and only when not `Facts.EMPTY`
    (`:226-235`).
  - `factsVersions` are kept only for kept facts and only when non-zero
    (`:237-249`).

  The usage table follows `BazelDepGraphFunction.java:97-137`: active usages,
  labels resolved with each module's bazel_dep mapping, and IDs spelled as in
  `GsonTypeAdapterUtil.java:176-187`. An isolated usage matches by its
  `label%name%<module key>+` prefix, because native does not record the exported
  proxy name. If the usage table cannot be built, the command fails with the
  error the lazy path reports, and nothing is written. Bazel fails the command
  as well (`getExtensionUsagesById`, `:115-130`) and writes nothing without a
  `BazelDepGraphValue` (`:95-100`). Before N51 native silently skipped the
  write. The message text differs: Bazel reports `invalid label for module
  extension found at <loc>: no repo visible as '@x' here`. Order is the file
  order; Bazel sorts by `ModuleExtensionId.LEXICOGRAPHIC_COMPARATOR`, which is
  already the order of a Bazel-written file.
  - Genuine Bazel reads such a file the same way: missing entries are evaluated
    again, and stale entries are detected by their digests.
  - Bazel itself would record new non-reproducible extension entries and drop
    entries of extensions that are no longer used. Native does neither.
    `--native-lockfile` exists so probes avoid that difference in a shared
    checkout.
- Persisted facts are not passed to extensions. An extension whose ID has facts
  in the lockfile fails explicitly in every lockfile mode.
- In `error` mode, a non-reproducible extension, or one that returns facts,
  fails:
  - Bazel's `The module extension 'ID' does not exist in the lockfile` when the
    lockfile has no entry for it.
  - Otherwise an explicit "validating … is not implemented" error.
- The hidden output-base lockfile is not implemented.

## Default registry and offline behaviour

- Without `--registry`, query/build/mod graph use Bazel 9.3's default list
  `https://bcr.bazel.build/` (`BazelRepositoryModule.java:133`; RepositoryOptions
  `registry` defaults to null, `repository/RepositoryOptions.java:100-110`). Any
  explicit `--registry` replaces the default, and the builtin-module fixture
  registry is passed explicitly as before. The library `ResolverOptions` default
  stays empty.
- `mod graph` keeps its off-only lockfile profile, so even the default registry
  needs an explicit `--lockfile_mode=off`.
- There is no offline mode. `--repository_disable_download` gates archive,
  overlay and patch acquisition only. Registry metadata is still fetched, as in
  Bazel, where `DownloadManager.disableDownload` (`DownloadManager.java:331`) does
  not apply to `downloadAndReadOneUrlForBzlmod`. `--nofetch` is unsupported.
- Ways to run without network:
  - Pass an explicit `file://` registry; it can be empty.
  - Use a lockfile that records every registry file, together with a populated
    `--cache-dir`. In `update` mode every hashed file comes from the repository
    cache and `metadata.json` is skipped. `error` mode additionally refuses any
    file the lockfile does not record.
  - Use `--lockfile_mode=off` with only main-repository targets. This keeps the
    lazy profile described below.
- Tests and validators always pass an explicit registry and lockfile mode. An
  `strace -e connect` audit of `cargo test` shows no connection to
  bcr.bazel.build.

## Eager resolution boundary

Bazel resolves the module graph in every command. Native resolves eagerly only
when a lockfile mode other than `off` is combined with a remote (`http`/`https`)
registry, including the default `https://bcr.bazel.build/`, and the workspace
has a `MODULE.bazel`. Otherwise native keeps its
lazy profile (`NATIVE_BOUNDARY` tests): a main-repository-only query does not
resolve, and nothing is written. As a result, with `--lockfile_mode=off` a bad
extension label, or a dependency's `bazel_tools` name collision, fails only when
demanded. Bazel fails every command (`artifacts/eager_mapping-followup-check.py`
rows F1/off).

## Native-only `--native-lockfile=workspace|readonly|/absolute/path`

- `workspace` (default): Bazel's behavior.
- `readonly`: reads the workspace lockfile and never writes any lockfile.
- An absolute path is a private lockfile:
  - It is read instead of the workspace lockfile. If it does not exist yet, the
    workspace lockfile is read instead, as a seed.
  - Updates are written to the private path. The workspace file is never
    written.

**Fast warm probes of real workspaces** use
`--lockfile_mode=update --native-lockfile=/private/MODULE.bazel.lock` with a
private `--cache-dir`:

- The first run seeds the private lockfile from the checkout. A checkout file of
  another version is ignored, as Bazel does.
- Later runs serve every hashed registry file from the cache without network
  access. On ~/code a warm query takes 1.1–1.2 s, against about 6 s with
  `--lockfile_mode=off`.
- The checkout is never written.

`--lockfile_mode=off` remains "neither read nor write".

## Other limits

- Registry URLs must be spelled in normalized form when a lockfile is used.
  Bazel keys entries by the URL as given (`IndexRegistry.java:134-143`); a
  non-normalized `--registry` URL is rejected explicitly. Override registries in
  `MODULE.bazel` are not checked.
- Not implemented: Gson's lenient JSON extensions, vendor mode, module mirrors,
  and `%workspace%` registry URLs.
- The native "not found in registries" text keeps its own wording,
  `module M@V was not found in registries [R, ...]`, because the validators
  match it. Bazel prints `module M@V not found in registries:` followed by one
  `* URL: reason` line per registry. Both name the registries, including the
  default `https://bcr.bazel.build/`.
- Gson parse messages are not reproduced. The wrapper and suffix are Bazel's;
  the inner message is native.
- An invalid lockfile checksum crashes Bazel 9.3 (uncaught `JsonParseException`).
  Native reports `Invalid checksum: …` through the parse-error wrapper.
- `selectedYankedVersions` order for several entries that were re-fetched from
  `metadata.json` is resolution order. Bazel uses `HashMap` order there; the
  order does not affect value equality.
- Local registries keep lazy `source.json` reads, and their files are never
  recorded.

## Evidence

- Paired capture: `artifacts/lockfile-capture.py`, receipt
  `tests/fixtures/registry_lockfile/reference_receipt.json`.
- ~/code timings: `artifacts/code_lockfile_probe.py`.
- Tests: `tests/registry_lockfile.rs`, and the unit tests in `src/lockfile.rs` and
  `src/registry_session.rs`.
