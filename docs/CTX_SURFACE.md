# Rule context surface (N23c)

The native rule `ctx` (`src/analysis_ctx.rs`, `src/analysis_ctx_surface.rs`,
`src/ctx_surface.rs`) has every member of Bazel 9.3's
`StarlarkRuleContext` for rules: `dir(ctx)` is Bazel's 36 names
(`analysis/starlark/StarlarkRuleContext.java`, `starlarkbuildapi/StarlarkRuleContextApi.java`;
paths below are relative to `src/main/java/com/google/devtools/build/lib/` of
the pinned source `/tmp/opencode/bazel-9.3-reference`). Each member either
returns the value Bazel computes for the configuration the target is analyzed
in, or fails explicitly. An unknown member reports Bazel's
`'ctx' value has no field or method 'X'` (with its `did you mean` hint). Reads
outside the implementation keep the scope check of `checkMutable`.

Values come from the target's configuration (`Engine::ctx_surface`,
`src/engine.rs`): the output root, `--compilation_mode`, `--define`,
`--stamp`, `--strip`/`--force_pic`, `--platforms`/`--host_platform`, the exec
flag and the no-config configuration. Every other option the values depend
on is rejected on the native command line, so its Bazel default is the value
of every native configuration (for example `--collect_code_coverage`,
`--features`, `--action_env`, `--test_env`, the Java/Apple/ObjC/proto options).

## Members

