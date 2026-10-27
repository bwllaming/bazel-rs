# N58: Starlark aspects

Native analysis of Starlark-defined aspects at Bazel 9.3 semantics
(`aspect()` definitions were already validated at load since N33,
docs/LOAD_DECLARATIONS.md). Expected behaviors are checked against a paired
genuine Bazel 9.3 capture (tests/fixtures/aspects/reference_receipt.json);
native-only boundaries fail loudly, never silently.

## Definition

`aspect()` (N33) keeps Bazel's parameter names, validation and errors at
load. At application, the native profile supports: `implementation`
(called as `f(target, ctx)`, `StarlarkAspectFactory.create`),
`attr_aspects` and `toolchains_aspects` (fixed lists or functions),
`propagation_predicate` (see "Propagation functions" and "Toolchain
propagation"), `attrs` (implicit `_`-attributes and
parameter attributes), `required_providers`, `required_aspect_providers`,
`provides`, `requires`, `apply_to_generating_rules`, `mandatory_providers`
wording, `toolchains`, `exec_compatible_with`, `exec_groups`, `fragments`
(and `host_fragments`, which Bazel 9.3 ignores) — see "Aspect toolchains,
exec groups and fragments". Aspect-declared features whose application is
not modelled fail explicitly when the aspect applies (never a silent
no-op): `subrules`:

```
aspect //:gaps.bzl%subrules_aspect declares subrules: not analyzed by the native profile
```

## Propagation functions

