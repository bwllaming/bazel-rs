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
(attr_dicts_computed_defaults::skylib_unittest_loads_with_analysis_test_transition_and_fails_analysis_explicitly,
execution_info's three, go_transitions::coverage_instrumentation_is_an_explicit_gap)
also materialize apple_support 2.5.2, protobuf 33.4, rules_cc 0.2.17,
rules_java 9.1.0, rules_kotlin 1.9.6 (23 MB), rules_shell 0.6.1 and
rules_swift 3.1.2: about 30 MB, not vendored. `seed_module_archives` takes
them from a shared mirror instead, `<target dir>/tmp/module-archive-mirror/<sha256>`
(`CARGO_TARGET_TMPDIR`; `BAZEL_RS_TEST_ARCHIVE_MIRROR` names another
directory): a missing blob is downloaded ONCE with curl from its source.json
URL, verified against the pinned integrity and renamed into place; every use
verifies it again and hard-links (or copies) it into the test's download
cache, so bazel-rs never reaches the network in these tests. Offline with an
empty mirror the test fails before bazel-rs runs, naming the archive, URL and
sha256 to put there. Each new target directory (a worker's
`CARGO_TARGET_DIR`) fills its mirror on its first full suite run (~30 MB);
copy a filled mirror directory to skip that.
