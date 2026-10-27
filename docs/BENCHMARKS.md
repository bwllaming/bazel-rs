# Benchmarks against genuine Bazel 9.3

This is the first like-for-like comparison of bazel-rs with genuine Bazel 9.3.0
(ROADMAP milestone 5). It uses three public bzlmod repositories that both tools
build with identical outputs. The harness is `scripts/bench_genuine.py`, with
repositories and edits in `scripts/bench_genuine_repos.json` and unit tests in
`tests/test_bench_genuine.py`. The full raw report (every sample, every side)
was written alongside the run (`bench_genuine.py report`).

## Method

- **Binaries.** Genuine: the official `bazel-9.3.0-linux-x86_64` ELF
  (sha256 `d302d22e…`). bazel-rs: the installed release build of 2026-10-26
  (sha256 `a1887179…`, built from main d8407d5).
- **Isolation.** Each side has its own copy of the workspace and its own state:
  output user root, repository cache and HOME for genuine, and `--cache-dir`
  and HOME for bazel-rs. Genuine uses a shared, pre-extracted `--install_base`
  so that extraction is measured on its own (`startup`). Every command runs
  under `env -i PATH=/usr/bin:/bin HOME=… TMPDIR=…`, at nice +10, with
  `--ignore_all_rc_files` and `--jobs=16`. Both sides run the same target
  patterns.
- **Sides.**
  - `gs`: genuine, server mode, with the server warm.
  - `gs_restart`: genuine after `bazel shutdown`, so the JVM is cold but the
    on-disk action cache persists.
  - `gb`: genuine `--batch`.
  - `rs`: bazel-rs, a fresh process every time (there is no daemon).
- **Scenarios.**
  - `cold`: a fresh workspace copy and fresh output base, repository cache
    and HOME, then `build`. This covers module resolution, every download,
    analysis and execution.
  - `noop`: `build` again.
  - `source_edit`: append a new top-level declaration to one source file
    (Go/Python), then `build`.
  - `build_edit`: append a new `filegroup` to one BUILD file, then `build`.
  - `query`: `query //...`.
  - `test`: `test` with every result cached.
  - `startup`: `version`. This scenario measures genuine extraction, a cold
    server start and a warm server.
- **Ordering and repetitions.** Each scenario has one unmeasured warm-up per
  side. Trials are interleaved, and the side order rotates every repetition.
  There were 5 repetitions for cold builds and 7 for everything else. Tables
  show the median, p95 (nearest rank; with 7 samples this is the maximum) and
  the raw samples.
- **Memory.** For `gs`, peak RSS is the genuine server's VmHWM for the command
  (reset through `/proc/<pid>/clear_refs` before it) plus the client. For
  `gb`, it is the batch JVM's max RSS. For `rs`, it is the bazel-rs process's
  own VmHWM (polled). Child actions are excluded on every side.
- **Correctness gates (all passed).**
  - Exit codes are identical in every trial.
  - The top-level default outputs of a verification build are identical. For
    genuine they come from BEP `namedSetOfFiles`; for bazel-rs, from the
    `label<TAB>path` lines. They are compared by relative path and SHA-256:
    0 differences in 2,313 (gazelle), 7 (pip_parse) and 4 (basic_gazelle)
    files. Go binaries are byte-identical.
  - `query //...` output is identical as a set.
  - Test outcomes and the `Executed N out of M tests` line are identical.
  - The same source edit executes the same 182 spawns on both sides
    (181 GoLink + 1 GoCompilePkg, from `--execution_log_json_file`).
- **Host.** Intel Xeon 6731P, 64 CPUs, 251 GB RAM, Linux 6.8. The machine was
  **shared** with other workers (load average 5–57 during the runs). Cold
  numbers include public network downloads (BCR, Go proxy, PyPI, GitHub). The
  timings are indicative.

## Repositories

Each repository was used as published, with only these modifications:

| name | source (commit) | what it exercises | modifications |
|---|---|---|---|
| basic_gazelle | bazel-contrib/rules_go `examples/basic_gazelle` (fb591bc) | rules_go 0.63.0, gazelle 0.51.3, go_deps (x/net, testify); 4 targets | rules_go/gazelle/Go bumped to the ~/code versions (rules_go 0.52 fails on Bazel 9: `CcInfo` removed); its `.bazelrc` and lockfile dropped |
| gazelle | bazel-contrib/bazel-gazelle HEAD (5cc8299) | ~250 packages, 396 targets, 1,083 sandboxed spawns cold, 177 go_tests, go_deps/go.work, protobuf | `for key in "A", "B":` parenthesized in `internal/env.bzl` (a bazel-rs parse error, see below). The vendor BUILD files the repo's `.bazelrc` hides with `--deleted_packages` are deleted. Excluded: `//docs/...` (stardoc), the rules_testing analysis tests (`//tests/{bzlmod,cli,bzl_deps}/...`, `//internal:gazelle_binary_test*`) and, for `test`, `//internal:bazel_test` (it runs Bazel itself) |
| pip_parse | bazel-contrib/rules_python `examples/pip_parse` (9e587e8) | hermetic CPython 3.14, `pip.parse` hub (requests, sphinx, yamllint, libclang…) | rules_python 1.7.0 from the BCR instead of `local_path_override`. `//:pip_parse_test` excluded (bazel-rs analysis gap, see below) |

## Results (median s, p95 in parentheses)

### gazelle (medium Go repository)

| scenario | genuine warm server | genuine after shutdown | genuine --batch | bazel-rs |
|---|---|---|---|---|
| cold fetch+build | 49.45 (57.07) | — | 49.70 (51.54) | **85.82 (97.04)** |
| no-op build | 0.42 (0.67) | 4.84 (6.51) | 4.90 (6.25) | 0.40 (0.44) |
| source edit (`rule/rule.go`) | 4.71 (6.43) | — | 8.62 (11.22) | **18.75 (22.04)** |
| BUILD edit (`rule/BUILD.bazel`) | 1.05 (1.19) | — | 5.05 (6.15) | **13.77 (14.53)** |
| `query //...` | 0.14 (0.33) | 2.90 (3.08) | 2.60 (2.80) | 0.41 (0.43) |
| `test` (177 tests, cached) | 0.42 (0.48) | 4.95 (9.95) | 4.94 (6.20) | 0.66 (0.81) |
| peak RSS, cold (MB) | 1,585 (server) | — | 1,547 | 464 |
| peak RSS, warm commands (MB) | 1,385–1,601 | 498–1,446 | 468–1,074 | 116–505 |

### basic_gazelle (small Go repository)

| scenario | genuine warm server | genuine after shutdown | genuine --batch | bazel-rs |
|---|---|---|---|---|
| cold fetch+build | 52.40 (54.44) | — | 53.17 (55.74) | 52.71 (58.69) |
| no-op build | 0.32 (0.41) | 4.43 (4.52) | 3.95 (4.34) | 0.26 (0.34) |
| source edit | 0.78 (0.87) | — | 4.57 (4.94) | 0.82 (0.96) |
| BUILD edit | 0.32 (0.36) | — | 4.63 (5.98) | 2.33 (2.46) |
| `query //...` | 0.10 (0.15) | 2.25 (2.32) | 1.91 (2.08) | 0.19 (0.19) |
| `test` (cached) | 0.31 (0.46) | 4.29 (7.07) | 4.14 (4.81) | 0.30 (0.40) |
| peak RSS, cold (MB) | 1,334 | — | 1,205 | 376 |

### pip_parse (Python + pip)

| scenario | genuine warm server | genuine after shutdown | genuine --batch | bazel-rs |
|---|---|---|---|---|
| cold fetch+build | 22.18 (28.78) | — | 20.97 (32.51) | 21.61 (21.81) |
| no-op build | 0.27 (0.30) | 3.70 (4.15) | 3.61 (4.04) | 0.24 (0.25) |
| source edit (`main.py`) | 0.23 (0.32) | — | 3.94 (4.11) | 0.25 (0.36) |
| BUILD edit | 0.26 (0.32) | — | 3.81 (4.84) | 1.83 (1.92) |
| `query //...` | 0.07 (0.10) | 2.01 (2.35) | 1.81 (1.94) | 0.15 (0.16) |
| `test` (cached) | 0.26 (0.44) | 3.93 (8.62) | 3.96 (4.58) | 0.39 (0.42) |
| peak RSS, cold (MB) | 1,326 | — | 1,143 | 617 |

### Startup (`version`, all three repos)

| | median s |
|---|---|
| genuine: extraction into a fresh install base + batch JVM | 1.81–2.19 |
| genuine: cold server start | 1.34–1.54 |
| genuine: warm server | 0.03–0.04 |
| bazel-rs | 0.01 |

## Reading the results

- **Warm commands.** On warm no-op builds and cached tests, bazel-rs (a fresh
  process with its analysis snapshot) matches genuine with a warm server.
  Query is 0.1–0.3 s slower. Against genuine without a warm server (after
  `shutdown` or `--batch`), bazel-rs is 7–17× faster on these commands.
