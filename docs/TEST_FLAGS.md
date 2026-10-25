# Test toolchain flags and feature flags (Bazel 9.3, increment N30)

Two configuration gates that genuine rule sets reach once their registered
toolchains and flags are analyzed: the built-in test toolchain machinery of
`@bazel_tools//tools/test` behind `--use_target_platform_for_tests`, and
`config_common.FeatureFlagInfo` / `config_feature_flag` as rules_python 1.x
uses them. Everything else about feature flags fails explicitly.

Pinned source: `/tmp/opencode/bazel-9.3-reference` (e89755b, Release 9.3.0).
Paths are relative to `src/main/java/com/google/devtools/build/lib/`. Code:
`src/configuration.rs` (option, warning), `src/config_registry.rs` (exec
transition), `src/main.rs` (warning output), `src/toolchains.rs` and
`src/engine.rs` (test exec group, config_feature_flag analysis, planning of
Starlark flag keys), `src/feature_flags.rs` (provider, constructor, rule
semantics), `src/config_setting.rs` (flag_values), `src/native_rules.rs`
(rule class), `src/language_modules.rs`, `src/providers.rs`.

## `--use_target_platform_for_tests`

`@Option(name = "use_target_platform_for_tests", defaultValue = "false")`
in CoreOptions (`analysis/config/CoreOptions.java:1110-1123`), a boolean:

* Command line: `--use_target_platform_for_tests[=BOOL]`,
  `--nouse_target_platform_for_tests`, BooleanConverter errors
  (`While parsing option --use_target_platform_for_tests=maybe: 'maybe' is not
  a boolean`). It is a CoreOptions option, so `query` accepts and ignores it.
* Its `deprecationWarning` is printed once per invocation (the parser keeps a
  LinkedHashSet, `common/options/OptionsParserImpl.java:191, 336-369`) after
  the native options parsed (`runtime/BlazeOptionHandler.java:536-539`):
  `WARNING: Option 'use_target_platform_for_tests' is deprecated: Tests select
  an execution platform matching all constraints of the target platform by
  default. Instead of using this flag, make sure that all test target platform
  are registered as execution platforms.` — also for `=0`.
* `config_setting(values = {"use_target_platform_for_tests": ...})` matches it
  (ConfigSetting.optionMatches); `@bazel_tools//tools/test:BUILD`'s
  `use_default_test_toolchain` and
  `use_legacy_test_toolchain_due_to_use_target_platform_for_tests` do.
* The exec transition does not propagate it (not in CoreOptions' `propagate`
  list, `src/main/starlark/builtins_bzl/common/builtin_exec_platforms.bzl:231-275`):
  exec configurations have the default.
* The configuration identity serializes it only when true, so every existing
  identity is unchanged. TestActionBuilder's use of the option (the test
  action owner, `analysis/test/TestActionBuilder.java:157-170`) has no native
  counterpart: the host platform is the only execution platform of the
  `test` command (N23g, docs/TEST_COMMAND.md).

## The default test exec group

Every Starlark rule with `test = True` (also `analysis_test = True`) that
declares no `test` exec group gets
`RuleClass.DEFAULT_TEST_RUNNER_EXEC_GROUP` (`packages/RuleClass.java:153-167`,
`analysis/starlark/StarlarkRuleClassFunctions.java:1078-1082`): one optional
type, `@bazel_tools//tools/test:default_test_toolchain_type`
(`analysis/platform/PlatformConstants.java:27-28`), no execution constraints.
Natively:

* Its toolchain context is resolved after the rule's default context, with
  the same machinery as `rule(toolchains = ...)` (docs/TOOLCHAINS.md): the
  ToolchainContextKey has the type, no constraints and no forced execution
  platform (`analysis/producers/UnloadedToolchainContextsProducer.java:88-126`,
  `packages/DeclaredExecGroup.java:94-128`); only the first error is
  reported (`:181-186`). So a test rule analyzes the target and host
  platforms and every registered toolchain (D1), like Bazel: a broken
  registration fails it (`While resolving toolchains for target //tt:a_test
  (ID): invalid registered toolchain '//tt:notatoolchain': target does not
  provide the DeclaredToolchainInfo provider`), also when the rule has no
  `toolchains`.
