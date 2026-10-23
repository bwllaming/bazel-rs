# Load-time rule declarations (Bazel 9.3)

Rulesets in the application `query` load closure (rules_go 0.63, rules_cc
0.2.17, gazelle, rules_java, rules_python, protobuf, grpc) declare
analysis-time features while their `.bzl` files are evaluated. The native
profile now **accepts, validates and keeps** these declarations where pinned
Bazel 9.3 does it at load time. It does **not** analyze them: when building a
target would use one of these features, analysis fails explicitly with
`declared rule features accepted at load time but not analyzed by the native
profile: <feature>; ...`. A target that declares a feature but does not use it
(for example an empty `cfg = config.none()` label list) still analyzes as
before. The exec transition (`cfg = "exec"`, `"host"`, `config.exec()`) is
analyzed since N18b (docs/CONFIGURATION.md "Exec transition").

Pinned sources: `/tmp/opencode/bazel-9.3-reference` (e89755b, Release 9.3.0).
Paths below are relative to `src/main/java/com/google/devtools/build/lib/`.
Code comments cite file:line.

## New `.bzl` globals

| Global | Bazel source | Native behavior |
| --- | --- | --- |
| `aspect(...)` | `StarlarkRuleFunctionsApi.java:790-1028`, `StarlarkRuleClassFunctions.java:1323-1520` | Every parameter in Bazel's order (`implementation`, `attr_aspects`, `toolchains_aspects`, `attrs`, `required_providers`, `required_aspect_providers`, `provides`, `requires`, `propagation_predicate`, `fragments`, `host_fragments`, `toolchains`, `doc`, then named-only `apply_to_generating_rules`, `exec_compatible_with`, `exec_groups`, `subrules`). Bazel checks and messages for aspect attributes (public ones must be bool/int/string; `values` default; a private attribute needs a value, which only `attr.label` without `default` and outputs lack, because every other `attr.*()` has a non-None default `@Param`), the generating-rules conflicts, `'*'` in `attr_aspects`/`toolchains_aspects`, exec group names and toolchains. `subrules` are accepted from any `.bzl` (`--experimental_rule_extension_api` defaults to true, `FlagConstants.java:24`). Type `Aspect`, repr `<aspect>`, exported identity `<bzl>%<name>`. Metadata is kept; callbacks are never called. |
| `transition(implementation, inputs, outputs)` | `ConfigGlobalLibraryApi.java:33-95`, `ConfigGlobalLibrary.java:49-204`, `StarlarkDefinedConfigTransition.java:100-186` | Named-only parameters (`transition() got unexpected positional argument`), `validateBuildSettingKeys` (`--experimental_*`/`--incompatible_*` options; non-option settings must be absolute labels, checked with `Label.parseWithRepoContext` syntax and its exact texts, e.g. `invalid label ':flag': absolute label must begin with '@' or '//'`; duplicates), then canonical duplicate detection (outputs first, then inputs). Type `transition`, repr `<transition object>`, `dir()` is `["and_then"]`. |
| `config.exec(exec_group=None)`, `config.target()`, `config.none()` | `StarlarkConfigApi.java:174-205`, `StarlarkConfig.java:68-83` | Added to the existing `config` module. Type `ExecTransitionFactory`/`transition`; reprs use Bazel's default `<unknown object ...ExecutionTransitionFactory>` / `...NoTransition$Factory>` / `...NoConfigTransition$Factory>`. `config.exec()` values compare by identity (two calls are never equal); `config.target()`/`config.none()` are records and compare by value. All transition values expose `and_then` (`ConfigurationTransitionApi.java:37-56`); calling it fails explicitly (composition is not implemented). |
| `rule` / `attr.*` values | `StarlarkRuleClassFunctions.java:1999-2005`, `StarlarkAttrModule.java:1218` | Exported rules print `<rule NAME>`, unexported `<rule>`; attribute descriptors have type `Attribute` and repr `<attr.KIND>` (for example `<attr.string_dict>`). |
| `select(x, no_match_error='')` (in BUILD and `.bzl`) | `SelectorList.java:60-310`, `SelectorValue.java:90` | Dict check, empty-dict and key-type errors, `+` (and `|` for dicts) concatenation with `Cannot combine incompatible types (...)`, type `select`, Bazel repr. Keys stay strings (`--incompatible_resolve_select_keys_eagerly` is off by default, `BuildLanguageOptions.java:854`). |
| `OutputGroupInfo` | `OutputGroupInfo.java`, `BuiltinProvider.java:94` | Type `Provider`, repr `<function OutputGroupInfo>`, usable in `providers=`/`provides=` (identity `builtin:OutputGroupInfo`). Calling it constructs an output-group provider (N25d5, docs/CC_TOOLCHAIN.md). |

