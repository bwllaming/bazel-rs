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
* the store's inodes give one identity for a file reachable at several
  input paths (`stat -L` equality, as Bazel's symlinks give), and distinct
  artifacts with one content distinct identities ("Distinct artifacts,
  distinct inodes" below);
* no input byte is copied after the first time its content is seen.

Bytes from outside the cache (sources, external repositories) are copied into
the store **once** (a streamed, digest-verified copy; the user's files are
never linked, chmod'ed or otherwise touched). Files the cache already owns
with the blob's mode (published outputs) are linked into the store, not
copied — but never a file of the repository store (`repositories/`), even a
read-only one with the blob's mode (rules_python makes its interpreter's
`lib/` 0444): the direct repository store keeps one link per tree file and
rejects a hard-linked one when it verifies the tree, so such a link made the
repository re-execute on the next command and be republished over its
read-only tree (test
`read_only_repository_files_are_copied_into_the_store_never_linked`). Native
writes and transforms (symlink, template) spawn no process and stage nothing.

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
mtime, ctime) avoids re-hashing a shared input for every action. A state read
is remembered only when it was settled: its timestamps at least 2 s older
than the start of hashing ("racy git"), so neither a same-size rewrite nor a
new file that reuses the inode number can show it again. Files the cache owns
are keyed without ctime (every link() moves it) and with their birth time
instead, which must be settled as well; what the executor itself produces
(blobs, published outputs) it remembers in a racy state only by path
("Per-process memos" below). Since L2-W2 the digests also persist across
commands, there keyed with the ctime of repository tree files
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
  repository rules run arbitrary code in `modextwd/` and the repository
  directories `external/<name>` (a link to a tree, or mutable), and
  downloads, locks and manifests are no trees;
* the directories of `actions/`, `cas/` and those tree stores are lstat'ed
  once to establish that they are real directories;
* many inputs (an SDK) or tree files are hashed on helper threads taken from
  one process-wide budget of as many threads as cores; a call that finds it
  exhausted hashes on its own thread, so execution never runs more than
  `--jobs` workers plus that many helpers (and the process's few other
  threads).

Since perf-cache-hits (a warm no-op build of ~/code's `//src/ts/...`
checks 24,608 cached actions; see "Cache checks of published trees" below
for the measurements):

* every other published output — a file, an `actions.symlink` output and
  what it resolves to when that is a published output or a file of the
  immutable stores, an unresolved symlink's link text — is read once per
  process too: its producer's valid cache entry seeds the **output memo**
  (`action::OutputMemo`) with exactly what a consumer's read would return,
  and drops it before it publishes the entry again (`forget_outputs`, also
  for the symlinks of other entries that resolve into it). Consumers used
  to inspect each such input again (a TypeScript build: ~300k inputs, most
  of them `node_modules` links and published files);
* the directories of paths outside the cache (the workspace's sources,
  local repositories) are lstat'ed once per process to establish that they
  are real directories (`source_directories`), like Bazel's per-build file
  system values, instead of once per input (~90k lstats of ten ancestors on
  a TypeScript no-op); the input itself is still inspected (lstat and
  digest) on every use, and a path through a symlink still fails exactly as
  before;
* a cache-owned directory is verified by opening the chain below its
  deepest verified ancestor one component at a time (`O_PATH | O_NOFOLLOW
  | O_DIRECTORY`, relative to the parent's descriptor) instead of one
  absolute-path lstat per directory (`verify_directories`): on overlay
  filesystems absolute-path lookups serialize across threads
  (crate::dir_stamps). `openat2(RESOLVE_NO_SYMLINKS)` was measured and is
  ~70x slower here;
* the file, output and verified-directory memos are sharded by path (64
  shards): a warm build's workers look them up ~1M times at once;
* a store file being read is read by one action at a time (single flight:
  the others take the result from the file memo), and each action reads its
  memo misses from its own starting point, so the actions that start
  together (all reading one toolchain) do not queue behind each other;
* the cache's `actions/`, `locks/`, `work/` and `cas/` directories are
  created (and the cache root canonicalized) once per process instead of
  per action (5 `mkdir` and two `canonicalize` — a `readlink` per path
  component — per action before); a test that removes a cache meanwhile is
  served by creating them again when an action's lock is missing;
* a valid cache entry is accepted without taking its lock: entries are
  published by an atomic rename and only ever removed, so a check that finds
  every output valid found one complete entry, and a check racing a removal
  fails and takes the lock. The lock serializes checking and executing one
  key across processes; it never protected an entry after its check. Tree
  stamps (below) are written under it.

The memos are bounded (256 Ki entries each, the tree memo 1 Mi entries — a
TypeScript build's `node_modules` package trees hold ~430k, and a smaller
memo started over during a warm build so that consumers walked the trees
again; a few hundred MB at most in all; a memo that would exceed its cap
starts over).

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
is hardlinked instead and its digest checked after linking), a repository
tree file that an executable is linked to (below) by mode, size and digest,
all through the stat-keyed digest memo (the tree file by its change time);
a copy or tree file whose bytes no longer match fails the action
(`input ... changed while it was being staged`), and every staged inode is
re-checked after its process ran. Execution roots (`work/`) and the
working areas of repository rules and module extensions keep the full
checks on every use; paths outside the cache keep them for the file itself
(its directories once per process, above).

**The digest memo answers only for a file state it observed**
(fix-digest-memo, `action::DigestMemo`). A file deleted and replaced by one
of the same size inside one timestamp tick usually passes its inode number
on at once, with identical modification, change and birth times (always on
overlayfs). The memo used to answer by that stat key alone, for any path, and
it remembered an output it had just hashed for publication unconditionally
(and any owned file whose modification time was old): an output in an
execution root whose content's blob already existed is not linked into the
store and goes away with the execution root, so a new output of the same size
created in the same tick by another action got its digest, and the old
content's blob was published as that action's output — in about one of four
cold builds of 4,000 equal-size write actions, and every later build took
the wrong output from the action cache. Now:

* a state read when it was settled (modification and change time, for a
  cache-owned file modification and birth time, at least 2 s older than the
  read) answers for every path of its inode: no file created or changed after
  the read can show it again;
* a racy state answers only at the path where this process established the
  content: a blob it validated or created (its name is its content) and a
  published output linked to such a blob. Every removal or replacement of
  such a path by the process forgets it: an invalid blob removed by
  `blob_valid` or `check_staged`, a store candidate renamed over a blob, an
  action entry removed to be published again (`forget_published`);
* an output being published is hashed without being remembered by its path
  (the execution root goes away); its state is handed to the blob it is
  linked into (`ensure_blob_hashed`), so it is still read once.

The persistent memo receives trusted records only from such knowledge (a
settled state, or content established on that path), never from a stat key
alone; see docs/DIGEST_MEMO.md, "What is recorded". Unit tests
`a_new_file_never_inherits_a_deleted_files_digest`,
`outputs_of_one_size_are_published_with_their_own_content`,
`stale_states_never_reach_the_persistent_table`; CLI test
`outputs_of_one_size_keep_their_own_bytes` (tests/digest_memo.rs).

## Cache checks of published trees (perf-cache-hits)

A cache check used to walk every published tree artifact of the entry and
every published runfiles tree (list each directory, lstat every entry, look
every file's digest up, `readlink` every runfiles link) on every command: a
warm no-op `build //src/ts/...` on ~/code made ~2.1M `statx` (1.65M on
stored npm package trees), 518k `readlink` and 124k `mkdir`, created 8,183
threads and spent ~150 s of kernel time in 6.5 s of wall time. Source:
`src/action/tree_stamps.rs`, `src/dir_stamps.rs`.

**Directory stamps.** Once a full check found a published tree valid, the
entry keeps, next to its manifest, the stamp of each of the tree's
directories (`actions/<key>/tree-stamps`): device, inode, mode, modification
and change time. A directory's change time moves with every entry added,
removed or renamed in it and with every change of its own mode or times, so
while every directory still shows its stamp the tree still has exactly the
names, and each name exactly the inode, that the full check found. The next
check opens each directory relative to its parent's descriptor (`O_PATH`,
never through a symlink) and compares the stamps, then takes the files from
the manifest (digests, modes) and the stamp (sizes) without inspecting them,
like the per-process memos take a published output's metadata once per
command. A runfiles tree is stamped the same way (its `dir` entries), its
listing (links and empty files) then not read again; its executable sibling
is still checked.

* **Trust.** The tree's files are store inodes, read-only like every
  published output, and follow their trust (docs/DIGEST_MEMO.md, "Trust"):
  an in-place edit of a file's bytes (after a `chmod`) is not re-detected by
  a cache check — before, one that kept size, modification time and mode
  was not; now none is, as long as no directory of the tree changes. Any
  change of the tree's structure is (an added, removed, renamed or replaced
  entry, at any depth), and so is a directory replaced or moved (its inode
  or change time), or a symlink in its place. Staging still verifies every
  blob it links by mode, size and digest (above), so an executed consumer
  never runs on bytes other than the keyed ones.
