//! Configured analysis without execution-plan lowering. Graph capture is opt-in;
//! retained query nodes contain no providers, runfiles, actions or Starlark heaps.
use super::*;
use crate::cquery_proto as wire;
use crate::query_language::{Argument, Expr, Function, Operator};
use serde::{Deserialize, Serialize};
use std::io::Write;

#[derive(Default)]
pub(super) struct Capture {
    pub rules: BTreeMap<ConfiguredTargetKey, Arc<Rule>>,
    pub incompatible: BTreeMap<ConfiguredTargetKey, Vec<(String, ConfigMatching)>>,
    pub direct_incompatible: BTreeSet<ConfiguredTargetKey>,
    edges: Vec<CapturedEdge>,
    target_ids: BTreeMap<Arc<ConfiguredTargetKey>, u32>,
    target_keys: Vec<Arc<ConfiguredTargetKey>>,
    application_ids: BTreeMap<(u32, Arc<str>, Vec<u32>), u32>,
    strings: BTreeSet<Arc<str>>,
    applications: Vec<CapturedAspect>,
    requests: BTreeMap<u32, Vec<u32>>,
    /// Formatter facts of each configured identity, read from the live
    /// analysis state at the cquery boundary (provider key names, important
    /// output files). Not providers: their values never enter the graph.
    facts: BTreeMap<(Label, Option<ConfigId>), Result<Facts, String>>,
    provider_lists: HashSet<Arc<[String]>>,
}

fn capture_interned(capture: &mut Option<Capture>, names: Vec<String>) -> Option<Arc<[String]>> {
    let capture = capture.as_mut()?;
    if let Some(existing) = capture.provider_lists.get(names.as_slice()) {
        return Some(existing.clone());
    }
    let names: Arc<[String]> = Arc::from(names);
    capture.provider_lists.insert(names.clone());
    Some(names)
}

/// What `--output=starlark|files` read from a configured target that the
/// provider-free graph cannot reconstruct ([`Engine::capture_cquery_facts`]).
#[derive(Clone, Debug, Default)]
pub(super) struct Facts {
    /// The configured target's providers, as `providers(target)` keys them
    /// (AbstractConfiguredTarget.tryAddProviderForQuery), in genuine order;
    /// interned (few distinct lists).
    providers: Arc<[String]>,
    /// The exec paths of the important artifacts of the requested output
    /// groups (TopLevelArtifactHelper.getAllArtifactsToBuild).
    files: Vec<String>,
}

/// Sparse Skyframe-like provenance, not copies of providers or base rule edges.
/// Targets, attribute names and applications are interned once, including
/// ordinary edges. No edge owns copies of its two configured labels.
struct CapturedAspect {
    base: u32,
    class: Arc<str>,
    descriptor: Arc<str>,
    applicable: bool,
    targets: Vec<u32>,
    aspects: Vec<u32>,
    /// The aspect declaration's `fragments` (`--show_config_fragments`),
    /// `None` when empty (no allocation per application).
    fragments: Option<Arc<[String]>>,
}

impl Capture {
    fn string(&mut self, text: &str) -> Arc<str> {
        if let Some(text) = self.strings.get(text) {
            return text.clone();
        }
        let text: Arc<str> = Arc::from(text);
        self.strings.insert(text.clone());
        text
    }

    fn target(&mut self, key: ConfiguredTargetKey) -> u32 {
        if let Some(id) = self.target_ids.get(&key) {
            return *id;
        }
        let id = self.target_keys.len() as u32;
        let key = Arc::new(key);
        self.target_ids.insert(key.clone(), id);
        self.target_keys.push(key);
        id
    }

    pub(super) fn applications(
        &mut self,
        base: &ConfiguredTargetKey,
        collection: &crate::aspects::AspectCollection,
        rule: &Rule,
    ) -> Vec<u32> {
        let base_id = self.target(base.clone());
        let mut ids = Vec::with_capacity(collection.used.len());
        for (aspect, bases) in &collection.used {
            let descriptor = self.string(&aspect.descriptor);
            // The collection is topological. Refer to the exact preceding
            // applications, including their own base paths, not just names.
            let required: Vec<_> = collection
                .used
                .iter()
                .take(ids.len())
                .enumerate()
                .filter(|(_, (aspect, _))| bases.contains(&aspect.descriptor))
                .map(|(index, _)| ids[index])
                .collect();
            let key = (base_id, descriptor.clone(), required.clone());
            let id = if let Some(id) = self.application_ids.get(&key) {
                *id
            } else {
                let id = self.applications.len() as u32;
                self.application_ids.insert(key, id);
                let class = self.string(&aspect.identity);
                self.applications.push(CapturedAspect {
                    base: base_id,
                    class,
                    descriptor,
                    applicable: aspect.applies_to_rule(rule),
                    targets: Vec::new(),
                    aspects: required,
                    fragments: (!aspect.declaration.fragments.is_empty())
                        .then(|| aspect.declaration.fragments.clone().into()),
                });
                id
            };
            ids.push(id);
        }
        ids
    }

    fn application_dependencies(&mut self, id: u32, dependencies: impl IntoIterator<Item = u32>) {
        let app = &mut self.applications[id as usize];
        for dependency in dependencies {
            if !app.aspects.contains(&dependency) {
                app.aspects.push(dependency);
            }
        }
    }

    pub(super) fn request(&mut self, requester: &ConfiguredTargetKey, ids: Vec<u32>) {
        let requester = self.target(requester.clone());
        let requests = self.requests.entry(requester).or_default();
        for id in ids {
            if !requests.contains(&id) {
                requests.push(id);
            }
        }
    }

    pub(super) fn aspect_target(&mut self, id: u32, label: Label, config: ConfigId) {
        let target = self.target(ConfiguredTargetKey::new(label, config));
        self.aspect_targets(id, &[target]);
    }

    pub(super) fn aspect_targets(&mut self, id: u32, dependencies: &[u32]) {
        let targets = &mut self.applications[id as usize].targets;
        for target in dependencies {
            if !targets.contains(target) {
                targets.push(*target);
            }
        }
    }

    pub(super) fn propagated(
        &mut self,
        ids: &[u32],
        collection: &crate::aspects::AspectCollection,
        children: &[u32],
        attribute: &str,
        attached: &[Arc<crate::aspects::AspectDefinition>],
    ) {
        for (index, (aspect, _)) in collection.used.iter().enumerate() {
            // AspectFunction computes dependencies over THIS aspect's path,
            // not every aspect that happened to be applied to the same base.
            let path = collection.path_descriptions(&aspect.descriptor);
            let incoming: Vec<_> = collection
                .used
                .iter()
                .filter(|(a, _)| path.contains(&a.descriptor))
                .map(|(a, _)| a.clone())
                .collect();
            let propagated = crate::aspects::propagating_aspects(attached, &incoming, attribute);
            let children: Vec<_> = children
                .iter()
                .copied()
                .filter(|id| {
                    propagated.iter().any(|a| {
                        a.descriptor.as_str() == self.applications[*id as usize].descriptor.as_ref()
                    })
                })
                .collect();
            self.application_dependencies(ids[index], children);
        }
    }
}

struct CapturedEdge {
    from: u32,
    to: u32,
    attribute: Arc<str>,
    tool: bool,
    aspect: bool,
}

/// Output-independent compact configured graph. Node identity is (label, config),
/// never label alone. Configurations are indexed once, sources use null.
#[derive(Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Graph {
    pub nodes: Vec<Node>,
    pub configurations: Vec<wire::Configuration>,
    pub top_checksum: String,
    pub patterns: BTreeMap<String, Vec<u32>>,
    pub repository_mapping: BTreeMap<String, String>,
    /// Interned `providers(target)` key lists ([`Node::providers`]).
    pub provider_sets: Vec<Vec<String>>,
    /// `build_options(target)` of each configuration (same indices as
    /// `configurations`), computed while the native options are live.
    pub build_options: Vec<Vec<(String, crate::cquery_starlark::BuildOption)>>,
    /// Nodes whose formatter facts could not be read at the analysis
    /// boundary; only `--output=starlark|files` report them.
    pub fact_errors: BTreeMap<u32, String>,
    /// Whether [`Node::files`] were recorded (`--output=files` graphs only).
    pub files_recorded: bool,
    /// `--output=build` graphs only: each analyzed rule node's BUILD block
    /// with its configured attribute values (BuildOutputFormatterCallback).
    pub build_blocks: BTreeMap<u32, String>,
    pub build_recorded: bool,
    /// `--output=transitions` graphs only (super::cquery_presentation).
    pub transitions: BTreeMap<u32, super::cquery_presentation::NodeTransitions>,
    pub transitions_recorded: bool,
    /// `--show_config_fragments` graphs only (super::cquery_presentation).
    pub fragments: BTreeMap<u32, super::cquery_presentation::NodeFragments>,
    pub fragments_recorded: bool,
}

#[derive(Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Node {
    pub target: wire::Target,
    pub configuration: Option<u32>,
    pub edges: Vec<GraphEdge>,
    pub incompatible: bool,
    /// Only failed select projections. Label output never inspects ordinary
    /// attributes; proto/attr() fail explicitly rather than inventing values.
    pub attribute_errors: Option<Box<BTreeMap<String, String>>>,
    /// Index into [`Graph::provider_sets`]; [`NO_PROVIDERS`] when
    /// `providers(target)` is None (package groups).
    pub providers: u32,
    /// Important artifacts of the requested output groups (`--output=files`).
    pub files: Box<[String]>,
}

/// [`Node::providers`] of a target without a providers dict.
pub const NO_PROVIDERS: u32 = u32::MAX;

#[derive(Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct GraphEdge {
    pub to: u32,
    pub attribute: String,
    pub implicit: bool,
    pub tool: bool,
    pub aspect: bool,
}

#[derive(Clone, Debug)]
pub struct Options {
    pub common: super::query::QueryOptions,
    pub universe: Vec<String>,
    pub include_aspects: bool,
    pub writer: wire::WriterOptions,
    /// `--starlark:expr` / `--starlark:file` (CqueryOptions.expr/file).
    pub starlark_expr: String,
    pub starlark_file: String,
    /// `--show_config_fragments` (off|direct|transitive).
    pub show_config_fragments: String,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            common: Default::default(),
            universe: Vec::new(),
            include_aspects: true,
            writer: Default::default(),
            starlark_expr: String::new(),
            starlark_file: String::new(),
            show_config_fragments: "off".into(),
        }
    }
}

