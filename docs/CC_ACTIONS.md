# cc_common.compile/link: `cc_internal` compile and link actions (N60)

Bazel 9.3 implements `cc_common.compile`/`cc_common.link` as rules_cc 0.2.17
Starlark (`cc/private/compile/compile.bzl`, `cc/private/link/*.bzl`) over the
private `cc_internal` surface (`CcStarlarkInternal.java`). The native profile
now runs that Starlark against genuine `cc_import` and `cc_library` targets
and the go c-archive/c-shared/cdeps paths on top of it. Citations:
`J = src/main/java/com/google/devtools/build/lib` of the pinned
`/tmp/opencode/bazel-9.3-reference` tree.

## What is implemented

Every remaining member of the N57 gap list is native (src/cc_actions.rs unless
noted):

- **`actions2ctx_cheat(actions)`** (:486-494) — the action factory carries its
  rule or aspect context value (`Actions.rule_ctx`, set when the context is
  allocated), so rules_cc's Starlark reads `ctx.label`, `ctx.configuration`,
  `ctx.exec_groups`, `ctx.toolchains` and `ctx.workspace_name` through it.
- **`create_cc_compile_action`** (:578-757) — registers a native CppCompile
  spawn: argv = the feature configuration's tool path plus
  `getPerFeatureExpansions` over the action config and enabled features
  (CcToolchainFeatures.java:941-975; the `unfiltered_compile_flags` feature is
  exempt from the CoptsFilter, and the filter inverts and FULLY matches —
  `!noCoptsPattern.matcher(flag).matches()`, CcCommon.java:122-130),
  environment = the configuration's action environment (fixed and the
  inherited `--action_env` names) then `PWD=/proc/self/cwd` put over it, then
  the feature env sets over that (CppCompileAction.getEffectiveEnvironment
  :1010-1021),
  inputs = the mandatory inputs of `buildMandatoryInputs` (additional inputs
  and their depset form, builtin include files + build info headers, the
  compilation context's `headers`/`_non_code_inputs`/`_direct_module_maps`/
  `_module_map`, the source, `_grep_includes`, the toolchain's
  `compiler_files` — `as_files` for `assemble`, `all_files` for a tree
  artifact source, CppCompileActionBuilder.java:280-288 with
  `--incompatible_use_specific_tool_files`) plus the prunable
  headers (include scanning is off), outputs = the object/dotd/diagnostics/
  gcno/dwo/lto-indexing files and `additional_outputs`, the mnemonic from
  `actionNameToMnemonic` (CppCompileAction.java:1993-2018), and the action
  config's execution requirements merged into the action info. The action runs
  under the N59 sandbox; `gcc`/`ar` write their outputs with read-only
  normalization identical to every other action. Like Bazel's CppCompileAction
  it is not shareable (`CppCompileActionBuilder.java:99`): an identical compile
  of another target on the same object is an action conflict, one target's
  duplicates are one action (docs/SHAREABLE_ARTIFACTS.md).
- **`get_link_args`** (:407-440) — an `Args` value expanding to the
  LinkCommandLine's parameter command line (the flags without the tool; the
  spawn's `executable` is the tool): `getParamCommandLine`
  (LinkCommandLine.java:92-130) including the `linker_param_file` filter, and
  `getParamFileInfo` (:132-150): the named `parameter_file_type` argument is
  required, but accepts **None** as well as a string. None disables splitting:
  the Args has **no parameter-file metadata**, even if the expanded flags
  contain a placeholder. The `linker_param_file` variable's value is still
  filtered out of the flags; this is independent of splitting. With a
  supported string type, a flag containing
  `LINKER_PARAM_FILE_PLACEHOLDER` (rules_cc sets the `linker_param_file`
  build variable to that string; the toolchain's legacy `linker_param_file`
  feature emits `@%{linker_param_file}`) installs an always-on
  [`ParamFileInfo`] on the Args — flag format `@%s`, the
  `parameter_file_type` quoting (GCC_QUOTED as the native Shell writer,
  UNQUOTED as the multiline one; WINDOWS and SHELL_QUOTED fail explicitly).
  Types and enum text are validated even without a placeholder; missing is
  not an implicit None default. Invalid strings fail cleanly (genuine's
  unchecked `ParameterFileType.valueOf` exception is not reproduced). The executed
  argvs are then `ar @…libbuilt.a-0.params` / `gcc @…libbuilt.so-0.params`
  exactly like genuine (CommandLines.java:113-141's `<output>-N.params`
  naming and the always split; genuine `aquery` prints the pre-split form,
  its execution log confirms the rewrite —
  tests/fixtures/cc_compile/evidence.json).
