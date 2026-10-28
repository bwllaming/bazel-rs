# Incompatible configured targets (Bazel 9.3)

Configured query includes both directly and indirectly incompatible targets,
including explicit roots and wildcard results. These are successful, actionless
configured results, not analysis failures or omitted nodes.

## Analysis contract

`Engine::check_target_compatibility` runs after the target platform and config
conditions and before ordinary prerequisites or toolchain resolution. An unmet
constraint or config setting publishes an incompatible result with the declared
rule identity. Ordinary dependencies and implementations are not analyzed.
Constraint/config-setting validation errors still fail.

After ordinary prerequisites, transitioned dependencies and selected toolchain
implementations have resolved, indirect replacement precedes normal factory,
provider, visibility and testonly validation. The exact exemptions are `toolchain`,
`config_setting` and `label_flag`. Incompatible prerequisites remain real graph
edges. Registered toolchains that were not selected do not contaminate consumers.

Causes are a shared DAG of labels/configured keys, not prerequisite provider maps.
The phase-two entry point publishes empty files/runfiles before prepare/compute/
commit, including early `plan_pending` flushes. Dependency views expose empty
DefaultInfo/files-to-run (no executable) without synthesizing ordinary output
groups, coverage providers or declared outputs. Alias and label_setting
replacement retains its own label; exempt label_flag retains its build setting
and forwards the incompatible actual. Configured keys and action keys are
unchanged.

Aspects on an incompatible base are nonapplicable before their own prerequisites
or implementations. An aspect on a compatible base with its **own** incompatible
attribute still runs using the empty view. Required providers are checked normally
and can fail before the aspect runs. An aspect's own incompatible selected
toolchain has no successful genuine runtime capture in this milestone; the
offline reference fixture stopped at missing rules_swift source. No parity claim
is made for that case.

## Cquery projection and reuse

Direct replacements have edges to analyzed config conditions and compatibility
constraints, but no target-platform or ordinary-prerequisite edge. Their ordinary
select attributes are projected using the retained conditions without loading
selected or inactive dependencies. `rule_input` retains declaration-level labels,
including inactive branches; `configured_rule_input` records actual configured
edges. A directly incompatible output remains GENERATED_FILE metadata, with no
invented generating-rule dependency edge.

An invalid ordinary select under direct replacement does not prevent label output.
The graph retains the projection error as plain data. Proto/streamed_proto fail
explicitly before writing results if a requested node needs that invalid attribute;
`attr()` fails when reading that attribute. No empty value is substituted. Genuine
9.3 textproto crashes after successful analysis with IllegalStateException/exit 37
for the captured no-match and ambiguous cases. Binary proto shares its formatter
source, but those two crash cases were captured only as textproto. Native's safe
format error is deliberate.

The graph snapshot key is `cquery-graph-v2`. Its payload remains compact binary
data, without rule/provider/Starlark heaps; the engine is freed before output
encoding. Output format and query traversal are excluded from the analysis key,
so label, proto, repeated deps and roots reuse the same graph, including retained
format errors. Existing observed-input validation covers BUILD, implementation,
platform, constraint and config-condition edits. Source presence is observed;
source content remains TD's own hash input, as documented in CQUERY.md.

## Remaining boundary

Build/test, including `--nobuild`, still reject incompatible requested closures
with the explicit unsupported-skipping diagnostic. Wildcard skipping, original
positive SINGLE_TARGET explicitness, `--skip_incompatible_explicit_targets`, test
SKIPPED state and BEP require the separate root-policy increment. Keep-going
incompatibility is not converted into successful skipping. Ordinary compatible
builds retain normal action-plan checks.

Cquery Starlark formatting/default TD incompatible filtering is still unsupported;
**TD requires `-filter-incompatible-targets=false`**. Whole-repository TD success
is not established by a configured query.

The native configuration model still lacks genuine's separate untrimmed
top-level label_flag/label_setting instances and uses incoming ConfigIds for
platform trees. The exact reference fixture yields 31 genuine vs 29 native
configured results: flag_bad and setting_bad have two genuine instances each,
one native instance each. Label/kind sets, label edges and selected attributes
match. This pre-existing identity difference is documented rather than changing
DefaultConfigId or inventing genuine checksums. Actual incoming/split/exec
configuration identities are preserved and tested independently.

## Evidence

Pinned `IncompatibleTargetChecker.java:145–174,257–311,328–359`,
`ConfiguredTargetFunction.java:300–375`, `AspectFunction.java:299–314`, and
`ProtoOutputFormatterCallback.java:342–375` (9.3.0, e89755ba).

The independent genuine captures are indexed in
`tests/fixtures/target_compatibility_cquery/REFERENCE.md`. Existing
target_compatible_with's 13 matching and 11 build-policy divergence cases remain
strict. New tests cover action/implementation/status sentinels, direct/indirect
edges, selects, aliases/outputs/flags, validation order, early flushes, aspects,
selected/unselected toolchains, incoming/split/exec configurations, decoded proto
and streamed_proto, binary snapshot reuse and strict verification.

## Bounded application follow-up

Native cquery only, on the isolated read-only PR clone at
`0b29c533c060e5ccb9466bbd200d2b0510be01da`, private absolute state, all rc files
ignored and remotes disabled. Every run retained 8,192 MiB / 900 seconds:

| Query | Wall seconds | Peak RSS MiB | Output bytes |
|---|---:|---:|---:|
| DuckDB ARM64 runtime deps | 0.755 | 156.6 | 7,434 |
| `deps(//...)`, analysis/save | 218.201 | 8,033.4 | 481,114,922 |
| one repeated `deps(//...)`, graph hit | 2.936 | 1,633.6 | 481,114,922 |
| `//...`, shared-graph hit | 2.445 | 1,634.5 | 139,683,706 |

Full-query headroom is **158.6 MiB**, so the 8 GiB cap remains significant.
The previous run stopped at DuckDB's host-incompatible ARM64 runtime after
220.186 s / 7,624.4 MiB without output. The completed graph includes additional
analysis; those two peak figures are not an equivalent-work memory comparison.

Official-schema streaming decode validates 265,121 configured results,
1,116,527 configured edges and 34 configurations. Every edge endpoint exists
and every checksum agrees with its configuration table. The 45,227 root results
are byte-equivalent configured records from the deps graph. Warm deps output is
byte-identical (SHA256
`ba4760e23df4d3a48e3dd0d9b2de3e02b82a5b1ee957bffb8733aa9036f58cbc`).
The ARM64 target in the host configuration has only its two compatibility
constraint inputs; other real configurations of that label remain distinct.

Clone HEAD, lockfile and `.bazelignore` canaries are unchanged. No original
checkout, application build/test, genuine Bazel or TD run was involved.
Receipts: `/home/coder/opencodetmp/target-compatibility/pr-{arm64,full,full-warm,roots}`
and `application-decoding.json`. Release SHA256:
`0591e3613ee7b78a8be18c512d0f543ad767ed879a824f2bdda10e9a0d084cbd`.
