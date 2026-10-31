# Credential helpers and the HTTP and gRPC remote caches (N41)

`bazel-rs` implements Bazel 9.3.0's credential helpers, its HTTP remote
cache and its REAPI v2 gRPC remote cache (section "The gRPC remote
cache"). Action keys are computed exactly as Bazel computes them, so a native
build reuses cache entries genuine Bazel wrote and genuine Bazel reuses
entries a native build wrote. Implementation: `src/credentials.rs`,
`src/reapi.rs`, `src/remote_cache.rs`, the spawn model in `src/action.rs` and
`src/analysis_runtime.rs`, the genrule action in `src/engine.rs`, the option
plumbing in `src/main.rs`. Pinned source: `/tmp/opencode/bazel-9.3-reference`
(e89755b); Java paths below are relative to
`src/main/java/com/google/devtools/build/lib/`.

## Credential helpers

Options (`authandtls/AuthAndTLSOptions.java:202-336`), accepted by `build`,
`query` and `mod` (also from rc files, e.g. `common --credential_helper=…`):

* `--credential_helper=[SCOPE=]PATH` (old name `--experimental_credential_helper`,
  repeatable). The scope is everything before the first `=`. It must be an
  ASCII domain name per `IDN.toASCII(…, USE_STD3_ASCII_RULES)` (letters,
  digits and hyphens, labels of 1–63 characters, no leading or trailing
  hyphen), optionally prefixed by `*.`. Errors use Bazel's texts:
  `Credential helper scope must not be empty`,
  `Credential helper scope 'S' must be a valid domain name with an optional leading '*.' wildcard`,
  `Credential helper path must not be empty`, each prefixed by
  `While parsing option --credential_helper=V: `. Non-ASCII scopes need
  nameprep and Punycode; they fail explicitly.
* `--credential_helper_timeout` (default `10s`) and
  `--credential_helper_cache_duration` (default `30m`) use
  `Converters.DurationConverter` (`common/options/Converters.java:178-210`):
  `0` or `<digits>(d|h|m|s|ms|ns)`, else `Illegal duration 'V'.`.

**Paths** (`runtime/CommandLinePathFactory.java`). `%workspace%/…` is
relative to the workspace root, an absolute path is used as is, and a single
segment is looked up on the client's `PATH` (absolute entries only, the first
executable regular file). Errors: `Path 'P' must not contain any uplevel
references ('..')`, `Path 'P' must either be absolute or not contain any path
separators`, `Could not find file with name 'N' on PATH '…'`, `Unknown root R`.
Like Bazel, every helper path resolves when the command starts, for every
command (`bazel/BazelRepositoryModule.java:446-477`): the error is printed as
`ERROR: …` and the command ends with `Error initializing credential helper`.
With an http(s) remote cache the remote module reports it again and the exit
message is `Error initializing RemoteModule` (`remote/RemoteModule.java:604-621`).
`%install_base%` is a real Bazel root that the native profile does not model;
it fails explicitly.

**Matching** (`credentialhelper/CredentialHelperProvider.java:66-116`): the
URI's host (as written, no case folding) selects an exact scope, else the
nearest `*.` scope walking up the parent domains (the wildcard also covers the
domain itself), else the unscoped helper. Later options for the same scope
win.

**Protocol** (`credentialhelper/CredentialHelper.java:69-172`): the helper runs
as `PATH get` in the workspace root with the client environment (the native
process's environment), receives `{"uri":"URI"}` on stdin (Gson's
HTML-safe escaping, e.g. `=` as `\u003d`; a helper that ignores stdin is
fine) and prints `{"headers":{"Name":["value",…]},"expires":"RFC 3339"}`.
Unknown keys are ignored; `expires` follows `yyyy-MM-dd'T'HH:mm:ssXXX`
(lenient). Errors are Bazel's:
`Failed to get credentials for 'URI' from helper 'PATH': ` followed by
`process exited with code N. stderr: …`, `process exited without output.
stderr: …`, `error parsing output. stderr: …` or `process timed out`.
The helper runs in its own process group; at the timeout the group is
killed, and a descendant that keeps stdout/stderr open past the deadline
(after the helper exited) is also a timeout: the group is killed and the
pipe readers are detached after a short grace period, so a stray process
cannot hang the invocation (Bazel would block on such a pipe).
`--credential_helper_timeout=0` means no timeout.

**Cache** (`CredentialHelperCredentials.java:61-79`,
`CredentialCacheExpiry.java`): responses are cached per URI string until
`expires`, else for `--credential_helper_cache_duration` (`0` disables
caching). A native process is one invocation, so the cache lives for one
command, as with `bazel --batch`; a Bazel server keeps it across commands
until the duration changes or `clean` runs.

