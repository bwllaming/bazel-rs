# `query`: the Bazel 9.3 query language (N39)

`bazel-rs query [OPTION...] EXPRESSION...` evaluates Bazel 9.3's query
language over the unconfigured target graph. The residue is joined with
spaces like `QueryOptionHelper.readQuery`, so `query deps( //a )` works.

Pinned source: `/tmp/opencode/bazel-9.3-reference`; paths are relative to
`src/main/java/com/google/devtools/build/lib/`.

| Area | Native | Bazel source |
|---|---|---|
| Lexer, parser, `toString` forms, syntax errors | `src/query_language.rs` | `query2/engine/Lexer.java`, `QueryParser.java`, `TargetLiteral.java:90-115`, `BinaryOperatorExpression.java:184-193` |
| Environment, error/exit handling | `src/engine/query.rs` | `runtime/commands/QueryCommand.java`, `QueryEnvironmentBasedCommand.java`, `query2/common/AbstractBlazeQueryEnvironment.java:150-345`, `query2/engine/QueryException.java:26-37` |
| Visitation | `Env::visit`, `Env::edges` | `query2/query/LabelVisitor.java`, `PathLabelVisitor.java`, `packages/LabelVisitationUtils.java` |
| Attribute model | `src/engine/query_attrs.rs` | `packages/AggregatingAttributeMapper.java:75-219`, `DependencyFilter.java`, `StarlarkRuleClassFunctions.java:159-378` |
| Output formatters | `Env::output` and helpers | `query2/query/output/*.java` |

## Language

All of `QueryEnvironment.DEFAULT_QUERY_FUNCTIONS`
(`query2/engine/QueryEnvironment.java:741-758`): `allpaths`, `attr`,
`buildfiles`, `deps` (optional depth), `executables`, `filter`, `kind`,
`labels`, `loadfiles`, `rdeps` (universe, optional depth),
`same_pkg_direct_rdeps`, `siblings`, `some` (optional count), `somepath`,
`tests`, `visible`; `set(...)`, `let x = e in e` with `$x`, and the binary
operators `union`/`+`, `intersect`/`^`, `except`/`-` (equal precedence, left
associative, runs of one operator in one node). `allrdeps` and `rbuildfiles`
are SkyQuery functions and therefore `unknown function` errors, as in Bazel
without `--universe_scope`. `|` and `&` are not operators in Bazel 9.3's lexer
(they lex as words). `kind`/`filter`/`attr` regular expressions use
`fancy-regex` (Java `Matcher.find` semantics; common syntax errors use
Java's `PatternSyntaxException` text, other Java-only constructs differ). A
fancy-regex runtime failure (its backtracking limit) is an explicit query
error, never a non-match; Java has no such limit.

Target patterns: every target-literal word is parsed and expanded like a
`build` pattern (N38, docs/LABEL_SYNTAX.md): `TargetPattern.Parser` with the
working directory below the workspace root as relativeDirectory
(`crate::target_pattern::parse`), then `Engine::expand_pattern` with
FilteringPolicies.NO_FILTER. That covers `//pkg:target`, relative `:t`/`t`,
`//pkg` (`//pkg:pkg`), path-as-target words, `:all`, `:*`, `:all-targets`,
`//pkg/...`, `//...`, `...:*`, `...:all-targets` and external `@repo//...`
forms. An absolute `:all` naming an existing target wins over the wildcard
with Bazel's ambiguity warning; an empty recursive pattern is `no targets
found beneath 'dir'`; ignored directories warn like `build`. A leading `-` is
the query's `except` operator, never a negative pattern. Label syntax errors
are N38's typed TargetParsingExceptions; loading failures keep the
NoSuchPackage/NoSuchTarget classification (skipped under `--keep_going`).
`query` with no expression (QueryOptionHelper.readQuery: `missing query
expression`, or both a residue and `--query_file`) exits 2 before any loading
with nothing on stderr, as genuine Bazel 9.3 does (receipt case
error.no_expression).

## Graph semantics

Query is unconfigured. A rule's edges are, in rule-class attribute order
(`LabelVisitationUtils.visitTarget`):

* `visibility` package groups (a nodep attribute, explicit only when given);
* every dependency-typed attribute (label, label list, label-keyed dict,
  string-keyed label dict, label-list dict; output and nodep types excluded):
  plain values, defaults, Starlark computed defaults, `select()` keys (except
  `//conditions:default`) and **every** branch (a `None` branch is the
  attribute default), `$config_dependencies` (all select keys), test-rule
  implicit attributes (`$test_wrapper`, `$xml_writer`, `$test_runtime`,
  `$test_setup_script`, `$xml_generator_script`, `$collect_coverage_script`,
  `:coverage_support`, `:coverage_report_generator`), genrule's
  `$genrule_setup`, test_suite's `$implicit_tests` (an explicit value: the
  package's non-manual tests matching the suite's tags), label_flag/
  label_setting `:alias` (the build_setting_default), package
  `default_compatible_with`/`default_restricted_to`/`package_metadata`
  defaults and `configuration_field` late-bound defaults (their annotation
  default label, e.g. `@bazel_tools//tools/objc:host_xcodes`);