A function `attr_aspects` or `toolchains_aspects` and the
`propagation_predicate`
(crate::aspect_propagation; AspectPropagationEdgesSupplier,
AspectPropagationPredicate, StarlarkAspectPropagationContext) are called
with a `propagation_ctx`: `ctx.attr` is a struct of the aspect's public
parameters (AspectParameters: every parameter of a top-level aspect, the
rule-extracted ones of an attribute's aspect); `ctx.rule`
(`StarlarkAspectPropagationRule`) has `label`, `qualified_kind`
(`QualifiedRuleKind`: `file_label`, the `.bzl` that defined the rule class,
and `rule_name`) and `attr`, whose fields are `RuleAttribute`s (`value`,
`is_tool` = an exec-transitioned attribute). For the edge function
`ctx.rule.attr` holds every attribute of the configured (select-resolved)
target, label attributes as Labels
(convertAttributeValueForAspectPropagationFunc); for the predicate only
`tags` (`Only rule's "tags" are available in propagation_predicate
function.`).

- The edge function runs per configured target the aspect applies to
  (AspectResolutionHelpers.computeAttributeAspects in DependencyMapProducer;
  `Engine::with_computed_attr_aspects`, memoized per (target, descriptor),
  so its `print()`s appear once) and its list names the attributes the
  aspect propagates along from THAT target (`'*'` is rejected:
  `'*' is not allowed in 'attr_aspects' list`; a non-list `Expected a
  list`; both at the target's location).
- The predicate runs in computeAspectCollection for each aspect the target
  admits (required providers, or required by a kept aspect), also for a
  top-level target: `False` drops the aspect there (a top-level target
  then gets no application); a non-boolean is `Expected a boolean`.
  Memoized per (label, descriptor).

Paired capture tests/fixtures/aspect_propagation_functions (genuine.txt;
tests/aspects.rs `aspect_propagation_functions`).

## Toolchain propagation

`toolchains_aspects` (labels, `'*'`, or a function returning Labels,
evaluated per target like the edge function) propagates an aspect to the
base target's RESOLVED toolchains whose type it names (the type label or
any requested alias of it; computePropagatingAspectsToToolchainDep, the
path top-down with required aspects pulled in), in the base target's
configuration like the toolchain dependencies (`Engine::
base_target_toolchains` over the default and exec-group contexts; phase 1
`analyze_aspect_applications`, planning `plan_aspect_application`). The
aspect then propagates on from the toolchain target along its own
`attr_aspects`.

`ctx.rule.toolchains` (StarlarkAttributesCollection.toolchains over
AspectBaseTargetResolvedToolchainContext) is a `ToolchainContext` of the
base target's requested types (`<toolchain_context.resolved_labels: //tc:t,
//tc:u>`, empty for a rule without toolchain types) whose entries are
`ToolchainAspectsProviders` (`<ToolchainAspectsProviders for toolchain
target: //tc:t_impl>`): `label`, and indexing/`in` over the providers of
the aspects applied to that toolchain only (none where no aspect
propagated; the toolchain's own providers are never visible there). An
unrequested type fails `In aspect A applied to KIND rule L, toolchain type
T was requested but only types [...] are configured`. Paired capture
tests/fixtures/aspect_toolchains_aspects (genuine.txt; tests/aspects.rs
`aspects_propagate_to_base_target_toolchains`). Not projected into cquery's
aspect edges.

## Application and propagation

- `attr.label/list/dict(aspects = [a, b])` attaches the aspects to that
  attribute's dependencies (the attribute's list first, then the incoming
  aspect path — `computePropagatingAspects`, AspectResolutionHelpers.java:
  86-92). The aspect values a rule class captures may be private
  (underscore) top-level bindings of the rule's own `.bzl` — rules_go's
  `_go_cc_aspect` of `binary.bzl`, referenced same-file; Bazel keeps the
  aspect value itself, so the identity re-load here reads the binding at
  any visibility, while top-level `--aspects` stay public-only like a
  `load()` (N58 on N57, integration-17).
- An aspect application's dependencies include the base rule's
  Starlark-transitioned prerequisites under their transition keys
  (`go_library`'s `srcs` go config transition; AspectFunction over
  `TransitionedConfiguredTarget` keys, N28c).
- An aspect propagates from a target to the dependencies of the attributes
  named in its `attr_aspects` (`'*'` for all) — the propagation happens on
  the dependency's attributes (`collectPropagatingAspects`), recursively:
  applying an aspect to a target applies it along the target's whole
  `attr_aspects` closure.
- On each dependency, the aspects the target does not admit are silently
  dropped (`computeAspectCollection`: `required_providers` unsatisfied)
  unless an already-kept aspect explicitly `requires` them
  (`isAspectRequired`; `required_aspect_providers` only decides
  visibility); a repeated aspect that would see a different aspect set
  fails (`AspectCollection.create`'s `AspectCycleOnPathException`,
  `Aspect X is applied twice, both before and after aspect Y (when
  propagating to //pkg:t)`).

### Required aspects (`aspect(requires = [...])`)

- An `attr.*(aspects = [...])` list and the `--aspects` list are an
  `AspectsList` (AspectsList.Builder.addAspect, packages/AspectsList.java:
  331-425): each aspect is preceded by its required aspects (depth first,
  recursively), each aspect once; listing a required aspect explicitly
  BEFORE its requirer is legal, after it fails `aspect X was added before
  as a required aspect of aspect Y`, a repeated aspect `aspect X added more
  than once` (an `attr.label_list` error at load; for `--aspects` the
  build's error).
- A required aspect propagates along every edge its requirer propagates
  along, even without `attr_aspects` of its own (`collectPropagatingAspects`'
  `isAspectRequired`: explicit `requires` only — an aspect seen only
  through `required_aspect_providers` does not follow its viewer).
- The requirer sees its required aspects' providers on `target` without
  `required_aspect_providers` (`AspectCollection.create`: visible =
  required_aspect_providers satisfied OR required); the `target` merges the
  DIRECTLY visible aspects only (a transitively required aspect's providers
  are not on it).
- A required aspect whose `required_providers` the target does not satisfy
  stays on the path (and in the key) but is a `NonApplicableAspect`
  (AspectFunction.java:332-339): no implementation call, no own
  dependencies, no providers.
- Top-level required aspects take the `--aspects_parameters` values, and
  the parameter validation (`Missing mandatory attribute 'p' for aspect
  'X'.`, the unused-parameter error) runs over the expanded list.
- Paired capture tests/fixtures/aspect_requires (reference_receipt.json,
  11/11 MATCH; tests/aspects.rs `required_aspects_expand_like_aspects_list`).
- Output-file edges apply the aspect to the generating rule only with
  `apply_to_generating_rules` (the native ValidateTarget applies to files);
  aliases apply at the alias chain's end; aspects never apply to source
  files or package groups (`AspectFunction.java:327-341`).
- An attribute's aspect takes its parameters from the rule declaring the
  attribute: the values of its same-named, same-typed attributes
  (StarlarkDefinedAspect.getDefaultParametersExtractor), shown in its
  descriptor (`//:defs.bzl%a[p="x",n="2"]`, `ctx.aspect_ids`). Applications
  are identified by descriptor (class + parameters, AspectCollection over
  AspectDescriptor): two rules with different values apply the aspect twice to
  shared dependencies, and the same aspect from `--aspects` (every parameter,
  given or default) and from an attribute is two applications when the
  parameters differ, one when they are equal (paired capture
  tests/fixtures/aspect_parameters). The `Aspect X of L up-to-date:` lines
  name the class.
- Applications are keyed per aspect like Bazel's `AspectKey`
  (base configured target key + aspect descriptor + directly required aspects);
  aspect actions register under the application's own action owner
  (`label[config]%aspect`), so the base target's and several aspects'
  actions never collide (duplicate declared output paths still fail, the
  engine's overlapping-output check), except that equal actions of two
  applications on one path are shared like any two owners' (Actions.canBeShared,
  docs/SHAREABLE_ARTIFACTS.md). An aspect's spawns carry its base rule's
  execution-requirement tags (an aspect's `RuleContext.getRule` is the base
  target), its writes and transforms no execution info; the base target's
  `exec_properties` are not the aspect actions' (`ConfiguredTargetFactory.createAspect`'s
  `setExecProperties(ImmutableMap.of())`, `analysis/ConfiguredTargetFactory.java:702`;
  two applications' equal spawns share whatever their base targets'
  `exec_properties`, tests/fixtures/shared_actions `aspect_shared`). An
  aspect's action is otherwise planned like a rule's (`Engine::commit_action`):
  its tools' runfiles trees are staged (shared input groups,
  docs/RUNFILES_TREES.md) and a command line expanding tree artifacts is
  completed when it executes (PA-1 follow-up B6; the aspect commit used to
  drop both; tests/runfiles_trees.rs `aspect_actions_stage_their_tools_runfiles`,
  outputs byte-identical to genuine Bazel 9.3.0's).

## Providers: the aspect-configured target

An application's returned providers merge with the base configured target's
into the aspect-configured target consumers see (`MergedConfiguredTarget.of`).
Aspect providers are found FIRST — the lookup checks them before the base
target's (`rawGetStarlarkProvider`), and `ctx.attr.dep[Provider]` on an
aspect-applied dependency resolves in that merged map. Only four providers
are special-cased in the merge; every other provider an aspect returns that
the base target or an earlier aspect also provides fails with
`Provider DupInfo provided twice` (display name, at the consumer's rule
location):

- `OutputGroupInfo`: merged per group — a group named twice unions its
  files, no error (`mergeOutputGroupProviders`);
- `DefaultInfo`: an aspect's must only set `files` (else `Provider
  'DefaultInfo' returned by an aspect not at top level must only have the
  'files' field set`); its files join the merged files to build;
- `InstrumentedFilesInfo`: when it is on both the base target and an
  aspect, the base's copy is IGNORED — the aspect's wins, without an error,
  because a rule and a related aspect may legitimately share the
  instrumentation implementation (`MergedConfiguredTarget.java:296-305`);
  two aspects both returning it still fail `Provider
  InstrumentedFilesInfo provided twice`;
- (the internal `FileProvider`/`AnalysisFailureInfo`/
  `RequiredConfigFragmentsProvider` merge internally and never fail the
  merge; they are not Starlark-visible here.)

## The aspect context

`ctx` of an aspect application (`StarlarkRuleContext` with an
AspectDescriptor):

- `ctx.attr`, `ctx.file`, `ctx.files`, `ctx.executable`: the aspect's OWN
  attributes (StarlarkRuleContext.java:270-285 over the main aspect's
  attributes, StarlarkAttributesCollection.Builder.addAttribute) — its
  implicit label attributes (resolved as dependencies from the aspect's
  `.bzl` package, visibility-checked from there, each in its attribute's
  configuration) and its parameter values (`--aspects_parameters` or
  defaults). `ctx.attr` has no `name`; a label list is a list of Targets (an
  alias as its actual); every dependency attribute has a `ctx.files` entry
  (each File once, PrerequisiteArtifacts.nestedSet), an `allow_single_file`
  one a `ctx.file` entry, an executable label one a `ctx.executable` entry
  (an exec- or target-configured tool, a source file), and a spawn running
  that File stages its FilesToRunProvider's runfiles; string dicts and int
  lists are plain values; label-keyed dictionary attributes fail explicitly
  (paired capture tests/fixtures/aspect_ctx_attributes, 2/2 MATCH; tests/
  aspects.rs `aspect_context_lists_its_own_attributes`);
- `ctx.rule`: the base target's attributes — `ctx.rule.attr.<name>` values
  are the aspect-applied `Target`s for attributes the aspect (or the path)
  propagates along, plain configured targets otherwise; `ctx.rule.kind` is
  the base rule class. `ctx.rule.attr`/`files`/`file`/`executable` also
  hold the attributes of the path's BASE aspects (StarlarkRuleContext.java:
  302-316 over AspectContext's rule-and-base-aspects prerequisites): the
  rule's attributes win a name, then the base aspects in path order (the
  first attribute of a name wins, StarlarkAttributesCollection.Builder.
  addAttribute); a base aspect's parameters show their (top-level) values.
  Their label attributes are dependencies of the main aspect's application
  (DependencyResolutionHelpers.getAttributes/addAspectAttributes), each in
  its attribute's configuration over the MAIN aspect's execution platforms
  (`Engine::analyze_aspect_attribute_edges`, memo `owner@main`), visibility-
  checked from the base aspect's `.bzl` package. The path's other aspects
  propagate along an aspect's own attributes when their `attr_aspects`
  name them (collectPropagatingAspects' `aspectOwningAttribute`: never the
  owner itself) — a base aspect with `attr_aspects = ["_mdep"]` applies to
  the main aspect's `_mdep` dependency. The path is the topological order
  over the AspectKey's baseKeys sorted by aspect class name
  (AspectKeyCreator.createAspectKey:127-133), so `ctx.aspect_ids` of an
  aspect requiring `base` and seeing `seen` is `[base, seen, main]` whatever
  the attribute's list order. Paired capture tests/fixtures/
  aspect_base_attributes (genuine.txt; tests/aspects.rs
  `ctx_rule_attr_holds_base_aspect_attributes`). Not captured into cquery's
  aspect edges (crate::engine::cquery records the main aspect's own
  attribute edges only);
- `ctx.aspect_ids`: the descriptions of the application's aspect path —
  its base aspects in topological order (a postorder over the directly
  required aspects, AspectFunction.collectAspectKeysInTopologicalOrder),
  then itself; a top-level aspect's description lists every parameter
  attribute's value, given or default (`//:defs.bzl%a[tag="t0"]`,
  AspectDescriptor.getDescription), an attribute's aspect is its class
  name. The same path names the aspect in `ASPECTS aspect on KIND`
  messages;
