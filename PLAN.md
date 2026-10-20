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

## Commit-pinned Git increment

The native engine uses isolated system Git for full-commit HTTPS/file overrides,
verifies fetched/checked-out SHA identity, validates final patched MODULE before
publication, and rechecks source manifests on cache hits. Discovery and repository
access share one normalized cache path; CLI-relative paths remain cwd-relative.
The regular-file-only profile and subprocess/export limits are documented in
`docs/GIT_OVERRIDES.md`.

The full check passes 452 Rust tests and 216 Python tests, formatting, strict
Clippy and release compilation. The real-Git report passes four genuine Bazel
differentials and 15 native-only checks. Public API/CLI regressions also prove
case-insensitive full-SHA consistency and offline reuse of a workspace-relative
library cache. Concurrent Linux procfs exit sentinels/ESRCH are distinguished from
permission and malformed-data errors, preserving fail-closed cleanup.

The application fetch verifies the requested `rules_distroless` commit but stops
at its tracked internal symlink. The link is not omitted or flattened; preserving
it safely is the next source-materialization increment. This remains incomplete
application query/build/test compatibility.

## Direct repository-rule increment

The bounded direct executor loads real transitive Starlark definitions, validates
whole innate usages and runs demanded file/template callbacks. It preserves
MODULE-owner mappings, private frozen helpers, label-default ownership, binary
template bytes and ordered substitutions. Explicit source/attribute guards reject
unsupported byte-string computations rather than silently changing output.

Initialization and callbacks have evaluator limits. Repository demand cycles fail
before recursive initialization; publication follows successful private execution
and verified content/mode snapshots. Every fresh invocation reevaluates callbacks.
Ordinary source aliases remain lazy and do not force unrelated extension namespace
resolution. See `docs/REPOSITORY_RULES.md` for the exact subset and API limits.

The application still stops at the unmaterialized nogo module-extension repository;
this increment does not establish application query/build/test or full parity.

Verification: 608 Rust and 270 Python tests pass, along with strict Clippy,
formatting, release compilation and error-level Rust Analyzer diagnostics. The
real CLI report passes seven Bazel differentials and nine native-only checks;
all seven adjacent suites pass against the same release with recorded cleanup.

## Preserved Git file-link increment

The preserved Git file-link implementation retains literal internal file
symlinks and records their kind/target in verified manifests. Committed blob
bytes are bounded and validated before checkout, then compared to native link
targets; policy v3 invalidates earlier unchecked link caches. Complete and
stripped namespaces plus post-patch/warm snapshots share safety validation.
Directory/external links and general source/action/load symlinks remain outside
this subset (`docs/GIT_SYMLINKS.md`).

The full check passes 497 Rust tests, with the final Python suite at 240 tests,
plus formatting, strict Clippy
and release compilation. The reviewed CLI report passes four genuine Bazel
differentials and 15 native-only checks; all six adjacent suites pass against
the same release binary with recorded cleanup.
The application probe preserves the required `rules_distroless` link but still
fails on the unmaterialized `@io_bazel_rules_nogo` extension repository. The
checkout is unchanged; no application query/build/test success is claimed.

## Credential helpers and the HTTP remote cache (N41)

- [x] Bazel 9.3 credential helpers (scopes, wildcards, `%workspace%`/PATH
      resolution, JSON protocol, timeout, per-URI cache, error texts) for the
      remote cache and the downloader.
- [x] HTTP remote cache (`/ac/`, `/cas/`, ActionResult, upload/accept flags,
      headers, timeout) with Bazel's exact REAPI action keys; spawns aligned
      with Bazel (genrule-setup.sh, strict PATH, run_shell padding, argv[0]).
- [x] Divergence D1 closed (`k8-<mode>` output roots), needed for shared keys.
- [x] Paired capture: entries shared both ways (8/8), identical AC/CAS
      contents and cold-build requests, exact helper error texts; receipt
      `tests/fixtures/remote_cache/reference_receipt.json`. gRPC caches and
      remote execution remain explicit failures (docs/REMOTE_CACHE.md).

## Remaining compatibility gates

### Scalar string dictionaries (isolated branch verified)

- [x] Add ordered scalar `attr.string_dict` values across declaration, binding,
      snapshots, frozen projections and real rule/repository/tag consumers.
- [x] Verify 1,183 Rust/700 Python tests, 140 comparison phases/369 commands and
      five targeted regression matrices with independent bounded-profile review.

Safe loaded-BUILD constructor behavior intentionally differs from a reference
crash. Configurable/select/macro and repository Unicode boundaries remain explicit;
combined host-metadata integration is a separate verification gate.

