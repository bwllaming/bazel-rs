# Cold fetch phase (cold-fetch-speed)

What a build does before analysis can run on an empty cache: module
resolution against the registry, module extension evaluation, repository
rules (downloads, extraction, `execute()`), publication into the repository
store. This note records where that time went on the public benchmark
repositories (docs/BENCHMARKS.md) and on ~/code, what changed, and what is
left. Every change is scheduling or transport only: the bytes fetched, the
repositories executed, the lockfile and every published tree are the same.

## Where the time went

`BAZEL_RS_TIMING_TIMELINE=1` spans (docs/MODULE_EXTENSIONS.md "Profiling";
`transfer`, `registry`, `resolve` and `builtin` were added here):

* **Download permits starved the coordinator.** On ~/code's cold
  `build --nobuild //src/...` 3,477 npm tarballs, ~350 wheels and ~175
  Debian packages were fetched speculatively by 32 workers sharing 8
  process-wide permits. The coordinator's own downloads queued behind them:
  the Go SDK's `download_and_extract` took 19.3 s (3 s alone), rules_rust's
  `stable_tools` 14 s.
* **No connection reuse.** Each repository built its own HTTP client
  (loading the native root certificates) with the connection pool switched
  off (`pool_max_idle_per_host(0)`): every tarball paid TCP + TLS setup.
* **A slow host held every permit.** snapshot.debian.org answers in
  10–40 s per package; eight of them blocked every other host's transfers.
* **Decompression alternated with the tar writes** on one thread, and
  hermetic_cc_toolchain's `zig_config` extracts the same 45 MB tar.xz twice
  (as genuine does): the xz decode alone is 2.7 s of each.
* **Module discovery reads one horizon at a time**: ~/code's 386
  `MODULE.bazel` files took 5.2 s, although MODULE.bazel.lock names each
  one with its checksum.
* **A repository rule walking a label list fetched each repository at its
  turn**: rules_python's `whl_library` resolves its 15 `pypi__*` tool
  repositories one after another (3.3 s of sequential small downloads on
  pip_parse).
* **`execute()` polled with fixed 5 ms waits**: `uname -m` took 9 ms;
  aspect_rules_js runs three such commands per npm package (10,465 on ~/code).

## Changes

* **One pooled download client** (`extension_download::client`): process-wide,
  keep-alive pooling (30 s idle), built once per proxy environment. npm
  transfers: p50 22 ms, p90 0.23 s.
* **Demanded transfers first; permits per host**
  (`extension_download::DownloadPermit`). A transfer the build waits for now
  — the coordinator's, a module extension's it evaluates, a speculation's
  the coordinator waits for (`Speculations::take` marks it demanded) — may
  use 4 reserve permits, and speculative transfers yield while one waits.
  Genuine's 8 permits (`--http_max_parallel_downloads`, still accepted and
  ignored) are 8 per host, under a total of 16. The Go SDK's demanded
  download: 19.3 s → 3.1 s.
* **Pipelined decompression** (`archive::pipelined`): gzip, xz, zstd and
  bzip2 decode on their own thread, 256 KiB chunks, 16 in flight. Decoder
  errors arrive at the same stream position; a module-root warm-up's
  cancellation is checked on the extracting thread.
* **A repeated extraction skips decoding** (`archive::DECODED`): the decoded
  tar stream of the last xz/bzip2/zstd archive of at least 16 MB is kept in an
  unnamed temporary file, keyed by the decoder and the SHA-256 of the
  compressed bytes, once the decoder reached its end cleanly; one per thread,
  released when the repository rule running there ends (a single shared
  entry was evicted by other workers' extractions on ~/code). The same tar
  entries and checks run over it. zig_config's second extraction:
  4.1 s → 1.3–1.4 s.
* **Registry files the lockfile names are fetched at once**
  (`RegistrySession::warm_known_files`, docs/REGISTRY_LOCKFILE.md): into
  the content-addressed cache only, nothing recorded or memoized, a
  mismatching transfer dropped. `BAZEL_RS_DEBUG_NO_REGISTRY_WARM` turns it
  off.
* **Label-list siblings fetched ahead** (`repository_ctx` `label_siblings`,
  `RepositorySet::fetch_ahead`): when a repository rule resolves a label of a
  label-list (or label-keyed / label-valued dict) attribute, the other
  repositories of that attribute are requested on the speculation pool.
  Scheduling only: a repository the rule never resolves costs one fetch.
* **Retries like genuine's downloader.** A request that fails before any
  response byte (a refused or reset connection, a pooled keep-alive
  connection the server closed meanwhile — the keep-alive reuse race), and
  403/408/429/5xx but 501/505, are tried up to 8 times per URL after
  HttpConnector's backoff (100 ms doubling, ×0.75–1.25; HttpConnector.java
  :118-300); unknown hosts, certificate failures, 404/410 and other 4xx are
  final. A transfer cut at the socket (reset, truncated body) is downloaded
  again up to 5 times (`--experimental_repository_downloader_retries`'
  default; DownloadManager.shouldRetryDownload, :372-393; the option itself
  is still accepted and ignored). Both for repository downloads and registry
  files (`registry::open_http_with`, `read_http`). Before, registry files had
  no retry at all and downloads one, for connect errors only; with pooled
  connections a closed idle connection surfaced as `HTTP request failed`.
  Pooled connections are dropped after 30 s idle (registry and download
  clients), below common server keep-alive timeouts.
