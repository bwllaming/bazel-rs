# Full-parity roadmap and acceptance gates

## Current state

This is a new native engine, not a Rust port of upstream's implementation and not
a Rust launcher around the Java server. Only the explicitly documented subset is
implemented. Full Bazel parity remains an extensive, multi-year engineering effort.

The engine now includes local `.bzl` loading, legacy macros, globbing, visibility,
parallel actions, and bounded custom-rule analysis. Custom providers, all depset
orders, attributes, Args, native writes and direct executable actions have
focused conformance coverage. An explicit local-module resolver implements MVS,
nodep discovery and multiple-version overrides. The captured module differential
report passes 11 exact JSON cases; five additional probes test native capability
rejections only. Local override repositories now support BUILD/`.bzl` loading,
canonical/apparent mappings, lexical labels and repository-qualified visibility.
Registry `local_path` source roots are implemented; the separate local repository
differential covers 20 scoped checks. Explicit HTTP registries and verified
tar/tar.gz/ZIP source materialization now have an implementation, including
mirrors, overlays, private staging and cache validation. See
`docs/REMOTE_REPOSITORIES.md` for its strict limits and verification profile.
These are portions of milestones 1–3, not complete gates: full Git/extension
semantics, archive overrides, complete patch semantics, lockfiles, configured
analysis, toolchains and the production execution/test stack remain incomplete.
Successful fixture or filegroup builds do not establish application parity.

The progression below records historical increment boundaries, not the current
application state. The earlier N3 runtime-pinned query reached the JSON resource
budget in effect before the decode-only JSON capacity profile (`ac9d646`). On the
current tree, the native query of `//src/beyondwork.ai/utils/str:str` succeeds
with the expected single-label output. A native build of that target passes
loading and stops at the first analysis gap (`label_flag`/configuration).
Evidence: main commit `2d2bfd4` with installed release `79bc245f…`, probe
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
@@bazel_tools//tools/cpp:toolchain_type])`, toolchain resolution was not yet
implemented then; N17 implements it with the host platform as the only
execution platform, docs/TOOLCHAINS.md); its evidence receipt is recorded in `artifacts/n16a-m1-promotion.json`
at promotion. With toolchain resolution (N17) the build of that target now enters toolchain
resolution, which expands and analyzes every registered toolchain, and currently
stops while loading rules_cc's `cc_shared_library.bzl` (computed attribute defaults
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
stops later while expanding the rules_java toolchain registrations
(`java_common.google_legacy_api_enabled`, an analysis-time Java API not yet
implemented natively); its receipt is recorded in
`artifacts/n23a-n26-promotion.json` at promotion. N28 implements the Java
semantics getters (N28a), the JavaOptions version strings in
`config_setting`/`select()` (N28b) and `package_metadata` analysis in the
no-config configuration (N28c), which the rules_java toolchain packages need
next. This
is a query and loading result only: application build/test and performance
remain unproven.

These application runs pass `--ignore_all_rc_files`. With rc evaluation (N31,
docs/BAZELRC.md) a native `build` or `query` in `~/code` without it evaluates the
checkout's `.bazelrc` and stops at the first unmodelled option, naming the file
and line. N40 ([RC_OPTIONS.md](RC_OPTIONS.md)) models or explicitly rejects
the rc options; N41 (docs/REMOTE_CACHE.md) implements its HTTP `--remote_cache`
and `--credential_helper` lines (with Bazel's action keys, so the cache is
shared with genuine Bazel) and N42 (docs/ACTION_ENV.md) the action
environment options. Before those, with the remote-cache lines removed in a
scratch copy, `query //src/beyondwork.ai/utils/str:str` accepted every rc
option and stopped at `aspect_bazel_lib@2.22.5` missing from the (empty)
native registries; `build` stopped at the rc Starlark flag
`@com_google_protobuf//bazel/toolchains:prefer_prebuilt_protoc`, on the same
missing module (the merged tree's ~/code probes are the parent's).

