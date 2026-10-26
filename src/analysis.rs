//! Filesystem-free analysis of the supported Starlark custom-rule subset.

use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

use anyhow::{anyhow, bail, ensure};
use starlark::environment::Module;
use starlark::eval::Evaluator;
use starlark::values::structs::AllocStruct;
use starlark::values::{FrozenHeapName, Heap, OwnedFrozenValue, Value};

use crate::custom_rule::{AttrKind, AttrSpec, AttrValue};
use crate::package::Rule;
use crate::repository::RepositorySet;
use crate::starlark_types::{File, Target, alloc_file, alloc_shared_target, alloc_target};
mod split;

/// What a consumer sees of one dependency edge's configured target.
#[derive(Clone, Debug)]
pub struct DependencyView {
    pub label: String,
    pub files: Vec<File>,
    pub is_source: bool,
    /// Actual ordinary rule target, not a source/output file or package group.
    pub is_rule: bool,
    /// Actual package-group kind; never inferred from provider presence.
    pub is_package_group: bool,
    /// Actual native `filegroup` rule (Filegroup.java).
    pub is_filegroup: bool,
    pub providers: BTreeMap<String, OwnedFrozenValue>,
    /// Display names of the returned declared providers, in return order.
    pub provider_keys: Vec<String>,
    /// RunfilesProvider/FilesToRunProvider (docs/RUNFILES.md), shared with
    /// the engine's planned runfiles of the target (W1a).
    pub runfiles: Arc<crate::runfiles::TargetRunfiles>,
    /// The actual rule's class name (ConfiguredTargetAndData.getRuleClass):
    /// `None` for files and package groups.
    pub rule_class: Option<String>,
    /// The configured target's one shared `Target` value (N55): the engine
    /// allocates it once per configured target — keyed by the label and the
    /// configuration Bazel keys it under (a rule/alias edge's
    /// post-delegation configuration, no configuration for a source file
    /// or package group, the generating rule's for an output file) — so
    /// every consumer's `ctx.attr`, every provider that stores it and
    /// every depset snapshot references the same `Arc<Target>`
    /// (crate::engine `ActionPlanning`; Bazel's one ConfiguredTarget
    /// instance per label and configuration).
    pub(crate) identity: std::sync::Arc<Target>,
}

/// A dependency edge's [`DependencyView`], shared: the engine memoizes one
/// view per requested label and edge configuration and hands every edge
/// of that key the same view (W1a, PLAN-L1 §2.1), so cloning is an `Arc`
/// bump. Fields read through `Deref`; writes through `DerefMut` are
/// copy-on-write, so a handle keeps value semantics (a change never
/// reaches another handle of the same view).
#[derive(Clone)]
pub struct DependencyTarget(Arc<DependencyView>);

impl DependencyTarget {
    pub fn new(view: DependencyView) -> Self {
        Self(Arc::new(view))
    }
}

impl From<DependencyView> for DependencyTarget {
    fn from(view: DependencyView) -> Self {
        Self::new(view)
    }
}

impl std::ops::Deref for DependencyTarget {
    type Target = DependencyView;

    fn deref(&self) -> &DependencyView {
        &self.0
    }
}

impl std::ops::DerefMut for DependencyTarget {
    fn deref_mut(&mut self) -> &mut DependencyView {
        Arc::make_mut(&mut self.0)
    }
}

impl std::fmt::Debug for DependencyTarget {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}

use crate::analysis_runtime::Context;
pub use crate::analysis_runtime::RegisteredAction;
pub(crate) use crate::analysis_runtime::repository_context;

#[derive(Debug)]
pub struct AnalysisResult {
    pub files: Vec<File>,
    /// The package-relative declared outputs (not the shareable artifacts).
    pub declared: Vec<File>,
    pub actions: Vec<RegisteredAction>,
    pub providers: BTreeMap<String, OwnedFrozenValue>,
    /// Display names of the returned declared providers, in return order.
    pub provider_keys: Vec<String>,
    /// RunfilesProvider/FilesToRunProvider of the configured target.
    pub runfiles: crate::runfiles::TargetRunfiles,
}

/// The Label of a predeclared output file of `rule`.
fn output_label<'v>(heap: Heap<'v>, rule: &Rule, output: &str) -> anyhow::Result<Value<'v>> {
    let label = rule.label.with_name(output).map_err(anyhow::Error::msg)?;
    crate::starlark_types::alloc_label(heap, &label.to_string())
}

/// A `ctx.attr` Target whose repr names its configured-target class, with
/// its runfiles providers. A requested label that differs from the resolved
/// one is an alias.
/// One Target value per configured target for the whole analysis (N55):
/// Bazel hands the same ConfiguredTarget instance per label and
/// configuration (an alias is its own AliasConfiguredTarget) to every
/// consumer, so `ctx.attr.a[0] == ctx.attr.b.keys()[0]` within a rule
/// context, a Target carried in a dependency's provider equals the
/// consumer's own, and two attributes with the same Starlark transition see
/// the same instance. The engine keys the shared value as Bazel keys the
/// configured target: a rule/alias edge's post-delegation configuration,
/// no configuration for a source file or package group, the generating
/// rule's for an output file; exec-configured rule edges are distinct
/// keys, hence distinct instances.
fn alloc_dependency<'v>(heap: Heap<'v>, target: &DependencyTarget) -> Value<'v> {
    alloc_shared_target(heap, &target.identity)
}

/// The Target data of a dependency edge (N55): built once per
/// ConfiguredTargetKey by the engine (memoized in `ActionPlanning`), from
/// the dependency's resolved fields and the edge's requested label.
#[allow(clippy::too_many_arguments)]
pub(crate) fn dependency_target_data(
    requested: &str,
    label: &str,
    files: &[File],
    is_source: bool,
    is_rule: bool,
    is_package_group: bool,
    is_filegroup: bool,
    providers: &BTreeMap<String, OwnedFrozenValue>,
    provider_keys: &[String],
    runfiles: &Arc<crate::runfiles::TargetRunfiles>,
) -> Target {
    use crate::starlark_types::TargetKind;
    let mut provider_keys = provider_keys.to_vec();
    let actual = if is_source {
        TargetKind::InputFile
    } else if is_filegroup {
        // Filegroup adds InstrumentedFilesInfo as its only declared provider
        // (rules/filegroup/Filegroup.java:103-108) and makes a single file its
        // executable (Filegroup.java:107,117-119).
        provider_keys = vec!["InstrumentedFilesInfo".to_owned()];
        if files.len() == 1 {
            TargetKind::SingleExecutableRule
        } else {
            TargetKind::Rule
        }
    } else if is_rule {
        TargetKind::Rule
    } else if is_package_group {
        TargetKind::Unknown
    } else {
        TargetKind::OutputFile
    };
    // Both labels are canonical forms (crate::label::Label's Display).
    let kind = if requested != label {
        TargetKind::Alias(requested.to_owned(), Box::new(actual))
    } else {
        actual
    };
    crate::starlark_types::target_of_kind(
        label,
        files.to_vec(),
        providers.clone(),
        kind,
        provider_keys,
        Arc::clone(runfiles),
    )
}

/// The key of a dependency in the implementation's dependency map: the
/// label text of a target-configuration edge, `LABEL (exec)` for an edge
/// with the exec transition, whose dependency is configured in the exec
/// configuration (crate::engine; the same label may be both).
pub(crate) fn dependency_key(label: &str, exec: bool) -> String {
    if exec {
        format!("{label} (exec)")
    } else {
        label.to_owned()
    }
}

/// The key of a `package_metadata` dependency, configured in the no-config
/// configuration (crate::engine, EdgeCfg::NoConfig).
pub(crate) fn metadata_dependency_key(label: &str) -> String {
    format!("{label} (noconfig)")
}

/// The key of a dependency through attribute `attribute`'s Starlark
/// transition: `LABEL (transition ATTRIBUTE)` (crate::engine; the
/// dependency is configured in that attribute's transitioned configuration).
pub(crate) fn transition_dependency_key(label: &str, attribute: &str) -> String {
    format!("{label} (transition {attribute})")
}

pub(crate) fn split_dependency_key(
    label: &str,
    attribute: &str,
    config: crate::configuration::ConfigId,
) -> String {
    format!("{label} (transition {attribute} {config})")
}

pub(crate) fn dependency_labels(value: &AttrValue) -> Vec<&str> {
    match value {
        AttrValue::Label(Some(label)) => vec![label.as_str()],
        AttrValue::Labels(labels) => labels.iter().map(String::as_str).collect(),
        AttrValue::LabelKeyedStringDict(entries) => {
            entries.iter().map(|(label, _)| label.as_str()).collect()
        }
        AttrValue::StringKeyedLabelDict(entries) => {
            entries.iter().map(|(_, label)| label.as_str()).collect()
        }
        AttrValue::LabelListDict(entries) => entries
            .iter()
            .flat_map(|(_, labels)| labels.iter().map(String::as_str))
            .collect(),
        _ => Vec::new(),
    }
}

/// The key of `label` in the dependency map through attribute `name`: its
/// edge configuration (target, exec or the attribute's Starlark transition).
pub(crate) fn attribute_dependency_key(label: &str, name: &str, spec: &AttrSpec) -> String {
    if starlark_transition(spec) {
        transition_dependency_key(label, name)
    } else {
        dependency_key(
            label,
            matches!(spec.declaration.cfg, crate::rule_declarations::Cfg::Exec(_)),
        )
    }
}

/// Whether the attribute named `name` (a rule-class attribute, Starlark
/// spelling) is an implicit dependency whose validation outputs never
/// propagate: a `$`-prefixed native name (Attribute.isImplicit,
/// Attribute.java:2265-2275), or a `_`-prefixed Starlark name of an
/// attribute whose value source converts `_x` to the native `$x` — DIRECT
/// and COMPUTED_DEFAULT (AttributeValueSource.java:25-28, 64-72). A
/// LATE_BOUND (or MATERIALIZER, unmodelled here) `_x` converts to `:x`,
/// which is NOT implicit (AttributeValueSource.java:25-26): its
/// prerequisite's `_validation` group propagates like any explicit
/// dependency's (unless it is a tool dependency).
pub(crate) fn implicit_attribute(name: &str, spec: &AttrSpec) -> bool {
    name.starts_with('$') || (name.starts_with('_') && !spec.declaration.late_bound)
}

/// The built-in dependency-class attributes every Starlark rule class
/// carries (StarlarkRuleClassFunctions.baseRule over
/// BaseRuleClasses.commonCoreAndStarlarkAttributes), in rule-class order,
/// with whether Attribute.isToolDependency (Attribute.java:2119-2127)
/// filters them: `aspect_hints` (BaseRuleClasses.java:298) and `toolchains`
/// (:167-170 of StarlarkRuleClassFunctions) are collected;
/// `exec_compatible_with` and `target_compatible_with` are walked and
/// dropped — `.tool("... exists for constraint checking, not to create an
/// actual dependency")` (StarlarkRuleClassFunctions.java:174-177, 186-192).
/// `visibility` is a nodep label list; `$`/`:`-prefixed built-ins
/// (`:action_listener`, `$config_dependencies`, the no-config environment
/// and `package_metadata` attributes) are implicit or tool dependencies.
const BUILTIN_DEPENDENCY_ATTRIBUTES: [(&str, bool); 4] = [
    ("aspect_hints", false),
    ("toolchains", false),
    ("exec_compatible_with", true),
    ("target_compatible_with", true),
];

/// RuleConfiguredTargetBuilder.collectTransitiveValidationOutputGroups
/// (:397-425): the `_validation` groups of every prerequisite reached
/// through the rule class's label-class attributes that are not implicit
/// (`$`-prefixed native names; `_`-prefixed Starlark DIRECT/
/// COMPUTED_DEFAULT names, which become `$x` — AttributeValueSource.java:
/// 25-28, 64-72), not tool dependencies (an exec/no-config transition or a
/// `.tool()` built-in, Attribute.isToolDependency :2119-2127) and not
/// `skip_validations`. Bazel iterates `ruleContext.attributes().
/// getAttributeNames()` — the rule class's attributes in class order:
/// built-ins first (BUILTIN_DEPENDENCY_ATTRIBUTES), then the declared
/// attributes in declaration order (`attr_order`), so the collected
/// to_list() order matches. The framework's own edges (resolved toolchain
/// implementations, implicit test-runner attributes) are not attribute
/// prerequisites and contribute nothing. `aspect_hints` prerequisites are
/// not analyzed by the native profile (aspect machinery), so that walk
/// finds no entries.
///
/// Returns the prerequisites' non-empty `_validation` sets in that order
/// (crate::output_group_info::validation_member), not their files: the
/// caller composes them into one node (output_group_info::add_transitive,
/// W1c), whose `to_list` is the first-occurrence order of their flattened
/// files.
fn transitive_validation(
    rule: &Rule,
    invocation: &crate::custom_rule::CustomInvocation,
    dependencies: &BTreeMap<String, DependencyTarget>,
    repositories: &RepositorySet,
) -> Vec<Arc<crate::starlark_types::DepsetNode>> {
    let mut members = Vec::new();
    let mut collect = |target: &DependencyTarget| {
        if let Some(group) = target
            .providers
            .get(crate::output_group_info::PROVIDER_ID)
            .and_then(|info| crate::output_group_info::validation(info.value()))
        {
            members.push(crate::output_group_info::validation_member(group));
        }
    };
    for (name, tool) in BUILTIN_DEPENDENCY_ATTRIBUTES {
        // A `.tool()` built-in is walked and dropped, exactly like Bazel's
        // isToolDependency filter.
        if tool {
            continue;
        }
        for text in invocation.declaration.builtin.target.labels(name) {
            let Ok(label) = repositories.parse(
                text.as_str(),
                rule.label.repository.as_str(),
                rule.label.package.as_str(),
            ) else {
                continue;
            };
            if let Some(target) = dependencies.get(&dependency_key(&label.to_string(), false)) {
                collect(target);
            }
        }
    }
    for name in &invocation.declaration.attr_order {
        let Some(spec) = invocation.schema.get(name) else {
            continue;
        };
        if implicit_attribute(name, spec)
            || spec.declaration.skip_validations
            || matches!(spec.declaration.cfg, crate::rule_declarations::Cfg::Exec(_))
        {
            continue;
        }
        let Some(values) = invocation.attrs.get(name) else {
            continue;
        };
        if let Some(split) = invocation.split_dependencies.get(name) {
            for p in &split.prerequisites {
                if let Some(target) = dependencies.get(&p.map_key) {
                    collect(target);
                }
            }
            continue;
        }
        // Type.getLabelClass() == LabelClass.DEPENDENCY (BuildType): label,
        // label_list, label_keyed_string_dict, string_keyed_label_dict and
        // label_list_dict; outputs are not dependency attributes.
        let labels: Vec<&str> = match (&spec.kind, values) {
            (AttrKind::Label, AttrValue::Label(Some(label))) => vec![label.as_str()],
            (AttrKind::LabelList, AttrValue::Labels(labels)) => {
                labels.iter().map(String::as_str).collect()
            }
            (AttrKind::LabelKeyedStringDict, AttrValue::LabelKeyedStringDict(entries)) => {
                entries.iter().map(|(label, _)| label.as_str()).collect()
            }
            (AttrKind::StringKeyedLabelDict, AttrValue::StringKeyedLabelDict(entries)) => {
                entries.iter().map(|(_, label)| label.as_str()).collect()
            }
            (AttrKind::LabelListDict, AttrValue::LabelListDict(entries)) => entries
                .iter()
                .flat_map(|(_, labels)| labels.iter().map(String::as_str))
                .collect(),
            _ => continue,
        };
        for label in labels {
            let Some(target) = dependencies.get(&attribute_dependency_key(label, name, spec))
            else {
                continue;
            };
            collect(target);
        }
    }
    members
}

/// Whether `spec`'s `cfg` is a Starlark (non analysis-test) transition.
fn starlark_transition(spec: &AttrSpec) -> bool {
    matches!(
        spec.declaration.cfg,
        crate::rule_declarations::Cfg::Starlark {
            analysis_test: false,
            ..
        }
    )
}

/// The consuming rule of a prerequisite in RuleContext messages: its rule
/// class and canonical label.
#[derive(Clone, Copy, Debug, Default)]
pub struct Consumer<'a> {
    pub rule_class: &'a str,
    pub label: &'a str,
}

fn canonical_text(label: &str) -> String {
    crate::label::Label::parse_canonical(label)
        .map(|label| crate::rule_declarations::canonical_form(&label))
        .unwrap_or_else(|_| label.to_owned())
}

