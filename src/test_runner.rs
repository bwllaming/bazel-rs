//! Local test execution (N23g, docs/TEST_COMMAND.md): TestRunnerAction run
//! by StandaloneTestStrategy (exec/StandaloneTestStrategy.java) and
//! TestStrategy (analysis/test/TestStrategy.java).
//!
//! Every test action (one per shard and run) gets a fresh execution root
//! holding the genuine `@bazel_tools//tools/test:test-setup.sh` and
//! `generate-xml.sh` at their exec paths and the test's runfiles staged by
//! content at `<executable>.runfiles/<path>` (N36 store hardlinks, like a
//! sandboxed SpawnInputExpander and N23f tool runfiles; no `MANIFEST`).
//! The action key frames those contents (never the runfiles tree node's
//! link-only key), the test environment, arguments and properties
//! (TestRunnerAction.computeKey, analysis/test/TestRunnerAction.java:526-561).
//! Results are published below `<cache>/tests/<key>/` with the testlogs
//! layout `bazel-out/<cfg>/testlogs/<pkg>/<name>/...` and a native status
//! file; `--cache_test_results` decides when a previous result is reused
//! (TestRunnerAction.executeUnconditionally, :565-601).
//!
//! Test actions are local-only: results are never looked up in or uploaded
//! to a remote/disk cache (Bazel's shouldAcceptCachedResult path) until a
//! REAPI encoding of test outputs (testlogs, status) exists.
//!
//! Paths below are under src/main/java/com/google/devtools/build/lib/ of the
//! pinned Bazel 9.3 sources.

use crate::action::{
    ContentInput, ContentSource, read_content_input, read_content_source, read_symlink_input,
    stage_content_inputs,
};
use crate::test_command::{
    EnvInputs, RunResult, TestCase, TestOptions, TestOutput, TestRuleInfo, TestStatus, TestSummary,
    TestSummaryFormat, TriState, filter_header, should_output_log, termination_short,
    test_directory, test_environment, test_log_block, test_name, tmp_dir_name,
};
use fs2::FileExt;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::fs;
use std::io::{Read, Write};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// The exec paths of the test runner scripts in `@bazel_tools`.
pub const SETUP_EXEC_PATH: &str = "external/bazel_tools/tools/test/test-setup.sh";
pub const XML_EXEC_PATH: &str = "external/bazel_tools/tools/test/generate-xml.sh";
/// The runfiles prefix of the main repository (TestRunnerAction.getRunfilesPrefix).
pub const WORKSPACE: &str = "_main";
/// `--local_termination_grace_seconds` (default 15): SIGTERM, then SIGKILL.
const TERMINATION_GRACE: Duration = Duration::from_secs(15);
/// The exit code a timed-out spawn reports (128 + SIGALRM, as Bazel's
/// local runner does; genuine test.xml shows `exited with error code 142`).
const TIMEOUT_EXIT_CODE: i32 = 142;
const STATUS_FORMAT: &str = "native-test-v1";

/// One test target to run.
pub struct TestSpec {
    /// Label.print of the target (TEST_TARGET, summaries).
    pub label: String,
    pub info: TestRuleInfo,
    /// The target's location (`BUILD:line:col`), for action errors.
    pub location: Option<String>,
    pub executable: PathBuf,
    /// The executable's exec path (`bazel-out/<cfg>/bin/...`).
    pub executable_exec: String,
    /// The executable's runfiles path relative to `_main` (TEST_BINARY).
    pub binary: String,
    /// The runfiles mapping with published paths (`None`: an empty file).
    pub entries: Vec<(String, Option<PathBuf>)>,
    /// The runfiles paths of `entries` that are unresolved symlink
    /// artifacts, staged as symlinks with their link text.
    pub symlinks: std::collections::BTreeSet<String>,
    /// `bazel-out/<cfg>/testlogs/<pkg>`.
    pub testlogs_package: String,
    pub args: Vec<String>,
    pub target_env: Vec<(String, String)>,
    pub target_inherited: Vec<String>,
    /// The target configuration's action and test environments (N42).
    pub action_env: crate::action_env::ActionEnvironment,
    pub test_env: crate::action_env::ActionEnvironment,
    /// The requirements of the test rule's testing.ExecutionInfo
    /// (TestTargetProperties.java:115-118), empty without one.
    pub requirements: Vec<(String, String)>,
    /// `--incompatible_exclusive_test_sandboxed` (default true): when false
    /// an exclusive test's execution info gets `local`
    /// (TestTargetProperties.java:91-108).
    pub exclusive_sandboxed: bool,
    /// The `--run_under` target's executable by its runfiles path
    /// (LabelRunUnder; the target's runfiles hold it).
    pub run_under_executable: Option<String>,
    /// A coverage test action's additions (docs/COVERAGE.md).
    pub coverage: Option<crate::request::RunCoverage>,
}

impl TestSpec {
    /// The test spec of a built test target (TestActionBuilder.createTestAction,
    /// analysis/test/TestActionBuilder.java:224-480).
    pub fn new(
        label: &crate::label::Label,
        info: TestRuleInfo,
        location: Option<String>,
        target: &crate::engine::RunTarget,
    ) -> Result<Self, String> {
        let utf8 = |path: &Path| {
            path.to_str()
                .map(str::to_owned)
                .ok_or_else(|| format!("non-UTF-8 path {} is not supported", path.display()))
        };
        let Some(entries) = target.entries.clone() else {
            return Err(format!(
                "{}: test target has no runfiles support",
                label_print(label)
            ));
        };
        let package = if label.repository.is_empty() {
            label.package.clone()
        } else if label.package.is_empty() {
            format!("external/{}", label.repository)
        } else {
            format!("external/{}/{}", label.repository, label.package)
        };
        let testlogs_package = if package.is_empty() {
            format!("bazel-out/{}/testlogs", target.mnemonic)
        } else {
            format!("bazel-out/{}/testlogs/{package}", target.mnemonic)
        };
        // The test rule's testing.ExecutionInfo (TestActionBuilder
        // .setExecutionRequirements): its requirements join the test action's
        // execution info (TestTargetProperties.java:115-118). Another exec group
        // makes that group's action owner run the test
        // (TestActionBuilder.getTestActionOwner :156-170: its execution platform,
        // exec properties and unrunnable check), not modelled.
        let requirements = match &target.settings.test_requirements {
            None => Vec::new(),
            Some(info) if info.default_exec_group() => info.requirements.clone(),
            Some(info) => {
                return Err(format!(
                    "{}: testing.ExecutionInfo with exec_group = \"{}\" is not implemented in the native profile: the test action would run in that exec group (TestActionBuilder.getTestActionOwner); only the default \"test\" group is modelled",
                    label_print(label),
                    info.exec_group
                ));
            }
        };
        Ok(TestSpec {
            label: label_print(label),
            info,
            location,
            executable: target.executable.clone(),
            executable_exec: utf8(&target.executable_exec)?,
            binary: target.executable_short_path.clone(),
            entries,
            symlinks: target.symlinks.clone(),
            testlogs_package,
            args: target.settings.args.clone(),
            target_env: target.settings.environment.clone(),
            target_inherited: target.settings.inherited.clone(),
            action_env: target.action_env.clone(),
            test_env: target.test_env.clone(),
            requirements,
            exclusive_sandboxed: true,
            run_under_executable: target.settings.run_under_executable.clone(),
            coverage: target.coverage.clone(),
        })
    }

    /// TestTargetProperties.getExecutionInfo (analysis/test/
    /// TestTargetProperties.java:84-122): the rule's execution info
    /// (crate::test_command::TestRuleInfo::execution_info) with the
    /// ExecutionInfo requirements put over it.
    pub fn execution_info(&self) -> BTreeMap<String, String> {
        let mut info = self.info.execution_info();
        if !self.exclusive_sandboxed && self.info.exclusive() {
            info.remove("no-remote-exec");
            info.insert("local".to_owned(), String::new());
        }
        info.extend(self.requirements.iter().cloned());
        info
    }
}

/// The test command line (TestStrategy.expandedArgsFromAction,
/// analysis/test/TestStrategy.java:205-238): test-setup.sh, the `--run_under`
/// prefix (TestStrategy.addRunUnderArgs), the executable's runfiles path, its
/// `args` and `--test_arg`s.
pub fn test_arguments(spec: &TestSpec, options: &TestOptions) -> Vec<String> {
    let mut args = vec![SETUP_EXEC_PATH.to_owned()];
    // A coverage test action runs collect_coverage.sh first (:219-226).
    if let Some(coverage) = &spec.coverage {
        args.push(coverage.script.clone());
    }
    if let Some(run_under) = &options.run_under {
        args.extend(run_under.test_prefix(spec.run_under_executable.as_deref(), &options.shell));
    }
    args.push(spec.binary.clone());
    args.extend(spec.args.iter().cloned());
    args.extend(options.args.iter().cloned());
    args
}

/// A LabelRunUnder needs an executable (TestTargetExecutionSettings
/// .getRunUnderExecutable: the target's FilesToRunProvider executable).
pub fn run_under_ready(spec: TestSpec, options: &TestOptions) -> Result<TestSpec, String> {
    match &options.run_under {
        Some(run_under) if run_under.is_label() && spec.run_under_executable.is_none() => {
            Err(format!(
                "{}: the --run_under target {} has no executable",
                spec.label, run_under.first
            ))
        }
        _ => Ok(spec),
    }
}

/// TestRunnerAction.getCoverageDirectory (:963-985): the
/// `--experimental_fetch_all_coverage_outputs` tree `<base>/_coverage`, else
/// `<testlogs>/_coverage/<pkg>/<dir>/test` (the test log's root-relative
/// path without its extension).
fn coverage_directory(spec: &TestSpec, paths: &Paths) -> String {
    if crate::coverage::settings().fetch_all_outputs {
        return format!("{}/_coverage", paths.base);
    }
    let root = spec
        .testlogs_package
        .split('/')
        .take(3)
        .collect::<Vec<_>>()
        .join("/");
    let relative = paths
        .base
        .strip_prefix(&format!("{root}/"))
        .unwrap_or(&paths.base);
    format!("{root}/_coverage/{relative}/test")
}

/// Label.print: `//pkg:name` or `@@repo//pkg:name`.
pub fn label_print(label: &crate::label::Label) -> String {
    if label.repository.is_empty() {
        format!("//{}:{}", label.package, label.name)
    } else {
        format!("@@{}//{}:{}", label.repository, label.package, label.name)
    }
}

/// The genuine test runner scripts of the embedded `@bazel_tools`.
pub struct Tools {
    pub setup: PathBuf,
    pub xml: PathBuf,
}

impl Tools {
    pub fn embedded(cache: &Path) -> Result<Self, String> {
        let root = crate::builtin_tools::materialize(&cache.join("embedded_tools"))?;
        Ok(Self {
            setup: root.join("tools/test/test-setup.sh"),
            xml: root.join("tools/test/generate-xml.sh"),
        })
    }
}

