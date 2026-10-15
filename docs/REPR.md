# repr() and str() of builtin values (N22)

Bazel 9.3.0 (`/tmp/opencode/bazel-9.3-reference`, e89755b) is authoritative.
Paths are relative to `src/main/java/`; `lib/` abbreviates
`com/google/devtools/build/lib/`. Unless a row says otherwise, `str(x)` is
`repr(x)`: `StarlarkValue.str` defaults to `repr`
(`net/starlark/java/eval/StarlarkValue.java:61-63`), and a class without a
`repr` override prints `<unknown object CLASS>`
(`StarlarkValue.java:50-52`).

Formatting with `%s` and `"{}".format(x)` uses `str()`
(`net/starlark/java/eval/Printer.java:402-408`,
`net/starlark/java/eval/FormatParser.java:126`); `%r` uses `repr()`. The
vendored starlark-rust used `repr()` for `%s` and for the single-`{}` fast path;
both now use `str()` (vendor patches below). This matters for
`"%s" % ctx.label`, which must stay `@@//pkg:x`, and changes `"%s" % path`
for a module_ctx/repository_ctx `path` from its quoted repr to the plain path.
Bazel's `FormatParser` has no `!r`/`!s` conversions: `"{!r}".format(x)` fails
with `Missing argument '!r'` (the whole field text names a keyword argument),
as does any missing keyword (`FormatParser.java:110-116`).

`print()` and `fail()` format each argument with `Printer.debugPrint`
(`net/starlark/java/eval/MethodLibrary.java:895-913` fail, `:940-955` print;
`Printer.java:117-124`): a string prints itself, other values their
`StarlarkValue.debugPrint`, which defaults to `str()` (`StarlarkValue.java:77-79`).
Only top-level arguments use debugPrint; elements of lists etc. print their repr.

| Value | debugPrint (print/fail) | Bazel source |
|---|---|---|
| `Label` | shorthand display form relative to the main repository mapping: `//pkg:x`, `//pkg` for `//pkg:pkg`, `@dep//pkg:x` (apparent name), `@@name//...` when the main repository cannot see it | `lib/cmdline/Label.java:725-737`, `:511-539`; `RepositoryName.java:305-324` |
| rule target | `<target //pkg:g, keys:[P, OutputGroupInfo]>`: returned declared providers in return order (exported names), then the OutputGroupInfo every rule target carries | `RuleConfiguredTarget.java:288-303`; `RuleConfiguredTargetBuilder.java:158-172,211-213`; `StarlarkProvider.java:655-657` |
| `Args` | the expanded arguments joined by spaces | `lib/analysis/starlark/Args.java:72-83` |
| `Facts` (module_ctx.facts) | `Facts({})` | `lib/bazel/bzlmod/Facts.java:60,162-168` |
| anything else | `str()` | `StarlarkValue.java:77-79` |

## Analysis values

