# The parallel analysis evaluator

Status: **the default engine** (0.4.16, "Selecting the engine" below).
The evaluator plans every request it covers unless the walk is faster;
`BAZEL_RS_ANALYSIS=walk` forces today's sequential phase-1 walk with the
P3/S1/S2 pipeline (docs/PARALLEL_ANALYSIS.md); `BAZEL_RS_EVAL_VERIFY=1`
plans with both and compares.

## Selecting the engine

`src/engine/eval/select.rs`, at the start of every planning
(`Engine::plan_build_with_checkpoint`; every analyzing command goes
through it):

| `BAZEL_RS_ANALYSIS` | Engine |
|---|---|
| unset, empty, `auto` (the default) | the evaluator, unless a fallback below applies |
| `eval` | the evaluator, unless a **coverage** fallback applies (also where the walk is faster: tests, measurements) |
| `walk` | the walk, always |
| anything else | an error naming the three values |

A test-suite binary built with `BAZEL_RS_DEFAULT_ANALYSIS=walk` (or
`eval`) has that as its default (the walk-forced and eval-forced suite
runs; tests that set `BAZEL_RS_ANALYSIS` themselves are unaffected).
`BAZEL_RS_ANALYSIS_REPORT=1` (hidden) prints one line per planning:
`bazel-rs: analysis engine: eval`, `walk (forced)` or
`walk (fallback: <reason>)`.

**Fallbacks** (silent: both engines plan identically, so nothing the user
sees depends on the choice; `tests/eval_default.rs` checks each against the
forced walk byte for byte):

| Reason (report text) | When | Kind | Why |
|---|---|---|---|
| `cquery root checkpoint` | cquery's accepted-query cut between requested targets (docs/CQUERY_ROOT_CHECKPOINT.md) | coverage | the walk's `root_cut` between roots; the evaluator's driver has no cut point |
| `aquery capture` | `aquery`, `print_action` | coverage | the action capture (and the cquery capture aquery uses) is filled by the walk only |
| `cquery capture` | `cquery` | coverage | the bodies emit `CqueryEdge` events, but no pass builds the capture (rules, incompatible targets, aspect targets, toolchain contexts) from them |
| `--keep_going` | `--keep_going`, `-k` (build/test/run/coverage) | coverage | the walk skips a failed requested target and continues (`keep_going_state` edges, failed targets, docs/KEEP_GOING.md); the evaluator stops at the first error (frontier-only) |
| `top-level aspects` | `--aspects`, and the validation aspect (`--run_validations` with `--experimental_use_validation_aspect`) | coverage | the walk analyzes the aspect applications after the requested targets; the driver has no `TopLevelAspects` segment yet. (Before 0.4.16 the validation aspect alone did not fall back.) |
| `--profile` | `--profile` | speed | the evaluator keeps every flush ordered there (the profile's spans of phase 2 and of the tape in the walk's order) |

A snapshot recording (`BAZEL_RS_SNAPSHOT` `on`/`verify` on a miss — the
default mode —, `record`) and the incremental tracker plan with the
evaluator since eval-record (0.4.16 had a `snapshot recording or
incremental tracking` fallback: the evaluator kept every flush ordered
there and was slower, 81.5 s / 10.5 GB against the walk's 63 s / 9.9 GB):
their flushes are deferred too, the tape's tracker ops applied in plan
order by the deferred walk ("eval-record" below).

Everything else plans with the evaluator: `build`, `test`, `run`,
`coverage` (with `--nobuild`, `--check_up_to_date`, `--run_under`, test
sharding, `--platforms`, Starlark flags and transitions, several
configurations, the workspace status, BEP, remote/disk caches — execution
reads the plan, not the engine), `affected` (its `test --nobuild`
subprocesses; the in-place one records when the snapshot is on) and the
self-build (`scripts/self_build.py`), with the snapshot on (a miss
records and saves; a hit's partial plannings, `verify`'s shadow analysis)
or off. Commands without analysis are unaffected (`query`, `fetch`,
`info`, `mod`, `config`, `dump`, `clean`, `gc`).

**Internal errors** of an evaluator planning name the workaround: an error
returned from it that contains `internal error` and not yet
`BAZEL_RS_ANALYSIS=walk` gets ` (the parallel analysis evaluator, the
default engine; rerun with BAZEL_RS_ANALYSIS=walk to plan with the
sequential walk)` appended (`select::with_workaround`; a user error is the
walk's text, unchanged), and a panic while it plans (any thread:
`internal_error::EvaluatorScope`) prints `bazel-rs: the panic happened
while the parallel analysis evaluator (the default engine) planned; rerun
with BAZEL_RS_ANALYSIS=walk …` after the panic (`tests/internal_error.rs`).
Such an error is the one place where the engines differ visibly.

**Measured** (2026-11-10, ~/code `91867d4ddf` private clone, release
build of this branch, warm repositories, 48 threads, host load ~15):

| | walk (`BAZEL_RS_ANALYSIS=walk`) | default |
|---|---|---|
| `build --nobuild //src/...`, snapshot off: elapsed / max RSS | 44.5 / 43.9 s, 8.96 / 8.89 GB | 25.1 / 25.0 s (eval), 9.32 / 9.26 GB |
| the same, snapshot on, a miss (records and saves) | 63.1 s, 9.88 GB | 63.7 s, 9.91 GB (walk: fallback; eval forced: 81.5 s, 10.50 GB); since eval-record the evaluator: 43.5 s, 10.3 GB (below) |
| `affected --scope=full --base=HEAD^` | 219.9 s, 9.13 GB | 84.7 s, 9.53 GB |
| `cquery 'deps(//src/rust/...)'` | 28.5 s | 13.0 s (walk: fallback; the walk run was first) |

Identical: the action dump (`BAZEL_RS_DUMP_ACTIONS=1`, 6,620,633 lines)
and stdout of `build` and `test --nobuild //src/...`; the snapshot
recording (`BAZEL_RS_SNAPSHOT=record`, 521,010 observations; timings
aside) walk vs eval; `affected --base=HEAD^` (scoped and `--scope=full`, 7
of 2,021); `cquery 'deps(//src/rust/...)'` and `cquery //src/python/...
--output=jsonproto`; snapshot `on` (save) → `on` (hit) → `verify` clean. A
real `build //src/rust/graphlake/...` (1,864 actions) and `test
//src/rust/graphlake/...` (187 tests pass) on the evaluator, then the walk:
everything up to date / cached (the same action and test keys). The
self-build (`scripts/self_build.py check --no-test`, snapshot off) on the
evaluator: built and smoke-tested.

**No automatic retry with the walk**: when an evaluator planning fails,
the process has fetched repositories (and may have written the lockfile),
written console output (warnings, `print()`, fetch progress) and fed the
snapshot recording and the tracker; a second planning would print and
record twice and could see the repositories in another state. Not
provably side-effect free, so not done.

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
dropped in the no-config configuration only — it is kept for every rule
of a platform's tree, because the context propagates to the rule's
dependencies (the earlier per-rule rule, "dropped for rules that neither
use toolchain resolution nor have exec edges", was unsound: a filegroup's
genrule dependency escaped the empty configuration; the bodies pass
`exec_edges: true` for every rule); `forced_exec` is dropped unless the
rule uses toolchain resolution outside a platform tree. A requester needs `Tgt`
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
- 2026-11-07 (B, S1): `Cx::id_of(&self, &NodeKey) -> NodeId` (the id an
  event names a node by: `Reach`, `MemoUse`; `InlineCx`/`PoolCx`/`MapCx`
  intern through the graph). `Ct1Value::transcript` keeps a copy of the
  attempt's events (the same as `Entry::transcript`).
  `CtKey::normalized`'s per-rule `platform_tree` rule is unsound: the
  context propagates to the rule's dependencies (a filegroup inside a
  platform's tree would let its genrule escape the empty configuration);
  B's bodies keep `platform_tree` for every rule of a tree (they pass
  `Observes { exec_edges: true, .. }`; `forced_exec` is normalized as
  documented). The `ExecCfg` value marks an interning failure with
  `nodes::phase1::EXEC_INTERN_ERROR` (the consumer locates it at its
  rule).
- 2026-11-10 (eval-phase1, step 1): `repository::EvalAdoption::start_limited(&[group keys])`
  (adoption restricted to those groups; thread-local `EVAL_ALLOWED`);
  `nodes::loading::PkgValue::adopted_groups` (the groups the pool load
  that produced the value used speculatively; empty for a sequential
  load); `target_body`'s source-file part is `source_lookup`.

**Phase-1 bodies as built (S1, worker B)** — `eval/nodes/`:

- `phase1.rs`: the walk's phase-1 region (`analyze_key_body` …
  `check_target_compatibility`, the memo functions' bodies, aspects'
  phase 1), moved off `&mut Engine` into `trait Phase1: Host` (default
  methods, blanket impl). `host.rs`: `trait Host`, the primitives a body
  reads and writes (shared reads, loading, dependency edges, the analyzed
  key's results, order-observable effects, memo nodes).
- `walk.rs`: `Walk { engine, active, configured, plan }` implements
  `Host` with today's code (`BAZEL_RS_ANALYSIS=walk` is unchanged; the
  engine's `analyze`, `target_platform`, `platform_info`, … wrap it).
- `eval_host.rs`: `EvalHost` implements `Host` for one attempt: reads go
  to `EvalShared` or needed nodes; writes become the `Ct1Ok` fields or
  events (`Reach`, `PlanPush`, `MemoUse` of Platform / RegToolchains /
  RegExecPlatforms / ExecCfg / FlagScope / TransitionDetails, `Warn`,
  `Print`, `CqueryEdge`, `Flush`, `ToolchainImpl`, `Error`; `Access` /
  `Source` only while incremental tracking is on; `ScopeEnter`/`Exit`
  not yet: S4). `body()` serves `Tgt`, `Group` (S1: the engine, see
  below), `Src`, `Ct1`, `Platform`, `RegToolchains`, `RegExecPlatforms`,
  `ExecCfg`, `FlagScope`, `TransitionDetails`, `CsMatch` (S1: the
  config_setting's CT1 matching).
- `bodies.rs`: `Phase1Bodies` implements A's `Bodies`; `begin`/`finish`/
  `abandon` keep the stack of attempts (the walk's active chain: cycle
  texts, the 256 depth limit, "registered toolchains in progress") and the
  done CT1 values a body reads without a need (`configured` lookups).
  `trait Phase1Cx<'e>: Cx { coord() }`: the S1 coordinator (`Coord`):
  the engine's loading services (package loading, target lookups, package
  groups, source visibility, package args, aspect definitions,
  registration patterns, module execution platforms — `Pkg`/`Tgt`/`Group`
  bodies over `EvalShared` in S2) and the first-visit state pass 1
  decides from S2 on (`FirstVisit`: the first `CtKey` variant per
  ConfiguredTargetKey, toolchain-implementation claims and `forced_exec`,
  cycle reports, aspect memos, the transition cache).
- `verify_phase1.rs`: `BAZEL_RS_EVAL_VERIFY_PHASE1=1` (log of successes:
  `BAZEL_RS_EVAL_VERIFY_PHASE1_LOG=<file>`): after the walk's phase 1
  (or its failure), the requests are evaluated by A's evaluator (one
  thread, a 1 GiB stack) and compared with the walk: plan order (a DFS
  replay of the transcripts), configured nodes, delegations, configured
  rules, toolchain/exec-group contexts, exec/group/edge configurations,
  deferred duplicates, empty-configuration rules, toolchain claims, the
  error text. Not compared (S2+): plan entries phase 2 analyzes (a
  deferred target platform); errors after a `Flush` (Starlark
  flag_values keys: the walk plans their CT2 there; `planned_feature_flag`
  reads the walk's planning in S1); which registered target a toolchain /
  execution-platform cycle report names (the graph memoizes the first
  failure, the walk re-analyzes it per registration: pass 1's, §7);
  cquery captures, `--keep_going`, top-level `--aspects` (skipped, S4).
  Measured: the test suites (2,952 verified builds), ~/code //src/...
  (`build` and `test --nobuild`: 133,173 configured targets, 133,156
  plan entries, 444,236 nodes, identical; the inline evaluation 13 s
  after the walk loaded every package).

- 2026-11-07 (C, S1; announced to A and B):
  - `Event::Done` (new): the body's configured node is complete (the
    walk's `configured.insert`); later reaches are memo hits, reaches after
    it are no longer below the key in the active chain. B emits it where
    the walk inserts the node (main path, delegation, incompatible target).
  - `Event::PlanPush` carries no key (always the body's own CtKey);
    `Event::CqueryEdge(Box<CqueryEdge>)` and
    `Event::ToolchainImpl(Box<ToolchainImpl>)` box their payloads: `Event`
    is 80 bytes instead of 248 (~/code: 5.9 M events in phase 1).
  - `Event::ScopeEnter` also names the tracker's pattern and suite scopes
    (`"\0pattern"`, `"\0suite"`, incremental::PATTERN_SCOPE/SUITE_SCOPE).
  - `Event::Reach` contract: emitted at the walk's `track_key` point; what
    the walk does before it (the reached label's lookup, a package
    group's resolution) belongs to the reacher.
  - `TapeOp` (C-owned) extended (`Segment`, `Reach`, `Fail`, `Leave`,
    `Plan { key, position }`); `pass1::TapeSink` streams it.
  - Pass 1 reads per CT1 visit `empty_configuration` (also for a failed
    body: the walk inserts it before most errors) and `incompatible`
    (`pass1::Ct1Record`), besides the value's `delegate`.
- 2026-11-08 (C, S1 integration):
  - `Event::Console(u32)` (new): a console write a coordinator service made
    while the evaluator ran, by index into the evaluation's captured
    writes; written once at its linearized position.
  - `Event::PkgCommit(PackageId, bool)`: the commit's BUILD-printed flag
    (the incremental tracker's package record).
  - B's `EvalHost` adapted: `Done` at `publish_node` of its own key,
    `PlanPush` without key, boxed payloads, `MemoUse` of `Src` and `Group`,
    no `Access` for the CT1 body's own lookup, phase-1 prefetch fed as the
    walk does, the first-visit memos written through to the engine, the
    registration cycle reports reworded for the current chain;
    `Coord` keeps the running attempts' events and the finished
    transcripts (`linearize::driver::Store`), and drains the engine's
    services into them; with the linearizer the coordinator's store is the
    transcript of record (`Entry::transcript`/`Ct1Value.transcript` stay
    empty).
  - Pass 1 is resumable (`Pass1::resume`, `Transcripts::segment`,
    records with a `complete` flag); a body's transcript is replayed again
    on each revisit of a failed key (no per-visit records).
- 2026-11-08 (C, S2 part 2: the first-visit decisions out of the bodies;
  announced to eval-s2-load and the extensions worker):
  - `Event::Info(String)` (new): a `--toolchain_resolution_debug` INFO
    event, captured instead of printed while the evaluator runs
    (`toolchains::capture_debug_events`, drained with
    `linearize::capture::take`); the tape application prints it at its
    linearized position, deduplicated process-wide as before.
  - `Event::MemoBegin(u64)` / `Event::MemoEnd(Option<u64>)` (new): a
    first-visit memo's computation (the four aspect memos), by a digest of
    its key; the end carries the value's digest (`None`: it failed, not
    memoized).
  - `ToolchainImpl.location` (new): where the walk reports a conflict.
  - `TransitionValue` is now produced: `NodeKey::Transition(key)` has a body;
    its input (`nodes::host::TransitionInput`: definition, kind, target,
    configuration, attributes) is registered by the first requester in
    `EvalShared::transitions` (new field, a mutex'd map by cache key).
  - `NodeKey::Ct2(ct)` has an S2 body: the providers a flush planned (a
    config_setting's Starlark flag_values key, `planned_feature_flag`).
  - `Host` (B's trait) gains `transition_node`, `open_cycle_report`,
    `memo_failed` (defaults: the walk's behaviour), and
    `claim_toolchain_implementation` takes the consumer's location; `phase1`
    splits `compute_transition`, `compute_aspect_toolchains`,
    `compute_aspect_attribute_configurations` out of their memoizing
    callers; `phase1::REGISTRATION_IN_PROGRESS` is the answer to a back edge
    to `RegToolchains`/`RegExecPlatforms` (the body words the report).
  - `FirstVisit` is gone; `Coord` keeps `ct1_done` (the first variant done
    of each key: reads of a key a body did not reach) and `ct1_values`
    (every variant, by CtKey); `edge_ct_key(no_config, rule, key,
    platform_tree, forced_exec)` takes the context only.
  - **What the pool driver must provide** (eval-s2-load): (1) a parked
    attempt's events up to its `Missing` stay visible to pass 1 as a
    partial transcript until the re-run replaces them (identical prefix;
    pass 1 never consumed beyond it) — a config_setting's `Flush` and its
    `Ct2` need are in one attempt; (2) `Coord::active_ct1` (the inline
    driver's chain, used for the inline cycle and depth checks) must be
    empty on pool threads: cycles then come from `Pass1Cycles` as
    `Missing::BackEdge`, the depth limit from pass 1; (3) the `Ct2` S2 body
    and the published memo values run on the coordinator (they hold the
    engine); (4) call `Coord::frontier()` and `Evaluator::boost` whenever
    pass 1 is blocked (the driver does it per segment today).
- 2026-11-08 (eval-s2-load, S2 part 1: loading nodes and the pool;
  announced to eval-s2-pass1; branch based on eval-s2-pass1):
  - `NodeValue::Pkg(Result<Arc<nodes::loading::PkgValue>, String>)` (was
    `Arc<Package>`): the package with its output-file and declared-input
    indexes. `EvalShared::loading` (new field, `nodes::loading::Loading`):
    the packages loaded before phase 1, the source-directory probes and
    the package-group budget (moved in from the engine and back), the
    resolved groups, the lane's units, the aspect definitions' prints;
    `EvalShared::aspects` is the engine's registry (moved in and back).
  - `Event::LoadPrints(Arc<[LoadPrintEvent]>)` (new): a load's buffered
    print events, written by the tape where the walk's
    `Engine::emit_load_prints` writes them (a module's lines once). A Pkg
    node's transcript is its commit: `LoadPrints`/`PkgCommit` in the
    engine's order; the tape's `PkgCommit` commits the package into the
    engine (`packages`, `generated`, loaded functions, progress) unless
    the engine loaded it itself meanwhile (a flush's phase 2). Every
    lookup `MemoUse`s its package's Pkg node (once per attempt; not for a
    package loaded before phase 1).
  - `Cx::hint(keys)` and `Cx::peek(key)` (new, default no-ops): schedule
    nodes a body needs soon / read a done value without depending on it.
    `EvalConfig::{help_first, stack}` (new), `Evaluator::set_inline`,
    `Evaluator::pooled`, `build_pool(threads, stack)`.
  - `Phase1Cx::flags()` (new; `nodes::bodies::Flags`): tracking and
    linearized, read without the coordinator's lock. The running attempts
    are per thread (`nodes::bodies::{with_running, push_running,
    active_ct1, in_progress}`, no longer `Coord::stack`); `active_ct1` is
    empty on a pool thread (requirement (2) above).
  - `Host::hint_edges(deps)` (new, default no-op): the edge loop's
    dependencies, hinted to the pool.
  - `Coord::{begin_attempt, finish_attempt, abandon_attempt}` take/return
    the `Running` attempt (the thread-local stack is the bodies').

- 2026-11-08 (eval-csmatch, the CsMatch split): `Phase1Cx::{scratch,
  ct1_done}` (new): the speculative phase 2s' scratch engine and the done
  CT1 values without the coordinator's lock; `Coord::ct1_done` is a
  sharded `Ct1Done`. `NodeKey::Ct2` is no longer coordinator-only (a
  pooled evaluation speculates it on the pool; the sequential pass runs
  it inline as before). `Evaluator::evaluate_values` (new): `evaluate`
  without the outcome's scan. `Pass1::take_flush_point` (new): where each
  `Flush` of the tape happens. `progress::analysis_counts` includes the
  configured-target count.

- 2026-11-08 (eval-s3, S3 pass 2; announced to eval-round1): `Loading::
  stable_sources` (new field, `nodes/loading.rs`): the `Tgt` body records
  the source files it resolved to a memoizable path, which the linearizer
  publishes into `Engine::source_targets` (as the walk's lookups memoize
  them) when flushes are deferred. `EvalHost::flush_pending`: in an
  evaluation that defers flushes the sequential pass's flush is a marker
  too (as on the pool), and a flag's `Ct2` there is speculated
  (`flag_ct2`), else every deferred flush runs first
  (`Coord::linearize_all`, new). `Coord::defers` (new). Engine fields
  `eval_pass2`, `eval_async_phase2`, `frozen_walk` (pass 2's ops, the
  build's phase 2 after phase 1, the frozen view of a walk);
  `engine::PlanReads` (new trait: what a prepare reads of the planned
  targets' results).

- 2026-11-08 (eval-round1; announced to eval-s3, which owns `linearize/`):
  - `bodies.rs` owns the attempt bookkeeping that lived in `driver.rs`:
    `Coord::{begin_attempt, missing_here, finish_attempt, abandon_attempt}`
    moved there (`finish_attempt` takes `FinishParts`, computed on the
    attempt's thread); pool attempts hand them to an `Inbox` (records
    sharded by node, applied whenever `Phase1Bodies::coord()` takes the
    lock) instead of taking the coordinator's lock. Anything that reads the
    store goes through `Phase1Bodies::coord()` (it does today).
  - `Phase1Cx::{defer, missing_here}` (new): a store update without the
    lock (`EvalHost::publish`, a Pkg node's roots, a lookup's source side);
    a body's `Missing` without the lock on the pool.
  - `driver::commit_roots` returns its console writes (an extension group's
    commit among the roots, below): the tape writes them at that position
    (S3: an effect at the node's position, like `Console`).
  - `linearize_with` keeps a rerun/adoption mismatch in
    `Store::rerun_mismatch` (sticky) and checks it again after applying the
    tape (a caller that ignored its result lost it).
  - `ROUNDS` (stats) carries each later round's start offset.

- 2026-11-09 (eval-t2, T2; no frozen interface changed; rebased on
  eval-round1, which does not touch these files): the artifact index's
  entries carry `provisional` (registered from a published phase-2 result,
  `ConcurrentArtifactIndex::publish_provisional`) and `superseded`;
  `ConcurrentArtifactIndex::register` takes whether the registration is a
  worker prepare's (`provisional`: it never canonicalizes); `get_entry`
  (the stamp of a provisional hit), `current`. `delta::ProvisionalRead`,
  `delta::check_reads`, `TargetDelta::reads`; `SingleFlight::{peek,
  remove}`; the memo values `ExpandedTree`/`SharedInputs` keep their fill's
  provisional reads. `engine::native_results` (new module): a native rule's
  own results as functions of a `prepare::Lookup` and `PlanReads`
  (`native_validation_in`, `inputs_in`, `native_plain_in`,
  `native_transitive_in`), called by `commit_native_target`/`prepare_target`
  and by the frozen walk's workers. `PlannedResults::publish_delta`,
  `Engine::{publish_ahead, dispatch_native, publish_native,
  check_published_native}` (`parallel/frozen.rs`).

- 2026-11-10 (eval-t3, round 3; no frozen interface changed): engine.rs's
  deferred-walk aspect planning (`plan_aspect_collection`,
  `plan_aspect_application`, `plan_aspect_attribute_dependencies`,
  `plan_aspect_dependency`) knows the dry walk of `parallel::aspects`
  (`PENDING`, `Analysis`); `source_target_with` beside `source_target_in`
  (unchanged; `eval/nodes/loading.rs` untouched); `KeyedParts::input_memo`;
  `analysis_runtime::OutputIndex` (a `Context` field).

- 2026-11-11 (task A, the sweep): `DeltaOp::Lowering` carries its
  Starlark action's id (`None`: a native lowering); a new
  `DeltaOp::Rewritten` (a rewritten delta's lowering in flight, placed
  through `PlannedNodes::enqueue_rewritten`); `Engine::commit_with_nodes`
  (each placed node passed to a hook). The sweep is on only with the T3
  merge buffer, i.e. only in frozen walks; a walk the driver keeps ordered
  (recording or tracking, task E) sees none of it. No change to the
  recording/driver side.
- 2026-11-11 (task B, aspect jobs): `Engine::plan_aspect_dependency`'s
  recursion is generic (`engine/aspect_plan.rs`: `AspectCx`, the
  coordinator's `CoordAspects`, `commit_application`);
  `Engine::split_attribute` and `analyzed_edge_configurations_in` take a
  `prepare::Lookup`; `SharedPrepare` gains `aspect_jobs` (attribute
  aspects planned over the frozen view instead of aborting) and
  `SharedRecord::aspects`; `Frozen::aspects` (`aspect_jobs::AspectTable`:
  the walk's applications and aspect-configured `Target` values,
  single-flight; phase 1's aspect tables the pass-2 ops publish, read at
  the job's position like `Frozen::platform_at`); `FrozenOutcome::aspects`;
  `SingleFlight::get` (waits for a fill in progress). Frozen walks only
  (no tracker): the recording/driver side is unchanged.

- 2026-11-11 (task E, eval-record; no frozen interface changed):
  `NodeValue::Tgt(result, observed)` (what the lookup observed, replayed
  at every use by `NodeValue::tgt()`); `observed_fs::Observations::
  {into_captured, of_captured}`, `Captured::is_recorded`,
  `observed_fs::wait_commits` (`commit_to_recorder` is applied by
  background threads; every reader of the recorder waits);
  `SourceDirProbe` is a struct (`found: DirProbe`, `recorded`: the probe's
  captured observations while recording); `incremental::TrackOp`,
  `TapeReplay::{compact, replay}` (the tape's tracker part, kept by plan
  position: `Linearizer::track`, `pass2::Pending::{track, replay}`,
  `Engine::eval_pass2_track` beside `eval_pass2_turn` at the three walk
  loops); `FrozenOutcome::accesses` (a worker's prepare's tracker
  accesses; the frozen fork carries an empty tracker as the tracking
  mark), `buffered_starlark` wraps its commit in the entry's planning
  frame (`buffered_commit`), `frozen_dependency_view` captures and replays
  its lookups for the tracker; fast native merges are off with the
  tracker; `sweep::enabled()` is false while the snapshot records;
  `Loading::source_observed`.

## 4. Stages and gates

| Stage | Work | Gate |
|---|---|---|
| S0 (done) | skeleton, measurements, classification | fmt, clippy, lib tests, ptest unchanged, release build |
| S1 (done) | A graph/scheduler/cx/cycles; B CT1 + memo bodies, inline driver; C transcript, pass 1, the tape applied (eval = bodies + pass 1) | threads=1 eval identical across all suites; EVAL_VERIFY clean on ~/code |
| S2 (1–2 wk) | parallel CT1; Pkg nodes replace prefetch.rs; CsMatch split; streaming pass 1 | identical at 1/2/8/32 threads + jitter; phase 1 ≤ 2 s; wall ≤ ~33 s |
| S3 (3–4 wk) | plan-index gating, delta cap, platform waits; native deltas; pass 2 | EVAL_VERIFY + per-node verify; linearizer ≤ 3 s; wall ≤ 15 s |
| S4 (~3 wk) | repo lane + cold tests; Aspect nodes, top-level aspects, deferred platform trees; keep_going, aquery/cquery, snapshots, affected digests | all suites in the thread matrix; ~/code identical; wall ≤ 11 s |
| S5 (1–2 wk) | delete Pipeline, the lowering queue, ui::HELD, plan_pending, … | after 2 weeks green CI and one release with `walk` available |

### S1, worker C: pass 1, the linearizer and the S1 integration

Code: `src/engine/eval/linearize/{pass1,driver,capture,cycles}.rs`,
`eval/verify.rs`, `incremental::TapeReplay`, the `Coord` store
(`nodes/bodies.rs`), `ui::{SharedWork, DeferredConsole::bypass}`.

**`BAZEL_RS_ANALYSIS=eval` (S1, as integrated)**: worker B's node bodies
on worker A's inline driver (one thread: `need` evaluates in place, the
walk's DFS) produce the transcripts; pass 1 linearizes them; the tape is
applied to the engine; phase 2 runs on pass 1's plan (`Engine::eval_phase1`,
`linearize::driver`). Requests the evaluator does not cover yet plan with
the walk (cquery/aquery captures, `--keep_going`, top-level aspects, the
cquery root checkpoint: S4).

- **Transcripts**: the coordinator keeps, per running attempt, what it
  emitted plus what the engine's coordinator services did in between
  (`Coord::drain` at every emit, begin and finish): the tracker scopes and
  the engine's own target lookups and package commits (`capture`), the
  warnings the engine pushed (`Warn`), and every console write — the
  evaluation runs inside a `DeferredConsole`; each held write becomes
  `Event::Console(i)`. A `Tgt` lookup's events belong to its user (the walk
  looks targets up inline); `Src` and `Group` are `MemoUse`d like the other
  memos. One copy: under the linearizer the store is the transcript of
  record (A's `Entry::transcript` and `Ct1Value.transcript` stay empty).
  An abandoned attempt keeps what happened anyway (console, warnings) for
  its rerun.
- **Pass 1 is resumable**: an explicit frame stack over transcripts that
  may still grow; `resume` runs as far as they go (`Progress::Blocked`)
  and continues there. It runs at each `Flush` (a config_setting with
  Starlark `flag_values`: `EvalHost::flush_pending` calls
  `Coord::linearize`) and at the end.
- **The tape's application** (`Coord::linearize`, S1's pass 2 without
  deltas): console writes and `print()` lines are written around the
  evaluation's `DeferredConsole` into the console it was started in
  (`DeferredConsole::bypass`), warnings join the engine's list, the
  incremental tracker replays the tape in its own scope context
  (`TapeReplay`), plan entries get their configured rule; at a `Flush` the
  done CT1 and memo values are published into the engine's maps (what
  phase 2 reads: `configured`, contexts, exec configurations, edge
  configurations, delegations, configured rules, empty-configuration rules,
  platforms, registrations, exec transitions, flag scopes, transition
  details, analyzed sources; the first-visit memos are written through as
  the bodies decide them) and the plan so far's phase 2 runs with the
  engine as the walk has it (configurations, tracker, direct console,
  capture paused). Plan entries that phase 2 makes itself (a deferred
  platform's tree) are external to pass 1 (complete, planned) and counted
  by the tracker replay.
- **First-visit decisions** (B's `FirstVisit`): in S1 they stay in the
  bodies, decided in the inline driver's order, which is the walk's DFS;
  pass 1 checks or recomputes what it can. S2 (parallel CT1) must move
  them:

| Decision | S1 | S2 (as built, part 2; see "S2, worker C" below) |
|---|---|---|
| CtKey variant of a key (`canonical`) | first edge in DFS order (`edge_ct_key`); pass 1 checks `platform_tree`/`forced_exec` and `same_observable` (divergences are internal errors) | the body's context only (inside a platform tree, its own claim); pass 1 decides the configured target (first reach in DFS order) and publishes it; a consumer of another variant is compared (`same_observable`: success and configured node; a divergence stays an internal error, no re-run) |
| toolchain implementation claims / `forced_exec` | `claims` at claim time, written through to `toolchain_implementations`; pass 1 recomputes them from `ToolchainImpl` (conflict error, forced check) | the body knows its own claims only; pass 1 decides the first claim in DFS order, publishes `toolchain_implementations`, and stops the build at a conflict with the walk's located error |
| toolchain / execution-platform cycle reports | the bodies; a memoized failure carrying a report from another registration's chain is reworded for the current chain (`EvalHost::reword_cycle_report`: the walk re-analyzes the failed target there) | built bottom-up (`linearize::chains`): created with no chain where detected (an in-progress check, or a back edge to the registration), each failing configured target adds itself, the registration renders it; dependency cycles likewise (rendered by the requested target's segment, with the test-trimmed root) |
| aspect memos (predicate + prints, attr aspects, toolchains, attribute configurations) | `FirstVisit`, written through | memo **scopes**, not nodes: every attempt computes them, `MemoBegin`/`MemoEnd` delimit the computation's events, pass 1 replays them at the first linearized use and skips later uses (the walk's hits; a different value digest is a divergence) |
| transition cache (+ prints) | `FirstVisit`, written through | `Transition` nodes (input registered in `EvalShared::transitions`); a failure is applied again by the requester (the walk does not memoize failures) |
| Starlark `flag_values` (CsMatch after a flush) | the flush runs phase 2 of pass 1's plan prefix inside the body | the flush still runs at the config_setting's `Flush` (its errors are the walk's there); the FeatureFlagInfo is read through a `Ct2(flag)` node computed by today's phase-2 path on the coordinator; the matching stays in the config_setting's CT1. **eval-csmatch** (below): on the pool the `Flush` is a marker the tape applies, the `Ct2(flag)` a speculative phase 2 off the sequential pass |
| `--toolchain_resolution_debug` events | printed from resolution, deduplicated process-wide | `Event::Info` through the transcript, printed and deduplicated at the linearized position |

- **Pass 1** (`pass1::Pass1`) replays transcripts depth first over the
  top-level segments (`SegmentKind`: `Setup` — the explicit
  `--platforms` resolution —, one `Root` per requested label, `RootsDone`,
  `TopLevelAspects` per label), each with an empty active chain. A Reach of
  a complete key (its body's `Done` passed) is a memo hit; otherwise the
  frame begins, the cycle check and the depth limit run, and the body
  replays with the key on the chain; `Done` pops the chain's top (as the
  walk's `active.pop()`), marks the key complete and records its
  delegation. A failed body (no `Done`) stays on the chain like the walk's
  `?` leaves it and is replayed again on a revisit (the walk re-runs it).
  A memo's transcript is replayed at its first use; a platform /
  registration memo's reaches each start a fresh chain. PkgCommit, Warn and
  Console are applied once (warnings reported before phase 1 count);
  ToolchainImpl keeps the first execution platform and reports the walk's
  conflict error. Output (`Linearized`): the plan, delegations, the
  empty-configuration rules, the reach edges (keep-going), toolchain
  implementations, warnings, pass-1 failures, each segment's final chain
  length, the first successful registered-toolchains configuration.
- **The tape** (`TapeOp`): `Segment`, `Reach{node, key, parent, enter}`,
  `Fail` (cycle/depth), `Leave`, `Effect(Event)`, `Plan{key, position}`,
  `Flush`; `pass1::TapeSink` streams it (`BAZEL_RS_EVAL_TAPE_DUMP=<file>`
  writes it).
- **Cycles**: `linearize::cycles::Pass1Cycles` is the walk-order
  `CycleHandler` for A's evaluator (used by the driver): the parked graph
  is replayed depth first from the roots in need order; a CT1 node is a
  back edge only when active in the current chain, the
  platform/registration memos open a fresh chain, any other node is a back
  edge when active anywhere. At one thread the bodies word the cycle errors
  themselves (the inline driver's chain is the walk's).
- **`BAZEL_RS_EVAL_VERIFY=1`** (`verify.rs`): `plan_build` forks the
  engine before planning (`Engine::fork_for_verify`: every pre-planning
  field cloned, the per-build state fresh, the same `Arc<RepositorySet>`),
  plans the walk on the fork with its console captured, then the evaluator
  on the engine, captured, and compares in `Compared` order: the planning
  outcome; for a successful analysis the phase-1 plan, configured nodes
  (Debug digests), toolchain/exec-group contexts and exec configurations,
  delegations, empty-configuration rules, toolchain implementations,
  configured rules (a failed walk keeps partial writes the build never
  reads); the planned actions (`request::format_action_graph`) and
  requested/result artifacts; warnings/errors and the console writes; the
  incremental index (the SNAPSHOT_INDEX_DUMP form; a difference names its
  section); the cquery capture. Process-wide state the walk leaves is
  restored first (progress counters, printed `--toolchain_resolution_debug`
  events). Writes of work shared through the repository set (repository
  rules, module extensions, first `.bzl` evaluations: `ui::SharedWork`,
  `Deferred::Shared`) happen in the walk only: when only they differ, the
  consoles agree, and the walk's console is written.
  `BAZEL_RS_EVAL_VERIFY_DUMP=<dir>` writes both consoles. B's
  `BAZEL_RS_EVAL_VERIFY_PHASE1` (the nodes after the walk's phase 1, in one
  engine) is kept for walk-mode runs.
- **Test-suite modes**: a binary built with `BAZEL_RS_DEFAULT_ANALYSIS=eval`
  (resp. `BAZEL_RS_DEFAULT_EVAL_VERIFY=1`) plans that way unless the
  environment says otherwise (the integration tests clear it).
- **Console audit**: `execute(quiet=False)`'s echo, the direct-dependency
  check's `ERROR` and a git fetch's cleanup error go through `ui::raw`
  (held/deferred like every console write; unit test). Under the
  evaluator's deferred console, held writes count as prints and private
  notes are swallowed before deferral (module extension re-probes).

### S2, worker C (part 2): pass 1 over out-of-order completions

Branch `eval-s2-pass1`. The bodies keep no first-visit state (`FirstVisit`
is gone) and write nothing order-dependent to the engine; what the walk's
order decides is pass 1's (the table above).

- **Streaming pass 1**: nodes finish in any order. A reach of a node that
  has not run, a memo use of an unrun memo, a plan entry of a body still
  running, and a memo hit of another variant whose value is not done wait
  (`Blocker`; nothing is consumed: the replay resumes at the same event);
  the delegation is recorded when the body's frame ends. `Pass1::blocked_on`
  / `Coord::frontier()` name the frontier for `Evaluator::boost` (lowest
  plan length first). A transcript pass 1 will not read again (a body past
  its `Done`, a memo other than a transition, a finished segment) is
  released and freed by the driver (`Store::release`). Random-order unit
  test: 60 random phase 1s × 4 completion orders with partial transcripts
  and released transcripts emptied give the sequential result
  (`any_completion_order_gives_the_sequential_result`).
- **Stops**: a toolchain implementation conflict and the depth limit are
  pass 1's (`Linearized::stop`, `Progress::Stopped`): the tape before them
  is applied and the build fails with the walk's text.
- **Publication** follows pass 1: the configured targets it decided
  (`take_canonical`, published once their value is done), its
  `toolchain_implementations`; the memo values (platforms, registrations,
  exec configurations, flag scopes, transition details, sources,
  transitions, aspect memos) from the store, first one wins (equal values
  per key).
- **Order-independent chain errors** (`linearize::chains`): a node's error
  text depends on its subtree only. Caveat: a memoized failure reached
  later from another chain keeps its subtree's text; in fail-fast mode the
  first error in DFS order is the build's, so this only differs from the
  walk where S1 already did (a target reached outside a registration after
  failing inside it).
- **Test hook** `BAZEL_RS_EVAL_RESTARTS=<seed>`: a node's first need of a
  key is answered `Missing` at random (one in four): attempts are abandoned
  and re-run as in the pool; the output must not change. It found two
  order dependences, fixed: (1) an abandoned attempt whose partial
  transcript pass 1 had replayed (a flush inside a child) lost those
  events — now its events up to the `Missing` stay as the rerun's prefix,
  which the rerun re-emits in place (`Coord::push_event`; the one-time
  effects of the engine's services there — a package's load output and
  commit, its warnings, a `Tgt` node's lookups and scopes — are not
  re-emitted; any other difference is an internal error); (2) the walk's
  `analyzed_sources` shortcut read completion-order state — a source
  file's lookup is now a memo scope (replayed at the first successful
  linearized use).
- **`Tgt` events** (for eval-s2-load): a `Tgt` node's captured effects go
  to the attempt that first needed it (S1: "the walk looks targets up
  inline"); with parallel completion they belong at the first linearized
  use — make them a memo (`MemoUse`, or the `Pkg` node's transcript).
- **Open** (not in this part; done in eval-csmatch below, with the
  matching kept in the config_setting's CT1): the CsMatch split proper (the matching as a
  node of its own needing the flags' CT2, so that a config_setting's CT1
  does not wait for phase 2) needs the pool's partial transcripts
  (interface changes above) and moves the flush's error wording from the
  config_setting to its consumers; parallel-order testing end to end
  (pre-evaluating roots out of order) is blocked on it (a flush inside a
  pre-evaluated body has no plan prefix yet).

**S2 part 2 results** (2026-11-08, ~/code `80d6724f3a` private clone,
release build without LTO, warm repositories, `BAZEL_RS_SNAPSHOT=off`, the
evaluator on the inline driver; host load 14–17):

| `build --nobuild //src/...` | walk | eval |
|---|---|---|
| wall | 43.9 / 48.0 s | 57.6 / 55.5 s |
| plan.start-relative phase 1 done | 35.8 / 39.6 s | 49.2 / 47.4 s |
| max RSS (process) | 8.87 / 8.89 GB | 9.30 / 9.32 GB (+4.8%; S1: +6.7%) |
| eval phase 1 (`BAZEL_RS_EVAL_STATS`) | | pass 1 2.1–2.2 s, tape application 0.17–0.19 s, publication 0.53–0.65 s, flushes' phase 2 21.8–23.1 s |

Byte-identical stderr with `BAZEL_RS_DUMP_ACTIONS=1` (6,485,168 lines)
walk vs eval, and vs eval under `BAZEL_RS_EVAL_RESTARTS=3` (2,037 s: the
hook re-runs bodies quadratically). `BAZEL_RS_EVAL_VERIFY=1` clean on
`build --nobuild //src/...`, `test --nobuild //src/...`,
`cquery 'deps(//src/rust/...)'`, `affected --base=HEAD^` (32 GB memory
limit: two engines). Test suite (ptest): walk 3679/3679, eval
(`BAZEL_RS_DEFAULT_ANALYSIS=eval`) 3679/3679, eval with
`BAZEL_RS_EVAL_RESTARTS=7` 3679/3679.
### S2, part 1 (eval-s2-load): loading nodes and the pool

Branch `eval-s2-load` (rebased on main `f397e89a`, eval-s2-pass1 merged).

- **Integration with pass 1's part 2**: the rerun prefix lives in the
  per-thread running attempts (`bodies::push_running`: a rerun re-emits its
  abandoned attempt's events in place, one-time effects excepted — now also
  `LoadPrints`; a difference is the internal error, `bodies::rerun_mismatch`).
  A parked attempt's events stay visible to pass 1 (`Store::carry`) until
  its rerun replaces them; an attempt the pool leaves to the sequential
  pass (a swallowed sentinel, a flush) keeps only its rerun prefix.
  `active_ct1` is empty on pool threads. `Ct2` is coordinator-only
  (help-first leaves such nodes to the coordinator); memo values are
  published by the linearizer on the coordinator. Each pool round boosts
  pass 1's frontier (`Coord::frontier`). A `Tgt` node emits nothing of its
  own (its lookup's `Access` and its package's `MemoUse` are the user's),
  so its effects are the `Pkg` memo's.

- **Loading nodes** (`nodes/loading.rs`): `Pkg`, `Tgt`, `Src`, `Group`
  over `EvalShared` replace the engine's loading services (package
  loading, target lookups, package groups, source visibility, package
  args, implicit tests, registration pattern expansion, aspect
  definitions, module execution platforms); the engine's helpers became
  free functions over the repository set (`settle_load_unit`,
  `source_target_in`, `expand_registration_patterns_in`,
  `load_aspect_definition`, …) so the walk is unchanged. A Pkg node's
  transcript is its commit (`LoadPrints`, `PkgCommit` in the engine's
  order); every lookup `MemoUse`s its package (once per attempt, never for
  a package loaded before phase 1); the tape's `PkgCommit` commits the
  package into the engine, its `LoadPrints` are written with the walk's
  `.bzl` deduplication. A package group's include graph is resolved in its
  Group body (the walk's recursion). Phase-1 prefetch is the walk's only.
- **The pool** (threads = `--loading_phase_threads` bounded by the cores,
  > 1): `EvalConfig::help_first` — a `need` of a node nobody runs claims
  and runs it in place, depth first (a restart only when a dependency runs
  on another worker); `Cx::hint` (the edge loop's dependencies,
  `Host::hint_edges`) spreads the work; an attempt that parks waits for
  everything it hinted. 512 MiB worker stacks, arena binding (A's
  `build_pool`). Running attempts are per thread (help-first nesting);
  `active_ct1` is empty on a pool thread.
- **Two passes**: the pool evaluates what the requests need
  (`warm_up`: the lookups, then the configured roots); what only the
  sequential order can do parks on the `INLINE` marker (`Missing::Repo`,
  the lane does nothing for it): a flush (a config_setting with Starlark
  `flag_values`), a repository sentinel (the repository is fetched ahead
  by repository speculation), an order-dependent decision; cycles and
  back edges park too. Then the walk's order drives the inline driver over
  the same graph (`drive`), which runs what is parked and finds the rest
  done; after a requested target whose sequential pass did work, the pool
  runs again (`pool_round`: unresolvable nodes retried, waiters an inline
  completion woke scheduled).
- **Repositories**: the lane materializes a module repository's root a
  unit was blocked on (`lane_fetch`; what it writes and observes is the
  re-run unit's); extension preparation and direct repositories go to the
  sequential pass (their recorded-input keys depend on the order:
  `phase1_prefetch`'s recorded-input tests), with speculation started for
  the pool. Roots a pool unit obtained count from where the tape replays
  the node; the sequential pass of a pooled evaluation applies the tape
  before each attempt (the roots, commits and observations the walk would
  have by then).
- **Observations**: in a pooled evaluation every attempt observes in its
  own `observed_fs` frame (`Bodies::Attempt`), committed where the tape
  replays the node (entered, first used); an abandoned attempt's frame is
  dropped.

Results (2026-11-08, ~/code `80d6724f3a`, LTO release binary, warm
repositories, `BAZEL_RS_SNAPSHOT=off`, `build --nobuild //src/...`, one run
each, the host loaded by other work; action dump, stdout and stderr
identical in all four):

| | walk, default threads | eval, default threads (48) | eval, 1 thread | walk, 1 thread |
|---|---|---|---|---|
| plan.start → phase 1 done | 36.6 s | 58.9 s | 75.1 s | 57.5 s |
| wall | 54.2 s | 76.6 s | 102.6 s | 83.6 s |
| CPU (user+sys) | 139 s | 191 s | 105 s | 86 s |
| maxrss (with the action dump) | 11.1 GB | 11.2 GB | 11.5 GB | 11.2 GB |

Of eval's 58.9 s: the flushes' phase 2 23.4 s, pass 1 1.9 s, publication
0.6 s, the tape 0.5 s. The pool alone does little on ~/code: its first
round completes 14k of 462k nodes in 0.1 s, then the roots wait for
config_settings with Starlark `flag_values` (their flush is the
sequential pass's) and the sequential pass runs most of the graph; the
later rounds help (75 → 59 s). **The S2 gate (phase 1 ≤ 2 s) needs the
CsMatch split** (the flag's CT2 as a node, the matching without a
flush), then the pool can take the subtrees below the config_settings.
`EVAL_VERIFY` clean on `build`/`test --nobuild //src/...`.

### S2, the CsMatch split (eval-csmatch)

Branch `eval-csmatch` (main `08c7174b`). A config_setting with Starlark
`flag_values` keys no longer waits for the sequential pass:

- **The flush is a marker.** On the pool `EvalHost::flush_pending` emits
  `Event::Flush` and continues (no linearization in the body). Pass 1
  replays it where the walk flushes; the tape's `Flush` runs phase 2 of
  the plan so far there (`Coord::flush`, unchanged), so the console order,
  the flush's phase-2 work and its errors are the walk's. The inline
  driver (one thread, and the sequential pass of a pooled evaluation)
  flushes in the body as before (S1's baseline).
- **The flag's `Ct2` off the sequential pass** (`nodes::speculate`,
  `eval_host::flag_ct2`): the providers the planning state holds once a
  flush planned the key; otherwise a **speculative phase 2**: the flag's
  subtree as a fresh walk analyzes it (`Coord::subtree`: a pass 1 over
  the subtree's transcripts with one `Reach` segment; what a flush or an
  earlier speculation planned is imported instead of replayed — its
  phase-2 entries (`dry`, providers, provider keys, runfiles, validation,
  native output groups, transitive repositories) copied from the planning
  state; the registration and platform memos' declarations are skipped),
  its plan entries prepared, computed and committed on a **scratch
  engine** (`Scratch`: `fork_for_verify` of the engine before the pool
  runs, sequential planning, the subtree's CT1 values and the published
  memo values and packages published into it, the configurations absorbed
  from `EvalShared`). Only the providers are kept: console writes,
  observations, captures, `--toolchain_resolution_debug` events and the
  progress counters of the run are dropped (`speculate::isolated`,
  `capture::isolated`); the workspace status is never computed there
  (`speculate::speculating`). A run that cannot be speculated (a
  transcript freed, a repository sentinel, a failure anywhere in the
  subtree, `--profile`) leaves the `Ct2` to the sequential pass (the
  `INLINE` marker), which flushes as the walk does. A failed run discards
  the scratch engine (the next one forks anew).
- **Verification.** The FeatureFlagInfo a config_setting read is compared
  with the planned one by the flush that plans the key
  (`Coord::verify_speculated`; at once if planned already): a difference
  is an internal error naming `BAZEL_RS_ANALYSIS=walk`.
- **Released transcripts** are freed only once a flush planned them
  (`Store::delay_release`, a pooled evaluation): the subtree replay finds
  a node either planned (imported) or with its transcript.
- **A flush that fails in the tape** (`FlushFailure`, sticky: later
  linearizations fail with it): when its config_setting runs in the
  sequential pass, the body receives the error as in the walk
  (`flush_failure_delivered`); when the pool finished it (it read its
  flags speculatively), the driver asks the walk for the error text
  (`Coord::flush_failure`): pass 1 records where each `Flush` happens
  (`FlushPoint`: segment and chain), the walk re-analyzes the segment's
  requested target on the published values, the chain's nodes left out
  (its memos unpublished), and its `plan_pending` fails with the flush's
  error at the config_setting (`Engine::eval_flush_failure`), so every
  consumer on the chain words it as the walk does ("errors encountered
  resolving select() keys for …"); its effects are dropped. A requested
  target whose evaluation failed still linearizes first: a failing flush
  earlier in the walk's order is the build's error. So the error wording
  does not move: the matching stays in the config_setting's CT1.
- **The driver.** The sequential pass evaluates requested targets with
  `Evaluator::evaluate_values` (no O(nodes) outcome scan: 3 scans of
  461k nodes per requested target were 7.6 s of ~/code's phase 1); the
  pool runs again after a requested target whose sequential pass ran
  attempts (`bodies::SEQUENTIAL_ATTEMPTS`; was: took ≥ 5 ms, which every
  flush did). The done CT1 values (`Ct1Done`, 64 shards) are read without
  the coordinator's lock (3.45 M → 1.0 M acquisitions on ~/code).
- Tried and dropped: resolving the sequential pass's blockers one at a
  time with a pool round after each (it ran the pool ahead of a failure —
  `parallel_fetch`'s cancelled speculation — and loaded a blocked package
  before its demander's rerun had observed its prefix —
  `phase1_prefetch`'s obtained-root keys); the inline driver evaluating a
  fresh CT1's subtree on the pool first (449 sequential CT1 attempts on
  ~/code: no gain).

Diagnostics: `BAZEL_RS_EVAL_STATS=1` adds the later pool rounds (nodes of
the first 16, time), the driver's split, the coordinator lock's
contention and the speculations; `BAZEL_RS_EVAL_SPECULATE_DEBUG=1` names
each speculation (its time, plan entries, FeatureFlagInfo or why it was
left); `BAZEL_RS_EVAL_PARKED_DEBUG=1` the parked nodes after the first
round and the sequential attempts by kind.

Tests: `tests/eval_csmatch.rs` (a transition setting the flags, select()
on them, defaults, a FeatureFlagInfo flag, a flag derived from another
through a select() — nested flushes in the speculated subtree —, a flag
read by many config_settings in many packages, a toolchain's
`target_settings` on a flag, the flag's implementation failing, an
unrelated phase-2 failure planned before a flush) at 1/2/8/32 threads,
with jitter and `BAZEL_RS_EVAL_RESTARTS`, stderr (with the action dump)
and stdout byte-identical to the walk.

**Results** (2026-11-08, ~/code `80d6724f3a` private clone, release
without LTO, warm repositories, `BAZEL_RS_SNAPSHOT=off`, `build --nobuild
//src/...`, 48 threads, two runs each, host load ~14):

| | walk (main) | eval (main) | walk (branch) | eval (branch) |
|---|---|---|---|---|
| plan.start → phase 1 done | 36.4 / 36.1 s | 56.3 / 56.4 s | 35.7 / 35.2 s | 39.4 / 38.7 s |
| wall | 47.1 / 46.4 s | 66.8 / 67.3 s | 46.0 / 45.3 s | 50.0 / 49.5 s |
| max RSS | 8.96 / 8.97 GB | 9.65 / 9.61 GB | 8.97 / 9.04 GB | 9.72 / 9.81 GB |

Nodes per pool round (eval, branch): the first round 14,126 of 461,579
(75 ms), then #2 295,738 (2.4 s), #3 73,064, #4 7,532, #5 1,371, #6
11,185; 450 loads, 449 CT1 and ~12k lookups left to the sequential pass;
294 speculative phase 2s (383 scratch plan entries, 0 left to the
sequential pass, 1.0 s, 0.8 s of it holding the coordinator's lock). On
main: the first round 14,118, flushes 3,826 left to the sequential pass.
Of the branch's 38.7 s: the linearizer 27.2 s (the flushes' phase 2
23.5 s, pass 1 2.5 s, publication 0.66 s, tape 0.58 s), the pool rounds
5.5 s, the sequential pass's own attempts ~5 s.

**What remains for the S2 gate (phase 1 ≤ 2 s).** (1) The first round is
no longer stopped by flushes but by packages that need a module
extension's preparation or a direct repository (`prepare:ordinary` of
rules_distroless `apt`, gazelle `go_deps`, aspect_rules_js `pnpm`,
rules_rust, …: order-dependent recorded inputs, the sequential pass's);
the root package `//:` itself needs `go_deps`, so the second round starts
only after the first requested target's sequential pass. (2) The flushes'
phase 2 (23.5 s) still runs inside phase 1 on the coordinator, as the
walk's `pending_flush` does: it is phase-2 work the walk does there too;
moving it out of phase 1 is S3's pass 2 (per-node deltas merged in plan
order; the speculative phase 2's scratch engine then gives way to the
flag's own CT2 delta merged at its plan position). (3) The pool scales
poorly past ~16 threads: the coordinator's lock (begin/finish of every
attempt: 0.36 M contended acquisitions, 47 s waited across threads) —
the store's transcripts per thread would remove it. (4) The linearizer
runs between rounds; overlapping it with the pool (the engine out of the
coordinator's lock while a flush plans) would hide the rounds' 5.5 s.

### S3, pass 2 (eval-s3)

Branch `eval-s3` (main `dc344493`). Code: `eval/linearize/pass2.rs`,
`eval/linearize/driver.rs` (`Coord::flush`, `catch_up`, `linearize_all`),
`engine/parallel/frozen.rs`, `engine::PlanReads`.

**Deferred flushes (on by default in a pooled evaluation).** A flush no
longer runs phase 2 inside phase 1. At a `Flush` of the tape the
configured targets pass 1 decided are published at once (phase 2 reads
them by key, a dependency only after it was analyzed; the work ahead of
its turn needs them); everything else the tape applies from there on —
the memo values the flush publishes (platforms, registrations, exec
transitions, sources, transitions, aspect memos), console writes,
`print()` lines, warnings, `--toolchain_resolution_debug` events, load
prints — is kept as a `pass2::Op` by the plan position before whose turn
the ordered walk applies it: effects between flush k-1 and flush k come
after the phase 2 of the plan up to flush k-1. Package commits stay
eager (the engine's packages: work ahead of its turn reads them; phase 2
loading a package itself is the case the walk already skips). Pass 2 is
then one walk over many flushes' segments (`Coord::catch_up`), each op
applied at its turn by `Engine::eval_pass2_turn` (the pipelined walk
places every lowering before a due op: a deferred segment's failure is
found in its segment). A failure inside a deferred segment is that
flush's (`FlushFailure` with its `FlushPoint`, worded by the walk's
re-analysis as before); the ops after it are dropped. What forces the
deferred walks to run (`linearize_all`): a body that needs the planning
state (a flag the sequential pass cannot speculate), a failure of a
requested target (an earlier flush's failure comes first), pass 1's stop,
the end of phase 1. The scratch-engine speculation of a flag's
FeatureFlagInfo stays (verified against the planned value after the walk
that plans it); the sequential pass speculates too.

A flush stays **ordered** (its walk runs at once, after every deferred
one) when the order would show (counted, `BAZEL_RS_EVAL_STATS`):

| Reason | Why | ~/code `build //src/...` |
|---|---|---|
| it publishes a new platform | work ahead of its turn would see the platform deferred where the ordered walk sees it resolved (ctx surface, execution table); the deferred segments are walked before it is published | 5 (the first 22,397 plan entries) |
| its segment holds a Starlark rule or genrule whose configuration's target platform is unresolved | its phase 2 may resolve the deferred platform, whose tree the ordered walk plans at the flush (`Engine::eval_pass2_resolves_platform`: an internal error if it happens in a deferred segment anyway); the no-config configuration's rules (licenses, package metadata) have none to resolve | 0 (68 before the no-config exception) |
| `--profile`, `BAZEL_RS_EVAL_NO_DEFER=1` | the profile records phase 2 and the tape in their order | 0 |

(Until eval-record the tracker and the observation recording kept every
flush ordered too: "eval-record" below.)

Memory: the transcripts are freed at the end of phase 1, before the
deferred walk (they were held until a flush planned them: the
speculations' subtree replays).

**Prepares on the workers (`BAZEL_RS_EVAL_FROZEN_PREPARES=1`, off by
default).** A pass-2 walk can prepare its Starlark entries on the
analysis workers while the coordinator commits (`parallel::frozen`):
the workers read a frozen view — the engine forked when the walk starts
(phase 1 is complete), the configured nodes, the planning state's
dependency views / `Target` identities / native output groups (moved in
for the walk; the walk's fills are single-flight memos shared with the
coordinator's own views, one value per key) and the committed targets'
results (`PlannedResults`, the planning state's per-target entries copied
when an entry commits, ahead of its turn or at it, before its dependents
wake; `PlanReads` is the read interface of a prepare, over the planning
state's maps or these). One job prepares, computes and builds the delta;
a prepare that needs the coordinator (a package not loaded, an attribute
aspect) aborts and the entry is prepared at its turn; the turn of an
entry still on a worker commits others ahead while it waits. Output
identical on the suites (`tests/eval_pass2.rs`, `=all`: every walk) and
~/code. It is off because it does not pay on ~/code: the coordinator's
prepares (~11 s of its CPU) leave it, but the walk is bound by its **turn
chain** (below), and the fork costs ~0.4 s and ~0.5 GB.

**Results** (2026-11-08, ~/code `80d6724f3a` private clone, LTO release,
warm repositories, `BAZEL_RS_SNAPSHOT=off`, `build --nobuild //src/...`,
48 threads, two runs each, alternating, host load 16–18):

| | walk (main) | eval (main) | walk (branch) | eval (branch) | eval (branch, frozen prepares) |
|---|---|---|---|---|---|
| plan.start → phase 1 done | 37.4 / 35.3 s | 39.8 / 40.0 s | 36.3 / 37.7 s | 36.7 / 34.8 s | 37.5 s |
| of it: the flushes' phase 2 (eval) | | 24.8 / 24.4 s | | 21.2 / 20.0 s (5 ordered walks + the deferred walk) | 20.6 s |
| plan.start → phase 2 done | 43.1 / 41.9 s | 46.3 / 46.5 s | 42.4 / 43.6 s | 43.1 / 40.9 s | 45.3 s |
| wall | 47.4 / 46.3 s | 50.7 / 50.9 s | 46.6 / 48.1 s | 47.4 / 45.2 s | 50.9 s |
| max RSS | 8.93 / 8.90 GB | 9.75 / 9.72 GB | 8.95 / 8.97 GB | 9.69 / 9.64 GB | 10.47 GB |

Phase 1 proper (the evaluation, before the deferred walk) is ~15 s; pass
2 (~20 s plus the ~6 s tail) is one walk instead of ~3,800. Eval is 3–5 s
faster than on main and on par with the walk: the walk's phase 2 is the
same work in fewer walks. Byte-identical stderr with
`BAZEL_RS_DUMP_ACTIONS=1` (6,486,167 lines) walk vs eval for `build` and
`test --nobuild //src/...`; `BAZEL_RS_EVAL_VERIFY=1` clean on both and on
`affected --base=HEAD^`. Suites (ptest): walk 3697/3697, eval
(`BAZEL_RS_DEFAULT_ANALYSIS=eval`) 3697/3697; with frozen prepares on every
walk (`BAZEL_RS_DEFAULT_EVAL_FROZEN_PREPARES=all`) 3695/3697 — the two are
instrumentation expectations (`parallel_rule_implementations`
`action_graph_and_prints_are_the_sequential_ones` asserts the batch
prepares ran; `plan_timings` stage coverage), not output.

**Where pass 2's time goes** (`BAZEL_RS_TIMING=1`, `PERF-PARALLEL
coordinator split`, the one walk of ~110k entries, frozen prepares on):
Starlark entries at their turn 14.5 s (~3,600 entries not committed
ahead: waiting for their worker 5 s, prepared at the turn 5 s, computed
at the turn 4.5–5.5 s, their deltas built here 3 s), natives committed
ahead 3.0 s, commits ahead 2.0–2.4 s, classification 1.7 s, the ops'
turns 1.2–1.4 s (the lowering drains before them), natives at their turn
0.9 s, the rest ~5 s. The entries at their turn are chains through the
commit order: an entry is ready only when its requirements are
**committed**, and a commit ahead of the turn waits for every earlier
entry of a related package whose outputs are unknown (`unsettled
package`: ~/code's `npm_package_store_internal` targets share one package
and commit almost in plan order), behind attribute-aspect barriers, the
committed budget and an unresolved platform. Raising the budget
(`BAZEL_RS_COMMITTED_LIMIT=65536`) gains ~2 s.

**What remains for S3** (in order of yield):

1. Readiness on results, not commits: a dependent may prepare as soon as
   its requirements' CT2 results exist (their worker finished), with
   their artifacts registered at their stamp; the merge stays in plan
   order. The hazard is a shared action or conflict with an earlier entry
   whose outputs were unknown (the producer of the later one's artifacts
   changes): its dependents' deltas (and the view memos built from it)
   must be discarded and redone at the merge — the invalidation the
   pipeline avoids today by waiting.
2. Native rules as deltas on the workers (91k of ~/code's 133k entries
   commit ahead on the coordinator: ~4 s; their prepares and views too):
   `commit_native_target` writes the planning state directly.
3. Then the coordinator only merges (~1.4 s at ~94k merges/s; the 2–3
   thread split of a batched merge applies at a hook position, where
   nothing reads the maps).
4. Overlap pass 2 with phase 1's pool rounds: the pool's attempts barely
   use the engine (warnings in `Coord::drain`, the speculation's planned
   imports), so a walk could run while a round runs if `Coord` did not
   hold `&mut Engine` across rounds (eval-round1's coordinator-lock work).
5. The lowering drain before an op (1.2 s): attribute a failed lowering to
   its plan entry instead.

### S2, round 1 and the coordinator's lock (eval-round1)

Branch `eval-round1` (main `dc344493`).

**Extensions on the pool.** A pool package load (`Pkg` body,
`crate::repository::EvalAdoption`) that needs an extension group the
coordinator has not prepared uses the group's finished speculative
preparation (docs/COLD_ANALYSIS.md "Module extensions prepared ahead"):
`RepositorySet::eval_adopt_group`, through the existing chained-adoption
hook (`adopt_speculated_group`, the generated repositories' roots,
`note_obtained` for a `.bzl` cache hit in such a repository). Used only when
the speculation finished and its lockfile entry is still reusable
(`eval_adoption_check`: the commit's check without its effects, once per
group; chained groups likewise). The load records the group as
`EVAL_GROUP_KEY + id` first among its obtained roots; where the tape
replays the node (`commit_roots` → `note_committed_roots`) the group is
prepared — the speculation taken and committed, verified as today —
before its repositories count, so the group's commit happens where the
walk's load prepares it, and the order-dependent sets
(`sequential_roots`) are the walk's. The prepared group must be the very
preparation the pool used (`check_eval_group`, also when the sequential
pass prepares it first): otherwise an internal error naming
`BAZEL_RS_ANALYSIS=walk` (`EVAL_ADOPTION_MISMATCH`). The output-file
checks of the package run under the same guards (a sentinel there leaked
into the value before).

**The lane resolves what such a load waits for** (`nodes::loading`,
`lane_fetch`): a group whose speculation is running, queued, not
requested (a group the root module does not use) or blocked
(`\u{2}extension:` blocker: requested and waited for, at most 10 s; what
it is blocked on is resolved — another group's speculation, a repository
below, a registry module root materialized on the lane as `lane_fetch`
does, its writes the waiting unit's), and a repository executed ahead by
repository speculation for the load (`\u{2}local:` blocker,
`eval_local_blocker`/`wait_eval_local`): a `local = True` rule (never
speculated before) when it calls no `execute()` (`repository_ctx`
refuses it there: a subprocess may write anything; it stays the
sequential pass's), or a repository of a group the pool uses
speculatively (its speculation adopts the group too). Its root is
published as any speculation's (not when it printed), counting where the
tape replays the load. Never while local executions are captured for
replay (`BAZEL_RS_SNAPSHOT` on/record: `RepositorySet::state.replay`).

**Single-flight snapshot verification** (`repository_store`,
`verify_snapshot_shared`): with the loads on the pool, 47 threads'
recorded-input checks nested-probed the Go SDK's root (17,377 files) at
once (nested probes never wait for each other); a tree's verification now
runs once and the others take its result (+60 s CPU, round 2 2.7 → 5 s
before the fix).

**The coordinator's lock out of begin/finish/abandon**: pool attempts
push their records into the `Inbox` (64 shards by node; a node's records
in order, its attempts never overlap); the carried events of an abandoned
attempt are kept per node for its rerun (`take_carry`), the store gets
them for pass 1 with the records; the done CT1 value is published to the
sharded `Ct1Done` at once. The coordinator (any thread taking
`Phase1Bodies::coord()`) applies the records first.

**Residual (documented, tested)**: the files an adopted group's recorded
inputs name must not change during the invocation before the group's
commit — as for every warm-reuse probe (`sequential_roots`). A local rule
whose `execute()` rewrites such a file in phase 1, after the pool's use
and before the walk's first demand of the group, is the internal error
(`an_input_rewritten_in_phase_one_after_the_pools_use_is_an_internal_error`);
a rewrite before phase 1 (the target pattern phase) fails the use check
and changes nothing.

**Review fixes (eval-round1, second round).**
- A `.bzl` module the pool loaded through an adopted group sits in the
  shared cache: a hit by a load that does not adopt (the sequential pass's
  package before the adopter in walk order, a loading worker) would skip
  the group's preparation. `RepositorySet::note_obtained` (now called
  before a hit is served) prepares the group there on a thread that may
  demand (`root()`, as the walk's load), or notes `EVAL_GROUP_KEY` and the
  root in the unit (no demand), counting a sentinel (an evaluator attempt
  other than a package load leaves itself to the sequential pass).
- The mismatch check runs only at a used group's commit
  (`commit_eval_group`): an attempt discarded after its use, or a
  repository speculation's use, is no mismatch. When the walk prepares a
  used group otherwise, what the pool loaded from the speculative
  preparation leaves the shared caches (`evict_dropped_group`: the `.bzl`
  modules of its repositories and their loaders, transitively, the
  load-graph memo, the ignored subdirectories). The use check compares a
  materialized registry root with the one the speculation used.
- A group key among the roots of an attempt the tape replayed before it
  finished is not committed on the thread applying the record: an internal
  error (fail closed; never expected: only package loads adopt).
- A local rule the lane asked speculation for: the coordinator's own
  evaluation of its root takes that speculation (waiting while it runs),
  and a speculation gives up while the coordinator evaluates the root —
  never two executions. A rule refused for `execute()` is not queued again;
  the lane waits at most 10 s per blocker. No local rule runs ahead while
  any replay capture is installed (checked at use).
- Residuals: a published local root's workspace or host reads can go stale
  if an earlier repository's `execute()` writes them (as for any
  non-local speculation); the module events of a discarded pool attempt
  of a package (kept for its rerun) may name a module re-evaluated after
  such an eviction (its lines are printed once, the first version's).

**Not done: overlapping the linearizer with the pool rounds** (item (4)
of "What remains" above): it runs in `driver.rs` beside eval-s3's pass 2,
which moves the flushes' phase 2 out of the coordinator's linearization
(the overlap's only large part); left to S3.

Results (2026-11-08, ~/code `80d6724f3a` private clone, release without
LTO, warm repositories, `BAZEL_RS_SNAPSHOT=off`, `build --nobuild
//src/...`, interleaved, host load 10–57):

| | main (dc344493) | eval-round1 |
|---|---|---|
| pool round 1 | 14,106–14,129 nodes, 77–118 ms | 14,645–14,668 nodes, 0.29–0.59 s (the lane waits for speculations) |
| round 2 | 295k nodes, 2.6–3.1 s | 306–308k nodes, 2.9–3.0 s, starts at +2.7–2.8 s |
| coordinator lock (48 threads) | 615–631k uncontended, 344–360k contended, 42–55 s waited | 102–104k uncontended, 0.7–1.3k contended, 2.4–4.7 s waited |
| coordinator lock (16 threads) | 257–271k contended, 15.7–16.3 s waited | 0.9k contended, 2.7–2.9 s waited |
| plan.start → phase 1 done | 44.2 / 44.2 s | 41.2 / 42.3 s |
| CPU (user+sys) | 199 / 201 s | 188 / 197 s |
| max RSS | 9.71 / 9.72 GB | 9.71 / 9.72 GB |

What still stops round 1 on ~/code: `rules_java++toolchains+local_jdk`, a
local rule that calls `execute()` (the toolchain registrations' package:
1,586 CT1 nodes wait for `RegToolchains`), so the first requested
target's sequential pass still runs ~17k attempts (2.3–2.8 s) before round
2. Making that rule's execution available ahead needs its `execute()` to
be known side-effect free (e.g. a allow-list of read-only probes) — not
done.

**S1 results** (2026-11-08, ~/code `80d6724f3a`, LTO release binary
sha256 `272b5f78…`, warm repositories, `BAZEL_RS_SNAPSHOT=off`, default
loading threads, the evaluator at one thread):

| | walk | eval |
|---|---|---|
| wall (`build --nobuild //src/...`) | 47.4–53.5 s | 62.3–64.9 s |
| plan.start → phase 1 done | 36.1–38.4 s | 50.7–50.8 s |
| hwm | 8.9 GB | 9.5 GB |
| with the incremental tracker (snapshot save) | phase 1 54 s, hwm 9.8 GB | phase 1 69 s, hwm 11.3 GB |

Eval phase 1 (`BAZEL_RS_EVAL_STATS=1`): the flushes' phase 2 24–27 s (the
walk's `pending_flush` 23 s), pass 1 1.7–2.0 s, publication 0.55 s, the
tape's application 0.2 s; the rest (~23 s vs the walk's ~14 s outside its
flushes) is the bodies on the inline driver (`config_conditions` 20.1 s vs
15.5 s).

Byte-identical to the walk (stderr without timing lines, stdout,
BAZEL_RS_DUMP_ACTIONS; the incremental index): `build --nobuild` and
`test --nobuild //src/...`, `cquery 'deps(//src/rust/...)'`,
`cquery //src/python/... --output=jsonproto` (the walk plans cquery in S1),
`affected --base=HEAD^` (digests), each under `BAZEL_RS_ANALYSIS=eval` and
`BAZEL_RS_EVAL_VERIFY=1`. Test suite: walk 3661/3661, eval 3661/3661,
verify 3647/3661 — the 14 are the harness's two plannings in one process
(snapshot recording counts and status-command runs, the allocation report,
the cquery root checkpoint given to the evaluator only, a once-per-process
lowering-failure hook, the hidden lockfile seeding, the definition `.bzl`
cache's print replay across two engines), none a pass-1 difference.

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

Since branch `pre-analysis-x` (docs/COLD_ANALYSIS.md, "Pattern loading:
blocked units re-run on the workers"): X = 2.09–2.51 s with the LTO binary
(main 2.80–4.61 s on the same host the same hour); `select.discovered` →
`select.loaded` 1.68 s. Blocked units re-run on the workers once the
coordinator satisfied their demand (~600 re-runs, in waves); the
critical path is the coordinator's inline re-runs up to the `npm`
extension (~0.93 s), then `//src/ts` on a worker (0.73–0.86 s).

What would cut X (to ~1 s):

1. ~~Bulk repository demand for pattern loading~~ (done in
   `pre-analysis-x`, as pipelined re-runs: 2.6 → 1.7 s; the batch shape
   is limited by the demands only the coordinator satisfies, in order).
2. ~~Evaluate the module extensions the lockfile names in parallel at
   startup~~ (done in `parallel-extensions`: the root module's groups
   are prepared ahead and committed at their demand; X median 2.30 →
   1.96 s, docs/COLD_ANALYSIS.md "Module extensions prepared ahead").
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

### Merge throughput after the fixes (branch eval-merge-throughput)

Fixes (a)–(c) and two found by the per-operation profile, all in
`delta.rs`/`engine.rs`; the merge's semantics and order are unchanged:

- **Artifact registration**: the shards of `ConcurrentArtifactIndex` are
  `hashbrown::HashTable`s hashed by the FxHash of the File's path (stored in
  the entry: growth never rehashes); `register_artifact` is one hash, one
  write lock and one probe (`ConcurrentArtifactIndex::register`: an entry
  visible to the reader is kept and checked, one registered ahead of its
  turn is replaced with the earlier stamp, else inserted). A delta's
  `DeltaOp::Artifact` carries the hash (computed on the worker) and the
  merge moves its File and Artifact in (no clone).
- **Producers**: `DeltaOp::Output` carries a `HashedPath` — the path, its
  `parallel::path_hash` and its proper ancestors' hashes, computed on the
  worker. `Producers::insert_hashed` feeds them to `PathHashes`, to the
  ancestor set (a `HashTable` of `(hash, path)`: an ancestor path is
  allocated only when new, where it used to be allocated per ancestor
  visited) and to the paths table (`HashTable<ProducerEntry>`).
  `delta_meets_producers` uses them too (`meets_producer_hashed`).
- **Declared outputs**: `Declared` is a `HashTable` keyed by `DeclaredKey`
  (configuration, path, hash = path hash ^ the configuration digest's first
  word); `Path`'s component-wise `Hash` of the old `(ConfigId, PathBuf)` key
  was most of the cost. `DeltaOp::Declared` carries the key.
- **Action executions**: an FxHashMap by action id (lookups only) instead of
  a B-tree whose insert compared ~17 cold ids.
- **Presizing**: phase 2 sizes `dry` (a plan entry each), `custom_providers`,
  `provider_keys` and `runfiles_info` (a Starlark rule target each) from the
  plan before it walks (`ActionPlanning::presize`; at most what growth
  reaches). Presizing the artifact index, producers and declared from the
  plan's size measured no gain and was dropped; so was a stored-hash table
  for the action executions.

Measured with the release build, ~/code `//src/... build --nobuild`,
`BAZEL_RS_EVAL_REPLAY_BENCH=1`, three runs each (main ada4ba6d → branch):

| | main | branch | |
|---|---|---|---|
| real `merge_delta` (38,757 merges) | 18.0–18.6 µs, 54–56k/s | 10.6 µs, 94–95k/s | 1.75× |
| stub replay | 12.7–14.8 µs, 67–79k/s | 6.4–6.9 µs, 146–156k/s | 2.1× |
| stub replay, presized | 11.7–12.0 µs, 84–86k/s | 5.5–6.0 µs, 168–183k/s | 2.0× |
| extrapolated all-delta (timed per-op, noisy) | 132–145k/s | 179–259k/s | |

Per operation now (timed replay, timing overhead ~0.1 µs/op included):
artifact 0.60 µs × 4.0, output 0.63 µs × 2.8, declared 0.23 µs × 4.0,
providers 1.2 µs, action execution 0.41 µs × 2.3, dry and runfiles info
0.5 µs each. What remains is mostly cache misses on large cold tables
(probe + entry write per map, and the configured-target key's clones for
the four per-target maps). The real merge is slower than the replay
because the coordinator's caches are cold between merges.

**Splitting the merge across threads (d), not done.** The maps a merge
writes are independent, and within each map only the order of the
operations on one key matters: the artifact index per File (kept, replaced
or inserted, and the re-registration check), producers per path (the last
registration of a path wins; `first_below` takes the least path whatever
the order), declared per key (replace vs keep), the per-target maps per
key, the action executions per id; the planned nodes, lowerings and their
console marks are one ordered sequence. So three threads — the artifact
index | producers, `PathHashes` and declared | the per-target maps, nodes
and the engine's tables — each applying its operations of the batch's
deltas in merge order give the sequential result, as long as nothing reads
those maps before the join. That holds for pass 2's batched merge at a
`Flush` (S3), not for today's walk, which merges one delta at a time
between reads (a thread hand-off costs as much as a merge). Expected from
the per-op costs above: ~2.4 | ~2.8 | ~3.2 µs per merge plus partitioning
the operations by kind, ~1.5–1.7× on a batch.

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

## 8. T1: order coupling audit

Branch `eval-t1-audit` (main `c4bcf177`). The plan "compute in any order,
merge in plan order": a target's phase 2 (prepare + compute + delta) runs
on the evaluator pool as soon as CT1(K) and the phase-2 results of its
dependencies, toolchains and aspects exist — not gated on plan order or on
commits — and a plan-ordered merge buffer applies the deltas in pass 1's
order (first wins, conflict errors, console, tracker and dump unchanged).
That is sound only if phase 2 reads nothing another target's **merge**
writes, other than through its declared dependencies' values. T1 classifies
every read of `prepare_custom`/`prepare_target`, `compute_target`,
`DeltaBuilder::custom_target`, `commit_custom_target`,
`commit_native_target` (+ `commit_native_instrumented`), the dependency
views (`plan_dependency_view` → `dependency_view_in`), aspect applications
(`plan_aspect_collection` → `plan_aspect_application`) and toolchain
loading (`load_toolchain_context`): **(A)** a dependency's value (the
dependency's, toolchain implementation's or aspect's phase-2 result, the
target's own CT1, a phase-1 memo node) or **(B)** state another target's
merge writes.

### The reads

(A), no change but the source: `PlanReads::{dry, providers, provider,
provider_keys, runfiles_info, validation, repositories}` are keyed by the
dependency's (or toolchain implementation's) configured target key
(`shared_artifacts_in` for a rule target, `dependency_view_in`,
`planned_validation_in`, `dependency_repositories_in`,
`load_toolchain_context`, the template variables of `prepare_custom`, the
natives' `artifacts`/`inputs`/`planned_validation`/`custom_providers`/
`runfiles_info` reads, an alias's forwarded providers); the phase-1 maps
(`configured`, `rule_delegations`, `configured_rules`, `toolchain_contexts`,
`exec_group_contexts`, `exec_configurations`, `group_exec_configurations`,
`edge_configs`, `aspect_attribute_configs`, `aspect_toolchains`,
`empty_configuration_rules`, `toolchain_implementations`); packages and
targets (`cx.target`, the transition allowlist's package group, genquery's
scope, a test_suite's implicit tests, starlark_doc_extract's module); the
configurations. Today they are read from the engine's published maps; under
T2 they become need edges (`Ct2`, `Ct1`, `Pkg`/`Tgt`/`Group`, memo nodes).
The last three phase-1 maps are pass-1 decisions (first visit): available
once pass 1 has linearized K (or computed on the variant and checked with
`same_observable` at the merge, as pass 1 does for CT1).

(B):

| # | Read (file: function) | What it reads | Differs with the merge order? | How it becomes (A) / where it goes |
|---|---|---|---|---|
| B1 | engine.rs `ConcurrentArtifactIndex::get` via `DeltaBuilder::{lookup, artifact}` (action inputs, DefaultInfo `defaults`, runfiles support entries and executable, `SharedInputs` and tool-runfiles group fills), `commit_native_target` (a genrule tool's executable), `indexed_artifact` (a filegroup's `output_group` files) | File → Artifact; visible by stamp (`turn`/`stamp_ahead`/`view_artifacts`) | **Producer only**: a shared output's Artifact names the first owner's action (`output_artifact`'s `first`); a different-label owner's File (a distinct entry) names the winner, a same-label cross-configuration File is one entry registered by the winner. Source Artifacts and every other field are functions of the File. Visibility is readiness, not value | An index without stamps filled when a phase-2 **result** is published (each File by its declaring owner, which is in the reader's closure: Files reach a target only through providers and views); provisional producer = the owner's own action; canonicalized at the merge (T3 alias map) |
| B2 | engine.rs `shared_artifacts_in` (Output target) → `PlanReads::declared(config, path)` | the output-file label's Artifact in the generating rule's configuration (`rule_delegations`) | No in value (the generating rule predeclares it: `replace`; another writer of the path with `replace` is a conflict); its producer as B1 | Read the generating rule's `Ct2` value (its predeclared outputs) instead of the global map; the loser's `Declared { replace }` is decided at its merge |
| B3 | engine.rs `check_output`/`register_output`, delta.rs `DeltaBuilder::register_output` (`Combined` over the build's producers), `delta_meets_producers`, `PathHashes::{meet, meet_earlier}`, parallel.rs `Pipeline::{unsettled, meets_known}` | the producers of output paths, written by earlier merges | **Yes**: who is first (New / Shared / conflict), the `Shared` ops, the dropped nodes, the conflict text | Merge-time only (T3): the merge checks every `Output` op against the producers; a loser's delta is rebuilt there; `PathHashes`, the unsettled-package rules and the commit-ahead barriers go away |
| B4 | engine.rs `plan_aspect_collection` / `plan_aspect_application` (inside the first dependent's prepare), `aspect_providers`, `aspect_target_identities` | an aspect application computed — actions registered, console owner, prints — by whichever dependent prepares first | Value no (a function of the base target and its dependencies); **placement yes** (its actions, owner and prints land at the first preparer) | `Aspect(AspectApplicationKey)` nodes with their own delta, merged right before the first dependent in plan order (the walk's position) |
| B5 | engine.rs `prepare_custom` (`custom.target_platform`, `ctx_surface`), `plan_aspect_application`, `commit_native_target` (genrule surface), delta.rs `Engine::remote_platforms`, parallel.rs `platform_resolved` | `Engine::platforms`: a deferred default target platform resolved by a phase-2 commit (`deferred_target_platform`), an execution platform analyzed yet | Value no (an implementation that asks gets NEEDS_TARGET_PLATFORM and is computed again with it; `remote_platforms` decides only where the delta is built); **placement yes**: the platform's tree is planned at the first asker's commit | `Platform` node edge for the value; the tree's plan entries at the first asker's merge (the ordered flush rule of S3 stays) |
| B6 | engine.rs `ensure_workspace_status_with` (via `DeltaBuilder::ensure_status`, `indexed_artifact`, a stamped genrule), delta.rs `resolve_build_info` | the workspace status planned on first need | Value no (`Artifact::workspace_status`); placement of its nodes yes | Merge-time: the first merged delta that needs it plans it (workers fall back today: 12 deltas) |
| B7 | delta.rs `SingleFlight` memos `input_groups` (tool runfiles trees), `input_memos` (memoized depset walks, keyed by the walk's address — kept alive by the process-wide flat memo) | Artifacts looked up by the first filler | Producer only (B1) | Keep (single-flight); fill from the provisional index; lowering of a contested reader at its merge |
| B8 | engine.rs `views` (`DependencyViews`), `target_identities`, `native_output_groups` (and the frozen walk's `BatchMemos`) | memos keyed by the dependency's key, filled by the first consumer | No: functions of the dependency's values; a view keeps Files only (B1's Artifacts are converted with `Artifact::file`) | (A) memos; keep single-flight |
| B9 | `console_owners`, `aquery_owner`, `note_analysis_print`, warnings, the tracker's accesses (`SharedRecord`) | writes | — | Merge-time, at the entry's position (as today) |
| B10 | `compute_target`'s repository demand (NEEDS_REPOSITORY, re-run at the commit slot) | whether a repository is fetched | Placement only | `Missing::Repo` on the lane, as in phase 1 |

**No producer choice reaches providers or views.** `Artifact` is private
to `crate::engine`; providers are frozen Starlark values, `DependencyView`,
`TargetRunfiles` and depsets are defined outside it and hold `File`s; a
`File` has no producer (`starlark_types.rs` `FileData`: path, short path,
owner, is_source, is_directory, is_symlink) and `Artifact::file` depends on
the producer's presence, never its id. The tripwire test
`engine::tests::file_names_no_producer` destructures `FileData` without
`..` (a new field fails it) and checks that two owners' Artifacts of one
shared output give the same File. The Starlark compute reads only its
snapshot (views, rule, configuration): order-free.

### Measured (`BAZEL_RS_ORDER_AUDIT=1`)

`src/engine/order_audit.rs`: hooks at the merge (`DeltaOp::Shared`, the
coordinator's `register_output`), `shared_artifacts_in`,
`dependency_view_in`, the target-platform reads, `deferred_target_platform`,
`ensure_workspace_status_with`, `plan_aspect_collection` and
`commit_custom_target`, and a scan of the planning state at the end of phase
2: a **contested reader** is a planned target (action owner) whose result
names a shared action's winning id — its nodes' generated inputs, its
shared input groups, its files to build (`dry`), its runfiles support's
artifacts — the winner itself excepted (an upper bound: a reader through the
winner's own chain is counted too). `BAZEL_RS_ORDER_AUDIT_LIST=1` lists the
winners and readers. Off: the hooks are behind one `LazyLock` flag; dumps
identical (below).

~/code `80d6724f3a` private clone, `build --nobuild //src/...` (walk; `test
--nobuild` gives the same numbers), fast release build:

| | |
|---|---|
| plan entries | 133,156 |
| shared output registrations (contested Files: a later owner's File whose Artifact names the winner) | 47,761 |
| shared actions (winners) = contested paths | 12,958 |
| owners that shared (losers) | 464 |
| action / prefix conflicts | 0 |
| **contested readers** | **836 (0.63%)**: 68 losers, 768 others |
| — through their nodes' own inputs / shared input groups / `dry` / runfiles support | 215 / 26 / 621 / 117 (only through `dry`: 621, i.e. no action of theirs reads it) |
| — references | node inputs 189,003, group inputs 2,544, dry 180,587, runfiles 73,508 |
| winners reading their own shared outputs | 5 |
| dependency views built from a contested Artifact (File kept only) | 180,824 of 1,234,986 |
| output-file label resolutions (`declared`) | 440 (401 paths), contested 0 |
| target-platform reads | 41,396 (1,363 deferred); platforms resolved by phase 2: 0 |
| workspace status planned on first need | 1 (`--nobuild`: no status nodes, 0 readers) |
| aspect applications planned inside a dependent's prepare / memo hits of other dependents | 1,777 / 64,793 |
| Starlark deltas: worker / coordinator / rebuilt because an output met a producer / workspace status | 35,885 / 2,514 / 346 / 12 |
| today's waits this removes (PERF-PARALLEL) | unsettled package 19,758, enclosing 461, enclosed 1,434, outputs 68; `Pipeline` parks ~21.7k |

Every shared action is aspect_rules_js's: npm package stores of different
packages (`.aspect_rules_js/node_modules/<pkg>`) sharing their dependencies'
symlink/copy actions, and `copy_to_bin` of one source by many targets of a
package (`//src/ts` 4,992 winners, `//src/website3` 4,523, `//src/ts/neui`
1,608, `//src/vscode/beyondwork` 881, …). The readers are the stores'
`node_modules/<pkg>` link targets (450, `dry` only), the stores (120), and
the binaries/tests/`*_runfiles`/`*_js_info_files` above them.

**Gate: met.** Contested readers 0.63% ≤ ~1%; no producer choice in
providers or views (static: type privacy + tripwire; measured: the 180,824
contested Artifacts views read reach them as Files). Every other (B) read is
value-neutral (B4–B8) or merge-time already (B3, B9).

Behaviour off: `BAZEL_RS_DUMP_ACTIONS=1` stderr (6,486,167 lines, the
elapsed-time line dropped) identical for main's installed binary
(`c4bcf177`), the branch with the audit off and on (the `ORDER-AUDIT`
lines dropped).

### T2: phase 2 as an order-free node

`Ct2(CtKey)` runs prepare + compute + delta on the pool. Its needs:
`Ct1(K)`; for every dependency edge (label, edge configuration) the `Ct2`
of the dependency (after the alias chain and delegation, as
`dependency_view_in` keys it); for an output-file edge the `Ct2` of the
generating rule in its delegated configuration; the `Ct2` of each resolved
toolchain implementation; the `Aspect` nodes of its edges' aspect
collections; `Platform` of its target platform (value only); `Pkg`/`Tgt`/
`Group` for its lookups; `Missing::Repo` for a repository. Its value: the
results its dependents read (dry, providers + keys, runfiles info,
validation, transitive repositories, the predeclared outputs' Artifacts)
and the delta (ops + prints + console owner), plus `platform_read`.

- **Reads → edges.** `PlanReads` gets a node-backed implementation (the
  needed `Ct2` values; `PlannedResults` of the frozen walk is the shape);
  `declared(config, path)` becomes the generating rule's value (B2); the
  phase-1 maps become `Ct1`/memo node reads (the pass-1 decisions from pass
  1's output, or checked at the merge).
- **The artifact index** loses its stamps (`turn`, `stamp_ahead`,
  `view_artifacts`, `ConcurrentArtifactIndex::register`'s replace-ahead):
  a `Ct2` publishes its outputs' **provisional** Artifacts (producer = its
  own action id) when its result is published; a same-label File registered
  by two owners keeps either (both are canonicalized at the merge). Source
  Artifacts are computed from the File; the status Artifacts are fixed values.
- **Native rules as delta builders.** `commit_native_target` (genrule,
  filegroup, alias, test_suite, genquery, starlark_doc_extract, platform
  providers, java_plugins_flag_alias) and `commit_native_instrumented`
  write the planning state directly; they become a
  `DeltaBuilder::native_target` emitting `Providers`, `Dry`, `RunfilesInfo`,
  a new `Validation` op, genrule/genquery/doc-extract `Output` (native
  producer), `Artifact`, `Declared` and `Node` ops — the same order as
  today's writes — so the 91k natives committed ahead on the coordinator
  (~4 s) move to the pool. Their `inputs()` check ("different artifacts map
  to the same execution path", comparing producers) runs on canonical
  producers: in the delta build when no input is provisional-shared, else at
  the merge.
- **Aspects**: `plan_aspect_collection`'s applications become `Aspect`
  nodes (their own prepare/compute/delta over the base target's `Ct2` and
  their dependencies'); the dependent reads their providers (the merged
  view is a function of node values).
- **Stays merge-time**: output registration (B3), the workspace status (B6),
  the deferred platform's tree (B5), console/owners/tracker (B9).

### T2 as built (eval-t2)

Branch `eval-t2` (main `2e929159`). Code: `parallel/frozen.rs`
(publication, native jobs, the Ct2 profile), `native_results.rs` (new),
`delta.rs` (`ProvisionalRead`, `check_reads`), `engine.rs` (the index's
provisional entries, `commit_custom_target`'s check), `parallel.rs` (the
pipeline's readiness on published results).

**Where `Ct2` runs.** In this step `Ct2(K)` is the frozen walk's job
(S3's `dispatch_frozen`: prepare + compute + delta on an analysis worker,
`BAZEL_RS_EVAL_FROZEN_PREPARES=1`), not yet an evaluator node: its prepare
reads `&Engine` (the published phase-1 maps), so it runs over the frozen
view after phase 1, where every pass-1 decision it needs
(`empty_configuration_rules`, `forced_exec`/`toolchain_implementations`,
the canonical variant, `rule_delegations`) is published. Moving it into
phase 1 needs the prepare's engine reads as node reads (a `PrepareCx` over
`Ct1`/memo values; open). What changed is its gating:

- **Readiness on results, not commits.** A Starlark entry is ready when
  each requirement (`ahead_requirements`: dependency edges after the alias
  chain / output-file generating rule, toolchain implementations) is
  committed **or published**; a worker's finished job publishes its results
  at once (`Engine::publish_ahead`): `PlannedResults::publish_delta` (the
  delta's `Providers`, `Dry`, `RunfilesInfo`, `Declared`, the transitive
  repositories) and its `Artifact` ops as **provisional** index entries
  (B1, B2). Not published: a sentinel (B10), a failed compute or a delta
  with an error, a delta not built (workspace status, B6; build-info; an
  unknown execution platform), an implementation that asked for a deferred
  target platform (B5). The merge republishes the planning state's entries
  over the provisional ones.
- **Native rules' results on the workers.** `native_results` splits the
  part of `commit_native_target` that computes a native rule's own results
  (the transitive `_validation` group, `inputs`, the platform / feature
  flag / forwarded providers, filegroup and test_suite runfiles, the
  transitive repositories) into functions of a `prepare::Lookup` and
  `PlanReads`: the commit calls them over the planning state (its output
  is unchanged: same code), a worker of the frozen walk over the published
  results (`Engine::dispatch_native`, once the rule's requirements are
  published or committed; a filegroup selecting an `output_group` reads its
  `srcs`' views there). Its results and its `inputs`' registrations are
  published like a Starlark rule's; its commit stays on the coordinator
  (reading committed state only), where `check_published_native` checks
  the published results against the committed ones (Files, provider ids —
  their values under `BAZEL_RS_VERIFY_DELTA` and in debug builds —,
  runfiles, validation, repositories; a difference is an internal error
  naming `BAZEL_RS_EVAL_NO_PUBLISH=1`). Not on the workers: genrule,
  genquery, starlark_doc_extract (outputs: their commit registers
  producers), java_plugins_flag_alias, package_group, coverage, cquery;
  their dependents wait for their commit.
- **Provisional reads (B1, B7).** A provisional entry is visible to later
  plan indices, not to its publisher's own builds (a rebuild registers it
  again), and only a merge's registration replaces it (a different
  canonical artifact — a shared output's loser naming the winner — is kept
  as `superseded` for re-registrations of the stale copy). A worker's
  delta records every read of a provisional entry and every generated File
  it found unregistered (`TargetDelta::reads`); the tool runfiles groups
  and shared input runs keep their fill's reads (users inherit them; a
  value whose reads are stale is dropped and filled again). At the merge
  `check_reads` compares them with the index: stale → the delta is rebuilt
  there from a recompute (`provisional_stale`, the contested readers of
  §8); a commit ahead of its turn waits until the publishers merged
  (`provisional_pending`, parked on the publisher) and until what it became
  ready on is committed (`provisional_dependency`), so a coordinator build
  never reads a provisional entry. Producers stay merge-time (B3:
  `delta_meets_producers` and the recompute, unchanged).
- **Bounds.** The pipeline's horizon (4,096 entries past the turn) and
  held bound (768 entries prepared ahead) stay the defaults (memory);
  `BAZEL_RS_T2_HORIZON` / `BAZEL_RS_T2_HELD` (hidden) widen them.
- `BAZEL_RS_TIMING=1`: `PERF-PARALLEL ct2` (the Ct2 completion profile:
  when each job finished relative to its walk's start, when its turn came,
  the lag), `PERF-PARALLEL provisional published= natives= stale_rebuilt=
  with_reads= memo_refilled=`.

**Review fixes (eval-t3, first part).** Silent-divergence holes of the
provisional index, each with a unit test (two same-path owners, late
publication):

- A published result makes only its **predeclared** outputs visible as
  `declared` (`DeltaOp::Declared::predeclared`): a same-package sharer of
  the generating rule's action (rules_js `copy_to_bin`) publishing after it
  no longer hands readers of the output-file label its own Artifact.
- A **source** File read as missing is recorded too, and a late
  publication by an earlier entry lowers the stamp of an occupied
  producer-less entry (as that entry's merge would); `check_reads` reads
  the entry now (`current`, provisional included: no `reader - 1`
  off-by-one) and treats a read entry stamped at or after the reader
  (inherited from a memo a later entry filled) as stale.
- A worker prepare's registration never chooses a producer: it inserts or
  flips an entry only for a producer-less artifact, never replaces a
  canonical later-stamped entry with another artifact; a vacant generated
  File there is an internal error in debug builds and under
  `BAZEL_RS_VERIFY_ARTIFACTS`.
- `check_published` at every published key's successful commit, Starlark
  rules too: files to build, provider ids and keys, runfiles
  (`TargetRunfiles::same_published`: identity, else each set's contents),
  validation and repositories (natives); provider values by identity, else
  by repr — always for an alias, in debug builds and under
  `BAZEL_RS_VERIFY_DELTA` otherwise.
- A successful frozen walk fails with an internal error if an index entry
  is still provisional (one scan).
- A worker's memo fill that resolved a File through its own unpublished
  registrations (recording no read) is dropped from the memo after its use.

### T3: the plan-ordered merge buffer

Deltas land in a buffer by plan index in completion order; the cursor
merges the next one as soon as it is present (`merge_ops`, batches split
over 2–3 threads where nothing reads between, §6). At each merge:

1. **Producers** (exact, as today): each `Output` op checked against the
   build's producers. A conflict is the build's error at that position
   (**fail-fast stop**: the evaluator switches to frontier-only — the
   existing `first_error` mode —, the buffer beyond is dropped; with
   `--keep_going` recorded and continued). A `Shared` registration makes the
   delta a **loser**: it is rebuilt here from its kept `AnalysisResult`
   (no Starlark re-run; today's `meets_producer` path recomputes, 346 on
   ~/code, ~1 s): shared actions plan no node, its outputs' Artifacts name
   the winner, `Declared` keeps the winner's entry unless predeclared; and
   the **alias map** records `loser action id → winner id`.
2. **Contested readers**: a delta lists the foreign action ids its
   Artifacts name (built with it, a few per delta); if one is in the alias
   map, its lowering and native input checks run here on canonical ids
   (the delta is rebuilt from its kept result, or its `Node`/`Dry`/
   `RunfilesNodes` ops rewritten through the map when it has no
   producer-comparing check). **No cascade**: the rewrite changes only that
   delta's Artifacts; its dependents computed from Files (providers, views)
   are unaffected, and their own Artifacts are canonicalized at their own
   merge — later in plan order (postorder), when every alias of their inputs
   is known (the winner precedes the loser, the loser precedes its readers).
   ~/code: ≤ 836 + 464 entries (≤ 1%).
3. Effects at the position: prints, console owners, aquery owners, tracker,
   the workspace status on first need, an aspect application's delta before
   its first dependent, the deferred platform's tree.

**Buffer cap and boost**: unmerged deltas ≤ 16,384 (§1); beyond it only the
cursor's missing entries and what they need run (`Evaluator::boost` on the
cursor's `Ct2` and, transitively, its waits; deferred nodes otherwise).

### T3 as built (eval-t3)

Branch `eval-t3`. Code: `parallel/buffer.rs` (new), `parallel/frozen.rs`
(the jobs as functions of the frozen view, genrule results, the results
kept across walks), `engine.rs` (`Shared`), `eval/verify.rs`
(`fork_frozen`), `lowering.rs`.

**The merge buffer** replaces the pipeline in a frozen walk (every eval
pass-2 walk of ≥ 2,048 entries; `BAZEL_RS_EVAL_FROZEN_PREPARES=0` off,
`=all` every walk; `BAZEL_RS_EVAL_NO_BUFFER=1` the T2 pipeline):

- **Classification on the workers**, 64 entries per job, for the window
  ahead of the cursor (16,384, `BAZEL_RS_T3_WINDOW`; 262,144 since task B): a Starlark rule's
  requirements (`ahead_requirements` over the frozen view: dependency edges
  after the alias chain, output files' generating rules — looked up in the
  frozen packages —, resolved toolchains), a native rule's whose results a
  worker computes (`publishes_native`, now including genrules: their
  outputs, declarations and data runfiles); attribute-aspect rules,
  package groups and coverage natives are the cursor's at their turn.
- **Dispatch on publication**: an entry whose requirements all have
  results (published by a worker or committed) is queued; its job (a
  Starlark rule's prepare + compute + delta, a native rule's results)
  publishes its own results and readies its dependents (`Board::mark`).
  The queue takes the lowest plan index first.
- **Per-entry stage** (idle, classifying, queued, running, done, claimed):
  the cursor takes a done job, waits for a running one, runs a queued one
  itself (`stolen`), and prepares an idle one at its turn (`buffered_at_turn`:
  prepare + compute there, as the walk).
- **The cursor only merges**: `commit_turn` (the producers' exact check;
  a delta meeting a producer, or with stale provisional reads, is rebuilt
  there — from the kept analysis result when the worker left it, else
  computed again), the pass-2 ops at their positions, the effects the
  jobs held (console, observations, roots) applied in plan order. A plain
  native rule whose results a worker published is merged from them
  (`merge_published_native`: artifacts resolved through the index, the
  published files to build reused when no producer changed;
  `BAZEL_RS_VERIFY_DELTA` / `BAZEL_RS_VERIFY_AHEAD` / `--profile` /
  `BAZEL_RS_T3_NATIVE_COMMIT=1` commit instead, checking them). The first
  error in plan order stops the walk (fail fast; later finished jobs held
  their effects: dropped; a worker's panic is raised at its turn).
- **Declarations**: a merge republishes a declaration only as an
  output-file target's when it is one (`frozen::predeclared`): another
  target's earlier declaration of the path no longer hides the generating
  rule's published one (`shared_actions` `write_output_first`, reachable
  under T2's frozen walks too).
- What the buffer makes unnecessary in the eval path: commit-ahead
  classification and retries (`Pipeline::unsettled`, barriers, parks),
  Starlark at the turn, the turn-stamped prepares (merges are at the turn).
  The walk keeps the pipeline unchanged.

**The frozen view without a fork**: the phase-1 tables the workers read
are `Shared<T>` (an `Arc`, copied on the first write while shared:
`Arc::make_mut`), as are the configured nodes; `fork_frozen` shares them
and leaves out phase 2's own tables (console owners, action executions,
warnings, errors, BEP groups, run plans). The committed results
(`PlannedResults`) are kept in the planning state from one frozen walk to
the next (each catches up with what was committed since), and a finished
walk's view memos keep no `view_artifacts` (every entry merged). Freezing
took 0.35 s + 0.94 s and ~0.6 GB on ~/code; now ~0 s and ~0 MB.
`PERF-SHARED copies` (`BAZEL_RS_TIMING`) names the tables a write copied
(`walk:` during a frozen walk).

**Scheduling**: 8 workers for frozen walks (the cursor bounds the walk;
48 workers gave the same walk time and +0.35 GB of concurrent computes),
`--loading_phase_threads / 3` lowering workers (4–16; the op positions
drain the lowerings: hooks 1.85 → 0.35 s).

**Diagnostics** (`BAZEL_RS_TIMING=1`): `PERF-PARALLEL buffer` (classified
by kind, dispatched, stolen, cursor waits and µs, at-turn entries, fast
natives), `buffer at_turn` (by why and rule class), `buffer classified_turn`,
`buffer cursor` (the cursor's time at each tenth of each walk, also as
`PERF … pass2.cursor` marks with the RSS), the coordinator split's
`cursor_claim`, `merge_starlark`, `merge_native`, `at_turn`, `fast_native`.
`BAZEL_RS_T3_DEBUG=1`: the 40 slowest cursor steps per walk with the chain
of what readied each entry.

**Measured** (2026-11-09, ~/code `80d6724f3a` private clone, LTO release,
warm repositories, `BAZEL_RS_SNAPSHOT=off`, `build --nobuild //src/...`,
default threads, alternating runs, host load 10–19):

| | main eval | main eval, frozen + T2 wide bounds | T3 eval (default) | main walk | T3 walk |
|---|---|---|---|---|---|
| plan.start → phase 1 done | 36.6 / 34.1 s | 30.4 s | 24.9 / 24.1 s | 35.2 s | 35.0 s |
| phase 2 done (process time) | 45.2 / 42.5 s | 37.5 s | 31.0 / 29.6 s | 43.2 s | 42.8 s |
| wall | 47.6 / 44.7 s | 39.9 s | 33.4 / 32.0 s | 45.4 s | 45.1 s |
| CPU (user+sys) | 175 / 188 s | 207 s | 186 / 192 s | 135 s | 131 s |
| max RSS | 9.69 / 9.63 GB | 10.71 GB | 9.94 / 9.94 GB | 8.93 GB | 8.89 GB |

The big deferred walk (96,694 entries) takes 9.9 s on the cursor (T2:
16.6 s): the Ct2s are published at p50 2.4 s / p99 8.4 s, the cursor's
time at each tenth 1.5/3.3/3.5/4.3/4.5/4.6/5.8/8.4/9.1 s. Of its ~10 s:
waits for a running job 3.9 s (chains through entries the cursor
commits: attribute-aspect rules — rust_clippy / rustfmt_test 2.5 s at
their turn —, unpublished filegroups, rules_js npm stores whose deltas
meet a shared action's producer and so publish nothing), Starlark merges
2.1 s (of it ~1.1 s rebuilding 439 such deltas from their kept analysis),
native merges 1.9 s + 0.5 s published natives, publication 1.0 s.

Identical output: the action dump (6,486,165 lines) of `build` and `test
--nobuild //src/...` for main's walk, T3 eval and T3 walk;
`BAZEL_RS_VERIFY_DELTA=1 BAZEL_RS_VERIFY_ARTIFACTS=1` clean (38,070 worker
deltas verified); `BAZEL_RS_EVAL_VERIFY=1` clean on `build`, `test
--nobuild` and `affected --base=HEAD^`. Suites (ptest): walk 3735/3735, eval
3736/3736; frozen walks everywhere (`BAZEL_RS_DEFAULT_EVAL_FROZEN_PREPARES=all`,
also with `BAZEL_RS_EVAL_RESTARTS=7`) 3730/3736 — the six are the
pipeline's instrumentation expectations (`plan_timings` stage coverage,
`parallel_rule_implementations`' commit-ahead and shared-prepare counters),
not output. `tests/eval_buffer.rs`: shared outputs and a conflict where the
loser finishes first, a reader of the loser; an output-file label whose
path another target declared first; a fail-fast error mid-plan with the
later entries finished; prints across buffered merges — each at 2/8/32
threads, with jitter, restarts, a window of 3 and `BAZEL_RS_VERIFY_DELTA`.

**Not done**: the 2–3-thread split of a batched merge (merges are one at
a time at the cursor; natives are interleaved with Starlark deltas, so a
batch without reads in between is short); contested readers rewritten
through an alias map instead of rebuilt (publishing a loser's delta made
~540 readers stale at their merge: +6 s, so a delta meeting a producer
publishes nothing and is rebuilt from its kept analysis); attribute
aspects and package groups as jobs; `Aspect` nodes (T2c).

### T3, round 2 (eval-t3, after the review)

- **Memory** (~/code peak 9.94 → 9.36–9.37 GB, same host): phase 1's
  evaluator graph and the speculations' scratch engine are dropped before
  the deferred walks; jemalloc's dirty pages are purged
  (`alloc_arena::purge`, `arena.<all>.purge`) once they are freed and
  after phase 1's structures go; a published delta sharing an output keeps
  only what its merge's rebuild needs.
- **Native rules' sources**: a native rule's worker resolves the declared
  source files its lookups miss (`SharedPrepare::resolve_sources`: the
  package's declared inputs, `source_target_in` without the memo), the
  lookups' records (memoized path, missing files, source directories)
  applied at its merge; the 11 large filegroups that published nothing
  (python runtimes, `npm_files`, pip `:srcs`) now publish and merge as
  published (`native_unpublished` 11 → 0, native merges 1.9 → 0.9 s).
- **Shared outputs**: a delta whose output path an earlier entry computed
  or registered (`SharedView::meets_earlier`) is built anyway and its
  results published, its shared outputs naming the earlier entry's
  artifact (the index's: `PlannedResults::publish_shared`; none registered
  yet → unpublished as before); its analysis result, runfiles support and
  outputs check are kept and the delta dropped, so its merge rebuilds it
  from them as before (no implementation run). Its dependents no longer
  wait for its merge.
- Not done: attribute-aspect rules (rust_clippy, rustfmt_test: ~2.7 s at
  their turn) as worker jobs — their prepare applies aspects over the
  dependency tree, writing the planning state (the `Aspect` nodes of T2c);
  the 2–3-thread merge split. The cursor's remaining waits (~5 s) are
  chains of computes readied by their last dependency (each link a
  compute's latency) and long single computes started late by the
  lowest-index-first queue.

### T3, round 3 (eval-t3): scheduling, natives, aspects

- **Critical-path scheduling**: every entry is classified when the walk
  starts; its job is queued `BAZEL_RS_T3_CHAIN_WEIGHT` (256) entries
  earlier per Starlark implementation in its longest chain of dependents;
  ready entries beyond the window are parked until the cursor reaches
  them. Neutral on ~/code (the remaining waits are not queue order).
- **Natives**: a worker resolves each source file once per job, reading
  the engine's directory probes and one link policy per repository
  (`source_target_with`; `RepositorySet::link_policy` canonicalized the
  store per symlinked source, 2–9 ms each).
- **Action keys** hash the inputs in content-defined blocks, a memoized
  depset walk's blocks once (`shared_actions::MemoBlocks`): the same key
  with or without the memo; rules_python's PyCompile action per source no
  longer hashes the runtime's 1.6k files each.
- **Attribute aspects at their turn** (`parallel/aspects.rs`): a Starlark
  rule with attribute aspects (rules_rust's clippy, rustfmt) stays at its
  turn, but before its prepare the coordinator walks its aspect-applied
  edges *dry* — `Engine::plan_aspect_dependency`'s recursion building
  each application's `AspectApplication`, which is queued as a job
  instead of analyzed (`PENDING` up the walk; siblings walked on). The
  jobs run on the walk's workers and the cursor, isolated like a
  compute; as each finishes, the dependencies that waited for nothing
  else are walked again (an analyzed application's providers stand for its
  result, a planned dependency is walked once per turn). Nothing commits
  while dry (no aspect memo, action, print). The prepare then replays: each
  application takes its analysis from the table and commits in the
  sequential order (first-wins and the aspect memo unchanged: the memo is
  filled by the prepare only). An analysis that failed, panicked or had
  effects is redone by the prepare at its point. Aspects whose analyses
  are cheap (< 1 ms on average) are analyzed inline in the dry walk, and a
  turn whose aspects are all cheap is not prefetched. `BAZEL_RS_T3_ASPECTS=0`
  turns it off; `BAZEL_RS_VERIFY_DELTA=1` analyzes every replayed
  application again and compares; `PERF-PARALLEL aspects`. ~/code: clippy
  turns 1.6 → 0.53 s, rustfmt 1.0 → 1.1 s, the big walk's cursor 9.5 →
  8.5 s.
- Not done: a full `Aspect` node (applications off the coordinator's
  planning walk altogether); `RegisteredAction.inputs` sharing the
  memoized walk (about 33 µs per PyCompile action cloning 1.6k `Arc`s;
  ~60 readers of `inputs`).

### Phase 1, measured and cut (eval-phase1)

Branch `eval-phase1`. "Phase 1 proper" below is plan.start →
`plan.eval_graph_dropped` (the evaluation, before the deferred walks of
pass 2, which are T3's).

**Instrument: `BAZEL_RS_EVAL_TIMELINE=1`** (`src/eval_timeline.rs`;
diagnostics only, off by default): marks with the process CPU since the
previous mark (rounds, each requested target's evaluation and
linearization, every pass-2 walk, assist rounds, slow sequential attempts
≥ 20 ms, slow pool attempts ≥ 50 ms, slow lane fetches, probe waits ≥
50 ms), the cores in use per 250 ms, the graph's queued/running/waiting
counts per 100 ms (serving), the pool attempts' self time by kind per
250 ms, the sequential pass's attempts by cause (the node's state before:
fresh, waiting on a node — with the chain of first unfinished
dependencies to what blocks it —, left by the pool with the sentinel's
subject, queued, a lane blocker) and by root cause (a fresh node inherits
its parent's), occupancy of the speculations, the scratch engines, `.bzl`
claim, probe-claim and speculation waits, and the coordinator lock's
contended waits by call site. `BAZEL_RS_EVAL_WATCHDOG=1` prints every 5 s
what the sequential pass waits for, the running nodes and what each
thread blocks on (it found the deadlocks below).

**Where phase 1's ~13–15 s went** (main `ce83252b`, ~/code `//src/...`,
warm, snapshots off, 48 threads; plan.start-relative):

| Span | Time | What |
|---|---|---|
| round 1 | 0–0.48 s | 14.7k of 461.6k nodes; everything else waits for `RegToolchains`, which waits for `Pkg(@rules_java++toolchains+local_jdk)` (a local rule calling `execute()`: left to the sequential pass) |
| requested #0's sequential pass | 0.48–2.48 s | 17,246 attempts on one core: local_jdk 0.1 s, then the woken 1,587 CT1s' subtrees inline (`Pkg` fresh 769 attempts 2.5 s self, CT1 fresh 9.3k 0.9 s, Tgt 24k 0.3 s) |
| round 2 | 2.48–6.05 s | 307k nodes, 54 core-s; cores 20–35, then 5–15 for the last ~1.5 s |
| later sequential passes | ~2.4 s | #8 0.6 s (2.3k attempts), #148 1.5 s (29.8k), #1023 0.5 s, small ones; root causes: `RegToolchains` woken subtrees, `apt`/`go_deps`/`pnpm`/`i` extension sentinels on `Tgt` lookups (left by the pool), pool waiters woken by inline completions |
| later rounds | ~1.4 s | 69k, 7.5k, 1.4k, 7.4k nodes |
| linearization | ~4.6 s | pass 1 2.6 s (5.9 M events), tape 0.5 s, publication 0.7 s, the new-platform ordered walks 0.9 s; serial, between rounds (the pool idle) |

CPU: ~1 core in use outside the rounds (≈ 8 of the ~15 s). The sequential
pass's own work that the pool could not do was small (1 `Pkg` left by
the pool, 0.1 s; 8 `Tgt`s): the rest was its depth-first runs of fresh
or woken subtrees, plus pass 1.

**What changed** (in order of the measured gain):

1. **The pool serves beside the sequential pass** (`Evaluator::serve`,
   `BAZEL_RS_EVAL_NO_SERVE=1` falls back to rounds): after round 1 the
   pool's coordinator runs on a thread of its own and never stops at
   quiescence; the sequential pass (this thread, marked
   `loading::set_sequential`: its attempts are the sequential pass's
   whatever `Loading::pooled` says, `Loading::pooled_here`) claims nodes
   by CAS (`claim_inline`) and waits while the pool makes progress on
   what it needs (`pool_progresses`: the chain of first unfinished
   dependencies reaches a queued or pool-running node, or a lane
   blocker); a fresh CT1/Pkg/Transition/Platform/registration node is
   handed to the pool first; the end of the waited chain is boosted.
   Inline completions schedule their waiters at once; an inline attempt
   that parks registers as its dependencies' waiter
   (`park_inline_on_nodes`); a back edge is a node on this thread's
   `INLINE_STACK`, not any running node. Repository demands stay on the
   sequential thread (`lane_work`: the serving coordinator queues them).
   While waiting, the sequential thread linearizes (`Bodies::idle`, in
   steps of 20k pass-1 events, the coordinator released in between).
   The sequential pass hints its edges (and a registration's toolchains)
   to the pool. Round 1 stays a round (its lane's writes are where the
   walk's are).
2. **Pass 1** 2.6 → 1.5 s: the hot loop reads a node's events without
   its value (`Transcripts::events`), keys by reference, events by
   reference (cloned only into the tape), Fx hash maps, no keep-going
   edges (`Options::no_edges`).
3. **The coordinator's lock off the pool's paths**: a `Flush` marks
   `Phase1Bodies::saw_flush` (an atomic; it took the lock: 28 s of
   threads blocked), a speculated FeatureFlagInfo is checked through the
   inbox, the planned-providers check uses `try_coord`; a speculation
   takes its subtree and the store's new packages/memos/CT1 values under
   the lock and publishes them into its scratch engine after it
   (packages by insertion order, not a scan of all).
4. **Speculations on 2 scratch engines** (`BAZEL_RS_EVAL_SCRATCHES`).
5. Smaller: toolchain implementations published incrementally.
6. With `BAZEL_RS_EVAL_NO_SERVE=1` (rounds between requested targets),
   the sequential pass hands what inline completions woke to the pool
   once ≥ 8 are woken (an *assist round*, `BAZEL_RS_EVAL_ASSIST_WOKEN`,
   `BAZEL_RS_EVAL_NO_ASSIST=1`), and the coordinator linearizes while a
   round runs (`BAZEL_RS_EVAL_NO_IDLE_LINEARIZE=1`): phase 1 proper
   ~11–13.8 s there.

**Fixes found on the way** (latent before, exposed by the concurrency):
released transcripts were freed after *any* walk (a deferred segment's
nodes too: a speculation then failed on "a transcript of the subtree was
freed"; now `free_released(walked)`); a speculation's progress counters
were restored to a snapshot (racing the tape's package commits: "10244
packages loaded"; now `progress::uncounted` per thread); a rerun's start
on a pool thread removed the abandoned attempt's events from the store
before the rerun finished, so pass 1, inside that memo's partly replayed
transcript, saw no transcript and ended the memo there (a
`RegToolchains` memo lost ~3.5k plan entries in ~1 of 4 runs: "129636
targets configured"); now the carry stays until the rerun finishes or
stops, and pass 1 waits for a vanished transcript before the end.
Deadlocks found with the watchdog and fixed: an inline attempt's rerun
re-claiming its own node; an inline park without waiter registrations;
a timeline mark taking the coordinator's lock under the timeline's lock;
a registration-package hint whose module-graph resolution waited for a
demand on the coordinator (removed).

**Measured** (2026-11-10, ~/code `80d6724f3a` private clone, LTO release
of main `2c7c1a64` and of the branch rebased on it, warm, snapshots off,
`build --nobuild //src/...`, default threads, alternating, host load
10–13):

| | main | eval-phase1 |
|---|---|---|
| phase 1 proper (plan.start → graph dropped) | 18.1 / 15.1 / 15.4 s | 9.5 / 9.5 / 8.7 s |
| plan.start → phase 1 done | 28.5 / 26.1 / 26.4 s | 20.2 / 20.6 / 19.6 s |
| `INFO: Elapsed time` | 37.6 / 34.0 / 34.1 s | 28.0 / 28.3 / 28.3 s |
| max RSS | 9.3–9.5 GB | 9.2–9.3 GB |

Byte-identical: `BAZEL_RS_DUMP_ACTIONS` stderr of `build` and `test
--nobuild` (6,486,165 lines; walk = branch eval), `affected --base=HEAD^`
(737 of 802); `BAZEL_RS_EVAL_VERIFY=1` clean on `build`, `test --nobuild`
and `affected`; `MODULE.bazel.lock` (native), the hidden lockfile and the
snapshot recording (520,171 records) identical between main's walk and the
branch's eval. Suites: eval default (`BAZEL_RS_DEFAULT_ANALYSIS=eval`)
3738/3738, also with `BAZEL_RS_EVAL_RESTARTS=7`.

**What remains of phase 1 proper (~9 s)** (the final timeline):
round 1 0.3 s; requested #0 until 4.4 s — the zig toolchain's
`Pkg(@hermetic_cc_toolchain++toolchains+zig_config)` loads for 0.6–0.9 s
on one thread on `RegToolchains`' path, local_jdk 0.1 s, go_deps probe
claims (up to 18 threads wait for one repository's warm-reuse probe,
~8 s of thread time), speculations serialized on their subtree replay
under the coordinator's lock (~3–4 s of waits); 4.4–9.3 s: the
sequential pass reaches what the pool left (`Tgt` lookups whose
extension group is not prepared: `apt`, `go_deps`, `pnpm`, `i`; ~6–8k
nodes wait for it) while linearizing (pass 1 1.4 s, tape 0.5 s,
publication 0.7 s — mostly `publish_ct1`'s map inserts and the `Shared`
tables copied after each ordered walk —, the ordered new-platform walks
0.9 s), on one core. Next, in order: (a) `Tgt`/`Src` nodes adopting a
finished extension speculation as `Pkg` loads do (repository-side; needs
review) so the pool does not wait for the sequential pass; (b) the
publication without copying the shared tables (fewer ordered walks, or
publication batched before a walk); (c) the subtree replay of a
speculation outside the coordinator's lock (a store readable
concurrently); (d) warm-reuse probes of a repository's tree started
ahead (go_deps); (e) `local_jdk` executed ahead when its `execute()`
calls are known read-only.

**After the review of 5d835ab4** (four fixes, each its own commit):
(1) the internal error of a speculated flag, a rerun or an extension
adoption that differs is sticky in the store (`Coord::sticky_mismatch`):
the idle steps, a sequential attempt's begin and the stepped
linearization drop their result, and the error was taken with it; now
every later linearization and the end of phase 1 fail with it (hook
`BAZEL_RS_EVAL_TEST_FLAG_MISMATCH=1`, test in `eval_csmatch`). (2) An
extension group's adoption by a pool load against the group's eviction
when the sequential thread prepares it otherwise: adopting loads count
themselves in flight per group until the load ends (`GroupFlight`); the
eviction marks the group dropped under the adoption locks (no
`eval_used` fast path), adoption re-checks the mark, and an adopter in
flight at the drop is the adoption-mismatch internal error. (3) A
platform memo is published at the first flush after the tape's first
use of it (held in `Linearizer::held_platforms` until then; the rest at
phase 1's end), not at the first flush after the pool finished it: the
walk's segment before that use saw the platform deferred. The
new-platform walks are now order-invariant
(`BAZEL_RS_EVAL_PLATFORM_LOG=1` logs each publication's flush; the same
51 publications at the same flushes on ~/code in every run, also under
`BAZEL_RS_EVAL_RESTARTS=3` with `BAZEL_RS_EVAL_JITTER=100`; test in
`eval_pass2`). Its cost: ~/code's pass 2 is now always "new platform 27,
walks 51 (112,246 entries)" (before: 5–6 new-platform walks, 9–16 walks,
~22k entries, by timing: the pool's early platforms hid the new ones);
the ordered walks run on the sequential thread inside phase 1, so phase 1
proper is 23.5–23.7 s (was 9.8–10.2 s), flush phase 2 15.0–15.4 s (was
11.0–11.3 s), phase 1 done at 24.4–24.6 s (was 20.4–21.0 s), elapsed
32.8–33.2 s (was 28.9–29.5 s), RSS 9.4–9.5 GB. Getting this back means
deferring a new platform's flush instead of walking at once (the
platform published as an effect at its position in the deferred walk),
which is pass 2's deferral (the deferred walks' area). Rebased on T3
round 3 (642cbd8a): phase 1 proper 22.0–22.7 s, phase 1 done 23.0–23.6 s,
elapsed 31.4–31.8 s, RSS 9.4–9.5 GB.

**Step 1 (repository side, separate commit for review): lookups adopt.**
A `Tgt` lookup on the pool whose source-file part (`source_target_with`
→ `root()`) needs an extension group not prepared yet raised a sentinel
and went to the sequential pass (~/code: 4,765, the `go_deps`, `apt`,
`pnpm`, `i`, `protoc` repositories) although the package's own pool
load had adopted that group's speculative preparation. Such a lookup now
adopts the groups the load of the very package value it read adopted
(`PkgValue::adopted_groups`, `EvalAdoption::start_limited`; a debug
assertion checks the subset), and no other. A lookup records no group
itself: it has no replay point of its own (its events are its user's),
and the groups are among that load's roots. Their commit stays the
load's: where the tape replays the `Pkg` (the lookup's user replays
`MemoUse(Pkg)` before it reads the target), and the commit's check
(`check_eval_group`) still fails closed when the walk prepares the group
otherwise; the lookup counts in flight for the eviction check as a load
does. ~/code: repository sentinels 4,765 → 0, output identical (dump,
platform publications). No suite fixture leaves such a lookup to the
sequential pass (their groups are committed before the lookups run), so
the change has no regression test of its own. (4) The serving
sequential pass's wait is bounded by progress: no node completed and no
repository fetched for 300 s (`BAZEL_RS_EVAL_SERVE_STALL_SECS`) is an
internal error naming `BAZEL_RS_EVAL_NO_SERVE=1` (a cycle of a `.bzl`
claim, a demand and a speculation the claim waits' own detection does
not see); a waiting claim whose dependencies are all done is taken over
(a park without waiter registrations no longer spins). No suite test
reaches the serving wait (the test workspaces finish in round 1). A
debug assertion checks that a pool thread's root commits write nothing;
the suggested one on pass 1's blocker after a begin does not hold for
cycles (pass 1 waits for `cycle_b`, parked on `cycle_a`, while `cycle_a`
begins again with its back edge answered), so it is not kept.
The sticky mismatch surfaced two reruns that differ in the suites
(`test_flags` `early_planned_flag_keys…`, `toolchains`
`registration_pattern_errors…`, eval default): an attempt abandoned
after a flush whose phase 2 fails (or past pass 1's stop) reruns with
the error where it emitted a `MemoUse`; pass 1 never replayed those
events (it stops at the failure). So a mismatch fails a build that would
succeed, and a build the walk fails keeps the walk's error.

### Genuine parity, task A: the sweep (eval-t3)

Design: `plans/genuine-parity-design.md` (workers do the heavy work in any
order; every position-dependent decision is a function of plan order and
the delta's own ops). What is built (`src/engine/sweep.rs`,
`BAZEL_RS_EVAL_SWEEP`, on; `=0` keeps the T3 cursor's rebuilds):

- **Losers are not built again.** A worker's delta always names its own
  actions (a shared output's loser publishes its results with its own
  action ids when the winner's artifact is not known yet, instead of
  publishing nothing). At its merge, `localize` replays the walk's output
  registration over the build's producers — `check_output` per `Output`
  op in order: New / Shared (winner and its stamp), the partial-share rule
  (every output of an action as its first), any conflict — and turns each
  shared action into the walk's: its `Output` ops `Shared`, its
  declarations `replace` only when predeclared, its node, execution,
  build-info and listing marker dropped, its id mapped to the winner's. An
  error, a failed output, a conflict, a partial share or a sharing inside
  the delta is built again as before (its exact text and partial state).
- **Contested readers are not built again.** `read_aliases` checks a
  delta's recorded index reads at its merge: a read that differs from the
  index's canonical entry now *only in its producer id* (same output
  index, every other field equal, stamped before the reader) maps that id
  to the canonical one; any other difference (a File read as missing that
  is registered now, a still-provisional entry that differs) is built again.
  A read once found canonical is remembered by its artifact's identity
  (`ActionPlanning::canonical_reads`: canonical entries never change and
  readers merge in plan order), so the memoized depset walks' reads every
  PyCompile delta inherits are checked once.
- **The rewrite** (`rewrite`): every Artifact naming a mapped id names the
  canonical one (its output index kept: a shared action shares every
  output, in order), each once by identity — `Artifact`, `Declared`,
  `Dry`, `RunfilesNodes` —, and so does every generated input of the
  delta's nodes and shared input groups (each `Arc<InputGroup>` once, by
  pointer). A lowering in flight becomes `DeltaOp::Rewritten`: where it is
  placed, a shared action's product is dropped and another's rewritten (a
  listed action's nodes after the placement builds them). Nothing else of
  a node depends on a producer id (inputs at one exec path are one input
  by producer class, dbb5be3c). Dependents are not touched: providers and
  views hold Files.
- **Hooks**: `BAZEL_RS_PARITY_VERIFY=1` builds every rewritten delta again
  as the T3 cursor does (from the kept analysis, else computed again) and
  compares op for op (lowerings as placed, groups by content), and at each
  walk's end checks that every generated input of every node, every file to
  build and runfiles artifact names a planned action (no unaliased loser id
  is left); `BAZEL_RS_PARITY_REVERSE=1` runs the ready job latest in plan
  order first. `tests/eval_buffer.rs` runs both at 1/2/8/32 threads with
  jitter and restarts, and `BAZEL_RS_EVAL_SWEEP=0`.
- **Workers**: 16 for frozen walks (8 with the sweep off).
- `BAZEL_RS_TIMING`: `PERF-PARALLEL sweep` (readers, losers, fallbacks,
  verified, artifacts rewritten, the time of each step),
  `PERF-PARALLEL buffer cursor … last_job=` (when the walk's last job
  ended).

Not built (the rest of task A): the cursor itself — claims, waits,
steals, the per-entry stages and at-turn entries stay; output registration
is replayed at the cursor's merge rather than in a separate sweep; no
per-entry result slots (the published results stay `PlannedResults`).

**Measured** (2026-11-10, ~/code `80d6724f3a` private clone, LTO release,
warm, snapshots off, `build --nobuild //src/...`, alternating, host load
6–9):

| | main dbb5be3c | sweep, 8 workers | sweep, 16 workers |
|---|---|---|---|
| `INFO: Elapsed time` | 25.7 / 26.2 s | 24.3 / 24.1 s | 23.3 s (12: 23.9 s, 24: 22.9 s) |
| big deferred walk (119k entries) | 9.2–9.4 s | 8.7 s | 8.2–8.4 s |
| cursor `merge_starlark` | 2.2–2.6 s | 1.4 s | 1.4 s |
| deltas built again at the merge | 432 losers + ~170 stale | 0 (734 rewritten: 464 losers, ~270 readers; 0 fallbacks) | 0 |
| max RSS | 9.2 GB | 9.3–9.4 GB | 9.8 GB (24: 10.0 GB) |

**Memory** (8 → 16 workers: 9.3–9.4 → 9.7–9.8 GB; main 9.2 GB): mostly
the 16 workers' computes in flight. The rest: the kept analysis results of
the deltas that meet an earlier entry's outputs (kept only for those,
~430 on ~/code, for a fallback rebuild), the rewritten Artifacts (~70k)
and the per-delta copies of the shared input groups a rewrite touches
(the memo's own value is refilled by later users), and
`canonical_reads` (one artifact handle per provisional read found
canonical, for the planning run).

**After the review of 19223aae**: a shared action's lowering is dropped
at the rewrite (the lowering op carries its action id), so a failure only
its lowering finds no longer fails the build (`BAZEL_RS_LOWERING_FAIL`
test) and is not waited for; the sweep needs the merge buffer
(`BAZEL_RS_EVAL_NO_BUFFER=1` is the T2 pipeline, whose builds ahead of
their turn record no reads); the walk-end scan also covers the index's
entries, the declarations, the execution table and the source inputs;
debug builds and `PARITY_VERIFY` assert no alias chain and no mapped or
dropped id left in a rewritten delta or a placed node.

The rewrites cost 0.3 s on the cursor (reads 0.1 s, replay 0.09 s,
rewrite 0.11 s) where the rebuilds cost ~1.3 s. The walk still ends with its
last job (`last_job` within 0.3 s of the end), but the jobs themselves are
gated by the cursor: a profile of the cursor (pprof, 16 workers) is 42%
entries at their turn (`buffered_at_turn`: attribute-aspect applications,
rustfmt/clippy 1.1 + 0.5 s), 13% Starlark merges, 12% published natives'
merges, 6% natives committed at their turn; and `BAZEL_RS_T3_DEBUG`'s slow
steps are those turns and the chains they ready (a `package_group` at its
turn readying the cc toolchain, `_sandbox_native.so`'s cc_binary readying
`//src/python:web_lib` → `:web`, 0.4 s). Those are task B (no at-turn
entries).

Identical output: `BAZEL_RS_DUMP_ACTIONS` stderr of `build --nobuild`
(85ae7ead3db2, 6,486,165 lines) for the walk, the sweep at 16 workers and
under `BAZEL_RS_PARITY_REVERSE=1`; `test --nobuild` (941d72967f5c).
`BAZEL_RS_PARITY_VERIFY=1`: 758 rewritten deltas compared, the producer-id
scan clean; `BAZEL_RS_VERIFY_DELTA=1 BAZEL_RS_VERIFY_ARTIFACTS=1` clean
(37,916 worker deltas, 771 rewritten ones); `BAZEL_RS_EVAL_VERIFY=1`
(`--memory-limit=40000`) clean.

### Genuine parity, task B: no at-turn entries (eval-t3, in progress)

- **Package groups** are worker natives: a worker resolves the group
  read-only over the frozen view (`Engine::resolve_group_readonly`: loaded
  packages, no memo, no budget; anything else leaves it at its turn) and
  publishes its PackageSpecificationInfo, empty files to build and
  transitive repositories; its merge (`merge_published_group`) resolves it
  again as the commit does (the memo, the budget, an error at its
  position). ~/code: 29 at-turn package groups → 0 (no walk-time change:
  their readers were not the critical chain). Test
  `eval_buffer::package_groups_publish_from_the_workers`.
- A published native rule's console owner is built on its worker
  (`NativeMerge::owner`; 5% of the cursor's time).
- The T3 window is 262,144 entries (was 16,384): with the cursor slower
  than the workers, the window held ready jobs back (Ct2 published p90
  6.2 → 3.3 s); ~/code's deferred walk 8.4 → 7.6–7.8 s, peak RSS unchanged
  (9.8 GB).
- **Attribute-aspect rules are worker jobs** (`parallel/aspect_jobs.rs`,
  `BAZEL_RS_T3_ASPECT_JOBS`, on; `=0` the prefetch at the rule's turn,
  `parallel::aspects`). The aspect planning (`plan_aspect_dependency`'s
  recursion, now generic over `aspect_plan::AspectCx`) runs in the rule's
  prepare on the worker over the frozen view (`FrozenAspects`): dependency
  views as any worker's, the aspect memo as the walk froze it, phase 1's
  aspect tables (`aspect_attribute_configs`, `aspect_toolchains`,
  predicates, `attr_aspects`) from the engine or from the pass-2 ops at the
  job's position, a predicate or `attr_aspects` missing computed there.
  Each application is analyzed once per walk, by whichever job needs it
  first (`AspectTable`, single-flight; the analysis isolated like a
  compute): its providers, analysis, prints, owners and the applications
  its own dependencies met (its "subs", in the recursion's order). A
  dependency whose results are not published yet sends the job back to
  wait for them (`Board::wait_for`, ≤ 16 times). Nothing is committed on
  the worker.
- **Placement** at the rule's merge (`Engine::place_aspects`): the source
  lookups its prepare made are applied, each aspect-configured `Target`
  value it used is checked against the planning state's (the first in plan
  order wins; another one prepares the rule at its turn), then the
  applications its edges met are walked in the recursion's order, each
  one's subs first, and those not in the aspect memo are committed with
  the sequential code (prints, console and aquery owners, workspace status,
  `register_action_outputs`, nodes, executions; `commit_application`), an
  error located at the rule as its prepare reports it. The rule's own
  commit follows (`commit_turn`, its delta built on the cursor). An
  application that could not be used (failed, panicked, had effects) or an
  input not registered yet stops the placement: the rule is prepared at its
  turn from there (what was placed is a prefix of the sequential commits),
  and the coordinator's walk takes the analyses from the table
  (`Engine::walk_aspect_analysis`; `BAZEL_RS_VERIFY_DELTA=1` analyzes each
  again from the coordinator's inputs and compares).
  `BAZEL_RS_T3_ASPECT_JOBS=verify` runs the jobs and prepares every such
  rule at its turn (with `BAZEL_RS_VERIFY_DELTA`: every worker analysis
  checked against the coordinator's inputs). `BAZEL_RS_T3_ASPECT_DELTAS=1`
  (off): the job builds the rule's delta too (published for its
  dependents; one failing on its applications' files is computed again at
  the merge) — no gain on ~/code now. `PERF-PARALLEL aspect_jobs`.
- **Classification in plan order**: the classification chunks are queued at
  their first entry's plan index among the jobs (they were all ahead of
  every job: a job readied early waited up to ~1.9 s for the whole walk's
  classification); `BAZEL_RS_T3_CLASSIFY_FIRST=1` the old order.
- Measured (~/code `80d6724f3a`, LTO release, warm, snapshots off,
  `build --nobuild //src/...`, alternating, host load 13–20; "before" =
  the same binary with `BAZEL_RS_T3_ASPECT_JOBS=0
  BAZEL_RS_T3_CLASSIFY_FIRST=1`):

  | | before | task B aspect jobs |
  |---|---|---|
  | `INFO: Elapsed time` | 23.6 / 22.9 / 23.3 s | 22.1 / 21.6 / 20.9 s |
  | big deferred walk (119k entries) | 7.61–7.70 s | 5.55–5.66 s |
  | its cursor's CPU | 5.47–5.55 s | 3.62–3.66 s |
  | max RSS | 9.67–9.73 GB | 9.79–9.83 GB |

  At-turn entries: no attribute-aspect rule (22 aspect jobs in the frozen
  walks, 1,777 applications placed, 0 fallbacks; 6 `unready` natives).
  The walk now ends with its last job: the job DAG's critical path is
  2.3 s (`BAZEL_RS_T3_CRITICAL`: `server_clippy`'s job alone ~2.0 s — its
  closure's applications analyzed in one job), work 69 s / 16 workers.
  Identical: the action dumps of `build` (85ae7ead3db2, also under
  `BAZEL_RS_PARITY_REVERSE=1`) and `test --nobuild` (941d72967f5c);
  `BAZEL_RS_T3_ASPECT_JOBS=verify BAZEL_RS_VERIFY_DELTA=1` (1,703 worker
  analyses compared), `BAZEL_RS_VERIFY_DELTA=1 BAZEL_RS_VERIFY_ARTIFACTS=1`,
  `BAZEL_RS_PARITY_VERIFY=1 BAZEL_RS_PARITY_REVERSE=1` and
  `BAZEL_RS_EVAL_VERIFY=1` (`--memory-limit=40000`) clean.
- **After the review** (an application's analysis depends on how it was
  reached, not only on its key): the walk's table is keyed by key and
  *context* (`aspect_plan::Context`: the collection's aspects, the content
  of the base view the `target` parameter is built from — provider values
  and runfiles by identity, so an alias's view and its actual's share an
  analysis —, the target-platform answer). Each entry records the
  plan-position range where its position-dependent reads hold (target
  platforms, phase 1's aspect tables published by pass-2 ops) and the
  aspect-configured `Target` values its dependencies' views hold. A
  placement: an application already committed must have the very
  providers the job saw (else the rule at its turn), an entry only inside
  its range, its `Target` values checked and put in the planning state
  (the rule's own after all its placements). The `Target` table is keyed
  by key and variant (requested label, the merged applications' provider
  identities); the coordinator takes its values from it. The coordinator
  takes a job's analysis only where it holds (range, `Target` values, its
  dependencies' applications as committed); a rule whose job was not
  placed is prepared at its turn with the prefetch. Applications committed
  by earlier merges are memo hits for later jobs. A requeue stops when the
  key is published already. Tests: `eval_buffer`
  `an_aspect_application_reached_in_two_contexts_is_the_first_in_plan_orders`,
  `an_alias_first_in_plan_order_keeps_its_aspect_configured_target`;
  `eval_pass2`
  `aspect_applications_under_deferred_platforms_read_them_at_the_placing_rules_position`.
- ~/code after the review: 14 aspect rules placed, 8 at their turn (6
  `Target` values differ: crate_universe's `@crates//:x` aliases and
  protobuf's `@protobuf//:any_proto` aliases make the one shared
  aspect-configured `Target` value of a crate the first requester's in
  plan order, alias kind or not, and the jobs above it saw the other kind;
  2 applications committed in another context), 689 analyses redone at
  those turns. Big walk 6.2–6.7 s (before task B 7.4–7.8 s; the
  pre-review jobs 5.6 s), `INFO: Elapsed time` 21.2–21.7 s vs 22.0–23.6 s
  (`BAZEL_RS_T3_ASPECT_JOBS=0 BAZEL_RS_T3_CLASSIFY_FIRST=1`, alternating,
  host load 10–12). Dumps identical (build, build under
  `BAZEL_RS_PARITY_REVERSE=1`, `test --nobuild`); `=verify` +
  `BAZEL_RS_VERIFY_DELTA`, `VERIFY_DELTA` + `VERIFY_ARTIFACTS`,
  `PARITY_VERIFY` + `REVERSE`, `EVAL_VERIFY` clean.
- **Parity fix (merged targets)**: genuine 9.3.1 prints an aspect-applied
  dependency as `<merged target ACTUAL>` (MergedConfiguredTarget.repr)
  whether it was requested through an alias or directly and whatever the
  request order (tests/fixtures/aspect_merged_alias, genuine-captured);
  bazel-rs shared one value per (actual, configuration, aspects) whose kind
  was the first requester's in plan order (`<alias target …>` or
  `<target …>` for every consumer). Now `TargetKind::Merged` over the
  requested view's kind (an alias's label kept for `expand_location`), the
  shared value keyed by the requested label: no order dependence. ~/code:
  the 8 aspect rules that fell back (6 `Target` values, 2 contexts, 689
  analyses redone) are all placed now (22 placed, 0 fallbacks, 0 redone);
  the big walk 5.6–5.9 s. A rule whose job is not placed is prepared at its
  turn with the prefetch: its non-cheap analyses run on the workers in its
  exact context (cheap ones, < 1 ms on average, inline), so redoing the
  fallbacks' analyses on the workers needed nothing more.
- Left at their turn: the natives committed there (genrules, filegroups,
  aliases: published, so nobody waits, ~0.85 s cursor CPU). Not done: an
  application's delta built on its worker (`DeltaBuilder` +
  `sweep::adopt`: the placement commits on the cursor), the applications of
  one large closure analyzed in parallel (the critical path's
  `server_clippy`).

### eval-record: the recording and the tracker under deferred flushes (task E)

Until 0.4.16 the evaluator kept every flush ordered while the analysis
snapshot recorded or the incremental tracker ran, and `auto` planned those
requests with the walk. Measured on ~/code (`80d6724f3a` private clone,
`build --nobuild //src/...`, a snapshot miss, main `fc955997`): eval 81 s
against the walk's 63–65 s. Why:

- **Ordered flushes**: ~2,357 small walks inside phase 1 instead of one
  deferred frozen walk (T3) — snapshot off with `BAZEL_RS_EVAL_NO_DEFER=1`
  44 s against 26 s deferred;
- **classification not quiet**: the evaluator's source lookups were not
  memoized for phase 2 when not deferring, so 9k Starlark entries' lookups
  observed and stayed at their turn (`fallback requirements=8983`, turn
  Starlark 35 s against 16 s);
- **the recorder's commits on the coordinator** (the tape's frames: 6.4 s
  of "tape application");
- **48 threads re-probing** what a recording did not memoize: source
  directories (3.6M more stats than off, mostly BUILD probes of pip trees
  under `site-packages`) and store trees (each frame's claim walked the
  tree: the Go SDK's 17k entries ~94 times) — kernel time on the pool's
  threads went from 35 s to 210 s.

What changed:

1. **Deferral under the recording**: the recorder is a set (first outcome
   wins, a second one is Unstable), so the order of its commits does not
   show; only which observations reach it. Two holes the ordered flushes
   had masked (phase 2 looked everything up again): a `Tgt` lookup's
   observations went to its first user's frame, lost when that attempt was
   abandoned while the lookup stayed done (~/code: 43,656 STAT/LSTAT of
   source files) — the value now keeps them and every use replays them
   (the memo rule of docs/ANALYSIS_SNAPSHOT_RECORDING.md); and the sources
   the evaluation resolved are memoized for phase 2 only once their
   lookup's observations are recorded (else before a later deferred walk,
   or never: phase 2 then looks them up and observes, as the walk's first
   lookup).
2. **Deferral with the tracker**: the tape's tracker ops become compact
   `TrackOp`s (keys, labels, packages interned) kept by plan position like
   the tape's other effects and applied before the turns of the deferred
   walk (`Engine::eval_pass2_track`; no lowering drain), so the tracker
   sees phase 2 and the tape in the walk's order (key ids, package ids and
   first reasons depend on it). ~/code: 6.8M ops pending at the end of
   phase 1 (~110 MB).
3. **T3 frozen walks with the tracker**: a worker's prepare records its
   accesses, replayed in the entry's planning frame at its turn; the
   coordinator's frozen dependency views capture and replay their lookups
   as `plan_dependency_view` does (~/code: 265 accesses of the
   attribute-aspect rules otherwise missing); fast native merges are off
   (committed at the turn, in their planning frame).
4. **Speed of the recording**: source-directory probes are memoized while
   recording with their captured observations (a hit replays them; the
   walk too); a tree claim inside a frame walks a store tree once
   (`Recorder::walked`); the tape's commits go to background threads
   (`wait_commits` before every read of the recorder and at the end of
   phase 1); the probes move into the engine for the deferred walk.
5. **The sweep is off while recording**: a rewritten delta's input groups
   are groups of their own, so the planned request a snapshot saves shared
   107 input groups instead of the walk's 105 — not the same from run to
   run, the action dump the same — and verify mode reported a plan
   mismatch for every walk/eval pairing. Built again at the merge there
   (the T3 cursor's rebuilds). *Open (sweep):* keep the sharing in the
   rewrite.
6. `select.rs`: the `Recording` fallback is gone; `--profile` keeps the
   walk (its spans' order).

**Measured** (2026-11-11, ~/code `80d6724f3a` private clone, release build
of the branch rebased on main `e3a704cc`, warm repositories, host load
10–25; a miss: the snapshot store emptied first):

| `build --nobuild //src/...` | walk | eval (default) |
|---|---|---|
| snapshot on, a miss (records, tracks, saves) | 63.1 / 61.9 s, 9.94 / 9.93 GB | 43.5 / 43.5 s, 10.33 / 10.30 GB |
| `BAZEL_RS_SNAPSHOT=record` | 60–66 s, 10.4 GB | 38–41 s, 10.7 GB |
| snapshot off (for comparison) | 48.9 s, 8.89 GB | 24.1 s, 9.68 GB |

Identical walk vs eval: the record mode's dump (520,098 observations,
425,099 integrity entries, 8,848 trees; timings aside), the incremental
index (`BAZEL_RS_SNAPSHOT_INDEX_DUMP`, 15.8M lines), stderr with the action
dump (6,486,301 lines); a miss → `verify` (strict) → hit cycle clean and
stderr identical for every pairing (walk saves / eval verifies, eval saves
/ walk verifies, eval/eval, walk/walk). The walk's index and dumps equal
main's walk. Test: `eval_pass2::the_recording_and_the_incremental_index_are_the_walks`
(1–32 threads, jitter, restarts, frozen prepares on and off; a restarted
attempt's source lookup, an aspect's implicit tool, an alias view shared
between a worker and the turn).

**Open**: peak RSS on a miss is the walk's + ~0.4 GB (10.3 GB; the
evaluator's own overhead is +0.8 GB with the snapshot off since the sweep
and task B: frozen views, the 262,144-entry window); `--profile` still
plans with the walk.

### Effort

| Step | Work | Estimate |
|---|---|---|
| T2a | `Ct2` body on the pool over node reads (`PlanReads` over needed values, output-file edges, phase-1 values from `Ct1`), the stamp-free provisional index | 1 week |
| T2b | natives as delta builders (`DeltaBuilder::native_target`, `Validation` op), verified op-for-op with `BAZEL_RS_VERIFY_DELTA` | 1 week |
| T2c | `Aspect` nodes from `plan_aspect_collection` (1,777 applications on ~/code) | 1 week |
| T3 | merge buffer, alias map, loser rebuild without recompute, contested-reader canonicalization, fail-fast, cap, boost; delete `Pipeline` (unsettled, commit ahead, barriers), `PathHashes`, the artifact stamps | 1.5–2 weeks |
| verify | thread matrix + jitter + `BAZEL_RS_EVAL_RESTARTS` on the suites, ~/code dumps, `BAZEL_RS_ORDER_AUDIT` as a regression report | 0.5–1 week |

Risks: a File reaching a target outside its dependency closure (none found:
Files flow through providers and views; a lookup that misses is today's
"input has no declared artifact" error, then at the merge); the
same-label cross-configuration File (one index entry for two owners: both
ids canonical at the merge); memory of buffered results (cap); pass-1
decisions (`empty_configuration_rules`, `forced_exec`) needed before a
`Ct2` can start — pass 1 streams ahead of phase 2, or the merge checks the
variant.
