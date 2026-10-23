//! The `test` command (N23g, docs/TEST_COMMAND.md): the test options
//! (TestConfiguration, ExecutionOptions, TestSummaryOptions, LoadingOptions),
//! test target properties (TestTargetProperties, TestSize, TestTimeout),
//! the test filters (TestFilter, TestTargetUtils), the test environment
//! (TestPolicy.computeTestEnvironment, TestRunnerAction.setupEnvVariables)
//! and the terminal summary (TerminalTestResultNotifier, TestSummaryPrinter,
//! TestLogHelper). Execution is crate::test_runner.
//!
//! Paths below are under src/main/java/com/google/devtools/build/lib/ of the
//! pinned Bazel 9.3 sources.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

/// packages/TestSize.java:31-36.
#[derive(
    Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, serde::Serialize, serde::Deserialize,
)]
pub enum TestSize {
    Small,
    Medium,
    Large,
    Enormous,
}

impl TestSize {
    pub const ALL: [TestSize; 4] = [
        TestSize::Small,
        TestSize::Medium,
        TestSize::Large,
        TestSize::Enormous,
    ];

    /// TestSize.getTestSize(String): lowercase names only.
    pub fn parse(text: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|size| size.name() == text)
    }

    pub fn name(self) -> &'static str {
        match self {
            TestSize::Small => "small",
            TestSize::Medium => "medium",
            TestSize::Large => "large",
            TestSize::Enormous => "enormous",
        }
    }

    pub fn default_timeout(self) -> TestTimeout {
        match self {
            TestSize::Small => TestTimeout::Short,
            TestSize::Medium => TestTimeout::Moderate,
            TestSize::Large => TestTimeout::Long,
            TestSize::Enormous => TestTimeout::Eternal,
        }
    }

    /// TestSize.getTestSize(TestTimeout).
    fn for_timeout(timeout: TestTimeout) -> Option<Self> {
        Self::ALL
            .into_iter()
            .find(|size| size.default_timeout() == timeout)
    }
}

/// packages/TestTimeout.java:42-45.
#[derive(
    Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, serde::Serialize, serde::Deserialize,
)]
pub enum TestTimeout {
    Short,
    Moderate,
    Long,
    Eternal,
}

/// One year, the cap of the largest suggested and fuzzy ranges (:57).
const MAX_TIMEOUT: u64 = 365 * 24 * 60 * 60;

impl TestTimeout {
    pub const ALL: [TestTimeout; 4] = [
        TestTimeout::Short,
        TestTimeout::Moderate,
        TestTimeout::Long,
        TestTimeout::Eternal,
    ];

    pub fn parse(text: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|timeout| timeout.name() == text)
    }

    pub fn seconds(self) -> u64 {
        match self {
            TestTimeout::Short => 60,
            TestTimeout::Moderate => 300,
            TestTimeout::Long => 900,
            TestTimeout::Eternal => 3600,
        }
    }

    /// TestTimeout.toString (lowercase).
    pub fn name(self) -> &'static str {
        match self {
            TestTimeout::Short => "short",
            TestTimeout::Moderate => "moderate",
            TestTimeout::Long => "long",
            TestTimeout::Eternal => "eternal",
        }
    }

    fn index(self) -> usize {
        Self::ALL
            .iter()
            .position(|timeout| *timeout == self)
            .unwrap_or(0)
    }

    /// The fuzzy and suggested ranges of the static initializer (:51-99):
    /// fuzzy `[previous / 2, 90%)`, suggested `[previous max, 75%)`, the
    /// last capped at one year.
    fn ranges(self) -> ((u64, u64), (u64, u64)) {
        let mut previous_max_suggested = 0;
        let mut previous_timeout = 0;
        for (position, timeout) in Self::ALL.iter().enumerate() {
            let last = position + 1 == Self::ALL.len();
            let seconds = timeout.seconds();
            let max_suggested = if last {
                MAX_TIMEOUT
            } else {
                (seconds as f64 * 0.75) as u64
            };
            let max_fuzzy = if last {
                MAX_TIMEOUT
            } else {
                (seconds as f64 * 0.9) as u64
            };
            let ranges = (
                (previous_timeout / 2, max_fuzzy),
                (previous_max_suggested, max_suggested),
            );
            if *timeout == self {
                return ranges;
            }
            previous_max_suggested = max_suggested;
            previous_timeout = seconds;
        }
        unreachable!("every timeout is in ALL")
    }

    pub fn in_range_fuzzy(self, seconds: u64) -> bool {
        let (low, high) = self.ranges().0;
        low <= seconds && seconds < high
    }

    /// TestTimeout.getSuggestedTestTimeout.
    pub fn suggested(seconds: u64) -> Option<Self> {
        Self::ALL.into_iter().find(|timeout| {
            let (low, high) = timeout.ranges().1;
            low <= seconds && seconds < high
        })
    }
}

/// ExecutionOptions.TestOutputFormat (exec/ExecutionOptions.java:528-549).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TestOutput {
    Summary,
    Errors,
    All,
    Streamed,
}

/// ExecutionOptions.TestSummaryFormat (:552-577).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TestSummaryFormat {
    Short,
    ShortUncached,
    Terse,
    Detailed,
    DetailedUncached,
    None,
    Testcase,
}

/// TriState (common/options/TriState.java) of `--cache_test_results`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TriState {
    Auto,
    Yes,
    No,
}

/// TestConfiguration.TestShardingStrategy (analysis/test/TestConfiguration.java:521-560).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ShardingStrategy {
    Explicit,
    Disabled,
    Forced(i32),
}

impl ShardingStrategy {
    /// getNumberOfShards: the explicit shard count (0 when unspecified).
    pub fn shards(self, explicit: i32) -> i32 {
        match self {
            ShardingStrategy::Explicit => explicit.max(0),
            ShardingStrategy::Disabled => 0,
            ShardingStrategy::Forced(count) => count,
        }
    }
}

/// The test options of this profile. Every other test option is rejected
/// by the option parser (never silently ignored).
#[derive(Clone, Debug)]
pub struct TestOptions {
    pub output: TestOutput,
    pub summary: TestSummaryFormat,
    pub cache_test_results: TriState,
    /// `--test_arg`.
    pub args: Vec<String>,
    /// `--test_filter` (TESTBRIDGE_TEST_ONLY).
    pub filter: Option<String>,
    pub runner_fail_fast: bool,
    /// `--test_timeout`: seconds per TestTimeout (SHORT..ETERNAL).
    pub timeouts: [u64; 4],
    /// `--flaky_test_attempts` (a number; `None` is `default`).
    pub flaky_attempts: Option<u32>,
    pub runs_per_test: u32,
    pub sharding: ShardingStrategy,
    pub tag_filters: Vec<String>,
    pub size_filters: Option<BTreeSet<TestSize>>,
    pub timeout_filters: Option<BTreeSet<TestTimeout>>,
    pub lang_filters: Vec<String>,
    pub build_tests_only: bool,
    pub verbose_summary: bool,
    pub verbose_timeout_warnings: bool,
    pub max_test_output_bytes: i64,
    pub check_sharding_support: bool,
    pub zip_undeclared_outputs: bool,
    /// `--local_test_jobs` (0 is `auto`: the `--jobs` value).
    pub local_test_jobs: usize,
    /// `--test_tmpdir` (relative to the workspace).
    pub test_tmpdir: Option<String>,
    /// `--test_strategy=exclusive`: every test runs alone.
    pub exclusive: bool,
    /// `--keep_going`/`-k` (KeepGoingOption): a failing test action does
    /// not stop the remaining tests.
    pub keep_going: bool,
}

impl Default for TestOptions {
    fn default() -> Self {
        Self {
            output: TestOutput::Summary,
            summary: TestSummaryFormat::Short,
            cache_test_results: TriState::Auto,
            args: Vec::new(),
            filter: None,
            runner_fail_fast: false,
            timeouts: [60, 300, 900, 3600],
            flaky_attempts: None,
            runs_per_test: 1,
            sharding: ShardingStrategy::Explicit,
            tag_filters: Vec::new(),
            size_filters: None,
            timeout_filters: None,
            lang_filters: Vec::new(),
            build_tests_only: false,
            verbose_summary: true,
            verbose_timeout_warnings: false,
            max_test_output_bytes: -1,
            check_sharding_support: true,
            zip_undeclared_outputs: false,
            local_test_jobs: 0,
            test_tmpdir: None,
            exclusive: false,
            keep_going: false,
        }
    }
}

/// `While parsing option --NAME=VALUE: MESSAGE` (OptionsParserImpl).
fn parse_error(flag: &str, value: &str, message: &str) -> String {
    format!("While parsing option {flag}={value}: {message}")
}

