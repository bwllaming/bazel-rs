# C++ toolchain features, variables and tool selection

Bazel 9.3 (`e89755babf3e941bc1e9d8120b1cd48854ab9dfa`) turns a Starlark
`CcToolchainConfigInfo` into a feature graph, selects features for a request,
expands flag and environment templates with build variables, and selects the
tool of an action. rules_cc 0.2.17 drives all of this from Starlark
(`create_cc_toolchain_config_info`, `configure_features`, `cc_common.*`).
The native profile ports that engine: `src/cc_features.rs` (parsing,
graph, selection, tools, Starlark surface) and `src/cc_variables.rs`
(variables, string templates, expansion).

Citations use `J = src/main/java/com/google/devtools/build/lib/rules/cpp` of
the pinned Bazel tree.

## Entry points

| Starlark | Bazel | Native |
| --- | --- | --- |
| `cc_internal.cc_toolchain_features(toolchain_config_info=, tools_directory=)` | `J/CcStarlarkInternal.java:256-270` | `CcToolchainFeatures` value |
| `<CcToolchainFeatures>.configure_features(requested_features=)` | `J/CcToolchainFeatures.java:1229-1245` | `FeatureConfiguration` value |
| `<CcToolchainFeatures>.default_features_and_action_configs()` | `J/CcToolchainFeatures.java:1301-1309` | frozen list |
| `<FeatureConfiguration>.is_enabled(feature)` / `.is_requested(feature)` | `J/FeatureConfigurationForStarlark.java:71-91` | bool |
| `cc_internal.cc_toolchain_variables(vars=)` | `J/CcStarlarkInternal.java:133-144` | `Variables` (MapVariables) |
| `cc_internal.combine_cc_toolchain_variables(parent, *variables)` | `J/CcStarlarkInternal.java:145-168` | `Variables` |
| `cc_internal.intern_string_sequence_variable_value(seq)` | `J/CcStarlarkInternal.java:170-184` | frozen list |
| `cc_internal.exec_os(ctx)` | `J/CcStarlarkInternal.java:496-502` | OS enum name |
| `cc_common.get_tool_for_action` | `cc_common_bazel.bzl:21-22`, `J/CcModule.java:61-73` | string |
| `cc_common.get_execution_requirements` | `cc_common_bazel.bzl:24-25`, `J/CcModule.java:86-96` | frozen list |
| `cc_common.get_tool_requirement_for_action` | `cc_common_bazel.bzl:54-56`, `J/CcModule.java:75-85` | frozen list |
| `cc_common.action_is_enabled` | `cc_common_bazel.bzl:27-28`, `J/CcModule.java:98-106` | bool |
| `cc_common.get_memory_inefficient_command_line` | `cc_common_bazel.bzl:30-31`, `J/CcModule.java:108-118` | frozen list |
| `cc_common.get_environment_variables` | `cc_common_bazel.bzl:33-34`, `J/CcModule.java:120-132` | frozen dict |
| `cc_common.empty_variables()` | `cc_common_bazel.bzl:36-37`, `J/CcModule.java:134-138` | empty `Variables` |

The `cc_common` members keep their `@_builtins` def shape (`<function
_get_tool_for_action from @@_builtins//:common/cc/cc_common_bazel.bzl>`,
keyword-only parameters, Starlark-function argument errors); the internal
`CcModule` parameter checks then report `in call to get_tool_for_action(),
parameter 'feature_configuration' got value of type 'int', want
'FeatureConfiguration'` (`want 'Variables'` / `'string'` likewise).
`get_tool_requirement_for_action` first applies `@_builtins`'
`PRIVATE_STARLARKIFICATION_ALLOWLIST` (`builtins_bzl/common/cc/cc_helper_internal.bzl:21-64`).
`configure_features`, `default_features_and_action_configs`, `is_enabled` and
`is_requested` apply `BuiltinRestriction.INTERNAL_STARLARK_API_ALLOWLIST` to the
calling `.bzl` (`CcModule.checkPrivateStarlarkificationAllowlist`,
`J/CcModule.java:189-192`) after their parameter type checks, failing with
`file '//pkg:x.bzl' cannot use private API`.

