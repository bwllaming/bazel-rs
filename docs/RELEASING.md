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

`scripts/release.py [--version X.Y.Z] [--dry-run] [--notes FILE] [--target TRIPLE]...
[--allow-dirty-for-test]` (Python 3.11+, standard library only; `--help` lists the rest).
Besides Rust (`scripts/setup-rust.sh`; `rust-toolchain.toml` lists the musl
targets) the static builds need zig as their C compiler: run
`scripts/setup-zig.sh` once (pinned zig 0.16.0, sha256-checked, into
`.direnv/zig`), or pass `--zig PATH` / set `ZIG`.

### Targets

| Target | Default | Linking | Built with |
|---|---|---|---|
| `x86_64-unknown-linux-gnu` | yes | dynamic, glibc of the build host (≥ 2.39 when built on Ubuntu 24.04) | `cargo build --locked --release`, host `cc` |
| `x86_64-unknown-linux-musl` | yes | static-pie: musl libc, jemalloc, no shared library | `--target x86_64-unknown-linux-musl`, C code by `zig cc -target x86_64-linux-musl`, linked by the host `cc` with Rust's self-contained musl runtime |
| `aarch64-unknown-linux-musl` | no (`--target aarch64-unknown-linux-musl`) | static | as above with `zig cc -target aarch64-linux-musl`, linked by `rust-lld`, `JEMALLOC_SYS_WITH_LG_PAGE=16` (runs on 4/16/64 KiB page kernels) |

The static x86_64 binary is the portable one: it runs on any x86_64 Linux
(no glibc version requirement; Rust's minimum kernel 3.2). It is the same code
as the glibc binary with two musl-only differences (`Cargo.toml`, `build.rs`):
jemalloc also takes over the unprefixed `malloc` family, so musl's allocator
serves nothing, and `src/musl_string.c` replaces musl's byte-loop
`memcmp`/`bcmp` and `rep movsq` `memcpy`/`memmove` (linked as a whole archive
ahead of `libc.a`; `tests/libc_string.rs` checks them). Without the latter the
musl binary was ~25 % slower on analysis. Measured on `~/code`,
`build //src/beyondwork.ai/tools/oauth2token/...` with private caches,
alternating runs on a shared 64-core machine (seconds wall, peak RSS):

| | glibc | static musl |
|---|---|---|
| cold (fetch + 692 actions) | 83-124 | 94-118 |
| full analysis, all cache hits (`BAZEL_RS_SNAPSHOT=off`), 4 runs | 3.9-4.4, 735 MB | 3.2-4.9, 737 MB |
| warm no-op (analysis snapshot hit) | 0.2-0.3, 77 MB | 0.3, 113 MB |

The warm no-op's extra ~35 MB is jemalloc arena memory (it shrinks to ~8 MB
with `MALLOC_CONF=narenas:8`), not seen at the full-analysis peak.

The arm64 binary cross-compiles cleanly and runs a full build under QEMU
user emulation, but bazel-rs itself is Linux x86_64-only today: the embedded
`@bazel_tools` bundle and the repository host metadata (`ctx.os`) refuse other
hosts, so most real workspaces fail. It is therefore opt-in and its notes row
says so. Smoke-testing it needs a way to run arm64 binaries: natively, through
a binfmt_misc `qemu-aarch64` registration (full smoke test), or with
`--qemu-aarch64 PATH` (qemu-user, e.g. from the `qemu-user-static` package;
only `--version`, `version`, `help` and `query`, because actions are started
through `/proc/self/exe`, which plain qemu-user cannot exec). `--allow-untested`
ships it without a smoke test, labelled so in the notes.

### Steps


1. **Preflight** (all problems are listed at once): on `main`; clean working
   tree; after `git fetch origin`, HEAD is `origin/main` — the script never
   pushes a branch, push `main` yourself; `Cargo.toml` and `Cargo.lock` at HEAD
   say the release version; tag `v<version>` exists neither locally nor on
   `origin`; `gh` is authenticated and the repository has no release with that
   tag. The version defaults to `Cargo.toml`'s. `gh` is `--gh PATH`, else `$GH`,
   else `gh` on PATH.
