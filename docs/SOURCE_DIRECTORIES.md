# Source directories as action inputs, outputs and runfiles

A source artifact may name a directory: a package directory used as a file
target (`srcs = ["dir"]`, `glob(..., exclude_directories = 0)`), or a
directory of an external repository — rules_js's `npm_import` extracts an npm
package into `package/` and its `npm_package_internal(src = "package")` hands
that directory to the package store's `copy_directory` action and to the
lifecycle-hook action (`lc`). Until this change the native profile accepted
such targets in analysis but failed every executing use explicitly
(`source directory 'L' as an action input is not supported by the native
profile`; on ~/code 41 such errors stopped `build --nobuild //src/ts/...`).
Implementation: `src/action.rs` (`SourceDirectory`, `read_source_directory`,
`ActionExtra::source_directories`), `src/source_directories.rs` (traversal
errors), the planner in `src/engine.rs`; evidence
`tests/fixtures/source_directories/reference_receipt.json` and
`tests/source_directories.rs`.

## Bazel 9.3

Pinned source: `/tmp/opencode/bazel-9.3-reference`; paths below are relative
to `src/main/java/com/google/devtools/build/lib/`.

**Tracking mode.** `skyframe/TrackSourceDirectoriesFlag.java`: the JVM system
property `BAZEL_TRACK_SOURCE_DIRECTORIES` (`0` or `1`, set with the startup
option `--host_jvm_args=-DBAZEL_TRACK_SOURCE_DIRECTORIES=…`, so toggling it
restarts the server) overrides `TRACK_SOURCE_DIRECTORIES_DEFAULT`, which is
**true in Bazel** (Blaze's Copybara rewrite makes it false). Bazel 9.3 tracks
source directories by content unless told otherwise.

**The value** (`skyframe/ArtifactFunction.java:274-364`, `createSourceValue`).
A source artifact whose FileValue is a directory posts a
`SourceDirectoryEvent` and, with tracking on, is traversed by
RecursiveFilesystemTraversalFunction (`DirectoryArtifactTraversalRequest`:
strict output files, skip the root's own package test, **emit empty
directory nodes**); the value is
`FileArtifactValue.createForDirectoryWithHash(fingerprint)` over every
traversed entry's `(path in the symlink tree, metadata digest)`
(`HashedDirectoryArtifactValue`, type DIRECTORY, `getDigest()` the
fingerprint). Without tracking it is `DirectoryArtifactValue(mtime)`, which
the action cache compares by the directory's mtime only (unsound for edits
below it).

**The traversal** (`skyframe/RecursiveFilesystemTraversalFunction.java`,
`RecursiveFilesystemTraversalValue.java`):

* Entries come from DirectoryListingValue; a regular file contributes its
  content digest, a symlink to a file the target's digest at the link's path,
  a symlink to a directory is traversed at the link's path (plus a node
  hashing its children), a dangling symlink a digest of its link text
  (`ResolvedFileFactory.danglingSymlink`), an empty directory an empty node.
  Executable bits are not part of any digest.
* Every subdirectory of a **main-repository** source directory is tested for
  a package (`forChildEntry` clears `skipTestingForSubpackage`):
  `Directory artifact P crosses package boundary into package rooted at Q`.
  External repositories are not checked unless
  `--incompatible_check_external_repo_source_dir_package_boundary`
  (`BuildLanguageOptions.java:767-778`, default false).
* A link into a directory the link lies in is
  FileSymlinkInfiniteExpansionException (`Error while traversing directory
  P: Infinite symlink expansion`, the chain reported once as `infinite
  symlink expansion detected [start of symlink chain] … [end of symlink
  chain]`); a link chain that does not resolve is
  FileSymlinkCycleException (`… : Symlink cycle`, the cycle reported once as
  `circular symlinks detected [start of symlink cycle] … [end of symlink
  cycle]`, rotated to its smallest path).
* Failures are `SourceArtifactException`s
  (`ArtifactFunction.java:520-557`): `error traversing directory 'LABEL':
  MESSAGE`, which the consumer reports like a missing input
  (`ActionExecutionFunction.createSourceErrorCodeAndMessage`, `:1543-1582`):
  `LOC: PROGRESS failed: error traversing directory …` then
  `LOC: PROGRESS failed: N input file(s) are in error` (`… or do not exist`
  when files are missing too); a requested target: `LOC: TARGET: error
  traversing directory …` / `LOC: N input file(s) are in error`.
  An executable's runfiles fail its `RunfilesTree` action
  (SymlinkTreeAction's inputs); SourceManifestAction has no inputs
  (`analysis/SourceManifestAction.java:168`). Only an executing build
  traverses: `--nobuild` succeeds.

**The warning.** `SkyframeActionExecutor.checkForUnsoundDirectoryInputs`
(`skyframe/SkyframeActionExecutor.java:1696-1728`), called before every
action execution, returns at once when tracking is on; without tracking it
warns `input X is a directory; dependency checking of directories is
unsound` for each source-directory mandatory input of an executed action
(except FilesetTraversal). At Bazel's default it never prints.

**Staging.** Sandboxes stage the input as a symlink to its execroot path
(`sandbox/SandboxHelpers.processInputFiles` skips inputs nested below it;
`SymlinkedSandboxedSpawn`), so the action sees the directory's links as they
are; local (`no-sandbox`) execution sees the execroot's directory (the
symlink forest links top-level entries). Runfiles trees link the directory
(`exec/SymlinkTreeHelper`). The remote cache's input root explodes it
(`remote/merkletree/MerkleTreeComputer.java:1113-1141`,
`explodeDirectory`: `readdir(FOLLOW)`, files at their paths, directories
recursed — no empty directories — and any other entry type, e.g. a dangling
link, is `IOException: The file type of '…' is not supported.`, which fails
the spawn).

