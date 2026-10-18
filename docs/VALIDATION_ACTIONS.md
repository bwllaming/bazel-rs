# Validation output groups and validation actions (N56)

Bazel 9.3 parity for the `_validation` output group end to end: how
validation outputs are declared, collected transitively, selected at the
command line, executed with the depending target, and kept out of the
reported build outputs. Verified against a paired genuine-Bazel capture
(`tests/fixtures/validation_actions/reference_receipt.json`), not a full
parity claim.

## The group and its producers

- `OutputGroupInfo(**kwargs)` accepts arbitrary group names, `_validation`
  included, from any rule or aspect implementation
  (`OutputGroupInfoProvider.constructor`, OutputGroupInfo.java:567-586).
  The i15 application boundary that motivated this work is exactly this
  shape: rules_go's `go_library` returns
  `OutputGroupInfo(_validation = [validation_output])` directly from the
  rule (go/private/rules/library.bzl:69-75) — no aspect is involved.
- **There is no Starlark `ValidationProvider` in Bazel 9.3.** Validation
  outputs are only the `_validation` group; referencing the symbol fails at
  load in both engines (receipt phase A17). `DefaultInfo` has no
  `output_groups` parameter either — output groups are declared by
  returning `OutputGroupInfo`.
- A rule (native allowlist only) may return
  `OutputGroupInfo(_validation_transitive = ...)` to *replace* the automatic
  propagation with its own set; the private API is rejected for any rule
  defined outside `BuiltinRestriction.INTERNAL_STARLARK_API_ALLOWLIST`
  (RuleConfiguredTargetBuilder.java:366-377) with
  `//pkg:defs.bzl cannot access the _transitive_validation private API`.

## Transitive collection

`RuleConfiguredTargetBuilder` applies to **every** rule target, Starlark and
native alike (propagateTransitiveValidationOutputGroups, :175-176,
360-430): the target's `_validation` group is the union of its own returned
outputs and the `_validation` groups of every prerequisite reached through

- a label-class attribute (`label`, `label_list`, `label_keyed_string_dict`,
  `string_keyed_label_dict`, `label_list_dict` — `getLabelClass() ==
  DEPENDENCY`),
- that is not an implicit dependency (`$`-prefixed, Attribute.java:2273-2275,
  **and `_`-prefixed Starlark private attributes** — the observed genuine-9.3
  contract of receipt phase A08: a `_`-prefixed
  `attr.label(default = ...)` prerequisite's validation outputs do not
  propagate while a public attribute's do),
- not a tool dependency (`cfg = "exec"`; the `package_metadata`
  NoConfigTransition is a tool transition too — Attribute.isToolDependency,
  :2119-2127),
- and not `skip_validations=True` (genuine Bazel additionally gates that
  attribute kind behind `tools/allowlists/skip_validations_allowlist`).

The framework's own edges (resolved toolchains, the implicit test-runner
attributes) are not attribute prerequisites and contribute nothing. Native
rules (genrule, filegroup, alias, ...) collect through their `srcs`
(target-configuration) edges only. Analysis tests propagate nothing
(RuleConfiguredTargetBuilder.java:294-296); those rules never reach the
native analysis (`rule(analysis_test=True)` is an unanalyzed declared
feature).

An output-file target (`//:x.out`) answers `dep[OutputGroupInfo]` with a
single `_validation` group holding the generating rule's validation outputs
(OutputFileConfiguredTarget.java:109-118), so consumers collect through
generated-file prerequisites and `bazel build //:x.out` runs the generating
rule's validation actions.

## Command-line selection

`--output_groups` (BuildRequestOptions.java:160-177, comma-separated,
repeatable) with `OutputGroupInfo.determineOutputGroups` (:207-243):

- tokens keep their `+`/`-` prefix; if every token is prefixed, the default
  set (`default`, `temp_files_INTERNAL_`, `_hidden_top_level_INTERNAL_`) is
  kept and adjusted, otherwise the tokens replace it;
- the `default` group is the target's files to build;
- an unknown group contributes no artifacts and no error
  (TopLevelArtifactHelper skips empty groups, :221-224);
- `_validation` is added whenever validation actions run and **cannot be
  subtracted**: `--output_groups=-_validation` still builds them. The only
  switch is `--run_validations=false` (:178-190).
- `--experimental_use_validation_aspect=true` (the ASPECT mode that promotes
  `_validation` through the ValidateTarget aspect) fails explicitly: aspects
  are not modeled by the native profile.

Groups whose name starts with `_` (validation, hidden top level) are built
but never reported as the build's outputs
(TopLevelArtifactHelper.getAllArtifactsToBuild, :228-236); an
`--output_groups` override drops the hidden top level group, so default
runfiles artifacts are not built then. `bazel test` keeps the `default`
group for failure reporting (BuildRequest.shouldRunTests), and `bazel
query` never sees output groups at all (files-to-build is the default
group). `--compile_one_dependencies` remains an unmodelled option and fails
explicitly.

## Native profile boundaries

- Aspects (`--aspects`, phase A16) and `skip_validations=True` attributes
  (A15) are explicit native-unsupported boundaries; the receipt records
  genuine behavior for both (an aspect's `_validation` outputs run with the
  target it is applied to).
- Everything else in the receipt is exact: exit codes, materialized
  bazel-bin trees and reported output sets of 16 phases match genuine Bazel
  9.3.0.

## Evidence

`tests/fixtures/validation_actions/reference_receipt.json` — paired capture
(genuine Bazel 9.3.0 sha256
`d302d22ed77ee5658b87857249baa579db84300ecebe0a1e9a357b5b20f80eb8` vs the
native release binary) produced once by
`artifacts/validation_actions-capture.py` plus one determinism rerun
(byte-identical modulo per-command timings); 18 phases: 16 exact, 2
native-unsupported. Guards: `tests/validation_actions.rs` (executable
native columns), `tests/test_validate_validation_actions.py` (hermetic
receipt structure/pins/classifications) and
`scripts/validate_validation_actions.py [--native BINARY]` (receipt replay).