pub struct Runner<'a> {
    pub cache: &'a Path,
    pub workspace: &'a Path,
    pub options: &'a TestOptions,
    pub client_env: &'a BTreeMap<String, String>,
    /// RunfilesSupport: whether runfiles symlinks exist (`--enable_runfiles`).
    pub runfiles_enabled: bool,
    pub tools: &'a Tools,
}

/// The results of a `test` invocation.
pub struct Outcome {
    /// One per test, in the session's (planned) order.
    pub summaries: Vec<TestSummary>,
    /// Per test: whether every test action of it reported a result (its
    /// summary was posted, TestResultAggregator.summaryPosted). A failed
    /// build reports only these as they are; the others never ran or were
    /// interrupted.
    pub completed: Vec<bool>,
    /// Targets with at least one test action that ran (not cached).
    pub executed: usize,
    /// Action errors (`Testing X failed: ...`), reported again at the end.
    pub errors: Vec<String>,
    /// The first infrastructure failure of a test action (the command fails).
    pub failure: Option<String>,
    /// `--notest_keep_going`: a test that did not pass aborted its action
    /// ([`TEST_FAILED_ABORTING`]).
    pub aborted: bool,
    /// The coverage.dat exec paths of the coverage test actions that
    /// reported a result (TestParams.getCoverageArtifacts), in test order.
    pub coverage: Vec<String>,
    /// The top-level targets' baseline coverage files (exec path,
    /// published path).
    pub baselines: Vec<(String, PathBuf)>,
}

/// The failure of a test action that did not pass under
/// `--notest_keep_going` (TestRunnerAction.executeAllAttempts,
/// analysis/test/TestRunnerAction.java:1293-1310: TestExecException
/// NO_KEEP_GOING_TEST_FAILURE, after the test's result was posted).
pub const TEST_FAILED_ABORTING: &str = "Test failed, aborting";

/// The persisted result of a test action (Bazel's test.cache_status holds
/// TestResultData; this is the native equivalent, `status.json`).
#[derive(Serialize, Deserialize, Clone)]
#[serde(deny_unknown_fields)]
struct Status {
    format: String,
    key: String,
    status: String,
    cachable: bool,
    test_times: Vec<u64>,
    /// Logs relative to the published entry.
    passed_log: Option<String>,
    failed_logs: Vec<String>,
    test_case: Option<TestCase>,
}

fn status_from(name: &str) -> TestStatus {
    match name {
        "PASSED" => TestStatus::Passed,
        "FLAKY" => TestStatus::Flaky,
        "TIMEOUT" => TestStatus::Timeout,
        "FAILED" => TestStatus::Failed,
        "INCOMPLETE" => TestStatus::Incomplete,
        _ => TestStatus::NoStatus,
    }
}

/// One test action of a target.
struct Unit<'a> {
    spec: &'a TestSpec,
    shard: u32,
    shards: u32,
    run: u32,
    runs: u32,
}

/// An action outcome: a result, or an action error (no result: NO STATUS).
enum UnitOutcome {
    Result(RunResult),
    Error(String),
}

/// One test action of a session (TestActionBuilder: one per shard and run).
struct UnitKey {
    test: usize,
    shard: u32,
    shards: u32,
    run: u32,
    runs: u32,
}

/// The test actions of one `bazel test` (PERF(TEST)), scheduled the way
/// Bazel 9.3 schedules TestRunnerActions: the parallel ones are leaves of
/// the execution graph (crate::scheduler::Leaves; crate::request runs each
/// as soon as its test's inputs are built,
/// skyframe/BuildDriverFunction.java:624-633), the exclusive ones run one at
/// a time after the graph succeeded (ExclusiveTestBuildDriverValue,
/// SkyframeBuildView.analyzeAndExecuteTargets :759-794). Results aggregate
/// per test as they arrive (AggregatingTestListener); [`Session::finish`]
/// summarizes them for the summary printed after the build.
pub struct Session<'a> {
    runner: Runner<'a>,
    tests: Vec<crate::request::PlannedTest>,
    units: Vec<UnitKey>,
    /// The leaves of the execution graph (unit indices).
    parallel: Vec<usize>,
    /// The test actions run after the graph, in this order.
    exclusive: Vec<usize>,
    /// Test actions per test.
    expected: Vec<usize>,
    /// Each test's spec, made when its first test action runs.
    specs: Vec<std::sync::OnceLock<Result<TestSpec, String>>>,
    results: Mutex<Vec<Vec<UnitOutcome>>>,
    summaries: Mutex<Vec<Option<TestSummary>>>,
    printer: Mutex<()>,
    failure: Mutex<Option<String>>,
    /// --nokeep_going: a test action error (TestExecException) stops the
    /// test actions not started yet (no result: NO STATUS).
    abort: std::sync::atomic::AtomicBool,
    /// `--notest_keep_going`: a test that did not pass aborted its action.
    aborted: std::sync::atomic::AtomicBool,
}

impl<'a> Session<'a> {
    /// The test actions of `tests` (in order; the planned tests of the
    /// request): one per shard and run, exclusive ones apart.
    pub fn new(runner: Runner<'a>, tests: Vec<crate::request::PlannedTest>) -> Self {
        let options = runner.options;
        let streamed = options.output == TestOutput::Streamed;
        let mut units = Vec::new();
        let mut parallel = Vec::new();
        let mut exclusive = Vec::new();
        let mut expected = Vec::new();
        for (test, planned) in tests.iter().enumerate() {
            // TestCommand: streamed output disables sharding (TestCommand.java:84-92).
            let shards = if streamed {
                0
            } else {
                options.sharding.shards(planned.info.shard_count).max(0) as u32
            };
            let runs = options.runs(&label_print(&planned.label));
            // Exclusive tests run after the others, one at a time
            // (SkyframeBuildView.determineTestTypeImpl :1115-1156: `exclusive`,
            // `exclusive-if-local` and --test_strategy=exclusive, which
            // streamed output implies; no local strategy runs them in
            // parallel, TestActionContext.forceExclusive*InParallel).
            let alone = streamed || options.exclusive || planned.info.runs_exclusively();
            let before = units.len();
            for shard in 0..shards.max(1) {
                for run in 0..runs {
                    if alone {
                        exclusive.push(units.len());
                    } else {
                        parallel.push(units.len());
                    }
                    units.push(UnitKey {
                        test,
                        shard,
                        shards,
                        run,
                        runs,
                    });
                }
            }
            expected.push(units.len() - before);
        }
        crate::progress::tests_planned(tests.len());
        Session {
            specs: tests.iter().map(|_| std::sync::OnceLock::new()).collect(),
            results: Mutex::new(tests.iter().map(|_| Vec::new()).collect()),
            summaries: Mutex::new(vec![None; tests.len()]),
            runner,
            tests,
            units,
            parallel,
            exclusive,
            expected,
            printer: Mutex::new(()),
            failure: Mutex::new(None),
            abort: std::sync::atomic::AtomicBool::new(false),
            aborted: std::sync::atomic::AtomicBool::new(false),
        }
    }

    /// The label of test `test`.
    pub fn test_label(&self, test: usize) -> &crate::label::Label {
        &self.tests[test].label
    }

    /// The number of tests.
    pub fn test_count(&self) -> usize {
        self.tests.len()
    }

    /// The leaves of the execution graph: the test of each parallel test
    /// action, in dispatch-priority order.
    pub fn leaf_tests(&self) -> Vec<usize> {
        self.parallel
            .iter()
            .map(|&unit| self.units[unit].test)
            .collect()
    }

    /// At most this many test actions run at once (`--local_test_jobs`; 0:
    /// only `--jobs` bounds them, ExecutionTool.configureResourceManager).
    pub fn leaf_cap(&self) -> usize {
        self.runner.options.local_test_jobs
    }

    /// Run leaf `leaf` (the `leaf`-th parallel test action) once its test's
    /// inputs are built; `target` resolves the test's built target the
    /// first time one of its actions runs. Nothing runs once `interrupt` is
    /// set (the build failed) or a test action error stopped the tests.
    pub fn run_leaf(
        &self,
        leaf: usize,
        interrupt: &std::sync::atomic::AtomicBool,
        target: impl FnOnce() -> Result<crate::engine::RunTarget, String>,
    ) {
        self.run(self.parallel[leaf], interrupt, target);
    }

    /// Run the exclusive test actions one at a time, in order (after the
    /// execution graph succeeded); `target` resolves a test's built target.
    pub fn run_exclusive(
        &self,
        target: impl Fn(usize) -> Result<crate::engine::RunTarget, String>,
    ) {
        self.run_exclusive_built(|_| true, target);
    }

    /// [`Self::run_exclusive`] of the tests `built` accepts (`--keep_going`:
    /// a test whose inputs failed to build never runs).
    pub fn run_exclusive_built(
        &self,
        built: impl Fn(usize) -> bool,
        target: impl Fn(usize) -> Result<crate::engine::RunTarget, String>,
    ) {
        let never = std::sync::atomic::AtomicBool::new(false);
        for &unit in &self.exclusive {
            let test = self.units[unit].test;
            if built(test) {
                self.run(unit, &never, || target(test));
            }
        }
    }

    fn run(
        &self,
        unit: usize,
        interrupt: &std::sync::atomic::AtomicBool,
        target: impl FnOnce() -> Result<crate::engine::RunTarget, String>,
    ) {
        use std::sync::atomic::Ordering;
        if self.abort.load(Ordering::SeqCst) || interrupt.load(Ordering::Acquire) {
            return;
        }
        let key = &self.units[unit];
        let planned = &self.tests[key.test];
        let spec = self.specs[key.test].get_or_init(|| {
            target().and_then(|target| {
                TestSpec::new(
                    &planned.label,
                    planned.info.clone(),
                    planned.location.clone(),
                    &target,
                )
                .and_then(|spec| run_under_ready(spec, self.runner.options))
                .map(|spec| TestSpec {
                    exclusive_sandboxed: self.runner.options.exclusive_test_sandboxed,
                    ..spec
                })
            })
        });
        let spec = match spec {
            Ok(spec) => spec,
            Err(error) => {
                self.fail(error.clone());
                return;
            }
        };
        let action = Unit {
            spec,
            shard: key.shard,
            shards: key.shards,
            run: key.run,
            runs: key.runs,
        };
        let traced = (crate::bep::active() || crate::profile::active())
            .then(|| (std::time::Instant::now(), std::time::SystemTime::now()));
        let outcome = self.runner.run_unit(&action, &self.printer, interrupt);
        if let (Some((started, start_time)), Ok(Some(UnitOutcome::Result(result)))) =
            (traced, &outcome)
        {
            let label = label_print(&planned.label);
            crate::profile::span(
                "action processing",
                format!("Testing {label}"),
                started,
                vec![("mnemonic", "TestRunner".to_owned())],
            );
            let log = result
                .passed_log
                .clone()
                .or_else(|| result.failed_logs.first().cloned());
            let xml = log
                .as_ref()
                .map(|log| log.with_file_name("test.xml"))
                .filter(|xml| xml.exists());
            crate::bep::test_result(
                &label,
                key.run,
                key.shard,
                result,
                start_time,
                started.elapsed(),
                log,
                xml,
                // TestResult.execution_info.strategy: the resolved one
                // (a remote spawn's "remote").
                if matches!(
                    self.runner.strategy(&action),
                    Ok((crate::sandbox::Strategy::Remote, _))
                ) {
                    "remote"
                } else if crate::sandbox::all_sandboxed() {
                    "linux-sandbox"
                } else {
                    "local"
                },
            );
        }
        match outcome {
            Ok(Some(outcome)) => {
                // --notest_keep_going: an executed attempt that did not pass
                // throws once its result is posted (a cached result runs no
                // attempt; a flaky pass is a pass).
                let aborting = !self.runner.options.test_keep_going
                    && matches!(&outcome, UnitOutcome::Result(result)
                        if !result.cached && !result.status.passed());
                self.record(key.test, outcome);
                if aborting {
                    self.abort_on_failure(&action, interrupt);
                }
            }
            // Interrupted: the build failed while it ran (no result).
            Ok(None) => {}
            Err(error) => self.fail(error),
        }
    }

