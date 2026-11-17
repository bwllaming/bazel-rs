# Open-source corpus: bazel-rs against genuine Bazel 9.3.1

This corpus runs bazel-rs and genuine Bazel 9.3.1 side by side on public bzlmod
projects. It has two goals:

- find bazel-rs correctness bugs on code we did not write, and
- compare performance on the same commands.

The harness is `scripts/oss_corpus.py`. The projects, their pinned commits and
their workaround edits are in `scripts/oss_corpus_repos.json`. Unit tests for
the harness's pure parts are in `tests/test_oss_corpus.py`.

First run: 2026-11-16/17, bazel-rs 0.4.23 (the installed main binary, sha256
`69ce3bf6…`), genuine `bazel-9.3.1-linux-x86_64`.

## Method

- **Sources.** Each project is a public GitHub repository at a pinned commit,
  fetched with `git fetch --depth 1` (plus submodules where configured).
  Downloads come only from public sources: BCR, GitHub, PyPI, npm, crates.io,
  Maven Central and the Go proxy.
- **Isolation.** Each tool gets its own copy of the project (without `.git`)
  and its own HOME and TMPDIR. Genuine gets a private `--output_user_root`
  and a shared, pre-extracted `--install_base`. It also gets
  `--host_jvm_args=-Duser.home=<private home>`, because the JVM expands an rc
  file's `~` from getpwuid, not from `$HOME`. bazel-rs gets a private
  `--cache-dir`. Every command runs under
  `env -i PATH=/usr/bin:/bin HOME=… TMPDIR=…` at `nice -n 10`.
- **Flags.** Both tools read the project's own `.bazelrc` files
  (`--nohome_rc --nosystem_rc`). Both get the same `--jobs=16` and
  `--disk_cache= --remote_cache=`, so execution is local only. Genuine runs
  with `--batch`, except in the `server` phase.
- **Phases.** Each phase runs on both tools, in this order:
  1. `nobuild_cold`: `build --nobuild --keep_going //...` with fresh state.
     This covers module resolution, every fetch, loading and analysis.
  2. `nobuild_warm`: the same command twice more; the second run is measured.
     bazel-rs does not save an analysis snapshot for a command that wrote
     `MODULE.bazel.lock`, which the cold command does. So its first repeat
     re-analyses, and the second one uses the snapshot.
  3. `server` (genuine only): the same command twice in server mode; the
     second run, with a warm server, is measured. Then `shutdown`.
  4. `query`: `query --keep_going //...`. The two label sets are compared.
  5. `build`: `build --keep_going //...`. The file list of `bazel-bin` is
     compared, following directory symlinks and skipping `*.runfiles` trees.
     The SHA-256 of an even sample of up to 200 common files is compared too.
     Runfiles manifests are compared by their runfiles paths only, because
     their targets are absolute paths into each tool's own tree. The
     `<out>-N.params` files are counted separately (see C1).
  6. `test`: `test --keep_going //...`. Each target's status is compared.
- **Measurements.** For each command: exit code and wall time. Also the CPU
  time of the client's process tree (from wait4) and the tool's peak RSS:
  the VmHWM of the tool's own process, polled. For the warm genuine server,
  peak RSS is the server's VmHWM. The stderr tail and the full logs are kept.
- **Commands per project:** cold `build --nobuild`, then two more, then two
  genuine server runs, then `query //...`, `build //...` and `test //...`.
  Each command has a 20-minute cap.
- **Verdict per project:**
  - `OK`: identical exit codes, label sets, outputs and test statuses.
  - `RS-FAIL`: genuine succeeds in a phase where bazel-rs fails.
  - `DIFF`: same success, but different exit codes in a failing phase, or
    different labels, outputs or tests.
  - `BOTH-FAIL`: both fail the same way (for example, a project test that
    fails on both tools).
- **Host.** 64 CPUs and 251 GB RAM, shared with other workers. The load
  average was 15–60 during the runs, so timings are indicative only.

```
python3 scripts/oss_corpus.py clone --src /tmp/opencode/w/oss-src
python3 scripts/oss_corpus.py run --src /tmp/opencode/w/oss-src --work /tmp/opencode/w/oss-run \
    --genuine /path/to/bazel-9.3.1-linux-x86_64 --bazel-rs /path/to/bazel-rs [--only a,b] \
    [--phases nobuild_cold,nobuild_warm,server,query,build,test] [--keep]
python3 scripts/oss_corpus.py table --results /tmp/opencode/w/oss-run/results.json
```

Results accumulate in `results.json`, merged per project and phase. Logs go
to `<work>/logs/<project>/<tool>/<phase>.{out,err}`. A tool's state is
deleted after each project unless `--keep` is given.

## Projects

