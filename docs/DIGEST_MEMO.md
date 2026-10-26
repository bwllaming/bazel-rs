# Persistent file-digest memo (`<cache>/digests/v2.dmemo`)

Every bazel-rs command is a fresh process (there is no server), so the per-process digest memos of the
executor (docs/HARDLINK_STAGING.md, `action::digest_memo`) start empty on every command: a warm no-op build
used to re-read every action input and output once, and every warm analysis re-hashed every registry module
tree, the embedded `bazel_tools` tree and the files repository rules recorded. The digest memo keeps the
SHA-256 of a file by its path and stat key across commands — Bazel's `DigestUtils` cache
(`vfs/DigestUtils.java:36-60`: a digest is reused while the file keeps its node id, change time,
modification time and size), made persistent. Source: `src/digest_memo.rs`.

## File format

One file per cache, binary (never JSON), little-endian, read whole once per process (no mmap: the crate
forbids `unsafe`):

| bytes | header field |
|---|---|
| 0..8 | magic `BZRSDMEM` |
| 8..12 | format version (u32, 2) |
| 12..16 | record size (u32, 88) |
| 16..24 | record count (u64) |
| 24..32 | first 8 bytes of the blake3 hash of the body (u64) |

followed by `count` fixed-width 88-byte records, sorted strictly by (path hash, OWNED):

| offset | record field |
|---|---|
| 0 | path hash: first 8 bytes of blake3 of the path's bytes (u64) |
| 8 | device (u64) |
| 16 | inode (u64) |
| 24 | size (u64) |
| 32 | modification time, ns since the epoch (i64) |
| 40 | change slot, ns (i64): the ctime; for a blob or a published output its birth time (below) |
| 48 | SHA-256 (32 bytes) |
| 80 | epoch (u32): the save that wrote the record |
| 84 | flags (u32): 1 = OWNED, 2 = TRUSTED |

A lookup is a binary search directly on the bytes (no per-record decoding or allocation); the full stat key
is always compared, so a path-hash collision can only miss. A missing, truncated, corrupt (checksum,
count, order) or foreign (magic, version, record size) file reads as empty: it costs re-hashing, never a
wrong digest and never a failure. The header carries no binary identity on purpose: the payload (SHA-256
of file bytes by stat key) does not depend on the binary, and an identity would force a full re-hash after
every upgrade.

The format version (also in the file names, `v<version>.dmemo`, so that binaries of two versions keep
their own tables) must be bumped with any narrowing of the trusted areas or of what a record may vouch
for (a stronger key, a stricter rule for what is recorded or trusted): a record does not say which rule
admitted it, so the records an older binary wrote under the broader rule would otherwise stay trusted.
Version 2 (fix-digest-memo) is such a bump, and a necessary one: a version-1 table can hold wrong
digests. The executor's per-process memo answered by stat key alone, also for a new file that had
reused a deleted file's inode number inside the same timestamp tick, and recorded such an answer as
TRUSTED — for a file outside the cache (a source saved during a build) under the new file's own path
and change-time key, so that later commands kept reading the deleted file's digest for it (a scratch
run against 0363050: 1,862 of 2,000 such files recorded wrong, every one found by a fresh table); its
owned files also counted as settled by their modification time alone. `v1.dmemo` is ignored from
version 2 on (the first command re-hashes what it reads once) and can be deleted.
`Table::lookup` also re-checks the one rule a path shows: a birth-time (OWNED) key counts only for a blob
or a published output. That voids the records written before L2-W1 (995d70b) narrowed the recorded areas
from any `repositories/<kind>/trees/<identity>/` to the four tree stores, and before the tree stores' files
were keyed by change time (both while the format was unreleased; unit test
`birth_time_keys_count_only_for_blobs_and_published_outputs`).

## Keys

