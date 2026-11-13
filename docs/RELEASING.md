# Releasing bazel-rs

Binary releases are GitHub releases of
[bwllaming/bazel-rs](https://github.com/bwllaming/bazel-rs) (public: the
assets download without authentication), tagged
`v<version>` with the version of `Cargo.toml`. bazel-rs is licensed under
Apache-2.0 (`LICENSE`, `NOTICE`).

## Cutting a release

Releases are built and published by GitHub Actions
(`.github/workflows/release.yml`) when a `v*` tag is pushed. A developer
machine only checks, tags and pushes the tag:

```sh
git switch main && git pull --ff-only     # the release commit must already be on origin/main
scripts/release.py --dry-run               # optional rehearsal: build, smoke test, archive, notes locally
scripts/release.py [--notes FILE] --watch  # tag v<version>, push only the tag, follow the workflow run
```

`scripts/release.py [--version X.Y.Z] [--notes FILE [--replace-changelog]] [--watch]`,
`scripts/release.py --dry-run [--target TRIPLE]... [--allow-dirty-for-test] [--smoke-network]`,
`scripts/release.py --ci [--stage build --target TRIPLE... | --stage publish --from DIR]`
(the workflow) — Python 3.11+, standard library only;
`--help` lists the rest. The tag flow needs `git` and an authenticated `gh`
(`--gh PATH`, else `$GH`, else `gh` on PATH). `--dry-run` also needs Rust
(`scripts/setup-rust.sh`; `rust-toolchain.toml` lists the musl targets) and zig
as the static builds' C compiler: run `scripts/setup-zig.sh` once (pinned zig
0.16.0, sha256-checked, into `.direnv/zig`), or pass `--zig PATH` / set `ZIG`.

### The tag flow (default)

