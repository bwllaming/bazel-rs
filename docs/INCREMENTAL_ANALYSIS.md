# Analysis after a snapshot miss: profile, fixes, and incremental analysis

Any BUILD or `.bzl` edit, which is the common development loop, misses the
analysis snapshot (docs/ANALYSIS_SNAPSHOT.md). Before tier A, bazel-rs then
re-analyzed the whole request from scratch. This page covers:

1. Where that time goes: the profile of phase 1 on ~/code.
2. The profile-driven fixes made on branch `perf-analysis`.
3. The design for reusing the previous snapshot's results on a miss, with a
   measured estimate.
4. **Tier A, implemented** (branch `incremental-analysis`): what it does,
   when it falls back, how it is checked, and what it measured.

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

## 3. Incremental analysis on a snapshot miss (design)

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

## 4. Tier A, implemented

Code: `src/engine/incremental.rs` (the dependency tracker),
`src/snapshot/incremental.rs` (the index, attribution, graph check,
splice, merges), `src/snapshot/hit.rs` (the miss keeps what it decoded),
`incremental_build` in `src/main.rs` (the driver). `build` only; `test`,
`run`, `--keep_going`, `--aspects` and coverage keep the full analysis.
`BAZEL_RS_SNAPSHOT_INCREMENTAL=0` turns it off (no index is recorded
either): every miss is a full analysis, exactly as before.

### What a full analysis records: the INDEX section

While a `build` analysis that the snapshot saves runs, the engine records
a dependency index, saved as a sixth section of the snapshot file (INDEX,
postcard compressed like the plan; a hit never reads it):

- **Keys and edges.** Every configured target `analyze_key` reaches, and an
  edge from the target whose frame is innermost — recorded *before* the
  memo check, so every consumer of a configured target has its edge, not
  only the first. Source files are keys without a configuration.
- **Lookups.** Every `Engine::target` lookup is attributed to the innermost
  frame: the configured target being analyzed (`analyze_key`) or planned
  (`plan_one_target`, also a pending flush).
- **Shared computations.** Lookups and keys reached inside a computation
  whose memo is not keyed by the configured target — toolchain and
  execution-platform registration and resolution, platforms, exec and
  Starlark transitions, transition details, package groups, the aspects'
  toolchains, `--aspects` — or outside every frame (option parsing, the
  top-level setup, but the requested labels themselves) make their
  package *global* and the key *shared* (`SharedScope`, a thread-local
  depth; a frame begun inside a shared computation owns its own lookups).
- **Packages.** For every loaded main-repository package a fingerprint:
  a digest of its rules with their load-time attributes (`custom.attrs`,
  the invocation declaration, native attributes, `srcs`/`outs`, visibility,
  location, generator, ...), exports, package arguments and direct loads;
  a *summary* of what the target pattern phase reads (names, classes, tags,
  `manual`, test and deprecation attributes); and whether it defines
  toolchains, platforms, constraints, flags or package groups. Whether its
  BUILD file printed.
- **Prints, aspects, repositories.** The keys whose analysis printed
  (`print()` in an implementation, aspect or transition), the aspect
  applications each key's planning requested, and every path a repository
  rule's or module extension's recorded inputs named (`watch`, `watch_tree`,
  dirents).

Cost on ~/code (release, shared machine): the full analysis 131–141 s
against 130 s for main on `//src/beyondwork.ai/...` (245 s against 239 s on
`//src/...`), peak RSS +110–160 MB (5.73 against 5.62 GB, 8.70 against
8.54 GB), the snapshot +3.5 MB (55.0 against 51.5 MB) and +5.5 MB.

### What a miss does

