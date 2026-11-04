# Spawn supervision: the process-wrapper (src/process_wrapper.rs)

Every local spawn bazel-rs executes — a build action (sandboxed or standalone),
a test and its test.xml generator, a repository rule's or module extension's
`execute` — runs under a supervisor process that outlives the spawn's main
process, like Bazel 9.3's process-wrapper and linux-sandbox. Once the main
process has exited (normally, by a signal, or after a timeout), the supervisor
kills and reaps every process the spawn left behind, including processes that
left its process group: `setsid` daemons (embedded PostgreSQL's `pg_ctl start`),
a daemon's double fork. Before this, bazel-rs only killed the spawn's process
group, so such daemons survived the command (the bworker tests leaked 39
PostgreSQL servers in one survey, /tmp/opencode/perf-research/TEST-SURVEY.md B1).

## Genuine Bazel 9.3

* Local spawns run under the process-wrapper: `LocalSpawnRunner`
  (exec/local/LocalSpawnRunner.java:375-386, `StandaloneModule.java:89` passes
  `ProcessWrapper.fromCommandEnvironment`), `ProcessWrapperSandboxedSpawnRunner`
  (the processwrapper-sandbox strategy genuine resolves to on this host, where
  namespaces are unavailable), and `repository_ctx`/`module_ctx.execute`
  (StarlarkBaseExternalContext.java:2079-2085, BazelRepositoryModule.java:294-297).
* process-wrapper (src/main/tools/process-wrapper-legacy.cc): `SpawnChild` makes
  itself a child subreaper (`prctl(PR_SET_CHILD_SUBREAPER)`, :53-62) before it
  forks; the child `setsid()`s and execs the command. `WaitForChild` waits for the
  command (`wait()` until its PID, discarding reparented zombies), then
  `TerminateAndWaitForAll` (process-tools-linux.cc:24-55): `kill(-pid, SIGKILL)`
  for the command's process group, then SIGKILL to every child listed in
  `/proc/thread-self/children` until `waitpid(-1)` reports ECHILD. Escaped
  descendants get SIGKILL right away; there is no grace period for them. The
  wrapper then exits with the command's status (re-raising its signal).
  Timeouts: SIGALRM sends SIGTERM to the command's group and SIGKILL after
  `--kill_delay` (`--local_termination_grace_seconds`, default 15;
  process-tools.cc:93-111); SIGTERM/SIGINT to the wrapper SIGKILL the group.
* Signal dispositions: the wrapper's child (process-wrapper-legacy.cc:72) and
  linux-sandbox's pid 1 and its child (linux-sandbox-pid1.cc:567, 774) call
  `ClearSignalMask` (process-tools.cc:151-173): an empty mask and SIG_DFL for
  every signal, so a spawn never inherits a signal bazel was started with
  ignored (SIGINT/SIGQUIT in a background job, SIGHUP under nohup). Captured in
  tests/fixtures/signal_dispositions (src/signals.rs has the other starts:
  `run`, the workspace status command).
* linux-sandbox ends a PID namespace with its pid 1 (linux-sandbox-pid1.cc:
  811-814): the kernel SIGKILLs every remaining process of the spawn.
* Captured (scratch fixture /tmp/opencode/fixdesc-scratch/ws, `--batch`,
  processwrapper-sandbox): a test that starts `setsid sleep`, `setsid sh -c
  'sleep &'` and a plain `sleep &` and then passes, fails or times out
  (`--test_timeout=3`), a genrule doing the same (default and `tags=["local"]`),
  and a repository rule whose `execute` does the same: no `sleep` survives any of
  the commands; results PASSED / FAILED / TIMEOUT, build rc 0, execute result
  `rc=0` with its stdout.

## bazel-rs

* The executor starts its own binary (/proc/self/exe) as the supervisor:
  `__process-wrapper` for standalone spawns and repository execute,
  `__sandbox-worker` (crate::sandbox) for sandboxed spawns — the worker applies
  the Landlock ruleset to itself and then supervises exactly the same way. The
  supervisor is single-threaded, becomes a child subreaper, starts the command in
  its own session and process group (`setsid()` in the child like genuine's
  process-wrapper, crate::signals::new_session_in_child; until 0.4.4 only
  `process_group(0)`, which left spawns in the session of bazel-rs and of
  the shell that started it) with an empty stdin and the
  inherited working directory, environment, stdout and stderr, and waits on a
  pidfd (polling where pidfds are unavailable). While the command runs, a
  second thread reaps every reparented descendant as soon as it exits (the
  process-wrapper's `wait()` loop discards such zombies, process-tools.cc:
  194-199; linux-sandbox's pid 1 likewise), so a command that waits for a
  daemon's end with `kill(pid, 0)` sees it gone; the command itself stays
  unreaped until the group kill.
* After the command has exited — still unreaped, so its PID pins its process
  group's ID — the supervisor SIGKILLs the group, reaps the command (its exact
  wait status), then repeatedly SIGKILLs every child it still has (reparented
  descendants; a child that leads its own process group takes the group along,
  which only its descendants can be in) and reaps them until none is left.
* The command starts with every signal this process inherited as ignored back
  at its default action (crate::signals, `Defaults::ProcessWrapper`: a
  `pre_exec` hook added only when such a signal exists, between fork and
  exec); the supervisor and bazel-rs themselves keep ignoring them.
* The control channel is one end of a Unix socket pair, the supervisor's stdin.
  Supervisor to executor: `S<pid>` once the command runs (the executor registers
  the command's group with the memory watchdog), `E<errno>` when it cannot start
  (the executor returns the same `io::Error` a direct start returns, so messages
  and the ETXTBSY retry are unchanged), and `X<raw wait status>` once every
  process is gone (the executor rebuilds the identical `ExitStatus`: exit code,
  signal and core flag; the supervisor cannot faithfully re-raise SIGPIPE or
  SIGSEGV itself without unsafe code). Executor to supervisor: `T` = SIGTERM to
  the command's group (test timeout), `K` = SIGKILL to the command and its group
  (end of the 15 s grace period, a repository `execute` deadline). End-of-file —
  bazel-rs exited (Ctrl-C, the watchdog's exit 33, a crash) or dropped the spawn
  — counts as `K`, like linux-sandbox dying with its parent; what the command
  left is then killed as after a normal exit.
* Repository `execute`: the executor computes the execvp candidates (unchanged)
  and the supervisor runs the glibc `__execvpe` search over them
  (`repository_process::search`), so the errno rules and the
  `process-wrapper-legacy.cc:80` failure text are unchanged; the deadline sends
  `K`; the captured pipes close once every descendant is gone.
* The memory watchdog SIGKILLs the registered command groups (never a
  supervisor, which must survive to clean up); supervised groups leave the
  registry when their spawn ends.
* Only the CLI binary can be re-executed: `main` calls `process_wrapper::enable()`.
  The library's in-process users (unit tests, benches) start standalone spawns
  directly with the former discipline: the group is SIGKILLed once the leader
  has exited (leader unreaped, so the group ID is pinned); a process that left
  the group is not reached.

## Intended differences from the previous bazel-rs behaviour

* Processes that leave a spawn's process group no longer outlive the spawn
  (tests, actions, repository/module `execute`), as in genuine.
* Spawns (tests, actions, repository `execute`) run in a session of their own
  (setsid), as under genuine's process-wrapper; before 0.4.5 they shared
  bazel-rs's session (in their own process group already, so `kill 0` in a
  spawn never reached bazel-rs either way).
* Ctrl-C (or any death of bazel-rs) now kills the running spawns, like genuine
  interrupting them; before they ran on as orphans.
* A standalone spawn whose environment has no `PATH` and whose `argv[0]` has no
  slash is searched in glibc's default `/bin:/usr/bin` (execvp in the
  supervisor, which is what genuine's process-wrapper and the sandbox worker
  already did), no longer in bazel-rs's own `PATH` (posix_spawnp in the client).
* A standalone spawn costs one extra exec of the bazel-rs binary (~3 ms of
  latency, ~1 ms CPU per spawn on this host: 400 trivial genrules, cold,
  `--jobs=16`, 0.18 s -> 0.22 s wall); sandboxed spawns already paid it (the
  worker forks the action instead of exec'ing it: 0.23 s -> 0.21 s). A
  repository `execute` costs ~4-6 ms more (300 × `/bin/true`: 5.8 -> 9.5 ms
  each); a cold `//src/beyondwork.ai/tools/oauth2token:all` fetch runs 94
  executes, the B2 trees 465, so well under 1 % of those cold runs (their wall
  times vary far more with the network); warm runs execute none.

## Tests

* tests/process_cleanup.rs: tests (sandboxed and standalone, pass/fail/timeout),
  genrules (sandboxed and `local`) and repository `execute` (normal and timed
  out) that leave `setsid`, double-fork and same-group daemons: none survives the
  command; results, exit codes and failure messages (`(Killed)`, `(Broken pipe)`, `(Exit 3)`,
  TerminationStatus.toShortString) are the spawn's own; Ctrl-C (SIGINT
  to bazel-rs's process group) still ends bazel-rs by the signal and its running
  test and daemons are killed; the supervisor's protocol (`S`/`E`/`X`, `T`, `K`,
  end-of-file, execvp candidates) and the prompt reaping of an exited daemon
  (`kill -0` loop) directly. A test that runs `kill -TERM 0` fails alone
  (bazel-rs exits 3) and ran in a session and process group other than
  bazel-rs's (sandboxed and standalone; the session check fails without
  setsid). With the cleanup, the end-of-file abort or the
  reaper disabled, the corresponding tests fail.
* ~/code (`test //src/beyondwork.ai/bworker/database/...`, survey recipe,
  `--memory-limit=20000`): before (s2-ca7e18b) the passing database_test left
  2 embedded PostgreSQL postmasters (+10 workers, PPID 1); after, the test still
  passes (2 servers started per its log) and no new process remains.
* src/process_wrapper.rs unit tests: report encoding, coalesced reports, option
  parsing, the direct-start group discipline.
