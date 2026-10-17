# Runfiles, DefaultInfo, executable and test rules (N23a, analysis only)

N23a adds the Bazel 9.3 analysis model of runfiles. **No runfiles tree, no
`<exe>.runfiles/MANIFEST`, no `_repo_mapping` entry and no `<exe>.repo_mapping`
file is created until N23f.** Genuine Bazel creates all of them when it builds
an executable or test target; natively they are never produced silently or
faked: using a manifest as a file fails explicitly (below), and building an
executable target produces only its files to build and runfiles artifacts. Source paths below are relative to
`src/main/java/com/google/devtools/build/lib/` of the pinned Bazel 9.3 sources.
Implementation: `src/runfiles.rs`, DefaultInfo in `src/starlark_types.rs`,
`ctx.runfiles`/`ctx.executable` in `src/analysis_ctx.rs`/`src/analysis.rs` and
native providers in `src/engine.rs`.

## `runfiles` values

* Fields `files` (a COMPILE-order depset, shown as `order="postorder"`;
  analysis/Runfiles.java:497-503), `symlinks` and `root_symlinks` (STABLE
  depsets of `SymlinkEntry`) and `empty_filenames` (always an empty string
  depset: no Starlark API installs an empty-files supplier, :243-264).
* `merge(other)` and `merge_all(others)` (:830-885): merging with an empty side
  returns the other object itself; `merge_all` with exactly one non-empty input
  returns that input, with none returns `Runfiles.EMPTY`. Type errors use
  Bazel's binder text (`in call to merge(), parameter 'other' got value of type
  'int', want 'runfiles'`, `at index 1 of param, got element of type int, want
  runfiles`).
* The conflict policy (WARN/ERROR) propagates as the strictest one, even from
  empty runfiles (Builder.merge, :638-662). It is retained; conflicts are only
  detected when a manifest is built (N23f).
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
  private API`; allowlisted main packages are honored; external repositories
  (whose allowlist entries need apparent-name mapping) fail explicitly.

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
(`--build_runfile_links` default) and `<executable>.repo_mapping` in the
executable's directory (analysis/RunfilesSupport.java:83-86, 405-440, 684-716).
Bazel generates them (SourceManifestAction, SymlinkTreeAction,
RepoMappingManifestAction) and accepts them as action inputs, DefaultInfo files
and runfiles. Natively they are declared Files without actions until N23f:
any such use fails explicitly with `<path> is the runfiles manifest of L:
generating runfiles manifests, repository mapping manifests and runfiles trees
is not supported by the native profile (N23f, docs/RUNFILES.md)` (or `the
repository mapping manifest`). Their exact Bazel contents use absolute execroot
target paths, which the native execution layout does not have; N23f must
define the native target paths and compare normalized content.

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
  `args` (ShellUtils.tokenize) and their use by `run`/`test` are N23g.
* The implicit test attributes `$test_wrapper`, `$xml_writer`, `$test_runtime`,
  `$test_setup_script`, `$xml_generator_script`, `$collect_coverage_script` and
  the coverage/run-under attributes (StarlarkRuleClassFunctions.java:258-378) use
  the test-runner exec transition; they are recorded in the rule class but not
  analyzed or exposed in `ctx.attr` (N23g).
* `bazel build` of an executable or test target builds its files to build and,
  like OutputGroupInfo.HIDDEN_TOP_LEVEL (RuleConfiguredTargetBuilder.java:156-171),
  the artifacts of its default runfiles without reporting them. The runfiles
  tree, manifests and repo-mapping manifest are not created (N23f); `run` and
  `test` commands do not exist.

## `ctx.executable` and actions

* `attr.label(executable=True, cfg=...)` is analyzed with `cfg="target"` and
  `cfg="exec"` (the exec transition, docs/CONFIGURATION.md: the executable is
  the exec-configured dependency's, below its exec output root such as
  `bazel-out/rust-opt-exec/bin`); a missing `cfg` still fails with `cfg parameter is mandatory when executable=True is
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
  inputs. When an executable or tool carries runfiles (a FullFilesToRunProvider
  or a File from `ctx.executable` with one), analysis fails explicitly:
  attaching executable runfiles to spawns needs runfiles trees (N23f; Bazel
  accepts it).

## Native rules

* filegroup (rules/filegroup/Filegroup.java:80-119): default runfiles from the
  `srcs`' default runfiles, data runfiles from the files to build plus the
  `srcs`' data runfiles (`--incompatible_filegroup_runfiles_for_data`), a
  single executable when it has exactly one file. `data` stays gated.
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
type/repr of `DefaultInfo` (N22), `dir(ctx)` (N23c members), and the explicit
`args`, `run_lazy`/`run_provider` and source-listing differences described
below. No mismatch concerns runfiles content, order, identity or the
DefaultInfo target view.

## Not covered (explicit or documented)

`ctx.workspace_name`
(N23c) and the other ctx members (Bazel's `dir(ctx)` has 36 names, native 10),
runfiles trees, MANIFEST/`_repo_mapping`/`.repo_mapping` files and their actions
(N23f), runfiles attached to actions (N23f), test-runner implicit dependencies,
`args` tokenization and `bazel test`/`run` (N23g), the nested-set depth-limit message,
empty-files suppliers. The native CLI also lists source files among a target's
outputs, which Bazel's `--show_result` omits (pre-existing).
