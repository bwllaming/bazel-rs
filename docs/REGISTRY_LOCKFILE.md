# MODULE.bazel.lock, registry file cache and parallel registry fetching (N37, N52)

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
  that failed after its loading phase (target selection) succeeded, but never in
  `error` or `off` mode. A command that fails earlier — module resolution or any
  demanded module extension resolution (evaluation or lockfile reuse) — writes
  nothing at all, the hidden lockfile included: the write gate opens only when
  the whole resolution succeeded (loading phase). This fail-closed gate is
  **stricter than Bazel**: `BazelLockFileModule.afterCommand` combines whatever
  evaluated (:105-141, over `getDoneValues()`) and genuine Bazel 9.3 still
  rewrote MODULE.bazel.lock (184 `registryFileHashes`, `moduleExtensions: {}`)
  for a command that failed on a demanded extension (`fail()` in its
  implementation; the E1c2 observation) — an expected native mismatch. Stale
  entries are no loophole: a demanded extension that failed leaves no new
  result, so `combineModuleExtensions` keeps the old entry and a later write
  would re-serialize it unchanged (and the unchanged-value rule suppresses the
  write). As in Bazel, a write failure does not fail the command (native
  prints a warning). When workspace-lockfile bytes were actually written,
  native prints the diagnostic `bazel-rs: resolved N module extension(s) for
  MODULE.bazel.lock` (N counts extensions the invocation resolved — evaluated
  or reused from an up-to-date entry); Bazel prints nothing.
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

## Module extensions and facts (N52)

Native computes, writes, validates and reuses `moduleExtensions` entries and
`facts` as Bazel 9.3 does. Paths in this section are relative to
`src/main/java/com/google/devtools/build/lib/`; the implementation is
`src/lockfile_extensions.rs` and `RepositorySet::{extension_lock,
reusable_lock_entry, repositories_from_lock_entry, record_extension_lock}` in
`src/repository.rs`.

### Entry format

`moduleExtensions` maps a ModuleExtensionId, spelled like
`GsonTypeAdapterUtil.java:176-187` (`//:ext.bzl%ext`, `@@dep+//:x.bzl%x`), to
eval-factor keys (`general`, or `os:linux,arch:amd64` for `os_dependent`/
`arch_dependent` extensions; `bazel/bzlmod/ModuleExtensionEvalFactors.java`,
`RegularRunnableExtension.java:203-207`). Each LockFileModuleExtension
(`bazel/bzlmod/LockFileModuleExtension.java:30-73`) is written with these fields:

- **`bzlTransitiveDigest`**: base64 of BzlLoadValue's transitive digest for a
  Bzlmod key. It is the SHA-256 over the SHA-256 of the extension `.bzl` bytes,
  followed by the transitive digests of its loads in load-statement order
  (`skyframe/BzlLoadFunction.java:858-879`). Bzlmod keys add no builtins digest
  (`:1350-1365`). `@bazel_tools` files are the embedded Bazel 9.3 tools, so
  digests over them match Bazel 9.3 only. Lockfiles written by other Bazel
  versions differ wherever an extension loads `@bazel_tools`.
- **`usagesDigest`**: `SingleExtensionUsagesValue.hashForEvaluation`
  (`bazel/bzlmod/SingleExtensionUsagesValue.java:78-99`). It is the SHA-256 of
  the UTF-16LE code units (Guava `hashUnencodedChars`) of the compact Gson
  JSON of the trimmed usages. That JSON lists, per module key, the
  normalized `extensionBzlFile` (`ModuleFileGlobals.java:486-519`), the
  extension name, `proxies: []`, and the tags with their kwargs in call order.
  Tag values are written like `AttributeValuesAdapter` (`'`-wrapping of `@@…`
  and `'…'` strings, `None` members dropped), at `Location.BUILTIN`, followed
  by `repoOverrides: {}`. Then come the unique name (`+ext`, `dep++ext`), the
  abridged modules (`name`, declared `version`, key `<root>`/`name@version`/
  `name@_`), `repoMappings: {}` and `repoOverrides: {}`.