/// EnumConverter.convert (common/options/EnumConverter.java:46-56).
fn enum_value<T: Copy>(
    flag: &str,
    value: &str,
    type_name: &str,
    values: &[(&str, T)],
) -> Result<T, String> {
    let lower = value.to_ascii_lowercase();
    if let Some((_, item)) = values.iter().find(|(name, _)| *name == lower) {
        return Ok(*item);
    }
    let names: Vec<&str> = values.iter().map(|(name, _)| *name).collect();
    Err(parse_error(
        flag,
        value,
        &format!(
            "Not a valid {type_name}: '{value}' (should be {})",
            english_or(&names)
        ),
    ))
}

/// Converters.joinEnglishList (common/options/Converters.java:240-250).
fn english_or(items: &[&str]) -> String {
    let mut text = String::new();
    for (index, item) in items.iter().enumerate() {
        if index > 0 {
            text.push_str(if index + 1 < items.len() {
                ", "
            } else {
                " or "
            });
        }
        text.push_str(item);
    }
    text
}

/// StringUtil.joinEnglishList(choices, "and") with the Oxford comma
/// (util/StringUtil.java:44-73).
pub fn english_and(items: &[String]) -> String {
    let mut text = String::new();
    for (index, item) in items.iter().enumerate() {
        if index > 0 {
            let last = index + 1 == items.len();
            if !last || index >= 2 {
                text.push(',');
            }
            if last {
                text.push_str(" and");
            }
            text.push(' ');
        }
        text.push_str(item);
    }
    if text.is_empty() {
        "nothing".to_owned()
    } else {
        text
    }
}

/// CommaSeparatedOptionListConverter: empty values are dropped.
fn comma_list(value: &str) -> Vec<String> {
    value
        .split(',')
        .filter(|item| !item.is_empty())
        .map(str::to_owned)
        .collect()
}

/// EnumFilterConverter.convert (packages/EnumFilterConverter.java:44-73).
fn enum_filter<T: Copy + Ord>(
    flag: &str,
    value: &str,
    pretty: &str,
    values: &[(&str, T)],
) -> Result<Option<BTreeSet<T>>, String> {
    if value.is_empty() {
        return Ok(None);
    }
    let mut included = BTreeSet::new();
    let mut excluded = BTreeSet::new();
    for item in value.split(',') {
        let (exclude, name) = match item.strip_prefix('-') {
            Some(name) => (true, name),
            None => (false, item),
        };
        let lower = name.to_ascii_lowercase();
        let Some((_, parsed)) = values.iter().find(|(known, _)| *known == lower) else {
            return Err(parse_error(
                flag,
                value,
                &format!("Invalid {pretty} filter '{item}' in the input '{value}'"),
            ));
        };
        if exclude {
            excluded.insert(*parsed);
        } else {
            included.insert(*parsed);
        }
    }
    if included.is_empty() {
        included = values.iter().map(|(_, item)| *item).collect();
    }
    for item in &excluded {
        included.remove(item);
    }
    if included.is_empty() {
        let mut capitalized = pretty.to_owned();
        capitalized[..1].make_ascii_uppercase();
        return Err(parse_error(
            flag,
            value,
            &format!("{capitalized} filter '{value}' definition cannot match any tests"),
        ));
    }
    Ok(Some(included))
}

const SIZES: [(&str, TestSize); 4] = [
    ("small", TestSize::Small),
    ("medium", TestSize::Medium),
    ("large", TestSize::Large),
    ("enormous", TestSize::Enormous),
];

const TIMEOUTS: [(&str, TestTimeout); 4] = [
    ("short", TestTimeout::Short),
    ("moderate", TestTimeout::Moderate),
    ("long", TestTimeout::Long),
    ("eternal", TestTimeout::Eternal),
];

impl TestOptions {
    /// `--[no]keep_going`/`-k` (runtime/KeepGoingOption.java) of the `test`
    /// command: only its effect on test actions is modelled (the native
    /// build itself stops at the first failing action), so `build` and
    /// `run` do not accept it.
    pub fn parse_keep_going(&mut self, arg: &str) -> Result<bool, String> {
        if arg == "-k" {
            self.keep_going = true;
            return Ok(true);
        }
        match crate::configuration::boolean_option(arg, "keep_going") {
            Some(value) => {
                self.keep_going = value?;
                Ok(true)
            }
            None => Ok(false),
        }
    }

    /// One test option: `Some(used_next)` when `arg` is one. Boolean options
    /// take `--NAME`, `--noNAME` and `--NAME=BOOL`.
    pub fn parse(&mut self, arg: &str, next: Option<&str>) -> Result<Option<bool>, String> {
        const BOOLEANS: [&str; 10] = [
            "build_tests_only",
            "verbose_test_summary",
            "test_verbose_timeout_warnings",
            "test_runner_fail_fast",
            "incompatible_check_sharding_support",
            "zip_undeclared_test_outputs",
            "print_relative_test_log_paths",
            "test_keep_going",
            "check_tests_up_to_date",
            "expand_test_suites",
        ];
        for name in BOOLEANS {
            if let Some(value) = crate::configuration::boolean_option(arg, name) {
                let value = value?;
                match name {
                    "build_tests_only" => self.build_tests_only = value,
                    "verbose_test_summary" => self.verbose_summary = value,
                    "test_verbose_timeout_warnings" => self.verbose_timeout_warnings = value,
                    "test_runner_fail_fast" => self.runner_fail_fast = value,
                    "incompatible_check_sharding_support" => self.check_sharding_support = value,
                    "zip_undeclared_test_outputs" => self.zip_undeclared_outputs = value,
                    // The native profile has no convenience symlinks and no
                    // --notest_keep_going cancellation, and does not model
                    // the up-to-date check or unexpanded test suites.
                    "print_relative_test_log_paths"
                    | "check_tests_up_to_date"
                    | "expand_test_suites"
                        if value != (name == "test_keep_going" || name == "expand_test_suites") =>
                    {
                        return Err(unsupported(arg));
                    }
                    "test_keep_going" if !value => return Err(unsupported(arg)),
                    _ => {}
                }
                return Ok(Some(false));
            }
        }
        // `-t`/`-t-` abbreviate --cache_test_results (TestConfiguration.java:169-187).
        if arg == "-t" {
            self.cache_test_results = TriState::Yes;
            return Ok(Some(false));
        }
        if arg == "-t-" {
            self.cache_test_results = TriState::No;
            return Ok(Some(false));
        }
        if arg == "--nocache_test_results" {
            self.cache_test_results = TriState::No;
            return Ok(Some(false));
        }
        if arg == "--cache_test_results" {
            self.cache_test_results = TriState::Yes;
            return Ok(Some(false));
        }
        // `--test_env` is a configuration option (N42, TestConfiguration;
        // crate::configuration) and reaches the test through the
        // configuration's test ActionEnvironment (EnvInputs::test_env).
        const VALUED: [&str; 17] = [
            "test_output",
            "test_summary",
            "cache_test_results",
            "test_arg",
            "test_filter",
            "test_timeout",
            "flaky_test_attempts",
            "runs_per_test",
            "test_sharding_strategy",
            "test_tag_filters",
            "test_size_filters",
            "test_timeout_filters",
            "test_lang_filters",
            "max_test_output_bytes",
            "local_test_jobs",
            "test_tmpdir",
            "test_strategy",
        ];
        for name in VALUED {
            let flag = format!("--{name}");
            let (value, used) = if arg == flag {
                // test_filter/test_arg take any separate value.
                (
                    next.ok_or_else(|| format!("{flag} requires a value"))?,
                    true,
                )
            } else if let Some(value) = arg.strip_prefix(&format!("{flag}=")) {
                (value, false)
            } else {
                continue;
            };
            self.set(name, &flag, value)?;
            return Ok(Some(used));
        }
        Ok(None)
    }

