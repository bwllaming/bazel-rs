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

