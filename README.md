# bazel-rs

**Experimental native Rust foundations for a Bazel-compatible build system. Not
a replacement for Bazel, and not full feature parity.** Compatibility target:
official Bazel **9.3.0**. No command delegates to upstream Bazel.

The starting implementation evaluates a narrow BUILD subset and executes native
local actions. It cannot build or test the main `bwllaming/code` application.
That checkout currently pins **9.1.1**, independently of this project's 9.3 target.

## Tooling

```sh
./scripts/setup-rust.sh
direnv allow
direnv exec . cargo test --locked
direnv exec . cargo clippy --locked --all-targets -- -D warnings
direnv exec . cargo build --locked --release
```

Rust **1.99.0**, rustfmt, Clippy, rust-analyzer and rust-src are pinned in
`rust-toolchain.toml`. Rustup and Cargo live under `.direnv/`; installation checks
the official rustup-init SHA256 and does not change shell startup files. `.envrc`
only sets paths and variables; entering the directory does not run installers.
Dependencies are locked in `Cargo.lock`. The pinned interpreter is preserved as
a [licensed, provenance-tracked source tree](docs/STARLARK_VENDOR.md). Linux is the
supported execution host.
After a release build, direnv also exposes `bazel-rs` on PATH. It never installs
a `bazel` alias or replaces the main repo's launcher.

### Global allocator (jemalloc)

The `bazel-rs` binary uses jemalloc as its Rust global allocator: cargo feature
`jemalloc`, on by default, through `tikv-jemallocator` 0.7.0 (bundling a jemalloc
5.3.1 snapshot) with jemalloc's default options. It is linked statically with
prefixed symbols, so only Rust allocations go to jemalloc, C code keeps glibc
malloc, and `ldd` shows no new library. Building it needs `make` and a POSIX `sh`
in addition to the C compiler that `ring` and `blake3` already require: the
`tikv-jemalloc-sys` build script runs jemalloc's bundled `configure` and `make`,
which adds about 90-100 s of CPU to a clean build.
Measured on `~/code` with `build --nobuild //src/beyondwork.ai/tools/oauth2token:all`,
warm, median of 4 runs: wall time fell from 46.1 s to 28.0 s (−39 %), CPU time
by the same 39 %, and peak RSS from 1386 MB to 1365 MB.

To opt out, build and test with `--no-default-features`, for example
`cargo build --locked --release --no-default-features` or
`cargo test --locked --no-default-features`. The binary then uses the system
allocator (glibc malloc), which heaptrack, valgrind and other `LD_PRELOAD` tools
need; this also avoids the `make` requirement. jemalloc fixes the page size when
it is configured, at build time. A binary that may run on an aarch64 kernel with
64 KiB pages must therefore be built with `JEMALLOC_SYS_WITH_LG_PAGE=16`, unless
it is built on such a host; otherwise it aborts at start-up. x86_64 is not
affected.

## Try the native subset

```sh
cd tests/fixtures/basic
../../../target/release/bazel-rs --ignore_all_rc_files query //:all
../../../target/release/bazel-rs --ignore_all_rc_files build //:upper
# Run again to exercise verified local action-cache hits.
../../../target/release/bazel-rs --ignore_all_rc_files build //:upper
```

Build stdout contains `label<TAB>absolute artifact path`. Outputs live in an
implementation-specific content-addressed cache; every executing build also
links them at their exec paths below the workspace's
`<cache>/worktrees/<name>-<hash>/execroot/_main` (`bazel-rs info
output_base`), so `$(bazel-rs info bazel-bin)/pkg/tool` names a built file
like Bazel's `bazel-bin` (docs/INFO.md; no convenience symlinks are created in
the workspace). Default cache: `<workspace>/bazel-rs-out`; override with the
startup option `--cache-dir=/absolute/path`.

**Progress on a terminal.** When stdout and stderr are a terminal,
`build`, `test` and `run` keep a Bazel-style status area at the bottom of the
screen (`Loading: N packages loaded`, `Analyzing: N targets (P packages
loaded, T targets configured)`, `[done / total] <running actions>; Ns
<strategy>`, in-flight fetches and downloads) and end with `INFO: Elapsed
time` and `INFO: Build completed successfully, N total actions`. Piped or
redirected output, `CI`, `--curses=no`, `--noshow_progress`, the startup
option `--quiet` or `BAZEL_RS_PROGRESS=0` turn it off, and the output is then
byte for byte the plain one; it never slows a build (docs/PROGRESS.md).

