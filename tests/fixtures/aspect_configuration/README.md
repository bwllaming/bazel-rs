# Aspect attribute configurations fixture

`workspace/` is the workspace of tests/aspects.rs
`aspect_attributes_take_their_own_configurations` and
`undeclared_aspect_exec_group_fails_at_the_base_target` (docs/ASPECTS.md
"Aspect attribute configurations"): `cfg_aspect`'s label attributes use
`cfg = "exec"`, `config.exec()`, `cfg = "target"` and a Starlark
transition; it runs its exec tool in an action and prints the configuration
of every attribute's prerequisite and its own `ctx.bin_dir`/`ctx.var`,
applied top-level (`--aspects`) and through `attr.label_list(aspects = ...)`
of a target- and an exec-configured edge. `bad_group_aspect` names an exec
group it does not declare.

`reference_receipt.json` is the paired genuine Bazel 9.3.0 / native capture
(/tmp/opencode/perf-research/pasp-capture.py and pasp-receipt.py, not
committed); evidence only, no test reads it. The native runs use
`--experimental-builtin-modules` with the fixture registry
(tests/fixtures/builtin_modules/registry).
