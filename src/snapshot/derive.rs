//! Derived hits (docs/ANALYSIS_SNAPSHOT.md "Derived hits"): a `build` or
//! `test` whose targets are a subset of a saved snapshot's executes that
//! snapshot's plan restricted to them, instead of analyzing them again.
//!
//! Genuine Bazel keeps one server: a `bazel test` after a `bazel build` of
//! more targets, with options that differ only in what selects targets and
//! runs tests, configures nothing again (`INFO: Analyzed 152 targets (0
//! packages loaded, 0 targets configured)`). An exact snapshot hit needs
//! the same command and the same patterns, so ~/code's CI sequence (`build
//! --keep_going --target_pattern_file F1`, then `test ... --test_tag_filters
//! ... --target_pattern_file F2` of a subset) paid the full analysis twice.
//!
//! * **Family key F** ([`crate::snapshot::key::family`]): G without the
//!   command (`build` and `test` alike, unless `--output_groups` is given:
//!   `test` keeps the `default` group), the target patterns and the options
//!   that only select targets or say whether to execute (`--keep_going`,
//!   `--[no]build`, `--target_pattern_file`, the test filters,
//!   `--build_tests_only`, `--build_tag_filters`, `--expand_test_suites`).
//!   Every other option of G is in F, so the analysis of a configured
//!   target is the same in every snapshot of a family.
//! * **Recording** ([`Coverage`], the DERIVE section): a full analysis of a
//!   `build` or `test` that the snapshot saves records, per requested
//!   label, the producers of what building it builds (requested and hidden
//!   top-level artifacts) and, for a test rule, what running it in a `test`
//!   builds with its run plan — the one the `test` planned, or one planned
//!   on the side for a test rule that was only built (crate::engine). A
//!   marker `<state>/snapshots/derive/<F>/<G>` names the snapshot.
//! * **Lookup** (crate::main, after the target pattern phase, which selects
//!   the labels, expands test suites and applies the filters exactly as
//!   before): a snapshot of the family whose labels cover the request's is
//!   validated like an exact hit (every recorded input unchanged), its plan
//!   decoded and [`derive`]d: the actions reachable from the requested
//!   labels' roots, in the saved order; their requested artifacts, results,
//!   run plans and build events. Analysis prints nothing but `INFO:
//!   Analyzed N targets (P packages loaded, 0 targets configured).` and the
//!   `Found` line (P: what the target pattern phase loaded), like a warm
//!   Bazel server, which neither replays the analysis-time `print()`s and
//!   warnings of cached targets. A derived run is not saved; a candidate
//!   that fails validation loses its marker (its own key still hits), and
//!   the full analysis runs, unsaved.
//!
//! `BAZEL_RS_SNAPSHOT_DERIVE=0` turns derivation off (neither recorded nor
//! looked up).

use crate::label::Label;
use crate::request::{
    ConfigurationSummary, KeepGoingPlan, PlannedRequest, PlannedRun, PlannedTest, RequestCommand,
};
use crate::snapshot::key::Key;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::path::{Path, PathBuf};

/// Whether derived hits are on (`BAZEL_RS_SNAPSHOT_DERIVE`, default on).
pub fn enabled() -> bool {
    !matches!(
        std::env::var_os("BAZEL_RS_SNAPSHOT_DERIVE").as_deref(),
        Some(value) if value == "0" || value == "off" || value == "false"
    )
}

/// What a snapshot's plan offers a request of a subset of its targets (the
/// DERIVE section).
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Coverage {
    /// Whether the plan executes (planned without `--nobuild`).
    pub execute: bool,
    /// Of a `--nobuild` plan: it traversed every source directory an
    /// executing build of it would, without a failure (`bazel-rs affected`'s
    /// saved head planning, crate::affected::SAVE_VAR): its actions reading
    /// a source directory may serve an executing build (the traversal's
    /// reads are recorded inputs, validated like any).
    pub source_directories_checked: bool,
    /// Of a `--nobuild` plan: the roots whose actions read the workspace
    /// status (which it does not produce) or a source directory (which it
    /// never traversed), themselves or below — no executing build derives
    /// them.
    pub unexecutable: BTreeSet<String>,
    /// The requested labels in request order, each with the producers of
    /// what building it builds.
    pub targets: Vec<(Label, Vec<String>)>,
    /// The test rules among them, with what running them builds.
    pub runs: Vec<Run>,
    /// The configurations the side run plans index.
    pub configurations: Vec<ConfigurationSummary>,
}

