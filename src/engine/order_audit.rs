//! T1 of the evaluator plan (docs/EVALUATOR.md, "T1: order coupling
//! audit"): `BAZEL_RS_ORDER_AUDIT=1` measures how much of a build's phase
//! 2 reads state that another target's **merge** writes, rather than its
//! dependencies' results — what keeps phase 2 from running in any order
//! with a plan-ordered merge.
//!
//! What it counts (the coordinator's merge and the planning state at the
//! end of phase 2; prepares on any thread note through one mutex):
//!
//! * shared actions: every output registered as shared with an equal
//!   action of another owner, the winning action ids (the first owner's in
//!   plan order), the contested output paths, and action conflicts;
//! * the readers of a producer choice (a scan of the final planning
//!   state): every planned target whose own result names a winning action
//!   id — its planned nodes' generated inputs and shared input groups,
//!   its files to build (`dry`), its runfiles support's artifacts —
//!   besides the winner's own;
//! * the output-file label resolutions (`declared(config, path)`) and how
//!   many resolve a contested path;
//! * target-platform reads (resolved or deferred) of a platform that phase
//!   2 resolved itself (a deferred platform: the read depends on which
//!   commit came first);
//! * the workspace status planned on first need; aspect applications
//!   planned inside a dependent's prepare and the memo hits of the others;
//! * why the commit deltas were (re)built where they were.
//!
//! Diagnostic only: off by default, every hook is behind [`enabled`]; the
//! report goes to the diagnostics stream (`ORDER-AUDIT` lines).

use super::*;

/// Whether `BAZEL_RS_ORDER_AUDIT=1` asks for the audit.
pub(super) fn enabled() -> bool {
    static ON: std::sync::LazyLock<bool> = std::sync::LazyLock::new(|| {
        // snapshot-unrecorded: a diagnostic switch; it never changes the plan.
        std::env::var_os("BAZEL_RS_ORDER_AUDIT").is_some_and(|value| value == "1")
    });
    *ON
}

#[derive(Default)]
struct Audit {
    /// Outputs registered as shared (a later owner's equal action).
    shared: u64,
    /// The winning action ids (the action the first owner registered), with
    /// the owners that shared it.
    winners: crate::fast_hash::FxHashMap<String, Vec<Sharer>>,
    /// The output paths of shared actions.
    contested: crate::fast_hash::FxHashSet<PathBuf>,
    /// Action and prefix conflicts met by an output registration.
    conflicts: u64,
    /// Output-file label resolutions through `declared`, by path.
    output_files: crate::fast_hash::FxHashMap<PathBuf, u64>,
    /// Target-platform reads by platform key: (deferred, resolved).
    platform_reads: crate::fast_hash::FxHashMap<ConfiguredTargetKey, (u64, u64)>,
    /// Platforms phase 2 resolved (a deferred target platform).
    platforms_resolved: BTreeSet<ConfiguredTargetKey>,
    /// The workspace status planned on first need.
    status_first_need: u64,
    /// Aspect applications planned in a dependent's prepare, and the
    /// applications another dependent found planned.
    aspects_planned: u64,
    aspect_hits: u64,
    /// Where each Starlark commit delta was built (Engine::commit_custom_target).
    deltas: BTreeMap<&'static str, u64>,
    /// The artifacts dependency views were built from (their Files only
    /// reach the view), by path.
    view_artifacts: crate::fast_hash::FxHashMap<PathBuf, u64>,
}

/// An owner that shared an action: a configured target (a commit delta's
/// merge) or an action owner's display (an aspect application).
pub(super) enum Sharer {
    Target(ConfiguredTargetKey),
    Owner(String),
}

impl Sharer {
    fn owner(&self, top: ConfigId) -> String {
        match self {
            Sharer::Target(key) => key.action_owner(top),
            Sharer::Owner(owner) => owner.clone(),
        }
    }
}

fn audit() -> std::sync::MutexGuard<'static, Audit> {
    static AUDIT: std::sync::LazyLock<std::sync::Mutex<Audit>> =
        std::sync::LazyLock::new(Default::default);
    AUDIT.lock().unwrap_or_else(|e| e.into_inner())
}

/// An output of `owner` registered as shared with the action `winner`.
pub(super) fn note_shared(path: &Path, winner: Option<&str>, owner: Sharer) {
    let mut audit = audit();
    audit.shared += 1;
    audit.contested.insert(path.to_path_buf());
    if let Some(winner) = winner {
        audit
            .winners
            .entry(winner.to_owned())
            .or_default()
            .push(owner);
    }
}

/// An output registration met a conflict.
pub(super) fn note_conflict() {
    audit().conflicts += 1;
}

/// An output-file label resolved through `declared`.
pub(super) fn note_output_file(path: &Path) {
    *audit().output_files.entry(path.to_path_buf()).or_default() += 1;
}

