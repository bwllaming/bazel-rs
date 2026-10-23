# Layer-1 analysis performance: benchmark and instruments (W0-A)

This page covers the measurement side of the Layer-1 program (in-process
analysis bookkeeping, `/tmp/opencode/perf-research/PLAN-L1.md`):

- how to run the A/B benchmark (`scripts/ab_bench.py`) and its target sets;
- how to read the `PERF`, `PERF-STAGE` and `PERF-ALLOC` lines;
- the baseline measured with the base binary, and where the memory goes.

The stage timers themselves (M5) are described in docs/PARALLEL_ANALYSIS.md,
"P3 instrumentation".

## Switches

| Variable | Output (stderr only) | Cost when unset |
|---|---|---|
| `BAZEL_RS_TIMING=1` | `PERF` phase marks, `PERF-REPO`, `PERF-STAGE` report | one environment or atomic load per hook |
| `BAZEL_RS_TIMING_ALLOC=1` | `PERF-ALLOC*` retained-memory breakdown at the end of phase 2 | one atomic load |
| `BAZEL_RS_DUMP_ACTIONS=1` | full identity of every planned action (`ACTION …`) | none |

With none of them set, stdout and stderr are byte-identical to a build
without the instruments. `BAZEL_RS_TIMING_ALLOC` works with or without
`BAZEL_RS_TIMING`. It walks every planning structure, which takes about
0.4 s on oauth2token and 0.9 s on B2, and allocates its own bookkeeping.
Timings and VmHWM from a run with the breakdown on are therefore not
comparable with a plain timing run.

## Benchmark recipe

The workspace is `~/code`, which is read-only (see
`/tmp/opencode/perf-research/CONTEXT.md`, HARD RULES). The recipe mirrors
the user's default lockfile mode (update mode) with a private native
lockfile. Never use `--lockfile_mode=off`: it re-downloads registry files
and re-runs module extensions on every run, about 10.7 s of network wait.

```sh
B=/tmp/opencode/<you>-ab; mkdir -p $B
cp /home/coder/code/MODULE.bazel.lock $B/lock.seed     # pinned seed, read once
python3 scripts/ab_bench.py \
  --a /tmp/opencode/perf-research/bin/base-6bebe72-bazel-rs \
  --b $CARGO_TARGET_DIR/release/bazel-rs \
  --cache-a $B/cache-a --cache-b $B/cache-b \
  --lockfile-seed $B/lock.seed --lockfile-dir $B \
  --out $B/a-nobuild --mode nobuild --warmups 2 --runs 3 \
  //src/beyondwork.ai/tools/oauth2token:all
```

For every run, `ab_bench.py` does the following, from `~/code` and under
`env -i` with a private HOME and TMPDIR:

```sh
<bin> --ignore_all_rc_files --cache-dir=<cache> build [--nobuild] \
  --experimental-builtin-modules --lockfile_mode=update \
  --native-lockfile=<lockfile-dir>/lock-<a|b> --remote_cache= \
  --noremote_upload_local_results <targets>
```

It always sets `BAZEL_RS_TIMING=1` and `BAZEL_RS_DUMP_ACTIONS=1`.

- **Lockfiles.** The seed is copied once to `lock-a` and `lock-b`. Each
  side then keeps its own lockfile, written by update mode, so one
  `--lockfile-dir` is shared by all target sets.
- **First runs are slow.** The first run of a fresh cache fetches the
  public dependencies; with a fresh lockfile it took about 100–115 s for
  oauth2token, and about 400 s for B2 on a cache already warm from
  oauth2token. The first `--mode build` run executes all 689 actions
  (about 90 s). Count these runs as warm-ups.
- **Run order.** The sides run interleaved (a0 b0 a1 b1 …). The machine is
  shared, so timings are indicative; compare medians.
- **Equivalence.** The sides are equivalent when the last runs have the
  same exit code, stdout, stderr outside the action dump and PERF lines,
  and the same normalized `BAZEL_RS_DUMP_ACTIONS` dump. Normalization
  replaces the cache prefix with `<CACHE>`, `repositories/direct/trees/<hash>`
  with `trees/<RUN>`, and `lock-a`/`lock-b` with `lock-<SIDE>`. Differences
  are written to `OUT/diff-{rest,dump,stdout}.{a,b}`. The exit status is 0
  iff the sides are equivalent.
