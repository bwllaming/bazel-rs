# Archive symlinks and hard links (N19)

Native repository archive extraction (`src/archive.rs`) now materializes
symbolic links and tar hard links with Bazel 9.3 semantics instead of rejecting
them. Real BCR module archives need this: `aspect_bazel_lib` 2.22.5 contains 15
symlinks, `bazel_lib` 3.0.0 eight and `tar.bzl` 0.6.0 one (all relative links to
files, e.g. `e2e/smoke/.bazelversion -> ../../.bazelversion`), so these modules,
and every toolchain repository that depends on them, could not be materialized.

Citations are to the pinned reference checkout
`/tmp/opencode/bazel-9.3-reference` (e89755b, "Release 9.3.0"), relative to
`src/main/java/com/google/devtools/build/lib/` unless a path says otherwise.

## Consumers and formats

`archive::extract`/`extract_named` has three consumers: registry module sources
(`source.json` archives, `materializer.rs`, Bazel's `http_archive` →
`ctx.download_and_extract`, `tools/build_defs/repo/http.bzl:203`), the embedded
`bazel_tools` tree (`builtin_tools.rs`, no links) and, since N21, direct
repository rules' `repository_ctx.extract`/`download_and_extract`
(`repository_ctx.rs`, docs/REPOSITORY_RULES.md), which name the archive by its
basename in the zip link-escape error and wrap it, like the missing hard-link
target error, in `java.io.IOException: Error extracting ARCHIVE to PARENT: ...`
(`DecompressorValue.java:260-270`). Extracted tar hard links still make that
direct repository fail when its stage is snapshotted (`hard-linked direct
repository files are unsupported`, `repository_store.rs`). Supported formats are unchanged: `tar`, `tar.gz`/`tgz` and `zip`
(including `jar`/`war`/`aar`); `.tar.xz`, `.tar.zst`, `.tar.bz2`, 7z and ar remain
unsupported archive types. Zip has no hard-link representation.

## Bazel 9.3 semantics implemented

