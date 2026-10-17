# Genuine rules_shell sh_binary / sh_test analysis (N50)

Genuine `@rules_shell` 0.6.1 `sh_binary` (with and without
`use_bash_launcher`), `sh_test` and `sh_library`, and
`@bazel_tools//tools/test:collect_coverage` (a rules_shell `sh_binary` that
`bazel test` analyzes for every test), analyze and build natively in the
builtin profile. Before N50 analysis stopped at
`attribute 'data' flags=[SKIP_CONSTRAINTS_OVERRIDE]; attribute 'deps'
allow_rules=[sh_library]`, then at the label-keyed `root_symlinks` of
`@rules_shell//shell/runfiles:runfiles_at_legacy_location`, the external
`skip_conflict_checking` allowlist and `coverage_common.instrumented_files_info`.

## `attr.label_keyed_string_dict` rule values

* Loading (BuildType.LABEL_KEYED_STRING_DICT, `packages/BuildType.java:436-490`):
  keys are labels resolved in the package context (strings or `Label`s),
  values strings; `select()` branches and `select() | {...}` concatenation are
  configurable values. Type errors use the conversion texts
  (`expected value of type 'string' for dict value element, ...`); keys that
  resolve to the same label are `//pkg:x: duplicate labels in attribute 'm'
  of 'lk': //pkg:a.txt (as ["a.txt", ":a.txt", "//pkg:a.txt"])`
  (AttributeProvider.java:252-267 prefixes the target).
* Dependency edges: every key, with the attribute's `cfg`
  (crate::engine rule edges; query `deps()`/`labels()` include them, also both
  select() branches, LabelKeyedDictType visitLabels).
* Prerequisite validation (RuleContext.Builder, `analysis/RuleContext.java:1845-2096`):
  keys are checked like any label attribute — no files without `allow_files`
  (`source file '//pkg:f.txt' is misplaced here (expected no files)`),
  extension lists, `providers`, and `allow_rules` (below).
* `ctx.attr.<name>` is a frozen dict of the prerequisite Targets to their
  strings, in prerequisite (key) order
  (StarlarkAttributesCollection.convertAttributeValue,
  `analysis/starlark/StarlarkAttributesCollection.java:361-369`); an alias key
  is its own Target (label of the actual). `ctx.files.<name>` is the
  deduplicated union of the keys' files. Targets are hashable (immutable
  Starlark values with identity equality), so they can key dicts.

## `allow_rules`

`attr.label*(allow_rules = [...])` applies validateRuleDependency
(`RuleContext.java:2002-2096`): a rule prerequisite of an allowed class is
accepted; otherwise satisfied mandatory `providers` still accept it, else the
unfulfilled requirements are joined with `and`:
`in deps attribute of sh_binary rule //bad:b: genrule rule '//bad:g' is
misplaced here (expected sh_library)`, `alias '//e2:ga' referring to genrule
rule '//e2:g' is misplaced here (expected prov)`, `filegroup rule '//e6:fg'
is misplaced here (expected prov) and '//e6:fg' does not have mandatory
providers: 'P'`. The expected list is RuleClassNamePredicate's
`StringUtil.joinEnglishList` (`a`, `a or b`, `a, b, or c`).

## `flags`

Every name must be an Attribute.PropertyFlag (`packages/Attribute.java:83-205`),
else `unknown attribute flag 'X'` at `attr.*()` (`:466-479`).
`SKIP_CONSTRAINTS_OVERRIDE` is accepted as a **documented no-op**: Bazel reads
it only in RuleContextConstraintSemantics.getConstraintCheckedDependencies
(`analysis/constraints/RuleContextConstraintSemantics.java:731-736`) to leave
the attribute's dependencies out of the constraint-environment check. Native
analysis performs no environment check, and everything that could make it
fail (non-empty `compatible_with`/`restricted_to`, environment rules) is
already an explicit gap. Every other flag (`DIRECT_COMPILE_TIME_INPUT`, ...)
still fails analysis explicitly when the attribute has a value.

## `coverage_common.instrumented_files_info`