/// A test rule of the snapshot's request and what a `test` of it runs.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Run {
    pub test: PlannedTest,
    /// The producers of what running it builds (its completion).
    pub roots: Vec<String>,
    /// Planned with the runfiles tree of `run` (`RequestSpec::trees`).
    pub trees: bool,
    /// A run plan planned on the side (the label was only built): its
    /// configuration indexes [`Coverage::configurations`]. `None`: the
    /// request's own run plan of the label.
    pub side: Option<PlannedRun>,
}

/// The coverage the last planned build recorded, for its save.
static PLANNED: std::sync::Mutex<Option<Coverage>> = std::sync::Mutex::new(None);

/// Keep `coverage` for the save of this command.
pub fn set_planned(coverage: Option<Coverage>) {
    *PLANNED
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = coverage;
}

/// Take the recorded coverage (the save writes it).
pub fn take_planned() -> Option<Coverage> {
    PLANNED
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .take()
}

/// The DERIVE section's bytes (compressed like the plan).
pub fn encode(coverage: &Coverage) -> Result<Vec<u8>, String> {
    let bytes = postcard::to_stdvec(coverage).map_err(|error| error.to_string())?;
    Ok(crate::snapshot::format::assemble_compressed(vec![
        crate::snapshot::format::compress_part(bytes),
    ]))
}

/// [`encode`]'s inverse (the section's checksum already passed).
pub fn decode(stored: Vec<u8>) -> Result<Coverage, String> {
    let bytes = crate::snapshot::format::decompress_plan(stored)?;
    postcard::from_bytes(&bytes).map_err(|error| format!("malformed DERIVE section: {error}"))
}

/// `<state>/snapshots/derive/<F>`: the markers of a family's snapshots.
fn family_directory(cache: &Path, family: &[u8; 32]) -> PathBuf {
    crate::snapshot::store::root(cache)
        .join("derive")
        .join(crate::observed_fs::hex(&family[..16]))
}

/// Name the saved snapshot `key` in its family.
pub fn mark(cache: &Path, family: &[u8; 32], key: &Key) {
    let directory = family_directory(cache, family);
    if std::fs::create_dir_all(&directory).is_ok() {
        let _ = std::fs::write(directory.join(key.short()), crate::observed_fs::hex(&key.g));
    }
}

/// Forget the snapshot `key` in its family (it did not validate).
pub fn unmark(cache: &Path, family: &[u8; 32], key: &Key) {
    let _ = std::fs::remove_file(family_directory(cache, family).join(key.short()));
}

/// The request a derivation serves.
pub struct Request<'a> {
    /// `Build`, or `Test` with its tests (their properties are the
    /// snapshot's).
    pub command: RequestCommand,
    pub build: &'a [Label],
    pub runs: &'a [Label],
    pub trees: bool,
    pub execute: bool,
    pub keep_going: bool,
}

impl Coverage {
    /// Whether this snapshot can serve `request` as far as its labels go
    /// (the plan decides the rest, [`derive`]).
    pub fn covers(&self, request: &Request<'_>) -> Result<(), String> {
        let runs_of: HashMap<&Label, &Run> =
            self.runs.iter().map(|run| (&run.test.label, run)).collect();
        if request.execute && !self.execute && !self.unexecutable.is_empty() {
            let build: HashMap<&Label, &Vec<String>> = self
                .targets
                .iter()
                .map(|(label, roots)| (label, roots))
                .collect();
            let unexecutable = |roots: Option<&Vec<String>>| {
                roots.is_some_and(|roots| roots.iter().any(|id| self.unexecutable.contains(id)))
            };
            let label = request
                .build
                .iter()
                .find(|label| unexecutable(build.get(label).copied()))
                .or_else(|| {
                    request
                        .runs
                        .iter()
                        .find(|label| unexecutable(runs_of.get(label).map(|run| &run.roots)))
                });
            if let Some(label) = label {
                return Err(format!(
                    "{label} needs actions reading the workspace status or a source directory, which its --nobuild plan does not check"
                ));
            }
        }
        let targets: HashSet<&Label> = self.targets.iter().map(|(label, _)| label).collect();
        if let Some(label) = request.build.iter().find(|label| !targets.contains(label)) {
            return Err(format!("{label} is not one of its targets"));
        }
        let runs: HashMap<&Label, &Run> =
            self.runs.iter().map(|run| (&run.test.label, run)).collect();
        for label in request.runs {
            match runs.get(label) {
                None => return Err(format!("{label} is not one of its test rules")),
                Some(run) if run.trees != request.trees => {
                    return Err(format!("{label}'s run plan has another runfiles tree"));
                }
                Some(_) => {}
            }
        }
        Ok(())
    }
}

