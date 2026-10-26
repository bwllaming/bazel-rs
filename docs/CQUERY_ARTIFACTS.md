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

## Retention boundary

A bounded full-source memory profile found 133,488,492 bytes retained in
`ActionPlanning.repositories` before the 8 GiB ceiling. These per-target
transitive repository-name sets are execution-plan inputs for rendering repository
mapping/runfiles manifests, not provider data. Cquery now omits constructing and
retaining those sets; it keeps the real RepositorySet and the actual semantic
support Files. Build and --nobuild still compute them unchanged. No configured
node, provider, action declaration or validation is dropped.

The unoptimized full-source attempt exited 33 at the unchanged 8192 MiB ceiling.
A temporary checkpoint used the existing retained-memory profiler at about
5.5 GiB, then stopped explicitly; that diagnostic code was removed. With the
execution-only closures omitted (and current main's independent aspect projection
fix integrated), the bounded full query succeeds:

| query | wall | peak child RSS | output bytes | snapshot |
|---|---:|---:|---:|---|
| first deps(//src/...) | 142.79 s | 7,997 MiB | 464,264,083 | initialization left lockfile write pending |
| second deps(//src/...) | 144.74 s | 8,001 MiB | 464,264,083 | saved |
| third deps(//src/...) | 2.04 s | 1,514 MiB | 464,264,083 | hit |
| //src/..., same universe | 1.74 s | 1,516 MiB | 139,599,392 | hit |

All three deps outputs have identical SHA256. Independent upstream-schema decoding
reads 261,196 targets, 31 configurations and 1,104,178 configured inputs. Peak
analysis RSS remains close to the ceiling; this is bounded success, not unlimited
memory headroom. No native daemon is involved.

The official target-determinator v0.34.0 original and split matrices still match
genuine 9.3 (11/11 each). A new shared-output mutation changes only writer b's
content, making its action conflict globally with a; cquery succeeds and both
engines return exactly b, its filegroup, consumer and transitive parent. Actual TD
was run only in scratch git repositories, never on the read-only monorepo.
