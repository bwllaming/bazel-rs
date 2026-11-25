//! The engine side of `aquery` (crate::aquery, docs/AQUERY.md): the
//! configured graph of the universe (crate::engine::cquery's capture) with
//! action planning enabled, and a description of every planned action of
//! its configured targets and aspect applications. Nothing executes: no
//! workspace status, no input digests, no scheduler.
use super::*;
use crate::aquery::{ActionRecord, Aspect, Dump, InternedStrings, Meta};
use crate::aquery_key::{CommandLinePart, KeyKind, KeyPlatform};

/// The owner of each action id prefix, and the planned actions.
#[derive(Default)]
pub(super) struct Capture {
    owners: HashMap<String, Arc<Owner>>,
    nodes: Vec<PlannedAction>,
    top_level: Vec<TopLevel>,
    /// The analyzed test rules (their TestRunner actions, super::aquery_test).
    pub(super) tests: Vec<super::aquery_test::TestCapture>,
    /// The configured graph's nodes of toolchain dependencies keyed by an
    /// execution platform (B13c), by (label, configuration, platform): the
    /// graph itself identifies a node by label and configuration only.
    pub(super) exec_platform_nodes: HashMap<(Label, ConfigId, ExecPlatform), u32>,
}

/// A requested (top-level) configured target as `print_action` reads it
/// (crate::print_action).
#[derive(Clone, Debug)]
pub struct TopLevel {
    /// Label.toString of the target.
    pub label: String,
    /// The configuration checksum.
    pub checksum: String,
    /// The files to build (exec paths).
    pub files_to_build: Vec<String>,
    /// OutputGroupInfo.FILES_TO_COMPILE (`compilation_outputs`), exec paths.
    pub files_to_compile: Vec<String>,
}

/// Label.getDisplayForm(main repository mapping): the text output's
/// `Target:` spelling (LabelPrinter.displayForm).
fn display_form(graph: &cquery::Graph) -> impl Fn(&Label) -> String + Sync + '_ {
    let mut apparent: BTreeMap<&str, &str> = BTreeMap::new();
    for (name, canonical) in &graph.repository_mapping {
        let shorter = apparent
            .get(canonical.as_str())
            .is_none_or(|known| name.len() < known.len());
        if shorter {
            apparent.insert(canonical.as_str(), name.as_str());
        }
    }
    move |label: &Label| -> String {
        if label.repository.is_empty() {
            return format!("//{}:{}", label.package, label.name);
        }
        match apparent.get(label.repository.as_str()) {
            Some(name) if !name.is_empty() => {
                format!("@{name}//{}:{}", label.package, label.name)
            }
            _ => label.to_string(),
        }
    }
}

/// `BAZEL_RS_PARITY_VERIFY=1`: an aquery action without an owner is an
/// internal error (always in debug builds).
fn parity_verify() -> bool {
    // snapshot-unrecorded: a diagnostics check; it changes no result (it fails on a missing owner).
    std::env::var_os("BAZEL_RS_PARITY_VERIFY").is_some_and(|value| value == "1")
}

/// Whether the planned action `id` is the build's own, owned by no target.
/// (The workspace status actions, BazelWorkspaceStatusAction: a stamped
/// action's build-info actions are its owner's.)
fn owned_by_the_build(id: &str) -> bool {
    id.starts_with(crate::request::STATUS_ID_PREFIX)
}

/// One planned action to describe (`Engine::aquery_describe`).
struct RecordJob {
    /// The action's index in the capture's nodes.
    index: usize,
    /// Its owner (the target and aspect path).
    owner: Arc<Owner>,
    graph_node: u32,
    configuration: usize,
}

/// A configured target's (graph node's) planned actions to describe and its
/// TestRunner records (`Engine::aquery_describe_snapshot`).
type TargetJobs<'a> = (u32, (Vec<&'a RecordJob>, Vec<&'a ActionRecord>));

/// The planned actions of an analysis not described yet: a query's are
/// described once its expression selected their targets (only theirs are
/// printed).
struct Pending {
    jobs: Vec<RecordJob>,
    /// The TestRunner actions' records, listed after the planned actions'.
    tests: Vec<(u32, ActionRecord)>,
}

/// `describe` of every job on up to `threads` threads, results in the
/// jobs' order.
fn in_parallel<I: Sync, T: Send>(
    jobs: &[I],
    threads: usize,
    describe: &(dyn Fn(&I) -> T + Sync),
) -> Vec<T> {
    const CHUNK: usize = 256;
    let chunks = jobs.len().div_ceil(CHUNK);
    let threads = threads.min(chunks).min(32);
    if threads <= 1 {
        return jobs.iter().map(describe).collect();
    }
    let results: Vec<std::sync::Mutex<Vec<T>>> = (0..chunks)
        .map(|_| std::sync::Mutex::new(Vec::new()))
        .collect();
    let next = std::sync::atomic::AtomicUsize::new(0);
    let work = || {
        loop {
            let chunk = next.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            if chunk >= chunks {
                break;
            }
            let slice = &jobs[chunk * CHUNK..jobs.len().min((chunk + 1) * CHUNK)];
            let described: Vec<T> = slice.iter().map(describe).collect();
            *results[chunk].lock().unwrap_or_else(|e| e.into_inner()) = described;
        }
    };
    std::thread::scope(|scope| {
        for _ in 1..threads {
            std::thread::Builder::new()
                .name("aquery-records".to_owned())
                .stack_size(16 << 20)
                .spawn_scoped(scope, work)
                .expect("spawn an aquery record thread");
        }
        work();
    });
    results
        .into_iter()
        .flat_map(|chunk| chunk.into_inner().unwrap_or_else(|e| e.into_inner()))
        .collect()
}

pub(super) struct Owner {
    key: ConfiguredTargetKey,
    /// The aspect path (outermost first); empty for a rule's own actions.
    aspects: Vec<Aspect>,
}

