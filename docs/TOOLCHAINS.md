# Toolchain resolution (Bazel 9.3, increments N17a and N17b)

Native analysis of `toolchain_type` and `toolchain`, the registered
toolchains of a configuration (`register_toolchains`, `--extra_toolchains`),
toolchain resolution of every rule that declares `rule(toolchains = ...)`
against the target platform and the host execution platform, and the
Starlark side: `platform_common.ToolchainInfo(...)`, `dep[ToolchainInfo]` and
`ctx.toolchains`. The execution platform is always the host platform:
registered execution platforms, exec groups and execution transitions stay
explicit failures (below).

Pinned source: `/tmp/opencode/bazel-9.3-reference` (e89755b, Release 9.3.0).
Paths are relative to `src/main/java/com/google/devtools/build/lib/`. Code:
`src/toolchains.rs` (providers, registered toolchains, resolution, messages),
`src/toolchain_info.rs` (ToolchainInfo values, `ctx.toolchains`),
`src/engine.rs` (type lookup, registration collection and analysis, platform
keys, implementation dependencies, ResolvedToolchainContext),
`src/configuration.rs` (`--extra_toolchains`), `src/module_graph.rs`
(breadth-first module order), `src/repository.rs` (registrations).

## Providers and rules

| Native model | Source |
| --- | --- |
| ToolchainTypeRequirement: label, `mandatory`, `ignore_if_invalid` | `analysis/config/ToolchainTypeRequirement.java` |
| ToolchainTypeInfo: type label, `no_match_error` (empty means none); equal by both | `analysis/platform/ToolchainTypeInfo.java:29-104`, `rules/ToolchainType.java:40-52` |
| DeclaredToolchainInfo: type, exec/target ConstraintCollections, `use_target_platform_constraints`, `target_settings` matchers, the `toolchain` target, the NODEP implementation label | `analysis/platform/DeclaredToolchainInfo.java:42-190`, `rules/platform/Toolchain.java:42-110` |
| RegisteredToolchains (registration order), ToolchainContextKey (types, execution constraints, forced execution platform), UnloadedToolchainContext, ResolvedToolchainContext | `skyframe/toolchains/*.java`, `analysis/ResolvedToolchainContext.java` |

`toolchain_type` and `toolchain` targets are analyzed like the platform rules
(docs/PLATFORMS.md): load-time checks, select() conditions, attribute
validation, then their prerequisites, validated like `RuleContext.Builder`
(`ToolchainRule.java:57-111`): `toolchain_type` must be a `toolchain_type`
rule or provide ToolchainTypeInfo (`filegroup rule '//x:fg' is misplaced here
(expected toolchain_type) and '//x:fg' does not have mandatory providers:
'ToolchainTypeInfo'`), `exec_compatible_with`/`target_compatible_with` must
provide ConstraintValueInfo, `target_settings` must be `config_setting` rules
(no files, no provider requirement: `filegroup rule '//x:fg' is misplaced here
(expected config_setting)`, `source file '//x:f.txt' is misplaced here
(expected no files)`; the end of an alias chain counts, so skylib's
`config_setting_group` aliases work); the `toolchain` attribute is NODEP (not
analyzed with the toolchain). Toolchain.create then reports `in
use_target_platform_constraints attribute of toolchain rule L: Cannot set
use_target_platform_constraints to True and also set exec_compatible_with or
target_compatible_with`, or the duplicate-constraint errors of each attribute
(`in exec_compatible_with attribute ...: Duplicate constraint values
detected: constraint_setting //os:os has [//os:linux, //os:osx]`, also for
`target_compatible_with`). Both providers are forwarded by `alias`. Building
these targets directly succeeds without files.

`ConstraintCollection::diff` and `has_without_default` follow
`analysis/platform/ConstraintCollection.java:151-155, 182-194, 232-238`.

## Which rules resolve toolchains

`Rule.useToolchainResolution` (docs/PLATFORMS.md): every Starlark rule except
build settings, `filegroup`, `genrule`, aliases with select() keys and
`config_setting`s with `constraint_values`; no rule inside a platform's tree.
The ToolchainContextKey (`skyframe/toolchains/ToolchainContextUtil.java:60-165`)
holds the rule class's `toolchains`, its `exec_compatible_with` followed by
the target's `exec_compatible_with` attribute (Starlark rules), and forces the
parent's execution platform for a toolchain implementation (`:136-141`).
Resolution runs after the select() conditions and before the dependencies
(PrerequisiteProducer).