/// CqueryCommand's formatter names, in its error message order.
const OUTPUT_FORMATS: &str = "label_kind, label, transitions, proto, streamed_proto, textproto, jsonproto, build, graph, starlark, files";

impl Options {
    pub fn parse(&mut self, arg: &str, next: Option<&str>) -> Result<Option<bool>, String> {
        for (name, destination) in [
            ("output", &mut self.common.output),
            ("query_file", &mut self.common.query_file),
            ("output_file", &mut self.common.output_file),
        ] {
            if let Some(value) = arg.strip_prefix(&format!("--{name}=")) {
                *destination = value.to_owned();
                return Ok(Some(false));
            }
            if arg == format!("--{name}") {
                *destination = next
                    .ok_or_else(|| format!("Expected value after --{name}"))?
                    .to_owned();
                return Ok(Some(true));
            }
        }
        for name in [
            "universe_scope",
            "transitions",
            "starlark:expr",
            "starlark:file",
            "show_config_fragments",
            "graph:node_limit",
        ] {
            let (value, used) = if let Some(value) = arg.strip_prefix(&format!("--{name}=")) {
                (value, false)
            } else if arg == format!("--{name}") {
                (
                    next.ok_or_else(|| format!("Expected value after --{name}"))?,
                    true,
                )
            } else {
                continue;
            };
            match name {
                "universe_scope" => {
                    self.universe = value
                        .split(',')
                        .filter(|s| !s.is_empty())
                        .map(str::to_owned)
                        .collect()
                }
                "transitions" => {
                    self.writer.transitions = match value {
                        "none" => wire::Transitions::None,
                        "lite" => wire::Transitions::Lite,
                        "full" => wire::Transitions::Full,
                        _ => {
                            return Err(format!(
                                "While parsing option --transitions={value}: Not a valid transition verbosity: '{value}' (should be full, lite or none)"
                            ));
                        }
                    }
                }
                "starlark:expr" => self.starlark_expr = value.to_owned(),
                "starlark:file" => self.starlark_file = value.to_owned(),
                "show_config_fragments" => {
                    if !matches!(value, "off" | "direct" | "transitive") {
                        crate::exit_code::record(crate::exit_code::COMMAND_LINE_ERROR);
                        return Err(format!(
                            "While parsing option --show_config_fragments={value}: Not a valid include config fragments provider option: '{value}' (should be off, direct or transitive)"
                        ));
                    }
                    self.show_config_fragments = value.to_owned();
                }
                _ => {
                    self.common.parse(&format!("--{name}={value}"), None)?;
                }
            }
            return Ok(Some(used));
        }
        for (name, destination) in [
            ("implicit_deps", &mut self.common.implicit_deps),
            ("tool_deps", &mut self.common.tool_deps),
            ("include_aspects", &mut self.include_aspects),
            (
                "proto:include_configurations",
                &mut self.writer.include_configurations,
            ),
            (
                "proto:default_values",
                &mut self.writer.include_default_values,
            ),
            ("proto:locations", &mut self.writer.include_locations),
            ("graph:factored", &mut self.common.graph_factored),
            (
                "proto:rule_inputs_and_outputs",
                &mut self.writer.include_rule_inputs_and_outputs,
            ),
        ] {
            if let Some(value) = crate::configuration::boolean_option(arg, name) {
                *destination = value?;
                return Ok(Some(false));
            }
        }
        Ok(None)
    }

    pub fn expression(&self, text: &str) -> Result<Expr, String> {
        match self.common.output.as_str() {
            "label" | "label_kind" | "proto" | "streamed_proto" | "jsonproto" | "textproto"
            | "starlark" | "files" | "graph" | "build" | "transitions" => {}
            other => {
                crate::exit_code::record(crate::exit_code::COMMAND_LINE_ERROR);
                return Err(format!(
                    "Invalid output format '{other}'. Valid values are: {OUTPUT_FORMATS}"
                ));
            }
        }
        crate::query_language::parse_cquery(text).map_err(|e| e.to_string())
    }

    pub fn universe(&self, expression: &Expr) -> Vec<String> {
        if !self.universe.is_empty() {
            return self.universe.clone();
        }
        let mut patterns = Vec::new();
        expression.collect_target_patterns(&mut patterns);
        patterns.sort();
        patterns.dedup();
        patterns
    }
}

type Nodes = BTreeSet<u32>;

/// The ordinary target-pattern selection/expansion work, before configured
/// analysis. Owned by the same engine's repository set; consuming it resumes
/// that engine without repeating package loading or repository callbacks.
pub struct LoadedQuery {
    roots: Vec<Label>,
    expanded: BTreeMap<String, Vec<Label>>,
    repositories: std::sync::Arc<crate::repository::RepositorySet>,
    current: Option<crate::repository::CurrentRefresh>,
}

impl LoadedQuery {
    pub fn current(&self) -> &crate::repository::CurrentRefresh {
        self.current
            .as_ref()
            .expect("explicit current-loading continuation")
    }
}

impl Graph {
    fn checksum(&self, node: &Node) -> &str {
        node.configuration.map_or("null", |id| {
            self.configurations[id as usize].checksum.as_str()
        })
    }

    fn pattern(&self, text: &str, relative: &str) -> Result<Nodes, String> {
        if let Some(ids) = self.patterns.get(text) {
            return Ok(ids.iter().copied().collect());
        }
        let pattern = crate::target_pattern::parse(text, relative)?;
        if pattern.negative {
            return Err(
                "negative target literals are not supported in configured expressions; use except"
                    .into(),
            );
        }
        let repository = match &pattern.repo {
            None => "",
            Some(repo) if pattern.repo_is_canonical => repo,
            Some(repo) => self
                .repository_mapping
                .get(repo)
                .map(String::as_str)
                .ok_or_else(|| format!("unknown repository @{repo}"))?,
        };
        use crate::target_pattern::Kind;
        let mut selected = Nodes::new();
        for (id, node) in self.nodes.iter().enumerate() {
            let label = Label::parse_canonical(node.label())?;
            if label.repository != repository {
                continue;
            }
            let matches =
                match &pattern.kind {
                    Kind::SingleTarget { target } => {
                        label.package == pattern.package && label.name == *target
                    }
                    Kind::TargetsInPackage { rules_only, .. } => {
                        label.package == pattern.package
                            && (!rules_only || matches!(node.target, wire::Target::Rule(_)))
                    }
                    Kind::TargetsBelowDirectory { rules_only } => {
                        (pattern.package.is_empty()
                            || label.package == pattern.package
                            || label.package.starts_with(&format!("{}/", pattern.package)))
                            && (!rules_only || matches!(node.target, wire::Target::Rule(_)))
                    }
                    Kind::PathAsTarget { .. } => return Err(
                        "relative path-as-target cquery expressions require an explicit :target"
                            .into(),
                    ),
                };
            if matches {
                selected.insert(id as u32);
            }
        }
        if selected.is_empty() && matches!(pattern.kind, Kind::SingleTarget { .. }) {
            return Err(format!(
                "target {text} is not in the analyzed cquery universe"
            ));
        }
        Ok(selected)
    }

    fn edge_allowed(edge: &GraphEdge, options: &Options) -> bool {
        (options.common.implicit_deps || !edge.implicit)
            && (options.common.tool_deps || !edge.tool)
            && (options.include_aspects || !edge.aspect)
    }

    fn closure(&self, roots: &Nodes, depth: usize, options: &Options) -> Nodes {
        let mut found = roots.clone();
        let mut frontier = roots.clone();
        for _ in 0..depth {
            let mut next = Nodes::new();
            for id in frontier {
                for edge in &self.nodes[id as usize].edges {
                    if Self::edge_allowed(edge, options) && found.insert(edge.to) {
                        next.insert(edge.to);
                    }
                }
            }
            if next.is_empty() {
                break;
            }
            frontier = next;
        }
        found
    }