Values: `type()` is `CcToolchainFeatures`, `FeatureConfiguration` and
`Variables`; `repr()` is Bazel's default `<unknown object CLASS>` with
`CcToolchainFeatures`, `FeatureConfigurationForStarlark` and
`CcToolchainVariables$MapVariables`/`$SingleVariables`. `str()` of a feature
configuration is `<FeatureConfiguration(ENABLED:a, b;REQUESTED:x, y)>` and
`print()` uses the `debugPrint` form `<FeatureConfiguration(a, b)>`. Bound
methods are `<built-in method is_enabled of FeatureConfiguration value>`.
Lists and dicts that Bazel returns immutable (`StarlarkList.immutableCopyOf`,
`Dict.immutableCopyOf`) are allocated frozen, so mutating them fails.

## Parsing (`J/CcToolchainFeaturesLib.java:55-553`, `J/CcToolchainConfigInfo.java:56-92`)

Fields are read from the `_features_DO_NOT_USE`, `_action_configs_DO_NOT_USE`,
`_artifact_name_patterns_DO_NOT_USE` and `_exec_os_DO_NOT_USE` fields of any
struct or provider instance, in Bazel's order: all features, then the exec OS,
all action configs, the duplicate checks and the artifact name patterns.
Each struct must carry the expected `type_name` (`Expected object of type
'feature', received 'flag_set'.`). Mandatory and typed fields, nonempty names,
the `^[_a-z0-9+\-\.]*$` name rule, flag-group shape, action-config flag sets,
tool `path`/`tool` exclusivity, artifact categories and allowed extensions all
use Bazel's texts, including `Field 'name' is not of 'java.lang.String' type.`
and the `Sequence.cast` texts (`for implies, got string, want sequence`,
`at index 0 of flag_sets, got element of type string, want struct`).
`infoError` messages are prefixed `in <provider name or struct> instantiated
at <location>: ` as in Bazel (`CcToolchainFeaturesLib.java:55-62`). For a
provider instance the location is its creation call site
(`Info.getCreationLocation()`; `StarlarkProvider` passes
`thread.getCallerLocation()`): `<file path>:<line>:<column>` of the call's
opening parenthesis, 1-based, with a byte column, e.g.
`cc_toolchain_config_lib.bzl:429:23` for rules_cc's `FeatureInfo(` call.

Tool paths are normalized as Unix `PathFragment`s. An absolute `path` is used
verbatim (FILESYSTEM_ROOT); a relative `path` is resolved against
`tools_directory` (CROSSTOOL_PACKAGE, `getRelative` then `getSafePathString`,
so `../x` may climb out of it); `tool = File` uses the file's exec path
(WORKSPACE_ROOT). `J/CcToolchainFeatures.java:566-606`.

## Graph and selection

Duplicate action names (`multiple action configs for action 'x'`), duplicate
selectable names (`feature or action config 'x' was specified multiple
times.`) and undefined `implies`/`requires` references (`feature 'x', which is
referenced from feature 'y', is not defined.`) are rejected while building the
graph (`J/CcToolchainFeatures.java:1099-1217, 1319-1328`).

`FeatureSelection.run` (`J/FeatureSelection.java:145-300`) is ported directly:
requested selectables that exist (unknown names are ignored but still reported
as requested) and everything they transitively imply are enabled; then a
selectable is removed while it is not (requested or implied by an enabled
selectable), not all of its implications are enabled, or none of its
`requires` sets is fully enabled, re-checking implying, requiring and implied
selectables. All three conditions are monotone, so the native worklist reaches
the same greatest fixpoint as Bazel's recursion. Enabled features are reported
in declaration order. Two enabled providers of one symbol fail with
`Symbol sym is provided by all of the following features: p1 p2`.
`with_features` sets on flag sets, env sets and tools see only enabled
*features*, never action-config names (`enabledFeatureNames`).

Configurations are cached per requested *set*: a second request with the same
names in another order returns the first configuration, including its
`REQUESTED:` order (`J/CcToolchainFeatures.java:1222-1281`).