1. **Classify** (on the lookup's validation thread, while the plan decodes
   and the replay runs; anything else cancels them as before): every
   observation that changed (all of them, `validate::changed_observations`,
   on threads) must be attributable to a loaded main-repository package —
   the BUILD file's READ or a glob listing of it (*loaded*), or another
   observation inside it, the innermost loaded package containing it
   (*content*: a source file's existence, type or link). Not attributable,
   so the full analysis runs: an environment variable, a tree's integrity
   entry or a local repository's replay; a path in the cache or outside the
   main repository; a repository's or extension's recorded input; a `.bzl`
   file; any other READ; a listing of the target patterns or a repository
   (`dirs`, `all`); a BUILD file's existence (a package boundary); a
   package that is global, printed while loading, or defines shared kinds.
2. **Reload** the packages (in this engine, before anything else is
   analyzed). A loaded package whose fingerprint is unchanged and has no
   content change is *pruned*. A changed summary (a target added, removed,
   renamed or retagged), a print, a shared kind, or any digest change under
   a file pattern (`:*`, `:all-targets`) falls back.
3. **Dirty targets**: every key in a changed package or whose analysis
   looked it up, closed over the reverse edges. A shared or printing dirty
   key, or dirty keys that reach no requested target, fall back. The
   requested labels whose root key is dirty are re-analyzed.
4. **No requested label dirty** (a comment, an unused target): the saved
   plan stands; the workspace status is computed as a hit computes it.
5. **Otherwise the partial analysis**: `plan_request` of those labels in
   this engine — its stderr kept apart (a shadow capture), every direct
   repository tree the lookup validated reused without its warm check (the
   *repository verdict carried across the miss*, below) — then:
   - **The graph is unchanged**: every configured target it reached is one
     the saved analysis had, each dirty one with the same configured
     dependencies (source files may come and go: a `srcs` edit), the
     aspect applications are the same, it loaded no package the saved
     analysis did not, and no package only dirty targets looked up is now
     unread. So the plan order, the actions' positions and the window's
     `INFO: Analyzed N targets (P packages loaded, T targets configured,
     A aspect applications)` are the full analysis's.
   - **The splice** (`incremental::splice`): actions are owned by their id
     (`L#n`, `L[cfg]#n`, `L%aspect#n`, a native rule's `L`). Each dirty
     owner's run of actions, contiguous in both plans, replaces the saved
     run in place; every other action the partial analysis planned must
     equal the saved one byte for byte (with its shared input groups);
     the re-analyzed runs' dependencies on other owners' actions must be
     the saved runs'; no other action may consume a dirty owner's; no
     dirty action's output may equal or nest in another's; no other label
     may report a dirty action's output; the re-analyzed labels' reported
     outputs, results and build events must equal the saved ones; no
     warnings, errors, build-info callbacks or run plans. Shared input
     groups map to the saved plan's by position.
   - **The window**: the partial analysis printed nothing the saved window
     does not have (its own `INFO: Analyzed`/`Found` lines aside), and no
     taint.
6. **Save**: INPUTS = what the partial analysis recorded (selected by the
   policy) plus every saved observation, environment digest and tree it
   did not record — but the changed observations, which it either recorded
   again or no longer makes (every target observing inside a changed
   package was re-analyzed) — the replay records and pinned tables merged
   likewise; EVENTS = the saved window (printed now, in place of the
   partial analysis's); PLAN = the spliced plan; INDEX = the saved one with
   the new fingerprints, the re-analyzed keys' lookups and source edges.
   Then it executes like any planned request.

**Fallbacks.** A refusal before the partial analysis, with nothing printed
since the check point, promotes the shadow capture to a tee and the full
analysis runs on in this process. Any later refusal (or a reload that
printed) executes this binary again (`BAZEL_RS_INCREMENTAL_REEXEC=fallback`,
read and removed at startup; stderr silenced until the check point, which
the first process passed): a fresh process runs the full analysis, so
nothing the partial analysis printed, cached (the `.bzl` cache, local
repository executions) or counted leaks into it. EXPLAIN says why
(`incremental <key>: not attempted: ...`, `incremental: the full analysis
runs: ...`, `incremental: falls back to the full analysis: ...`), else
`incremental <key>: N changed observation(s) in P package(s), U unchanged;
D configured target(s) dirty, L requested target(s) re-analyzed`.

**Verify mode** (`BAZEL_RS_SNAPSHOT=verify`): a successful splice is saved
at once and the binary executed again (`BAZEL_RS_INCREMENTAL_REEXEC=verify`):
its lookup hits the spliced snapshot, whose shadow full analysis compares
everything a verified hit compares — the plan's bytes, the window, the
recording's completeness and outcomes, the local repositories — and with
`BAZEL_RS_SNAPSHOT_VERIFY_STRICT=1` a difference exits 70. So the
test-suite and fuzz runs in verify mode check every splice against a full
analysis.

### The repository verdict carried across the miss

The lookup validated every repository tree record of the saved analysis
(root, manifest, directories; every entry under `--verify_repo_tampering`)
and every observation of their recorded inputs, and the classification
refused any change to a recorded input. So the partial analysis reuses
each direct tree of the saved recording without its warm check
(`repository_store::carry_verified_trees`: no recorded-input revalidation,
no stat walk against the manifest); it still records the tree's REPO entry
(its links decide which observations below it a hit validates), and the
merged INPUTS keep the validated observations of its recorded inputs. The
guarantee is a hit's: an in-place edit of a store-tree file is not seen
(the documented per-tree trade-off of docs/ANALYSIS_SNAPSHOT.md). On ~/code
it saves the partial analysis's warm checks (0.33 s of CPU over 95 trees
and 56,504 files for the oauth2token closure). A full analysis after a miss
still runs every warm check: carrying the verdict there would need the
recording seeded with the validated observations of the reused
repositories (a follow-up; on ~/code the checks are 1.26 s + 2.66 s of
mostly prefetch-worker CPU).

**What genuine Bazel 9.3 does with a watched tree on a BUILD-only edit**
(gazelle, warm server, `rule/BUILD.bazel` appended a target): it *re-runs*
`bazel_gazelle_go_repository_tools` (its `external/` link and repository
contents cache entry are replaced) — `rule/BUILD.bazel` is one of the
files it `watch`es (`GO_REPOSITORY_TOOLS_SRCS`) — and does not re-run the
ten `go_deps` repositories that `watch` the gazelle binary it builds: the
binary's digest is unchanged, so their markers are up to date. bazel-rs
does the same now (`PERF-REPO executed ... other=1:
+non_module_deps+bazel_gazelle_go_repository_tools`; an earlier version
re-ran the ten as well). The cost differs: genuine's re-run takes well under
a second (1.05 s for the whole build), bazel-rs's 8.8 s, because the go
build cache (`GOCACHE`) lives in `bazel_gazelle_go_repository_cache`'s
directory. Genuine leaves that directory writable and never looks at it
again; bazel-rs publishes it as a verified store tree, so the cache written
into it by the tools' `go build` makes the next lookup miss
(`INTEGRITY .../trees/<id>: lstat changed`) and the next full analysis
re-execute the cache repository into a fresh, empty tree: every re-run of
the tools compiles gazelle from scratch, and the run after it is a miss
(4.6–4.9 s on gazelle). That is a repository-store matter (trees that rules
use as mutable caches), not incremental analysis's; it is the bulk of the
gazelle BUILD-edit gap (13.4 s against genuine's 1.05 s).

### Measured

Release binaries (main `beec367` = installed, branch `5d1cc4f`), private
caches and lockfile, the real `.bazelrc` with the storagebox overrides,
`build --nobuild`, wall by the harness, peak RSS by bazel-rs's VmHWM line,
one run per cell on the shared machine (load 5–15). ~/code private copy at
~/code `8fdf5779`; edits of `src/beyondwork.ai/tools/oauth2token/BUILD.bazel`
(a comment appended; `x_defs` of `:oauth2token` set or changed) and of
`src/beyondwork.ai/gomock.bzl` (a comment, loaded by 69 packages).

| request | edit | main | tier A | outcome |
|---|---|---|---|---|
| `//src/beyondwork.ai/...` (1,140 targets, 69,061 configured, 22,756 actions) | BUILD comment | 131.7 s, 5.60 GB | **1.8–1.9 s**, 1.6 GB | pruned (reload 43 ms) |
| | BUILD attribute | 131.7 s (any edit) | **5.7–7.1 s**, 2.2 GB | 3 targets re-analyzed (3.9–4.9 s), spliced |
| | `.bzl` comment | 130.9 s, 5.61 GB | 134.9 s, 5.75 GB | not attempted (falls back) |
| | none (hit) | 0.4–0.8 s | 0.76 s | |
| `//src/...` (57,313 actions) | BUILD comment | 253.7 s, 8.57 GB | **4.3–4.8 s**, 5.25 GB | pruned |
| | BUILD attribute | 253.7 s (any edit) | **8.7 s**, 5.25 GB | spliced (4.4 s) |
| | `.bzl` comment | ~250 s | 248.9 s, 8.74 GB | not attempted |
| | none (hit) | — | 2.2 s | |

On `//src/...` a hit decodes a 2.18 GB plan in 1.4 s; an incremental run
pays that, the reload or partial analysis, then encodes and saves the plan
again (1.4–1.9 s on the main thread).

gazelle (bazel-contrib/bazel-gazelle, the benchmark's workspace and flags,
`build //...` minus the exclusions):

| edit | main | tier A | outcome |
|---|---|---|---|
| `rule/BUILD.bazel`: a target appended (bench_genuine.py `build_edit`) | 13.4–13.7 s | 13.4–13.6 s | not attempted: a recorded input of `bazel_gazelle_go_repository_tools`, and a new target |
| `tools/releaser/BUILD.bazel`: a comment | 4.45 s | **0.64–0.69 s** | pruned |
| `tools/releaser/BUILD.bazel`: an `x_defs` attribute | 4.34–4.49 s | **3.0–3.3 s** | 6 targets re-analyzed (2.4–2.7 s), spliced |

Verify mode (strict) on ~/code agreed on every splice tried (the
`x_defs` edits, three times: "the full analysis agrees with the snapshot
(22756 actions, PLAN 597770148 bytes, window stderr 12082 bytes, 22951
observations, 45 local repositories)").

### Tests

- `tests/snapshot.rs::incremental_analysis_splices_prunes_and_falls_back`
  (and its `verify_mode` twin, strict: every splice is checked by a full
  analysis): a comment (pruned), an attribute, a globbed source added and
  removed (spliced, the consumer in another package re-analyzed), then
  fallbacks — a target added (the patterns), a dependency added (the graph),
  a BUILD print, a deprecation (an analysis warning), a `.bzl` edit, a
  package group — a source edit (a hit), a deleted explicit source (the
  build fails like the full analysis's); every step byte-identical with
  the snapshot off, the next run a hit. `//incr:b` reads a generated
  repository's store tree (the carried verdict).
- The mutation fuzz gains an `incr/` package with its consumer and nine
  mutations (an attribute, comments, a dependency, a target, a print, a
  deprecation or the visibility, globbed and explicit sources, the rule's
  `.bzl`) and two commands over them; `BAZEL_RS_SNAPSHOT_FUZZ_COMMANDS`
  restricts the commands like `..._MUTATIONS` the mutations. Run here: the
  default catalogue (120–150 steps, seeds 9101, 7301, 7402, 7503) and the
  incremental subset (`..._COMMANDS=12,13,0,7`, `..._MUTATIONS=0,3,11,46,56,...,64`,
  120 steps, seeds 8101, 8202, 8303, 8404): all steps identical, 1–9
  splices per run, 30 % of the steps in strict verify mode.
- Unit tests: the index round trip, the attribution of changes, action
  owners, the partial window's check. The whole suite under
  `BAZEL_RS_SNAPSHOT=verify BAZEL_RS_SNAPSHOT_VERIFY_STRICT=1` also checks
  every splice its fixtures cause.

### Not handled (fall back), and follow-ups

- **New or removed targets, and dependency changes.** Any change of the
  configured graph falls back: the plan order, the actions' positions and
  the window's counts would have to be computed for the full analysis.
  The index already holds the ordered edges; replaying the phase-1
  post-order over the changed graph, and the target pattern phase over the
  saved package summaries, is the next step (it is the gazelle
  benchmark's edit, besides its repository input).
- **`.bzl` edits.** A changed `.bzl` falls back. A module only BUILD files
  load (macros), outside the load closure of every rule, aspect,
  transition and extension the analysis used, could reload the packages
  that load it and compare fingerprints like a BUILD edit.
- **Repository inputs.** A change to a watched file falls back; re-executing
  the repository and comparing its published tree (Skyframe's change
  pruning of repository values, which genuine gets from its markers) would
  keep the analysis when the tree is unchanged.
- **Tier B** (unchanged design, §3): persist the providers of the
  configured targets with dirty dependants, so the partial analysis
  re-runs the dirty targets only instead of their forward closure (on ~/code
  the 3.9–4.4 s of a one-package edit are mostly the Go toolchain and
  standard library analysis).
- **The save** re-encodes and re-compresses the whole plan (0.4 s on
  beyondwork, 1.4–1.9 s on `//src/...`); reusing the saved PLAN's
  compressed chunks that hold no dirty action would remove most of it.
