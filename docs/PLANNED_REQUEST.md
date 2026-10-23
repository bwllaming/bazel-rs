# The plan/execute split: `PlannedRequest` (L2-W4)

Design: PLAN-L2 §1d/§1e/§6 and update 1 §(c)4 (the analysis-snapshot program). Code:
`src/request.rs` (the request, `finalize`, `execute`), `src/snapshot/codec.rs` (the binary encoding and the
full-analysis hand-over), `Engine::plan_request` (src/engine.rs), the `build`/`test`/`run` flows of src/main.rs.

## The pipeline

Every `build`, `test` and `run` now runs the same steps after target-pattern selection:

```text
full analysis:  plan_request -> PlannedRequest -> hand_over -> finalize -> execute -> report
snapshot hit:   decode ---------------------------------------> finalize -> execute -> report   (L2-W5)
```

* `Engine::plan_request(RequestSpec)` runs analysis and action planning (`plan_build`) and returns a
  `crate::request::PlannedRequest`: **plain owned data** — no Starlark values, no closures (the one
  exception, an Args `map_each`, is below). It marks `plan.end` (which closes the record mode's window) when
  planning succeeded.
* `crate::snapshot::codec::hand_over` is the full-analysis path's hand-over to execution: it does nothing
  unless a switch asks for the encoding (below).
* `PlannedRequest::finalize` completes the request for this run (on a hit as on a miss), then
  `crate::request::execute` writes the action dumps, runs the scheduler and computes the build summary and the
  run targets. The analysis warnings and errors travel in the request and are printed after execution, as
  before.
* After the hand-over the CLI frees the engine (see *Freeing the engine*): execution and reporting read only the
  request.

`Engine::build`/`build_with_jobs`/`build_with_options` (the library path, used by the engine's tests) run the
same pipeline and keep the analysis diagnostics in the engine (`take_warnings`/`take_errors`).
`Engine::build_for_run`, `build_for_tests` and `build_requests` are gone: `run` and `test` plan through
`plan_request` with their own `RequestSpec`.

## What a request holds (PLAN-L2 §1d)

| Field | Content |
|---|---|
| `command` | `Build`; `Test { build, tests }` (the targets built, and every test to run after the test filters with its `TestRuleInfo` and location); `Run { label, test }` (the target's test properties, looked up before planning while the target is loaded, for `run` of a test; an error surfaces where `run_test` read it before) |
| `execute` | false under `--nobuild` |
| `nodes` | the reachable planned actions in plan order (the `BAZEL_RS_DUMP_ACTIONS` order), with the workspace status nodes as placeholders at their position |
| `requested`, `aspect_reports`, `non_empty_targets` | the reported artifacts as `Produced { real, producer }` (where `--nobuild` reports them / which action output publishes them) |
| `run_plans` | per run/test target with an executable: the executable (exec path, short path), the built runfiles tree's MANIFEST, the runfiles mapping, the run settings, and an index into `configurations` |
| `configurations` | per configuration a run plan uses: the output directory mnemonic, the action environment and the `--test_env` environment (what `build_requests` read from the configuration table) |
| `warnings`, `errors` | the analysis diagnostics, printed after execution |
| `build_info` | the planned actions whose planning ran a build-info translation callback |

Paths and byte strings stay exactly what they were in memory (lone bytes included, crate::bazel_bytes).

## `finalize`

* **Workspace status.** Planning still computes the status where it did (the status command's stderr is the
  build's stderr at that point, and build-info callbacks read it), but the two status nodes it plans are
  placeholders, `Transform::WorkspaceStatus { volatile }`, always in the success layout (`#stable`, then
  `#volatile`; the stable file's consumers depend on `#stable`). `finalize` takes the status from the source
  it is given — `StatusSource::Planned` (the miss path: `Engine::take_workspace_status`) or
  `StatusSource::Compute` (a decoded request: run `--workspace_status_command` now) — and writes it into the
  placeholders. A failed status becomes ONE failing `#volatile` node where `#stable` was, and every dependency
  and reported producer of `#stable` is redirected to it: exactly the layout planning produced before the
  split (`Failed to determine workspace status: ...` once). A placeholder that reaches execution is an
  internal error (it is keyed apart and never executes).
* **Environment.** Every inherited action-environment name is resolved against the client environment of this
  run (ActionEnvironment.resolve); actions without inherited names get no client values. Planning still
  resolves them too (the in-memory request is complete), but the encoding never holds the values.
* **Embedded inputs** (`genrule-setup.sh`) are encoded as the tag of the compiled-in blob and decoded back to
  the static bytes.

## Deferred command lines are data

`scheduler::Deferred` is `Deferred(Arc<DeferredSpec>)` with `DeferredSpec::CommandLine { spec, progress,
extra_param_files }` (a Starlark spawn whose Args expand tree artifacts) and `DeferredSpec::WriteArgs {
snapshot, extra_param_files }` (an `actions.write` of such Args); `Deferred::apply` expands either with the
tree listings of the action's generated inputs, exactly as the two closures of analysis_runtime.rs and the
engine's wrapper (which kept a tool runfiles tree's empty files, now `extra_param_files`) did.

An Args `map_each` is a Starlark function: it stays an in-memory `OwnedFrozenValue`, and a request with a
planned action that still needs one cannot be encoded (`EncodeError::NotSerializable`, with the action id).
W7 may store it by reference.

## Snapshot blockers decided on the plan (update 1 §(c)4)

`PlannedRequest::blockers()` lists the planned actions that keep a request from being saved: build-info
callbacks (their substitutions are this run's workspace status) and `map_each` expansions. Analysis records
these taints when it *registers* the action; `plan_request` settles them on the plan
(`observed_fs::settle_plan_taints`): the registrations move to the recording's `plan_registered`, and only the
actions the plan kept taint the run. The record mode reports

```
bazel-rs-snapshot: plan check: build-info-callback registered 8 (8 distinct), 0 in the plan; map-each-deferred registered 0 (0 distinct), 0 in the plan
bazel-rs-snapshot: plan: 689 actions, encoded N bytes (M outline, K action chunk(s)); encode A ms, decode B ms
```

and `eligible=` no longer has the `unless-pruned` value once the plan decided (it remains only for a run whose
planning did not complete, which errors taint anyway). The dump lists the registrations as `REGISTERED` lines.

## The encoding (crate::snapshot::codec)

postcard (serde, varint integers; the user's directive: binary formats only) in a small sectioned container,
little-endian:

| Offset | Content |
|---|---|
| 0 | magic `BZRSPLN1` |
| 8 | format (u32) |
| 12 | section count (u32) |
| 16 | per section: kind (u32), length (u64) |
| … | one META section (everything but the actions), then the actions in NODES chunks of at most 1,024 |

The framing decodes without copying (borrowed slices); chunks of plans of 4 MiB and more decode on scoped
threads. The derives sit on the in-memory types (no mirror types): snapshots never survive a binary change.
Paths, path pairs, parameter files, runfiles entries and byte strings use raw-byte field helpers (serde's
`PathBuf` refuses non-UTF-8 paths; a `Vec<u8>` is a byte string, not a sequence of numbers); a Starlark
`File` inside an Args vector is encoded field by field (it derives starlark's always-failing `Serialize`).
The encoding is deterministic: no hash order, no per-process ids, no status bytes, no environment values.
L2-W5 embeds these bytes as the PLAN section of its snapshot file.

## Switches

| Variable | Effect |
|---|---|
| `BAZEL_RS_SNAPSHOT_ROUNDTRIP=1` | every full-analysis request is encoded, the planned request dropped, the encoding decoded, and the decoded request re-encoded section by section (it must be identical, else an internal error); the **decoded** request executes. The CLI harnesses that clear the environment forward it like `BAZEL_RS_VERIFY_*` (tests/support/verify_env.rs), so `cargo test` with it exported executes every build of the suite from a decoded plan. Its cost on B2: about 0.1 s and +75–85 MB of peak RSS (the 44.7 MB encoding and a chunk's re-encoding alive with the decoded plan). |
| `BAZEL_RS_SNAPSHOT_PLAN_OUT=<file>` | write the encoding to the file (the determinism gate compares it across thread counts and jitter seeds) |
| `BAZEL_RS_SNAPSHOT=record` | also encodes and decodes once for the report's `plan:` line |
| `BAZEL_RS_TIMING=1` with any of the above | a `PERF-PLAN nodes=… bytes=… meta=… chunks=… encode_ms=… decode_ms=… roundtrip=0|1` line |
| `BAZEL_RS_DEBUG_ENGINE_DROP=inline\|leak` | measurement switches for freeing the engine (below) |

Without a switch nothing is encoded.

## Freeing the engine (PLAN-L2 §5)

After the hand-over the CLI moves the `Engine` — packages, configured rules, toolchain and transition caches
and the Starlark values they hold — to a background thread that drops it while the actions execute; the thread
is never joined, so a drop still running when the command ends is skipped (the process exits; nothing in the
engine has an effect on drop). The repository set (the module graph, repository specifications, the `.bzl`
module cache, shared with the engine) is never dropped at all: it holds only memory, which the process exit
frees. `BAZEL_RS_DEBUG_ENGINE_DROP=inline` drops the engine on the main thread before execution, `=leak` never
drops it (measurement switches).

Measured on ~/code `build //src/beyondwork.ai/tools/oauth2token/...` (release binaries, warm repository
cache; the machine is shared, timings indicative):

| | base 95bda96 | W4: engine dropped in the background | W4 + repository set never dropped (shipped) |
|---|---|---|---|
| warm (689 cache hits), 3 rounds: wall after `execute.end` | 0.22–0.27 s | 0.16–0.19 s | 0.09–0.11 s |
| warm: peak RSS | 1,312–1,316 MB | 1,312–1,316 MB | 1,312–1,314 MB |
| cold actions (689 executed, about 36 s of execution): RSS at `execute.end` | 968 MB | — | 579 MB |
| cold: peak RSS / wall after `execute.end` | 1,316 MB / 0.23 s | — | 1,320 MB / 0.08 s |

The peak is reached at the end of planning, before anything can be freed, so it does not move (lowering it is
Layer 1's work). What freeing changes is the resident memory during a long execution (jemalloc returns the
freed pages as they decay: about 390 MB less at the end of the cold build) and the end of the command (no
drop: 0.13–0.15 s less on every build, test and run). `--nobuild` oauth2token:all: 0.23–0.25 s → 0.09–0.11 s
after the last PERF mark.

## Measured on ~/code (L2-W4, warm, update-mode recipe)

The encoded request (`PERF-PLAN`, the record mode's `plan:` line; encode and decode on the main thread):

| | actions | encoded | encode | decode |
|---|---|---|---|---|
| oauth2token:all `--nobuild` | 689 | 10.76 MB (605 B outline, 1 chunk) | 12–16 ms | 4.3–6.2 ms |
| oauth2token/... build | 691 (with the 2 status placeholders) | 10.76 MB | 12–14 ms | 4.3–4.6 ms |
| test oauth2token_test | 683 | 10.67 MB | 13–14 ms | 4.6–4.8 ms |
| B2 `--nobuild` (prober, pvcautoscaler, decommission) | 3,615 | 44.7 MB (4.5 KB outline, 4 chunks) | 69–80 ms | 15–42 ms (4 chunks in parallel; the high values on a loaded machine) |

About 12–15 KB per action, nearly all of it input paths (absolute source paths and their exec paths, repeated
by every action of a toolchain): PLAN-L2 §5's interned path encoding (W7) would shrink it most. Determinism:
the encoding of oauth2token:all `--nobuild` and of oauth2token/... build is identical at
`--loading_phase_threads` 1, 8 and 32 and with jitter 7 and 40 ms (6 runs each). The record mode now reports
`eligible=yes` for oauth2token:all `--nobuild`, oauth2token/... build and B2 (`plan check: build-info-callback
registered 8 (12 on B2), 0 in the plan`).

## Gates

Unit tests: `snapshot::codec::tests` (every planned shape round-trips byte for byte, non-UTF-8 paths, the
embedded blob, no client environment values, decoded completions expand like the planned ones, fixed chunks
and the parallel decode, damaged encodings refused, `map_each` unencodable and a blocker, unknown blobs
refused) and `request::tests` (status placeholders on success, the failure layout, no status needed without
placeholders, the inherited environment). CLI tests: tests/planned_request.rs — byte-identical output with
and without the round trip for build, `--nobuild`, test, run of a binary and of a test, cache hits and a
failing status command; the encoded request identical across `--loading_phase_threads` 1/8/32 and jitter
seeds 7/40/99 (build, `--nobuild`, test); the record mode's build-info eligibility on bazel_tools'
`cc_build_info` (pruned: eligible; kept: not).