    fn set(&mut self, name: &str, flag: &str, value: &str) -> Result<(), String> {
        match name {
            "test_output" => {
                self.output = enum_value(
                    flag,
                    value,
                    "test output",
                    &[
                        ("summary", TestOutput::Summary),
                        ("errors", TestOutput::Errors),
                        ("all", TestOutput::All),
                        ("streamed", TestOutput::Streamed),
                    ],
                )?;
            }
            "test_summary" => {
                self.summary = enum_value(
                    flag,
                    value,
                    "test summary",
                    &[
                        ("short", TestSummaryFormat::Short),
                        ("short_uncached", TestSummaryFormat::ShortUncached),
                        ("terse", TestSummaryFormat::Terse),
                        ("detailed", TestSummaryFormat::Detailed),
                        ("detailed_uncached", TestSummaryFormat::DetailedUncached),
                        ("none", TestSummaryFormat::None),
                        ("testcase", TestSummaryFormat::Testcase),
                    ],
                )?;
            }
            "cache_test_results" => {
                // Converters.TriStateConverter (common/options/Converters.java:133-151).
                let lower = value.to_ascii_lowercase();
                self.cache_test_results = match lower.as_str() {
                    "auto" => TriState::Auto,
                    "true" | "1" | "yes" | "t" | "y" => TriState::Yes,
                    "false" | "0" | "no" | "f" | "n" => TriState::No,
                    _ => {
                        return Err(parse_error(
                            flag,
                            value,
                            &format!("'{lower}' is not a boolean"),
                        ));
                    }
                };
            }
            "test_arg" => self.args.push(value.to_owned()),
            "test_filter" => self.filter = Some(value.to_owned()),
            "test_timeout" => self.timeouts = parse_timeouts(flag, value)?,
            "flaky_test_attempts" => {
                // TestAttemptsConverter (exec/ExecutionOptions.java:579-610): a
                // regex@attempts form is a PerLabelOptions filter.
                if value == "default" {
                    self.flaky_attempts = None;
                } else if let Ok(attempts) = value.parse::<i64>() {
                    if attempts < 1 {
                        return Err(parse_error(
                            flag,
                            value,
                            &format!("'{value}' should be >= 1"),
                        ));
                    }
                    if attempts > 10 {
                        return Err(parse_error(
                            flag,
                            value,
                            &format!("'{value}' should be <= 10"),
                        ));
                    }
                    self.flaky_attempts = Some(attempts as u32);
                } else {
                    return Err(format!(
                        "{flag}={value}: per-label (regex@attempts) flaky test attempts are not supported by the native profile; use a number or `default`"
                    ));
                }
            }
            "runs_per_test" => match value.parse::<i64>() {
                Ok(runs) if runs >= 1 => self.runs_per_test = runs as u32,
                Ok(_) => {
                    return Err(parse_error(
                        flag,
                        value,
                        &format!("'{value}' should be >= 1"),
                    ));
                }
                Err(_) => {
                    return Err(format!(
                        "{flag}={value}: per-label (regex@runs) runs per test are not supported by the native profile; use a number"
                    ));
                }
            },
            "test_sharding_strategy" => {
                // ShardingStrategyConverter (TestConfiguration.java:530-560).
                self.sharding = match value {
                    "explicit" => ShardingStrategy::Explicit,
                    "disabled" => ShardingStrategy::Disabled,
                    _ => match value.strip_prefix("forced=").map(str::parse::<i32>) {
                        Some(Ok(count)) if count >= 0 => ShardingStrategy::Forced(count),
                        _ => {
                            return Err(parse_error(
                                flag,
                                value,
                                &format!(
                                    "Not a valid sharding strategy: '{value}' (should be explicit, disabled or forced=k)"
                                ),
                            ));
                        }
                    },
                };
            }
            "test_tag_filters" => self.tag_filters = comma_list(value),
            "test_size_filters" => {
                self.size_filters = enum_filter(flag, value, "test size", &SIZES)?;
            }
            "test_timeout_filters" => {
                self.timeout_filters = enum_filter(flag, value, "test timeout", &TIMEOUTS)?;
            }
            "test_lang_filters" => self.lang_filters = comma_list(value),
            "max_test_output_bytes" => {
                self.max_test_output_bytes = value
                    .parse::<i32>()
                    .map_err(|_| parse_error(flag, value, &format!("'{value}' is not an int")))?
                    as i64;
            }
            "local_test_jobs" => {
                self.local_test_jobs = if value == "auto" {
                    0
                } else {
                    value
                        .parse::<usize>()
                        .map_err(|_| unsupported(&format!("{flag}={value}")))?
                };
            }
            "test_tmpdir" => self.test_tmpdir = Some(value.to_owned()),
            "test_strategy" => match value {
                "" | "standalone" | "local" => self.exclusive = false,
                "exclusive" => self.exclusive = true,
                _ => return Err(unsupported(&format!("{flag}={value}"))),
            },
            _ => unreachable!("every valued test option is handled"),
        }
        Ok(())
    }

    /// TestConfiguration.getTestTimeout: the timeout of `timeout`.
    pub fn timeout_seconds(&self, timeout: TestTimeout) -> u64 {
        self.timeouts[timeout.index()]
    }
}

fn unsupported(arg: &str) -> String {
    format!(
        "unsupported test option {arg:?}: not modelled by the native profile (docs/TEST_COMMAND.md); options are never silently ignored"
    )
}

/// TestTimeout.TestTimeoutConverter (packages/TestTimeout.java:196-243).
fn parse_timeouts(flag: &str, value: &str) -> Result<[u64; 4], String> {
    let mut values: Vec<i64> = Vec::new();
    for (index, token) in value.splitn(6, ',').enumerate() {
        let _ = index;
        if !token.is_empty() || values.len() > 1 {
            values.push(
                token
                    .parse::<i32>()
                    .map_err(|_| parse_error(flag, value, &format!("'{value}' is not an int")))?
                    as i64,
            );
        }
    }
    let given: [i64; 4] = match values.as_slice() {
        [one] => [*one; 4],
        [a, b, c, d] => [*a, *b, *c, *d],
        _ => {
            return Err(parse_error(
                flag,
                value,
                "Invalid number of comma-separated entries",
            ));
        }
    };
    let mut timeouts = [0; 4];
    for (index, timeout) in TestTimeout::ALL.iter().enumerate() {
        timeouts[index] = if given[index] <= 0 {
            timeout.seconds()
        } else {
            given[index] as u64
        };
    }
    Ok(timeouts)
}

/// The test-related loading-time properties of a test rule
/// (TestTargetProperties, analysis/test/TestTargetProperties.java:81-135).
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct TestRuleInfo {
    pub rule_class: String,
    pub size: TestSize,
    pub timeout: TestTimeout,
    pub flaky: bool,
    /// `shard_count` (-1 when unspecified).
    pub shard_count: i32,
    pub local: bool,
    pub tags: Vec<String>,
}

impl TestRuleInfo {
    /// Rule.getRuleTags (packages/Rule.java:752-765): `tags` and the
    /// taggable attributes `size`, `timeout`, `flaky` and `local`.
    pub fn rule_tags(&self) -> BTreeSet<String> {
        let mut tags: BTreeSet<String> = self.tags.iter().cloned().collect();
        tags.insert(self.size.name().to_owned());
        tags.insert(self.timeout.name().to_owned());
        if self.flaky {
            tags.insert("flaky".to_owned());
        }
        if self.local {
            tags.insert("local".to_owned());
        }
        tags
    }

    fn has_tag(&self, tag: &str) -> bool {
        self.tags.iter().any(|item| item == tag)
    }

    /// TargetUtils.isExclusiveTestRule.
    pub fn exclusive(&self) -> bool {
        self.has_tag("exclusive")
    }

    /// TargetUtils.isExternalTestRule: never cached.
    pub fn external(&self) -> bool {
        self.has_tag("external")
    }

    /// TargetUtils.isLocalTestRule.
    pub fn is_local(&self) -> bool {
        self.local || self.has_tag("local")
    }

    /// TargetUtils.getRuleLanguage: the rule class before its last `_`.
    pub fn language(&self) -> &str {
        match self.rule_class.rfind('_') {
            Some(index) => &self.rule_class[..index],
            None => &self.rule_class,
        }
    }

    /// TestTargetProperties' execution info (:96-122, with
    /// --incompatible_exclusive_test_sandboxed): TargetUtils.getExecutionInfo
    /// tags, `local` for local tests, `no-remote-exec` for exclusive ones.
    pub fn execution_info(&self) -> BTreeMap<String, String> {
        let mut info: BTreeMap<String, String> = self
            .tags
            .iter()
            .filter(|tag| {
                [
                    "block-",
                    "requires-",
                    "no-",
                    "supports-",
                    "disable-",
                    "cpu:",
                    "resources:",
                ]
                .iter()
                .any(|prefix| tag.starts_with(prefix))
                    || *tag == "local"
                    || *tag == "worker-key-mnemonic"
            })
            .map(|tag| (tag.clone(), String::new()))
            .collect();
        if self.is_local() {
            info.insert("local".to_owned(), String::new());
        } else if self.exclusive() {
            info.insert("no-remote-exec".to_owned(), String::new());
        }
        if self.has_tag("no-testloasd") {
            info.insert("local".to_owned(), String::new());
            info.insert("no-testloasd".to_owned(), String::new());
        }
        info
    }
}

/// TestTargetUtils.sortTagsBySense (packages/TestTargetUtils.java:121-140).
pub fn sort_tags_by_sense(tags: &[String]) -> (BTreeSet<String>, BTreeSet<String>) {
    let mut required = BTreeSet::new();
    let mut excluded = BTreeSet::new();
    for tag in tags {
        if let Some(tag) = tag.strip_prefix('-') {
            excluded.insert(tag.to_owned());
        } else if let Some(tag) = tag.strip_prefix('+') {
            required.insert(tag.to_owned());
        } else if tag != "manual" {
            required.insert(tag.clone());
        }
    }
    (required, excluded)
}

