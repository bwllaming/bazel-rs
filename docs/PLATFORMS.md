# Platforms and constraints (Bazel 9.3, increment N16c)

Native analysis of `constraint_setting`, `constraint_value` and `platform`,
the build's target platform, `config_setting(constraint_values = ...)` and
`constraint_value` targets used directly as `select()` keys. Toolchain
resolution against these platforms is N17 (docs/TOOLCHAINS.md); execution
platform registrations stay explicit failures (exec groups: docs/EXEC_GROUPS.md).

Pinned source: `/tmp/opencode/bazel-9.3-reference` (e89755b, Release 9.3.0).
Paths are relative to `src/main/java/com/google/devtools/build/lib/`. Code:
`src/platforms.rs` (providers, platform construction, matching),
`src/engine.rs` (prerequisites, target platform resolution, select() keys),
`src/config_setting.rs` (`constraint_values`), `src/configuration.rs`
(`--platforms`/`--host_platform`).

## Native providers

| Provider | Native model | Source |
| --- | --- | --- |
| ConstraintSettingInfo | label, `default_constraint_value` label, refined ConstraintValueInfo; equal by label | `analysis/platform/ConstraintSettingInfo.java:62-112` |
| ConstraintValueInfo | setting + label; equal when both are | `analysis/platform/ConstraintValueInfo.java:76-118` |
| ConstraintCollection | parent + one value per setting (first wins); `get` = own, parent's, else the setting's default | `analysis/platform/ConstraintCollection.java:83-97, 125-145, 168-217, 305-364` |
| PlatformInfo | label, constraints, remote/exec properties, flags (parent's first), required settings, toolchain-type checks, missing-toolchain message | `analysis/platform/PlatformInfo.java:217-452`, `PlatformProperties.java:61-90` |

The providers are forwarded by `alias`, `label_flag` and `label_setting`.
Starlark sees them as `dep[platform_common.ConstraintSettingInfo]`,
`dep[platform_common.ConstraintValueInfo]`, `dep[platform_common.PlatformInfo]`
and `platform_common.X in dep`, and `attr.label(providers =
[platform_common.ConstraintValueInfo])` accepts `constraint_value` targets
(`src/platform_providers.rs`). Without `--experimental_platforms_api` (not a
native option) the only visible field is
`ConstraintSettingInfo.has_default_constraint_value`
(`starlarkbuildapi/platform/ConstraintSettingInfoApi.java:36-56`); `label`,
`constraint`, `default_constraint_value` and `constraints` are flag-gated
(`ConstraintValueInfoApi.java:37-51`, `PlatformInfoApi.java:42-56`). Reprs
follow the providers (`ConstraintValueInfo(setting=//os:os, //os:linux)`).
`ctx.target_platform_has_constraint(cv)`
(`analysis/starlark/StarlarkRuleContext.java:902-904`,
`analysis/RuleContext.java:1144-1150`) is the target platform's value for the
setting (defaults included) compared with `cv`; rules without toolchain
resolution (Starlark build settings) get `False`. When the default host
platform is still unresolved, the implementation's analysis stops at the
call, the platform is resolved and the rule is analyzed again; `print()`
output of the discarded attempt is dropped, so it appears once. A non-ConstraintValueInfo
argument is `in call to target_platform_has_constraint(), parameter
'constraintValue' got value of type 'T', want 'ConstraintValueInfo'`.

## Rules

All three classes are analyzed like other native rules (load-time checks,
select() conditions, attribute validation, value gaps) and then validate their
label attributes like `RuleContext.Builder` (`analysis/RuleContext.java:1708-1745,
1845-1880, 2002-2093, 2165-2172`): each prerequisite is analyzed, then — in
rule-class attribute order, all errors reported at the rule's location —
`validateRuleDependency` (an allowed rule class accepts; otherwise the
mandatory provider accepts; otherwise both unfulfilled requirements are joined
with ` and `), the no-files check for files, visibility, testonly and
deprecation:

* `constraint_value.constraint_setting` (`rules/platform/ConstraintValueRule.java:56-63`:
  rule class `constraint_setting`, provider ConstraintSettingInfo): `in
  constraint_setting attribute of constraint_value rule //e:v: filegroup rule
  '//e:fg' is misplaced here (expected constraint_setting) and '//e:fg' does
  not have mandatory providers: 'ConstraintSettingInfo'`; alias chains are
  described (`alias '//os:macos' referring to constraint_value rule
  '//os:osx' ...`).
* `constraint_setting.refines_constraint_value` (`ConstraintSettingRule.java:84-89`,
  rule class `constraint_value`, ConstraintValueInfo). `default_constraint_value`
  is a NODEP label (no edge): `The default constraint value must be defined in
  the same package as the constraint setting itself.` and `The default
  constraint value '//e:x' does not exist` (`ConstraintSetting.java:270-304`).
* `platform.constraint_values` (ConstraintValueInfo), `parents` (PlatformInfo),
  `required_settings` (rule class `config_setting`) (`PlatformRule.java:75-152`).
  `Platform.create` (`rules/platform/Platform.java:44-120`) then reports, in
  order: `parents attribute must have a single value`; the
  remote/exec-properties conflicts (`PlatformInfo.java:372-399`, `in
  exec_properties attribute ...`); `Duplicate constraint values detected:
  constraint_setting //os:os has [//os:linux, //os:osx], ...` (settings with
  more than one distinct value, first-seen order; the same label twice is a
  package error at loading, below, so through a BUILD file only an alias and
  its actual can repeat a value, which is not a
  duplicate); and the refinement checks of `validateRefinedConstraintValues`
  (`:130-183`, labels in `getShorthandDisplayForm` with the main repository
  mapping, `cmdline/Label.java:527-539`): `constraint_value //:debian refines
  //os:linux, but platform //:debian_only does not set the latter. Fix with:\n
  buildozer 'add constraint_values //os:linux' //:debian_only` and `... sets
  the conflicting constraint_value //os:osx for //os`.
* Inheritance: constraints not set on a platform come from its parent, else
  from the setting's `default_constraint_value`; `exec_properties` merge over
  the parent's (an empty value removes the key, only when there is a non-empty
  parent); `remote_execution_properties` replaces
  `{PARENT_REMOTE_EXECUTION_PROPERTIES}` with the parent's value (the attribute
  default `""` never inherits); flags are the parent's followed by the
  platform's; required settings, toolchain-type checks and the missing-toolchain
  message are not inherited.

