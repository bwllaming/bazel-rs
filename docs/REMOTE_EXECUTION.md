# Remote execution (`--remote_executor`)

`bazel-rs` executes spawns on a REAPI v2 remote executor like Bazel 9.3.0:
`--remote_executor=grpc://HOST[:PORT]`, `grpcs://HOST[:PORT]`, `HOST[:PORT]`
(TLS) or `unix:PATH`, over the gRPC client of the remote cache
(docs/REMOTE_CACHE.md, "The gRPC remote cache"). Implementation:
`src/remote_execution.rs` (Execute/WaitExecution, ExecuteResponse, error
texts, retry classification, platforms), `src/remote_grpc.rs` (endpoints,
capabilities), `src/remote_cache.rs` (setup, input upload, output
download), `src/action.rs` (`remote_spawn`, the build action path),
`src/test_runner.rs` (test spawns), `src/sandbox.rs` (the `remote`
strategy), `src/engine.rs` (`remote_platform`). Pinned source:
`/tmp/opencode/bazel-9.3-reference`; Java paths below are relative to
`src/main/java/com/google/devtools/build/lib/`.

## Options

| option | native |
| --- | --- |
| `--remote_executor` | the executor endpoint; an empty `--remote_cache` reuses it (`RemoteModule.setup`) |
| `--remote_cache` equal to `--remote_executor` | one endpoint with execution and cache capabilities (`EXECUTION_AND_CACHE`) |
| `--remote_cache` another gRPC target | separate endpoints (`EXECUTION`, `CACHE`) |
| `--remote_cache=http(s)://…` with an executor | `Cannot combine gRPC based remote execution with HTTP-based caching` |
| `--remote_exec_header` | sent with every call on the executor endpoint, after `--remote_header`; a shared endpoint carries them on cache calls too and never `--remote_cache_header` (`TracingMetadataUtils.newExecHeadersInterceptor`) |
| `--remote_default_exec_properties=K=V` | `PlatformUtils` defaults (last value of a name wins) |
| `--[no]remote_local_fallback` | local (standalone) fallback after a remote failure |
| `--remote_local_fallback_strategy` | `local`/`standalone` only (the default) |
| `--remote_print_execution_messages=failure\|success\|all` | `Remote server execution message: …` |
| `--remote_execution_priority`, `--remote_result_cache_priority` | `0` only |
| `--remote_timeout`, `--remote_retries`, `--remote_retry_max_delay`, `--remote_instance_name`, TLS and credential helpers | as for the cache; Execution calls have **no** deadline (`RemoteGrpcServiceConfig` covers the cache services only) |
| `--remote_download_outputs` | accepted; every output is downloaded (`all`) |

These options are execution-only: they are left out of the analysis
snapshot key (`src/snapshot/key.rs`).

## Strategies

