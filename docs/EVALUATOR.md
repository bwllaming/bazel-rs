# The parallel analysis evaluator

Status: **S1 integrated** (A, B, C). `BAZEL_RS_ANALYSIS=walk` (the
default) is today's sequential phase-1 walk with the P3/S1/S2 pipeline
(docs/PARALLEL_ANALYSIS.md); `BAZEL_RS_ANALYSIS=eval` runs phase 1 with
the node bodies on the inline driver, linearized by pass 1 (§4 "S1,
worker C"); `BAZEL_RS_EVAL_VERIFY=1` plans with both and compares.

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
