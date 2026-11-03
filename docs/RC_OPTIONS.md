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
| `--sandbox_add_mount_pair` (`build:linux_*`) | `SandboxOptions` (:173-180, `MountPairConverter` :50-80). Bind mounts of linux-sandbox; an identity mount of `/tmp` also disables hermetic /tmp (`LinuxSandboxedSpawnRunner.useHermeticTmp`:218-245). Genuine on this host runs processwrapper-sandbox, which ignores mounts. | Building commands and `info` (which inherit build's options) parse it with the converter's errors; `query` rejects it as unrecognized like Bazel. An identity mount of an absolute path (`/tmp`, `/tmp:/tmp`) is accepted with no further effect: the Landlock sandbox already shows every host path at its own path and keeps the host `/tmp` writable (docs/SANDBOX.md). Any real remount (`SRC:DST`, relative paths) fails explicitly naming the rc file:line. Receipt: `tests/fixtures/sandbox_mount_pair/`. |
| `--remote_executor`, `--remote_default_exec_properties` (`build:buildbarn`) | `RemoteOptions` (:91, :601) | Fail explicitly with `remote execution is not implemented`. The value is not echoed. Only reached with `--config=buildbarn`. `--remote_instance_name` (:367) and `--[no]remote_upload_local_results` are N41's RemoteOptions (accepted; docs/REMOTE_CACHE.md). |
| `--check_direct_dependencies` | `RepositoryOptions` (:305-319), every command, default `warning`. `BazelModuleResolutionFunction.verifyRootModuleDirectDepsAreAccurate` (:261-292) compares each root dependency, as requested after the overrides, with the selected version. | All three modes with Bazel's texts: `WARNING:`/`ERROR: For repository 'R', the root module requires module version M@V, but got M@W …`, then `Direct dependency check failed.` under `error` (`mod`: exit 37). Printed whenever the module graph is resolved; a warm snapshot, like a warm Bazel server, prints nothing again. `query` resolves the graph lazily, so it does not print or fail when it never needs the graph. |
| `--deleted_packages` | `PackageOptions` (:219-244, `PackageIdentifier.parse` per element), `PackageLookupFunction` (:98-99). | The packages are no packages: loading one fails with `no such package 'P': Package is considered deleted due to --deleted_packages`, `//...` skips it but still descends, and its BUILD file is no boundary for globs, labels, outputs or loads of the parent package (main repository directories; `@repo//pkg` entries make only package loading fail). |
| `--subcommands`, `-s` | `ExecutionOptions.showSubcommands` (BoolOrEnumConverter, `true`/`pretty_print`/`false`), `ActionExecutionContext.maybeReportSubcommand` (:459-499). | Each spawn that reaches a runner (not a cache hit) prints `SUBCOMMAND: # OWNER [action 'PROGRESS', configuration: C, mnemonic: M]` and Bazel's `describeCommand` body verbatim: `(cd EXECROOT && exec env - VARS ARGV)`, one argument per line under `pretty_print`, then `# Configuration:` and `# Runner:`. Differences: native actions carry no ActionOwner, progress message, configuration checksum or execution platform, so the owner is the label, the progress is `Executing genrule L` for genrules and `MNEMONIC OUTPUT` otherwise, the configuration is the output directory name, no platform lines are printed, and `cd` names the spawn's own execution root. Test spawns and remote cache hits are not reported. |
| `--build_tag_filters` | `LoadingOptions` (:57-70); `TargetPatternPhaseFunction.mergeAll` (:372-373) with `TargetUtils.tagFilter`. | The selected targets are filtered by their rule tags (a test rule's include size, timeout, `[no]flaky`, `[no]local`); non-rules pass only without required tags; tests determined by `test` are not filtered. |
| `--incompatible_python_disallow_native_rules` | `PythonOptions` (:71-81): fails the builtin `py_*` rules, which Bazel 9.3 no longer has. | A configuration value option (`config_setting` matches it; propagated to exec configurations); no other effect. |
| Bazel no-op booleans (`--enable_bzlmod`, `--enable_workspace`, `--legacy_external_runfiles`, `--incompatible_enable_cc_toolchain_resolution`, … 97 in all) and server-state booleans (`--discard_analysis_cache`, `--[no]track_incremental_state`, `--[no]keep_state_after_build`, `--heap_dump_on_oom`, `--watchfs`) | `help = "No-op"` or the `NO_OP` effect tag; the others act on the server's Skyframe state, JVM or file watching. | `crate::rc_options::no_effect_option`: any boolean value is accepted with no effect for the commands genuine registers them for (others: `Unrecognized option`). `--fetch` and `--incompatible_disallow_empty_glob` are accepted at their default only. |
| `--repository_cache`, `--repo_contents_cache`, `--experimental_repository_cache_hardlinks`, `--experimental_repository_downloader_retries`, `--disk_cache` | `RepositoryOptions`, `RemoteOptions` | Modelled with genuine's on-disk layouts, shared with genuine Bazel ([REPOSITORY_CACHE.md](REPOSITORY_CACHE.md)). |
| `--java_runtime_version` | `JavaOptions` (:373) | main models it in N28b (8301936), which this branch's base predates. Here it stays an explicit unsupported option, and integration takes main's model. |

### Configuration flags, execution and output options (option-parity batch)

Pinned source for this section: `/tmp/opencode/bazel-9.3.1-reference` (9.3.1).
Implementation: `src/configuration.rs` (flag lists), `src/execution_options.rs`,
`src/output_filter.rs`, `src/option_defaults.rs`, `src/toolchains.rs` (debug
messages), `src/action.rs` (`--check_up_to_date`), `src/main.rs`. Tests:
`tests/config_flag_options.rs`, `tests/cc_compile.rs`
(`cc_flag_options_reach_the_compile_command_lines`), `tests/execution_options.rs`;
genuine receipts beside their fixtures (`capture*.py`).

| Option | Bazel 9.3.1 | Native |
| --- | --- | --- |
| `--features`, `--host_features` | CoreOptions (:831-856), normalized by `getNormalizedFeatures`; `ctx.features`/`ctx.disabled_features` merge them under the package's and rule's (RuleContext.computeFeatures); exec: `--features` = `--host_features` | Modelled (docs/CONFIGURATION.md "On the command line"); rules_cc's feature configuration and rules_go read `ctx.features`. rules_go 0.63 turns `--features=race` into a `WARNING: --features=race is no longer supported` DEBUG line and **no** race mode, natively exactly as genuine (verified on a scratch go_test: same action command lines); race mode is `--@io_bazel_rules_go//go/config:race`. |
| `--copt`, `--cxxopt`, `--conlyopt`, `--linkopt`, `--per_file_copt` and `--host_*` | CppOptions; read by rules_cc's Starlark (`ctx.fragments.cpp`, `cc_internal.per_file_copts`) | Modelled; the CppCompile command lines equal genuine's (receipt `tests/fixtures/cc_compile/flag_options_receipt.json`). `query`: `Unrecognized option`. |
| `--dynamic_mode` | CppOptions (:205-216), `ctx.fragments.cpp.dynamic_mode()` | A value option (off/default/fully; not propagated to exec). |
| `--javacopt`, `--host_javacopt` | JavaOptions (:50-69); `default_javac_flags[_depset]` (JavaConfiguration :107-172) | Modelled, tokenized/detokenized like JavaHelper (receipt `tests/fixtures/config_flag_options`). |
| `--output_filter`, `--auto_output_filter` | Reporter OutputFilter on successful actions' output (SkyframeActionExecutor :1305-1315; OutputFilteringModule, AutoOutputFilter) | Modelled on the native action-output notes; failed actions always report. |
| `--experimental_ui_max_stdouterr_bytes` | UiEventHandler.getContentIfSmallEnough | Modelled: the oversized stream is saved under `<output base>/execroot/_main/bazel-out/_tmp/actions` and replaced by genuine's `… exceeds maximum size …; skipping` line. |
| `--toolchain_resolution_debug` | SingleToolchainResolutionDebugPrinter, ToolchainResolutionDebugPrinter | The main messages verbatim (`Performing resolution of …`, `Rejected toolchain …`, `Compatible/Incompatible execution platform …`, `Recap …`/`No … toolchain found …`, `Removed execution platform …`, `Target platform …: Selected execution platform …`). Differences: each identical message prints once (genuine repeats some on Skyframe restarts), and `target_settings` rejections (RegisteredToolchainsValue's `rejectedToolchains`) are not listed. |
| `--check_up_to_date` | CheckUpToDateFilter; `action 'X' is not up-to-date` (SkyframeActionExecutor :646-656) | Modelled at the action-cache miss; the progress text is native's (`Executing genrule L`, `BazelWorkspaceStatusAction FILE`, else `MNEMONIC OUTPUT`) and the error carries the native `action 'ID': ` prefix. Tests still run, as in genuine. |
| `--modify_execution_info`, `--[no]incompatible_modify_execution_info_additive` | ExecutionInfoModifier on every spawn's execution info by mnemonic | Applied to every planned spawn's execution requirements (and rule-tag requirements for `-key`) when the plan is final: keyed, and the strategy/remote policies see it. `query` accepts and ignores them (CoreOptions). aquery/cquery show the unmodified requirements. |
| `--local_resources` | ResourceManager over DEFAULT_RESOURCE_SET (1 CPU, 250 MB) | `cpu`/`memory` cap the local action concurrency (`min(jobs, cpu, memory/250)`, at least 1; not with a remote executor). Other resource names are accepted; actions' `resources:NAME:N` tags are not modelled. |
| `--jobs=auto`, `--jobs=HOST_CPUS*F` | JobsConverter | `auto` is the default (native bound); keyword forms are computed. |
| `--progress_report_interval` | non-curses progress reports | Validated (0-3600), no effect: the native console prints no periodic reports. |
| `--[no]action_cache_store_output_metadata` (old `experimental_…`) | lets the action cache serve remote-only outputs | Accepted (no result effect). |
| `--symlink_prefix`, `--experimental_convenience_symlinks` | convenience symlinks | bazel-rs creates none (genuine's `--symlink_prefix=/`): `/`, `ignore` and `clean` (removes `PREFIX{bin,testlogs,out,genfiles,<workspace>}` symlinks) are accepted; any combination that would create symlinks fails explicitly. |
| `version --[no]gnu_format` | VersionCommand | `bazel 9.3.1+bazel-rs.<version>` (the version of `info release`). |
| `--ui_event_filters` | UiOptions.EventFiltersConverter; the UI drops the filtered event kinds | INFO, WARNING and DEBUG (the native notes) and START/FINISH (never shown) are filtered like genuine; filtering any other kind (ERROR, STDOUT, PROGRESS, …) fails explicitly. |
| `--platform_suffix` | CoreOptions; added to the output directory mnemonic | Modelled (`k8-fastbuild-SUFFIX`, a `/` fails like genuine; receipt `tests/fixtures/config_flag_options`). |
| `--jvmopt`, `--host_jvmopt` | JavaOptions; `ctx.fragments.java.default_jvm_opts`; exec: `--host_jvmopt` or `-XX:ErrorFile=/dev/stderr` | Modelled (receipt `tests/fixtures/config_flag_options`). |

#### Options at their default

`crate::option_defaults`: an option this profile does not otherwise model is
accepted when it is spelled at its genuine 9.3.1 default — `help <command>
--long`'s `default: "…"` (src/help_topics.rs) or, for undocumented options,
the source's literal `defaultValue` (src/option_source_defaults.rs,
scripts/extract_option_defaults.py) — and fails explicitly with the default
named otherwise. Genuine treats an option at its default exactly like an
absent one. Not covered: repeatable options, computed (`see description`) and
`null` defaults, expansion/`Void` options, options whose presence warns or
implies others (`deprecationWarning`, `implicitRequirements`), and the
client-supplied ones (`--isatty`, `--client_env`, …).

#### Sweep

`scripts/option_sweep.py --bazel-rs BIN --scratch DIR` probes every genuine
option of `build`, `test`, `run`, `query` and `cquery` (src/option_metadata.rs)
with its default (booleans both ways) and lists what bazel-rs still rejects
as not modelled; see [Remaining rejected options](#remaining-rejected-options).

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

## Remaining rejected options

`scripts/option_sweep.py` over the genuine 9.3.1 options of `build`, `test`,
`run`, `query` and `cquery` (release binary of this batch; each option probed
with its default and, for booleans, both spellings; `default`/`other` says
whether the probe spelled the genuine default). Every probe below still fails
with the explicit `unsupported option … options are never silently ignored`
error (or a native limitation message). Rejected even at the default: only the
deliberate exclusions of crate::option_defaults (client-supplied
`--isatty`/`--emacs`/`--terminal_columns`/`--command_wait_time`/`--startup_time`/
`--extract_data_time`/`--restart_reason`, the deprecated `--fat_apk_cpu`).
Every other row is an unmodelled option at a non-default value. Probe values
are placeholders (`x`, `//:x`, `1`), so a converter error may stand for a
rejection of any value.

| probe | value | commands |
| --- | --- | --- |
| `--add_go_exec_groups_to_binary_rules` | other | build,test,run,query,cquery |
| `--allow_analysis_failures` | other | build,test,run,query,cquery |
| `--allow_experimental_loads` | other | build,test,run,query,cquery |
| `--allowed_cpu_values=x` | other | build,test,run,query,cquery |
| `--allowed_local_actions_regex=x` | other | build,test,run,cquery |
| `--allowed_strategies_by_exec_platform=x` | other | build,test,run,cquery |
| `--android_compiler=x` | other | build,test,run,cquery |
| `--android_cpu=x` | other | build,test,run,cquery |
| `--android_crosstool_top=x` | other | build,test,run,cquery |
| `--android_grte_top=x` | other | build,test,run,cquery |
| `--android_migration_tag_check` | other | build,test,run,cquery |
| `--android_platforms=//:x` | other | build,test,run,cquery |
| `--android_resource_shrinking` | other | build,test,run,cquery |
| `--android_sdk=x` | other | build,test,run,cquery |
| `--apple_crosstool_top=x` | other | build,test,run,query,cquery |
| `--apple_generate_dsym` | other | build,test,run,cquery |
| `--apple_platforms=//:x` | other | build,test,run,cquery |
| `--apple_split_cpu=x` | other | build,test,run,cquery |
| `--auto_cpu_environment_group=x` | other | build,test,run,query,cquery |
| `--bes_backend=x` | other | query,cquery |
| `--bes_check_preceding_lifecycle_events` | other | query,cquery |
| `--bes_header=x=y` | other | query,cquery |
| `--bes_instance_name=x` | other | query,cquery |
| `--bes_keywords=x` | other | query,cquery |
| `--bes_proxy=x` | other | query,cquery |
| `--bes_results_url=x` | other | query,cquery |
| `--bes_system_keywords=x` | other | query,cquery |
| `--binary_path=x` | other | build,test,run,query,cquery |
| `--break_build_on_parallel_dex2oat_failure` | other | build,test,run,cquery |
| `--build_event_binary_file=x` | other | query,cquery |
| `--build_event_json_file=x` | other | query,cquery |
| `--build_event_publish_all_actions` | other | query,cquery |
| `--build_event_text_file=x` | other | query,cquery |
| `--build_manual_tests` | other | build,test,run,cquery |
| `--build_metadata=x=y` | other | query,cquery |
| `--build_python_zip` | other | build,test,run,cquery |
| `--build_request_id=x` | other | build,test,run,query,cquery |
| `--build_test_dwp` | other | build,test,run,cquery |
| `--catalyst_cpus=x` | other | build,test,run,cquery |
| `--cc_include_scanning` | other | build,test,run,cquery |
| `--cc_output_directory_tag=x` | other | build,test,run,cquery |
| `--check_licenses` | other | build,test,run,query,cquery |
| `--check_tests_up_to_date` | other | build,test,run,cquery |
| `--client_cwd=/tmp/x` | other | build,test,run,query,cquery |
| `--client_env=x=y` | other | build,test,run,query,cquery |
| `--command_wait_time=0` | default | build,test,run,query,cquery |
| `--compile_one_dependency` | other | build,test,run,cquery |
| `--compiler=x` | other | build,test,run,cquery |
| `--consistent_labels` | other | cquery |
| `--cs_fdo_absolute_path=x` | other | build,test,run,cquery |
| `--cs_fdo_instrument=x` | other | build,test,run,cquery |
| `--cs_fdo_profile=//:x` | other | build,test,run,cquery |
| `--custom_malloc=//:x` | other | build,test,run,cquery |
| `--debug_spawn_scheduler` | other | build,test,run,cquery |
| `--default_override=x` | other | build,test,run,query,cquery |
| `--default_test_resources=x` | other | build,test,run,cquery |
| `--desugar_java8_libs` | other | build,test,run,cquery |
| `--distdir=/tmp/x` | other | build,test,run,query,cquery |
| `--downloader_config=/tmp/x` | other | build,test,run,query,cquery |
| `--dynamic_local_strategy=x=y` | other | build,test,run,cquery |
| `--dynamic_remote_strategy=x=y` | other | build,test,run,cquery |
| `--emacs` | other | build,test,run,query,cquery |
| `--emit_script_path_in_exec_request` | other | run |
| `--enforce_proguard_file_extension` | other | build,test,run,cquery |
| `--enforce_transitive_configs_for_config_feature_flag` | other | build,test,run,cquery |
| `--exec_aspects=x` | other | build,test,run,query,cquery |
| `--execution_log_binary_file=/tmp/x` | other | cquery |
| `--execution_log_compact_file=/tmp/x` | other | cquery |
| `--execution_log_json_file=/tmp/x` | other | cquery |
| `--experimental_action_listener=//:x` | other | build,test,run,query,cquery |
| `--experimental_active_directories=x` | other | build,test,run,cquery |
| `--experimental_allow_project_files` | other | build,test,run,query,cquery |
| `--experimental_always_filter_duplicate_classes_from_android_test` | other | build,test,run,cquery |
| `--experimental_analysis_cache_key_distinguisher_for_testing=x` | other | build,test,run,cquery |
| `--experimental_analysis_cache_server_checksum_override=x` | other | build,test,run,cquery |
| `--experimental_analysis_cache_service=x` | other | build,test,run,cquery |
| `--experimental_android_assume_minsdkversion` | other | build,test,run,cquery |
| `--experimental_android_compress_java_resources` | other | build,test,run,cquery |
| `--experimental_android_library_exports_manifest_default` | other | build,test,run,cquery |
| `--experimental_android_resource_cycle_shrinking` | other | build,test,run,cquery |
| `--experimental_android_resource_name_obfuscation` | other | build,test,run,cquery |
| `--experimental_android_resource_path_shortening` | other | build,test,run,cquery |
| `--experimental_android_resource_shrinking` | other | build,test,run,cquery |
| `--experimental_android_rewrite_dexes_with_rex` | other | build,test,run,cquery |
| `--experimental_android_use_parallel_dex2oat` | other | build,test,run,cquery |
| `--experimental_aquery_dump_after_build_format=x` | other | build,test,run,cquery |
| `--experimental_aquery_dump_after_build_output_file=/tmp/x` | other | build,test,run,cquery |
| `--experimental_async_execution` | other | build,test,run,cquery |
| `--experimental_bep_target_summary` | other | build,test,run,query,cquery |
| `--experimental_build_event_expand_filesets` | other | build,test,run,query,cquery |
| `--experimental_build_event_output_group_mode=x` | other | build,test,run,query,cquery |
| `--experimental_build_event_upload_strategy=x` | other | build,test,run,query,cquery |
| `--experimental_builtins_dummy` | other | build,test,run,query,cquery |
| `--experimental_builtins_injection_override=x` | other | build,test,run,query,cquery |
| `--experimental_cc_shared_library` | other | build,test,run,query,cquery |
| `--experimental_cc_skylark_api_enabled_packages=x` | other | build,test,run,query,cquery |
| `--experimental_circuit_breaker_strategy=x` | other | build,test,run,query,cquery |
| `--experimental_collect_pressure_stall_indicators` | other | query,cquery |
| `--experimental_collect_resource_estimation` | other | query,cquery |
| `--experimental_collect_skyframe_counts_in_profiler` | other | query,cquery |
| `--experimental_command_profile=x` | other | query,cquery |
| `--experimental_convenience_symlinks` | other | build,test,run |
| `--experimental_cpp_compile_resource_estimation` | other | build,test,run,cquery |
| `--experimental_cpp_modules` | other | build,test,run,cquery |
| `--experimental_cpu_load_scheduling` | other | build,test,run,cquery |
| `--experimental_debug_selects_always_succeed` | other | build,test,run,query,cquery |
| `--experimental_disable_external_package` | other | build,test,run,query,cquery |
| `--experimental_disable_instrumentation_manifest_merge` | other | build,test,run,cquery |
| `--experimental_docker_image=x` | other | build,test,run,cquery |
| `--experimental_docker_privileged` | other | build,test,run,cquery |
| `--experimental_docker_verbose` | other | build,test,run,cquery |
| `--experimental_dormant_deps` | other | build,test,run,query,cquery |
| `--experimental_dynamic_ignore_local_signals=x` | other | build,test,run,cquery |
| `--experimental_enable_android_migration_apis` | other | build,test,run,query,cquery |
| `--experimental_enable_docker_sandbox` | other | build,test,run,cquery |
| `--experimental_enable_execution_graph_log` | other | build,test,run,cquery |
| `--experimental_enable_skyfocus` | other | build,test,run,cquery |
| `--experimental_enable_thread_dump` | other | build,test,run,query,cquery |
| `--experimental_enforce_transitive_visibility` | other | build,test,run,query,cquery |
| `--experimental_exclude_defines_from_exec_config` | other | build,test,run,query,cquery |
| `--experimental_execution_graph_include_change_pruned_actions` | other | build,test,run,cquery |
| `--experimental_execution_graph_log_path=x` | other | build,test,run,cquery |
| `--experimental_explicit_aspects` | other | cquery |
| `--experimental_extended_sanity_checks` | other | build,test,run,query,cquery |
| `--experimental_extra_action_filter=-x` | other | build,test,run,cquery |
| `--experimental_extra_action_top_level_only` | other | build,test,run,cquery |
| `--experimental_filter_library_jar_with_program_jar` | other | build,test,run,cquery |
| `--experimental_filter_r_jars_from_android_test` | other | build,test,run,cquery |
| `--experimental_force_gc_after_build` | other | build,test,run,query,cquery |
| `--experimental_frontier_violation_verbose` | other | build,test,run,cquery |
| `--experimental_generate_llvm_lcov` | other | build,test,run,cquery |
| `--experimental_genquery_use_graphless_query` | other | build,test,run,cquery |
| `--experimental_get_android_java_resources_from_optimized_jar` | other | build,test,run,cquery |
| `--experimental_import_deps_checking=x` | other | build,test,run,query,cquery |
| `--experimental_include_xcode_execution_requirements` | other | build,test,run,cquery |
| `--experimental_inmemory_dotincludes_files` | other | build,test,run,cquery |
| `--experimental_inmemory_sandbox_stashes` | other | build,test,run,cquery |
| `--experimental_java_test_auto_create_deploy_jar` | other | build,test,run,cquery |
| `--experimental_lazy_macro_expansion_packages=x` | other | build,test,run,query,cquery |
| `--experimental_local_java_optimization_configuration=//:x` | other | build,test,run,cquery |
| `--experimental_local_java_optimizations` | other | build,test,run,cquery |
| `--experimental_local_lockfree_output` | other | build,test,run,cquery |
| `--experimental_materialize_param_files_directly` | other | build,test,run,cquery |
| `--experimental_omit_resources_info_provider_from_android_binary` | other | build,test,run,cquery |
| `--experimental_omitfp` | other | build,test,run,cquery |
| `--experimental_one_version_enforcement_use_transitive_jars_for_binary_under_test` | other | build,test,run,cquery |
| `--experimental_oom_sensitive_skyfunctions_semaphore_size=HOST_CPUS` | other | build,test,run,cquery |
| `--experimental_override_platform_cpu_name=x` | other | build,test,run,query,cquery |
| `--experimental_persistent_aar_extractor` | other | build,test,run,cquery |
| `--experimental_platform_in_output_dir` | other | build,test,run,query,cquery |
| `--experimental_platforms_api` | other | build,test,run,query,cquery |
| `--experimental_process_wrapper_graceful_sigterm` | other | build,test,run,cquery |
| `--experimental_profile_additional_tasks=x` | other | query,cquery |
| `--experimental_profile_include_primary_output` | other | query,cquery |
| `--experimental_profile_include_target_configuration` | other | query,cquery |
| `--experimental_profile_include_target_label` | other | query,cquery |
| `--experimental_propagate_custom_flag=x` | other | build,test,run,query,cquery |
| `--experimental_publish_package_metrics_in_bep` | other | build,test,run,query,cquery |
| `--experimental_py_binaries_include_label` | other | build,test,run,cquery |
| `--experimental_record_metrics_for_all_mnemonics` | other | build,test,run,query,cquery |
| `--experimental_record_skyframe_metrics` | other | build,test,run,query,cquery |
| `--experimental_remotable_source_manifests` | other | build,test,run,query,cquery |
| `--experimental_remote_analysis_cache=x` | other | build,test,run,cquery |
| `--experimental_remote_analysis_write_proxy=x` | other | build,test,run,cquery |
| `--experimental_remote_cache_chunking` | other | build,test,run,query,cquery |
| `--experimental_remote_cache_lease_extension` | other | build,test,run,query,cquery |
| `--experimental_remote_capture_corrupted_outputs=/tmp/x` | other | build,test,run,query,cquery |
| `--experimental_remote_downloader=x` | other | build,test,run,query,cquery |
| `--experimental_remote_mark_tool_inputs` | other | build,test,run,query,cquery |
| `--experimental_remote_output_service=x` | other | build,test,run,query,cquery |
| `--experimental_remote_output_service_output_path_prefix=x` | other | build,test,run,query,cquery |
| `--experimental_remote_require_cached` | other | build,test,run,query,cquery |
| `--experimental_remote_scrubbing_config=x` | other | build,test,run,query,cquery |
| `--experimental_repo_remote_exec` | other | build,test,run,query,cquery |
| `--experimental_repository_ctx_execute_wasm` | other | build,test,run,query,cquery |
| `--experimental_reuse_include_scanning_threads` | other | build,test,run,cquery |
| `--experimental_run_android_lint_on_java_rules` | other | build,test,run,cquery |
| `--experimental_run_bep_event_include_residue` | other | query,cquery |
| `--experimental_sandbox_enforce_resources_regexp=x` | other | build,test,run,cquery |
| `--experimental_sandbox_limits=x=y` | other | build,test,run,cquery |
| `--experimental_save_feature_state` | other | build,test,run,cquery |
| `--experimental_shrink_worker_pool` | other | build,test,run,cquery |
| `--experimental_sibling_repository_layout` | other | build,test,run,query,cquery |
| `--experimental_single_package_toolchain_binding` | other | build,test,run,query,cquery |
| `--experimental_skyfocus_dump_post_gc_stats` | other | build,test,run,cquery |
| `--experimental_skyframe_cpu_heavy_skykeys_thread_pool_size=HOST_CPUS` | other | build,test,run,cquery |
| `--experimental_skyframe_memory_dump=x` | other | build,test,run,cquery |
| `--experimental_skylark_debug` | other | build,test,run,cquery |
| `--experimental_skylark_debug_reset_analysis` | other | build,test,run,cquery |
| `--experimental_skylark_debug_verbose_logging` | other | build,test,run,cquery |
| `--experimental_spawn_scheduler` | other | build,test,run,cquery |
| `--experimental_starlark_type_checking` | other | build,test,run,query,cquery |
| `--experimental_starlark_types_allowed_paths=x` | other | build,test,run,query,cquery |
| `--experimental_stats_summary` | other | build,test,run,cquery |
| `--experimental_stream_log_file_uploads` | other | build,test,run,query,cquery |
| `--experimental_strict_fileset_output` | other | build,test,run,query,cquery |
| `--experimental_strict_repo_env` | other | build,test,run,query,cquery |
| `--experimental_tool_command_line=x` | other | build,test,run,query,cquery |
| `--experimental_turbine_annotation_processing` | other | build,test,run,cquery |
| `--experimental_ui_debug_all_events` | other | build,test,run,query,cquery |
| `--experimental_unsupported_and_brittle_include_scanning` | other | build,test,run,cquery |
| `--experimental_use_cpp_compile_action_args_params_file` | other | build,test,run,cquery |
| `--experimental_use_hermetic_linux_sandbox` | other | build,test,run,cquery |
| `--experimental_use_llvm_covmap` | other | build,test,run,cquery |
| `--experimental_use_rtxt_from_merged_resources` | other | build,test,run,cquery |
| `--experimental_use_windows_sandbox` | other | build,test,run,cquery |
| `--experimental_windows_watchfs` | other | build,test,run,query,cquery |
| `--experimental_worker_cancellation` | other | build,test,run,cquery |
| `--experimental_worker_sandbox_hardening` | other | build,test,run,cquery |
| `--experimental_worker_sandbox_inmemory_tracking=x` | other | build,test,run,cquery |
| `--experimental_worker_use_cgroups_on_linux` | other | build,test,run,cquery |
| `--experimental_workspace_rules_log_file=/tmp/x` | other | build,test,run,query,cquery |
| `--experimental_writable_outputs` | other | build,test,run,query,cquery |
| `--explain=/tmp/x` | other | build,test,run,cquery |
| `--explicit_java_test_deps` | other | build,test,run,cquery |
| `--extract_data_time=0` | default | build,test,run,query,cquery |
| `--fat_apk_cpu=armeabi-v7a` | default | build,test,run,cquery |
| `--fdo_instrument=x` | other | build,test,run,cquery |
| `--fdo_optimize=x` | other | build,test,run,cquery |
| `--fdo_prefetch_hints=//:x` | other | build,test,run,cquery |
| `--fdo_profile=//:x` | other | build,test,run,cquery |
| `--flag_alias=x` | other | query |
| `--force_python=x` | other | build,test,run,query,cquery |
| `--force_starlark_stack_trace` | other | build,test,run,query,cquery |
| `--generate_json_trace_profile` | other | query,cquery |
| `--google_credentials=x` | other | build,test,run,query,cquery |
| `--google_default_credentials` | other | build,test,run,query,cquery |
| `--grte_top=x` | other | build,test,run,cquery |
| `--guard_against_concurrent_changes` | other | build,test,run,query,cquery |
| `--heuristically_drop_nodes` | other | build,test,run,query,cquery |
| `--hide_aspect_results=x` | other | build,test,run,cquery |
| `--high_priority_workers=x` | other | build,test,run,cquery |
| `--host_compiler=x` | other | build,test,run,cquery |
| `--host_crosstool_top=x` | other | build,test,run,cquery |
| `--host_force_python=x` | other | build,test,run,query,cquery |
| `--host_grte_top=x` | other | build,test,run,cquery |
| `--host_java_launcher=//:x` | other | build,test,run,cquery |
| `--host_java_toolchain=x` | other | build,test,run,query,cquery |
| `--host_javabase=x` | other | build,test,run,query,cquery |
| `--host_macos_minimum_os=1.0` | other | build,test,run,cquery |
| `--incompatible_autoload_externally=x` | other | build,test,run,query,cquery |
| `--incompatible_bep_cpu_from_platform` | other | build,test,run,query,cquery |
| `--incompatible_check_external_repo_source_dir_package_boundary` | other | build,test,run,query,cquery |
| `--incompatible_check_testonly_for_output_files` | other | build,test,run,query,cquery |
| `--incompatible_config_setting_private_default_visibility` | other | build,test,run,query,cquery |
| `--incompatible_default_to_explicit_init_py` | other | build,test,run,cquery |
| `--incompatible_disable_native_android_rules` | other | build,test,run,cquery |
| `--incompatible_disable_select_on=x` | other | build,test,run,query,cquery |
| `--incompatible_disable_target_default_provider_fields` | other | build,test,run,query,cquery |
| `--incompatible_disable_transitions_on=x` | other | build,test,run,query,cquery |
| `--incompatible_disallow_java_import_exports` | other | build,test,run,cquery |
| `--incompatible_disallow_sdk_frameworks_attributes` | other | build,test,run,cquery |
| `--incompatible_enable_apple_toolchain_resolution` | other | build,test,run,cquery |
| `--incompatible_enforce_starlark_utf8` | other | build,test,run,query,cquery |
| `--incompatible_exclude_starlark_flags_from_exec_config` | other | build,test,run,query,cquery |
| `--incompatible_limit_platforms_in_output_dir_to=//:x` | other | build,test,run,query,cquery |
| `--incompatible_no_implicit_file_export` | other | build,test,run,query,cquery |
| `--incompatible_no_rule_outputs_param` | other | build,test,run,query,cquery |
| `--incompatible_objc_alwayslink_by_default` | other | build,test,run,cquery |
| `--incompatible_remote_local_fallback_for_remote_cache` | other | build,test,run,query,cquery |
| `--incompatible_remove_ctx_android_fragment` | other | build,test,run,cquery |
| `--incompatible_require_repo_extension_metadata` | other | build,test,run,query,cquery |
| `--incompatible_resolve_select_keys_eagerly` | other | build,test,run,query,cquery |
| `--incompatible_stop_exporting_build_file_path` | other | build,test,run,query,cquery |
| `--incompatible_stop_exporting_language_modules` | other | build,test,run,query,cquery |
| `--incompatible_strip_executable_safely` | other | build,test,run,cquery |
| `--incompatible_use_cc_configure_from_rules_cc` | other | build,test,run,query,cquery |
| `--incompatible_use_cpp_compile_header_mnemonic` | other | build,test,run,cquery |
| `--infer_universe_scope` | other | cquery |
| `--inject_repository=x` | other | build,test,run,query,cquery |
| `--internal_persistent_android_dex_desugar` | other | build,test,run,cquery |
| `--internal_persistent_busybox_tools` | other | build,test,run,cquery |
| `--internal_persistent_multiplex_android_dex_desugar` | other | build,test,run,cquery |
| `--internal_persistent_multiplex_busybox_tools` | other | build,test,run,cquery |
| `--internal_starlark_flag_test_canary` | other | build,test,run,query,cquery |
| `--invocation_id=x` | other | build,test,run,query,cquery |
| `--ios_memleaks` | other | build,test,run,cquery |
| `--ios_minimum_os=1.0` | other | build,test,run,cquery |
| `--ios_multi_cpus=x` | other | build,test,run,cquery |
| `--ios_signing_cert_name=x` | other | build,test,run,cquery |
| `--ios_simulator_device=x` | other | build,test,run,cquery |
| `--ios_simulator_version=1.0` | other | build,test,run,cquery |
| `--isatty` | other | build,test,run,query,cquery |
| `--j2objc_dead_code_report=//:x` | other | build,test,run,cquery |
| `--j2objc_translation_flags=x` | other | build,test,run,cquery |
| `--java_debug` | other | build,test,run,cquery |
| `--java_launcher=//:x` | other | build,test,run,cquery |
| `--java_optimization_mode=x` | other | build,test,run,query,cquery |
| `--java_toolchain=x` | other | build,test,run,query,cquery |
| `--javabase=x` | other | build,test,run,query,cquery |
| `--keep_going` | other | cquery |
| `--legacy_important_outputs` | other | build,test,run,query,cquery |
| `--legacy_main_dex_list_generator=//:x` | other | build,test,run,cquery |
| `--line_terminator_null` | other | cquery |
| `--ltobackendopt=x` | other | build,test,run,cquery |
| `--ltoindexopt=x` | other | build,test,run,cquery |
| `--macos_cpus=x` | other | build,test,run,cquery |
| `--macos_minimum_os=1.0` | other | build,test,run,cquery |
| `--materialize_param_files` | other | build,test,run,cquery |
| `--memory_profile=/tmp/x` | other | query,cquery |
| `--memprof_profile=//:x` | other | build,test,run,cquery |
| `--merge_android_manifest_permissions` | other | build,test,run,cquery |
| `--minimum_os_version=x` | other | build,test,run,cquery |
| `--module_mirrors=x=y` | other | build,test,run,query,cquery |
| `--noallow_analysis_cache_discard` | other | build,test,run,cquery |
| `--noallow_one_action_on_resource_unavailable` | other | build,test,run,cquery |
| `--noallow_unresolved_symlinks` | other | build,test,run,query,cquery |
| `--noalways_profile_slow_operations` | other | build,test,run,query,cquery |
| `--noanalyze` | other | build,test,run,cquery |
| `--noandroid_databinding_use_androidx` | other | build,test,run,cquery |
| `--noandroid_databinding_use_v3_4_args` | other | build,test,run,cquery |
| `--noandroid_fixed_resource_neverlinking` | other | build,test,run,cquery |
| `--nobes_lifecycle_events` | other | query,cquery |
| `--nobuild_event_binary_file_path_conversion` | other | query,cquery |
| `--nobuild_event_json_file_path_conversion` | other | query,cquery |
| `--nobuild_event_text_file_path_conversion` | other | query,cquery |
| `--nobuild_python_zip` | other | build,test,run,cquery |
| `--nocc_dotd_files` | other | build,test,run,cquery |
| `--nocheck_bzl_visibility` | other | build,test,run,query,cquery |
| `--nocheck_visibility` | other | build,test,run,query,cquery |
| `--nodesugar_for_android` | other | build,test,run,cquery |
| `--nodevice_debug_entitlements` | other | build,test,run,cquery |
| `--noemacs` | default | build,test,run,query,cquery |
| `--noenable_propeller_optimize_absolute_paths` | other | build,test,run,cquery |
| `--noenable_remaining_fdo_absolute_paths` | other | build,test,run,cquery |
| `--noenforce_constraints` | other | build,test,run,query,cquery |
| `--noenforce_project_configs` | other | build,test,run,cquery |
| `--noenforce_transitive_configs_for_config_feature_flag` | other | build,test,run,cquery |
| `--noexecution_log_sort` | other | cquery |
| `--noexperimental_add_test_support_to_compile_time_deps` | other | build,test,run,cquery |
| `--noexperimental_allow_map_directory` | other | build,test,run,query,cquery |
| `--noexperimental_analysis_cache_enable_metadata_queries` | other | build,test,run,cquery |
| `--noexperimental_android_databinding_v2` | other | build,test,run,cquery |
| `--noexperimental_bzl_visibility` | other | build,test,run,query,cquery |
| `--noexperimental_cancel_concurrent_tests` | other | build,test,run,cquery |
| `--noexperimental_cc_implementation_deps` | other | build,test,run,cquery |
| `--noexperimental_check_desugar_deps` | other | build,test,run,cquery |
| `--noexperimental_check_external_other_files` | other | build,test,run,query,cquery |
| `--noexperimental_check_external_repository_files` | other | build,test,run,query,cquery |
| `--noexperimental_check_output_files` | other | build,test,run,query,cquery |
| `--noexperimental_collect_load_average_in_profiler` | other | query,cquery |
| `--noexperimental_collect_system_network_usage` | other | query,cquery |
| `--noexperimental_collect_worker_data_in_profiler` | other | query,cquery |
| `--noexperimental_convenience_symlinks_bep_event` | other | build,test,run,cquery |
| `--noexperimental_docker_use_customized_images` | other | build,test,run,cquery |
| `--noexperimental_dynamic_exclude_tools` | other | build,test,run,cquery |
| `--noexperimental_enable_aspect_hints` | other | build,test,run,query,cquery |
| `--noexperimental_enable_critical_path_profiling` | other | build,test,run,cquery |
| `--noexperimental_enable_first_class_macros` | other | build,test,run,query,cquery |
| `--noexperimental_enable_jspecify` | other | build,test,run,cquery |
| `--noexperimental_enable_scl_dialect` | other | build,test,run,query,cquery |
| `--noexperimental_enable_starlark_set` | other | build,test,run,query,cquery |
| `--noexperimental_execution_graph_enable_edges_from_filewrite_actions` | other | build,test,run,cquery |
| `--noexperimental_genquery_use_graphless_query` | other | build,test,run,cquery |
| `--noexperimental_graphless_query` | other | query |
| `--noexperimental_incremental_dexing_after_proguard_by_default` | other | build,test,run,cquery |
| `--noexperimental_inmemory_dotd_files` | other | build,test,run,cquery |
| `--noexperimental_inmemory_jdeps_files` | other | build,test,run,cquery |
| `--noexperimental_link_static_libraries_once` | other | build,test,run,cquery |
| `--noexperimental_merged_skyframe_analysis_execution` | other | build,test,run,cquery |
| `--noexperimental_platform_in_output_dir` | other | build,test,run,query,cquery |
| `--noexperimental_prefer_mutual_xcode` | other | build,test,run,cquery |
| `--noexperimental_python_import_all_repositories` | other | build,test,run,cquery |
| `--noexperimental_remote_discard_merkle_trees` | other | build,test,run,query,cquery |
| `--noexperimental_remove_r_classes_from_instrumentation_test_jar` | other | build,test,run,cquery |
| `--noexperimental_retain_test_configuration_across_testonly` | other | build,test,run,cquery |
| `--noexperimental_rule_extension_api` | other | build,test,run,query,cquery |
| `--noexperimental_skyframe_target_pattern_evaluator` | other | build,test,run,cquery |
| `--noexperimental_starlark_type_syntax` | other | build,test,run,query,cquery |
| `--noexperimental_throttle_action_cache_check` | other | build,test,run,query,cquery |
| `--noexperimental_throttle_remote_action_building` | other | build,test,run,query,cquery |
| `--noexperimental_use_dex_splitter_for_incremental_dexing` | other | build,test,run,cquery |
| `--noexperimental_use_platforms_in_output_dir_legacy_heuristic` | other | build,test,run,query,cquery |
| `--nofetch` | other | build,test,run,query,cquery |
| `--nogenerate_json_trace_profile` | other | query,cquery |
| `--nograph:factored` | other | cquery |
| `--nogrpc_tcp_keepalive` | other | build,test,run,query,cquery |
| `--noguard_against_concurrent_changes` | other | build,test,run,query,cquery |
| `--noincompatible_allow_tags_propagation` | other | build,test,run,query,cquery |
| `--noincompatible_always_check_depset_elements` | other | build,test,run,query,cquery |
| `--noincompatible_always_include_files_in_data` | other | build,test,run,query,cquery |
| `--noincompatible_avoid_hardcoded_objc_compilation_flags` | other | build,test,run,cquery |
| `--noincompatible_bazel_test_exec_run_under` | other | query |
| `--noincompatible_builtin_objc_strip_action` | other | build,test,run,cquery |
| `--noincompatible_compact_repo_mapping_manifest` | other | query |
| `--noincompatible_disable_autoloads_in_main_repo` | other | build,test,run,query,cquery |
| `--noincompatible_disable_nocopts` | other | build,test,run,cquery |
| `--noincompatible_disable_objc_library_transition` | other | build,test,run,query,cquery |
| `--noincompatible_disallow_ctx_resolve_tools` | other | build,test,run,query,cquery |
| `--noincompatible_disallow_empty_glob` | other | build,test,run,query,cquery |
| `--noincompatible_do_not_split_linking_cmdline` | other | build,test,run,query,cquery |
| `--noincompatible_dont_enable_host_nonhost_crosstool_features` | other | build,test,run,cquery |
| `--noincompatible_enable_deprecated_label_apis` | other | build,test,run,query,cquery |
| `--noincompatible_enable_proto_toolchain_resolution` | other | build,test,run,query,cquery |
| `--noincompatible_enforce_config_setting_visibility` | other | build,test,run,query,cquery |
| `--noincompatible_enforce_starlark_utf8` | other | build,test,run,query,cquery |
| `--noincompatible_fail_on_unknown_attributes` | other | build,test,run,query,cquery |
| `--noincompatible_filegroup_runfiles_for_data` | other | build,test,run,query,cquery |
| `--noincompatible_fix_package_group_reporoot_syntax` | other | build,test,run,query,cquery |
| `--noincompatible_locations_prefers_executable` | other | build,test,run,query,cquery |
| `--noincompatible_merge_genfiles_directory` | other | build,test,run,query,cquery |
| `--noincompatible_multi_release_deploy_jars` | other | build,test,run,cquery |
| `--noincompatible_no_attr_license` | other | build,test,run,query,cquery |
| `--noincompatible_no_implicit_watch_label` | other | build,test,run,query,cquery |
| `--noincompatible_package_group_has_public_syntax` | other | build,test,run,query,cquery |
| `--noincompatible_package_group_includes_double_slash` | other | cquery |
| `--noincompatible_remove_ctx_bazel_py_fragment` | other | build,test,run,cquery |
| `--noincompatible_remove_ctx_py_fragment` | other | build,test,run,cquery |
| `--noincompatible_remove_legacy_whole_archive` | other | build,test,run,cquery |
| `--noincompatible_repo_env_ignores_action_env` | other | build,test,run,query,cquery |
| `--noincompatible_run_shell_command_string` | other | build,test,run,query,cquery |
| `--noincompatible_simplify_unconditional_selects_in_rule_attrs` | other | build,test,run,query,cquery |
| `--noincompatible_skip_genfiles_symlink` | other | build,test,run,cquery |
| `--noincompatible_target_cpu_from_platform` | other | build,test,run,query,cquery |
| `--noincompatible_unambiguous_label_stringification` | other | build,test,run,query,cquery |
| `--noincompatible_use_new_cgroup_implementation` | other | build,test,run,cquery |
| `--noincompatible_use_specific_tool_files` | other | build,test,run,cquery |
| `--noincremental_dexing` | other | build,test,run,cquery |
| `--nointerface_shared_objects` | other | build,test,run,cquery |
| `--nointernal_spawn_scheduler` | other | build,test,run,cquery |
| `--nointernal_starlark_utf_8_byte_strings` | other | build,test,run,query,cquery |
| `--noisatty` | default | build,test,run,query,cquery |
| `--nojava_deps` | other | build,test,run,cquery |
| `--nojava_header_compilation` | other | build,test,run,cquery |
| `--nokeep_backend_build_event_connections_alive` | other | build,test,run,query,cquery |
| `--nolegacy_whole_archive` | other | build,test,run,cquery |
| `--nonodep_deps` | other | cquery |
| `--noobjc_use_dotd_pruning` | other | build,test,run,cquery |
| `--noone_version_enforcement_on_java_tests` | other | build,test,run,cquery |
| `--nooutput_library_merged_assets` | other | build,test,run,cquery |
| `--nooutput_tree_tracking` | other | build,test,run,cquery |
| `--noproto:flatten_selects` | other | cquery |
| `--noproto:include_starlark_rule_env` | other | cquery |
| `--noproto_profile` | other | build,test,run,cquery |
| `--noremote_cache_async` | other | build,test,run,query,cquery |
| `--noremote_verify_downloads` | other | build,test,run,query,cquery |
| `--noreuse_sandbox_directories` | other | build,test,run,cquery |
| `--norewind_lost_inputs` | other | build,test,run,cquery |
| `--nosandbox_enable_loopback_device` | other | build,test,run,cquery |
| `--noshare_native_deps` | other | build,test,run,cquery |
| `--noshow_loading_progress` | other | build,test,run,query,cquery |
| `--noslim_profile` | other | query,cquery |
| `--nostart_end_lib` | other | build,test,run,cquery |
| `--notrim_test_configuration` | other | build,test,run,cquery |
| `--nouse_action_cache` | other | build,test,run,cquery |
| `--nouse_ijars` | other | build,test,run,cquery |
| `--objc_debug_with_GLIBCXX` | other | build,test,run,cquery |
| `--objc_enable_binary_stripping` | other | build,test,run,cquery |
| `--objc_generate_linkmap` | other | build,test,run,cquery |
| `--objccopt=x` | other | build,test,run,cquery |
| `--oom_message=x` | other | build,test,run,query,cquery |
| `--optimizing_dexer=//:x` | other | build,test,run,cquery |
| `--override_module=x` | other | build,test,run,query,cquery |
| `--override_repository=x` | other | build,test,run,query,cquery |
| `--per_file_ltobackendopt=//x@-y` | other | build,test,run,cquery |
| `--persistent_android_dex_desugar` | other | build,test,run,cquery |
| `--persistent_android_resource_processor` | other | build,test,run,cquery |
| `--persistent_multiplex_android_dex_desugar` | other | build,test,run,cquery |
| `--persistent_multiplex_android_resource_processor` | other | build,test,run,cquery |
| `--persistent_multiplex_android_tools` | other | build,test,run,cquery |
| `--platform_mappings=x` | other | build,test,run,cquery |
| `--portable_paths` | other | run |
| `--print_relative_test_log_paths` | other | test,cquery |
| `--process_headers_in_dependencies` | other | build,test,run,cquery |
| `--profile=/tmp/x` | other | query,cquery |
| `--proguard_top=//:x` | other | build,test,run,cquery |
| `--propeller_optimize=//:x` | other | build,test,run,cquery |
| `--propeller_optimize_absolute_cc_profile=x` | other | build,test,run,cquery |
| `--propeller_optimize_absolute_ld_profile=x` | other | build,test,run,cquery |
| `--proto:definition_stack` | other | cquery |
| `--proto:include_attribute_source_aspects` | other | cquery |
| `--proto:include_synthetic_attribute_hash` | other | cquery |
| `--proto:instantiation_stack` | other | cquery |
| `--proto:rule_classes` | other | cquery |
| `--proto_profile_path=//:x` | other | build,test,run,cquery |
| `--protocopt=x` | other | build,test,run,cquery |
| `--python_native_rules_allowlist=//:x` | other | build,test,run,cquery |
| `--python_path=x` | other | build,test,run,cquery |
| `--python_top=x` | other | build,test,run,cquery |
| `--python_version=x` | other | build,test,run,query,cquery |
| `--rc_source=x` | other | build,test,run,query,cquery |
| `--record_full_profiler_data` | other | query,cquery |
| `--record_metrics_for_all_packages` | other | build,test,run,query,cquery |
| `--redirect_local_instrumentation_output_writes` | other | build,test,run,query,cquery |
| `--relative_locations` | other | cquery |
| `--remote_analysis_json_log=x` | other | build,test,run,cquery |
| `--remote_bytestream_uri_prefix=x` | other | build,test,run,query,cquery |
| `--remote_download_symlink_template=x` | other | build,test,run,query,cquery |
| `--remote_downloader_header=x=y` | other | build,test,run,query,cquery |
| `--remote_grpc_log=/tmp/x` | other | build,test,run,query,cquery |
| `--remote_grpc_service_config=/tmp/x` | other | build,test,run,query,cquery |
| `--remote_proxy=x` | other | build,test,run,query,cquery |
| `--repositories_without_autoloads=x` | other | build,test,run,query,cquery |
| `--restart_reason=no_restart` | default | build,test,run,query,cquery |
| `--run_under=x` | other | query |
| `--runs_per_test_detects_flakes` | other | build,test,run,cquery |
| `--sandbox_add_mount_pair=x` | other | build,test,run,cquery |
| `--sandbox_base=x` | other | build,test,run,cquery |
| `--sandbox_explicit_pseudoterminal` | other | build,test,run,cquery |
| `--sandbox_fake_hostname` | other | build,test,run,cquery |
| `--sandbox_fake_username` | other | build,test,run,cquery |
| `--save_temps` | other | build,test,run,cquery |
| `--scl_config=x` | other | build,test,run,query,cquery |
| `--serialized_frontier_profile=x` | other | build,test,run,cquery |
| `--show_timestamps` | other | build,test,run,query,cquery |
| `--split_bytecode_optimization_pass` | other | build,test,run,cquery |
| `--starlark:expr=x` | other | cquery |
| `--starlark:file=x` | other | cquery |
| `--starlark_cpu_profile=x` | other | build,test,run,query,cquery |
| `--startup_time=0` | default | build,test,run,query,cquery |
| `--strategy_regexp=x` | other | build,test,run,cquery |
| `--strict_filesets` | other | build,test,run,query,cquery |
| `--strict_system_includes` | other | build,test,run,cquery |
| `--stripopt=x` | other | build,test,run,cquery |
| `--target_environment=//:x` | other | build,test,run,query,cquery |
| `--target_platform_fallback=x` | other | build,test,run,cquery |
| `--terminal_columns=80` | default | build,test,run,query,cquery |
| `--tool_tag=x` | other | build,test,run,query,cquery |
| `--tvos_cpus=x` | other | build,test,run,cquery |
| `--tvos_minimum_os=1.0` | other | build,test,run,cquery |
| `--unconditional_warning=x` | other | build,test,run,query,cquery |
| `--use_platforms_in_apple_crosstool_transition` | other | build,test,run,cquery |
| `--vendor_dir=/tmp/x` | other | build,test,run,query,cquery |
| `--verbose_visibility_errors` | other | build,test,run,query,cquery |
| `--visionos_cpus=x` | other | build,test,run,cquery |
| `--watchos_cpus=x` | other | build,test,run,cquery |
| `--watchos_minimum_os=1.0` | other | build,test,run,cquery |
| `--write_command_log` | other | build,test,run,query,cquery |
| `--xbinary_fdo=//:x` | other | build,test,run,cquery |
| `--xcode_version=x` | other | build,test,run,cquery |

Probes that fail for another reason (a converter error on the placeholder
value, a fixture limitation such as `--platforms=//:x`, a remote endpoint that
does not exist) are recognized options and are not listed.

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
