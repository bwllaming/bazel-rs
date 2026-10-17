# N35 exec_groups fixture

`workspace/` is the workspace of `tests/exec_groups.rs` and
`artifacts/exec_groups-capture.py` (docs/EXEC_GROUPS.md): `eg/` declares
toolchain types, constraints, platforms and the Starlark rules exercising
declared and automatic exec groups, `ctx.exec_groups`, action
`exec_group`/`toolchain`, `exec_properties`, `exec_group_compatible_with`,
`config.exec(exec_group = ...)`, subrules, the default `test` group and the
declaration shapes of genuine rules_python/rules_go/rules_cc (chain E, with
local stand-in types); `tcs/` holds the registered toolchains. The root
`MODULE.bazel` is generated: `register_toolchains("//tcs:all")` plus N30's
builtin-closure stubs (`tests/fixtures/test_flags/stubs`, reused unchanged)
as `local_path_override`s. `reference_receipt.json` is evidence only (no test
reads it).
