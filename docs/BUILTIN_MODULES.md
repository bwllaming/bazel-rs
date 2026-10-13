# Genuine Bazel 9.3 built-in profile

`--experimental-builtin-modules` is a **native-only transitional option** for
`query`, `build`, and `mod graph`. It selects genuine Bazel 9.3 built-in module
discovery before evaluating the root MODULE. `=false` retains the existing
explicit-module profile, which remains the default. This option is not claimed
to be an upstream Bazel flag or complete default-semantics parity.

Registry and lockfile policy are independent: supply explicit registries, and
use `--lockfile_mode=off` for remote registries. Missing registry metadata is an
error, not a reason to switch profiles or omit genuine dependencies. The current
`mod graph` command still rejects active extensions. Even if MODULE overrides
remove those usages, genuine-profile graph presentation is explicitly rejected;
it never silently includes built-ins while accepting `--noinclude_builtin`.
Selecting this profile does not implement `--include_builtin` presentation.

## Discovery and ownership

Only `bazel_tools` is implicitly injected. Its empty-version dependency is
appended after user declarations in every evaluated MODULE except the actual
`bazel_tools` discovery key. A root merely declaring that name is not exempt.
The name is reserved while the MODULE recorder still retains collisions,
including aliases of ignored dev dependencies. The implicit dependency survives
`--ignore_dev_dependency`.

Declared dependencies and original source bytes remain separate from implicit
edges. Discovery, mappings and extension-catalog breadth-first order consume the
same ordered effective dependency sequence. The canonical name is `bazel_tools`,
not `bazel_tools+`. Other dependencies of its genuine MODULE participate in normal
discovery, nodep processing and version selection; they are not made universally
visible to other modules. In particular, `platforms`, `rules_java` and `rules_cc`
are not independently injected.

## Complete static resources, native extraction

The Linux x86-64 resource bundle contains the complete official Bazel 9.3.0
installed `embedded_tools` tree: 421 regular files totaling 73,326,972 bytes.
This includes the bundled JDK, legal notices and compiled helper tools. The
packaging script reads the SHA256-verified official distribution as static
archive data during development. The runtime embeds these resources and neither
looks up nor executes the upstream Bazel executable or Java build engine.

The unmodified `MODULE.bazel` is available for discovery without extracting the
full tree. Demanding the repository uses a separate digest-addressed built-in
cache beneath the configured repository cache. Publication is staged and locked;
warm trees are checked against the distributed manifest rather than trusting a
mutable cache manifest. Direct repository file/template limits are unchanged.
The resource provenance and reproduction recipe are in
`resources/bazel_tools/PROVENANCE.md`.

Module metadata and repository filesystem precedence are separate. A supported
MODULE override can change discovered metadata for `bazel_tools`; canonical
repository access still selects the genuine embedded tree, matching the pinned
special repository rule. `--override_repository` is not implemented and is
rejected, not approximated with a MODULE override.

## Remaining boundaries

This profile supplies real resources and dependency closure, not SDK execution,
unrestricted repository contexts, complete built-in rules, toolchains, lockfiles,
or successful whole-application builds/tests. Genuine loaded definitions still
fail explicitly when they use unsupported APIs. No extension imports or
transitive loads are suppressed to make an application appear to work.

Making genuine built-ins the default requires a separate migration of graph
presentation, registry/lockfile defaults and real command fixtures. The
transitional option is not completion of that migration. Startup or performance
improvements are not inferred from this functionality.

## Observed application boundary

The verified candidate passes 725 Rust tests and 360 Python tests, including
independent captured selections for empty-root and older-platforms cases.
`artifacts/builtin-modules-validation.json` records two genuine Bazel CLI
comparisons and six native boundary/cache cases. A standalone copied native
executable is traced with `strace`; the query starts no upstream process.
The helper-copy comparison exposed and now verifies the regular action-output
[permission normalization](ACTION_OUTPUTS.md), rather than relaxing its checks.

The read-only application query with this profile advances beyond the absent
built-in mapping and fails on the unimplemented `platform_common` global in
`bazel_skylib`'s `rules/common_settings.bzl`. The required Git file link is
preserved, checkout fingerprints are unchanged and scratch is removed. See
`artifacts/mainrepo-builtin-module-loading.json`. This is not a successful whole
application query/build/test, nor proof that its SDK/toolchain paths execute.
