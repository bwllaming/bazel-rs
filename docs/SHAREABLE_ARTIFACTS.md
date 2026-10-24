# Shareable artifacts and rules_cc virtual includes

`ctx.actions.declare_shareable_artifact` is the builtins-only artifact factory
rules_cc 0.2.17 uses for the `_virtual_includes` headers of
`strip_include_prefix`/`include_prefix` (cc/private/compile/
cc_compilation_helper.bzl `_compute_public_headers`), the header modules and
the LTO/linkstamp outputs. Analysing
`@@protobuf+//src/google/protobuf:internal_visibility` (`strip_include_prefix =
"/src"`) failed with `'actions' value has no field or method
'declare_shareable_artifact'`. Citations: `J =
src/main/java/com/google/devtools/build/lib` of the pinned
`/tmp/opencode/bazel-9.3-reference` tree.

## declare_shareable_artifact(path, artifact_root=unbound)

`StarlarkActionFactoryApi` (:1142-1151) declares both parameters
positional-only and `path` a string; the binding errors are Bazel's
(`declare_shareable_artifact() got named argument for positional-only
parameter 'path'`, `... accepts no more than 2 positional arguments but got
3`, `in call to declare_shareable_artifact(), parameter 'path' got value of
type 'int', want 'string'`). `StarlarkActionFactory.createShareableArtifact`
(J/analysis/starlark/StarlarkActionFactory.java:1267-1276), natively
`crate::analysis_runtime` (`declare_shareable_artifact`,
`Context::declare_shareable`):

- `BuiltinRestriction.failIfCalledOutsideDefaultAllowlist` first: the
  innermost `.bzl` must be `@_builtins` or match
  `INTERNAL_STARLARK_API_ALLOWLIST` (rules_cc, protobuf, the main repository's
  `test` package, ...), else `file '//denied:defs.bzl' cannot use private
  API`. `ctx.actions.symlink(use_exec_root_for_source=...)` is restricted the
  same way whenever the parameter is passed, even `False` (:270-275).
- The artifact root is `artifact_root` or the bin directory of the target's
  configuration (`RuleContext.getBinDirectory`, :319-321); `ctx.bin_dir`,
  `ctx.genfiles_dir` and a generated File's `root` are accepted.
- `path` is relative to that root, not to the package, normalized like
  `PathFragment.create` (`test/./sub2/../x` is `test/x`). No package check
  applies: a path outside the package or below a subpackage is fine, so the
  engine's package-relative output validation skips these declarations
  (`Context::shareable`, filtered out of `AnalysisResult::declared`).
- The rule owns the artifact (`owner`), `short_path` is the runfiles path
  (`external/r/x` is `../r/x`), `root` the artifact root. The runfiles path
  of a generated File does not depend on its owner (`Artifact.getRunfilesPath`
  under the legacy output layout: J/actions/Artifact.java:636-662), so a
  main-repository rule's `external/somerepo/x.txt` is `../somerepo/x.txt`.
  That is the one short-path rule of every generated File
  (`crate::label::generated_short_path`): declared and predeclared outputs and
  the engine's Artifact of an output compute the same File, so a consumer gets
  one File through `ctx.files`, `DefaultInfo` or any provider field (the
  commit checks `declared File == Artifact File` in debug builds and under
  `BAZEL_RS_VERIFY_ARTIFACTS=1`). Source Files keep their rule (a source root
  is not a legacy root). The same exec path
  declared twice, or once with `declare_file`, is one artifact
  (`CachingAnalysisEnvironment.dedupAndTrackArtifactAndOrigin`, :241-259);
  without a generating action it is an orphan (`The following files have no
  generating action:`).

## Shared and conflicting actions

