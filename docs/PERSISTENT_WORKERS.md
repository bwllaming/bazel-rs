# Persistent workers

Bazel 9.3 runs a spawn as a request to a long-lived **persistent worker**
process when its execution requirements say it can. bazel-rs implements the
same `worker` spawn strategy in `src/worker.rs`, wired into the executor in
`src/action.rs`.

## Value on ~/code

No ~/code action supports workers today. The planned graph of
`//src/beyondwork.ai/tools/oauth2token/... //src/ts/... //src/rust/...`
has 27,554 actions and none of them has `supports-workers` or
`supports-multiplex-workers`.

The only `.bzl` files in the fetched external repositories that set these
requirements are:

- rules_python sphinxdocs
- rules_kotlin
- rules_java `android_lint`
- rules_swift

~/code instantiates none of those rules: it has no Java, Kotlin or Swift
sources. Its TypeScript uses `js_run_binary`/`js_library` rather than the
`ts_project` tsc worker. rules_rust, rules_go, protobuf and rules_js never
request workers.

So this feature saves no time on ~/code yet. It is a correct, minimal model
of 9.3 for repositories that use worker-capable rule sets, such as Java,
Kotlin, Scala or ts_project with workers.

## Strategy selection

- **Default strategy list.** It is Bazel's: `worker`, `sandboxed`,
  `standalone` (`BazelStrategyModule.java:67-80`).
  `--spawn_strategy`, `--strategy=<mnemonic>=` and `--genrule_strategy`
  select it like any other strategy.
- **When `worker` can execute a spawn** (`WorkerSpawnRunner.canExec`):
  - the spawn's `execution_requirements` must map `supports-workers` or
    `supports-multiplex-workers` to exactly `"1"`. A rule tag gives the
    value `""`, which does not count. `Spawns.supportsWorkers`.
  - the spawn must pass `--experimental_worker_allowlist`, matched against
    the worker key mnemonic.
  - the spawn must have tool files. This engine approximates that as: the
    executable is a File, either a `FilesToRunProvider`'s or a File
    `executable`. `run_shell` with `tools` never selects `worker`, because
    tools are not tracked separately from inputs.
- **Spawns that cannot use `worker`** fall through to the next strategy.
  - If `worker` is the only strategy requested, the build fails with Bazel's
    message (`... available strategies: [worker]. ...`).
  - The strategy list prints like Java's `List.toString`.
- **The strategy is not part of the action key.** A worker-built entry is a
  cache hit for a standalone or sandboxed build, and the reverse.
  Outputs are byte-identical; `tests/persistent_workers.rs` checks this.

## Worker key and requests

`WorkerParser` and `WorkerSpawnRunner.createWorkRequest` define the worker
key and each request.

### Worker argv

- The spawn's argv without its flag-file arguments, followed by
  `--persistent_worker` and the mnemonic's `--worker_extra_flag`s.
- Without `--experimental_worker_strict_flagfiles`, every argument matching
  `(@|--?flagfile=).+` is a flag file. The legacy pattern also takes `@@`
  arguments. No flag file gives Bazel's error, which lists the argv as a
  Java list.
- With `--experimental_worker_strict_flagfiles`, only the last argument may
  be a flag file, and it must be one.
- A relative `argv[0]` that has a directory is made absolute against the
  worker's working directory (`makeExecPathAbsolute`).

### Worker key

The key is made of:

- the worker argv
- the environment: the spawn's own, plus the local `TMPDIR`
- `worker-key-mnemonic`, or the mnemonic if it is not set
- the protocol (`requires-worker-protocol`: `json` or `proto`, default
  proto). Any other value fails the spawn with
  `Exec failed due to IOException: ...` and exit code 36.
- sandboxed and multiplex flags: multiplex when `--worker_multiplex`
  (default true) and `supports-multiplex-workers=1`; sandboxed for a
  singleplex key under `--worker_sandboxing`.

Genuine also keys the combined digest of the tool files. Within one bazel-rs
command an exec path names exactly one content, so the argv already
identifies the tools.

### WorkRequest

- **`arguments`**: the flag files expanded line by line, recursively. `@@x`
  and `@repo//label` arguments stay literal. A flag file that is not a
  declared input fails the spawn.
