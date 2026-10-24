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
  its own process group (bazel-rs's former `process_group(0)`; Rust cannot
  `setsid()` in the child without unsafe code) with an empty stdin and the
  inherited working directory, environment, stdout and stderr, and waits on a
  pidfd (polling where pidfds are unavailable).
* After the command has exited — still unreaped, so its PID pins its process
  group's ID — the supervisor SIGKILLs the group, reaps the command (its exact
  wait status), then repeatedly SIGKILLs every child it still has (reparented
  descendants; a child that leads its own process group takes the group along,
  which only its descendants can be in) and reaps them until none is left.
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
* Ctrl-C (or any death of bazel-rs) now kills the running spawns, like genuine
  interrupting them; before they ran on as orphans.
* A standalone spawn whose environment has no `PATH` and whose `argv[0]` has no
  slash is searched in glibc's default `/bin:/usr/bin` (execvp in the
  supervisor, which is what genuine's process-wrapper and the sandbox worker
  already did), no longer in bazel-rs's own `PATH` (posix_spawnp in the client).
* A standalone spawn costs one extra exec of the bazel-rs binary (~1 ms CPU per
  spawn on this host: 400 trivial genrules, cold, `--jobs=16`, 0.18 s -> 0.22 s
  wall); sandboxed spawns already paid it (the worker forks the action instead
  of exec'ing it).

## Tests

* tests/process_cleanup.rs: tests (sandboxed and standalone, pass/fail/timeout),
  genrules (sandboxed and `local`) and repository `execute` (normal and timed
  out) that leave `setsid`, double-fork and same-group daemons: none survives the
  command; results, exit codes and failure messages (`signal: 9 (SIGKILL)`,
  `signal: 13 (SIGPIPE)`, `exit status: 3`) are the spawn's own; the supervisor's
  protocol (`S`/`E`/`X`, `T`, `K`, end-of-file, execvp candidates) directly.
  With the cleanup disabled every one of these tests fails.
* src/process_wrapper.rs unit tests: report encoding, coalesced reports, option
  parsing, the direct-start group discipline.
