# Configured-query retention and export

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
measurements and the separate actual TD default universe (`//...`) acceptance
are recorded in the worker report. The <=6 GiB aspiration is not established by
this scoped reduction; provider analysis is still the peak.

Native full-graph query success is **not** a whole-repository target-determinator
before/after revision comparison. Actual TD comparisons are confined to scratch
repositories; no revision mutation or TD is run on the read-only monorepo.