    /// `--notest_keep_going`: the test action fails with
    /// [`TEST_FAILED_ABORTING`] (`ERROR: LOCATION: Testing L failed: Test
    /// failed, aborting`). Under `--nokeep_going` that is the build's first
    /// failure: no test or build action starts after it and the test
    /// actions in flight are interrupted (`interrupt` is the execution
    /// graph's failure flag; NO STATUS), like any action failure; under
    /// `--keep_going` everything else goes on.
    fn abort_on_failure(&self, unit: &Unit<'_>, interrupt: &std::sync::atomic::AtomicBool) {
        use std::sync::atomic::Ordering;
        let spec = unit.spec;
        let test_name = test_name(&spec.label, unit.shard, unit.shards, unit.run, unit.runs);
        let location = spec
            .location
            .as_deref()
            .map(|location| format!("{location}: "))
            .unwrap_or_default();
        {
            let _guard = self.printer.lock().unwrap_or_else(|e| e.into_inner());
            eprintln!("ERROR: {location}Testing {test_name} failed: {TEST_FAILED_ABORTING}");
        }
        self.aborted.store(true, Ordering::SeqCst);
        if !self.runner.options.keep_going {
            self.abort.store(true, Ordering::SeqCst);
            interrupt.store(true, Ordering::Release);
        }
    }

    /// Whether a test that did not pass stopped the build
    /// (`--notest_keep_going` under `--nokeep_going`).
    pub fn stopped_the_build(&self) -> bool {
        !self.runner.options.keep_going && self.aborted.load(std::sync::atomic::Ordering::SeqCst)
    }

    /// An infrastructure failure: the command fails (the first one is
    /// reported); the other test actions go on.
    fn fail(&self, error: String) {
        self.failure
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get_or_insert(error);
    }

    /// TestResultAggregator: the summary of a test's outcomes so far;
    /// `expected` actions in all.
    fn aggregate(&self, test: usize, outcomes: &[UnitOutcome]) -> TestSummary {
        let options = self.runner.options;
        let planned = &self.tests[test];
        let collected: Vec<RunResult> = outcomes
            .iter()
            .filter_map(|outcome| match outcome {
                UnitOutcome::Result(result) => Some(result.clone()),
                UnitOutcome::Error(_) => None,
            })
            .collect();
        let incomplete = collected.len() != self.expected[test];
        let mut summary = TestSummary::aggregate(
            &label_print(&planned.label),
            planned.info.timeout,
            &collected,
            options.verbose_timeout_warnings,
        );
        // TestResultAggregator.markIncomplete (runtime/TestResultAggregator.java
        // :270-282): a test with an action without a result is NO_STATUS when
        // the build stops on the first failure (--nokeep_going:
        // skipTargetsOnFailure, BuildTool.java:1128); otherwise NO_STATUS
        // stays, any other status becomes INCOMPLETE.
        if incomplete && summary.status != TestStatus::NoStatus {
            summary.status = if options.keep_going {
                summary.status.max(TestStatus::Incomplete)
            } else {
                TestStatus::NoStatus
            };
        }
        // Under --keep_going a failed test action (TestExecException) fails
        // its target like a build error (targetFailure: FAILED_TO_BUILD; its
        // actions ran; genuine capture M36).
        if options.keep_going
            && outcomes
                .iter()
                .any(|outcome| matches!(outcome, UnitOutcome::Error(_)))
        {
            summary.status = TestStatus::FailedToBuild;
            summary.action_ran = true;
        }
        summary
    }

    /// Record one test action's outcome; the last one posts the test's
    /// summary (UiEventHandler.testSummary prints it when it says more than
    /// the summary line will).
    fn record(&self, test: usize, outcome: UnitOutcome) {
        use std::sync::atomic::Ordering;
        // --nokeep_going (default): a failing test action (TestExecException)
        // stops the build's tests; test actions not started yet get no
        // result (NO STATUS, "skipped").
        if matches!(outcome, UnitOutcome::Error(_)) && !self.runner.options.keep_going {
            self.abort.store(true, Ordering::SeqCst);
        }
        let mut results = self.results.lock().unwrap_or_else(|e| e.into_inner());
        results[test].push(outcome);
        if results[test].len() == self.expected[test] {
            let summary = self.aggregate(test, &results[test]);
            crate::progress::test_finished(summary.status == TestStatus::Passed);
            // UiEventHandler.testSummary (runtime/UiEventHandler.java:938-966).
            let new_information = !matches!(
                summary.status,
                TestStatus::Passed | TestStatus::FailedToBuild | TestStatus::NoStatus
            ) && (summary.status != TestStatus::Failed
                || summary.failed_logs.len() != 1);
            if new_information {
                let _guard = self.printer.lock().unwrap_or_else(|e| e.into_inner());
                let mut text =
                    format!("\n{}: {} (Summary)\n", summary.status.name(), summary.label);
                for log in &summary.failed_logs {
                    text.push_str(&format!("      {}\n", log.display()));
                }
                eprint!("{text}");
            }
            self.summaries.lock().unwrap_or_else(|e| e.into_inner())[test] = Some(summary);
        }
    }

    /// The tests' summaries (AggregatingTestListener.differentialAnalyzeAndReport):
    /// a test whose actions did not all report is summarized as it stands
    /// (TestResultAggregator.aggregateAndReportSummary).
    pub fn finish(self) -> Outcome {
        let results = std::mem::take(&mut *self.results.lock().unwrap_or_else(|e| e.into_inner()));
        let posted = std::mem::take(&mut *self.summaries.lock().unwrap_or_else(|e| e.into_inner()));
        let completed: Vec<bool> = posted.iter().map(Option::is_some).collect();
        let summaries: Vec<TestSummary> = posted
            .into_iter()
            .enumerate()
            .map(|(test, summary)| summary.unwrap_or_else(|| self.aggregate(test, &results[test])))
            .collect();
        let mut coverage = Vec::new();
        let mut baselines = Vec::new();
        for key in &self.units {
            let Some(Ok(spec)) = self.specs[key.test].get() else {
                continue;
            };
            let Some(spec_coverage) = &spec.coverage else {
                continue;
            };
            if !results[key.test]
                .iter()
                .any(|outcome| matches!(outcome, UnitOutcome::Result(_)))
            {
                continue;
            }
            if baselines.is_empty() {
                baselines = spec_coverage.baselines.clone();
            }
            let unit = Unit {
                spec,
                shard: key.shard,
                shards: key.shards,
                run: key.run,
                runs: key.runs,
            };
            coverage.push(format!("{}/coverage.dat", self.runner.paths(&unit).base));
        }
        let errors = results
            .into_iter()
            .flatten()
            .filter_map(|outcome| match outcome {
                UnitOutcome::Error(error) => Some(error),
                UnitOutcome::Result(_) => None,
            })
            .collect();
        let executed = summaries
            .iter()
            .filter(|summary| summary.action_ran)
            .count();
        Outcome {
            coverage,
            baselines,
            summaries,
            completed,
            executed,
            errors,
            failure: self.failure.into_inner().unwrap_or_else(|e| e.into_inner()),
            aborted: self.aborted.into_inner(),
        }
    }
}