- **Artifact names** — `get_artifact_name_for_category` /
  `get_artifact_name_extension_for_category` (:345-374) over
  `ArtifactNamePatternMapper` (CcToolchainFeatures.java:765-812): the
  toolchain's `artifact_name_pattern` overrides (differing from the defaults
  only) against `ArtifactCategory` defaults; queries go through
  `ArtifactCategory.valueOf` (rules_cc's uppercase `artifact_category_names`),
  the toolchain config's patterns through `getCategoryName` (lowercase).
  `declare_compile_output_file` (CppHelper.getCompileOutputArtifact,
  `<package>/_objs/<name>/<output>` below bin) and
  `declare_other_output_file` (a sibling of the object file).
- **Solib symlinks** — `dynamic_library_symlink`/`dynamic_library_symlink2`/
  `dynamic_library_soname`/`solib_symlink_action`
  (SolibSymlinkAction.java:186-319) with `Actions.escapedPath`
  (Actions.java:43-52), the 255-character hash cut-off
  (`maybeHashPreserveExtension`), the mangled names and sonames of the paired
  capture, registered as real symlink transforms with the `SolibSymlink`
  mnemonic. Like genuine, registering the action does not execute it: the
  outputs are built when a consumer requests them.
- **`create_header_info` with artifacts** (CcStarlarkInternal.java:1166-1248):
  HeaderInfo now carries the module artifacts and header lists; the list
  getters are immutable copies; `create_header_info_with_deps` propagates the
  fields with a new identity.
- **Linkstamp mnemonic** — `actionNameToMnemonic` maps the
  `linkstamp-compile` action name (CppActionNames.java:26) to
  `CppLinkstampCompile`.
- **Small semantics helpers** — `rule_class`, `aspect_class`,
  `check_toplevel`, `intern_seq`, `is_tree_artifact` (tree artifacts),
  `compute_output_name_prefix_dir` ("" outside objc purposes),
  `per_file_copts` (`--per_file_copt` is unmodelled, no matches) and
  `wrap_link_actions` (the wrapped factory differs only for shareable
  artifacts). The dependency `Target` surface gained the default-provider
  fields `data_runfiles`/`default_runfiles`/`files_to_run`
  (AbstractConfiguredTarget.getValue :58-59), which rules_go's cgo context
  reads on `cc_import` targets; `dir(target)` follows `getFieldNames`
  (:183-193): `data_runfiles`, `default_runfiles`, `files`, `files_to_run`,
  `label` and `output_groups` (readable) when the target declares an
  OutputGroupInfo.

## What is verified

The direct nullable-API fixture (`tests/fixtures/cc_features/tools/build_defs/cc/link_args.bzl`,
compact genuine receipt `link_args_reference.json`, provenance in that fixture's
`LINK_ARGS.md`) bypasses the default-config policy path that did not reproduce
the whole-source cquery failure. It records executed argv and raw parameter-file
bytes for None/GCC_QUOTED/UNQUOTED, with a linker-variable placeholder, no
placeholder, a custom linker-variable value, and a literal placeholder without
the variable. None is byte-exact genuine argv with no params, including spaces,
quotes, backslashes, percent signs and an empty argument. A unit test separately
asserts the absence of metadata, not merely lack of a short-command spill.
Missing arguments and int/bool/list types match genuine diagnostics. Unsupported
types and invalid text fail even when there is no placeholder.

**Existing quoting limitation retained:** GCC_QUOTED currently uses the native
Shell writer, whereas genuine uses `GccParamFileEscaper` backslash escaping. The
new fixture exposes and pins these differing raw bytes rather than claiming
byte parity or changing existing GCC_QUOTED behavior in this nullable-API fix.
UNQUOTED bytes and param-reference percent escaping match the genuine capture.

The paired capture (`artifacts/cc_compile-capture.py`, receipt
`tests/fixtures/cc_compile/reference_receipt.json`): genuine `cc_library`
(`//cdep:built`) compile+link — `prebuilt.pic.o`, `prebuilt.pic.d`,
`libbuilt.a`, `libbuilt.so` — byte-identical to genuine 9.3.0; genuine
`cc_import` (`//cdep:prebuilt`, `//cdep:prebuilt_shared`) analyzing and
registering the `_solib_k8` symlinks exactly like the genuine aquery; the go
c-archive `//cgo:cgo_archive` end to end (through N58's `_go_cc_aspect`,
rules_go's `new_cc_import` and the rules_cc link path) with `cgo_archive.a`,
`cgo_archive_.a`, `.x` byte-identical; the cdeps go binary `//cmd:cdepcmd`
and its `with_cdep.a`/`.x` byte-identical; and the identical gcc failure text
when the cgo header include path is missing.

The go c-shared `libcgo_shared.so` is **EXPLAINED**, not exact: the go
external linker passes the output path as `-soname`, so `.dynstr` embeds the
absolute execroot path of the executing sandbox; its length shifts every
later section offset. Genuine Bazel 9.3.0 itself produces different bytes
across output bases while the GNU build id stays fixed (it does not cover
that string) — two genuine runs under different `--output_user_root`s are
recorded in `tests/fixtures/cc_compile/evidence.json`, which also compares
the executed action surface (argv including the `@…-0.params` rewrite,
environment, inputs) of CppCompile/CppArchive/CppLink against the native
`--native-action-dump` diagnostic: equal. The capture records that the
string sets are identical modulo that prefix and the section tables are
identical modulo the shift.

## ThinLTO and linkstamps (OSS corpus B32)

- **`create_lto_backend_action`** (CcStarlarkInternal.java:1027-1081,
  LtoBackendArtifacts.createLtoBackendActionForStarlark :149-165) registers
  a `CcLtoBackendCompile` spawn (LtoBackendAction.java): argv = the
  `lto-backend` tool and the feature configuration's command line, then
  `-fPIC` for a PIC backend (getLtoBackendCommandLine :103-146); inputs =
  the given depset; exactly the given environment (ActionEnvironment.create,
  no configuration environment); progress `LTO Backend Compile %{output}`;
  its aquery ActionKey is LtoBackendAction.computeKey (:276-298: GUID,
  arguments, mnemonic, the mandatory inputs' paths, the allowed bitcode
  files' fingerprint and the imports path of an indexed backend, the
  environment, the execution info). Its outputs are shareable artifacts.
  **Not modelled:** LtoBackendAction's input discovery (the imported
  bitcode files the `.imports` file of the indexing step names, a subset of
  `all_bitcode_files`, LtoBackendAction.discoverInputs): the action has its
  mandatory inputs, like genuine's before discovery (what `Target.actions`
  and aquery show), so a sandboxed indexed backend does not see the
  imported bitcode files.