**Git worktrees share one cache.** In a linked git worktree (`git worktree
add`) the default cache is the main checkout's `bazel-rs-out` (found by
reading `.git`, no git process), so a new worktree's first build reuses the
repositories, actions and test results the other worktrees already have
instead of paying a cold build and another copy of the cache. What belongs
to one workspace — analysis snapshots, the test result index, the hidden
lockfile, the output tree `bazel-rs info bazel-bin` names, the execution
roots of `run` of tests — lives in `<cache>/worktrees/<name>-<hash>/` (the
native output base). Repositories whose content depends on
the worktree's path are kept per worktree. A worktree's old
`bazel-rs-out` is left alone with a one-line note (delete it to free the
space). Caveats — a branch that bumps Go or gazelle must not build for the
first time while another worktree builds; repositories watching files that
differ between branches re-run on each switch —
are in docs/WORKTREES.md. The cache is garbage-collected at the end of
commands (by age, a size budget and deleted worktrees; `bazel-rs gc` collects
now): docs/CACHE_GC.md.

`bazel-rs fetch` (target patterns, `--repo`, `--all`, `--configure`,
`--force`) materializes repositories without building; `bazel-rs clean`
empties the workspace's output tree and state (`--expunge`: its whole state
directory) but keeps the shared action cache and repositories, so the next
build relinks cache hits; `shutdown` does nothing (no server); `vendor` is
not implemented (docs/COMMANDS.md).

The analysis snapshot is on by default: a `build`, `test` or `run` saves its
analysis in the workspace's `<cache>/worktrees/<name>-<hash>/snapshots/`, and
a re-run whose recorded inputs are all unchanged skips loading and analysis.
`BAZEL_RS_SNAPSHOT=off` turns it off; `BAZEL_RS_SNAPSHOT=verify` checks every
hit against a full analysis when a result looks stale
(docs/ANALYSIS_SNAPSHOT.md).

Implemented:

- Workspace and package discovery; main and local external-repository labels, with a deliberately
  restricted ASCII path grammar and no symlink package directories. `.git`,
  `.direnv`, and `bazel-rs-out` are reserved directories. `.bazelignore` prefixes and
  REPO.bazel `ignore_directories()` patterns follow Bazel 9.3 (docs/IGNORED_DIRECTORIES.md).
- Real Starlark parsing/evaluation via `starlark` 0.14.2: expressions, constants,
  and comprehensions; BUILD-appropriate restrictions on definitions/control flow.
- Main and local-override repository `.bzl` loading: nested/aliased loads, frozen modules, load-cycle
  detection and legacy macros calling a Bazel-style `native` struct. Loaded
  symbols are file-local unless explicitly re-exported. `struct()` is available
  in `.bzl` files, not predeclared in BUILD files.
- Canonical/apparent repository mappings, lexical `.bzl` label/default resolution
  and repository-qualified visibility. CLI query/build first record root MODULE;
  dependency discovery and low-level source operations remain lazy. External source `File.short_path` is metadata
  (`../<canonical>/...`), distinct from its execution path (`external/<canonical>/...`).
- `glob` with `*`/`**`, sorted results, exclusions, directory filtering and
  package boundaries. Bazel 9.3 defaults to `allow_empty=False`; each inclusion
  pattern and the final result must match unless explicitly overridden.
- `filegroup(name, srcs)` and `genrule(name, srcs, outs, cmd)`; public/private,
  `__pkg__` and `__subpackages__` visibility, enforced across dependencies.
- Bazel 9.3 native rule classes at loading (`alias`, `config_setting`,
  `toolchain`, `toolchain_type`, `constraint_setting`/`constraint_value`,
  `platform`, `label_flag`/`label_setting`, `genquery`,
  `starlark_doc_extract`, `test_suite`, ...) with exact schemas, Bazel errors and
  `select()` on configurable attributes; query lists them, analysis fails
  explicitly except `filegroup`/`genrule`/`config_setting`, the alias-like
  `alias`/`label_flag`/`label_setting` and the platform rules
  `constraint_setting`/`constraint_value`/`platform` (docs/NATIVE_BUILD_RULES.md,
  docs/PLATFORMS.md).
- One native build configuration (`-c`, `--[no]stamp`, `--strip`,
  `--[no]force_pic`, `--define`, `--platforms`, `--host_platform`, Starlark
  build settings `--//pkg:flag=value`), and `select()` resolution against
  `config_setting` `values`/`define_values`/`flag_values`/`constraint_values`
  and `constraint_value` keys matched against the target platform
  (docs/CONFIGURATION.md, docs/PLATFORMS.md).
