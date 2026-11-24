# Analysis tests

bazel_skylib's `analysistest`/`unittest` and rules_testing test rules by
analyzing a target under test and inspecting its providers and actions. This
page covers the parts of Bazel 9.3.1 they rely on, and what bazel-rs does for
each. Bazel sources are relative to
`src/main/java/com/google/devtools/build/lib/` at tag 9.3.1.

`rule(analysis_test = True)` itself (the returned `AnalysisTestResultInfo`
becomes the test script `NAME.sh`, no actions allowed) is described in
[ATTR_DICTS_COMPUTED_DEFAULTS.md](ATTR_DICTS_COMPUTED_DEFAULTS.md).

Fixture: `tests/fixtures/analysis_tests` (genuine 9.3.1 capture, receipt in
`reference_receipt.json`), checked by `tests/analysis_tests.rs`.

## `analysis_test_transition`

`analysis_test_transition(settings = {...})` is an attribute transition whose
result is the settings dict itself (AnalysisTestTransition.evaluate,
`analysis/config/StarlarkDefinedConfigTransition.java:311-370`). bazel-rs
applies it like a Starlark `transition()`: the settings go through the same
conversion and validation (`crate::starlark_transitions`), and the attribute
is a list in `ctx.attr`, as for any Starlark-transitioned attribute.

Genuine's restrictions:

- Only `rule(analysis_test = True)` may use it on an attribute
  (`StarlarkRuleClassFunctions.java:991-998`).
- It does not add the function-transition allowlist attribute and is not
  checked against it (`StarlarkAttrModule.java:489-498`).
- A configuration it changes is marked `evaluating for analysis test`
  (`FunctionTransitionUtil.java:776-781`). An unchanged configuration stays
  the original one. The marker is the internal option
  `NativeOptions::evaluating_for_analysis_test`. It is part of the
  configuration identity and of the `ST-` hash, so the output roots match
  genuine's (`k8-opt-ST-b10a2bc3d406` and so on). The exec transition resets
  it.
- In such a configuration, an analysis test rule fails with `analysis_test
  rule 'L' cannot be transitively depended on by another analysis test rule`
  (`RuleConfiguredTargetBuilder.java:215-224`).

Not modelled: `--analysis_testing_deps_limit`. Genuine counts the transitive
labels behind an analysis-test transition and fails above the limit (2000).
bazel-rs never enforces the limit.

## `testing.analysis_test`

`testing.analysis_test(name, implementation, attrs, fragments, toolchains,
attr_values)` is only callable on a BUILD thread, including legacy macros.
It defines a rule class with `analysis_test = True` and `test = True`,
exports it as `NAME_test`, and instantiates the one target `NAME` with
`attr_values` (`rules/test/StarlarkTestingModule.java`).

bazel-rs runs the same `rule()` definition path
(`crate::custom_rule::analysis_test`). The rule's definition label is the
`.bzl` file of the calling frame. `query --output=build/xml/label_kind`
matches genuine. The errors use genuine's texts:

- `analysis_test can only be called in a BUILD thread`
- `'name' is limited to Starlark identifiers, got NAME`
- `'name' cannot be set or overridden in 'attr_values'`

## `Target.actions`

A rule target's `actions` field lists its registered actions as `Action`
values (`RuleConfiguredTarget.rawGetStarlarkProvider`,
`analysis/configuredtargets/RuleConfiguredTarget.java:270-281`;
`starlarkbuildapi/ActionApi.java`). skylib reads it with
`_action_retrieving_aspect`, rules_testing with `testing_aspect`.