/// TestTargetUtils.testMatchesFilters (:33-60).
pub fn matches_filters(
    test_tags: &BTreeSet<String>,
    required: &BTreeSet<String>,
    excluded: &BTreeSet<String>,
    must_match_all_positive: bool,
) -> bool {
    if excluded.iter().any(|tag| test_tags.contains(tag)) {
        return false;
    }
    if required.is_empty() {
        true
    } else if must_match_all_positive {
        required.iter().all(|tag| test_tags.contains(tag))
    } else {
        required.iter().any(|tag| test_tags.contains(tag))
    }
}

/// The tags a test_suite filters with (TestTargetUtils.filterTests and
/// TestSuiteImplicitTestsAccumulator.processRule): `tags` plus `size`.
pub fn suite_tags(info: &TestRuleInfo) -> BTreeSet<String> {
    let mut tags: BTreeSet<String> = info.tags.iter().cloned().collect();
    tags.insert(info.size.name().to_owned());
    tags
}

impl TestOptions {
    /// TestFilter (pkgcache/TestFilter.java:59-80, 128-170).
    pub fn accepts(&self, info: &TestRuleInfo) -> bool {
        if let Some(sizes) = &self.size_filters
            && !sizes.contains(&info.size)
        {
            return false;
        }
        if let Some(timeouts) = &self.timeout_filters
            && !timeouts.contains(&info.timeout)
        {
            return false;
        }
        if !self.tag_filters.is_empty() {
            // TargetUtils.tagFilter (packages/TargetUtils.java:366-386).
            let (required, excluded) = sort_tags_by_sense(&self.tag_filters);
            if !(required.is_empty() && excluded.is_empty())
                && !matches_filters(&info.rule_tags(), &required, &excluded, false)
            {
                return false;
            }
        }
        if !self.lang_filters.is_empty() {
            let mut required = BTreeSet::new();
            let mut excluded = BTreeSet::new();
            for lang in &self.lang_filters {
                match lang.strip_prefix('-') {
                    Some(lang) => excluded.insert(lang.to_owned()),
                    None => required.insert(lang.clone()),
                };
            }
            let language = info.language().to_owned();
            if !(required.is_empty() || required.contains(&language))
                || excluded.contains(&language)
            {
                return false;
            }
        }
        true
    }

    /// TestStrategy.getTestAttempts (analysis/test/TestStrategy.java:236-265).
    pub fn attempts(&self, info: &TestRuleInfo) -> u32 {
        match self.flaky_attempts {
            Some(attempts) => attempts,
            None if info.flaky => 3,
            None => 1,
        }
    }
}

/// TestStrategy.getTmpDirName (:301-308): the first 32 hex digits of the
/// SHA-256 Fingerprint of the executable's exec path, the shard and the run
/// number (util/Fingerprint.java: CodedOutputStream strings and int32s).
pub fn tmp_dir_name(executable_exec: &str, shard: u32, run: u32) -> String {
    use sha2::{Digest, Sha256};
    fn varint(mut value: u64, out: &mut Vec<u8>) {
        loop {
            if value < 0x80 {
                out.push(value as u8);
                return;
            }
            out.push((value as u8 & 0x7f) | 0x80);
            value >>= 7;
        }
    }
    let mut bytes = Vec::new();
    varint(executable_exec.len() as u64, &mut bytes);
    bytes.extend_from_slice(executable_exec.as_bytes());
    varint(u64::from(shard), &mut bytes);
    varint(u64::from(run), &mut bytes);
    let digest = Sha256::digest(&bytes);
    digest
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>()[..32]
        .to_owned()
}

/// TestRunnerAction.getTestSuffix/getTestName (analysis/test/TestRunnerAction.java:833-858).
pub fn test_name(label: &str, shard: u32, shards: u32, run: u32, runs: u32) -> String {
    let suffix = if shards > 1 && runs > 1 {
        format!(
            "(shard {} of {shards}, run {} of {runs})",
            shard + 1,
            run + 1
        )
    } else if shards > 1 {
        format!("(shard {} of {shards})", shard + 1)
    } else if runs > 1 {
        format!("(run {} of {runs})", run + 1)
    } else {
        String::new()
    };
    if suffix.is_empty() {
        label.to_owned()
    } else {
        format!("{label} {suffix}")
    }
}

/// The directory of one test action below the testlogs root
/// (TestActionBuilder.createTestAction, analysis/test/TestActionBuilder.java:398-414).
pub fn test_directory(name: &str, shard: u32, shards: u32, run: u32, runs: u32) -> PathBuf {
    let shard_runs = shards.max(1);
    let shard_dir = (shard_runs > 1).then(|| format!("shard_{}_of_{shards}", shard + 1));
    let target = PathBuf::from(name);
    if runs > 1 {
        let run_dir = format!("run_{}_of_{runs}", run + 1);
        match shard_dir {
            None => target.join(run_dir),
            Some(shard_dir) => target.join(format!("{shard_dir}_{run_dir}")),
        }
    } else {
        match shard_dir {
            None => target,
            Some(shard_dir) => target.join(shard_dir),
        }
    }
}

/// What the test environment is computed from.
pub struct EnvInputs<'a> {
    /// The configuration's action environment (`--action_env`, strict PATH;
    /// BuildConfigurationValue.getActionEnvironment, N42).
    pub action_env: &'a crate::action_env::ActionEnvironment,
    /// The configuration's test environment (`--test_env`;
    /// BuildConfigurationValue.getTestActionEnvironment, N42).
    pub test_env: &'a crate::action_env::ActionEnvironment,
    pub client_env: &'a BTreeMap<String, String>,
    pub options: &'a TestOptions,
    /// RunEnvironmentInfo.environment and inherited_environment.
    pub target_env: &'a [(String, String)],
    pub target_inherited: &'a [String],
    pub label: &'a str,
    pub size: TestSize,
    pub timeout_seconds: u64,
    /// The runfiles prefix (the workspace name, `_main`).
    pub workspace: &'a str,
    /// The executable's runfiles path (TEST_BINARY).
    pub binary: &'a str,
    /// The runfiles directory relative to the execution root.
    pub runfiles_dir: &'a str,
    /// The test's tmp directory (relative to the execution root, or absolute).
    pub tmp_dir: &'a str,
    /// The action's testlogs directory relative to the execution root.
    pub base_dir: &'a str,
    pub shard: u32,
    pub total_shards: u32,
    pub run: u32,
    pub total_runs: u32,
    pub runfiles_enabled: bool,
}

/// ActionEnvironment.resolve (actions/ActionEnvironment.java:113-122): the
/// fixed values, then the inherited names the client environment sets.
fn resolve_into(
    env: &mut BTreeMap<String, String>,
    environment: &crate::action_env::ActionEnvironment,
    client_env: &BTreeMap<String, String>,
) {
    env.extend(environment.fixed.clone());
    for name in &environment.inherited {
        if let Some(value) = client_env.get(name) {
            env.insert(name.clone(), value.clone());
        }
    }
}