`check_toolchain_types`, `allowed_toolchain_types` and
`missing_toolchain_error` are consumed by toolchain resolution
(docs/TOOLCHAINS.md); `flags`, `required_settings` and the execution
properties are recorded but only execution platform resolution or
platform-based flags would consume them; platform flags fail explicitly when
the platform is the target platform (below).

## Target platform

`--platforms` keeps only its first entry (`analysis/PlatformOptions.java:189-200`);
the target platform is that entry, else `--host_platform`
(`PlatformOptions.computeTargetPlatform`, `:205-212`;
`skyframe/config/PlatformMappingValue.java:154-158`), whose default is
`@bazel_tools//tools:host_platform` (`:46-64`). A platform label is resolved
like `PlatformFunction.compute` (`analysis/platform/PlatformFunction.java:57-126`)
and memoized per configuration:

1. Package and target errors propagate as the loading exception, which Bazel
   prints with its class (`com.google.devtools.build.lib.packages.NoSuchTargetException:
   no such target '//:p': ...`, `...BuildFileNotFoundException: no such package
   'nopkg': ...`; paired capture A4/platforms_e_nope). An apparent repository
   the main repository does not see stays an unresolvable label until then:
   `...BuildFileNotFoundException: no such package '@@[unknown repo 'nope'
   requested from @@]//': The repository '@@[unknown repo 'nope' requested from
   @@]' could not be resolved: No repository visible as '@nope' from main
   repository`.
2. `PlatformLookupUtil.hasPlatformInfo` (`skyframe/toolchains/PlatformLookupUtil.java:160-172`):
   the target must be a rule that does not use toolchain resolution and
   advertises PlatformInfo — `platform`, or the any-provider classes `alias`
   (without select() keys or `target_compatible_with`), `label_flag` and
   `label_setting`. A file, a `constraint_value`, a `filegroup` or an alias with
   select() keys gives `Target //:fg was referenced as a platform, but does not
   provide PlatformInfo` (`:175-205`).