### Callback host metadata (isolated branch verified)

- [x] Provide immutable Linux/x86_64 callback metadata with freeze-stable identity,
      explicit unsupported environment access, and retained extension host flags.
- [x] Verify selected extension factors and unconditional versioned repository
      host fingerprints, including the intentional one-time publication rekey.
- [x] Verify 1,181 Rust/720 Python tests, 52 phases/124 commands, native factor
      proofs and four repository/environment regression matrices.

This branch still requires combined integration verification with scalar dictionary
attributes. No runtime host detection, emulation or full platform parity is claimed.

### Repository environment dependencies

- [x] Observe declared and dynamic repository environment inputs lazily, with
      real getenv/file/consumer behavior and keyed publication identities.
- [x] Preserve empty-profile identities and method bindings while enforcing
      redacted diagnostics, bounded observation caches and explicit exclusions.
- [x] Verify synthetic-variable reference/cache/privacy scenarios and regressions
      before claiming this repository execution profile.

Verified 1,149 Rust/671 Python tests, 67 comparison phases/353 commands and
sixteen adjacent suites. The isolated application query advances to missing
attr.string_dict; no ambient-environment or application-success equivalence is claimed.

### Native JSON codec

- [x] Implement four JSON methods with exact integer/finite-float handling,
      caller-owned decoding, typed fields and token-preserving indentation.
- [x] Verify real provider/repository action outputs, 1,024 float cases and
      bounded resource rejection before scans, sorting or integer conversion.
- [x] Complete 1,104 Rust/645 Python tests, 82 comparison phases/127 commands,
      fourteen adjacent suites and independent review.

Nested diagnostic context, builtin NativeInfo and external File roots remain
outside this profile. The preserved application advances to nonempty repository
environment dependencies, not a successful application build/test.

### Scoped subrule execution

- [x] Execute real zero-attribute subrules and declared nested calls with frozen
      ownership, current-context guards and actual action/provider outputs.
- [x] Preserve method-specific Args factory lifetime and document controlled
      rejection of expired factories instead of reproducing reference crashes.
- [x] Verify real CLI/reference/cache/source-change behavior and adjacent context
      and build-setting regressions before claiming the bounded integration.

Verified 1,057 Rust/609 Python tests, 99 comparison phases/162 commands and
thirteen adjacent suites. Default Args-file serialization is supported without
claiming deferred mapper or configurable parameter-file parity. The preserved
application advances to missing json.

Private attribute dependencies, configured toolchains/fragments and their
visibility/default semantics remain a separate increment, not ignored inputs.

### Starlark set execution

- [x] Preserve the exact pinned interpreter as a licensed, provenance-verified
      vendor baseline; verify unchanged dependency resolution and native behavior.
- [x] Repair real set methods, constructor metadata and assignment-specific
      mutation semantics without changing ordinary binary operations.
- [x] Enable genuine sets in BUILD/MODULE/.bzl and retain generated File
      dependencies through provider-contained sets; verify actual action outputs.

Production integration verified 1,011 Rust/572 Python tests, 62 comparison phases,
136 CLI commands and twelve adjacent suites. Invalid/unexported Target membership
now rejects correctly. The unchanged application advances to missing subrule.

Each stage is separately verified and committed. Default-enabled core set support
does not establish general-value formatting, disabled-flag or full Bazel parity.

### Run-environment metadata

- [x] Pin constructor, snapshot, structural equality and direct/nested return
      contracts against Bazel 9.3; approve the bounded metadata scope.
- [x] Implement genuine native values, nested transport, explicit consumers and
      ordinary top-level return rejection without run/test placeholders.
- [x] Complete the bounded 96-phase native/reference matrix, 923 Rust/519 Python
      tests, ten adjacent suites, application/analyzer probes and cleanup.
- [x] Obtain unconditional bounded-profile final review; retain the separate
      root-MODULE mismatch rather than claiming the original matrix passed.

See `docs/RUN_ENVIRONMENT_INFO.md`; automatic execution environment handling and
executable/test rule support remain outside this increment.

### Root MODULE validation

- [x] Evaluate root MODULE for local CLI query/build before BUILD loading or
      actions; preserve low-level repository laziness and unused dependency
      laziness, and reuse the recorded root when graph resolution is demanded.
- [x] Reproduce and close the retained RunEnvironmentInfo local-query phase
      mismatch with genuine-reference, caching, no-MODULE and regression checks.

Verified 952 Rust/533 Python tests, 29-phase root matrix, 98-phase metadata
matrix, ten adjacent suites and unconditional review. Original failed evidence
remains unchanged; application still stops at missing set.

