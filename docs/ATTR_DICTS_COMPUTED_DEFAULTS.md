# Label dictionaries, computed defaults and analysis-test transitions

This increment (N20) unblocks loading of the embedded
`@bazel_tools//tools/build_defs/repo:http.bzl` (`http_archive`'s `files`
attribute), `@bazel_skylib//lib:unittest.bzl` and rules_cc's
`cc/common/semantics.bzl` (`_def_parser`). Citations are to the pinned Bazel
9.3.0 tree (`e89755b`), paths below
`src/main/java/com/google/devtools/build/lib/`.

## `attr.string_keyed_label_dict` and `attr.label_list_dict`

Signatures follow `starlarkbuildapi/StarlarkAttrModuleApi.java:770-886` and
`:1013-1133`, implemented by `analysis/starlark/StarlarkAttrModule.java:918-967`
and `:1023-1075`:

```starlark
attr.string_keyed_label_dict(allow_empty=True, *, configurable=unbound, default={},
    doc=None, allow_files=None, allow_rules=None, providers=[],
    for_dependency_resolution=unbound, flags=[], mandatory=False, cfg=None, aspects=[])
attr.label_list_dict(allow_empty=True, *, configurable=unbound, default={}, doc=None,
    allow_files=None, allow_rules=None, providers=[], for_dependency_resolution=unbound,
    flags=[], mandatory=False, skip_validations=False, cfg=None, aspects=[])
```

`string_keyed_label_dict` has no `skip_validations`; neither has `executable`
or `allow_single_file`. The types are `BuildType.LABEL_DICT_UNARY`
(`dict(string, label)`) and `BuildType.LABEL_LIST_DICT`
(`dict(string, list(label))`, `packages/BuildType.java:63-75`).

* Defaults: `string_keyed_label_dict` accepts a dict or function, `label_list_dict`
  only a dict (`@Param` types; otherwise `in call to KIND(), parameter 'default'
  got value of type 'T', want 'dict or function'` / `want 'dict'`). Dict
  defaults are converted with `DictType.convert` (`packages/Type.java:565-580`)
  and `Attribute.Builder.defaultValue`'s `what` (`packages/Attribute.java:699-708`):
  `expected value of type 'string' for dict value element, but got 1 (int)`,
  `expected value of type 'list(label)' for dict value element, ...`,
  `... for element 0 of dict value element ...`. Label strings resolve relative
  to the defining `.bzl` (`LabelConverter.forBzlEvaluatingThread`).
* `configurable=` (True or False) is only meaningful for symbolic-macro
  attributes: `rule()` rejects it with `attribute 'v' has the 'configurable'
  argument set, which is not allowed in rule definitions` and `aspect()` with
  `... not allowed in aspect definitions`, naming the native attribute (`$v`
  for `_v`) (`StarlarkAttrModule.java:402-410`,
  `StarlarkRuleClassFunctions.java:964-970`, `:1374-1379`); repository rules
  and tags accept and ignore it. Function defaults of these dictionaries are a
  native-profile gap (explicit failure, see below).
* Rule instances: values are type-checked, labels resolve in the target's
  package; `select()`, `select() | {...}`, `{...} | select()` and
  `x |= select()` (a vendored reverse `|` hook, docs/STARLARK_VENDOR.md;
  `SelectorList.binaryOp`, `packages/SelectorList.java:171-182`) follow the
  existing configurable attribute support (`Type.concat` dict union). An
  apparent repository the main repository does not see becomes
  `@@[unknown repo 'R' requested from @@]` (cmdline/RepositoryName.java): the
  package loads and only that dependency edge fails at analysis with `no such
  package '@@[unknown repo 'nope' requested from @@]//': The repository
  '@@[unknown repo 'nope' requested from @@]' could not be resolved: No
  repository visible as '@nope' from main repository` (then `... and
  referenced by '//pkg:t'`); this applies to every label attribute of Starlark
  rules in the main repository. Labels of the dictionary values
  are dependency edges and assumed input files (`DictType.visitLabels`,
  `packages/Type.java:504-514`). `label_list_dict` duplicates are reported per
  value list (`AggregatingAttributeMapper.checkForDuplicateLabels`,
  `packages/AggregatingAttributeMapper.java:243-264`): `Label '//:a' is
  duplicated in the 'l' attribute of rule 't'`; the same label under
  different keys is allowed, and the analysis-time duplicate check skips
  LABEL_LIST_DICT (`analysis/RuleContext.java:1553-1567`).
