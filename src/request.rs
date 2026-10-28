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
use crate::scheduler::{self, Dependency, InputGroup, PlannedAction};
use crate::test_command::TestRuleInfo;
use crate::workspace_status::{ComputedWorkspaceStatus, WorkspaceStatusOptions};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

/// The ids of the workspace status nodes (BazelWorkspaceStatusAction's
/// stable and volatile outputs; the native executor has no two-output
/// write, so the one logical action is two nodes, crate::engine).
pub const STABLE_STATUS_ID: &str = "BazelWorkspaceStatusAction#stable";
pub const VOLATILE_STATUS_ID: &str = "BazelWorkspaceStatusAction#volatile";
/// The prefix of the workspace status node ids (never counted in the
/// build summary: Bazel reports the status action as an internal process).
pub const STATUS_ID_PREFIX: &str = "BazelWorkspaceStatusAction#";

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
    /// The producers of the target's top-level artifacts (TargetCompletion:
    /// its requested output groups, validation outputs and runfiles support
    /// files; the workspace status nodes excluded), sorted: a test's actions
    /// start once these are built (TestCompletionFunction.java:55 requests
    /// the target's completion first), PERF(TEST).
    pub completion: Vec<String>,
    /// A test's coverage test action additions (docs/COVERAGE.md), in a
    /// coverage configuration.
    pub coverage: Option<PlannedCoverage>,
}

/// What a coverage test action adds (crate::runfiles::TestCoverage with
/// the inputs' artifacts).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlannedCoverage {
    /// The collect_coverage script's exec path.
    pub script: String,
    /// Inputs by exec path.
    pub inputs: Vec<(String, Produced)>,
    pub manifest_exec: String,
    pub manifest: Vec<String>,
    pub env: Vec<(String, String)>,
    pub reported: Option<(String, String)>,
    pub native_merger: bool,
    /// The baseline coverage files of the request's top-level targets
    /// (CoverageReportActionBuilder.getBaselineCoverageArtifacts), carried
    /// by the first coverage test.
    pub baselines: Vec<(String, Produced)>,
}

/// [`PlannedCoverage`] with the inputs at their published paths.
#[derive(Clone, Debug)]
pub struct RunCoverage {
    pub script: String,
    pub inputs: Vec<(String, PathBuf)>,
    pub manifest_exec: String,
    pub manifest: Vec<String>,
    pub env: Vec<(String, String)>,
    pub reported: Option<(String, String)>,
    pub native_merger: bool,
    pub baselines: Vec<(String, PathBuf)>,
}

impl PlannedRun {
    /// The actions a test of this run plan waits for: the target's
    /// completion ([`PlannedRun::completion`]) and the producers of its
    /// executable, runfiles tree and runfiles entries (deduplicated, in
    /// that order).
    pub fn test_dependencies(&self) -> Vec<String> {
        let mut seen: std::collections::HashSet<&str> = std::collections::HashSet::new();
        let mut dependencies = Vec::new();
        let produced = std::iter::once(&self.executable)
            .chain(self.tree.as_ref().map(|(tree, _)| tree))
            .chain(
                self.entries
                    .iter()
                    .flatten()
                    .filter_map(|(_, entry)| entry.as_ref()),
            )
            .chain(self.coverage.iter().flat_map(|coverage| {
                coverage
                    .inputs
                    .iter()
                    .chain(&coverage.baselines)
                    .map(|(_, produced)| produced)
            }));
        let ids = self.completion.iter().map(String::as_str).chain(
            produced.filter_map(|produced| produced.producer.as_ref().map(|(id, _)| id.as_str())),
        );
        for id in ids {
            if seen.insert(id) {
                dependencies.push(id.to_owned());
            }
        }
        dependencies
    }
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

/// What a `--keep_going` analysis decided (docs/KEEP_GOING.md): the
/// root-cause errors, the requested targets it skipped, and per analyzed
/// requested target the actions its completion needs.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct KeepGoingPlan {
    /// Explicit incompatible roots are genuine policy errors, with their cause
    /// chains reported alongside the per-root warning, not as prerequisite errors.
    pub incompatible: BTreeMap<Label, String>,
    /// The number of top-level targets (analyzed or not).
    pub top_level: usize,
    /// The root-cause analysis errors, in report order (each once).
    pub errors: Vec<String>,
    /// The requested targets whose analysis failed, in label order, with
    /// whether a dependency (not the target itself) failed: SkyframeError-
    /// Processor's `errors encountered while analyzing target 'L', it will
    /// not be built.` and its `Analysis failed` cause.
    pub failed: Vec<(Label, bool)>,
    /// Each analyzed requested target with the ids of the actions producing
    /// its top-level artifacts (requested, hidden and aspect artifacts).
    pub targets: Vec<(Label, Vec<String>)>,
    /// The input failures planning found (`missing input file`, source
    /// directory traversal errors), reported after execution; the actions
    /// they fail (and those above them) are not planned.
    pub input_errors: Vec<String>,
    /// The analyzed requested targets whose own top-level artifacts are
    /// missing inputs.
    pub unbuilt: Vec<Label>,
}

