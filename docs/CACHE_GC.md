# Garbage collection of the native cache

Code: `src/cache_gc.rs` (options, the cache lock, last-use recording, the
end-of-command trigger, `bazel-rs gc`), `src/cache_gc/collect.rs` (scan,
policy, removal, the size index). Hooks: `src/action.rs` (`read_manifest`),
`src/test_runner.rs`, `src/materializer.rs` and `src/repository_ctx.rs`
(download cache hits), `src/observed_fs.rs` and `src/repository_store.rs`
(repository trees used), `src/snapshot/hit.rs` (trees of a snapshot hit),
`src/main.rs` (install, finish, the `gc` command), `src/rc_options.rs`
(options). Tests: unit tests of both modules, `tests/cache_gc.rs` (CLI).

The native cache only grew: action entries, CAS blobs and their distinct
copies, repository trees, downloads, test results, the state of deleted
worktrees and the temporaries of killed processes stayed until the cache
directory was deleted (~/code's `bazel-rs-out` reached 30 GB; private
benchmark caches 60–125 GB).

## Genuine Bazel 9.3

* **Disk cache** (`--disk_cache`): `DiskCacheGarbageCollectorIdleTask` runs
  `DiskCacheGarbageCollector` once the server has been idle for
  `--experimental_disk_cache_gc_idle_delay` (default `5m`), when
  `--experimental_disk_cache_gc_max_size` (bytes, `K`/`M`/`G`/`T`) or
  `--experimental_disk_cache_gc_max_age` (a duration) is set — both default
  to `0`, off (RemoteOptions.java:409-447, RemoteModule.java:511-514). It
  lists every file, sorts by mtime (the cache touches entries it serves),
  deletes the oldest until the size is under the limit and everything older
  than the age, skipping a file whose mtime changed since the scan (a
  concurrent hit), under the exclusive non-blocking lock `gc/lock`
  (DiskCacheGarbageCollector.java:68-116, 184-214, 283-310).
* **Repo contents cache**: entries unused for
  `--repo_contents_cache_gc_max_age` (default `14d`; `0`: only duplicates)
  are removed after `--repo_contents_cache_gc_idle_delay` (`5m`) of
  idleness. Every command holds `gc_lock` shared; the collector takes it
  exclusively without waiting ("someone will come along later"), renames
  garbage into `_trash` and empties the trash after releasing the lock, so
  an interrupted collection is safe (LocalRepoContentsCache.java:62-78,
  187-275; BazelRepositoryModule.java:407, 414-419, 734). A hit touches the
  entry's recorded-inputs file (`CandidateRepo.touch`).
* The repository (download) cache and the output base are never collected.

## Native: one cache, one collector

The native cache is an output base, a disk cache, a repo contents cache and
a repository cache at once, shared by every worktree of a repository
(docs/WORKTREES.md), with no server. So:

* **The lock.** Every command takes a flock of the cache directory itself
  shared as soon as it knows its cache and holds it until it exits
  (`cache_gc::install`; a note is printed only if it waits more than a
  second for a collection). Locking the directory creates nothing: a
  command that fails before it writes leaves no cache (genuine's
  `gc/lock` would). A cache that does not exist yet holds nothing to
  protect and is not locked.
