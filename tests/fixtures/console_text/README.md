# console_text

Bazel 9.3's console lines in non-interactive mode (stderr not a terminal,
`--curses=no --color=no`): `print()` as `DEBUG: FILE:LINE:COL: TEXT` in every
evaluation context, `INFO: Analyzed …`, `INFO: Found …`, `INFO: Elapsed time …`
and `INFO: Build completed …`. Pinned by tests/console_text.rs.

Captured with the genuine 9.3.0 binary on a copy of `workspace/` (one server,
in this order, a fresh `--output_user_root`):

    bazel build //:top //:m --curses=no --color=no          > genuine_build.txt
    bazel build --nobuild //:gen --curses=no --color=no     > genuine_nobuild.txt
    bazel test //:t //:gen --curses=no --color=no           > genuine_test.txt
    bazel test //:t //:f --curses=no --color=no             > genuine_test_failed.txt

Each file holds the exit code (`rc=N`) and the stderr lines starting `DEBUG:`
or `INFO:` (plus a multi-line print's continuation `line`), the workspace path
replaced by `<WS>`. Declared native differences the test normalizes: the counts
in `Analyzed (…)`, the elapsed time, the `INFO: N processes: …` line and the
`N total actions` count (other internal actions; tests/fixtures/build_results
pins their form), and the order of the DEBUG lines (genuine's parallel evaluation order
varies; the native order is the sequential one).
