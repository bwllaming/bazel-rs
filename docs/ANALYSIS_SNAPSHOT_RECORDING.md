# Analysis snapshot: the recording layer (L2-W3)

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
  `@bazel_tools`) with the lstat key of each entry and of its manifest; the
  spec pointer naming a direct repository is a READ.
* **TAINT**: conditions under which a run is never saved — network access,
  an executed repository rule (`local = True` rules always execute), an
  evaluated (not lockfile-reused) module extension, a (re)published repository
  tree, a pending MODULE.bazel.lock or hidden-lockfile write, a symlink hop
  into another repository root or a host path, a build-info callback
  (`transform_version_file` / `transform_info_file`), a `map_each` over a tree
  artifact, enumeration of the whole repository environment, errors (planning
  failed or reported errors, or the command failed), Unstable, and test/debug
  hooks (`BAZEL_RS_LOADING_JITTER`, `BAZEL_RS_LOADING_NO_DEMAND`,
  `BAZEL_RS_DEBUG_*`).

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
first reasons, `eligible=yes|no`, and the measured time of the validation a
hit would run, as a dry run over everything recorded
(`crate::snapshot::validate`: path-sorted blocks on min(available
parallelism, 16) scoped threads; settled stat keys are trusted, racy or
changed ones re-read and compared by content; integrity entries by lstat).
With `BAZEL_RS_SNAPSHOT_DUMP` the observations (`OBS`), environment digests
(`ENV`), repositories and integrity entries (`REPO`, `INT`), taints and the
summary are written to the file, one record per line, paths with every
non-graphic byte escaped as `\xNN`.

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
   `strace -f -y -e trace=%file,getdents64` in record mode; the window is
   bracketed by stats of `/.bazel-rs-snapshot-marker/window-{open,close}`.
   Every path touched inside the window outside the cache dir must be covered
   by the dump (an observation, an ancestor of a canonicalized path, a listed
   directory, or a path inside a verified repository tree); `/proc`, `/sys`,
   `/dev` and the workspace status's host facts are exempt. Without arguments
   it audits a copy of `tests/fixtures/snapshot_audit/workspace`;
   `--workspace DIR --cache DIR -- ARGS` audits any workspace.

Shadow verification and the determinism gate (layers 3 and 4) come with the
snapshot store itself.
