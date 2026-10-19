# Parallel analysis — P0 baseline and P1 record

Program: oracle design review `ses_eb8f16924ffeNtzHiI8k1sLAME` (2026-10-19),
`/tmp/opencode/parallel-design-review.md`, implemented VERBATIM on branch
`parity-parallel-analysis` (base `feb566f`). P0 and P1 landed; P2/P3 follow
in a later session after the parent reviews P1.

## P0 — baseline (`BAZEL_RS_TIMING` phase marks)

`BAZEL_RS_TIMING=1` prints `PERF <elapsed>s rss=<kB> <mark>` on stderr.
Marks: `main.start`, `select.start`/`select.end` (pattern expansion and,
for query, the whole evaluation), `plan.start`, `plan.phase1_analysis_done`,
`plan.phase2_actions_done` (implementations and action planning, including
top-level aspects), `plan.end`, `execute.end`.
`BAZEL_RS_DUMP_ACTIONS=1` (hidden, review §5) dumps every planned action's
full identity — id, argv/program/write, inputs and generated-input
destinations, outputs, environment, execution info — in plan order.

Workspace `~/code` (rules_go/rules_js/rules_cc bzlmod graph, 18,489
targets), release build, `--experimental-builtin-modules
--lockfile_mode=off`, private cache, warm (repositories materialized,
actions all cached; second run measured). 64 cores.

### Warm query `//src/... + //bin/... + //docs/... + //platforms/... + //thirdparty/...`

Top-level `//...` is not loadable here (`venv/` ships BUILD files using
`py_library`, outside the supported rule set; `--deleted_packages` is not
implemented) — the union above is everything else.

| span | seconds |
|---|---|
| main.start → select.start (module resolution, lockfile warm) | 0.18 |
| select.start → select.end (package loading + query evaluation) | **115.8** |
| peak RSS | 1.34 GB |

### Warm build, all cache hits (60 genrule targets + dependencies; 384 actions, 0 executed)

| span | seconds | share of wall |
|---|---|---|
| main.start → select.start | 0.004 | — |
| select span (pattern loading) | 114.98 | 49.8% |
| plan.start → phase1_analysis_done (configured-target analysis) | 112.23 | 48.6% |
| phase1 → phase2_actions_done (**rule implementations**) | **1.10** | **0.5%** |
| phase2 → plan.end (planning tail) | 0.48 | 0.2% |
| plan.end → execute.end (384 cache hits) | 2.20 | 1.0% |
| total wall | 230.99 | — |
| peak RSS | 1.95 GB | — |

(Phase-1 analysis includes loading of dependency packages the patterns did
not cover, so parallel loading — P2 — attacks both large spans.)

### Go/no-go (review §1.1)

