# Action graph query (`aquery`) and `genquery`

`bazel-rs aquery EXPRESSION` analyzes the expression's universe with action
planning (like `build --nobuild`, without workspace status, input digests or
execution) and prints the actions of the configured targets the expression
selects, in Bazel 9.3.1's formats
(query2/aquery/ActionGraph{Text,Summary,Proto}OutputFormatterCallback,
skyframe/actiongraph/v2, analysis_v2.proto). Sources: `src/aquery.rs`
(options, filters, formatters), `src/aquery_key.rs` (Bazel action keys),
`src/engine/aquery.rs` (capture and action descriptions),
`src/engine/aquery_test.rs` (TestRunner actions),
`src/java_actions/command.rs` (the Java actions' CustomCommandLines and
keys, docs/JAVA_ACTIONS.md), `src/java_gzip.rs`
(Java's GZIP bytes). Genuine captures: `tests/fixtures/aquery/kitchen` and
`tests/fixtures/aquery/depsets`, `tests/fixtures/aquery/test_runner`
(`tests/aquery.rs`),
`tests/fixtures/cc_compile/genuine_aquery.txt` (`tests/cc_compile.rs`),
`tests/fixtures/java_action_parity` (`tests/java_action_parity.rs`: every
Java action class, keys included). A Starlark spawn's `%{input}` is its
first declared input (the inputs NestedSet lists inputs before tools).

```sh
bazel-rs aquery //pkg:target
bazel-rs aquery 'mnemonic("CppCompile", deps(//...))' --output=jsonproto
bazel-rs aquery 'inputs(".*\.go", outputs(".*\.a", deps(//cmd:app)))' --output=summary
```

## Surface

* **Expressions**: target patterns and the configured-query language of
  `cquery` (`deps`, `rdeps`, `set`, `let`, union/intersect/except, `kind`,
  `filter`, `attr`, `labels`, `some`; docs/CQUERY.md), evaluated over the
  same captured configured graph (`--universe_scope`, `--noimplicit_deps`,
  `--notool_deps`). A pattern selects every configuration of its targets in
  the analyzed universe, like genuine post-analysis queries. `config()` is a
  cquery-only function and is rejected by the parser, as in Bazel.
* **Action filters** `inputs(PATTERN, expr)`, `outputs(PATTERN, expr)`,
  `mnemonic(PATTERN, expr)`: Java-style full-match regular expressions;
  filters along the chain of first function arguments from the top combine
  (AqueryProcessor.buildActionFilters: every pattern of one function must
  match; `inputs`/`outputs` match when any artifact does). A filter below
  another function fails before analysis with genuine's message and exit 1;
  a one-argument filter fails at evaluation like genuine.
* **Outputs**: `text` (default), `commands`, `summary`, `proto`,
  `streamed_proto`, `textproto`, `jsonproto`; anything else fails with
  genuine's `Invalid output format` list (exit 2, before analysis instead of
  after). `--output_file` writes atomically.
* **Options**: `--include_commandline`, `--include_artifacts`,
  `--include_param_files` (ParameterFileWriteAction contents on the actions
  that read them; implies the command line), `--include_file_write_contents`,
  `--include_aspects`, `--include_pruned_inputs` (accepted; native inputs are
  never pruned). `--skyframe_state` fails explicitly: the native profile keeps
  no Skyframe state between commands (an aquery snapshot, "Snapshot" below,
  holds one universe's described actions, not a server's action graph).
* Options are inherited from `build` (rc groups `common`, `build`, `aquery`).

### Formats