The analysis keeps a compact view of each registered action only for the
targets an analysis test of the request can inspect: every label an
attribute of a requested `rule(analysis_test = True)` target names
(`select()` branches included), and the `actual` of each such label that
is an `alias` with a non-configurable `actual` in a package loaded by then
(chains followed; rules_kotlin's analysis tests name
`//kotlin/compiler:btapi_runtime`, an alias of `@btapi_impl//:runtime`,
B35), and the attribute labels of a Starlark rule (not an analysis test)
that such an attribute names in the test's own package (a rule under test
handing `ctx.attr.dep.actions` on in a provider, protobuf's
`attach_upb_aspect`, B33). Every target analyzed in an analysis test's own
configuration (`evaluating for analysis test`: the test's
`analysis_test_transition` changed the configuration) keeps its views
too, so a testing aspect that propagates (`attr_aspects = ["*"]`, rules_cc's
`_propagating_testing_aspect`) reads `target.actions` of the subject's
dependencies (B32). When a requested analysis test has an attribute aspect
that propagates (a non-empty or function `attr_aspects` or
`toolchains_aspects`), every target of the request keeps its views: such an
aspect visits targets outside the test's configuration and package (rules_cc's
`test_propeller_host_builds` reads the actions of an exec tool and of its
toolchain and wrapper dependencies, B32). The engine decides that set from the
requested labels and their packages before the analysis
(`Engine::inspected_targets`), so it never depends on the analysis order.
A snapshot holds no providers, and an incremental re-analysis requests
every dirty analysis test again, so neither can see a different set. The
views sit under a reserved provider key, `builtin:Target.actions`
(`crate::target_actions`). No exported provider can have that key, and the
cquery provider listings skip it. `dir(target)` lists `actions` for rule
targets, as genuine does.

Every other Starlark rule target holds one shared "not kept" marker under
that key, and `target.actions` fails there as an unknown field. That is a
deliberate, explicit gap: genuine answers the actions for any rule target.
It is reached only by a rule that is not an analysis test reading
`dep.actions`, or by an analysis test that is not requested itself (only a
dependency of the request). skylib's and rules_testing's aspects read the
direct attribute dependencies of the requested tests, which are kept.

| field | value |
|---|---|
| `mnemonic` | `FileWrite`, `TemplateExpand`, `Symlink`/`ExecutableSymlink`/`SolibSymlink`/`UnresolvedSymlink` (as aquery names them), or the spawn's mnemonic |
| `argv` | the spawn's argv (`/bin/bash -c CMD "" ARGS…` for `run_shell`), parameter files expanded like SpawnAction.getArguments (an argument naming one of the action's parameter files, `@path`, `--flagfile=path` or the bare path of `use_param_file("%s")`, is replaced by the arguments it holds; `shell` quoting undone; a `flag_per_line` file's `--flag=value` lines stay single arguments). A command line natively deferred to execution (Args over tree artifacts) is expanded as genuine's analysis does without input metadata: a directory stays its path, or what a `map_each` maps it to with a NoopExpander (a `map_each` closure retained with `allow_closure` is not callable before the target's module freezes and leaves the path). A Java compilation (`java_common.compile`'s Javac, JavacTurbine) answers JavaCompileAction.getArguments: the launcher, the flags and `--classpath`, as aquery prints them, without JavaBuilder's parameter file. `None` for writes and template expansions |
| `content` | the written content; for a template expansion, the source template read when `target.actions` is accessed (an observed read), with the substitutions applied; `None` otherwise |
| `substitutions` | the template expansion's substitutions; `None` otherwise |
| `inputs`, `outputs` | depsets of the action's files, in registration order |
| `env` | the fixed environment |
| `args` | always `None` (the `Args` objects are not kept) |

