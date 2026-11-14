# `--profile` and the execution logs

## `--profile=<file>`

`build`, `test` and `run` write a Chrome trace JSON profile in the layout of
Bazel 9.3's JsonTraceFileWriter (profiler/JsonTraceFileWriter.java), gzipped
when the name ends in `.gz` (`src/profile.rs`). chrome://tracing, Perfetto
(ui.perfetto.dev) and the usual Bazel profile readers load it.

```json
{"otherData":{"bazel_version":"bazel-rs 0.1.0 (Bazel 9.3.0 compatible)","build_id":"<uuid>",
  "output_base":"<state dir>","date":"<RFC 3339>","profile_start_ts":<epoch ms>},
 "traceEvents":[ ... ]}
```

| events | what |
|---|---|
| `M` `thread_name`/`thread_sort_index` | the fake `Critical Path` thread 0, `main` (tid 1), each recording thread (its Rust thread name, or `worker-N`) |
| `i` `build phase marker` | ProfilePhase descriptions at the crate::perf_mark points: `Initialize command` (0), `Evaluate target patterns` (`select.start`), `Load and analyze dependencies` (`plan.start`, or `snapshot.hit`), `Prepare for build` (`plan.end`), `Build artifacts` (`execute.start`), `Complete build` (`execute.end`); `Launch Blaze` as a zero-length `X` |
| `X` `general information` | the three phases as spans on `main`: `evaluate target patterns`, `analysis`, `execution`; the analysis's steps (either engine, from the PERF marks): `analysis phase 1` (`plan.start` → `plan.phase1_analysis_done`), `analysis phase 2` (→ `plan.phase2_actions_done`), `planning tail` (→ `plan.tail.end`); the evaluator's deferred walks (`evaluator deferred walks`, its pass 2) |
| `X` `skyframe evaluator` | the evaluator's parallel evaluation (`evaluator phase 1 and pass 1`: the pool's loading and phase 1 with the sequential pass 1 beside it), as genuine's `Parallel Evaluator evaluation` spans in that category |
| `X` `package creation` | one per package the build loaded (committed), on the thread that loaded it (parallel loading shows as parallel lanes), named as genuine's PackageFunction names it (`PackageIdentifier.toString()`: `p` for `//p`, `` for the root package, `@@repo//pkg`) |
| `X` `configured target` | one per configured target (each plan entry's turn on the planning thread: its commit, or the merge of what a worker computed), `args.rule` (a bazel-rs category: genuine records none by default; its `skyfunction` task, off by default, has `CONFIGURED_TARGET` spans) |
| `X` `action processing` | one per executed or cache-checked action on its scheduler worker: name `<Mnemonic> <primary output>`, `args.mnemonic`; `args.target` with `--experimental_profile_include_target_label`, `args.out` with `--experimental_profile_include_primary_output`; test attempts as `Testing //pkg:t` (`TestRunner`) |
| `X` `critical path component` | on tid 0: the chain of executed actions whose durations add up to the most (dependencies from the plan), `args.tid` |

`--generate_json_trace_profile` without `--profile` writes
`<output base>/command-<id>.profile.gz` (Bazel's default name); the native
default (`auto`) writes no profile — Bazel writes one on every command. The
other profiler options (`--slim_profile`, `--record_full_profiler_data`,
`--experimental_profile_additional_tasks`, `--experimental_collect_*`,
`--memory_profile`, ...) are accepted and ignored.

Recording: each thread appends spans to its own buffer (registered once; an
uncontended mutex per thread), the file is written when the command ends.
Nothing orders the spans across threads: the planning engine (the parallel
evaluator by default, docs/EVALUATOR.md) records them as it works, with no
ordered flushes or other scheduling of its own for the profile.

### Parity

What genuine Bazel 9.3.1 writes for analysis (`build --nobuild --profile`,
captured on a scratch workspace; ProfilerTask.java, BlazeRuntime's
profiled tasks): the `otherData` keys, `M` thread metadata for every lane,
`build phase marker` instants on `main`, `general information` spans of
its command steps on `main` (`runAnalysisPhase`, `evaluateTargetPatterns`,
`skyframeExecutor.configureTargets`, …), `skyframe evaluator` spans
(`Parallel Evaluator evaluation`) on `main`, `package creation` spans on
the `skyframe-evaluator-*` threads, `Fetching repository` spans, Starlark
spans only above their 50 ms threshold, `gc notification`s, counters
(`C`). Nothing per configured target by default (the `skyfunction` task is
off unless `--experimental_profile_additional_tasks=skyfunction`). Its
spans come from many threads in a nondeterministic order, and a restarted
SkyFunction repeats its span (genuine's `package creation` of a package
can appear twice).

So the order of spans, their threads, timings and lanes are not part of
parity. What is:

| | requirement |
|---|---|
| structure | the JSON trace above: `otherData` (`bazel_version`, `build_id`, `output_base`, `date`, `profile_start_ts`), a `thread_name` metadata event for every lane used, integer `ts`/`dur` |
| phases | the build phase markers of the command's phases |
| `package creation` | exactly one span per loaded package (the `N packages loaded` count), named as genuine names it; a speculative load nobody takes (the walk's prefetch) or an attempt aborted for its inline re-run records none (`profile::PendingSpan`, committed with the load unit) |
| `configured target` | exactly one span per configured target (the `N targets configured` count; `profile::TargetSpan`, one per plan entry; a `--keep_going` entry skipped above a failure has none), the (label, rule) multiset the same on either engine; the speculative flag phase 2s' scratch planning records none (`profile::without_targets`) |
| `action processing`, critical path | per executed or cache-checked action, as above |

Engine-specific (allowed to differ): the evaluator's own phase spans
(`skyframe evaluator`, `evaluator deferred walks`), lanes and their names
(`eval-N` pool threads, `bazel-rs-prefetch`, workers), timings.

Tested: `tests/eval_pass2.rs` (`the_profile_has_the_walks_targets_and_packages`:
the walk's and the evaluator's (label, rule) and package multisets equal
and equal to the counts, at 1, 8 and 32 threads, jitter, restarts, the
workers' prepares, with flushes deferred and flags speculated),
`tests/eval_default.rs` (`--profile` plans with the evaluator, the output
the walk's), `tests/bep.rs` (the file's structure). ~/code
(`build --nobuild --profile //src/...`, 80d6724f3a): walk and evaluator
133,156 `configured target` spans (29,497 labels) and 10,264
`package creation` spans, multisets equal, matching the `Analyzed` line.
Without a profile (and without a build event file, which needs the phase marks
and the critical path) every hook is one relaxed atomic load.

Overhead (~/code `build //src/beyondwork.ai/tools/oauth2token/...`, release
binary, `--profile=x.gz --build_event_json_file=x --execution_log_compact_file=x`
vs none, private caches; shared machine, indicative):

| run | without | with |
|---|---|---|
| cold (692 actions executed) | 90.7 s | 90.6 s |
| warm full analysis (`BAZEL_RS_SNAPSHOT=off`, 692 cache hits), 3 runs each | 4.05 / 3.96 / 4.00 s, peak 748-752 MB | 4.19 / 4.00 / 4.23 s, peak 755-764 MB |
| warm snapshot hit | 0.29 s | 0.30 s |

The warm profile holds ~21k spans (19k configured targets, 1.1k packages, 694
actions) in 315 KB gzipped.

## Execution logs

`src/exec_log.rs`, schema `src/main/protobuf/spawn.proto`. At most one of
(SpawnLogModule; otherwise `Must specify at most one of ...`, exit 2):

| option | format |
|---|---|
| `--execution_log_compact_file=F` (Bazel 9's recommendation; `experimental_` name too) | a zstd stream of length-delimited `ExecLogEntry`: the Invocation entry, then per spawn its File/Directory/UnresolvedSymlink entries (each path logged once, later spawns reference the id), an InputSet in the action's input order, the Spawn (args, env, input set, outputs by id or `invalid_output_path`, label, mnemonic, exit code, status, runner, cache hit, remotable/cacheable flags, metrics) |
| `--execution_log_json_file=F` | `SpawnExec` messages printed by `JsonFormat.printer().alwaysPrintFieldsWithNoPresence()` (pretty, concatenated), inputs sorted by path, actual outputs with SHA-256 digests |
| `--execution_log_binary_file=F` | the same messages length-delimited |

`--[no]execution_log_sort` (default on) orders the expanded logs producers
first (StableSort). Logged: every spawn the executor ran (failures included)
and remote cache hits (`runner: remote cache hit`); native writes, transforms
and action cache hits are not spawns, as in Bazel. Runners: `worker` (a persistent
worker, crate::worker), `linux-sandbox` (the Landlock sandbox),
`processwrapper-sandbox`, `local`. Not modelled: tool
input sets (`tool_set_id` 0, `is_tool` false), runfiles tree entries (the
staged files are logged as inputs), timeouts, measured memory; the compact
log's input sets are flat (no shared transitive sets).

Parity (`tests/bep.rs`, genuine captures in `tests/fixtures/bep/`): the JSON
log's spawns (keys, argv, environment, inputs and their digests, outputs,
mnemonic, exit code, label, metrics keys) and the compact log's resolved
spawns (args, label, mnemonic, exit code, inputs and outputs with digests)
equal genuine's.