- `--use_target_platform_for_tests`, the default `test` exec group of Starlark
  test rules resolved against the genuine `@bazel_tools//tools/test`
  toolchains (built-in profile), `config_common.FeatureFlagInfo`,
  `config_feature_flag` and `config_setting(flag_values)` over feature flags
  as rules_python 1.x uses them; feature-flag transitions fail explicitly
  (docs/TEST_FLAGS.md).
- Exec groups: `rule(exec_groups)` with per-group toolchain resolution,
  `ctx.exec_groups`, automatic exec groups (`_use_auto_exec_groups`,
  `--incompatible_auto_exec_groups`), `ctx.actions.run`/`run_shell`
  `exec_group`/`toolchain`, per-group `exec_properties` and
  `exec_group_compatible_with`, `config.exec(exec_group = ...)` edges, on the
  host platform as the only execution platform (docs/EXEC_GROUPS.md).
- `package(...)` and `REPO.bazel` `repo(...)` arguments, including late
  default visibility, `licenses()`, and `exports_files`.
  Exports default to public regardless of package defaults. Implicit files inherit
  package defaults, matching Bazel 9.3's default flag setting.
- Native custom `rule` definitions with typed string/list/int/bool/label/output
  attributes, declaration snapshots, `None` fallback and definition-relative label defaults.
  Implementations run during analysis, never during query. Bazel's built-in
  attributes (`tags`, `visibility`, `testonly`, `deprecation`, `features`, ...)
  load with Bazel's schemas, drive `manual` filtering, the testonly error and
  deprecation warnings, and appear in `ctx.attr` (docs/COMMON_RULE_ATTRIBUTES.md).
- `DefaultInfo` and custom providers survive module freezing and retain identity
  across packages. Provider-carried generated files remain usable even when they
  are not default outputs. File values and depsets preserve artifact identity.
- Persistent-DAG depsets support all four orders and homogeneous File, string,
  Label, bool, int32 and immutable tuple elements, and immutable structs and
  provider instances with Bazel's equality (docs/DEPSETS.md). Native `Args` supports `add`,
  `add_all`, `add_joined`, registered top-level mappers and action-registration snapshots.
- `ctx.actions.declare_file`, binary-safe native `write`, `run_shell` and direct
  `run`. Shell arguments/environment are keyed; custom shell actions do not
  inherit genrule's strict-shell flags. Only requested output actions execute.
- `--keep_going`/`-k` for `build`, `test` and `run`: patterns and packages that
  fail to load, targets that fail to analyze and failing actions skip only what
  depends on them; everything else is built and tested, with Bazel's summary
  lines and exit codes (docs/KEEP_GOING.md).
- Build Event Protocol files for `build`, `test` and `run`
  (`--build_event_json_file`, `--build_event_binary_file`,
  `--build_event_text_file`, `--build_event_publish_all_actions`;
  `--bes_backend` is an explicit error; docs/BEP.md), `--profile=<file>[.gz]`
  Chrome trace profiles and the spawn execution logs
  (`--execution_log_compact_file`, `--execution_log_json_file`,
  `--execution_log_binary_file`; docs/PROFILE.md). All off by default.
- The `--quiet` startup option (`bazel-rs --quiet build …`, `startup --quiet`
  in rc files): only errors and the command's own output reach the console,
  like Bazel 9.3's server mode (docs/STARTUP_QUIET.md).
- `query`: Bazel 9.3's query language (every default function, set algebra,
  `let`, label/label_kind/build/package/location/minrank/maxrank/graph output,
  `--keep_going`, `--[no]implicit_deps`, `--[no]tool_deps`; docs/QUERY.md).
- `mod` with every Bazel 9.3 subcommand (graph, deps, all_paths, path,
  explain, show_repo, show_extension, dump_repo_mapping, tidy) and display
  option, on the built-in profile and every lockfile mode
  ([docs/MOD_COMMAND.md](docs/MOD_COMMAND.md)); `--registry=file:///absolute/path`
  filesystem registries: local metadata/MVS, nodep and multiple-version selection.
  `query` and `build` also accept filesystem `--registry` options. Local override
  roots and registry `source.json` type `local_path` roots are loadable and have
  scoped differential coverage. Neither mechanism downloads repositories.
- Dependency analysis, cycle detection, generated-file dependencies, and
  validation of the selected graph before executing actions.
- Genrule Make variables `$@`, `$<`, `$(SRCS)`, `$(OUTS)`, `$(RULEDIR)`, `$(@D)`,
  the eight location functions, the configuration's variables and
  `TemplateVariableInfo` from `toolchains` (docs/MAKE_VARIABLES.md), and escaped `$$`.