/// A dependency view was built from `artifacts` (Engine::dependency_view_in).
pub(super) fn note_view_artifacts(artifacts: &[Artifact]) {
    let mut audit = audit();
    for artifact in artifacts {
        *audit
            .view_artifacts
            .entry(artifact.exec.to_path_buf())
            .or_default() += 1;
    }
}

/// A prepare or commit read the target platform `key`.
pub(super) fn note_platform_read(key: &ConfiguredTargetKey, resolved: bool) {
    let mut audit = audit();
    let entry = audit.platform_reads.entry(key.clone()).or_default();
    if resolved {
        entry.1 += 1;
    } else {
        entry.0 += 1;
    }
}

/// Phase 2 resolved the deferred target platform `key`.
pub(super) fn note_platform_resolved(key: ConfiguredTargetKey) {
    audit().platforms_resolved.insert(key);
}

/// The workspace status was planned on first need.
pub(super) fn note_status_first_need() {
    audit().status_first_need += 1;
}

/// An aspect application planned in a dependent's prepare (`planned`), or
/// found planned by another one.
pub(super) fn note_aspect(planned: bool) {
    let mut audit = audit();
    if planned {
        audit.aspects_planned += 1;
    } else {
        audit.aspect_hits += 1;
    }
}

/// Where a Starlark commit delta was built.
pub(super) fn note_delta(reason: &'static str) {
    *audit().deltas.entry(reason).or_default() += 1;
}

/// The owner of an action id (`owner#n`, `owner#runfiles_tree`, …).
fn owner_of(id: &str) -> &str {
    id.split_once('#').map_or(id, |(owner, _)| owner)
}

/// The readers of the producer choices, found in the final planning state.
#[derive(Default)]
struct Readers {
    /// Owners (action owners) whose result names a winning action id that
    /// is not their own.
    contested: BTreeSet<String>,
    /// Of them, the owners that shared an action themselves (a loser reads
    /// the winner's artifacts for its own outputs).
    losers: BTreeSet<String>,
    /// References by where they are found.
    node_inputs: u64,
    group_inputs: u64,
    dry: u64,
    runfiles: u64,
    /// The contested readers by where they name a winner: planned nodes'
    /// own inputs, shared input groups, files to build, runfiles support.
    by_place: [BTreeSet<String>; 4],
    /// Owners naming only their own winning actions.
    winners_only: BTreeSet<String>,
    /// Owners whose nodes read the workspace status.
    status: BTreeSet<String>,
}