| Value | repr | str | Bazel source | Native |
|---|---|---|---|---|
| `Label` | `Label("//pkg:x")`, `Label("@@repo+//pkg:x")` (canonical form, Printer-quoted) | `@@//pkg:x`, `@@repo+//pkg:x` | `lib/cmdline/Label.java:718-723` (repr), `:740-754` (str), `:490-492`/`:499-501` (canonical/unambiguous forms), `lib/cmdline/RepositoryName.java:284-286`; `--incompatible_unambiguous_label_stringification` defaults to true (`lib/packages/semantics/BuildLanguageOptions.java:546-548`) | `src/starlark_types.rs` `LabelValue` (`Display` = repr, `collect_str` = str) |
| `File` (source) | `<source file pkg/x.txt>` (root-relative: external sources drop `external/<repo>/`) | = repr | `lib/actions/Artifact.java:1256-1262`, `:530-532`, `:773-775` | `src/starlark_types.rs` `File` |
| `File` (generated) | `<generated file pkg/out>`; external: `<generated file external/repo+/pkg/out>` | = repr | `Artifact.java:1256-1262`, `:307-309` | same |
| rule target | `<target //pkg:a>` | = repr | `lib/analysis/configuredtargets/RuleConfiguredTarget.java:284-286` (label `toString` = canonical form, `Label.java:478-481`) | `TargetKind::Rule` |
| source-file target | `<input file target //pkg:f.txt>` | = repr | `InputFileConfiguredTarget.java:100-102` | `TargetKind::InputFile` |
| output-file target | `<output file target //pkg:out>` | = repr | `OutputFileConfiguredTarget.java:149-151` | `TargetKind::OutputFile` |
| alias target | `<alias target //pkg:alias of //pkg:actual>` | = repr | `lib/rules/AliasConfiguredTarget.java:246-249` | `TargetKind::Alias` (requested label differs from the resolved one) |
| package group target | `<unknown target //pkg:g>` (not observable through `attr.label`: Bazel and native both reject a package group there, capture `B/g_pkggroup`) | = repr | `AbstractConfiguredTarget.java:243-245` | `TargetKind::Unknown` |
| `struct(...)` | `struct(a = 1, b = "x")`, fields sorted by code point (Bazel strings hold UTF-8 bytes as Latin-1 chars, so `Ordering.natural()` is byte order: `struct(Z = 5, _ = 6, z = 2, é = 1, � = 4, 😀 = 3)` in the capture, U+FFFD before U+1F600) | = repr | `lib/packages/StructImpl.java:139-153` | vendor `structs/value.rs` |
| `struct` itself | `<function struct>`, type `Provider` | = repr | `lib/packages/StructProvider.java:30-44`; `BuiltinProvider.java:94-97`; `starlarkbuildapi/core/StructApi.java:38-53` | `starlark_types::StructProvider` (positional arguments fail with `struct() got unexpected positional argument`, `BuiltinFunction.java:152-154`) |
| Starlark provider instance | `struct(a = ..., b = ...)` (present fields, sorted) | = repr | `StarlarkInfo` inherits `StructImpl.repr` | `src/providers.rs` `ProviderInstanceGen` |
| Starlark provider (exported or not) | `<provider>` | = repr | `lib/packages/StarlarkProvider.java:528-530` | `ProviderConstructorGen` |
| builtin provider (`DefaultInfo`, `OutputGroupInfo`, ...) | `<function DefaultInfo>`, type `Provider` | = repr | `lib/packages/BuiltinProvider.java:94-97`; `starlarkbuildapi/DefaultInfoApi.java:97-98` | `DefaultInfoConstructor` (others already) |
| `DefaultInfo(...)` | `struct(data_runfiles = None, default_runfiles = None, files = depset([...]) \| None, files_to_run = None)` | = repr | `lib/packages/NativeInfo.java:64-94,125-138`; `lib/starlarkbuildapi/DefaultInfoApi.java:52-96`; `lib/analysis/DefaultInfo.java:199-224` | `DefaultInfoValue` |
| `target[DefaultInfo]` | `struct(data_runfiles = <unknown object com.google.devtools.build.lib.analysis.Runfiles>, default_runfiles = <... Runfiles>, files = depset([...]), files_to_run = <unknown object CLASS>)`; CLASS is `com.google.devtools.build.lib.analysis.FilesToRunProvider` for (non-executable) rule targets and `...FilesToRunProvider$SingleExecutableFilesToRunProvider` for source/output file targets and one-file filegroups, whose file is their executable (`rules/filegroup/Filegroup.java:107,117-119`; an alias uses its actual target's) | = repr | `DefaultInfo.java:146-188` (DelegatingDefaultInfo); `FilesToRunProvider.java:39-55`; `configuredtargets/FileConfiguredTarget.java:98-101` | `DefaultInfoValue { files_to_run_class }` |
| `TemplateVariableInfo(...)` | `struct(variables = {"K": "V"})` | = repr | `NativeInfo.java:125-138`; `TemplateVariableInfoApi.java:45-52` | `src/template_variable_info.rs` |
| `depset` | `depset([e1, e2])`, `depset([...], order = "postorder")` (omitted for `default`) | = repr | `lib/collect/nestedset/Depset.java:319-328` | `Depset` |
| `ctx` (rule, build setting) | `<rule context for //pkg:x>` | = repr | `lib/analysis/starlark/StarlarkRuleContext.java:590-597`, `:197` | `src/analysis_ctx.rs` |
| subrule `ctx` | `<NAME context for //pkg:x>` | = repr | `StarlarkSubrule.java:493-501` | `SubruleCtx` |
| `ctx.attr` | `struct(...)` (a struct, like native); `attr.output`/`attr.output_list` values are their Labels (`o = Label("//pkg:p.o")`) | = repr | genuine capture `B/g_ctxcoll` (`tests/fixtures/repr/reference_receipt.json`) | `src/analysis.rs` |
| `ctx.outputs` | `ctx.outputs(o = <generated file pkg/p.o>)` (sorted fields), type `Outputs`; it may be returned inside providers/structs, and once the implementation has returned (frozen natively, the context nullified in Bazel) it prints `ctx.outputs(for //pkg:p)` and its fields are inaccessible | = repr | `StarlarkRuleContext.java:375-377,415-447,471,586-588` | `analysis_ctx::CtxOutputsGen` (frozen variant `immutable`) |
| `ctx.actions` | `actions for<rule context for //pkg:x>` / `actions for<NAME context for //pkg:x>` (no space) | = repr | `StarlarkActionFactory.java:1284-1287` | `src/analysis_runtime.rs` `Actions` |
| `ctx.actions.args()` | `context.args() object` | = repr | `lib/analysis/starlark/Args.java:67-70` | `src/args.rs` |
| `rule` | `<rule NAME>` / `<rule>` | = repr | `StarlarkRuleClassFunctions.java:1999-2005` | unchanged |
| symbolic macro | `<macro NAME>` / `<macro>` | = repr | `StarlarkRuleClassFunctions.java:1683-1689` | unexported now `<macro>` |
| subrule | `<subrule NAME>` / `<subrule unexported subrule>` | = repr | `StarlarkSubrule.java:101-112` | unchanged |

`Args.add(label)`, `add_all` and `add_joined` (including `format`/`format_each`)
expand a Label to `Label.getDisplayForm` with the main repository mapping
(`lib/analysis/starlark/StarlarkCustomCommandLine.java:967-978`), never `repr`:
`//pkg:x` in the main repository, `@dep//pkg:x` through an apparent name the main
repository has, `@@name//pkg:x` otherwise. `map_each` receives the Label value
itself.

## Builtin functions

`BuiltinFunction.repr` (`net/starlark/java/eval/BuiltinFunction.java:192-203`)
prints `<built-in method NAME of TYPE value>` when the receiver is a
`StarlarkValue` (or string) and `<built-in function NAME>` otherwise. Every
builtin function or method has type `builtin_function_or_method` and is not
hashable: `unhashable type: 'builtin_function_or_method'`, the text Bazel 9.3
uses for every unhashable value (`StarlarkValue.java:120`) and now also the
vendored interpreter's (`unhashable type: 'list'`, ...); a `def` or
`lambda` has type `function` and repr `<function NAME from LABEL>`, LABEL being
the defining file's canonical label (`StarlarkFunction.java:222-231`,
`cmdline/BazelModuleContext.java:133-135`), e.g. `<function _f from //pkg:defs.bzl>`.

