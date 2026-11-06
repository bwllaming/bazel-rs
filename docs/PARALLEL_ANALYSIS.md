# Parallel analysis — P0 baseline and P1 record

Program: oracle design review `ses_eb8f16924ffeNtzHiI8k1sLAME` (2026-10-19),
`/tmp/opencode/parallel-design-review.md`, implemented VERBATIM on branch
`parity-parallel-analysis` (base `feb566f`). P0 and P1 landed; P2/P3 follow
in a later session after the parent reviews P1.

## P0 — baseline (`BAZEL_RS_TIMING` phase marks)

`BAZEL_RS_TIMING=1` prints `PERF <elapsed>s rss=<kB> <mark>` on stderr
(since W0-A followed by ` hwm=<kB>kB`, the peak RSS so far; see
docs/PERF_LAYER1.md).
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
  slot; implementation prints emit from the commit (§1.8). The platform
  tree that retry analyzes is appended to the plan being walked and
  planned right after the target (`Engine::deferred_target_platform`).
  It used to go to a discarded plan: a later phase-1 edge to one of its
  memoized nodes (an alias to a constraint value) then ended at a target
  phase 2 never planned (docs/AFFECTED_TARGETS.md §8.6).
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

## N62 — per-label `.bzl` single-flight (independent modules in parallel)

The global evaluation lock (review §1.3 as implemented in P1/P2) is
replaced by a per-label flight registry (`BzlFlight` in
`src/loading.rs`): the first traversal to reach a cache miss claims the
label and evaluates; a concurrent traversal of the SAME label waits on
the claim's condvar and then rechecks the shared cache — exactly the
Finish-time recheck the global lock performed — so one identity and one
print record exist per label. Disjoint labels evaluate in parallel up
to the loading workers' width. A claim is released on drop (including
panics and sentinel aborts).

Claims are never nested in the current architecture (a module's loads
are all evaluated before its own `Work::Finish` frame), so the wait
graph cannot cycle. As a safety net, a wait that WOULD close a cycle
across threads — or a same-thread re-claim — aborts the unit with the
`PARALLEL_BZL_CYCLE` sentinel (`src/repository_demand.rs`) instead of
deadlocking; the coordinator treats it exactly like the no-demand
sentinel (`is_inline_rerun_sentinel`): module events already emitted
stay emitted, BUILD lines are dropped, and the unit re-runs inline at
its commit slot, where a genuine load cycle reports with the sequential
traversal's message (identical bytes) and a spurious abort merely
re-loads from the cache. Sentinel errors are never published to the
shared `.bzl` cache (`Loader::failed`).

### Breakdown (cb1b3eb, `BAZEL_RS_TIMING_MODULES=1`)

Post-N61, `//src/ts:all` warm select span is ~122 s of which repository
`root()` materialization (npm `__links` local repositories re-executed
by design at ~70 ms each, serialized) is ~121 s; `.bzl` evaluation self
time is ~0.5 s and BUILD evaluation ~0.25 s. The `.bzl` share the
global lock serialized never existed on this workspace; N62 is still
the correct Bazel-equivalent structure (BzlLoadFunction evaluates
disjoint loads concurrently), and it removes the serialization for
workspaces whose graphs are evaluation-heavy.

### Perf (`~/code`, warm, release, 64 cores; wall seconds, this session)

| workload | t=1 | t=8 | t=32 | t=8 jitter 40 ms |
|---|---|---|---|---|
| query `//src/ts:all` (563,900 B) | 122.4 | 122.9 | 125.6 | — |
| query `//src/...` (1,280,427 B) | 291.4 | 181.4 | 170.1 | 170.7 |
| build 60 genrules (`beyondwork.ai`) | fails* | fails* | fails* | fails* |

`//src/ts` is one package: no package breadth, root()-bound — unchanged
by design. `//src/...` keeps the P2 package-parallel win (post-N61
baseline in cb1b3eb: t1 284.9 / t8 161.0; this session's t8 181 s is
within run-to-run root()-materialization variance, and stdout is
byte-identical). *The 60-genrule warm build currently fails identically
at every thread count with `invalid direct repository path or
path/depth limit exceeded` during analysis (a direct-repository
snapshot rejecting a path containing a forbidden character, in an
unrelated subsystem; observed identically at t=1/8/32 and under
jitter, so thread-invariant). The action-graph A/B therefore runs on
the synthetic fixture below, and the `~/code` query A/B above.

### Determinism evidence (§5)

- Fixture A/B (tests/parallel_loading.rs, 13 rows): the previous 10
  rows unchanged and green, plus (a) `parallel_bzl_graph_shapes_*`: a
  diamond (`left`/`right` both load printing `base`) and a wide fan-in
  (ten modules loading printing `fan`) reached from 11 packages in
  shuffled roots — byte-identical stdout/stderr at 1/2/8/32 and jitter
  40/25 ms, each print exactly once; (b)
  `cross_package_load_cycle_*`: a load cycle (a↔b) reached from two
  packages with different roots — the first package in pattern order
  reports `load cycle` naming both modules exactly once, byte-identical
  at 1/2/8/32 and jitter; (c) `action_dump_*`: warm fixture built at
  1/8/32 + jitter with `BAZEL_RS_DUMP_ACTIONS=1` — the full action dump
  byte-identical after `trees/<run-hash>` normalization, stdout/stderr
  (outside the dump) identical.
- Unit rows (src/loading.rs): `independent_bzl_labels_evaluate_concurrently`
  (two labels rendezvous inside Starlark evaluation — impossible under
  the global lock), `bzl_evaluation_is_single_flight_and_reentrant`
  (rewritten: loser waits and rechecks), and
  `wait_cycle_abort_sentinel_prevents_cross_thread_deadlock` (same-thread
  re-claim and a constructed cross-thread wait cycle both abort with
  the sentinel; unique labels because the registry is process-global
  and lib tests run concurrently).
- `~/code` warm query: `//src/ts:all` md5 `4f4e7201…` and `//src/...`
  md5 `84b70775…` identical at t=1/8/32 and under 40 ms jitter; stderr
  identical modulo PERF lines. Warm build of the 60-genrule set fails
  identically (above) — its query form (`kind(genrule,
  //src/beyondwork.ai/...)`, 60 targets) is md5-identical at 1/8/32 +
  jitter.
- Gates: `cargo test --locked` 1,335 lib tests + 103 integration
  suites (2,182 tests) pass; two intermittent failures are pre-existing
  and unrelated (see Residual). Python validators: 882/883 pass; the
  one failure (`test_validate_label_passthrough` manifest-provenance
  pin) fails identically on HEAD cb1b3eb — a stale source-hash pin from
  the N61 era. `fmt`/`clippy -D warnings` clean.

### go_deps cold variance: diagnosed, named residual (no fix)

Question: do cold runs of a `go_deps`-demanding query vary? Three, then
seven more, independent cold runs (fresh private `--cache-dir` each) of
`deps(//src/beyondwork.ai/utils/str:str)` — which demands gazelle's
`go_deps` extension (403 generated targets in the output) — are
**byte-identical in stdout and stderr** (47,920 B, md5 `5746f65e…`)
with warm repeats in the same caches also identical and adding zero
new repository trees (warm reuse works). What DOES vary per cold run
is the content identity (tree directory name) of ~17 of the 20 direct
publications, including byte-identical trees: e.g. the Go SDK tree has
the same 17,357 files with identical content hashes in every run, yet
a different identity each time. Root cause, traced with the
`BAZEL_RS_DEBUG_PROVENANCE` diagnostic: the SDK repository's
*specification* embeds its extension's recorded file probes, whose
paths are absolute paths into the cache (Bazel's RepoRecordedInput
records the same absolute output-base paths); downstream, the
`go_host_compatible_sdk_label` repository's generated `go.env` embeds
`GOROOT=<that tree path>` and a stage path as file *content*, so the
identity chain is self-referential across different cache directories.
Within one cache directory (the supported warm workflow) identities
are stable and publications are reused (verified: 20 trees before and
after two warm runs). This is Bazel-equivalent behavior — genuine
Bazel keys `external/` by output base, and a fresh output base
re-fetches — so it is a **named residual, not a bug**: cold publication
identities are cache-path-dependent by design; user-visible output is
deterministic. (The earlier trees/<run-hash> finding for warm *shared*
caches was the N61 fix; nothing here reopens it.)

