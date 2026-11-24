# aspect_transition_attribute

B79 of docs/OSS_CORPUS.md (aspect-bazel-examples' `//tools:bazel_env`): an
attribute aspect applied to a target whose own attribute both has a
Starlark transition and declares aspects (bazel_env.bzl's
`_toolchain.target`, `cfg = _flip_output_dir, aspects =
[_extract_toolchain_info]`). The aspect's `ctx.rule.attr.target` is that
attribute's aspect view, a list (a Starlark-transition attribute). Genuine
9.3.1 (`--batch --ignore_all_rc_files build --platforms=//:p --host_platform=//:p
--aspects=//:defs.bzl%top //:e`)
builds and writes `bazel-bin/e.txt` = `@@//:w: list @@//:leaf @@//:leaf\n`;
bazel-rs 0.5.0 failed `in //:defs.bzl%extract aspect on wrap rule //:w:
attribute target: dependency //:leaf was not resolved` (with or without the
top-level aspect). Pinned by tests/aspects.rs
`transition_attributes_with_aspects_resolve_in_aspects`.

`rule_transition/`: a top-level aspect (`--aspects=//:defs.bzl%top`) on a
target whose rule transition is not idempotent (bazel_env.bzl's
`_flip_output_dir` as a rule `cfg`): the transition's delegate is analyzed
under its own key (ConfiguredTargetKey.shouldApplyRuleTransition false),
and the aspect application is keyed by it too, so its base rule's exec edge
(the `_allowlist_function_transition` package group) is in the delegate's
exec configuration. Genuine 9.3.1 (`build --platforms=//:p
--host_platform=//:p --aspects=//:defs.bzl%top //:e`) prints `DEBUG:
<WS>/defs.bzl:39:10: top on @@//:e: mode=opt tool=@@//:tool`; bazel-rs
0.5.0 failed `internal analysis order error:
@@bazel_tools//tools/allowlists/function_transition_allowlist:function_transition_allowlist`.
