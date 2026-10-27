# Analysis after a snapshot miss: profile, fixes, and an incremental design

Any BUILD or `.bzl` edit, which is the common development loop, misses the
analysis snapshot (docs/ANALYSIS_SNAPSHOT.md). bazel-rs then re-analyzes the
whole request from scratch. This page covers three things:

1. Where that time goes: the profile of phase 1 on ~/code.
2. The profile-driven fixes made on branch `perf-analysis`.
3. The design for reusing the previous snapshot's results on a miss, with a
   measured estimate. The design is not implemented: it is too large for one
   branch.

## 1. Profile of phase 1 (~/code, warm, `--nobuild`)

**Setup.**

- Base: main `d8407d5`.
- Command: `build --nobuild //src/beyondwork.ai/...` with
  `BAZEL_RS_SNAPSHOT=off`, a warm private cache and the update-mode
  lockfile recipe (docs/PERF_LAYER1.md).
- Size: 69,289 planned targets, 15,656 Starlark computes, 23,198 actions.
- Phase 1 took 49–55 s out of a 54–59 s wall.
- Sampler: the ptrace sampler (`/tmp/opencode/perf-research/psample3.py`,
  3 ms interval, `--wall-main`) on a frame-pointer build.

**Finding: one CPU-bound coordinator.** Phase 1 runs on the coordinator
thread. The 16 prefetch workers only load packages.

- At `d8407d5` the coordinator was on-CPU for about 42 s.
- It slept for about 12 s:
  - ~6 s waiting on the `link_canonical` mutex in
    `RepositorySet::link_policy`. Prefetch workers held that mutex while they
    canonicalized every root of a new materialized set.
  - ~3 s in `Prefetcher::take`, waiting for a package a worker was still
    loading.

**On-CPU time**, attributed to the innermost of these frames (after the
fixes; 35.5 s sampled):

| category | s |
|---|---|
| Starlark rule implementations (`analysis::analyze_inner`, including ctx/actions/depset builtins) | 8.8 |
| `validate_structure` of registered actions | 3.4 |
| `source_target` (stat/lstat of source files, link policy) | 2.7 |
| `analyze_key` bookkeeping (edges, select resolution, visibility) | 2.6 |
| `plan_runfiles_support` | 2.3 |
| `Engine::target` (package and rule lookups) | 2.0 |
| `prepare_target` / `commit_action` / `commit_custom_target` / `register_action_outputs` | 6.5 |
| repository group preparation (`repository_demand::evaluate`) | 1.4 |
| attribute transitions, dependency views, native commits, package loads | 4.4 |

The leaf profile was flat. The single largest leaf was `memcmp`, at 17% of
on-CPU time, from ordered-map searches whose keys are labels (three
strings) and files (three strings). Next came:

- path hashing: std's `Path` hash plus SipHash;
- `statx`;
- label validation in `Label::parse_canonical`, which runs once per artifact
  in `planted()`.

The critical path of rule implementations is short: 0.6 s over 23 targets
(`PERF-STAGE critical-path`). Parallel analysis would therefore be the big
structural win. That is a much larger change than this branch, and is not
part of this design.

## 2. Profile-driven fixes (branch `perf-analysis`)

Each fix keeps the `BAZEL_RS_DUMP_ACTIONS` dump byte-identical.

| change | what it removes |
|---|---|
| `link_policy` no longer holds the canonical-path memo across `canonicalize` (`RepositorySet::link_canonical_all`): the memo is locked once to look up and once to insert. The memo is keyed by bytes in an FxHash map instead of a `BTreeMap<PathBuf>` compared component by component. Of two concurrently computed root lists, the larger is kept, so there is no ping-pong. | ~6 s of coordinator sleep. A first version that locked per root made prefetch slower: `wait` went from 2.5 s to 12.6 s. Hence the batched lookups. |
| `crate::path_key::PathKey`: source-input deduplication (`source_paths`, `SourceInputs`) hashes a path's bytes when they are already in normal form. This keeps `Path` equality. Main's `crate::fast_hash` independently did the same for `namespace_may_overlap`. | ~2 s of `Path` hashing through SipHash |
| `ConfiguredNodes` is an FxHash map instead of a `BTreeMap`. It is only looked up, never iterated into output. | ~2 s of label comparisons over ~663k lookups |
| `Depset::flatten` and `dedup` keep plain items (files, strings, labels, ints, bools, `None`) in a hashed `ItemSet`. Other kinds stay ordered, because their `Ord` is the only equality they have. | ~2 s of `FileData` comparisons in `to_list()` |

### Before/after

**Recipe.**

- Binaries:
  - main `25b4cb6` (ELF `ed22aa0c…`, the installed release);
  - this branch.
- Runs: warm `--nobuild` with `BAZEL_RS_SNAPSHOT=off`, private cache,
  `--memory-limit=20000`, `nice -n 10`.
