# Open-source corpus: bazel-rs against genuine Bazel 9.3.1

This corpus runs bazel-rs and genuine Bazel 9.3.1 side by side on public bzlmod
projects. It has two goals:

- find bazel-rs correctness bugs on code we did not write, and
- compare performance on the same commands.

The harness is `scripts/oss_corpus.py`. The projects, their pinned commits and
their workaround edits are in `scripts/oss_corpus_repos.json`. Unit tests for
the harness's pure parts are in `tests/test_oss_corpus.py`.

First run: 2026-11-16/17, bazel-rs 0.4.23 (the installed main binary, sha256
`69ce3bf6…`), genuine `bazel-9.3.1-linux-x86_64`. Latest full run (the
results below): 2026-11-20, bazel-rs 0.4.34 (main d11712c8, LTO release).

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
  average was 15–60 during the first runs and 6–95 (mostly 10–35) during
  the corpus-rerun run, so timings are indicative only.

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

Workaround edits are recorded per project in the JSON (`delete`, `replace`,
and the harness's `comment_out` and `vendor_archives`). Every edit is
applied to both tools, so the comparison stays like for like. Since the
corpus-rerun run no edit works around a bazel-rs bug: the B2 `comment_out`
lines (16 projects) and OpenROAD's vendored `kepler-formal` archive (B9,
B22) were dropped, because bazel-rs accepts every rc option of the corpus
and supports `archive_override`. The two edits left are for genuine:

- **rules-template.** Its v26 `MODULE.bazel.lock` is rejected by genuine
  9.3.1 too. It is deleted and the project runs with
  `--lockfile_mode=update`.
- **rules_go-basic_gazelle.** rules_go, gazelle and Go are bumped to 0.63.0,
  0.51.3 and 1.27.1, as in docs/BENCHMARKS.md. The pinned rules_go 0.52
  fails on Bazel 9 because `CcInfo` was removed.

## Results