    /// The configured targets `expression` selects (also aquery's, crate::aquery).
    pub fn evaluate(
        &self,
        expression: &Expr,
        options: &Options,
        relative: &str,
        variables: &mut BTreeMap<String, Nodes>,
    ) -> Result<Nodes, String> {
        let mut eval = |expr: &Expr| self.evaluate(expr, options, relative, variables);
        match expression {
            Expr::Target(text) if text.starts_with('$') => variables
                .get(&text[1..])
                .cloned()
                .ok_or_else(|| format!("undefined query variable {text}")),
            Expr::Target(text) => self.pattern(text, relative),
            Expr::Set(words) => {
                let mut found = Nodes::new();
                for word in words {
                    found.extend(self.pattern(word, relative)?);
                }
                Ok(found)
            }
            Expr::Let(name, value, body) => {
                let value = eval(value)?;
                let previous = variables.insert(name.clone(), value);
                let result = self.evaluate(body, options, relative, variables);
                match previous {
                    Some(previous) => {
                        variables.insert(name.clone(), previous);
                    }
                    None => {
                        variables.remove(name);
                    }
                }
                result
            }
            Expr::Binary(operator, operands) => {
                let mut result = eval(&operands[0])?;
                for operand in &operands[1..] {
                    let right = eval(operand)?;
                    match operator {
                        Operator::Union | Operator::Plus => result.extend(right),
                        Operator::Intersect | Operator::Caret => {
                            result.retain(|id| right.contains(id))
                        }
                        Operator::Except | Operator::Minus => {
                            result.retain(|id| !right.contains(id))
                        }
                    }
                }
                Ok(result)
            }
            Expr::Function(function, args) => {
                let expr = |index| match &args[index] {
                    Argument::Expression(expr) => expr,
                    _ => unreachable!("parser argument type"),
                };
                let word = |index| match &args[index] {
                    Argument::Word(word) => word.as_str(),
                    _ => unreachable!("parser argument type"),
                };
                let depth = |index| -> Result<usize, String> {
                    match args.get(index) {
                        Some(Argument::Integer(n)) => usize::try_from(*n)
                            .map_err(|_| "query depth/count must be nonnegative".into()),
                        None => Ok(usize::MAX),
                        _ => unreachable!("parser argument type"),
                    }
                };
                match function {
                    Function::Deps => Ok(self.closure(&eval(expr(0))?, depth(1)?, options)),
                    Function::Config => {
                        let input = eval(expr(0))?;
                        let selector = word(1);
                        let checksum = if selector == "target" {
                            self.top_checksum.as_str()
                        } else if selector == "null" {
                            "null"
                        } else {
                            let matches: Vec<_> = self
                                .configurations
                                .iter()
                                .filter(|c| c.checksum.starts_with(selector))
                                .collect();
                            if matches.len() != 1 {
                                return Err(format!(
                                    "configuration prefix {selector:?} matches {} configurations",
                                    matches.len()
                                ));
                            }
                            matches[0].checksum.as_str()
                        };
                        Ok(input
                            .into_iter()
                            .filter(|id| self.checksum(&self.nodes[*id as usize]) == checksum)
                            .collect())
                    }
                    Function::Kind | Function::Filter | Function::Attr => {
                        let (pattern, at) = if *function == Function::Attr {
                            (word(1), 2)
                        } else {
                            (word(0), 1)
                        };
                        let regex = fancy_regex::Regex::new(pattern)
                            .map_err(|e| format!("invalid query regex: {e}"))?;
                        let mut result = Nodes::new();
                        for id in eval(expr(at))? {
                            let node = &self.nodes[id as usize];
                            let value = match function {
                                Function::Kind => node.kind(),
                                Function::Filter => node.label().to_owned(),
                                _ => {
                                    if let Some(error) = node
                                        .attribute_errors
                                        .as_ref()
                                        .and_then(|errors| errors.get(word(0)))
                                    {
                                        return Err(error.clone());
                                    }
                                    let wire::Target::Rule(rule) = &node.target else {
                                        continue;
                                    };
                                    let Some(attribute) =
                                        rule.attributes.iter().find(|a| a.name == word(0))
                                    else {
                                        continue;
                                    };
                                    let Some(text) = attribute_text(&attribute.value)? else {
                                        continue;
                                    };
                                    text
                                }
                            };
                            if regex
                                .is_match(&value)
                                .map_err(|e| format!("query regex evaluation failed: {e}"))?
                            {
                                result.insert(id);
                            }
                        }
                        Ok(result)
                    }
                    Function::Labels => {
                        let mut found = Nodes::new();
                        for id in eval(expr(1))? {
                            for edge in &self.nodes[id as usize].edges {
                                if edge.attribute == word(0) && Self::edge_allowed(edge, options) {
                                    found.insert(edge.to);
                                }
                            }
                        }
                        Ok(found)
                    }
                    Function::Some => Ok(eval(expr(0))?
                        .into_iter()
                        .take(depth(1)?.min(if args.len() == 1 { 1 } else { usize::MAX }))
                        .collect()),
                    Function::Rdeps => {
                        let universe = self.closure(&eval(expr(0))?, usize::MAX, options);
                        let mut found: Nodes =
                            eval(expr(1))?.intersection(&universe).copied().collect();
                        for _ in 0..depth(2)? {
                            let next: Vec<_> = universe
                                .iter()
                                .copied()
                                .filter(|id| {
                                    !found.contains(id)
                                        && self.nodes[*id as usize].edges.iter().any(|e| {
                                            Self::edge_allowed(e, options) && found.contains(&e.to)
                                        })
                                })
                                .collect();
                            if next.is_empty() {
                                break;
                            }
                            found.extend(next);
                        }
                        Ok(found)
                    }
                    _ => Err(format!(
                        "configured query function {} is not implemented",
                        function.name()
                    )),
                }
            }
        }
    }

    /// Stream one result at a time; no second graph or complete encoded buffer.
    pub fn write(
        &self,
        expression: &Expr,
        options: &Options,
        relative: &str,
        destination: impl Write,
    ) -> Result<(), String> {
        let ids = self.evaluate(expression, options, relative, &mut BTreeMap::new())?;
        let mut destination = std::io::BufWriter::new(destination);
        match options.common.output.as_str() {
            "starlark" => {
                self.write_starlark(&self.sorted(&ids), options, &mut destination)?;
                return destination.flush().map_err(|e| e.to_string());
            }
            "files" => {
                self.write_files(&self.sorted(&ids), &mut destination)?;
                return destination.flush().map_err(|e| e.to_string());
            }
            "graph" => {
                self.write_graph(&ids, options, &mut destination)?;
                return destination.flush().map_err(|e| e.to_string());
            }
            "build" => {
                self.write_build(&self.sorted(&ids), &mut destination)?;
                return destination.flush().map_err(|e| e.to_string());
            }
            "transitions" => {
                if options.writer.transitions == wire::Transitions::None {
                    eprintln!(
                        "ERROR: Instead of using --output=transitions, set the --transitions flag explicitly to 'lite' or 'full'"
                    );
                    return Ok(());
                }
                self.write_transitions(
                    &self.sorted(&ids),
                    options.writer.transitions == wire::Transitions::Full,
                    &mut destination,
                )?;
                return destination.flush().map_err(|e| e.to_string());
            }
            "jsonproto" | "textproto" => {
                let mut transcoder = crate::cquery_text::Transcoder::new(
                    &mut destination,
                    options.common.output == "jsonproto",
                    options.writer.include_configurations,
                );
                self.write_proto(
                    &ids,
                    options,
                    wire::OutputFormat::StreamedProto,
                    &mut transcoder,
                )?;
                transcoder.finish().map_err(|e| e.to_string())?;
                return destination.flush().map_err(|e| e.to_string());
            }
            _ => {}
        }
        if matches!(options.common.output.as_str(), "label" | "label_kind") {
            let mut memo = HashMap::new();
            for id in self.sorted(&ids) {
                let node = &self.nodes[id as usize];
                if options.common.output == "label_kind" {
                    write!(destination, "{} ", node.kind()).map_err(|e| e.to_string())?;
                }
                let checksum = self.checksum(node);
                write!(
                    destination,
                    "{} ({})",
                    node.label(),
                    if checksum == "null" {
                        checksum
                    } else {
                        &checksum[..7]
                    }
                )
                .map_err(|e| e.to_string())?;
                if options.show_config_fragments != "off" {
                    let fragments = self.fragments_text(
                        id,
                        options.show_config_fragments == "transitive",
                        &mut memo,
                    )?;
                    write!(destination, " {fragments}").map_err(|e| e.to_string())?;
                }
                writeln!(destination).map_err(|e| e.to_string())?;
            }
            return destination.flush().map_err(|e| e.to_string());
        }
        let format = if options.common.output == "proto" {
            wire::OutputFormat::Proto
        } else {
            wire::OutputFormat::StreamedProto
        };
        self.write_proto(&ids, options, format, destination)
    }

    /// Results in label order (then discovery order): the order of every
    /// native line-oriented formatter.
    fn sorted(&self, ids: &Nodes) -> Vec<u32> {
        let mut ids: Vec<_> = ids.iter().copied().collect();
        ids.sort_by(|a, b| {
            self.nodes[*a as usize]
                .label()
                .cmp(self.nodes[*b as usize].label())
                .then(a.cmp(b))
        });
        ids
    }

    /// The actual configured target an alias forwards to
    /// (AliasConfiguredTarget.getActual, through alias chains).
    fn actual(&self, mut id: u32) -> u32 {
        for _ in 0..self.nodes.len() {
            let node = &self.nodes[id as usize];
            let wire::Target::Rule(rule) = &node.target else {
                break;
            };
            if node.incompatible || rule.rule_class != "alias" {
                break;
            }
            match node.edges.iter().find(|edge| edge.attribute == "actual") {
                Some(edge) => id = edge.to,
                None => break,
            }
        }
        id
    }

    fn write_starlark(
        &self,
        ids: &[u32],
        options: &Options,
        destination: &mut impl Write,
    ) -> Result<(), String> {
        let formatter =
            crate::cquery_starlark::Formatter::new(&options.starlark_expr, &options.starlark_file)
                .map_err(|e| format!("Error doing post analysis query: {e}"))?;
        for &id in ids {
            let node = &self.nodes[id as usize];
            let actual_id = self.actual(id);
            let actual = &self.nodes[actual_id as usize];
            let alias = (actual_id != id).then(|| node.label());
            let providers_error = self.fact_errors.get(&actual_id).map(String::as_str);
            let view = crate::cquery_starlark::TargetView {
                original: node.label(),
                label: actual.label(),
                repr: crate::cquery_starlark::target_repr(&actual.target, actual.label(), alias),
                providers: (actual.providers != NO_PROVIDERS)
                    .then(|| self.provider_sets[actual.providers as usize].as_slice()),
                providers_error,
                build_options: node
                    .configuration
                    .and_then(|index| self.build_options.get(index as usize))
                    .map(Vec::as_slice),
            };
            match formatter.format(&view) {
                Ok(line) => writeln!(destination, "{line}").map_err(|e| e.to_string())?,
                // StarlarkOutputFormatterCallback.processOutput: an error
                // event per target; formatting continues.
                Err(error) => eprintln!("ERROR: {error}"),
            }
        }
        Ok(())
    }

    /// BuildOutputFormatterCallback: each result's rule (an output file's
    /// generating rule) once per label (TargetOutputter's printed set), the
    /// first configured target's values (native: label, then configuration
    /// order; genuine: Skyframe order).
    fn write_build(&self, ids: &[u32], destination: &mut impl Write) -> Result<(), String> {
        if !self.build_recorded {
            return Err("internal error: this configured graph records no BUILD blocks".into());
        }
        let mut printed = HashSet::new();
        for &id in ids {
            let node = &self.nodes[id as usize];
            let rule = match &node.target {
                wire::Target::Rule(_) => id,
                wire::Target::GeneratedFile(_) => {
                    match node.edges.iter().find(|edge| {
                        edge.attribute.is_empty()
                            && !edge.aspect
                            && matches!(self.nodes[edge.to as usize].target, wire::Target::Rule(_))
                    }) {
                        Some(edge) => edge.to,
                        None => continue,
                    }
                }
                _ => continue,
            };
            let label = self.nodes[rule as usize].label();
            if printed.contains(label) {
                continue;
            }
            if let Some(error) = self.nodes[rule as usize]
                .attribute_errors
                .as_ref()
                .and_then(|errors| errors.values().next())
            {
                return Err(error.clone());
            }
            let Some(block) = self.build_blocks.get(&rule) else {
                continue;
            };
            printed.insert(label);
            destination
                .write_all(block.as_bytes())
                .map_err(|e| e.to_string())?;
        }
        Ok(())
    }