## Variables and expansion

`StringValueParser` (`J/CcToolchainVariables.java:140-253`) splits templates into
literal, `%{name}` and `%{path:rel}` chunks with Bazel's quirks (`%%` escapes
`%`; `abc%%def` is the two chunks `abc` and `%def`; error positions count
UTF-8 bytes, because Bazel's Java strings hold the UTF-8 bytes as Latin-1
characters, so `é%x` fails at position 3): `Invalid toolchain configuration: expected '{' at position 1 while
parsing a flag containing '%x'`, `expected variable name`, `expected path after
'path:'`, `expected relative Unix-style path after 'path:'`.

Variable lookup (`J/CcToolchainVariables.java:323-465`) follows `MapVariables`
(keys sorted, parent chain; a key bound to None hides the parent and is
unavailable) and `SingleVariables`. Dotted names first try the whole name, then
the longest bound prefix and read structure fields; errors keep Bazel's
outermost structure path (`Cannot expand variable 's.nope': structure s
doesn't have a field named 'nope'` for `%{s.inner.nope}`). Values convert as
`asVariableValue` (`:826-840`): bool (`1`/`0`, truthiness), string, Label
(canonical form), File (exec path), list/tuple/dict keys/set/range and depset
(sequences), struct and provider instances (structures, always truthy).

