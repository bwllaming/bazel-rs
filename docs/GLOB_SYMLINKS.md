# glob() through symlinks (N28d) and label-keyed repository dictionaries (N29)

Pinned reference: Bazel 9.3 sources at `/tmp/opencode/bazel-9.3-reference`
(paths below are relative to `src/main/java/com/google/devtools/build/lib/`).

## N28d: glob() follows symlinks like Bazel

Before N28d the native `glob()` never followed links: any potentially matching
link failed with `glob could match unsupported symlink … (symlinks are never
followed)`. That blocked every `local_jdk` repository
(`rules_java` `local_java_repository.bzl` symlinks the JDK's `bin`, `lib`,
`conf`, … directories and its `BUILD` globs `bin/**`, `lib/**`; the genuine
JDK even carries a dangling `lib/src.zip`).

### Bazel's behaviour (SINGLE_GLOBS_HYBRID)

Bazel 9.3 evaluates BUILD globs with `PackageFunctionWithSingleGlobsDep`
(`skyframe/PackageFunction.java:1533`, `SINGLE_GLOBS_HYBRID`). Two globbers
run, and the paired capture (`tests/fixtures/glob_symlinks/reference_receipt.json`,
run 1: 81 EXACT / 30 EXACT_MESSAGE / 20 expected-unsupported / 50 mismatches)
shows which one each observable comes from:

1. **The legacy globber** (`packages/GlobCache.java:261-291` over
   `vfs/UnixGlob.java:771-882`) computes the results while the BUILD file is
   evaluated, one include pattern at a time in order. Symlink dirents are
   statted following links on their *logical* path (`statIfFound`,
   `vfs/SyscallCache.java:61-63`), so the kernel decides: a dangling link is
   skipped (`UnixGlob.java:814-818, 864-866`); a cycle, a link to its own
   descendant, or a recursion through a link to an ancestor *without* a BUILD
   file (`a/b/up -> ..` under `**`: `a/b/up/b/up/…` until the 41st link of one
   path; Linux may refuse from the 21st link on while other processes rename
   files, as an RCU path walk's retry keeps its link count, for Bazel too)
   fails with `[unix_jni.cc:382] PATH (Too many levels of symbolic
   links)`. Directories whose BUILD/BUILD.bazel resolves to a file are
   subpackages and never traversed (`GlobCache.java:131-156`); ignored
   subdirectories neither. `StarlarkNativeModule.runGlobOperation`
   (`packages/StarlarkNativeModule.java:851-876`) turns the IOException into an
   error event, the package's IOException
   (`TargetDefinitionContext.java:499-504`, the last one wins) and an empty
   result; evaluation continues. The package then fails with
   `no such package 'PKG': error globbing [INCLUDES] - [EXCLUDES] op=FILES|FILES_AND_DIRS: MESSAGE`
   (`TargetDefinitionContext.beforeBuild`, :822-826), thrown before the
   Skyframe globs are requested (`PackageFunction.java:551-591`), so it wins
   over everything else, even when the link is excluded or `allow_empty=True`.
2. **The Skyframe glob** (`packages/producers/PatternWithWildcardProducer.java:101-215`)
   can then only add an *unbounded ancestor expansion* of a directory link
   reached by a wildcard component ("more strict than necessary", :181-197),
   which the legacy globber does not hit because the link's target (the package
   directory, an ancestor package, the repository root, an ignored link) is
   not traversed: `no such package 'PKG': Symlink issue while evaluating globs:
   Infinite symlink expansion: LINK- > TARGET` (`PackageFunction.java:576-584`,
   `io/FileSymlinkInfiniteExpansionException.java:56-60`). Genuine Bazel prints
   no `infinite symlink expansion detected` uniqueness event with it. Literal
   components are plain FileValue lookups without that check
   (`PatternWithoutWildcardProducer.java:70-97`).
