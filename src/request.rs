//! The plan/execute split (L2-W4, PLAN-L2 §1d/§1e): planning
//! (crate::engine::Engine::plan_request) returns a [`PlannedRequest`] —
//! plain owned data, no Starlark values and no closures (an Args `map_each`
//! of a deferred command line is the one in-memory value, and it makes the
//! request unencodable) — and `build`, `test` and `run` all execute it the
//! same way: [`execute`] finalizes it ([`PlannedRequest::finalize`]: the
//! workspace status placeholders and the inherited client environment),
//! writes the action dumps, runs the scheduler and computes the build summary
//! and the run targets. The full-analysis path hands every request over
//! through crate::snapshot::codec::hand_over (a codec round trip under
//! `BAZEL_RS_SNAPSHOT_ROUNDTRIP=1`), and a snapshot hit (L2-W5) will decode a
//! saved request and run the same code:
//!
//! ```text
//! miss: plan -> PlannedRequest -> hand_over -> finalize -> execute -> report
//! hit:  decode ---------------------------> finalize -> execute -> report
//! ```

use crate::action::{Action, Transform};
use crate::engine::{BuildSummary, RunRunfiles, RunTarget};
use crate::label::Label;
use crate::scheduler::{self, Dependency, PlannedAction};
use crate::test_command::TestRuleInfo;
use crate::workspace_status::{ComputedWorkspaceStatus, WorkspaceStatusOptions};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// The ids of the workspace status nodes (BazelWorkspaceStatusAction's
/// stable and volatile outputs; the native executor has no two-output
/// write, so the one logical action is two nodes, crate::engine).
pub const STABLE_STATUS_ID: &str = "BazelWorkspaceStatusAction#stable";
pub const VOLATILE_STATUS_ID: &str = "BazelWorkspaceStatusAction#volatile";
/// The prefix of the workspace status node ids (never counted in the
/// build summary: Bazel reports the status action as an internal process).
const STATUS_ID_PREFIX: &str = "BazelWorkspaceStatusAction#";

/// Where an artifact is reported or published after the execution phase:
/// its planned path and its producer (action id, declaration-order output
/// index), if generated ([`executed_path`]).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Produced {
    /// A source's path, or a generated file's planned location (reported
    /// as is under `--nobuild`).
    #[serde(with = "crate::snapshot::codec::path")]
    pub real: PathBuf,
    pub producer: Option<(String, usize)>,
}

/// What `run` and `test` execute after the build, per run/test target
/// (crate::engine's RunPlan with the artifacts resolved): only targets
/// with an executable.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PlannedRun {
    /// FilesToRunProvider.getExecutable.
    pub executable: Produced,
    /// Its exec path (`bazel-out/<cfg>/bin/<pkg>/<name>`).
    #[serde(with = "crate::snapshot::codec::path")]
    pub executable_exec: PathBuf,
    /// Its runfiles path (Artifact.getRunfilesPath).
    pub executable_short_path: String,
    /// The built runfiles tree's MANIFEST and its exec path, if published.
    pub tree: Option<(Produced, PlannedPath)>,
    /// The runfiles mapping (`None`: an empty file), with runfiles support.
    pub entries: Option<Vec<(String, Option<Produced>)>>,
    pub settings: crate::runfiles::RunSettings,
    /// Index into [`PlannedRequest::configurations`].
    pub configuration: usize,
}

/// A path encoded as raw bytes (a tuple member).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlannedPath(#[serde(with = "crate::snapshot::codec::path")] pub PathBuf);

/// What run targets read from their configuration (Engine::build_requests
/// read it from the configuration table): the output directory mnemonic,
/// the action environment and the `--test_env` environment.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConfigurationSummary {
    pub mnemonic: String,
    pub action_env: crate::action_env::ActionEnvironment,
    pub test_env: crate::action_env::ActionEnvironment,
}

/// One test of `bazel test` (or the target of `bazel run`): its loading-time
/// test properties and the rule's location.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlannedTest {
    pub label: Label,
    pub info: TestRuleInfo,
    pub location: Option<String>,
}

/// The command a request was planned for, with what only that command
/// reads after planning.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum RequestCommand {
    Build,
    /// `bazel test`: the targets built, and the tests to run (after the
    /// test filters, in label order).
    Test {
        build: Vec<Label>,
        tests: Vec<PlannedTest>,
    },
    /// `bazel run LABEL`: the target's test properties when it is a test
    /// rule (`run` of a test, crate::engine::Engine::test_rule_info:
    /// `Ok(None)` for any other target, `Err` as that lookup fails).
    Run {
        label: Label,
        test: Result<Option<PlannedTest>, String>,
    },
}

