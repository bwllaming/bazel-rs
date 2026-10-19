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
`attr_aspects` (a fixed list), `attrs` (implicit `_`-attributes and
parameter attributes), `required_providers`, `required_aspect_providers`,
`provides`, `requires`, `apply_to_generating_rules` and `mandatory_providers`
wording. Aspect-declared features whose application is not modelled fail
explicitly when the aspect applies (never a silent no-op): a function
`attr_aspects`/`toolchains_aspects` or a propagation predicate,
non-empty `toolchains_aspects`, `toolchains`, `fragments`/`host_fragments`,
`exec_compatible_with`, `exec_groups` and `subrules`:

```
aspect //:defs.bzl%fragments_aspect declares fragments: not analyzed by the native profile
```

## Application and propagation

- `attr.label/list/dict(aspects = [a, b])` attaches the aspects to that
  attribute's dependencies (the attribute's list first, then the incoming
  aspect path — `computePropagatingAspects`, AspectResolutionHelpers.java:
  86-92).
- An aspect propagates from a target to the dependencies of the attributes
  named in its `attr_aspects` (`'*'` for all) — the propagation happens on
  the dependency's attributes (`collectPropagatingAspects`), recursively:
  applying an aspect to a target applies it along the target's whole
  `attr_aspects` closure.
- On each dependency, the aspects the target does not admit are silently
  dropped (`computeAspectCollection`: `required_providers` unsatisfied)
  unless an already-kept aspect requires them (`required_aspect_providers`,
  `requires`); required aspects are ordered before their dependents and a
  repeated aspect that would see a different aspect set fails
  (`AspectCollection.create`'s `AspectCycleOnPathException`).
- Output-file edges apply the aspect to the generating rule only with
  `apply_to_generating_rules` (the native ValidateTarget applies to files);
  aliases apply at the alias chain's end; aspects never apply to source
  files or package groups (`AspectFunction.java:327-341`).
- Applications are keyed per aspect like Bazel's `AspectKey`
  (base configured target key + aspect + directly required aspects);
  aspect actions register under the application's own action owner
  (`label[config]%aspect`), so the base target's and several aspects'
  actions never collide (duplicate declared output paths still fail, the
  engine's overlapping-output check).

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

- `ctx.attr`, `ctx.file`, `ctx.files`: the aspect's OWN attributes — its
  implicit label attributes (resolved as dependencies from the aspect's
  `.bzl` package, visibility-checked from there) and its parameter values
  (`--aspects_parameters` or defaults);
- `ctx.rule`: the base target's attributes — `ctx.rule.attr.<name>` values
  are the aspect-applied `Target`s for attributes the aspect (or the path)
  propagates along, plain configured targets otherwise; `ctx.rule.kind` is
  the base rule class;
- `ctx.aspect_ids`: the applied aspects' descriptions in application order;
- `ctx.label`, `ctx.actions`, `ctx.var`, `ctx.expand_location`,
  `ctx.fragments` (the base target's configuration), the shared
  configuration surface;
- `ctx.outputs` fails with `'outputs' is not defined`, `ctx.super` with
  `Can't use 'super' call in an aspect.`, `ctx.rule`/`ctx.aspect_ids`
  in a rule context with Bazel's
  `'rule'/'aspect_ids' is only available in aspect implementations`.

The `target` parameter is the base configured target merged with the
directly required aspects' providers (`AspectFunction.java:341-346`), so an
aspect reads its required aspects' providers through `target[Provider]`.

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

- function `attr_aspects`/`toolchains_aspects`, propagation predicates;
  non-empty `toolchains_aspects`;
- aspect `toolchains`, `fragments`, `host_fragments`,
  `exec_compatible_with`, `exec_groups`, `subrules` at application
  (`ctx.toolchains` of an aspect fails like a rule without toolchains);
- parameter values of `--aspects_parameters` are string/bool/int/string-list
  typed by the attribute (AspectParameters); other types are rejected at
  aspect() load (N33).

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
