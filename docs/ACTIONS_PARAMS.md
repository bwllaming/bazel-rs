# Starlark action surface: parameter files, tree artifacts, symlinks, templates (N23d)

This profile adds the `ctx.actions` surface that Go/C++ builder actions need
and really executes it. Every claim below is checked by Rust tests
(`src/command_line.rs`, `src/action.rs`, `src/args.rs`, `tests/actions_params.rs`)
and by a paired genuine Bazel 9.3.0 capture
(`tests/fixtures/actions_params/reference_receipt.json`, evidence only).
Citations are to the pinned Bazel source
(`src/main/java/com/google/devtools/build/lib/`, release 9.3.0, e89755b).

## Args parameter files

* `Args.use_param_file(param_file_arg, *, use_always=False)` validates the
  format like `SingleStringArgFormatter.isValid` (exactly one `%s`, `%%` a
  literal percent) with Bazel's message
  (`analysis/starlark/Args.java:548-563`).
* `Args.set_param_file_format(format)`: `shell` (SHELL_QUOTED, the default),
  `multiline` (UNQUOTED) or `flag_per_line` (UNQUOTED, flags only); a second
  call fails with `set_param_file_format() may only be called once`
  (`Args.java:565-595`).
* `flag_per_line` groups every `add`/`add_all`/`add_joined` call: a call with
  two or more arguments becomes `first=rest` (rest joined by spaces; `rest`
  alone when `first` is empty) — on the command line too, not only in the file
  (`StarlarkCustomCommandLine.java:995-1049, 1468-1477, 1696-1735`). Only
  arguments starting with `--` go to the file; the others stay on the command
  line after the `@file` argument (`CommandLines.java:132-143`,
  `ParameterFile.java:103-126`).
* The command line of `run`/`run_shell` is built like
  `StarlarkActionFactory.buildCommandLine` (`:690-717`): consecutive strings
  form one part, every Args object is its own part with its ParamFileInfo.
  `CommandLines.expand` (`actions/CommandLines.java:89-153`) decides per part:
  `use_always`, or the running length (each argument's internal byte length
  plus one, the executable part included) exceeding
  `--min_param_file_size` (32768, `analysis/config/CoreOptions.java:294-304`)
  minus 512 per part, spills the part into
  `<primary output exec path>-<n>.params` (`ParameterFile.derivePath`,
  `ParameterFile.java:79-81`), `n` counting spilled parts from 0. The
  executable part is `[exec path]` for `run` and `["/bin/bash", "-c",
  command]` plus `""` when `arguments` is nonempty for `run_shell`
  (`ShellCommand.java:37-48`).
* File contents: each argument's internal bytes plus LF, shell-escaped for
  SHELL_QUOTED (`ParameterFile.writeParameterFile`, `:84-101`). The file is
  written into the execution root before the process starts and is part of
  the action key by content.
* `ctx.actions.write(output, args)` writes the Args in its parameter-file
  format with flag_per_line grouping (ParameterFileWriteAction,
  `StarlarkActionFactory.java:356-410`); `use_param_file` does not affect it.
* Only the default `--min_param_file_size` exists: the option is not modelled
  (the CLI rejects unmodelled options).

## Tree artifacts

* `ctx.actions.declare_directory(filename, sibling=None)` declares a tree
  artifact (`File.is_directory` is True); a path declared both as a file and
  as a directory fails when the implementation returns with `The following
  directories were also declared as files:` (`StarlarkProviderValidationUtil.java:25-32`).
* Tree outputs are created before execution; afterwards every regular file
  and subdirectory below the root is made `0555`
  (`skyframe/ActionOutputMetadataStore.java:245-281`), and so is the root
  (observed by the paired capture). Published trees,
  including empty subdirectories, are verified on cache reuse (file digests,
  directory set and modes). Symlinks inside a tree are followed like
  Bazel's TreeArtifactValue.visitTree (the files they resolve to, at their
  paths, with their targets' modes; dangling links, links to special files
  and cycles fail with Bazel's texts) and materialized when the tree is
  published (docs/TREE_SYMLINKS.md); a special file fails like Bazel's
  `child X has an unsupported type`.