    fn write_files(&self, ids: &[u32], destination: &mut impl Write) -> Result<(), String> {
        if !self.files_recorded {
            return Err("internal error: this configured graph records no output files".into());
        }
        let mut seen = HashSet::new();
        for &id in ids {
            let node = &self.nodes[id as usize];
            if let wire::Target::Rule(rule) = &node.target
                && rule.rule_class.contains('$')
            {
                continue;
            }
            if let Some(error) = self.fact_errors.get(&id) {
                return Err(error.clone());
            }
            for file in node.files.iter() {
                if seen.insert(file.as_str()) {
                    writeln!(destination, "{file}").map_err(|e| e.to_string())?;
                }
            }
        }
        Ok(())
    }

    /// GraphOutputFormatterCallback / GraphOutputWriter: the result's
    /// subgraph, ordered by label then configuration checksum (null first).
    fn write_graph(
        &self,
        ids: &Nodes,
        options: &Options,
        destination: &mut impl Write,
    ) -> Result<(), String> {
        type Key = (String, Option<String>, u32);
        let key = |id: u32| -> Key {
            let node = &self.nodes[id as usize];
            let checksum = self.checksum(node);
            (
                node.label().to_owned(),
                (checksum != "null").then(|| checksum.to_owned()),
                id,
            )
        };
        let mut graph: BTreeMap<Key, BTreeSet<Key>> = BTreeMap::new();
        for &id in ids {
            let successors = self.nodes[id as usize]
                .edges
                .iter()
                .filter(|edge| Self::edge_allowed(edge, options) && ids.contains(&edge.to))
                .map(|edge| key(edge.to))
                .collect();
            graph.insert(key(id), successors);
        }
        let name = |key: &Key| -> String {
            let checksum = key.1.as_deref().map_or("null", |checksum| &checksum[..7]);
            format!("{} ({checksum})", key.0)
        };
        let mut out = String::from("digraph mygraph {\n  node [shape=box];\n");
        if !options.common.graph_factored {
            for node in super::query::topological_order(&graph) {
                let from = name(&node);
                out.push_str(&format!("  \"{from}\"\n"));
                for successor in &graph[&node] {
                    out.push_str(&format!("  \"{from}\" -> \"{}\"\n", name(successor)));
                }
            }
        } else {
            let mut predecessors: BTreeMap<&Key, BTreeSet<&Key>> =
                graph.keys().map(|node| (node, BTreeSet::new())).collect();
            for (from, successors) in &graph {
                for to in successors {
                    if let Some(set) = predecessors.get_mut(to) {
                        set.insert(from);
                    }
                }
            }
            let mut classes: BTreeMap<(Vec<&Key>, Vec<&Key>), Vec<&Key>> = BTreeMap::new();
            for node in graph.keys() {
                let class = (
                    predecessors[node].iter().copied().collect(),
                    graph[node].iter().collect(),
                );
                classes.entry(class).or_default().push(node);
            }
            let mut partition: Vec<Vec<&Key>> = classes.into_values().collect();
            partition.sort();
            let class_of: HashMap<&Key, usize> = partition
                .iter()
                .enumerate()
                .flat_map(|(index, class)| class.iter().map(move |node| (*node, index)))
                .collect();
            let mut class_graph: BTreeMap<usize, BTreeSet<usize>> = BTreeMap::new();
            for (index, class) in partition.iter().enumerate() {
                let successors = class
                    .iter()
                    .flat_map(|node| graph[*node].iter())
                    .map(|successor| class_of[successor])
                    .collect();
                class_graph.insert(index, successors);
            }
            let limit = options.common.graph_node_limit;
            let serialize = |class: &Vec<&Key>| -> String {
                let reserved = "\\n...and 9999999 more items".len() as i64;
                let actual = i64::from(limit) - reserved;
                let mut buffer = String::new();
                for (count, node) in class.iter().enumerate() {
                    let text = name(node);
                    if count > 0 {
                        buffer.push_str("\\n");
                        if limit != -1 && (buffer.len() + text.len()) as i64 > actual {
                            buffer.push_str(&format!("...and {} more items", class.len() - count));
                            break;
                        }
                    }
                    buffer.push_str(&text);
                }
                buffer
            };
            let names: Vec<String> = partition.iter().map(serialize).collect();
            for class in super::query::topological_order(&class_graph) {
                let from = &names[class];
                out.push_str(&format!("  \"{from}\"\n"));
                for successor in &class_graph[&class] {
                    out.push_str(&format!("  \"{from}\" -> \"{}\"\n", names[*successor]));
                }
            }
        }
        out.push_str("}\n");
        destination
            .write_all(out.as_bytes())
            .map_err(|e| e.to_string())
    }

    fn write_proto(
        &self,
        ids: &Nodes,
        options: &Options,
        format: wire::OutputFormat,
        destination: impl Write,
    ) -> Result<(), String> {
        for id in ids {
            if let Some(errors) = &self.nodes[*id as usize].attribute_errors
                && let Some(error) = errors.values().next()
            {
                return Err(error.clone());
            }
        }
        let mut writer = wire::CqueryWriter::new(destination, format, options.writer);
        for &id in ids {
            let node = &self.nodes[id as usize];
            let configuration = node
                .configuration
                .map(|id| &self.configurations[id as usize]);
            match &node.target {
                wire::Target::Rule(rule) => {
                    let mut rule = rule.clone();
                    let mut inputs = BTreeSet::new();
                    for edge in &node.edges {
                        if !Self::edge_allowed(edge, options) || !inputs.insert(edge.to) {
                            continue;
                        }
                        let target = &self.nodes[edge.to as usize];
                        if !node.incompatible {
                            rule.rule_inputs.push(target.label().to_owned());
                        }
                        rule.configured_rule_inputs.push(wire::ConfiguredRuleInput {
                            label: target.label().to_owned(),
                            configuration_checksum: target
                                .configuration
                                .map(|id| self.configurations[id as usize].checksum.clone()),
                            configuration_id: target
                                .configuration
                                .map(|id| self.configurations[id as usize].id),
                        });
                    }
                    writer
                        .write_target(&wire::Target::Rule(rule), configuration)
                        .map_err(|e| e.to_string())?;
                }
                _ => writer
                    .write_target(&node.target, configuration)
                    .map_err(|e| e.to_string())?,
            }
        }
        writer
            .write_configurations(self.configurations.iter())
            .map_err(|e| e.to_string())?;
        writer.finish().map_err(|e| e.to_string())?;
        Ok(())
    }
}

impl Node {
    pub fn label(&self) -> &str {
        match &self.target {
            wire::Target::Rule(target) => &target.name,
            wire::Target::SourceFile(target) => &target.name,
            wire::Target::GeneratedFile(target) => &target.name,
            wire::Target::PackageGroup(target) => &target.name,
        }
    }

    fn kind(&self) -> String {
        match &self.target {
            wire::Target::Rule(rule) => format!("{} rule", rule.rule_class),
            wire::Target::SourceFile(_) => "source file".to_owned(),
            wire::Target::GeneratedFile(_) => "generated file".to_owned(),
            wire::Target::PackageGroup(_) => "package group".to_owned(),
        }
    }
}

/// Register only the identities needed by subsequent provider analysis. No
/// shared-action fingerprint, action conflict scan or action input digesting.
pub(super) fn register_outputs<'a>(
    files: impl IntoIterator<Item = &'a File>,
    config: ConfigId,
    index: &mut BTreeMap<File, Artifact>,
    declared: &mut Declared,
    predeclared: impl Fn(&File) -> bool,
) {
    for file in files {
        let artifact = Artifact::semantic(file);
        register_artifact(index, file, &artifact);
        // Only predeclared outputs have package-level output-file targets.
        // Runtime declarations with the same path must not overwrite their
        // generating-rule identity. Other consumers resolve through `index`.
        if predeclared(file) {
            declared.insert((config, PathBuf::from(&file.path)), artifact);
        }
    }
}

impl Engine {
    /// ToolchainResolutionFunction's direct CT requests (PlatformKeys,
    /// ConstraintValueLookupUtil, ToolchainTypeLookupUtil), NOT registered
    /// toolchain implementations that weren't selected. Resolution has already
    /// analyzed these; the lazy native default platform may have no real node.
    pub(super) fn capture_cquery_aspect_toolchain_context(
        &mut self,
        config: ConfigId,
        context: &ToolchainContextKey,
    ) -> Vec<u32> {
        let Some(capture) = &mut self.cquery_capture else {
            return Vec::new();
        };
        let configuration = self.configurations.get(config);
        let platforms = self
            .registered_execution_platforms
            .get(&config)
            .and_then(|result| result.as_ref().ok())
            .into_iter()
            .flat_map(|platforms| platforms.iter().map(|platform| &platform.label));
        let labels = [
            configuration.native.target_platform(),
            &configuration.native.host_platform,
        ]
        .into_iter()
        .chain(platforms)
        .chain(context.exec_constraints.iter())
        .chain(context.types.iter().map(|requirement| &requirement.label));
        let mut ids = Vec::new();
        for label in labels {
            let key = ConfiguredTargetKey::new(label.clone(), config);
            if capture.rules.contains_key(&key) {
                let id = capture.target(key);
                if !ids.contains(&id) {
                    ids.push(id);
                }
            }
        }
        ids
    }

    pub(super) fn capture_cquery_edge(
        &mut self,
        from: &ConfiguredTargetKey,
        label: Label,
        config: ConfigId,
        attribute: &str,
        tool: bool,
        aspect: bool,
    ) {
        if let Some(capture) = &mut self.cquery_capture {
            let from = match capture.target_ids.get(from) {
                Some(id) => *id,
                None => capture.target(from.clone()),
            };
            let to = capture.target(ConfiguredTargetKey::new(label, config));
            let attribute = capture.string(attribute);
            capture.edges.push(CapturedEdge {
                from,
                to,
                attribute,
                tool,
                aspect,
            });
        }
    }