- **`recordedInputs`**: RepoRecordedInput.WithValue strings
  (`rules/repository/RepoRecordedInput.java`), with Bazel's escaping (`\s`,
  `\n`, `\\`, `\0` for null), in recording order
  (`StarlarkBaseExternalContext.java:255-280`, first value wins):
  1. `ENV:` for `module_extension(environ=...)`, sorted by name
     (`ModuleExtensionContext.java:91-93`).
  2. The static `REPO_MAPPING:` table, rows by repository name and columns by
     apparent name (`cmdline/Label.java:266-281`). It holds load statements
     and top-level `Label()` calls of the transitive `.bzl` closure, plus
     label-typed tag attributes (`RegularRunnableExtension.java:352-365`).
  3. Observations during the implementation in order: `FILE:` (read/watch/
     path-readdir type: digest, `DIR` or `ENOENT`), `DIRENTS:` (Fingerprint
     of the sorted names), `ENV:` (`getenv`) and `REPO_MAPPING:` (`Label()`
     with an apparent repository).

  Workspace paths are `@@//path` and external repository files `@@repo//path`.
- **`generatedRepoSpecs`**: in creation order, each repo spec has these parts:
  - `repoRuleId`: `@@bzl//pkg:file.bzl%rule`, the defining file and exported
    name.
  - `attributes`: only explicitly given, non-`None` attributes except `name`,
    `tags`, `deprecation` and `visibility`, in call order
    (`bazel/repository/RepoRule.java:87-120`). Values are the type-checked
    values: labels in `@@repo//pkg:name` form, escaped strings, ints, bools,
    lists and dicts (`bazel/bzlmod/AttributeValuesAdapter.java`).
- **`moduleExtensionMetadata`**: present unless the result equals
  ModuleExtensionMetadata.DEFAULT, whose comparison includes empty facts
  (`LockfileModuleExtensionMetadata.java:148-159`). Fields:
  `explicitRootModuleDirectDeps`/`explicitRootModuleDirectDevDeps` (omitted
  when null), `useAllRepos` and `reproducible`.

`facts` holds, per extension, the `extension_metadata(facts=...)` dict:
`json.encode` of the normalized value, re-indented by Gson
(`bazel/bzlmod/FactsAdapter.java`; object members whose value is `None` are
dropped on write, as Gson does). `factsVersions` holds the non-zero
`module_extension(facts_version=...)` of extensions that have facts.

### Which results are recorded, and where

After the command, BazelLockFileModule.afterCommand (`:63-226`) runs in
`update` and `refresh`. It combines the old entries with the results of this
invocation:

- An entry is kept only while the extension still has usages.

