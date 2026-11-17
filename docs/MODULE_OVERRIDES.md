# MODULE overrides: `override_repo`, `inject_repo`, `archive_override`, `git_override`

Pinned source: `/tmp/opencode/bazel-9.3.1-reference` (Release 9.3.1); Java
paths are relative to `src/main/java/com/google/devtools/build/lib/bazel/bzlmod/`.
Paired receipt: `tests/fixtures/module_overrides/reference_receipt.json`
(`tests/fixtures/module_overrides/capture.py`, genuine 9.3.1; local file://
archive and local Git remote), checked by `tests/module_overrides.rs`.

## `override_repo` / `inject_repo`

`ModuleFileGlobals.overrideRepo` / `injectRepo` (:716-800) record
`extension-internal name -> apparent name in the root module` on the usage of
the extension proxy. They return before recording anything in a non-root
module or under `--ignore_dev_dependency` (`shouldIgnoreDevDeps`), so BCR
modules that call them (rules_swift 4.1.1, hermetic_launcher 0.0.15) are
unaffected. Native: `bzlmod::repo_override`, `ExtensionUsage::repo_overrides`.

Checks, with Bazel's texts (receipt `repo_override.variants`):

- during evaluation: the proxy type, valid repository names, a second
  override of the same name (`... is already overridden with '...' at LOC`);
- at the end of the MODULE file (ModuleThreadContext.buildUsage /
  buildModule): the overriding name must be visible in the root module
  (SpellChecker suggestion over the HashMap iteration order of the names);
  an `inject_repo`'d name must not be imported by `use_repo` (`Cannot import
  repo ...`, reported at the `use_repo`); a repo cannot both override and be
  overridden;
- after the extension ran (SingleExtensionFunction.java:76-105): an
  `override_repo` name the extension does not generate, an `inject_repo`
  name it does generate. Imports of overridden names need no generated repo.

Effects (`repository_catalog::apply_repo_overrides`,
BazelDepGraphFunction.getRepoOverrides): the overriding apparent name
resolves in the root module's mapping without overrides; then

1. every module's `use_repo` import of an overridden name maps to the
   overriding repository (BazelDepGraphValue.getRepositoryMapping);
2. the extension's generated repositories see the override last in their
   mapping (ModuleExtensionRepoMappingEntriesFunction), and label attributes
   of the repositories it declares convert with it
   (ModuleExtensionEvalStarlarkThreadContext.createRepos). The overridden
   repository itself is still generated under its own canonical name.

The lockfile usages digest (SingleExtensionUsagesValue.trimForEvaluation)
carries `repoOverrides`: internal name -> canonical name, in the iteration
order of the `HashMap` ModuleThreadContext collects them in
(`lockfile_extensions::java_hash_map_order`). The receipt's lockfile entry is
byte-identical.

Not implemented: overrides on isolated usages (explicit failure).

## `archive_override`

Bazel 9.3 forwards every keyword to an `http_archive` repo spec
(ModuleFileGlobals.archiveOverride) and fetches it during discovery to read
its MODULE.bazel. Native (`materializer::materialize_override`, called from
module discovery like a Git override): download (`url` then `urls`,
http.bzl `_get_source_urls`), verified against `integrity` or `sha256` when
given, extraction with `strip_prefix` and `type`, `build_file_content`, then
the root module's `patches` (native patcher, `patch_strip` or `-pN`
`patch_args`) and `patch_cmds` (`bash -c` in the repository root, Bazel's
`Error applying patch command` text) — utils.bzl `patch` order. Without a
checksum the fetch prints genuine's two reproducible-form DEBUG lines. The
tree is published in the repository store keyed by these attributes and the
patch contents, and later commands reuse it after its manifest check (like
Bazel's marker-file reuse of a fetched repository; nothing is printed then).
The lockfile is unaffected (no registry files).

Attributes without a native effect (`canonical_id`, `netrc`,
`auth_patterns`, `patch_cmds_win`) are accepted; any other http_archive
attribute set to a non-default value (`remote_patches`, `add_prefix`,
`strip_components`, `build_file`, `files`, `patch_tool`, ...) fails when the
module is fetched — an override of a module outside the graph is never
fetched, as in Bazel.

## `git_override`

On top of docs/GIT_OVERRIDES.md:

- `tag`, `branch` or neither (the remote's default HEAD, git_worker.bzl)
  resolve to a commit with `git ls-remote`, pinned like Bazel's fetched
  repository behind its marker file: in the workspace's own state directory
  (`<cache>/worktrees/<name>-<hash>/pins/git`, or the `--output_base`), keyed
  by the override's complete identity (remote, ref, `strip_prefix`,
  submodule flags, `build_file_content`, patch inputs). So the same workspace
  keeps building the pinned commit, while a fresh workspace, `clean
  --expunge`, `fetch --force` (which ignores pins) or any attribute change
  resolves the ref again. The commit-keyed trees stay in the shared store.
  The fetch prints genuine's reproducible-form DEBUG lines (`commit = "..."
  and dropping [tag]`).
  More than one of `commit`/`tag`/`branch` fails with git.bzl's text.
- submodule gitlinks are empty directories without `init_submodules`
  (genuine skips them; OpenROAD's kepler-formal route). `init_submodules` /
  `recursive_init_submodules` on a tree with gitlinks fail explicitly.
- `patch_cmds`, `-pN` `patch_args` and `build_file_content` as for archives.

Printed DEBUG lines taint the analysis snapshot, so a later snapshot hit
never replays them.
