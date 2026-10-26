# Direct repository-rule execution

This is a bounded native implementation of `use_repo_rule`, not complete Bazel
repository or module-extension compatibility. The official Bazel 9.3 executable
is used only for comparison, never as an execution fallback.

## Execution and ownership

Direct declarations in root and selected dependency modules are indexed without
loading their definitions. Demanding a generated repository loads the requested
public `repository_rule` export and its complete transitive `.bzl` closure. Frozen
implementation functions, including private helpers, remain alive throughout
execution. All declarations belonging to the same innate usage are type-checked
before any of that usage's repositories execute. Only demanded callbacks run.

Canonical names and mappings are MODULE-owner scoped. Generated repositories
inherit that owner's mappings plus their own usage's sibling repository names.
Labels supplied in MODULE attributes resolve in the declaring module; label
defaults resolve where their descriptors are defined. Shared runtime handles use
the same resolved graph and cache configuration. Cycles fail with a demand chain;
concurrent demands for the same in-flight slot in one library instance are
explicitly unsupported rather than waiting. Separate CLI processes can publish
to the same cache.

Ordinary extension usages now have a separate bounded executor described in
[MODULE_EXTENSIONS.md](MODULE_EXTENSIONS.md). Recorded isolated
extension identities lack exported variable names; their reservations are skipped
only where their underscore-prefixed namespace is disjoint from public direct
rule symbols. Other ambiguous namespace combinations fail explicitly.

## Supported surface

Callbacks expose immutable `ctx.os.name` and `ctx.os.arch` under the bounded
[host metadata profile](HOST_OS.md). `ctx.os.environ` is a fresh mutable
dictionary of the client environment (`repoEnv`) described there.
Both direct and generated repository fingerprints always include the captured
host name/architecture, introducing a documented one-time publication rekey.

`repository_rule` supports callable implementations, `attrs`, `doc` and retained
`local`/`configure` metadata and snapshotted list/tuple `environ` declarations
(`configure=True` affects only unsupported `fetch --configure` and vendoring). See
[LOCAL_REPOSITORIES.md](LOCAL_REPOSITORIES.md) for the bounded serverless lifecycle
and MODULE-only `native.bazel_version` binding. Attribute kinds are string,
string-list, ordered scalar string dictionary, ordered string-list dictionary,
int32, bool, label and label-list. Scalar dictionary behavior is documented in
[STRING_DICT.md](STRING_DICT.md); repository ASCII restrictions remain unchanged.
Dictionary values and nested lists are frozen; their full typing, tuple,
ordering and snapshot contract is in [STRING_LIST_DICT.md](STRING_LIST_DICT.md).
Defaults, mandatory attributes,
explicit `None`, and the pinned 9.3 behavior for declared underscore-prefixed
attributes are supported. Unknown `None` attributes are ignored, while unknown
non-`None` attributes fail. Output attributes are invalid for repository rules.

Callbacks receive real `ctx.attr` values, `ctx.name` (canonical), and
`ctx.original_name` (the invocation name). `ctx.attr.name` is also canonical;
undeclared attributes are absent (`hasattr` is false). `ctx.workspace_root` is a
`path` to the main workspace. Citations below are Bazel 9.3
`StarlarkBaseExternalContext.java` (SBEC), `StarlarkRepositoryContext.java`
(SRC), `StarlarkPath.java` and `StarlarkExecutionResult.java`.

### Paths

Every path argument accepts a string, a `Label` or a `path` (SBEC:1567-1601).
Strings resolve against the repository directory with Bazel's lexical
normalization (`.`/empty segments vanish, `..` never climbs above `/`, absolute
strings replace the base). Labels resolve to source files: the package must exist
(`Unable to load package for ...`), the file need not, and no implicit watch is
recorded (`--incompatible_no_implicit_watch_label`). `ctx.path(x)` returns a
hashable `path` value whose `str()` is the absolute path (bytes as Latin-1
characters) with `basename`, `dirname` (None at `/`), `exists`, `is_dir`,
`realpath`, `get_child(*relative_paths)` and `readdir(watch="auto")` in directory
order (StarlarkPath.java:77-196). During execution the repository directory is
its slot `<cache>/repositories/external/<canonical name>`, genuine's
`<output_base>/external/<name>` (docs/REPOSITORY_SLOTS.md); an ordinary
repository's slot links its published tree afterwards, a self-referencing one
stays a mutable directory (absolute links into it are replanted, below).

### File operations

* `ctx.file(path, content="", executable=True, legacy_utf8=False)` replaces a
  file or symlink at `path` and creates parents (SBEC:1466-1527).
* `ctx.template(path, template, substitutions={}, executable=True,
  watch_template="auto")` (SRC:258-359): sequential literal replacements in
  dictionary order; empty keys do nothing; a directory template fails with
  `attempting to use a directory as template`.
* `ctx.read(path, watch="auto")` returns Latin-1 bytes (SBEC:1603-1647);
  directories fail with `attempting to read() a directory`. Missing files fail
  with Java's `java.io.FileNotFoundException: PATH (No such file or directory)`.
  Bazel creates the repository directory lazily (first write, makeDirectories or
  `execute`), so before that `read('.')` fails with `FileNotFoundException ... (No
  such file or directory)` and `template(..., '.')` with `(Is a directory)`; the
  native profile reproduces this for the repository directory itself only.
* `ctx.file` over a non-empty directory fails with `java.io.IOException:
  [unix_jni.cc:660] PATH (Directory not empty)`; an empty directory is replaced.
