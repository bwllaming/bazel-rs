# `native.subpackages()` (Bazel 9.3)

`subpackages(include=..., exclude=[], allow_empty=False)` is available in
BUILD files and as `native.subpackages` in legacy macros. All three arguments
are named-only; include is required. It returns a **fresh mutable sorted list
of unique strings**, relative to the caller's package and repository. It is
not recursive package discovery, a file glob, or a rules_python special case.

## Reference and captures

Pinned source: Bazel 9.3.0, commit `e89755babf3e941bc1e9d8120b1cd48854ab9dfa`,
at `/home/coder/opencodetmp/bazel-9.3-reference`. Source paths below are relative
to `src/main/java/com/google/devtools/build/lib/`:

* `starlarkbuildapi/StarlarkNativeModuleApi.java:336-371`: signature and result.
* `packages/StarlarkNativeModule.java:810-881`: BUILD-only context, string-list
  conversion, SUBPACKAGES glob operation, sorting, deferred I/O errors.
* `packages/Package.java:691-710`: BUILD/legacy macro only, not symbolic macros.
* `packages/GlobCache.java:131-155, 260-291, 340-374`: traversal boundaries,
  include ordering/deduplication, ignored-directory behavior, per-pattern cache.
* `packages/producers/DirectoryDirentProducer.java:95-167`: stop at the first
  package; trailing `**` can match the boundary itself, never a package below it.
* `skyframe/GlobValue.java:45-56`: include validation/error texts.
* `vfs/UnixGlob.java:171-191, 212-310, 889-982`: pattern syntax, matching and
  the deliberately partially validated exclusion fast paths.
* `packages/GlobberUtils.java:24-53`: per-include and all-excluded empty errors.

Genuine ELF: `/home/coder/bazel/.direnv/reference/bazel-9.3.0-linux-x86_64`,
SHA256 `d302d22ed77ee5658b87857249baa579db84300ecebe0a1e9a357b5b20f80eb8`.
Raw capture commands, exit codes, output and durations are retained under
`/home/coder/opencodetmp/native-subpackages/genuine/`:

* `receipt.json`: results/types/mutability, 25 argument/pattern/link cases,
  `.bzl` top-level and symbolic-macro restrictions.
* `bzlmod-receipt.json`: main/external ignore patterns and prefixes, direct
  lookup of an ignored returned boundary, ignored nonpackage traversal.
* `build-receipt.json`: real macro -> filegroup -> child file dependencies,
  BUILD.bazel precedence, genuine successful custom-rule write action. The
  manifest is exactly `a\np/a/value.txt\n`; query returns `//p:manifest`,
  `//p:manifest_files`, `//p/a:data`, `//p/a:value.txt`.

The capture harnesses live beside that directory (`capture.py`,
`capture_bzlmod.py`, `capture_build.py`). All commands were batch, offline,
rc-free, with private HOME/TMP/output/repository caches. Bzlmod captures used
empty scratch overrides for unused builtins and an empty file registry; the
write-action capture used a scratch native platform. No real workspace or
user caches were used. Expected behavior is checked in `tests/subpackages.rs`
and `glob.rs` unit tests; genuine Bazel is not required to run those tests.

## Semantics

* Only **nearest** BUILD boundaries are returned. With `a/BUILD` and
  `a/deeper/BUILD`, `**` and `a/**` return `a`; `a/deeper` returns nothing.
  With no `z/BUILD`, `z/deep/BUILD` is returned as `z/deep`. The caller's own
  package is never a result. Child BUILD contents are not evaluated by the API.
* A regular BUILD.bazel takes precedence over BUILD; either marks a package.
  Symlinks to regular files count, dangling markers and directories do not.
  MODULE.bazel, REPO.bazel and WORKSPACE do not create package boundaries.
* Includes support `*` and whole-component `**`. Empty, absolute, empty-segment,
  `.`/`..`, embedded `**`, and `?` patterns fail with captured Bazel errors.
  Brackets, braces, backslashes, colons and parentheses are literal characters
  in literal components, not extra glob syntax. Wildcard components use
  UnixGlob's matcher (including its parentheses quirk); compound wildcard
  components do not implicitly match leading dots.
