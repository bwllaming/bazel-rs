# Built-in attributes of Starlark rules (Bazel 9.3)

Every Starlark `rule()` class inherits Bazel's built-in ("common") attributes.
Before this increment custom rules rejected them, so the application's build
stopped at rules_go's `nogo` macro (`go/private/rules/nogo.bzl:143`,
`_nogo(name = ..., visibility = visibility, **kwargs)` with
`kwargs["tags"] = [..., "manual"]`) with `unknown attribute "tags"`. Starlark
rule instances now accept, convert, retain and (where Bazel does) expose these
attributes, and analysis applies the testonly check and deprecation warnings.

Pinned source: `/tmp/opencode/bazel-9.3-reference` (e89755b, Release 9.3.0).
Paths are relative to `src/main/java/com/google/devtools/build/lib/`; code
comments in `src/native_rules.rs`, `src/custom_rule.rs`, `src/analysis.rs`,
`src/package.rs` and `src/engine.rs` cite file:line.

## Attribute sets

`StarlarkRuleClassFunctions.createRule` picks the parent class
(`analysis/starlark/StarlarkRuleClassFunctions.java:760-784`):

| Rule | Parent | Built-in attributes (configurable unless marked *nc*) |
| --- | --- | --- |
| any | `baseRule` (`:159-194`) | `name`; `expect_failure` (string); `BaseRuleClasses.commonCoreAndStarlarkAttributes` (`analysis/BaseRuleClasses.java:226-299`): `visibility`, `transitive_configs` (nodep label lists, *nc*), `deprecation` (*nc*), `tags` (*nc*), `generator_name`/`generator_function`/`generator_location` (*nc*, undocumented), `testonly` (*nc*), `features`, `compatible_with`, `restricted_to`, `package_metadata` (*nc*), `aspect_hints`, private `:action_listener` and `$config_dependencies`; then `toolchains`, `exec_properties` (string dict), `exec_compatible_with` (*nc*), `exec_group_compatible_with` (dict of label lists, *nc*), `target_compatible_with` |
| `executable = True` | `binaryBaseRule` (`:232-241`) | + `args`, `output_licenses`, private `$is_executable` |
| `test = True` / `analysis_test = True` | `getTestBaseRule` (`:258-378`) | + `size` (default `medium`, *nc*), `timeout` (*nc*, default from size), `flaky` (*nc*), `shard_count` (default -1), `local` (*nc*), `args`, private `$test_wrapper`, `$xml_writer`, `$test_runtime`, `$test_setup_script`, `$xml_generator_script`, `$collect_coverage_script`, `:coverage_support`, `:coverage_report_generator`, `:run_under_exec_config`, `:run_under_target_config`, `$is_executable`; `testonly` forced to default `True` (`packages/RuleClass.java:839-851`) |
| `dependency_resolution_rule = True` | `dependencyResolutionBaseRule` (`:197-212`) | base minus `:action_listener`, `aspect_hints`, `toolchains`, `exec_compatible_with`, `exec_group_compatible_with`, `target_compatible_with`, `compatible_with`, `restricted_to`, `$config_dependencies`, `package_metadata` |

The common part is the same `commonCoreAndStarlarkAttributes` list the native
rule classes use (`src/native_rules.rs`: `common_core_and_starlark_attributes`
feeds both `native_build_rule` and `starlark_builtins`). `licenses` and
`distribs` belong to `NativeBuildRule` only (`BaseRuleClasses.java:322-338`):
Starlark rules do **not** have them (`no such attribute 'licenses'`), and may
define their own attributes with those names. `env`/`env_inherit` are not
built-in attributes of Starlark test rules in 9.3.

## Rule definitions

