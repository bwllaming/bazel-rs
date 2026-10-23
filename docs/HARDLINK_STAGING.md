# Action input staging and output publication without copies (N36)

Native actions run in a fresh per-action execution root
(`<cache>/work/<key>-<pid>-<n>/execroot`, `src/action.rs`). Until N36 every
input was read into memory and written again as a copy into that root, every
output was read and written again into the completed cache entry, and
`actions.symlink` outputs were copies. Large shared inputs (SDKs, toolchains,
`node_modules`) were copied once per action, and one source reachable at two
input paths (rules_cc `_virtual_includes`, `actions.symlink`) became two
inodes, unlike Bazel, where both resolve to one file (clang's `#pragma once`
keys on file identity).

## Design

**Content-addressed store.** `<cache>/cas/<sha256>-<mode>` holds one inode per
content and executable bits. Blob modes are read-only: `0444` plus the keyed
executable bits of the input (`0555` for generated outputs, which Bazel makes
`0555`, `OutputPermissions.READONLY`). A blob is valid when it is a regular
file with that mode, the expected size and the digest in its name; an invalid
blob is removed and recreated.

**Input staging = hardlinks.** A process action's input files are hardlinked
from their blobs into the execution root; tree inputs become real (writable)
directories of hardlinked files. This is Bazel's hermetic linux-sandbox
(`--experimental_use_hermetic_linux_sandbox`,
`sandbox/LinuxSandboxedSpawnRunner.java:351-366` selects
`HardlinkedSandboxedSpawn`), which hardlinks inputs and falls back to a copy
when the hardlink fails (`sandbox/HardlinkedSandboxedSpawn.java:75-102`;
`AbstractContainerizingSandboxedSpawn.createInputs`, `:157-180`). Bazel's
default sandboxes stage symlinks instead
(`sandbox/SymlinkedSandboxedSpawn.java:128-129`), and local execution runs in
the execroot symlink forest (`buildtool/SymlinkForest.java:145-161`). Native
chooses hardlinks because:

* they keep native's snapshot guarantee: the action sees exactly the keyed
  bytes even if a source changes during the build (a symlink to the source
  tree would expose the live file);
