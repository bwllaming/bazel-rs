# Options of real rc files (N40)

With rc evaluation (N31, [BAZELRC.md](BAZELRC.md)), every option in an evaluated
rc file needs a native model with Bazel's semantics, or it fails explicitly.
N40 covers the options of `~/code`'s `.bazelrc` and `.bazelrc.coder`, except
`--remote_cache` and `--credential_helper` (remote cache work, N41).

Implementation: `src/rc_options.rs` (option parsing), `src/repo_env.rs`,
`src/download_proxy.rs`, `src/location_printer.rs`, the Apple options in
`src/configuration.rs`, the `isolate` gate in `src/bzlmod.rs`, and the call sites
in `src/main.rs`. Pinned source: `/tmp/opencode/bazel-9.3-reference` (e89755b).
Paths are relative to `src/main/java/com/google/devtools/build/lib/`.

## Per option

| Option | Bazel 9.3 | Native |
| --- | --- | --- |
| `--incompatible_strict_action_env` (old name `experimental_…`) | `BazelRuleClassProvider.StrictActionEnvOptions` (:73-87). The default is **true**. Its only consumer is `SHELL_ACTION_ENV` (:140-200): `PATH=/bin:/usr/bin:/usr/local/bin` (`pathOrDefault`, :347-360). Exec configurations inherit it (`builtin_exec_platforms.bzl`). | Since N42 a configuration option of `build` (crate::configuration, docs/ACTION_ENV.md): both values are modelled, `false` inherits the client `PATH` and `LD_LIBRARY_PATH`; N40's explicit `=false` failure is gone. `query` gets Bazel's `Unrecognized option`. |
| `--test_output`, `--test_summary` | `ExecutionOptions` (:271-330), enums. Read only when tests run (`TestCommand` :84-100, `TestSummaryPrinter` :51-90). | ~/code sets them in the `test` group, which `build` and `query` never read. The `test` command fails at command level. On `build`, values are validated with Bazel's EnumConverter text and have no effect, because no tests run. `query` gets `Unrecognized option`. |
| `--attempt_to_print_relative_paths` (old name `experimental_ui_…`) | `UiOptions` (:221-229), a common option of every command. `LocationPrinter.getRelativeLocationString` (`runtime/LocationPrinter.java:43-80`) prints an event's location relative to the workspace or a package-path root (`UiEventHandler` :223-224, :475). | Boolean. When it is on, every printed error and warning line that starts with an absolute event location `PATH[:LINE[:COL]]: ` under the workspace has that location printed workspace-relative. The default `--package_path` is `%workspace%`. Paths under the native cache directory (Bazel's output base, which is never a package-path root) stay absolute. Native's span form `LINE:COL-COL` counts as a location. |
| `--repo_env` | `CommonCommandOptions` (:607-622), on every command. `EnvVarsConverter` (`common/options/Converters.java:627-644`) accepts `NAME=VALUE`, `NAME` (inherit) and `=NAME` (unset). `CommandEnvironment` (:342-416) applies the values in order over the client environment, with `%bazel_workspace%` replaced and Inherit taking only a client value. The result is `repoEnv`, used for `repository_ctx.os.environ`, `getenv` and `execute`, for `module_ctx`, and for the downloader. | All forms, with Bazel's errors (`While parsing option --repo_env=: Variable definitions must be…`, `Illegal use of 'no' prefix`). Values apply in priority order into a fixed per-command repository environment (`RepositorySet::with_repository_environment`). That environment feeds every repository rule and extension observation, so the existing observed-value identities re-run a repository rule when a watched value changes. Values are never printed: notes redact them (N31), and so does the `Unknown startup option` error. `--action_env` (N42) feeds actions only: `--incompatible_repo_env_ignores_action_env` is true. |
| proxies (`.bazelrc.coder`: `--repo_env=HTTPS_PROXY=…`, `--repo_env=NO_PROXY=…`) | Registry reads (`bazel/bzlmod/IndexRegistry.java:235`), archive sources and ctx downloads (`StarlarkBaseExternalContext.java:864-873`) pass `nonstrictRepoEnv` to the downloader. The downloader builds `ProxyHelper(env)` (`HttpDownloader.java:280-282`) and asks it on every connection, redirects included (`HttpConnector.java:125-136`). | `crate::download_proxy` ports `ProxyHelper.createProxyIfNeeded` and `createProxyInfo` (`bazel/repository/downloader/ProxyHelper.java:73-335`), applied to registry, archive, `module_ctx` and `repository_ctx` downloads and checked before every request and redirect. See the details below. |
| `--macos_sdk_version` (and `--ios_sdk_version`, `--tvos_sdk_version`, `--watchos_sdk_version`) | `AppleCommandLineOptions` (:59-100), `DottedVersionConverter`, `DottedVersion.Option`. Equality is by string representation (`DottedVersion.java:128-139`). `AppleConfiguration` (:104-107) exposes them as `*_sdk_version_flag`. The exec transition propagates them (`builtin_exec_platforms.bzl:170-176`). | Configuration options. `DottedVersion.fromString` validation gives Bazel's `Dotted version components must all start with the form …` text, and values are stored as given. `config_setting(values=…)` compares strings, so `10.13` ≠ `10.13.0`, as in Bazel. A null value is `null`, and `@null` in output-path hashes. The options are part of the identity only when set, and exec configurations copy them. Not `query` options. `ctx.fragments.apple`, a parallel branch, reads `NativeOptions::apple_sdk_versions`. A parallel `dotted_version` module may replace the local validator at integration. |
| `--noremote_upload_local_results` (`build:dev`) | `RemoteOptions` (:333-340), default true. Consulted only with a disk, HTTP or gRPC cache or remote execution. Otherwise the remote module is a placeholder (`remote/RemoteModule.java:526-531`). | N41's `crate::remote_cache::RemoteOptions` parses it (no longer `RcOptions`): with an http(s) `--remote_cache` it disables uploads; without a cache it has no effect, which is Bazel's behavior (docs/REMOTE_CACHE.md). |
| `--experimental_isolated_extension_usages` | `BuildLanguageOptions` (:278-286), default false. `use_extension(isolate=)` has `enableOnlyWithFlag` (`bazel/bzlmod/ModuleFileGlobals.java:436-449`). Without the flag, passing `isolate` at all fails while arguments are bound (`net/starlark/java/eval/BuiltinFunction.java:369-375`). | A Starlark-semantics flag for every MODULE.bazel evaluation (`ResolverOptions`, `EvaluationContext`; `mod` receives it too). Without it, `isolate=True` and `isolate=False` fail with Bazel's message. Before N40, native accepted `isolate` unconditionally. With the flag, a root-module isolated usage that imports no repository fails with Bazel's `the isolated usage at … has no effect as no repositories are imported from it. …` (`ModuleFileFunction.java:573-584`; on `query`/`build` after Bazel's `Error computing the main repository mapping: ` wrapper, `skyframe/RepositoryMappingValue.java:158-171`). Isolated usages themselves keep their existing bounded profile. |
| `--incompatible_enable_proto_toolchain_resolution` | `BuildLanguageOptions` (:644-652), default **true** (`FlagConstants.java:48`). Exposed as `proto_common_do_not_use.INCOMPATIBLE_ENABLE_PROTO_TOOLCHAIN_RESOLUTION` (`builtins_bzl/common/exports.bzl:23-24`). | `true` is accepted; native exports `True`. `=false` fails explicitly, because the legacy `--proto_compiler`/`--proto_toolchain_for_*` path is not modelled. |
| `--@com_google_protobuf//…:prefer_prebuilt_protoc`, `--@rules_python//…:bootstrap_impl=script` | Starlark options | N31: rc Starlark flags reach the Starlark option parser on `build`, and `query` skips them. On `~/code`, `build` loads them, which needs the module graph (next stop below). |
| `--platforms` (`build:linux_*`) | `PlatformOptions` | Already modelled. |
| `--sandbox_add_mount_pair` (`build:linux_*`) | `SandboxOptions` (:173-180). Bind mounts of linux-sandbox, which is part of Bazel's default Linux strategy, so the option is not a no-op there. | Fails explicitly with `sandboxed execution is not implemented`, naming the rc file:line. The value is not echoed. Only reached with `--config=linux_amd64`/`linux_arm64`. |
| `--remote_executor`, `--remote_default_exec_properties` (`build:buildbarn`) | `RemoteOptions` (:91, :601) | Fail explicitly with `remote execution is not implemented`. The value is not echoed. Only reached with `--config=buildbarn`. `--remote_instance_name` (:367) and `--[no]remote_upload_local_results` are N41's RemoteOptions (accepted; docs/REMOTE_CACHE.md). |
| `--java_runtime_version` | `JavaOptions` (:373) | main models it in N28b (8301936), which this branch's base predates. Here it stays an explicit unsupported option, and integration takes main's model. |

### Download proxies

* The environment is the client environment with `--repo_env` applied.
  `no_proxy` is used, or `NO_PROXY` when `no_proxy` is unset or empty. Entries
  are split with Java's `\s*,\s*` and trailing empty entries are dropped. A
  `.suffix` entry matches subdomains only. Any other entry matches the host
  itself or its subdomains. There is no `*` wildcard.
* The first non-null of `https_proxy`/`HTTPS_PROXY` serves `https` URLs, and
  `http_proxy`/`HTTP_PROXY` serves `http` URLs. An empty lowercase value
  selects no proxy.
* Addresses follow `URL_PATTERN` with the regex engine's priority, including
  its quirks: `ftp://u:p@h` is host `h` with user `ftp`, and `https://p/` keeps
  the slash in the host. Without a protocol the port defaults to 80, with
  `https://` to 443. An HTTP proxy is always spoken to in plain HTTP, also for
  `https://` addresses, as Java does: requests use absolute form and HTTPS goes
  through a CONNECT tunnel. Credentials are URL-decoded with Java's
  `URLDecoder` rules and sent as Basic `Proxy-Authorization`.
* Errors use Bazel's texts: `No password given for proxy …`,
  `Error parsing proxy port: …`, `port out of range:N`, and `Unknown host: H`
  for a host that cannot be a URL host. `Proxy address … is not a valid URL` is
  printed with `<redacted>` in place of the address, a deliberate difference,
  since the address may contain credentials.
* SOCKS addresses (`socks://`, `socks4://`, `socks5://`) fail explicitly,
  because the HTTP transport has no SOCKS support.
* Java system properties (`http.nonProxyHosts`, `https.proxyHost`) are unset,
  like a Bazel server started without `--host_jvm_args` for them.
* Git child processes, the native `git_override` and Git sources, get the
  repository environment's proxy variables (`http(s)_proxy`, `HTTP(S)_PROXY`,
  `all_proxy`, `ALL_PROXY`, `no_proxy`, `NO_PROXY`). Bazel's `git_repository`
  runs git with the repository environment, so curl's own proxy rules apply.