`Actions.canBeShared` (`J/actions/Actions.java:85-115`) lets two identical
actions generate one output: same mnemonic, equal action key
(`ActionKeyComputer.getKey`, `J/actions/ActionKeyComputer.java:37-57`: the
action's `computeKey` — for a spawn the command lines by exec path, the
mnemonic, the environment and the execution info with the propagated tags,
`J/analysis/actions/SpawnAction.java:396-417` — plus the execution platform
and the exec properties), and equal mandatory inputs and outputs compared
without their owner (`Artifact.equalsWithoutOwner`, exec path and root, in
order). The owner, the progress message and the configuration checksum are
never compared. It applies within one target
(`Actions.assignOwnersAndThrowIfConflictToleratingSharedActions`: a rule
writing the same content twice, two identical `run_shell` spawns, rules_cc
registering two symlink actions for a header listed in both `hdrs` and
`textual_hdrs`) and between targets (`MapBasedActionGraph.registerAction`,
`J/actions/MapBasedActionGraph.java:46-63`: two `py_library` targets
precompiling one `__init__.py`, rules_pkg's `//pkg/private:archive` and
`:helpers`). A shared action runs once; each target keeps its own Artifact
(`File.owner` is the declaring target).

Natively (`crate::shared_actions`) every registered action gets one
owner-independent `ActionKey` at registration
(`Context::register_action`): a SHA-256 over a framed encoding of the
native action without its owner-numbered id, a symlink's progress message
and the owner-specific remote gate (destructured exhaustively, so a new
action field must choose), the rule's propagated execution-requirement tags
on spawns, the inputs and outputs as (exec path, directory), each tool's
runfiles tree as its directory and mapping (runfiles path -> exec path), and
the action's exec group resolved to its execution platform (none without
toolchain resolution) and exec properties — the group's name is not keyed.
The same key decides both cases, so they cannot drift:

- within one target, a second registration of an output with an equal key
  is the first;
- across targets, the engine's producer map (`register_output`) keeps each
  output's producer with its action id and key: an exact path whose
  producer has an equal key is that action — the later target's outputs are
  its own Artifacts produced by the first owner's action, the later target
  plans no node for it, and the first target in commit order owns it (the
  local action key hashes the action id). A consumer may take both targets'
  Files of the output as inputs: inputs at one exec path are one artifact
  without owner (one producing action and index). An output-file target
  shared with another target's declared file resolves to its predeclaring
  rule's File;
- a difference in a compared part is the first lines of Bazel's
  `ActionConflictException`, `file 'pkg/__pycache__/__init__.tag.pyc' is
  generated by these conflicting actions:` / `Label: //pkg:c_args, //pkg:a`
  (attempted, then previous; genuine's order depends on analysis timing);
  native producers (genrules, runfiles support, workspace status) are never
  shared, so an exact path they produce is a conflict too;
- an output below or above another producer's output stays an error
  (`overlapping output paths: ...`);
- equal in every compared part, but either action has a part the native key
  does not cover: contents or command lines that expand tree artifacts when
  the action executes (Bazel keys the unexpanded directory, natively a
  closure), or build-info translation callbacks. Such actions may be
  identical (Bazel shares them) or not (a conflict); neither is guessed:
  `file 'share/args_same.args' is generated by two actions of
  //share:args_same: sharing identical actions with command lines or
  contents expanding tree artifacts is not implemented in the native profile
  (Actions.canBeShared, docs/SHAREABLE_ARTIFACTS.md)` (across targets:
  `... is generated by actions of //b and //a: ...`).

## The rules_cc virtual-include path

With `declare_shareable_artifact` and `use_exec_root_for_source` the genuine
rules_cc Starlark runs unchanged: each public (and textual) header under the
strip prefix gets `<package exec path>/_virtual_includes/<name>/<include path>`
below the bin root, a SymlinkAction to the source file's path (the workspace
file; an external repository's file is genuinely below
`<output_base>/external/<repo>`, natively in the repository's directory, e.g.
a `local_path_override` directory itself), and
`bazel-out/<cfg>/bin/<package>/_virtual_includes/<name>` joins the
CcCompilationContext's include directories, so dependents compile with
`-I` on it and their `.d` files name the virtual headers. Header-only targets
build nothing; the symlinks are created when a compile needs them.

## Verification

