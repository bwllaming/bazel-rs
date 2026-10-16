# Ignored directories (Bazel 9.3, N14)

Bazel ignores repository subdirectories listed in `.bazelignore` (path
prefixes) and in REPO.bazel `ignore_directories([...])` (glob patterns). The
native profile implements the same per-repository value and every loading-time
consumer. Citations are to the pinned Bazel 9.3 source
(`e89755babf3e941bc1e9d8120b1cd48854ab9dfa`), relative to
`src/main/java/com/google/devtools/build/lib/`. Implementation:
`src/ignored.rs`, consumers in `src/package.rs`, `src/glob.rs`,
`src/workspace.rs`, `src/engine.rs`, `src/loading.rs`, `src/extension_ctx.rs`.

## The per-repository value

`skyframe/IgnoredSubdirectoriesFunction.java:185-200` computes, for one
repository, `IgnoredSubdirectories.of(prefixes, patterns)`:

1. **Patterns** first (`computeIgnoredPatterns`, `:105-125`): the
   `ignore_directories()` strings of the repository's REPO.bazel
   (`RepoFileValue`). REPO.bazel errors therefore surface before `.bazelignore`
   errors. `RepoFileValue` keeps `repo()` kwargs raw; only
   `RepoPackageArgsFunction` converts them, so computing ignored directories
   never converts labels (`repo_ignored_directories`, `process_args = false`).
2. **Prefixes** (`computeIgnoredPrefixes`, `:128-181`): `<repository
   root>/.bazelignore` when `FileValue.isFile()` (following symlinks; a
   directory, dangling symlink or missing file contributes nothing). This
   applies to the main repository *and* to external repositories (`:171-178`
   reads the fetched repository root). `--vendor_dir` (which adds the vendor
   directory as a prefix, `:138-151`) is not implemented by the native profile.

### `.bazelignore` lines

`getIgnoredPrefixes` (`:75-102`) reads the file as UTF-8 (malformed bytes
become U+FFFD) with `CharStreams.readLines` (`\n`, `\r\n`, `\r`) and
`PathFragmentLineProcessor` (`:202-227`):

* Only empty lines and lines **starting** with `#` are skipped. Nothing is
  trimmed: ` a ` is the prefix ` a `, `  # x` is a prefix, not a comment.
* Each line is `PathFragment.create(line)`: repeated and trailing slashes
  collapse (`node_modules/` == `node_modules`), `.` segments vanish, `..`
  pops a preceding non-`..` segment (`a/../b` == `b`; `..` stays). A line `.`
  is the empty prefix, which ignores every directory including the root
  package.
* Duplicates collapse in first-seen order (`ImmutableSet`); `matchingEntry`
  reports the first matching prefix.
* A NUL character fails (`Path.of(line)`): `Invalid path in <file>: Nul
  character not allowed: <line>`.
* After all lines are read, the first absolute entry fails:
  `Invalid path in <file>: '<normalized>': cannot be an absolute path`
  (`InvalidIgnorePathException`, `IgnoredSubdirectoriesValue.java`).
* A read failure is `<file> is not readable because: error '<message>'. Was it
  modified mid-build?`.
* Encoding quirk: the decoded Unicode line is compared with Bazel's Latin-1
  "internal" directory names. Entries with characters above U+00FF never
  match; others match the directory whose bytes are the entry's code points
  (only ASCII lines behave intuitively). The native profile reproduces this,
  as Bazel behaves under a UTF-8 server locale (the capture ran with
  `LC_ALL=C.UTF-8`). Under an ISO-8859-1 JVM path encoding (non-UTF-8
  locale), Java's `Path.of(line)` instead rejects such lines with an
  `InvalidPathException` (`Invalid path in <file>: ...`); that locale-dependent
  failure is not reproduced.
* Native policy (stricter than Bazel): `.bazelignore` must resolve inside the
  repository, like REPO.bazel and BUILD files.
* Diagnostic path of an external repository's `.bazelignore`: Bazel names the
  fetched copy, `<output_base>/external/<canonical name>/.bazelignore`; the
  native profile has no output base / `external/` layout and names the file
  in the repository's source directory (for a `local_path_override`, the
  overridden directory). The message text is otherwise identical.

