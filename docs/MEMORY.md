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

## TypeScript analysis (perf-ts, ~/code `//src/ts/...`)

aspect_rules_js generates one repository per npm package (~4,300 on ~/code), and an extension's
repositories share one mapping with an entry per sibling. Per-target work proportional to the
repository count dominated warm `build --nobuild //src/ts/...` (snapshot off, private caches):

| what | cost before | fix |
|---|---|---|
| the repo mapping manifest of every js_binary/js_test runfiles tree cloned (and dropped) the mapping of each transitive repository | ~61 s, the ~8.5 GB analysis spike | `runfiles_tree::MappingLookup` borrows the mappings |
| `validate_structure` scanned all inputs per declared tree/directory/symlink input | ~30 s | one `HashSet` lookup each; `namespace_may_overlap` hashes ancestors instead of sorting |
| `rule_surface` copied the rule's repository mapping (~0.8 MB for an npm package target) | ~2.3 s | `RepositorySet::shared_mapping`: one `Arc` per mapping table entry |
| `link_policy` re-canonicalized every materialized root per linked source; resolution re-ran realpath on each | ~4 s | the last canonical root list is shared (`Arc`), resolution trusts it |
| `ctx.expand_location` copied the rule's location map per call | ~2.6 s | read in place with the call's `targets` |

Result: `--nobuild //src/ts/...` 132 s / 8.2 GB peak -> 26 s / 4.6 GB; warm `build //src/ts/...`
(24,608 cache hits) 139 s / 9.1 GB -> 36 s / 4.6 GB. Action dump and action keys unchanged.

## Rust analysis (MEM3, ~/code `//src/rust/...`)

rules_rust expands depsets of provider instances (`CrateInfo`, `DepVariantInfo`, ... in `transitive_crates`
and friends) with `to_list()` in every rustc/clippy action of every crate. A depset element is an owned
snapshot (docs/DEPSETS.md, N49), and each expansion allocated the instance anew; each of its frozen
list/dict fields went to a private frozen heap of its own (N49b) that the caller's heap — and so the
target's frozen analysis heap — kept for the rest of the build. A heap profile of `build --nobuild
//src/rust/...` (8,692 targets, 2,001 actions) showed 8.2 GB of those copies (9.8 million frozen heaps)
out of 9.4 GB live at the end of planning.

| what | cost before | fix |
|---|---|---|
| a struct/provider depset element re-allocated by every `to_list()`, its list/dict fields on one private frozen heap each | ~8.2 GB, 9.8 M frozen heaps | `StructItem::frozen`: the element is allocated once, frozen on one heap with its containers, and shared by every expansion |

Result (`BAZEL_RS_SNAPSHOT=off`, private caches): `--nobuild //src/rust/...` 10.1 GB peak, plan 25 s ->
1.86 GB, plan 15 s; warm `build //src/rust/...` (2,001 cache hits) 10.1 GB / 31 s -> 1.86 GB / 18 s.
Action dump byte-identical and action keys unchanged (the warm build hits every action).

## The whole repository (MEM4, ~/code `//src/...`)

`build --nobuild //src/...` (10,500 packages, 121,000 configured targets, 96,000 planned actions)
peaked at 17.3 GB with the snapshot off and 20.7 GB on a first run with it on (default cap: 16 GB).
Nothing grew super-linearly: the per-tree peaks add up to more than the whole. What dominated was
copies — the same strings held once per place that refers to them:

| what | cost before | fix |
|---|---|---|
| a `File` (three Strings) copied into every depset, dependency view, `ctx.files` struct, target's files and runfiles | ~4.5 GB of String copies | `File` is `Arc<FileData>` |
| an `Artifact` (two PathBufs, owner, producer id) copied into `dry`, the declared map, the index, views and every runfiles mapping | ~2 GB | `Artifact` is `Arc<ArtifactData>` |
| every planned action's inputs and generated inputs as owned `(PathBuf, PathBuf)` pairs: the transitive inputs of Rustc, Clippy, PythonZipper, and each executable's runfiles tree and manifest | ~1.8 GB | `shared_path::SharedPath` (an `Arc<Path>` that compares, hashes and borrows as its `Path`) for `Action::inputs`, `Dependency::destination` and the artifacts' paths |
| each executable's runfiles mapping three times (manifest transform, tree transform, the target's entries) | ~0.8 GB | one shared `RunfilesEntries` list, shared path strings |
| the hidden top-level artifacts (every runfile of every requested executable) each with a copy of the target's key | ~0.6 GB spike at the end of planning | keys by index (`KeyTable`) |
| a configured rule copied into `configured_rules`; native targets deep-copied without select()s; call stacks per copy | ~0.6 GB | `Arc<Rule>` in the plan, the declared native target shared, `Arc<[..]>` call stacks |
| a depset `Item` sized by its largest variants (a label value, 72 bytes) | ~0.3 GB | label and symlink entries boxed: 32-byte items |
| the snapshot save: the 2.1 GB plan encoded, copied into one buffer and compressed while the analysis is in memory | +3.8 GB | parts compressed as they are encoded (`codec::encode_parts_with`, `format::compress_part`) |