/// The analyzed universe: its configured graph and its actions, each with
/// the graph node of its owner.
pub struct Analysis {
    pub graph: cquery::Graph,
    configurations: Vec<crate::aquery::Configuration>,
    actions: Vec<(u32, ActionRecord)>,
    top_level: Vec<TopLevel>,
    /// The planned actions the records list their inputs from
    /// ([`ActionRecord::listed_from`]).
    nodes: Arc<Vec<PlannedAction>>,
    /// An aquery snapshot hit's listings instead (crate::aquery_snapshot).
    stored: Option<Arc<crate::aquery_snapshot::StoredListings>>,
    /// The actions not described yet ([`Engine::aquery_describe`]).
    pending: Option<Pending>,
}

impl Analysis {
    /// The analysis an aquery snapshot hit prints from: the stored graph and
    /// configurations, the selected targets' records and their listings
    /// (crate::aquery_snapshot::Hit::load).
    pub fn from_snapshot(
        graph: cquery::Graph,
        configurations: Vec<crate::aquery::Configuration>,
        actions: Vec<(u32, ActionRecord)>,
        listings: crate::aquery_snapshot::StoredListings,
    ) -> Self {
        Self {
            graph,
            configurations,
            actions,
            top_level: Vec::new(),
            nodes: Arc::default(),
            stored: Some(Arc::new(listings)),
            pending: None,
        }
    }

    /// What the records' inputs are listed from.
    fn listings(&self) -> crate::aquery::Listings {
        match &self.stored {
            Some(stored) => crate::aquery::Listings::Stored(Arc::clone(stored)),
            None => crate::aquery::Listings::Planned(Arc::clone(&self.nodes)),
        }
    }

    /// The dump's configurations.
    pub fn configurations(&self) -> &[crate::aquery::Configuration] {
        &self.configurations
    }

    /// The requested configured targets (crate::print_action).
    pub fn top_level(&self) -> &[TopLevel] {
        &self.top_level
    }

    /// List every record's inputs now (crate::print_action reads them all
    /// at once; the aquery output lists one action's at a time).
    pub fn materialize(&mut self) {
        let listings = self.listings();
        for (_, record) in &mut self.actions {
            if record.listed_from.is_some() {
                *record = crate::aquery::materialized(&listings, record).into_owned();
            }
        }
    }

    /// Every planned action with its owner's configuration checksum.
    pub fn actions_with_checksums(&self) -> impl Iterator<Item = (&ActionRecord, &str)> {
        self.actions.iter().map(|(_, record)| {
            (
                record,
                self.configurations[record.configuration].checksum.as_str(),
            )
        })
    }

    /// The actions of the selected configured targets: per target (by label,
    /// then configuration order), its own actions in registration order, then
    /// its aspects' (with `include_aspects`).
    pub fn dump(&self, selected: &BTreeSet<u32>, include_aspects: bool) -> Dump {
        let mut ids: Vec<u32> = selected.iter().copied().collect();
        ids.sort_by(|a, b| {
            self.graph.nodes[*a as usize]
                .label()
                .cmp(self.graph.nodes[*b as usize].label())
                .then(a.cmp(b))
        });
        let mut by_node: BTreeMap<u32, Vec<&ActionRecord>> = BTreeMap::new();
        for (node, record) in &self.actions {
            by_node.entry(*node).or_default().push(record);
        }
        let mut actions = Vec::new();
        for id in ids {
            let Some(records) = by_node.get(&id) else {
                continue;
            };
            // RunfilesSupport registers its actions after the rule's own.
            for support in [false, true] {
                actions.extend(
                    records
                        .iter()
                        .filter(|record| {
                            record.aspects.is_empty() && record.runfiles_support == support
                        })
                        .map(|record| (*record).clone()),
                );
            }
            if include_aspects {
                actions.extend(
                    records
                        .iter()
                        .filter(|record| !record.aspects.is_empty())
                        .map(|record| (*record).clone()),
                );
            }
        }
        Dump {
            configurations: self.configurations.clone(),
            actions,
            nodes: self.listings(),
        }
    }
}

/// `record` lists its own inputs (set next), not its planned action's
/// ([`ActionRecord::listed_from`]).
fn unlist(record: &mut ActionRecord, meta: &Meta) {
    record.listed_from = None;
    record.input_order = meta.input_order_paths();
}

/// The internal string of internal bytes (crate::bazel_bytes): UTF-8 where
/// valid, a lone byte as its private-use character.
fn internal_string(bytes: &[u8]) -> String {
    let mut out = String::new();
    let mut rest = bytes;
    while !rest.is_empty() {
        match std::str::from_utf8(rest) {
            Ok(text) => {
                out.push_str(text);
                break;
            }
            Err(error) => {
                let valid = error.valid_up_to();
                out.push_str(std::str::from_utf8(&rest[..valid]).unwrap_or_default());
                let byte = rest[valid];
                out.push(char::from_u32(0x10_ff00 + u32::from(byte)).unwrap_or('\u{fffd}'));
                rest = &rest[valid + 1..];
            }
        }
    }
    out
}

/// The AspectDescriptor of an aspect definition: its class name and the
/// parameters its description shows.
pub(super) fn aspect_descriptor(aspect: &crate::aspects::AspectDefinition) -> Aspect {
    let mut parameters = Vec::new();
    for (name, spec) in &aspect.declaration.attrs {
        if !crate::aspects::is_parameter_attribute(name, spec) {
            continue;
        }
        let value = if aspect.top_level {
            aspect.parameters.get(name).unwrap_or(&spec.default)
        } else {
            match aspect.parameters.get(name) {
                Some(value) => value,
                None => continue,
            }
        };
        let value = match value {
            crate::custom_rule::AttrValue::String(text) => text.clone(),
            crate::custom_rule::AttrValue::Bool(value) => value.to_string(),
            crate::custom_rule::AttrValue::Int(value) => value.to_string(),
            crate::custom_rule::AttrValue::Strings(items) => items.join(","),
            _ => continue,
        };
        parameters.push((name.clone(), value));
    }
    Aspect {
        name: aspect.identity.clone(),
        parameters,
    }
}