- Bounded parallel action DAG execution (`build --jobs=N`) with full preflight,
  generated-input dependency tracking and failure propagation. Default parallelism
  is the available CPU count capped at 32; the library's `build()` remains serial.
- Spawns use Bazel 9.3's argv and environment: genrules run
  `bash -c 'source …/genrule-setup.sh; CMD'` (`set -euo pipefail`) with
  their configuration's action environment (strict `PATH=/bin:/usr/bin:/usr/local/bin`
  by default, N42); custom shell actions are plain `bash -c`.
  SHA256 action keys use `native-action-v8`, their manifests `native-action-v9` (entries of
  earlier binaries are executed once more: docs/DIGEST_MEMO.md, "Upgrade note"); output-content and executable
  mode validation on cache hits; per-key cross-process locks; atomic publication.
- An http(s) or REAPI gRPC (`grpc://`, `grpcs://`, `unix:`) `--remote_cache`
  with Bazel's action keys (entries are shared with genuine Bazel both ways) and Bazel's credential helpers for the cache and the
  downloader ([`docs/REMOTE_CACHE.md`](docs/REMOTE_CACHE.md)); uploads need
  the explicit opt-in `BAZEL_RS_ALLOW_REMOTE_UPLOAD=1` (safety divergence).

Unknown attributes, options, rules, and unsupported commands fail rather than
being silently ignored. rc files are evaluated like Bazel 9.3 (system, workspace,
home, `$BAZELRC` and `--bazelrc` files, imports, `common`/`always`/command groups,
`--config`, `--announce_rc`; [`docs/BAZELRC.md`](docs/BAZELRC.md)); every
resulting option goes through the native parsers, and options the profile does not
model fail naming the rc file:line they came from. The options of real rc files
(`--repo_env` with Bazel's downloader proxy rules, relative message paths, Apple
SDK versions, test, remote and sandbox options, …) are covered in
[`docs/RC_OPTIONS.md`](docs/RC_OPTIONS.md). `capabilities` prints the machine-readable
scope in [`docs/capabilities.json`](docs/capabilities.json).

### Execution and cache limitations