**Correction (review i20, item 4):** the recorded-probe half of this
diagnosis was wrong about Bazel. Genuine RepoRecordedInput keys a path in
an external repository as `@@NAME//path` (RepoCacheFriendlyPath,
toRepoCacheFriendlyPath — never the absolute output-base path; genuine
capture in tests/fixtures/repo_cache_friendly_paths/reference_receipt.json).
Native now records the same keys, so specifications and identities no longer
depend on the cache location through recorded inputs (docs/REPOSITORY_RULES.md).
What remains cache-dependent is file *content* that embeds absolute paths
(the `go.env` GOROOT above), as in Bazel, whose `external/` contents embed
the output base likewise.

### Diagnostics kept

`BAZEL_RS_DEBUG_PROVENANCE=1` (hidden, off by default) prints each
direct/ordinary repository's provenance bytes at materialization —
used to isolate the go_deps chain above; kept because repository
identity bugs recur and the bytes are otherwise unobservable.

### Residual risks / notes

- Pre-existing intermittent lib test
  `repository_process::tests::exit_codes_streams_signals_and_spawn_failures_match_bazel_results`
  failed 2 of 5 full-suite runs this session (a `kill -INT $$` child
  racing its own output flush; passes standalone and on HEAD, failed
  identically before N62 changes under parallel load). Unrelated
  subsystem; not addressed here.
- The stale validator pin (`repository_store_sha256`) and the
  direct-repository path-limit failure on the 60-genrule warm build are
  pre-existing and thread-invariant; both noted for their owners.
- Per-label waiting means a worker can now block on another worker's
  evaluation instead of spinning on the global lock; the 1-hour
  condvar timeout is a deadlock backstop only (a timed-out wait
  re-loops and re-checks claims, it does not fail the load).

## N62 follow-up (review round)

Four fixes: (1) **Post-claim cache recheck** — winning a flight claim
no longer licenses evaluation by itself: a publisher that inserted and
released between this thread's lookup and its claim wins, and the
claim winner rechecks and serves the cached entry instead of
evaluating a second instance whose identity its package would have
kept (the split the single-flight exists to prevent; the cache's
first-wins insert had masked it). A `#[cfg(test)]` pre-claim hook
forces the interleaving; the forcing test fails with exactly two
evaluations when the recheck is mutated out. (2) **Registry keyed by
(cache instance, label)** — `BzlCache` carries a process-unique id;
independent repository sets in one process no longer couple (the
wait-cycle unit test no longer needs unique labels). (3) **Sentinel
mapping** — a wait-cycle abort escaping a shared definition load, or
surviving the coordinator's inline re-run (`commit_load_unit`), is a
genuine re-entrant demand (an extension loading the very module being
evaluated): it reports as `load cycle: ...` and the raw
`\u{1}parallel-bzl-cycle` sentinel never reaches the user. (4)
**Re-pinned** `store_compatibility.repository_store_sha256` in the
label_passthrough fixture to the current `src/repository_store.rs`
hash (`05cc27b6…`; 9b01ce3's test-module change moved it — note the
pending safe_relative change on another branch will move it again).

A/B rerun: fixture suite 13/13 (genuine a↔b cycle, the three failure
shapes, diamond+fan-in, action dump, all at 1/8/32 + jitter); cargo
1,337 lib + 103 suites green (the known `repository_process` SIGINT
flake passed standalone); python validators 883/883; fmt/clippy clean.
### Memory-cap bound on the auto expansion (perf follow-up 3)

The "auto" expansion of `--loading_phase_threads` is bounded by the
process memory cap: `min(auto, cap_mb / 64 MB per worker heap)`,
floored at 2 (`memory_limit::bound_auto_loading_threads`), because
each worker owns a Starlark heap (16 MiB stacks are virtual-cheap
beside it). Explicit `--loading_phase_threads=N` is never rewritten.
`--jobs` (action execution) defaults to `min(CPUs, 32)` and is a
separate pool of child-spawning workers. See
docs/REPOSITORY_WARM_PERF.md "Perf follow-up 3".

Postscript (integration-19): N62 LANDED (flight registry; bzl self-time 0.5s; src mmap maps the serialized provenance that the digest-form spec replaced).

## P3 instrumentation — plan_targets stage timers (M5)

Rationale: P0 measured phase 2 at 1.10 s on the 60-genrule workload and
P3 was declared NO-GO there, but no instrument could say, for a given
build, whether phase-2 wall is prepare, compute or commit — the split
that decides whether parallelizing compute (P3) can win at all, given
the invariant that **commit stays sequential** (it owns every state
mutation and every output write, in plan order). M5 adds that
instrument: `src/plan_timings.rs`, modeled on `module_timings`
(src/loading.rs) — a process-global, append-only record list fed by
µs timers at the stage boundaries, reported once from `plan_build`
right after the outer `plan_targets` call.

Records (`PlanStageRecord { index, stage, class, elapsed_us, outermost }`):

- `prepare` / `compute` / `commit` per planned target, at the P1 split
  points inside `plan_targets` (`index` = the target's plan position,
  `class` = its interned rule class). Native targets have no `compute`.
- `commit_retry`: the compute re-runs inline in `commit_custom_target`
  (NEEDS_TARGET_PLATFORM / no-demand) — real compute, counted as such.
- `analyze`: each phase-1 `Engine::analyze` step, re-attributed to the
  plan index its target joins at; nested steps overlap their parent, so
  coverage sums only `outermost` steps (tracked by `Engine::analyze_depth`).
- `pending_flush`: one `plan_pending` flush (a `config_setting`'s
  Starlark `flag_values` key forcing its key's implementation to run
  during phase 1); `config_conditions`: one `config_conditions` call.
  Both are inside the span and their targets record their own stage
  rows, so both are report-only (never added into coverage).

Span and coverage: the span starts at the first `plan_targets` entry
(on this class of workspace most targets are planned by phase-1
flushes, not by the phase-2 call — the outer call alone spans ~0.1 s of
a ~46 s plan span) and ends when the outer call returns, i.e. exactly
`plan.phase1_analysis_done − plan.start` on the PERF marks plus the
phase-2 tail. Coverage = (prepare+compute+commit+commit_retry
+ outermost analyze − pending_flush) / span; the flush subtraction
removes the one overlap (flush wall contains its targets' stage times).

Use: `BAZEL_RS_TIMING=1` (the env that already gates `perf_mark`).
Output is stderr-only, after the standard plan summary marks, as
`PERF-STAGE` lines: per-stage totals with counts, the envelope
(`plan_span` + covered %), a per-rule-class table (top 15 by total,
with % of covered time and the prepare/compute/commit split), and the
critical path — the longest dependency chain's summed compute, walked
over the dep graph rebuilt from the plan (`rule_edges` resolved to
plan indices) with each node's label and compute time. The module
report stays `BAZEL_RS_TIMING_MODULES=1`-only; the stage report is not
printed in MODULES-only mode. With neither variable set, every hook is
one relaxed atomic load and output is byte-identical.

How to read it for a P3 decision: parallel compute can recover at most
the `compute` (+ `commit_retry`) total, and only down to the
critical-path compute; `commit` is the sequential floor, and
`prepare` is coordinator work a P3 design must also parallelize (or
accept as serial) before promising a speedup.
`--loading_phase_threads` does not affect phase 2; the 1/32 A/B below
confirms the report itself is thread-count-invariant.

### Measured: oauth2token (`~/code`, warm, `--nobuild`)

`BAZEL_RS_TIMING=1 BAZEL_RS_TIMING_MODULES=1 bazel-rs build --nobuild
--experimental-builtin-modules //src/beyondwork.ai/tools/oauth2token:all`
(18,968 planned targets, 707 Starlark computes):

| stage | seconds | count |
|---|---|---|
| prepare | 17.0 | 18,968 |
| compute | 17.0–17.6 | 707 |
| commit | 4.8 | 18,968 |
| commit_retry | 0.0 | 0 |
| analyze (all steps; outermost ≈ span − flush residual) | 185–186 | 142,273 |
| pending_flush | 38.8–39.6 | 1,237 |
| config_conditions | 1.5 | 18,976 |
| **plan_span** | **46.1–47.1** | **covered 99.9%** |

Per-class (top): `go_library` is 92.5% of covered stage time
(prepare 16.1 s / compute 16.3 s / commit 4.1 s). Critical path:
compute 0.69 s over 21 targets (… → `//:nogo_actual`, 0.11 s) — i.e.
even perfect compute parallelism is bounded by prepare+commit
(≈22 s serial) unless prepare is parallelized too; compute's 17 s
would compress toward the 0.69 s path. That is the P3 sizing question
this instrument exists to answer, per workspace.

Before/after: base `fe93cc1` vs this commit, same command — stdout and
stderr byte-identical without timing vars (54.8 s vs 56.5 s wall,
within warm-run variance); with `BAZEL_RS_TIMING=1` the new lines are
the only additions (52–53 s). `--loading_phase_threads=1` vs `=32`
with timing: stdout identical, stderr identical modulo `PERF` lines
and one pre-existing thread-dependent lockfile line (present at base
too), exit 0; stage counts identical, values run-dependent as expected.


## P3 — rule implementations on analysis workers (`src/engine/parallel.rs`)

Phase 2 (and every phase-1 `plan_pending` flush) walks the plan in its
order and **commits every target in that order on the coordinator**, as the
sequential engine does: every planning-state mutation and every console
write of a planned target happens at its turn, so the action graph, the
first error, `--keep_going`'s errors and `print()` lines are the
sequential ones by construction. What moves is *when and where* a Starlark
rule's implementation runs: `Engine::compute_target` is a pure function of
an owned `Send` snapshot (P1), and frozen Starlark values are `Sync`
(starlark-rust's `HeapSyncable` bound), so a worker evaluates the
implementation in its own heap/`Module`/`Evaluator` against the
dependencies' frozen providers.

- **Ahead of its turn.** A plan entry is *classified* once when it enters
  the horizon (4,096 entries past the turn): a compatible `RuleKind::Custom`
  rule without attribute aspects, whose every dependency prepare views
  (`Engine::custom_dependencies` — the code prepare itself runs, factored
  out — plus the resolved toolchain implementations; an output file maps to
  its generating rule) is waited for until it is committed (`dry` holds the
  key: its entries are final; delegations wake their requested keys). A
  ready entry is prepared on the coordinator (dependency views, toolchain
  contexts, ctx surface) and its compute dispatched; at most 2 × workers are
  in flight. Its turn then replays what it held and commits.
- **At its turn.** An entry not prepared ahead is prepared at its turn and
  computed inline on the coordinator (handing it to a worker only adds
  latency: the following entries mostly depend on it), while the workers
  compute the entries prepared ahead.
- **What is held** (applied at the turn, in the sequential order): console
  notes (`ui::DeferredConsole`), what the work observed for the analysis
  snapshot (`observed_fs::unit_scope` → `commit`), the repository roots it
  obtained (`UnitRoots` → `note_committed_roots`), and the incremental
  tracker's events (`Engine::track_defer_start`; replayed under the target's
  planning frame with the scope context — shared/pattern/suite depth — each
  was recorded in).
- **What work off its turn never does.** It runs in no-demand + reuse-only
  mode (an unfetched repository raises the sentinel; a per-thread counter
  catches one a caller swallowed), and analyzing a configured target,
  analyzing a source or loading a package aborts it (`Engine::ahead_abort`,
  sticky). An aborted prepare is done again at the turn (what it observed is
  applied there: a memo it filled is a hit then); a compute that raised a
  sentinel runs again inline at the commit (P1's retry). A prepare whose
  configuration's target platform a commit resolved after it (its ctx
  surface saw it deferred) is redone at the turn. The worker also computes
  the pure part of the target's runfiles support (repo mapping manifest
  content, runfiles tree mapping; `RunfilesPrecomputed`), unless that raised
  a sentinel.
