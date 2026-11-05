# Analysis on a snapshot miss (cold-analysis-speed)

What a full analysis costs when no analysis snapshot serves the command
(the first command of a target set, `BAZEL_RS_SNAPSHOT=off`, the plannings
of `affected`), with the repositories already fetched: loading the packages,
configured-target analysis (phase 1), action planning (phase 2) and the
planning tail. Fetching and module extensions are not covered here.

## Measurements

Private clone of ~/code at `80d6724f3a`, repositories warm (fetched by a
first run into the same `--cache-dir`), `BAZEL_RS_SNAPSHOT=off`, storagebox
overrides, `nice -n 10`, 64-core shared host. Genuine Bazel 9.3.1:
`--batch` (a fresh server, so a cold analysis), its own output base with the
repositories fetched by a first run.

| command | 0.4.10 | this revision | genuine 9.3.1 |
|---|---|---|---|
| `build --nobuild //src/...` wall | 176.4 / 176.1 s | **78.2 / 78.6 s** (2.25×) | 10.7 s |
| — CPU user + sys | 182.5 + 30.4 s | 83.9 + 35.2 s | 278.8 + 21.9 s |
| — peak RSS | 8,454 / 8,479 MB | 8,499 / 8,493 MB | 8,365 MB |
| — `plan.phase1_analysis_done` mark | 147.0 s | 58.8 s | – |
| `test --nobuild //src/...` wall | 185.4 s | **79.2 s** (2.34×) | 10.2 s |
| — CPU user + sys, peak RSS | 183.5 + 38.7 s, 8,492 MB | 84.4 + 37.5 s, 8,526 MB | 279.5 + 15.5 s, 7,818 MB |
| CI step `affected --base=HEAD^` (802 targets in scope, both revisions at once) | 178.6 s | **81.2 s** (2.20×) | – |
| — CPU user + sys, peak RSS | 334.4 + 180.1 s, 6,887 MB | 149.5 + 137.4 s, 7,246 MB | – |
| `affected --noconcurrent_revisions` | 323.1 s | 136.8 s | – |

Peak RSS with jemalloc's decay off (`_RJEM_MALLOC_CONF=dirty_decay_ms:0,muzzy_decay_ms:0`,
the live peak): `build` 8,473 → 8,498 MB (the memo tables below, ~10 MB, and
run-to-run noise of ±25 MB), concurrent `affected` 6,899 → 6,907 MB. With the
default 10 s decay the concurrent `affected` peaks 5% higher: the same work
in half the time leaves pages freed by one thread's arena undecayed when
another thread allocates (docs/MEMORY.md, "The allocator").

Genuine is CPU-bound too (279 s of CPU) but spreads it over 64 cores;
bazel-rs analyzes on one coordinator thread, busy for the whole wall time
(sampled: 104.7 s user + 6.9 s sys of a 112.7 s run), so the remaining lever
is parallel analysis (below).

Outputs: the `BAZEL_RS_DUMP_ACTIONS` dump of `build --nobuild //src/...`
and `test --nobuild //src/...` (6,483,434 lines each) is byte-identical to
0.4.10's, so is the rest of stderr; `aquery --output=summary` of
`//src/python/github_backup/...` and `//src/python/bworker/...` (PyCompile
included); `affected`'s output and both `--keep_digests` digest files.

## Where the time went (0.4.10)

A CPU profile (pprof, phase 1: 128 s of samples) of `build --nobuild
//src/...`: `py_library` targets were 58% of the planned targets' time
(compute 33 s, commit 57 s). rules_python's `actions_run` adds the exec
runtime's files (a 1,657-file depset) to every PyCompile action's inputs, and
the precompiler's interpreter is also the action's executable. Per action:

- registration (compute): the inputs depset flattened and deduplicated
  (hashing every File's three strings), every path validated twice over
  (`validate_relative`, and the namespace overlap check hashing every proper
  ancestor of every path), a SHA-256 key over every input;
- commit: every input resolved through the build's `BTreeMap<File,
  Artifact>`, validated again (the shared-group check), deduplicated by
  execution path, and — because the interpreter is listed twice — its
  source paths listed (one `PathBuf` per input) and compared with the
  planned ones.

## What changed

**Memoized depset walks** (`Depset::flatten_files_with_memo`,
crate::starlark_types). A node of Files whose walk has at least 128 files and
is requested a second time keeps its walk (its files in walk order and their
set) in a process-wide table keyed by the node's address, holding a weak
reference so the address stays the node's; the node records the state in one
byte of its padding. A walk that reaches a memoized node first — down the
first-visited child of each node above it, with nothing seen yet — starts
from its files, then walks the rest exactly as NestedSet.toList does, also
deduplicating against the memoized set. `ctx.actions.run`'s inputs carry the
walk they begin with (`RegisteredAction::input_memo`).

**Shared input checks.** The registration validates an action's own paths
against the walk's namespace summary (`action::InputNamespace`: hashes and
indexes of the paths and their ancestors, no copies), and the commit resolves
the walk's artifacts once per build (`SharedInputs`), with their namespace,
their own execution-path occurrences and their runfiles-group roots. Every
fast path declines on any doubt (a write, a transform, a Java compile, a hash
collision, an earlier own input at a shared path with another real path, any
check that would fail): the full path then runs and reports exactly what it
reported before. Debug builds (the tests) check every accepted fast path
against the full one.

**Smaller items.**

- A later occurrence of a source at its first occurrence's real path (the
  interpreter, executable and runtime file) no longer forces listing and
  comparing the action's source paths.
- Action-sharing keys (in-process only): one buffered BLAKE3 digest of the
  framed encoding instead of streamed SHA-256.
- Depset deduplication hashes a File's path only (equal Files have equal
  paths).
- `Engine::target` memoizes rule lookups (hash of the label, checked against
  the rule's label); `flush_delegations` rescans the rule delegations only
  when new ones were recorded.
- The planning tail looks missing sources and source directories up by hash
  (crate::path_key) and skips an action none of whose source paths names one;
  hidden runfiles look their target's runfiles tree up by owner index.
- Runfiles checks hash paths with crate::path_key; the repository mapping
  manifest parses each runfiles owner once.

`BAZEL_RS_TIMING=1` prints `PERF-MEMO walks=… walk_files=… shared_inputs=…
shared_artifacts=…` (`//src/...`: 162 walks of 112,421 files, 32 shared
input runs of 59,698 artifacts).

