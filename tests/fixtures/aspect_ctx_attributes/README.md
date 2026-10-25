# Aspect context attribute structs fixture

`workspace/` is the workspace of tests/aspects.rs
`aspect_context_lists_its_own_attributes` (docs/ASPECTS.md "The aspect
context"): `attrs_aspect` declares executable label attributes (an exec-
and a target-configured tool, an `allow_single_file` source file), a single
file, a label list (with an alias and a duplicate file), a string dict, an
int list and a `values`-restricted parameter; it prints `dir()` of
`ctx.attr`/`ctx.files`/`ctx.file`/`ctx.executable` and their values, and
runs `ctx.executable._tool` (a tool reading its own runfiles).

`reference_receipt.json` is the paired genuine Bazel 9.3.0 / native capture
(/tmp/opencode/perf-research/pasp-capture.py and pasp-receipt.py, not
committed); evidence only, no test reads it.