/// The report, at the end of phase 2 (top-level aspects included).
pub(super) fn report(state: &ActionPlanning, plan: usize, top: ConfigId) {
    let audit = audit();
    let losers: BTreeSet<String> = audit
        .winners
        .values()
        .flatten()
        .map(|sharer| sharer.owner(top))
        .collect();
    let mut readers = Readers::default();
    let note = |readers: &mut Readers, reader: &str, id: &str, place: u8| {
        if !audit.winners.contains_key(id) {
            return;
        }
        if owner_of(id) == reader {
            readers.winners_only.insert(reader.to_owned());
            return;
        }
        match place {
            0 => readers.node_inputs += 1,
            1 => readers.group_inputs += 1,
            2 => readers.dry += 1,
            _ => readers.runfiles += 1,
        }
        readers.by_place[usize::from(place.min(3))].insert(reader.to_owned());
        readers.contested.insert(reader.to_owned());
    };
    let mut groups_seen = 0usize;
    for node in state.nodes.iter() {
        let reader = owner_of(&node.action.id);
        for dependency in &node.dependencies {
            if dependency
                .action_id
                .starts_with(crate::request::STATUS_ID_PREFIX)
            {
                readers.status.insert(reader.to_owned());
            }
            note(&mut readers, reader, &dependency.action_id, 0);
        }
        for group in &node.groups {
            groups_seen += 1;
            for dependency in &group.dependencies {
                note(&mut readers, reader, &dependency.action_id, 1);
            }
        }
    }
    let produced = |artifact: &Artifact| artifact.producer.as_ref().map(|(id, _)| id.clone());
    for (key, artifacts) in state.dry.iter() {
        let reader = key.action_owner(top);
        for id in artifacts.iter().filter_map(produced) {
            note(&mut readers, &reader, &id, 2);
        }
    }
    for (key, nodes) in &state.runfiles_nodes {
        let reader = key.action_owner(top);
        let artifacts = nodes
            .tree
            .iter()
            .chain(&nodes.manifest)
            .chain([&nodes.repo_mapping, &nodes.executable])
            .chain(
                nodes
                    .entries
                    .iter()
                    .filter_map(|(_, artifact)| artifact.as_ref()),
            );
        for id in artifacts.filter_map(produced) {
            note(&mut readers, &reader, &id, 3);
        }
    }
    readers.losers = readers
        .contested
        .iter()
        .filter(|reader| losers.contains(*reader))
        .cloned()
        .collect();
    readers
        .winners_only
        .retain(|owner| !readers.contested.contains(owner));
    let percent = |count: usize| 100.0 * count as f64 / plan.max(1) as f64;
    let line = |text: std::fmt::Arguments<'_>| crate::snapshot::diag::line_fmt(text);
    line(format_args!(
        "ORDER-AUDIT shared outputs={} actions={} sharing_owners={} contested_paths={} conflicts={}",
        audit.shared,
        audit.winners.len(),
        losers.len(),
        audit.contested.len(),
        audit.conflicts,
    ));
    line(format_args!(
        "ORDER-AUDIT readers plan={plan} contested={} ({:.3}%) of_them_sharing_owners={} third_party={} winners_reading_own={} refs: node_inputs={} group_inputs={} dry={} runfiles={} (groups scanned {groups_seen})",
        readers.contested.len(),
        percent(readers.contested.len()),
        readers.losers.len(),
        readers.contested.len() - readers.losers.len(),
        readers.winners_only.len(),
        readers.node_inputs,
        readers.group_inputs,
        readers.dry,
        readers.runfiles,
    ));
    let only = |place: usize| {
        readers.by_place[place]
            .iter()
            .filter(|reader| {
                (0..4).all(|other| other == place || !readers.by_place[other].contains(*reader))
            })
            .count()
    };
    line(format_args!(
        "ORDER-AUDIT readers_by_place node_inputs={} group_inputs={} dry={} runfiles={} only: node_inputs={} group_inputs={} dry={} runfiles={}",
        readers.by_place[0].len(),
        readers.by_place[1].len(),
        readers.by_place[2].len(),
        readers.by_place[3].len(),
        only(0),
        only(1),
        only(2),
        only(3),
    ));
    let view_reads: u64 = audit.view_artifacts.values().sum();
    let view_contested: u64 = audit
        .view_artifacts
        .iter()
        .filter(|(path, _)| audit.contested.contains(*path))
        .map(|(_, count)| count)
        .sum();
    line(format_args!(
        "ORDER-AUDIT views artifacts={view_reads} paths={} contested={view_contested} (a view keeps their Files only: File has no producer)",
        audit.view_artifacts.len(),
    ));
    let output_reads: u64 = audit.output_files.values().sum();
    let contested_reads: u64 = audit
        .output_files
        .iter()
        .filter(|(path, _)| audit.contested.contains(*path))
        .map(|(_, count)| count)
        .sum();
    line(format_args!(
        "ORDER-AUDIT output_file_resolutions reads={output_reads} paths={} contested_reads={contested_reads}",
        audit.output_files.len(),
    ));
    let (mut late, mut late_deferred, mut late_resolved) = (0usize, 0u64, 0u64);
    for key in &audit.platforms_resolved {
        if let Some((deferred, resolved)) = audit.platform_reads.get(key) {
            late += 1;
            late_deferred += deferred;
            late_resolved += resolved;
        }
    }
    let (all_deferred, all_resolved) = audit
        .platform_reads
        .values()
        .fold((0, 0), |(d, r), (deferred, resolved)| {
            (d + deferred, r + resolved)
        });
    line(format_args!(
        "ORDER-AUDIT platforms reads={} (deferred {all_deferred}, resolved {all_resolved}) resolved_in_phase2={} read_of_those={late} (deferred {late_deferred}, resolved {late_resolved})",
        all_deferred + all_resolved,
        audit.platforms_resolved.len(),
    ));
    line(format_args!(
        "ORDER-AUDIT first_need status={} status_readers={} aspects_planned_in_prepares={} aspect_memo_hits={}",
        audit.status_first_need,
        readers.status.len(),
        audit.aspects_planned,
        audit.aspect_hits,
    ));
    let deltas = audit
        .deltas
        .iter()
        .map(|(reason, count)| format!("{reason}={count}"))
        .collect::<Vec<_>>()
        .join(" ");
    line(format_args!("ORDER-AUDIT deltas {deltas}"));
    // snapshot-unrecorded: a diagnostic switch; it never changes the plan.
    if std::env::var_os("BAZEL_RS_ORDER_AUDIT_LIST").is_some() {
        for (winner, owners) in &audit.winners {
            line(format_args!(
                "ORDER-AUDIT winner {winner} shared_by {}",
                owners
                    .iter()
                    .map(|sharer| sharer.owner(top))
                    .collect::<Vec<_>>()
                    .join(",")
            ));
        }
        for reader in &readers.contested {
            line(format_args!("ORDER-AUDIT reader {reader}"));
        }
    }
}