- `ctx.label`, `ctx.actions`, `ctx.var`, `ctx.expand_location`,
  `ctx.fragments` (the base target's configuration, the aspect's fragment
  policy), the shared configuration surface; `ctx.toolchains` and
  `ctx.exec_groups` of the aspect's own ToolchainCollection;
- `ctx.outputs` fails with `'outputs' is not defined`, `ctx.super` with
  `Can't use 'super' call in an aspect.`, `ctx.rule`/`ctx.aspect_ids`
  in a rule context with Bazel's
  `'rule'/'aspect_ids' is only available in aspect implementations`.

The `target` parameter is the base configured target merged with the
directly required aspects' providers (`AspectFunction.java:341-346`), so an
aspect reads its required aspects' providers through `target[Provider]`.

## Aspect attribute configurations

Every label attribute of an aspect is resolved per application in its own
configuration (AspectFunction's dependency resolution: DependencyProducer
over the main aspect's attributes, `analysis/producers/DependencyProducer.java:184-219`,
with `DependencyResolutionHelpers.getExecutionPlatformLabel`, `:195-253`),
never the base rule's attribute configuration:

- `cfg = "target"`: the base configured target's configuration;
- `cfg = "exec"` / `config.exec()`: the exec transition
  (ExecutionTransitionFactory) for the execution platform of the ASPECT's
  own default exec group — its toolchain context
  (`AspectFunction.getUnloadedToolchainContextsInputs`, `AspectFunction.java:811-838`,
  see "Aspect toolchains, exec groups and fragments"), the host platform
  natively (the only execution platform, docs/TOOLCHAINS.md) — so a tool
  lands in `bazel-out/k8-opt-exec`; on an
  exec-configured base target the exec transition of the exec
  configuration is itself;
