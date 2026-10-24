# `ctx.actions.run(unused_inputs_list = ...)`

Bazel 9.3's `run` takes `unused_inputs_list`, a File or None
(StarlarkActionFactoryApi.java:423-438; another type fails at the call
binding: `in call to run(), parameter 'unused_inputs_list' got value of type
'T', want 'File or NoneType'`). The native profile accepts it like Bazel
(`src/analysis_runtime.rs`): the action gets Bazel's
`internal-inline-outputs = <list exec path>` execution requirement
(StarlarkAction.java:177-189, ExecutionRequirements.REMOTE_EXECUTION_INLINE_OUTPUTS)
and otherwise executes unchanged. The genuine capture is
`tests/fixtures/unused_inputs_list/reference_receipt.json` (replayed by
`tests/unused_inputs_list.rs`): the aspect_bazel_lib `tar.bzl` shape (the
list written by another action and passed as an input) and a list the action
writes itself build the same outputs, also after an unused and a used input
change.

## Divergence (declared)

Bazel reads the list after the spawn (EnhancedStarlarkAction.afterExecute,
:400-445) and keeps only the used inputs in its action cache entry, so a
later change of an unused input is an action cache hit (the capture's
`unused_changed`: `4 action cache hit`). The native action cache keys an
action by all its inputs: such a change re-executes it (`2 actions executed,
2 cache hits`), with the same outputs. A list that is missing after the spawn
(Bazel: `Action did not create expected output file listing unused inputs:
PATH`) is not checked, and the action stays shareable.
