# Analysis-time coverage API (Bazel 9.3)

Code: `src/instrumented_files.rs` (N54), `src/engine.rs` (native targets),
`src/ctx_surface.rs`/`src/analysis_ctx_surface.rs` (`coverage_enabled`,
`ctx.coverage_instrumented`). Tests: `tests/instrumented_files.rs`. Pinned
source `/tmp/opencode/bazel-9.3-reference`; paths relative to
`src/main/java/com/google/devtools/build/lib/`.

`--collect_code_coverage` is a native option only Starlark transitions set
(N47, docs/CONFIGURATION.md); the command line and `bazel coverage` reject
it. Everything below is the behaviour **without** coverage; in a coverage
configuration every instrumentation step fails explicitly (`... in a
configuration with --collect_code_coverage (set by a Starlark transition) is
not implemented in the native profile ...`).

## `coverage_common.instrumented_files_info`

CoverageCommon.instrumentedFilesInfo (`analysis/test/CoverageCommon.java:47-157`,
parameters `starlarkbuildapi/test/CoverageCommonApi.java:39-133`):

1. Argument binding and `@Param` types, in declaration order: `ctx`
   (positional or named; `want 'ctx'`), named-only `source_attributes` and
   `dependency_attributes` (`sequence`), `coverage_support_files`
   (`sequence or depset`), `coverage_environment` (`dict`), `extensions`
   (`sequence or NoneType`), `metadata_files` (`sequence`),
   `reported_to_actual_sources` (`depset or NoneType`),
   `baseline_coverage_files` (`sequence or NoneType`); BuiltinFunction texts
   for unknown keywords, extra positionals and the missing `ctx`.
2. The element casts, in Bazel's order: `extensions` (strings),
   `reported_to_actual_sources` (a depset of tuples), `baseline_coverage_files`
   (Files), `coverage_environment` (`dict<string, string>`),
   `coverage_support_files` (Files and File depsets; `at index N of
   coverage_support_files, got element of type T, want one of depset, File or
   FilesToRunProvider`). Non-empty support files, environment or
   reported-to-actual sources need the private-API allowlist
   (BuiltinRestriction: `file 'L' cannot use private API`; rules_cc is
   allowed). Then `source_attributes`, `dependency_attributes` (strings) and
   `metadata_files` (Files).
3. InstrumentedFilesCollector.collect (`analysis/test/InstrumentedFilesCollector.java:94-148`)
   returns the shared `InstrumentedFilesInfo.EMPTY` when coverage is disabled:
   the attribute names are not looked up and nothing is collected.

## `InstrumentedFilesInfo`

A BuiltinProvider (`<function InstrumentedFilesInfo>`, type `Provider`, not
callable: `'Provider' object is not callable`). The EMPTY instance
(`analysis/test/InstrumentedFilesInfo.java:34-41`): type
`InstrumentedFilesInfo`, `instrumented_files` and `metadata_files` empty
depsets, `dir()` `["get_coverage_environment", "get_coverage_support_files",
"instrumented_files", "metadata_files"]`, repr NativeInfo's
`struct(instrumented_files = depset([]), metadata_files = depset([]))`
(`packages/NativeInfo.java:120-138`), equal to and hashing like any EMPTY;
the two `get_*` methods are private API.

Targets carrying it without coverage: Starlark rules that return it,
`filegroup` (`rules/filegroup/Filegroup.java:63-108`) and `genrule`
(`rules/genrule/GenRuleBase.java:234-239`), and every output file (its
generating rule's or EMPTY, `analysis/configuredtargets/OutputFileConfiguredTarget.java:100-107`);
aliases answer for their actual. Source files and rules that do not return it
have none (RuleConfiguredTargetBuilder only forwards it with coverage,
`analysis/RuleConfiguredTargetBuilder.java:179-186`).

Paired capture: `artifacts/instrumented_files-capture.py`, receipt
`tests/fixtures/instrumented_files/reference_receipt.json`.
