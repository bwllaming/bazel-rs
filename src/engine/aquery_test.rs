//! aquery's TestRunner actions (docs/AQUERY.md): the TestRunnerAction of
//! each shard and run of every analyzed test rule
//! (analysis/test/TestActionBuilder.java:224-480), described from the
//! planning state at the analysis boundary like crate::test_runner builds
//! the test it runs. Nothing here executes.
//!
//! Pinned source: `/tmp/opencode/bazel-9.3.1-reference` (Release 9.3.1);
//! paths relative to `src/main/java/com/google/devtools/build/lib/`.
use super::*;
use crate::aquery::{ActionRecord, Configuration};
use crate::aquery_key::{Fingerprint, KeyKind, KeyPlatform};
use crate::test_command::{TestOptions, TestRuleInfo};

/// TestRunnerAction.GUID (analysis/test/TestRunnerAction.java:111).
const TEST_RUNNER_GUID: &str = "cc41f9d0-47a6-11e7-8726-eb6ce83a8cc8";

/// What one analyzed test rule's TestRunner actions are built from.
pub(super) struct TestCapture {
    pub key: ConfiguredTargetKey,
    info: TestRuleInfo,
    /// The executable's runfiles path (TestStrategy.expandedArgsFromAction).
    binary: String,
    /// The RunfilesTree artifact (`<exe>.runfiles`).
    tree: String,
    settings: crate::runfiles::RunSettings,
    /// The `--run_under` target's executable: (exec path, runfiles path).
    run_under: Option<(String, String)>,
}

/// The tests' shared inputs: the configuration's environments.
struct ConfigFacts {
    action_env: crate::action_env::ActionEnvironment,
    test_env: crate::action_env::ActionEnvironment,
    mnemonic: String,
    coverage: bool,
}

/// java.time.Duration.toString of whole seconds (`PT1M`, `PT1H1M1S`, `PT0S`).
fn java_duration(seconds: u64) -> String {
    if seconds == 0 {
        return "PT0S".to_owned();
    }
    let (hours, minutes, rest) = (seconds / 3600, seconds / 60 % 60, seconds % 60);
    let mut text = "PT".to_owned();
    if hours > 0 {
        text.push_str(&format!("{hours}H"));
    }
    if minutes > 0 {
        text.push_str(&format!("{minutes}M"));
    }
    if rest > 0 {
        text.push_str(&format!("{rest}S"));
    }
    text
}

/// java.lang.String.hashCode over Bazel's internal (Latin-1) string.
fn java_hash(text: &str) -> i32 {
    crate::bazel_bytes::internal_bytes(text)
        .iter()
        .fold(0i32, |hash, byte| {
            hash.wrapping_mul(31).wrapping_add(i32::from(*byte))
        })
}

/// TargetUtils.legalExecInfoKeys (packages/TargetUtils.java:53-63).
fn legal_execution_info_tag(tag: &str) -> bool {
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
        || tag == "local"
        || tag == "worker-key-mnemonic"
}

/// TestTargetProperties' execution info in its map order
/// (TestTargetProperties.java:84-122): TargetUtils.getExecutionInfo (a
/// `HashMap` of the legal tags: Java's bucket order), then `local` /
/// `no-remote-exec`, `no-testloasd`, the rule's testing.ExecutionInfo and
/// `--modify_execution_info` (LinkedHashMap puts).
fn execution_info(
    info: &TestRuleInfo,
    requirements: &[(String, String)],
    exclusive_sandboxed: bool,
) -> Vec<(String, String)> {
    let mut tags: Vec<&str> = Vec::new();
    for tag in &info.tags {
        if legal_execution_info_tag(tag) && !tags.contains(&tag.as_str()) {
            tags.push(tag);
        }
    }
    let mut capacity = 16usize;
    while tags.len() > capacity * 3 / 4 {
        capacity *= 2;
    }
    let mut ordered: Vec<(usize, usize, &str)> = tags
        .iter()
        .enumerate()
        .map(|(index, tag)| {
            let hash = java_hash(tag) as u32;
            (
                ((hash ^ (hash >> 16)) as usize) & (capacity - 1),
                index,
                *tag,
            )
        })
        .collect();
    ordered.sort();
    let mut map: Vec<(String, String)> = ordered
        .into_iter()
        .map(|(_, _, tag)| (tag.to_owned(), String::new()))
        .collect();
    let put = |map: &mut Vec<(String, String)>, key: &str, value: &str| match map
        .iter_mut()
        .find(|(existing, _)| existing == key)
    {
        Some(entry) => entry.1 = value.to_owned(),
        None => map.push((key.to_owned(), value.to_owned())),
    };
    if exclusive_sandboxed {
        if info.is_local() {
            put(&mut map, "local", "");
        } else if info.exclusive() {
            put(&mut map, "no-remote-exec", "");
        }
    } else if info.is_local() || info.exclusive() {
        put(&mut map, "local", "");
    }
    if info.tags.iter().any(|tag| tag == "no-testloasd") {
        put(&mut map, "local", "");
        put(&mut map, "no-testloasd", "");
    }
    for (key, value) in requirements {
        put(&mut map, key, value);
    }
    crate::execution_options::modify_ordered("TestRunner", &mut map);
    map
}