* Analysis: `ctx.attr.NAME` is a dict of string to Target (`string_keyed_label_dict`)
  or to a list of Targets (`label_list_dict`)
  (`StarlarkAttributesCollection.convertStringToLabelMap/ListMap`,
  `analysis/starlark/StarlarkAttributesCollection.java:214-235`, `:358-373`).
  `ctx.files.NAME` is the stable-order deduplicated union of the distinct
  prerequisites' files (`PrerequisiteArtifacts.nestedSet`,
  `analysis/PrerequisiteArtifacts.java:64-72`). There is no `ctx.file`
  entry (no `allow_single_file`). Prerequisites are validated like
  `attr.label_list` ones: `allow_files` (files without it are
  `source file '//:x' is misplaced here (expected no files)`; extension
  lists filter by name), `providers`,
  and `allow_empty=False` (`attribute must be non empty`,
  `RuleContext.checkAttributesNonEmpty`, `:1532-1550`, applies to every map;
  every empty attribute is reported, in attribute order).
* Every `ctx.attr`, `ctx.files`, `ctx.file` and `ctx.outputs` value of a rule
  implementation is frozen (Bazel's immutable lists/dicts,
  `StarlarkAttributesCollection.java:300-390`); mutation fails with Bazel's
  `trying to mutate a frozen dict value` / `trying to mutate a frozen list
  value` (vendored message, docs/STARLARK_VENDOR.md).
  `cfg`, `aspects`, `flags` (except the no-op `SKIP_CONSTRAINTS_OVERRIDE`) and
  `skip_validations` keep the existing explicit unanalyzed-feature gate;
  `allow_rules` is validated since N50 (docs/SH_BINARY.md); extension-list `allow_files` is
  validated (docs/LOAD_DECLARATIONS.md, "Extension-list `allow_files`").
* Repository rules and module-extension tags accept both kinds (Bazel does not
  restrict repository attribute types, `bazel/repository/starlark/StarlarkRepositoryModule.java:80-91`,
  `:242-256`). Strings bind with the caller's repository mapping; genuine
  `Label` values recorded by a module extension keep their identity. Type
  errors use Bazel's where-text and conversion message: `in call to
  'http_archive' repo rule with name 'arch', expected value of type 'string'
  for dict value element, but got 1 (int)` (`bazel/repository/RepoRule.java:95-105`)
  and `in 'tag' tag, ...` (`bazel/bzlmod/TypeCheckedTag.java:106-114`); they are
  reported when the repository / extension is evaluated, not while
  MODULE.bazel is evaluated (Bazel: exit 48/7 with a MODULE.bazel traceback).
  The repository rule repr is `<starlark repository rule @@//pkg:defs.bzl%name>`
  (`<anonymous starlark repository rule>` before export;
  `StarlarkRepositoryModule.java:152-159`, `bazel/bzlmod/RepoRuleId.java:31-34`). `ctx.attr`
  / tag values are immutable dicts of `Label` (lists of `Label`)
  (`Attribute.valueToStarlark`, `packages/Attribute.java:2442-2450`).
  `attr.label_keyed_string_dict` is supported there since N29 (Label-keyed
  frozen dicts, docs/GLOB_SYMLINKS.md); `attr.int_list` remains unsupported.

