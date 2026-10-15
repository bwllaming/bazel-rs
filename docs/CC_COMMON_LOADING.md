# Language-module globals for `.bzl` loading (cc_common and friends)

Bazel 9.3 (`e89755babf3e941bc1e9d8120b1cd48854ab9dfa`) exposes several
language-module globals to every `.bzl` file. Rule sets use them at load time:
rules_cc `cc/private/cc_internal.bzl:17`, `cc_info.bzl:134`,
`rules_impl/native_cc_common.bzl` and `cc_common.bzl:735`, rules_go
`go/platform/apple.bzl`, rules_python `common.bzl` and
`py_internal_renamed.bzl`, rules_java `java/private/native.bzl` and protobuf
`bazel/private/native.bzl`.

The native profile reproduces what can be observed while a `.bzl` file
initializes: names, types, `dir()`, `hasattr()`, `repr()`, and the pure values.
It also implements the load-time constructors. Anything that needs configured
targets, toolchains, actions or C++/Java/Apple/Python analysis exists, but
fails explicitly when called:

```
<owner>.<member> is not implemented in the native profile (analysis-time C++ API)
```

The domain in parentheses names the API family (C++, Java, Apple, Python,
platform/toolchain, configuration, coverage or testing). **C++ analysis, C++
toolchains, `cc_common.compile`/`link`, feature configuration, and every other
analysis-time member are unsupported.** This milestone only lets `.bzl`
loading get past these globals.

All the globals below are installed in both BUILD-loaded and MODULE-loaded
`.bzl` environments. Bazel's `BazelStarlarkEnvironment.createBzlEnvUsingInjection`
injects `exports.bzl` toplevels into both. They are not added to BUILD files.

## Injected `@_builtins` structs

Bazel builds these in Starlark (`builtins_bzl/bazel/exports.bzl` and
`builtins_bzl/common/exports.bzl`), so their type is `struct`. Bazel's struct
(`StarlarkInfo`) sorts its fields, and both `repr` and `dir` list them in sorted
order. The native structs are built with sorted field order to match. Native
struct `repr` spacing (`struct(a=1)`) is a pre-existing difference from Bazel
(`struct(a = 1)`) and is not changed here.

| Global | Source | Fields |
| --- | --- | --- |
| `cc_common` | `common/cc/cc_common_bazel.bzl:69-84` | 14: `internal_DO_NOT_USE`, `do_not_use_tools_cpp_compiler_present` (None, `CcModuleApi.compilerFlagExists`), and 12 analysis-time functions |
| `java_common` | `common/java/java_common.bzl` | `internal_DO_NOT_USE` |
| `apple_common` | `common/objc/apple_common.bzl:37-50` (`apple_common_bazel`) | 10 (see below) |
| `py_internal` | `common/python/py_internal.bzl` | 26: 24 functions, `cc_helper`, `PackageSpecificationInfo` |
| `proto_common_do_not_use` | `common/exports.bzl` | `INCOMPATIBLE_ENABLE_PROTO_TOOLCHAIN_RESOLUTION = True` (flag default) |

Struct members that Bazel defines with `def` have type `function`, and their
`repr` is `<function NAME from @@_builtins//:FILE>`. Native function values
compare equal by definition, so re-allocated copies, such as those in
`apple_toolchain()`, behave like Bazel's single objects.

### `cc_common.internal_DO_NOT_USE()` and the private-API check

`_internal_exports` calls `cc_internal.check_private_api` with the allowlist
`[("", "third_party/bazel_rules/rules_cc"), ("rules_cc", "")]` and `depth=1`.
The native implementation follows `BuiltinRestriction.AllowlistEntry.allows`:

- A non-main repository matches `("rules_cc", …)` when its canonical name
  starts with `rules_cc+`. For example, `rules_cc+` and `rules_cc++ext+repo`
  match, while `my_rules_cc+` and a bare `rules_cc` do not.
- For the main repository, an empty apparent name always matches. Otherwise the
  main repository mapping must map the name to the main repository.
- `bazel_tools` matches only the `bazel_tools` entry.
- The package prefix is compared by path segment, and an empty prefix matches
  every package.
- A rejected call fails with Bazel's exact message:
  `file '@@other+//pkg:x.bzl' cannot use private API`.

