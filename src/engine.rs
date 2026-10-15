//! A deliberately small native analysis/execution pipeline, not Skyframe.
use crate::action::{self, Action};
use crate::analysis::{self, DependencyTarget};
use crate::config_registry::Configurations;
pub use crate::config_registry::ConfiguredTargetKey;
use crate::configuration::{ConfigId, Configuration, LabelParser, SettingValue};
use crate::configured_attributes::{self, ConfigMatching};
use crate::ignored::IgnoredSubdirectories;
use crate::label::{Label, PackageId};
use crate::package::{self, Package, Rule, RuleKind};
use crate::package_specification::{self, GroupNode};
use crate::platforms::{ConstraintSettingInfo, ConstraintValueInfo, PlatformInfo};
use crate::repository::RepositorySet;
use crate::repository_budget::HostBudget;
use crate::scheduler::{self, Dependency, PlannedAction};
use crate::starlark_options::{BuildSettingInfo, LoadedTarget, SettingType};
use crate::starlark_types::File;
use crate::toolchains::{
    DeclaredToolchainInfo, RegisteredToolchains, ToolchainContextKey, ToolchainTypeInfo,
    ToolchainTypeRequirement, UnloadedToolchainContext,
};
use crate::visibility::Visibility;
use crate::workspace;
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;

#[derive(Clone, Debug)]
struct Artifact {
    real: PathBuf,
    exec: PathBuf,
    producer: Option<(String, usize)>,
    owner: String,
}

impl Artifact {
    fn file(&self) -> File {
        // A generated file is relative to its configuration's output root.
        let exec = self.exec.to_string_lossy();
        let relative = self
            .producer
            .as_ref()
            .and_then(|_| crate::label::split_generated_root(&exec))
            .map_or(self.exec.as_path(), |(_, relative)| Path::new(relative));
        // External short paths are metadata, never filesystem lookup paths.
        let short_path = if Label::parse_canonical(&self.owner)
            .is_ok_and(|label| !label.repository.is_empty())
        {
            relative
                .strip_prefix("external")
                .map(|path| PathBuf::from("..").join(path))
                .unwrap_or_else(|_| relative.to_path_buf())
        } else {
            relative.to_path_buf()
        };
        File {
            path: self.exec.to_string_lossy().into_owned(),
            short_path: short_path.to_string_lossy().into_owned(),
            owner: self.owner.clone(),
            is_source: self.producer.is_none(),
        }
    }
}

type Locations = BTreeMap<String, Vec<PathBuf>>;
/// Declared generated artifacts by configuration and execution path.
type Declared = BTreeMap<(ConfigId, PathBuf), Artifact>;
type ResolvedInputs = (Vec<Artifact>, Locations);

#[derive(Clone)]
enum Target {
    Rule(Rule),
    Output(Rule, String),
    Source(PathBuf),
}

/// An analyzed rule target in its configuration.
#[derive(Clone, Debug, Default)]
struct ConfiguredNode {
    /// ConfigMatchingProvider of an analyzed `config_setting` (forwarded by
    /// aliases).
    matching: Option<ConfigMatching>,
    /// The native BuildSettingProvider (forwarded by `alias`, overridden by
    /// label_flag/label_setting; rules/LateBoundAlias.java:58-79).
    build_setting: Option<BuildSettingInfo>,
    /// The direct `actual` of `alias`, `label_flag` and `label_setting`
    /// (AliasConfiguredTarget, rules/AliasConfiguredTarget.java:99-123).
    actual: Option<Label>,
    /// ConstraintSettingInfo of a `constraint_setting` (forwarded by aliases).
    constraint_setting: Option<ConstraintSettingInfo>,
    /// ConstraintValueInfo of a `constraint_value` (forwarded by aliases).
    constraint_value: Option<ConstraintValueInfo>,
    /// PlatformInfo of a `platform` (forwarded by aliases).
    platform: Option<Arc<PlatformInfo>>,
    /// ToolchainTypeInfo of a `toolchain_type` (forwarded by aliases).
    toolchain_type: Option<ToolchainTypeInfo>,
    /// DeclaredToolchainInfo of a `toolchain` (forwarded by aliases).
    declared_toolchain: Option<Arc<DeclaredToolchainInfo>>,
}

/// A failure of toolchain resolution: a ToolchainException, reported as
/// `While resolving toolchains for target L (CONFIG): MESSAGE` at the
/// rule's location (skyframe/toolchains/ToolchainException.java:67-91), or
/// an error that already carries its own report (the analysis error of a
/// prerequisite, ConfiguredValueCreationException, which
/// asConfiguredValueCreationException propagates unchanged).
#[derive(Clone, Debug)]
enum ResolutionError {
    Toolchain(String),
    /// An error that aborts the analysis of the requested target
    /// (`Analysis of target 'L' failed; build aborted: MESSAGE`).
    Aborted(String),
    /// A ToolchainException preceded by reported events (a loading event).
    Reported(String, String),
    Propagated(String),
}

/// Why a platform label could not be resolved (PlatformFunction).
#[derive(Clone, Debug)]
enum PlatformError {
    /// A package or target loading error (NoSuchThingException, propagated
    /// unchanged by InvalidPlatformException, PlatformLookupUtil.java:186-190).
    Missing {
        class: &'static str,
        message: String,
    },
    /// Any other failure, its full text.
    Other(String),
}

impl PlatformError {
    /// A `no such target`/`no such package` loading error, or another error.
    fn loading(error: String) -> Self {
        let class = if error.starts_with("no such target '") {
            "NoSuchTargetException"
        } else if error.starts_with("no such package '") {
            if error.contains("BUILD file not found") {
                "BuildFileNotFoundException"
            } else {
                "NoSuchPackageException"
            }
        } else {
            return Self::Other(error);
        };
        Self::Missing {
            class,
            message: error,
        }
    }

    /// The text of a failing target platform: Bazel 9.3 prints the loading
    /// exception itself (`com.google.devtools.build.lib.packages.CLASS:
    /// MESSAGE`, paired capture A4/platforms_e_nope, U/.../platforms_missing).
    fn target_text(&self) -> String {
        match self {
            Self::Missing { class, message } => {
                format!("com.google.devtools.build.lib.packages.{class}: {message}")
            }
            Self::Other(text) => text.clone(),
        }
    }
}

/// The platform providers of a `constraint_setting`, `constraint_value` or
/// `platform` target.
type PlatformProviders = (
    Option<ConstraintSettingInfo>,
    Option<ConstraintValueInfo>,
    Option<Arc<PlatformInfo>>,
);

/// A native provider an attribute requires (`mandatoryProviders`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum NativeProvider {
    ConstraintSetting,
    ConstraintValue,
    Platform,
    ToolchainType,
}

impl NativeProvider {
    /// BuiltinProvider names (RequiredProviders.getDescription quotes them,
    /// packages/RequiredProviders.java:209-213).
    fn name(self) -> &'static str {
        match self {
            Self::ConstraintSetting => "ConstraintSettingInfo",
            Self::ConstraintValue => "ConstraintValueInfo",
            Self::Platform => "PlatformInfo",
            Self::ToolchainType => "ToolchainTypeInfo",
        }
    }

    fn provided_by(self, node: &ConfiguredNode) -> bool {
        match self {
            Self::ConstraintSetting => node.constraint_setting.is_some(),
            Self::ConstraintValue => node.constraint_value.is_some(),
            Self::Platform => node.platform.is_some(),
            Self::ToolchainType => node.toolchain_type.is_some(),
        }
    }
}

/// A label attribute of a native rule as RuleContext.Builder validates its
/// prerequisites (analysis/RuleContext.java:1845-1880, 2002-2093, 2165-2172):
/// every attribute here allows no files.
struct PrerequisiteAttribute {
    name: &'static str,
    labels: Vec<String>,
    /// `allowedRuleClasses` (empty: any rule).
    allowed_rule_classes: &'static [&'static str],
    mandatory_provider: Option<NativeProvider>,
}

/// Rule.useToolchainResolution (packages/Rule.java:846-854) of the native
/// rule classes and Starlark rules: disabled for `constraint_setting`,
/// `constraint_value`, `platform`, `toolchain`, `toolchain_type`
/// (ToolchainResolutionMode.DISABLED, rules/platform/*Rule.java,
/// rules/ToolchainType.java:62), label settings and Starlark build settings
/// (rules/LabelBuildSettings.java:99, packages/RuleClass.java:916-918),
/// conditional for `alias` (select() keys or target_compatible_with,
/// rules/Alias.java:78-98) and `config_setting` (non-empty constraint_values,
/// rules/config/ConfigRuleClasses.java:196-201), enabled otherwise.
fn uses_toolchain_resolution(rule: &Rule) -> bool {
    if let Some(custom) = &rule.custom {
        return custom.build_setting.is_none();
    }
    match (&rule.kind, &rule.native) {
        (RuleKind::PackageGroup(_), _) => false,
        (RuleKind::Native("alias"), Some(native)) => {
            !configured_attributes::rule_config_dependencies(rule).is_empty()
                || !native.labels("target_compatible_with").is_empty()
                || native.configurable.contains_key("target_compatible_with")
        }
        (RuleKind::Native("config_setting"), Some(native)) => {
            !native.labels("constraint_values").is_empty()
        }
        (
            RuleKind::Native(
                "label_flag" | "label_setting" | "constraint_setting" | "constraint_value"
                | "platform" | "toolchain" | "toolchain_type",
            ),
            _,
        ) => false,
        _ => true,
    }
}

/// The label attributes of `constraint_setting` (ConstraintSettingRule.java:84-89),
/// `constraint_value` (ConstraintValueRule.java:56-63) and `platform`
/// (PlatformRule.java:75-89, 149-152), in rule-class attribute order.
fn platform_prerequisite_attributes(rule: &Rule) -> Vec<PrerequisiteAttribute> {
    let Some(native) = &rule.native else {
        return Vec::new();
    };
    let label = |name: &str| match native.attrs.get(name) {
        Some(crate::custom_rule::AttrValue::Label(Some(text))) => vec![text.clone()],
        _ => Vec::new(),
    };
    match rule.kind {
        RuleKind::Native("constraint_setting") => vec![PrerequisiteAttribute {
            name: "refines_constraint_value",
            labels: label("refines_constraint_value"),
            allowed_rule_classes: &["constraint_value"],
            mandatory_provider: Some(NativeProvider::ConstraintValue),
        }],
        RuleKind::Native("constraint_value") => vec![PrerequisiteAttribute {
            name: "constraint_setting",
            labels: label("constraint_setting"),
            allowed_rule_classes: &["constraint_setting"],
            mandatory_provider: Some(NativeProvider::ConstraintSetting),
        }],
        RuleKind::Native("platform") => vec![
            PrerequisiteAttribute {
                name: "constraint_values",
                labels: native.labels("constraint_values"),
                allowed_rule_classes: &[],
                mandatory_provider: Some(NativeProvider::ConstraintValue),
            },
            PrerequisiteAttribute {
                name: "parents",
                labels: native.labels("parents"),
                allowed_rule_classes: &[],
                mandatory_provider: Some(NativeProvider::Platform),
            },
            PrerequisiteAttribute {
                name: "required_settings",
                labels: native.labels("required_settings"),
                allowed_rule_classes: &["config_setting"],
                mandatory_provider: None,
            },
        ],
        // rules/platform/ToolchainRule.java:57-111 (the NODEP `toolchain`
        // attribute is not a prerequisite).
        RuleKind::Native("toolchain") => vec![
            PrerequisiteAttribute {
                name: "toolchain_type",
                labels: label("toolchain_type"),
                allowed_rule_classes: &["toolchain_type"],
                mandatory_provider: Some(NativeProvider::ToolchainType),
            },
            PrerequisiteAttribute {
                name: "exec_compatible_with",
                labels: native.labels("exec_compatible_with"),
                allowed_rule_classes: &[],
                mandatory_provider: Some(NativeProvider::ConstraintValue),
            },
            PrerequisiteAttribute {
                name: "target_compatible_with",
                labels: native.labels("target_compatible_with"),
                allowed_rule_classes: &[],
                mandatory_provider: Some(NativeProvider::ConstraintValue),
            },
            // `target_settings`: only config_setting rules, no files, no
            // mandatory provider (ToolchainRule.java:97-103).
            PrerequisiteAttribute {
                name: "target_settings",
                labels: native.labels("target_settings"),
                allowed_rule_classes: &["config_setting"],
                mandatory_provider: None,
            },
        ],
        _ => Vec::new(),
    }
}

/// RuleContext.Builder.validateRuleDependency (analysis/RuleContext.java:2002-2093):
/// a rule prerequisite is accepted when its class is allowed, else when it
/// has the mandatory provider; otherwise both unfulfilled requirements are
/// joined (StringUtil.joinEnglishList with "and").
fn rule_dependency_error(
    attribute: &PrerequisiteAttribute,
    chain: &[Label],
    actual: &Label,
    actual_rule: &Rule,
    node: Option<&ConfiguredNode>,
) -> Option<String> {
    let mut unfulfilled = Vec::new();
    if !attribute.allowed_rule_classes.is_empty() {
        if attribute
            .allowed_rule_classes
            .contains(&actual_rule.rule_class_name())
        {
            return None;
        }
        unfulfilled.push(format!(
            "{} is misplaced here (expected {})",
            describe_with_aliases(
                chain,
                actual,
                &format!("{} rule", actual_rule.rule_class_name())
            ),
            join_english_list(attribute.allowed_rule_classes, "or")
        ));
    }
    if let Some(provider) = attribute.mandatory_provider {
        if node.is_some_and(|node| provider.provided_by(node)) {
            return None;
        }
        unfulfilled.push(format!(
            "'{}' does not have mandatory providers: '{}'",
            crate::rule_declarations::canonical_form(actual),
            provider.name()
        ));
    }
    (!unfulfilled.is_empty()).then(|| join_english_list(&unfulfilled, "and"))
}

/// StringUtil.joinEnglishList (util/StringUtil.java:54-73) with the Oxford
/// comma.
fn join_english_list<T: AsRef<str>>(choices: &[T], last: &str) -> String {
    let mut text = String::new();
    for (index, choice) in choices.iter().enumerate() {
        if index > 0 {
            let is_last = index + 1 == choices.len();
            if !is_last || index >= 2 {
                text.push(',');
            }
            if is_last {
                text.push(' ');
                text.push_str(last);
            }
            text.push(' ');
        }
        text.push_str(choice.as_ref());
    }
    if text.is_empty() {
        "nothing".to_owned()
    } else {
        text
    }
}

/// PlatformLookupUtil.hasPlatformInfo (skyframe/toolchains/PlatformLookupUtil.java:160-172):
/// the target's rule does not use toolchain resolution and its class
/// advertises PlatformInfo (`platform`, PlatformRule.java:53) or can have any
/// provider (`alias`, rules/Alias.java:76; label settings,
/// rules/LabelBuildSettings.java:98).
fn has_platform_info(rule: &Rule) -> bool {
    !uses_toolchain_resolution(rule)
        && matches!(
            rule.kind,
            RuleKind::Native("platform" | "alias" | "label_flag" | "label_setting")
        )
}

/// Rules whose configured target is an AliasConfiguredTarget: `alias`
/// (rules/Alias.java:43-56) and the LateBoundAlias build settings
/// label_flag/label_setting (rules/LateBoundAlias.java:47-79).
fn forwards(rule: &Rule) -> bool {
    matches!(
        rule.kind,
        RuleKind::Native("alias" | "label_flag" | "label_setting")
    )
}

/// AliasProvider.describeTargetWithAliases (analysis/AliasProvider.java:123-148):
/// `KIND 'L'`, or `alias 'A' referring to KIND 'L'[ through 'B' -> 'C']`.
fn describe_with_aliases(chain: &[Label], target: &Label, kind: &str) -> String {
    let target = crate::rule_declarations::canonical_form(target);
    let Some((first, rest)) = chain.split_first() else {
        return format!("{kind} '{target}'");
    };
    let mut text = format!(
        "alias '{}' referring to {kind} '{target}'",
        crate::rule_declarations::canonical_form(first)
    );
    if !rest.is_empty() {
        text.push_str(&format!(
            " through '{}'",
            rest.iter()
                .map(crate::rule_declarations::canonical_form)
                .collect::<Vec<_>>()
                .join("' -> '")
        ));
    }
    text
}

type ConfiguredNodes = BTreeMap<ConfiguredTargetKey, ConfiguredNode>;

/// The transition of a dependency edge.
#[derive(Clone, Debug, PartialEq, Eq)]
enum EdgeCfg {
    /// No transition: the parent's configuration.
    Target,
    /// ExecutionTransitionFactory for the default (`None`) or a named exec
    /// group: Starlark `cfg = "exec"`/`"host"`/`config.exec(...)`, genrule
    /// `tools` (rules/genrule/GenRuleBaseRule.java:89-92).
    Exec(Option<String>),
}

/// A dependency edge of a configured rule: the label text, its attribute
/// and transition.
struct Edge {
    label: String,
    attribute: String,
    cfg: EdgeCfg,
}

/// The dependency edges of a configured (select-resolved) rule, in the
/// order of `rule.srcs` (Starlark attributes in name order,
/// crate::package::custom_dependency_labels), then genrule `tools`.
fn dependency_edges(rule: &Rule) -> Vec<Edge> {
    let edge = |label: &String, attribute: &str, cfg: EdgeCfg| Edge {
        label: label.clone(),
        attribute: attribute.to_owned(),
        cfg,
    };
    if let Some(custom) = &rule.custom {
        let mut edges = Vec::new();
        for (name, value) in &custom.attrs {
            let cfg = match custom.schema.get(name).map(|spec| &spec.declaration.cfg) {
                Some(crate::rule_declarations::Cfg::Exec(group)) => EdgeCfg::Exec(
                    group
                        .clone()
                        // The default exec group by name
                        // (DeclaredExecGroup.DEFAULT_EXEC_GROUP_NAME).
                        .filter(|group| group != "default-exec-group"),
                ),
                _ => EdgeCfg::Target,
            };
            // As crate::package::custom_dependency_labels, per attribute.
            match value {
                crate::custom_rule::AttrValue::Label(Some(label)) => {
                    edges.push(edge(label, name, cfg));
                }
                crate::custom_rule::AttrValue::Labels(labels) => {
                    edges.extend(labels.iter().map(|label| edge(label, name, cfg.clone())));
                }
                crate::custom_rule::AttrValue::LabelKeyedStringDict(entries) => {
                    edges.extend(
                        entries
                            .iter()
                            .map(|(label, _)| edge(label, name, cfg.clone())),
                    );
                }
                // DictType.visitLabels (packages/Type.java:504-514): label values.
                crate::custom_rule::AttrValue::StringKeyedLabelDict(entries) => {
                    edges.extend(
                        entries
                            .iter()
                            .map(|(_, label)| edge(label, name, cfg.clone())),
                    );
                }
                crate::custom_rule::AttrValue::LabelListDict(entries) => {
                    edges.extend(
                        entries
                            .iter()
                            .flat_map(|(_, labels)| labels.iter())
                            .map(|label| edge(label, name, cfg.clone())),
                    );
                }
                _ => {}
            }
        }
        return edges;
    }
    let mut edges = rule
        .srcs
        .iter()
        .map(|label| edge(label, "srcs", EdgeCfg::Target))
        .collect::<Vec<_>>();
    if let (RuleKind::Genrule, Some(native)) = (&rule.kind, &rule.native) {
        edges.extend(
            native
                .labels("tools")
                .iter()
                .map(|label| edge(label, "tools", EdgeCfg::Exec(None))),
        );
    }
    edges
}

/// The planned actions of a build and the requested targets' artifacts.
type BuildPlan = (Vec<PlannedAction>, Vec<(ConfiguredTargetKey, Artifact)>);

/// The analyzed rules of a build in analysis (postorder) order, each with
/// the configuration it was analyzed in.
type Plan = Vec<(ConfigId, Rule)>;

#[derive(Debug, Default)]
pub struct BuildSummary {
    pub outputs: Vec<(Label, PathBuf)>,
    pub executed: usize,
    pub cache_hits: usize,
}

pub struct Engine {
    repositories: Arc<RepositorySet>,
    packages: BTreeMap<PackageId, Package>,
    generated: BTreeMap<Label, (Label, String)>,
    group_nodes: BTreeMap<Label, Arc<GroupNode>>,
    group_budget: HostBudget,
    /// The configurations of the build: requested targets are analyzed in
    /// the top-level one, and every dependency edge keeps its parent's
    /// configuration (no transitions are modelled yet; docs/CONFIGURATION.md).
    configurations: Configurations,
    /// Declared source files that do not exist (path -> label).
    missing_sources: BTreeMap<PathBuf, Label>,
    /// Warnings in emission order, without Bazel's `WARNING: ` prefix (added
    /// by the CLI): filtered target patterns and analysis warnings.
    warnings: Vec<String>,
    /// PlatformValue per platform label and configuration
    /// (PlatformFunction, analysis/platform/PlatformFunction.java:57-126).
    platforms: BTreeMap<ConfiguredTargetKey, Result<Arc<PlatformInfo>, PlatformError>>,
    /// Platforms being resolved (reentrancy guard).
    resolving_platforms: Vec<ConfiguredTargetKey>,
    /// Whether execution platform registrations were checked in this build.
    registrations_checked: bool,
    /// Rules analyzed while resolving a platform (no toolchain context).
    empty_configuration_rules: BTreeSet<ConfiguredTargetKey>,
    /// RegisteredToolchainsValue per configuration, computed when a rule
    /// first requests a toolchain type
    /// (skyframe/toolchains/RegisteredToolchainsFunction.java:64-147).
    registered_toolchains: BTreeMap<ConfigId, Result<Arc<RegisteredToolchains>, ResolutionError>>,
    /// Reentrancy guards of [`Self::registered_toolchains`]: per
    /// configuration whose registered toolchains are being computed, the
    /// registered toolchain target being analyzed (a stack: an exec edge of
    /// a registered target may compute another configuration's).
    computing_toolchains: Vec<ConfiguredTargetKey>,
    /// The cycle report of a registered toolchain that needs toolchain
    /// resolution itself (Self::toolchain_cycle), per configuration.
    toolchain_cycle_reports: BTreeMap<ConfigId, String>,
    /// The exec configuration of each analyzed target's default exec group
    /// (its parent configuration when the exec transition is a no-op), for
    /// targets with exec-transition dependency edges.
    exec_configurations: BTreeMap<ConfiguredTargetKey, ConfigId>,
    /// The top-level Starlark flags with `scope = "target"`, reset by the
    /// exec transition (computed on first use).
    target_scoped_flags: Option<BTreeSet<Label>>,
    /// The requested top-level rule being analyzed when its rule is test
    /// trimmed (not alias-like): its top-level key precedes it in cycle paths
    /// (Self::cycle_error).
    requested_trimmed_root: Option<ConfiguredTargetKey>,
    /// The resolved toolchain context of each toolchain-resolving rule of
    /// the build (UnloadedToolchainContext), read by the build loop.
    toolchain_contexts: BTreeMap<ConfiguredTargetKey, UnloadedToolchainContext>,
    /// Targets analyzed as toolchain implementations: their own resolution
    /// forces the parent's execution platform, the host platform
    /// (ToolchainContextUtil.java:136-141; docs/TOOLCHAINS.md).
    toolchain_implementations: BTreeSet<ConfiguredTargetKey>,
}

impl Engine {
    pub fn new(root: PathBuf) -> Self {
        Self::with_repositories(Arc::new(RepositorySet::new(root)))
    }

    pub fn with_repositories(repositories: Arc<RepositorySet>) -> Self {
        Self {
            repositories,
            packages: BTreeMap::new(),
            generated: BTreeMap::new(),
            group_nodes: BTreeMap::new(),
            group_budget: HostBudget::default(),
            configurations: Configurations::default(),
            missing_sources: BTreeMap::new(),
            warnings: Vec::new(),
            platforms: BTreeMap::new(),
            resolving_platforms: Vec::new(),
            registrations_checked: false,
            empty_configuration_rules: BTreeSet::new(),
            registered_toolchains: BTreeMap::new(),
            computing_toolchains: Vec::new(),
            toolchain_cycle_reports: BTreeMap::new(),
            exec_configurations: BTreeMap::new(),
            target_scoped_flags: None,
            requested_trimmed_root: None,
            toolchain_contexts: BTreeMap::new(),
            toolchain_implementations: BTreeSet::new(),
        }
    }

    /// Analyze builds in `configuration` (default: the default configuration).
    pub fn set_configuration(&mut self, configuration: Configuration) {
        self.configurations = Configurations::new(configuration);
        self.platforms.clear();
        self.registered_toolchains.clear();
        self.target_scoped_flags = None;
    }

    /// Drain the warnings reported so far (also after a failed build, as
    /// Bazel prints warnings before the error that stops it).
    pub fn take_warnings(&mut self) -> Vec<String> {
        std::mem::take(&mut self.warnings)
    }

    fn ignored(&self, repository: &str) -> Result<Arc<IgnoredSubdirectories>, String> {
        self.repositories.ignored_subdirectories(repository)
    }

    fn load_package(&mut self, package: &PackageId) -> Result<(), String> {
        // A label whose apparent repository the main repository does not see
        // (crate::label::unknown_repository_name) names no loadable package:
        // RepositoryMapping's unresolved name (cmdline/RepositoryName.java),
        // reported like PackageLookupFunction's repository error.
        if let Some(name) = crate::label::unknown_repository_name(&package.repository) {
            return Err(format!(
                "no such package '@@{repository}//{}': The repository '@@{repository}' could not be resolved: No repository visible as '@{name}' from main repository",
                package.package,
                repository = package.repository
            ));
        }
        if !self.packages.contains_key(package) {
            let rules = package::evaluate_in(self.repositories.clone(), package)?;
            let ignored = self.ignored(&package.repository)?;
            for rule in rules.values() {
                for out in &rule.outs {
                    self.validate_output_directory(package, out, &ignored)?;
                    let output = rule.label.with_name(out)?;
                    self.generated
                        .insert(output, (rule.label.clone(), out.clone()));
                }
            }
            self.packages.insert(package.to_owned(), rules);
        }
        Ok(())
    }

    fn validate_output_directory(
        &self,
        package: &PackageId,
        out: &str,
        ignored: &IgnoredSubdirectories,
    ) -> Result<(), String> {
        let mut directory = self
            .repositories
            .root(&package.repository)?
            .join(&package.package);
        let mut relative = package.package.clone();
        let components = out.split('/').collect::<Vec<_>>();
        for component in components.iter().take(components.len().saturating_sub(1)) {
            directory.push(component);
            if !relative.is_empty() {
                relative.push('/');
            }
            relative.push_str(component);
            match std::fs::symlink_metadata(&directory) {
                Ok(metadata) if metadata.file_type().is_symlink() => {
                    return Err(format!(
                        "output {out} has an unsupported symlink source-tree parent {}",
                        directory.display()
                    ));
                }
                // Generated outputs inhabit another tree. A regular source file
                // or missing path cannot contain a source subpackage boundary.
                Ok(metadata) if !metadata.is_dir() => break,
                Ok(_) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => break,
                Err(error) => {
                    return Err(format!(
                        "cannot inspect output package boundary {}: {error}",
                        directory.display()
                    ));
                }
            }
            // An ignored directory is a deleted package, never a subpackage
            // (ContainingPackageLookupFunction).
            let subpackage = ignored.matching_entry(&relative).is_none()
                && (directory.join("BUILD.bazel").is_file() || directory.join("BUILD").is_file());
            if subpackage || workspace::has_marker(&directory)? {
                return Err(format!(
                    "output {out} crosses a package or nested workspace boundary at {}",
                    directory.display()
                ));
            }
        }
        Ok(())
    }