impl Engine {
    /// Record the owner of the actions `action_owner#…` (aquery only).
    pub(super) fn aquery_owner(
        &mut self,
        action_owner: &str,
        key: &ConfiguredTargetKey,
        aspects: impl FnOnce() -> Vec<Aspect>,
    ) {
        if let Some(capture) = &mut self.aquery_capture {
            capture.owners.insert(
                action_owner.to_owned(),
                Arc::new(Owner {
                    key: key.clone(),
                    aspects: aspects(),
                }),
            );
        }
    }

    /// Keep the requested targets' files to build and files to compile
    /// (aquery only; crate::print_action).
    pub(super) fn aquery_top_level(
        &mut self,
        labels: &[Label],
        top: ConfigId,
        state: &ActionPlanning,
    ) {
        if self.aquery_capture.is_none() {
            return;
        }
        let mut top_level = Vec::new();
        for label in labels {
            let Ok(key) = self.output_group_key(label, top) else {
                continue;
            };
            let requested = ConfiguredTargetKey::new(label.clone(), top);
            let files_to_build = state
                .dry
                .get(&requested)
                .or_else(|| state.dry.get(&key))
                .map(|artifacts| {
                    artifacts
                        .iter()
                        .map(|artifact| artifact.exec.to_string_lossy().into_owned())
                        .collect()
                })
                .unwrap_or_default();
            let files_to_compile = self
                .planned_output_group(
                    &state.custom_providers,
                    &state.validation,
                    &key,
                    "compilation_outputs",
                )
                .into_iter()
                .map(|file| file.path.clone())
                .collect();
            top_level.push(TopLevel {
                label: label.to_string(),
                checksum: top.to_string(),
                files_to_build,
                files_to_compile,
            });
        }
        if let Some(capture) = &mut self.aquery_capture {
            capture.top_level = top_level;
        }
    }

    /// Keep the planned actions of the analysis (aquery only).
    pub(super) fn aquery_nodes(&mut self, nodes: &mut Vec<PlannedAction>) {
        if let Some(capture) = &mut self.aquery_capture {
            capture.nodes = std::mem::take(nodes);
        }
    }

    /// Whether provider analysis is all a configured query needs (cquery):
    /// aquery's capture also plans every action.
    pub(super) fn semantic_only(&self) -> bool {
        self.cquery_capture.is_some() && self.aquery_capture.is_none()
    }

    /// Analyze `patterns` (the universe) with action planning and describe
    /// every action of its configured targets.
    pub fn aquery_analysis(
        &mut self,
        patterns: &[String],
        relative_directory: &str,
        test_options: &crate::test_command::TestOptions,
    ) -> Result<Analysis, String> {
        crate::aquery::set_capturing(true);
        self.aquery_capture = Some(Capture::default());
        let graph = self.cquery_graph(patterns, relative_directory);
        crate::aquery::set_capturing(false);
        let capture = self.aquery_capture.take().unwrap_or_default();
        let graph = graph?;
        crate::perf_mark("aquery.graph");
        let exec_platform_nodes = capture.exec_platform_nodes.clone();
        let exec_platform_ids: HashSet<u32> = exec_platform_nodes.values().copied().collect();
        let mut nodes_by_key: HashMap<(String, String), u32> = HashMap::new();
        for (id, node) in graph.nodes.iter().enumerate() {
            if exec_platform_ids.contains(&(id as u32)) {
                continue;
            }
            if let Some(configuration) = node.configuration {
                nodes_by_key.insert(
                    (
                        node.label().to_owned(),
                        graph.configurations[configuration as usize]
                            .checksum
                            .clone(),
                    ),
                    id as u32,
                );
            }
        }
        let display = display_form(&graph);
        let mut configurations = Vec::new();
        let mut configuration_index: BTreeMap<ConfigId, usize> = BTreeMap::new();
        // The owner, graph node and configuration of each planned action, in
        // plan order (configurations are numbered in first-use order).
        let mut jobs: Vec<RecordJob> = Vec::new();
        let mut unowned: Vec<&str> = Vec::new();
        let mut ungraphed = 0usize;
        for (index, node) in capture.nodes.iter().enumerate() {
            let id = node.action.id.as_str();
            let owner = capture.owners.get(id).or_else(|| {
                id.rsplit_once('#')
                    .and_then(|(prefix, _)| capture.owners.get(prefix))
            });
            let Some(owner) = owner else {
                unowned.push(id);
                continue;
            };
            let config = owner.key.config;
            let graph_node = match owner.key.exec_platform {
                Some(platform) => {
                    exec_platform_nodes.get(&(owner.key.label.clone(), config, platform))
                }
                None => nodes_by_key.get(&(owner.key.label.to_string(), config.to_string())),
            };
            let Some(&graph_node) = graph_node else {
                ungraphed += 1;
                continue;
            };
            let configuration = *configuration_index.entry(config).or_insert_with(|| {
                let mnemonic = self.configurations.mnemonic(config).to_owned();
                configurations.push(crate::aquery::Configuration {
                    platform_name: mnemonic.split('-').next().unwrap_or_default().to_owned(),
                    mnemonic,
                    checksum: config.to_string(),
                    is_tool: self.configurations.get(config).native.is_exec,
                });
                configurations.len() - 1
            });
            jobs.push(RecordJob {
                index,
                owner: Arc::clone(owner),
                graph_node,
                configuration,
            });
        }
        // Every planned action is listed under its owner but the build's
        // own (the workspace status actions, owned by no target): an action
        // without an owner would be missing from the output silently.
        let unexpected: Vec<&str> = unowned
            .iter()
            .copied()
            .filter(|id| !owned_by_the_build(id))
            .collect();
        if crate::plan_timings::enabled() {
            crate::snapshot::diag::line_fmt(format_args!(
                "PERF-AQUERY nodes={} owners={} described={} unowned={} unexpected={} ungraphed={ungraphed}",
                capture.nodes.len(),
                capture.owners.len(),
                jobs.len(),
                unowned.len(),
                unexpected.len(),
            ));
        }
        // (And an owner outside the configured graph, which holds every
        // analyzed target.)
        if cfg!(debug_assertions) || parity_verify() {
            if let Some(id) = unexpected.first() {
                return Err(format!(
                    "internal error: aquery: the planned action {id} has no owner ({} such actions)",
                    unexpected.len()
                ));
            }
            if ungraphed > 0 {
                return Err(format!(
                    "internal error: aquery: {ungraphed} planned actions' owners are not in the configured graph"
                ));
            }
            if jobs.len() + unowned.len() != capture.nodes.len() {
                return Err(
                    "internal error: aquery: a planned action neither described nor skipped"
                        .to_owned(),
                );
            }
        }
        // TestRunner actions: registered with the rule's (after its
        // RunfilesSupport actions, TestActionBuilder via
        // RuleConfiguredTargetBuilder.build).
        let mut tests = Vec::new();
        for test in &capture.tests {
            let config = test.key.config;
            let graph_node = match test.key.exec_platform {
                Some(platform) => {
                    exec_platform_nodes.get(&(test.key.label.clone(), config, platform))
                }
                None => nodes_by_key.get(&(test.key.label.to_string(), config.to_string())),
            };
            let Some(&graph_node) = graph_node else {
                continue;
            };
            let configuration = *configuration_index.entry(config).or_insert_with(|| {
                let mnemonic = self.configurations.mnemonic(config).to_owned();
                configurations.push(crate::aquery::Configuration {
                    platform_name: mnemonic.split('-').next().unwrap_or_default().to_owned(),
                    mnemonic,
                    checksum: config.to_string(),
                    is_tool: self.configurations.get(config).native.is_exec,
                });
                configurations.len() - 1
            });
            let rule_class = match &graph.nodes[graph_node as usize].target {
                crate::cquery_proto::Target::Rule(rule) => Some(rule.rule_class.clone()),
                _ => None,
            };
            let display_target = display(&test.key.label);
            for mut record in
                self.aquery_test_records(test, test_options, configuration, &configurations)
            {
                record.rule_class = rule_class.clone();
                record.display_target = display_target.clone();
                record.display_execution_platform = record
                    .execution_platform
                    .as_deref()
                    .and_then(|platform| Label::parse_canonical(platform).ok())
                    .map(|platform| display(&platform));
                tests.push((graph_node, record));
            }
        }
        drop(display);
        Ok(Analysis {
            graph,
            configurations,
            actions: Vec::new(),
            top_level: capture.top_level.clone(),
            nodes: Arc::new(capture.nodes),
            stored: None,
            pending: Some(Pending { jobs, tests }),
        })
    }

