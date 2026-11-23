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

`workspace/shared` (OSS corpus B18) is hand-written: three cc_library
targets, two cc_shared_library targets (`exports_filter`, `dynamic_deps`
between them, `shared_lib_name`) and a cc_binary with `dynamic_deps`.
`genuine_aquery_shared.txt` is genuine Bazel 9.3.1's
`aquery "mnemonic('CppLink|CppArchive|SolibSymlink|CppCompile', //shared:all)"`
of the same package in a scratch module with `bazel_dep(rules_cc, 0.2.17)`
from the BCR (host gcc, `--batch --ignore_all_rc_files`); the genuine
binary prints `112`. Its `bazel-bin/shared` holds the same files as the
native build (`_objs`, `lib{base,foo,bar}.{a,so}`, `libfoo_so.so`,
`libbarcustom.so`, `main`, runfiles) apart from C1's `-0.params` files.

`workspace/tcfiles` (OSS corpus B45) is hand-written: a `cc_toolchain`
whose `compiler_files`, `as_files`, `ar_files` and `all_files` hold
different marker files, and a `cc_library` with a `.c`, a `.S` and a `.s`
source. `genuine_aquery_tcfiles.txt` is genuine Bazel 9.3.1's
`aquery --extra_toolchains=//tcfiles:tc "mnemonic('CppCompile|CppArchive', //tcfiles:lib)"`
of the same package in a scratch module with `bazel_dep(rules_cc, 0.2.17)`
and `bazel_dep(platforms, 1.1.0)` from the BCR (`--batch
--ignore_all_rc_files`): compiles stage `compiler_files` (`as_files` for
the `assemble` `.s` source), the archive `ar_files`.

`genuine_aquery_fission.txt` (OSS corpus B32) is genuine Bazel 9.3.1's
`aquery --fission=yes -c dbg "mnemonic(CppCompile, //cdep:built)"` of the
`cdep` package in a scratch module with `bazel_dep(rules_cc, 0.2.17)` and
`bazel_dep(platforms, 1.1.0)` from the BCR (`--batch`, no rc files): the
compile's `.pic.dwo` output beside its object (CcStarlarkInternal
.declareOtherOutputFile) and the `per_object_debug_info` flags.

`workspace/lto` (OSS corpus B32) is hand-written: a toolchain with the
`thin_lto` feature (the flag sets of rules_cc's own test toolchain,
`tests/cc/testutil/toolchains/cc_toolchain_config.bzl`), a `cc_library`, a
statically linked `cc_binary` and a `cc_test` with
`thin_lto_linkstatic_tests_use_shared_nonlto_backends`.
`genuine_aquery_lto.txt` is genuine Bazel 9.3.1's
`aquery --extra_toolchains=//lto:tc "mnemonic('CcLtoBackendCompile|CppLTOIndexing|CppLink|CppArchive|CppCompile', //lto:all)"`
of the same package in a scratch module with `bazel_dep(rules_cc, 0.2.17)`
and `bazel_dep(platforms, 1.1.0)` from the BCR (`--batch
--ignore_all_rc_files`): the indexed backends (LtoBackendAction, its
progress message and ActionKey), the LTO indexing actions and the library's
shared non-LTO backend (`shared.nonlto/`, index `/dev/null`).
The `lto` toolchain also has an `autofdo` feature (`-fauto-profile=%{fdo_profile_path}`
on compiles and LTO backends) and the package exports an empty
`profile.afdo`: `genuine_aquery_fdo_label.txt` and
`genuine_aquery_fdo_absolute.txt` are genuine Bazel 9.3.1's
`aquery -c opt --fdo_optimize=//lto:profile.afdo` and
`--fdo_optimize=/abs/dir/profile.afdo` with
`--extra_toolchains=//lto:tc "mnemonic('Symlink|CppCompile|CcLtoBackendCompile', //lto:bin + //lto:cc_tc)"`
(the same scratch module; `genuine_aquery_lto.txt` is unchanged by the
feature, re-captured byte-identical).
