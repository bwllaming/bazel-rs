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
     Before corpus-perf (P1), bazel-rs did not save an analysis snapshot
     for a command that wrote `MODULE.bazel.lock`, which the cold command
     does, so its first repeat re-analysed; the rows before that rerun were
     measured so, and the second repeat is the snapshot hit either way.
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
| abseil-cpp | **OK**¹⁵ ¹⁹ | 14.8 / 12.0 (rerun: 16.1–17.5 / 16.7–44.6) | 3.3 / 0.2 / 0.2 | 97.7 / 76.3 | 1131 / 453 | 2767, 0, 0, 0 | PASSED:252 / PASSED:252 | (B2) |
| limdor-cpp | **OK** | 11.0 / 6.7 | 2.8 / 0.1 / 0.1 | 7.3 / 0.7 | 704 / 217 | 18, 0, 0, 0 | PASSED:1 / PASSED:1 | |
| limdor-rust | **OK** | 29.0 / 20.5 | 2.8 / 0.1 / 0.2 | 4.7 / 1.9 | 807 / 426 | 91, 0, 0, 0 | PASSED:1 / PASSED:1 | |
| examples-bzlmod-01 | **OK** | 11.5 / 6.7 | 3.0 / 0.1 / 0.1 | 5.4 / 2.6 | 641 / 217 | 187, 0, 0, 0 | — | |
| examples-bzlmod-05 | **OK** | 8.6 / 3.5 | 2.1 / 0.1 / 0.1 | 2.2 / 0.1 | 332 / 80 | 1, 0, 0, 0 | — | |
| examples-cpp-tutorial | **OK** | 10.8 / 6.7 | 2.8 / 0.2 / 0.1 | 3.6 / 0.7 | 755 / 203 | 13, 0, 0, 0 | — | |
| examples-rust-ffi | **OK** | 27.0 / 17.4 | 2.9 / 0.2 / 0.1 | 4.0 / 1.6 | 788 / 373 | 57, 0, 0, 0 | — | (B2) |
| rules_go-basic_gazelle | **OK** | 27.5 / 23.9 | 5.9 / 0.2 / 0.2 | 42.6 / 32.0 | 1281 / 490 | 117, 0, 0, 0 | PASSED:1 / PASSED:1 | |
| rules_python-pip_parse | **OK**¹⁵ ¹⁹ | 30.3–35.7 / 46.5–51.7 (rerun: 34.9–37.8 / 38.7–61.0) | 3.5 / 0.2 / 0.2 | 4.7 / 1.1 | 1625 / 614 | 56, 0, 0, 0 | PASSED:2 / PASSED:2 | (B2) |
| rules-template | BOTH-FAIL¹ | 25.8 / 24.1 | 3.7 / 0.2 / 0.2 | 26.4 / 25.6 | 1405 / 423 | 446, 0, 0, 0 | FAILED:1 PASSED:1 / same | (B2) |
| yalantinglibs | DIFF² | 12.2 / 7.5 | 3.1 / 0.2 / 0.1 | 61.5 (exit 1) / 52.1 (exit 1) | 638 / 213 | 126, 6, 0, 0 | same | (B2) |
| buildtools | BOTH-FAIL¹⁷ | 40.4 / 62.3 | 4.7 / — / 0.2 | 56.4 / 83.4 | 1990 / 1471 | 566, 0, 0, 0 | FAILED:2 PASSED:23 / same | |
| bazel-gazelle | RS-FAIL¹⁶ | 49.8 / 40.2 | 4.4 / 0.3 / not rerun | 53.0–62.5 / 55.1–59.7 | 1647 / 1031 | 2545, 6, 0, 0 (before B13's fix) | FAILED:2 PASSED:194 / FAILED:2 PASSED:192 (before B13's fix) | (B13 fixed; not rerun) |
| diff.bzl | **OK**¹⁷ | 29.4 / 43.5 | 3.6 / — / 0.2 | 13.0 / 16.0 | 812 / 374 | 308, 0, 0, 0 | PASSED:6 / PASSED:6 | |
| rules_python-bzlmod | DIFF¹⁷ | 51.9 / 77.6 | 6.2 / — / 0.2 | 48.7 / 43.0 | 1936 / 2583 | 319, 0, 0, 1 | PASSED:31 / PASSED:31 | B34 |
| with_cfg-examples | RS-FAIL¹⁰ ¹⁸ | 36.6 / 72.5 (exit 1) | 5.0 / — / 3.5 (exit 1) | 10.0 / 9.1 (exit 1) | 2061 / 1718 | 201, 43, 0, 0 | PASSED:20 / PASSED:18 | B43 |
| examples-rules | RS-FAIL | 12.3 / 8.3 (exit 1) | 2.9 / 0.1 / 1.2 (exit 1) | 3.6 / 1.7 (exit 1) | 673 / 250 | 99, 4, 0, 0 | PASSED:1 / PASSED:1 | B23 |
| googletest | RS-FAIL | 25.4 / 14.7 (exit 1) | 3.2 / 0.2 / 1.5 (exit 1) | 32.0 / 1.1 (exit 1) | 1198 / 453 | 8, 521, 0, 0 | PASSED:44 / — | B1 |
| re2 | RS-FAIL³ | 13.7 (exit 1) / 9.5 (exit 1) | 2.9 / 0.2 / 1.4 (both exit 1) | 15.3 (exit 1) / 6.6 (exit 1) | 1071 / 313 | 324, 761, 0, 0 | PASSED:21 / PASSED:1 | B1 |
| highway | RS-FAIL | 11.9 / 7.6 (exit 1) | — | — | 736 / 234 | — | — | B1 |
| bazel-skylib | BOTH-FAIL¹⁷ | 27.5 / 43.3 | 3.6 / — / 0.2 | 57.9 / 62.9 | 1327 / 558 | 689, 0, 0, 0 | FAILED:4 PASSED:158 / same | |
| junit5-starter | DIFF¹⁷ | 29.5 / 46.7 | 3.8 / — / 0.1 | 8.7 / 6.1 | 1243 / 488 | 100, 28, 0, 0 | PASSED:1 / PASSED:1 | C2 |
| rules_zig | RS-FAIL¹⁵ | 30.7 / 36.8 (exit 1) | 13.2 / — / 12.0 (exit 1) | 36.9 / 11.9 (exit 1) | 1909 / 2042 | 0, 1292, 0, 0 | PASSED:211 SKIPPED:4 / SKIPPED:4 | B13c |
| capnproto | RS-FAIL⁴ | 17.1 / 11.3 (exit 1) | 3.3 / 0.1 / 1.5 (exit 1) | 22.6 (exit 1) / 7.5 (exit 1) | 1127 / 384 | 317, 1085, 0, 0 | both: FAILED TO BUILD:5 SKIPPED:14 | B11, B20, (B2) |
| tink-cc-examples | RS-FAIL | 17.3 / 17.1 (exit 1) | — | — | 1127 / 490 | — | — | B11, B14 |
| examples-java-maven | RS-FAIL¹⁷ | 36.4 / 64.0 (exit 1) | 3.5 / — / 1.3 (exit 1) | 7.9 / 1.5 (exit 1) | 1225 / 445 | 1, 134, 0, 0 | PASSED:2 / 0 | B31 |
| rules_cc | RS-FAIL¹⁵ | 20.0 (exit 1) / 20.4 (exit 1) | 5.3 (exit 1) / — / 4.7 (exit 1) | 52.6 (exit 1) / 63.6 (exit 1) | 2635 / 1392 | 1348, 636, 0, 1 | FAILED:1 PASSED:453 SKIPPED:2 / FAILED:3 FAILED TO BUILD:2 PASSED:258 SKIPPED:2 | B32, B17, B18 |
| examples-rust-deps-cargo | RS-FAIL | 44.0 / 0.3 (exit 1) | 2.9 / 0.2 / 0.3 (exit 1) | 28.8 / 0.3 (exit 1) | 1034 / 29 | 0, 103, 0, 0 | — | B9, (B2) |
| protobuf | BOTH-FAIL¹⁵ | 49.3 (exit 1) / 79.0 (exit 1) | 7.0 (exit 1) / — / 18.1 (exit 1) | — | 2699 / 3315 | — | — | B33 |
| openroad | DIFF¹¹ ¹⁶ ¹⁸ ¹⁹ | 89.6 / 242.5 (corpus-fetch rerun, exit 1 before B18: 77.9–85.5 / 184.3–248.9) | 8.9 / — / 1.8 | `//:openroad` 676–702 / 1536 | 3014 / 9169 | `//:openroad`: 15223, 3978, 301, 0 | — | B40, B41, P6 |
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

