# Local repository metadata and MODULE-native version

`repository_rule(local=True)` is retained as owned definition metadata through
freezing, aliases, reexports and ordinary-extension call recording. The explicit
boolean participates in both direct and ordinary repository provenance, including
`false`. It is not an attribute exposed in `ctx.attr`. `configure` (default
`False`) is retained the same way; in Bazel it only selects repositories for
`fetch --configure` and excludes them from vendoring
(RepositoryFetchFunction.java:511-527), none of which native supports, so ordinary
fetches are unaffected. It enters provenance only when `True`, keeping existing
keys unchanged. Declared and dynamic named environment inputs have a separate
[bounded profile](REPOSITORY_ENVIRONMENT.md), independent of the local flag.

## Native lifecycle bounds

Within one runtime, repeated demand for the same repository uses its completed
demand slot. Across native CLI invocations, a warm invocation computes the
repository specification first (rule identity, attributes, mappings, host
markers, declared environment values — `repository_store`, N61) and reuses the
existing publication without executing the callback when its recorded inputs
(templates, watched files/directories, observed environment) are unchanged;
any changed recorded input or specification executes the callback again.
`local=False` repositories reuse this way; `local=True` repositories never
reuse across invocations — they always execute again
(`RepositoryFetchFunction.shouldUseCachedRepoContents`: local repositories
never use cached contents while fetching is enabled, and a native invocation
is a fresh server; their unrecorded `which`/`execute`/executed-file
dependencies make stale reuse dangerous).
Unused repositories remain lazy; failed callbacks do not publish partial trees.

This is **not** full persistent-server parity (Bazel 9.3 keeps one resident
server per output base; native is a per-invocation client), but repository
publications now behave like Bazel's `external/<name>` directories: fetched
once, reused until a recorded input changes. Local rules follow the same
discipline natively rather than always refetching on warm commands.

The local flag grants no additional filesystem or network permissions. Existing
path, symlink, template-input, ASCII and resource guards remain in force. General
watch tracking, legacy `os.environ`, CLI environment overrides, fetch-disabled behavior, `fetch --configure` handling,
reproducible repository-content caches and vendoring are not implemented here.

## Version binding is phase-specific

MODULE-loaded `.bzl` files receive `native.bazel_version`, including transitive
loads, initialization, module extensions and repository callbacks. Frozen
functions retain the correct environment. The value is the native implementation's
compatibility target, currently `9.3.0`, not its Rust package version and not a
claim that all Bazel 9.3 features are implemented.

BUILD-loaded `.bzl` files deliberately lack that field. BUILD and MODULE files
themselves do not gain a `native` global. Separate loader environments prevent
the same transitive source from leaking MODULE bindings into BUILD evaluation,
even with one shared repository set.

One executable constant supplies the binding, embedded tools version and CLI
target text. Parsed capabilities are tested for agreement and continue to report
`full_feature_parity: false`.

## Genuine-helper proof boundaries

The unchanged `bazel_features` 1.42.1 fixture is used for complete generated-file
comparisons, not as evidence of the application's selected version. A separate
QA repository can copy helper outputs using existing label-template inputs and
feed them to real actions without modifying the genuine generated BUILD files.
That demonstrates repository execution and file consumption, not support for
every target declared by upstream BUILD code or every generated global symbol.
Loading a generated version value and a separately requested minimal provider
subset are likewise narrower than executing the complete features library.

`artifacts/local-repositories-validation.json` records six passing families and
39 phases on native/reference executables. It verifies all four complete helper
outputs, generated-version consumption, the minimal real provider consumer,
phase isolation, native callback counts and cleanup. All 41 original archive
files and seven directories remain unchanged. The QA-owned extension invokes
original helper definitions directly; it does not replace the upstream extension.

Pinned contracts and actual reference lifecycle observations are preserved in
`/tmp/opencode/independent-next-b-contract/local-repository-contract.json`.
Full application builds/tests, SDKs, configured analysis and performance parity
remain unfinished.

The application query at the local-repository increment advanced past construction
and failed at `native.existing_rule` in Bazel's genuine `maybe()` utility, called by
the upstream `bazel_features` extension. Thus helper-callback comparisons do not
claim complete upstream-extension execution. See
`artifacts/mainrepo-local-repositories-loading.json`; checkout preservation and
cleanup are recorded there.

The subsequent [extension introspection increment](EXISTING_RULES.md) executes
that original extension and advances to a missing `macro` global in generated
code. The separate proof boundaries above remain unchanged.
