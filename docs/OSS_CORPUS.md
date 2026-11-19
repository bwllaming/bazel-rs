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
| abseil-cpp | **OK**¹⁵ ¹⁹ ²² | 13.1–13.8 / 10.5–12.6 (cold-eval²²; main 12.6–13.2; download cache pre-filled: 8.4–9.1, main 9.7–9.9; before: 14.8 / 12.0, rerun 16.1–17.5 / 16.7–44.6) | 3.3 / 0.2 / 0.2 | 97.7 / 76.3 | 1131 / 453 | 2767, 0, 0, 0 | PASSED:252 / PASSED:252 | (B2) |
| limdor-cpp | **OK** | 11.0 / 6.7 | 2.8 / 0.1 / 0.1 | 7.3 / 0.7 | 704 / 217 | 18, 0, 0, 0 | PASSED:1 / PASSED:1 | |
| limdor-rust | **OK** | 29.0 / 20.5 | 2.8 / 0.1 / 0.2 | 4.7 / 1.9 | 807 / 426 | 91, 0, 0, 0 | PASSED:1 / PASSED:1 | |
| examples-bzlmod-01 | **OK** | 11.5 / 6.7 | 3.0 / 0.1 / 0.1 | 5.4 / 2.6 | 641 / 217 | 187, 0, 0, 0 | — | |
| examples-bzlmod-05 | **OK** | 8.6 / 3.5 | 2.1 / 0.1 / 0.1 | 2.2 / 0.1 | 332 / 80 | 1, 0, 0, 0 | — | |
| examples-cpp-tutorial | **OK** | 10.8 / 6.7 | 2.8 / 0.2 / 0.1 | 3.6 / 0.7 | 755 / 203 | 13, 0, 0, 0 | — | |
| examples-rust-ffi | **OK** | 27.0 / 17.4 | 2.9 / 0.2 / 0.1 | 4.0 / 1.6 | 788 / 373 | 57, 0, 0, 0 | — | (B2) |
| rules_go-basic_gazelle | **OK** | 27.5 / 23.9 | 5.9 / 0.2 / 0.2 | 42.6 / 32.0 | 1281 / 490 | 117, 0, 0, 0 | PASSED:1 / PASSED:1 | |
| rules_python-pip_parse | **OK**¹⁵ ¹⁹ ²² | 28.0–33.4 / 29.6–30.5 (cold-eval²²; main 34.3–36.2; download cache pre-filled: 21.7–22.6, main 26.0–26.5; before: 30.3–35.7 / 46.5–51.7, rerun 34.9–37.8 / 38.7–61.0) | 3.5 / 0.2 / 0.2 | 4.7 / 1.1 | 1625 / 614 | 56, 0, 0, 0 | PASSED:2 / PASSED:2 | (B2) |
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
| junit5-starter | DIFF¹⁷ ²¹ | 25.9 / 21.4 | 3.4 / — / 0.1 | 7.6 / 5.4 | 1239 / 481 | 100, 28, 0, 0 | PASSED:1 / PASSED:1 | C2 |
| rules_zig | RS-FAIL¹⁵ ²³ | 36.0 / 37.7 (exit 1) | 17.2 / — / 12.5 (exit 1) | 39.5 / 12.1 (exit 1) | 2233 / 1956 | 0, 1292, 0, 0 | PASSED:211 SKIPPED:4 / SKIPPED:4 | B13c |
| capnproto | RS-FAIL⁴ | 17.1 / 11.3 (exit 1) | 3.3 / 0.1 / 1.5 (exit 1) | 22.6 (exit 1) / 7.5 (exit 1) | 1127 / 384 | 317, 1085, 0, 0 | both: FAILED TO BUILD:5 SKIPPED:14 | B11, B20, (B2) |
| tink-cc-examples | RS-FAIL | 17.3 / 17.1 (exit 1) | — | — | 1127 / 490 | — | — | B11, B14 |
| examples-java-maven | DIFF²¹ | 33.0 / 25.4 | 3.3 / — / 0.1 | 7.0 / 5.5 | 1137 / 479 | 133, 2, 0, 0 | PASSED:2 / PASSED:2 | C2 |
| rules_cc | RS-FAIL¹⁵ ²³ | 23.7 (exit 1) / 21.4 (exit 1) | 7.8 (exit 1) / — / 5.9 (exit 1) | 52.9 (exit 1) / 53.6 (exit 1) | 2634 / 1647 | 1559, 425, 0, 1 | FAILED:1 PASSED:453 SKIPPED:2 / FAILED:5 FAILED TO BUILD:2 PASSED:317 SKIPPED:2 | B32 |
| examples-rust-deps-cargo | RS-FAIL | 44.0 / 0.3 (exit 1) | 2.9 / 0.2 / 0.3 (exit 1) | 28.8 / 0.3 (exit 1) | 1034 / 29 | 0, 103, 0, 0 | — | B9, (B2) |
| protobuf | BOTH-FAIL¹⁵ ²³ | 44.6 (exit 1) / 62.3 (exit 1) | 7.3 (exit 1) / — / 16.2 (exit 1) | — | 2075 / 3566 | — | — | B33 (2 targets left) |
| openroad | OK²¹ ²² (nobuild, query) | 69.9–76.3 / 109.9–110.7 (one 198.8: a mirror's connect timeout) (cold-eval²²; main 139.4–173.2; download cache pre-filled: 67.1–67.4, main 86.3–87.0 — openroad-perf's 87.2–96.4²⁰ were such runs; corpus-parity3²¹ at load 52–91: 78.0 / 178.1) | 8.9 / — / 1.2–1.3 (corpus-parity3: 10.4 / — / 1.4) | `//:openroad` 676–702, 693.5 / 773.9 (was 1536) | 3014 / 6957–6983 (was 9169) | `//:openroad` before corpus-parity3: 15223, 3978, 301, 0; the B40/B41 targets now match²¹ | — | (C1) |
| rules_kotlin | **OK**²¹ ²³ (nobuild, query) | 38.0 / 42.6 | 6.9 / — / 0.2 | — | 2334 / 2109 | — | — | |
| riegeli | BOTH-FAIL⁶ | 30.2 (exit 1) / 31.0 (exit 1) | 4.1 / 0.2 / 3.7 (all exit 1) | 78.2 (exit 1) / 92.5 (exit 1) | 1427 / 794 | 2800, 0, 0, 0 | PASSED:2 / PASSED:2 | |
| examples-frontend | DIFF⁷ ²¹ | 87.8 (exit 1) / 128.8 (exit 1) | 64.5 / — / 96.5 (all exit 1) | 60.5 (exit 1) / 135.9 (exit 1) | 1524 / 1933 | 215801, 12, 2, 0 | PASSED:14 / PASSED:14 | C3, (B2) |
| rules_jvm_external-simple | BOTH-FAIL¹⁵ ²¹ | 55.8 (exit 1) / 30.4 (exit 1) | 3.7 (exit 1) / — / 1.6 (exit 1) | 3.9 (exit 1) / 1.5 (exit 1) | 1726 / 607 | 0, 0, 0, 0 | — | B47 |
| flatbuffers | BOTH-FAIL⁸ | 21.7 (exit 1) / 8.2 (exit 1) | — | — | 864 / 184 | — | — | B12 |
| examples-java-tutorial | DIFF | 18.7 (exit 1) / 15.1 (exit 1) | 3.2 / 0.2 / 1.6 (both exit 1) | 5.9 (exit 1) / 4.2 (exit 1) | 881 / 266 | 11, 2, 0, 0 | — | C2 |
| examples-configurations | BOTH-FAIL⁹ ²³ | 10.0 (exit 1) / 5.4 (exit 1) | 2.2 / — / 0.2 (both exit 1) | 2.2 (exit 1) / 0.2 (exit 1) | 406 / 129 | 0, 0, 0, 0 | — (both: no test runs) | |

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
   check, analyses on and fails later (fixed on corpus-parity4, B24).
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

