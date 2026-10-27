# Coverage (Bazel 9.3): `bazel coverage` and the analysis-time API

Code: `src/coverage.rs` (filter, options, native coverage output
generator, combined report), `src/instrumented_files.rs`
(`coverage_common.instrumented_files_info`, `InstrumentedFilesInfo`),
`src/analysis.rs` (forwardAll, `test_coverage`), `src/engine.rs` (native
rules, run plans, baselines), `src/test_runner.rs` (the coverage test
action), `src/main.rs` (the command). Tests: `tests/coverage_command.rs`
(paired genuine capture `tests/fixtures/coverage_command/genuine`, fixture
`tests/fixtures/coverage_command/workspace`), `tests/go_transitions.rs`
(coverage configurations below a Starlark transition),
`tests/instrumented_files.rs` (without coverage), unit tests in
`src/coverage.rs`. Pinned source `/tmp/opencode/bazel-9.3-reference`;
paths relative to `src/main/java/com/google/devtools/build/lib/`.

## The command and the options

- `bazel-rs coverage` is CoverageCommand (`runtime/commands/CoverageCommand.java`):
  `test` (its rc groups `always`, `common`, `build`, `test`, `coverage`)
  with `--collect_code_coverage` forced (SOFTWARE_REQUIREMENT, over
  `--nocollect_code_coverage`) and `--test_timeout=300,600,1200,3600` unless
  given (COMPUTED_DEFAULT). `--collect_code_coverage` is a command-line
  option of every building command too (`build`, `test`, `run`; `query`
  accepts and ignores it).
- `--instrumentation_filter` (RegexFilter: comma-separated, `\,` escapes,
  `-` excludes, atomic-group union, `Matcher.find`), `--instrument_test_targets`,
  `--experimental_collect_code_coverage_for_generated_files`: configuration
  options (`NativeOptions::instrumentation`, in the configuration identity
  only when set, not propagated to exec configurations; the output
  directory stays `k8-fastbuild` as in Bazel).
- Without an explicit `--instrumentation_filter` a coverage `test` computes
  InstrumentationFilterSupport's default from the tests to run (suites
  expanded; `/internal`, `/public`, `/tests` suffixes dropped, `javatests/`
  → `java/`, `test/java/` → `main/java/`, nested prefixes dropped, `^//[/:]`
  → `^//`) before analysis and prints `INFO: Using default value for
  --instrumentation_filter: "..."` / `INFO: Override the above default with
  --instrumentation_filter`.
- `--combined_report=lcov|none` (default `lcov`),
  `--experimental_fetch_all_coverage_outputs` (COVERAGE_DIR becomes
  `<testlogs dir>/_coverage`, published with the test logs).
- Explicit errors: `--coverage_output_generator`, `--coverage_report_generator`,
  `--coverage_support` with any value, and
  `--experimental_split_coverage_postprocessing` (the second coverage spawn).
- An analysis snapshot is neither looked up nor saved for a coverage build.

## Analysis in a coverage configuration

- `coverage_common.instrumented_files_info` is InstrumentedFilesCollector.collect:
  the dependency attributes' InstrumentedFilesInfo (instrumented, metadata,
  baseline and support files, environment), the coverage environment and
  support files given, and — when the filter includes the rule
  (`--instrument_test_targets` for test rules) — the source attributes'
  files to build (source files unless `..._for_generated_files`; `extensions`
  are filename suffixes, `[]` none) as local sources, with a
  BaselineCoverageAction (`<testlogs>/<pkg>/<name>/baseline_coverage.dat`,
  `SF:<path>` / `end_of_record` per source) unless
  `baseline_coverage_files` is given.
- RuleConfiguredTargetBuilder's forwardAll: a Starlark rule returning neither
  InstrumentedFilesInfo nor ToolchainInfo forwards its non-tool attribute
  prerequisites' (implicit ones included); natively filegroup collects from
  `srcs`/`data`, genrule its `srcs` local sources (no baseline action:
  declared divergence), other native rules forward all target-configuration
  edges, aliases their actual's, output files their generating rule's.
- `ctx.coverage_instrumented([target])`, `ctx.fragments.coverage.output_generator`
  (the default label `@@bazel_tools//tools/test:lcov_merger`) and
  `ctx.configuration.coverage_enabled` follow Bazel. The late-bound
  `configuration_field("coverage", "output_generator")` default (rules_go's,
  rules_python's and rules_shell's `_lcov_merger`) stays `None`: Bazel's
  generator is `@remote_coverage_tools//:Main`, a rules_java `java_binary`
  the native profile cannot build, so no dependency edge is made (declared
  divergence: `ctx.attr._lcov_merger` is None).

