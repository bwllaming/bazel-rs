# Releasing bazel-rs

Binary releases are GitHub releases of
[bwllaming/bazel-rs](https://github.com/bwllaming/bazel-rs) (private), tagged
`v<version>` with the version of `Cargo.toml`. bazel-rs is licensed under
Apache-2.0 (`LICENSE`, `NOTICE`).

## Cutting a release

```sh
git switch main && git pull --ff-only     # the release commit must already be on origin/main
scripts/release.py --dry-run               # optional rehearsal: build, smoke test, archive, notes
scripts/release.py                         # tag v<version>, push the tag, create the release
```

`scripts/release.py [--version X.Y.Z] [--dry-run] [--notes FILE] [--allow-dirty-for-test]`
(Python 3.11+, standard library only; `--help` lists the rest):

1. **Preflight** (all problems are listed at once): on `main`; clean working
   tree; after `git fetch origin`, HEAD is `origin/main` — the script never
   pushes a branch, push `main` yourself; `Cargo.toml` and `Cargo.lock` at HEAD
   say the release version; tag `v<version>` exists neither locally nor on
   `origin`; `gh` is authenticated and the repository has no release with that
   tag. The version defaults to `Cargo.toml`'s. `gh` is `--gh PATH`, else `$GH`,
   else `gh` on PATH.
2. **Build** the commit in a fresh detached `git worktree` in a temporary work
   directory (`--work-dir` to choose it), never in the caller's tree, with its
   own `CARGO_TARGET_DIR`: `cargo build --locked --release`. The toolchain comes
   from that commit's `rust-toolchain.toml`; `RUSTUP_HOME`/`CARGO_HOME` are
   honoured and default to the checkout's `.direnv/{rustup,cargo}` like
   `.envrc`. `RUSTFLAGS`, `RUSTUP_TOOLCHAIN`, `RUSTC*` wrappers and target-dir
   variables of the caller are dropped; `RUSTFLAGS` only remaps the worktree,
   `CARGO_HOME` and `RUSTUP_HOME` paths (`/bazel-rs`, `/cargo`, `/rustup`), so
   the binary does not depend on where it was built, and `SOURCE_DATE_EPOCH` is
   the commit time.
3. **Package** `bazel-rs-<version>-x86_64-unknown-linux-gnu.tar.gz` with
   `bazel-rs-<version>-x86_64-unknown-linux-gnu/{bazel-rs, LICENSE, NOTICE,
   README.md, THIRD_PARTY_LICENSES.md}`. The tarball is deterministic: entries
   sorted, mtime = commit time, uid/gid 0 (`root`), modes 0755/0644, ustar;
   gzip level 9 with header mtime 0 and no file name. `THIRD_PARTY_LICENSES.md`
   is generated (not committed) from `cargo metadata --locked` (normal and build
   dependencies for the target: crate, version, license expression, repository,
   and every license file of the crate, identical texts printed once) plus the
   legal files of the embedded Bazel tools bundle (Bazel's `LICENSE`, the JDK's
   `legal/` notices). Next to the archive: `<archive>.sha256` and `SHA256SUMS`
   (`sha256sum -c` format).
4. **Smoke test** the binary extracted from the archive, in a scratch
   workspace with a scratch HOME, cache and TMPDIR and a dead HTTP(S) proxy (no
   network): `--version` (must print the release version), `version`, `help`,
   then `query //:all` and `build //:upper` twice (the second all cache hits)
   with the explicit-module profile (`--experimental-builtin-modules=false`),
   checking the output file.
5. **Release notes** (`<work>/RELEASE_NOTES.md`): version, commit, compatibility
   target (from `--version`), platform and minimum glibc (highest `GLIBC_x.y`
   symbol version of the binary, `objdump -T` or `readelf -V`), toolchain,
   archive sha256, requirements, install instructions, then the changes: for
   the first release the "Status" section of `PLAN.md`; afterwards `git log
   --no-merges <previous tag>..<commit>` grouped by subject prefix (`feat`,
   `parity`, `perf`, `fix`/`harden`, `docs`, `test`, everything else under
   "Other"; `release:` commits are left out). `--notes FILE` is prepended;
   with `--replace-changelog` it replaces the generated changelog.
6. **Publish** (not with `--dry-run`, which prints the commands instead and
   exits 1 if the preflight found problems): preflight is repeated (the build
   takes minutes), then `git tag -a v<version>` at the built commit, `git push
   origin refs/tags/v<version>` (only the tag), `gh release create v<version>
   --verify-tag --title "bazel-rs <version>" --notes-file … <archive,
   .sha256, SHA256SUMS>`, then `gh release view` checks the assets and `gh
   release download` re-fetches the archive and compares its sha256.

**Prereleases.** 0.x versions and `X.Y.Z-pre` versions are published as GitHub
prereleases (bazel-rs is not at Bazel parity; GitHub then never shows them as
"latest"). `--latest` publishes them as a full release marked latest; 1.0 and
later are always full releases.

**Failure after tagging.** Nothing is rolled back automatically. The script
prints the exact commands to undo what was done so far, e.g.
`gh release delete v0.1.0 --repo bwllaming/bazel-rs --yes`,
`git push origin :refs/tags/v0.1.0`, `git tag -d v0.1.0` (or `gh release upload
… --clobber` to keep the release and replace its assets).

The work directory is kept (archive, checksums, notes, the smoke workspace);
the temporary worktree is removed and so is its target directory unless
`--keep-build`. `--allow-dirty-for-test` only skips the clean-tree check (the
build uses the commit, never uncommitted changes).

## Preparing the next version

```sh
scripts/bump_version.py 0.2.0      # --dry-run shows what it would change
cargo build --locked && git commit -am 'release: bump version to 0.2.0'
```

`bump_version.py` rewrites `Cargo.toml`'s `[package] version`, the root
`bazel-rs` entry of `Cargo.lock`, and the test files that assert the native
`version` text literally (`tests/fixtures/module_extension_io/native_capture.py`,
`tests/test_validate_label_passthrough.py`). Everything else derives from
`CARGO_PKG_VERSION` (`version`, `info release`, user agents, snapshot keys).
It then lists the remaining mentions of the old version: captured receipts
of earlier binaries under `tests/fixtures/` and the `docs/PROFILE.md` example,
which stay as recorded. The registry client's user agent `bazel-rs/0.1`
(`src/registry.rs`) is a separate fixed string.

## Tests

`python3 -m unittest tests.test_release` covers version parsing, the preflight
with fake git/gh runners, byte-identical tarballs, checksums, the license
inventory, release notes and `bump_version.py`, all offline.
