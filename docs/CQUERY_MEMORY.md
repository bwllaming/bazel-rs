# Configured-query retention and export

## Default-universe headroom follow-up

The completed default `deps(//...)` graph at `40ef505` is the equal-work baseline
for this follow-up, with ELF SHA256
`0591e3613ee7b78a8be18c512d0f543ad767ed879a824f2bdda10e9a0d084cbd`.
The earlier failed 7,624 MiB analysis did less work and is not a baseline.

One diagnostic pass of that exact ELF used the existing `BAZEL_RS_TIMING` and
`BAZEL_RS_TIMING_ALLOC` counters. The retained plan owned about **872.6 MB**,
beside **739.4 MB** of package rules, **985.4 MB** of frozen provider heaps,
**394.4 MB** of dependency views and **310.1 MB** of configured nodes. These are
allocation-census buckets, not independent RSS peaks. There were 200,845 planned
rules, 57,326 loaded rules and zero lowered action nodes. Provider analysis was
still the peak phase; engine release and export reduced memory. Diagnostic hooks
conservatively bypass snapshots, so this pass's 235.24 s / 7,513.8 MiB is not
substituted for the known plain analysis/save measurement.

`configured_attributes::configure` now shares the loaded rule's `Arc` when
configuration leaves its data unchanged. Selects are still resolved and validated
in their original order. Changed native, custom and built-in attributes produce
a private copy; late-bound defaults copy only when their values change. The
engine likewise copies before changing an alias's actual, split dependencies or
a build-setting value. Configured target keys, provider instances and analysis
remain separate even when immutable rule data is shared. Tests pin sharing and
isolation across selected configurations and late-bound null/label defaults.

The final release is based on `c09c0cb` (only shared-action fixtures/docs changed
since `40ef505`) and has SHA256
`791b09ddaf79d6899ec72af485b40407fc0d949d97bc304fe0ddb46d6b121a70`.
The same read-only PR clone at `0b29c533c060e5ccb9466bbd200d2b0510be01da`,
existing private content cache and 8,192 MiB / 900 s limits were used. Fresh result
directories and the new binary identity yielded a confirmed miss, full analysis
and save, followed by one deps hit and one shared-universe roots hit:

| Query | Baseline seconds / peak MiB | Candidate seconds / peak MiB |
|---|---:|---:|
| `deps(//...)`, analysis + save | 218.201 / 8,033.4 | 215.044 / 7,460.5 |
| `deps(//...)`, graph hit | 2.936 / 1,633.6 | 2.297 / 1,636.9 |
| `//...`, shared-graph hit | 2.445 / 1,634.5 | 1.816 / 1,636.7 |

Peak falls **572.8 MiB**, leaving **731.5 MiB** headroom (previously 158.6 MiB).
This meets the >=512 MiB headroom goal; <=6 GiB is not reached. Warm RSS is
3.4 MiB / 2.2 MiB higher in these individual observations. Timings include fresh
process startup; they are single shared-host measurements, not statistical
speedup or broader startup claims.

All three outputs compare byte-for-byte to their baseline counterparts. Deps is
481,114,922 bytes, SHA256
`ba4760e23df4d3a48e3dd0d9b2de3e02b82a5b1ee957bffb8733aa9036f58cbc`;
roots is 139,683,706 bytes. Official-schema streaming decode again validates
265,121 configured results, 1,116,527 edges, 34 configurations and 45,227 roots
with identical configured records. Every edge endpoint and configuration checksum
is valid, including the ARM64 host-incompatible constraint-only inputs.

Compatibility causes remain compact, the engine is freed before encoding, and
binary snapshot serialization remains bounded. No analysis/provider/attribute
checks, ordinary prerequisites or graph records are omitted. Clone canaries are
unchanged. Whole-repository TD remains unvalidated.

Receipts and allocation output:
`/home/coder/opencodetmp/cquery-memory-headroom/{baseline-profile,pr-full,pr-full-warm,pr-roots}`;
official decode: `application-decoding.json`; gate details: `REPORT.md`.
The single full suite reported 3,239 passed / 1 failed / 8 ignored: its source
discipline test caught an unnecessary test-helper lint allowance. Removing that
allowance passed strict Clippy, all 1,811 lib tests and both discipline tests.
The full suite was not repeated; all its semantic programs, including snapshot
verification, passed on their first run. The release was built once after this
correction. The focused pre-commit gate passed 190 integration tests as well.

