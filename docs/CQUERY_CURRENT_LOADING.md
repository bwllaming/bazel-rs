# Experimental current loading before configured-graph lookup

**Default off.** Explicitly set `BAZEL_RS_CQUERY_CURRENT_LOADING=1` (or `on`)
to select this native experiment. Unset, empty, `0` and `off` select the installed
default early cquery history lookup, ordinary local replay and saved diagnostic
replay, with no package-loading prefix before a default warm hit. `on`/`off` are
case-insensitive. Any other value, including non-UTF-8, is a cquery command-line
error before repository/package demand. Other commands ignore this control.

This is not a Bazel flag and is not enabled by user/workspace rc files. It uses a
separate cquery analysis-key discriminator, without changing ConfigIds or action
keys. Default and experimental history cannot alias. `BAZEL_RS_SNAPSHOT=off`
still disables lookup/save, and `verify` retains its normal comparison semantics
inside the selected profile. Once the experiment starts, a miss resumes current
loading; it never switches to legacy replay and repeats completed callbacks.

With the opt-in, cquery history lookup follows the command's existing target-pattern selection
and expansion, before `plan_build`. Option validation, module/registry setup,
Starlark option loading and their errors keep their ordinary order. Observation
and diagnostic recording begins before constructing the current RepositorySet.

The loading prefix is run regardless of which history files exist. It is not
driven by ReplayRecord, PinnedTables, old graph nodes or MODULE registrations.
Removing or corrupting history therefore cannot change which local callbacks the
prefix requires. A miss resumes the **same engine, repository set and selected
patterns**; completed evaluations and failures are not repeated. Normal current
public effects persist on a miss. This is not a local replay transaction and
does not claim rollback or private execution isolation.

## Current result certificates

A successful normal local materialization records a compact invocation-owned
result: actual evaluator-demand witness, canonical identity, current spec digest
(which binds the recipe/provenance), definition identity, full publication root
and identity, both execution-input and tree digests, actual slot class and output
status. Constructors are private and these results are never serialized authority.
Execution uses the ordinary real Context.stage/cwd, current environment and tools;
there is no root rewriting, facility whitelist or skipped local freshness.

After successful ordinary loading, `CurrentRefresh` seals a lookup-only view of
these completed locals and the prefix observations. Candidate validation requires
exact local-record coverage (extra current or historical locals miss), matching
full identities and both digests, fresh Ordinary results, and matching verified
current views. Self-referencing/mutable locals continue normally but do not
qualify for this history path. Current nonlocal roots already loaded constrain
adoption: a conflicting historical publication cannot be installed behind the
retained continuation.

The read-only producer prover may consume these sealed current local certificates
after rechecking current recipe selection. It cannot execute a local callback.
Unmatched locals are misses, even on a candidate whose pointer observations match.
The experimental path never calls `replay_records`. Default cquery and
build/test/run snapshot replay are unchanged. Default commands do not collect the
experimental completed-local ledger or seal prefix observations.

## Acceptance and output

All ordinary observations, local results, nonlocal proofs, EVENTS, graph decoding
and indices are validated before adoption. Existing nonblocking guards acquire
all current-local and adopted-nonlocal slots in one order, then publication
identity locks. Current prefix observations, completed local integrity/views,
loaded-root constraints and nonlocal proofs are rechecked under these guards.
Only eligible nonlocal pointers/views are installed; local current views are
never rebound to historical versions. Errors after installation begins remain
terminal command errors, not candidate misses.

The current profile records **two explicitly owned event segments**: ordinary
loading prefix, then configured-analysis continuation. Current prefix diagnostics
are always emitted by normal loading, once. A candidate must have a byte-identical
recorded prefix (including UI note markers); only its analysis suffix is replayed,
after all validation and guarded adoption succeeds. Deterministic nonempty MODULE,
`.bzl` and BUILD prints are supported, as are analysis prints and warnings. An empty
current prefix is insufficient by itself: the recorded prefix must also be empty.
Mismatch resumes the same continuation; it never replays an old prefix.

