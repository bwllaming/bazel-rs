# extra_execution_platforms fixture

`workspace/` is the workspace of tests/platforms.rs
`extra_execution_platforms_come_before_the_host_platform` and
`extra_execution_platform_errors_follow_bazel` (docs/TOOLCHAINS.md
"`--extra_execution_platforms`"): `uv_transition` is rules_uv's
`transition_to_target` (extra_execution_platforms = the target platforms);
`tool` resolves `//tc:t` (a `@platforms//os:freebsd` and a
`@platforms//os:linux` execution toolchain) and has an exec dependency,
`plain` has only the exec dependency (no toolchain types), `pip` reaches both
through the transition, `reader` reads the option and appends `//p:bar`,
`none_reader` sets it to `None`. `p/` has the platforms `foo` (freebsd) and
`bar` (openbsd), an alias of `foo` and a genrule. Every rule writes
`<name>.out` naming its toolchain and its exec dependency's output path, so
the output directories (ST- hashes, exec platform names) pin the
configurations and the selected execution platforms.

`reference_receipt.json` is the paired genuine Bazel 9.3.0 / native capture
(/tmp/opencode/perf-research/xep-capture.py and xep-receipt.py, not
committed; `platforms` extracted from
tests/fixtures/label_repo_name/platforms_archive.json): 11 MATCH, 3 declared
divergences (two registered labels of one platform crash Bazel 9.3 with exit
37, an explicit error natively; `query`'s exit code 2 versus the native 1);
evidence only, no test reads it. The tests add N30's builtin-closure stubs
(tests/fixtures/test_flags/stubs) as `local_path_override`s to keep the
built-in profile offline.
