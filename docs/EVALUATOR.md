# The parallel analysis evaluator

Status: **S0 done** (interfaces frozen, measurements below). Nothing here
runs in a build yet: `BAZEL_RS_ANALYSIS=walk` (the default) is today's
sequential phase-1 walk with the P3/S1/S2 pipeline
(docs/PARALLEL_ANALYSIS.md); `BAZEL_RS_ANALYSIS=eval` and
`BAZEL_RS_EVAL_VERIFY=1` fail with an error naming the stage.

Goal: cold analysis of `~/code //src/...` (repositories fetched, analysis
snapshot off) below genuine Bazel (~10–11 s; bazel-rs today ~49–50 s). The
estimate is wall ≈ X + 6–8 s, X the pre-analysis span (everything before
`plan.start`); below 11 s needs X ≤ ~3 s and the linearizer's pass 2 ≤ 1.5 s.

This document adapts the design review of 2026-11-07 (the oracle session
behind /tmp/opencode/w/plans/evaluator-design.md); it replaces the
pipeline plan's S3–S6 (docs/PARALLEL_ANALYSIS.md).

## 1. Model

A restart-based, Skyframe-style evaluator. Every value analysis computes is
a **node**, named by a `NodeKey` and interned to a dense `NodeId` (u32).
A node's **body** is a synchronous function of the shared read view
(`&EvalShared`, `Sync`) and a context (`&mut dyn Cx`): it asks for the
values it needs with `cx.need(&[keys])`, which either returns them all (in
key order) or returns `Missing`; the body returns that at once (`?`), the
attempt is dropped, and the node is re-run from its start once what it
waits for is done. Batches follow the walk's dependency phases: select()
conditions; the target platform; toolchains; dependency edges; toolchain
implementations; native prerequisites; config_setting prerequisites;
aspect edges. If the CPU spent re-running exceeds 20%, a per-node
compute-state (kept across restarts) is added for the expensive bodies.

**Inline driver**: at one thread, `need` runs each missing node
immediately, depth first — exactly today's DFS. It is S1's correctness
baseline.

### Nodes (`src/engine/eval/key.rs`)

| Key | Value | Replaces |
|---|---|---|
| `Pkg(PackageId)` | loaded package | `packages`, `generated`, crate::prefetch |
| `Tgt(Label)` | target lookup (rule/output/source) | `rule_targets`, `source_targets`, `declared_inputs`, `source_dir_probes` |
| `Src(Label)` | source file's visibility | `analyzed_sources` |
| `Group(Label)` | resolved package group | `group_nodes` |
| `Ct1(CtKey)` | phase 1: configured node, rule, toolchain ctx, exec groups/configs, edge configs, delegation, **transcript** | `configured`, `toolchain_contexts`, `exec_group_contexts`, `exec_configurations`, `group_exec_configurations`, `edge_configs`, `deferred_duplicates`, `configured_rules`, `rule_delegations` (value side) |
| `CsMatch(CtKey)` | config_setting matching (may need CT2 of Starlark flags) | `plan_pending` flushes from phase 1 |
| `Platform(ConfiguredTargetKey)` | PlatformValue | `platforms`, `resolving_platforms` |
| `RegToolchains(ConfigId)` / `RegExecPlatforms(ConfigId)` / `ModuleExecPlatforms` | registrations | `registered_toolchains`, `registered_execution_platforms`, `module_execution_platforms`, the `computing_*` guards |
| `ExecCfg(ConfigId, Label)` | exec configuration | `exec_transitions` |
| `Transition(TransitionCacheKey)` | configurations + print() lines | `transition_cache` |
| `TransitionDetails(Vec<Label>)` | build-setting details | `transition_details` |
| `FlagScope(Label)` | target-scoped flag | `flag_scopes` |
| `AspectPred(Label, String)` | predicate + its prints | `aspect_predicates` |
| `AspectAttrAspects(ConfiguredTargetKey, String)` | function attr_aspects lists | `aspect_attr_aspects` |
| `AspectToolchains(ConfigId, String)` | aspect toolchain contexts | `aspect_toolchains`, `resolving_aspect_toolchains` |
| `Status` | workspace status (coordinator-only) | `workspace_status_once` |
| `Ct2(CtKey)` | prepare + compute + commit **as a delta** (natives too) | phase 2's commit, `ActionPlanning` per-key maps |
| `View(ConfiguredTargetKey)` | dependency view | `views` |
| `Aspect(AspectApplicationKey)` | aspect application | `aspect_providers`, `aspect_target_identities` |