* Labels whose package has no BUILD file fail with Bazel's `Unable to load package
  for L: BUILD file not found in any of the following directories. ...`
  (PackageLookupFunction.java:130-155).
* `ctx.delete(path)` (string or path only) returns whether something was
  removed (SRC:373-405). Workspace paths that are not ignored fail with Bazel's
  `delete() can only be applied to external paths ...`; the native profile
  refuses other paths outside the repository directory.
* `ctx.rename(src, dst)` fails if `dst` exists (SRC:407-473), with Bazel's
  `java.io.IOException: Could not rename ...` texts (including unix_jni's
  `[unix_jni.cc:638] SRC -> DST (REASON)`).
* `ctx.symlink(target, link_name)` creates a link to the ABSOLUTE target path
  (SRC:196-256), so `readlink`/`realpath` inside the callback observe Bazel's
  values; an existing link name fails with `... [unix_jni.cc:297] LINK (File
  exists)`. Divergence: genuine Bazel 9.3 leaves links into the repository
  directory absolute in `<output_base>/external/<repo>` (captured; its
  `RepositoryUtils.replantSymlinks` only runs when moving a repository into the
  repository contents cache). The native repository directory is published as a
  content-addressed directory whose name depends on these very links, so after
  the callback the native profile rewrites links into the repository directory
  (from `ctx.symlink` or executed tools) as relative links with the same
  resolved contents. A link resolving outside it keeps what it resolved to
  there: a relative one becomes absolute, one into a sibling repository's
  directory is pinned to that repository's tree (docs/REPOSITORY_SLOTS.md).
  Other absolute targets stay as they are. The store records link targets without
  following them. Archive links from `ctx.extract`/`ctx.download_and_extract`
  follow N19's extraction semantics (docs/ARCHIVE_SYMLINKS.md). When a file of
  the published repository is used as a source artifact or loaded as a `.bzl`,
  source resolution follows these links inside the repository root and, since
  N26, into the root of any repository already materialized in the current
  invocation (main workspace, a module, the rules_cc `local_config_cc` link from
  `unix_cc_configure.bzl:362-370` into `rules_cc`); see
  docs/ARCHIVE_SYMLINKS.md "Cross-repository links". Since N28d these
  repository-rule-generated trees may also be read through links to other host
  paths (the `local_jdk` shape), for source artifacts, `.bzl` loads and `glob()`,
  never into the repository store (`source_links::LinkPolicy`,
  docs/GLOB_SYMLINKS.md); `glob()` follows links like Bazel. Package
  directories through links stay unsupported.
* `path.realpath` resolves component by component; a dangling link fails naming
  the missing target: `[unix_jni.cc:382] TARGET (No such file or directory)`.
* `ctx.extract(archive, output="", strip_prefix="", ...)` and
  `ctx.download_and_extract(url, output="", sha256="", type="", strip_prefix="",
  allow_fail=False, canonical_id="", auth={}, headers={}, integrity="", ...)`
  (SBEC:907-1400) validate the legacy `stripPrefix` alias, `strip_components`,
  archive types (`Expected a file with a ... suffix`, `No decompressor found for
  type ...`) and URLs/checksums with Bazel's messages, then extract with the
  native extractor (zip/jar/war/aar/nupkg/whl, tar, tar.gz/tgz, ar/deb) into
  the output, merging with existing directories. `.ar`/`.deb` archives follow
  ArFunction (docs/AR_ARCHIVES.md): members become files, `strip_prefix` and
  `strip_components` are ignored, `rename_files` applies, modes are `mode |
  0400` and atime/mtime the member's. download_and_extract names its download
  like DownloadManager (the first URL's decoded basename, `.TYPE` appended,
  unsafe characters as `_`) and chooses the decompressor by that name.
  `strip_prefix` is PathFragment-normalized
  (`pre/` strips `pre`, StripPrefixedPath.java:53); a prefix absent from the
  archive fails with `java.io.IOException: Error extracting ARCHIVE to PARENT:
  Prefix "P" was given, but not found in the archive. Here are possible prefixes
  for this archive: ...` (DecompressorValue.java:134-164,260-270). Malformed
  checksums report Checksum.java's texts (`Invalid SHA-256 checksum '...'`,
  `Unsupported checksum algorithm: '...' (expected SHA-1, SHA-256, SHA-384, or
  SHA-512)`, `Invalid base64 '...'`, `Invalid SHA-256 SRI checksum '...'`) after
  the download attempt; a checksum mismatch reports `java.io.IOException: Error
  downloading [URL] to OUTPUT/temp<N>/FILE: Checksum was ACTUAL but wanted
  EXPECTED` (HashOutputStream.java:84-90; hex for `sha256`, SRI for
  `integrity`; `<N>` is random as in Bazel). Every mirror failure is reported
  like HttpDownloader (java:217-262): a `WARNING: Download from URL failed:
  class CLASS MESSAGE` line per failed mirror (also under `allow_fail`), and,
  without `allow_fail`, `java.io.IOException: Error downloading [URLS] to PATH:
  LAST_MESSAGE` with Bazel's messages: `GET returned CODE REASON` (the
  server's reason phrase; `java.io.FileNotFoundException` for 404/410),
  `Redirect loop detected`, `Checksum was ...`, `Connection refused`,
  `Unknown host: HOST`, `Connect timed out`. Bazel's empty `temp<N>` directory
  left after an `allow_fail` failure is not reproduced. Native
  formats are zip/jar/war/aar/nupkg/whl, tar, tar.gz/tgz, tar.xz/txz,
  tar.zst/tzst, tar.bz2/tbz and ar/deb; the single-file gz/xz/zst/bz2
  decompressors and 7z, and for zip and tar `strip_components > 0` and
  `rename_files`, fail explicitly. Downloads reuse the module-extension transport
  (verified TLS, redirect/retry policy, Bazel's 10 s connect / 20 s read
  timeouts, `--repository_disable_download` policy) and **stream** into a
  private temporary directory of the repository while hashing; extraction then
  streams through the decompressor (constant memory, no size caps; see
  docs/REPOSITORY_IO_STREAMING.md). They defer a malformed checksum until after
  the attempt like Bazel, and return `struct(success, integrity, sha256,
  size_bytes)`. Like Bazel's repository cache (DownloadManager.java:244-386,
  DownloadCache.java) every completed download is stored under
  `<cache>/downloads/<algorithm>/<hex>` and a checksummed download is served
  from there without any request (also with downloads disabled); a
  `canonical_id` restricts such hits to entries stored with the same id
  (N43). `extract()` streams local archives of any size.
  `download()` (StarlarkBaseExternalContext.java:799-895) streams straight into
  the output path like Bazel's HttpDownloader (java:90-109), so a checksum
  mismatch (detected at end of stream, HashInputStream.java:61-91) leaves the
  downloaded bytes there, also under `allow_fail`; an existing non-directory
  output is replaced (unlinked) rather than written through. With a checksum,
  an output that already matches is reused without downloading, also offline
  (DownloadManager.java:247-261). It honours `executable` (execute bits
  added after success), `allow_fail`, `sha256`/`integrity` and returns the same
  struct; `block = False` returns a `PendingDownload` whose `wait()` returns or
  raises the blocking result (docs/EXTENSION_CTX_SURFACE.md); `auth` (basic and
  pattern) and `headers` are sent like Bazel's (SBEC:350-441). Archive symlinks and tar hard links are
  extracted like Bazel (docs/ARCHIVE_SYMLINKS.md); the zip link-escape and
  missing hard-link target errors keep Bazel's text, naming the archive by its
  basename, wrapped in `java.io.IOException: Error extracting ARCHIVE to
  PARENT: ...`. A repository that still contains an extracted hard link when the
  callback returns fails at the store snapshot with `hard-linked direct
  repository files are unsupported` (Bazel keeps the hard link).
* `ctx.patch(patch_file, strip=0, *, directory="", watch_patch="auto")`
  (SRC:475-567): `patch_file` (positional-only) and `directory` take a string,
  Label or path; `directory` must be inside the repository directory (`Cannot
  write outside of the repository directory for path ...`); a directory patch
  file fails with `attempting to use a directory as patch file: PATH`; the patch
  file is watched like `ctx.read` (`watch_patch`); a missing file fails with
  `Error applying patch PATH: Cannot find patch file: PATH`
  (PatchUtil.java:451-453). The patch is applied by the PatchUtil port shared
  with registry, override and Git patches (`patching.rs`, docs/PATCHES.md:
  `applyFuzzy(..., 2)` hunks, Git renames and modes, PatchUtil's error texts)
  and failures read `Error applying patch PATH: REASON`; patches that crash
  genuine Bazel are explicit native errors. A negative `strip` strips nothing
  (PatchUtil.stripPath). Label patch files keep the strict Label read profile.
  This is what `@bazel_tools` `http_archive(patches=..., patch_args=["-pN"])`
  uses (`tools/build_defs/repo/utils.bzl:250-257`).
* `ctx.repo_metadata(*, reproducible=False, attrs_for_reproducibility={})`
  (SRC:611-656) returns the opaque `repo_metadata` value (repr `<unknown object
  com.google.devtools.build.lib.bazel.repository.starlark.RepoMetadata>`, no
  fields, record equality, unhashable). `reproducible=True` with a non-empty dict
  fails with `attrs_for_reproducibility can only be specified if reproducible is
  False`; non-string keys fail with Dict.cast's `got dict<K, V> for
  'attrs_for_reproducibility', want dict<string, unknown>`.
* Implementation return values follow RepositoryFetchFunction.java:681-695 under
  the default `--incompatible_require_repo_extension_metadata=false`: a
  `repo_metadata`, `None` or any other non-dict value is accepted, and a dict must
  have string keys (`got dict<K, V> for 'return value', want dict<
  unknown>`). In Bazel a reproducible result only allows storing the repository
  in the repo contents cache; the native store reuses one publication per
  repository specification (below) and has no separate repo contents cache, so
  reproducibility has no native effect. A dict or `attrs_for_reproducibility`
  that changes attributes prints Bazel's two lines (RepositoryResolvedEvent.java:33-136,
  RepositoryFetchFunction.java:697-703): `DEBUG: Repo 'NAME' indicated that a
  canonical reproducible form can be obtained by modifying arguments k = REPR,
  ... [and dropping [a, b]]` (or `by dropping arguments [a, b]`), then `DEBUG:
  Repo NAME defined by rule RULE in @@//pkg:file.bzl`. Attributes are compared in
  declaration order against their defaults; `name` and `_private` attributes are
  ignored. Bazel prints them once per fetch, native on every execution (a warm
  reuse never runs the callback, so nothing is printed).
