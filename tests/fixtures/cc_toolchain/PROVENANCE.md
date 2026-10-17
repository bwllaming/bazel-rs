# cc_toolchain fixture provenance

`workspace/` is hand-written for N25d4/N25d5 (docs/CC_TOOLCHAIN.md).
`expected/*.json` are the native outputs of `tests/cc_toolchain.rs`'s builds;
the paired genuine Bazel 9.3 capture (`reference_receipt.json`) compares them
with genuine outputs (bazel-out mnemonic divergence D1 normalized; since
N41's k8 roots the expected files are byte-identical to the genuine outputs).

`vendor/` and `vendor_shared.txt` (paths taken from
`tests/fixtures/cc_features/vendor`, byte-identical) are byte copies of the
genuine module sources Bazel 9.3 fetched into the module cache
`/home/coder/.cache/bazel/_bazel_coder/c133fb6f7a443d300f42ed363b8adea5/external`
(rules_cc+ = rules_cc 0.2.17, bazel_skylib+ = 1.9.0, platforms = 1.1.0,
bazel_features+ = 1.42.1, package_metadata+ = 0.0.5, apple_support+ = 1.24.2)
and `/home/coder/.cache/bazel/_bazel_coder/030cc0662e1bfc104e26240cf6d1a067/external/hermetic_cc_toolchain+`
(hermetic_cc_toolchain 4.1.0), restricted to the files the native profile
opens while building every target of the tests (determined with `strace` on
a run against the complete copies), plus each module's LICENSE. Every
`MODULE.bazel` is the genuine one. No file is modified.

`stubs/protobuf` is a test stub (`module(name = "protobuf")` plus a
`bazel/common/proto_info.bzl` exporting an empty `ProtoInfo`): rules_cc's
`cc_shared_library.bzl` loads `ProtoInfo` from protobuf on the cc_toolchain
load path; the genuine protobuf module is not needed for toolchain analysis.
The other closure stubs are N30's `tests/fixtures/test_flags/stubs`.