`tests/cc_virtual_includes.rs` replays the paired genuine Bazel 9.3.0 capture
`tests/fixtures/cc_virtual_includes/reference_receipt.json` (31 cases: 24
MATCH, 7 declared divergences): cc_library with relative, repository-absolute
and external (`@ext//src/ext`, `strip_include_prefix = "/src"`) strip
prefixes, `include_prefix`, textual headers, a consumer cc_library and a
cc_binary — every object, dependency file, archive, shared library and the
linked binary byte-identical, every `_virtual_includes` symlink pointing to the
same file — plus the API probes and misuse cases above, the `//share`
package's sharing within one target (identical `run_shell` spawns and
identical `ctx.actions.run` registrations of a tool with runfiles shared,
registrations with different arguments conflicting, and the declared case
below), two targets on one shareable path (`share_same` shared, `share_diff`
a conflict naming both labels), and `consumed_external_path` (integration-20, the
review F2 repro): `//test:external_producer` declares
`external/somerepo/x.txt` and `//test:external_consumer` reads it through
`ctx.files`, `DefaultInfo` and a provider field holding the producer's own
File — one File every way (equal, one depset element), short_path
`../somerepo/x.txt`, and an action input either way.

`tests/shared_actions.rs` replays `tests/fixtures/shared_actions/reference_receipt.json`
(25 cases: 22 MATCH, 3 declared divergences), sharing between targets: the
rules_python precompile shape (`declare_file(sibling = src)`, a tool with
runfiles in the exec configuration, a param file, env and execution
requirements, per-target progress messages) shared by two targets, built and
`--nobuild`, with a consumer of both Files (two Files, one path, different
owners, both action inputs); plain tags and exec groups of one platform
shared; a declared file shared with an output-file target (owner of the
label's File: the predeclaring rule, either analysis order); conflicts for
each compared part — arguments, an execution-requirement tag, mnemonic,
environment, inputs, tool, exec properties of the target or of the group,
write contents — with both labels; two applications of one aspect sharing
a spawn on one path although their base targets' exec_properties differ (an
aspect's RuleContext takes no exec_properties of its base target,
`ConfiguredTargetFactory.createAspect`'s `setExecProperties(ImmutableMap.of())`,
`J/analysis/ConfiguredTargetFactory.java:702`) and conflicting when the spawns
differ; a shared tree artifact with a consumer of both Files; a shared output
as a runfile of an executable that depends on both owners, built and `run`
(the program reads it through the runfiles tree); identical Args writes
expanding a tree artifact across targets (the declared not-implemented
error, genuine shares); and the planned graph of a shared action (one node,
owned by the first target in commit order, one dependency of the consumer),
also for the aspect commit path and the tree artifact. Every build runs with
`BAZEL_RS_VERIFY_ARTIFACTS=1` and `BAZEL_RS_VERIFY_VIEWS=1`.

## Known differences

- Misuse that crashes Bazel 9.3 (exit 37) is an explicit native analysis
  error naming the Java exception: an `artifact_root` that is not a root
  (`ClassCastException`) or a source root, an absolute path, uplevel
  references (`IllegalArgumentException` in `ArtifactFactory.validatePath`).
  The empty path (the root itself; genuine fails on the output prefix
  conflict) is rejected at declaration.
- Identical registrations with a part the native key does not cover are an
  explicit error natively, genuine shares them: two identical writes of Args
  content expanding a tree artifact (`share_args_same`), and the same across
  targets (shared_actions `expand_shared`: `file 'pkg/expand.args' is
  generated by actions of //pkg:expand_b and //pkg:expand_a: ...`).
- An output below another target's output fails with the native
  `overlapping output paths` text, genuine with
  `Actions.findArtifactPrefixConflicts`' `One of the output paths ... is a
  prefix of the other` (shared_actions `prefix`).
- A conflict stops the native build at the first conflicting target with
  Bazel's first two lines; genuine reports every conflicting target with
  its further action-description lines. A conflict found while committing
  an aspect application carries the consuming rule's location natively
  (the location every aspect-dependency error takes), genuine none.
- Native producers (genrules, runfiles support, workspace status) are never
  shared: an identical genrule action of the same target in two
  configurations with one output directory would conflict natively.
- `use_exec_root_for_source=True` links a source file itself: the native
  profile has no persistent execution root (rules_cc passes `False`).
