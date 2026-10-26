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
    edges: Vec<CapturedEdge>,
}

struct CapturedEdge {
    from: ConfiguredTargetKey,
    to: ConfiguredTargetKey,
    attribute: String,
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
}

#[derive(Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Node {
    pub target: wire::Target,
    pub configuration: Option<u32>,
    pub edges: Vec<GraphEdge>,
}

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
}

impl Default for Options {
    fn default() -> Self {
        Self {
            common: Default::default(),
            universe: Vec::new(),
            include_aspects: true,
            writer: Default::default(),
        }
    }
}

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
                "universe_scope" => self.universe = value.split(',').filter(|s| !s.is_empty()).map(str::to_owned).collect(),
                "transitions" => self.writer.transitions = match value { "none" => wire::Transitions::None, "lite" => wire::Transitions::Lite, "full" => return Err("cquery --transitions=full is not implemented (use lite)".into()), _ => return Err(format!("invalid --transitions={value}")) },
                _ => return Err("Starlark cquery output is not implemented; target-determinator must use -filter-incompatible-targets=false".into()),
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
        if !matches!(
            self.common.output.as_str(),
            "label" | "label_kind" | "proto" | "streamed_proto"
        ) {
            return Err(format!(
                "cquery output {:?} is not implemented; supported: label, label_kind, proto, streamed_proto (Starlark filtering is not implemented)",
                self.common.output
            ));
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

    fn evaluate(
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
                                _ => match &node.target {
                                    wire::Target::Rule(rule) => rule
                                        .attributes
                                        .iter()
                                        .find(|a| a.name == word(0))
                                        .map(|a| format!("{:?}", a.value))
                                        .unwrap_or_default(),
                                    _ => String::new(),
                                },
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
        if matches!(options.common.output.as_str(), "label" | "label_kind") {
            let mut ids: Vec<_> = ids.into_iter().collect();
            ids.sort_by(|a, b| {
                self.nodes[*a as usize]
                    .label()
                    .cmp(self.nodes[*b as usize].label())
                    .then(a.cmp(b))
            });
            for id in ids {
                let node = &self.nodes[id as usize];
                if options.common.output == "label_kind" {
                    write!(destination, "{} ", node.kind()).map_err(|e| e.to_string())?;
                }
                let checksum = self.checksum(node);
                writeln!(
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
            }
            return destination.flush().map_err(|e| e.to_string());
        }
        let format = if options.common.output == "proto" {
            wire::OutputFormat::Proto
        } else {
            wire::OutputFormat::StreamedProto
        };
        let mut writer = wire::CqueryWriter::new(destination, format, options.writer);
        for id in ids {
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
                        rule.rule_inputs.push(target.label().to_owned());
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
pub(super) fn register_outputs(
    files: &[File],
    config: ConfigId,
    index: &mut BTreeMap<File, Artifact>,
    declared: &mut Declared,
) {
    for file in files {
        let path = PathBuf::from(&file.path);
        let mut artifact = Artifact::new(
            path.clone(),
            path.clone(),
            None,
            file.owner.clone(),
            file.is_directory,
            false,
        );
        artifact.make_mut().file = file.clone();
        register_artifact(index, file, &artifact);
        declared.insert((config, path), artifact);
    }
}

impl Engine {
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
            capture.edges.push(CapturedEdge {
                from: from.clone(),
                to: ConfiguredTargetKey::new(label, config),
                attribute: attribute.to_owned(),
                tool,
                aspect,
            });
        }
    }

    /// Full configured/provider analysis, never a PlannedRequest. This is not
    /// --nobuild: that still lowers executable actions and may run status.
    pub fn cquery_graph(
        &mut self,
        patterns: &[String],
        relative_directory: &str,
    ) -> Result<Graph, String> {
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
        self.cquery_capture = Some(Capture::default());
        let execute = self.execute_build;
        self.execute_build = false;
        let analyzed = self.plan_build(&roots);
        self.execute_build = execute;
        let capture = self.cquery_capture.take().expect("capture installed");
        analyzed?;
        let graph = self.finish_cquery_graph(capture, &roots, expanded)?;
        self.record_link_anchors();
        Ok(graph)
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
        capture: Capture,
        roots: &[Label],
        patterns: BTreeMap<String, Vec<Label>>,
    ) -> Result<Graph, String> {
        let mut ids: BTreeMap<(Label, Option<ConfigId>), u32> = BTreeMap::new();
        let mut intern = |key| -> Result<(), String> {
            if !ids.contains_key(&key) {
                let id = u32::try_from(ids.len())
                    .map_err(|_| "cquery graph exceeds u32 node capacity")?;
                ids.insert(key, id);
            }
            Ok(())
        };
        for key in capture.rules.keys() {
            intern(self.cquery_key(key)?)?;
        }
        for edge in &capture.edges {
            intern(self.cquery_key(&edge.to)?)?;
        }
        for label in roots {
            intern(self.cquery_key(&self.configurations.top_key(label.clone()))?)?;
        }
        let mut configurations = Vec::new();
        let mut config_ids = BTreeMap::new();
        for (id, configuration) in self.configurations.iter() {
            let index = configurations.len() as u32;
            config_ids.insert(id, index);
            let options = crate::config_registry::native_option_strings(&configuration.native)
                .into_iter()
                .map(|(name, value)| wire::ConfigurationOption {
                    name: Some(name.to_owned()),
                    value: Some(value.strip_prefix('=').unwrap_or(&value).to_owned()),
                })
                .collect();
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
        let mut hashes = HashMap::new();
        let mut conditions: BTreeMap<&ConfiguredTargetKey, Vec<String>> = BTreeMap::new();
        for edge in &capture.edges {
            if edge.attribute == "$config_dependencies" {
                conditions
                    .entry(&edge.from)
                    .or_default()
                    .push(edge.to.label.to_string());
            }
        }
        for ((label, config), _) in ordered {
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
                Target::Rule(_) => {
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
                    let mut attributes = query_attrs::rule_attributes(rule, package)
                        .into_iter()
                        .map(|attribute| configured_attribute(attribute, rule))
                        .collect::<Result<Vec<_>, _>>()?;
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
                        attributes.push(wire::Attribute::new(
                            ":flag_alias_settings",
                            wire::AttributeValue::LabelList(Vec::new()),
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
                        rule_outputs: rule
                            .outs
                            .iter()
                            .map(|out| label.with_name(out).map(|l| l.to_string()))
                            .collect::<Result<_, _>>()?,
                        ..Default::default()
                    })
                }
            };
            nodes.push(Node {
                target,
                configuration: config.map(|id| config_ids[&id]),
                edges: Vec::new(),
            });
        }
        for edge in capture.edges {
            let from_key = self.cquery_key(&edge.from)?;
            let to_key = self.cquery_key(&edge.to)?;
            let (Some(&from), Some(&to)) = (ids.get(&from_key), ids.get(&to_key)) else {
                return Err("cquery edge has no analyzed endpoint".into());
            };
            let implicit = match &nodes[from as usize].target {
                wire::Target::Rule(rule) => !rule.attributes.iter().any(|a| {
                    (a.name == edge.attribute
                        || a.name.strip_prefix('$').map(|s| format!("_{s}"))
                            == Some(edge.attribute.clone()))
                        && a.explicitly_specified == Some(true)
                }),
                _ => false,
            };
            nodes[from as usize].edges.push(GraphEdge {
                to,
                attribute: edge.attribute,
                implicit,
                tool: edge.tool,
                aspect: edge.aspect,
            });
        }
        // An output file points to its generating rule in the same configured
        // identity. It never impersonates that rule or drops its owner edge.
        for ((label, config), id) in &ids {
            if let Target::Output(rule, _) = self.target(label)? {
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
        Ok(Graph {
            nodes,
            configurations,
            top_checksum: self.configurations.top().to_string(),
            patterns: graph_patterns,
            repository_mapping: self.repositories.mapping("")?.clone(),
        })
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