16. Rerun 2026-11-17 on branch corpus-perf (main 460717e5 plus the
    performance fixes below), phases `nobuild_cold` and `nobuild_warm` (and
    `build` for bazel-gazelle) only, with the harness's flags and isolation
    but driven by hand, three rounds each; ranges are the three runs (load
    13–47). bazel-gazelle's cold `--nobuild` and `build //...` now exit 0
    on both tools (B13 is fixed); its `build` outputs are byte-identical
    before and after the scheduler change (2,551 files of `bazel-bin`);
    query and test were not rerun. abseil-cpp's first repeat now hits the
    snapshot (0.19–0.24 s, was 3.2–3.5 s). rules_python-pip_parse's cold
    run stays unsaved: its pip extension prints a warning
    (`extension-output`), which the next run does not print.
17. Rerun 2026-11-18 on branch corpus-parent (main 0b26c20d plus B6, B17
    and B14's fixes), phases `nobuild_cold,nobuild_warm,query,build,test`
    (no genuine server phase), load 25-38 with other workers' runs (the
    cold times include every fetch, P3/P4). B6 is fixed: a repo mapping
    manifest's transitive packages now include those of the select() keys
    and of an output file's generating rule (its exec tools), so every
    `.repo_mapping` of diff.bzl, bazel-skylib and buildtools matches
    (tests/fixtures/repo_mapping_closure). diff.bzl is **OK**; bazel-skylib
    and buildtools match genuine in every compared output, label and test
    status (their failing project tests fail identically). B17 is fixed
    (docs/RULE_EXTENSION.md): with_cfg's `rule(parent=...)` rules load and
    analyse, `query //...` matches (194 labels) and 17 of genuine's 20
    tests pass; the three left are `//dynamic_deps` (B18), and the outputs
    differ by B18's targets, B19's 8 solib hashes and B43. junit5-starter
    (B3 fixed meanwhile) builds and passes its test; only C2's `_javac`
    class directories differ. examples-java-maven: B14 is fixed (it was an
    evaluator bug, below); only B31 remains. rules_python-bzlmod: B34 left
    (cause below).

