# Native BUILD rule classes at loading (Bazel 9.3)

Step 1 of the native-rules roadmap: the application's build closure (rules_go,
bazel_skylib, rules_python, `@bazel_tools//tools/cpp`, the platforms module,
`local_config_cc`-style toolchains) instantiates Bazel's *native* rule classes
in BUILD files and legacy macros. The native profile now **registers** these
classes with Bazel 9.3's attribute schemas, **validates** their attributes with
Bazel's load-time messages, **retains** the converted values (including
`select()`), and lists their targets in `query`. Except `filegroup`, `genrule`,
`config_setting` (`values`/`define_values`/`flag_values`) and the alias-like
`alias`/`label_flag`/`label_setting` (docs/CONFIGURATION.md), the platform
rules (docs/PLATFORMS.md) and `toolchain`/`toolchain_type`
(docs/TOOLCHAINS.md) it does **not** analyze them: analysis fails explicitly,
never silently.

Pinned source: `/tmp/opencode/bazel-9.3-reference` (e89755b, Release 9.3.0).
Paths are relative to `src/main/java/com/google/devtools/build/lib/` unless
noted; code comments in `src/native_rules.rs`, `src/package.rs` and
`src/engine.rs` cite file:line.

## Registered rule classes

All are available as BUILD globals and as `native.<name>` in `.bzl` files
(BUILD- and MODULE-loaded, `BazelStarlarkEnvironment.java:252-274`); their repr
is `<built-in rule NAME>` (`packages/RuleFactory.java:342-344`), `type()` is
`"rule"` (captured for every class) and positional arguments fail with
`unexpected positional arguments` (`:304-308`).

| Class | Bazel definition | Rule-specific attributes (configurable unless marked *nc*) |
| --- | --- | --- |
| `alias` | `rules/Alias.java:61-107` | `actual` (label, mandatory); no `licenses`/`distribs` |
| `config_setting` | `rules/config/ConfigRuleClasses.java:44-66, 130-320` | `values`, `define_values` (string dicts, *nc*), `flag_values` (label-keyed dict, *nc*), `constraint_values` (*nc*); default `tags=["manual"]`; licenses ignored (`setIgnoreLicenses`) |
| `toolchain` | `rules/platform/ToolchainRule.java:44-111` | `toolchain_type` (mandatory, *nc*), `exec_compatible_with` / `target_compatible_with` / `use_target_platform_constraints` (*nc*), `target_settings` (configurable), `toolchain` (nodep label, mandatory); default `tags=["manual"]` |
| `toolchain_type` | `rules/ToolchainType.java:57-81` | `no_match_error` (*nc*) |
| `constraint_setting` | `rules/platform/ConstraintSettingRule.java:38-87` | `default_constraint_value` (nodep, *nc*), `refines_constraint_value` (*nc*); `manual`; no `package_metadata` |
| `constraint_value` | `rules/platform/ConstraintValueRule.java:37-63` | `constraint_setting` (mandatory, *nc*); `manual`; no `package_metadata` |
| `platform` | `rules/platform/PlatformRule.java:48-152` | `constraint_values`, `parents`, `remote_execution_properties`, `exec_properties`, `flags`, `missing_toolchain_error` (*nc*); `required_settings`, `check_toolchain_types`, `allowed_toolchain_types`; `manual`; no `package_metadata` |
| `label_flag`, `label_setting` | `rules/LabelBuildSettings.java:86-127`, `packages/RuleClass.java:904-919` | `scope` (*nc*, one of `universal`/`target`/`project`, `analysis/config/Scope.java:31-60`), `build_setting_default` (nodep label, mandatory, *nc*), `help` (*nc*) |
| `genquery` | `rules/genquery/GenQueryRule.java:37-85` | `scope` (mandatory), `strict`, `expression` (mandatory), `opts`, `compressed_output` |
| `starlark_doc_extract` | `rules/starlarkdocextract/StarlarkDocExtractRule.java:33-139` | `src` (mandatory), `deps`, `symbol_names`, `render_main_repo_name`, `allow_unused_doc_comments`; implicit outputs `NAME.binaryproto`, `NAME.textproto` (`StarlarkDocExtract.java:77-78`) |
| `test_suite` | `rules/test/TestSuiteRule.java:30-120` | `tests` (*nc*); `testonly` defaults to True |
| `java_plugins_flag_alias` | `rules/java/JavaPluginsFlagAliasRule.java:47-69` (`bazel/rules/JavaRules.java:58`) | none |
| `cc_libc_top_alias` | `rules/cpp/CcLibcTopAlias.java`, `rules/LateBoundAlias.java:93-145` (`bazel/rules/CcRules.java:59`) | none; no `licenses`/`distribs` |
| `filegroup` | `rules/filegroup/FilegroupRule.java:36-80` | `srcs`, `output_group`, `data`, `output_licenses` |
| `genrule` | `rules/genrule/GenRuleBaseRule.java:66-348`, `bazel/rules/genrule/BazelGenRuleRule.java:40-56` | `srcs`, `tools`, `outs` (output list, mandatory, *nc*), `cmd`, `cmd_bash`, `cmd_bat`, `cmd_ps`, `output_to_bindir` (*nc*), `local`, `message`, `output_licenses`, `executable` (*nc*), `heuristic_label_expansion`, `toolchains` (*nc*), `stamp` (tristate), plus `exec_properties`, `exec_compatible_with` (*nc*), `exec_group_compatible_with` (*nc*) |