Each `Variables` instance (and each per-element `iterate_over` binding) keeps
Bazel's `structuredVariableCache` (`J/CcToolchainVariables.java:292-297,
360-395`): the first lookup of a dotted name records found, not found or the
expansion error, and later lookups of that name on the same instance reuse
it, also across `get_memory_inefficient_command_line` calls. So a failed
`expand_if_available = "s.x"` check makes a later `%{s.x}` report the generic
`Cannot find variable named 's.x'.` instead of `structure s doesn't have a
field named 'x'`.

`Variables` equality and hashing follow `MapVariables.equals` /
`SingleVariables.equals` (`J/CcToolchainVariables.java:993-1011, 1044-1062`):
the same class, the same parent *instance*, equal keys and values. They are
hashable (`isImmutable()`), and `cc_common.empty_variables()` is the
`CcToolchainVariables.empty()` singleton (`:273-290`), so two
`combine_cc_toolchain_variables(cc_common.empty_variables(), v)` results are
equal.

Expansion ports `FlagSet`, `FlagGroup.canBeExpanded`/`expand` and `EnvEntry`
(`J/CcToolchainFeatures.java:112-393`): flag sets apply when the action matches
and `with_features` is satisfied; `expand_if_available`,
`expand_if_not_available`, `expand_if_true`, `expand_if_false` and
`expand_if_equal` gate a group; `iterate_over` binds each element. A command
line expands the enabled action config's flag sets first, then every enabled
feature in order (`J/CcToolchainFeatures.java:911-936`); the environment visits
enabled features' env sets (`:976-984`). The tool of an action is the first
tool whose `with_features` match, among all action configs (enabled or not)
(`:986-1002`, `:694-703`).

## Native divergences

- **Creation locations.** Provider instances record their creation call
  site; its line and column match Bazel, but the file is the native path of
  the defining repository (for a `local_path_override` the override
  directory, e.g. `<workspace>/vendor/rules_cc/cc/cc_toolchain_config_lib.bzl`)
  where Bazel prints `<output_base>/external/rules_cc+/cc/...`. Plain
  `struct()` values (starlark-rust structs) carry no location, so `infoError`
  prints `instantiated at <unknown>` for them where Bazel prints the
  `struct(` call site.
- **Bazel internal crashes become errors.** Where Bazel 9.3 throws an unchecked
  Java exception instead of a Starlark error, the native profile fails the
  evaluation with an explicit message ending in `(Bazel 9.3 fails with an
  internal <Exception>)`: an unterminated `%{name`
  (StringIndexOutOfBoundsException), variable values of other types such as
  int (ClassCastException), a None element of an iterated sequence
  (NullPointerException), `enabled = None`, a non-string `type_name`, missing
  `_*_DO_NOT_USE` fields or an unknown `_exec_os_DO_NOT_USE`, duplicate
  environment keys (`Multiple entries with same key: K=2 and K=1`,
  IllegalArgumentException, raised like Bazel's `buildOrThrow` only after
  every entry expanded, so an expansion error is reported first), overlapping `combine_cc_toolchain_variables`
  inputs (`Cannot overwrite existing variables: [a]`) and an unconfigured or
  unmatched action in `get_execution_requirements` /
  `get_tool_requirement_for_action` (IllegalArgumentException; the same case
  in `get_tool_for_action` is a normal Starlark error in Bazel too).
- **Windows exec platforms.** `_exec_os_DO_NOT_USE = "WINDOWS"` (Windows path
  policy) fails explicitly. `exec_os(ctx)` reports the host OS, which is the
  native profile's only execution platform (docs/TOOLCHAINS.md).
- **Cache bound.** Bazel's Caffeine cache evicts beyond 10000 entries; natively
  further configurations are computed but not cached.
- **Equality and hashing.** Feature configurations compare by identity as in
  Bazel. `Variables` values compare Starlark values with Starlark equality
  where Bazel uses Java `equals` (the same for strings, booleans, lists,
  tuples, dicts, structs and Labels; depsets compare by identity in both), and
  their hash covers only the class and the keys.
- **Lookup cache of found values.** A cached found dotted name is
  re-resolved from the immutable variables instead of reusing Bazel's cached
  value object; this differs only if a referenced list is mutated between
  expansions.
- Path mapping and tree-artifact expansion of structure fields are not
  modelled (Bazel uses `PathMapper.NOOP` and no input metadata here).

## Out of scope

`cc_common.compile`, `link`, `create_linking_context_from_compilation_outputs`,
`cc_internal.solib_symlink_action`, `get_artifact_name_for_category` /
`get_artifact_name_extension_for_category` (artifact name patterns are parsed
and validated only), `cc_toolchain`/`cc_toolchain_config` rule analysis end to
end, rule `ctx.fragments` and `xcode_config` remain unsupported and fail
explicitly.

## Evidence

- Unit tests: `src/cc_variables.rs` (parser and `PathFragment` normalization)
  and `src/cc_features/tests.rs` (parser errors, selection, expansion matrix,
  variables, tools, allowlists) with hand-built config structs.
- CLI: `tests/cc_features.rs` builds `tools/build_defs/cc/probe.bzl` scenarios
  of `tests/fixtures/cc_features` against a vendored, byte-identical subset of
  the genuine rules_cc 0.2.17 Starlark files (`PROVENANCE.md`): selection
  (implies chains, requires-any-of, provides, enabled defaults, cache order),
  the expansion matrix for every `expand_if_*` gate, `iterate_over` over lists,
  tuples, depsets, dict keys and structures, `%%`, `%{a.b}`, `%{path:}`,
  Booleans, Labels and Files, `with_features` on flag sets, env sets and tools,
  absolute, relative and File tools, legacy features on/off
  (`no_legacy_features`) through rules_cc's own `configure_features`, and one
  failing target per error path, including rules_cc's `unconditionally
  implies` check and the private-API allowlist. Outputs are `json.encode`
  documents compared exactly.
- `src/cc_features/tests.rs` also compares the native selection worklist with
  a literal port of Bazel's recursive `FeatureSelection` over every
  implies/requires/requested combination of up to three features (bounded as
  documented on the test) and 20000 pseudo-random four-feature graphs.
- Paired genuine Bazel 9.3 capture: `tests/fixtures/cc_features_reference_receipt.json`
  (harness `artifacts/n25-capture.py`). The trimmed `tests/fixtures/cc_features`
  copy is a native-only fixture: genuine Bazel's `bazel_tools` needs the
  complete modules (rules_cc's genuine MODULE.bazel, platforms' host
  constraints, bazel_features, package_metadata), so the capture runs the same
  probe files against the full genuine cached modules on both engines; the
  copied `.bzl` bytes are identical.
