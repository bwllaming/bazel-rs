# Sandboxed local execution (N59)

Bazel 9.3 executes Linux actions under the `sandboxed` strategy by default:
the platform default strategy list is [worker,] sandboxed, local
(`BazelStrategyModule.java:67-80`; `SandboxModule.java:353-356` registers
`sandboxed` as the default spawn strategy whenever a sandbox implementation
is supported), and on Linux the implementation is the `linux-sandbox` tool
(`SandboxModule.java:326`, `src/main/tools/linux-sandbox.cc`). This engine
implements the same semantics: `sandboxed` is the default, `--spawn_strategy`
and `--strategy=<mnemonic>=` select strategies per spawn, and test actions
run sandboxed like any action (test actions stay local-only, the N23g note).

## Strategy plumbing

- `--spawn_strategy=a,b,...`: comma-separated list, highest priority first;
  a repeated flag replaces the list (CommaSeparatedNonEmptyOptionListConverter;
  the option is not allowMultiple, `ExecutionOptions.java:63-75`).
  Registered identifiers are `standalone`/`local` (`StandaloneModule.java:98`),
  `sandboxed`/`linux-sandbox` (`SandboxModule.java:326`),
  `processwrapper-sandbox` (SandboxModule.java:285-299: a sandbox strategy
  that isolates nothing — the strategy genuine Bazel resolves to on hosts
  without usable namespaces, like this one; an explicit escape hatch here)
  and `worker` (BazelStrategyModule.java:75: registered but never usable —
  persistent workers are not modelled — so resolution always skips it).
  Unknown identifiers are rejected once at startup with Bazel's
  STRATEGY_NOT_FOUND text and exit code 2 (SpawnStrategyRegistry.toStrategy:
  717-727): the request name is "explicit default strategies" for
  `--spawn_strategy` (built at :546-551) and "mnemonic M" for
  `--strategy=M=` / `--genrule_strategy` (:494); the trailing
  "Valid values are: [...]" is approximate — genuine lists the strategies
  its modules registered on the host, in an order the registry does not
  define, while this engine lists its fixed registered set.
- `--strategy=<mnemonic>=<list>` overrides the default list for that mnemonic
  (`SpawnStrategyRegistry.getStrategies:142-143`); a repeated flag for the
  same mnemonic replaces its entry (the multimap put of
  BazelStrategyModule.java:84-89 into SpawnStrategyRegistry.java:359-361);
  `--genrule_strategy` is the deprecated Genrule-only spelling and also
  replaces on repeat (`ExecutionOptions.java:77-87`).
  Mnemonics: `Genrule` for genrules (`GenRuleAction.java:35`), `TestRunner`
  for test spawns (`TestRunnerAction.java:112`), the Starlark `mnemonic`
  parameter or `Action` for `ctx.actions` spawns
  (`StarlarkActionFactory.java:811`).
- For each action the engine picks the highest-priority strategy that can
  execute it (`SpawnStrategyResolver.resolveOne`). Spawns tagged `no-sandbox`,
  `nosandbox` or `local` skip the sandboxed strategies
  (`Spawns.mayBeSandboxed`, `Spawns.java:63-67` — the `local` tag arrives
  from a genrule's `local = 1` and from rule tags through
  `TargetUtils.getExecutionInfo`); the tag set is merged with the spawn's
  `execution_requirements` map exactly like `remote_policy`
  (crate::action::remote_policy). A kernel without Landlock support skips
  `sandboxed` the same way. When no requested strategy can execute the spawn
  the build fails with Bazel's message (`SpawnStrategyResolver.java:77-96`).
- The platform default list is Bazel's own — [worker,] sandboxed, local
  (`BazelStrategyModule.java:67-80`): `worker` never executes here, so the
  default that runs is `sandboxed`, then `standalone`.
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
worker process immediately before it starts the action — exactly the point
where linux-sandbox applies its namespace setup:

- The executor starts its own binary as the hidden `__sandbox-worker`
  subcommand (the role the linux-sandbox binary fills as the spawn's
  argv[0]), with the action's cwd and environment. The worker applies the
  ruleset to itself and then starts and supervises the action, which
  inherits it, exactly like the process-wrapper (docs/PROCESS_WRAPPER.md): a
  child subreaper that, once the action has exited, kills and reaps every
  process it left — the effect of linux-sandbox's PID namespace ending with
  its pid 1. The action's exact wait status reaches the executor through the
  worker's control channel.
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
- **Network is allowed by default**, exactly like Bazel
  (`--sandbox_default_allow_network`, default true — SandboxOptions.java:
  283-292): no network access is handled unless the spawn must be denied one
  (LinuxSandboxedSpawnRunner.java:302-313 creates the network namespace only
  when the spawn does not require network; Spawns.requiresNetwork:78-88).
  The TCP denial (`AccessNet::BindTcp | ConnectTcp`, Landlock ABI 4) applies
  when the spawn tags `block-network`, or when
  `--nosandbox_default_allow_network` is set and the spawn does not tag
  `requires-network` (which overrides the flag to allow). Approximation of a
  network namespace: only TCP bind/connect are covered — UDP, raw sockets
  and abstract UNIX sockets stay reachable — and, unlike a namespace with
  the loopback device up, the denial also blocks 127.0.0.1 (loopback
  included, like NETNS without NETNS_WITH_LOOPBACK).
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

- Every handled access is a Landlock **HardRequirement** and the worker
  probes the enforcement status after `restrict_self`: a ruleset that is not
  fully enforced (older kernel, unexpected LSM state) is a sandbox setup
  failure — never a silently weaker sandbox. Setup failures are
  environmental errors of the strategy and the CLI exits **36** for them
  (Bazel's EnvironmentalExecException class; the createFileSystem ->
  "Could not copy inputs into sandbox" role).

Deviations from genuine namespaces (all forced by this host's seccomp
filter, none by choice): no user/mount/PID/IPC/UTS namespaces; `/proc` shows
host processes (no pid-namespace isolation); no fresh `/proc`/`/sys` mounts;
no fake hostname (`sandbox_fake_hostname` unimplemented); the tmpfs mounts
of `--sandbox_tmpfs_path` are writable grants, not empty filesystems. Two
further, deliberate loosenings: `/tmp` is the **shared host /tmp** (Bazel's
linux-sandbox mounts a private, empty hermetic /tmp per sandbox when no
explicit /tmp mount exists — LinuxSandboxedSpawnRunner.useHermeticTmp:218-296
with known-path remounts — so cross-action /tmp side channels exist here
that Bazel's sandbox prevents; a workspace under ~/code that needs the host
/tmp visible uses Bazel's own answer, `--sandbox_add_mount_pair`, which is
not modelled); and **metadata operations stay allowed** — chmod, chown,
utime and xattr (setattr-class operations) are outside Landlock's ABI-4
access rights, so an action can, for example, chmod a readable file it
cannot write. Writes, creates, removes, renames and truncations are denied.

## Practical notes and known limits

- **Tool caches under $HOME fail sandboxed** — go, pip, npm, cargo and zig
  without configured cache directories write `~/.cache`-style paths, which
  the sandbox denies with EACCES ("Permission denied"), exactly like a
  genuine linux-sandbox (whose read-only remount denies the same writes).
  Point the tool's cache at TMPDIR or /tmp (both sandbox-writable), grant
  the path with `--sandbox_writable_path=<dir>`, or tag the action
  `no-sandbox` (Bazel's own answers are `--sandbox_writable_path` and the
  tag).
- **A `--cache-dir` under /tmp is writable by sandboxed actions** (the /tmp
  grant is Bazel-parity). Cross-linking docs/HARDLINK_STAGING.md: staged
  inputs are hardlinks into the content-addressed store, and Landlock does
  not restrict metadata operations — an action can chmod a staged input
  (read-only hardlink) and then write the shared store blob through it.
  This is the existing hardlink-staging limit (HARDLINK_STAGING.md):
  consumption re-verifies digests and detects the tampering unless size,
  mtime and mode are all forged. Genuine linux-sandbox's read-only remount
  prevents the write at the inode level; with the Landlock sandbox it is a
  documented known limit of this host's design (no mount namespaces
  available).

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
| `--[no]sandbox_default_allow_network` | default true: sandboxed actions reach the network; `--nosandbox_...` denies TCP unless the spawn tags `requires-network` |

Paired receipt: `tests/fixtures/sandbox/reference_receipt.json` (genuine
Bazel 9.3.0 vs this engine on the same fixtures: default strategy choice,
out-of-sandbox write failure texts, escalation absence, output bytes).