The previously captured native query of `//src/beyondwork.ai/utils/str:str` failed
before application analysis because `aspect_bazel_lib@2.22.5` is unavailable in
the supplied local registries (none); remote fetching was then unsupported. See
`docs/REPOSITORY_VALIDATION.md` and `artifacts/mainrepo-application-loading.json`.
The explicit-HTTPS follow-up now resolves remote MODULE files and reaches
`rules_swift@3.1.2`, where MODULE evaluation rejects `use_repo_rule`. This is a
new, observed compatibility blocker, not a successful application query. The
follow-up preserved the checkout and cleaned its isolated cache; evidence is
`artifacts/mainrepo-remote-loading.json`.
The subsequent direct-repository metadata increment records `use_repo_rule`
without executing it and advances to the unsupported `hermetic_cc_toolchain`
patch override (`artifacts/mainrepo-direct-loading.json`). See
`docs/DIRECT_REPOSITORY_DECLARATIONS.md`; generated repositories, genuine built-in
execution and application analysis remain separate, incomplete requirements.
The text-patch implementation now projects local MODULE patches during discovery
and applies registry/local patches to staged archive sources. Its explicit bounds
are documented in `docs/PATCHES.md`; 14 reference differential cases and 13
native-only checks pass. The application follow-up reaches the unsupported `rules_distroless` Git
override, without changing the checkout (`artifacts/mainrepo-patch-loading.json`).

Commit-pinned Git overrides now use an isolated system-Git process and a verified
private source cache; final patched MODULE bytes drive discovery. Mutable refs,
submodules and unrestricted symlinks remain unsupported. The initial Git fetch reached a
tracked internal symlink in `rules_distroless` and fails explicitly rather than
omitting it (`artifacts/mainrepo-git-loading.json`). Full source and build/test
compatibility therefore remain incomplete. See `docs/GIT_OVERRIDES.md`.

The preserved-file-link increment retains and validates internal relative Git
symlinks, including committed target bytes and cold/warm manifest identity.
The read-only application probe now preserves the required `rules_distroless`
link and advances to `@io_bazel_rules_nogo`, whose module extension has not been
executed. No extension repository is fabricated and the application query still
fails. See `docs/GIT_SYMLINKS.md` and
`artifacts/mainrepo-git-links-loading.json`.

Direct `use_repo_rule` execution now loads genuine frozen definitions, validates
every declaration in a demanded innate usage, and synchronously runs demanded
file/template callbacks. Generated names and label attributes remain owner-scoped;
publication follows complete execution and checked snapshots. ASCII-only evaluated
strings and the remaining API limits are explicit in `docs/REPOSITORY_RULES.md`.
This is not ordinary module-extension execution or application compatibility.

The next increment adds bounded non-isolated ordinary extension execution:
immutable BFS module/tag projections, complete callbacks, call-time repository
argument snapshots, deferred sibling label binding and complete import/spec
validation before demanded repository execution. See `docs/MODULE_EXTENSIONS.md`.
Metadata, facts, SDK operations and implicit built-in closure were still missing
at that increment; full compatibility remains unproven (see the current
application state above).

Genuine built-in discovery is now available as an opt-in native profile. It
injects only `bazel_tools` before selection, evaluates its unchanged official
MODULE and normal dependency closure, and publishes the complete official tools
tree through a native verified cache. The default transition, graph presentation,
registry/lockfile defaults and SDK APIs remain unfinished. See
`docs/BUILTIN_MODULES.md`; this is not full application or performance parity.

Default-only build settings now execute genuine skylib implementations and
propagate their providers into consumer actions. Native TemplateVariableInfo
preserves snapshot ownership and mutable copy-on-read dictionaries. This does
not implement CLI overrides, transitions, implicit Make variables or toolchains.
The preserved application loading probe next encounters the archive-source
`docs_url` field; see `docs/BUILD_SETTINGS.md` for scope and evidence.

The official 9.3.0 release was published 2026-10-07, with upstream tag commit
`e89755babf3e941bc1e9d8120b1cd48854ab9dfa`. The main checkout's `.bazeliskrc` pins
9.1.1. Changing that checkout's pin is outside this implementation.

## Why a launcher is insufficient

Bazel already uses a native C++ client and a persistent Java server. Replacing
the client alone does not replace Starlark, configured-target analysis, Skyframe,
action execution or JVM startup. A Rust implementation needs observable semantic
compatibility, not necessarily identical internals. Performance is a separate
acceptance criterion from correctness.

## Main-repo inventory (read-only inspection)

`~/code` has 407 tracked BUILD files and 11 tracked Starlark files, with 27 direct
Bzlmod dependencies. Representative workload:

- Go libraries/binaries/tests, strict deps, Gazelle, nogo/staticcheck, custom
  mocks, exec-configured tools and cgo.
- Python 3.12, pip/uv resolution, runfiles and venv/script bootstrapping.
- Node 24, pnpm, JS/TS, Next/Vite, generated directory outputs.
- Protobuf/gRPC, Java tooling, C/C++, Zig GNU-libc cross-toolchains.
- OCI images with amd64/arm64 transitions, packaging, custom repository downloads,
  archive extraction and patches.