- Files outside the cache and the files of the repository tree stores
  (`repositories/{trees,direct/trees,git/trees,builtins/trees}/<identity>/...`): (device, inode, size,
  mtime, ctime), exactly `DigestUtils.CacheKey`. Any rewrite, `touch`, chmod or rename-over changes the
  key — also a same-size rewrite that restores the mtime (`touch -d`), through the ctime. Repository
  verification (registry trees, `bazel_tools`, recorded repository inputs; the same strength as
  `repository_store`'s Stat-mode fingerprints) and the executor use and share these records.
- The executor's own files, store blobs (`cas/<blob>`) and published outputs (`actions/<key>/...`; flag
  OWNED): the executor hardlinks blobs constantly and every `link()` moves the inode's ctime, so the
  ctime is left out, as in the per-process memo. The slot holds the statx birth time instead: `link()`
  leaves it alone and an inode number reused for a new file gets a new one, so equal sizes and
  modification times (tools that fix mtimes, archive extraction) cannot alias a deleted file — once the
  birth time is settled: a new file created inside the same timestamp tick gets the same one (the
  executor's outputs are racy when published; see "What is recorded" for what vouches for them). Where
  the filesystem reports no birth time such files are not persisted. A birth-time key of any other path
  is neither recorded nor answered (`Table::remember`, `Table::lookup`).

## What is recorded

- Only a file that was settled when it was read: its mtime and ctime (for blobs and published
  outputs mtime and birth time) at least 2 s older than the start of the read ("racy git", the margin
  of `digest_memo::RACY_MARGIN`, used by the executor's per-process memo too, and `repository_store`'s
  fingerprints) and unchanged after the read. A later
  in-place change, and a new file that reuses the inode number, then necessarily carry a different
  timestamp. A modification time alone settles nothing (`touch -d`, archive extraction and `cp -p`
  give new files old ones).
- Or (TRUSTED) what the executor knows of a path without having read it there, and only from
  knowledge that belongs to that path's file: another path of a state it read settled (a published
  output and its blob), and the content it established on that very path — a blob it validated (read
  it, or found its record) or created (from an output it read in exactly the linked state, or by a
  copy hashed while written; the blob's name is its content), and a published output linked to such a
  blob in the state the blob was established in. Never a stat key alone (a racy state of another
  file, or of a file since deleted: the executor's per-process memo answers for a racy state only at
  the path where the content was established, and forgets it whenever it removes or replaces that path,
  `action::DigestMemo`), and never a repository tree file: those only as read in a settled state. A
  published output is hashed right after the action's process exited, inside the racy window, so
  nothing of the action may write it afterwards: the action's supervisor kills and reaps every process
  the action left — also those that left its process group (`setsid`, a daemon) — before the executor
  hashes the outputs (docs/PROCESS_WRAPPER.md, as Bazel's process-wrapper does); only a process that
  escaped the supervisor itself could still change an output after its TRUSTED record was made. Its
  blob keeps the inode number from being reused, and this process drops
  its unsaved records of a path it removes (`Table::forget`: an invalid blob, an action entry that is
  published again).
- Only paths that recur across commands: files outside the cache root, and below it published action
  outputs (`actions/<key>/...`), store blobs (`cas/<blob>`) and the files of the repository tree stores
  (`repositories/{trees,direct/trees,git/trees,builtins/trees}/<identity>/...`); never execution roots
  (`work/`), the working areas of repository rules and module extensions (`modextwd/`, the
  repository directories `external/<name>`, the legacy `direct/stages/`), stages or temporary
  names (`.stage-*`, `.tmp-*`, `.trash-*`).

## When it is written

At the end of a command (`digest_memo::finish`, also before `run`'s exec and on failures), only when the
process hashed files whose digests the table did not hold yet. TRUSTED records ride along with such a save
but never cause one: the workspace status is published anew by every build, and a no-op build must write
nothing. A save takes the fs2 lock `digests/v2.lock`, re-reads the file, merges (per path and kind the newer
file state wins: the larger change slot, then mtime, then this save's record), drops the records of the
oldest epochs once there are more than 2^20 records (about 88 MiB) down to 7/8 of that, and replaces the
file through a temporary file and a rename. Readers never lock. Two processes saving at once lose no
records (unit test `concurrent_saves_lose_no_records`).

## Who consults it

| consumer | where | key |
|---|---|---|
| executor: action inputs, cache checks of published outputs, store blobs | `action::hash_file_from` (the funnel of `hash_file`, `read_regular[_from]`, `blob_valid`, `ensure_blob`): per-process memo → table → read | owned (blobs, published outputs) / external (everything else, repository tree files included) |
| registry module trees, warm check | `materializer::tree_matches` (walk as `snapshot`, digests via the table) | external |
| embedded `bazel_tools` tree, warm check | `builtin_tools::verified_warm` | external |
| repository-rule inputs: `watch`, `read` of watched files, and their warm revalidation (`repository_warm::unchanged`) | `repository_ctx::file_state` → the in-process `FileDigestMemo` (docs/REPOSITORY_WARM_PERF.md) → on a miss `digest_memo::opened_sha256` (the table by the opened file's fstat; otherwise the opened file is read, and recorded when settled and unchanged) | external |
| repository-rule inputs: `watch_tree` (and its warm revalidation) | `repository_ctx::file_sha256` → `digest_memo::path_sha256` → `digest_memo::opened_sha256` | external |

The walks themselves are unchanged (every entry is still lstat'ed and compared for type, mode, size and
symlink target); only reading unchanged regular files is skipped. Direct repositories keep their own
Stat-mode fingerprints in their manifests (`repository_store::verify_snapshot_at`).

The registry and `bazel_tools` checks take the key from the walk's lstat and record a digest only when the
opened file's fstat after the read still shows that key (`digest_memo::file_sha256`). Callers that start
from a path that may lead through symlinks go through `digest_memo::opened_sha256(&file, &metadata, path,
use)`: the caller opens the file, `metadata` is that file's own fstat, the table is consulted by that key
and on a miss the opened file itself is read — never the path again — so a key and a digest always belong
to one inode (a stat of the path followed by a read of the path could pair one file's key with another
file's bytes if a symlink on the path is swapped and swapped back in between; unit test
`digests_belong_to_the_opened_file`). `path_sha256` opens the path and hashes it that way; the caller's
own hasher runs only without a table, for a path that does not open as a regular file, or when the read
fails. `opened_sha256` is the entry point for any in-process digest memo in front of the table: the
i20-hardening `repository_ctx::FileDigestMemo` opens and fstats the file itself and hashes it through
`opened_sha256(.., Use::Recorded)` (its `opened_sha256_in` form, which also tells a table answer — counted
as a PERF-REPO memo hit — from a read) instead of reading it directly, so recorded-input file states keep
hitting (and filling) the persistent table (integration-20).

## Trust (with the executor's per-process memos)

The executor reads a repository tree file once per command and trusts it from then on (L2-W1,
docs/HARDLINK_STAGING.md, "Per-process memos"): the command verified the trees it materialized before
any action ran. With the table that verification no longer reads every file of a registry or
`bazel_tools` tree, but it stays exactly as strong as `repository_store`'s Stat mode, because it keys by
the change time — and so does the executor, for every repository tree file:

- **Next command: an edited tree file is caught.** A file of a tree store edited in place between
  commands, even with the same size and its modification time restored, has a new ctime. The next
  command's verification finds no record of that state, hashes the file, sees the mismatch and replaces
  the tree before any action runs: registry trees and `bazel_tools` are extracted afresh; a
  repository rule's tree fails its Stat fingerprint (ctime), so the rule runs again and the publication
  replaces the damaged tree; Git trees are hashed in full on every warm check anyway. The executor then
  reads new inodes, which its records never match, so it hashes them. CLI test
  `edited_repository_tree_files_are_caught_by_the_next_command` (both a repository rule's tree and
  `bazel_tools`; it fails if the verification's key leaves out the ctime).
- **Tree identities no command verifies any more are keyed by ctime, too.** A command verifies only the
  trees it materializes. A cached `actions.symlink` output keeps pointing into the tree identity it was
  created with (its action key holds the target's digest, not its path): after a change that gives the
  repository a new identity but leaves the file's bytes alone, its cache check reads the file of the
  old identity, which nothing verifies. With the executor's records keyed by birth time (L2-W2 before
  the review follow-ups) a same-size edit of that file that restored its mtime was answered from the
  table — 0 actions executed, and the symlink resolved to the edited bytes — while without the table
  the symlink action ran again. The executor therefore looks tree files up and records them by the
  change time, only from a read of a state that was settled by both times and that the read left
  unchanged; never as TRUSTED, and never from its per-process memo, whose entries leave the ctime out and
  can predate an edit made while the command runs. CLI test
  `symlinks_into_an_old_tree_identity_see_edits_that_restore_the_mtime` (fails with birth-time keys).
  The same holds for anything else that reaches tree files without verifying their tree (for instance
  analysis results reused without materializing the repositories again). The price: a tree file with a
  blob's mode (`0444`/`0555`, only possible for repository rules' trees) is hardlinked into `cas/` by
  staging, so its ctime moves whenever it is staged and the next command hashes it once (as
  `repository_store`'s fingerprint check already did).
- **Within one command: unchanged.** The executor's per-process memos and their trust are those of
  L2-W1: an edit made while a command runs, after the command looked at the file, is not re-detected by
  that command. The table only supplies a file's first digest in exactly the state it was recorded in:
  for a blob or a published output the same device, inode, size, modification time and birth time, for
  a repository tree file the same device, inode, size, modification time and change time. (Since
  fix-digest-memo the per-process digest memo answers only for a state it observed: a settled one on
  any path of the inode, a racy one only at the path where the content was established;
  docs/HARDLINK_STAGING.md, "Per-process memos".)
- **Blobs and published outputs** (`cas/`, `actions/`) have no second check: they are read-only, and an
  in-place edit that keeps their size, mtime and mode (after a `chmod`) is trusted by later commands
  too, where before the table only the rest of the command trusted it (as the per-process memo always
  did, docs/HARDLINK_STAGING.md, "Trade-offs and limits"). The files of a published tree artifact whose
  directories still show their stamps are not even inspected by a cache check (perf-cache-hits,
  docs/HARDLINK_STAGING.md, "Cache checks of published trees"): an in-place edit of one is not
  re-detected at all, while any change of the tree's structure is. `BAZEL_RS_DIGEST_MEMO=off` restores the
  re-hash per command while it is set, but repairs nothing (below): only deleting `<cache>/digests/`
  drops a record that trusts such an edit.

## Switches

- `--verify_repo_tampering` / `BAZEL_RS_VERIFY_REPOS=full`: repository verification (registry trees,
  `bazel_tools`, recorded inputs) re-hashes every file without consulting the table (the executor keeps
  using it, like its per-process memo).
- `BAZEL_RS_DIGEST_MEMO=off`: the table is neither read nor written (the per-process memos stay). It
  does not repair stale records: they stay in the file untouched, and once the variable is unset the
  table trusts them again.
- Deleting `<cache>/digests/` (or just `v2.dmemo`) drops every record: the next command re-hashes what
  it reads and writes a new table. This is the way to discard records, for instance after editing files
  below the cache by hand.
- `BAZEL_RS_TIMING=1` prints two `PERF-DMEMO` lines at the end: load outcome, records, bytes and load time;
  lookups (hits in the file, hits among this process's new records, misses), records remembered and what
  the save wrote; files and bytes actually hashed per consumer, with the time spent in registry and
  `bazel_tools` tree checks, and `repository_store`'s warm-check phases.

## Measurements (~/code 14bd7e2, lockfile c7a0c3df…, update mode, private caches; L2-W2)

Shared, loaded machine (overlayfs, 48-CPU quota): indicative, interleaved where it matters.

- Table on ~/code after `oauth2token/...` and B2: 24,780 records, 2,180,672 bytes; loaded by a fresh
  process in 1.6–2.7 ms. The first run that builds it saved 24,652 records in 8 ms; a no-op build writes
  nothing.
- `build //src/beyondwork.ai/tools/oauth2token/...` (689 cache hits), execution tail
  (`PERF execute.end − plan.end`, no action dump): base 6bebe72 3.68–4.65 s, after L2-W1 0.45–0.51 s, after
  L2-W2 0.25–0.32 s. A warm build hashes one file (the volatile status) instead of 9,250 action files
  (952 MB); `strace -c` of the tail (build − `--nobuild`): openat 72,329 (base) → 11,117 (W1) → 1,762,
  statx 1,189,977 → 45,624 → 36,819. Peak RSS 1,400–1,418 MB (base) → 1,381–1,385 MB.
- Repository checks per warm run (serial sums; B2 = `--nobuild` prober/pvcautoscaler/decommission):
  | | without the table (`BAZEL_RS_DIGEST_MEMO=off`) | with it |
  |---|---|---|
  | registry trees (34) | 15,012 files, 337.6 MB hashed, 2.68 s | 0 hashed, 0.49 s (lstat walk; 0.09 s for oauth2token's 32) |
  | `bazel_tools` | 421 files, 73.3 MB, 0.16 s | 0 hashed, 0.007 s |
  | recorded inputs (`recorded_check`, 352 repositories) | 884 files, 3.55 GB hashed, 2.47 s | 0 hashed, 0.010 s |
- Remaining tail (critical path, TEMP trace): scheduler validation ~70 ms (first main-thread allocations
  after planning pay the allocator debt the analysis left), the SDK `builder` action ~55 ms (5,975
  inputs: lstat-bound, overlayfs path lookups serialize), the `stdlib` actions ~25–35 ms, and a chain of
  ~36 small actions ~75 ms.
- Review follow-ups (repository tree files by ctime, `opened_sha256`, process-group kill; same caches,
  ~/code 14bd7e2): the first run after the change re-hashed the 7,354 repository-rule tree files the
  executor reads (200.9 MB, recorded by birth time before) once and recorded them by ctime. The table
  grew to 32,123 records / 2.83 MB: 7,343 of them are those files' old birth-time records, never
  answered again and kept until eviction (a fresh cache never has them; deleting `<cache>/digests/`
  drops them). Warm `oauth2token/...` build tail (no dump, interleaved ×3): base 2.91–3.25 s, before
  the follow-ups 0.282–0.309 s, after 0.259–0.299 s (24,361 hits, 1 miss, nothing saved; peak RSS
  1,383–1,386 MB). B2 `--nobuild`: registry trees 0 files hashed (0.091 s over 34 trees), `bazel_tools`
  0 (0.003 s), recorded inputs 0 files hashed with `recorded_check` 0.008 s over 352 (`path_sha256`
  now opens each recorded file once: no measurable change).

## fix-digest-memo (version 2): stale per-process answers (~/code 14bd7e2, lockfile c7a0c3df…)

### Upgrade note: action manifests `native-action-v9`

Binaries before fix-digest-memo — action manifests `native-action-v8` and earlier, among them the i20
release (5d62bd61) — could publish another action's output (the stale per-process digest described
below). Such an entry is self-consistent: its manifest names exactly the bytes it holds, so no cache
check can tell it from a good one, and it was served as a cache hit until the action's key changed. The
action manifests therefore move to `native-action-v9` (`action::MANIFEST_FORMAT`); the action keys stay
`native-action-v8`, so key-pinned tests and the directory names `actions/<key>/` are unchanged.

- **One-time re-execution.** The first build with this binary on an existing cache executes every action
  it needs once more: an entry of another manifest format is a cache miss, never an error, and is replaced
  by a v9 entry (unit test `entries_of_earlier_manifest_formats_are_executed_again`). Repositories are not
  fetched or executed again: their stores have formats of their own, untouched by this change. Together
  with the digest table's version 2 (each file the build reads is hashed once more) the measured cost is
  below ("Upgrade cost").
- **Old entries stay on disk.** Entries of actions that are not built again stay in `<cache>/actions/`,
  never trusted, until they are deleted: there is no action-cache garbage collection; deleting
  `<cache>/actions/` drops them (with every other entry). `<cache>/digests/v1.dmemo` can be deleted too.
- **Do not alternate binaries on one cache.** An older binary treats the v9 entries as misses in turn, so
  two binaries of different manifest formats sharing a cache re-execute each other's actions.
- **Remote caches.** The remote cache keys are Bazel's (REAPI action digests), not this format: an entry
  that an affected binary uploaded is still found and its outputs are downloaded and published
  (`--remote_accept_cached`). Native uploads needed both `--remote_upload_local_results` (Bazel's default)
  and `BAZEL_RS_ALLOW_REMOTE_UPLOAD=1` (docs/REMOTE_CACHE.md). If an affected binary ran with both, purge
  the Action Cache entries that bazel-rs uploaded, or the remote cache's whole Action Cache: the wrong part
  is the mapping from an action to its outputs (the CAS blobs are content-addressed and correct). Native
  ActionResults carry no `execution_metadata.worker`, only its four timestamps.

- Repro (4,000 write actions, 8-byte outputs alternating one shared and unique contents, fresh cache per
  build): the 0363050 release published an old blob as a new action's output in 10 of 40 builds (1–5
  outputs each; a second, warm build served the same wrong outputs from the action cache); after the fix
  0 of 60. Unit test `a_new_file_never_inherits_a_deleted_files_digest`: 2,800–3,150 stale digests at
  0363050 (inode numbers reused 1,690–1,800 times of 1,800), 0 after; `outputs_of_one_size_are_published_with_their_own_content`
  150–170 of 600 wrong outputs, 0 after.
- Persistence: a scratch run against 0363050 (2,000 files outside the cache, each created in the tick an
  execution-root output was deleted) recorded 1,862 wrong TRUSTED records, every one answered by a fresh
  table; unit test `stale_states_never_reach_the_persistent_table` (no record of a racy file at all).
- Cost (private caches, release builds, interleaved; shared machine): warm `oauth2token/...` build tail
  (no dump) 0.164–0.215 s (median 0.180) before, 0.150–0.263 s (median 0.182) after, peak RSS 1,321–1,326
  MB both, 1 file hashed both. Cold tail (689 actions executed on the warm repositories, no table) 44.4 /
  36.2 s before, 35.3 / 33.3 s after, hashing the same 9.3k files / 956.5 MB; the next warm build reads
  only the volatile status on both. A cold build records 33,090 records (32,656 before: blobs validated
  in a racy state now ride along as TRUSTED).
- Upgrading: a cache whose table is `v1.dmemo` (cache-a above, built by 5d62bd61) starts from an empty
  `v2.dmemo`. The first warm `oauth2token/...` build after the upgrade read 9,245 action files (955 MB),
  14,886 registry files (337 MB), 421 `bazel_tools` files and 10 recorded inputs once (plan.end 9.5 s
  instead of 6.2 s, tail 0.313 s) and saved 24,617 records; the next one read 1 file (tail 0.186 s).