- **P2 (parallel package loading): GO.** Loading spans dominate both the
  warm query (≈100% of wall) and the warm build (≈50% wall + most of
  phase 1's span).
- **P3 (parallel rule implementations): NO-GO on this workload.**
  Phase-2 implementation time is 1.10 s of the 113.8 s analysis wall
  (plan.start → plan.end) — **0.97%**, not a meaningful share (review
  §1.1). Revisit only with a Starlark-implementation-heavy workspace.

## P1 — pure refactor, no threads (GATE: byte-identical)

- `plan_target` split into `prepare_target` / `Engine::compute_target`
  (owned `Send` snapshot, asserted by `compute_boundary_types_are_send`)
  / `commit_target` (→ `commit_custom_target`, `commit_native_target`).
  Coordinator-only operations stay coordinator-only (§1.7): N56
  `planned_validation`, N58 `plan_aspect_dependency`, the repositories and
  provider inserts, `restricted_to_error` and `check_transition_allowlist`
  in the commit; the NEEDS_TARGET_PLATFORM retry runs inline at the commit
  slot; implementation prints emit from the commit (§1.8).
- Loading runs as commit-ordered units: BUILD prints buffered, `.bzl`
  top-level prints buffered in the shared cache entry (failures included)
  and replayed once per module in the package's load-DFS order; the
  emitted-`.bzl` set lives on the coordinator (§1.4). Both loaders are
  covered (query loads through `Engine::load_package`, §1.5).
- Single-flight `.bzl` evaluation: one global reentrant lock (owner thread
  + depth), taken only on cache misses, with a cache re-check after
  acquiring (§1.3).
- No-demand guard (§1.2): `DemandSlot::get_or_try_init` and the
  module-repository materialization slots return the `NEEDS_REPOSITORY`
  sentinel instead of fetching; the coordinator re-runs aborted units
  inline at their commit slot — package loads and analysis compute both.
  The guard is inactive on the coordinator in P1;
  `BAZEL_RS_LOADING_NO_DEMAND` (hidden test hook) exercises the fallback
  end to end.

### P1 gate evidence (byte-identical to `feb566f`)

- Full `cargo test --locked` (1,316 lib tests incl. the new ordering
  tests + every integration suite with captured expected outputs): PASS.
- Python validator suite (`python3 -m unittest discover`, 881 tests): OK.
- `fmt`/`clippy -D warnings`: clean.
- `~/code` warm A/B against the pristine `feb566f` binary — stdout,
  stderr, exit codes and cache-hit counts all byte-identical:
  - query (union pattern, 1,268,139 B stdout), `kind(genrule, //src/...)`,
    `query --output build //src/k8s/...`: identical, exit 0/0;
  - 60-target warm build: stdout identical (each line carries the
    action's content-addressed cache path, i.e. its action key), stderr
    identical, `0 actions executed, 384 cache hits` on both sides.
- `BAZEL_RS_DUMP_ACTIONS` (added for §5) is only in the new binary; the
  P2/P3 thread-count A/B compares it between builds of the same code.

### Notes for P2 (from this session)

- Workers get explicit ≥8 MiB stacks (§1.6), the pool is bounded by
  `--loading_phase_threads`, and commits follow
  `validate_output_directory -> generated inserts -> packages.insert`
  in `select_filtered` order — `commit_loaded_package` already implements
  that order.
- The loading retry's print rules (§1.2 + §1.4 interaction): a
  guard-aborted unit's `.bzl` module lines are emitted once (the modules are
  cached and never evaluate again) while its BUILD lines are dropped with
  the aborted evaluation; the inline re-run reproduces the sequential
  output, fetch output included.

## P2 — parallel package loading (`--loading_phase_threads`)

Both loaders' pattern expansion (`Engine::expand_pattern` — the engine's
`select_filtered` and the query environment's `resolve_pattern`) loads a
`//dir/...` pattern's packages on a scoped pool of
`--loading_phase_threads` workers (LoadingPhaseThreadsOption.java:
"auto" — the default — expands to the available processors; positive
integers accepted for `build`/`run`/`test` and `query`). Workers get
16 MiB stacks (§1.6), run in no-demand mode (an unfetched repository
aborts the unit with the sentinel; the coordinator re-runs it inline at
its commit slot — verified end to end by the cold-and-warm local-registry
stress test), and evaluate with their own Starlark
Module/heap/Evaluator, buffered prints and fresh thread-locals. The
coordinator commits in pattern order
(`validate_output_directory -> generated inserts -> packages.insert`,
print replay included, `.bzl` load-DFS order), so the first error in
that order wins and later results are discarded; threads=1 is exactly
the P1 sequential path.

The single-flight `.bzl` lock is now held per cache miss (§1.3 read
literally): around each module's evaluation and publication in
`Work::Finish`, with a re-check under the lock — a load that finds the
label published by a concurrent traversal reuses it (one identity, one
print record). Parsing needs no guard (it creates no identity). A
worker's sentinel failure is never published to the shared cache (the
inline retry must evaluate the module for real).

`BAZEL_RS_LOADING_JITTER=<millis>` (hidden test hook, §5) injects
reproducible per-package worker sleeps. Unconditional-dependency
prefetch (review §3 P2 optional item) is not implemented — noted below.

### Determinism evidence (§5)

- Fixture A/B (tests/parallel_loading.rs): 12-package workspace with a
  shared printing `.bzl` graph — warm builds at threads 1/2/8/32 and
  threads=8 + 40 ms jitter: stdout, stderr and exit byte-identical;
  prints once per module in load-DFS order, implementation prints in
  plan order. Two failing siblings: the first error in pattern order
  wins with identical bytes at every thread count. A 110-frame Starlark
  call chain (no recursion — this profile rejects it) on workers:
  identical. Query output identical at 1/2/8/32.