* A Starlark attribute whose native name (`_x` → `$x`, or `:x` for
  `configuration_field` defaults, `packages/AttributeValueSource.java:62-70`)
  is a built-in attribute of the chosen parent fails with Bazel's
  `attribute `tags`: built-in attributes cannot be overridden.`
  (`packages/RuleClass.java:1301-1306`), including `name`, `expect_failure`,
  `aspect_hints`, `_config_dependencies`, test rules' `size`/`flaky`/
  `_is_executable` and executable rules' `args`/`output_licenses`. This
  replaces the previous `attribute "tags" is reserved` (which also wrongly
  reserved `licenses`/`distribs` and missed several names). As in
  `attrObjectToAttributesList` (`StarlarkRuleClassFunctions.java:1286-1300`),
  every name is validated before any override check.
* A Starlark `_x` whose native name differs from a built-in one with the same
  public name (`_action_listener` is `$action_listener`, the built-in is
  `:action_listener`; late-bound `_config_dependencies` would be
  `:config_dependencies`) fails when the rule is exported, as the RuleClass
  constructor does (`packages/RuleClass.java:1905-1918`): `Rule r: Attributes
  $action_listener and :action_listener have an identical public name:
  _action_listener`.
* Initializers may set only Starlark-defined attributes
  (`StarlarkRuleClassFunctions.java:1855-1861`): the check uses the parent's
  built-in set (`Initializer can only set Starlark defined attributes, not
  'tags'`); `licenses`, `env` and `applicable_licenses` are no longer
  wrongly blocked (the latter is then converted as `package_metadata`).

## Instantiation (`AttributeProvider.populateRuleAttributeValues`, `packages/AttributeProvider.java:162-328`)

* Built-in values are converted with the native converter shared with the
  native rule classes: Bazel's `Type.convert` messages prefixed by the target
  (`//pkg:x: expected value of type 'list(string)' for attribute 'tags' of
  'my_rule', but got 1 (int)`; booleans `expected one of [False, True, 0,
  1]`), label resolution in the BUILD file's package, lists from any
  iterable, `None` meaning unset.
* `visibility`, `transitive_configs`, `tags` and `features` (and native
  `test_suite.tests`) are `orderIndependent` (`BaseRuleClasses.java:232-267`,
  `rules/test/TestSuiteRule.java:99-103`): the converted list is sorted with
  Java's natural order, duplicates kept (`BuildType.java:248-253`), also after
  simplifying an unconditional `select()`; retained selects are not sorted.
  `visibility` is sorted before `RuleVisibility.validateAndSimplify`. Strings
  compare by UTF-16 code units, labels by repository, package, then name
  (`cmdline/Label.java:677-685`). `ctx.attr.tags == ["a", "z", "z"]` for
  `tags = ["z", "a", "z"]`.
* `select()` is accepted exactly where the attribute is configurable
  (`features`, `aspect_hints`, `toolchains`, `exec_properties`,
  `target_compatible_with`, `expect_failure`, `shard_count`, `args`,
  `output_licenses`) and otherwise fails with `//pkg:x: attribute "tags" is
  not configurable` (`BuildType.java:234-238`), including `visibility`.
  Unconditional selects are simplified (`BuildType.java:165-211`); other
  selects are retained and their keys/branch labels become assumed input files.
* `applicable_licenses` is the alternate spelling of `package_metadata`
  (`AttributeProvider.java:213-217`); the renamed key is the one reported when
  the rule lacks it (dependency-resolution rules: `//pkg:x: no such attribute
  'package_metadata' in 'r' rule`).