- `config.exec("g")`: the execution platform of the aspect's exec group
  `g`; of an exec group the aspect does not declare:
  DependencyProducer.ExecGroupErrorEmitter at the base target, with the
  attribute's native name (`BUILD.bazel:12:4: Attr '$tool' declares a
  transition for non-existent exec group 'nope'`);
- a Starlark transition: applied to the base configuration with the base
  target's attributes as `attr` (AttributeTransitionData.attributes); the
  `transition()` objects are kept on the aspect value (`AspectGen::transitions`)
  and the configuration is memoized with the application. Like a rule's, a
  Starlark-transitioned label attribute is a LIST in `ctx.attr`
  (StarlarkAttributesCollection.convertAttributeValue: `isSplit()`);
- `config.none()` and `analysis_test_transition` fail explicitly, as on a
  rule attribute.

Phase 1 (`Engine::analyze_aspect_applications`, also for the top-level
`--aspects` over each requested target) computes these configurations
(`Engine::aspect_attribute_configurations`, memoized per application key)
and analyzes the attribute dependencies there; the application's planning
reads the memo, so every application the planner reaches was analyzed:
both walk the base rule's edges alike (Starlark-transitioned edges in their
transitioned configuration; aspects never apply in the no-config
configuration, `AspectFunction.java:296-305` NonApplicableAspect), and the
application's configuration is the base target's after a rule transition's
delegation. The aspect's attributes are a prerequisite collection of their
own (`AspectApplication::aspect_dependencies`,
AspectContext.getMainAspectPrerequisitesCollection), so an aspect attribute
and a base rule attribute naming the same label never share a view.

The aspect context's configuration surface is the base configured
target's configuration (the aspect's RuleContext is built in it,
ConfiguredTargetFactory.createAspect): `ctx.bin_dir`, `ctx.var`,
`ctx.configuration`, `ctx.fragments` and `ctx.target_platform_has_constraint`
read it (they read a default fastbuild surface before: an aspect on an
exec-configured target saw `bazel-out/k8-fastbuild/bin`, genuine
`bazel-out/k8-opt-exec/bin`).

Paired capture tests/fixtures/aspect_configuration (reference_receipt.json,
4/4 MATCH; tests/aspects.rs `aspect_attributes_take_their_own_configurations`,
`undeclared_aspect_exec_group_fails_at_the_base_target`); the oracle repro
`/tmp/opencode/oracle-aspect-exec/ws` (genuine `bazel-out/k8-opt-exec/bin/tool`)
now matches. Aspect actions running such tools changed their keys once (the
tools moved to exec paths); ~/code plans no aspect-owned action.

## Aspect toolchains, exec groups and fragments

An aspect resolves its OWN ToolchainCollection in the base target's
configuration (`AspectFunction.getUnloadedToolchainContextsInputs`,
`AspectFunction.java:811-838`, with DependencyContextProducer and
UnloadedToolchainContextsProducer; `Engine::aspect_toolchain_contexts`,
memoized per (configuration, aspect), resolved in phase 1 before the
aspect's attribute configurations), never the base rule's: the default exec
group over the aspect's `exec_compatible_with` and — without automatic exec
groups — its `toolchains` (`createDefaultToolchainContextKey`, no forced
execution platform), then DeclaredExecGroup.process's groups: the aspect's
`exec_groups`, and under automatic exec groups (an `_use_auto_exec_groups`
attribute, else `--incompatible_auto_exec_groups`; `shouldUseAutoExecGroups`)
one group per toolchain type. The resolved toolchain implementations are
dependencies of the application, analyzed in the base configuration
(DependencyKind.forExecGroup). A resolution failure is the application's
error as is (the ToolchainException message, e.g. `No matching toolchains
found for types:`; AspectFunction's AspectCreationException), not a target's
`While resolving toolchains for target` error.

At analysis (`Engine::plan_aspect_application`, `analysis::AspectToolchains`):
each context is loaded (ResolvedToolchainContext.load) described `aspect A
applied to KIND rule L` (`AspectFunction.java:468-485`), so `ctx.toolchains`
is always a valid context of the aspect (repr `<toolchain_context.resolved_labels:
//tc:t, //tc:optional_t>`, None for an unresolved optional type, `In aspect
//:defs.bzl%plain_aspect applied to lib rule //:a, toolchain type //tc:t was
requested but only types [] are configured` for an unrequested one; under
automatic exec groups the automatic groups' types); `ctx.exec_groups` is the
aspect's collection; `ctx.actions.run`/`run_shell` take `exec_group =` and
`toolchain =` against it, and each action's execution platform and exec
properties come from the aspect's execution table over NO `exec_properties`
(an aspect's RuleContext takes none, `ConfiguredTargetFactory.createAspect:702`)
— an aspect action's key now carries its execution platform like a rule's.
`ctx.var` adds the TemplateVariableInfo of the aspect's resolved toolchains
(RuleContext.fromToolchains). The aspect's `cfg = "exec"` attributes use the
default group's execution platform, `config.exec("g")` the group's.

`fragments` is the aspect's configuration fragment policy
(`ConfiguredTargetFactory.createAspect:692`), and the aspect's RuleContext
names itself `ASPECTS aspect on KIND` (`RuleContext.Builder.getRuleClassNameForLogging`,
`:1908-1917`): `ctx.fragments.cpp` of an aspect without `fragments = ["cpp"]`
fails `//:defs.bzl%nofrag_aspect aspect on lib has to declare 'cpp' as a
required fragment in order to access it. ...` whatever the base rule
declares; `host_fragments` is accepted and ignored (Bazel 9.3's aspect()
never reads it).

Paired capture tests/fixtures/aspect_toolchains (reference_receipt.json, 7/7
MATCH; tests/aspects.rs `aspects_resolve_their_own_toolchains`,
`aspect_fragments_are_the_aspects_own`). rules_rust's `rust_clippy_aspect`
(`fragments = ["cpp"]`, the rust and optional C++ toolchain types, an exec
`_process_wrapper`) needs all three.

## Top-level aspects and validation

- `--aspects={bzl-file-label}%{name}` applies aspects to the top-level
  targets (comma-separated, repeatable); the legacy relative spelling
  converts to an absolute `.bzl` label (BuildView.java:566-583). Errors:
  `Aspect 'X' is unknown` (a non-native name), `Invalid aspect 'X': ...`
  (label syntax), `name from //:file.bzl is not an aspect`,
  `cannot load '//:file.bzl': no such file`, and
  `Parameters '[x]' are not parameters of any of the top-level aspects but
  they are specified in --aspects_parameters.`