**Lazy default host platform (divergence D2).** Bazel resolves the target and
host platforms for every such rule. Natively a rule without toolchain types
and without execution constraints resolves nothing when `--host_platform` is
the default (so builds without toolchains keep working without the built-in
profile); a non-default `--host_platform` is always resolved and validated
(and a host platform with `check_toolchain_types` makes such rules fail with
`Unable to find an execution platform for target platform ...`, as in
Bazel, unless the rule is a toolchain implementation, whose execution
platform is forced). The default host platform needs the built-in
profile (docs/PLATFORMS.md); without it a toolchain-using rule fails with
`While resolving toolchains for target L (CONFIG): the target platform
@@bazel_tools//tools:host_platform (the default host platform) needs the
built-in @bazel_tools repository, ...`.

## Resolution (`ToolchainResolutionFunction.java:62-345`)

1. **Platform keys** (`PlatformKeys.java:78-224`): the target platform, then
   the host platform as the only execution platform (registered execution
   platforms fail explicitly before, unchanged from N16c). Platform errors
   are wrapped (`While resolving toolchains for target L (CONFIG): Target
   //e:fg was referenced as a platform, but does not provide PlatformInfo`).
   The execution constraints are looked up like
   `ConstraintValueLookupUtil.java:40-124` (a loading error is the
   exception itself, `com.google.devtools.build.lib.packages.NoSuchTargetException:
   no such target ...`; a target without ConstraintValueInfo is `Target L was
   referenced as a constraint_value, but does not provide
   ConstraintValueInfo`), and the host platform is kept only if it has all of
   them (`ConstraintCollection.findMissing`).
