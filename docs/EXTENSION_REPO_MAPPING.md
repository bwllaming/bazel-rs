# Extension repository mapping and invalid-repository labels (N45)

Pinned source: `/tmp/opencode/bazel-9.3-reference` (Release 9.3.0); Java paths
are relative to `src/main/java/com/google/devtools/build/lib/`. Paired
receipt: `tests/fixtures/ext_repo_mapping/reference_receipt.json`
(`artifacts/ext_mapping-capture.py`, offline).

## Repository mapping of a generated repository

`RepositoryMappingFunction.computeInternal` (skyframe/RepositoryMappingFunction.java:100-136)
gives a repository whose canonical name starts with an extension's unique
name plus `+` the entries of `ModuleExtensionRepoMappingEntriesFunction`
(bazel/bzlmod/ModuleExtensionRepoMappingEntriesFunction.java:58-82), built
with `buildKeepingLast`, in this order:

1. the full repository mapping of the module hosting the extension's `.bzl`
   file (`getFullRepoMapping`: `""` -> main for the root module only, the
   module's own repo name, its `bazel_dep`s, then its `use_repo` imports;
   Module.java:49-68, BazelDepGraphValue.java:130-159);
2. every repository the same extension generated, under its internal name
   (so a generated repo sees its siblings and itself);
3. `override_repo`/`inject_repo` overrides (unsupported in the root module's
   native MODULE evaluation: `override_repo() is unsupported`; ignored like
   Bazel in non-root modules and under `--ignore_dev_dependency`).

A later entry wins: a generated name equal to an apparent name of the
hosting module (e.g. an extension generating `platforms`) refers to the
generated repository inside the extension's repositories (the NOTE at
:69-72). `use_repo_rule` repositories belong to the module's innate
extension (`"//:MODULE.bazel"`, ModuleFileGlobals.java:819-830): they see the
using module's full mapping plus the other repositories of the same rule in
that module.

Native already modelled this (repository.rs `prepare_ordinary_group`:
`generated_mapping` = host mapping, then generated names inserted;
repository_catalog.rs for `use_repo_rule` groups) and materializes each
generated repository lazily, only when its root is demanded
(`ordinary_root`): loading a hub package whose BUILD names siblings does not
fetch them, and a `toolchain()`'s `toolchain` attribute (a no-dependency
label) is never fetched unless the toolchain is selected.

## Invalid-repository labels (the real gap)

`Label.parseWithPackageContext` / `parseWithRepoContext`
(cmdline/Label.java:177-238) never fail on an apparent name the context
repository does not see: `RepositoryMapping.get`
(cmdline/RepositoryMapping.java:133-140) returns
`createUnvalidated(name).toNonVisible(contextRepo, SpellChecker.didYouMean(name, mapping keys))`,
whose canonical form is
`@@[unknown repo 'NAME' requested from @@CONTEXT SUGGESTION]`
(cmdline/RepositoryName.java:268-276; `@@` alone for the main repository).
Only fetching that repository fails, with
`No repository visible as '@NAME' from main repository` or
`from repository '@@CONTEXT'` (bazel/repository/RepositoryFetchFunction.java:149-153,
RepositoryName.getContextRepoDisplayString :250-258), reported by package
lookup as `no such package '@@[...]//PKG': The repository '@@[...]' could not
be resolved: No repository visible as ...`.

Before N45 native deferred this only for main-repository custom-rule
attributes, toolchain types and `--platforms`; any other context failed at
loading with `unknown apparent repository @R from canonical repository "C";
extension repository not materialized or dependency not declared`. That
stopped `~/code` at rules_oci's `oci_regctl_toolchains` hub, whose BUILD
declares a `toolchain()` for every platform in `PLATFORMS` although
`regctl_repositories` creates only `REGCTL_PLATFORMS`
(`@oci_regctl_linux_armv6` does not exist; genuine Bazel accepts it because
the label is never used).

N45 (`crate::label::{unknown_repository, unresolvable_repository,
deferred_label, unresolvable_package_message}`,
`RepositorySet::parse_deferred`):

- BUILD attributes of Starlark rules in ANY repository (main, module,
  extension-generated, `use_repo_rule`) and native-rule label attributes
  (`engine::resolve_rule_src`) keep the invalid label; the dependency edge
  fails at analysis with Bazel's text (and `referenced by '//...'`).
- `Label("@R//...")` in a `.bzl` returns the invalid Label
  (StarlarkRuleClassFunctions.label, analysis/starlark/StarlarkRuleClassFunctions.java:2124-2143);
  `str()`, `name`, `package`, equality and passthrough work, and
  `workspace_name`/`repo_name`/`workspace_root` (and `repo_root`, which this
  base does not otherwise implement) fail with
  `'M' is not allowed on invalid Label L` (Label.checkRepoVisibilityForStarlark,
  cmdline/Label.java:762-766).
- `@//` outside the main repository is the empty apparent name, which only
  the root module's mapping contains: from a dep-hosted generated repo it is
  `@@[unknown repo '' requested from @@C]`; from a root-hosted generated repo
  or a root `use_repo_rule` repo it is the main repository.