## Earlier full-source retention and export work

The full-source uncached query's fragile ~8 GiB peak was profiled **before**
choosing the optimization. A retained allocation run and a separate phase-only
snapshot run showed:

* Peak is late provider analysis (8,196,468 KiB in the phase-only run), not
  snapshot encoding. No validation, provider or required graph branch can be
  omitted to fix it.
* The capture owned 1,635,803 ordinary edges: about 768 MB of copied endpoint
  labels / attribute strings, excluding allocator overhead. Existing sparse
  aspect capture already interned its keys; ordinary capture did not.
* Cquery already returns before the build-only `skip_teardown` path and drops
  ActionPlanning. The CLI did retain Engine until process exit, through export.
* The all-at-once postcard buffer added about 370 MiB during encoding, but that
  phase occurred after RSS had fallen below the analysis peak.

## Changes

Ordinary and aspect capture share one configured-key interner and interned
attribute names. Every edge retains its order, endpoints and flags. Neither
ConfigId nor the pre/post incoming-transition key is changed. The final graph
still assigns IDs in the original rule / edge / aspect discovery order.

Export resolves each distinct captured key once, not repeatedly per edge, then
uses a compact ID mapping. Construction-only lookup maps are released first.
After export, the CLI explicitly drops its dead Engine before snapshot encoding;
library callers still own their Engine and can issue subsequent requests.

Postcard is serialized directly through a bounded 4 MiB frame buffer into the
existing compressed PLAN framing. There is no graph-sized uncompressed Vec,
JSON intermediate, changed reader, weaker validator or graph copy. Unit tests
compare complete compressed bytes with the original encoder across frame
boundaries. Warm decoding and output are unchanged.

All semantic rule/provider/attribute/File/alias/owner checks remain active. Build
and --nobuild behavior, action keys, concurrency defaults and configuration
identities are unchanged. Instrumentation uses the existing performance markers;
the temporary snapshot-profiling environment exception was removed.

The diagnostic optimized run retained byte-identical full-source output and
reduced peak to 7,728,752 KiB. Export took 8.89 s instead of 23.06 s in the two
phase runs; save encoding no longer adds the 370 MiB buffer. Plain final-binary
measurements, using the same private cache and 8192 MiB / 900 s bounds:

| query | baseline | optimized | peak RSS baseline / optimized |
|---|---:|---:|---:|
| `deps(//src/...)`, analysis + save | 144.74 s | 139.17 s | 8,001 / 7,549 MiB |
| `deps(//src/...)`, hit | 2.21 s | 2.23 s | 1,514 / 1,517 MiB |
| `//src/...`, shared-universe hit | 1.76 s | 1.68 s | 1,514 / 1,515 MiB |

The baseline save is the preceding exact-ELF receipt; baseline hits were repeated
in this series. Shared-machine timings are individual observations, not a claimed
statistical speedup. All deps output is byte identical (464,264,083 bytes,
SHA256 `51c095b091244146ba10c86fbd2bd5ae099864a6f93b3d1f551cbd4d0c7429ad`).
Roots output is also byte identical (139,599,392 bytes). Headroom grows from about
191 to 643 MiB, a 452 MiB reduction. The <=6 GiB aspiration is **not met** by this
scoped reduction; provider analysis is still the peak.

## Actual default TD universe: blocked separately

Both `deps(//...)` and `//...` fail loading
`venv/lib/python3.12/site-packages/pandas/BUILD.bazel:1:1`:
`name 'py_library' is not defined (did you mean 'cc_library'?)`.
The calls exit 1 after 9.01 s / 6.12 s, at 875 / 877 MiB, publishing no output.
No package is excluded and no unconfigured fallback is substituted. The default
universe is therefore **not supported yet**, despite full-source success.

Scratch real TD v0.34 comparisons remain exact against genuine: original 11/11,
split 11/11, shared-output 2/2 and aspect 9/9. Snapshot tests cover hit, verification,
format/query reuse and BUILD/.bzl/environment/source-presence invalidation. An
independent upstream-schema streaming decode validates 261,196 targets, 31
configurations and 1,104,178 configured inputs. Ordinary scratch build output and
action keys/dumps remain identical to the baseline.

Native full-graph query success is **not** a whole-repository target-determinator
before/after revision comparison. Actual TD comparisons are confined to scratch
repositories; no revision mutation or TD is run on the read-only monorepo.
