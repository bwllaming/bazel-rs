# Sandboxed local execution (N59)

Bazel 9.3 executes Linux actions under the `sandboxed` strategy by default:
`SandboxModule.java:353-356` registers it as the default spawn strategy
whenever a sandbox implementation is supported, and on Linux the
implementation is the `linux-sandbox` tool (`SandboxModule.java:326`,
`src/main/tools/linux-sandbox.cc`). This engine implements the same
semantics: `sandboxed` is the default, `--spawn_strategy` and
`--strategy=<mnemonic>=` select strategies per spawn, and test actions run
sandboxed like any action (test actions stay local-only, the N23g note).

## Strategy plumbing

- `--spawn_strategy=a,b,...`: comma-separated list, highest priority first;
  repeated flags append (`CommaSeparatedNonEmptyOptionListConverter`,
  `ExecutionOptions.java:63-75`). Registered identifiers are
  `standalone`/`local` (`StandaloneModule.java:98`) and
  `sandboxed`/`linux-sandbox` (`SandboxModule.java:326`).
- `--strategy=<mnemonic>=<list>` overrides the default list for that mnemonic
  (`SpawnStrategyRegistry.getStrategies:142-143`); `--genrule_strategy`
  is the deprecated Genrule-only spelling (`ExecutionOptions.java:77-87`).
  Mnemonics: `Genrule` for genrules (`GenRuleAction.java:35`), `TestRunner`
  for test spawns (`TestRunnerAction.java:112`), the Starlark `mnemonic`
  parameter or `Action` for `ctx.actions` spawns
  (`StarlarkActionFactory.java:811`).
- For each action the engine picks the highest-priority strategy that can
  execute it (`SpawnStrategyResolver.resolveOne`). Spawns tagged `no-sandbox`
  or `nosandbox` skip the sandboxed strategy (`Spawns.mayBeSandboxed`,
  `Spawns.java:63-67`); a kernel without Landlock support skips it the same
  way. When no requested strategy can execute the spawn the build fails with
  Bazel's message (`SpawnStrategyResolver.java:77-96`).
- **Failure semantics (no escalation-on-failure):** an action that fails
  under the sandboxed strategy fails the build; Bazel 9.3 does not retry it
  with the next strategy. The strategy list is a *preference order over
  usable* strategies, not a fallback chain ("For each action Bazel picks the
  strategy with the highest priority that can execute the action",
  `ExecutionOptions.java:69-74`; `SpawnStrategyResolver.resolveOne:49-57`
  executes with `strategies.get(0)` only). The older escalate-on-failure
  behavior (`SandboxedSpawnRunner` of Bazel ≤0.4, removed with strategy
  chains in 2019, cf. https://blog.bazel.build/2019/06/19/list-strategy.html)
  is intentionally not reproduced; genuine Bazel 9.3.0 paired captures
  confirm it (`--spawn_strategy=sandboxed,standalone` fails once).
- **Failed inputs/outputs:** like `AbstractSandboxSpawnRunner.runSpawn`
  ("We copy the outputs even when the command failed", :162-164), a failed
  sandboxed action's outputs remain in its sandbox execroot (this engine's
  per-action work directory), retained exactly when `--sandbox_debug` is on
  (`runSpawn`'s finally deletes the sandbox otherwise, :172-178). Failed
  actions never enter the action cache, in either engine.
- The strategy wraps execution only. The action key (and the runfiles tree
  layout an action sees) is strategy-independent: an entry produced sandboxed
  satisfies a standalone request and vice versa
  (`tests/sandbox.rs::action_keys_do_not_change_with_the_strategy`).
- The build summary's note names the mode: "(experimental local execution;
  sandboxed)" when every executed action ran sandboxed, "(experimental local
  execution; not sandboxed)" otherwise.

## The sandbox

Genuine `linux-sandbox` clones the action into fresh user, mount, IPC, PID
and (by default) network namespaces and remounts the whole filesystem
read-only except the writable paths (`linux-sandbox.cc:185-196`,
`linux-sandbox-pid1.cc: SetupMountNamespace`, `MakeFilesystemMostlyReadOnly`,
`MountProcAndSys`, `SetupNetworking`). This engine's build client runs in a
Docker container whose default seccomp policy denies `unshare`/`clone` with
`CLONE_NEWNS`/`CLONE_NEWPID` and every `mount(2)` to unprivileged processes,
so those namespaces cannot be created here (probe evidence in
`tests/fixtures/sandbox/reference_receipt.json`; the same filter is why
genuine Bazel 9.3.0 on this host cannot register `linux-sandbox` either and
falls back to processwrapper-sandbox, which isolates nothing). The closest
Bazel-faithful design that still makes out-of-sandbox writes FAIL is a
Landlock ruleset (Linux LSM, ABI 4 on this kernel) applied by a sandbox
worker process immediately before exec'ing the action — exactly the point
where linux-sandbox applies its namespace setup:

- The executor starts its own binary as the hidden `__sandbox-worker`
  subcommand (the role the linux-sandbox binary fills as the spawn's
  argv[0]), with the action's cwd and environment. The worker applies the
  ruleset and execs the action through `spawn_lock::exec`; the action's exit
  status is the worker's.
- Every write-ish filesystem access (`AccessFs::from_write`: write, remove,
  mkdir/mknod/symlink, cross-directory refer, truncate) is denied everywhere
  except the paths linux-sandbox keeps writable: the action's execroot,
  TMPDIR, TEST_TMPDIR, `/tmp` and `/dev/shm`
  (`LinuxSandboxedSpawnRunner.getWritableDirs:391-398`,
  `AbstractSandboxSpawnRunner.getWritableDirs:352-405`), plus
  `--sandbox_writable_path` entries and their symlink targets, and
  `--sandbox_tmpfs_path` targets (granted writable where they exist; the
  tmpfs emptiness itself is not modelled). `/dev` is granted writable: the
  linux-sandbox mounts a private writable `/dev` with the standard device
  nodes (`linux-sandbox-pid1.cc MountDev:645-672`); the host's node set is
  used instead of a minimal tmpfs.
- Reads stay allowed everywhere (the read accesses are simply not handled,
  like read-only bind mounts of the whole tree), except under
  `--sandbox_block_path` subtrees: Landlock rules only grant access, so
  blocking a path means granting read on everything else — every top-level
  subtree and, along the blocked path, each sibling (the same total walk
  `MakeFilesystemMostlyReadOnly` does over `/proc/self/mounts`). Execution
  stays unhandled: binaries outside blocked subtrees keep running.
- TCP bind and connect are denied by default (`AccessNet::BindTcp |
  ConnectTcp`, Landlock ABI 4) — the counterpart of the network namespace
  linux-sandbox creates unless the spawn requires network
  (`LinuxSandboxedSpawnRunner.prepareSpawn:302-313`,
  `ExecutionRequirements.REQUIRES_NETWORK = "requires-network"`), which the
  `requires-network` tag and `--sandbox_writable_path`-style tags feed from
  rule tags. With the tag the ruleset handles no network accesses (TCP
  reaches the stack; UDP and raw sockets are outside Landlock ABI 4 and
  remain reachable — a documented approximation of a network namespace).
- A sandbox setup failure (missing writable path, Landlock unavailable) is
  an infrastructure failure of the strategy, not the action's: the worker
  reports it through a setup-error file and the action error reads
  "could not set up the sandbox: ..." (the role of
  `sandbox.createFileSystem` -> "Could not copy inputs into sandbox",
  `AbstractSandboxSpawnRunner.runSpawn:142-149`).
- Denials surface as `EACCES` ("Permission denied"), not the `EROFS` a
  read-only remount produces. A fixture writing outside its sandbox shows
  the tool's own "Permission denied" in the action's stderr; genuine
  linux-sandbox (where it can run) would show "Read-only file system". The
  paired receipt records the difference and why.

Deviations from genuine namespaces (all forced by this host's seccomp
filter, none by choice): no user/mount/PID/IPC/UTS namespaces; `/proc` shows
host processes (no pid-namespace isolation); no fresh `/proc`/`/sys` mounts;
no fake hostname (`sandbox_fake_hostname` unimplemented); the tmpfs mounts
of `--sandbox_tmpfs_path` are writable grants, not empty filesystems.

## --sandbox_debug

`--sandbox_debug` (`SandboxOptions.java:87-97`) keeps the sandbox build root
untouched after the build: the action's work directory (sandbox execroot,
tmp, `sandbox-debug.log`) is retained instead of deleted — after successes
and failures — and the failure message says where
("(sandbox build root retained at '...')"). Without the flag a sandboxed
action failure appends Bazel's verbatim suggestion
(`AbstractSandboxSpawnRunner.SANDBOX_DEBUG_SUGGESTION`, :70-72):

```
Use --sandbox_debug to see verbose messages from the sandbox and retain the sandbox build root for debugging
```

## Flags

| Flag | Effect |
| --- | --- |
| `--spawn_strategy=<list>` | strategy preference order (default `sandboxed,standalone`) |
| `--strategy=<mnemonic>=<list>` | per-mnemonic override |
| `--genrule_strategy=<list>` | Genrule-only override (deprecated spelling) |
| `--sandbox_debug` | retain the sandbox build root, print setup log, no suggestion |
| `--sandbox_writable_path=<path>` | keep an existing path writable (with its symlink target) |
| `--sandbox_block_path=<path>` | make the path's subtree unreadable |
| `--sandbox_tmpfs_path=<path>` | keep the path writable (no empty tmpfs is mounted) |

Paired receipt: `tests/fixtures/sandbox/reference_receipt.json` (genuine
Bazel 9.3.0 vs this engine on the same fixtures: default strategy choice,
out-of-sandbox write failure texts, escalation absence, output bytes).
