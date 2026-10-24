# Runfiles trees and `bazel run` (N23f)

N23f builds the runfiles trees of Bazel 9.3 executables and adds the `run`
command. Source paths are relative to `src/main/java/com/google/devtools/build/lib/`
of the pinned Bazel 9.3 sources (`/tmp/opencode/bazel-9.3-reference`) unless they
start with `src/main/cpp`. Implementation: `src/runfiles.rs` (the mapping),
`src/runfiles_tree.rs` (options, repository mapping manifest),
`src/action.rs` (`Transform::RunfilesManifest`/`RunfilesTree`), `src/engine.rs`
(`plan_runfiles_support`, hidden top-level trees, tool trees, `plan_request`'s run plans),
`src/analysis_runtime.rs` (tools), `src/run_command.rs` and `src/main.rs` (`run`).
The analysis model (runfiles values, DefaultInfo, FilesToRunProvider) is N23a,
docs/RUNFILES.md.

## What an executable gets

Every configured target with runfiles support (an executable or test rule
target whose default runfiles are non-empty, RunfilesSupport.create,
analysis/RunfilesSupport.java:280-347) has three native actions, planned with
its analysis and executed only when needed:

| Bazel action | native node | output |
|---|---|---|
| RepoMappingManifestAction (:683-717) | native write | `<exe>.repo_mapping` |
| SourceManifestAction SOURCE_SYMLINKS (:501-523) | `Transform::RunfilesManifest` | `<exe>.runfiles_manifest` |
| SymlinkTreeAction + RunfilesTreeAction (:525-544, 467-491) | `Transform::RunfilesTree` | `<exe>.runfiles/MANIFEST` |

`bazel build` of an executable or test target builds the runfiles tree
artifact like OutputGroupInfo.HIDDEN_TOP_LEVEL (RuleConfiguredTargetBuilder.java:144-158):
the tree (and through it the manifests) with `--build_runfile_links` (default),
only the input manifest and the repository mapping manifest with
`--nobuild_runfile_links`, only the repository mapping manifest with
`--nobuild_runfile_manifests`. Only the files to build are reported.

`FilesToRunProvider.runfiles_manifest` is `<exe>.runfiles/MANIFEST`
(`--build_runfile_links`), `<exe>.runfiles_manifest` (`--nobuild_runfile_links`)
or `None` (`--nobuild_runfile_manifests`); `repo_mapping_manifest` is
`<exe>.repo_mapping`. Both are generated files: action inputs, DefaultInfo
files and runfiles may use them (N23a failed explicitly).

## The mapping (Runfiles.getRunfilesInputs, analysis/Runfiles.java:343-401)

* Symlinks, then artifacts at their runfiles paths (later entries win; a
  replaced key keeps its first position, LinkedHashMap).
* filterListForObscuringSymlinks (:267-314) drops an entry below another
  entry; when it does not resolve to the same exec path through that
  ancestor, SourceManifestAction reports `runfiles symlink P -> E obscured by
  A -> F` at the rule's location (SourceManifestAction.java:233-273): an
  `ERROR:` under ConflictPolicy.ERROR (explicit `symlinks`/`root_symlinks`,
  docs/RUNFILES.md), a `WARNING:` otherwise. Like Bazel the build still
  succeeds (paired capture B5). Nested runfiles-tree artifacts cannot occur
  natively.
* The empty-files supplier adds empty files: BazelPyBuiltins.GET_INIT_PY_FILES
  (PythonUtils.GetInitPyFiles, rules/python/PythonUtils.java:40-109): an
  `__init__.py` in every parent directory of a `.py`/`.so`/`.pyc` file unless
  the directory is `__pycache__` or already has `__init__.py(c)`. It is
  installed by `py_internal.merge_runfiles_with_generated_inits_empty_files_supplier(ctx=, runfiles=)`
  (rules/python/PyBuiltins.java:223-250, `input runfiles cannot be empty`),
  propagated by `merge` (Runfiles.Builder.merge, :638-662) and exposed as
  `runfiles.empty_filenames`, a STABLE depset in Java `HashSet` iteration
  order (String.hashCode buckets; paired capture B4: exact).
* Paths get the workspace prefix `_main/`; `..`-relative paths of external
  repositories become siblings (`<name>.runfiles/<canonical repo>/...`;
  Bazel 9 has no legacy external runfiles). Root symlinks follow,
  `_repo_mapping` links the repository mapping manifest, and
  `_main/.runfile` (empty) is added when no entry is below `_main`.

## File formats

* `<exe>.runfiles_manifest` (SourceManifestAction.ManifestType.SOURCE_SYMLINKS,
  :369-404): sorted `path target` lines; an empty file has no target (the
  trailing space stays); a path with a space or newline (or a target with a
  newline) is escaped (`\s`, `\n`, `\b`) and the line starts with a space.
  Targets are absolute: workspace source files, the external repository's
  source directory (Bazel: `<output_base>/external/<repo>/...`; native: the
  repository's native root, e.g. a `local_path_override` directory) and
  generated files at their published path (Bazel:
  `<execroot>/bazel-out/<cfg>/bin/...`; native:
  `<cache>/actions/<key>/outputs/bazel-out/<cfg>/bin/...`).
* `<exe>.repo_mapping` (RepoMappingManifestAction.newDeterministicWriter,
  :191-278): `source,apparent,target` lines for the repositories of the
  packages the target transitively depends on, sorted by canonical name,
  restricted to apparent names (never empty) of repositories that are the
  first segment of a runfiles path (the main repository when there are
  `symlinks`, root symlinks' first segments, artifact owners' repositories),
  sorted by apparent name; the main repository is written `_main`. With
  `--incompatible_compact_repo_mapping_manifest` (default) consecutive
  repositories with equal mappings whose names agree up to their last `+`
  (one module extension's repositories) become one `prefix+*` source.
  Native transitive packages are the repositories of the configured target's
  dependency edges (attributes after select() resolution, toolchains),
  transitively, per configured target key: a select() that picks another
  repository's target in one configuration changes only that
  configuration's manifest.
* The tree (SymlinkTreeHelper.createSymlinks, exec/SymlinkTreeHelper.java:98-146):
  directories, symlinks to the absolute targets above, empty files (0644);
  `_main` always exists (createWorkspaceSubdirectory, :178-188); `MANIFEST` is a
  symlink to the input manifest (linkManifest, :167-176). `--enable_runfiles=no`
  (RunfileSymlinksMode.SKIP, BuildConfigurationValue.java:861-882) creates only
  `MANIFEST` and `_main` (createMinimalRunfilesDirectory, :148-156).
  Manifests are published `0555` like every action output.

## Native layout and caching

Bazel places the tree next to the executable in the shared output tree.
Native outputs live in per-action cache entries, so the tree node's entry
`<cache>/actions/<key>/outputs/` holds `bazel-out/<cfg>/bin/<pkg>/<name>.runfiles/`
(the tree) **and the executable itself at `bazel-out/<cfg>/bin/<pkg>/<name>`,
a hardlink of the executable's content-addressed blob** (one inode with the
published executable, N36). Started through that path (as `run` does),
`$0.runfiles`, `realpath($0)` and `/proc/self/exe` (Go's `os.Executable()`,
canonicalizing runfiles libraries) all resolve next to the tree, like Bazel's
`bazel-out` (a symlink would make the canonical path leave the tree entry). The runfiles nodes key the absolute paths their
entries link to, never input contents: a changed source keeps its link (live,
like Bazel's symlinks into the workspace), a changed generated input has a new
published path and so a new tree. Cache hits re-check every tree entry, the
MANIFEST link and the sibling's digest and mode; a damaged tree is rebuilt. The manifest
JSON records the tree (`runfiles`, an optional field of the manifest format,
`native-action-v8` since the N41 merge, `native-action-v9` since
fix-digest-memo).

## Tools with runfiles in actions

A FilesToRunProvider with runfiles in `tools` or as `executable`, or a File of
an executable attribute whose provider has runfiles
(StarlarkActionFactory.java:433-441, 765-806), attaches its runfiles tree
(StarlarkAction.Builder.addTool(FilesToRunProvider), SpawnAction.java:808-818).
Like a sandboxed spawn (SpawnInputExpander.addSingleRunfilesTreeToInputs,
exec/SpawnInputExpander.java:87-135) every mapping entry is staged at
`<tool>.runfiles/<path>` (`_repo_mapping` included, never `MANIFEST`, :212):
files as N36 hardlinks, empty files as empty files. Inside the action the
entries are regular files where Bazel's sandboxes show symlinks (and
`--spawn_strategy=local` shows the execroot symlink farm with `MANIFEST`,
capture B3L). `input_manifests` is accepted and ignored ("Legacy argument.
Ignored.", sequence or None). The same staged entries (and the empty files,
as parameter-file inputs) form the REAPI input root of the spawn's remote
cache key (N41, crate::action::remote_action): a changed runfile is a
different remote key (tests/remote_cache.rs
`tool_runfiles_enter_the_remote_input_root`). Runfiles artifacts are never
package targets, so `query` does not list them (tests/runfiles_trees.rs
`query_never_lists_runfiles_artifacts`).

### One shared input group per tool tree (PA-1)

Planning keeps a tool's tree once, as Bazel's spawns hold the tool's one
runfiles tree artifact: `tool_runfiles_tree` (crate::analysis_runtime) returns
one `Arc<ToolRunfilesTree>` per FilesToRunProvider identity while any
registration or the planner holds it, and the commit expands it once per build into a
`scheduler::InputGroup` (its planted source inputs, generated inputs, tree
and constant-metadata inputs, empty files, every source path and whether its
namespace is valid) that every planned action using the tool holds
(`PlannedAction::groups`, memoized by tree identity with the tree). Both
memos rely on one identity being one content, which is structural: a
FilesToRunProvider's fields are private to crate::runfiles (constructors
and accessors; a constructor copying an identity copies its content) and a
`ToolRunfilesTree` is only computed from its provider; debug builds check
the content on every memo hit. Identities come from a per-process counter:
they must never be persisted (a snapshot restoring identities could
collide with the fresh ones of the restoring process). The
owner-independent key every registration computes for Actions.canBeShared
(crate::shared_actions::ActionKey) takes each tool tree through the
tree's content digest (`shared_actions::tree_digest`: SHA-256 of the
framed directory and entries, computed once with the tree), not its
entries: on pythonworker that removed hashing 36.8 million entries during
analysis (`--nobuild` CPU 30.3 s -> 23.8 s). The key bytes differ from
the full encoding but keys are in-process only and compare exactly as
before (a function of the content, not the identity; fixed-width digests
in an injective framing). A group stays shared
only when listing it could change nothing but the list
(`scheduler::groups_apart`: a process action, valid groups with unrelated
roots, no path of the action itself at, above or below a root, and the
action's own part validates); otherwise the tree is listed in the action as
before. Everything observable is the listed form: the action dumps
(`BAZEL_RS_DUMP_ACTIONS`, `--native-action-dump`) print a node's own inputs
then each group's (`PlannedAction::inputs`/`all_dependencies`), the scheduler
validates exactly the listed node (`validate_node`; debug builds and
`BAZEL_RS_VERIFY_ARTIFACTS=1` also check every node it accepts without
listing in its listed form, a different result being an internal error) and a dispatched job lists
its groups in the action right before it executes (sources after its own,
generated inputs after its own, empty files re-added after a deferred command
line), so keys, staging and outputs are unchanged. On ~/code every rules_python
`py_library` registers one PyCompile action per source with the exec
interpreter as its tool: `build --nobuild //src/beyondwork.ai/pythonworker/...`
listed 36.8 million tree entries in 24,009 planned actions (27.5 GB) and now
holds 7 groups of 1,868 entries (`PERF-GROUPS` under `BAZEL_RS_TIMING`; tests
`actions_sharing_a_tool_list_its_runfiles_tree`, scheduler
`grouped_nodes_*`). Staging is per executed action as before: a cache-hit
action still checks and keys every listed input (about 20 ms for a 3,000-file
workspace-source tree, mostly the per-input ancestor checks workspace files
keep). A group's input metadata is deliberately not memoized once per build
(Bazel's once-per-build file state): the entries the cache owns (repository
trees such as ~/code's hermetic Python runtime, published outputs, blobs)
already take the per-process memos (docs/HARDLINK_STAGING.md, "Per-process
memos"), and the remaining per-use cost is exactly the full checks every
path outside the cache keeps on every use ("Trust"), which a per-group memo
would skip for runfiles entries only (PA-1 follow-up B5, not done; a
build-wide workspace file-state memo would be its own design). An aspect's actions are planned by the same commit step
(`Engine::commit_action`) and stage their tools' trees too (before PA-1
follow-up B6 the aspect commit dropped them, and the deferred completion of
a command line expanding tree artifacts; test
`aspect_actions_stage_their_tools_runfiles`, its outputs byte-identical to
genuine Bazel 9.3.0's). A planned request (docs/PLANNED_REQUEST.md, the
analysis snapshot's PLAN section) encodes each group once — a table its
holders index, the `valid` flag recomputed when decoded — and never the
trees, the memos, the identities or the action keys (integration-21).

## `run` (runtime/commands/RunCommand.java)

`bazel-rs run [build and run options] TARGET [-- ARGS...]`: `run` inherits the
`build` options and rc groups (`always`, `common`, `build`, `run`).

* The target pattern goes through `build`'s target pattern phase
  (BuildTool.evaluateTargetPatterns, buildtool/BuildTool.java:467-493;
  docs/TEST_COMMAND.md "Selecting tests"): a test suite is expanded
  (`--expand_test_suites`, `--build_tests_only` and the test filters apply)
  before RunCommand.validateTargets sees the targets, so a suite of one test
  runs that test.
* Exactly one target (`Only a single target can be run. Your target pattern P
  expanded to the targets ...`, `No targets found to run`, `Must specify a
  target to run`); it must be a file, an executable rule or an alias
  (`Cannot run target L: Not executable`, :1052-1211; a suite kept by
  `--noexpand_test_suites` is not executable). A suite's expansion error
  builds the rest, then fails with `command succeeded, but there were loading
  phase errors` and `Build failed. Not running target` (:421-426). Test
  rules run through test-setup.sh with the test environment
  (getTestCommandLine, :754-850; N23g, docs/TEST_COMMAND.md). Paired capture:
  the `run_*` cases of `tests/fixtures/test_suite`.
* The target and its runfiles tree are built (the tree also with
  `--nobuild_runfile_links`, ensureRunfilesBuilt, :996-1040);
  `--nobuild_runfile_manifests` fails with Bazel's text (:503-513). The build's
  `label<TAB>path` lines go to stderr; stdout belongs to the program.
* Command line (RunCommandLine.LinuxFormatter.formatArgv): `[shell, -c,
  "<run_under prefix> <escaped executable> <escaped args attribute> <escaped residue>"]`
  with ShellEscaper quoting; the shell is `--shell_executable` (default
  `/bin/bash`). The executable is the sibling next to the runfiles tree (or
  the file target's path). `--run_under=COMMAND-PREFIX` is supported; a label
  prefix (LabelRunUnder) fails explicitly.
* Working directory: `<tree>/_main` (`<tree>` with `--enable_runfiles=no`),
  the client's working directory with `--run_in_cwd` or without runfiles
  support.
* Environment (getCommandLineInfo, :688-752; makeMutableRunEnvironment,
  :853-860): `BUILD_WORKSPACE_DIRECTORY`, `BUILD_WORKING_DIRECTORY`,
  `BUILD_EXECROOT` (native: the tree node's `outputs` directory), `BUILD_ID`
  (a random UUID), then `--run_env` (`NAME=VALUE`, `NAME`, `=NAME`;
  Converters.EnvVarsConverter, the parser of `--action_env`,
  crate::action_env::EnvVar), then the target's RunEnvironmentInfo
  `environment` and `inherited_environment` (ActionEnvironment.resolve).
  `JAVA_RUNFILES`, `RUNFILES_DIR`, `RUNFILES_MANIFEST_FILE`,
  `RUNFILES_MANIFEST_ONLY` and `TEST_SRCDIR` are cleared (:214-224). Native
  applies them like the client (src/main/cpp/blaze.cc:2166-2195): the client
  environment, minus the cleared variables, plus the run environment.
* The process replaces `bazel-rs` (exec): exit code, stdin and terminal pass
  through. `bazel-rs: Running command line: <escaped command> <args omitted>`
  goes to stderr first (`--noomit_run_args` shows the arguments; `--norun`
  prints `Runnable command line` and does not run).
* `--script_path=PATH` writes LinuxFormatter.getScriptForm (no trailing
  newline) relative to the working directory, adds the executable bits and
  does not run (handleScriptPath/writeScript, :653-686, 1042-1049); the
  script's environment is the run environment only.

## Paired capture

`artifacts/runfiles_trees-capture.py` builds and runs the fixture of
`tests/runfiles_trees.rs` with genuine Bazel 9.3.0 (`--batch`, offline, default
strategy plus one `--spawn_strategy=local` reference row) and natively; the
receipt is `tests/fixtures/runfiles_trees/reference_receipt.json` (run 2, the one
allowed rerun after harness fixes: 75 EXACT, 6 MISMATCH, 5 UNAVAILABLE,
1 EXPECTED_NATIVE_UNSUPPORTED, 1 REFERENCE_ONLY). Exact: every tree listing
with normalized symlink targets (main and external repositories, symlinks,
root symlinks including `ext+/rooted.txt`, merged runfiles, Python empty
files, obscured entries, `--enable_runfiles=no`), every `MANIFEST` link,
input manifest bytes and mode, every `_repo_mapping`, the empty filenames, the
conflict `ERROR:` line with exit 0, tool runfiles seen inside actions, the
manifests as action inputs, `--nobuild_runfile_links`/`--nobuild_runfile_manifests`
artifacts and error, `run` stdout/exit code/cwd/env/argv0/stdin without
colliding client variables, `--run_in_cwd`, source-file targets, validation
messages, and the `--script_path` scripts (also with `--run_under`) and their
execution. The mismatches, all explained in the receipt:

| case | Bazel | native | cause |
|---|---|---|---|
| `Running command line:` (3), `Runnable command line:` | `../bazel-bin/pkg/b` | absolute native path | Bazel pretty-prints through convenience symlinks; native has none |
| R1 stdout | client `RUNFILES_DIR` and the `--run_env==` variable survive | cleared | `--batch` puts the client environment over the run environment (RunCommand.java:320-327); native follows the client/server contract |
| R6 `--norun` stdout | runs the target | does not | `--batch` ignores `should_exec` (BlazeRuntime.java:1137-1180) |

Unavailable: a genuine rules_shell `sh_binary` (and the test-target row, whose
test toolchain needs rules_shell): the offline reference could not fetch
`@@rules_shell+` (seeded repository-cache entries lacked the default canonical
id marker). Natively a genuine `sh_binary` was not feasible then (attribute
flag `SKIP_CONSTRAINTS_OVERRIDE`, `attr.label_keyed_string_dict` values); N50
closed both, and on integration-15 `bazel run` of N50's genuine `sh_binary`
targets matches the recorded genuine runfiles trees, MANIFEST paths,
`_repo_mapping` bytes and output (docs/SH_BINARY.md).

## Not covered

`--run_under` labels (`bazel test` and test targets under `run` are N23g,
docs/TEST_COMMAND.md); Bazel's
convenience symlinks and relative pretty paths; `--portable_paths` and
`--emit_script_path_in_exec_request`; `args` needing Make/location expansion or
quote/escape tokenization (docs/RUNFILES.md); fileset trees, unresolved
symlink artifacts in runfiles, output services; Windows (no runfiles trees,
manifest-only runfiles).