3. **Empty configuration.** Bazel analyzes the platform and its whole
   dependency tree in `CommonOptions.EMPTY_OPTIONS`
   (`PlatformFunction.configuredTargetDep`, `PlatformFunction.java:46-52`;
   `analysis/config/CommonOptions.java:32-42`: CoreOptions defaults only, no
   platform options, no Starlark build settings). There a `label_flag` or
   `label_setting` has no actual, so one on the platform label's alias chain
   provides nothing and is named: `Target //:plat_flag was referenced as a
   platform, but does not provide PlatformInfo` (also for a `label_flag` whose
   default is a platform, and through an alias; paired capture
   B2/platforms*_plat_flag, *_fg_flag); one reached through a platform rule's
   attribute fails that rule's prerequisite check: `LOC: in parents attribute
   of platform rule //:flag_parent: '//:plat_flag' does not have mandatory
   providers: 'PlatformInfo'` followed by `Target //:flag_parent was referenced
   as a platform, ...` (A8/gap_flag_parent, B8/target_cv_flag_platform). No
   rule in the tree resolves toolchains there (no PlatformConfiguration,
   `skyframe/toolchains/ToolchainContextUtil.java:79-82`), so `alias(actual =
   ":fg")` as `--platforms` reports `Target //:fg ...`. The native profile
   analyzes in the build configuration, which is the same only for a
   configuration-independent tree, so it fails explicitly on select() keys
   anywhere in the tree, label settings reached otherwise, and
   `required_settings` naming `config_setting`s (only used by
   execution-platform resolution, N17; other targets get Bazel's
   `is misplaced here (expected config_setting)`): `//:sel_alias: select() in
   the dependency tree of platform //:sel_parent is not implemented in the
   native profile: Bazel analyzes platforms and their dependencies in the empty
   configuration (...)`. Analyzed directly (`bazel build //:p`), a platform is
   in the target configuration (it has no transition), so select() works there.
4. The target is analyzed; an analysis error is reported, followed by the same
   `referenced as a platform` line. A target without PlatformInfo at the end
   of its alias chain names that end.
5. A platform with `flags` fails explicitly: platform-based flags
   (`ParsedFlagsValue`, `PlatformFunction.java:102-125`) are not modelled.

**When.** An explicit `--platforms` entry is resolved before any target is
analyzed, as Bazel resolves it when it creates the top-level configuration
(`analysis/producers/BuildConfigurationKeyProducer.java:126-145`). A
non-default target platform that comes from `--host_platform` is resolved by
each rule that uses toolchain resolution, before its select() conditions
(`analysis/producers/DependencyContextProducerWithCompatibilityCheck.java:80-110`).
A non-default `--host_platform` (the execution platform) is resolved by the
toolchain resolution of such rules, after their conditions, and reported like
Bazel: `LOC: While resolving toolchains for target //e:fg (CONFIG): Target
//e:fg was referenced as a platform, but does not provide PlatformInfo` (a
missing target is reported the same way; CONFIG is the native identifier,
paired capture B2/host_host_platform_fg_platforms_p_mac). A platform target
resolves no toolchains, so `build --host_platform=//e:fg //p:bare` succeeds as
in Bazel. The registered execution platforms are checked at the same point.

**Lazy default host platform (divergence).** Bazel resolves the default host
platform for every toolchain-resolving rule (as target and execution
platform). The native profile resolves it only where a constraint needs it: a
`constraint_value` select() key, `config_setting` `constraint_values` matching
(after the `values`/`flag_values`/not-a-constraint_value/duplicate checks, so
`//os:os is not a constraint_value` comes first as in Bazel, paired capture
B2/direct_cv_setting), or `ctx.target_platform_has_constraint` (the rule is
analyzed again once it is resolved), the toolchain resolution of a rule
with toolchain types or execution constraints (docs/TOOLCHAINS.md), and the
execution platform of a toolchain-using rule with a resolved exec-transition
edge (N18b, docs/CONFIGURATION.md "Exec transition"). A build
that never needs either never resolves it, and builds without the built-in
profile keep working.

**Default host platform.** `@bazel_tools//tools:host_platform` is the genuine
embedded `tools/BUILD` alias to `@platforms//host`; that package loads
`HOST_CONSTRAINTS` from `@host_platform//:constraints.bzl`, which the platforms
module's `host_platform` module extension generates from `repository_ctx.os`
(`linux`/`amd64` → `@platforms//os:linux`, `@platforms//cpu:x86_64`;
docs/HOST_OS.md). This needs the genuine built-in profile
(`--experimental-builtin-modules`, docs/BUILTIN_MODULES.md). Without it the
native module graph has no `@bazel_tools`, and resolving the default host
platform fails explicitly: `the target platform
@@bazel_tools//tools:host_platform (the default host platform) needs the
built-in @bazel_tools repository, which this module profile does not provide
(...); use --experimental-builtin-modules, or select a platform with
--platforms`. `--platforms=@bazel_tools//tools:host_platform` (any spelling of
the default) is the default configuration and is resolved lazily too.