| Value | repr | Native |
|---|---|---|
| top-level builtins (`len`, `depset`, `rule`, `provider`, ...; `StarlarkRuleFunctionsApi` is no `StarlarkValue`, `StarlarkRuleFunctionsApi.java:39`) | `<built-in function rule>` | vendor `NativeFunction` |
| `config.string`, ... (`StarlarkConfigApi extends StarlarkValue`, `starlarkbuildapi/config/StarlarkConfigApi.java:32-49`) | `<built-in method string of config value>` | `set_builtin_receiver("config")` in `src/build_settings.rs` |
| BUILD-file `glob`, `package_group`, `exports_files`, `package_name`, `repo_name`, `package_relative_label`, `module_name`, ... (`StarlarkGlobalsImpl.java:76-78`, `StarlarkNativeModule.java:84-90`) | `<built-in method glob of native value>` | `install_build_native_methods` in `src/package.rs` |
| `native.cc_toolchain_suite` / BUILD `cc_toolchain_suite` (a `_builtins` lambda, `src/main/starlark/builtins_bzl/bazel/exports.bzl:53-54`) | `<function lambda from @@_builtins//:bazel/exports.bzl>`, type `function` | `native_functions::CcToolchainSuite` |
| the `attr` module | `<attr>`, type `attr` (`StarlarkAttrModule.java:660-662`) | `starlark_types::AttrModule` |
| `native.glob`, `native.package_group`, ... (`native` is a struct, `lib/packages/BazelStarlarkEnvironment.java:277-279`, of methods bound to a `StarlarkNativeModule`, `lib/packages/StarlarkNativeModule.java:84-90`; `StarlarkNativeModuleApi extends StarlarkValue`, `lib/starlarkbuildapi/StarlarkNativeModuleApi.java:49`) | `<built-in method glob of native value>` | `GlobalsBuilder::set_builtin_receiver("native")` in `src/package.rs` |
| `attr.string`, ... (`StarlarkAttrModuleApi extends StarlarkValue`, `StarlarkAttrModuleApi.java:57`) | `<built-in method string of attr value>` | `set_builtin_receiver("attr")` in `src/custom_rule.rs` |
| bound methods (`"x".upper`, `ctx.actions.write`, `depset.to_list`) | `<built-in method upper of string value>` | vendor `BoundMethodGen` (receiver `type()`) |
| MODULE.bazel functions (`ModuleFileGlobals` is no `StarlarkValue`, `ModuleFileGlobals.java:61`) | `<built-in function bazel_dep>` | `src/bzlmod.rs` `Builtin` |

