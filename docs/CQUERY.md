# Configured query (`cquery`)

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
bazel-rs cquery --universe_scope=//app:all 'config(//app:tool, target)'
```

## Supported surface

* `label`, `label_kind`, `proto` and `streamed_proto` output. Proto uses the Bazel
  9.3 schema and streamed output frames **CqueryResult**, not ConfiguredTarget.
  Inline Configuration.checksum is retained, including `null` for sources.
  See [the wire contract](CQUERY_PROTO.md).
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
fail explicitly. In particular: Starlark formatting and incompatible-target
filtering, keep-going partial results, transitions=full, full aspect metadata
formatting, relative path-as-target literals outside the recorded patterns,
and attr() over License values are not implemented. Existing native analysis
limitations (including incompatible targets and split transitions) still fail.
There is no genuine-Bazel fallback and no special-case TD output rewriting.

Configuration checksums are the **existing full native ConfigIds**, not Bazel's
BuildOptions checksums. Actions and ordinary build cache keys are not rekeyed.
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

Graph snapshots reuse the sectioned binary snapshot store, binary identity,
observed_fs input validator, recorded repository replay and local publication
validation. The analysis key includes universe/configuration/aspect/loading
inputs but excludes expression traversal, output format/path, transitions and
edge-presentation filters. `deps(P)` and `P` therefore share analysis. On a hit
there is no package loading. BUILD, `.bzl`, select/configuration, observed
environment, package listings and source-presence changes are revalidated.
Source **content** is read by TD itself; graph reuse does not replace TD's hashing.

`BAZEL_RS_SNAPSHOT=off` disables graph reuse. `verify` compares a reused graph to
fresh complete semantic analysis and fails/removes the snapshot on a mismatch.
`BAZEL_RS_SNAPSHOT_EXPLAIN=1` reports hit/miss/save reasons. Pending lockfile writes
conservatively prevent saving: the initial repository/lockfile initialization
may need another analysis before subsequent calls hit. No daemon is used.

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
`deps(//src/...)` universe, which was not attempted without a safe bound.

A 200-way scratch fanout measured 69 ms cold, 9 ms repeated deps, 7 ms for P,
8 ms after source-content edit (valid graph hit; TD reads the changed content),
52 ms after BUILD edit, 48 ms after transitive `.bzl` edit and 47 ms after
configuration edit (all three misses). Peak RSS was 14–28 MiB; deps output was
216,343 bytes. Snapshot tests also cover source removal/restoration, repository
environment changes and verification against fresh semantic analysis.