The configuration identity includes the target and host platform labels
(docs/CONFIGURATION.md); output paths keep the `k8-<mode>` root, as Bazel's
`<cpu>-<mode>` does not depend on `--platforms`.

**Platforms module versions.** The tests and the paired capture use the
genuine platforms 1.0.0 archive (sha256 3384eb1c…,
`tests/fixtures/label_repo_name/platforms_archive.json`); the application's
module graph selects platforms 1.1.0. The host detection is the same code: the
1.1.0 repository's `host/extension.bzl`, `host/constraints.bzl` and
`host/BUILD` are byte-identical to 1.0.0's (`extension.bzl` sha256
b7686f14f3dc03f586299247aa30b3eeb85da03ee02d8b7fca0b273c82ae56fb in both; the
application's generated `@host_platform//:constraints.bzl` lists
`@platforms//cpu:x86_64` and `@platforms//os:linux`, as the 1.0.0 fixture
yields natively).

## Constraint matching

* **`config_setting(constraint_values = ...)`** (`rules/config/ConfigSetting.java:300-357`).
  The labels are prerequisites (no files, `ConfigRuleClasses.java:314-316`;
  analyzed and validated with the `flag_values` keys, in attribute order).
  After the `values` and `flag_values` errors: each target without
  ConstraintValueInfo is `in constraint_values attribute of config_setting
  rule //e:c: //e:fg is not a constraint_value` (the end of an alias chain);
  any error so far stops the matching; then duplicate settings are the rule
  error `in config_setting rule //e:c: Duplicate constraint values detected:
  ...`. The setting matches when the target platform has every value
  (`ConstraintCollection.containsAll`); otherwise each differing setting is a
  diff `want` = the value's name, `got` = the platform's value name or
  `<unset>`. The specified constraint values (for `refines`) are the
  prerequisites' labels plus their settings' refinement chains
  (`getSpecifiedConstraintValues`, `:841-852`), so `config_setting(constraint_values
  = [":debian"])` refines `config_setting(constraint_values = ["//os:linux"])`
  when `:debian`'s setting refines `//os:linux`.
* **`constraint_value` keys** (`analysis/config/ConfigConditions.java:64-83`,
  `ConstraintValueInfo.configMatchingProvider`): a key without
  ConfigMatchingProvider but with ConstraintValueInfo matches when the target
  platform's value for its setting equals it. It specifies no settings, flags
  or constraint values, so a `config_setting` with the same constraint refines
  it. The key is valid only when the consuming rule uses toolchain resolution
  (`Rule.useToolchainResolution`, `packages/Rule.java:846-854`); otherwise it is
  Bazel's `//os:linux is not a valid select() condition for //e:s.` error.
  Toolchain resolution is disabled for `constraint_setting`,
  `constraint_value`, `platform`, `toolchain`, `toolchain_type`
  (`ToolchainResolutionMode.DISABLED`), `label_flag`/`label_setting`
  (`rules/LabelBuildSettings.java:99`) and Starlark build settings
  (`packages/RuleClass.java:916-918`); conditional for `alias` (select() keys or
  `target_compatible_with`, `rules/Alias.java:78-98` — so an alias's
  `constraint_value` keys are valid) and `config_setting` (non-empty
  `constraint_values`, `ConfigRuleClasses.java:196-201`); enabled for
  `filegroup`, `genrule` and other Starlark rules. `constraint_setting` and
  `platform` targets are never valid keys.

## Loading

