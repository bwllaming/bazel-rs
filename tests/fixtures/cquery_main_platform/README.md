# cquery with a main-repository target platform

`tests/cquery_main_platform.rs` runs
`cquery --platforms=//:p 'deps(//:gt)'` on `workspace/`. In that workspace,
`//:p` is a platform of the main repository, and `//:gt` is a genrule with an
exec-configured tool.

The evaluator used to stop with the internal error `Ct1(//:gt) re-emits
MemoUse(..) where its abandoned attempt emitted MemoUse(..)`. Here is why.
The pool's attempt of `//:gt` swallowed the no-demand sentinel of the still
unfetched `@bazel_tools`, so it skipped the `$genrule_setup` edge. The
attempt then stopped on its exec platform. Its rerun took the edge, which
did not match the carried events (docs/EVALUATOR.md, `bodies::push_running`).

`genuine-deps.out` is what genuine Bazel 9.3.1 printed for the same command:
`bazel-9.3.1-linux-x86_64` (sha256 `7f775b54…e39ef585c`), with `--batch
--ignore_all_rc_files` and a private `--output_user_root`, built-in modules
from the BCR, 2026-11-17. Its exit code was 0.

The test compares bazel-rs's output with this file. Both sides are sorted,
deduplicated, use the `@bazel_tools`/`@platforms` spelling, and have their
7-hex configuration checksums replaced, as in `tests/cquery_output_formats.rs`
(native ConfigIds are not BuildOptions checksums). One line is a known open
divergence: genuine also lists the execution platform
`@bazel_tools//tools:host_platform` among the deps when the target platform
is another platform. bazel-rs captures only the `$target_platform` edge
(`//:p`).
