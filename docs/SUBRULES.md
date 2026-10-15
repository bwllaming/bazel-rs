# Subrule execution profile

This bounded profile executes real subrule callbacks and actions; it is not a
constructor-only placeholder or full subrule/configuration parity. The I0
zero-attribute profile below is extended by N24 (subrule attributes,
toolchains, fragments and late-bound defaults).

## Intended supported surface

The keyword-only constructor accepts a Starlark-defined implementation function
and `attrs`, `toolchains`, `fragments` and directly declared `subrules`.
Factory closures are valid; native callables such as `len` are not. Definitions
retain their frozen implementations and dependency values, exported identity and
aliasing. Subrule values are unhashable.

`rule(subrules=[...])` declares direct call permissions. A rule declaring Outer
does not thereby gain permission to call Outer’s Leaf. Each invocation injects a
fresh subrule context before user arguments and returns the actual callback
result. Generated Files and actions belong to the consuming target.

The constructor belongs in ordinary and MODULE-loaded `.bzl` files, not top-level
BUILD or MODULE globals. Aspects (their lifted attributes and toolchain types
are retained, never applied), inheritance and run/test APIs remain unsupported
rather than accepted and ignored. The four subrule context property names are
`actions`, `fragments`, `label` and `toolchains`.

## N24: attributes, toolchains, fragments, late-bound defaults

Reference: pinned Bazel 9.3 (`/tmp/opencode/bazel-9.3-reference`, e89755ba).
Paths below are relative to `src/main/java/com/google/devtools/build/lib/`.
Status: a paired genuine capture exists (`artifacts/n24-paired-capture.json`,
`tests/fixtures/subrules_full/reference_receipt.json`); the statements below
are what it observed, and the remaining differences are listed at the end of
this section. This is a bounded profile, not a parity claim.

**Constructor** (`analysis/starlark/StarlarkRuleClassFunctions.java:2160-2212`).
Parameter types first, in order (`starlarkbuildapi/StarlarkRuleFunctionsApi.java:1089-1148`):
`in call to subrule(), parameter 'attrs' got value of type 'list', want 'dict'`,
`... parameter 'toolchains'|'fragments'|'subrules' ... want 'sequence'`,
`... parameter 'implementation' ... want 'function'`. Then, in Bazel's order:
`attrs` must be `dict<string, Attribute>`; `fragments` a string sequence
(duplicates collapse, unknown names accepted); per attribute in dict order:
`bad cfg for attribute 'X': subrules may only have target/exec attributes.`
only for Starlark `transition()` objects — `config.none()` is a tool
transition (`analysis/config/transitions/NoConfigTransition.java:96-99`) and is
accepted, so `{'dep': attr.label(cfg=config.none())}` reports the next check —
`attribute name `X` is not a valid identifier.`, `illegal attribute name 'X':
subrules may only define private attributes (whose names begin with '_').`,
`illegal default value for attribute 'X': subrules cannot define computed
defaults.`, `for attribute 'X': no default value specified` and `bad type for
attribute 'X': subrule attributes may only be label or lists of labels.`; then
toolchain types and `subrules may require at most 1 toolchain, got:
[ToolchainTypeRequirement[...]]`. `mandatory=True` is accepted; every target of
a consuming rule then fails with `//pkg:t: missing value for mandatory
attribute '$//pkg:defs.bzl%S%_dep' in 'r' rule`
(`packages/AttributeProvider.java:319-327`). `attr.label_list(default=configuration_field(...))`
(or `default=None`) is `in call to label_list(), parameter 'default' got value
of type 'LateBoundDefault', want 'sequence or function'`
(`starlarkbuildapi/StarlarkAttrModuleApi.java:621-626`).

**Lifting** (`analysis/starlark/StarlarkSubrule.java:236-303, 553-560`;
`StarlarkRuleClassFunctions.java:1940-1964`): the transitive subrules of
`rule(subrules=...)` contribute their attributes after the rule's own, named
`$//pkg:defs.bzl%S%_dep` (`:` for late-bound). They are absent from
`ctx.attr`/`ctx.files`/`ctx.file`
(`analysis/starlark/StarlarkAttributesCollection.java:247-252`) but are
ordinary dependencies. Attribute errors of every private Starlark attribute
are masked as `in (an implicit dependency) attribute of r rule //:t: ...`
(late-bound: `in :x attribute ...`; `analysis/EventHandlingErrorReporter.java:84-86`),
e.g. `rule '//:nope' does not exist`, `source file '//:data.txt' is misplaced
here (expected no files)` and `'//:two' must produce a single file`
(`analysis/RuleContext.java:1966-1974`). Present `cfg='exec'`/`config.none()`
values and executable values fail explicitly (no transitions in this tree).
Aspects append lifted attributes after their own (`:1355-1365`).