* Unknown keyword arguments, also with value `None`, fail with
  `//pkg:x: no such attribute 'tagz' in 'my_rule' rule (did you mean
  'tags'?)` (`AttributeProvider.java:220-235`, SpellChecker over documented
  attributes — built-in ones in Bazel's order, then Starlark ones). Private
  attributes (`$x`/`:x`, built-in or a Starlark `_x`) are UNDOCUMENTED
  (`packages/Attribute.java:444-449`) and never suggested. A Starlark private
  attribute `_x` given in BUILD is likewise `no such attribute '_x' in 'r'
  rule` (no suggestion), replacing `private attribute "_x" cannot be set
  explicitly` and the former `unknown attribute "x"` texts.
* Test rules check `size`/`timeout` after population
  (`packages/RuleClass.java:2138-2156`): `In rule 'x_test', size 'huge' is
  not a valid size.` / `In rule 'x_test', timeout 'forever' is not a valid
  timeout.` (the timeout default derives from the size,
  `BaseRuleClasses.java:84-98`).
* `checkForValidSizeAndTimeoutValues` runs for *every* rule with a STRING
  `size`/`timeout` attribute and reads it with `NonconfigurableAttributeMapper`
  (`NonconfigurableAttributeMapper.java:44-49`), whose precondition fails for
  a configurable Starlark-defined attribute: Bazel 9.3 crashes instantiating a
  non-test rule with `attrs = {"size": attr.string()}`. The native profile
  fails such instantiations explicitly (`//pkg:x: Starlark attribute 'size'
  of type string is not supported by the native profile: Bazel 9.3 crashes
  validating it ...`) instead of reproducing or guessing; other types (for
  example `attr.int()`) are unaffected.
* `visibility` follows `RuleVisibility.validateAndSimplify` with the
  `//pkg:x Invalid visibility label ...` prefix (`AttributeProvider.java:277-286`),
  now for Starlark rules too.
* Effective `tags` drive `manual` filtering: `build //pkg:all` and `/...`
  skip `manual` Starlark targets (`pkgcache/FilteringPolicies.java:60-65`),
  as for native ones — the rules_go `nogo` targets are `manual`.

## Package defaults

`testonly` and `deprecation` are computed defaults
(`BaseRuleClasses.java:68-82`): `package(default_testonly = ...)` /
`default_deprecation` (and `repo()`) apply unless the attribute is set; test
rules ignore `default_testonly` (always `True` unless explicit). The values are
finalized with the package (`package()` may follow the rule call). As for
native rules, non-empty `default_compatible_with`/`default_restricted_to` and
`transitive_visibility` make analysis of rules that do not set the attribute
fail explicitly; explicit values (also `[]`) win.

**Package metadata (N28c).** `package_metadata` (input alias
`applicable_licenses`; computed default `default_package_metadata` /
`default_applicable_licenses`, `BaseRuleClasses.java:100-107`) is a
nonconfigurable `NO_FILE` label list with `NoConfigTransition` and no
constraint check (`BaseRuleClasses.java:291-297`). Its targets (explicit, or
the package/`repo()` default) are dependency edges of every rule that has the
attribute, analyzed after the attribute dependencies in the **no-config
configuration** (`CommonOptions.EMPTY_OPTIONS`,
`analysis/config/CommonOptions.java:32-42`: CoreOptions defaults, no Starlark
build settings; output root `bazel-out/fastbuild-noconfig/bin`,
`OutputPathMnemonicComputer.java:187-190`). Visibility and testonly are
checked as for any prerequisite; a file is `in package_metadata attribute of
KIND rule L: source file 'F' is misplaced here (expected no files)`; a
same-package default naming no target is an assumed input file (Rule.getLabels
visits computed defaults): `rule '//p:nope' does not exist`. In the
no-config configuration rules resolve no toolchains (no
PlatformConfiguration), and `EMPTY_OPTIONS` has no Starlark exec transition
(`starlarkExecConfig = null`, `CommonOptions.java:34-41`): a target there
with any exec-transition dependency edge fails before its dependencies with
`LOC: expected a Starlark exec transition definition, but was null`
(`ExecutionTransitionFactory.java:92-114`, at the target's location) -
unless its rule has an attribute named `license_kinds` (`AttributeMap.has`,
`:109-111`, the rules_license `license()` hack), whose exec edges are
`NoConfigTransition` and stay in the no-config configuration. Genrules always
have the exec edge `$genrule_setup` (`BazelGenRuleRule.java:44-46`) and
Starlark test rules their `$test_*` tools (`StarlarkRuleClassFunctions.java:290-352`),
so both always fail there. No configuration fragment exists there: a
subrule's `ctx.fragments.X` fails with `There is no configuration fragment
named 'X'. Available fragments: 'D'` over its declared fragments
(`StarlarkSubrule.java:570-591`, `FragmentCollectionApi.java:39-49`);
`select()` there fails explicitly. `ctx.attr.package_metadata` /
`ctx.files.package_metadata` are the no-config targets and their files.
AttributeProvider's pruning of a metadata rule's own default
(`AttributeProvider.java:347-372`, `RuleClass.isPackageMetadataRule`
`:2404-2425`) is unreachable for this computed-default attribute (the
`hasComputedDefault()` branch, `:334-340`, comes first) and would not match a
Bzlmod `rules_license+` anyway: rules_license's macros set
`applicable_licenses = []` themselves (`rules/license.bzl:132`); a metadata
rule that receives its own package default is a dependency cycle
(`[self-edge]`). `default_testonly` no longer makes any rule
fail analysis: the testonly check below uses the effective value.

## Analysis

* **testonly** (`analysis/CommonPrerequisiteValidator.java:730-776` with
  `bazel/rules/BazelPrerequisiteValidator.java`, no experimental packages):
  every analyzed rule (Starlark, `filegroup`, `genrule`) whose effective
  `testonly` is false and that depends on a rule target whose effective
  `testonly` is true fails with
  `in my_rule rule //a:lib: non-test target '//a:lib' depends on testonly
  target '//b:helper' and doesn't have testonly attribute set`
  (`RuleContext.ruleError` prefix, `analysis/EventHandlingErrorReporter.java:80-82`).
  Same-package edges are checked; output files of testonly rules are not
  (`--incompatible_check_testonly_for_output_files` defaults to false,
  `analysis/config/CoreOptions.java:690-701`). Consequently `testonly = True`
  on `filegroup`/`genrule` is no longer an analysis gap.
* **deprecation** (`CommonPrerequisiteValidator.java:683-726`): a rule
  without an effective `deprecation` (an explicit `""` counts as set) that
  depends on a rule in another package whose effective `deprecation` is set
  prints `WARNING: /abs/a/BUILD:3:5: in my_rule rule //a:user: target
  '//a:user' depends on deprecated target '//old:old': MESSAGE`; for an output
  file `... depends on the output file //old:gen.out of a deprecated rule
  //old:gen': MESSAGE` (Bazel's stray quote included). Per edge the testonly
  check runs first and the deprecation check second (`validate`,
  `CommonPrerequisiteValidator.java:48-60`), so an edge that is both prints
  its warning and then fails. Native and Starlark consumers are covered.
* **Requested deprecated targets** (`skyframe/TargetPatternPhaseFunction.java:255-275`,
  `maybeReportDeprecation`): before analysis, every requested rule (explicit
  label or wildcard result) whose `deprecation` is *explicitly* specified —
  `""` included, package/`repo()` defaults excluded — prints `WARNING:
  /abs/BUILD:2:5: target '//:p' is deprecated: MESSAGE`.
* Event locations are Bazel's `Rule.getLocation()`: the left parenthesis of
  the BUILD-level call (for targets created by a macro, the macro call;
  `TargetDefinitionContext.createRule`, `packages/TargetDefinitionContext.java:543-559`),
  printed as `/abs/path/BUILD.bazel:LINE:COLUMN`. Warnings are printed before
  the build result or error.
* **restricted_to = []**: `RuleConfiguredTargetBuilder.build` runs
  `RuleContextConstraintSemantics.validateAttributes` after the rule's
  implementation/factory (`analysis/RuleConfiguredTargetBuilder.java:104-106,
  450-468`, `analysis/constraints/RuleContextConstraintSemantics.java:354-367`):
  an explicit empty `restricted_to` fails with `in restricted_to attribute of
  my_rule rule //:p: attribute cannot be empty` (Starlark rules, `filegroup`,
  `genrule`). No other built-in attribute is `nonEmpty`; `compatible_with = []`
  is accepted.
* `ctx.attr` exposes the built-in attributes like
  `StarlarkRuleContext`/`StarlarkAttributesCollection`
  (`analysis/starlark/StarlarkRuleContext.java:205-266`,
  `StarlarkAttributesCollection.java:225-390`): `tags`, `testonly`,
  `deprecation` (`None` when unset, `Attribute.valueToStarlark`), `features`,
  `visibility` (the explicit simplified `Label` list, `[]` when unset),
  `transitive_configs` (Labels), `exec_properties`, `expect_failure`,
  `generator_name`/`generator_function`/`generator_location` (below), and the
  dependency lists `toolchains`, `compatible_with`, `restricted_to`,
  `package_metadata`, `exec_compatible_with`, `target_compatible_with`,
  `_action_listener`, `_config_dependencies` as (empty) Target lists with
  matching `ctx.files` entries; `package_metadata` holds the effective
  metadata targets, configured in the no-config configuration (N28c, above). `aspect_hints` and
  `exec_group_compatible_with` are not exposed, as in Bazel
  (`StarlarkRuleContext.java:207-209`, `StarlarkAttributesCollection.java:255-259`).
* `generator_*` (`packages/Rule.java:395-405`, `RuleFactory.getGeneratorName`,
  `packages/RuleFactory.java:238-274`): explicit values win; a rule called
  directly from BUILD gets `""`; a rule created by a legacy macro (the
  BUILD-level callee's current location is in a `.bzl` file) gets
  `generator_name` = the `name = "literal"` of the BUILD-level call
  (`PackageFactory.checkBuildSyntax` generator map,
  `packages/PackageFactory.java:637-648`, nested calls included; otherwise the
  target name), `generator_function` = the BUILD-level callee's name, and
  `generator_location` = `Rule.getRelativeLocation` (`packages/Rule.java:353-366`):
  the call location from the first occurrence of the package name (the whole
  absolute path in the root package, `sub/BUILD.bazel:2:4` otherwise). For
  targets of a symbolic macro the generator name/location come from the macro
  call and the function is the macro's exported name
  (`TargetDefinitionContext.java:543-595`); this branch is ported from source
  and not covered by the genuine capture.* Metadata without native analysis effect: `tags` (execution-requirement tags
  such as `no-cache`/`no-remote`/`no-sandbox` are not applied to actions:
  native actions always run locally, unsandboxed and cached — the genrule
  divergence of docs/NATIVE_BUILD_RULES.md), `features` (no `ctx.features`
  is exposed), `exec_properties` (validated and combined per exec group,
  recorded per action, not applied by local execution; docs/EXEC_GROUPS.md).
* Values whose semantics are not implemented make analysis fail explicitly
  (`declared rule features not analyzed by the native profile: ...`), never
  silently: non-empty `target_compatible_with` (constraint analysis and
  incompatible target skipping), `compatible_with`/`restricted_to`
  (constraint environments), `toolchains` (TemplateVariableInfo Make
  variables), `aspect_hints`, `transitive_configs`, a non-empty `expect_failure` (expected-failure
  analysis; `""` has no effect, `StarlarkRuleConfiguredTargetUtil.java:80-131`).
  `exec_compatible_with` takes part in toolchain resolution (it filters the
  execution platforms; `ctx.attr.exec_compatible_with` holds the constraint
  value targets; docs/TOOLCHAINS.md).
  Retained `select()`s on built-in attributes are resolved first and these
  checks apply to the resolved values (docs/CONFIGURATION.md). Test and executable rules are
  analyzed (docs/RUNFILES.md); their implicit test-runner attributes are not.

## Not implemented / divergences

* Warnings are re-emitted on every native build (no persistent analysis
  cache); errors (testonly, restricted_to) are printed with the `bazel-rs:`
  prefix instead of Bazel's `ERROR: <location>:` framing. Columns count
  characters, Bazel's UTF-16 units (they differ only after supplementary
  characters on the line).
* SpellChecker ties between equally distant Starlark attributes are resolved
  in name order (the retained schema does not keep `attrs` dict order).
* When a built-in dependency attribute names a target of the wrong kind,
  Bazel reports its own analysis error first (for example `in
  target_compatible_with attribute of show rule //:x: '//:c' does not have
  mandatory providers: ConfigMatchingProvider or 'ConstraintValueInfo'`, `in
  toolchains attribute ...: '//:c' does not have mandatory providers:
  'TemplateVariableInfo'`, `in compatible_with attribute ...: show rule
  '//:c' is misplaced here (expected environment)`, `While resolving
  toolchains for target //:x ...: Target //:c was referenced as a
  constraint_value, but does not provide ConstraintValueInfo`, `in show rule
  //:x: Expected failure not found: boom`), and for a satisfied
  `target_compatible_with` it builds or skips the target (`Target //:x is
  incompatible and cannot be built, but was explicitly requested.`); native
  fails all of these with its explicit not-analyzed message instead.
* Pre-existing native gaps observed by the capture, unchanged here:
  `ctx.executable` was absent (added by N23a, docs/RUNFILES.md); invalid attribute names report `invalid
  attribute name "1bad"` (Bazel: ``attribute name `1bad` is not a valid
  identifier.``); `attr.string(configurable = False)` reports ``Found
  `configurable` extra named parameter(s) for call to string`` (since N33
  `string() got unexpected keyword argument 'configurable'`; Bazel:
  `attribute 'size' has the 'configurable' argument set, which is not
  allowed in rule definitions`); `**kwargs` in BUILD reports `*args and
  **kwargs calls are not allowed in BUILD files` (Bazel: `**kwargs arguments
  are not allowed in BUILD files. Pass the arguments in explicitly.`);
  `environment_group`/`environment` are not registered; `query` supports no
  `attr()` expressions and `build` no negative (`-//x`) patterns.
* `native.existing_rule()`/`existing_rules()` remain absent in BUILD context
  (docs/EXISTING_RULES.md) and `query` has no `kind()`/attribute functions;
  built-in attribute values are therefore not introspectable.
* Private test-rule attributes (`_test_wrapper`, ...) are not exposed: test
  rules are not analyzed.
* `RuleClass.checkForDuplicateLabels` (`Label '//:a' is duplicated in the
  'toolchains' attribute of rule 'x'`, `packages/RuleClass.java:2114-2130`) is
  still not implemented for any label-list attribute, built-in or not
  (pre-existing gap shared with native and Starlark-defined attributes).

## Tests

* `src/native_rules.rs`: attribute sets per parent, shared schema with native
  rules, configurability, order independence, private names and public-name
  conflicts, SpellChecker candidates.
* `src/custom_rule.rs`: override errors per parent kind, names outside the
  built-in set, conversion/retention/finalization of built-in values, input
  labels, gaps.
* `src/package.rs`: unknown/private/`licenses`/`size` errors, effective
  testonly/deprecation, removal of the `default_testonly` gap.
* `tests/common_rule_attributes.rs` (CLI): the rules_go `nogo` macro shape,
  `manual` wildcard filtering, `ctx.attr` exposure, package defaults, the
  testonly error (Starlark and filegroup consumers, package default, output
  files), deprecation warnings, select configurability, conversion and
  unknown-attribute messages, explicit analysis gaps, test/executable
  attributes and override errors, sorted order-independent lists, generator
  attributes, requested-target deprecation warnings with locations, empty
  `restricted_to`, undocumented private attributes, public-name clashes and
  the testonly/deprecation edge order.
* Genuine paired capture: `artifacts/n15-capture.py` (ignored) writes
  `artifacts/n15-paired-capture.json` and the tracked receipt
  `tests/fixtures/common_rule_attributes/reference_receipt.json`.