    /// The formatter facts of every captured configured rule/output/alias
    /// identity, read from the still-live planning state (memoized
    /// dependency views) right after provider analysis. Failures are kept
    /// per identity: only the formatters that read facts report them.
    pub(super) fn capture_cquery_facts(
        &mut self,
        state: &mut ActionPlanning,
        configured: &mut ConfiguredNodes,
        roots: &[Label],
    ) {
        if self.aquery_capture.is_some() {
            return;
        }
        let Some(capture) = &self.cquery_capture else {
            return;
        };
        let mut keys: Vec<ConfiguredTargetKey> = capture.rules.keys().cloned().collect();
        keys.extend(capture.target_keys.iter().map(|key| (**key).clone()));
        keys.extend(
            roots
                .iter()
                .map(|label| self.configurations.top_key(label.clone())),
        );
        let selected = self.selected_output_groups();
        let mut facts = BTreeMap::new();
        for key in keys {
            let Ok(identity) = self.cquery_key(&key) else {
                continue;
            };
            if identity.1.is_none() || facts.contains_key(&identity) {
                continue;
            }
            let result = self.cquery_target_facts(state, configured, &key, &selected);
            facts.insert(identity, result);
        }
        if let Some(capture) = &mut self.cquery_capture {
            capture.facts = facts;
        }
    }

    /// A light read of what a dependency view would expose (crate::engine
    /// `dependency_view`): the committed providers and files to build,
    /// without building or memoizing views, target identities or artifact
    /// registrations for keys nothing depended on.
    fn cquery_target_facts(
        &mut self,
        state: &mut ActionPlanning,
        configured: &mut ConfiguredNodes,
        key: &ConfiguredTargetKey,
        selected: &BTreeSet<String>,
    ) -> Result<Facts, String> {
        if self
            .target_incompatibility(configured, &key.label, key.config)?
            .is_some()
        {
            // The graph's own incompatibility answers (IncompatiblePlatformProvider).
            return Ok(Facts::default());
        }
        let PlanState {
            dry,
            custom_providers,
            provider_keys,
            declared,
            ..
        } = state.plan_state();
        let artifacts = if self.cquery_files {
            self.shared_artifacts(&key.label, key.config, dry, declared)?
        } else {
            Arc::new(Vec::new())
        };
        let actual = self
            .forwarded_actual(configured, key)
            .unwrap_or_else(|| key.label.clone());
        let target = self.target(&actual)?;
        let is_rule = matches!(&target, Target::Rule(rule) if !matches!(rule.kind, RuleKind::PackageGroup(_)));
        let native_kind = match &target {
            Target::Rule(rule) => match &rule.kind {
                RuleKind::Native(kind) => Some(*kind),
                RuleKind::Filegroup => Some("filegroup"),
                RuleKind::Genrule => Some("genrule"),
                _ => None,
            },
            _ => None,
        };
        let providers = custom_providers.get(key);
        let mut files = Vec::new();
        let mut seen = HashSet::new();
        for group in selected.iter().filter(|_| self.cquery_files) {
            if group.starts_with('_') {
                continue;
            }
            if group == crate::output_group_info::DEFAULT {
                for artifact in artifacts.iter() {
                    let path = &artifact.file().path;
                    if seen.insert(path.clone()) {
                        files.push(path.clone());
                    }
                }
                continue;
            }
            if !is_rule {
                continue;
            }
            if let Some(groups) = providers
                .and_then(|providers| providers.get(crate::output_group_info::PROVIDER_ID))
                .and_then(|value| crate::output_group_info::groups(value.value()))
                && let Some((_, depset)) = groups.iter().find(|(name, _)| name == group)
            {
                for file in crate::output_group_info::depset_files(depset) {
                    if seen.insert(file.path.clone()) {
                        files.push(file.path.clone());
                    }
                }
            }
        }
        let actual_key = ConfiguredTargetKey::new(
            actual.clone(),
            self.delegated_config(
                &ConfiguredTargetKey::new(actual.clone(), key.config),
                key.config,
            ),
        );
        let order = provider_keys.get(&actual_key).cloned().unwrap_or_default();
        let mut declared: Vec<String> = Vec::new();
        let mut output_group = is_rule;
        let mut instrumented = false;
        for id in providers.into_iter().flat_map(|providers| providers.keys()) {
            match id.as_str() {
                crate::output_group_info::PROVIDER_ID => output_group = true,
                crate::providers::DEFAULT_INFO_PROVIDER_ID => {}
                crate::instrumented_files::PROVIDER_ID => instrumented = true,
                id => declared.push(crate::cquery_starlark::provider_key_name(id)),
            }
        }
        // InstrumentedFilesInfo.EMPTY of filegroup/genrule/output files, or
        // a coverage configuration's forwarded one (dependency_view).
        let coverage = self
            .configurations
            .get(key.config)
            .native
            .collect_code_coverage;
        let has_toolchain_info = declared.iter().any(|name| name == "ToolchainInfo");
        instrumented |= match &target {
            Target::Rule(_) if coverage => is_rule && !has_toolchain_info,
            Target::Rule(_) => matches!(native_kind, Some("filegroup" | "genrule")),
            Target::Output(..) => true,
            Target::Source(_) => false,
        };
        if instrumented && is_rule {
            declared.push("InstrumentedFilesInfo".into());
        }
        if native_kind == Some("toolchain_type")
            && !declared.iter().any(|n| n == "ToolchainTypeInfo")
        {
            declared.push("ToolchainTypeInfo".into());
        }
        // Declared providers in return order (RuleConfiguredTargetBuilder's
        // LinkedHashMap); unlisted (native) ones keep identity order.
        declared.sort_by_key(|name| {
            let short = name.rsplit_once('%').map_or(name.as_str(), |(_, n)| n);
            order
                .iter()
                .position(|key| key == short)
                .unwrap_or(usize::MAX)
        });
        let mut names = Vec::new();
        if is_rule {
            let files_first = matches!(
                native_kind,
                Some("platform" | "constraint_setting" | "constraint_value")
            );
            if !files_first {
                names.append(&mut declared);
            }
            names.extend(["FileProvider", "FilesToRunProvider"].map(String::from));
            names.append(&mut declared);
            if output_group {
                names.push("OutputGroupInfo".into());
            }
            names.push("DefaultInfo".into());
        } else {
            // OutputFileConfiguredTarget.getProvidersDictForQuery.
            names.extend(
                [
                    "VisibilityProvider",
                    "FileProvider",
                    "FilesToRunProvider",
                    "DefaultInfo",
                ]
                .map(String::from),
            );
            if instrumented {
                names.push("InstrumentedFilesInfo".into());
            }
            if output_group {
                names.push("OutputGroupInfo".into());
            }
        }
        let providers = match capture_interned(&mut self.cquery_capture, names) {
            Some(names) => names,
            None => Arc::from(Vec::new()),
        };
        Ok(Facts { providers, files })
    }

    /// Full configured/provider analysis, never a PlannedRequest. This is not
    /// --nobuild: that still lowers executable actions and may run status.
    pub fn cquery_graph(
        &mut self,
        patterns: &[String],
        relative_directory: &str,
    ) -> Result<Graph, String> {
        let loaded = self.load_cquery_patterns_inner(patterns, relative_directory, false)?;
        self.cquery_graph_loaded(loaded)
    }

    pub fn load_cquery_patterns(
        &mut self,
        patterns: &[String],
        relative_directory: &str,
    ) -> Result<LoadedQuery, String> {
        self.repositories.enable_current_loading();
        self.load_cquery_patterns_inner(patterns, relative_directory, true)
    }

    /// Same ordinary selection, but no pattern seal/lookup for the distinct
    /// root-cut profile. Completed locals are freshly sealed on the live cut.
    pub fn load_cquery_root_patterns(
        &mut self,
        patterns: &[String],
        relative_directory: &str,
    ) -> Result<LoadedQuery, String> {
        self.repositories.enable_current_loading();
        self.load_cquery_patterns_inner(patterns, relative_directory, false)
    }

    fn load_cquery_patterns_inner(
        &mut self,
        patterns: &[String],
        relative_directory: &str,
        current_loading: bool,
    ) -> Result<LoadedQuery, String> {
        if self.keep_going {
            return Err(
                "cquery --keep_going is not implemented; configured query errors fail closed"
                    .into(),
            );
        }
        let mut expanded = BTreeMap::new();
        let roots = self.select(patterns, relative_directory)?;
        for pattern in patterns.iter().filter(|pattern| !pattern.starts_with('-')) {
            let labels = self.select(std::slice::from_ref(pattern), relative_directory)?;
            expanded.insert(pattern.clone(), labels);
        }
        Ok(LoadedQuery {
            roots,
            expanded,
            repositories: self.repositories.clone(),
            current: current_loading.then(|| {
                crate::repository::CurrentRefresh::seal(
                    self.repositories.clone(),
                    self.warnings.is_empty() && self.prefetch.is_none(),
                )
            }),
        })
    }

    pub fn cquery_graph_loaded(&mut self, loaded: LoadedQuery) -> Result<Graph, String> {
        match self.cquery_graph_loaded_inner(loaded, None)? {
            super::root_checkpoint::Outcome::Continue(graph) => Ok(graph),
            super::root_checkpoint::Outcome::AcceptedQuery(_) => unreachable!("no root checkpoint"),
        }
    }

    pub fn cquery_graph_loaded_with_checkpoint(
        &mut self,
        loaded: LoadedQuery,
        budget: super::root_checkpoint::Budget,
        callback: &mut impl FnMut(
            crate::repository::CurrentRefresh,
            super::root_checkpoint::RootCut,
        ) -> Result<super::root_checkpoint::Outcome, String>,
    ) -> Result<super::root_checkpoint::Outcome<Graph>, String> {
        self.root_checkpoint = Some(super::root_checkpoint::Request::new(budget));
        let result = self.cquery_graph_loaded_inner(loaded, Some(callback));
        self.root_checkpoint = None;
        result
    }

    fn cquery_graph_loaded_inner(
        &mut self,
        loaded: LoadedQuery,
        checkpoint: Option<&mut super::root_checkpoint::Callback<'_>>,
    ) -> Result<super::root_checkpoint::Outcome<Graph>, String> {
        if !std::sync::Arc::ptr_eq(&loaded.repositories, &self.repositories) {
            return Err(
                "configured-query continuation belongs to another engine repository set".into(),
            );
        }
        let LoadedQuery {
            roots,
            expanded,
            repositories,
            current,
        } = loaded;
        drop(current);
        drop(repositories);
        self.cquery_capture = Some(Capture::default());
        // Engine instances may issue more than one configured request. These
        // semantic platform memos otherwise bypass the new graph's capture.
        self.platforms.clear();
        let execute = self.execute_build;
        self.execute_build = false;
        let analyzed = self.plan_build_with_checkpoint(&roots, checkpoint);
        self.execute_build = execute;
        let capture = self.cquery_capture.take().expect("capture installed");
        if let super::root_checkpoint::Outcome::AcceptedQuery(graph) = analyzed? {
            // Drop partial action/provider state, not just the graph recorder.
            self.action_planning = None;
            return Ok(super::root_checkpoint::Outcome::AcceptedQuery(graph));
        }
        crate::perf_mark("cquery.analysis_state_dropped");
        crate::perf_mark("cquery.graph_begin");
        let graph = self.finish_cquery_graph(capture, &roots, expanded)?;
        crate::perf_mark("cquery.graph_ready");
        self.record_link_anchors();
        Ok(super::root_checkpoint::Outcome::Continue(graph))
    }