- **`inputs`**: every input file with its SHA-256 as lowercase hex text
  (the proto `bytes` field holds the hex string's UTF-8). Then the parameter
  files.
  - Tree artifacts are expanded to their files.
  - Runfiles trees are left out.
  - A source directory is listed without a digest.
  - Declared divergence: native lists inputs sorted by execution path.
    Genuine uses nested-set order: inputs, then tools, then parameter files.
    Workers treat the list as the set of readable files.
- **`request_id`**: `0` for singleplex keys. Multiplex keys use a
  process-wide counter starting at 1.
- **`verbosity`**: `10` under `--worker_verbose`.

### Encoding

- **Proto:** `writeDelimitedTo`, a varint length then the message.
- **JSON:** one ndjson line in protobuf `JsonFormat` style:
  - proto3 field names, with default values omitted
  - `bytes` fields in base64
  - Gson's HTML-safe string escaping: `= < > & '` and U+2028/U+2029 become
    `\u00XX`

  The genuine capture pins all of these rules.

### Responses

- **Proto:** a delimited `WorkResponse`; unknown fields are skipped.
- **JSON:** one object (lenient: leading whitespace is skipped, and the
  object may span lines). Fields are `exitCode`, `output` and `requestId`;
  unknown fields are ignored.
- **Result:** the response `output` becomes the spawn's stderr, and
  `exit_code` its exit status. A non-zero exit code fails the action like a
  failed process.

## Pool, lifetime and working directories

- **Pool.** Workers are pooled per key (`WorkerPoolImpl`).
  - `--worker_max_instances` limits singleplex keys; the default is 4.
  - `--worker_max_multiplex_instances` limits multiplex keys; the default
    is 8.
  - Both take `[mnemonic=]value`, where the value is an integer, `auto`, or
    `HOST_CPUS[*f|-n]`. An unnamed value sets the default.
  - A spawn waits while its key is at the maximum.
- **No server.** The pool lives for one bazel-rs command. All workers are
  stopped when the command ends: after `run()`, on every `exit()` path, and
  before `bazel-rs run` execs the target.
  - On stop, stdin is closed and the worker's process group is sent
    SIGKILL.
  - `--worker_quit_after_build` is accepted; it is always in effect.
  - Gain is only within one build. A warm JIT therefore does not carry over
    to the next command.
- **Process start.** A worker process starts on its key's first request,
  with:
  - its own process group
  - signals reset, as for the process-wrapper
  - stdin and stdout piped
  - stderr appended to its log file

  It is registered with the memory watchdog like every grouped spawn. It
  does **not** run under the process-wrapper, because the wrapper's stdin is
  its control channel. Genuine likewise starts workers directly
  (`SingleplexWorker.createProcess`).
- **Working directory.** Each worker has a private working directory:
  `<cache>/work/bazel-workers-<pid>/worker-<id>-<mnemonic>/execroot`.
  The log file sits next to it as `worker-<id>-<mnemonic>.log`; multiplex
  keys use `multiplex-worker-...`.
  - bazel-rs has no shared execution root, so every worker uses genuine's
    sandboxed-worker layout (`SandboxedWorker` / `WorkerExecRoot`).
  - Before each request the directory is emptied, keeping the directory
    itself because it is the worker's cwd. The spawn's inputs are then
    hardlink-staged into it, exactly like a sandboxed spawn's
    (docs/HARDLINK_STAGING.md).
  - The staged-inode check runs after the response.
  - Outputs are published from the directory as for any spawn.
  - The worker's own executable and jars are re-linked to the same
    content-store inode on every request, so a running process that reopens
    them sees identical bytes.
- **`--worker_sandboxing`.** The worker is started through the Landlock
  sandbox worker in `--exec` mode (`crate::sandbox::wrap_worker`).
  - The ruleset is the sandboxed strategy's: the working directory, TMPDIR,
    `/tmp` and `/dev` are writable. The sandbox worker then execs the worker
    in its own place, so stdin and stdout stay the protocol streams and the
    Landlock domain survives the exec.
  - Network stays allowed; genuine's sandboxed workers isolate only the
    filesystem.
  - A setup failure goes to the worker log with exit code 126 and surfaces
    as "did not return a WorkResponse".
- **Cleanup.** Working directories are removed when the command ends. The
  logs are kept when a failure message named one or under
  `--worker_verbose`. The cache GC sweeps stale `work/` entries.

## Failures

Each failure invalidates the worker. The process is killed and the next
spawn of the key starts a new one, which models crash and restart.
Messages follow `WorkerSpawnRunner`, inside native's
`action '<id>' failed: ...` frame:

- A worker exits without responding:
  `Worker process did not return a WorkResponse:` followed by the log
  section (`---8<---8<--- Start of log, file at <log> ---8<---8<---` ...).
- An unparseable response:
  `Worker process returned an unparseable WorkResponse!` with the recorded
  response bytes and the log section. The Java exception details are reduced
  to one `java.io.IOException: <reason>` line.
- A failed write: `Worker process quit or closed its stdin stream when we
  tried to send a WorkRequest:`.
- A response with `was_cancelled`: `Received cancel response for N without
  having cancelled`.

## Multiplex workers

**Declared divergence.** A multiplex key's requests carry non-zero request
ids like genuine's, but each process has at most one request in flight, and
there are up to `--worker_max_multiplex_instances` processes per key.
Genuine runs one process per key and up to that many concurrent requests in
it.

bazel-rs stages one spawn's inputs per working directory, so concurrent
requests would need a shared execution root. That requires
`WorkRequest.sandbox_dir` multiplex sandboxing, which is not modelled.

`--experimental_worker_multiplex_sandboxing` with
`supports-multiplex-sandboxing` marks the key sandboxed. Otherwise
multiplex keys are not sandboxed, as in genuine.

## Not modelled

These options are still rejected as unsupported options, as before:

- Worker cancellation (`--experimental_worker_cancellation`), because there
  is no dynamic execution.
- Cgroups and memory limits.
- Metrics collection and `--experimental_shrink_worker_pool`.
- `--high_priority_workers`.

`--worker_verbose` prints genuine's
`INFO: Created new ... worker (id N, key hash H), logging to <log>` line.
The key hash is native's own.

## Evidence

The genuine capture is `tests/fixtures/persistent_workers/reference_receipt.json`,
produced by `/tmp/opencode/perf-research/pw-capture.py` from Bazel 9.3.0
`--batch` on the fixture workspace. It has 25 cases:

- both protocols
- two keys, `worker-key-mnemonic`, data and tree-artifact inputs, escaping
- noisy output, failures, crash, garbage response, missing flag file,
  invalid protocol
- `supports-workers=true`, standalone, sandboxed, `--strategy=X=worker`
- allowlist, multiplex, `--noworker_multiplex`, `--worker_sandboxing`,
  extra flags, strict flag files, verbose

The fixture worker `pkg/worker.py` traces process starts and requests.
`tests/persistent_workers.rs` replays every case natively and requires the
same exit code, outputs and normalized trace. It also checks:

- worker and standalone builds share keys and outputs
- no worker outlives the command
- a crashed worker is replaced
