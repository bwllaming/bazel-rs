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
| `X` `general information` | the three phases as spans on `main`: `evaluate target patterns`, `analysis`, `execution` |
| `X` `package creation` | one per evaluated package, on the thread that loaded it (parallel loading shows as parallel lanes) |
| `X` `configured target` | one per planned target (prepare, Starlark implementation, commit), `args.rule` |
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