/// TestPolicy.computeTestEnvironment (exec/TestPolicy.java:56-101) with
/// StandaloneTestStrategy.DEFAULT_LOCAL_POLICY (exec/StandaloneTestStrategy.java:75-87)
/// and TestRunnerAction.setupEnvVariables (:745-830).
pub fn test_environment(inputs: &EnvInputs<'_>) -> BTreeMap<String, String> {
    let mut env = BTreeMap::new();
    let put = |env: &mut BTreeMap<String, String>, name: &str, value: &str| {
        env.insert(name.to_owned(), value.to_owned());
    };
    put(&mut env, "TZ", "UTC");
    for name in [
        "TEST_SRCDIR",
        "JAVA_RUNFILES",
        "PYTHON_RUNFILES",
        "RUNFILES_DIR",
    ] {
        put(&mut env, name, inputs.runfiles_dir);
    }
    put(&mut env, "TEST_TMPDIR", inputs.tmp_dir);
    put(&mut env, "RUN_UNDER_RUNFILES", "1");
    // The configuration's action environment, then its test environment
    // (TestPolicy.java:88-92; both enter TestRunnerAction.computeKey,
    // :542-543, through the keyed environment).
    resolve_into(&mut env, inputs.action_env, inputs.client_env);
    resolve_into(&mut env, inputs.test_env, inputs.client_env);
    // The rule's RunEnvironmentInfo (TestRunnerAction.getExtraTestEnv).
    for (name, value) in inputs.target_env {
        put(&mut env, name, value);
    }
    for name in inputs.target_inherited {
        if let Some(value) = inputs.client_env.get(name) {
            put(&mut env, name, value);
        }
    }
    put(&mut env, "TEST_TARGET", inputs.label);
    put(&mut env, "TEST_SIZE", inputs.size.name());
    put(
        &mut env,
        "TEST_TIMEOUT",
        &inputs.timeout_seconds.to_string(),
    );
    put(&mut env, "TEST_WORKSPACE", inputs.workspace);
    put(&mut env, "TEST_BINARY", inputs.binary);
    if inputs.total_runs > 1 {
        env.entry("TEST_RANDOM_SEED".to_owned())
            .or_insert_with(|| (inputs.run + 1).to_string());
        env.entry("TEST_RUN_NUMBER".to_owned())
            .or_insert_with(|| (inputs.run + 1).to_string());
    }
    if let Some(filter) = &inputs.options.filter {
        put(&mut env, "TESTBRIDGE_TEST_ONLY", filter);
    }
    if inputs.options.runner_fail_fast {
        put(&mut env, "TESTBRIDGE_TEST_RUNNER_FAIL_FAST", "1");
    }
    let base = |name: &str| format!("{}/{name}", inputs.base_dir);
    put(
        &mut env,
        "TEST_WARNINGS_OUTPUT_FILE",
        &base("test.warnings"),
    );
    put(
        &mut env,
        "TEST_UNUSED_RUNFILES_LOG_FILE",
        &base("test.unused_runfiles_log"),
    );
    put(
        &mut env,
        "TEST_LOGSPLITTER_OUTPUT_FILE",
        &base("test.raw_splitlogs/test.splitlogs"),
    );
    if inputs.options.zip_undeclared_outputs {
        put(
            &mut env,
            "TEST_UNDECLARED_OUTPUTS_ZIP",
            &base("test.outputs/outputs.zip"),
        );
    }
    put(
        &mut env,
        "TEST_UNDECLARED_OUTPUTS_DIR",
        &base("test.outputs"),
    );
    put(
        &mut env,
        "TEST_UNDECLARED_OUTPUTS_MANIFEST",
        &base("test.outputs_manifest/MANIFEST"),
    );
    put(
        &mut env,
        "TEST_UNDECLARED_OUTPUTS_ANNOTATIONS",
        &base("test.outputs_manifest/ANNOTATIONS"),
    );
    put(
        &mut env,
        "TEST_UNDECLARED_OUTPUTS_ANNOTATIONS_DIR",
        &base("test.outputs_manifest"),
    );
    put(
        &mut env,
        "TEST_PREMATURE_EXIT_FILE",
        &base("test.exited_prematurely"),
    );
    put(
        &mut env,
        "TEST_INFRASTRUCTURE_FAILURE_FILE",
        &base("test.infrastructure_failure"),
    );
    // isSharded: more than one shard (TestRunnerAction.java:266-268, 999-1001).
    if inputs.total_shards > 1 {
        put(&mut env, "TEST_SHARD_INDEX", &inputs.shard.to_string());
        put(
            &mut env,
            "TEST_TOTAL_SHARDS",
            &inputs.total_shards.to_string(),
        );
        put(&mut env, "TEST_SHARD_STATUS_FILE", &base("test.shard"));
    }
    put(&mut env, "XML_OUTPUT_FILE", &base("test.xml"));
    if !inputs.runfiles_enabled {
        put(&mut env, "RUNFILES_MANIFEST_ONLY", "1");
    }
    env
}

/// BlazeTestStatus (src/main/protobuf/test_status.proto:34-44).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum TestStatus {
    NoStatus = 0,
    Passed = 1,
    Flaky = 2,
    Timeout = 3,
    Failed = 4,
    Incomplete = 5,
    FailedToBuild = 7,
}

impl TestStatus {
    /// The enum name (TestSummaryPrinter.statusString replaces `_` by a space).
    pub fn name(self) -> &'static str {
        match self {
            TestStatus::NoStatus => "NO_STATUS",
            TestStatus::Passed => "PASSED",
            TestStatus::Flaky => "FLAKY",
            TestStatus::Timeout => "TIMEOUT",
            TestStatus::Failed => "FAILED",
            TestStatus::Incomplete => "INCOMPLETE",
            TestStatus::FailedToBuild => "FAILED_TO_BUILD",
        }
    }

    /// TestResult.isBlazeTestStatusPassed.
    pub fn passed(self) -> bool {
        matches!(self, TestStatus::Passed | TestStatus::Flaky)
    }

    /// TestSummary.getSortKey (runtime/TestSummary.java:557-559).
    fn sort_key(self) -> i32 {
        if self == TestStatus::Passed {
            -1
        } else {
            self as i32
        }
    }
}

/// One test action's result (TestResultData): status, attempt times and
/// logs, for aggregation.
#[derive(Clone, Debug)]
pub struct RunResult {
    pub shard: u32,
    pub status: TestStatus,
    /// Whether the result came from the test cache.
    pub cached: bool,
    /// TestResultData.test_times: one per attempt, in milliseconds.
    pub test_times: Vec<u64>,
    pub passed_log: Option<PathBuf>,
    pub failed_logs: Vec<PathBuf>,
    /// The parsed test.xml (detailed summaries only, TestStrategy.parseTestResult).
    pub test_case: Option<TestCase>,
}

/// TestSummary (runtime/TestSummary.java), aggregated by
/// TestResultAggregator.incrementalAnalyze (runtime/TestResultAggregator.java:164-268).
#[derive(Clone, Debug)]
pub struct TestSummary {
    pub label: String,
    pub status: TestStatus,
    pub num_cached: usize,
    pub action_ran: bool,
    pub test_times: Vec<u64>,
    pub passed_logs: Vec<PathBuf>,
    pub failed_logs: Vec<PathBuf>,
    pub warnings: Vec<String>,
    pub was_unreported_wrong_size: bool,
    pub cases: TestCases,
}

impl TestSummary {
    /// Aggregate the runs of one test target (runs_per_test_detects_flakes
    /// is false: statuses aggregate by enum order).
    pub fn aggregate(
        label: &str,
        timeout: TestTimeout,
        results: &[RunResult],
        verbose_timeout_warnings: bool,
    ) -> Self {
        let mut summary = TestSummary {
            label: label.to_owned(),
            status: TestStatus::NoStatus,
            num_cached: 0,
            action_ran: false,
            test_times: Vec::new(),
            passed_logs: Vec::new(),
            failed_logs: Vec::new(),
            warnings: Vec::new(),
            was_unreported_wrong_size: false,
            cases: TestCases::default(),
        };
        for result in results {
            summary.cases.collect(result.test_case.as_ref());
            if result.cached {
                summary.num_cached += 1;
            } else {
                summary.action_ran = true;
            }
            if result.status > summary.status {
                summary.status = result.status;
            }
            summary.passed_logs.extend(result.passed_log.clone());
            summary
                .failed_logs
                .extend(result.failed_logs.iter().cloned());
            summary.test_times.extend(result.test_times.iter().copied());
            // shouldEmitTestSizeWarningInSummary (:301-344) with the summary
            // status after this result.
            if summary.status == TestStatus::Passed {
                let max = result.test_times.iter().copied().max().unwrap_or(0);
                let seconds = max / 1000;
                if !timeout.in_range_fuzzy(seconds) {
                    let expected = TestTimeout::suggested(seconds);
                    if verbose_timeout_warnings {
                        let mut warning = format!(
                            "{label}: Test execution time ({}s excluding execution overhead) outside of range for {} tests. Consider setting timeout=\"{}\"",
                            tenths(max),
                            timeout.name().to_ascii_uppercase(),
                            expected.map(TestTimeout::name).unwrap_or("null")
                        );
                        if let Some(size) = expected.and_then(TestSize::for_timeout) {
                            warning.push_str(&format!(" or size=\"{}\"", size.name()));
                        }
                        warning.push('.');
                        summary.warnings.push(warning);
                    } else {
                        summary.was_unreported_wrong_size = true;
                    }
                }
            }
        }
        summary
    }

    fn total_runs(&self) -> usize {
        self.test_times.len()
    }

    fn num_uncached(&self) -> i64 {
        self.total_runs() as i64 - self.num_cached as i64
    }

    /// TestSummary.compareTo (runtime/TestSummary.java:561-573).
    pub fn compare(&self, other: &Self) -> std::cmp::Ordering {
        (other.num_cached > 0)
            .cmp(&(self.num_cached > 0))
            .then(self.num_uncached().cmp(&other.num_uncached()))
            .then(self.status.sort_key().cmp(&other.status.sort_key()))
            .then(self.label.cmp(&other.label))
    }
}

/// TestSummaryPrinter.timeInSec (runtime/TestSummaryPrinter.java:260-266).
/// `%.1f` rounds HALF_UP in java.util.Formatter (on the decimal value).
fn time_in_sec(milliseconds: u64) -> String {
    format!("{}s", tenths(milliseconds))
}

/// `milliseconds / 1000.0` with one decimal, rounded HALF_UP like Java.
fn tenths(milliseconds: u64) -> String {
    let tenths = (milliseconds + 50) / 100;
    format!("{}.{}", tenths / 10, tenths % 10)
}

/// TestSummaryPrinter.getAttemptSummary (:268-281).
fn attempt_summary(summary: &TestSummary) -> String {
    let attempts = summary.passed_logs.len() + summary.failed_logs.len();
    if attempts > 1 {
        if summary.status == TestStatus::Flaky {
            return format!(
                ", failed in {} out of {attempts}",
                summary.failed_logs.len()
            );
        }
        if matches!(summary.status, TestStatus::Timeout | TestStatus::Failed) {
            return format!(" in {} out of {attempts}", summary.failed_logs.len());
        }
    }
    String::new()
}