- The did-you-mean suffix is computed over the context repository's complete
  mapping (`native_rules::did_you_mean`, SpellChecker.java). Bazel iterates
  the mapping in insertion order and keeps the first of equally distant
  candidates; native mappings are sorted, so an exact tie between two
  candidates at the same edit distance picks the lexicographically first.
  `--platforms` option values (converted without a repository set) carry no
  suffix.
- Visibility labels (rule `visibility`, `package(default_visibility)`) and
  `package_group(includes)` convert the same way: a `__pkg__`/`__subpackages__`
  scope in a non-visible repository matches no package, and a package_group
  there fails only when the visibility check loads it (genuine-checked:
  `no such package '@@[unknown repo 'nope' requested from @@]//': ...`;
  Bazel also repeats it located with `referenced by '//x:g'`, native prints
  the unlocated line only). `package_group(packages)` already never matched.
- User-written label text never accepts the bracketed encoding: like
  LabelParser (cmdline/LabelParser.java:72, RepositoryName.java:178-196),
  `@@[unknown repo ...]//...` or `@[x]//...` fails with `invalid repository
  name '[...]': repo names may contain only A-Z, a-z, 0-9, '-', '_', '.' and
  '+'` (`invalid label in Label(): ...` from `Label()`); only
  `Label::parse_canonical` of native-produced values round-trips it.
- Ordering: Label.compareTo / PackageIdentifier.compareTo
  (cmdline/PackageIdentifier.java:295-307) compare `RepositoryName.getName()`,
  which for a non-visible repository is just the apparent name; Starlark
  `sorted`/`<` and order-independent attribute sorting use
  `label::repository_sort_name` (genuine-checked: `@@aaa+//:a` <
  `@@[unknown repo 'mmm' ...]//:a` < `@@[unknown repo 'zzz' ...]//:a`).
- Other errors (syntax, `..` components) are unchanged.

## Rule initializers and select() keys

`StarlarkRuleFunction.call` lifts every supplied public attribute with the
TARGET's label converter before calling the initializer
(analysis/starlark/StarlarkRuleClassFunctions.java:1810-1826), and
`BuildType.copyAndLiftSelectorList` (packages/BuildType.java:264-301)
converts select() keys to Labels there. Only values the initializer returns
as strings are then resolved relative to the rule definition (:1858-1876).
Native kept select() values unlifted, so a returned select's keys were
resolved in the rule definition's repository: rules_cc's `cc_binary`
(initializer `_cc_binary_initializer`) failed on a BUILD-file key
`"@zig_sdk//libc:gnu.2.28"` with `unknown apparent repository @zig_sdk from
canonical repository "rules_cc+"`. N45 lifts select keys (and label-typed
branch values) in the target's package (`custom_rule::lift`,
`rule_declarations::lift_select`); `//conditions:default` stays the default
key.

## Not modelled

- `<exe>.repo_mapping` / `_repo_mapping` runfiles manifests are declared
  only (docs/RUNFILES.md, N23f); no mapping content is written.
- `query` prints canonical labels where Bazel prints the main-repository
  display form (pre-existing, docs/NATIVE_BUILD_RULES.md).
- `override_repo`/`inject_repo` in the root module (non-root calls return
  early as in ModuleFileGlobals.java:723-728/:783-788), isolated extension
  usages.

## Paired receipt

`tests/fixtures/ext_repo_mapping/reference_receipt.json` (run 2; run 1 in
`history/` failed only in the harness: genuine's offline cache lacked the
canonical-id `id-*` markers). 17 cases, offline (seeded caches,
`--repository_disable_download`, file:// registry): 11 MATCH, 4
EXACT_MESSAGE, 2 MISMATCH. Both mismatches (A08, A09) are one extra
`N45|fetch|x_linux_amd64` print: genuine reuses the repository fetched by an
earlier invocation in the same output base, native re-runs every demanded
repository rule in every invocation (pre-existing, docs/REPOSITORY_RULES.md
"Other documented divergences"); within an invocation both fetch exactly the
demanded sibling and never `x_unused`. The genuine rules_oci 2.2.7 cases
(B01-B03; the toolchains hub naming the non-existent `@oci_regctl_linux_armv6`)
all MATCH; the main 2356bf4 binary fails B01 with the exact `~/code` stop.

The run-2 native binary (sha256 523f6c50…) was built before the last source
edit (label.rs `unresolvable_repository` also rejects `/` in the apparent
name, unreachable from label parsing); the receipt's `source` block hashes the
final source. `artifacts/ext_mapping-native-replay.py` replays all 17 cases
with the rebuilt binary (`native_replay.json`): identical native results.

Post-capture changes (review fixes, after run 2; listed in the receipt's
`post_capture_changes`): visibility/package_group includes through
`parse_deferred`, bracketed names rejected in user-written labels,
apparent-name ordering of invalid labels, `invalid label in Label():` prefix
for Bazel-worded repository-name errors. None of the 17 capture cases
exercises them (each was checked against genuine Bazel 9.3.0 offline and is
pinned by tests/ext_repo_mapping.rs).
