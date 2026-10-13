# Rust Bazel implementation plan

Target: Bazel 9.3 compatibility, with measured startup and build performance.
This repository starts empty. Full parity is not yet implemented or claimed.

- [x] Verify the upstream release and inventory `~/code` requirements (read-only).
- [x] Install a pinned, project-local Rust toolchain with explicit direnv setup.
- [x] Establish native Rust architecture and a runnable, explicitly bounded first slice.
- [x] Add correctness tests and an upstream differential/performance harness.
- [x] Run formatting, linting, tests, and release builds.
- [x] Exercise the implementation against `~/code` without changing its tracked files.
- [x] Record evidence, unsupported features, and next milestones.

Constraints: preserve existing changes in `~/code`; no implicit upstream fallback;
never report a successful build for unsupported semantics; no unmeasured speed claims.

This checklist covers the initial foundation only. Full Bazel 9.3 parity and the
main application's build/test suite remain unimplemented; see docs/ROADMAP.md.

## Loading and scheduling milestone (verified native subset)

- [x] Native local `.bzl` loading, module freezing/cache, macros and load-cycle errors.
- [x] Package-aware `glob`, `native` struct, package defaults and source exports.
- [x] Public/private/package/subpackage visibility enforcement.
- [x] Bounded parallel action DAG scheduling with dependency/failure guarantees.
- [x] Differential fixtures and expanded source-only `~/code` coverage.
- [x] Formatting, linting, unit/integration tests, release build and measured evidence.

## Custom-rule and module-graph increment

- [x] Custom rule declaration/analysis: typed attributes, `DefaultInfo`, custom
      providers, depset traversal orders, artifacts and write/run/run_shell actions.
- [x] Args formatting/mappers, cross-package provider identity and output propagation.
- [x] Explicit local-registry module selection, nodep discovery and multiple-version overrides.
- [x] Scoped custom-rule differential checks, including negative analysis cases.
- [x] Captured module-graph differential: 11 exact reference JSON cases and five
      separate native-only capability-boundary probes (`artifacts/module-validation.json`).

## Local external-repository increment

- [x] Local override BUILD/`.bzl` loading with canonical/apparent repository mappings.
- [x] Lexical `.bzl` Label/default context, repository-qualified visibility and
      external artifact identity; metadata short paths remain separate from execution paths.
- [x] Lazy MODULE resolution for main-only source operations; filesystem registry
      options on query/build/mod graph.
- [x] Implement registry `source.json` type `local_path` source-root lookup.
- [x] Validate registry local-path source support, including live-source changes.
- [x] Complete the separate repository differential: 20 scoped checks
      (`artifacts/repository-validation.json`), cleanup and reviewer approval.
- [x] Record the safe native application-loading failure and unchanged source
      checkout (`artifacts/mainrepo-application-loading.json`); no application parity.

## Explicit HTTP registry and verified archive increment

- [x] Preserve selected registry origin and exact authoritative MODULE bytes.
- [x] Native bounded HTTP(S), explicit lockfile-off opt-in, 404/410 fallback and
      hard transport/metadata errors; filesystem registry behavior preserved.
- [x] Verified tar/tar.gz/ZIP materialization, mirrors, executable overlays,
      authoritative MODULE replacement, staged publication and checked caches.
- [x] Reject unsupported patches, links, archive extensions and unsafe paths;
      document strict native-profile limits, not full archive parity.
- [x] Reproduce and fix warm-cache URL-policy bypass and skipped ZIP CRC checks.
- [x] Complete 17-check remote report: three format differentials and fourteen
      native-only lifecycle/security checks, with captured cleanup receipts.
- [x] Pass 328 Rust tests, 143 Python tests, formatting, strict Clippy, release
      compilation and error-free rust-analyzer diagnostics.
- [x] Preserve adjacent differential coverage; replace the obsolete HTTPS
      rejection probe with a genuinely unsupported registry scheme.
- [x] Probe real application loading with HTTPS: advances to the explicit
      `rules_swift@3.1.2` `use_repo_rule` blocker, preserving the checkout.

Current evidence: `artifacts/remote-validation.json`,
`artifacts/mainrepo-remote-loading.json`, `docs/REMOTE_REPOSITORIES.md`.

## Direct repository declaration increment

`use_repo_rule` metadata now uses explicit root/dependency contexts, deferred
MODULE attribute capture, and owner-scoped pending-repository diagnostics. MODULE
syntax rejects global rebinding and unsupported star-argument forms before
evaluation; ignored dev extension tags do not trigger unsupported value conversion.
This remains recording-only, not repository-rule execution. See
`docs/DIRECT_REPOSITORY_DECLARATIONS.md` for the bounded contract.

Its full check passes 349 Rust tests, 169 Python tests, formatting, strict Clippy,
and release compilation. Real-surface metadata/reference and application probes
are retained under `artifacts/direct-*` and `artifacts/mainrepo-direct-loading.json`.
The latter stops at the unsupported `hermetic_cc_toolchain` patch override.
The direct declaration report passes 46 differential cases, one lazy-loading probe
and one native-only unsupported-execution boundary. Adjacent module/local/remote
reports pass 16/20/17 checks on the same release binary.

## Bounded native text patch increment

Local single-version patches project root MODULE changes before discovery/MVS.
Original registry bytes remain separate from evaluation bytes. Archive source
materialization applies overlays, ordered verified registry patches, original
MODULE replacement, then snapshotted local patches before publication. Cache keys
include local patch contents and order; unsupported commands, external patch
inputs and live local-path transformations fail explicitly.

The full check passes 408 Rust tests and 194 Python tests, formatting, strict
Clippy and release compilation. `artifacts/patch-validation.json` passes 14 genuine
reference differential cases and 13 separately identified native-only boundaries.
Exact graph selection and built UTF-8 file bytes prove the supported pipeline.
The parser counts lexical path components before normalization and rejects
adjacent ambiguous plain file sections, whose divergent upstream behavior is
retained in `artifacts/patch-order-*-report.json`. See `docs/PATCHES.md` for limits.

The read-only application probe advances to the unsupported `rules_distroless`
Git override; the checkout remains unchanged. This is not application query,
build or test success, nor full patch/Bazel compatibility.

## Remaining compatibility gates

- [ ] Complete Bzlmod/repository compatibility beyond local selection/loading:
      complete downloader/archive semantics, Git retrieval, full patch semantics, lockfiles, built-in closure,
      module extensions and repository rules using pinned fixtures.
- [ ] Configurations, toolchains and providers sufficient for actual repository
      Go/Python/Node rule analysis, followed by safe executable/test targets.

Captured differential artifacts record 39 custom-rule and 24 loading checks;
module JSON validation is separate from the 20 repository differential checks.
The previous local increment passed 296 Rust tests and 124 Python tests; the
remote increment's updated counts are recorded above.
This still does not mean full application builds/tests work.
