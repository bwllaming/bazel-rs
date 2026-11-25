# The first failure interrupts the actions in flight (`--nokeep_going`)

Without `--keep_going` (the default) the build's first failed action stops
the build at once, like genuine Bazel 9.3.1: no new action starts **and the
actions still running are interrupted** — their spawns are stopped, nothing
of them is reported, cached or published, and the command exits right after
the failure with the failing action's console and exit code. `--keep_going`
is unchanged: every action not depending on a failure runs to its end.

Before this, bazel-rs stopped starting actions but waited for the running
ones: a build whose failure came early still ran a long compilation (the
0.5.1 canary on ~/code: failure reported at ~87 s, exit at ~213–249 s).

## Genuine (tests/fixtures/failfast_interrupt)

`capture.py --bazel <bazel-9.3.1>` on the fixture workspace (three
15-second sleepers that write a partial output first, a 15-second test or a
15-second persistent-worker request, beside an action that fails after a
second); `reference_receipt.json`:

- every case ends within a second of the `ERROR: … failed` line, exit code 1;
- the console is the failing action's, then the usual summary; nothing is
  printed for the interrupted actions (no "interrupted" line):
  ```
  ERROR: <WS>/pkg/BUILD.bazel:12:8: Executing genrule //pkg:fail failed: (Exit 1): …
  Use --sandbox_debug to see verbose messages from the sandbox and retain the sandbox build root for debugging
  failing-now
  Use --verbose_failures to see the command lines of failed build steps.
  INFO: 5 processes: 5 internal.
  ERROR: Build did NOT complete successfully
  ```
- the interrupted actions count as **internal** processes: SkyframeActionExecutor
  posts their ActionCompletionEvent in a `finally`, without spawn results
  (SpawnStats: all actions minus those with spawn results). 5 = the workspace
  status action, the failed action and the three sleepers;
- no process of an interrupted action survives; the disk cache stays empty;
  a rerun executes the sleeper again (`2 processes: 1 internal, 1
  processwrapper-sandbox`);
- an interrupted test reports `NO STATUS` (`Executed 0 out of 1 test: 1 was
  skipped.`); an interrupted persistent worker is destroyed.

How genuine stops a spawn: the interrupted LocalSpawnRunner calls
`subprocess.destroyAndWait()` (LocalSpawnRunner.java:415-417), SIGTERM to the
process-wrapper, which SIGKILLs the command's process group at once
(OnAbruptSignal; `--experimental_process_wrapper_graceful_sigterm` is off),
or to linux-sandbox, which forwards SIGTERM and SIGKILLs the sandbox after
`--local_termination_grace_seconds` (15; OnTimeoutOrTerm,
linux-sandbox.cc:139-161). A non-sandboxed worker's response is not awaited
(WorkerSpawnRunner); a remote Execute call is cancelled.

## Native (crate::interrupt)

The scheduler's failure flag is an `Interrupt` (src/interrupt.rs): the flag
and the spawns running under it. Without `--keep_going` every action gets it
(`Action::execution_cancel`):

- a local spawn registers a stopper while it runs (process_wrapper::Stopper:
  the supervisor's control channel, `T`/`K`; a direct start's process
  group). `Interrupt::fire` — the first failed action, or after a leaf's
  `--notest_keep_going` abort or panic — stops every registered spawn: the
  namespace sandbox (linux-sandbox's role) with SIGTERM and SIGKILL after 15
  s, every other strategy (standalone, processwrapper-sandbox, the darwin
  sandbox, which runs under the process-wrapper) with SIGKILL at once. A
  spawn registered after the interrupt is stopped at once; no spawn starts
  once it fired. The supervisor kills and reaps the rest of the spawn as
  after any exit (setsid escapees included on Linux, its process group on
  macOS);
- a persistent worker's request registers the worker's process group (killed
  at once; the worker is destroyed, the read of its response ends);
- a remote Execute/WaitExecution call is cancelled (the stream is reset; no
  retry, no local fallback);
- the spawn's thread sees the spawn end and returns the interrupted marker
  (`interrupt::interrupted`): the action is neither counted as a spawn, nor
  reported, nor cached, nor published; its work directory goes like any
  failed action's. The scheduler completes it like a skipped job, counts it
  (`KeepGoingOutcome::interrupted`), and the summary counts it internal;
- test leaves keep their polling (`run_leaf`'s flag): SIGTERM, SIGKILL after
  the grace period, `NO STATUS`.

An action whose spawn had already ended when the interrupt came completes
normally (its result is kept, like genuine's completed actions).

## Divergences

- Output tree: bazel-rs runs every spawn in a private execution root and
  publishes only what completed, so an interrupted standalone spawn leaves no
  partial output in `bazel-bin` (genuine's ran in the execroot and leave
  theirs), and the interrupted worker action's parameter file is not there.
- `bazel-rs test` after a failed build action prints neither the build
  summary nor publishes the completed outputs (a gap unrelated to the
  interrupt: also without a running test).
- A remote test spawn is not interrupted (its call runs to its end).