* `ctx.watch(path)` and `watch=`/`watch_template=`/`watch_archive=` use Bazel's
  `yes`/`no`/`auto` rules (SBEC:1656-1755): watching the repository directory
  fails with `attempted to watch path under working directory` for `yes` and is
  skipped for `auto`.

Watched files, directory listings and Label `read()` calls outside the
repository directory are recorded in the execution inputs as their
RepoCacheFriendlyPath (RepoRecordedInput.java:325-405, formed by
toRepoCacheFriendlyPath, SBEC:1656-1704) — `@@//relative` in the main
workspace, `@@NAME//relative` in external repository NAME (the longest
materialized root containing the path: native repositories live in the
repository store under the cache directory, by default inside the workspace),
the absolute host path elsewhere — or the canonical Label of a Label read, with
a content digest (`file:<sha256>`, `directory`, `missing`, `dirents:<sha256>`).
Label templates add no such file input: every template read (any
`watch_template` mode) is already recorded with its source and full bytes, which
keys its content and preserves pre-N21 identities. Inputs identify the
publication only; they never allow skipping execution.

The recorded probes the warm path revalidates (version 2) spell every path,
Label reads included, exactly as the key of a Bazel marker file (`@@REPO//path`;
genuine capture: tests/fixtures/repo_cache_friendly_paths/reference_receipt.json)
and resolve it through the repository's *current* root. Before review i20 a path
into another repository was recorded as a host path into that repository's
content-addressed store tree, which is never rewritten: identities depended on
the cache location (go_deps cold-identity variance), and a reader kept being
reused after the repository it read was fetched anew (Bazel re-fetches it).
Version-1 probes into the store are therefore never trusted: such a repository
executes once more and records version 2 — also when its output keeps the
publication's identity, since an execution whose recorded probes differ from
the manifest's republishes it (otherwise the stale probes would re-execute the
rule on every run).