Duplicate labels in a LABEL_LIST or LABEL_LIST_DICT attribute (native and
Starlark rules, plain values and each select() branch separately; NODEP lists
and outputs are exempt) are package errors at loading, every one reported at
its rule's location: `LOC: Label '//os:linux' is duplicated in the
'constraint_values' attribute of rule 'dup_same'`
(`packages/RuleClass.java:2080, 2107-2129`,
`packages/AggregatingAttributeMapper.java:243-320`); every target of the
package then fails, also as a `--platforms` value. A LABEL_LIST value that
concatenates several elements (`srcs = [":a"] + select({":c": [":a"], ...})`)
is checked again once resolved, at analysis
(`RuleContext.Builder.checkAttributesForDuplicateLabels`,
`analysis/RuleContext.java:1553-1568, 2110-2113`;
`packages/ConfiguredAttributeMapper.java:456-467`; LABEL_LIST_DICT is exempt
there): `LOC: in srcs attribute of filegroup rule //:fg: Label '//:a.txt' is
duplicated` for native and Starlark rules, after the dependency checks and
before the select() conditions' visibility. A missing target of
another package is first reported at that package's BUILD file (`/ws/os/BUILD.bazel:
no such target '//os:nothere': ...`), then as the consumer's edge error (`...
and referenced by '//:cv_missing'`) or select() key error.

## Divergences

* The default host platform is resolved lazily (above), and only with the
  built-in profile; a failure is reported where the native profile first
  needs it (inside a `config_setting` key, followed by `errors encountered
  resolving select() keys for ...`).
* Execution platform registrations of non-root modules are only seen when
  the module graph has already been resolved in the build: a main-repository-
  only build (no external label resolved) never resolves it, so a non-root
  module's `register_execution_platforms` is not validated there, where
  Bazel would fail (`invalid registered execution platform ...` or its
  explicit native gap). This divergence only drops an error; it never changes
  a successful build's result (registrations are not used natively).
* Platforms and their trees are analyzed in the build configuration; trees
  that would differ in the empty configuration fail explicitly (above).
  `constraint_setting`/`constraint_value` with select() keys fail explicitly
  (Bazel analyzes them under `NoConfigTransition`,
  `rules/platform/ConstraintSettingRule.java:42`, `ConstraintValueRule.java:41`).
* Exit codes: Bazel exits 2 for an invalid `--platforms` (configuration
  creation) and for `query --platforms=X :: Unrecognized option:
  --platforms=X`; the native CLI exits 1 for every failure.
* The configuration identifier in `While resolving toolchains for target L
  (CONFIG)` is the native one (the paired capture normalizes it like the other
  7-hex identifiers).
* Gated provider fields failed with the interpreter's ``Object of type
  `PlatformInfo` has no attribute `label` `` where Bazel says `'PlatformInfo'
  value has no field or method 'label'`; since N33 the native text is Bazel's
  (with its SpellChecker hint, docs/STARLARK_VENDOR.md).
  `platform_common.PlatformInfo()` fails explicitly where Bazel says
  `'Provider' object is not callable`.
* The BUILD file itself is not a target natively (pre-existing), so
  `default_constraint_value = "BUILD.bazel"` or `--platforms=//p:BUILD.bazel`
  differ.

## Explicit gaps

* Toolchain resolution is implemented with the host platform as the only
  execution platform (docs/TOOLCHAINS.md).
* `register_execution_platforms` (the root module's registrations, and the
  non-dev registrations of the other modules once the module graph has been
  resolved; no extra module resolution I/O) fails the first toolchain-resolving
  rule: `register_execution_platforms("//:p") is not implemented in the native
  profile: execution platform resolution (N17)`; `--extra_execution_platforms`
  is an unsupported option.
