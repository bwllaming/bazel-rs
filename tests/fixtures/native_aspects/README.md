# native_aspects

Starlark aspects applied to targets of NATIVE rule classes (Bazel 9.3 applies
an aspect to any rule target; docs/ASPECTS.md "Native base rules").

`asp` (`attr_aspects = ['*']`, returns a provider and an output group, writes
one file) runs over `genrule` (also `executable = True`), `filegroup`,
`alias` (seen through: the application is the actual's), `label_flag`
(likewise), `test_suite`, `config_setting`, `config_feature_flag`,
`platform`, `toolchain`, `toolchain_type`, `constraint_setting` /
`constraint_value` (no-config rule transition: no application), a Starlark
rule, source files and an output file (no application). It prints, per
application in the main repository, `ctx.rule.kind` and the attribute names
(`K`), every `ctx.rule.attr` value with its type (`V`), `ctx.rule.files`
(`F`), `ctx.rule.file`/`ctx.rule.executable` (`S`) and the providers it saw
on the propagated dependencies (`P`); `root` prints what its `deps` carry
(`R`). `pasp` (`required_providers = [PlatformInfo]`) applies to a platform
only (`Q`, `O`). `multirun` is rules_multirun's shape: an aspect on
`commands` reading `ctx.rule.attr.args`/`env` with `getattr` defaults over a
genrule command, its alias and filegroups (`M`, `N`).

`genuine.txt` holds the genuine 9.3.0 capture of both commands (a fresh
output base each): `build --nobuild //:r //:mr` and the top-level
`build --aspects=//:defs.bzl%asp --output_groups=asp //...`, whose aspect
outputs are `bazel-bin/{fg,fg2,g,gx,l,mr,r,tool_fg,tty,tty2,vis}.asp`
(manual targets are not top-level ones). tests/aspects.rs
`aspects_apply_to_native_rule_targets` compares the native prints and
outputs with it.
