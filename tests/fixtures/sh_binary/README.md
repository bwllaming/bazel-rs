# Genuine rules_shell 0.6.1 rule sources (N50)

`rules_shell/` is a local module for `tests/sh_binary.rs`. These files are
**byte-identical** to `rules_shell-v0.6.1.tar.gz` (BCR
`modules/rules_shell/0.6.1/source.json`, SRI
`sha256-5rh8ib0LJwOeOvLF2gEUdFLyQPddUF9baICHTzEDYwc=`, archive SHA256
`e6b87c89bd0b27039e3af2c5da01147452f240f75d505f5b6880874f31036307`, verified
by extracting the archive from a genuine Bazel repository-cache blob), under
`strip_prefix = rules_shell-0.6.1` (Apache-2.0, `rules_shell/LICENSE`):

| file | SHA256 |
| --- | --- |
| LICENSE | cfc7749b96f63bd31c3c42b5c471bf756814053e847c10f3eb003417bc523d30 |
| shell/sh_binary.bzl | ab8ff142abbefef817b9cf1c311f3be15dc361b5b4b2a8320c340eb2e9aeeef3 |
| shell/sh_library.bzl | 8efba626ddd0721869ffff6f81a149cea3a5180a17e66c5e098eb1030099eba0 |
| shell/sh_test.bzl | 96c9759b995baeafea43eb8361832827cdedfa320c079c708864bbf6182ab7a8 |
| shell/private/sh_executable.bzl | 73b1fbd8278a7702c012830868293a8cbcf86c274841d16e7a074b200d8be39f |
| shell/private/sh_binary.bzl | eb9765a5a82092410e843fa6ccc6c7cad13dd3f7094ae22cc7a8313183ac870b |
| shell/private/sh_library.bzl | 0c524048ecfaadfdf89695c3fee055010a25820562d3441541deaa1471b76539 |
| shell/private/sh_test.bzl | 338e74a7c382c69ca1aa2e3047563d792d0cd6d50898b2d899854e2220fada68 |
| shell/private/root_symlinks.bzl | c013e80a69c337c6b068a02b3645516da75ab7a2e5178e9803e4a9a8f978338d |
| shell/runfiles/BUILD | 2114531f9e3667746fd493de6f6c9969138761163111d4ad1827c544875a5e1b |
| shell/runfiles/runfiles.bash | c66029f99768bc24eb0e65a9120d321a6982221e554f5720a2268885dc4edf76 |
| shell/toolchains/sh_toolchain.bzl | 22495ba926ec580860d4d56054a16a96ec4c8f844717190c4c35b08ee46e0591 |

Four files are offline replacements, each marked in its header: `MODULE.bazel`
(no bazel_features/bazel_skylib/sh_configure), `shell/BUILD` and
`shell/private/BUILD` (no skylib `bzl_library`), and `shell/toolchains/BUILD`
(the Linux toolchain `sh_config` would generate, path `/bin/bash`). The rule
implementations exercised by the test (sh_binary, sh_test, sh_library,
root_symlinks with `attr.label_keyed_string_dict`, `flags =
["SKIP_CONSTRAINTS_OVERRIDE"]`, `allow_rules = ["sh_library"]`,
`coverage_common.instrumented_files_info`, `skip_conflict_checking`) are the
genuine ones. The paired capture against genuine Bazel 9.3.0 with the genuine
module from the builtin registry is `reference_receipt.json`.