* All includes are validated before traversal. With allow_empty=False, each
  include must match before exclusions, in include order; the final union
  must also remain nonempty. Duplicate includes/results collapse. Exclusions
  are applied after these checks. Literal exclusions are exact removals and
  need not be syntactically valid paths. `head**/*tail` uses string prefix and
  suffix matching; other wildcard excludes are validated and matched by
  UnixGlob's segment matcher (`?` is allowed here).
* Include/exclude accept sequences (lists, tuples, even empty range); each
  element must be a string. allow_empty must be a bool, not an integer. Calls
  outside BUILD/legacy macro context fail, including `.bzl` initialization,
  symbolic macros, repository rules and analysis-time rule implementations.

### Ignored directories: the captured hybrid-glob quirk

The legacy `GlobUnixPathDiscriminator.shouldTraverseDirectory` checks ignored
directories, but `shouldIncludePathInResult(SUBPACKAGES)` only checks for a
BUILD file. Therefore **a matched ignored directory with its own BUILD file
is returned**. If the ignored directory has no BUILD file, packages below it
are not discovered. This holds for both `.bazelignore` prefixes and REPO.bazel
patterns, in main and external repositories. Querying a returned ignored
package still fails with `Package is considered deleted due to --deleted_packages`.
This is not the behavior of recursive package discovery or native.glob, and
neither existing consumer was changed to imitate it.

## Implementation, observation and safety

The API in `package.rs` uses a separate per-package/per-include cache keyed by
the link policy. Repeated includes, new excludes and allow_empty changes reuse
the same walk. Fresh lists are allocated on every call. The shared `glob.rs`
walker is pattern-directed and stops at nearest package boundaries; a narrow
include does not walk unrelated trees. No package BUILD contents, processes
or actions are needed to answer the API.

All listings, followed stats, canonicalization and symlink observations use
the existing `observed_fs` interfaces. subpackages records full listings of
**visited directories only**, rather than reusing glob's narrower-dialect
projection. This conservatively invalidates on unrelated entry changes in a
visited directory, but cannot miss BUILD additions/removals or wider-dialect
matches. No snapshot schema, binary format, action keys or content hashes
changed. Existing repository ignore-value observation/replay is retained.
Mutation tests exercise normal snapshot save/hit/strict verify, adding/removing
intermediate BUILD boundaries, `.bazelignore`/REPO.bazel changes, directory link
retargeting, formerly dangling targets, and BUILD-marker link target changes.

The shared link policy is unchanged: links inside the repository or admitted
materialized repositories are readable; host reads are admitted only for
repository-rule-generated repositories, never into blocked store/cache trees.
Dangling links disappear; cycles fail even when excluded or allow_empty=True.
Wildcard ancestor expansions are package errors, while a literal link to the
caller package can be returned. Existing caps/reserved-tree restrictions and
deterministic error ordering apply (see `GLOB_SYMLINKS.md`).

### Existing native restrictions retained

* Discovering a symlinked package is supported; **loading a target in a symlink
  package directory is still rejected by the package loader**. The new API
  does not bypass that check. Snapshot link tests consume names in a write
  rule, while real filegroup dependency tests use ordinary package directories.
* Main/local/archive repositories cannot use links to arbitrary host paths.
  A subpackages boundary probe through an ignored host link still applies that
  policy; unlike glob, the probe may need to inspect the boundary itself.
* BUILD-marker lookup I/O failures retain glob's native deferred boundary
  diagnostic; competing link errors are deterministic rather than Bazel's
  concurrent winner. Non-UTF-8 names retain the explicit native rejection.

native.glob's argument defaults, pattern grammar, exclusion matching, ignored
directory semantics, caching and result projection are unchanged.

## Application follow-up

Combined with MODULE aliases and the tab lexer correction, a bounded native-only
`query 'deps(//...)'` on the isolated code PR checkout passed the subpackages
loading failure. It then failed while extracting an ansible-core wheel at
`py_internal.regex_match(name, "(?U)\\w+")`, a separate unimplemented API.
That probe took 874.69 s at 4,031.5 MiB peak child RSS, below the unchanged
900-second/8-GiB bounds. It exited 1 with no output; it did not time out.
Checkout HEAD, MODULE lockfile and `.bazelignore` hashes were unchanged.
Configured query and whole-repository target-determinator were not attempted
after this loading failure, so no complete application-success claim is made.