fn io(context: &str, path: &Path, error: std::io::Error) -> String {
    format!("{context} '{}': {error}", path.display())
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn frame(hash: &mut Sha256, bytes: &[u8]) {
    hash.update((bytes.len() as u64).to_le_bytes());
    hash.update(bytes);
}

/// Record `key` as the last key of a test action output in this
/// workspace's index (a temporary file renamed into place).
fn write_index(index_path: &Path, key: &str) -> Result<(), String> {
    if let Some(parent) = index_path.parent() {
        fs::create_dir_all(parent).map_err(|e| io("cannot create", parent, e))?;
    }
    let temporary = index_path.with_extension(format!("tmp-{}", std::process::id()));
    fs::write(&temporary, key).map_err(|e| io("cannot write", &temporary, e))?;
    fs::rename(&temporary, index_path).map_err(|e| io("cannot write", index_path, e))
}

/// Remove a directory tree, making read-only entries writable first.
fn remove_tree(path: &Path) -> Result<(), String> {
    if fs::symlink_metadata(path).is_err() {
        return Ok(());
    }
    fn writable(path: &Path) {
        if let Ok(metadata) = fs::symlink_metadata(path)
            && metadata.is_dir()
        {
            let _ = fs::set_permissions(path, fs::Permissions::from_mode(0o755));
            if let Ok(entries) = fs::read_dir(path) {
                for entry in entries.flatten() {
                    writable(&entry.path());
                }
            }
        }
    }
    writable(path);
    fs::remove_dir_all(path).map_err(|e| io("cannot remove", path, e))
}

/// Make the declared outputs read-only like completed action outputs
/// (test.log, test.xml and the test.outputs tree are 0555 in Bazel).
fn seal(path: &Path) {
    if let Ok(metadata) = fs::symlink_metadata(path) {
        if metadata.is_dir() {
            if let Ok(entries) = fs::read_dir(path) {
                for entry in entries.flatten() {
                    seal(&entry.path());
                }
            }
            let _ = fs::set_permissions(path, fs::Permissions::from_mode(0o555));
        } else if metadata.is_file() {
            let _ = fs::set_permissions(path, fs::Permissions::from_mode(0o555));
        }
    }
}

/// The paths of one test action relative to its execution root.
struct Paths {
    /// `bazel-out/<cfg>/testlogs/<pkg>/<dir>`.
    base: String,
    runfiles: String,
    tmp: String,
}

impl Runner<'_> {
    fn paths(&self, unit: &Unit<'_>) -> Paths {
        let name = unit
            .spec
            .label
            .rsplit_once(':')
            .map(|(_, name)| name)
            .unwrap_or(&unit.spec.label);
        let directory = test_directory(name, unit.shard, unit.shards, unit.run, unit.runs);
        let tmp_name = tmp_dir_name(&unit.spec.executable_exec, unit.shard, unit.run);
        // TestStrategy.getTmpRoot (:445-449): `--test_tmpdir` (relative to
        // the workspace) or the execution root's `_tmp`.
        let tmp = match &self.options.test_tmpdir {
            Some(root) => self
                .workspace
                .join(root)
                .join("_tmp")
                .join(&tmp_name)
                .display()
                .to_string(),
            None => format!("_tmp/{tmp_name}"),
        };
        Paths {
            base: format!("{}/{}", unit.spec.testlogs_package, directory.display()),
            runfiles: format!("{}.runfiles", unit.spec.executable_exec),
            tmp,
        }
    }

    fn environment(&self, unit: &Unit<'_>, paths: &Paths) -> BTreeMap<String, String> {
        let spec = unit.spec;
        let mut env = self.plain_environment(unit, paths);
        if let Some(coverage) = &spec.coverage {
            // TestRunnerAction.setupEnvVariables (:745-830): the coverage
            // environment yields to every other value, then the coverage
            // mode's variables.
            for (name, value) in &coverage.env {
                env.entry(name.clone()).or_insert_with(|| value.clone());
            }
            let settings = crate::coverage::settings();
            env.insert("RUNTEST_PRESERVE_CWD".to_owned(), "1".to_owned());
            env.insert(
                "COVERAGE_MANIFEST".to_owned(),
                coverage.manifest_exec.clone(),
            );
            env.insert("COVERAGE_DIR".to_owned(), coverage_directory(spec, paths));
            env.insert(
                "COVERAGE_OUTPUT_FILE".to_owned(),
                format!("{}/coverage.dat", paths.base),
            );
            env.insert(
                "SPLIT_COVERAGE_POST_PROCESSING".to_owned(),
                if settings.split_postprocessing {
                    "1"
                } else {
                    "0"
                }
                .to_owned(),
            );
            env.insert("IS_COVERAGE_SPAWN".to_owned(), "0".to_owned());
        }
        env
    }

    fn plain_environment(&self, unit: &Unit<'_>, paths: &Paths) -> BTreeMap<String, String> {
        let spec = unit.spec;
        test_environment(&EnvInputs {
            action_env: &spec.action_env,
            test_env: &spec.test_env,
            client_env: self.client_env,
            options: self.options,
            target_env: &spec.target_env,
            target_inherited: &spec.target_inherited,
            label: &spec.label,
            size: spec.info.size,
            timeout_seconds: self.options.timeout_seconds(spec.info.timeout),
            workspace: WORKSPACE,
            binary: &spec.binary,
            runfiles_dir: &paths.runfiles,
            tmp_dir: &paths.tmp,
            base_dir: &paths.base,
            shard: unit.shard,
            total_shards: unit.shards,
            run: unit.run,
            total_runs: unit.runs,
            runfiles_enabled: self.runfiles_enabled,
        })
    }

    /// The command line ([`test_arguments`]).
    fn arguments(&self, spec: &TestSpec) -> Vec<String> {
        test_arguments(spec, self.options)
    }

    /// One test action (TestRunnerAction.execute): a cached result, or the
    /// attempts in a fresh execution root and the published outputs. `None`:
    /// an interrupt (the build failed) stopped it — no result, nothing
    /// published or cached, like an interrupted TestRunnerAction.
    fn run_unit(
        &self,
        unit: &Unit<'_>,
        printer: &Mutex<()>,
        interrupt: &std::sync::atomic::AtomicBool,
    ) -> Result<Option<UnitOutcome>, String> {
        let spec = unit.spec;
        let options = self.options;
        let paths = self.paths(unit);
        let env = self.environment(unit, &paths);
        let args = self.arguments(spec);
        let test_name = test_name(&spec.label, unit.shard, unit.shards, unit.run, unit.runs);
        // The interactive progress display's running test (None when off).
        let _running = crate::progress::Running::test(&test_name);
        // Inputs by content: the runner scripts, then the runfiles.
        let mut inputs: Vec<(ContentInput, PathBuf)> = vec![
            (
                read_content_input(self.cache, &self.tools.setup)?,
                PathBuf::from(SETUP_EXEC_PATH),
            ),
            (
                read_content_input(self.cache, &self.tools.xml)?,
                PathBuf::from(XML_EXEC_PATH),
            ),
        ];
        let mut empty_files = Vec::new();
        // A tree artifact's files are staged at their paths below the
        // tree's runfile; an empty tree is its directory
        // (SpawnInputExpander.addSingleRunfilesTreeToInputs).
        let mut empty_trees = Vec::new();
        for (path, source) in &spec.entries {
            let destination = PathBuf::from(&paths.runfiles).join(path);
            match source {
                Some(source) if spec.symlinks.contains(path) => {
                    inputs.push((read_symlink_input(source)?, destination))
                }
                Some(source) => match read_content_source(self.cache, source)? {
                    ContentSource::File(input) => inputs.push((input, destination)),
                    ContentSource::Tree(files) if files.is_empty() => empty_trees.push(destination),
                    ContentSource::Tree(files) => inputs.extend(
                        files
                            .into_iter()
                            .map(|(name, input)| (input, destination.join(name))),
                    ),
                },
                None => empty_files.push(destination),
            }
        }
        // The coverage inputs at their exec paths, and the generated ones
        // (the instrumented file manifest, reported_to_actual_sources.txt and
        // the native LCOV_MERGER launcher) written into the execution root.
        let mut generated: Vec<(String, Vec<u8>, bool)> = Vec::new();
        if let Some(coverage) = &spec.coverage {
            for (exec, published) in &coverage.inputs {
                let destination = PathBuf::from(exec);
                match read_content_source(self.cache, published)? {
                    ContentSource::File(input) => inputs.push((input, destination)),
                    ContentSource::Tree(files) => inputs.extend(
                        files
                            .into_iter()
                            .map(|(name, input)| (input, destination.join(name))),
                    ),
                }
            }
            let mut manifest = String::new();
            for line in &coverage.manifest {
                manifest.push_str(line);
                manifest.push('\n');
            }
            generated.push((coverage.manifest_exec.clone(), manifest.into_bytes(), false));
            if let Some((path, content)) = &coverage.reported {
                generated.push((path.clone(), content.clone().into_bytes(), false));
            }
            if coverage.native_merger {
                generated.push((
                    crate::coverage::LAUNCHER_EXEC_PATH.to_owned(),
                    Vec::new(),
                    true,
                ));
            }
        }
        let execution_info = spec.execution_info();
        let mut hash = Sha256::new();
        frame(&mut hash, STATUS_FORMAT.as_bytes());
        frame(&mut hash, spec.label.as_bytes());
        for arg in &args {
            frame(&mut hash, arg.as_bytes());
        }
        frame(&mut hash, b"env");
        for (name, value) in &env {
            frame(&mut hash, name.as_bytes());
            frame(&mut hash, value.as_bytes());
        }
        frame(&mut hash, b"tags");
        for tag in &spec.info.tags {
            frame(&mut hash, tag.as_bytes());
        }
        for (name, value) in &execution_info {
            frame(&mut hash, name.as_bytes());
            frame(&mut hash, value.as_bytes());
        }
        frame(&mut hash, spec.info.size.name().as_bytes());
        frame(&mut hash, spec.info.timeout.name().as_bytes());
        frame(
            &mut hash,
            &options.timeout_seconds(spec.info.timeout).to_le_bytes(),
        );
        for number in [unit.shard, unit.shards, unit.run, unit.runs] {
            frame(&mut hash, &number.to_le_bytes());
        }
        frame(&mut hash, &[u8::from(options.zip_undeclared_outputs)]);
        frame(&mut hash, b"inputs");
        for (input, destination) in &inputs {
            frame(&mut hash, destination.as_os_str().as_encoded_bytes());
            frame(&mut hash, input.digest().as_bytes());
            frame(&mut hash, &[u8::from(input.executable())]);
        }
        for destination in &empty_files {
            frame(&mut hash, destination.as_os_str().as_encoded_bytes());
            frame(&mut hash, b"empty");
        }
        for destination in &empty_trees {
            frame(&mut hash, destination.as_os_str().as_encoded_bytes());
            frame(&mut hash, b"empty-tree");
        }
        frame(&mut hash, b"coverage");
        for (path, content, launcher) in &generated {
            frame(&mut hash, path.as_bytes());
            if *launcher {
                frame(&mut hash, b"native-coverage-output-generator-v1");
            } else {
                frame(&mut hash, content);
            }
        }
        // The output format decides whether test.xml is parsed (and stored).
        let parse_xml = matches!(
            options.summary,
            TestSummaryFormat::Detailed
                | TestSummaryFormat::DetailedUncached
                | TestSummaryFormat::Testcase
        );
        let key = hex(&hash.finalize());
        let tests = self.cache.join("tests");
        fs::create_dir_all(tests.join("locks")).map_err(|e| io("cannot create", &tests, e))?;
        let lock_path = tests.join("locks").join(format!("{key}.lock"));
        let lock = fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(&lock_path)
            .map_err(|e| io("cannot open lock", &lock_path, e))?;
        lock.lock_exclusive()
            .map_err(|e| io("cannot lock", &lock_path, e))?;
        let entry = tests.join(&key);
        let status_path = entry.join("status.json");
        // The action cache holds one entry per test action output
        // (test.cache_status): only the last key of this action's testlogs
        // directory is reusable, so a result of an older configuration of
        // the same test is not (genuine capture M37). The index of last keys
        // is the workspace's own (crate::worktree::state_directory: every
        // worktree sharing the cache has its output base's action cache),
        // and a test this workspace never ran — no index entry — accepts the
        // stored result of its exact key like a disk-cache hit: the first
        // run in a new worktree of the commit another one tested is cached,
        // and switching worktrees reruns nothing (docs/WORKTREES.md).
        let index_path = {
            let mut hash = Sha256::new();
            frame(&mut hash, b"native-test-index-v1");
            frame(&mut hash, paths.base.as_bytes());
            crate::worktree::state_directory(self.cache)
                .join("tests")
                .join("index")
                .join(hex(&hash.finalize()))
        };
        let (last_key, indexed) = match fs::read_to_string(&index_path) {
            Ok(key) => (key, true),
            Err(error) => (String::new(), error.kind() != std::io::ErrorKind::NotFound),
        };
        // TestRunnerAction.shouldAcceptCachedResult/executeUnconditionally.
        let accept = !(spec.info.external()
            || options.cache_test_results == TriState::No
            || (options.cache_test_results == TriState::Auto && unit.runs > 1));
        if accept
            && let Ok(bytes) = fs::read(&status_path)
            && let Ok(status) = serde_json::from_slice::<Status>(&bytes)
            && status.format == STATUS_FORMAT
            && status.key == key
            && status.cachable
            && (last_key == key || !indexed)
            && !(options.cache_test_results == TriState::Auto
                && !status_from(&status.status).passed())
            && status
                .passed_log
                .iter()
                .chain(&status.failed_logs)
                .all(|log| entry.join(log).is_file())
        {
            // The result's last use (docs/CACHE_GC.md).
            crate::cache_gc::note_used_path(&status_path);
            if !indexed {
                // This workspace's last key is now this one (best effort: a
                // failed write only costs the same lookup next time).
                let _ = write_index(&index_path, &key);
            }
            let result = RunResult {
                shard: unit.shard,
                status: status_from(&status.status),
                cached: true,
                test_times: status.test_times.clone(),
                passed_log: status.passed_log.as_ref().map(|log| entry.join(log)),
                failed_logs: status
                    .failed_logs
                    .iter()
                    .map(|log| entry.join(log))
                    .collect(),
                test_case: status.test_case.clone(),
            };
            // The cached TestResultData is used as stored (actionCacheHit,
            // TestRunnerAction.java:686-700): a test case is present only if
            // the run that produced it parsed test.xml; the stored test.xml
            // is not re-parsed for a detailed summary, like Bazel.
            // TerminalTestResultNotifier.notify prints locally cached
            // logs (TestSummaryPrinter.printCachedOutput) before the summary;
            // crate::main does that from the summary.
            self.link_testlogs(&paths.base, &entry);
            crate::spawn_stats::action_cache_hit();
            return Ok(Some(UnitOutcome::Result(result)));
        }
        if interrupt.load(std::sync::atomic::Ordering::Acquire) {
            return Ok(None);
        }
        // A fresh execution root.
        let work_parent = tests.join("work");
        fs::create_dir_all(&work_parent).map_err(|e| io("cannot create", &work_parent, e))?;
        let work = work_parent.join(format!("{key}-{}", std::process::id()));
        remove_tree(&work)?;
        let execroot = work.join("execroot");
        fs::create_dir_all(&execroot).map_err(|e| io("cannot create", &execroot, e))?;
        stage_content_inputs(self.cache, &inputs, &execroot)?;
        for destination in &empty_files {
            let path = execroot.join(destination);
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent).map_err(|e| io("cannot create", parent, e))?;
            }
            fs::write(&path, b"").map_err(|e| io("cannot write", &path, e))?;
        }
        for destination in &empty_trees {
            let path = execroot.join(destination);
            fs::create_dir_all(&path).map_err(|e| io("cannot create", &path, e))?;
        }
        for (path, content, launcher) in &generated {
            let path = execroot.join(path);
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent).map_err(|e| io("cannot create", parent, e))?;
            }
            let bytes = if *launcher {
                let binary = std::env::current_exe()
                    .map_err(|error| format!("cannot locate the bazel-rs binary: {error}"))?;
                crate::coverage::launcher_script(&binary).into_bytes()
            } else {
                content.clone()
            };
            fs::write(&path, bytes).map_err(|e| io("cannot write", &path, e))?;
            if *launcher {
                fs::set_permissions(&path, fs::Permissions::from_mode(0o755))
                    .map_err(|e| io("cannot chmod", &path, e))?;
            }
        }
        // The remote executor's view of the inputs (docs/REMOTE_EXECUTION.md),
        // built once per action when `--remote_executor` is set.
        if spec.coverage.is_some() && crate::sandbox::remote_executor() {
            return Err(format!(
                "{}: coverage test actions on a remote executor are not implemented in the native profile (docs/COVERAGE.md)",
                spec.label
            ));
        }
        let remote_inputs = if crate::sandbox::remote_executor() {
            let mut remote = RemoteTestInputs {
                spec: crate::remote_cache::SpawnSpec::default(),
                blobs: BTreeMap::new(),
                accept,
            };
            for (input, destination) in &inputs {
                let destination = crate::remote_cache::path_string(destination)?;
                input.add_remote(&destination, &mut remote.spec, &mut remote.blobs)?;
            }
            for destination in &empty_files {
                remote.spec.inputs.insert(
                    crate::remote_cache::path_string(destination)?,
                    crate::reapi::Digest::of(b""),
                );
            }
            for destination in &empty_trees {
                remote
                    .spec
                    .directories
                    .insert(crate::remote_cache::path_string(destination)?);
            }
            Some(remote)
        } else {
            None
        };
        let max_attempts = options.attempts(&spec.label, &spec.info);
        let mut failed_attempts: Vec<Attempt> = Vec::new();
        let mut printed = Vec::new();
        let outcome = loop {
            let attempt = match self.attempt(
                unit,
                &paths,
                &env,
                &args,
                &execroot,
                &test_name,
                parse_xml,
                interrupt,
                remote_inputs.as_ref(),
            )? {
                Attempted::Finished(attempt) => attempt,
                Attempted::Failed(error) => {
                    remove_tree(&work)?;
                    let location = spec
                        .location
                        .as_deref()
                        .map(|location| format!("{location}: "))
                        .unwrap_or_default();
                    let message = format!("Testing {test_name} failed: {error}");
                    let _guard = printer.lock().unwrap_or_else(|e| e.into_inner());
                    eprintln!("ERROR: {location}{message}");
                    break Some(UnitOutcome::Error(error));
                }
                // The build failed while the test ran: the action is
                // interrupted, its outputs are neither published nor cached.
                Attempted::Interrupted => {
                    remove_tree(&work)?;
                    break None;
                }
            };
            let last = attempt.status == TestStatus::Passed
                || failed_attempts.len() + 1 >= max_attempts as usize;
            let number = failed_attempts.len() + 1;
            let log = if last {
                PathBuf::from(&paths.base).join("test.log")
            } else {
                rename_attempt(&execroot, &paths.base, number)?
            };
            // TestStrategy.processTestOutput (:368-433).
            let passed = attempt.status == TestStatus::Passed;
            if should_output_log(options.output, passed) {
                printed.extend(test_log_block(
                    &execroot.join(&log),
                    &test_name,
                    options.max_test_output_bytes,
                ));
            }
            if !passed {
                let shown = entry.join(&log);
                let message = if attempt.status == TestStatus::Timeout {
                    None
                } else {
                    Some(format!(
                        "FAIL: {test_name} ({}) (see {})",
                        termination_short(attempt.exit_code),
                        shown.display()
                    ))
                };
                if let Some(message) = message {
                    // A FAIL event: dropped under --quiet (crate::ui).
                    let _guard = printer.lock().unwrap_or_else(|e| e.into_inner());
                    crate::ui::note(&message);
                }
            }
            if last {
                // StandaloneTestStrategy.finalizeTest (:211-247).
                let mut status = attempt.status;
                if status == TestStatus::Passed && !failed_attempts.is_empty() {
                    status = TestStatus::Flaky;
                }
                let mut test_times = vec![attempt.millis];
                test_times.extend(failed_attempts.iter().map(|failed| failed.millis));
                let mut failed_logs: Vec<String> = Vec::new();
                if !passed {
                    failed_logs.push(log.display().to_string());
                }
                failed_logs.extend(failed_attempts.iter().map(|failed| failed.log.clone()));
                let saved = Status {
                    format: STATUS_FORMAT.to_owned(),
                    key: key.clone(),
                    status: status.name().to_owned(),
                    cachable: attempt.cachable,
                    test_times: test_times.clone(),
                    passed_log: passed.then(|| log.display().to_string()),
                    failed_logs: failed_logs.clone(),
                    test_case: attempt.test_case.clone(),
                };
                self.publish(
                    &execroot,
                    &paths.base,
                    &entry,
                    &saved,
                    !failed_attempts.is_empty(),
                )?;
                self.link_testlogs(&paths.base, &entry);
                write_index(&index_path, &key)?;
                remove_tree(&work)?;
                break Some(UnitOutcome::Result(RunResult {
                    shard: unit.shard,
                    status,
                    cached: false,
                    test_times,
                    passed_log: saved.passed_log.as_ref().map(|log| entry.join(log)),
                    failed_logs: failed_logs.iter().map(|log| entry.join(log)).collect(),
                    test_case: attempt.test_case,
                }));
            }
            failed_attempts.push(Attempt {
                millis: attempt.millis,
                log: log.display().to_string(),
            });
        };
        drop(lock);
        if outcome.is_some() {
            crate::spawn_stats::test_completed();
        }
        // `INFO: From Testing`, the test log bound to the event: both are
        // dropped under --quiet (crate::ui).
        if !printed.is_empty() && outcome.is_some() && !crate::ui::quiet() {
            let _guard = printer.lock().unwrap_or_else(|e| e.into_inner());
            eprintln!("INFO: From Testing {test_name}:");
            let mut stdout = std::io::stdout().lock();
            let _ = stdout.write_all(&printed);
            let _ = stdout.flush();
        }
        Ok(outcome)
    }

    /// `$(bazel-rs info bazel-testlogs)/<pkg>/<name>` names this result's
    /// published testlogs (crate::output_tree); never fails the test.
    fn link_testlogs(&self, base: &str, entry: &Path) {
        if let Err(error) =
            crate::output_tree::link_directory(self.cache, Path::new(base), &entry.join(base))
        {
            crate::note!(
                "bazel-rs: warning: the output tree below execroot/ is incomplete: {error}"
            );
        }
    }

    /// Publish the testlogs of a finished action and its status below the
    /// cache entry (replacing an earlier result of the same key).
    fn publish(
        &self,
        execroot: &Path,
        base: &str,
        entry: &Path,
        status: &Status,
        attempted: bool,
    ) -> Result<(), String> {
        let source = execroot.join(base);
        for name in ["test.log", "test.xml"] {
            seal(&source.join(name));
        }
        seal(&source.join("test.outputs"));
        // With failed attempts, genuine Bazel leaves the final test.log
        // writable and the attempt logs read-only (capture M6/M10).
        if attempted {
            let _ = fs::set_permissions(source.join("test.log"), fs::Permissions::from_mode(0o644));
            if let Ok(entries) = fs::read_dir(source.join("test_attempts")) {
                for entry in entries.flatten() {
                    if entry.file_name().to_string_lossy().ends_with(".log") {
                        let _ =
                            fs::set_permissions(entry.path(), fs::Permissions::from_mode(0o555));
                    }
                }
            }
        }
        let staging = entry.with_extension("new");
        remove_tree(&staging)?;
        let target = staging.join(base);
        if let Some(parent) = target.parent() {
            fs::create_dir_all(parent).map_err(|e| io("cannot create", parent, e))?;
        }
        fs::rename(&source, &target).map_err(|e| io("cannot publish", &target, e))?;
        let bytes = serde_json::to_vec_pretty(status).map_err(|e| e.to_string())?;
        fs::write(staging.join("status.json"), bytes)
            .map_err(|e| io("cannot write", &staging.join("status.json"), e))?;
        remove_tree(entry)?;
        fs::rename(&staging, entry).map_err(|e| io("cannot publish", entry, e))
    }

    /// The spawn strategy and network policy for one test spawn (N59):
    /// TestRunner actions resolve like any other spawn
    /// (crate::sandbox::resolve with the TestRunner mnemonic,
    /// TestRunnerAction.java:112), the test rule's tags and the test
    /// action's execution info (TargetUtils.getExecutionInfo with the
    /// rule's testing.ExecutionInfo requirements, TestSpec::execution_info).
    /// Returns whether the sandbox must deny network
    /// (crate::sandbox::denies_network).
    fn strategy(&self, unit: &Unit<'_>) -> Result<(crate::sandbox::Strategy, bool), String> {
        let mut info: std::collections::BTreeSet<String> =
            unit.spec.info.tags.iter().cloned().collect();
        info.extend(unit.spec.execution_info().into_keys());
        Ok((
            crate::sandbox::resolve("TestRunner", &info, false)?,
            crate::sandbox::denies_network(&info),
        ))
    }

    /// The sandbox worker's arguments for one test spawn: the full process
    /// argv (`executable` in place of the wrapper's argv[0] spelling, which
    /// travels as --arg0), the test environment for TEST_TMPDIR, and the
    /// attempt's work directory for the worker's logs.
    fn worker_args(
        &self,
        env: &BTreeMap<String, String>,
        execroot: &Path,
        args: &[String],
        executable: &str,
        arg0: &str,
        deny_network: bool,
    ) -> Vec<String> {
        let environment: BTreeMap<String, std::ffi::OsString> = env
            .iter()
            .map(|(name, value)| (name.clone(), std::ffi::OsString::from(value)))
            .collect();
        let tmp = self
            .client_env
            .get("TMPDIR")
            .filter(|value| !value.is_empty())
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("/tmp"));
        let mut arguments = vec![executable.to_owned()];
        arguments.extend_from_slice(&args[1..]);
        crate::sandbox::wrap(
            execroot,
            &tmp,
            &environment,
            deny_network,
            &arguments,
            execroot.parent().unwrap_or(execroot),
            Some(arg0),
        )
    }

    /// One attempt (StandaloneTestRunnerSpawn.execute and runTestAttempt,
    /// exec/StandaloneTestStrategy.java:577-842). The error is an
    /// infrastructure failure; [`Attempted::Failed`] a TestExecException (the
    /// action fails without a result); [`Attempted::Interrupted`] an
    /// interrupt while the test ran (SIGTERM to its group, SIGKILL after the
    /// grace period, like a local spawn the build's failure interrupts).
    #[allow(clippy::too_many_arguments)]
    fn attempt(
        &self,
        unit: &Unit<'_>,
        paths: &Paths,
        env: &BTreeMap<String, String>,
        args: &[String],
        execroot: &Path,
        test_name: &str,
        parse_xml: bool,
        interrupt: &std::sync::atomic::AtomicBool,
        remote: Option<&RemoteTestInputs<'_>>,
    ) -> Result<Attempted, String> {
        let options = self.options;
        let base = execroot.join(&paths.base);
        // TestRunnerAction.prepare: outputs of an earlier attempt are gone
        // (renamed); TestStrategy.prepareFileSystem (:84-103).
        let tmp = if Path::new(&paths.tmp).is_absolute() {
            PathBuf::from(&paths.tmp)
        } else {
            execroot.join(&paths.tmp)
        };
        remove_tree(&tmp)?;
        for directory in [
            tmp.clone(),
            base.clone(),
            base.join("test.outputs"),
            base.join("test.outputs_manifest"),
            base.join("test.raw_splitlogs"),
        ] {
            fs::create_dir_all(&directory).map_err(|e| io("cannot create", &directory, e))?;
        }
        let log_path = base.join("test.log");
        let err_path = base.join("test.err");
        let log = fs::File::create(&log_path).map_err(|e| io("cannot create", &log_path, e))?;
        let err = fs::File::create(&err_path).map_err(|e| io("cannot create", &err_path, e))?;
        let tmpdir = self
            .client_env
            .get("TMPDIR")
            .filter(|value| !value.is_empty())
            .map(String::as_str)
            .unwrap_or("/tmp");
        // Test spawns run under the same spawn strategy selection as build
        // actions (N59, TestRunnerAction.java:112's TestRunner mnemonic; test
        // actions stay local-only, N23g): sandboxed tests go through the
        // sandbox worker, standalone ones start directly. A sandbox setup
        // failure is an infrastructure failure of the attempt; a non-zero
        // test exit under the sandbox is an ordinary test failure, without
        // the action suggestion text (StandaloneTestStrategy reports the
        // spawn's own status).
        let (strategy, deny_network) = self.strategy(unit)?;
        // The `remote` strategy (StandaloneTestStrategy's spawn through
        // RemoteSpawnRunner, docs/REMOTE_EXECUTION.md); a failure with
        // `--remote_local_fallback` runs the attempt standalone.
        let remote_run = match remote {
            Some(remote) if strategy == crate::sandbox::Strategy::Remote => {
                Some(self.run_test_remotely(unit, paths, env, args, remote, execroot)?)
            }
            _ => None,
        };
        let strategy = match remote_run {
            Some(RemoteTestRun::Fallback) => crate::sandbox::Strategy::Standalone,
            _ => strategy,
        };
        if let Some(RemoteTestRun::Failed(message)) = remote_run {
            return Ok(Attempted::Failed(message));
        }
        let remote_done = match remote_run {
            Some(RemoteTestRun::Done {
                exit_code,
                timed_out,
                millis,
            }) => Some((millis, exit_code, timed_out)),
            _ => None,
        };
        let sandboxed = strategy == crate::sandbox::Strategy::Sandboxed;
        // The attempt's spawn (`INFO: N processes: …`, crate::spawn_stats).
        crate::spawn_stats::spawn(strategy.runner_name());
        let (millis, exit_code, timed_out) = if let Some(done) = remote_done {
            drop((log, err));
            done
        } else {
            // The test runs under a supervisor (the process-wrapper or the
            // sandbox worker, crate::process_wrapper) in its own process group:
            // once it has exited, every process it left is killed and reaped —
            // also daemons that left its group (setsid; embedded databases'
            // `pg_ctl start`) — like Bazel's process-wrapper and linux-sandbox.
            let mut launch = if sandboxed {
                crate::process_wrapper::Launch::sandbox_worker(self.worker_args(
                    env,
                    execroot,
                    args,
                    SETUP_EXEC_PATH,
                    &args[0],
                    deny_network,
                ))
            } else {
                crate::process_wrapper::Launch::new(
                    execroot.join(SETUP_EXEC_PATH),
                    Some(std::ffi::OsStr::new(&args[0])),
                    &args[1..],
                )
            };
            launch
                .command()
                .env_clear()
                .envs(env)
                // PosixLocalEnvProvider.rewriteLocalEnv (exec/local/PosixLocalEnvProvider.java:44-60).
                .env("TMPDIR", tmpdir)
                .current_dir(execroot)
                .stdout(log)
                .stderr(err);
            let start = Instant::now();
            let mut child = launch
                .start()
                .map_err(|e| format!("cannot start {}: {e}", args[0]))?;
            let mut streamer = (options.output == TestOutput::Streamed).then(Streamer::default);
            let mut timed_out = false;
            let mut interrupted = false;
            let mut terminated_at = None;
            let timeout = Duration::from_secs(options.timeout_seconds(unit.spec.info.timeout));
            let status = loop {
                if let Some(streamer) = streamer.as_mut() {
                    streamer.pump(&log_path);
                }
                if let Some(status) = child.try_wait().map_err(|e| format!("cannot wait: {e}"))? {
                    break status;
                }
                let elapsed = start.elapsed();
                // The build failed (--nokeep_going): the test is interrupted like
                // a timed-out one — SIGTERM, SIGKILL after the grace period —
                // and reports no result (Bazel interrupts in-flight actions).
                if !timed_out
                    && !interrupted
                    && interrupt.load(std::sync::atomic::Ordering::Acquire)
                {
                    interrupted = true;
                    terminated_at = Some(Instant::now());
                    child.terminate();
                } else if !timed_out && !interrupted && elapsed >= timeout {
                    // SIGTERM to the test's process group, SIGKILL after the
                    // grace period (the process-wrapper's --timeout and
                    // --kill_delay, process-tools.cc:93-111).
                    timed_out = true;
                    terminated_at = Some(Instant::now());
                    child.terminate();
                } else if let Some(at) = terminated_at
                    && at.elapsed() >= TERMINATION_GRACE
                {
                    child.kill();
                    terminated_at = None;
                }
                std::thread::sleep(Duration::from_millis(10));
            };
            // What the interrupted test left was killed and reaped by its
            // supervisor, as after any exit.
            if interrupted {
                return Ok(Attempted::Interrupted);
            }
            // A sandbox setup failure is an infrastructure failure of the attempt
            // (crate::sandbox), not a test result.
            if sandboxed
                && let Some(work) = execroot.parent()
                && let Ok(error) = fs::read_to_string(work.join("sandbox-setup-error"))
                && !error.is_empty()
            {
                crate::sandbox::note_setup_failure();
                return Ok(Attempted::Failed(format!(
                    "{test_name}: could not set up the sandbox: {}",
                    error.trim_end()
                )));
            }
            // Test spawns stay out of crate::sandbox::count: the build summary's
            // "(... sandboxed)" note describes the build's actions, as it did
            // when the tests ran only after that summary was printed.
            let millis = start.elapsed().as_millis() as u64;
            if let Some(streamer) = streamer.as_mut() {
                streamer.pump(&log_path);
            }
            let exit_code = if timed_out {
                TIMEOUT_EXIT_CODE
            } else {
                use std::os::unix::process::ExitStatusExt;
                status
                    .code()
                    .unwrap_or_else(|| 128 + status.signal().unwrap_or(0))
            };
            (millis, exit_code, timed_out)
        };
        let mut test_status = if timed_out {
            TestStatus::Timeout
        } else if exit_code == 0 {
            TestStatus::Passed
        } else {
            TestStatus::Failed
        };
        let mut cachable = true;
        // TEST_PREMATURE_EXIT_FILE (:668-683).
        let premature = base.join("test.exited_prematurely");
        if fs::remove_file(&premature).is_ok() && test_status == TestStatus::Passed {
            cachable = false;
            test_status = TestStatus::Failed;
            let mut file = fs::OpenOptions::new()
                .append(true)
                .open(&err_path)
                .map_err(|e| io("cannot write", &err_path, e))?;
            file.write_all(b"-- Test exited prematurely (TEST_PREMATURE_EXIT_FILE exists) --\n")
                .map_err(|e| io("cannot write", &err_path, e))?;
        }
        // Sharding support (:685-703).
        if test_status == TestStatus::Passed
            && unit.shards > 1
            && options.check_sharding_support
            && !base.join("test.shard").exists()
        {
            return Ok(Attempted::Failed("Sharding requested, but the test runner did not advertise support for it by touching TEST_SHARD_STATUS_FILE. Either remove the 'shard_count' attribute, use a test runner that supports sharding or temporarily disable this check via --noincompatible_check_sharding_support.".to_owned()));
        }
        // Append stray stderr output to the test log (:762-778).
        let mut stray = Vec::new();
        fs::File::open(&err_path)
            .and_then(|mut file| file.read_to_end(&mut stray))
            .map_err(|e| io("cannot read", &err_path, e))?;
        let _ = fs::remove_file(&err_path);
        if !stray.is_empty() {
            let mut file = fs::OpenOptions::new()
                .append(true)
                .open(&log_path)
                .map_err(|e| io("cannot write", &log_path, e))?;
            file.write_all(&stray)
                .map_err(|e| io("cannot write", &log_path, e))?;
        }
        // The XML generating spawn when the test wrote no test.xml (:784-813).
        let xml = base.join("test.xml");
        if !xml.exists()
            && let (Some(remote), Some(_)) = (remote, remote_done)
        {
            crate::spawn_stats::spawn("remote");
            self.xml_remotely(
                unit, paths, env, test_name, remote, execroot, millis, exit_code,
            )?;
        } else if !xml.exists() {
            crate::spawn_stats::spawn(strategy.runner_name());
            let mut xml_env = env.clone();
            xml_env.insert("TEST_NAME".to_owned(), test_name.to_owned());
            if unit.shards <= 1 {
                xml_env.insert("TEST_SHARD_INDEX".to_owned(), "0".to_owned());
                xml_env.insert("TEST_TOTAL_SHARDS".to_owned(), "0".to_owned());
            }
            let xml_arguments = [
                format!("{}/test.log", paths.base),
                format!("{}/test.xml", paths.base),
                (millis / 1000).to_string(),
                exit_code.to_string(),
            ];
            let mut launch = if sandboxed {
                let mut arguments = vec![XML_EXEC_PATH.to_owned()];
                arguments.extend(xml_arguments.iter().cloned());
                crate::process_wrapper::Launch::sandbox_worker(self.worker_args(
                    &xml_env,
                    execroot,
                    &arguments,
                    XML_EXEC_PATH,
                    XML_EXEC_PATH,
                    deny_network,
                ))
            } else {
                crate::process_wrapper::Launch::new(
                    execroot.join(XML_EXEC_PATH),
                    Some(std::ffi::OsStr::new(XML_EXEC_PATH)),
                    &xml_arguments,
                )
            };
            launch
                .command()
                .env_clear()
                .envs(&xml_env)
                .env(
                    "TMPDIR",
                    self.client_env
                        .get("TMPDIR")
                        .filter(|value| !value.is_empty())
                        .map(String::as_str)
                        .unwrap_or("/tmp"),
                )
                .current_dir(execroot)
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::piped());
            let output = launch
                .start()
                .and_then(crate::process_wrapper::Spawned::wait_with_output)
                .map_err(|e| format!("cannot start {XML_EXEC_PATH}: {e}"))?;
            if !output.status.success() {
                return Err(format!(
                    "{test_name}: generating test.xml failed ({}): {}",
                    output.status,
                    String::from_utf8_lossy(&output.stderr)
                ));
            }
        }
        let test_case = if parse_xml {
            fs::read_to_string(&xml)
                .ok()
                .and_then(|text| crate::test_command::parse_test_xml(&text))
        } else {
            None
        };
        Ok(Attempted::Finished(AttemptResult {
            status: test_status,
            exit_code,
            millis,
            cachable,
            test_case,
        }))
    }
}

