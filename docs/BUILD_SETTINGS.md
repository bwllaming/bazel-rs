# Default-only build-setting execution

This native profile implements typed declaration defaults and actual analysis
callbacks for build settings. It is **not CLI flag/configuration parity**. The
`flag` and `repeatable` options are retained descriptor metadata; command-line
setting overrides remain explicit errors, not silently ignored options.

## Supported declarations and analysis

The `.bzl` `config` namespace provides keyword-only `bool`, `int`, `string` and
`string_list` constructors. Each accepts `flag=False`; string lists also accept
`repeatable=False`, with repeatability requiring `flag=True`.
`config.string(allow_multiple=False)` is accepted, while `True` remains explicitly
unsupported. String-set and label-setting constructors are not implemented.

`rule(build_setting=descriptor)` injects a mandatory typed
`build_setting_default` and a string `help` attribute, rejecting collisions with
explicit schemas. Missing and `None` defaults fail. Boolean setting defaults
accept booleans or integers 0/1; this does not relax ordinary `attr.bool` rules.
Integer defaults use the signed 32-bit declaration range. Defaults are owned
invocation-time snapshots, not shared mutable rule metadata.

Setting implementations receive `ctx.build_setting_value`. Each list-valued
access creates a new mutable list from the invocation snapshot: modifying one
read cannot change later reads, another target, or the stored default. The
ordinary context fields continue to work. Non-setting rules do not acquire this
property. `cfg=None` remains omission-equivalent; non-`None` configurations and
transitions are unsupported.

The genuine `bazel_skylib` 1.8.2 `rules/common_settings.bzl` file is used unchanged
to exercise all nine exported setting rules. Its implementations—not native
special cases—create `BuildSettingInfo`, enforce allowed strings and validate
Make-variable names. Provider-only targets need not produce files or actions.
The fixture version is not an assertion about the application's selected version.

## Native template-variable provider

`platform_common.TemplateVariableInfo(vars)` accepts one positional or named
`vars` dictionary with string keys and values. It snapshots entries in insertion
order. Every `.variables` access returns a fresh mutable dictionary; mutation of
the constructor input or a returned dictionary never changes provider storage.

The constructor has stable native provider identity across modules and aliases;
instances use identity equality/hash, not mapping equality. Frozen provider
ownership, target membership/indexing, duplicate-provider validation and nested
provider payload traversal use the normal native analysis paths.

Consumers may explicitly read this provider from a declared dependency and use
its values in actions. Producing it does **not** implement implicit Make-variable
expansion, `ctx.var`, `ctx.toolchains`, platform selection, or genrule toolchain
dependencies. Other `platform_common` fields are not placeholder implementations.

## Configuration and cache boundaries

`--//:flag=value`, positive/negative boolean CLI forms, aliases, transitions,
`select`, configuration matching and multi-configuration analysis remain
unsupported. This increment adds no partial option parser or mutable global
configuration. Analysis consumes declaration defaults; changed defaults flow
through actual providers into action bytes/arguments and existing content-based
cache keys. A default-change cache round trip is not a CLI-override test.

Reference mutation contracts and fixture provenance are retained in
`artifacts/build-settings-mutability-contract.json` and
`tests/fixtures/build_settings/`. Successful loading or these scoped fixture
builds do not establish whole-application build/test or performance parity.

Verification includes 741 Rust tests, 388 Python tests, and nine validation
families covering 55 probes and 104 actual engine phases. Eight families compare
with genuine Bazel 9.3; the remaining family checks explicit native boundaries.
See `artifacts/build-settings-validation.json` and the verification manifest.

The read-only application probe advances past `common_settings.bzl` and now
fails on the unsupported archive-source `docs_url` field while demanding
`bazel_features`. Its checkout fingerprints and required Git symlink remain
unchanged, and scratch is removed. The actual application's selected skylib
version is not inferred from the separately pinned fixture.
See `artifacts/mainrepo-build-setting-loading.json`.
