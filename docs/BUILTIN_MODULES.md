# Genuine Bazel 9.3 built-in profile

Like genuine Bazel 9.3, `query`, `build`, `run` and `test` inject the built-in
modules by default: genuine Bazel 9.3 built-in module discovery (`bazel_tools`
and its MODULE closure) runs before evaluating the root MODULE.
`--experimental-builtin-modules` is a **native-only option** (not an upstream
Bazel flag; rejected in rc files, docs/BAZELRC.md):

- `--experimental-builtin-modules` and `=true` select the built-in profile, the
  default (a no-op, kept for existing command lines);
- `--experimental-builtin-modules=false` selects the **explicit-module
  profile**: `@bazel_tools` is mapped in every module but has no contents and
  its MODULE closure is not discovered (below). It is an escape hatch, not
  Bazel behaviour;
- any other value fails (`--experimental-builtin-modules expects true or
  false`).

The explicit-module profile was the native default until the built-in profile
became it; error texts of the explicit-module profile name the opt-out
(`drop --experimental-builtin-modules=false`). Its library default
(`module_graph::ResolverOptions::default()`) is unchanged.

Registry and lockfile policy are independent: the built-in closure resolves
through the selected registries (the default https://bcr.bazel.build/ without
`--registry`). Missing registry metadata is an error, not a reason to switch
profiles or omit genuine dependencies.

**`mod graph`.** Native `mod graph` presents the explicit-module graph by
default (as if `--experimental-builtin-modules=false`): built-in graph
presentation (`--include_builtin`, built-in versions selected by MVS) is not
implemented, and an explicit `--experimental-builtin-modules[=true]` there is
rejected (`builtin module graph presentation is unsupported; ...`) rather than
silently presenting a graph without built-ins. The command still rejects
active extensions. Version selection of a root whose dependencies overlap the
built-in closure (e.g. `platforms`) can therefore differ from genuine `bazel
mod graph` (which selects with the built-in closure).

Integration tests whose fixtures predate the default (offline file registries
without the built-in closure, explicit-profile error texts) select the
explicit-module profile through `tests/support/explicit_modules.rs`; tests of
the built-in profile use the offline fixture registry
(`tests/fixtures/builtin_modules/registry`) and seeded archives
(`tests/support/module_archives.rs`), never the network.

## Discovery and ownership

Only `bazel_tools` is implicitly injected. Its empty-version dependency is
appended after user declarations in every evaluated MODULE except the actual
`bazel_tools` discovery key. A root merely declaring that name is not exempt.
The name is reserved in both profiles, including aliases of ignored dev
dependencies (see below). The implicit dependency survives
`--ignore_dev_dependency`.

Declared dependencies and original source bytes remain separate from implicit
edges. Discovery, mappings and extension-catalog breadth-first order consume the
same ordered effective dependency sequence. The canonical name is `bazel_tools`,
not `bazel_tools+`. Other dependencies of its genuine MODULE participate in normal
discovery, nodep processing and version selection; they are not made universally
visible to other modules. In particular, `platforms`, `rules_java` and `rules_cc`
are not independently injected.

## `@bazel_tools` in the explicit-module profile (N51)

Bazel adds the built-in dependency to every module but `bazel_tools` itself
(`bzlmod/ModuleThreadContext.java:383-401`), so every module's mapping
(`Module.getRepoMappingWithBazelDepsOnly`, `Module.java:49-67`) has
`bazel_tools -> @@bazel_tools` (`ModuleKey.java:45-55`).
`BazelDepGraphFunction` (`:97-137`) resolves every module's extension labels
with that mapping. The explicit-module profile adds the same mapping entry to the
root and to every selected module. It does not discover the built-in MODULE or
its closure (`buildozer`, `rules_license`, ...), and it does not materialize any
contents:

- A non-root `use_extension("@bazel_tools//...")` or
  `use_repo_rule("@bazel_tools//...")` resolves in the extension usage table in
  both lockfile paths. Examples are rules_jvm_external 6.7's
  `remote_android_tools_extensions` and `http_file`. The extension is evaluated
  only on demand, and its repository rule is loaded only on demand.
- `Label("@bazel_tools//x")` in root and non-root `.bzl` files is
  `@@bazel_tools//x:x` with `repo_name` `bazel_tools`.
- Demanding the repository fails explicitly:
  `cannot load '@@bazel_tools//...': the built-in repository @@bazel_tools is
  visible to every module but has no contents in the explicit-module profile;
  drop --experimental-builtin-modules=false`.
- With `--lockfile_mode=update`, `registryFileHashes` is a subset of Bazel's.
  The undiscovered built-in closure is missing.
- In every profile, a module may not use the name itself. This covers a
  `bazel_dep` repo name (dev or not), `use_repo` (positional or keyword),
  `module(name=...)`, `module(repo_name=...)` and a repository rule. Native
  fails with Bazel's text (`ModuleThreadContext.java:115-125,383-401`):
  `The repo name 'bazel_tools' cannot be defined as a built-in dependency at
  <builtin> as it is already defined by a bazel_dep at <file>:<line>:<col>,
  'bazel_tools' is a built-in dependency and cannot be used by any 'bazel_dep'
  or 'use_repo' directive`.
  - The location is the call's left parenthesis.
  - The file is the absolute root `MODULE.bazel`, the registry URL, or
    `@@<name>+//:MODULE.bazel` for non-registry overrides
    (`ModuleFileFunction.java:752-777`).
  - Profile-free recording (`bzlmod::evaluate*`) still permits the name.

**Residual divergence.** Native resolves the module graph lazily, so a command
that never resolves it can succeed where Bazel fails. Examples are a
dependency's `bazel_tools` name collision or a broken extension label, in a
main-repository-only query without a lockfile session. Whenever the graph is
resolved, the MODULE checks and Bazel's eager extension usage table apply in
every lockfile mode (docs/REGISTRY_LOCKFILE.md, "Eager resolution boundary").

**Intentional repository identity re-key (N51).** Native repository provenance
serialises the repository mapping. Adding `@bazel_tools` to every
explicit-module-profile mapping (then the default) therefore gave every such
direct and extension-generated repository a new native cache identity.
`tests/repository_rules.rs` re-pins its two goldens. Retained explicit-profile
native identity evidence is historical and does not describe the current
binary. Examples are the label_passthrough publication observation
`2fb9dd9d…` (`tests/fixtures/label_passthrough/native_publication_observation.json`)
and the V-series identity tables in earlier receipts. Built-in-profile mappings
already contained the entry, so this change does not re-key them.

Evidence comes from a paired capture against genuine Bazel 9.3.0, plus a
follow-up check: `tests/fixtures/eager_resolution_mapping/reference_receipt.json`. With the
built-in profile, all 6 steps match exactly. In the explicit-module profile, the
outputs of 4 steps match exactly. The lockfile is a subset, and the `.bzl` load
from `@bazel_tools` fails explicitly.

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

MODULE.bazel.lock entries that genuine Bazel recorded with
`REPO_MAPPING:bazel_tools,…` inputs (lookups inside `@@bazel_tools`'s `.bzl`
closure) look outdated outside this profile, where `@@bazel_tools` has no
contents: `update` re-evaluates, `error` fails explicitly where Bazel passes
(docs/REGISTRY_LOCKFILE.md, "Native limits").

This profile supplies real resources and dependency closure, not SDK execution,
unrestricted repository contexts, complete built-in rules, toolchains, lockfiles,
or successful whole-application builds/tests. Genuine loaded definitions still
fail explicitly when they use unsupported APIs. No extension imports or
transitive loads are suppressed to make an application appear to work.

Genuine built-ins are now the default of query/build/run/test; built-in graph
presentation (`mod graph`) remains unimplemented (above). Startup or
performance improvements are not inferred from this functionality.

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