* The genuine `@bazel_tools//tools/test` package (embedded bundle) supplies
  the toolchains: `default_test_toolchain` (`use_target_platform_constraints
  = True`, selected by `use_default_test_toolchain`: the private
  `bool_flag` `incompatible_use_default_test_toolchain` is true and
  `use_target_platform_for_tests` is false), `legacy_test_toolchain`
  (`--@bazel_tools//tools/test:incompatible_use_default_test_toolchain=false`)
  and `legacy_test_toolchain_use_target_platform_for_tests`
  (`--use_target_platform_for_tests`). Their `empty_toolchain`
  (`default_test_toolchain.bzl`, `platform_common.ToolchainInfo()`) is analyzed
  as a toolchain dependency (`analysis/DependencyResolutionHelpers.java:358-373`),
  then loaded like ResolvedToolchainContext.load: a registered toolchain for
  the type whose implementation lacks ToolchainInfo fails with `toolchain
  type @@bazel_tools//tools/test:default_test_toolchain_type resolved to
  target //tt:noinfo, but that target does not provide ToolchainInfo`.
  The type is optional, so no match (a target platform the host cannot
  satisfy) is not an error.
* Its ToolchainInfo is visible through `ctx.exec_groups["test"].toolchains`
  (N35, docs/EXEC_GROUPS.md: a declared `test` group replaces it,
  `config.exec(exec_group = "test")` edges use it, it counts for automatic
  exec groups' toolchain parameter); the
  test-runner implicit attributes (`$test_wrapper`, `$xml_writer`,
  `$test_runtime`, ..., whose exec transition targets this group) are
  analyzed as its `exec(test)` edges (N23g, docs/TEST_COMMAND.md). The execution platform is the host
  platform (the only one natively).
* **Divergence D3:** the group is resolved only in the built-in profile
  (the default). With `--experimental-builtin-modules=false` there is no `@bazel_tools`,
  so its type cannot exist; like the lazy default host platform (D2,
  docs/TOOLCHAINS.md) the group is then not resolved, and test rules keep
  building as before N30.

## `config_common.FeatureFlagInfo`

ConfigFeatureFlagProvider (`rules/config/ConfigFeatureFlagProvider.java:40-149`,
`starlarkbuildapi/config/ConfigFeatureFlagProviderApi.java:25-58`):

* `config_common.FeatureFlagInfo` is the provider (type `FeatureFlagInfo`,
  repr `<function FeatureFlagInfo>`, unchanged). `FeatureFlagInfo(value)`
  takes one positional-or-named string (`@Param(name = "value", named =
  true)`, `:82-90`); BuiltinFunction binding errors are Bazel's
  (`net/starlark/java/eval/BuiltinFunction.java:140-162, 325-386, 470-499`):
  `in call to FeatureFlagInfo(), parameter 'value' got value of type
  'NoneType', want 'string'`, `FeatureFlagInfo() missing 1 required
  positional argument: value`, `FeatureFlagInfo() got unexpected keyword
  argument 'other'` (with SpellChecker's `(did you mean 'value'?)`),
  `... accepts no more than 1 positional argument but got 2`, `... got
  multiple values for argument 'value'`.
* Instances: type `FeatureFlagInfo`, `dir()` `["error", "is_valid_value",
  "value"]`, repr/str `struct(error = None, value = "x")` (NativeInfo repr of
  the struct fields), `value` (None when there is an error), `error`,
  `is_valid_value(value)` (a `builtin_function_or_method`; always True for
  constructed instances, the allowed values for `config_feature_flag`;
  `in call to is_valid_value(), parameter 'value' got value of type 'int',
  want 'string'`), immutable (`cannot set .value field of FeatureFlagInfo
  value`). Equality and hash are identity (`:135-148`): two instances with the
  same value differ.
* Returned like any provider; `dep[config_common.FeatureFlagInfo]`, `in`,
  `attr.label(providers = [config_common.FeatureFlagInfo])` and
  `rule(provides = [...])` use the `builtin:FeatureFlagInfo` key.

## `config_feature_flag`

The native rule class (`rules/config/ConfigRuleClasses.java:455-499`;
loading schema in docs/NATIVE_BUILD_RULES.md): `allowed_values` (mandatory,
non-empty, order independent), `default_value`, `scope`; default
`tags = ["manual"]`; toolchain resolution disabled. Analysis
(`rules/config/ConfigFeatureFlag.java:109-235`, all errors reported at the
rule's location):

1. `in allowed_values attribute of config_feature_flag rule L: attribute must
   be non empty` (RuleContext.checkAttributesNonEmpty,
   `analysis/RuleContext.java:1531-1549`).
2. The `$allowlist_config_feature_flag` package group
   `@bazel_tools//tools/allowlists/config_feature_flag:config_feature_flag`
   (`ConfigFeatureFlag.java:56-64`) must contain the package
   (`analysis/Allowlist.java:88-99`); the genuine group is
   `packages = ["public"]`. Without the built-in profile the allowlist does
   not exist and the rule fails explicitly.
3. `in allowed_values attribute of config_feature_flag rule L: cannot contain
   duplicates, but contained multiple of ["a", "b"]` and `in default_value
   attribute of config_feature_flag rule L: must be one of ["a", "b"], but
   was "q"` (both reported).
4. The value is FeatureFlagValue.DefaultValue: no native transition sets
   feature flags and they are not command-line options (`--//ff:ff=b ::
   Unrecognized option: //ff:ff`, as in Bazel). The provider has the
   `default_value`, or, without one, value None and the deferred error
   `Feature flag L has no default but no value was explicitly specified.`
   The target builds no files; `dep[config_common.FeatureFlagInfo]` is the
   provider.

## `config_setting(flag_values = ...)`

UserDefinedFlagMatch (`rules/config/ConfigSetting.java:633-797`): a key with
ConfigFeatureFlagProvider is a feature flag even if it also has
BuildSettingProvider (rules_python's `string_flag` and python version flags
return both; the provider's value wins over the command-line value):

* a value the flag does not allow is the attribute error `error while parsing
  user-defined configuration values: 'zz' is not a valid value for
  '//ff:ff'`;
* a provider error is deferred: the match is InError and a select() over it
  reports `Unresolvable config_settings for configurable attribute "s" in
  L:` / `config_setting K is unresolvable because: Feature flag ... has no
  default but no value was explicitly specified.`;
* otherwise the provider's value is compared (diff got = expected, want =
  value).

A Starlark key's FeatureFlagInfo comes from its implementation, so natively
the key's implementation (and every target analyzed so far, in plan order)
runs before the config_setting is matched (`Engine::plan_pending`): Bazel's
config_setting depends on the configured key the same way.

Implementations therefore may run before the rest of phase 1 (analysis of
the remaining targets) has finished. Natively, Starlark implementations run
in phase 2 (`Engine::plan_target`, in post-order of the analysis plan) and
phase 1 only resolves configurations, select()s and dependency edges; Bazel
has no such split, a configured target's implementation runs as soon as its
dependencies are configured. Every target analyzed so far is complete (its
dependencies precede it in the plan), so planning them early gives the same
results as planning them later, and a flush plans exactly what ordinary
planning would: N27's prerequisite pass (provider, file and held-back
duplicate-label checks) runs before the implementation; dependencies use
their edge's configuration (exec, no-config or Starlark-transitioned, N18c)
and alias chains end in that configuration; a rule-transitioned key's
delegate hands its providers, files and runfiles back to the requested key;
runfiles (N23a) are built in the same post-order, and the requested targets'
source-directory runfiles gate (N27) still sees them. A flush cannot nest:
a `config_setting` with Starlark keys analyzed while action planning itself
runs (e.g. inside a deferred target platform's tree) fails explicitly
(`config_setting L: the implementation of a Starlark flag_values key is
needed during action planning ...`).

## Explicitly unsupported

* Transitions that set feature flags: `config_common.config_feature_flag_transition`
  (`config_common.config_feature_flag_transition is not implemented in the
  native profile`), Android's `feature_flags`; Starlark 1:1 transitions
  (N18c, docs/CONFIGURATION.md) write only Starlark build settings and the
  modelled native options, never feature flags.
* `--enforce_transitive_configs_for_config_feature_flag` (an unsupported
  option) and `transitive_configs` values (common-attribute gap,
  docs/COMMON_RULE_ATTRIBUTES.md): Bazel's tagged trimming
  (`ConfigFeatureFlagTaggedTrimmingTransitionFactory.java`).
* The test-runner implicit attributes and the `test` command (exec groups:
  docs/EXEC_GROUPS.md).

## Tests

* Unit: `src/feature_flags.rs` (rule errors, default states), the option rows
  of `src/configuration.rs`, `src/config_setting.rs` (feature flag matching).
* CLI: `tests/test_flags.rs`.
* Paired capture: `artifacts/test_flags-capture.py`, receipt
  `tests/fixtures/test_flags/reference_receipt.json` (evidence only).
