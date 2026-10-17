# N30 test_flags fixture

`stubs/` are minimal local modules that a root module installs with
`local_path_override` for the toolchain-registering and heavy modules of the
genuine Bazel 9.3 `@bazel_tools` closure (rules_java, rules_cc, rules_python,
rules_shell, protobuf, apple_support, zlib, buildozer). They provide exactly
what genuine `@bazel_tools` needs from them (rules_java's `toolchains`
extension with `local_jdk`/`remote_java_tools`, buildozer's
`buildozer_binary`, rules_cc's `cc_configure`/`toolchain_config_utils`
symbols for `winsdk_configure`, rules_shell's `sh_binary` for
`@bazel_tools//tools/test:collect_coverage`), so that genuine Bazel and the
native built-in profile analyze the same registered toolchains: genuine
`@bazel_tools` (`//tools/test:all`, `//tools/launcher:all`,
`@local_config_winsdk//:all`, `//tools/res:empty_rc_toolchain`), genuine
`platforms` and `rules_license`. Used by `tests/test_flags.rs` and
`artifacts/test_flags-capture.py`; `reference_receipt.json` is evidence only
(no test reads it).
