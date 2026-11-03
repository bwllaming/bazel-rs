# Action graph query (`aquery`) and `genquery`

`bazel-rs aquery EXPRESSION` analyzes the expression's universe with action
planning (like `build --nobuild`, without workspace status, input digests or
execution) and prints the actions of the configured targets the expression
selects, in Bazel 9.3.1's formats
(query2/aquery/ActionGraph{Text,Summary,Proto}OutputFormatterCallback,
skyframe/actiongraph/v2, analysis_v2.proto). Sources: `src/aquery.rs`
(options, filters, formatters), `src/aquery_key.rs` (Bazel action keys),
`src/engine/aquery.rs` (capture and action descriptions). Genuine captures:
`tests/fixtures/aquery/kitchen` (`tests/aquery.rs`).

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
genrules and Starlark `run`/`run_shell` spawns (flat strings and `Args`
built from `add`/`add_all`/`add_joined` over lists, formats, uniquify,
omit_if_empty, expand_directories, map_each over lists, Labels, param files),
`ctx.actions.write` of a string up to 256 characters, `ctx.actions.write`
of an `Args` (ParameterFileWriteAction), `expand_template`, `symlink`
(SymlinkAction with its internal platform) and unresolved symlinks. Verified
against genuine on the kitchen fixture (every key) and a rules_cc/rules_python
fixture (CcStrip, PyCompile, Symlink, ExecutableSymlink, TemplateExpand,
FileWrite).

Not reproduced (a deterministic native key is printed instead):

* `Args` over a **depset** (NestedSetFingerprintCache digests the NestedSet's
  internal node structure) and map_each over directories — e.g. rules_go's
  GoCompilePkg and rules_python's PythonZipper.
* FileWrite contents longer than 256 characters (CompressedFileWriteAction
  keys Java's GZIP bytes).
* Native-class actions: CppCompile/CppLink/CppArchive/SolibSymlink,
  RepoMappingManifest, SourceSymlinkManifest, SymlinkTree, RunfilesTree,
  TranslateBuildInfo.

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

Not implemented: analysis fails with `native rule starlark_doc_extract
analysis not implemented`. It needs a ModuleInfo (stardoc_output.proto)
extractor over a loaded module: RuleInfo (attributes with AttributeType,
default value reprs, mandatory, provider name groups, docs), ProviderInfo
(fields, docs, init), StarlarkFunctionInfo (parameter kinds/defaults and
Starlark docstring parsing: Args/Returns/Deprecated sections),
AspectInfo, MacroInfo, ModuleExtensionInfo and RepositoryRuleInfo; the
OriginKey of each entity (exporting name and defining file label); the
module docstring; `symbol_names`, `render_main_repo_name` and
`allow_unused_doc_comments`; and binary/text proto encoding of the result.
The loader keeps the evaluated globals but none of this metadata today.
