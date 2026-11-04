# Configured query (`cquery`)

Full-source follow-up: semantic artifact collection now preserves owner-distinct
Files sharing an execution path without lowering actions. The bounded native
`deps(//src/...)` probe succeeds under 8 GiB/15 minutes, with a repeated graph hit
in 2.04 seconds. See [CQUERY_ARTIFACTS.md](CQUERY_ARTIFACTS.md) for exact measurements,
remaining memory headroom, support-file identities and validation boundaries.

The [memory follow-up](CQUERY_MEMORY.md) reduces uncached peak from about 8,001 to
7,549 MiB without changing full-source output bytes. The later isolated PR clone
fixes the venv traversal hygiene and now passes the default `deps(//...)` universe
after actionless incompatible-target support: 218.20 s / 8,033.4 MiB, followed by
a 2.94 s / 1,633.6 MiB graph hit with identical output. See
[TARGET_COMPATIBILITY.md](TARGET_COMPATIBILITY.md) for the exact clone, receipts,
the historical 158.6 MiB cap headroom and build/test boundary. The subsequent
[default-universe memory follow-up](CQUERY_MEMORY.md#default-universe-headroom-follow-up)
shares unchanged rule data: the same full graph takes 215.04 s / 7,460.5 MiB,
leaving 731.5 MiB headroom, with byte-identical output and measured warm reuse.
This is native query success, not a whole-repository TD revision comparison.

The native `cquery` analyzes configured targets and executes Starlark rule and
aspect implementations to produce their providers. It **does not execute build
actions, tests, workspace-status commands, action-input digesting, runfiles-tree
rendering, scheduler work, or execution/output-tree staging**. Repository rules
needed for loading may still run. This is a distinct semantic-analysis boundary,
not an alias for `query` or `build --nobuild`.

```sh
bazel-rs cquery --output=streamed_proto --transitions=lite \
  'deps(//app:all)' --output_file=/tmp/app-deps.pb
bazel-rs cquery --output=streamed_proto '//app:all' --output_file=/tmp/app.pb
bazel-rs config --output=json --dump_all
bazel-rs cquery --output=starlark '//app:all' \
  --starlark:expr='target.label if "IncompatiblePlatformProvider" not in providers(target) else ""'
bazel-rs cquery --universe_scope=//app:all 'config(//app:tool, target)'
```

## Supported surface

* `label`, `label_kind`, `proto` and `streamed_proto` output. Proto uses the Bazel
  9.3 schema and streamed output frames **CqueryResult**, not ConfiguredTarget.
  Inline Configuration.checksum is retained, including `null` for sources.
  See [the wire contract](CQUERY_PROTO.md).
* `starlark` (`--starlark:expr|file`, `providers()` key names, `build_options()`),
  `files` (`--output_groups`), `jsonproto`, `textproto` and `graph` output, and
  `config` listing/show/diff: see [CQUERY_OUTPUT.md](CQUERY_OUTPUT.md).
  target-determinator's default incompatible-target filtering works.
* Configured sets preserve `(label, configuration)` identity. A label used as
  both a target and an exec tool is not collapsed into one result.
* Target patterns, set/let, union/intersect/except, `deps`, `rdeps`, `filter`,
  `kind`, `attr`, `labels`, `some`, and `config(expr,target|null|hash-prefix)`.
  Attr filtering uses resolved values, not all select branches.
* `--universe_scope`; otherwise the expression's target patterns form the
  analyzed universe. Pattern expansion does not remove manual targets.
* `--noimplicit_deps`, `--notool_deps`, `--noinclude_aspects` filter recorded
  configured edges. Source files have null configurations; aliases remain
  distinct; predeclared generated-file targets retain their generating rule.
  Aspect inputs are projected onto the requesting rule, not the aspect's base
  target: required/propagated applications retain sparse provenance during
  analysis, then follow Bazel 9.3's per-request aspect-class traversal. Plain
  consumers of the same base do not inherit aspect-private inputs. Top-level
  aspects do not add forward edges to their bases in non-explicit cquery.
  Config conditions survive `--noimplicit_deps` on their own base rules, but
  conditions flattened through an aspect are implicit on the requester.
  `--notool_deps` compares endpoint configurations, not attribute transitions
  (in particular a null-config source on an exec attr remains a target dep).
* Resolved native/Starlark attrs and modeled defaults, `$rule_implementation_hash`
  over the defining `.bzl` and its transitive loads, real source locations,
  configured attribute/exec/toolchain/condition/platform/aspect dependencies.
* Cquery inherits both **build and test** rc groups and configuration options.
* `config --output=json --dump_all` reads the preceding successful cquery's
  binary configuration table, including modeled native/Starlark values, platform,
  mnemonic and exec status. Native fragment names are identified as native,
  not invented Java fragment instances. It accepts/validates common rc options
  without rerunning analysis. An explicit `--output_base` isolates this state.

### Declared boundaries

This is not full Bazel cquery parity. Unsupported functions/output modes/options
fail explicitly. In particular: provider *values* in Starlark output, keep-going
partial results, full aspect metadata formatting, relative path-as-target literals outside the recorded patterns,
and attr() over License values are not implemented. Existing native analysis
limitations still fail. Direct/indirect incompatible targets are actionless
configured results included in queries; retained conditions project their
attributes without ordinary dependency analysis. See
[TARGET_COMPATIBILITY.md](TARGET_COMPATIBILITY.md) for formatter errors and the
separate [build/test root policy](INCOMPATIBLE_BUILD_SKIP.md). Attribute split
transitions now analyze every branch; see [SPLIT_TRANSITIONS.md](SPLIT_TRANSITIONS.md).
There is no genuine-Bazel fallback and no special-case TD output rewriting.

TD's default mode (`-filter-incompatible-targets=true`) matched genuine 9.3.1 on
a private ~/code pair, including both compatibility-filter cqueries
([CQUERY_OUTPUT.md](CQUERY_OUTPUT.md#target-determinator-default-mode)).

For native TD lockfile isolation, pass
`--bazel-startup-opts=--native-lockfile=/private/MODULE.bazel.lock` as well as
private cache/output-base and rc-ignore startup controls. TD v0.34 does not
forward `--bazel-opts` to `info` or `mod`; a command-only private lockfile flag
therefore cannot protect its `mod dump_repo_mapping` companion. The native
startup default covers all commands; explicit command values still override it.
See [lockfile semantics](REGISTRY_LOCKFILE.md#native-only---native-lockfileworkspacereadonlyabsolutepath).

Configuration checksums are the **existing full native ConfigIds**, not Bazel's
BuildOptions checksums (the genuine algorithm is reproduced from genuine data
in CQUERY_OUTPUT.md; matching it needs every 9.3 fragment option modelled). Actions and ordinary build cache keys are not rekeyed.
Native platform configurations can remain distinct where Bazel trims them to one
empty-options configuration; native graph label spelling is canonical (`@@repo`).
The implementation hash is a versioned native SHA-256 Merkle digest of real
`.bzl` content and ordered transitive loads, not Bazel's builtins-environment
fingerprint. These deliberate identity differences must be normalized for
cross-engine graph comparisons; they are stable across native invocations.

The explicit-module profile does not expose Bazel's embedded genrule setup
source as a repository target. Its implicit setup edge is captured when built-in
repositories are enabled (the default). Loading-only unconfigured `query` still
has its separate output-format support and is not changed.

## Reuse and memory

Graph capture is opt-in. Ordinary builds incur no full graph copy. During
analysis it holds shared rule Arcs and configured edges; the final binary graph
has compact node/configuration indices and no provider, runfiles or Starlark heap
copies. The writer buffers one record, not an entire output stream.
Aspect capture interns application and target identities; application links are
compact IDs, and shared toolchain context dependencies are captured even on
resolution memo hits. No provider or base-graph copies are made per aspect edge.

Graph snapshots reuse the sectioned binary snapshot store, binary identity,
observed_fs input validator, recorded repository replay and local publication
validation. The analysis key includes universe/configuration/aspect/loading
inputs but excludes expression traversal, output format/path, transitions and
edge-presentation filters. `deps(P)` and `P` therefore share analysis. On a default
hit there is no package loading. BUILD, `.bzl`, select/configuration, observed
environment, package listings and source-presence changes are revalidated.
Source **content** is read by TD itself; graph reuse does not replace TD's hashing.

`BAZEL_RS_CQUERY_CURRENT_LOADING=1/on` explicitly opts into an experimental
post-loading checkpoint in a separate cquery analysis namespace. It is off by
default, requires exact current-prefix local coverage and silent/empty events,
and does not replace the default warm path. See the
[current-loading contract](CQUERY_CURRENT_LOADING.md) for accepted values and
remaining scope/performance boundaries; this is a native control, not a Bazel flag.

Each existing analysis key retains **at most two** configured-graph versions.
The fixed filenames are `<short-G>-cq0.snap` and `<short-G>-cq1.snap`; both
headers contain the original full G and exact ELF identity. A checkout/revert
can reuse either version only when **all** its recorded observations, including
`.bazelignore`, still validate. Changed metadata is accepted by the existing
content validator when bytes match. No Git revision is used as a cache key.

Candidates are probed sequentially in MRU order (mtime, then creation time and
slot name). INPUTS/environment, workspace-status rejection, repository replay
and publication integrity precede graph loading. Failed candidate data is
dropped before the next candidate. Corrupt or absent candidates fall through;
changed inputs retain their version for a future revert. Only a selected valid
graph's diagnostics are emitted, once. Graph node/configuration/edge/pattern
indices are checked. Output errors are terminal command errors; they do not
trigger another candidate or full analysis. The config dump is written from the
graph actually used by the successful output command.

Publication stages a complete checksummed binary file before acquiring one
store-scoped `cquery-history.lock` (at most two seconds waiting). Under that lock
it reopens both slots, deduplicates equal content versions, chooses an empty slot
or evicts the LRU, then atomically renames. The 32-byte STATS section is a binary
deduplication digest over normalized input observations, replay identities,
events and graph bytes; it is **not** a validation shortcut. Only input stat
fast-path hints, recording window time and replay timings are normalized. Thus
two ignore-file contents producing identical graphs still occupy distinct
versions, while a verification or concurrent save of A cannot displace B with a
second A. Verification pins the selected reader FD and performs removal/refresh
under the same lock only if the path still names that generation. Readers stay
valid across rename/unlink, and cannot suffer inode-number reuse while pinned.

Both slots count toward the existing store-wide **64 entries** and filesystem-
adaptive **512 MiB–4 GiB** byte budget, together with build snapshots. There is no
second history budget. GC runs after publication: the instant before collection
and concurrent staging files can exceed the budget. Each graph staging file is
bounded by 256 MiB; interrupted `.tmp-` files use the existing one-hour cleanup.
The one fixed lock file is never unlinked; lock files do not grow per key.

Graph-specific bounds (ordinary build/Index decoding is unchanged):

| Item | Maximum |
|---|---:|
| Stored entry, including header | 256 MiB |
| INPUTS | 64 MiB |
| EVENTS | 8 MiB |
| Compressed PLAN | 192 MiB |
| Raw PLAN | 768 MiB |
| Raw frame / frame count | 4 MiB / 192 |
| STATS deduplication digest | exactly 32 bytes |

Entry/section bounds are checked before section allocation. A checked-arithmetic
frame-table preflight checks all offsets, canonical frame sizes and the aggregate
raw length before allocating the decompressed PLAN or frame vectors. The writer
also stops at the raw/stored limits. These are byte-buffer bounds, not a total
process RSS guarantee: the decoded graph and validator/replay structures also
use memory. A normal hit never retains two decoded graphs. The known 265k-node
graph's saved header/framing measures 404,498,195 raw PLAN bytes, 24,530,264 stored
PLAN bytes, 7,961,305 INPUTS bytes and 88,297 EVENTS bytes (32,580,218 total before
the digest section), within these byte limits. Experimental current loading also
retains a loading continuation. Its nonempty-EVENTS and current-local coverage
restrictions exclude the saved application case; these historical buffer sizes
are not a current-prefix memory-fit claim.

`BAZEL_RS_SNAPSHOT=off` disables graph reuse. `verify` compares a reused graph to
fresh complete semantic analysis and fails/invalidates that generation on a mismatch.
`BAZEL_RS_SNAPSHOT_EXPLAIN=1` reports hit/miss/save reasons. Pending lockfile writes
conservatively prevent saving: the initial repository/lockfile initialization
may need another analysis before subsequent calls hit. No daemon is used.

`tests/cquery_history.rs` covers a real tiny Git A/B/A/B checkout sequence,
same-byte replacement and restored-mtime changed-byte adversaries, ignore-file
changes with both equal and different graphs, loading/source/glob/directory/link
observations, replay/environment/publication validation, corrupt-candidate
fallback, LRU eviction, verify deduplication and selected config/diagnostic state.
Dedicated opt-in tests separately cover current-loading counters, mode/key
separation, exactly-once diagnostics and its unsupported fallback boundaries.
Graph-local unit tests cover concurrent writers, pinned-reader generation races,
shared GC, ELF/full-key separation and allocation preflights. These small-fixture
proofs do not establish whole-application TD memory fit; a new ELF must create its
own snapshots before the separate bounded acceptance run.

## Validation and measured performance

The pinned official target-determinator v0.34.0 binary was run with
`--nocache_results --filter-incompatible-targets=false
--before-query-error-behavior=fatal` in fresh scratch git repositories.
All eleven affected-label sets matched a fresh genuine Bazel 9.3 matrix:
unchanged, README, selected/unselected source, executable bit, defining/transitive
`.bzl`, alias, generated file, exec tool and configuration edits. In particular,
the generated-file edit affected its generator and transitive consumers.
Native whole-TD wall time was 0.55–1.28 s, peak child-tree RSS about 49–51 MiB.

Release native-only measurements on the read-only monorepo, private caches and
lockfile, no rc/remote cache/wrappers, Linux x86-64 (shared host; indicative):

| analyzed target | initial repositories + analysis | cached-repo analysis/save | repeated deps | P, same universe | deps bytes |
|---|---:|---:|---:|---:|---:|
| oauth2token:all | 55.40 s / 842 MiB | 11.69 s / 856 MiB | 0.82 s / 157 MiB | 0.82 s¹ | 11,830,048 |
| Rust graphlake/core:core | 32.63 s / 825 MiB | 3.29 s / 657 MiB | 0.81 s / 80 MiB | 0.79 s / 80 MiB | 5,056,432 |
| TS sandbox-types:pkg | 93.67 s / 835 MiB | 6.93 s / 819 MiB | 0.25 s / 55 MiB | 0.23 s / 54 MiB | 18,237 |

¹ An exact-child RSS follow-up measured 147 MiB and 2.85 s on the shared host;
the first P run's aggregate child RSS included an earlier sibling invocation.
All measured analyses were below 1 GiB. Counters reported zero action-input
bytes hashed and zero scheduler nodes. These are not timings of a full
`deps(//src/...)` universe.

A subsequent integration probe of `deps(//src/...)`, capped at 8 GiB and 15 minutes,
stopped with an explicit unsupported split-transition error on
`//src/beyondwork.ai/appscanner/cmd:image_push_index`'s `images` edge to
`image_image`. It took 248.9 s (including further repository initialization), with
3,453 MiB peak child RSS; no graph output was published. Thus the scratch TD matrix
and subset timings do **not** establish whole-repository TD support. Split
transitions were the next analysis prerequisite. The checkout HEAD and lockfile
were unchanged throughout this probe. The split-transition follow-up now passes
that appscanner target: first/second/third deps queries took 28.15/11.05/0.40 s,
and roots sharing the universe took 0.36 s. The first run initialized another OCI
repository and could not save; the second saved and the remaining calls hit.
Deps output was 30,424,420 bytes; peak analysis RSS was 1,314 MiB, hit RSS 269 MiB.

The follow-up full `deps(//src/...)` probe reached a **new** analysis blocker after
430.09 s at 4,875 MiB (still within 8 GiB/15 minutes), with no output published:
`//src/python/sandbox-extension:policy`, `get_link_args()` parameter
`parameter_file_type` received `NoneType`, but the native API requires `string`.
Genuine CcStarlarkInternal.getArgs accepts None. A focused default-configuration
query of this target succeeds; the failure is in a configuration reached by the
whole-source graph. The nullable API follow-up fixes this without adding parameter-
file metadata for None.

With that correction and genuine converged-split cleanup, the next bounded
whole-source query failed after 464.69 s at 6,230 MiB with an analysis-order error:
`rustfmt_srcs_aspect` on the external `crates__fastrand-2.5.0//:fastrand` target
"was not analyzed" (reported while analyzing `//src/rust/graphlake/core`). No query
output was published; the 8 GiB cap and 15-minute timeout were retained and the
checkout canaries were unchanged. Whole-repository TD support is therefore
**still not established**.

A 200-way scratch fanout measured 69 ms cold, 9 ms repeated deps, 7 ms for P,
8 ms after source-content edit (valid graph hit; TD reads the changed content),
52 ms after BUILD edit, 48 ms after transitive `.bzl` edit and 47 ms after
configuration edit (all three misses). Peak RSS was 14–28 MiB; deps output was
216,343 bytes. Snapshot tests also cover source removal/restoration, repository
environment changes and verification against fresh semantic analysis.