/// How an attempt ended ([`Runner::attempt`]).
enum Attempted {
    Finished(AttemptResult),
    /// A TestExecException: the action fails without a result.
    Failed(String),
    /// The build failed while the test ran.
    Interrupted,
}

/// A finished attempt.
struct AttemptResult {
    status: TestStatus,
    exit_code: i32,
    millis: u64,
    cachable: bool,
    test_case: Option<TestCase>,
}

/// A test action's inputs as a remote spawn's input root, and whether a
/// cached result is acceptable (TestRunnerAction.shouldAcceptCachedResult:
/// otherwise the spawn carries `no-cache`, StandaloneTestStrategy.java:113-120).
struct RemoteTestInputs<'a> {
    spec: crate::remote_cache::SpawnSpec,
    blobs: BTreeMap<crate::reapi::Digest, crate::remote_cache::BlobSource<'a>>,
    accept: bool,
}

/// How a remote test spawn ended.
enum RemoteTestRun {
    Done {
        exit_code: i32,
        timed_out: bool,
        millis: u64,
    },
    Fallback,
    Failed(String),
}

/// Spawns.mayBeCachedRemotely over a test's execution requirements.
fn remotely_cacheable(info: &std::collections::BTreeSet<String>) -> bool {
    ["no-cache", "local", "no-remote", "no-remote-cache"]
        .iter()
        .all(|tag| !info.contains(*tag))
}