3. Empty results with `allow_empty=False` are Starlark errors with
   `GlobberUtils` texts (`packages/GlobberUtils.java:24-55`):
   `glob pattern 'P' didn't match anything, but allow_empty is set to False
   (the default value of allow_empty can be set with
   --incompatible_disallow_empty_glob).` (per include, in order, before a later
   include's I/O error) and `all files in the glob have been excluded, but
   allow_empty is set to False (…).`
4. Only BUILD/BUILD.bazel files are boundaries: Bazel 9.3 has no nested
   repositories (`skyframe/LocalRepositoryLookupFunction.java:27-35` always
   answers the main repository), so a directory with `MODULE.bazel`,
   `REPO.bazel` or `WORKSPACE` is globbed, loaded through and discovered like
   any other (fixed for glob, package lookup, `//...` discovery, label/load
   paths and outputs; this predated N28d).

Native (`glob.rs`) reproduces this: each include pattern is walked on logical
paths through the kernel (stat/readdir like UnixGlob), the first I/O error
(depth-first, name order) becomes `GlobOutcome::io_error`, which
`package.rs` records like `setIOException` and raises as the package error
after evaluation; otherwise the first wildcard-reached ancestor expansion
(`source_links::FileValues`, a FileFunction emulation,
`skyframe/FileFunction.java:101-391`) is the package error. Other FileValue
errors (cycles) never surface in a glob, as in Bazel.

Documented differences:

* With two links failing in one directory, Bazel's concurrent `processSymlink`
  tasks report whichever stat fails first (`ioException.compareAndSet`,
  `UnixGlob.java:860-872`): `cyc2` (`a <-> b`) reported `a`, `cyc_through_dir`
  (`x -> y/f`, `y -> x/g`) reported `y`. Native deterministically reports the
  first in name order (`a`, `x`).
* Framing: native prints the package error as `bazel-rs: no such package …`
  (exit 1); Bazel prints `ERROR: no such package …` (query exit 7).
* A Starlark error in the same BUILD file after a glob with a Skyframe
  expansion: native reports the Starlark error, Bazel the symlink issue.
* Special files (FIFOs, sockets) stay `unsupported glob file type` when matched
  (Bazel skips `UNKNOWN` dirents and returns links to them as files);
  non-UTF-8 names stay errors.
* A directory reached through a link (or a plain directory) as a glob result
  with `exclude_directories = 0` used as a source (`dlink_dirs0`): Bazel accepts
  the directory as a source file target and as an action input. Natively (N27)
  it is an input-file target too (the receipt row records the earlier native
  `source is not a regular file: LABEL`); only its use as an action input or
  requested output is the explicit gap, named once per label of the
  directory: `//dlink_dirs0:dump: source directory '//dlink_dirs0:d' as an
  action input is not supported by the native profile` and the same for
  `//dlink_dirs0:real`.
* Native safety cap: one pattern visits at most 100000 entries below
  symlinked host directories (explicit error; Bazel has none). The native
  repository store and `--cache-dir` below a workspace are skipped by glob and
  `//...` discovery unless they own the globbed repository (Bazel's output base
  is never below the workspace).

### Where links may point: the shared `LinkPolicy`

`source_links::LinkPolicy` (`RepositorySet::link_policy`) is shared by source
artifacts (`engine.rs`), `.bzl` loads (`loading.rs`) and `glob()`:

* Links resolving inside the repository, or into the canonical root of another
  repository materialized in this invocation (N26 `materialized_roots`), are
  followed for reads, for every repository kind.
* The repository store (stale trees, `.stage-*`, `.trash-*`) and the whole
  `--cache-dir` root are never claimed by a shorter root (N26 `blocked`).
