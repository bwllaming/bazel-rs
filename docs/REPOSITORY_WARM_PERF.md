# Warm repository reuse — perf increment (Step A record)

Branch `perf-repo-warm` @ f0f9c22. Workload: `~/code`, release binary,
`--ignore_all_rc_files query --experimental-builtin-modules
--lockfile_mode=off --loading_phase_threads=32 //src/ts:all` (8,345 targets),
default cache `~/code/bazel-rs-out`. `~/code` is read-only for this work.

## BEFORE (pristine f0f9c22)

| run | wall | trees before→after |
|---|---|---|
| 1 | 187.8s | 8538 → 10593 (spec churn re-published) |
| 2 | 123.1s | 10593 → 10593 |
| 3 | 129.3s | stable |
| 4 | 123.4s | stable |

Stdout byte-identical across all runs.

## Instrumentation (Step A commit)

`BAZEL_RS_TIMING=1` now also prints, at command end (query after
`select.end`, build before execution):

```
PERF-REPO reused=N executed=M manifest_read=… recorded_check=… snapshot=… (files, bytes) execute=… publish=… spec_serialize=… prepare_group=…
PERF-REPO spec bytes total=… provenance bytes total=…
PERF-REPO warm-validation serial sum=…; per-root serial sum=… over R roots; parallel-eligible estimate at T workers: …
PERF-REPO root <secs> <reused|executed> <canonical>   (top 25)
```

Phases are accumulated in `repository_store::timing` (relaxed atomics; one
load per phase when unset). `spec_serialize` is timed at the two
`repository.rs` call sites (provenance JSON + specification).

## Measured breakdown (warm, instrumented, before any fix)

```
reused=2056 executed=2  manifest_read=0.046s/2058  recorded_check=0.010s/2058
snapshot=0.203s/2055 (9,072 files, 23,069,616 bytes)  execute=0.127s/3
spec_serialize=30.7s/2058   provenance bytes total=64,291,933,352 (64 GB!)
per-root serial sum=51.4s over 2058 roots
```

Findings, definitively:

1. **rules_js `__links` repos are REUSED, not re-executed, warm.** 2,056
   reused vs 2–3 executed per warm run; zero new trees. The top per-root
   entries are `aspect_rules_js++npm+npm__*__links` at ~35 ms each — that
   is the per-repo fixed cost below, ×2,058 repos ≈ 51 s serial.
2. **The snapshot full re-hash is NOT the warm bottleneck on this
   workload** (0.20 s total; the demanded trees are small: 9k files /
   23 MB). The oracle brief's hypothesis (a) holds only for the giant
   trees (Go SDK 17k files/244 MB) when they are demanded — see Step B.
3. **Root cause of warm slowness: per-repository provenance/spec
   serialization.** The direct-repository spec embeds
   `prepared.loaded.sources` — the full bytes of the defining `.bzl` load
   closure (rules_js macro graph ≈ 31 MB/repo) — serialized with
   serde_json and SHA-256 hashed *per repository*, although the closure
   is identical for all 2,058 repos of a group family: 64 GB of JSON per
   warm invocation, ~30 s in `spec_serialize` alone, and the same bytes
   again in `materialize`'s identity framing. `prepare_group` measures
   0.000 s because `load_definition_in` uses a fresh `Loader` but the
   shared `BzlCache` serves module values after the first evaluation —
   the *sources map*, however, is rebuilt and re-serialized per repo.
4. The remaining ~65 s of the 123 s wall is `.bzl`/BUILD evaluation on
   the coordinator: workers run under the no-demand sentinel, the first
   demand of each of 2,058 repos aborts the unit, and the coordinator
   re-runs it inline (Step C attacks exactly this).

## Fix landed with Step A (digest-form specification)

`repository_store::materialize_keyed` takes the pointer identity from the
caller. Direct repos compute `repository_warm::specification_digest`:
SHA-256 over the provenance JSON with each source replaced by
`{sha256(content), mapping}` + host + declared-environment framing.
Ordinary repos use `specification_digest_bytes` over their already-framed
provenance bytes. The full provenance bytes still enter the *content*
identity exactly as before (tree identities unchanged; the one-time
republish above produced the same tree count ±1). New pointers mean each
repo executes once on first use after upgrade, then reuses.

