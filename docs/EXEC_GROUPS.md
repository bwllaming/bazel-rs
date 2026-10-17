# Exec groups (Bazel 9.3, increment N35)

Native exec groups as genuine rule sets use them: `rule(exec_groups = {...})`
with `exec_group(toolchains = ..., exec_compatible_with = ...)`, per-group
toolchain resolution and execution platform selection, `ctx.exec_groups`,
automatic exec groups (`_use_auto_exec_groups`, `--incompatible_auto_exec_groups`),
`ctx.actions.run`/`run_shell(exec_group = ..., toolchain = ...)`,
`exec_properties` and `exec_group_compatible_with` per group, and
`cfg = config.exec(exec_group = NAME)` edges. The only execution platform is
still the host platform (docs/TOOLCHAINS.md), so every group resolves its
own toolchains and constraints against it.

Pinned source: `/tmp/opencode/bazel-9.3-reference` (e89755b, Release 9.3.0).
Paths are relative to `src/main/java/com/google/devtools/build/lib/`. Code:
`src/exec_groups.rs` (group names, InvalidExecGroupException, exec
properties, the action-side group choice, `ctx.exec_groups` values),
`src/engine.rs` (processing, per-group resolution, implementations, exec
configurations, ToolchainInfo loading, `ctx.toolchains` under automatic exec
groups, per-action execution records), `src/rule_declarations.rs`
(DeclaredExecGroup), `src/analysis_runtime.rs` (`run`/`run_shell`
parameters), `src/analysis_ctx.rs` (`ctx.exec_groups`), `src/subrule.rs`
(subrule toolchains and action overrides), `src/toolchain_info.rs` (label
strings of the calling `.bzl`), `src/runfiles.rs` (executable attributes),
`src/configuration.rs`/`src/config_registry.rs` (the option).

## Declarations and processing

`exec_group(...)` values (`packages/DeclaredExecGroup.java:43-53`,
`analysis/starlark/StarlarkRuleClassFunctions.java:2146-2160`; load-time
shape in docs/CC_COMMON_LOADING.md) are kept per rule in declaration order
(`RuleClass.Builder` LinkedHashMap, `packages/RuleClass.java:747`).
`rule(exec_groups)` names are validated at load time as before
(`StarlarkRuleClassFunctions.java:1066-1076`). A test rule that declares no
`test` group gets `RuleClass.DEFAULT_TEST_RUNNER_EXEC_GROUP` after its own
groups (`:1078-1082`); a declared `test` group replaces it (N30,
docs/TEST_FLAGS.md).

Before the target platform and the select() conditions
(`analysis/producers/RuleTransitionApplier.java:150-165`),
ToolchainContextUtil.getUnloadedToolchainContextsInputs
(`skyframe/toolchains/ToolchainContextUtil.java:42-93`) computes:

* **useAutoExecGroups**: `RuleClass.AutoExecGroupsMode.DYNAMIC`
  (`packages/RuleClass.java:179-203`): the value of a `_use_auto_exec_groups`
  attribute (`$use_auto_exec_groups`) if the rule class has one, otherwise
  `--incompatible_auto_exec_groups` (`analysis/config/CoreOptions.java:419-433`,
  default false). The option is a modelled boolean (BooleanConverter
  forms, config_setting `values`, accepted by `query`), propagated by the exec
  transition (`builtin_exec_platforms.bzl:246`), and in the configuration
  identity only when true. A non-boolean `_use_auto_exec_groups` attribute
  fails explicitly (Bazel reads it as a boolean).
