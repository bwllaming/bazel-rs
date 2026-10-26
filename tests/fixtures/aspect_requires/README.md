# aspect(requires = [...]) fixture

`workspace/` is the workspace of tests/aspects.rs
`required_aspects_expand_like_aspects_list` (docs/ASPECTS.md "Required
aspects"): `asp_b` requires `asp_a` (which does not propagate itself),
`asp_e` requires `asp_b` (transitively `asp_a`), `asp_m` requires `asp_n`
(whose required providers no rule advertises), and `asp_q` sees `asp_p`
through `required_aspect_providers` only. `err/` holds the
`attr.label_list(aspects = ...)` lists AspectsList.Builder rejects.

`reference_receipt.json` is the paired genuine Bazel 9.3.0 / native capture
(/tmp/opencode/perf-research/preq-capture.py and preq-receipt.py, not
committed); evidence only, no test reads it.