/// Everything execution and reporting need after planning (PLAN-L2 §1d).
#[derive(Debug)]
pub struct PlannedRequest {
    pub command: RequestCommand,
    /// False under `--nobuild`: no execution phase.
    pub execute: bool,
    /// The actions to execute, in plan order (the reachable ones). The
    /// workspace status nodes are placeholders at their position
    /// (Transform::WorkspaceStatus), an action's inherited client
    /// environment is resolved by finalize.
    pub nodes: Vec<PlannedAction>,
    /// The requested targets' reported artifacts.
    pub requested: Vec<(Label, Produced)>,
    /// `Aspect NAME of LABEL up-to-date:` reports.
    pub aspect_reports: Vec<(String, Label, Vec<Produced>)>,
    /// Labels whose own artifacts print a target result (`--show_result`).
    pub non_empty_targets: usize,
    pub run_plans: Vec<(Label, PlannedRun)>,
    pub configurations: Vec<ConfigurationSummary>,
    /// Analysis warnings and errors, printed after execution (without the
    /// `WARNING: `/`ERROR: ` prefixes).
    pub warnings: Vec<String>,
    pub errors: Vec<String>,
    /// The planned actions whose planning ran a build-info translation
    /// callback (actions.transform_{version,info}_file): their
    /// substitutions belong to this run's workspace status.
    pub build_info: Vec<String>,
}

/// Why a planned request must not be saved as a snapshot (PLAN-L2 §1b's
/// plan-dependent taints, decided on the plan itself: an action analysis
/// registered but the plan pruned never blocks).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Blocker {
    pub taint: crate::observed_fs::Taint,
    pub detail: String,
}

/// The analysis diagnostics a request prints after execution.
#[derive(Clone, Debug, Default)]
pub struct Diagnostics {
    pub warnings: Vec<String>,
    pub errors: Vec<String>,
}

/// Where finalize takes the workspace status from.
pub enum StatusSource {
    /// The full-analysis path: the status planning computed
    /// (crate::engine::Engine::take_workspace_status; `None` when planning
    /// computed none — then the request has no placeholder).
    Planned(Option<ComputedWorkspaceStatus>),
    /// A decoded request (a snapshot hit): run the status command now.
    Compute {
        options: WorkspaceStatusOptions,
        workspace: PathBuf,
    },
}

impl PlannedRequest {
    /// The snapshot blockers among the planned actions: build-info
    /// callbacks (their substitutions are this run's workspace status) and
    /// deferred command lines whose `map_each` is a Starlark function.
    pub fn blockers(&self) -> Vec<Blocker> {
        let mut blockers: Vec<Blocker> = self
            .build_info
            .iter()
            .map(|id| Blocker {
                taint: crate::observed_fs::Taint::BuildInfo,
                detail: id.clone(),
            })
            .collect();
        blockers.extend(
            self.nodes
                .iter()
                .filter(|node| node.deferred.as_ref().is_some_and(|d| d.uses_map_each()))
                .map(|node| Blocker {
                    taint: crate::observed_fs::Taint::MapEach,
                    detail: node.action.id.clone(),
                }),
        );
        blockers
    }

    /// The analysis diagnostics, taken (printed by the caller after
    /// execution, also when it fails).
    pub fn take_diagnostics(&mut self) -> Diagnostics {
        Diagnostics {
            warnings: std::mem::take(&mut self.warnings),
            errors: std::mem::take(&mut self.errors),
        }
    }

    /// The labels of the tests a `test` request runs.
    pub fn test_labels(&self) -> Vec<Label> {
        match &self.command {
            RequestCommand::Test { tests, .. } => {
                tests.iter().map(|test| test.label.clone()).collect()
            }
            _ => Vec::new(),
        }
    }

