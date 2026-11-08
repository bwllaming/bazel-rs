# Analysis on a snapshot miss (cold-analysis-speed)

What a full analysis costs when no analysis snapshot serves the command
(the first command of a target set, `BAZEL_RS_SNAPSHOT=off`, the plannings
of `affected`), with the repositories already fetched: loading the packages,
configured-target analysis (phase 1), action planning (phase 2) and the
planning tail. Fetching and module extensions are not covered here.

## Measurements

Private clone of ~/code at `80d6724f3a`, repositories warm (fetched by a
first run into the same `--cache-dir`), `BAZEL_RS_SNAPSHOT=off`, storagebox
overrides, `nice -n 10`, 64-core shared host. Genuine Bazel 9.3.1:
`--batch` (a fresh server, so a cold analysis), its own output base with the
repositories fetched by a first run.

| command | 0.4.10 | this revision | genuine 9.3.1 |
|---|---|---|---|
| `build --nobuild //src/...` wall | 176.4 / 176.1 s | **78.2 / 78.6 s** (2.25×) | 10.7 s |
| — CPU user + sys | 182.5 + 30.4 s | 83.9 + 35.2 s | 278.8 + 21.9 s |
| — peak RSS | 8,454 / 8,479 MB | 8,499 / 8,493 MB | 8,365 MB |
| — `plan.phase1_analysis_done` mark | 147.0 s | 58.8 s | – |
| `test --nobuild //src/...` wall | 185.4 s | **79.2 s** (2.34×) | 10.2 s |
| — CPU user + sys, peak RSS | 183.5 + 38.7 s, 8,492 MB | 84.4 + 37.5 s, 8,526 MB | 279.5 + 15.5 s, 7,818 MB |
| CI step `affected --base=HEAD^` (802 targets in scope, both revisions at once) | 178.6 s | **81.2 s** (2.20×) | – |
| — CPU user + sys, peak RSS | 334.4 + 180.1 s, 6,887 MB | 149.5 + 137.4 s, 7,246 MB | – |
| `affected --noconcurrent_revisions` | 323.1 s | 136.8 s | – |

Peak RSS with jemalloc's decay off (`_RJEM_MALLOC_CONF=dirty_decay_ms:0,muzzy_decay_ms:0`,
the live peak): `build` 8,473 → 8,498 MB (the memo tables below, ~10 MB, and
run-to-run noise of ±25 MB), concurrent `affected` 6,899 → 6,907 MB. With the
default 10 s decay the concurrent `affected` peaks 5% higher: the same work
in half the time leaves pages freed by one thread's arena undecayed when
another thread allocates (docs/MEMORY.md, "The allocator").

Genuine is CPU-bound too (279 s of CPU) but spreads it over 64 cores;
bazel-rs analyzes on one coordinator thread, busy for the whole wall time
(sampled: 104.7 s user + 6.9 s sys of a 112.7 s run), so the remaining lever
is parallel analysis (below).

Outputs: the `BAZEL_RS_DUMP_ACTIONS` dump of `build --nobuild //src/...`
and `test --nobuild //src/...` (6,483,434 lines each) is byte-identical to
0.4.10's, so is the rest of stderr; `aquery --output=summary` of
`//src/python/github_backup/...` and `//src/python/bworker/...` (PyCompile
included); `affected`'s output and both `--keep_digests` digest files.

## Where the time went (0.4.10)

A CPU profile (pprof, phase 1: 128 s of samples) of `build --nobuild
//src/...`: `py_library` targets were 58% of the planned targets' time
(compute 33 s, commit 57 s). rules_python's `actions_run` adds the exec
runtime's files (a 1,657-file depset) to every PyCompile action's inputs, and
the precompiler's interpreter is also the action's executable. Per action:

- registration (compute): the inputs depset flattened and deduplicated
  (hashing every File's three strings), every path validated twice over
  (`validate_relative`, and the namespace overlap check hashing every proper
  ancestor of every path), a SHA-256 key over every input;
- commit: every input resolved through the build's `BTreeMap<File,
  Artifact>`, validated again (the shared-group check), deduplicated by
  execution path, and — because the interpreter is listed twice — its
  source paths listed (one `PathBuf` per input) and compared with the
  planned ones.

## What changed

**Memoized depset walks** (`Depset::flatten_files_with_memo`,
crate::starlark_types). A node of Files whose walk has at least 128 files and
is requested a second time keeps its walk (its files in walk order and their
set) in a process-wide table keyed by the node's address, holding a weak
reference so the address stays the node's; the node records the state in one
byte of its padding. A walk that reaches a memoized node first — down the
first-visited child of each node above it, with nothing seen yet — starts
from its files, then walks the rest exactly as NestedSet.toList does, also
deduplicating against the memoized set. `ctx.actions.run`'s inputs carry the
walk they begin with (`RegisteredAction::input_memo`).

**Shared input checks.** The registration validates an action's own paths
against the walk's namespace summary (`action::InputNamespace`: hashes and
indexes of the paths and their ancestors, no copies), and the commit resolves
the walk's artifacts once per build (`SharedInputs`), with their namespace,
their own execution-path occurrences and their runfiles-group roots. Every
fast path declines on any doubt (a write, a transform, a Java compile, a hash
collision, an earlier own input at a shared path with another real path, any
check that would fail): the full path then runs and reports exactly what it
reported before. Debug builds (the tests) check every accepted fast path
against the full one.

**Smaller items.**

- A later occurrence of a source at its first occurrence's real path (the
  interpreter, executable and runtime file) no longer forces listing and
  comparing the action's source paths.
- Action-sharing keys (in-process only): one buffered BLAKE3 digest of the
  framed encoding instead of streamed SHA-256.
- Depset deduplication hashes a File's path only (equal Files have equal
  paths).
- `Engine::target` memoizes rule lookups (hash of the label, checked against
  the rule's label); `flush_delegations` rescans the rule delegations only
  when new ones were recorded.
- The planning tail looks missing sources and source directories up by hash
  (crate::path_key) and skips an action none of whose source paths names one;
  hidden runfiles look their target's runfiles tree up by owner index.
- Runfiles checks hash paths with crate::path_key; the repository mapping
  manifest parses each runfiles owner once.

`BAZEL_RS_TIMING=1` prints `PERF-MEMO walks=… walk_files=… shared_inputs=…
shared_artifacts=…` (`//src/...`: 162 walks of 112,421 files, 32 shared
input runs of 59,698 artifacts).