- **Measurements.** Each run writes `OUT/<side>-<i>.json` with:
  - wall time;
  - the child's user and sys CPU and max RSS (`os.wait4` rusage);
  - the parsed PERF marks;
  - the PERF-STAGE totals and counters;
  - any `PERF-ALLOC*` rows.

  `OUT/report.json` has these per run, plus medians over the measured
  runs (`summary`), the comparison, the binaries' sha256 and the
  `~/code` canary (HEAD and lockfile sha256, before and after).
- **Other options.**
  - `--sides a|b` runs one side only, for baselines.
  - `--extra-env-b K=V` sets a variable on one side, for example
    `BAZEL_RS_TIMING_ALLOC=1`.
  - `--compare-only` rebuilds `report.json` from the files already in
    `OUT` without running anything. It also reads directories written by
    the former `ab_equiv.py`.
  - With the default `--warmups 0`, `--runs N` behaves exactly like
    `ab_equiv.py`: N runs, and the last one is compared.

### Target sets

| Set | Command | Size | Warm wall (base) |
|---|---|---|---|
| (a) | `--mode nobuild //src/beyondwork.ai/tools/oauth2token:all` | 18,981 planned targets, 707 Starlark computes, 689 actions in the dump | 47.2 s |
| (a) | `--mode build //src/beyondwork.ai/tools/oauth2token/...` | warm: 0 executed, 689 cache hits (691 dump entries) | 49.7 s |
| (b) B2 | `--mode nobuild //src/beyondwork.ai/prober/... //src/beyondwork.ai/pvcautoscaler/... //src/beyondwork.ai/decommission/...` | 24,628 planned targets, 2,588 computes, 3,615 actions | 338 s |

Run (b) at least once per side before reporting.

## Reading the output

### `PERF` phase marks

```
PERF    46.179s rss=1410424kB plan.phase1_analysis_done hwm=1410424kB
```

Each line gives the wall time since process start, VmRSS, the mark name
and VmHWM (peak RSS). `hwm=` was added by W0-A after the mark name, so any
parser of the original `PERF <t>s rss=<kB>kB <mark>` prefix still works.

The marks are:

| Mark | Meaning |
|---|---|
| `main.start` | process start |
| `select.start` / `select.end` | target-pattern expansion |
| `plan.start` | start of analysis |
| `plan.phase1_analysis_done` | end of configured-target analysis; most phase-2 work already ran inside its `plan_pending` flushes |
| `plan.phase2_actions_done` | end of implementations and action planning |
| `plan.end` | end of planning |
| `execute.end` | end of execution |

The kernel updates VmHWM lazily, so it can lag a short peak and can read
lower at a later mark (for example 1,421,536 kB at `phase2_actions_done`
and 1,413,604 kB at `plan.end`). Use the largest `hwm` over the marks
together with the run's rusage max RSS that `ab_bench.py` records.

### `PERF-STAGE` (M5 report plus the W0-A counters)

The `total`, `class` and `critical-path` rows are described in
docs/PARALLEL_ANALYSIS.md. W0-A adds two rows:

```
PERF-STAGE flush-batch flushes=1237 planned=18851 empty=0 min=1 median=2 p90=61 max=615 phase2=130
PERF-STAGE views edges=8669 constructed=8669 distinct=2181 artifact_file=9309489
```

- **`flush-batch`.** A `config_setting` with a Starlark `flag_values` key
  plans every pending target in phase 1; each of those calls is one
  flush. The row gives the number of targets planned per flush:
  - `flushes` is the number of flushes and `planned` their total;
  - `empty` is the number of flushes that planned nothing;
  - `min`, `median`, `p90` and `max` use the nearest-rank method;
  - `phase2` is the number of targets the final phase-2 call planned.

  The batch size limits how many targets a parallel compute (P3) could
  overlap. PLAN-L1 §4 reopens P3 only at a median of 32 or more; the
  measured median is 2.