    fn target(&mut self, label: &Label) -> Result<Target, String> {
        self.load_package(&label.package_id())?;
        if let Some(rule) = self.packages[&label.package_id()].get(label) {
            return Ok(Target::Rule(rule.clone()));
        }
        if let Some((owner, output)) = self.generated.get(label) {
            return Ok(Target::Output(
                self.packages[&owner.package_id()][owner].clone(),
                output.clone(),
            ));
        }
        let package = &self.packages[&label.package_id()];
        let declared_source = package.exports.contains_key(label)
            || (!package.macro_names.contains(&label.name)
                && package
                    .values()
                    .filter(|rule| {
                        rule.macro_owner.is_none()
                            && !matches!(rule.kind, RuleKind::PackageGroup(_))
                    })
                    .any(|rule| {
                        rule.srcs
                            .iter()
                            .any(|src| self.resolve_rule_src(rule, src).as_ref() == Ok(label))
                            // Labels in any other native attribute or select()
                            // are assumed input files too (Package.java:897-920).
                            || rule
                                .native
                                .as_ref()
                                .is_some_and(|native| native.input_labels.contains(label))
                            // Built-in attributes of Starlark rules likewise.
                            || rule.custom.as_ref().is_some_and(|custom| {
                                custom.declaration.builtin.target.input_labels.contains(label)
                                    // Starlark select() keys and branches.
                                    || custom.declaration.configurable_labels.contains(label)
                            })
                    }));
        if !declared_source {
            return Err(self.no_such_target(label)?);
        }
        let repository_root = self.repositories.root(&label.repository)?.to_path_buf();
        let package_dir = repository_root.join(&label.package);
        let ignored = self.ignored(&label.repository)?;
        let mut path = package_dir.clone();
        let mut relative = label.package.clone();
        let mut linked = false;
        let parts: Vec<_> = Path::new(&label.name).components().collect();
        if parts
            .iter()
            .any(|component| !matches!(component, Component::Normal(_)))
        {
            return Err(format!("unsafe source label {label}"));
        }
        for (i, component) in parts.iter().enumerate() {
            path.push(component.as_os_str());
            if !relative.is_empty() {
                relative.push('/');
            }
            relative.push_str(&component.as_os_str().to_string_lossy());
            let metadata = match std::fs::symlink_metadata(&path) {
                Ok(metadata) => metadata,
                // A declared (assumed) input file is a target whether or not
                // the file exists (Package.java:893-920); a missing file only
                // fails when an action or the build request needs it
                // (skyframe/ArtifactFunction.java:79-90).
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                    let full = package_dir.join(&label.name);
                    self.missing_sources.insert(full.clone(), label.clone());
                    return Ok(Target::Source(full));
                }
                Err(error) => return Err(format!("no target or source {label}: {error}")),
            };
            // Bazel follows source symlinks (docs/ARCHIVE_SYMLINKS.md); package
            // boundaries below stay checked on the logical label path.
            linked |= metadata.file_type().is_symlink();
            if i + 1 < parts.len()
                && ((ignored.matching_entry(&relative).is_none()
                    && (path.join("BUILD.bazel").is_file() || path.join("BUILD").is_file()))
                    || workspace::has_marker(&path)?)
            {
                return Err(format!(
                    "{label} crosses a package or nested workspace boundary at {}",
                    path.display()
                ));
            }
        }
        if linked {
            let relative = Path::new(&label.package).join(&label.name);
            match crate::source_links::resolve_in_root(&repository_root, &relative)
                .map_err(|error| format!("source {label}: {error}"))?
            {
                crate::source_links::Resolution::Resolved(resolved) => path = resolved,
                // A dangling source symlink is a missing input file, like a
                // declared source that does not exist.
                crate::source_links::Resolution::Missing(missing) => {
                    self.missing_sources.insert(missing.clone(), label.clone());
                    return Ok(Target::Source(missing));
                }
            }
        }
        if !path.is_file() {
            return Err(format!("source is not a regular file: {label}"));
        }
        Ok(Target::Source(path))
    }

    /// NoSuchTargetException for an undeclared target of a loaded package:
    /// Package.getTarget (packages/Package.java:335-362) with
    /// getAlternateTargetSuggestion (:364-396) and TargetSuggester
    /// (packages/TargetSuggester.java), NoSuchTargetException.java:51.
    fn no_such_target(&self, label: &Label) -> Result<String, String> {
        let root = self.repositories.root(&label.repository)?;
        let Ok(build) = workspace::package_build_file(root, &label.package) else {
            // Without a BUILD file path, the succinct form (Package.java:348-350).
            return Ok(format!(
                "no such target '{}': target '{}' not declared in package '{}'",
                crate::rule_declarations::canonical_form(label),
                label.name,
                label.package
            ));
        };
        let directory = root.join(&label.package);
        let candidate = directory.join(&label.name);
        let build_name = build
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_default();
        let relative_build = if label.package.is_empty() {
            build_name
        } else {
            format!("{}/{build_name}", label.package)
        };
        let suggestion = if label.name == "*" {
            String::new()
        } else if candidate.is_dir() {
            format!(
                "; however, a source directory of this name exists.  (Perhaps add 'exports_files([\"{}\"])' to {relative_build}, or define a filegroup?)",
                label.name
            )
        } else if candidate.exists() {
            format!(
                "; however, a source file of this name exists.  (Perhaps add 'exports_files([\"{}\"])' to {relative_build}?)",
                label.name
            )
        } else {
            suggest_targets(&label.name, &self.package_target_names(&label.package_id()))
        };
        Ok(format!(
            "no such target '{}': target '{}' not declared in package '{}' defined by {}{suggestion}",
            crate::rule_declarations::canonical_form(label),
            label.name,
            label.package,
            build.display()
        ))
    }

    /// Names of a loaded package's targets (rules, outputs, exported and
    /// assumed input files), in sorted order (Packageoid.targets is an
    /// ImmutableSortedMap, packages/Packageoid.java:84).
    fn package_target_names(&self, package: &PackageId) -> BTreeSet<String> {
        let mut names = BTreeSet::new();
        let Some(rules) = self.packages.get(package) else {
            return names;
        };
        names.extend(rules.exports.keys().map(|label| label.name.clone()));
        for rule in rules.values() {
            names.insert(rule.label.name.clone());
            names.extend(rule.outs.iter().cloned());
            if rule.macro_owner.is_some() || matches!(rule.kind, RuleKind::PackageGroup(_)) {
                continue;
            }
            let mut labels = rule
                .srcs
                .iter()
                .filter_map(|src| self.resolve_rule_src(rule, src).ok())
                .collect::<Vec<_>>();
            if let Some(native) = &rule.native {
                labels.extend(native.input_labels.iter().cloned());
            }
            if let Some(custom) = &rule.custom {
                labels.extend(
                    custom
                        .declaration
                        .builtin
                        .target
                        .input_labels
                        .iter()
                        .cloned(),
                );
                labels.extend(custom.declaration.configurable_labels.iter().cloned());
            }
            names.extend(
                labels
                    .into_iter()
                    .filter(|label| label.package_id() == *package)
                    .map(|label| label.name),
            );
        }
        names
    }

    fn resolve_rule_src(&self, rule: &Rule, text: &str) -> Result<Label, String> {
        if matches!(rule.kind, RuleKind::Custom)
            || (rule.native.is_some() && (text.starts_with("//") || text.starts_with("@@")))
        {
            // Custom and native attributes were canonicalized during package evaluation.
            // In particular, //p:t here denotes main, even in an external rule.
            Label::parse_canonical(text)
        } else {
            self.repositories
                .parse(text, &rule.label.repository, &rule.label.package)
        }
    }

    /// Only literal labels, :all, //... and //package/...; not the query language.
    /// `query` keeps every rule (FilteringPolicies.NO_FILTER).
    pub fn select(
        &mut self,
        patterns: &[String],
        current_package: &str,
    ) -> Result<Vec<Label>, String> {
        self.select_filtered(patterns, current_package, false)
    }

    /// Build target patterns: wildcards skip rules tagged `manual`
    /// (FilteringPolicies.FILTER_MANUAL, pkgcache/FilteringPolicies.java:60-65;
    /// skyframe/TargetPatternPhaseFunction.java:293-296); literal labels stay.
    pub fn select_for_build(
        &mut self,
        patterns: &[String],
        current_package: &str,
    ) -> Result<Vec<Label>, String> {
        self.select_filtered(patterns, current_package, true)
    }

    fn select_filtered(
        &mut self,
        patterns: &[String],
        current_package: &str,
        filter_manual: bool,
    ) -> Result<Vec<Label>, String> {
        let mut selected = BTreeSet::new();
        for pattern in patterns {
            // A `-` prefix makes a negative pattern (TargetPattern.Parser,
            // cmdline/TargetPattern.java); its syntax is checked like any
            // pattern, but subtraction is not implemented natively.
            if let Some(negative) = pattern.strip_prefix('-') {
                crate::bazel_label::Parts::parse(negative)?;
                return Err(format!(
                    "negative target pattern {pattern:?} is not implemented in the native profile"
                ));
            }
            if (pattern.starts_with("//") || pattern.starts_with('@')) && pattern.ends_with("/...")
            {
                let prefix = pattern.strip_suffix("...").unwrap();
                let prefix = if prefix.ends_with("//") {
                    prefix
                } else {
                    prefix.strip_suffix('/').unwrap()
                };
                let context = self.repositories.parse(
                    &format!("{prefix}:__pattern__"),
                    "",
                    current_package,
                )?;
                // TargetPattern.TargetsBelowDirectory.eval: a directory inside
                // an ignored directory yields a warning and no targets; the
                // rest is searched with filterForDirectory'd entries.
                let ignored = self.ignored(&context.repository)?;
                if let Some(entry) = ignored.matching_entry(&context.package) {
                    self.warnings.push(format!(
                        "Pattern '{pattern}' was filtered out by ignored directory '{entry}'"
                    ));
                    continue;
                }
                let ignored = ignored.filter_for_directory(&context.package);
                let root = self.repositories.root(&context.repository)?;
                for package in workspace::discover_packages(root, &context.package, &ignored)? {
                    let id = PackageId {
                        repository: context.repository.clone(),
                        package,
                    };
                    self.load_package(&id)?;
                    selected.extend(
                        self.packages[&id]
                            .values()
                            .filter(|rule| !matches!(rule.kind, RuleKind::PackageGroup(_)))
                            .filter(|rule| !(filter_manual && rule.is_manual()))
                            .map(|rule| rule.label.clone()),
                    );
                }
            } else if let Some(prefix) = pattern.strip_suffix(":all") {
                if !prefix.is_empty() && !prefix.starts_with("//") && !prefix.starts_with('@') {
                    return Err(format!("unsupported target pattern: {pattern}"));
                }
                let id = self
                    .repositories
                    .parse(pattern, "", current_package)?
                    .package_id();
                self.load_package(&id)?;
                selected.extend(
                    self.packages[&id]
                        .values()
                        .filter(|rule| !matches!(rule.kind, RuleKind::PackageGroup(_)))
                        .filter(|rule| !(filter_manual && rule.is_manual()))
                        .map(|rule| rule.label.clone()),
                );
            } else {
                if pattern.contains(['(', ')', '*', ' ']) {
                    return Err(format!(
                        "unsupported query expression or target pattern: {pattern}"
                    ));
                }
                let label = self.repositories.parse(pattern, "", current_package)?;
                self.target(&label)?;
                selected.insert(label);
            }
        }
        Ok(selected.into_iter().collect())
    }

    fn resolve_group(
        &mut self,
        label: &Label,
        active: &mut Vec<Label>,
    ) -> Result<Arc<GroupNode>, String> {
        let package = label.package_id();
        self.load_package(&package)?;
        let Some(rule) = self.packages[&package].get(label) else {
            // Preserve existing missing/source/output diagnostics, but never
            // analyze a callback or fabricate a source from an include edge.
            self.target(label)?;
            return Err(format!("{label} does not refer to a package group"));
        };
        let RuleKind::PackageGroup(definition) = &rule.kind else {
            return Err(format!("{label} does not refer to a package group"));
        };
        // Cache hits cannot bypass actual target kind or symbolic ownership.
        check_macro_namespace(rule, label)?;
        if let Some(node) = self.group_nodes.get(label) {
            return Ok(Arc::clone(node));
        }
        if active.contains(label) {
            return Err(format!("package group include cycle at {label}"));
        }
        if active.len() >= 256 {
            return Err("package group DAG exceeds depth limit 256 path nodes".into());
        }
        self.group_budget.node()?;
        definition.own_specs.charge(&mut self.group_budget)?;
        for part in [&label.repository, &label.package, &label.name] {
            self.group_budget.string(part)?;
        }
        // Repeated edges consume budget too, BEFORE allocating the edge vector.
        for edge in &definition.includes {
            self.group_budget.node()?;
            for part in [&edge.repository, &edge.package, &edge.name] {
                self.group_budget.string(part)?;
            }
        }
        // Keep the declaration alive across recursive package loading without
        // copying its specifications or include labels. Cache hits above do not
        // even clone this Arc. The sole local payload copy is precharged above.
        let definition = Arc::clone(definition);
        active.push(label.clone());
        let result = (|| {
            let mut includes = Vec::with_capacity(definition.includes.len());
            for edge in &definition.includes {
                includes.push(self.resolve_group(edge, active)?);
            }
            // Cached child heights are checked by the constructor, independently
            // of this traversal's active depth.
            let node = Arc::new(GroupNode::new(
                Arc::new(definition.own_specs.clone()),
                includes,
            )?);
            self.group_nodes.insert(label.clone(), node.clone());
            Ok(node)
        })();
        active.pop();
        result
    }

    fn source_visibility(&self, label: &Label) -> Visibility {
        let package = &self.packages[&label.package_id()];
        package
            .exports
            .get(label)
            .unwrap_or(&package.default_visibility)
            .clone()
    }

    /// Resolve every effective group reference, even when a pure scope or the
    /// same-authority fast path would already grant access. Roots pass None:
    /// their outgoing visibility dependencies are validated, not access-checked.
    fn visible(
        &mut self,
        visibility: &Visibility,
        declaring: &PackageId,
        requesting: Option<&PackageId>,
    ) -> Result<bool, String> {
        let mut group_grant = false;
        for label in visibility.group_labels() {
            let node = self.resolve_group(label, &mut Vec::new())?;
            group_grant |= requesting.is_some_and(|package| node.contains(package));
        }
        Ok(
            requesting
                .is_none_or(|package| visibility.allows_in(declaring, package) || group_grant),
        )
    }

    /// Analyze one target in configuration `config`, in Bazel's per-target
    /// order (ConfiguredTargetFunction / PrerequisiteProducer): load-time gates
    /// and pre-dependency gaps, configuration conditions in the same
    /// configuration (ConfigConditionsProducer), attribute validation
    /// (ConfiguredAttributeMapper.validateAttributes), gaps of the resolved
    /// attribute values, then dependencies from resolved values only.
    fn analyze(
        &mut self,
        label: &Label,
        config: ConfigId,
        active: &mut Vec<ConfiguredTargetKey>,
        configured: &mut ConfiguredNodes,
        plan: &mut Plan,
    ) -> Result<(), String> {
        let rule = match self.target(label)? {
            Target::Source(_) => {
                self.visible(&self.source_visibility(label), &label.package_id(), None)?;
                return Ok(());
            }
            Target::Rule(rule) => rule,
            Target::Output(rule, _) => {
                check_macro_namespace(&rule, label)?;
                rule
            }
        };
        // Check the requested target and its generator before memoization, but
        // never scan unrelated outputs or impose this analysis gate on query.
        check_macro_namespace(&rule, &rule.label)?;
        self.visible(&rule.visibility, &rule_authority(&rule), None)?;
        if matches!(rule.kind, RuleKind::PackageGroup(_)) {
            self.resolve_group(&rule.label, &mut Vec::new())?;
        }
        // A rule transition decides the configuration everything else is
        // evaluated in (TargetAndConfigurationProducer); it is not modelled.
        if let Some(custom) = &rule.custom
            && !custom.declaration.transition_gaps.is_empty()
        {
            return Err(custom_gap_error(&rule, &custom.declaration.transition_gaps));
        }
        let key = ConfiguredTargetKey::new(rule.label.clone(), config);
        if configured.contains_key(&key) {
            return Ok(());
        }
        if let Some(start) = active.iter().position(|active| *active == key) {
            return Err(self.cycle_error(&rule, &active[..start], &active[start..]));
        }
        if active.len() >= 256 {
            return Err("native prototype dependency depth limit (256) exceeded".into());
        }
        let configuration = self.configurations.get(config).clone();
        active.push(key.clone());
        // constraint_setting and constraint_value are analyzed under
        // NoConfigTransition (rules/platform/ConstraintSettingRule.java:42,
        // ConstraintValueRule.java:41): select() in their configurable
        // attributes would be evaluated in the no-op configuration, which is
        // not modelled.
        if let RuleKind::Native(kind @ ("constraint_setting" | "constraint_value")) = rule.kind
            && !configured_attributes::rule_config_dependencies(&rule).is_empty()
        {
            return Err(format!(
                "{}: select() in {kind} attributes is not implemented in the native profile (Bazel analyzes {kind} targets in the no-op configuration, NoConfigTransition)",
                rule.label
            ));
        }
        // DependencyContextProducerWithCompatibilityCheck
        // (analysis/producers/DependencyContextProducerWithCompatibilityCheck.java:80-110, 132-145):
        // a rule that uses toolchain resolution resolves its target platform
        // before its select() conditions. A non-default target platform is
        // resolved so; the default host platform only where a constraint
        // needs it (constraint_value keys in config_conditions,
        // config_setting constraint_values matching below; docs/PLATFORMS.md).
        // Inside a platform's tree (the empty configuration, no
        // PlatformConfiguration) no rule resolves toolchains
        // (ToolchainContextUtil.java:79-82: platformConfig == null).
        let in_empty_configuration = !self.resolving_platforms.is_empty();
        if in_empty_configuration {
            self.empty_configuration_rules.insert(key.clone());
        }
        let uses_toolchains = uses_toolchain_resolution(&rule) && !in_empty_configuration;
        let mut target_platform = None;
        if uses_toolchains && !self.default_target_platform(config) {
            target_platform = Some(self.target_platform(config, configured, plan)?);
        }
        // PrerequisiteProducer order (analysis/producers/PrerequisiteProducer.java):
        // select() conditions, then toolchain resolution, then dependency
        // resolution (attribute validation, dependencies), then the factory.
        let ordered = self.config_conditions(&rule, config, active, configured, plan)?;
        if uses_toolchains {
            self.check_execution_platform_registrations()?;
        }
        if let Some(custom) = &rule.custom {
            // Exec groups and automatic exec groups (a `_use_auto_exec_groups`
            // attribute that is True, packages/RuleClass.java:179-203; the
            // --incompatible_auto_exec_groups default is false,
            // analysis/config/CoreOptions.java:419-432) are not modelled.
            let mut gaps = custom.declaration.toolchain_gaps.clone();
            if matches!(
                custom.attrs.get("_use_auto_exec_groups"),
                Some(crate::custom_rule::AttrValue::Bool(true))
            ) {
                gaps.push(
                    "attribute '_use_auto_exec_groups' = True (automatic exec groups)".to_owned(),
                );
            }
            if !gaps.is_empty() {
                return Err(custom_gap_error(&rule, &gaps));
            }
        }
        // ToolchainResolutionFunction (skyframe/toolchains/ToolchainResolutionFunction.java:62-131).
        let toolchain_context = if uses_toolchains {
            self.resolve_toolchains(&rule, config, active, configured, plan)?
        } else {
            None
        };
        native_analysis_gate(&rule)?;
        let conditions = ordered.iter().cloned().collect::<BTreeMap<_, _>>();
        let resolver = configured_attributes::Resolver {
            target: &rule.label,
            conditions: &conditions,
            config_hash: config.to_string(),
        };
        // DependencyResolutionHelpers.visitRule reports a validation failure
        // at the rule's location (analysis/DependencyResolutionHelpers.java:309-313).
        let configured_rule = configured_attributes::configure(&rule, &resolver)
            .map_err(|error| located(rule.location.as_deref(), error))?;
        let duplicate_errors =
            configured_attributes::concatenated_duplicate_errors(&rule, &configured_rule);
        let mut rule = configured_rule;
        if let Some(custom) = &rule.custom
            && !custom.declaration.unanalyzed.is_empty()
        {
            return Err(custom_gap_error(&rule, &custom.declaration.unanalyzed));
        }
        resolved_value_gate(&rule)?;
        // An alias-like rule has one dependency, its (resolved) actual: the
        // `actual` attribute, or the late-bound `:alias` of label_flag /
        // label_setting (rules/LabelBuildSettings.java:62-83).
        let build_setting = self.build_setting_info(&rule)?;
        if forwards(&rule) {
            rule.srcs = vec![self.alias_actual(&rule, &configuration, build_setting.as_ref())?];
        }
        // Late-bound attributes skip the visibility check
        // (CommonPrerequisiteValidator.java:159-163); rules that can have any
        // provider skip the testonly check (:730-738).
        let late_bound = matches!(rule.kind, RuleKind::Native("label_flag" | "label_setting"));
        let skip_testonly = forwards(&rule);
        let requesting_package = rule_authority(&rule);
        // The configuration of each dependency edge
        // (DependencyResolutionHelpers.getExecutionPlatformLabel,
        // analysis/DependencyResolutionHelpers.java:223-253): the exec
        // transition needs the parent's execution platform, which toolchain
        // resolution chose before any dependency (ExecutionTransitionFactory).
        let edges = dependency_edges(&rule);
        let exec_config =
            if uses_toolchains && edges.iter().any(|edge| edge.cfg == EdgeCfg::Exec(None)) {
                let exec_config = self.exec_configuration(
                    &rule,
                    config,
                    toolchain_context.as_ref(),
                    configured,
                    plan,
                )?;
                self.exec_configurations.insert(key.clone(), exec_config);
                exec_config
            } else {
                // No execution platform: the exec transition is a no-op
                // (ExecutionTransitionFactory.java:194-197).
                config
            };
        for edge in edges {
            let dep_config = match &edge.cfg {
                EdgeCfg::Target => config,
                EdgeCfg::Exec(None) => exec_config,
                // Named exec groups (exec_groups fail before, so the group
                // never exists): a dependency error when the rule has
                // toolchain contexts (DependencyResolutionHelpers.java:243-253,
                // DependencyProducer.ExecGroupErrorEmitter), otherwise no
                // execution platform and a no-op transition.
                EdgeCfg::Exec(Some(group)) => {
                    if uses_toolchains {
                        return Err(located(
                            rule.location.as_deref(),
                            format!(
                                "Attr '{}' declares a transition for non-existent exec group '{group}'",
                                edge.attribute
                            ),
                        ));
                    }
                    config
                }
            };
            let src = edge.label;
            let dependency = self.resolve_rule_src(&rule, &src)?;
            let target = self
                .target(&dependency)
                .map_err(|error| self.missing_edge_error(&rule, &dependency, error))?;
            let is_source = matches!(target, Target::Source(_));
            let forwarding = matches!(&target, Target::Rule(dep) if forwards(dep));
            let visible = match target {
                Target::Rule(dep) | Target::Output(dep, _) => {
                    check_macro_namespace(&dep, &dependency)?;
                    check_macro_namespace(&dep, &dep.label)?;
                    if matches!(dep.kind, RuleKind::PackageGroup(_))
                        && !matches!(rule.kind, RuleKind::Custom)
                    {
                        return Err(format!(
                            "{}: package group '{dependency}' is misplaced in {}",
                            rule.label, edge.attribute
                        ));
                    }
                    late_bound
                        || self.visible(
                            &dep.visibility,
                            &rule_authority(&dep),
                            Some(&requesting_package),
                        )?
                }
                Target::Source(_) => {
                    late_bound
                        || self.visible(
                            &self.source_visibility(&dependency),
                            &dependency.package_id(),
                            Some(&requesting_package),
                        )?
                }
            };
            if !forwarding {
                if !visible {
                    return Err(visibility_error(
                        &rule,
                        &describe_with_aliases(&[], &dependency, "target"),
                        is_source,
                    ));
                }
                self.validate_prerequisite(
                    &rule,
                    &dependency,
                    &describe_with_aliases(&[], &dependency, "target"),
                    skip_testonly,
                )?;
                self.analyze(&dependency, dep_config, active, configured, plan)?;
                continue;
            }
            // The configured alias (its actual's label and kind) is needed to
            // describe and validate the edge (AliasProvider.describeTargetWithAliases).
            self.analyze(&dependency, dep_config, active, configured, plan)?;
            let (chain, actual) = self.alias_chain(configured, dep_config, &dependency);
            let description = describe_with_aliases(&chain, &actual, "target");
            if !visible {
                let is_source = matches!(self.target(&actual)?, Target::Source(_));
                return Err(visibility_error(&rule, &description, is_source));
            }
            self.validate_prerequisite(&rule, &actual, &description, skip_testonly)?;
        }
        // The resolved toolchain implementations are dependencies after the
        // attribute dependencies (DependencyResolutionHelpers.java:358-373),
        // analyzed in the parent's configuration (DependencyProducer.java:160-175,
        // DependencyResolver.java:835-866), each once.
        if let Some(context) = &toolchain_context {
            for implementation in context.resolved_labels() {
                self.toolchain_implementations
                    .insert(ConfiguredTargetKey::new(implementation.clone(), config));
                self.target(&implementation)
                    .map_err(|error| self.missing_edge_error(&rule, &implementation, error))?;
                self.analyze(&implementation, config, active, configured, plan)?;
            }
        }
        self.starlark_file_type_errors(&rule, config, exec_config, configured)?;
        // config_setting flag_values/constraint_values and the platform rules'
        // label attributes: prerequisites analyzed, then validated
        // (RuleContext.Builder, analysis/RuleContext.java:1700-1745, 1845-1880, 2165-2172).
        let (flag_prerequisites, constraint_prerequisites) =
            self.config_setting_prerequisites(&rule, config, active, configured, plan)?;
        let platform_prerequisites =
            self.platform_rule_prerequisites(&rule, config, active, configured, plan)?;
        // RuleContext.Builder.validateExtraPrerequisites first checks resolved
        // label lists for duplicates (analysis/RuleContext.java:2110-2113,
        // 1553-1568), attribute errors at the rule's location.
        if !duplicate_errors.is_empty() {
            return Err(duplicate_errors
                .iter()
                .map(|error| located(rule.location.as_deref(), error.clone()))
                .collect::<Vec<_>>()
                .join("\n"));
        }
        // RuleContext.Builder.validateExtraPrerequisites validates the config
        // conditions like direct prerequisites, after dependency analysis
        // (analysis/RuleContext.java:2104-2152). Under the DEFAULT_PUBLIC
        // policy the validated prerequisite is the END of an alias chain
        // (`condition.fromConfiguredTargetNoCheck(getActual())`, :2148-2150;
        // AliasConfiguredTarget.getActual, rules/AliasConfiguredTarget.java:229-233):
        // the alias's own visibility is skipped and the message names the
        // config_setting without an alias description.
        for (condition, _) in &ordered {
            let dependency = self.resolve_rule_src(&rule, condition)?;
            let (_, actual) = self.alias_chain(configured, config, &dependency);
            if let Target::Rule(dep) = self.target(&actual)? {
                let description = describe_with_aliases(&[], &actual, "target");
                if !self.visible(
                    &dep.visibility,
                    &rule_authority(&dep),
                    Some(&requesting_package),
                )? {
                    return Err(visibility_error(&rule, &description, false));
                }
                self.validate_prerequisite(&rule, &actual, &description, skip_testonly)?;
            }
        }
        // ConfigSetting.create (rules/config/ConfigSetting.java:111-166).
        let mut matching = match (&rule.kind, &rule.native) {
            (RuleKind::Native("config_setting"), Some(native)) => {
                let repositories = self.repositories.clone();
                let context = rule.label.clone();
                let run = |configuration: &Configuration, platform: Option<&PlatformInfo>| {
                    crate::config_setting::analyze(
                        &rule.label,
                        native,
                        configuration,
                        &flag_prerequisites,
                        &constraint_prerequisites,
                        platform,
                        &|text| repositories.parse(text, &context.repository, &context.package),
                    )
                };
                let mut result = run(&configuration, target_platform.as_deref());
                if result.as_ref().err().map(String::as_str)
                    == Some(crate::config_setting::NEEDS_TARGET_PLATFORM)
                {
                    let platform = self.target_platform(config, configured, plan)?;
                    result = run(&configuration, Some(&platform));
                }
                Some(result.map_err(|error| located_lines(rule.location.as_deref(), &error))?)
            }
            _ => None,
        };
        // ConstraintSetting/ConstraintValue/Platform.create (rules/platform/*.java).
        let (mut constraint_setting, mut constraint_value, mut platform) =
            self.platform_rule_providers(&rule, config, &platform_prerequisites, configured)?;
        // ToolchainType.create / Toolchain.create (rules/ToolchainType.java:40-52,
        // rules/platform/Toolchain.java:42-110).
        let (mut toolchain_type, mut declared_toolchain) =
            self.toolchain_rule_providers(&rule, config, &platform_prerequisites, configured)?;
        // AliasConfiguredTarget forwards the actual's providers; label_flag and
        // label_setting override BuildSettingProvider (LateBoundAlias.java:58-79).
        let mut actual = None;
        let mut build_setting = build_setting;
        if forwards(&rule) {
            let target = self.resolve_rule_src(&rule, &rule.srcs[0])?;
            let forwarded = configured
                .get(&ConfiguredTargetKey::new(target.clone(), config))
                .cloned()
                .unwrap_or_default();
            matching = forwarded.matching;
            if build_setting.is_none() {
                build_setting = forwarded.build_setting;
            }
            constraint_setting = forwarded.constraint_setting;
            constraint_value = forwarded.constraint_value;
            platform = forwarded.platform;
            toolchain_type = forwarded.toolchain_type;
            declared_toolchain = forwarded.declared_toolchain;
            actual = Some(target);
        }
        // ctx.build_setting_value of a Starlark build setting.
        if let (Some(info), Some(custom)) = (&build_setting, rule.custom.as_mut()) {
            custom.setting_value = Some(info.value(&configuration));
        }
        active.pop();
        if let Some(context) = toolchain_context {
            self.toolchain_contexts.insert(key.clone(), context);
        }
        configured.insert(
            key,
            ConfiguredNode {
                matching,
                build_setting,
                actual,
                constraint_setting,
                constraint_value,
                platform,
                toolchain_type,
                declared_toolchain,
            },
        );
        plan.push((config, rule));
        Ok(())
    }

    /// The configuration of `rule`'s exec-transition edges (its default exec
    /// group): the exec transition (crate::config_registry::exec_transition)
    /// for the execution platform chosen by its toolchain resolution — the
    /// host platform, the only execution platform (docs/TOOLCHAINS.md),
    /// resolved here when toolchain resolution needed no platform (the lazy
    /// default host platform, docs/PLATFORMS.md).
    fn exec_configuration(
        &mut self,
        rule: &Rule,
        config: ConfigId,
        context: Option<&UnloadedToolchainContext>,
        configured: &mut ConfiguredNodes,
        plan: &mut Plan,
    ) -> Result<ConfigId, String> {
        let platform = match context {
            Some(context) => context.exec_platform.label.clone(),
            None => {
                let host = self.configurations.get(config).native.host_platform.clone();
                self.platform_info(&host, config, configured, plan)
                    .map_err(|error| self.platform_resolution_error(rule, config, &host, error))?
                    .label
                    .clone()
            }
        };
        let target_scoped = self.target_scoped_flags()?;
        // An invalid exec output directory name fails the dependency's
        // configuration (InvalidMnemonicException), reported at the rule.
        self.configurations
            .intern_exec(config, platform, &target_scoped)
            .map_err(|error| located(rule.location.as_deref(), error))
    }

    /// The top-level Starlark flags whose rule explicitly sets
    /// `scope = "target"` (Scope.ScopeType.TARGET, analysis/config/Scope.java:31-46).
    fn target_scoped_flags(&mut self) -> Result<BTreeSet<Label>, String> {
        if let Some(flags) = &self.target_scoped_flags {
            return Ok(flags.clone());
        }
        let mut flags = BTreeSet::new();
        let labels = self
            .configurations
            .top_configuration()
            .starlark
            .keys()
            .cloned()
            .collect::<Vec<_>>();
        for label in labels {
            if let Target::Rule(rule) = self.target(&label)?
                && self
                    .build_setting_info(&rule)?
                    .is_some_and(|info| info.scope.is_target())
            {
                flags.insert(label);
            }
        }
        self.target_scoped_flags = Some(flags.clone());
        Ok(flags)
    }

    /// A missing dependency edge (missing_edge), preceded by the target
    /// loading event when the target belongs to another package.
    fn missing_edge_error(&self, rule: &Rule, dependency: &Label, error: String) -> String {
        let event = self.foreign_target_event(&rule.label.package_id(), dependency, &error);
        let edge = missing_edge(rule, dependency, error);
        match event {
            Some(event) => format!("{event}\n{edge}"),
            None => edge,
        }
    }

    /// Bazel reports a missing target of another package first as that
    /// package's loading event, located at its BUILD file
    /// (`/ws/p/BUILD.bazel: no such target '//p:x': ...`, an event of the
    /// target lookup, as observed in the paired captures N16b
    /// A10/missing_target_edge and N16c B2/lx_use_cv_missing), before the
    /// consumer's edge error.
    fn foreign_target_event(
        &self,
        consumer: &PackageId,
        dependency: &Label,
        error: &str,
    ) -> Option<String> {
        if dependency.package_id() == *consumer {
            return None;
        }
        self.target_loading_event(dependency, error)
    }

    /// The loading event of a missing target, at its package's BUILD file.
    fn target_loading_event(&self, dependency: &Label, error: &str) -> Option<String> {
        if !error.starts_with("no such target '") {
            return None;
        }
        let root = self.repositories.root(&dependency.repository).ok()?;
        let build = workspace::package_build_file(root, &dependency.package).ok()?;
        Some(format!("{}: {error}", build.display()))
    }

    /// AbstractLabelCycleReporter.maybeReportCycle (skyframe/AbstractLabelCycleReporter.java:81-170):
    /// `in KIND L: cycle in dependency graph:` at the location of the cycle's
    /// first element, the path to the cycle indented by four spaces, then the
    /// cycle drawn with `.-> `, `|   `, `` `-- `` (`[self-edge]` for one
    /// element). Configured target keys print as `LABEL (CONFIG)`; the native
    /// configuration is the full native ConfigId, not Bazel's BuildOptions
    /// checksum, and the single native configuration has no separate
    /// top-level key (docs/CONFIGURATION.md).
    fn cycle_error(
        &self,
        head: &Rule,
        path: &[ConfiguredTargetKey],
        cycle: &[ConfiguredTargetKey],
    ) -> String {
        let key = ConfiguredTargetKey::to_string;
        let mut message = String::from("cycle in dependency graph:");
        // The path starts at the requested target's top-level key: for a
        // test-trimmed rule, its key in the untrimmed configuration, then its
        // own (AbstractLabelCycleReporter prints every key of the path,
        // skyframe/AbstractLabelCycleReporter.java:105-114).
        if let Some(root) = &self.requested_trimmed_root
            && path.first().or(cycle.first()) == Some(root)
        {
            message.push_str(&format!(
                "\n    {} ({})",
                crate::rule_declarations::canonical_form(&root.label),
                self.configurations.get(root.config).untrimmed_id()
            ));
        }
        for element in path {
            message.push_str("\n    ");
            message.push_str(&key(element));
        }
        message.push_str("\n.-> ");
        message.push_str(&key(&cycle[0]));
        if cycle.len() == 1 {
            message.push_str(" [self-edge]\n`--");
        } else {
            for element in &cycle[1..] {
                message.push_str("\n|   ");
                message.push_str(&key(element));
            }
            message.push_str("\n`-- ");
            message.push_str(&key(&cycle[0]));
        }
        located(
            head.location.as_deref(),
            format!(
                "in {} rule {}: {message}",
                head.rule_class_name(),
                crate::rule_declarations::canonical_form(&head.label)
            ),
        )
    }

    /// The alias chain from `label` (AliasProvider.getAliasChain) and its
    /// end (AliasConfiguredTarget.getLabel, rules/AliasConfiguredTarget.java:178-185).
    fn alias_chain(
        &self,
        configured: &ConfiguredNodes,
        config: ConfigId,
        label: &Label,
    ) -> (Vec<Label>, Label) {
        let mut chain = Vec::new();
        let mut current = label.clone();
        while let Some(actual) = configured
            .get(&ConfiguredTargetKey::new(current.clone(), config))
            .and_then(|node| node.actual.clone())
        {
            if chain.contains(&current) {
                break;
            }
            chain.push(current);
            current = actual;
        }
        (chain, current)
    }

    /// The configured actual of an alias-like rule, as label text: `actual`
    /// after select() resolution, or the label_flag/label_setting value
    /// (LabelBuildSettings.ACTUAL, rules/LabelBuildSettings.java:62-83).
    fn alias_actual(
        &self,
        rule: &Rule,
        configuration: &Configuration,
        build_setting: Option<&BuildSettingInfo>,
    ) -> Result<String, String> {
        if let Some(info) = build_setting {
            return match info.value(configuration) {
                SettingValue::Label(label) => Ok(label.to_string()),
                other => Err(format!(
                    "internal error: label build setting {} has value {other}",
                    rule.label
                )),
            };
        }
        match rule
            .native
            .as_ref()
            .and_then(|native| native.attrs.get("actual"))
        {
            Some(crate::custom_rule::AttrValue::Label(Some(actual))) => Ok(actual.clone()),
            _ => Err(format!(
                "internal error: alias {} has no resolved actual",
                rule.label
            )),
        }
    }

    /// The BuildSettingProvider of a build-setting rule: Starlark rules with
    /// `build_setting` (RuleConfiguredTargetBuilder.java:250-259) and
    /// label_flag/label_setting (NODEP_LABEL, LabelBuildSettings.java:97).
    fn build_setting_info(&self, rule: &Rule) -> Result<Option<BuildSettingInfo>, String> {
        if let Some(custom) = &rule.custom
            && let Some(setting) = &custom.build_setting
        {
            return Ok(Some(BuildSettingInfo {
                label: rule.label.clone(),
                ty: SettingType::of(setting.kind),
                default: crate::analysis::setting_default(custom)
                    .map_err(|error| format!("{}: {error}", rule.label))?,
                is_flag: setting.flag,
                repeatable: setting.repeatable,
                scope: custom.declaration.explicit_scope.clone(),
            }));
        }
        let RuleKind::Native(kind @ ("label_flag" | "label_setting")) = rule.kind else {
            return Ok(None);
        };
        let Some(crate::custom_rule::AttrValue::Label(Some(default))) = rule
            .native
            .as_ref()
            .and_then(|native| native.attrs.get("build_setting_default"))
        else {
            return Err(format!(
                "internal error: {kind} {} has no build_setting_default",
                rule.label
            ));
        };
        Ok(Some(BuildSettingInfo {
            label: rule.label.clone(),
            ty: SettingType::Label,
            default: SettingValue::Label(self.resolve_rule_src(rule, default)?),
            is_flag: kind == "label_flag",
            repeatable: false,
            // The native `scope` attribute (rules/LabelBuildSettings.java:93-96)
            // is stored only when explicitly specified.
            scope: match rule
                .native
                .as_ref()
                .and_then(|native| native.attrs.get("scope"))
            {
                Some(crate::custom_rule::AttrValue::String(scope)) => {
                    crate::starlark_options::ScopeAttribute::Value(scope.clone())
                }
                _ => crate::starlark_options::ScopeAttribute::Unspecified,
            },
        }))
    }

    /// RuleContext.Builder.validateDirectPrerequisiteType for an attribute
    /// that allows no files (analysis/RuleContext.java:1845-1880): the end of
    /// the alias chain must be a rule. A missing input file is `rule 'L' does
    /// not exist` (`target` when the name has a `.`); a file is `KIND 'L' is
    /// misplaced here (expected no files)` with the alias description
    /// (RuleContext.java:1819-1830, AliasProvider.java:123-148).
    fn no_files_error(
        &mut self,
        chain: &[Label],
        actual: &Label,
    ) -> Result<Option<String>, String> {
        let misplaced = |kind: &str| {
            Some(format!(
                "{} is misplaced here (expected no files)",
                describe_with_aliases(chain, actual, kind)
            ))
        };
        Ok(match self.target(actual)? {
            Target::Source(path) if std::fs::symlink_metadata(&path).is_err() => Some(format!(
                "{} '{}' does not exist",
                if actual.name.contains('.') {
                    "target"
                } else {
                    "rule"
                },
                crate::rule_declarations::canonical_form(actual)
            )),
            Target::Source(_) => misplaced("source file"),
            Target::Output(..) => misplaced("generated file"),
            Target::Rule(_) => None,
        })
    }

    /// Starlark `attr.label`/`attr.label_list` without `allow_files` allow no
    /// files and check labels strictly (StarlarkAttrModule.java:431-445,
    /// Attribute.java:2349): every file prerequisite is an attribute error
    /// `in A attribute of KIND rule L: ...`, all reported at the rule's
    /// location, in attribute order.
    fn starlark_file_type_errors(
        &mut self,
        rule: &Rule,
        config: ConfigId,
        exec_config: ConfigId,
        configured: &ConfiguredNodes,
    ) -> Result<(), String> {
        let Some(custom) = &rule.custom else {
            return Ok(());
        };
        let shown = crate::rule_declarations::canonical_form(&rule.label);
        let mut errors = Vec::new();
        for name in &custom.declaration.attr_order {
            let (Some(spec), Some(value)) = (custom.schema.get(name), custom.attrs.get(name))
            else {
                continue;
            };
            if spec.allow_files || spec.allow_single_file || spec.declaration.file_types.is_some() {
                continue;
            }
            let labels = match value {
                crate::custom_rule::AttrValue::Label(Some(label)) => vec![label.clone()],
                crate::custom_rule::AttrValue::Labels(labels) => labels.clone(),
                crate::custom_rule::AttrValue::StringKeyedLabelDict(entries) => {
                    entries.iter().map(|(_, label)| label.clone()).collect()
                }
                crate::custom_rule::AttrValue::LabelListDict(entries) => entries
                    .iter()
                    .flat_map(|(_, labels)| labels.iter().cloned())
                    .collect(),
                _ => continue,
            };
            let edge_config = match &spec.declaration.cfg {
                crate::rule_declarations::Cfg::Exec(group)
                    if group
                        .as_deref()
                        .is_none_or(|group| group == "default-exec-group") =>
                {
                    exec_config
                }
                _ => config,
            };
            for text in labels {
                let dependency = self.resolve_rule_src(rule, &text)?;
                let (chain, actual) = self.alias_chain(configured, edge_config, &dependency);
                if let Some(error) = self.no_files_error(&chain, &actual)? {
                    errors.push(located(
                        rule.location.as_deref(),
                        format!(
                            "in {name} attribute of {} rule {shown}: {error}",
                            rule.rule_class_name()
                        ),
                    ));
                }
            }
        }
        if errors.is_empty() {
            Ok(())
        } else {
            Err(errors.join("\n"))
        }
    }

    /// The `flag_values` and `constraint_values` prerequisites of a
    /// `config_setting`: every key and label is analyzed first, then each
    /// edge is validated like RuleContext.Builder does
    /// (validateDirectPrerequisiteType, analysis/RuleContext.java:1845-1880,
    /// with no allowed file types, ConfigRuleClasses.java:292-294, 314-316;
    /// then visibility, testonly and deprecation), in attribute order. All
    /// errors are reported.
    fn config_setting_prerequisites(
        &mut self,
        rule: &Rule,
        config: ConfigId,
        active: &mut Vec<ConfiguredTargetKey>,
        configured: &mut ConfiguredNodes,
        plan: &mut Plan,
    ) -> Result<
        (
            Vec<crate::config_setting::FlagPrerequisite>,
            Vec<crate::config_setting::ConstraintPrerequisite>,
        ),
        String,
    > {
        let (RuleKind::Native("config_setting"), Some(native)) = (&rule.kind, &rule.native) else {
            return Ok((Vec::new(), Vec::new()));
        };
        let entries = match native.attrs.get("flag_values") {
            Some(crate::custom_rule::AttrValue::LabelKeyedStringDict(entries)) => entries.clone(),
            _ => Vec::new(),
        };
        let mut keys = Vec::new();
        for (key, value) in entries {
            let dependency = self.resolve_rule_src(rule, &key)?;
            self.target(&dependency)
                .map_err(|error| self.missing_edge_error(rule, &dependency, error))?;
            self.analyze(&dependency, config, active, configured, plan)?;
            keys.push(("flag_values", dependency, Some(value)));
        }
        for text in native.labels("constraint_values") {
            let dependency = self.resolve_rule_src(rule, &text)?;
            self.target(&dependency)
                .map_err(|error| self.missing_edge_error(rule, &dependency, error))?;
            self.analyze(&dependency, config, active, configured, plan)?;
            keys.push(("constraint_values", dependency, None));
        }
        let shown = crate::rule_declarations::canonical_form(&rule.label);
        let requesting_package = rule_authority(rule);
        let mut errors = Vec::new();
        let mut flags = Vec::new();
        let mut constraints = Vec::new();
        for (attribute, dependency, value) in keys {
            let (chain, actual) = self.alias_chain(configured, config, &dependency);
            let is_source = matches!(self.target(&actual)?, Target::Source(_));
            if let Some(error) = self.no_files_error(&chain, &actual)? {
                errors.push(format!(
                    "in {attribute} attribute of config_setting rule {shown}: {error}"
                ));
            }
            let visible = match self.target(&dependency)? {
                Target::Rule(dep) | Target::Output(dep, _) => self.visible(
                    &dep.visibility,
                    &rule_authority(&dep),
                    Some(&requesting_package),
                )?,
                Target::Source(_) => self.visible(
                    &self.source_visibility(&dependency),
                    &dependency.package_id(),
                    Some(&requesting_package),
                )?,
            };
            let description = describe_with_aliases(&chain, &actual, "target");
            if !visible {
                errors.push(visibility_message(rule, &description, is_source));
            }
            if let Some(error) = self.prerequisite_error(rule, &actual, &description, false)? {
                errors.push(error);
            }
            let node = configured.get(&ConfiguredTargetKey::new(dependency.clone(), config));
            match value {
                Some(value) => flags.push(crate::config_setting::FlagPrerequisite {
                    specified: dependency,
                    actual,
                    value,
                    setting: node.and_then(|node| node.build_setting.clone()),
                }),
                None => constraints.push(crate::config_setting::ConstraintPrerequisite {
                    actual,
                    value: node.and_then(|node| node.constraint_value.clone()),
                }),
            }
        }
        if !errors.is_empty() {
            return Err(errors
                .into_iter()
                .map(|error| located(rule.location.as_deref(), error))
                .collect::<Vec<_>>()
                .join("\n"));
        }
        Ok((flags, constraints))
    }

    /// The label attributes of `constraint_setting`
    /// (ConstraintSettingRule.java:84-89), `constraint_value`
    /// (ConstraintValueRule.java:56-63) and `platform` (PlatformRule.java:75-89,
    /// 149-152), in rule-class attribute order, analyzed and validated (the
    /// resolved labels per attribute).
    fn platform_rule_prerequisites(
        &mut self,
        rule: &Rule,
        config: ConfigId,
        active: &mut Vec<ConfiguredTargetKey>,
        configured: &mut ConfiguredNodes,
        plan: &mut Plan,
    ) -> Result<Vec<Vec<Label>>, String> {
        let attributes = platform_prerequisite_attributes(rule);
        if attributes.is_empty() {
            return Ok(Vec::new());
        }
        self.native_prerequisites(rule, config, &attributes, active, configured, plan)
    }

    /// RuleContext.Builder.createTargetMap for native label attributes
    /// (analysis/RuleContext.java:1708-1745, 2165-2172): every prerequisite is
    /// analyzed, then validated in attribute order — validateRuleDependency
    /// for rules (allowed rule classes, else mandatory providers, :2002-2093),
    /// the no-files check for files (:1845-1880), then visibility, testonly
    /// and deprecation. All errors are reported at the rule's location.
    fn native_prerequisites(
        &mut self,
        rule: &Rule,
        config: ConfigId,
        attributes: &[PrerequisiteAttribute],
        active: &mut Vec<ConfiguredTargetKey>,
        configured: &mut ConfiguredNodes,
        plan: &mut Plan,
    ) -> Result<Vec<Vec<Label>>, String> {
        let mut resolved = Vec::new();
        for attribute in attributes {
            let mut labels = Vec::new();
            for text in &attribute.labels {
                let dependency = self.resolve_rule_src(rule, text)?;
                self.target(&dependency)
                    .map_err(|error| self.missing_edge_error(rule, &dependency, error))?;
                self.analyze(&dependency, config, active, configured, plan)?;
                labels.push(dependency);
            }
            resolved.push(labels);
        }
        let shown = crate::rule_declarations::canonical_form(&rule.label);
        let class = rule.rule_class_name().to_owned();
        let requesting_package = rule_authority(rule);
        let mut errors = Vec::new();
        for (attribute, labels) in attributes.iter().zip(&resolved) {
            let prefix = format!("in {} attribute of {class} rule {shown}: ", attribute.name);
            for dependency in labels {
                let (chain, actual) = self.alias_chain(configured, config, dependency);
                let actual_target = self.target(&actual)?;
                let is_source = matches!(actual_target, Target::Source(_));
                let type_error = match &actual_target {
                    Target::Rule(actual_rule) => rule_dependency_error(
                        attribute,
                        &chain,
                        &actual,
                        actual_rule,
                        configured.get(&ConfiguredTargetKey::new(actual.clone(), config)),
                    ),
                    _ => self.no_files_error(&chain, &actual)?,
                };
                if let Some(error) = type_error {
                    errors.push(format!("{prefix}{error}"));
                }
                let visible = match self.target(dependency)? {
                    Target::Rule(dep) | Target::Output(dep, _) => self.visible(
                        &dep.visibility,
                        &rule_authority(&dep),
                        Some(&requesting_package),
                    )?,
                    Target::Source(_) => self.visible(
                        &self.source_visibility(dependency),
                        &dependency.package_id(),
                        Some(&requesting_package),
                    )?,
                };
                let description = describe_with_aliases(&chain, &actual, "target");
                if !visible {
                    errors.push(visibility_message(rule, &description, is_source));
                }
                if let Some(error) = self.prerequisite_error(rule, &actual, &description, false)? {
                    errors.push(error);
                }
            }
        }
        if !errors.is_empty() {
            return Err(errors
                .into_iter()
                .map(|error| located(rule.location.as_deref(), error))
                .collect::<Vec<_>>()
                .join("\n"));
        }
        Ok(resolved)
    }

    /// The providers of `constraint_setting` (rules/platform/ConstraintSetting.java:227-304),
    /// `constraint_value` (ConstraintValue.java:338-353) and `platform`
    /// (Platform.java:44-120) from their validated prerequisites.
    fn platform_rule_providers(
        &mut self,
        rule: &Rule,
        config: ConfigId,
        prerequisites: &[Vec<Label>],
        configured: &ConfiguredNodes,
    ) -> Result<PlatformProviders, String> {
        let Some(native) = rule.native.clone() else {
            return Ok((None, None, None));
        };
        let shown = crate::rule_declarations::canonical_form(&rule.label);
        let class = rule.rule_class_name().to_owned();
        let attribute_error = |attribute: &str, message: &str| {
            located(
                rule.location.as_deref(),
                format!("in {attribute} attribute of {class} rule {shown}: {message}"),
            )
        };
        let node = |label: &Label| configured.get(&ConfiguredTargetKey::new(label.clone(), config));
        let internal = |what: &str| {
            format!(
                "internal error: validated prerequisite of {} lacks {what}",
                rule.label
            )
        };
        match rule.kind {
            RuleKind::Native("constraint_setting") => {
                // validateDefaultConstraintValue (ConstraintSetting.java:270-304).
                let mut default_value = None;
                if let Some(crate::custom_rule::AttrValue::Label(Some(text))) =
                    native.attrs.get("default_constraint_value")
                {
                    let default = self.resolve_rule_src(rule, text)?;
                    if default.package_id() != rule.label.package_id() {
                        return Err(attribute_error(
                            "default_constraint_value",
                            "The default constraint value must be defined in the same package as the constraint setting itself.",
                        ));
                    }
                    if self.target(&default).is_err() {
                        return Err(attribute_error(
                            "default_constraint_value",
                            &format!(
                                "The default constraint value '{}' does not exist",
                                crate::rule_declarations::canonical_form(&default)
                            ),
                        ));
                    }
                    default_value = Some(default);
                }
                let refined = match prerequisites.first().and_then(|labels| labels.first()) {
                    Some(label) => Some(Box::new(
                        node(label)
                            .and_then(|node| node.constraint_value.clone())
                            .ok_or_else(|| internal("ConstraintValueInfo"))?,
                    )),
                    None => None,
                };
                Ok((
                    Some(ConstraintSettingInfo {
                        label: rule.label.clone(),
                        default_value,
                        refined,
                    }),
                    None,
                    None,
                ))
            }
            RuleKind::Native("constraint_value") => {
                let setting = prerequisites
                    .first()
                    .and_then(|labels| labels.first())
                    .and_then(node)
                    .and_then(|node| node.constraint_setting.clone())
                    .ok_or_else(|| internal("ConstraintSettingInfo"))?;
                Ok((
                    None,
                    Some(ConstraintValueInfo {
                        setting: Arc::new(setting),
                        label: rule.label.clone(),
                    }),
                    None,
                ))
            }
            RuleKind::Native("platform") => {
                let provided = |index: usize| prerequisites.get(index).cloned().unwrap_or_default();
                let mut constraint_values = Vec::new();
                for label in provided(0) {
                    constraint_values.push(
                        node(&label)
                            .and_then(|node| node.constraint_value.clone())
                            .ok_or_else(|| internal("ConstraintValueInfo"))?,
                    );
                }
                let mut parents = Vec::new();
                for label in provided(1) {
                    parents.push(
                        node(&label)
                            .and_then(|node| node.platform.clone())
                            .ok_or_else(|| internal("PlatformInfo"))?,
                    );
                }
                let required_settings = provided(2)
                    .iter()
                    .map(|label| self.alias_chain(configured, config, label).1)
                    .collect();
                let mut allowed_toolchain_types = Vec::new();
                for text in native.labels("allowed_toolchain_types") {
                    allowed_toolchain_types.push(self.resolve_rule_src(rule, &text)?);
                }
                let exec_properties = match native.attrs.get("exec_properties") {
                    Some(crate::custom_rule::AttrValue::StringDict(entries)) => entries.clone(),
                    _ => Vec::new(),
                };
                let attributes = crate::platforms::PlatformAttributes {
                    parents,
                    constraint_values,
                    remote_execution_properties: native
                        .string("remote_execution_properties")
                        .unwrap_or_default()
                        .to_owned(),
                    exec_properties,
                    flags: native.strings("flags"),
                    required_settings,
                    check_toolchain_types: matches!(
                        native.attrs.get("check_toolchain_types"),
                        Some(crate::custom_rule::AttrValue::Bool(true))
                    ),
                    allowed_toolchain_types,
                    missing_toolchain_error: native
                        .string("missing_toolchain_error")
                        .unwrap_or(crate::platforms::DEFAULT_MISSING_TOOLCHAIN_ERROR)
                        .to_owned(),
                };
                let mapping = self.repositories.mapping("").cloned().unwrap_or_default();
                let platform =
                    crate::platforms::create_platform(&rule.label, attributes, &|label| {
                        crate::platforms::shorthand_display_form(label, &mapping)
                    })
                    .map_err(|errors| {
                        errors
                            .iter()
                            .map(|error| attribute_error(error.attribute, &error.message))
                            .collect::<Vec<_>>()
                            .join("\n")
                    })?;
                Ok((None, None, Some(Arc::new(platform))))
            }
            _ => Ok((None, None, None)),
        }
    }

    /// The target platform of configuration `config`
    /// (PlatformConfiguration.getTargetPlatform, analysis/PlatformConfiguration.java:84-86),
    /// resolved like PlatformFunction. Loading errors carry Bazel's
    /// exception-class prefix (PlatformError::target_text).
    fn target_platform(
        &mut self,
        config: ConfigId,
        configured: &mut ConfiguredNodes,
        plan: &mut Plan,
    ) -> Result<Arc<PlatformInfo>, String> {
        let label = self
            .configurations
            .get(config)
            .native
            .target_platform()
            .clone();
        self.platform_info(&label, config, configured, plan)
            .map_err(|error| error.target_text())
    }

    /// Whether the target platform of `config` is the default host
    /// platform, which the native profile resolves only where a constraint
    /// needs it.
    fn default_target_platform(&self, config: ConfigId) -> bool {
        *self.configurations.get(config).native.target_platform()
            == crate::configuration::default_host_platform()
    }

    /// RegisteredExecutionPlatformsFunction: registered execution platforms
    /// fail explicitly (N17 keeps the host platform as the only execution
    /// platform), once per build, at the first toolchain-resolving rule.
    fn check_execution_platform_registrations(&mut self) -> Result<(), String> {
        if !self.registrations_checked {
            self.registrations_checked = true;
            if let Some(labels) = self
                .repositories
                .known_execution_platform_registrations()
                .into_iter()
                .next()
            {
                return Err(format!(
                    "register_execution_platforms({}) is not implemented in the native profile: execution platform resolution (N17)",
                    labels
                        .iter()
                        .map(|label| format!("{label:?}"))
                        .collect::<Vec<_>>()
                        .join(", ")
                ));
            }
        }
        Ok(())
    }

    /// ToolchainException.asConfiguredValueCreationException
    /// (skyframe/toolchains/ToolchainException.java:67-91) as reported by
    /// ConfiguredTargetFunction (:394-404): `While resolving toolchains for
    /// target L (CONFIG): MESSAGE` at the rule's location, after `events`
    /// (CONFIG is the native identifier, docs/PLATFORMS.md).
    fn toolchain_error(
        &self,
        rule: &Rule,
        config: ConfigId,
        events: &str,
        message: &str,
    ) -> String {
        let id = config.to_string();
        let error = located(
            rule.location.as_deref(),
            format!(
                "While resolving toolchains for target {} ({}): {message}",
                crate::rule_declarations::canonical_form(&rule.label),
                &id[..7]
            ),
        );
        if events.is_empty() {
            error
        } else {
            format!("{events}\n{error}")
        }
    }

    /// An InvalidPlatformException of PlatformKeys (PlatformKeys.java:150-166):
    /// a loading error names the label as a non-platform (paired capture
    /// B2/host_host_platform_fg_platforms_p_mac); other errors keep their
    /// reported lines and wrap the last one.
    fn platform_resolution_error(
        &self,
        rule: &Rule,
        config: ConfigId,
        label: &Label,
        error: PlatformError,
    ) -> String {
        let text = match error {
            PlatformError::Missing { .. } => crate::platforms::not_a_platform(label),
            PlatformError::Other(text) => text,
        };
        match text.rsplit_once('\n') {
            Some((events, last)) => self.toolchain_error(rule, config, events, last),
            None => self.toolchain_error(rule, config, "", &text),
        }
    }

    fn resolution_error(&self, rule: &Rule, config: ConfigId, error: ResolutionError) -> String {
        match error {
            ResolutionError::Toolchain(message) => self.toolchain_error(rule, config, "", &message),
            ResolutionError::Reported(events, message) => {
                self.toolchain_error(rule, config, &events, &message)
            }
            ResolutionError::Propagated(message) => message,
            ResolutionError::Aborted(message) => format!("{ANALYSIS_ABORTED}{message}"),
        }
    }

    /// ToolchainContextUtil.getUnloadedToolchainContextsInputs
    /// (skyframe/toolchains/ToolchainContextUtil.java:60-165): the rule
    /// class's toolchain types and execution constraints; a toolchain
    /// implementation forces its parent's execution platform.
    fn toolchain_context_key(
        &self,
        rule: &Rule,
        config: ConfigId,
    ) -> Result<ToolchainContextKey, String> {
        let mut key = ToolchainContextKey {
            forced_exec: self
                .toolchain_implementations
                .contains(&ConfiguredTargetKey::new(rule.label.clone(), config)),
            ..Default::default()
        };
        if let Some(custom) = &rule.custom {
            for (text, mandatory) in &custom.declaration.toolchain_types {
                key.types.push(ToolchainTypeRequirement {
                    label: crate::toolchains::parse_label(text)?,
                    mandatory: *mandatory,
                    ignore_if_invalid: false,
                });
            }
            // The rule class's constraints, then the target's
            // `exec_compatible_with` (an ImmutableSet, :152-164).
            let mut labels = Vec::new();
            for text in &custom.declaration.exec_compatible_with {
                labels.push(Label::parse_canonical(text)?);
            }
            for text in custom
                .declaration
                .builtin
                .target
                .labels("exec_compatible_with")
            {
                labels.push(self.resolve_rule_src(rule, &text)?);
            }
            for label in labels {
                if !key.exec_constraints.contains(&label) {
                    key.exec_constraints.push(label);
                }
            }
        }
        Ok(key)
    }

    /// ToolchainResolutionFunction.compute (skyframe/toolchains/ToolchainResolutionFunction.java:62-131)
    /// for a toolchain-resolving rule: PlatformKeys (the target platform,
    /// then the host platform as the only execution platform, filtered by
    /// the rule's execution constraints; PlatformKeys.java:78-224), the
    /// toolchain types (ToolchainTypeLookupUtil.java:44-170), the registered
    /// toolchains and the per-type resolution. The default host platform is
    /// resolved only when the rule has toolchain types or execution
    /// constraints (docs/TOOLCHAINS.md); a non-default `--host_platform` is
    /// always validated.
    fn resolve_toolchains(
        &mut self,
        rule: &Rule,
        config: ConfigId,
        active: &mut Vec<ConfiguredTargetKey>,
        configured: &mut ConfiguredNodes,
        plan: &mut Plan,
    ) -> Result<Option<UnloadedToolchainContext>, String> {
        let key = self.toolchain_context_key(rule, config)?;
        let configuration = self.configurations.get(config).clone();
        let host = configuration.native.host_platform.clone();
        let target_label = configuration.native.target_platform().clone();
        let default_host = host == crate::configuration::default_host_platform();
        if !key.needs_platforms() {
            if default_host {
                return Ok(None);
            }
            let host_info = self
                .platform_info(&host, config, configured, plan)
                .map_err(|error| self.platform_resolution_error(rule, config, &host, error))?;
            // PlatformKeys.isPlatformSuitable (:265-275): without toolchain
            // types a platform that checks toolchain types is unsuitable,
            // unless forced.
            if host_info.check_toolchain_types && !key.forced_exec {
                let target = self
                    .platform_info(&target_label, config, configured, plan)
                    .map_err(|error| {
                        self.platform_resolution_error(rule, config, &target_label, error)
                    })?;
                return Err(self.toolchain_error(
                    rule,
                    config,
                    "",
                    &crate::toolchains::no_matching_platform(&[], &target.label, &[host_info]),
                ));
            }
            return Ok(None);
        }
        let target = self
            .platform_info(&target_label, config, configured, plan)
            .map_err(|error| self.platform_resolution_error(rule, config, &target_label, error))?;
        let host_info = self
            .platform_info(&host, config, configured, plan)
            .map_err(|error| self.platform_resolution_error(rule, config, &host, error))?;
        // PlatformKeys.filterExecutionPlatforms (:168-205).
        let mut constraints = Vec::new();
        for label in &key.exec_constraints {
            let value = self
                .exec_constraint(rule, label, config, active, configured, plan)
                .map_err(|error| self.resolution_error(rule, config, error))?;
            constraints.push(value);
        }
        let exec = if host_info
            .constraints
            .contains_all(&constraints.iter().collect::<Vec<_>>())
        {
            vec![host_info.clone()]
        } else {
            Vec::new()
        };
        let mut lookups = Vec::new();
        for requirement in &key.types {
            let info = self
                .toolchain_type_info(rule, requirement, config, active, configured, plan)
                .map_err(|error| self.resolution_error(rule, config, error))?;
            lookups.push((requirement.clone(), info));
        }
        let types = crate::toolchains::resolved_types(&lookups);
        let requested = crate::toolchains::requested_types(&lookups);
        // SingleToolchainResolutionFunction reads RegisteredToolchainsValue
        // only for a type to resolve (:72-84).
        let registered = if types.is_empty() {
            Arc::new(RegisteredToolchains::default())
        } else {
            if self
                .computing_toolchains
                .iter()
                .any(|computing| computing.config == config)
                && !self.registered_toolchains.contains_key(&config)
            {
                return Err(self.toolchain_cycle(
                    config,
                    active,
                    &key,
                    &types[0].requirement.label,
                ));
            }
            self.registered_toolchains(config, configured, plan)
                .map_err(|error| self.resolution_error(rule, config, error))?
        };
        // PlatformKeys.find (:230-245): the forced platform is the target
        // platform or one of the execution platforms.
        let forced = key.forced_exec.then(|| host_info.clone()).filter(|forced| {
            forced.label == target.label || exec.iter().any(|exec| exec.label == forced.label)
        });
        crate::toolchains::resolve(
            &types,
            requested,
            crate::toolchains::Platforms {
                target: &target,
                exec: &exec,
                forced,
            },
            &registered,
        )
        .map(Some)
        .map_err(|message| self.toolchain_error(rule, config, "", &message))
    }

    /// RegisteredToolchainsCycleReporter.maybeReportCycle
    /// (skyframe/toolchains/RegisteredToolchainsCycleReporter.java:61-139)
    /// with AbstractLabelCycleReporter.printCycle (:134-170): a registered
    /// toolchain target whose analysis (`active`, from the registered target
    /// to the rule) needs the registered toolchains. The cycle is
    /// RegisteredToolchains, the configured targets, the rule's toolchain
    /// context key (`toolchain types ...`) and its first type's single
    /// resolution (`toolchain type T`); reported without location.
    fn toolchain_cycle(
        &mut self,
        config: ConfigId,
        active: &[ConfiguredTargetKey],
        key: &ToolchainContextKey,
        first_type: &Label,
    ) -> String {
        let shown = crate::rule_declarations::canonical_form;
        let head = active
            .first()
            .map(|key| shown(&key.label))
            .unwrap_or_default();
        let mut report = format!(
            "Misconfigured toolchains: {head} is declared as a toolchain but has inappropriate dependencies. Declared toolchains should be created with the 'toolchain' rule and should not have dependencies that themselves require toolchains.\n.-> RegisteredToolchains"
        );
        for element in active {
            report.push_str("\n|   ");
            report.push_str(&shown(&element.label));
        }
        report.push_str(&format!(
            "\n|   toolchain types {}\n|   toolchain type {}\n`-- RegisteredToolchains",
            key.types
                .iter()
                .map(|requirement| shown(&requirement.label))
                .collect::<Vec<_>>()
                .join(", "),
            shown(first_type)
        ));
        self.toolchain_cycle_reports.insert(config, report.clone());
        report
    }

    /// A target loaded for toolchain resolution: a loading error is the
    /// NoSuchThingException itself (`com.google...NoSuchTargetException:
    /// ...`, ToolchainTypeLookupUtil.java:139-144,
    /// ConstraintValueLookupUtil.java:101-105), preceded by the loading event
    /// of another package's BUILD file.
    fn resolution_target(&mut self, rule: &Rule, label: &Label) -> Result<(), ResolutionError> {
        // An unresolvable apparent repository (crate::toolchains::deferred_label_text):
        // the BuildFileNotFoundException of its package, also reported as an
        // event without location (paired capture B4/resolution_errors/unknown_repo).
        if let Some((name, requester)) =
            crate::toolchains::unknown_repository_parts(&label.repository)
        {
            let message = format!(
                "no such package '@@{repository}//{package}': The repository '@@{repository}' could not be resolved: No repository visible as '@{name}' from {context}",
                repository = label.repository,
                package = label.package,
                context = if requester.is_empty() {
                    "main repository".to_owned()
                } else {
                    format!("repository '@@{requester}'")
                }
            );
            return Err(ResolutionError::Reported(
                message.clone(),
                format!(
                    "com.google.devtools.build.lib.packages.BuildFileNotFoundException: {message}"
                ),
            ));
        }
        self.target(label).map(|_| ()).map_err(|error| {
            // The loading event is reported for the rule's own package too
            // (paired capture A5/resolution_errors/missing_type).
            let _ = rule;
            let event = self.target_loading_event(label, &error);
            let message = PlatformError::loading(error).target_text();
            match event {
                Some(event) => ResolutionError::Reported(event, message),
                None => ResolutionError::Toolchain(message),
            }
        })
    }

    /// ConstraintValueLookupUtil.findConstraintValueInfo
    /// (skyframe/toolchains/ConstraintValueLookupUtil.java:62-92).
    fn exec_constraint(
        &mut self,
        rule: &Rule,
        label: &Label,
        config: ConfigId,
        active: &mut Vec<ConfiguredTargetKey>,
        configured: &mut ConfiguredNodes,
        plan: &mut Plan,
    ) -> Result<ConstraintValueInfo, ResolutionError> {
        self.resolution_target(rule, label)?;
        self.analyze(label, config, active, configured, plan)
            .map_err(ResolutionError::Propagated)?;
        let node = configured.get(&ConfiguredTargetKey::new(label.clone(), config));
        node.and_then(|node| node.constraint_value.clone())
            .ok_or_else(|| {
                ResolutionError::Toolchain(crate::toolchains::not_a_constraint_value(
                    &self.alias_chain(configured, config, label).1,
                ))
            })
    }

    /// ToolchainTypeLookupUtil.findToolchainTypeInfo
    /// (skyframe/toolchains/ToolchainTypeLookupUtil.java:97-148): the type
    /// target is analyzed in the rule's configuration; `None` for an invalid
    /// type that is ignored.
    fn toolchain_type_info(
        &mut self,
        rule: &Rule,
        requirement: &ToolchainTypeRequirement,
        config: ConfigId,
        active: &mut Vec<ConfiguredTargetKey>,
        configured: &mut ConfiguredNodes,
        plan: &mut Plan,
    ) -> Result<Option<ToolchainTypeInfo>, ResolutionError> {
        let label = &requirement.label;
        self.resolution_target(rule, label)?;
        self.analyze(label, config, active, configured, plan)
            .map_err(ResolutionError::Propagated)?;
        let node = configured.get(&ConfiguredTargetKey::new(label.clone(), config));
        if let Some(info) = node.and_then(|node| node.toolchain_type.clone()) {
            return Ok(Some(info));
        }
        if requirement.ignore_if_invalid {
            return Ok(None);
        }
        let is_toolchain = node.is_some_and(|node| node.declared_toolchain.is_some());
        Err(ResolutionError::Toolchain(
            crate::toolchains::invalid_toolchain_type(
                &self.alias_chain(configured, config, label).1,
                is_toolchain,
            ),
        ))
    }

    /// RegisteredToolchainsFunction.compute (skyframe/toolchains/RegisteredToolchainsFunction.java:64-147),
    /// once per configuration (D1: every registered toolchain is analyzed).
    fn registered_toolchains(
        &mut self,
        config: ConfigId,
        configured: &mut ConfiguredNodes,
        plan: &mut Plan,
    ) -> Result<Arc<RegisteredToolchains>, ResolutionError> {
        if let Some(cached) = self.registered_toolchains.get(&config) {
            return cached.clone();
        }
        if let Some(key) = self
            .computing_toolchains
            .iter()
            .find(|computing| computing.config == config)
        {
            // Reached only through resolve_toolchains, which reports the
            // cycle itself (Self::toolchain_cycle).
            return Err(ResolutionError::Propagated(format!(
                "internal error: registered toolchains needed while analyzing {}",
                crate::rule_declarations::canonical_form(&key.label)
            )));
        }
        // Set to each registered target while it is analyzed.
        self.computing_toolchains.push(ConfiguredTargetKey::new(
            Label {
                repository: String::new(),
                package: String::new(),
                name: String::new(),
            },
            config,
        ));
        let result = self.compute_registered_toolchains(config, configured, plan);
        self.computing_toolchains.pop();
        self.registered_toolchains.insert(config, result.clone());
        result
    }

    fn compute_registered_toolchains(
        &mut self,
        config: ConfigId,
        configured: &mut ConfiguredNodes,
        plan: &mut Plan,
    ) -> Result<Arc<RegisteredToolchains>, ResolutionError> {
        // --extra_toolchains, reversed so the last one wins, parsed with the
        // main repository mapping (:82-93); then the modules of the
        // dependency graph (:96-104, 149-172).
        let mut patterns = self
            .configurations
            .get(config)
            .native
            .extra_toolchains
            .iter()
            .rev()
            .map(|pattern| (String::new(), pattern.clone()))
            .collect::<Vec<_>>();
        for (repository, labels) in self
            .repositories
            .toolchain_registrations()
            .map_err(ResolutionError::Propagated)?
        {
            patterns.extend(labels.into_iter().map(|label| (repository.clone(), label)));
        }
        let labels = self.expand_toolchain_patterns(&patterns)?;
        // configureRegisteredToolchains (:174-215).
        let mut declared = Vec::new();
        // Of several registered targets in a toolchain cycle, Bazel reports
        // the last one (paired capture B4/resolution_errors/extra_cyc_all_o:
        // `//cyc:all` names //cyc:type_is_user, not //cyc:exec_user).
        let mut cycle = None;
        let mut analyzed = Vec::new();
        for label in labels {
            if let Some(computing) = self.computing_toolchains.last_mut() {
                computing.label = label.clone();
            }
            match self.analyze(&label, config, &mut Vec::new(), configured, plan) {
                Ok(()) => analyzed.push(label),
                Err(error) => match self.toolchain_cycle_reports.remove(&config) {
                    Some(report) if error.contains(&report) => cycle = Some(report),
                    _ => return Err(ResolutionError::Propagated(error)),
                },
            }
        }
        if let Some(report) = cycle {
            return Err(ResolutionError::Propagated(report));
        }
        for label in analyzed {
            let info = configured
                .get(&ConfiguredTargetKey::new(label.clone(), config))
                .and_then(|node| node.declared_toolchain.clone())
                .ok_or_else(|| {
                    ResolutionError::Toolchain(crate::toolchains::invalid_registered_toolchain(
                        &crate::rule_declarations::canonical_form(&label),
                        "target does not provide the DeclaredToolchainInfo provider",
                    ))
                })?;
            declared.push(info);
        }
        // ConfigMatchingUtil.validate per toolchain (:120-141).
        let mut toolchains = Vec::new();
        for info in declared {
            match crate::toolchains::target_settings_match(&info.target_label, &info.settings) {
                Ok(true) => toolchains.push(info),
                Ok(false) => {}
                Err(message) => {
                    return Err(ResolutionError::Toolchain(
                        crate::toolchains::invalid_registered_toolchain(
                            &crate::rule_declarations::canonical_form(&info.target_label),
                            &message,
                        ),
                    ));
                }
            }
        }
        Ok(Arc::new(RegisteredToolchains { toolchains }))
    }

    /// TargetPatternUtil.expandTargetPatterns (skyframe/TargetPatternUtil.java:44-87)
    /// with FilteringPolicies.ruleTypeExplicit("toolchain")
    /// (pkgcache/FilteringPolicies.java:45-47): patterns in order, the first
    /// copy of a label kept; wildcards keep only `toolchain` rules (manual
    /// tags are not filtered), explicit labels are kept whatever they are.
    /// `:all` lists a package's targets by name; `/...` visits packages in
    /// the RecursivePkgFunction order (sorted directory entries, each
    /// directory's subpackages before itself: a STABLE_ORDER nested set
    /// iterates postorder, collect/nestedset/NestedSet.java:190-230,
    /// skyframe/RecursivePkgFunction.java:88-116).
    fn expand_toolchain_patterns(
        &mut self,
        patterns: &[(String, String)],
    ) -> Result<Vec<Label>, ResolutionError> {
        let mut labels: Vec<Label> = Vec::new();
        for (repository, pattern) in patterns {
            let invalid = |message: String| {
                ResolutionError::Toolchain(crate::toolchains::invalid_registered_toolchain(
                    pattern, &message,
                ))
            };
            if pattern.starts_with('-') {
                return Err(ResolutionError::Propagated(format!(
                    "negative toolchain registration pattern {pattern:?} is not implemented in the native profile"
                )));
            }
            // An unknown apparent repository fails the whole analysis, not
            // the toolchain resolution (paired capture
            // B1/extra_extra_toolchains_nope_x): `Analysis of target 'L'
            // failed; build aborted: No repository visible as '@R' from main
            // repository` (RepositoryFetchFunction.java:149-153,
            // RepositoryName.getContextRepoDisplayString, :251-258).
            let parse = |this: &Self, text: &str| {
                this.repositories
                    .parse(text, repository, "")
                    .map_err(
                        |error| match unknown_apparent_repository(text, repository, &error) {
                            Some(message) => ResolutionError::Aborted(message),
                            None => invalid(error),
                        },
                    )
            };
            let (base, wildcard) = match pattern.rsplit_once(':') {
                Some((base, "all")) => (base, true),
                // FilteringPolicies.RuleTypeFilter.shouldRetain
                // (pkgcache/FilteringPolicies.java:120) dereferences the
                // associated rule of every target, so Bazel 9.3 crashes with
                // a NullPointerException on the package's file targets (paired
                // capture B1/extra_extra_toolchains_names, exit 37).
                Some((_, kind @ ("*" | "all-targets"))) => {
                    return Err(ResolutionError::Propagated(format!(
                        "toolchain registration pattern '{pattern}' (the :{kind} wildcard) is not implemented in the native profile: Bazel 9.3 crashes expanding it (NullPointerException in FilteringPolicies.RuleTypeFilter.shouldRetain, pkgcache/FilteringPolicies.java:120)"
                    )));
                }
                _ => (pattern.as_str(), false),
            };
            let found = if base == "..." || base.ends_with("/...") {
                // TargetPattern.TargetsBelowDirectory: `//...` and `@r//...`
                // keep their `//`, other directories drop the trailing `/`
                // (as select_filtered).
                let prefix = base.strip_suffix("...").unwrap_or_default();
                let directory = if prefix.is_empty() || prefix.ends_with("//") {
                    prefix
                } else {
                    prefix.strip_suffix('/').unwrap_or(prefix)
                };
                let context = parse(self, &format!("{directory}:__pattern__"))?;
                let ignored = self.ignored(&context.repository).map_err(invalid)?;
                // A directory inside an ignored directory warns and yields no
                // targets (TargetPattern.TargetsBelowDirectory.eval; the
                // warning of select_filtered, docs/IGNORED_DIRECTORIES.md).
                if let Some(entry) = ignored.matching_entry(&context.package) {
                    self.warnings.push(format!(
                        "Pattern '{pattern}' was filtered out by ignored directory '{entry}'"
                    ));
                    continue;
                }
                let ignored = ignored.filter_for_directory(&context.package);
                let root = self
                    .repositories
                    .root(&context.repository)
                    .map_err(invalid)?
                    .to_path_buf();
                let mut packages = if root.join(&context.package).is_dir() {
                    workspace::discover_packages(&root, &context.package, &ignored)
                        .map_err(invalid)?
                } else {
                    Vec::new()
                };
                // RecursivePackageProviderBackedTargetPatternResolver.java:326-329.
                if packages.is_empty() {
                    return Err(invalid(format!(
                        "no targets found beneath '{}'",
                        context.package
                    )));
                }
                packages.sort_by(|a, b| subpackages_first(a, b));
                let mut found = Vec::new();
                for package in packages {
                    let id = PackageId {
                        repository: context.repository.clone(),
                        package,
                    };
                    self.load_package(&id).map_err(invalid)?;
                    found.extend(self.package_toolchain_rules(&id));
                }
                found
            } else if wildcard {
                let id = parse(self, &format!("{base}:all"))?.package_id();
                // TargetPatternResolverUtil.getParsingErrorMessage
                // (pkgcache/TargetPatternResolverUtil.java:33-39).
                self.load_package(&id)
                    .map_err(|error| invalid(format!("while parsing '{pattern}': {error}")))?;
                self.package_toolchain_rules(&id)
            } else {
                let label = parse(self, pattern)?;
                self.target(&label).map_err(invalid)?;
                vec![label]
            };
            for label in found {
                if !labels.contains(&label) {
                    labels.push(label);
                }
            }
        }
        Ok(labels)
    }

    /// The `toolchain` rules of a loaded package, by name.
    fn package_toolchain_rules(&self, id: &PackageId) -> Vec<Label> {
        self.packages[id]
            .values()
            .filter(|rule| matches!(rule.kind, RuleKind::Native("toolchain")))
            .map(|rule| rule.label.clone())
            .collect()
    }

    /// ToolchainType.create (rules/ToolchainType.java:40-52) and
    /// Toolchain.create (rules/platform/Toolchain.java:42-110) from the
    /// validated prerequisites (crate::toolchains).
    fn toolchain_rule_providers(
        &self,
        rule: &Rule,
        config: ConfigId,
        prerequisites: &[Vec<Label>],
        configured: &ConfiguredNodes,
    ) -> Result<
        (
            Option<ToolchainTypeInfo>,
            Option<Arc<DeclaredToolchainInfo>>,
        ),
        String,
    > {
        let Some(native) = &rule.native else {
            return Ok((None, None));
        };
        match rule.kind {
            RuleKind::Native("toolchain_type") => Ok((
                Some(ToolchainTypeInfo::new(
                    rule.label.clone(),
                    native.string("no_match_error").unwrap_or_default(),
                )),
                None,
            )),
            RuleKind::Native("toolchain") => {
                let node = |label: &Label| {
                    configured.get(&ConfiguredTargetKey::new(label.clone(), config))
                };
                let internal = |what: &str| {
                    format!(
                        "internal error: validated prerequisite of {} lacks {what}",
                        rule.label
                    )
                };
                let provided = |index: usize| prerequisites.get(index).cloned().unwrap_or_default();
                let toolchain_type = provided(0)
                    .first()
                    .and_then(node)
                    .and_then(|node| node.toolchain_type.clone())
                    .ok_or_else(|| internal("ToolchainTypeInfo"))?;
                let constraints = |labels: Vec<Label>| {
                    labels
                        .iter()
                        .map(|label| {
                            node(label)
                                .and_then(|node| node.constraint_value.clone())
                                .ok_or_else(|| internal("ConstraintValueInfo"))
                        })
                        .collect::<Result<Vec<_>, _>>()
                };
                let exec_compatible_with = constraints(provided(1))?;
                let target_compatible_with = constraints(provided(2))?;
                let target_settings = provided(3)
                    .iter()
                    .map(|label| {
                        node(label)
                            .and_then(|node| node.matching.clone())
                            .ok_or_else(|| internal("ConfigMatchingProvider"))
                    })
                    .collect::<Result<Vec<_>, _>>()?;
                let Some(crate::custom_rule::AttrValue::Label(Some(toolchain))) =
                    native.attrs.get("toolchain")
                else {
                    return Err(internal("a toolchain attribute"));
                };
                let toolchain = self.resolve_rule_src(rule, toolchain)?;
                let shown = crate::rule_declarations::canonical_form(&rule.label);
                let declared = crate::toolchains::declare_toolchain(
                    &rule.label,
                    crate::toolchains::ToolchainAttributes {
                        toolchain_type,
                        exec_compatible_with,
                        target_compatible_with,
                        use_target_platform_constraints: matches!(
                            native.attrs.get("use_target_platform_constraints"),
                            Some(crate::custom_rule::AttrValue::Bool(true))
                        ),
                        target_settings,
                        toolchain,
                    },
                )
                .map_err(|errors| {
                    errors
                        .iter()
                        .map(|error| {
                            located(
                                rule.location.as_deref(),
                                format!(
                                    "in {} attribute of toolchain rule {shown}: {}",
                                    error.attribute, error.message
                                ),
                            )
                        })
                        .collect::<Vec<_>>()
                        .join("\n")
                })?;
                Ok((None, Some(Arc::new(declared))))
            }
            _ => Ok((None, None)),
        }
    }

    /// PlatformFunction.compute (analysis/platform/PlatformFunction.java:57-126),
    /// memoized per configuration.
    fn platform_info(
        &mut self,
        label: &Label,
        config: ConfigId,
        configured: &mut ConfiguredNodes,
        plan: &mut Plan,
    ) -> Result<Arc<PlatformInfo>, PlatformError> {
        let key = ConfiguredTargetKey::new(label.clone(), config);
        if let Some(cached) = self.platforms.get(&key) {
            return cached.clone();
        }
        if self.resolving_platforms.contains(&key) {
            return Err(PlatformError::Other(format!(
                "platform {} is needed while it is being resolved; this is not implemented in the native profile",
                crate::rule_declarations::canonical_form(label)
            )));
        }
        self.resolving_platforms.push(key.clone());
        let result = self.compute_platform_info(label, config, configured, plan);
        self.resolving_platforms.pop();
        self.platforms.insert(key, result.clone());
        result
    }

    fn compute_platform_info(
        &mut self,
        label: &Label,
        config: ConfigId,
        configured: &mut ConfiguredNodes,
        plan: &mut Plan,
    ) -> Result<Arc<PlatformInfo>, PlatformError> {
        let shown = crate::rule_declarations::canonical_form(label);
        // An apparent repository the main repository does not see stays an
        // unresolvable label (`@@[unknown repo 'R' requested from @@]`,
        // crate::configuration) until the package is looked up.
        if let Some(name) = crate::configuration::unknown_repository_name(label) {
            return Err(PlatformError::Missing {
                class: "BuildFileNotFoundException",
                message: format!(
                    "no such package '@@{}//{}': The repository '@@{}' could not be resolved: No repository visible as '@{name}' from main repository",
                    label.repository, label.package, label.repository
                ),
            });
        }
        // The default --host_platform is @bazel_tools//tools:host_platform
        // (PlatformOptions.java:46-56), an alias of @platforms//host whose
        // constraints the platforms module's host_platform extension computes
        // from the host (docs/PLATFORMS.md). Without the built-in bazel_tools
        // repository there is nothing to resolve.
        if label.repository == "bazel_tools"
            && let Err(error) = self.repositories.root("bazel_tools")
        {
            return Err(PlatformError::Other(format!(
                "the target platform {shown} (the default host platform) needs the built-in @bazel_tools repository, which this module profile does not provide ({error}); use --experimental-builtin-modules, or select a platform with --platforms"
            )));
        }
        // PlatformLookupUtil.hasPlatformInfo before analysis (:160-172);
        // package and target errors propagate (InvalidPlatformException(NoSuchThingException)).
        let rule = match self.target(label) {
            Err(error) => return Err(PlatformError::loading(error)),
            Ok(Target::Rule(rule) | Target::Output(rule, _)) => rule,
            Ok(Target::Source(_)) => {
                return Err(PlatformError::Other(crate::platforms::not_a_platform(
                    label,
                )));
            }
        };
        if rule.label != *label || !has_platform_info(&rule) {
            return Err(PlatformError::Other(crate::platforms::not_a_platform(
                label,
            )));
        }
        self.check_platform_tree(label)?;
        self.analyze(label, config, &mut Vec::new(), configured, plan)
            .map_err(|error| {
                PlatformError::Other(format!(
                    "{error}\n{}",
                    crate::platforms::not_a_platform(label)
                ))
            })?;
        let platform = configured
            .get(&ConfiguredTargetKey::new(label.clone(), config))
            .and_then(|node| node.platform.clone())
            .ok_or_else(|| {
                PlatformError::Other(crate::platforms::not_a_platform(
                    &self.alias_chain(configured, config, label).1,
                ))
            })?;
        // PlatformFunction parses PlatformInfo.flags into the configuration
        // (:102-125, ParsedFlagsValue): not modelled.
        if !platform.flags.is_empty() {
            return Err(PlatformError::Other(format!(
                "platform {shown} sets flags [{}]: platform-based flags (platform(flags = ...), analysis/platform/PlatformFunction.java:102-125) are not implemented in the native profile",
                platform.flags.join(", ")
            )));
        }
        Ok(platform)
    }

    /// The alias chain from `label` through plain (select()-free) aliases,
    /// stopping at a label setting (no actual in the empty configuration):
    /// the aliases passed and the end.
    fn static_alias_chain(&mut self, label: &Label) -> (Vec<Label>, Label) {
        let mut chain = Vec::new();
        let mut current = label.clone();
        loop {
            let Ok(Target::Rule(rule)) = self.target(&current) else {
                break;
            };
            if !matches!(rule.kind, RuleKind::Native("alias")) || chain.contains(&current) {
                break;
            }
            let Some(crate::custom_rule::AttrValue::Label(Some(actual))) = rule
                .native
                .as_ref()
                .and_then(|native| native.attrs.get("actual"))
            else {
                break;
            };
            let Ok(actual) = self.resolve_rule_src(&rule, actual) else {
                break;
            };
            chain.push(current);
            current = actual;
        }
        (chain, current)
    }

    /// Bazel analyzes a platform and its whole dependency tree in
    /// `CommonOptions.EMPTY_OPTIONS` (PlatformFunction.configuredTargetDep,
    /// analysis/platform/PlatformFunction.java:46-52;
    /// analysis/config/CommonOptions.java:32-42: CoreOptions defaults only,
    /// no platform options and no Starlark build settings). The native
    /// profile analyzes in the build configuration, which gives the same
    /// result only when nothing in the tree depends on the configuration:
    ///
    /// * a `label_flag`/`label_setting` on the platform label's alias chain
    ///   resolves to no actual there, so it provides no PlatformInfo (Bazel
    ///   names it: `Target //:plat_flag was referenced as a platform, but
    ///   does not provide PlatformInfo`, paired capture B2 platforms*_plat_flag,
    ///   platforms*_fg_flag); one reached through a platform rule's attribute
    ///   fails that rule's prerequisite validation (Bazel's text);
    /// * select() keys anywhere in the tree, label settings reached otherwise
    ///   and `required_settings` naming config_settings (config_setting
    ///   matching, only used by execution-platform resolution, N17) fail
    ///   explicitly.
    fn check_platform_tree(&mut self, platform: &Label) -> Result<(), PlatformError> {
        let shown = crate::rule_declarations::canonical_form(platform);
        let mut current = platform.clone();
        let mut chain = BTreeSet::new();
        while chain.insert(current.clone()) {
            let Ok(Target::Rule(rule)) = self.target(&current) else {
                break;
            };
            match rule.kind {
                RuleKind::Native("label_flag" | "label_setting") => {
                    return Err(PlatformError::Other(crate::platforms::not_a_platform(
                        &current,
                    )));
                }
                RuleKind::Native("alias") => {
                    let Some(crate::custom_rule::AttrValue::Label(Some(actual))) = rule
                        .native
                        .as_ref()
                        .and_then(|native| native.attrs.get("actual"))
                    else {
                        break;
                    };
                    match self.resolve_rule_src(&rule, actual) {
                        Ok(actual) => current = actual,
                        Err(_) => break,
                    }
                }
                _ => break,
            }
        }
        let gap = |what: String| {
            PlatformError::Other(format!(
                "{what} in the dependency tree of platform {shown} is not implemented in the native profile: Bazel analyzes platforms and their dependencies in the empty configuration (analysis/platform/PlatformFunction.java:46-52, analysis/config/CommonOptions.java:32-42), which the native profile does not model"
            ))
        };
        let mut stack = vec![platform.clone()];
        let mut visited = BTreeSet::new();
        while let Some(label) = stack.pop() {
            if visited.len() >= 100_000 || !visited.insert(label.clone()) {
                continue;
            }
            let Ok(Target::Rule(rule)) = self.target(&label) else {
                continue;
            };
            let rule_shown = crate::rule_declarations::canonical_form(&rule.label);
            if !configured_attributes::rule_config_dependencies(&rule).is_empty() {
                return Err(gap(format!("{rule_shown}: select()")));
            }
            if let RuleKind::Native(kind @ ("label_flag" | "label_setting")) = rule.kind {
                return Err(gap(format!("{rule_shown}: {kind}")));
            }
            // A label setting reached through a platform rule's attribute
            // has no providers in the empty configuration: Bazel's
            // prerequisite error (paired capture A8/gap_flag_parent,
            // B8/target_cv_flag_platform), then InvalidPlatformException.
            let attributes = platform_prerequisite_attributes(&rule);
            let mut errors = Vec::new();
            let mut required_are_settings = true;
            for attribute in &attributes {
                for text in &attribute.labels {
                    let Ok(dependency) = self.resolve_rule_src(&rule, text) else {
                        continue;
                    };
                    let (chain, end) = self.static_alias_chain(&dependency);
                    let Ok(Target::Rule(end_rule)) = self.target(&end) else {
                        required_are_settings &= attribute.name != "required_settings";
                        continue;
                    };
                    if attribute.name == "required_settings" {
                        required_are_settings &=
                            matches!(end_rule.kind, RuleKind::Native("config_setting"));
                    }
                    if matches!(
                        end_rule.kind,
                        RuleKind::Native("label_flag" | "label_setting")
                    ) && let Some(error) =
                        rule_dependency_error(attribute, &chain, &end, &end_rule, None)
                    {
                        errors.push(located(
                            rule.location.as_deref(),
                            format!(
                                "in {} attribute of {} rule {rule_shown}: {error}",
                                attribute.name,
                                rule.rule_class_name()
                            ),
                        ));
                    }
                }
            }
            if !errors.is_empty() {
                errors.push(crate::platforms::not_a_platform(platform));
                return Err(PlatformError::Other(errors.join("\n")));
            }
            // Non-config_setting required settings fail prerequisite
            // validation, which the analysis below reports like Bazel.
            if let (RuleKind::Native("platform"), Some(native)) = (&rule.kind, &rule.native)
                && !native.labels("required_settings").is_empty()
                && required_are_settings
            {
                return Err(gap(format!(
                    "{rule_shown}: required_settings (config_setting matching for execution platform resolution, N17)"
                )));
            }
            if let Some(native) = &rule.native {
                stack.extend(native.input_labels.iter().cloned());
            }
            if let Some(custom) = &rule.custom {
                stack.extend(
                    custom
                        .declaration
                        .builtin
                        .target
                        .input_labels
                        .iter()
                        .cloned(),
                );
                for text in crate::package::custom_dependency_labels(&custom.attrs) {
                    if let Ok(dependency) = self.resolve_rule_src(&rule, &text) {
                        stack.push(dependency);
                    }
                }
            }
        }
        Ok(())
    }

    /// Starlark build settings of the command line
    /// (runtime/StarlarkOptionsParser.java, crate::starlark_options): each
    /// option name is loaded as a target pattern
    /// (BlazeOptionHandler.SkyframeExecutorTargetLoader,
    /// runtime/BlazeOptionHandler.java:371-410). `command_args` are the
    /// command's arguments (for the `Did you mean` hint); `labels` converts
    /// label values with the main repository mapping.
    pub fn starlark_options(
        &mut self,
        args: &[String],
        command_args: &[String],
        labels: &LabelParser<'_>,
        current_package: &str,
    ) -> Result<BTreeMap<Label, SettingValue>, String> {
        if args.is_empty() {
            return Ok(BTreeMap::new());
        }
        let mut loader = |name: &str| self.load_option_target(name, current_package);
        crate::starlark_options::parse(args, command_args, &mut loader, labels)
    }

    fn load_option_target(
        &mut self,
        name: &str,
        current_package: &str,
    ) -> Result<LoadedTarget, String> {
        let labels =
            if name.ends_with("/...")
                || name.ends_with(":all")
                || name.ends_with(":*")
                || name.ends_with(":all-targets")
            {
                self.select(&[name.to_owned()], current_package)?
            } else {
                // Label syntax first (LabelParser.Parts.parse, cmdline/LabelParser.java),
                // then repository visibility with Bazel's wording
                // (cmdline/RepositoryMapping.java, RepositoryName.getNameWithAt).
                crate::bazel_label::Parts::parse(name)?;
                let label = self.repositories.parse(name, "", current_package).map_err(
                    |error| match name
                        .strip_prefix('@')
                        .filter(|rest| !rest.starts_with('@'))
                        .and_then(|rest| rest.split_once("//"))
                    {
                        Some((repository, _))
                            if error.starts_with("unknown apparent repository") =>
                        {
                            format!("No repository visible as '@{repository}' from main repository")
                        }
                        _ => error,
                    },
                )?;
                self.target(&label)?;
                vec![label]
            };
        let [label] = labels.as_slice() else {
            return Err("user-defined flags must reference exactly one target".to_owned());
        };
        // Options are loaded through the target pattern phase, which reports
        // deprecated targets (BlazeOptionHandler.java:393-409).
        self.report_pattern_deprecation(label);
        Ok(match self.target(label)? {
            Target::Source(_) => LoadedTarget::NotRule(label.clone()),
            // Target.getAssociatedRule of an output file is its generating rule.
            Target::Output(..) => LoadedTarget::OtherRule(label.clone()),
            Target::Rule(rule) => {
                if let Some(info) = self.build_setting_info(&rule)? {
                    LoadedTarget::BuildSetting(info)
                } else if matches!(rule.kind, RuleKind::Native("alias")) {
                    let native = rule.native.as_ref().expect("native alias");
                    let actual = if native.configurable.contains_key("actual") {
                        None
                    } else {
                        match native.attrs.get("actual") {
                            Some(crate::custom_rule::AttrValue::Label(Some(actual))) => {
                                Some(self.resolve_rule_src(&rule, actual)?)
                            }
                            _ => None,
                        }
                    };
                    LoadedTarget::Alias {
                        label: label.clone(),
                        actual,
                    }
                } else {
                    LoadedTarget::OtherRule(label.clone())
                }
            }
        })
    }

    /// ConfigConditionsProducer (analysis/producers/ConfigConditionsProducer.java:91-172):
    /// every `$config_dependencies` label is analyzed in this configuration;
    /// each must then provide a ConfigMatchingProvider
    /// (ConfigConditions.fromConfiguredTarget, analysis/config/ConfigConditions.java:64-83).
    fn config_conditions(
        &mut self,
        rule: &Rule,
        config: ConfigId,
        active: &mut Vec<ConfiguredTargetKey>,
        configured: &mut ConfiguredNodes,
        plan: &mut Plan,
    ) -> Result<Vec<(String, ConfigMatching)>, String> {
        let mut keys = Vec::new();
        for text in configured_attributes::rule_config_dependencies(rule) {
            let label = self.resolve_rule_src(rule, &text)?;
            // A key that fails to load or analyze is reported by itself, then
            // the consumer adds `errors encountered resolving select() keys
            // for L` at its location (ConfigConditionsProducer.emitErrorIfMostImportant,
            // analysis/producers/ConfigConditionsProducer.java:121-129, 200-211).
            // A missing key of another package is first reported at that
            // package's BUILD file (the target loading event, paired capture
            // B2/lx_missing_key).
            let loaded = self.target(&label).map(|_| ()).map_err(|error| {
                self.foreign_target_event(&rule.label.package_id(), &label, &error)
                    .unwrap_or(error)
            });
            loaded
                .and_then(|()| self.analyze(&label, config, active, configured, plan))
                .map_err(|error| {
                    format!(
                        "{error}\n{}",
                        located(
                            rule.location.as_deref(),
                            format!(
                                "errors encountered resolving select() keys for {}",
                                crate::rule_declarations::canonical_form(&rule.label)
                            )
                        )
                    )
                })?;
            keys.push((text, label));
        }
        let mut conditions = Vec::new();
        for (text, label) in keys {
            let key = ConfiguredTargetKey::new(label.clone(), config);
            // The invalid-condition message names the prerequisite's target,
            // the end of an alias chain (ConfigConditionsProducer.java:156-165,
            // ConfiguredTargetAndData.getTargetLabel).
            let label = self.alias_chain(configured, config, &label).1;
            // ConfigConditions.fromConfiguredTarget (analysis/config/ConfigConditions.java:64-83):
            // a ConfigMatchingProvider, or a constraint_value matched against
            // the target platform when the consuming rule uses toolchain
            // resolution (otherwise the key is not a valid condition).
            let node = configured.get(&key);
            let mut matching = node.and_then(|node| node.matching.clone());
            if matching.is_none()
                && let Some(value) = node.and_then(|node| node.constraint_value.clone())
                && uses_toolchain_resolution(rule)
            {
                let platform = self.target_platform(config, configured, plan)?;
                matching = Some(value.config_matching(&platform));
            }
            let Some(matching) = matching else {
                let target = crate::rule_declarations::canonical_form(&rule.label);
                return Err(located(
                    rule.location.as_deref(),
                    format!(
                        "{} is not a valid select() condition for {target}.\nTo inspect the select(), run: bazel query --output=build {target}.\nFor more help, see https://bazel.build/reference/be/functions#select.\n\n",
                        crate::rule_declarations::canonical_form(&label)
                    ),
                ));
            };
            conditions.push((text, matching));
        }
        Ok(conditions)
    }

    /// CommonPrerequisiteValidator.validateDirectPrerequisiteForTestOnly and
    /// validateDirectPrerequisiteForDeprecation (CommonPrerequisiteValidator.java:683-776)
    /// with BazelPrerequisiteValidator (bazel/rules/BazelPrerequisiteValidator.java:24-33:
    /// no experimental packages, same logical package = same package). Messages
    /// use RuleContext's `in CLASS rule LABEL: ` prefix
    /// (analysis/EventHandlingErrorReporter.java:80-82).
    fn validate_prerequisite(
        &mut self,
        rule: &Rule,
        dependency: &Label,
        description: &str,
        skip_testonly: bool,
    ) -> Result<(), String> {
        // A RuleContext.ruleError at the consuming rule's location.
        self.prerequisite_error(rule, dependency, description, skip_testonly)?
            .map_or(Ok(()), |error| {
                Err(located(rule.location.as_deref(), error))
            })
    }

    /// The testonly error of [`Self::validate_prerequisite`] (`None` when
    /// valid); deprecation warnings are recorded. `dependency` is the end of
    /// an alias chain and `description` its describeTargetWithAliases text.
    fn prerequisite_error(
        &mut self,
        rule: &Rule,
        dependency: &Label,
        description: &str,
        skip_testonly: bool,
    ) -> Result<Option<String>, String> {
        let (generating, output) = match self.target(dependency)? {
            Target::Rule(dep) => (dep, false),
            Target::Output(dep, _) => (dep, true),
            Target::Source(_) => return Ok(None),
        };
        let shown = crate::rule_declarations::canonical_form(&rule.label);
        let shown_dependency = crate::rule_declarations::canonical_form(dependency);
        let prefix = format!("in {} rule {shown}: ", rule.rule_class_name());
        let args = &self.packages[&rule.label.package_id()].args;
        let dependency_args = &self.packages[&generating.label.package_id()].args;
        // Output files of testonly rules are only checked with
        // --incompatible_check_testonly_for_output_files (default false,
        // analysis/config/CoreOptions.java:690-701).
        let testonly_error = (!skip_testonly
            && !output
            && generating.is_test_only(dependency_args)
            && !rule.is_test_only(args))
        .then(|| {
            format!(
                "{prefix}non-test target '{shown}' depends on testonly {description} and doesn't have testonly attribute set"
            )
        });
        // validate() runs the testonly check, then the deprecation check, for
        // each edge (CommonPrerequisiteValidator.java:48-60): an edge's
        // deprecation warning is still reported with its testonly error.
        if rule.deprecation_warning(args).is_none()
            && rule.label.package_id() != generating.label.package_id()
            && let Some(warning) = generating.deprecation_warning(dependency_args)
        {
            let message = if output {
                format!(
                    "{prefix}target '{shown}' depends on the output file {shown_dependency} of a deprecated rule {}': {warning}",
                    crate::rule_declarations::canonical_form(&generating.label)
                )
            } else {
                format!(
                    "{prefix}target '{shown}' depends on deprecated target '{shown_dependency}': {warning}"
                )
            };
            // Skyframe reports an identical warning event once
            // (EmittedEventState.addWarning, skyframe/EmittedEventState.java:32-52):
            // an edge to the same deprecated target in two configurations,
            // or through two attributes, warns once.
            let warning = located(rule.location.as_deref(), message);
            if !self.warnings.contains(&warning) {
                self.warnings.push(warning);
            }
        }
        Ok(testonly_error)
    }

    /// The artifacts of `label` configured in `config`.
    fn artifacts(
        &mut self,
        label: &Label,
        config: ConfigId,
        built: &BTreeMap<ConfiguredTargetKey, Vec<Artifact>>,
        declared: &Declared,
    ) -> Result<Vec<Artifact>, String> {
        match self.target(label)? {
            Target::Source(real) => Ok(vec![Artifact {
                real,
                exec: label.source_exec_path(),
                producer: None,
                owner: label.to_string(),
            }]),
            Target::Rule(rule) => built
                .get(&ConfiguredTargetKey::new(rule.label.clone(), config))
                .cloned()
                .ok_or_else(|| format!("internal analysis order error: {label}")),
            Target::Output(rule, out) => {
                let exec = rule
                    .label
                    .with_name(&out)?
                    .generated_exec_path(&self.configurations.output_root(config));
                declared
                    .get(&(config, exec))
                    .cloned()
                    .map(|artifact| vec![artifact])
                    .ok_or_else(|| format!("missing analyzed output {label}"))
            }
        }
    }

    fn inputs(
        &mut self,
        rule: &Rule,
        config: ConfigId,
        built: &BTreeMap<ConfiguredTargetKey, Vec<Artifact>>,
        declared: &Declared,
    ) -> Result<ResolvedInputs, String> {
        let mut inputs = Vec::new();
        let mut seen: BTreeMap<PathBuf, Artifact> = BTreeMap::new();
        let mut locations = BTreeMap::new();
        for src in &rule.srcs {
            let label = self.resolve_rule_src(rule, src)?;
            let artifacts = self.artifacts(&label, config, built, declared)?;
            let paths = artifacts.iter().map(|a| a.exec.clone()).collect::<Vec<_>>();
            locations.insert(label.to_string(), paths.clone());
            locations.insert(src.clone(), paths);
            for artifact in artifacts {
                if let Some(previous) = seen.get(&artifact.exec) {
                    if previous.real != artifact.real
                        || previous.producer != artifact.producer
                        || previous.owner != artifact.owner
                    {
                        return Err(format!(
                            "different artifacts map to the same execution path: {}",
                            artifact.exec.display()
                        ));
                    }
                } else {
                    seen.insert(artifact.exec.clone(), artifact.clone());
                    inputs.push(artifact);
                }
            }
        }
        Ok((inputs, locations))
    }

    /// TargetPatternPhaseFunction.maybeReportDeprecation
    /// (skyframe/TargetPatternPhaseFunction.java:255-275): a loaded rule whose
    /// `deprecation` is explicitly specified (package defaults excluded, ""
    /// included) warns at its location.
    fn report_pattern_deprecation(&mut self, label: &Label) {
        if let Ok(Target::Rule(rule)) = self.target(label) {
            let explicit = rule
                .native
                .as_ref()
                .and_then(|native| native.string("deprecation"))
                .or_else(|| {
                    rule.custom
                        .as_ref()
                        .and_then(|custom| custom.declaration.builtin.target.string("deprecation"))
                });
            if let Some(message) = explicit {
                self.warnings.push(located(
                    rule.location.as_deref(),
                    format!(
                        "target '{}' is deprecated: {message}",
                        crate::rule_declarations::canonical_form(&rule.label)
                    ),
                ));
            }
        }
    }

    pub fn build(&mut self, labels: &[Label], cache: &Path) -> Result<BuildSummary, String> {
        self.build_with_jobs(labels, cache, 1)
    }

    pub fn build_with_jobs(
        &mut self,
        labels: &[Label],
        cache: &Path,
        jobs: usize,
    ) -> Result<BuildSummary, String> {
        if jobs == 0 {
            return Err("jobs must be greater than zero".into());
        }
        let (nodes, requested) = self.plan_build(labels)?;
        let results = scheduler::execute_graph(cache, nodes, jobs)?;
        let mut summary = BuildSummary::default();
        for result in results.values() {
            if result.cache_hit {
                summary.cache_hits += 1;
            } else {
                summary.executed += 1;
            }
        }
        for (key, mut artifact) in requested {
            if let Some((id, index)) = &artifact.producer {
                artifact.real = results[id].outputs[*index].clone();
            }
            summary.outputs.push((key.label, artifact.real));
        }
        Ok(summary)
    }

    /// Analysis and action planning of a build: the actions reachable from
    /// the requested targets' artifacts, validated before any execution.
    fn plan_build(&mut self, labels: &[Label]) -> Result<BuildPlan, String> {
        // TargetPatternPhaseFunction.maybeReportDeprecation
        // (skyframe/TargetPatternPhaseFunction.java:255-275): every requested
        // rule whose `deprecation` is explicitly specified (package defaults
        // excluded, "" included), before analysis.
        for label in labels {
            self.report_pattern_deprecation(label);
        }
        // Requested targets are analyzed in the top-level configuration;
        // generated files live under their own configuration's output root.
        let top = self.configurations.top();
        let mut plan = Plan::new();
        let mut configured = ConfiguredNodes::new();
        self.registrations_checked = false;
        self.registered_toolchains.clear();
        self.toolchain_cycle_reports.clear();
        self.exec_configurations.clear();
        self.toolchain_contexts.clear();
        self.toolchain_implementations.clear();
        // BuildConfigurationKeyProducer resolves an explicit --platforms
        // entry when the top-level configuration is created, before analysis
        // (analysis/producers/BuildConfigurationKeyProducer.java:126-145). A
        // target platform that comes from --host_platform (empty --platforms)
        // is resolved by each toolchain-resolving rule
        // (DependencyContextProducerWithCompatibilityCheck.java:96-110), and a
        // non-default --host_platform by their toolchain resolution
        // (resolve_execution_platform); the default host platform only where
        // a constraint needs it (docs/PLATFORMS.md).
        let top_configuration = self.configurations.top_configuration().clone();
        let target = top_configuration.native.target_platform().clone();
        if target != crate::configuration::default_host_platform()
            && target != top_configuration.native.host_platform
        {
            self.platform_info(&target, top, &mut configured, &mut plan)
                .map_err(|error| error.target_text())?;
        }
        for label in labels {
            // A requested non-alias rule is first keyed in the untrimmed
            // top-level configuration, then in its test-trimmed one
            // (TestTrimmingTransitionFactory.create,
            // analysis/test/TestTrimmingTransitionFactory.java:92-135:
            // aliases may point to tests and are not trimmed).
            self.requested_trimmed_root = match self.target(label) {
                Ok(Target::Rule(rule))
                    if !forwards(&rule) && !matches!(rule.kind, RuleKind::PackageGroup(_)) =>
                {
                    Some(ConfiguredTargetKey::new(rule.label.clone(), top))
                }
                _ => None,
            };
            let analyzed = self.analyze(label, top, &mut Vec::new(), &mut configured, &mut plan);
            self.requested_trimmed_root = None;
            analyzed.map_err(|error| match error.split_once(ANALYSIS_ABORTED) {
                // BuildView: `Analysis of target 'L' failed; build
                // aborted: MESSAGE` for the requested target.
                Some((_, message)) => format!(
                    "Analysis of target '{}' failed; build aborted: {message}",
                    crate::rule_declarations::canonical_form(label)
                ),
                None => error,
            })?;
        }
        // Alias-like targets and the end of their alias chain: dependents see
        // that target's label, kind and providers (AliasConfiguredTarget).
        let forwarded: BTreeMap<ConfiguredTargetKey, Label> = configured
            .iter()
            .filter(|(_, node)| node.actual.is_some())
            .map(|(key, _)| {
                (
                    key.clone(),
                    self.alias_chain(&configured, key.config, &key.label).1,
                )
            })
            .collect();
        let main_ignored = self.ignored("")?;
        // Validate every command before executing any action. The dry artifacts
        // carry the same exec paths as the actual generated files.
        let mut dry: BTreeMap<ConfiguredTargetKey, Vec<Artifact>> = BTreeMap::new();
        let mut custom_providers: BTreeMap<
            ConfiguredTargetKey,
            BTreeMap<String, starlark::values::OwnedFrozenValue>,
        > = BTreeMap::new();
        let mut provider_files: BTreeMap<ConfiguredTargetKey, Vec<File>> = BTreeMap::new();
        // Returned declared-provider names per analyzed rule (Target debugPrint).
        let mut provider_keys: BTreeMap<ConfiguredTargetKey, Vec<String>> = BTreeMap::new();
        // File metadata is not a source filesystem path. Keep the real artifact
        // identity even when a custom provider forwards a non-default file.
        let mut artifact_index: BTreeMap<File, Artifact> = BTreeMap::new();
        let mut declared = Declared::new();
        let mut producers: BTreeMap<PathBuf, ConfiguredTargetKey> = BTreeMap::new();
        let mut nodes = Vec::new();
        // Every source input of every action, before execroot planting drops
        // main-repository inputs under ignored top-level entries: Bazel's
        // ArtifactFunction checks a source artifact regardless of planting.
        let mut source_inputs: BTreeMap<String, Vec<PathBuf>> = BTreeMap::new();
        for (config, rule) in &plan {
            let config = *config;
            let key = ConfiguredTargetKey::new(rule.label.clone(), config);
            let output_root = self.configurations.output_root(config);
            let action_owner = key.action_owner(top);
            // The configuration of the target's exec-transition edges.
            let exec_config = self
                .exec_configurations
                .get(&key)
                .copied()
                .unwrap_or(config);
            let dependency_key = |label: Label| ConfiguredTargetKey::new(label, config);
            if matches!(rule.kind, RuleKind::PackageGroup(_)) {
                let node = self.resolve_group(&rule.label, &mut Vec::new())?;
                let info = starlark::environment::Module::with_temp_heap(|module| {
                    let value = package_specification::alloc_info(&module.heap(), node);
                    module.set("__info", value);
                    let frozen = module.freeze().map_err(|error| {
                        format!("cannot freeze package group provider: {error:?}")
                    })?;
                    frozen.get("__info").map_err(|error| error.to_string())
                })?;
                custom_providers.insert(
                    key.clone(),
                    BTreeMap::from([(package_specification::PROVIDER_ID.to_owned(), info)]),
                );
                provider_files.insert(key.clone(), Vec::new());
                dry.insert(key, Vec::new());
                continue;
            }
            if matches!(rule.kind, RuleKind::Custom) {
                let mut dependency_targets = BTreeMap::new();
                let mut available = BTreeMap::new();
                // Resolved dependencies only: `$config_dependencies` is never a
                // prerequisite (DependencyResolutionHelpers.java:408-416). The
                // resolved toolchain implementations are dependencies too
                // (their ToolchainInfo files are action inputs).
                // Each dependency with the configuration of its edge and its
                // key in the implementation's dependency map
                // (analysis::dependency_key).
                let mut dependency_labels: Vec<(Label, ConfigId, String)> = Vec::new();
                for edge in dependency_edges(rule) {
                    let label = self.resolve_rule_src(rule, &edge.label)?;
                    let edge_config = match edge.cfg {
                        EdgeCfg::Target => config,
                        EdgeCfg::Exec(_) => exec_config,
                    };
                    let map_key =
                        analysis::dependency_key(&label.to_string(), edge.cfg != EdgeCfg::Target);
                    dependency_labels.push((label, edge_config, map_key));
                }
                let mut add_target = |label: Label| {
                    let map_key = label.to_string();
                    if !dependency_labels
                        .iter()
                        .any(|(_, _, seen)| *seen == map_key)
                    {
                        dependency_labels.push((label, config, map_key));
                    }
                };
                if let Some(context) = self.toolchain_contexts.get(&key) {
                    for implementation in context.resolved_labels() {
                        add_target(implementation);
                    }
                }
                // The target's exec_compatible_with constraint values
                // (ctx.attr.exec_compatible_with), analyzed by its toolchain
                // resolution.
                if let Some(custom) = &rule.custom {
                    for text in custom
                        .declaration
                        .builtin
                        .target
                        .labels("exec_compatible_with")
                    {
                        add_target(self.resolve_rule_src(rule, &text)?);
                    }
                }
                for (label, edge_config, map_key) in dependency_labels {
                    let artifacts = self.artifacts(&label, edge_config, &dry, &declared)?;
                    let files = artifacts.iter().map(Artifact::file).collect();
                    for artifact in artifacts {
                        artifact_index.insert(artifact.file(), artifact.clone());
                        available.insert(artifact.file(), artifact);
                    }
                    let dependency = ConfiguredTargetKey::new(label.clone(), edge_config);
                    let target_provider_files =
                        provider_files.get(&dependency).cloned().unwrap_or_default();
                    for file in &target_provider_files {
                        let artifact = artifact_index.get(file).ok_or_else(|| {
                            format!(
                                "provider file has no declared artifact identity: {}",
                                file.path
                            )
                        })?;
                        available.insert(file.clone(), artifact.clone());
                    }
                    let actual = forwarded.get(&dependency).unwrap_or(&label).clone();
                    let target = self.target(&actual)?;
                    dependency_targets.insert(
                        map_key,
                        DependencyTarget {
                            label: actual.to_string(),
                            files,
                            is_source: matches!(target, Target::Source(_)),
                            is_rule: matches!(&target, Target::Rule(rule) if !matches!(rule.kind, RuleKind::PackageGroup(_))),
                            is_package_group: matches!(&target, Target::Rule(rule) if matches!(rule.kind, RuleKind::PackageGroup(_))),
                            is_filegroup: matches!(&target, Target::Rule(rule) if matches!(rule.kind, RuleKind::Filegroup)),
                            providers: custom_providers
                                .get(&dependency)
                                .cloned()
                                .unwrap_or_default(),
                            provider_files: target_provider_files,
                            // The alias chain's end, in the same configuration
                            // (Self::alias_chain).
                            provider_keys: provider_keys
                                .get(&ConfiguredTargetKey::new(actual.clone(), edge_config))
                                .cloned()
                                .unwrap_or_default(),
                        },
                    );
                }
                // ctx.target_platform_has_constraint: a toolchain-resolving
                // rule sees its target platform; a deferred default host
                // platform is resolved only when the implementation asks,
                // then the (side-effect free) analysis runs again.
                let mut analyzed_rule = rule.clone();
                let uses_toolchains = uses_toolchain_resolution(rule)
                    && !self.empty_configuration_rules.contains(&key);
                let target = dependency_key(
                    self.configurations
                        .get(config)
                        .native
                        .target_platform()
                        .clone(),
                );
                if let Some(custom) = analyzed_rule.custom.as_mut() {
                    custom.target_platform = match self.platforms.get(&target) {
                        _ if !uses_toolchains => crate::platforms::TargetPlatform::Absent,
                        Some(Ok(platform)) => {
                            crate::platforms::TargetPlatform::Resolved(platform.clone())
                        }
                        _ => crate::platforms::TargetPlatform::Deferred,
                    };
                    // ResolvedToolchainContext.load (analysis/ResolvedToolchainContext.java:48-110)
                    // with ConfiguredTargetFunction.java:348-366: the
                    // ToolchainInfo of each resolved implementation (through
                    // an alias), else TargetNotToolchainException.
                    custom.toolchains = if uses_toolchains {
                        let mut toolchains = BTreeMap::new();
                        let mut requested = Vec::new();
                        if let Some(context) = self.toolchain_contexts.get(&key) {
                            for (info, implementation) in &context.resolved {
                                let Some(provider) = custom_providers
                                    .get(&dependency_key(implementation.clone()))
                                    .and_then(|providers| {
                                        providers.get(crate::toolchain_info::PROVIDER_ID)
                                    })
                                else {
                                    return Err(self.toolchain_error(
                                        rule,
                                        config,
                                        "",
                                        &crate::toolchains::not_a_toolchain(
                                            &info.type_label,
                                            implementation,
                                        ),
                                    ));
                                };
                                toolchains.insert(info.type_label.clone(), provider.clone());
                            }
                            requested = context
                                .requested
                                .iter()
                                .map(|(label, info)| (label.clone(), info.type_label.clone()))
                                .collect();
                        }
                        Some(Arc::new(crate::toolchain_info::ToolchainContextData {
                            target_description: format!(
                                "{} rule {}",
                                rule.rule_class_name(),
                                crate::rule_declarations::canonical_form(&rule.label)
                            ),
                            requested,
                            toolchains,
                            repositories: self.repositories.clone(),
                        }))
                    } else {
                        None
                    };
                }
                // print() output of a discarded attempt is dropped: Bazel
                // runs the implementation once, after its target platform is
                // known.
                let outputs = analysis::ConfiguredOutputs {
                    output_root: &output_root,
                    action_owner: &action_owner,
                };
                let (mut analyzed, mut printed) = analysis::analyze_buffered(
                    &analyzed_rule,
                    &dependency_targets,
                    self.repositories.clone(),
                    &outputs,
                );
                if analyzed
                    .as_ref()
                    .is_err_and(|error| error.contains(crate::platforms::NEEDS_TARGET_PLATFORM))
                {
                    // The target's own configuration's target platform.
                    let platform =
                        self.target_platform(config, &mut configured, &mut Vec::new())?;
                    if let Some(custom) = analyzed_rule.custom.as_mut() {
                        custom.target_platform =
                            crate::platforms::TargetPlatform::Resolved(platform);
                    }
                    (analyzed, printed) = analysis::analyze_buffered(
                        &analyzed_rule,
                        &dependency_targets,
                        self.repositories.clone(),
                        &outputs,
                    );
                }
                for line in printed {
                    eprintln!("{line}");
                }
                let analyzed = analyzed?;
                restricted_to_error(rule)?;
                custom_providers.insert(key.clone(), analyzed.providers.clone());
                provider_keys.insert(key.clone(), analyzed.provider_keys.clone());
                provider_files.insert(key.clone(), analyzed.provider_files.clone());
                for file in &analyzed.declared {
                    let package_output =
                        PathBuf::from(&output_root).join(rule.label.package_exec_path());
                    let short_name = Path::new(&file.path)
                        .strip_prefix(&package_output)
                        .map_err(|_| format!("output outside declaring package: {}", file.path))?;
                    rule.label.with_name(&short_name.to_string_lossy())?;
                    let ignored = self.ignored(&rule.label.repository)?;
                    self.validate_output_directory(
                        &rule.label.package_id(),
                        &short_name.to_string_lossy(),
                        &ignored,
                    )?;
                }
                // First index EVERY action's outputs, so registration order is not
                // an execution order and forward generated dependencies are legal.
                for registered in &analyzed.actions {
                    for (index, file) in registered.outputs.iter().enumerate() {
                        let exec = PathBuf::from(&file.path);
                        register_output(&mut producers, &exec, &key)?;
                        let artifact = Artifact {
                            real: exec.clone(),
                            exec: exec.clone(),
                            producer: Some((registered.action.id.clone(), index)),
                            owner: file.owner.clone(),
                        };
                        available.insert(file.clone(), artifact.clone());
                        artifact_index.insert(file.clone(), artifact.clone());
                        declared.insert((config, exec), artifact);
                    }
                }
                for file in &analyzed.provider_files {
                    if !available.contains_key(file) {
                        return Err(format!(
                            "{}: provider file has no declared artifact identity: {}",
                            rule.label, file.path
                        ));
                    }
                }
                for registered in analyzed.actions {
                    let inputs = registered
                        .inputs
                        .iter()
                        .map(|file| {
                            available.get(file).cloned().ok_or_else(|| {
                                format!(
                                    "{}: input has no declared artifact: {}",
                                    rule.label, file.path
                                )
                            })
                        })
                        .collect::<Result<Vec<_>, _>>()?;
                    source_inputs.insert(registered.action.id.clone(), source_paths(&inputs));
                    nodes.push(plan_action(registered.action, inputs, &main_ignored)?);
                }
                let defaults = analyzed
                    .files
                    .iter()
                    .map(|file| {
                        available.get(file).cloned().ok_or_else(|| {
                            format!(
                                "{}: DefaultInfo file has no artifact: {}",
                                rule.label, file.path
                            )
                        })
                    })
                    .collect::<Result<Vec<_>, _>>()?;
                dry.insert(key, defaults);
                continue;
            }
            let (inputs, mut locations) = self.inputs(rule, config, &dry, &declared)?;
            for artifact in &inputs {
                artifact_index.insert(artifact.file(), artifact.clone());
            }
            // genrule `tools`, built in the exec configuration: action inputs
            // and $(location) entries merged with the srcs' (CommandHelper,
            // analysis/CommandHelper.java:128-180), never $(SRCS)
            // (GenRuleBase.java:95-110, 170-175).
            let mut tools = Vec::new();
            if let (RuleKind::Genrule, Some(native)) = (&rule.kind, &rule.native) {
                for text in native.labels("tools") {
                    let label = self.resolve_rule_src(rule, &text)?;
                    let artifacts = self.artifacts(&label, exec_config, &dry, &declared)?;
                    let paths = artifacts
                        .iter()
                        .map(|artifact| artifact.exec.clone())
                        .collect::<Vec<_>>();
                    locations
                        .entry(label.to_string())
                        .or_default()
                        .extend(paths.iter().cloned());
                    if text != label.to_string() {
                        locations.entry(text).or_default().extend(paths);
                    }
                    for artifact in &artifacts {
                        artifact_index.insert(artifact.file(), artifact.clone());
                    }
                    tools.extend(artifacts);
                }
            }
            let outputs = rule
                .outs
                .iter()
                .map(|o| {
                    rule.label
                        .with_name(o)
                        .map(|label| label.generated_exec_path(&output_root))
                })
                .collect::<Result<Vec<_>, _>>()?;
            if matches!(rule.kind, RuleKind::Genrule) {
                if outputs.is_empty() {
                    // GenRuleBase.create (rules/genrule/GenRuleBase.java:75-77) with
                    // RuleContext.attributeError's prefix.
                    return Err(located(
                        rule.location.as_deref(),
                        format!(
                            "in outs attribute of genrule rule {}: Genrules without outputs don't make sense",
                            crate::rule_declarations::canonical_form(&rule.label)
                        ),
                    ));
                }
                add_output_locations(rule, &outputs, &mut locations)?;
                let command = expand_command_in(
                    // GenRuleBase.determineCommandTypeAndAttribute
                    // (rules/genrule/GenRuleBase.java:262-284); cmd_bash makes
                    // the rule unanalyzed earlier, cmd_bat/cmd_ps are Windows-only.
                    rule.cmd.as_deref().ok_or_else(|| {
                        located(
                            rule.location.as_deref(),
                            format!(
                                "in cmd attribute of genrule rule {}: missing value for `cmd` attribute, you can also set `cmd_ps` or `cmd_bat` on Windows and `cmd_bash` on other platforms.",
                                crate::rule_declarations::canonical_form(&rule.label)
                            ),
                        )
                    })?,
                    &inputs.iter().map(|a| a.exec.clone()).collect::<Vec<_>>(),
                    &outputs,
                    &locations,
                    &rule.label,
                    &self.repositories,
                    &output_root,
                )?;
                restricted_to_error(rule)?;
                for output in &outputs {
                    register_output(&mut producers, output, &key)?;
                }
                let action = Action {
                    id: action_owner.clone(),
                    command,
                    inputs: Vec::new(),
                    outputs: outputs.clone(),
                    write: None,
                    program: None,
                    shell_strict: true,
                    arguments: Vec::new(),
                    environment: BTreeMap::new(),
                };
                let action_inputs = inputs.iter().cloned().chain(tools).collect::<Vec<_>>();
                source_inputs.insert(action.id.clone(), source_paths(&action_inputs));
                nodes.push(plan_action(action, action_inputs, &main_ignored)?);
                let artifacts = outputs
                    .into_iter()
                    .enumerate()
                    .map(|(index, exec)| Artifact {
                        real: exec.clone(),
                        exec,
                        producer: Some((action_owner.clone(), index)),
                        owner: rule.label.to_string(),
                    })
                    .collect::<Vec<_>>();
                for artifact in &artifacts {
                    artifact_index.insert(artifact.file(), artifact.clone());
                    declared.insert((config, artifact.exec.clone()), artifact.clone());
                }
                dry.insert(key, artifacts);
            } else {
                restricted_to_error(rule)?;
                // The native platform providers, visible to Starlark as
                // dep[platform_common.X] (crate::platform_providers).
                if let Some(node) = configured.get(&key)
                    && !forwards(rule)
                {
                    use crate::platform_providers::Provider;
                    let provider = match (
                        &node.constraint_setting,
                        &node.constraint_value,
                        &node.platform,
                    ) {
                        (Some(info), _, _) => Some(Provider::Setting(info.clone())),
                        (_, Some(info), _) => Some(Provider::Value(info.clone())),
                        (_, _, Some(info)) => Some(Provider::Platform(info.clone())),
                        _ => None,
                    };
                    if let Some(provider) = provider {
                        let (id, value) = crate::platform_providers::freeze(provider)?;
                        custom_providers.insert(key.clone(), BTreeMap::from([(id, value)]));
                        provider_files.insert(key.clone(), Vec::new());
                    }
                }
                // An alias-like target forwards its actual's files (above) and
                // providers.
                if forwards(rule) {
                    let actual = dependency_key(self.resolve_rule_src(rule, &rule.srcs[0])?);
                    if let Some(providers) = custom_providers.get(&actual).cloned() {
                        custom_providers.insert(key.clone(), providers);
                    }
                    if let Some(files) = provider_files.get(&actual).cloned() {
                        provider_files.insert(key.clone(), files);
                    }
                }
                dry.insert(key, inputs);
            }
        }
        let mut requested: Vec<(ConfiguredTargetKey, Artifact)> = Vec::new();
        for label in labels {
            for artifact in self.artifacts(label, top, &dry, &declared)? {
                requested.push((self.configurations.top_key(label.clone()), artifact));
            }
        }
        // Analyze everything first, but execute only the actions reachable from
        // requested outputs. Empty DefaultInfo must not execute unused actions.
        let by_id: BTreeMap<_, _> = nodes
            .iter()
            .map(|node| (node.action.id.clone(), node))
            .collect();
        let mut todo = requested
            .iter()
            .filter_map(|(_, artifact)| artifact.producer.as_ref().map(|(id, _)| id.clone()))
            .collect::<Vec<_>>();
        let mut required = BTreeSet::new();
        while let Some(id) = todo.pop() {
            if required.insert(id.clone()) {
                let node = by_id
                    .get(&id)
                    .ok_or_else(|| format!("missing producing action: {id}"))?;
                todo.extend(
                    node.dependencies
                        .iter()
                        .map(|dependency| dependency.action_id.clone()),
                );
            }
        }
        nodes.retain(|node| required.contains(&node.action.id));
        // Missing declared source files fail only where needed: as requested
        // outputs or as inputs of an executed action (ArtifactFunction
        // "missing input file", skyframe/ArtifactFunction.java:79-90, 404-430;
        // ActionExecutionFunction.createSourceErrorCodeAndMessage,
        // skyframe/ActionExecutionFunction.java:1543-1583).
        let missing_sources = self.missing_sources.clone();
        let missing = |real: &Path| {
            missing_sources
                .get(real)
                .filter(|_| std::fs::symlink_metadata(real).is_err())
                .map(crate::rule_declarations::canonical_form)
        };
        let mut errors = Vec::new();
        // Requested targets: `LOC: TARGET: missing input file 'L'` lines, then
        // `LOC: N input file(s) do not exist` (CompletionFunction).
        let mut by_target: Vec<(Label, Vec<String>)> = Vec::new();
        for (key, artifact) in &requested {
            if artifact.producer.is_none()
                && let Some(missing_label) = missing(&artifact.real)
            {
                match by_target.iter_mut().find(|(seen, _)| *seen == key.label) {
                    Some((_, labels)) => labels.push(missing_label),
                    None => by_target.push((key.label.clone(), vec![missing_label])),
                }
            }
        }
        for (label, labels) in &by_target {
            let location = match self.target(label) {
                Ok(Target::Rule(rule) | Target::Output(rule, _)) => rule.location,
                _ => None,
            };
            let shown = crate::rule_declarations::canonical_form(label);
            for missing_label in labels {
                errors.push(located(
                    location.as_deref(),
                    format!("{shown}: missing input file '{missing_label}'"),
                ));
            }
            errors.push(located(
                location.as_deref(),
                format!("{} input file(s) do not exist", labels.len()),
            ));
        }
        // Actions: `LOC: PROGRESS failed: missing input file 'L'` lines, then
        // `LOC: PROGRESS failed: N input file(s) do not exist`. The genrule
        // progress message is `Executing genrule L`
        // (rules/genrule/GenRuleBase.java); custom actions use their action id.
        let rules_by_label = plan
            .iter()
            .map(|(config, rule)| {
                (
                    ConfiguredTargetKey::new(rule.label.clone(), *config).action_owner(top),
                    rule,
                )
            })
            .collect::<BTreeMap<_, _>>();
        for node in &nodes {
            let absent = source_inputs
                .get(&node.action.id)
                .into_iter()
                .flatten()
                .filter_map(|real| missing(real))
                .collect::<Vec<_>>();
            if absent.is_empty() {
                continue;
            }
            let owner = node.action.id.split('#').next().unwrap_or_default();
            let rule = rules_by_label.get(owner);
            let progress = match rule {
                Some(rule) if matches!(rule.kind, RuleKind::Genrule) => format!(
                    "Executing genrule {}",
                    crate::rule_declarations::canonical_form(&rule.label)
                ),
                _ => node.action.id.clone(),
            };
            let location = rule.and_then(|rule| rule.location.as_deref());
            for label in &absent {
                errors.push(located(
                    location,
                    format!("{progress} failed: missing input file '{label}'"),
                ));
            }
            errors.push(located(
                location,
                format!(
                    "{progress} failed: {} input file(s) do not exist",
                    absent.len()
                ),
            ));
        }
        if !errors.is_empty() {
            return Err(errors.join("\n"));
        }
        Ok((nodes, requested))
    }
}

