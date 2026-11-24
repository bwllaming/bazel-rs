# convenience_symlinks

Bazel 9.3.1's convenience symlinks (OutputDirectoryLinksUtils,
ExecutionTool.handleConvenienceSymlinks, PathPrettyPrinter;
crate::convenience_symlinks), pinned by tests/convenience_symlinks.rs.

`genuine.txt` is the capture of `capture.sh` (Bazel 9.3.1 in batch mode, one
fresh `--output_user_root`, the steps in that script in order on a copy of
`workspace/` named `ws`): per Bazel step the exit code, the convenience
symlink warnings, the build results with their paths, the test summary's log
paths, the BEP ConvenienceSymlinksIdentified event (steps with
`--build_event_json_file=bep.json`) and then the workspace's link-like entries
with their targets (`<OB>` the output base, `<WS>` the workspace). It covers
`build`, `test`, `run` and `coverage` creating `bazel-bin`, `bazel-testlogs`,
`bazel-out` and `bazel-ws`; `--symlink_prefix=foo-`, `out/x-` and `/`;
`--experimental_convenience_symlinks=ignore|clean|log_only` and its boolean
forms; a stale link replaced; a real directory and a real file left alone
(the `failed to create one or more convenience symlinks` warning, and the
results printed through the links that exist); `--noincompatible_skip_genfiles_symlink`;
rule-transitioned targets (one configuration, mixed with the top-level one,
two others: the `cleared convenience symlink(s)` warning); failed actions and
analyses (links updated) and failed target patterns, `--nobuild`, `cquery`,
`aquery`, `query`, `fetch` and `info` (links left alone); results printed
from a subdirectory (`../bazel-bin/…`); `clean` with a prefix, a directory
prefix and `--expunge`.

Declared native differences the test leaves out of the comparison (none is a
convenience symlink difference): the native error texts (`ERROR: …`,
`WARNING: Target pattern parsing failed.`), `INFO: Running command line`
(not printed natively), the coverage warnings' order, the test log paths
(`FAIL: …` and the summary print the shared test result's path in the
native cache, docs/TEST_COMMAND.md), and `--print_relative_test_log_paths`
(explicitly unsupported natively).