* **Settled only.** A stamp is recorded only for directories whose
  modification and change times were at least
  crate::digest_memo::RACY_MARGIN (2 s) older than the start of the check
  and that the full check left unchanged, so no later change can show the
  stamped state again (a change inside the same timestamp tick as the
  stamp would). A tree published just now is therefore walked once more by
  the next check, which stamps it.
* **Format.** `BZRSTSTP`, format version 1 (u32), the first 8 bytes of the
  body's blake3 hash, then a postcard body: the blake3 of the manifest's
  bytes (stamps of another manifest never count) and, per stamped output
  (index, tree or runfiles), the directory stamps (root first, then the
  manifest's directories in order) and the file sizes. A missing, foreign,
  corrupt or non-matching stamp file means the full check, which writes a
  new one (atomically, under the entry's lock). Stamps are written only when
  they change: a no-op build writes nothing. Entries published again lose
  their stamps with the entry; binaries without stamps ignore the file
  (their manifests and keys are unchanged), so caches stay shared both ways
  (a cache of the master binary gives 100% hits to this one and vice versa).
* **Switch.** `BAZEL_RS_VERIFY_OUTPUTS=full` never trusts a stamp: every
  check walks the trees (and records stamps).
* `PERF-STAGE cache-check setup=<s> prepare=<s> lock=<s> validate=<s>
  hits=<n> misses=<n> trees: stamped=<n> walked=<n> stamps_written=<n>`
  (under `BAZEL_RS_TIMING`) sums the cache checks' phases over all workers.

**Measurements** (~/code 31caa52, private cache, release builds, warm no-op
`build` with an analysis snapshot hit, interleaved, shared 64-core machine;
wall / user / sys of the whole command):

| target set | actions | master 00e6d59 | perf-cache-hits |
|---|---|---|---|
| `//src/ts/...` | 24,608 | 6.53–6.77 s / 20 s / 151 s | 1.50–1.56 s / 13 s / 7–9 s |
| `//src/beyondwork.ai/bworker/...`¹ | 9,853 | 3.17–3.48 s / 11 s / 68–75 s | 1.36–1.47 s / 8 s / 4–5 s |
| `//src/beyondwork.ai/tools/oauth2token/...` | 692 | 0.33 s / 0.6 s / 1.3–1.5 s | 0.30–0.31 s / 0.55 s / 0.7 s |

¹ without `//src/beyondwork.ai/bworker/docsfilter:docsfilter_test`, whose
dependency `//src/python:docstar` fails to execute (also with master).
Peak RSS of the TS no-op 1.32–1.36 GB → 1.45–1.48 GB (the larger tree memo
and the output memo). The microbenchmark behind the descriptor-relative
lookups (this host, overlayfs): an absolute-path `lstat` of a deep cache path
costs ~2.5–3 µs whatever the number of threads (it does not scale), a
one-component `fstatat` from the parent's descriptor ~1 µs on one thread and
~0.3 µs per call with 64 threads.

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
written into the execution root and published through this store.
fix-digest-memo moved the manifests alone to `native-action-v9`: entries of
the binaries whose digest memo could publish another action's output are
never trusted, so every cache executes its actions once more; the keys stay
`native-action-v8`, docs/DIGEST_MEMO.md "Upgrade note".)

## Trade-offs and limits

* Staged inputs are `0444`/`0555`, not the source's mode: a source `0644`
  header reads as `0444` inside the action (Bazel: `0644`, writable under
  local execution and processwrapper-sandbox, read-only mount under
  linux-sandbox). Executable bits are unchanged and keyed. Actions that write
  their inputs in place fail (Bazel's sandboxes also forbid or discard this);
  so do metadata changes of a staged input (`touch`, `chmod`), which Bazel's
  hermetic sandbox also reports, while its symlinking sandboxes pass them on
  to the source file.
* Distinct artifacts with identical contents are distinct inodes inside an
  action, as in Bazel ("Distinct artifacts, distinct inodes"); the further
  ones cost a persistent copy each.
* Published outputs share inodes with the store and with each other (so do
  their links in the output tree, `bazel-bin`; Bazel: separate files, see
  "Distinct artifacts, distinct inodes"); editing one in place (after `chmod u+w`) edits all and is detected (digest
  mismatch) on next use. A process that forges size, mtime and mode after an
  in-place write is not detected (execution is sandboxed by default since
  N59, but the Landlock sandbox leaves metadata operations unrestricted and
  denies nothing for `standalone`/`no-sandbox` actions or a store below a
  sandbox-writable root such as `/tmp`; docs/SANDBOX.md).
* Once the action's process exits, every process it left — its process group
  and the descendants that left the group (`setsid`, a daemon) — is killed and
  reaped by the action's supervisor before its staged inputs are re-checked and
  its outputs hashed (docs/PROCESS_WRAPPER.md; Bazel's process-wrapper is a
  child subreaper that does the same, `process-wrapper-legacy.cc`
  `WaitForChild` and `TerminateAndWaitForAll`, and its linux-sandbox ends a PID
  namespace), so a background process the action started cannot change a
  published output whose digest this command and, through the persistent
  digest memo, later commands trust.
* Inside the action every input is a regular file; Bazel's sandboxes show
  symlinks (a source directory's dangling symlinks stay symlinks,
  docs/SOURCE_DIRECTORIES.md). `readlink` of a published symlink output names a native path
  (workspace source or cache entry), like Bazel's absolute execroot path.
  One exception (below): an executable aliasing a repository file.
* The store persists one copy of each distinct input/output content until
  the cache's garbage collection removes blobs no entry links and abandoned
  `.tmp-*` files (docs/CACHE_GC.md).
* Cross-device or link-limited filesystems (`EXDEV`, `EMLINK`, ...) fall back
  to copies for staging and to rename/copy for publication (unit tests
  `failed_hardlinks_fall_back_to_copies`, `emlink_hardlink_failures_fall_back_to_copies`).
* A blob removed between its validity check and the link (a concurrent
  damaged-blob removal) is ensured once more and linked again.
* The digest memo keys a cache-owned file (blob, published output) without
  its ctime, since every link() to it bumps that (keyed by inode, size, mtime
  and birth time; settled by mtime and birth time): an in-place edit that
  restores size and mtime is not seen, as for the staged-inode check above.
  Since L2-W2 such digests also persist across commands; repository tree
  files persist only with their ctime, because a
  command verifies only the trees it materializes (docs/DIGEST_MEMO.md,
  "Trust").

## Distinct artifacts, distinct inodes

The store keeps one blob per content and mode, and staging used to hardlink
every input file to its blob: two distinct artifacts with identical bytes
(two sources, two generated files, a copy of a source, a tree artifact's
file) were one inode inside the action. Bazel shows them as distinct files —
its sandboxes link every input to its own execroot file or source, its
runfiles trees link to those, and its outputs are separate files — so tools
that detect hard links by inode (`tar`, `bsdtar` with an mtree spec, `cp
-a`, `rsync -H`) recorded links between files that are unrelated in Bazel.
On ~/code, rules_js's `js_image_layer` stored
`.../main.runfiles/_main/src/website3/package.json` (a copy to bin of the
source) as a tar hard link to the identical
`.../main/standalone/src/website3/package.json` of the Next.js standalone
tree; `//src/website3:image_structure_test` extracts a subset of the layer
that holds the former but not the link's target and failed with
`package.json missing`.

Now staging gives every distinct artifact of one execution root (an
action's, a test's runfiles, crate::test_runner) its own inode
(`DistinctInodes` in `stage_inputs`): the first artifact of a content and
mode is staged from the blob `cas/<digest>-<mode>` as before, the `n`th
further distinct artifact from the blob's `n`th distinct copy
`cas/<digest>-<mode>.<n>` (`ensure_copy`: a verified copy of the blob,
created on first use, then kept, validated, remembered and, when damaged,
removed exactly like a blob). An artifact is identified by its resolved
path (the workspace source, the published output; `actions.symlink` aliases
resolve to the artifact they name), so one artifact keeps one inode at all
its paths — its execution path, runfiles, aliases — as `stat -L` shows it
through Bazel's symlinks. Files of repository trees are links to their own
tree files already ("Repository files" below). The key, the manifests and
the published outputs are unchanged; only which store inode a staged path
links to changes.

The genuine capture `tests/fixtures/distinct_inodes` (capture.py,
reference_receipt.json; test
`distinct_artifacts_with_one_content_are_distinct_inodes`, unit test
`distinct_artifacts_with_one_content_are_staged_as_distinct_inodes`) pins the
inode partition of two sources, two genrule outputs, a copy of a source, a
tree file (all `same\n`) and an `actions.symlink` alias of a source, as an
action's inputs, a tool's runfiles and a test's runfiles, standalone and
sandboxed: genuine and native both show six distinct inodes and the alias on
its source's. (Native shows regular files where genuine shows symlinks, the
difference declared since N36.)

**Disk and time** (~/code, private caches, same binary but this change,
repositories fetched first; the machine was shared, timings indicative):

| | cold `build //src/beyondwork.ai/tools/oauth2token/...` (692 actions) | then cold `test //src/website3/...` (13,172 actions) |
|---|---|---|
| before | 34.8-47.8 s, `actions/`+`cas/` 545 MB | 350-351 s, 8.09 GB, image_structure_test FAILED (9/10) |
| distinct copies (this change) | 33.6-44.5 s, 545 MB | 359 s (paired with 351 s), 10/10, 9.64 GB (+1.55 GB: 25,864 copies, 1.39 GB of them a first copy of ~17,500 contents) |
| also separate published outputs (rejected) | 38.2 s, 787 MB (+44%) | 361 s, 10/10, 14.24 GB (+6.15 GB) |

The copies are what one execution root needs at most: on ~/code mostly the
Next.js standalone tree's copies of `node_modules` files (prebuilt native
binaries of ~100 MB among them) beside the package store in the image layer
and test runfiles; the oauth2token build needs none. Publishing every output
as a file of its own (renamed into place instead of linked to an existing
blob), which would also give the output tree Bazel's separate files, was
measured and rejected: it stores every duplicate output again (rules_go
builds the standard library in the exec and target configurations with
identical results: +242 MB on oauth2token; +4.5 GB more on website3) and
fixes nothing a process of the build sees. So published outputs with one
content still share their blob's inode, also in `bazel-bin` (declared
difference; the capture's `output_tree_*` lines). Without reflinks on ext4,
there is no cheaper way to give hardlink-staged artifacts distinct inodes.