    /// Describe the planned actions of `analysis` whose owners are in
    /// `selected` (graph nodes; all of them without), in plan order, then
    /// the TestRunner actions'. The records of each action are independent
    /// of the others' (a function of the planned action, its owner and the
    /// engine's finished analysis): described on several threads. Only the
    /// first call describes.
    pub fn aquery_describe(&self, analysis: &mut Analysis, selected: Option<&BTreeSet<u32>>) {
        let Some(Pending { jobs, tests }) = analysis.pending.take() else {
            return;
        };
        let wanted =
            |graph_node: u32| selected.is_none_or(|selected| selected.contains(&graph_node));
        let jobs: Vec<RecordJob> = jobs
            .into_iter()
            .filter(|job| wanted(job.graph_node))
            .collect();
        let graph = &analysis.graph;
        let nodes = &analysis.nodes;
        let configurations = &analysis.configurations;
        let display = display_form(graph);
        let platform_infos = self.aquery_platform_infos();
        let describe = |job: &RecordJob| -> Vec<(u32, ActionRecord)> {
            self.aquery_job_records(job, graph, nodes, configurations, &display, &platform_infos)
                .into_iter()
                .map(|record| (job.graph_node, record))
                .collect()
        };
        let mut actions: Vec<(u32, ActionRecord)> =
            in_parallel(&jobs, self.loading_phase_threads, &describe)
                .into_iter()
                .flatten()
                .collect();
        drop(display);
        actions.extend(tests.into_iter().filter(|(node, _)| wanted(*node)));
        if crate::plan_timings::enabled() {
            crate::snapshot::diag::line_fmt(format_args!(
                "PERF-AQUERY described={} records={}",
                jobs.len(),
                actions.len()
            ));
        }
        analysis.actions = actions;
        crate::perf_mark("aquery.records");
    }

    /// The records of one planned action (`job`), with its owner's rule
    /// class and the display forms of its target and execution platform.
    fn aquery_job_records(
        &self,
        job: &RecordJob,
        graph: &cquery::Graph,
        nodes: &[PlannedAction],
        configurations: &[crate::aquery::Configuration],
        display: &(dyn Fn(&Label) -> String + Sync),
        platform_infos: &HashMap<Label, Arc<crate::platforms::PlatformInfo>>,
    ) -> Vec<ActionRecord> {
        let rule_class = match &graph.nodes[job.graph_node as usize].target {
            crate::cquery_proto::Target::Rule(rule) => Some(rule.rule_class.clone()),
            _ => None,
        };
        let display_target = display(&job.owner.key.label);
        let mut records = self.aquery_records(
            &nodes[job.index],
            job.index,
            &job.owner,
            rule_class,
            job.configuration,
            configurations,
            platform_infos,
        );
        for record in &mut records {
            record.display_target = display_target.clone();
            record.display_execution_platform = record
                .execution_platform
                .as_deref()
                .and_then(|platform| Label::parse_canonical(platform).ok())
                .map(|platform| display(&platform));
        }
        records
    }