After: `spec_serialize` total bytes 64 GB → 132 KB of keys; warm wall
123 s → ~60 s (see PROGRESS / final report). `specification()` (byte form)
is retained under `#[cfg(test)]` as the reference.

## Parallel-eligible estimate

Warm validation (manifest+recorded+snapshot) sums to 0.23 s serial —
already negligible here; per-root total 0.34 s after the digest fix. The
serial remainder is coordinator `.bzl` evaluation gated on repo demand:
with reuse-only demand on workers (Step C), the 2,058 reuse checks
(~0.1 ms each) leave the critical path and units stop aborting.

## Step B — metadata-first snapshot

Manifest `Entry` gains `stat: {dev, ino, mtime, ctime}` (serde-optional;
legacy manifests parse unchanged and hash-verify until re-published).
The content identity is computed over `content_view` (entries without
stat), so tree identities — and the persisted-bytes fixture round-trip —
are unchanged. Warm `verify_snapshot`:

- `Stat` (default): walk comparing structure/mode/size/symlink + the stat
  fingerprint; trusted files (fingerprint equal, mtime+ctime older than
  the 2 s racy margin, mirroring `action::racy_safe`) are not hashed.
  Fingerprint mismatch or racy → hash that file (a size+mtime-preserving
  tamper moves ctime/inode and is caught — test
  `stat_verify_reuse_and_tamper_matrix`: do-nothing reuse, size-change
  re-execute, touch-preserved tamper re-execute).
- `Full` (opt-in): `BAZEL_RS_VERIFY_REPOS=full` or
  `--verify-repo-tampering` re-hashes everything (pre-Step-B behavior).

Measured on the real 15,643-file / 244 MB tree: stat walk 0.39 s vs hash
walk 2.73 s in Python; the Rust verifier shares the walk code with the
hash path minus the reads (target ≤50 ms is for the warm *reuse check of
the Go SDK tree inside a run* — with fingerprints recorded it performs
15k stats and zero reads; the residual is page-cache dependent and
reported honestly in the final numbers rather than asserted).

Note: manifests written before Step B carry no fingerprints, so each
such tree hash-verifies once more (still reuses — no re-execution), and
upgrades when it next executes for another reason.

## Step C — parallel warm reuse (reuse-only demand)

Workers already ran under the no-demand sentinel; every first repo demand
aborted the load unit to the coordinator, serializing all 2,058 warm
checks (and their `.bzl` provenance work) on one thread. Step C adds a
**reuse-only** thread mode (`repository_demand::reuse_only`, entered by
loading workers inside the no-demand guard):

- `DemandSlot::get_or_probe`: under the guard, a pure-read probe may
  produce the value (published into the slot for all later demands); a
  miss returns the sentinel WITHOUT populating the slot — the coordinator
  fetches inline at the commit slot, in demand/pattern order, exactly as
  before. Errors, prints and first-error-wins are untouched: probes never
  print, never execute a rule, never evaluate an extension.
- `repository_store::try_reuse` is the shared warm probe (pointer →
  manifest → recorded-input validate → stat verify), also used by
  `materialize_keyed` itself.
- Direct repos probe only when the group's prepared slot is already
  ready (definition/attributes known without evaluating anything new);
  ordinary repos only when the extension group is already prepared.
  `local` rules never probe (they never reuse across invocations).

Batch-prefetch evaluation: the natural boundaries are (a)
`select_filtered`'s package list — already known before workers start,
but the repos each package needs are only discovered by its load DFS —
and (b) each `load()` statement — inside the DFS. A separate prefetch
pass would have to guess the load graph; the probe instead converts the
demand itself into the prefetch: the first worker to demand a repo
validates it in parallel with the other workers' loads, and the slot
publish serves the rest. Extension/callback execution stays on the
coordinator (fallback path unchanged).

Determinism: warm `//src/ts:all` stdout byte-identical at
threads 1/8/32 and threads=8 + 25 ms jitter; the three failure shapes
stay green in tests/parallel_loading.rs (sentinel abort suppresses
partial prints; cached-failure replay; first error in pattern order).