* **Host paths (decision).** Bazel follows any link, including absolute host
  paths such as `local_jdk`'s `/usr/lib/jvm/...` (or, without `JAVA_HOME`, the
  embedded JDK under the output base). Natively, `host_reads` admits any other
  absolute host path **for reads only** and **only in repositories generated by
  repository rules** (`use_repo_rule` declarations and module extension
  repository calls, all executed through `repository_ctx`, whose
  `repository_ctx.symlink` creates exactly such links, as Bazel's
  `StarlarkBaseExternalContext.symlink` does). The final resolved path must
  still be outside the blocked store prefixes. The main repository, local path
  modules, registry archives and Git trees keep the explicit error
  (`outside its repository root`): their links are author content, not
  repository-rule output, and following them would let checked-in or archived
  links read arbitrary host files. Nothing is ever written through a link.
* **Convenience links in the main repository.** Bazel's `bazel-bin`,
  `bazel-out`, `bazel-testlogs` and `bazel-<workspace>` links point into the
  output base, a host path. A main-repository `glob()` that reaches one (for
  example `glob(["**/*.txt"])` in the root package) fails explicitly with
  `glob symlink …/bazel-out resolves to …, outside its repository root …`.
  Keep them out of globs and `//...` by ignoring them: list each link in
  `.bazelignore` (its lines are literal path prefixes) or match them all with
  `ignore_directories(["bazel-*"])` in the root `REPO.bazel`
  (docs/IGNORED_DIRECTORIES.md). An ignored directory link is neither
  traversed nor read, so the policy check is skipped for it, as Bazel skips
  ignored directories (GlobCache.globCacheShouldTraverseDirectory).
* Dangling links are skipped by `glob()` and are `missing input file` for
  source artifacts under every policy.
* Archive data in generated repositories: entries extracted by
  `repository_ctx.extract`/`download_and_extract` may be links with absolute or
  escaping targets (N19 preserves tar links literally); in a
  repository-rule-generated repository they are followed for reads under
  `host_reads` like any other link there (Bazel follows them too), never into
  the store, never for writes.
* The policy (materialized roots, canonical blocked prefixes) is memoized per
  invocation (`RepositorySet::link_policy`).

Source artifacts reached through glob results use the same policy
(`source_links::resolve_with_policy`): the label stays logical
(`@@…local_jdk//:bin/java`), the action reads the target's bytes from the
symlink-free resolved path, and the action key hashes those bytes.

## N29: `attr.label_keyed_string_dict` in repository rules and tags

Bazel does not restrict attribute types of repository rules or tag classes
(`bazel/repository/starlark/StarlarkRepositoryModule.java:80-91, 241-256`);
values are converted by `AttributeUtils.typeCheckAttrValues`
(`bazel/repository/AttributeUtils.java:51-100`) with the caller's label
converter and `what = "attribute 'NAME'"`, the where-text being
`call to 'R' repo rule with name 'N'` (`RepoRule.java:95-105`) or
`'T' tag` (`bzlmod/TypeCheckedTag.java:106-114`).
`BuildType.LABEL_KEYED_STRING_DICT` (`packages/BuildType.java:67-68, 436-490`)
converts keys as labels (`dict key element`) and values as strings
(`dict value element`) in insertion order (`packages/Type.java:565-580`), then
reports keys converting to the same label as
`duplicate labels in attribute 'NAME': L (as [k1, k2])`.

Native (`repository_rule::label_dict_json` and
`label_keyed_string_dict_json`, `module_extension::bind_tag`):

* MODULE-level calls (`use_repo_rule`, tags) transport string keys; they are
  resolved with the caller's repository mapping, values must be strings.
* Module extension repository calls may use genuine `Label` keys (the
  `rules_oci` toolchains/`oci_alias` shape: `platforms = {Label(...): "linux/amd64"}`).
  The call snapshot keeps them as `RecordedValue::LabelKeyedDict` (only when a
  `Label` key is present, so all other snapshots and provenance bytes are
  unchanged); they keep their canonical identity, string keys are resolved in
  the extension's mapping, and a string and a `Label` key naming the same
  target are Bazel's duplicate error with `Label("…")` reprs.
* `ctx.attr.NAME` / tag values are fresh frozen dicts from `Label` to string
  (`Attribute.valueToStarlark`, `packages/Attribute.java:2442-2450`).