/// Marks an error that aborts the analysis of a requested target; the
/// build prefixes `Analysis of target 'L' failed; build aborted: `.
const ANALYSIS_ABORTED: &str = "\u{0}analysis aborted:";

/// `No repository visible as '@R' from main repository` (or `from repository
/// '@@C'`) for a label text naming an apparent repository the context
/// repository does not see (RepositoryFetchFunction.java:149-153,
/// cmdline/RepositoryName.java:251-258).
fn unknown_apparent_repository(text: &str, context: &str, error: &str) -> Option<String> {
    if !error.starts_with("unknown apparent repository") {
        return None;
    }
    let name = text
        .strip_prefix('@')
        .filter(|rest| !rest.starts_with('@'))
        .and_then(|rest| rest.split_once("//"))
        .map(|(name, _)| name)?;
    Some(format!(
        "No repository visible as '@{name}' from {}",
        if context.is_empty() {
            "main repository".to_owned()
        } else {
            format!("repository '@@{context}'")
        }
    ))
}

/// The RecursivePkgFunction order of packages below a directory: sorted
/// directory entries, a directory's subpackages before the directory itself
/// (a STABLE_ORDER nested set iterates postorder).
fn subpackages_first(a: &str, b: &str) -> std::cmp::Ordering {
    let mut left = a.split('/').filter(|part| !part.is_empty());
    let mut right = b.split('/').filter(|part| !part.is_empty());
    loop {
        match (left.next(), right.next()) {
            (Some(x), Some(y)) if x == y => continue,
            (Some(x), Some(y)) => return crate::native_rules::java_string_cmp(x, y),
            // A descendant comes before its ancestor.
            (Some(_), None) => return std::cmp::Ordering::Less,
            (None, Some(_)) => return std::cmp::Ordering::Greater,
            (None, None) => return std::cmp::Ordering::Equal,
        }
    }
}