- cquery/aquery, SkyQuery and xml/proto query output (`query` itself: N39,
  docs/QUERY.md), JSON/protobuf output, BEP consumers, test caching and
  architecture-dependent native artifacts.

The local checkout contains pre-existing modifications. Its wrapper can run
Gazelle and change source files after strict-dependency failures, and can publish
telemetry. Validation must keep using an explicit reference executable instead.

## Milestones

### 0. Native foundation (this slice)

Strict subset, explicit unsupported errors, deterministic fixture queries,
content-addressed local actions and a differential harness. Gate: unit and
integration tests; matching fixture artifacts against 9.3; source-preserving
main-repo filegroup smoke checks. No whole-repo speed claim.

### 1. Loading and repository graph

- `.bzl` loads, module freezing, load cycles, Bazel Starlark dialect/builtins,
  `glob`, `select`, package defaults, visibility/package groups, ignore files.
- Bzlmod resolution/lockfiles, canonical/apparent repo mappings, registry access,
  module extensions, repository rules, overrides, verified downloads and patches.
- Incremental filesystem graph, glob invalidation, package/repository boundaries.
- Command/startup options, rc imports/config expansion and environment semantics.

Implemented sub-slice: lazy local module selection; local override roots;
repository-aware package/label identity and loading; optional filesystem
registries for `query`, `build` and `mod graph`. Registry `source.json`
`local_path` support accesses existing source trees. Explicit HTTP(S) registries
work in every lockfile mode for query/build (N37, `docs/REGISTRY_LOCKFILE.md`:
MODULE.bazel.lock registry hashes, repository-cache reuse, parallel fetching,
Bazel's default `https://bcr.bazel.build/` registry; `mod graph` still requires an
explicit `--lockfile_mode=off`); supported archive sources
use verified download blobs and staged tree publication. The historical local
loading/source-root differential does not cover downloaded archives;
`scripts/validate_remote.py` is their separate validation surface. Neither profile
establishes extension-generated repository or application compatibility.

Gate: `query //...` and the repo's graph expressions match reference sets and
diagnostics; offline replay of pinned external repositories; repository scripts
cannot accidentally publish telemetry or change the checkout during testing.

### 2. Configured target and rule analysis

- Custom rules/attributes/providers, `depset`, aspects, `ctx.actions`, `Args`,
  executable tools, tree artifacts, runfiles and `FilesToRunProvider`.
- Configurations, platforms, toolchain resolution, exec groups, transitions,
  target compatibility constraints, proto/native rules and action analysis.
- Incremental dependency tracking, graph parallelism, cycle diagnostics.

Gate: cquery/aquery equivalence for representative Go/Python/Node/proto/native
and multiarch image analysis. Public/private visibility tests must include
negative cases; accepted attributes may never become silent no-ops.

### 3. Production execution

- Parallel DAG scheduling, cancellation, process supervision and timeouts.
- Linux sandboxing and declared tool identity; reproducible action environments.
- Remote Execution API/CAS/AC, disk-cache policy, persistent workers, resource
  scheduling, path/symlink semantics and crash recovery.
- Correct action keys across tool, config, environment and input changes.
- Content-addressed store maintenance (N36 follow-up): garbage collection of
  unreferenced `cache/cas/<sha256>-<mode>` blobs and a sweep of abandoned
  `.tmp-*` staging files left by killed processes; the store currently only
  grows (docs/HARDLINK_STAGING.md).

Gate: warm/cold/no-op/incremental builds with artifact and exit-code equivalence;
cache corruption and concurrent-process tests; reproducibility tests across hosts.

### 4. Test/run and ecosystem interfaces

- Test execution, exit code 3, sharding, timeouts, test env, runfiles, caching;
  `run`, `--run_under`, script-path generation.
- BEP, profiles, query output formats, target-pattern files, info/clean/shutdown.
- Remaining supported OSes, upstream rule families and CLI compatibility matrix.

Gate: safely selected real-repo Go/Python/Node/shell tests actually execute and
match reference; broader upstream integration/conformance suites pass. Passing
a filegroup build is not a substitute for any of these gates.

### 5. Performance acceptance

Measure identical inputs, configuration, outputs and cache states:

1. First installation/extraction, separately from cold server startup.
2. Warm server query/no-op builds, with raw samples and median/p95.
3. Single-source changes, BUILD changes, configuration changes and clean builds.
4. Large-repo analysis, filesystem pressure, memory, parallel build throughput.
5. Remote cache hits/misses, remote execution, persistent workers and contention.

Use independent output bases, fixed concurrency, warmups, interleaved trials and
multiple repetitions. Correctness gates must pass before presenting a speed ratio
as a compatible replacement result. Language choice alone is not evidence.