2. **Build** the commit in a fresh detached `git worktree` in a temporary work
   directory (`--work-dir` to choose it; by default a new directory under
   `$TMPDIR`, or `~/opencodetmp` when `TMPDIR` is unset and that directory
   exists, else the system default), never in the caller's tree, with its
   own `CARGO_TARGET_DIR`, once per target (table above; the musl builds get
   generated `zig cc`/`zig ar`/`zig ranlib` wrappers in `<work>/tools`, a zig
   cache in `<work>/zig-cache`, and `rustup target add` first). The toolchain comes
   from that commit's `rust-toolchain.toml`; `RUSTUP_HOME`/`CARGO_HOME` are
   honoured and default to the checkout's `.direnv/{rustup,cargo}` like
   `.envrc`. `RUSTFLAGS`, `RUSTUP_TOOLCHAIN`, `RUSTC*` wrappers and target-dir
   variables of the caller are dropped; `RUSTFLAGS` only remaps the worktree,
   `CARGO_HOME` and `RUSTUP_HOME` paths (`/bazel-rs`, `/cargo`, `/rustup`), so
   the binary does not depend on where it was built, and `SOURCE_DATE_EPOCH` is
   the commit time. Each binary is checked: ELF machine of the target; the
   static targets must have no program interpreter and no `DT_NEEDED`
   entry; the glibc target's requirement is its highest `GLIBC_x.y` symbol
   version (`objdump -T` or `readelf -V`).
3. **Package** one `bazel-rs-<version>-<target>.tar.gz` per target with
   `bazel-rs-<version>-<target>/{bazel-rs, LICENSE, NOTICE, README.md,
   THIRD_PARTY_LICENSES.md}`. The tarball is deterministic: entries
   sorted, mtime = commit time, uid/gid 0 (`root`), modes 0755/0644, ustar;
   gzip level 9 with header mtime 0 and no file name. `THIRD_PARTY_LICENSES.md`
   is generated (not committed) from `cargo metadata --locked` (normal and build
   dependencies for the target: crate, version, license expression, repository,
   and every license file of the crate, identical texts printed once) plus the
   legal files of the embedded Bazel tools bundle (Bazel's `LICENSE`, the JDK's
   `legal/` notices); for the static targets also the statically linked C
   runtime (musl's `COPYRIGHT` and LLVM libunwind's licence, taken from the
   zig distribution). Next to each archive: `<archive>.sha256`, and one
   `SHA256SUMS` over all archives (`sha256sum -c` format).
4. **Smoke test** each binary extracted from its archive, in a scratch
   workspace with a scratch HOME, cache and TMPDIR and a dead HTTP(S) proxy (no
   network): `--version` (must print the release version), `version`, `help`,
   then `query //:all` and `build //:upper` twice (the second all cache hits)
   with the explicit-module profile (`--experimental-builtin-modules=false`),
   checking the output file (arm64: see Targets).
5. **Release notes** (`<work>/RELEASE_NOTES.md`): version, commit, compatibility
   target (from `--version`), toolchain; a downloads table (archive, platform,
   linking — static, or the minimum glibc —, how it was smoke-tested,
   sha256); requirements per archive (static: any distribution; glibc: the
   version, e.g. Ubuntu 24.04 or newer; arm64: the x86_64-only caveat);
   install instructions for the static x86_64 archive; then the changes: for
   the first release the "Status" section of `PLAN.md`; afterwards `git log
   --no-merges <previous tag>..<commit>` grouped by subject prefix (`feat`,
   `parity`, `perf`, `fix`/`harden`, `docs`, `test`, everything else under
   "Other"; `release:` commits are left out). `--notes FILE` is prepended;
   with `--replace-changelog` it replaces the generated changelog.
6. **Publish** (not with `--dry-run`, which prints the commands instead and
   exits 1 if the preflight found problems): preflight is repeated (the build
   takes minutes), then `git tag -a v<version>` at the built commit, `git push
   origin refs/tags/v<version>` (only the tag), `gh release create v<version>
   --verify-tag --title "bazel-rs <version>" --notes-file … <archives,
   .sha256 files, SHA256SUMS>`, then `gh release view` checks the assets and
   `gh release download` re-fetches every archive and compares its sha256.

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

`python3 -m unittest discover -s tests -p test_release.py` covers version
parsing, the preflight with fake git/gh runners, the target table, the zig
wrappers and cross-build environment, ELF linkage checks, smoke-test
planning, byte-identical tarballs, checksums, the license inventory (with the
static runtime section), release notes and `bump_version.py`, all offline.
`cargo test --locked --target x86_64-unknown-linux-musl --test libc_string`
(with the `CC_`/`AR_` variables release.py sets, or the whole suite that way)
runs the string-function checks against the musl build.