| Bazel source | Behavior | Native |
| --- | --- | --- |
| `bazel/repository/decompressor/CompressedTarFunction.java:114-122` | Tar entries `isSymbolicLink()` (type `2`) and `isLink()` (type `1`) take their target through `maybeDeprefixSymlink`. | Same entry types; `archive.rs` `Extraction::symlink`/`hard_link`. |
| `bazel/repository/decompressor/StripPrefixedPath.java:85-96`, `vfs/UnixOsPathPolicy.java:62-84`, `vfs/OsPathPolicy.java:94-127` | The target is normalized as a `PathFragment` (`.`/empty segments vanish, `a/..` cancels, leading `..` stays for relative paths and is dropped at an absolute root), made relative, and **the strip prefix is removed whenever the target starts with it — for relative targets too**. An absolute target is re-rooted at the extraction root (`root.getRelative`). | `normalize` + `deprefixed_target`; the link text is the normalized, deprefixed target (`data/` → `data`, `pkg/data/x` under prefix `pkg` → `data/x`). |
| `bazel/repository/RepositoryFetchFunction.java:318-325`, `bazel/repository/RepositoryUtils.java:109-170` (same-repo branch :160-164) | After the fetch, `replantSymlinks` rewrites every absolute link into the repository as `"../" × depth` + the repository-relative target. | The same text is written directly: `/pkg/data/real.txt` at `abs.txt` → `data/real.txt`, at `sub/abs_nested.txt` → `../data/real.txt`, `/` at top level → `.`. The paired capture confirms Bazel's trees carry exactly these relative texts (receipt `observations.absolute_link_targets`); there is no divergence here. |
| `CompressedTarFunction.java:75-76, 121, 160-166` | Tar symlinks are collected in a map (later entry for the same path wins) and created after all other entries; an existing path is deleted first (`linkPath.exists()`/`delete()`). | Deferred `BTreeMap`, last entry wins; a regular file or empty directory at the link path is replaced; a nonempty directory fails (Bazel: `[unix_jni.cc:660] <path> (Directory not empty)`; native: `archive link "<path>" conflicts with a directory that cannot be deleted: Directory not empty`). |
| `ZipDecompressor.java:84-85, 118-120, 166`, `vfs/FileSystemUtils.java:329-349`, `unix/UnixFileSystem.java:311-320`, `vfs/FileSystem.java:76-83` | Zip symlinks are deferred the same way (later entry wins) but created with `ensureSymbolicLink` **without** a delete: an existing regular file or directory at the link path (an earlier or later regular entry, or entries below the link path) makes `readSymbolicLink` throw `NotASymlinkException`: `<path> is not a symlink`. | `create_symlinks(false)` for zip: the same failure and text with the native staging path (e.g. a link `d.txt` plus a file `./d.txt`, or a link `evil` plus `evil/payload`). |
| `src/java_tools/singlejar/java/com/google/devtools/build/zip/ZipFileData.java:39, 54, 203-204` (Bazel's `ZipReader`) | Zip records are kept in a `LinkedHashMap` keyed by entry name: records with the same raw name collapse to the **later** record (whatever its kind) at the first record's position. | The `zip` crate's `ZipArchive` keeps the same one-record-per-name view (later record's data), so same-name file/link records behave identically. |
| `CompressedTarFunction.java:142-147`, `ZipDecompressor.java:168-178`, `vfs/DiskBackedFileSystem.java:90-104` | Regular entries are written with a truncating `FileOutputStream`: a later regular entry for the same path wins (`d.txt` = `b\n` in the capture), in place — through an earlier hard link to the same inode too. | Later entry wins for tar and zip, written in place into the existing regular file (only an earlier entry or an in-root hard link can be there; no symlink exists before the final pass). A directory at the path remains an error. |
| `CompressedTarFunction.java:123-139`, `vfs/FileSystemUtils.java:937-953`, `vfs/FileSystem.java:768-777` | A hard link is a **real hard link** to `destinationPath().getRelative(deprefixedTarget)` (an archive-root path), created while streaming; a link to itself is dropped; an existing path is deleted first; a target that does not exist fails with `File "<target basename>" linked from "<link basename>" does not exist`. A directory target is hard-linked file by file. | Real `fs::hard_link`, same target interpretation, self-link drop and replacement. A missing target (not yet extracted, outside the prefix, below a non-directory) uses Bazel's exact text. Directory targets remain an explicit unsupported error. |
| `ZipDecompressor.java:142-166` | Zip entries whose Unix mode has `S_IFLNK` are links whose data is the target; `Preconditions.checkState(size < 256)`; when the normalized target has uplevel references and `parent(strippedPath)/target` still does, extraction fails with `Zip entries cannot refer to files outside of their directory: <archive> has a symlink <strippedPath> pointing to <target>`. Absolute targets are not checked. | Same detection, size bound (explicit error instead of Bazel's `IllegalStateException`), check on the stripped path and verbatim message. `<archive>` is the downloaded file's basename in Bazel (`probe_zip_escape-1.zip` in the capture); native uses the source URL's last path segment, which is the same name. |
| `CompressedTarFunction.java` (no check) | Tar symlinks are never checked for escaping the repository; Bazel creates them. | Preserved literally (creating a link writes nothing outside the root). Consumers that would follow them are restricted below. |

The source checkout's own archives confirm the profile: native extraction of the
three cached genuine archives (repository-cache SHA-256 `94e19203…` for
`bazel-lib-2.22.5`, `6fd3b1e1…` for `bazel-lib-3.0.0`, `a147d473…` for
`tar.bzl-0.6.0`) reproduced every path, file digest and link target of the
genuine `external/aspect_bazel_lib+`, `external/bazel_lib+` and `external/tar.bzl+`
trees (328, 250 and 86 entries); the only difference is `MODULE.bazel`, which both
Bazel and the materializer replace with the registry file afterwards. The paired
capture (`tests/fixtures/archive_symlinks/reference_receipt.json`) compares
generated archives and CLI results with genuine Bazel 9.3.0.

## Native safety posture (explicit errors where Bazel would follow)

* Extraction happens in a fresh private stage. Directory creation and file writes
  still reject any symlink component, and no symlink exists until the final pass,
  so no archive entry can write through a link created by the same archive.
* A link below another link in the same archive (`a -> data` plus `a/b.txt -> x.txt`)
  is rejected before any link is created: `archive symlink "a/b.txt" is below
  archive symlink "a" (native extraction never creates entries through a
  symlink)`. Bazel is nondeterministic here because its link map is a `HashMap`:
  with identical archive bytes and fresh output bases, genuine Bazel failed with
  `[unix_jni.cc:660] <OB>/external/probe_link_below+/a (Directory not empty)` in
  some runs and succeeded in others, writing `a/b.txt` through the link as
  `data/b.txt` (receipt `observations.link_below_link_reference_outcomes` and
  `link_below_link_history`: B3 and the six B9 repeats). Native keeps the
  deterministic explicit rejection.
* Entries below a link path (`evil -> ../outside` plus `evil/payload`) create a
  real directory first; the deferred link then fails on the nonempty directory
  (tar) or with `is not a symlink` (zip), as in Bazel.
* A hard link whose deprefixed target starts with `..` is rejected (`escapes the
  extraction root (native extraction never links outside it)`). Bazel reports
  `File "<name>" linked from "<link>" does not exist` only when that outside file
  is absent and would otherwise hard-link a file outside its repository; native
  never inspects or links the outside path.
* Targets must be UTF-8, nonempty, NUL-free and at most 4096 bytes; a target
  that normalizes to the empty path is rejected (Bazel's OS call fails). Tar link
  names are read from the ustar field only: GNU `K` and PAX `linkpath` records
  remain unsupported, so targets longer than 100 bytes are an explicit error
  (Bazel extracts them). Link names on non-link entries are still rejected.
  Special files remain rejected (Bazel would write some of them as empty regular
  files).

## Identity, provenance and cache keys

The materializer manifest records a link as
`{"directory": false, "mode": 0, "sha256": "", "symlink": "<target>"}` without
following it for hashing, chmod or recursion (dangling and external links
included). The `symlink` field is omitted for files and directories, so link-free
manifests keep their exact previous bytes; the cache format (`native-repository-v2`)
and cache keys are unchanged, and no pre-N19 tree can contain links. Retargeting a
link (even to identical content) or replacing it with a regular copy is a cache
mismatch repaired from the verified blob. Hard links are recorded as ordinary
files (content digests); hard-link identity is not part of the manifest.

## Repository transformations after extraction

* Authoritative `MODULE.bazel` and overlay writes replace an existing symlink or
  multiply-linked file instead of writing through it (`materializer.rs`
  `write_source_file`). This is Bazel's `ctx.delete("MODULE.bazel")` before the
  module download (`tools/build_defs/repo/utils.bzl:242-248`) and its
  repository-cache copy (`vfs/FileSystemUtils.java:402`, `REPLACE_EXISTING`). A fresh
  overlay download in Bazel (`bazel/repository/downloader/HttpDownloader.java:104`)
  could instead write through a link: native deliberately does not.
* Registry and local patches (`patching.rs`, also used for Git overrides) follow
  `PatchUtil.applyPatchToFile` (`bazel/repository/decompressor/PatchUtil.java:200-260`):
  the old file is read through links (:205-216), `oldFile.delete()` removes the
  path itself (:246-250) and a fresh regular file is written (:255-260). A patched
  link leaf therefore becomes a regular file with the patched bytes while its
  referent is unchanged, and patching a hard-linked file breaks the hard link (the
  other name keeps the old bytes), as the capture shows (B5/B6). Native reads
  through a leaf link only when it resolves inside the tree (an escaping leaf link
  is `unsafe patch symlink …`); symlinked ancestor directories remain rejected.
  When one patch edits both a link and its referent, native reads every input
  before writing, so the link's input is the original referent (Bazel applies
  sections in order on the filesystem).

## Consumer audit

| Consumer | Bazel 9.3 | Native decision |
| --- | --- | --- |
| Source artifacts (`engine.rs` source target resolution, action inputs) | Follows symlinks (`FileValue`); a dangling source link is a missing input file; a link cycle is reported as `circular symlinks detected`. | **Now follows**, uniformly for every repository (main, archive, Git, local): `source_links::resolve_in_root` resolves relative, absolute-inside-root, directory and chained links to a symlink-free path, which the action layer reads (its no-symlink checks still hold). A dangling link is `missing input file '<label>'`. A cycle produces Bazel's report (`io/AbstractFileChainUniquenessFunction.java:45-52`, `io/FileSymlinkCycleUniquenessFunction.java:64-82`, rotated to the smallest path as in `canonicalize` :63-77): `circular symlinks detected` / `[start of symlink cycle]` / the link paths / `[end of symlink cycle]`. Since N26 a link leaving its repository is followed into another repository materialized in this invocation (`resolve_across`, see "Cross-repository links" below); a missing target outside the root is `missing input file`, as in Bazel (the A2 `escape.txt -> ../../outside.txt` probe); an existing target outside every materialized repository is an explicit error (`outside its repository root`) where Bazel would read it. Package-boundary checks stay on the logical label path. |
| `glob()` (`glob.rs`, N14 policy) | Follows file and directory symlinks (`skyframe/GlobFunctionWithMultipleRecursiveFunctions.java:207-288`); dangling links are skipped. | **Unchanged**: "symlinks are never followed"; a potentially matching link is `glob could match unsupported symlink`. No inspected archive needs more: `aspect_bazel_lib` links are under `.bazelignore`d `e2e/` or in `.aspect/bazelrc/local/`, which `glob(["*.bazelrc", "*.md"])` in `.aspect/bazelrc` cannot match; `bazel_lib` and `tar.bzl` links are all under ignored `e2e/`. |
| `.bzl` loads (`loading.rs`) | Follows links (the `.bzl` FileValue); BzlCompileFunction.java:109-136 reports `cannot load 'L': no such file`, `cannot load 'L': is a directory`. | **Follows since N26** with the cross-repository policy below: package boundaries are checked on the logical label path, then `source_links::resolve_across` supplies the content (leaf or directory links, relative or absolute). Missing targets, directories and cycles report Bazel's texts (`cannot load '//lib:x.bzl': no such file`, `... is a directory`, `circular symlinks detected ...`). The label keeps its logical identity for relative loads, repository mapping and errors; only the bytes come from the resolved file (and are hashed in load provenance). |
| `BUILD`/`REPO.bazel`, package discovery (`workspace.rs`) | Follows links; `//...` traverses directory links. | Unchanged: symlinked package directories are explicit errors; internal `BUILD` file links are accepted; recursive patterns do not descend into directory links. |
| `module_ctx.path/read`, `repository_ctx` templates/labels (`extension_io.rs`, `repository_ctx.rs`) | Follow links. | Unchanged explicit symlink errors (docs/MODULE_SOURCE_INPUTS.md, docs/REPOSITORY_RULES.md). |
| Repository-rule publication (`repository_store.rs`) | — | Unchanged: repository rules cannot create links natively, and published trees remain regular files/directories only. |
| Git sources (`git_source.rs`, `repository_links.rs`) | — | Unchanged validated profile (docs/GIT_SYMLINKS.md); their internal file links now also build as source artifacts, and patched link leaves become regular files. `scripts/validate_git_links.py` was updated accordingly (consumer case: `@gitdep//artifact:selected` must build with the bytes of `data`, compared with genuine Bazel; `native.patch-symlink-leaf` must succeed with a regular patched leaf). `artifacts/git-links-validation.json` predates this and must be regenerated. |

## Cross-repository links (N26)

Reads of source artifacts and `.bzl` loads follow a link that leaves its
repository only into the canonical root of a repository already materialized in
the current invocation's repository set (`RepositorySet::materialized_roots`:
main, `local_path` modules of the resolved graph, published registry/Git/built-in
trees and generated repositories whose demand completed). The target is matched
by longest path prefix; resolution then continues inside that root with the same
40-hop budget and cross-root cycle detection. No repository is fetched by this
lookup. The repository store (stale trees, `.stage-*`, `.trash-*`) and the whole
`--cache-dir` root (actions, downloads) are never claimed by a shorter root that
contains them, and arbitrary host paths remain the
explicit `outside its repository root` error. This covers rules_cc's
`local_config_cc` (`unix_cc_configure.bzl:362-370` links `cc_toolchain_config.bzl`
to `@rules_cc//cc/private/toolchain:...`, whose root the callback just demanded).
Patches, writes and globs keep refusing links outside the tree.