## Rule implementations on analysis workers (P3)

Design and determinism: docs/PARALLEL_ANALYSIS.md, "P3". In short: the
coordinator still analyzes (phase 1), prepares and commits every target in
plan order; a Starlark rule whose dependencies are committed is prepared
ahead of its turn and its implementation — with the pure part of its
runfiles support — runs on one of 8 workers; everything order-observable it
would have written is applied at its turn. Beside it, the build's
File → Artifact index (looked up per input, never iterated) is an
`FxHashMap` instead of a `BTreeMap`.

Same setup as above (private clone at `80d6724f3a`, repositories warm,
`BAZEL_RS_SNAPSHOT=off`, `nice -n 10`, shared 64-core host; two runs each
for `build`). Before: main `c92f6c08`; after: this branch.

| command | before | after |
|---|---|---|
| `build --nobuild //src/...` wall | 82.0 / 79.7 s | **72.1 / 69.9 s** |
| — CPU user + sys | 82.4 + 28.2 / 83.2 + 33.7 s | 90.4 + 32.9 / 87.0 + 31.5 s |
| — peak RSS | 8,478 / 8,489 MB | 8,813 / 8,825 MB (+4%) |
| — `plan.phase1_analysis_done` | 60.6 s | 55.3 s |
| `test --nobuild //src/...` wall | 80.5 s | **70.9 s** |
| — CPU user + sys, peak RSS | 83.4 + 35.4 s, 8,513 MB | 88.4 + 31.8 s, 8,778 MB |
| CI `affected --base=HEAD^` wall | 82.2 s | **76.9 s** |
| — CPU user + sys, peak RSS | 149.3 + 124.5 s, 7,246 MB | 158.7 + 120.9 s, 7,309 MB |

`PERF-PARALLEL` (`build`): 22,715 of 38,757 implementations computed ahead
on workers, 16,042 at their turn on the coordinator, 0 aborted or
invalidated prepares, 0.7 s waiting for workers. The extra peak RSS is the
workers' heaps and the up to 16 prepared snapshots in flight (jemalloc
arenas per thread).

Outputs: the `BAZEL_RS_DUMP_ACTIONS` dump of `build --nobuild` and `test
--nobuild //src/...` (6,483,434 lines) and the rest of stderr are identical
to main's — also at `--loading_phase_threads=1` and with 32 workers and
5 ms worker jitter; `aquery` (text and `--output=summary`) of
`//src/beyondwork.ai/tools/oauth2token/...`; `affected`'s output and every
`--keep_digests` file.