### Native package specifications

- [x] Pin real group/provider, constraint and visibility contracts against Bazel
      9.3; approve owned graph and implementation boundaries.
- [x] Implement real package-group production, membership consumers, required
      providers and group visibility, including source-root validation.
- [x] Verify native/reference cases and update the previous unsupported-group
      macro boundary to genuine supported semantics.
- [x] Complete 897 Rust / 490 Python tests, both native/reference matrices,
      seven adjacent suites, application probe, analyzer and cleanup.
- [x] Obtain unconditional final review of the bounded package-specification profile.

See `docs/PACKAGE_SPECIFICATIONS.md`; reference contracts alone do not establish
native success or full parity.

### Ownership-aware symbolic macros

- [x] Pin constructor, export, visibility, output-name and implicit-source
      contracts against Bazel 9.3, then approve the bounded architecture.
- [x] Implement real no-user-attrs expansion, ownership and source eligibility.
- [x] Verify native/custom actions, private-access denials, namespace timing,
      collisions, frozen callbacks and adjacent ordinary behavior.
- [x] Complete eight reference QA families / 108 phases, 838 Rust / 469 Python
      tests, seven adjacent suites, release, analyzer, application probe and cleanup.
- [x] Obtain unconditional final review of the bounded macro profile.

This increment does not substitute a loader-only `macro` global or claim complete
symbolic-macro support. See `docs/SYMBOLIC_MACROS.md` for exact scope.

### Dynamic extension introspection

- [x] Implement active-extension-only `existing_rule` / `existing_rules`, strict
      signatures, frozen empty results, and rejection in other evaluation contexts.
- [x] Preserve genuine `maybe()` duplicate errors and execute the full unchanged
      upstream extension; pass six QA families and 70 phases with cleanup.
- [x] Pass 794 Rust tests, 446 Python tests, six adjacent suites, strict Clippy,
      release build and analyzer diagnostics on one frozen candidate.
- [x] Obtain unconditional final review of the bounded extension-call-path profile.

The application now reaches a missing `macro` global in generated globals code.
Symbolic macros require real declaration ownership and visibility semantics;
no constructor placeholder or full application success is claimed.

### Local metadata and MODULE-native version

- [x] Retain local definition metadata in direct/ordinary provenance and preserve
      existing callback-before-publication behavior and guards.
- [x] Isolate MODULE-native version bindings from BUILD-loaded `.bzl` and test
      shared-repository-set loading, ownership and unchanged warm-run execution.
- [x] Pass 788 Rust tests, 422 Python tests, strict Clippy and release build.
- [x] Pass six genuine helper/lifecycle QA families and 39 phases, preserving
      every original archive file and checking complete generated outputs.
- [x] Finish all six adjacent receipts and unconditional final review.

The preserved application now reaches missing `native.existing_rule` in genuine
`maybe()` code. Full upstream-extension execution remains unproven.

### Ordered dictionary attributes

- [x] Implement typed ordered values across custom rules, direct repositories,
      ordinary extension calls and tags, with contract-correct frozen ownership.
- [x] Preserve MODULE-end versus extension-call snapshot timing and normalize
      list/tuple attribute values consistently.
- [x] Reproduce and fix aggregate direct-default amplification before callbacks;
      pass 781 Rust tests, 408 Python tests, strict Clippy and release build.
- [x] Complete eight dictionary QA families and 146 phases plus six adjacent suites.
- [x] Obtain unconditional final review of this bounded profile.

The preserved application advances to `repository_rule(local=True)` without
checkout changes. Local lifecycle metadata and MODULE-only native version
bindings remain a separate increment; full application builds/tests are unproven.

### Bounded global PAX comments

- [x] Verify unchanged genuine archive inventory against raw headers, Python and
      Bazel's pinned Commons Compress parser before implementation.
- [x] Stream strictly framed inert comments while retaining archive path, type,
      checksum, byte/count limits and dangling-header checks.
- [x] Compare all 41 genuine files and seven directories, including licenses and
      upstream BUILD/MODULE content, without filtering or repackaging.
- [x] Pass 751 Rust tests, 388 Python tests, strict Clippy and release build.
- [x] Pass ten CLI cases, the preserved application probe, three adjacent
      suites and unconditional final review of the bounded archive profile.

Semantic global PAX fields, local PAX, GNU extensions and full extraction metadata
parity remain unsupported; this increment does not enable them silently.

### Default-only build settings