| project | lang | source | commit |
|---|---|---|---|
| abseil-cpp | C++ | abseil/abseil-cpp | 957149a29f4f |
| googletest | C++ | google/googletest | e882c3641019 |
| re2 | C++ | google/re2 | 2da0056814cf |
| capnproto | C++ | capnproto/capnproto `c++` | 20fe8fd325bf |
| highway | C++ | google/highway | 53bb99a6d506 |
| yalantinglibs | C++ | alibaba/yalantinglibs | 54f9c0642386 |
| tink-cc-examples | C++ | tink-crypto/tink-cc `examples` | a8d43aeece4c |
| riegeli | C++ | google/riegeli | 54b40506e54f |
| flatbuffers | C++ | google/flatbuffers | 5eb190f00005 |
| openroad | C++ | The-OpenROAD-Project/OpenROAD (+ OpenSTA submodule) | 4e64be82ff21 |
| protobuf | C++/Java/Python | protocolbuffers/protobuf (minus `csharp/`, `ruby/`) | 55e773ee73a6 |
| bazel-skylib | Starlark | bazelbuild/bazel-skylib | a920cdfe0215 |
| rules_cc | Starlark/C++ | bazelbuild/rules_cc | c100d5c63b2c |
| rules-template | Starlark/Go | bazel-contrib/rules-template | cb8787e74153 |
| with_cfg-examples | Starlark/C++/Java | fmeum/with_cfg.bzl `examples` | 104c28c5b3ef |
| diff.bzl | Starlark | kormide/diff.bzl | fa6aa97c7fab |
| buildtools | Go | bazelbuild/buildtools | fe6ef4d58b3b |
| bazel-gazelle | Go | bazel-contrib/bazel-gazelle | bc28018427cf |
| rules_go-basic_gazelle | Go | bazel-contrib/rules_go `examples/basic_gazelle` | e2d52babf321 |
| rules_jvm_external-simple | Java | bazel-contrib/rules_jvm_external `examples/simple` | 1ba9a81c928c |
| junit5-starter | Java | junit-team/junit-examples `junit-jupiter-starter-bazel` | 262f84d1d250 |
| examples-java-tutorial | Java | bazelbuild/examples `java-tutorial` | efab5bd4660e |
| examples-java-maven | Java | bazelbuild/examples `java-maven` | efab5bd4660e |
| rules_kotlin | Kotlin | bazelbuild/rules_kotlin | 55ce1fb0ef11 |
| rules_python-bzlmod | Python | bazel-contrib/rules_python `examples/bzlmod` | ea4234e6457c |
| rules_python-pip_parse | Python | bazel-contrib/rules_python `examples/pip_parse` | ea4234e6457c |
| examples-frontend | JS/TS | bazelbuild/examples `frontend` | efab5bd4660e |
| limdor-rust | Rust | limdor/bazel-examples `rust` | cd69be99aff9 |
| examples-rust-ffi | Rust/C++ | bazelbuild/examples `rust-examples/04-ffi` | efab5bd4660e |
| examples-rust-deps-cargo | Rust | bazelbuild/examples `rust-examples/05-deps-cargo` | efab5bd4660e |
| limdor-cpp | C++ | limdor/bazel-examples `cpp` | cd69be99aff9 |
| examples-cpp-tutorial | C++ | bazelbuild/examples `cpp-tutorial/stage3` | efab5bd4660e |
| examples-bzlmod-01 | C++ | bazelbuild/examples `bzlmod/01-depend_on_bazel_module` | efab5bd4660e |
| examples-bzlmod-05 | C++/Starlark | bazelbuild/examples `bzlmod/05-…package_manager` | efab5bd4660e |
| examples-configurations | Starlark/C++ | bazelbuild/examples `configurations` | efab5bd4660e |
| examples-rules | Starlark | bazelbuild/examples `rules` | efab5bd4660e |
| rules_zig | Zig | hermeticbuild/rules_zig | 4c6543de82a4 |

These projects were screened out because genuine 9.3.1 itself fails on them:

- google/glog: native `cc_*` rules were removed in Bazel 9.
- google/or-tools: about 1,200 loading errors.
- bazelbuild/examples `go-tutorial`: rules_go 0.4x uses the removed `CcInfo`.

flatbuffers is kept even though genuine fails on it the same way, through
the old rules_go's `CcInfo`, because bazel-rs fails with an extra error
there (B12).

### Workaround edits (applied to both tools)

Workaround edits are recorded per project in the JSON (`comment_out`,
`replace`, `vendor_archives`). Each one names the bazel-rs bug it bypasses.
Every edit is applied to both tools, so the comparison stays like for like.

- **B2 (rc options).** The rc lines that bazel-rs rejects are commented out.
  This applies to 16 projects; see B2 for the list of options.
- **rules-template.** Its v26 `MODULE.bazel.lock` is rejected by genuine
  9.3.1 too. It is deleted and the project runs with
  `--lockfile_mode=update`.
- **rules_go-basic_gazelle.** rules_go, gazelle and Go are bumped to 0.63.0,
  0.51.3 and 1.27.1, as in docs/BENCHMARKS.md. The pinned rules_go 0.52
  fails on Bazel 9 because `CcInfo` was removed.
- **openroad.** `archive_override(kepler-formal)` (B9) is replaced by the same
  archive, downloaded, verified against its integrity and patched with the
  seven `bazel/kepler-patches`, then wired in with `local_path_override`. The
  equivalent `git_override` fails on bazel-rs too (B22).

## Results

Times are wall seconds: genuine (g) against bazel-rs (rs). The "warm nobuild"
column gives genuine `--batch` / warm genuine server / bazel-rs. Peak RSS is
the tool process during the cold run. The outputs column counts `bazel-bin`
files: common, only genuine, only bazel-rs, and hash differences in the
sample.