- **`views`.** These are dependency-view requests:
  - `edges` is the number of `plan_dependency_view` calls, one per
    dependency edge of a Starlark rule or aspect;
  - `constructed` is how many of them built a view;
  - `distinct` is the number of distinct (label, edge configuration)
    keys, which is what W1a's view memo would build;
  - `artifact_file` is the number of `Artifact::file` conversions.

  Today `constructed` equals `edges`. W1a calls
  `plan_timings::note_dependency_view(key, false)` on a memo hit.

  Integration-20 (on top of perf-w1): the memo exists, so `constructed`
  equals `distinct` (a memo hit reports `constructed=false`), and
  `artifact_file` counts the Files computed in `Artifact::compute_file` —
  once per Artifact, since W1a caches the File (`Artifact::file` only clones
  it). The numbers below are W0-A's, measured before W1.

The counters cover everything up to the end of phase 2, which is where the
report is printed.

### `PERF-ALLOC` (`BAZEL_RS_TIMING_ALLOC=1`)

The breakdown is printed right after phase 2, when the planning structures
are at their largest. The implementation is `src/engine/alloc_report.rs`.

```
PERF-ALLOC <bytes> <bucket>                        largest first
PERF-ALLOC-DETAIL <bytes> <bucket>/part:<part>     Rule data by part (inside its bucket)
PERF-ALLOC-HEAP <bytes> <group> heaps=<n>          frozen-heap arena bytes by first referrer
PERF-ALLOC-SIZES rule=<bytes> ...                  inline sizes of key types
PERF-ALLOC-COUNTS packages=<n> rules=<n> ...       entries of the main structures
PERF-ALLOC-SUMMARY total=<bytes> rss=… hwm=… covered=<total/rss>% rss_anon=… rss_file=… threads=… elapsed=…
```

**Buckets.** These are the measured structures:

- `packages`;
- `plan`, the `Rule` copy of every planned target;
- `configured_rules`;
- `configured_nodes`;
- each `action_planning.*` map;
- the per-target toolchain and exec-group contexts and a few other
  `engine.*` maps.

They are walked natively. Sizes come from capacities. `BTreeMap` nodes
are estimated at about 8 entries per 11-slot node, and hash tables from
their capacity.

**Shared data.** Every shared allocation is counted exactly once,
because the whole measurement uses one `allocative::FlameGraphBuilder`
and therefore one "already counted" set.

- An `Arc` pointee counts toward the first bucket that reaches it, in the
  order above. `packages` comes first, so a rule shared with the plan
  counts there.
- Starlark data is measured by its own `Allocative` implementations and
  appears as `shared:<Type>` buckets. This covers every frozen heap
  reachable from the structures (arena chunks plus the Rust allocations
  its values own), depset nodes, `Target` values and struct snapshots.

**Malloc overhead.** `native.malloc_overhead_est` estimates glibc's chunk
rounding and headers for the native walk's allocations. The model is
16-byte granules, an 8-byte header, a 32-byte minimum, and page rounding
above 128 KiB.

**Details and heaps.**

- `PERF-ALLOC-DETAIL` splits the `Rule` data of a bucket into parts: the
  attribute schema, attribute values, declaration, ctx surface, toolchain
  contexts, function handles, the native attribute values
  (`Arc<NativeTarget>`) and the location or call stack. A rule's inline
  1,472 bytes stay in the bucket's plain total.
- `PERF-ALLOC-HEAP` splits the frozen-heap arena bytes only, by the first
  structure that references each heap. The `.bzl` module cache is
  checked first.

**Not covered.**

- Not walked:
  - the package loader's other caches;
  - the repository set (mappings);
  - the configuration registry;
  - registered toolchains and platforms;
  - aspect definitions;
  - the action cache.
- Shallow, counted at inline size only: the `Arc`s of `CtxSurface`,
  `ToolchainContextData`, `ExecGroupsData`, `PlatformInfo` and
  `DeclaredToolchainInfo`.