- [x] Preserve owned setting descriptors and per-invocation typed defaults.
- [x] Execute genuine skylib 1.8.2 implementations for all nine exported settings.
- [x] Match reference-confirmed fresh mutable value and provider-map getter copies.
- [x] Produce native TemplateVariableInfo and consume it through real dependencies.
- [x] Verify native default-change cache behavior without accepting CLI overrides.
- [x] Pass 741 Rust tests, 388 Python tests, strict linting, build and analyzer checks.
- [x] Preserve the application checkout and advance loading to the archive-source
      `docs_url` boundary, without claiming an application build or test success.
- [x] Pass nine validation families (55 probes, 104 engine phases), five adjacent
      CLI suites and unconditional review of the bounded default-only profile.

CLI setting overrides, configuration matching/transitions, implicit Make-variable
consumption and platform/toolchain resolution remain separate unimplemented gates.

### Genuine builtin transition

- [x] Package all 421 official Bazel 9.3 Linux x86-64 embedded-tools files with
      reproducible bytes, trusted manifest, provenance and licenses.
- [x] Add an explicit native profile with implicit `bazel_tools` edges before
      discovery/selection, key-based identity, shared ordering and normal closure.
- [x] Preserve MODULE-override metadata independently of canonical builtin
      filesystem precedence; implement native checked lazy tree publication.
- [x] Match two official 28-module selections from captured genuine registry
      metadata, including the older explicit platforms request upgrade.
- [x] Query/build through an unmodified builtin helper; provide the real Starlark
      `print` global required when loading that helper.
- [x] Correct the genuinely observed action-output permission mismatch: all
      completed regular outputs now normalize to `0555`, with v5 cache checks.
- [x] Pass 725 Rust tests, 360 Python tests, strict Clippy and release compilation.
- [x] Pass two genuine builtin CLI comparisons and six native boundary/cache
      checks, including a standalone executable process trace and cleanup.
- [x] Retry the unchanged application: now fails on genuine missing
      `platform_common` in `bazel_skylib`'s `common_settings.bzl`, not a fabricated
      builtin repository. Scratch removed and required Git link preserved.
- [x] Complete builtin/adjacent CLI reports, final-delta checks, Python suite and
      unconditional review of the bounded profile and action-permission fix.

This is an opt-in transition, not complete default builtin/registry/lockfile or
application compatibility. Default migration and functional build settings,
platform providers, SDKs and toolchains remain required work.

The bounded ordinary-extension increment adds real non-isolated callbacks and
typed immutable module/tag views. It snapshots repository arguments at call time,
binds their labels after complete sibling discovery and validates every generated
spec/import before fetching a demanded repository. Private/imported requested
symbols and cross-phase cycles are handled without fabricated repositories.
Host-side expansion and provenance limits must reject default amplification before
allocation; evaluator budgets alone are insufficient. Full extension APIs, SDKs,
metadata/facts and actual application compatibility remain separate work.

The ordinary increment is verified by 695 Rust tests, 306 Python tests, strict
Clippy, formatting, release compilation and clean Rust Analyzer diagnostics.
Eight genuine Bazel comparisons, eight native lifecycle cases, fourteen resource
control/limit checks and all eight adjacent suites pass. Final review approved
the bounded profile; the unchanged application still requires genuine builtin
module closure before further SDK/analysis compatibility can be established.

## Repository publication reuse increment (N61)

- [x] Hash each repository's specification BEFORE execution (rule identity,
      attributes, mappings, sources, host markers, declared environment values)
      and address a publication pointer per specification under
      `repositories/direct/specs/`.
- [x] Reuse verified publications on warm invocations without executing the
      repository rule; keep the content-addressed tree identity formula
      unchanged so existing identities stay stable.
- [x] Refetch exactly like Bazel: changed attributes, declared environment
      values, host markers, template contents, watched files/directories and
      observed environment values re-execute and republish.
- [x] Record watchable-input probes in manifests (names and digests only;
      environment values are never persisted), revalidated on warm calls.

- [ ] Complete Bzlmod/repository compatibility beyond local selection/loading:
      complete downloader/archive/Git semantics, full patch semantics, lockfiles, built-in closure,
      module extensions and repository rules using pinned fixtures.
- [ ] Configurations, toolchains and providers sufficient for actual repository
      Go/Python/Node rule analysis, followed by safe executable/test targets.

Captured differential artifacts record 39 custom-rule and 24 loading checks;
module JSON validation is separate from the 20 repository differential checks.
The previous local increment passed 296 Rust tests and 124 Python tests; the
remote increment's updated counts are recorded above.
This still does not mean full application builds/tests work.
