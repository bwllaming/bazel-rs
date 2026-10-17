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

use crate::custom_rule::{AttrSpec, AttrValue};
use crate::package::Rule;
use crate::repository::RepositorySet;
use crate::starlark_types::{File, alloc_file, alloc_target};

#[derive(Clone, Debug)]
pub struct DependencyTarget {
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
    pub provider_files: Vec<File>,
    /// Display names of the returned declared providers, in return order.
    pub provider_keys: Vec<String>,
    /// RunfilesProvider/FilesToRunProvider (docs/RUNFILES.md).
    pub runfiles: crate::runfiles::TargetRunfiles,
}

use crate::analysis_runtime::Context;
pub use crate::analysis_runtime::RegisteredAction;
pub(crate) use crate::analysis_runtime::repository_context;

#[derive(Debug)]
pub struct AnalysisResult {
    pub files: Vec<File>,
    pub declared: Vec<File>,
    pub actions: Vec<RegisteredAction>,
    pub providers: BTreeMap<String, OwnedFrozenValue>,
    pub provider_files: Vec<File>,
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
fn alloc_dependency<'v>(heap: Heap<'v>, requested: &str, target: &DependencyTarget) -> Value<'v> {
    use crate::starlark_types::TargetKind;
    let mut provider_keys = target.provider_keys.clone();
    let actual = if target.is_source {
        TargetKind::InputFile
    } else if target.is_filegroup {
        // Filegroup adds InstrumentedFilesInfo as its only declared provider
        // (rules/filegroup/Filegroup.java:103-108) and makes a single file its
        // executable (Filegroup.java:107,117-119).
        provider_keys = vec!["InstrumentedFilesInfo".to_owned()];
        if target.files.len() == 1 {
            TargetKind::SingleExecutableRule
        } else {
            TargetKind::Rule
        }
    } else if target.is_rule {
        TargetKind::Rule
    } else if target.is_package_group {
        TargetKind::Unknown
    } else {
        TargetKind::OutputFile
    };
    // Both labels are canonical forms (crate::label::Label's Display).
    let kind = if requested != target.label {
        TargetKind::Alias(requested.to_owned(), Box::new(actual))
    } else {
        actual
    };
    crate::starlark_types::alloc_target_of_kind(
        heap,
        &target.label,
        target.files.clone(),
        target.providers.clone(),
        kind,
        provider_keys,
        target.runfiles.clone(),
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
pub(crate) struct Consumer<'a> {
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
pub(crate) fn prerequisite_errors(
    name: &str,
    spec: &AttrSpec,
    target: &DependencyTarget,
    consumer: Consumer<'_>,
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
        // RuleContext.Builder.checkRuleDependencyMandatoryProviders
        // (analysis/RuleContext.java:2070-2095) with the missing providers'
        // RequiredProviders.getDescription (packages/RequiredProviders.java:157-190,
        // 207-212, 308-321): per alternative, the missing providers, quoted,
        // bracketed when several, joined with " or ".
        if !spec.required_providers.is_empty()
            && !spec
                .required_providers
                .iter()
                .any(|clause| clause.iter().all(provided))
        {
            errors.push(format!(
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
        errors.push(format!(
            "in {} attribute of {} rule {}: package group '{shown}' is misplaced here (they are only allowed in the visibility attribute)",
            crate::custom_rule::native_attribute_name(name, spec.declaration.late_bound),
            consumer.rule_class,
            consumer.label,
        ));
    }
    errors
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
    ensure!(
        !target.is_source || spec.allow_files || spec.allow_single_file,
        "attribute {name}: source file {label} is not allowed (allow_files=False)"
    );
    // crate::engine reports every prerequisite error before analysis; this
    // keeps the direct analysis entry points checked.
    if let Some(error) = prerequisite_errors(name, spec, target, consumer)
        .into_iter()
        .next()
    {
        bail!("{prefix}{error}");
    }
    Ok(target)
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
        values.insert(label, alloc_dependency(heap, label, target));
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
    const NAMES: [&str; 6] = ["attr", "files", "file", "outputs", "executable", "lifted"];
    let owned = Module::with_temp_heap(|module| -> anyhow::Result<_> {
        let structs =
            ctx_attribute_structs(rule, invocation, context, dependencies, module.heap())?;
        for (name, value) in NAMES.into_iter().zip(structs) {
            module.set(name, value);
        }
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
    let [attr, files, file, outputs, executable, lifted] = <[OwnedFrozenValue; 6]>::try_from(owned)
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
    let backing = heap.alloc(AllocStruct([
        ("attr", attr),
        ("files", files),
        ("file", file),
        ("outputs", outputs),
        ("executable", executable),
        ("label", label),
        (
            "actions",
            crate::analysis_runtime::alloc_actions_for(
                heap,
                context.scope.root(),
                crate::analysis_ctx::rule_context_repr(&rule.label.to_string()),
            ),
        ),
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
        Ok(crate::build_setting_ctx::wrap(
            heap,
            backing,
            value,
            context.scope.root(),
            surface,
        ))
    } else {
        Ok(crate::analysis_ctx::alloc_rule(
            heap,
            backing,
            context.scope.root(),
            None,
            invocation.target_platform.clone(),
            invocation.toolchains.clone(),
            invocation.exec_groups.clone(),
            surface,
        ))
    }
}

/// RuleContext.getDefaultTemplateVariableProviders (analysis/RuleContext.java:900-940):
/// the TemplateVariableInfo of the prerequisites of the attributes
/// `toolchains`, `:cc_toolchain`, `$toolchains`, `$cc_toolchain`, in that
/// order (DEFAULT_MAKE_VARIABLE_ATTRIBUTES), before the resolved toolchains'.
/// Starlark `_x` attributes are `$x`, or `:x` when late-bound
/// (AttributeValueSource.convertToNativeName). A non-empty built-in
/// `toolchains` attribute is a value gap (crate::custom_rule::builtin_gaps).
fn with_attribute_template_variables(
    invocation: &crate::custom_rule::CustomInvocation,
    dependencies: &BTreeMap<String, DependencyTarget>,
) -> crate::ctx_surface::CtxSurface {
    let mut surface = (*invocation.surface).clone();
    let mut from_attributes = Vec::new();
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
            .mapping(&rule.label.repository)
            .cloned()
            .unwrap_or_default(),
        errors: Default::default(),
    }
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
        format!(
            "in {shown} attribute of {} rule {}: ",
            invocation.declaration.rule_class,
            crate::rule_declarations::canonical_form(&rule.label)
        )
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
                // Nonempty values are rejected by the unanalyzed-feature gate.
                ensure!(
                    entries.is_empty(),
                    "attribute {name}: attr.label_keyed_string_dict values are {}",
                    crate::rule_declarations::NOT_ANALYZED
                );
                heap.alloc(starlark::values::dict::AllocDict::EMPTY)
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
                        .map(|requested| alloc_dependency(heap, requested, target))
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
                        .map(|target| alloc_dependency(heap, label, target))
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
                            let Some(file) = files_to_run.executable.clone() else {
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
                    .map(|(requested, target)| alloc_dependency(heap, requested, target));
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
                    heap.alloc(
                        targets
                            .iter()
                            .flat_map(|target| target.files.iter())
                            .cloned()
                            .map(|file| alloc_file(heap, file))
                            .collect::<Vec<_>>(),
                    ),
                );
                heap.alloc(
                    labels
                        .iter()
                        .zip(&targets)
                        .map(|(requested, target)| alloc_dependency(heap, requested, target))
                        .collect::<Vec<_>>(),
                )
            }
            AttrValue::Output(_) | AttrValue::Outputs(_)
                if name == "executable" && context.runfiles.default_executable.is_some() =>
            {
                // Outputs.addOutput (StarlarkRuleContext.java:362-371).
                bail!(
                    "in {} rule {}: Multiple outputs with the same key: executable",
                    invocation.declaration.rule_class,
                    crate::rule_declarations::canonical_form(&rule.label)
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
                "in {} rule {}: Multiple outputs with the same key: {key}",
                invocation.declaration.rule_class,
                crate::rule_declarations::canonical_form(&rule.label)
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
                    metadata_files.extend(
                        dependency
                            .files
                            .iter()
                            .cloned()
                            .map(|file| alloc_file(heap, file)),
                    );
                    targets.push(alloc_dependency(heap, &key, dependency));
                }
                files.insert(name.to_owned(), heap.alloc(metadata_files));
                heap.alloc(targets)
            }
            // The constraint values of the target's toolchain resolution
            // (crate::engine): Targets with their ConstraintValueInfo.
            ("exec_compatible_with", NativeType::LabelList) => {
                let mut targets = Vec::new();
                let mut constraint_files = Vec::new();
                for label in target.labels(name) {
                    let key = crate::label::Label::parse_canonical(&label)
                        .map(|label| label.to_string())
                        .unwrap_or(label.clone());
                    let dependency = dependencies.get(&key).ok_or_else(|| {
                        anyhow!("attribute {name}: dependency {label} was not resolved")
                    })?;
                    constraint_files.extend(
                        dependency
                            .files
                            .iter()
                            .cloned()
                            .map(|file| alloc_file(heap, file)),
                    );
                    targets.push(alloc_dependency(heap, &key, dependency));
                }
                files.insert(name.to_owned(), heap.alloc(constraint_files));
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
            empty.push(format!(
                "in {} attribute of {} rule {}: attribute must be non empty",
                crate::custom_rule::attribute_message_name(name, spec),
                invocation.declaration.rule_class,
                crate::rule_declarations::canonical_form(&rule.label)
            ));
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
        eprintln!("{line}");
    }
    result
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
        self.0.borrow_mut().push(text.to_owned());
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
    let result = analyze_inner(rule, dependencies, repositories, target, &prints)
        .map_err(|error| format!("analysis of {}: {error:#}", rule.label));
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
            Ok(crate::runfiles::DefaultExecutable::new(File {
                is_directory: false,
                path: output
                    .generated_exec_path(target.output_root)
                    .to_string_lossy()
                    .into_owned(),
                short_path: output.short_path().to_string_lossy().into_owned(),
                owner: rule.label.to_string(),
                is_source: false,
            }))
        })
        .transpose()?;
    let mut surface = rule_surface(rule, invocation, &repositories, dependencies);
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
        output_root: target.output_root.to_owned(),
        action_owner: target.action_owner.to_owned(),
        // Dependencies' runfiles artifacts are observable through
        // dep[DefaultInfo] and their FilesToRunProviders.
        dependency_files: dependencies
            .values()
            .flat_map(|target| {
                target
                    .files
                    .iter()
                    .chain(&target.provider_files)
                    .cloned()
                    .chain(target.runfiles.files())
            })
            .collect(),
        declared: RefCell::new(Vec::new()),
        actions: RefCell::new(Vec::new()),
        active,
        top_level_functions: invocation.top_level_functions.clone(),
        loaded_functions: target.loaded_functions.cloned(),
        repositories,
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
            manifests: dependencies
                .values()
                .flat_map(|target| target.runfiles.manifests())
                .collect(),
            ..Default::default()
        },
    };
    for output in &rule.outs {
        context.declare(output)?;
    }
    let (files, providers, provider_files, provider_keys, target_runfiles) =
        Module::with_temp_heap(|module| -> anyhow::Result<_> {
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
                let mut message = reported.join("\n");
                if let Err(error) = &returned {
                    message.push('\n');
                    message.push_str(&error.to_string());
                }
                bail!("{message}");
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
                    args: invocation.declaration.builtin.target.strings("args"),
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
            // checkDeclaredProviders (analysis/starlark/StarlarkRuleConfiguredTargetUtil.java:138-148)
            // iterates getStarlarkProviders() only: built-in provider keys
            // (DefaultInfo, OutputGroupInfo, ...) are never checked (:141).
            for provider in &invocation.declaration.provides {
                ensure!(
                    provider.id.starts_with("builtin:") || custom.contains_key(&provider.id),
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
            }
            if let Some((_, validation)) = groups
                .iter()
                .find(|(name, _)| name == crate::output_group_info::VALIDATION)
                && !validation.node.empty
            {
                bail!(
                    "output group '{}' (validation actions) is not supported by the native profile",
                    crate::output_group_info::VALIDATION
                );
            }
            let merged = crate::output_group_info::merged(
                groups,
                target_runfiles.default_runfiles().all_artifacts(),
            );
            custom.insert(
                crate::output_group_info::PROVIDER_ID.to_owned(),
                module.heap().alloc(merged),
            );
            let mut slots = Vec::new();
            let mut provider_files = BTreeSet::new();
            for (identity, provider) in custom {
                provider_files.extend(crate::providers::provider_files(provider)?);
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
                .map_err(|error| anyhow!("cannot freeze analysis providers: {error:?}"))?;
            let providers = slots
                .into_iter()
                .map(|(identity, slot)| {
                    frozen
                        .get_any_visibility(&slot)
                        .map(|(value, _)| (identity, value))
                })
                .collect::<anyhow::Result<BTreeMap<_, _>>>()?;
            Ok((
                files,
                providers,
                provider_files.into_iter().collect::<Vec<_>>(),
                provider_keys,
                target_runfiles,
            ))
        })?;
    for file in files.iter().chain(&provider_files) {
        ensure!(
            context.known_file(file),
            "DefaultInfo contains unknown file {}",
            file.path
        );
    }
    // Runfiles artifacts need an artifact identity like every other file; the
    // FilesToRunProvider manifests are declared but have no action yet
    // (docs/RUNFILES.md).
    for file in target_runfiles.files() {
        ensure!(
            context.known_file(&file),
            "runfiles contain unknown file {}",
            file.path
        );
    }
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
        declared,
        actions,
        providers,
        provider_files,
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
                    assert!(result.provider_files.is_empty());
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
        let deps = BTreeMap::from([(
            "//:producer".into(),
            DependencyTarget {
                label: "//:producer".into(),
                files: producer.files,
                is_source: false,
                is_rule: true,
                is_package_group: false,
                providers: producer.providers,
                provider_files: producer.provider_files,
                provider_keys: producer.provider_keys,
                is_filegroup: false,
                runfiles: producer.runfiles,
            },
        )]);
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
        let target = DependencyTarget {
            label: "//pkg:dep".into(),
            files: Vec::new(),
            is_source: false,
            is_rule: true,
            is_package_group: false,
            providers,
            provider_files: Vec::new(),
            provider_keys: Vec::new(),
            is_filegroup: false,
            runfiles: Default::default(),
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
        deps.get_mut("//pkg:dep").unwrap().files.push(File {
            is_directory: false,
            path: "pkg/dep".into(),
            short_path: "pkg/dep".into(),
            owner: "//pkg:dep".into(),
            is_source: true,
        });
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
        let file = |path: &str| File {
            path: path.into(),
            short_path: path.into(),
            owner: "//pkg:dep".into(),
            is_source: false,
            is_directory: false,
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
            action_env: Default::default(),
            output_root: crate::label::GENERATED_EXEC_ROOT.into(),
            action_owner: "//pkg:rule".into(),
            dependency_files: BTreeSet::new(),
            declared: RefCell::new(Vec::new()),
            actions: RefCell::new(Vec::new()),
            active: Arc::new(AtomicBool::new(true)),
            top_level_functions: Default::default(),
            loaded_functions: None,
            repositories: Arc::new(RepositorySet::new(std::path::PathBuf::from("."))),
            scope: crate::analysis_runtime::ScopeState::new("//pkg:rule".into(), BTreeSet::new()),
            subrule: Default::default(),
            runfiles: Default::default(),
            exec_groups: Default::default(),
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
                "more than one",
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
        foreign.owner = "//other:rule".into();
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