- Machine: shared, with load averages of 15–47, so wall times are
  indicative. `main CPU` is the coordinator thread's utime+stime.
- Timings are medians of 2–3 interleaved runs (`//src/...`: 1 run plus the
  dump run).

| target | main phase 1 / end / main CPU | branch phase 1 / end / main CPU | peak RSS |
|---|---|---|---|
| `//src/beyondwork.ai/tools/oauth2token/...` | 3.7–4.6 / 3.8–4.7 / 3.7–4.0 s | 3.9–4.0 / 4.0 / 3.4–3.6 s | 0.8 GB both |
| `//src/python:bworker_env` | 4.8–5.4 / 5.8–6.2 / 5.5 s | 4.4–5.2 / 5.3–6.3 / 5.0–5.7 s | 1.0 GB both |
| `//src/beyondwork.ai/...` | 42.7–50.1 / 46.2–54.4 / 40.5–45.9 s | 36.0–37.0 / 39.1–40.4 / 38.4–39.6 s | 3.8 GB both |
| `//src/...` | 94.0 / 124.5 / 100.5 s | 74.4 / 104.7 / 98.8 s | 6.7 GB both |

`//src/...` dump runs gave phase 1 of 95.6 s (main) and 69.1 s (branch). Both
dumps hold the same 60,237 actions, byte-identical. For `//src/beyondwork.ai/...`
the dumps are identical too (23,198 actions).

**What the numbers show.**

- The wall-time gain on large requests is mostly the coordinator no longer
  waiting: the prefetch `wait` halved and the link-policy sleep is gone.
- The CPU gain is 2–5 s.
- Small requests are unchanged within noise.

**One BUILD-file edit.** The scenario runs in a private copy of ~/code:

- copy: `/home/coder/opencodetmp/w/perf-analysis-code`;
- snapshot on;
- edit: append a comment line to `src/beyondwork.ai/tools/oauth2token/BUILD.bazel`;
- request: `--nobuild //src/beyondwork.ai/...`.

Results:

| | main | branch |
|---|---|---|
| snapshot hit (no edit) | — | 0.59 s |
| after the edit (miss, full re-analysis), 3 runs | 41.5 / 40.3 / 40.0 s phase 1; 45.4 / 44.1 / 44.0 s end | 42.3 / 38.5 / 39.0 s phase 1; 45.9 / 42.1 / 42.8 s end |

While the snapshot records, the gain is smaller than with the snapshot off
(36 s). Recording roughly offsets it: the sequential-roots bookkeeping and the
observation hooks cost about 3 s.

The full re-analysis is what an incremental scheme removes.

## 3. Incremental analysis on a snapshot miss (design, not implemented)

### What a miss throws away today

Validating the snapshot (`PERF-SNAPSHOT miss attempt_ms≈330`) already finds
the changed inputs, for example `READ …/oauth2token/BUILD.bazel: content
changed`. Then everything is discarded and the planner starts from nothing.

Yet for the edit above, the request's 1,379 targets include only 3 that
depend on the edited package:

```
query 'rdeps(//src/beyondwork.ai/..., //src/beyondwork.ai/tools/oauth2token:all)'
```

That query returns oauth2token's own three targets. The other 69,286
configured targets and their 23,000+ actions are unchanged.

### Scheme (Skyframe-style change pruning at package granularity)

1. **Record per-package input sets.** Today the snapshot records inputs
   (docs/ANALYSIS_SNAPSHOT_RECORDING.md) for the whole plan. Attribute each
   recorded observation to the package whose load or analysis made it. That
   means the BUILD file, the `.bzl` closure, glob listings, and the source
   stats done by `source_target`. Global inputs (flags, module graph,
   repository roots, toolchain registrations) stay plan-wide; a change there
   is still a full miss. Configured targets record their package, their
   configured dependency keys and their action range in the plan.

2. **On a miss, classify instead of discard.** Validation yields the changed
   observations. If none of them is plan-wide, the dirty set is:
   - the packages owning them;
   - closed over reverse dependencies: configured targets whose dependency
     keys reach a dirty target.

   The plan already holds `rule_edges`, the dependency graph rebuilt for
   `PERF-STAGE critical-path`, so the reverse closure is cheap. The edited
   oauth2token BUILD file gives 3 dirty targets.

3. **Re-analyze the dirty set only.** A dirty target's implementation needs
   its dependencies' providers. The snapshot keeps actions, not Starlark
   provider values. There are two tiers:
   - **Tier A** (no new persistence): re-analyze the forward closure of the
     dirty targets in a fresh engine. That is what
     `--nobuild //src/beyondwork.ai/tools/oauth2token/...` does today:
     3.7–4.0 s of phase 1, mostly Go toolchain and stdlib analysis. Then splice
     the dirty targets' actions into the decoded plan in place of the old
     range, and keep everything else as decoded. Action identifiers are
     owner-based (`L#n`), so the splice is by owner.
   - **Tier B**: persist the provider values of configured targets that have
     dirty dependants. These are frozen Starlark values, encoded like the
     depset `StructItem` snapshots (docs/DEPSETS.md). Re-analysis then touches
     only the dirty targets.

   Change pruning: if a re-analyzed target's providers and actions are
   byte-equal to the snapshot's (a comment-only edit), its reverse
   dependencies are not re-run.