| project | verdict | cold nobuild g / rs (s) | warm nobuild g-batch / g-server / rs (s) | build g / rs (s) | peak RSS cold g / rs (MB) | outputs (common, only g, only rs, hash diff) | tests g / rs | blocking bugs |
|---|---|---|---|---|---|---|---|---|
| abseil-cpp | **OK** | 14.8 / 12.0 | 3.3 / 0.2 / 0.2 | 97.7 / 76.3 | 1131 / 453 | 2767, 0, 0, 0 | PASSED:252 / PASSED:252 | (B2) |
| limdor-cpp | **OK** | 11.0 / 6.7 | 2.8 / 0.1 / 0.1 | 7.3 / 0.7 | 704 / 217 | 18, 0, 0, 0 | PASSED:1 / PASSED:1 | |
| limdor-rust | **OK** | 29.0 / 20.5 | 2.8 / 0.1 / 0.2 | 4.7 / 1.9 | 807 / 426 | 91, 0, 0, 0 | PASSED:1 / PASSED:1 | |
| examples-bzlmod-01 | **OK** | 11.5 / 6.7 | 3.0 / 0.1 / 0.1 | 5.4 / 2.6 | 641 / 217 | 187, 0, 0, 0 | — | |
| examples-bzlmod-05 | **OK** | 8.6 / 3.5 | 2.1 / 0.1 / 0.1 | 2.2 / 0.1 | 332 / 80 | 1, 0, 0, 0 | — | |
| examples-cpp-tutorial | **OK** | 10.8 / 6.7 | 2.8 / 0.2 / 0.1 | 3.6 / 0.7 | 755 / 203 | 13, 0, 0, 0 | — | |
| examples-rust-ffi | **OK** | 27.0 / 17.4 | 2.9 / 0.2 / 0.1 | 4.0 / 1.6 | 788 / 373 | 57, 0, 0, 0 | — | (B2) |
| rules_go-basic_gazelle | **OK** | 27.5 / 23.9 | 5.9 / 0.2 / 0.2 | 42.6 / 32.0 | 1281 / 490 | 117, 0, 0, 0 | PASSED:1 / PASSED:1 | |
| rules_python-pip_parse | **OK** | 30.1 / 41.7 | 3.5 / 0.2 / 0.2 | 4.7 / 1.1 | 1625 / 614 | 56, 0, 0, 0 | PASSED:2 / PASSED:2 | (B2) |
| rules-template | BOTH-FAIL¹ | 25.8 / 24.1 | 3.7 / 0.2 / 0.2 | 26.4 / 25.6 | 1405 / 423 | 446, 0, 0, 0 | FAILED:1 PASSED:1 / same | (B2) |
| yalantinglibs | DIFF² | 12.2 / 7.5 | 3.1 / 0.2 / 0.1 | 61.5 (exit 1) / 52.1 (exit 1) | 638 / 213 | 126, 6, 0, 0 | same | (B2) |
| buildtools | DIFF¹⁴ | 25.9 / 29.5 | 4.3 / 0.3 / 0.2 | 50.4 / 54.0 | 1974 / 1464 | 566, 0, 0, 1 | FAILED:2 PASSED:23 / same | B6 |
| bazel-gazelle | RS-FAIL | 32.6 / 39.9 (exit 1) | 4.4 / 0.3 / 3.0 (exit 1) | 60.6 / 112.6 (exit 1) | 1647 / 1031 | 2545, 6, 0, 0 | FAILED:2 PASSED:194 / FAILED:2 PASSED:192 | B13 |
| diff.bzl | DIFF¹⁵ | 22.3 / 14.5 | 3.5 / — / 0.2 | 11.8 / 10.9 | 1190 / 354 | 308, 0, 0, 1 | PASSED:6 / PASSED:6 | B6 |
| rules_python-bzlmod | DIFF¹⁵ | 44.9 / 59.6 | 7.3 / — / 0.2 | 43.8 / 41.4 | 2022 / 2577 | 319, 0, 0, 1 | PASSED:31 / PASSED:31 | B34 |
| with_cfg-examples | RS-FAIL¹⁰ | 35.3 / 43.9 (exit 1) | 4.2 / 0.3 / 3.3 (exit 1) | 8.7 / 8.6 (exit 1) | 1405 / 1739 | 188, 56, 0, 8 | PASSED:20 / PASSED:15 | B17, B18, B19 |
| examples-rules | RS-FAIL | 12.3 / 8.3 (exit 1) | 2.9 / 0.1 / 1.2 (exit 1) | 3.6 / 1.7 (exit 1) | 673 / 250 | 99, 4, 0, 0 | PASSED:1 / PASSED:1 | B23 |
| googletest | RS-FAIL | 25.4 / 14.7 (exit 1) | 3.2 / 0.2 / 1.5 (exit 1) | 32.0 / 1.1 (exit 1) | 1198 / 453 | 8, 521, 0, 0 | PASSED:44 / — | B1 |
| re2 | RS-FAIL³ | 13.7 (exit 1) / 9.5 (exit 1) | 2.9 / 0.2 / 1.4 (both exit 1) | 15.3 (exit 1) / 6.6 (exit 1) | 1071 / 313 | 324, 761, 0, 0 | PASSED:21 / PASSED:1 | B1 |
| highway | RS-FAIL | 11.9 / 7.6 (exit 1) | — | — | 736 / 234 | — | — | B1 |
| bazel-skylib | DIFF¹⁵ | 28.3 / 20.5 | 3.3 / — / 0.1 | 48.5 / 58.1 | 1328 / 553 | 689, 0, 0, 9 | FAILED:4 PASSED:158 / FAILED:4 PASSED:158 | B6 |
| junit5-starter | RS-FAIL | 25.3 / 4.2 (exit 1) | 4.0 / 0.2 / 0.3 (exit 1) | 7.7 / 0.3 (exit 1) | 1293 / 115 | 0, 128, 0, 0 | PASSED:1 / — | B3 |
| rules_zig | RS-FAIL¹⁵ | 30.7 / 36.8 (exit 1) | 13.2 / — / 12.0 (exit 1) | 36.9 / 11.9 (exit 1) | 1909 / 2042 | 0, 1292, 0, 0 | PASSED:211 SKIPPED:4 / SKIPPED:4 | B13c |
| capnproto | RS-FAIL⁴ | 17.1 / 11.3 (exit 1) | 3.3 / 0.1 / 1.5 (exit 1) | 22.6 (exit 1) / 7.5 (exit 1) | 1127 / 384 | 317, 1085, 0, 0 | both: FAILED TO BUILD:5 SKIPPED:14 | B11, B20, (B2) |
| tink-cc-examples | RS-FAIL | 17.3 / 17.1 (exit 1) | — | — | 1127 / 490 | — | — | B11, B14 |
| examples-java-maven | RS-FAIL¹⁵ | 35.5 / 26.9 (exit 1) | 3.5 / — / 1.1 (exit 1) | 8.0 / 1.1 (exit 1) | 1177 / 430 | 1, 134, 0, 0 | PASSED:2 / 0 | B31, B14 |
| rules_cc | RS-FAIL¹⁵ | 20.0 (exit 1) / 20.4 (exit 1) | 5.3 (exit 1) / — / 4.7 (exit 1) | 52.6 (exit 1) / 63.6 (exit 1) | 2635 / 1392 | 1348, 636, 0, 1 | FAILED:1 PASSED:453 SKIPPED:2 / FAILED:3 FAILED TO BUILD:2 PASSED:258 SKIPPED:2 | B32, B17, B18 |
| examples-rust-deps-cargo | RS-FAIL | 44.0 / 0.3 (exit 1) | 2.9 / 0.2 / 0.3 (exit 1) | 28.8 / 0.3 (exit 1) | 1034 / 29 | 0, 103, 0, 0 | — | B9, (B2) |
| protobuf | BOTH-FAIL¹⁵ | 49.3 (exit 1) / 79.0 (exit 1) | 7.0 (exit 1) / — / 18.1 (exit 1) | — | 2699 / 3315 | — | — | B33 |
| openroad | RS-FAIL¹¹ | 75.2 / 211.0 (exit 1) | 8.5 / 0.8 / 64.7 (exit 1) | — | 3764 / 3980 | — | — | B18, B30 |
| rules_kotlin | RS-FAIL¹⁵ | 36.9 / 35.7 (exit 1) | 4.7 / — / 7.4 (exit 1) | — | 2027 / 2165 | — | — | B35 |
| riegeli | BOTH-FAIL⁶ | 30.2 (exit 1) / 31.0 (exit 1) | 4.1 / 0.2 / 3.7 (all exit 1) | 78.2 (exit 1) / 92.5 (exit 1) | 1427 / 794 | 2800, 0, 0, 0 | PASSED:2 / PASSED:2 | |
| examples-frontend | DIFF⁷ | 89.7 (exit 1) / 129.3 (exit 1) | 60.6 / 110.6 / 100.7 (all exit 1) | 60.7 (exit 1) / 208.3 (exit 1) | 1350 / 1838 | 44827, 170986, 2, 0 | PASSED:14 / — | B8, B25, (B2) |
| rules_jvm_external-simple | BOTH-FAIL¹⁵ | 62.6 (exit 1) / 6.2 (exit 1) | 4.0 (exit 1) / — / 0.3 (exit 1) | 4.1 (exit 1) / 0.3 (exit 1) | 1505 / 110 | 0, 0, 0, 0 | — | B31 |
| flatbuffers | BOTH-FAIL⁸ | 21.7 (exit 1) / 8.2 (exit 1) | — | — | 864 / 184 | — | — | B12 |
| examples-java-tutorial | DIFF | 18.7 (exit 1) / 15.1 (exit 1) | 3.2 / 0.2 / 1.6 (both exit 1) | 5.9 (exit 1) / 4.2 (exit 1) | 881 / 266 | 11, 2, 0, 0 | — | C2 |
| examples-configurations | DIFF⁹ | 9.1 (exit 1) / 7.2 (exit 1) | 1.8 / 0.1 / 0.8 (both exit 1) | 2.0 (exit 1) / 0.8 (exit 1) | 460 / 225 | 0, 0, 16, 0 | — | B24 |