### `ignore_directories(dirs)`

`packages/RepoFileGlobals.java:35-61`: `dirs` is a positional-only parameter
(`@Param` defaults `named = false`), bound like every Java builtin
(`net/starlark/java/eval/BuiltinFunction.java`): `ignore_directories() accepts
no more than 1 positional argument but got 2`, `ignore_directories() got named
argument for positional-only parameter 'dirs'`, `ignore_directories() got
unexpected keyword argument '<name>'`, `ignore_directories() missing 1 required
positional argument: dirs`. Its type is `sequence` (`in call to ignore_directories(), parameter 'dirs' got value of type
'string', want 'sequence'`), cast with `Sequence.cast(dirs, String.class,
"dirs")` (`at index 0 of dirs, got element of type int, want string`; an
empty `range()` is an empty sequence), then a second call fails with Bazel's
spelling `'ignored_directories()' can only be called once`. `repo()` after
`ignore_directories()` fails with `if repo() is called, it must be called
before any other functions`. Patterns are **not** validated: any string is
accepted and simply never matches if it cannot.

## Matching

`cmdline/IgnoredSubdirectories.java`:

* `matchingEntry(directory)` (`:168-185`): the first prefix that is a
  segment-wise prefix (`PathFragment.startsWith`: `pkg` matches `pkg/x`, not
  `pkgs`), else the first pattern for which `UnixGlob.matchesPrefix` holds.
* `filterForDirectory(directory)` (`:126-140`): keeps prefixes under the
  directory and patterns with `UnixGlob.canMatchChild`.

`vfs/UnixGlob.java:942-980` (`matchesPattern`): patterns are split on `/`
keeping empty segments (which never match, so `node_modules/` in
`ignore_directories()` matches nothing, unlike in `.bazelignore`); `**` matches
zero or more segments. `PREFIX` succeeds once the pattern is exhausted;
`CAN_MATCH_CHILD` succeeds once the path is exhausted (and fails when the
pattern is exhausted first). The native table-driven evaluation implements
the same recurrence without exponential backtracking.

Segments use `UnixGlob.matches(String, String, Map)` (`:212-255`) byte-wise:
an empty pattern or name never matches; `*` and `**` match any name, even
hidden ones; otherwise a name starting with `.` needs a pattern starting with
`.`; `*xyz` and `xyz*` (a single star) compare the rest literally (so `?` and
parentheses are literal there); every other pattern goes through
`makePatternFromWildcard` (`:257-310`): `*`/`**` → `.*`, `?` → `.` (one byte),
`(`/`)` dropped, everything else literal, and Java's `.` never matches `\n`,
`\r` or the byte `0x85`.

Examples: `**/node_modules` matches `node_modules`, `a/b/node_modules` and
every directory below them; `a/*/b` matches `a/x/b` and `a/.x/b` (bare `*`),
but not `a/x/c`; `src/ts/node_modules` in `.bazelignore` matches exactly that
tree.

## Consumers