- An aspect's artifacts in requested (non-`_`-prefixed) output groups are
  built and reported per aspect: `Aspect //:defs.bzl%count_aspect of //:b
  up-to-date:` with the artifact paths (BuildResultPrinter.outputAspects);
  a top-level aspect's dependency applications are analyzed but their
  outputs are not requested.
- `--experimental_use_validation_aspect` (with `--run_validations`, the
  default) switches the validation mode to ASPECT
  (OutputGroupInfo.determineOutputGroups requests `_validation_top_level`
  instead of `_validation`) and appends the native `ValidateTarget` aspect
  (rules/core/ValidateTarget.java): a non-recursive aspect that promotes
  the merged `_validation` outputs — the base target's and every applied
  aspect's — to `_validation_top_level`; built but never reported. Aspect
  `_validation` outputs otherwise flow through N56's machinery unchanged
  (docs/VALIDATION_ACTIONS.md).

## Identity of the aspect-applied `Target` values

In Bazel, an aspect-applied dependency is a `MergedConfiguredTarget` — an
ephemeral proxy created per consuming edge (`MergedConfiguredTarget.java:
55-64`: "This is an ephemeral object created only for the analysis of a
single configured target"), so two consumers (or two attributes of one
rule) holding the same dep with the same aspects hold DIFFERENT instances
with identical providers; plain base targets keep one `ConfiguredTarget`
instance per `ConfiguredTargetKey` (N55). The native profile shares one
`Target` value per aspect application key — the base `ConfiguredTargetKey`
plus the aspect and its directly required aspects (`AspectKey`) — so
equality across consumers is value-stable where Bazel's is per-edge; the
base target's own `Target` value keeps its per-`ConfiguredTargetKey` N55
identity unchanged, and depset snapshots taken from either agree.