/// RuleContext.Builder.validateDirectPrerequisite for a prerequisite that is
/// not a file (analysis/RuleContext.java:2165-2172), every attribute error in
/// Bazel's order, without the `in A attribute of K rule L: ` prefix:
/// validateRuleDependency's mandatory providers (:1851-1855, 2070-2095),
/// validateDirectPrerequisiteFileTypes (:1945-1994), then
/// CommonPrerequisiteValidator.checkForMisplacedPackageGroups
/// (analysis/CommonPrerequisiteValidator.java:48-59, 343-378). File targets
/// are validated by crate::engine (their errors precede analysis). Labels
/// are the prerequisite's own (ConfiguredTargetAndData.getTargetLabel, the
/// alias's actual).
///
/// `described` is the prerequisite's AliasProvider.describeTargetWithAliases
/// WITH_KIND text (`KIND rule 'L'`, through its alias chain); without it the
/// target is described without aliases.
pub(crate) fn prerequisite_errors(
    name: &str,
    spec: &AttrSpec,
    target: &DependencyTarget,
    consumer: Consumer<'_>,
    described: Option<&str>,
) -> Vec<String> {
    let mut errors = Vec::new();
    let shown = canonical_text(&target.label);
    if target.is_rule {
        let provided = |provider: &crate::providers::ProviderRequirement| {
            // Every rule target has an OutputGroupInfo (HIDDEN_TOP_LEVEL).
            provider.id == crate::providers::DEFAULT_INFO_PROVIDER_ID
                || provider.id == crate::output_group_info::PROVIDER_ID
                || target.providers.contains_key(&provider.id)
        };
        // RuleContext.Builder.validateRuleDependency (analysis/RuleContext.java:2002-2022):
        // an allowed rule class accepts the prerequisite (checkRuleDependencyClass,
        // :2024-2045); otherwise the rejected class is remembered, satisfied
        // mandatory providers still accept it (:2070-2096), and the unfulfilled
        // requirements are joined with StringUtil.joinEnglishList(_, "and").
        // Starlark attributes never set a warning predicate (:2052-2068).
        let rule_class = target.rule_class.as_deref().unwrap_or_default();
        let mut unfulfilled = Vec::new();
        let class_allowed = match &spec.declaration.allow_rules {
            None => false,
            Some(allowed) if allowed.iter().any(|allowed| allowed == rule_class) => true,
            Some(allowed) => {
                // RuleClassNamePredicate.only: an ImmutableSet of the names,
                // described by StringUtil.joinEnglishList (packages/RuleClass.java:541-548).
                let mut names: Vec<&str> = Vec::new();
                for allowed in allowed {
                    if !names.contains(&allowed.as_str()) {
                        names.push(allowed);
                    }
                }
                let described = described
                    .map(str::to_owned)
                    .unwrap_or_else(|| format!("{rule_class} rule '{shown}'"));
                // badPrerequisiteMessage (:1819-1830).
                unfulfilled.push(format!(
                    "{described} is misplaced here (expected {})",
                    join_english_list(&names, "or")
                ));
                false
            }
        };
        // RuleContext.Builder.checkRuleDependencyMandatoryProviders
        // (analysis/RuleContext.java:2070-2095) with the missing providers'
        // RequiredProviders.getDescription (packages/RequiredProviders.java:157-190,
        // 207-212, 308-321): per alternative, the missing providers, quoted,
        // bracketed when several, joined with " or ".
        let providers_satisfied = !spec.required_providers.is_empty()
            && spec
                .required_providers
                .iter()
                .any(|clause| clause.iter().all(provided));
        if class_allowed || providers_satisfied {
            unfulfilled.clear();
        } else if !spec.required_providers.is_empty() {
            unfulfilled.push(format!(
                "'{}' does not have mandatory providers: {}",
                target.label,
                spec.required_providers
                    .iter()
                    .map(|clause| {
                        let missing = clause
                            .iter()
                            .filter(|provider| !provided(provider))
                            .map(|provider| format!("'{}'", provider.display_name))
                            .collect::<Vec<_>>();
                        if missing.len() > 1 {
                            format!("[{}]", missing.join(", "))
                        } else {
                            missing.join(", ")
                        }
                    })
                    .collect::<Vec<_>>()
                    .join(" or ")
            ));
        }
        if !unfulfilled.is_empty() {
            errors.push(join_english_list(&unfulfilled, "and"));
        }
    }
    // validateDirectPrerequisiteFileTypes: a single-file attribute needs
    // exactly one file (:1966-1974) and stops there; then one file to build
    // must have an allowed type (:1975-1993).
    if spec.allow_single_file && target.files.len() != 1 {
        errors.push(format!("'{shown}' must produce a single file"));
    } else if let Some(error) = spec.produced_file_type_error(
        name,
        consumer.rule_class,
        &shown,
        target
            .files
            .iter()
            .map(|file| file.path.rsplit('/').next().unwrap_or("")),
    ) {
        errors.push(error);
    }
    // Upstream's placement exception deliberately uses printable-name
    // substrings, not provider identity or AND/OR satisfaction. Groups are
    // non-rules (TargetData.getRuleClass is empty), so a missing provider
    // does not reject them. The message carries its own attribute prefix,
    // with the native attribute name (:365-375), so Bazel prints it twice.
    if target.is_package_group
        && !matches!(name, "visibility" | "_allowlist_function_transition")
        && !spec.required_providers.iter().flatten().any(|provider| {
            provider
                .display_name
                .contains("PackageSpecificationProvider")
                || provider.display_name.contains("PackageSpecificationInfo")
        })
    {
        errors.push(
            crate::rule_errors::RuleEvents {
                rule_class: consumer.rule_class,
                label: consumer.label,
            }
            .attribute_message(
                &crate::custom_rule::native_attribute_name(name, spec.declaration.late_bound),
                &format!(
                    "package group '{shown}' is misplaced here (they are only allowed in the visibility attribute)"
                ),
            ),
        );
    }
    errors
}

/// StringUtil.joinEnglishList(choices, lastSeparator) with the Oxford comma
/// (util/StringUtil.java:44-73): `a`, `a or b`, `a, b, or c`; `nothing` when empty.
pub(crate) fn join_english_list<S: AsRef<str>>(choices: &[S], last_separator: &str) -> String {
    let mut text = String::new();
    for (index, choice) in choices.iter().enumerate() {
        if index > 0 {
            let last = index + 1 == choices.len();
            if !last || index >= 2 {
                text.push(',');
            }
            if last {
                text.push(' ');
                text.push_str(last_separator);
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

fn dependency<'a>(
    label: &str,
    name: &str,
    spec: &AttrSpec,
    dependencies: &'a BTreeMap<String, DependencyTarget>,
    prefix: &str,
    consumer: Consumer<'_>,
) -> anyhow::Result<&'a DependencyTarget> {
    let target = dependencies
        .get(&attribute_dependency_key(label, name, spec))
        .ok_or_else(|| anyhow!("attribute {name}: dependency {label} was not resolved"))?;
    checked_dependency(label, name, spec, target, prefix, consumer)
}

fn checked_dependency<'a>(
    label: &str,
    name: &str,
    spec: &AttrSpec,
    target: &'a DependencyTarget,
    prefix: &str,
    consumer: Consumer<'_>,
) -> anyhow::Result<&'a DependencyTarget> {
    ensure!(
        !target.is_source || spec.allow_files || spec.allow_single_file,
        "attribute {name}: source file {label} is not allowed (allow_files=False)"
    );
    // crate::engine reports every prerequisite error before analysis; this
    // keeps the direct analysis entry points checked.
    // The alias chain is unknown here; a requested label naming another
    // target is described as a one-step alias.
    let described = target.rule_class.as_ref().map(|class| {
        let actual = format!("{class} rule '{}'", canonical_text(&target.label));
        if label == target.label {
            actual
        } else {
            format!("alias '{}' referring to {actual}", canonical_text(label))
        }
    });
    if let Some(error) = prerequisite_errors(name, spec, target, consumer, described.as_deref())
        .into_iter()
        .next()
    {
        bail!("{prefix}{error}");
    }
    Ok(target)
}

/// A label-list attribute's `ctx.files` entry (StarlarkAttributesCollection
/// .addAttribute, analysis/starlark/StarlarkAttributesCollection.java:398-409,
/// over PrerequisiteArtifacts.nestedSet, PrerequisiteArtifacts.java:64-72):
/// a stable-order NestedSet of every prerequisite's files to build, so its
/// list has each File once, at its first occurrence in prerequisite order
/// (a filegroup over another prerequisite's file adds nothing).
fn prerequisite_files<'v, 'a>(
    heap: Heap<'v>,
    prerequisites: impl Iterator<Item = &'a [File]>,
) -> Value<'v> {
    let mut seen = BTreeSet::new();
    let mut files = Vec::new();
    for file in prerequisites.flatten() {
        if seen.insert(file) {
            files.push(alloc_file(heap, file.clone()));
        }
    }
    heap.alloc(files)
}

/// Configured prerequisites of a label-valued dictionary attribute.
struct DictPrerequisites<'v, 'a> {
    /// One Target value per distinct label.
    values: BTreeMap<&'a str, Value<'v>>,
    /// `ctx.files` entry: PrerequisiteArtifacts.nestedSet, a stable-order
    /// deduplicated union over the distinct prerequisites in label-visitation
    /// order (analysis/PrerequisiteArtifacts.java:64-72; DictType.visitLabels,
    /// packages/Type.java:504-514).
    files: Value<'v>,
}

#[allow(clippy::too_many_arguments)]
fn dict_prerequisites<'v, 'a>(
    labels: impl Iterator<Item = &'a str>,
    name: &str,
    spec: &AttrSpec,
    dependencies: &BTreeMap<String, DependencyTarget>,
    prefix: &str,
    consumer: Consumer<'_>,
    heap: Heap<'v>,
) -> anyhow::Result<DictPrerequisites<'v, 'a>> {
    let mut values = BTreeMap::new();
    let mut files = Vec::new();
    let mut seen_files = BTreeSet::new();
    for label in labels {
        if values.contains_key(label) {
            continue;
        }
        let target = dependency(label, name, spec, dependencies, prefix, consumer)?;
        for file in &target.files {
            if seen_files.insert(file) {
                files.push(alloc_file(heap, file.clone()));
            }
        }
        values.insert(label, alloc_dependency(heap, target));
    }
    Ok(DictPrerequisites {
        values,
        files: heap.alloc(files),
    })
}

fn make_ctx<'v>(
    rule: &Rule,
    context: &Context,
    dependencies: &BTreeMap<String, DependencyTarget>,
    heap: Heap<'v>,
    surface: Arc<crate::ctx_surface::RuleSurface>,
) -> anyhow::Result<Value<'v>> {
    let invocation = rule
        .custom
        .as_ref()
        .ok_or_else(|| anyhow!("{} is not a custom rule", rule.label))?;
    // ctx.attr/files/file/outputs values are immutable in Bazel
    // (Attribute.valueToStarlark / StarlarkList.immutableCopyOf /
    // Dict.immutableCopyOf, analysis/starlark/StarlarkAttributesCollection.java:300-390):
    // mutating one fails with `trying to mutate a frozen list value`. Build
    // them on a private heap and freeze it.
    const NAMES: [&str; 7] = [
        "attr",
        "files",
        "file",
        "outputs",
        "executable",
        "lifted",
        "split_attr",
    ];
    let owned = Module::with_temp_heap(|module| -> anyhow::Result<_> {
        let structs =
            ctx_attribute_structs(rule, invocation, context, dependencies, module.heap())?;
        for (name, value) in NAMES.into_iter().zip(structs) {
            module.set(name, value);
        }
        module.set(
            "split_attr",
            split::split_attr(invocation, dependencies, module.heap())?,
        );
        let frozen = module
            .freeze_named(FrozenHeapName::User(Box::new(
                "rule context attributes".to_owned(),
            )))
            .map_err(|error| anyhow!("cannot freeze rule context attributes: {error:?}"))?;
        NAMES
            .map(|name| frozen.get(name).map_err(|error| anyhow!(error.to_string())))
            .into_iter()
            .collect::<anyhow::Result<Vec<_>>>()
    })?;
    let [attr, files, file, outputs, executable, lifted, split_attr] =
        <[OwnedFrozenValue; 7]>::try_from(owned)
            .map_err(|_| anyhow!("internal error: rule context attribute structs"))?;
    // Lifted subrule attributes are read by subrule invocations only.
    context.subrule.lifted.replace(Some(lifted));
    // ctx.outputs (the Outputs object of every rule, docs/REPR.md) carries
    // the frozen outputs struct and, for executable and test rules, the
    // on-demand ctx.outputs.executable (StarlarkRuleContext.java:344-450).
    let outputs = crate::runfiles::alloc_outputs(
        heap,
        outputs,
        context.runfiles.default_executable.clone(),
        context.active.clone(),
        crate::rule_declarations::canonical_form(&rule.label),
    );
    let executable = crate::runfiles::alloc_executable_struct(heap, executable);
    let [attr, files, file] =
        [attr, files, file].map(|value| heap.access_owned_frozen_value(&value));
    let (collect_data, collect_default) = collected_runfiles(invocation, dependencies)?;
    let label = alloc_target(heap, &rule.label.to_string(), Vec::new())
        .get_attr_error("label", heap)
        .map_err(|error| anyhow!(error.to_string()))?;
    let actions = crate::analysis_runtime::alloc_actions_for(
        heap,
        context.scope.root(),
        crate::analysis_ctx::rule_context_repr(&rule.label.to_string()),
    );
    let backing = heap.alloc(AllocStruct([
        ("split_attr", heap.access_owned_frozen_value(&split_attr)),
        ("attr", attr),
        ("files", files),
        ("file", file),
        ("outputs", outputs),
        ("executable", executable),
        ("label", label),
        ("actions", actions),
        ("$collect_data", heap.alloc(collect_data)),
        ("$collect_default", heap.alloc(collect_default)),
    ]));
    if invocation.build_setting.is_some() {
        // The configured value (StarlarkRuleContext.getBuildSettingValue,
        // analysis/starlark/StarlarkRuleContext.java:738-757): the command-line
        // value set by crate::engine, else build_setting_default.
        let value = match &invocation.setting_value {
            Some(value) => value.clone(),
            None => setting_default(invocation)?,
        };
        let ctx =
            crate::build_setting_ctx::wrap(heap, backing, value, context.scope.root(), surface);
        crate::analysis_runtime::set_rule_ctx(&actions, ctx);
        Ok(ctx)
    } else {
        let ctx = crate::analysis_ctx::alloc_rule(
            heap,
            backing,
            context.scope.root(),
            None,
            invocation.target_platform.clone(),
            invocation.toolchains.clone(),
            invocation.exec_groups.clone(),
            surface,
        );
        // The action factory returns this context value
        // (cc_internal.actions2ctx_cheat).
        crate::analysis_runtime::set_rule_ctx(&actions, ctx);
        Ok(ctx)
    }
}

/// RuleContext.getDefaultTemplateVariableProviders (analysis/RuleContext.java:900-940):
/// the TemplateVariableInfo of the prerequisites of the attributes
/// `toolchains`, `:cc_toolchain`, `$toolchains`, `$cc_toolchain`, in that
/// order (DEFAULT_MAKE_VARIABLE_ATTRIBUTES), before the resolved toolchains'.
/// Starlark `_x` attributes are `$x`, or `:x` when late-bound
/// (AttributeValueSource.convertToNativeName). The built-in `toolchains`
/// attribute's prerequisites are in the target configuration.
fn with_attribute_template_variables(
    invocation: &crate::custom_rule::CustomInvocation,
    dependencies: &BTreeMap<String, DependencyTarget>,
) -> crate::ctx_surface::CtxSurface {
    let mut surface = (*invocation.surface).clone();
    let mut from_attributes = Vec::new();
    for label in invocation.declaration.builtin.target.labels("toolchains") {
        let key = crate::label::Label::parse_canonical(&label)
            .map(|label| label.to_string())
            .unwrap_or(label);
        if let Some(variables) = dependencies
            .get(&dependency_key(&key, false))
            .and_then(|target| {
                target
                    .providers
                    .get(crate::template_variable_info::PROVIDER_ID)
            })
            .and_then(|provider| crate::template_variable_info::variables(provider.value()))
        {
            from_attributes.push(variables);
        }
    }
    for (name, late_bound) in [
        ("_cc_toolchain", true),
        ("_toolchains", false),
        ("_cc_toolchain", false),
    ] {
        let Some(spec) = invocation.schema.get(name) else {
            continue;
        };
        if spec.declaration.late_bound != late_bound {
            continue;
        }
        if let Some(split) = invocation.split_dependencies.get(name) {
            for p in &split.prerequisites {
                if let Some(variables) = dependencies
                    .get(&p.map_key)
                    .and_then(|target| {
                        target
                            .providers
                            .get(crate::template_variable_info::PROVIDER_ID)
                    })
                    .and_then(|provider| crate::template_variable_info::variables(provider.value()))
                {
                    from_attributes.push(variables);
                }
            }
            continue;
        }
        let labels = match invocation.attrs.get(name) {
            Some(AttrValue::Label(Some(label))) => vec![label.clone()],
            Some(AttrValue::Labels(labels)) => labels.clone(),
            _ => Vec::new(),
        };
        let exec = matches!(spec.declaration.cfg, crate::rule_declarations::Cfg::Exec(_));
        for label in labels {
            if let Some(variables) = dependencies
                .get(&dependency_key(&label, exec))
                .and_then(|target| {
                    target
                        .providers
                        .get(crate::template_variable_info::PROVIDER_ID)
                })
                .and_then(|provider| crate::template_variable_info::variables(provider.value()))
            {
                from_attributes.push(variables);
            }
        }
    }
    from_attributes.append(&mut surface.template_variables);
    surface.template_variables = from_attributes;
    surface
}

