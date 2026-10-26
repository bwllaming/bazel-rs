# Configured aspect dependency ordering

Aspect dependency discovery and implementation planning must walk the **same
configured attributes**. Loading declarations still contain `select()` syntax
and placeholder/default attribute values; they are not an analysis dependency
graph.

Previously `analyze_aspect_applications` traversed the loading declaration, while
`plan_aspect_application` used the resolved rule saved at phase-2 commit. A
selected dependency could therefore be absent from the first walk and present
in the second, producing `internal analysis order error: aspect ... was not
analyzed`. A `config_setting(flag_values=...)` can call `plan_pending` before all
roots finish phase 1, so this error can appear underneath a misleading secondary
"errors encountered resolving select() keys" message.

The engine now publishes its existing shared `Arc<Rule>` at phase-1 completion,
before discovering attached aspects. Both aspect walks read that configured
declaration by `ConfiguredTargetKey`. The phase-2 commit no longer republishes it.
No new provider graph, lock, global serialization or second implementation run
is introduced. This retains the same map of custom-rule declarations, just at the
correct analysis boundary. Aspect application/provider memoization is unchanged.

Pinned Bazel 9.3: `AspectFunction.java:710-729` requires the base configured target
and checks its configuration against the aspect key; `:466-479` constructs the
aspect with those configuration conditions and resolved dependency values.

`tests/fixtures/aspect_order` reproduces the original failure using a required
three-aspect chain, a selected dependency, two consumers sharing the application,
and a later custom setting that flushes pending providers. A genuine 9.3 build
returns the captured marker bytes and runs each implementation once (print order
is intentionally not compared to Skyframe completion order). Additional tests
cover incoming rule, outgoing split and exec configurations, selected-only graph
reachability, aspect filtering, binary hit/verify, and source-presence/`.bzl`
invalidation. No unsupported aspect is silently skipped.