/// Everything execution and reporting need after planning (PLAN-L2 §1d).
#[derive(Debug)]
pub struct PlannedRequest {
    pub command: RequestCommand,
    /// Successful incompatible configured roots excluded from execution.
    pub skipped: Vec<Label>,
    /// `--keep_going`: what the analysis skipped (`None` without it).
    pub keep_going: Option<KeepGoingPlan>,
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
    /// The requested targets that print a build result, each with the
    /// artifacts it displays, in request order (BuildResultPrinter
    /// .outputTargets; crate::engine filters them like
    /// filterTargetsToPrint).
    pub results: Vec<(Label, Vec<Produced>)>,
    pub run_plans: Vec<(Label, PlannedRun)>,
    pub configurations: Vec<ConfigurationSummary>,
    /// Analysis warnings (printed by planning, before `INFO: Analyzed …`)
    /// and errors (printed after execution), without the `WARNING: `/`ERROR: `
    /// prefixes.
    pub warnings: Vec<String>,
    pub errors: Vec<String>,
    /// The planned actions whose planning ran a build-info translation
    /// callback (actions.transform_{version,info}_file): their
    /// substitutions belong to this run's workspace status.
    pub build_info: Vec<String>,
    /// What the build event files report of the requested targets
    /// (crate::bep::plan, docs/BEP.md); its output groups index `requested`.
    pub bep: crate::bep::plan::BepPlan,
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
    /// A request of nothing (a placeholder).
    pub fn empty() -> Self {
        Self {
            command: RequestCommand::Build,
            keep_going: None,
            execute: false,
            nodes: Vec::new(),
            requested: Vec::new(),
            aspect_reports: Vec::new(),
            results: Vec::new(),
            run_plans: Vec::new(),
            configurations: Vec::new(),
            warnings: Vec::new(),
            errors: Vec::new(),
            build_info: Vec::new(),
            bep: Default::default(),
        }
    }

    /// Whether the plan holds the workspace status placeholders (an
    /// executing build: the status is computed for them).
    pub fn has_status_placeholders(&self) -> bool {
        self.nodes.iter().any(|node| {
            matches!(
                node.action.extra.transform,
                Some(crate::action::Transform::WorkspaceStatus { .. })
            )
        })
    }