/// TestSummaryPrinter.getCacheMessage (:283-296).
fn cache_message(summary: &TestSummary) -> String {
    if summary.num_cached == 0
        || matches!(
            summary.status,
            TestStatus::Incomplete | TestStatus::NoStatus | TestStatus::FailedToBuild
        )
    {
        String::new()
    } else if summary.num_cached == summary.total_runs() {
        "(cached) ".to_owned()
    } else {
        format!("({}/{} cached) ", summary.num_cached, summary.total_runs())
    }
}

/// TestSummaryPrinter.getTimeSummary (:298-323).
fn time_summary(summary: &TestSummary) -> String {
    if summary.test_times.is_empty()
        || matches!(
            summary.status,
            TestStatus::NoStatus | TestStatus::FailedToBuild
        )
    {
        return String::new();
    }
    if summary.test_times.len() == 1 {
        return format!(" in {}", time_in_sec(summary.test_times[0]));
    }
    let min = *summary.test_times.iter().min().unwrap_or(&0);
    let max = *summary.test_times.iter().max().unwrap_or(&0);
    let sum: u64 = summary.test_times.iter().sum();
    let squares: f64 = summary
        .test_times
        .iter()
        .map(|value| (*value as f64) * (*value as f64))
        .sum();
    let count = summary.test_times.len() as f64;
    let mean = sum as f64 / count;
    let deviation = ((squares - sum as f64 * mean) / count).sqrt();
    let max_time = time_in_sec(max);
    format!(
        " in {max_time}\n  Stats over {} runs: max = {max_time}, min = {}, avg = {}, dev = {}",
        summary.test_times.len(),
        time_in_sec(min),
        time_in_sec(mean as u64),
        time_in_sec(deviation as u64)
    )
}

/// TestSummaryPrinter.print (:120-191).
fn print_summary_line(summary: &TestSummary, verbose: bool, show_cases: bool, out: &mut String) {
    let status = summary.status;
    if !verbose && status == TestStatus::FailedToBuild {
        return;
    }
    let message = format!(
        "{}{}",
        cache_message(summary),
        status.name().replace('_', " ")
    );
    let width = 78usize.saturating_sub(message.chars().count());
    let mut line = summary.label.clone();
    while line.chars().count() < width {
        line.push(' ');
    }
    out.push_str(&line);
    out.push(' ');
    out.push_str(&message);
    if verbose {
        out.push_str(&attempt_summary(summary));
        out.push_str(&time_summary(summary));
    }
    out.push('\n');
    if show_cases {
        for case in summary.cases.passed.iter().chain(&summary.cases.skipped) {
            print_test_case(case, out);
        }
        if summary.status == TestStatus::Failed {
            if summary.cases.full == Some(false) {
                out.push_str("    (individual test case information not available) \n");
            } else {
                for case in &summary.cases.failed {
                    if case.status != "PASSED" {
                        print_test_case(case, out);
                    }
                }
                if summary.cases.full != Some(true) {
                    out.push_str("    (some shards did not report details, list of failed test cases incomplete)\n");
                }
            }
        }
        return;
    }
    for warning in &summary.warnings {
        out.push_str(&format!("  WARNING: {warning}\n"));
    }
    for path in &summary.failed_logs {
        if path.exists() {
            out.push_str(&format!("  {}\n", path.display()));
        }
    }
}

/// TerminalTestResultNotifier.notify (runtime/TerminalTestResultNotifier.java:181-246)
/// and printStats (:276-347): the summary text (stdout).
pub fn summary_text(
    summaries: &[TestSummary],
    executed_targets: usize,
    options: &TestOptions,
) -> String {
    let mut out = String::new();
    let mut sorted: Vec<&TestSummary> = summaries.iter().collect();
    sorted.sort_by(|a, b| a.compare(b));
    let format = options.summary;
    let show_all = matches!(
        format,
        TestSummaryFormat::Detailed
            | TestSummaryFormat::DetailedUncached
            | TestSummaryFormat::Short
            | TestSummaryFormat::ShortUncached
    );
    let show_no_status = matches!(
        format,
        TestSummaryFormat::Detailed | TestSummaryFormat::DetailedUncached
    );
    let show_cached = matches!(
        format,
        TestSummaryFormat::Detailed | TestSummaryFormat::Short
    );
    if !matches!(
        format,
        TestSummaryFormat::None | TestSummaryFormat::Testcase
    ) {
        let mut failed_to_build = 0;
        for summary in &sorted {
            if !show_all
                && (summary.status == TestStatus::Passed
                    || (!show_no_status && summary.status == TestStatus::NoStatus))
            {
                continue;
            }
            if summary.status == TestStatus::FailedToBuild {
                if failed_to_build == 5 {
                    out.push_str("(Skipping other failed to build tests)\n");
                }
                failed_to_build += 1;
                if failed_to_build > 5 {
                    continue;
                }
            }
            if !show_cached && summary.status == TestStatus::Passed && !summary.action_ran {
                continue;
            }
            print_summary_line(summary, options.verbose_summary, show_no_status, &mut out);
        }
    }
    if matches!(
        format,
        TestSummaryFormat::Detailed
            | TestSummaryFormat::DetailedUncached
            | TestSummaryFormat::Testcase
    ) {
        let total: usize = summaries.iter().map(|summary| summary.cases.total).sum();
        let unknown: usize = summaries.iter().map(|summary| summary.cases.unknown).sum();
        let failed: usize = summaries
            .iter()
            .map(|summary| summary.cases.failed.len())
            .sum();
        let skipped: usize = summaries
            .iter()
            .map(|summary| summary.cases.skipped.len())
            .sum();
        let passing = total - failed - unknown - skipped;
        out.push_str(&format!(
            "Test cases: finished with {passing} passing, {skipped} skipped and {failed} failing out of {total} test cases"
        ));
        if unknown != 0 {
            out.push_str(" (some targets did not have test case information)");
        }
        out.push('\n');
    }
    let mut pass = 0;
    let mut no_status = 0;
    let mut failed_to_build = 0;
    let mut failed_locally = 0;
    let mut wrong_size = false;
    for summary in summaries {
        if summary.status.passed() {
            pass += 1;
        } else if summary.status == TestStatus::NoStatus {
            no_status += 1;
        } else if summary.status == TestStatus::FailedToBuild {
            failed_to_build += 1;
        } else {
            failed_locally += 1;
        }
        wrong_size |= summary.was_unreported_wrong_size;
    }
    let mut results = Vec::new();
    if pass == 1 {
        results.push("1 test passes".to_owned());
    } else if pass > 0 {
        results.push(format!("{pass} tests pass"));
    }
    let mut add = |singular: &str, plural: &str, message: &str, count: usize| {
        if count > 0 {
            results.push(format!(
                "{count} {} {message}",
                if count == 1 { singular } else { plural }
            ));
        }
    };
    add("fails", "fail", "to build", failed_to_build);
    add("fails", "fail", "locally", failed_locally);
    add("was", "were", "skipped", no_status);
    out.push_str(&format!(
        "\nExecuted {executed_targets} out of {} {}: {}.\n",
        summaries.len(),
        if summaries.len() == 1 {
            "test"
        } else {
            "tests"
        },
        english_and(&results)
    ));
    if wrong_size {
        out.push_str("There were tests whose specified size is too big. Use the --test_verbose_timeout_warnings command line option to see which ones these are.\n");
    }
    out
}

/// TestLogHelper.HEADER_DELIMITER (exec/TestLogHelper.java:33-34).
pub const HEADER_DELIMITER: &str =
    "-----------------------------------------------------------------------------";

/// TestLogHelper.shouldOutputTestLog (:40-43).
pub fn should_output_log(output: TestOutput, passed: bool) -> bool {
    output == TestOutput::All || (!passed && output == TestOutput::Errors)
}

/// The bytes after the header delimiter line (FilterTestHeaderOutputStream),
/// or the whole log when there is none (streamTestLog, :163-175).
pub fn filter_header(log: &[u8]) -> &[u8] {
    let mut start = 0;
    while start < log.len() {
        let end = log[start..]
            .iter()
            .position(|byte| *byte == b'\n')
            .map(|offset| start + offset);
        let Some(end) = end else {
            break;
        };
        if &log[start..end] == HEADER_DELIMITER.as_bytes() {
            return &log[end + 1..];
        }
        start = end + 1;
    }
    log
}

/// TestLogHelper.writeTestLog (:54-80).
pub fn test_log_block(log_path: &Path, test_name: &str, max_bytes: i64) -> Vec<u8> {
    let mut out = format!("==================== Test output for {test_name}:\n").into_bytes();
    let bytes = std::fs::read(log_path).unwrap_or_default();
    if max_bytes < 0 || bytes.len() as i64 <= max_bytes {
        out.extend_from_slice(filter_header(&bytes));
    } else {
        out.extend_from_slice(
            format!(
                "Test log too large ({} > {max_bytes}), skipping...\n",
                bytes.len()
            )
            .as_bytes(),
        );
    }
    out.extend_from_slice(
        b"================================================================================\n",
    );
    out
}

