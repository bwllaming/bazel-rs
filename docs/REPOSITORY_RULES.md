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
the private stage, so absolute repository paths observed by the callback are not
the published location (relative symlinks into it are replanted, below).

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
  repository contents cache). The native stage is renamed to a content-addressed
  directory whose name depends on these very links, so after the callback the
  native profile rewrites links into the repository directory (from
  `ctx.symlink` or executed tools) as relative links with the same resolved
  contents. Other targets stay absolute. The store records link targets without
  following them. (Archive symlink replanting in extraction is separate N19 work.)
* `path.realpath` resolves component by component; a dangling link fails naming
  the missing target: `[unix_jni.cc:382] TARGET (No such file or directory)`.
* `ctx.extract(archive, output="", strip_prefix="", ...)` and
  `ctx.download_and_extract(url, output="", sha256="", type="", strip_prefix="",
  allow_fail=False, canonical_id="", auth={}, headers={}, integrity="", ...)`
  (SBEC:907-1400) validate the legacy `stripPrefix` alias, `strip_components`,
  archive types (`Expected a file with a ... suffix`, `No decompressor found for
  type ...`) and URLs/checksums with Bazel's messages, then extract with the
  native extractor (zip/jar/war/aar/nupkg/whl, tar, tar.gz/tgz) into the output,
  merging with existing directories. `strip_prefix` is PathFragment-normalized
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
  `integrity`; `<N>` is random as in Bazel). HTTP failures keep the native
  transport text (Bazel prints the server's reason phrase, e.g. `GET returned 404
  File not found`, which the transport does not expose), and Bazel's empty
  `temp<N>` directory left after an `allow_fail` failure is not reproduced. Other Bazel formats, `strip_components > 0`,
  `rename_files`, `auth` and `headers` fail explicitly. Downloads reuse the
  module-extension transport (verified TLS, redirect/retry policy, 8 MiB per
  archive, 32 MiB per callback, `--repository_disable_download` policy), defer a
  malformed checksum until after the attempt like Bazel, and return
  `struct(success, integrity, sha256, size_bytes)`; `canonical_id` only keys
  Bazel's repository cache, which the native profile does not have. Local
  archives are limited to 16 MiB.
* `ctx.watch(path)` and `watch=`/`watch_template=`/`watch_archive=` use Bazel's
  `yes`/`no`/`auto` rules (SBEC:1656-1755): watching the repository directory
  fails with `attempted to watch path under working directory` for `yes` and is
  skipped for `auto`.

Watched files, directory listings and Label reads outside the repository
directory are recorded in the execution inputs as `@@//relative` (main
workspace), the canonical Label, or the absolute host path, with a content digest
(`file:<sha256>`, `directory`, `missing`, `dirents:<sha256>`). They identify the
publication only; they never allow skipping execution.

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
filesystem access; this is not a sandbox. Supervision only: the child leads its
own process group, which is SIGKILLed through `/usr/bin/kill` after the leader
exits or on timeout/overflow (Bazel would keep orphans that hold its pipes and
then block on them); output is captured up to 16 MiB per stream and overflow is
an explicit error instead of truncation; at most 1,000 calls, 24-hour timeouts and
2 MiB of arguments per callback.

Repositories that call `execute`, `which` or downloads record those facilities
(never their outputs, which Bazel does not record either) in the execution inputs.
Every native invocation re-executes every demanded repository before publication
reuse, which is at least as conservative as Bazel's refetch of `local=True`
rules: no cached publication is ever served for a callback that did not run.

### Writes

Writes (file, template, symlink, rename, extract outputs) must stay inside the
repository directory: anything else fails with Bazel's `Cannot write outside of
the repository directory for path ...` (SBEC:331-339). The native profile also
refuses to traverse a symlinked parent while writing and requires output names
the content-addressed store can represent (UTF-8 without control characters,
`\`, `:`, `?`, `#` or `%`; depth 64; 4096 bytes). Arbitrary template bytes,
including invalid UTF-8 and CRLF, are preserved. Label template and Label read
inputs keep the strict source profile: a regular BUILD file and no symlinks.

### Not implemented