/// The per-target facts of the N23c `ctx` members (crate::ctx_surface).
fn rule_surface(
    rule: &Rule,
    invocation: &crate::custom_rule::CustomInvocation,
    repositories: &RepositorySet,
    dependencies: &BTreeMap<String, DependencyTarget>,
) -> crate::ctx_surface::RuleSurface {
    let label = crate::rule_declarations::canonical_form(&rule.label);
    crate::ctx_surface::RuleSurface {
        config: Arc::new(with_attribute_template_variables(invocation, dependencies)),
        rule_class: invocation.declaration.rule_class.clone(),
        label: rule.label.clone(),
        declared_fragments: invocation.declaration.fragments.clone(),
        target_description: format!("{} rule {label}", invocation.declaration.rule_class),
        has_alwayslink_attribute: invocation.schema.contains_key("alwayslink"),
        split_transition_attribute: invocation.schema.values().any(|spec| {
            matches!(
                spec.declaration.cfg,
                crate::rule_declarations::Cfg::Starlark { .. }
            )
        }),
        mapping: repositories
            .shared_mapping(&rule.label.repository)
            .unwrap_or_default(),
        locations: Default::default(),
        errors: Default::default(),
    }
}

/// LocationExpander.buildLocationMap with collectSrcs and without data
/// (analysis/LocationExpander.java:393-485), the `ctx.expand_location` map
/// besides `targets`: every output file of the rule, the `srcs` files to
/// build, and the executable (else the files to build) of `deps`,
/// `implementation_deps` and `tools` - each attribute only when the rule
/// defines it as a label list - under AliasProvider.getDependencyLabels
/// (the label as written and the actual label).
fn location_map(
    rule: &Rule,
    invocation: &crate::custom_rule::CustomInvocation,
    dependencies: &BTreeMap<String, DependencyTarget>,
    output_root: &str,
) -> anyhow::Result<crate::location_expander::LocationMap> {
    let mut map = crate::location_expander::LocationMap::default();
    for name in &rule.outs {
        let output = rule.label.with_name(name).map_err(anyhow::Error::msg)?;
        map.add(
            &output.to_string(),
            [File::new(crate::starlark_types::FileData {
                is_directory: false,
                is_symlink: false,
                path: output
                    .generated_exec_path(output_root)
                    .to_string_lossy()
                    .into_owned(),
                short_path: output.generated_short_path(),
                owner: rule.label.to_string(),
                is_source: false,
            })],
        );
    }
    for name in ["srcs", "deps", "implementation_deps", "tools"] {
        attribute_locations(invocation, dependencies, name, &mut |label, files| {
            map.add(label, files)
        });
    }
    Ok(map)
}

/// The location map entries of one label-list attribute
/// (LocationExpander.buildLocationMap, analysis/LocationExpander.java:428-485):
/// per dependency, its executable (FilesToRunProvider.getExecutable) or
/// else its files to build (`srcs` always the files to build), under both
/// the written and the actual label (AliasProvider.getDependencyLabels).
fn attribute_locations(
    invocation: &crate::custom_rule::CustomInvocation,
    dependencies: &BTreeMap<String, DependencyTarget>,
    name: &str,
    add: &mut dyn FnMut(&str, Vec<File>),
) {
    let (Some(spec), Some(AttrValue::Labels(labels))) =
        (invocation.schema.get(name), invocation.attrs.get(name))
    else {
        return;
    };
    if spec.kind != crate::custom_rule::AttrKind::LabelList {
        return;
    }
    if let Some(split) = invocation.split_dependencies.get(name) {
        for p in &split.prerequisites {
            let Some(target) = dependencies.get(&p.map_key) else {
                continue;
            };
            let files = match target.runfiles.files_to_run(&target.files).executable() {
                Some(file) if name != "srcs" => vec![file.clone()],
                _ => target.files.clone(),
            };
            add(&p.label.to_string(), files.clone());
            add(&target.label, files);
        }
        return;
    }
    for label in labels {
        let Some(target) = dependencies.get(&attribute_dependency_key(label, name, spec)) else {
            continue;
        };
        let files = match target.runfiles.files_to_run(&target.files).executable() {
            Some(executable) if name != "srcs" => vec![executable.clone()],
            _ => target.files.clone(),
        };
        let written = crate::label::Label::parse_canonical(label)
            .map(|label| label.to_string())
            .unwrap_or_else(|_| label.clone());
        add(&written, files.clone());
        add(&target.label, files);
    }
}

/// RunfilesSupport.computeArgs (analysis/RunfilesSupport.java:602-610):
/// the `args` attribute through `Expander.withDataLocations().tokenized`
/// (crate::ctx_surface::expand_args), whose location map adds `data`
/// (allowData) to the rule's. Its attribute errors fail the target at the
/// rule's location.
fn computed_args(
    rule: &Rule,
    invocation: &crate::custom_rule::CustomInvocation,
    dependencies: &BTreeMap<String, DependencyTarget>,
    surface: &crate::ctx_surface::RuleSurface,
) -> anyhow::Result<Vec<String>> {
    let values = invocation.declaration.builtin.target.strings("args");
    if values.is_empty() {
        return Ok(Vec::new());
    }
    let mut data = Vec::new();
    attribute_locations(invocation, dependencies, "data", &mut |label, files| {
        data.push((label.to_owned(), files))
    });
    let tokens = crate::ctx_surface::expand_args(surface, "args", &values, &data)?;
    let reported = surface.take_errors();
    if !reported.is_empty() {
        return Err(anyhow::Error::new(crate::rule_errors::ReportedErrors(
            crate::rule_errors::located_events(rule.location.as_deref(), reported),
        )));
    }
    Ok(tokens)
}

/// The implementation's return value shape
/// (analysis/starlark/StarlarkRuleConfiguredTargetUtil.java:119-127, 245-275).
fn check_return_shape(returned: Value<'_>) -> anyhow::Result<()> {
    if returned.is_none()
        || crate::starlark_types::is_default_info(returned)
        || crate::providers::instance_identity(returned).is_some()
        || starlark::values::list::ListRef::from_value(returned).is_some()
        || starlark::values::tuple::TupleRef::from_value(returned).is_some()
    {
        return Ok(());
    }
    if starlark::values::structs::StructRef::from_value(returned).is_some() {
        bail!("Returning a struct from a rule implementation function is deprecated.");
    }
    // Other Iterables (dict, set) are not Sequences.
    if matches!(returned.get_type(), "dict" | "set") {
        bail!(
            "Expected a list of providers, but got {}",
            returned.get_type()
        );
    }
    bail!(
        "Rule should return a struct or a list, but got {}",
        returned.get_type()
    )
}

/// The runfiles `ctx.runfiles(collect_data=True)` and
/// `ctx.runfiles(collect_default=True)` collect (Runfiles.Builder.addRunfiles,
/// analysis/Runfiles.java:669-781): `data`, then `srcs` and `deps`, when the
/// rule defines them as label lists.
fn collected_runfiles(
    invocation: &crate::custom_rule::CustomInvocation,
    dependencies: &BTreeMap<String, DependencyTarget>,
) -> anyhow::Result<(crate::runfiles::Runfiles, crate::runfiles::Runfiles)> {
    use crate::runfiles::{Mapping, Prerequisite, collect};
    let prerequisites = |name: &str| -> anyhow::Result<Vec<Prerequisite<'_>>> {
        if invocation
            .schema
            .get(name)
            .is_some_and(|s| s.kind == AttrKind::LabelList)
            && let Some(split) = invocation.split_dependencies.get(name)
        {
            return split
                .prerequisites
                .iter()
                .map(|p| {
                    let target = dependencies.get(&p.map_key).ok_or_else(|| {
                        anyhow!("split dependency {} was not resolved", p.map_key)
                    })?;
                    Ok(Prerequisite {
                        files: &target.files,
                        runfiles: &target.runfiles,
                    })
                })
                .collect();
        }
        match (invocation.schema.get(name), invocation.attrs.get(name)) {
            (Some(spec), Some(AttrValue::Labels(labels)))
                if spec.kind == crate::custom_rule::AttrKind::LabelList =>
            {
                // The edge's configuration (crate::engine; exec or Starlark
                // transition keys, attribute_dependency_key).
                labels
                    .iter()
                    .map(|label| {
                        let target = dependencies
                            .get(&attribute_dependency_key(label, name, spec))
                            .ok_or_else(|| {
                                anyhow!("attribute {name}: dependency {label} was not resolved")
                            })?;
                        Ok(Prerequisite {
                            files: &target.files,
                            runfiles: &target.runfiles,
                        })
                    })
                    .collect()
            }
            _ => Ok(Vec::new()),
        }
    };
    let data = prerequisites("data")?;
    let mut non_data = prerequisites("srcs")?;
    non_data.extend(prerequisites("deps")?);
    Ok((
        collect(&data, &non_data, Mapping::Data)?,
        collect(&data, &non_data, Mapping::Default)?,
    ))
}

/// The `ctx.attr`, `ctx.files`, `ctx.file`, `ctx.outputs` and
/// `ctx.executable` structs, and the dict of lifted subrule attribute values.
fn ctx_attribute_structs<'v>(
    rule: &Rule,
    invocation: &crate::custom_rule::CustomInvocation,
    context: &Context,
    dependencies: &BTreeMap<String, DependencyTarget>,
    heap: Heap<'v>,
) -> anyhow::Result<[Value<'v>; 6]> {
    // RuleContext attribute errors: `in A attribute of KIND rule L: `
    // (analysis/EventHandlingErrorReporter.java:92-101), private names masked.
    let attribute_prefix = |name: &str| {
        let shown = match invocation.schema.get(name) {
            Some(spec) => crate::custom_rule::attribute_message_name(name, spec),
            None => name.to_owned(),
        };
        crate::rule_errors::RuleEvents {
            rule_class: &invocation.declaration.rule_class,
            label: &crate::rule_declarations::canonical_form(&rule.label),
        }
        .attribute_message(&shown, "")
    };
    let shown_label = crate::rule_declarations::canonical_form(&rule.label);
    let consumer = Consumer {
        rule_class: invocation.declaration.rule_class.as_str(),
        label: &shown_label,
    };
    let mut attrs = BTreeMap::new();
    let mut files = BTreeMap::new();
    let mut single_files = BTreeMap::new();
    let mut outputs = BTreeMap::new();
    let mut executables = BTreeMap::new();
    let mut lifted = Vec::new();
    attrs.insert("name".to_owned(), heap.alloc(rule.label.name.as_str()));
    for (name, value) in &invocation.attrs {
        let spec = invocation
            .schema
            .get(name)
            .ok_or_else(|| anyhow!("attribute {name} has no schema"))?;
        if let Some(metadata) = invocation.split_dependencies.get(name) {
            let values = split::allocate(
                name,
                value,
                spec,
                metadata,
                dependencies,
                context,
                &attribute_prefix(name),
                consumer,
                heap,
            )?;
            attrs.insert(name.clone(), values.attr);
            files.insert(name.clone(), values.files);
            if let Some(value) = values.file {
                single_files.insert(name.clone(), value);
            }
            if let Some(value) = values.executable {
                executables.insert(name.clone(), value);
            }
            continue;
        }
        let attr = match value {
            AttrValue::String(value) => heap.alloc(value.as_str()),
            AttrValue::Strings(value) => heap.alloc(value.clone()),
            AttrValue::StringDict(entries) => {
                crate::attribute_values::alloc_string_dict(heap, entries)?
            }
            AttrValue::StringListDict(entries) => {
                crate::attribute_values::alloc_string_list_dict(heap, entries)?
            }
            AttrValue::Int(value) => heap.alloc(*value),
            AttrValue::Ints(values) => heap.alloc(values.clone()),
            AttrValue::LabelKeyedStringDict(entries) => {
                // StarlarkAttributesCollection.Builder.convertAttributeValue
                // (analysis/starlark/StarlarkAttributesCollection.java:361-369):
                // a frozen dict of the prerequisite Targets, in prerequisite
                // (key visitation) order, to their string values; the
                // attribute's files are ctx.files (:398-409).
                let labels = entries.iter().map(|(label, _)| label.as_str());
                let targets = dict_prerequisites(
                    labels,
                    name,
                    spec,
                    dependencies,
                    &attribute_prefix(name),
                    consumer,
                    heap,
                )?;
                files.insert(name.clone(), targets.files);
                let mut items = Vec::new();
                for (label, value) in entries {
                    items.push((targets.values[label.as_str()], heap.alloc(value.as_str())));
                }
                heap.alloc(starlark::values::dict::AllocDict(items))
            }
            AttrValue::StringKeyedLabelDict(entries) => {
                // StarlarkAttributesCollection.convertStringToLabelMap
                // (analysis/starlark/StarlarkAttributesCollection.java:214-222).
                let labels = entries.iter().map(|(_, label)| label.as_str());
                let targets = dict_prerequisites(
                    labels,
                    name,
                    spec,
                    dependencies,
                    &attribute_prefix(name),
                    consumer,
                    heap,
                )?;
                files.insert(name.clone(), targets.files);
                let mut items = Vec::new();
                for (key, label) in entries {
                    items.push((key.as_str(), targets.values[label.as_str()]));
                }
                heap.alloc(starlark::values::dict::AllocDict(items))
            }
            AttrValue::LabelListDict(entries) => {
                // StarlarkAttributesCollection.convertStringToLabelListMap (:224-235).
                let labels = entries
                    .iter()
                    .flat_map(|(_, labels)| labels.iter().map(String::as_str));
                let targets = dict_prerequisites(
                    labels,
                    name,
                    spec,
                    dependencies,
                    &attribute_prefix(name),
                    consumer,
                    heap,
                )?;
                files.insert(name.clone(), targets.files);
                let mut items = Vec::new();
                for (key, labels) in entries {
                    let values = labels
                        .iter()
                        .map(|label| targets.values[label.as_str()])
                        .collect::<Vec<_>>();
                    items.push((key.as_str(), heap.alloc(values)));
                }
                heap.alloc(starlark::values::dict::AllocDict(items))
            }
            AttrValue::Bool(value) => heap.alloc(*value),
            AttrValue::Label(value) if crate::subrule::is_lifted_attribute(name) => {
                // StarlarkSubrule.call (:166-178): FilesToRunProvider for an
                // executable, the single File for allow_single_file, else the
                // Target; None without a value.
                let target = value
                    .as_ref()
                    .map(|label| {
                        dependency(
                            label,
                            name,
                            spec,
                            dependencies,
                            &attribute_prefix(name),
                            consumer,
                        )
                    })
                    .transpose()?;
                let value = match target {
                    None => Value::new_none(),
                    Some(_) if spec.declaration.executable => bail!(
                        "{}executable subrule attributes (FilesToRunProvider) are {}",
                        attribute_prefix(name),
                        crate::rule_declarations::NOT_ANALYZED
                    ),
                    Some(target) if spec.allow_single_file => target
                        .files
                        .first()
                        .map(|file| alloc_file(heap, file.clone()))
                        .unwrap_or_else(Value::new_none),
                    Some(target) => value
                        .as_deref()
                        .map(|_| alloc_dependency(heap, target))
                        .unwrap_or_else(Value::new_none),
                };
                lifted.push((name.as_str(), value));
                continue;
            }
            AttrValue::Labels(labels) if crate::subrule::is_lifted_attribute(name) => {
                let targets = labels
                    .iter()
                    .map(|label| {
                        dependency(
                            label,
                            name,
                            spec,
                            dependencies,
                            &attribute_prefix(name),
                            consumer,
                        )
                        .map(|target| alloc_dependency(heap, target))
                    })
                    .collect::<anyhow::Result<Vec<_>>>()?;
                lifted.push((name.as_str(), heap.alloc(targets)));
                continue;
            }
            AttrValue::Label(value) => {
                let target = value
                    .as_ref()
                    .map(|label| {
                        dependency(
                            label,
                            name,
                            spec,
                            dependencies,
                            &attribute_prefix(name),
                            consumer,
                        )
                    })
                    .transpose()?;
                let target_files = target
                    .map(|target| target.files.clone())
                    .unwrap_or_default();
                files.insert(
                    name.clone(),
                    heap.alloc(
                        target_files
                            .iter()
                            .cloned()
                            .map(|file| alloc_file(heap, file))
                            .collect::<Vec<_>>(),
                    ),
                );
                if spec.allow_single_file {
                    single_files.insert(
                        name.clone(),
                        target_files
                            .first()
                            .map(|file| alloc_file(heap, file.clone()))
                            .unwrap_or_else(Value::new_none),
                    );
                }
                if spec.declaration.executable {
                    // StarlarkAttributesCollection.addAttribute (:410-433) with
                    // PrerequisitesCollection.getExecutablePrerequisite (:281-303):
                    // the first FilesToRunProvider of an executable is kept.
                    let executable = match target {
                        Some(target) => {
                            let files_to_run = target.runfiles.files_to_run(&target.files);
                            let Some(file) = files_to_run.executable().cloned() else {
                                // An implicit attribute is masked
                                // (EventHandlingErrorReporter.java:84-86).
                                bail!(
                                    "{}{} does not refer to a valid executable target",
                                    attribute_prefix(name),
                                    crate::label::Label::parse_canonical(&target.label)
                                        .map(|label| crate::rule_declarations::canonical_form(
                                            &label
                                        ))
                                        .unwrap_or_else(|_| target.label.clone())
                                );
                            };
                            context
                                .runfiles
                                .executables
                                .borrow_mut()
                                .entry(file.clone())
                                .or_insert(files_to_run);
                            alloc_file(heap, file)
                        }
                        None => Value::new_none(),
                    };
                    executables.insert(name.clone(), executable);
                }
                let dependency = value
                    .as_deref()
                    .zip(target)
                    .map(|(_requested, target)| alloc_dependency(heap, target));
                // A label attribute with a Starlark (split) transition is a
                // list of its prerequisites
                // (StarlarkAttributesCollection.convertAttributeValue,
                // analysis/starlark/StarlarkAttributesCollection.java:351-357).
                if starlark_transition(spec) {
                    heap.alloc(dependency.into_iter().collect::<Vec<_>>())
                } else {
                    dependency.unwrap_or_else(Value::new_none)
                }
            }
            AttrValue::Labels(labels) => {
                let targets = labels
                    .iter()
                    .map(|label| {
                        dependency(
                            label,
                            name,
                            spec,
                            dependencies,
                            &attribute_prefix(name),
                            consumer,
                        )
                    })
                    .collect::<anyhow::Result<Vec<_>>>()?;
                files.insert(
                    name.clone(),
                    prerequisite_files(heap, targets.iter().map(|target| target.files.as_slice())),
                );
                heap.alloc(
                    labels
                        .iter()
                        .zip(&targets)
                        .map(|(_requested, target)| alloc_dependency(heap, target))
                        .collect::<Vec<_>>(),
                )
            }
            AttrValue::Output(_) | AttrValue::Outputs(_)
                if name == "executable" && context.runfiles.default_executable.is_some() =>
            {
                // Outputs.addOutput (StarlarkRuleContext.java:362-371).
                bail!(
                    "{}",
                    crate::rule_errors::RuleEvents {
                        rule_class: &invocation.declaration.rule_class,
                        label: &crate::rule_declarations::canonical_form(&rule.label),
                    }
                    .rule_message("Multiple outputs with the same key: executable")
                );
            }
            // Output attributes are Files in ctx.outputs and their Labels in
            // ctx.attr (genuine Bazel 9.3 capture, tests/fixtures/repr).
            AttrValue::Output(value) => {
                let output = value
                    .as_ref()
                    .map(|name| context.declare(name))
                    .transpose()?
                    .map(|file| alloc_file(heap, file))
                    .unwrap_or_else(Value::new_none);
                outputs.insert(name.clone(), output);
                value
                    .as_ref()
                    .map(|output| output_label(heap, rule, output))
                    .transpose()?
                    .unwrap_or_else(Value::new_none)
            }
            AttrValue::Outputs(values) => {
                let output = heap.alloc(
                    values
                        .iter()
                        .map(|name| context.declare(name).map(|file| alloc_file(heap, file)))
                        .collect::<anyhow::Result<Vec<_>>>()?,
                );
                outputs.insert(name.clone(), output);
                heap.alloc(
                    values
                        .iter()
                        .map(|output| output_label(heap, rule, output))
                        .collect::<anyhow::Result<Vec<_>>>()?,
                )
            }
        };
        attrs.insert(name.clone(), attr);
    }
    // rule(outputs=...) implicit outputs after the output attributes
    // (StarlarkRuleContext.java:248-255); Outputs.addOutput rejects a key
    // already used, by an unset output attribute too, or `executable` of an
    // executable rule (:363-371). docs/IMPLICIT_OUTPUTS.md.
    for (key, output) in &invocation.declaration.implicit_outputs {
        if outputs.contains_key(key)
            || (key == "executable" && context.runfiles.default_executable.is_some())
        {
            bail!(
                "{}",
                crate::rule_errors::RuleEvents {
                    rule_class: &invocation.declaration.rule_class,
                    label: &crate::rule_declarations::canonical_form(&rule.label),
                }
                .rule_message(&format!("Multiple outputs with the same key: {key}"))
            );
        }
        let file = alloc_file(heap, context.declare(output)?);
        outputs.insert(key.clone(), file);
    }
    builtin_ctx_attrs(rule, invocation, dependencies, &mut attrs, &mut files, heap)?;
    Ok([
        heap.alloc(AllocStruct(attrs)),
        heap.alloc(AllocStruct(files)),
        heap.alloc(AllocStruct(single_files)),
        heap.alloc(AllocStruct(outputs)),
        heap.alloc(AllocStruct(executables)),
        heap.alloc(starlark::values::dict::AllocDict(lifted)),
    ])
}