* the rule class's toolchain types (the hidden implicit attribute);
* aspects of an attribute (`attr.label_list(aspects=...)`, required aspects
  included) only when AspectDefinition.satisfies the dependency's rule
  class (its `provides=` providers; alias/label_flag/label_setting advertise
  any; no required providers accepts every rule): their attributes' default
  labels, as edges of the source at the next depth (fixture
  `tests/fixtures/query_language/aspects`, targeted genuine rerun in the
  receipt's `targeted_reruns`).

Input files depend on their visibility package groups, output files on their
generating rule (and its visibility groups), package groups on their
includes. Cycles are ordinary edges. `--noimplicit_deps` keeps explicitly
specified attributes only (aspect attributes and toolchain types are always
implicit), `--notool_deps`/`--nohost_deps` drops exec-configured attributes,
`--nonodep_deps` drops nodep (visibility) edges.

Missing dependencies are `ERROR: LOCATION: MESSAGE and referenced by 'L'`
(or Bazel's `every rule of type K implicitly depends upon the target ...`
for implicit attributes) followed by `Evaluation of query "E" failed: errors
were encountered while computing transitive closure` (exit 7). With
`--keep_going`/`-k` the event names the failing subexpression, the partial
result prints, `WARNING: --keep_going specified, ignoring errors. Results may
be inaccurate` follows and the exit code is 3. Pattern failures are the bare
TargetParsingException message (exit 7) or `Skipping 'P': MESSAGE` with
`--keep_going`. Syntax errors are `Error while parsing 'Q': MESSAGE` (exit 2);
an invalid `--output` and `--output=location` with `buildfiles`/`loadfiles`
are exit 2. Package evaluation failures outside Bazel's NoSuchPackage/
NoSuchTarget domain (BUILD/.bzl errors, unsupported loading features) remain
native fail-fast errors (`bazel-rs: ...`, exit 1), unlike Bazel's
containsErrors packages.

Loading is lazy: only demanded packages are evaluated. It is sequential: a
parallel prefetch was measured and removed because repository demand slots
reject concurrent demand (`concurrent repository demand is unsupported`,
`src/repository_demand.rs:52-80`) and a nested demand may cache that error.
`--loading_phase_threads` is accepted and has no effect.

## Output and ordering

Labels print in `LabelPrinter.displayForm`: main-repository labels as
`//pkg:name`, external labels with the main repository's apparent name
(`@rules_go//go:def.bzl`) — the first name in the main mapping's entry order
(RepositoryMapping.getInverse: extension imports in `use_repo` call order,
`""`, the root's repo name, bazel_deps in declaration order), otherwise
`@@canonical//...`;
`--consistent_labels` prints `@@repo//...`. (Before N39 native query printed
canonical `@@repo+//` forms for every external label.)

* `label`, `label_kind`, `package`, `location`, `build` are streamed
  formatters: with `--order_output=auto` (and
  `--incompatible_lexicographical_output`) results are sorted by label,
  except a top-level `somepath`, which prints its path in order;
  `--order_output=no` prints evaluation order.
* `--order_output=deps|full` and the non-streamed `minrank`, `maxrank`,
  `graph` use BlazeQueryEnvironment's Digraph of every visited target (plus
  intra-package edges among pattern targets), restricted to the result with
  transitive edges preserved (`Digraph.extractSubgraph`). Ordered label
  output is the full topological order (`Digraph.getTopologicalOrder(comparator)`);
  Bazel's `deps` order is an arbitrary topological order and uses the same
  deterministic order here. `minrank`/`maxrank` ranks are over the strongly
  connected component graph; `graph` is the factored DOT graph with
  `select()` condition annotations (`--graph:node_limit`,
  `--graph:conditional_edges_limit`, `--[no]graph:factored`). For
  `--order_output=auto`, Bazel iterates hash sets in these three formatters;
  native prints the sorted (`full`) order.
* `location` prints `LOCATION: KIND LABEL` (input files `PKGDIR/NAME:1:1`,
  `--relative_locations` repository-root relative).
* `build` prints every explicitly specified attribute (and `select()`s) in
  rule-class order in Starlark repr with display-form labels, then the
  instantiation call stack and the Starlark rule class's definition stack.
* `--null`/`--line_terminator_null`, `--output_file`, `--query_file`.

`xml`, `proto`, `streamed_jsonproto` and `streamed_proto` are recognized
formatter names and fail explicitly (exit 1). `--universe_scope` (non-empty)
and `--infer_universe_scope` (SkyQuery) fail explicitly.
`--experimental_graphless_query=yes` with a non-streaming output is Bazel's
exit-2 error. `--aspect_deps`, `--include_aspects`, xml/proto options are
accepted (they only affect unimplemented formatters).

## Evidence

`tests/query_language.rs` (fixture `tests/fixtures/query_language/ws`:
selects, implicit and exec dependencies, legacy macros, aliases, package
groups, cycles, test suites, missing edges) and the paired genuine Bazel
9.3.0 capture `artifacts/query-capture.py` (89 cases: every function, every
implemented formatter and ordering, the dependency-filter options, error and
keep-going cases), receipt `tests/fixtures/query_language/reference_receipt.json`:
83 EXACT, 5 EXACT_UNORDERED (Bazel hash-set orders: minrank/maxrank/graph
under `--order_output=auto`, `--order_output=deps|no`), 1
EXPECTED_NATIVE_UNSUPPORTED (`--output=xml`), no mismatches. The receipt also
records the first run (68 exact, 11 mismatches) and the fixes between runs.
`deps(//app:bin_test)` reproduces Bazel's full 62-target test-runner closure
through `@bazel_tools`, `rules_java`, `rules_shell`, `platforms` and the
`remote_coverage_tools` extension repository.

Two `--keep_going` details are Skyframe races in Bazel and approximated:
which missing edge an unbounded preload reports first (native: already loaded
packages, then missing BUILD files, then packages to load), and the order of
the visitor's repeated missing-edge events.

## Application measurement (~/code, native only)

`artifacts/query-code-measure.py` mirrors `artifacts/probe_application_v2.py`
(isolated HOME/TMPDIR, private cache, `--experimental-builtin-modules`, public
BCR, `--lockfile_mode=off`; tracked checkout state unchanged), report
`artifacts/query-code-measurement.json`. Each run spends ~80 s in the
registry fetch (tracked elsewhere).

* `deps(//src/beyondwork.ai/utils/str:str, 1) --output=label_kind`: exit 0, 10
  targets (go_context_data, the go/cpp toolchain types, rules_go config flags,
  the nogo alias, `@bazel_tools//tools/objc:host_xcodes` from the late-bound
  apple default, the function-transition allowlist).
* `deps(//src/beyondwork.ai/utils/str:str)` (label and label_kind): the next
  blocker is outside query: gazelle's `go_deps` extension, demanded by the
  rules_go closure, calls `path.dirname` on a `module_ctx` path, which the
  native module-extension path type does not implement
  (`@@gazelle+//internal/bzlmod:go_mod.bzl:437:30`).
* `rdeps(//src/beyondwork.ai/utils/..., //src/beyondwork.ai/utils/str:str)` and
  `kind(go_library, //src/beyondwork.ai/utils/...)`: package loading of
  `//src/beyondwork.ai/utils/azurefx` fails on target names with spaces and
  parentheses (`testdata/651970 Delpharm_Pack_Folding Box (1).pdf`), the
  native label character profile; `kind(go_library, //src/beyondwork.ai/utils/str/...)`
  succeeds.