`ctx.download`, `ctx.patch`, `ctx.watch_tree` and `ctx.repo_metadata` exist (so
`hasattr` matches Bazel) but fail with `repository_ctx.<method>() is not
implemented by the native profile`. Only an implicit or explicit `None` callback
return is supported; other returns fail explicitly even where upstream accepts
them as repository metadata. `module_ctx` keeps its narrower profile (no
`execute`/`which`/`file`), but shares `os.environ`, `getenv` and
`module_extension(environ=...)` with repository rules. `ctx.attr` has type
`RepoDefinition` and `repr(ctx)` is Bazel's `<unknown object
...StarlarkRepositoryContext>`.

Other documented divergences: Bazel serves a cached repository across
invocations unless a recorded input changed, while native re-runs every demanded
repository in every invocation (unrecorded `os.environ` changes are therefore
seen natively but not by Bazel); Starlark interpreter wording differs (`in call
to F(), parameter 'P' got value of type ...`, `has no field or method`, ...);
download, patch, watch_tree, repo_metadata, `rename_files`, `strip_components`,
xz/bz2/gz/zst/ar/7z formats, `auth`/`headers` and `delete()` outside the
repository directory are native-explicit errors; the four write restrictions
above (symlinked parents, store-representable names, strict Label inputs,
16 MiB inputs) remain.

### Release notes (N21)

* Repository rules and module extensions can now read the WHOLE client
  environment through `ctx.os.environ` (like Bazel). Any dependency's repository
  rule or extension can therefore read secrets such as tokens from the
  environment; Bazel behaves the same, but this is new for bazel-rs.
* `repository_ctx.execute` runs host programs. If bazel-rs itself is killed
  (for example Ctrl-C), a running child process may be orphaned; normal
  completion, timeouts and output overflow kill the child's process group.
* Label templates and Label reads are now recorded as file inputs, so
  repositories using them rekey once.
* The process-wrapper timeout path was probed (EXACT); `timeout=0` crashes
  genuine Bazel and is an explicit native error.

## String and resource bounds

Bazel strings are byte-oriented; Rust Starlark uses Unicode strings. To avoid
silently computing different contents, direct execution currently requires ASCII
string literals and attributes. Every definition's transitive source is checked
before evaluation, including unused expressions. MODULE violations are captured
before evaluation and enforced only when its direct repository usage is demanded;
metadata-only operations keep their existing behavior. Even an unrelated Unicode
MODULE constant can therefore block a demanded direct repository.

`chr` references, including aliases and shadowed uses, are rejected conservatively.
Original literal spellings are checked: high-byte octal escapes are unsupported,
and hex/Unicode escapes are invalid in the pinned Bazel grammar. ASCII octal,
escaped backslashes, raw ASCII strings and Unicode comments remain allowed.
Because executed sources are ASCII-literal-only and `chr`-free, non-ASCII runtime
strings can arise only from runtime data, such as module-extension reads/downloads
(each byte maps to U+0000..U+00FF exactly as Bazel's internal Latin-1 strings do)
and `json.decode` `\u` escape runs (decoded to UTF-8 bytes exactly as Bazel does).
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

Callbacks and each demanded definition-file evaluation are bounded to 128 call
frames, one million evaluator ticks and a 128 MiB evaluator heap. Ordinary BUILD
loading is unchanged. Files and template inputs are at most 16 MiB each;
cumulative I/O and operation counts are bounded. Publication allows at most
10,000 entries, depth 64, 4096-byte paths and 64 MiB file contents. These are
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
`execute.host-process`; that report predates it and needs recapture. Seven adjacent
Git-link, Git, local-repository, declaration, patch, remote and module reports
pass against the same release binary. Reports retain hashes, actual output
bytes/modes, reference shutdown and scratch-removal evidence.
Final review approved this bounded profile; the verification manifest binds the
release, source files and reports without claiming unrestricted compatibility.

The final read-only application probe still fails at `@io_bazel_rules_nogo`,
preserves the required Git symlink, and leaves checkout fingerprints unchanged:
`artifacts/mainrepo-direct-repository-loading.json`. These results are not
application query/build/test success or a whole-application performance claim.
