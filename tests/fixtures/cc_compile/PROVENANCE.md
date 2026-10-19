N60 committed fixture: the `cdep` package of the paired capture
(artifacts/cc_compile-capture.py) as a permanent gate. `workspace/` is
hand-written; the vendored modules, the shared vendor list and the stubs are
the byte-identical copies of tests/fixtures/cc_toolchain (PROVENANCE.md
there), reached through the same local_path_override MODULE that its host
@local_config_cc (F2) tests use, plus `workspace/vendor/rules_cc`: the eleven
genuine rules_cc files the cc_library/cc_import rule path loads beyond the
N25d toolchain subset (`cc/defs.bzl` and the rule/export files it pulls in;
byte copies of the module cache tree, determined by running the build until
every load resolved). The prebuilt `libprebuilt.a`/`.so` are not committed:
the test generates them with the host `gcc`/`ar` (deterministic `ar rcD`,
`-Wl,-build-id=none -Wl,-soname=libprebuilt.so`) exactly like the capture's
setup, so nothing binary sits in git and the commands stay the ones the
paired receipt compares against genuine Bazel.

No cgo target lives here: the go SDK is a host installation, not a fixture;
the cgo paths stay covered by the paired capture script
(artifacts/cc_compile-capture.py, receipt reference_receipt.json).