`select` was not available in native BUILD files before; it is now a BUILD
global too. Custom rules type-check and keep `select()` values. Native rule
classes (including `filegroup`/`genrule`) accept them on configurable
attributes. Analysis resolves them per configuration (docs/CONFIGURATION.md).

## `rule()` parameters

`implementation` can now be positional, as in Bazel. All Bazel 9.3 parameters
are accepted (`StarlarkRuleFunctionsApi.java:428-788`): `test`, `attrs`,
`outputs`, `executable`, `output_to_genfiles`, `fragments`, `host_fragments`,
`_skylark_testable`, `toolchains`, `doc`, `provides`,
`dependency_resolution_rule`, `exec_compatible_with`, `analysis_test`,
`build_setting`, `cfg`, `exec_groups`, `initializer`, `parent`, `extendable`,
`subrules`. Validation follows `StarlarkRuleClassFunctions.java:548-1171`:

* test naming at export: `Invalid rule class name '%s', test rule class names
  must end with '_test' and other rule classes must not` (`:1931-1937`;
  `analysis_test=True` implies `test`);
* `cfg`: `transition()`, `config.target()` or `config.none()`; anything else
  (including `config.exec()`, which only applies to attributes) gives
  ``"`cfg` must be set to a transition object initialized by the transition()
  function."``; `build_setting` with `cfg` gives Bazel's message;
* `toolchains` (`:2213-2258`): strings resolved from the `.bzl`, `Label`s or
  `config_common.toolchain_type` values, deduplicated (mandatory wins);
* `exec_groups`: `Dict.cast` and `Exec group name '%s' is not a valid name.`;
* `outputs`: a dict of strings (Bazel's `Dict.cast` message) or a function;
* `extendable`, `initializer`, `exec_compatible_with`, `provides`,
  `dependency_resolution_rule` checks, `parent` checks (`:585-622`).

The values are kept in the rule definition (`RuleDeclaration`).

Load-time semantics that are implemented:

* **`initializer`** (`StarlarkRuleClassFunctions.java:1810-1885`): runs at
  instantiation with `name` plus public Starlark attributes (label values
  turned into `Label`s), merges the returned dict with Bazel's checks (cannot
  rename the target, invalid identifiers, `file '...' cannot use private API`
  for private attributes outside the rules_cc allowlist, `Initializer can only
  set Starlark defined attributes, not '%s'`). Label strings it returns are
  resolved from the rule's `.bzl` package.
* **`outputs=...`** (N46, docs/IMPLICIT_OUTPUTS.md): dict templates with
  `%{attr}`/`%{dirname}`/`%{basename}` placeholders and the function form are
  evaluated at instantiation with Bazel's messages; the results are output
  files of the package (`query //pkg:name.dwp` works) and `ctx.outputs`
  entries at analysis.
* **`attr.string/int(values=...)`**: checked at instantiation only for values
  that are supplied (by the call or by the initializer) and for every
  `select()` branch, never for defaults (`AttributeProvider.java:288-290,
  478-497`). Repository rules and module-extension tags check supplied values
  too, with `AttributeUtils.java:90-98`'s `in %s, the value for attribute '%s'
  %s` (where-text `call to 'NAME' repo rule with name 'X'` /
  `'TAG' tag`).
* **`provides=[...]`**: checked after analysis for every advertised key,
  Starlark and built-in (`platform_common.ToolchainInfo`,
  `platform_common.TemplateVariableInfo`, `config_common.FeatureFlagInfo`,
  `RunEnvironmentInfo`, `InstrumentedFilesInfo`, `testing.ExecutionInfo`, ...;
  returned or forwarded from a dependency), except `DefaultInfo` and
  `OutputGroupInfo`, which every rule target has:
  `rule advertised the '%s' provider, but this provider was not among those
  returned` (`StarlarkRuleConfiguredTargetUtil.java:138-148`; built-in cases
  pinned by tests/fixtures/execution_info).
* **`select()` values in BUILD files**: each branch is type-checked with
  Bazel's conversion text, prefixed by the target label (`//pkg:x: expected
  value of type 'list(label)' for each branch in select expression of
  attribute 'deps' of 'r' (including '//conditions:default'), but got 1
  (int)`, `BuildType.java:654-668`, `Type.java:296-321`). Output attributes
  and a build setting's `build_setting_default`/`help` are not configurable
  (`//pkg:x: attribute "out" is not configurable`, `BuildType.java:234-238`,
  `RuleClass.java:904-914`).