impl Runner<'_> {
    /// The test's execution requirements (its tags and the rule's
    /// testing.ExecutionInfo, TargetUtils.getExecutionInfo).
    fn requirements(&self, unit: &Unit<'_>) -> std::collections::BTreeSet<String> {
        let mut info: std::collections::BTreeSet<String> =
            unit.spec.info.tags.iter().cloned().collect();
        info.extend(unit.spec.execution_info().into_keys());
        info
    }

    /// One remote spawn of a test action (crate::action::remote_spawn).
    fn remote_test_spawn(
        &self,
        spec: crate::remote_cache::SpawnSpec,
        blobs: &BTreeMap<crate::reapi::Digest, crate::remote_cache::BlobSource<'_>>,
        declared: &[(String, crate::remote_cache::OutputKind)],
        cacheable: bool,
    ) -> Result<crate::action::RemoteSpawn, String> {
        let remote =
            crate::remote_cache::active().ok_or("remote execution without a remote module")?;
        let mut spec = spec;
        spec.cacheable = cacheable;
        let action = crate::remote_cache::remote_action_of(&spec)?;
        crate::action::remote_spawn(remote, &action, blobs, declared, cacheable, false, &|_| {
            false
        })
    }

    /// The common SpawnSpec fields of a test action's spawns: the
    /// environment, the `Platform` (`--remote_default_exec_properties`; a
    /// test target's own exec_properties are not recorded natively) and the
    /// REAPI output form.
    fn remote_base(
        &self,
        remote_inputs: &RemoteTestInputs<'_>,
        env: &BTreeMap<String, String>,
    ) -> Result<crate::remote_cache::SpawnSpec, String> {
        let remote =
            crate::remote_cache::active().ok_or("remote execution without a remote module")?;
        let output_paths = remote.output_paths().map_err(|error| error.to_string())?;
        let mut spec = remote_inputs.spec.clone();
        spec.environment = env.clone();
        spec.platform = crate::remote_execution::platform_properties(
            &crate::remote_execution::SpawnPlatform::default(),
            &remote.default_exec_properties,
        )?;
        spec.use_output_paths = output_paths;
        spec.may_be_executed_remotely = true;
        Ok(spec)
    }

    /// The test spawn on the remote executor (StandaloneTestStrategy
    /// .createTestRunnerSpawn through RemoteSpawnRunner): TestRunnerAction
    /// .getSpawnOutputs (none mandatory), the `timeout` requirement as
    /// `Action.timeout`; stdout becomes test.log and stderr test.err, the
    /// outputs land in the attempt's execution root.
    #[allow(clippy::too_many_arguments)]
    fn run_test_remotely(
        &self,
        unit: &Unit<'_>,
        paths: &Paths,
        env: &BTreeMap<String, String>,
        args: &[String],
        remote_inputs: &RemoteTestInputs<'_>,
        execroot: &Path,
    ) -> Result<RemoteTestRun, String> {
        use crate::remote_cache::OutputKind;
        let base = &paths.base;
        let mut spec = self.remote_base(remote_inputs, env)?;
        spec.arguments = args.to_vec();
        let undeclared = if self.options.zip_undeclared_outputs {
            (format!("{base}/test.outputs/outputs.zip"), OutputKind::File)
        } else {
            (format!("{base}/test.outputs"), OutputKind::Directory)
        };
        let mut declared = vec![
            (format!("{base}/test.xml"), OutputKind::File),
            (format!("{base}/test.exited_prematurely"), OutputKind::File),
        ];
        if unit.shards > 1 {
            declared.push((format!("{base}/test.shard"), OutputKind::File));
        }
        declared.extend([
            (format!("{base}/test.warnings"), OutputKind::File),
            (
                format!("{base}/test.raw_splitlogs/test.splitlogs"),
                OutputKind::File,
            ),
            (format!("{base}/test.unused_runfiles_log"), OutputKind::File),
            (
                format!("{base}/test.infrastructure_failure"),
                OutputKind::File,
            ),
            undeclared,
            (
                format!("{base}/test.outputs_manifest/MANIFEST"),
                OutputKind::File,
            ),
            (
                format!("{base}/test.outputs_manifest/ANNOTATIONS"),
                OutputKind::File,
            ),
            (
                format!("{base}/test.outputs_manifest/ANNOTATIONS.pb"),
                OutputKind::File,
            ),
        ]);
        for (path, kind) in &declared {
            spec.outputs.push(path.clone());
            if *kind == OutputKind::Directory {
                spec.output_directories.insert(path.clone());
                spec.directories.insert(path.clone());
            }
        }
        spec.timeout_seconds = Some(self.options.timeout_seconds(unit.spec.info.timeout));
        let mut info = self.requirements(unit);
        if !remote_inputs.accept {
            info.insert("no-cache".to_owned());
        }
        let start = Instant::now();
        let outcome = self.remote_test_spawn(
            spec,
            &remote_inputs.blobs,
            &declared,
            remotely_cacheable(&info),
        )?;
        let millis = start.elapsed().as_millis() as u64;
        let root = execroot.join(base);
        let write = |name: &str, bytes: &[u8]| {
            let path = root.join(name);
            fs::write(&path, bytes).map_err(|e| io("cannot write", &path, e))
        };
        match outcome {
            crate::action::RemoteSpawn::Fallback => Ok(RemoteTestRun::Fallback),
            crate::action::RemoteSpawn::Failed(message) => Ok(RemoteTestRun::Failed(message)),
            crate::action::RemoteSpawn::Timeout => Ok(RemoteTestRun::Done {
                exit_code: TIMEOUT_EXIT_CODE,
                timed_out: true,
                millis,
            }),
            crate::action::RemoteSpawn::Done { result, .. } => {
                for ((path, _), output) in declared.iter().zip(&result.outputs) {
                    let full = execroot.join(path);
                    if matches!(output, crate::remote_cache::Downloaded::Missing) {
                        continue;
                    }
                    if let Some(parent) = full.parent() {
                        fs::create_dir_all(parent).map_err(|e| io("cannot create", parent, e))?;
                    }
                    if !matches!(output, crate::remote_cache::Downloaded::Directory(_)) {
                        let _ = fs::remove_file(&full);
                    }
                    crate::action::write_downloaded(&full, output)?;
                }
                write("test.log", &result.stdout)?;
                write("test.err", &result.stderr)?;
                Ok(RemoteTestRun::Done {
                    exit_code: result.exit_code,
                    timed_out: false,
                    millis,
                })
            }
        }
    }

    /// The XML generating spawn on the remote executor
    /// (StandaloneTestStrategy.createXmlGeneratingSpawn: the test log and
    /// generate-xml.sh as inputs, test.xml the only output, the action's
    /// execution info without `no-cache` or a timeout).
    #[allow(clippy::too_many_arguments)]
    fn xml_remotely(
        &self,
        unit: &Unit<'_>,
        paths: &Paths,
        env: &BTreeMap<String, String>,
        test_name: &str,
        remote_inputs: &RemoteTestInputs<'_>,
        execroot: &Path,
        millis: u64,
        exit_code: i32,
    ) -> Result<(), String> {
        use crate::remote_cache::{BlobSource, OutputKind};
        let base = &paths.base;
        let mut xml_env = env.clone();
        xml_env.insert("TEST_NAME".to_owned(), test_name.to_owned());
        if unit.shards <= 1 {
            xml_env.insert("TEST_SHARD_INDEX".to_owned(), "0".to_owned());
            xml_env.insert("TEST_TOTAL_SHARDS".to_owned(), "0".to_owned());
        }
        let mut spec = self.remote_base(remote_inputs, &xml_env)?;
        let script = spec
            .inputs
            .get(XML_EXEC_PATH)
            .cloned()
            .ok_or("generate-xml.sh is not a test input")?;
        spec.inputs.clear();
        spec.symlinks.clear();
        spec.directories.clear();
        spec.inputs.insert(XML_EXEC_PATH.to_owned(), script.clone());
        let log_path = execroot.join(base).join("test.log");
        let log = fs::read(&log_path).map_err(|e| io("cannot read", &log_path, e))?;
        let log_digest = crate::reapi::Digest::of(&log);
        spec.inputs
            .insert(format!("{base}/test.log"), log_digest.clone());
        let mut blobs = BTreeMap::new();
        if let Some(source) = remote_inputs.blobs.get(&script) {
            blobs.insert(script, *source);
        }
        blobs.insert(log_digest, BlobSource::Bytes(&log));
        spec.arguments = vec![
            XML_EXEC_PATH.to_owned(),
            format!("{base}/test.log"),
            format!("{base}/test.xml"),
            (millis / 1000).to_string(),
            exit_code.to_string(),
        ];
        let declared = vec![(format!("{base}/test.xml"), OutputKind::File)];
        spec.outputs = vec![declared[0].0.clone()];
        let info = self.requirements(unit);
        match self.remote_test_spawn(spec, &blobs, &declared, remotely_cacheable(&info))? {
            crate::action::RemoteSpawn::Done { result, .. } if result.exit_code == 0 => {
                if let Some(output) = result.outputs.first() {
                    crate::action::write_downloaded(&execroot.join(&declared[0].0), output)?;
                }
                Ok(())
            }
            crate::action::RemoteSpawn::Done { result, .. } => Err(format!(
                "{test_name}: generating test.xml failed (exit status: {}): {}",
                result.exit_code,
                String::from_utf8_lossy(&result.stderr)
            )),
            crate::action::RemoteSpawn::Failed(message) => Err(format!(
                "{test_name}: generating test.xml failed: {message}"
            )),
            crate::action::RemoteSpawn::Timeout => {
                Err(format!("{test_name}: generating test.xml timed out"))
            }
            // The fallback strategy for the XML spawn: run it locally.
            crate::action::RemoteSpawn::Fallback => Err(format!(
                "{test_name}: generating test.xml remotely failed and the local fallback of the XML spawn is not modelled"
            )),
        }
    }
}