    /// Complete the request for this run (PLAN-L2 §1d `finalize`, on the
    /// full-analysis path and on a hit alike):
    ///
    /// * the workspace status placeholders become the status nodes: on
    ///   success the `#stable` and `#volatile` writes; on failure ONE
    ///   failing `#volatile` node (Bazel's one action fails once), every
    ///   consumer of the stable file depending on it instead, exactly as
    ///   planning laid the failure out before the split;
    /// * every inherited action environment name is resolved against the
    ///   client environment (ActionEnvironment.resolve; the encoding never
    ///   holds the values).
    pub fn finalize(&mut self, status: StatusSource) -> Result<(), String> {
        let placeholders = self.nodes.iter().any(|node| {
            matches!(
                node.action.extra.transform,
                Some(Transform::WorkspaceStatus { .. })
            )
        });
        if placeholders {
            let computed = match status {
                StatusSource::Planned(Some(computed)) => computed,
                StatusSource::Planned(None) => {
                    return Err(
                        "internal error: a workspace status placeholder without a computed workspace status"
                            .to_owned(),
                    );
                }
                StatusSource::Compute { options, workspace } => {
                    let client = crate::action_env::client_environment();
                    match crate::workspace_status::compute(
                        &options,
                        &workspace,
                        &client,
                        crate::workspace_status::current_time_millis(&client),
                    ) {
                        Ok(status) => ComputedWorkspaceStatus::Ok(status),
                        Err(error) => ComputedWorkspaceStatus::Failed(error),
                    }
                }
            };
            self.expand_status(&computed);
        }
        if self
            .nodes
            .iter()
            .any(|node| !node.action.extra.inherited_environment.is_empty())
        {
            let client = crate::action_env::client_environment();
            for node in &mut self.nodes {
                let extra = &mut node.action.extra;
                extra.client_environment = if extra.inherited_environment.is_empty() {
                    BTreeMap::new()
                } else {
                    crate::action_env::ActionEnvironment::resolve_inherited(
                        &extra.inherited_environment,
                        &client,
                    )
                };
            }
        }
        Ok(())
    }

    fn expand_status(&mut self, computed: &ComputedWorkspaceStatus) {
        match computed {
            ComputedWorkspaceStatus::Ok(status) => {
                for node in &mut self.nodes {
                    if let Some(Transform::WorkspaceStatus { volatile }) =
                        node.action.extra.transform
                    {
                        node.action.extra.transform = None;
                        node.action.write = Some(crate::action::WriteAction {
                            content: if volatile {
                                status.volatile_bytes.clone()
                            } else {
                                status.stable_bytes.clone()
                            },
                            executable: false,
                        });
                    }
                }
            }
            ComputedWorkspaceStatus::Failed(error) => {
                self.nodes.retain(|node| {
                    !matches!(
                        node.action.extra.transform,
                        Some(Transform::WorkspaceStatus { volatile: false })
                    )
                });
                for node in &mut self.nodes {
                    if matches!(
                        node.action.extra.transform,
                        Some(Transform::WorkspaceStatus { volatile: true })
                    ) {
                        node.action.write = None;
                        node.action.extra.transform = Some(Transform::Unsupported {
                            message: format!(
                                "BazelWorkspaceStatusAction volatile-status.txt failed: Failed to determine workspace status: {}",
                                error.0
                            ),
                        });
                    }
                    for dependency in &mut node.dependencies {
                        redirect_stable(&mut dependency.action_id);
                    }
                }
                let redirect = |produced: &mut Produced| {
                    if let Some((id, _)) = &mut produced.producer {
                        redirect_stable(id);
                    }
                };
                for (_, produced) in &mut self.requested {
                    redirect(produced);
                }
                for (_, _, artifacts) in &mut self.aspect_reports {
                    artifacts.iter_mut().for_each(redirect);
                }
                for (_, run) in &mut self.run_plans {
                    redirect(&mut run.executable);
                    if let Some((tree, _)) = &mut run.tree {
                        redirect(tree);
                    }
                    for (_, entry) in run.entries.iter_mut().flatten() {
                        if let Some(entry) = entry {
                            redirect(entry);
                        }
                    }
                }
            }
        }
    }
}

/// A failed status has one node: what produced the stable file is the
/// failing volatile node (output 0, its only output).
fn redirect_stable(id: &mut String) {
    if id == STABLE_STATUS_ID {
        *id = VOLATILE_STATUS_ID.to_owned();
    }
}

/// How a request executes.
pub struct ExecuteOptions<'a> {
    pub cache: &'a Path,
    pub jobs: usize,
    /// `--native-action-dump=<file>` (`build` only).
    pub action_dump: Option<&'a Path>,
}

/// An executed request: the build summary and the run targets, with the
/// command's own data.
#[derive(Debug)]
pub struct Executed {
    pub summary: BuildSummary,
    pub targets: BTreeMap<Label, RunTarget>,
    pub command: RequestCommand,
    pub execute: bool,
}