* The CLI installs the environment once per process. Library callers and unit
  tests connect directly, the behavior before N40. A known edge case: hosts
  are compared after URL normalization (lower case), whereas Java compares the
  URI's host as written.

## Deliberate and known differences

* `Proxy address <redacted> is not a valid URL`, as described above.
* Event-location rewriting applies to the message prefixes native prints.
  Native `print()` output carries no location, which predates N40.
* The former genrule environment divergence (native `PATH=/usr/bin:/bin`,
  `LANG=C`, `LC_ALL=C`) is gone since N42: genrules and Starlark actions get
  Bazel's action environment, and `--[no]incompatible_strict_action_env`,
  `--action_env` and `--host_action_env` are configuration options
  (docs/ACTION_ENV.md).

## `~/code` (read-only) measurement

These runs evaluate rc files: they do not pass `--ignore_all_rc_files`. `HOME`
points to a private directory, `BAZELRC` is unset, and the options are
`--noworkspace_rc --bazelrc=<scratch copy of ~/code/.bazelrc without
--remote_cache/--credential_helper>`. The runs stop at:

* `query //src/beyondwork.ai/utils/str:str`: every rc option is accepted, and
  loading stops with
  `…/str/BUILD.bazel:1:6-38: external repository load: module aspect_bazel_lib@2.22.5 was not found in registries []`.
  Native has no implicit Bazel Central Registry; Bazel's default `--registry` is
  `https://bcr.bazel.build/`.
