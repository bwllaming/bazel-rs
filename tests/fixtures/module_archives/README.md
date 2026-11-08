# Genuine module source archives for offline tests

Release archives of builtin-closure modules that the tests' builds
materialize from the offline genuine registry
(`tests/fixtures/builtin_modules/registry`, which carries no source
archives). `tests/support/module_archives.rs` seeds them into a test's
repository download cache (`<cache dir>/repositories/downloads/sha256/<hex>`)
before the build, so bazel-rs verifies and uses them instead of downloading
from GitHub, exactly like Bazel's repository cache. The helper checks each
file against the `integrity` of the module's `source.json` in that registry
(the pinned genuine capture) before seeding it, so a changed byte fails the
test instead of reaching the cache.

The helper also seeds the registry's local patches and overlays, verifying
each against its own `source.json` integrity. This matters when fixture
commands use `--repository_disable_download`: a missing patch blob cannot be
copied even from a `file://` registry under that policy. `console_text` and
`distinct_inodes` seed `TOOLCHAIN_CLOSURE_ARCHIVES` and disable downloads during
their build/test commands, covering cold materialization (including genuine
registry patches) without a per-run network fallback. Their captured outputs
and module source/provenance files remain unchanged.

| module | file | source.json URL | sha256 | bytes |
|---|---|---|---|---|
| platforms 1.0.0 | `platforms-1.0.0.tar.gz` | https://github.com/bazelbuild/platforms/releases/download/1.0.0/platforms-1.0.0.tar.gz | `3384eb1c30762704fbe38e440204e114154086c8fc8a8c2e3e28441028c019a8` | 7,879 |
| rules_license 1.0.0 | `rules_license-1.0.0.tar.gz` | https://github.com/bazelbuild/rules_license/releases/download/1.0.0/rules_license-1.0.0.tar.gz | `26d4021f6898e23b82ef953078389dd49ac2b5618ac564ade4ef87cced147b38` | 35,903 |
| bazel_features 1.42.1 | `bazel_features-v1.42.1.tar.gz` | https://github.com/bazel-contrib/bazel_features/releases/download/v1.42.1/bazel_features-v1.42.1.tar.gz | `8189bac9a6bf9cc155a854c4cbebfebf58b9ca7a2d0a67645f7d0c1f83c523ac` | 18,078 |
| rules_python 1.7.0 | `rules_python-1.7.0.tar.gz` | https://github.com/bazel-contrib/rules_python/releases/download/1.7.0/rules_python-1.7.0.tar.gz | `f609f341d6e9090b981b3f45324d05a819fd7a5a56434f849c761971ce2c47da` | 1,007,915 |

bazel_skylib 1.8.2 is the already vendored
`tests/fixtures/build_settings/bazel-skylib-1.8.2.tar.gz` (sha256
`6e78f0e57de26801f6f564fa7c4a48dc8b36873e416257a92bbb0937eeac8446`, the
registry's pin as well).

Acquisition: copied from existing bazel-rs repository-cache blobs on the
development host whose SHA-256 equals the registry SRI (no download).

## The test archive mirror (not vendored)

The tests that resolve every registered toolchain of the full closure
also materialize apple_support 2.5.2, protobuf 33.4, rules_cc 0.2.17,
rules_java 9.1.0, rules_kotlin 1.9.6 (23 MB), rules_shell 0.6.1 and
rules_swift 3.1.2: about 30 MB, not vendored. `seed_module_archives` takes
them from a shared mirror instead, `<target dir>/tmp/module-archive-mirror/<sha256>`
(`CARGO_TARGET_TMPDIR`; `BAZEL_RS_TEST_ARCHIVE_MIRROR` names another
directory): a missing blob is downloaded ONCE with curl, verified against the
pinned integrity and renamed into place. The fill tries the source.json URL,
then the BCR mirrors (`https://bcr.cloudflaremirrors.com/<url without
scheme>`, `https://mirror.bazel.build/…`; GitHub's release downloads answer
504 at times), three rounds with backoff, one download per blob under
`<mirror>/<sha256>.lock` (concurrent test programs wait for it), and a test
process whose fill failed fails its later uses at once. Each test process
verifies a mirror blob once and hard-links (or copies) it into the test's
download cache, so bazel-rs never reaches the network in these tests. Offline
with an empty mirror the test fails before bazel-rs runs, naming the
archive, every URL tried and the sha256 to put there. Each new target
directory (a worker's `CARGO_TARGET_DIR`) fills its mirror on its first full
suite run (~30 MB); copy a filled mirror directory to skip that.

## No test reaches the network

The default module profile injects `@bazel_tools` and its MODULE closure
(platforms, rules_cc, rules_shell, rules_license, …), which resolve through
the selected registries: without `--registry`, https://bcr.bazel.build/
registry files and GitHub source archives on every run. Every test that
builds with the built-in modules therefore uses the offline registry and a
seeded download cache (`tests/support/module_archives.rs`):

- `with_builtin_registry(args)` adds `--registry=file://…/builtin_modules/registry`
  after the command word of `build`, `test`, `run`, `query`, `cquery`,
  `aquery`, `mod`, `fetch` and `coverage` (not `help`, `version`, `info`,
  `clean` or an unknown command, whose exit codes and texts stay the same);
- `Command::seeded_archives()` (chained after `current_dir`, before
  `.output()`) seeds every archive of the built-in closure
  (`TOOLCHAIN_CLOSURE_ARCHIVES`) into the command's `--cache-dir` whenever
  the command line selects that registry.

Check: with the mirror filled, the full suite passes with the network blocked
(`HTTPS_PROXY=http://127.0.0.1:9 HTTP_PROXY=http://127.0.0.1:9
NO_PROXY=127.0.0.1,localhost,::1` and the lowercase spellings; harnesses
that clear the environment were checked with a logging build of
`download_proxy::check`). Requests that remain by design: `rc_options`'
proxy tests name `registry.n40.invalid` / `moved.n40.invalid` (RFC 2606
names that never resolve; the requests go to loopback proxies or fail).
The one test of the real BCR, `mod_command::mod_tidy_matches_genuine_bazel`
(buildozer from the BCR and GitHub), is `#[ignore]`d: run it explicitly with
`--ignored`.