Every class starts from `name` (`RuleClass.java:771`), the common attributes
(`analysis/BaseRuleClasses.java:226-299`: `visibility`, `transitive_configs`,
`deprecation`, `tags`, `generator_*`, `testonly` — all *nc* — and configurable
`features`, `aspect_hints`; *nc* `compatible_with`, `restricted_to`,
`package_metadata`) and `NativeBuildRule` (`:318-340`: *nc* `licenses`,
`distribs`, configurable `target_compatible_with`).
`exemptFromConstraintChecking` removes `compatible_with`, `restricted_to` and
`target_compatible_with` (`packages/RuleClass.java:1559-1567`). Private
(`$`/`:`) attributes cannot be named by keyword arguments and are not modeled.
`applicable_licenses` is accepted as `package_metadata`
(`AttributeProvider.java:213-217`).

### Builtins injection (`src/main/starlark/builtins_bzl/bazel/exports.bzl`)

* `cc_toolchain_suite` is `lambda name, **kwargs: native.filegroup(name = name)`
  (`:53-54`): it creates a plain `filegroup` with **only** the name (no tags, no
  visibility), accepts and ignores every other argument, and its target is
  queryable and buildable. `type(cc_toolchain_suite)` is `"function"` and the
  lambda's binding errors are Starlark's (`lambda() missing 1 required
  positional argument: name`, `lambda() accepts no more than 1 positional
  argument but got 2`, `lambda() got multiple values for parameter 'name'`).
* `cc_toolchain_alias` and the other `_REMOVED_RULES` (`cc_binary`,
  `cc_import`, `cc_library`, `cc_shared_library`, `cc_static_library`,
  `cc_test`, `cc_toolchain`, `fdo_prefetch_hints`, `fdo_profile`,
  `memprof_profile`, `propeller_optimize`, `objc_import`, `objc_library`;
  `:22-37`) are replaced by `_removed_rule_failure` (`:39-44`), whose `fail()`
  message (`This rule has been removed from Bazel. Please add a load() statement
  for it. ...`) is reproduced. In Bazel 9.3 `cc_toolchain_alias` is therefore
  **not** a loadable native rule even though `CcRules.java:58` registers its
  class; the builtins override wins in BUILD and `native`.

## Attribute conversion (`AttributeProvider.java:162-328`)