/// ActionEnvironment.addTo: the fixed map, then the inherited names.
fn add_environment(
    fp: &mut Fingerprint<Vec<u8>>,
    fixed: &[(String, String)],
    inherited: &[String],
) {
    fp.string_map(fixed.iter().map(|(k, v)| (k.as_str(), v.as_str())));
    fp.strings(inherited.iter().map(String::as_str));
}

fn sorted_environment(
    environment: &crate::action_env::ActionEnvironment,
) -> (Vec<(String, String)>, Vec<String>) {
    (
        environment
            .fixed
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect(),
        environment.inherited.iter().cloned().collect(),
    )
}

impl Engine {
    /// Keep what the TestRunner actions of the analyzed test rules need
    /// (aquery only; TestActionBuilder runs for every test rule with
    /// runfiles support, RuleConfiguredTargetBuilder.java:190-201).
    pub(super) fn aquery_tests(&mut self, state: &ActionPlanning) {
        if self.aquery_capture.is_none() {
            return;
        }
        let mut tests = Vec::new();
        for (key, support) in &state.runfiles_nodes {
            let Ok(Target::Rule(rule)) = self.target(&key.label) else {
                continue;
            };
            let Some(info) = test_rule_properties(&rule) else {
                continue;
            };
            let settings = state
                .runfiles_info
                .get(key)
                .map(|runfiles| runfiles.run.clone())
                .unwrap_or_default();
            let executable = support.executable.exec.to_string_lossy().into_owned();
            // The runfiles tree directory: the MANIFEST's parent, else
            // RunfilesSupport's `<exe>.runfiles`.
            let tree = support
                .tree
                .as_ref()
                .map(|tree| tree.exec.to_string_lossy().into_owned())
                .and_then(|manifest| manifest.strip_suffix("/MANIFEST").map(str::to_owned))
                .unwrap_or_else(|| format!("{executable}.runfiles"));
            // A LabelRunUnder's executable is a runfile of the test
            // (crate::runfiles merges the run_under target's runfiles).
            let run_under = settings.run_under_executable.as_ref().and_then(|short| {
                let path = match short.strip_prefix("../") {
                    Some(external) => external.to_owned(),
                    None => format!("_main/{short}"),
                };
                support
                    .entries
                    .iter()
                    .find(|(entry, _)| **entry == *path)
                    .and_then(|(_, artifact)| artifact.as_ref())
                    .map(|artifact| (artifact.exec.to_string_lossy().into_owned(), short.clone()))
            });
            tests.push(TestCapture {
                key: key.clone(),
                info,
                binary: support.executable.file.short_path.clone(),
                tree,
                settings,
                run_under,
            });
        }
        if let Some(capture) = &mut self.aquery_capture {
            capture.tests = tests;
        }
    }