18. Rerun 2026-11-18 on branch corpus-ccshared (bazel-rs at its HEAD,
    non-LTO release), phases `nobuild_cold,nobuild_warm,query` for
    OpenROAD and all phases for with_cfg-examples. Fixed: B18
    (`cc_shared_library`), B19 (transitioned solib names), B30, and the
    next OpenROAD blockers B36–B39; with_cfg's
    `//dynamic_deps:bin_test_from_symlinked_runfiles` needed B42.
    OpenROAD: `build --nobuild //...` exits 0 on both tools (6,773
    targets), `query //...` matches (7,079 labels). Genuine's cold run of
    this rerun failed on a DNS error for `users.aalto.fi` (bliss), so its
    cold time is the previous round's 89.6 s; the warm columns are the
    second repeat (bazel-rs's first repeat: 58.2 s, genuine 20.5 s). The
    full `build //...` exceeds the 20-minute cap, so `build //:openroad`
    ran by hand after the nobuild phases (`--jobs=16`, both cold action
    caches): genuine 676 s (load 15) and 702 s (load 37), bazel-rs 1,536 s
    (load 20–48), the same 13,479/13,480 actions (7,642 sandboxed spawns).
    The `openroad` binary (119 MB) and every other common `bazel-bin` file
    are byte-identical (all 15,223 compared; the runfiles manifest differs
    only by its absolute paths). Only genuine: `src/web/node_modules`
    (B41); only bazel-rs: the clang `include/`/`share/` files of
    `@llvm//toolchain:linux_x86_64_cc_toolchain_resource_directory` (B40).
    with_cfg-examples: every hash matches (B19); the two missing tests and
    the `args_location_expansion`/`sh` outputs are B17; the rest of the 43
    only-genuine files are C2 (`_javac` class directories) and runfiles
    trees of targets in the `_with_reset`/`cc_no_copt_binary` packages, as
    in earlier runs.

19. Rerun 2026-11-18 on branch corpus-fetch (main 0b26c20d plus the
    fetch changes below, with the side-by-side lane experiment that was
    then left unmerged; non-LTO), phase `nobuild_cold` only, the
    harness's flags and isolation driven by hand, three rounds each, main
    and the branch and genuine `--batch` alternating (load 7–36; cold
    downloads from the public mirrors, whose speed varied by 2× between
    rounds: abseil-cpp's own runs took 16.7–44.6 s on both binaries). The
    ranges are the branch's. Exit codes, stdout, stderr (without timings
    and progress), `MODULE.bazel.lock` and the repository-to-tree map
    match main's in every round; abseil-cpp's and pip_parse's action
    dumps (2,313 and 88 actions) are byte-identical.

**Summary.** 37 projects ran. On 13 of them bazel-rs matches genuine exactly
(10 before the corpus-parent rerun, footnote 17: diff.bzl, bazel-skylib
and buildtools since B6's fix; bazel-skylib's and buildtools' own failing
tests fail identically on both tools; junit5-starter differs only by C2):
same exit codes, `query //...` labels, `bazel-bin` file lists, sampled
SHA-256s and test results. abseil-cpp is the largest: 2,767 outputs and 252
passing tests. Two more (rules-template, yalantinglibs; riegeli too since the
corpus-runfiles rerun) fail the same way on
both tools. Two are clean except for one bug each: bazel-gazelle (B13,
fixed since; the corpus-perf rerun builds `//...` on both tools, its query
and tests not rerun) and rules_python-bzlmod (B34, its zipapp's modes). The
rest are blocked by the
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
| B6 | fixed (corpus-parent; rules-template not rerun) | `_repo_mapping` manifest missed the repositories of select() keys and of an output file's generating rule |
| B11 | 2 (tink-cc, capnproto); every boringssl user | `strip_prefix` with a trailing `/` rejected |
| B14 | fixed for examples-java-maven (corpus-parent); tink-cc, examples-frontend not rerun | internal errors "has no recorded duplicate-label check" / "internal analysis order error" |
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
| B17 | fixed (corpus-parent) | `rule(parent=…)` rule extension (docs/RULE_EXTENSION.md) |
| B18, B19 | fixed (corpus-ccshared) | `cc_shared_library` (`cc_common.check_experimental_cc_shared_library`); solib naming in transitioned configurations |
| B26 | fixed | depset elements holding functions that are not top-level defs (skylib `DirectoryInfo` in rules_cc `cc_args`; openroad) |
| B29 | fixed (corpus-parity2) | "file '//cc/private/rules_impl:cc_test.bzl' cannot use private API": the private-API allowlist now treats the main repository as rules_cc when the root module is rules_cc |
| B20 | 1 (capnproto) | `rule(output_to_genfiles=True)` |
| B21 | 1 (rules_zig) | `attr.int_list` in repository rules |
| B22 | 1 (openroad) | `git_override` of a repository with submodule gitlinks |
| B23 | 1 (examples-rules) | predeclared `attr.output` given as `":name"` |
| B12 | 1 (flatbuffers) | `@local_config_platform` unknown |
| B24 | 1 (examples-configurations) | no PROJECT.scl project-file check |
| B25 | 1 (examples-frontend) | "needs-repository: repository @@rules_diff+ is unfetched and this thread does not fetch" on a warm `test` |
| B30 | fixed (corpus-ccshared) | `provider in target` fails "Target membership requires an exported provider constructor" in with_cfg's `transitioning_alias.bzl:55` (toolchains_llvm's `*_with_cfg` glibc/libunwind/libcxxabi targets; one of with_cfg's forwarded builtin providers is not an exported constructor natively: `apple_common.Objc`) |
| B31 | 2 (examples-java-maven, rules_jvm_external-simple); every rules_android user | `config_common.config_feature_flag_transition` (rules_android `android_binary/rule.bzl:93`, at load time) is not implemented |
| B32 | 1 (rules_cc's own tests) | Starlark transitions on unmodelled native options (`fdo_optimize`, `cs_fdo_profile`, `instrumentation_filter`, `experimental_cpp_modules`, `build_test_dwp`, `fission`, `custom_malloc`, `incompatible_remove_legacy_whole_archive`), `cc_internal.freeze` of mutable structs / unfrozen functions, `objc.target_should_alwayslink` with an `alwayslink` attribute, `Target.actions` read in an aspect |
| B33 | 1 (protobuf) | analysis: `java_common.check_java_toolchain_is_declared_on_rule`, aspect providers (`does not provide …UpbMinitableCcInfo` / `JavaInfo` on proto_library targets), `Target.actions` in aspect tests, `cc_common.check_experimental_cc_shared_library` (B18) |
| B34 | 1 (rules_python-bzlmod); not blocking | `external/other_module+/other_module/pkg/bin_zipapp.zip` differs from genuine's: rules_python's zipper stores each input's mode and relative symlinks as it finds them in the sandbox; native staging hardlinks read-only store blobs (docs/HARDLINK_STAGING.md) |
| B43 | 1 (with_cfg); not blocking | genuine builds `cc_no_copt_binary/binary_/binary.runfiles_manifest` and `.repo_mapping` (the manual `cc_binary` behind with_cfg's frontend and transitioning alias, a no-op transition); bazel-rs does not (not investigated) |
| B35 | 1 (rules_kotlin) | rules_testing `TestingAspectInfo provider missing` (8 analysis tests), `download_and_extract(rename_files=…)` unsupported (ksp repository), `Target.actions` in an aspect |
| B27 | fixed (corpus-runfiles) | `depset` was not a BUILD-file global, and list-typed attributes did not accept depsets (`@openssl//:BUILD.bazel:508`, `select({...: depset([...])})`) |
| B36 | fixed (corpus-ccshared) | `ctx.actions.write(execution_requirements=…)` rejected (rules_cc_autoconf `ctx_actions_write.bzl`; 360 OpenROAD targets) |
| B37 | fixed (corpus-ccshared) | a repository's BUILD file symlinked into another repository's tree: "build file … resolves outside workspace" (sv-elab's `vendored-hashlib`) |
| B38 | fixed (corpus-ccshared) | `repository_ctx.delete()` of the repository directory itself (bazel_tools `git_repository`'s `git_worker.bzl`; sv-elab's `vendored-slang`) |
| B39 | fixed (corpus-ccshared) | a tool hardlinking a repository file into its output (bazel_lib `copy_to_directory`): publication chmod'ed the shared tree file, then libcxx compiles failed "changed while it was being staged" |
| B40 | 1 (openroad); outputs only | `copy_to_directory` over a source-directory input: native stages the directory as a real directory, genuine's sandbox as a symlink that the tool does not descend, so native's output tree has 301 more files |
| B41 | 1 (openroad); outputs only | rules_js `node_modules` links in `bazel-bin` (`src/web/node_modules`, 3,978 files) do not resolve natively: a relative package link points into another action's cache entry |
| B42 | fixed (corpus-ccshared) | a test's runfile of an `actions.symlink` output of a generated file was a copy at the link's exec path, so `$ORIGIN` of a with_cfg-forwarded `cc_binary` missed its `dynamic_deps` |
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

**B14: internal errors after B11 / B4.** Fixed on branch corpus-parent for
the "has no recorded duplicate-label check" form: with `--keep_going`, a
requested target failing after the evaluator's last flush (a dependency
whose package fails to load, after a select() key's flush) left the
configured targets it planned after that flush unpublished, and the end of
phase 1 pruned their node keys before the final publication; their phase 2
then found no phase-1 record. The keys of unpublished configured targets
are kept now (`tests/eval_keep_going.rs`
`keep_going_failure_after_the_last_flush_plans_as_the_walk`). The
"internal analysis order error" form (tink-cc) was not rerun. The messages: "internal error:
@@protobuf+//bazel/flags/cc:cc_proto_library_header_suffixes has no recorded
duplicate-label check" (×10), "internal analysis order error:
@@bazel_tools//tools/def_parser:def_parser" (tink-cc) and "internal error:
@@bazel_tools//tools/jdk:TestRunner has no recorded duplicate-label check"
(java-maven). They only appear in the same run as a loading failure, so they
are probably a cascade. Recheck after fixing B11 and B4.

**B6: `_repo_mapping` manifest.** Fixed on branch corpus-parent:
RepoMappingManifestAction's transitive packages are the configured
target's, which include its select() keys' (ConfigConditionsProducer shares
the target's TransitiveDependencyState) and, for a dependency on an output
file, the generating rule's closure (its exec tools). The missing lines
were `bazelrc-preset.bzl+,bazel_tools,bazel_tools` (a select() key in
`@bazelrc-preset.bzl`), and skylib's/buildtools' stardoc and genrule tool
repositories. Genuine capture `tests/fixtures/repo_mapping_closure`. For rules-template
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
- B17 (fixed on branch corpus-parent, docs/RULE_EXTENSION.md): "rule(parent=...)
  rule extension is not supported" (`extend.bzl:10`).
- B18 (fixed on branch corpus-ccshared): `cc_shared_library` failed:
  "cc_common.check_experimental_cc_shared_library is not implemented". It
  now returns `--experimental_cc_shared_library` (default false), and
  rules_cc's Starlark implementation runs on the existing
  cc_common/cc_internal surface: the graph structure aspect,
  `CcSharedLibraryInfo`, `exports_filter`, `dynamic_deps` between shared
  libraries and on `cc_binary`, `shared_lib_name`. Genuine fixture:
  tests/fixtures/cc_compile `shared` (aquery of every compile, archive,
  link and SolibSymlink action; the binary runs).
- B19: in transitioned configurations (`k8-fastbuild-ST-…`), solib names and
  rpaths differ. Genuine has `_solib_k8/libST-b44ef279653a_external_Srules_Ucc+…so`
  with rpath `$ORIGIN/../../../_solib_k8/`. bazel-rs has
  `_solib_k8/libST-b44ef279653a/bin_external_…so` with rpath `…/_solib_k8/libST-b44ef279653a`.
  So the linked binaries differed (8 hash differences, `cc_define_test`,
  `cc_asan_test`). Fixed on branch corpus-ccshared: the soname mangles the
  configuration mnemonic, not the output root `bazel-out/<mnemonic>/bin`.

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

**B36–B42 (corpus-ccshared).** OpenROAD's analysis after B18/B30 hit B36
(rules_cc_autoconf), B37 and B38 (sv-elab's repository rules); its
`//:openroad` build hit B39. B40: bazel_lib's `copy_to_directory` walks
its inputs; a source directory input is a symlink in genuine's sandbox
(docs/SOURCE_DIRECTORIES.md "Staging", the declared divergence `view`), so
the tool apparently copies nothing below it, while bazel-rs stages a real
directory and the tool copies `include/` and `share/` of the prebuilt
clang's resource directory. The compiles do not read that tree, so the
binary is identical. B41: in the output tree, `node_modules/<pkg>` is an
absolute link into the producing action's cache entry, where the package
store's relative link does not resolve (the declared `bazel run`
difference of docs/RUNFILES_TREES.md, here in `bazel-bin`).

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
  17.4 against 27.0). It is slower on rules_python pip_parse (46–52 against
  30–36 s; P3 below), on OpenROAD (144–153 against 88 s; P4) and was on
  bazel-gazelle (39.9 against 32.6; 40.2 against 49.8 in the corpus-perf
  rerun). It is even on buildtools.
- **Warm `build --nobuild`:** bazel-rs, with its analysis snapshot, takes
  0.1–0.2 s, like a warm genuine server (0.1–0.4 s) and 15–30× faster than
  genuine `--batch` (2.8–5.9 s). Since corpus-perf the command that wrote
  `MODULE.bazel.lock` (the cold one) saves a snapshot too (P1, fixed), so
  the first repeat hits: abseil 0.19–0.24 s, was 3.2–3.5 s. A command whose
  analysis failed is never saved; OpenROAD's repeats (5,439 targets fail on
  B18/B30) re-analyse: 15.4–17.4 s against genuine `--batch` 8.4 s (was
  65–68 s; P5).
- **Full build** (after the cold analysis): bazel-rs is faster on abseil
  (76 s against 98 s), buildtools (58 against 75), basic_gazelle (32 against
  43) and the small projects. bazel-gazelle (P2, fixed) is now even:
  55.1–59.7 s against genuine's 53.0–62.5 s (main before the fix:
  63.5–71.6 s; the corpus's 113 s ran into B13).
- **Memory:** bazel-rs's peak RSS is 1.3–3.5× lower on cold analysis (for
  example 453 MB against 1,131 MB on abseil). The exceptions are the large JS
  (1.8 GB against 1.35 GB) and rules_python bzlmod (2.6 GB, about equal)
  workspaces.

### Findings and fixes (branch corpus-perf)

Measured with non-LTO release builds of main (460717e5) and the branch,
alternating, three rounds; genuine 9.3.1 `--batch` in the same rounds. The
load average is the 1-minute value when each command started. Profiles:
`--profile`, `BAZEL_RS_TIMING=1` (PERF marks), `BAZEL_RS_TIMING_TIMELINE=1`
(`PERF-TL` spans of every repository execution, download, extension and
module root, docs/MODULE_EXTENSIONS.md "Profiling") and
`BAZEL_RS_EVAL_TIMELINE=1` (the evaluator's slow attempts and lane fetches).

| | load | genuine | main | corpus-perf |
|---|---|---|---|---|
| OpenROAD cold `--nobuild` (exit 1 on bazel-rs) | 18–40 | 87.6 s | 205.4 / 174.4 / 185.7 s | 144.1 / 153.0 / 143.5 s |
| OpenROAD second repeat (exit 1) | 13–33 | 8.4 s (`--batch`) | 67.5 / 66.9 / 65.2 s | 15.4 / 16.1 / 17.4 s |
| OpenROAD first repeat | 14–29 | | 111.7 / 99.6 / 68.1 s | 31.9 / 31.2 / 45.2 s |
| abseil-cpp cold `--nobuild` | 18–27 | | 15.0 / 12.2 / 11.9 s | 13.5 / 13.0 / 13.4 s |
| abseil-cpp first repeat | 19–27 | | 3.51 / 3.20 / 3.27 s | 0.24 / 0.19 / 0.23 s (hit) |
| bazel-gazelle `build //...` (after the cold analysis, outputs and action cache removed) | 22–47 | 58.0 / 62.5 / 53.0 s | 71.6 / 65.0 / 63.5 s | 59.7 / 55.5 / 55.1 s |
| rules_python-pip_parse cold `--nobuild` | 13–30 | 30.3 / 34.5 / 35.7 s | 44.2 / 43.4 / 49.6 s | 46.5 / 51.7 / 50.1 s |

Output identity: stdout and stderr (without timings, progress and PERF
lines) of every abseil-cpp and OpenROAD command match main's; the snapshot
hit after a lockfile-writing cold run passed `BAZEL_RS_SNAPSHOT=verify`
with `BAZEL_RS_SNAPSHOT_VERIFY_STRICT=1`; bazel-gazelle's 2,551 `bazel-bin`
files hash identically before and after the scheduler change.

- **P5: `--keep_going` report, O(artifacts × failed targets)** (fixed).
  After execution (also `--nobuild`), every requested artifact was checked
  against the failed targets by a linear scan (`KeepGoingResult::built`):
  48 s of one core on OpenROAD's 5,439 failed targets, in the cold run and
  in every repeat. Now a set (`KeepGoingResult::not_built`).
- **Git override warm check** (fixed). `qt-bazel`'s `git_override` tree
  (262 MB, 17,632 files) was hashed in full by every command's module
  resolution (3.3 s of `startup.modules`); its warm check now takes digests
  from the persistent digest memo, like registry trees
  (`startup.modules` 0.46–0.52 s).
- **rules_m4 / rules_flex re-executed every command** (fixed).
  `ctx.template("src/m4.h", "src/m4.h", …)` applied twice left `stage:`
  template probes the published tree could never satisfy, so both toolchain
  repositories were fetched again by every repeat (m4 1.5–3.8 s, once 90 s
  on a slow mirror). Such a probe is no longer recorded
  (docs/REPOSITORY_RULES.md).
- **P1: no snapshot after a lockfile write** (fixed; soundness-sensitive,
  docs/ANALYSIS_SNAPSHOT.md "The workspace lockfile as written"). The run
  that writes MODULE.bazel.lock now records the lockfile's READ as the
  bytes it writes, and the registry downloads it made without a checksum
  settle when the written lockfile records their hash (or absence), as do
  the `metadata.json` reads the written lockfile answers. Not recorded so:
  `--native-lockfile=<path>` before that path exists, and `readonly`.
- **P2: bazel-gazelle build, ready-action order** (fixed). The profile
  showed `GoStdlib` starting 17 s after its only input
  (`GoToolchainBinaryBuild`, the Go builder) finished: the 16 jobs took
  ready C++ compilations by action id first. With more than one job the
  scheduler now starts the ready action heading the longest chain of
  dependent actions first (`Graph::ready_key`; Bazel's evaluator orders by
  depth likewise). The jobs stay busy to the end (16.0 per 5 s window from
  the first to the last but one).
- **P6: OpenROAD `build //:openroad` is 2.2× genuine** (open; corpus-ccshared,
  footnote 18): 1,536 s against 676–702 s for the same 13,480 actions and
  7,642 sandboxed spawns, critical path 77 s against 55–59 s. Not profiled
  yet; each C++ compile stages the hermetic LLVM toolchain's headers and
  sysroot (thousands of inputs) by hardlink, where genuine's
  processwrapper-sandbox makes symlinks.
- **OpenROAD cold `--nobuild` after B18/B30** (corpus-ccshared): now that
  analysis passes, 242.5 s against genuine's 89.6 s (peak RSS 9.2 GB
  against 3.0 GB), the first repeat 58.2 s against 20.5 s, the second
  1.8 s (snapshot hit) against 8.9 s.
- **P4: OpenROAD cold `--nobuild` is still 1.6–1.7× genuine** (open). With
  the fixes above, ~145 s against 88 s. The process uses about one core
  (95 s user + 65 s sys over 145 s; genuine 246 + 54 s over 88 s). The
  timeline shows why: of ~550 repositories (348 executed; 84–97 s of
  execution summed) the evaluator demands most one at a time. Pool workers
  never fetch: a load that needs an unfetched repository parks, and the
  coordinator's single FIFO lane materializes it (registry module roots
  `module-root` on `main`, e.g. freetype 7.6 s, icu 4.9 s, tcl_lang 4.1 s,
  git 6.9 s) before the parked load runs again and discovers the next
  one; module-root warm-ups and repository speculation start only when
  the demand is discovered, so they overlap the lane by seconds, not the
  chain. Long single waits stall everything: one run spent 91 s in
  `Pkg(@@rules_m4++…m4_v1.4.18//rules_m4_internal)` waiting for a 90 s
  download of `m4-1.4.18.tar.xz` from mirrors.kernel.org. Module
  resolution itself is 13–26 s (the registry files arrive in 4.7 s; the
  rest is the `qt-bazel` `git_override` clone, 262 MB). Genuine fetches as
  many repositories at once as Skyframe evaluates. The fix is in the
  evaluator: demands of different pool attempts fetched side by side (the
  lane's queue fetched concurrently, or speculation requested for every
  queued demand), keeping the order-independence of recorded inputs
  (docs/EVALUATOR.md "Repositories"). corpus-fetch tried the lane's
  queue fetched side by side (branch `corpus-fetch-lane`, not merged): it
  was not the bottleneck; the next design items are P4a and P4b below.
- **P3: rules_python-pip_parse cold fetch is ~1.4× genuine** (open; same
  cause as P4). The timeline is one chain: the Python interpreter
  (8.8 s), the wheels, then rules_rust's `stable_tools` (16 s, on the
  fetch pool but alone), then the `crate` extension (12.7 s) whose
  evaluation fetches `rust_host_tools` (12.2 s) on `main`: both rust
  repositories decompress the same `rustc`, `llvm-tools` and `rust-std`
  archives (the second from the download cache, 5.3 s of xz alone), one
  after the other. Genuine runs them side by side. Since corpus-fetch the
  second extraction reads the first's decoded tar (`rust_host_tools`
  11.8 s → 3.4 s); the chain itself remains (below).
