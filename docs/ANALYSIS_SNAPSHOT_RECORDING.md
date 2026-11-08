# Analysis snapshot: the recording layer (L2-W3) and local-repository replay (L2-W4R)

(L2-W4 adds the planned request a snapshot saves and decides the plan-dependent taints on it:
docs/PLANNED_REQUEST.md.)

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
canonicalize, read_link, read_dir_projected}` and the environment only
through the repository environment (crate::repository_environment, which
records what it reads with `observed_fs::note_env`). Each performs exactly
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
| ENV | a digest of the repository environment's value (never the value): the client environment with `--repo_env` applied, which a hit validates against its own command's repository environment (L2-W5f: no name counts as "fixed" — the last `--repo_env` entry wins) |

Listings are projected to what the caller uses, so unrelated changes do not
invalidate a snapshot: package discovery (`//x/...`, toolchain patterns)
records only subdirectories; a glob listing records the entries the include
pattern can match or descend into at that depth plus the BUILD markers a
subpackage lookup consults (`glob::listing_keeps`); recorded repository inputs
(`dirents`) record everything. While recording, the facade returns only the
projected entries, so a caller cannot depend on an unrecorded entry.

A `glob()` with excludes (ci-pipeline-perf, snapshot format 6) records its
listings as `Projection::Excluding`: the digest leaves out the regular files
an exclude pattern removes (never a BUILD marker), while the walker still
sees them (each include's `allow_empty` check counts them, and the native
host-entry cap). rules_python's interpreter trees glob `lib/**` without
`**/__pycache__/*.pyc*`, whose `__pycache__` links into
`/tmp/rules_python_pycache`, where every action running Python writes: on
~/code a full listing there made every snapshot of a build that ran Python
actions miss on its next lookup (`READDIR[all] .../encodings/__pycache__:
listing changed`), so neither a re-run nor the `test` after a `build` hit.

A READ answered by a digest memo instead of the facade's own read
(integration-20) is recorded the same way by `observed_fs::note_read_digest`:
the warm revalidation of recorded repository-rule inputs takes file digests
from the recorded-input memo and the persistent digest table
(`repository_ctx::recorded_file_state`, `observed_tree_digest`; a hit reads
nothing), and every answer, hit or read, becomes the READ of that path with the
stat key the lookup used — the opened file's fstat, for a symlink the target's
plus the link's LSTAT — never a fresh lstat.

The same `(op, path)` observed twice with different outcomes, or a file that
changed while it was read, makes the run **Unstable**.

Semantic records complete the filesystem ones:

* **REPO + INTEGRITY**: every repository tree whose integrity the run verified
  (registry archives, direct/extension repositories, Git trees, the embedded
  `@bazel_tools`, and a published store tree a symlink crossed into, see
  *Link anchors*) with the lstat key of each entry and of its manifest; the
  spec pointer naming a (non-local) direct repository is a READ. A tree's
  owner records it (`observed_fs::repository_tree`); a crossing records a
  tree no repository of the run named yet (`crossed_tree`) and the owner's
  record replaces it if the repository is materialized later, so the
  recording does not depend on which came first. A local repository's tree
  is never a REPO (`local_tree`: it is replayed, its observations are
  validated one by one).
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
  `transform_info_file`) or a `map_each` over a tree artifact of an action
  the planned request keeps (analysis records them at registration; L2-W4's
  `plan_request` settles them on the plan, see *Record mode*), errors (planning
  failed or reported errors, or the command failed), Unstable, and test/debug
  hooks (`BAZEL_RS_LOADING_JITTER`, `BAZEL_RS_LOADING_NO_DEMAND`,
  `BAZEL_RS_DEBUG_*`; integration-20 adds the analysis self-checks
  `BAZEL_RS_VERIFY_VIEWS` / `BAZEL_RS_VERIFY_ARTIFACTS`, the persistent
  digest table's switch `BAZEL_RS_DIGEST_MEMO` and the retained-memory
  breakdown `BAZEL_RS_TIMING_ALLOC`: `snapshot::record::TEST_HOOKS`).

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
same directory and under the same writer lock as a full run (the repository's
slot `external/<name>`, `direct/locks/slot-<slug>{,.gate}`,
docs/REPOSITORY_SLOTS.md: `java -XshowSettings:properties` prints its working
directory), publishes nothing, restores the slot for the recorded tree, and
compares both digests. It runs against a *pinned*
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
the reproducibility `DEBUG:` lines — still reaches stderr as it happens and,
while the record mode's replay capture records local executions, is also
collected per execution (`repository_ctx::execute`,
`ExecutedRepository::output`; otherwise nothing reads it, so nothing is
kept). Printing as it happens keeps every byte at
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
crossing (memoized per repository set; registry, direct and Git manifests
alike) and recorded as a REPO; a tree the invocation materialized is
recorded by its owner. When the materialized root that contains the target
already is that tree (a link within one store tree, e.g. a glob through an
internal link), nothing is checked: the test is lexical. Stages, trash and
every other store path stay blocked. Previously the link resolved only when
the target repository happened to be materialized earlier in the run;
otherwise it failed (an order-dependent error, now fixed:
`tests/local_repository_replay.rs`).