## Remaining documented differences

The paired capture's remaining classes are documented rather than fixed here:
query label form (`@@repo+//:t` native vs `@repo//:t` apparent in Bazel), the
CLI report for source-only targets (Bazel prints `(nothing to build)`, native
lists the source path), PAX/GNU long link targets, hard links to directories,
glob through links, links escaping a repository (native refuses to follow, also
for `.bzl` loads), the link-below-link layout, and the wording of the nonempty-directory
failure. `repository_ctx.download_and_extract` does not exist natively.

## Tests

`archive.rs` unit tests cover, for `tar`, `tar.gz`, `tgz` and `zip`: relative,
nested, directory, chained-prefix, absolute and root targets and a link before its
referent; tar hard links (inode identity, absolute targets, self links,
replacement) and their escape/missing (Bazel text)/directory errors; preserved
escaping tar links versus the verbatim zip error, including the stripped-path case;
write-through attacks (link then child, child then link, link below link, hard link
through a pending link) leaving an outside directory untouched; tar links replacing
files and empty directories versus zip links failing with `<path> is not a symlink`
(including same-name zip records); later regular entries winning in tar and zip,
in place through a hard link; empty, oversized and non-UTF-8 targets.
`materializer.rs` covers literal manifest records, MODULE/overlay replacement of
links, warm reuse without following dangling/external links and repair of
retargeted or copied links. `patching.rs` and `git_source.rs` cover patched link
leaves replaced by regular files, deletion of a link, dangling leaves and broken
hard links. `source_links.rs` covers resolution, escapes, Bazel's cycle report
(rotation) and missing targets. `tests/archive_symlinks.rs` builds a local file
registry whose `tar.gz` module source has symlinks, a hard link, a directory link,
dangling, escaping and `e2e`-style links, then queries the module and builds
genrules (in the module and in the main repository) reading files only through
links, warm-cached, plus the dangling (`missing input file`), escaping and glob
errors.