## The coverage test action

TestActionBuilder/TestRunnerAction with a test rule's InstrumentedFilesInfo:

- command `test-setup.sh <collect_coverage exec path> <test> args...`
  (`$collect_coverage_script`, the rules_shell sh_binary of
  `@bazel_tools//tools/test:collect_coverage`, runs the genuine
  collect_coverage.sh);
- inputs: the script, the coverage support and metadata files,
  `:coverage_support` (collect_coverage.sh), `_collect_cc_coverage`; the
  instrumented file manifest `<bin>/<pkg>/<name>.instrumented_files`
  (sorted exec paths of the instrumented and metadata files) and
  `reported_to_actual_sources.txt` are written into the execution root (no
  separate FileWrite action natively);
- environment: the coverage environment (`BAZEL_CC_COVERAGE_TOOL=GCOV`,
  `CC_CODE_COVERAGE_SCRIPT`, `LCOV_MERGER`, the InstrumentedFilesInfo's) below
  every other value, then `RUNTEST_PRESERVE_CWD=1`, `COVERAGE_MANIFEST`,
  `COVERAGE_DIR=<testlogs>/_coverage/<pkg>/<name>/test`,
  `COVERAGE_OUTPUT_FILE=<testlogs dir>/coverage.dat`,
  `SPLIT_COVERAGE_POST_PROCESSING=0`, `IS_COVERAGE_SPAWN=0`;
- `LCOV_MERGER` (rules with an `_lcov_merger`) is
  `external/bazel_tools/tools/test/bazel-rs-coverage-output-generator`, a
  launcher running this binary's hidden `__coverage-output-generator`
  subcommand: a port of CoverageOutputGenerator's LCOV merger (LcovParser,
  Coverage, LcovPrinter, Main: `--coverage_dir`, `--reports_file`,
  `--output_file`, `--filter_sources`, `--source_file_manifest`,
  `--sources_to_replace_file`, `--legacy_branches`; an empty result is an
  empty file and `WARNING: There was no coverage found.` without the
  java.util.logging header line). The release Bazel 9.3 runs (v2.9) keeps a
  branch's evaluated flag per branch (`BRDA:...,-` survives next to an
  evaluated branch of the same line; paired capture). gcov/`.gcov.json.gz`
  files fail explicitly (C/C++ coverage is not modelled); a lone `.profdata`
  is copied like Bazel.
- `coverage.dat` is published with the test logs and cached with the test
  result; the launcher's absolute path is not part of the test key.

## The combined report

After the tests, `--combined_report=lcov` writes below the execution root
(`bazel-rs info execution_root`) `bazel-out/_coverage/baseline_lcov_files.tmp`
(the baseline coverage files of the top-level targets) and
`coverage_lcov_files.tmp` (those, then every coverage test action's
coverage.dat), merges them with the native generator into
`_baseline_report.dat` and `_coverage_report.dat`, and prints
`INFO: LCOV coverage report is located at <path>\n and execpath is
bazel-out/_coverage/<name>` for each (CoverageReportActionBuilder,
BazelCoverageReportModule). A generator failure fails the command (exit 1).

Paired evidence: a scratch rules_go 0.63.0 `go_test` + rules_shell
`sh_test` workspace (both reports, the Go test's coverage.dat and the
go_library's baseline_coverage.dat byte-identical to genuine 9.3.0; only the
test order inside `coverage_lcov_files.tmp` differs, which Bazel does not
fix either) and `tests/fixtures/coverage_command` (Starlark rules: filter
default, test environment, manifest, merged coverage.dat with FN/FNDA/BRDA,
`--filter_sources` and manifest filtering, reports). On `~/code`:
`coverage //src/beyondwork.ai/utils/str:str_test` (rules_go with nogo).

# Without coverage

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
have none. With coverage, RuleConfiguredTargetBuilder adds
InstrumentedFilesCollector.forwardAll to **every** rule that provides neither
InstrumentedFilesInfo nor ToolchainInfo (`analysis/RuleConfiguredTargetBuilder.java:179-186`):
see "Analysis in a coverage configuration" above. A collected instance's
repr is NativeInfo's with its `instrumented_files`/`metadata_files` depsets.

Paired capture: `artifacts/instrumented_files-capture.py`, receipt
`tests/fixtures/instrumented_files/reference_receipt.json`.
