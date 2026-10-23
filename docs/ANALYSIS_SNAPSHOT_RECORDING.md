# Analysis snapshot: the recording layer (L2-W3) and local-repository replay (L2-W4R)

Native-only infrastructure for a future persistent analysis snapshot (a whole
request's plan saved to disk and reused when nothing it observed changed, in
the style of Gradle's configuration cache). This wave ships only the recording
layer: every filesystem and environment observation of planning goes through
one facade that can record it, plus a diagnostic mode that reports what a
snapshot would have to validate and why a run could not be saved. Nothing is
saved or loaded; every command's output is byte-identical with recording on
and off.

## The facade: `crate::observed_fs`

Loading, module resolution and analysis read the filesystem and the
environment only through `observed_fs::{read, read_to_string, read_staged,
sha256_file, metadata, symlink_metadata, exists, is_file, is_dir,
canonicalize, read_link, read_dir_projected, env_var}`. Each performs exactly
the system call of the `std` function it replaces and returns its result
unchanged. Outside the recording window the cost is one relaxed atomic load.

Inside the window each call records `(op, path) -> outcome`, the value the
caller actually used:

| Op | Outcome |
|---|---|
| READ | SHA-256 of the bytes read; the stat key (dev, ino, size, mtime_ns, ctime_ns) from an lstat before the read (a symlink's target stat, the link itself becoming an LSTAT) that must equal the fstat after it; `racy` when max(mtime, ctime) is within 2 s of the window start or later |
| STAT | the type class (file, dir, other) or the errno |
| LSTAT | the type class; a symlink's target |
| READDIR | the digest of the projected `(name, type)` entries, the directory's stat key before and after the listing, `racy` |
| CANON / READLINK | the resulting path or the errno |
| ENV | a digest of the value (never the value); names fixed by `--repo_env=NAME=VALUE` / `=NAME` are marked as such |

Listings are projected to what the caller uses, so unrelated changes do not
invalidate a snapshot: package discovery (`//x/...`, toolchain patterns)
records only subdirectories; a glob listing records the entries the include
pattern can match or descend into at that depth plus the BUILD markers a
subpackage lookup consults (`glob::listing_keeps`); recorded repository inputs
(`dirents`) record everything. While recording, the facade returns only the
projected entries, so a caller cannot depend on an unrecorded entry.

The same `(op, path)` observed twice with different outcomes, or a file that
changed while it was read, makes the run **Unstable**.

Semantic records complete the filesystem ones:

* **REPO + INTEGRITY**: every repository tree whose integrity the run verified
  (registry archives, direct/extension repositories, Git trees, the embedded
  `@bazel_tools`, and a published store tree a symlink crossed into, see
  *Link anchors*) with the lstat key of each entry and of its manifest; the
  spec pointer naming a (non-local) direct repository is a READ.
* **REPLAY**: every `local = True` repository rule the window executed, with
  the pinned tables its replay needs (see *Local repository rules*).
* **TAINT**: conditions under which a run is never saved — network access,
  an executed non-local repository rule (a cold fetch or a changed
  specification), a local one that printed (`local-repository-output`), a
  local one whose replay did not reproduce it (`local-repository-replay-mismatch`,
  the record mode's dry run), an evaluated (not lockfile-reused) module
  extension, a (re)published repository tree, a pending MODULE.bazel.lock or
  hidden-lockfile write, a symlink hop into another live repository root
  (`cross-repository-symlink`: a `local_path_override` module or a local
  registry source), a build-info callback (`transform_version_file` /
  `transform_info_file`), a `map_each` over a tree artifact, errors (planning
  failed or reported errors, or the command failed), Unstable, and test/debug
  hooks (`BAZEL_RS_LOADING_JITTER`, `BAZEL_RS_LOADING_NO_DEMAND`,
  `BAZEL_RS_DEBUG_*`).

## Local repository rules (L2-W4R, PLAN-L2 update 1 §(a), §(b))

`local = True` rules execute on every run (Bazel refetches local repositories
in every batch command and after every server restart): on ~/code 45 of them
run in every warm command (rules_java's toolchain configurations and
`local_jdk`, the rules_java/rules_cc compatibility proxies, bazel_features,
the host platform, rules_swift's autoconfiguration, `local_config_winsdk`).
Their executions are no taint. Each becomes a replay record
(`crate::snapshot::replay::ReplayRecord`): the definition as the run loaded it
(the lockfile `repoRuleId` of an extension's repository, or a `use_repo_rule`
group's label and symbol), the bound attributes, the canonical and original
names, and the digests of the two runtime parts of its identity — the exact
serialized identity inputs (`host_os::repository_inputs`) and the encoded
tree inventory — that `repository_store::materialize_local` reports. The
provenance part of the identity is fixed by what the snapshot validates, so
a replay that reproduces both digests reproduces the identity and the root
path the plan names. A local rule never reuses a publication, so its
specification pointer is neither read nor recorded, and an unchanged
pointer is no longer rewritten (`publish_pointer` keeps identical bytes).

A replay (`crate::snapshot::replay::replay`) re-executes every record in the
same stage directory and under the same stage lock as a full run
(`direct/stages/<blake3(name)>`, `locks/stage-<slug>`: `java
-XshowSettings:properties` prints its working directory), publishes nothing,
deletes the stage, and compares both digests. It runs against a *pinned*
repository set (`RepositorySet::pinned`) that answers `root`, `mapping`, label
parsing, the link policy and the "generated by a repository rule" flag from
tables recorded in the window: the `root()` and `mapping()` results the
definition loads of repository rules and the local executions consulted (a
replay scope is open on their thread). It never reads MODULE.bazel, resolves
the module graph, builds the extension catalog or materializes anything;
anything unpinned is an error, which makes the run a miss, never a false hit.
Definitions load optimistically from the recorded roots (read-only); a root
that a local rule publishes is confirmed by that rule's own replay. One
thread per definition replays its records (the 36 toolchain configurations
share one load). A definition load evaluates the `.bzl` closure again with
the loader's default print handler: a top-level `print()` in that closure
(none on ~/code) would print during the replay — the record mode's dry run
shows it after the build's output; the L2-W5 hit path captures fd 2 while it
replays (or `loading` gains a print sink for definition loads).

What a repository rule prints — `print()`, the `execute(quiet=False)` echo,
the reproducibility `DEBUG:` lines — still reaches stderr as it happens and
is also collected per execution (`repository_ctx::execute`,
`ExecutedRepository::output`). Printing as it happens keeps every byte at
its position, also when a rule demands another repository while it runs:
the other rule's output appears between the demanding rule's earlier and
later lines, as before and as Bazel's suspended fetch prints it (writing a
buffer when the execution ends would move it after the nested output;
`tests/local_repository_replay.rs`). A replay never prints
(`execute_captured`). v1 of the output protocol does not splice live output
into replayed window bytes: a local rule that printed taints the run
(`local-repository-output`); W7's splicing needs per-segment offsets in the
captured window for nested executions.

The execution also notes impurity flags that never enter an identity: a read
of `repository_ctx.os.environ` and the host facilities (`execute`, `which`,
downloads). They feed the record mode's report, the syscall audit and the
optional purity classes; every record is replayed regardless. Building
`repository_ctx.os` itself records nothing (the old `environment-enumerated`
taint fired for every execution).

## Link anchors (PLAN-L2 update 1 §(c))

A symlink target inside a published store tree —
`<store>/{trees,direct/trees,git/trees,builtins/trees}/<id>` with its manifest
(`repository_store::published_tree`) — anchors the link whatever the
invocation materialized before (`source_links::StoreTrees`), as Bazel follows
such a link and fetches the target repository. A tree no root of the
invocation names is verified against its manifest's content on its first
crossing (memoized per repository set) and recorded as a REPO. Stages, trash
and every other store path stay blocked. Previously the link resolved only
when the target repository happened to be materialized earlier in the run;
otherwise it failed (an order-dependent error, now fixed:
`tests/local_repository_replay.rs`).

Hops into store trees, into the main repository (always materialized) and to
plain host paths (outside the store and every live root; every hop is
recorded) resolve the same in any order and no longer taint. A hop into
another live root — a `local_path_override` module or a local registry
source — still taints (`cross-repository-symlink`); the former `host-symlink`
taint is gone (a host target inside a known live root anchors to that root).

Not recorded by design: the registry's content-addressed file cache (checked
against its hash; a miss is a network fetch, which taints), the workspace
status and inherited action-environment values (recomputed after planning on
every run), and everything before the window (bootstrap values that belong to
the snapshot key).

## Record mode

```
BAZEL_RS_SNAPSHOT=record [BAZEL_RS_SNAPSHOT_DUMP=<file>] bazel-rs build|test|run|query ...
```

The window opens at the snapshot check point (after option parsing and the
other bootstrap values, before the registry session opens) and closes at the
end of planning (the `plan.end` mark of build, test and run; the end of
loading for query). At the end of the command (for test and run, once their
build part ended) the mode prints `bazel-rs-snapshot:` lines on stderr:
observation counts by op and READDIR projection, distinct paths, environment
reads, verified repositories and integrity entries, every taint with its
first reasons, `eligible=yes|no|unless-pruned` (`unless-pruned`: only
registrations the saved plan may still prune — build-info callbacks,
`map_each` — taint, which L2-W4 decides on the plan), the measured time of
the validation a hit would run, and the replay dry run of every local
repository rule of the window (`replay dry-run: M/N matching over D
definition(s); wall, CPU of the replay threads and of child processes; pinned
roots and mappings`, the slowest replays, every mismatch; a mismatch taints).

Validation (`crate::snapshot::validate`, decision (1) of PLAN-L2 update 1) is
a dry run of the hit's checks under the command's policy: per tree by default
(the observations outside verified repository trees, plus each tree's root
and manifest — a content-addressed tree whose root and manifest are
unchanged stands for everything inside it, like Bazel trusting repository
markers), every INTEGRITY entry under the existing `--verify_repo_tampering`
/ `BAZEL_RS_VERIFY_REPOS=full`. Settled stat keys are trusted, racy or
changed ones re-read and compared by content. Below 4,000 checks one thread
runs them inline and in place (no work list: in the large, fragmented heap
at the end of a full analysis one allocation cost the record mode's dry run
3–7 ms of glibc malloc housekeeping, more than the checks themselves);
otherwise path-sorted blocks on min(available parallelism, 16) scoped
threads. The report line splits the wall time into setup, checks and
environment. Record-mode knobs:
`BAZEL_RS_SNAPSHOT_THREADS` (validation threads), `BAZEL_RS_SNAPSHOT_DRY_RUNS`
(repeat the validation and replay dry runs), `BAZEL_RS_SNAPSHOT_COMPARE=1`
(also time the other validation policy).

With `BAZEL_RS_SNAPSHOT_DUMP` the observations (`OBS`), environment digests
(`ENV`), repositories and integrity entries (`REPO`, `INT`), taints, the
pinned tables (`ROOT name path generated:0|1`, `MAPPING name entries:N
isolated:N`), the replayed definitions (`DEFINITION source records:N load:ms
thread-cpu:ms`), the replay records with their outcome (`REPLAY name
match|mismatch definition:… flags:environ,execute,which|- output:bytes
recorded:ms replay:ms root:… [reason:…]`) and the summary are written to the
file, one record per line, paths with every non-graphic byte escaped as
`\xNN`.

The diagnostics appear only when `BAZEL_RS_SNAPSHOT` is set, always with the
`bazel-rs-snapshot:` prefix (strip them like `PERF` lines when comparing
stderr). Any other value of the variable is reported and ignored.

## Keeping the record complete

1. **Static check** (`tests/observed_fs_discipline.rs`): outside
   `#[cfg(test)]` code, no module of `src/` may call the `std::fs` readers,
   `File::open`, `OpenOptions`, the path methods (`exists`, `metadata`,
   `read_dir`, `canonicalize`, `is_file`, `is_dir`, ...) or `std::env::var*`
   unless it is an execution-phase or storage module on the allowlist
   (`action`, `sandbox`, `scheduler`, `test_*`, `run_command`, `spawn_lock`,
   `archive`, `materializer`, `repository_store`, `builtin_tools`, `git_*`,
   `patching`, `repository_ctx`, `extension_*`, `download_proxy`,
   `credentials`, `remote_cache`, `reapi`, `memory_limit`, `bazelrc`, `main`,
   `workspace_status`, `runfiles_tree`, `lib`, and the recorder itself:
   `observed_fs`, `snapshot/*`), the variable is a diagnostics switch or test
   hook, or the use carries a `// snapshot-unrecorded: <reason>` comment.
   Allowlisted storage modules record what they verify through the semantic
   hooks (`repository_tree`, `note_read`, `taint`).
2. **Syscall audit** (`scripts/snapshot_fs_audit.py`): runs bazel-rs under
   `strace -f -y -e trace=%file,getdents64,%process` in record mode; the
   window is bracketed by stats of
   `/.bazel-rs-snapshot-marker/window-{open,close}`. Every path touched inside
   the window outside the cache dir must be covered by the dump (an
   observation, a step of a canonicalized path's resolution, a listed
   directory, or a path inside a verified repository tree); `/proc`, `/sys`,
   `/dev` and the workspace status's host facts are exempt, and so is work
   that taints the run anyway: a repository rule or module extension
   executing is bracketed on its thread by
   `/.bazel-rs-snapshot-marker/tainted-{begin,end}`
   (`observed_fs::tainted_section`; a local rule is replayed instead), and
   processes started inside the window are skipped with all their threads.
   The replay dry run after the window
   (`/.bazel-rs-snapshot-marker/replay-{begin,end}`) is audited too: a replay
   may touch recorded paths, the cache dir (its stage, the store's trees) and
   pinned roots (`ROOT` lines), and — inside the execution of a rule the dump
   marks impure (bracketed per thread by
   `/.bazel-rs-snapshot-marker/replay-exec/NAME` and `.../replay-exec-end`) —
   any host path; processes it starts are skipped. Without arguments it
   audits a copy of `tests/fixtures/snapshot_audit/workspace`; `--workspace
   DIR --cache DIR -- ARGS` audits any workspace.

Shadow verification and the determinism gate (layers 3 and 4) come with the
snapshot store itself.

## Measured on ~/code (L2-W4R, warm, update-mode recipe)

| | oauth2token:all `--nobuild` | oauth2token/... build | B2 `--nobuild` |
|---|---|---|---|
| eligible | unless-pruned | unless-pruned | unless-pruned |
| remaining taint | build-info ×8 (0 planned of 689) | build-info ×8 (0 of 691) | build-info ×12 (0 of 3,615) |
| replay dry run | 45/45 in every run (4 runs + 1 under strace) | 45/45 (2 runs) | 45/45 (3 runs + 1 under strace) |
| replay wall / CPU | 119–153 ms / 103–172 ms + 50–80 ms in `java` | 133–137 ms / 105–115 ms + 70 ms | 112–134 ms / 102–119 ms + 60–70 ms |
| validation [per-tree], 1 thread | 1.5–2.7 ms (613 obs + 220 entries) | 2.5–2.7 ms | 7.1–8.6 ms (2,206 obs + 596 entries) |
| syscall audit (window / replay) | 29,757 / 2,730 touches, 0 unrecorded | — | 139,268 / 2,727, 0 unrecorded |

The critical path of a replay is `local_jdk`: its definition load (the
rules_java/rules_cc `.bzl` closure, 57–75 ms) plus its execution (`java`,
47–69 ms); the other 44 replay
in 0.2–1.6 ms each next to it (the 37 `_toolchain_config` repositories share
one 3–9 ms load). No local rule printed. Every build-info registration is
`@bazel_tools//tools/build_defs/build_info:cc_build_info`, pruned by the plan
(none of their outputs is a planned action): L2-W4 decides that taint on the
saved plan, after which these runs report `eligible=yes`.

## Behaviour changes of L2-W4R (outside record mode)

* A `.bzl`, source or glob symlink into a published store tree resolves even
  when the target repository was not materialized earlier in the run (it was
  an error before, depending on the order of demands).
* A local repository rule no longer reads its specification pointer before
  executing (it never reuses one): a corrupted pointer of a local repository
  is repaired by the execution instead of failing the command. Unchanged
  pointers of any repository are no longer rewritten.

Not changed: what repository rules print reaches stderr as it happens (also
collected per execution for the replay record).

## Not yet (L2-W5)

The replay records and pinned tables live in memory: saving them needs
serialization (`custom_rule::AttrValue` lacks `Deserialize`, owned by L2-W4's
derives), the hit path runs the replays in parallel with validation and
splices nothing (v1), and the self-healing rule (delete the snapshot when an
action fails on a missing store-tree input) needs the store.