| member | native |
| --- | --- |
| `fragments` | `FragmentCollection` (`analysis/FragmentCollection.java`): type `fragments`, repr `<unknown object com.google.devtools.build.lib.analysis.FragmentCollection>`. Fields are the configuration's Starlark fragments (`BuildConfigurationValue.getStarlarkFragmentNames`, `:360-370, 980-986`); `dir()` sorts them, messages list them in fragment-class order (`FragmentClassSet.LEXICAL_FRAGMENT_SORTER`): `'platform', 'coverage', 'android', 'bazel_android', 'apple', 'cpp', 'java', 'j2objc', 'objc', 'proto'` (`py`/`bazel_py` are dropped by `Fragment.shouldInclude`). An unknown name: `There is no configuration fragment named 'X'. Available fragments: '...'` (`FragmentCollectionApi.java:39-49`); a fragment neither declared in `rule(fragments=...)` (`StarlarkRuleClassFunctions.java:1051-1052`) nor universal (`platform`, `BazelRuleClassProvider.java:235-237`): `R has to declare 'X' as a required fragment in order to access it. Please update the 'fragments' argument of the rule definition (for example: fragments = ["X"])` (`RuleContext.java:511-539`), also from `hasattr`/`getattr`. The no-config configuration has no fragments (`Available fragments: ''`). |
| `configuration` | type `configuration`, Bazel's repr; `bin_dir`, `genfiles_dir`, `host_path_separator` (`:`), `default_shell_env` (`{"PATH": "/bin:/usr/bin:/usr/local/bin"}`, strict action env), `test_env` (`{}`), `coverage_enabled` (False), `short_id`, `is_tool_configuration()`; private API (`BuiltinRestriction` default allowlist): `stamp_binaries()`, `has_separate_genfiles_directory()` (False), `is_sibling_repository_layout()` (False), `runfiles_enabled()` (True), `disabled_features()` (`set()`) (`analysis/config/BuildConfigurationValue.java:396-969`). |
| `bin_dir`, `genfiles_dir` | an `ArtifactRoot`: type `root`, repr `<derived root>`, `dir()` `["path"]`, equal by path; `genfiles_dir` is `bin_dir` (`--incompatible_merge_genfiles_directory`). |
| `var` | an immutable dict (`StarlarkRuleContext.var`, `:832-850`; `ConfigurationMakeVariableContext.collectMakeVariables`): `TARGET_CPU` (the target platform's `@platforms//cpu:cpu` value name, `--incompatible_target_cpu_from_platform`, `skyframe/config/BuildConfigurationFunction.java:96-122`; `""` without one), `COMPILATION_MODE`, `BINDIR`, `GENDIR` (`BuildConfigurationValue.java:322-333`), the `TemplateVariableInfo` variables of the prerequisites of the attributes `toolchains`, `:cc_toolchain`, `$toolchains`, `$cc_toolchain` (Starlark `_cc_toolchain` late-bound, `_toolchains`, `_cc_toolchain`; `RuleContext.java:900-940`) and of the resolved toolchains (`ResolvedToolchainContext.java:56-85`) — merged later-wins and sorted for `ctx.var`, first match for lookups (`MakeVariableSupplier.java:44-70`) — then the `--define` values; `Dict.Builder` keeps the last value at the first position. A deferred default host platform is resolved on demand (crate::engine). |
| `expand_make_variables(attr, cmd, subst)` | `TemplateExpander` (`analysis/stringtemplate/TemplateExpander.java`) over `subst`, `--define`, toolchain variables and the global environment; errors are attribute errors (`in A attribute of R rule L: $(X) not defined`, unterminated references, `'$X' syntax is not supported ...`) that fail the target after the implementation, which receives the command unchanged (`analysis/Expander.java:159-172`). |
| `expand_location(input, targets, short_paths)` | text without a location function (`$(location)`, `$(execpath)`, ...) is returned unchanged and an unterminated one is a rule error (`analysis/LocationExpander.java:172-224`); `short_paths=True` needs private API; resolving a location function **fails explicitly** (the location map of the rule's prerequisites is not modelled). |
| `workspace_name` | `_main`. |
| `build_file_path` | `pkg/BUILD.bazel` relative to its repository root (`--incompatible_stop_exporting_build_file_path` is false by default). |
| `features`, `disabled_features` | `RuleContext.computeFeatures` (`RuleContext.java:268-276`, `packages/FeatureSet.java`): the package's features (REPO.bazel `repo()` and `package()` layers, each `FeatureSet.parse`d and merged with `FeatureSet.merge`, the finer layer winning: `PackageArgs.java:126-129, 175-177, 220-222`) merged with the rule's `features`, under the empty configuration features (`--features`/`--host_features` are rejected natively). |
| `coverage_instrumented(target)` | False (coverage is never enabled natively); Bazel's parameter-type error. |
| `exec_groups` | N35's `StarlarkExecGroupCollection` over the ToolchainCollection (docs/EXEC_GROUPS.md): type `ExecGroupCollection`, repr `<ctx.exec_groups: A, B>` with the declared, default `test` and automatic groups sorted; `in`, indexing to an exec group context with `.toolchains`, and `In R rule L, unrecognized exec group 'X' requested. Available exec groups: [...]`. A rule without toolchain resolution has no ToolchainCollection (Bazel 9.3 fails with a NullPointerException): explicit failure. |
| `tokenize`, `check_placeholders`, `package_relative_label` | `ShellUtils.tokenize` (`util/ShellUtils.java:90-143`), `ImplicitOutputsFunction.createPlaceholderSubstitutionFormatString`, `Label.parseWithPackageContext` with the rule repository's mapping (native label syntax messages, except Bazel's `target names may not contain ':'`). |
| `split_attr` | `struct()`; an attribute with a Starlark transition fails explicitly. |
| `super()` | `Can't use 'super' call, the rule has no parent.` (`rule(parent=...)` is rejected at load time). |
| `created_actions()` | None (`_skylark_testable` rules are not analyzed). |
| `rule`, `aspect_ids` | `'X' is only available in aspect implementations`. |
| `build_setting_value` | of a non-setting: `attempting to access 'build_setting_value' of non-build setting L`. |
| `resolve_tools` | Bazel's `--incompatible_disallow_ctx_resolve_tools` error. |
| `info_file`, `version_file`, `resolve_command` | **fail explicitly** (workspace status artifacts and command resolution are not modelled). |

Every configuration-derived member (`configuration`, `bin_dir`,
`genfiles_dir`, `var`, `fragments`, `features`) is computed per configured
target key (`Engine::ctx_surface` in `Engine::plan_target`), never from a
build-global configuration: a dependency behind a Starlark attribute
transition and a rule-transitioned target (N18c) see their transitioned
configuration (e.g. `rust-dbg-ST-<hash>` roots and `COMPILATION_MODE`),
exec and no-config targets theirs, and a `flag_values` key planned early for
a `config_setting` (N30, docs/TEST_FLAGS.md) its own. `ctx.split_attr` of a
rule with a Starlark-transition attribute fails explicitly.

## Configuration fragments

Each fragment has Bazel's type name, repr (`<unknown object CLASS>`) and
`dir()`; struct fields are values, the other members are
`builtin_function_or_method` values (`<built-in method M of F value>`); members
calling `failIfCalledOutsideDefaultAllowlist` check the calling `.bzl`.

* `cpp`: the 49 CppConfiguration members of docs/SUBRULES.md (shared with
  `subrule_ctx.fragments.cpp`); the exec configuration reports its own
  compilation mode (`opt`) and strip mode, and `copts`/`cxxopts` are
  `["-g0"]` there (the exec transition appends `-g0` on non-Windows hosts,
  builtin_exec_platforms.bzl:341-342).
* `platform`: `host_platform`, `platform` (the first `--platforms` entry),
  `platforms` (`PlatformConfigurationApi`).
* `java`: all 22 JavaConfiguration members at their JavaOptions defaults
  (`default_javac_flags_depset` is an empty preorder depset; the
  thread-taking methods are private API); `default_jvm_opts` is
  `["-XX:ErrorFile=/dev/stderr"]` in exec configurations (the exec
  transition's `--jvmopt`, builtin_exec_platforms.bzl:350-354).
* `apple`: all 15 members: `apple_platform_type` `macos`, `single_arch_cpu`
  and `apple_cpus` from the host CPU defaults
  (`AppleCommandLineOptions.java:174-192`), `single_arch_platform` the
  `macos` `apple_platform` (`name`, `platform_type`, `is_device`,
  `name_in_plist`), the version flags None (`--xcode_version` and the
  `--<platform>_sdk_version`/`--<platform>_minimum_os` options are rejected
  natively; once modelled these must report them as DottedVersion values,
  docs/XCODE_VERSION_INFO.md), `prefer_mutual_xcode` True,
  `include_xcode_exec_requirements` False.
* `objc`: all 11 members; `copts_for_current_compilation_mode` is
  `["-O0", "-DDEBUG=1"]` in fastbuild and empty otherwise
  (`--incompatible_avoid_hardcoded_objc_compilation_flags`);
  `uses_device_debug_entitlements` is False under `opt` (also every exec
  configuration, ObjcConfiguration.java:150-157);
  `target_should_alwayslink(ctx)` is False, and fails explicitly for a rule
  with an `alwayslink` attribute (explicitly specified values are not tracked).
* `proto`, `j2objc`, `coverage` (`output_generator` None without coverage).
* `android` (37 members) and `bazel_android` (1): `dir()` matches, every read
  **fails explicitly**.

## Divergences

* **D1**: roots use the CPU token `rust` where Bazel writes `k8`
  (`bazel-out/rust-fastbuild/bin`, docs/CONFIGURATION.md); `ctx.var["BINDIR"]`
  and `GENDIR` follow. `TARGET_CPU` is the platform's CPU (`x86_64`) on both.
* `configuration.short_id` is the first 7 hex digits of the native
  configuration identity, not Bazel's BuildOptions checksum.

## Evidence

`tests/ctx_surface.rs` (CLI), unit tests in `src/ctx_surface.rs`, and the paired
capture against genuine Bazel 9.3 (`artifacts/ctx_surface-capture.py`,
tracked receipt `tests/fixtures/ctx_surface/reference_receipt.json`).
