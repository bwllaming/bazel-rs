# Compatibility target Bazel 9.3.1

Since 2026-10-30 bazel-rs targets official **Bazel 9.3.1** (published 2026-10-20,
tag commit `fee2bf2d1c14bc2096068a4d46f1842472092044`), previously 9.3.0
(`e89755babf3e941bc1e9d8120b1cd48854ab9dfa`).

## Why

`native.bazel_version` is the compatibility version. bazel_features' version
extension writes it into `@bazel_features_version//:version.bzl`, and every
module extension loading `@bazel_features` (pnpm, crate, …) has that file in its
`bzlTransitiveDigest`. A workspace whose committed `MODULE.bazel.lock` was
written by Bazel 9.3.1 (`~/code` pins `USE_BAZEL_VERSION=9.3.1`) therefore got
two lockfile lines rewritten by a bazel-rs reporting 9.3.0, and
target-determinator, which fails when the base and head lockfiles differ,
never cached its head result. `tests/bazel_version_digest.rs` pins this with a
genuine capture (`tests/fixtures/bazel_version_digest`: genuine 9.3.1 lockfile
expected byte for byte; 9.3.0's differs in exactly the digest and the
recorded value).

## 9.3.0 → 9.3.1 upstream changes (`git diff 9.3.0..9.3.1`, 5 commits, 19 files)

| Commit | Change | Class | bazel-rs |
|---|---|---|---|
| 35b3fab9 (#31902) | An action whose own output blob is lost when `finalizeAction` downloads it (Build without the Bytes top-level/regex download, a disk cache with partly retained blobs, a symlink to a lost output) is rewound and rerun bypassing the caches instead of failing with `unable to finalize action` (`AbstractActionInputPrefetcher`, `SkyframeActionExecutor.actionsWithLostOutputs`, `ActionRewindStrategy`) | Java server logic, observable | Already equivalent: a remote cache hit whose download finds a blob missing is a cache miss and the action runs (`RemoteCache::download`/`lookup`), a lost blob of a materialized input invalidates the producer and retries the command (docs/REMOTE_CACHE.md "Lost blobs"); a disk cache entry whose blob is gone is a miss the same way (docs/REPOSITORY_CACHE.md). Nothing to port. |
| dd6f75c0 (#31896) | `FileSystemLock` closes the channel when acquiring a lock fails: on POSIX `fcntl` locks, closing any descriptor of the file released the process's other locks on it (repo contents cache GC vs. a running command) | Java server logic (locking) | Not applicable: bazel-rs locks with `flock(2)` (fs2), which is per open file description, and a failed attempt's `File` is dropped at once. |
| d6471209 (#31912) | `discardPreExecutionCache` keeps the packages of every alias in a top-level alias chain (fixes `IllegalStateException` with Skymeld + `--notrack_incremental_state`/`--discard_analysis_cache`) | Java server logic (Skyframe internals, crash fix) | Not applicable: no Skyframe graph or analysis-cache discarding. |
| 7f11dd25 (#31920) | The embedded JDK's `lib/security/cacerts` drops the distrusted Sectigo "AAA Certificate Services" and five eMudhra/emSign roots (`src/minimize_jdk.sh`) | Embedded tools | Ported: `resources/bazel_tools/<platform>/` regenerated from the official 9.3.1 binaries; only `jdk/lib/security/cacerts` differs (resources/bazel_tools/PROVENANCE.md). |
| fee2bf2d | Release 9.3.1 (CHANGELOG.md, also the 8.8.2 entry) | Docs | Version: `COMPATIBILITY_VERSION` = `9.3.1` (`native.bazel_version`, `version`/`--version`, `info release` = `release 9.3.1+bazel-rs.<v>`, BEP `build_tool_version`, profile metadata, capabilities). |

Unchanged in 9.3.1: `src/MODULE.tools` (the `@bazel_tools` MODULE.bazel
sidecar is byte-identical, so the builtin module closure is too), all
Starlark builtins (`src/main/starlark`, so `@_builtins` and its digest), bzlmod
and lockfile code (`lockFileVersion` 28), options and help texts, every
embedded tool other than the JDK truststore.

## Re-checked against genuine 9.3.1

- Builtin modules (`tests/fixtures/builtin_modules`, re-captured with
  `scripts/capture_builtin_modules.py`): both cases' `graph.json` and
  `MODULE.bazel.lock` byte-identical to the 9.3.0 capture; the fixture now
  names the 9.3.1 ELF.
- `info` (`tests/fixtures/info`) and `help` (`tests/fixtures/help`, regenerated
  `src/help_topics.rs`) re-captured: only `release`, the install MD5 and
  run-dependent values (heap, gc, pid, log name) changed; help differs only in
  the `[bazel release 9.3.1]` header.
- Lockfiles, paired genuine 9.3.1 vs native on the same inputs: the
  `lockfile_extensions` fresh update (also equal to the 9.3.0 receipt) and the
  `registry_lockfile` A/update_fresh step (two HTTP registries) are byte-identical.
- New `tests/fixtures/bazel_version_digest` (above).

## Reference binaries and older captures

`.direnv/reference/bazel-9.3.1-linux-x86_64` (SHA-256
`7f775b54bbec487efaf0f49ccda1d2ed02cd34f582edfdd6aeeec76e39ef585c`) is the
genuine reference for new captures; the source is `/tmp/opencode/bazel-9.3.1-reference`
(shallow clone with both tags). `bazel-9.3.0-linux-x86_64` stays next to it:
the receipts captured with it, and the `scripts/validate_*.py` replay harnesses
that verify them (pinned to its SHA-256 `d302d22e…` and the 9.3.0 source), remain
9.3.0 evidence. None of the 9.3.1 changes above touches what they record
(loading, analysis, actions, query, bzlmod, options), so they are kept as
captured instead of being relabelled. Exceptions moved to 9.3.1:
`scripts/package_builtin_tools.py`, `capture_builtin_modules.py`,
`validate_builtin_modules.py` (bound to the bundle), the `info`/`help` capture
scripts, and the native version identity in `bump_version.py`, `release.py`,
`validate_label_passthrough.py` and `module_extension_io/native_capture.py`.
Harnesses that compare a live genuine run's `native.bazel_version` with native
output (e.g. `validate_local_repositories.py`, `validate_existing_rules.py`)
need the 9.3.1 ELF and expectations before their next live run.