/// TerminationStatus.toShortString (shell/TerminationStatus.java:222-226)
/// of a test's exit code (128 + signal for a signaled process).
pub fn termination_short(exit_code: i32) -> String {
    const SIGNALS: [&str; 32] = [
        "",
        "Hangup",
        "Interrupt",
        "Quit",
        "Illegal instruction",
        "Trace/breakpoint trap",
        "Aborted",
        "Bus error",
        "Floating point exception",
        "Killed",
        "User defined signal 1",
        "Segmentation fault",
        "User defined signal 2",
        "Broken pipe",
        "Alarm clock",
        "Terminated",
        "Stack fault",
        "Child exited",
        "Continued",
        "Stopped (signal)",
        "Stopped",
        "Stopped (tty input)",
        "Stopped (tty output)",
        "Urgent I/O condition",
        "CPU time limit exceeded",
        "File size limit exceeded",
        "Virtual timer expired",
        "Profiling timer expired",
        "Window changed",
        "I/O possible",
        "Power failure",
        "Bad system call",
    ];
    if !(129..=191).contains(&exit_code) {
        return format!("Exit {exit_code}");
    }
    let signal = (exit_code - 128) as usize;
    match SIGNALS.get(signal) {
        Some(name) if signal > 0 => (*name).to_owned(),
        _ => format!("Signal {signal}"),
    }
}

/// TestCase (src/main/protobuf/test_status.proto:46-80) as parsed by
/// TestXmlOutputParser (exec/TestXmlOutputParser.java).
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct TestCase {
    pub name: String,
    pub class_name: String,
    /// TEST_SUITE, TEST_CASE or TEST_DECORATOR.
    pub kind: String,
    /// PASSED, FAILED, ERROR or SKIPPED.
    pub status: String,
    pub run: bool,
    pub duration_millis: Option<i64>,
    pub children: Vec<TestCase>,
}

/// A minimal XML pull tokenizer for test.xml: start/end elements with
/// attributes; text, comments, CDATA, processing instructions and DOCTYPE
/// are skipped.
enum XmlEvent {
    Start(String, Vec<(String, String)>, bool),
    End(String),
}

fn xml_unescape(text: &str) -> Option<String> {
    let mut out = String::new();
    let mut rest = text;
    while let Some(index) = rest.find('&') {
        out.push_str(&rest[..index]);
        let end = rest[index..].find(';')? + index;
        let entity = &rest[index + 1..end];
        let decoded = match entity {
            "lt" => '<',
            "gt" => '>',
            "amp" => '&',
            "quot" => '"',
            "apos" => '\'',
            _ => {
                let number = if let Some(hex) = entity.strip_prefix("#x") {
                    u32::from_str_radix(hex, 16).ok()?
                } else {
                    entity.strip_prefix('#')?.parse::<u32>().ok()?
                };
                char::from_u32(number)?
            }
        };
        out.push(decoded);
        rest = &rest[end + 1..];
    }
    out.push_str(rest);
    Some(out)
}

fn xml_events(text: &str) -> Option<Vec<XmlEvent>> {
    let mut events = Vec::new();
    let mut rest = text;
    while let Some(index) = rest.find('<') {
        rest = &rest[index..];
        if let Some(after) = rest.strip_prefix("<!--") {
            rest = &after[after.find("-->")? + 3..];
        } else if let Some(after) = rest.strip_prefix("<![CDATA[") {
            rest = &after[after.find("]]>")? + 3..];
        } else if rest.starts_with("<?") || rest.starts_with("<!") {
            rest = &rest[rest.find('>')? + 1..];
        } else if let Some(after) = rest.strip_prefix("</") {
            let end = after.find('>')?;
            events.push(XmlEvent::End(after[..end].trim().to_owned()));
            rest = &after[end + 1..];
        } else {
            let body = &rest[1..];
            // The tag ends at the first `>` outside a quoted value.
            let mut quote = None;
            let mut end = None;
            for (offset, character) in body.char_indices() {
                match (quote, character) {
                    (Some(q), c) if c == q => quote = None,
                    (Some(_), _) => {}
                    (None, '"' | '\'') => quote = Some(character),
                    (None, '>') => {
                        end = Some(offset);
                        break;
                    }
                    _ => {}
                }
            }
            let end = end?;
            let mut tag = &body[..end];
            let empty = tag.ends_with('/');
            if empty {
                tag = &tag[..tag.len() - 1];
            }
            let name_end = tag.find(|c: char| c.is_whitespace()).unwrap_or(tag.len());
            let name = tag[..name_end].to_owned();
            let mut attributes = Vec::new();
            let mut attrs = tag[name_end..].trim_start();
            while !attrs.is_empty() {
                let equals = attrs.find('=')?;
                let key = attrs[..equals].trim().to_owned();
                let value_part = attrs[equals + 1..].trim_start();
                let quote = value_part.chars().next()?;
                if quote != '"' && quote != '\'' {
                    return None;
                }
                let close = value_part[1..].find(quote)? + 1;
                attributes.push((key, xml_unescape(&value_part[1..close])?));
                attrs = value_part[close + 1..].trim_start();
            }
            events.push(XmlEvent::Start(name, attributes, empty));
            rest = &body[end + 1..];
        }
    }
    Some(events)
}

/// The local name of an element (namespace prefixes are ignored).
fn local_name(name: &str) -> &str {
    name.rsplit(':').next().unwrap_or(name)
}

/// TestXmlOutputParser.parseTimeToMillis (:142-151).
fn time_to_millis(value: &str) -> Option<i64> {
    if value.contains(['.', 'e', 'E']) {
        value
            .parse::<f32>()
            .ok()
            .map(|seconds| (seconds * 1000.0).round() as i64)
    } else {
        value.parse::<i64>().ok()
    }
}

/// TestXmlOutputParser.parseXmlIntoTestResult (:51-99): the first
/// `testsuites`/`testsuite` element as a TEST_SUITE tree; `None` when the
/// file is not parseable.
pub fn parse_test_xml(text: &str) -> Option<TestCase> {
    let events = xml_events(text)?;
    let mut position = 0;
    while position < events.len() {
        if let XmlEvent::Start(name, attributes, empty) = &events[position]
            && matches!(local_name(name), "testsuites" | "testsuite")
        {
            let (case, _) = parse_element(&events, position, local_name(name), attributes, *empty)?;
            return Some(case);
        }
        position += 1;
    }
    None
}

/// Parse the element starting at `position` (its start event); returns it
/// and the position after its end event.
fn parse_element(
    events: &[XmlEvent],
    position: usize,
    element: &str,
    attributes: &[(String, String)],
    empty: bool,
) -> Option<(TestCase, usize)> {
    let mut case = TestCase {
        name: String::new(),
        class_name: String::new(),
        kind: match element {
            "testcase" => "TEST_CASE",
            "testdecorator" => "TEST_DECORATOR",
            _ => "TEST_SUITE",
        }
        .to_owned(),
        status: "PASSED".to_owned(),
        run: true,
        duration_millis: None,
        children: Vec::new(),
    };
    for (key, value) in attributes {
        match (element, local_name(key)) {
            (_, "name") => case.name = value.clone(),
            ("testcase" | "testdecorator", "classname") => case.class_name = value.clone(),
            (_, "time") => case.duration_millis = Some(time_to_millis(value)?),
            ("testcase", "status") if value == "notrun" => case.run = false,
            ("testcase", "status") if value == "run" => case.run = true,
            _ => {}
        }
    }
    let mut failures = 0;
    let mut errors = 0;
    let mut skipped = false;
    let mut next = position + 1;
    if !empty {
        loop {
            match events.get(next)? {
                XmlEvent::Start(name, child_attributes, child_empty) => {
                    let child = local_name(name);
                    match child {
                        "testsuite" | "testcase" | "testdecorator" => {
                            let (parsed, after) =
                                parse_element(events, next, child, child_attributes, *child_empty)?;
                            case.children.push(parsed);
                            next = after;
                        }
                        _ => {
                            match child {
                                "failure" => failures += 1,
                                "error" => errors += 1,
                                "skipped" => skipped = true,
                                _ => {}
                            }
                            next = skip_element(events, next, *child_empty)?;
                        }
                    }
                }
                XmlEvent::End(name) => {
                    if local_name(name) != element {
                        return None;
                    }
                    next += 1;
                    break;
                }
            }
        }
    }
    for child in &case.children {
        match child.status.as_str() {
            "ERROR" => errors += 1,
            "FAILED" => failures += 1,
            _ => {}
        }
    }
    case.status = if errors > 0 {
        "ERROR"
    } else if failures > 0 {
        "FAILED"
    } else if skipped {
        "SKIPPED"
    } else {
        "PASSED"
    }
    .to_owned();
    Some((case, next))
}