| Bazel | Behavior | Native |
| --- | --- | --- |
| `skyframe/PackageLookupFunction.java:111-120` (main), `:365-375` (external) | A package whose path has a matching entry is `DELETED_PACKAGE_VALUE`; `PackageFunction.java:461-467` reports `no such package '<pkg>': Package is considered deleted due to --deleted_packages` (`PackageLookupValue.java:363`; main packages print their path, external ones `@@repo//pkg`). | `package::evaluate_in`, before any BUILD lookup; also BUILD dependencies on such packages. |
| `cmdline/TargetPattern.java:587-614, 655-666` | `//dir/...` with `dir` inside an ignored directory: warning `Pattern '//dir/...' was filtered out by ignored directory '<entry>'`, no targets, success. The warning is reported as it happens, so it precedes an error from a later pattern. | `Engine::select*` collects warnings; the CLI prints `WARNING: ...` on stderr, also when a later pattern fails. |
| `skyframe/ProcessPackageDirectory.java:293-301`, `RecursivePkgValueRootPackageExtractor.java:43-48` | Recursive package discovery does not recurse into matching subdirectories (checked before nested-repository and package-name checks); children receive `filterForDirectory` entries. | `workspace::discover_packages`. Unsupported directory names below an ignored directory are never inspected. |
| `packages/producers/DirectoryDirentProducer.java:77-82`, `GlobFunctionWithMultipleRecursiveFunctions.java:63-68`, `packages/GlobCache.java:132-140` | `glob()` neither returns nor traverses directories (including resolved directory symlinks) whose repository-relative path matches; regular files are never checked. | `glob::glob` with the package's filtered entries; a link is resolved first (its cycle/expansion errors included, docs/GLOB_SYMLINKS.md) and skipped only if it resolves to a directory (never traversed); a link to a file is a regular result. |
| `skyframe/ContainingPackageLookupFunction.java:30-60` | Ignored directories are deleted packages, never subpackages: labels and outputs may pass through an ignored directory that has a BUILD file. | `Engine::target` / `validate_output_directory` and `.bzl` load paths skip BUILD boundaries inside ignored directories (MODULE.bazel/REPO.bazel/WORKSPACE files are no boundaries since N28d: Bazel 9.3 has no nested repositories, docs/GLOB_SYMLINKS.md). |
| `skyframe/BzlLoadFunction.java:693-726, 1625-1650`, `ContainingPackageLookupValue.java:63-104` | `load()` of a `.bzl` whose package is ignored walks up to the nearest existing package: `Label '<label>' is invalid because '<pkg>' is not a package; perhaps you meant to put the colon here: '//<containing>:<path>'?`, or without any package `Every .bzl file must have a corresponding package, but '<label>' does not have one. ...`. | `loading::ignored_package_load_error`. |
| `bazel/repository/RepositoryUtils.java:53-70` | `module_ctx.path(label)` in an ignored package: `Unable to load package for <label>: Package is considered deleted due to --deleted_packages`. | `extension_ctx` source path resolution. |
| `skyframe/RepoFileFunction.java:87-116` | `FileValue.exists()` is true for directories: a directory (or other non-regular file) named REPO.bazel is exactly `error reading REPO.bazel file at <path>` (the `IOException` message carries no cause), not an absent file. A dangling symlink does not exist. | `workspace::repo_file`, `package::repo_file_value`. |
| `buildtool/SymlinkForest.java:425-442` with `skyframe/SkyframeExecutor.java:3776-3793` (`withPrefix(workspace)`) | The execroot symlink forest does not plant a top-level main-repository entry that has a `matchingEntry` (`node_modules` under `**/node_modules`, `pkg` under a `pkg` prefix). A declared source input below it is absent when the action runs (`cat: node_modules/b.txt: No such file or directory`); source targets that need no action still build, and nested ignored directories (`src/ts/node_modules`) stay reachable through their planted top-level ancestor. | The native execroot holds only declared inputs (no forest); `engine::planted` omits exactly the main-repository source inputs below an unplanted top-level entry, so the command sees the same missing file. Omitted inputs are not part of the action key: the action cannot observe them. A declared input there that does not exist at all still fails as Bazel's `missing input file` (ArtifactFunction checks it regardless of planting; docs/CONFIGURATION.md). |

## Identity and determinism

Ignored directories only change *which* packages, glob entries and source
paths exist; they add no new inputs. Glob results remain sorted and unique
(`BTreeSet`), discovery is sorted, and the value is a pure function of the
repository's REPO.bazel and `.bazelignore` contents. Action cache keys and
provenance continue to hash the selected sources themselves, so an ignore
change that alters a glob changes keys exactly through the changed source
set. The value is computed once per repository and `RepositorySet` (one CLI
invocation), like a Skyframe value; there is no daemon or incremental
invalidation in the native profile.

## Not implemented / differences

* `--vendor_dir` prefixes and `--deleted_packages` itself.
* The execroot forest's other omissions (top-level `bazel-*` and `external`
  entries) and undeclared access to planted sources: the native execroot only
  ever contains declared inputs.
* The external-repository `.bazelignore` diagnostic path (see above).
* Bazel's message framing (`ERROR: ...`, `Skipping ...`) around the cited
  texts, and the `INFO: Empty results` line for empty queries.
* Non-UTF-8 directory names remain unsupported (they are reported before
  matching can be attempted, except below an already ignored directory).
