# Runfiles, DefaultInfo, executable and test rules (N23a, analysis only)

N23a adds the Bazel 9.3 analysis model of runfiles. Since N23f
(docs/RUNFILES_TREES.md) executables get their runfiles tree,
`<exe>.runfiles/MANIFEST`, `_repo_mapping` entry and `<exe>.repo_mapping` file
like genuine Bazel, tools with runfiles attach their tree to spawns and the
`run` command exists; the N23a explicit gaps for those are gone. Source paths
below are relative to
`src/main/java/com/google/devtools/build/lib/` of the pinned Bazel 9.3 sources.
Implementation: `src/runfiles.rs`, DefaultInfo in `src/starlark_types.rs`,
`ctx.runfiles`/`ctx.executable` in `src/analysis_ctx.rs`/`src/analysis.rs` and
native providers in `src/engine.rs`.

## `runfiles` values

* Fields `files` (a COMPILE-order depset, shown as `order="postorder"`;
  analysis/Runfiles.java:497-503), `symlinks` and `root_symlinks` (STABLE
  depsets of `SymlinkEntry`) and `empty_filenames` (the empty-files
  supplier's paths, :243-264: empty unless
  `py_internal.merge_runfiles_with_generated_inits_empty_files_supplier`
  installed the Python `__init__.py` supplier, N23f, docs/RUNFILES_TREES.md).