/// A failed attempt kept for the final result.
struct Attempt {
    millis: u64,
    log: String,
}

/// StandaloneTestStrategy.renameOutputs (:140-187): the outputs of failed
/// attempt N move to `test_attempts/attempt_N.*`; returns the log's path
/// relative to the execution root.
fn rename_attempt(execroot: &Path, base: &str, number: usize) -> Result<PathBuf, String> {
    let root = execroot.join(base);
    let attempts = root.join("test_attempts");
    fs::create_dir_all(&attempts).map_err(|e| io("cannot create", &attempts, e))?;
    let prefix = format!("attempt_{number}");
    let mut outputs: Vec<PathBuf> = Vec::new();
    for name in [
        "test.xml",
        "test.warnings",
        "test.raw_splitlogs/test.splitlogs",
        "test.outputs_manifest/MANIFEST",
        "test.outputs_manifest/ANNOTATIONS",
        "test.outputs_manifest/ANNOTATIONS.pb",
    ] {
        if root.join(name).is_file() {
            outputs.push(PathBuf::from(name));
        }
    }
    fn files(root: &Path, relative: &Path, out: &mut Vec<PathBuf>) {
        if let Ok(entries) = fs::read_dir(root.join(relative)) {
            let mut names: Vec<_> = entries.flatten().map(|entry| entry.file_name()).collect();
            names.sort();
            for name in names {
                let path = relative.join(name);
                if root.join(&path).is_dir() {
                    files(root, &path, out);
                } else {
                    out.push(path);
                }
            }
        }
    }
    files(&root, Path::new("test.outputs"), &mut outputs);
    for output in outputs {
        let text = output.display().to_string();
        let renamed = text.replacen("test", &prefix, 1);
        let destination = attempts.join(&renamed);
        if let Some(parent) = destination.parent() {
            fs::create_dir_all(parent).map_err(|e| io("cannot create", parent, e))?;
        }
        fs::rename(root.join(&output), &destination)
            .map_err(|e| io("cannot rename", &destination, e))?;
    }
    let log = attempts.join(format!("{prefix}.log"));
    fs::rename(root.join("test.log"), &log).map_err(|e| io("cannot rename", &log, e))?;
    Ok(PathBuf::from(base)
        .join("test_attempts")
        .join(format!("{prefix}.log")))
}