**Trusted BUILD files and commands only. Actions execute sandboxed by default
on Linux** — like Bazel 9.3's default `sandboxed` strategy
([`docs/SANDBOX.md`](docs/SANDBOX.md)): a Landlock ruleset denies every
write outside the sandbox's writable paths (the action execroot, `TMPDIR`,
`/tmp`, `/dev/shm`, `--sandbox_writable_path` entries), so out-of-sandbox
writes fail with `EACCES` ("Permission denied") followed by Bazel's
`--sandbox_debug` suggestion. **The network stays allowed by default**
(`--sandbox_default_allow_network`, default true, like Bazel; deny with
`--nosandbox_default_allow_network` or the `block-network` tag, lift a denial
with `requires-network`). The sandbox is a filesystem-write restriction, not a
security boundary: there is no user/PID/mount namespace isolation (`/proc`
shows host processes), reads stay allowed everywhere except
`--sandbox_block_path` subtrees, and metadata operations (chmod, utime, ...)
are unrestricted. Escape hatches: `--spawn_strategy=standalone` (or
`processwrapper-sandbox`, which isolates nothing), the `no-sandbox`/`local`
tags, and `--sandbox_writable_path=<path>`. Sandbox isolation is not hermetic
execution. Each process gets exactly its Bazel action environment
(strict `PATH`, `--action_env`/`--host_action_env`, keyed inherited values and a
fresh `TMPDIR`; docs/ACTION_ENV.md) and nothing else from the caller, but
undeclared host tools/files, time and network responses are not tracked. Cache
correctness requires actions to depend only on declared inputs and a stable host
tool environment. Absolute temporary/working paths must not enter outputs.
Regular files and tree artifacts are supported; symlink outputs are real symlinks
and unresolved symlinks are unsupported (docs/ACTIONS_PARAMS.md). Inputs are
staged as read-only hardlinks to a content-addressed store and outputs are
published by hardlink/rename, never copied per action (docs/HARDLINK_STAGING.md). On action failure,
new scheduling stops and in-flight actions are joined. There is no process
cancellation/timeout or persistent analysis server yet. Globs do not follow
symlinks; potentially matching symlinks fail explicitly (Bazel can follow them).
`bazel-rs test` runs tests locally like Bazel 9.3 (docs/TEST_COMMAND.md; no coverage).
Full symbolic-macro schemas,
the full package-specification grammar and license metadata remain unsupported.
External loading supports the
bounded verified registry/archive, text-patch and pinned Git profiles described
in `docs/`. Direct `use_repo_rule` callbacks now have a bounded native file/template
executor; see [its exact limits](docs/REPOSITORY_RULES.md), including Latin-1
Starlark strings and binary-preserving templates. [Ordinary module extensions](docs/MODULE_EXTENSIONS.md)
support bounded non-isolated callbacks, typed tags and deferred repository
declarations; full extension/repository APIs and toolchains remain incomplete.
[Extension repository mapping](docs/EXTENSION_REPO_MAPPING.md) follows Bazel 9.3
(host module mapping + siblings + self), and an apparent repository a context
repository does not see is Bazel's deferred invalid label, failing only when used.
`module_ctx` shares repository_ctx's whole StarlarkBaseExternalContext surface
(download incl. `block = False`/`auth`/`headers`, extract, file, execute, which,
watch, a working directory for string paths, full `path` values, `tag_sort_key`)
without evaluator or result-size caps ([EXTENSION_CTX_SURFACE.md](docs/EXTENSION_CTX_SURFACE.md));
`--repository_disable_download` is a download policy, not an offline flag.
[MODULE.bazel.lock and registry caching](docs/REGISTRY_LOCKFILE.md) cover Bazel 9.3
`--lockfile_mode` semantics for registry hashes and yanked versions, the
content-addressed registry file cache, parallel registry fetching and the
native-only `--native-lockfile` target for read-only probes. Without `--registry`
the registry is Bazel's default `https://bcr.bazel.build/`. Fast warm probes of a
real workspace use `--lockfile_mode=update --native-lockfile=/private/MODULE.bazel.lock`
with a private `--cache-dir`: the checkout is never written, and hashed registry
files come from the cache. Native writes version-28 lockfiles without the
module-extension entries Bazel would regenerate.
[Build-setting defaults](docs/BUILD_SETTINGS.md) execute genuine implementations
and propagate native template-variable providers to explicit consumers; command-line
setting values (`--//pkg:flag=value`), `flag_values` matching and `label_flag`/`label_setting`
analysis follow [CONFIGURATION.md](docs/CONFIGURATION.md); implicit Make expansion remains
unsupported.
[Ordered dictionary attributes](docs/STRING_LIST_DICT.md) preserve typed values,
snapshot timing and frozen ownership across rules, repositories and extension tags.
[Label dictionaries and computed defaults](docs/ATTR_DICTS_COMPUTED_DEFAULTS.md)
cover `attr.string_keyed_label_dict`, `attr.label_list_dict`, private function
defaults and the retained `analysis_test_transition`.
[Glob symlinks and label-keyed repository dictionaries](docs/GLOB_SYMLINKS.md)
follow links in `glob()` like Bazel (cycles and infinite expansions included,
host-path reads only for repository-rule-generated repositories such as
`local_jdk`) and accept `attr.label_keyed_string_dict` in repository rules and tags.
[Runfiles](docs/RUNFILES.md) cover the analysis of `runfiles`, `ctx.runfiles`,
DefaultInfo runfiles/executables, FilesToRunProvider and executable/test rules;
[runfiles trees and `run`](docs/RUNFILES_TREES.md) build Bazel's runfiles
trees, `MANIFEST` and `_repo_mapping` files, stage tool runfiles into actions
and run executables (`bazel-rs run //target -- args`, `--script_path`).
[Genuine rules_shell](docs/SH_BINARY.md) `sh_binary`, `sh_test` and `sh_library`
analyze and build: `attr.label_keyed_string_dict` rule values, `allow_rules`,
`flags`, `coverage_common.instrumented_files_info` and the private-API allowlist.
[The rule context surface](docs/CTX_SURFACE.md) gives `ctx` Bazel's 36 members:
`ctx.fragments` with configuration fragments, `ctx.configuration`, output roots,
`ctx.var` and Make-variable expansion, with unmodelled members failing explicitly.
[Local repository metadata](docs/LOCAL_REPOSITORIES.md) retains the local flag and
provides the compatibility version only in MODULE-loaded native bindings.
[Extension introspection](docs/EXISTING_RULES.md) enforces active-callback phase
checks and preserves genuine `maybe()` duplicate-declaration behavior.
[Symbolic macros](docs/SYMBOLIC_MACROS.md) have a bounded name/visibility-only
profile with real expansion, definition ownership and source/visibility checks.
[Labels and target patterns](docs/LABEL_SYNTAX.md) follow Bazel 9.3's full
label grammar and error texts, `@repo` shorthand, `//...`/`:all`/`:*` patterns,
negative build patterns and package discovery over invalid directory names.
[Package specifications](docs/PACKAGE_SPECIFICATIONS.md) connect native groups,
provider-constrained consumers, membership and visibility checks.
[`.bzl` load visibility](docs/BZL_VISIBILITY.md) emulates `visibility()` and
per-requesting-package load checks with Bazel's exact diagnostics.
[Run-environment metadata](docs/RUN_ENVIRONMENT_INFO.md) supports native snapshots
and nested transport to explicit consumers, not automatic run/test environments.
[Root MODULE validation](docs/ROOT_MODULE_VALIDATION.md) records the root before
CLI target loading and reuses it without eagerly resolving unused declarations.
[Native sets](docs/STARLARK_SETS.md) are available in BUILD, MODULE and `.bzl`,
including ordered File sets transported through custom providers.
[Provider initializers](docs/PROVIDER_INIT.md) run `provider(init=...)` callbacks
and expose raw constructors that bypass them under the same provider key.
[Language-module globals](docs/CC_COMMON_LOADING.md) give `.bzl` loading Bazel's
`cc_common`/`apple_common`/`platform_common` shapes; C++ analysis is unsupported.
[C++ toolchain features](docs/CC_FEATURES.md) port Bazel's feature selection,
build-variable expansion and tool selection behind rules_cc's Starlark API.
[C++ toolchains](docs/CC_TOOLCHAIN.md) analyze rules_cc's genuine `cc_toolchain`,
`cc_toolchain_alias` and `cc_toolchain_config` rules (host `@local_config_cc`,
zig configs) in target and exec configurations; compile/link remain unsupported.
[rules_go transitions](docs/CONFIGURATION.md) run rules_go's genuine
`transition.bzl` with `--collect_code_coverage`, `--compilation_mode` and
`--platforms` as transition options; coverage instrumentation remains unsupported.
[rules_go cgo](docs/CGO.md) runs every `go_library`'s `cgo_context_data_impl`
path against the host `@local_config_cc` toolchain: cgo libraries, their
binaries (external link), tests and the cgo standard library build
byte-identical to genuine Bazel; since N60 genuine `cc_import`/`cc_library`
compile and link through rules_cc's Starlark ([docs/CC_ACTIONS.md](docs/CC_ACTIONS.md))
and the `c-archive`/`c-shared` go binaries build end to end (the c-shared
`.so` embeds its execroot path as the linker soname, so even genuine builds
differ across output roots).
[Scoped subrules](docs/SUBRULES.md) execute declared zero-attribute helpers with
real actions and guarded contexts; private dependency attributes remain unsupported.
[String methods](docs/STRING_METHODS.md) follow Bazel's UTF-8 byte-string case,
whitespace and `repr` rules, with invalid-UTF-8 result rows documented.
[Native JSON](docs/JSON.md) provides a dedicated bounded codec with exact numeric
handling rather than the interpreter's incompatible generic serialization path.
[Repository environment inputs](docs/REPOSITORY_ENVIRONMENT.md) track declared
and dynamic named reads without exposing a fabricated OS environment dictionary.
[Scalar string dictionaries](docs/STRING_DICT.md) preserve insertion order and
snapshot semantics through rule, tag and repository attribute consumers.
[Callback host metadata](docs/HOST_OS.md) provides the verified Linux/x86_64
name/architecture view and explicit host-dependent repository fingerprints.
[Resolved Label repository names](docs/LABEL_REPO_NAME.md) expose canonical
identity without changing owner mappings or hiding invalid-label timing limits.
[Label passthrough](docs/LABEL_PASSTHROUGH.md) preserves existing Label identity
across lexical contexts while retaining explicit invalid-label timing limits.
[Genuine built-in modules](docs/BUILTIN_MODULES.md) are injected by default,
like Bazel 9.3, using complete verified Linux x86-64 resources; the native-only
`--experimental-builtin-modules=false` selects the explicit-module profile
(no `@bazel_tools` contents). Load caching is per package evaluation,
not a persistent incremental graph.
Action logs spool to temporary files and bounded tails are replayed to stderr.