* Exec groups are N35 (docs/EXEC_GROUPS.md); the exec transition is modelled (N18b,
  docs/CONFIGURATION.md: an exec configuration's target platform is the
  execution platform, the end of the host platform's alias chain); platform
  `flags` as the target
  platform; `required_settings` and select() in a target platform's tree;
  `platform_mappings` (a workspace file fails the build);
  incompatible target skipping (an incompatible `target_compatible_with`
  fails explicitly, below); `--experimental_platforms_api`.

## target_compatible_with (step 1: compatible targets)

IncompatibleTargetChecker.IncompatibleTargetProducer
(`analysis/constraints/IncompatibleTargetChecker.java:126-227`), run by
DependencyContextProducerWithCompatibilityCheck after the target platform
and the select() conditions and before toolchain resolution and every other
dependency (`analysis/producers/DependencyContextProducerWithCompatibilityCheck.java:159-189`),
natively `Engine::check_target_compatibility`:

* The built-in `target_compatible_with` of Starlark rules and of the native
  rules that are not exempt from constraint checking (filegroup, genrule,
  alias, ...; BaseRuleClasses.java:331-337) is resolved like
  ConfiguredAttributeMapper.getAndValidate, without configuring the rule's
  other attributes (a select() without a match is Bazel's `configurable
  attribute "target_compatible_with" in L doesn't match this configuration`
  error). Its labels are analyzed in the rule's own configuration and must
  provide a ConstraintValueInfo or a ConfigMatchingProvider (`in
  target_compatible_with attribute of writer rule L: 'X' does not have
  mandatory providers: ConfigMatchingProvider or 'ConstraintValueInfo'`);
  they are `ctx.attr.target_compatible_with` (Targets).
* For a rule that uses toolchain resolution: duplicate constraint settings
  fail (`Duplicate constraint values detected: constraint_setting
  @@platforms//os:os has [...]`), config_setting errors fail (`For
  config_setting L: ...`), and the target is compatible when the target
  platform has every constraint value (ConstraintCollection.get with the
  setting's default, so an unset setting matches its default value) and
  every config_setting matches. The target platform is resolved only when a
  constraint value needs it (the default host platform stays lazy). A rule
  without toolchain resolution (a build setting, the empty configuration)
  is never checked, as in Bazel.
* A compatible target is analyzed and built as usual (~/code's duckdb
  `cross_libs` cc_library targets and `//src/python/baseimage:image`'s
  select() of cpu constraints plus `os:linux`).
* An incompatible target fails explicitly at its analysis, naming the target
  platform and the unmet constraints with Bazel's reason text
  (TopLevelConstraintSemantics.formatIncompatibilityReasons): `//pkg:x is
  incompatible with its target platform: target platform
  (@@platforms//host:host) didn't satisfy constraint @@platforms//cpu:aarch64;
  skipping incompatible targets (IncompatiblePlatformProvider) is not
  implemented in the native profile (docs/PLATFORMS.md)`. Bazel instead makes
  it an IncompatiblePlatformProvider target: an explicit request (directly or
  through dependencies) fails with `Target L is incompatible and cannot be
  built, but was explicitly requested.` and its dependency chain (exit 1 in
  both), a wildcard pattern skips it (`Target L was skipped`, the build
  succeeds), `--skip_incompatible_explicit_targets` and SKIPPED tests are not
  modelled (step 2).

`tests/target_compatible_with.rs` replays the paired genuine capture
`tests/fixtures/target_compatible_with/reference_receipt.json` (17 cases: 11
MATCH, 6 declared divergences — the incompatible cases and the wildcard
skip).

## Tests

* Unit: `src/platforms.rs` (inheritance, overrides, defaults, duplicates,
  parents, exec/remote properties, refinement, direct-key matching including
  `<unset>`, display forms), `src/platform_providers.rs` (Starlark views,
  gated fields, reprs), `src/config_setting.rs` (`constraint_values`
  match/diff/refinement/errors), `src/configuration.rs` (target platform
  options, identities, unresolvable repositories, query options).
* CLI: `tests/platforms.rs` — constraint_values and direct keys under
  `--platforms` (inheritance/override, aliases, defaults, a constraint-free
  platform), `--host_platform` as the target platform, specialization,
  the rules_go `go_config` shape (`//go/constraints/amd64:v2..v4` keys with a
  `None` default), invalid keys in rules without toolchain resolution,
  `config_setting` errors, prerequisite/duplicate/parents/exec-properties/
  default-value/refinement errors, platform flags, non-platform and missing
  `--platforms`/`--host_platform` (exception classes, unknown repository,
  lazy execution platform), the empty-configuration rules (label settings,
  select()/label-setting/required_settings gaps), Starlark provider access and
  `ctx.target_platform_has_constraint`, duplicate labels, cross-package missing
  targets, the `config_setting` error order, `query --platforms`, execution
  platform registrations, and the genuine default host platform (built-in
  profile, the genuine platforms 1.0.0 archive with its `host_platform`
  extension: linux/x86_64, also through `ctx.target_platform_has_constraint`);
  `tests/native_rules.rs` (skylib `config_setting_group`, genrule and filegroup
  selects under the fixture's `@platforms//host`); `tests/configuration.rs`
  (`--platforms=//:p` loading errors); `tests/configured_build_settings.rs`
  (cross-package missing edge).
* Paired capture: `artifacts/n16c-capture.py` / `n16c-receipt.py`, tracked
  receipt `tests/fixtures/platforms/reference_receipt.json`.