/// Finalize, dump, execute and summarize one planned request (the shared
/// tail of `build`, `test` and `run`). The caller prints the analysis
/// diagnostics ([`PlannedRequest::take_diagnostics`]) after it, also when
/// it fails.
pub fn execute(
    mut request: PlannedRequest,
    status: StatusSource,
    options: &ExecuteOptions<'_>,
) -> Result<Executed, String> {
    request.finalize(status)?;
    if let Some(path) = options.action_dump
        && let Err(error) = dump_actions(&request.nodes, path)
    {
        return Err(format!("--native-action-dump: {error}"));
    }
    dump_action_graph(&request.nodes);
    let PlannedRequest {
        command,
        execute,
        nodes,
        requested,
        aspect_reports,
        non_empty_targets,
        run_plans,
        configurations,
        ..
    } = request;
    // --nobuild: analysis/planning completed; the execution phase is
    // skipped entirely (BuildTool skips ExecutionTool without an execution
    // phase), so no action runs, no output is written and the summary
    // reports 0 executed / 0 hits. Planned artifacts are still reported
    // at their exec paths, as BuildResultPrinter reports artifactsToBuild
    // under --nobuild (no status nodes are planned under --nobuild).
    let results = if execute {
        scheduler::execute_graph(options.cache, nodes, options.jobs)?
    } else {
        drop(nodes);
        BTreeMap::new()
    };
    crate::perf_mark("execute.end");
    let mut summary = BuildSummary::default();
    // `build` counts remote cache hits apart; the build part of `test` and
    // `run` counts them as executed (as before the split).
    let remote_hits_apart = matches!(command, RequestCommand::Build);
    // The workspace status nodes (BazelWorkspaceStatusAction#*) execute
    // like any action but are not counted: Bazel reports the status action
    // as an internal process (`INFO: 1 process: 1 internal`), outside the
    // build's action tally.
    for (id, result) in &results {
        if id.starts_with(STATUS_ID_PREFIX) {
            continue;
        }
        if result.cache_hit {
            summary.cache_hits += 1;
        } else if remote_hits_apart && result.remote_hit {
            summary.remote_hits += 1;
        } else {
            summary.executed += 1;
        }
    }
    let published = |produced: &Produced| executed_path(&results, execute, produced);
    for (label, produced) in &requested {
        summary.outputs.push((label.clone(), published(produced)));
    }
    summary.non_empty_targets = non_empty_targets;
    summary.aspects = aspect_reports
        .iter()
        .map(|(aspect, label, artifacts)| {
            (
                aspect.clone(),
                label.clone(),
                artifacts.iter().map(published).collect(),
            )
        })
        .collect();
    let mut targets = BTreeMap::new();
    for (label, plan) in run_plans {
        let runfiles = plan.tree.as_ref().and_then(|(tree, exec)| {
            let manifest = published(tree);
            let directory = manifest.parent()?.to_path_buf();
            // `<root>/<tree exec dir>`: strip the exec path to find the root.
            let exec_directory = exec.0.parent()?;
            let mut root = directory.clone();
            for _ in exec_directory.components() {
                root = root.parent()?.to_path_buf();
            }
            Some(RunRunfiles {
                executable: root.join(&plan.executable_exec),
                directory,
                root,
            })
        });
        let entries = plan.entries.as_ref().map(|entries| {
            entries
                .iter()
                .map(|(path, produced)| (path.clone(), produced.as_ref().map(published)))
                .collect()
        });
        let configuration = configurations
            .get(plan.configuration)
            .ok_or_else(|| format!("internal error: run plan of {label} has no configuration"))?;
        targets.insert(
            label,
            RunTarget {
                executable: published(&plan.executable),
                executable_exec: plan.executable_exec,
                executable_short_path: plan.executable_short_path,
                runfiles,
                entries,
                mnemonic: configuration.mnemonic.clone(),
                action_env: configuration.action_env.clone(),
                test_env: configuration.test_env.clone(),
                settings: plan.settings,
            },
        );
    }
    Ok(Executed {
        summary,
        targets,
        command,
        execute,
    })
}