2. **Types** (`ToolchainTypeLookupUtil.java:44-170`): each type target is
   analyzed in the rule's configuration; aliases resolve to the actual type,
   and both the requested and the resolved label are recorded
   (requestedLabelToToolchainType). Errors: a loading error is the exception
   itself (preceded by the event at the package's BUILD file, also for the
   rule's own package), an apparent repository the requesting `.bzl` does not
   see is kept until here (Label.parseWithRepoContext) and is
   `com.google.devtools.build.lib.packages.BuildFileNotFoundException: no
   such package '@@[unknown repo 'R' requested from @@]//': ...` after the
   same text as an event without location, an analysis
   error is the type target's own, `Target L was referenced as a toolchain
   type, but does not provide ToolchainTypeInfo`, or `... but is a toolchain
   instance. Is the rule definition for the target you're building setting
   "toolchains =" to a toolchain() instead of the expected toolchain_type()?`.
3. **Registered toolchains** (`RegisteredToolchainsFunction.java:64-172`),
   computed once per configuration and only when a rule has at least one
   type: `--extra_toolchains` reversed (the last one wins; parsed with the main
   repository mapping), then each module's `register_toolchains` patterns in
   the dependency graph's breadth-first order (root first,
   `bazel/bzlmod/Selection.java:91, 376-410`), parsed with that module's
   repository and mapping. Dev registrations of non-root modules are dropped,
   the root's under `--ignore_dev_dependency`
   (`bazel/bzlmod/ModuleFileGlobals.java:390-411`; that flag also drops the
   root's overrides, `ModuleThreadContext.java:373`). Patterns expand like
   `TargetPatternUtil.expandTargetPatterns` (`skyframe/TargetPatternUtil.java:44-87`)
   with `FilteringPolicies.ruleTypeExplicit("toolchain")`: patterns in order,
   the first copy of a label kept; `:all` keeps the package's `toolchain`
   rules by name (manual tags are not filtered; a missing package is
   `while parsing 'PATTERN': no such package ...`,
   `pkgcache/TargetPatternResolverUtil.java:33-39`); `D/...`, `//...` and
   `@r//...` visit packages like RecursivePkgFunction (sorted directory
   entries, each directory's subpackages before itself: a STABLE_ORDER nested
   set iterates postorder, `collect/nestedset/NestedSet.java:190-230`;
   `no targets found beneath 'D'` without packages or directory,
   `skyframe/RecursivePackageProviderBackedTargetPatternResolver.java:326-329`;
   below an ignored directory `WARNING: Pattern 'P' was filtered out by
   ignored directory 'D'` and nothing, docs/IGNORED_DIRECTORIES.md); an
   explicit label is kept whatever it is. An apparent repository the
   registering repository does not see aborts the build: `Analysis of target
   'L' failed; build aborted: No repository visible as '@R' from main
   repository` (`bazel/repository/RepositoryFetchFunction.java:149-153`,
   `cmdline/RepositoryName.java:251-258`). `:*` and `:all-targets` fail
   explicitly: Bazel 9.3 crashes on them (a NullPointerException in
   `FilteringPolicies.RuleTypeFilter.shouldRetain`,
   `pkgcache/FilteringPolicies.java:120`, for the package's file targets;
   exit 37; the four REFERENCE_CRASH rows of the paired capture,
   B1/extra_extra_toolchains_names, B1/extra_extra_toolchains_names_all_targets,
   A9/pattern_errors/extra_tc and A9/pattern_errors/extra_tc_all_targets, are
   these genuine Bazel crashes, not native ones). A non-absolute
   MODULE.bazel registration is `Expected absolute target patterns (must
   begin with '//' or '@') for 'register_toolchains' argument, but got 'P' as
   an argument` (`bazel/bzlmod/ModuleFileGlobals.java:198-208`; native exit
   code 1, Bazel 48). **Every** registered target is analyzed in the
   current configuration (D1, so a broken unrelated toolchain fails every
   resolution, as in Bazel): a target without DeclaredToolchainInfo is
   `invalid registered toolchain 'L': target does not provide the
   DeclaredToolchainInfo provider`, a pattern error `invalid registered
   toolchain 'PATTERN': MESSAGE`. Then `target_settings` filter the
   toolchains (`ConfigMatchingUtil.java:42-82`; a setting in error is
   `invalid registered toolchain 'L': Unrecoverable errors resolving
   config_setting associated with L: For config_setting NAME: ...`).
4. **Per type** (`SingleToolchainResolutionFunction.java:149-293`), toolchains
   of the type in registration order: the target constraints must match the
   target platform unless `use_target_platform_constraints`; then each
   execution platform not yet matched is checked: `allowed_toolchain_types`
   when it `check_toolchain_types`, then the execution constraints (the
   target platform's for `use_target_platform_constraints`). A mismatch
   counts only for settings the toolchain sets itself (hasWithoutDefault, so
   a platform setting with a non-default value does not reject toolchains
   that do not mention it, while a toolchain constrained to a non-default
   value is rejected by a platform that has the setting's default); the loop
   stops once every execution platform is matched.
5. **Missing mandatory types** (`:189-235, 381-414`): `No matching
   toolchains found for types:` with one `  LABEL[: no_match_error]` row per
   type, `To debug, rerun with --toolchain_resolution_debug='A|B'` (labels
   with a regex character `\Q...\E`-quoted, `PlatformConfiguration.java:125-142`)
   and the target platform's `missing_toolchain_error` last (the default text
   `For more information on platforms or toolchains see
   https://bazel.build/concepts/platforms-intro.`; `""` prints nothing, not
   even an empty line).
6. **Execution platform** (`:302-345`, `PlatformKeys.isPlatformSuitable`):
   the forced platform if suitable (ignoring `check_toolchain_types` for a
   rule without types), else the first suitable platform with the most
   resolved types; none: `Unable to find an execution platform for [toolchains
   [T1, T2] and ]target platform P from available execution platforms [E]`
   (`NoMatchingPlatformData.java:50-71`; the filtered list).

Every ToolchainException is reported at the rule's location as `While
resolving toolchains for target L (CONFIG): MESSAGE`
(`ToolchainException.java:67-91`, `ConfiguredTargetFunction.java:394-404`).

**Implementations** are dependencies after the attribute dependencies
(`analysis/DependencyResolutionHelpers.java:358-373`), each once, analyzed in
the parent's configuration (`analysis/producers/DependencyProducer.java:160-175`,
`skyframe/DependencyResolver.java:835-866`); the execution platform part of
the native configured-target key is always the host platform. Only selected
implementations are analyzed (the `toolchain` attribute is NODEP). An
implementation whose analysis is not supported (for example `cc_toolchain`
through rules_cc) fails explicitly in its own analysis; an optional type that
resolves never silently becomes `None`.

## Starlark

* `platform_common.ToolchainInfo(**kwargs)` (`analysis/platform/ToolchainInfo.java:41-106`):
  keyword fields only, sorted by name; type `ToolchainInfo`, repr
  `struct(a = 1, b = 2)`, `dir()` the field names, structural equality.
  Rules return it like any provider; `dep[platform_common.ToolchainInfo]`,
  `platform_common.ToolchainInfo in dep` and `attr.label(providers = [...])`
  work; files inside it are provider files (action inputs).
* ResolvedToolchainContext.load (`analysis/ResolvedToolchainContext.java:48-110`):
  each resolved implementation (through an alias: the alias label is the
  resolved label) must provide ToolchainInfo, else `While resolving
  toolchains for target L (CONFIG): toolchain type T resolved to target I,
  but that target does not provide ToolchainInfo`.
* `ctx.toolchains` (`analysis/starlark/StarlarkToolchainContext.java:40-148`,
  `StarlarkRuleContext.java:877-898`): type `ToolchainContext`, repr
  `<toolchain_context.resolved_labels: ...>` (the requested labels and the
  resolved type labels), `dir()` `["toolchain_types"]`, whose method returns
  those labels (`starlarkbuildapi/platform/ToolchainContextApi.java:43-44`;
  none when toolchains are not valid). Keys are Labels or strings (`Unable to parse
  toolchain label 'S': ...`); other keys are `Toolchains only supports
  indexing by toolchain type, got T instead`. A type that was not requested
  is `In KIND rule L, toolchain type X was requested but only types [...] are
  configured`; an optional type without a toolchain is `None` while `in` is
  `True`; `in` checks the requested (and resolved) type labels. Rules
  without toolchain resolution (build settings) see `Toolchains are not
  valid in this context` (and `in` is `False`). `ctx.attr.exec_compatible_with`
  holds the targets of the constraint values.
* String keys are parsed with the package and repository mapping of the
  innermost calling `.bzl` (`LabelConverter.forBzlEvaluatingThread`,
  `cmdline/LabelConverter.java:41-44`); an apparent repository it does not
  see stays an unresolvable label (`@@[unknown repo 'R' requested from
  @@C]//...`, then not requested). The vendored interpreter passes the
  indexing or `in` expression's location to the value
  (`StarlarkValue::at_with_location` / `is_in_with_location`,
  docs/STARLARK_VENDOR.md): its module is the calling `.bzl`, also when
  Starlark inlined a small helper function from another module (the inlined
  expression keeps its own span). The implementation runs once. A
  `ctx.toolchains` value stored in a provider and indexed by another target
  uses that caller's `.bzl` (its ToolchainInfo files are provider files).
  Without a location (indexing from native code) or with a module name that
  is not a label, a string key fails explicitly (`ctx.toolchains["K"]: the
  native profile cannot determine the calling .bzl file ...`); there is no
  fallback mapping. Canonical `@@` strings and Labels need no caller.
  ToolchainTypeInfo is not exposed to Starlark natively, so it is never a
  key.
* `platform_common.ToolchainInfo(1)` is `ToolchainInfo() got unexpected
  positional argument`; a Starlark rule attribute requiring a provider the
  dependency lacks is `in A attribute of KIND rule L: 'D' does not have
  mandatory providers: 'P'` (the missing providers of each alternative,
  `analysis/RuleContext.java:2070-2095`,
  `packages/RequiredProviders.java:157-212, 308-321`; native prefixes
  `analysis of L: `).
* **Divergence (interpreter wording):** a missing ToolchainInfo field is
  ``Object of type `ToolchainInfo` has no attribute `zz` `` where Bazel says
  `'ToolchainInfo' value has no field or method 'zz'` / `Available
  attributes: a` (as for the platform providers, docs/PLATFORMS.md);
  Starlark tracebacks keep the native format.

## Explicitly unsupported

* Registered execution platforms (`register_execution_platforms`,
  `--extra_execution_platforms`): the host platform is the only execution
  platform; `required_settings` of platforms.
* Exec groups (`rule(exec_groups = ...)`), automatic exec groups (a
  `_use_auto_exec_groups` attribute that is True; `--incompatible_auto_exec_groups`
  defaults to false, `analysis/config/CoreOptions.java:419-432`,
  `packages/RuleClass.java:179-203`), `exec_group_compatible_with`, execution
  transitions.
* `genrule(toolchains = ...)` and the Starlark `toolchains` attribute (Make
  variables), `genrule(exec_compatible_with = ...)`, toolchains of aspects
  and subrules.
* Negative registration patterns (`--extra_toolchains=-//x:y`), `:*` and
  `:all-targets` registrations (a Bazel crash, above), and
  `--toolchain_resolution_debug` (an unsupported option).
* `cc_toolchain` analysis (rules_cc's C++ toolchains), so a C++ toolchain
  type that resolves fails in its implementation's analysis.
* A registered toolchain target that itself needs toolchain resolution is
  `Misconfigured toolchains: L is declared as a toolchain but has
  inappropriate dependencies. ...` with the cycle drawn like
  AbstractLabelCycleReporter.printCycle (`.-> RegisteredToolchains`, the
  configured targets from L to the rule, `toolchain types T1, T2`,
  `toolchain type T1`, `` `-- RegisteredToolchains``;
  RegisteredToolchainsCycleReporter.java:61-139), reported without location;
  of several registered targets in such cycles the last is named (paired
  capture B4/resolution_errors/extra_cyc_all_o).
* An analysis error of a registered toolchain, toolchain type or toolchain
  implementation target is reported as that target's own error; Bazel adds
  its generic `LOC: Analysis of target 'I' (config: ID) failed`, `Analysis
  of target 'L' failed; build aborted: Analysis failed` and, for a failed
  dependency, an empty `LOC: Analysis failed` line at the consumer, which the
  native CLI never prints (paired capture A7/starlark_context/broken).
* With the genuine built-in profile, `@bazel_tools` and the rule modules
  register their toolchains (for example `//tools/test:all`,
  `@local_config_cc_toolchains//:all`); every one is expanded and analyzed,
  so toolchain resolution there needs those repositories (their module
  extensions and sources); otherwise it fails with Bazel's `invalid
  registered toolchain` form naming the pattern as written in its
  MODULE.bazel (`'//tools/test:all'`, TargetPatternKey.getPattern). Offline,
  the fixture registry cannot materialize `@rules_shell` (archive symlinks,
  expected with N19), so the built-in profile currently fails there.
  Likewise, with genuine Bazel every build has these registrations: a host
  platform with `check_toolchain_types` fails on rules_java's toolchain
  `target_settings` (`Unable to find an execution platform for target
  platform //p:strict ...`, paired capture A6/execution_constraints/strict_u),
  so `allowed_toolchain_types` is only covered by unit tests.

## Tests

* Unit: `src/toolchains.rs` (diff/defaults, Toolchain.create errors, target
  settings, single resolution, messages, execution platform selection),
  `src/platforms.rs`, `src/configuration.rs` (`--extra_toolchains`),
  `src/module_graph.rs` (breadth-first order), `src/engine.rs` (recursive
  package order), `src/language_modules.rs` (ToolchainInfo),
  `src/toolchain_info.rs` (string keys by expression location, explicit
  failures without one).
* CLI: `tests/toolchains.rs` — target/execution platform selection, aliases,
  registration order (root before dependencies, `--extra_toolchains`
  reversed and deduplicated, `:all` by name, `/...` subpackages first,
  dependency dev registrations ignored, `--ignore_dev_dependency`), libc-style
  defaults, `use_target_platform_constraints` and its conflict, duplicate
  constraints, misplaced `target_settings`, broken unrelated toolchains,
  `target_settings` with a `config_setting` and a `config_setting_group`-style
  alias, every resolution error, `check_toolchain_types`, rule- and
  target-level `exec_compatible_with`, `ctx.toolchains`, whole-repository
  `//...`/`@dep//...` registrations with an ignored directory, registration
  pattern errors (`:*`, missing directory/package, unknown repository),
  toolchain cycles, deferred unknown repositories in `rule(toolchains)`,
  string keys from another repository's `.bzl` and the Starlark API details,
  escaped `ctx.toolchains` values indexed downstream and many string-keyed
  accesses in one implementation;
  the updated rows
  of `tests/configuration.rs`, `tests/load_declarations.rs`,
  `tests/native_rules.rs` and `tests/common_rule_attributes.rs`.