- **Memory.** bazel-rs uses 2–12× less memory than the genuine server
  (75–620 MB against 0.8–1.6 GB).
- **Incremental analysis is the gap.** A BUILD edit costs bazel-rs a full
  re-analysis: 1.8 s (pip_parse), 2.3 s (basic_gazelle), 13.8 s (gazelle),
  against 0.3–1.1 s for genuine's warm server. On gazelle bazel-rs is slower
  than even genuine `--batch` (5.1 s).
  - BAZEL_RS_TIMING shows 12.6 s of planning on gazelle. About 9 s of it is
    repository re-validation after the snapshot miss:
    `PERF-REPO recorded_check=9.005s/71`, `execute=8.397s/55`, 139 MB of
    recorded inputs re-hashed.
  - A `.go` edit on gazelle misses the snapshot too (`READ …/rule/rule.go:
    content changed`). The repository's `go.work`-driven go_repository watches
    the workspace tree, so the miss itself is correct.
  - bazel-rs therefore pays a full re-plan (16 s) on top of the same 182
    spawns that genuine runs. Genuine re-evaluates only the invalidated nodes.
- **Cold builds.** On the two small repositories, cold builds are on par. On
  gazelle, bazel-rs takes 86 s against 49 s:
  - 40 s is spent before analysis, in loading and repository fetches. The
    repository executions sum to 40.8 s across 73 repositories, so they run
    nearly serially. Examples: `go_deps` buildtools 11.9 s,
    `bazel_gazelle_go_repository_tools` 11.2 s.
  - Cold analysis takes 24 s, and execution takes 30 s for the same 1,083
    spawns (genuine's critical path is 23 s).
  - Genuine overlaps fetching, analysis and execution in Skyframe.
  - Also note that bazel-rs ran some actions unsandboxed here ("not
    sandboxed"), while genuine used processwrapper-sandbox for all but 5.

## Correctness and compatibility findings (bug reports)

None of these changes outputs: every finding either rejects the command
up front or blocks analysis of the affected targets. They are why the
benchmark needed the patch and exclusions listed above.

1. **Parser.** An unparenthesized tuple as a `for` iterable is rejected:
   `for key in "GONOPROXY", "GONOSUMDB":` fails with "Parse error: unexpected
   symbol ',', expected symbol ':'". Genuine accepts it (bazel-gazelle HEAD,
   `internal/env.bzl:223`). Any project depending on that gazelle revision
   fails to load.
2. **Unsupported options** that genuine accepts. bazel-rs exits 1 with
   `unsupported option`:
   - `--check_direct_dependencies=off` (in the gazelle `.bazelrc`)
   - `--deleted_packages=…`
   - `--incompatible_python_disallow_native_rules` (in the rules_python
     examples' `.bazelrc`)
   - `--subcommands`
3. **Analysis gaps.**
   - `rule(_skylark_testable=True)` (unittest/analysistest from
     rules_testing/skylib)
   - `rule(analysis_test=True)`
   - the native rule `java_plugins_flag_alias` (`@bazel_tools//tools/jdk`,
     reached through stardoc)
   - `py_internal.expand_location_and_make_variables` (a rules_python 1.7.0
     `py_test` with `env = {"X": "$(rootpaths …)"}`)
4. **Different progress counts, same work.** The `INFO: Analyzed N targets
   (P packages loaded, T targets configured)` line reports far fewer
   configured targets in bazel-rs: 5,596 against 29,615 (gazelle), 6,352
   against 12,311, and 1,319 against 6,716. Packages loaded are equal
   (±1). "total actions" also differs (1,728 against 1,950) because the two
   tools count actions differently. The spawns executed are the same.

## Reproducing

```sh
B=/abs/scratch/dir   # never a private repository: genuine Bazel runs on every workspace
python3 scripts/bench_genuine.py run --config "$PWD/scripts/bench_genuine_repos.json" \
  --work $B/run --bazel /abs/bazel-9.3.0-linux-x86_64 --bazel-rs /abs/bazel-rs \
  --reps 7 --cold-reps 5 [--repo gazelle] [--scenario noop ...]
python3 scripts/bench_genuine.py verify --config "$PWD/scripts/bench_genuine_repos.json" --work $B/run
python3 scripts/bench_genuine.py report --work $B/run --out $B/run/report.md
```

A full run of the three repositories takes about 65 minutes.
