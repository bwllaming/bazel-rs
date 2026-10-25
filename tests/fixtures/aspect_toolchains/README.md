# Aspect toolchains fixture

`workspace/` is the workspace of tests/aspects.rs
`aspects_resolve_their_own_toolchains` and
`aspect_fragments_are_the_aspects_own` (docs/ASPECTS.md "Aspect
toolchains, exec groups and fragments"): `tc/` declares toolchain types and
registered toolchains (one restricted to `@platforms//os:linux` execution);
`tc_aspect` declares a mandatory and an optional toolchain type, an exec
group, `exec_compatible_with` and `fragments = ["cpp"]` and runs actions with
`toolchain =` and `exec_group =`; `auto_aspect` uses automatic exec groups;
`nofrag_aspect`/`hostfrag_aspect` read undeclared fragments
(`host_fragments` is ignored by Bazel 9.3); `missing_aspect` requests a type
without toolchains; `plain_aspect` indexes `ctx.toolchains` with a type it
did not request. The base rule `lib` resolves no toolchains.

`reference_receipt.json` is the paired genuine Bazel 9.3.0 / native capture
(/tmp/opencode/perf-research/pasp-capture.py and pasp-receipt.py, not
committed; `platforms` extracted from
tests/fixtures/label_repo_name/platforms_archive.json); evidence only, no
test reads it. The test adds N30's builtin-closure stubs
(tests/fixtures/test_flags/stubs) as `local_path_override`s to keep the
built-in profile offline (exec_groups' approach); they register no toolchain
of the fixture's types.