* `merge(other)` and `merge_all(others)` (:830-885): merging with an empty side
  returns the other object itself; `merge_all` with exactly one non-empty input
  returns that input, with none returns `Runfiles.EMPTY`. Type errors use
  Bazel's binder text (`in call to merge(), parameter 'other' got value of type
  'int', want 'runfiles'`, `at index 1 of param, got element of type int, want
  runfiles`).
* The conflict policy (WARN/ERROR) propagates as the strictest one, even from
  empty runfiles (Builder.merge, :638-662). Conflicts (obscured entries) are
  reported when the runfiles manifest is built (N23f, docs/RUNFILES_TREES.md).
* Identity: Bazel's Runfiles has no `equals`. Here two values are equal exactly
  when they share the same nested sets and policy, which is the same object in
  Bazel (`Runfiles.EMPTY` is one shared identity). `type()` is `runfiles`;
  `repr` is the StarlarkValue default
  `<unknown object com.google.devtools.build.lib.analysis.Runfiles>`.
* `SymlinkEntry` (analysis/SymlinkEntry.java): `path`, `target_file`, repr
  `SymlinkEntry(path = "...", target_file = ...)`; each created entry is a
  distinct identity (no `equals`).

## `ctx.runfiles`

`ctx.runfiles(files, transitive_files, collect_data, collect_default,
symlinks, root_symlinks, *, skip_conflict_checking)`
(analysis/starlark/StarlarkRuleContext.java:1059-1125):

* Order of contributions: `collect_data`, `collect_default`, `files`,
  `transitive_files`, symlinks, root symlinks. The artifacts set is COMPILE
  order, so transitive sets precede direct `files` in `files.to_list()`.
* Values are equal only by identity. `print()`/`fail()` use Bazel's debugPrint
  forms (N22 `collect_debug_print` hook): `Runfiles(empty_files = depset([]),
  files = depset([...], order = "postorder"), root_symlinks = depset([...]),
  symlinks = depset([...]))` (Runfiles.java:935-945; the depsets print their
  repr) and `FilesToRunProvider(executable = ..., repo_mapping_manifest = ...,
  runfiles_manifest = ...)` with Files or `None` (FilesToRunProvider.java:112-120);
  `repr()` keeps the StarlarkValue default. SymlinkEntry paths are Starlark
  string reprs. Assigning a field fails with Bazel's text
  (`cannot set .files field of runfiles value`, EvalUtils.java:529-536; also for
  SymlinkEntry and FilesToRunProvider).
* `collect_data`/`collect_default` (Runfiles.Builder.addRunfiles, :669-781):
  the `data` prerequisites contribute their files to build
  (`--incompatible_always_include_files_in_data`, default true,
  CoreOptions.java:612-624) and, for rule targets, their data runfiles; then
  `srcs` and `deps` contribute their data or default runfiles. Only attributes
  the rule defines as label lists count. File targets have no RunfilesProvider
  and contribute only as `data`.
* `transitive_files` must be a depset of Files of a COMPILE-compatible order
  (`order 'preorder' is invalid for transitive_files`); element type errors use
  `for 'transitive_files', got a depset of 'string', expected a depset of
  'File'`.
* `symlinks`/`root_symlinks` are a dict of string to File (paths are
  normalized like PathFragment) or a depset of SymlinkEntry. Non-empty values
  set the ERROR conflict policy unless `skip_conflict_checking`.
* `skip_conflict_checking=True` is a private API
  (BuiltinRestriction.failIfCalledOutsideDefaultAllowlist): main-repository
  `.bzl` files outside the allowlist fail with `file '//p:defs.bzl' cannot use
  private API`; the whole default allowlist is honored (N50): main packages,
  the main repository's apparent-name mapping, `bazel_tools` and external
  module entries by canonical-name prefix (`rules_shell+`, reposMatch,
  BuiltinRestriction.java:120-140); others fail with `file
  '@@other_rules+//:defs.bzl' cannot use private API`.

## DefaultInfo

* Constructor (analysis/DefaultInfo.java:203-225): named `files` (depset),
  `runfiles`, `data_runfiles`, `default_runfiles`, `executable` (File), each
  also `None`; `runfiles` together with `data_runfiles`/`default_runfiles`
  fails with Bazel's text. Fields are `files`, `files_to_run`, `data_runfiles`,
  `default_runfiles` (falling back to `runfiles` when neither stateful field is
  given, :126-135); unset fields are `None` (so `DefaultInfo().files` is `None`,
  a change from the previous empty depset) and `files_to_run` is `None` on a
  constructed instance. repr follows NativeInfo:
  `struct(data_runfiles = None, default_runfiles = None, files = None, files_to_run = None)`.
  The `files` element type is checked when the rule returns it. Equality is
  NativeInfo's (packages/NativeInfo.java:96-112): `DefaultInfo() == DefaultInfo()`
  is `True`; depsets, runfiles and FilesToRunProviders compare by identity.
* `target[DefaultInfo]` is the DelegatingDefaultInfo view (:148-196) of the
  configured target: files to build, its RunfilesProvider (Runfiles.EMPTY when
  absent) and its FilesToRunProvider.
* Returned DefaultInfo (analysis/starlark/StarlarkRuleConfiguredTargetUtil.java:359-505):
  the executable must be owned by the rule; a different executable after
  accessing `ctx.outputs.executable` fails; an executable or test rule without
  an executable fails (`The rule 'x' is executable. It needs to create an
  executable File and pass it as the 'executable' parameter to the DefaultInfo
  it returns.`). Files to build are the predeclared outputs plus the executable
  unless `files` is given. Without any runfiles the stateless runfiles are
  EMPTY; the executable is added to the default runfiles (stateless runfiles
  always, stateful ones for executables/tests) as the builder's *direct*
  element, so in the COMPILE-order `files.to_list()` it appears **last**, after
  the transitive runfiles (StarlarkRuleConfiguredTargetUtil.mergeFiles,
  :493-502); `Test rules have to define runfiles` is kept (unreachable while the
  executable is merged in). Bazel frames these rule errors as `in KIND rule L:
  \n<creation location>: TEXT`; native analysis errors keep the global
  `analysis of L: TEXT` framing (no creation location), with the same TEXT.
* Return values (StarlarkRuleConfiguredTargetUtil.java:119-127, 245-275):
  `Returning a struct from a rule implementation function is deprecated.`,
  `Rule should return a struct or a list, but got T`, `Expected a list of
  providers, but got dict`; two DefaultInfo are `Multiple conflicting returned
  providers with key DefaultInfo`. Declared outputs without an action report
  `The following files have no generating action:\n<root-relative paths>`
  (StarlarkProviderValidationUtil.java:25-46; Bazel iterates a HashMap, native
  lists them in declaration order).
* RunEnvironmentInfo from an executable or test rule is accepted (metadata only).

## FilesToRunProvider

FilesToRunProvider.create (analysis/FilesToRunProvider.java:40-58) chooses the
class whose name is the repr: `...FilesToRunProvider` (empty, or files without
an executable), `...$SingleExecutableFilesToRunProvider` (file targets and
single-file filegroups) or `...$FullFilesToRunProvider` (targets with runfiles
support). Fields `executable`, `runfiles_manifest` and `repo_mapping_manifest`.
With runfiles support the manifests are `<executable>.runfiles/MANIFEST`
(`--build_runfile_links` default; `<executable>.runfiles_manifest` with
`--nobuild_runfile_links`, none with `--nobuild_runfile_manifests`) and
`<executable>.repo_mapping` in the executable's directory
(analysis/RunfilesSupport.java:83-86, 305-325, 405-440, 684-716). Since N23f
they are generated like Bazel (SourceManifestAction, SymlinkTreeAction,
RepoMappingManifestAction; docs/RUNFILES_TREES.md) and usable as action
inputs, DefaultInfo files and runfiles. Their contents use absolute target
paths; native paths are those of the native execution layout (published cache
entries), compared normalized by the N23f capture.

Identity: a rule target's FilesToRunProvider is one object
(`info.files_to_run == info.files_to_run`); a file target creates a new one on
every access, so the same comparison is `False` (FileConfiguredTarget.java:85-101).

## Executable and test rules

* `rule(executable=True)`/`rule(test=True)` are analyzed. `ctx.outputs.executable`
  is created on access (it then appears in `dir(ctx.outputs)`, and an output
  attribute named `executable` fails with `Multiple outputs with the same key:
  executable`; StarlarkRuleContext.java:344-450). `ctx.outputs` is one `Outputs`
  object for every rule (shared with N22's repr: `ctx.outputs(name = ...)` while
  the implementation runs, `ctx.outputs(for //pkg:x)` afterwards, when reading
  a field fails with `cannot access outputs of rule '//pkg:x' outside of its own
  rule implementation function`, StarlarkRuleContext.java:440-447); an
  unknown field reports `No attribute 'x' in outputs. Make sure you declared a
  rule output with this name.` (so `ctx.outputs.executable` of a non-executable
  rule fails like Bazel; StarlarkRuleContext.java:402-408).
