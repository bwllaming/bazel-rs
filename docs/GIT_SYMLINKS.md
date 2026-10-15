# Preserved internal file symlinks in Git sources

This increment preserves native symlink entries in published Git source trees.
It does not omit unselected links or replace them with regular-file copies.
The motivating pinned repository contains
`e2e/smoke/.bazelversion -> ../../.bazelversion`; its exact target spelling and
19-byte target are recorded in `artifacts/rules-distroless-symlink-inventory.json`.

## Supported boundary

Links must be relative and resolve to an existing regular file inside the
published repository. Leaf-link chains are supported up to 40 hops, matching
Linux's file-resolution limit. Absolute/external links, directory targets,
dangling links, cycles and unsafe target spellings are rejected explicitly.
Root `MODULE.bazel` remains regular-file-only.

Targets may contain `.` and `..`, but validation walks their components rather
than merely collapsing them lexically. For example, `missing/../file` and
`file/../file` do not become valid just because their normalized spelling names
an existing file. Intermediate components must be real directories, not files
or links. The validator uses the complete file/link inventory and its implicit
parent directories; empty intermediate directories are outside this profile.

The literal target string is preserved. Names/targets are limited to 4096 bytes,
path depth to 64 components, and raw targets to 256 components. Non-UTF-8 names
and targets, control characters, backslashes, colons, question marks, hashes,
percent signs and empty target components are unsupported. These are explicit
subset limits, not full Git/filesystem symlink compatibility.

Committed symlink blobs are checked before checkout. Git can truncate malformed
blob contents at a NUL byte when creating a link, so checkout `read_link` alone
is not sufficient evidence of the committed target. Export requires equality
between the validated committed target and the native checkout target.
The Git tree cache policy is version 3, so trees produced before committed-blob
validation are not reused. Each link's size and content are read by separate
bounded Git commands sharing the existing 120-second fetch deadline; large link
sets can therefore hit the deadline rather than being silently skipped.

## Publication and cache integrity

The full checked-out inventory is validated before export, including links
outside a selected prefix. The selected namespace is checked again relative to
the published root; a link that escapes after stripping is rejected. Only a
private stage receives new links, using their original targets.

Manifest entries distinguish regular files, directories and symlinks. Link
entries record target spelling without following the link for hashing, chmod or
recursion; referent files have independent content hashes. Cold post-patch and
warm-cache validation apply the same safety checks. Retargeting a link to an
identical-content file is still a cache mismatch, as is replacing a link with a
regular file. Removing a referent through a patch cannot publish a dangling link.

Cache ancestors, locks and manifests must remain real files/directories, not
symlinks. The cache and private stage are trusted against hostile concurrent
mutation, as with the existing materializers; this is not an OS sandbox.

## Adjacent behavior remains restricted

Internal BUILD-file links already accepted by package loading can be preserved.
Selected `.bzl` links and relevant glob links remain controlled unsupported cases.
Since N19 (`docs/ARCHIVE_SYMLINKS.md`), source-artifact labels follow links that
resolve inside their repository, so a preserved internal file link builds; action
inputs are the resolved, symlink-free files.
Since N19, a patch targeting an internal link leaf follows Bazel's PatchUtil: the
link is read through and replaced by a regular patched file, leaving its referent
unchanged (links escaping the tree and symlinked ancestors still fail before
mutation). `artifacts/git-links-validation.json` predates both changes and must be
regenerated: `scripts/validate_git_links.py` now requires the
`native.patch-symlink-leaf` case to succeed with a regular patched leaf, and the
consumer case to build `@gitdep//artifact:selected` with the bytes of `data`,
compared with genuine Bazel. Directory links are not implemented by this change;
archive links are covered by `docs/ARCHIVE_SYMLINKS.md`. Full Bazel 9.3 and application build/test parity remain incomplete.

The initial read-only application probe preserved the required `rules_distroless`
link and advanced to the unmaterialized extension repository
`@io_bazel_rules_nogo`. It did not complete the application query. The probe's
checkout fingerprints remained unchanged and its private cache was removed;
evidence is in `artifacts/mainrepo-git-links-loading.json`.

## Verification

The reviewed increment passes 497 Rust tests and 240 Python tests, formatting,
strict Clippy, release compilation and error-level Rust Analyzer diagnostics.
Before N19, `artifacts/git-links-validation.json` passed four genuine Bazel
differentials and 15 native-only checks, including malformed committed-blob
rejection. N19 changed the artifact and patch-symlink-leaf phases, so that report
is stale until regenerated with the updated validator. Six adjacent
Git, patch, repository, module and declaration reports pass against the same
release binary. Reports retain exact link/output evidence, executable/source
hashes, successful reference shutdown and scratch removal. This approval covers
only the bounded profile described above.