/// The candidates of `family`: each saved snapshot key with its coverage,
/// the most recently saved first. Unreadable ones are skipped.
pub fn candidates(cache: &Path, family: &[u8; 32], binary: [u8; 32]) -> Vec<(Key, Coverage)> {
    let directory = family_directory(cache, family);
    let Ok(entries) = std::fs::read_dir(&directory) else {
        return Vec::new();
    };
    let mut markers: Vec<(std::time::SystemTime, PathBuf)> = entries
        .flatten()
        .filter_map(|entry| {
            let modified = entry.metadata().ok()?.modified().ok()?;
            Some((modified, entry.path()))
        })
        .collect();
    markers.sort_by_key(|marker| std::cmp::Reverse(marker.0));
    let mut found = Vec::new();
    for (_, marker) in markers {
        let Some(g) = std::fs::read_to_string(&marker)
            .ok()
            .and_then(|text| parse_hex(text.trim()))
        else {
            let _ = std::fs::remove_file(&marker);
            continue;
        };
        let key = Key { g, binary };
        let reader = match crate::snapshot::store::Reader::open(cache, &key) {
            Ok(Some(reader)) => reader,
            // Evicted, damaged or of another binary: the marker goes.
            _ => {
                let _ = std::fs::remove_file(&marker);
                continue;
            }
        };
        match reader
            .stored_section(crate::snapshot::format::Section::Derive)
            .and_then(decode)
        {
            Ok(coverage) => found.push((key, coverage)),
            Err(_) => {
                let _ = std::fs::remove_file(&marker);
            }
        }
    }
    found
}

fn parse_hex(text: &str) -> Option<[u8; 32]> {
    if text.len() != 64 {
        return None;
    }
    let mut bytes = [0u8; 32];
    for (index, byte) in bytes.iter_mut().enumerate() {
        *byte = u8::from_str_radix(text.get(index * 2..index * 2 + 2)?, 16).ok()?;
    }
    Some(bytes)
}

/// The exec paths of the workspace status files.
const STATUS_PATHS: [&str; 2] = [
    "bazel-out/stable-status.txt",
    "bazel-out/volatile-status.txt",
];

/// The workspace status placeholder nodes of an executing plan
/// (crate::engine's ensure_workspace_status).
fn status_placeholders() -> [crate::scheduler::PlannedAction; 2] {
    let placeholder = |id: &str, output: &str, volatile: bool| crate::scheduler::PlannedAction {
        action: crate::action::Action {
            id: id.to_owned(),
            outputs: vec![PathBuf::from(output)],
            extra: crate::action::ActionExtra {
                transform: Some(crate::action::Transform::WorkspaceStatus { volatile }),
                mnemonic: "BazelWorkspaceStatusAction".to_owned(),
                ..crate::action::ActionExtra::default()
            },
            ..crate::action::Action::default()
        },
        dependencies: Vec::new(),
        deferred: None,
        groups: Vec::new(),
    };
    [
        placeholder(crate::request::STABLE_STATUS_ID, STATUS_PATHS[0], false),
        placeholder(crate::request::VOLATILE_STATUS_ID, STATUS_PATHS[1], true),
    ]
}