Notes:

1. One project test fails identically on both tools.
2. Four gcc 13 compile errors are identical on both tools. Genuine keeps the
   failed compiles' `.d` files and the failed test's runfiles manifest; that
   is harmless.
3. The project's `//app` package fails on both tools. Beyond that, B1 fails
   every gtest target on bazel-rs.
4. Genuine analyses everything; its build fails on gcc 13 compile errors in
   `//src/kj`.
5. Genuine needs a system `ruby` for rules_ruby, so `//ruby/...` and
   `//csharp/...` are excluded. bazel-rs fails at module resolution (B9).
6. Rerun 2026-11-17 (branch corpus-macros, after B10 and B16): both tools
   fail the same way in loading and analysis (`@local_config_tf` is not
   visible; `sh_binary` undefined in `//python`), and the same 256 of 263
   top-level targets build. `query //...` (264 labels) and the tests match.
   The output difference was B28 plus C1's 322 params files; rerun
   2026-11-17 (branch corpus-runfiles, B28 fixed): the 2,800 outputs match,
   only C1's params files remain.
7. `cosmo.zip` (the `ape` toolchain) timed out from this host on both tools,
   so timings include connection timeouts. Besides that, bazel-rs fails
   `//react-router:build` (B8). Its `test` run then hit B25. The output
   difference is mostly the missing outputs of the failing targets.
8. Genuine fails too: rules_jvm_external-simple on the missing Android SDK,
   flatbuffers on the old rules_go's `CcInfo`. bazel-rs fails earlier or
   with extra errors.
