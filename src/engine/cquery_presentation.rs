//! cquery's `--output=transitions` (TransitionsOutputFormatterCallback,
//! CqueryTransitionResolver) and `--show_config_fragments`
//! (RequiredFragmentsUtil, LabelAndConfigurationOutputFormatterCallback)
//! facts (docs/CQUERY_OUTPUT.md). They are recorded into the configured graph
//! only when the command asks for them (Engine::set_cquery_transitions,
//! Engine::set_cquery_fragments; each keys its own analysis), so the other
//! formats' graphs carry nothing extra.
//!
//! Pinned source: `/tmp/opencode/bazel-9.3.1-reference` (Release 9.3.1);
//! paths relative to `src/main/java/com/google/devtools/build/lib/`.
use super::cquery::{Graph, Node};
use super::*;
use crate::cquery_proto as wire;
use serde::{Deserialize, Serialize};
use std::io::Write;

/// One rule node's transition facts (CqueryTransitionResolver).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct NodeTransitions {
    /// getRuleClassTransition: the rule class's own transition name.
    pub class: String,
    /// getRuleTransition: the rule class transition composed with the
    /// trimming transitions, the name a parent's edge to this node shows.
    pub rule: String,
    /// The attribute transitions by attribute (native) name: `exec`,
    /// `Starlark transition:LOCATION`, `NoConfigTransition`.
    pub attributes: BTreeMap<String, String>,
}

/// One node's directly required configuration (RequiredConfigFragmentsProvider
/// before the merges RequiredFragmentsUtil.getRequiredFragments makes).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct NodeFragments {
    /// Options/fragment class names, `--define:NAME` and Starlark option labels.
    pub own: BTreeSet<String>,
    /// A build setting (BuildSettingProvider): its own label.
    pub setting: Option<String>,
    /// The fragments declared by the aspects this node requests on its
    /// dependencies (merged into those dependencies' providers).
    pub aspects: BTreeSet<String>,
    /// No RequiredConfigFragmentsProvider (source files, incompatible
    /// targets, the empty configuration of platforms and constraints).
    pub none: bool,
}

/// The universally required fragments (BazelRuleClassProvider.java:235-237)
/// with the options every rule requires, as genuine prints them.
const BASE: [&str; 5] = [
    "BazelRuleClassProvider$StrictActionEnvConfiguration",
    "CoreOptions",
    "PlatformConfiguration",
    "PlatformOptions",
    "ShellConfiguration",
];

/// `rule(fragments=[...])` names to the registered fragment classes
/// (BuildConfigurationValue.getStarlarkFragmentByName; unregistered names,
/// `py` and `bazel_py` included in Bazel 9.3.1's captures, are dropped).
fn starlark_fragment(name: &str) -> Option<&'static str> {
    Some(match name {
        "android" => "AndroidConfiguration",
        "apple" => "AppleConfiguration",
        "bazel_android" => "BazelAndroidConfiguration",
        "coverage" => "CoverageConfiguration",
        "cpp" => "CppConfiguration",
        "j2objc" => "J2ObjcConfiguration",
        "java" => "JavaConfiguration",
        "objc" => "ObjcConfiguration",
        "platform" => "PlatformConfiguration",
        "proto" => "ProtoConfiguration",
        _ => return None,
    })
}

/// The FragmentOptions class (ClassName.getSimpleNameWithOuter) defining a
/// native option, from its defining file (crate::bazel_options).
fn options_class(option: &str) -> Option<&'static str> {
    let (_, _, _, source) = crate::bazel_options::FRAGMENT_OPTIONS
        .iter()
        .find(|(name, old, _, _)| *name == option || (!old.is_empty() && *old == option))?;
    let file = source.split(':').next()?;
    let stem = file.rsplit('/').next()?.strip_suffix(".java")?;
    Some(match stem {
        "AndroidConfiguration" => "AndroidConfiguration$Options",
        "BazelAndroidConfiguration" => "BazelAndroidConfiguration$Options",
        "BazelPythonConfiguration" => "BazelPythonConfiguration$Options",
        "BazelRuleClassProvider" => "BazelRuleClassProvider$StrictActionEnvOptions",
        "CoverageConfiguration" => "CoverageConfiguration$CoverageOptions",
        "ProtoConfiguration" => "ProtoConfiguration$Options",
        "ShellConfiguration" => "ShellConfiguration$Options",
        "TestConfiguration" => "TestConfiguration$TestOptions",
        "AppleCommandLineOptions" => "AppleCommandLineOptions",
        "ConfigFeatureFlagOptions" => "ConfigFeatureFlagOptions",
        "CoreOptions" => "CoreOptions",
        "CppOptions" => "CppOptions",
        "J2ObjcCommandLineOptions" => "J2ObjcCommandLineOptions",
        "JavaOptions" => "JavaOptions",
        "ObjcCommandLineOptions" => "ObjcCommandLineOptions",
        "PlatformOptions" => "PlatformOptions",
        "PythonOptions" => "PythonOptions",
        _ => return None,
    })
}