fn skip_element(events: &[XmlEvent], position: usize, empty: bool) -> Option<usize> {
    if empty {
        return Some(position + 1);
    }
    let mut depth = 1;
    let mut next = position + 1;
    while depth > 0 {
        match events.get(next)? {
            XmlEvent::Start(_, _, true) => {}
            XmlEvent::Start(..) => depth += 1,
            XmlEvent::End(_) => depth -= 1,
        }
        next += 1;
    }
    Some(next)
}

/// The test cases of a summary (TestSummary.Builder.collectTestCases,
/// runtime/TestSummary.java:183-226).
#[derive(Clone, Debug, Default)]
pub struct TestCases {
    pub total: usize,
    pub unknown: usize,
    pub passed: Vec<TestCase>,
    pub skipped: Vec<TestCase>,
    pub failed: Vec<TestCase>,
    /// FailedTestCasesStatus: `Some(true)` FULL, `Some(false)` NOT_AVAILABLE.
    pub full: Option<bool>,
}

impl TestCases {
    pub fn collect(&mut self, case: Option<&TestCase>) {
        match case {
            None => {
                self.full = Some(false);
                self.total += 1;
                self.unknown += 1;
            }
            Some(case) => {
                self.full = Some(true);
                self.total += self.traverse(case);
            }
        }
    }

    fn traverse(&mut self, case: &TestCase) -> usize {
        if !case.children.is_empty() {
            return case.children.iter().map(|child| self.traverse(child)).sum();
        }
        if case.kind != "TEST_CASE" || !case.run {
            return 0;
        }
        match case.status.as_str() {
            "PASSED" => self.passed.push(case.clone()),
            "SKIPPED" => self.skipped.push(case.clone()),
            _ => self.failed.push(case.clone()),
        }
        1
    }
}

/// TestSummaryPrinter.printTestCase (runtime/TestSummaryPrinter.java:229-254).
fn print_test_case(case: &TestCase, out: &mut String) {
    let time = case
        .duration_millis
        .map(|millis| format!(" ({})", time_in_sec(millis.max(0) as u64)))
        .unwrap_or_default();
    out.push_str(&format!(
        "    {:<8}{}.{}{time}\n",
        case.status, case.class_name, case.name
    ));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tmp_dir_name_matches_bazel_fingerprint() {
        // Observed in the N23g design probe of genuine Bazel 9.3.0.
        assert_eq!(
            tmp_dir_name("bazel-out/k8-fastbuild/bin/pkg/pass_test.sh", 0, 0),
            "6d6ad32491d201b7db1194ee8bd5b1f5"
        );
    }

    #[test]
    fn timeouts_and_fuzzy_ranges() {
        assert!(TestTimeout::Moderate.in_range_fuzzy(30));
        assert!(!TestTimeout::Moderate.in_range_fuzzy(0));
        assert!(TestTimeout::Short.in_range_fuzzy(0));
        assert_eq!(TestTimeout::suggested(0), Some(TestTimeout::Short));
        assert_eq!(TestTimeout::suggested(50), Some(TestTimeout::Moderate));
        assert_eq!(parse_timeouts("--test_timeout", "2").unwrap(), [2; 4]);
        assert_eq!(
            parse_timeouts("--test_timeout", "1,-1,3,0").unwrap(),
            [1, 300, 3, 3600]
        );
        assert_eq!(
            parse_timeouts("--test_timeout", "1,2").unwrap_err(),
            "While parsing option --test_timeout=1,2: Invalid number of comma-separated entries"
        );
        assert_eq!(
            parse_timeouts("--test_timeout", "x").unwrap_err(),
            "While parsing option --test_timeout=x: 'x' is not an int"
        );
    }

    #[test]
    fn options_parse_like_bazel() {
        let mut options = TestOptions::default();
        assert_eq!(
            options.parse("--test_output=bogus", None).unwrap_err(),
            "While parsing option --test_output=bogus: Not a valid test output: 'bogus' (should be summary, errors, all or streamed)"
        );
        assert_eq!(options.parse("--test_output=ERRORS", None), Ok(Some(false)));
        assert_eq!(options.output, TestOutput::Errors);
        assert_eq!(options.parse("--test_arg", Some("x")), Ok(Some(true)));
        assert_eq!(options.args, ["x"]);
        assert_eq!(
            options
                .parse("--test_size_filters=-small,-medium,-large,-enormous", None)
                .unwrap_err(),
            "While parsing option --test_size_filters=-small,-medium,-large,-enormous: Test size filter '-small,-medium,-large,-enormous' definition cannot match any tests"
        );
        assert_eq!(
            options.parse("--test_size_filters=small,-large", None),
            Ok(Some(false))
        );
        assert_eq!(
            options.size_filters,
            Some(BTreeSet::from([TestSize::Small]))
        );
        assert_eq!(options.parse("-t-", None), Ok(Some(false)));
        assert_eq!(options.cache_test_results, TriState::No);
        assert_eq!(options.parse("--unrelated", None), Ok(None));
        assert_eq!(english_and(&["a".into(), "b".into()]), "a and b");
        assert_eq!(
            english_and(&["a".into(), "b".into(), "c".into()]),
            "a, b, and c"
        );
    }

    #[test]
    fn names_and_directories() {
        assert_eq!(test_name("//p:t", 1, 3, 0, 1), "//p:t (shard 2 of 3)");
        assert_eq!(test_name("//p:t", 0, 0, 1, 2), "//p:t (run 2 of 2)");
        assert_eq!(
            test_directory("t", 1, 3, 0, 1),
            PathBuf::from("t/shard_2_of_3")
        );
        assert_eq!(
            test_directory("t", 0, 3, 1, 2),
            PathBuf::from("t/shard_1_of_3_run_2_of_2")
        );
        assert_eq!(termination_short(1), "Exit 1");
        assert_eq!(termination_short(139), "Segmentation fault");
    }

    #[test]
    fn summary_lines_and_order() {
        let options = TestOptions::default();
        let passed = TestSummary::aggregate(
            "//pkg:pass_test",
            TestTimeout::Moderate,
            &[RunResult {
                shard: 0,
                status: TestStatus::Passed,
                cached: true,
                test_times: vec![10],
                passed_log: None,
                failed_logs: vec![],
                test_case: None,
            }],
            false,
        );
        let flaky = TestSummary::aggregate(
            "//pkg:flaky_test",
            TestTimeout::Moderate,
            &[RunResult {
                shard: 0,
                status: TestStatus::Flaky,
                cached: false,
                test_times: vec![10, 20],
                passed_log: Some(PathBuf::from("/nonexistent/test.log")),
                failed_logs: vec![PathBuf::from("/nonexistent/attempt_1.log")],
                test_case: None,
            }],
            false,
        );
        let text = summary_text(&[flaky, passed], 1, &options);
        assert_eq!(
            text,
            "//pkg:pass_test                                                 (cached) PASSED in 0.0s\n\
             //pkg:flaky_test                                                          FLAKY, failed in 1 out of 2 in 0.0s\n  Stats over 2 runs: max = 0.0s, min = 0.0s, avg = 0.0s, dev = 0.0s\n\
             \nExecuted 1 out of 2 tests: 2 tests pass.\nThere were tests whose specified size is too big. Use the --test_verbose_timeout_warnings command line option to see which ones these are.\n"
        );
        assert_eq!(filter_header(b"a\n-----------------------------------------------------------------------------\nb\n"), b"b\n");
        assert_eq!(filter_header(b"no header\n"), b"no header\n");
    }

    #[test]
    fn generated_xml_parses_into_one_error_case() {
        let xml = "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<testsuites>\n  <testsuite name=\"pkg/fail_test.sh\" tests=\"1\" failures=\"0\" errors=\"1\">\n    <testcase name=\"pkg/fail_test.sh\" classname=\"\" status=\"run\" duration=\"0\" time=\"0\"><error message=\"exited with error code 1\"></error></testcase>\n      <system-out>\nGenerated test.log:\n<![CDATA[a <b> ]]>\n      </system-out>\n    </testsuite>\n</testsuites>\n";
        let case = parse_test_xml(xml).unwrap();
        let mut cases = TestCases::default();
        cases.collect(Some(&case));
        assert_eq!(cases.total, 1);
        assert_eq!(cases.failed.len(), 1);
        let mut out = String::new();
        print_test_case(&cases.failed[0], &mut out);
        assert_eq!(out, "    ERROR   .pkg/fail_test.sh (0.0s)\n");
        let empty = parse_test_xml("<?xml version=\"1.0\"?><testsuites/>").unwrap();
        let mut cases = TestCases::default();
        cases.collect(Some(&empty));
        assert_eq!(cases.total, 0);
        assert!(parse_test_xml("not xml").is_none());
    }
}
