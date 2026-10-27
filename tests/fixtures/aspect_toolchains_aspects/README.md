# aspect_toolchains_aspects

`toolchains_aspects` (a fixed list, `'*'` and a function) and the aspect's
`ctx.rule.toolchains` (AspectFunction.getBaseTargetToolchainContexts,
AspectBaseTargetResolvedToolchainContext, AspectResolutionHelpers.
computePropagatingAspectsToToolchainDep): `uses_tc` resolves `//tc:t` and
`//tc:u`; `ta` propagates to the `t` toolchain (and on along its `deps`),
`tfa` to the type its function computes from the `which` parameter, `tall`
to every toolchain; `ctx.rule.toolchains[T]` is a
`ToolchainAspectsProviders` with the propagated aspects' providers and the
toolchain's label; `plain` indexes an unrequested type. `platforms` is
extracted by the test (tests/aspects.rs `toolchains_fixture_named`).
`genuine.txt` holds the genuine 9.3.0 capture; tests/aspects.rs
`aspects_propagate_to_base_target_toolchains` compares the native run.