- Fields that the Starlark derives mark `#[allocative(skip)]`: the
  `DefaultInfo` files and runfiles, and a `Target`'s runfiles when it is
  reached through a heap. The runfiles DAGs are still counted through
  `action_planning.runfiles_info`.
- Memory the allocator keeps after a free.

**Coverage.** `covered=` compares the total with VmRSS. Most of the gap is
anonymous memory: the binary is `rss_file`, about 40 MB.

## Baseline (base binary `6bebe72`, sha256 `b1d76919…`; warm; update mode)

Measured 2026-10-22/23 on the shared 64-core host (load average 10–16) with
`scripts/ab_bench.py`. `~/code` was at HEAD `14bd7e2e`, with lockfile
sha256 `c7a0c3df…`, unchanged during the runs. A is the base binary. B is
the W0-A binary, shown to confirm that the instruments are cost-neutral.
All values are medians over the measured runs.

| | (a) nobuild A | (a) nobuild B | (a) build A | (a) build B | (b) B2 A | (b) B2 B |
|---|---|---|---|---|---|---|
| measured runs (after warm-ups) | 3 (2) | 3 (2) | 2 (1) | 2 (1) | 1 (1) | 1 (1) |
| wall | 47.17 s | 47.24 s | 49.69 s | 50.34 s | 338.2 s | 329.8 s |
| user / sys CPU | 45.40 / 1.58 s | 44.65 / 1.62 s | 48.14 / 43.80 s | 48.84 / 44.53 s | 319.9 / 6.5 s | 310.0 / 6.4 s |
| max RSS (rusage) | 1,415,260 kB | 1,415,020 kB | 1,459,220 kB | 1,456,008 kB | 2,612,476 kB | 2,610,944 kB |
| VmHWM at `phase2_actions_done` | — | 1,421,460 kB | — | 1,421,316 kB | — | 2,629,976 kB |
| `plan.start` | 0.33 s | 0.35 s | 0.33 s | 0.33 s | 0.79 s | 1.31 s |
| `plan.phase1_analysis_done` | 45.97 s | 46.18 s | 45.27 s | 45.89 s | 56.89 s | 61.66 s |
| `plan.phase2_actions_done` | 46.12 s | 46.31 s | 45.42 s | 46.03 s | 335.13 s | 326.66 s |
| `plan.end` | 46.50 s | 46.60 s | 45.79 s | 46.40 s | 336.37 s | 327.92 s |
| `execute.end` | 46.74 s | 46.82 s | 49.26 s | 49.92 s | 337.30 s | 328.90 s |
| stage `prepare` | 16.54 s | 17.23 s | 16.62 s | 16.49 s | 75.83 s | 71.54 s |
| stage `compute` | 17.13 s | 17.02 s | 17.00 s | 17.59 s | 213.11 s | 207.11 s |
| stage `commit` | 4.61 s | 4.87 s | 4.69 s | 4.69 s | 21.86 s | 20.29 s |
| stage `pending_flush` | 38.83 s | 38.63 s | 38.29 s | 38.74 s | 33.15 s | 34.53 s |
| stage `analyze` (nested, report-only) | 181.3 s | 180.4 s | 179.0 s | 180.9 s | 467.3 s | 490.5 s |
| stage `config_conditions` | 1.46 s | 1.51 s | 1.55 s | 1.47 s | 2.09 s | 2.06 s |
| `plan_span` (covered) | 45.75 s (99.9%) | 45.88 s (99.9%) | 45.03 s (99.9%) | 45.65 s (99.8%) | 334.25 s (99.9%) | 325.24 s (99.9%) |

Stage counts on both sides:

- (a) has 18,981 prepares and commits, 707 computes, 1,237 flushes,
  142,298 analyze steps and 18,989 `config_conditions` calls.
- (b) has 24,628 prepares and commits, 2,588 computes, 1,484 flushes,
  199,058 analyze steps and 24,649 `config_conditions` calls.

The base binary prints no `hwm=`, so its peak is the rusage max RSS.

W0-A counters, identical between runs of the same set:

