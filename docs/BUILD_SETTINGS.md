# Build-setting execution

This native profile implements typed declaration defaults and actual analysis
callbacks for build settings. Since N16b, `flag` and `repeatable` drive the
command-line parser (`--//pkg:flag=value`) and `ctx.build_setting_value`
receives the configured value; docs/CONFIGURATION.md describes the options,
`config_setting(flag_values = ...)` and the native BuildSettingProvider.
Since N18c, Starlark 1:1 transitions read and write build settings
(docs/CONFIGURATION.md "Starlark transitions"); split transitions remain
unsupported.

## Supported declarations and analysis

The `.bzl` `config` namespace provides keyword-only `bool`, `int`, `string` and
`string_list` constructors. Each accepts `flag=False`; string lists also accept
`repeatable=False`, with repeatability requiring `flag=True`.
`config.string(allow_multiple=False)` is accepted, while `True` remains explicitly
unsupported. String-set constructors are not implemented; label-typed settings are the
native `label_flag`/`label_setting` rules, which are analyzed (docs/CONFIGURATION.md).

`rule(build_setting=descriptor)` injects a mandatory typed
`build_setting_default` and a string `help` attribute, rejecting collisions with
explicit schemas. Missing and `None` defaults fail. Boolean setting defaults
accept booleans or integers 0/1, like every `attr.bool` value given in a BUILD
call (Type.BOOLEAN.convert, packages/Type.java:409-429; N39: rules_java's
`remote_coverage_tools` passes `create_executable = 1`).
Integer defaults use the signed 32-bit declaration range. Defaults are owned
invocation-time snapshots, not shared mutable rule metadata.

Setting implementations receive `ctx.build_setting_value`: the command-line
value when one differs from the default, else the default
(`StarlarkRuleContext.java:738-757`). Each list-valued access creates a new
mutable list from that value: modifying one
read cannot change later reads, another target, or the stored default. The
ordinary context fields continue to work. Non-setting rules do not acquire this
property. `cfg=None` remains omission-equivalent; non-`None` configurations and
transitions are unsupported. An explicitly specified string `scope` attribute
decides whether a command-line value reaches exec configurations (`target`
resets it; docs/CONFIGURATION.md "Exec transition"); exec-transition
attributes of a build-setting rule are no-ops (no execution platform).

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
ownership, target membership/indexing, duplicate-provider validation and nesting
in other providers' payloads use the normal native analysis paths.

Consumers may explicitly read this provider from a declared dependency and use
its values in actions. Producing it from a resolved toolchain or a `toolchains`
prerequisite feeds `ctx.var`, `ctx.expand_make_variables` and genrule commands
(docs/CTX_SURFACE.md, docs/MAKE_VARIABLES.md)
(`ctx.toolchains` is toolchain resolution, docs/TOOLCHAINS.md). Other `platform_common` fields are not placeholder implementations.

## Configuration and cache boundaries

`--//:flag=value`, the positive/negative boolean forms, alias resolution,
`select()` on `flag_values` and the configuration identity are N16b
(docs/CONFIGURATION.md). Starlark 1:1 transitions (N18c) and the exec
transition (N18b) analyze targets in further configurations; split
transitions remain unsupported. Defaults and command-line values flow through actual providers
into action bytes/arguments and existing content-based cache keys (output
paths do not change with Starlark settings, like Bazel's top-level
configuration). The default-change cache round trip below predates N16b.
`scripts/validate_build_settings.py` retires the three CLI-option probes of
its `native.unsupported` family (`--//:string_flag=blue`, `--//:bool_flag`,
`--no//:bool_flag`): they are never executed and are recorded as
`SUPERSEDED` with the reason "superseded by N16b: ..." (`SUPERSEDED`,
`SUPERSEDED_REASON`); a report passes only with exactly that record. The
other boundary probes (`allow_multiple`, `string_set`, legacy `True`) remain.

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
