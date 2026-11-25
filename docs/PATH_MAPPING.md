# Path mapping: `--experimental_output_paths=strip` (B33)

Genuine Bazel 9.3.1 (`analysis/actions/PathMappers.java`,
`analysis/actions/StrippingPathMapper.java`, `actions/PathMapper.java`,
`actions/PathMapperConstants.java`) runs the actions that support path
mapping with the configuration segment of every output path replaced by
`cfg`: `bazel-out/k8-fastbuild-ST-0695294e3b4b/bin/pkg/g.txt` becomes
`bazel-out/cfg/bin/pkg/g.txt` in their command lines, and their inputs are
staged and their outputs produced at those paths. Two configurations that
build the same thing then run the same spawn: one remote/disk cache entry.
This engine implements it (`src/path_mapping.rs` and its callers); the
genuine capture is `tests/fixtures/path_mapping` (replayed by
`tests/path_mapping.rs`).

## The option

`--experimental_output_paths` (`off`, `strip`; CoreOptions.outputPathsMode)
is a configuration option (crate::configuration VALUE_OPTIONS): set on the
command line or by a Starlark transition (protobuf's and rules_java's
analysis tests), propagated to exec configurations
(builtin_exec_platforms.bzl:236), part of the ST- hash like any changed
option. `--modify_execution_info` written by a Starlark transition (protobuf's
`test_compile_path_mapping` adds `supports-path-mapping` that way) is that
configuration's value (`NativeOptions::modify_execution_info`, the exec
transition copies it, ExecutionTransitionFactory.java:261-262; its ST- hash
form is each ExecutionInfoModifier's AutoValue `toString`); without a
transition the command line's applies to every spawn as before.

## Which actions are mapped

PathMappers.getEffectiveOutputPathsMode: an action of a `strip`
configuration whose final execution info (the rule's propagated tags, the
`execution_requirements`, `--modify_execution_info` applied) has
`supports-path-mapping` — or whose mnemonic is one of PathMappers'
SUPPORTED_MNEMONICS (Android tools, `Mock action`) — unless it has `local`,
or both `no-sandbox` and `no-remote`. Its inputs and outputs then decide
(StrippingPathMapper.isPathStrippable): two inputs from different
configurations with the same root-relative path (`bin/pkg/g.txt`), or an
input with an output's, must have equal contents, else the action runs
unmapped. At analysis (`Target.actions`, aquery) no contents are known and
any such pair of distinct paths keeps the command line unmapped; at
execution the staged inputs' digests decide (tree inputs by their whole
listing).

## What is mapped