Bazel inspects the module of the Starlark function one frame below the builtins
wrapper. In the native profile there is no wrapper frame, so the check uses the
immediate caller's location. That location belongs to the innermost Starlark
function or to the top level, which counts as a function frame in Bazel. A
helper defined in `rules_cc+` therefore succeeds even when a file elsewhere
calls it, as in Bazel. If no module is found on the stack (Bazel's `null`
module), the call is permitted, as in `CcStarlarkInternal.checkPrivateApi`.
`java_common.internal_DO_NOT_USE()` follows the same rules with the rules_java
allowlist. It returns the 11-field struct of `java_common` builtin methods.

### `cc_internal`

The returned value has type `cc_internal` and Bazel's default `repr`
`<unknown object com.google.devtools.build.lib.rules.cpp.CcStarlarkInternal>`.
It exposes all 39 `@StarlarkMethod` names of `CcStarlarkInternal.java`.
`declare_shareable_directory` is not among them because it belongs to the
nested action factory. The members have type `builtin_function_or_method`.

- `check_private_api(allowlist=[(repo, prefix), ...], depth=1)` is
  implemented. `depth` counts Starlark frames below the caller. Native frames
  between Starlark frames are not skipped, which is a documented edge.
- `freeze(value)` (positional-only) is implemented (CcStarlarkInternal.java:949-960):
  a dict yields a dict copy, a list/tuple/range/set yields a list copy, anything
  else is returned unchanged. Bazel's copies are immutable; native copies become
  immutable when their module freezes (rules_cc's top-level
  `EMPTY_COMPILATION_OUTPUTS`), so mutating a `freeze()` result during analysis
  is a known divergence.
- `create_header_info()` is implemented for the artifact-free form: all
  defaults, `None`, or empty sequences. It returns an immutable, hashable
  `HeaderInfo` with Bazel's unannotated type name and identity equality, as
  with Bazel's identity token. Like other immutable values, it freezes into
  module globals and provider fields. A call with header artifacts fails
  explicitly.
- `HeaderInfo` exposes the 8 `structField` getters of
  `CcCompilationContext.HeaderInfo`. `dir()` lists them sorted:
  `header_module`, `modular_private_headers`, `modular_public_headers`,
  `pic_header_module`, `separate_module`, `separate_module_headers`,
  `separate_pic_module`, `textual_headers`. For the empty form, the four
  artifact getters (`allowReturnNones`) return `None`. The four list getters
  return an empty immutable list (`StarlarkList.immutableCopyOf`); natively
  this is the frozen empty list, so mutation fails.
- Every other member fails explicitly.

### `apple_common`

The `platform_type` and `platform` tables are real values from
`apple_platform.bzl`. For example, `platform.ios_device` is
`struct(is_device=True, name="ios_device", name_in_plist="iPhoneOS",
platform_type="ios")`.

`apple_toolchain()` returns `struct(developer_dir, platform_developer_framework_dir, sdk_dir)`,
taken from `apple_toolchain.bzl`. Its functions are implemented:

- `developer_dir()` returns `__BAZEL_XCODE_DEVELOPER_DIR__`.
- `sdk_dir()` returns `__BAZEL_XCODE_SDKROOT__`.
- `platform_developer_framework_dir(fragment)` reads
  `fragment.single_arch_platform.name_in_plist`.

The remaining members are as follows:

- `XcodeProperties` is `None`.
- `XcodeVersionConfig` is the `XcodeVersionInfo` provider, and `Objc` and
  `new_objc_provider` are `ObjcInfo`. These are `@_builtins` Starlark
  providers with `repr` `<provider>`, so calling them fails explicitly.
- `apple_host_system_env`, `target_apple_env` and `dotted_version` fail
  explicitly.

## Java builtin namespaces

| Global | Type | `dir()` |
| --- | --- | --- |
| `platform_common` | `platform_common` | `ConstraintSettingInfo`, `ConstraintValueInfo`, `PlatformInfo`, `TemplateVariableInfo`, `ToolchainInfo` |
| `config_common` | `config_common` | `FeatureFlagInfo`, `config_feature_flag_transition`, `toolchain_type` |
| `coverage_common` | `coverage_common` | `instrumented_files_info` |
| `testing` | `testing` | `ExecutionInfo`, `TestEnvironment`, `analysis_test` |

`repr` follows Bazel: the default `<unknown object CLASS>`, except
`<coverage_common>`. The provider members have repr `<function NAME>`
(`BuiltinProvider`) and type `Provider`. The exception is
`config_common.FeatureFlagInfo`: its class `ConfigFeatureFlagProvider.Constructor`
carries its own `@StarlarkBuiltin(name = "FeatureFlagInfo")`, so its type is
`FeatureFlagInfo`. `ToolchainInfo`, `PlatformInfo`, `ConstraintSettingInfo`,
`ConstraintValueInfo` and `ExecutionInfo` are unannotated `BuiltinProvider`
subclasses. `XcodeVersionInfo` and `ObjcInfo` are Starlark providers. All of
these report `Provider`. They compare and hash by key and are
valid `attr.label(providers=[...])` requirements under `builtin:NAME`.
`@_builtins` Starlark providers use `@@_builtins//:FILE%NAME`. Calling them
fails explicitly. `TemplateVariableInfo` remains the existing native
implementation.

