# exit_codes

Bazel 9.3's exit codes (util/ExitCode.java via the failures' FailureDetail
metadata) for a matrix of failing and succeeding commands, pinned by
tests/exit_codes.rs (crate::exit_code).

`genuine.txt`: one line per command, `ARGS => EXIT CODE`, captured with the
genuine 9.3.0 binary (one server, a fresh `--output_user_root`,
`--ignore_all_rc_files ARGS --curses=no --color=no`, stdin from /dev/null) in
the commands' order, on a copy of tests/fixtures/build_results/workspace with
`my_test(name = "tf", code = 1)` appended to its BUILD file. ARGS are shell
words (`'deps(//:a'` is one argument). The remote cases name a closed port
(`grpc://127.0.0.1:1`): genuine's capabilities query fails the action, exit
34.

Codes: 0 success (also `build` without targets: a warning, nothing built),
1 build/analysis/loading failure, 2 command-line error (unknown option, bad
option value, undefined `--config`, `run` of no/several/non-executable
targets, query syntax, unknown command), 3 tests failed, 4 no test targets,
7 query evaluation error, 32 a registry module resolution could not read
(tests/fixtures/registry_failure_exit_codes), 34 remote error, 37 `mod`'s
invalid subcommand.
Not in the matrix: 8 (interrupted: SIGINT, tests/process_cleanup.rs and
tests/progress_ui.rs), 33 (out of memory, tests/memory_limit.rs), 36 (a
sandbox setup failure, tests/sandbox.rs).
