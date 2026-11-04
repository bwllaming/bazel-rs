# console_action_output

Bazel 9.3.1's console for executed actions (crate::action_console,
docs/PROGRESS.md "Action output and failures"), pinned by
tests/console_action_output.rs.

`genuine/NN.txt` are the captures of `capture.sh`: Bazel 9.3.1 with
`--batch` (a cold server per command), a fresh `--output_user_root`, `clean`
before each command, `--curses=no --color=no`, on a copy of `workspace/`.
Each file holds the command, its exit code, its stderr and its stdout, the
workspace path as `<WS>` and the output base as `<OB>`.

What the commands cover: `INFO: From <progress message>:` with a genrule's,
a `message` genrule's, a Starlark action's (`progress_message`, the default
`MNEMONIC output`) and an exec-configured tool's (` [for tool]`) stderr on
stderr and stdout on stdout, a last line without newline, `--output_filter`,
`--experimental_ui_max_stdouterr_bytes`; failures with `(Exit N)`, a signal
(`(Killed)`), a Starlark `run` (`sh failed: error executing FailRun command
(from failrule rule target //fail:sfail)`), missing outputs (`declared
output '…' was not created by genrule…` and `not all outputs were created
or valid`), `--verbose_failures`' complete command, `--keep_going`, the
failed build's tail (`Target //x failed to build`, `Use --verbose_failures
…`, the summary, `ERROR: Build did NOT complete successfully`),
`--subcommands` (also `pretty_print`) and the `INFO: N processes: …` tally.

Declared native differences the test normalizes:

* the progress and analysis lines (`Loading:`, `Analyzing:`, `INFO:
  Analyzed …`) and `INFO: Found N targets...`, which genuine's merged
  analysis and execution prints after the actions;
* times, the execution roots of `(cd … &&` (genuine's sandbox or output
  base execroot, the native action's private execroot), the paths of the
  results (bazel-rs creates no convenience symlinks) and of the saved
  oversized streams;
* `# Configuration:` / `configuration:` (native ConfigIds, docs/CQUERY.md).