* **Per-group constraints** from the target's `exec_group_compatible_with`
  (nonconfigurable, `:169-245`): an identifier key must be a declared group
  of the rule class (the default `test` group included), otherwise
  `Tried to set execution constraints for non-existent exec groups on L: K`
  with SpellChecker's ` (did you mean 'G'?)`; under automatic exec groups a
  label key (parsed in the target's package) must be one of the rule's
  toolchain types (suggestions: the types' display forms, for a
  main-repository target only); any other key is the same error without a
  suggestion. The InvalidExecGroupException is reported at the target's
  location (`skyframe/DependencyResolver.java:627-636`).
* **DeclaredExecGroup.process** (`packages/DeclaredExecGroup.java:94-150`):
  each declared group with its extra constraints, then (automatic exec
  groups) one group per rule toolchain type named by its `Label.toString()`
  (`//x:t`, `@@r//x:t`), with the type, the rule's default execution
  constraints and the key's extra constraints.

## Resolution

UnloadedToolchainContextsProducer (`analysis/producers/UnloadedToolchainContextsProducer.java:88-191`):
the default context first — without toolchain types under automatic exec
groups (`ToolchainContextUtil.createDefaultToolchainContextKey`, `:120-144`)
— then each group with its types and constraints, the same configuration and
no forced execution platform (`:110-124`), through the same
ToolchainResolutionFunction as the default context (docs/TOOLCHAINS.md: the
lazy default host platform D2, `No matching toolchains found for types`,
`Unable to find an execution platform for [toolchains [...] and ]target
platform P from available execution platforms [...]`, every message wrapped
in `While resolving toolchains for target L (CONFIG)`). Only the first error
is reported (`:178-183`). Every group's resolved implementations are
dependencies after the default context's
(`analysis/DependencyResolutionHelpers.java:358-373`) and are loaded like
ResolvedToolchainContext.load (`toolchain type T resolved to target I, but
that target does not provide ToolchainInfo`).

## Starlark

* `ctx.exec_groups` (`analysis/starlark/StarlarkRuleContext.java:906-910`,
  `analysis/starlark/StarlarkExecGroupCollection.java`): type
  `ExecGroupCollection`, repr `<ctx.exec_groups: a, b>` (the non-default
  group names sorted, automatic ones included), `dir()` `[]`; `in` and
  indexing take strings only (`exec groups only support indexing by exec
  group name, got 1 of type int instead`, also for `in`); an unknown name
  (also `default-exec-group`) is `In KIND rule L, unrecognized exec group
  'G' requested. Available exec groups: [a, b]`. `ctx.exec_groups[G]` is an
  `ExecGroupContext` (repr `<exec_group_context>`, `dir()` `["toolchains"]`)
  whose `toolchains` is a ToolchainContext over that group's requested types
  (docs/TOOLCHAINS.md: repr, `toolchain_types()`, string keys of the calling
  `.bzl`, `In KIND rule L, toolchain type X was requested but only types
  [...] are configured`).
* `ctx.toolchains` under automatic exec groups
  (`StarlarkRuleContext.java:860-898`, `analysis/RuleContext.java:1089-1150`):
  the requested types of the automatic groups, each resolved in its own
  group; the default context has no types.
* Subrules (`analysis/starlark/StarlarkSubrule.java:377-403`): under
  automatic exec groups `subrule_ctx.toolchains` is that context narrowed to
  the subrule's own types (without them Bazel's `subrules using toolchains
  must enable automatic exec-groups ...`, docs/SUBRULES.md). A subrule's
  actions may set neither `exec_group` (`'exec_group' may not be specified in
  subrules`) nor `toolchain` (`'toolchain' may not be specified in subrules`,
  also for `None`); their toolchain is the subrule's type, or None
  (`:464-481`).

## Actions

`ctx.actions.run` and `run_shell` take `exec_group` (string or None, `in
call to run(), parameter 'exec_group' got value of type 'int', want 'string
or NoneType'`) and `toolchain` (Label, string or None, default unbound;
`want 'Label, string, or NoneType'`); strings are parsed with the calling
`.bzl`'s repository mapping (LabelConverter.forBzlEvaluatingThread; a
syntax error is the LabelSyntaxException message, `invalid target name
'a:b': target names may not contain ':'`)
(`analysis/starlark/StarlarkActionFactory.java:415-475, 601-642, 740-898`,
`starlarkbuildapi/StarlarkActionFactoryApi.java:540-620`):