* **text**: byte-identical to genuine for every modelled field: `action
  'PROGRESS'` (placeholders, ` [for tool]`), Mnemonic, Target (display form
  through the main repository mapping), Configuration mnemonic, Execution
  platform (an alias resolves to its PlatformInfo label), AspectDescriptors,
  ActionKey, sorted Inputs/Outputs (`(TreeArtifact)`), sorted fixed
  Environment (CppCompile: the configuration's, like genuine), the
  `describeCommand` command line with `$`-safe ShellEscaper quoting and the
  `# Configuration:`/`# Execution platform:` trailer, Params File Content,
  sorted ExecutionInfo, Template/Substitutions, IsExecutable,
  FileWriteContents (base64), UnresolvedSymlinkTarget, and Bazel 9's
  `runfiles for LABEL` RunfilesTree entries.
* **summary**: counts in genuine's order, including Java `HashMap.merge`
  iteration order for equal counts.
* **proto family**: the ActionGraphContainer messages in the order the
  sequential ActionGraphDump emits them (BaseCache: ids in first-use order,
  path fragments and depsets emitted after their children). `proto` writes
  them as container fields, `streamed_proto` as length-delimited
  single-entry containers, `textproto` as `label {…}` TextFormat blocks,
  `jsonproto` as JsonFormat's monolithic container. Genuine's default
  parallel output order is nondeterministic; the tests compare decoded,
  id-resolved containers.

### Action keys

`ActionKey`/`action_key` is Bazel's `ActionKeyComputer.getKey` (Fingerprint
over SHA-256, the execution platform's PlatformInfo, exec properties) for:

* genrules and Starlark `run`/`run_shell` spawns: flat strings and `Args`
  (`add`/`add_all`/`add_joined` with formats, uniquify, omit_if_empty,
  expand_directories, map_each, Labels, param files). An `Args` over a
  **depset** is fingerprinted like ActionKeyContext's
  NestedSetFingerprintCache: the order's ordinal, then the NestedSet's
  internal structure — a leaf's strings, or the SHA-256 of the children
  array, each nested array by its own digest — rebuilt from the native depset
  DAG with the NestedSet constructor's layout (per-order placement of direct
  and transitive members, singleton hoisting, identity deduplication, LINK
  order's reversal; `aquery_key::nested_set`). Without map_each the
  StringificationType comes first; map_each runs per element (at analysis
  a directory is mapped as it is, with a NoopExpander). rules_go
  (GoCompilePkg, GoLink, nogo), rules_python's PythonZipper and rules_cc's
  link actions use these. The command line aquery prints shows that
  NoopExpander mapping of a tree artifact too (it used to print the
  directory's path). When the mapping fails (rules_js's `js_image_layer`
  reads `tree_relative_path` of what the expander lists, here the directory
  itself, js_image_layer.bzl:495) analysis and the build are unaffected,
  and an aquery that prints the action fails like genuine's: `ERROR: build
  interrupted`, exit code 8, no output (the formatter's
  CommandLineExpansionException becomes an IOException, then an
  interruption, OutputFormatterCallback.processAllTargets). Queries that do
  not print the action and `--output=summary` succeed. Before, the mapping
  failure was an analysis error of the target for every aquery. Genuine
  capture: `tests/fixtures/aquery_noop_expander`.
* `ctx.actions.write` of a string: up to 256 characters a
  RegularFileWriteAction; longer (in Bazel's internal Latin-1 characters) a
  CompressedFileWriteAction keyed by the bytes of Java's GZIPOutputStream,
  produced by `src/java_gzip.rs` — a port of zlib 1.3's deflate at the JDK
  Deflater's parameters (level 6, windowBits 15, memLevel 8), checked against
  zlib on pinned samples and a differential vector set
  (tests/fixtures/aquery/depsets/vectors.py).
* `ctx.actions.write` of an `Args` (ParameterFileWriteAction),
  `expand_template`, `symlink` (SymlinkAction on its internal platform),
  unresolved symlinks.
* rules_cc: CppCompile (CppCompileAction.computeKey: the in-memory .d bit,
  the configuration and feature environments, execution info, the digest of
  the compiler options, the declared include sources, the mandatory and
  spawn inputs built like CppCompileActionBuilder — toolchain
  `compiler_files`, builtin include files, module maps, the source — and the
  built-in include directories, all NestedSets by structure); CppLink and
  CppArchive (cc_internal.get_link_args: LinkCommandLine's arguments and its
  param file info, GCC_QUOTED included); SolibSymlink (the symlink's and the
  library's exec paths on the internal platform).
* RunfilesTree (an empty computeKey on the internal platform).
* TestRunner (TestRunnerAction.computeKey): one action per shard and run of
  every analyzed test rule (TestActionBuilder.createTestAction), listed
  after the rule's RunfilesSupport actions: `Testing LABEL (shard i of n,
  run j of m)`, inputs (the runfiles tree, test-setup.sh, generate-xml.sh,
  a `--run_under` label's executable), outputs (`test.log`, `test.xml`,
  `test.cache_status`, the `test.outputs` tree), the configuration's
  environment, TestStrategy.expandedArgsFromAction's command line
  (`--run_under` prefixes, `args`, `--test_arg`), and the key over the test
  arguments, `--test_filter`, `--test_runner_fail_fast`, `--run_under`,
  RunEnvironmentInfo, the action and `--test_env` environments, size,
  timeout and its `--test_timeout` duration, tags, remotability,
  shard/run numbers (`--runs_per_test`, `--test_sharding_strategy`),
  `--zip_undeclared_test_outputs` and the execution info in its map order
  (TargetUtils.getExecutionInfo's HashMap order, then `local`/
  `no-remote-exec`, testing.ExecutionInfo, `--modify_execution_info`).
  The execution platform is the configuration's host platform (genuine:
  the `test` exec group's; equal on single-platform hosts).
  Genuine-captured: tests/fixtures/aquery/test_runner
  (`aquery_test_runner_actions_match_genuine`).

Verified against genuine 9.3.1 on tests/fixtures/aquery/kitchen and
tests/fixtures/aquery/depsets (every action byte for byte),
tests/fixtures/cc_compile/genuine_aquery.txt, and by hand on rules_go,
rules_cc/rules_python workspaces and a private ~/code clone (every Go action
of `deps(//src/beyondwork.ai/utils/str/...)`).

Not reproduced (a deterministic native key is printed instead):

* SymlinkTree and SourceSymlinkManifest: Runfiles.fingerprint with absolute
  paths (`digestAbsolutePaths`: the output base's execroot and external
  directory), so their keys differ between any two output bases, genuine
  ones included.
* RepoMappingManifest: the NestedSet of the target's transitive
  `Package.Metadata` (Skyframe's transitive-packages structure) and each
  package's repository mapping.
* TranslateBuildInfo: the bzlTransitiveDigest of the translation callback's
  module, which includes the digest of the binary's embedded @_builtins.
* Args over a depset of Labels (LABEL stringification adds the main
  repository mapping) or of other non-string objects, and natively built
  command lines other than get_link_args.

Known gaps seen on stardoc 0.8.0's `deps(//:docs)` (2026-11-04; keys equal
unless stated): the starlark_doc_extract binary
proto write (BinaryFileWriteAction) differs in key and prints
`IsExecutable`.

Descriptions follow AbstractAction.getProgressMessage (crate::action_console
shares the rules with the console): a C++ compile names its source file
(CppCompileAction.getRawProgressMessage, recorded at registration, so a
header-parsing action names its header), `%{input}` is the first of the
action's own inputs, and an action without a raw progress message
(SolibSymlinkAction) prints `MNEMONIC output` without ` [for tool]`.

The text output's `Execution platform:` prints the platform through the main
repository mapping (`@platforms//host:host` when `platforms` is a direct
dependency), the command trailer, summary and proto its canonical form, like
genuine. A target in the no-config configuration (NoConfigTransition, e.g.
`package_metadata`) has no execution platform.

### Divergences (normalized in the tests)

* `# Configuration:` and the proto `checksum` are the native ConfigIds, not
  Bazel's BuildOptions checksums (docs/CQUERY.md).
* Result order: configured targets by label, then native configuration
  order; each target's actions in registration order, RunfilesSupport
  actions after the rule's own, then its aspects'. Genuine iterates a hash
  set: two configurations of one label may appear in the other order.
* `dep_set_of_files`: one flat depset per action (genuine mirrors each
  action's NestedSet structure); consumers that flatten depsets see the same
  inputs. Input order inside the depset is the native one.
* A tool's runfiles appear as its `.runfiles` tree input (as genuine) when
  the planner shares the tree; otherwise as its entries.
* Shared actions (two owners registering one action) are listed once, under
  the owner that planned them.
* TestRunner actions under `--collect_code_coverage` (the coverage
  TestRunnerAction's collect_coverage.sh argument, coverage inputs,
  environment and `coverage.dat` output) are not listed.
* `--include_file_write_contents` on SourceSymlinkManifest prints empty
  contents (genuine's names absolute output-base paths).

Memory: like genuine's formatters, the output lists one action's inputs at
a time. A record whose inputs are exactly its planned action's keeps no
copy of them (`ActionRecord::listed_from`): they are listed from the
planned action (its shared input groups held once) when the action is
written, and a Starlark spawn's NestedSet input order (`Meta::input_order`,
for `--print_action` and the primary input) holds its Files, not their
paths as strings. `aquery 'mnemonic("PyCompile", deps(//src/python/...))'
--output=text` on ~/code (69,237 actions, 12.9 GB of text, a 1,655-file
Python runtime per action) peaks at 6.4 GB; before, it exceeded 40 GB
before printing anything (every action's inputs as strings, twice: the
analysis's records and the selected copy).

`--print_action` reads every action's inputs at once: they are listed for
all records first (`Analysis::materialize`).

Only `print_action` reads a spawn's whole input order; under `aquery`,
`Meta::input_order` keeps the first input (the primary input of the
description, `%{input}`) and nothing else (`aquery::whole_input_order`).
`aquery 'kind("cc_.*", //src/...)'` on ~/code analyzes 223k configured
targets (the pattern keeps manual targets, 1.67x the targets of
`build //src/...`; a `build --nobuild` of the same 17,901 targets peaks at
11.0 GiB): 11.7 -> 10.7 GiB peak, 180 -> 151 s, output byte-identical (the
whole order held ~0.95 GB: 70k PyCompile spawns listing the 1,655-file
Python runtime as Files).

### Speed (query-speed, 2026-11-20)

aquery analyzes on the build's fast paths: its walks are the frozen walks
through the merge buffer, with worker deltas and natives published by the
workers (before, an aquery capture switched off commits ahead, worker deltas
and frozen walks, so every Starlark rule computed and committed at its turn
on the coordinator). What aquery needs beyond a build is captured on those
paths too: the action owners (`Engine::aquery_owner`) are recorded wherever
the console owners are (every prepare path: the coordinator's, a batch's, a
frozen job's merge, an aspect job's), the descriptions (`Meta`) are built at
the compute and travel in the actions, and the configured graph is
cquery's phase-1 capture. `BAZEL_RS_TIMING=1` prints `PERF-AQUERY nodes=…
owners=… described=… records=…` and `aquery.*` marks.

The action records are then described on several threads
(`--loading_phase_threads`, chunks of 256 actions, kept in plan order),
each platform's PlatformInfo looked up once (it was a scan of the
platform table per action).

~/code (non-LTO builds, 4 alternating rounds, load 24-70 from other
work, output byte-identical every round):

| command | before | after | peak RSS |
|---|---|---|---|
| `aquery 'kind("cc_.*", //src/...)'` | 153-165 s | 48-54 s | 10.7 -> 13.7 GB |
| `aquery 'deps(//src/python/tools/...)'` | 7.0-11.2 s | 5.8-9.5 s | 1.2 GB |

Of the 160 s before, the analysis took ~100 s and the records 41 s; after,
~27 s and 2.5 s. The extra memory is the frozen walk's (its view and the
deltas in flight), as for `build --nobuild`.

Memory (query-speed, second pass): the fast paths first peaked at 13.7 GB
on that query (10.7 GB before them). Brought back under it by:

* query walks classify at most 1,024 entries past the cursor
  (`BAZEL_RS_T3_QUERY_WINDOW`; unbounded, finished jobs' results waited
  for the cursor: ~1.3 GB) and plan rules with attribute aspects at their
  turn (no aspect jobs for queries);
* the deferred flushes' pending ops queue (768 MB at its walk's start, the
  cquery capture's edges mostly) shrinks as the walk drains it;
* one interned transitive-repository set per distinct closure, shared by
  a target's prepare, commit and published results (each target's set
  was held twice: 257 MB in the planning state alone);
* a worker's source-file views use the index's artifact instead of a copy
  per view;
* only the selected targets' actions are described (after the
  expression's selection), `Meta::arguments` interned
  (`InternedStrings`, crate::interner; 420 MB as `Vec<String>`), the fixed
  environment shared per distinct value.

Every planned action is described or skipped as the build's own (the
workspace status actions): an action without an owner, or whose owner is
not in the configured graph, is an internal error in debug builds and
under `BAZEL_RS_PARITY_VERIFY=1`. Differential tests compare the fast
paths (default, frozen walks forced, with jitter) with the sequential path
(`BAZEL_RS_NO_COMMIT_AHEAD=1 BAZEL_RS_NO_WORKER_DELTAS=1
BAZEL_RS_EVAL_FROZEN_PREPARES=0`) byte for byte: tests/aquery.rs (every
format, aspects, param files, writes) and tests/shared_actions.rs (shared
actions in two target orders: one owner each). `--keep_going` fails closed
for configured queries on both paths.

### Snapshot (aquery-snapshot, 2026-11-25)

A repeated aquery of the same universe and options prints from a snapshot
instead of analyzing again (genuine answers it from its warm server's
Skyframe). Code: `src/aquery_snapshot.rs` (payload),
`src/cquery_state.rs` (`start_aquery`, `AquerySession`: key, lookup,
save), `Engine::aquery_describe_snapshot` (src/engine/aquery.rs).

* **What it is.** The configured-graph snapshot of docs/CQUERY.md ("Reuse
  and memory") with aquery's payload: the same recording window (opened
  before the registry session and loading), validation of every recorded
  observation, local repository replay, publication adoption, saved
  diagnostics (EVENTS, printed once on a hit), the store-wide history lock
  and garbage collection, `BAZEL_RS_SNAPSHOT=off|verify` and
  `BAZEL_RS_SNAPSHOT_EXPLAIN=1` (`bazel-rs-aquery-snapshot:` lines). Two
  slots per key, `<short-G>-aq0.snap` and `-aq1.snap`, beside cquery's
  `-cq` slots (separate keys: they never mix).
* **Key.** G (docs/ANALYSIS_SNAPSHOT.md) of command `aquery-graph-v1`, the
  universe's target patterns (`--universe_scope`, else the expression's) and
  every option but the presentation ones — `--output`, `--output_file`,
  `--query_file`, `--universe_scope`, `--[no]implicit_deps`,
  `--[no]tool_deps`, `--[no]include_aspects`, `--[no]include_commandline`,
  `--[no]include_artifacts`, `--[no]include_pruned_inputs`,
  `--[no]include_param_files`, `--[no]include_file_write_contents`,
  `--transitions`, `--proto:*`, `--starlark:*`, `--graph:*`. The options G
  leaves out for builds as execution-only (`snapshot::key::EXCLUDED`: the
  test options decide the TestRunner records) are folded into the command
  (a digest of them, in order). So the expression, its filters and the
  output flags can change between hits over one universe; anything else is
  a miss, and a miss saves anew. `print_action` (an aquery that lists every
  input) never uses it.
* **Payload.** PLAN: the configured graph, the dump's configurations and the
  index of the records (postcard, the graph's deflated frames). AQUERY:
  every planned action of the universe **described** at the save — its
  `ActionRecord`s, computed by the code a cold aquery prints from — in
  blocks of consecutive configured targets (about 256 actions each, a
  target's records never split, in the dump's per-target order), each block
  with its distinct strings once and deflated on its own; a record listed
  from its planned action (`ActionRecord::listed_from`) stores that
  listing as runs of its own inputs and references into a table of the
  shared input groups (stored once, deflated in chunks of 32), expanded and
  deduplicated like `aquery::listed_inputs` when written. A debug build
  (and `BAZEL_RS_PARITY_VERIFY=1`) checks every stored listing's expansion
  against the planned action's listing at the save.
* **A hit** runs no loading, analysis, Starlark or description: the
  expression's selection over the stored graph, then only the blocks
  holding a selected target and the group chunks their listings reference
  are inflated (on `--loading_phase_threads`, at most 16 threads), and the
  formatters write as a cold aquery does (filters, `interrupted`, every
  format and include flag). Output and diagnostics are byte-identical to
  `BAZEL_RS_SNAPSHOT=off` (tests/aquery.rs
  `aquery_snapshot_hit_prints_what_a_cold_aquery_prints`: every format and
  include flag, filters, expressions over one universe, a changed template
  and BUILD file, verify mode; `aquery_snapshot_is_keyed_by_the_test_options`).
* **Soundness.** What a hit prints is a function of the analysis (the
  window's recorded inputs, validated like cquery's) and of the template
  sources aquery prints: those are read through `observed_fs` at the save
  (READ observations, validated by content; before, an unrecorded read).
  No client environment value reaches a description (inherited variables
  stay names, as in genuine's aquery). Plan-dependent taints (map_each,
  build-info callbacks) do not block the save: a hit never runs them, it
  prints their descriptions computed at the save; every other taint does.
  `BAZEL_RS_SNAPSHOT=verify` compares the fresh graph, configurations,
  record index and AQUERY bytes with the hit and removes a mismatching
  generation (the command fails).
* **Miss cost.** The save describes the whole universe (a cold aquery
  without it describes only the selected targets): the records of a block
  are encoded as they are described and dropped, 16 threads, one block per
  task, so the described universe is never held at once.

~/code (non-LTO fast builds, 3 alternating rounds, load 13-26 from other
work; output byte-identical in every run; a cold run of this branch is a
miss that saves):

| command | before | miss (saves) | `BAZEL_RS_SNAPSHOT=off` | hit | hit peak RSS |
|---|---|---|---|---|---|
| `aquery 'kind("cc_.*", //src/...)'` (175,608 actions, 411 printed) | 46.6-50.0 s, 8.5 GB | 53.8-58.0 s, 8.9 GB | 45.8 s | 1.4-1.7 s | 1.9 GB |
| `aquery 'deps(//src/python/tools/...)'` (2,613 actions, 437 MB of text) | 5.0-5.8 s | 5.4-5.8 s | 5.2 s | 1.7-2.5 s (0.3 s before the formatter) | 0.2 GB |

A miss costs ~7 s more than before on kind(cc_.*): ~5 s of analysis under
the recording window (observed_fs, the same cost as a cquery miss; +0.4 GB
peak for its observations) and ~2.5 s after it: group table 0.6 s,
describing and encoding the universe 0.7 s, the window's inputs 0.8 s, the
graph's frames 0.6 s, the write 0.2 s (describing the universe is
streamed: no peak over the analysis's). `BAZEL_RS_SNAPSHOT=off` is the
path before, unchanged. The kind(cc_.*) snapshot is 83 MB: PLAN ~35 MB
(the graph, as a cquery snapshot of the universe) and AQUERY 47.7 MB (137 MB
before the per-block string tables); the Python one 4 MB. A kind(cc_.*)
hit: validation 0.56 s, the graph's inflate 0.1 s and chunked postcard
decode 0.4 s (on 8 threads; 1.15 s decoded whole), the selected blocks
0.03 s, the output 0.12 s.

## `genquery`

The native `genquery` rule (rules/genquery/GenQuery.java) runs its
`expression` with the query engine of `bazel-rs query` at analysis and
writes the result (a FileWrite-mnemonic write; `compressed_output` gzips
it). Labels resolve from the repository root (`:a` is `//:a`); `opts`
accepts the query options and rejects `--keep_going`, `--universe_scope`,
`--order_results`, `--noorder_results`, `--order_output` and
`--experimental_graphless_query` like genuine; nodep dependencies are
excluded unless `--nodep_deps` is given; streamed formats (`label`,
`label_kind`, `build`, …) are sorted lexicographically unless the expression
is a top-level `somepath`, others are fully ordered; labels print as
`Label.toString` (`@@repo//…`). Genuine-captured tests: `tests/genquery.rs`.

The scope is the transitive closure of `scope`. Native checks the
**result**: with `strict` (the default) the first result target outside the
scope fails with genuine's `query failed: Evaluation failed: target 'X' is
not within the scope of the query`; with `strict = False` each is skipped
with genuine's warning. Genuine also refuses (or prunes, non-strict) targets
the evaluation merely *visits* outside the scope, so a non-strict query
that traverses an out-of-scope target keeps that target's in-scope
dependencies natively but not in genuine. The scope targets are analyzed as
ordinary dependencies.

## `starlark_doc_extract`

Native: the ModuleInfo binary and text protos byte-identical to genuine
(docs/STARLARK_DOC_EXTRACT.md). Its two outputs are `FileWrite` actions.
