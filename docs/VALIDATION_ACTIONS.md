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
  DEPENDENCY`) — the built-in dependency-class attributes included:
  `aspect_hints` and `toolchains` collect; `exec_compatible_with` and
  `target_compatible_with` are walked and dropped like Bazel's
  `Attribute.isToolDependency` filter (`.tool("... exists for constraint
  checking, not to create an actual dependency")`,
  StarlarkRuleClassFunctions.java:174-177, 186-192); `visibility` is a nodep
  label list;
- that is not an implicit dependency: a `$`-prefixed native name
  (Attribute.isImplicit, Attribute.java:2265-2275), or a `_`-prefixed
  Starlark name whose value source converts `_x` to the native `$x` —
  DIRECT and COMPUTED_DEFAULT (AttributeValueSource.java:25-28, 64-72).
  A LATE_BOUND `_x` converts to `:x`, which is **not** implicit: its
  prerequisite's `_validation` group propagates like any explicit
  dependency's (unless it is a tool dependency); a MATERIALIZER `_x`
  behaves the same way and remains unmodelled here. This source chain is
  what the paired receipt observed as phase A08;
- not a tool dependency (`cfg = "exec"`; the `package_metadata`
  NoConfigTransition is a tool transition too — Attribute.isToolDependency,
  :2119-2127),
- and not `skip_validations=True` (genuine Bazel additionally gates that
  attribute kind behind `tools/allowlists/skip_validations_allowlist`).

Bazel iterates `ruleContext.attributes().getAttributeNames()` — the rule
class's attributes in class order — so the collected group's `to_list()`
order follows it: built-ins first (`aspect_hints`, then `toolchains`), then
the declared attributes in declaration order. The native walk matches
(`BUILTIN_DEPENDENCY_ATTRIBUTES`, then `attr_order`; receipt
`post_capture_changes` P01: both engines print
`t_toolchains,t_z,t_a` for `toolchains = [":t_toolchains"]` with `z_first`
declared before `a_second`). Native rules (genrule, filegroup, alias, ...)
collect through their `toolchains` and `srcs` (target-configuration) edges
in the same class order. The framework's own edges (resolved toolchain
implementations, the implicit test-runner attributes) are not attribute
prerequisites and contribute nothing, and `aspect_hints` prerequisites are
not analyzed by the native profile (aspect machinery), so that walk finds
no entries. Analysis tests propagate nothing
(RuleConfiguredTargetBuilder.java:294-296); those rules never reach the
native analysis (`rule(analysis_test=True)` is an unanalyzed declared
feature).

An output-file target (`//:x.out`) answers `dep[OutputGroupInfo]` with a
single `_validation` group holding the generating rule's validation outputs
(OutputFileConfiguredTarget.java:109-118), so consumers collect through
generated-file prerequisites and `bazel build //:x.out` runs the generating
rule's validation actions.

The group is built like Bazel's one stable-order `NestedSetBuilder` per
output group (`addOutputGroup`, RuleConfiguredTargetBuilder.java:636-656):
one set composed of the target's own group (first) and each collected
prerequisite set, never their flattened files (W1c,
`output_group_info::add_transitive`/`compose_validation`; native rules store
the composed set, `engine::planned_validation`). Its `to_list()` is the own
group's elements, then each prerequisite set's, first occurrences only.
Empty sets are skipped and repeated ones count once, so a group of exactly
one contributed stable-order set — a lone contributing prerequisite, a
forwarded `dep[OutputGroupInfo]._validation` — IS that set
(NestedSetBuilder.buildInternal, :181-202): `t[OutputGroupInfo]._validation
== dep[OutputGroupInfo]._validation` is True as in genuine Bazel 9.3 (the
native profile answered False before W1c; `tests/output_group_info.rs`
`validation_groups_compose_in_collection_order_and_reuse_a_lone_set`, whose
ORDER lines and identity checks genuine 9.3.0 reproduces). An output-file
target shares its generating rule's set and the ValidateTarget aspect
promotes the set itself. A topological prerequisite group (Bazel would walk
it unreversed inside the stable-order set) joins as its own `to_list()`,
keeping the order the native profile always reported. These compositions,
like the HIDDEN_TOP_LEVEL group over the default runfiles' artifacts
(`Runfiles::all_artifacts_depset`, Runfiles.getAllArtifacts
:425-440), never check Starlark's `depset()` depth limit: each target in a
chain adds one level.

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
  `_validation` through the ValidateTarget aspect) is accepted as a no-op:
  since N58 the native profile analyzes aspects, and `_validation` is always
  collected rule-side, so the mode changes nothing (`//:plain` builds
  identically with the flag on).

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

- `skip_validations=True` attributes (A15) are the one explicit
  native-unsupported boundary; the receipt records genuine behavior for it
  (genuine Bazel gates the feature behind its own allowlist).
- Aspects (`--aspects`, phase A16) are exact since N58 analyzes them: an
  aspect's `_validation` outputs run with the target it is applied to
  (`tests/validation_actions.rs::aspect_validation_outputs_run_with_the_target`;
  native column re-recorded in place by i17).
- Everything else in the receipt is exact: exit codes, materialized
  bazel-bin trees and reported output sets of 17 phases match genuine Bazel
  9.3.0, and the post-refinement row P01 matches the collection order.
- A LATE_BOUND `_x` attribute (native `:x`) propagating its prerequisite's
  validation outputs is source-derived (AttributeValueSource) and unit
  tested (`analysis::tests::implicit_attributes_follow_the_value_source_prefixes`);
  no cheap genuine row exists without a custom configuration field.

## Evidence

`tests/fixtures/validation_actions/reference_receipt.json` — paired capture
(genuine Bazel 9.3.0 sha256
`d302d22ed77ee5658b87857249baa579db84300ecebe0a1e9a357b5b20f80eb8` vs the
native release binary) produced once by
`artifacts/validation_actions-capture.py` plus one determinism rerun
(byte-identical modulo per-command timings); 18 phases: 17 exact, 1
native-unsupported. Guards: `tests/validation_actions.rs` (executable
native columns), `tests/test_validate_validation_actions.py` (hermetic
receipt structure/pins/classifications) and
`scripts/validate_validation_actions.py [--native BINARY]` (receipt replay).