* Test rule names must end in `_test` (unchanged). The built-in `args` attribute
  is accepted when its values need no Make-variable/location expansion or shell
  tokenization (no `$`, quotes or backslashes); otherwise analysis fails
  explicitly (RunfilesSupport.computeArgs, :602-610). Shell tokenization of
  `args` (ShellUtils.tokenize) remains; `run` and `test` (N23g,
  docs/TEST_COMMAND.md) use the whitespace-separated values.
* The implicit test attributes `$test_wrapper`, `$xml_writer`, `$test_runtime`,
  `$test_setup_script`, `$xml_generator_script`, `$collect_coverage_script` and
  the coverage/run-under attributes (StarlarkRuleClassFunctions.java:258-378) use
  the test-runner exec transition; they are not exposed in `ctx.attr`. In the
  built-in profile they are analyzed as `exec(test)` dependency edges (N23g,
  docs/TEST_COMMAND.md); the coverage report generator and run-under ones are
  null without coverage/`--run_under`.
* `bazel build` of an executable or test target builds its files to build and,
  like OutputGroupInfo.HIDDEN_TOP_LEVEL (RuleConfiguredTargetBuilder.java:144-171),
  the artifacts of its default runfiles and its runfiles tree without
  reporting them (N23f, docs/RUNFILES_TREES.md). `run` exists since N23f (not
  for test targets); `test` exists since N23g (docs/TEST_COMMAND.md). Simple `args` (whitespace
  separated) are passed by `run`.

## `ctx.executable` and actions