**Consumers.**

* The HTTP remote cache (below): failures fail the action.
* The downloader (`bazel/repository/downloader/HttpConnectorMultiplexer.java:133-151`,
  `DownloadManager.java:344, 472`): registry files (`--registry=http(s)://…`),
  registry archives, overlays and patches, and repository/module-extension
  downloads. The helper is asked per URL; a failure is the warning
  `Error retrieving auth headers, continuing without: …` and the request goes
  ahead without credentials. Repository/extension downloads ask again for each
  redirect hop, like Bazel; registry and archive fetches attach the headers
  of the requested URL and reqwest keeps them for same-host redirects only
  (divergence). Non-empty `auth`/`headers` arguments of downloads remain
  explicit failures, and `.netrc` files are not read by the downloader
  (pre-existing divergence).

## The HTTP remote cache

Options (`remote/options/RemoteOptions.java`), accepted by `build`, `query`
and `mod`:

| option | native |
| --- | --- |
| `--remote_cache=http(s)://[user@]host[:port][/path]` (old `--remote_http_cache`) | HTTP cache |
| `--remote_cache=grpc://…`, `grpcs://…`, `unix:…`, no scheme (TLS) | gRPC cache (below) |
| `--remote_executor`, `--remote_exec_header`, `--remote_default_exec_properties`, `--remote_local_fallback` | remote execution (docs/REMOTE_EXECUTION.md) |
| `--[no]remote_accept_cached`, `--[no]remote_upload_local_results` | modelled |
| `--remote_timeout` (default 60s; unitless means seconds) | connect and read timeout |
| `--remote_header`, `--remote_cache_header` (`NAME=VALUE`) | sent with every request |
| `--remote_instance_name` | gRPC requests and resource names; HTTP caches ignore it, like Bazel |
| `--remote_retries` (old `--experimental_remote_retry_max_attempts`, default 5), `--remote_retry_max_delay` (5s) | gRPC retries (HTTP requests are not retried) |
| `--remote_max_connections` (100; 0 unlimited), `--remote_max_concurrency_per_connection` (100) | gRPC connection pool |
| `--remote_download_outputs=all\|minimal\|toplevel` (default `toplevel`), `--remote_download_{all,minimal,toplevel}`, `--remote_download_regex`, `--experimental_remote_cache_ttl` (3h), `--experimental_remote_cache_eviction_retries` (5) | Build without the Bytes (below) |
| `--[no]remote_cache_compression`, `--experimental_remote_cache_compression_threshold` (100) | gRPC zstd ByteStream (below); HTTP caches ignore it, like Bazel |
| `--tls_certificate`, `--tls_client_certificate`, `--tls_client_key`, `--tls_authority_override` | grpcs:// (and the root/client certificate for https://) |
| `--[no]incompatible_strict_action_env` (build) | both values, N42's action environment (docs/ACTION_ENV.md) |
| anything else (`--disk_cache`, `--remote_local_fallback`, `--remote_proxy`…) | unmodelled option error |

**Setup** (`RemoteModule.java:429-640, 1458-1490`). An absent port becomes the
scheme's default, so credentials are requested for `https://host:443`
(`http/HttpCacheClient.java:240-253`). For an `http://` cache Bazel asks for
credentials of the cache URI (as written) at startup and warns
`Credentials are transmitted in plaintext to URI. Please consider using an
HTTPS endpoint.`; a helper failure there is `ERROR: …` plus
`Error initializing RemoteModule`. The native profile does the same for every
command. Without a matching helper, Bazel falls back to `$NETRC` or
`~/.netrc`; when such a file exists the native profile fails explicitly
(`.netrc credentials are not implemented`).

**Protocol** (`http/HttpCacheClient.java`, `AbstractHttpHandler.java`):
`GET`/`PUT` `<path>/ac/<sha256>` and `<path>/cas/<sha256>`; `404` and `204`
are misses; `200`, `201`, `202`, `204` complete uploads; URI user info is
`Authorization: Basic <base64url>` instead of helper headers; credential
headers come before `--remote_header`/`--remote_cache_header` headers. A
`401`, or a `WWW-Authenticate: Bearer … error="invalid_token"` challenge,
clears the helper cache and retries once (`authTokenExpired`,
`refreshCredentials`). Other statuses are errors with Bazel's
`<code> <reason>\n<body>` message.

**The action key** (`RemoteExecutionService.java:269-332, 512-591`,
`util/Utils.java:430-453`, `UploadManifest.java`), protobuf-encoded in
canonical form (`src/reapi.rs`):

* `Command`: `arguments` (the spawn argv), `environment_variables` sorted by
  name, `output_paths` sorted (REAPI 2.1, which the HTTP cache implies); no
  platform (no execution properties are modelled) and no working directory.
