//! The engine side of `aquery` (crate::aquery, docs/AQUERY.md): the
//! configured graph of the universe (crate::engine::cquery's capture) with
//! action planning enabled, and a description of every planned action of
//! its configured targets and aspect applications. Nothing executes: no
//! workspace status, no input digests, no scheduler.
use super::*;
use crate::aquery::{ActionRecord, Aspect, Dump, Meta};
use crate::aquery_key::{CommandLinePart, KeyKind, KeyPlatform};

/// The owner of each action id prefix, and the planned actions.
#[derive(Default)]
pub(super) struct Capture {
    owners: HashMap<String, Owner>,
    nodes: Vec<PlannedAction>,
    top_level: Vec<TopLevel>,
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

struct Owner {
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
}

impl Analysis {
    /// The requested configured targets (crate::print_action).
    pub fn top_level(&self) -> &[TopLevel] {
        &self.top_level
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
        }
    }
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
                Owner {
                    key: key.clone(),
                    aspects: aspects(),
                },
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
    ) -> Result<Analysis, String> {
        crate::aquery::set_capturing(true);
        self.aquery_capture = Some(Capture::default());
        let graph = self.cquery_graph(patterns, relative_directory);
        crate::aquery::set_capturing(false);
        let capture = self.aquery_capture.take().unwrap_or_default();
        let graph = graph?;
        let mut nodes_by_key: HashMap<(String, String), u32> = HashMap::new();
        for (id, node) in graph.nodes.iter().enumerate() {
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
        // Label.getDisplayForm(main repository mapping): the text output's
        // `Target:` spelling (LabelPrinter.displayForm).
        let mut apparent: BTreeMap<&str, &str> = BTreeMap::new();
        for (name, canonical) in &graph.repository_mapping {
            let shorter = apparent
                .get(canonical.as_str())
                .is_none_or(|known| name.len() < known.len());
            if shorter {
                apparent.insert(canonical.as_str(), name.as_str());
            }
        }
        let display = |label: &Label| -> String {
            if label.repository.is_empty() {
                return format!("//{}:{}", label.package, label.name);
            }
            match apparent.get(label.repository.as_str()) {
                Some(name) if !name.is_empty() => {
                    format!("@{name}//{}:{}", label.package, label.name)
                }
                _ => label.to_string(),
            }
        };
        let mut configurations = Vec::new();
        let mut configuration_index: BTreeMap<ConfigId, usize> = BTreeMap::new();
        let mut actions = Vec::new();
        for node in &capture.nodes {
            let id = node.action.id.as_str();
            let owner = capture.owners.get(id).or_else(|| {
                id.rsplit_once('#')
                    .and_then(|(prefix, _)| capture.owners.get(prefix))
            });
            let Some(owner) = owner else {
                continue;
            };
            let config = owner.key.config;
            let Some(&graph_node) =
                nodes_by_key.get(&(owner.key.label.to_string(), config.to_string()))
            else {
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
            let display_target = display(&owner.key.label);
            for mut record in
                self.aquery_records(node, owner, rule_class, configuration, &configurations)
            {
                record.display_target = display_target.clone();
                record.display_execution_platform = record
                    .execution_platform
                    .as_deref()
                    .and_then(|platform| Label::parse_canonical(platform).ok())
                    .map(|platform| display(&platform));
                actions.push((graph_node, record));
            }
        }
        Ok(Analysis {
            graph,
            configurations,
            actions,
            top_level: capture.top_level.clone(),
        })
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

    pub(super) fn aquery_platform_info(
        &self,
        label: &Label,
    ) -> Option<Arc<crate::platforms::PlatformInfo>> {
        self.platforms
            .iter()
            .filter(|(key, _)| key.label == *label)
            .find_map(|(_, info)| info.as_ref().ok().cloned())
    }

    fn aquery_records(
        &self,
        node: &PlannedAction,
        owner: &Owner,
        rule_class: Option<String>,
        configuration: usize,
        configurations: &[crate::aquery::Configuration],
    ) -> Vec<ActionRecord> {
        let action = &node.action;
        let meta: Meta = action.extra.aquery.as_deref().cloned().unwrap_or_default();
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
            self.aquery_platform_info(&platform_label)
        };
        // An alias (`@bazel_tools//tools:host_platform`) prints as the
        // platform it resolves to (PlatformInfo.label).
        let platform_label = platform
            .as_ref()
            .map_or(platform_label, |info| info.label.clone());
        let mut inputs: Vec<String> = Vec::new();
        for (_, destination) in &action.inputs {
            inputs.push(destination.to_string_lossy().into_owned());
        }
        for dependency in &node.dependencies {
            inputs.push(dependency.destination.to_string_lossy().into_owned());
        }
        for (path, _) in &action.embedded_inputs {
            inputs.push(path.to_string_lossy().into_owned());
        }
        for group in &node.groups {
            inputs.push(group.root.to_string_lossy().into_owned());
        }
        let inputs = crate::aquery::dedup(inputs);
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
        let primary_input = meta
            .input_order
            .as_ref()
            .and_then(|order| order.first())
            .or(inputs.first())
            .map(String::as_str);
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
            inputs: inputs.clone(),
            input_order: meta.input_order.clone(),
            outputs: outputs.clone(),
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
            let content = action
                .inputs
                .iter()
                .find(|(_, destination)| destination.as_os_str() == template.as_os_str())
                // snapshot-unrecorded: aquery's description of a template's text;
                // aquery never saves or replays an analysis snapshot.
                .and_then(|(source, _)| std::fs::read(&**source).ok())
                .map(|bytes| internal_string(&bytes))
                .unwrap_or_else(|| format!("ARTIFACT: {exec}"));
            (exec, content)
        };
        match (&action.write, &action.extra.transform) {
            (Some(write), _) => {
                if let Some(param) = &meta.param_write {
                    record.mnemonic = "FileWrite".into();
                    record.param_file = Some(param.arguments.clone());
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
                record.action_key = self.aquery_native_key(&record, key_platform, &properties);
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
                record.inputs.clear();
                record.file_write = Some((false, String::new()));
                record.action_key = self.aquery_native_key(&record, key_platform, &properties);
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
                record.inputs = vec![manifest_input];
                record.environment = self
                    .configurations
                    .get(owner.key.config)
                    .native
                    .action_environment()
                    .fixed
                    .into_iter()
                    .collect();
                record.action_key = self.aquery_native_key(&record, key_platform, &properties);
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
                record.action_key = self.aquery_native_key(&record, key_platform, &properties);
            }
            (None, None) => {
                record.mnemonic = action.extra.mnemonic.clone();
                let arguments = meta
                    .arguments
                    .clone()
                    .unwrap_or_else(|| crate::action::arguments(action));
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
                record.environment = meta.environment.clone().unwrap_or_else(|| {
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
                let lines = meta
                    .command_lines
                    .clone()
                    .unwrap_or_else(|| vec![CommandLinePart::Flat(arguments.clone())]);
                let inherited: Vec<String> =
                    action.extra.inherited_environment.iter().cloned().collect();
                let spawn = KeyKind::Spawn {
                    command_lines: &lines,
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
                if record.discovers_inputs {
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
        platform: KeyPlatform<'_>,
        properties: &[(String, String)],
    ) -> String {
        let mut description = vec![record.mnemonic.clone()];
        description.extend(record.inputs.iter().cloned());
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