    fn cquery_key(
        &mut self,
        key: &ConfiguredTargetKey,
    ) -> Result<(Label, Option<ConfigId>), String> {
        let owner = match self.target(&key.label)? {
            Target::Source(_) => return Ok((key.label.clone(), None)),
            Target::Rule(rule) if matches!(rule.kind, RuleKind::PackageGroup(_)) => {
                return Ok((key.label.clone(), None));
            }
            Target::Output(rule, _) => ConfiguredTargetKey::new(rule.label.clone(), key.config),
            _ => key.clone(),
        };
        let delegated = self.rule_delegations.get(&owner).unwrap_or(&owner);
        Ok((key.label.clone(), Some(delegated.config)))
    }

    fn finish_cquery_graph(
        &mut self,
        mut capture: Capture,
        roots: &[Label],
        patterns: BTreeMap<String, Vec<Label>>,
    ) -> Result<Graph, String> {
        // Analysis is complete: only the indexed data is read from here on.
        // Release construction indexes before the independent graph grows.
        capture.target_ids.clear();
        capture.application_ids.clear();
        capture.strings.clear();
        let mut ids: BTreeMap<(Label, Option<ConfigId>), u32> = BTreeMap::new();
        let mut intern = |key| -> Result<u32, String> {
            let next = ids.len();
            match ids.entry(key) {
                std::collections::btree_map::Entry::Occupied(entry) => Ok(*entry.get()),
                std::collections::btree_map::Entry::Vacant(entry) => {
                    // The assigned IDs retain the original rule / edge / aspect
                    // discovery order, independently of the interner's ordering.
                    let id = u32::try_from(next)
                        .map_err(|_| "cquery graph exceeds u32 node capacity")?;
                    entry.insert(id);
                    Ok(id)
                }
            }
        };
        for key in capture.rules.keys() {
            intern(self.cquery_key(key)?)?;
        }
        let mut mapped = vec![None; capture.target_keys.len()];
        for edge in &capture.edges {
            let slot = &mut mapped[edge.to as usize];
            if slot.is_none() {
                *slot = Some(intern(
                    self.cquery_key(&capture.target_keys[edge.to as usize])?,
                )?);
            }
        }
        for (slot, key) in mapped.iter_mut().zip(&capture.target_keys) {
            if slot.is_none() {
                *slot = Some(intern(self.cquery_key(key)?)?);
            }
        }
        let mapped: Vec<u32> = mapped.into_iter().map(Option::unwrap).collect();
        for label in roots {
            intern(self.cquery_key(&self.configurations.top_key(label.clone()))?)?;
        }
        let mut configurations = Vec::new();
        let mut build_options = Vec::new();
        let mut config_ids = BTreeMap::new();
        for (id, configuration) in self.configurations.iter() {
            let index = configurations.len() as u32;
            config_ids.insert(id, index);
            let mut options: Vec<_> =
                crate::config_registry::native_option_strings(&configuration.native)
                    .into_iter()
                    .map(|(name, value)| wire::ConfigurationOption {
                        name: Some(name.to_owned()),
                        value: Some(value.strip_prefix('=').unwrap_or(&value).to_owned()),
                    })
                    .collect();
            for (name, value) in [
                ("no_config", configuration.native.no_config.to_string()),
                (
                    "incompatible_strict_action_env",
                    configuration.native.strict_action_env.to_string(),
                ),
                (
                    "action_env",
                    format!(
                        "[{}]",
                        configuration
                            .native
                            .action_env
                            .iter()
                            .map(crate::action_env::EnvVar::reverse)
                            .collect::<Vec<_>>()
                            .join(", ")
                    ),
                ),
                (
                    "host_action_env",
                    format!(
                        "[{}]",
                        configuration
                            .native
                            .host_action_env
                            .iter()
                            .map(crate::action_env::EnvVar::reverse)
                            .collect::<Vec<_>>()
                            .join(", ")
                    ),
                ),
                (
                    "test_env",
                    format!(
                        "[{}]",
                        configuration
                            .native
                            .test_env
                            .iter()
                            .map(crate::action_env::EnvVar::reverse)
                            .collect::<Vec<_>>()
                            .join(", ")
                    ),
                ),
            ] {
                options.push(wire::ConfigurationOption {
                    name: Some(name.to_owned()),
                    value: Some(value),
                });
            }
            let mut fragment_options = vec![wire::FragmentOptions {
                name: "bazel_rs.configuration.NativeOptions".to_owned(),
                options,
            }];
            if !configuration.starlark.is_empty() {
                fragment_options.push(wire::FragmentOptions {
                    name: "user-defined".into(),
                    options: configuration
                        .starlark
                        .iter()
                        .map(|(label, value)| wire::ConfigurationOption {
                            name: Some(label.to_string()),
                            value: Some(value.to_string()),
                        })
                        .collect(),
                });
            }
            build_options.push(crate::cquery_starlark::build_options(configuration));
            configurations.push(wire::Configuration {
                id: index + 1,
                checksum: id.to_string(),
                mnemonic: self.configurations.mnemonic(id).to_owned(),
                platform_name: configuration.native.target_platform().to_string(),
                is_tool: configuration.native.is_exec,
                fragments: vec![wire::Fragment {
                    name: "bazel_rs.configuration.Configuration".into(),
                    fragment_option_names: fragment_options
                        .iter()
                        .map(|f| f.name.clone())
                        .collect(),
                }],
                fragment_options,
            });
        }
        let mut ordered = ids.iter().collect::<Vec<_>>();
        ordered.sort_by_key(|(_, id)| **id);
        let mut nodes = Vec::with_capacity(ids.len());
        let mut provider_sets: Vec<Vec<String>> = Vec::new();
        let mut provider_set_ids: HashMap<Vec<String>, u32> = HashMap::new();
        let mut fact_errors = BTreeMap::new();
        let default_group = self
            .selected_output_groups()
            .contains(crate::output_group_info::DEFAULT);
        let mut hashes = HashMap::new();
        let mut conditions: BTreeMap<&ConfiguredTargetKey, Vec<String>> = BTreeMap::new();
        for edge in &capture.edges {
            if edge.attribute.as_ref() == "$config_dependencies" {
                conditions
                    .entry(&capture.target_keys[edge.from as usize])
                    .or_default()
                    .push(capture.target_keys[edge.to as usize].label.to_string());
            }
        }
        let main_mapping_order = if self.cquery_build {
            self.repositories.main_mapping_order()
        } else {
            Vec::new()
        };
        let mut build_blocks = BTreeMap::new();
        for ((label, config), _) in ordered {
            let mut attribute_errors = BTreeMap::new();
            // `--output=build`: the rule's query attributes with selects
            // resolved, rendered after the match (Engine::configured_build_block).
            let mut build_attributes: Option<Vec<query_attrs::QueryAttribute>> = None;
            let owner = match self.target(label)? {
                Target::Output(rule, _) => rule.label.clone(),
                _ => label.clone(),
            };
            let captured_key = config.map(|id| {
                let key = ConfiguredTargetKey::new(owner.clone(), id);
                if capture.rules.contains_key(&key) {
                    key
                } else {
                    ConfiguredTargetKey::without_rule_transition(owner.clone(), id)
                }
            });
            let incompatible = captured_key
                .as_ref()
                .is_some_and(|key| capture.incompatible.contains_key(key));
            let target = match self.target(label)? {
                Target::Source(path) => wire::Target::SourceFile(wire::SourceFile {
                    name: label.to_string(),
                    location: Some(format!("{}:1:1", path.display())),
                    ..Default::default()
                }),
                Target::Output(rule, _) => wire::Target::GeneratedFile(wire::GeneratedFile {
                    name: label.to_string(),
                    generating_rule: rule.label.to_string(),
                    location: rule.location.clone(),
                }),
                Target::Rule(rule) if matches!(rule.kind, RuleKind::PackageGroup(_)) => {
                    let RuleKind::PackageGroup(group) = &rule.kind else {
                        unreachable!()
                    };
                    wire::Target::PackageGroup(wire::PackageGroup {
                        name: label.to_string(),
                        contained_packages: group.own_specs.query_patterns(),
                        included_package_groups: group
                            .includes
                            .iter()
                            .map(ToString::to_string)
                            .collect(),
                    })
                }
                Target::Rule(declaration) => {
                    let config = config.ok_or("rule has null configuration")?;
                    let key = ConfiguredTargetKey::new(label.clone(), config);
                    let rule = capture
                        .rules
                        .get(&key)
                        .or_else(|| {
                            capture
                                .rules
                                .get(&ConfiguredTargetKey::without_rule_transition(
                                    label.clone(),
                                    config,
                                ))
                        })
                        .ok_or_else(|| format!("cquery missing analyzed rule {key}"))?;
                    let package = self
                        .packages
                        .get(&label.package_id())
                        .ok_or("cquery missing loaded package")?;
                    let retained = captured_key
                        .as_ref()
                        .and_then(|key| capture.incompatible.get(key))
                        .map(|conditions| conditions.iter().cloned().collect());
                    let resolver =
                        retained
                            .as_ref()
                            .map(|conditions| configured_attributes::Resolver {
                                target: label,
                                conditions,
                                config_hash: config.to_string(),
                            });
                    let mut attributes = Vec::new();
                    let mut resolved = Vec::new();
                    for mut attribute in query_attrs::rule_attributes(rule, package) {
                        if attribute.select.is_some()
                            && let Some(resolver) = &resolver
                        {
                            match configured_attributes::query_value(
                                rule,
                                resolver,
                                &attribute.public_name,
                            ) {
                                Ok(value) => {
                                    attribute.value = value;
                                    attribute.select = None;
                                }
                                Err(error) => {
                                    attribute_errors.insert(
                                        attribute.name.clone(),
                                        format!(
                                            "cquery cannot format attribute {} of {}: {error}",
                                            attribute.name, label
                                        ),
                                    );
                                    continue;
                                }
                            }
                        }
                        if self.cquery_build {
                            resolved.push(attribute.clone());
                        }
                        attributes.push(configured_attribute(attribute, rule)?);
                    }
                    if self.cquery_build && attribute_errors.is_empty() {
                        resolved.insert(0, query_attrs::QueryAttribute::name(&label.name));
                        build_attributes = Some(resolved);
                    }
                    attributes.push(wire::Attribute::new(
                        "name",
                        wire::AttributeValue::String(label.name.clone()),
                        true,
                    ));
                    if rule.custom.is_none()
                        && !matches!(
                            rule.kind,
                            RuleKind::Native("alias" | "label_flag" | "label_setting")
                        )
                    {
                        attributes.push(wire::Attribute::new(
                            ":action_listener",
                            wire::AttributeValue::LabelList(Vec::new()),
                            false,
                        ));
                    }
                    if matches!(rule.kind, RuleKind::Native("config_setting")) {
                        let aliases = &self.configurations.get(config).native.flag_aliases;
                        let flags = rule
                            .native
                            .as_ref()
                            .and_then(|native| native.attrs.get("values"));
                        let referenced = match flags {
                            Some(crate::custom_rule::AttrValue::StringDict(values)) => values
                                .iter()
                                .filter_map(|(name, _)| aliases.get(name).map(ToString::to_string))
                                .collect(),
                            _ => Vec::new(),
                        };
                        attributes.push(wire::Attribute::new(
                            ":flag_alias_settings",
                            wire::AttributeValue::LabelList(referenced),
                            false,
                        ));
                    }
                    if matches!(rule.kind, RuleKind::Genrule) {
                        attributes.push(wire::Attribute::new(
                            "$is_executable",
                            wire::AttributeValue::Boolean(genrule_is_executable(rule)),
                            false,
                        ));
                    }
                    // config_dependencies are taken from the analysis edges, not
                    // from the configured rule whose select syntax is gone.
                    if let Some(attribute) = attributes
                        .iter_mut()
                        .find(|a| a.name == "$config_dependencies")
                    {
                        attribute.value = wire::AttributeValue::LabelList(
                            conditions.get(&key).cloned().unwrap_or_default(),
                        );
                    }
                    attributes.sort_by(|a, b| a.name.cmp(&b.name));
                    if let Some(custom) = &rule.custom {
                        let definition = Label::parse_canonical(
                            &custom.declaration.definition_environment_label,
                        )?;
                        let hash = implementation_hash(
                            &definition,
                            &package.load_graph,
                            &self.repositories,
                            &mut hashes,
                        )?;
                        attributes.push(wire::Attribute {
                            name: "$rule_implementation_hash".into(),
                            value: wire::AttributeValue::String(hash),
                            explicitly_specified: None,
                            nodep: None,
                            source_aspect_name: None,
                        });
                    }
                    wire::Target::Rule(wire::Rule {
                        name: label.to_string(),
                        rule_class: rule.rule_class_name().to_owned(),
                        location: rule.location.clone(),
                        attributes,
                        rule_inputs: if incompatible {
                            query_attrs::rule_attributes(&declaration, package)
                                .iter()
                                .flat_map(query_attrs::QueryAttribute::labels)
                                .collect::<BTreeSet<_>>()
                                .into_iter()
                                .collect()
                        } else {
                            Vec::new()
                        },
                        rule_outputs: rule
                            .outs
                            .iter()
                            .map(|out| label.with_name(out).map(|l| l.to_string()))
                            .collect::<Result<_, _>>()?,
                        ..Default::default()
                    })
                }
            };
            if let Some(attributes) = build_attributes {
                let block = self.configured_build_block(label, attributes, &main_mapping_order);
                build_blocks.insert(nodes.len() as u32, block);
            }
            let (provider_names, files) = match self.target(label)? {
                Target::Source(_) => (
                    Some(
                        [
                            "VisibilityProvider",
                            "FileProvider",
                            "FilesToRunProvider",
                            "DefaultInfo",
                        ]
                        .map(String::from)
                        .to_vec(),
                    ),
                    if default_group && self.cquery_files {
                        vec![source_exec_path(label)]
                    } else {
                        Vec::new()
                    },
                ),
                Target::Rule(rule) if matches!(rule.kind, RuleKind::PackageGroup(_)) => {
                    (None, Vec::new())
                }
                // RuleConfiguredTargetBuilder of an incompatible target
                // (IncompatibleTargetChecker.createIncompatibleTarget).
                _ if incompatible => (
                    Some(
                        [
                            "IncompatiblePlatformProvider",
                            "FileProvider",
                            "FilesToRunProvider",
                            "DefaultInfo",
                        ]
                        .map(String::from)
                        .to_vec(),
                    ),
                    Vec::new(),
                ),
                _ => match capture.facts.get(&(label.clone(), *config)) {
                    Some(Ok(facts)) => (Some(facts.providers.to_vec()), facts.files.clone()),
                    Some(Err(error)) => {
                        fact_errors.insert(nodes.len() as u32, error.clone());
                        (None, Vec::new())
                    }
                    None => {
                        fact_errors.insert(
                            nodes.len() as u32,
                            format!("cquery recorded no providers of {label}"),
                        );
                        (None, Vec::new())
                    }
                },
            };
            let providers = match provider_names {
                None => NO_PROVIDERS,
                Some(names) => match provider_set_ids.get(&names) {
                    Some(id) => *id,
                    None => {
                        let id = provider_sets.len() as u32;
                        provider_sets.push(names.clone());
                        provider_set_ids.insert(names, id);
                        id
                    }
                },
            };
            nodes.push(Node {
                target,
                configuration: config.map(|id| config_ids[&id]),
                edges: Vec::new(),
                incompatible,
                attribute_errors: (!attribute_errors.is_empty())
                    .then(|| Box::new(attribute_errors)),
                providers,
                files: files.into_boxed_slice(),
            });
        }
        crate::perf_mark("cquery.graph_vertices_ready");
        for edge in &capture.edges {
            let from = mapped[edge.from as usize];
            let to = mapped[edge.to as usize];
            let implicit = match &nodes[from as usize].target {
                // Config conditions are loaded by DependencyContextProducer,
                // not rule prerequisites enumerated by Util.findImplicitDeps.
                // They remain explicit on the base, but implicit when reached
                // through targetifyValues(null, aspectDeps, ...).
                wire::Target::Rule(rule) => {
                    edge.attribute.as_ref() != "$config_dependencies"
                        && !rule.attributes.iter().any(|a| {
                            (a.name == edge.attribute.as_ref()
                                || a.name.strip_prefix('$').map(|s| format!("_{s}")).as_deref()
                                    == Some(edge.attribute.as_ref()))
                                && a.explicitly_specified == Some(true)
                        })
                }
                _ => false,
            };
            nodes[from as usize].edges.push(GraphEdge {
                to,
                attribute: edge.attribute.to_string(),
                implicit,
                tool: edge.tool,
                aspect: edge.aspect,
            });
        }
        let aspect_targets = mapped;
        // Only the condition/platform dependencies are Skyframe CT deps of an
        // aspect's base context. Ordinary prerequisites come from the base's
        // already computed prerequisites supplier, not fresh CT dependencies.
        // Keep a sparse index so traversal never copies or scans whole graphs.
        let mut context_deps: BTreeMap<u32, Vec<u32>> = BTreeMap::new();
        for (id, node) in nodes.iter().enumerate() {
            for edge in &node.edges {
                if matches!(
                    edge.attribute.as_str(),
                    "$config_dependencies" | "$target_platform"
                ) {
                    context_deps.entry(id as u32).or_default().push(edge.to);
                }
            }
        }
        for (requester, applications) in &capture.requests {
            let from = aspect_targets[*requester as usize];
            let mut known: BTreeSet<_> = nodes[from as usize].edges.iter().map(|e| e.to).collect();
            let mut classes = BTreeSet::new();
            let mut pending: Vec<_> = applications.iter().rev().copied().collect();
            while let Some(id) = pending.pop() {
                let application = &capture.applications[id as usize];
                // PostAnalysisQueryEnvironment.targetifyValues resolves each
                // class once PER REQUESTER, before following propagation. Never
                // union the private deps of every application onto its base.
                if !classes.insert(application.class.as_ref()) {
                    continue;
                }
                let base = aspect_targets[application.base as usize];
                let context = context_deps
                    .get(&base)
                    .into_iter()
                    .flatten()
                    .copied()
                    .filter(|_| application.applicable);
                let targets = application
                    .targets
                    .iter()
                    .map(|id| aspect_targets[*id as usize]);
                for to in std::iter::once(base).chain(context).chain(targets) {
                    if known.insert(to) {
                        nodes[from as usize].edges.push(GraphEdge {
                            to,
                            attribute: String::new(),
                            implicit: true,
                            tool: false,
                            aspect: true,
                        });
                    }
                }
                if application.applicable {
                    pending.extend(application.aspects.iter().rev().copied());
                }
            }
        }
        // An output file points to its generating rule in the same configured
        // identity. It never impersonates that rule or drops its owner edge.
        for ((label, config), id) in &ids {
            if let Target::Output(rule, _) = self.target(label)? {
                if config.is_some_and(|config| {
                    capture
                        .direct_incompatible
                        .contains(&ConfiguredTargetKey::new(rule.label.clone(), config))
                        || capture.direct_incompatible.contains(
                            &ConfiguredTargetKey::without_rule_transition(
                                rule.label.clone(),
                                config,
                            ),
                        )
                }) {
                    // Direct output replacement happens from its associated
                    // rule before the generating-rule prerequisite. The proto
                    // generating_rule string is metadata, not an analyzed edge.
                    continue;
                }
                let to = ids
                    .get(&(rule.label.clone(), *config))
                    .ok_or("cquery output has no generating rule")?;
                nodes[*id as usize].edges.push(GraphEdge {
                    to: *to,
                    attribute: String::new(),
                    implicit: false,
                    tool: false,
                    aspect: false,
                });
            }
        }
        // ONLY_TARGET_DEPS compares endpoint configurations, not transition
        // kinds. In particular exec-labelled source files remain visible from
        // target-config parents, and exec parents retain only exec children.
        let mut tools = vec![false; nodes.len()];
        for ((label, config), id) in &ids {
            tools[*id as usize] = config.is_some_and(|config| {
                // Platforms and their prerequisites are analyzed in the empty
                // configuration. Native preserves their incoming ConfigIds;
                // that identity difference must not make them exec query deps.
                !self
                    .empty_configuration_rules
                    .contains(&ConfiguredTargetKey::new(label.clone(), config))
                    && self.configurations.get(config).native.is_exec
            });
        }
        for (id, node) in nodes.iter_mut().enumerate() {
            for edge in &mut node.edges {
                edge.tool = tools[id] != tools[edge.to as usize];
            }
        }
        let mut graph_patterns = BTreeMap::new();
        for (pattern, labels) in patterns {
            let labels: BTreeSet<_> = labels.into_iter().collect();
            graph_patterns.insert(
                pattern,
                ids.iter()
                    .filter(|((label, _), _)| labels.contains(label))
                    .map(|(_, id)| *id)
                    .collect(),
            );
        }
        // `--output=transitions` / `--show_config_fragments` facts.
        let mut transitions = BTreeMap::new();
        let mut fragments = BTreeMap::new();
        if self.cquery_transitions || self.cquery_fragments {
            let mut aspect_fragments: BTreeMap<u32, BTreeSet<String>> = BTreeMap::new();
            for (requester, applications) in &capture.requests {
                let from = aspect_targets[*requester as usize];
                for id in applications {
                    aspect_fragments.entry(from).or_default().extend(
                        super::cquery_presentation::aspect_fragments(
                            capture.applications[*id as usize]
                                .fragments
                                .as_deref()
                                .unwrap_or_default(),
                        ),
                    );
                }
            }
            for ((label, config), id) in &ids {
                let Ok(Target::Rule(rule)) = self.target(label) else {
                    continue;
                };
                if matches!(rule.kind, RuleKind::PackageGroup(_)) {
                    continue;
                }
                if self.cquery_transitions {
                    transitions.insert(*id, self.node_transitions(&rule));
                }
                if self.cquery_fragments {
                    let mut facts = self.node_fragments(&rule);
                    facts.none = config.is_none_or(|config| {
                        self.configurations.get(config).native.no_config
                            || self
                                .empty_configuration_rules
                                .contains(&ConfiguredTargetKey::new(label.clone(), config))
                    });
                    facts.aspects = aspect_fragments.remove(id).unwrap_or_default();
                    fragments.insert(*id, facts);
                }
            }
        }
        crate::perf_mark("cquery.graph_edges_ready");
        Ok(Graph {
            nodes,
            configurations,
            top_checksum: self.configurations.top().to_string(),
            patterns: graph_patterns,
            repository_mapping: self.repositories.mapping("")?.clone(),
            provider_sets,
            build_options,
            fact_errors,
            files_recorded: self.cquery_files,
            build_blocks,
            build_recorded: self.cquery_build,
            transitions,
            transitions_recorded: self.cquery_transitions,
            fragments,
            fragments_recorded: self.cquery_fragments,
        })
    }
}