1. **Preflight** (all problems are listed at once; exit 2): on `main`; clean
   working tree (`--allow-dirty-for-test` skips this; only the commit is
   tagged); after `git fetch origin`, HEAD is `origin/main` — the script never
   pushes a branch, push `main` yourself; `Cargo.toml` and `Cargo.lock` at HEAD
   say the release version (default: `Cargo.toml`'s); tag `v<version>` exists
   neither locally nor on `origin`; `gh` is authenticated and the repository
   has no release with that tag.
2. **Tag**: `git tag -a --cleanup=verbatim -m <message> v<version> HEAD`. The
   message's subject is `bazel-rs <version>`; its body is the `--notes FILE`
   text (verbatim, so markdown `#` headings survive), preceded by the line
   `<!-- release.py: replace-changelog -->` with `--replace-changelog`. This
   is how the extra notes reach CI: `--ci` reads them back from the tag.
3. **Push only the tag**: `git push origin refs/tags/v<version>:refs/tags/v<version>`.
4. **Show the run**: polls `gh run list --workflow release.yml --event push
   --branch v<version>` (about a minute) for the run of the tagged commit and
   prints its URL and the `gh run watch` command. `--watch` waits with `gh run
   watch --exit-status` and prints the release URL (exit status of the run).

`--latest` is not passed to CI (the workflow publishes 0.x/-pre versions as
prereleases; promote one in the GitHub UI or with `gh release edit --latest`).
If tagging or pushing fails, the exact undo commands are printed
(`git tag -d v<version>`, `git push origin :refs/tags/v<version>`).

### The workflow (`--ci`)

`release.yml` runs on `push` of `v*` tags and on `workflow_dispatch` with a
`tag` input (re-runs: `gh workflow run release.yml -f tag=v<version>`; the
build always uses the tag's sources and its `scripts/release.py`; the
workflow definition comes from the ref the dispatch names, default `main`).
One concurrency group per tag (never cancelled). Two stages:

- **`build`**, a matrix with one job per runner — each target is built
  natively on a machine of its platform, so its smoke test runs the binary
  itself:

  | Matrix entry | Runner | Targets |
  |---|---|---|
  | `linux-x86_64` | `ubuntu-24.04` (GitHub-hosted x86_64; glibc 2.39, so the glibc archive needs glibc ≥ 2.39 — printed by the job's "Build machine" step) | `x86_64-unknown-linux-gnu`, `x86_64-unknown-linux-musl` |
  | `linux-arm64` | `ubuntu-24.04-arm` (GitHub-hosted arm64) | `aarch64-unknown-linux-musl` |

  `fail-fast`, timeout 75 minutes, `permissions: contents: read`: check out
  the tag with full history (`fetch-depth: 0`); install the toolchain of
  `rust-toolchain.toml` with the entry's targets (rustup and `gh` are
  preinstalled on the images); zig via `scripts/setup-zig.sh` (cached on the script's hash and
  the runner's architecture); `Swatinem/rust-cache` (keyed per matrix entry on
  `Cargo.lock`, `rust-toolchain.toml` and the rustc version); then
  `scripts/release.py --ci --stage build --smoke-network --target … --version
  ${TAG#v} --work-dir $RUNNER_TEMP/release` (`GH_TOKEN` only for the
  preflight's "no such release yet" check). Its `dist/` — the archives, their
  `.sha256` files and one build record each — becomes the artifact
  `release-build-<entry>` (14 days).
- **`publish`**, one job on `ubuntu-24.04` after every build job succeeded,
  the only one with `permissions: contents: write`, timeout 30 minutes: check
  out the tag (full history, for the changelog), download every
  `release-build-*` artifact and run `scripts/release.py --ci --stage publish
  --from $RUNNER_TEMP/built --version ${TAG#v} --work-dir
  $RUNNER_TEMP/release` with `GH_TOKEN=${{ github.token }}`; it needs no Rust.
  The archives, checksums and notes are kept as the artifact `release-<tag>`
  for 14 days (also on failure).

**Build records** (`<archive stem>.release.json`, schema 1): version, commit,
compatibility target, rustc, and the archive's notes row (triple, platform,
linkage, smoke test, sha256, …). The publish stage requires exactly one
record per target of `--target` (default: every default target), for this
version and commit, each next to its archive at the recorded sha256, and one
compatibility target; it copies the archives, writes `SHA256SUMS` and the
`.sha256` files again, and the notes. **Adding a platform** (e.g. macOS arm64
on `macos-14`): a row in `TARGETS` of `scripts/release.py` (its `runner`,
`cpu`, `bundle`, linkage rule) and an entry in the workflow matrix;
`tests/test_release.py` checks that the matrix builds every default target on
its runner exactly once.

`--ci` without `--stage` (`all`) still does both on one machine (the CI
rehearsal below; a host that cannot run arm64 binaries needs binfmt_misc or
`--qemu-aarch64`, see Targets).

`--ci` differs from the local flow only where it must:

- **CI preflight** (exit 2): `git fetch --force origin
  +refs/tags/v<version>:refs/tags/v<version>` (so the annotated tag object is
  present, whatever the checkout fetched); the tag exists and its commit is
  HEAD (detached; no branch or pushed-ness check); the checkout is clean (it
  is built in place); `Cargo.toml`/`Cargo.lock` say the version; `gh` works
  and no release exists yet. The extra notes are the tag message body
  (`git for-each-ref --format=%(contents:body)`); a lightweight tag has none.
  `--notes`/`--replace-changelog` are rejected with `--ci`.
- **Build in place**: the checkout itself into its `./target` (the cached
  directory), not a temporary worktree. Everything else — build, checks,
  packaging, smoke test, notes (steps below) — is the same code as `--dry-run`.
  Every stage runs the CI preflight (the publish stage too, before reading the
  records).
- **Notes** additionally get a `Built by` row linking the workflow run
  (`$GITHUB_SERVER_URL/$GITHUB_REPOSITORY/actions/runs/$GITHUB_RUN_ID`, plus
  `/attempts/N` on re-runs).
- **Publish**: `gh release create v<version> --verify-tag --title "bazel-rs
  <version>" --notes-file … --prerelease|--latest <archives, .sha256 files,
  SHA256SUMS>` (never creates or pushes a tag), `gh release view` checks the
  assets, `gh release download` re-fetches every archive and compares its
  sha256. On failure the tag is kept and the script prints `gh release
  delete v<version> --yes` (if the release was created), `gh release upload
  --clobber` and the re-run command.

`--ci --dry-run` stops before publishing (prints the `gh release create`
command): a CI rehearsal in a scratch clone (see Tests).

### Targets

| Target | Default | Linking | Built with |
|---|---|---|---|
| `x86_64-unknown-linux-gnu` | yes | dynamic, glibc of the build host: the release is built on `ubuntu-24.04`, so it needs glibc ≥ 2.39 (Ubuntu 24.04, Debian 13 or newer); no glibc version suffix is pinned | `cargo build --locked --release`, host `cc` |
| `x86_64-unknown-linux-musl` | yes | static-pie: musl libc, jemalloc, no shared library | `--target x86_64-unknown-linux-musl`, C code by `zig cc -target x86_64-linux-musl`, linked by the host `cc` with Rust's self-contained musl runtime |
| `aarch64-unknown-linux-musl` | yes | static | as above with `zig cc -target aarch64-linux-musl`, linked by `rust-lld`, `JEMALLOC_SYS_WITH_LG_PAGE=16` (runs on 4/16/64 KiB page kernels); in CI natively on `ubuntu-24.04-arm`, locally also cross-built on x86_64 |

The static x86_64 binary is the portable one: it runs on any x86_64 Linux
(no glibc version requirement; Rust's minimum kernel 3.2). It is the same code
as the glibc binary with three musl-only differences (`Cargo.toml`,
`build.rs`): jemalloc also takes over the unprefixed `malloc` family, so
musl's allocator serves nothing; `src/musl_string.c` replaces musl's
byte-loop `memcmp`/`bcmp` and `rep movsq` `memcpy`/`memmove` (without it the
musl binary was ~25 % slower on analysis); and it provides `strerror_r` with
glibc's texts, because Bazel's messages embed them (`(Too many levels of
symbolic links)`, where musl says `Symbolic link loop`). The file is linked as
a whole archive ahead of `libc.a`; `tests/libc_string.rs` checks it. Other
libc differences that remain: user lookups read `/etc/passwd` only (no NSS
modules such as LDAP/sssd; bazel-rs uses `$HOME`), and host names resolve
through musl's stub resolver (`/etc/hosts`, `/etc/resolv.conf`; no
`nsswitch.conf`).

Tests against the musl build: `cargo test --target x86_64-unknown-linux-musl`
(with the `CC_`/`AR_`/`RANLIB_` variables release.py sets) builds the test
programs static against musl. Run once for 0.2.0: the library unit tests plus
82 integration programs (the CI subset and the execution, sandbox, process
cleanup, remote cache, clean/fetch and `bazel test` programs), 2249 tests, all
passing; the whole suite did not fit the build machine's memory at once (every
musl test program is a static ~100 MB link). Measured on `~/code`,
`build //src/beyondwork.ai/tools/oauth2token/...` with private caches,
alternating runs on a shared 64-core machine (seconds wall, peak RSS):