The phase cut leaves queued warnings alone. It requires no pending engine warnings
or prefetch, no live/admitting speculative pool (including late listeners after a
bounded stop), and an already-quiescent root warmer. The speculative producer is
checked before the warmer it can enqueue. These checks are nonblocking and never
drain unneeded repositories to obtain a hit. Captures use the existing bounded Tee
semantics: restore stderr, wait at most two seconds for EOF/drain, and require a
complete, nonoverflowed, non-early result. A partial line is flushed before the
next phase. An inherited lingering writer or unknown ownership makes the cut
ineligible; its current output continues to be forwarded, never saved/replayed.

Lookup has a separate transient capture. Private producer proof notes are
suppressed using the existing `PrivateNotes` scope; only this profile's explicit
prefix agreement permits diagnostic-producing proof re-evaluation. Proof is not
ordinary loading and emits no command output. All proof observations, errors,
taints, source identities and publication guards still apply. Unexpected lookup
output prevents saving; if adoption already succeeded it is a terminal acceptance
error, never a partial-commit fallback or saved-suffix replay.

Saving requires complete prefix/suffix capture and final worker quiescence.
Overflow/incomplete capture continues ordinary output but saves no candidate.
`verify` runs the continuation and compares both graph and actual suffix bytes;
it emits only fresh output, never the saved suffix. A mismatch invalidates the
candidate. The current UI applies quiet filtering to the replayed marked suffix,
with the ordinary color policy and byte-protobuf stdout routing. Native outer
`bazel-rs-cquery-snapshot: hit`, final `miss`, and `saved` markers remain outside
captured EVENTS through `snapshot::diag`; no line-prefix stripping is used.

Only the experimental key changes to `cquery-graph-current-loading-v2`. Within
its ordinary EVENTS byte-string section, `BZCQEV01` plus two little-endian u64
lengths frames prefix then suffix, with exact exhaustion and a combined payload
limit of 8 MiB minus 32 bytes (including space for framing/outer codec). Legacy,
truncated, trailing or oversized frames miss before diagnostic emission. The
default remains `cquery-graph-v2` and its flat event codec/early replay is unchanged.
Existing ELF/key checks still apply. Certificates are ephemeral and persisted
ReplayRecord remains comparison evidence only. INPUTS stays format 5; ConfigIds,
action keys, typed selection guards, incremental A/B/CARRIED semantics and
mutable-slot detection are unchanged.

## Enabled slice and remaining application boundary

The mixed fixture loads a generated `.bzl` from a local repository during current
BUILD loading; a separate watched-input nonlocal dependency is first needed in
configured analysis. A/B/A/B histories hit with local execution once per command,
nonlocal execution only for two primes, and correct graphs/pointers/native views.
The local callback executes a real subprocess and checks its real cwd. Tests also
cover local producer definitions for nonlocal rules, environment/PATH/tool changes,
missing/corrupt history, unused failing declarations, extra historical locals,
local dependency cycles, cancellation, mutable locals and exactly-once output.

This prefix does **not** establish neededness for all 45 saved application locals.
In particular registered toolchains still load in configured analysis only after
the nonempty resolved-toolchain-type gate. No registration/history sweep was added.
Nonempty EVENTS alone is no longer a rejection, but old unframed snapshots are
incompatible and no application run is established by these small fixtures.
Pending warnings, dynamic/untracked inputs, output-producing local callbacks,
unknown async ownership and non-equivalent prefixes remain conservative misses.
Full-application prefix cost and memory overlap with graph decoding are
unmeasured; one continuation/candidate is retained, but loading need not be cheap.
The application/large TD gate remains halted pending further implementation and
a separately authorized measurement.

The separate [registered-root checkpoint](CQUERY_ROOT_CHECKPOINT.md) has its own
explicit opt-in, key and cut identity. This pattern-only profile keeps its
target-loading boundary and never advances to a second lookup during analysis.
