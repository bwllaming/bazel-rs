# The interactive progress display

On a terminal, `bazel-rs build`, `test` and `run` keep a status area at the
bottom of the screen, like Bazel's curses mode (runtime/UiEventHandler.java,
UiStateTracker.java). Everything else prints exactly as before, above it.
Implementation: `src/progress.rs` (hooks, counters, options),
`src/progress/render.rs` (the text), `src/progress/terminal.rs` (the thread
that owns the terminal).

## What it shows

```
INFO: Analyzed 1 target (1 package loaded, 2 targets configured).
[2 / 4] Genrule slow.txt; 1s linux-sandbox
```

| phase | status line (Bazel's wording) |
| --- | --- |
| module resolution, extensions, the snapshot check | `Computing main repo mapping:` |
| target pattern expansion | `Loading: N packages loaded` |
| analysis | `Analyzing: N targets (P packages loaded, T targets configured)`; once phase 1 ended `T / D targets configured` (D: the targets phase 2 configures) |
| analysis snapshot hit | `Analysis snapshot hit: preparing the saved plan` |
| execution | `[done / total]` then `N / M tests, F failed;` (test), then the running action (`<mnemonic> <primary output>; Ns <strategy>`), or `K actions, R running` with up to `--ui_actions_shown` (default 3) running actions below, `checking cached actions` when none has started a process, `no actions running` |

Below any phase: `    Fetching repository @@name; Ns`, `    Fetching module
extension @@m+//:ext.bzl%name; Ns`, `    Fetching <url>; 3.3 MiB 2s` for
in-flight repository rules, extension evaluations and downloads (at most
`--ui_actions_shown`, then `... (N fetches)`). Each line is cut to the
terminal's width (TIOCGWINSZ, re-read at every refresh; `$COLUMNS`/80 when
there is none). The leads are green (red with failed tests) unless
`--color=no` or `NO_COLOR` is set.

Interactive-only lines (never printed without the display):
`INFO: Analyzed N targets (P packages loaded, T targets configured).` at the
end of analysis, `INFO: Analysis snapshot hit: loading and analysis skipped.`
on a hit, and at the end `INFO: Elapsed time: X.XXXs` plus
`INFO: Build completed successfully, N total actions` (exit 0) or
`INFO: Build completed, K tests FAILED, N total actions` (exit 3). There is no
critical path.

## When it runs

`build`, `test` and `run` only (not `query`, `mod`, `info`), and only when
* `--curses=auto` (the default) and stdout and stderr are both a terminal with
  a real `TERM` (Bazel's client `--isatty`, IsStandardTerminal: not `dumb`,
  not emacs) and `CI` is unset (or `false`/`0`), or `--curses=yes`;
* not `--curses=no`, `--noshow_progress`, the startup option `--quiet`
  (UiVerbosity.QUIET: UiEventHandler.addProgressBar shows no progress bar), or
  `BAZEL_RS_PROGRESS=0`/`off` — these win over `--curses=yes`.

Off, nothing of it runs: every engine hook is one relaxed atomic load, and
the output is byte for byte what it was before the display existed (the
CLI tests and the A/B comparisons rely on that; tests/progress_ui.rs checks
it on a pseudo-terminal with `--curses=no`, `--noshow_progress` and `CI`).

UI options (UiOptions.java; before, they were rejected as unsupported):
`--curses=yes|no|auto`, `--color=yes|no|auto`, `--[no]show_progress`,
`--show_progress_rate_limit=<seconds>` (default 0.2, never below 0.1),
`--[no]progress_in_terminal_title` (the status line in the title),
`--ui_actions_shown=N` (`--experimental_ui_actions_shown`). They are left
out of the analysis snapshot key (`snapshot::key::EXCLUDED`).

## How

* **One owner of the terminal.** `progress::start` saves the terminal
  (a duplicate of fd 2), then redirects fd 2 — and fd 1 when it is the same
  terminal, so stdout keeps its place — into a pipe. The `progress` thread
  reads the pipe and forwards whole lines: erase the status area
  (`\r`, cursor up, `ESC[J`), write the lines, draw the area again, in one
  `write`. Every printer is covered by construction: `eprintln!`, Starlark
  `print`, child processes that inherited the descriptor, the diagnostics
  stream. A partial line waits for its newline at most 0.5 s (or 64 KiB),
  then goes out as it is and the area stays hidden until a line ends.
* **Refresh.** At most every `--show_progress_rate_limit` the thread samples
  the counters and redraws the area in place (line by line with `ESC[K`, no
  blank frame); an identical frame is not written. Forwarded output redraws
  it with a fresh sample.
* **No cost on hot paths.** Counters are relaxed atomics (packages committed,
  targets configured, actions done, tests). A running action holds one of 64
  slots, claimed with a compare-and-swap, its description (mnemonic and
  primary output, at most 192 bytes) written under a sequence lock that the
  sampler reads without blocking; the strategy is one atomic store when the
  spawn starts. No lock and no allocation per action. Fetches and downloads
  (rare) register in a mutexed list; a download adds its bytes to an atomic.
* **The analysis snapshot.** Its capture of planning's stderr
  (crate::snapshot::capture) nests inside: the capture's "original stderr" is
  the display's pipe, the status area is written to the saved terminal and
  never to fd 2, and the `INFO:` lines go through the diagnostics stream
  (crate::snapshot::diag) — none of it can enter a snapshot window, and a hit
  replays exactly what it replayed before.
* **The end.** Every exit path of the CLI (`exit`, the end of `main`, a panic
  unwinding out of `main`, the memory watchdog's exit, a strict verify
  mismatch, and `run` before it executes its target) calls
  `progress::finish`/`abandon`: restore fd 1 and 2, let the thread forward
  what is still in the pipe (bounded), erase the area, print the summary. A
  process that still holds the pipe (a lingering child) is forwarded
  directly afterwards. SIGINT, SIGTERM and SIGHUP erase the area from the
  signal handler (one `write`), then end the process by the same signal, as
  before. The cursor is never hidden, so nothing needs restoring.

## Overhead

Measured on ~/code with the release binary, on a pseudo-terminal (display
on) and with `--curses=no` on the same pseudo-terminal (off); see the
progress-ui worker report for the numbers. The display costs one thread
waking at most 5 times a second and a few hundred bytes per refresh.