* Unknown attributes are errors even when the value is `None`
  (`--incompatible_fail_on_unknown_attributes` defaults to true,
  `BuildLanguageOptions.java:634-636`): `//pkg:x: no such attribute 'foo' in
  'alias' rule (did you mean 'actual'?)` with Starlark's `SpellChecker`.
* `None` means unset; missing mandatory values give `//pkg:x: missing value for
  mandatory attribute 'actual' in 'alias' rule`.
* `name` errors use `RuleFactory.java:66-81` texts (`alias rule has no 'name'
  attribute`, `alias 'name' attribute must be a string`) and `Label.create`'s
  target-name validation (`cmdline/Label.java:303-311`,
  `LabelParser.java:215-232`): `illegal rule name: a:b: invalid target name
  'a:b': target names may not contain ':'`.
* `licenses` type errors name `list(string)` (`LicenseType.convert` delegates
  to `STRING_LIST`, `BuildType.java:504-512`).
* Rule `visibility` and `package(default_visibility)` / `repo(default_visibility)`
  follow `RuleVisibility.validateAndSimplify` (`RuleVisibility.java:162-215`):
  `//visibility:public` subsumes every other label (so
  `package_default_visibility()` then returns `[Label("//visibility:public")]`),
  `//visibility:private` is dropped next to other labels, and any other
  `//visibility:X` except `__pkg__`/`__subpackages__` fails with `Invalid
  visibility label '//visibility:X'; did you mean //visibility:public or
  //visibility:private?` (for a rule attribute prefixed `//pkg:x `,
  `AttributeProvider.java:277-286`). This replaces the previous native error for
  mixed public/private lists, also for custom Starlark rules.
* Values use `Type.convert` messages (`packages/Type.java:297-307`): `expected
  value of type 'list(label)' for attribute 'srcs' of 'filegroup', but got 1
  (int)`, `element N of ...`, `dict key element`/`dict value element`, booleans
  `expected one of [False, True, 0, 1]` (`:411-429`), 32-bit ints, tristates
  (`BuildType.java:900-922`), licenses (`License.java:115-142`), label-keyed
  dict duplicates (`BuildType.java:449-490`), label syntax via the Bazel label
  grammar port (`invalid label '../x' in element 0 of attribute 'srcs' of
  'filegroup': ...`). Lists accept lists, tuples, sets and dicts (Java
  `Iterable`, `Type.java:730-745`).
* Duplicate labels in a LABEL_LIST / LABEL_LIST_DICT attribute (native and
  Starlark rules; plain value and each select() branch separately) are package
  errors `LOC: Label 'L' is duplicated in the 'A' attribute of rule 'N'`
  (`packages/RuleClass.java:2107-2129`; docs/PLATFORMS.md).
* Allowed values: `label_flag(scope = "bogus")` gives `invalid value in 'scope'
  attribute: has to be one of 'universal', 'target', or 'project' instead of
  'bogus'`.
* Labels resolve against the BUILD file's package and repository mapping.
  A label of a **not visible apparent repository** (`@missing//:x`) is kept
  unresolved, like Bazel's `@@[unknown repo ...]` labels that only fail when
  used; native analysis then fails resolving it.
* `config_setting` without any setting **loads**: its "empty config_setting"
  errors are analysis-time `RuleContext` errors (`rules/config/ConfigSetting.java`),
  not loading errors (see Analysis).

## `select()`

`select()` is accepted exactly where Bazel's attribute is configurable
(`Attribute.isConfigurable`, `packages/Attribute.java:2101-2105`) and rejected
elsewhere with `//pkg:x: attribute "toolchain_type" is not configurable`
(`BuildType.java:234-238`). Examples: `alias.actual`, `toolchain.target_settings`,
`filegroup.srcs`, `genrule.srcs`/`cmd` and `platform.required_settings` are
configurable; `outs`, `tags`, `visibility`, `testonly`, `toolchain_type`,
`config_setting.values`, `label_flag.build_setting_default`,
`constraint_value.constraint_setting` and `test_suite.tests` are not.