**Invocation** (`StarlarkSubrule.java:135-199`): an explicit keyword naming an
attribute fails with `got invalid named argument: '_dep' is an implicit
dependency and cannot be overridden`; attributes are passed by keyword as
`File` (`allow_single_file`), `Target`, or `None`. A lifted `attr.label_list`
(even empty) CRASHES Bazel 9.3: `getPrerequisites` returns a Java list
(`:171-172`) that `Dict.immutableCopyOf` rejects (`:191-192`), exit 37
`InvalidStarlarkValueException: invalid Starlark value: class
com.google.common.collect.Lists$TransformingRandomAccessList` (captures
A2/lifted/check, B2b/*, B3/*). Native fails explicitly instead: `subrule S
attribute '_deps' (attr.label_list) is not supported by the native profile:
Bazel 9.3 crashes passing a lifted label_list to the implementation
(InvalidStarlarkValueException: Lists$TransformingRandomAccessList)`.

**Toolchains**: subrule types are requested by the consuming rule before its
own (`StarlarkRuleClassFunctions.java:1024-1027, 1064`; LinkedHashSet in
`packages/RuleClass.java:743, 1587-1590`; captured `No matching toolchains
found for types:\n  //:b\n  //:a`), so the rule's `ctx.toolchains` sees them.
`subrule_ctx.toolchains` without automatic exec groups is `subrules using
toolchains must enable automatic exec-groups. For more info, see
https://bazel.build/extending/auto-exec-groups#migration-aegs`
(`StarlarkSubrule.java:377-397`; captured with and without toolchain types).
Rules with `_use_auto_exec_groups = True` or `--incompatible_auto_exec_groups`
remain explicit engine gaps.

**Fragments** (`StarlarkSubrule.java:562-597`): the collection's fields are the
DECLARED fragments: `dir(ctx.fragments)` lists them sorted, and `hasattr`/dot/
`getattr` of an undeclared Starlark fragment fail with `S has to declare 'F' as
a required fragment in order to access it. Please update the 'fragments'
argument of the subrule definition (for example: fragments = ["F"])` (the
`.bzl` `hasattr`/`dir` builtins apply Structure semantics to this one type).
Starlark fragments are the configuration's (`analysis/config/BuildConfigurationValue.java:360-370`):
android, apple, bazel_android, coverage, cpp, j2objc, java, objc, platform,
proto — `py`/`bazel_py` are dropped by `Fragment.shouldInclude`
(`rules/python/PythonConfiguration.java:84-87`). Declared `cpp` is a `cpp`
value with all 49 `CppConfiguration` members (`rules/cpp/CppConfiguration.java`,
`starlarkbuildapi/cpp/CppConfigurationApi.java`): public fields (`copts`,
`cxxopts`, `conlyopts`, `objccopts`, `linkopts`, `lto_backend_options` = `[]`
immutable, `custom_malloc` = None, ...), `minimum_os_version()` None,
`dynamic_mode()` `"DEFAULT"`, and private API members guarded by
`BuiltinRestriction.INTERNAL_STARLARK_API_ALLOWLIST`
(`packages/BuiltinRestriction.java:36-104`, `file '//pkg:x.bzl' cannot use
private API`; `force_pic`/`generate_llvm_lcov`/`fdo_instrument` append ` (feature
'X' in CppConfiguration)`, `CppConfiguration.java:899-906`). Values are Bazel's
option defaults except `compilation_mode()`, `force_pic()` and
`should_strip_binaries()`, which come from the configuration the target is
analyzed in (`CustomInvocation::cpp`, set by the engine from
`configuration.native`). Other declared fragments fail explicitly.
Divergence: an unknown name (`py`, `nope`) is the interpreter's missing-attribute
error, not `There is no configuration fragment named 'nope'. Available
fragments: 'cpp'`.

**Late-bound defaults** (`analysis/starlark/StarlarkLateBoundDefault.java:57-66`,
`src/late_bound.rs`): the configured target takes the field's value: `None` for
every `cpp` field, `coverage.output_generator`, `java` launcher/proguard_top/
bytecode_optimizer/java_toolchain_bytecode_optimizer/local_java_optimization_configuration,
`py.native_rules_allowlist` and `bazel_py.python_top` (all captured `None`).
`apple.xcode_config_label` resolves to the `--xcode_version_config` default
`@bazel_tools//tools/cpp:host_xcodes` (`rules/apple/AppleCommandLineOptions.java:293-296`),
an alias, so Bazel's `ctx.attr._x.label` is its actual
`@@bazel_tools+xcode_configure_extension+local_config_xcode//:host_xcodes`
(captured; static query deps list `@bazel_tools//tools/objc:host_xcodes`, not
modelled). Natively the alias edge is analyzed like any alias; the plain
profile has no `@bazel_tools`, so it fails with the repository error followed
by `//pkg:t: the late-bound configuration_field default resolves to
@@bazel_tools//tools/cpp:host_xcodes, which the native profile cannot analyze
here (@bazel_tools is provided only with --experimental-builtin-modules)`; the
builtin-modules profile needs downloads (rules_cc, apple_support) and was not
runnable offline. Late-bound edges skip visibility
(`analysis/CommonPrerequisiteValidator.java:156-159`). Other fields (e.g.
`proto.proto_compiler`) stay explicit gaps naming `configuration_field
fragment.name`. Implicit-dependency visibility relative to the rule's `.bzl`
(`CommonPrerequisiteValidator.java:175-221`) is not modelled.

**Remaining captured differences** (see the receipt for exact strings):
struct `repr` (Bazel sorts fields and prints ` = `; N22 repr work on a separate
branch, pre-N22 here), unknown-fragment message, interpreter argument-binding
text, rule `ctx.fragments`/`ctx.executable` (absent natively, outside N24), and
options/queries the native profile rejects explicitly (`--output=build`,
`--toolchain_resolution_debug`, `--fdo_optimize`, `--collect_code_coverage`,
`deps()`).

## Context and receiver lifetimes

Existing-property reads on a locked parent context fail, including
`getattr(parent, "label", None)`. Structural `hasattr` and `dir` do not invoke
getters; a missing-property default still works. Child contexts expire after
return and do not become valid again during another invocation of the same
subrule. Ordinary-error restoration must preserve the caller’s scope.

Action registration/declaration methods require the current invocation’s
receiver, including captured objects and bound methods. `actions.args()` is an
evidence-backed exception: a captured, still-live ancestor factory remains usable
inside a child. Already-created Args retain the root analysis lifetime and may be
returned to, extended by and consumed by the parent.

Expired child Args factories crash genuine Bazel 9.3 with a null-context exception.
Native controlled rejection is an explicit
**KNOWN_REFERENCE_DEFECT — INTENTIONAL_DIVERGENCE**, not differential PASS or an
attempt to reproduce that crash. No new Rust-panic recovery or frozen runtime
context capability is claimed.

## Evidence and deferred work

`ctx.actions.write` accepts a string or a concrete, active Args builder. Strings
retain their original bytes; Args use the verified default shell-quoted format,
with one LF after each argument. Empty Args writes zero bytes, and an empty
argument writes `''` followed by LF. Quoting preserves embedded newlines and
UTF-8. Receiver validation precedes serialization and action registration.

This does not enable `use_param_file` or `set_param_file_format`; both remain
explicitly unsupported. Existing native Args mappers still expand eagerly,
unlike Bazel's deferred write/action expansion. The write-format evidence in
`artifacts/actions-write-args-contract.json` does not establish mapper-phase parity.

I0 reference facts and executable fixtures are recorded in
`artifacts/subrule-contract.json`, `artifacts/subrule-scope-contract.json` and
`artifacts/subrule-args-factory-contract.json`. Post-error restoration expectations
derived from Java source remain labeled as such; native host tests must inspect
actual restored state. The original fragment-reading fixture is preserved and
not presented as a supported native case; a separately named no-configuration-
read fixture establishes the intended functional path.

Lifted private dependency attributes are implemented by N24 (above); their
`cfg='exec'` values still need the exec transition. The observed exec
label-list reference crashes are not evidence of native support. Explicit
experimental command-line flag control is not implemented; its rejection is not
Bazel’s explicit-false allowlist behavior.

Full Bazel compatibility, application build/test success and performance goals
remain unfinished.

## Verification

N24 plan: **64 probes, 123 phases, 198 commands** (reference-only: 59 phases,
115 commands): 14 differential `configuration` probes and the native-only
`boundaries/lifted-label-list` (Bazel crashes). The family-scoped genuine run
is recorded in `tests/fixtures/subrules_full/reference_receipt.json`. The I0
history below is unchanged.

I0 (historical): the combined gate passed **1,057 Rust and 609 Python tests**, including 27 real
CLI groups and 37 portable harness tests. The frozen comparison records **54
probes, 99 phases and 162 commands**: 95 passing phases, two known reference
crashes and two intentional native controlled-error divergences. Its status is
`PASS_ONLY_BOUNDED_PROFILE`, not unrestricted parity. Thirteen adjacent suites
pass on the same candidate.

Constructor metadata, bounded argument forwarding, parent restoration, actual
File/provider/action outputs, cache/source changes, context guards and method-
specific Args behavior are covered. Test fixtures are tracked; compilation and
unit verification do not depend on ignored research reports. Historical failures
and the original configuration-reading reference fixture remain preserved.

Analyzer diagnostics retain exactly the twelve known pristine-dependency errors,
with zero new or root-source errors. The preserved application query advances to
missing `json` in `rules_go` SDK loading; checkout contents and the required Git
symlink remain unchanged. All I0-owned servers, processes and scratch are cleaned.
See `artifacts/subrules-validation.json`,
`artifacts/i0-subrule-verification-manifest.json` and
`artifacts/mainrepo-subrules-loading.json` for the bound evidence.