| | flushes | planned in flushes | batch min / median / p90 / max | phase2 batch | view edges | distinct view keys | `Artifact::file` calls |
|---|---|---|---|---|---|---|---|
| (a) | 1,237 | 18,851 | 1 / 2 / 61 / 615 | 130 | 8,669 | 2,181 (25%) | 9,309,489 |
| (b) B2 | 1,484 | 22,345 | 1 / 2 / 61 / 493 | 2,283 | 39,011 | 12,987 (33%) | 35,813,570 |

What the numbers say:

- On (a), phase 2 is almost entirely inside phase-1 flushes (18,851 of
  18,981 targets), in batches of median 2. That supports the PLAN-L1 §4
  verdict against P3 for this workload.
- A view memo keyed by (label, edge configuration) would build 4×
  (a) or 3× (b) fewer views.
- `Artifact::file` runs about 490 times per planned target on (a) and
  1,450 on B2. W1a caches the File in `Artifact`.

## Where the memory goes (end of phase 2)

The W0-A binary was run once more per set with `BAZEL_RS_TIMING_ALLOC=1`
on the warm caches (`--sides b --extra-env-b BAZEL_RS_TIMING_ALLOC=1`).
MiB = 2^20 bytes.

### (a) oauth2token `--nobuild`

The measured total is 798 MiB of 1,388 MiB VmRSS (57.5% covered). The
report took 0.43 s.

| # | bucket | MiB | share of measured |
|---|---|---|---|
| 1 | `packages` | 239.8 | 30.0% |
| 2 | `plan` | 120.6 | 15.1% |
| 3 | `native.malloc_overhead_est` | 71.1 | 8.9% |
| 4 | `action_planning.provider_files` | 60.7 | 7.6% |
| 5 | `shared:FrozenFrozenHeap` | 47.6 | 6.0% |
| 6 | `shared:DepsetNode` | 46.3 | 5.8% |
| 7 | `action_planning.dry` | 40.9 | 5.1% |
| 8 | `configured_nodes` | 32.8 | 4.1% |
| 9 | `action_planning.runfiles_info` | 24.6 | 3.1% |
| 10 | `action_planning.nodes` | 18.7 | 2.3% |
| 11 | `shared:Target` | 18.1 | 2.3% |
| 12 | `action_planning.target_identities` | 16.3 | 2.0% |
| 13 | `configured_rules` | 15.6 | 2.0% |
| 14 | `action_planning.repositories` | 11.3 | 1.4% |
| 15 | `action_planning.source_inputs` | 7.9 | 1.0% |
| | 33 more buckets (`artifact_index` 7.3, `engine.declared_inputs` 4.9, `shared:StructItem` 3.9, …) | 25.9 | 3.2% |

**Counts.** 597 packages and 8,353 loaded rules; 18,981 plan entries;
18,984 configured nodes. `provider_files` has 1,099 targets holding
200,694 File entries. There are 4,116 frozen heaps.

**Rule parts.**

| Bucket | Part | MiB |
|---|---|---|
| `packages` | `native` | 115.7 |
| `packages` | `schema` | 32.3 |
| `packages` | `location` | 8.3 |
| `packages` | `attrs` | 5.7 |
| `packages` | `declaration` | 4.6 |
| `packages` | `functions` | 3.4 |
| `plan` | `native` | 37.9 |
| `plan` | `location` | 13.7 |
| `plan` | `schema` | 10.1 |
| `plan` | `attrs` | 1.6 |
| `plan` | `declaration` | 1.1 |

The rest of `plan` is mostly the inline `(ConfiguredTargetKey, Rule)`
copies: 18,981 × (112 + 1,472) B = 28.7 MiB.

**Frozen-heap arenas.** `custom_providers` has 11.8 MiB in 2,981 heaps,
`bzl_modules` 6.3 MiB in 537 heaps, and `rules` 1.7 MiB in 533 heaps. The
rest of the `shared:FrozenFrozenHeap` bucket is Rust allocations owned by
heap values.

### (b) B2 `--nobuild`

The measured total is 1,788 MiB of 2,568 MiB VmRSS (69.6% covered). The
report took 0.92 s.

