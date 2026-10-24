# `bazel test` and test targets under `run` (N23g)

N23g adds Bazel 9.3's `test` command for local execution and lets `run` run
test targets. Source paths are relative to
`src/main/java/com/google/devtools/build/lib/` of the pinned Bazel 9.3 sources
(`/tmp/opencode/bazel-9.3-reference`, e89755b). Implementation:
`src/test_command.rs` (options, properties, filters, environment, summary,
test.xml parsing), `src/test_runner.rs` (execution, attempts, caching,
publication), `src/engine.rs` (`rule_edges`, `target_pattern_phase` (the
target pattern phase: the tests selected, test suites expanded),
`test_rule_info`, `plan_request`), `src/request.rs` (the planned request a
`test` executes, docs/PLANNED_REQUEST.md), `src/action.rs` (`ContentInput`,
`stage_content_inputs`), `src/main.rs` (`test`, `run` of tests). Runfiles
trees and `run` are N23f (docs/RUNFILES_TREES.md).

## Command and options

`bazel-rs test [build options] [test options] TARGET...`; `test` inherits the
`build` options and rc groups (`always`, `common`, `build`, `test`;
TestCommand `inheritsOptionsFrom`). The test options of TestConfiguration and
ExecutionOptions are also accepted by `build`/`run` (they are build options in
Bazel); `--keep_going`/`-k` only by `test` (only its effect on test actions is
modelled). Supported, with Bazel's converter errors (`While parsing option
--test_output=bogus: Not a valid test output: 'bogus' (should be summary,
errors, all or streamed)`, TestTimeoutConverter, TestAttemptsConverter,
EnumFilterConverter, TriStateConverter):

| option | semantics |
|---|---|
| `--test_output=summary\|errors\|all\|streamed` | TestLogHelper blocks after each attempt; `streamed` copies the log after its header to stdout as it grows, disables sharding and runs one test at a time (TestCommand.java:84-107 warning) |
| `--test_summary=short\|short_uncached\|terse\|detailed\|detailed_uncached\|testcase\|none` | TerminalTestResultNotifier.printSummary/printStats, TestSummaryPrinter (test.xml parsed like TestXmlOutputParser for the detailed formats) |
| `--cache_test_results=auto\|yes\|no`, `-t`, `-t-` | TestRunnerAction.shouldAcceptCachedResult/executeUnconditionally (:565-680) |
| `--test_env`, `--test_arg`, `--test_filter`, `--test_runner_fail_fast` | TestPolicy / setupEnvVariables / TestTargetExecutionSettings |
| `--test_timeout=N\|N,N,N,N` | TestTimeout.TestTimeoutConverter |
| `--flaky_test_attempts=N\|default` | TestStrategy.getTestAttempts (flaky: 3) |
| `--runs_per_test=N` | TestActionBuilder (run directories, TEST_RANDOM_SEED/TEST_RUN_NUMBER) |
| `--test_sharding_strategy=explicit\|disabled\|forced=K` | TestConfiguration.TestShardingStrategy |
| `--test_tag_filters`, `--test_size_filters`, `--test_timeout_filters`, `--test_lang_filters`, `--build_tests_only` | TestFilter, TargetPatternPhaseFunction |
| `--[no]verbose_test_summary`, `--test_verbose_timeout_warnings`, `--max_test_output_bytes`, `--[no]incompatible_check_sharding_support`, `--zip_undeclared_test_outputs`, `--local_test_jobs`, `--test_tmpdir`, `--test_strategy=standalone\|local\|exclusive`, `--[no]keep_going` | as in Bazel |

Explicit failures: coverage (`--collect_code_coverage`, not parsed),
per-label `regex@N` forms of `--flaky_test_attempts`/`--runs_per_test`,
`--check_tests_up_to_date`, `--print_relative_test_log_paths` (no
convenience symlinks), other `--test_strategy` values, `--run_under` with a
test under `run`, a `select()` in `shard_count` of a test to run (`test`, `run`
of a test; a build, the `--build_tests_only` filters and a test_suite's
runfiles do not read the shard count), `--keep_going` with a build
failure (the native build stops at the first failing action). (A `select()`
in test_suite `tests` is a loading error: the attribute is not configurable.)

## Selecting tests

TargetPatternPhaseFunction (skyframe/TargetPatternPhaseFunction.java:75-250,
determineTests :408-500; `Engine::target_pattern_phase`, shared with
`build` and `run`, docs/RUNFILES_TREES.md): the targets are every pattern's targets (FILTER_MANUAL, negative
patterns subtracted); the tests are determineTests' — per positive pattern
its test rules and test suites (FilteringPolicies.FILTER_TESTS: manual ones
only when named explicitly), per negative pattern its unfiltered targets,
each expanded by TestsForTargetPatternFunction and added or removed in
pattern order, then the command-line filters (TestFilter; tag filters use
Rule.getRuleTags: tags, size, timeout, `flaky`/`noflaky`, `local`/`nolocal`
— a false boolean is `no` + its name, Type.BOOLEAN.toTagSet). `test` builds the
targets plus the tests, `--build_tests_only` (also under `build`) only the
tests; with `--expand_test_suites` (default) every test suite among the
targets to build is replaced by its tests — so `//pkg:suite -//pkg:t` still
builds `t` but does not test it. TestExpansionFunction
(skyframe/TestExpansionFunction.java:58-136, strict, one value per suite): a
suite's `tests` (test rules, manual ones included; nested suites
recursively) and `$implicit_tests` (when `tests` is empty, the package's
non-manual tests matching the suite's tags, TestSuiteImplicitTestsAccumulator),
filtered by the suite's `tags` (TestTargetUtils.filterTests on tags + size;
a nested suite's expansion is not filtered again). A member that is neither
a test nor a test suite (a library, an alias, a file) is the error `in
test_suite rule 'L': expecting a test or a test_suite rule but 'M' is not
one.` at the suite: the command still builds (and tests) the rest, then
fails with exit 1 and BuildView's `command succeeded, but there were loading
phase errors` (`build`) or, after `WARNING: Target pattern parsing failed.`
and `All tests passed but there were other errors during the build.`,
`command succeeded, but there were errors parsing the target pattern`
(`test`). `--noexpand_test_suites` analyzes the suites themselves
(docs/NATIVE_BUILD_RULES.md, test_suite). `WARNING: All specified test
targets were excluded by filters`; no tests: `ERROR: No test targets were
found, yet testing was requested`, exit 4. Paired capture
`tests/fixtures/test_suite` (56 genuine 9.3.0 cases, all MATCH, 8 of them
`run`; replayed by `tests/test_suite.rs`).
`shard_count` is checked like RuleConfiguredTargetBuilder.initializeTestProvider
(`Must not be negative.`, more than 50 shards).

## Implicit test-runner dependencies

In the built-in profile (where `@bazel_tools` and the default `test` exec
group exist; D3 of docs/TEST_FLAGS.md) every Starlark test rule's
`$test_wrapper`, `$xml_writer`, `$test_runtime`, `$test_setup_script`,
`$xml_generator_script`, `$collect_coverage_script` and `:coverage_support`
(StarlarkRuleClassFunctions.java:290-352) are `exec(test)` dependency edges:
they are analyzed and their packages' repositories reach the test's
`_repo_mapping` (RuleContext.getTransitivePackagesForRunfileRepoMappingManifest;
capture M35 shows `bazel_tools,platforms,platforms`; tests/bazel_test.rs proves
a repository reached only through `$collect_coverage_script`).
`:coverage_report_generator` and the run-under attributes are null without
coverage/`--run_under` (CoverageConfiguration.java:98-103, BaseRuleClasses.java:155-197).

## The test action

One action per shard and run (TestActionBuilder.createTestAction,
analysis/test/TestActionBuilder.java:224-480), executed like
StandaloneTestStrategy (exec/StandaloneTestStrategy.java) in a fresh
execution root `<cache>/tests/work/<key>-<pid>/execroot`:

* inputs: the genuine `@bazel_tools` `test-setup.sh` and `generate-xml.sh`
  (bytes of the embedded bundle) at `external/bazel_tools/tools/test/...`, and
  the test's runfiles mapping (N23f, `_repo_mapping` included) staged **by
  content** at `<executable>.runfiles/<path>` (N36 read-only store hardlinks,
  empty files as empty files, no `MANIFEST`), like a sandboxed spawn. The
  action key frames those contents, the arguments, the environment, tags,
  the execution info, size/timeout, shard/run numbers and
  `--zip_undeclared_test_outputs` (TestRunnerAction.computeKey, :526-561) —
  never the runfiles tree node's link-only key, so a changed source file
  reruns the test.
* execution info (TestTargetProperties, analysis/test/TestTargetProperties.java:84-122):
  the rule's tags (TargetUtils.getExecutionInfo), `local` for local tests,
  `no-remote-exec` for exclusive ones, then the requirements of the rule's
  `testing.ExecutionInfo` put over them (`exec_group` other than `test`
  fails explicitly, docs/CC_COMMON_LOADING.md); with the tags it selects the
  spawn strategy and network policy (docs/SANDBOX.md).
* command: `[test-setup.sh exec path, executable runfiles path, args, --test_arg...]`
  (TestStrategy.expandedArgsFromAction, analysis/test/TestStrategy.java:176-205),
  stdout to `test.log`, stderr to `test.err` (appended to the log), stdin
  `/dev/null`, its own process group.
* environment (TestPolicy.computeTestEnvironment, exec/TestPolicy.java:56-101;
  DEFAULT_LOCAL_POLICY; TestRunnerAction.setupEnvVariables, :745-830): `TZ=UTC`,
  `TEST_SRCDIR`/`JAVA_RUNFILES`/`PYTHON_RUNFILES`/`RUNFILES_DIR` (relative
  runfiles directory), `TEST_TMPDIR=_tmp/<first 32 hex digits of the SHA-256
  Fingerprint of the executable exec path, shard and run>` (TestStrategy.getTmpDirName),
  `RUN_UNDER_RUNFILES=1`, the configuration's action environment (N42: strict
  `PATH` or inherited `PATH`/`LD_LIBRARY_PATH`, `--action_env`), its `--test_env`
  environment (a configuration option, docs/ACTION_ENV.md; fixed values, then
  inherited names the client sets; both keyed into the test action),
  RunEnvironmentInfo `environment`/`inherited_environment`, `TEST_TARGET`,
  `TEST_SIZE`, `TEST_TIMEOUT`, `TEST_WORKSPACE=_main`, `TEST_BINARY`,
  `TEST_RANDOM_SEED`/`TEST_RUN_NUMBER` (several runs), `TESTBRIDGE_TEST_ONLY`,
  `TESTBRIDGE_TEST_RUNNER_FAIL_FAST`, the testlogs file variables,
  `TEST_SHARD_INDEX`/`TEST_TOTAL_SHARDS`/`TEST_SHARD_STATUS_FILE` (more than one
  shard), `XML_OUTPUT_FILE`, `RUNFILES_MANIFEST_ONLY` (`--enable_runfiles=no`),
  and `TMPDIR` from the client or `/tmp` (PosixLocalEnvProvider). test-setup.sh
  makes them absolute and sets `HOME`, `USER`, `BAZEL_TEST`, `GTEST_*`.
* timeout (`size`/`timeout`, `--test_timeout`): SIGTERM to the process group,
  SIGKILL after 15 s (`--local_termination_grace_seconds`), status TIMEOUT,
  exit code 142 in test.xml.
* after the process: `TEST_PREMATURE_EXIT_FILE` turns a pass into a failure
  (`-- Test exited prematurely (TEST_PREMATURE_EXIT_FILE exists) --`); a
  passing sharded test that did not touch `TEST_SHARD_STATUS_FILE` is an action
  error (`Testing //p:t (shard 1 of 2) failed: Sharding requested, ...`): NO
  STATUS (exit 3) and, under `--nokeep_going`, test actions not started yet
  are skipped; under `--keep_going` the target FAILED TO BUILD (exit 1);
  test.xml is written by `generate-xml.sh LOG XML SECONDS EXIT` (with
  `TEST_NAME`) when the test wrote none.
* attempts (TestRunnerAction.executeAllAttempts, :1246-1300): failed attempts
  move to `test_attempts/attempt_N.{log,xml,...}` (StandaloneTestStrategy.renameOutputs);
  a pass after failures is FLAKY.
* publication: `<cache>/tests/<key>/bazel-out/<cfg>/testlogs/<pkg>/<name>[/shard_K_of_N][_run_R_of_M]/`
  with `test.log`, `test.xml`, `test.outputs/`, `test.outputs_manifest/`,
  `test.raw_splitlogs/` (`test.log`, `test.xml` and `test.outputs` 0555; with
  failed attempts the final `test.log` stays 0644 and attempt logs are 0555,
  as observed) and the native `status.json` (TestResultData: status,
  cachable, test times, logs, parsed test case). Like the action cache only
  the last key of each test action output is reusable (a run with another
  `--test_timeout` reruns the next default run).
* a cached result is used as stored, like TestRunnerAction.actionCacheHit
  (analysis/test/TestRunnerAction.java:686-700): its test case exists only if
  the run that produced it parsed test.xml (a detailed format), so a later
  `--test_summary=detailed` counts a result cached under another format as
  one unknown case (`(some targets did not have test case information)`);
  the stored test.xml is not re-parsed (genuine Bazel behaves the same).
* **local only:** native test actions are never looked up in or uploaded to
  a remote or disk cache (Bazel's `shouldAcceptCachedResult` path for
  `--remote_cache`/`--disk_cache`); the native test cache is the local
  `<cache>/tests` store until a REAPI encoding of test outputs (testlogs and
  the result status) exists.
* exclusive tests (and `--test_strategy=exclusive`, streamed output) run after
  the others, one at a time; others run with `--local_test_jobs` or `--jobs`.

## Output

stdout: the test output blocks (`==================== Test output for NAME:`,
the log after test-setup.sh's header line, 80 `=`), the cached-output block
of locally cached results (`PASSED: L (see LOGS)` / `INFO: From Testing L`),
then the summary: lines `padEnd(label, 78 - len) + " " + [(cached) ]STATUS`
with attempt and time summaries (`FLAKY, failed in 1 out of 2 in 0.0s`,
`Stats over N runs: ...`), failed log paths, test cases for detailed
formats (`    ERROR   .pkg/fail_test.sh (0.0s)`, `Test cases: finished with
...`), `Executed X out of Y tests: ...` (Oxford-comma English list) and the
size note. Summaries are ordered by TestSummary.compareTo. stderr: `FAIL:
NAME (Exit 1) (see LOG)` per failed attempt (`Segmentation fault` etc. by
TerminationStatus), `INFO: From Testing NAME:`, `STATUS: L (Summary)` when
UiEventHandler shows it, action errors, the built targets as `label<TAB>path`
and the native action counts. Exit codes: 0 all passed, 3 tests failed or
skipped, 4 no tests, 1 build failure (summary: FAILED TO BUILD for the tests
the failing actions name, NO STATUS for the rest).

## `run` of a test target

RunCommand.getTestCommandLine (runtime/commands/RunCommand.java:754-850): one
shard and run only (`Run command does not support tests with more than one
shard or run`); the command is `test-setup.sh EXE ARGS... RESIDUE` in an
execution root `<cache>/test-run/<key>/execroot` holding test-setup.sh and a
link `<executable>.runfiles` to the built runfiles tree; the environment is
`BUILD_*` plus the test environment (no `--run_env`); `BUILD_EXECROOT` makes
test-setup.sh exec the test directly. `--script_path` writes the same command.

## Paired capture

`artifacts/bazel_test-capture.py` runs the fixture of `tests/bazel_test.rs`
(with tests/fixtures/test_flags/stubs) with genuine Bazel 9.3.0 (`--batch`,
offline, default sandboxed strategy) and natively; the receipt is
`tests/fixtures/bazel_test/reference_receipt.json` (run 2, the one allowed
rerun): 37 EXACT, 4 EXACT_MESSAGE, 1 MISMATCH (fixed after the capture and
verified against the recorded reference row), 1 UNAVAILABLE, 1
EXPECTED_NATIVE_UNSUPPORTED. Exact: normalized env and names, test.log and
test.xml bytes and modes, summaries of every format, caching (`(cached)`,
auto reruns of failures, `yes`, external tests, several runs, last key),
sharding, the sharding-support error, timeout (142), flaky and failing
attempts, signals, premature exit, undeclared outputs and annotations,
streamed output, filters, suites, manual tests, `--build_tests_only`, exit
codes 0/1/3/4, the build-failure summary, rc `test` inheriting `build`,
`_repo_mapping` with `@bazel_tools` (implicit dependencies) and `run` of a
test target. EXACT_MESSAGE: option errors (Bazel exits 2 with `ERROR:`, the
native CLI exits 1 with `bazel-rs:`). UNAVAILABLE: a genuine rules_shell
`sh_test` (the fixture registry lacks `bazel_skylib@1.6.1`; natively genuine
rules_shell rules then hit analysis gaps that N50 closed: on integration-15 the
genuine `sh_test` of N50's receipt passes under `bazel test` with Bazel's exact
output, docs/SH_BINARY.md). Normalized: execution
roots (Bazel sandbox vs native work directories), testlogs roots, the
configuration mnemonic (`k8-fastbuild` vs the then-native `rust-fastbuild`, D1), `_tmp`
fingerprints (they hash that exec path), durations and dates. Since N41 closed D1
the native roots are `k8-fastbuild` and the `_tmp` fingerprint is Bazel's for the same
exec path (`tmp_dir_name_matches_bazel_fingerprint`); the integration-15 native replay
against the logged reference reproduces every recorded verdict (M36: EXACT).

## Not covered

Coverage; `--run_under` for tests and labels; remote caching/execution of test
actions (local-only, see above); `--test_strategy` values beyond
`standalone`/`local`/`exclusive` and worker test strategies (test actions
themselves run through the spawn strategy selection and are sandboxed by
default, docs/SANDBOX.md; runfiles are hardlinks where
Bazel's sandbox shows symlinks); `--keep_going` with build failures; Bazel's
convenience symlinks (`bazel-testlogs`) and relative log paths; the
test.cache_status proto (native `status.json`); `test.err` splitting beyond
appending; BEP/test result events; Windows (`tw.exe`, `xml.exe`).