/// TargetSuggester.suggestTargets (packages/TargetSuggester.java:41-140) with
/// SpellChecker.editDistance (net/starlark/java/spelling/SpellChecker.java:33-77).
fn suggest_targets(input: &str, words: &BTreeSet<String>) -> String {
    fn edit_distance(s1: &[char], s2: &[char], max: usize) -> Option<usize> {
        if s1 == s2 {
            return Some(0);
        }
        if s1.len().abs_diff(s2.len()) > max {
            return None;
        }
        let mut row = (0..=s2.len()).collect::<Vec<_>>();
        for i in 1..=s1.len() {
            row[0] = i;
            let mut best = row[0];
            let mut previous = i - 1;
            for j in 1..=s2.len() {
                let old = row[j];
                row[j] = (previous + usize::from(s1[i - 1] != s2[j - 1]))
                    .min(1 + row[j - 1].min(row[j]));
                previous = old;
                best = best.min(row[j]);
            }
            if best > max {
                return None;
            }
        }
        let result = row[s2.len()];
        (result <= max).then_some(result)
    }
    let lower = input.to_lowercase().chars().collect::<Vec<_>>();
    let max = 5.min(input.chars().count().div_ceil(2));
    let mut by_distance: BTreeMap<usize, Vec<&str>> = BTreeMap::new();
    for word in words {
        let candidate = word.to_lowercase().chars().collect::<Vec<_>>();
        if let Some(distance) = edit_distance(&lower, &candidate, max) {
            by_distance.entry(distance).or_default().push(word);
        }
    }
    let suggested = if let Some(words) = by_distance.get(&0).or_else(|| by_distance.get(&1)) {
        words.iter().take(10).copied().collect::<Vec<_>>()
    } else {
        let mut suggested = Vec::new();
        for distance in 2..max {
            for word in by_distance.get(&distance).into_iter().flatten() {
                if suggested.len() < 10 {
                    suggested.push(*word);
                }
            }
        }
        suggested
    };
    match suggested.as_slice() {
        [] => String::new(),
        [only] => format!(" (did you mean {only}?)"),
        [init @ .., last] => format!(" (did you mean {}, or {last}?)", init.join(", ")),
    }
}

