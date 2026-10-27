# `--quiet` (startup option)

`bazel-rs --quiet COMMAND …` (also `startup --quiet` in an rc file; the last
`--[no]quiet` wins, the command line after the rc files) prints only errors
and the command's own output, like Bazel 9.3:

* Bazel's client sends `--quiet` with each request (`blaze.cc:2041`); the
  server's `UiEventHandler` then drops every event except ERROR, FATAL, STDOUT
  and STDERR (`UiEventHandler.java:538-545`) and draws no progress bar
  (`:1140`). `--quiet` never restarts the server. (Under `--batch` the client
  does not pass it to the server: only the client's own messages go.)
* **Dropped**: DEBUG events (`print()` in `.bzl`, BUILD, MODULE.bazel files,
  rule, aspect, transition, module extension and repository rule code), INFO
  events (with the output of a *successful* action or test bound to their
  `INFO: From …` event — so `--test_output=errors|all` logs too), WARNING
  events, the tests' `FAIL:` lines, `INFO: Running command line`, and the
  native informational lines (`bazel-rs: N actions executed …`, the memory
  limit, peak RSS and watchdog warning lines, legacy-cache, lockfile and
  snapshot-removal notes, `bazel-rs: warning: …` cleanup notes).
* **Kept**: errors (with a *failed* action's output), the build result lines
  (Bazel's `Target //x up-to-date:`, `Use --verbose_failures …`; the native
  `LABEL<TAB>PATH` with `BAZEL_RS_RESULT_LINES=1`), aspect result
  lines, the test summary on stdout (with `(Summary)` blocks and cached
  outputs), `execute(quiet = False)` echoes of repository rules, a `run`
  binary's and a streamed test's own output, `query`/`info` output, and the
  explicitly requested diagnostics (`BAZEL_RS_TIMING` PERF lines,
  `BAZEL_RS_SNAPSHOT_EXPLAIN`). Exit codes do not change.

Genuine capture: tests/fixtures/startup_quiet/reference_receipt.json (server
mode, each scenario with and without `--quiet`); native test:
tests/startup_quiet.rs.

## Implementation

`crate::ui` holds the verbosity (`ui::quiet()`, the accessor every console
display honours: the interactive progress display of docs/PROGRESS.md never
runs under it) and the one check: every
informational line goes through `ui::note` (`note!`). Bytes that are stored
before they reach the console — what planning printed, which the analysis
snapshot keeps and a hit replays, a repository speculation's deferred output
— keep their notes *marked* (APC escape sequences around each note). Every
console writer of such bytes (the stderr capture's forwarding, the hit's
replay, verify mode's reports, a speculation's commit) filters them through
`ui::Filter`: the marks go, and under `--quiet` the notes too.

So `--quiet` is console-only: it is no part of the analysis snapshot key (the
key leaves `--[no]quiet` out of the startup options), a snapshot recorded by a
quiet run replays every note to a loud one, and one recorded loud replays
none to a quiet one. Output without `--quiet` is byte-identical to before.

Not modelled: Bazel's `--quiet` also silences its client messages
(`Extracting Bazel installation...`, `Starting local Bazel server…`), which
the native client never prints; the native error formats (e.g. a failed
action's stdout inside its error on stderr, where Bazel writes it to stdout)
are unchanged.