* `build //src/beyondwork.ai/utils/str:str`: `--java_runtime_version=remotejdk_11`
  (`.bazelrc:7`, main's N28b). Without that line, the build stops at the rc
  Starlark flag `@com_google_protobuf//bazel/toolchains:prefer_prebuilt_protoc`
  on the same missing module: `Error loading option …: module
  aspect_bazel_lib@2.22.5 was not found in registries []`.
* With `.bazelrc.coder` added, its proxy replaced by a closed local port, the
  stops are the same. No download is attempted before the registry error.

## Evidence

* Rust unit tests: `src/rc_options.rs`, `src/repo_env.rs`,
  `src/download_proxy.rs`, `src/location_printer.rs`, `src/configuration.rs`
  (Apple SDK versions), `src/config_registry.rs` (exec propagation) and
  `src/bzlmod.rs` (the `isolate` gate).
* `src/download_proxy.rs` `create_proxy_if_needed_details` covers
  `ProxyHelper.createProxyIfNeeded` (:74-206): `no_proxy` before `NO_PROXY`
  with the empty fallback; `\s*,\s*` entries; leading `.` matching
  sub-domains only; exact host or `.entry`; no `*`, port or CIDR matching;
  case sensitivity; the scheme-selected variable with lowercase winning even
  when empty; and `No password given for proxy …`.
  `tests/rc_options.rs` `proxies_are_chosen_again_for_every_redirect_target`
  re-chooses the proxy for each redirect target, both ways, and a rejected
  proxy on a redirect fails before any connection.
* CLI tests: `tests/rc_options.rs` covers ~/code's rc lines, defaults and
  explicit failures, `--repo_env` set/unset/inherit/`%bazel_workspace%`/rc
  with repository re-execution, never-printed values, the `isolate` gate,
  relative locations, Apple selects, and proxies through local servers only:
  absolute-form HTTP, client-environment proxies, `NO_PROXY`, `=NAME`, CONNECT,
  and invalid and SOCKS addresses. Updated tests: `tests/bazelrc.rs`,
  `tests/host_os.rs`, `tests/repository_environment_options.rs` and
  `tests/module_extensions.rs`.
* Paired capture: `artifacts/rc_options-capture.py`. Its tracked receipt is
  `tests/fixtures/rc_options/reference_receipt.json`, which no test reads. The
  40 cases ran against genuine Bazel 9.3.0 with `--batch` and a private output
  user root; genuine `build` fetched Bazel's own built-in module dependencies
  from its default registry. Proxy cases used local servers only. Results: 17
  EXACT, 11 EXACT_MESSAGE, 5 EXACT_ROUTE (identical request lines at the local
  proxy and direct servers, including CONNECT for HTTPS), 2 EQUIVALENT_MESSAGE
  (the `isolate` error inside each engine's Starlark traceback framing), 4
  EXPECTED_NATIVE_UNSUPPORTED (sandbox, remote execution,
  `--noincompatible_strict_action_env`,
  `--incompatible_enable_proto_toolchain_resolution=false`) and 1 MISMATCH
  (`isolate-with-flag`). That case was fixed after the capture (listed in
  the receipt's `post_capture_changes`) and re-captured on both engines in the
  receipt's `targeted_rerun`, where it is EXACT_MESSAGE, so 0 mismatches
  remain. The receipt also records the native `~/code` probe above.