/// CommonPrerequisiteValidator.generateVisibilityConflictMessage
/// (analysis/CommonPrerequisiteValidator.java:380-405, default
/// --noverbose_visibility_errors) reported by RuleContext.ruleError at the
/// consuming rule (`LOC: in CLASS rule L: `, EventHandlingErrorReporter.java:80-82).
/// `description` is describeTargetWithAliases(WITHOUT_KIND); `is_source`
/// whether the (alias chain's end) target is a source file. Implicit
/// (private Starlark) attributes are attributed to the consumer target here,
/// not to its rule's `.bzl` (divergence, docs/CONFIGURATION.md).
fn visibility_error(rule: &Rule, description: &str, is_source: bool) -> String {
    located(
        rule.location.as_deref(),
        visibility_message(rule, description, is_source),
    )
}

fn visibility_message(rule: &Rule, description: &str, is_source: bool) -> String {
    let mut message = format!(
        "in {} rule {}: Visibility error:\n{description} is not visible from\ntarget '{}'\nRecommendation: modify the visibility declaration if you think the dependency is legitimate. For more info see https://bazel.build/concepts/visibility",
        rule.rule_class_name(),
        crate::rule_declarations::canonical_form(&rule.label),
        crate::rule_declarations::canonical_form(&rule.label),
    );
    if is_source {
        message.push_str(
            ". To set the visibility of that source file target, use the exports_files() function",
        );
    }
    message
}