    /// The TestRunner actions of one captured test, one per shard and run
    /// (TestActionBuilder.java:398-470), each with its TestRunnerAction key
    /// (TestRunnerAction.computeKey :526-558).
    pub(super) fn aquery_test_records(
        &self,
        test: &TestCapture,
        options: &TestOptions,
        configuration: usize,
        configurations: &[Configuration],
    ) -> Vec<ActionRecord> {
        let native = &self.configurations.get(test.key.config).native;
        let facts = ConfigFacts {
            action_env: native.action_environment(),
            test_env: native.test_environment(),
            mnemonic: self.configurations.mnemonic(test.key.config).to_owned(),
            coverage: native.collect_code_coverage,
        };
        // A coverage TestRunnerAction (collect_coverage.sh, the coverage
        // inputs and environment, coverage.dat) is not described: not listed.
        if facts.coverage {
            return Vec::new();
        }
        let label = crate::test_runner::label_print(&test.key.label);
        let runs = options.runs(&label).max(1);
        let shards = options.sharding.shards(test.info.shard_count).max(0) as u32;
        let requirements: Vec<(String, String)> = test
            .settings
            .test_requirements
            .as_ref()
            .map(|requirements| requirements.requirements.clone())
            .unwrap_or_default();
        let execution_info =
            execution_info(&test.info, &requirements, options.exclusive_test_sandboxed);
        let remotable = !execution_info
            .iter()
            .any(|(key, _)| matches!(key.as_str(), "local" | "no-remote" | "no-remote-exec"));
        // TestTargetExecutionSettings.getArgs: the rule's args, then --test_arg.
        let test_args: Vec<String> = test
            .settings
            .args
            .iter()
            .chain(&options.args)
            .cloned()
            .collect();
        let mut arguments = vec![crate::test_runner::SETUP_EXEC_PATH.to_owned()];
        if let Some(run_under) = &options.run_under {
            arguments.extend(run_under.test_prefix(
                test.run_under.as_ref().map(|(_, short)| short.as_str()),
                &options.shell,
            ));
        }
        arguments.push(test.binary.clone());
        arguments.extend(test_args.iter().cloned());
        let mut inputs = vec![
            test.tree.clone(),
            crate::test_runner::SETUP_EXEC_PATH.to_owned(),
            crate::test_runner::XML_EXEC_PATH.to_owned(),
        ];
        if let Some((exec, _)) = &test.run_under {
            inputs.push(exec.clone());
        }
        let (action_fixed, action_inherited) = sorted_environment(&facts.action_env);
        let (test_fixed, test_inherited) = sorted_environment(&facts.test_env);
        let mut extra_inherited: Vec<String> = Vec::new();
        for name in &test.settings.inherited {
            if !extra_inherited.contains(name) {
                extra_inherited.push(name.clone());
            }
        }
        let timeout = java_duration(options.timeout_seconds(test.info.timeout));
        let tool = configurations[configuration].is_tool;
        let (platform_label, properties) = (native.host_platform.clone(), Vec::new());
        let platform = self.aquery_platform_info(&platform_label);
        let platform_label = platform
            .as_ref()
            .map_or(platform_label, |info| info.label.clone());
        let package = if test.key.label.repository.is_empty() {
            test.key.label.package.clone()
        } else if test.key.label.package.is_empty() {
            format!("external/{}", test.key.label.repository)
        } else {
            format!(
                "external/{}/{}",
                test.key.label.repository, test.key.label.package
            )
        };
        let testlogs = if package.is_empty() {
            format!("bazel-out/{}/testlogs", facts.mnemonic)
        } else {
            format!("bazel-out/{}/testlogs/{package}", facts.mnemonic)
        };
        let mut records = Vec::new();
        for shard in 0..shards.max(1) {
            for run in 0..runs {
                let directory = crate::test_command::test_directory(
                    &test.key.label.name,
                    shard,
                    shards,
                    run,
                    runs,
                );
                let base = format!("{testlogs}/{}", directory.to_string_lossy());
                let mut fp: Fingerprint<Vec<u8>> = Fingerprint::default();
                fp.string(TEST_RUNNER_GUID);
                for argument in &test_args {
                    fp.boolean(true).string(argument);
                }
                fp.boolean(false);
                fp.string(options.filter.as_deref().unwrap_or_default());
                fp.boolean(options.runner_fail_fast);
                fp.string(
                    options
                        .run_under
                        .as_ref()
                        .map_or("", |run_under| run_under.value.as_str()),
                );
                // The coverage environment (empty without coverage).
                fp.string_map(std::iter::empty());
                add_environment(&mut fp, &test.settings.environment, &extra_inherited);
                add_environment(&mut fp, &action_fixed, &action_inherited);
                add_environment(&mut fp, &test_fixed, &test_inherited);
                fp.string(test.info.size.name())
                    .string(test.info.timeout.name())
                    .string(&timeout);
                fp.strings(test.info.tags.iter().map(String::as_str));
                fp.boolean(remotable)
                    .int(shard as i32)
                    .int(shards as i32)
                    .int(run as i32)
                    .int(runs as i32)
                    .boolean(facts.coverage)
                    .boolean(options.zip_undeclared_outputs);
                fp.string_map(execution_info.iter().map(|(k, v)| (k.as_str(), v.as_str())));
                fp.nullable_string(None);
                let bytes = fp.into_inner();
                let key_platform = match &platform {
                    Some(info) => KeyPlatform::Resolved(info),
                    None => KeyPlatform::None,
                };
                let action_key = crate::aquery_key::action_key(
                    &KeyKind::Encoded(&bytes),
                    key_platform,
                    &properties,
                );
                let name = crate::test_command::test_name(&label, shard, shards, run, runs);
                records.push(ActionRecord {
                    target: test.key.label.to_string(),
                    configuration,
                    mnemonic: "TestRunner".to_owned(),
                    description: crate::aquery::progress_message(
                        &format!("Testing {name}"),
                        &label,
                        None,
                        None,
                        tool,
                    ),
                    action_key,
                    arguments: Some(arguments.clone()),
                    environment: action_fixed.clone(),
                    inputs: inputs.clone(),
                    // TestRunnerAction's outputs (:213-222): the log, the
                    // XML, the cache status, the undeclared outputs tree.
                    outputs: vec![
                        (format!("{base}/test.log"), false),
                        (format!("{base}/test.xml"), false),
                        (format!("{base}/test.cache_status"), false),
                        (format!("{base}/test.outputs"), true),
                    ],
                    // The map's order (the proto output's; text sorts).
                    execution_info: execution_info.clone(),
                    execution_platform: Some(crate::rule_declarations::canonical_form(
                        &platform_label,
                    )),
                    runfiles_support: true,
                    ..Default::default()
                });
            }
        }
        records
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn durations_print_like_java() {
        assert_eq!(java_duration(0), "PT0S");
        assert_eq!(java_duration(7), "PT7S");
        assert_eq!(java_duration(60), "PT1M");
        assert_eq!(java_duration(300), "PT5M");
        assert_eq!(java_duration(3600), "PT1H");
        assert_eq!(java_duration(3661), "PT1H1M1S");
    }
}