Parameter type errors that Bazel's `@Param(allowedTypes)` binder raises before
the function body (for example `allow_files`, `values`, `exec_groups`,
`fragments`, `extendable`, `propagation_predicate`, `flags`, `aspects`,
`subrules`) use its text: `in call to %s(), parameter '%s' got value of type
'%s', want '%s'`. Label strings in `toolchains`, `exec_compatible_with`,
`toolchains_aspects`, `extendable` and transitions are first checked with the
Bazel label grammar (`src/bazel_label.rs`), so syntax errors carry Bazel's
`LabelSyntaxException` text. `toolchains` type errors name the Java class
(`Int32`, `String`, `MutableStarlarkList`, ...), like the language-module
branch. Defaults of `attr.int_list` and `attr.label_keyed_string_dict` report
Bazel's conversion messages (`expected value of type 'int' for element 0 of
parameter 'default' of attribute '', but got "a" (string)`; `duplicate labels
in parameter 'default' of attribute 'label_keyed_string_dict': //a:b (as
["//a:b", "@@//a:b"])`). Exported-aspect checks use `Aspects should be
top-level values in extension files that define them.`
(`AspectsList.java:352`). `allow_empty=False` failures at analysis use
`LOC: in ATTR attribute of RULE rule LABEL: attribute must be non empty` at the
rule's location (`RuleContext.java:1548`, `EventHandlingErrorReporter.java:92-101`).

## `attr` changes

Following `StarlarkAttrModuleApi.java` and `StarlarkAttrModule.java:317-565`:

* `attr.label`/`label_list`/new `label_keyed_string_dict`: `allow_files` and
  `allow_single_file` take `True`/`False`/`None`/a list of extensions
  (`%s should be a boolean or a string list`, `Cannot specify both
  allow_files and allow_single_file`); `cfg` takes `"target"`, `"exec"`,
  `"host"` (exec while `--incompatible_disable_starlark_host_transitions`
  is off, its default; with it, Bazel's `'cfg = "host"' is deprecated ...`
  error, docs/CONFIGURATION.md), `transition()`, `config.exec/target/none()` (otherwise Bazel's
  `cfg must be either 'target', 'exec' ...`); `executable=True` requires `cfg`;
  `aspects=[...]` takes exported aspects; also `allow_rules`, `flags` (each
  a PropertyFlag name, else `unknown attribute flag 'X'`, Attribute.java:466-479),
  `skip_validations`, `for_dependency_resolution`; nested `providers` lists
  (OR of ANDs) as before.
* `attr.label(default=configuration_field(...))`: a late-bound default,
  recognized by Starlark type `LateBoundDefault`. It must be on a private
  attribute (`When an attribute value is a function, the attribute must be
  private ...`) and keeps its `configuration_field` identity; the configured
  target resolves it (docs/SUBRULES.md) when the field is resolvable natively.
* New `attr.int_list(mandatory, allow_empty, default, doc)` and
  `attr.label_keyed_string_dict(...)`; `attr.string_keyed_label_dict(...)`
  and `attr.label_list_dict(...)` (docs/ATTR_DICTS_COMPUTED_DEFAULTS.md).
* `allow_empty` on `string_list`, `label_list`, `output_list`, `int_list`
  (checked at analysis, like the existing dict attributes); `values` on
  `string`/`int`.

### Extension-list `allow_files` / `allow_single_file`

A list becomes `allowedFileTypes(FileType.of(list))`
(`StarlarkAttrModule.java:127-140`, `Attribute.java:848-868`): a one-entry
`FileTypeSet` with strict label checking, never `NO_FILE`, even for `[]`.
Every prerequisite of a Starlark `attr.label`, `attr.label_list`,
`attr.string_keyed_label_dict` or `attr.label_list_dict` is validated like
`RuleContext.Builder.validateDirectPrerequisite` (`RuleContext.java:2165-2172`):

* Matching is `FileType.hasExtension` off Windows: a case-sensitive
  `String.endsWith` per entry (`util/FileType.java:109-133`), so entries need
  no leading dot, can be exact names (`Makefile`) or multi-dot suffixes
  (`.tar.gz`), and `""` matches everything.
* A file at the end of the alias chain (`validateDirectPrerequisiteType`,
  `:1845-1886`) is tested by its label name. On a mismatch, a missing input
  file is `target 'L' does not exist` (always `target`, the set is not
  `NO_FILE`); otherwise `source file 'L' is misplaced here (expected T)` /
  `generated file ...`, with the alias description. A missing input file
  whose name matches passes analysis, as in Bazel; as an action input it fails
  at execution with `missing input file 'L'`, the native action described by
  its id (`//:t#0 failed: ...`) where Bazel uses the action's progress message
  (`Action t.cat failed: ...`, plus a location-without-colon summary line).
