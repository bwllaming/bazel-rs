# Memory: the default cap, how to measure, what holds the memory (MEM2)

The goal on large monorepos is a peak RSS of about 6–8 GB for any command; bazel-rs must never use
unbounded memory. Every command is a fresh process (no daemon), so the peak of one command is what matters.

## The memory ceiling and its default

`--memory-limit=<MB>` (build/run/test/query) or `BAZEL_RS_MAX_MEMORY_MB` caps bazel-rs's own RSS
(crate::memory_limit; the flag wins over the environment). A watchdog thread polls `/proc/self/statm`
every 250 ms: above 75% of the cap it warns once; above the cap it kills the live child process groups
and exits with **33**, Bazel's OUT_OF_MEMORY. There is no RLIMIT_AS (it would be inherited by every child
action). The cap bounds bazel-rs itself, not its children. Prefer the environment variable: a
`--memory-limit` in a shared `.bazelrc` also reaches genuine Bazel, which rejects the unknown option.

When neither is set, the cap defaults to **the smaller of 16 GB and 50% of MemTotal**
(`memory_limit::default_cap_for_total_mb`, 1 GB = 1024 MB), announced once per run:

| MemTotal | default cap |
|---|---|
| >= 32 GB | 16 GB (16,384 MB) |
| < 32 GB | 50% of MemTotal |

```
bazel-rs: memory limit defaulted to 16384 MB (the 16 GB default cap; at most 50% of machine memory); set BAZEL_RS_MAX_MEMORY_MB (recommended) or pass --memory-limit=<MB> to change it; the cap bounds bazel-rs itself, not child actions
```

16 GB is the safety net, not the target: the measured peaks below are 2–5× under it. It replaced the
sliding scale of perf follow-up 3 (70% below 32 GB, 50% from 32–128 GB, a flat 64 GB above;
docs/REPOSITORY_WARM_PERF.md), which still let one invocation take 64 GB of the 251 GB development host.
The automatic `--loading_phase_threads` bound (`cap / 64 MB` per worker heap, at least 2) is unchanged
(256 workers at 16 GB, more than the CPUs of any host measured).

## How to measure

- Peak and timeline: `BAZEL_RS_TIMING=1` prints `PERF <t>s rss=<kB> <mark> hwm=<kB>` at the phase marks
  (`plan.phase1_analysis_done`, `plan.phase2_actions_done`, `plan.end`, `execute.end`); with an explicit
  cap the last line is `bazel-rs: peak RSS (VmHWM) N MB (memory limit M MB)`. Measure analysis with
  `--nobuild` and `BAZEL_RS_SNAPSHOT=off` (a snapshot hit skips analysis).
- Retained structures: `BAZEL_RS_TIMING_ALLOC=1` (crate::engine::alloc_report) walks the planning
  structures at the end of phase 2: `PERF-ALLOC <bytes> <bucket>` (packages, plan, configured rules,
  every `action_planning.*` map, shared Starlark data), `PERF-ALLOC-DETAIL` (rule parts),
  `PERF-ALLOC-HEAP` (frozen heaps by referrer) and `PERF-ALLOC-NODES` (planned actions by mnemonic:
  count and bytes split over inputs, dependencies, outputs, arguments, environment, extra, strings,
  execution info and source inputs). `covered=` is the share of RSS the walk attributes.
- Allocation sites of everything live: a jemalloc heap profile. Build a profiling binary in its own
  target directory (no change to the sources):

  ```
  CARGO_PROFILE_RELEASE_STRIP=none CARGO_PROFILE_RELEASE_DEBUG=line-tables-only \
    cargo build --locked --release --features tikv-jemallocator/profiling
  ```

  run it with `_RJEM_MALLOC_CONF=prof:true,prof_final:true,lg_prof_sample:17,prof_prefix:/abs/dir/name`
  (the allocator's symbols are prefixed `_rjem_`, hence the variable name; `prof_final` dumps the heap at
  exit, which for `--nobuild` is the end-of-planning state because the planner's structures are not freed
  before exit) and read the `*.f.heap` file with jemalloc's `jeprof` (`bin/jeprof.in` of the
  tikv-jemalloc-sys crate with `@jemalloc_prefix@` replaced by `_rjem_`), or symbolize the stacks with
  `addr2line -i -f -C` for inline frames. Sampled sizes are scaled by `1/(1-exp(-size/2^lg_prof_sample))`.
  Beware identical-code folding: a monomorphized generic frame may carry another type's name.

## What held the memory (MEM2, ~/code at integration-21 stage 3)

Warm `build --nobuild` with `BAZEL_RS_SNAPSHOT=off`, private caches, `--memory-limit=20000`. A heap profile
of `//src/beyondwork.ai/bworker/wcb/...` (3.6 GB live at the end of planning, 4.06 GB peak) showed:

| what | live | fix |
|---|---|---|
| attribute schemas (`BTreeMap<String, AttrSpec>`) copied per rule instance, per configured-target Rule clone and per generated repository | ~470 MB | `custom_rule::SharedSchema`: one `Arc` per rule class |
| module extension group provenance JSON kept for the rest of the process | ~134 MB | regenerated on demand from a small recipe (lockfile modes), checked against the prepared digest |
| package `.bzl` load graphs copied per package | ~135 MB | one shared copy per equal graph (`BzlCache::shared_load_graph`) |
| a second copy of every planned action's source paths (`ActionPlanning::source_inputs`) | ~137 MB | derived from the planned action when equal (`SourceInputs::planned`) |
| deep copies of a configured target's runfiles in its shared `Target` value | ~110 MB | `Target.runfiles: Arc<TargetRunfiles>`, shared with the views |

Peaks after each step are in the series' commit messages; the summary table is at the end of this file.