## Behavior changes

* **Nested `MODULE.bazel`/`REPO.bazel` are no longer boundaries (N28d).** Only
  `BUILD`/`BUILD.bazel` files delimit packages, as in Bazel 9.3, which has no
  nested repositories: directories below a nested `MODULE.bazel` or
  `REPO.bazel` (an example or vendored module, test fixtures) are now part of
  the enclosing repository, so `//...` patterns and `glob()` see their
  packages and files. List such directories in `.bazelignore` (or match them
  with `ignore_directories()` in the root `REPO.bazel`) to keep them out, as
  with Bazel (docs/IGNORED_DIRECTORIES.md, docs/GLOB_SYMLINKS.md).
* **`glob()` follows symlinks (N28d).** A main-repository glob reaching a link
  to a host path, such as Bazel's `bazel-*` convenience links, fails
  explicitly; ignore those links (see docs/GLOB_SYMLINKS.md).

## Differential checks and measurements

```sh
./scripts/fetch-bazel.sh
python3 scripts/validate.py --iterations 10
python3 scripts/validate_loading.py
python3 scripts/validate_custom.py
python3 -m unittest discover -s tests -p 'test_*.py'
```

The separately downloaded, SHA256-verified Bazel binary is a **test oracle only**.
The validation harness compares fixture query labels and generated bytes, checks
local cache reuse, and measures first invocation separately from repeated warm
queries. It also probes the small `~/code` filegroup `//src/docker:config` and
records the unsupported Go-rule loading boundary.