/// The fully qualified FragmentOptions class of a native option (the
/// BuildOptions fragment order OptionsDiff iterates), from its file.
fn options_class_key(option: &str) -> Option<String> {
    let (_, _, _, source) = crate::bazel_options::FRAGMENT_OPTIONS
        .iter()
        .find(|(name, _, _, _)| *name == option)?;
    let file = source.split(':').next()?.strip_suffix(".java")?;
    Some(format!(
        "com.google.devtools.build.lib.{}",
        file.replace('/', ".")
    ))
}

/// StarlarkTransition.addRequiredFragments: each native option's options
/// class, each Starlark setting's label.
fn transition_requirements(settings: &[String], out: &mut BTreeSet<String>) {
    for setting in settings {
        match setting.strip_prefix("//command_line_option:") {
            Some(option) => {
                if let Some(class) = options_class(option) {
                    out.insert(class.to_owned());
                }
            }
            None => {
                out.insert(setting.clone());
            }
        }
    }
}

/// AliasProvider.mayBeAlias: alias, label_flag and label_setting (rule
/// classes that can have any provider, Alias.java:79, LabelBuildSettings.java:98).
fn may_be_alias(rule: &Rule) -> bool {
    matches!(
        rule.rule_class_name(),
        "alias" | "label_flag" | "label_setting"
    )
}

/// The option names of TestConfiguration.TestOptions (TEST_OPTIONS of
/// TestTrimmingTransitionFactory.java:43-44).
fn test_option(name: &str) -> bool {
    crate::bazel_options::FRAGMENT_OPTIONS
        .iter()
        .any(|(option, _, _, source)| {
            *option == name && source.starts_with("analysis/test/TestConfiguration.java")
        })
}

impl Engine {
    /// The absolute `path:line:column` of a `LABEL:LINE:COLUMN` location.
    fn absolute_location(&self, location: &str) -> String {
        let mut parts = location.rsplitn(3, ':');
        let (Some(column), Some(line), Some(label)) = (parts.next(), parts.next(), parts.next())
        else {
            return location.to_owned();
        };
        let Ok(label) = Label::parse_canonical(label) else {
            return location.to_owned();
        };
        match self.repositories.root(&label.repository) {
            Ok(root) => format!(
                "{}:{line}:{column}",
                root.join(&label.package).join(&label.name).display()
            ),
            Err(_) => location.to_owned(),
        }
    }

    /// The retained Starlark transition of a rule (`""`) or attribute.
    fn starlark_transition(
        &self,
        rule: &Rule,
        key: &str,
    ) -> Option<(String, Vec<String>, Vec<String>)> {
        let custom = rule.custom.as_ref()?;
        let value = custom.transitions.get(key)?;
        let transition = crate::rule_declarations::Transition::from_value(value.value())?;
        Some((
            format!(
                "Starlark transition:{}",
                self.absolute_location(&transition.location)
            ),
            transition.canonical_inputs.clone(),
            transition.canonical_outputs.clone(),
        ))
    }

    /// TestTrimmingTransitionFactory.create (:93-135) and
    /// ConfigFeatureFlagTaggedTrimmingTransitionFactory.create (:110-153)
    /// names, composed (ComposingTransitionFactory.create: always a
    /// ComposingTransition of the two created transitions).
    fn trimming_name(&self, rule: &Rule) -> String {
        let test_rule = rule
            .custom
            .as_ref()
            .is_some_and(|custom| custom.declaration.builtin.kind.test)
            || rule.rule_class_name() == "test_suite";
        let tagged_skip = matches!(
            rule.custom.as_ref().and_then(|custom| custom.attrs.get("transitive_configs")),
            Some(crate::custom_rule::AttrValue::Labels(labels))
                if labels.iter().any(|label| label == "//command_line_option/fragment:test")
        );
        let test_config_setting = rule.rule_class_name() == "config_setting"
            && matches!(
                rule.native.as_ref().and_then(|native| native.attrs.get("values")),
                Some(crate::custom_rule::AttrValue::StringDict(values))
                    if values.iter().any(|(name, _)| test_option(name))
            );
        let test = if test_rule || may_be_alias(rule) || tagged_skip || test_config_setting {
            "NoTransition"
        } else {
            "TestTrimmingTransition"
        };
        let tagged = if may_be_alias(rule) {
            "NoTransition"
        } else {
            "ConfigFeatureFlagTaggedTrimmingTransition"
        };
        format!("({test} + {tagged})")
    }