Result (release builds, warm private cache, `--memory-limit=24000`):

| run | master 4b8e690 | MEM4 |
|---|---|---|
| `--nobuild //src/...`, snapshot off | 17.3 GB, 188 s | 6.8 GB, 168 s |
| `--nobuild //src/...`, snapshot on, first run (miss, save) | 20.7 GB, 146 s | 8.0 GB, 127 s |
| `--nobuild //src/...`, snapshot hit | — | 5.1 GB, 2 s |

The action dump (`BAZEL_RS_DUMP_ACTIONS=1`, 59,788 actions) is byte-identical.

## Snapshot hits (MEM5, ~/code)

A snapshot hit only decodes the planned request and runs the execution tail, but it held far more than the
decoded plan. On `--config=linux_arm64 //src/rust/...` (rules_rust cross-compiling through the zig cc
toolchain) a hit peaked at 2.6 GB, against 0.9 GB for amd64. A heap profile at the peak showed what was held:

| what | cost before | fix |
|---|---|---|
| every Rustc/Clippy action lists the toolchain's ~4,000 files in full: a 1.28 GB raw plan (arm64), decoded one `SharedPath` allocation per listing (1.2 GB) | ~2.5 GB at the arm64 peak | a path table per GROUPS/NODES section (format 8, docs/PLANNED_REQUEST.md): each distinct path once, every listing an index, decoded as one shared `SharedPath`. The raw plan is 55 MB and the stored snapshot 23 MB instead of 72 MB |
| the whole PLAN section inflated before decoding (768 MB for `//src/...`) and held beside the decoded actions | ~0.7 GB | `codec::decode_stored`: each section is inflated on the thread that decodes it, then dropped |
| an executable's runfiles manifest and tree encode the same entry list, and every name was decoded as its own `String` and then `Arc<str>` | ~0.4 GB (`//src/...`) | a list is written once per section, by content; names are path-table indices; decoded listings share the list and the names |
| stamped tree files' paths built with `join` (the copy grows by doubling); the tree memo of the TypeScript hit holds ~10^5 of them | ~0.08 GB (ts) | exact-size paths |

Result (release builds, warm private cache, real `.bazelrc`, `--memory-limit=20000`). Each hit was the third run
of its binary, and the two binaries ran back to back on the same ~/code state:

| hit | main 59b528b | MEM5 |
|---|---|---|
| `build //src/rust/...` | 889 MB, 0.9 s | 282 MB, 0.7 s |
| `build --config=linux_arm64 //src/rust/...` | 2,594 MB, 2.2 s | 470 MB, 1.5 s |
| `build //src/ts/...` | 1,058 MB, 1.6 s | 880 MB, 1.5 s |
| `build --nobuild //src/...` | 5,215 MB, 2.1 s | 1,202 MB, 1.0 s |

Each hit's stdout and stderr, apart from the PERF lines and timings, are identical to the base binary's. Every
action is still a cache hit (keys unchanged), and `BAZEL_RS_SNAPSHOT=verify` reports `verified` for arm64 rust,
ts and `//src/...`.

What remains:

- **`//src/...`.** The decoded plan itself: the actions' input vectors, each chunk's own path table, the
  dependencies and the path sets.
- **ts.** The execution's tree memo (the npm package trees, ~200 MB live) and the allocator.
- **The allocator.** jemalloc's default of 4 arenas per CPU (256 on the 64-CPU host) keeps memory freed by one
  thread out of the other threads' reach. With `_RJEM_MALLOC_CONF=narenas:4` the ts hit peaks at 776 MB instead of
  920 MB, and with `dirty_decay_ms:0` at 697 MB.
  - This is not changed here. It is process-wide, and fewer arenas may slow the parallel analysis.