| | glibc | static musl |
|---|---|---|
| cold (fetch + 692 actions) | 83-124 | 94-118 |
| full analysis, all cache hits (`BAZEL_RS_SNAPSHOT=off`), 4 runs | 3.9-4.4, 735 MB | 3.2-4.9, 737 MB |
| warm no-op (analysis snapshot hit) | 0.2-0.3, 77 MB | 0.3, 113 MB |

The warm no-op's extra ~35 MB is jemalloc arena memory (it shrinks to ~8 MB
with `MALLOC_CONF=narenas:8`), not seen at the full-analysis peak.

**Linux arm64.** The arm64 binary embeds Bazel 9.3.1's linux-arm64
`@bazel_tools` (`resources/bazel_tools/linux-arm64/`, its JDK and compiled
helpers) and reports the host like genuine Bazel on arm64 (`ctx.os.arch`
`aarch64`, `@platforms//cpu:aarch64`, `bazel-out/aarch64-fastbuild`;
docs/HOST_OS.md). Its minimum kernel is Rust's 4.1. Smoke-testing it needs a
way to run arm64 binaries: natively (the CI's `ubuntu-24.04-arm` job), through
a binfmt_misc `qemu-aarch64` registration (full smoke test), or with
`--qemu-aarch64 PATH` (qemu-user, e.g. from the `qemu-user-static` package;
only `--version`, `version`, `help` and `query`, because actions are started
through `/proc/self/exe`, which plain qemu-user cannot exec). `--allow-untested`
ships it without a smoke test, labelled so in the notes.

Without root, a private binfmt_misc registration works in a user+mount
namespace (Linux 6.7 or newer; on Ubuntu 24.04 with
`kernel.apparmor_restrict_unprivileged_userns=1` the namespace has to be
created through `sudo`): mount `binfmt_misc` on `/proc/sys/fs/binfmt_misc`
inside it and register `:qemu-aarch64:M::<ELF aarch64 magic>:<mask>:/path/qemu-aarch64-static:F`;
the host's registrations stay untouched. Genuine `bazel-9.3.0-linux-arm64`
(a glibc launcher plus an arm64 JVM) also runs that way when qemu gets an
arm64 glibc sysroot (`QEMU_LD_PREFIX`, e.g. the `libc6`, `libstdc++6`,
`libgcc-s1` and `zlib1g` arm64 packages unpacked, with a binfmt interpreter
wrapper that sets it so the server's actions inherit it) — slowly (a tiny
build: ~2 minutes) but enough for paired captures (docs/HOST_OS.md).

### Build steps (`--dry-run` and `--ci`)

1. **Build** the commit — with `--dry-run` in a fresh detached `git worktree`
   in a temporary work directory (`--work-dir` to choose it; by default a new
   directory under `$TMPDIR`, or `~/opencodetmp` when `TMPDIR` is unset and
   that directory exists, else the system default), never in the caller's
   tree, with its own `CARGO_TARGET_DIR`; with `--ci` the checkout in place —
   once per target (table above; the musl builds get generated `zig cc`/`zig
   ar`/`zig ranlib` wrappers in `<work>/tools`, a zig cache in
   `<work>/zig-cache`, and `rustup target add` first). The toolchain comes
   from that commit's `rust-toolchain.toml`; `RUSTUP_HOME`/`CARGO_HOME` are
   honoured and default to the checkout's `.direnv/{rustup,cargo}` like
   `.envrc`. `RUSTFLAGS`, `RUSTUP_TOOLCHAIN`, `RUSTC*` wrappers and target-dir
   variables of the caller are dropped; `RUSTFLAGS` only remaps the source,
   `CARGO_HOME` and `RUSTUP_HOME` paths (`/bazel-rs`, `/cargo`, `/rustup`), so
   the binary does not depend on where it was built, and `SOURCE_DATE_EPOCH` is
   the commit time. Each binary is checked: ELF machine of the target; the
   static targets must have no program interpreter and no `DT_NEEDED`
   entry; the glibc target's requirement is its highest `GLIBC_x.y` symbol
   version (`objdump -T` or `readelf -V`).
2. **Package** one `bazel-rs-<version>-<target>.tar.gz` per target with
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
3. **Smoke test** each binary extracted from its archive, in a scratch
   workspace with a scratch HOME, cache and TMPDIR and a dead HTTP(S) proxy (no
   network): `--version` (must print the release version), `version`, `help`,
   then `query //:all` and `build //:upper` twice (the second all cache hits)
   with the explicit-module profile (`--experimental-builtin-modules=false`),
   checking the output file (arm64: see Targets). With `--smoke-network` (the
   workflow passes it) a second workspace builds in the default built-in
   profile with network (`bazel_dep(name = "platforms", version = "1.0.0")`
   from the Bazel Central Registry): it materializes the binary's embedded
   `@bazel_tools` and resolves the default host platform; its genrule's
   `$(TARGET_CPU)` and `@platforms//host`'s `HOST_CONSTRAINTS` must be
   `<cpu> @platforms//cpu:<cpu>,@platforms//os:linux` for the target's CPU
   (`x86_64`, `aarch64`).
4. **Release notes** (`<work>/RELEASE_NOTES.md`): version, commit, compatibility
   target (from `--version`), toolchain, (CI) the workflow run; a downloads
   table (archive, platform, linking — static, or the minimum glibc —, how it
   was smoke-tested, sha256); requirements per archive (static: any
   distribution of the platform with Rust's minimum kernel — 3.2 on x86_64,
   4.1 on arm64; glibc: the version, e.g. Ubuntu 24.04 or newer); install
   instructions (unauthenticated `curl` from the public download URL, then
   `sha256sum -c`) for the static archive of the reader's machine
   (`bazel-rs-<version>-$(uname -m)-unknown-linux-musl`, or the one static
   archive's name);
   then the changes: for the first release the "Status" section of `PLAN.md`;
   afterwards `git log --no-merges <previous tag>..<commit>` grouped by
   subject prefix (`feat`, `parity`, `perf`, `fix`/`harden`, `docs`, `test`,
   everything else under "Other"; `release:` commits are left out). The extra
   notes (`--notes FILE` with `--dry-run`, the tag message body with `--ci`)
   are prepended; with `--replace-changelog` (or its marker in the tag) they
   replace the generated changelog.
5. `--dry-run` then prints what a real release would do and exits (1 if the
   preflight found problems); `--ci` publishes (above).

**Prereleases.** 0.x versions and `X.Y.Z-pre` versions are published as GitHub
prereleases (bazel-rs is not at Bazel parity; GitHub then never shows them as
"latest"). `--ci --latest` would publish them as a full release marked latest
(the workflow does not pass it; promote a published prerelease with `gh release
edit v<version> --prerelease=false --latest`); 1.0 and later are always full
releases.

The `--dry-run` work directory is kept (archive, checksums, notes, the smoke
workspace); the temporary worktree is removed and so is its target directory
unless `--keep-build`. `--allow-dirty-for-test` only skips the clean-tree check
(the build and the tag use the commit, never uncommitted changes).

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
parsing, the preflight and the CI preflight with fake git/gh runners, the tag
flow's command sequence (annotated tag, push of only the tag, run lookup,
`--watch`, recovery output), the tag-message notes round trip through real
git (a temporary bare `origin`), the run URL, the workflow's key settings and
its build matrix (every default target exactly once, on its `runner`), the
build records and the publish stage's checks and output, the network smoke
step, the target table (every target's tools bundle is checked in), the zig
wrappers and cross-build environment, ELF linkage checks, smoke-test
planning, byte-identical tarballs, checksums, the license inventory (with the
static runtime section), release notes and `bump_version.py`, all offline.
`cargo test --locked --target x86_64-unknown-linux-musl --test libc_string`
(with the `CC_`/`AR_` variables release.py sets, or the whole suite that way)
runs the string-function checks against the musl build.

CI rehearsal of `--ci` without publishing: in a scratch clone whose `origin`
is a local bare repository holding an annotated `v<version>` tag, check out
the tag detached and run `GITHUB_SERVER_URL=https://github.com
GITHUB_REPOSITORY=<owner/name> GITHUB_RUN_ID=1 scripts/release.py --ci
--dry-run --version <version> --work-dir <empty dir>`: it runs the CI
preflight, builds in place, smoke-tests, packages and writes the notes (with
the run link and the tag's notes), then prints the `gh release create`
command instead of running it. The staged flow rehearses the same way:
`--ci --dry-run --stage build --target …` per runner (a different
`--work-dir` each), then `--ci --dry-run --stage publish --from <dir holding
their dist/ directories> --work-dir <empty dir>`. Workflow lint: `actionlint`
(1.7.12, with `shellcheck` on PATH) over `.github/workflows/`.