* Errors use Bazel's texts, e.g. `in call to 'oci_alias' repo rule with name 'x',
  expected value of type 'dict(label, string)' for attribute 'platforms', but
  got [":x"] (list)`; N20's `string_keyed_label_dict`/`label_list_dict`
  errors now also name `attribute 'NAME'` (was `attribute value`).
* Non-string dict keys in MODULE.bazel (`{1: "x"}`) are transported to the
  attribute's type check (`repository_rule::DICT_ENTRIES`, only for such
  dicts) and fail like DictType.convert: `expected value of type 'string' for
  dict key element, but got 1 (int)` (also `string_dict`, `string_list_dict`,
  `string_keyed_label_dict`, `label_list_dict`; tags `in 'T' tag, …`).
* Label strings of the label dictionaries go through Bazel's label syntax
  (`crate::bazel_label`, BuildType.LABEL.convert `invalid label 'L' in WHAT:
  …`, packages/BuildType.java:412-429; Label.parseWithPackageContext's
  `absolute label must begin with '@' or '//'`, cmdline/Label.java:234-245)
  before native resolution; an apparent repository the caller does not see
  fails after all attributes converted (`AttributeUtils.java:104-135`):
  `no repository visible as '@nope' to the root module, but referenced by label
  '@nope//:a' in attribute 'platforms'` (`to the module 'NAME@VERSION'`,
  `in the extension '@@//:ext.bzl%ext'`). Labels of `attr.label`/`label_list`
  keep native resolution diagnostics.
* `Label` values are ordered like `Label.compareTo` (repository, package, name;
  cmdline/Label.java:677-685, PackageIdentifier.java:295-307), so
  `sorted(ctx.attr.platforms)` works.
* Duplicate keys in a dict *literal* are an interpreter error before any
  attribute check: Bazel `dictionary expression has duplicate key: Label("//:a")`
  vs native starlark-rust `Dictionary key repeated for Label("//:a")`
  (interpreter-generic, documented).
* Provenance: bound values serialize as the existing
  `LabelKeyedStringDict` attribute variant, present only when the attribute is
  declared. `attr.int_list` remains unsupported in repository rules and tags.

## Tests

* `glob.rs`: followed file/directory/dangling links, legacy I/O errors for
  cycles (also when excluded, literal or `allow_empty`), descendant links and
  ancestor recursions (41-link paths, earlier under concurrent renames), Skyframe expansions for links to
  package directories (wildcard vs literal), only BUILD boundaries, linked
  subpackages, BUILD-link markers, the host/materialized/blocked policy and
  store skipping, GlobberUtils empty texts.
* `source_links.rs`: `resolve_with_policy` host reads (absolute, chained,
  relative escapes, never the store) and `FileValues` chain rules.
* `repository_rule.rs`, `module_extension.rs`: label-keyed binding, caller
  resolution, recorded `Label` keys, duplicate and type errors.
* `tests/glob_symlinks.rs` (CLI): main-repository globs through links and a
  genrule reading their bytes; cycle (`error globbing`) and infinite-expansion
  (`Symlink issue`) package errors with genuine Bazel's texts; host links
  in the main repository rejected; a `local_jdk`-shaped `use_repo_rule`
  repository linking a host JDK directory (with a dangling `lib/src.zip`)
  queried and built; a `rules_oci`-shaped extension and `use_repo_rule` with
  `label_keyed_string_dict` platforms, duplicate keys and a frozen value;
  MODULE integer keys, invalid labels, unknown repositories and `sorted()` of
  Label keys.
* `tests/archive_symlinks.rs`, `tests/repositories.rs`: the archive and Git
  glob-through-link scenarios that previously asserted the native rejection
  now succeed like Bazel. The N14/N19/N26 reference receipts record the old
  native rejection (`glob could match unsupported symlink`) next to genuine
  Bazel's success; they are historical captures and are left unchanged — those
  rows are now expected to match Bazel and need a fresh paired capture.