## Acceptance measurements (Step D, final binary, warm, t=32)

| workload | BEFORE (f0f9c22) | AFTER | target | verdict |
|---|---|---|---|---|
| `query //src/ts:all` | 123.1–129.3s | **57.8–65.9s** (stdout byte-identical) | ≤25s | MISS |
| `query //src/...` (18,798 targets) | not recorded (est. ≥230s by ts ratio) | **143.9–226.5s** | ≤60s | MISS |
| warm build 41 genrule targets, 363 actions | ~240s class (60-target set) | **68–77s, 363/363 cache hits, 0 executed** | ≤15s at 384/384 | MISS |
| new trees on warm runs | +54/run class | **0** (12,386 stable across all warm runs) | 0 | PASS |
| `BAZEL_RS_DUMP_ACTIONS` dump | — | byte-identical t=1 vs t=32 vs jitter8 (after `trees/<hash>` normalization, as before) | identical | PASS |
| stdout at t=1/8/32 + jitter | — | byte-identical on query and build | identical | PASS |
| full cargo test | — | 104 suites / ~2,027 tests, 0 failed | green | PASS |
| python validators | 881 | 883 tests OK | green | PASS |
| fmt / clippy -D warnings | — | clean | clean | PASS |

### Why the walls miss, honestly

Repository work is now off the critical path (warm validation totals
0.2–1.1 s per run; per-root serial sum 0.26–1.2 s). What remains is
`.bzl` evaluation: the rules_js/rules_go macro graphs are re-evaluated
in every process (no resident server), and `Work::Finish` serializes
evaluation on the single global `BZL_EVALUATION` lock (review §1.3's
known simplification, flagged in PARALLEL_ANALYSIS.md as the N62
lever: per-label single-flight with in-flight waiting, or an
invocation-persistent `.bzl` cache). At t=1 vs t=32 the walls are
identical (60.5 vs 60.9 s), proving the remaining time is that serial
evaluation, not repository fetching — this increment removed the
repository share (~63 s of the 123 s; spec serialization 64 GB → digest
keys, warm validation 51 s serial → parallel probes).

Also observed, pre-existing at f0f9c22 and out of scope: building
`*_unique_buildid` genrule targets fails with `invalid direct
repository path or path/depth limit exceeded` (confirmed identical on
the pristine f0f9c22 binary), and `//src/k8s:alertdashboards` fails in
`ctx.actions.run` (`progress_message` None). The acceptance build set
is the 41 genrule targets in artifacts/stepD-50.txt that build at
baseline.

## Perf follow-up (commits 97caa84 + 6fa2be8, 2026-10-21)

Review items R1–R4 landed in 97caa84; R5 (progress_message) in 6fa2be8.

### What the dead workers' diff implemented (adopted after audit)

1. **Source digests once per prepared group**: `PreparedGroup.source_entries`
   is a `OnceLock<Result<Vec<u8>,String>>` of digest-entry bytes; the direct
   spec key *streams* per-repo fields + those cached bytes through a
   counting pass + a SHA-256 sink pre-seeded with the Step-A framing
   (`specification_digest_streaming`). Ordinary: `PreparedOrdinary` caches
   the SHA-256 of the group provenance once + per-canonical spec keys
   (`specification_digest_mixed`, new pointer profile — old ordinary
   pointers miss once and republish; direct pointers byte-identical:
   reused count unchanged, test `source_digest_streaming_matches_value_form`
   + `spec_reference_matches_streamed_and_racy_never_trusted`).
   **Provenance bytes are lazy**: `materialize_lazy` runs the provenance
   closure only on the execute path (after the double-checked warm probe
   under the stage lock added during adoption — sequential semantics
   unchanged). Warm provenance-bytes total: 0.
2. **Probe errors are sentinel misses**: `get_or_probe` maps every probe
   `Err` to `Ok(None)`; the coordinator's sequential path owns all errors
   (test `probe_error_maps_to_sentinel_without_populating`).