Each branch is converted with `SelectBranchMessage`
(`BuildType.java:654-668`, `... for each branch in select expression of
attribute 'srcs' of 'filegroup' (including '//pkg:a') ...`); keys may be
strings or `Label`s (skylib's `native.package_relative_label` keys);
concatenation of non-concatenable types fails with `type 'label' doesn't
support select concatenation` (`:177-180`). With
`--incompatible_simplify_unconditional_selects_in_rule_attrs` (default true,
`BuildLanguageOptions.java:755-756`) a select whose only branch is
`//conditions:default`, possibly concatenated with plain values, becomes a plain
value (`BuildType.java:165-211`); such targets analyze normally. Every other
select is retained as `ConfigurablePart`s and its key/branch labels become
assumed input files (`Package.java:897-920`).

Assumed input files come from `Rule.getLabels()`, i.e.
`AggregatingAttributeMapper.visitLabels` (`AggregatingAttributeMapper.java:96-109`),
which skips the values of `NODEP_LABEL`/`NODEP_LABEL_LIST` attributes:
`toolchain.toolchain`, `constraint_setting.default_constraint_value`,
`platform.allowed_toolchain_types`, `transitive_configs` and `visibility` never
create input files (`toolchain(toolchain = ':impl')` with a file `impl` but no
target: `query //pkg:impl` fails). Select *keys* of every attribute, NODEP ones
included, are still assumed inputs because they are collected into
`$config_dependencies` (`AttributeProvider.java:443-466`, a `LABEL_LIST`,
`BaseRuleClasses.java:288-290`); the genuine capture confirms this
(`B/nodep_platform_allowed_toolchain_types_select_key`: Bazel's `query //pkg:k`
succeeds). The build-setting default of
`label_flag`/`label_setting` *is* visited, through the `:alias` attribute's
rule-based default (`rules/LabelBuildSettings.java:62-90`), so
`label_flag(build_setting_default = ':x')` makes `//pkg:x` a target.

**Selects resolve per configuration** (docs/CONFIGURATION.md): analysis of a
`filegroup`/`genrule` resolves its retained selects against the analyzed
`config_setting` conditions; unmodeled attribute values are checked on the
resolved values. `constraint_values` conditions such as `//skylib:linux`
match the target platform (docs/PLATFORMS.md).

## Analysis

* `constraint_setting`, `constraint_value` and `platform` provide native
  ConstraintSettingInfo/ConstraintValueInfo/PlatformInfo (docs/PLATFORMS.md).
* The classes above other than `filegroup`/`genrule`/`config_setting`/`alias`/
  `label_flag`/`label_setting` and the platform rules fail analysis with
  `//pkg:x: native rule KIND
  analysis not implemented in the native profile`, including when reached as
  a dependency or through an implicit output (`//pkg:doc.binaryproto`).
  `alias`, `label_flag` and `label_setting` forward their configured actual
  (docs/CONFIGURATION.md); any of their other non-empty attributes
  (`target_compatible_with`, `compatible_with`, `restricted_to`,
  `package_metadata`, `aspect_hints`, `transitive_configs`) fails explicitly.
* `filegroup`/`genrule` keep their native analysis only for the modeled
  attributes (`srcs`, `outs`, `cmd`, genrule `tools` in the exec
  configuration, docs/CONFIGURATION.md "Exec transition"; metadata such as `tags`, `deprecation`,
  `licenses`, `features`, `message`, `local`, `output_to_bindir`,
  `exec_properties`, Windows-only `cmd_bat`/`cmd_ps`). Any other non-empty
  value (`data`, `toolchains`, `cmd_bash`,
  `executable=True`, `stamp` other than 0, `target_compatible_with`,
  `package_metadata`, `aspect_hints`, `output_group`, ...) is recorded and
  analysis fails explicitly listing it.
* genrule pass-through: `local`, `deprecation`, `message`, `exec_properties`
  and every `tags` value (including execution-requirement tags such as
  `no-cache`, `no-remote`, `no-sandbox`, `requires-network`, `local`) are
  accepted and have **no effect** on native execution. Native actions always
  run locally and unsandboxed, so `local = True`/`no-sandbox` match, but every
  native action is cached: a `no-cache` genrule is cached natively where Bazel
  would not cache it. `output_to_bindir` has no effect because genfiles and bin
  are one directory (`--incompatible_merge_genfiles_directory`).
* `testonly` and `deprecation` (explicit or from `package()`/`repo()`
  defaults) drive Bazel's prerequisite checks for every analyzed consumer:
  the testonly error and the deprecation warning
  (docs/COMMON_RULE_ATTRIBUTES.md, "Analysis"). `testonly = True` on
  `filegroup`/`genrule` is therefore modeled, not an analysis gap.
* `genrule(outs = [])` loads (mandatory only means present) and fails analysis
  with `in outs attribute of genrule rule //pkg:x: Genrules without outputs
  don't make sense` (`rules/genrule/GenRuleBase.java:75-77`); a genrule without
  `cmd` fails analysis with `in cmd attribute of genrule rule //pkg:g: missing
  value for `cmd` attribute, you can also set `cmd_ps` or `cmd_bat` on Windows
  and `cmd_bash` on other platforms.` (`GenRuleBase.java:262-284`).
* Pure attribute checks Bazel's factories run first are reported before the
  "analysis not implemented" failure: `config_setting` with no non-empty
  `values`/`define_values`/`flag_values`/`constraint_values` fails with `in
  config_setting rule //pkg:c: Either values, flag_values or constraint_values
  must be specified and non-empty`, and a `values` key that looks like a label
  with `in values attribute of config_setting rule //pkg:c: '//x:y' is not a
  valid setting name, but appears to be a label. Did you mean to place it in
  flag_values instead?` (`rules/config/ConfigSetting.java:116-136, 384-397`);
  `java_plugins_flag_alias` outside `//tools/jdk` / `@bazel_tools//tools/jdk`
  fails with `in java_plugins_flag_alias rule //pkg:x: Rule //pkg:x cannot use
  private rule` (`rules/java/JavaPluginsFlagAliasRule.java:42-45, 80-83`).
* Diagnostics print labels in Bazel's canonical form (`//pkg:x`,
  `@@repo//pkg:x`, `Label.getCanonicalForm`, `RepositoryName.java:270-286`).