#### Upgrade note (review i20)

The first invocation of an i20 binary on a cache written by an older bazel-rs
re-runs, once:

* every repository whose recorded probes name a path into the repository
  store: a rule that read, watched (`ctx.watch` of a file in another
  repository, by Label or by the path `ctx.path(Label)` returned), listed or
  `watch_tree`d a file of *another* repository. Its inputs now name
  `@@NAME//path` instead of a store tree, so its publication identity changes:
  the specification pointer is rewritten and a new tree published. On ~/code
  this is the go_deps chain — every go_repository watches `bin/fetch_repo`
  and `bin/gazelle` of `bazel_gazelle_go_repository_tools` and `go.env` of
  `bazel_gazelle_go_repository_cache` (gazelle `internal/go_repository.bzl`)
  — plus go_repository_tools itself, the Go SDK download and
  `local_config_cc`/`local_config_cc_toolchains`. The review counted ~459
  specification pointers naming 213 such publications on ~/code's default
  cache, ~0.9 GB of new trees. Measured on a private cache for
  `build --nobuild //src/beyondwork.ai/{prober,pvcautoscaler,decommission}/...`:
  221 re-executions (217 go_repository), 221 pointers rewritten, 221 new
  trees (743 MB); 344 s against 317 s for a settled warm run. The superseded
  trees stay on disk until the cache's garbage collection removes them
  (unreferenced trees a day after their last use; docs/CACHE_GC.md).
* in the default `--lockfile_mode=update`, every module extension whose
  MODULE.bazel.lock or hidden-lockfile entry keys a file by a store path,
  `@@//bazel-rs-out/repositories/...`: what bazel-rs recorded before i20
  with the cache inside the workspace (the default layout). Such a key is
  outdated (`is a path into the native repository store (recorded by an
  older bazel-rs)`), so the extension re-evaluates once and records
  `@@NAME//path` keys, Bazel's own (EXTENSION_CTX_SURFACE.md). ~/code's
  hidden lockfile (then `<cache-dir>/hidden-lockfiles/<sha256 of the workspace
  path>/MODULE.bazel.lock`; now in the workspace's state directory,
  docs/WORKTREES.md) holds 11 such keys — go_deps 7, go_sdk 1, pip 3
  — so those three extensions re-evaluate once; its MODULE.bazel.lock holds
  none. With the cache outside the workspace the pre-i20 keys were already
  Bazel's `@@NAME//path`, and no bazel-rs has written an absolute key into a
  lockfile (a path in no repository fails the write, EXTENSION_CTX_SURFACE.md),
  so absolute store keys are checked only defensively.

"Once" holds only in a mode that writes lockfiles. `--lockfile_mode=error`
writes none, hidden or not (as in Bazel, BazelLockFileModule.java:53-54), so
it never migrates store keys:

* a non-reproducible extension's MODULE.bazel.lock entry with a store key
  fails the command, exit 1 (e.g. `MODULE.bazel.lock is no longer up-to-date
  because an input to the extension '@@//:defs.bzl%ext' changed:
  @@//bazel-rs-out/repositories/direct/trees/<id>/file.txt is a path into
  the native repository store (recorded by an older bazel-rs). Please run
  ...`);
* a reproducible extension's hidden-lockfile entry (~/code's go_deps, go_sdk
  and pip) re-evaluates on every run, until one update-mode run writes the
  new keys.