- `~/code` warm query (18,489 targets): threads 1 vs 8 vs 32 — stdout
  byte-identical (1,269,639 B), stderr identical modulo `PERF` timing
  lines, exit 0; threads=8 + 25 ms jitter: identical.
- `~/code` warm build (60 targets, all cached): stdout byte-identical
  at 1/8/32; the `BAZEL_RS_DUMP_ACTIONS` action-graph dump (384
  actions: ids, argv, inputs, outputs, env, execution info) identical
  after normalizing the per-run repository staging paths
  (`cache/repositories/direct/trees/<run hash>`, a pre-existing
  staging-path nondeterminism that only this debug dump ever exposed;
  the exec paths, keys and outputs are identical). One transient
  action-cache miss (383/384 hits, one action re-executed to the same
  key) was observed once in a threads=1 run — not thread-correlated.
- Full `cargo test --locked` (102 suites incl. the new
  tests/parallel_loading.rs), 881 python validators, fmt/clippy clean.

### Perf (`~/code`, warm, 64 cores; wall seconds)

| workload | threads=1 | threads=8 | threads=32 |
|---|---|---|---|
| warm query (union pattern) | 134.8 | 139.9 | 133.0 |
| — `//src/beyondwork.ai/...` (390 light pkgs) | 10.9 | 10.0 | 10.0 |
| warm build (60 targets) | 242.3 | 240.2 | 249.5 |

**The expected drop did not materialize — the loading wall is not
package breadth.** A single `//src/ts` package loads in 61.2 s and a
single `//src/python` package in 26.2 s: their cost is evaluating the
shared `.bzl` graphs (rules_ts/nodejs, rules_python, rules_go), which
the single-flight lock correctly serializes (one identity per label)
and whose DFS evaluation is inherently sequential per graph. Parallel
package loading wins only the per-package remainder (visible on the
390-light-package slice: 10.9 → 10.0 s, and 22 ms → 8 ms on a synthetic
12-package probe). The next meaningful lever for `~/code`-style
workspaces is **parallel (or invocation-cached) `.bzl` graph
evaluation** — Bazel amortizes this with its resident server — not
broader package parallelism.

### Residual risks / notes

- Repository-graph resolution on a worker (lazy `resolve_graph` after
  startup) is not under the no-demand guard; warm invocations resolve
  before pattern expansion (startup + eager lockfile resolution), and
  every guarded demand path (demand slots, module-repository
  materialization slots) returns the sentinel. A cold, lazily-resolving
  workspace could fetch from a worker through an unguarded `OnceLock`;
  not observed on `~/code` (startup resolves eagerly).
- Unconditional-dependency prefetch (review §3 P2, optional): skipped —
  the measured bottleneck makes it low-value now; the no-demand guard
  makes a future prefetch safe by construction.
- One transient action-cache miss observed once (above); pre-existing
  flake profile, not thread-correlated.

## P2 follow-up (review round 2)

Four fixes landed with this round:

1. **Print replay on failed loads (blocker).** The loader now records a
   marker for EVERY shared-cache hit — module successes AND failures,
   including the failure hits that abort a load and the Finish-time
   recheck hits — each in DFS position, meaning "replay that module's
   stored prints once". The coordinator's replay is one event path
   (`emit_load_prints`) for success, failure and inline-rerun commits
   alike. Without this, a FAILED package lost the prints of `.bzl`
   modules it had taken from the cache (reproduced: `//...` with a
   failing `bad.bzl` at threads=8 lost `bad evaluates` when a sibling's
   worker evaluated it first; fixed, marker pushed on failure hits).
2. **Lazy resolution closed.** `load_packages_parallel` forces
   `resolve_graph()` on the coordinator before spawning workers — a
   worker can no longer fetch through an unguarded resolution path.
3. **Prerequisite order restored.** `starlark_prerequisite_errors` runs
   in PREPARE again (its pre-split position, before the compute), so a
   speculative implementation can never fetch in a build that
   sequentially stopped at the prerequisite error.
4. **Commit interleaving.** Each package's `package_targets` runs right
   after its commit — inside `load_packages_parallel`'s commit loop —
   exactly like the sequential path, so warnings and first-error
   positions cannot move.

### trees/<run-hash> staging variance: pre-existing, and a real perf bug