- **First repeat after a cold OpenROAD run** (open, by design): 31–45 s,
  because the files of every repository tree published by the cold run
  were younger than the digest memo's racy margin, so the first repeat
  hashes all 2 GB of them once (`PERF-DMEMO registry=152,302 files`); the
  second repeat reads none.
- **Memory:** bazel-rs's peak RSS is 1.3–3.5× lower on cold analysis (for
  example 453 MB against 1,131 MB on abseil). The exceptions are the large JS
  (1.8 GB against 1.35 GB) and rules_python bzlmod (2.6 GB, about equal)
  workspaces.

### Findings and fixes (branch corpus-fetch)

Non-LTO release builds of main (0b26c20d) and the branch, alternating with
genuine 9.3.1 `--batch`, three rounds, the corpus harness's flags and
isolation (note 19). The "corpus-fetch" column is the measured build:
the merged changes plus the side-by-side lane experiment (kept on branch
`corpus-fetch-lane`, not merged: no measured win, the riskiest part). The
load average is the 1-minute value when each
command started.

| | load | genuine | main | corpus-fetch |
|---|---|---|---|---|
| OpenROAD cold `--nobuild` (exit 1 on bazel-rs) | 7–23 | 77.9 / 85.5 / 84.6 s | 182.3 / 211.5 / 198.7 s | 184.3 / 248.9 / 195.1 s |
| rules_python-pip_parse cold `--nobuild` | 8–29 | 34.9 / 37.8 / 35.8 s | 71.6 / 62.2 / 49.5 s | 61.0 / 38.7 / 52.7 s |
| abseil-cpp cold `--nobuild` | 8–36 | 17.5 / 16.6 / 16.1 s | 40.6 / 38.7 / 16.7 s | 24.4 / 44.6 / 41.9 s |
| library suite (`cargo test --lib`, debug) | 11–28 | | 18.3 / 18.0 / 18.2 s | 17.9 / 17.8 / 22.7 s |
| the same from a background job (`trap '' INT QUIT`) | 12–32 | | 106.7 / 113.1 / 120.0 s | 18.7 / 18.6 / 25.2 s |
| 2,000 trivial genrules, `build` (`trap '' INT` or not) | 20–27 | | 1.0–1.03 s either way | 0.99–1.30 s either way |