9. Genuine aborts on `alias/project_main:PROJECT.scl` ("Every .bzl file must
   have a corresponding package"). bazel-rs does not run that project-file
   check, analyses on and fails later.
10. Rerun 2026-11-17 (branch corpus-macros): B3 and B16 are fixed, so the
    `inherit_attrs` wrappers (`wrapper.bzl:31`) load and expand. The two
    test failures (`//opt_filegroup:slow_app_integration_test` and its
    `_with_reset` variant) were B15: the test checks that the runfile's
    `realpath` contains `-opt/bin/`, but bazel-rs's per-test runfiles were
    hardlinked copies. Rerun 2026-11-17 (branch corpus-runfiles, B15 fixed):
    both pass. The five tests still missing are behind the `.bzl` files
    using `rule(parent=…)` (B17: `//sh`, `//args_location_expansion`) and
    `cc_shared_library` (B18: `//dynamic_deps`).
11. Rerun 2026-11-17 (branch corpus-macros), without the `build` phase (a
    full genuine OpenROAD build exceeds the 20-minute cap; bazel-rs fails in
    analysis first). B9, B22, B10 and B16 are passed: `query //...` matches
    (7,079 labels). Analysis stops at B26 and B27.
12. Rerun 2026-11-17 on branch corpus-depset, phases
    `nobuild_cold,nobuild_warm,query`. B26 is fixed: rules_cc's `cc_args`
    (`cc/toolchains/args.bzl:109`) puts skylib `DirectoryInfo` values, whose
    fields are lambdas, in a depset; bazel-rs now accepts functions inside
    depset elements (docs/DEPSETS.md "Functions inside elements"), so
    OpenROAD's `@llvm//toolchain/args/linux:*_headers_include_search_paths`
    analyse. rules_cc as the main module additionally hits B29 (4 targets)
    and B13 `testing.analysis_test` (17 targets); genuine fails there too
    (its `//tests/integration` targets need a `bazel` on PATH).
    Rerun 2026-11-17 on branch corpus-runfiles (B26 from main, B27 fixed),
    phases `nobuild_cold,nobuild_warm,server,query`: `query //...` still
    matches (7,079 labels) and `@openssl` analyses. The next analysis errors
    are B30 (13 targets) and B18 (9 `cc_shared_library` targets under
    `@llvm//runtimes/glibc` and `@llvm-project//libcxxabi`).
13. Rerun on 2026-11-17 on branch corpus-analysistest (bazel-rs 0.4.27 plus
    the B13 fix). bazel-skylib's `nonexistent_subdirectory_test` fails in
    that run and passes after its last commit: 154 of 158 pass. rules_kotlin
    is the base 0.4.27 run's `nobuild_cold`; its other phases were not
    rerun, because genuine's `test //...` runs nested Bazel integration
    tests. These runs predate main's B16 fix: rules_cc's
    `extension_support_tests.bzl` (`inherit_attrs`) failed in them too.
14. Rerun 2026-11-17 (branch corpus-runfiles, B15 fixed): the test statuses
    match (`//:make_location_lint` passes; the two failures are the same on
    both tools). The one remaining output difference is B6
    (`lang/tables.gen.go_checkshtest.repo_mapping`).

15. Rerun 2026-11-17 on branch corpus-parity2 (phases `nobuild_cold,
    nobuild_warm,query,build,test`; protobuf and rules_kotlin
    `nobuild_cold,nobuild_warm,query`; rules_jvm_external-simple without
    `test`), with the B2 workaround edits of protobuf, diff.bzl,
    rules_kotlin, rules_python-bzlmod and rules_zig dropped. Fixed: B2's
    `--incompatible_check_testonly_for_output_files` and the other rc options
    of docs/RC_OPTIONS.md "Options of the OSS corpus", B4, B5, B7, B13a,
    B13b, B13e, B29 (= B13d). bazel-skylib and diff.bzl: every test status
    matches; only B6 remains. rules_python-bzlmod: tests match, B34 left.
    protobuf: `query //...` matches (3,411 labels); genuine's cold
    `build --nobuild` fails on rules_ruby (no `ruby` on PATH), bazel-rs on
    B33 first. rules_cc: 258 passing tests (50 before; genuine 453), next
    B32, B17, B18. rules_zig: B13c only. examples-java-maven and
    rules_jvm_external-simple: B31 (rules_jvm_external-simple also fails
    on genuine: no Android SDK). rules_kotlin: B35.

**Summary.** 37 projects ran. On 10 of them bazel-rs matches genuine exactly:
same exit codes, `query //...` labels, `bazel-bin` file lists, sampled
SHA-256s and test results. abseil-cpp is the largest: 2,767 outputs and 252
passing tests. Two more (rules-template, yalantinglibs; riegeli too since the
corpus-runfiles rerun) fail the same way on
both tools. Three are clean except for one bug each: bazel-gazelle (B13,
fixed since, gazelle not rerun),
buildtools (B15; since fixed, only B6's repo mapping manifest remains) and
rules_python-bzlmod (B7). The rest are blocked by the
bugs below. Most block loading, analysis or module resolution, not
execution: where both tools built, the outputs were identical except in
with_cfg (B19).

## Bugs (ranked by the number of projects they block)

Every bug below is a case where genuine 9.3.1 succeeds and bazel-rs fails or
differs. The logs are in `<work>/logs/<project>/rs/<phase>.err`.

| id | projects | summary |
|---|---|---|
| B2 | 16 (abseil-cpp, capnproto, yalantinglibs, with_cfg, rules_zig, rules_kotlin, rules-template, diff.bzl, examples-frontend, rules_python ×2, examples-rust ×2, openroad, protobuf, examples-rust-ffi query) | rc options rejected at startup: "options are never silently ignored" |
| B3 | 4 (bazel-skylib, junit5-starter, rules_zig, with_cfg) | `native.existing_rule` / `existing_rules` missing in BUILD and legacy-macro context |
| B9 | 4 (rules_cc, protobuf, openroad, examples-rust-deps-cargo) | `archive_override` unsupported |
| B1 | 3+ (googletest, re2, highway; every project that tests with googletest ≥ 1.15) | `config_setting(values = {"features": …})` |
| B10 | 3 (rules_kotlin, riegeli, openroad) | `override_repo()` / `inject_repo()` unsupported in MODULE.bazel |
| B6 | 4 (rules-template, diff.bzl, bazel-skylib, buildtools); not blocking | `_repo_mapping` manifest misses a repository |
| B11 | 2 (tink-cc, capnproto); every boringssl user | `strip_prefix` with a trailing `/` rejected |
| B14 | 3 (tink-cc, examples-java-maven, examples-frontend) | internal errors "has no recorded duplicate-label check" / "internal analysis order error" |
| B4 | fixed (corpus-parity2) | `android_common` global undefined: now the `.bzl` toplevel; its two methods fail explicitly |
| B13 | fixed (bazel-skylib, rules_zig, rules_cc; bazel-gazelle not rerun) | analysis tests: `analysis_test_transition`, `testing.analysis_test`, `Target.actions`, `AnalysisFailureInfo` (docs/ANALYSIS_TESTS.md) |
| B13a | fixed (corpus-parity2) | transition on `//command_line_option:minimum_os_version` and `ctx.fragments.cpp.minimum_os_version()` |
| B13b | fixed (corpus-parity2) | `srcs = ["."]` (the package directory as a source label): "unsafe source label //tests/directory:." |
| B13c | 1 (rules_zig) | one toolchain implementation for two execution platforms in one configuration (ConfiguredTargetKey.executionPlatformLabel), a fatal error (open: needs the execution platform in the configured-target key, see below) |
| B13d | fixed (= B29) | rules_cc's own `cc_test.bzl`/`cc_binary.bzl` "cannot use private API" |
| B13e | fixed (corpus-parity2) | `cc_import` of `mylib.dll`/`mylib.lib` on Linux: "does not match expected filetype" |
| B15 | fixed (corpus-runfiles) | test runfiles were hardlinks, not symlinks |
| B7 | fixed (corpus-parity2) | transition: "Starlark flag and its alias have different values: False and "false"" |
| B5 | fixed (corpus-parity2) | `local_repository` → `rctx.symlink(path, ".")` rejected (now a copy plus a `watch_tree` input, docs/LOCAL_REPOSITORIES.md) |
| B8 | 1 (examples-frontend) | declared output "crosses a package boundary" |
| B17, B18, B19 | 1 (with_cfg; B18 also openroad) | `rule(parent=…)`; `cc_shared_library`; solib naming in transitioned configurations (B16, symbolic macro `inherit_attrs`, is fixed) |
| B26 | fixed | depset elements holding functions that are not top-level defs (skylib `DirectoryInfo` in rules_cc `cc_args`; openroad) |
| B29 | fixed (corpus-parity2) | "file '//cc/private/rules_impl:cc_test.bzl' cannot use private API": the private-API allowlist now treats the main repository as rules_cc when the root module is rules_cc |
| B20 | 1 (capnproto) | `rule(output_to_genfiles=True)` |
| B21 | 1 (rules_zig) | `attr.int_list` in repository rules |
| B22 | 1 (openroad) | `git_override` of a repository with submodule gitlinks |
| B23 | 1 (examples-rules) | predeclared `attr.output` given as `":name"` |
| B12 | 1 (flatbuffers) | `@local_config_platform` unknown |
| B24 | 1 (examples-configurations) | no PROJECT.scl project-file check |
| B25 | 1 (examples-frontend) | "needs-repository: repository @@rules_diff+ is unfetched and this thread does not fetch" on a warm `test` |
| B30 | 1 (openroad) | `provider in target` fails "Target membership requires an exported provider constructor" in with_cfg's `transitioning_alias.bzl:55` (toolchains_llvm's `*_with_cfg` glibc/libunwind/libcxxabi targets; one of with_cfg's forwarded builtin providers is not an exported constructor natively) |
| B31 | 2 (examples-java-maven, rules_jvm_external-simple); every rules_android user | `config_common.config_feature_flag_transition` (rules_android `android_binary/rule.bzl:93`, at load time) is not implemented |
| B32 | 1 (rules_cc's own tests) | Starlark transitions on unmodelled native options (`fdo_optimize`, `cs_fdo_profile`, `instrumentation_filter`, `experimental_cpp_modules`, `build_test_dwp`, `fission`, `custom_malloc`, `incompatible_remove_legacy_whole_archive`), `cc_internal.freeze` of mutable structs / unfrozen functions, `objc.target_should_alwayslink` with an `alwayslink` attribute, `Target.actions` read in an aspect |
| B33 | 1 (protobuf) | analysis: `java_common.check_java_toolchain_is_declared_on_rule`, aspect providers (`does not provide …UpbMinitableCcInfo` / `JavaInfo` on proto_library targets), `Target.actions` in aspect tests, `cc_common.check_experimental_cc_shared_library` (B18) |
| B34 | 1 (rules_python-bzlmod); not blocking | `external/other_module+/other_module/pkg/bin_zipapp.zip` differs from genuine's (zipapp contents) |
| B35 | 1 (rules_kotlin) | rules_testing `TestingAspectInfo provider missing` (8 analysis tests), `download_and_extract(rename_files=…)` unsupported (ksp repository), `Target.actions` in an aspect |
| B27 | fixed (corpus-runfiles) | `depset` was not a BUILD-file global, and list-typed attributes did not accept depsets (`@openssl//:BUILD.bazel:508`, `select({...: depset([...])})`) |
| B28 | fixed (corpus-runfiles) | `build //...` missed genuine's `.pic.o`/`.pic.d` of external C++ deps: the rule-returned `_hidden_top_level_INTERNAL_` group (rules_cc cc_library's transitive objects) was not built |

### Details and minimal repros

**B2: rc options rejected.** bazel-rs exits at the first rc line it does not
model, while genuine accepts it. The options seen:

- Startup: `--host_jvm_args=…` (the aspect `bazelrc-presets`, used by
  rules-template, diff.bzl and frontend) and `--[no]windows_enable_symlinks`
  (capnproto, rules_kotlin, rules_zig, with_cfg, rules_python,
  rules_rust examples, or-tools).
- Common/build: `--show_timestamps`, `--http_connector_attempts`,
  `--http_timeout_scaling`, `--experimental_downloader_config`,
  `--downloader_config`, `--module_mirrors`, `--distdir`,
  `--noexperimental_check_external_repository_files`,
  `--incompatible_default_to_explicit_init_py`,
  `--incompatible_enforce_starlark_utf8=error`,
  `--incompatible_config_setting_private_default_visibility`,
  `--incompatible_use_cc_configure_from_rules_cc`,
  `--noexperimental_merged_skyframe_analysis_execution`,
  `--experimental_platform_in_output_dir`, `--grpc_keepalive_time`,
  `--terminal_columns`.
- Non-default incompatible values: `--noincompatible_disallow_empty_glob` and
  `--incompatible_disallow_empty_glob=false` ("only the Bazel 9.3 default …
  is modelled").
- `query` rejects `common --noslim_profile` and the
  `--experimental_profile_include_*` options, which `build` accepts.

Repro: a workspace whose `.bazelrc` has `startup --host_jvm_args=-Dx=1`, then
`bazel-rs build //...`. Suspicion: most of these are no-ops for bazel-rs
(JVM, Windows, UI, downloader tuning) and could be accepted and recorded. A
few need modelling: `module_mirrors`, `distdir`, `downloader_config` and
empty globs.

**B1: `config_setting` on `features`.** "select() on build option
'features' is not implemented in the native profile". googletest's
`BUILD.bazel:61` (`:emscripten_without_threads`, `values = {"features":
"-threads"}` style) breaks `@googletest//:gtest`. Repro: in googletest at
e882c364, run `bazel-rs build --nobuild //:gtest`. re2 and highway hit it through their
googletest dependency. abseil-cpp, on BCR googletest 1.18.0, is not affected.

**B3: `native.existing_rule(s)` in BUILD context.** docs/EXISTING_RULES.md
lists this as unsupported, but it is common:

- skylib `docs/private:stardoc_with_diff_test.bzl:92` and
  `tests/unittest_tests.bzl:315`;
- `apple_rules_lint` `get_lint_config.bzl:38`, through contrib_rules_jvm
  `java_test_suite`;
- aspect_bazel_lib `docs.bzl:57`, through rules_zig.

**B9: `archive_override`.** "archive_override for module X is unsupported:
no archive fetching/extraction". Seen for rules_testing (rules_cc),
kepler-formal (openroad), rules_fuzzing (protobuf) and rules_rust
(rust-examples).

**B10: `override_repo` / `inject_repo`.** "override_repo() is unsupported in
MODULE evaluation". It appears in rules_kotlin's MODULE.bazel:42, in BCR
`rules_swift` 4.1.1 MODULE.bazel:361 (which riegeli depends on) and in BCR
`hermetic_launcher` 0.0.15 (`inject_repo(crate, "bzip2")`, which openroad
depends on).

**B11: `strip_prefix` with a trailing slash.** "repository @@boringssl+ is
not materialized: strip_prefix must contain only normal path components".
BCR boringssl 0.20261105.0 `source.json` has `"strip_prefix":
"boringssl-0.20261105.0/"`. Likely fix: drop the trailing `/` first (trivial).

**B14: internal errors after B11 / B4.** The messages: "internal error:
@@protobuf+//bazel/flags/cc:cc_proto_library_header_suffixes has no recorded
duplicate-label check" (×10), "internal analysis order error:
@@bazel_tools//tools/def_parser:def_parser" (tink-cc) and "internal error:
@@bazel_tools//tools/jdk:TestRunner has no recorded duplicate-label check"
(java-maven). They only appear in the same run as a loading failure, so they
are probably a cascade. Recheck after fixing B11 and B4.

**B6: `_repo_mapping` manifest.** For rules-template
`//tools:preset.update_update.sh`, genuine's `.repo_mapping` contains
`bazelrc-preset.bzl+,bazel_tools,bazel_tools`, and bazel-rs omits that line.
The missing repository holds a target in the runfiles graph that
contributes no files. The same kind of difference appears in diff.bzl,
skylib (`tests/run_binary/run_bin_test-test.sh.repo_mapping`) and buildtools
(`lang/tables.gen.go_checkshtest.repo_mapping`).

**B15: test runfiles are hardlinks.** buildtools `//:make_location_lint`, a
`buildifier_test` with `no_sandbox = True`, passes on genuine and fails on
bazel-rs with "xargs: : No such file or directory". The runner does
`readlink buildifier/buildifier_/buildifier` and `realpath WORKSPACE`. In
bazel-rs's per-test execroot (`<cache>/tests/work/<key>/execroot/…runfiles/_main`)
runfiles are hardlinked regular files, so `readlink` prints nothing and
`realpath WORKSPACE` stays inside the runfiles tree. Genuine's runfiles are
symlinks. Repro: `bazel-rs test //:make_location_lint` in buildtools at
fe6ef4d5. Fixed on branch corpus-runfiles: a test's runfiles are symlinks to
the artifacts staged at their exec paths and to the live sources
(docs/RUNFILES_TREES.md "Test runfiles", genuine capture
`tests/fixtures/runfiles_symlinks`).

**B7: flag and alias values.** rules_python's `_zipapp_transition`
(`python/private/zipapp/py_zipapp_rule.bzl:307`) sets
`//python/config_settings:build_python_zip` = False and
`//command_line_option:build_python_zip` = "false". bazel-rs fails with
"Starlark flag and its alias have different values: False and "false"".
Genuine converts the native value through the alias and compares typed
values. Repro: `bazel-rs build //tests/other_module:other_module_import_test`
in `examples/bzlmod`.

**B5: local_repository symlink.** diff.bzl's `use_repo_rule`
`local_repository(path = "diff/tests/external_workspace")` fails: "Could
not create symlink from <ws>/diff/tests/external_workspace to
<cache>/repositories/external/+local_repository+external_workspace: the
repository directory itself cannot be a symlink" (bazel_tools `local.bzl:64`
`rctx.symlink(path, ".")`).

**B8: output crossing a package boundary.** aspect rules_js
`js_run_binary(out_dirs = ["build", "public/build"])` in `react-router/`,
where `react-router/public/BUILD.bazel` exists. bazel-rs fails with "output
public/build crosses a package boundary" (engine.rs:2626). Genuine builds it,
and its `//react-router:build_smoke_test` passes. Genuine does not check
`declare_directory` / `declare_file` outputs this way.

**B16–B19 (with_cfg.bzl):**

- B16 (fixed on branch corpus-macros, docs/SYMBOLIC_MACROS.md): symbolic
  macro "macro inherit_attrs is unsupported" (`wrapper.bzl:31`).
- B17: "rule(parent=...) rule extension is not supported" (`extend.bzl:10`).
- B18: `cc_shared_library` fails: "cc_common.check_experimental_cc_shared_library
  is not implemented".
- B19: in transitioned configurations (`k8-fastbuild-ST-…`), solib names and
  rpaths differ. Genuine has `_solib_k8/libST-b44ef279653a_external_Srules_Ucc+…so`
  with rpath `$ORIGIN/../../../_solib_k8/`. bazel-rs has
  `_solib_k8/libST-b44ef279653a/bin_external_…so` with rpath `…/_solib_k8/libST-b44ef279653a`.
  So the linked binaries differ (8 hash differences, `cc_define_test`,
  `cc_asan_test`).

**B26–B28 (rerun 2026-11-17).** B26 is the next OpenROAD analysis error
after `inherit_attrs` (toolchains_llvm's `resource_directory` macro now
expands): "a depset element containing a function that is not a global def
of a loaded .bzl file is not implemented" (fixed on branch corpus-depset; 10 targets under
`@llvm//toolchain/args/linux`). B27: openssl's BUILD.bazel uses
`depset([...])` as a `select()` branch; genuine accepts `depset` in BUILD
files and converts a depset to a list attribute value (Type.ListType.convert,
packages/Type.java:731-738). B28: genuine's riegeli `bazel-bin` has ~600
`.pic.o`/`.pic.d` files of external libraries (curl, google_cloud_cpp, xz,
bzip2) that bazel-rs does not build; no `.params` link file names them;
probably an output group of rules_cc targets (not investigated). Fixed on
branch corpus-runfiles: B27 (`depset` in BUILD files, depsets for list
attributes, docs/DEPSETS.md) and B28 (rules_cc's cc_library returns its and
its deps' objects in `_hidden_top_level_INTERNAL_`, which genuine builds for
every requested target; bazel-rs built only the default runfiles' part of
that group, docs/RUNFILES.md).

**B20.** `rule(output_to_genfiles=True)` "accepted at load time but not
analyzed" (capnproto's `capnp_gen`).

**B21.** "repository attribute "exec_lengths": attribute kind IntList is not
supported for repository rules" (rules_zig `zig_toolchains`).

**B22.** `git_override` of a repository with submodules: "unsupported Git
tree entry: expected file or symlink blob (no gitlinks)". Genuine skips
gitlinks when `init_submodules = False`.

**B23.** A legacy macro passing `outs = [":dog.md5"]`-style labels to
`attr.output_list` fails: "invalid output path ":dog.md5": target names may
not contain ':'" (examples `rules/predeclared_outputs/hash.bzl:68`).

**B12.** aspect_bazel_lib 1.x `platform_utils.bzl:3` loads
`@local_config_platform//:constraints.bzl`. bazel-rs fails with "unknown
apparent repository @local_config_platform"; genuine still provides it.

**B4.** rules_android `reexport_android_common.bzl:23`: "name
'android_common' is not defined". Genuine 9.3.1 still has the global.

**B24.** Genuine rejects a `PROJECT.scl` outside any package ("error loading
project files: Every .bzl file must have a corresponding package").
bazel-rs does not run that check.

**B25.** In examples-frontend, the `test //...` run after `build //...`
failed every target with "While resolving toolchains … invalid registered
toolchain '//diff/toolchain/...': needs-repository: repository @@rules_diff+
is unfetched and this thread does not fetch". The earlier commands had
fetched that repository. It looks like an internal fetch-scheduling bug on a
warm command.

**B13 (fixed).** skylib `analysistest`, rules_testing and
`analysis_test_transition`, including `expect_failure`, are analysed
(docs/ANALYSIS_TESTS.md). One known gap remains: an aspect applied to a
target in a configuration where nothing resolved the target platform, for
example a failure target below `expect_failure`'s transition analysed
alone (`test //tests/directory:nonexistent_subdirectory_test` in
bazel-skylib), stops with the deferred-platform sentinel "rule
implementation needs the target platform" when it reads `ctx.var`. The
whole `//tests/directory/...` package analyses.

**B13a.** skylib's `change_setting_test`,
`change_setting_with_failure_test` and `inspect_output_dirs_test` set
`//command_line_option:minimum_os_version` (CppOptions, default null). It is
not a modelled native option.

**B13c.** `zig/tests:version_validation_test` needs the zig toolchain for
two execution platforms in one configuration, which ends the whole command.

**B13c (open).** Genuine keys a toolchain implementation's configured
target by its parent's execution platform (DependencyResolver
`executionPlatformLabel`, ConfiguredTargetKey); rules_zig's
`version_validation_test` resolves one SDK for two execution platforms in
one configuration and expects two validation stamps. The native
`ConfiguredTargetKey` is (label, configuration, apply-rule-transition);
adding the execution platform touches every evaluator (walk, eval,
parallel, prepare, linearize pass 1's `toolchain_implementations` claim and
its whole-command stop), cquery and the snapshot codec. Not attempted on
corpus-parity2.

**B31–B35** are the next blockers the corpus-parity2 rerun found (details in
the table above and footnote 15).

### Cosmetic differences

- C1: C++ link actions' `<output>-N.params` files remain in genuine's
  `bazel-bin`, not in bazel-rs's.
- C2: genuine leaves JavaBuilder's `_javac/<target>/<target>_classes/*.class`
  in `bazel-bin`; bazel-rs does not.
- Error message formats differ; for example, Starlark tracebacks use the
  `* file:line` style. The exit codes match.

## Performance

These are indicative only, because the host was shared (load 15–60).

- **Cold `build --nobuild //...`** (module resolution, fetch, loading,
  analysis): bazel-rs is faster on 9 of the 11 matching projects, by 1.2–1.7×
  (abseil 12.0 s against 14.8 s, limdor-cpp 6.7 against 11.0, rust-ffi
  17.4 against 27.0). It is slower on rules_python pip_parse (41.7 against
  30.1, pip wheel fetching) and on bazel-gazelle (39.9 against 32.6). It is
  even on buildtools.
- **Warm `build --nobuild`:** bazel-rs, with its analysis snapshot, takes
  0.1–0.2 s, like a warm genuine server (0.1–0.4 s) and 15–30× faster than
  genuine `--batch` (2.8–5.9 s). bazel-rs does not save a snapshot for the
  command that wrote `MODULE.bazel.lock` (the cold one), so the first repeat
  re-analyses, for example 2.7 s on abseil (P1).
- **Full build** (after the cold analysis): bazel-rs is faster on abseil
  (76 s against 98 s), buildtools (58 against 75), basic_gazelle (32 against
  43) and the small projects. It is 1.9× slower on bazel-gazelle (113 s
  against 61 s, with similar CPU time: 1,147 against 1,051 s), which matches
  docs/BENCHMARKS.md's gazelle cold build gap (P2).
- **Memory:** bazel-rs's peak RSS is 1.3–3.5× lower on cold analysis (for
  example 453 MB against 1,131 MB on abseil). The exceptions are the large JS
  (1.8 GB against 1.35 GB) and rules_python bzlmod (2.6 GB, about equal)
  workspaces.

## Recommendations

1. B2 first. One table change, accepting the no-op startup and command
   options, unblocks 16 projects' first command with no edits.
2. Then B1, B11 and B3 (C++ with gtest, boringssl users, skylib/aspect
   helpers), then B9 and B10 (module resolution for rules_cc, protobuf,
   OpenROAD, rules_kotlin, rules_swift users). OpenROAD needs B9 (or B22)
   and B10.
3. B15 (symlinked runfiles for tests) and B6 (`_repo_mapping` entries) are
   silent behaviour differences; they deserve parity fixtures.
4. Rerun the corpus after each fix: `oss_corpus.py run --only <project>`,
   then drop the corresponding workaround edits from the JSON.
