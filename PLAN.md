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

## Remaining compatibility gates

- [ ] Complete Bzlmod/repository compatibility beyond local selection/loading:
      verified HTTP/Git/archive retrieval, patches, lockfiles, built-in closure,
      module extensions and repository rules using pinned fixtures.
- [ ] Configurations, toolchains and providers sufficient for actual repository
      Go/Python/Node rule analysis, followed by safe executable/test targets.

Captured differential artifacts record 39 custom-rule and 24 loading checks;
module JSON validation is separate from the 20 repository differential checks.
Full checks pass 296 Rust tests and 124 Python tests; rust-analyzer diagnostics,
formatting, strict Clippy and release compilation pass.
This still does not mean full application builds/tests work.
