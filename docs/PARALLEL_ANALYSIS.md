# Parallel analysis — P0 baseline and P1 record

Program: oracle design review `ses_eb8f16924ffeNtzHiI8k1sLAME` (2026-10-19),
`/tmp/opencode/parallel-design-review.md`, implemented VERBATIM on branch
`parity-parallel-analysis` (base `feb566f`). P0 and P1 landed; P2/P3 follow
in a later session after the parent reviews P1.

## P0 — baseline (`BAZEL_RS_TIMING` phase marks)

`BAZEL_RS_TIMING=1` prints `PERF <elapsed>s rss=<kB> <mark>` on stderr.
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