**Stale trees (caveat).** Store trees are content-addressed, so a link names
one publication of its target repository. A reused repository can hold a
link into an *older* tree of its target (the target was republished, e.g.
with other BUILD bytes, while the linked file kept its digest, so the
linking repository's recorded inputs still validate): the link resolves to
that old, verified tree, where Bazel would read the target repository's
current contents. Likewise a link into a local repository's tree reads its
last publication, where Bazel refetches the local repository. Both are rare,
and a hit and a miss agree (the full-analysis path resolves the same way).
The record mode names such trees: `link anchors: N store tree(s) a link
crossed into never became a repository root in this run` (only for runs that
executed or published no repository, which could root a tree without a
REPO record).

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
reads, verified repositories and integrity entries (with the observations
inside them, and how many of those lie at or below a tree's symlinks), store
trees a link crossed into that no repository of the run has as its root
(`link anchors:`, see *Stale trees*), every taint with its
first reasons, `eligible=yes|no`, the plan check (`plan check:
build-info-callback registered N (D distinct), K in the plan; map-each-deferred
...`: L2-W4 decides these two taints on the planned request — what analysis
registered but the plan pruned does not taint; `eligible=unless-pruned` remains
only for a run whose planning never completed, which errors taint anyway), the
planned request's encoding (`plan: N actions, encoded B bytes ...; encode,
decode ms`, docs/PLANNED_REQUEST.md), the measured time of
the validation a hit would run, and the replay dry run of every local
repository rule of the window (`replay dry-run: M/N matching over D
definition(s); wall, CPU of the replay threads and of child processes; pinned
roots and mappings`, the slowest replays, every mismatch; a mismatch taints).

Validation (`crate::snapshot::validate`, decision (1) of PLAN-L2 update 1) is
a dry run of the hit's checks under the command's policy: per tree by default
(the observations outside verified repository trees and those at or below a
symlink of a verified tree, plus each tree's root and manifest — a
content-addressed tree whose root and manifest are unchanged stands for
everything inside it, like Bazel trusting repository markers, but not for
what its links reach: the glob walker lists a host directory through a
tree's link, and `glob::is_subpackage` stats its BUILD files, under the
logical path inside the tree, so `ctx.symlink("/usr/include/x", "inc")` plus
`glob(["inc/**"])` must miss when a header is added there), every INTEGRITY
entry under the existing `--verify_repo_tampering` /
`BAZEL_RS_VERIFY_REPOS=full`. Settled stat keys are trusted, racy or
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

With `BAZEL_RS_SNAPSHOT_DUMP` the digest of the INPUTS a snapshot would
hold, with what depends on the time of the run zeroed (`INPUTS-CANONICAL`,
W6R: equal for any loading thread count), the observations (`OBS`), environment digests
(`ENV`), repositories and integrity entries (`REPO`, `INT`), taints (and,
once the plan decided them, the plan-dependent registrations as
`REGISTERED kind count:N detail` lines), the
pinned tables (`ROOT name path generated:0|1`, `MAPPING name entries:N
isolated:N`), the replayed definitions (`DEFINITION source records:N load:ms
thread-cpu:ms`), the replay records with their outcome (`REPLAY name
match|mismatch definition:… flags:environ,execute,which|- output:bytes
recorded:ms replay:ms root:… [reason:…]`) and the summary are written to the
file, one record per line, paths with every non-graphic byte escaped as
`\xNN`.

The diagnostics appear only in the record mode (`BAZEL_RS_SNAPSHOT=record`),
always with the `bazel-rs-snapshot:` prefix (strip them like `PERF` lines when
comparing stderr). An unknown value of the variable is reported and ignored:
the default, `on` (L2-W7, docs/ANALYSIS_SNAPSHOT.md).

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
   hooks (`repository_tree`, `note_read`, `note_read_digest`, `taint`).
   `digest_memo` is not on the allowlist: only its own table files (and the
   hashing it does for callers that record the answer themselves) carry
   `snapshot-unrecorded:` comments.
2. **Syscall audit** (`scripts/snapshot_fs_audit.py`): runs bazel-rs under
   `strace -f -y -e trace=%file,getdents64,%process` in record mode; the
   window is bracketed by stats of
   `/.bazel-rs-snapshot-marker/window-{open,close}`. Every path touched inside
   the window outside the cache dir must be covered by the dump (an
   observation, a step of a canonicalized path's resolution, a listed
   directory, or a path inside a verified repository tree but not at or below
   one of its symlinks), and a file opened for reading needs a READ record of
   that path (its content digest; reported as `UNREAD` otherwise) unless it
   lies inside such a tree; `/proc`, `/sys`,
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
snapshot store itself. The recording's own determinism is tested now
(`record_mode_observes_the_same_inputs_in_every_loading_order`): every
observation, environment digest, REPO and INTEGRITY record (without the
`racy` flags), replay record, pinned table and taint but the test hook's is
the same at 1, 8 and 32 loading threads under `BAZEL_RS_LOADING_JITTER`
seeds, and never Unstable, on a workspace with globs through links and
subpackages, a replayed local repository and a link into another
repository's store tree raced against that repository's demand.

## Recording with phase-1 prefetch (W6R)

W6's phase-1 prefetch workers load packages ahead of the coordinator —
also packages the build never commits (a select() branch not taken, a
toolchain implementation resolution never selects) — and fill memos
shared by the invocation's threads, which the coordinator then hits
without repeating the work. Integration-21 kept prefetch off while
recording, so every snapshot miss paid phase 1's sequential loading.
Since W6R prefetch runs while recording (`on` misses, `record`, verify's
shadow analysis), and the recording is still the one the threads-1 run
makes: the observations travel with the three things the sequential
engine's view of the roots (`RepositorySet::sequential_roots`, W6 review
F2) already travels with.

* **Frames** (`observed_fs::unit_scope`, `capture`, `capture_detached`,
  `discard`): while a frame is open on the observing thread the window's
  recorder appends each observation, taint, environment digest and tree
  claim to the innermost frame instead of its shards. A claim carries the
  REPO record walked on the observing thread (none when the recorder
  already has the tree's owner) and is decided when it reaches the
  recorder, by the same rules (an owner's record replaces a crossing's, a
  local claim drops one).
* **Load units.** A unit evaluated on a loading worker (no-demand mode:
  prefetch, P2 pattern loading, the `BAZEL_RS_LOADING_NO_DEMAND` hook)
  records into its frame (`LoadUnit::observations`), committed by
  `Engine::commit_load_unit` exactly where it notes the unit's roots; a
  sentinel unit's frame is dropped (its inline re-run records), and so
  are the frames of units nobody demands and P2 units after the first
  error.
* **Memo entries.** A memo a worker can fill and whose hit skips the miss
  path's observations keeps a captured set (`observed_fs::Captured`) with
  the entry, and every hit replays it — into the hitting unit's frame, or
  into the recorder, where a set lands at most once: the shared
  BUILD-phase `.bzl` cache (what entering and evaluating a module
  observed; replayed at both hit points beside `note_obtained`), the
  ignored subdirectories, and an extension repository's cached spec key
  (its declared environment). The repository environment records its
  value at every lookup, not only at the first read, and the store path is
  computed on the coordinator before workers start. **The rule** for any
  new memo: if a worker can fill it and a hit skips observations its miss
  makes, the entry must carry a `Captured` replayed at every hit (or the
  hit must observe again).
* **Probe publications.** A worker's warm-reuse probe runs in a detached
  capture: its observations (pointer, manifest, recorded-input check, the
  tree's REPO claim) are stored with the publication
  (`DemandSlot::observations`, under the publication lock, dropped on a
  miss, an error or a lost race) and replayed by
  `DirectRuntime::mark_sequential` where the root begins to count for the
  sequential engine — transitively down the probes that published what it
  obtained. The sequential engine's evaluation of that root runs the same
  check.
* **The link policy** holds the sequential roots while recording: a
  worker-published root is a store tree, which anchors a link in any
  order, so no decision changes, but a crossing's REPO record is the
  threads-1 one. The canonical forms of the policy's roots, the store and
  the blocked prefixes are taken unrecorded and recorded once at the end
  of planning (`RepositorySet::record_link_anchors`: every counting root,
  the store, the blocked prefixes; before `settle_plan_taints`, and before
  a query's window closes) — the set is the same at every thread count.
  The store (`<cache>/repositories`) and so the cache root are created
  before the window opens (`observed_fs::create_with_window`, registered
  by the CLI; only a recording command creates them): a speculative
  fetch, a module-root warm-up or a registry prefetch may create them at
  any moment of the window, and their CANON records must not depend on
  whether one came before the end of planning
  (`BAZEL_RS_DEBUG_STORE_RACE=create` forces it,
  tests/snapshot_prefetch.rs).
* **One spelling per path**: `Path` equality ignores a trailing separator
  (`root.join("")` spells `root/`), so a recorder entry keeps the
  byte-wise smallest spelling seen, whatever order the observations come
  in.

The recorder is a set keyed by (path, op), first outcome wins, a second
one is Unstable, so neither the order nor the time of a merge matters —
only which events are merged; `racy` is computed against the window start,
so an earlier worker read of an unchanged file has the same outcome. A
file changed while the window is open gives Unstable, or a recording
consistent with what was analysed (never a false hit). A caught worker
panic breaks the stderr capture, so nothing is saved. Workers print
nothing; `PERF-PREFETCH` goes to the diagnostics stream (never part of the
captured window bytes a hit replays). Replay scopes for local repository
rules open on the coordinator only (debug-asserted), so their root/mapping
capture sees the sequential demands. The definition `.bzl` cache stays on
while it captures (W6R): a module the cache serves inside a replay scope
repeats the `root()`/`mapping()` lookups of its first evaluation
(`snapshot::replay::lookups`), so the pinned tables are the uncached
loader's (`tests/definition_bzl_cache.rs`; on ~/code the same recording
and 45/45 replays with and without it, −195 MB peak RSS).
`BAZEL_RS_SNAPSHOT_PREFETCH=0` keeps prefetch off while recording (a
diagnostics switch, not part of the snapshot key).

**Gates.** The record mode's dump prints `INPUTS-CANONICAL <blake3>`: the
digest of the INPUTS a snapshot of the recording would hold (the policy's
selection, the replay records and their pinned tables) with the window
start, the racy flags and the replay records' execution times zeroed
(worker jitter turns `on` off, so determinism tests compare record-mode
dumps). `tests/snapshot_prefetch.rs`: the digest and every comparable dump
line equal threads 1's at threads 2/8/32 and 8/40, 32/25, 8/7 with
`PERF-PREFETCH loaded>0`; eight targeted fixtures (a `.bzl` a speculative
unit evaluated first; an aborted unit's module hit by its inline re-run; a
worker-filled ignored memo hit by a `/...` toolchain pattern; a root probed
for a never-committed unit, and one reached only through its probe;
environment values a worker read first, directly and through a spec key;
a glob into a worker-published store tree; a never-committed unit's
cross-repository hop; an unused unit) each fail with the hook they need
switched off (`BAZEL_RS_DEBUG_W6R_OFF=unit,bzl,ignored,probe,env,spec,link,canon`,
a debug switch; unit tests switch hooks per thread); strict verify across
thread counts; no `PERF-PREFETCH` in a hit's replayed bytes. Unit tests:
the frames (commit-only, drop, conflicts, claim merge orders, once per
sink, detached and discard frames, inert outside a window), the probe set
publication, the probed chain `@a` → `@b` → `@c` with a worker's unit
(observations exactly at a demand, a commit, a `.bzl` hit, an ignored hit;
one kill switch per hook), the environment and spec-key replays, the link
policy. The spec-key replay is defence in depth today: every coordinator
path after a spec key re-reads the declared environment (the warm check's
watched environment, or the execution's), so it matters only with the
environment hook off.

Measured on ~/code (warm, update-mode recipe; ~/code HEAD moved from
14bd7e2e to 31494f32/45ed7432/916008a5 during the measurements, its
MODULE.bazel.lock unchanged), with the series both before and after the
definition cache item: oauth2token:all `--nobuild` (96,661 comparable dump
lines) and B2 `--nobuild` (159,181) record identically — `INPUTS-CANONICAL`
and every line — at threads 1/8/32 and 8/40, 32/25, 8/7 (prefetch loaded
867–923 and 2,201–2,390 units); verify-mode runs cycling threads 1/8/32,
saving at each (45 + 42 runs over both binaries), verified 33 + 30 hits
with no mismatch; the syscall audit at threads 1 is clean (26,790 window
touches with the definition cache, 31,400 without, 0 unrecorded; 2,730
replay touches, 0 unrecorded). A miss's wall: docs/ANALYSIS_SNAPSHOT.md,
"What is saved, and when".

## Measured on ~/code (L2-W4R, warm, update-mode recipe)

| | oauth2token:all `--nobuild` | oauth2token/... build | B2 `--nobuild` |
|---|---|---|---|
| eligible | unless-pruned | unless-pruned | unless-pruned |
| remaining taint | build-info ×8 (0 planned of 689) | build-info ×8 (0 of 691) | build-info ×12 (0 of 3,615) |
| replay dry run | 45/45 in every run (4 runs + 1 under strace) | 45/45 (2 runs) | 45/45 (3 runs + 1 under strace) |
| replay wall / CPU | 119–153 ms / 103–172 ms + 50–80 ms in `java` | 133–137 ms / 105–115 ms + 70 ms | 112–134 ms / 102–119 ms + 60–70 ms |
| validation [per-tree], 1 thread | 1.5–2.7 ms (613 obs + 220 entries) | 2.5–2.7 ms | 7.1–8.6 ms (2,206 obs + 596 entries) |
| syscall audit (window / replay) | 29,757 / 2,730 touches, 0 unrecorded | — | 139,268 / 2,727, 0 unrecorded |

With the per-tree policy also validating what a tree's links reach (review
follow-up, 5b91a73; oauth2token:all `--nobuild`, 7 dry runs on a machine at
load 25–32): 719 observations + 220 entries — the 106 more are the STAT,
CANON, LSTAT and READLINK records at or below the verified trees' 100
symlinks (toolchain wrapper links, `local_config_cc`'s links into
rules_cc) — in
2.1–7.7 ms on one thread (the previous binary, same conditions: 1.5–2.2 ms),
0 mismatches; no `link anchors:` line. The stricter syscall audit (a READ
record for every file opened for reading, no tree exemption below a tree's
links) is clean: 29,757 window touches including 38 files opened for reading,
2,730 replay touches, 0 unrecorded.

The critical path of a replay is `local_jdk`: its definition load (the
rules_java/rules_cc `.bzl` closure, 57–75 ms) plus its execution (`java`,
47–69 ms); the other 44 replay
in 0.2–1.6 ms each next to it (the 37 `_toolchain_config` repositories share
one 3–9 ms load). No local rule printed. Every build-info registration is
`@bazel_tools//tools/build_defs/build_info:cc_build_info`, pruned by the plan
(none of their outputs is a planned action): L2-W4 decides that taint on the
saved plan, after which these runs report `eligible=yes` (measured with the
L2-W4 binary on all three target sets: `plan check: build-info-callback
registered 8 (B2: 12) (... distinct), 0 in the plan`, `taints: none`; the
planned request encodes to 10.76 MB for oauth2token and 44.7 MB for B2,
docs/PLANNED_REQUEST.md).

## Behaviour changes of L2-W4R (outside record mode)

* A `.bzl`, source or glob symlink into a published store tree resolves even
  when the target repository was not materialized earlier in the run (it was
  an error before, depending on the order of demands).
* A local repository rule no longer reads its specification pointer before
  executing (it never reuses one): a corrupted pointer of a local repository
  is repaired by the execution instead of failing the command. Unchanged
  pointers of any repository are no longer rewritten.

Not changed: what repository rules print reaches stderr as it happens (also
collected per execution for the replay record, in record mode only).

## The store and the hit path (L2-W5)

`BAZEL_RS_SNAPSHOT=on` (the default since L2-W7) saves what this recording describes — the
observations a hit validates, the replay records and pinned tables (binary,
crate::snapshot::format), the window's stderr, the planned request — and
skips loading and analysis when every input is unchanged:
docs/ANALYSIS_SNAPSHOT.md. The replay of a hit uses the current command
line's resolver options and repository environment (not the recorded
set's), and any non-empty local-repository output still keeps a run from
being saved (v1 of the printing protocol). The record mode itself is
unchanged, except that its diagnostics go to the diagnostics stream
(crate::snapshot::diag) and a `--workspace_status_command` that writes to
stderr is reported as the taint `workspace-status-stderr`.