CoverageCommon.instrumentedFilesInfo (`analysis/test/CoverageCommon.java:46-117`):
parameter types (`in call to instrumented_files_info(), parameter 'ctx' got
value of type 'NoneType', want 'ctx'`), the body's casts (`at index 0 of
source_attributes, got element of type int, want string`, coverage support
files, `coverage_environment` dict types), the private-API restriction of
non-empty support files / environment / reported sources (`file
'//cov:defs.bzl' cannot use private API`), then InstrumentedFilesInfo.EMPTY:
coverage is never enabled natively (`--collect_code_coverage` is rejected), so
InstrumentedFilesCollector.collect returns it (`:96-109`). The value has type
`InstrumentedFilesInfo`, empty `instrumented_files`/`metadata_files` depsets,
NativeInfo's repr `struct(instrumented_files = depset([]), metadata_files =
depset([]))` and equality; it is a returned provider of the target. Non-empty
`reported_to_actual_sources` fails explicitly. The `InstrumentedFilesInfo`
global and its `get_coverage_*` methods are not provided.

## `skip_conflict_checking`

`ctx.runfiles(skip_conflict_checking = True)` uses Bazel's whole
INTERNAL_STARLARK_API_ALLOWLIST (`packages/BuiltinRestriction.java:36-205`),
including external module entries matched by canonical-name prefix
(`rules_shell` → `@@rules_shell+//...`) and the main repository's apparent
mapping; others fail with `file '@@other_rules+//:defs.bzl' cannot use
private API`.

## Evidence

`tests/sh_binary.rs` runs the genuine rule sources
(`tests/fixtures/sh_binary/rules_shell`, 12 byte-identical files of the 0.6.1
archive, 4 marked offline replacements; README.md there) against
`tests/fixtures/sh_binary/workspace` offline. The paired capture
(`artifacts/sh_binary-capture.py`, receipt
`tests/fixtures/sh_binary/reference_receipt.json`) runs genuine Bazel 9.3.0 and
native on the same workspace with the **genuine** module from the builtin
fixture registry (full builtin closure, no stubs; source archives from a
verified seeded repository cache, `--repository_disable_download`):

| rows | verdict |
| --- | --- |
| build of `//dump` (DefaultInfo files, executable, default/data runfiles files/symlinks/root_symlinks, RunEnvironmentInfo of collect_coverage, sh_binary, launcher sh_binary, sh_library, sh_test, the runfiles alias), `//sh:all` (launcher bytes, entrypoint symlinks), `//lk:all`, `//cov:ok`; `-c opt` select; `collect_coverage` | EXACT |
| LKSD duplicate labels, allow_rules (direct + alias), providers, no files, allow_rules+providers, sh_binary `deps` allow_rules | EXACT |
| unknown flag, instrumented_files_info errors, external skip_conflict_checking | EXACT_MESSAGE (traceback framing only) |
| `query deps()` / `labels()` | EXACT |
| `bazel run` sh_binary / launcher, `bazel test` sh_test, other flags | EXPECTED_NATIVE_UNSUPPORTED |

Run 2 counts: 11 EXACT, 7 EXACT_MESSAGE, 4 EXPECTED_NATIVE_UNSUPPORTED (run 1:
6 harness-only MISMATCHes, see the receipt). The reference's `bazel run`
outputs, runfiles trees, MANIFEST and `_repo_mapping` are recorded in the
receipt (`reference_run_outputs`); genuine `bazel test //sh:t` PASSED.

## Remaining

* Runfiles trees, MANIFEST/`_repo_mapping` materialization and `bazel run`
  are N23f; `bazel test` (test runner, implicit test attributes and the
  `collect_coverage` edge) is N23g. Neither is merged on this base, so the
  sh_binary run output and a genuine `sh_test` under `bazel test` are not
  native yet; the analysis/build they need is (see the receipt for the
  genuine outputs to match).
* `ctx.expand_location` with location functions (sh_binary
  `env = {"X": "$(rlocationpath ...)"}`) fails explicitly
  (crate::ctx_surface).
* `InstrumentedFilesInfo` as a global / `target[InstrumentedFilesInfo]`,
  coverage collection.