Call-stack frames keep naming native functions and methods by their plain name.

## Module and repository values

| Value | repr | Bazel source |
|---|---|---|
| repository rule | `<starlark repository rule @@//pkg:defs.bzl%name>` / `<anonymous starlark repository rule>` | `lib/bazel/repository/starlark/StarlarkRepositoryModule.java:153-159`; `lib/bazel/bzlmod/RepoRuleId.java:32-34` (unambiguous `.bzl` label) |
| `repository_ctx` | `<unknown object com.google.devtools.build.lib.bazel.repository.starlark.StarlarkRepositoryContext>` | `StarlarkRepositoryContext.java:73` |
| `module_ctx` | `<unknown object com.google.devtools.build.lib.bazel.bzlmod.ModuleExtensionContext>` | `ModuleExtensionContext.java:51` |
| `bazel_module` | `<unknown object com.google.devtools.build.lib.bazel.bzlmod.StarlarkBazelModule>` | `StarlarkBazelModule.java:45` |
| `bazel_module.tags` | `<unknown object com.google.devtools.build.lib.bazel.bzlmod.StarlarkBazelModule$Tags>`, type `bazel_module_tags` | `StarlarkBazelModule.java:51-94` |
| tag | `<unknown object com.google.devtools.build.lib.bazel.bzlmod.TypeCheckedTag>`, type `bazel_module_tag` | `TypeCheckedTag.java:38-39` |
| `tag_class(...)` | `<unknown object com.google.devtools.build.lib.bazel.bzlmod.TagClass>` | `TagClass.java:33` (record) |
| `module_extension(...)` | `<unknown object com.google.devtools.build.lib.bazel.bzlmod.ModuleExtension>`, type `ModuleExtension` | `ModuleExtension.java:37` (record without `@StarlarkBuiltin`) |
| `use_extension(...)` | `<unknown object com.google.devtools.build.lib.bazel.bzlmod.ModuleFileGlobals$ModuleExtensionProxy>` | `ModuleFileGlobals.java:548-549` |
| `proxy.tag` | `<unknown object ...ModuleFileGlobals$ModuleExtensionProxy$TagCallable>`, type `TagCallable` | `ModuleFileGlobals.java:579` |
| `use_repo_rule(...)` | `<unknown object ...ModuleFileGlobals$RepoRuleProxy>` | `ModuleFileGlobals.java:832-833` |
| `bazel_dep`, `module`, ... | `<built-in function bazel_dep>`, type `builtin_function_or_method` | `ModuleFileGlobals.java:61` |
| `path` | the quoted path; `str` (and now `%s`) is the path | `StarlarkPath.java:209-211` |

Already-matching values (unchanged): `Attribute` `<attr.KIND>`, `Aspect`,
`transition`, `select`, `exec_transition`, platform providers, language
modules, `json`, `Facts`, `extension_metadata`, `repository_os`,
`RunEnvironmentInfo`, `RawConstructor`, `build_setting`.

## Vendor patches (starlark-rust 0.14.2)

Recorded in `vendor/starlark-0.14.2.provenance.json`:

- `src/values/types/structs/value.rs`: `struct(name = value)` with fields sorted
  by code point; nested values use their own `repr`.
- `src/values/types/function.rs`: `NativeFunction` and `BoundMethod` print
  `BuiltinFunction.repr` and have type `builtin_function_or_method`, which
  (unlike `function`) is unhashable as in Bazel; `name_for_call_stack` keeps
  the plain name.
- `src/environment/globals.rs`: `GlobalsBuilder::set_builtin_receiver`, applied
  to functions set or populated afterwards.
- `src/values/types/string/interpolation.rs`, `dot_format.rs`: `%s` and the
  single-`{}` fast path format with `str()`; `{x!r}` / missing keywords report
  `Missing argument 'x!r'`.
- `src/eval/compiler/def.rs`: `def`/`lambda` repr `<function NAME from FILE>`.
- `src/values/traits.rs`, `src/values/layout/vtable.rs`: the
  `StarlarkValue::collect_debug_print(collector, eval)` hook (default `str()`).
