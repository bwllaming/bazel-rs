# Unresolved symlink artifacts (execution)

Bazel 9.3 semantics, cited from the pinned sources
(`src/main/java/com/google/devtools/build/lib/...`), and the native model.
Analysis (`ctx.actions.declare_symlink`, `ctx.actions.symlink(target_path=...)`,
`File.is_symlink`, the argument errors) is described in docs/ACTIONS_PARAMS.md.

## Bazel 9.3

- `--allow_unresolved_symlinks` (old name `--experimental_allow_unresolved_symlinks`)
  defaults to **true** (`analysis/config/CoreOptions.java:877-893`); it is not a
  modelled native option, its default is the native behaviour.
- **Creation.** `UnresolvedSymlinkAction.execute`
  (`analysis/starlark/UnresolvedSymlinkAction.java`) runs
  `outputPath.createSymbolicLink(PathFragment.create(target), targetType)`: the
  link text is the *normalized* target (`vfs/UnixOsPathPolicy.normalize`,
  `OsPathPolicy.removeRelativePaths`: repeated/trailing `/` and `.` segments
  dropped, `x/..` removed, leading `..` kept for relative and dropped for absolute
  paths). Its key is GUID + target + target type (`computeKey`), no inputs; a
  dangling target is fine. A failure is `failed to create symbolic link 'P' with
  target 'T' due to I/O error: ...`.
- **Spawn outputs.** A spawn may create a declared symlink. The output's metadata
  is read with readlink, never followed
  (`skyframe/ActionOutputMetadataStore.fileArtifactValueFromArtifact` ->
  `FileArtifactValue.createForUnresolvedSymlink`); anything else at the path is
  `declared output 'P' is not a symlink` (`SkyframeActionExecutor.checkOutputs`,
  NotASymlinkException), followed by `... failed: not all outputs were created or
  valid`.
- **Value.** `FileArtifactValue.UnresolvedSymlinkArtifactValue` (`:1145-1190`):
  the link text, and as digest the SHA-256 of the text (ISO-8859-1) with the first
  byte inverted (so no file holding the text has that digest). Consumers' action
  keys and the action cache see that value.
- **Staging.** Sandboxes stage an unresolved symlink input as a symlink with its
  literal text (`sandbox/SandboxHelpers.processInputFiles` -> `SandboxInputs`
  symlinks); unsandboxed spawns see the artifact in the execution root. A tool's
  runfiles are staged at their runfiles paths the same way
  (`exec/SpawnInputExpander`).
- **Runfiles.** Runfiles trees create unresolved symlinks textually
  (`exec/SymlinkTreeHelper.createRunfilesSymlinks`), so a relative text resolves
  inside the runfiles tree (rules_python's venv: `_X.venv/bin/python3 ->
  ../../../../../../rules_python++python+python_3_12_x86_64-unknown-linux-gnu/bin/python3`).
  SourceManifestAction has the runfiles' symlink artifacts as its inputs and
  writes the link text as their target (`analysis/SourceManifestAction.java:
  190-215, 301-312`).
- Unresolved symlinks inside tree artifacts are not supported by Bazel either
  (the option's help text); symlinks in trees are followed (docs/TREE_SYMLINKS.md).

## Native model

- `ActionExtra::symlink_outputs` (outputs that are declared symlinks) and
  `ActionExtra::symlink_inputs` (input destinations that are symlink artifacts,
  marked by the planner from `File.is_symlink`, also inside a tool runfiles
  tree's shared input group, `InputGroup::symlink_inputs`).
- Creation: `Transform::UnresolvedSymlink` creates the symlink with
  `action::unresolved_link_text(target)` (PathFragment.create). A spawn's symlink
  output must be a symlink after the process (`declared output 'P' is not a
  symlink` otherwise).
- Publication and the action cache: a symlink output is published as the symlink
  itself (renamed, never followed); its manifest entry (the existing
  `native-action-v9` JSON manifest, no new store) records the link text
  (`symlink`, hex) and Bazel's digest of it (`digest`). A cached entry is valid
  only while the published path is still a symlink with that text and digest.
- Inputs: `Staged::Symlink(text)` read with readlink (an input that is not a
  symlink fails), keyed by its text (`unresolved-symlink` frame); only actions
  with symlink inputs or outputs frame the extra `unresolved-symlinks` block, so
  every other action key is unchanged. Staged as a symlink with the literal text
  in the action's execution root, which serves sandboxed and standalone spawns
  alike.
- Runfiles: the runfiles tree transform links a symlink entry with its text, the
  runfiles manifest writes the text; both read the text (keyed) although they
  otherwise read paths only. `test` stages a test's symlink runfiles with their
  text (`RunTarget::symlinks`, `TestSpec::symlinks`); `run` uses the runfiles
  tree.
- Remote cache: a spawn with an unresolved symlink input or output does not use
  the remote cache yet (`Remote Cache: not used for ...` warning; it runs
  locally): input-root SymlinkNodes and `output_symlinks` are not modelled.

## Evidence

`tests/fixtures/unresolved_symlinks` (genuine Bazel 9.3.0 paired capture, 26
cases, all MATCH; `tests/unresolved_symlinks.rs` runs them live): created links
(relative, dangling at creation, nested, spawn-created), consumers reading them
with `readlink` sandboxed and standalone, a link resolving in the execution root
into an external repository, `run` listing the runfiles tree's symlinks, a
rules_python-like venv (`_venv_tool.venv/bin/python3 -> ../../../../pyrt+/bin/python3`
next to a declared `pyvenv.cfg`) used by `run`, as a spawn's tool (sandboxed and
standalone) and by a test, its runfiles manifest line, and a spawn leaving a
regular file at a declared symlink's path. Unit tests in `src/action.rs`
(creation, normalization, digest, cache validation, consumers' keys, runfiles
manifest, spawn outputs).