## Repository files: links into their tree

Every input file of a process that is — or resolves through published
symlink outputs to — a file of a published repository tree is staged as an
absolute symlink to that tree file instead of a hardlinked blob
(`stage_tree_link`, `stage_input_file`): at its own execution path
(`external/<repo>/...`), as an `actions.symlink` alias below `bazel-out/`, as
a tool's runfile (`<tool>.runfiles/<repo>/...`) and as a test's runfile
(`stage_content_inputs`, crate::test_runner), files of tree and source
directory inputs included. Genuine Bazel 9.3 presents such a file as a
symlink into `<output_base>/external/<repo>` everywhere: its runfiles trees
are symlinks (SymlinkTreeHelper), its symlinked sandboxes link every input
(`SymlinkedSandboxedSpawn`) and its execroot reaches `external/<repo>` through
a directory symlink (`SymlinkForest`). A program that finds its installation
through its own resolved path then finds it complete in the repository tree:

* rules_nodejs's `bin/node` is a wrapper that follows `readlink` of its own
  path and runs the undeclared `nodejs/bin/node` beside it. As a staged blob
  it resolved to the runfiles directory and failed with exit 127
  (`.../bin/nodejs/bin/node: No such file or directory`; ~/code's
  `//src/beyondwork.ai/crossplane/testing/valuehunterlivedata:valuehunterlivedata_test`,
  data `@nodejs//:node`).