### CtKey and its normalization

`CtKey = ConfiguredTargetKey + { platform_tree: bool, forced_exec:
Option<Label> }`: the two facts today's walk reads from **first-visit
state** instead of from the key.

- `platform_tree`: analyzed inside a platform's tree
  (`!resolving_platforms.is_empty()`): the empty configuration (no
  toolchain resolution, exec edges fail, `empty_configuration_rules`).
- `forced_exec`: a selected toolchain implementation forced to its
  parent's execution platform (`toolchain_implementations`, read by
  `toolchain_context_key`).

Normalization (`CtKey::normalized`, unit-tested): `platform_tree` is
dropped in the no-config configuration and for rules that neither use
toolchain resolution nor have exec edges (genrule, Starlark test rule,
exec-transition attribute); `forced_exec` is dropped unless the rule uses
toolchain resolution outside a platform tree. A requester needs `Tgt`
first (it already does: `self.target(&dependency)` precedes `analyze`),
so it normalizes before needing `Ct1`.

**First-visit rule** (pass 1): when two variants of one
ConfiguredTargetKey are reached, the variant reached first in DFS order is
the configured node and plan entry; later reaches are memo hits of it. A
consumer whose CT1 read a non-canonical variant's *observable* fields
(the configured node's providers, `target_incompatibility`) is re-run
with the canonical one (pass 1 reports it; rare: only toolchain-resolving
targets reached both inside and outside a platform tree, or both as a
toolchain implementation and before any registration).

### Scheduling (worker A)

rayon-core pool, 16 MiB stacks, arena binding per thread. Node states
Fresh → Queued → Running → Waiting{missing} → Done; claims by CAS;
completion spawns ready waiters LIFO on the local deque; a boost heap
favours the linearizer's frontier (the lowest unmerged plan indices). No
lock is held across a body, a fetch or Starlark.

**As built (S1, worker A)** — `graph.rs`, `scheduler.rs`, `cycles.rs`:

- `Graph`: the interner plus one `Entry` per id in a segmented arena
  (lock-free by id): state (`Fresh`, `Queued`, `Running`, `Waiting`,
  `Done`, `Deferred`), `missing` (outstanding deps + 1 guard), `epoch`
  (bumped per park; a waiter registration carries it, stale ones are
  ignored), `attempts`, `wanted`, the value (`OnceLock<Arc<NodeValue>>`),
  the transcript of the successful attempt (`Entry::transcript`, the
  events it `emit`ted) and a short mutex over the waiter list and what
  the node waits for (`Wait`). `Tracked` mutexes count the locks a thread
  holds (debug builds); `graph::assert_no_locks(what)` checks it where a
  body or a fetch starts (bodies may call it before Starlark).
- `trait Bodies: Sync { type Attempt; begin(id, key) -> Attempt;
  run(&mut dyn Cx, key) -> Result<NodeValue, Missing>; finish(Attempt,
  NodeValue) -> NodeValue; abandon(Attempt); target_platform(ConfigId) ->
  NodeKey; stops_build(key, value) -> bool }`: `begin`/`finish`/`abandon`
  bracket every attempt on its thread — **the hook for the caller's
  Isolation and `observed_fs` frame** (a `Missing` attempt is abandoned,
  a done one finished; at one thread the frames nest like the walk's).
  `FnBodies { shared, body: NodeFn, target_platform }` adapts a `NodeFn`.
- `Evaluator::new(&bodies, EvalConfig { threads, keep_going,
  jitter_micros })`; `evaluate(&roots, &mut dyn RepoLane, &mut dyn
  CycleHandler) -> Outcome { complete, unresolved, deferred, first_error
  }`; `value(&key)`, `graph()`, `boost(&key, priority)`, `stats()`,
  `frontier_only()`. Several `evaluate` calls share the graph.
  `EvalConfig::for_build(loading_phase_threads, keep_going, cap_mb)`:
  threads = `worker_threads` = min(cores, `--loading_phase_threads`,
  cap / 64 MB), at least 1; jitter from `BAZEL_RS_EVAL_JITTER` (µs bound
  of a reproducible per-attempt sleep; a snapshot test hook).
- **Inline driver** (`threads == 1`, no pool): `need` runs each missing
  key in place, in key order (today's DFS); a key that is running (an
  ancestor, or the node itself) answers `BackEdge(key)` if no earlier key
  of the batch is missing; `Missing::Repo` is fetched at once through the
  lane and the node re-run; a body's own `Missing::Nodes` runs the fresh
  ones and re-runs. A node that cannot finish (an unconverted back edge)
  parks; `need` re-runs parked nodes it meets.
- **Pool driver**: `build_pool` (16 MiB stacks, `eval-N` threads bound to
  the caller's jemalloc arena). Each attempt: CAS `Queued → Running`;
  `need` answers from published values only (`BackEdge` for the node
  itself and for handler-answered pairs); a value is published and its
  waiters counted down — the first ready one continues on the same
  thread, the rest are pushed on the local deque (LIFO); `Missing` parks:
  `Nodes` (deduplicated; fresh ones claimed and spawned in chunks: ≥ 8
  per worker, ≤ 64 nodes each), `TargetPlatform(cfg)` (as `Nodes` of
  `Bodies::target_platform(cfg)`), `Repo(name)` (parked on the name; the
  coordinator's FIFO lane fetches each once and re-queues the demanders;
  a repository fetched *before* the attempt began is `Unresolvable`),
  `BackEdge` (parked for the handler). Coordinator-only keys run on the
  thread that called `evaluate`, which also runs the lane.
- **Quiescence** (no job outstanding, coordinator queues empty, roots not
  done): the coordinator builds a `WaitGraph` (parked nodes and their
  waits in need order, deferred nodes, answered pairs) and calls
  `CycleHandler::at_quiescence` → `Resolution::Answer(vec![(parent,
  child)])` (each parent re-runs; its `need` of the child answers
  `BackEdge`) or `Stop`. A pair is answered at most once; nothing new →
  stop, parked nodes in `Outcome::unresolved`. `cycles::DfsCycles` (the
  first back edge of a DFS from the roots over the wait graph) is the
  stand-in until pass 1 (C) provides the handler.
- **Frontier-only**: without `keep_going`, the first value
  `Bodies::stops_build` calls fatal (default: a failed CT1/CT2/Aspect)
  sets `Outcome::first_error` and switches it on: queued nodes not
  `wanted` become `Deferred` instead of running. `boost(key, p)` wants a
  node and (transitively) what it waits for, un-defers them, and pushes
  it on the boost heap that every job pops (lowest `p`) before its own
  nodes; a wanted node's dependencies inherit `wanted` when it parks.
  The inline driver ignores the mode (it is the DFS itself).
- **Panics** in a body propagate out of `evaluate` (no hang: the job's
  outstanding count is released by a drop guard).

Measured (release without LTO, this host, `cargo test --release --lib
eval_scheduler_bench -- --ignored --nocapture`; trivial bodies, keys
pre-interned):

| Graph | 1 thread (inline) | 2 | 8 | 32 |
|---|---|---|---|---|
| tree 64×64×50 (208,961 nodes) | 0.85 µs/node | 0.83 | 0.42 wall (3.4 CPU) | 0.23 wall (7.3 CPU) |
| one node needing 200,000 | 0.69 µs/node | 1.51 | 1.36 | 1.22 |

Chain latency (dependency completed → dependent's attempt running), a
5,000 chain: median 0.35–0.39 µs, p99 0.74–0.90 µs, max 3–8 µs at 2/8/32
threads; with two dependencies where the last completes on another
worker: median 0.59 µs, p99 2.7 µs (target ≤ 50 µs).

CT2(K) runs when CT1(K), the CT2 of its edges, toolchain implementations
and aspects are done **and** pass 1 has assigned K's plan index. Memory:
unmerged CT2 deltas capped at 16,384; CT2 far beyond the merge frontier
is not started.

### Cycles and errors

Reentrancy guards (`resolving_platforms`, `computing_toolchains`,
`computing_execution_platforms`, the CT1 `active` stack) become
`Missing::BackEdge`. At quiescence with parked nodes, pass 1's DFS replay
finds the back edge in walk order and reports today's text
(`cycle_error`, `toolchain_cycle`, `execution_platform_cycle`).

Errors are values (`Result` inside `NodeValue`). Without `--keep_going`
the first error (in linearized order) switches the evaluator to
frontier-only mode (only nodes the linearizer still needs to reach the
error's position); with it, errors are memoized and their transcripts
replayed on revisit.

### Repositories

Workers never fetch: a body that needs an unfetched repository gets the
sentinel and returns `Missing::Repo(name)`; the coordinator's single
FIFO repository lane fetches (repository speculation as today) and the
node is re-run. Fetch stderr is emitted at the first demander's
linearized position. A repository whose recorded-input keys would depend
on the order of demands is an internal error naming
`BAZEL_RS_ANALYSIS=walk`.

### Determinism: pass 1 and pass 2 (worker C)

Bodies record what the walk's order decides as transcript **events**
(`transcript.rs`): `Reach`, `Access`, `Source`, `PkgCommit`, `MemoUse`,
`Warn`, `Print`, `CqueryEdge`, `ScopeEnter`/`ScopeExit`, `Flush`,
`Continue(CsMatch)`, `ToolchainImpl`, `Error`, `PlanPush`.

- **Pass 1** replays the CT1 transcripts depth first from the requested
  targets (the walk's DFS): plan indices, `rule_delegations`,
  `empty_configuration_rules`, the `toolchain_implementations` conflict
  check and first-visit `forced_exec`, keep-going edges, the cquery
  capture, and the **tape** (`Tape`: `Effect`, `Plan`, `Flush`).
- **Pass 2** streams the tape: emits phase-1 effects at their positions;
  at each `Flush` and at the end merges the CT2 deltas of
  `plan[flushed..len]` with `merge_ops` (the merge `Engine::merge_delta`
  uses, S0); then top-level aspects and the planning tail.

Producers are written only by pass 2 (`ReOwned` set and outcome rebuild
for shared paths); console output goes through `DeferredConsole`s emitted
at linearized positions; the workspace status through a `NeedsStatus`
op; the deferred target platform through today's `platform_pending`
logic; `observed_fs` per-attempt frames are committed at merge; captured
sets are replayed on memo hits (mandatory); the incremental tracker is
owned by the linearizer.

### Data (`shared.rs`, `value.rs`)

`EvalShared` (immutable for bodies): `repositories`, the request options
(`EvalOptions`), `configurations` (concurrent intern; ids are content
digests), `aspects` (AspectRegistry), `loaded_functions`, the client
environment, and the shared single-flight caches (`input_groups`,
`input_memos`, `ConcurrentArtifactIndex` — stamps become a debug check —,
target identities; the `.bzl` cache lives in the repository set).
Everything else is a node value, a transcript event, linearizer-only, or
removed (§5).

## 2. Ownership (S1 starts in parallel)

| Worker | Owns | S1 deliverable |
|---|---|---|
| **A** evaluator core | `eval/key.rs`, `eval/cx.rs`; new `eval/graph.rs`, `eval/scheduler.rs`, `eval/cycles.rs` | graph storage, node states, inline driver + pool scheduler implementing `Cx`, BackEdge/cycle plumbing, unit tests (toy node functions over `cx::tests::MapCx`-style fixtures) |
| **B** node functions | `eval/value.rs`, `eval/shared.rs`; new `eval/bodies/*.rs`; the phase-1 region of `src/engine.rs` (`analyze_key_body` … `inputs`, ~4600–10650) | CT1 + memo bodies off `&mut self` (writes per §5), `EvalShared` built from the Engine, run by A's inline driver |
| **C** linearizer / determinism | `eval/transcript.rs`, `eval/replay_bench.rs`, `eval/verify.rs`; new `eval/pass1.rs` (S3: `eval/pass2.rs`); `delta::merge_ops` | pass 1 over transcripts; the old walk driven by the tape; `BAZEL_RS_EVAL_VERIFY` |

Changing a frozen interface (a `NodeKey` variant, a `Missing` variant, an
`Event`, a field of `Ct1Value`/`Ct2Value`/`EvalShared`) is a reviewed
change announced to the other two workers; adding a helper is not.

## 3. Interfaces (frozen in S0)

- `key.rs`: `NodeKey` (21 variants above), `CtKey { key, platform_tree,
  forced_exec }` + `CtKey::plain`/`normalized(Observes)`, `NodeId(u32)`,
  `Interner` (64 mutex shards key→id, segmented append-only arena
  id→key with lock-free reads; `intern`, `get`, `key`, `len`).
- `cx.rs`: `trait Cx { need(&[NodeKey]) -> Result<Vals, Missing>;
  need_one; emit(Event); current() }`; `Missing::{Nodes(Vec<NodeId>),
  Repo(String), TargetPlatform(ConfigId), BackEdge(NodeId)}`; `Vals`;
  `type NodeFn = fn(&EvalShared, &mut dyn Cx, &NodeKey) ->
  Result<NodeValue, Missing>`; `cx::tests::MapCx` (test context).
- `value.rs`: `NodeValue` (one variant per key kind, wrapping today's
  types; errors are values), `Ct1Value { outcome: Result<Ct1Ok, String>,
  transcript }`, `Ct1Ok` (one field per §5 "node value" write),
  `Ct2Value { reads: Ct2Reads, delta: Mutex<Option<TargetDelta>>, error }`,
  `TransitionValue`, `AspectValue`. Compile-time `Send + Sync` checks.
- `shared.rs`: `EvalShared`, `EvalOptions` (compile-time `Sync` check).
- `transcript.rs`: `Event`, `Transcript`, `TapeOp`, `Tape`.
- `mod.rs`: `AnalysisMode::{Walk, Eval}` from `BAZEL_RS_ANALYSIS`;
  `check_mode()` at the start of planning; `verify.rs`: the
  `BAZEL_RS_EVAL_VERIFY=1` entry point and the `Compared` stages.
- `delta.rs`: `merge_ops`/`merge_op`/`MergeSink` (the linearizer's
  merge; `Engine::merge_delta` calls it), `DeltaOp::kind`,
  `DeltaOp::replay_copy`.

### Interface changes

- 2026-11-07 (A, S1): none to the frozen interfaces. Added beside them:
  `graph.rs`, `scheduler.rs` (`Bodies`, `FnBodies`, `RepoLane`,
  `Evaluator`, `EvalConfig`, `Outcome`), `cycles.rs` (`CycleHandler`,
  `WaitGraph`, `Resolution`, `DfsCycles`); `key::slot_of` is
  `pub(super)` (the graph shares the interner's arena layout). Semantics
  made precise: `Cx::need` takes the keys in order and answers
  `BackEdge(k)` only when no earlier key of the batch is missing
  (`MapCx` answers it first); the events a body `emit`s are kept with the
  successful attempt (`Entry::transcript`), not put into the value.

## 4. Stages and gates

| Stage | Work | Gate |
|---|---|---|
| S0 (done) | skeleton, measurements, classification | fmt, clippy, lib tests, ptest unchanged, release build |
| S1 (4–5 wk) | A graph/scheduler/cx/cycles; B CT1 + memo bodies, inline driver; C transcript, pass 1, old walk driven by the tape | threads=1 eval identical across all suites; EVAL_VERIFY clean on ~/code |
| S2 (1–2 wk) | parallel CT1; Pkg nodes replace prefetch.rs; CsMatch split; streaming pass 1 | identical at 1/2/8/32 threads + jitter; phase 1 ≤ 2 s; wall ≤ ~33 s |
| S3 (3–4 wk) | plan-index gating, delta cap, platform waits; native deltas; pass 2 | EVAL_VERIFY + per-node verify; linearizer ≤ 3 s; wall ≤ 15 s |
| S4 (~3 wk) | repo lane + cold tests; Aspect nodes, top-level aspects, deferred platform trees; keep_going, aquery/cquery, snapshots, affected digests | all suites in the thread matrix; ~/code identical; wall ≤ 11 s |
| S5 (1–2 wk) | delete Pipeline, the lowering queue, ui::HELD, plan_pending, … | after 2 weeks green CI and one release with `walk` available |

## 5. Test strategy

- **EVAL_VERIFY** (`BAZEL_RS_EVAL_VERIFY=1`): the dual-engine oracle — two
  Engines over one `Arc<RepositorySet>`, the walk then the evaluator,
  compared in order: plan (keys and order), configured nodes, contexts,
  delegations, action dump, stderr, incremental index, cquery graph; per
  node, `compare_deltas` (the BAZEL_RS_VERIFY_DELTA comparison). The first
  difference names the key and the field.
- **Thread matrix**: every suite at 1, 2, 8 and 32 threads; plus a jitter
  mode (reproducible per-node sleeps, like `BAZEL_RS_LOADING_JITTER`) that
  shuffles completion order. Outputs must be byte-identical
  (stdout/stderr, BAZEL_RS_DUMP_ACTIONS, keys).
- **Unit tests** per module (S0 has them for the interner, normalization,
  `need` batching/back edges, values, the tape, the switch).
- **~/code canary**: `build`/`test --nobuild //src/...` action dumps and
  stdout identical to the walk; cold timing per stage gate.

## 6. Measured (S0)

All on ~/code `80d6724f3a` (private clone), `build --nobuild //src/...`,
repositories fetched (warm cache), `BAZEL_RS_SNAPSHOT=off`, default
threads, `BAZEL_RS_TIMING=1` (+ `BAZEL_RS_TIMING_TIMELINE=1` for the
spans), fast release profile (no LTO). Wall 49.7–50.3 s.
With the LTO release binary: wall 47.9 s, X = 3.06 s (modules 0.21 s,
configuration 0.24 s, pattern loading 2.56 s), phase 1 35.4 s, phase-2 tail
6.5 s, plan.built 1.7 s.

### The pre-analysis span X = 3.07–3.09 s

New marks (S0): `startup.rc`, `startup.options`, `startup.modules`,
`startup.configuration`, timeline spans `pattern-unit` (a loading
worker's unit) and `pattern-commit` (the coordinator's commit, with its
inline re-run).

| Span | Time | What |
|---|---|---|
| main.start → startup.options | 0.002 s | argv, rc files, options |
| → startup.modules | 0.20 s | registry session, RepositorySet, root module, module graph (`resolve` 0.08 s) |
| → startup.configuration | 0.20 s | Starlark options: loads `@rules_python//python/config_settings` (0.09 s), `@rules_rust//rust/settings` (0.09 s), `@protobuf//bazel/toolchains`; the python/bazel_features extensions on the way |
| → select.discovered | 0.035 s | 514 packages below //src |
| → select.loaded | **2.63 s** | see below |
| → plan.start | 0.017 s | |

Pattern loading: the 514 worker units take 0.055 s wall (1.1 s CPU). The
coordinator then commits in pattern order for 2.57 s, mostly **inline
re-runs** of units that needed an extension or an unpublished repository
(workers never fetch): `//src/ts` 0.73 s, `//src/website3` 0.47 s,
`//src/beyondwork.ai/idl/api/taskrunner` 0.28 s (the npm extension,
0.26 s), `//src/vscode/beyondwork` 0.18 s, `//src/ansible` 0.17 s (pip,
0.15 s), `//src/rust/graphlake/core` 0.09 s (crate, 0.07 s); 3,531
`aspect_rules_js++npm+npm__*__links` repositories are reuse-validated by
the fetch pool (3.47 s busy over 32 workers, but demanded a few at a time
by the sequential re-runs: a 1.14–2.84 s window); ~500 plain commits
~0.4 s.

What would cut X (to ~1 s):

1. Bulk repository demand for pattern loading: collect the sentinels of
   all failed units, warm-validate them in one parallel batch (3.5 s CPU
   → ~0.15 s at 32 workers), re-run the failed units on the workers;
   only the commits stay ordered. Est. loading 2.6 → ~0.6 s.
2. Evaluate the module extensions the lockfile names in parallel at
   startup (npm 0.26, pip 0.15, crate 0.07, python 0.06 s … ≈0.6 s
   sequential, ≈0.26 s critical), overlapped with the configuration step.
3. Overlap the Starlark-option package loads (0.2 s) with pattern
   discovery/loading.
4. Pkg nodes (S2): pattern loading and phase 1 overlap; the coordinator
   orders only effects.
5. Module resolution (0.2 s): reuse the resolved graph when MODULE.bazel
   files and the lockfile are unchanged (digest memo).

### The planning tail

| Span | Time |
|---|---|
| plan.start → plan.phase1_analysis_done | 38.2 s |
| → plan.phase2_actions_done | 6.6 s (the plan walk's remaining commits after phase 1, top-level aspects) |
| → plan.built (new) | 1.48 s (top-level output groups and artifacts, reachability prune, BuildPlan) |
| → plan.end | 0.02 s |
| → execute.end | 0.21 s (`--nobuild`) |
| → exit | ~0.6 s (reports, teardown) |

### Linearizer merge throughput (`BAZEL_RS_EVAL_REPLAY_BENCH=1`)

The bench copies each delta `merge_delta` merges, times the real merge,
and at `plan.phase2_actions_done` replays the copies through `merge_ops`
into a fresh `ActionPlanning` (the stub linearizer): untimed best of 3,
presized best of 3, and once with every operation timed.

| | Merges | Time | Per merge | Rate |
|---|---|---|---|---|
| real `merge_delta` (in the build) | 38,757 | 0.77–0.79 s | 19.9–20.3 µs | ~50k/s |
| stub replay | 38,757 (677,594 ops; 92,678 in-flight lowerings not copied) | 0.57–0.63 s | 14.8–16.1 µs | 62–68k/s |
| stub replay, maps presized | 38,757 | 0.52 s | 13.5 µs | 74k/s |
| extrapolated: every target a delta (final state volume) | 133,156 | 1.12 s | 8.4 µs | **119k/s** |

Per operation (timed replay): artifact registration 1.66 µs × 4.0 per
merge = 6.7 µs; output (producer) 1.75 µs × 2.8 = 4.9 µs; declared 0.62 µs
× 4.0 = 2.5 µs; providers 1.6 µs; action execution 0.40 µs × 2.3 = 0.9 µs;
dry 0.6 µs; runfiles info 0.65 µs; shared 0.15 µs; node 1.5 µs. Volume of
the build: 133,156 plan entries, 109,914 producers, 95,414 nodes, 298,216
indexed artifacts, 109,914 declared, 133,173 dry.

**Gate (≥150k merges/s): not met** (≈0.8×). The merge is hashing and
allocation bound. Fixes, in order of yield: (a) one hash per artifact
registration (today `get` then `insert`, each hashing the File and taking
a shard lock; the linearizer is the single writer); (b) `Producers::insert`
allocates a `PathBuf` per new ancestor and hashes into `PathHashes`:
carry precomputed path/ancestor hashes in the delta (computed on the CT2
worker) into hash tables keyed by them; (c) presize from the CT2 count
(−9% measured); (d) merge the independent maps on 2–3 linearizer threads
(index | producers + declared | per-key maps; order matters within a map
only). (a)+(b)+(c) are estimated ~2× (≈240k/s), inside pass 2's 1.5 s.

## 7. The phase-1 writes, classified

Every `self.` write of the phase-1 region (`analyze` … `inputs`,
src/engine.rs ~4600–10650) and of the `&mut self` helpers it calls.
Kinds: **V** node value (which node), **T** transcript event, **L**
linearizer-only, **S** shared (`EvalShared`, single-flight or concurrent),
**R** removed. ⚠ = a hidden first-visit memo (the result depends on what
the walk visited first; pass 1 reproduces it).

| Write | Where | Kind | Becomes |
|---|---|---|---|
| `configured.insert` (local `ConfiguredNodes`) | analyze_key_body, delegation, publish_incompatible | V + L | `Ct1Ok.node`; pass 1 builds the map from the first-reached variant |
| `plan.push` (local) | analyze_key_body, publish_incompatible | T + L | `PlanPush`; pass 1 assigns the index |
| `active` push/pop (local) | analyze_key_body, rule_transition | R | the evaluator's active chain → `Missing::BackEdge`, pass-1 cycle text |
| `analyze_depth` | analyze | R | per-node timing in the scheduler |
| `ahead` | analyze_key_body (`ahead_abort`) | R | P3 ahead mode goes away |
| `analyzed_sources` ⚠ | analyze_key_body | V + T | `Src(label)` (pure: visibility does not depend on the consumer; failures were not memoized and are recomputed identically); `Source` |
| `rule_delegations` | analyze_key_body | V + L | `Ct1Ok.delegate`; pass 1 builds the map |
| `keep_going_state.record_edge` | analyze_key_body | T + L | `Reach` (pass 1 records edges in DFS order) |
| `empty_configuration_rules` ⚠ | analyze_key_body | V + L | `Ct1Ok.empty_configuration` / `CtKey.platform_tree`; pass 1's set |
| `resolving_platforms` push/pop ⚠ | platform_info | R | `CtKey.platform_tree` on edges inside a platform's tree; `BackEdge` for the reentrancy error |
| `platforms` | platform_info | V | `Platform(key)` |
| `planning_plan` take/set | deferred_target_platform | R + L | `Missing::TargetPlatform(cfg)`; the linearizer's platform_pending logic |
| `module_execution_platforms` | module_execution_platforms | V | `ModuleExecPlatforms` (coordinator-only) |
| `registered_toolchains` | registered_toolchains | V | `RegToolchains(cfg)` |
| `computing_toolchains` push/pop | registered_toolchains | R | `BackEdge` |
| `toolchain_cycle_reports` insert/remove ⚠ | toolchain_cycle, compute_registered_toolchains | L | pass 1 decides the report at quiescence (the text names the registered target being analyzed) |
| `root_checkpoint.first` | registered_toolchains | T + L | `MemoUse(RegToolchains)`: first successful configuration in linearized order |
| `registered_execution_platforms` | execution_platforms | V | `RegExecPlatforms(cfg)` |
| `computing_execution_platforms` | execution_platforms | R | `BackEdge` |
| `execution_platform_cycle_reports` ⚠ | compute_execution_platforms, execution_platform_cycle | L | as toolchain cycle reports |
| `toolchain_contexts` ⚠ | analyze_key_body | V | `Ct1Ok.toolchain_context` (depends on `forced_exec`, `platform_tree`: in the key) |
| `exec_group_contexts` ⚠ | analyze_key_body | V | `Ct1Ok.exec_groups` (same) |
| `toolchain_implementations` ⚠ | toolchain_implementation (read by toolchain_context_key) | T + L | `ToolchainImpl`; pass 1's conflict check and first-visit `forced_exec` |
| `exec_configurations` | analyze_key_body | V | `Ct1Ok.exec_config` |
| `group_exec_configurations` | analyze_key_body | V | `Ct1Ok.group_exec_configs` |
| `exec_transitions` | exec_configuration | V | `ExecCfg(cfg, platform)` |
| `configurations.intern_*` | exec_configuration, apply_transition, analyze_key_body, split | S | concurrent intern (content-addressed ids; mnemonics verified by EVAL_VERIFY) |
| `flag_scopes` | target_scoped_flags | V | `FlagScope(label)` |
| `transition_cache` ⚠ (prints) | apply_transition | V + T | `Transition(key)` with its prints; a `Print` per application (a hit replays them); failures not memoized |
| `transition_details` | transition_details | V | `TransitionDetails(labels)` |
| `edge_configs` | analyze_key_body | V | `Ct1Ok.edge_configs` |
| `deferred_duplicates` | analyze_key_body | V | `Ct1Ok.deferred_duplicates` |
| `configured_rules` | analyze_key_body, publish_incompatible | V + L | `Ct1Ok.rule`; pass 1's map |
| `warnings.push` (deduplicated) | analyze_key_body (config_setting), expand_registration_patterns, prerequisite_error, report_pattern_deprecation | T + L | `Warn`; dedup at the linearized position |
| `cquery_capture` (`&mut`, rules, incompatible, edges) | analyze_key_body, aspect edges/applications, capture_cquery_edge, publish_incompatible | T + L | `CqueryEdge`; rules/incompatible from `Ct1Ok`; the capture is pass 1's |
| `group_nodes` | resolve_group | V | `Group(label)` |
| `group_budget` | resolve_group | S | atomic budget; which group exhausts it is decided by pass 1 |
| `prefetch_followed`, `prefetch_platforms` | resolve_toolchains, prefetch_compatible_toolchain, prefetch_rule | R | Pkg nodes replace crate::prefetch (S2) |
| `aspect_predicates` ⚠ (prints once) | aspect_collection_for | V + T | `AspectPred` with its prints; `MemoUse` prints them at the first linearized use |
| `aspect_attr_aspects` | with_computed_attr_aspects | V | `AspectAttrAspects` |
| `aspect_attribute_configs` | aspect_attribute_configurations | V | `Ct1Ok.aspect_attribute_configs` |
| `aspect_toolchains` | aspect_toolchain_contexts | V | `AspectToolchains(cfg, aspect)` |
| `resolving_aspect_toolchains` | aspect_toolchain_contexts | R | a parameter of the AspectToolchains body (error wording) |
| `rule_targets`, `source_targets` | target | V | `Tgt(label)` |
| `declared_inputs` | target → declared_source | V | part of the `Pkg` value (computed once per package) |
| `source_dir_probes` | source_target | S | single-flight filesystem probe cache |
| `source_directories`, `missing_sources` | source_target | T + L | `Source`/`Access`; linearizer-only maps |
| `access_capture` | target → track_access | R | `Access` events |
| `packages`, `generated` | load_package → commit_loaded_package | V | `Pkg(id)` |
| `loaded_functions.register` | commit_loaded_package | S | append-only registry |
| `emitted_bzl_prints`, progress | commit_load_unit | T + L | `PkgCommit` |
| `incremental` (track_key/enter/entry/position/restore/source, printed_keys) | analyze_key, analyze_key_body, note_analysis_print | T + L | `Reach`/`Access`/`Source`/`ScopeEnter`/`ScopeExit`/`PlanPush`; the tracker is the linearizer's |
| console (`ui::note`) | note_analysis_print | T | `Print` |
| `action_planning` take/set | plan_pending (from config_conditions) | R + T + L | `CsMatch` node; `Flush`/`Continue`; the merge at the flush is pass 2's |
| `ahead_aborted` | ahead_abort | R | |

Phase-2 state for reference (not phase-1 writes, owned by C): `plan`,
`producers`, `nodes`, `source_inputs`, `action_executions`, `declared`,
`console_owners`, `errors`, `keep_going_state`, `bep_groups`, the
aquery capture and `status` are linearizer-only; `views`,
`target_identities`, `input_groups`, `input_memos` and the artifact index
are shared single-flight; `view_accesses`, `view_artifacts` and the
tracker's deferral are removed.
