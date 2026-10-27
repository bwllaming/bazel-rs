# Design note (deferred): toolchain dependencies keyed by execution platform

Status: NOT implemented. The native profile fails explicitly
(`Engine::toolchain_implementation`):

```
toolchain implementation //tc:impl is needed for the execution platforms //p:a and //p:b in one configuration: configured targets keyed by their execution platform (ConfiguredTargetKey.executionPlatformLabel) are not implemented in the native profile
```

## Bazel 9.3

- A toolchain dependency (DependencyKind.isToolchain: a ToolchainDependencyKind
  of an exec group, or the BaseTargetToolchainDependencyKind an aspect reads
  for `toolchains_aspects`) is requested in the PARENT's configuration with
  `executionPlatformLabel` = the execution platform the parent's exec group
  resolved (analysis/producers/DependencyProducer.java:161-175,
  PrerequisiteParameters.getExecutionPlatformLabel). Every other edge passes
  `null`.
- ConfiguredTargetKey carries that label (skyframe/ConfiguredTargetKey.java):
  `(label, configurationKey, executionPlatformLabel, shouldApplyRuleTransition)`.
  The toolchain target's own toolchain resolution is forced onto it
  (ToolchainContextKey.forceExecutionPlatform: DependencyResolver.
  getUnloadedToolchainContextsInputs over `configuredTargetKey.getExecutionPlatformLabel()`),
  which is how a toolchain's own toolchains (and its `cfg = "exec"` tools)
  follow its parent's execution platform.
- Two parents (or two exec groups of one parent) whose groups select
  different execution platforms therefore get two configured targets of the
  same toolchain label in one configuration. Their actions have distinct
  owners (ActionLookupKey = the key) but the same output paths; identical
  actions are shared (Actions.canBeShared), differing ones are an action
  conflict.

## Native profile today

`ConfiguredTargetKey { label, config, apply_rule_transition }`;
`Engine::toolchain_implementations: BTreeMap<ConfiguredTargetKey, Label>`
records the one execution platform a toolchain implementation is needed on
per configuration and feeds `toolchain_context_key`'s `forced_exec`. A second,
different platform is the explicit failure above. With the default single
execution platform (the host) — every ~/code build — it never triggers.

## Implementation plan (when needed)

1. `ConfiguredTargetKey.exec_platform: Option<Label>` (None everywhere except
   toolchain edges), part of `Ord`/`Hash`, of `action_owner` (a suffix like
   Bazel's key toString) and of the snapshot codec (format bump;
   docs/ANALYSIS_SNAPSHOT.md).
2. Construct keyed toolchain edges in the three places that analyze/plan
   toolchain implementations: rule toolchain contexts (Engine::analyze_key's
   resolution and `load_toolchain_context`), exec groups
   (`exec_group_contexts`), aspect contexts (`aspect_toolchain_contexts`) and
   `base_target_toolchains` (toolchains_aspects). `load_toolchain_context`
   reads providers under the keyed key.
3. `toolchain_context_key` takes `forced_exec` from the key itself; drop
   `toolchain_implementations`.
4. The dependencies OF a keyed toolchain target are ordinary (None) keys:
   only the toolchain target itself is duplicated.
5. Output registration: two keyed instances register the same artifacts; the
   engine's shareable-action check (docs/SHAREABLE_ARTIFACTS.md) must treat
   equal actions as shared and report differing ones as Bazel's conflict.
6. Consumers that enumerate configured targets (cquery output, BEP
   `configured` events, `--output=files`, keep_going bookkeeping) must decide
   whether to show both instances; cquery prints each configured target —
   genuine prints the label once per key (to be captured).
7. Fixture: two exec platforms (`--extra_execution_platforms`), a toolchain
   type with one toolchain compatible with both, two rules whose
   `exec_compatible_with` select different platforms, the toolchain with a
   `cfg = "exec"` tool: genuine `cquery --output=label_kind 'deps(...)'`,
   build outputs and action keys.

Estimated touch: ~80 `ConfiguredTargetKey::new` sites in 11 files plus the
snapshot codec and cquery; deferred because no target on ~/code needs it.