    /// [`Self::aquery_describe`] of every planned action of `analysis`, for
    /// its aquery snapshot (crate::aquery_snapshot): the records of all of
    /// them, encoded by configured target in blocks, while the records of
    /// the `selected` targets' (all without) stay in the analysis for this
    /// command's output, exactly as `aquery_describe` leaves them (each
    /// target's records in the same order; the dump lists by target).
    /// Blocks describe and encode on several threads, so the described
    /// records of the whole universe are never held at once.
    pub fn aquery_describe_snapshot(
        &self,
        analysis: &mut Analysis,
        selected: Option<&BTreeSet<u32>>,
    ) -> Result<crate::aquery_snapshot::Records, String> {
        let (records, actions) = self.aquery_snapshot_records(analysis, selected)?;
        // Described: the pending actions are the records now.
        analysis.pending = None;
        analysis.actions = actions;
        crate::perf_mark("aquery.records");
        Ok(records)
    }

    /// [`Self::aquery_describe_snapshot`]'s records and the selected
    /// targets' ones, `analysis` untouched (an error leaves it to
    /// [`Self::aquery_describe`]).
    fn aquery_snapshot_records(
        &self,
        analysis: &Analysis,
        selected: Option<&BTreeSet<u32>>,
    ) -> Result<(crate::aquery_snapshot::Records, Vec<(u32, ActionRecord)>), String> {
        use crate::aquery_snapshot::{GroupTable, Records};
        let Some(Pending { jobs, tests }) = analysis.pending.as_ref() else {
            return Err("aquery snapshot: the analysis was described already".into());
        };
        let wanted =
            |graph_node: u32| selected.is_none_or(|selected| selected.contains(&graph_node));
        // Per configured target: its planned actions in plan order, then its
        // TestRunner records (the order `actions` gives each target).
        let mut by_node: BTreeMap<u32, (Vec<&RecordJob>, Vec<&ActionRecord>)> = BTreeMap::new();
        for job in jobs {
            by_node.entry(job.graph_node).or_default().0.push(job);
        }
        for (node, record) in tests {
            by_node.entry(*node).or_default().1.push(record);
        }
        let units: Vec<TargetJobs> = by_node.into_iter().collect();
        let nodes = &analysis.nodes;
        let mut table = GroupTable::default();
        for (_, (jobs, _)) in &units {
            for job in jobs {
                table.note(&nodes[job.index]);
            }
        }
        crate::perf_mark("aquery.snapshot_groups");
        let mut blocks: Vec<&[TargetJobs]> = Vec::new();
        let mut start = 0;
        let mut size = 0;
        for (index, (_, (jobs, tests))) in units.iter().enumerate() {
            size += jobs.len() + tests.len();
            if size >= crate::aquery_snapshot::BLOCK || index + 1 == units.len() {
                blocks.push(&units[start..=index]);
                start = index + 1;
                size = 0;
            }
        }
        let graph = &analysis.graph;
        let configurations = &analysis.configurations;
        let display = display_form(graph);
        let platform_infos = self.aquery_platform_infos();
        let parity = crate::aquery::parity_checked();
        type Described = Result<(Vec<u8>, Vec<(u32, ActionRecord)>), String>;
        let describe = |block: &&[TargetJobs]| -> Described {
            let mut kept = Vec::new();
            let mut encoder = crate::aquery_snapshot::BlockEncoder::default();
            for (node, (jobs, tests)) in block.iter() {
                encoder.start(*node);
                let keep = wanted(*node);
                for job in jobs {
                    for record in self.aquery_job_records(
                        job,
                        graph,
                        nodes,
                        configurations,
                        &display,
                        &platform_infos,
                    ) {
                        let listing = match record.listed_from {
                            Some(index) => {
                                let listing = table.listing(&nodes[index]).ok_or_else(|| {
                                    "aquery snapshot: a shared input group was not noted".to_owned()
                                })?;
                                if parity
                                    && table.expand(&listing).as_deref()
                                        != Some(&crate::aquery::listed_inputs(&nodes[index])[..])
                                {
                                    return Err(format!(
                                        "internal error: aquery snapshot: the stored listing of {} differs from its planned action's",
                                        nodes[index].action.id
                                    ));
                                }
                                Some(listing)
                            }
                            None => None,
                        };
                        encoder.push(&record, listing.as_ref());
                        if keep {
                            kept.push((*node, record));
                        }
                    }
                }
                for record in tests {
                    encoder.push(record, None);
                    if keep {
                        kept.push((*node, (*record).clone()));
                    }
                }
            }
            Ok((encoder.finish()?, kept))
        };
        // One block per task (each a few hundred actions), on at most 16
        // threads (each holds its block's records and encoding).
        let described = crate::aquery_snapshot::on_threads(
            &blocks,
            self.loading_phase_threads.min(16),
            &describe,
        )?;
        drop(display);
        let mut records = Records::default();
        records.push_groups(&table)?;
        let mut actions = Vec::new();
        for (block, (encoded, kept)) in blocks.iter().zip(described) {
            let (Some((first, _)), Some((last, _))) = (block.first(), block.last()) else {
                continue;
            };
            records.push_block(*first, *last, &encoded);
            actions.extend(kept);
        }
        if crate::plan_timings::enabled() {
            crate::snapshot::diag::line_fmt(format_args!(
                "PERF-AQUERY snapshot units={} blocks={} groups={} bytes={} records={}",
                units.len(),
                records.blocks.len(),
                records.group_count,
                records.bytes.len(),
                actions.len()
            ));
        }
        Ok((records, actions))
    }

    /// The execution platform of an action: its exec group's, a native
    /// spawn's resolved remote platform, else the host platform.
    fn aquery_platform(
        &self,
        node: &PlannedAction,
        config: ConfigId,
    ) -> (Label, Vec<(String, String)>) {
        if let Some(execution) = self.action_executions.get(&node.action.id) {
            return (execution.platform.clone(), execution.properties.clone());
        }
        if let Ok(label) = Label::parse_canonical(&node.action.remote_platform.label) {
            return (label, Vec::new());
        }
        (
            self.configurations.get(config).native.host_platform.clone(),
            Vec::new(),
        )
    }