It bypasses the main repo's source-modifying/telemetry wrapper, disables known
telemetry and remote-write channels, uses isolated output bases, disables lockfile
updates and convenience symlinks, and shuts down only its own reference server.
It does not run deployment targets or the main repo's application test suite.
Raw JSON goes to `artifacts/validation.json`.

The loading suite adds a six-action macro-defined diamond, independent jobs=1/4
caches, source exports/defaults, visibility failures, frozen-global errors and
six allowlisted source-only targets from `~/code`. It records repository
status/diff hashes before and after. Results go to
`artifacts/loading-validation.json`; no repository script is executed.
See [`docs/LOADING_VALIDATION.md`](docs/LOADING_VALIDATION.md) for milestone scope.

The custom-rule suite records generated bytes/hashes, jobs=1/4 cold/warm action
counts, source invalidation, providers, direct execution, Args snapshots and
`None` defaults in `artifacts/custom-validation.json`. See
[`docs/CUSTOM_RULE_VALIDATION.md`](docs/CUSTOM_RULE_VALIDATION.md) for evidence,
timing caveats and its historical milestone capture.

Module graph evidence is in `artifacts/module-validation.json`. Local external
repository differential validation passes 20 scoped checks and is not implied by
module JSON parity. Its historical native application query failed on unavailable
registry metadata for `aspect_bazel_lib@2.22.5`. The later N3 runtime-pinned query
reached the JSON resource budget in effect before the decode-only JSON capacity
profile (`ac9d646`). By the N13–N15 promotion the native query of
`//src/beyondwork.ai/utils/str:str` succeeded with the expected single-label
output; a native build of that target then passed loading and stopped at
the first analysis gap (`label_flag`/configuration). Evidence: main commit `2d2bfd4` with installed release `79bc245f…`, probe
`artifacts/probe_application_v2.py` (native `query --experimental-builtin-modules
--registry=https://bcr.bazel.build --lockfile_mode=off //src/beyondwork.ai/utils/str:str`,
isolated HOME/TMPDIR/cache, tracked checkout state asserted unchanged): query exit 0,
receipt `artifacts/mainrepo-n13-n15-promoted-loading.json` (sha256 `64213a66…`);
the same probe with `build` exits 1 at the `label_flag` analysis gap, receipt
`artifacts/mainrepo-n13-n15-promoted-build.json` (sha256 `24a46217…`). Both receipts
live in the main checkout's ignored `artifacts/` directory and are referenced from
`artifacts/n13-n15-promotion.json`; no genuine Bazel capture of this query exists. With the configuration and `select()` increment (N16a) on this tree, the same
build gets past that `label_flag` boundary and first stops at `go_library`'s own
toolchain requirement (`rule(toolchains=[@@rules_go+//go:toolchain,
@@bazel_tools//tools/cpp:toolchain_type])`, toolchain resolution is not yet
implemented); its evidence receipt is recorded in `artifacts/n16a-m1-promotion.json`
at promotion. With toolchain resolution (N17) the build of that target entered toolchain
resolution, which expanded and analyzed every registered toolchain, and then
stopped while loading rules_cc's `cc_shared_library.bzl` (computed attribute defaults
at `cc/common/semantics.bzl:62`, being added); its receipt is recorded in
`artifacts/n17-n19-promotion.json` at promotion. With label dictionary attributes, computed defaults (N20) and the repository_ctx
host APIs (N21) that load succeeds and the build stops later, while loading rules_cc's
`fdo_context.bzl` (subrule `attrs`, `cc/private/rules_impl/fdo/fdo_context.bzl:430`);
its receipt is recorded in `artifacts/n20-n21-promotion.json` at promotion. With the exec transition (N18a/N18b), repr parity (N22) and subrule
attrs/toolchains/late-bound defaults (N24) that file loads and the build stops
later, while expanding registered toolchains: the `@build_stack_rules_proto`
http_archive needs `repository_ctx.patch()` (being added in N26); its receipt is
recorded in `artifacts/n18-n22-n24-promotion.json` at promotion. With runfiles/executable rules (N23a) and repository patches, lexer and
string semantics (N26) that archive is fetched and patched, and the build
then stopped while expanding the rules_java toolchain registrations
(`java_common.google_legacy_api_enabled`, an analysis-time Java API); its
receipt is recorded in `artifacts/n23a-n26-promotion.json` at promotion.
N28 (Java semantics getters, JavaOptions version strings, no-config
`package_metadata` analysis) and the later increments moved past that
boundary; the promotion receipts continuing the chain are
`artifacts/n18c-n25-n27-n28-promotion.json`,
`artifacts/n30-n23-n31-n32-n35-n36-promotion.json`,
`artifacts/n33-n37-n44-promotion.json` and
`artifacts/n51-n54-promotion.json` (integration-15: everything through the
lockfile fail-closed write gate).