/// Where a planned artifact is reported or published after the execution
/// phase. An executing build indexes the scheduler results exactly as
/// before `--nobuild` existed: execute_graph returns one result per planned
/// node or fails, and planning keeps the producer of everything reported
/// or published, so a missing result is an invariant violation that panics
/// rather than an unbuilt artifact silently reported as built. Only
/// `--nobuild` (no execution phase, no results) keeps the planned exec
/// path, as BuildResultPrinter reports artifactsToBuild without executing.
pub fn executed_path(
    results: &BTreeMap<String, crate::action::ActionResult>,
    execute: bool,
    produced: &Produced,
) -> PathBuf {
    match &produced.producer {
        Some((id, index)) if execute => results[id].outputs[*index].clone(),
        _ => produced.real.clone(),
    }
}

/// One JSON object per planned action (`--native-action-dump`): a native
/// introspection facility (Bazel has no equivalent option; aquery is the
/// genuine surface, docs/CC_ACTIONS.md), appended before execution.
fn dump_actions(nodes: &[PlannedAction], path: &Path) -> std::io::Result<()> {
    use std::io::Write;
    // snapshot-unrecorded: --native-action-dump output (a write).
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)?;
    for node in nodes {
        let action = &node.action;
        let environment = crate::action::resolved_environment(action);
        // The node's listed inputs (PA-1: its shared groups' after its own).
        let inputs: Vec<String> = node
            .inputs()
            .map(|(_, destination)| destination.to_string_lossy().into_owned())
            .chain(node.all_dependencies().map(destination))
            .collect();
        let mut requirements: Vec<&str> =
            action.execution_info.iter().map(String::as_str).collect();
        requirements.extend(action.extra.execution_info.keys().map(String::as_str));
        requirements.sort_unstable();
        requirements.dedup();
        let object = serde_json::json!({
            "id": action.id,
            "mnemonic": action.extra.mnemonic,
            "arguments": crate::action::arguments(action),
            "environment": environment
                .iter()
                .map(|(key, value)| (key, value.to_string_lossy().into_owned()))
                .collect::<Vec<_>>(),
            "inherited_environment": action.extra.inherited_environment.iter()
                .cloned()
                .collect::<Vec<_>>(),
            "inputs": inputs,
            "outputs": action.outputs.iter().map(|output| output.to_string_lossy().into_owned()).collect::<Vec<_>>(),
            "execution_requirements": requirements,
        });
        writeln!(file, "{object}")?;
    }
    Ok(())
}

/// A generated dependency's destination exec path.
fn destination(dependency: &Dependency) -> String {
    dependency.destination.to_string_lossy().into_owned()
}