    /// [`Self::aquery_platform_info`] of every analyzed platform label.
    fn aquery_platform_infos(&self) -> HashMap<Label, Arc<crate::platforms::PlatformInfo>> {
        let mut infos = HashMap::new();
        for (key, info) in self.platforms.iter() {
            if let Ok(info) = info {
                infos
                    .entry(key.label.clone())
                    .or_insert_with(|| Arc::clone(info));
            }
        }
        infos
    }

    pub(super) fn aquery_platform_info(
        &self,
        label: &Label,
    ) -> Option<Arc<crate::platforms::PlatformInfo>> {
        self.platforms
            .iter()
            .filter(|(key, _)| key.label == *label)
            .find_map(|(_, info)| info.as_ref().ok().cloned())
    }

    /// The records of the planned action `node` (`index` in the capture's
    /// nodes; a record listing exactly its inputs lists them from there
    /// when written, ActionRecord::listed_from).
    #[allow(clippy::too_many_arguments)]
    fn aquery_records(
        &self,
        node: &PlannedAction,
        index: usize,
        owner: &Owner,
        rule_class: Option<String>,
        configuration: usize,
        configurations: &[crate::aquery::Configuration],
        platform_infos: &HashMap<Label, Arc<crate::platforms::PlatformInfo>>,
    ) -> Vec<ActionRecord> {
        // The inputs are the node's listing (crate::aquery::listed_inputs: a
        // shared run's groups, not runfiles trees, listed one by one, as the
        // node lists them without sharing; a runfiles tree one input),
        // listed only when a record needs them other than through
        // `listed_from` (the records of the whole universe are described for
        // an aquery snapshot: 70k PyCompile actions list the 1,655-file
        // Python runtime).
        let listing: std::cell::OnceCell<Vec<String>> = std::cell::OnceCell::new();
        let listed = || -> &[String] { listing.get_or_init(|| crate::aquery::listed_inputs(node)) };
        let action = &node.action;
        let no_meta = Meta::default();
        let meta: &Meta = action.extra.aquery.as_deref().unwrap_or(&no_meta);
        let tool = configurations[configuration].is_tool;
        let label = owner.key.label.to_string();
        let (platform_label, properties) = self.aquery_platform(node, owner.key.config);
        // A no-config target (NoConfigTransition: no PlatformConfiguration)
        // resolves no toolchain context, so its actions have no execution
        // platform (ToolchainContextUtil.java:79-82).
        let no_config = self.configurations.get(owner.key.config).native.no_config;
        let platform = if no_config {
            None
        } else {
            platform_infos.get(&platform_label).cloned()
        };
        // An alias (`@bazel_tools//tools:host_platform`) prints as the
        // platform it resolves to (PlatformInfo.label).
        let platform_label = platform
            .as_ref()
            .map_or(platform_label, |info| info.label.clone());
        let outputs: Vec<(String, bool)> = action
            .outputs
            .iter()
            .map(|output| {
                (
                    output.to_string_lossy().into_owned(),
                    action.extra.tree_outputs.contains(output),
                )
            })
            .collect();
        let primary_output = outputs.first().map(|(path, _)| path.as_str());
        // Action.getPrimaryInput: the first of the inputs NestedSet (a
        // Starlark spawn's declared inputs precede its tools).
        let first_input = match meta.input_order.as_ref().and_then(|order| order.first()) {
            Some(file) => Some(file.path.clone()),
            None => crate::aquery::first_listed_input(node),
        };
        let primary_input = first_input.as_deref();
        let suffix = action.id.rsplit_once('#').map(|(_, suffix)| suffix);
        let mut extra_records = Vec::new();
        let mut record = ActionRecord {
            runfiles_support: matches!(
                suffix,
                Some("repo_mapping" | "runfiles_manifest" | "runfiles_tree")
            ),
            target: label.clone(),
            rule_class,
            configuration,
            aspects: owner.aspects.clone(),
            execution_platform: (!no_config)
                .then(|| crate::rule_declarations::canonical_form(&platform_label)),
            // Listed when written (ActionRecord::listed_from): not held as
            // strings for every action of the universe at once.
            listed_from: Some(index),
            outputs: outputs.clone(),
            expansion_error: meta.expansion_error.clone(),
            ..Default::default()
        };
        let key_platform = match &platform {
            Some(info) => KeyPlatform::Resolved(info),
            None => KeyPlatform::None,
        };
        let progress = |raw: &str| {
            crate::aquery::progress_message(raw, &label, primary_output, primary_input, tool)
        };
        let template_input = |template: &std::path::Path| -> (String, String) {
            let exec = template.to_string_lossy().into_owned();
            let content = crate::aquery::listed_sources(node)
                .find(|(_, destination)| destination.as_os_str() == template.as_os_str())
                // Recorded: an aquery snapshot prints the text it read.
                .and_then(|(source, _)| crate::observed_fs::read(&**source).ok())
                .map(|bytes| internal_string(&bytes))
                .unwrap_or_else(|| format!("ARTIFACT: {exec}"));
            (exec, content)
        };
        match (&action.write, &action.extra.transform) {
            (Some(write), _) => {
                if let Some(param) = &meta.param_write {
                    record.mnemonic = if action.extra.mnemonic.is_empty() {
                        "FileWrite".into()
                    } else {
                        action.extra.mnemonic.clone()
                    };
                    record.param_file = Some(param.arguments.to_vec());
                    record.action_key = crate::aquery_key::action_key(
                        &KeyKind::ParamFileWrite {
                            file_type: param.file_type,
                            args: &param.recipe,
                        },
                        key_platform,
                        &properties,
                    );
                } else {
                    let contents = internal_string(&write.content);
                    record.mnemonic = if action.extra.mnemonic.is_empty() {
                        "FileWrite".into()
                    } else {
                        action.extra.mnemonic.clone()
                    };
                    record.action_key = crate::aquery_key::action_key(
                        &KeyKind::FileWrite {
                            executable: write.executable,
                            contents: &contents,
                        },
                        key_platform,
                        &properties,
                    );
                    record.file_write = Some((write.executable, contents));
                }
                if suffix == Some("repo_mapping") || meta.repo_mapping {
                    record.mnemonic = "RepoMappingManifest".into();
                    record.description = progress("Writing repo mapping manifest for %{label}");
                } else {
                    record.description = progress(&format!(
                        "Writing {} %{{output}}",
                        if write.executable { "script" } else { "file" }
                    ));
                }
            }
            (
                None,
                Some(crate::action::Transform::Symlink {
                    require_executable,
                    progress: message,
                    ..
                }),
            ) => {
                record.mnemonic = if !action.extra.mnemonic.is_empty() {
                    action.extra.mnemonic.clone()
                } else if message.starts_with("SolibSymlink ") {
                    // cc_common's solib symlinks (SolibSymlinkAction).
                    "SolibSymlink".into()
                } else if *require_executable {
                    "ExecutableSymlink".into()
                } else {
                    "Symlink".into()
                };
                // SolibSymlinkAction has no raw progress message: describe()'s
                // default, without the tool configuration's ` [for tool]`.
                record.description = if record.mnemonic == "SolibSymlink" {
                    message.clone()
                } else {
                    progress(message)
                };
                let kind = match (record.mnemonic.as_str(), primary_output, primary_input) {
                    ("SolibSymlink", Some(symlink), Some(library)) => {
                        KeyKind::SolibSymlink { symlink, library }
                    }
                    _ => KeyKind::Symlink,
                };
                record.action_key = crate::aquery_key::action_key(&kind, key_platform, &properties);
            }
            (
                None,
                Some(crate::action::Transform::UnresolvedSymlink {
                    target,
                    target_type,
                    progress: message,
                }),
            ) => {
                record.mnemonic = "UnresolvedSymlink".into();
                record.description = progress(message);
                record.unresolved_symlink = Some(target.clone());
                record.action_key = crate::aquery_key::action_key(
                    &KeyKind::UnresolvedSymlink {
                        target,
                        target_type: target_type.name(),
                    },
                    key_platform,
                    &properties,
                );
            }
            (None, Some(crate::action::Transform::TemplateKeyed { volatile, .. })) => {
                // BuildInfoFileWriteAction (TranslateBuildInfo): its key is
                // over the translation callback (not reproduced).
                record.mnemonic = "TranslateBuildInfo".into();
                record.description = progress(&format!(
                    "Translating {} BuildInfo file",
                    if *volatile { "volatile" } else { "stable" }
                ));
                record.action_key =
                    self.aquery_native_key(&record, listed(), key_platform, &properties);
            }
            (
                None,
                Some(crate::action::Transform::Template {
                    template,
                    substitutions,
                    executable,
                }),
            ) => {
                record.mnemonic = if action.extra.mnemonic.is_empty() {
                    "TemplateExpand".into()
                } else {
                    action.extra.mnemonic.clone()
                };
                record.description = progress("Expanding template %{output}");
                let (exec, content) = template_input(template);
                let substitutions: Vec<(String, String)> = substitutions
                    .iter()
                    .map(|(key, value)| (internal_string(key), internal_string(value)))
                    .collect();
                record.action_key = crate::aquery_key::action_key(
                    &KeyKind::Template {
                        executable: *executable,
                        template_exec_path: &exec,
                        substitutions: &substitutions,
                    },
                    key_platform,
                    &properties,
                );
                record.template = Some((content, substitutions));
            }
            (None, Some(crate::action::Transform::RunfilesManifest { .. })) => {
                // SourceManifestAction: no inputs, a FileContentsProvider
                // (its contents name absolute paths; not reproduced).
                record.mnemonic = "SourceSymlinkManifest".into();
                record.description = progress("Creating source manifest for %{label}");
                unlist(&mut record, meta);
                record.inputs.clear();
                record.file_write = Some((false, String::new()));
                record.action_key = self.aquery_native_key(&record, &[], key_platform, &properties);
            }
            (
                None,
                Some(crate::action::Transform::RunfilesTree {
                    entries, manifest, ..
                }),
            ) => {
                // SymlinkTreeAction over the input manifest, then Bazel 9's
                // RunfilesTreeAction over the runfiles and the MANIFEST.
                let manifest_input = manifest.to_string_lossy().into_owned();
                let tree_manifest = primary_output.unwrap_or_default().to_owned();
                let directory = tree_manifest
                    .strip_suffix("/MANIFEST")
                    .unwrap_or(&tree_manifest)
                    .to_owned();
                record.mnemonic = "SymlinkTree".into();
                record.description = progress(&format!("Creating runfiles tree {directory}"));
                unlist(&mut record, meta);
                record.inputs = vec![manifest_input];
                record.environment = self
                    .configurations
                    .get(owner.key.config)
                    .native
                    .action_environment()
                    .fixed
                    .into_iter()
                    .collect();
                record.action_key =
                    self.aquery_native_key(&record, &record.inputs, key_platform, &properties);
                let mut tree = ActionRecord {
                    mnemonic: "RunfilesTree".into(),
                    pretty: Some(format!("runfiles for {label}")),
                    inputs: crate::aquery::dedup(
                        entries
                            .iter()
                            .filter_map(|(_, path)| {
                                path.as_ref()
                                    .map(|path| path.to_string_lossy().into_owned())
                            })
                            .chain(std::iter::once(tree_manifest.clone())),
                    ),
                    outputs: vec![(directory.clone(), false)],
                    environment: Vec::new(),
                    ..record.clone()
                };
                tree.description = format!("runfiles for {label}");
                tree.action_key = crate::aquery_key::action_key(
                    &KeyKind::RunfilesTree,
                    key_platform,
                    &properties,
                );
                extra_records.push(tree);
            }
            (None, Some(transform)) => {
                let mnemonic = match transform {
                    crate::action::Transform::BuildInfo { .. } => "TranslateBuildInfo",
                    _ => "Action",
                };
                record.mnemonic = if action.extra.mnemonic.is_empty() {
                    mnemonic.to_owned()
                } else {
                    action.extra.mnemonic.clone()
                };
                record.description = progress(&format!("{} %{{output}}", record.mnemonic));
                record.action_key =
                    self.aquery_native_key(&record, listed(), key_platform, &properties);
                if let Some(target) = &meta.absolute_symlink {
                    // SymlinkAction.toAbsolutePath: its raw progress message
                    // (no input to expand `%{input}`) and key.
                    if let Some(raw) = &meta.progress {
                        record.description = progress(raw);
                    }
                    record.action_key = crate::aquery_key::action_key(
                        &KeyKind::AbsoluteSymlink { target },
                        key_platform,
                        &properties,
                    );
                }
            }
            (None, None) => {
                record.mnemonic = action.extra.mnemonic.clone();
                let arguments = meta
                    .arguments
                    .as_ref()
                    .map_or_else(|| crate::action::arguments(action), InternedStrings::to_vec);
                let raw = match (&meta.progress, record.mnemonic.as_str()) {
                    (Some(raw), _) => raw.clone(),
                    (None, "Genrule") => "Executing genrule %{label}".to_owned(),
                    // CppCompileAction: "Compiling " + the source file.
                    (None, "CppCompile" | "ObjcCompile") => "Compiling %{input}".to_owned(),
                    (None, mnemonic) => format!("{mnemonic} %{{output}}"),
                };
                let source = arguments
                    .iter()
                    .position(|argument| argument == "-c")
                    .and_then(|index| arguments.get(index + 1));
                record.description = match (record.mnemonic.as_str(), source) {
                    // CppCompileAction.getRawProgressMessage over its source
                    // file, recorded at registration (a header-parsing
                    // action's header; crate::cc_actions).
                    (_, _)
                        if action.extra.mnemonic.starts_with("Cpp")
                            && let Some(recorded) = &action.extra.progress =>
                    {
                        progress(&recorded.as_path().to_string_lossy())
                    }
                    ("CppCompile" | "ObjcCompile", Some(source)) => {
                        crate::aquery::progress_message(
                            &raw,
                            &label,
                            primary_output,
                            Some(source),
                            tool,
                        )
                    }
                    _ => progress(&raw),
                };
                record.environment = meta
                    .environment
                    .as_deref()
                    .map(<[_]>::to_vec)
                    .unwrap_or_else(|| {
                        action
                            .environment
                            .iter()
                            .map(|(key, value)| (key.clone(), value.clone()))
                            .collect()
                    });
                // TargetUtils.getFilteredExecutionInfo (packages/TargetUtils.java
                // :285-306): the legal requirements, then each propagated tag
                // as `tag: ""`, sorted (crate::shared_actions keys the same map).
                let mut merged: BTreeMap<String, String> = action
                    .extra
                    .execution_info
                    .iter()
                    .filter(|(key, _)| legal_execution_info_key(key))
                    .map(|(key, value)| (key.clone(), value.clone()))
                    .collect();
                for tag in &action.execution_info {
                    merged.entry(tag.clone()).or_default();
                }
                let info: Vec<(String, String)> = match &meta.execution_info {
                    Some(info) => {
                        let mut info = info.clone();
                        info.sort();
                        info
                    }
                    None => merged.into_iter().collect(),
                };
                record.execution_info = info;
                let flat;
                let lines: &[CommandLinePart] = match &meta.command_lines {
                    Some(lines) => lines,
                    None => {
                        flat = [CommandLinePart::Flat(arguments.clone())];
                        &flat
                    }
                };
                let inherited: Vec<String> =
                    action.extra.inherited_environment.iter().cloned().collect();
                let spawn = KeyKind::Spawn {
                    command_lines: lines,
                    mnemonic: &record.mnemonic,
                    environment: &record.environment,
                    inherited: &inherited,
                    execution_info: &record.execution_info,
                };
                let kind = match &meta.compute_key {
                    Some(bytes) => KeyKind::Encoded(bytes),
                    None => spawn,
                };
                record.action_key = crate::aquery_key::action_key(&kind, key_platform, &properties);
                record.discovers_inputs =
                    matches!(record.mnemonic.as_str(), "CppCompile" | "ObjcCompile");
                // Every CppCompileAction (crate::cc_actions compile_mnemonic),
                // the linkstamp compile included.
                if matches!(
                    record.mnemonic.as_str(),
                    "CppCompile"
                        | "ObjcCompile"
                        | "CppLinkstampCompile"
                        | "CppHeaderAnalysis"
                        | "CppDepsScanning"
                ) {
                    // CppCompileAction.getEnvironment is the configuration's;
                    // the toolchain's variables are only in its effective
                    // environment (the proto output's).
                    record.text_environment = Some(
                        self.configurations
                            .get(owner.key.config)
                            .native
                            .action_environment()
                            .fixed
                            .into_iter()
                            .collect(),
                    );
                }
                record.arguments = Some(arguments);
            }
        }
        if crate::aquery::parity_checked() {
            assert_eq!(
                first_input.as_deref(),
                meta.input_order
                    .as_ref()
                    .and_then(|order| order.first())
                    .map(|file| file.path.as_str())
                    .or(listed().first().map(String::as_str)),
                "internal error: aquery: the primary input of {}",
                action.id
            );
        }
        let mut records = vec![record];
        records.extend(extra_records);
        records
    }

    /// A deterministic key for an action class whose Bazel key the native
    /// profile does not reproduce (docs/AQUERY.md): a spawn-shaped
    /// fingerprint of its description.
    fn aquery_native_key(
        &self,
        record: &ActionRecord,
        inputs: &[String],
        platform: KeyPlatform<'_>,
        properties: &[(String, String)],
    ) -> String {
        let mut description = vec![record.mnemonic.clone()];
        description.extend(inputs.iter().cloned());
        description.extend(record.outputs.iter().map(|(path, _)| path.clone()));
        let lines = [CommandLinePart::Flat(description)];
        crate::aquery_key::action_key(
            &KeyKind::Spawn {
                command_lines: &lines,
                mnemonic: &record.mnemonic,
                environment: &record.environment,
                inherited: &[],
                execution_info: &[],
            },
            platform,
            properties,
        )
    }
}