`attr.label_keyed_string_dict` rule values are analyzed since N50: `ctx.attr`
is a frozen dict of the prerequisite Targets to their strings
(StarlarkAttributesCollection.java:361-369), see docs/SH_BINARY.md.

## Computed (function) defaults

`attr.label(default=fn)` and `attr.label_list(default=fn)` create a
`StarlarkComputedDefaultTemplate` (`StarlarkAttrModule.java:358-368`,
`packages/Attribute.java:1370-1513`). The `Attribute` value retains the
function (repr unchanged, `<attr.label>`).

* The attribute must be private: `When an attribute value is a function, the
  attribute must be private (i.e. start with '_'). Found 'p'`
  (`packages/AttributeValueSource.java:47-58`), checked by `rule()`,
  `repository_rule()` and `tag_class()`.
* For every rule instance, after all other attributes are populated
  (`packages/AttributeProvider.java:335-341, 400-435`), the function is called with
  one positional argument per parameter (`StarlarkCallbackHelper.buildArgumentList`,
  `StarlarkCallbackHelper.java:83-99`). Parameter names are attribute names:
  `name`, public Starlark attributes (including their defaults; `attr.output`
  / `attr.output_list` values are Labels in the target's package, BuildType
  OUTPUT), `tags`, a set `visibility` (the declared labels), explicitly set
  built-ins, and built-ins whose default is plain (`features`, `toolchains`,
  `exec_properties`, `size`, `shard_count`, ...). Errors use
  `Cannot compute default value of attribute '_p' in rule '//pkg:t': ...`
  with `No such attribute X in rule //pkg:t` (unknown or private names: their
  native names are `$x`; `applicable_licenses` is not an attribute name), the
  function's own failure message (`fail("boom")` gives `boom`), or
  `expected 'label', but got 'string'` / `expected 'list(label)', ...`
  (`invokeCallback`'s `Type.cast`, `Attribute.java:1474-1500`; `None` is the
  type default).
* A parameter without a regular value fails with StructImpl's message and the
  visible fields (`packages/StructImpl.java:86-95`): `No such regular (non
  computed) attribute 'testonly'.` then `Available attributes: $config_dependencies,
  compatible_with, exec_compatible_with, exec_group_compatible_with,
  generator_function, generator_location, generator_name, name, restricted_to,
  tags, transitive_configs, visibility` (the nonconfigurable built-ins, plus
  `$is_executable`, `flaky`, `local`, `size` for test rules and
  `$is_executable` for executable rules, and the parameters that have values;
  `AggregatingAttributeMapper.createMapBackedAttributeMap`, `:533-566`). This
  is always the case for the built-in computed defaults `testonly`,
  `deprecation`, `package_metadata` and the test `timeout`, set or unset
  (`BaseRuleClasses.java:67-108, 244, 265, 293`,
  `StarlarkRuleClassFunctions.java:272-276`), and for other computed defaults
  and `None`-valued attributes.
* The computed value is the attribute's ordinary value: dependency edge,
  `ctx.attr`/`ctx.file(s)`, input-file and visibility handling all apply.