## Native-only boundaries (explicit failures, documented)

- aspect `subrules` at application;
- parameter values of `--aspects_parameters` are string/bool/int/string-list
  typed by the attribute (AspectParameters); other types are rejected at
  aspect() load (N33). They apply to the top-level aspects (and the
  aspects those require) only;
- the rule-instantiation check that a rule propagating an aspect declares
  its required (`values`-restricted) parameter attributes (`Aspect X requires
  rule R to specify attribute 'p' with type int.`,
  AspectsList.validateRulePropagatedAspectsParameters) is not made: such a
  rule's aspect keeps the parameter's default.

## Divergences

- `str(Label)`/`str(Target)` render canonically (`@@//pkg:name`), the
  engine's label display (N38's display forms appear in diagnostics);
  content written through `str()` shows the canonical spelling.
- The dependency map is keyed per configured target (`(label, exec)`), so
  two attributes of one rule naming the same label share one dependency
  view: the union of their aspects applies (Bazel keeps per-edge views).
- One shared `Target` value per aspect application key (see the identity
  section); Bazel's `MergedConfiguredTarget` is per-consumer ephemeral
  (identical providers), so cross-consumer `Target` equality differs.
- `--aspect_filters` does not exist in Bazel 9.3 (probed in the reference
  tree); nothing to implement.