The usage table follows `BazelDepGraphFunction.java:97-137`: active usages,
labels resolved with each module's bazel_dep mapping, and IDs spelled as in
`GsonTypeAdapterUtil.java:176-187`. An isolated usage matches by its
`label%name%<module key>+` prefix, because native does not record the exported
proxy name. If the usage table cannot be built, the command fails and
nothing is written. Bazel fails the command as well
(`getExtensionUsagesById`, `:115-130`) and writes nothing without a
`BazelDepGraphValue` (`:95-100`). Native uses Bazel's text: `Error computing
the main repository mapping: invalid label for module extension found at
<file>:<line>:<col>: no repo visible as '@x' here`. The location is the
usage's first `use_extension` left parenthesis. Before N51 native silently
skipped the write.
- A new result replaces the entry with the same factors, and entries with
  other dependencies (`:262-330`) are dropped.
- IDs are sorted by `ModuleExtensionId.LEXICOGRAPHIC_COMPARATOR`.

Non-reproducible results go to the workspace lockfile. Results with
`reproducible = True` go to the **hidden lockfile**: Bazel's
`<output_base>/MODULE.bazel.lock`, which native keeps in the workspace's
state directory, `<cache-dir>/worktrees/<name>-<hash>/MODULE.bazel.lock`
(docs/WORKTREES.md; before per-worktree state it was
`<cache-dir>/hidden-lockfiles/<sha256 of the workspace path>/MODULE.bazel.lock`).
That file is read in update mode, and any read failure counts as empty
(`BazelLockFileFunction.java:75-98`). Facts go to both files. Results taken
from a lockfile are recorded again unchanged. As before, a file is written
only when its value changes, through a temporary file and a rename (another
worktree may read it as its seed at any time).

While a workspace has no hidden lockfile of its own, the first that exists
of these seeds it (native; an output base starts empty in Bazel): its own
of the flat layout, then — in a linked git worktree — its main checkout's,
in the per-worktree and the flat layout. A seed is validated entry by entry
like any lockfile, so a reproducible extension the worktree uses unchanged
is not evaluated again; the main checkout's entries (and facts) whose JSON
names the main checkout's path are dropped first (an attribute holding a
path below it would point the worktree's repositories into the main
checkout; paths into the cache both share are kept). A seeded hidden
lockfile is written to the workspace's own path when the command ends.

### Reuse and validation (`SingleExtensionEvalFunction.java:144-407`)

Except in `off` mode, the entry for the current factors is taken from the
workspace lockfile, or failing that from the hidden lockfile. It is
up to date when all of these hold:

- Its `bzlTransitiveDigest` and `usagesDigest` equal the current ones.
- No recorded input is outdated. Inputs are compared in list order with
  Bazel's describeChange texts. A `FILE` in an external repository fetches
  that repository, as Bazel does. A `REPO_MAPPING` that was not visible
  (null) is always outdated, because RepositoryMapping.get never returns null.
- For a reproducible entry, the lockfile facts are empty or equal to the
  hidden lockfile's facts.

An up-to-date entry is used **without running the extension**: no prints, no
downloads, no module_ctx side effects. Its repositories are rebuilt from
`generatedRepoSpecs`. Each `repoRuleId` is loaded from its `.bzl` file and the
attributes are deserialized like `AttributeValuesAdapter.read`. The use_repo
fixup is generated from the locked metadata, and the import checks run as
after an evaluation. With a lockfile in use, generated repositories are keyed
by what the result determines (rule definitions, attributes, mapping and
sources), so an evaluated result and a reused one have the same repository
identity. In `off` mode the previous evaluation-based identity is kept.

Facts passed to the extension as `module_ctx.facts` are the workspace
lockfile's (when its `factsVersions` matches `facts_version`), otherwise the
hidden lockfile's (`:145-183`). `Facts` supports `[]`, `in`, `get` and value
equality; `print` shows `Facts(<repr>)`.

`--lockfile_mode=error`:

- An outdated non-reproducible entry fails with
  ``MODULE.bazel.lock is no longer up-to-date because <reasons>. Please run
  `bazel mod deps --lockfile_mode=update` to update your lockfile.``. All
  reasons are listed, joined by `,`.
- A non-reproducible extension without an entry fails with
  `The module extension '<@@id>'[ for platform <factors>] does not exist in
  the lockfile`.
- Facts that differ from the workspace lockfile's fail with
  `… because the extension '<@@id>' has changed its facts: <repr> != <repr>. …`.

Messages print IDs as `ModuleExtensionId.toString`, for example
`@@//:ext.bzl%ext`. An extension that recorded a `REPO_MAPPING` to an
invisible repository prints Bazel's
`produced an invalid lockfile entry because it referenced @x` warning.

### Native limits (explicit failures)

- In `update` and `refresh`, an extension whose observations have no Bazel
  representation fails with `cannot write its MODULE.bazel.lock entry: …`
  instead of writing an inexact entry. Examples are a watched special file and
  a watched path in no known repository. Validating a `DIRTREE` input reports
  it as outdated with an explicit reason. It cannot be recorded by
  `module_ctx`.
- Tag values that Bazel's AttributeValuesAdapter cannot serialize fail
  explicitly. Bazel throws for these: floats, ints outside 32 bits, and
  non-string dict keys.
- Isolated usages, `override_repo` and `inject_repo` stay unsupported in
  MODULE evaluation, so `isolationKey` and `repoOverrides` are never needed.
- Innate `use_repo_rule` extensions are never recorded, as in Bazel.
- `--native-lockfile=readonly` is a native-only stateless probe mode; Bazel has
  no analogue. It reads the workspace lockfile and the hidden lockfile (so
  their up-to-date entries are still reused) but writes neither: an extension
  without an up-to-date entry runs again in every readonly invocation. An
  absolute-path target writes the private lockfile and the hidden lockfile.