- **Experiment, not merged: the repository lane side by side** (branch
  `corpus-fetch-lane`, commit 76fc6e48 there). The pool's queued lane
  demands (registry module roots, extension and local-rule blockers) ran
  on up to 8 threads (`BAZEL_RS_EVAL_LANE_WIDTH`), each demand's parked
  loads re-queued when its own fetch ended instead of behind the queue's
  head (icu's 4.4 s root held 20 ready roots' demanders); what a fetch
  wrote and observed stayed keyed by its blocker. On OpenROAD the lane's
  busy time (the union of its module-root spans) fell from 102.5 s to
  80.0 s (175 and 146 fetches, up to 8 overlapping), but the wall time
  did not move beyond the mirrors' noise (table): the run is not
  lane-bound. The timeline (`BAZEL_RS_TIMING_TIMELINE`,
  `BAZEL_RS_EVAL_TIMELINE`) shows the process at ~0 cores most of the
  time, waiting on one chain at a time, for the two causes below.
  Single long downloads remain too (`m4-1.4.18.tar.xz` 33 s,
  kepler-formal's `boost_headers` 11–23 s, `abc+` 15 s during the
  configuration phase).
- **P4a (next design item): pool loads that need a direct or
  extension-generated repository wait for the sequential pass.** Such a
  load parks (`INLINE`); its repository speculation starts, but every
  configured target behind it waits until the sequential pass reaches it
  in the requested targets' order, so later repositories are discovered
  only then (`//:_openroad.so`, the first requested target, took 105 s;
  `openssl+` was first demanded at +119 s, `capnp-cpp+` at +170 s, each a
  lone 4–7 s root). Direction: let the parked load adopt the finished
  speculation on the pool (as local-rule blockers already do,
  docs/EVALUATOR.md "The lane resolves what such a load waits for"),
  committed where the tape replays the load, with the recorded-input
  order checks the sequential pass applies.
