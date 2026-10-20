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