/// A source artifact's exec path: main-repository files relative to the
/// exec root, external ones under `external/<canonical name>`.
fn source_exec_path(label: &Label) -> String {
    let relative = if label.package.is_empty() {
        label.name.clone()
    } else {
        format!("{}/{}", label.package, label.name)
    };
    if label.repository.is_empty() {
        relative
    } else {
        format!("external/{}/{relative}", label.repository)
    }
}

fn implementation_hash(
    label: &Label,
    graph: &crate::loading::LoadGraph,
    repositories: &RepositorySet,
    memo: &mut HashMap<Label, String>,
) -> Result<String, String> {
    use sha2::{Digest, Sha256};
    if let Some(hash) = memo.get(label) {
        return Ok(hash.clone());
    }
    let path = repositories
        .root(&label.repository)?
        .join(&label.package)
        .join(&label.name);
    let bytes = crate::observed_fs::read(&path)
        .map_err(|e| format!("cannot hash rule implementation {}: {e}", path.display()))?;
    let mut hash = Sha256::new();
    hash.update(b"bazel-rs-rule-implementation-v1\0");
    hash.update((bytes.len() as u64).to_le_bytes());
    hash.update(bytes);
    for load in graph.get(label).into_iter().flatten() {
        hash.update(implementation_hash(load, graph, repositories, memo)?.as_bytes());
    }
    let hash = format!("{:x}", hash.finalize());
    memo.insert(label.clone(), hash.clone());
    Ok(hash)
}