/// TargetUtils.formatMissingEdge (packages/TargetUtils.java:394-430) at the
/// consuming rule's location (MissingEdgeError, analysis/producers/MissingEdgeError.java:86-111;
/// DependencyProducer.java:279-290): an explicit dependency appends
/// ` and referenced by 'L'`; an implicit one (label_flag/label_setting's
/// late-bound `:alias`, private Starlark attributes) is `every rule of type
/// KIND implicitly depends upon the target 'D', but this target could not be
/// found because of: MESSAGE`. A missing package is first reported on its
/// own (the package lookup error event). Other errors pass through.
fn missing_edge(rule: &Rule, dependency: &Label, error: String) -> String {
    let package = error.starts_with("no such package '");
    if !package && !error.starts_with("no such target '") {
        return error;
    }
    let explicit = match (&rule.kind, &rule.custom) {
        (RuleKind::Native("label_flag" | "label_setting"), _) => false,
        (_, Some(custom)) => custom.attrs.iter().any(|(name, value)| {
            !name.starts_with('_')
                && crate::package::custom_dependency_labels(&BTreeMap::from([(
                    name.clone(),
                    value.clone(),
                )]))
                .iter()
                .any(|text| Label::parse_canonical(text).as_ref() == Ok(dependency))
        }),
        _ => true,
    };
    let edge = if explicit {
        format!(
            "{error} and referenced by '{}'",
            crate::rule_declarations::canonical_form(&rule.label)
        )
    } else {
        format!(
            "every rule of type {} implicitly depends upon the target '{}', but this target could not be found because of: {error}",
            rule.rule_class_name(),
            crate::rule_declarations::canonical_form(dependency)
        )
    };
    let edge = located(rule.location.as_deref(), edge);
    if package {
        format!("{error}\n{edge}")
    } else {
        edge
    }
}

/// [`located`] for each line of several events.
fn located_lines(location: Option<&str>, messages: &str) -> String {
    messages
        .lines()
        .map(|message| located(location, message.to_owned()))
        .collect::<Vec<_>>()
        .join("\n")
}

/// Event text with Bazel's `LOCATION: ` prefix when the location is known.
fn located(location: Option<&str>, message: String) -> String {
    match location {
        Some(location) => format!("{location}: {message}"),
        None => message,
    }
}

/// RuleContextConstraintSemantics.validateAttributes
/// (analysis/constraints/RuleContextConstraintSemantics.java:354-367), run by
/// RuleConfiguredTargetBuilder.build after the rule's factory or
/// implementation (analysis/RuleConfiguredTargetBuilder.java:104-106,
/// 450-468): an explicit empty `restricted_to` is an attribute error.
fn restricted_to_error(rule: &Rule) -> Result<(), String> {
    let explicit_empty = |target: &crate::native_rules::NativeTarget| matches!(target.attrs.get("restricted_to"), Some(crate::custom_rule::AttrValue::Labels(labels)) if labels.is_empty());
    let empty = rule.native.as_deref().is_some_and(explicit_empty)
        || rule
            .custom
            .as_ref()
            .is_some_and(|custom| explicit_empty(&custom.declaration.builtin.target));
    if empty {
        return Err(located(
            rule.location.as_deref(),
            format!(
                "in restricted_to attribute of {} rule {}: attribute cannot be empty",
                rule.rule_class_name(),
                crate::rule_declarations::canonical_form(&rule.label)
            ),
        ));
    }
    Ok(())
}

fn rule_authority(rule: &Rule) -> PackageId {
    rule.macro_owner
        .as_ref()
        .map(|owner| owner.definition_package.clone())
        .unwrap_or_else(|| rule.label.package_id())
}

fn check_macro_namespace(rule: &Rule, target: &Label) -> Result<(), String> {
    if let Some(owner) = &rule.macro_owner {
        let valid = target.name == owner.name
            || target
                .name
                .strip_prefix(&owner.name)
                .is_some_and(|suffix| suffix.len() > 1 && suffix.starts_with(['_', '-', '.']));
        if !valid {
            return Err(format!(
                "target {target} is outside symbolic macro namespace {}: expected the macro name or its prefix followed by '_', '-', or '.' and a nonempty suffix",
                owner.name
            ));
        }
    }
    Ok(())
}

/// Output paths are global across configurations (Bazel's action
/// conflicts); a conflict names the owning labels.
fn register_output(
    producers: &mut BTreeMap<PathBuf, ConfiguredTargetKey>,
    output: &Path,
    owner: &ConfiguredTargetKey,
) -> Result<(), String> {
    for ancestor in output.ancestors() {
        if let Some(previous) = producers.get(ancestor) {
            return Err(format!(
                "overlapping output paths: {} from {} conflicts with {} from {}",
                ancestor.display(),
                previous.label,
                output.display(),
                owner.label
            ));
        }
    }
    if let Some((previous_path, previous)) = producers.range(output.to_path_buf()..).next()
        && previous_path.starts_with(output)
    {
        return Err(format!(
            "overlapping output paths: {} from {} conflicts with {} from {}",
            output.display(),
            owner.label,
            previous_path.display(),
            previous.label
        ));
    }
    producers.insert(output.to_path_buf(), owner.clone());
    Ok(())
}

/// Whether Bazel plants the execroot entry for a source input. The execroot
/// symlink forest omits every top-level main-repository entry with a
/// `matchingEntry` (buildtool/SymlinkForest.java:425-442 with the main
/// repository's ignored subdirectories `withPrefix(workspace)`,
/// skyframe/SkyframeExecutor.java:3776-3793), so a declared source input
/// below such an entry is absent when the action runs. External repositories
/// are planted separately under `external/` and are unaffected.
fn planted(artifact: &Artifact, main_ignored: &IgnoredSubdirectories) -> bool {
    if artifact.producer.is_some()
        || Label::parse_canonical(&artifact.owner).is_ok_and(|l| !l.repository.is_empty())
    {
        return true;
    }
    match artifact.exec.components().next() {
        Some(Component::Normal(top)) => main_ignored
            .matching_entry(&top.to_string_lossy())
            .is_none(),
        _ => true,
    }
}

/// Source (non-generated) input paths of an action, deduplicated in order.
fn source_paths(inputs: &[Artifact]) -> Vec<PathBuf> {
    let mut paths: Vec<PathBuf> = Vec::new();
    for artifact in inputs.iter().filter(|artifact| artifact.producer.is_none()) {
        if !paths.contains(&artifact.real) {
            paths.push(artifact.real.clone());
        }
    }
    paths
}

fn plan_action(
    mut action: Action,
    inputs: Vec<Artifact>,
    main_ignored: &IgnoredSubdirectories,
) -> Result<PlannedAction, String> {
    action.inputs = inputs
        .iter()
        .map(|artifact| (artifact.real.clone(), artifact.exec.clone()))
        .collect();
    action::validate_structure(&action)?;
    action.inputs.clear();
    let mut dependencies = Vec::new();
    let mut seen = BTreeMap::new();
    for artifact in inputs {
        if let Some(previous) = seen.insert(artifact.exec.clone(), artifact.file()) {
            if previous != artifact.file() {
                return Err(format!(
                    "different artifacts map to the same execution path: {}",
                    artifact.exec.display()
                ));
            }
            continue;
        }
        if let Some((action_id, output_index)) = artifact.producer {
            dependencies.push(Dependency {
                action_id,
                output_index,
                destination: artifact.exec,
            });
        } else if planted(&artifact, main_ignored) {
            action.inputs.push((artifact.real, artifact.exec));
        }
    }
    Ok(PlannedAction {
        action,
        dependencies,
    })
}