* **`execute()` exit polling backs off** from 50 µs to 5 ms
  (`repository_process::backoff`).

## Measured

Cold = fresh output base / `--cache-dir`, repository cache and HOME;
`build --nobuild` of the benchmark's targets with its flags, `--jobs=16`,
`nice -n 10`, shared 64-core host, public network; median of 3 interleaved
runs, seconds. bazel-rs before = main 00a36c5f (v0.4.10); genuine = 9.3.1
`--batch` with a pre-extracted install base (as docs/BENCHMARKS.md).

| repository | bazel-rs before | bazel-rs after | genuine 9.3.1 |
|---|---|---|---|
| gazelle | 33.1 (32.4–33.7) | **28.9** (28.8–29.7) | 30.4 (29.2–30.6) |
| pip_parse | 24.2 (23.5–27.4) | **21.2** (20.0–22.0) | 21.7 (20.6–22.6) |
| basic_gazelle | 22.9 (22.8–22.9) | **22.3** (22.2–22.8) | 26.7 (26.2–27.6) |

The fetch phase is now at or below genuine's on all three. The full cold
build of gazelle (fetch, analysis, execution) is 64–66 s against genuine's
49 s (before: 66–69 s): what remains is analysis and execution, which
genuine overlaps with fetching.

The lockfiles written are byte-identical before/after/genuine; stderr
differs only in cache paths.

~/code (private clone at 80d6724f3a, private cache and lockfile copy, the
1,024 snapshot.debian.org packages seeded into the download cache
identically for every run — the archive answered 504 for some of them and
takes 10–40 s per package; one run each, seconds):

| | before | after |
|---|---|---|
| cold `build --nobuild //src/...` | 381 | 373, 386 |
| cold CI `affected --base=HEAD^` (737 targets, identical output) | 329 | 324 |
| module resolution (`resolve` span) | 5.2 | 3.2 |
| Go SDK `download_and_extract` (demanded) | 3.4–19.3 | 2.5 |
| zig_config's second extraction | 5.0 | 1.3 |
| 10,468 short `execute()`s (worker time) | 101 | 58 |

These workloads are analysis-bound: the coordinator spends ~300 s in
phase-1 analysis, of which ~180 s waiting on or executing fetches whose
order the analysis decides (below); the fetch-side savings are within the
run-to-run noise of the total. Lockfile copies are unchanged (sha256
`ca7c20a7…` before and after every run); the download set is identical
(8.2 GB `//src/...`, 6.8 GB `affected`).

## Not changed (measured)

* **Module extensions stay sequential.** With MODULE.bazel.lock entries, the
  extension spans on ~/code's cold `//src/...` sum to 14.7 s, spread over the
  run (`pip` 6.2 s at 12 s, `npm` 3.4 s at 20 s, `go_deps` 1.3 s at 95 s);
  on gazelle (no usable lockfile) `go_deps` (9.5 s) depends on `go_sdk`'s
  repositories. Evaluating independent ones concurrently would save a few
  seconds at most, against a new class of speculative state (deferred
  prints and observations of an evaluation, its lockfile record) — not done.
* **select()-gated wheels** (torch's `nvidia_*` dependencies) are decided by
  rules_python's environment-marker targets, which only the coordinator's
  analysis evaluates; on ~/code the coordinator waits 10–16 s for each
  large one that becomes certain late.
* **Bandwidth for demanded transfers.** A demanded download shares the
  link with up to 16 speculative ones (zig's 45 MB: 2.7–16 s; rules_kotlin's
  23 MB module archive: 21 s, 0.8 s alone). Pausing speculative transfers
  while a demanded one runs cut the module-root waits (49 → 22 s) but grew
  the waits for speculations (37 → 56 s): not adopted.
* **Registered toolchains fetched at registration.** Marking each toolchain
  type's first compatible toolchain (target settings matching) certain took
  rules_rust's tools and zig off the coordinator, but also fetched 44
  repositories the build never uses (rules_multitool's per-tool toolchains,
  1.8 GB): not adopted; it would need the toolchain types the analyzed rules
  actually require.
* **Discovery horizons**: without a usable lockfile (gazelle's is version
  18, which 9.3 discards, as genuine does) resolution still reads ~20
  horizons one after another (3.0–3.5 s, the same as genuine's structure).
