# aspect_subrules

Subrules called from aspects (StarlarkSubrule.call over an aspect's
StarlarkRuleContext): `asp` declares `sub` (actions under the aspect) and
`outer` (which calls its own `inner`); `bad`/`nested` call undeclared
subrules (`aspect 'A' must declare 'S' in 'subrules'`), `//:br` a rule that
does (`rule 'R' must declare ...`); `attr_asp`'s subrule has an implicit
label attribute (a Target). `file_asp` passes an `allow_single_file` one,
which crashes genuine Bazel 9.3 (the native profile fails explicitly).
`genuine.txt` holds the genuine 9.3.0 capture; tests/aspects.rs
`aspects_call_their_subrules` compares the native run.
