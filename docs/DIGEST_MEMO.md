# Persistent file-digest memo (`<cache>/digests/v1.dmemo`)

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
| 8..12 | format version (u32, 1) |
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
  modification times (tools that fix mtimes, archive extraction) can never alias a deleted file. Where
  the filesystem reports no birth time such files are not persisted. A birth-time key of any other path
  is neither recorded nor answered (`Table::remember`, `Table::lookup`).

## What is recorded

- Only a file that was settled when it was read: its mtime and ctime (mtime only for blobs and
  published outputs) at least 2 s older than the start of the read ("racy git", the margin of
  `action::RACY_MARGIN` and `repository_store`'s fingerprints) and unchanged after the read. A later
  in-place change then necessarily carries a different timestamp.
- Or a file the executor wrote itself and made read-only (TRUSTED: a published output), and other paths
  of an inode the process hashed (a published output and its blob). Never a repository tree file: those
  only as read in a settled state.
- Only paths that recur across commands: files outside the cache root, and below it published action
  outputs (`actions/<key>/...`), store blobs (`cas/<blob>`) and the files of the repository tree stores
  (`repositories/{trees,direct/trees,git/trees,builtins/trees}/<identity>/...`); never execution roots
  (`work/`), the working areas of repository rules and module extensions (`modextwd/`,
  `direct/stages/`), stages or temporary names (`.stage-*`, `.tmp-*`, `.trash-*`).

## When it is written

At the end of a command (`digest_memo::finish`, also before `run`'s exec and on failures), only when the
process hashed files whose digests the table did not hold yet. TRUSTED records ride along with such a save
but never cause one: the workspace status is published anew by every build, and a no-op build must write
nothing. A save takes the fs2 lock `digests/v1.lock`, re-reads the file, merges (per path and kind the newer
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
| repository-rule inputs: `watch`, `read` of watched files, and their warm revalidation (`repository_warm::unchanged`) | `repository_ctx::file_state` → the in-process `FileDigestMemo` (docs/REPOSITORY_WARM_PERF.md) → on a miss the table (lookup by the opened file's fstat; a settled read that kept its stat is recorded) | external |
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
i20-hardening branch's `repository_ctx::FileDigestMemo::sha256` (not on this branch) opens and fstats the
file itself and must hash it through `opened_sha256(.., Use::Recorded)` instead of reading it directly,
or recorded-input file states stop hitting (and filling) the persistent table after the merge.

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
  a repository tree file the same device, inode, size, modification time and change time.
- **Blobs and published outputs** (`cas/`, `actions/`) have no second check: they are read-only, and an
  in-place edit that keeps their size, mtime and mode (after a `chmod`) is trusted by later commands
  too, where before the table only the rest of the command trusted it (as the per-process memo always
  did, docs/HARDLINK_STAGING.md, "Trade-offs and limits"). `BAZEL_RS_DIGEST_MEMO=off` restores the
  re-hash per command while it is set, but repairs nothing (below): only deleting `<cache>/digests/`
  drops a record that trusts such an edit.

## Switches

- `--verify_repo_tampering` / `BAZEL_RS_VERIFY_REPOS=full`: repository verification (registry trees,
  `bazel_tools`, recorded inputs) re-hashes every file without consulting the table (the executor keeps
  using it, like its per-process memo).
- `BAZEL_RS_DIGEST_MEMO=off`: the table is neither read nor written (the per-process memos stay). It
  does not repair stale records: they stay in the file untouched, and once the variable is unset the
  table trusts them again.
- Deleting `<cache>/digests/` (or just `v1.dmemo`) drops every record: the next command re-hashes what
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