**Why only ~12%.** The coordinator stays busy for the whole run: a sampled
profile of this revision's coordinator (66 s) is commits 27 s, the
implementations computed at their turn 14 s, phase-1 analysis 11 s,
prepares 10 s. An implementation can start only when its dependencies are
committed, and in phase 1's post-order the last dependency of most targets
commits right before them (a `py_library` after its deps' subtrees): 16,042
of them find nothing to overlap with. Computing those on a worker instead
only adds the hand-off latency (measured: 91 s, the coordinator waited
29 s). With commits in plan order on one thread, the floor is the
coordinator's own work (~55 s); genuine's 10.7 s comes from Skyframe
running whole configured-target functions — prepare, implementation and
action registration — concurrently, ordering nothing but their results.

## Deferred lowering and commits ahead of the turn (P4)

Design and determinism: docs/PARALLEL_ANALYSIS.md, "P4". In short:

- **Hash indexes.** The output producers (exact path lookups plus a set of
  every registered path's ancestors for the prefix check, which an ordered
  map answered by comparing paths component by component), `declared` and
  the configured-target-keyed planning maps that are only looked up (`dry`,
  providers, provider keys, runfiles, transitive repositories, edge
  configurations, toolchain and exec-group contexts, exec configurations)
  are `FxHashMap`s; the planning tail's reachability walk is hashed too.
- **Lowering on workers.** The pure tail of every action's commit (listing,
  structure and namespace checks, the source split, runfiles manifest and
  tree nodes) runs on 4 lowering workers and is placed in commit order; the
  shared input run of a PyCompile action (the Python runtime's 1,657 files)
  joins its inputs there, not as a per-action copy on the coordinator.
- **Commits ahead of the turn.** A target whose compute finished on a worker
  commits as soon as its outputs are settled, so its dependents are
  prepared and computed ahead too: implementations computed at their turn
  on the coordinator 16,042 → ~6,700, commits ahead ~22,000.
- The analysis worker also resolves the repo mapping manifests of
  `create_repo_mapping_manifest` actions; `display_form` no longer copies
  the main repository mapping per call.

Same setup as above (private clone at `80d6724f3a`, repositories warm,
`BAZEL_RS_SNAPSHOT=off` unless noted, `nice -n 10`, the shared 64-core host,
loaded by other work — run-to-run noise is several seconds). Before: main
`985c6423` (0.4.13); after: this branch.

| command | before | after | genuine 9.3.1 |
|---|---|---|---|
| `build --nobuild //src/...` wall | 72.1 / 69.4 s | **55.3 / 55.5 s** | 10.7 s |
| — CPU user + sys | 90.6 + 43.7 / 90.2 + 42.2 s | 96.6 + 42.9 / 97.3 + 42.7 s | 278.8 + 21.9 s |
| — peak RSS | 8,805 / 8,811 MB | 8,908 / 8,875 MB (+1%) | 8,365 MB |
| — `plan.phase1_analysis_done` | 54.7 s | 43.5 s | – |
| — stage totals: commit / prepare | 22.8 / 9.8 s | 9.3 / 8.1 s | – |
| `test --nobuild //src/...` wall | 70.1 s | **56.3 s** | 10.2 s |
| — CPU user + sys, peak RSS | 91.4 + 42.2 s, 8,785 MB | 98.7 + 42.8 s, 8,859 MB | 279.5 + 15.5 s, 7,818 MB |
| `build --nobuild` recording the snapshot (default mode) | 87.4 s, 9,620 MB | 72.4 s, 9,751 MB | – |
| CI `affected --base=HEAD^` wall | 78.9 s | **67.0 s** | – |
| — CPU user + sys, peak RSS (with the arena binding since removed; below) | 159.4 + 152.7 s, 7,415 MB | 169.1 + 153.6 s, 7,428 MB | – |
| `affected` P2 pair (lockfile change, both revisions in full, warm; idem) | 204.3 s, 9,673 MB | 182.1 s, 9,749 MB | – |

The coordinator is still CPU-bound for the whole run. Sampled (pprof,
phase 1 + phase 2 + tail, ~52 s): commits 14.9 s (ahead of their turn
included), implementations computed at their turn 10.4 s (the
`py_library` and `go_library` chains whose dependencies settle late: 738
`py_library` take 3.9 s), phase-1 analysis 9.1 s, prepares 7.4 s, the
planning tail 5.4 s, classification 1.4 s, package loading 1.4 s.

The concurrent `affected` peaks ~8% higher (7,372 → 7,944 MB on the CI
step): the lowering workers' freed temporaries stay in their own jemalloc
arenas until they decay. Binding the workers to the coordinator's arena
removed it, but needs handwritten unsafe (`mallctl`; tikv-jemallocator has
no safe API for it), so it was dropped in review.

Restored (plan B, step 0, approved): `src/alloc_arena.rs` reads and binds
`thread.arena` through `tikv_jemalloc_sys::mallctl` (the crate
tikv-jemallocator already links; it picks the prefixed or unprefixed symbol),
checking jemalloc's status; the lowering and the analysis workers bind to the
coordinator's arena. Private clone at `80d6724f3a`, warm, same host, one run
each: CI `affected --base=HEAD^` 7,926 / 7,939 MB (main) → 7,458 MB
(lowering workers bound) → **7,320 MB** (both pools bound, −7.8%), wall
69.1 / 67.3 → 65.2 / 65.1 s; `build --nobuild //src/...` 8,892 → 8,709 MB,
57.1 → 56.0 s. Outputs and digest files identical.

After the review fixes (main `d4b1d573` → this branch, fresh repository
fetch, so the dump's md5 differs from the table's run but is identical
between them): `build --nobuild //src/...` 70.8 / 70.7 → 55.9 / 55.4 s
(8,799 / 8,779 → 8,864 / 8,858 MB), `test --nobuild` 71.4 → 57.4 s, CI
`affected` 80.6 → 67.2 s (7,372 → 7,944 MB); 25,201 entries committed
ahead, each verified at its turn under `BAZEL_RS_VERIFY_AHEAD=1`.

## Plan B, first stages (branch `analysis-planb`)

Same setup (private clone at `80d6724f3a`, repositories warm,
`BAZEL_RS_SNAPSHOT=off`, storagebox overrides, `nice -n 10`), the shared
64-core host under load from other work (load average 15–21): run-to-run
noise is ±2 s, larger than most single changes below. Genuine 9.3.1
re-measured on the same host (`--batch`, its own output base, repositories
fetched by earlier runs).

| command | main 0.4.14 | this branch | genuine 9.3.1 |
|---|---|---|---|
| `build --nobuild //src/...` wall | 57.1 / 55.3 / 57.6 s | 56.9 / 54.3 s | 10.1 / 10.1 / 9.9 s |
| — CPU user + sys | 89–97 + 27–38 s | 90.9 + 34.8 / 93.6 + 31.1 s | 238–261 + 14–18 s |
| — peak RSS | 8,892 / 8,923 / 8,873 MB | **8,705 / 8,719 MB** | 5,894–8,065 MB |
| — planning tail (`phase2_actions_done` → `plan.end`) | 4.7 s | **1.4 s** | – |
| `test --nobuild //src/...` wall, CPU, RSS | 58.6 s, 90.6 + 29.6 s, 8,871 MB | 54.6 s, 93.9 + 31.5 s, 8,715 MB | 10.1 s, 272.6 + 13.4 s, 6,025 MB |
| CI `affected --base=HEAD^` wall | 69.1 / 67.3 / 66.7 / 70.9 s | 64.8 / 71.8 s | – |
| — peak RSS | 7,926 / 7,939 / 7,908 / 7,931 MB | **7,385 / 7,318 MB** | – |
| `affected` P2 pair (`e7b42879 → 9e6b547e`, warm) | 184.1 s, 10,424 / 10,461 MB | 175.9 s, **9,696 MB** | – |

What changed:

- **Arena binding** (step 0, above): −7.8% concurrent `affected` peak; the
  P2 pair, over 10 GB on main, stays below it.
- **Commits ahead decided by known outputs** (B1, first part): a finished
  compute reports the output paths its commit registers (hashed, with their
  ancestors; a superset: actions' outputs, the runfiles support's
  executable, manifests and tree); a native rule without outputs is known
  from the start. An earlier uncommitted entry with known outputs holds a
  later one only when their outputs meet (an exact path: shared action or
  conflict; a prefix); earlier entries with unknown outputs of a related
  package still hold it. That package rule does not cover an earlier
  entry, outputs still unknown, writing outside its package (a shareable
  artifact, which any rule calling an allowlisted helper may declare; a
  main-repository root package output under `external/`): the producers
  carry the registering plan index and such a meeting stops the build with
  the internal error naming `BAZEL_RS_NO_COMMIT_AHEAD=1` (a backstop, review
  item 1; making the later entry wait would hold every external-repository
  entry behind the root package's many `//:node_modules` targets).
  `BAZEL_RS_VERIFY_AHEAD=1` checks every
  Starlark commit's registered outputs against its compute's summary
  (`PERF-PARALLEL verified_outputs=`, 38,757 on `//src/...`). Commits ahead
  22,118 → 24,185 (27,648 under VERIFY_AHEAD's slower pace).
- **The declared outputs' check on the worker**: the commit's
  source-tree probes of every declared output (`validate_declared_outputs`:
  no subpackage or symlink on its way) run on the analysis worker after the
  implementation, inside its isolation (observations and obtained roots
  applied at the turn); debug builds compare it with the commit's own.
- **Planning tail on workers** (B4): the requested targets' runfiles
  artifacts (the hidden top-level group: Runfiles.getAllArtifacts resolved
  and deduplicated per target), and the scans for source directories and
  missing sources (only candidate nodes are visited in order, so the
  filesystem probes and their order are the sequential ones); unreachable
  nodes, spent action inputs and compute snapshots are dropped on a dropper
  thread.

Outputs: the `BAZEL_RS_DUMP_ACTIONS` dump of `build --nobuild` and `test
--nobuild //src/...` (6,483,434 lines, md5 `bc225d8e…`) and the rest of
stderr (but the elapsed time) identical to main's at the default threads,
`--loading_phase_threads=1` and 32 workers with 5 ms jitter;
`BAZEL_RS_DUMP_ACTIONS=remote` identical; `BAZEL_RS_VERIFY_AHEAD=1` clean
(32,605 prepares, 27,648 commits ahead and 38,757 output summaries
verified); `aquery` text and summary of `//src/python/bworker/...` and
`//src/beyondwork.ai/tools/oauth2token/...` identical
(`//src/python/github_backup/...` could not fetch its arm64 debs from
snapshot.debian.org, on main too); the CI `affected` output and every
`--keep_digests` file identical, also on the P1 (`470886f0 → 7ab6d35d`) and
P2 pairs; a snapshot this branch records verifies at threads 1 and 8.
Tests: `known_outputs_decide_commits_ahead_within_a_package` (siblings commit
ahead of a known, uncommitted one; exact and prefix conflicts with it are the
sequential errors; fails if known outputs are ignored),
`declared_outputs_checked_on_workers_fail_at_their_turn`.

### Why the coordinator is still the bottleneck

Sampled after these changes (pprof of the coordinator, ~49 s): phase-1
analysis 8.3 s, implementations computed at their turn 8.0 s, commits 9.8 s,
prepares 6.6 s, aspect applications 2.3 s, the tail 2.9 s (profiling build),
package loading 1.1 s; the profile is flat (the largest leaf, `Engine::target`
lookups, is 1.7 s). Three experiments rule out cheaper fixes for the
implementations computed at their turn (~7,000 entries, `turn_compute`
11–12 s):

- Computing them on a worker while the coordinator commits others ahead:
  the coordinator then waits 19.8 s (wall 60.6 s). They are chain links
  whose predecessor committed just before their turn: nothing else is ready.
- 16 or 32 analysis workers (twice as many in flight): `turn_compute`
  unchanged (11.0 s).
- Skipping the 2,030 of 2,357 `config_setting` flushes whose Starlark flag
  keys were already planned (longer walks; output order not preserved, an
  experiment only): `turn_compute` unchanged (12.6 s). Walk length is not
  the cause.

So the chains advance at the coordinator's pace: every link's prepare and
commit is coordinator work in plan order, and the at-turn entries are the
links the coordinator reaches before the worker could have computed them.
Shaving the coordinator's per-target work only moves this a little; the
whole per-target unit has to leave it.

### Plan B S0 baseline (counters for the per-target pipeline)

`BAZEL_RS_TIMING=1` prints, after the review fixes (HEAD of this branch,
`build --nobuild //src/...`, 133,156 entries, each counted once):

- `PERF-PARALLEL fallback` — why an entry ran or committed where it did:

| reason | `SNAPSHOT=off` | recording |
|---|---|---|
| `native` (classified: a native rule, committed at its turn) | 92,041 | 92,041 |
| `native_unclassified` (reached at its turn before classification) | 2,358 | 2,358 |
| `committed_ahead` | 25,121 | 25,429 |
| `ready_late` (dependencies committed, not prepared before its turn: woken by the previous turns) | 6,661 | 6,540 |
| `ineligible_transition_or_restricted` | 2,166 | 2,209 |
| `late` (computed ahead, finished at its turn) | 1,982 | 1,797 |
| `platform_unknown` (prepared with the target platform deferred) | 1,349 | 1,349 |
| `unsettled_package` / `enclosing` / `enclosed` / `outputs` (last try) | 883 / 47 / 154 / 11 | 809 / 49 / 146 / 15 |
| `dependency_late` (classified, still waiting at its turn) | 261 | 280 |
| `attribute_aspects` | 48 | 48 |
| `ineligible_outputs_elsewhere` / `workspace_status` / `first_fill` | 47 / 12 / 0 | 48 / 12 / 1 |
| `unsettled_barrier` / `requirements` | 15 / 0 | 15 / 10 |

- `PERF-PARALLEL shared total=47,761 unmerged_owner=4,263` (recording:
  4,692): shared actions, and how many met an owner off its turn (the
  first owner committed ahead, or the registering target ahead of its own
  turn) — the merges a delta pipeline must order.
- `PERF-PARALLEL effects` (recording only; nothing is observed with
  `SNAPSHOT=off`): off-turn work whose held effects reached the turn —
  `prepare_observations=31,879 compute_observations=13,651
  classify_observations=10`; no roots or console.

`BAZEL_RS_NO_ARENA_BINDING=1` keeps the workers in their own jemalloc arenas
(to measure arena-lock contention once prepares and commits run on many
workers).

### S1: commit deltas on the workers (branch `analysis-s1`)

Design: docs/PARALLEL_ANALYSIS.md, "S1". Same setup (private clone at
`80d6724f3a`, repositories warm, `BAZEL_RS_SNAPSHOT=off`, storagebox
overrides, `nice -n 10`, the shared host, load ~7), runs interleaved:

| command | main 75ef424a | this branch | genuine 9.3.1 |
|---|---|---|---|
| `build --nobuild //src/...` wall | 53.4 / 52.7 s | 52.8 / 53.4 s | 10.9 / 10.7 s |
| — CPU user + sys | 99.5 + 45.4 / 99.9 + 45.4 s | 101.9 + 37.7 / 101.7 + 41.0 s | 315.1 + 15.9 / 311.4 + 15.3 s |
| — peak RSS | 8,700 / 8,712 MB | 8,726 / 8,731 MB | 5,669 / 7,122 MB |
| — stage totals: commit / prepare | 8.8 / 8.4 s | 8.5 / 8.3 s (+ 1.3 s delta builds on workers) | – |
| `test --nobuild //src/...` wall, CPU, RSS | 53.8 s, 101.1 + 40.9 s, 8,826 MB | 53.9 s, 102.5 + 43.5 s, 8,720 MB | 10.2 s, 285.0 + 15.1 s, 5,629 MB |
| CI `affected --base=HEAD^` wall, RSS | 68.4 s, 7,367 MB | 65.9 s, 7,404 MB | – |
| `affected` P2 pair (warm) RSS | – | 175.8 s, 9,669 MB | – |

`PERF-PARALLEL delta (count:commit)` of `build`: `worker=31,141:0.42s
coordinator=7,273:4.35s delta_meets_producer=331:0.76s
delta_workspace_status=12 recomputed_on_worker=8`; the fallback line is
otherwise main's (committed ahead 23,711, ready_late 6,937, late 2,773).

Why the gate (coordinator `commit` ≤3 s, wall −5 s) is not met: before S1
the commits ahead of their turn cost the coordinator ~2 s
(`BAZEL_RS_NO_WORKER_DELTAS=1`: 6.6 s for every Starlark commit, of which
4.4 s at the turn); S1 moves those to the workers (0.4 s of merges), but the
`commit` stage is now the entries computed at their turn (4.3 s: their
runfiles support, declared-output checks and input resolution, built where
they are computed) and the native rules (~3 s, `filegroup` 2.1 s). The
walk's tail is latency-bound (the turn waits for chain links), so the
coordinator time saved does not show in the wall. Moving the at-turn links
off the coordinator needs their prepare there (S2).

### S2, first part: the at-turn cascade roots (branch `analysis-s2`)

Design: docs/PARALLEL_ANALYSIS.md, "S2, first part". Same setup (private
clone at `80d6724f3a`, repositories warm, `BAZEL_RS_SNAPSHOT=off`,
storagebox overrides, `nice -n 10`, the shared host, load 5–14), runs
interleaved; genuine 9.3.1 re-measured the same night (`--batch`, its own
output base, repositories fetched by a first run):

| command | main 8054e96e | this branch | genuine 9.3.1 |
|---|---|---|---|
| `build --nobuild //src/...` wall | 54.0 / 54.0 s | **47.9 / 48.1 s** | 11.0 / 11.0 s |
| — CPU user + sys | 98.7 + 41.3 / 100.3 + 37.5 s | 101.1 + 39.4 / 105.1 + 40.1 s | 290.0 + 24.4 / 277.2 + 21.4 s |
| — peak RSS | 8,730 / 8,682 MB | 8,732 / 8,844 MB | 7,205 / 8,122 MB |
| — implementations computed at their turn (`turn`, `turn_compute`) | 7,310 / 7,177, 11.8 s | 2,554, 4.0 s | – |
| — stage totals: commit / prepare | 8.6 / 8.5 s | 6.7 / 8.5 s | – |
| — coordinator thread CPU (`coordinator_cpu`) | – | 33.5 + 4.2 s | – |
| `test --nobuild //src/...` wall, RSS | 54.4 s, 8,712 MB | **48.5 s**, 8,852 MB | 11.6 s, 7,320 MB |
| CI `affected --base=HEAD^` wall, RSS | 69.0 s, 7,400 MB | **60.3 s**, 7,368 MB | – |
| `affected` P2 pair (warm) | – | 163.8 s, 9,618 MB | – |

A recorded snapshot's `verify` peaks at 9,501 MB on main and 9,639 /
11,077 MB on this branch (two runs: jemalloc decay timing; the live peak is
bounded by the held entries, 8 × the in-flight bound).
`PERF-PARALLEL fallback` of `build`: `committed_ahead=32,863
native_committed_ahead=87,114 ready_late=2,240 late=2,028
unsettled_package=1,066 native_ready_late=3,716` (main: `committed_ahead`
~23,900, `ready_late` ~6,900, `late` ~2,600, every native at its turn).
`BAZEL_RS_NO_ARENA_BINDING=1`: 49.8 vs 50.0 s, the same coordinator CPU —
no arena contention while the prepares stay on the coordinator.

**Where the wall goes now.** `PERF-PARALLEL lat` (`build`): an entry
committed ahead waits on average 3.4 ms from ready (its last dependency
merged) to dispatched (the coordinator prepares it), 1.0 ms on its worker
(compute and delta), 1.9 ms until the coordinator notices it finished —
the coordinator's turnaround is ~5 of each link's ~6 ms. Moving the
remaining at-turn implementations to a worker as well (an experiment,
`BAZEL_RS_TURN_ON_WORKER`, not kept) cut the coordinator's CPU from 38.8 to
33.6 s but its wait for workers grew from 6.5 to 15 s and the wall from 49.6
to 52.0 / 52.6 s: what is left at the turn is the critical path of the
walk's chains, advanced at the coordinator's pace. So moving the prepares
to the workers (the rest of S2: −8.5 s of coordinator CPU) pays only when
the links stop going through the coordinator's turnaround: the ready queue
dispatching prepare + compute + delta from a merged dependency without the
coordinator in between (S2's second part) and publishing before the merge
(S3). The wait is spread over the walks (2 walks of ~10,000 entries: 1.6 s;
66 of ≤4,096: 2.0 s; 1,085 of ≤256 entries — `config_setting` flushes —:
2.6 s), so narrowing the flushes (S4) matters as much.

Review experiment (not kept): noticing finished computes inside the
lookahead's prepares and native commits too (earliest first) cut the
finished → noticed latency from 2.0 to 0.8 ms and ready → dispatched from
2.6 to 2.2 ms for entries committed ahead, but the at-turn entries'
ready → dispatched went from 2.3–3.0 to 3.9–4.4 ms and the wall did not move
(50.2 vs 49.2 / 50.9 s, wait 6.2 vs 5.8 / 6.2 s).

### S2, second part: prepares off the coordinator's state (branch `analysis-s2b`)

Design: docs/PARALLEL_ANALYSIS.md, "S2, second part". Same setup (private
clone at `80d6724f3a`, repositories warm, `BAZEL_RS_SNAPSHOT=off`,
storagebox overrides, `nice -n 10`, the shared host, load 8–15), runs
interleaved; genuine 9.3.1 the same night (`--batch`, its own output base,
repositories fetched by a first run):

| command | main f260892f | this branch | genuine 9.3.1 |
|---|---|---|---|
| `build --nobuild //src/...` wall | 46.3 / 47.2 s | 47.3 / 48.6 s | 11.0 / 10.2 s |
| — CPU user + sys | 93.4 + 41.9 / 99.0 + 44.9 s | 95.5 + 39.0 / 101.1 + 39.7 s | 295.9 + 19.3 / 284.6 + 14.9 s |
| — peak RSS | 8,808 / 8,820 MB | 8,757 / 8,887 MB | 6,558 / 5,988 MB |
| — coordinator thread CPU | 32.9 + 4.3 / 33.4 + 4.2 s | 33.9 + 4.3 / 34.9 + 4.1 s | – |
| — with 7 prepare threads; and without the arena binding | – | 49.0 s; 48.9 s (coordinator 32.8 + 4.2 s both) | – |
| `test --nobuild //src/...` wall, CPU, RSS | 48.8 s, 102.1 + 38.3 s, 8,909 MB | 48.2 s, 104.2 + 39.7 s, 8,844 MB | 11.0 s, 320.6 + 14.6 s, 5,611 MB |
| CI `affected --base=HEAD^` wall, RSS | 64.4 s, 7,370 MB | 60.7 s, 7,396 MB | – |
| `affected` P2 pair (warm) | – | 166.6 s, 9,635 MB | – |
| `BAZEL_RS_SNAPSHOT=record` peak RSS | 10,252 MB | 10,250 MB | – |

No change, as the counters predicted. `PERF-PARALLEL prepare` (`build`,
default): 36,168 prepares ahead of their turn through the shared context in
26,991 batches (1.3 entries each), 10 fallbacks (a source file not
memoized); the coordinator's time in them 5.7 s (4.6 s preparing, the rest
dispatching), in the turn's prepares 3.6 s (2.8 s of it the rules applying
attribute aspects, which never prepare ahead). With 7 prepare threads the
batches took as long (`batch_run` 4.4–4.7 s, the `prepare` stage's CPU
8.4 → 10.5–13.4 s: a prepare on another core costs about twice the CPU, and
the coordinator waits for the batch's slowest); classifying until the room
was full (10,800 batches of 3.3) did not help either. Without the jemalloc
arena binding, with 7 prepare threads: 48.9 vs 49.0 s and the same `prepare`
stage — no arena contention.

**Where the wall goes** (pprof of the coordinator, ~40 s CPU): phase-1
analysis 12.8 s and package loading 1.8 s; the walks ~23 s — commits ahead,
native commits ahead, classification and the prepare batches 6.2 s,
implementations computed at their turn 4.8 s, commits at the turn 3.6 s,
prepares at the turn 4.1 s (aspects 2.8 s), the rest bookkeeping. Latency
per chain link (`PERF-PARALLEL lat`, committed ahead): ready → dispatched
2.8 ms, on the worker 1.1 ms, finished → noticed 2.1 ms. **Critical path**
(`PERF-PARALLEL chains`): the walks' deepest chains of Starlark
implementations sum to 1,121 links (164 over the 2,821 entries of walks of
≤16 entries, 452 over the ≤256 ones — the `config_setting` flushes —, 465
over the ≤4,096 ones, 40 over the two large ones; the deepest 47): ~6.7 s at
today's ~6 ms per link, ~1.2 s at the worker's 1.1 ms. So the chains bound
the wall at a few seconds even before S3; the coordinator's own work (~38 s
of a ~47 s wall: phase 1 and the flush walks' merges, natives and at-turn
links) is what keeps the wall far above 25 s. The gate of S2 (coordinator
prepares ≤1 s, ready → dispatched ≤0.5 ms, wall ≤~30 s) is not met; even
the prepares the plan meant to move (~4.5 s ahead, the rest are the turn's
links and aspect applications) bound what moving them all to workers could
buy at ~10% of the coordinator. S4 next (phase 1 in parallel, the
`config_setting` flushes narrowed): phase 1 is the largest serial block,
and its fork-join shape (many independent keys per batch) suits the `Sync`
engine and the shared-read contexts this branch adds.

## Pattern loading: blocked units re-run on the workers (branch `pre-analysis-x`)

The pre-analysis span X (start → `select.loaded`, docs/EVALUATOR.md) was
3.0 s on ~/code `//src/...` warm, 2.6 s of it the pattern's loading: the
514 worker units finish in 0.05 s, but 300 of them abort with the
no-demand sentinel — on a registry module root (`@@rules_go+`,
`@@aspect_rules_js+`, `@@aspect_bazel_lib+`, `@@rules_oci+`), an extension
group (`npm`, `pip`, `crate`, `workspace_deps`) or a generated repository
— and the coordinator re-ran every one of them inline at its commit slot,
one after another (`//src/ts` 0.73 s, `//src/website3` 0.46 s, ~250 Go
packages ~0.7 ms each).

Now (`crate::engine::pattern_pool`):

- **Pipelined commits.** The workers take units from a queue; the
  coordinator commits in pattern order as soon as each unit is there.
- **Re-runs on the workers.** A worker's sentinel names the demand that
  blocked the unit (`pattern_pool::blocker`: the `runtime N:KEY` /
  `repository @@NAME` text `RepositorySet::sentinel_ready` decides on).
  The coordinator satisfies each demand where the sequential engine does —
  the first unit blocked on it re-runs inline at its slot, as before —
  and after every commit (and while it waits for a unit) it queues again
  every later unit whose demand is ready now. A re-run aborted on another
  demand waits for that one (at most 8 re-runs, then the unit re-runs
  inline at its slot). This is phase-1 prefetch's retry
  (`Prefetcher::retry`) for the pattern's units: the same no-demand +
  reuse-only workers, beside the coordinator, under the same two guards
  (docs/PARALLEL_ANALYSIS.md: probes publish only while no evaluator
  runs; link decisions that depend on the materialized roots abort). The
  coordinator commits a re-run unit like a first attempt: the aborted
  attempts' module print events first (as before an inline re-run), then
  the unit's prints, roots and observations at its slot. Repository and
  extension demands, their output, errors, the lockfile and the snapshot's
  recordings stay where the sequential engine has them.
- **Waves.** The units of one demand re-run in waves of 1, 2, 4, … in
  pattern order, each once the previous one finished: units blocked on
  one demand mostly load the same files (the `@npm` hub loads 1,700
  `__links` `.bzl` files), and a later wave finds them in the shared
  `.bzl` cache. Without waves 18 `//src/ts/...` units walked the same
  graph at once (the shared cache claims a module only at its finish, so
  every traversal entered every module) and `//src/ts` took 1.6–1.9 s on
  a worker instead of 0.7 s.
- **Loads warmed ahead from the workers.** A pattern unit is committed
  (up to the pattern's first error), so its workers run in committed-unit
  mode (`repository_demand::committed_unit`): `warm_load_roots` requests
  the repositories of the files it loads on the speculation pool and the
  module-root warmer, as the coordinator's loads do — the 3,531 `__links`
  repositories validate in parallel instead of one probe after another
  on the worker.
- **Link policy by count.** `RepositorySet::link_policy` built the list of
  every materialized root (~4,000 names and paths) and compared it with
  the cached one on each call — on every `.bzl` reached through a symlink.
  Roots only begin to count and never change their path, so the same
  count is the same list (`roots_materialized_count`, no allocation).

| ~/code `//src/...`, warm, `BAZEL_RS_SNAPSHOT=off`, LTO release, 3 runs each (shared host) | main 521de384 | this branch |
|---|---|---|
| X (`select.loaded`) | 2.80 / 3.12 / 4.61 s | **2.09 / 2.10 / 2.51 s** |
| — `startup.configuration` | 0.40 / 0.42 / 0.77 s | 0.39 / 0.39 / 0.47 s |
| — `select.discovered` → `select.loaded` | 2.36 / 2.67 / 3.80 s | **1.68 / 1.68 / 1.90 s** |
| `plan.phase1_analysis_done` | 38.4 / 38.9 / 40.7 s | 36.9 / 37.3 / 37.3 s |
| cold fetch (fresh cache): X, wall | 28.0 s, 256.8 s | 24.7 s, 253.6 s |

`PERF-PATTERN units= workers= retried= waited= wait=` (`BAZEL_RS_TIMING`)
counts the re-runs: ~600 on ~/code. The remaining critical path (quiet
host): configuration 0.39 s → the coordinator's sequential inline re-runs
up to the `npm` extension (`//src/ansible`'s `pip` 0.14 s,
`//src/beyondwork.ai/idl/api/taskrunner`'s `npm` 0.26 s, ready at
~0.93 s) → `//src/ts` on a worker (0.73–0.86 s: 1,700 `.bzl` modules on
one thread) → `//src/website3`'s inline re-run (0.2 s: a generated
repository only the coordinator executes) → `select.loaded` ~2.1 s.

Not done (each needs its own design): ~~evaluating the extensions the
lockfile names at startup~~ (done next, branch `parallel-extensions`);
overlapping the Starlark-option loads with discovery (~0.08 s: the first
wave of units only takes 0.05 s); reusing the resolved module graph
(~0.1 s).

## Module extensions prepared ahead (branch `parallel-extensions`)

Once the module graph is resolved and the extension catalog built (at
startup, or right after the catalog's lazy construction — never inside its
initializer, which the speculations read), the extension groups the root
module uses are prepared on up to 8
background workers (`crate::extension_speculation`, scheduling;
`RepositorySet::extension_speculation_job`, the preparation) while the
coordinator configures and loads. A speculation is the coordinator's own
preparation (`compute_ordinary`, the former closure of
`evaluate_ordinary_group`) run in no-demand + reuse-only mode, and only
when the group's lockfile entry is reusable — the extension itself never
runs ahead. Everything the preparation would make visible is held back:

| effect | held as | committed by |
|---|---|---|
| console writes (definition `.bzl` prints, the use_repo fixup warning) | `ui::DeferredConsole` | written last, in order |
| snapshot observations | detached capture | replayed |
| direct / generated roots it obtained | `UnitRoots` | `note_committed_roots` |
| registry module roots its loads read | warm store trees (`speculative_registry_root`, never the demand slot) | demanded in first-use order; the same tree, or the outcome is dropped |
| definition `.bzl` cache entries, definition modules, main-repository module code | `loading::speculative_definitions` (thread overlay) | installed; in record mode their replay-capture lookups repeated in a replay scope |
| hidden-lockfile consultation, lockfile record | not done | `extension_lock` + `reusable_lock_entry` again, then `record_extension_lock` |
| ignored subdirectories it computed | not memoized | — (the sequential engine computes them) |

The coordinator takes the outcome inside the group's demand slot (its
first sequential demand, `Speculations`-style `take`: waiting while it
runs) and commits it there: the registry roots first, then the groups whose
speculative preparations it used (below), then the lockfile check against
the inputs as they are now — the same entry still reusable, nothing
printed — then the record, observations, roots, definitions and output.
Any difference drops the outcome and the coordinator prepares the group
itself; what the commit did up to there (silent root demands, other
groups' commits, the check's observations and consultation) is what that
preparation does first anyway. Outcomes nobody takes are dropped with the
pool (`ExtensionSpeculationGuard`, end of the command); a running
speculation is cancelled, not awaited.

**Chained speculations.** `npm` loads `@aspect_tools_telemetry_report`
(generated by `telemetry`), `crate` a repository of `rules_cc`'s
`compatibility_proxy`: a speculation blocked on another group's
preparation requests that group and, once its speculation finished,
uses that speculative preparation (`adopt_speculated_group`; the group
slot holds an `Arc<PreparedOrdinary>` so the very preparation can be
shared). Its commit prepares the used group on the coordinator first and
keeps the outcome only if that is the same preparation (pointer
equality); a speculation that wrote output before using another group is
not kept (that output would precede the other group's).

**Blocked committed units wait for running speculations.** The
`//src/website3` inline re-run was not a coordinator-only repository: its
re-runs on the workers each found the next `npm_website3__*` repository
its loads reached executing on the fetch pool (requested by its own
`warm_loads`), aborted, and re-ran from scratch once it was published. A
committed unit's reuse-only probe now waits for that speculation
(`Speculations::wait_running`, marking its transfers demanded) and serves
the root it published (one that printed stays the coordinator's).

Invariants checked on ~/code `//src/...` (repositories fetched, LTO
binaries, main `ada4ba6d` vs this branch): stderr identical (warm and
cold, `BAZEL_RS_SNAPSHOT=off`); `MODULE.bazel.lock` and the hidden
lockfile identical (warm and cold); the snapshot recording identical (the
sorted record dump of a `record` run, timings stripped), and a snapshot
saved with speculation hits and verifies (`BAZEL_RS_SNAPSHOT=verify`);
`BAZEL_RS_DUMP_ACTIONS` identical (6,483,434 lines); `affected` CI step
(`HEAD^`, 737 of 802), P1 (20) and P2 (53) outputs and digests identical.
tests/extension_speculation.rs pins threads 1/2/8/32 + jitter (+ off) on a
registry module's extension (warm-root loads, prints, fixup warning), a
chained one, a failing one, one never demanded, and one whose recorded
input a local repository rule rewrites before the demand (dropped; also
chained: both dropped), with snapshots off and recording.

| ~/code `//src/...`, warm, `BAZEL_RS_SNAPSHOT=off`, LTO, interleaved, shared host (load 15–26) | main `ada4ba6d` | this branch |
|---|---|---|
| X (`select.loaded`), 8 runs | 2.09 / 2.14 / 2.22 / 2.24 / 2.36 / 2.77 / 3.25 / 3.81 s (median 2.30) | 1.87 / 1.87 / 1.88 / 1.94 / 1.97 / 2.00 / 2.04 / 2.10 s (median 1.96) |
| `plan.phase1_analysis_done` | 37.5–41.5 s | 36.7–38.9 s |
| cold fetch (fresh cache, one run each): X, wall | 23.3 s, 254.5 s | 22.8 s, 240.5 s |

On the coordinator `npm` costs 0.056 s at its demand (was 0.22–0.26 s:
the check and the record; the speculation ran 0.32–0.55 s), `pip` 0.106 s
(was 0.17 s: 0.09 s of it is materializing `@grpc+`, the sequential root
demand, plus its 0.07 s input check), `crate` is mostly not speculated (it
waits for `cc_compatibility_proxy`, a local repository only the
coordinator executes). The critical path now: configuration 0.43 s → the
coordinator's commits up to `npm`'s demand (~0.75 s) → `//src/ts` on a
worker (0.83 s, 1,700 `.bzl` modules on one thread) → `//src/website3`,
which re-runs in a later wave of the `@aspect_rules_js+` batch
(1.41–1.98 s) → `select.loaded` ~1.95 s.

Residuals (soundness review): a speculation reads `.bzl` files and
lockfile inputs at its time, not the sequential one (as pattern and
prefetch workers do); recorded inputs are checked again at the commit, a
`.bzl` edited in between is not (the snapshot records what was read). A
registry root materialization that prints (download retry warnings, cold)
writes before the speculation's held output rather than interleaved with
it. Not done, each its own step: publishing a quiet speculation into the
group slot before its sequential demand (workers would start `//src/ts`
at ~0.55 s instead of ~0.77 s; a publication cannot be withdrawn, so an
input that changes before the commit would have to fail the build);
speculating evaluations without a reusable entry (the evaluated path's
re-probe and taints); the `@aspect_rules_js+` waves holding
`//src/website3` behind the `//src/ts/...` units (~0.35 s of X); `//src/ts`
itself (0.83 s on one thread).

## What remains

The goal (cold analysis faster than genuine 9.3.1: 10 s wall on this host)
needs the coordinator to stop doing per-target work. In order:

- **B1+B2 together: the per-target unit on workers.** A worker prepares the
  target (dependency views, toolchain and exec-group contexts, ctx surface),
  runs its implementation and computes its commit as a delta (output
  registrations with their artifacts, declared entries, providers, dry
  files, runfiles and their support nodes, lowered nodes, source inputs,
  action executions, notes, observations, tracker events); the coordinator
  merges deltas strictly in plan order. Preconditions, each its own commit:
  - the artifact index as a sharded concurrent map read with the target's
    stamp (`ArtifactIndex::stamp_ahead` defines the visibility) and the
    producers stamped by plan index, so a worker resolves inputs and a
    merge decides registrations;
  - the prepare's memos made shareable: the views memo (and its
    `view_accesses`), `target` lookups (rule/source memos), input groups and
    input memos (keyed by identity: sharded maps or per-target then merged),
    toolchain/exec-group contexts and execution tables (read-only after
    phase 1 analyzed the key), the repositories' mapping lookups;
  - `Engine` split so that a worker holds a `Sync` read view (configurations,
    platforms memo, rule declarations, ignored subdirectories, repository
    set) and never `&mut Engine`.
- **Conflicts at the merge.** With known outputs (this branch) a later entry
  waits only behind earlier entries whose outputs are not known yet
  (`PERF-PARALLEL unsettled package=` ~330,000 retries remain). Removing
  that wait needs optimistic merges: a registration that meets a
  later-stamped producer wins (first owner in plan order); the later
  target's merge is redone at its turn. For a conflict that turn fails and
  the build stops, so nothing after it matters; for a shared action the
  later target's artifacts change producer, so its dependents' deltas (and
  the views memo entries built from it) are discarded and redone.
- **Aspect applications as deltas** keyed like their `AspectKey` (2.3 s on
  the coordinator, and the barrier they impose on commits ahead).
- **Phase 1 in parallel (B3)**: analysis keyed like Skyframe's configured
  target functions; the `config_setting` flushes reduced to the flag keys'
  own closure (merged ahead), with the coordinator's console writes held
  until the walk passes the flush point, so the sequential output order and
  first error stay as they are.
- **Smaller serial costs** (B4): `Engine::target` and label hashing (SipHash
  maps keyed by labels and configured target keys, ~1 s), label re-parsing
  in hot paths (`parse_canonical` ~1 s), depset walk dedup sets (~1 s);
  PyCompile nodes holding the shared runtime run by reference (the
  scheduler, action keys and snapshot codec accepting an input group in the
  middle of the order).
