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
| 40 | change slot, ns (i64): the ctime; for a file the cache owns its birth time (below) |
| 48 | SHA-256 (32 bytes) |
| 80 | epoch (u32): the save that wrote the record |
| 84 | flags (u32): 1 = OWNED, 2 = TRUSTED |

A lookup is a binary search directly on the bytes (no per-record decoding or allocation); the full stat key
is always compared, so a path-hash collision can only miss. A missing, truncated, corrupt (checksum,
count, order) or foreign (magic, version, record size) file reads as empty: it costs re-hashing, never a
wrong digest and never a failure. The header carries no binary identity on purpose: the payload (SHA-256
of file bytes by stat key) does not depend on the binary, and an identity would force a full re-hash after
every upgrade.

## Keys

- Files outside the cache: (device, inode, size, mtime, ctime), exactly `DigestUtils.CacheKey`. Any
  rewrite, `touch`, chmod or rename-over changes the key — also a same-size rewrite that restores the
  mtime (`touch -d`), through the ctime.
- Files the cache owns (store blobs, published outputs, repository trees: everything below the cache root):
  the executor hardlinks blobs constantly and every `link()` moves the inode's ctime, so the ctime is left
  out, as in the per-process memo. The slot holds the statx birth time instead: `link()` leaves it alone
  and an inode number reused for a new file gets a new one, so equal sizes and modification times (tools
  that fix mtimes, archive extraction) can never alias a deleted file. Where the filesystem reports no
  birth time such files are not persisted.
- Repository verification (registry trees, `bazel_tools`, recorded repository inputs) always uses the
  external key (with ctime), the same strength as `repository_store`'s Stat-mode fingerprints.

## What is recorded

- Only a file that was settled when it was read: its mtime and ctime (mtime only for cache-owned files)
  at least 2 s older than the start of the read ("racy git", the margin of `action::RACY_MARGIN` and
  `repository_store`'s fingerprints) and unchanged after the read. A later in-place change then
  necessarily carries a different timestamp.
- Or a file the executor wrote itself and made read-only (TRUSTED: a published output), and other paths
  of an inode the process hashed (a published output and its blob).
- Only paths that recur across commands: files outside the cache root, and below it published action
  outputs (`actions/<key>/...`), store blobs (`cas/<blob>`) and repository tree files
  (`repositories/[<kind>/]trees/<identity>/...`); never execution roots (`work/`), stages or temporary
  names (`.stage-*`, `.tmp-*`, `.trash-*`).

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
| executor: action inputs, cache checks of published outputs, store blobs | `action::hash_file_from` (the funnel of `hash_file`, `read_regular[_from]`, `blob_valid`, `ensure_blob`): per-process memo → table → read | owned / external |
| registry module trees, warm check | `materializer::tree_matches` (walk as `snapshot`, digests via the table) | external |
| embedded `bazel_tools` tree, warm check | `builtin_tools::verified_warm` | external |
| repository-rule inputs: `watch`, `read` of watched files, `watch_tree`, and their warm revalidation (`repository_warm::unchanged`) | `repository_ctx::file_sha256` → `digest_memo::path_sha256` | external |

The walks themselves are unchanged (every entry is still lstat'ed and compared for type, mode, size and
symlink target); only reading unchanged regular files is skipped. Direct repositories keep their own
Stat-mode fingerprints in their manifests (`repository_store::verify_snapshot_at`).

## Switches

- `--verify_repo_tampering` / `BAZEL_RS_VERIFY_REPOS=full`: repository verification (registry trees,
  `bazel_tools`, recorded inputs) re-hashes every file without consulting the table (the executor keeps
  using it, like its per-process memo).
- `BAZEL_RS_DIGEST_MEMO=off`: the table is neither read nor written (the per-process memos stay).
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
