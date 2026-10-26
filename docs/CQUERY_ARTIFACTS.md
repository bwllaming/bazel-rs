# Semantic Files versus execution inputs

Cquery performs full rule/provider analysis but skips **global inter-target**
action conflicts (`CqueryCommand.checkForActionConflicts(false)`). Distinct
generating rules can therefore expose different Files with the same execution
path. These are not interchangeable semantic artifacts.

For cquery only, native rule source collection now uses File identity (including
owner and artifact kind), retaining first-occurrence order. In particular,
filegroup mirrors `Filegroup.java`'s `NestedSet<Artifact>`: `a + b` retains two
owner-distinct Files even when both writers declare `same.txt`; `a + alias(a)`
deduplicates to one File. Singleton executable selection consequently remains
correct. Tree artifacts, unresolved symlinks, and cross-owner file/tree paths
use the same identity rule.

The execution input normalizer is unchanged. Both `build` and `build --nobuild`
retain shared-action reconciliation and strict execution-path checks. The switch
is explicit cquery capture, **not** `!execute_build`. No generic conflict checks
are disabled. Same-rule tree/file declarations, orphan Files, unequal duplicate
actions, required-provider checks and executable ownership checks still run and
fail cquery. The existing ordinary-build normalization of shared outputs is not
changed by this scoped correction.

The semantic collector stores only cheap shared File/Artifact handles in an
ordered result and a lookup set. It does not allocate the execution normalizer's
unused location/path vectors, fingerprint action inputs or stage any tree.

`tests/fixtures/cquery_artifacts/genuine.json` captures fresh genuine Bazel 9.3
results: seven ownership/kind/ordering/executable probes, five invalid-rule
failures, identical-writer build success and unequal-writer build/nobuild errors.
`tests/cquery_artifacts.rs` checks that contract, graph hit/verify/BUILD/.bzl
invalidation, and action/workspace-status sentinels. Native configuration identity
and existing action keys are not rekeyed.

## Semantic support and output-target identities

Cquery registers actual validated action-output Files (including root-relative
shareable outputs), without lowering actions. Only **predeclared** outputs enter
the path lookup used by package-level generated-file targets. Runtime declarations
at the same path cannot replace the predeclared generating owner; normal provider
consumers resolve through the owner-aware File index.

Executable FilesToRunProvider exposes runfiles and repository-mapping manifest
Files during analysis. Their existing Option values are registered as semantic
identities, honoring manifest options/None. Cquery never calls runfiles execution
planning, mapping rendering, input expansion or manifest/tree staging to create
these identities. The captured support-file cases include default options,
`--enable_runfiles=no`, and `--nobuild_runfile_manifests`; hit/verify tests retain
their exact provider metadata.