**After upgrading, run one command in update mode (the default) before any
`--lockfile_mode=error` run**, and commit MODULE.bazel.lock if that run
rewrote it: a CI that checks the lockfile in error mode fails until then.
Tests (tests/repo_cache_friendly_paths.rs):
`store_keys_in_the_lockfile_re_evaluate_the_extension_once` (error mode
first, then update mode) and
`store_keys_in_the_hidden_lockfile_re_evaluate_on_every_error_mode_run`; both
behaviours were also reproduced from lockfiles the pre-i20 binary wrote.

After the first update-mode run a warm invocation re-runs only `local = True`
repositories, which never reuse a publication (below; 45 on that ~/code target
set, ~0.07 s together).

Switching binaries on one cache costs a re-run every time. A pre-i20 binary
trusts only version-1 probes, so it re-executes every repository the i20 binary
re-recorded and flips its pointer back to the old tree (still present: nothing
new is published); the next i20 run distrusts the store probes the old binary
recorded and flips the pointer forward again. The go_deps chain therefore
re-runs on every switch, in both directions (221 re-executions each way on the
cache above; the i20 run after a round trip took 359 s against 317 s settled).
Keep pre-i20 binaries off a migrated cache (give them their own
`--cache-dir`). Extension entries do not flip: a pre-i20 binary resolves
`@@NAME//path` keys through the repositories' current roots, and only an
extension it re-evaluates for another reason records store keys again, which
the next i20 run re-evaluates once more.

Known divergence: Bazel tests the workspace before `<output_base>/external`
(toRepoCacheFriendlyPath, SBEC:1669 then :1676), so every path inside the
workspace is `@@//rel` — also one inside a `local_path_override` (or
`local_repository`) directory that lies in the workspace, unless it is
reached through `external/NAME`, as a Label path is. Native has no
`external/` tree to tell the two apart and takes the longest materialized
root containing the path (`RepositorySet::friendly_repository`), so a file `x`
in the directory of module `foo` keys as `@@foo+//x` however it was reached
(Bazel: `@@//third_party/foo/x` through the workspace path, `@@foo+//x`
through a Label). Both keys name the same file while the override stays put
and each implementation resolves the other's spelling (`@@//rel` through the
workspace, `@@foo+//rel` through the repository root), so a lockfile shared
with Bazel stays up to date across them; only a freshly recorded key's
spelling differs.

### Host access

* `ctx.which(program)` searches the absolute entries of the client `PATH` for a
  regular file with the owner execute bit, trims the name like Java, and returns a
  `path` or None (SBEC:2323-2383).