Full rerun on 2026-11-20 (branch corpus-rerun): main d11712c8 = bazel-rs
0.4.34, LTO release build (sha256 `6528369c…`), against genuine
`bazel-9.3.1-linux-x86_64`. Every project ran every phase of its JSON entry
(OpenROAD: `nobuild_cold,nobuild_warm,server,query`, then `build //:openroad`
by hand on the same state) with no workaround edits for bazel-rs bugs (the B2
`comment_out` lines and OpenROAD's B9/B22 vendored archive are dropped:
`archive_override` and every rc option now work). One project ran at a
time, after the 1-minute load average fell below 20 (or 30 minutes passed);
the "load" column of the performance table is the 1-minute load sampled
every 20 s during the project (64 CPUs, shared with other workers' runs).
The table and notes below are from this run only; earlier runs' tables
and notes are in this file's git history (before this commit).

Times are wall seconds: genuine (g) against bazel-rs (rs); † marks a
non-zero exit. The "warm nobuild" column gives genuine `--batch` / warm
genuine server / bazel-rs. Peak RSS is the tool process during the cold run.
The outputs column counts `bazel-bin` files: common, only genuine, only
bazel-rs, and hash differences in the sample (C1's params files are not
counted). `query //...` labels match on all 37 projects.

| project | verdict | cold nobuild g / rs (s) | warm nobuild g-batch / g-server / rs (s) | build g / rs (s) | peak RSS cold g / rs (MB) | outputs (common, only g, only rs, hash diff) | tests g / rs | open differences |
|---|---|---|---|---|---|---|---|---|
| abseil-cpp | **OK** | 17.2 / 13.2 | 3.5 / 0.2 / 0.2 | 76.3 / 66.0 | 1191 / 540 | 2767, 0, 0, 0 | PASSED:252 / same | |
| diff.bzl | **OK** | 20.7 / 15.4 | 3.4 / 0.2 / 0.2 | 13.7 / 11.4 | 1178 / 367 | 308, 0, 0, 0 | PASSED:6 / same | |
| examples-bzlmod-01 | **OK** | 11.9 / 6.6 | 3.1 / 0.1 / 0.1 | 6.1 / 2.9 | 600 / 257 | 187, 0, 0, 0 | — | |
| examples-bzlmod-05 | **OK** | 8.6 / 3.7 | 2.5 / 0.1 / 0.1 | 2.5 / 0.1 | 387 / 82 | 1, 0, 0, 0 | — | |
| examples-cpp-tutorial | **OK** | 11.5 / 6.4 | 2.7 / 0.1 / 0.2 | 3.0 / 0.6 | 597 / 235 | 13, 0, 0, 0 | — | |
| examples-rules | **OK** | 13.7 / 8.5 | 3.0 / 0.2 / 0.1 | 4.9 / 1.1 | 637 / 286 | 103, 0, 0, 0 | PASSED:1 / same | |
| examples-rust-ffi | **OK** | 56.1 / 22.6 | 2.8 / 0.2 / 0.2 | 4.2 / 1.7 | 851 / 398 | 57, 0, 0, 0 | — | |
| googletest | **OK** | 18.9 / 12.5 | 3.4 / 0.2 / 0.2 | 29.4 / 29.8 | 1181 / 436 | 529, 0, 0, 0 | PASSED:44 / same | |
| highway | **OK**¹ | 12.4 / 7.3 | 3.1 / 0.2 / 0.2 | 178.0 / 172.6 | 582 / 268 | 1053, 0, 0, 0 | PASSED:138 / same | |
| limdor-cpp | **OK** | 12.0 / 6.6 | 2.8 / 0.2 / 0.1 | 3.1 / 0.6 | 635 / 251 | 18, 0, 0, 0 | PASSED:1 / same | |
| limdor-rust | **OK** | 28.5 / 26.0 | 3.0 / 0.1 / 0.2 | 4.5 / 2.1 | 847 / 485 | 91, 0, 0, 0 | PASSED:1 / same | |
| openroad | **OK**² | 90.7 / 120.1 | 8.7 / 0.7 / 1.4 | `//:openroad` 714.0 / 994.6 | 3749 / 6868 | `//:openroad` 19201, 0, 0, 0 (all compared) | — | |
| rules-template | **OK** | 27.2 / 22.3 | 4.2 / 0.2 / 0.2 | 27.1 / 25.0 | 1311 / 445 | 446, 0, 0, 0 | PASSED:2 / same | |
| rules_go-basic_gazelle | **OK** | 25.7 / 22.3 | 4.0 / 0.3 / 0.2 | 31.3 / 28.9 | 1244 / 488 | 117, 0, 0, 0 | PASSED:1 / same | |
| rules_python-pip_parse | **OK** | 31.2 / 32.5 | 3.6 / 0.3 / 0.2 | 4.4 / 1.1 | 1635 / 800 | 56, 0, 0, 0 | PASSED:2 / same | |
| tink-cc-examples | **OK** | 18.0 / 12.1 | 3.6 / 0.3 / 0.2 | 95.1 / 87.3 | 1219 / 601 | 3499, 0, 0, 0 | PASSED:13 / same | |
| bazel-gazelle | BOTH-FAIL³ | 41.0 / 36.8 | 4.6 / 0.3 / 0.2 | 57.0 / 52.0 | 1765 / 940 | 2551, 0, 0, 0 | FAILED:2 PASSED:194 / same | |
| bazel-skylib | BOTH-FAIL³ | 21.7 / 20.0 | 3.7 / 0.3 / 0.2 | 50.7 / 50.2 | 1289 / 566 | 689, 0, 0, 0 | FAILED:4 PASSED:158 / same | |
| buildtools | BOTH-FAIL³ | 24.5 / 24.1 | 4.5 / 0.3 / 0.2 | 49.6 / 48.7 | 1951 / 1300 | 566, 0, 0, 0 | FAILED:2 PASSED:23 / same | |
| examples-configurations | BOTH-FAIL⁴ | 9.8† / 5.6† | 2.2† / 0.1† / 0.2† | 1.8† / 0.2† | 472 / 104 | 0, 0, 0, 0 | — | |
| re2 | BOTH-FAIL⁵ | 11.7† / 8.1† | 3.2† / 0.2† / 1.2† | 16.4† / 15.9† | 1057 / 316 | 1085, 0, 0, 0 | PASSED:21 / same | |
| riegeli | BOTH-FAIL⁶ | 28.5† / 20.0† | 4.4† / 0.3† / 3.2† | 70.9† / 68.8† | 1373 / 805 | 2800, 0, 0, 0 | PASSED:2 / same | |
| examples-java-maven | DIFF | 34.8 / 56.0 | 3.3 / 0.2 / 0.2 | 8.9 / 7.2 | 1216 / 457 | 133, 2, 0, 0 | PASSED:2 / same | C2 |
| examples-java-tutorial | DIFF⁷ | 18.8† / 19.4† | 3.3† / 0.1† / 0.8† | 6.4† / 4.2† | 745 / 306 | 11, 2, 0, 0 | — | C2 |
| junit5-starter | DIFF | 22.2 / 15.0 | 3.6 / 0.2 / 0.2 | 7.8 / 5.4 | 1298 / 515 | 100, 28, 0, 0 | PASSED:1 / same | C2 |
| yalantinglibs | DIFF⁸ | 12.0 / 6.8 | 3.4 / 0.2 / 0.2 | 58.6† / 52.2† | 679 / 244 | 126, 6, 0, 0 | FAILED TO BUILD:1 PASSED:2 SKIPPED:2 / same | C5 |
| examples-frontend | DIFF⁹ | 88.3† / 121.6† | 60.9† / 110.9† / 97.7† | 60.8† / 137.7† | 1488 / 1806 | 215801, 12, 2, 0 | PASSED:14 / same | C3 |
| examples-rust-deps-cargo | DIFF¹⁰ | 39.6 / 28.8 | 3.4 / 0.2 / 0.2 | 27.7 / 24.7 | 1016 / 371 | 103, 0, 0, 1 | — | C4 |
| rules_python-bzlmod | DIFF | 43.2 / 36.2 | 4.6 / 0.6 / 0.2 | 43.7 / 39.7 | 1959 / 2577 | 319, 0, 0, 1 | PASSED:31 / same | B34 |
| with_cfg-examples | DIFF | 33.2 / 28.2 | 4.0 / 0.4 / 0.1 | 8.9 / 5.9 | 1737 / 1678 | 207, 37, 0, 0 | PASSED:20 / same | C2, B43 |
| capnproto | DIFF¹¹ | 17.1 / 11.1 | 3.5 / 0.2 / 0.2 | 23.9 / 18.1 | 1079 / 385 | 1270, 132, 0, 0 | FAILED TO BUILD:5 SKIPPED:14 / same | C5 |
| flatbuffers | DIFF¹² | 17.1† / 7.4† | 3.0† / 0.3† / 0.5† | 3.1† / 0.4† | 763 / 207 | 0, 0, 0, 0 | — | B12 |
| rules_jvm_external-simple | DIFF¹² | 72.9† / 38.1† | 4.2† / 0.4† / 1.8† | 4.0† / 1.5† | 1625 / 655 | 0, 0, 0, 0 | — | B47 |
| rules_cc | DIFF | 18.8† / 20.5† | 5.7† / 0.6† / 5.7† | 51.3† / 56.0† | 2655 / 1696 | 1669, 315, 0, 1 | FAILED:1 PASSED:453 SKIPPED:2 / FAILED:1 FAILED TO BUILD:2 PASSED:346 SKIPPED:2 | B32 |
| protobuf | DIFF¹³ | 43.7† / 73.3† | 6.2† / 0.6† / 14.8† | 290.5† / 302.1† | 2970 / 3625 | 19840, 21247, 0, 0 | — | B33, C2, C5 |
| rules_kotlin | DIFF¹⁴ | 38.0 / 42.0 | 5.0 / 0.6 / 0.3 | 59.2 / 59.7 | 2113 / 2082 | 2235, 1116, 0, 1 | genuine 772 s, FAILED:2 PASSED:266 / 20-minute cap at 224 of 268 | C2, B53 |
| rules_zig | RS-FAIL | 38.0 / 37.5† | 14.1 / 0.3 / 12.0† | 38.0 / 12.2† | 2517 / 1933 | 0, 1292, 0, 0 | PASSED:211 SKIPPED:4 / SKIPPED:4 | B13c |

Notes:

1. The first highway run's `build` coincided with a machine-wide load
   spike (1-minute load up to 1,973 from other work; bazel-rs 382 s against
   genuine's 206 s before it); `nobuild_cold,build` were rerun on their own
   at load 15–33 (the row). Its `test` (both 739–749 s) ran at load 9–25.
2. Phases `nobuild_cold,nobuild_warm,server,query` (`query //...` matches,
   7,079 labels; `build --nobuild //...` exits 0 on both, with the
   project's own `archive_override(kepler-formal)` and its seven patches:
   B9 is fixed), then `build --keep_going //:openroad` by hand on the same
   state, `--jobs=16`: genuine 714.0 s (load 12 → 38), bazel-rs 994.6 s
   (load 20 → 52, other workers' runs), the same 13,480/13,479 actions and
   7,642 sandboxed spawns, critical path 59.7 s against 63.5 s, peak RSS
   2.9 GB against 3.0 GB. All 19,201 `bazel-out/k8-opt/bin` files match byte
   for byte (none only on one side, C1's params files aside).
3. The project's own failing tests (and the `query //...` exit 3 of its
   `--keep_going` loading errors) are identical on both tools.
4. Both tools abort on genuine's `error loading project files: Every .bzl
   file must have a corresponding package` (B24, fixed).
5. Both fail the project's `//app` package; every test status, label and
   output matches (B1 is fixed).
6. Both fail the same way in loading (`sh_binary` undefined in `//python`,
   `@local_config_tf` not visible); the outputs and tests match.
7. Both fail loading on `//:greeter`; only C2 differs.
8. The compile errors (gcc 13) are identical; genuine keeps the failed
   compiles' `.d` files and the failed test's runfiles manifest (C5).
9. `cosmo.zip` (the `ape` toolchain) times out from this host on both
   tools, so every command fails and the timings include connection
   timeouts. Only C3 and next.js's random build id (2 files each side)
   differ.
10. corpus-parity6 (B49 fixed): the file lists match; the hash difference
    is `_bs.out_dir/anyhow.d`, whose dep-info lines hold each tool's
    absolute sandbox path (C4).
11. corpus-parity6 (B50, B52 fixed): the compile errors (gcc 13), "Build
    succeeded for only 40 of 171 top-level targets", the 1,177 processes
    (697 internal) and the test statuses match; the 132 only-genuine files
    are the failed compiles' `.d` files (C5). The `test` row is from a
    non-LTO build of the final commit (the B52 fix came after the release
    binary of the other phases).
12. Genuine fails too (flatbuffers: the old rules_go's `CcInfo`;
    rules_jvm_external-simple: no Android SDK); bazel-rs adds B12 / B47.
13. `//ruby/...` and `//csharp/...` excluded (genuine needs a system
    `ruby`), no `test` phase. `query` matches (3,411 labels). Both fail
    the same compiles (gcc 13) and the rules_ruby fetch; bazel-rs also fails
    analysis of `//bazel/tests:test_compile_path_mapping` (B33). Of the
    21,247 only-genuine files, 21,243 are C2 `_javac` class files and 3 are
    B33's target. corpus-parity6 (B48, B49 fixed): 13,940 processes against
    13,945 (the 5 are B33's target), the build 302 s against 290 s (was
    648 s). Its warm run re-analyses (14.8 s): a command whose analysis
    failed saves no snapshot. The cold and warm rows ran at a load of up to
    250 from other work.
14. corpus-parity6 (B51 fixed): `build //...` succeeds on both; the
    only-genuine files are C2's `_javac` classes, the hash difference a
    KSP-generated srcjar. `test //...` runs nested Bazel integration tests
    (`//examples:*_test`): genuine finishes in 772 s (2 of them fail);
    bazel-rs hits the 20-minute cap with 224 of 268 tests done (B53), so
    the test statuses are not compared.

**Summary.** 37 projects ran, all with no workaround edits for bazel-rs
bugs:

- **16 identical** (same exit codes, `query //...` labels, `bazel-bin`
  file lists, sampled SHA-256s and test results): abseil-cpp, diff.bzl,
  examples-bzlmod-01, examples-bzlmod-05, examples-cpp-tutorial,
  examples-rules, examples-rust-ffi, googletest, highway, limdor-cpp,
  limdor-rust, openroad (nobuild, query and the full `//:openroad` build:
  19,201 identical files), rules-template, rules_go-basic_gazelle,
  rules_python-pip_parse, tink-cc-examples.
- **6 fail identically** (the project's own failures): bazel-gazelle,
  bazel-skylib, buildtools, examples-configurations, re2, riegeli.
- **14 differ**: 6 only cosmetically (examples-java-maven,
  examples-java-tutorial, junit5-starter: C2; yalantinglibs, capnproto:
  C5; examples-rust-deps-cargo: C4), and examples-frontend (C3),
  rules_python-bzlmod (B34), with_cfg-examples (C2, B43), flatbuffers (B12)
  and rules_jvm_external-simple (B47) where genuine fails too, rules_cc
  (B32: 346 of genuine's 453 tests pass), protobuf (B33), rules_kotlin
  (builds; its `test //...` does not finish in 20 minutes, B53).
- **1 blocked**: rules_zig (B13c ends analysis).

corpus-parity6 (2026-11-21, its LTO release binary) reran protobuf,
examples-rust-deps-cargo, rules_kotlin and capnproto (their rows and notes
10, 11, 13 and 14); the other rows are the corpus-rerun run's.

`query //...` matches on all 37. Every common output sampled is
byte-identical except B34's zipapp, C4's dep-info file and rules_cc's one
file. Fixed since the previous runs, by this run's evidence: B1, B2 (every
rc option of the corpus; all `comment_out` edits dropped), B3, B9 (no
vendored archive for OpenROAD), B10, B11, B14, B20, B21, B23, B24, B25.

## Bugs (ranked by the number of projects they block)

Every bug below is a case where genuine 9.3.1 succeeds and bazel-rs fails or
differs. The logs are in `<work>/logs/<project>/rs/<phase>.err`. The status
column is from the 2026-11-20 full rerun (corpus-rerun): open bugs first,
ranked by the projects they affect in that run, then the fixed ones.

| id | status (corpus-rerun) | summary |
|---|---|---|
| B53 | open, 1 (rules_kotlin); `test //...` | rules_kotlin's `test //...` (nested Bazel integration tests `//examples:*_test`) hits the 20-minute cap on bazel-rs with 224 of 268 tests done; genuine finishes in 772 s (`//examples:android_8.x_test` 670 s, `android_.bazelversion_test` 122 s; on bazel-rs the latter ran over 840 s). Not investigated (the nested builds' sandboxing, scheduling or parallelism) |
| B13c | open, 1 (rules_zig); blocking | one toolchain implementation for two execution platforms in one configuration (ConfiguredTargetKey.executionPlatformLabel), a fatal error (needs the execution platform in the configured-target key; design below) |
| B32 | open, 1 (rules_cc's own tests: 346 of genuine's 453 pass) | rules_cc's own analysis tests. Fixed on corpus-parity4: `--fission`, `--build_test_dwp`, `--legacy_whole_archive`, `--incompatible_remove_legacy_whole_archive` are configuration options and `--dynamic_mode` is transitionable; a fission compile's `.dwo` is declared where genuine declares it. Fixed on corpus-parity5: `cc_internal.freeze` is Bazel's shallow immutable copy (the vendored starlark's immutable list/dict that is not frozen yet, docs/DEPSETS.md; the 38 thin-LTO and linkstamp targets), `create_header_info_with_deps` adds the merged deps' headers (cc_import's `direct_public_headers`), a toolchain's `artifact_name_pattern` applies (mingw/msvc dll names), `objc.target_should_alwayslink` of an explicit `alwayslink`, map_each's `must be declared by a top-level def statement` error, structs/providers with frozen list fields are hashable, and transitions on `instrumentation_filter`, `save_temps`, `start_end_lib`, `interface_shared_objects`, `process_headers_in_dependencies` (tests/fixtures/corpus_parity5). Left (tests): the LTO backends (`cc_internal.collect_per_file_lto_backend_opts`, `create_lto_backend_action*`, compile actions with shareable artifacts: ~37, also the linkstamp compile), the FDO/Propeller/memprof options (`fdo_optimize`, `fdo_profile`, `fdo_instrument`, `cs_fdo_profile`, `xbinary_fdo`, `fdo_prefetch_hints`, `propeller_optimize*`: ~37), `experimental_cpp_modules` (10), `custom_malloc` (5, a late-bound dependency per configuration), `cc_internal.expand_and_tokenize` (3 objc tests), `action_env` and `grte_top` transitions (2 each), `ltoindexopt`/`ltobackendopt`/`per_file_ltobackendopt`, and two builds: cc_static_library's `test_output_groups` builds a compile of a missing `file.cc` (the analysis test's target's validation outputs; genuine does not) and `validate_static_library_env_test`'s archive is not created |
| B33 | open, 1 (protobuf); 1 target | protobuf analysis. Fixed on corpus-parity4: symbolic-macro visibility delegation and alias select() keys under `--incompatible_config_setting_private_default_visibility` (5 visibility errors), `declare_file(sibling=)` from another package, per-attribute aspect views of aspects propagating over one attribute (`UpbMinitableCcInfo`/`JavaInfo` on proto_library targets), `Target.actions` of targets merged with aspects and of a rule under test's dependencies, `--dynamic_mode` transitions (rules_fuzzing). Fixed on corpus-parity5: transitions writing `instrumentation_filter` (`generated_sources_not_coverage_instrumented`; the filter is per configuration natively too). Left: `experimental_output_paths=strip` with `modify_execution_info` (`test_compile_path_mapping`): its `Target.actions` argv shows path-mapped `bazel-out/cfg/bin` paths, so path mapping would have to be modelled for execution too (not attempted) |
| B34 | open, 1 (rules_python-bzlmod); not blocking | `external/other_module+/other_module/pkg/bin_zipapp.zip` differs from genuine's: rules_python's zipper stores each input's mode and relative symlinks as it finds them in the sandbox; native staging hardlinks read-only store blobs (docs/HARDLINK_STAGING.md) |
| B43 | open, 1 (with_cfg); not blocking | genuine builds `cc_no_copt_binary/binary_/binary.runfiles_manifest` and `.repo_mapping` (the manual `cc_binary` behind with_cfg's frontend and transitioning alias, a no-op transition); bazel-rs does not (not investigated) |
| B12 | open, 1 (flatbuffers; genuine fails too) | `@local_config_platform` unknown (aspect_bazel_lib 1.x `platform_utils.bzl:3`) |
| B47 | open, 1 (rules_jvm_external-simple; genuine fails too, no Android SDK) | rules_android's `android_split_transition` reads `//command_line_option:Android configuration distinguisher`: "transition inputs ... do not correspond to valid settings" (an unmodelled Android native option) |
| B52 | fixed (corpus-parity6) | the test summary listed `FAILED TO BUILD` targets in string order (`//a/b:c` before `//a:z`); genuine orders them as Label.compareTo, package before name (capnproto's first five lines) |
| B48 | fixed (corpus-parity6) | protobuf built protoc in 8 extra exec configurations and the prebuilt-protoc validation in 17 extra target configurations: an analysis test (`testing.analysis_test`, rules_testing) propagated its target under test's `_validation` group; RuleConfiguredTargetBuilder.propagateValidationActionOutputGroup propagates none for analysis tests. protobuf: 13,940 processes against 13,945 (B33's target), 302 s against 290 s |
| B49 | fixed (corpus-parity6) | rules_rust's `cargo_build_script` `_bs-` runfiles manifest: a tool's runfiles manifest and tree are built with the action that stages the tool's runfiles (Bazel's spawn holds the tool's runfiles tree artifact, whose RunfilesTreeAction takes the MANIFEST); never staged in the sandbox (tests/fixtures/corpus_parity6) |
| B50 | fixed (corpus-parity6) | capnproto `build --keep_going`: a runfiles manifest or tree still runs for a binary whose link failed (SourceManifestAction's inputs are the unresolved symlinks only, SymlinkTreeAction's the input manifest); the tree tallies as SymlinkTreeAction alone then. 1,177 processes on both |
| B51 | fixed (corpus-parity6) | rules_kotlin's multiplex JdepsMerge reads the `-kt.jar` beside the jdeps it merges, not an input: a non-sandboxed worker runs in genuine's shared execution root, so its native working directory also holds the other outputs of its inputs' producing actions (docs/PERSISTENT_WORKERS.md) |
| B1 | fixed (googletest, re2, highway identical) | `config_setting(values = {"features": …})` |
| B2 | fixed (every rc option of the corpus; all `comment_out` edits dropped) | rc options rejected at startup: "options are never silently ignored" |
| B3 | fixed (bazel-skylib, junit5-starter, with_cfg analyse) | `native.existing_rule` / `existing_rules` missing in BUILD and legacy-macro context |
| B9 | fixed (openroad's own `archive_override`, rules_cc, protobuf, examples-rust-deps-cargo) | `archive_override` unsupported |
| B10 | fixed (rules_kotlin, riegeli, openroad) | `override_repo()` / `inject_repo()` unsupported in MODULE.bazel |
| B11 | fixed (tink-cc-examples identical, capnproto) | `strip_prefix` with a trailing `/` rejected |
| B14 | fixed (tink-cc-examples, examples-java-maven) | internal errors "has no recorded duplicate-label check" / "internal analysis order error" |
| B20 | fixed (capnproto analyses) | `rule(output_to_genfiles=True)` |
| B21 | fixed (rules_zig's repository rules) | `attr.int_list` in repository rules |
| B22 | not exercised (OpenROAD's `archive_override` works, so the `git_override` alternative is not needed) | `git_override` of a repository with submodule gitlinks |
| B23 | fixed (examples-rules identical) | predeclared `attr.output` given as `":name"` |
| B24 | fixed (corpus-parity4; examples-configurations aborts as genuine) | PROJECT.scl project files: looked up and loaded before the analysis, genuine's `error loading project files` (docs/CONFIGURATION.md "PROJECT.scl project files"); a project with configurations fails explicitly |
| B25 | fixed (examples-frontend's warm `test` passes 14/14) | "needs-repository: repository @@rules_diff+ is unfetched and this thread does not fetch" on a warm `test` |
| B6 | fixed (corpus-parent) | `_repo_mapping` manifest missed the repositories of select() keys and of an output file's generating rule |
| B4 | fixed (corpus-parity2) | `android_common` global undefined: now the `.bzl` toplevel; its two methods fail explicitly |
| B13 | fixed (bazel-skylib, rules_zig, rules_cc, bazel-gazelle) | analysis tests: `analysis_test_transition`, `testing.analysis_test`, `Target.actions`, `AnalysisFailureInfo` (docs/ANALYSIS_TESTS.md) |
| B13a | fixed (corpus-parity2) | transition on `//command_line_option:minimum_os_version` and `ctx.fragments.cpp.minimum_os_version()` |
| B13b | fixed (corpus-parity2) | `srcs = ["."]` (the package directory as a source label): "unsafe source label //tests/directory:." |
| B13d | fixed (= B29) | rules_cc's own `cc_test.bzl`/`cc_binary.bzl` "cannot use private API" |
| B13e | fixed (corpus-parity2) | `cc_import` of `mylib.dll`/`mylib.lib` on Linux: "does not match expected filetype" |
| B15 | fixed (corpus-runfiles) | test runfiles were hardlinks, not symlinks |
| B7 | fixed (corpus-parity2) | transition: "Starlark flag and its alias have different values: False and "false"" |
| B5 | fixed (corpus-parity2) | `local_repository` → `rctx.symlink(path, ".")` rejected (now a copy plus a `watch_tree` input, docs/LOCAL_REPOSITORIES.md) |
| B8 | fixed (corpus-parity3) | declared output "crosses a package boundary": a `declare_file`/`declare_directory` output may lie below a subpackage; predeclared outputs keep genuine's `Label ... is invalid because ... is a subpackage` |
| B16 | fixed (corpus-macros) | symbolic macro `inherit_attrs` |
| B17 | fixed (corpus-parent) | `rule(parent=…)` rule extension (docs/RULE_EXTENSION.md) |
| B18, B19 | fixed (corpus-ccshared) | `cc_shared_library` (`cc_common.check_experimental_cc_shared_library`); solib naming in transitioned configurations |
| B26 | fixed | depset elements holding functions that are not top-level defs (skylib `DirectoryInfo` in rules_cc `cc_args`; openroad) |
| B27 | fixed (corpus-runfiles) | `depset` was not a BUILD-file global, and list-typed attributes did not accept depsets (`@openssl//:BUILD.bazel:508`, `select({...: depset([...])})`) |
| B28 | fixed (corpus-runfiles) | `build //...` missed genuine's `.pic.o`/`.pic.d` of external C++ deps: the rule-returned `_hidden_top_level_INTERNAL_` group (rules_cc cc_library's transitive objects) was not built |
| B29 | fixed (corpus-parity2) | "file '//cc/private/rules_impl:cc_test.bzl' cannot use private API": the private-API allowlist now treats the main repository as rules_cc when the root module is rules_cc |
| B30 | fixed (corpus-ccshared) | `provider in target` fails "Target membership requires an exported provider constructor" in with_cfg's `transitioning_alias.bzl:55` |
| B31 | fixed (corpus-parity3) | `config_common.config_feature_flag_transition`: loads; no transition unless the attribute sets flag values, which fails explicitly (docs/TEST_FLAGS.md) |
| B35 | fixed (corpus-parity3) | rules_kotlin: `download_and_extract(rename_files=…)`, `Target.actions` through an alias, TestingAspectInfo (per-attribute aspect views), `check_java_toolchain_is_declared_on_rule`, mutable `default_jvm_opts`, genquery `buildfiles` scope |
| B36 | fixed (corpus-ccshared) | `ctx.actions.write(execution_requirements=…)` rejected (rules_cc_autoconf `ctx_actions_write.bzl`; 360 OpenROAD targets) |
| B37 | fixed (corpus-ccshared) | a repository's BUILD file symlinked into another repository's tree: "build file … resolves outside workspace" (sv-elab's `vendored-hashlib`) |
| B38 | fixed (corpus-ccshared) | `repository_ctx.delete()` of the repository directory itself (bazel_tools `git_repository`'s `git_worker.bzl`; sv-elab's `vendored-slang`) |
| B39 | fixed (corpus-ccshared) | a tool hardlinking a repository file into its output (bazel_lib `copy_to_directory`): publication chmod'ed the shared tree file, then libcxx compiles failed "changed while it was being staged" |
| B40 | fixed (corpus-parity3) | `copy_to_directory` over a source-directory input: sandboxed spawns now see the directory as a symlink, as genuine's sandboxes stage it (docs/SOURCE_DIRECTORIES.md) |
| B41 | fixed (corpus-parity3) | rules_js `node_modules` links in `bazel-bin`: a relative symlink output keeps its text in the output tree (docs/UNRESOLVED_SYMLINKS.md) |
| B42 | fixed (corpus-ccshared) | a test's runfile of an `actions.symlink` output of a generated file was a copy at the link's exec path, so `$ORIGIN` of a with_cfg-forwarded `cc_binary` missed its `dynamic_deps` |
| B44 | fixed (corpus-parity3) | `ctx.actions.write(mnemonic=...)` was dropped: rules_cc's module maps are `CppModuleMap` in aquery, the profile and `Target.actions` |
| B45 | fixed (corpus-parity3) | compiles staged the toolchain's `all_files` instead of `compiler_files` (`as_files` for `assemble`) |
| B46 | fixed (corpus-parity4) | `Target.actions` `argv` is empty for an action whose `Args` use `map_each` |

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

**B13c (corpus-parity5): the shared-action conflict check.** The
prerequisite exists: crate::shared_actions (docs/SHAREABLE_ARTIFACTS.md,
tests/shared_actions.rs against a genuine receipt, 31 cases MATCH) keys
every registered action (mnemonic, command lines, environment, execution
info, inputs and outputs without owner, the execution platform and the exec
properties) and checks each output path across all configured targets
(crate::engine `register_output`), whatever their label or configuration:
equal shareable actions share, anything else is genuine's `file 'X' is
generated by these conflicting actions` (`Label:` line). corpus-parity5
adds the B13c-relevant genuine cases (tests/fixtures/corpus_parity5,
`exec_platform_*`): two targets' equal `run_shell`/`write` actions on one
execution platform share their output, on two execution platforms they
conflict, natively too. For rules_zig the two `zig_toolchain` copies
declare `<name>/<escaped exec platform>.version_validation`, so no output
is shared. What B13c itself still needs (not attempted: it touches the
key of every evaluator, the dependency views and their (label,
configuration) lookups — engine.rs `dependency_labels`/`artifacts`/
`runfiles_info`, the parallel buffer's and prepare's waits, the eval
host's `analyze(label, config)` and `forced_exec`/`toolchain_implementations`
claims, linearize pass 1, cquery and the snapshot codec): the execution
platform in `ConfiguredTargetKey` on toolchain-resolution edges, and,
for unshareable actions (C++ compiles), the owner identity of their
outputs (`ActionKey`'s owned files) must include it, since genuine's
Artifact owner is the full ConfiguredTargetKey: two copies' equal C++
compiles conflict in genuine, while today's native owner (label and
configuration) would share them.

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

**B31–B35** are the next blockers the corpus-parity2 rerun found. B31, B35,
B40, B41, B44 and B45 are fixed on corpus-parity3; B46 and B47 are what that
rerun found next. B48–B51 are what the corpus-rerun full run found (table
above); corpus-parity6 fixed them and B52 and found B53. The "note"/"footnote" numbers cited in this section and under
Performance refer to the earlier runs' results notes (this file's git
history before the corpus-rerun commit).

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
- C4: dep-info (`.d`) files a build script writes hold the absolute
  sandbox path of each tool's execution root
  (`_bs.out_dir/anyhow.d` in examples-rust-deps-cargo); they differ between
  two genuine runs too.
- C5: after a failed action genuine's `bazel-bin` keeps the failed
  compiles' `.d` files (yalantinglibs, capnproto, protobuf); bazel-rs does
  not. (The failing targets' runfiles manifests match since B50.)
- Error message formats differ; for example, Starlark tracebacks use the
  `* file:line` style. The exit codes match.

## Performance

### Performance vs genuine (corpus-rerun, 2026-11-20)

From the full rerun above (bazel-rs 0.4.34, LTO release; one project at a
time; "load" is the 1-minute load average sampled during the project, other
workers' runs included). Wall seconds; † a non-zero exit (both tools fail
the same command unless the results table says otherwise), "(cap)" the
20-minute cap. Each × is genuine ÷ bazel-rs, so above 1 means bazel-rs is
faster or smaller: warm compares bazel-rs's fresh process with genuine's
warm server, peak RSS is the cold `build --nobuild` run's. Cold includes
every download from the public mirrors. OpenROAD's build is
`build //:openroad` (by hand; its peak RSS 2.9 GB genuine, 3.0 GB bazel-rs);
the test column of OpenROAD and protobuf is not run (JSON phases).

| project | load | cold g / rs (s) | × | warm g-batch / g-server / rs (s) | × vs server | build g / rs (s) | × | test g / rs (s) | × | peak RSS cold g / rs (MB) | × |
|---|---|---|---|---|---|---|---|---|---|---|---|
| abseil-cpp | 17–28 | 17.2 / 13.2 | 1.30 | 3.5 / 0.2 / 0.2 | 1.00 | 76.3 / 66.0 | 1.16 | 57.2 / 57.0 | 1.00 | 1191 / 540 | 2.2 |
| bazel-gazelle | 11–42 | 41.0 / 36.8 | 1.11 | 4.6 / 0.3 / 0.2 | 1.50 | 57.0 / 52.0 | 1.09 | 47.0† / 46.1† | 1.02 | 1765 / 940 | 1.9 |
| bazel-skylib | 17–33 | 21.7 / 20.0 | 1.08 | 3.7 / 0.3 / 0.2 | 1.50 | 50.7 / 50.2 | 1.01 | 4.3† / 1.8† | 2.39 | 1289 / 566 | 2.3 |
| buildtools | 11–95 | 24.5 / 24.1 | 1.02 | 4.5 / 0.3 / 0.2 | 1.75 | 49.6 / 48.7 | 1.02 | 7.9† / 4.3† | 1.86 | 1951 / 1300 | 1.5 |
| capnproto | 14–19 | 39.2 / 11.1 | 3.54 | 3.4 / 0.2 / 0.2 | 1.25 | 20.2† / 18.2† | 1.11 | 9.4† / 7.5† | 1.26 | 1078 / 384 | 2.8 |
| diff.bzl | 19–25 | 20.7 / 15.4 | 1.34 | 3.4 / 0.2 / 0.2 | 0.80 | 13.7 / 11.4 | 1.20 | 3.8 / 0.8 | 4.70 | 1178 / 367 | 3.2 |
| examples-bzlmod-01 | 15–19 | 11.9 / 6.6 | 1.81 | 3.1 / 0.1 / 0.1 | 1.00 | 6.1 / 2.9 | 2.12 | 3.0† / 0.5† | 6.02 | 600 / 257 | 2.3 |
| examples-bzlmod-05 | 12–13 | 8.6 / 3.7 | 2.36 | 2.5 / 0.1 / 0.1 | 3.00 | 2.5 / 0.1 | 25.10 | 2.5† / 0.1† | 49.20 | 387 / 82 | 4.7 |
| examples-configurations | 18–23 | 9.8† / 5.6† | 1.76 | 2.2† / 0.1† / 0.2† | 0.50 | 1.8† / 0.2† | 9.05 | 1.9† / 0.3† | 6.37 | 472 / 104 | 4.5 |
| examples-cpp-tutorial | 12–15 | 11.5 / 6.4 | 1.81 | 2.7 / 0.1 / 0.2 | 0.75 | 3.0 / 0.6 | 5.47 | 3.4† / 0.5† | 7.60 | 597 / 235 | 2.5 |
| examples-frontend | 8–43 | 88.3† / 121.6† | 0.73 | 60.9† / 110.9† / 97.7† | 1.14 | 60.8† / 137.7† | 0.44 | 60.9† / 104.4† | 0.58 | 1488 / 1806 | 0.8 |
| examples-java-maven | 18–82 | 34.8 / 56.0 | 0.62 | 3.3 / 0.2 / 0.2 | 1.00 | 8.9 / 7.2 | 1.23 | 7.0 / 2.6 | 2.68 | 1216 / 457 | 2.7 |
| examples-java-tutorial | 12–20 | 18.8† / 19.4† | 0.97 | 3.3† / 0.1† / 0.8† | 0.20 | 6.4† / 4.2† | 1.52 | 3.4† / 0.9† | 3.73 | 745 / 306 | 2.4 |
| examples-rules | 9–14 | 13.7 / 8.5 | 1.62 | 3.0 / 0.2 / 0.1 | 1.33 | 4.9 / 1.1 | 4.39 | 3.6 / 0.9 | 3.76 | 637 / 286 | 2.2 |
| examples-rust-deps-cargo | 17–23 | 42.2 / 30.3 | 1.39 | 3.1 / 0.2 / 0.2 | 1.00 | 27.3 / 24.6 | 1.11 | 3.9† / 0.6† | 7.04 | 1013 / 364 | 2.8 |
| examples-rust-ffi | 13–29 | 56.1 / 22.6 | 2.49 | 2.8 / 0.2 / 0.2 | 1.25 | 4.2 / 1.7 | 2.43 | 3.2† / 1.4† | 2.24 | 851 / 398 | 2.1 |
| flatbuffers | 12–13 | 17.1† / 7.4† | 2.33 | 3.0† / 0.3† / 0.5† | 0.67 | 3.1† / 0.4† | 7.65 | 3.2† / 0.5† | 7.02 | 763 / 207 | 3.7 |
| googletest | 14–17 | 18.9 / 12.5 | 1.51 | 3.4 / 0.2 / 0.2 | 1.25 | 29.4 / 29.8 | 0.99 | 7.0 / 6.0 | 1.18 | 1181 / 436 | 2.7 |
| highway | 15–33 | 12.4 / 7.3 | 1.70 | 3.1 / 0.2 / 0.2 | 1.00 | 178.0 / 172.6 | 1.03 | 739.1 / 749.0 | 0.99 | 582 / 268 | 2.2 |
| junit5-starter | 12–18 | 22.2 / 15.0 | 1.48 | 3.6 / 0.2 / 0.2 | 1.25 | 7.8 / 5.4 | 1.46 | 4.5 / 1.5 | 3.06 | 1298 / 515 | 2.5 |
| limdor-cpp | 12–16 | 12.0 / 6.6 | 1.82 | 2.8 / 0.2 / 0.1 | 1.33 | 3.1 / 0.6 | 5.18 | 3.2 / 0.8 | 4.28 | 635 / 251 | 2.5 |
| limdor-rust | 11–37 | 28.5 / 26.0 | 1.10 | 3.0 / 0.1 / 0.2 | 0.75 | 4.5 / 2.1 | 2.19 | 3.6 / 1.1 | 3.14 | 847 / 485 | 1.7 |
| openroad (`//:openroad`) | 6–21 | 90.7 / 120.1 | 0.76 | 8.7 / 0.7 / 1.4 | 0.46 | 714.0 / 994.6 | 0.72 | — / — | — | 3749 / 6868 | 0.5 |
| protobuf | 16–62 | 41.9† / 62.3† | 0.67 | 7.3† / 0.6† / 15.2† | 0.04 | 286.9† / 648.0† | 0.44 | — / — | — | 2718 / 3613 | 0.8 |
| re2 | 10–30 | 11.7† / 8.1† | 1.44 | 3.2† / 0.2† / 1.2† | 0.21 | 16.4† / 15.9† | 1.03 | 327.9† / 331.8† | 0.99 | 1057 / 316 | 3.3 |
| riegeli | 12–25 | 28.5† / 20.0† | 1.43 | 4.4† / 0.3† / 3.2† | 0.09 | 70.9† / 68.8† | 1.03 | 12.8† / 10.6† | 1.21 | 1373 / 805 | 1.7 |
| rules-template | 12–20 | 27.2 / 22.3 | 1.22 | 4.2 / 0.2 / 0.2 | 1.25 | 27.1 / 25.0 | 1.08 | 4.3 / 0.8 | 5.02 | 1311 / 445 | 2.9 |
| rules_cc | 16–39 | 18.8† / 20.5† | 0.92 | 5.7† / 0.6† / 5.7† | 0.10 | 51.3† / 56.0† | 0.92 | 6.3† / 7.2† | 0.88 | 2655 / 1696 | 1.6 |
| rules_go-basic_gazelle | 13–23 | 25.7 / 22.3 | 1.15 | 4.0 / 0.3 / 0.2 | 1.75 | 31.3 / 28.9 | 1.09 | 4.2 / 0.9 | 4.69 | 1244 / 488 | 2.5 |
| rules_jvm_external-simple | 19–35 | 72.9† / 38.1† | 1.91 | 4.2† / 0.4† / 1.8† | 0.23 | 4.0† / 1.5† | 2.72 | 4.3† / 1.6† | 2.77 | 1625 / 655 | 2.5 |
| rules_kotlin | 15–43 | 37.6 / 45.1 | 0.83 | 5.4 / 0.6 / 0.2 | 2.40 | 66.9 / 79.0† | 0.85 | 1209.0 (cap) / 1200.0 (cap) | — | 2320 / 2073 | 1.1 |
| rules_python-bzlmod | 10–36 | 43.2 / 36.2 | 1.19 | 4.6 / 0.6 / 0.2 | 2.20 | 43.7 / 39.7 | 1.10 | 7.5 / 10.3 | 0.73 | 1959 / 2577 | 0.8 |
| rules_python-pip_parse | 12–17 | 31.2 / 32.5 | 0.96 | 3.6 / 0.3 / 0.2 | 1.40 | 4.4 / 1.1 | 4.22 | 4.8 / 2.0 | 2.38 | 1635 / 800 | 2.0 |
| rules_zig | 17–32 | 38.0 / 37.5† | 1.01 | 14.1 / 0.3 / 12.0† | 0.03 | 38.0 / 12.2† | 3.10 | 14.4 / 12.1† | 1.19 | 2517 / 1933 | 1.3 |
| tink-cc-examples | 10–21 | 18.0 / 12.1 | 1.48 | 3.6 / 0.3 / 0.2 | 1.50 | 95.1 / 87.3 | 1.09 | 4.9 / 1.7 | 2.85 | 1219 / 601 | 2.0 |
| with_cfg-examples | 12–21 | 33.2 / 28.2 | 1.18 | 4.0 / 0.4 / 0.1 | 2.67 | 8.9 / 5.9 | 1.52 | 5.0 / 1.2 | 4.03 | 1737 / 1678 | 1.0 |
| yalantinglibs | 14–35 | 12.0 / 6.8 | 1.78 | 3.4 / 0.2 / 0.2 | 1.00 | 58.6† / 52.2† | 1.12 | 54.4† / 53.7† | 1.01 | 679 / 244 | 2.8 |

- **Cold `build --nobuild //...`** (module resolution, every fetch,
  loading, analysis): bazel-rs is faster on 29 of 37 projects, typically
  1.1–2.5× (abseil 13.2 s against 17.2, capnproto 11.1 against 39.2,
  rust-ffi 22.6 against 56.1, both with slow genuine downloads). Slower:
  OpenROAD (120.1 against 90.7 s, P7), protobuf (62.3 against 41.9),
  examples-frontend (121.6 against 88.3, both failing on a download
  timeout), rules_kotlin (45.1 against 37.6), rules_cc (20.5 against 18.8)
  and examples-java-maven (56.0 against 34.8, during a load peak of 82).
- **Warm `build --nobuild`:** a fresh bazel-rs process with its analysis
  snapshot takes 0.1–0.2 s on every project whose analysis succeeds, like
  genuine's warm server (0.1–0.6 s) and 15–30× faster than genuine
  `--batch` (2.5–14.1 s); OpenROAD 1.4 s against the server's 0.7 s and
  `--batch`'s 8.7 s. A command whose analysis failed saves no snapshot,
  so its repeat re-analyses (protobuf 15.2 s, rules_zig 12.0, rules_cc
  5.7, riegeli 3.2 against the server's 0.3–0.6 s).
- **Full build:** bazel-rs is faster or even on 32 of 37 (abseil 66.0 s
  against 76.3, tink-cc 87.3 against 95.1, highway 172.6 against 178.0).
  Slower: protobuf (648 against 287 s: B48 built protoc 8 more times;
  302 against 290 s after corpus-parity6),
  examples-frontend (137.7 against 60.8, both failing), OpenROAD
  `//:openroad` (994.6 against 714.0 s for the same 13,479 actions, at a
  higher load: 20–52 against 12–38; openroad-perf measured 773.9 against
  693.5 s), rules_kotlin (79.0 against 66.9, failing on B51; 59.7
  against 59.2 after corpus-parity6).
- **Tests:** faster or even on 31 of the 34 `test` commands (some without
  test targets); slower on
  examples-frontend, rules_python-bzlmod (10.3 against 7.5 s) and rules_cc
  (fewer tests run).
- **Memory:** bazel-rs's cold peak RSS is lower on 32 of 37, typically
  1.5–3×. Higher: OpenROAD (6.9 GB against 3.7 GB), protobuf (3.6 against
  2.7 GB), rules_python-bzlmod (2.6 against 2.0 GB), examples-frontend (1.8
  against 1.5 GB).

### Earlier summary (first runs, 2026-11-16/18)

- **Cold `build --nobuild //...`**: bazel-rs was faster on 9 of the 11
  matching projects, by 1.2–1.7×, slower on rules_python pip_parse (P3) and
  OpenROAD (P4); since cold-eval (fully cold) abseil 10.5–12.6 s against
  13.1–13.8, pip_parse 29.6–30.5 against 28.0–33.4, OpenROAD 110–111
  against 70–76 (67 s with the download cache pre-filled; P7).
- **Warm `build --nobuild`:** 0.1–0.2 s with the snapshot; since
  corpus-perf the command that wrote `MODULE.bazel.lock` saves a snapshot
  too (P1).
- **Full build:** bazel-gazelle (P2, fixed) even with genuine; OpenROAD
  `//:openroad` 1,536 s → 773.9 s against 693.5 s (P6).

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

From the corpus-rerun run (2026-11-20):

1. The blocked project: B13c (rules_zig; the design above). (B48–B52
   are fixed on corpus-parity6.)
2. B53: rules_kotlin's `test //...` (nested Bazel integration tests) does
   not finish in 20 minutes on bazel-rs; genuine takes 772 s.
3. Output parity left: B43, B34, and the rules_cc/protobuf analysis gaps
   B32 and B33.
4. Performance: OpenROAD cold analysis (P7) and peak RSS (6.9 GB against
   3.7 GB), `//:openroad` build time, protobuf's memory, examples-frontend.
5. Rerun the corpus after each fix: `oss_corpus.py run --only <project>`.