* Then `validateDirectPrerequisiteFileTypes` (`:1945-1994`): after the
  `allow_single_file` check (`'L' must produce a single file`), one of the
  prerequisite's files to build must match by file name, else `'L' does not
  produce any RULE ATTR files (expected T)` (ATTR is Bazel's native name,
  `$x` for a private `_x`; L the alias's actual). A mismatched file therefore
  gets both errors, like Bazel. The same check runs for `allow_files=True` on
  an `allow_empty=False` list or `allow_single_file=True` (`expected any
  files`).
* T is `FileTypeSet.toString`, `StringUtil.joinEnglishList` of the entries in
  declaration order (`util/FileTypeSet.java:143-146`,
  `util/StringUtil.java:35-73`): `.go`, `.go or .s`, `.go, .s, or .h`,
  `nothing` for `[]`.
* Rule prerequisites are not filtered: `ctx.files` is all files to build
  (`PrerequisiteArtifacts.nestedSet`, `PrerequisiteArtifacts.java:64-72`) and
  `ctx.file` the single file (`PrerequisitesCollection.java:253-271`). The
  nested set is a stable-order union of the prerequisites' files to build:
  for a label list (and the built-in dependency lists, an aspect's
  `ctx.rule.files`) each File appears once, at its first occurrence in
  prerequisite order — `deps = [":gen", ":fg"]` with `fg` a filegroup of
  `gen`'s file is `[gen.txt]`, so action inputs and command lines built from
  it list the file once (paired capture `tests/fixtures/label_list_files`,
  9 genuine 9.3.0 cases, all MATCH; `tests/label_list_files.rs`).
* Prerequisite errors follow `RuleContext.Builder.build`
  (`RuleContext.java:1491-1503`): after the prerequisites are analyzed and
  before the implementation runs, every prerequisite of every attribute is
  validated in attribute declaration order, then prerequisite order
  (`createTargetMap`, `:1708-1740`): missing mandatory providers
  (`validateRuleDependency`), the file checks above, then a package group
  outside `visibility`, whose message carries its own prefix
  (`CommonPrerequisiteValidator.java:48-59, 343-378`), so Bazel prints `LOC:
  in srcs attribute of K rule L: in srcs attribute of K rule L: package group
  '//:pg' is misplaced here (they are only allowed in the visibility
  attribute)` (the inner name is the native one, `$x`). Then
  `validateExtraPrerequisites`: empty `allow_empty=False` attributes, then
  duplicate labels (`:2102-2113`). All are attribute errors at the rule's
  location, all reported; identical events (same location and text, e.g. a
  file and an alias to it) are reported once, as a configured target's
  events form a NestedSet (`skyframe/SkyFunctionEnvironment.java:318-321`,
  `collect/nestedset/NestedSetBuilder.java:43, 86`). Visibility,
  `config_setting` visibility and testonly checks still fail earlier, on
  their own. `label_keyed_string_dict` keys are validated like any
  prerequisite since N50, and `allow_rules` applies RuleContext's rule-class
  check with the mandatory-provider fallback (docs/SH_BINARY.md).
* A source directory is an input-file target like any file, matched by name
  (`godir.go` passes `[".go"]`, `dir` gets the error pair). Using one as an
  action input or requested output (Bazel tracks source directories
  unsoundly) fails explicitly: `source directory 'L' as an action input is
  not supported by the native profile`; likewise as a requested target's
  runfile, which is built with it (N23a hidden top-level runfiles): `T:
  source directory 'L' in its runfiles is not supported by the native
  profile`.

## Accepted but not analyzed (explicit analysis failure when used)

`rule(executable=True)`, `rule(test=True)` and `attr.label(executable=True,
cfg="target")` are analyzed since N23a (docs/RUNFILES.md).

A target fails analysis with the message above if it uses any of these:

* rule level: `analysis_test=True`, `_skylark_testable=True`, `output_to_genfiles=True`,
  `dependency_resolution_rule=True`, `cfg =
  config.none()` (a Starlark `transition()` is applied since N18c,
  docs/CONFIGURATION.md) (`outputs` implicit outputs are analyzed since
  N46, docs/IMPLICIT_OUTPUTS.md);
* attribute level, when the attribute's resolved value (explicit, default or
  chosen `select()` branch, docs/CONFIGURATION.md) is non-empty:
  `cfg` other than target, exec and a Starlark `transition()` (N18c:
  `config.none()`, `analysis_test_transition`),
  non-empty `aspects`, non-empty `flags` other than `SKIP_CONSTRAINTS_OVERRIDE`
  (a documented no-op, docs/SH_BINARY.md), `skip_validations=True`;
* always: a late-bound (`configuration_field`) default whose field is not
  resolved natively (resolved fields are ordinary values, docs/SUBRULES.md);
* `aspect()` objects are never applied; `transition()` implementations are
  called at analysis (N18c, docs/CONFIGURATION.md);
  `attr_aspects`/`toolchains_aspects` functions and `propagation_predicate` are
  never called; `AnalysisTestResultInfo(...)` fails when called
  (`OutputGroupInfo(...)` is implemented, docs/CC_TOOLCHAIN.md); `analysis_test_transition()` objects are retained, restricted
  to `analysis_test=True` rules, and never applied.

`toolchains` and `exec_compatible_with` drive toolchain resolution
(docs/TOOLCHAINS.md).

`fragments` decides which configuration fragments `ctx.fragments` may
read (docs/CTX_SURFACE.md). Accepted, kept, and with no effect on analysis:
`host_fragments` (ignored by Bazel 9.3 too), `doc`, `extendable`, `for_dependency_resolution`,
`subrules` (existing behavior), `provides` (checked).

## Remaining explicit boundaries

* `rule(parent=...)`: Bazel's checks run, then it fails explicitly (rule
  extension, inherited attributes and `super()` are not implemented natively).
* Probes that need `exec_group()` or `configuration_field()` (language-module
  branch) cannot reach the ported checks in this branch.
* `configurable=` on the older `attr.*` functions (unchanged profile).
  `attr.string_keyed_label_dict`, `attr.label_list_dict` (whose `configurable=`
  is rejected by `rule()`/`aspect()` like Bazel),
  computed (function) defaults of private `attr.label`/`attr.label_list`
  attributes and `analysis_test_transition` are implemented by
  [ATTR_DICTS_COMPUTED_DEFAULTS.md](ATTR_DICTS_COMPUTED_DEFAULTS.md); its
  remaining explicit failures (computed defaults reading `select()` values or
  unmodelled built-in defaults, function defaults of label dictionaries,
  computed defaults of aspects/repository rules/tags) are listed there.
* `dict | select(...)` (dict on the left) is not supported by the vendored
  Starlark dict `|`; `select(...) | dict` and `+`/`+=` with lists work.
* `IntList` attributes in repository rules and module extension tags fail
  explicitly when their values are materialized (`string_keyed_label_dict`,
  `label_list_dict` and, since N29, `label_keyed_string_dict` are supported
  there; docs/GLOB_SYMLINKS.md).
* Built-in attributes (`tags`, `testonly`, `size`, ...) are accepted by
  custom rule instantiation since the common-attributes increment
  (docs/COMMON_RULE_ATTRIBUTES.md); defining an attribute with a built-in
  name fails with Bazel's `built-in attributes cannot be overridden`.
* The BUILD error footer was updated by the native-rule-class increment
  (docs/NATIVE_BUILD_RULES.md, "Error footer"); validators accept the current
  and the legacy literal.

## Integration with language modules

`config_common.toolchain_type`, `exec_group` and `configuration_field` are
defined by the language-module globals (separate branch). This module only
recognizes them by Starlark type (`toolchain_type` with
`toolchain_type`/`mandatory` fields, `exec_group`, `LateBoundDefault`), so the
two changes merge independently. After merging, the language-module
preflight that rejects late-bound defaults (`AttrKind::preflight_value_with_budget`)
no longer sees `attr.label` defaults, because `dependency_descriptor` takes
late-bound values out before coercion. It still applies to values given in
BUILD files.