| # | bucket | MiB | share of measured |
|---|---|---|---|
| 1 | `action_planning.provider_files` | 466.7 | 26.1% |
| 2 | `packages` | 372.5 | 20.8% |
| 3 | `native.malloc_overhead_est` | 195.7 | 10.9% |
| 4 | `plan` | 172.2 | 9.6% |
| 5 | `shared:FrozenFrozenHeap` | 87.2 | 4.9% |
| 6 | `action_planning.nodes` | 76.2 | 4.3% |
| 7 | `configured_rules` | 60.7 | 3.4% |
| 8 | `shared:DepsetNode` | 59.7 | 3.3% |
| 9 | `action_planning.dry` | 50.4 | 2.8% |
| 10 | `configured_nodes` | 41.9 | 2.3% |
| 11 | `action_planning.runfiles_info` | 31.5 | 1.8% |
| 12 | `shared:Target` | 28.4 | 1.6% |
| 13 | `action_planning.target_identities` | 26.4 | 1.5% |
| 14 | `action_planning.source_inputs` | 22.8 | 1.3% |
| 15 | `action_planning.artifact_index` | 19.3 | 1.1% |
| | 33 more buckets (`repositories` 17.8, `shared:StructItem` 17.4, …) | 76.5 | 4.3% |

**Counts.** 1,876 packages and 11,891 rules; 24,628 plan entries.
`provider_files` has 3,106 targets holding 1,593,229 File entries. There
are 12,660 heaps.

**Rule parts.**

| Bucket | Part | MiB |
|---|---|---|
| `packages` | `native` | 117.5 |
| `packages` | `schema` | 75.0 |
| `plan` | `native` | 44.7 |
| `plan` | `schema` | 38.9 |
| `configured_rules` | `schema` | 38.9 |
| `plan` | `location` | 17.3 |

### What the split means for the waves

- **W1b.** Deleting `provider_files` frees about 61 MiB on (a) and
  467 MiB on B2, plus its share of the malloc overhead. It is the largest
  single bucket on B2: 1.59 M File clones, which is the O(N·T) term PLAN
  §2.6 describes. W1b also removes the per-target `available` maps, but
  those are transient and do not show here.
- **W3 (Rule and schema sharing).**
  - The per-instance attribute schema costs 32 MiB (`packages`) + 10 + 10
    MiB (`plan`, `configured_rules`) on (a), and 75 + 39 + 39 MiB on B2.
  - The plan's `Rule` copies are 121 MiB (a) and 172 MiB (B2). Of that,
    38 to 45 MiB is the deep-copied native attributes from
    `configure()`, and about 29 MiB is inline.
  - The largest part is `packages/part:native` at 116 to 118 MiB: the
    retained attribute values, `select()` parts and input labels of
    native rules. Which rule classes account for it is not split out yet;
    that is the next refinement if W3 targets it.
- **W2, W4 (File and Label interning).** The string-heavy buckets are
  `dry`, `artifact_index`, `target_identities`, `configured_nodes`,
  `source_inputs` and `repositories`, together with
  `native.malloc_overhead_est`: 71 MiB on (a) and 196 MiB on B2, mostly
  small strings.
- **Starlark data.** Frozen heaps, depset nodes and `Target` values come
  to about 112 MiB on (a) and 175 MiB on B2. That is modest, so W5 should
  wait for after-W1 measurements.

### The unattributed part

About 42% of RSS on (a) and 30% on B2 is not covered. It is anonymous
memory: `rss_file` is 40 MB and `threads` is 2. Two experiments on (a)
changed neither RSS nor coverage:

- `MALLOC_ARENA_MAX=1`, so it is not per-thread arenas;
- a fixed 128 KiB `MALLOC_MMAP_THRESHOLD_`, so it is not retained large
  transient buffers.

What is left:

- freed small allocations that glibc keeps (the phase-1 and phase-2
  churn, fragmentation);
- the structures listed as not walked above.

A heap profiler run, or an allocation-counting allocator behind a cargo
feature, would separate the two. The breakdown answers the question "what
does the engine still hold". It does not show what the allocator holds.