* The input root: a Merkle tree of `Directory` messages over the spawn's
  inputs at their execution paths, entries sorted by name, files before
  directories, **every file `is_executable`** (Bazel marks all inputs
  executable).
* `Action`: `command_digest`, `input_root_digest`, `salt` = `CacheSalt
  {may_be_executed_remotely}` (`cache_salt.proto`); no timeout for build
  actions.

The key only matches when the native spawn equals Bazel's, so N41 also aligns
the spawns (the paired capture confirms every point):

* genrule: `["/bin/bash", "-c", "source external/bazel_tools/tools/genrule/genrule-setup.sh; CMD"]`
  (`rules/genrule/GenRuleBase.java:141-147`, `analysis/BashCommandConstructor.java`),
  environment the configuration's action environment (N42,
  `ActionEnvironment::shell`: `PATH=/bin:/usr/bin:/usr/local/bin` under the
  default strict action environment, `bazel/rules/BazelRuleClassProvider.java:139-202,
  347-361`, plus `--action_env`; `--host_action_env` for exec configurations),
  and `@bazel_tools//tools/genrule:genrule-setup.sh` as an input (its bytes
  are compiled in, `resources/genrule-setup.sh`, checked against the embedded
  tools manifest; stored as an executable `cas/` blob and staged through
  N36's store like every input). Genrules therefore now run with Bazel's `set -euo
  pipefail` from that script instead of native `-e -o pipefail` flags.
  Commands longer than 64000 characters (Bazel runs them from a generated
  script file) fail explicitly.
* `ctx.actions.run_shell`: `["/bin/bash", "-c", CMD]`, plus `""` and the
  arguments only when `arguments` is a non-empty list
  (`StarlarkActionFactory.java:627-631`); before N41 the native `$0` was
  always empty.
* `ctx.actions.run` with a File executable: argv[0] is its execution path
  (formerly `./PATH`); the process is started from the execution root with
  that argv[0].
* The environment of `run`/`run_shell` is N42's
  SpawnAction.createActionEnvironment (exactly `env` by default, the
  configuration's shell environment with `use_default_shell_env`).
  `Command.environment_variables` and the executed environment are one map,
  `crate::action::resolved_environment`: the fixed values, then the client
  values of the inherited names (`--action_env=NAME`, resolved once per
  build), a declared `TMPDIR` included; the executor applies it and then
  replaces `TMPDIR` last with its fresh directory, like Bazel's local spawns
  (PosixLocalEnvProvider). No `PATH`, `LANG` or `LC_ALL` is added. A bare
  executable name without `PATH` resolves with glibc's default search path.
  `tests/remote_cache.rs` (`executed_environment_equals_the_keyed_environment`,
  `executed_environment_equals_the_resolved_environment_with_inherited_values`)
  compares the exported environment of genrules, `run` and `run_shell` with
  the uploaded Command (equal apart from TMPDIR). With N42's environment
  framing and the k8 roots this is the native cache format
  `native-action-v8` (keys and manifests; docs/ACTION_ENV.md records the
  rekey; the keyed tests were re-pinned once). Since fix-digest-memo the
  local manifests are `native-action-v9`; remote keys are unaffected
  (docs/DIGEST_MEMO.md, "Upgrade note"). Since snapshot-keys the local key is
  `native-action-v10` (owner-free, docs/ACTION_ENV.md "Owner-free keys"); the
  remote keys are again unchanged.
* Parameter files (`args.use_param_file`) are files of the input root at
  their `<output>-N.params` execution paths (Bazel's ParamFileActionInput);
  tree artifact inputs contribute their files (no empty subdirectories), and
  so do source directory inputs, links followed
  (MerkleTreeComputer.explodeDirectory; a dangling link keeps the spawn
  local where Bazel fails it; docs/SOURCE_DIRECTORIES.md,
  `tests/remote_cache.rs::source_directory_inputs_enter_the_remote_input_root`
  pins genuine's key).
* Tree artifact outputs need `ActionResult.output_directories`, which is not
  modelled: such spawns stay local (`WARNING: Remote Cache: not used for …
  output_directories are not modelled`). Cached entries with
  `output_directories`, or `output_symlinks` for a declared file output, are
  rejected. Symlink outputs (`ctx.actions.symlink`) are native transforms,
  never spawns, so they never use the remote cache.
* Unresolved symlink artifacts (`declare_symlink`): inputs are input-root
  `SymlinkNode`s with their link text, a spawn's symlink outputs are
  `output_file_symlinks`/`output_symlinks` entries restored as symlinks on a hit
  (docs/UNRESOLVED_SYMLINKS.md;
  `tests/remote_cache.rs::unresolved_symlinks_enter_the_remote_cache` pins
  genuine's keys).
* Execution properties (a target's `exec_properties`, an action's combined
  exec-group properties (N35), an execution platform with
  `exec_properties`/`remote_execution_properties`, and
  `--remote_default_exec_properties`) enter the key as the Command's and
  the Action's `Platform` (PlatformUtils.getPlatformProto,
  docs/REMOTE_EXECUTION.md). An execution platform that analysis did not
  resolve counts as unknown: such spawns never use the remote cache
  (`WARNING: Remote Cache: not used for …`). A spawn's `timeout`
  requirement is the Action's `timeout`.
* Output roots are Bazel's `k8-<mode>` (D1 closed, below).
* Execution-requirement tags (`packages/TargetUtils.java:53-63, 241-301`):
  `no-cache`, `no-remote-cache`, `no-remote` and `local` (also genrule
  `local = True`) bypass the remote cache; `local`, `no-remote` and
  `no-remote-exec` clear `may_be_executed_remotely` (`actions/Spawns.java:29-56`).

Native write actions (`ctx.actions.write`, Args parameter files) are not
spawns and never use the remote cache, like Bazel's `FileWriteAction`.

**Lookup and upload** (`RemoteSpawnCache.java:85-305`). The native action cache
is consulted first; a native hit makes no remote request. Otherwise, with
`--remote_accept_cached`, the AC entry is fetched; a failed cached action, a
missing declared output or a missing CAS blob is a miss. Every declared output
that is to be downloaded (Build without the Bytes, below) is downloaded and
verified (`--remote_verify_downloads`; inlined
`contents`/`stdout_raw`/`stderr_raw` are verified against their digests too),
stdout/stderr are
replayed, and the outputs are written into the execution root and published
into the native action cache through the content-addressed store
(docs/HARDLINK_STAGING.md), exactly like executed outputs. On a miss the action runs locally and, with
`--remote_upload_local_results` **and the native opt-in below**, the Action and Command protos, the outputs and
non-empty stdout/stderr are uploaded (empty blobs never are; each blob once per
invocation), then the `ActionResult` (`output_files` with `is_executable`,
`stdout_digest`/`stderr_digest`, worker/execution timestamps). Cache errors
are `WARNING: Remote Cache: …` and the action runs (or stays) local;
credential helper errors during a lookup fail the action.

The build summary adds `N remote cache hits` when a remote cache is active.

**Uploads follow `--remote_upload_local_results`, like Bazel.** Local
results are written to the remote cache by default (Bazel's default is true);
`--noremote_upload_local_results` keeps the cache read-only for the client.
Reads (`--remote_accept_cached`) are unaffected. Earlier bazel-rs releases
(up to 0.4.1) only uploaded with `BAZEL_RS_ALLOW_REMOTE_UPLOAD=1`; that
variable is no longer read. A binary before fix-digest-memo could publish
another action's output as an action's result when uploads were enabled:
purge the Action Cache entries such a binary uploaded (docs/DIGEST_MEMO.md,
"Upgrade note").

## The gRPC remote cache

`--remote_cache` values that are not `http://`/`https://` are gRPC
(`GrpcCacheClient.isRemoteCacheOptions`). Implementation:
`src/grpc_channel.rs` (gRPC over HTTP/2: `h2` and the tokio runtime and
rustls that reqwest already brings; protobuf hand-encoded with
`src/reapi.rs`'s primitives; no tonic/prost/protoc, so the only new crates
are `h2` 0.4.20 and `tokio-util` 0.7.20, both MIT) and `src/remote_grpc.rs`
(the REAPI client). Pinned source: `remote/GrpcCacheClient.java`,
`ByteStreamUploader.java`, `RemoteServerCapabilities.java`,
`GoogleChannelConnectionFactory.java`, `RemoteRetrier.java`,
`util/TracingMetadataUtils.java`, `authandtls/GoogleAuthUtils.java`.

**Targets** (`GoogleAuthUtils.newChannel`): `grpc://HOST[:PORT]` is
plaintext, `grpcs://HOST[:PORT]` and `HOST[:PORT]` use TLS (ALPN h2; the
default port is 443 with TLS, 80 without), `unix:PATH` is a plaintext Unix
domain socket. `--tls_certificate` replaces the system roots;
`--tls_client_certificate` with `--tls_client_key` enables mutual TLS.
Other forms (`dns:`, a path after the authority) fail at startup.

**Capabilities.** The first cache use calls `GetCapabilities` (with
`--remote_instance_name`) and applies
`checkClientServerCompatibility` for a cache: the server's API range must
meet Bazel's 2.0–2.11 (else `The client supported API versions, 2.0 to
2.11, is not supported by the server, …`), `SHA256` must be a cache digest
function (`Cannot use hash function SHA256 with remote cache. Server
supported functions are: [...]`), and `--remote_upload_local_results`
without `update_enabled` warns. Errors before the last print as `ERROR:`;
the last, or a failed call, fails every action that consults the cache
with `Failed to query remote execution capabilities: CAUSE` (Bazel's
`RemoteSpawnCache` without `--remote_local_fallback`), CAUSE being the
transport error (`finishConnect(..) failed: Connection refused:
/127.0.0.1:PORT`, as grpc-java prints it), the credential helper error, or
`CODE: description`. The highest common API version selects the key:
`Command.output_paths` from 2.1 on, REAPI 2.0's `output_files` before
(`RemoteExecutionService.initUseOutputPaths`). Like Bazel
(`RemoteModule.createChannel`: `channel.withChannelFuture` submitted at
setup) the check starts in the background when the cache is set up, for
every command that sets one up: its warnings print when the server
answers, the first spawn that needs the endpoint waits for it, and the
command never waits for it otherwise. A spawn failing because of it exits
the command with code 34 (ExitCode.REMOTE_ERROR,
`CAPABILITIES_QUERY_FAILURE`).

**Calls** (`GrpcCacheClient`, `ByteStreamUploader`): lookups are
`ActionCache.GetActionResult` (`digest_function` SHA256, no inlining;
NOT_FOUND is a miss) and a `ByteStream.Read` of
`[INSTANCE/]blobs/HASH/SIZE` per output, stdout and stderr (a server
sending more than the digest's size fails the download; an interrupted read
resumes at `read_offset` = the bytes received, with ProgressiveBackoff's
immediate retry after progress; NOT_FOUND is a miss). Uploads are one
`ContentAddressableStorage.FindMissingBlobs` per action over the Action,
Command, outputs, stdout and stderr (empty digests included, batches of at
most 1 MiB), a `ByteStream.Write` of
`[INSTANCE/]uploads/UUID/blobs/HASH/SIZE` in 16 KiB requests per missing
non-empty blob not yet uploaded in this invocation (concurrent uploads of
one blob run once; a server completing early or ALREADY_EXISTS means the
blob exists; `committed_size` must equal the size; a retry resumes at
`QueryWriteStatus`'s committed size, or from the start when it is
UNIMPLEMENTED), then `ActionCache.UpdateActionResult`. Like Bazel 9.3 the
cache never uses `BatchReadBlobs`/`BatchUpdateBlobs`. Every call carries
`--remote_header`/`--remote_cache_header` (lowercased names), the binary
`RequestMetadata` header (tool `bazel-rs`, action id = the action key, a
command id and build request id per invocation) and the credential
helper's headers, and has `--remote_timeout` as its deadline
(`RemoteGrpcServiceConfig`; also the connect timeout).

**Compression** (`--remote_cache_compression`, GrpcCacheClient
.shouldCompress/getResourceName, ByteStreamUploader, Chunker,
RemoteServerCapabilities): the capability check requires `ZSTD` among
the server's `supported_compressors`, else its error `--remote_cache_compression
requested but remote does not support compression` fails every action
that uses the cache (exit 34). Blobs of at least
`--experimental_remote_cache_compression_threshold` bytes (default 100)
are read from `[INSTANCE/]compressed-blobs/zstd/HASH/SIZE` (a zstd stream,
decoded and verified against the digest; skippable frames, which
bazel-remote stores in front of its blobs, are skipped; an interrupted
read resumes at the uncompressed bytes received, with a fresh stream) and
written to `[INSTANCE/]uploads/UUID/compressed-blobs/zstd/HASH/SIZE` (one
zstd frame, `write_offset`s of the compressed bytes; a resumed upload
compresses the rest from the committed uncompressed offset like
Chunker.seek; `committed_size` must be the stream's end or -1). Keys and
FindMissingBlobs are unchanged. Paired with genuine 9.3.0 and
bazel-remote 2.x (GitHub release binary, loopback, threshold 1): genuine
writes compressed entries and native reads them (2 remote cache hits);
native writes them (`compressed-blobs/zstd/` writes in bazel-remote's
log) and genuine reads them (`2 remote cache hit`).
`tests/remote_grpc_cache.rs::zstd_compression_follows_the_server_capabilities`
pins resource names, the threshold and the capability error against the
in-process server.

**Retries** (`RemoteRetrier`): CANCELLED, UNKNOWN, DEADLINE_EXCEEDED,
ABORTED, INTERNAL, UNAVAILABLE and RESOURCE_EXHAUSTED are retried up to
`--remote_retries` times with exponential backoff (100 ms doubling up to
`--remote_retry_max_delay`, jitter 0.1); connection failures are
`UNAVAILABLE: io exception`. An UNAUTHENTICATED call refreshes the
credential helper cache and runs once more (`refreshIfUnauthenticated`).
Other failures are `WARNING: Remote Cache: CODE: description` and the
action runs locally, as for HTTP.

**Credentials.** The helper is asked for grpc-java's per-service URI
`https://AUTHORITY/SERVICE` (`:443` omitted), e.g.
`https://127.0.0.1:8980/build.bazel.remote.execution.v2.Capabilities`, so
one cache uses four URIs (Capabilities, ActionCache,
ContentAddressableStorage, google.bytestream.ByteStream); a failure inside
a call is `UNAUTHENTICATED: Failed computing credential metadata: …`. Unlike
`http://`, `grpc://` prints no plaintext-credentials warning (Bazel checks
the `http://` prefix only); a helper path error is still `Error
initializing RemoteModule`. Without a matching helper and with a `.netrc`
file the cache fails explicitly, as for HTTP. Responses are single-flight
per URI (Caffeine's `Cache.get(key, loader)`), for HTTP caches and the
downloader too.

**Remote execution** (docs/REMOTE_EXECUTION.md) uses the same client:
`Execution.Execute`/`WaitExecution` are server-streaming `Operation` calls
(`grpc_channel::Channel::server_streaming`), and one endpoint serves both
when `--remote_executor` and `--remote_cache` are equal.

## Build without the Bytes

`--remote_download_outputs` (crate::remote_outputs; RemoteOutputChecker,
RemoteExecutionService.downloadOutputs, RemoteActionInputFetcher,
ActionCacheChecker in the pinned source) decides which regular-file
outputs of remote cache hits and remote executions are downloaded:

* `all`: every output (the behaviour before this change).
* `toplevel` (Bazel's default): the requested targets' reported artifacts
  (in `test` not those of the test targets themselves), aspect reports, and
  in `run` the target's executable, runfiles tree and runfiles; plus
  `--remote_download_regex` matches (whole exec paths). Everything else stays
  in the remote CAS.
* `minimal`: only `--remote_download_regex` matches (and, in `run`, what
  `toplevel` downloads, like Bazel).

Tree artifacts, unresolved symlinks, stdout/stderr, empty files and the
outputs of failed results are always downloaded. A remote-only output is
recorded in its action's native cache entry as metadata only (manifest
`remote`: digest, size, expiration = now + `--experimental_remote_cache_ttl`),
with no file; the process-wide registry gives consumers its digest without
reading it, so they key, hit (native or remote) and execute remotely without
the bytes, and the output tree (`bazel-bin`) has nothing at its path (an
earlier build's link is removed), as in Bazel. A consumer that runs locally
(a local spawn, a local test, a native transform: symlink, template,
runfiles tree) first **materializes** its remote-only inputs: the blob is
downloaded, verified, stored and linked at its published path, where it
then is an ordinary output for this and later commands.

**Later commands.** A cache entry whose remote-only output has no file is
valid while the output may stay remote under the command's policy and its
metadata has not expired; otherwise the action reruns (a remote cache hit),
which downloads it — switching `minimal` to `toplevel` downloads the newly
requested outputs, and an expired lease is rechecked. Bazel 9.3 records the
same expiration but trusts expired metadata optimistically (relying on
rewinding); the native check is the conservative reading of
`--experimental_remote_cache_ttl`. Without a remote cache such entries
always rerun.

**Lost blobs.** A remote cache hit whose remote-only outputs would stay
remote is checked with one FindMissingBlobs first (a missing blob makes the
result unusable: a miss, or execution without the cached result) — a
native addition (Bazel trusts the result and recovers later), so that a
consumer is never handed a lost input knowingly. A blob lost later (a
materialization finds NOT_FOUND, or remote execution needs a remote-only
input the CAS lost) invalidates the producer's cache entry and fails the
consumer with `Lost inputs no longer available remotely: PATH (DIGEST)`; the
command then runs again in a fresh process (`Found transient remote cache
error, retrying the build...`), at most
`--experimental_remote_cache_eviction_retries` times, else exits 34.
An HTTP cache cannot tell whether a blob is present: its hits download
everything (like `all`).

**Measurement** (release binary, NativeLink 1.8.2 local CAS+executor on
loopback, a 691-genrule scratch build of ~150 kB outputs (10 layers),
`--jobs=16`, fresh native cache each time, `BAZEL_RS_REMOTE_STATS=1`
counting ByteStream reads):

| run | wall | downloaded |
|---|---|---|
| cold (691 executed remotely), `all` | 8.9 s | 691 blobs, 103.5 MB |
| cold, `minimal` | 9.9 s | 0 |
| warm remote (691 remote cache hits), `all` | 0.55 s | 691 blobs, 103.5 MB |
| warm remote, `minimal` | 0.40 s | 0 |
| warm remote, `toplevel` | 0.42 s | 1 blob, 15 B |

On loopback the transfer is nearly free (the cold builds are dominated by
the executor; their difference is noise); the saving is the 103.5 MB that
never cross a real network nor land in the local cache.

Evidence: `tests/remote_download.rs` (in-process executor): minimal builds
download nothing and leave `bazel-bin` empty; a second build is a native
hit with no remote call; `toplevel` afterwards reruns only the requested
action and downloads its output; a local consumer materializes its input;
`--remote_download_regex`, `all`, an expired TTL; `run` and a local test of
remote-only binaries/runfiles; a remote test; an evicted blob retried
(producer re-executed) and, with no retries, exit 34. Paired with genuine
9.3.0 (`genuine_bazel_shares_grpc_entries_both_ways`): genuine and native
now both read 9 blobs from a fresh native cache (native adds one
FindMissingBlobs); `genuine_bazel_sends_identical_execute_requests`:
genuine 12 Reads, native 13 (was 14).

## D1: output roots

Before N41, native target configurations wrote `bazel-out/rust-<mode>/bin`
where Bazel writes `bazel-out/k8-<mode>/bin` (divergence D1,
docs/CONFIGURATION.md). Output paths are part of the key twice
(`Command.output_paths` and the input paths of every consumer), and a
genrule's expanded command embeds them, so with D1 no action producing or
consuming a generated file could ever share an entry. N41 closes D1:
`NATIVE_CPU` is `k8` (`CoreOptions.java:279-282`, `AutoCpuConverter.java:52`).
This rekeyed the native action cache once and changed the paths that
`$(location)`, `File.path` and the CLI report. Exec configurations of the
default host platform are `k8-opt-exec` on both sides (the capture's genrule
tool `//pkg:mk` shares its entry); exec configurations whose Bazel name
carries an `ST-` hash cannot match, because Bazel hashes options the native
profile does not model (docs/CONFIGURATION.md). Closing D1 was a one-line
change (`config_registry::NATIVE_CPU`) plus test expectations: no Rust code
depended on the `rust` spelling, and validation scripts already accepted
both roots. Tracked receipts of earlier captures keep their `rust-*` paths.

## Remaining divergences

* HTTP caches are not retried (`--remote_retries`, default 5 in Bazel): a
  failing request is one warning and local execution. gRPC calls are.
* Build without the Bytes: expired metadata reruns its action, a remote
  cache hit with remote-only outputs costs one FindMissingBlobs, tree
  artifacts are always downloaded, a native transform (runfiles tree,
  symlink, template) materializes its inputs (Bazel's in-memory output
  filesystem does not need to), and HTTP cache hits download everything
  (above). Build event files name remote-only outputs by
  `bytestream://AUTHORITY/[INSTANCE/]blobs/HASH/SIZE` (other files keep
  `file://`; Bazel converts every file with a remote cache).
  The execution log lists no `actual_outputs` entry for a remote-only
  output of a remote hit.
* gRPC metadata: native sends no `grpc-accept-encoding` (it accepts no
  compressed messages), `user-agent: bazel-rs/<version> grpc-h2` and a
  millisecond `grpc-timeout`; RequestMetadata names the tool `bazel-rs` and
  carries no mnemonic, target or configuration id.
* Request headers: the native `User-Agent` is `bazel-rs/<version>`, and
  `Accept-Encoding` is `gzip,deflate` (decoded natively; Bazel also offers
  snappy and zstd). Header names are lowercase on the wire (HTTP/1.1 header
  names are case-insensitive).
* The credential cache lives for one native invocation.
* Gson leniency in helper output (comments, single quotes, unquoted names) is
  not reproduced; such output is `error parsing output`.
* The disk cache, the remote downloader,
  remote output services, `.netrc` for the cache and
  helper scopes needing Punycode remain explicit failures.

## Evidence

* Unit tests: `src/credentials.rs`, `src/reapi.rs` (Command, input root and
  Action digests equal genuine Bazel's blobs byte for byte),
  `src/remote_cache.rs`, `src/builtin_tools.rs` (genrule-setup.sh identity).
* Integration tests: `tests/remote_cache.rs` (loopback HTTP cache and
  registry, local helper scripts): the fixture's three AC keys equal the keys
  genuine Bazel 9.3.0 wrote, round trips, flags, tags, failures, helper
  errors and caching, registry downloads.
* gRPC: unit tests in `src/grpc_channel.rs` and `src/remote_grpc.rs`;
  `tests/remote_grpc_cache.rs` against the in-process REAPI server
  `tests/support/grpc_cache_server.rs` (in-memory CAS/AC, request log,
  failure injection, TLS/mTLS, Unix sockets): genuine keys, call counts,
  headers and instance names, per-service helper URIs, 3 MB blobs with
  interrupted reads/writes resumed (QueryWriteStatus or a restart), retries,
  capability errors/warnings and the REAPI 2.0 key, helper failures, TLS.
  Paired check: the ignored test
  `genuine_bazel_shares_grpc_entries_both_ways`
  (`BAZEL_RS_GENUINE_BAZEL=<ELF> [BAZEL_RS_BAZEL_REMOTE=<bazel-remote>]
  [BAZEL_RS_GRPC_RECEIPT=<out>] cargo test --test remote_grpc_cache --
  --ignored`); tracked receipt `tests/fixtures/remote_cache/grpc_receipt.json`.
  With the in-process server and with bazel-remote 2.6.4 (GitHub release
  binary, loopback): genuine writes 10 AC entries and native reads all 10
  (10 remote hits), native writes the same 10 keys and genuine reads all
  10 (`10 remote cache hit`) with identical outputs; the cold builds'
  request multisets (GetActionResult, FindMissingBlobs, Write resources,
  UpdateActionResult) are identical.
* Paired capture: `artifacts/remote_cache-capture.py`; tracked receipt
  `tests/fixtures/remote_cache/reference_receipt.json` (no test reads it).
  Both engines use one loopback HTTP cache (127.0.0.1) or registry
  (127.0.0.2) per case and local helper scripts; genuine Bazel runs with
  `--batch --ignore_all_rc_files`, a private output user root and HOME, and
  the offline file registry `tests/fixtures/builtin_modules/registry`; native
  runs with `--experimental-builtin-modules` and the same registry (genrule
  tools need the default host platform; native runs set the upload opt-in).
  Run 2 plus the review's targeted rerun of the 8 cross-engine rows (after
  the exact-environment fix): 15 EXACT, 17 EXACT_MESSAGE, 4
  EXPECTED_DIVERGENCE, 2 EXPECTED_NATIVE_UNSUPPORTED, 4 MISMATCH; the
  receipt's `analysis` explains every non-exact row:
  * **Sharing**: a fixture with a genrule, a genrule consuming a generated
    file, a genrule with an exec-configured tool, a Starlark `run` and
    `run_shell`, an environment probe and `no-remote-exec`/`no-remote-cache`
    tags. Native reads all 8 entries genuine Bazel wrote (8 remote hits, the
    `no-remote-cache` genrule runs locally) with identical outputs; genuine
    Bazel reads all 8 entries native wrote (`8 remote cache hit`). The AC key
    sets, the Action and Command protos, `output_files`, stdout/stderr digests
    and the uploaded CAS blob set are identical (rerun: EXACT; run 2 differed
    only in timing-dependent execution metadata of a sub-millisecond native
    action); a cold build's request multiset is identical.
  * **Credential helpers**: the startup invocation (argv `get`, workspace
    cwd, `{"uri":"http://127.0.0.1:PORT"}`, client environment), request
    headers, `expires` in the past/future, `*.0.0.1` wildcard matching
    `127.0.0.1`, unmatched scopes, and all 16 error texts (helper exit code,
    bad/empty/wrong-type output, bad `expires`, timeout, path and scope
    errors, durations, `--remote_header`) are exact; a mid-build helper
    failure has Bazel's `Exec failed due to CredentialHelperException: …`
    text inside the native action-failure line.
  * **Flags**: `--noremote_upload_local_results`, `--noremote_accept_cached`,
    both, `--remote_header`/`--remote_cache_header`/`--remote_instance_name`
    /`--remote_timeout`, and a cache URI path prefix give identical requests.
  * **Downloader**: registry fetches ask the helper once per URL with
    identical URIs and attach the helper header (or warn identically when it
    fails), compared on the URLs both engines request.
  * **Divergences** (documented above): no retries on HTTP 500, helper
    invocation counts under concurrent uploads, all outputs downloaded on
    hits (before Build without the Bytes), User-Agent/Accept-Encoding,
    exit codes (34 since).
  * Native-only probe: a scratch copy of `~/code/.bazelrc` (never run in
    `~/code`) now stops at `--attempt_to_print_relative_paths=true`
    (`.bazelrc:6`).
  * Network: no storagebox, BuildBarn, proxy or real helper was contacted.
    Builds that needed `@platforms` fetched the genuine platforms-1.0.0
    archive from github.com through the registry fixture's `source.json`
    (both engines); with an unscoped helper genuine Bazel also sent the test
    helper's dummy header there.