    /// The snapshot blockers among the planned actions: build-info
    /// callbacks (their substitutions are this run's workspace status) and
    /// deferred command lines whose `map_each` is a Starlark function
    /// without a checked portable description (crate::snapshot::mapper).
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
                .filter(|node| {
                    node.deferred
                        .as_ref()
                        .is_some_and(|d| d.uses_unportable_map_each())
                })
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
    ///   consumer of the stable file depending on it instead (also through
    ///   a shared input group: a tool's runfiles can hold the file), exactly
    ///   as planning laid the failure out before the split;
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
                crate::bep::workspace_status(status);
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
                self.redirect_stable_in_groups();
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
                    run.completion.iter_mut().for_each(redirect_stable);
                    for (_, entry) in run.entries.iter_mut().flatten() {
                        if let Some(entry) = entry {
                            redirect(entry);
                        }
                    }
                }
            }
        }
    }

    /// The failed-status layout of the shared input groups (PA-1): a
    /// tool's runfiles tree holding the stable status file depends on
    /// `#stable` through its group. Each such group is rebuilt once with
    /// the dependency redirected and every holder takes the rebuilt group,
    /// so its holders still share one (a group is never changed in place:
    /// `Arc::make_mut` would give every holder a copy of its own); the
    /// other groups stay as they are.
    fn redirect_stable_in_groups(&mut self) {
        let stamped = |group: &InputGroup| {
            group
                .dependencies
                .iter()
                .any(|dependency| dependency.action_id == STABLE_STATUS_ID)
        };
        // Old pointer -> (old group kept alive, its redirected copy): no
        // address is reused while the pass runs.
        let mut redirected: HashMap<*const InputGroup, (Arc<InputGroup>, Arc<InputGroup>)> =
            HashMap::new();
        for node in &mut self.nodes {
            for group in &mut node.groups {
                if !stamped(group) {
                    continue;
                }
                let (_, copy) = redirected.entry(Arc::as_ptr(group)).or_insert_with(|| {
                    let mut copy = InputGroup::clone(group);
                    for dependency in &mut copy.dependencies {
                        redirect_stable(&mut dependency.action_id);
                    }
                    (Arc::clone(group), Arc::new(copy))
                });
                *group = Arc::clone(copy);
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
    /// `--keep_going`: what failed (`None` without it).
    pub keep_going: Option<KeepGoingResult>,
}

/// The failures of a `--keep_going` request (docs/KEEP_GOING.md).
#[derive(Debug, Default)]
pub struct KeepGoingResult {
    /// The analysis's decisions.
    pub plan: KeepGoingPlan,
    /// The failed actions' errors, by action id.
    pub actions: Vec<(String, String)>,
    /// The analyzed requested targets whose top-level artifacts did not
    /// all build, in label order.
    pub unbuilt: Vec<Label>,
}

impl KeepGoingResult {
    /// Whether the build failed (an analysis or execution failure).
    pub fn failed(&self) -> bool {
        !self.plan.failed.is_empty()
            || !self.plan.input_errors.is_empty()
            || !self.actions.is_empty()
            || !self.unbuilt.is_empty()
    }

    /// The top-level targets that analyzed and built.
    pub fn succeeded(&self) -> usize {
        self.plan
            .top_level
            .saturating_sub(self.plan.failed.len() + self.unbuilt.len())
    }

    /// Whether requested `label` was analyzed and built.
    pub fn built(&self, label: &Label) -> bool {
        !self.unbuilt.contains(label) && !self.plan.failed.iter().any(|(failed, _)| failed == label)
    }
}

/// Finalize, dump, execute and summarize one planned request (the shared
/// tail of `build`, `test` and `run`). The caller prints the analysis
/// diagnostics ([`PlannedRequest::take_diagnostics`]) after it, also when
/// it fails.
pub fn execute(
    request: PlannedRequest,
    status: StatusSource,
    options: &ExecuteOptions<'_>,
) -> Result<Executed, String> {
    execute_with_tests(request, status, options, None)
}

/// Whether every test of a `test` request can be specified from its plan
/// (crate::test_runner::TestSpec::new over the planned paths: a run plan,
/// runfiles support, the default `test` exec group, a UTF-8 exec path), so
/// its test actions can run inside the execution phase
/// ([`execute_with_tests`]). The first problem is the error the command
/// reports once the build has run, exactly as before PERF(TEST).
pub fn test_preflight(request: &PlannedRequest) -> Result<(), String> {
    let RequestCommand::Test { tests, .. } = &request.command else {
        return Ok(());
    };
    for test in tests {
        if request.skipped.contains(&test.label) {
            continue;
        }
        let plan = request
            .run_plans
            .iter()
            .find(|(label, _)| *label == test.label)
            .map(|(_, plan)| plan)
            .ok_or_else(|| format!("internal error: {} was not built as a test", test.label))?;
        let target = run_target(
            &test.label,
            plan,
            &request.configurations,
            &HashSet::new(),
            &|produced| produced.real.clone(),
        )?;
        crate::test_runner::TestSpec::new(
            &test.label,
            test.info.clone(),
            test.location.clone(),
            &target,
        )?;
    }
    Ok(())
}

/// The run plans' runfiles that are unresolved symlink outputs
/// (crate::action::ActionExtra::symlink_outputs), read from the nodes
/// before they execute.
fn symlink_outputs(
    nodes: &[PlannedAction],
    run_plans: &[(Label, PlannedRun)],
) -> HashSet<(String, usize)> {
    let producers: HashSet<&str> = run_plans
        .iter()
        .flat_map(|(_, plan)| plan.entries.iter().flatten())
        .filter_map(|(_, produced)| Some(produced.as_ref()?.producer.as_ref()?.0.as_str()))
        .collect();
    nodes
        .iter()
        .filter(|node| {
            !node.action.extra.symlink_outputs.is_empty()
                && producers.contains(node.action.id.as_str())
        })
        .flat_map(|node| {
            node.action
                .outputs
                .iter()
                .enumerate()
                .filter(|(_, output)| node.action.extra.symlink_outputs.contains(*output))
                .map(|(index, _)| (node.action.id.clone(), index))
        })
        .collect()
}

/// The run target of one run plan, its artifacts at the paths `published`
/// gives (the scheduler's results, [`executed_path`]).
fn run_target(
    label: &Label,
    plan: &PlannedRun,
    configurations: &[ConfigurationSummary],
    symlink_outputs: &HashSet<(String, usize)>,
    published: &dyn Fn(&Produced) -> PathBuf,
) -> Result<RunTarget, String> {
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
    let symlinks = plan
        .entries
        .iter()
        .flatten()
        .filter(|(_, produced)| {
            produced
                .as_ref()
                .and_then(|produced| produced.producer.as_ref())
                .is_some_and(|producer| symlink_outputs.contains(producer))
        })
        .map(|(path, _)| path.clone())
        .collect();
    let configuration = configurations
        .get(plan.configuration)
        .ok_or_else(|| format!("internal error: run plan of {label} has no configuration"))?;
    Ok(RunTarget {
        executable: published(&plan.executable),
        executable_exec: plan.executable_exec.clone(),
        executable_short_path: plan.executable_short_path.clone(),
        runfiles,
        entries,
        symlinks,
        mnemonic: configuration.mnemonic.clone(),
        action_env: configuration.action_env.clone(),
        test_env: configuration.test_env.clone(),
        settings: plan.settings.clone(),
        coverage: plan.coverage.as_ref().map(|coverage| RunCoverage {
            script: coverage.script.clone(),
            inputs: coverage
                .inputs
                .iter()
                .map(|(exec, produced)| (exec.clone(), published(produced)))
                .collect(),
            manifest_exec: coverage.manifest_exec.clone(),
            manifest: coverage.manifest.clone(),
            env: coverage.env.clone(),
            reported: coverage.reported.clone(),
            native_merger: coverage.native_merger,
            baselines: coverage
                .baselines
                .iter()
                .map(|(exec, produced)| (exec.clone(), published(produced)))
                .collect(),
        }),
    })
}

/// The execution phase of `bazel test` (PERF(TEST)): every parallel test
/// action of `session` is a leaf of the execution graph, run as soon as its
/// test's inputs are built (crate::scheduler::Leaves; Bazel requests each
/// test's TestCompletionValue in the same evaluation as the build,
/// skyframe/BuildDriverFunction.java:589-642); the exclusive ones run one at
/// a time once the graph succeeded (SkyframeBuildView.java:759-794). The
/// results are the build's; the tests report to the session.
fn execute_tests(
    options: &ExecuteOptions<'_>,
    nodes: Vec<PlannedAction>,
    run_plans: &[(Label, PlannedRun)],
    configurations: &[ConfigurationSummary],
    symlink_outputs: &HashSet<(String, usize)>,
    session: &crate::test_runner::Session<'_>,
    keep_going: bool,
) -> Result<scheduler::KeepGoingOutcome, String> {
    let plans: BTreeMap<&Label, &PlannedRun> = run_plans
        .iter()
        .map(|(label, plan)| (label, plan))
        .collect();
    let plan_of = |test: usize| -> Result<(&Label, &PlannedRun), String> {
        let label = session.test_label(test);
        plans
            .get(label)
            .map(|plan| (label, *plan))
            .ok_or_else(|| format!("internal error: {label} was not built as a test"))
    };
    // A test without a plan never runs (test_preflight rules it out).
    for test in 0..session.test_count() {
        plan_of(test)?;
    }
    let planned_actions = nodes.len();
    let leaf_tests = session.leaf_tests();
    let dependencies = leaf_tests
        .iter()
        .map(|&test| plan_of(test).map(|(_, plan)| plan.test_dependencies()))
        .collect::<Result<Vec<_>, String>>()?;
    let run = |leaf: usize, inputs: &scheduler::LeafInputs, interrupt: &AtomicBool| {
        session.run_leaf(leaf, interrupt, || {
            let (label, plan) = plan_of(leaf_tests[leaf])?;
            let by_id: HashMap<&str, &crate::action::ActionResult> = inputs
                .iter()
                .map(|(id, result)| (id.as_str(), &**result))
                .collect();
            run_target(
                label,
                plan,
                configurations,
                symlink_outputs,
                &|produced: &Produced| match &produced.producer {
                    Some((id, index)) => by_id[id.as_str()].outputs[*index].clone(),
                    None => produced.real.clone(),
                },
            )
        });
    };
    let leaves = scheduler::Leaves {
        dependencies,
        cap: session.leaf_cap(),
        run: &run,
    };
    let outcome = if keep_going {
        scheduler::execute_graph_keep_going(options.cache, nodes, options.jobs, Some(&leaves))?
    } else {
        scheduler::KeepGoingOutcome {
            results: scheduler::execute_graph_with_leaves(
                options.cache,
                nodes,
                options.jobs,
                Some(&leaves),
            )?,
            errors: BTreeMap::new(),
        }
    };
    // --notest_keep_going under --nokeep_going: a test that did not pass
    // stopped the build before it completed (its failure is the command's,
    // TestRunnerAction.executeAllAttempts); a complete build reports as usual.
    if session.stopped_the_build() && outcome.results.len() < planned_actions {
        return Err(crate::test_runner::TEST_FAILED_ABORTING.to_owned());
    }
    let results = &outcome.results;
    // --keep_going: an exclusive test runs once its inputs are built.
    session.run_exclusive_built(
        |test| {
            plan_of(test).is_ok_and(|(_, plan)| {
                plan.test_dependencies()
                    .iter()
                    .all(|id| results.contains_key(id))
            })
        },
        |test| {
            let (label, plan) = plan_of(test)?;
            run_target(
                label,
                plan,
                configurations,
                symlink_outputs,
                &|produced: &Produced| executed_path(results, true, produced),
            )
        },
    );
    Ok(outcome)
}

/// RemoteOutputChecker's top-level outputs (crate::remote_outputs): with
/// `--remote_download_outputs=toplevel`, and always in `run`, the requested
/// targets' reported artifacts (in `test` not those of the tests
/// themselves: shouldAddTopLevelTarget), aspect reports, and `run`'s
/// executable, runfiles tree and runfiles.
fn set_remote_downloads(
    command: &RequestCommand,
    requested: &[(Label, Produced)],
    aspect_reports: &[(String, Label, Vec<Produced>)],
    run_plans: &[(Label, PlannedRun)],
) {
    let Some(policy) = crate::remote_outputs::policy() else {
        return;
    };
    let run = matches!(command, RequestCommand::Run { .. });
    if !run && policy.mode != crate::remote_outputs::Mode::Toplevel {
        return;
    }
    let tests: std::collections::HashSet<&Label> = match command {
        RequestCommand::Test { tests, .. } => tests.iter().map(|test| &test.label).collect(),
        _ => Default::default(),
    };
    let mut download = std::collections::HashSet::new();
    let mut add = |produced: &Produced| {
        if let Some((id, index)) = &produced.producer {
            download.insert((id.clone(), *index));
        }
    };
    for (label, produced) in requested {
        if !tests.contains(label) {
            add(produced);
        }
    }
    for (_, _, produced) in aspect_reports {
        produced.iter().for_each(&mut add);
    }
    if run {
        for (_, plan) in run_plans {
            add(&plan.executable);
            if let Some((manifest, _)) = &plan.tree {
                add(manifest);
            }
            for (_, entry) in plan.entries.iter().flatten() {
                if let Some(entry) = entry {
                    add(entry);
                }
            }
        }
    }
    crate::remote_outputs::set_download(download);
}

/// [`execute`], with the test actions of `bazel test` run inside its
/// execution phase when `tests` is given ([`execute_tests`]); a failed build
/// returns its error while the session keeps what its tests reported.
pub fn execute_with_tests(
    mut request: PlannedRequest,
    status: StatusSource,
    options: &ExecuteOptions<'_>,
    tests: Option<&crate::test_runner::Session<'_>>,
) -> Result<Executed, String> {
    request.finalize(status)?;
    // The build event files' analysis events (crate::bep), from the plan:
    // a snapshot hit streams what its full analysis did.
    crate::bep::planned(&request);
    if let Some(path) = options.action_dump
        && let Err(error) = dump_actions(&request.nodes, path)
    {
        return Err(format!("--native-action-dump: {error}"));
    }
    dump_action_graph(&request.nodes);
    let PlannedRequest {
        command,
        skipped,
        keep_going,
        execute,
        nodes,
        requested,
        aspect_reports,
        results: result_targets,
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
    let symlink_outputs = symlink_outputs(&nodes, &run_plans);
    set_remote_downloads(&command, &requested, &aspect_reports, &run_plans);
    let mut action_errors: Vec<(String, String)> = Vec::new();
    let results = if execute {
        crate::perf_mark("execute.start");
        // The runfiles trees link as whole directories in the output tree.
        let runfiles_trees = nodes
            .iter()
            .filter(|node| {
                matches!(
                    node.action.extra.transform,
                    Some(Transform::RunfilesTree { .. })
                )
            })
            .map(|node| node.action.id.clone())
            .collect();
        let scheduler::KeepGoingOutcome { results, errors } = match tests {
            Some(session) => execute_tests(
                options,
                nodes,
                &run_plans,
                &configurations,
                &symlink_outputs,
                session,
                keep_going.is_some(),
            )?,
            None if keep_going.is_some() => {
                scheduler::execute_graph_keep_going(options.cache, nodes, options.jobs, None)?
            }
            None => scheduler::KeepGoingOutcome {
                results: scheduler::execute_graph(options.cache, nodes, options.jobs)?,
                errors: BTreeMap::new(),
            },
        };
        action_errors = errors.into_iter().collect();
        // A failed action completed without a spawn result (internal).
        for _ in &action_errors {
            crate::spawn_stats::action_completed(&[]);
        }
        crate::perf_mark("execute.graph");
        // `$(bazel-rs info bazel-bin)/...` names the built files
        // (crate::output_tree, docs/INFO.md); never fails the build.
        let published = crate::output_tree::publish(options.cache, &results, &runfiles_trees);
        if let Some(error) = published.error {
            crate::note!(
                "bazel-rs: warning: the output tree below execroot/ is incomplete: {error}"
            );
        }
        results
    } else {
        drop(nodes);
        BTreeMap::new()
    };
    crate::perf_mark("execute.end");
    crate::action::report_staging();
    let mut summary = BuildSummary::default();
    if execute {
        summary.skipped_targets = skipped;
    }
    // `build` counts remote cache hits apart; the build part of `test` and
    // `run` counts them as executed (as before the split).
    let remote_hits_apart = matches!(command, RequestCommand::Build);
    // The workspace status nodes (BazelWorkspaceStatusAction#*) execute
    // like any action but are not counted: Bazel reports the status action
    // as an internal process (`INFO: 1 process: 1 internal`), outside the
    // build's action tally.
    // `INFO: N processes: …` and the critical path (crate::spawn_stats):
    // the status nodes are Bazel's one workspace status action, executed
    // (internal) on every build.
    if results.keys().any(|id| id.starts_with(STATUS_ID_PREFIX)) {
        crate::spawn_stats::action_completed(&[]);
    }
    for (id, result) in &results {
        crate::spawn_stats::critical_path_at_least(result.critical);
        if id.starts_with(STATUS_ID_PREFIX) {
            continue;
        }
        if result.cache_hit {
            crate::spawn_stats::action_cache_hit();
        } else {
            crate::spawn_stats::action_completed(result.runner.as_slice());
            if let Some(runner) = result.runner {
                for _ in 0..result.additional_spawns {
                    crate::spawn_stats::spawn(runner);
                }
            }
        }
    }
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
    // --keep_going: the analyzed requested targets whose top-level
    // artifacts did not all build report nothing.
    let keep_going = keep_going.map(|plan| {
        let unbuilt = if execute {
            plan.targets
                .iter()
                .filter(|(label, ids)| {
                    plan.unbuilt.contains(label) || ids.iter().any(|id| !results.contains_key(id))
                })
                .map(|(label, _)| label.clone())
                .collect()
        } else {
            plan.unbuilt.clone()
        };
        KeepGoingResult {
            plan,
            actions: action_errors,
            unbuilt,
        }
    });
    let reported = |label: &Label| keep_going.as_ref().is_none_or(|result| result.built(label));
    let published = |produced: &Produced| executed_path(&results, execute, produced);
    for (label, produced) in &requested {
        if reported(label) {
            summary.outputs.push((label.clone(), published(produced)));
        }
    }
    // The build results (BuildResultPrinter): only after an execution
    // phase, at the artifacts' paths in the output tree (crate::output_tree;
    // the paths Bazel prints without convenience symlinks).
    let execution_root = crate::output_tree::execution_root(options.cache);
    let displayed = |produced: &Produced| execution_root.join(&produced.real);
    if execute {
        for (label, artifacts) in &result_targets {
            if reported(label) {
                summary
                    .results
                    .push((label.clone(), artifacts.iter().map(displayed).collect()));
            } else {
                summary.failed_targets.push(label.clone());
            }
        }
    }
    summary.aspects = aspect_reports
        .iter()
        .filter(|(_, label, _)| execute && reported(label))
        .map(|(aspect, label, artifacts)| {
            (
                aspect.clone(),
                label.clone(),
                artifacts.iter().map(displayed).collect(),
            )
        })
        .collect();
    let mut targets = BTreeMap::new();
    for (label, plan) in &run_plans {
        // --keep_going: a run plan whose artifacts did not all build has no
        // run target.
        if execute
            && keep_going.is_some()
            && !plan
                .test_dependencies()
                .iter()
                .all(|id| results.contains_key(id))
        {
            continue;
        }
        let target = run_target(label, plan, &configurations, &symlink_outputs, &published)?;
        targets.insert(label.clone(), target);
    }
    Ok(Executed {
        summary,
        targets,
        command,
        execute,
        keep_going,
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
        if let Some(java) = &action.extra.java_compile {
            eprintln!("  java-mode {:?}", java.mode);
            eprintln!("  java-flags {}", java.flags.join("\u{1}"));
            for (name, paths) in [
                ("classpath", &java.classpath),
                ("direct", &java.direct_jars),
                ("jdeps", &java.dependency_jdeps),
                ("mandatory", &java.mandatory_inputs),
            ] {
                eprintln!(
                    "  java-{name} {}",
                    paths
                        .iter()
                        .map(|path| path.display().to_string())
                        .collect::<Vec<_>>()
                        .join("\u{1}")
                );
            }
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
                remote_executed: false,
                runner: None,
                additional_spawns: 0,
                critical: std::time::Duration::ZERO,
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
                    destination: PathBuf::from("bazel-out/stable-status.txt").into(),
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
            skipped: Vec::new(),
            command: RequestCommand::Build,
            keep_going: None,
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
            results: vec![(Label::parse_canonical("//:t").unwrap(), vec![stable()])],
            run_plans: Vec::new(),
            configurations: Vec::new(),
            warnings: Vec::new(),
            errors: Vec::new(),
            build_info: Vec::new(),
            bep: Default::default(),
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
                completion: Vec::new(),
                coverage: None,
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
                completion: Vec::new(),
                coverage: None,
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

    /// A shared input group below `bazel-out/tool.runfiles` with one
    /// generated input made by `producer`.
    fn tool_group(destination: &str, producer: &str) -> Arc<InputGroup> {
        Arc::new(InputGroup {
            root: PathBuf::from("bazel-out/tool.runfiles"),
            inputs: Vec::new(),
            dependencies: vec![Dependency {
                action_id: producer.to_owned(),
                output_index: 0,
                destination: PathBuf::from("bazel-out/tool.runfiles")
                    .join(destination)
                    .into(),
            }],
            tree_inputs: Vec::new(),
            symlink_inputs: Vec::new(),
            constant_metadata: Vec::new(),
            empty_files: Vec::new(),
            sources: Vec::new(),
            valid: true,
        })
    }

    /// PA-1 x W4 (integration-21): a tool's runfiles can hold the stable
    /// status file, so a failed status redirects a shared input group's
    /// dependency on `#stable` too — each affected group rebuilt once and
    /// shared again by all its holders, every other group untouched — and
    /// no dependency on the removed `#stable` node remains (the scheduler
    /// would refuse the graph: `unknown dependency`).
    #[test]
    fn a_failed_status_redirects_the_shared_input_groups_holding_the_stable_file() {
        let stamped = tool_group("_main/stable-status.txt", STABLE_STATUS_ID);
        let plain = tool_group("_main/out", "//:first");
        let holders = |request: &mut PlannedRequest| {
            for (index, groups) in [vec![&stamped, &plain], vec![&stamped]]
                .into_iter()
                .enumerate()
            {
                let mut holder = node(&format!("//:holder{index}"), None, &[]);
                holder.groups = groups.into_iter().map(Arc::clone).collect();
                request.nodes.push(holder);
            }
        };
        let mut request = with_placeholders();
        holders(&mut request);
        request
            .finalize(StatusSource::Planned(Some(
                ComputedWorkspaceStatus::Failed(crate::workspace_status::WorkspaceStatusError(
                    "Process exited with status 1".to_owned(),
                )),
            )))
            .unwrap();
        let held: Vec<&Vec<Arc<InputGroup>>> = request
            .nodes
            .iter()
            .filter(|node| node.action.id.starts_with("//:holder"))
            .map(|node| &node.groups)
            .collect();
        let rebuilt = &held[0][0];
        assert!(!Arc::ptr_eq(rebuilt, &stamped));
        assert!(Arc::ptr_eq(rebuilt, &held[1][0]), "shared again");
        assert_eq!(rebuilt.dependencies[0].action_id, VOLATILE_STATUS_ID);
        assert_eq!(
            rebuilt.dependencies[0].destination,
            stamped.dependencies[0].destination
        );
        assert!(Arc::ptr_eq(&held[0][1], &plain), "untouched");
        // The planned group itself is not changed in place.
        assert_eq!(stamped.dependencies[0].action_id, STABLE_STATUS_ID);
        assert!(request.nodes.iter().all(|node| {
            node.all_dependencies()
                .all(|dependency| dependency.action_id != STABLE_STATUS_ID)
        }));
        // A successful status keeps every group as planned.
        let mut request = with_placeholders();
        holders(&mut request);
        request
            .finalize(StatusSource::Planned(Some(ComputedWorkspaceStatus::Ok(
                status(),
            ))))
            .unwrap();
        let holder = request
            .nodes
            .iter()
            .find(|node| node.action.id == "//:holder0")
            .unwrap();
        assert!(Arc::ptr_eq(&holder.groups[0], &stamped));
        assert!(Arc::ptr_eq(&holder.groups[1], &plain));
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