/// Hidden action-graph dump (`BAZEL_RS_DUMP_ACTIONS=1`, review §5): every
/// planned action's full identity — id, argv/program/write, inputs
/// (sources and generated destinations), outputs, environment and
/// execution info — in plan order, on stderr. The A/B gate for the
/// parallel phases compares this dump byte for byte between thread
/// counts; it contains no pointer identity.
pub fn dump_action_graph(nodes: &[PlannedAction]) {
    if std::env::var_os("BAZEL_RS_DUMP_ACTIONS").is_none() {
        return;
    }
    for node in nodes {
        let action: &Action = &node.action;
        eprintln!("ACTION {}", action.id);
        if let Some(write) = &action.write {
            eprintln!(
                "  write {} bytes executable={}",
                write.content.len(),
                write.executable
            );
        } else if let Some(program) = &action.program {
            eprintln!("  program {program}");
        }
        if !action.arguments.is_empty() {
            eprintln!("  arguments {}", action.arguments.join("\u{1}"));
        }
        if !action.command.is_empty() {
            eprintln!("  command {}", action.command);
        }
        // The listed inputs: a node's shared groups after its own (PA-1).
        for (source, destination) in node.inputs() {
            eprintln!("  input {} -> {}", source.display(), destination.display());
        }
        for dependency in node.all_dependencies() {
            eprintln!(
                "  generated {}[{}] -> {}",
                dependency.action_id,
                dependency.output_index,
                dependency.destination.display()
            );
        }
        for output in &action.outputs {
            eprintln!("  output {}", output.display());
        }
        for (key, value) in &action.environment {
            eprintln!("  env {key}={value}");
        }
        if !action.execution_info.is_empty() {
            eprintln!(
                "  execution-info {}",
                action
                    .execution_info
                    .iter()
                    .cloned()
                    .collect::<Vec<_>>()
                    .join(",")
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn generated_out_txt() -> Produced {
        Produced {
            real: PathBuf::from("bazel-out/k8-fastbuild/bin/pkg/out.txt"),
            producer: Some(("//pkg:gen#genrule".into(), 0)),
        }
    }

    /// Only `--nobuild` reports the planned exec path; an executing build
    /// reports the executed output.
    #[test]
    fn executed_path_falls_back_to_the_planned_path_only_under_nobuild() {
        let artifact = generated_out_txt();
        let mut results = BTreeMap::new();
        assert_eq!(executed_path(&results, false, &artifact), artifact.real);
        let executed =
            PathBuf::from("/cache/actions/k/outputs/bazel-out/k8-fastbuild/bin/pkg/out.txt");
        results.insert(
            "//pkg:gen#genrule".to_owned(),
            crate::action::ActionResult {
                outputs: vec![executed.clone()],
                cache_hit: false,
                remote_hit: false,
                trees: BTreeMap::new(),
            },
        );
        assert_eq!(executed_path(&results, true, &artifact), executed);
        assert_eq!(executed_path(&results, false, &artifact), artifact.real);
    }

    /// An executing build never reports an unbuilt artifact: a missing
    /// scheduler result panics exactly as the `results[id]` indexing before
    /// `--nobuild` support did.
    #[test]
    #[should_panic(expected = "no entry found for key")]
    fn executed_path_never_falls_back_when_executing() {
        executed_path(&BTreeMap::new(), true, &generated_out_txt());
    }

    fn node(id: &str, transform: Option<Transform>, dependencies: &[&str]) -> PlannedAction {
        PlannedAction {
            action: Action {
                id: id.to_owned(),
                outputs: vec![PathBuf::from(format!("bazel-out/{id}"))],
                extra: crate::action::ActionExtra {
                    transform,
                    mnemonic: "M".to_owned(),
                    ..crate::action::ActionExtra::default()
                },
                ..Action::default()
            },
            dependencies: dependencies
                .iter()
                .map(|id| Dependency {
                    action_id: (*id).to_owned(),
                    output_index: 0,
                    destination: PathBuf::from("bazel-out/stable-status.txt"),
                })
                .collect(),
            deferred: None,
            groups: Vec::new(),
        }
    }

    /// A request as planning lays out a build consuming the stable status
    /// file: a consumer, the two placeholders, a later action.
    fn with_placeholders() -> PlannedRequest {
        let stable = || Produced {
            real: PathBuf::from("bazel-out/stable-status.txt"),
            producer: Some((STABLE_STATUS_ID.to_owned(), 0)),
        };
        PlannedRequest {
            command: RequestCommand::Build,
            execute: true,
            nodes: vec![
                node("//:first", None, &[]),
                node(
                    STABLE_STATUS_ID,
                    Some(Transform::WorkspaceStatus { volatile: false }),
                    &[],
                ),
                node(
                    VOLATILE_STATUS_ID,
                    Some(Transform::WorkspaceStatus { volatile: true }),
                    &[],
                ),
                node("//:consumer", None, &[STABLE_STATUS_ID, VOLATILE_STATUS_ID]),
            ],
            requested: vec![(Label::parse_canonical("//:t").unwrap(), stable())],
            aspect_reports: vec![(
                "a".to_owned(),
                Label::parse_canonical("//:t").unwrap(),
                vec![stable()],
            )],
            non_empty_targets: 1,
            run_plans: Vec::new(),
            configurations: Vec::new(),
            warnings: Vec::new(),
            errors: Vec::new(),
            build_info: Vec::new(),
        }
    }

    fn status() -> crate::workspace_status::WorkspaceStatus {
        crate::workspace_status::WorkspaceStatus {
            stable: BTreeMap::new(),
            volatile: BTreeMap::new(),
            stable_bytes: b"BUILD_HOST h\n".to_vec(),
            volatile_bytes: b"BUILD_TIMESTAMP 1\n".to_vec(),
        }
    }

    #[test]
    fn finalize_writes_the_computed_status_into_its_placeholders() {
        let mut request = with_placeholders();
        request
            .finalize(StatusSource::Planned(Some(ComputedWorkspaceStatus::Ok(
                status(),
            ))))
            .unwrap();
        let ids: Vec<&str> = request.nodes.iter().map(|n| n.action.id.as_str()).collect();
        assert_eq!(
            ids,
            [
                "//:first",
                STABLE_STATUS_ID,
                VOLATILE_STATUS_ID,
                "//:consumer"
            ]
        );
        for (index, bytes) in [(1, &b"BUILD_HOST h\n"[..]), (2, b"BUILD_TIMESTAMP 1\n")] {
            let action = &request.nodes[index].action;
            assert!(action.extra.transform.is_none());
            assert_eq!(action.write.as_ref().unwrap().content, bytes);
            assert!(!action.write.as_ref().unwrap().executable);
            assert_eq!(action.extra.mnemonic, "M");
        }
        assert_eq!(request.nodes[3].dependencies[0].action_id, STABLE_STATUS_ID);
        assert_eq!(
            request.requested[0].1.producer,
            Some((STABLE_STATUS_ID.to_owned(), 0))
        );
    }

    /// A failed status is ONE failing #volatile node where the stable one
    /// was planned, and every stable-file consumer depends on it — the
    /// layout planning produced before the plan/execute split.
    #[test]
    fn finalize_lays_out_a_failed_status_as_one_failing_node() {
        let mut request = with_placeholders();
        request
            .finalize(StatusSource::Planned(Some(
                ComputedWorkspaceStatus::Failed(crate::workspace_status::WorkspaceStatusError(
                    "Process exited with status 1".to_owned(),
                )),
            )))
            .unwrap();
        let ids: Vec<&str> = request.nodes.iter().map(|n| n.action.id.as_str()).collect();
        assert_eq!(ids, ["//:first", VOLATILE_STATUS_ID, "//:consumer"]);
        let failing = &request.nodes[1].action;
        assert!(failing.write.is_none());
        match &failing.extra.transform {
            Some(Transform::Unsupported { message }) => assert_eq!(
                message,
                "BazelWorkspaceStatusAction volatile-status.txt failed: Failed to determine workspace status: Process exited with status 1"
            ),
            other => panic!("{other:?}"),
        }
        let consumer: Vec<&str> = request.nodes[2]
            .dependencies
            .iter()
            .map(|d| d.action_id.as_str())
            .collect();
        assert_eq!(consumer, [VOLATILE_STATUS_ID, VOLATILE_STATUS_ID]);
        assert_eq!(
            request.requested[0].1.producer,
            Some((VOLATILE_STATUS_ID.to_owned(), 0))
        );
        assert_eq!(
            request.aspect_reports[0].2[0].producer,
            Some((VOLATILE_STATUS_ID.to_owned(), 0))
        );
    }

    /// The failed layout redirects the stable file's producer in the run
    /// plans too (the executable, the runfiles tree, every entry), and
    /// leaves every other producer alone.
    #[test]
    fn finalize_redirects_the_run_plans_of_a_failed_status() {
        let stable = || Produced {
            real: PathBuf::from("bazel-out/stable-status.txt"),
            producer: Some((STABLE_STATUS_ID.to_owned(), 0)),
        };
        let other = Produced {
            real: PathBuf::from("bazel-out/k8-fastbuild/bin/pkg/out.txt"),
            producer: Some(("//:first".to_owned(), 0)),
        };
        let source = Produced {
            real: PathBuf::from("/ws/pkg/in.txt"),
            producer: None,
        };
        let mut request = with_placeholders();
        request.run_plans = vec![(
            Label::parse_canonical("//:t").unwrap(),
            PlannedRun {
                executable: stable(),
                executable_exec: PathBuf::from("bazel-out/stable-status.txt"),
                executable_short_path: "stable-status.txt".to_owned(),
                tree: Some((
                    stable(),
                    PlannedPath(PathBuf::from("bazel-out/t.runfiles/MANIFEST")),
                )),
                entries: Some(vec![
                    ("_main/stable".to_owned(), Some(stable())),
                    ("_main/other".to_owned(), Some(other.clone())),
                    ("_main/source".to_owned(), Some(source.clone())),
                    ("_main/empty".to_owned(), None),
                ]),
                settings: crate::runfiles::RunSettings::default(),
                configuration: 0,
            },
        )];
        request
            .finalize(StatusSource::Planned(Some(
                ComputedWorkspaceStatus::Failed(crate::workspace_status::WorkspaceStatusError(
                    "Process exited with status 1".to_owned(),
                )),
            )))
            .unwrap();
        let volatile = Some((VOLATILE_STATUS_ID.to_owned(), 0));
        let (_, run) = &request.run_plans[0];
        assert_eq!(run.executable.producer, volatile);
        assert_eq!(run.tree.as_ref().unwrap().0.producer, volatile);
        // The paths themselves stay the planned ones.
        assert_eq!(run.executable.real, stable().real);
        let entries = run.entries.as_ref().unwrap();
        assert_eq!(entries[0].1.as_ref().unwrap().producer, volatile);
        assert_eq!(entries[1].1.as_ref(), Some(&other));
        assert_eq!(entries[2].1.as_ref(), Some(&source));
        assert_eq!(entries[3].1, None);
        // A successful status redirects nothing.
        let mut request = with_placeholders();
        request.run_plans = vec![(
            Label::parse_canonical("//:t").unwrap(),
            PlannedRun {
                executable: stable(),
                executable_exec: PathBuf::from("bazel-out/stable-status.txt"),
                executable_short_path: "stable-status.txt".to_owned(),
                tree: None,
                entries: None,
                settings: crate::runfiles::RunSettings::default(),
                configuration: 0,
            },
        )];
        request
            .finalize(StatusSource::Planned(Some(ComputedWorkspaceStatus::Ok(
                status(),
            ))))
            .unwrap();
        assert_eq!(
            request.run_plans[0].1.executable.producer,
            Some((STABLE_STATUS_ID.to_owned(), 0))
        );
    }

    /// A decoded request (a snapshot hit) runs the status command itself.
    #[test]
    fn finalize_computes_the_status_of_a_decoded_request() {
        let workspace = tempfile::tempdir().unwrap();
        let mut request = with_placeholders();
        request
            .finalize(StatusSource::Compute {
                options: WorkspaceStatusOptions {
                    command: "echo STABLE_W4 yes; echo W4_VOLATILE now".to_owned(),
                    embed_label: "w4".to_owned(),
                },
                workspace: workspace.path().to_path_buf(),
            })
            .unwrap();
        let content = |index: usize| {
            String::from_utf8(request.nodes[index].action.write.clone().unwrap().content).unwrap()
        };
        let (stable, volatile) = (content(1), content(2));
        assert!(stable.contains("BUILD_EMBED_LABEL w4\n"), "{stable}");
        assert!(stable.contains("STABLE_W4 yes\n"), "{stable}");
        assert!(volatile.contains("BUILD_TIMESTAMP "), "{volatile}");
        assert!(volatile.contains("W4_VOLATILE now\n"), "{volatile}");
        let mut failing = with_placeholders();
        failing
            .finalize(StatusSource::Compute {
                options: WorkspaceStatusOptions {
                    command: "exit 3".to_owned(),
                    embed_label: String::new(),
                },
                workspace: workspace.path().to_path_buf(),
            })
            .unwrap();
        assert_eq!(failing.nodes.len(), 3);
        assert!(matches!(
            &failing.nodes[1].action.extra.transform,
            Some(Transform::Unsupported { message }) if message.ends_with("Process exited with status 3")
        ));
    }

    #[test]
    fn placeholders_need_a_status() {
        let mut request = with_placeholders();
        let error = request.finalize(StatusSource::Planned(None)).unwrap_err();
        assert!(error.contains("internal error"), "{error}");
        // Without placeholders (--nobuild) no status is needed.
        let mut request = with_placeholders();
        request.nodes.retain(|node| {
            !matches!(
                node.action.extra.transform,
                Some(Transform::WorkspaceStatus { .. })
            )
        });
        request.finalize(StatusSource::Planned(None)).unwrap();
    }

    /// finalize resolves every inherited name against the client
    /// environment of this run and clears values nothing inherits.
    #[test]
    fn finalize_resolves_the_inherited_client_environment() {
        let mut request = with_placeholders();
        request.nodes.truncate(1);
        let mut inheriting = node("//:inherits", None, &[]);
        inheriting.action.extra.inherited_environment =
            ["PATH".to_owned(), "BAZEL_RS_SURELY_UNSET_W4".to_owned()].into();
        request.nodes.push(inheriting);
        request.nodes[0].action.extra.client_environment =
            BTreeMap::from([("STALE".to_owned(), "x".into())]);
        request.finalize(StatusSource::Planned(None)).unwrap();
        assert!(request.nodes[0].action.extra.client_environment.is_empty());
        let resolved = &request.nodes[1].action.extra.client_environment;
        assert_eq!(
            resolved.get("PATH"),
            std::env::var_os("PATH").as_ref(),
            "{resolved:?}"
        );
        assert!(!resolved.contains_key("BAZEL_RS_SURELY_UNSET_W4"));
    }
}