* CPython searches `lib/python3.X/` upwards from `realpath(argv[0])`
  (`Modules/getpath.py`). rules_python's PyCompile action runs
  `bazel-out/<exec>/bin/external/rules_python+/python/private/python3`, an
  `actions.symlink` of the hermetic interpreter (`py_exec_tools_toolchain.bzl`
  `current_interpreter_executable`); staged as a blob it resolved to itself
  and failed with `Could not find platform independent libraries <prefix>` /
  `No module named 'encodings'`.

The genuine capture `tests/fixtures/runfiles_repo_links` (capture.py,
reference_receipt.json; test
`runfiles_and_inputs_of_repository_files_link_into_their_repository`) pins a
test's runfile, an input run at its execution path and a tool's runfile, each
such a wrapper, under the standalone and sandboxed strategies (genuine:
local and processwrapper-sandbox): the runfiles are symlinks and every
wrapper runs its sibling.

Blob staging stays for everything else, where the digest guarantee needs a
copy of the keyed bytes: workspace sources (live files a symlink would
expose to edits during the build; genuine links them too — a declared
difference since N36), published outputs (already store inodes) and every
other path outside the repository tree stores (`REPOSITORY_TREE_STORES`; a
repository rule's working area, a symlinked-in source directory).

The key is unchanged (staging only). When it is linked, the tree file must
still be exactly the keyed input — size, mode and digest — or the action
fails with `input ... changed while it was being staged`. The key took the
digest from the per-process file memo, which keeps a store file's digest by
its path: without the digest check, a tree file rewritten in place after its
first read in this command would run its new bytes under a key computed from
the old ones (blob staging stages the blob of the keyed bytes instead). The
check hashes the file by its change-time key, so the digest memo answers
only for the unchanged file, never for a rewrite that kept the size and
restored the modification time (test
`tree_links_verify_the_keyed_content_when_staged`). After the process the
tree path itself is checked (the link in the execution root is the
process's own to replace): the same inode in a changed state (an in-place
write, `touch`, `chmod`) fails the action like a modified blob, and so does
another inode or none at that path — a file renamed over it, a removal —
unless the path holds a regular file with the keyed content and mode
(another process's republication of the same tree identity replaces every
inode by identical bytes and modes; test
`tree_links_count_replaced_tree_files_as_modified_unless_identical`). The
tree's file is never removed (the executor removes only `cas/` blobs).
Through the link a process cannot write to the tree file or next to it:
published trees are sealed (docs/REPOSITORY_SLOTS.md, "Sealed trees"; a
Python importing from a tree skips the `__pycache__` it cannot write, as it
does in rules_python's read-only `lib/`). A tree of an older binary, still
writable, that is written into makes the repository re-execute on the next
command (the store's verification). Tests:
`generated_executables_aliasing_tree_files_resolve_into_the_tree` (unit:
alias, own path, runfile and plain input),
`aliases_of_sources_and_outputs_keep_blob_staging` (unit),
`tests/tree_executables.rs` (CLI, sandboxed and standalone; aliases of a
repository file, a workspace source and a generated file; the genuine
capture above).

`PERF-STAGE staging <s> calls=<n> tree_links=<n> blobs=<n>` (after
`execute.end`, under `BAZEL_RS_TIMING`) reports the summed wall time of input
staging over all workers and how many files took each path.

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
`native-action-v9` (v8 before fix-digest-memo, v7 before the N41 x N42 merge) regular-file outputs
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