4. **Save as a delta.** Write the merged plan, today a 0.6 s save for this
   request. Or, better, append a delta part with a pointer to the base
   snapshot, so the next hit decodes base + delta.

### Measured estimate (~/code, `//src/beyondwork.ai/...`, one BUILD edit)

| step | measured basis | cost |
|---|---|---|
| validate + classify | miss `attempt_ms` 320–340 | 0.35 s |
| decode the previous plan | hit `decode_ms` 391 | 0.4 s |
| replay/local checks | hit `replay_ms` + `local_ms` | 0.15 s |
| re-analyze (Tier A: forward closure of 3 targets) | phase 1 of `--nobuild //src/beyondwork.ai/tools/oauth2token/...` | 3.7–4.0 s |
| re-analyze (Tier B: the 3 targets only) | ~0.8 ms/target average (`PERF-STAGE`), go_library ~25 ms | < 0.1 s |
| splice + save | plan.end → execute.end of a miss run | 0.6 s |
| **total** | | **Tier A ≈ 5.3 s, Tier B ≈ 1.6 s** (today: 42–46 s) |

An edit in a widely used package (the root BUILD, a `.bzl` loaded
everywhere, a toolchain package) dirties most of the plan, and is no faster
than today's full re-analysis.

Tier A's forward closure is dominated by toolchain and stdlib analysis. A
per-configuration cache of the registered-toolchain analysis would cut it
further:

- `compute_registered_toolchains` was ~13 s inclusive of all of phase 1 on
  `//src/beyondwork.ai/...`;
- it is ~2–3 s of the oauth2token closure.

### Repository re-validation on a miss (the gazelle data point)

On bazel-gazelle (docs/BENCHMARKS.md), a `rule/BUILD.bazel` edit costs bazel-rs
13.8 s against 1.05 s for genuine Bazel's warm server. About 9 s of the 12.6 s
of planning is repository re-validation after the miss:

- `PERF-REPO recorded_check=9.005s/71`;
- `execute=8.397s/55`;
- 139 MB of recorded inputs re-hashed.

On ~/code the same miss shows
`PERF-REPO recorded_check=1.26s snapshot_check=2.66s` over 5,260 reused
repositories (337k files stat-walked), with 45 cheap re-executions (0.06 s).
Those checks run mostly on prefetch workers.

Two parts of the design address this:

1. **Carry repository verdicts across the miss.** The snapshot's validation
   already checked every recorded input of every repository the plan used,
   and found which changed. A repository none of whose recorded inputs
   changed is verified by that validation. Its warm-reuse probe (recorded-input
   digests, manifest, tree stat walk) can be skipped. That is a per-repository
   flag handed from `snapshot::validate` to `RepositorySet` (a
   `DemandSlot::get_or_probe` fast path). On gazelle this removes the 71
   `recorded_check`s for every repository not watching the edited file.

2. **Watched trees and change pruning.** gazelle's `go.work`-driven
   `go_repository` rules watch the workspace tree, so a BUILD edit under it
   legitimately invalidates them. Today they re-execute (8.4 s for 55), and
   everything they feed is re-analyzed. Two steps follow:
   - compare the re-executed repository's published tree digest
     (content-addressed store) with the previous one;
   - when equal, which is the normal case for an edit that does not touch the
     files the rule reads, mark the repository unchanged, so packages and
     targets in it are not dirty. That is Skyframe's change pruning applied
     to repository values.

   Before relying on this, check what genuine Bazel 9.3 re-fetches for a
   watched-tree change (`RepoRecordedInput.DirTree`): its 1.05 s suggests
   the warm server either does not re-run these rules for a BUILD-only
   change, or prunes immediately.

### Risks and invariants

- **Determinism.** Spliced output must be byte-identical to a full
  re-analysis. Verify mode (docs/ANALYSIS_SNAPSHOT.md, "Verify mode") would
  compare the incremental result against a full plan on every Nth run, as it
  does for hits today.
- **Printed output.** Planning prints (BUILD/`.bzl` `print`, warnings) are
  replayed from the snapshot for clean packages and re-emitted for dirty ones,
  in plan order. The recorder already stores what planning printed.
- **Memory.** The decoded plan plus a partial engine coexist. Tier A on
  `//src/...` would add the closure's engine (~1 GB for an oauth2token-sized
  closure) to the decoded 5 GB plan, within the 6–8 GB goal.