/// Load-time gates and pre-dependency gaps: native rules that loading
/// accepts but analysis does not model, package-default gaps, and declared
/// rule-level features (toolchains, exec groups, rule transitions, late-bound
/// defaults) fail explicitly before configuration conditions or dependencies
/// are visited; nothing is silently analyzed (docs/NATIVE_BUILD_RULES.md,
/// docs/CONFIGURATION.md).
fn native_analysis_gate(rule: &Rule) -> Result<(), String> {
    if let Some(error) = rule
        .native
        .as_ref()
        .and_then(|native| crate::native_rules::analysis_error(&rule.label, native))
    {
        // RuleContext errors carry the rule's location.
        return Err(located(rule.location.as_deref(), error));
    }
    // config_setting analysis is modelled (crate::config_setting),
    // alias/label_flag/label_setting forward to their actual,
    // constraint_setting/constraint_value/platform provide their platform
    // providers (crate::platforms) and toolchain_type/toolchain their
    // toolchain providers (crate::toolchains); the other native classes
    // fail explicitly.
    if let RuleKind::Native(kind) = rule.kind
        && !matches!(
            kind,
            "config_setting"
                | "alias"
                | "label_flag"
                | "label_setting"
                | "constraint_setting"
                | "constraint_value"
                | "platform"
                | "toolchain_type"
                | "toolchain"
        )
    {
        return Err(format!(
            "{}: native rule {kind} analysis not implemented in the native profile",
            rule.label
        ));
    }
    if !rule.unanalyzed.is_empty() {
        return Err(format!(
            "{}: {} accepted at load time but not analyzed by the native profile: {}",
            rule.label,
            rule.kind.class_name(),
            rule.unanalyzed.join("; ")
        ));
    }
    Ok(())
}

fn custom_gap_error(rule: &Rule, gaps: &[String]) -> String {
    format!(
        "analysis of {}: declared rule features {}: {}",
        rule.label,
        crate::rule_declarations::NOT_ANALYZED,
        gaps.join("; ")
    )
}

/// Gaps of the configured (select-resolved) attribute values, evaluated
/// after attribute validation and before any dependency edge is followed:
/// edges with transitions, aspects or file/rule restrictions and unmodelled
/// attribute values fail explicitly at the edge.
fn resolved_value_gate(rule: &Rule) -> Result<(), String> {
    if let Some(native) = &rule.native
        && matches!(
            native.rule_class.as_str(),
            "filegroup"
                | "genrule"
                | "config_setting"
                | "alias"
                | "label_flag"
                | "label_setting"
                | "constraint_setting"
                | "constraint_value"
                | "platform"
                | "toolchain_type"
                | "toolchain"
        )
    {
        let gaps = crate::native_rules::analysis_gaps(native);
        if !gaps.is_empty() {
            return Err(format!(
                "{}: {} accepted at load time but not analyzed by the native profile: {}",
                rule.label,
                rule.kind.class_name(),
                gaps.join("; ")
            ));
        }
    }
    if let Some(custom) = &rule.custom {
        let gaps = crate::custom_rule::value_gaps(custom);
        if !gaps.is_empty() {
            return Err(custom_gap_error(rule, &gaps));
        }
    }
    Ok(())
}

fn add_output_locations(
    rule: &Rule,
    outputs: &[PathBuf],
    locations: &mut BTreeMap<String, Vec<PathBuf>>,
) -> Result<(), String> {
    for (name, path) in rule.outs.iter().zip(outputs) {
        locations.insert(name.clone(), vec![path.clone()]);
        locations.insert(format!(":{name}"), vec![path.clone()]);
        locations.insert(rule.label.with_name(name)?.to_string(), vec![path.clone()]);
    }
    Ok(())
}

fn paths_string(paths: &[PathBuf]) -> String {
    paths
        .iter()
        .map(|p| p.to_string_lossy())
        .collect::<Vec<_>>()
        .join(" ")
}

fn single(paths: &[PathBuf], variable: &str) -> Result<String, String> {
    if paths.len() != 1 {
        return Err(format!(
            "{variable} requires exactly one file, found {}",
            paths.len()
        ));
    }
    Ok(paths[0].to_string_lossy().into_owned())
}

#[cfg(test)]
fn expand_command(
    command: &str,
    inputs: &[PathBuf],
    outputs: &[PathBuf],
    locations: &BTreeMap<String, Vec<PathBuf>>,
    package: &str,
) -> Result<String, String> {
    expand_command_in(
        command,
        inputs,
        outputs,
        locations,
        &Label::parse(":__command__", package)?,
        &RepositorySet::new(PathBuf::new()),
        crate::label::GENERATED_EXEC_ROOT,
    )
}