* Tree inputs are staged as directories and keyed by their complete file
  listing, digests and executable bits, so a changed tree reruns consumers.
* `Args.add_all`/`add_joined` with `expand_directories=True` (the default)
  expand a tree to its files in byte order of relative path when the consuming
  action executes (`VectorArg.preprocess`/`expandDirectories`,
  `StarlarkCustomCommandLine.java:309-485`). `map_each` is then called on each
  child File when the action executes: a top-level function of a loaded `.bzl`
  file is called frozen; any other callable — a closure with
  `allow_closure=True` (aspect_rules_js's js_image_layer `map_entry`), a
  lambda, a built-in such as `str` — is retained by the analysis module and
  frozen with it (`analysis_runtime::retain_mapper`), like Bazel's retained
  StarlarkCallable.
  Without `allow_closure=True`, `map_each` must be a global `def`
  (Args.validateMapEach, `analysis/starlark/Args.java:366-385`): one of the
  analyzing package's load graph, or (N47) of any package the build loaded,
  as when a function reaches the rule through a dependency's or toolchain's
  provider (rules_go's `go.toolchain.actions.stdlib` and its `_dirname`
  mapper; `crate::analysis_runtime::LoadedFunctions`, keyed by value
  identity). Any other callable — built-ins such as `str`, `repr` and
  `json.encode` (aspect_rules_js's js_image_layer) — is accepted as it is
  (only StarlarkFunctions must be global; also TemplateDict.add_joined,
  `TemplateDict.java:70-78`); a non-callable is the binding error `in call to
  add_all(), parameter 'map_each' got value of type 'int', want 'callable or
  NoneType'` (also `add_joined`), before the format-string checks (paired
  capture `tests/fixtures/args_map_each`, 5 cases, all MATCH). Closures and lambdas stay rejected unless
  `allow_closure = True`, with the native message
  (`map_each must be a top-level function visible in this analysis module;
  ...`, not Bazel's text).
  Non-directory values keep the existing eager mapping. An expanded directory
  that is not an action input fails when the action executes with Bazel's
  `Action %{output} failed: Failed to expand directory ...` text (the action's
  progress message prefixes it). `Args.add(directory)` fails
  with Bazel's `Cannot add directories to Args#add ...` (`Args.java:534-541`).
  Written Args expand their directories too (the directories become inputs of
  the write).
* Two-argument `map_each(item, expander)`
  (`StarlarkCustomCommandLine.applyMapEach`/`wantsDirectoryExpander`,
  `:1085-1188`): a `def` or lambda declaring two or more parameters (also
  `def f(x, *rest)`) gets a `DirectoryExpander` (type name `DirectoryExpander`)
  as its second argument; built-ins never do. `expand(file)` returns a tree
  artifact input's files (relative-path byte order) or `[file]`; a tree that
  is no input of the action is FullExpander's `Failed to expand directory
  <generated file ...>. Only directories that are action inputs can be
  expanded.`, failing the action. With `expand_directories=False` a tree
  reaches such a map_each as it is, when the action executes; with `True` its
  files do. Non-directory values keep the eager mapping at analysis time,
  where `expand()` of a tree fails explicitly (genuine expands it at
  execution); the list it returns is mutable natively. Paired capture
  `tests/fixtures/directory_expander` (4 genuine 9.3.0 cases, all MATCH),
  replayed by `tests/directory_expander.rs`.

## ctx.actions.symlink

`symlink(output, target_file=None, target_path=None, target_type=None,
is_executable=False, progress_message=None)` follows
`StarlarkActionFactory.symlink` (`:254-354`) including its argument errors.
With `target_file` the output is a real symlink to the target's absolute
input path (`SymlinkAction.java:218-238`; since N36, docs/HARDLINK_STAGING.md),
so it observes the target's bytes, permission bits (a source file keeps its
mode, a generated file `0555`) and inode; a directory target gives a symlink to
the target tree. `is_executable=True` checks the target's owner execute bit at
execution with Bazel's message (`SymlinkAction.java:296-330`), prefixed like
Bazel's action failures by the expanded progress message (`Creating symlink
%{output} failed: ...`; `%{label}`, `%{output}`, `%{input}` are expanded,
`AbstractAction.java:380-400`).
`target_path` needs an unresolved symlink output:
`declare_symlink(filename, sibling=None)` (`StarlarkActionFactory.declareSymlink`,
`:194-225`; `--allow_unresolved_symlinks` defaults to true and is not a modelled
option) declares a symlink artifact — `File.is_symlink` True, otherwise like a
declared file, a different artifact from a file declared at the same path (so
such a pair leaves one without a generating action) — and
`symlink(output, target_path=..., target_type=None|"file"|"directory")`
registers an UnresolvedSymlinkAction (mnemonic `UnresolvedSymlink`, keyed by
the literal target and the target type, no inputs) with Bazel's argument
errors (`"is_executable" cannot be True when using "target_path"`, `"target_type"
must be one of "file" or "directory"`, a `target_file` with a symlink output,
the `string or NoneType` parameter types). Analysis (`--nobuild`) of their
producers, consumers and runfiles is complete; *executing* an action that
creates an unresolved symlink (this one, or a spawn whose output was declared
with `declare_symlink`) is an explicit native gap — creating, caching and
staging literal symlinks as inputs, runfiles and outputs are not modelled —
so a build that needs one fails at it with `creating the unresolved symlink
artifact 'PATH' -> 'TARGET' (ctx.actions.declare_symlink) is not implemented in
the native profile`. Paired capture `tests/fixtures/unresolved_symlinks`
(16 genuine 9.3.0 cases: 12 MATCH, 4 declared execution cases), replayed by
`tests/unresolved_symlinks.rs`; motivation: aspect_rules_js's node_modules
symlinks (npm/private/utils.bzl `_make_symlink`). The undocumented
`use_exec_root_for_source` (rules_cc's virtual includes) is restricted to the
internal Starlark API allowlist whenever it is passed; `True` links a source
file itself, as the native profile has no persistent execution root
(docs/SHAREABLE_ARTIFACTS.md, which also covers `declare_shareable_artifact`
and shared actions, within one target and across targets).

## expand_template and template_dict

`expand_template(template, output, substitutions={}, is_executable=False,
computed_substitutions=...)` applies the substitutions (dict entries, then the
TemplateDict's, duplicate keys fail like `ImmutableMap.buildOrThrow`)
sequentially and literally on the template's bytes
(`LocalTemplateExpansionStrategy.getExpandedTemplateUnsafe`,
`StringUtilities.replaceAllLiteral`: an empty key is a no-op). The template
may be generated. `ctx.actions.template_dict()` supports `add(key, value)` and
`add_joined(key, values: depset, *, join_with, map_each, uniquify=False,
format_joined=None, allow_closure=False)` with Bazel's map_each result checks
and Starlark `%` formatting for `format_joined` (`TemplateDict.java`); map_each
runs eagerly at `add_joined` (Bazel: when the template action executes).

## run/run_shell environment and execution requirements

* Starlark spawns get exactly their action environment plus `TMPDIR`
  (`exec/local/PosixLocalEnvProvider.java:41-59`): `env`, or with
  `use_default_shell_env=True` the configuration's shell environment (the
  strict `PATH=/bin:/usr/bin:/usr/local/bin` and `--action_env`) overridden by
  `env` (`SpawnAction.createActionEnvironment`, `:584-614`). Genrules use the
  configuration's shell environment as is. See docs/ACTION_ENV.md (N42).
* `execution_requirements` (string dict) is accepted and keyed; it has no
  local effect. `mnemonic`, `progress_message` and `resource_set` are
  accepted; `exec_group` and `toolchain` choose the action's exec group with
  Bazel's checks (N35, docs/EXEC_GROUPS.md); `input_manifests` (a sequence
  or None) is ignored like Bazel's legacy argument (N23f). `param 'outputs'
  may not be empty` is Bazel's text.
* Tools or executables whose FilesToRunProvider carries runfiles attach their
  runfiles tree to the spawn (N23f, docs/RUNFILES_TREES.md).

## transform_version_file / transform_info_file

Both are restricted to Bazel's private-API allowlist
(`BuiltinRestriction.failIfCalledOutsideDefaultAllowlist`) with Bazel's
`file '...' cannot use private API`. Allowed callers (bazel_tools'
`bazel_cc_build_info`) get the declared output File and a registered action.
Since N63 the action executes (docs/WORKSPACE_STATUS.md): planning evaluates
the translation callback against the build's workspace status and rewrites
the registration into a keyed Template expansion (`Transform::TemplateKeyed`)
— the inputs are the workspace status artifact and the template, like
`BuildInfoFileWriteAction`'s (`StarlarkActionFactory.transformBuildInfoFile
:501-534`), and a volatile output is constant metadata (its changing bytes
never re-key consumers). The key is Bazel's `computeKey:189-197` (GUID,
isVolatile, callback bzl digest) plus the template input's digest,
**excluding the substitutions**: a changed timestamp alone does not re-run
the volatile translate (the extended `native_only/warm_rebuild` receipt
row); the stable variant's status input remains a regular keyed input.

## Cache identity

Cache manifests use format `native-action-v6` (tree entries, per-output
modes); the key identity stayed `native-action-v5` so actions without N23d
features kept their keys (N42 moved it to `native-action-v6`: the environment
is framed as fixed map, inherited names and resolved client values for every
action, and the exact-environment flag is gone, docs/ACTION_ENV.md; the N41
merge moved key and manifests to `native-action-v8`, docs/REMOTE_CACHE.md;
fix-digest-memo the manifests alone to `native-action-v9`,
docs/DIGEST_MEMO.md "Upgrade note").
Parameter files are input-root files of the remote cache key at their
`<output>-N.params` paths (crate::action::remote_action). Tree
outputs/inputs, parameter files, the
exact-environment flag of Starlark spawns (until N42), execution requirements and the
native transforms (symlink target and its mode, template substitutions) are
framed in an extra key block. Starlark run/run_shell keys therefore changed
(exact environment). Deferred tree expansion runs before the key is computed,
so expanded arguments and parameter-file contents are keyed like any other.

## Known differences

* `ctx.actions.write` to a declared directory fails at analysis natively;
  Bazel registers it and fails at execution with an I/O exception naming the
  absolute execroot path.
* Symlink outputs were copies until N36; they are real symlinks now
  (docs/HARDLINK_STAGING.md). Inside a consuming action every input,
  including a symlink output, is a read-only hardlink (Bazel's sandboxes
  stage symlinks).
* Source inputs are read-only inside actions since N36: a staged source is a
  hardlink to a 0444 store blob (0555 when executable), so a process sees
  mode 0444 and cannot write it, where Bazel's sandboxes show the source
  file's own mode (e.g. 0644) through a symlink (docs/HARDLINK_STAGING.md;
  the paired capture's documented mismatches).
* map_each of non-directory values, TemplateDict map_each and the
  file/directory conflict run at analysis as before; directory contents are
  mapped at execution like Bazel.

## Paired capture

`artifacts/actions_params-capture.py` builds one fixture with genuine Bazel
9.3.0 (`--batch`, private output root, local spawn strategy, private copy of
`tests/fixtures/builtin_modules/registry`, platforms distfile) and the native
CLI, comparing followed output bytes and modes, tree listings and parameter
files, and rejection messages. The final run
(`tests/fixtures/actions_params/reference_receipt.json`): 32 EXACT,
15 EXACT_MESSAGE, 3 EXPECTED_NATIVE_UNSUPPORTED (declare_symlink, a symlink
inside a tree artifact, tools carrying runfiles) and 2 MISMATCH
(`file_then_directory`: framing only; `write_directory`: the analysis-time
rejection above). The receipt is static evidence of its run: the row
`error:tree_symlink_entry` (genuine builds it) is superseded by
`tests/fixtures/tree_symlinks/reference_receipt.json`, whose cases build
natively (docs/TREE_SYMLINKS.md).