* `ctx.execute(arguments, timeout=600, environment={}, quiet=True,
  working_directory="")` (SBEC:1970-2108) runs the command the way Bazel's
  process-wrapper does (RepositoryFetchFunction.java:99-124,644-645;
  src/main/tools/process-wrapper-legacy.cc:63-82,125-169), without a shell:
  `arguments` elements are strings, Labels or paths; `argv[0]` is resolved by
  `execvp` semantics against the CHILD environment's `PATH` (glibc default
  `/bin:/usr/bin` when unset; a name with a slash is used relative to the working
  directory; `EACCES` candidates do not stop the search; `ENOEXEC` runs through
  `/bin/sh`); the environment is the client environment plus `environment`
  overrides, with `None` removing a variable; stdin is empty. The result is an
  `exec_result` (hashable by identity, repr `<unknown object
  ...StarlarkExecutionResult>`) with `return_code`, `stdout` and `stderr`
  (Latin-1). Results follow StarlarkExecutionResult.java:192-252 and
  TerminationStatus.java:134-160 over the wrapper's raw status (exit code, or
  128+signal since the wrapper re-raises the child's signal): raw 129..191 are not
  `exited()`, so they report `(raw, "", "")` with empty streams, and raw 143 (an
  exit with 143 or SIGTERM) is an interrupt error; other exits keep their streams;
  a timeout returns 256 with stderr `Timed out`. A program that cannot be started
  returns 1 with stderr `src/main/tools/process-wrapper-legacy.cc:80:
  "execvp(ARGV0, ...)": No such file or directory` (or `Permission denied`)
  followed by a newline; `execute([])` returns 1 with process-wrapper's `No
  command specified.` usage text (native prints `process-wrapper` where Bazel
  prints its installed wrapper path). `timeout=0` crashes genuine Bazel
  (captured); the native profile fails explicitly. `quiet=False` echoes the
  captured streams to stderr after completion. A relative `working_directory` is
  created inside the repository directory; an absolute one outside it must
  already exist (Bazel creates it). The interrupt fails the native build with
  exit code 1 (Bazel: exit 8, `build interrupted`); the native CLI has no exit
  code model for interrupts. The timeout path under Bazel's process-wrapper was
  probed (EXACT `[256,"","Timed out"]`).
* `ctx.report_progress(status="")` is accepted; the native CLI has no progress UI.

Security posture: repository rules are trusted code, as in Bazel. `execute` and
`which` run host programs with the user's credentials, environment, network and
filesystem access; this is not a sandbox. Supervision only: the command runs
under the native process-wrapper (docs/PROCESS_WRAPPER.md), a child subreaper,
in its own process group; the command and its group are SIGKILLed on timeout,
and after the command has exited every process it left — also those that left
its group (`setsid` daemons, double forks) — is killed and reaped, like Bazel's
process-wrapper (`TerminateAndWaitForAll`), so no descendant keeps the captured
pipes open; output is captured whole like Bazel's
(StarlarkExecutionResult.java:201-203); there are no call-count, timeout or
argument-size caps (N32).

Repositories that call `execute`, `which` or downloads record those facilities
(never their outputs, which Bazel does not record either) in the execution inputs.

### Publication identity and warm reuse (N61)

A repository's specification is computed BEFORE any execution and is a pure
function of the rule definition identity, the complete serialized provenance
(attributes, mappings, sources), the host name/architecture and the values of
the declared `environ` names (RepositoryFunction's key: rule class, attributes,
environment, host factors). Its digest addresses a pointer under
`repositories/direct/specs/` in the cache, which names the published
content-addressed tree.

A warm invocation revalidates the previous publication for that specification —
the manifest, the recorded watchable inputs (template bytes, watched
file/directory states, observed environment names, exactly Bazel's
RepoRecordedInput set) and a full re-hash of the published tree against the
manifest inventory — and reuses it without running the repository rule, like
Bazel reusing `external/<name>` until a dependency changes. The full-tree
re-hash is deliberately STRONGER than Bazel's marker trust (Bazel believes the
marker file unless the recorded inputs changed; the native store also verifies
every content digest on reuse — a cheaper metadata-only check can replace it
later). Changed attributes, declared environment values, host markers,
template contents, watched files or observed environment values refetch: the
rule executes again and publishes (or re-deduplicates) a tree under the
unchanged content-addressing formula, so deterministic repositories keep their
identities across re-executions.

Environment persistence is a NATIVE policy, not Bazel's: the native manifest
records names and digests only — values never persist — while Bazel's own
`.marker` file stores `ENV:` values in plain text. `os.environ` and
unrequested environment changes record nothing (a changed undeclared variable
reuses the publication, like Bazel serving a cached repository whose rule
never recorded that input).

`local=True` repositories never reuse a publication across invocations: Bazel's
`RepositoryFetchFunction.shouldUseCachedRepoContents` keeps local repositories
refetching while fetching is enabled, and every native invocation is a fresh
server — their unrecorded dependencies (`which`/`execute`/executed-file reads)
make stale reuse dangerous. A rule whose output is nondeterministic under
unchanged recorded inputs is pinned to its first publication, exactly like a
Bazel fetch into a warm output base. A publication written by an older binary
(a manifest without the specification and recorded fields) is upgraded by
exactly one re-execution; after that warm invocations reuse it.

### Writes

Writes (file, template, symlink, rename, extract outputs) must stay inside the
repository directory: anything else fails with Bazel's `Cannot write outside of
the repository directory for path ...` (SBEC:331-339). The native profile also
refuses to traverse a symlinked parent while writing and requires output names
the content-addressed store can represent (UTF-8; 4096 bytes total, 255 bytes per component, no NUL, empty, `.` or `..` components (no depth cap)). Arbitrary template bytes,
including invalid UTF-8 and CRLF, are preserved. Label template and Label read
inputs resolve like Bazel's getPathFromLabel (the package must exist) and are
read following symlinks; a symlinked package directory remains a native
package-lookup restriction.

### Not implemented

Every StarlarkRepositoryContext member is implemented. `ctx.watch_tree(path)`
records the directory and a digest of its tree (SRC:568-609).
`module_ctx` shares the whole StarlarkBaseExternalContext implementation
(docs/EXTENSION_CTX_SURFACE.md). `ctx.attr` has type
`RepoDefinition` and `repr(ctx)` is Bazel's `<unknown object
...StarlarkRepositoryContext>`.

Other documented divergences: Bazel serves a cached repository across
invocations unless a recorded input changed; since N61 native does the same
(unrecorded `os.environ` mutations are invisible to both — the whole-dict
enumeration is not an individually recorded input); Starlark interpreter wording differs (`in call
to F(), parameter 'P' got value of type ...`, `has no field or method`, ...);
`rename_files`, `strip_components`,
single-file gz/xz/zst/bz2 and ar/7z formats and `delete()`
outside the repository directory are native-explicit errors; the write
restrictions above (symlinked parents, store-representable names, strict Label
inputs) remain; the former 16 MiB input cap was removed in N32.

### Release notes (N21)

* Repository rules and module extensions can now read the WHOLE client
  environment through `ctx.os.environ` (like Bazel). Any dependency's repository
  rule or extension can therefore read secrets such as tokens from the
  environment; Bazel behaves the same, but this is new for bazel-rs.
* `repository_ctx.execute` runs host programs. If bazel-rs itself is killed
  (for example Ctrl-C), the process-wrapper kills the running command and
  everything it started; normal completion and timeouts kill every process the
  command left (docs/PROCESS_WRAPPER.md).
* Label `read()` calls (unless `watch = "no"`) and host-path reads/watches are
  recorded as file inputs. Label templates are not: their existing `templates`
  entry (canonical label and full bytes) already keys their content, so
  repositories that only use `ctx.file` and Label templates keep their
  pre-N21 publication identity (no rekey). N51 later re-keyed every
  default-profile repository on purpose, because the mapping now includes
  `@bazel_tools` (docs/BUILTIN_MODULES.md).
* The process-wrapper timeout path was probed (EXACT); `timeout=0` crashes
  genuine Bazel and is an explicit native error.

### Release notes (N26)

* `repository_ctx.patch` and `repository_ctx.repo_metadata` are implemented, so
  `@bazel_tools` `http_archive` (which returns `ctx.repo_metadata(...)` at
  `tools/build_defs/repo/http.bzl:191`) and native-patch `patches` work.
  Implementation return values follow Bazel instead of failing.
* Octal escapes `\200`..`\377` and user-defined `chr`/`ord` run in repository
  and extension code. A free `chr`/`ord` reference is now Bazel's `name 'chr' is
  not defined` in every `.bzl`, BUILD and MODULE file; previously ordinary BUILD
  loading accepted the interpreter's builtins.
* `.bzl` files are loaded through symlinks that resolve inside their repository
  or into a repository materialized in the same invocation.
* Patches follow Bazel's PatchUtil exactly (fuzz, Git renames/modes, error
  texts) for every patch consumer (docs/PATCHES.md).
* Invalid string escapes are lexer errors in every BUILD, `.bzl`, MODULE and
  REPO.bazel file at the escape's column; MODULE.bazel errors are no longer
  deferred. Raw strings keep backslashes; `split()`/`rsplit()` require `sep` and
  `elems()` returns a list, as in Bazel 9.3 (docs/STRING_METHODS.md).
* Repository rules print Bazel's `DEBUG:` reproducibility lines.

## String and resource bounds

Bazel 9.3 strings are byte-oriented: the lexer reads a file's UTF-8 bytes as
Latin-1 `char`s (`--internal_starlark_utf_8_byte_strings`), and repository and
extension evaluation here models each such `char` as one Rust character
U+0000..U+00FF that `ctx.file` writes back as one byte. Since N38 the `.bzl`
sources of that profile are decoded as Latin-1 before parsing, like Bazel's
`ParserInput.fromLatin1` (`skyframe/StarlarkUtil.java:43-67`), so a raw
non-ASCII literal is one char per UTF-8 byte (`len('é') == 2`) and is accepted;
only a non-ASCII `load()` label fails explicitly (docs/LABEL_SYNTAX.md). Raw
non-ASCII MODULE literals are captured before evaluation and enforced only when
its direct repository usage is demanded; metadata-only operations keep their
existing behavior. Even an unrelated Unicode MODULE constant can therefore block
a demanded direct repository. Lexer errors (invalid escapes) are not deferred:
like Bazel (exit 48) they fail every command.

Original literal spellings are checked against Bazel's lexer
(`net/starlark/java/syntax/Lexer.java:372-403`). Octal escapes `\0`..`\377`
denote the char `NNN & 0xff`, the same scalar the native lexer decodes, so since
N26 they are accepted, including `\200`..`\377` (bazel_lib's
`lib/private/strings.bzl:262-518` byte tables; aspect_rules_aws). `\400` and above
fail with `octal escape sequence out of range (maximum is \377)`; `\x`, `\u`,
`\U`, `\N` and every other unknown escape fail with Bazel's `invalid escape
sequence: \x. Use '\\' to insert '\'.`, even when the native lexer accepts
them. These lexer errors apply to every BUILD, `.bzl`, MODULE and REPO.bazel file
and point at the escape (`V = "\x41"` reports column 7, `"\400"` column 9).
Escaped backslashes, raw ASCII strings and Unicode comments remain allowed.
`chr` and `ord` are not Bazel builtins (MethodLibrary.java has neither): a free
reference anywhere in a `.bzl`, BUILD or MODULE file fails at compile time with
Bazel's resolver error `name 'chr' is not defined` (Module.java:250-256,
Resolver.java:941-953, with SpellChecker suggestions over visible names), whether
or not it runs. A loaded or locally defined `chr`/`ord` (bazel_lib's
`strings.bzl` and the `base64.bzl` that loads it, rules_oci/aspect_bazel_lib
toolchains) is an ordinary function and executes normally; `decode()` of a
non-ASCII payload yields one char per byte and `ctx.file` writes those bytes.
Non-ASCII runtime strings therefore arise from octal escapes, user `chr` tables and
runtime data, such as module-extension reads/downloads (each byte maps to
U+0000..U+00FF exactly as Bazel's internal Latin-1 strings do) and `json.decode`
`\u` escape runs (decoded to UTF-8 bytes exactly as Bazel does). String methods
use Bazel's char semantics in this model (`"\240".isspace()` is True,
`"a\351b".title() == "A\351b"`; docs/STRING_METHODS.md).
Repository snapshots, string-valued repository attributes (including dictionary
keys) and `ctx.file` contents therefore accept U+0000..U+00FF; `ctx.file` writes
one byte per character. Characters above U+00FF are rejected explicitly (Bazel
cannot write them either). Repository names, labels and tag attributes remain ASCII;
template substitutions, paths and `execute` arguments/environment accept Latin-1. In repository and extension evaluation
`json.decode` maps `\u` escape runs to UTF-8 bytes like Bazel. String methods follow
Bazel's byte-string rules (`docs/STRING_METHODS.md`); non-UTF-8 byte data can
still differ for `title`, `capitalize` and `isspace`. Template file bytes never become Starlark
strings and are not subject to that restriction. This does not establish complete
Starlark language parity.