fn expand_command_in(
    command: &str,
    inputs: &[PathBuf],
    outputs: &[PathBuf],
    locations: &BTreeMap<String, Vec<PathBuf>>,
    label: &Label,
    repositories: &RepositorySet,
    output_root: &str,
) -> Result<String, String> {
    let rule_dir = PathBuf::from(output_root).join(label.package_exec_path());
    let mut expanded = String::new();
    let mut chars = command.chars();
    while let Some(c) = chars.next() {
        if c != '$' {
            expanded.push(c);
            continue;
        }
        match chars.next() {
            Some('$') => expanded.push('$'),
            Some('@') => expanded.push_str(&single(outputs, "$@")?),
            Some('<') => expanded.push_str(&single(inputs, "$<")?),
            Some('(') => {
                let mut variable = String::new();
                loop {
                    match chars.next() {
                        Some(')') => break,
                        Some(c) => variable.push(c),
                        None => return Err("unterminated genrule Make variable".into()),
                    }
                }
                let value = match variable.as_str() {
                    "SRCS" => paths_string(inputs),
                    "OUTS" => paths_string(outputs),
                    "RULEDIR" => rule_dir.to_string_lossy().into_owned(),
                    "@D" => {
                        if outputs.len() == 1 {
                            outputs[0]
                                .parent()
                                .ok_or("output has no parent")?
                                .to_string_lossy()
                                .into_owned()
                        } else {
                            rule_dir.to_string_lossy().into_owned()
                        }
                    }
                    _ => {
                        let (multiple, target) =
                            if let Some(t) = variable.strip_prefix("locations ") {
                                (true, t.trim())
                            } else if let Some(t) = variable.strip_prefix("location ") {
                                (false, t.trim())
                            } else {
                                return Err(format!(
                                    "unsupported genrule Make variable: $({variable})"
                                ));
                            };
                        let canonical = repositories
                            .parse(target, &label.repository, &label.package)?
                            .to_string();
                        let paths = locations
                            .get(target)
                            .or_else(|| locations.get(&canonical))
                            .ok_or_else(|| {
                                format!("location is not a declared src or out: {target}")
                            })?;
                        if multiple {
                            let mut sorted = paths.clone();
                            sorted.sort();
                            sorted.dedup();
                            paths_string(&sorted)
                        } else {
                            single(paths, &variable)?
                        }
                    }
                };
                expanded.push_str(&value);
            }
            Some(c) => {
                return Err(format!(
                    "unsupported Make variable ${c}; escape shell dollars with $$"
                ));
            }
            None => return Err("unescaped trailing $ in genrule command".into()),
        }
    }
    Ok(expanded)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recursive_package_order_lists_subpackages_first() {
        let mut packages = vec!["tc", "tc/b", "tc/a", "tc/a/deep", "tc/a-b", "tc/B"];
        packages.sort_by(|a, b| subpackages_first(a, b));
        assert_eq!(
            packages,
            ["tc/B", "tc/a/deep", "tc/a", "tc/a-b", "tc/b", "tc"]
        );
        let mut root = vec!["", "x", "a/b", "a"];
        root.sort_by(|a, b| subpackages_first(a, b));
        assert_eq!(root, ["a/b", "a", "x", ""]);
    }

    #[test]
    fn english_lists_follow_string_util() {
        assert_eq!(join_english_list::<&str>(&[], "or"), "nothing");
        assert_eq!(join_english_list(&["a"], "or"), "a");
        assert_eq!(join_english_list(&["a", "b"], "and"), "a and b");
        assert_eq!(join_english_list(&["a", "b", "c"], "or"), "a, b, or c");
    }

    #[test]
    fn alias_descriptions_follow_alias_provider() {
        let label = |text: &str| Label::parse_canonical(text).unwrap();
        assert_eq!(
            describe_with_aliases(&[], &label("//p:x"), "target"),
            "target '//p:x'"
        );
        assert_eq!(
            describe_with_aliases(&[label("//a:a")], &label("@@r+//p:x"), "source file"),
            "alias '//a:a' referring to source file '@@r+//p:x'"
        );
        assert_eq!(
            describe_with_aliases(
                &[label("//a:a"), label("//a:b"), label("//a:c")],
                &label("//p:x"),
                "target"
            ),
            "alias '//a:a' referring to target '//p:x' through '//a:b' -> '//a:c'"
        );
    }

    #[test]
    fn only_main_repository_sources_below_ignored_top_level_entries_are_unplanted() {
        let ignored = IgnoredSubdirectories::new(
            vec!["pkg".into(), "src/ts/node_modules".into()],
            vec!["**/node_modules".into()],
        );
        let source = |exec: &str, owner: &str| Artifact {
            real: PathBuf::from("/ws").join(exec),
            exec: PathBuf::from(exec),
            producer: None,
            owner: owner.into(),
        };
        for (artifact, expected) in [
            (source("node_modules/b.txt", "//:node_modules/b.txt"), false),
            (source("pkg/sub/c.txt", "//pkg/sub:c.txt"), false),
            (source("pkgs/c.txt", "//pkgs:c.txt"), true),
            // Nested ignored directories hang below a planted top-level entry.
            (
                source("src/ts/node_modules/x.js", "//src/ts:node_modules/x.js"),
                true,
            ),
            (
                source("external/ext+/node_modules/x", "@@ext+//:node_modules/x"),
                true,
            ),
            (
                Artifact {
                    producer: Some(("//:gen".into(), 0)),
                    ..source("node_modules/o.txt", "//:gen")
                },
                true,
            ),
        ] {
            assert_eq!(
                planted(&artifact, &ignored),
                expected,
                "{:?}",
                artifact.exec
            );
        }
        assert!(!planted(
            &source("a.txt", "//:a.txt"),
            &IgnoredSubdirectories::new(vec![String::new()], Vec::new())
        ));
    }
    use crate::package::MacroOwner;
    use crate::visibility::Visibility;

    fn test_label(text: &str) -> Label {
        Label::parse_canonical(text).unwrap()
    }

    fn test_rule(label: &str, owner: Option<(&str, &str)>) -> Rule {
        Rule {
            label: test_label(label),
            kind: RuleKind::Filegroup,
            srcs: Vec::new(),
            outs: Vec::new(),
            cmd: None,
            visibility: Visibility::Private,
            custom: None,
            macro_owner: owner.map(|(definition, name)| MacroOwner {
                definition_package: test_label(definition).package_id(),
                name: name.into(),
            }),
            native: None,
            unanalyzed: Vec::new(),
            location: None,
        }
    }

    // Isolate analysis policy from macro evaluation: these are the same owned
    // declaration snapshots that package evaluation hands to the engine.
    fn test_engine(root: PathBuf, rules: Vec<Rule>) -> Engine {
        let mut engine = Engine::new(root);
        for rule in rules {
            for out in &rule.outs {
                engine.generated.insert(
                    rule.label.with_name(out).unwrap(),
                    (rule.label.clone(), out.clone()),
                );
            }
            let package = engine.packages.entry(rule.label.package_id()).or_default();
            if let Some(owner) = &rule.macro_owner {
                package.macro_names.insert(owner.name.clone());
            }
            package.rules.insert(rule.label.clone(), rule);
        }
        engine
    }

    /// CommonPrerequisiteValidator's visibility text (test rules have no location).
    fn assert_not_visible(error: &str, dependency: &str, consumer: &str, source: bool) {
        let suffix = if source {
            ". To set the visibility of that source file target, use the exports_files() function"
        } else {
            ""
        };
        assert_eq!(
            error,
            format!(
                "in filegroup rule {consumer}: Visibility error:\ntarget '{dependency}' is not visible from\ntarget '{consumer}'\nRecommendation: modify the visibility declaration if you think the dependency is legitimate. For more info see https://bazel.build/concepts/visibility{suffix}"
            )
        );
    }

    fn analyze_roots(engine: &mut Engine, roots: &[&str]) -> Result<Vec<Rule>, String> {
        let mut done = ConfiguredNodes::new();
        let mut plan = Plan::new();
        let top = engine.configurations.top();
        for root in roots {
            engine.analyze(
                &test_label(root),
                top,
                &mut Vec::new(),
                &mut done,
                &mut plan,
            )?;
        }
        Ok(plan.into_iter().map(|(_, rule)| rule).collect())
    }

    fn group(label: &str, specs: &[&str], includes: &[&str]) -> Rule {
        let mut rule = test_rule(label, None);
        rule.kind = RuleKind::PackageGroup(Arc::new(package_specification::GroupDefinition {
            own_specs: package_specification::parse_specs(
                &specs.iter().map(|s| (*s).to_owned()).collect::<Vec<_>>(),
                &rule.label.package_id(),
                &RepositorySet::new(PathBuf::new()),
            )
            .unwrap(),
            includes: includes.iter().map(|s| test_label(s)).collect(),
        }));
        rule.visibility = Visibility::Public;
        rule
    }

    #[test]
    fn package_group_cached_diamond_keeps_independent_exclusions_and_identity() {
        let mut engine = test_engine(
            PathBuf::new(),
            vec![
                group("//g:leaf", &["//foo/private"], &[]),
                group("//g:left", &["//foo/...", "-//foo/private"], &["//g:leaf"]),
                group("//g:right", &["//other"], &["//g:leaf"]),
                group("//g:root", &[], &["//g:left", "//g:right", "//g:left"]),
            ],
        );
        let root = engine
            .resolve_group(&test_label("//g:root"), &mut Vec::new())
            .unwrap();
        assert_eq!(engine.group_nodes.len(), 4);
        for member in ["//foo/private:x", "//foo/public:x", "//other:x"] {
            assert!(root.contains(&test_label(member).package_id()));
        }
        assert!(!root.contains(&test_label("//foobar:x").package_id()));
        let left = engine
            .resolve_group(&test_label("//g:left"), &mut Vec::new())
            .unwrap();
        let right = engine
            .resolve_group(&test_label("//g:right"), &mut Vec::new())
            .unwrap();
        assert!(!Arc::ptr_eq(&left, &right));
        assert!(left.contains(&test_label("//foo/public:x").package_id()));
        assert!(!right.contains(&test_label("//foo/public:x").package_id()));
        assert!(Arc::ptr_eq(
            &root,
            &engine
                .resolve_group(&test_label("//g:root"), &mut Vec::new())
                .unwrap()
        ));
    }

    #[test]
    fn package_group_large_shared_declarations_and_cached_edges_never_copy_payloads() {
        let leaf_label = test_label("//g:leaf");
        let root_label = test_label("//g:root");
        let large_package = format!("//{}", "p".repeat(512 * 1024));
        let leaf = group("//g:leaf", &[&large_package], &[]);
        let mut root = group("//g:root", &[], &[]);
        let RuleKind::PackageGroup(definition) = &mut root.kind else {
            unreachable!()
        };
        const EDGES: usize = 4096;
        Arc::get_mut(definition).unwrap().includes = vec![leaf_label.clone(); EDGES];
        let mut engine = test_engine(PathBuf::new(), vec![leaf, root]);
        let RuleKind::PackageGroup(declaration) =
            &engine.packages[&leaf_label.package_id()][&leaf_label].kind
        else {
            unreachable!()
        };
        let declaration = Arc::clone(declaration);
        let Target::Rule(first) = engine.target(&leaf_label).unwrap() else {
            unreachable!()
        };
        let RuleKind::PackageGroup(first) = first.kind else {
            unreachable!()
        };
        assert!(Arc::ptr_eq(&declaration, &first));
        for _ in 0..128 {
            let Target::Rule(next) = engine.target(&leaf_label).unwrap() else {
                unreachable!()
            };
            let RuleKind::PackageGroup(next) = next.kind else {
                unreachable!()
            };
            assert!(Arc::ptr_eq(&first, &next));
        }
        // GroupDefinition deliberately has no Clone implementation: every
        // generic Target/Rule snapshot above must share the declaration payload.
        let declaration_owners = Arc::strong_count(&declaration);
        let leaf_node = engine.resolve_group(&leaf_label, &mut Vec::new()).unwrap();
        let root_node = engine.resolve_group(&root_label, &mut Vec::new()).unwrap();
        assert_eq!(engine.group_nodes.len(), 2);
        assert_eq!(Arc::strong_count(&leaf_node), EDGES + 2);
        assert_eq!(Arc::strong_count(&declaration), declaration_owners);
        assert!(root_node.contains(&test_label(&format!("{large_package}:x")).package_id()));

        // Once the engine's cumulative node budget is exhausted, both cached
        // resolutions must still succeed without charging or copying payloads.
        while engine.group_budget.node().is_ok() {}
        for _ in 0..128 {
            assert!(Arc::ptr_eq(
                &leaf_node,
                &engine.resolve_group(&leaf_label, &mut Vec::new()).unwrap()
            ));
            assert!(Arc::ptr_eq(
                &root_node,
                &engine.resolve_group(&root_label, &mut Vec::new()).unwrap()
            ));
        }
        assert_eq!(Arc::strong_count(&declaration), declaration_owners);
        assert_eq!(Arc::strong_count(&leaf_node), EDGES + 2);
    }

    #[test]
    fn package_group_cached_height_rejects_257_path_nodes_child_first() {
        let mut rules = vec![group("//g:n0", &[], &[])];
        for n in 1..=256 {
            rules.push(group(
                &format!("//g:n{n}"),
                &[],
                &[&format!("//g:n{}", n - 1)],
            ));
        }
        let mut engine = test_engine(PathBuf::new(), rules);
        for n in 0..256 {
            engine
                .resolve_group(&test_label(&format!("//g:n{n}")), &mut Vec::new())
                .unwrap();
        }
        let error = engine
            .resolve_group(&test_label("//g:n256"), &mut Vec::new())
            .unwrap_err();
        assert!(error.contains("depth limit 256"), "{error}");
        assert_eq!(engine.group_nodes.len(), 256);
        assert!(!engine.group_nodes.contains_key(&test_label("//g:n256")));
    }

    #[test]
    fn package_group_edge_budget_precedes_allocation_and_cache_publication() {
        let mut huge = group("//g:huge", &[], &[]);
        let RuleKind::PackageGroup(definition) = &mut huge.kind else {
            unreachable!()
        };
        Arc::get_mut(definition).unwrap().includes = vec![test_label("//g:leaf"); 100_001];
        let mut engine = test_engine(PathBuf::new(), vec![huge, group("//g:leaf", &[], &[])]);
        let error = engine
            .resolve_group(&test_label("//g:huge"), &mut Vec::new())
            .unwrap_err();
        assert!(
            error.contains("host preparation") && error.contains("limit"),
            "{error}"
        );
        assert!(engine.group_nodes.is_empty());

        // Distinct successfully published nodes share one engine-wide budget.
        let long_package = format!("//{}", "p".repeat(1024 * 1024));
        let rules = (0..9)
            .map(|n| group(&format!("//g:n{n}"), &[&long_package], &[]))
            .collect();
        let mut engine = test_engine(PathBuf::new(), rules);
        for n in 0..7 {
            engine
                .resolve_group(&test_label(&format!("//g:n{n}")), &mut Vec::new())
                .unwrap();
        }
        assert!(
            engine
                .resolve_group(&test_label("//g:n7"), &mut Vec::new())
                .unwrap_err()
                .contains("byte limit")
        );
        assert_eq!(engine.group_nodes.len(), 7);
        // Cached hits require no new persistent allocation budget.
        assert!(
            engine
                .resolve_group(&test_label("//g:n0"), &mut Vec::new())
                .is_ok()
        );
    }

    #[test]
    fn package_group_query_excludes_nonrules_and_includes_never_declare_files() {
        let f = Fixture::new();
        f.write("MODULE.bazel", "");
        f.write("app/data", "not an implicit source");
        f.write(
            "app/BUILD",
            "package_group(name='g', includes=[':data'])\nfilegroup(name='ordinary')",
        );
        let mut engine = Engine::new(f.0.clone());
        for pattern in ["//app:all", "//app/..."] {
            assert_eq!(
                engine.select(&[pattern.into()], "").unwrap(),
                vec![test_label("//app:ordinary")]
            );
        }
        assert_eq!(
            engine.select(&["//app:g".into()], "").unwrap(),
            vec![test_label("//app:g")]
        );
        assert!(
            engine
                .select(&["//app:data".into()], "")
                .unwrap_err()
                .contains("not declared")
        );
        assert!(
            engine
                .build(&[test_label("//app:g")], &f.0.join("cache"))
                .unwrap_err()
                .contains("not declared")
        );
        assert!(!f.0.join("cache").exists());
    }

    #[test]
    fn package_group_visibility_validates_roots_sources_and_all_refs_before_grants() {
        let f = Fixture::new();
        f.write("app/data", "source");
        for invalid in ["missing", "ordinary", "cycle"] {
            for source in [false, true] {
                for incoming in [false, true] {
                    let mut root = test_rule("//app:root", None);
                    let visibility = Visibility::from_labels(vec![
                        test_label("//app:__pkg__"),
                        test_label(&format!("//app:{invalid}")),
                    ])
                    .unwrap();
                    root.visibility = visibility.clone();
                    let mut consumer = test_rule("//app:consumer", None);
                    consumer
                        .srcs
                        .push(if source { ":data" } else { ":root" }.into());
                    let mut engine = test_engine(
                        f.0.clone(),
                        vec![
                            root,
                            consumer,
                            test_rule("//app:ordinary", None),
                            group("//app:cycle", &[], &["//app:cycle"]),
                        ],
                    );
                    engine
                        .packages
                        .get_mut(&test_label("//app:root").package_id())
                        .unwrap()
                        .exports
                        .insert(test_label("//app:data"), visibility);
                    let target = if incoming {
                        "//app:consumer"
                    } else if source {
                        "//app:data"
                    } else {
                        "//app:root"
                    };
                    assert!(engine.select(&[target.into()], "").is_ok());
                    let error = engine
                        .build(&[test_label(target)], &f.0.join("cache"))
                        .unwrap_err();
                    assert!(
                        error.contains(match invalid {
                            "missing" => "not declared",
                            "ordinary" => "does not refer to a package group",
                            _ => "cycle",
                        }),
                        "{target}: {error}"
                    );
                    assert!(!f.0.join("cache").exists());
                }
            }
        }
    }

    #[test]
    fn package_group_visibility_uses_effective_macro_and_source_authorities() {
        let f = Fixture::new();
        f.write("app/data", "source");
        for requested in ["//producer:demo", "//producer:demo.out", "//app:data"] {
            for allowed in ["//defs", "//app", "@@dep//defs"] {
                let mut producer = test_rule("//producer:demo", Some(("//owner:m.bzl", "demo")));
                producer.visibility =
                    Visibility::from_labels(vec![test_label("//g:allow")]).unwrap();
                producer.outs.push("demo.out".into());
                let mut consumer = test_rule("//app:demo", Some(("//defs:m.bzl", "demo")));
                consumer.srcs.push(requested.into());
                let mut engine = test_engine(
                    f.0.clone(),
                    vec![producer, consumer, group("//g:allow", &[allowed], &[])],
                );
                engine
                    .packages
                    .get_mut(&test_label("//app:demo").package_id())
                    .unwrap()
                    .exports
                    .insert(
                        test_label("//app:data"),
                        Visibility::from_labels(vec![test_label("//g:allow")]).unwrap(),
                    );
                let result = analyze_roots(&mut engine, &["//app:demo"]);
                assert_eq!(
                    result.is_ok(),
                    allowed == "//defs",
                    "{requested} {allowed}: {result:?}"
                );
            }
        }
    }

    #[test]
    fn package_group_kind_checks_precede_callbacks_and_native_inputs_reject_groups() {
        let f = Fixture::new();
        f.write("MODULE.bazel", "");
        f.write(
            "app/defs.bzl",
            "def impl(ctx):\n    fail('WRONG_KIND_CALLBACK_RAN')\nr = rule(implementation=impl)\n",
        );
        for build in [
            "package_group(name='root', includes=[':wrong'])",
            "filegroup(name='root', visibility=[':wrong'])",
        ] {
            f.write(
                "app/BUILD",
                &format!("load(':defs.bzl', 'r')\nr(name='wrong')\n{build}"),
            );
            let mut engine = Engine::new(f.0.clone());
            let error = engine
                .build(&[test_label("//app:root")], &f.0.join("cache"))
                .unwrap_err();
            assert!(
                error.contains("does not refer to a package group"),
                "{error}"
            );
            assert!(!error.contains("WRONG_KIND_CALLBACK_RAN"), "{error}");
        }
        for declaration in [
            "filegroup(name='root', srcs=[':g'])",
            "genrule(name='root', srcs=[':g'], outs=['out'], cmd='touch $@')",
        ] {
            f.write(
                "app/BUILD",
                &format!("package_group(name='g')\n{declaration}"),
            );
            let mut engine = Engine::new(f.0.clone());
            assert!(
                engine
                    .build(&[test_label("//app:root")], &f.0.join("cache"))
                    .unwrap_err()
                    .contains("misplaced")
            );
        }
        assert!(!f.0.join("cache").exists());
    }

    #[test]
    fn package_group_macro_namespace_is_checked_on_includes_and_cache_hits() {
        let mut owned = group("//app:outside", &["public"], &[]);
        owned.macro_owner = Some(MacroOwner {
            definition_package: test_label("//defs:m.bzl").package_id(),
            name: "demo".into(),
        });
        let mut engine = test_engine(
            PathBuf::new(),
            vec![owned, group("//app:root", &[], &["//app:outside"])],
        );
        assert!(engine.select(&["//app:outside".into()], "").is_ok());
        assert!(
            engine
                .resolve_group(&test_label("//app:root"), &mut Vec::new())
                .unwrap_err()
                .contains("symbolic macro namespace")
        );
        assert!(engine.group_nodes.is_empty());
        let mut engine = test_engine(
            PathBuf::new(),
            vec![group("//app:outside", &["public"], &[])],
        );
        engine
            .resolve_group(&test_label("//app:outside"), &mut Vec::new())
            .unwrap();
        engine
            .packages
            .get_mut(&test_label("//app:outside").package_id())
            .unwrap()
            .rules
            .get_mut(&test_label("//app:outside"))
            .unwrap()
            .macro_owner = Some(MacroOwner {
            definition_package: test_label("//defs:m.bzl").package_id(),
            name: "demo".into(),
        });
        assert!(
            engine
                .resolve_group(&test_label("//app:outside"), &mut Vec::new())
                .unwrap_err()
                .contains("symbolic macro namespace")
        );
    }

    #[test]
    fn package_group_provider_materializes_independent_frozen_instances_and_empty_files() {
        let f = Fixture::new();
        f.write("MODULE.bazel", "");
        f.write("app/defs.bzl", r#"
def impl(ctx):
    a = ctx.attr.a[PackageSpecificationInfo]
    b = ctx.attr.b[PackageSpecificationInfo]
    a == b or fail("native equality")
    (a.contains("//allowed:x") and not b.contains("//allowed:x")) or fail("merged data")
    (b.contains("//different:x") and not a.contains("//different:x")) or fail("merged data")
    ctx.attr.a.files.to_list() == [] or fail("group artifacts")
    ctx.attr.a[DefaultInfo].files.to_list() == [] or fail("default artifacts")
    out = ctx.actions.declare_file("result")
    ctx.actions.write(out, str(a.contains("//allowed:x")) + ":" + str(b.contains("//allowed:x")))
    return [DefaultInfo(files=depset([out]))]
check = rule(implementation=impl, attrs={"a":attr.label(providers=[PackageSpecificationInfo]), "b":attr.label(providers=[PackageSpecificationInfo])})
def emit(name, visibility):
    native.package_group(name=name, packages=["//allowed"])
m = macro(implementation=emit)
"#);
        f.write("app/BUILD", "load(':defs.bzl', 'check', 'm')\nm(name='a')\npackage_group(name='b', packages=['//different'])\ncheck(name='consumer', a=':a', b=':b')\npackage(default_visibility=[':missing'])");
        // The consumer's bad default is intentional: no callback/action may run
        // until its outgoing visibility references have been validated.
        let mut engine = Engine::new(f.0.clone());
        let standalone = engine
            .build(&[test_label("//app:a")], &f.0.join("empty-cache"))
            .unwrap();
        assert!(standalone.outputs.is_empty());
        assert_eq!((standalone.executed, standalone.cache_hits), (0, 0));
        assert!(
            engine
                .build(&[test_label("//app:consumer")], &f.0.join("bad-cache"))
                .unwrap_err()
                .contains("not declared")
        );
        assert!(!f.0.join("bad-cache").exists());
        f.write("app/BUILD", "load(':defs.bzl', 'check', 'm')\nm(name='a')\npackage_group(name='b', packages=['//different'])\ncheck(name='consumer', a=':a', b=':b')");
        let mut engine = Engine::new(f.0.clone());
        for warm in [false, true] {
            let result = engine
                .build(&[test_label("//app:consumer")], &f.0.join("cache"))
                .unwrap();
            assert_eq!(std::fs::read(&result.outputs[0].1).unwrap(), b"True:False");
            assert_eq!(
                (result.executed, result.cache_hits),
                if warm { (0, 1) } else { (1, 0) }
            );
        }
        assert_eq!(engine.group_nodes.len(), 2);
    }

    #[test]
    fn package_group_source_default_visibility_and_public_normalization() {
        let f = Fixture::new();
        f.write("MODULE.bazel", "");
        f.write("app/data", "source");
        f.write("app/BUILD", "filegroup(name='declares_source', srcs=['data'])\npackage(default_visibility=[':missing'])");
        let mut engine = Engine::new(f.0.clone());
        assert!(engine.select(&["//app:data".into()], "").is_ok());
        assert!(
            engine
                .build(&[test_label("//app:data")], &f.0.join("bad-cache"))
                .unwrap_err()
                .contains("not declared")
        );
        assert!(!f.0.join("bad-cache").exists());
        f.write("app/defs.bzl", "def impl(name, visibility):\n    native.filegroup(name=name, visibility=visibility)\nm=macro(implementation=impl)");
        f.write("app/BUILD", "load(':defs.bzl','m')\nm(name='root', visibility=['//nonexistent:group', '//visibility:public'])");
        let mut engine = Engine::new(f.0.clone());
        assert!(
            engine
                .build(&[test_label("//app:root")], &f.0.join("cache"))
                .unwrap()
                .outputs
                .is_empty()
        );
        assert!(engine.group_nodes.is_empty());
    }

    #[test]
    fn macro_authority_checks_both_directions_even_within_one_physical_package() {
        for macro_producer in [true, false] {
            let owner = ("//defs:macro.bzl", "demo");
            let producer = test_rule("//app:demo_secret", macro_producer.then_some(owner));
            let mut consumer = test_rule("//app:demo_use", (!macro_producer).then_some(owner));
            consumer.srcs.push("//app:demo_secret".into());
            let mut engine = test_engine(PathBuf::new(), vec![producer, consumer]);
            // A direct root is not itself subject to visibility. It also must
            // not let a subsequent unauthorized edge bypass the check via done.
            let error =
                analyze_roots(&mut engine, &["//app:demo_secret", "//app:demo_use"]).unwrap_err();
            assert_not_visible(&error, "//app:demo_secret", "//app:demo_use", false);
        }
    }

    #[test]
    fn shared_macro_definition_authority_allows_private_cross_package_edges() {
        let owner = Some(("//defs:macro.bzl", "demo"));
        let producer = test_rule("//one:demo_secret", owner);
        let mut consumer = test_rule("//two:demo_use", owner);
        consumer.srcs.push("//one:demo_secret".into());
        let mut engine = test_engine(PathBuf::new(), vec![producer, consumer]);
        let plan = analyze_roots(&mut engine, &["//two:demo_use"]).unwrap();
        assert_eq!(plan[0].label, test_label("//one:demo_secret"));
        assert_eq!(plan[1].label, test_label("//two:demo_use"));
    }

    #[test]
    fn authority_keeps_canonical_repositories_distinct_and_honors_explicit_grants() {
        let producer = test_rule("//app:demo_secret", Some(("@@dep//defs:m.bzl", "demo")));
        let mut consumer = test_rule("//app:demo_use", Some(("//defs:m.bzl", "demo")));
        consumer.srcs.push("//app:demo_secret".into());
        let mut engine = test_engine(PathBuf::new(), vec![producer.clone(), consumer.clone()]);
        assert!(
            analyze_roots(&mut engine, &["//app:demo_use"])
                .unwrap_err()
                .contains("not visible")
        );
        let mut producer = producer;
        producer.visibility =
            Visibility::Private.with_package(&test_label("//defs:x").package_id());
        let mut engine = test_engine(PathBuf::new(), vec![producer, consumer]);
        assert_eq!(
            analyze_roots(&mut engine, &["//app:demo_use"])
                .unwrap()
                .len(),
            2
        );
    }

    #[test]
    fn generated_output_visibility_uses_generator_authority() {
        for macro_producer in [true, false] {
            let owner = ("//defs:m.bzl", "demo");
            let mut producer = test_rule("//app:demo_make", macro_producer.then_some(owner));
            producer.outs.push("demo.out".into());
            let mut consumer = test_rule("//app:demo_use", (!macro_producer).then_some(owner));
            consumer.srcs.push("//app:demo.out".into());
            let mut engine = test_engine(PathBuf::new(), vec![producer, consumer]);
            assert_not_visible(
                &analyze_roots(&mut engine, &["//app:demo_make", "//app:demo_use"]).unwrap_err(),
                "//app:demo.out",
                "//app:demo_use",
                false,
            );
        }
    }

    #[test]
    fn macro_namespace_requires_separator_and_nonempty_suffix_for_every_rule_kind() {
        for kind in [RuleKind::Filegroup, RuleKind::Genrule, RuleKind::Custom] {
            for (name, valid) in [
                ("demo", true),
                ("demo_x", true),
                ("demo-x", true),
                ("demo.x", true),
                ("demo_", false),
                ("demo-", false),
                ("demo.", false),
                ("demox", false),
                ("demo/x", false),
                ("other", false),
            ] {
                let mut rule = test_rule(&format!("//app:{name}"), Some(("//defs:m.bzl", "demo")));
                rule.kind = kind.clone();
                let mut engine = test_engine(PathBuf::new(), vec![rule]);
                let result = analyze_roots(&mut engine, &[&format!("//app:{name}")]);
                assert_eq!(result.is_ok(), valid, "{kind:?}: {name}");
                if let Err(error) = result {
                    assert!(error.contains("symbolic macro namespace"), "{error}");
                }
            }
        }
    }

    #[test]
    fn outside_namespace_output_is_queryable_and_emitted_but_not_a_build_target() {
        let f = Fixture::new();
        let mut rule = test_rule("//app:demo", Some(("//defs:m.bzl", "demo")));
        rule.kind = RuleKind::Genrule;
        rule.outs.push("outside.out".into());
        rule.cmd = Some("printf macro-output > $@".into());
        let mut engine = test_engine(f.0.clone(), vec![rule.clone()]);
        assert_eq!(
            engine.select(&["//app:all".into()], "").unwrap(),
            vec![rule.label.clone()]
        );
        assert_eq!(
            engine.select(&["//app:outside.out".into()], "").unwrap(),
            vec![test_label("//app:outside.out")]
        );
        let built = engine
            .build(&[rule.label.clone()], &f.0.join("cache"))
            .unwrap();
        assert_eq!(built.executed, 1);
        assert_eq!(std::fs::read(&built.outputs[0].1).unwrap(), b"macro-output");
        for roots in [
            vec!["//app:outside.out"],
            vec!["//app:demo", "//app:outside.out"],
            vec!["//app:outside.out", "//app:demo"],
        ] {
            let mut engine = test_engine(f.0.clone(), vec![rule.clone()]);
            let roots = roots.into_iter().map(test_label).collect::<Vec<_>>();
            let error = engine
                .build(&roots, &f.0.join("rejected-cache"))
                .unwrap_err();
            assert!(error.contains("//app:outside.out"), "{error}");
            assert!(error.contains("symbolic macro namespace"), "{error}");
            assert!(!f.0.join("rejected-cache").exists());
        }
    }

    #[test]
    fn valid_output_cannot_hide_invalid_generator_and_unselected_rule_does_not_poison_build() {
        let mut invalid = test_rule("//app:outside", Some(("//defs:m.bzl", "demo")));
        invalid.outs.push("demo.out".into());
        let valid = test_rule("//app:demo_ok", Some(("//defs:m.bzl", "demo")));
        let mut engine = test_engine(PathBuf::new(), vec![invalid, valid]);
        assert_eq!(engine.select(&["//app:all".into()], "").unwrap().len(), 2);
        assert!(
            engine
                .select(&["//app:outside".into(), "//app:demo.out".into()], "")
                .is_ok()
        );
        assert_eq!(
            analyze_roots(&mut engine, &["//app:demo_ok"])
                .unwrap()
                .len(),
            1
        );
        let error = analyze_roots(&mut engine, &["//app:demo.out"]).unwrap_err();
        assert!(error.contains("//app:outside"), "{error}");
        assert!(error.contains("symbolic macro namespace"), "{error}");
    }

    #[test]
    fn macro_only_source_reference_does_not_create_source_even_with_same_authority() {
        let f = Fixture::new();
        f.write("app/data", "source");
        let mut rule = test_rule("//app:demo", Some(("//app:m.bzl", "demo")));
        rule.srcs.push(":data".into());
        let mut engine = test_engine(f.0.clone(), vec![rule]);
        assert!(
            engine
                .select(&["//app:data".into()], "")
                .unwrap_err()
                .contains("not declared")
        );
        assert!(
            analyze_roots(&mut engine, &["//app:demo"])
                .unwrap_err()
                .contains("not declared")
        );
    }

    #[test]
    fn explicit_export_or_unbuilt_ordinary_reference_establishes_macro_source() {
        let f = Fixture::new();
        f.write("app/data", "source");
        for exported in [true, false] {
            let mut macro_rule = test_rule("//app:demo", Some(("//app:m.bzl", "demo")));
            macro_rule.srcs.push(":data".into());
            let mut engine = test_engine(f.0.clone(), vec![macro_rule]);
            let source = test_label("//app:data");
            let package = engine.packages.get_mut(&source.package_id()).unwrap();
            if exported {
                package.exports.insert(source.clone(), Visibility::Private);
            } else {
                let mut ordinary = test_rule("//app:unselected", None);
                ordinary.srcs.push(":data".into());
                package.rules.insert(ordinary.label.clone(), ordinary);
            }
            assert_eq!(
                engine.select(&["//app:data".into()], "").unwrap(),
                vec![source]
            );
            let plan = analyze_roots(&mut engine, &["//app:demo"]).unwrap();
            assert_eq!(plan.len(), 1);
            assert_eq!(plan[0].label, test_label("//app:demo"));
        }
    }

    #[test]
    fn reserved_macro_name_cannot_be_an_implicit_source_but_rules_and_outputs_win() {
        let f = Fixture::new();
        f.write("app/demo", "source");
        let mut ordinary = test_rule("//app:ordinary", None);
        ordinary.srcs.push(":demo".into());
        let mut engine = test_engine(f.0.clone(), vec![ordinary]);
        let label = test_label("//app:demo");
        engine
            .packages
            .get_mut(&label.package_id())
            .unwrap()
            .macro_names
            .insert("demo".into());
        assert!(
            engine
                .select(&["//app:demo".into()], "")
                .unwrap_err()
                .contains("not declared")
        );
        let rule = test_rule("//app:demo", Some(("//defs:m.bzl", "demo")));
        engine
            .packages
            .get_mut(&label.package_id())
            .unwrap()
            .rules
            .insert(label.clone(), rule);
        assert!(matches!(engine.target(&label), Ok(Target::Rule(_))));
        let output = test_label("//app:reserved_output");
        engine
            .packages
            .get_mut(&label.package_id())
            .unwrap()
            .macro_names
            .insert(output.name.clone());
        engine
            .generated
            .insert(output.clone(), (label, output.name.clone()));
        assert!(matches!(engine.target(&output), Ok(Target::Output(_, _))));
    }

    #[test]
    fn macro_source_visibility_uses_physical_source_and_definition_consumer_authorities() {
        let f = Fixture::new();
        f.write("app/data", "source");
        for exported in [true, false] {
            let mut consumer = test_rule("//app:demo", Some(("//defs:m.bzl", "demo")));
            consumer.srcs.push(":data".into());
            let mut ordinary = test_rule("//app:ordinary", None);
            ordinary.srcs.push(":data".into());
            let mut engine = test_engine(f.0.clone(), vec![consumer, ordinary]);
            let source = test_label("//app:data");
            if exported {
                engine
                    .packages
                    .get_mut(&source.package_id())
                    .unwrap()
                    .exports
                    .insert(source.clone(), Visibility::Private);
            }
            assert_not_visible(
                &analyze_roots(&mut engine, &["//app:demo"]).unwrap_err(),
                "//app:data",
                "//app:demo",
                true,
            );
            let package = engine.packages.get_mut(&source.package_id()).unwrap();
            let grant = Visibility::Private.with_package(&test_label("//defs:x").package_id());
            if exported {
                package.exports.insert(source, grant);
            } else {
                package.default_visibility = grant;
            }
            assert_eq!(
                analyze_roots(&mut engine, &["//app:demo"]).unwrap().len(),
                1
            );
        }
    }

    #[test]
    fn macro_dynamic_artifact_filename_is_not_a_namespace_target() {
        let f = Fixture::new();
        f.write("MODULE.bazel", "");
        f.write(
            "app/BUILD",
            "load(':defs.bzl','write')\nwrite(name='demo')\n",
        );
        f.write("app/defs.bzl", "def _impl(ctx):\n    out = ctx.actions.declare_file('outside.txt')\n    ctx.actions.write(out, str(ctx.label))\n    return [DefaultInfo(files=depset([out]))]\nwrite = rule(implementation=_impl)\n");
        let label = test_label("//app:demo");
        let mut engine = Engine::new(f.0.clone());
        engine.load_package(&label.package_id()).unwrap();
        engine
            .packages
            .get_mut(&label.package_id())
            .unwrap()
            .rules
            .get_mut(&label)
            .unwrap()
            .macro_owner = Some(MacroOwner {
            definition_package: test_label("//defs:m.bzl").package_id(),
            name: "demo".into(),
        });
        let result = engine.build(&[label], &f.0.join("cache")).unwrap();
        assert_eq!(result.executed, 1);
        assert_eq!(
            std::fs::read(&result.outputs[0].1).unwrap(),
            b"@@//app:demo"
        );
        assert!(engine.select(&["//app:outside.txt".into()], "").is_err());
    }

    #[test]
    fn external_artifact_short_paths_are_metadata_only() {
        for (exec, producer, expected) in [
            ("external/dep+/p/in", None, "../dep+/p/in"),
            (
                "bazel-out/rust-fastbuild/bin/external/dep+/p/out",
                Some(("action".into(), 0)),
                "../dep+/p/out",
            ),
        ] {
            let artifact = Artifact {
                real: PathBuf::from("/real/local/root/p/in"),
                exec: PathBuf::from(exec),
                producer,
                owner: "@@dep+//p:target".into(),
            };
            assert_eq!(artifact.file().short_path, expected);
            assert_eq!(artifact.file().path, exec);
        }
        let main = Artifact {
            real: PathBuf::from("/root/external/dep/in"),
            exec: PathBuf::from("external/dep/in"),
            producer: None,
            owner: "//external/dep:in".into(),
        };
        assert_eq!(main.file().short_path, "external/dep/in");
    }

    #[test]
    fn custom_canonical_main_sources_are_not_rebased_to_external() {
        let engine = Engine::new(PathBuf::new());
        let mut rule = Rule {
            label: Label::parse_canonical("@@dep+//p:target").unwrap(),
            kind: RuleKind::Custom,
            srcs: vec![],
            outs: vec![],
            cmd: None,
            visibility: Default::default(),
            custom: None,
            macro_owner: None,
            native: None,
            unanalyzed: Vec::new(),
            location: None,
        };
        assert_eq!(
            engine.resolve_rule_src(&rule, "//p:in").unwrap().repository,
            ""
        );
        rule.kind = RuleKind::Filegroup;
        assert_eq!(
            engine.resolve_rule_src(&rule, "//p:in").unwrap().repository,
            "dep+"
        );
    }

    #[test]
    fn external_make_locations_and_rule_directory_use_execution_paths() {
        let label = Label::parse_canonical("@@dep+//p:target").unwrap();
        let input = label.with_name("in").unwrap().source_exec_path();
        let output = label
            .with_name("nested/out")
            .unwrap()
            .generated_exec_path(crate::label::GENERATED_EXEC_ROOT);
        let locations = BTreeMap::from([("@@dep+//p:in".into(), vec![input])]);
        let expanded = expand_command_in(
            "$(location //p:in) $(location :in) $(RULEDIR) $(@D)",
            &[],
            &[output],
            &locations,
            &label,
            &RepositorySet::new(PathBuf::new()),
            crate::label::GENERATED_EXEC_ROOT,
        )
        .unwrap();
        assert_eq!(
            expanded,
            "external/dep+/p/in external/dep+/p/in bazel-out/rust-fastbuild/bin/external/dep+/p bazel-out/rust-fastbuild/bin/external/dep+/p/nested"
        );
    }

    struct Fixture(PathBuf);

    impl Fixture {
        fn new() -> Self {
            use std::sync::atomic::{AtomicU64, Ordering};
            static NEXT: AtomicU64 = AtomicU64::new(0);
            let root = PathBuf::from("/tmp/opencode").join(format!(
                "engine-repositories-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            std::fs::create_dir_all(&root).unwrap();
            Self(root)
        }

        fn write(&self, path: &str, contents: &str) {
            let path = self.0.join(path);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, contents).unwrap();
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// A workspace whose `//:top` depends on `//:leaf` in the target and
    /// (cfg = "exec") the exec configuration, every rule resolving a
    /// toolchain, with the explicit host platform //p:linux.
    fn two_configuration_engine(
        build: &str,
        registrations: &str,
        extra_defs: &str,
    ) -> (Fixture, Engine) {
        let f = Fixture::new();
        f.write(
            "MODULE.bazel",
            &format!("module(name='main')\nregister_toolchains('//tc:all'{registrations})\n"),
        );
        f.write(
            "os/BUILD",
            "constraint_setting(name='os', visibility=['//visibility:public'])\nconstraint_value(name='linux', constraint_setting=':os', visibility=['//visibility:public'])\n",
        );
        f.write(
            "p/BUILD",
            "platform(name='linux', constraint_values=['//os:linux'], visibility=['//visibility:public'])\n",
        );
        f.write("r/BUILD", "");
        f.write(
            "r/defs.bzl",
            &format!("def _impl_impl(ctx):\n    return [platform_common.ToolchainInfo(name='tc'), DefaultInfo()]\nimpl = rule(implementation=_impl_impl)\ndef _gen_impl(ctx):\n    out = ctx.actions.declare_file(ctx.label.name + '.txt')\n    ctx.actions.write(out, ' '.join([f.path for f in ctx.files.tool + ctx.files.dep]) + ctx.toolchains['//tc:type'].name)\n    return [DefaultInfo(files=depset([out] + ctx.files.tool + ctx.files.dep))]\ngen = rule(implementation=_gen_impl, toolchains=['//tc:type'], attrs={{'tool': attr.label(cfg='exec'), 'dep': attr.label()}})\n{extra_defs}"),
        );
        f.write(
            "tc/BUILD",
            "load('//r:defs.bzl', 'impl')\ntoolchain_type(name='type', visibility=['//visibility:public'])\nimpl(name='impl')\ntoolchain(name='tc', toolchain_type=':type', toolchain=':impl')\n",
        );
        f.write("BUILD", build);
        let mut engine = Engine::new(f.0.clone());
        let mut options = crate::configuration::CommandLineOptions::default();
        options.parse("--host_platform=//p:linux", None).unwrap();
        engine.set_configuration(
            options
                .configuration(&crate::configuration::parse_canonical_option_label)
                .unwrap(),
        );
        (f, engine)
    }

    #[test]
    fn one_engine_analyzes_a_target_in_two_configurations() {
        let (_f, mut engine) = two_configuration_engine(
            "load('//r:defs.bzl', 'gen')\ngen(name='leaf')\ngen(name='top', tool=':leaf', dep=':leaf')\n",
            "",
            "",
        );
        let labels = engine.select_for_build(&["//:top".into()], "").unwrap();
        let (nodes, requested) = engine.plan_build(&labels).unwrap();
        let top = engine.configurations.top();
        let key = |name: &str, config| ConfiguredTargetKey::new(test_label(name), config);
        let exec = engine.exec_configurations[&key("//:top", top)];
        assert_ne!(exec, top);
        assert_eq!(engine.configurations.mnemonic(exec), "linux-opt-exec");
        // Separate actions: `L#n` in the top-level configuration,
        // `L[config8]#n` in the exec configuration; separate output roots,
        // so the producers of the two leaf outputs never conflict.
        let actions = nodes
            .iter()
            .map(|node| {
                (
                    node.action.id.clone(),
                    node.action.outputs[0].display().to_string(),
                )
            })
            .collect::<BTreeMap<_, _>>();
        let exec_leaf = format!("//:leaf[{}]#0", &exec.to_string()[..8]);
        assert_eq!(
            actions,
            BTreeMap::from([
                (
                    "//:leaf#0".to_owned(),
                    "bazel-out/rust-fastbuild/bin/leaf.txt".to_owned()
                ),
                (
                    exec_leaf.clone(),
                    "bazel-out/linux-opt-exec/bin/leaf.txt".to_owned()
                ),
                (
                    "//:top#0".to_owned(),
                    "bazel-out/rust-fastbuild/bin/top.txt".to_owned()
                ),
            ])
        );
        // The consumer sees both configured leaves (its write content).
        let top_action = nodes
            .iter()
            .find(|node| node.action.id == "//:top#0")
            .unwrap();
        assert_eq!(
            top_action.action.write.as_ref().unwrap().content,
            b"bazel-out/linux-opt-exec/bin/leaf.txt bazel-out/rust-fastbuild/bin/leaf.txttc"
        );
        // The requested target's files: its output and both leaves'.
        assert_eq!(
            requested
                .iter()
                .map(|(requested, artifact)| {
                    assert_eq!(*requested, key("//:top", top));
                    artifact.exec.display().to_string()
                })
                .collect::<Vec<_>>(),
            [
                "bazel-out/rust-fastbuild/bin/top.txt",
                "bazel-out/linux-opt-exec/bin/leaf.txt",
                "bazel-out/rust-fastbuild/bin/leaf.txt"
            ]
        );
        // Toolchain contexts and registered toolchains per configuration.
        assert_eq!(
            engine
                .toolchain_contexts
                .keys()
                .cloned()
                .collect::<Vec<_>>(),
            {
                let mut keys = vec![
                    key("//:leaf", top),
                    key("//:leaf", exec),
                    key("//:top", top),
                ];
                keys.sort();
                keys
            }
        );
        assert_eq!(
            engine
                .registered_toolchains
                .keys()
                .copied()
                .collect::<BTreeSet<_>>(),
            BTreeSet::from([top, exec])
        );
        assert!(engine.computing_toolchains.is_empty());
        // Exec of exec is the same configuration; nothing else was interned.
        assert_eq!(
            engine
                .configurations
                .intern_exec(exec, test_label("//p:linux"), &BTreeSet::new()),
            Ok(exec)
        );
    }

    /// The registered-toolchain guards are per configuration: computing
    /// the top-level registry analyzes a registered target whose exec edge
    /// needs the exec configuration's registry, which is computed nested
    /// instead of being reported as a toolchain cycle.
    #[test]
    fn nested_exec_registries_are_not_toolchain_cycles() {
        let (_f, mut engine) = two_configuration_engine(
            "load('//r:defs.bzl', 'gen', 'plain')\nconfig_setting(name='opt', values={'compilation_mode': 'opt'})\ngen(name='leaf')\nplain(name='reg', tool=select({':opt': None, '//conditions:default': ':leaf'}))\ngen(name='top')\n",
            ", '//:reg'",
            "def _plain_impl(ctx):\n    return []\nplain = rule(implementation=_plain_impl, attrs={'tool': attr.label(cfg='exec')})\n",
        );
        let labels = engine.select_for_build(&["//:top".into()], "").unwrap();
        let error = engine.plan_build(&labels).unwrap_err();
        assert!(!error.contains("Misconfigured toolchains"), "{error}");
        assert!(
            error.contains("invalid registered toolchain '//:reg': target does not provide the DeclaredToolchainInfo provider"),
            "{error}"
        );
        assert!(engine.computing_toolchains.is_empty());
        // The exec configuration's registry was computed (nested) and kept.
        assert_eq!(engine.registered_toolchains.len(), 2);
    }

    #[test]
    fn cycles_through_exec_edges_print_each_configuration() {
        let (_f, mut engine) = two_configuration_engine(
            "load('//r:defs.bzl', 'gen')\ngen(name='a', tool=':b')\ngen(name='b', tool=':a')\n",
            "",
            "",
        );
        let labels = engine.select_for_build(&["//:a".into()], "").unwrap();
        let error = engine.plan_build(&labels).unwrap_err();
        let top = engine.configurations.top();
        let exec = engine.exec_configurations[&ConfiguredTargetKey::new(test_label("//:a"), top)];
        // The requested rule's untrimmed top-level key precedes its own.
        let untrimmed = engine.configurations.get(top).untrimmed_id();
        assert_ne!(untrimmed, top);
        assert!(
            error.ends_with(&format!(
                ": in gen rule //:b: cycle in dependency graph:\n    //:a ({untrimmed})\n    //:a ({top})\n.-> //:b ({exec})\n|   //:a ({exec})\n`-- //:b ({exec})"
            )),
            "{error}"
        );
        // A requested rule inside its own cycle: only the untrimmed key is
        // on the path.
        let (_f, mut engine) = two_configuration_engine(
            "load('//r:defs.bzl', 'gen')\ngen(name='a', dep=':b')\ngen(name='b', dep=':a')\n",
            "",
            "",
        );
        let labels = engine.select_for_build(&["//:a".into()], "").unwrap();
        let error = engine.plan_build(&labels).unwrap_err();
        let top = engine.configurations.top();
        let untrimmed = engine.configurations.get(top).untrimmed_id();
        assert!(
            error.ends_with(&format!(
                ": in gen rule //:a: cycle in dependency graph:\n    //:a ({untrimmed})\n.-> //:a ({top})\n|   //:b ({top})\n`-- //:a ({top})"
            )),
            "{error}"
        );
    }

    #[test]
    fn repository_patterns_and_native_build_keep_equal_packages_separate() {
        let f = Fixture::new();
        f.write("main/MODULE.bazel", "module(name='main')\nbazel_dep(name='dep',version='1',repo_name='alias')\nlocal_path_override(module_name='dep',path='../dep')\n");
        f.write("dep/MODULE.bazel", "module(name='dep')\n");
        let build =
            "genrule(name='copy',srcs=['in'],outs=['out'],cmd='cat $(location :in) > $@')\n";
        f.write("main/p/BUILD", build);
        f.write("dep/p/BUILD", build);
        f.write("main/p/in", "main contents");
        f.write("dep/p/in", "dependency contents");
        let mut engine = Engine::new(f.0.join("main"));
        let main = engine.select(&["//...".into()], "").unwrap();
        assert_eq!(
            main.iter().map(ToString::to_string).collect::<Vec<_>>(),
            ["//p:copy"]
        );
        let external = engine.select(&["@alias//p:all".into()], "").unwrap();
        assert_eq!(
            external.iter().map(ToString::to_string).collect::<Vec<_>>(),
            ["@@dep+//p:copy"]
        );
        for pattern in ["@alias//...", "@@dep+//p/..."] {
            assert_eq!(engine.select(&[pattern.into()], "").unwrap(), external);
        }
        let labels = main.into_iter().chain(external).collect::<Vec<_>>();
        let summary = engine.build(&labels, &f.0.join("cache")).unwrap();
        assert_eq!(summary.executed, 2);
        for (label, path) in summary.outputs {
            let expected = if label.repository.is_empty() {
                "main contents"
            } else {
                "dependency contents"
            };
            assert_eq!(std::fs::read_to_string(path).unwrap(), expected);
        }
    }

    #[test]
    fn make_expansion_is_strict() {
        let srcs = vec![PathBuf::from("pkg/in")];
        let outs = vec![PathBuf::from("out/file")];
        assert_eq!(
            expand_command(
                "cat $< > $@; echo $$HOME $(SRCS) $(@D)",
                &srcs,
                &outs,
                &BTreeMap::new(),
                "pkg"
            )
            .unwrap(),
            "cat pkg/in > out/file; echo $HOME pkg/in out"
        );
        for cmd in [
            "$HOME",
            "$(UNKNOWN)",
            "$(location :not_declared)",
            "$(SRCS",
            "$",
        ] {
            assert!(
                expand_command(cmd, &srcs, &outs, &BTreeMap::new(), "pkg").is_err(),
                "{cmd}"
            );
        }
        assert!(expand_command("$<", &[], &outs, &BTreeMap::new(), "pkg").is_err());
    }
}