A target merged with aspects (an aspect's `target` parameter merged with
the aspects it requires, an attribute's aspect-applied dependency) answers
the aspects' actions in application order, then its own
(MergedConfiguredTarget.rawGetStarlarkProvider); an aspect application keeps
its views when its base target is inspected (rules_testing's
`make_testing_aspect(aspects = [...])` over protobuf's `java_proto_aspect`).

An executable's views end with the actions its RunfilesSupport registers
(RunfilesSupport.java:280-347, 467-545): `RepoMappingManifest`, with
runfiles manifests `SourceSymlinkManifest` (the input manifest), with
runfile links `SymlinkTree` (input: the input manifest; env: the
configuration's action environment), then `RunfilesTree` (inputs: the
runfiles' artifacts, the manifest and the repository mapping manifest;
output: the `<executable>.runfiles` tree artifact); `argv` is `None` for all
four.

A kept `genrule` answers its GenRuleAction (`Genrule`; argv `/bin/bash -c
"source <genrule-setup.sh>; CMD"`; inputs the `srcs`, the tools' files to
run with their runfiles tree artifacts, `genrule-setup.sh` and the
workspace status files of a stamping genrule; env the action environment;
GenRuleBase.java:168-232; B32: rules_cc's `test_fdo_profile_action_graph`).
The other native rules do not keep their actions. Their targets answer an
empty list, except `genquery` and `starlark_doc_extract`, which register
actions: for those, and for a genrule that is not kept, `actions` is not a
field. A failed target in a configuration that allows analysis failures has
no actions.

Cost: on abseil-cpp (`build --nobuild //...`, 3,722 actions, about 2,000
of them registered by Starlark rules, no analysis tests), the retained
memory (`BAZEL_RS_TIMING_ALLOC=1`, PERF-ALLOC-SUMMARY `total`) is 182,838,914
bytes against 182,771,274 without the feature. The difference, +68 KB, is
the marker's map entries, one per Starlark rule target. Keeping every
target's views had cost +4.5 MB (about 2.2 KB per action). Peak RSS is
441–445 MB against 440–444 MB, which is within noise.

`ctx.created_actions()` of a `_skylark_testable` rule (skylib's
`unittest.make`) still fails explicitly.

## `--allow_analysis_failures` and `AnalysisFailureInfo`

`--allow_analysis_failures` is a modelled option. It works on the command
line, in `config_setting(values)` and in transitions, and the exec
transition propagates it. skylib's `analysistest.make(expect_failure =
True)` sets it to `"True"` through an `analysis_test_transition`.

In a configuration that allows analysis failures:

- A Starlark rule whose implementation fails becomes a configured target
  with only an `AnalysisFailureInfo` and no files or actions
  (`ConfiguredTargetFactory.erroredConfiguredTarget`,
  `analysis/ConfiguredTargetFactory.java:585-622`). The same happens when it
  reports rule errors. Its `causes` depset holds one
  `AnalysisFailure(label, message)` per suppressed error.
- A Starlark rule with a prerequisite that carries an `AnalysisFailureInfo`
  does not run its implementation. It propagates the prerequisites' causes
  (`depAnalysisFailures`, `erroredConfiguredTargetWithFailures`, :410-417,
  545-583).
- A Starlark rule's prerequisite validation errors (a file of a type the
  attribute does not allow, a missing input file:
  RuleContext.Builder.validateDirectPrerequisite) are its suppressed
  errors: the target does not run its implementation and its `causes` are
  the attribute errors' own messages, without the `in ATTR attribute of
  KIND rule L: ` context (B32; rules_cc's
  `test_wrong_cc_import_definitions_fails`).
- An aspect applied to a target that carries an `AnalysisFailureInfo` is
  the errored aspect: no validation, no implementation, the base target's
  causes (ConfiguredTargetFactory.createAspect :724-727); the merged
  target's `AnalysisFailureInfo` joins the base's and the aspects' causes
  (MergedConfiguredTarget.getAnalysisFailures :335-350).
  tests/fixtures/corpus_rulescc `prerequisite_failures_allowed` is the
  genuine capture.

The message of an implementation error is genuine's `"\n" +
getMessageWithStack()`. That is `Traceback (most recent call last):`, then
`\tFile "ABSOLUTE PATH", line L, column C, in FUNCTION` and the trimmed
source line for each frame, then `Error in fail: MESSAGE`
(`crate::analysis_failures::message_with_stack`). The paths of `.bzl` files
in external repositories are bazel-rs's repository roots.

These errors are never suppressed, so they cannot pass as expected failures:

- native-profile gaps (`not implemented in the native profile`, declared
  features that are not analyzed);
- internal errors;
- the engine's retry signals (deferred target platform, repository demand).

Not modelled:

- Other errors raised before the implementation runs (visibility, errors
  of the dependency resolution) stay ordinary failures. In genuine they are
  rule errors and are suppressed too.
- Native rules never become failure targets.
- `AnalysisFailure` elements are struct-like provider instances. Their
  fields are right, but `type()` and `repr()` differ from genuine's.