## `build` wildcards and `manual`

`query` keeps every rule (`FilteringPolicies.NO_FILTER`). `build` wildcard
patterns (`:all`, `/...`) now skip targets tagged `manual`
(`FilteringPolicies.FILTER_MANUAL`, `pkgcache/FilteringPolicies.java:60-65`;
`skyframe/TargetPatternPhaseFunction.java:293-296`); explicitly named targets
are kept. `config_setting`, `toolchain`, `constraint_setting`,
`constraint_value` and `platform` default to `tags = ["manual"]`, so
`build //pkg:all` skips them as Bazel does. Starlark rules accept the built-in
attributes (`tags`, `testonly`, ...) and `manual` Starlark targets are skipped
too (docs/COMMON_RULE_ATTRIBUTES.md).

## Package and repository helpers

* `package()` accepts every `PackageArgs.processParam` key
  (`packages/PackageArgs.java:160-204`): `default_visibility`,
  `default_testonly`, `default_deprecation`, `features`, `licenses`,
  `default_compatible_with`, `default_restricted_to`,
  `default_applicable_licenses`/`default_package_metadata`,
  `default_hdrs_check`, `transitive_visibility`, with Bazel's messages
  (`'package' can only be used once per BUILD file`, `at least one argument must
  be given to the 'package' function`, `unexpected keyword argument: X`,
  `duplicate label(s) in ...`, `Can not set both default_package_metadata and
  default_applicable_licenses. ...`; conversion `what` is `package() argument
  'X'`, `PackageCallable.java:50-94`).
