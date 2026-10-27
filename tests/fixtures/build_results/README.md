# build_results

Bazel 9.3's build results and execution summary on stderr
(BuildResultPrinter, BuildSummaryStatsModule; crate::build_results,
crate::spawn_stats), pinned by tests/build_results.rs.

`genuine.txt` is the capture of `capture.sh` (Bazel 9.3.0, one server, a fresh
`--output_user_root`, the commands in that script in order on a copy of
`workspace/`, `--curses=no --color=no`): per command the exit code and the
stderr lines `Target …`/`Aspect …` with their paths, `Use --verbose_failures
…`, `INFO: Analyzed …`, `INFO: Elapsed time: …` (times as `<T>`) and
`INFO: N processes: …`.

Declared native differences the test normalizes: the counts of `Analyzed`
(a warm server configures nothing; its `N aspect applications` count only
the newly configured ones), the process counts (other internal actions,
`linux-sandbox` for genuine's `processwrapper-sandbox`), the order of several
targets' results (genuine's is a hash set's; native: the request's order),
and the paths: bazel-rs creates no convenience symlinks, so it prints the
output tree's paths (`$(bazel-rs info bazel-bin)/…`), what Bazel prints with
`--symlink_prefix=/`. `--verbose_failures`' command lines are not modelled.