20. Rerun 2026-11-18 on branch openroad-perf (b8ce9666 plus its
    commits; non-LTO builds), by hand with the harness's flags and
    isolation, three rounds alternating with b8ce9666 (load 12–42): cold
    `--nobuild //...` (every fetch), its first repeat, a snapshot miss
    (`--define` changed) and hit; then `build //:openroad` after the
    `--nobuild` (action cache and store removed; genuine: `clean`) once per
    tool (load 21–34). The 15,223 common `bazel-bin` files of `//:openroad`
    match genuine's byte for byte (all compared; only genuine: B41's 3,978,
    only bazel-rs: B40's 301); the action dump of `//src/utl:utl
    //src/sta:opensta_lib` is byte-identical to b8ce9666's. P6 and the
    OpenROAD memory below.

21. Rerun 2026-11-18/19 on branch corpus-parity3 (main 97611a75 = 0.4.30
    plus its commits; non-LTO), phases `nobuild_cold,nobuild_warm,query,
    build,test` for junit5-starter, examples-java-maven,
    rules_jvm_external-simple and examples-frontend,
    `nobuild_cold,nobuild_warm,query` for rules_kotlin and openroad (load
    14–91 with other workers' runs). Fixed: B8, B25 (not reproduced: the
    warm `test` passes 14/14; 502d3267's sentinel fix covers it), B31, B35
    (and the next rules_kotlin errors: `check_java_toolchain_is_declared_on_rule`,
    mutable `default_jvm_opts`, per-attribute aspect views, genquery
    `buildfiles` scope), B40, B41, B44, B45 and rules_android's `$x`
    private attribute keywords. examples-java-maven builds and passes both
    tests; only C2 is left. examples-frontend: query (8,701 labels) and
    tests match; `bazel-bin` differs by 10 rules_js lifecycle `.bin` files
    (C3) and next.js's random build id (2 files each side); was 170,965
    only-genuine files (B41). rules_kotlin: `query //...` matches (1,319
    labels), one analysis error left (B46). rules_jvm_external-simple fails
    on both (no Android SDK); query matches. OpenROAD: `build --nobuild
    //...` exits 0 on both, `query //...` matches (7,079 labels); by hand on
    the openroad-perf state: `build //src/utl:utl` runs 829 actions (630
    spawns), genuine's exact counts (was 1,109/867), its 40 common
    `bazel-bin/src/utl` files are byte-identical (only genuine: C1's
    params file); `aquery --output=summary 'deps(//src/utl:utl)'` is
    identical; after `build //src/web:node_modules
    @llvm//toolchain:linux_x86_64_cc_toolchain_resource_directory`,
    `bazel-bin/src/web` holds genuine's 7,387 files (B41) and the resource
    directory genuine's 2 (B40). The full `//:openroad` build was not
    rerun.

22. Rerun 2026-11-18/19 on branch cold-eval (main 97611a75 plus its
    commits; non-LTO builds), phase `nobuild_cold` only, by hand with the
    harness's flags and isolation, three rounds each, main and the branch
    alternating: fully cold (every download from the public mirrors, with
    genuine `--batch` in the same rounds; load 7–19) and with the download
    cache pre-filled (the repository cache's `downloads` seeded from an
    earlier run, everything else fresh; load 6–19). Exit codes, stdout,
    stderr (without timings and progress) and `MODULE.bazel.lock` match
    main's in every round (one fully cold OpenROAD round of the branch
    printed a `m4-1.4.18.tar.xz` mirror's connect-timeout warning, the
    network's); abseil-cpp's and pip_parse's action dumps
    (2,313 and 88 actions) and snapshot recordings (6,307 and 39,523
    observation, environment and taint records, `BAZEL_RS_SNAPSHOT=record`)
    are identical. Peak RSS (`plan.end` high-water mark, pre-filled cache):
    OpenROAD 7.17 GB against 7.15 GB, abseil-cpp 566 MB against 514 MB,
    pip_parse 808 MB against 654 MB (`stable_tools` and `rust_host_tools`
    now extract the same rust archives side by side).

23. Rerun 2026-11-19 on branch corpus-parity4 (main 0d2e0367 = 0.4.32 plus
    its commits; non-LTO), phases `nobuild_cold,nobuild_warm,query,build,
    test` for examples-configurations, rules_cc and rules_zig,
    `nobuild_cold,nobuild_warm,query` for rules_kotlin and protobuf (load
    17–25). Fixed: B46 (rules_kotlin: `build --nobuild //...` exits 0 on
    both tools, `query //...` matches, 1,319 labels), B24
    (examples-configurations: both tools abort on genuine's `error loading
    project files: Every .bzl file must have a corresponding package, but
    '//alias/project_main:PROJECT.scl' does not have one…`, `test` included;
    `query` matches, 54 labels), most of B33 (protobuf: 28 of its 30
    analysis errors gone; `query` matches, 3,411 labels; genuine still
    fails on rules_ruby) and part of B32 (rules_cc: 317 passing tests, was
    258; genuine 453). rules_zig: unchanged, B13c (not attempted, design
    below).

**Summary.** 37 projects ran. On 15 of them bazel-rs matches genuine exactly (rules_kotlin since corpus-parity4, footnote 23, its build and tests not compared)
(10 before the corpus-parent rerun, footnote 17: diff.bzl, bazel-skylib
and buildtools since B6's fix; bazel-skylib's and buildtools' own failing
tests fail identically on both tools; junit5-starter and, since
corpus-parity3, examples-java-maven differ only by C2; OpenROAD's compared
phases and outputs match too, its full build not rerun, footnote 21):
same exit codes, `query //...` labels, `bazel-bin` file lists, sampled
SHA-256s and test results. abseil-cpp is the largest: 2,767 outputs and 252
passing tests. Two more (rules-template, yalantinglibs; riegeli too since the
corpus-runfiles rerun, and examples-configurations since corpus-parity4)
fail the same way on both tools. Two are clean except for one bug each: bazel-gazelle (B13,
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
| B13c | 1 (rules_zig) | one toolchain implementation for two execution platforms in one configuration (ConfiguredTargetKey.executionPlatformLabel), a fatal error (open: needs the execution platform in the configured-target key; design refined on corpus-parity4, below) |
| B13d | fixed (= B29) | rules_cc's own `cc_test.bzl`/`cc_binary.bzl` "cannot use private API" |
| B13e | fixed (corpus-parity2) | `cc_import` of `mylib.dll`/`mylib.lib` on Linux: "does not match expected filetype" |
| B15 | fixed (corpus-runfiles) | test runfiles were hardlinks, not symlinks |
| B7 | fixed (corpus-parity2) | transition: "Starlark flag and its alias have different values: False and "false"" |
| B5 | fixed (corpus-parity2) | `local_repository` → `rctx.symlink(path, ".")` rejected (now a copy plus a `watch_tree` input, docs/LOCAL_REPOSITORIES.md) |
| B8 | fixed (corpus-parity3) | declared output "crosses a package boundary": a `declare_file`/`declare_directory` output may lie below a subpackage; predeclared outputs keep genuine's `Label ... is invalid because ... is a subpackage` |
| B17 | fixed (corpus-parent) | `rule(parent=…)` rule extension (docs/RULE_EXTENSION.md) |
| B18, B19 | fixed (corpus-ccshared) | `cc_shared_library` (`cc_common.check_experimental_cc_shared_library`); solib naming in transitioned configurations |
| B26 | fixed | depset elements holding functions that are not top-level defs (skylib `DirectoryInfo` in rules_cc `cc_args`; openroad) |
| B29 | fixed (corpus-parity2) | "file '//cc/private/rules_impl:cc_test.bzl' cannot use private API": the private-API allowlist now treats the main repository as rules_cc when the root module is rules_cc |
| B20 | 1 (capnproto) | `rule(output_to_genfiles=True)` |
| B21 | 1 (rules_zig) | `attr.int_list` in repository rules |
| B22 | 1 (openroad) | `git_override` of a repository with submodule gitlinks |
| B23 | 1 (examples-rules) | predeclared `attr.output` given as `":name"` |
| B12 | 1 (flatbuffers) | `@local_config_platform` unknown |
| B24 | fixed (corpus-parity4) | PROJECT.scl project files: looked up and loaded before the analysis, genuine's `error loading project files` (docs/CONFIGURATION.md "PROJECT.scl project files"); a project with configurations fails explicitly |
| B25 | fixed (502d3267; not reproduced in corpus-parity3) | "needs-repository: repository @@rules_diff+ is unfetched and this thread does not fetch" on a warm `test` (a worker's sentinel answer cached by the ignored-subdirectories memo) |
| B30 | fixed (corpus-ccshared) | `provider in target` fails "Target membership requires an exported provider constructor" in with_cfg's `transitioning_alias.bzl:55` (toolchains_llvm's `*_with_cfg` glibc/libunwind/libcxxabi targets; one of with_cfg's forwarded builtin providers is not an exported constructor natively: `apple_common.Objc`) |
| B31 | fixed (corpus-parity3) | `config_common.config_feature_flag_transition`: loads; no transition unless the attribute sets flag values, which fails explicitly (docs/TEST_FLAGS.md) |
| B32 | 1 (rules_cc's own tests) | Starlark transitions on unmodelled native options. Fixed on corpus-parity4: `--fission`, `--build_test_dwp`, `--legacy_whole_archive`, `--incompatible_remove_legacy_whole_archive` are configuration options and `--dynamic_mode` is transitionable (docs/CONFIGURATION.md); a fission compile's `.dwo` is declared where genuine declares it. Left: the FDO and Propeller options (`fdo_optimize`, `fdo_profile`, `fdo_instrument`, `cs_fdo_profile`, `xbinary_fdo`, `propeller_optimize*`, `memprof_profile`), `custom_malloc` (a late-bound dependency per configuration), `instrumentation_filter`, `experimental_cpp_modules`, `save_temps`, `grte_top`, `start_end_lib`, `ltoindexopt`/`ltobackendopt`/`per_file_ltobackendopt`; `cc_internal.freeze` of mutable structs / unfrozen functions (38 targets, the thin-LTO and linkstamp tests); `objc.target_should_alwayslink` with an `alwayslink` attribute; Windows artifact name patterns (`*_def_file_*_dll_pattern`) |
| B33 | 1 (protobuf); 2 targets left | protobuf analysis. Fixed on corpus-parity4: symbolic-macro visibility delegation and alias select() keys under `--incompatible_config_setting_private_default_visibility` (5 visibility errors), `declare_file(sibling=)` from another package, per-attribute aspect views of aspects propagating over one attribute (`UpbMinitableCcInfo`/`JavaInfo` on proto_library targets), `Target.actions` of targets merged with aspects and of a rule under test's dependencies, `--dynamic_mode` transitions (rules_fuzzing). Left: transitions on `instrumentation_filter` (`generated_sources_not_coverage_instrumented`; native's filter is per command, not per configuration) and `experimental_output_paths=strip` (`test_compile_path_mapping`; path mapping) |
| B34 | 1 (rules_python-bzlmod); not blocking | `external/other_module+/other_module/pkg/bin_zipapp.zip` differs from genuine's: rules_python's zipper stores each input's mode and relative symlinks as it finds them in the sandbox; native staging hardlinks read-only store blobs (docs/HARDLINK_STAGING.md) |
| B43 | 1 (with_cfg); not blocking | genuine builds `cc_no_copt_binary/binary_/binary.runfiles_manifest` and `.repo_mapping` (the manual `cc_binary` behind with_cfg's frontend and transitioning alias, a no-op transition); bazel-rs does not (not investigated) |
| B35 | fixed (corpus-parity3) | rules_kotlin: `download_and_extract(rename_files=…)`, `Target.actions` through an alias, TestingAspectInfo (per-attribute aspect views), `check_java_toolchain_is_declared_on_rule`, mutable `default_jvm_opts`, genquery `buildfiles` scope |
| B27 | fixed (corpus-runfiles) | `depset` was not a BUILD-file global, and list-typed attributes did not accept depsets (`@openssl//:BUILD.bazel:508`, `select({...: depset([...])})`) |
| B36 | fixed (corpus-ccshared) | `ctx.actions.write(execution_requirements=…)` rejected (rules_cc_autoconf `ctx_actions_write.bzl`; 360 OpenROAD targets) |
| B37 | fixed (corpus-ccshared) | a repository's BUILD file symlinked into another repository's tree: "build file … resolves outside workspace" (sv-elab's `vendored-hashlib`) |
| B38 | fixed (corpus-ccshared) | `repository_ctx.delete()` of the repository directory itself (bazel_tools `git_repository`'s `git_worker.bzl`; sv-elab's `vendored-slang`) |
| B39 | fixed (corpus-ccshared) | a tool hardlinking a repository file into its output (bazel_lib `copy_to_directory`): publication chmod'ed the shared tree file, then libcxx compiles failed "changed while it was being staged" |
| B40 | fixed (corpus-parity3) | `copy_to_directory` over a source-directory input: sandboxed spawns now see the directory as a symlink, as genuine's sandboxes stage it (docs/SOURCE_DIRECTORIES.md) |
| B41 | fixed (corpus-parity3) | rules_js `node_modules` links in `bazel-bin`: a relative symlink output keeps its text in the output tree (docs/UNRESOLVED_SYMLINKS.md) |
| B42 | fixed (corpus-ccshared) | a test's runfile of an `actions.symlink` output of a generated file was a copy at the link's exec path, so `$ORIGIN` of a with_cfg-forwarded `cc_binary` missed its `dynamic_deps` |
| B44 | fixed (corpus-parity3) | `ctx.actions.write(mnemonic=...)` was dropped: rules_cc's module maps are `CppModuleMap` in aquery, the profile and `Target.actions` |
| B45 | fixed (corpus-parity3) | compiles staged the toolchain's `all_files` instead of `compiler_files` (`as_files` for `assemble`), so `build //src/utl:utl` also built the toolchain's link-time runtimes (1,109 actions against genuine's 829) |
| B46 | fixed (corpus-parity4) | `Target.actions` `argv` is empty for an action whose `Args` use `map_each` (deferred to execution natively; genuine expands it at analysis): rules_kotlin's `windows_coverage_test_suite_flag_test` (`JavaLauncherMaker`) fails analysis on `jvm_flags[0]`, and the `javac_receives_the_plugins_of_the_flag` test sees an empty `Javac` argv |
| B47 | 1 (rules_jvm_external-simple); not blocking (genuine fails too, no Android SDK) | rules_android's `android_split_transition` reads `//command_line_option:Android configuration distinguisher`: "transition inputs ... do not correspond to valid settings" (an unmodelled Android native option) |
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
`declare_directory` / `declare_file` outputs this way. Fixed on
corpus-parity3: only predeclared outputs (targets of the package) are
checked, with genuine's `Label '//p:sub/x' is invalid because 'p/sub' is a
subpackage; perhaps you meant to put the colon here: '//p/sub:x'?`
(tests/corpus_parity3.rs).

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
bazel-rs does not run that check. Fixed on corpus-parity4 (footnote 23):
both tools abort the same way.

**B25.** In examples-frontend, the `test //...` run after `build //...`
failed every target with "While resolving toolchains … invalid registered
toolchain '//diff/toolchain/...': needs-repository: repository @@rules_diff+
is unfetched and this thread does not fetch". The earlier commands had
fetched that repository. It looks like an internal fetch-scheduling bug on a
warm command. Not reproduced on corpus-parity3 (the warm `test` passes
14/14): 502d3267 stopped the ignored-subdirectories memo from caching a
worker's sentinel answer, the same path (a recursive registration pattern
in a repository a pool attempt may not fetch).

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

**B13c (design, corpus-parity4).** Not attempted: the change cannot be
made soundly in one step. What genuine does
(ToolchainsForTargetsProducer/DependencyResolver, ConfiguredTargetKey): a
toolchain implementation's key carries the execution platform of its
parent's exec group (`executionPlatformLabel`), and only those edges; every
other configured target stays keyed by (label, configuration). So one
target can exist twice in one configuration, differing only in the
execution platform its own exec-transitioned tools see. A sound native
design:

1. `ConfiguredTargetKey` gains `exec_platform: Option<Label>`, set only on
   toolchain-resolution edges (the resolved implementation and an alias
   chain to it), `None` everywhere else, so existing keys, their ordering
   and their encodings stay byte-identical when it is `None` (the snapshot
   codec writes the field only when present, with a format-version bump).
2. Inside a key with `exec_platform`, the exec transition of the target's
   own exec-configured dependencies uses that platform instead of the one
   its own toolchain resolution picks (genuine: the toolchain's `cfg =
   "exec"` attributes follow the parent's execution platform).
3. Every place that collapses keys to labels needs an audit: the
   evaluators' memo/`configured_contains`, linearize pass 1's
   `toolchain_implementations` claim (today the whole-command stop),
   the action owners and output paths (two keys of one target in one
   configuration must not register the same outputs: genuine relies on
   the shared-action check, `ActionConflict`; the exec-configured tools
   differ, the toolchain target's own outputs usually do not), cquery
   (`--output` shows the same label twice; genuine prints both), aquery
   and the analysis snapshot's key interning.

The cost is in step 3: the claim in linearize pass 1 assumes one node per
(label, configuration); removing it without the conflict check would let
two nodes write one output. The order: the key field and codec (no
behaviour change), then the exec-platform propagation behind the existing
whole-command stop (verified by the walk/eval/verify modes), then the
shared-action conflict check, and only then lifting the stop.

**B36–B42 (corpus-ccshared).** OpenROAD's analysis after B18/B30 hit B36
(rules_cc_autoconf), B37 and B38 (sv-elab's repository rules); its
`//:openroad` build hit B39. B40: bazel_lib's `copy_to_directory` walks
its inputs; a source directory input is a symlink in genuine's sandbox
(docs/SOURCE_DIRECTORIES.md "Staging", the declared divergence `view`), so
the tool apparently copies nothing below it, while bazel-rs stages a real
directory and the tool copies `include/` and `share/` of the prebuilt
clang's resource directory. The compiles do not read that tree, so the
binary is identical. (openroad-perf: matching genuine means staging a
source directory of a repository tree as a directory symlink under the
sandbox strategies — genuine's local execution shows the real directory —
not done.) B41: in the output tree, `node_modules/<pkg>` is an
absolute link into the producing action's cache entry, where the package
store's relative link does not resolve (the declared `bazel run`
difference of docs/RUNFILES_TREES.md, here in `bazel-bin`).

**B31–B35** are the next blockers the corpus-parity2 rerun found (details in
the table above and footnote 15). B31, B35, B40, B41, B44 and B45 are fixed
on corpus-parity3 (footnote 21); B46 and B47 are what that rerun found next.

### Cosmetic differences

- C1: C++ link actions' `<output>-N.params` files remain in genuine's
  `bazel-bin`, not in bazel-rs's.
- C2: genuine leaves JavaBuilder's `_javac/<target>/<target>_classes/*.class`
  in `bazel-bin`; bazel-rs does not (genuine's persistent JavaBuilder
  worker writes them into the shared execution root; native workers have
  their own working directories; not matched).
- C3: genuine leaves rules_js lifecycle hooks' undeclared
  `node_modules/.aspect_rules_js/<pkg>/node_modules/.bin/*` shims in
  `bazel-bin` (10 files in examples-frontend; no action declares them);
  bazel-rs does not.
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
  rerun). It is even on buildtools. Since cold-eval (note 22), fully
  cold: abseil 10.5–12.6 s against 13.1–13.8, pip_parse 29.6–30.5 against
  28.0–33.4, OpenROAD 110–111 against 70–76 (67 s with the download
  cache pre-filled; P7).
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
- **P6: OpenROAD `build //:openroad` was 2.2× genuine** (mostly fixed;
  openroad-perf, footnote 20): 1,490.9 s → 773.9 s against genuine's
  693.5 s (676–702 s before) for the same 13,479/13,480 actions and 7,642
  sandboxed spawns; critical path 80.1 → 61.0 s against 56.6 s. The
  profile (`PERF-STAGE parts`) showed the time in the kernel (sys 11,167 s
  of 20,428 s CPU): staging (7,563 s summed over the jobs: a tree link,
  two inspections and a directory check per input, 5,600 inputs per
  compile) and removing execution roots (2,726 s). On the overlay
  filesystem a link or unlink costs ~70 µs with 16 jobs and absolute-path
  lookups of shared directories serialize. Now execution roots are reused
  like genuine's `--reuse_sandbox_directories` (95% of the input files
  kept) and staging inspects through open directories
  (docs/HARDLINK_STAGING.md, "Execution-root reuse"); sys 2,333 s. What
  remains is reconciliation (400 s summed, a `readlinkat` per kept link,
  as genuine's) and the checks genuine does not make (the inputs' and the
  staged inodes' states before and after the process, ~670 s).
- **OpenROAD memory** (openroad-perf, footnote 20; docs/MEMORY.md
  "OpenROAD"): cold `--nobuild //...` peak 9.2 → 7.0 GB (genuine 3.0 GB),
  a snapshot miss 9.1 → 6.9 GB in 56–57 → 30–34 s, a hit 3.3 → 1.7 GB in
  1.6 → 1.2–1.3 s. Each compile listed its compilation context's
  transitive headers (54 M input pairs); it now holds them as one shared
  input group. The rest of the peak is the analysis results and deltas
  waiting for pass 2's cursor. genuine's "150,522 targets configured"
  against bazel-rs's 65,387 is the counting difference of
  docs/BENCHMARKS.md (genuine counts every configured target value, file
  targets included; bazel-rs the rule targets it plans), not missing work.
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
- **P4a (largely fixed on cold-eval, below): pool loads that need a
  direct or extension-generated repository wait for the sequential
  pass.** Such a load parked (`INLINE`); its repository speculation
  started, but every configured target behind it waited until the
  sequential pass reached it in the requested targets' order, so later
  repositories were discovered only then (`//:_openroad.so`, the first
  requested target, took 105 s; `openssl+` was first demanded at +119 s,
  `capnp-cpp+` at +170 s, each a lone 4–7 s root).
- **P4b (fixed on cold-eval, below): module extensions never run ahead on
  a cold run.** An extension speculation was eligible only with a
  reusable lockfile entry (docs/COLD_ANALYSIS.md), so every extension
  evaluated on the sequential pass, one after another: pip_parse's
  `crate` extension waited for rules_rust's `stable_tools` (15–17 s).
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

### Findings and fixes (branch cold-eval)

Non-LTO release builds of main (97611a75) and the branch, alternating,
three rounds, the harness's flags and isolation (note 22); fully cold with
genuine 9.3.1 `--batch` in the same rounds, and with the download cache
pre-filled (no network noise; genuine not run so). The load average is
the 1-minute value when each command started.

| | load | genuine | main | cold-eval |
|---|---|---|---|---|
| OpenROAD cold `--nobuild`, download cache pre-filled | 6–17 | | 87.0 / 87.0 / 86.3 s | 67.1 / 67.3 / 67.4 s |
| rules_python-pip_parse, the same | 8–16 | | 26.5 / 26.0 / 26.3 s | 21.7 / 22.6 / 22.0 s |
| abseil-cpp, the same | 8–13 | | 9.7 / 9.8 / 9.9 s | 8.4 / 9.1 / 9.0 s |
| OpenROAD cold `--nobuild`, fully cold | 7–19 | 76.3 / 72.0 / 69.9 s | 173.2 / 139.4 / 146.2 s | 198.8 (mirror timeout) / 110.7 / 109.9 s |
| rules_python-pip_parse, fully cold | 10–16 | 33.4 / 31.8 / 28.0 s | 36.2 / 34.3 / 35.9 s | 30.4 / 30.5 / 29.6 s |
| abseil-cpp, fully cold | 10–17 | 13.5 / 13.1 / 13.8 s | 12.8 / 12.6 / 13.2 s | 12.6 / 10.5 / 10.9 s |

An earlier build of the branch (before its last two fixes) at load 13–39
measured the same picture: pre-filled OpenROAD 68.4–69.0 s against main's
86.9–95.7, fully cold 112.0–124.6 against 164.2–190.7 (genuine 69.9–75.0,
once 192.9).

OpenROAD's phase 1 (plan start to the evaluator's end, pre-filled cache)
went from 51.7 s to 32.1 s; the "87–96 s" of openroad-perf (note 20)
were runs with the download cache pre-filled too, the fully cold runs take
139–191 s on main.

- **Extensions evaluated ahead (P4b)** (docs/COLD_ANALYSIS.md "Module
  extensions evaluated ahead"). A group the evaluator's pool waits for —
  a parked load's, the registered toolchains', what a speculation is
  blocked on — is evaluated on the speculation workers with no lockfile
  entry to reuse, its output, observations, roots, definitions and taint
  held back, and committed where the sequential engine prepares the group
  after that engine's own lockfile check: the same lockfile state, still
  nothing reusable, and the evaluation's entry current (its digests and
  every recorded input unchanged, the condition for the next command's
  reuse); then the entry is recorded and re-probed there. pip_parse:
  `crate` and `rust_host_tools` are evaluated ahead (the chain was
  stable_tools → crate → rust_host_tools); OpenROAD: 34 groups committed
  from evaluations ahead (llvm, pycross, kepler-formal's deps, rules_java's
  toolchains, …); abseil-cpp: `go_sdk`.
- **Repositories ahead (P4a)** (docs/EVALUATOR.md "cold-eval"). A pool
  load waits for a repository's speculation instead of the sequential pass
  (the published root serves its reuse-only probe, counting where the tape
  replays the load); what a speculation is blocked on — and every
  repository its rule's label attributes name — is requested too; the
  registered toolchains' repositories (also `/...` patterns') are prepared
  ahead; the lane holds a demand whose speculation or module-root warm-up
  is still running instead of waiting for it on the sequential pass's
  thread.
- **A latent ordering bug fixed.** The lane prepared a speculatively used
  extension group through a statistics lookup (`wait_eval_local`'s
  `speculation_slot`) when it ran, not where the walk prepares the group;
  an input a local rule rewrote in between was missed. Found by the new
  cold-run test; reachable before with `BAZEL_RS_SNAPSHOT=off`.
- **What remains (P7, fully cold OpenROAD 110–125 s against genuine's
  70–76 s).** With the cache pre-filled the run is 67–69 s; the rest is
  the network on the discovery chain: module resolution (12.2 s cold),
  the target pattern phase's `pip` extension (10 s on the coordinator,
  for `//docs`), `llvm-project` (7.4 s download, then 8.1 s of extraction
  on the sequential pass — every analysis waits for the registered
  toolchains), the `pycross` toolchains extension (8 s on the sequential
  pass: its `rules_pycross_internal` chain exhausts the speculation's
  four attempts), then CPython 3.13 (12.4 s transfer while other
  downloads share the bandwidth), `boost_1_89_0.tar.gz` (14.6 s) and
  `m4` (8.9 s), each discovered only once the analysis reaches it.
  Directions: demanded transfers ahead of speculative ones on the
  critical path, more speculation attempts for chains whose blockers are
  all requested, the pattern phase's extensions ahead.

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