    /// The rule class's transition name (RuleClass.getTransitionFactory().create().getName()).
    fn class_transition_name(&self, rule: &Rule) -> String {
        if let Some((name, _, _)) = self.starlark_transition(rule, "") {
            return name;
        }
        if matches!(
            rule.rule_class_name(),
            "constraint_setting" | "constraint_value"
        ) {
            return "NoConfigTransition".to_owned();
        }
        "NoTransition".to_owned()
    }

    /// The transition facts of a rule node (`--output=transitions`).
    pub(super) fn node_transitions(&self, rule: &Rule) -> NodeTransitions {
        let class = self.class_transition_name(rule);
        // CqueryTransitionResolver.getRuleTransition (:252-270): the rule
        // class transition composed with the trimming transitions, except
        // for a rule NAMED `alias` (rule.getAssociatedRule().getName()).
        let rule_name = if rule.label.name == "alias" {
            class.clone()
        } else if class == "NoTransition" {
            self.trimming_name(rule)
        } else {
            format!("({class} + {})", self.trimming_name(rule))
        };
        let mut attributes = BTreeMap::new();
        if let Some(custom) = &rule.custom {
            for (name, spec) in custom.schema.iter() {
                let native = name.clone();
                let transition = match &spec.declaration.cfg {
                    crate::rule_declarations::Cfg::Exec(_) => Some("exec".to_owned()),
                    crate::rule_declarations::Cfg::NoConfig => {
                        Some("NoConfigTransition".to_owned())
                    }
                    crate::rule_declarations::Cfg::Starlark { .. } => self
                        .starlark_transition(rule, name)
                        .map(|(name, _, _)| name),
                    crate::rule_declarations::Cfg::Target => None,
                };
                if let Some(transition) = transition {
                    attributes.insert(native, transition);
                }
            }
        }
        NodeTransitions {
            class,
            rule: rule_name,
            attributes,
        }
    }

    /// A rule node's own required fragments (RequiredFragmentsUtil
    /// .getRuleRequiredFragmentsIfEnabled without the merges).
    pub(super) fn node_fragments(&self, rule: &Rule) -> NodeFragments {
        let mut own: BTreeSet<String> = BASE.iter().map(|name| (*name).to_owned()).collect();
        let mut setting = None;
        if let Some(custom) = &rule.custom {
            for fragment in &custom.declaration.fragments {
                if let Some(class) = starlark_fragment(fragment) {
                    own.insert(class.to_owned());
                }
            }
            // Transitions attached to the rule and its attributes
            // (addRequiredFragmentsFromRuleTransitions).
            for key in custom.transitions.keys() {
                if let Some((_, inputs, outputs)) = self.starlark_transition(rule, key) {
                    transition_requirements(&inputs, &mut own);
                    transition_requirements(&outputs, &mut own);
                }
            }
            if custom.build_setting.is_some() {
                setting = Some(rule.label.to_string());
            }
        }
        match rule.rule_class_name() {
            // test_suite's ConfigurationFragmentPolicy requires TestConfiguration.
            "test_suite" => {
                own.insert("TestConfiguration".to_owned());
            }
            "label_flag" | "label_setting" => setting = Some(rule.label.to_string()),
            // ConfigSetting.addRuleImplSpecificRequiredConfigFragments
            // (rules/config/ConfigSetting.java): the options `values`
            // reads, `--define`s, and the `flag_values` settings.
            "config_setting" => {
                let native = rule.native.as_ref();
                let dict = |name: &str| match native.and_then(|native| native.attrs.get(name)) {
                    Some(crate::custom_rule::AttrValue::StringDict(values)) => values.clone(),
                    Some(crate::custom_rule::AttrValue::LabelKeyedStringDict(values)) => {
                        values.clone()
                    }
                    _ => Vec::new(),
                };
                for (name, value) in dict("values") {
                    if name == "define" {
                        let define = value.split_once('=').map_or(value.as_str(), |(n, _)| n);
                        own.insert(format!("--define:{define}"));
                    } else if let Some(class) = options_class(&name) {
                        own.insert(class.to_owned());
                    }
                }
                for (name, _) in dict("define_values") {
                    own.insert(format!("--define:{name}"));
                }
                for (flag, _) in dict("flag_values") {
                    let label = Label::parse_canonical(&flag)
                        .or_else(|_| {
                            self.repositories.parse(
                                &flag,
                                &rule.label.repository,
                                &rule.label.package,
                            )
                        })
                        .map(|label| label.to_string())
                        .unwrap_or(flag);
                    own.insert(label);
                }
            }
            _ => {}
        }
        if let Some(setting) = &setting {
            own.insert(setting.clone());
        }
        NodeFragments {
            own,
            setting,
            aspects: BTreeSet::new(),
            none: false,
        }
    }
}