- `src/stdlib/extra.rs` (`print`), `src/stdlib/funcs/other.rs` (`fail`): format
  arguments with debugPrint.
- `src/values/error.rs`: the unhashable-value error reads `unhashable type: 'T'`
  (`StarlarkValue.java:120`; its one constructor is the default
  `StarlarkValue::write_hash`, and `set.__contains__` still recognizes it).
- The crate's own tests follow these changes (all declared in the provenance):
  `environment/modules.rs`, `values/layout/value.rs`, `tests/{basic,uncategorized,fstring,opt/def_inline}.rs`,
  `values/types/set/{methods,set,value}.rs`, the `string/methods.rs` format doc test,
  `testcases/eval/go/function.star`, and 29 regenerated `.golden` files (bytecode
  dumps, profiles, `typing/tests/golden/incorrect_type_dot.golden`) whose only
  changes are the builtin/def/struct reprs and the builtin type name. Tests that
  used builtins as hashable values or `!r`/`!s` conversions incidentally keep their
  purpose with `def`/`lambda` keys, `repr()` and lists, plus assertions of the
  new behaviour. The vendored crate passes 964 lib tests and 157 doc tests
  (17 ignored) under the frozen isolated test graph.
- Not used by bazel-rs and left unchanged: `Value::describe` and the debug
  adapter (`src/debug/adapter.rs`) special-case type `function`, so builtins
  now take their generic paths there.

## Remaining differences

- Reading a field of an immutable (returned) `ctx.outputs` fails in both; Bazel says
  `cannot access outputs of rule '//pkg:p' outside of its own rule implementation function`
  (`StarlarkRuleContext.java:440-447`), natively the attribute is absent
  (``Object of type `Outputs` has no attribute `o` ``), and `hasattr` is `False`
  natively where Bazel raises that error (`Starlark.java:967-971`).
- When the main repository has several apparent names for one repository, Bazel's
  display form (debugPrint, Args) uses the first in mapping insertion order
  (`RepositoryMapping.java:143-170`); the native main mapping is ordered by apparent
  name, so it picks the alphabetically first.
- `type(repository_ctx.attr)` is `RepoDefinition` in Bazel (capture
  `B/module_values`); natively the attribute struct has type `struct` (its repr
  matches).
- `print(args)`: Bazel's `Args.debugPrint` expands Labels without a main
  repository mapping (`Args.java:76`, `@@dep+//pkg:x`); native Args store
  arguments already expanded with the mapping (`@dep//pkg:x`).
- The debugPrint keys of native-rule targets other than filegroup (genrule,
  platform rules, ...) are approximated as `[OutputGroupInfo]`; native
  platform providers are not listed. A filegroup lists
  `[InstrumentedFilesInfo, OutputGroupInfo]` (`rules/filegroup/Filegroup.java:103-108`).
- A bzlmod tag's debugPrint (`'NAME' tag at LOCATION`, `TypeCheckedTag.java:163-166`)
  is not modelled; it prints the repr.
- Bazel's `fail()` also accepts `msg`, `attr`, `sep` and `stack_trace`
  (`MethodLibrary.java:841-913`); the vendored `fail(*args)` does not.
- Merged targets (aspects) are not modelled; `{x!r}` forms the vendored format
  parser rejects outright (`{a!x}`, `{a!rr}`) keep its own syntax errors.

Release note: action keys and outputs change for rules that write `str()`,
`repr()`, `%s` or `"{}".format()` of Labels, Files, targets, structs, providers,
depsets or functions, and for `Args` holding Labels of external repositories,
which now expand to their apparent display form.

Receipts that recorded these as `KNOWN_DIVERGENCE` (N9 provider/instance reprs,
N11/N13 Label reprs and `native.package_group`, the `struct(a = 1)` spacing in
cc_common_loading, N15 ctx.attr Label reprs) are re-run by the paired N22
capture (`tests/fixtures/repr/reference_receipt.json`,
`earlier_receipt_divergences_recaptured`); the earlier receipts themselves are
unchanged.

Tests: `tests/repr.rs` (end-to-end builds incl. print/fail/Args and the capture fixes), `src/starlark_types.rs`
`bazel_repr_and_str_of_analysis_values`, `file_and_target_reprs_follow_their_configured_class`,
`src/providers.rs`, `src/package.rs`, `src/repository_rule.rs`, `src/bzlmod.rs`.