- **P4b (next design item): module extensions never run ahead on a cold
  run.** An extension speculation is eligible only with a reusable
  lockfile entry (docs/COLD_ANALYSIS.md), so every extension evaluates on
  the sequential pass, one after another: pip_parse's `crate` extension
  waits for rules_rust's `stable_tools` (15–17 s). Direction: evaluate
  extensions ahead of the sequential order and commit them
  deterministically (console output, lockfile entries and recorded
  inputs at the sequential position), as Bazel's Skyframe evaluates
  them in parallel.
- **Decoding shared between rules** (docs/COLD_FETCH.md). A finished
  rule's decoded tar stream is kept for any thread, by decoder and
  archive SHA-256, at most 1 GiB in all (`archive::RETAINED`):
  `rust_host_tools` (the `crate` extension, on the coordinator) reads the
  `rustc`, `llvm-tools` and `rust-std` tar streams `stable_tools` decoded on
  a fetch worker, 11.8 s → 3.4 s on pip_parse's chain. Only decoding is
  skipped; every entry and check runs as before.
- **Inherited-ignored signals are held** (docs/PROCESS_WRAPPER.md,
  src/signals.rs). Started from a background job (SIGINT/SIGQUIT ignored)
  or under nohup, every start added a `pre_exec` reset hook, which forces
  std's fork path; forking a large many-threaded process is slow. bazel-rs
  now blocks such signals in every thread from main's first line with a
  handler that does nothing (it still never acts on them), so exec(2) and
  std's cleared start mask give children the defaults without a hook.
  Starts that want the process-wrapper's defaults (and the spawn
  supervisors) keep posix_spawn; `Client`/`Jvm` starts and
  `spawn_lock::spawn` without defaults (which inherits) get a hook that
  sets the held signals back to SIG_IGN in the child. The CLI's actions
  were not affected (they start through the posix-spawned supervisor);
  direct starts from the big process (git for git repositories,
  persistent workers, credential helpers) and every library-test start
  were: the library suite from a background job takes 18.6–25.2 s instead
  of 107–120 s. tests/signal_dispositions.rs (genuine's receipt) passes
  unchanged.

## Recommendations

1. B2 first. One table change, accepting the no-op startup and command
   options, unblocks 16 projects' first command with no edits.
2. Then B1, B11 and B3 (C++ with gtest, boringssl users, skylib/aspect
   helpers), then B9 and B10 (module resolution for rules_cc, protobuf,
   OpenROAD, rules_kotlin, rules_swift users). OpenROAD needs B9 (or B22)
   and B10.
3. B15 (symlinked runfiles for tests) and B6 (`_repo_mapping` entries) were
   silent behaviour differences; both are fixed with parity fixtures.
4. Rerun the corpus after each fix: `oss_corpus.py run --only <project>`,
   then drop the corresponding workaround edits from the JSON.