/// The typed `build_setting_default` of a build-setting rule invocation.
pub(crate) fn setting_default(
    invocation: &crate::custom_rule::CustomInvocation,
) -> anyhow::Result<crate::configuration::SettingValue> {
    use crate::configuration::SettingValue;
    Ok(
        match invocation
            .attrs
            .get("build_setting_default")
            .ok_or_else(|| anyhow!("build-setting invocation has no typed default"))?
        {
            AttrValue::Bool(value) => SettingValue::Bool(*value),
            AttrValue::Int(value) => SettingValue::Int(i64::from(*value)),
            AttrValue::String(value) => SettingValue::String(value.clone()),
            AttrValue::Strings(values) => SettingValue::StringList(values.clone()),
            other => bail!("unsupported build-setting default {other:?}"),
        },
    )
}

/// `ctx.attr`/`ctx.files` entries of Bazel's built-in attributes
/// (StarlarkRuleContext.java:205-266, StarlarkAttributesCollection.java:225-390):
/// every attribute of the rule class is exposed under its public name except
/// `aspect_hints` (filtered, StarlarkRuleContext.java:207-209) and
/// `exec_group_compatible_with` (shouldIgnore, :255-259); public names are
/// unique (clashes fail when the rule is exported, RuleClass.java:1905-1918).
/// Non-dependency values convert directly (`Attribute.valueToStarlark`, so an
/// unset `deprecation` is `None`); dependency label lists become Targets and
/// have `ctx.files` entries. Values are the configured (select-resolved)
/// ones; non-empty dependency lists never reach here (resolved-value gaps,
/// docs/COMMON_RULE_ATTRIBUTES.md).
fn builtin_ctx_attrs<'v>(
    rule: &Rule,
    invocation: &crate::custom_rule::CustomInvocation,
    dependencies: &BTreeMap<String, DependencyTarget>,
    attrs: &mut BTreeMap<String, Value<'v>>,
    files: &mut BTreeMap<String, Value<'v>>,
    heap: Heap<'v>,
) -> anyhow::Result<()> {
    use crate::native_rules::NativeType;
    let builtin = &invocation.declaration.builtin;
    let target = &builtin.target;
    let empty_targets = |name: &str| -> anyhow::Result<Value<'v>> {
        ensure!(
            target.labels(name).is_empty() && !target.configurable.contains_key(name),
            "{}: built-in attribute '{name}' dependencies are {}",
            rule.label,
            crate::rule_declarations::NOT_ANALYZED
        );
        Ok(heap.alloc(Vec::<Value<'v>>::new()))
    };
    let labels = |values: &[String]| -> anyhow::Result<Value<'v>> {
        Ok(heap.alloc(
            values
                .iter()
                .map(|label| crate::starlark_types::alloc_label(heap, label))
                .collect::<anyhow::Result<Vec<_>>>()?,
        ))
    };
    let class = crate::native_rules::starlark_builtins(builtin.kind);
    let mut values = Vec::new();
    for attribute in class.attrs() {
        let name = attribute.name;
        if matches!(name, "aspect_hints" | "exec_group_compatible_with") {
            continue;
        }
        let explicit = target.attrs.get(name);
        let value = match (name, attribute.ty) {
            ("testonly", _) => heap.alloc(builtin.testonly),
            ("deprecation", _) => builtin
                .deprecation
                .as_deref()
                .map(|text| heap.alloc(text))
                .unwrap_or_else(Value::new_none),
            ("tags", _) => heap.alloc(target.tags.clone()),
            // Explicit values are stored; otherwise Rule.getAttr computes them
            // from the interior call stack, "" outside macros (Rule.java:395-405).
            ("generator_name" | "generator_function" | "generator_location", _)
                if !matches!(explicit, Some(AttrValue::String(_))) =>
            {
                let computed = builtin.generator.as_ref().map(|generator| match name {
                    "generator_name" => Some(generator.name.as_str()),
                    "generator_function" => Some(generator.function.as_str()),
                    _ => generator.location.as_deref(),
                });
                match computed {
                    None => heap.alloc(""),
                    Some(Some(text)) => heap.alloc(text),
                    Some(None) => Value::new_none(),
                }
            }
            ("visibility", _) => labels(builtin.visibility.as_deref().unwrap_or_default())?,
            // The effective package metadata (explicit or the package
            // default), configured in the no-config configuration.
            ("package_metadata", NativeType::LabelList) => {
                let mut targets = Vec::new();
                let mut metadata_files = Vec::new();
                for label in &rule.package_metadata {
                    let key = crate::label::Label::parse_canonical(label)
                        .map(|label| label.to_string())
                        .unwrap_or(label.clone());
                    let dependency = dependencies
                        .get(&metadata_dependency_key(&key))
                        .ok_or_else(|| {
                            anyhow!("attribute {name}: dependency {label} was not resolved")
                        })?;
                    metadata_files.push(dependency.files.as_slice());
                    targets.push(alloc_dependency(heap, dependency));
                }
                files.insert(
                    name.to_owned(),
                    prerequisite_files(heap, metadata_files.into_iter()),
                );
                heap.alloc(targets)
            }
            // The constraint values of the target's toolchain resolution
            // and of its compatibility check (crate::engine): Targets with
            // their ConstraintValueInfo or ConfigMatchingProvider; the
            // `toolchains` prerequisites (target configuration).
            (
                "exec_compatible_with" | "target_compatible_with" | "toolchains",
                NativeType::LabelList,
            ) => {
                let mut targets = Vec::new();
                let mut constraint_files = Vec::new();
                for label in target.labels(name) {
                    let key = crate::label::Label::parse_canonical(&label)
                        .map(|label| label.to_string())
                        .unwrap_or(label.clone());
                    let dependency = dependencies.get(&key).ok_or_else(|| {
                        anyhow!("attribute {name}: dependency {label} was not resolved")
                    })?;
                    constraint_files.push(dependency.files.as_slice());
                    targets.push(alloc_dependency(heap, dependency));
                }
                files.insert(
                    name.to_owned(),
                    prerequisite_files(heap, constraint_files.into_iter()),
                );
                heap.alloc(targets)
            }
            (_, NativeType::LabelList) => {
                files.insert(name.to_owned(), heap.alloc(Vec::<Value<'v>>::new()));
                empty_targets(name)?
            }
            (_, NativeType::NodepLabelList) => labels(&target.labels(name))?,
            (_, NativeType::String) => match explicit {
                Some(AttrValue::String(text)) => heap.alloc(text.as_str()),
                _ => heap.alloc(match name {
                    "size" => "medium",
                    "timeout" => crate::custom_rule::default_test_timeout(
                        target.string("size").unwrap_or("medium"),
                    ),
                    _ => "",
                }),
            },
            (_, NativeType::StringList) => heap.alloc(target.strings(name)),
            (_, NativeType::StringDict) => match explicit {
                Some(AttrValue::StringDict(entries)) => {
                    crate::attribute_values::alloc_string_dict(heap, entries)?
                }
                _ => crate::attribute_values::alloc_string_dict(heap, &[])?,
            },
            (_, NativeType::Bool) => heap.alloc(matches!(explicit, Some(AttrValue::Bool(true)))),
            (_, NativeType::Int) => heap.alloc(match explicit {
                Some(AttrValue::Int(value)) => *value,
                // shard_count defaults to -1 (StarlarkRuleClassFunctions.java:281).
                _ => -1,
            }),
            (_, ty) => bail!(
                "{}: built-in attribute '{name}' of type {} has no ctx.attr value",
                rule.label,
                ty.name()
            ),
        };
        values.push((name.to_owned(), value));
    }
    // Private built-in dependency attributes have public names `_x`
    // (Attribute.getStarlarkName, packages/Attribute.java:2307-2312). Without
    // --experimental_action_listener `_action_listener` is empty, and
    // `$config_dependencies` is never resolved as a prerequisite
    // (analysis/DependencyResolutionHelpers.java:408-416), so both are empty
    // target lists even when the target has select() conditions.
    if !builtin.kind.dependency_resolution {
        for name in ["_action_listener", "_config_dependencies"] {
            files.insert(name.to_owned(), heap.alloc(Vec::<Value<'v>>::new()));
            values.push((name.to_owned(), heap.alloc(Vec::<Value<'v>>::new())));
        }
    }
    for (name, value) in values {
        attrs.insert(name, value);
    }
    Ok(())
}

/// RuleContext.checkAttributesNonEmpty reports every empty attribute, in
/// rule-class attribute order (analysis/RuleContext.java:1532-1550), via
/// EventHandlingErrorReporter.completeAttributeMessage
/// (analysis/EventHandlingErrorReporter.java:84-101).
pub(crate) fn empty_attribute_errors(
    rule: &Rule,
    invocation: &crate::custom_rule::CustomInvocation,
) -> Vec<String> {
    let mut empty = Vec::new();
    for name in &invocation.declaration.attr_order {
        let Some(spec) = invocation.schema.get(name) else {
            continue;
        };
        if spec.non_empty
            && match invocation.attrs.get(name) {
                Some(AttrValue::StringListDict(entries) | AttrValue::LabelListDict(entries)) => {
                    entries.is_empty()
                }
                Some(
                    AttrValue::StringDict(entries)
                    | AttrValue::LabelKeyedStringDict(entries)
                    | AttrValue::StringKeyedLabelDict(entries),
                ) => entries.is_empty(),
                Some(
                    value @ (AttrValue::Strings(_)
                    | AttrValue::Labels(_)
                    | AttrValue::Outputs(_)
                    | AttrValue::Ints(_)),
                ) => crate::custom_rule::AttrKind::is_empty_value(value),
                _ => false,
            }
        {
            empty.push(
                crate::rule_errors::RuleEvents {
                    rule_class: &invocation.declaration.rule_class,
                    label: &crate::rule_declarations::canonical_form(&rule.label),
                }
                .attribute_message(
                    &crate::custom_rule::attribute_message_name(name, spec),
                    "attribute must be non empty",
                ),
            );
        }
    }
    empty
}

pub fn analyze(
    rule: &Rule,
    dependencies: &BTreeMap<String, DependencyTarget>,
) -> Result<AnalysisResult, String> {
    analyze_in(
        rule,
        dependencies,
        Arc::new(RepositorySet::new(std::path::PathBuf::from("."))),
    )
}

pub fn analyze_in(
    rule: &Rule,
    dependencies: &BTreeMap<String, DependencyTarget>,
    repositories: Arc<RepositorySet>,
) -> Result<AnalysisResult, String> {
    let owner = rule.label.to_string();
    let target = ConfiguredOutputs {
        output_root: crate::label::GENERATED_EXEC_ROOT,
        action_owner: &owner,
        action_env: Arc::new(crate::configuration::NativeOptions::default().action_environment()),
        runfiles: crate::runfiles_tree::RunfilesOptions::default(),
        loaded_functions: None,
    };
    let (result, printed) = analyze_buffered(rule, dependencies, repositories, &target);
    for line in printed {
        crate::ui::note(&line);
    }
    result
}

/// One aspect application at analysis: everything
/// [`analyze_aspect_buffered`] needs (AspectFunction's AspectValue inputs,
/// skyframe/AspectFunction.java:292-542).
pub(crate) struct AspectApplication {
    pub aspect_split_dependencies: BTreeMap<String, crate::custom_rule::SplitAttribute>,
    /// The applied aspect.
    pub aspect: std::sync::Arc<crate::aspects::AspectDefinition>,
    /// The descriptions of the application's aspect path: its base aspects
    /// in topological order, then itself (`ctx.aspect_ids`,
    /// RuleContext.getAspectDescriptors).
    pub aspect_ids: Vec<String>,
    /// The same path, named in messages (`ASPECTS aspect on KIND`).
    pub aspect_path: Vec<String>,
    /// The `target` parameter: the base configured target merged with the
    /// directly required aspects' providers (MergedConfiguredTarget.of,
    /// AspectFunction.java:341-346).
    pub target: DependencyTarget,
    /// The base rule, select-resolved, with its configured attribute values
    /// (`ctx.rule`).
    pub rule: Rule,
    /// The base rule's attribute edges (aspect-applied where they
    /// propagate; `ctx.rule`), keyed with the rule-context dependency keys
    /// of [`crate::analysis::dependency_key`] and friends.
    pub dependencies: BTreeMap<String, DependencyTarget>,
    /// The aspect's own attribute dependencies (`ctx.attr`/`ctx.file(s)`/
    /// `ctx.executable`), each configured by its attribute's `cfg`
    /// (AspectContext.getMainAspectPrerequisitesCollection), keyed by
    /// [`attribute_dependency_key`].
    pub aspect_dependencies: BTreeMap<String, DependencyTarget>,
    /// The aspect's own ToolchainCollection (crate::engine).
    pub toolchains: AspectToolchains,
    pub repositories: Arc<RepositorySet>,
}

/// The aspect's own toolchain contexts at analysis (AspectFunction.java:
/// 468-485, ConfiguredTargetFactory.createAspect): `ctx.toolchains`,
/// `ctx.exec_groups` and the action factory's exec groups with their
/// execution table.
pub(crate) struct AspectToolchains {
    pub toolchains: Arc<crate::toolchain_info::ToolchainContextData>,
    pub exec_groups: Arc<crate::exec_groups::ExecGroupsData>,
    pub action_exec_groups: crate::exec_groups::ActionExecGroups,
}

/// The analysis of one aspect application: runs the aspect's implementation
/// `f(target, ctx)` (StarlarkAspectFactory.create, StarlarkAspectFactory.java:
/// 62-66) and returns its providers and actions. Unlike a rule, the aspect's
/// OutputGroupInfo is exactly what it returned (no HIDDEN_TOP_LEVEL merge)
/// and a returned DefaultInfo is kept as a provider (its files join the
/// merged files to build at the consumer, MergedConfiguredTarget).
pub(crate) fn analyze_aspect_buffered(
    application: &AspectApplication,
    target: &ConfiguredOutputs<'_>,
) -> (Result<AnalysisResult, String>, Vec<String>) {
    let prints = BufferedPrints::default();
    let result = analyze_aspect_inner(application, target, &prints).map_err(|error| {
        match error.downcast_ref::<crate::rule_errors::ReportedErrors>() {
            Some(reported) => reported.0.clone(),
            None => {
                let rule_class = application
                    .rule
                    .custom
                    .as_ref()
                    .map(|custom| custom.declaration.rule_class.as_str())
                    .unwrap_or("native");
                format!(
                    "in {} aspect on {} rule {}: {error:#}",
                    application.aspect.identity,
                    rule_class,
                    crate::rule_declarations::canonical_form(&application.rule.label),
                )
            }
        }
    });
    (result, prints.0.into_inner())
}

fn analyze_aspect_inner(
    application: &AspectApplication,
    target: &ConfiguredOutputs<'_>,
    prints: &BufferedPrints,
) -> anyhow::Result<AnalysisResult> {
    let aspect = &application.aspect;
    let rule = &application.rule;
    // Declared aspect features whose application the native profile does not
    // model fail explicitly (docs/ASPECTS.md). An aspect's `toolchains`,
    // `exec_compatible_with`, `exec_groups` and `fragments` are analyzed
    // (its own ToolchainCollection, crate::engine; `host_fragments` is
    // ignored by Bazel 9.3, StarlarkRuleClassFunctions.aspect).
    let declaration = &aspect.declaration;
    let has_subrules = crate::rule_declarations::Aspect::from_value(aspect.value.value())
        .is_some_and(|value| !value.subrules.is_empty());
    if declaration.attr_aspects.is_none()
        || declaration
            .toolchains_aspects
            .as_ref()
            .is_some_and(|aspects| !aspects.is_empty())
        || has_subrules
    {
        let mut gaps = Vec::new();
        if declaration.attr_aspects.is_none() {
            gaps.push(
                "a function attr_aspects/toolchains_aspects or a propagation predicate".to_owned(),
            );
        }
        if declaration
            .toolchains_aspects
            .as_ref()
            .is_some_and(|aspects| !aspects.is_empty())
        {
            gaps.push("toolchains_aspects".to_owned());
        }
        if has_subrules {
            gaps.push("subrules".to_owned());
        }
        bail!(
            "aspect {} declares {}: not analyzed by the native profile",
            aspect.identity,
            gaps.join(", ")
        );
    }
    let invocation = rule
        .custom
        .as_ref()
        .ok_or_else(|| anyhow!("{} is not a custom rule", rule.label))?;
    let mut aspect_surface = rule_surface(
        rule,
        invocation,
        &application.repositories,
        &application.dependencies,
    );
    aspect_surface.locations = location_map(
        rule,
        invocation,
        &application.dependencies,
        target.output_root,
    )?;
    // The aspect's RuleContext: its configuration fragment policy is the
    // aspect's `fragments` (ConfiguredTargetFactory.createAspect:692), its
    // rule class for messages `ASPECTS aspect on KIND`
    // (RuleContext.Builder.getRuleClassNameForLogging, :1908-1917).
    aspect_surface.declared_fragments = declaration.fragments.clone();
    aspect_surface.rule_class = format!(
        "{} aspect on {}",
        application.aspect_path.join(","),
        invocation.declaration.rule_class
    );
    let aspect_surface = Arc::new(aspect_surface);
    let active = Arc::new(AtomicBool::new(true));
    let context = Context {
        action_env: target.action_env.clone(),
        label: rule.label.clone(),
        // The base rule's class (cc_internal.rule_class reads
        // RuleContext.getRule().getRuleClass()).
        rule_class: invocation.declaration.rule_class.clone(),
        output_root: target.output_root.to_owned(),
        action_owner: target.action_owner.to_owned(),
        declared: RefCell::new(Vec::new()),
        shareable: RefCell::new(BTreeSet::new()),
        actions: RefCell::new(Vec::new()),
        active,
        top_level_functions: invocation.top_level_functions.clone(),
        loaded_functions: target.loaded_functions.cloned(),
        repositories: application.repositories.clone(),
        scope: crate::analysis_runtime::ScopeState::new(rule.label.to_string(), BTreeSet::new()),
        subrule: crate::subrule::RuleEnvironment::new(None, false, invocation.cpp),
        exec_groups: application.toolchains.action_exec_groups.clone(),
        runfiles: crate::runfiles::ContextRunfiles {
            default_executable: None,
            ..Default::default()
        },
        // The base rule's tags (the aspect's action owner is the base
        // target's rule, crate::engine's aspect commit).
        propagated_tags: crate::engine::rule_execution_info(rule),
        retained_mappers: Default::default(),
    };
    Module::with_temp_heap(|module| -> anyhow::Result<_> {
        let ctx = make_aspect_ctx(application, &context, module.heap(), aspect_surface.clone())?;
        let mut eval = Evaluator::new(&module);
        eval.set_max_callstack_size(128)
            .map_err(|error| anyhow!(error.to_string()))?;
        eval.extra = Some(&context);
        eval.set_print_handler(prints);
        let target_value = alloc_dependency(module.heap(), &application.target);
        // The implementation function of the exported aspect value, kept
        // alive by the definition's owner (StarlarkAspectFactory).
        let aspect_value = module.heap().access_owned_frozen_value(&aspect.value);
        let implementation = crate::rule_declarations::Aspect::from_value(aspect_value)
            .ok_or_else(|| anyhow!("aspect {} is not an aspect value", aspect.identity))?
            .implementation;
        let returned = eval.eval_function(implementation, &[target_value, ctx], &[]);
        context.scope.finish();
        context.active.store(false, Ordering::Release);
        let reported = aspect_surface.take_errors();
        if !reported.is_empty() {
            let mut message =
                crate::rule_errors::located_events(rule.location.as_deref(), reported);
            if let Err(error) = &returned {
                message.push('\n');
                message.push_str(&error.to_string());
            }
            return Err(anyhow::Error::new(crate::rule_errors::ReportedErrors(
                message,
            )));
        }
        let returned = returned.map_err(|error| anyhow!(error.to_string()))?;
        check_return_shape(returned)?;
        let custom = crate::providers::returned_custom_providers(returned)?;
        let provider_keys = crate::providers::returned_provider_key_names(returned)?;
        crate::providers::check_freezable_payloads(custom.values().copied())?;
        let mut slots = Vec::new();
        for (identity, provider) in custom {
            let slot = format!("$provider:{}", slots.len());
            module.set(&slot, provider);
            slots.push((identity, slot));
        }
        drop(eval);
        let frozen = module
            .freeze_named(FrozenHeapName::User(Box::new(format!(
                "aspect:{}%{}",
                rule.label, aspect.exported_name
            ))))
            .map_err(crate::providers::unsupported_payload_freeze)?;
        // Deferred map_each callables retained by this module
        // (crate::analysis_runtime::retain_mapper).
        context.fill_retained_mappers(&frozen)?;
        let providers = slots
            .into_iter()
            .map(|(identity, slot)| {
                frozen
                    .get(&slot)
                    .map(|value| (identity, value))
                    .map_err(|error| anyhow!(error.to_string()))
            })
            .collect::<anyhow::Result<BTreeMap<_, _>>>()?;
        Ok(AnalysisResult {
            files: Vec::new(),
            declared: context.declared.borrow().clone(),
            actions: context.actions.borrow().clone(),
            providers,
            provider_keys,
            runfiles: crate::runfiles::TargetRunfiles::default(),
        })
    })
}

/// The aspect context: `ctx.attr`/`ctx.files`/`ctx.file` of the aspect's own
/// attributes (their defaults, or the `--aspects_parameters` values),
/// `ctx.label` of the base target, `ctx.actions`, and `ctx.rule` over the
/// base rule's attributes with the application's (aspect-applied)
/// dependencies (StarlarkRuleContext.java:188-330).
fn make_aspect_ctx<'v>(
    application: &AspectApplication,
    context: &Context,
    heap: Heap<'v>,
    surface: Arc<crate::ctx_surface::RuleSurface>,
) -> anyhow::Result<Value<'v>> {
    let aspect = &application.aspect;
    let rule = &application.rule;
    let invocation = rule
        .custom
        .as_ref()
        .ok_or_else(|| anyhow!("{} is not a custom rule", rule.label))?;
    let shown_label = crate::rule_declarations::canonical_form(&rule.label);
    let consumer = Consumer {
        rule_class: &aspect.exported_name,
        label: &shown_label,
    };
    // The aspect's own attributes (StarlarkRuleContext.java:270-285 over the
    // main aspect's attributes, StarlarkAttributesCollection.Builder
    // .addAttribute, :382-450): implicit label attributes resolved from the
    // application's own attribute dependencies, parameters from their
    // values (AspectParameters). `ctx.attr` holds only the aspect's
    // attributes (no `name`); every dependency attribute has a `ctx.files`
    // entry (PrerequisiteArtifacts.nestedSet: each File once), an
    // `allow_single_file` one a `ctx.file` entry and an executable label one
    // a `ctx.executable` entry, whose FilesToRunProvider a spawn running the
    // File picks up (the first one per executable, :410-433). Frozen like a
    // rule's (immutable in Bazel).
    const NAMES: [&str; 5] = ["attr", "files", "file", "executable", "$aspect_ids"];
    let owned = Module::with_temp_heap(|module| -> anyhow::Result<_> {
        let heap = module.heap();
        let mut attrs: BTreeMap<String, Value> = BTreeMap::new();
        let mut files: BTreeMap<String, Value> = BTreeMap::new();
        let mut single_files: BTreeMap<String, Value> = BTreeMap::new();
        let mut executables: BTreeMap<String, Value> = BTreeMap::new();
        let mut parameter_errors = Vec::new();
        for (name, spec) in &aspect.declaration.attrs {
            let value = aspect.parameters.get(name).unwrap_or(&spec.default);
            let prefix = crate::rule_errors::RuleEvents {
                rule_class: &aspect.exported_name,
                label: &shown_label,
            }
            .attribute_message(&crate::custom_rule::attribute_message_name(name, spec), "");
            if let Some(metadata) = application.aspect_split_dependencies.get(name) {
                let values = split::allocate(
                    name,
                    value,
                    spec,
                    metadata,
                    &application.aspect_dependencies,
                    context,
                    &prefix,
                    consumer,
                    heap,
                )?;
                attrs.insert(name.clone(), values.attr);
                files.insert(name.clone(), values.files);
                if let Some(file) = values.file {
                    single_files.insert(name.clone(), file);
                }
                if let Some(executable) = values.executable {
                    executables.insert(name.clone(), executable);
                }
                continue;
            }
            let resolve = |label: &str| {
                dependency(
                    label,
                    name,
                    spec,
                    &application.aspect_dependencies,
                    &prefix,
                    consumer,
                )
            };
            let attr = match value {
                AttrValue::Label(label) => {
                    let target = match label.as_deref().map(resolve).transpose() {
                        Ok(target) => target,
                        Err(error) => {
                            parameter_errors.push(format!("{prefix}{error}"));
                            continue;
                        }
                    };
                    let target_files = target.map(|target| target.files.as_slice());
                    files.insert(
                        name.clone(),
                        prerequisite_files(heap, target_files.into_iter()),
                    );
                    if spec.allow_single_file {
                        single_files.insert(
                            name.clone(),
                            target_files
                                .and_then(|files| files.first())
                                .map(|file| alloc_file(heap, file.clone()))
                                .unwrap_or_else(Value::new_none),
                        );
                    }
                    if spec.declaration.executable {
                        let executable = match target {
                            Some(target) => {
                                let files_to_run = target.runfiles.files_to_run(&target.files);
                                let Some(file) = files_to_run.executable().cloned() else {
                                    parameter_errors.push(format!(
                                        "{prefix}{} does not refer to a valid executable target",
                                        canonical_text(&target.label)
                                    ));
                                    continue;
                                };
                                context
                                    .runfiles
                                    .executables
                                    .borrow_mut()
                                    .entry(file.clone())
                                    .or_insert(files_to_run);
                                alloc_file(heap, file)
                            }
                            None => Value::new_none(),
                        };
                        executables.insert(name.clone(), executable);
                    }
                    let dependency = target.map(|target| alloc_dependency(heap, target));
                    // A label attribute with a Starlark transition is a
                    // list of its prerequisites (StarlarkAttributesCollection.
                    // convertAttributeValue: the transition factory isSplit).
                    if starlark_transition(spec) {
                        heap.alloc(dependency.into_iter().collect::<Vec<_>>())
                    } else {
                        dependency.unwrap_or_else(Value::new_none)
                    }
                }
                AttrValue::Labels(labels) => {
                    let targets = match labels
                        .iter()
                        .map(|label| resolve(label))
                        .collect::<anyhow::Result<Vec<_>>>()
                    {
                        Ok(targets) => targets,
                        Err(error) => {
                            parameter_errors.push(format!("{prefix}{error}"));
                            continue;
                        }
                    };
                    files.insert(
                        name.clone(),
                        prerequisite_files(
                            heap,
                            targets.iter().map(|target| target.files.as_slice()),
                        ),
                    );
                    heap.alloc(
                        targets
                            .iter()
                            .map(|target| alloc_dependency(heap, target))
                            .collect::<Vec<_>>(),
                    )
                }
                AttrValue::String(value) => heap.alloc(value.as_str()),
                AttrValue::Strings(values) => heap.alloc(values.clone()),
                AttrValue::Int(value) => heap.alloc(*value),
                AttrValue::Ints(values) => heap.alloc(values.clone()),
                AttrValue::Bool(value) => heap.alloc(*value),
                AttrValue::StringDict(entries) => {
                    crate::attribute_values::alloc_string_dict(heap, entries)?
                }
                AttrValue::StringListDict(entries) => {
                    crate::attribute_values::alloc_string_list_dict(heap, entries)?
                }
                _ => bail!(
                    "aspect attribute '{}' has a type the native profile does not analyze (label-keyed dictionaries, outputs); {}",
                    name,
                    crate::rule_declarations::NOT_ANALYZED
                ),
            };
            attrs.insert(name.clone(), attr);
        }
        if !parameter_errors.is_empty() {
            return Err(anyhow::Error::new(crate::rule_errors::ReportedErrors(
                parameter_errors.join("\n"),
            )));
        }
        module.set(
            "attr",
            module.heap().alloc(starlark::values::structs::AllocStruct(
                attrs.into_iter().collect::<Vec<_>>(),
            )),
        );
        module.set(
            "files",
            module.heap().alloc(starlark::values::structs::AllocStruct(
                files.into_iter().collect::<Vec<_>>(),
            )),
        );
        module.set(
            "file",
            module.heap().alloc(starlark::values::structs::AllocStruct(
                single_files.into_iter().collect::<Vec<_>>(),
            )),
        );
        module.set(
            "executable",
            module.heap().alloc(starlark::values::structs::AllocStruct(
                executables.into_iter().collect::<Vec<_>>(),
            )),
        );
        module.set(
            "$aspect_ids",
            module.heap().alloc(application.aspect_ids.clone()),
        );
        let frozen = module
            .freeze_named(FrozenHeapName::User(Box::new(
                "aspect context attributes".to_owned(),
            )))
            .map_err(|error| anyhow!("cannot freeze aspect context attributes: {error:?}"))?;
        NAMES
            .map(|name| frozen.get(name).map_err(|error| anyhow!(error.to_string())))
            .into_iter()
            .collect::<anyhow::Result<Vec<_>>>()
    })?;
    let owned = <[OwnedFrozenValue; 5]>::try_from(owned)
        .map_err(|_| anyhow!("internal error: aspect context attribute structs"))?;
    let [attr, files, file, executable, aspect_ids] =
        owned.map(|value| heap.access_owned_frozen_value(&value));
    // ctx.rule: the base rule's attribute structs over the application's
    // dependencies (allocated beside the ctx; StarlarkAttributesCollection
    // is not frozen separately in Bazel either).
    let [
        base_attr,
        base_files,
        base_file,
        _outputs,
        base_executable,
        _lifted,
    ] = ctx_attribute_structs(rule, invocation, context, &application.dependencies, heap)?;
    let rule_attributes = heap.alloc_complex_no_freeze(crate::analysis_ctx::RuleAttributes {
        attr: base_attr,
        executable: base_executable,
        file: base_file,
        files: base_files,
        kind: invocation.declaration.rule_class.clone(),
    });
    let label = alloc_target(heap, &rule.label.to_string(), Vec::new())
        .get_attr_error("label", heap)
        .map_err(|error| anyhow!(error.to_string()))?;
    let actions = crate::analysis_runtime::alloc_actions_for(
        heap,
        context.scope.root(),
        crate::analysis_ctx::aspect_context_repr(&shown_label),
    );
    let backing = heap.alloc(starlark::values::structs::AllocStruct([
        ("attr", attr),
        ("files", files),
        ("file", file),
        ("executable", executable),
        ("label", label),
        ("actions", actions),
        ("$rule", rule_attributes),
        ("$aspect_ids", aspect_ids),
        ("$collect_data", heap.alloc(Vec::<Value>::new())),
        ("$collect_default", heap.alloc(Vec::<Value>::new())),
    ]));
    let ctx = crate::analysis_ctx::alloc_aspect(
        heap,
        backing,
        context.scope.root(),
        invocation.target_platform.clone(),
        Some(application.toolchains.toolchains.clone()),
        Some(application.toolchains.exec_groups.clone()),
        surface,
    );
    crate::analysis_runtime::set_rule_ctx(&actions, ctx);
    Ok(ctx)
}

/// Where the analysis of a configured target declares its outputs and how
/// its actions are named (crate::config_registry): the output root of the
/// target's configuration and the owner part of its action ids.
pub(crate) struct ConfiguredOutputs<'a> {
    pub output_root: &'a str,
    pub action_owner: &'a str,
    /// The configuration's shell action environment (crate::action_env).
    pub action_env: Arc<crate::action_env::ActionEnvironment>,
    /// Which runfiles manifests RunfilesSupport declares (N23f).
    pub runfiles: crate::runfiles_tree::RunfilesOptions,
    /// The build's registry of loaded top-level functions
    /// (crate::analysis_runtime::LoadedFunctions).
    pub loaded_functions: Option<&'a Arc<crate::analysis_runtime::LoadedFunctions>>,
}

/// `print()` output of an implementation, kept until the caller decides
/// whether the attempt counts (an analysis retried for a deferred target
/// platform must print once, crate::engine).
#[derive(Default)]
struct BufferedPrints(RefCell<Vec<String>>);

impl starlark::PrintHandler for BufferedPrints {
    fn println(&self, text: &str) -> starlark::Result<()> {
        self.0.borrow_mut().push(crate::debug_print::line(text));
        Ok(())
    }
}

/// [`analyze_in`] without writing the implementation's `print()` output:
/// the result and the printed lines, in order.
pub(crate) fn analyze_buffered(
    rule: &Rule,
    dependencies: &BTreeMap<String, DependencyTarget>,
    repositories: Arc<RepositorySet>,
    target: &ConfiguredOutputs<'_>,
) -> (Result<AnalysisResult, String>, Vec<String>) {
    let prints = BufferedPrints::default();
    let result = analyze_inner(rule, dependencies, repositories, target, &prints).map_err(
        |error| match error.downcast_ref::<crate::rule_errors::ReportedErrors>() {
            Some(reported) => reported.0.clone(),
            None => format!("analysis of {}: {error:#}", rule.label),
        },
    );
    (result, prints.0.into_inner())
}

fn analyze_inner(
    rule: &Rule,
    dependencies: &BTreeMap<String, DependencyTarget>,
    repositories: Arc<RepositorySet>,
    target: &ConfiguredOutputs<'_>,
    prints: &BufferedPrints,
) -> anyhow::Result<AnalysisResult> {
    let invocation = rule
        .custom
        .as_ref()
        .ok_or_else(|| anyhow!("not a custom rule"))?;
    // Only configured targets are analyzed: every select() was resolved by
    // crate::configured_attributes::configure (docs/CONFIGURATION.md).
    ensure!(
        invocation.declaration.configurable.is_empty()
            && invocation
                .declaration
                .builtin
                .target
                .configurable
                .is_empty(),
        "internal error: unresolved select() reached analysis"
    );
    // Declared load-time features and resolved attribute values without native
    // analysis semantics fail explicitly instead of being silently ignored
    // (docs/LOAD_DECLARATIONS.md); crate::engine reports them before deps.
    let declaration = &invocation.declaration;
    let mut gaps = declaration.transition_gaps.clone();
    gaps.extend(declaration.toolchain_gaps.iter().cloned());
    gaps.extend(declaration.unanalyzed.iter().cloned());
    gaps.extend(crate::custom_rule::value_gaps(invocation));
    if !gaps.is_empty() {
        bail!(
            "declared rule features {}: {}",
            crate::rule_declarations::NOT_ANALYZED,
            gaps.join("; ")
        );
    }
    let empty = empty_attribute_errors(rule, invocation);
    if !empty.is_empty() {
        bail!("{}", empty.join("\n"));
    }
    let kind = invocation.declaration.builtin.kind;
    // RuleClass.isExecutableStarlark (StarlarkRuleClassFunctions.java:911-913).
    let executable_rule = kind.executable || kind.test;
    let active = Arc::new(AtomicBool::new(true));
    let default_executable = executable_rule
        .then(|| -> anyhow::Result<_> {
            // RuleContext.createOutputArtifact: the target name in the package.
            let output = rule
                .label
                .with_name(&rule.label.name)
                .map_err(anyhow::Error::msg)?;
            // Below the target's own configuration output root (an exec
            // configuration's executable lives in its exec root).
            Ok(crate::runfiles::DefaultExecutable::new(File::new(
                crate::starlark_types::FileData {
                    is_directory: false,
                    is_symlink: false,
                    path: output
                        .generated_exec_path(target.output_root)
                        .to_string_lossy()
                        .into_owned(),
                    short_path: output.generated_short_path(),
                    owner: rule.label.to_string(),
                    is_source: false,
                },
            )))
        })
        .transpose()?;
    let mut surface = rule_surface(rule, invocation, &repositories, dependencies);
    surface.locations = location_map(rule, invocation, dependencies, target.output_root)?;
    // ctx.configuration.default_shell_env reads the environment the target's
    // actions get (crate::action_env), not a constant.
    Arc::make_mut(&mut surface.config).action_env = target.action_env.clone();
    let surface = Arc::new(surface);
    // RuleConfiguredTargetBuilder.propagateTransitiveValidationOutputGroups
    // (:366-382): only rules defined under BuiltinRestriction's
    // INTERNAL_STARLARK_API_ALLOWLIST may return `_validation_transitive`.
    let transitive_validation_owner = {
        let environment = &invocation.declaration.definition_environment_label;
        let allowed = crate::label::Label::parse_canonical(environment).is_ok_and(|label| {
            let main_mapping = repositories.mapping("").ok();
            crate::subrule_fragments::INTERNAL_STARLARK_API_ALLOWLIST
                .iter()
                .any(|(apparent, prefix)| {
                    crate::cc_common::allowlist_allows(
                        &label,
                        |name| main_mapping.as_ref().and_then(|m| m.get(name).cloned()),
                        apparent,
                        prefix,
                    )
                })
        });
        (environment.clone(), allowed)
    };
    let context = Context {
        action_env: target.action_env.clone(),
        label: rule.label.clone(),
        rule_class: invocation.declaration.rule_class.clone(),
        output_root: target.output_root.to_owned(),
        action_owner: target.action_owner.to_owned(),
        declared: RefCell::new(Vec::new()),
        shareable: RefCell::new(BTreeSet::new()),
        actions: RefCell::new(Vec::new()),
        active,
        top_level_functions: invocation.top_level_functions.clone(),
        loaded_functions: target.loaded_functions.cloned(),
        repositories: repositories.clone(),
        scope: crate::analysis_runtime::ScopeState::new(
            rule.label.to_string(),
            crate::subrule::direct_ids(invocation.subrules.iter().map(|value| value.value()))?,
        ),
        subrule: crate::subrule::RuleEnvironment::new(
            invocation.toolchains.clone(),
            invocation.use_auto_exec_groups,
            invocation.cpp,
        ),
        exec_groups: invocation.action_exec_groups.clone(),
        runfiles: crate::runfiles::ContextRunfiles {
            default_executable,
            ..Default::default()
        },
        propagated_tags: crate::engine::rule_execution_info(rule),
        retained_mappers: Default::default(),
    };
    for output in &rule.outs {
        context.declare(output)?;
    }
    let (files, providers, provider_keys, target_runfiles) = Module::with_temp_heap(
        |module| -> anyhow::Result<_> {
            let ctx = make_ctx(rule, &context, dependencies, module.heap(), surface.clone())?;
            // Bazel's implicit DefaultInfo includes only predeclared outputs.
            let predeclared = context.declared.borrow().clone();
            let mut eval = Evaluator::new(&module);
            eval.set_max_callstack_size(128)
                .map_err(|error| anyhow!(error.to_string()))?;
            eval.extra = Some(&context);
            eval.set_print_handler(prints);
            let implementation = module
                .heap()
                .access_owned_frozen_value(&invocation.implementation);
            let returned = eval.eval_function(implementation, &[ctx], &[]);
            // Invalidate capabilities before inspecting or freezing providers,
            // on both successful callback return and ordinary evaluation error.
            context.scope.finish();
            context.active.store(false, Ordering::Release);
            // Attribute and rule errors reported during the implementation
            // (ctx.expand_make_variables, ctx.expand_location) fail the target
            // after it returns (StarlarkRuleConfiguredTargetUtil.buildRule
            // checks RuleContext.hasErrors), reported before an evaluation error.
            let reported = surface.take_errors();
            if !reported.is_empty() {
                // StarlarkRuleConfiguredTargetUtil.buildRule (:97-115):
                // the events at the rule's location (crate::rule_errors).
                let mut message =
                    crate::rule_errors::located_events(rule.location.as_deref(), reported);
                if let Err(error) = &returned {
                    message.push('\n');
                    message.push_str(&error.to_string());
                }
                return Err(anyhow::Error::new(crate::rule_errors::ReportedErrors(
                    message,
                )));
            }
            let returned = returned.map_err(|error| anyhow!(error.to_string()))?;
            check_return_shape(returned)?;
            let returned_default = crate::starlark_types::returned_default_info(returned)?;
            let custom = crate::providers::returned_custom_providers(returned)?;
            let provider_keys = crate::providers::returned_provider_key_names(returned)?;
            // addProviders (analysis/starlark/StarlarkRuleConfiguredTargetUtil.java:285-299):
            // RunEnvironmentInfo of a non-executable, non-test target. The
            // regular constructor sets error_on_non_executable=true;
            // testing.TestEnvironment sets it to false (ruleWarning, provider kept).
            if !executable_rule
                && custom
                    .get(crate::run_environment_info::PROVIDER_ID)
                    .is_some_and(|info| {
                        crate::run_environment_info::errors_on_non_executable_rule(*info)
                    })
            {
                bail!(
                    "Returning RunEnvironmentInfo from a non-executable, non-test target has no effect"
                );
            }
            // parseDefaultProviderFields/addSimpleProviders (:359-490).
            let (files, target_runfiles) = crate::runfiles::configure_target(
                returned_default.unwrap_or_default(),
                &crate::runfiles::RuleClassification {
                    label: &rule.label.to_string(),
                    rule_class: &invocation.declaration.rule_class,
                    executable: executable_rule,
                    test: kind.test,
                    args: &|| computed_args(rule, invocation, dependencies, &surface),
                },
                predeclared,
                context.runfiles.default_executable.as_ref(),
                target.runfiles,
            )?;
            let mut target_runfiles = target_runfiles;
            // RunfilesSupport.computeActionEnvironment (:612-619): a Starlark
            // rule's returned RunEnvironmentInfo.
            if let Some((environment, inherited)) = custom
                .get(crate::run_environment_info::PROVIDER_ID)
                .and_then(|info| crate::run_environment_info::environment(*info))
            {
                target_runfiles.run.environment = environment;
                target_runfiles.run.inherited = inherited;
            }
            // RuleConfiguredTargetBuilder.initializeTestProvider (:470-498):
            // a test rule's ExecutionInfo goes to its test action (the
            // requirements and the exec group of its owner), which the `test`
            // command assembles (crate::test_runner::TestSpec).
            if kind.test {
                target_runfiles.run.test_requirements = custom
                    .get(crate::execution_info::PROVIDER_ID)
                    .and_then(|info| crate::execution_info::test_requirements(*info));
            }
            // checkDeclaredProviders (analysis/starlark/StarlarkRuleConfiguredTargetUtil.java:138-148)
            // looks each advertised key up on the built target: DefaultInfo
            // and OutputGroupInfo are always there (AbstractConfiguredTarget.java:209,
            // RuleConfiguredTargetBuilder.build), any other key (built-in
            // ones such as testing.ExecutionInfo too) must be returned.
            for provider in &invocation.declaration.provides {
                ensure!(
                    [
                        crate::providers::DEFAULT_INFO_PROVIDER_ID,
                        crate::output_group_info::PROVIDER_ID,
                    ]
                    .contains(&provider.id.as_str())
                        || custom.contains_key(&provider.id),
                    "rule advertised the '{}' provider, but this provider was not among those returned",
                    provider.display_name
                );
            }
            // RuleConfiguredTargetBuilder.build (:155-213, 543-552): every rule
            // target has an OutputGroupInfo, the returned groups merged with
            // HIDDEN_TOP_LEVEL (the default runfiles' artifacts).
            let mut custom = custom;
            let mut groups = custom
                .remove(crate::output_group_info::PROVIDER_ID)
                .and_then(crate::output_group_info::groups)
                .map(<[_]>::to_vec)
                .unwrap_or_default();
            if let Some(index) = groups
                .iter()
                .position(|(name, _)| name == crate::output_group_info::VALIDATION_TRANSITIVE)
            {
                let (owner, allowed) = &transitive_validation_owner;
                ensure!(
                    *allowed,
                    "{} cannot access the _transitive_validation private API",
                    crate::label::Label::parse_canonical(owner)
                        .map(|label| crate::rule_declarations::canonical_form(&label))
                        .unwrap_or_else(|_| owner.clone())
                );
                let (_, transitive) = groups.remove(index);
                crate::output_group_info::add_group(
                    &mut groups,
                    crate::output_group_info::VALIDATION,
                    transitive,
                );
            } else {
                // RuleConfiguredTargetBuilder.propagateValidationActionOutputGroup
                // (:294-296): an analysis test propagates nothing; those rules
                // never reach the native analysis (rule(analysis_test=True) is
                // an unanalyzed declared feature). Every other rule collects
                // its dependencies' `_validation` groups
                // (collectTransitiveValidationOutputGroups, :397-430): through
                // label attributes that are not implicit (`$`-prefixed), not
                // tool dependencies (`cfg = "exec"`/no-config transitions) and
                // not `skip_validations`, merged into the target's own group
                // as ONE composed node (W1c: the prerequisites' sets, never
                // their files).
                crate::output_group_info::add_transitive(
                    &mut groups,
                    crate::output_group_info::VALIDATION,
                    transitive_validation(rule, invocation, dependencies, &repositories),
                );
            }
            let merged = crate::output_group_info::merged(
                groups,
                target_runfiles.default_runfiles().all_artifacts_depset(),
            );
            custom.insert(
                crate::output_group_info::PROVIDER_ID.to_owned(),
                module.heap().alloc(merged),
            );
            // No provider-file walk (W1b, PLAN-L1 §2.2): a consumer
            // resolves whatever File it reaches through the build's
            // artifact index, as Bazel resolves an input through its
            // Artifact (crate::engine). Only the values this analysis
            // allocated are checked, for what the freeze cannot carry.
            crate::providers::check_freezable_payloads(custom.values().copied())?;
            let mut slots = Vec::new();
            for (identity, provider) in custom {
                let slot = format!("$provider:{}", slots.len());
                module.set(&slot, provider);
                slots.push((identity, slot));
            }
            drop(eval);
            let frozen = module
                .freeze_named(FrozenHeapName::User(Box::new(format!(
                    "analysis:{}",
                    rule.label
                ))))
                .map_err(crate::providers::unsupported_payload_freeze)?;
            // Deferred map_each callables retained by this module
            // (crate::analysis_runtime::retain_mapper).
            context.fill_retained_mappers(&frozen)?;
            let providers = slots
                .into_iter()
                .map(|(identity, slot)| {
                    frozen
                        .get_any_visibility(&slot)
                        .map(|(value, _)| (identity, value))
                })
                .collect::<anyhow::Result<BTreeMap<_, _>>>()?;
            Ok((files, providers, provider_keys, target_runfiles))
        },
    )?;
    let declared = context.take_declared();
    let actions = std::mem::take(&mut *context.actions.borrow_mut());
    // StarlarkProviderValidationUtil.validateArtifacts
    // (analysis/StarlarkProviderValidationUtil.java:25-32): tree artifacts
    // whose path was also declared as a file, before orphans.
    let conflicting = declared
        .iter()
        .filter(|file| {
            file.is_directory
                && declared
                    .iter()
                    .any(|other| !other.is_directory && other.path == file.path)
        })
        .map(|file| crate::runfiles::root_relative(file).to_owned())
        .collect::<Vec<_>>();
    if !conflicting.is_empty() {
        bail!(
            "The following directories were also declared as files:\n{}",
            conflicting.join("\n")
        );
    }
    // StarlarkProviderValidationUtil.validateArtifacts
    // (analysis/StarlarkProviderValidationUtil.java:25-46): every orphan by its
    // root-relative path (Bazel iterates a HashMap; here declaration order).
    let orphans = declared
        .iter()
        .filter(|file| !actions.iter().any(|action| action.outputs.contains(file)))
        .map(|file| crate::runfiles::root_relative(file).to_owned())
        .collect::<Vec<_>>();
    if !orphans.is_empty() {
        bail!(
            "The following files have no generating action:\n{}",
            orphans.join("\n")
        );
    }
    let mut unique = BTreeSet::new();
    let files = files
        .into_iter()
        .filter(|file| unique.insert(file.clone()))
        .collect();
    Ok(AnalysisResult {
        files,
        // Shareable artifacts are root-relative (no package checks).
        declared: declared
            .into_iter()
            .filter(|file| !context.is_shareable(file))
            .collect(),
        actions,
        providers,
        provider_keys,
        runfiles: target_runfiles,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::label::Label;
    use starlark::environment::Globals;
    use starlark::syntax::{AstModule, Dialect};

    fn spec_of(declaration: crate::custom_rule::AttrDeclaration) -> AttrSpec {
        AttrSpec {
            kind: AttrKind::Label,
            default: AttrValue::Label(None),
            mandatory: false,
            allow_files: false,
            allow_single_file: false,
            non_empty: false,
            required_providers: Vec::new(),
            declaration,
        }
    }

    #[test]
    fn implicit_attributes_follow_the_value_source_prefixes() {
        let plain = crate::custom_rule::AttrDeclaration::default();
        let late_bound = crate::custom_rule::AttrDeclaration {
            late_bound: true,
            ..Default::default()
        };
        // AttributeValueSource.java:25-28, 64-72: DIRECT/COMPUTED_DEFAULT
        // `_x` becomes the native `$x` (implicit,
        // Attribute.java:2265-2275); LATE_BOUND/MATERIALIZER `_x` becomes
        // `:x`, which is not implicit.
        assert!(implicit_attribute("$implicit", &spec_of(plain.clone())));
        assert!(implicit_attribute("_direct", &spec_of(plain.clone())));
        assert!(!implicit_attribute("_late", &spec_of(late_bound)));
        assert!(!implicit_attribute("public", &spec_of(plain)));
    }

    #[test]
    fn string_dict_analysis_nonempty_freeze_unicode_and_action_environment() {
        let root = tempfile::Builder::new()
            .prefix("string-dict-analysis-")
            .tempdir_in("/tmp/opencode")
            .unwrap();
        std::fs::write(
            root.path().join("defs.bzl"),
            r#"
def impl(ctx):
    d = ctx.attr.d
    if ctx.attr.mutate:
        d["new"] = "changed"
    out = ctx.actions.declare_file(ctx.label.name + ".out")
    ctx.actions.run_shell(outputs=[out], command="unused", env=d)
    return [DefaultInfo(files=depset([out]))]
r = rule(implementation=impl, attrs={"d":attr.string_dict(allow_empty=False), "mutate":attr.bool()})
"#,
        )
        .unwrap();
        std::fs::write(
            root.path().join("BUILD.bazel"),
            r#"
load(":defs.bzl", "r")
r(name="omitted")
r(name="empty", d={})
r(name="blank", d={"z":""})
r(name="good", d={"z":"café☃", "a":"@missing//p:t"})
r(name="mutate", d={"z":"v"}, mutate=True)
"#,
        )
        .unwrap();
        // Loading/query succeeds for both omitted and explicit empty values.
        let package = crate::package::evaluate(root.path(), "").unwrap();
        for name in ["omitted", "empty"] {
            let rule = &package[&Label::parse(&format!("//:{name}"), "").unwrap()];
            assert!(
                analyze(rule, &BTreeMap::new())
                    .unwrap_err()
                    .contains("attribute must be non empty")
            );
        }
        let rule = &package[&Label::parse("//:good", "").unwrap()];
        assert!(rule.srcs.is_empty());
        let result = analyze(rule, &BTreeMap::new()).unwrap();
        assert_eq!(result.actions[0].action.environment["z"], "café☃");
        assert_eq!(result.actions[0].action.environment["a"], "@missing//p:t");
        analyze(
            &package[&Label::parse("//:blank", "").unwrap()],
            &BTreeMap::new(),
        )
        .unwrap();
        let error = analyze(
            &package[&Label::parse("//:mutate", "").unwrap()],
            &BTreeMap::new(),
        )
        .unwrap_err()
        .to_lowercase();
        assert!(
            error.contains("frozen") || error.contains("immutable"),
            "{error}"
        );
    }

    #[test]
    fn run_environment_direct_return_rejected_after_callback_before_action_handoff() {
        let root = tempfile::Builder::new()
            .prefix("run-environment-analysis-")
            .tempdir_in("/tmp/opencode")
            .unwrap();
        std::fs::write(
            root.path().join("defs.bzl"),
            r#"
P = provider()
def impl(ctx):
    ctx.actions.write(ctx.outputs.out, "registered, never executed")
    info = RunEnvironmentInfo({"FLAG": "seed"}, ["HOME"])
    if ctx.attr.mode == "callback_failure":
        fail("callback reached after registration")
    if ctx.attr.mode == "direct":
        return info
    if ctx.attr.mode == "list":
        return [info]
    if ctx.attr.mode == "with_default":
        return [DefaultInfo(files=depset([ctx.outputs.out])), info]
    if ctx.attr.mode == "test_environment":
        return [DefaultInfo(files=depset([ctx.outputs.out])), testing.TestEnvironment({"FLAG": "seed"}, ["HOME"])]
    if ctx.attr.mode == "nested":
        return P(info=info, nested=[{"k": struct(info=(info,))}])
    if ctx.attr.mode == "nested_default":
        return [P(info=info), DefaultInfo()]
    return [P(info=info), DefaultInfo(files=depset())]
r = rule(implementation=impl, attrs={"mode": attr.string(), "out": attr.output()})
"#,
        )
        .unwrap();
        for mode in [
            "direct",
            "list",
            "with_default",
            "callback_failure",
            "nested",
            "nested_default",
            "nested_empty",
            "test_environment",
        ] {
            std::fs::write(
                root.path().join("BUILD.bazel"),
                format!(
                    "load(':defs.bzl', 'r')\nr(name='producer', mode='{mode}', out='output')\n"
                ),
            )
            .unwrap();
            let package = crate::package::evaluate(root.path(), "").unwrap();
            let rule = &package[&Label::parse("//:producer", "").unwrap()];
            let result = analyze(rule, &BTreeMap::new());
            match mode {
                "direct" | "list" | "with_default" => {
                    let error = result.unwrap_err();
                    assert!(error.contains("Returning RunEnvironmentInfo from a non-executable, non-test target has no effect"), "{mode}: {error}");
                }
                // StarlarkRuleConfiguredTargetUtil: TestEnvironment instances
                // only warn (shouldErrorOnNonExecutableRule=false) and are kept.
                "test_environment" => {
                    let result = result.unwrap();
                    assert_eq!(result.files.len(), 1);
                    // RunEnvironmentInfo and the framework's OutputGroupInfo.
                    assert_eq!(result.providers.len(), 2);
                    assert!(
                        result
                            .providers
                            .contains_key(crate::run_environment_info::PROVIDER_ID)
                    );
                }
                "callback_failure" => assert!(
                    result
                        .unwrap_err()
                        .contains("callback reached after registration")
                ),
                _ => {
                    let result = result.unwrap();
                    assert_eq!(result.actions.len(), 1);
                    assert!(result.actions[0].action.environment.is_empty());
                    assert_eq!(
                        result.actions[0].action.write.as_ref().unwrap().content,
                        b"registered, never executed"
                    );
                    assert_eq!(result.declared.len(), 1);
                    assert_eq!(result.files.len(), usize::from(mode != "nested_empty"));
                    // The returned provider and the framework's OutputGroupInfo.
                    assert_eq!(result.providers.len(), 2);
                    assert!(
                        result
                            .providers
                            .contains_key(crate::output_group_info::PROVIDER_ID)
                    );
                    assert!(
                        !result
                            .providers
                            .contains_key(crate::run_environment_info::PROVIDER_ID)
                    );
                }
            }
            assert!(
                !root
                    .path()
                    .join("bazel-out/k8-fastbuild/bin/output")
                    .exists()
            );
        }
    }

    #[test]
    fn run_environment_nested_consumer_and_actual_provider_requirement_gate() {
        let root = tempfile::Builder::new()
            .prefix("run-environment-consumer-")
            .tempdir_in("/tmp/opencode")
            .unwrap();
        std::fs::write(root.path().join("defs.bzl"), r#"
P = provider()
def producer_impl(ctx):
    return P(info=RunEnvironmentInfo({"FLAG": "seed"}, ["HOME", "HOME"]))
producer = rule(implementation=producer_impl)
def consumer_impl(ctx):
    if ctx.attr.strict:
        fail("strict consumer callback must not execute")
    target = ctx.attr.dep
    (RunEnvironmentInfo not in target) or fail("nested native key leaked")
    info = target[P].info
    copy = info.environment
    copy["FLAG"] = "local"
    (info.environment == {"FLAG": "seed"}) or fail("snapshot")
    out = ctx.actions.declare_file(ctx.label.name + ".out")
    ctx.actions.run_shell(outputs=[out], command="printf '%s' \"$FLAG\"", env=info.environment)
    return [DefaultInfo(files=depset([out]))]
consumer = rule(implementation=consumer_impl, attrs={"dep": attr.label(providers=[P]), "strict": attr.bool()})
strict = rule(implementation=consumer_impl, attrs={"dep": attr.label(providers=[RunEnvironmentInfo]), "strict": attr.bool(default=True)})
"#).unwrap();
        std::fs::write(root.path().join("BUILD.bazel"), "load(':defs.bzl', 'producer', 'consumer', 'strict')\nproducer(name='producer')\nconsumer(name='consumer', dep=':producer')\nstrict(name='strict', dep=':producer')\n").unwrap();
        let package = crate::package::evaluate(root.path(), "").unwrap();
        let producer = analyze(
            &package[&Label::parse("//:producer", "").unwrap()],
            &BTreeMap::new(),
        )
        .unwrap();
        let deps = BTreeMap::from([("//:producer".into(), {
            // A direct analysis caller mints the identity (tests only;
            // the engine shares one per ConfiguredTargetKey, N55).
            let runfiles = Arc::new(producer.runfiles);
            let identity = std::sync::Arc::new(dependency_target_data(
                "//:producer",
                "//:producer",
                &producer.files,
                false,
                true,
                false,
                false,
                &producer.providers,
                &producer.provider_keys,
                &runfiles,
            ));
            DependencyTarget::new(DependencyView {
                label: "//:producer".into(),
                files: producer.files,
                is_source: false,
                is_rule: true,
                is_package_group: false,
                providers: producer.providers,
                provider_keys: producer.provider_keys,
                is_filegroup: false,
                runfiles,
                rule_class: Some("producer".into()),
                identity,
            })
        })]);
        let consumer = analyze(&package[&Label::parse("//:consumer", "").unwrap()], &deps).unwrap();
        assert_eq!(consumer.actions.len(), 1);
        assert_eq!(
            consumer.actions[0].action.environment,
            BTreeMap::from([("FLAG".into(), "seed".into())])
        );
        let error = analyze(&package[&Label::parse("//:strict", "").unwrap()], &deps).unwrap_err();
        assert!(
            error.contains("mandatory providers") && error.contains("RunEnvironmentInfo"),
            "{error}"
        );
        assert!(!error.contains("strict consumer callback"), "{error}");
    }

    fn provider_fixture() -> (
        BTreeMap<String, DependencyTarget>,
        BTreeMap<String, AttrSpec>,
    ) {
        let frozen = Module::with_temp_heap(|module| {
            let context = crate::loading::ExtensionContext {
                byte_strings: false,
                label: Label::parse("//defs:providers.bzl", "").unwrap(),
                repositories: Arc::new(RepositorySet::new(std::path::PathBuf::from("."))),
            };
            let globals = starlark::environment::GlobalsBuilder::standard()
                .with(crate::custom_rule::rule_globals)
                .with(crate::providers::globals)
                .with(crate::template_variable_info::install)
                .build();
            let ast = AstModule::parse(
                "//defs:providers.bzl",
                r#"
P = provider()
Q = provider()
NotPackageSpecificationInfo = provider()
NotPackageSpecificationProvider = provider()
T = platform_common.TemplateVariableInfo
p = P()
t = T({"key": "value"})
unrestricted = attr.label()
default = attr.label(providers=[DefaultInfo])
both = attr.label(providers=[P, T])
alternative = attr.label_list(providers=[[P, Q], [T]])
missing = attr.label(providers=[Q])
group = attr.label(providers=[[NotPackageSpecificationInfo, Q]])
old_group_name = attr.label(providers=[NotPackageSpecificationProvider])
erased = attr.label(providers=[[], [NotPackageSpecificationInfo]])
"#
                .into(),
                &Dialect::Standard,
            )
            .unwrap();
            {
                let mut eval = Evaluator::new(&module);
                eval.extra = Some(&context);
                eval.eval_module(ast, &globals).unwrap();
            }
            module.freeze().unwrap()
        });
        let providers = ["p", "t"]
            .into_iter()
            .map(|name| {
                let value = frozen.get(name).unwrap();
                (
                    crate::providers::instance_identity(value.value()).unwrap(),
                    value,
                )
            })
            .collect();
        let specs = [
            "unrestricted",
            "default",
            "both",
            "alternative",
            "missing",
            "group",
            "old_group_name",
            "erased",
        ]
        .into_iter()
        .map(|name| {
            let value = frozen.get(name).unwrap();
            (
                name.to_owned(),
                crate::custom_rule::attribute_spec(value.value()).unwrap(),
            )
        })
        .collect();
        let target = {
            let identity = std::sync::Arc::new(dependency_target_data(
                "//pkg:dep",
                "//pkg:dep",
                &[],
                false,
                true,
                false,
                false,
                &providers,
                &[],
                &Default::default(),
            ));
            DependencyTarget::new(DependencyView {
                label: "//pkg:dep".into(),
                files: Vec::new(),
                is_source: false,
                is_rule: true,
                is_package_group: false,
                providers,
                provider_keys: Vec::new(),
                is_filegroup: false,
                runfiles: Default::default(),
                rule_class: Some("dep_rule".into()),
                identity,
            })
        };
        (BTreeMap::from([(target.label.clone(), target)]), specs)
    }

    #[test]
    fn ordinary_provider_constraints_use_actual_instances_and_synthesized_default() {
        let (mut deps, specs) = provider_fixture();
        for spec in ["unrestricted", "default", "both", "alternative"] {
            dependency(
                "//pkg:dep",
                "dep",
                &specs[spec],
                &deps,
                "",
                Consumer::default(),
            )
            .unwrap();
        }
        let error = dependency(
            "//pkg:dep",
            "dep",
            &specs["missing"],
            &deps,
            "PREFIX: ",
            Consumer::default(),
        )
        .unwrap_err()
        .to_string();
        // Only the missing provider of the alternative is described
        // (packages/RequiredProviders.java:157-190).
        assert_eq!(
            error,
            "PREFIX: '//pkg:dep' does not have mandatory providers: 'Q'"
        );
        deps.get_mut("//pkg:dep").unwrap().providers.clear();
        dependency(
            "//pkg:dep",
            "another_attr",
            &specs["default"],
            &deps,
            "",
            Consumer::default(),
        )
        .unwrap();
        for spec in ["both", "alternative", "group"] {
            assert!(
                dependency(
                    "//pkg:dep",
                    "another_attr",
                    &specs[spec],
                    &deps,
                    "",
                    Consumer::default()
                )
                .is_err()
            );
        }
        // An empty files collection does not turn an ordinary rule into a file.
        assert!(deps["//pkg:dep"].files.is_empty());
    }

    #[test]
    fn package_groups_have_separate_name_placement_gate_not_provider_satisfaction() {
        let (mut deps, specs) = provider_fixture();
        let group = deps.get_mut("//pkg:dep").unwrap();
        group.is_rule = false;
        group.is_package_group = true;
        group.providers.clear();
        for spec in ["group", "old_group_name"] {
            dependency(
                "//pkg:dep",
                "g",
                &specs[spec],
                &deps,
                "",
                Consumer::default(),
            )
            .unwrap();
        }
        for spec in ["unrestricted", "default", "erased", "both"] {
            let error = dependency(
                "//pkg:dep",
                "g",
                &specs[spec],
                &deps,
                "",
                Consumer::default(),
            )
            .unwrap_err()
            .to_string();
            assert!(
                error.contains("package group") && error.contains("misplaced"),
                "{error}"
            );
            for name in ["visibility", "_allowlist_function_transition"] {
                dependency(
                    "//pkg:dep",
                    name,
                    &specs[spec],
                    &deps,
                    "",
                    Consumer::default(),
                )
                .unwrap();
            }
        }
        let mut single = specs["group"].clone();
        single.allow_single_file = true;
        assert!(
            dependency("//pkg:dep", "g", &single, &deps, "", Consumer::default())
                .unwrap_err()
                .to_string()
                .contains("must produce a single file")
        );
    }

    #[test]
    fn file_targets_skip_provider_matching_but_keep_file_checks_first() {
        let (mut deps, specs) = provider_fixture();
        let target = deps.get_mut("//pkg:dep").unwrap();
        target.is_rule = false;
        target.is_source = true;
        target.providers.clear();
        let mut spec = specs["missing"].clone();
        assert!(
            dependency("//pkg:dep", "src", &spec, &deps, "", Consumer::default())
                .unwrap_err()
                .to_string()
                .contains("allow_files=False")
        );
        spec.allow_files = true;
        dependency("//pkg:dep", "src", &spec, &deps, "", Consumer::default()).unwrap();
        spec.allow_single_file = true;
        assert!(
            dependency("//pkg:dep", "src", &spec, &deps, "", Consumer::default())
                .unwrap_err()
                .to_string()
                .contains("must produce a single file")
        );
        deps.get_mut("//pkg:dep")
            .unwrap()
            .files
            .push(File::new(crate::starlark_types::FileData {
                is_directory: false,
                is_symlink: false,
                path: "pkg/dep".into(),
                short_path: "pkg/dep".into(),
                owner: "//pkg:dep".into(),
                is_source: true,
            }));
        dependency("//pkg:dep", "src", &spec, &deps, "", Consumer::default()).unwrap();
        // Output-file targets also have no rule class. Preserve the existing
        // is_source-only permission behavior independently of provider checks.
        deps.get_mut("//pkg:dep").unwrap().is_source = false;
        dependency(
            "//pkg:dep",
            "out",
            &specs["missing"],
            &deps,
            "",
            Consumer::default(),
        )
        .unwrap();
    }

    #[test]
    fn extension_lists_check_rule_files_to_build_by_file_name() {
        let (mut deps, specs) = provider_fixture();
        let mut spec = specs["unrestricted"].clone();
        spec.allow_files = true;
        spec.declaration.file_types = Some(vec![".go".into(), ".s".into()]);
        let check = |deps: &BTreeMap<String, DependencyTarget>, spec: &AttrSpec, name: &str| {
            dependency(
                "//pkg:dep",
                name,
                spec,
                deps,
                "P: ",
                Consumer {
                    rule_class: "go_library",
                    label: "//pkg:x",
                },
            )
            .map(|_| ())
            .map_err(|error| error.to_string())
        };
        let file = |path: &str| {
            File::new(crate::starlark_types::FileData {
                path: path.into(),
                short_path: path.into(),
                owner: "//pkg:dep".into(),
                is_source: false,
                is_directory: false,
                is_symlink: false,
            })
        };
        // RuleContext.java:1975-1993: no file to build, or none matching.
        let expected =
            "P: '//pkg:dep' does not produce any go_library srcs files (expected .go or .s)";
        assert_eq!(check(&deps, &spec, "srcs").unwrap_err(), expected);
        let dep = deps.get_mut("//pkg:dep").unwrap();
        dep.files.push(file("out/pkg/a.go.txt"));
        dep.files.push(file("out/pkg/go"));
        assert_eq!(check(&deps, &spec, "srcs").unwrap_err(), expected);
        // Private attributes are named natively (`$x`) in the message body.
        assert_eq!(
            check(&deps, &spec, "_srcs").unwrap_err(),
            "P: '//pkg:dep' does not produce any go_library $srcs files (expected .go or .s)"
        );
        // One matching file suffices; ctx.files stays unfiltered.
        deps.get_mut("//pkg:dep")
            .unwrap()
            .files
            .push(file("out/pkg/b.s"));
        check(&deps, &spec, "srcs").unwrap();
        // An empty list matches nothing.
        spec.declaration.file_types = Some(Vec::new());
        assert_eq!(
            check(&deps, &spec, "srcs").unwrap_err(),
            "P: '//pkg:dep' does not produce any go_library srcs files (expected nothing)"
        );
        // ANY_FILE checks only non-empty lists and single files (:1955-1959).
        spec.declaration.file_types = None;
        deps.get_mut("//pkg:dep").unwrap().files.clear();
        check(&deps, &spec, "srcs").unwrap();
        spec.non_empty = true;
        assert_eq!(
            check(&deps, &spec, "srcs").unwrap_err(),
            "P: '//pkg:dep' does not produce any go_library srcs files (expected any files)"
        );
        // allow_single_file: the single-artifact check comes first (:1966-1974).
        spec.non_empty = false;
        spec.allow_single_file = true;
        spec.declaration.file_types = Some(vec![".go".into()]);
        assert_eq!(
            check(&deps, &spec, "src").unwrap_err(),
            "P: '//pkg:dep' must produce a single file"
        );
        deps.get_mut("//pkg:dep")
            .unwrap()
            .files
            .push(file("out/pkg/a.txt"));
        assert_eq!(
            check(&deps, &spec, "src").unwrap_err(),
            "P: '//pkg:dep' does not produce any go_library src files (expected .go)"
        );
    }

    #[test]
    fn provider_checks_precede_callback_and_check_each_attribute_and_list_item() {
        let root = tempfile::Builder::new()
            .prefix("provider-analysis-")
            .tempdir_in("/tmp/opencode")
            .unwrap();
        std::fs::write(
            root.path().join("defs.bzl"),
            r#"
P = provider()
def impl(ctx):
    fail("consumer callback entered")
r = rule(implementation=impl, attrs={
    "a": attr.label(providers=[DefaultInfo]),
    "b": attr.label(providers=[P]),
    "many": attr.label_list(providers=[platform_common.TemplateVariableInfo]),
})
"#,
        )
        .unwrap();
        let (mut deps, _) = provider_fixture();
        let mut second = deps["//pkg:dep"].clone();
        second.label = "//pkg:second".into();
        second.providers.clear();
        deps.insert(second.label.clone(), second);
        for (call, expected) in [
            (
                "r(name='consumer', a='//pkg:dep', b='//pkg:dep')",
                "in b attribute of r rule //:consumer: '//pkg:dep' does not have mandatory providers: 'P'",
            ),
            (
                "r(name='consumer', many=['//pkg:dep', '//pkg:second'])",
                "in many attribute of r rule //:consumer: ",
            ),
            ("r(name='consumer', b=None)", "consumer callback entered"),
        ] {
            std::fs::write(
                root.path().join("BUILD.bazel"),
                format!("load(':defs.bzl', 'r')\n{call}\n"),
            )
            .unwrap();
            let package = crate::package::evaluate(root.path(), "").unwrap();
            let rule = &package[&Label::parse("//:consumer", "").unwrap()];
            // Use the producer's real instance under its own exported identity;
            // this definition's distinct P intentionally cannot satisfy it.
            let error = analyze(rule, &deps).unwrap_err();
            assert!(error.contains(expected), "{call}: {error}");
            if expected.starts_with("in many attribute") {
                assert!(error.contains("//pkg:second"), "{error}");
            }
            if expected != "consumer callback entered" {
                assert!(!error.contains("consumer callback entered"), "{error}");
            }
        }
    }

    fn empty_context() -> Context {
        Context {
            label: Label::parse("//pkg:rule", "").unwrap(),
            rule_class: "rule".into(),
            action_env: Default::default(),
            output_root: crate::label::GENERATED_EXEC_ROOT.into(),
            action_owner: "//pkg:rule".into(),
            declared: RefCell::new(Vec::new()),
            shareable: RefCell::new(BTreeSet::new()),
            actions: RefCell::new(Vec::new()),
            active: Arc::new(AtomicBool::new(true)),
            top_level_functions: Default::default(),
            loaded_functions: None,
            repositories: Arc::new(RepositorySet::new(std::path::PathBuf::from("."))),
            scope: crate::analysis_runtime::ScopeState::new("//pkg:rule".into(), BTreeSet::new()),
            subrule: Default::default(),
            runfiles: Default::default(),
            exec_groups: Default::default(),
            propagated_tags: Default::default(),
            retained_mappers: Default::default(),
        }
    }

    fn evaluate(source: &str) -> anyhow::Result<Context> {
        let context = empty_context();
        Module::with_temp_heap(|module| -> anyhow::Result<()> {
            module.set(
                "ctx",
                module.heap().alloc(AllocStruct([(
                    "actions",
                    crate::analysis_runtime::alloc_actions(module.heap(), context.scope.root()),
                )])),
            );
            let mut eval = Evaluator::new(&module);
            eval.extra = Some(&context);
            let ast = AstModule::parse("analysis-test.star", source.to_owned(), &Dialect::Standard)
                .map_err(|error| anyhow!(error.to_string()))?;
            eval.eval_module(ast, &Globals::standard())
                .map_err(|error| anyhow!(error.to_string()))?;
            Ok(())
        })?;
        Ok(context)
    }

    #[test]
    fn declarations_are_owned_canonical_and_idempotent() {
        let context = empty_context();
        let first = context.declare("dir/result").unwrap();
        assert_eq!(first, context.declare("dir/result").unwrap());
        assert_eq!(first.path, "bazel-out/k8-fastbuild/bin/pkg/dir/result");
        assert_eq!(first.short_path, "pkg/dir/result");
        assert_eq!(first.owner, "//pkg:rule");
        assert!(!first.is_source);
        assert_eq!(context.declared.borrow().len(), 1);
        for name in [
            "",
            "../out",
            "/out",
            "//other:out",
            ":out",
            "a//b",
            "a/./b",
            "a\\b",
        ] {
            assert!(context.declare(name).is_err(), "accepted {name:?}");
        }
    }

    #[test]
    fn write_records_exact_bytes_without_shell_or_io() {
        let context = evaluate("out = ctx.actions.declare_file('out')\nctx.actions.write(out, 'hi\\x00hé\\n', is_executable=True)\n").unwrap();
        let actions = context.actions.borrow();
        let action = &actions[0];
        assert_eq!(action.action.id, "//pkg:rule#0");
        assert!(action.action.command.is_empty());
        assert!(action.action.inputs.is_empty());
        assert!(action.inputs.is_empty());
        let write = action.action.write.as_ref().unwrap();
        assert_eq!(write.content, "hi\0hé\n".as_bytes());
        assert!(write.executable);
    }

    #[test]
    fn run_shell_records_arguments_environment_and_local_edges() {
        let context = evaluate(r#"first = ctx.actions.declare_file('first')
second = ctx.actions.declare_file('second')
ctx.actions.run_shell(outputs=[second], inputs=[first], command='cat "$1" > "$2"', arguments=[first.path, second.path], env={'WORD': 'a b'}, mnemonic='Copy', progress_message='copying')
ctx.actions.write(first, 'hello')
"#).unwrap();
        let actions = context.actions.borrow();
        assert_eq!(actions.len(), 2);
        assert_eq!(actions[0].inputs, actions[1].outputs);
        assert_eq!(actions[0].action.environment["WORD"], "a b");
        assert_eq!(actions[0].action.arguments.len(), 2);
        assert!(actions[0].action.inputs.is_empty());
        assert!(actions[0].action.write.is_none());
    }

    #[test]
    fn duplicate_producers_and_unsupported_options_are_rejected() {
        for (body, expected) in [
            (
                "ctx.actions.write(out, 'a')\nctx.actions.write(out, 'b')",
                "file 'pkg/out' is generated by these conflicting actions:",
            ),
            (
                "ctx.actions.run_shell(outputs=[], command='true')",
                "param 'outputs' may not be empty",
            ),
            (
                "ctx.actions.run_shell(outputs=[out], command='true', exec_group='other')",
                "Action declared for non-existent exec group 'other'.",
            ),
            (
                "ctx.actions.run_shell(outputs=[out], command='true', tools=1)",
                "in call to run_shell(), parameter 'tools' got value of type 'int', want 'sequence or depset'",
            ),
            (
                "ctx.actions.run_shell(outputs=[out], command='true', tools=[1])",
                "expected value of type 'File, FilesToRunProvider or Depset of Files' for a member of parameter 'tools' but got int instead",
            ),
            (
                "ctx.actions.run_shell(outputs=[out], command='true', arguments=[1])",
                "strings",
            ),
            (
                "ctx.actions.run_shell(outputs=[out], command='true', env={'X': 1})",
                "strings",
            ),
        ] {
            let source = format!("out = ctx.actions.declare_file('out')\n{body}\n");
            let error = match evaluate(&source) {
                Ok(_) => panic!("accepted {body}"),
                Err(error) => error.to_string(),
            };
            assert!(error.contains(expected), "expected {expected:?} in {error}");
        }
    }

    #[test]
    fn foreign_outputs_and_unknown_inputs_are_rejected() {
        let context = empty_context();
        let output = context.declare("out").unwrap();
        let mut foreign = output.clone();
        foreign.make_mut().owner = "//other:rule".into();
        assert!(
            context
                .register(
                    vec![foreign.clone()],
                    Vec::new(),
                    "true".into(),
                    None,
                    None,
                    Vec::new(),
                    BTreeMap::new(),
                    false,
                    false
                )
                .is_err()
        );
        assert!(
            context
                .register(
                    vec![output],
                    vec![foreign],
                    "true".into(),
                    None,
                    None,
                    Vec::new(),
                    BTreeMap::new(),
                    false,
                    false
                )
                .is_err()
        );
        assert!(context.actions.borrow().is_empty());
    }
}