- **Probes sharing a `--cache-dir` see reproducible-extension reuse.** In
  `update`/`refresh` with the workspace or a private-path target, the first
  run stores reproducible results in the cache's hidden lockfile and later
  runs with the same cache and workspace skip those extensions, as a Bazel
  output base does (their print output and side effects disappear). To observe
  evaluations, use a fresh `--cache-dir`, `--native-lockfile=readonly` on a
  fresh cache, or `--lockfile_mode=off`.
- **Entries recorded inside `@@bazel_tools`'s load closure in the
  explicit-module profile.** With `--experimental-builtin-modules=false`
  (the built-in modules are the default), `@@bazel_tools` has no
  contents (N51 makes it visible in every mapping but still unloadable;
  docs/BUILTIN_MODULES.md). A Bazel-written entry whose `recordedInputs` holds
  `REPO_MAPPING:bazel_tools,…` (a load or `Label()` inside a `@bazel_tools`
  file of the extension's closure) therefore looks outdated
  (`source repo @@bazel_tools doesn't exist anymore`): `update` re-evaluates
  the extension (whose `@bazel_tools` load then fails explicitly), and
  `error` fails explicitly where Bazel passes. Use the built-in profile for
  such lockfiles.
- The validator probes that asserted `module_extension(facts_version=N)` is
  unsupported were retired in N52 (`native.facts-version-unsupported` in
  `scripts/validate_direct_repositories.py`, `native.unsupported` in
  `scripts/validate_repository_rules.py`; plan counts 47 and 20). Nonzero
  `facts_version` is covered differentially by this section's receipt.

### Evidence

- The paired capture `artifacts/lockfile_extensions-capture.py` writes the
  receipt `tests/fixtures/lockfile_extensions/reference_receipt.json`. The
  fixture is `tests/fixtures/lockfile_extensions/workspace`, with a
  `file://` registry. Every step matches: exit status, lockfile bytes and
  whether the file was written, which extensions were evaluated (reuse), and
  error texts.
- CLI tests: `tests/lockfile_extensions.rs`. Unit tests are in
  `src/lockfile_extensions.rs`.
- ~/code: `artifacts/code_lockfile_extensions_probe.py`; see the receipt's
  `code_probe`.

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
resolve, and nothing is written.

Whenever native does resolve the graph, in any lockfile mode and with
`file:`/local registries too, it then builds Bazel's eager extension usage
table (`repository_catalog::usage_table`). A broken extension label therefore
fails the command exactly where Bazel's `BazelDepGraphFunction` would, even if
the command needs no repository mapping. Native-only preparation
(`repository_catalog::build`: canonical namespaces, imports, budgets) stays
lazy, like Bazel's extension evaluation. On ~/code the extra work is not
measurable: the warm `query` median was 6.380 s before and 6.360 s after
(`artifacts/eager_mapping-code-timing.json`).

**Residual divergence.** A command that never resolves the module graph can
succeed where Bazel fails on a broken dependency MODULE file or extension label.
Bazel resolves in every command. Native skips resolution for a
main-repository-only query without a lockfile session: `--lockfile_mode=off`,
or a workspace that needs no external repository. Example:
`artifacts/eager_mapping-followup-check.py` rows F1/off, where `query //:plain`
exits 0 natively and 48 in Bazel.

## Native-only `--native-lockfile=workspace|readonly|/absolute/path`

- `workspace` (default): Bazel's behavior.
- `readonly`: reads the workspace lockfile (and the hidden lockfile) and never
  writes any lockfile, not even the hidden one (N52).
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
- `bazel mod deps --lockfile_mode=update` — the remedy Bazel's own messages
  suggest for an outdated lockfile — is a known gap: the native `mod` command
  is the bounded graph profile and never writes MODULE.bazel.lock (only
  `query`/`build`/`run`/`test` do).

## Evidence

- Paired capture: `artifacts/lockfile-capture.py`, receipt
  `tests/fixtures/registry_lockfile/reference_receipt.json`.
- ~/code timings: `artifacts/code_lockfile_probe.py`.
- Tests: `tests/registry_lockfile.rs`, and the unit tests in `src/lockfile.rs` and
  `src/registry_session.rs`.
