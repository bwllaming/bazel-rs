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
development host whose SHA-256 equals the registry SRI (no download). The
other modules of the builtin closure are not vendored; the tests that still
materialize them (they resolve every registered toolchain of the full
closure, which needs rules_kotlin 1.9.6 at 23 MB and protobuf 33.4 at 6.9 MB
among others) are listed in the deflake report.
