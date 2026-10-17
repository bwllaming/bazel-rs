# Credential helpers and the HTTP remote cache (N41)

`bazel-rs` implements Bazel 9.3.0's credential helpers and its HTTP remote
cache. Action keys are computed exactly as Bazel computes them, so a native
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
| `--remote_cache=grpc://…`, `grpcs://…`, `unix:…`, no scheme | explicit failure (gRPC) |
| `--remote_executor` | explicit failure (remote execution) |
| `--[no]remote_accept_cached`, `--[no]remote_upload_local_results` | modelled |
| `--remote_timeout` (default 60s; unitless means seconds) | connect and read timeout |
| `--remote_header`, `--remote_cache_header` (`NAME=VALUE`) | sent with every request |
| `--remote_instance_name` | accepted; HTTP caches ignore it, like Bazel |
| `--[no]incompatible_strict_action_env` (build) | both values, N42's action environment (docs/ACTION_ENV.md) |
| anything else (`--disk_cache`, `--remote_retries`, `--remote_download_outputs`, TLS options…) | unmodelled option error |

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
  rekey; the keyed tests were re-pinned once).
* Parameter files (`args.use_param_file`) are files of the input root at
  their `<output>-N.params` execution paths (Bazel's ParamFileActionInput);
  tree artifact inputs contribute their files (no empty subdirectories).
* Tree artifact outputs need `ActionResult.output_directories`, which is not
  modelled: such spawns stay local (`WARNING: Remote Cache: not used for …
  output_directories are not modelled`). Cached entries with
  `output_directories` or `output_symlinks` for a declared output are
  rejected. Symlink outputs (`ctx.actions.symlink`) are native transforms,
  never spawns, so they never use the remote cache.
* Execution properties (a target's `exec_properties`, an action's combined
  exec-group properties (N35), or an execution platform with
  `exec_properties`/`remote_execution_properties`) enter
  Bazel's key as a `Platform` message, which is not modelled: such spawns
  never use the remote cache (`WARNING: Remote Cache: not used for …`). A
  non-default `--host_platform` that analysis did not resolve counts as
  unknown and is gated the same way.
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
is downloaded and verified (`--remote_verify_downloads`; inlined
`contents`/`stdout_raw`/`stderr_raw` are verified against their digests too),
stdout/stderr are
replayed, and the outputs are written into the execution root and published
into the native action cache through the content-addressed store
(docs/HARDLINK_STAGING.md), exactly like executed outputs. Bazel's default
`--remote_download_outputs=toplevel` skips downloading intermediate outputs;
the native profile always materializes them (like `=all`), so its request log
has more CAS `GET`s on hits. On a miss the action runs locally and, with
`--remote_upload_local_results` **and the native opt-in below**, the Action and Command protos, the outputs and
non-empty stdout/stderr are uploaded (empty blobs never are; each blob once per
invocation), then the `ActionResult` (`output_files` with `is_executable`,
`stdout_digest`/`stderr_digest`, worker/execution timestamps). Cache errors
are `WARNING: Remote Cache: …` and the action runs (or stays) local;
credential helper errors during a lookup fail the action.

The build summary adds `N remote cache hits` when a remote cache is active.

**Uploads are denied by default (intentional safety divergence).** Until
native execution parity with Bazel is established, a native build writes to
a shared cache only when `BAZEL_RS_ALLOW_REMOTE_UPLOAD=1` is set in its
environment. Without it, `--remote_upload_local_results` (Bazel's default
true) is treated as false and the build warns once:
`native remote cache uploads are disabled by default (…); set
BAZEL_RS_ALLOW_REMOTE_UPLOAD=1 to upload local results`. Reads
(`--remote_accept_cached`) are unaffected.

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

* No retries (`--remote_retries`, default 5 in Bazel): a failing request is
  one warning and local execution.
* Request headers: the native `User-Agent` is `bazel-rs/<version>`, and
  `Accept-Encoding` is `gzip,deflate` (decoded natively; Bazel also offers
  snappy and zstd). Header names are lowercase on the wire (HTTP/1.1 header
  names are case-insensitive).
* The credential cache lives for one native invocation.
* Gson leniency in helper output (comments, single quotes, unquoted names) is
  not reproduced; such output is `error parsing output`.
* gRPC caches, remote execution, the disk cache, the remote downloader,
  remote output services, build without the bytes, `.netrc` for the cache and
  helper scopes needing Punycode remain explicit failures.

## Evidence

* Unit tests: `src/credentials.rs`, `src/reapi.rs` (Command, input root and
  Action digests equal genuine Bazel's blobs byte for byte),
  `src/remote_cache.rs`, `src/builtin_tools.rs` (genrule-setup.sh identity).
* Integration tests: `tests/remote_cache.rs` (loopback HTTP cache and
  registry, local helper scripts): the fixture's three AC keys equal the keys
  genuine Bazel 9.3.0 wrote, round trips, flags, tags, failures, helper
  errors and caching, registry downloads.
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
    hits, User-Agent/Accept-Encoding, exit codes.
  * Native-only probe: a scratch copy of `~/code/.bazelrc` (never run in
    `~/code`) now stops at `--attempt_to_print_relative_paths=true`
    (`.bazelrc:6`).
  * Network: no storagebox, BuildBarn, proxy or real helper was contacted.
    Builds that needed `@platforms` fetched the genuine platforms-1.0.0
    archive from github.com through the registry fixture's `source.json`
    (both engines); with an unscoped helper genuine Bazel also sent the test
    helper's dummy header there.