- **Pool.** Up to 8 workers (16 MiB stacks; `BAZEL_RS_ANALYSIS_WORKERS`
  overrides, at most `--loading_phase_threads`), started by the first compute dispatched to them (a build where nothing is ready ahead starts none),
  stopped at the end of phase 2.
  `--loading_phase_threads=1` and `BAZEL_RS_DEBUG_NO_PARALLEL_ANALYSIS` are
  the sequential engine; `BAZEL_RS_ANALYSIS_JITTER=<ms>` (test hook) delays
  each worker compute reproducibly. `BAZEL_RS_TIMING=1` prints
  `PERF-PARALLEL workers= turn= ahead= aborted= invalidated= never= failed=
  recomputed= wait= lost_notes=`.

**Error order** is the commit order (plan order), as before: a prepare
ahead that fails keeps its error for its turn; a failing compute's error is
reported by its commit; with `--keep_going` an entry whose dependency
failed is skipped at its turn and what was held for it is dropped.

**Review fixes.**

- *Tracker attribution.* A dependency view's miss path looks targets up
  (`Engine::target` → the tracker's accesses of the innermost frame); a
  memo hit used to look nothing up, so which frame recorded the accesses
  depended on who built the view — a target prepared ahead, or an aborted
  prepare whose tracker events were dropped. The miss path's lookups are
  now kept with the memo (`Engine::view_accesses`) and every hit re-issues
  them for its frame (an intended change at threads 1 too: consumers that
  hit the memo now record what the view looked up; more accesses only make
  incremental analysis re-analyze more). `BAZEL_RS_SNAPSHOT_INDEX_DUMP=<file>`
  (hidden) writes the planned index with its timing costs cleared; tests
  compare it at threads 1/8/32 and under jitter (an alias view shared by a
  target prepared ahead and one prepared at its turn; the deferred default
  host platform with an alias to a constraint value).
- *Turn-stamped artifact index.* `ArtifactIndex` maps a File to its
  artifact and the plan index from which it is visible: a prepare ahead of
  its turn sees, and stamps, its own index; a turn stamps the turn (lowering
  an ahead stamp); outside a walk everything is visible. So a registration
  made ahead is invisible to every earlier turn, and an earlier target
  resolves exactly what it resolves sequentially (unit test
  `artifact_registrations_ahead_of_their_turn_are_visible_from_it`; Files
  reach a target only through its dependency views, so no fixture reaches a
  source File only a later target registers).
- *Stopping.* A walk that fails stops the pool: computes queued for turns
  that never come are skipped (a cancel flag checked before each starts; a
  jitter sleep checks it too), the running ones are joined. A worker's panic
  is recorded by a panic hook instead of printed and raised at the target's
  turn with its message and location; a compute the build never reaches
  prints nothing.
- Work off its turn that printed notes is a debug assertion (the tests); an
  order-dependent link decision (`repository_demand::note_order_dependent`)
  during it counts as a sentinel, and the caller's record is restored.
- `BAZEL_RS_VERIFY_AHEAD=1` prepares every target prepared ahead again at
  its turn and compares the two snapshots — rule context (ctx surface,
  toolchains, platform), every dependency view (BAZEL_RS_VERIFY_VIEWS'
  comparison), output naming, transitive repositories, execution table
  (`PERF-PARALLEL verified=`). Clean over the parallel, prefetch,
  keep_going, snapshot_prefetch, platforms, custom_rules, aspects and
  toolchains suites and over ~/code `//src/...`.
- *Global identity counters* (FilesToRun, runfiles symlinks, HeaderInfo,
  OutputGroupInfo, TemplateVariableInfo, FeatureFlagInfo): workers draw ids
  in another order, so they were audited. They serve equality and hashing
  only; `HeaderInfoItem`'s `Ord` by id is used only by `DictItem`'s sorted
  comparison, whose equality result does not depend on the order chosen;
  no id is written by the snapshot codec or decides an iteration order.

**Residuals.** Peak RSS grows with the workers' heaps and in-flight
snapshots (docs/COLD_ANALYSIS.md).

**Determinism evidence.** `tests/parallel_rule_implementations.rs`: a DAG
of 48 Starlark targets over six packages (chains, diamonds, cross-package
edges; implementations that print, burn varying CPU, write files and run
actions over their dependencies' depsets) — `build --nobuild` with the full
`BAZEL_RS_DUMP_ACTIONS` dump, an executing build, two independent failing
implementations with and without `--keep_going`, a dependency cycle — at
threads 2/8/32 and 8/32 with 25/40 ms worker jitter, and threads 8 with the
pipeline off: exit code, stdout and stderr byte-identical to threads 1; the
analysis snapshot a pipelined run records replays the sequential run's
output. On ~/code (`//src/...`, `BAZEL_RS_SNAPSHOT=off`): the action dump of
`build --nobuild` and `test --nobuild` (6,483,434 lines) and the rest of
stderr identical to main's, also at threads 1 and with 32 workers and 5 ms jitter; `aquery`
(text and summary) identical; the CI `affected` output and both digest
files identical.

## P4 — deferred action lowering and commits ahead of the turn

P3 left one thread committing every target in plan order. P4 takes the
first two slices of the per-target-delta design of the review ("each
target's prepare + implementation + commit run on workers in dependency
order, merged in plan order"): the pure tail of every commit runs on
workers, and a finished target whose commit cannot depend on the turn
order commits as soon as its compute finishes, its order-observable
effects held for its turn. Code: `src/engine/lowering.rs`,
`src/engine/parallel.rs` (`commit_ahead`), `src/ui.rs` (`HELD`).

**Deferred lowering.** An action's commit splits in two. The coordinator
keeps everything the turn order decides: output registration (producers,
the artifact index, `declared`), input resolution through the index, the
tool runfiles groups' first expansion, the shared input runs' memo, the
exec group table, the workspace status. The rest — listing the resolved
inputs, the structural and namespace checks (`validate_structure`, the
shared-group check), the source/generated split (`plan_action_sources`),
the runfiles support's manifest and tree nodes, joining a shared input run
(the Python runtime of every PyCompile) into the inputs — is a function of
what the coordinator hands over and runs on a lowering worker (4 by default,
`BAZEL_RS_LOWERING_WORKERS`, 0: on the coordinator).

- *Order.* `ActionPlanning::nodes` is a queue (`PlannedNodes`): a node the
  coordinator plans is pushed behind everything queued, a lowering in
  flight holds its slot, and entries are placed strictly in queue order
  (each walk turn places what is done; at most 2,048 entries wait).
- *Console.* While a lowering is in flight the coordinator's console
  writes are held below every `DeferredConsole` (`ui::HELD`) and cut at each
  dispatched lowering, so each block is written once everything queued
  before it is placed.
- *First error.* A failed lowering is the first error of the walk: the
  entries queued after it, the console writes held after it and the
  warnings/errors recorded since are dropped — the sequential engine never
  made them. An error a turn raises first places everything queued before
  it (a failed earlier lowering wins). Every walk drains its queue before it
  returns: nothing is in flight outside a walk.
- *Scope.* Only a pipelined walk without `--keep_going` defers (a
  keep-going failure must skip its dependents at its own turn); work ahead
  of its turn lowers inline; `--loading_phase_threads=1` and
  `BAZEL_RS_DEBUG_NO_PARALLEL_ANALYSIS` are the sequential engine.
- *Test hook.* `BAZEL_RS_LOWERING_FAIL=<action id>,…` fails chosen
  lowerings wherever they run (registration checks most of what lowering
  checks, so a lowering failure is otherwise hard to provoke).

**Commits ahead of the turn.** A worker reports each finished compute
(`AnalysisPool::finished`); at every turn, and while the turn waits for its
own compute, the coordinator commits finished entries ahead of their turn
(`Engine::commit_ahead`) when their commit cannot depend on the order:

- *Outputs settled* (`Pipeline::unsettled`): no classified, uncommitted
  earlier entry of the same package (exact output paths: shared actions and
  conflicts), of an enclosing or an enclosed package (prefix conflicts), and
  no earlier uncommitted Starlark rule that is never prepared ahead (an
  aspect barrier: aspect applications register outputs in other packages).
- *Outputs in the package* (`outputs_in_package`, review item 1): every
  output the commit registers (its actions', its executable and runfiles
  manifests) lies below `<output root>/<package exec path>/`. A shareable
  output named from another package's path (a C++ link's `_solib_<cpu>/`
  symlink, `declare_shareable_artifact`) or a main-repository root package
  output under `external/` (the output tree of `@r//p`) is outside the
  package rule's reach: such a target commits at its turn, where the first
  owner in plan order registers it.
- *Platforms known* (review item 2): its prepare saw its target platform
  resolved (one that saw it deferred is prepared again at its turn when a
  commit resolves it in between, so it commits at its turn), and each
  spawn's execution platform is the default host platform, carries its
  resolved properties or is already in `Engine::platforms`
  (`remote_platforms_known`; `Engine::remote_platform` reads that memo).
- *Bounded*: the segments waiting for their turn hold at most 16,384 nodes
  and lowerings (`BAZEL_RS_COMMITTED_LIMIT`), besides the 4,096-entry
  horizon.
- *Safe commit* (`commit_ahead_unsafe` finds nothing): the compute succeeded without a
  sentinel; no transition allowlist (it loads its package), no workspace
  status needed (registered on first need), the runfiles support's pure part
  computed by the worker, no repo mapping manifest or build-info action left
  to resolve (the worker now resolves `create_repo_mapping_manifest`'s
  content too), every tool runfiles group and shared input run its actions
  use already memoized.
- *Held.* It commits like a prepare ahead: `self.ahead` (an analysis or
  package load would abort — an internal error, as would a repository
  demand under the no-demand guard), its registrations stamped with its plan
  index (`ArtifactIndex::stamp_ahead`; the internal error names
  `BAZEL_RS_NO_COMMIT_AHEAD=1` as the workaround), its console, observations and
  obtained roots in an `Isolation`, its tracker events deferred, the
  warnings/errors it records split off, its nodes and lowerings in a segment
  (`PlannedNodes::segment`) with its console cut at each lowering. Its turn
  replays the prepare's, compute's and commit's held effects in that order
  and splices the segment into the queue (`PlannedNodes::splice`); a commit
  that failed ahead returns its error there.
- *Dependents.* What dependents read (dry files, providers, runfiles,
  transitive repositories, artifacts) is final when it commits, so they are
  classified ready and prepared ahead earlier; an earlier turn never reads
  it (plan order is post-order; registrations are stamped).
- `BAZEL_RS_NO_COMMIT_AHEAD` turns it off; `BAZEL_RS_VERIFY_AHEAD=1`
  repeats the prepare right before the commit ahead and again at its turn,
  comparing both with the one committed (`verify_prepared`, P3's
  comparison; `PERF-PARALLEL verified_turn=`, and `platform_late=` counts
  entries committed ahead whose target platform got resolved before their
  turn — never); `BAZEL_RS_DUMP_ACTIONS=remote` adds each spawn's remote
  platform to the dump; `BAZEL_RS_TEST_ABORT_AHEAD=1` (tests) makes every
  commit ahead end in that internal error; `BAZEL_RS_TIMING=1` adds
  `PERF-PARALLEL committed_ahead= later_barrier= later_package= ineligible=
  turn_compute= walks(...)`, the reasons outputs were unsettled and the rule
  classes computed at their turn.

**Determinism evidence.** `tests/parallel_rule_implementations.rs` gains
`lowering_failures_are_reported_at_their_turn` (Starlark and runfiles
manifest/tree lowerings failing at worker speed, with and without
`--keep_going`), `commits_ahead_of_the_turn_keep_the_sequential_graph`
(independent packages with subpackages, genrules beside Starlark rules, a
shared action pair, an aspect-applying rule in the middle; the full action
dump) and `conflicts_committed_ahead_are_reported_at_their_turn`; after
the review `shareable_outputs_outside_the_package_commit_at_their_turn`
(two packages declaring one `_solib_k8/` artifact, shared and
conflicting: fails without the fix), `remote_platforms_resolved_late_commit_at_their_turn`
(a guard: the explicit host platform there is resolved before phase 2),
`verify_ahead_checks_commits_ahead_at_their_turn`,
`an_aborted_commit_ahead_names_the_workaround` and
`committed_segments_are_bounded`; all at
threads 2/8/32, with 25/40 ms jitter and VERIFY_AHEAD, against threads 1.
On ~/code (`80d6724f3a`, `BAZEL_RS_SNAPSHOT=off`): the action dump of
`build --nobuild` and `test --nobuild //src/...` (6,483,434 lines) and the
rest of stderr identical to main's, also at threads 1 and at 32 threads with
5 ms jitter; `BAZEL_RS_VERIFY_AHEAD=1` clean (32,281 verified); `aquery`
text and summary identical; the CI `affected` output and digests identical,
and on the P1 (`470886f0 → 7ab6d35d`), P2 (`e7b42879 → 9e6b547e`) and
`2c7b638 → 3f5b7908` pairs; a snapshot the pipelined run records verifies
(`BAZEL_RS_SNAPSHOT=verify`) against the full analysis at threads 1 and 8.

**Not yet** (docs/COLD_ANALYSIS.md, "What remains"): the commit itself and
the prepare still run on the coordinator; an exact-path conflict or shared
action with an uncommitted earlier entry is avoided by waiting, not decided
at the merge.

## Plan B — commits ahead decided by known outputs

P4's settled rule held a finished entry behind every earlier uncommitted
entry of a related package. Plan B's first part (docs/COLD_ANALYSIS.md,
"Plan B, first stages") narrows it to the entries whose outputs are not
known yet:

- A worker's finished compute reports, with its plan index, the output
  paths its commit registers (`OutputSummary`: the actions' outputs and the
  runfiles support's executable, repo mapping and runfiles manifests and
  tree, hashed with their proper ancestors — a superset; a hash collision
  only makes two entries look related). It is not reported when the compute
  raised a sentinel (it runs again inline) or panicked; it counts only for
  an entry prepared with its target platform resolved (never prepared
  again at its turn). A native rule other than genrule, genquery and
  starlark_doc_extract registers no output: known from its classification.
- `Pipeline::unsettled` then holds an entry behind an aspect barrier, an
  earlier entry of a related package whose outputs are unknown (as before),
  or an earlier uncommitted entry with known outputs that meet its own (an
  exact path, an ancestor of one or a path below one): `PERF-PARALLEL
  unsettled ... outputs=`. The conflicting or sharing pair still commits in
  plan order, so the shared action's owner and the conflict message are the
  sequential ones.
- `BAZEL_RS_VERIFY_AHEAD=1` records the paths every Starlark commit (ahead
  or at its turn) registers (`register_output`) and fails when one is not in
  the summary of its compute (`verified_outputs=`).
- The worker also checks the declared outputs' source-tree parents
  (`validate_declared_outputs`, the commit's first check after the
  providers) inside its isolation; the commit uses that result (debug
  builds compare it with the commit's own check).

## Phase-1 prefetch (W6, `src/prefetch.rs`)

P2 parallelized only the packages a target pattern names. On a warm
build most packages are loaded *during* phase-1 analysis — the registered
toolchain packages (oauth2token: 151 packages, 2.4 s of 3.3 s of loading,
the 15,185-file `@zig_config` BUILD alone 0.76–0.89 s) and the
dependency packages of the requested targets — one at a time, on the
coordinator. W6 loads them ahead of demand on a background pool while the
coordinator analyzes.

**Pool.** `Engine::plan_build` starts a `Prefetcher` for phase 1 when
`--loading_phase_threads` > 1 (at most 16 workers; 16 MiB stacks), the
module graph is already resolved, and the repository set is not pinned
(the replay of local repository rules; `RepositorySet::prefetch_allowed`);
the `BAZEL_RS_LOADING_NO_DEMAND` test hook and `BAZEL_RS_DEBUG_NO_PREFETCH`
switch it off. Since W6R it also runs while the analysis snapshot records
(what a worker observes reaches the recording with its unit's commit, a
memo hit or a counting root: docs/ANALYSIS_SNAPSHOT_RECORDING.md,
"Recording with phase-1 prefetch"); `BAZEL_RS_SNAPSHOT_PREFETCH=0` keeps
it off while recording. It stops when phase 1 ends, however it ends: workers finish
the unit they are loading, every unit nobody demanded is dropped
unreported. `--loading_phase_threads=1` is exactly the sequential engine.

**Workers** are P2 workers: `package::evaluate_load_unit` under the
no-demand + reuse-only guards (a repository that needs fetching, a module
extension group that is not prepared, a module root only the coordinator
materializes, a local repository rule: the unit aborts with the
`NEEDS_REPOSITORY` sentinel), their own Starlark heaps, print events
buffered in the unit, `.bzl` single-flight through the shared cache (N62).
`BAZEL_RS_LOADING_JITTER` delays each unit as for P2.

**Commit at the demand point.** Nothing a worker produces is reported.
`Engine::load_package` takes the package's unit from the prefetcher
(waiting if a worker is loading it; loading it itself if no worker started
it) and commits it through `commit_load_unit` — the sequential path's
commit, at the sequential point: BUILD lines, `.bzl` lines and hit markers
replayed in the unit's DFS order, `validate_output_directory`, generated
inserts, `packages.insert`, the error of a failed unit. A sentinel unit is
re-run inline there (P2). So the commit order, and every print, error and
warning a unit carries, is the sequential engine's; a speculative unit that
fails (a select() branch not taken, the implementation of a toolchain
resolution never selects) never surfaces.

A worker's work does not stay inside its unit, though: the units it loads —
also the ones the build never commits — publish shared state the
coordinator reads later. Each such reader keeps one invariant, so that what
it computes and prints does not depend on whether, or when, a worker got
there first:

- **The shared BUILD-phase `.bzl` cache** (`loading::BzlCache`; review F1 of
  d707764). Every loader that reads or publishes it is a BUILD-phase loader
  (`Loader::shared_build_phase`): BUILD-phase globals, the analysis char
  model, and print events into a sink — the lines of the modules it
  evaluated and a hit marker for every module the cache served. The
  coordinator replays each sink at its sequential point
  (`Engine::emit_load_prints`), so a module's lines print once, at the first
  commit or aspect load that reaches the module, read from the cache entry
  when a worker evaluated it. The coordinator's readers are the package load
  units and the aspect definition loads (`Engine::aspect_definition`:
  `--aspects`, `attr(aspects = ...)`). The aspect load used to be a definition
  load over the same cache: the Latin-1 char model, lines printed straight to
  stderr, nothing replayed on a hit. When a worker had published the module
  for a package the build never commits (`//tools`, reached only through a
  select() branch not taken), the aspect's top-level print was lost. The
  module's strings depended on who evaluated it first: with `NAME = "é"`,
  `len(NAME)` was 2 and the aspect's action wrote `c3 83 c2 a9` at threads 1,
  but `len(NAME)` was 1 and the action wrote `c3 a9` at threads 8.
  Genuine Bazel 9.3.0 prints the line once (`len(NAME) = 2`: its strings are
  bytes, the analysis model's documented `len` divergence, STRING_METHODS.md)
  and writes `c3 a9`. Now every thread count prints the line once with
  `len(NAME) = 1` and writes `c3 a9` — an intended change at threads 1,
  whose action bytes now match genuine's (`tests/phase1_prefetch.rs`,
  `aspect_bzl_prints_once_in_the_build_char_model_whoever_loads_it_first`;
  genuine receipt `tests/fixtures/phase1_prefetch/aspect_reference_receipt.json`).
- **Recorded-input keys** (`RepositorySet::friendly_repository`; review F2
  of d707764). A repository rule or module extension (coordinator-only
  work) records a host path it reads or watches as `@@NAME//path` when the
  path lies under the materialized root of NAME. Otherwise it records the
  absolute path; for a path in the repository store, update mode then
  cannot write the extension's lockfile entry. A worker's warm-reuse probe
  publishes the root of a repository the sequential engine may not have
  demanded yet, so the key depended on how far the workers had got: an
  extension reading a file of a warm `@data` before the build demanded
  `@data` failed at threads 1 and recorded `@@+data_repo+data//file.txt`
  at threads 8. Keys now use the roots as the sequential engine sees them
  (`RepositorySet::sequential_roots`, `DemandSlot::sequential`). The
  invariant: at every point of the coordinator, a direct or
  extension-generated root counts exactly when the sequential engine (one
  loading thread) has obtained it by that point. That is when
  - the coordinator demanded it (outside no-demand mode; a filled slot
    included);
  - the coordinator committed a load unit, or ran a load inline, that
    obtained it: the package's root and the root of every `.bzl` module
    the load reached. A worker records them in its unit (`LoadUnit::roots`,
    `repository::UnitRoots`), noted at the commit; the coordinator notes
    them at once, so an aborted unit's inline re-run counts a cached
    module's root before the module extensions its later loads run;
  - the root that a worker-fillable, invocation-wide memo demands on its
    miss path counts at a hit (`RepositorySet::note_obtained`: at once on
    the coordinator, in a worker's unit from its commit), whoever filled
    the memo: the sequential engine's lookup there is the miss that
    demands it, or a hit after one. There are two such memos:
    - the shared BUILD-phase `.bzl` cache — a module's root
      (`Loader::source_path`, at the module's entry). A worker may have
      evaluated the module first, for a unit the build never commits or
      one that aborted;
    - the ignored subdirectories (`RepositorySet::ignored_subdirectories`,
      IgnoredSubdirectoriesValue) — the repository's root, where REPO.bazel
      and `.bazelignore` are read. A worker computes them when it loads
      any package of the repository; target-pattern expansion (a `/...`
      toolchain registration during phase 1, a command-line pattern) asks
      for them before it demands the root, and never demands the root of
      a pattern they filter out;
  - transitively, the warm-reuse probe that published a root that counts
    obtained it while checking that root's recorded inputs: the probe runs
    in its own `UnitRoots` scope and its roots are stored with the
    publication (`DemandSlot::get_or_probe_obtaining`,
    `DemandSlot::obtained`), and they count wherever the published root
    begins to count (`DirectRuntime::mark_sequential`: the coordinator's
    demand of the filled slot, a commit, a memo hit), down a chain of
    nested probes. The coordinator's evaluation of that root runs the same
    check and demands the same roots.

  Registry, Git and built-in roots are only ever materialized outside
  no-demand mode. So a key is the sequential engine's whatever the phase-1
  prefetch and pattern-loading workers published, provided the files a
  probe checks do not change during the invocation. d3f2582 counted only
  the demands: a root reached through another root's check, or through a
  `.bzl` module a worker had evaluated first (for a unit the build never
  commits, or one that aborted and re-ran inline on a cache hit), never
  counted, and the extension failed at threads 2/8/32 where threads 1
  recorded `@@NAME//file.txt` (review F2 follow-up). 6474766 still missed
  the ignored-subdirectories hit (review FU1 of 6474766): with
  `register_toolchains('@data//ign/...', '@from_ext//:all')`, `ign`
  ignored by `@data`'s REPO.bazel and a worker loading `@data//x` while
  the coordinator analyzed, the expansion of `@data//ign/...` found the
  memo filled, warned and skipped the pattern without obtaining `@data`,
  and `@from_ext`'s extension failed the same way. The link policy keeps
  every published root: they are store trees, which anchor links in any
  order anyway (`tests/phase1_prefetch.rs`,
  `recorded_input_keys_name_only_roots_the_coordinator_has`,
  `recorded_input_keys_count_every_root_the_sequential_engine_obtains`;
  `repository::tests::a_probed_root_counts_with_the_roots_its_probe_obtained`:
  a chain `@a` → `@b` → `@c` of recorded reads, `@a` direct or
  extension-generated, counted by a demand, a commit, a `.bzl` cache hit
  and an ignored-subdirectories hit).

  The audit behind "two such memos" (FU1): every other memo shared by the
  threads of one invocation either demands no root on its miss path, or a
  worker cannot fill it. (The analysis snapshot's observations follow the
  same paths since W6R, plus the memos whose miss observes files or the
  environment without demanding a root: the extension repositories' spec
  keys carry their observations, the environment snapshot records at
  every lookup, the canonical-path memo's forms are recorded once at the
  end of planning, the recorded-input digests and the published-tree
  verifications record on every answer anyway.)
  - Filled by workers, no root on the miss path: a prepared group's
    source-digest entries and an extension group's spec digests, which a
    warm-reuse probe computes (in-memory sources; host and environment);
    the path-to-value memos — canonical paths
    (`RepositoryState::link_canonical`), the link policy's published-tree
    verifications (`verified_trees`) and the recorded-input file digests
    (`repository_ctx`'s `FILE_DIGESTS`; a probe resolves `@@NAME//path`
    through `root()` on every check, before the digest); the catalog
    (metadata only); the host and repository-environment snapshots; the
    immutable globals. The root demand slots themselves are the obtains
    above (the demand of a filled slot is its hit).
  - Load units: a prefetch or P2 worker's unit commits at the demand
    point with the roots it obtained (`note_committed_roots`); an aborted
    unit re-runs inline. A unit the build never demands never counts.
  - No memo at all: repository mappings (read from the catalog or a
    prepared group), REPO.bazel's `repo()` defaults (`repo_args` evaluates
    REPO.bazel, demanding the root, for every package), globs.
  - Never filled by a worker: the module graph (resolved before any
    worker starts), registry, Git and built-in roots (`guarded_root_slot`
    returns the sentinel in no-demand mode), prepared groups (likewise
    `DemandSlot::get_or_try_init`), and the definition `.bzl` cache: its
    hits skip a module's root too, but only definition loads fill it —
    group preparation, extension evaluation and lockfile-entry reuse, all
    inside a prepared-group slot — so its misses and hits run on the
    coordinator, in the sequential order.
  - Per thread or per load, never shared: the label memos of one package
    evaluation (`label::CANONICAL_MEMO`, `CONVERTED_MEMO`), a loader's own
    module map.

**What is queued** (target-directed, like a loading-phase transitive
closure; hints only — a wrong guess costs a wasted load, never output):

- the registered toolchain patterns' packages, in registration order, as
  soon as `compute_registered_toolchains` knows them (`:all` follows every
  non-test rule, an explicit label its rule; `/...` patterns stay
  sequential: discovery needs the root). The pattern labels resolve under
  the no-demand guard, so a hint never prepares a repository;
- the labels of every rule the coordinator analyzes for the first time
  (`analyze_key` → `Engine::prefetch_rule`) and of the requested targets;
- the implementation of every registered toolchain compatible with the
  build's target platform and its host execution platform, as soon as the
  toolchain is analyzed during registration
  (`Engine::prefetch_compatible_toolchain`, `toolchains::compatible`:
  single_resolution's constraint checks; target settings aside) — the Go
  SDK package (0.2 s) loads during registration instead of when the first
  Go rule analyzes its toolchain;
- the implementations toolchain resolution selects (`resolve_toolchains`):
  they are analyzed after the rule's attribute dependencies;
- recursively, up to 12 hops: a finished (or committed) package follows
  the labels of the targets wanted of it — native label attributes and
  select() keys/branches (`NativeTarget::input_labels`), a Starlark rule's
  label attributes, built-ins and select() labels, `package_metadata`
  (NODEP attributes such as a `toolchain`'s implementation are not
  followed: loading every registered implementation would probe
  repositories no build of this platform needs).

Lower depth first; a package reached again at a smaller depth moves up.

**Retry.** A sentinel unit remembers the demand it was blocked on (the
`prepare:ordinary:<group>` / `repository:<name>` / module-root text of the
sentinel). After every inline load, `Prefetcher::retry` queues again the
aborted units whose demand `RepositorySet::sentinel_ready` now finds
satisfied (it only reads ready slots): once the coordinator prepared the
`go_deps` extension group, every go_deps package loads on the workers.

**Two guards made the coordinator/worker overlap safe.** P2 workers only
ever ran while the coordinator waited; prefetch workers run beside it.

- (a) `DemandSlot::get_or_probe` publishes a worker's warm-reuse probe only
  under the evaluator claim lock and only while no evaluator runs: the
  coordinator evaluating (fetching) the same slot can never find its
  result slot occupied (`internal repository demand error`); the probe is
  a miss instead. Unit tests drive each interleaving with release
  channels: a probe that hits while the evaluator runs (started before or
  during the evaluation) misses and the evaluator publishes; a probe that
  hits after the evaluator published serves its value
  (`a_probe_hit_during_an_evaluation_is_a_miss_and_the_evaluator_publishes`,
  `a_probe_overlapping_an_evaluation_never_publishes_over_it`).
- (b) A link decision that depends on which repositories the invocation has
  materialized so far — a glob or a `.bzl` load following a symlink into
  another live repository root (the snapshot's `CrossRepositorySymlink`
  taint), or a link the policy refuses — is never taken on a worker
  (`repository_demand::order_dependent`, `note_order_dependent`): the unit
  aborts with the sentinel and the coordinator decides at the sequential
  point. (This also closes the same order dependence for P2 workers.)
  Tested end to end: a worker's load of a package whose glob, or whose
  `.bzl` load, crosses into a local_path_override module outside the
  workspace aborts as order-dependent while the coordinator's inline load
  follows the link (`prefetch::tests::worker_links_into_another_live_root_abort_as_order_dependent`),
  and the build is byte-identical across thread counts
  (`tests/phase1_prefetch.rs`,
  `links_into_another_live_root_are_the_coordinators_decisions`). A `.bzl`
  link to a file of a module nested inside the main repository's tree
  stays a link within the main repository (the loader resolves it in its
  own tree, in any order); the glob walker anchors it at the nested root
  and aborts, which is conservative.

**One warm probe per repository.** Workers fan out over the packages of
one repository at once, and each used to run the slot's warm-reuse probe
(manifest, recorded inputs, stat walk of the tree) itself; on ~/code at 16
workers that verified 2.3–2.6x the files of the sequential run (oauth2token
114k vs 48k files, B2 219k vs 84k; 3–7 s of worker time). A top-level
probe now claims the slot (`DemandSlot::get_or_probe`); another top-level
prober waits for it and is served by its publication, or misses with it.
A probe nested in another probe (a recorded input in another repository)
never waits — it probes on its own as before, so probe waits cannot form
a cycle — and a wait is capped at 1 s (`PROBE_WAIT`; probes only read).
A probe that reaches a slot its own thread is already probing (the
recorded inputs of two repositories naming each other, possible across
runs) is a miss instead of a recursion that overflowed the stack (review
F4; `a_probe_reentering_a_slot_it_is_probing_is_a_miss`).
The coordinator never probes (it evaluates). Verification volume is the
sequential run's again (oauth2token 48.4k files at 16 workers, B2 84.3k).
The unit tests hold every probe on a release channel instead of a sleep
and bound each waiter's wait per thread, so no outcome races the 1 s
bound: a waiter is served by the in-flight probe's publication or misses
with it, a wait past its bound probes on its own, and a nested probe never
waits (`repository_demand.rs`).

**Telemetry.** With `BAZEL_RS_TIMING=1`, `PERF-PREFETCH workers= loaded=
aborted= retried= taken= waited= wait= missed= unused=` at the end of
phase 1 (`wait`: the coordinator's total wait for units a worker was still
loading).

**Determinism evidence.** `tests/phase1_prefetch.rs`: toolchain packages
(`:all` and explicit labels), a dependency chain with BUILD and `.bzl`
prints, a select() branch not taken and an unselected toolchain
implementation that both fail to load, the first analysis error among two
failing chains, registry-module toolchain packages that need the
sentinel retry (cold, then warm), and a top-level aspect whose `.bzl` a
worker evaluates first for a package reached only through a select()
branch not taken (with and without the build committing that package
later), and a module extension that reads a file of a repository a
worker probed warm — before and after the build demands that repository,
and after the build obtains it through another repository's warm check,
through a `.bzl` module a worker evaluated first, and through an aborted
unit's inline re-run that reads such a module from the cache —
byte-identical stdout/stderr/exit (and lockfile, a fresh one per run) at
threads 1/2/8/32 and 8/32 with jitter; P2's `tests/parallel_loading.rs`
unchanged.

### Definition `.bzl` modules evaluated once per invocation

What the workers cannot take over is coordinator-only: preparing a module
extension group (the extension's `.bzl` graph, the lockfile entry, the
repository rules of its `generatedRepoSpecs`), materializing module roots,
re-running local repository rules. Group preparation was the largest part
of it (oauth2token: 21 groups, 1.07 s on the coordinator), and most of
that was the same `.bzl` files evaluated again: every group preparation
and every repository rule definition load ran a fresh loader over its
whole graph (`@bazel_tools//tools/build_defs/repo:utils.bzl` 52 times,
`rules_rust`'s `known_shas.bzl` 6 times; 1,607 evaluations of 703
labels).

`crate::loading::DefinitionBzlCache` (one per repository set) keeps every
successfully evaluated definition module (module-phase globals, Latin-1
char model, provenance capture): the frozen module, its functions and
loads, its source provenance, the mapping records of its load statements
and of its top-level evaluation, and its top-level print lines. The
definition loads of the extension machinery
(`load_shared_definition_module_in`: direct groups, ordinary groups,
lockfile repository rule definitions) reuse it: a reused module replays,
at the same DFS positions, exactly what a fresh loader records and prints
— its source and load-statement records when it is entered, its loads,
then its evaluation's records, functions and print lines — so the
`LoadedDefinition` (identities, spec keys, lockfile inputs) and stderr are
the uncached loader's. Failures are not cached (the next load evaluates
again, as before). The cache is off while root/mapping results are pinned
(a replaying set) and with the `BAZEL_RS_DEBUG_NO_DEFINITION_CACHE`
switch; while they are captured for the analysis snapshot's replay
records it keeps each module's root/mapping lookups and repeats them at a
hit inside a replay scope (W6R). `tests/definition_bzl_cache.rs` uses the
switch as its A/B reference (two extensions
of one `.bzl`, a repository rule shared by both and by a
`use_repo_rule`, top-level prints, cold and warm lockfile). oauth2token:
881 evaluations instead of 1,607, peak RSS −150 MB.

## Parallel repository fetching (`src/repository_speculation.rs`)

A cold build executed every repository rule at its sequential demand, one
after another: on ~/code a cold `build --nobuild //src/python:bworker_env`
spent ~310 s of its ~350–410 s fetching and extracting 284 pip wheel
repositories one at a time. Genuine Bazel fetches independent repositories
concurrently (Skyframe evaluates the `RepositoryDirectoryValue`s analysis
and loading demand in parallel, up to `--loading_phase_threads`; downloads
are bounded by `--http_max_parallel_downloads`, default 8,
CommonCommandOptions.java). The native engine now does too, in the
scheduling layer only — who executes which repository when — while what it
shows stays the sequential engine's.

**Who.** A pool of at most 32 fetch workers (also at most
`--loading_phase_threads`; 16 MiB stacks; 8 before perf-cold-ext) lives exactly as
long as the phase-1 prefetcher (`--loading_phase_threads=1` runs no pool
and is the sequential engine; `BAZEL_RS_DEBUG_NO_SPECULATION` switches the
pool off alone; never for a pinned set).

**What.** A prefetch unit aborted by the no-demand sentinel on a direct or
extension-generated repository (`runtime N:repository:NAME`) asks for that
repository — but only when the build *certainly* analyzes one of the
unit's targets (`prefetch::Reach`): the coordinator analyzes it (or the
requested targets), toolchain resolution selected it, or a certainly
analyzed target names it outside select() or in the select() branch the
coordinator's analysis of the conditions decides (`Prefetcher::note_condition`
records each condition's match result; a condition matching in one
configuration and not in another, or in error, decides nothing; a
select() waiting for an unanalyzed condition is expanded again once the
coordinator analyzed it). Registered toolchain patterns, compatible
toolchains and branches not (yet) decided are loaded as before but never
fetched ahead: a wrong package-load guess is cheap, a download is not (the
first cut fetched every select() branch and every registered toolchain's
repositories — macOS multitool binaries, other Go SDKs — 831 requested,
~600 not needed). Module extensions stay sequential (coordinator-only, as
do local rules, registry/Git/built-in roots and group preparation).

**How.** A speculation runs the coordinator's own materialization
(`RepositorySet::materialize_ordinary` / `materialize_direct`, through
crate::repository_store, unchanged) on a fetch worker in no-demand +
reuse-only mode: a demand of anything not available yet (an unprepared
group, a repository nobody fetched, one another speculation is executing)
aborts it with the sentinel, and it is queued again (at most 4 attempts)
once `sentinel_ready` finds that demand satisfied (after each inline load,
each published speculation). While it runs:

- everything the rule writes to stderr — print(), the `execute(quiet=False)`
  echo, the reproducibility DEBUG lines, download retry warnings — goes to
  the speculation's buffer (`repository_speculation::defer_stderr`);
- what it observes for the analysis snapshot is captured in a detached
  frame and the roots it obtains are recorded (`UnitRoots::during`);
- a recorded-input key (`friendly_repository`) is formed with the roots
  that count for the sequential engine now plus the ones the rule obtained
  itself; when the key could depend on a root that only begins to count
  later (a store path in no such root, or a materialized root naming it
  otherwise), the speculation notes an order-dependent decision and aborts
  before publishing (`speculation_order_check`); the coordinator executes
  the rule. Residual: two repositories publishing the very same tree, the
  one with the smaller name materialized later.

**Commit at the demand point.** A successful speculation that printed
nothing is published into the repository's demand slot like a warm-reuse
probe's value (`DemandSlot::publish_speculated`): its observations and
obtained roots count where the root begins to count for the sequential
engine (`DirectRuntime::mark_sequential`), and the prefetcher retries the
units blocked on it at once (the expansion runs ahead of the coordinator
through chains such as hub alias → wheel → hub alias). One that printed (or
finished while the coordinator was already evaluating the slot) is kept,
and every probe misses on it (`Speculations::hides`, checked before and
after a probe) until the coordinator demands the repository
(`Speculations::take`, waiting for a running one): the coordinator replays
its observations, counts its roots and writes its output there — exactly
where the sequential engine executes the rule. A failed or aborted
speculation is dropped and the coordinator executes the rule itself, so
errors (the first in sequential order) and their prints are the sequential
ones (a failing repository executes twice).

**Stop.** When phase 1 ends (however it ends) the pool stops: queued and
blocked speculations are dropped, running ones are cancelled — downloads
fail at their next read (`extension_download::copy_hashed`), `execute()`
subprocesses are killed at the next poll (`repository_process::run`); the
pool waits at most 2 s, then leaves a stuck worker (a server that never
answers) to end on its own. An outcome nobody committed has its
specification pointer withdrawn (`repository_speculation::withdraw_pointer`, only
when it still names that publication): the next invocation executes the
rule — and prints — again. Ctrl-C ends the process as before (the store is
crash-safe; `execute()` children run in their own process group, as on the
sequential path).

**Downloads.** Like HttpDownloader's semaphore (java:60-69, 184-209), every
mirror attempt of the downloader holds a permit, so the fetch workers'
transfers — and module extensions' non-blocking downloads
(docs/MODULE_EXTENSIONS.md) — are bounded, while their extraction and
`execute()` calls (a wheel's installer) run beside them. Genuine's 8
(`--http_max_parallel_downloads`' default; the option is accepted and
ignored) are 8 per host here, under a total of 16, and a transfer someone
waits for — the coordinator's, a module extension's it evaluates, a
speculation's the coordinator waits for — has 4 reserve permits and goes
first (docs/COLD_FETCH.md). Measured (cold `build --nobuild
//src/python:bworker_env`, shared host): the 284 wheel repositories took
~22 s with 8 workers, ~16 s with 32.

**Intended differences.** Repositories the build needed but did not reach
because of an earlier failure may be published (as genuine Bazel's
parallel fetches complete); none whose output was not shown. A
speculation whose repository the coordinator never demands (a certainty
guess wrong for another configuration) costs a fetch. Within the recording
the taint details keep their insertion order, so a diagnostics summary may
list them in another order (the recorder is a set).

**Telemetry.** With `BAZEL_RS_TIMING=1`, `PERF-FETCH workers= requested=
started= published= done= blocked= failed= retried= taken= waited= wait=
missed= withdrawn=` when the pool stops.

**Determinism evidence.** `tests/parallel_fetch.rs`: a workspace with 15
slow repository rules downloading from a loopback HTTP fixture that answers
late (direct and extension-generated, printing ones, one demanding another
repository's file, one reading a workspace file and the client
environment, one on a select() branch not taken): cold builds at threads 1
/ 8 / 32 / 8 + jitter byte-identical in exit code, stdout, stderr,
lockfile and every published tree; downloads overlap (peak > 1) and the
build is faster; the select() branch not taken is never fetched; the
snapshot recording's observations, environment reads and taints are the
sequential run's; the first failure in sequential order is reported, and
after fixing it the next build prints exactly what the sequential engine's
does (pointer withdrawal); a failing build does not wait for a trickling
speculative download; SIGINT during speculative downloads leaves a store
the next build completes from. Unit rows in `repository_speculation` and
`repository_demand`.

**Measured** (~/code, cold private cache and lockfile copy, release, 64
cores, public PyPI/GitHub; shared machine, indicative):

| cold build | before | after |
|---|---|---|
| `build --nobuild //src/python:bworker_env` (base 06da546) | 408.8 s (phase 1 366.8 s) | 143.9 s (phase 1 75.6 s) |
| same, rebased on f4d460f (installed aed1e34e vs this) | 456.4 s (phase 1 351.8 s) | 160.8 s (phase 1 73.5 s) |
| `build //src/beyondwork.ai/tools/oauth2token/...` (692 actions) | 144.1 s (phase 1 88.5 s) | 112.3 s (phase 1 55.6 s) |

bworker_env: 264 speculations requested, ~225 published into slots, 5–11
taken with output or by an evaluating coordinator, 0 withdrawn; stdout,
stderr (minus PERF lines) and the lockfile byte-identical; tree identities
differ only where they differ between sequential runs already
(rules_python's generated BUILD files order platform sets by Python's
randomized string hash; the interpreter tree embeds the cache path).
oauth2token (same cache path for both runs): stdout, stderr including the
full `BAZEL_RS_DUMP_ACTIONS` dump (46k lines), lockfile and the set of
published tree identities identical. Peak RSS unchanged (the high-water
mark is set during pattern loading). The pattern-loading span (~80 s of
bworker_env: extension evaluation, the interpreter download) is untouched:
module extensions stay sequential.

## Module roots and load batches fetched ahead (perf-cold-start)

Bazel requests the `load()`s of a `.bzl` or BUILD file as one Skyframe batch
(BzlLoadFunction.getLoadLabels → `env.getValuesAndExceptions`) and expands
the registered toolchain patterns together (RegisteredToolchainsFunction), so
the repositories behind them are fetched in parallel. The native loader
demanded them one at a time. `BAZEL_RS_TIMING_TIMELINE=1` (spans
`module-root`, `module-root-warm`, `package` added) showed on ~/code, cold:
~15 s of registry module roots (download + extraction, 0.2–2.8 s each) one
after another on the coordinator in bworker_env's pattern loading and
toolchain registration, and in `//src/...`'s pattern phase 3,842
aspect_rules_js repositories (the loads of `@npm//:defs.bzl`) executed one
after another: ~175 s of 217 s.

**Registry module roots** (`src/module_root_warm.rs`). At most 8 workers
publish the trees of the registry modules named by the loads of every file
the coordinator enters — following their `load()` statements transitively
(each file parsed once, resolved with its module's base mapping) — and by
the registered toolchain patterns (their packages' BUILD files, and the
extension `.bzl` of a pattern repository's generating group) into the
repository store. Only the store is warmed: no demand slot is filled, no
registry file is recorded for the lockfile (`SessionHandle::peek_object`
reads without memoizing or recording; the sequential read records it from
the content-addressed cache the peek filled), nothing is observed. The
demand materializes the root exactly as before; finding a tree this process
published (materializer `WARMED`, checked and consumed under the tree's
file lock) it replays what that cold materialization observed (download,
taint) instead of the reuse check. A failed warm-up leaves nothing.

**Invocation shutdown.** The CLI owns a small `RepositorySet::root_warming_scope`
independently of its intentionally leaked analysis graph. Normal return/unwind,
explicit CLI exits and retry-before-exec close admission, discard queued jobs,
cancel in-flight warm-ups and join their threads. A spawning worker uses the
same worker-list/state lock order as shutdown, so no new handle escapes it.
Library callers enabling warming may own the same scope; simply dropping the
repository set is insufficient while its workers still hold clones.

This closes a reproducible lifecycle hole: the foreground fails a missing
overlay under the per-key publication lock, cleans its stage and releases the
lock; a queued warmer then repeats materialization and owns a new `.stage-*`
when the command returns. Previously the worker's dropped JoinHandle allowed
process exit before that stage's destructor. The controlled subprocess test
records the exact private stage before and after exit. It does not imply that
every historical nameless directory-emptiness failure had this cause; published
64-hex trees/manifests must still be distinguished from private stages.

Cancellation does not delete any published tree or weaken verification. Warm
lock acquisition is cancellable; extraction and materialization check the
token before work/publication. Registry HTTP warm-ups use a per-worker async
transport to interrupt headers/body waits (10 ms polling), with **the same**
URL/redirect/proxy/credential/status/encoding rules, 10 s connect and 20 s
per-read limits, and no whole-transfer deadline. Ordinary transport is unchanged.
Credentials waiting on a URI lock and helper subprocesses also honor this token,
including helpers configured with an unlimited timeout. This avoids joining an
endlessly trickling body or a blocked helper, without shortening healthy warm
downloads' timeouts or globally disabling warming. Private stages/download
temporaries unwind before the worker is joined. Successful, fully verified
publications remain available for ordinary reuse and WARMED observation replay.

The cancellation bound is cooperative, not a hard real-time guarantee for
arbitrary local filesystem calls, decompression, patch processing or cleanup.
System DNS resolver tasks cannot safely be killed: canceled network futures
are dropped and their runtime does not wait for OS resolver tasks, which own
no cache files/stages. SIGKILL, immediate signal termination and filesystem
cleanup errors retain the existing crash/garbage-collection boundary. Tests
cover queued/late admission, active-stage cleanup, stalled HTTP headers/body
(including socket closure and incomplete blob removal), and an unlimited
credential helper's process-group termination, each with bounded deadlines.

**Load batches.** The loads of a file the coordinator enters request the
speculation (`repository_speculation`, unchanged semantics) of their
direct and extension-generated repositories — those of a group the batch's
first load prepares once it is prepared. The target pattern phase runs its
own speculation pool (phase 1 keeps its own), and a P2 worker that aborts on
an unfetched repository requests it (or warms the registry root): the
coordinator commits every pattern package, so it demands that repository
next.

**Non-blocking downloads.** `file()` of a path (not a label) commits only
the pending downloads to it or into it, like `read()`. bazel-gazelle's
go_deps (`internal/bzlmod/goproxy.bzl`) writes a `.info` file after each
non-blocking `.mod` download; committing all of them there serialized ~150
downloads.

Off at `--loading_phase_threads=1` (the sequential engine);
`BAZEL_RS_DEBUG_NO_ROOT_WARM` switches the warm-up off,
`BAZEL_RS_DEBUG_NO_SPECULATION` the pools. Determinism:
`tests/module_root_warm.rs` (cold builds at threads 1/8/32/8+jitter and
without the warm-up byte-identical in exit, stdout, stderr, lockfile and the
snapshot recording; transitive and registration warm-ups ran; a failing
archive fails identically; a file's extension repositories execute on
fetch workers during the pattern phase), `tests/parallel_download.rs`.

**Not done** (measured, left): module extensions are still evaluated one
after another (toolchain registration on bworker_env: ~6 s left, rules_rust's
root 2.7 s + its extension); `zig_config` (13.5 s, the bworker_env critical
path) extracts the same tarball twice because the rule calls
`download_and_extract` twice (genuine does too; the xz decode, 2.9 s of each,
is as fast as `xz -d`); select() conditions are still decided only by the
coordinator's analysis (rules_python's wheel conditions need Starlark flag
analysis; fetching every branch was rejected before).
