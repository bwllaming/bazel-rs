# Native text patches: bounded compatibility profile

Single-version override patches are read from real files in the root repository.
Their label context includes only main and the root module's own alias, not the
selected dependency graph. Package BUILD markers and regular files are required;
symlink inputs, generated patch targets and external patch materialization are
not supported. Canonical external patch labels are skipped during MODULE discovery,
as in Bazel 9.3, but fail explicitly if materialization is requested.

All local patch bytes are snapshotted before use. Discovery applies them with
Bazel's `PatchUtil.applyToSingleFile` semantics (ModuleFileFunction.java:935-955):
every section is scanned and format/path errors anywhere fail discovery
(`error applying single_version_override patch PATH to module file: MESSAGE`), but
only sections whose old and new file both are the root `MODULE.bazel` are applied.
Nested MODULE files, MODULE creation/deletion, renames, mode-only and binary
sections and concatenated plain sections ending in another file are skipped.
The projected MODULE is evaluated before dependency discovery and MVS.
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

## Patch semantics (N26: PatchUtil port)

`src/patching.rs` is a line-for-line port of Bazel 9.3
`bazel/repository/decompressor/PatchUtil.java` and of the java-diff-utils 4.12
code it calls (`UnifiedDiffUtils.parseUnifiedDiff`, `Patch.applyFuzzy`,
`Chunk.verifyChunk`, `ChangeDelta.applyFuzzyToAt`). It backs every native patch
consumer: single-version-override discovery and materialization, registry
`patches`, Git override patches and `repository_ctx.patch` (and therefore
`http_archive(patches=..., patch_args=["-pN"])`).

* Patch files and patched files are read as UTF-8 lines (Guava `readLines`:
  `\n`, `\r\n`, `\r`; malformed bytes become U+FFFD) and written back as UTF-8
  with `\n` after every line (`writeLinesAs`).
* Sections are applied while the patch is scanned; a new section starts at
  `diff --git` or any other unknown line. Two plain sections without a separator
  form one section for the second file, whose hunks then include the first
  section's lines (captured: the first file stays unchanged).
* Hunks of one section are applied separately, last hunk first, with
  `applyFuzzy(..., 2)` (PatchUtil.java:221-235): a hunk is retried at increasing
  offsets ignoring up to two leading and trailing hunk lines. A two-line hunk that
  matches nothing is therefore skipped without error, and a hunk with wrong
  context lines can apply. A hunk that matches at no offset fails with
  `in patch applied to FILE: could not apply patch due to
  CONTENT_DOES_NOT_MATCH_TARGET, error applying change near line N`; index errors
  of java-diff-utils report the JDK text (`Index 3 out of bounds for length 3`).
* Errors use PatchUtil's texts: `Cannot find file to patch (near line N), old file
  name (X) doesn't exist, new file name (Y) doesn't exist.`, `Cannot determine file
  name with strip = S at line N:\nLINE`, `Expecting more chunk line at line N`,
  `Looks like a unified diff at line N, but no patch chunk was found.`, `The patch
  content must start with ---/+++ prelude lines at line N.`, `Wrong chunk detected
  near line N: LINE, does not expect ... line here.`, `Cannot patch file outside of
  external repository (DIR), file path = "P" at line N`, `Cannot rename file ...`,
  `Wrong file mode format at line N: LINE` and the module a/b prefix hint.
* Creation into a missing directory creates it; creation over an existing file
  prepends the new lines (as Bazel does); deletion removes the file.
* Git headers: `diff --git a/x b/y` names, `rename from/to` (renames), `new mode`
  and `new file mode` owner digits (read 0400, write 0200, execute 0111); other
  Git lines (`index`, `similarity index`, ...) are ignored; binary sections apply
  nothing. Existing permissions are preserved.
* Cases that crash genuine Bazel 9.3 (exit 37), such as a one-line hunk that only
  "matches" with fuzz 1 (`subList(1, 0)`), an out-of-range hunk size or a short
  `new mode` line, are explicit native errors (`genuine Bazel 9.3 crashes applying
  this patch (...)`).

Native safety profile, stricter than Bazel: the tree root's existing ancestors
must be real directories; symlinked directories inside the tree are never
traversed; a leaf symlink is read through only when it resolves inside the tree
and is then replaced by a regular file (the referent is unchanged); modes are
normalized to 0644/0755 for the store. Like PatchUtil, patch and target texts
are read whole into line lists; N32 removed the former 16 MiB, 100000-line,
256 MiB working-set and 100-million-comparison native caps. Tree patches run only in caller-owned staging; a failing patch may leave
earlier sections applied (as in Bazel), and callers discard the stage.

Contract sources are Bazel 9.3 commit `e89755babf3e941bc1e9d8120b1cd48854ab9dfa`
and java-diff-utils 4.12. Full repository-rule execution, built-ins, configurations
and application build/test compatibility remain incomplete.

## Verification

`artifacts/patch-validation.json` records 14 passing genuine Bazel 9.3 differential
cases and 13 passing native-only safety/capability checks. It includes exact graph
JSON, built file bytes, command diagnostics, executable hashes and cleanup receipts.
The malformed remote-integrity check tests descriptor rejection; a Rust test
separately verifies rejection of downloaded bytes with a valid but wrong digest.
The full check passes 408 Rust tests, 194 Python tests, formatting, strict Clippy
and release compilation. This evidence covers the documented subset only.