/// The plan of `request` from the saved plan `source` and its `coverage`:
/// what a full analysis of the request plans, the shared actions' owners
/// and the plan order aside (both are the saved analysis's).
pub fn derive(
    source: PlannedRequest,
    coverage: &Coverage,
    request: Request<'_>,
) -> Result<PlannedRequest, String> {
    coverage.covers(&request)?;
    if !source.aspect_reports.is_empty() {
        return Err("the snapshot's request has top-level aspects".to_owned());
    }
    if !source.skipped.is_empty() {
        return Err("the snapshot's request skipped incompatible targets".to_owned());
    }
    if !source.errors.is_empty()
        || source.keep_going.as_ref().is_some_and(|plan| {
            !plan.incompatible.is_empty()
                || !plan.errors.is_empty()
                || !plan.failed.is_empty()
                || !plan.input_errors.is_empty()
                || !plan.unbuilt.is_empty()
        })
    {
        return Err("the snapshot's analysis reported errors".to_owned());
    }
    let build_roots: HashMap<&Label, &Vec<String>> = coverage
        .targets
        .iter()
        .map(|(label, roots)| (label, roots))
        .collect();
    let runs: HashMap<&Label, &Run> = coverage
        .runs
        .iter()
        .map(|run| (&run.test.label, run))
        .collect();
    let running: HashSet<&Label> = request.runs.iter().collect();
    // Each label's roots: what running it builds when it runs, else what
    // building it builds.
    let mut label_roots: BTreeMap<&Label, &Vec<String>> = BTreeMap::new();
    for label in request.build.iter().chain(request.runs) {
        let roots = if running.contains(label) {
            &runs[label].roots
        } else {
            build_roots[label]
        };
        label_roots.insert(label, roots);
    }
    let source_has_status = source
        .nodes
        .iter()
        .any(|node| node.action.id == crate::request::STABLE_STATUS_ID);
    let mut todo: Vec<&str> = label_roots
        .values()
        .flat_map(|roots| roots.iter().map(String::as_str))
        .collect();
    // An executing build builds the workspace status unconditionally.
    if request.execute {
        todo.push(crate::request::STABLE_STATUS_ID);
        todo.push(crate::request::VOLATILE_STATUS_ID);
    }
    let by_id: HashMap<&str, &crate::scheduler::PlannedAction> = source
        .nodes
        .iter()
        .map(|node| (node.action.id.as_str(), node))
        .collect();
    let mut required: HashSet<&str> = HashSet::new();
    while let Some(id) = todo.pop() {
        if !required.insert(id) {
            continue;
        }
        let Some(node) = by_id.get(id) else {
            if id.starts_with(crate::request::STATUS_ID_PREFIX) && !source_has_status {
                continue;
            }
            return Err(format!("the snapshot's plan has no action {id}"));
        };
        todo.extend(
            node.all_dependencies()
                .map(|dependency| dependency.action_id.as_str()),
        );
    }
    let is_status = |id: &str| id.starts_with(crate::request::STATUS_ID_PREFIX);
    if request.execute && !source.execute {
        // A `--nobuild` plan has no status action: its consumers name the
        // status files without a producer, and a source directory input
        // was never traversed (the executing analysis's checks).
        for id in &required {
            let Some(node) = by_id.get(id) else {
                continue;
            };
            if node
                .inputs()
                .any(|(_, exec)| STATUS_PATHS.iter().any(|status| exec == Path::new(status)))
            {
                return Err(format!(
                    "{id} reads the workspace status, which the saved --nobuild plan does not produce"
                ));
            }
            if !coverage.source_directories_checked
                && !node.action.extra.source_directories.is_empty()
            {
                return Err(format!(
                    "{id} reads a source directory, which the saved --nobuild plan never checked"
                ));
            }
        }
    }
    if !request.execute && source.execute {
        for id in &required {
            if by_id.get(id).is_some_and(|node| {
                node.all_dependencies()
                    .any(|dependency| is_status(&dependency.action_id))
            }) {
                return Err(format!(
                    "{id} reads the workspace status, which a --nobuild plan does not produce"
                ));
            }
        }
    }
    let required: HashSet<String> = required
        .into_iter()
        .filter(|id| request.execute || !is_status(id))
        .map(str::to_owned)
        .collect();
    let PlannedRequest {
        nodes,
        requested,
        results,
        run_plans,
        configurations,
        build_info,
        bep,
        ..
    } = source;
    let mut nodes: Vec<_> = nodes
        .into_iter()
        .filter(|node| required.contains(&node.action.id))
        .collect();
    if request.execute && !source_has_status {
        nodes.extend(status_placeholders());
    }
    // The requested artifacts in request order (the indices the build
    // events name move with them).
    let mut by_label: HashMap<Label, Vec<usize>> = HashMap::new();
    for (index, (label, _)) in requested.iter().enumerate() {
        by_label.entry(label.clone()).or_default().push(index);
    }
    let mut moved: HashMap<usize, usize> = HashMap::new();
    let mut order = Vec::new();
    for label in request.build {
        for index in by_label.get(label).into_iter().flatten() {
            moved.insert(*index, order.len());
            order.push(*index);
        }
    }
    let mut slots: Vec<Option<(Label, crate::request::Produced)>> =
        requested.into_iter().map(Some).collect();
    let requested: Vec<_> = order
        .iter()
        .map(|index| slots[*index].take().expect("each index once"))
        .collect();
    // The results in request order. A label without one reports nothing,
    // unless it is an output file whose generating rule the snapshot's
    // request built too (BuildResultPrinter shows it under its rule, which
    // this request may not build).
    let mut results_by_label: HashMap<Label, Vec<crate::request::Produced>> =
        results.into_iter().collect();
    let mut derived_results = Vec::new();
    for label in request.build {
        match results_by_label.remove(label) {
            Some(artifacts) => derived_results.push((label.clone(), artifacts)),
            None => {
                if requested
                    .iter()
                    .any(|(owner, artifact)| owner == label && artifact.producer.is_some())
                {
                    return Err(format!(
                        "{label} reports no result in the snapshot's request"
                    ));
                }
            }
        }
    }
    // The run plans (by label, as planning keeps them), the side plans'
    // configurations appended.
    let mut configurations = configurations;
    let mut side_index: HashMap<usize, usize> = HashMap::new();
    let mut own_plans: HashMap<Label, PlannedRun> = run_plans.into_iter().collect();
    let mut derived_runs: BTreeMap<Label, PlannedRun> = BTreeMap::new();
    for label in request.runs {
        let run = runs[label];
        let plan = match &run.side {
            None => own_plans
                .remove(label)
                .ok_or_else(|| format!("the snapshot's request has no run plan of {label}"))?,
            Some(plan) => {
                let mut plan = plan.clone();
                let index = match side_index.get(&plan.configuration) {
                    Some(index) => *index,
                    None => {
                        let summary = coverage
                            .configurations
                            .get(plan.configuration)
                            .ok_or("a side run plan names no configuration")?
                            .clone();
                        let index = match configurations.iter().position(|known| *known == summary)
                        {
                            Some(index) => index,
                            None => {
                                configurations.push(summary);
                                configurations.len() - 1
                            }
                        };
                        side_index.insert(plan.configuration, index);
                        index
                    }
                };
                plan.configuration = index;
                plan
            }
        };
        derived_runs.insert(label.clone(), plan);
    }
    // The tests' properties: the snapshot's (planning reads them once more
    // after the analysis).
    let command = match request.command {
        RequestCommand::Test { build, tests } => RequestCommand::Test {
            build,
            tests: tests
                .into_iter()
                .map(|test| match runs.get(&test.label) {
                    Some(run) => run.test.clone(),
                    None => test,
                })
                .collect(),
        },
        other => other,
    };
    let keep_going = request.keep_going.then(|| KeepGoingPlan {
        incompatible: BTreeMap::new(),
        top_level: request.build.len(),
        errors: Vec::new(),
        failed: Vec::new(),
        targets: request
            .build
            .iter()
            .collect::<BTreeSet<_>>()
            .into_iter()
            .map(|label| (label.clone(), label_roots[label].clone()))
            .collect(),
        input_errors: Vec::new(),
        unbuilt: Vec::new(),
    });
    // The build events of the requested targets, in request order: a test
    // keeps the top-level configuration, any other target the trimmed one.
    let mut bep_targets: HashMap<String, crate::bep::plan::BepTarget> = bep
        .targets
        .into_iter()
        .map(|target| (target.label.clone(), target))
        .collect();
    let mut bep_configurations = bep
        .configurations
        .iter()
        .take(1)
        .cloned()
        .collect::<Vec<_>>();
    let mut targets = Vec::new();
    for label in request.build {
        let mut target = bep_targets
            .remove(&crate::test_runner::label_print(label))
            .ok_or_else(|| format!("the snapshot's build events have no {label}"))?;
        target.configuration = if target.test.is_some() {
            0
        } else {
            if bep_configurations.len() == 1 {
                bep_configurations.push(
                    bep.configurations
                        .get(1)
                        .ok_or("the snapshot's build events have no trimmed configuration")?
                        .clone(),
                );
            }
            1
        };
        for (_, files) in &mut target.groups {
            for file in files.iter_mut() {
                *file = *moved
                    .get(file)
                    .ok_or("a build event names an artifact of another target")?;
            }
        }
        targets.push(target);
    }
    let kept: HashSet<&str> = nodes.iter().map(|node| node.action.id.as_str()).collect();
    let build_info = build_info
        .into_iter()
        .filter(|id| kept.contains(id.as_str()))
        .collect();
    Ok(PlannedRequest {
        command,
        skipped: Vec::new(),
        keep_going,
        execute: request.execute,
        nodes,
        requested,
        aspect_reports: Vec::new(),
        results: derived_results,
        run_plans: derived_runs.into_iter().collect(),
        configurations,
        warnings: Vec::new(),
        errors: Vec::new(),
        build_info,
        bep: crate::bep::plan::BepPlan {
            configurations: bep_configurations,
            targets,
        },
    })
}

