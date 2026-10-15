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

## Try the native subset

```sh
cd tests/fixtures/basic
../../../target/release/bazel-rs --ignore_all_rc_files query //:all
../../../target/release/bazel-rs --ignore_all_rc_files build //:upper
# Run again to exercise verified local action-cache hits.
../../../target/release/bazel-rs --ignore_all_rc_files build //:upper
```

Build stdout contains `label<TAB>absolute artifact path`. Outputs live in an
implementation-specific content-addressed cache, **not** Bazel's conventional
`bazel-bin` tree. Default cache: `<workspace>/bazel-rs-out`; override with the
startup option `--cache-dir=/absolute/path`.

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
  Label, bool, int32 and immutable tuple elements. Native `Args` supports `add`,
  `add_all`, `add_joined`, registered top-level mappers and action-registration snapshots.
- `ctx.actions.declare_file`, binary-safe native `write`, `run_shell` and direct
  `run`. Shell arguments/environment are keyed; custom shell actions do not
  inherit genrule's strict-shell flags. Only requested output actions execute.
- Literal `query` patterns: labels, `//pkg:all`, `//pkg/...`, and `//...`.
- Scoped `mod graph --output=json` for explicit modules, with optional
  `--registry=file:///absolute/path` filesystem registries: local metadata/MVS,
  nodep and multiple-version selection. The captured module report passes 11
  exact reference JSON cases plus five native-only capability probes.
  `query` and `build` also accept filesystem `--registry` options. Local override
  roots and registry `source.json` type `local_path` roots are loadable and have
  scoped differential coverage. Neither mechanism downloads repositories.
- Dependency analysis, cycle detection, generated-file dependencies, and
  validation of the selected graph before executing actions.
- Genrule Make variables `$@`, `$<`, `$(SRCS)`, `$(OUTS)`, `$(RULEDIR)`, `$(@D)`,
  `$(location ...)`, `$(locations ...)`, and escaped `$$`.
- Bounded parallel action DAG execution (`build --jobs=N`) with full preflight,
  generated-input dependency tracking and failure propagation. Default parallelism
  is the available CPU count capped at 32; the library's `build()` remains serial.
- Genrules use `bash -e -o pipefail`; custom shell actions do not. SHA256 action
  keys use `native-action-v4`; output-content and executable
  mode validation on cache hits; per-key cross-process locks; atomic publication.

Unknown attributes, options, rules, and unsupported commands fail rather than
being silently ignored. Existing rc files require explicit
`--ignore_all_rc_files`: **this deliberately removes their configuration** and is
not a way to claim rc compatibility. `capabilities` prints the machine-readable
scope in [`docs/capabilities.json`](docs/capabilities.json).

### Execution and cache limitations

**Trusted BUILD files and commands only. There is no security sandbox.** Shell
actions can access the host and network. Working-directory isolation is not
hermetic execution. The action environment is cleared and explicitly set, but
undeclared host tools/files, time and network responses are not tracked. Cache
correctness requires actions to depend only on declared inputs and a stable host
tool environment. Absolute temporary/working paths must not enter outputs.
Only regular files are supported; no trees or symlink artifacts. On action failure,
new scheduling stops and in-flight actions are joined. There is no process
cancellation/timeout or persistent analysis server yet. Globs do not follow
symlinks; potentially matching symlinks fail explicitly (Bazel can follow them).
Full symbolic-macro schemas, executable/test rule modes, runfiles, tree artifacts,
the full package-specification grammar and license metadata remain unsupported.
External loading supports the
bounded verified registry/archive, text-patch and pinned Git profiles described
in `docs/`. Direct `use_repo_rule` callbacks now have a bounded native file/template
executor; see [its exact limits](docs/REPOSITORY_RULES.md), including ASCII
Starlark strings and binary-preserving templates. [Ordinary module extensions](docs/MODULE_EXTENSIONS.md)
support bounded non-isolated callbacks, typed tags and deferred repository
declarations; full extension/repository APIs and toolchains remain incomplete.
[Bounded module-extension I/O](docs/MODULE_EXTENSION_IO.md) documents the candidate
`module_ctx.download`/`read` implementation and `--repository_disable_download`
policy (not an offline flag); final acceptance and release remain pending.
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
[Local repository metadata](docs/LOCAL_REPOSITORIES.md) retains the local flag and
provides the compatibility version only in MODULE-loaded native bindings.
[Extension introspection](docs/EXISTING_RULES.md) enforces active-callback phase
checks and preserves genuine `maybe()` duplicate-declaration behavior.
[Symbolic macros](docs/SYMBOLIC_MACROS.md) have a bounded name/visibility-only
profile with real expansion, definition ownership and source/visibility checks.
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
[Genuine built-in modules](docs/BUILTIN_MODULES.md) are available through the
native-only `--experimental-builtin-modules` transition option, using complete
verified Linux x86-64 resources; the default remains explicit modules only.
Active extension usages fail in `mod graph` even when
hidden from output. Load caching is per package evaluation,
not a persistent incremental graph.
Action logs spool to temporary files and bounded tails are replayed to stderr.

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
profile (`ac9d646`). On the current tree, the native query of
`//src/beyondwork.ai/utils/str:str` succeeds with the expected single-label
output; a native build of that target passes loading and stops at the first
analysis gap (`label_flag`/configuration). Evidence: main commit `2d2bfd4` with installed release `79bc245f…`, probe
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
at promotion. With toolchain resolution (N17) the build of that target now enters toolchain
resolution, which expands and analyzes every registered toolchain, and currently
stops while loading rules_cc's `cc_shared_library.bzl` (computed attribute defaults
at `cc/common/semantics.bzl:62`, being added); its receipt is recorded in
`artifacts/n17-n19-promotion.json` at promotion. With label dictionary attributes, computed defaults (N20) and the repository_ctx
host APIs (N21) that load succeeds and the build stops later, while loading rules_cc's
`fdo_context.bzl` (subrule `attrs`, `cc/private/rules_impl/fdo/fdo_context.bzl:430`);
its receipt is recorded in `artifacts/n20-n21-promotion.json` at promotion. Application build/test, Go build/test
parity and full-repository performance remain unestablished. See
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