## Rule implementations on analysis workers (P3)

Design and determinism: docs/PARALLEL_ANALYSIS.md, "P3". In short: the
coordinator still analyzes (phase 1), prepares and commits every target in
plan order; a Starlark rule whose dependencies are committed is prepared
ahead of its turn and its implementation — with the pure part of its
runfiles support — runs on one of 8 workers; everything order-observable it
would have written is applied at its turn. Beside it, the build's
File → Artifact index (looked up per input, never iterated) is an
`FxHashMap` instead of a `BTreeMap`.

Same setup as above (private clone at `80d6724f3a`, repositories warm,
`BAZEL_RS_SNAPSHOT=off`, `nice -n 10`, shared 64-core host; two runs each
for `build`). Before: main `c92f6c08`; after: this branch.

| command | before | after |
|---|---|---|
| `build --nobuild //src/...` wall | 82.0 / 79.7 s | **72.1 / 69.9 s** |
| — CPU user + sys | 82.4 + 28.2 / 83.2 + 33.7 s | 90.4 + 32.9 / 87.0 + 31.5 s |
| — peak RSS | 8,478 / 8,489 MB | 8,813 / 8,825 MB (+4%) |
| — `plan.phase1_analysis_done` | 60.6 s | 55.3 s |
| `test --nobuild //src/...` wall | 80.5 s | **70.9 s** |
| — CPU user + sys, peak RSS | 83.4 + 35.4 s, 8,513 MB | 88.4 + 31.8 s, 8,778 MB |
| CI `affected --base=HEAD^` wall | 82.2 s | **76.9 s** |
| — CPU user + sys, peak RSS | 149.3 + 124.5 s, 7,246 MB | 158.7 + 120.9 s, 7,309 MB |

`PERF-PARALLEL` (`build`): 22,715 of 38,757 implementations computed ahead
on workers, 16,042 at their turn on the coordinator, 0 aborted or
invalidated prepares, 0.7 s waiting for workers. The extra peak RSS is the
workers' heaps and the up to 16 prepared snapshots in flight (jemalloc
arenas per thread).

Outputs: the `BAZEL_RS_DUMP_ACTIONS` dump of `build --nobuild` and `test
--nobuild //src/...` (6,483,434 lines) and the rest of stderr are identical
to main's — also at `--loading_phase_threads=1` and with 32 workers and
5 ms worker jitter; `aquery` (text and `--output=summary`) of
`//src/beyondwork.ai/tools/oauth2token/...`; `affected`'s output and every
`--keep_digests` file.

**Why only ~12%.** The coordinator stays busy for the whole run: a sampled
profile of this revision's coordinator (66 s) is commits 27 s, the
implementations computed at their turn 14 s, phase-1 analysis 11 s,
prepares 10 s. An implementation can start only when its dependencies are
committed, and in phase 1's post-order the last dependency of most targets
commits right before them (a `py_library` after its deps' subtrees): 16,042
of them find nothing to overlap with. Computing those on a worker instead
only adds the hand-off latency (measured: 91 s, the coordinator waited
29 s). With commits in plan order on one thread, the floor is the
coordinator's own work (~55 s); genuine's 10.7 s comes from Skyframe
running whole configured-target functions — prepare, implementation and
action registration — concurrently, ordering nothing but their results.

## What remains

Profile of this revision, phase 1 (~59 s on the coordinator): Starlark rule
implementations ~16 s, commits ~17 s, `prepare` ~9 s, the rest configured
analysis (`analyze`, toolchain resolution, `config_conditions`), with ~12 s
of `BTreeMap` lookups (labels, package ids, files, configured-target keys,
output paths) spread over all of them.

- **Per-target commits off the coordinator.** P3 (above) moved the
  implementations whose dependencies commit early; what bounds the wall
  time now is one thread committing every target in plan order. Next: make
  a target's commit a function of its own result and its dependencies'
  committed results (its actions, artifact registrations, runfiles nodes —
  conflicts and shared actions decided by owner order at the merge), run
  prepare + implementation + commit per target on workers in dependency
  order, and merge the per-target results in plan order (the action order,
  first registration, errors and prints all follow from that merge).
- **Shared input vectors.** Every PyCompile node still lists the runtime's
  1,657 inputs in its own vector (26 KB per action); a shared input run held
  by reference, like the runfiles groups (PA-1) but in the middle of the
  input order, would shrink the plan and the commit further.
- Hash indexes beside the hot `BTreeMap`s whose order is never observed
  (done for the File → Artifact index; the `ConfiguredTargetKey`-keyed
  planning maps cost ~3 s of lookups, but some are iterated through
  parameters under other names and need an audit first).