/// What a derived lookup needs from the check point (crate::main keeps it
/// until the target pattern phase selected the request's labels).
pub struct Lookup {
    pub cache: PathBuf,
    pub replay: crate::snapshot::hit::ReplayInputs,
    pub workspace_status: crate::workspace_status::WorkspaceStatusOptions,
    pub workspace: PathBuf,
}

/// The outcome of [`attempt`].
pub enum Outcome {
    /// The derived request: its plan executes; nothing is saved.
    Hit(Box<crate::snapshot::hit::Hit>),
    /// No valid snapshot of the family covers the request: the full
    /// analysis runs, saved as before (a covering one whose inputs changed
    /// left its family).
    Uncovered(Box<crate::snapshot::save::Save>),
    /// A covering snapshot validated, but its hit failed (its replay, its
    /// local trees, its plan): the full analysis runs, unsaved (the window
    /// closed); the snapshot left its family.
    Failed,
}

/// Look for a snapshot of `save`'s family covering `request` and derive it.
pub fn attempt(
    lookup: &Lookup,
    save: Box<crate::snapshot::save::Save>,
    request: Request<'_>,
) -> Outcome {
    let explain = crate::snapshot::explain();
    let Some(family) = save.family() else {
        return Outcome::Uncovered(save);
    };
    let key = save.key();
    // The most recent covering candidate whose recorded inputs hold — checked
    // while this command can still save its own analysis: a stale one
    // leaves its family.
    let mut chosen = None;
    for (candidate, coverage) in candidates(&lookup.cache, &family, key.binary) {
        if candidate == key {
            continue;
        }
        let checked = coverage.covers(&request).and_then(|()| {
            crate::snapshot::hit::prevalidate(&lookup.cache, &candidate, &lookup.replay.environment)
                .inspect_err(|_| unmark(&lookup.cache, &family, &candidate))
        });
        match checked {
            Ok(()) => {
                chosen = Some((candidate, coverage));
                break;
            }
            Err(reason) => {
                if explain {
                    crate::observed_fs::diag(&format!(
                        "derive {}: not from {}: {reason}",
                        key.short(),
                        candidate.short()
                    ));
                }
            }
        }
    }
    let Some((candidate, coverage)) = chosen else {
        return Outcome::Uncovered(save);
    };
    let log = save.log();
    save.abandon(&format!("derived from {}", candidate.short()));
    let targets = request.build.len();
    let outcome = crate::snapshot::hit::attempt(crate::snapshot::hit::Attempt {
        cache: &lookup.cache,
        key: candidate,
        replay: crate::snapshot::hit::ReplayInputs {
            options: lookup.replay.options.clone(),
            environment: lookup.replay.environment.clone(),
            downloads_enabled: lookup.replay.downloads_enabled,
        },
        workspace_status: lookup.workspace_status.clone(),
        workspace: &lookup.workspace,
        verify: false,
        log,
        incremental: false,
        derive: Some(Box::new(move |source| derive(source, &coverage, request))),
    });
    match outcome {
        Ok(hit) => {
            if explain {
                crate::observed_fs::diag(&format!(
                    "derived {} from {}: {} target(s), {} action(s); {}",
                    key.short(),
                    candidate.short(),
                    targets,
                    hit.request.nodes.len(),
                    hit.summary
                ));
            }
            Outcome::Hit(hit)
        }
        Err((reason, _)) => {
            unmark(&lookup.cache, &family, &candidate);
            if explain {
                crate::observed_fs::diag(&format!(
                    "derive {} from {}: miss: {reason} (the full analysis runs, unsaved)",
                    key.short(),
                    candidate.short()
                ));
            }
            Outcome::Failed
        }
    }
}