Two threads=1 runs of the same warm build produce different
`cache/repositories/direct/trees/<64-hex>` paths for the same logical
inputs (`seq-1` vs `seq-2` dumps; the A/B normalization scope is
exactly `trees/[0-9a-f]{64}`): **the variance is per-run, not
per-thread-count.** Cause: `repository_store::materialize` executes the
repository rule into a private stage *before* computing the identity,
and the identity (provenance + serialized runtime inputs + snapshot)
varies per run for ~54 of the direct repositories — so **every warm
build re-executes and re-publishes ~54 repository trees** (measured:
+54 tree dirs per warm 60-target build; the shared cache accumulated
15,254 tree dirs / 46 GB over today's session). This also inflates every
warm wall time recorded in these tables (the re-materialization runs
inside loading/extension demand) and explains the input real-path churn
in the action dump. Fixing the volatile provenance component (or
skipping execution when a manifest exists for the same definition) is
**N62 candidate #1** — it masquerades as `.bzl` load cost today.

### PERF peak RSS by thread count (~/code, warm)

| workload | t=1 | t=8 | t=32 |
|---|---|---|---|
| query (union pattern) | 1,229 MB | 1,240 MB | 1,242 MB |
| build (60 targets) | 1,918 MB | 1,919 MB | 1,925 MB |

Parallel loading adds <1.5% peak RSS (the in-flight `LoadUnit`s are the
packages themselves).

### N62 groundwork (not implemented; measured baselines only)

- **Genuine Bazel 9.3.0, cold, `--batch --ignore_all_rc_files query
  --lockfile_mode=off //src/ts:all`** (scratch private
  `--output_user_root`, removed after): **20.2 s wall**, 8,341 targets
  — against bazel-rs's 61.2 s to load the same package. That is the N62
  gap (≈3×), measured cold including the JVM.
- **Per-module self-time + BUILD-macro breakdown** of the 61.2 s
  `//src/ts` package: not yet instrumented. The breakdown must separate
  (a) direct-repository re-materialization (above — suspected to be a
  large share), (b) `.bzl` module evaluation by label, (c) BUILD/macro
  evaluation and globs. Add per-module timing in `Loader::finish_module`
  under `BAZEL_RS_TIMING` when picked up.
- **Reviewer's point on the global `.bzl` lock**: the single global
  lock is a simplicity choice — it serializes evaluations of
  *independent* modules. The real N62 lever is **per-label single-flight
  with in-flight waiting** (a waiter for a label that is itself being
  evaluated aborts to the inline path on a cycle), which lets disjoint
  modules of one graph evaluate in parallel — *before* any
  invocation-persistent `.bzl` cache. Bazel's resident server amortizes
  exactly this; bazel-rs as a per-invocation client cannot without one
  of the two.

## P2 follow-up 2 (review round 3)

- **Cached failures replay their dependencies' prints.** `CachedFailure`
  now stores the module's `loads` (evaluation failures happen after the
  loads ran). A failure hit schedules those loads and a deferred
  `FailCached` item, so the dependencies' hit markers precede the
  failing module's marker in postorder — the first loading package
  replays D's lines, F's partial lines and the error, byte-identical to
  the sequential DFS, whichever worker evaluated them (stress row:
  `cached_failure_replays_dependency_prints_in_postorder`). The
  Finish-time recheck failure keeps its single marker: that traversal's
  own DFS already visited the loads.
- **Sentinel aborts suppress partial prints.** A module aborted
  mid-evaluation by the no-demand sentinel no longer pushes its partial
  lines to the unit sink — the early emit would mark the module emitted
  and suppress the inline re-run's full lines ("a" kept, "b" lost).
  Stress row: `sentinel_aborted_module_prints_fully_on_the_inline_rerun`
  (module prints, parses a label of an unfetched repository, prints
  again; cold parallel run emits both lines exactly once via the inline
  re-run).
- A/B rerun with the final binary: fixture suite (10 rows incl. all
  three failure shapes under jitter) green; `~/code` warm query stdout
  md5-identical at threads 1/8/32 and under jitter with stderr identical
  modulo PERF lines; warm build 384/384 cache hits at every thread
  count, stdout identical, full 384-action dump identical after
  `trees/<run-hash>` normalization. Full cargo test (102 suites), 881
  python validators, fmt/clippy clean.
