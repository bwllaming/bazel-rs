# `mod`

Native `mod` is Bazel 9.3's ModCommand (`bazel/commands/ModCommand.java`)
with the `bazel/bzlmod/modcommand` package: ModOptions, ModuleArg,
ExtensionArg, ModExecutor and the text, JSON and Graphviz formatters. Code:
`src/mod_command.rs` (options, the inspection graph, the extension usage
table, the subcommand dispatch), `src/mod_command/args.rs` (module and
extension arguments), `executor.rs` (graph/deps/path/all_paths/explain and
the result-graph pruner), `output.rs` (formatters), `repos.rs` (show_repo,
show_extension, dump_repo_mapping), `tidy.rs`.

## Profile

`mod` runs on the invocation's repository set like every other command: the
built-in modules (`@bazel_tools` and its closure) are part of the graph by
default (`--experimental-builtin-modules=false` selects the explicit-module
profile), every lockfile mode works (`--lockfile_mode`, `--native-lockfile`),
the registry session and the rc files apply, and the output base lock is held
shared. The inspection graph (BazelModuleInspectorValue's AugmentedModules:
dependants, original dependants, used and unused dependencies, resolution
reasons) is built from the resolver's un-pruned dependency graph
(`module_graph::ResolvedGraph::inspection`, BazelModuleInspectorFunction
.computeAugmentedGraph), so `--include_unused`, `--verbose` and `explain`
report the modules selection discarded and why (MVS, single/multiple version
override, non-registry override).

Subcommands: `graph`, `deps`, `all_paths`, `path`, `explain`, `show_repo`,
`show_extension`, `dump_repo_mapping`, `tidy`. Options: `--output=text|json|graph`
(`show_repo`: text), `--charset=utf8|ascii`, `--depth`, `--cycles`,
`--include_unused`, `--include_builtin`, `--verbose`, `--from`,
`--base_module`, `--extension_info=hidden|usages|repos|all`,
`--extension_filter`, `--extension_usages`, `--all_repos`,
`--all_visible_repos`. Argument and option errors are genuine's
(`... Type 'bazel help mod' for syntax and help.`, exit 2; an unknown or
missing subcommand and a module resolution failure exit 37; an option
conversion error is `While parsing option ...`, exit 2).

## Differences from genuine 9.3

- Extension evaluation is demand-driven. Genuine evaluates every module
  extension of the graph for every graph subcommand
  (BazelModuleInspectorFunction.computExtensionRepos), only to know their
  generated repositories; native evaluates (or reuses from the lockfile) only
  the extensions whose repositories the output shows: `--extension_info=all`
  (the extensions of the displayed modules), `show_extension`, `show_repo` of
  a generated repository and `show_repo --all_repos`. The output is the same;
  a failing extension is reported (`Results may be incomplete as N
  extension(s) failed.`, exit 2) only when it is evaluated, and the lockfile
  gains only the extensions that were evaluated.
- `show_repo --output=streamed_proto|streamed_jsonproto` (Build.Repository
  protos with every rule attribute and its default) is not implemented:
  explicit error, exit 2.
- `show_repo @bazel_tools` prints the native embedded tree's directory where
  genuine prints its install base's `embedded_tools`.
- The `--check_direct_dependencies` warning genuine prints after resolution
  (all commands) is not printed.
- An isolated usage's extension identity uses the usage's file-local index
  where genuine uses its exported proxy name (not recorded natively).

## Validation

`tests/mod_command.rs` replays the genuine capture
`tests/fixtures/mod_command/reference_receipt.json` (`capture.py` there; 74
cases on a scratch graph with an MVS upgrade, single-version and local-path
overrides, a cycle, an extension with a used and an unused repository and a
`use_repo_rule`): stdout byte for byte, exit codes and ERROR events. The
genuine capture resolved the built-in closure from the BCR, the test from the
offline `tests/fixtures/builtin_modules` registry. `tests/builtin_modules.rs`
compares `mod graph --output=json --include_builtin` with the genuine graphs
of the built-in closure fixtures.