- `config_common.toolchain_type(name, *, mandatory=True)` is implemented
  (`ConfigStarlarkCommon.toolchainType`). A string name is resolved with the
  evaluating `.bzl` file's repository mapping. The result has type
  `toolchain_type`, fields `toolchain_type` (a Label) and `mandatory`, and
  value equality. The positional-only and parse errors use Bazel's text. A
  value that is neither a string nor a Label fails the `@Param` type check
  first: `in call to toolchain_type(), parameter 'name' got value of type
  'int', want 'string or Label'`.
- `testing.TestEnvironment(environment, inherited_environment=[])` returns a
  `RunEnvironmentInfo`, as `StarlarkTestingModule` does. Type errors come from
  the RunEnvironmentInfo constructor. Like Bazel, the instance carries
  `shouldErrorOnNonExecutableRule=false`.
  - A rule that is neither executable nor a test can return such an instance;
    the provider is kept.
  - For instances from the public `RunEnvironmentInfo(...)` constructor, that
    is still the error `Returning RunEnvironmentInfo from a non-executable,
    non-test target has no effect`.
  - Bazel also prints the same text as a rule warning for the TestEnvironment
    case. The native analysis has no warning channel, so it prints nothing.
  - The flag does not take part in equality, which matches `NativeInfo`.

## Top-level builtins

- `exec_group(*, toolchains=[], exec_compatible_with=[])` is implemented
  (`StarlarkRuleClassFunctions.execGroup` / `parseToolchainTypes`). Toolchains
  may be `toolchain_type` values, Labels or strings. Duplicates keep their
  first position and the strictest requirement.
  - Any other toolchain element fails with `'toolchains' takes a
    toolchain_type, Label, or String, but instead got a %s`. Bazel fills `%s`
    with the Java `getClass().getSimpleName()`, so native maps the value to
    the class Bazel would hold: `Int32`/`Int64`/`Big`, `NoneType`, `Boolean`,
    `String`, `StarlarkFloat`, `MutableStarlarkList`, `SingletonTuple`/`RegularTuple`,
    `Dict`, `StarlarkSet`, `RangeList`, `Depset`, `StarlarkInfoNoSchema` for
    `struct`, `StarlarkFunction`, and `BuiltinFunction`. Other values fall back
    to their Starlark type name.
  - `exec_compatible_with` accepts strings only, as `parseLabels` uses
    `Sequence.cast(..., String.class, "exec_compatible_with")`. Every element
    is type-checked before any is parsed: `at index N of exec_compatible_with,
    got element of type T, want string`, including for Label values.
  - A string that fails to parse reports `Unable to parse label '%s' in
    attribute 'exec_compatible_with': %s`. The detail after the colon is the
    native label parser's text.
  - The result has type `exec_group` and value equality.
- `configuration_field(fragment, name)` is implemented
  (`BazelBuildApiGlobals.configurationField`). It is valid only during `.bzl`
  initialization; otherwise it fails with `configuration_field() can only be
  used during .bzl initialization (top-level evaluation)`. The fragment must
  be one of Bazel 9.3's named registered fragments (`android`, `apple`,
  `bazel_android`, `bazel_py`, `coverage`, `cpp`, `j2objc`, `java`, `objc`,
  `platform`, `proto`, `py`), and the name must be one of that fragment's
  `@StarlarkConfigurationField` methods. Otherwise it fails with
  `invalid configuration fragment name '…'` or `invalid configuration field
  name '…' on fragment '…'`. The result has type `LateBoundDefault` and
  `repr` `<late-bound default>`. Using it as an `attr.label` default fails
  explicitly, because late-bound defaults are an analysis-time configuration
  API.
- `CcToolchainConfigInfo` is `None`. This is the flag-guarded
  `Starlark.NONE` registered by `CcBootstrap`, which `exports.bzl` does not
  override.

## Compile-time guarded names

Bazel's resolver (`Module.resolve`) rejects a *free* reference to a disabled
`GuardedValue` while compiling a `.bzl` file. This happens before its loads
run and whether or not the referencing code executes. starlark-rust globals do
not depend on the file, so `src/guarded_globals.rs` resolves these names for
each file right after parsing:

- `CcInfo`, `DebugPackageInfo`, `CcSharedLibraryInfo` and
  `CcSharedLibraryHintInfo` are `CcRules` / `BaseRuleClasses.createEmptySymbol`
  names. They fail with Bazel's text:
  `The CcInfo symbol has been removed, add the following to your BUILD/bzl file:`
  followed by the `load("@rules_cc//cc/common:cc_info.bzl", "CcInfo")` line.
- `py_internal` (`ContextGuardedValue.onlyInAllowedRepos`) is allowed when
  the canonical repository name contains `_builtins`, `bazel_tools` or
  `rules_python` with a Java-regex word boundary on both sides, or in the main
  repository under `tools/build_defs/python`. Elsewhere it fails with
  `py_internal may only be used from one of the following repositories or
  prefixes: @@_builtins//, @@bazel_tools//, @@rules_python//, tools/build_defs/python`.

The analysis is conservative. A name bound by `load`, a top-level assignment
or `def`, a parameter, an assignment anywhere in the enclosing function, a
lambda or a comprehension is never reported. For the removed symbols, a missed
case still fails in starlark-rust's compiler with `Variable … not found`.

`cc_common` and `apple_common` are flag-guarded by
`--incompatible_stop_exporting_language_modules`. That flag defaults to false,
so both are accessible everywhere, as in Bazel's default configuration.

## Bazel version and the rules_cc compatibility proxy

`native.bazel_version` is `9.3.0` (`COMPATIBILITY_VERSION`) in MODULE-loaded
`.bzl` files and in repository-rule callbacks. bazel_features' `version_repo`
records it, and rules_cc's `_compatibility_proxy_repo_impl` compares it with
`_bazel_version_ge("9.0.0-pre.20250911")`, which selects the branch where
`symbols.bzl` loads rules_cc's own Starlark `cc/private/cc_common.bzl`. That
file needs exactly the `cc_common.internal_DO_NOT_USE()` path above. A unit
test reruns the comparison with bazel_features' parse logic.

## Paired capture against Bazel 9.3.0

`tests/fixtures/cc_common_loading/reference_receipt.json` records a paired
genuine Bazel 9.3.0 versus native capture. The workspaces are byte-identical
and mirror every `tests/cc_common_loading.rs` step, plus value and error
probes. Two diagnostic differences are pre-existing and deliberate:

- **Query exit code.** Each rejected mirror `query` (`mirror/private_api_cc`,
  `mirror/private_api_java`, `mirror/removed_ccinfo`,
  `mirror/py_internal_guard`) exits **7** in Bazel and **1** natively. The
  receipt marks these `exit_code_divergence`, and the CLI test asserts
  native 1. The rejecting `build` probes exit 1 in both.
  Bazel's message text appears verbatim in native stderr (`EXACT_MESSAGE`).
- **Traceback columns.** Native diagnostics use starlark-rust's
  `bazel-rs:`/`-->` traceback framing rather than Bazel's
  `ERROR:`/`File "...", line L, column C` frames. For failures raised inside a
  call, native reports the start of the call expression and Bazel the call's
  opening parenthesis. Examples are `4:15` versus Bazel `4:44` for
  `cc_common.internal_DO_NOT_USE()`, and `1:5` versus `1:24` for
  `configuration_field(...)`. Compile-time guarded names (removed `Cc*Info`
  symbols, `py_internal`) report the identical line and column.

Other recorded `KNOWN_DIVERGENCE` rows come from pre-existing native
formatting, each isolated by a control probe:

- struct `repr` spacing (`struct(a=1)`);
- Label `repr`;
- starlark-rust's generic missing-attribute text.

## Not covered (next loading blockers)

These globals are referenced by the same application closure and are **not**
provided by this milestone. starlark-rust rejects unknown names, even inside
function bodies, when it compiles a file:

- `aspect`, `transition`, `select`, `OutputGroupInfo` and `visibility` (the
  last is in progress separately).
- `config.exec`, `config.target` and `config.none`.
- Further `native.*` members, such as `alias`, `config_setting`,
  `cc_toolchain_suite` and `legacy_globals`, are needed only when called.
- The native `rule()`/`attr.*` profile rejects many load-time arguments that
  rules_cc uses, such as `executable`, `test`, `toolchains`, `exec_groups`,
  `fragments`, `cfg` transitions, list-valued `allow_files` and late-bound
  defaults. Loading the real rules_cc therefore stops at its first
  `rule()`/`attr` call, even with every global above present.