Callbacks and each demanded definition-file evaluation are bounded only to 128
call frames (Starlark forbids recursion); like Bazel there is no tick or heap
limit (the former 1 M ticks / 128 MiB heap were removed in N43). Ordinary BUILD
loading is unchanged. Repository I/O has no size or operation-count caps (N32):
files, templates, downloads and archives of any size are written and hashed in
streaming fashion. Publication enforces only the store path profile (UTF-8 paths: 4096 bytes total, 255 bytes per component, no NUL, empty, `.` or `..` components (no depth cap))
and hashes each published file incrementally. Note: deeply nested trees (worst case ~2048 levels) can hit EMFILE/thread-stack limits in the recursive walkers; all failures are explicit. These are
implementation limits, not a security sandbox.

Direct preparation also preflights one aggregate host budget across a usage's
declarations before binding or executing any callback. Repeated schema/default
expansion, names and raw nested arguments count toward the existing 64 MiB
weighted-byte and 100,000-node limits; name-only declarations cannot amplify a
large dictionary default without accounting for each copy.

## Publication and freshness

Each new invocation reevaluates demanded callbacks. Persistent cache hits never
skip evaluation. Exact loaded source bytes, mappings, typed attributes, template
reads, recorded watches/facilities and output snapshots identify the result.
Snapshots include directories, file contents, modes and symlink targets. After successful execution, a missing repository
boundary is supplied as an empty `REPO.bazel`, not a fabricated MODULE or BUILD.