fn attribute_text(value: &wire::AttributeValue) -> Result<Option<String>, String> {
    use wire::AttributeValue as V;
    let list = |values: &[String]| format!("[{}]", values.join(", "));
    Ok(Some(match value {
        V::Integer(value) => value.to_string(),
        V::Boolean(value) => if *value { "1" } else { "0" }.into(),
        V::Tristate(value) => match value {
            wire::Tristate::No => "0",
            wire::Tristate::Yes => "1",
            wire::Tristate::Auto => "-1",
        }
        .into(),
        V::String(value) | V::Label(value) | V::Output(value) => value.clone(),
        V::StringList(values)
        | V::LabelList(values)
        | V::OutputList(values)
        | V::DistributionSet(values) => list(values),
        V::IntegerList(values) => list(&values.iter().map(ToString::to_string).collect::<Vec<_>>()),
        V::StringDict(entries) | V::LabelDictUnary(entries) | V::LabelKeyedStringDict(entries) => {
            format!(
                "{{{}}}",
                entries
                    .iter()
                    .map(|(k, v)| format!("{k}={v}"))
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        }
        V::LabelListDict(entries) | V::StringListDict(entries) => format!(
            "{{{}}}",
            entries
                .iter()
                .map(|(k, v)| format!("{k}={}", list(v)))
                .collect::<Vec<_>>()
                .join(", ")
        ),
        V::Unset(_) => return Ok(None),
        V::License(_) => {
            return Err("attr() on license values is not implemented for cquery".into());
        }
    }))
}

fn configured_attribute(
    attribute: query_attrs::QueryAttribute,
    rule: &Rule,
) -> Result<wire::Attribute, String> {
    use crate::custom_rule::AttrValue as A;
    use query_attrs::Shape;
    use wire::{AttributeType as T, AttributeValue as V};
    if attribute.select.is_some() {
        return Err(format!(
            "cquery unresolved attribute {} of {}",
            attribute.name, rule.label
        ));
    }
    let mut value = attribute.value;
    // query_attrs models loading defaults; use the actual configured value of
    // late-bound Starlark attributes instead of the loading-time annotation.
    if let Some(custom) = &rule.custom
        && !custom
            .declaration
            .configurable
            .contains_key(&attribute.public_name)
        && let Some(actual) = custom.attrs.get(&attribute.public_name)
    {
        value = Some(actual.clone());
    }
    let value = match value {
        Some(A::Strings(s)) if attribute.name == "licenses" => {
            let mut license = wire::License::default();
            for entry in s {
                if let Some(exception) = entry.strip_prefix("exception=") {
                    license.exceptions.push(exception.to_owned());
                } else {
                    license.license_types.push(entry.to_ascii_uppercase());
                }
            }
            if license.license_types.is_empty() {
                license.license_types.push("NONE".into());
            }
            V::License(license)
        }
        Some(A::String(s)) => V::String(s),
        Some(A::Strings(s)) => V::StringList(s),
        Some(A::StringDict(s)) => V::StringDict(s),
        Some(A::StringListDict(s)) => V::StringListDict(s),
        Some(A::Int(_))
            if attribute.name == "stamp"
                && matches!(rule.kind, RuleKind::Genrule)
                && !attribute.explicit =>
        {
            V::Tristate(wire::Tristate::No)
        }
        Some(A::Int(i)) if attribute.shape == Shape::TriState => V::Tristate(match i {
            -1 => wire::Tristate::Auto,
            0 => wire::Tristate::No,
            1 => wire::Tristate::Yes,
            _ => return Err("invalid tristate in cquery".into()),
        }),
        Some(A::Int(i)) => V::Integer(i),
        Some(A::Ints(i)) => V::IntegerList(i),
        Some(A::Bool(b)) => V::Boolean(b),
        Some(A::Label(Some(s))) if attribute.shape == Shape::Nodep => V::String(s),
        Some(A::Labels(s)) if attribute.shape == Shape::Nodep => V::StringList(s),
        Some(A::Label(Some(s))) => V::Label(s),
        Some(A::Label(None)) => V::Unset(T::Label),
        Some(A::Labels(s)) => V::LabelList(s),
        Some(A::Output(Some(s))) => V::Output(rule.label.with_name(&s)?.to_string()),
        Some(A::Output(None)) => V::Unset(T::Output),
        Some(A::Outputs(s)) => V::OutputList(
            s.iter()
                .map(|s| rule.label.with_name(s).map(|l| l.to_string()))
                .collect::<Result<_, _>>()?,
        ),
        Some(A::LabelKeyedStringDict(s)) => V::LabelKeyedStringDict(s),
        Some(A::StringKeyedLabelDict(s)) => V::LabelDictUnary(s),
        Some(A::LabelListDict(s)) => V::LabelListDict(s),
        None => V::Unset(match attribute.shape {
            Shape::Label | Shape::Nodep => T::Label,
            _ => T::String,
        }),
    };
    let mut result = wire::Attribute::new(attribute.name, value, attribute.explicit);
    if attribute.shape == Shape::Nodep {
        result.nodep = Some(true);
    }
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn capture_shares_keys_and_names_but_keeps_transition_identity() {
        let label = Label::parse_canonical("//pkg:target").unwrap();
        let configuration = crate::configuration::Configuration::default();
        let key = ConfiguredTargetKey::new(label.clone(), configuration.id);
        let incoming =
            ConfiguredTargetKey::without_rule_transition(label.clone(), configuration.id);
        let mut capture = Capture::default();
        let id = capture.target(key.clone());
        for _ in 0..1000 {
            assert_eq!(id, capture.target(key.clone()));
        }
        assert_ne!(id, capture.target(incoming));
        let mut native = configuration.native;
        native.cpu = Some("other".into());
        let other = crate::configuration::Configuration::new(native, BTreeMap::new());
        assert_ne!(
            id,
            capture.target(ConfiguredTargetKey::new(label, other.id))
        );
        assert_eq!(capture.target_keys.len(), 3);
        let attr = capture.string("deps");
        assert!(Arc::ptr_eq(&attr, &capture.string("deps")));
        assert!(!Arc::ptr_eq(&attr, &capture.string("tools")));
    }
}
