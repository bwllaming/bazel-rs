# Native text patches: bounded compatibility profile

Single-version override patches are read from real files in the root repository.
Their label context includes only main and the root module's own alias, not the
selected dependency graph. Package BUILD markers and regular files are required;
symlink inputs, generated patch targets and external patch materialization are
not supported. Canonical external patch labels are skipped during MODULE discovery,
as in Bazel 9.3, but fail explicitly if materialization is requested.

All local patch bytes are snapshotted and parsed before use. Discovery validates
every section, including paths and hunk counts in source-file sections, then
applies only changes whose old and new paths both identify root `MODULE.bazel`.
Nested MODULE files and MODULE creation/deletion sections are not applied in this
projection. The projected MODULE is evaluated before dependency discovery and MVS.
`patch_cmds` do not affect discovery and are never executed by this implementation.

Materialization retains the original registry MODULE independently of the projected
evaluation bytes, and uses this order:

1. Extract the verified archive.
2. Download and verify overlay files.
3. Download, verify and apply registry patches in descriptor insertion order.
4. Replace MODULE with the **original** registry bytes.
5. Apply the snapshotted local override patches in order.
6. Validate and publish the completed tree.

This avoids applying a MODULE patch twice. Local patch contents, ordering, strip
settings and command settings participate in repository cache identity. Changes
to patch files affect a fresh resolution, not a plan already captured in memory.
Commands and external patch inputs are rejected before source downloads/cache reuse.
Live `local_path` sources with transformations are rejected rather than modified.

## Supported text semantics and limits

The native parser accepts UTF-8 unified text diffs, ordinary Git preambles and
index records, multiple files/hunks, and `/dev/null` creation/deletion. File
sections must have explicit boundaries such as `diff --git` headers; adjacent
plain `---`/`+++` sections are rejected. Bazel 9.3 does not reliably apply that
ambiguous form and can crash after a resulting context mismatch; retained
reference evidence is `artifacts/patch-order-followup-report.json` and
`artifacts/patch-order-firstonly-report.json`. No crash-equivalence is claimed.
File sections run in input order; hunks run in reverse order. Matching is exact with
nearest-offset search and earlier-position tie breaking. There is **no fuzz**:
fuzz-dependent or mismatched patches fail explicitly. Bazel's full fuzzy matcher,
Git rename/copy, explicit mode metadata, binary/combined diffs and quoted Git
paths are not implemented.

CR, CRLF and LF input lines are accepted; modified files are written with LF
after each line, including when a no-final-newline marker is present. Invalid
UTF-8 and NUL text fail explicitly. Strict path validation rejects absolute paths,
parent traversal, backslashes and ambiguous paths even in skipped sections; this
is stricter than Bazel's path normalization. Stripping counts lexical slash
components **before** normalizing `.` or empty components, so a prefix cannot
silently redirect a patch to a different file. Sources are checked for symlinks,
special files and multiply-linked files before modification.

Limits are 16 MiB per patch/text file, 100000 lines/hunks, 64 MiB of snapshotted
local patches, 256 MiB of patched-tree working data and 100 million context-line
comparisons per section. Existing file modes normalize to 0755 when owner-executable,
otherwise 0644; created files use 0644. These are explicit subset limits, not full
PatchUtil compatibility.

Tree patches run only in caller-owned staging. A complete patch is checked before
writes, but I/O errors still require discarding the stage. No source checkout is
modified, and failed transformations never publish a completed repository. Cache
and staging ancestors remain trusted against hostile concurrent mutation; this
is not a sandbox.

Contract sources are Bazel 9.3 commit `e89755babf3e941bc1e9d8120b1cd48854ab9dfa`:
`ModuleFileFunction.maybePatchModuleFile`, `PatchUtil.applyToSingleFile`, and the
remote/original-MODULE/local ordering in `tools/build_defs/repo/utils.bzl`.
Full repository-rule execution, Git overrides, built-ins, configurations and
application build/test compatibility remain incomplete.

## Verification

`artifacts/patch-validation.json` records 14 passing genuine Bazel 9.3 differential
cases and 13 passing native-only safety/capability checks. It includes exact graph
JSON, built file bytes, command diagnostics, executable hashes and cleanup receipts.
The malformed remote-integrity check tests descriptor rejection; a Rust test
separately verifies rejection of downloaded bytes with a valid but wrong digest.
The full check passes 408 Rust tests, 194 Python tests, formatting, strict Clippy
and release compilation. This evidence covers the documented subset only.