* `run_shell` checks a named group first (getExecutionPlatform, `:589-599`).
* Under automatic exec groups, an action without `exec_group` whose
  executable or tool is not an executable attribute's (a File without
  FilesToRunProvider, a FilesToRunProvider not from the attributes, any
  depset tool) needs `toolchain` when the ToolchainCollection has more than
  one context (also the default `test` group): `Couldn't identify if tools
  are from implicit dependencies or a toolchain. Please set the toolchain
  parameter. If you're not using a toolchain, set it to 'None'.`
  (`:130-139, 433-454, 764-800`).
* determineExecGroup (`:858-898`): `exec_group` must be a context of the
  collection (`Action declared for non-existent exec group 'G'.`) and a valid
  name (`Invalid name for exec group 'default-exec-group'.`, also for an
  automatic group's label name); with automatic exec groups and both
  parameters, the group must declare the toolchain (`` `toolchain` and
  `exec_group` parameters inside actions.{run, run_shell} are not compatible;
  ... ``); otherwise `toolchain` selects its automatic group (`Action
  declared for non-existent toolchain '//x:t'.`); else the default group.
* The chosen group decides the action's execution platform and exec
  properties (RuleContext.getActionOwner(execGroup),
  `analysis/RuleContext.java:455-479`). Natively it is recorded per planned
  action (`crate::exec_groups::ActionExecution`: group, platform — always
  the host platform — and combined properties); actions run locally, where
  exec properties have no effect.

## exec_properties

ExecGroupCollection (`analysis/ExecGroupCollection.java:52-216`), built by
RuleContext.Builder.build before the implementation
(`analysis/RuleContext.java:1491-1531`): `exec_properties` keys `property`
belong to the default group, `group.property` (split at the first `.`) to
`group`; for a toolchain-resolving rule an unknown group is `Tried to set
exec_properties for non-existent exec groups on L: a,b (did you mean 'g'?)`
at the target's location (`skyframe/ConfiguredTargetFunction.java:390-395,
468-469`). Per group the combined properties are the execution platform's
default-group properties, the target's default-group properties, the
platform's group properties, the target's group properties (later wins;
the target's default properties fill keys its group row lacks). A configurable
`exec_properties` value and a repeated cell (`k` with
`default-exec-group.k`; Bazel fails in ImmutableTable.Builder.buildOrThrow)
fail explicitly.

## config.exec(exec_group = NAME)

DependencyResolutionHelpers.getExecutionPlatformLabel
(`analysis/DependencyResolutionHelpers.java:223-253`): an edge whose exec
transition names a group of the ToolchainCollection uses that group's
execution platform (its own exec configuration, crate::config_registry);
`default-exec-group` is the default group; an unknown group is `Attr 'A'
declares a transition for non-existent exec group 'G'` for a rule with
toolchain contexts and a no-op transition otherwise (docs/CONFIGURATION.md).
rules_python's coverage attributes (`cfg = config.exec(exec_group =
"test")`) use the default test group.

## Genuine rule sets

Read-only sources in the external cache:

* rules_python 1.7.0: `python/private/attributes.bzl:39-45`
  (REQUIRED_EXEC_GROUP_BUILDERS `cpp_link`, `py_precompile`), `:350-353`
  (`_use_auto_exec_groups = True`), `:358-370` (coverage attributes with
  `config.exec(exec_group = "test")`); `py_library.bzl:278`,
  `py_executable.bzl:1820-1827` (groups and optional toolchains);
  `precompile.bzl:205` (`toolchain = EXEC_TOOLS_TOOLCHAIN_TYPE`).
* rules_go: `go/private/rules/binary.bzl:174, 500-504` and
  `go/private/rules/test.bzl:195, 497-501` (`go_link =
  exec_group(toolchains = [GO_TOOLCHAIN] + CGO_TOOLCHAINS)`,
  `link_exec_group = "go_link"`), `go/private/actions/link.bzl:206-216`
  (`go.actions.run(..., toolchain = GO_TOOLCHAIN_LABEL, exec_group =
  exec_group)`; without automatic exec groups the pair is not checked),
  `proto/def.bzl:234-236`.
* rules_cc: `cc/private/rules_impl/cc_binary.bzl:904-906`,
  `cc_library.bzl:953-960` (`cpp_link`, `_use_auto_exec_groups`),
  `cc_test.bzl:60, 160-164` (a declared `test` group with an optional type,
  `ctx.exec_groups["test"].toolchains[...]`),
  `cc/private/link/finalize_link_action.bzl:367-395` (`exec_group =
  "cpp_link"`).

The paired capture's chain E reproduces these declaration shapes with local
types. Genuine rules_python/rules_go/rules_cc targets themselves still stop
before analysis at other gaps (for example rules_java's registered
toolchains, `java_common.google_legacy_api_enabled is not implemented in the
native profile`, and `cc_toolchain` analysis).

## Divergences and explicit failures

* One execution platform (the host platform): registered execution
  platforms fail explicitly (docs/TOOLCHAINS.md), so per-group platform
  selection only filters the host platform.
* Exec properties are computed and recorded per action but not applied:
  actions run locally.
* D3 (docs/TEST_FLAGS.md): without the built-in profile the default `test`
  group exists (names, `config.exec(exec_group = "test")`, the
  toolchain-parameter count) but is not resolved; its
  `ctx.exec_groups["test"].toolchains` fails explicitly.
* `ctx.exec_groups` of a rule without toolchain resolution (build settings):
  Bazel 9.3 has no ToolchainCollection there and fails with a
  NullPointerException (AutoValue_StarlarkExecGroupCollection); natively an
  explicit failure.
* Exec groups of aspects (aspects are not analyzed), `exec_group` of
  `testing.ExecutionInfo`, genrule/native-rule exec groups, and the
  action-execution side of N23d (below).

## Merge with N23d (done in integration-13)

`run`/`run_shell` keep N23d's shared `SpawnOptions`/`register_spawn`; N23d's
explicit `exec_group != None` rejection is replaced by this increment's
analysis layer: `group_arguments` (types, subrule overrides) runs first,
`run_shell`'s early `verify_exec_group`, `GroupArguments::check_tool` at the
executable (`run`) and at each tool (`action_tools`), and
`GroupArguments::determine()` after the tools, recorded in
`Registration.exec_group` and so `RegisteredAction.exec_group`. N23d's
formerly ignored `toolchain` parameter is this one. Writes, symlinks and
templates are not spawns and stay in the default group. The engine's
`action_executions` record (per planned action, in N30's
`Engine::plan_target`) stays the only action-execution change; N36's
staging is unaffected. N23c's always-empty `ctx.exec_groups` stub is
replaced by this collection (docs/CTX_SURFACE.md), and `ctx.var` takes the
TemplateVariableInfo of every ToolchainCollection context's toolchains
(RuleContext.fromToolchains, analysis/RuleContext.java:914-924).

## Tests

* Unit: `src/exec_groups.rs` (names, InvalidExecGroupException,
  exec-property precedence, action group choice and subrule overrides),
  `src/engine.rs` (`actions_run_in_their_exec_groups_with_combined_properties`),
  `src/analysis_ctx.rs`, `src/configuration.rs`.
* CLI: `tests/exec_groups.rs` over `tests/fixtures/exec_groups/workspace`
  (N30's stubs), and the updated rows of `tests/toolchains.rs`.
* Paired capture: `artifacts/exec_groups-capture.py`, receipt
  `tests/fixtures/exec_groups/reference_receipt.json` (evidence only).