On the current tree the demonstrated `~/code` application boundaries are:

- **Build/test of `~/code` targets succeeds with the built-in modules**
  (then opted into with `--experimental-builtin-modules`, now the default;
  and `--lockfile_mode=off`): `build
  //src/beyondwork.ai/utils/str:str //:gazelle_bin //:aws_script` exits 0,
  and `test //src/beyondwork.ai/utils/str:str_test` passes
  (`Executed 1 out of 1 test: 1 test passes.`). The last analysis boundary
  was the rules_go `go_library` `OutputGroupInfo(_validation=...)` shape,
  lifted by N56 (receipt
  `tests/fixtures/validation_actions/reference_receipt.json`; probe receipts
  `artifacts/i16-app-build-final.json`, `artifacts/i16-app-test-final.json`,
  isolated HOME/TMPDIR/cache, tracked checkout state asserted unchanged).
- **The explicit-module profile stops at `@@bazel_tools` no-contents**: a
  build with `--experimental-builtin-modules=false` (the default before the
  built-in modules became it) fails with `cannot load
  '@@bazel_tools//tools/build_defs/repo/utils.bzl': the built-in repository
  @@bazel_tools is visible to every module but has no contents in the
  explicit-module profile; drop --experimental-builtin-modules=false` (receipt
  `artifacts/i16-app-defaultprofile-final.json`, recorded with the former
  wording; docs/BUILTIN_MODULES.md).
- **Lockfile update mode is functional (fail-closed)** since N52: with
  `--lockfile_mode=update --native-lockfile=<private path>` a `~/code`
  query resolves, writes the private lockfile (`moduleExtensions` entries
  with Bazel's digests) and reuses it byte-identically on the warm run
  (`resolved N module extension(s)` prints only on an actual write);
  receipt `artifacts/i16-code-lockfile-update.json` and the paired genuine
  capture `tests/fixtures/lockfile_extensions/reference_receipt.json`
  (22/22 steps exact, including its `~/code` `code_probe` cold/warm rows).
  The write gate is fail-closed and stricter than Bazel: a command that
  fails during module resolution or extension resolution writes nothing
  (docs/REGISTRY_LOCKFILE.md).

Go build/test parity beyond these targets and full-repository performance
remain unestablished. See
[`docs/REPOSITORY_VALIDATION.md`](docs/REPOSITORY_VALIDATION.md) for implementation
scope, the source-preserving application probe and evidence boundaries.

Tiny-fixture latency is **not evidence of full-repo performance**. See
[`docs/ROADMAP.md`](docs/ROADMAP.md) for the missing engine and compatibility work.

## Source references

- [Official Bazel 9.3.0 release](https://github.com/bazelbuild/bazel/releases/tag/9.3.0)
- [Official release artifacts](https://releases.bazel.build/9.3.0/release/index.html)
- [Bazel architecture](https://bazel.build/contribute/codebase)
- [Starlark embedding API](https://docs.rs/starlark/0.14.2/starlark/)
- [Rustup project toolchains](https://rust-lang.github.io/rustup/overrides.html)
- [direnv](https://direnv.net/)