3. **Racy fingerprints (git model)**: a fingerprint is trusted only when
   mtime AND ctime are >=2s older than the verify start; `snapshot`
   records a fingerprint only for files already settled at recording time
   (`settled_fingerprint` — freshly written trees carry none); a fallback
   hash match refreshes the manifest fingerprint **atomically**
   (`refresh_manifest`: re-read, identity+content guard, temp+rename;
   since i20 a try-locked compare-and-swap, see "Safe warm refresh");
   `PERF-REPO files_hashed_during_verify=N` counts fallback hashes.
4. **`unwrap_or_default` x2 in `spec_digest_identity` removed**: mapping and
   framed serialization errors propagate as `Err`.
5. **R5 (separate commit 6fa2be8)**: `ctx.actions.run`/`run_shell` accept
   explicit `None` for `mnemonic`/`progress_message` (string-or-NoneType,
   Bazel's declared types); non-string non-None fails with Bazel's message.
   `//src/k8s:alertdashboards` now builds (was the documented failure).

### Corrected root cause (supersedes "Why the walls miss" above)

The Step D conclusion that the remaining wall was serial .bzl evaluation
was wrong. It was **per-repository source hashing + deep copies**: every
warm run re-serialized and re-hashed the ~31 MB .bzl closure per
repository (Step A measured `spec_serialize=30.7–87s`; the digest form
still re-hashed per repo and cloned the closure-bearing Value). NOT bzl
serialization per se — the closure bytes themselves, hashed 2,058 times.
After this follow-up `spec_serialize=0.003s/2058`, total warm validation
~0.07s, and the coordinator is no longer the bottleneck.

### Before/after on this branch (warm, ~/code, default cache, t=32)

| workload | BEFORE (c78542b binary) | AFTER (97caa84+) |
|---|---|---|
| `query //src/ts:all` | 105.5s (spec_serialize 87.0s) | **10.9s** (spec_serialize 0.003s) |
| `query //src/...` (18,798 targets) | OOM-killed @309s under contention (Step D: 143.9–226.5s) | **55.0s**, exit 0 |
| warm build (41 genrule targets, 363 actions) | 80.9s class (Step D 68–77s) | **80.9s**, 363/363 cache hits, 0 executed |
| new trees on warm runs | — | **0** (12,388 stable everywhere) |
| stdout | md5 `eb4c8839…` (ts) | **byte-identical** t=1/8/32 + jitter8, ts and src (18,798 lines, md5 `820747e4…`) |
| `reused/executed` (ts) | 2056/2 | **2056/2** (direct pointers unchanged) |
| `files_hashed_during_verify` (ts) | — (counter new) | 6 |

## Memory ceiling (separate commit, `feat: BAZEL_RS_MAX_MEMORY_MB/--memory-limit ceiling with graceful watchdog`)

`--memory-limit=<MB>` (build/run/test/query) or `BAZEL_RS_MAX_MEMORY_MB`
(flag wins, Bazel's CLI-over-env) installs at startup, before heavy work:
RLIMIT_AS = 4x cap (floor 4 GB — virtual runs ~3–4x RSS; a 1x/2x rlimit
allocator-aborts at ~34s on //src/... while RSS is ~2 GB) via rustix
(warn-and-continue where RLIMIT_AS is unavailable), plus a watchdog
thread polling /proc/self/statm every 250 ms: >75% warns once; >cap
prints `bazel-rs: memory limit of MB exceeded (killed)` and exits 42.
Rust allocation failure aborts (handle_alloc_error) and cannot be made
graceful — the watchdog is the graceful path, the rlimit the backstop.
Caveat: exit(42) does not group-kill already-spawned children (the
sandbox worker inherits RLIMIT_AS across exec and dies on its next
allocation; a non-allocating child may be reparented). Unset = no cap,
no watchdog, unchanged behavior. Normal capped exits print
`bazel-rs: peak RSS (VmHWM) N MB (memory limit M MB)`.

Measured warm peaks (VmHWM, this branch): ts:all t32 **601 MB**;
`//src/...` t32 **3,423 MB**, t8 **3,426 MB**; warm build (363 actions)
**1,566 MB**. 6 GB and 8 GB caps both complete all three (exit 0,
byte-identical stdout). At `--memory-limit=2048`, //src/... warns at 75%
(1,569 MB) then is killed at **exit 42** (33.2s) with the exact message
above; ts:all under a 2048 cap completes (peak 601 MB).

## Perf follow-up 2 (oracle blockers on 97caa84/22cade7/a15efa8)

### 1. Provenance cache blowup — REMOVED

`PreparedOrdinary::framed_provenance` cached a full framed copy of the
group's ~31 MB provenance per canonical name, forever. Each canonical
name executes at most once per process, so the cache could never be
hit twice; a cold run executing ~2,058 ordinary repositories would
have held ~64 GB. `framed_provenance` now builds the bytes ON DEMAND
and returns them unretained (`materialize_lazy` already ran it only on
the execute path). The per-name spec-key cache stays (small strings).

Evidence that the executed-path peak is bounded (this host, 251 GB
RAM, default cap): cold `//src/ts:all` from an empty cache (cache dir
moved aside): exit 0, 75.7s, VmHWM 609 MB, md5 eb4c8839… Cold
`//src/...` (executes the repos ts misses): exit 0, 143.8s, VmHWM
3,430 MB, 3,858 trees published, md5 820747e4… — the same peak class
as warm (3,423 MB), NOT the ~64 GB the cache implied. The production
cache was then restored untouched (12,388 trees) and re-verified warm.

### 2+3. Memory ceiling — watchdog-only, default-capped, exit 33

a15efa8's RLIMIT_AS is DROPPED entirely (no setrlimit remains): an
address-space rlimit is inherited by every child, so a bazel-rs-sized
cap hard-breaks legitimate child actions (node/JVM/Go reserve far more
virtual space than they touch). Regression test
`watchdog_installs_no_rlimit_as` samples /proc/<pid>/limits of the
running binary and asserts `Max address space: unlimited` throughout.

The watchdog (250 ms /proc/self/statm poll) now: >75% warns once;
>cap sets the stop flag, SIGKILLs every live child process group
registered through `spawn_lock::spawn` (children started in their own
group: action spawns now `process_group(0)`, joining repository
execute(), git, credential helpers and test spawns; kill via the
existing `git_process::kill_group`), prints the killed line, and exits
**33** — Bazel's own OUT_OF_MEMORY (ExitCode.java:33) — not the old
private 42. Caps < 256 MB are rejected up front (`memory limit of
64 MB is below the minimum of 256 MB (bazel-rs's own startup footprint
exceeds it); raise the limit or unset it for the default of 50% of
machine memory`).

Unset flag+env now installs a DEFAULT cap of 50% of MemTotal, announced
once: `bazel-rs: memory limit defaulted to 128683 MB (50% of machine
memory); set BAZEL_RS_MAX_MEMORY_MB (recommended) or pass
--memory-limit=<MB> to change it; the cap bounds bazel-rs itself, not
child actions`. (The brief's "64 GB host -> 32 GB" assumed 64 GB; this
host actually has 251 GB RAM, so the default is 128,683 MB. The rule
is the same: half of MemTotal.) Set BAZEL_RS_MAX_MEMORY_MB to move it:
**the env var is the recommended place** — `--memory-limit` in a
shared `.bazelrc` also reaches genuine Bazel, which rejects the
unknown option and breaks. The cap bounds bazel-rs itself, not child
actions: a JVM/node action keeps the host's limits, and bazel-rs's RSS
does not include children's pages.

Measured with the fixed binary: `--memory-limit=2048` on warm
`//src/...`: 75% warning at 1,667 MB, killed line, **exit 33**.
`--memory-limit=64`: rejected, exit 1, message above. Unset on
`//src/ts:all`: default line + `peak RSS (VmHWM) 600 MB (memory limit
128683 MB)`, exit 0.

### 4. Prefix-binary evidence for //src/...

The surviving /tmp binary at the prefix turned out DISTRUSTED (it
reported ~69 GB and was OOM-killed at 137 both warm and guarded —
inconsistent with every other build; discarded, not evidence). c78542b
was rebuilt from source in the isolated scratch checkout
/tmp/opencode/perf-prefix-build (its .envrc forces an in-tree target,
noted): warm `//src/...` t=32 completes in 162.0s, sampled peak
3,438 MB, stdout BYTE-IDENTICAL to fu-src32.out (md5 820747e4…,
18,798 lines). Together with the fixed binary's byte-identical run,
the c78542b->HEAD output identity for //src/... is now proven directly.
`//src/...` t=1/8 outputs added (fixed binary): t=1, t=8, jitter8
(BAZEL_RS_LOADING_JITTER=25) all md5 820747e4….

### 5. Peak-memory bisect (warm //src/... t=32, same cache)

| binary | wall | peak RSS |
|---|---|---|
| f0f9c22 (rebuilt in scratch) | 394.2s | 3,439 MB |
| c78542b (rebuilt in scratch) | 162.0s | 3,438 MB |
| HEAD (fixed) | 56–62s | 3,423–3,439 MB |

**Attribution, honestly:** the "1.24 GB history vs 3.42 GB now" gap
does NOT reproduce between any of these three binaries on this cache —
all three peak at ~3.44 GB. The perf increments (f0f9c22->c78542b->HEAD)
changed TIME (394s -> 162s -> ~60s) by removing the 64 GB of spec
serialization, not the peak. The 3.4 GB peak is the retained
analysis/loading state for 18,798 targets (its plateau is already
reached by ~40s in, before repository execution dominates), and the
1.24 GB figure from Step-D-era notes was measured on the smaller
warm-build workload (41 targets; the fixed binary measures 1,396 MB
there), not on //src/... . First-after-upgrade behaviour: ordinary
pointers republish once (new pointer profile from 97caa84), direct
pointers unchanged (the warm ts run still reports its 600 MB peak and
byte-identical stdout; the reused/executed split is unchanged from the
97caa84 measurement);
cold peaks bounded as in §1.

### 6. Same-class audit (actions string-or-None)

`actions.symlink`'s `progress_message` rejected explicit None with a
hand-rolled message-divergent check — same class as the run/run_shell
fix. It now goes through the shared `string_or_none` (explicit None =
default "Creating symlink %{output}"; int fails with the unified
string-or-NoneType message). Test: `symlink_accepts_explicit_none_progress_message`.
`exec_group`/`toolchain` on run/run_shell already accept explicit None
(`group_arguments`; covered by tests/exec_groups.rs). `actions.write`'s
`mnemonic` is accepted-and-discarded by design (Bazel validates its
type; the native profile registers no mnemonic for write actions) —
not the rejecting-None class, left as is.

### Acceptance re-measure (fixed binary, warm, production cache)

| workload | result |
|---|---|
| `query //src/ts:all` t=32 | 8.3–11s, md5 eb4c8839…, peak 600 MB |
| `query //src/...` t=32 | 56–62s, md5 820747e4…, peak 3,423 MB |
| `query //src/...` t=1 / t=8 / jitter8 | all md5 820747e4… |
| build 41 targets (stepD-50 head) | 72s, 363/363 cache hits, 0 executed, md5 d89a7d7d… (== fu-build32.out), peak 1,396 MB |
| new trees on warm runs | 0 (12,388 stable) |

## Perf follow-up 3 (memory ceiling: the 68+ GB recurrence)

A bazel-rs process was again seen consuming **68+ GB** on this host
(251 GB RAM, many other services; killed by hand). Root causes, in
order of contribution:

### 1. The default cap was sized for the wrong host

56d0803's flat 50%-of-MemTotal default was picked under the brief's
assumed 64 GB host (→ 32 GB). This host has 251 GB, so the default
was **128,683 MB** — a runaway had ~128 GB of headroom before the
watchdog fired, on a host whose other services need the memory.

**New exact policy** (`memory_limit::default_cap_for_total_mb`,
sliding scale; 1 GB = 1024 MB, boundaries inclusive at the low end):

| MemTotal | default cap |
|---|---|
| >= 128 GB | **64 GB flat** (65,536 MB) |
| 32 GB – <128 GB | 50% of MemTotal |
| < 32 GB | 70% of MemTotal |

Rationale: no measured workload needs more than single-digit GB
(warm/cold peaks below are ≤ ~3.5 GB), so large shared hosts get an
absolute 64 GB ceiling — ~18× the largest measured peak, bounded
damage beside other services. Small hosts keep a larger share: the
binary's fixed footprint is a bigger fraction there and less else
competes. This host now announces, once:
`bazel-rs: memory limit defaulted to 65536 MB (64 GB flat cap for hosts with at least 128 GB of machine memory); set BAZEL_RS_MAX_MEMORY_MB (recommended) or pass --memory-limit=<MB> to change it; the cap bounds bazel-rs itself, not child actions`.
The announcement names the tier that produced the cap (50%/70%
wording on smaller hosts); the below-minimum error interpolates the
same policy text so the two cannot drift. Watchdog-only, no
RLIMIT_AS, exit 33, ≥256 MB floor: all unchanged.

### 2. Provenance retention — audited, already fixed, not regressed

The 64 GB class from follow-up 2 stays eliminated: spec identity is
digest-form (`specification_digest*` over once-per-group source
digests; per-repo frames are canonical name + attributes only),
`PreparedOrdinary::framed_provenance` builds bytes on demand and
returns them unretained, `materialize_lazy` invokes the provenance
closure only after the warm probe misses under the stage lock, and
the registry session's fetch memo holds `Arc<Vec<u8>>` per registry
file (shared, parsed-per-read — not per-repo copies). One stale
comment claiming framed bytes were "built once (cached)" was
corrected. Landed numbers (follow-up 2, unchanged): warm
`//src/ts:all` peak 600–609 MB; warm `//src/...` 3,423 MB; cold
`//src/...` 3,430 MB — re-measured for this follow-up below.

### 3. Per-command thread bounding

- `--jobs` (action execution): default `min(available CPUs, 32)` —
  the pool never exceeds 32; explicit `--jobs=N` is kept exactly.
  Action workers spawn child processes (their RSS is not bazel-rs's
  and is not capped) and hold only action records in-process.
- `--loading_phase_threads` (package loading): workers each own a
  Starlark heap on a 16 MiB stack. The **auto** expansion now respects
  the cap: `threads = min(auto, cap_mb / 64 MB per worker heap)`,
  floored at 2 (`memory_limit::bound_auto_loading_threads`). At this
  host's 64 GB default the bound is 1024 » 64 CPUs (no change); at a
  2048 MB cap it is 32; at the 256 MB minimum it is 4. An explicit
  `--loading_phase_threads=N` is the user's stated intent and is
  **never rewritten** (the watchdog still enforces the cap).
  QueryOptions now records `auto` as `None` instead of expanding at
  parse time, so query takes the same bound.

### 4. Cold-run acceptance

Cold `//src/...` (empty repository cache) must complete under the
cap with peak RSS ≤ cap + 5%. Measured with the follow-up-3 binary —
see the acceptance table at the end of this section (filled by the
re-acceptance run; the follow-up-2 cold number was 3,430 MB at the
old 128,683 MB default, i.e. the peak was never the problem — the
cap around it was).

Measured with the follow-up-3 release binary (this host, 251 GB /
MemTotal 257,366 MB, production cache `~/code/bazel-rs-out` unless
cold): the announcement on every run is the 65,536 MB line above.

| workload | wall | peak RSS (VmHWM sampled) | stdout |
|---|---|---|---|
| warm `query //src/ts:all` t=32 | 10.8s | 601 MB | md5 eb4c8839… |
| warm `query //src/...` t=32 | 56.3s | 3,425 MB | md5 820747e4… (18,798 lines) |
| warm `query //src/...` t=1 / t=8 / jitter8 | 49.8s / 51.3s / 52.6s | 3,429 / 3,440 / 3,440 MB | all md5 820747e4… |
| warm build 41 targets t=32 `--jobs=32` | 72.1s | 1,395 MB | 363/363 cache hits, 0 executed, md5 d89a7d7d… |
| warm `//src/...` auto threads, `--memory-limit=8192` | 52.6s | 3,442 MB | md5 820747e4… |
| warm `//src/...` `--memory-limit=2048` | 34.0s | killed (75% warning at 1,667 MB) | **exit 33**, killed line |
| **cold** `query //src/ts:all` (empty cache) | 78.4s | **607 MB** | md5 eb4c8839…, 2,058 trees published |
| **cold** `query //src/...` (same cold cache) | 144.7s | **3,434 MB** | md5 820747e4…, 3,858 trees |

Cold acceptance: cold `//src/...` peak **3,434 MB ≤ 65,536 MB cap +
5%** (5.2% of the cap) — the cold executed path stays in the warm
peak class, confirming per-repo provenance bytes are not retained
past materialization. New trees on warm runs: **0** (12,388 stable;
production cache restored untouched after the cold runs and
re-verified warm: ts 9.0s, 601 MB, md5 eb4c8839…). `--memory-limit=64`
is rejected with the sliding-scale policy text, exit 1.

Polish (integration-19): FOLLOW-UP 3 LANDED (sliding cap 64GB, bounded auto threads).

## i20 hardening — store manifests

### Forward-compatible readers

Every addition to the manifest (`Entry.symlink`, `spec`/`recorded`,
`Entry.stat`) used to make the binaries before it reject the newer
manifests (`deny_unknown_fields`), discard the valid publication and
republish it — re-executing the repository rule whenever two versions
shared a cache. Readers now tolerate unknown fields; writers are
unchanged. Per struct (`src/repository_store.rs`):

- `Manifest`: tolerant. Integrity rests on checks that stay strict: the
  new explicit schema gate `parse_manifest` (`format` = 1 and `profile`
  = `direct-file-template-v1`; a change to what an existing field means,
  or a field that must not be ignored, has to bump `format`), the
  required fields and their types, the identity the pointer names, the
  recorded inputs `validate` rechecks and the tree verification.
- `Entry`: tolerant. Reuse re-derives every known field (type, mode,
  size, digest, symlink target) from the tree on disk and identity /
  equality compare `content_view`s of the known content fields, so an
  unrecognized per-entry attribute cannot widen what is trusted.
- `StatFingerprint`: tolerant (it already was); a fingerprint only
  narrows trust (equal to the current stat, outside the racy window).
- Pointer (`read_pointer`): tolerant of unknown fields, strict
  otherwise (`format` = 1 and a 64-digit lowercase hex identity, which is
  joined into store paths). Only the identity is used, and the warm probe
  revalidates the manifest it names; rejecting an additive field would
  fail this binary's builds outright.
- A manifest this binary did not write byte-for-byte (a newer binary's,
  with fields this one does not know) is reused but never rewritten by
  a fingerprint refresh, which would strip those fields.

Test: `unknown_manifest_fields_are_reused_not_republished` (newer
top-level, entry and fingerprint fields plus a newer pointer field:
reused with the bytes untouched, also through an identical re-execution;
another `format`/`profile` is republished as this binary's). With
`deny_unknown_fields` restored on `Entry` the test fails with the
re-execution it guards against.

### Safe warm refresh

`try_reuse` was documented as "pure reads only — safe on worker
threads", yet a fallback hash match rewrites the manifest to add stat
fingerprints, without the repository lock: a probe could rename a
manifest derived from the bytes it read over a manifest a publisher (or
another probe) had just written. Not a correctness hole — identity fixes
content, and a stale fingerprint only mismatches and re-hashes — but a
lost update and an untrue contract. `refresh_manifest` is now:

- under the publication's lock `locks/<identity>` (held by publishers
  while they replace the manifest), only *tried*: a busy lock skips the
  refresh, so no worker thread ever blocks, and the lock is never
  created by a probe;
- a compare-and-swap: the manifest is rewritten only if it still holds
  exactly the bytes the probe verified the tree against, so a newer
  publication or another probe's refresh wins and repeating a refresh
  changes nothing;
- canonical-only (above) and a temp file renamed over the manifest, so
  readers see the old or the new manifest, never a partial one.

The `try_reuse` doc now states the one best-effort write. Tests:
`concurrent_warm_probes_refresh_the_manifest_safely` (16 threads probe a
settled publication at once: all reuse, the manifest ends canonical with
the settled fingerprint and intact content, no staged refresh left, a
later probe rewrites nothing) and
`manifest_refresh_is_a_locked_compare_and_swap` (skipped while the lock
is held — the test thread itself holds it, so a blocking refresh would
hang; skipped when the manifest changed since it was read; applied once
otherwise). Mutating the lock or the byte-exact compare back to the old
identity+content guard fails the latter.