* `REPO.bazel` is evaluated (`skyframe/RepoFileFunction.java:148-181`,
  `packages/RepoFileGlobals.java`): `repo(**kwargs)` takes the same arguments
  (`repo() argument 'X'`, `RepoPackageArgsFunction.java:117-130`) and supplies
  defaults that `package()` overrides (`PackageArgs.mergeWith`); `repo()` call
  order/arity errors are Bazel's. The file is read only when it is a regular
  file that resolves inside the repository (the same checks as BUILD files);
  any other existing path named `REPO.bazel` (for example a directory) is
  Bazel's `error reading REPO.bazel file at ...` (`FileValue.exists()`).
  `ignore_directories([...])` and `.bazelignore` are implemented (N14): see
  [IGNORED_DIRECTORIES.md](IGNORED_DIRECTORIES.md).
* Package defaults that change analysis in Bazel are recorded per rule and make
  its analysis fail explicitly unless the rule sets the attribute itself:
  non-empty `default_compatible_with`/`default_restricted_to`, and
  `transitive_visibility`. `default_package_metadata` (computed
  `package_metadata`, `BaseRuleClasses.java:100-107`) and explicit
  `package_metadata`/`applicable_licenses` are analyzed (N28c): the targets
  are dependencies in the no-config configuration
  (docs/COMMON_RULE_ATTRIBUTES.md "Package metadata"). Classes that removed
  the attribute (for example `platform` has no `package_metadata`) have none. `default_testonly` and
  `default_deprecation` are applied as computed defaults by the testonly check
  and the deprecation warning (docs/COMMON_RULE_ATTRIBUTES.md); Starlark rules
  follow the same rules with their own attribute sets.
* `licenses([...])` (BUILD only, `packages/BuildGlobals.java:100-132`) validates
  license types and records the package license.
* Shared BUILD/`native` helpers (`StarlarkNativeModule.java:616-683`):
  `module_name()`, `module_version()`, `repo_name()`, `repository_name()`,
  `package_relative_label()`, `package_default_visibility()`.
  `module_name()`/`module_version()` report the module associated with the
  package's repository (`skyframe/RepositoryMappingFunction.java:100-159`): the
  root module (empty strings without `module()` or `MODULE.bazel`) and every
  Bazel-module repository, with the version declared by `module()` (also for
  non-registry overrides, `ModuleFileFunction.java:274-279`). For
  module-extension and direct repositories Bazel reports the hosting module;
  that association is not modeled and the helpers fail explicitly there.

## Not implemented (explicit)

* `native.existing_rule()`/`existing_rules()` in BUILD context
  (`StarlarkNativeModule.java:320-522`): an exact view needs every attribute of
  every rule kind (including the computed and call-stack-derived built-in
  values); the functions stay absent from BUILD-loaded `native`
  (docs/EXISTING_RULES.md).
* `subpackages()`, `environment_group`, `query --output=label_kind` (query
  options are rejected), `test_suite`'s implicit `$implicit_tests`.
* Every other Bazel native rule class (for example the `EmptyRule` stubs
  `java_library`/`py_binary` that load in Bazel and fail at analysis) remains an
  unknown global.
* Analysis of every class listed above except `filegroup`/`genrule`,
  `config_setting`, the alias-like rules (docs/CONFIGURATION.md), the
  platform rules (docs/PLATFORMS.md) and `toolchain`/`toolchain_type`
  (docs/TOOLCHAINS.md).

## Documented divergences (pre-existing, unchanged here)

Recorded by the paired capture (`tests/fixtures/native_rules/reference_receipt.json`):

* `repr(Label(...))` prints the canonical string (`@@//pkg:x`); Bazel prints
  `Label("//pkg:x")` (display form). Values are equal after that normalization.
* `query` prints external labels canonically (`@@platforms//os:linux`); Bazel
  prints the display form `@platforms//os:linux` unless `--consistent_labels`.