Callbacks write only to private staging trees. Failures do not publish their
partial trees. Publication locks are acquired after evaluation and dependency
resolution; no interpreter runs under those locks. Verified matching trees may
be reused, while corrupted trees are replaced. Tree and manifest publication use
separate atomic renames; missing metadata after interruption forces reevaluation
and repair. The cache is user-owned and published trees must remain immutable
within a library invocation.

A replaced manifest is first renamed into a private `direct/trees/.trash-*`
quarantine, which is deleted once the new publication is in place. A replaced
tree is swapped with the finished repository directory (the stage of the
publication) in one step (`renameat2` with
`RENAME_EXCHANGE`) and then moved from the stage path into that quarantine, so a
reader in another process — an action running through a link into the tree
(docs/HARDLINK_STAGING.md, "Repository files: links into their tree"), a command
reading the repository — never finds the tree's path empty; quarantining the
tree first and renaming the stage after it left a window without any tree
between the two renames, which remains only as the fallback for a filesystem
that cannot exchange. Test:
`republication_never_leaves_the_tree_path_without_a_tree`.
Trees may hold read-only directories (a callback's `chmod -R ugo-w`,
as rules_python's `python_repository` does for the interpreter's `lib/`), so
quarantines and repository directories are deleted like a writer clears an
interrupted one: directories are made writable first and symlinks are never
followed. A
deletion failure leaves garbage and a `bazel-rs: warning: cannot remove ...`
line, never a failed command (before, `TempDir::close`
failed with `cannot remove direct quarantine: Permission denied` after the new
tree was already published, and left the quarantine behind). Publishers hold
`direct/locks/quarantine` shared while their quarantine exists; the first
publisher of a process that can take it exclusively first deletes every
leftover `.trash-*`, once per process and store (leftovers only come from
earlier processes; sweeping on every publication listed `direct/trees` once
per published tree, quadratic over a cold fetch, and briefly serialized
concurrent publishers each time). Tests:
`republishing_over_a_read_only_tree_removes_the_quarantine`,
`stale_quarantines_are_swept_once_when_no_quarantine_is_live`.

How a build with the Python toolchain reached that path: actions staging the
interpreter's runtime hardlinked its 0444 `lib/` files into the action store's
`cas/` (their mode is a blob's), so the published tree's files had a second
link; the next command's verification rejects hard-linked tree files, so the
rule re-executed (re-extracting the interpreter), kept its identity and
republished over the read-only tree. Repository files are now copied into
`cas/`, never linked (docs/HARDLINK_STAGING.md); the tree stays reusable.

The Git and registry stores stage and replace corrupt trees the same way
(`Scratch` stages, `remove_tree` for the replaced tree). Their trees cannot
normally hold read-only directories (the archive extractor normalizes modes
to 0755/0644, Git exports carry no directory modes, patches change none), but
a tampered tree with one no longer makes its repair fail (tests
`repairs_read_only_directory_tampering` in git_source and materializer).

## Application boundary

This executor also serves bounded ordinary module extensions. It does not
execute `go_sdk`, generate nogo by substitution, or bypass transitive features.
Successful application query/build/test and full Bazel 9.3 parity remain unproven.

## Verification

The implementation passes 608 Rust tests, 270 Python tests, formatting, strict
all-target Clippy, release compilation and error-level Rust Analyzer diagnostics.
`artifacts/repository-rules-validation.json` records seven genuine Bazel 9.3
differentials and nine native-only boundary/lifecycle checks. N21 replaced the
`native.unsupported` `execute` boundary with an eighth differential,
`execute.host-process`; N26 added four more (`literal.latin1-octal`,
`return.metadata-and-values`, `patch.label-strip-directory` and the rejection
differential `resolver.free-chr`) and removed the `octal`/`chr` ascii-profile
and `metadata`/`integer` return probes from the native boundaries. N52 retired
the `native.unsupported` nonzero `facts_version` boundary, which is supported and
compared against genuine Bazel by
`tests/fixtures/lockfile_extensions/reference_receipt.json` (20 cases: twelve
differential, eight boundaries). That report predates all three and needs
recapture. Seven adjacent
Git-link, Git, local-repository, declaration, patch, remote and module reports
pass against the same release binary. Reports retain hashes, actual output
bytes/modes, reference shutdown and scratch-removal evidence.
Final review approved this bounded profile; the verification manifest binds the
release, source files and reports without claiming unrestricted compatibility.

The final read-only application probe still fails at `@io_bazel_rules_nogo`,
preserves the required Git symlink, and leaves checkout fingerprints unchanged:
`artifacts/mainrepo-direct-repository-loading.json`. These results are not
application query/build/test success or a whole-application performance claim.