* **When.** At the end of a command (`cache_gc::finish`, after the
  progress display ended, also after a failure; not before `run` execs its
  target), when the last collection (the modification time of `gc/stamp`;
  for a cache never collected, the cache directory's birth time) is at least
  the idle delay old, or the last one left work: the command drops its shared lock, takes the
  exclusive one **without waiting** — only when no other bazel-rs process
  uses the cache — lowers its priority (`nice` +10) and collects in bounded
  time. The idle delay is `--experimental_disk_cache_gc_idle_delay` or
  `--repo_contents_cache_gc_idle_delay` (the smaller when both are given),
  **default 1 hour** (genuine: 5 minutes of server idleness; a command-end
  collection costs 0.5–1 s on a 26 GB cache, so not after every command).
  `BAZEL_RS_CACHE_GC=off` disables automatic collection.
* **`bazel-rs gc [--dry_run]`** (native-only command) collects now, waits
  for the processes using the cache (note printed), has no time bounds and
  prints what it found and freed per category:

  ```
  Garbage collection of /tmp/opencode/gc-measure/cache:
    actions      27832 of 28526 entries, 8.5 GiB of 8.7 GiB
    cas          0 of 184222 entries, 0 B of 6.7 GiB
    downloads    2960 of 2960 entries, 2.3 GiB of 2.3 GiB
    repositories 4109 of 4600 entries, 2.6 GiB of 5.6 GiB
    size budget  10.0 GiB
  Deleted 34901 of 220308 entries, reclaimed 13.5 GiB of 23.4 GiB in 21.62 s
  ```

  Its policy is the one a `build` there would apply: the garbage collection
  options of the rc files' `build`/`common` lines and of its command line
  (other rc options are ignored; other command-line options are an error).
  `clean` (another series) is left to genuine semantics; it may call
  `cache_gc::collect_now` for a `--gc`-like mode later — the name `gc` was
  chosen so the two never collide.

### Options and defaults

All five genuine options are accepted by every command (rc files included)
and parsed with genuine's converters and errors (`While parsing option
--experimental_disk_cache_gc_max_age=2x: Illegal duration '2x'.`).

| option | applies to | native default | genuine default |
|---|---|---|---|
| `--experimental_disk_cache_gc_max_size` | everything below | automatic budget | `0` (off) |
| `--experimental_disk_cache_gc_max_age` | action entries, test results, downloads, blobs | `60d` | `0` (off) |
| `--repo_contents_cache_gc_max_age` | repository trees | `14d` (as genuine) | `14d` |
| `--experimental_disk_cache_gc_idle_delay`, `--repo_contents_cache_gc_idle_delay` | the least time between automatic collections | `1h` | `5m` |

`0` switches a limit off, as in genuine. The **automatic budget** is 10 % of
what the cache's file system has available plus the cache's own size,
between **10 GiB and 64 GiB** — the snapshot store's formula
(docs/ANALYSIS_SNAPSHOT.md) with bounds for a whole cache: 10 GiB still holds
a full build of a large monorepo's common targets, 64 GiB keeps a big disk
from filling with years of history (the user's constraint: no endless
growth). On the development host (3.1 TB free) it is 64 GiB; a laptop with
200 GB free gets 20 GiB. Native enables limits by default (genuine leaves its
disk cache unbounded) because this cache is the only place native keeps
anything: without defaults it only grows.

The size budget is soft: it never removes what was used within the **recent
window** (a quarter of the shortest age limit, between a minute and a day:
one day by default), which is also the resolution of last-use times. A build
whose working set exceeds the budget keeps its working set.

### Last use

Recorded on hits, at most once per recent window per entry, never by a
write per file per run (`cache_gc::note_used`, `note_tree_used`, flushed at
the end of the command on up to 8 threads):

| entry | last use | recorded by |
|---|---|---|
| action entry `actions/<key>` | atime/mtime of `manifest.json` | every cache check reads the manifest (`read_manifest` has its stat: one comparison); the access time is set when older than the window (`utimensat`, mtime untouched; relatime does the same on a read) |
| test result `tests/<key>` | atime/mtime of `status.json` | a cached result |
| download `repositories/downloads/<alg>/<hex>` | atime/mtime | a cache hit (genuine `DownloadCache.get` touches it) |
| repository tree (direct, registry, Git, builtin) | mtime of `gc/used/<kind>-<identity>` (an empty stamp) and of its manifest | every materialization or reuse (`observed_fs::repository_tree`, links crossing into it, local trees, executions that kept an equal tree) and every tree an analysis-snapshot hit names or replays |
| CAS blob `cas/<sha256>-<mode>[.<n>]` | ctime (and atime/mtime) | staging hardlinks it, which changes its ctime |
| worktree state | atime of `gc/worktrees/<state name>` | every command of that workspace |

Tree stamps live outside the trees: a tree's root, manifest and directories
are integrity entries of the analysis snapshot (their stat keys include the
ctime), so touching them would make every snapshot verify the tree again.
The collector reads manifests and pointers with `O_NOATIME`.

### What goes, and what keeps it

* **Action entries** — by age, then by the budget (least recently used
  first). The entry is renamed into the trash: one rename, after which the
  key is a miss and the action re-executes. A hit never follows an entry
  that is gone (entries are published by rename and only ever removed).
* **CAS blobs** — a blob an entry, an output tree or a running action links
  (`st_nlink > 1`) is never removed (that frees nothing). An unlinked one (a
  staging copy of a source, a distinct-inode copy `.<n>`, the blob of a
  removed entry) goes by age and budget by its last use. The blobs whose
  last other link was a removed entry's file are found when the trash is
  emptied (`remove_in_place` notes files of two links) and judged by the
  last use the scan saw before; those freed by a trash an earlier collection
  left count as unused.
* **Repository trees** — by `--repo_contents_cache_gc_max_age` and the
  budget, as a unit: the specification pointers naming the tree
  (`direct/specs/<spec>`, `.ws-<hash>`), the ordinary slots linking it
  (`external/<name>`), the marker and mutable slot of a self-referencing
  repository made for it, the manifest, then the tree (renamed into the
  trash) and its stamp — the parts that make it trusted go first. **Kept
  trees keep the trees their bytes name** (the manifest's `refs`: a GOROOT
  in `go.env`, absolute links): the closure is computed before anything is
  removed. A direct tree that no pointer, slot or marker names (a superseded
  publication; genuine removes duplicates) goes once unused for the recent
  window. Readers validate what they find: a missing manifest or tree is
  re-executed (`try_reuse` → `Invalid`), a snapshot naming it misses
  (integrity lstat), a local repository replay verifies its tree.
* **Downloads** — by age and budget, with their canonical-id markers
  (`download_ids/<alg>/<hex>`). Genuine never collects its repository
  cache; native's is in the one cache, so it is bounded like the rest.
* **Test results** — by age and budget; a missing entry is a cache miss.
* **Always** (whatever the limits): temporaries unchanged for **24 hours**
  — execution roots `work/*`, `tests/work/*`, `repositories/modextwd/*`,
  every dot-named entry (`.tmp*`, `.stage-*`, `.trash-*`) of the stores
  (`actions/`, `cas/`, `tests/`, the tree, manifest, specification and slot
  directories, downloads, `gc/`) —; an entry without its manifest or status,
  a manifest without its tree, a pointer or slot link to no tree, a
  marker-less slot directory (each a day old: what a killed process or
  collection left); stamps of trees that are gone; and the **state of a
  deleted worktree**: `gc/worktrees/<state name>` records each workspace's
  canonical path (written by the first command of a workspace); when that
  path no longer exists and its parent does (a removed checkout), the state
  directory `worktrees/<name>-<hash>/` and the record go; when the parent is
  gone too (an unmounted disk?), after 60 days. The record is written at the
  end of a command whose state directory is non-empty. **Explicit output
  bases** (startup `--output_base`, docs/WORKTREES.md): one below
  `<cache>/worktrees/` is recorded under its own name and kept while its
  recorded workspace exists; one outside the cache is never scanned. A
  state directory without a record (written by older binaries, or an
  explicit base no command of this version used) is never removed: only a
  recorded workspace that is gone makes a state garbage.

Never removed: lock files (zero bytes: unlinking a lock another process has
open would let two processes hold "the" lock), the per-worktree state of
live worktrees (snapshots have their own budget; the hidden lockfile, the
test index, the output tree), the digest memo (bounded), the legacy
flat-layout directories (older binaries use them), `repositories/git/`
archives, `builtins/locks`.

### Safety

* **Concurrent processes.** A collection removes only under the exclusive
  lock: no bazel-rs process of this or a later version is using the cache.
  Earlier binaries take no such lock: before removing an entry the collector
  tries its own lock without waiting (`locks/<key>.lock`, test locks,
  publication locks `direct/locks/<id>`, `locks/<id>`, `git/locks/<id>`, slot
  writer locks) and keeps a busy one, and checks that its anchor (manifest,
  status, download) still has the inode and mtime it scanned (like genuine's
  mtime check). Do not run older binaries on a collected cache if you can
  avoid it.
* **Crash safety.** Every removal is one rename into `gc/trash/<pid>-<n>` or
  one unlink, ordered so that a killed collection leaves either a complete
  entry or one nothing trusts (above); the next collection empties the trash
  first and removes the orphans. `tests/cache_gc.rs` kills `gc` (SIGKILL via
  the test hook `BAZEL_RS_CACHE_GC_ABORT_AFTER=<n>`) at each of its first
  removal steps and builds after each.
* **Output tree.** `bazel-bin` links regular outputs by hardlink (they
  survive) and tree artifacts, symlink outputs and runfiles trees by symlink:
  after their entry is removed those links dangle until the next build of
  that target republishes the same path (the key is the same).
* **Running programs.** A program started by `run` (the lock is released at
  `exec`) or from `bazel-bin` uses entries the size budget will not take
  within the recent window; an age limit takes only what is that old.

### Size accounting and the index

An entry's size is the blocks of its files divided by their link counts, so
the entries of the cache add up to what it occupies on disk (an output and
its blob each count half; output-tree links of live worktrees are not
counted). Sizes of entries are computed once and kept in `gc/index`
(binary: `BZRSGCIX`, version, the first 8 bytes of the body's blake3, a
postcard list of (kind, name, anchor inode and mtime, size, refs); also
the identities pointers name), valid while the entry's anchor keeps its
inode and mtime. An automatic collection sizes new entries for at most
1 s (walks relative to their directories' descriptors); the size budget
waits until all are sized; the stamp records `pending`, and the next
command continues. It empties the trash for at most 1 s before and 1 s
after removing. `gc` prints the time of each phase; automatic collections
do under `BAZEL_RS_CACHE_GC_TIMING=1`.

### Costs (measured, release build, shared 64-core host)

Private cache built with `BAZEL_RS_CACHE_GC=off` on ~/code (f28b185):
`build //src/beyondwork.ai/tools/oauth2token/...`, `//src/ts/...`, then
`//src/beyondwork.ai/{prober,pvcautoscaler,decommission}/...`: 26 GB (`du`),
220,274 entries (28,509 actions, 184,205 blobs, 2,960 downloads, 4,600
trees), 23.4 GiB by the collector's accounting.

| | time |
|---|---|
| first collection (sizes every entry; `gc`) | 5.3–6.9 s |
| a collection with nothing to do (index warm; `gc`) | 0.53–0.89 s (scan 0.53–0.56 s) |
| the first automatic collections on that cache, no index (warm no-op `build oauth2token/...` with a snapshot hit, ~0.3 s alone) | +1.5 s, +1.5 s, +1.0 s (sizing done), then +0 until the idle delay passes |
| warm no-op `build oauth2token/...` (692 hits), automatic collection not due, vs `BAZEL_RS_CACHE_GC=off` | 0.289 vs 0.273 s (min of 4) |
| warm no-op `build //src/ts/...` (24,861 hits), same | 1.612 vs 1.624 s (min of 3) |
| the same TS no-op when every manifest's atime is 3 days old (24,848 touches) | 1.609 s |

**Default policy**: on the cache as built nothing is collectable (everything
used today); with every entry's last use set 20 days back
(`--dry_run`), the default policy removes the 4,600 repository trees
(14 days), 5.6 GiB, and keeps the rest (60 days, 64 GiB budget).
**Budget**: with every last use set 3 days back, then a warm
`build oauth2token/...` (692 hits), `gc --experimental_disk_cache_gc_max_size=10G`
removed 34,901 entries in 21.6 s (`du` 26 → 19 GB; the blobs of the removed
actions were linked the same day — ctime cannot be set back — so they wait
for their own last use to age), kept every oauth2token entry and tree: the
next oauth2token build had 692 hits, 0 executed; `//src/ts/...` re-executed
its 24,861 actions with the same outputs.

## Limits

* Earlier binaries ignore the lock (above).
* Last use has the resolution of the recent window (a day by default).
* A mutable slot (gazelle's `go_repository_cache` GOPATH) goes with the tree
  its marker names; another version of that repository that accepts the
  slot makes it again from its own tree.
* An automatic collection that removed much may leave trash for the next
  command (2 s of deletion per command); the blobs freed then count as
  unused.
* `gc` reports sizes by the collector's accounting, which `du` differs from
  by the uncounted parts (digest memo, snapshots, worktree state, output-tree
  links, lock files).
