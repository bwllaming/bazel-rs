# Sandboxed local execution (N59)

Bazel 9.3 executes Linux actions under the `sandboxed` strategy by default:
the platform default strategy list is [worker,] sandboxed, local
(`BazelStrategyModule.java:67-80`; `SandboxModule.java:353-356` registers
`sandboxed` as the default spawn strategy whenever a sandbox implementation
is supported). On Linux `sandboxed` is the `linux-sandbox` tool
(`src/main/tools/linux-sandbox.cc`) when it works on the host —
`LinuxSandboxedSpawnRunner.isSupported` runs `/bin/true` through it once —
and `processwrapper-sandbox`, a sandbox directory without any isolation,
otherwise (`SandboxModule.java:281-326` registers `sandboxed` for both,
linux-sandbox last so that it wins). This engine implements the same
semantics with the same two backends (`crate::sandbox::Backend`):

| Host | genuine 9.3.1 | bazel-rs | `INFO: N processes: …` |
| --- | --- | --- | --- |
| unprivileged user namespaces work (and a fresh `/proc` in one) | linux-sandbox | the namespace sandbox (`src/sandbox/namespaces.rs`) | `linux-sandbox` |
| user namespaces blocked (Docker's default seccomp profile — ~/code's CI runners —, Ubuntu's `apparmor_restrict_unprivileged_userns=1` — this dev host —, sysbox containers that refuse a fresh `/proc`) | processwrapper-sandbox | processwrapper-sandbox, restricted by a Landlock ruleset | `processwrapper-sandbox` |

`sandboxed` is the default, `--spawn_strategy` and `--strategy=<mnemonic>=`
select strategies per spawn, and test actions run sandboxed like any action.
Paired receipt of the backend choice: `tests/fixtures/sandbox_backend`
(genuine 9.3.1 on this host; the genuine `linux-sandbox` binary's own probe
fails here and in Docker with the reasons recorded there).

## Strategy plumbing

- `--spawn_strategy=a,b,...`: comma-separated list, highest priority first;
  a repeated flag replaces the list (CommaSeparatedNonEmptyOptionListConverter;
  the option is not allowMultiple, `ExecutionOptions.java:63-75`).
  Registered identifiers are `standalone`/`local` (`StandaloneModule.java:98`),
  `sandboxed`/`linux-sandbox` (`SandboxModule.java:326`),
  `processwrapper-sandbox` (SandboxModule.java:285-299: requested
  explicitly, a sandbox strategy that isolates nothing, like genuine's) and
  `worker` (BazelStrategyModule.java:75; persistent workers,
  docs/PERSISTENT_WORKERS.md: usable only for spawns that support workers,
  skipped for every other spawn).
  `linux-sandbox` is registered only where the namespace sandbox works:
  elsewhere `--spawn_strategy=linux-sandbox` is rejected exactly like
  genuine rejects it there (`SandboxModule` registers it only when
  `LinuxSandboxedSpawnRunner.isSupported`; receipt row `linux-sandbox`).
  Unknown identifiers are rejected once at startup with Bazel's
  STRATEGY_NOT_FOUND text, `ERROR: Build did NOT complete successfully`
  (plus `Couldn't start the build. Unable to run tests` for `test`) and
  exit code 2 (SpawnStrategyRegistry.toStrategy:
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
  (crate::action::remote_policy). A host with neither usable namespaces nor
  Landlock skips `sandboxed` the same way. When no requested strategy can execute the spawn
  the build fails with Bazel's message (`SpawnStrategyResolver.java:77-96`).
- The platform default list is Bazel's own — [worker,] sandboxed, local
  (`BazelStrategyModule.java:67-80`): `worker` executes the spawns whose
  execution requirements carry `supports-workers=1` or
  `supports-multiplex-workers=1` (docs/PERSISTENT_WORKERS.md); every other
  spawn runs `sandboxed`, then `standalone`. A strategy-list error prints the
  list like Java's `List.toString` (`[worker]`).
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
  execution; not sandboxed)" when none did, and "(experimental local
  execution; N sandboxed, M not sandboxed)" when only some did (spawns with
  `local`/`no-sandbox` requirements, Bazel's `M local` processes). On the
  bazel-gazelle cold build both tools sandbox the same 1,083 spawns and run
  the same 5 (`//internal:bazel_test`, `tags = ["local"]`) unsandboxed; the
  earlier bare "not sandboxed" note made that look like a divergence.

## The namespace sandbox (linux-sandbox)

Where unprivileged user namespaces work, `sandboxed` is genuine's
linux-sandbox, ported (`src/sandbox/namespaces.rs`; `linux-sandbox.cc`,
`linux-sandbox-pid1.cc`, `LinuxSandboxedSpawnRunner.prepareSpawn`,
`LinuxSandboxCommandLineBuilder`):

- **Probe.** Once per invocation (lazily, at the first sandboxed spawn or
  an explicit `linux-sandbox` request) the engine runs `/bin/true` through
  its own sandbox worker with the probe's options (user, mount, IPC and PID
  namespaces, the read-only remount, a fresh `/proc`), like
  `LinuxSandboxedSpawnRunner.computeIsSupported`. Any failure selects the
  fallback below. One extra process pair per invocation (~3 ms).
- **Processes.** The executor starts its own binary as `__sandbox-worker`
  (the linux-sandbox binary's role). The worker unshares the user, mount,
  IPC — plus network and UTS on demand — namespaces, maps the user (the
  client's uid/gid; `nobody` with `--sandbox_fake_username`; root for the
  `requires-fakeroot` execution requirement), sets the mounts up, then
  re-enters its working directory (EnterWorkingDirectory: the inherited cwd
  would still point at the pre-remount mount), unshares the PID namespace
  and starts pid 1 as its first process — a fork of the worker, no second
  exec: between fork and exec, still holding the worker's capabilities, it
  mounts a fresh `/proc`; then it forks the action as pid 2 in
  its own process group with umask 022 and every signal at its default,
  forwards SIGTERM to that group, reaps every process of the namespace, and
  exits with the action's code or 128 + its signal (`WaitForChild`); it also
  hands the action's exact wait status to the worker through a pipe, so a
  signalled action is reported as such like every other spawn here
  (genuine's linux-sandbox can only exit 128 + the signal).
  Leaving pid 1 kills whatever the action left (setsid daemons included).
  The worker stays outside the PID namespace as the spawn's supervisor
  (docs/PROCESS_WRAPPER.md): test timeouts (`T`), kills (`K`), the memory
  watchdog's group kill and the executor's death reach pid 1 unchanged; a
  SIGKILLed pid 1 takes the namespace with it.
- **Mounts** (`MountFilesystems`, `MakeFilesystemMostlyReadOnly`, in
  genuine's order): the mount namespace is made private (`MS_REC |
  MS_PRIVATE`); bind mounts (`MS_BIND | MS_REC`); tmpfs on every
  `--sandbox_tmpfs_path`; every writable path bound onto itself; the
  working directory (the execroot) bound onto itself; then every mount of
  `/proc/self/mounts` remounted read-only — keeping its locked
  nodev/noexec/nosuid/atime flags, retrying EBUSY, ignoring EACCES, EPERM,
  EINVAL, ENOENT, ESTALE, ENODEV — except the working directory, the
  writable paths and the tmpfs mounts. With a network namespace a fresh
  read-only sysfs is mounted on `/sys` and `lo` brought up
  (`--sandbox_enable_loopback_device`, default true). Writes outside the
  writable set fail with **EROFS** ("Read-only file system"), exactly like
  genuine.
- **Writable paths** (`getWritableDirs`, a sorted set): the execroot,
  TEST_TMPDIR and TMPDIR (relative to the execroot; outside it they must
  exist and are resolved — "Cannot resolve symlinks in TMPDIR because it
  doesn't exist: …" otherwise), `--sandbox_writable_path` entries and their
  resolved targets, the resolved `/dev/shm`, and `/tmp`. The host `/dev` is
  remounted read-only like the rest (device nodes stay usable).
- **Hermetic /tmp** (`useHermeticTmp`, always on in Bazel 9): each spawn's
  own empty `<sandbox>/_hermetic_tmp` is mounted at `/tmp` as the final
  mount, unless a `--sandbox_add_mount_pair` has the source `/tmp` (~/code's
  `build:linux_* --sandbox_add_mount_pair=/tmp`), a `--sandbox_tmpfs_path`
  is `/tmp`, or a bazel-rs root is `/tmp` itself. The bazel-rs roots below
  `/tmp` — the cache directory (sandbox base, CAS, output base), the
  workspace, the repo contents cache — are mounted back at their first two
  segments (`/tmp/x/y/z` → `/tmp/x`, `collectPathsToMountUnderHermeticTmp`),
  and writable paths and tmpfs mount points below `/tmp` are created inside
  the hermetic tmp first. TMPDIR keeps its value (the client's TMPDIR, or
  `/tmp` — now the private one); a client TMPDIR below `/tmp` stays the
  shared host directory, as in genuine. Mount pairs whose target lies below
  `/tmp` are moved into the hermetic tmp; a mount on `/tmp` itself with
  hermetic tmp fails with genuine's text.
- **`--sandbox_add_mount_pair`**: target ← source bind mounts (a relative
  source is relative to the execroot; per source the last pair wins, per
  target the last source), read-only unless the target is writable,
  validated like `LinuxSandboxUtil.validateBindMounts`: "Mount source '…'
  does not exist.", "Mount target '…' is a file but mount source '…' is a
  directory, they must be the same type.", "Mount target '…' does not
  exist. Bazel only supports bind mounting on top of existing
  files/directories. …" — spawn failures, like genuine's UserExecException.
- **`--sandbox_block_path`**: each existing (resolved) path is covered by an
  inaccessible helper — an empty mode-000 directory or file. Genuine keeps
  the helpers in the sandbox base; here they live on a private tmpfs the
  worker mounts on `<sandbox>/sandbox-helpers`, so nothing mode-000 is ever
  left in the cache. Reads fail with EACCES.
- **`--sandbox_tmpfs_path`**: an empty tmpfs (`MS_NOSUID | MS_NODEV |
  MS_NOATIME`); the working directory may not lie below one (genuine's
  text).
- **Network**: a network namespace exactly when the spawn must be denied
  the network (`block-network`, or `--nosandbox_default_allow_network`
  without `requires-network`; `Spawns.requiresNetwork`) — all protocols,
  loopback up unless `--nosandbox_enable_loopback_device`.
- **`--sandbox_fake_hostname`**: a UTS namespace with hostname `localhost`
  and domain name `localdomain`. **`--sandbox_fake_username`**: the user is
  `nobody`.
- Not modelled: `--experimental_use_hermetic_linux_sandbox` (pivot_root into
  a hardlinked tree; an unknown option here), cgroups
  (`--experimental_sandbox_memory_limit_mb` and friends),
  `--sandbox_explicit_pseudoterminal`. Persistent workers with
  `--worker_sandboxing` keep the Landlock wrapper (genuine's sandboxed
  workers are not linux-sandbox spawns either).
- **Test-only knob** `BAZEL_RS_SANDBOX_NAMESPACES`: `off` skips the
  namespaces (the fallback runs; measurements), `bind-proc` lets pid 1 bind
  the host `/proc` (and keep the host `/sys`) when the kernel refuses a fresh
  mount. Container runtimes like sysbox overmount `/proc` files, and the
  kernel's `mount_too_revealing` check then refuses a fresh proc in a nested
  user namespace — genuine's linux-sandbox dies there (`"mount /proc":
  Invalid argument`, receipt), and so does the default probe. The knob is how
  the namespace path is exercised on this machine:
  `docker run --security-opt seccomp=unconfined --security-opt
  apparmor=unconfined -e BAZEL_RS_SANDBOX_NAMESPACES=bind-proc …` (process
  listings under `/proc` are then the container's; the PID namespace still
  ends with pid 1). Integration tests (`tests/sandbox.rs`) adapt to the
  backend the engine's probe selects and skip the namespace-only checks with
  a message where it is unavailable.

### Per-spawn cost

200 trivial genrules, `--jobs=1`, cold cache, release binary, in a Docker
container with user namespaces allowed (`BAZEL_RS_SANDBOX_NAMESPACES=bind-proc`;
101 mounts in the container's table), median of 3:

| Strategy | wall | per spawn |
| --- | --- | --- |
| standalone | 1.74 s | 8.7 ms |
| processwrapper-sandbox (explicit, no isolation) | 1.59 s | 8.0 ms |
| sandboxed, Landlock fallback | 1.78 s | 8.9 ms |
| sandboxed, namespace sandbox | 3.38 s | 16.9 ms |
| — same, genuine's per-mount `MS_REMOUNT` loop instead of `mount_setattr` | 38.5 s | 192 ms |

The namespace sandbox adds ~8 ms per spawn: unsharing and tearing down the
namespaces, ~2 ms of mounts. MakeFilesystemMostlyReadOnly's one remount per
mount costs ~1 ms per mount here, so the read-only view is made with
mount_setattr(2) (Linux 5.12) — the whole tree read-only recursively, then
the writable mount points cleared — the same end state; older kernels take
genuine's loop. Pid 1 is a fork of the worker's child, not another exec.

## The fallback sandbox (processwrapper-sandbox + Landlock)

Where user namespaces are unavailable, genuine runs processwrapper-sandbox,
and so does this engine — by name (`INFO: N processes: … processwrapper-sandbox`,
the progress display, the execution log, BEP), and in what a spawn sees: the
shared host `/tmp`, the host's hostname and user, mount pairs and tmpfs
paths without mounts (genuine's processwrapper-sandbox ignores both; receipt
rows `fake-hostname-username`, `remount`). On top of that, so that
out-of-sandbox writes still FAIL, the sandbox worker applies a Landlock
ruleset (Linux LSM, ABI 4 on this kernel) before it starts the action —
exactly the point where linux-sandbox applies its namespace setup. An
explicitly requested `processwrapper-sandbox` stays without the ruleset,
like genuine's.

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

Deviations of the fallback from genuine's linux-sandbox (forced by the
missing namespaces, none by choice — genuine's own fallback isolates
nothing): no user/mount/PID/IPC/UTS namespaces; `/proc` shows host
processes; no fresh `/proc`/`/sys` mounts; no fake hostname or username;
`--sandbox_tmpfs_path` entries are writable grants, not empty filesystems;
`--sandbox_add_mount_pair` is ignored (like genuine's processwrapper-sandbox);
`/tmp` is the **shared host /tmp**, exactly as under genuine's
processwrapper-sandbox (cross-action /tmp side channels exist that the
namespace sandbox's hermetic /tmp prevents). TMPDIR has genuine's value under every strategy — the
client's `TMPDIR`, or `/tmp` (PosixLocalEnvProvider.rewriteLocalEnv;
docs/ACTION_ENV.md, `tests/fixtures/action_tmpdir`) — and is the shared
host directory too, not a per-action directory: a per-action directory below
the cache would put Unix socket paths beyond `sun_path`; and **metadata operations stay allowed** — chmod, chown,
utime and xattr (setattr-class operations) are outside Landlock's ABI-4
access rights, so an action can, for example, chmod a readable file it
cannot write. Writes, creates, removes, renames and truncations are denied.

## Practical notes and known limits

- **Tool caches under $HOME fail sandboxed** — go, pip, npm, cargo and zig
  without configured cache directories write `~/.cache`-style paths, which
  the sandbox denies — EROFS ("Read-only file system") under the namespace
  sandbox, exactly like genuine linux-sandbox, EACCES ("Permission denied")
  under the Landlock fallback (genuine's processwrapper-sandbox would let
  them through).
  Point the tool's cache at TMPDIR or /tmp (both sandbox-writable), grant
  the path with `--sandbox_writable_path=<dir>`, or tag the action
  `no-sandbox` (Bazel's own answers are `--sandbox_writable_path` and the
  tag).
- **A `--cache-dir` under /tmp is writable by actions of the Landlock
  fallback** (the /tmp grant is Bazel-parity; under the namespace sandbox the
  cache is mounted read-only into the hermetic /tmp). Cross-linking docs/HARDLINK_STAGING.md: staged
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
| `--sandbox_writable_path=<path>` | keep a path writable (with its symlink target): bound onto itself (namespaces) / granted writable (Landlock) |
| `--sandbox_block_path=<path>` | make the path unreadable: an inaccessible helper mounted over it (namespaces) / no Landlock read grant on its subtree |
| `--sandbox_tmpfs_path=<path>` | an empty tmpfs (namespaces) / the path granted writable, no mount (Landlock) |
| `--sandbox_add_mount_pair=<path>[:<path>]` | a bind mount target ← source, validated like genuine (namespaces) / ignored like genuine's processwrapper-sandbox (Landlock); a `/tmp` source disables the hermetic /tmp; converter errors are Bazel's |
| `--sandbox_fake_hostname` | hostname `localhost` in a UTS namespace (namespaces) / no effect, like processwrapper-sandbox |
| `--sandbox_fake_username` | user `nobody` (namespaces) / no effect, like processwrapper-sandbox |
| `--[no]sandbox_enable_loopback_device` | default true: a sandbox network namespace has `lo` up |
| `--[no]sandbox_default_allow_network` | default true: sandboxed actions reach the network; `--nosandbox_...` denies it (a network namespace / Landlock's TCP denial) unless the spawn tags `requires-network` |

Paired receipts: `tests/fixtures/sandbox/reference_receipt.json` (genuine
Bazel 9.3.0 vs this engine on the same fixtures: default strategy choice,
out-of-sandbox write failure texts, escalation absence, output bytes) and
`tests/fixtures/sandbox_backend/reference_receipt.json` (genuine 9.3.1: the
backend and runner name on a host without user namespaces, an explicit
`linux-sandbox`, the namespace-only options).
