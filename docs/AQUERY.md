# Action graph query (`aquery`) and `genquery`

`bazel-rs aquery EXPRESSION` analyzes the expression's universe with action
planning (like `build --nobuild`, without workspace status, input digests or
execution) and prints the actions of the configured targets the expression
selects, in Bazel 9.3.1's formats
(query2/aquery/ActionGraph{Text,Summary,Proto}OutputFormatterCallback,
skyframe/actiongraph/v2, analysis_v2.proto). Sources: `src/aquery.rs`
(options, filters, formatters), `src/aquery_key.rs` (Bazel action keys),
`src/engine/aquery.rs` (capture and action descriptions),
`src/java_actions/command.rs` (the Java actions' CustomCommandLines and
keys, docs/JAVA_ACTIONS.md), `src/java_gzip.rs`
(Java's GZIP bytes). Genuine captures: `tests/fixtures/aquery/kitchen` and
`tests/fixtures/aquery/depsets` (`tests/aquery.rs`),
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
  no analysis cache between commands.
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
  link actions use these.
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
* A test target's TestRunner action (TestActionBuilder) is not listed.
* `--include_file_write_contents` on SourceSymlinkManifest prints empty
  contents (genuine's names absolute output-base paths).

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