- **`collect_per_file_lto_backend_opts`** (:530-542): the options of every
  `--per_file_ltobackendopt` whose RegexFilter includes the object's exec
  path.
- **`wrap_link_actions(sharable_artifacts = True)`**: the plain action
  factory, whose `declare_shareable_artifact` already declares a shareable
  artifact below the bin root (LinkActionConstruction.create,
  CppLinkActionBuilder.java:59-67).
- **`create_cc_compile_action(shareable = True)`** (the linkstamp compile,
  linkstamp_compile.bzl; CppCompileActionBuilder.setShareable :559-562): a
  shareable compile action. Every CppCompileAction's aquery environment is
  the configuration's (CppCompileAction.getEnvironment :489-491), the
  linkstamp compile's included.

tests/cc_compile.rs `thin_lto_actions_match_genuine` and
`linkstamp_compiles_match_genuine` compare the aquery output with genuine
9.3.1's, ActionKeys included (tests/fixtures/cc_compile/PROVENANCE.md).

## What fails explicitly

- **C++20 modules**: `create_cc_compile_action_template` (tree-artifact
  compile action templates), `module_files`/module maps on the compile action.
- **LTO over tree artifacts**: `create_lto_backend_action_template` and the
  wrapped factory's `declare_shareable_directory`.
- **FDO**: `absolute_symlink` (absolute-path FDO profiles); FDO/propeller
  profiles remain N25d's explicit toolchain-analysis gaps.
- **libc_top** (N25d5) remains unsupported.
- **cc_library `linkstamp`** — supported since B32 (the shareable
  linkstamp compile above; the `LinkstampInfo` closures in depsets since
  N49b/corpus-parity5). The stamped build-info translation itself is supported
  (docs/WORKSPACE_STATUS.md): `cc_binary` over a built `cc_library` links
  byte-identically to genuine, and the `_build_info_files`
  `volatile_file.h`/`non_volatile_file.h` are byte-exact under a fixed
  `--workspace_status_command` script + `SOURCE_DATE_EPOCH`.
- **Windows/macOS toolchains** and `expand_and_tokenize` (objc flag
  expansion) fail explicitly; the profile runs the Linux `@local_config_cc`
  toolchain only.