- **Args** (`ctx.actions.args()` of a `strip` configuration): every value
  is recorded twice, unmapped and mapped (crate::command_line Group::Mapped,
  DeferredVector::mapped_items). A derived File argument is its stripped
  path (getMappedExecPathString; source files and strings never change: a
  hard-coded `f.path` string stays unmapped, as in genuine). `map_each` runs
  a second time with File attributes reporting mapped paths
  (PathMapper.storeIn: `path`, `dirname`, `root.path`; crate::path_mapping
  Mode, a thread-local the File's `get_attr` reads); a `map_each` failing only
  under the mapping fails only a mapped command line. Tree artifacts expand
  to mapped child paths, their `map_each` under the mapping too. `uniquify`
  and the formatting apply to the mapped values.
- **The executable**: a File executable (or a string executable that is a
  normalized path) is mapped; a shell command (`run_shell`, `/bin/bash -c`)
  never is.
- **Parameter files**: their paths (`<primary output>-N.params`) and
  contents are mapped, and their spill threshold uses the mapped lengths
  (CommandLines.expand).
- **The environment** is never mapped.
- **Android mnemonics** (StrippingPathMapper.mapCustomStarlarkArgs): the
  value after `--data`, `--resources`, `-i`, ... in an Args chunk is
  stripped heuristically (every `bazel-out/<segment>/`).
- **ctx.actions.write of an Args** (ParameterFileWriteAction): with
  `supports-path-mapping` in its `execution_requirements` it writes the
  mapped command line; `Target.actions` shows the unmapped content and aquery
  the `{supports-path-mapping: ''}` execution info.

## Execution

`crate::action::prepare` decides (the effective mode, then the staged
inputs' collisions); a mapped action executes its mapped spawn
(`mapped_spawn`): the mapped program, arguments and parameter files, every
input staged at its mapped destination (a tool's runfiles tree at
`bazel-out/cfg/bin/pkg/tool.runfiles/...`, colliding equal inputs once),
outputs and tree output directories created and read at their mapped
paths, then published under the action's own output paths. Strategies: the
`standalone` strategy cannot run a mapped spawn (LocalSpawnRunner.canExec;
genuine's "which requires sandboxing due to path mapping" error), a
persistent worker for it is sandboxed (WorkerParser.createWorkerKey's
mustSandbox), and a failed remote execution does not fall back to local
execution. The JSON execution log shows the mapped argv and staged inputs
and the action's own output paths, like genuine's.

## Keys

- **Remote and disk cache**: the REAPI Action of the mapped spawn (its
  argv, the input root at mapped paths, the mapped output paths) is
  genuine's: two configurations share one entry (the capture's `build_b`
  is a disk cache hit of `build_a`'s spawn, digest equal to genuine's).
- **The native action cache** keys the action as before (its own inputs,
  outputs and command line) plus, for a mapped execution only, a block with
  the mapper's GUID and the mapped argv and parameter files. Distinct
  configurations keep distinct entries, like genuine's action cache
  (PathMapperConstants.FOR_FINGERPRINTING keeps the configuration segment);
  sharing across configurations goes through the remote/disk cache, as in
  genuine.
- **aquery's action key** (SpawnAction.computeKey): the map_each results
  under FOR_FINGERPRINTING (`bazel-out/pm-<cfg>/...`), the executable
  path likewise, then StrippingPathMapper.GUID and the empty additional
  artifacts (PathMappers.addToFingerprint); a ParameterFileWriteAction's
  map_each results likewise.

## Soundness

Everything the executed spawn depends on is keyed: the unmapped command
line, inputs (by destination and digest), outputs and execution info are
framed as before; the mapping decision is a function of the execution
requirements (framed), the rule tags (not framed, but a mapped execution
frames its own block, so a mapped and an unmapped execution never share a
key) and the inputs' digests (framed); the mapped staging layout and output
paths are functions of the framed destinations; the mapped argv and
parameter files are framed whenever they are executed. Equal colliding
inputs are staged once, the first in destination order. Analysis-time reads
are the configuration (its identity covers both options) and this command's
`--modify_execution_info` (in the analysis snapshot key).

## Not modelled (explicit)

- Native process actions that support path mapping — Javac, Turbine
  (JavacTurbine) and JavaResourceJar always do — fail explicitly when they
  would run mapped (`path mapping (--experimental_output_paths=strip) of the
  native Javac action is not modelled in the native profile`): their
  command lines (JavaCompileAction's flag mapping, the `.jdeps` rewriting
  back to unmapped paths) are native. Their `Target.actions` and aquery
  command lines stay unmapped (genuine maps Turbine's and JavaResourceJar's)
  and their aquery keys differ. Java builds without `strip` are unaffected.
- A CppLink whose `cc_internal.get_link_args` command line would run mapped
  (rules_cc's opt-in `cpp_link_path_mapping` feature) fails explicitly: the
  expanded link variables cannot tell artifacts from strings
  (CcToolchainVariables' mapHeuristically).
- `--modify_execution_info` set by a transition applies to the Starlark
  actions of that configuration; native actions keep the command line's.
- Literal string runs in a spawn's `arguments` list are custom-stripped
  like an Args chunk for the Android mnemonics (genuine strips Args chunks
  only).
- The process summary's action cache counts follow the native action
  cache (shared across output bases), not genuine's.