## OpenROAD (openroad-perf, docs/OSS_CORPUS.md P6)

`build --nobuild //...` of OpenROAD (6,773 targets, 65,387 configured rule targets, 18,561 C++ compiles) peaked
at 9.2 GB (genuine 9.3.1: 3.0 GB). Heap profiles at every `PERF` mark (`BAZEL_RS_TIMING_HEAP_DUMP`) placed the
peak at the end of pass 2, when every target's delta waits for the cursor (the analysis critical path, boost's
cc_libraries, ends ~90% into the plan):

| what | at the peak | fix |
|---|---|---|
| each compile lists its compilation context's transitive headers (3,000–11,000; 54 M input pairs) | ~2.0 GB | `create_cc_compile_action` walks the headers (or the toolchain's `all_files`) through the memoized depset walk, so the registration carries it as a shared run (`input_memo`); the lowering holds it as one shared input group at its place among the action's own inputs (`PlannedAction::groups_at`, docs/PLANNED_REQUEST.md format 10) when listing it changes nothing but the list (none of the action's own execution paths is one of the run's; the listed action validates against the run's namespace) |
| the provisional reads of a memo value copied into every worker delta using it | ~0.8 GB | `TargetDelta::shared_reads`: the memo value's list, shared |
| a shared group's source paths as owned `PathBuf`s | ~0.5 GB | `InputGroup::sources` share the artifacts' `SharedPath`s |
| the linearizer's tape buffer kept the capacity of its largest step (recording runs only) | ~0.4 GB | shrunk to 64 Ki ops after a step |
| transcripts kept with their growth's spare room | ~0.2 GB | shrunk when published |

Result (non-LTO builds of b8ce9666 and the branch, alternating, three rounds, load 12–42; private caches; the
cold run fetches everything):