## Native

* **One mode, Bazel's default: content tracking.** The planner marks every
  source input whose path is a directory
  (`ActionExtra::source_directories`); the executor reads it once per
  process (`read_source_directory`, memoized by path with single flight,
  like Bazel's per-build value) and keys it by every file's relative path,
  content digest and executable bits, every directory (so empty ones count)
  and every dangling link's text (`frame_staged`); only such actions frame
  the extra `source-directories` block, so every other key is unchanged.
  Symlinks are followed like Bazel's traversal (a file link is read at its
  canonical path; a directory link is walked; a link back into an enclosing
  directory or an unresolvable chain fails). Digests come from the
  per-process and persistent digest memos (docs/DIGEST_MEMO.md), so a warm
  command stats each file once and hashes nothing.
  `--host_jvm_args` is not a native startup option, so the unsound mtime
  mode (and its warning) cannot be selected; the warning never prints, as at
  Bazel's default.
* **Change tracking** (fixture case `incremental`, across invocations like
  genuine's `--batch` action cache): `touch` of files or directories runs
  nothing again; new content, a new or removed empty directory, a retargeted
  dangling link and the dangling link's target appearing run the consumers
  again — exactly genuine's re-executions. **Declared divergence
  (`incremental_chmod`, sounder natively):** native keys executable bits (they
  are part of the staged content, docs/HARDLINK_STAGING.md), so a chmod +x
  inside runs the consumers again; genuine keeps the stale result. A return
  to an earlier state is a native cache hit (native keys are content
  addressed; genuine re-runs), as for every native input.
* **Staging (declared divergence `view`, `view_local`).** Like a tree
  artifact input: a real directory with every file a read-only hardlink of
  its content-addressed blob (`0444` plus its executable bits), every
  directory created, links followed (a link to a file is that file, a link to
  a directory a directory) and dangling links kept as symlinks with their
  link text. Contents, executable bits and names are genuine's (`find -L`,
  `ls -a`, cases `inputs`, `use_tool`, `test`); genuine's sandboxes show a
  symlink to the directory with its links, its local execution the real
  directory with its links. The staged bytes are the keyed ones (a file
  changed after its read fails staging with `changed while it was being
  staged`), and every staged inode is re-checked after the process, as for
  any input.
* **Traversal errors** are reported by the planner before anything executes,
  for executing builds only, with Bazel's texts (cases `err_*`): the package
  boundary of main-repository directories (`.bazelignore`d and invalid
  package names are no boundary), infinite expansion and symlink cycles with
  their chain/cycle events, per consuming action (progress = the native
  action id for Starlark actions, `Executing genrule L` for genrules), per
  requested target, and for an executable's runfiles through its runfiles
  tree action. With several failures below one directory native reports the
  first in byte order (genuine evaluates children in parallel and may report
  another, e.g. either link of a cycle).
* **Runfiles.** Runfiles trees and manifests link the directory's absolute
  path (`run`: `root: symlink`, genuine-exact); a tool's runfiles in an action
  and a test's runfiles (crate::test_runner, `ContentInput`) stage it like an
  input.
* **Remote cache.** The REAPI input root holds the directory's files at their
  paths, links followed, without empty directories — MerkleTreeComputer
  .explodeDirectory (unit test
  `source_directories_enter_the_remote_input_root_as_their_files`: the key
  equals the one of the same files as separate inputs). A dangling link: the
  spawn is not cached remotely (`WARNING: Remote Cache: not used for …: The
  file type of 'P' is not supported.`) and runs locally, where genuine fails
  the spawn (declared).
* **Requested outputs** need nothing built (`Target … up-to-date (nothing to
  build)` in Bazel; native lists the path).
* **Not modelled:** an action whose inputs include a source directory and a
  file below it (`nested`, declared: the analysis-time structure check
  rejects `overlapping input destination`, genuine reaches the file through
  the directory); `--incompatible_check_external_repo_source_dir_package_boundary`;
  special files below a source directory (explicit error; genuine digests
  their state); an empty source directory in the remote input root (genuine
  adds an empty Directory node).

## Cost (measured, release binary, shared machine)

`build` of one `run_shell` over a main-repository source directory, fresh
cache, each command a new process (`/tmp/opencode/perf-research/sd-measure.py`):

| directory | cold | warm no-op | one file changed |
|---|---|---|---|
| pnpm package (1,206 files, 22 MB) | 0.22 s, 32 MB | 0.05-0.09 s, 26 MB | — |
| synthetic (20,000 files in 200 dirs, 79 MB) | 2.25 s, 52 MB | 0.25 s, 45 MB | 0.95 s, 60 MB |

Cold = hash and copy every file into the store once, hardlink them into the
execution root; warm = one lstat per file plus the persistent digest memo (no
hashing); a change re-hashes that file and stages the directory again. The
~/code npm package directories hold at most 1,206 files (pnpm; webpack 730,
es5-ext 431).