* the store's one inode per content gives one identity for a file reachable
  at several input paths (`stat -L` equality, as Bazel's symlinks give);
* no input byte is copied after the first time its content is seen.

Bytes from outside the cache (sources, external repositories) are copied into
the store **once** (a streamed, digest-verified copy; the user's files are
never linked, chmod'ed or otherwise touched). Files the cache already owns
with the blob's mode (published outputs) are linked into the store, not
copied. Native writes and transforms (symlink, template) spawn no process and
stage nothing.

**Read-only inputs and verification.** Because staged inputs share the
store's inode, a process writing an input in place would corrupt the store.
Blobs are therefore read-only (an ordinary write fails with `EACCES`), and
after every process the executor re-checks each staged inode's size, mtime
and mode (through its execroot path, or the store path when the process
replaced the execroot entry). A change fails the action like
`LinuxSandboxedSpawnRunner.checkForConcurrentModifications`
(`:478-521`; message `input dependency <path> was modified during
execution.`) and the damaged blob is removed before any other action can
stage it. Ctime is not compared (Bazel's `FileContentsProxy` does): hardlinking
a blob updates its ctime, so concurrent staging would be mistaken for writes.

**Output publication = rename/hardlink.** A completed output file is
normalized to `0555` in the execution root, hashed, linked into the store and
the store inode is linked into `completed/outputs/<name>`; identical outputs
therefore share one inode. When a hardlink fails the file is renamed into place
(Bazel moves sandbox outputs by rename, falling back to a copy,
`sandbox/SandboxHelpers.java:178-236`), and only then copied. Tree outputs are
renamed as a whole, each file is replaced by its store inode, then directories
become `0555` deepest first. The completed directory is renamed into
`actions/<key>` as before.

**`actions.symlink` outputs are real symlinks.** Like
`SymlinkAction.execute` (`analysis/actions/SymlinkAction.java:218-238`:
`outputPath.createSymbolicLink(targetPath)` with the input's absolute path,
after deleting the pre-created tree directory, `:232-236`), the published
output is a symlink to the target's absolute input path: the workspace source
file, or the producer's published output (a symlink of a symlink is a chain).
`lstat` sees a symlink; `stat` sees the target's bytes, mode and inode. Tree
targets give a symlink to the target tree. When a symlink output is an input,
the executor follows such published links (only symlinks below
`<cache>/actions/`; any other symlink input is still rejected) and stages the
target's blob, so the header and its alias are one inode inside the
consumer. A cache hit requires the symlink to still name the recorded target
with the recorded digest and mode.

**Digests.** Inputs are hashed by streaming (the previous executor held every
input in memory). A process-wide memo keyed like Bazel's
`DigestUtils.CacheKey` (`vfs/DigestUtils.java:36-60`: device, inode, size,
mtime, ctime) avoids re-hashing a shared input for every action. Files outside
the cache are memoized only when their timestamps are at least 2 s older than
the start of hashing ("racy git"), so a same-size rewrite within the
filesystem's timestamp granularity is never missed; files the cache owns are
keyed without ctime and memoized when written. Since L2-W2 the digests also
persist across commands, there keyed with the ctime of repository tree files
(docs/DIGEST_MEMO.md).

## Per-process memos (L2-W1)

Within one command (one process) the executor inspects what the cache owns
once, like Bazel's per-build file metadata, instead of once per action:

* a published tree artifact (`actions/<key>/outputs/...`) is read once and
  shared (`Arc<Tree>`) by every consumer; its producer seeds the entry when its
  cache entry validates or when it publishes the tree, and drops or replaces
  it whenever it publishes again;
* a file of the content-addressed stores is read once, by path: the blobs
  `cas/<blob>` and the files of the repository tree stores
  `repositories/{trees,direct/trees,git/trees,builtins/trees}/<identity>/...`
  (registry modules, repository-rule results, Git sources, the embedded
  `bazel_tools`). Nothing else below `repositories/`: module extensions and
  repository rules run arbitrary code in `modextwd/` and `direct/stages/`,
  and downloads, locks and manifests are no trees;
* the directories of `actions/`, `cas/` and those tree stores are lstat'ed
  once to establish that they are real directories;
* many inputs (an SDK) or tree files are hashed on helper threads taken from
  one process-wide budget of as many threads as cores; a call that finds it
  exhausted hashes on its own thread, so execution never runs more than
  `--jobs` workers plus that many helpers (and the process's few other
  threads).

The memos are bounded (256 Ki entries each, a few hundred MB at most in all;
a memo that would exceed its cap starts over).

**Trust.** These areas are written only by bazel-rs: published outputs and
blobs are read-only and replaced only by republication, and repository
trees are only ever replaced as a whole; the trees a command materializes
are verified before any action runs. Not every tree file an action reads is
in such a tree: a cached `actions.symlink` output can point into an older
identity of a repository that no command materializes or verifies any more.
An edit under `<cache>/{actions,cas,repositories/<tree stores>}` made while
a command runs, after its memos took their first look, is not re-detected by
that command; the next command starts with empty per-process memos,
verifies the trees it materializes again, and takes a repository tree
file's digest from the persistent digest memo only for the file's exact
change time, so it also re-reads an edited file of an old identity
(docs/DIGEST_MEMO.md, "Trust"). Staging still verifies
what it links: an existing blob by mode, size and digest, a new blob by a
digest-checked copy (a file the cache owns that already has the blob's mode
is hardlinked instead and its digest checked after linking), both through
the stat-keyed digest memo; a copy whose bytes no longer match fails the
action (`input ... changed while it was being staged`), and every staged
inode is re-checked after its process ran. Execution roots (`work/`), the
working areas of repository rules and module extensions, and every path
outside the cache keep the full checks on every use.

## Cache keys

The action key is unchanged (`native-action-v5`, including the symlink
action's framing of the target's mode); `tests/configured_target_keys.rs`
pins pass unchanged. Manifests record symlink outputs and are
`native-action-v7`; pre-v7 entries (v5 from main, v6) rebuild once. (N42 later
moved the key identity to `native-action-v6` for exact action environments,
docs/ACTION_ENV.md; the N41 x N42 merge moved key and manifests together to
`native-action-v8`, docs/REMOTE_CACHE.md: pre-v8 entries rebuild once. The
manifest schema is unchanged. N41's compiled-in genrule-setup.sh is stored as
an executable `cas/` blob and staged like every input; remote cache hits are
written into the execution root and published through this store.)

## Trade-offs and limits

* Staged inputs are `0444`/`0555`, not the source's mode: a source `0644`
  header reads as `0444` inside the action (Bazel: `0644`, writable under
  local execution and processwrapper-sandbox, read-only mount under
  linux-sandbox). Executable bits are unchanged and keyed. Actions that write
  their inputs in place fail (Bazel's sandboxes also forbid or discard this);
  so do metadata changes of a staged input (`touch`, `chmod`), which Bazel's
  hermetic sandbox also reports, while its symlinking sandboxes pass them on
  to the source file.
* Identical contents share an inode: two different source files with equal
  bytes and executable bits are one identity inside the action (Bazel: two
  inodes). For `#pragma once` this merges only byte-identical headers.
* Published outputs share inodes with the store and with each other; editing
  one in place (after `chmod u+w`) edits all and is detected (digest
  mismatch) on next use. A process that forges size, mtime and mode after an
  in-place write is not detected (execution is sandboxed by default since
  N59, but the Landlock sandbox leaves metadata operations unrestricted and
  denies nothing for `standalone`/`no-sandbox` actions or a store below a
  sandbox-writable root such as `/tmp`; docs/SANDBOX.md).
* Once the action's process exits, whatever is left of its process group is
  killed before its staged inputs are re-checked and its outputs hashed
  (`kill_leftovers`; Bazel's process-wrapper also kills the group after the
  wait, `process-wrapper-legacy.cc` `WaitForChild`, and its linux-sandbox ends
  a PID namespace), so a background process the action started cannot change
  a published output whose digest this command and, through the persistent
  digest memo, later commands trust. A process that left the group (`setsid`,
  a daemon) and keeps writing an output can still change the published inode
  (previously the published copy was taken after exit).
* Inside the action every input is a regular file; Bazel's sandboxes show
  symlinks. `readlink` of a published symlink output names a native path
  (workspace source or cache entry), like Bazel's absolute execroot path.
* The store persists one copy of each distinct input/output content; there is
  no store garbage collection (native has no cache GC) and no sweep of
  `.tmp-*` files a killed process left (docs/ROADMAP.md, milestone 3).
* Cross-device or link-limited filesystems (`EXDEV`, `EMLINK`, ...) fall back
  to copies for staging and to rename/copy for publication (unit tests
  `failed_hardlinks_fall_back_to_copies`, `emlink_hardlink_failures_fall_back_to_copies`).
* A blob removed between its validity check and the link (a concurrent
  damaged-blob removal) is ensured once more and linked again.
* The digest memo trusts a cache-owned file (blob, published output) by its
  mtime alone, since every link() to it bumps its ctime (keyed by inode,
  size, mtime and birth time). Since L2-W2 such digests also persist across
  commands; repository tree files persist only with their ctime, because a
  command verifies only the trees it materializes (docs/DIGEST_MEMO.md,
  "Trust").

## Benchmark

`tests/staging_bench.rs` (ignored; `cargo test --release --test
staging_bench -- --ignored --nocapture`): 2000 files x 256 KiB (500 MiB)
shared by 50 actions, `--jobs` 8, through `scheduler::execute_graph`; the same
file run against 6ae0cc8 is the baseline (release builds, overlay filesystem,
64 cores).

| | cold wall | warm (all hits) | peak cache disk | final cache disk |
|---|---|---|---|---|
| before (copy) | 20.97 s | 20.02 s | 3.99 GB | 0.8 MB |
| after (hardlink) | 3.29-3.78 s | 0.90-0.98 s | 0.72-0.80 GB* | 0.51 GB (one store copy) |

\* `du` samples include transient block preallocation of freshly written
blobs on this overlay filesystem; the settled size is the final column.

End to end through the CLI (`artifacts/staging-bench.py`, 50 genrules over a
2000-file glob, `--jobs=8`, release binaries) a cold build took 109.8 s before
and 71.9 s after, a warm one (all hits) 104.6 s and 69.3 s; about 65 s of the
after runs is single-threaded analysis/planning before the first action
(outside staging; N44 reduced the whole run to 4.9 s cold and 2.3 s warm,
docs/LOADING_VALIDATION.md). Peak RSS fell from 4.19 GB to 116 MB (inputs
are no longer held in memory) and peak cache disk from 3.59 GB to 0.69 GB
(0.51 GB settled). `validate_structure`'s overlap check, run three times per
action, is now O(n log n) (adjacent pairs in component order), with the
previous quadratic scan kept only to name the first overlap in declaration order.

## Module-extension I/O native capture

The native diagnostic capture of `scripts/validate_module_extension_io.py`
(`tests/fixtures/module_extension_io/native_capture.py`) snapshots each case's
whole `--cache-dir`. Before N36 it required every cache file to be single-link;
with N36 the consumer's published output and its `cas/<sha256>-555` blob are
one inode. The snapshot now accepts a hardlinked file only when all `st_nlink`
names of that `(st_dev, st_ino)` were walked inside the case's cache root (a
name outside the cache, or in another case's cache, is still a blocker;
symlinks and special files are still rejected) and records each group as
`links`. A recorded group must be one `cas/` blob plus published action outputs
with identical bytes and mode. `cas/` is a publication root: every blob must be
named by its own digest and read-only mode (`0444` plus executable bits), and a
negative case may leave none. Action manifests are checked as
`native-action-v8` (v7 before the N41 x N42 merge) regular-file outputs
(`mode` `0555`, no `tree`/`symlink`).
Retained copies under `native-cache/` are independent single-link files.

## Paired capture

`artifacts/staging-capture.py` builds the fixture of
`tests/hardlink_staging.rs` with genuine Bazel 9.3.0 (local strategy, default
strategy, hermetic linux-sandbox when available) and with the native CLI
before and after N36; the receipt is
`tests/fixtures/hardlink_staging/reference_receipt.json`.

Capture results: 34 EXACT, 7 MISMATCH, 1 UNAVAILABLE (the hermetic
linux-sandbox: this host has no user namespaces, so Bazel registers no
`linux-sandbox` strategy; the default strategy was `processwrapper-sandbox`).
Against the default strategy native mismatched 10 of 20 observations before
N36 and 4 after; against `--spawn_strategy=local` 9 before and 3 after.
Changed by N36 and now equal to Bazel: `stat -L` inode equality of the source
header and its `_virtual_includes` alias (`different` -> `same`), of the
generated header and its alias (`different` -> `same`), the generated input's
mode (`755` -> `555`) and writability (`yes` -> `no`), the published aliases'
`lstat` kind (`file` -> `symlink`) and their followed-inode equality with the
target (`false` -> `true`). The remaining mismatches, all documented above
(reference string first, native second):

| reference | observation | Bazel | native |
|---|---|---|---|
| default, local | `source_mode` (inside the action) | `644` | `444` |
| default, local | `source_writable` | `yes` | `no` |
| default, local | `virtual_lstat` | `symbolic link` | `regular file` |
| default | `source_lstat` | `symbolic link` | `regular file` |