With an executor `remote` heads the default strategies: `remote, worker,
sandboxed, local` (`BazelStrategyModule.java:67-80`). `remote` is
registered whenever a remote cache or executor is configured
(`RemoteModule.registerSpawnStrategies`), so `--spawn_strategy=remote`
without either is Bazel's `'remote' was requested … no strategy with that
identifier was registered`, and with a cache only it can execute nothing.
It can execute a spawn that may be executed remotely
(`RemoteExecutionService.mayBeExecutedRemotely`, `Spawns.java:52-56`): no
`local`, `no-remote` or `no-remote-exec` requirement. Such spawns resolve to
the next strategy (and the remote cache, `RemoteSpawnCache`, serves them
unless `no-remote-cache`/`no-remote`/`no-cache`/`local`). `--strategy=M=remote`
with a `local` spawn is `M spawn cannot be executed with any of the
available strategies: [remote]`. `requires-network`/`block-network` do not
change remote resolution (they are the server's business, as in Bazel).

## The spawn (`RemoteExecutionService.buildRemoteAction`)

* **Command**: the argv, the environment sorted by name, the outputs
  (`output_paths` from REAPI 2.1 on, else `output_files` and
  `output_directories`; the endpoints' minimum version decides,
  `initUseOutputPaths`), and the `Platform`.
* **Input root** (`merkletree/MerkleTreeComputer.java`): files at their exec
  paths, every file `is_executable`; tree artifacts and source directories
  as their files, an **empty** one as an empty directory; unresolved
  symlinks as `SymlinkNode`s; parameter files; runfiles as staged; and every
  **tree artifact output as an empty directory** (`EmptyInputDirectory`,
  `doBuildForSpawn:341-348`). The cache key uses the same root, so tree
  outputs and empty trees now key like genuine's.
* **Action**: command and root digests, `timeout` from the spawn's `timeout`
  requirement (test spawns: the test timeout), `do_not_cache` for a spawn
  that may not be cached remotely, the `CacheSalt`, and the `Platform`.
* **Platform** (`analysis/platform/PlatformUtils.java`): the spawn's
  combined exec properties (exec group: execution platform under target
  `exec_properties`, `crate::exec_groups`; genrule: the default group's),
  the execution platform's `exec_properties`, else its
  `remote_execution_properties` (protobuf text format; a parse error is
  `Failed to parse remote_execution_properties from platform L`), else
  `--remote_default_exec_properties`, which also fill in under target
  properties when the platform has none; sorted by name. The analysis
  records the inputs (`Action::remote_platform`); the defaults join at
  execution. Execution properties are therefore no longer a reason to keep
  a spawn off the **remote cache** either: its key carries the Platform like
  genuine's. A spawn whose execution platform analysis did not resolve still
  stays local (`Remote Cache: not used for …`).

## The flow (`RemoteSpawnRunner.exec`, `remote_spawn`)

1. The capability check of each endpoint once (execution: `exec_enabled`,
   `Remote execution is not supported by the remote server, or the current
   account is not authorized to use remote execution.`, the execution digest
   functions, singular or repeated; cache: as before). A failure fails the
   spawn with `Failed to query remote execution capabilities: …` (no
   fallback).
2. With `--remote_accept_cached` and a remotely cacheable spawn:
   `GetActionResult`; a hit with exit code 0 and every declared output
   downloads and finishes the spawn (a failed or incomplete result, or a
   lost blob, is a miss).
3. Upload (`RemoteExecutionCache.ensureInputsPresent`): one
   `FindMissingBlobs` over every input file, `Directory` blob, the Action
   and the Command that this invocation has not yet seen present; a
   `ByteStream.Write` per missing blob, read from the staged file (verified
   against its digest) or memory. Blobs found or uploaded are not asked
   again.
4. `Execute` (`skip_cache_lookup` once a cached result proved unusable,
   SHA256, the instance name), then `WaitExecution` of the last operation
   whenever a stream ends before the operation is done (an OK end or a
   retried failure); a NOT_FOUND `WaitExecution` starts over with
   `Execute` (`GrpcRemoteExecutor`). An Operation `error` or a non-OK
   `ExecuteResponse.status` is an ExecutionStatusException. Calls are
   retried by the RemoteRetrier (`--remote_retries`, exponential backoff,
   GRPC_RESULT_CLASSIFIER: NOT_FOUND plus the transient codes; an
   execution timeout is converted to FAILED_PRECONDITION and never
   retried) and refreshed once when UNAUTHENTICATED.
5. The ExecuteRetrier around 3–6 (`--remote_retries` attempts): a status
   with a RetryInfo detail (its delay), or FAILED_PRECONDITION whose
   PreconditionFailure violations are all `MISSING` (inputs uploaded again,
   forced), or lost output blobs (retried without cached results), or a
   `cached_result` that failed or lacks outputs. Statuses without details
   are final.
6. Download (`--remote_download_outputs=all`): files (inline contents or
   ByteStream, verified), symlinks, tree artifacts from their `Tree`
   (directories, files with executable bits, symlinks), stdout and stderr.
   The outputs are written into the spawn's execution root and published
   through the content-addressed store like executed outputs; stdout/stderr
   are replayed. The server stores the result; nothing is uploaded.

**Failures** (`RemoteSpawnRunner.handleError`, `Utils.grpcAwareErrorMessage`):

* A non-zero exit fails the action like a local one:
  `action 'ID' failed (exit status: N):\nstdout:…\nstderr:…`.
* An execution timeout (`DEADLINE_EXCEEDED` as the response's status):
  `action 'ID' failed: (Exit 142): remote spawn timed out (failed due to
  timeout.)`; a test is TIMEOUT.
* Anything else: `action 'ID' failed: (Exit 34): MESSAGE`, MESSAGE being
  `Remote Execution Failure:\n<Status Name>: <message>\n<details>` (e.g.
  `Unavailable`, `Failed Precondition`; details rendered like Bazel:
  `Retry delay recommendation of 2.500s`, `Precondition Failure:\n
  (MISSING) subject: description`, Debug/Quota/BadRequest/RequestInfo/
  ResourceInfo/Help, else `Unrecognized error detail: …`) plus `\n` and
  the response's `message`; for call failures `CODE: description`
  (`UNAVAILABLE: io exception`, credential helper causes appended).
* With `--remote_local_fallback` every failure but an execution timeout
  runs the spawn with the standalone strategy and uploads the result like
  `execLocallyAndUpload` (remotely cacheable spawns, with the native upload
  opt-in of docs/REMOTE_CACHE.md).

Remote execution itself needs no upload opt-in: the server executes exactly
the uploaded Command, so its results are correct by construction.

## Tests

Test actions resolve with the `TestRunner` mnemonic and run remotely like
`StandaloneTestStrategy` with `RemoteSpawnRunner`:

* the test spawn: `[test-setup.sh, TEST_BINARY, args…]`, the test
  environment, the runfiles tree (with `_repo_mapping` and empty files),
  test-setup.sh and generate-xml.sh as inputs, the outputs of
  `TestRunnerAction.getSpawnOutputs` (test.xml, test.exited_prematurely,
  test.shard when sharded, test.warnings, test.raw_splitlogs/test.splitlogs,
  test.unused_runfiles_log, test.infrastructure_failure, test.outputs as a
  directory or test.outputs/outputs.zip, the undeclared-outputs manifest
  files), none mandatory, `Action.timeout` the test timeout, `no-cache`
  when the cached result may not be accepted
  (`--cache_test_results`, `external`); stdout becomes test.log, stderr
  test.err (appended to the log like a local test's);
* without a test.xml, the XML generating spawn: generate-xml.sh and
  test.log as inputs, `[generate-xml.sh, test.log, test.xml, SECONDS,
  EXIT]`, the environment plus `TEST_NAME` (and the shard variables),
  test.xml its only output, the test's execution info without `no-cache` or
  timeout.

The rest (premature exit, sharding check, attempts and flaky retries,
caching of results in the native test cache, summaries) is the local
runner's. A remotely failed test spawn is `Testing NAME failed: (Exit 34): …`.

## Execution log, BEP, summaries

Remote spawns enter `--execution_log_*_file` with runner `remote`
(`cacheHit` for an Action Cache or server-side cached result) or `remote
cache hit` for a remote-cache hit of another strategy. BEP's
`INFO: N processes:` line counts `R remote cache hit` and `E remote`;
TestResult's strategy is `remote`. The native summary adds `E executed
remotely` (`bazel-rs: … (experimental local execution; E executed
remotely, local spawns …)`).

## Evidence

* Unit tests: `src/remote_execution.rs` (error texts, retry
  classification, timeouts, operations, platforms, text format),
  `src/remote_grpc.rs` (execution capabilities), `src/reapi.rs`.
* `tests/remote_execution.rs` against the in-process server
  (`tests/support/grpc_cache_server.rs`, whose Execution service runs each
  Command in a fresh directory built from the uploaded root, failing with
  MISSING violations when blobs lack, and scripted stream breaks, lost
  operations, operation errors and response statuses): genuine keys,
  hits on a fresh native cache, tags, platforms and headers, trees and empty
  trees, symlinks, failures, timeouts, WaitExecution and NOT_FOUND
  restarts, MISSING re-uploads, retries, fallback, capability errors,
  strategies, options, remote tests with their XML spawns and the
  execution log.
* Paired checks (ignored; `BAZEL_RS_GENUINE_BAZEL=<9.3.0 ELF>`):
  * `genuine_bazel_sends_identical_execute_requests`: genuine and native
    build a 12-spawn fixture (genrules, run/run_shell, a chain, an
    exec-configured tool, `no-remote-cache`, `exec_properties`, tree and
    empty tree outputs and inputs) with `--remote_default_exec_properties`
    against fresh in-process executors: all 12 Execute requests are
    identical (Action digest, Command, input root, platform); outputs are
    identical; native then reads genuine's results (11 remote cache hits,
    the `no-remote-cache` genrule executes). Receipt
    `tests/fixtures/remote_execution/receipt.json`. Call counts differ
    (genuine: 13 FindMissingBlobs and 12 Reads, native 12 and 14: genuine
    downloads top-level outputs only).
  * `genuine_bazel_and_native_share_a_nativelink_executor`
    (`BAZEL_RS_NATIVELINK=<nativelink>`): NativeLink 1.8.2 (the
    `nativelink-1.8.2-x86_64-unknown-linux-musl` GitHub release binary: CAS,
    AC, scheduler and a local worker in one process on loopback, private
    directories): genuine and native write the same 12 Action Cache keys
    (NativeLink also stores the `do_not_cache` result), outputs are
    identical, native reads genuine's results (11 remote cache hits) and
    genuine reads native's (`11 remote cache hit, 1 internal, 1 remote`).
    Receipt `tests/fixtures/remote_execution/nativelink_receipt.json`.
  * `genuine_bazel_sends_identical_test_spawns`: a passing and a failing
    test: both test spawns are identical (Action digest, Command,
    environment, outputs, input root). Receipt
    `tests/fixtures/remote_execution/test_spawn_receipt.json`. (XML spawns
    carry the measured duration, so their keys vary between runs on both
    sides.)
  * No user server was contacted; only loopback servers this suite starts.

## Divergences

* Outputs are always downloaded (`--remote_download_outputs=all`); no
  build without the bytes.
* The capability check is lazy (the first spawn needing it), not at
  startup; a build whose actions all hit the native action cache contacts
  no endpoint.
* gRPC `grpc-status-details-bin` trailers are not parsed: call failures
  carry no details (ExecuteRetrier never retries them); details inside
  Operations and ExecuteResponses are used.
* LocalizedMessage details are not preferred over the status message; a
  JVM-locale-dependent choice in Bazel.
* A test target's own `exec_properties` are not recorded for its test spawn
  (the defaults apply); failed remote action spawns are not written to the
  execution log; outputs of failed remote actions are not downloaded;
  server logs are not downloaded; the XML spawn has no local fallback.
* `--incompatible_remote_symlinks` does not exist in 9.3 (output symlinks
  always come back as symlinks); `--experimental_remote_require_cached`,
  `--remote_grpc_log`, `--remote_proxy`, scrubbing, persistent-worker keys
  for remote workers (`--experimental_remote_mark_tool_inputs`) and
  compression stay unmodelled options.
* Exit codes: action failures exit like other native failures (genuine
  uses 34 for remote errors).