/// The fragments of an aspect declaration (its `fragments`).
pub(super) fn aspect_fragments(fragments: &[String]) -> BTreeSet<String> {
    fragments
        .iter()
        .filter_map(|name| starlark_fragment(name))
        .map(str::to_owned)
        .collect()
}

/// Java `Object.toString` of a native option value as OptionsDiff prints it.
fn java_value(value: &crate::cquery_starlark::BuildOption) -> String {
    use crate::cquery_starlark::BuildOption as V;
    match value {
        V::None => "null".to_owned(),
        V::Bool(value) => value.to_string(),
        V::Int(value) => value.to_string(),
        V::String(value) | V::Label(value) | V::Enum { value, .. } => value.clone(),
        V::Strings(items) | V::Labels(items) => format!("[{}]", items.join(", ")),
    }
}

impl Graph {
    fn short_id(&self, node: &Node) -> String {
        match node.configuration {
            None => "null".to_owned(),
            Some(id) => self.configurations[id as usize].checksum[..7].to_owned(),
        }
    }

    /// Label.getDisplayForm through the main repository mapping.
    pub(super) fn display_label(&self, label: &str) -> String {
        let Ok(parsed) = Label::parse_canonical(label) else {
            return label.to_owned();
        };
        if parsed.repository.is_empty() {
            return label.to_owned();
        }
        let apparent = self
            .repository_mapping
            .iter()
            .filter(|(_, canonical)| **canonical == parsed.repository)
            .map(|(name, _)| name.as_str())
            .min_by_key(|name| name.len());
        match apparent {
            Some(name) if !name.is_empty() => {
                format!("@{name}//{}:{}", parsed.package, parsed.name)
            }
            _ => label.to_owned(),
        }
    }