* Explicit native-profile failures: a parameter that reads a `select()` value
  (Bazel precomputes a per-combination lookup table, `StarlarkComputedDefault`);
  an unset `visibility` or unset `generator_*` parameter (genuine Bazel 9.3
  crashes on the unset visibility, exit 37); computed defaults on
  `dependency_resolution_rule` rules; function defaults of
  `string_keyed_label_dict`/`label_keyed_string_dict` (`Type.cast` of a map does
  not convert labels); computed defaults on repository rules and tags (genuine
  Bazel crashes on them, exit 37). Aspects reject them with Bazel's `Aspect
  attribute '_p' (label) with computed default value is unsupported.`
  (`StarlarkRuleClassFunctions.java:1435-1446`). A `None`-valued public label
  parameter reports `No such regular ...` natively; genuine Bazel crashes
  (exit 37).

## `analysis_test_transition`, `AnalysisTestResultInfo`, `AnalysisFailureInfo`

`analysis_test_transition(settings={...})` (`starlarkbuildapi/config/ConfigGlobalLibraryApi.java:101-130`,
`rules/config/ConfigGlobalLibrary.java:120-140`) checks at creation only the
key syntax (`invalid transition output 'compilation_mode'. If this is intended
as a native option, it must begin with //command_line_option: ...`), repository
visibility and duplicates, canonicalizes them
(`StarlarkDefinedConfigTransition.java:101-118`), and returns a `transition`
whose repr is `<analysis_test_transition object>` (`:300-357`). Settings are
retained. Option names and values (`//command_line_option:nope`, also
`experimental_*`/`incompatible_*`, a value of the wrong type, missing flag
targets) are not validated at creation: genuine Bazel fails when the analysis
test is analyzed (`transition outputs [//command_line_option:nope] do not
correspond to valid settings`), where the native profile fails at its explicit
analysis-test gate. Using it as an attribute `cfg` is restricted to `analysis_test=True`
rules: `Only rule definitions with analysis_test=True may have attributes with
analysis_test_transition transitions` (`StarlarkRuleClassFunctions.java:991-998`).
Argument errors use Bazel's texts (`analysis_test_transition() got unexpected
positional argument`, `... missing 1 required named argument: settings`,
`got dict<int, string> for 'changed_settings dict', want dict<string, unknown>`).

`AnalysisTestResultInfo` and `AnalysisFailureInfo` exist as `.bzl` globals
(`starlarkbuildapi/test/TestingBootstrap.java:46-52`; repr `<function NAME>`,
`packages/BuiltinProvider.java:93-96`). `AnalysisTestResultInfo(success,
message)` constructs the provider (`AnalysisTestResultInfoApi.java:55-79`);
`AnalysisFailureInfo` has no constructor.

`rule(analysis_test=True)` is analyzed natively (docs/BENCHMARKS.md, bazel-gazelle's
`//tests/cli:valid_env_variables_test`): the implementation may not register
actions (`StarlarkActionFactory.validateActionCreation`), must return an
`AnalysisTestResultInfo` (`RuleConfiguredTargetBuilder.build`, :261-274), and
that provider becomes the test's executable `NAME.sh`, written byte-identically
to `AnalysisTestActionBuilder.writeAnalysisTestAction` (every message character
but a newline backslash-escaped, one `echo` per line, `exit 0`/`exit 1`).
Attribute `analysis_test_transition`s (skylib `analysistest.make` with
`config_settings` or `expect_failure`, which sets
`//command_line_option:allow_analysis_failures` and reads
`AnalysisFailureInfo`) remain an explicit analysis gap, as do
`ctx.created_actions()` of `_skylark_testable` rules and `target.actions`.

## Known remaining differences (genuine capture)

`tests/fixtures/attr_dicts_computed_defaults/reference_receipt.json` records
the paired capture. Left as documented differences: Starlark traceback framing
and interpreter argument-error wording (`Found X extra named parameter(s)` vs
`got unexpected keyword argument`; since N33 the native text is Bazel's, see
docs/STARLARK_VENDOR.md); `Target`/`Label` repr (`<Target @@//:a>` /
`@@//:a` versus `<target //:a>` / `Label("//:a")`; a separate repr increment);
the native label-syntax subset (`//:a:b`); BUILD-time conversion and mandatory
messages of plain (non-`select()`) values; the mandatory-provider message form;
MODULE.bazel-time validation of repository/tag values; `http_file` (needs
`repository_ctx.path`, N21) and `http_archive` (`repository_ctx.os.environ`,
N21); genuine Bazel crashes (exit 37) for computed defaults in
`repository_rule`/`tag_class`, unset public label parameters and an unset
`visibility`; exec-configured attributes; `select()`-dependent computed
defaults; analysis tests; query output formats.