| run | b8ce9666 | openroad-perf |
|---|---|---|
| cold `--nobuild //...` | 125.9 / 111.0 / 110.8 s, 9,169 / 9,167 / 9,126 MB | 87.2 / 93.7 / 96.4 s, 6,961 / 6,957 / 6,983 MB |
| first repeat (re-analysis, recording) | 85.8 / 60.7 / 87.0 s, 9,130 / 9,116 / 9,141 MB | 66.4 / 47.1 / 48.9 s, 6,947 / 6,955 / 6,950 MB |
| snapshot miss (`--define` changed) | 57.0 / 57.1 / 56.0 s, 9,121 / 9,097 / 9,159 MB | 30.5 / 34.0 / 31.0 s, 6,934 / 6,944 / 6,952 MB |
| snapshot hit | 1.7 / 1.6 / 1.6 s, 3,333 / 3,309 / 3,328 MB | 1.3 / 1.2 s, 1,674 / 1,675 MB |
| `--nobuild`, snapshot off (heap profile's run) | 8.33 GB peak | 6.36 GB peak |

The action dump of `//src/utl:utl //src/sta:opensta_lib` (7,843 actions) is byte-identical, and the
`//:openroad` build's 15,223 `bazel-bin` files match genuine's. What holds the remaining ~6 GB live at the peak: the
analysis results awaiting their merge (Starlark heaps, providers, the registered actions' input lists, ~1.6 GB),
the deltas (~0.9 GB), the phase-1 configured rules and packages (~1.5 GB), the remaining listed inputs (~0.4 GB)
and, while recording, the transcripts (~1.1 GB at the end of phase 1). The next step is the cursor's lag: deltas
merged as their entries' turns come would not all be alive at once.

## Snapshot misses (analysis-memory, ~/code and OpenROAD)

Heap profiles at every `PERF` mark (`BAZEL_RS_TIMING_HEAP_DUMP`) and `BAZEL_RS_TIMING_ALLOC` of `build --nobuild`
snapshot misses (`--define` changed, recording on). Both peaks are at the end of the deferred walk, after the
cursor merged everything, not while results wait for it: OpenROAD's cursor waits ~10 s for the boost chain, but
what the waiting entries hold (prepares, unmerged deltas: ~1 GB live mid-wait) is gone again at the walk's end,
where the merged plan, the walk's tables and phase 1's structures peak together.

| what (~/code `//src/...`) | at the peak | fix |
|---|---|---|
| each of 31,114 PyCompile actions listed the Python runtime's ~1,650 files (rules_python `actions_run`: the interpreter as executable, `inputs = depset([src], transitive = [runtime files])`, the precompiler as a tool); the shared run could not be a group beside the precompiler's runfiles tree, and the run also held the interpreter listed first | 1.65 GB (mostly pruned as unreachable in the tail) | the run is one group beside the tree (planned request format 11: `groups_at` places shared runs only, runfiles trees stay after all own inputs), without the entries the action lists first (`SharedInputs::group_beside`) |

Result (non-LTO builds of main 0d2e0367 and the branch, alternating, three rounds, load 8–23, private caches):

| run | main | analysis-memory |
|---|---|---|
| ~/code `--nobuild //src/...`, snapshot miss | 22.0 / 23.5 / 23.3 s, 9,596 / 9,677 / 9,591 MB | 22.4 / 23.9 / 21.4 s, 7,915 / 7,954 / 7,927 MB |
| OpenROAD `--nobuild //...`, snapshot miss | 30.2 / 30.9 / 32.6 s, 6,895 / 6,909 / 6,911 MB | 28.6 / 29.8 / 28.3 s, 6,908 / 6,901 / 6,904 MB |

The action dump of ~/code `//src/...` (5.5 M lines) and stdout are byte-identical; `BAZEL_RS_RUN_GROUPS=0` (lists
every run in full) gives the same dump and the same aquery output (text and jsonproto, PyCompile included).

What remains on OpenROAD (live at the walk's end ~6.5 GB, 7.0 GB resident; genuine 3.0 GB), none of it a single
large item: the planned actions (1.2 GB: CppCompile arguments 250 MB as owned strings, 18,561 x ~200;
GenerateMoc's inputs 122 MB, its memoized run only the first of a dozen header depsets; the test runfiles
trees' and manifests' 30,839 nodes 190 MB), the targets' frozen analysis heaps (0.56 GB in 343,000 heaps),
packages and configured rules (0.9 GB), the walk's own tables freed at its end (published results, shared-input
memos, provisional reads, observations: ~0.4 GB) and phase 1's evaluator tables freed after it (~0.2 GB), and the
snapshot recording (+0.45 GB live: the incremental tracker's tapes and the observations; +0.8 GB resident
against `BAZEL_RS_SNAPSHOT=off`: 6.28 GB). jemalloc's arena count and decay times change nothing (narenas 16/64,
dirty decay 1 s, background thread: 6.87–7.05 GB).

## Test execution (miss-and-test-mem, ~/code)

`test` of a 129-target ~/code sample (Go, Python, JS/vitest tests; built beforehand, `--nocache_test_results`,
`--jobs=32`, derived snapshot hit: 1.8 GB at `execute.start`) grew bazel-rs to 7.8 GB within ~70 s and stayed
there while the tests ran. Heap profiles (`lg_prof_interval`) put ~5.4 GB of the 6.9 GB live heap in
`test_runner::Runner::run_unit`: each running test held its whole runfiles list several times over — the
expanded tree files (`ContentSource::Tree`: rules_js `node_modules` packages, hundreds of thousands of files per
test) as `(ContentInput, destination)` inputs, their layouts' exec paths, a cloned `staging` list, the `links`,
and inside `stage_test_inputs` a cloned `staged` map and a hex digest per entry — and kept the inputs, layouts
and links alive for the whole test run.

| what | at the peak | fix |
|---|---|---|
| a running test's runfiles inputs, layouts, staging copy and links, live until the test ends | ~5 GB at 32 jobs | the inputs and layouts are moved into the staging list (the remote view, which borrows them, keeps a copy only when a remote executor or spawn cache is active), `stage_test_inputs` consumes it (a first pass checks one-artifact-per-path with 32-byte digests, then the entries move into the staged/live maps), the links are dropped after staging |

Result (non-LTO, alternating, three rounds, load 31–42; bazel-rs's own peak VmRSS, 0.5 s polls):

| run | main 92711fc8 | branch |
|---|---|---|
| 129 tests, `--nocache_test_results`, snapshot hit | 7.76 / 7.78 / 7.81 GB, 238 / 241 / 245 s | 3.71 / 3.74 GB (first run a miss: 5.81 GB), 248 / 236 / 235 s |

Test cache keys are unchanged (the same `tests/<key>` entries); the staged set is the same (the first input at a
path is kept, a second identical one skipped, a different one fails as before). What remains at the peak
(~2.1 GB live): the tests mid-staging (their expanded tree entries, ~0.9 GB), the executed actions' cache
validation (`cache_valid`, tree stamps, ~0.5 GB) and the decoded plan.