    /// OptionsDiff.getPrettyPrintList (analysis/config/OptionsDiff.java
    /// :254-262) between two configurations: the native options that
    /// differ, by options class then name, the second value as a list.
    fn options_diff(&self, from: u32, to: &[u32]) -> Vec<String> {
        let options = |id: u32| -> BTreeMap<(String, String), String> {
            let configuration = &self.configurations[id as usize];
            let mut values = BTreeMap::new();
            for (name, value) in self.build_options.get(id as usize).into_iter().flatten() {
                let Some(option) = name.strip_prefix("//command_line_option:") else {
                    continue;
                };
                let Some(class) = options_class_key(option) else {
                    continue;
                };
                values.insert((class, option.to_owned()), java_value(value));
            }
            // Options the native profile does not list but the exec
            // transition writes (builtin_exec_platforms.bzl): `is exec
            // configuration`, `platform_suffix` and the default `--jvmopt`
            // (host_jvmopt; `--jvmopt` is not a native option, so its
            // target value is the default).
            let core = "com.google.devtools.build.lib.analysis.config.CoreOptions".to_owned();
            values.insert(
                (core.clone(), "is exec configuration".to_owned()),
                configuration.is_tool.to_string(),
            );
            values.insert(
                (core, "platform_suffix".to_owned()),
                if configuration.is_tool {
                    "exec".to_owned()
                } else {
                    "null".to_owned()
                },
            );
            values.insert(
                (
                    "com.google.devtools.build.lib.rules.java.JavaOptions".to_owned(),
                    "jvmopt".to_owned(),
                ),
                if configuration.is_tool {
                    "[-XX:ErrorFile=/dev/stderr]".to_owned()
                } else {
                    "[]".to_owned()
                },
            );
            values
        };
        let first = options(from);
        let seconds: Vec<_> = to.iter().map(|id| options(*id)).collect();
        let mut lines = Vec::new();
        for (key, value) in &first {
            let others: Vec<&String> = seconds
                .iter()
                .filter_map(|second| second.get(key))
                .filter(|other| *other != value)
                .collect();
            if others.is_empty() {
                continue;
            }
            lines.push(format!(
                "{}:{value} -> [{}]",
                key.1,
                others
                    .iter()
                    .map(|value| value.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            ));
        }
        lines
    }

    /// TransitionsOutputFormatterCallback.processOutput.
    pub(super) fn write_transitions(
        &self,
        ids: &[u32],
        full: bool,
        destination: &mut impl Write,
    ) -> Result<(), String> {
        if !self.transitions_recorded {
            return Err("internal error: this configured graph records no transitions".into());
        }
        let io = |error: std::io::Error| error.to_string();
        for &id in ids {
            let node = &self.nodes[id as usize];
            let facts = self.transitions.get(&id);
            let class = match (&node.target, facts) {
                (wire::Target::Rule(_), Some(facts)) => format!("{} -> ", facts.class),
                (wire::Target::GeneratedFile(_), _) => {
                    // An output file's associated rule's class transition.
                    match self
                        .generating_rule(id)
                        .and_then(|rule| self.transitions.get(&rule))
                    {
                        Some(facts) => format!("{} -> ", facts.class),
                        None => String::new(),
                    }
                }
                _ => String::new(),
            };
            writeln!(
                destination,
                "{class}{} ({})",
                self.display_label(node.label()),
                self.short_id(node)
            )
            .map_err(io)?;
            // CqueryTransitionResolver.dependencies: rule configured targets
            // only (not aliases, output files, sources or incompatible ones).
            let (wire::Target::Rule(rule), Some(facts)) = (&node.target, facts) else {
                continue;
            };
            // Aliases and label build settings are AliasConfiguredTargets.
            if matches!(
                rule.rule_class.as_str(),
                "alias" | "label_flag" | "label_setting"
            ) {
                continue;
            }
            let mut groups: Vec<(&str, &str, Vec<u32>)> = Vec::new();
            for edge in &node.edges {
                if edge.aspect
                    || edge.attribute.is_empty()
                    || matches!(
                        edge.attribute.as_str(),
                        "$config_dependencies" | "$target_platform" | "visibility"
                    )
                {
                    continue;
                }
                let label = self.nodes[edge.to as usize].label();
                match groups
                    .iter_mut()
                    .find(|(attribute, to, _)| *attribute == edge.attribute && *to == label)
                {
                    Some((_, _, targets)) => {
                        if !targets.contains(&edge.to) {
                            targets.push(edge.to);
                        }
                    }
                    None => groups.push((&edge.attribute, label, vec![edge.to])),
                }
            }
            for (attribute, label, targets) in groups {
                let first = &self.nodes[targets[0] as usize];
                // Attribute.getName: a Starlark private attribute `_x` is `$x`.
                let name = if attribute == "$toolchains" {
                    "[toolchain dependency]".to_owned()
                } else if let Some(private) = attribute.strip_prefix('_') {
                    format!("${private}")
                } else {
                    attribute.to_owned()
                };
                let (transition, changed) =
                    if targets.len() == 1 && first.configuration == node.configuration {
                        ("NoTransition".to_owned(), false)
                    } else if first.configuration.is_none() {
                        ("(null transition)".to_owned(), false)
                    } else {
                        let dependency = match &first.target {
                            wire::Target::GeneratedFile(_) => self.generating_rule(targets[0]),
                            _ => Some(targets[0]),
                        };
                        let rule_transition = dependency
                            .and_then(|id| self.transitions.get(&id))
                            .map(|facts| facts.rule.clone())
                            .unwrap_or_else(|| "NoTransition".to_owned());
                        let attribute_transition =
                            facts.attributes.get(attribute).cloned().or_else(|| {
                                // A native rule's tool attribute (genrule `tools`).
                                let to_tool = first
                                    .configuration
                                    .is_some_and(|id| self.configurations[id as usize].is_tool);
                                let from_tool = node
                                    .configuration
                                    .is_some_and(|id| self.configurations[id as usize].is_tool);
                                (to_tool && !from_tool).then(|| "exec".to_owned())
                            });
                        let name = match attribute_transition {
                            None => rule_transition,
                            Some(attribute) if rule_transition == "NoTransition" => attribute,
                            Some(attribute) => format!("({attribute} + {rule_transition})"),
                        };
                        (name, true)
                    };
                let configurations = if changed {
                    targets
                        .iter()
                        .map(|id| self.short_id(&self.nodes[*id as usize]))
                        .collect::<Vec<_>>()
                        .join(", ")
                } else {
                    String::new()
                };
                writeln!(
                    destination,
                    "  {name}#{}#{transition} -> {configurations}",
                    self.display_label(label)
                )
                .map_err(io)?;
                if full && changed {
                    let Some(from) = node.configuration else {
                        continue;
                    };
                    let to: Vec<u32> = targets
                        .iter()
                        .filter_map(|id| self.nodes[*id as usize].configuration)
                        .collect();
                    for line in self.options_diff(from, &to) {
                        writeln!(destination, "    {line}").map_err(io)?;
                    }
                }
            }
        }
        Ok(())
    }

    /// The generating rule node of an output file node.
    fn generating_rule(&self, id: u32) -> Option<u32> {
        self.nodes[id as usize]
            .edges
            .iter()
            .find(|edge| {
                edge.attribute.is_empty()
                    && !edge.aspect
                    && matches!(self.nodes[edge.to as usize].target, wire::Target::Rule(_))
            })
            .map(|edge| edge.to)
    }

    /// The `[...]` list LabelAndConfigurationOutputFormatterCallback
    /// appends (requiredFragmentStrings: sorted names).
    pub(super) fn fragments_text(
        &self,
        id: u32,
        transitive: bool,
        memo: &mut HashMap<u32, Arc<BTreeSet<String>>>,
    ) -> Result<String, String> {
        if !self.fragments_recorded {
            return Err(
                "internal error: this configured graph records no required fragments".into(),
            );
        }
        let set = self.required(id, transitive, memo);
        Ok(format!(
            "[{}]",
            set.iter()
                .map(String::as_str)
                .collect::<Vec<_>>()
                .join(", ")
        ))
    }

    /// RequiredFragmentsUtil.getRequiredFragments of one node, memoized.
    fn required(
        &self,
        id: u32,
        transitive: bool,
        memo: &mut HashMap<u32, Arc<BTreeSet<String>>>,
    ) -> Arc<BTreeSet<String>> {
        if let Some(set) = memo.get(&id) {
            return set.clone();
        }
        // Cycle guard: a node under evaluation contributes nothing to itself.
        memo.insert(id, Arc::new(BTreeSet::new()));
        let node = &self.nodes[id as usize];
        let set = match &node.target {
            wire::Target::GeneratedFile(_) => match self.generating_rule(id) {
                Some(rule) => self.required(rule, transitive, memo),
                None => Arc::new(BTreeSet::new()),
            },
            wire::Target::Rule(_) => match self.fragments.get(&id) {
                Some(facts) if !facts.none && !node.incompatible => {
                    let mut set = facts.own.clone();
                    for edge in &node.edges {
                        if edge.attribute == "$config_dependencies" {
                            // Config conditions merge their providers in
                            // both modes (getRequiredFragments).
                            set.extend(self.required(edge.to, transitive, memo).iter().cloned());
                        } else if transitive {
                            set.extend(self.required(edge.to, transitive, memo).iter().cloned());
                        } else if !edge.aspect {
                            // addStarlarkBuildSettings: a prerequisite's
                            // BuildSettingProvider (forwarded by aliases).
                            if let Some(setting) = self.setting_of(edge.to) {
                                set.insert(setting);
                            }
                        }
                    }
                    if transitive {
                        set.extend(facts.aspects.iter().cloned());
                    }
                    Arc::new(set)
                }
                _ => Arc::new(BTreeSet::new()),
            },
            _ => Arc::new(BTreeSet::new()),
        };
        memo.insert(id, set.clone());
        set
    }

    /// The build setting label a node provides (through alias chains).
    fn setting_of(&self, mut id: u32) -> Option<String> {
        for _ in 0..self.nodes.len() {
            let node = &self.nodes[id as usize];
            if let Some(setting) = self
                .fragments
                .get(&id)
                .and_then(|facts| facts.setting.clone())
            {
                return Some(setting);
            }
            let wire::Target::Rule(rule) = &node.target else {
                return None;
            };
            if rule.rule_class != "alias" {
                return None;
            }
            id = node
                .edges
                .iter()
                .find(|edge| edge.attribute == "actual")?
                .to;
        }
        None
    }
}