* `attr.label(executable=True, cfg=...)` is analyzed with `cfg="target"` and
  `cfg="exec"` (the exec transition, docs/CONFIGURATION.md: the executable is
  the exec-configured dependency's, below its exec output root such as
  `bazel-out/k8-opt-exec/bin`); a missing `cfg` still fails with `cfg parameter is mandatory when executable=True is
  provided`). `ctx.executable.<attr>` is the dependency's executable or `None`
  for an unset attribute; a dependency without one fails with `in tool attribute
  of r rule //p:r: //p:x does not refer to a valid executable target`
  (implicit attributes are shown as `(an implicit dependency)`). An unknown
  field reports `No attribute 'x' in executable. Make sure there is a label type
  attribute marked as 'executable' with this name\nAvailable attributes: a, b`
  (StarlarkAttributesCollection.java:82-86, StructImpl.java:86-95).
* The first FilesToRunProvider of each executable is kept
  (StarlarkRuleContext.java:978-990). `actions.run(executable=...)` accepts the
  File or a FilesToRunProvider (whose files to run become inputs).
  `run`/`run_shell` `tools` accept a sequence or depset of Files,
  FilesToRunProviders and depsets of Files with Bazel's type errors
  (StarlarkActionFactory.java:765-806); a provider's files to run are staged as
  inputs. An executable or tool with runfiles (a FullFilesToRunProvider or a
  File from `ctx.executable` with one) attaches its runfiles tree to the spawn
  (N23f, docs/RUNFILES_TREES.md).

## Native rules

* filegroup (rules/filegroup/Filegroup.java:80-119): `data` prerequisites
  (target configuration) never add files to build. Default runfiles:
  Builder.addRunfiles(DEFAULT_RUNFILES) — each `data` prerequisite's files to
  build and data runfiles, then the `srcs`' default runfiles; data runfiles:
  the files to build, then addRunfiles(DATA_RUNFILES) — `data` alike, then
  the `srcs`' data runfiles (`--incompatible_filegroup_runfiles_for_data`); a
  single executable when it has exactly one file to build. The default
  runfiles are hidden top-level artifacts, so building a filegroup builds its
  generated data. Paired capture `tests/fixtures/filegroup_data`.
* genrule (rules/genrule/GenRuleBase.java:215-235): no default runfiles, its
  outputs as data runfiles; `executable=True` stays gated.
* Source and output file targets: no RunfilesProvider, a single-executable
  FilesToRunProvider (FileConfiguredTarget.java:85-101).
* alias/label_flag/label_setting forward the actual's providers; other native
  classes have no runfiles.
* Runfiles, SymlinkEntry, FilesToRunProvider and DefaultInfo values nested in
  custom providers contribute their files to dependents.

## Genuine Bazel capture

`tests/fixtures/runfiles/reference_receipt.json` records the paired capture
against Bazel 9.3 (run 4, 255 rows): 143 exact, 18 exact messages, 2
pre-N22 repr differences, 20 expected native-unsupported, 72 mismatches. Of
the 72, 58 have the same final error text and differ only in framing (15
rule-context prefixes, 43 Starlark tracebacks). The other 14: generic
interpreter no-field wording for runfiles/SymlinkEntry/DefaultInfo values,
positional/keyword argument wording (`merge(other=)`, `DefaultInfo(x)`,
`attr.label_list(executable=)`), `ctx.actions.symlink` (N23d at the time;
implemented since, docs/ACTIONS_PARAMS.md), the provider
type/repr of `DefaultInfo` (N22), `dir(ctx)` (at the time; N23c now has
Bazel's 36 members), and the explicit
`args`, `run_lazy`/`run_provider` and source-listing differences described
below. No mismatch concerns runfiles content, order, identity or the
DefaultInfo target view.

## Not covered (explicit or documented)

`args` tokenization beyond whitespace separation, the nested-set depth-limit message. The
native CLI also lists source files among a target's outputs, which Bazel's
`--show_result` omits (pre-existing). Runfiles trees, manifests, tool
runfiles and `run` are N23f (docs/RUNFILES_TREES.md).
