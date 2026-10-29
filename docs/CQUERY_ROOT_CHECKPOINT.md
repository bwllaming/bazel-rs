# Experimental serial registered-root cquery checkpoint

**Separate opt-in, default OFF:** `BAZEL_RS_CQUERY_ROOT_CHECKPOINT=1` or `on`
(case-insensitive). Unset, empty, `0`, `off` disable it. Other/non-UTF-8 values
are cquery command-line errors before repository/package demand. Other commands
ignore the control. Enabling both this profile and
`BAZEL_RS_CQUERY_CURRENT_LOADING` is an error: existing `1/on` still means the
unchanged pattern-only checkpoint described in [CQUERY_CURRENT_LOADING.md](CQUERY_CURRENT_LOADING.md).
Neither profile is an rc option or a Bazel parity flag. With both off, default
cquery still returns warm history before current package loading.

## Current-demand boundary

This profile performs ordinary option/pattern loading and enters configured
analysis **without a pattern lookup**. It installs one request-local marker:
only after an actual `compute_registered_toolchains` succeeds, its computing
frame is popped and its result is memoized, remember that first successful
collection's actual `ConfigId`. A memo hit is not a collection or marker.

At the first subsequently successful requested-root phase1 completion, after
`analyze`, `check_requested_compatibility`, normal error handling and restoration
of root-analysis tracking, requested-trimmed-root and keep-going current-root
state, consume the marker. This happens before advancing to the next root or
analyzing top-level aspects. Preserve the engine's existing selected-label order
(which may differ from textual pattern order). A failure before that completion
wins normally; there is no mid-stack yield or error-string sentinel. No-types or
direct-incompatible roots do not cause extra registration demand.

All existing demand gates remain: options, deprecations and explicit platforms;
rule transitions, config conditions, direct compatibility; toolchain types,
execution groups/platforms/constraints; and the nonempty resolved-type gate.
Registrations use the current configuration's reversed `extra_toolchains`, then
current MODULE order. An invalid registered declaration still fails when normal
collection reaches it. Unselected implementation labels remain NODEP and do not
load an otherwise unused/failing repository. No saved name, count, inventory or
coverage list decides which work to do or where to cut.

## One live continuation

An inline callback runs with Plan, ConfiguredNodes, the root loop, configurations,
delegation/edge/exec/toolchain memos, ActionPlanning (including early providers),
partial cquery capture, observations, RepositorySet and local-result ledger still
live. It sees a newly sealed, lookup-only `CurrentRefresh` and `RootCut`, not a
repository-demand API or the old pattern seal.

Typed `Continue` resumes the same loop and phase2. It never calls `plan_build` or
`cquery_graph_loaded` again. A miss can therefore keep legitimate current effects
without repeating local callbacks, early `plan_pending` providers, or transition
print applications. The transition cache's normal per-application print replay is
unchanged. `AcceptedQuery` returns the fully validated saved graph through only
the shallow planning/cquery wrappers; partial analysis/capture is dropped instead
of exported. Ordinary `plan_build` is a no-hook wrapper.

There is only **one opportunity**. The first completed-root marker is consumed
before admission or candidate probing. A budget/coverage/ownership miss cannot
try another root in search of more historical local coverage. If no registration
collection succeeds, no lookup is attempted and no root-profile snapshot is saved.

## Serial and bounded overlap admission

The initial slice requires the existing loading thread count to be exactly1,
no engine prefetch, no admitting/live speculative work (including late listeners)
and an already-quiescent root warmer. The event-series try-lock checks test the
speculative producer before its root-warmer consumer. No settings are rewritten
and no workers are stopped, restarted, waited for or drained to force this cut.
Parallel/busy cases decline and continue ordinary analysis.

History-independent admission limits are:

| Native control | Default and maximum |
|---|---:|
| `BAZEL_RS_CQUERY_ROOT_MAX_MS` |30000ms since entering configured-prefix work |
| `BAZEL_RS_CQUERY_ROOT_MAX_NODES` |50000 combined ConfiguredNodes + Plan entries |
| `BAZEL_RS_CQUERY_ROOT_MAX_RSS_MB` |1024MiB current process RSS at the cut |

When root mode is enabled these controls accept unsigned integers from zero to
the respective ceiling. Zero declines overlap; values cannot raise the ceiling.
Invalid values fail before current demand. They affect only lookup admission, not
semantic inputs or namespace selection. Unknown current RSS also declines.

With `BAZEL_RS_SNAPSHOT_EXPLAIN`, a budget refusal retains the exact existing
prefix and appends measurements, for example:

```text
bazel-rs-cquery-root: declined: current work exceeds lookup admission budget; first_failed=elapsed_exceeded all_supported_failed=[elapsed_exceeded,nodes_exceeded,rss_exceeded] elapsed_ns=30000000001 max_ms=30000 nodes=50001 max_nodes=50000 rss_bytes=1073741825 max_rss_bytes=1073741824
```

`first_failed` preserves the original short-circuit predicate order:
`max_ms_zero`, `max_nodes_zero`, `max_rss_bytes_zero`, `elapsed_exceeded`,
`nodes_exceeded`, then `rss_unknown` or `rss_exceeded`.
`all_supported_failed` lists every failing criterion supported by the retained
values in that same order. Thus a zero node limit and positive actual nodes can
report both `max_nodes_zero` and `nodes_exceeded`. No unmeasured elapsed failure
is inferred. Unknown RSS is distinct from a measured value above its ceiling.

The existing single RSS sample is taken before budget evaluation, including
when a zero control refuses admission. If all three controls are positive,
elapsed is captured once at the original comparison point, after that RSS read.
The same full `Duration` decides admission and supplies integer `elapsed_ns`;
there is no millisecond rounding. Zero controls still skip the clock and report
`elapsed_ns=not_evaluated`. `rss_bytes=unknown` means the RSS sample was unavailable.
Effective limits are always reported as `max_ms`, `max_nodes`, `max_rss_bytes`;
`nodes` remains the actual combined ConfiguredNodes + Plan count.

Formatting happens only after refusal under the existing explain switch, through
`snapshot::diag` outside captured EVENTS. Earlier serial/quiescence and queued-warning
guards still win, with their unchanged reasons and no budget measurements. Success
tracing is unchanged. These diagnostics do not change admission, extend its timer
to pattern loading, or reconstruct measurements missing from older failed runs.

**These bounds do not limit the cost of completing a root.** One root can contain
most of the graph. The engine finishes whatever that root normally needs, checks
admission at the fixed cut, and continues normally on decline. It does not discard
fallback state and recompute it to hide memory cost. The limits are not evidence
of application-scale memory safety or the aggregate8GiB/900s target.

## Diagnostic and key identity

The capture window begins at the same pre-RepositorySet point as pattern mode.
Entering configured-prefix work keeps that prefix capture open. Committing the
successful cut closes/drains it with the existing bounded Tee semantics. Saved
prefix bytes must equal freshly emitted current prefix bytes, including prints
from rule transitions and early provider callbacks. A miss opens the suffix capture
and continues; an accepted hit emits only the saved suffix after all current
source/local/nonlocal/graph/adoption checks. Default and pattern-mode behavior is
unchanged. `off` disables history; `verify` runs this same continuation and compares
graph and suffix, without replaying cached diagnostics.

Pending engine warnings at the cut **decline admission**. They are never flushed
early or reclassified. This supports a typed first root followed by later-root
warnings in the saved suffix; it conservatively declines a first root that already
queued warnings. Incomplete/overflowed captures, unknown ownership and dynamic or
untracked inputs retain the event-series fail-closed behavior. Quiet, color and
protobuf routing still use the current UI. Native outer final hit/miss markers
occur only on the attempted lookup; a declined cut reports a separate
`bazel-rs-cquery-root: declined` diagnostic when snapshot explanation is enabled.
The successful-cut trace is also outside EVENTS through `snapshot::diag`.

The separate key is `cquery-graph-registered-root-v1`. Its EVENTS body starts with
a48-byte root header: `BZCQRC01` (the first-successful-registration-root cut kind),
zero-based root ordinal as little-endian u64, then the actual first registration
ConfigId's32 digest bytes. It wraps the existing `BZCQEV01` prefix/suffix frame.
Every read checks the current cut's exact kind/ordinal/configuration, length and
combined8MiB section bound before any saved diagnostic is emitted. Pattern-only
and flat default frames do not alias this namespace. ELF/key checks are retained;
INPUTS, action keys, ConfigIds, source/current-result validators and guarded view
adoption are unchanged.

## Evidence and remaining scope

`tests/cquery_root_checkpoint.rs` covers current local registration/cwd freshness,
unselected failing NODEP repositories, no-types/direct-incompatible roots, earlier
semantic errors, earlier no-resolution roots, named groups/exec/rule-transition
configurations, early provider/transition traces, absent/corrupt/late-invalid
history, one-shot coverage misses, budget/parallel/queued-warning refusal,
A/B history, current env/source/MODULE changes, root-frame corruption and
older-candidate selection, profile separation, quiet/color/proto/verify and no
actions/status execution. Existing event and default test files are unchanged.

This is a serial engine seam and small-fixture proof, not an application retry.
It does not establish coverage of the saved45 locals or current application
requirements. No inventory sweep or unconditional registration collection was
added. Full-application cut cost and memory overlap remain unmeasured; application
priming, large queries and TD require a separate decision.