/// `--test_output=streamed`: the test log after its header, copied to
/// stdout as it grows (StreamedTestOutput, exec/StreamedTestOutput.java).
#[derive(Default)]
struct Streamer {
    offset: u64,
    header_seen: bool,
    pending: Vec<u8>,
}

impl Streamer {
    fn pump(&mut self, log: &Path) {
        let Ok(mut file) = fs::File::open(log) else {
            return;
        };
        use std::io::Seek;
        if file.seek(std::io::SeekFrom::Start(self.offset)).is_err() {
            return;
        }
        let mut bytes = Vec::new();
        if file.read_to_end(&mut bytes).is_err() || bytes.is_empty() {
            return;
        }
        self.offset += bytes.len() as u64;
        let mut stdout = std::io::stdout().lock();
        if self.header_seen {
            let _ = stdout.write_all(&bytes);
        } else {
            self.pending.extend_from_slice(&bytes);
            let filtered = filter_header(&self.pending);
            if filtered.len() != self.pending.len() {
                self.header_seen = true;
                let _ = stdout.write_all(filtered);
                self.pending.clear();
            }
        }
        let _ = stdout.flush();
    }
}

/// The cached-output block TerminalTestResultNotifier prints for locally
/// cached results before the summary (TestSummaryPrinter.printCachedOutput,
/// runtime/TestSummaryPrinter.java:49-99).
pub fn cached_output(summary: &TestSummary, options: &TestOptions) -> Vec<u8> {
    let mut out = Vec::new();
    let logs: Vec<String> = summary
        .failed_logs
        .iter()
        .chain(&summary.passed_logs)
        .map(|path| path.display().to_string())
        .collect();
    out.extend(
        format!(
            "{}: {} (see {})\nINFO: From Testing {}\n",
            summary.status.name(),
            summary.label,
            logs.join(" "),
            summary.label
        )
        .into_bytes(),
    );
    if should_output_log(options.output, false) {
        for path in &summary.failed_logs {
            out.extend(test_log_block(
                path,
                &summary.label,
                options.max_test_output_bytes,
            ));
        }
    }
    if should_output_log(options.output, true) {
        for path in &summary.passed_logs {
            out.extend(test_log_block(
                path,
                &summary.label,
                options.max_test_output_bytes,
            ));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_command::{TestSize, TestTimeout};

    /// TestTargetProperties.java:96-122: the rule's tags-derived execution
    /// info, then the ExecutionInfo requirements put over it (a requirement
    /// replaces a tag's empty value), so they reach the spawn strategy and
    /// network policy like tags do.
    #[test]
    fn execution_info_requirements_go_over_the_rule_tags() {
        let spec = TestSpec {
            label: "//pkg:t".into(),
            info: TestRuleInfo {
                rule_class: "x_test".into(),
                size: TestSize::Medium,
                timeout: TestTimeout::Moderate,
                flaky: false,
                shard_count: -1,
                local: false,
                tags: vec!["requires-network".into(), "manual".into()],
            },
            location: None,
            executable: PathBuf::new(),
            executable_exec: String::new(),
            binary: String::new(),
            entries: Vec::new(),
            symlinks: Default::default(),
            testlogs_package: String::new(),
            args: Vec::new(),
            target_env: Vec::new(),
            target_inherited: Vec::new(),
            action_env: Default::default(),
            test_env: Default::default(),
            requirements: vec![
                ("requires-network".into(), "1".into()),
                ("no-sandbox".into(), String::new()),
            ],
            exclusive_sandboxed: true,
            run_under_executable: None,
            coverage: None,
        };
        assert_eq!(
            spec.execution_info(),
            BTreeMap::from([
                ("no-sandbox".to_owned(), String::new()),
                ("requires-network".to_owned(), "1".to_owned()),
            ])
        );
        let keys = spec
            .execution_info()
            .into_keys()
            .collect::<std::collections::BTreeSet<_>>();
        assert!(!crate::sandbox::denies_network(&keys));
        let without = TestSpec {
            requirements: Vec::new(),
            ..spec
        };
        assert_eq!(
            without.execution_info(),
            BTreeMap::from([("requires-network".to_owned(), String::new())])
        );
    }
}
