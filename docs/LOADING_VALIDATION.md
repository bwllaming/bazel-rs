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
- Each BUILD-phase `.bzl` is read, parsed, evaluated and frozen once per
  invocation (Bazel's BzlLoadValue): the repository set owns a lockable cache
  of successful evaluations that every loading package shares, so a top-level
  `print` appears once, as in Bazel. Each package still traverses its load
  graph in the same order with the same cycle, depth (256) and per-requester
  `visibility()` checks, and gets the same transitive top-level function
  registry. A module's own failure (reading, parsing, its loads' visibility,
  evaluation, freezing) is stored like Bazel's BzlLoadValue error: the module
  is evaluated, and its prints emitted, once, and the same error is replayed
  for every package (or dependent module) that loads it. Load-cycle and
  depth-limit errors name the traversal path and are never stored. The
  BUILD-file and BUILD-phase `.bzl` globals are built once per invocation.
  Repository and extension definitions keep their own per-load evaluation
  (`load_definition_in`).
- Identical `glob()` calls of one package evaluation (same includes,
  excludes, `exclude_directories`, `allow_empty` and link policy) walk the
  file system once (GlobCache).

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

## Performance (N44, 2026-10-17)

Release binaries, 64 cores, warm caches unless noted; before = d2a689d.

| case | before | after |
|---|---|---|
| N36 CLI bench (`artifacts/staging-bench.py`: 50 genrules over a 2000-file glob, 500 MiB, `--jobs=8`) cold / warm | 72.6 s / 69.3 s, 119 MB | 4.9 s / 2.3 s, 122 MB |
| 160 packages loading one 21-file `.bzl` graph: `query //...` | 7.8 s, 3.35 GB | 0.15 s, 59 MB |
| same, `build //...` warm | 9.0 s, 3.45 GB | 0.92 s, 127 MB |

The analysis/planning time was a per-call scan of every rule's `srcs` to
prove a source file is declared (now a per-package index plus a memo of
resolved source paths), per-edge source analysis (now once per label), a
quadratic `source_paths` dedupe, per-package re-evaluation of every loaded
`.bzl` (now shared, above) and deep `Rule` copies per lookup (now
`Arc<Rule>`, one `top_level_functions` registry per package). Every memo is
a lookup-only hash table, never iterated: two processes produce
byte-identical output (`tests/loading.rs`
`repeated_processes_produce_byte_identical_outputs`).

## Limits

Local macros are not custom `rule(...)` implementations or symbolic macros.
Bzlmod/external loads, providers, toolchains, configured-target analysis, remote
execution, process timeouts and actual Bazel test execution remain missing.
Globs, `.bzl` loads and source artifacts follow symlinks under the shared
link policy (docs/GLOB_SYMLINKS.md, docs/ARCHIVE_SYMLINKS.md). License metadata,
package groups and other package defaults are rejected rather than ignored.
Modules are cached only within each package evaluation.

Scheduler tests establish bounded concurrency and dependency/failure guarantees;
passing a tiny diamond does not establish real-repository build speedups.
The earlier timing table in `VALIDATION.md` is a foundation-milestone snapshot,
not a benchmark of this expanded implementation.