* Unknown apparent repositories: native `unknown apparent repository @missing
  from canonical repository ""; extension repository not materialized or
  dependency not declared` versus Bazel's `no such package '@@[unknown repo
  'missing' requested from @@]//': ... No repository visible as '@missing' from
  main repository`.
* Visibility violations (fixed by the configuration increment): native now
  prints Bazel's multi-line `LOC: in genrule rule //pkg:g: Visibility error:
  target '//dep:f' is not visible from target '//pkg:g' ...`
  (docs/CONFIGURATION.md).
* The native label grammar is a stricter subset (`//a b:c` is rejected at
  loading; Bazel accepts it).
* Unregistered Bazel native rule classes (`java_library`, ...) are unknown
  globals (`Variable `java_library` not found`).
* A missing source target (fixed by the configuration increment) reports
  Bazel's `no such target '//pkg:k': target 'k' not declared in package 'pkg'
  defined by BUILD ...` with the source-file and did-you-mean suggestions
  (docs/CONFIGURATION.md); a missing package Bazel's `no such package 'P':
  BUILD file not found ...`.
* `package_group` (a `native` builtin since before N13) has `type()`
  `function` and repr `package_group`; Bazel: `builtin_function_or_method`,
  `<built-in method package_group of native value>`.
* Analysis order: when several targets fail, native stops at the first one it
  analyzes, so the first reported error of a wildcard build can differ from
  Bazel's (`--keep_going` is not implemented).

The receipt (`tests/fixtures/native_rules/reference_receipt.json`, run 3:
EXACT 52, EXACT_MESSAGE 100, EXPECTED_NATIVE_UNSUPPORTED 21, MISMATCH 30,
REFERENCE_ONLY 18, REFERENCE_CRASH 1) records every remaining mismatch with
both messages. Run 3 was captured before the `$config_dependencies` select-key
fix above; its row `B/nodep_platform_allowed_toolchain_types_select_key` shows
the pre-fix native failure.

## Error footer

Every BUILD evaluation error ends with

```
Supported rules: the native rule classes in docs/NATIVE_BUILD_RULES.md and Starlark rule() definitions. select() resolves against modelled config_setting values (docs/CONFIGURATION.md).
```

(`src/package.rs` `NATIVE_LOADING_FOOTER`). Captures made before configured
attribute resolution end with `... select() values load, but native analysis
does not resolve them.`. The footer before that (`Supported
rules: filegroup, genrule, and local custom rules. select and non-local
repositories remain unsupported.`) was stale: it named only two rule classes and
claimed select was unsupported. `scripts/validate_label_passthrough.py` and
`scripts/validate_string_dict.py` compare the footer byte-exactly; they now
accept the current literal and, for captures retained from earlier increments,
the two earlier literals — each only as the single exact terminal line.

## Tests

* `src/native_rules.rs` and `src/package.rs` unit tests: schemas, kinds,
  defaults, configurability, simplification, Bazel messages, package/REPO
  arguments, module identity, deferred unknown repositories.
* `tests/native_rules.rs` (fixture `tests/fixtures/native_rules`, attributed
  shapes from rules_go 0.63, bazel_skylib 1.8.2, rules_python,
  `@bazel_tools//tools/cpp`, platforms 1.1.0): the **gate** asserts `query
  PKG:all` for twelve packages lists exactly the sorted labels Bazel lists;
  explicit analysis failures; `manual` wildcard filtering; unconditional select
  and `cc_toolchain_suite` builds; Bazel load errors with the footer; NODEP
  input files; pure-attribute analysis errors; REPO.bazel recursive queries.
  For the eight main-repository packages the query output matches the genuine
  capture byte for byte; for the four `@platforms` packages it matches after
  the display-form normalization above.
* Genuine capture: `artifacts/n13-capture.py` (ignored) writes
  `artifacts/n13-paired-capture.json` and the tracked receipt; earlier runs are
  kept in `artifacts/superseded/`.
