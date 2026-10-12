# Local loading and parallel execution milestone

This milestone extends the native engine; it does **not** implement full Bazel
9.3 parity or enable the main application's Go/Python/Node build and test suite.

## Added behavior

- Local `.bzl` imports, aliases, frozen modules, ordinary function macros, private
  import checks, explicit re-exports, load-order checks and load-cycle detection.
- A real `native` struct inside `.bzl`; BUILD and extension globals are distinct.
- Source globs with inclusions, exclusions, `*`/`**`, dotfile rules, directory
  filtering and package boundaries; Bazel 9.3's false `allow_empty` default.
- Package default visibility, including defaults declared after rules; explicit
  source exports; public/private/package/subpackage visibility enforcement.
- Bounded parallel action DAG scheduling through `build --jobs=N`, preserving
  input/output declaration order, shared dependencies and content-cache behavior.

Key distinctions verified against pinned upstream source: BUILD may have late
loads, ordinary `.bzl` files may not; imported `.bzl` names are not automatically
re-exported; `struct` is not a predeclared BUILD global; exported source files
default to public, while implicit sources inherit package defaults under Bazel
9.3's default configuration.

## Reproducible checks

```sh
TMPDIR=/tmp/opencode ./scripts/check.sh
python3 scripts/validate_loading.py --timeout 90
```

The differential suite compares a macro-defined six-action diamond with a shared
dependency and independent actions. Separate cold caches exercise `--jobs=1` and
`--jobs=4`; unchanged rebuilds must report all six cache hits and identical bytes.
It also compares loading/glob/visibility failures by semantic category, without
claiming identical diagnostic text or exit-code numbers.

Repository checks are restricted to six source-only targets:

- `//src/beyondwork.ai/bworker/config:config_files`
- `//src/k8s/crossplane:files`
- `//docs:runbook-temporal-encryption-key.md`
- `//thirdparty:build_stack_rules_proto/proto_compile_bazel9.patch`
- `//bin:bw-devenv` — source resolution only, never script execution
- `//src/beyondwork.ai/fleettrust/examples:tailscale_pilot`

## Verified results (2026-10-12)

- **115 Rust tests** passed (88 unit and 27 CLI integration tests).
- **43 Python harness tests** passed.
- Formatting, strict Clippy and optimized release build passed.
- **24 differential checks passed** against official Bazel 9.3.0, including all
  six repository targets. The YAML filegroups contained 7 and 109 files.
- Cold native diamond builds executed six actions with either jobs setting;
  unchanged rebuilds executed zero actions with six validated cache hits.
- Both engines agree on the added regressions: trailing `/**` does not turn a
  regular file into a directory; recursive exclusions remove matching dotfiles;
  source files do not conflict with generated directories; cross-package
  generated output-prefix conflicts fail; `$(locations ...)` sorts paths.
- Repository status and staged/unstaged diff hashes stayed unchanged. Private
  reference servers shut down and temporary build trees were removed.

Raw evidence: `artifacts/loading-validation.json`.
The original foundation differential suite also passed again, including
`//src/docker:config`; current evidence is `artifacts/validation-current.json`.
Its 20 grouped tiny-fixture query samples measured native median **4.78 ms**
(p95 **9.39 ms**) versus warm reference median **25.11 ms** (p95 **37.80 ms**).
These exploratory timings are not whole-repository performance evidence, and
the first reference invocation includes installation/server/module work.
Native binary SHA256:
`fcbdc615e27586fa1f795a590ef3bb8dd25bd4f0732c692ef61a144c99df9c79`.
Repository unstaged diff SHA256 before/after:
`3755abe6f8dcab5332d2feb50a321fa72a1d872cbc2d1f7b079e2669098a13d3`.

Source contents are compared by hashes, not stored in the report. The harness
records Git status and staged/unstaged diff hashes before and after; it never
reverts changes. Official Bazel uses private output roots, rc opt-out, disabled
remote services, no lockfile updates and no convenience symlink writes.

## Limits

Local macros are not custom `rule(...)` implementations or symbolic macros.
Bzlmod/external loads, providers, toolchains, configured-target analysis, remote
execution, process timeouts and actual Bazel test execution remain missing.
Globs and artifacts deliberately do not follow symlinks. License metadata,
package groups and other package defaults are rejected rather than ignored.
Modules are cached only within each package evaluation.

Scheduler tests establish bounded concurrency and dependency/failure guarantees;
passing a tiny diamond does not establish real-repository build speedups.
The earlier timing table in `VALIDATION.md` is a foundation-milestone snapshot,
not a benchmark of this expanded implementation.
