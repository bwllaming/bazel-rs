# progress_lines

Bazel 9.3.1's progress lines without cursor control: stderr is a file, so
UiEventHandler prints its short progress bar one line at a time
(`Computing main repo mapping: `, `Loading: `, `Analyzing: …`, `[N / M] …`),
at most once per `max(--show_progress_rate_limit, 1s)`, only when something
happened, plus the inactivity watchdog's reminder. Pinned by
tests/progress_lines.rs (crate::progress plain lines, docs/PROGRESS.md
"Without a terminal").

`capture.sh SCRATCH` reran the genuine binary (`--batch`, a fresh
`--output_user_root`, `clean` before each command) on a copy of
`workspace/` and wrote `genuine/NN.txt`: the command, `rc=N`, then every
stderr line stamped with the seconds since the command started.

| file | command | shows |
| --- | --- | --- |
| 01 | `build //:all` | the phase lines, repeated while `@slow` fetches (2 s), three slow genrules in parallel then `//:chain` |
| 02 | `test //:t1 //:t2 //:t3` | test lines: `N / M tests, F failed;`, `Testing //:t2` |
| 03 | `build //:long` | one 13 s action: `…; 1s …`, then the watchdog's `…; 11s …` |
| 04 | `build //:all --noshow_progress` | no progress line |
| 05 | `build //:all --show_progress_rate_limit=3` | the four-second execution shows no line |

Declared native differences the test normalizes: the counts (packages,
targets, actions), the seconds, the strategy (the host's sandbox), which of
the simultaneous genrules or tests is named, and how often a line repeats
(timing). `[Sched]` (an action waiting for local resources) has no native
equivalent.
