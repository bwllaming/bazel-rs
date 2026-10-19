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
  `_module_map`, the source, `_grep_includes`, `all_files`) plus the prunable
  headers (include scanning is off), outputs = the object/dotd/diagnostics/
  gcno/dwo/lto-indexing files and `additional_outputs`, the mnemonic from
  `actionNameToMnemonic` (CppCompileAction.java:1993-2018), and the action
  config's execution requirements merged into the action info. The action runs
  under the N59 sandbox; `gcc`/`ar` write their outputs with read-only
  normalization identical to every other action.
- **`get_link_args`** (:407-440) — an `Args` value expanding to the
  LinkCommandLine's parameter command line (the flags without the tool; the
  spawn's `executable` is the tool): `getParamCommandLine`
  (LinkCommandLine.java:92-130) including the `linker_param_file` filter, and
  `getParamFileInfo` (:132-150): a flag containing
  `LINKER_PARAM_FILE_PLACEHOLDER` (rules_cc sets the `linker_param_file`
  build variable to that string; the toolchain's legacy `linker_param_file`
  feature emits `@%{linker_param_file}`) installs an always-on
  [`ParamFileInfo`] on the Args — flag format `@%s`, the
  `parameter_file_type` quoting (GCC_QUOTED as the native Shell writer,
  UNQUOTED as the multiline one; WINDOWS fails explicitly). The executed
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

## What fails explicitly

- **C++20 modules**: `create_cc_compile_action_template` (tree-artifact
  compile action templates), `module_files`/module maps on the compile action.
- **LTO**: `create_lto_backend_action`, `create_lto_backend_action_template`,
  `collect_per_file_lto_backend_opts`.
- **Shareable link artifacts**: `wrap_link_actions(sharable_artifacts = True)`
  and shareable compile outputs.
- **FDO**: `absolute_symlink` (absolute-path FDO profiles); FDO/propeller
  profiles remain N25d's explicit toolchain-analysis gaps.
- **libc_top** (N25d5) and stamped link outputs remain unsupported.
- **Windows/macOS toolchains** and `expand_and_tokenize` (objc flag
  expansion) fail explicitly; the profile runs the Linux `@local_config_cc`
  toolchain only.
