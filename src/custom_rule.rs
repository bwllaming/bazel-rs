//! Owned custom-rule schemas and trace/freeze-safe Starlark definitions.
// Starlark's fixed attribute signatures also generate module-level helpers.
#![allow(clippy::too_many_arguments)]

use allocative::Allocative;
use anyhow::{anyhow, bail};
use starlark::environment::GlobalsBuilder;
use starlark::eval::{Arguments, Evaluator};
use starlark::starlark_module;
use starlark::values::dict::DictRef;
use starlark::values::list::{AllocList, ListRef};
use starlark::values::none::NoneType;
use starlark::values::structs::AllocStruct;
use starlark::values::tuple::TupleRef;
use starlark::values::{OwnedFrozenValue, Value, ValueLike};
use std::collections::BTreeMap;
use std::fmt;
use std::sync::OnceLock;

use crate::build_settings::{BuildSetting, BuildSettingKind};
use crate::providers::ProviderRequirement;

#[derive(Clone, Debug, PartialEq, Eq, Allocative, serde::Serialize)]
pub enum AttrKind {
    String,
    StringList,
    StringListDict,
    StringDict,
    Int,
    Bool,
    Label,
    LabelList,
    Output,
    OutputList,
    /// `attr.int_list` (StarlarkAttrModuleApi.java:559).
    IntList,
    /// `attr.label_keyed_string_dict` (StarlarkAttrModuleApi.java:889).
    LabelKeyedStringDict,
    /// `attr.string_keyed_label_dict` (StarlarkAttrModuleApi.java:770-886;
    /// BuildType.LABEL_DICT_UNARY, packages/BuildType.java:63-64).
    StringKeyedLabelDict,
    /// `attr.label_list_dict` (StarlarkAttrModuleApi.java:1013-1133;
    /// BuildType.LABEL_LIST_DICT, packages/BuildType.java:74-75).
    LabelListDict,
}
#[derive(Clone, Debug, PartialEq, Eq, Allocative, serde::Serialize)]
pub enum AttrValue {
    String(String),
    Strings(Vec<String>),
    StringListDict(Vec<(String, Vec<String>)>),
    StringDict(Vec<(String, String)>),
    Int(i32),
    Bool(bool),
    Label(Option<String>),
    Labels(Vec<String>),
    Output(Option<String>),
    Outputs(Vec<String>),
    Ints(Vec<i32>),
    /// Canonical label keys with their string values, in insertion order.
    LabelKeyedStringDict(Vec<(String, String)>),
    /// String keys with canonical label values, in insertion order.
    StringKeyedLabelDict(Vec<(String, String)>),
    /// String keys with canonical label-list values, in insertion order.
    LabelListDict(Vec<(String, Vec<String>)>),
}
#[derive(Clone, Debug, PartialEq, Eq, Allocative, serde::Serialize)]
pub struct AttrSpec {
    pub kind: AttrKind,
    pub default: AttrValue,
    pub mandatory: bool,
    pub allow_files: bool,
    pub allow_single_file: bool,
    #[serde(skip_serializing_if = "is_false")]
    pub non_empty: bool,
    #[allow(private_interfaces)]
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub required_providers: Vec<Vec<ProviderRequirement>>,
    /// Load-time declaration metadata (docs/LOAD_DECLARATIONS.md); empty
    /// metadata preserves the previous schema serialization.
    #[serde(skip_serializing_if = "AttrDeclaration::is_default")]
    pub declaration: AttrDeclaration,
}
fn is_false(value: &bool) -> bool {
    !*value
}

/// One `attr.string/int(values=[...])` entry (Attribute.AllowedValueSet,
/// packages/Attribute.java:210-243). `value` is `None` when the entry has a
/// different type and therefore can never match.
#[derive(Clone, Debug, PartialEq, Eq, Allocative, serde::Serialize)]
pub struct AllowedValue {
    pub value: Option<AttrValue>,
    pub display: String,
}

/// Retained Bazel attribute options that the native analysis does not model.
/// See StarlarkAttrModule.createAttribute (StarlarkAttrModule.java:317-513).
#[derive(Clone, Debug, Default, PartialEq, Eq, Allocative, serde::Serialize)]
pub struct AttrDeclaration {
    /// `allow_files`/`allow_single_file` given as a list of file extensions
    /// (AllowedFileTypes::Extensions, validated at analysis).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub file_types: Option<Vec<String>>,
    #[serde(skip_serializing_if = "crate::rule_declarations::Cfg::is_target")]
    pub cfg: crate::rule_declarations::Cfg,
    #[serde(skip_serializing_if = "is_false")]
    pub executable: bool,
    /// Exported aspect identities (`<extension>%<name>`).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub aspects: Vec<String>,
    /// The aspects' query-visible definitions, required aspects first
    /// (AspectsList; read by `query` LabelVisitor.visitAspectsIfRequired).
    #[serde(skip)]
    #[allow(private_interfaces)]
    pub aspect_definitions: Vec<crate::rule_declarations::QueryAspect>,
    /// `default = configuration_field(...)` (StarlarkLateBoundDefault).
    #[serde(skip_serializing_if = "is_false")]
    pub late_bound: bool,
    /// The `configuration_field(fragment, name)` of a late-bound default
    /// (StarlarkLateBoundDefault.getFragmentName/getFragmentFieldName),
    /// resolved per configuration by crate::late_bound.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub configuration_field: Option<ConfigurationField>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub allow_rules: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub flags: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub values: Vec<AllowedValue>,
    #[serde(skip_serializing_if = "is_false")]
    pub skip_validations: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub for_dependency_resolution: Option<bool>,
    /// Whether `default` was supplied (Descriptor.hasDefault, used by aspect()).
    #[serde(skip)]
    pub explicit_default: bool,
    /// `default` is a Starlark function (AttributeValueSource.COMPUTED_DEFAULT,
    /// StarlarkAttrModule.java:358-368); the function is retained by the
    /// `Attribute` value and the rule, and evaluated per target at loading.
    #[serde(skip_serializing_if = "is_false")]
    pub computed_default: bool,
    /// `configurable=` was given (Attribute.configurableAttrWasUserSet,
    /// StarlarkAttrModule.java:402-410): only symbolic-macro attributes may
    /// set it (StarlarkRuleClassFunctions.java:964-970, :1374-1379).
    #[serde(skip_serializing_if = "is_false")]
    pub configurable_set: bool,
}

/// A `configuration_field(fragment, name)` identity.
#[derive(Clone, Debug, PartialEq, Eq, Allocative, serde::Serialize)]
pub struct ConfigurationField {
    pub fragment: String,
    pub name: String,
}

impl AttrDeclaration {
    pub(crate) fn is_default(&self) -> bool {
        Self {
            explicit_default: false,
            ..self.clone()
        } == Self::default()
    }

    /// A late-bound default whose configured value crate::late_bound resolves.
    pub(crate) fn late_bound_resolvable(&self) -> bool {
        self.late_bound
            && self.configuration_field.as_ref().is_some_and(|field| {
                crate::late_bound::resolve(&field.fragment, &field.name).is_some()
            })
    }

    /// AllowedValueSet.apply; an empty set allows everything.
    pub(crate) fn allows(&self, value: &AttrValue) -> bool {
        self.values.is_empty()
            || self
                .values
                .iter()
                .any(|allowed| allowed.value.as_ref() == Some(value))
    }

    /// AllowedValueSet.getErrorReason with StringUtil.joinEnglishListSingleQuoted
    /// (util/StringUtil.java:54-88).
    pub(crate) fn allowed_values_reason(&self, value: &AttrValue) -> String {
        let choices = self
            .values
            .iter()
            .map(|allowed| allowed.display.as_str())
            .collect::<Vec<_>>();
        let mut joined = String::new();
        for (index, choice) in choices.iter().enumerate() {
            if index > 0 {
                let last = index + 1 == choices.len();
                if !last || choices.len() > 2 {
                    joined.push(',');
                }
                if last {
                    joined.push_str(" or");
                }
                joined.push(' ');
            }
            joined.push('\'');
            joined.push_str(choice);
            joined.push('\'');
        }
        if joined.is_empty() {
            joined.push_str("nothing");
        }
        let shown = match value {
            AttrValue::String(text) => text.clone(),
            AttrValue::Int(number) => number.to_string(),
            other => format!("{other:?}"),
        };
        format!("has to be one of {joined} instead of '{shown}'")
    }

    /// Features a target exercises at analysis when this attribute has a value.
    pub(crate) fn unanalyzed(&self, name: &str) -> Vec<String> {
        let mut features = Vec::new();
        // The exec transition and Starlark `transition()`s are modelled
        // (crate::engine, crate::starlark_transitions, docs/CONFIGURATION.md);
        // config.none() and analysis_test_transition are not.
        if !matches!(
            self.cfg,
            crate::rule_declarations::Cfg::Target
                | crate::rule_declarations::Cfg::Exec(_)
                | crate::rule_declarations::Cfg::Starlark {
                    analysis_test: false,
                    ..
                }
        ) {
            features.push(format!(
                "attribute '{name}' cfg={} (configuration transition)",
                self.cfg.describe()
            ));
        }
        // `aspects` are analyzed (N58, docs/ASPECTS.md): they propagate to
        // the attribute's dependencies and merge their providers into the
        // aspect-configured targets consumers see.
        if self.late_bound && !self.late_bound_resolvable() {
            let field = self
                .configuration_field
                .as_ref()
                .map(|field| format!(" {}.{}", field.fragment, field.name))
                .unwrap_or_default();
            features.push(format!(
                "attribute '{name}' late-bound default (configuration_field{field})"
            ));
        }
        // Extension-list allow_files/allow_single_file and allow_rules are
        // validated at analysis (AllowedFileTypes, crate::engine
        // starlark_prerequisite_errors, crate::analysis::prerequisite_errors).
        // ORDER_INDEPENDENT is implemented: the BUILD-language conversion
        // sorts list values (order_independent_sorted).
        let flags = self
            .flags
            .iter()
            .filter(|flag| {
                !NO_OP_ATTRIBUTE_FLAGS.contains(&flag.as_str()) && *flag != "ORDER_INDEPENDENT"
            })
            .map(String::as_str)
            .collect::<Vec<_>>();
        if !flags.is_empty() {
            features.push(format!("attribute '{name}' flags=[{}]", flags.join(", ")));
        }
        if self.skip_validations {
            features.push(format!("attribute '{name}' skip_validations=True"));
        }
        features
    }
}

/// Attribute.PropertyFlag names (packages/Attribute.java:83-205);
/// `attr.*(flags = [...])` resolves each with PropertyFlag.valueOf and fails
/// with `unknown attribute flag 'X'` otherwise (Attribute.java:466-479).
pub(crate) const ATTRIBUTE_FLAGS: &[&str] = &[
    "MANDATORY",
    "EXECUTABLE",
    "UNDOCUMENTED",
    "TAGGABLE",
    "ORDER_INDEPENDENT",
    "STRICT_LABEL_CHECKING",
    "DIRECT_COMPILE_TIME_INPUT",
    "NON_EMPTY",
    "SINGLE_ARTIFACT",
    "SILENT_RULECLASS_FILTER",
    "SKIP_ANALYSIS_TIME_FILETYPE_CHECK",
    "CHECK_ALLOWED_VALUES",
    "NONCONFIGURABLE",
    "CONFIGURABLE_ATTR_WAS_USER_SET",
    "SKIP_PREREQ_VALIDATOR_CHECKS",
    "CHECK_CONSTRAINTS_OVERRIDE",
    "SKIP_CONSTRAINTS_OVERRIDE",
    "OUTPUT_LICENSES",
    "HAS_STARLARK_DEFINED_TRANSITION",
    "HAS_ANALYSIS_TEST_TRANSITION",
    "IS_TOOL_DEPENDENCY",
    "STARLARK_DEFINED",
    "SKIP_VALIDATIONS",
    "FOR_DEPENDENCY_RESOLUTION",
    "FOR_DEPENDENCY_RESOLUTION_EXPLICITLY_SET",
];

/// Flags whose only effect native analysis provably shares. SKIP_CONSTRAINTS_OVERRIDE
/// is read only by RuleContextConstraintSemantics.getConstraintCheckedDependencies
/// (analysis/constraints/RuleContextConstraintSemantics.java:731-736, via
/// Attribute.skipConstraintsOverride, packages/Attribute.java:2096-2098): it
/// removes the attribute's dependencies from the constraint-environment check.
/// Native analysis performs no environment check at all, and every input that
/// could make that check fail (non-empty `compatible_with`/`restricted_to`,
/// environment rules) is an explicit gap, so skipping it is a no-op.
/// DIRECT_COMPILE_TIME_INPUT is only read by CompileOneDependencyTransformer
/// (--compile_one_dependency, pkgcache/CompileOneDependencyTransformer.java),
/// which the native profile does not implement.
const NO_OP_ATTRIBUTE_FLAGS: &[&str] = &["SKIP_CONSTRAINTS_OVERRIDE", "DIRECT_COMPILE_TIME_INPUT"];

/// BuildType.convertFromBuildLangType (packages/BuildType.java:245-253): an
/// ORDER_INDEPENDENT attribute's converted List value is canonicalized with
/// a natural-order sort. Labels order by Label.compareTo
/// (cmdline/Label.java:677-685): repository name, package path, target name
/// — a component order, not the canonical-string order. Only values that go
/// through the BUILD-language conversion are sorted: explicit call and
/// initializer values and simplified unconditional selects. Defaults are
/// converted once at rule-class creation (Type.convert, no sort), and
/// analysis-time select resolution resolves already-converted branch values
/// (SelectorList.resolve), so neither sorts.
pub(crate) fn order_independent_sorted(
    spec: &AttrSpec,
    mut value: AttrValue,
) -> anyhow::Result<AttrValue> {
    if !spec
        .declaration
        .flags
        .iter()
        .any(|flag| flag == "ORDER_INDEPENDENT")
    {
        return Ok(value);
    }
    match &mut value {
        AttrValue::Strings(values) | AttrValue::Outputs(values) => values.sort(),
        AttrValue::Ints(values) => values.sort(),
        AttrValue::Labels(values) => {
            // The values are canonical-form strings this module produced
            // (AttrKind::parse -> label_value -> Label::to_string); a value
            // that no longer parses is an internal error, never a silently
            // unsorted list.
            let mut keyed = values
                .iter()
                .map(|label| {
                    Ok((
                        crate::label::Label::parse_canonical(label)
                            .map_err(|error| {
                                anyhow!("internal error: ORDER_INDEPENDENT label {label:?} does not parse canonically: {error}")
                            })?,
                        label.clone(),
                    ))
                })
                .collect::<anyhow::Result<Vec<_>>>()?;
            keyed.sort_by(|a, b| a.0.cmp(&b.0));
            *values = keyed.into_iter().map(|(_, label)| label).collect();
        }
        _ => {}
    }
    Ok(value)
}

/// An element of a `select()`-valued BUILD attribute (BuildType.SelectorList).
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
pub enum ConfigurablePart {
    Plain(AttrValue),
    /// Condition label (or `//conditions:default`) and branch value (`None` = default).
    Select {
        branches: Vec<(String, Option<AttrValue>)>,
        no_match_error: String,
    },
}

/// Load-time invocation metadata beyond ordinary attribute values.
#[derive(Clone, Debug, Default)]
pub struct InvocationDeclaration {
    /// Implicit outputs from `rule(outputs={...})`, already in `Rule::outs`.
    pub implicit_outputs: Vec<(String, String)>,
    /// Exercised features that native analysis does not implement; analysis
    /// fails explicitly when nonempty, after attribute validation and before
    /// dependencies (test/executable classes, late-bound defaults, ...).
    pub unanalyzed: Vec<String>,
    /// A rule `cfg` transition: it decides the configuration the target and
    /// its select() conditions are analyzed in, so it fails first.
    pub transition_gaps: Vec<String>,
    /// Rule-level execution platform features the native toolchain
    /// resolution does not model (exec groups), which Bazel runs after the
    /// select() conditions and before attribute validation
    /// (analysis/producers/PrerequisiteProducer).
    pub toolchain_gaps: Vec<String>,
    /// `rule(toolchains = ...)`: canonical type labels and `mandatory`, in
    /// declaration order (RuleClass.getToolchainTypes).
    pub toolchain_types: Vec<(String, bool)>,
    /// `rule(fragments = [...])` (ConfigurationFragmentPolicy,
    /// StarlarkRuleClassFunctions.java:1051-1052): the fragments
    /// `ctx.fragments` may access besides the universal ones.
    pub fragments: Vec<String>,
    /// `rule(exec_compatible_with = ...)` canonical labels
    /// (RuleClass.getExecutionPlatformConstraints).
    pub exec_compatible_with: Vec<String>,
    /// `rule(exec_groups = ...)` (RuleClass.getDeclaredExecGroups), in
    /// declaration order (docs/EXEC_GROUPS.md).
    pub exec_groups: Vec<crate::rule_declarations::DeclaredExecGroup>,
    /// The `.bzl` that called rule() (`ctx.toolchains` string keys).
    pub definition_label: String,
    /// RuleClass.getRuleDefinitionEnvironmentLabel: the `.bzl` being
    /// initialized when rule() ran ([`RuleDeclaration::definition_environment_label`]).
    pub definition_environment_label: String,
    /// `rule(provides=[...])` identities checked against returned providers.
    #[allow(private_interfaces)]
    pub provides: Vec<ProviderRequirement>,
    /// Retained `select()` attribute values.
    pub configurable: BTreeMap<String, Vec<ConfigurablePart>>,
    /// Starlark attribute names in rule-class order: the `attrs` dict order,
    /// then the build-setting attributes (StarlarkRuleClassFunctions.java:935-1020,
    /// RuleClass.java:904-919). The schema map itself is name-ordered.
    pub attr_order: Vec<String>,
    /// Labels of Starlark `select()` keys and branches (and of the attribute
    /// default for `None` branches): assumed input files
    /// (AggregatingAttributeMapper.visitLabelsInSelect,
    /// packages/AggregatingAttributeMapper.java:182-215; Package.java:893-920).
    pub configurable_labels: Vec<crate::label::Label>,
    /// Rule class name for Bazel-form diagnostics (the exported rule name).
    pub rule_class: String,
    /// The explicitly specified `scope` of a string `scope` attribute
    /// (crate::starlark_options::ScopeAttribute).
    pub explicit_scope: crate::starlark_options::ScopeAttribute,
    /// Bazel's built-in attributes of this instance (docs/COMMON_RULE_ATTRIBUTES.md).
    pub builtin: BuiltinAttributes,
    /// Starlark attributes given a value by the BUILD call or an initializer
    /// (Rule.isAttributeValueExplicitlySpecified): `query --noimplicit_deps`.
    pub explicit: std::collections::BTreeSet<String>,
    /// RuleClass.getCallStack (`query --output=build`).
    pub definition_stack: Vec<(String, String)>,
}

/// Values of the built-in attributes a Starlark rule class inherits from
/// `StarlarkRuleClassFunctions.baseRule`/`binaryBaseRule`/`getTestBaseRule`
/// (crate::native_rules::starlark_builtins).
#[derive(Clone, Debug, Default)]
pub struct BuiltinAttributes {
    pub kind: crate::native_rules::StarlarkRuleKind,
    /// Explicit non-None values, retained `select()`s, effective `tags` and
    /// assumed input labels (`rule_class` is the exported rule name).
    pub target: crate::native_rules::NativeTarget,
    /// Explicit `visibility` after RuleVisibility.validateAndSimplify
    /// (canonical label strings); `None` when unset.
    pub visibility: Option<Vec<String>>,
    /// Effective `testonly` (explicit, the TEST class override, or the package
    /// default; BaseRuleClasses.java:68-74), finalized with the package.
    pub testonly: bool,
    /// Effective `deprecation` (explicit or the package default;
    /// BaseRuleClasses.java:76-82), finalized with the package.
    pub deprecation: Option<String>,
    /// Computed `generator_*` values when created by a macro; explicit
    /// values (in `target`) win.
    pub generator: Option<crate::package::Generator>,
}
/// The modelled options a CppConfiguration reads (rules/cpp/CppConfiguration.java:
/// compilationMode, --strip, --force_pic), taken from the Configuration the
/// target is analyzed in (crate::engine). The default is the default
/// configuration.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CppConfigurationView {
    pub compilation_mode: &'static str,
    pub strip: &'static str,
    pub force_pic: bool,
    /// The no-config configuration has no configuration fragments at all
    /// (CommonOptions.EMPTY_OPTIONS, analysis/config/CommonOptions.java:32-42).
    pub no_fragments: bool,
    /// An exec configuration: the exec transition appends `-g0` to `--copt`
    /// and `--cxxopt` on non-Windows hosts (builtin_exec_platforms.bzl:341-342).
    pub exec: bool,
}

impl Default for CppConfigurationView {
    fn default() -> Self {
        Self {
            compilation_mode: "fastbuild",
            strip: "sometimes",
            force_pic: false,
            no_fragments: false,
            exec: false,
        }
    }
}

impl CppConfigurationView {
    pub fn of(native: &crate::configuration::NativeOptions) -> Self {
        Self {
            compilation_mode: native.compilation_mode,
            strip: native.strip,
            force_pic: native.force_pic,
            no_fragments: native.no_config,
            exec: native.is_exec,
        }
    }

    /// CppConfiguration.stripBinaries (rules/cpp/CppConfiguration.java:299-302).
    pub(crate) fn should_strip_binaries(&self) -> bool {
        self.strip == "always"
            || (self.strip == "sometimes" && self.compilation_mode == "fastbuild")
    }
}

#[derive(Clone, Debug)]
pub struct CustomInvocation {
    pub build_setting: Option<BuildSetting>,
    /// The configured value of a build setting (`ctx.build_setting_value`),
    /// set by crate::engine for the analysis configuration; `None` means
    /// the `build_setting_default` (StarlarkRuleContext.java:738-757).
    pub setting_value: Option<crate::configuration::SettingValue>,
    /// The target platform the implementation sees, set by crate::engine
    /// (`ctx.target_platform_has_constraint`).
    pub target_platform: crate::platforms::TargetPlatform,
    /// The resolved toolchain context (`ctx.toolchains`), set by
    /// crate::engine; `None` for rules without toolchain resolution.
    pub toolchains: Option<std::sync::Arc<crate::toolchain_info::ToolchainContextData>>,
    /// The ToolchainCollection behind `ctx.exec_groups`, set by crate::engine;
    /// `None` for rules without toolchain resolution.
    #[allow(private_interfaces)]
    pub exec_groups: Option<std::sync::Arc<crate::exec_groups::ExecGroupsData>>,
    /// RuleContext.useAutoExecGroups, set by crate::engine.
    pub use_auto_exec_groups: bool,
    /// The exec groups actions may choose (crate::exec_groups), set by
    /// crate::engine.
    #[allow(private_interfaces)]
    pub action_exec_groups: crate::exec_groups::ActionExecGroups,
    /// The configuration's C++ options for `subrule_ctx.fragments.cpp`, set by
    /// crate::engine before analysis.
    pub cpp: CppConfigurationView,
    /// The configuration facts of the N23c `ctx` members, set by
    /// crate::engine before analysis (crate::ctx_surface).
    pub surface: std::sync::Arc<crate::ctx_surface::CtxSurface>,
    pub implementation: OwnedFrozenValue,
    /// Strong frozen roots for direct declarations, including factory closures.
    pub subrules: Vec<OwnedFrozenValue>,
    /// The package's transitive `.bzl` top-level functions (shared).
    pub top_level_functions: std::sync::Arc<[OwnedFrozenValue]>,
    /// Retained Starlark `transition()` objects: `""` for the rule's `cfg`
    /// (an incoming rule transition), else the attribute name (an outgoing
    /// attribute transition); crate::starlark_transitions.
    pub transitions: BTreeMap<String, OwnedFrozenValue>,
    pub attrs: BTreeMap<String, AttrValue>,
    pub schema: BTreeMap<String, AttrSpec>,
    pub declaration: InvocationDeclaration,
}

/// Attribute sequences accept only lists and tuples, without copying their elements.
pub(crate) fn sequence_ref<'v>(value: Value<'v>) -> Option<&'v [Value<'v>]> {
    ListRef::from_value(value)
        .map(ListRef::content)
        .or_else(|| TupleRef::from_value(value).map(TupleRef::content))
}

fn string_list(value: Value<'_>) -> anyhow::Result<Vec<String>> {
    sequence_ref(value)
        .ok_or_else(|| anyhow!("expected a list of strings"))?
        .iter()
        .map(|v| {
            v.unpack_str()
                .map(str::to_owned)
                .ok_or_else(|| anyhow!("expected a list of strings"))
        })
        .collect()
}

/// Normalize OR-of-AND requirements without copying an unbounded input sequence.
/// Every constructor is validated, even after an empty alternative was seen.
fn required_providers(value: Value<'_>) -> anyhow::Result<Vec<Vec<ProviderRequirement>>> {
    let outer = sequence_ref(value)
        .ok_or_else(|| anyhow!("providers must be a list or tuple of Provider constructors"))?;
    let mut budget = crate::repository_budget::HostBudget::default();
    budget.node().map_err(anyhow::Error::msg)?;
    if outer.is_empty() {
        return Ok(Vec::new());
    }
    let nested = sequence_ref(outer[0]).is_some();
    let mut clauses = Vec::new();
    let mut unrestricted = false;
    let mut clause = |items: &[Value<'_>]| -> anyhow::Result<Vec<ProviderRequirement>> {
        budget.node().map_err(anyhow::Error::msg)?;
        let mut seen = std::collections::BTreeSet::new();
        let mut requirements = Vec::new();
        for &item in items {
            budget.node().map_err(anyhow::Error::msg)?;
            // requirement_info bounds its transient owned result before cloning;
            // this cumulative budget charges it before either retained insertion.
            let requirement = crate::providers::requirement_info(item)
                .map_err(|error| anyhow!("providers: {error}"))?;
            budget.string(&requirement.id).map_err(anyhow::Error::msg)?;
            budget
                .string(&requirement.display_name)
                .map_err(anyhow::Error::msg)?;
            if seen.insert(requirement.id.clone()) {
                requirements.push(requirement);
            }
        }
        Ok(requirements)
    };
    if nested {
        for &item in outer {
            let items = sequence_ref(item).ok_or_else(|| {
                anyhow!("providers must not mix Provider constructors and sequences")
            })?;
            let requirements = clause(items)?;
            unrestricted |= requirements.is_empty();
            clauses.push(requirements);
        }
    } else {
        clauses.push(clause(outer)?);
    }
    if unrestricted {
        clauses.clear();
    }
    Ok(clauses)
}

fn with_required_providers(
    mut descriptor: Descriptor,
    value: Value<'_>,
) -> anyhow::Result<Descriptor> {
    descriptor.0.required_providers = required_providers(value)?;
    // Defaults and provider metadata must fit together, not just independently.
    crate::repository_budget::HostBudget::default()
        .spec(&descriptor.0)
        .map_err(anyhow::Error::msg)?;
    Ok(descriptor)
}

/// Attribute defaults use the innermost executing extension's lexical package.
/// A loaded helper can execute while another extension is being initialized,
/// so its canonical call-site filename takes precedence over the phase context.
/// This mirrors Bazel's LabelConverter.forBzlEvaluatingThread semantics.
fn definition_package(eval: &Evaluator<'_, '_, '_>) -> Option<crate::label::Label> {
    eval.extra?
        .downcast_ref::<crate::loading::ExtensionContext>()?;
    crate::loading::definition_label(eval)
}

type LabelResolver<'a> = dyn Fn(&str) -> anyhow::Result<crate::label::Label> + 'a;

/// LabelKeyedDictType.convert's collision report (packages/BuildType.java:449-490):
/// `duplicate labels in WHAT: L (as [k1, k2]), ...` with each label's
/// canonical form and the repr of the original keys, in first-seen order.
/// None when no keys collide (or a key does not resolve; parsing reports it).
fn value_label_duplicates(
    value: Value<'_>,
    resolver: Option<&LabelResolver<'_>>,
    what: &str,
) -> Option<String> {
    let dict = DictRef::from_value(value)?;
    let mut originals: Vec<(String, Vec<String>)> = Vec::new();
    for (key, _) in dict.iter() {
        let label = label_value(key, resolver).ok()?;
        match originals.iter_mut().find(|(seen, _)| *seen == label) {
            Some((_, keys)) => keys.push(key.to_repr()),
            None => originals.push((label, vec![key.to_repr()])),
        }
    }
    let duplicates = originals
        .iter()
        .filter(|(_, keys)| keys.len() > 1)
        .map(|(label, keys)| {
            let label = crate::label::Label::parse_canonical(label)
                .map(|label| crate::rule_declarations::canonical_form(&label))
                .unwrap_or_else(|_| label.clone());
            format!(" {label} (as [{}])", keys.join(", "))
        })
        .collect::<Vec<_>>();
    (!duplicates.is_empty()).then(|| format!("duplicate labels in {what}:{}", duplicates.join(",")))
}

fn label_value(value: Value<'_>, resolver: Option<&LabelResolver<'_>>) -> anyhow::Result<String> {
    if let Some(label) = crate::starlark_types::unpack_label(value) {
        return Ok(label);
    }
    let text = value
        .unpack_str()
        .ok_or_else(|| anyhow!("expected a string or Label"))?;
    if let Some(resolve) = resolver {
        return resolve(text).map(|label| label.to_string());
    }
    if !text.starts_with("@@")
        && !text.starts_with("//")
        && !text.starts_with("@//")
        && !text.starts_with("@@//")
    {
        bail!("relative label default requires an extension definition context");
    }
    crate::label::Label::parse_canonical(text.strip_prefix("@//").map_or(text, |_| &text[1..]))
        .map(|label| label.to_string())
        .map_err(anyhow::Error::msg)
}

impl AttrKind {
    /// The `attr` module function name (Descriptor repr `<attr.NAME>`).
    pub(crate) fn starlark_name(&self) -> &'static str {
        match self {
            Self::String => "string",
            Self::StringList => "string_list",
            Self::StringListDict => "string_list_dict",
            Self::StringDict => "string_dict",
            Self::Int => "int",
            Self::Bool => "bool",
            Self::Label => "label",
            Self::LabelList => "label_list",
            Self::Output => "output",
            Self::OutputList => "output_list",
            Self::IntList => "int_list",
            Self::LabelKeyedStringDict => "label_keyed_string_dict",
            Self::StringKeyedLabelDict => "string_keyed_label_dict",
            Self::LabelListDict => "label_list_dict",
        }
    }

    /// Bazel `Type.toString` (packages/Type.java, BuildType.java).
    pub(crate) fn type_name(&self) -> &'static str {
        match self {
            Self::String => "string",
            Self::StringList => "list(string)",
            Self::StringListDict => "dict(string, list(string))",
            Self::StringDict => "dict(string, string)",
            Self::Int => "int",
            Self::Bool => "boolean",
            Self::Label => "label",
            Self::LabelList => "list(label)",
            Self::Output => "output",
            Self::OutputList => "list(output)",
            Self::IntList => "list(int)",
            Self::LabelKeyedStringDict => "dict(label, string)",
            Self::StringKeyedLabelDict => "dict(string, label)",
            Self::LabelListDict => "dict(string, list(label))",
        }
    }

    /// Bazel `Type.convert` type failure in ConversionException form
    /// (packages/Type.java:296-321): `expected value of type 'T' for WHAT, but
    /// got REPR (TYPE)`; list elements use `element I of WHAT` (Type.java:689)
    /// and dict entries `dict key element` / `dict value element` (:576-577).
    /// Label-typed values that are not strings report type 'string'
    /// (BuildType.LabelType.convert). Shape only; label syntax is checked later.
    pub(crate) fn conversion_error(&self, value: Value<'_>, what: &str) -> Option<String> {
        fn expected(ty: &str, value: Value<'_>, what: &str) -> String {
            format!(
                "expected value of type '{ty}' for {what}, but got {} ({})",
                value.to_repr(),
                value.get_type()
            )
        }
        let string = |value: Value<'_>, what: &str| {
            value
                .unpack_str()
                .is_none()
                .then(|| expected("string", value, what))
        };
        let label = |value: Value<'_>, what: &str| {
            (value.unpack_str().is_none() && crate::starlark_types::label_parts(value).is_none())
                .then(|| expected("string", value, what))
        };
        let int = |value: Value<'_>, what: &str| {
            value
                .unpack_i32()
                .is_none()
                .then(|| expected("int", value, what))
        };
        let list = |value: Value<'_>, element: &dyn Fn(Value<'_>, &str) -> Option<String>| {
            let Some(items) = sequence_ref(value) else {
                return Some(expected(self.type_name(), value, what));
            };
            items
                .iter()
                .enumerate()
                .find_map(|(index, item)| element(*item, &format!("element {index} of {what}")))
        };
        let dict = |key: &dyn Fn(Value<'_>, &str) -> Option<String>,
                    item: &dyn Fn(Value<'_>, &str) -> Option<String>| {
            let Some(dict) = DictRef::from_value(value) else {
                return Some(expected(self.type_name(), value, what));
            };
            dict.iter().find_map(|(k, v)| {
                key(k, "dict key element").or_else(|| item(v, "dict value element"))
            })
        };
        match self {
            Self::String => string(value, what),
            Self::Int => int(value, what),
            // Type.BOOLEAN.convert's own expectation (packages/Type.java:409-428).
            Self::Bool => attr_bool(value).is_none().then(|| {
                format!(
                    "{} for {what}, but got {} ({})",
                    crate::repository_rule::BOOL_EXPECTED,
                    value.to_repr(),
                    value.get_type()
                )
            }),
            Self::Label => label(value, what),
            Self::Output => string(value, what),
            Self::StringList | Self::OutputList => list(value, &string),
            Self::LabelList => list(value, &label),
            Self::IntList => list(value, &int),
            Self::StringDict => dict(&string, &string),
            Self::LabelKeyedStringDict => dict(&label, &string),
            Self::StringKeyedLabelDict => dict(&string, &label),
            Self::LabelListDict => dict(&string, &|value, what| {
                let Some(items) = sequence_ref(value) else {
                    return Some(expected("list(label)", value, what));
                };
                items
                    .iter()
                    .enumerate()
                    .find_map(|(index, item)| label(*item, &format!("element {index} of {what}")))
            }),
            Self::StringListDict => dict(&string, &|value, what| {
                let Some(items) = sequence_ref(value) else {
                    return Some(expected("list(string)", value, what));
                };
                items
                    .iter()
                    .enumerate()
                    .find_map(|(index, item)| string(*item, &format!("element {index} of {what}")))
            }),
        }
    }

    /// Inspect borrowed defaults before parse copies shared Starlark payloads.
    /// Validate the complete type shape first, preserving ordinary type errors.
    /// This raw-copy phase and lexical normalization each have a cumulative
    /// per-descriptor budget; neither relies on later schema-wrapper checks.
    fn preflight_default(&self, value: Value<'_>) -> anyhow::Result<()> {
        self.preflight_value_with_budget(
            value,
            &mut crate::repository_budget::HostBudget::default(),
        )
    }

    pub(crate) fn preflight_value_with_budget(
        &self,
        value: Value<'_>,
        budget: &mut crate::repository_budget::HostBudget,
    ) -> anyhow::Result<()> {
        let is_label = |value: Value<'_>| {
            value.unpack_str().is_some() || crate::starlark_types::label_parts(value).is_some()
        };
        if matches!(self, Self::Label) && crate::language_modules::is_late_bound_default(value) {
            bail!(
                "late-bound label defaults (configuration_field) are {} (analysis-time configuration API)",
                crate::language_modules::NOT_IMPLEMENTED.trim_start_matches("is ")
            );
        }
        let expected = match self {
            Self::String if value.unpack_str().is_none() => Some("a string"),
            Self::StringList | Self::OutputList
                if !sequence_ref(value)
                    .is_some_and(|list| list.iter().all(|v| v.unpack_str().is_some())) =>
            {
                Some("a list of strings")
            }
            Self::StringListDict
                if !DictRef::from_value(value).is_some_and(|dict| {
                    dict.iter().all(|(key, values)| {
                        key.unpack_str().is_some()
                            && sequence_ref(values).is_some_and(|values| {
                                values.iter().all(|value| value.unpack_str().is_some())
                            })
                    })
                }) =>
            {
                Some("a dict of strings to a list of strings (or tuple of strings)")
            }
            Self::StringDict
                if !DictRef::from_value(value).is_some_and(|dict| {
                    dict.iter().all(|(key, value)| {
                        key.unpack_str().is_some() && value.unpack_str().is_some()
                    })
                }) =>
            {
                Some("a dict of strings to strings")
            }
            Self::Int if value.unpack_i32().is_none() => Some("a 32-bit int"),
            Self::IntList
                if !sequence_ref(value)
                    .is_some_and(|list| list.iter().all(|v| v.unpack_i32().is_some())) =>
            {
                Some("a list of 32-bit ints")
            }
            Self::LabelKeyedStringDict
                if !DictRef::from_value(value).is_some_and(|dict| {
                    dict.iter()
                        .all(|(key, value)| is_label(key) && value.unpack_str().is_some())
                }) =>
            {
                Some("a dict of strings or Labels to strings")
            }
            Self::StringKeyedLabelDict
                if !DictRef::from_value(value).is_some_and(|dict| {
                    dict.iter()
                        .all(|(key, value)| key.unpack_str().is_some() && is_label(value))
                }) =>
            {
                Some("a dict of strings to strings or Labels")
            }
            Self::LabelListDict
                if !DictRef::from_value(value).is_some_and(|dict| {
                    dict.iter().all(|(key, values)| {
                        key.unpack_str().is_some()
                            && sequence_ref(values)
                                .is_some_and(|values| values.iter().copied().all(is_label))
                    })
                }) =>
            {
                Some("a dict of strings to lists of strings or Labels")
            }
            Self::Bool if attr_bool(value).is_none() => Some("a bool"),
            Self::Label if !value.is_none() && !is_label(value) => Some("a string or Label"),
            Self::LabelList
                if !sequence_ref(value).is_some_and(|list| list.iter().copied().all(is_label)) =>
            {
                Some("a list of strings or Labels")
            }
            Self::Output if !value.is_none() && value.unpack_str().is_none() => Some("a string"),
            _ => None,
        };
        if let Some(expected) = expected {
            bail!("expected {expected}");
        }
        let mut charge = |value: Value<'_>| -> Result<(), String> {
            budget.node()?;
            if let Some(text) = value.unpack_str() {
                let bytes = if matches!(
                    self,
                    Self::Label
                        | Self::LabelList
                        | Self::LabelKeyedStringDict
                        | Self::StringKeyedLabelDict
                        | Self::LabelListDict
                ) {
                    // Covers canonical parsing/formatting even without a resolver.
                    crate::label::Label::resolution_size_bound(text, "", "", &BTreeMap::new())
                } else {
                    text.len()
                };
                budget.bytes(bytes)?;
            } else if let Some((repository, package, name)) =
                crate::starlark_types::label_parts(value)
            {
                budget.bytes(
                    repository
                        .len()
                        .saturating_add(package.len())
                        .saturating_add(name.len())
                        .saturating_add(5),
                )?;
            }
            Ok(())
        };
        charge(value).map_err(anyhow::Error::msg)?;
        if let Some(list) = sequence_ref(value) {
            for &value in list {
                charge(value).map_err(anyhow::Error::msg)?;
            }
        }
        if let Some(dict) = DictRef::from_value(value) {
            for (key, values) in dict.iter() {
                charge(key).map_err(anyhow::Error::msg)?;
                charge(values).map_err(anyhow::Error::msg)?;
                // The complete shape was checked above, before any budget errors.
                if matches!(self, Self::StringListDict | Self::LabelListDict) {
                    for &value in sequence_ref(values).expect("validated attribute sequence") {
                        charge(value).map_err(anyhow::Error::msg)?;
                    }
                }
            }
        }
        Ok(())
    }

    fn empty(&self) -> AttrValue {
        match self {
            Self::String => AttrValue::String(String::new()),
            Self::StringList => AttrValue::Strings(Vec::new()),
            Self::StringListDict => AttrValue::StringListDict(Vec::new()),
            Self::StringDict => AttrValue::StringDict(Vec::new()),
            Self::Int => AttrValue::Int(0),
            Self::Bool => AttrValue::Bool(false),
            Self::Label => AttrValue::Label(None),
            Self::LabelList => AttrValue::Labels(Vec::new()),
            Self::Output => AttrValue::Output(None),
            Self::OutputList => AttrValue::Outputs(Vec::new()),
            Self::IntList => AttrValue::Ints(Vec::new()),
            Self::LabelKeyedStringDict => AttrValue::LabelKeyedStringDict(Vec::new()),
            Self::StringKeyedLabelDict => AttrValue::StringKeyedLabelDict(Vec::new()),
            Self::LabelListDict => AttrValue::LabelListDict(Vec::new()),
        }
    }
    fn parse(
        &self,
        value: Value<'_>,
        resolver: Option<&LabelResolver<'_>>,
    ) -> anyhow::Result<AttrValue> {
        let string = || {
            value
                .unpack_str()
                .map(str::to_owned)
                .ok_or_else(|| anyhow!("expected a string"))
        };
        Ok(match self {
            Self::String => AttrValue::String(string()?),
            Self::StringList => AttrValue::Strings(string_list(value)?),
            Self::StringDict => AttrValue::StringDict(
                DictRef::from_value(value)
                    .ok_or_else(|| anyhow!("expected a dict of strings to strings"))?
                    .iter()
                    .map(|(key, value)| {
                        let key = key
                            .unpack_str()
                            .ok_or_else(|| anyhow!("expected dict keys to be strings"))?;
                        let value = value
                            .unpack_str()
                            .ok_or_else(|| anyhow!("expected dict values to be strings"))?;
                        Ok((key.to_owned(), value.to_owned()))
                    })
                    .collect::<anyhow::Result<_>>()?,
            ),
            Self::StringListDict => AttrValue::StringListDict(
                DictRef::from_value(value)
                    .ok_or_else(|| anyhow!("expected a dict of strings to a list of strings"))?
                    .iter()
                    .map(|(key, values)| {
                        let key = key
                            .unpack_str()
                            .ok_or_else(|| anyhow!("expected dict keys to be strings"))?;
                        Ok((key.to_owned(), string_list(values)?))
                    })
                    .collect::<anyhow::Result<_>>()?,
            ),
            Self::Int => AttrValue::Int(
                value
                    .unpack_i32()
                    .ok_or_else(|| anyhow!("expected a 32-bit int"))?,
            ),
            // Type.BOOLEAN also converts the integers 0 and 1 (Type.java:409-428).
            Self::Bool => {
                AttrValue::Bool(attr_bool(value).ok_or_else(|| anyhow!("expected a bool"))?)
            }
            Self::Label => AttrValue::Label(if value.is_none() {
                None
            } else {
                Some(label_value(value, resolver)?)
            }),
            Self::LabelList => AttrValue::Labels(
                sequence_ref(value)
                    .ok_or_else(|| anyhow!("expected a list of strings or Labels"))?
                    .iter()
                    .copied()
                    .map(|value| {
                        label_value(value, resolver)
                            .map_err(|e| anyhow!("expected a list of strings or Labels: {e}"))
                    })
                    .collect::<anyhow::Result<_>>()?,
            ),
            Self::Output => AttrValue::Output(if value.is_none() {
                None
            } else {
                Some(string()?)
            }),
            Self::OutputList => AttrValue::Outputs(string_list(value)?),
            Self::IntList => AttrValue::Ints(
                sequence_ref(value)
                    .ok_or_else(|| anyhow!("expected a list of 32-bit ints"))?
                    .iter()
                    .map(|value| {
                        value
                            .unpack_i32()
                            .ok_or_else(|| anyhow!("expected a list of 32-bit ints"))
                    })
                    .collect::<anyhow::Result<_>>()?,
            ),
            Self::LabelKeyedStringDict => {
                let mut entries: Vec<(String, String)> = Vec::new();
                for (key, value) in DictRef::from_value(value)
                    .ok_or_else(|| anyhow!("expected a dict of strings or Labels to strings"))?
                    .iter()
                {
                    let key = label_value(key, resolver)?;
                    let value = value
                        .unpack_str()
                        .ok_or_else(|| anyhow!("expected dict values to be strings"))?;
                    // BuildType.LabelKeyedDictType rejects keys that resolve equally.
                    if entries.iter().any(|(seen, _)| *seen == key) {
                        bail!("duplicate key {key} in label-keyed dict");
                    }
                    entries.push((key, value.to_owned()));
                }
                AttrValue::LabelKeyedStringDict(entries)
            }
            // DictType.convert (packages/Type.java:565-580): string keys,
            // each value converted by the label (list) type.
            Self::StringKeyedLabelDict => AttrValue::StringKeyedLabelDict(
                DictRef::from_value(value)
                    .ok_or_else(|| anyhow!("expected a dict of strings to strings or Labels"))?
                    .iter()
                    .map(|(key, value)| {
                        let key = key
                            .unpack_str()
                            .ok_or_else(|| anyhow!("expected dict keys to be strings"))?;
                        Ok((key.to_owned(), label_value(value, resolver)?))
                    })
                    .collect::<anyhow::Result<_>>()?,
            ),
            Self::LabelListDict => AttrValue::LabelListDict(
                DictRef::from_value(value)
                    .ok_or_else(|| {
                        anyhow!("expected a dict of strings to lists of strings or Labels")
                    })?
                    .iter()
                    .map(|(key, values)| {
                        let key = key
                            .unpack_str()
                            .ok_or_else(|| anyhow!("expected dict keys to be strings"))?;
                        let labels = sequence_ref(values)
                            .ok_or_else(|| anyhow!("expected a list of strings or Labels"))?
                            .iter()
                            .map(|value| label_value(*value, resolver))
                            .collect::<anyhow::Result<Vec<_>>>()?;
                        Ok((key.to_owned(), labels))
                    })
                    .collect::<anyhow::Result<_>>()?,
            ),
        })
    }

    /// `Type.getDefaultValue()` of the attribute type (packages/Type.java,
    /// packages/BuildType.java): the value of a `None` select branch inside
    /// its Selector (BuildType.java:722-725). Null label/output defaults are
    /// `Label(None)`/`Output(None)`.
    pub(crate) fn type_default(&self) -> AttrValue {
        match self {
            Self::String => AttrValue::String(String::new()),
            Self::StringList => AttrValue::Strings(Vec::new()),
            Self::StringListDict => AttrValue::StringListDict(Vec::new()),
            Self::StringDict => AttrValue::StringDict(Vec::new()),
            Self::Int => AttrValue::Int(0),
            Self::Bool => AttrValue::Bool(false),
            Self::Label => AttrValue::Label(None),
            Self::LabelList => AttrValue::Labels(Vec::new()),
            Self::Output => AttrValue::Output(None),
            Self::OutputList => AttrValue::Outputs(Vec::new()),
            Self::IntList => AttrValue::Ints(Vec::new()),
            Self::LabelKeyedStringDict => AttrValue::LabelKeyedStringDict(Vec::new()),
            Self::StringKeyedLabelDict => AttrValue::StringKeyedLabelDict(Vec::new()),
            Self::LabelListDict => AttrValue::LabelListDict(Vec::new()),
        }
    }

    pub(crate) fn is_empty_value(value: &AttrValue) -> bool {
        match value {
            AttrValue::String(_) | AttrValue::Int(_) | AttrValue::Bool(_) => false,
            AttrValue::Strings(values) | AttrValue::Labels(values) | AttrValue::Outputs(values) => {
                values.is_empty()
            }
            AttrValue::StringListDict(entries) | AttrValue::LabelListDict(entries) => {
                entries.is_empty()
            }
            AttrValue::StringDict(entries)
            | AttrValue::LabelKeyedStringDict(entries)
            | AttrValue::StringKeyedLabelDict(entries) => entries.is_empty(),
            AttrValue::Label(value) | AttrValue::Output(value) => value.is_none(),
            AttrValue::Ints(values) => values.is_empty(),
        }
    }
}

// Derives generate marker/coercion/trace implementations. Only implementation
// and the retained callbacks contain heap references; the ignored schema and
// declaration consist solely of owned data.
#[allow(unsafe_code)]
mod values {
    use super::*;
    use starlark::values::{
        Coerce, Freeze, NoSerialize, ProvidesStaticType, StarlarkPagablePanic, StarlarkValue,
        Trace, starlark_value,
    };
    use starlark::{starlark_complex_value, starlark_simple_value};
    #[derive(Clone, Debug, ProvidesStaticType, NoSerialize, Allocative, StarlarkPagablePanic)]
    pub(super) struct Descriptor(pub AttrSpec);
    impl fmt::Display for Descriptor {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            // Descriptor.repr (StarlarkAttrModule.java:1218): <attr.KIND>.
            write!(f, "<attr.{}>", self.0.kind.starlark_name())
        }
    }
    starlark_simple_value!(Descriptor);
    // StarlarkAttrModuleApi.Descriptor: @StarlarkBuiltin(name = "Attribute").
    #[starlark_value(type = "Attribute")]
    impl<'v> StarlarkValue<'v> for Descriptor {}

    /// An `Attribute` that retains Starlark values: a `default` that is a
    /// Starlark function (StarlarkComputedDefaultTemplate,
    /// packages/Attribute.java:1370-1513) and/or a `cfg` that is a Starlark
    /// `transition()` (StarlarkAttributeTransitionProvider,
    /// analysis/starlark/StarlarkAttributeTransitionProvider.java:52-75):
    /// the same owned schema plus the traced values (`None` when absent).
    #[derive(
        Debug,
        ProvidesStaticType,
        NoSerialize,
        Allocative,
        Trace,
        Freeze,
        Coerce,
        StarlarkPagablePanic,
    )]
    #[repr(C)]
    pub(super) struct ComputedDescriptorGen<V> {
        pub function: V,
        pub transition: V,
        #[trace(unsafe_ignore)]
        #[freeze(identity)]
        pub spec: AttrSpec,
    }
    impl<V> fmt::Display for ComputedDescriptorGen<V> {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            write!(f, "<attr.{}>", self.spec.kind.starlark_name())
        }
    }
    starlark_complex_value!(pub(super) ComputedDescriptor);
    #[starlark_value(type = "Attribute")]
    impl<'v, V: ValueLike<'v>> StarlarkValue<'v> for ComputedDescriptorGen<V> where
        Self: ProvidesStaticType<'v>
    {
    }

    #[derive(
        Debug,
        ProvidesStaticType,
        NoSerialize,
        Allocative,
        Trace,
        Freeze,
        Coerce,
        StarlarkPagablePanic,
    )]
    #[repr(C)]
    pub(super) struct DefinitionGen<V> {
        pub implementation: V,
        pub subrules: Vec<V>,
        /// `rule(initializer=...)`: zero or one Starlark function.
        pub initializer: Vec<V>,
        /// `rule(outputs=<function>)`: zero or one retained, never-called function.
        pub outputs_function: Vec<V>,
        /// Computed-default functions, parallel to `computed`.
        pub computed_functions: Vec<V>,
        /// Starlark `transition()` objects of the rule (`cfg`, keyed "") and
        /// of its attributes (keyed by attribute name), parallel to
        /// `transition_keys` (crate::starlark_transitions).
        pub transitions: Vec<V>,
        #[trace(unsafe_ignore)]
        #[freeze(identity)]
        pub transition_keys: Vec<String>,
        /// Computed-default attributes (Starlark name) with the function's
        /// parameter names (StarlarkCallbackHelper.getParameterNames), in
        /// rule-class attribute order.
        #[trace(unsafe_ignore)]
        #[freeze(identity)]
        pub computed: Vec<(String, Vec<String>)>,
        #[trace(unsafe_ignore)]
        #[freeze(identity)]
        pub build_setting: Option<BuildSetting>,
        #[trace(unsafe_ignore)]
        #[freeze(identity)]
        pub schema: BTreeMap<String, AttrSpec>,
        /// Schema keys in rule-class attribute order.
        #[trace(unsafe_ignore)]
        #[freeze(identity)]
        pub attr_order: Vec<String>,
        #[trace(unsafe_ignore)]
        #[freeze(identity)]
        pub declaration: RuleDeclaration,
        #[trace(unsafe_ignore)]
        #[freeze(identity)]
        pub exported: OnceLock<String>,
    }
    impl<V> fmt::Display for DefinitionGen<V> {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            // StarlarkRuleFunction.repr (StarlarkRuleClassFunctions.java:1999-2005).
            match self.exported.get() {
                Some(name) => write!(f, "<rule {name}>"),
                None => f.write_str("<rule>"),
            }
        }
    }
    starlark_complex_value!(pub(super) Definition);
    #[starlark_value(type = "rule")]
    impl<'v, V: ValueLike<'v>> StarlarkValue<'v> for DefinitionGen<V>
    where
        Self: ProvidesStaticType<'v>,
    {
        fn export_as(&self, name: &str, _eval: &mut Evaluator<'v, '_, '_>) -> starlark::Result<()> {
            if self.exported.get().is_none() {
                // StarlarkRuleFunction.export (StarlarkRuleClassFunctions.java:1931-1937),
                // TargetUtils.isTestRuleName (packages/TargetUtils.java:67).
                if self.declaration.test != name.ends_with("_test") {
                    return Err(starlark::Error::new_other(anyhow!(
                        "Invalid rule class name '{name}', test rule class names must end with '_test' and other rule classes must not"
                    )));
                }
                // RuleClass constructor (packages/RuleClass.java:1905-1918): a
                // Starlark `_x` whose native name differs from a built-in one
                // with the same public name (`$action_listener` versus
                // `:action_listener`) fails when the class is built.
                let builtins = crate::native_rules::starlark_builtins(self.declaration.kind());
                for (attribute, spec) in &self.schema {
                    if !attribute.starts_with('_') {
                        continue;
                    }
                    let native = native_attribute_name(attribute, spec.declaration.late_bound);
                    if let Some(conflicting) = builtins.public_name_conflict(&native) {
                        return Err(starlark::Error::new_other(anyhow!(
                            "Rule {name}: Attributes {native} and {conflicting} have an identical public name: {attribute}"
                        )));
                    }
                }
                let _ = self.exported.set(name.to_owned());
            }
            Ok(())
        }

        fn invoke(
            &self,
            _me: Value<'v>,
            args: &Arguments<'v, '_>,
            eval: &mut Evaluator<'v, '_, '_>,
        ) -> starlark::Result<Value<'v>> {
            args.no_positional_args(eval.heap())?;
            let named = args.names_map()?;
            let (repositories, package) =
                crate::package::custom_label_context(eval).map_err(starlark::Error::new_other)?;
            // One budget covers the retained schema and every supplied/default value.
            // Charge the schema before register_custom clones it, including defaults
            // that will be overridden by explicit BUILD arguments.
            let budget = std::cell::RefCell::new(crate::repository_budget::HostBudget::default());
            budget
                .borrow_mut()
                .schema(&self.schema)
                .map_err(|e| starlark::Error::new_other(anyhow!(e)))?;
            crate::subrule::preflight(
                self.subrules.iter().map(|value| value.to_value()),
                &mut budget.borrow_mut(),
            )
            .map_err(starlark::Error::new_other)?;
            let resolve_in = |text: &str, repository: &str, package: &str| {
                let bytes = repositories
                    .resolution_size_bound(text, repository, package)
                    .map_err(anyhow::Error::msg)?;
                budget
                    .borrow_mut()
                    .bytes(bytes)
                    .map_err(anyhow::Error::msg)?;
                repositories
                    .parse(text, repository, package)
                    .map_err(anyhow::Error::msg)
            };
            // Bazel converts an apparent repository the package's repository
            // (main, module or extension-generated) does not see into
            // `@@[unknown repo 'R' requested from @@C]` (RepositoryMapping.get,
            // cmdline/RepositoryName.java); the target loads and only its
            // dependency edge fails at analysis.
            let resolve = |text: &str| {
                resolve_in(text, &package.repository, &package.package).or_else(|error| {
                    if error.to_string().starts_with("unknown apparent repository") {
                        return repositories
                            .parse_deferred(text, &package.repository, &package.package)
                            .map_err(anyhow::Error::msg);
                    }
                    Err(error)
                })
            };
            let definition =
                crate::label::Label::parse_canonical(&self.declaration.definition_label)
                    .map_err(|e| starlark::Error::new_other(anyhow!(e)))?;
            // Initializer results are reified relative to the rule definition
            // (StarlarkRuleClassFunctions.java:1871-1880), except outputs.
            let resolve_definition =
                |text: &str| resolve_in(text, &definition.repository, &definition.package);
            let mut arguments = named
                .iter()
                .map(|(key, value)| (key.as_str().to_owned(), *value, Origin::Call))
                .collect::<Vec<_>>();
            if let Some(initializer) = self.initializer.first() {
                arguments = run_initializer(
                    initializer.to_value(),
                    arguments,
                    &self.schema,
                    &self.declaration,
                    &definition,
                    &resolve,
                    eval,
                )
                .map_err(starlark::Error::new_other)?;
            }
            let mut name = None;
            let mut visibility = None;
            let mut attrs = BTreeMap::new();
            let mut configurable = BTreeMap::new();
            let rule_class = self
                .exported
                .get()
                .cloned()
                .unwrap_or_else(|| "unexported rule".to_owned());
            // Package-loading errors are prefixed by the target label
            // (RuleClass.populateRuleAttributeValues reportError).
            let target_text = arguments
                .iter()
                .find(|(key, _, _)| key == "name")
                .and_then(|(_, value, _)| value.unpack_str())
                .map(|name| {
                    crate::rule_declarations::canonical_form(&crate::label::Label {
                        repository: package.repository.clone(),
                        package: package.package.clone(),
                        name: name.to_owned(),
                    })
                })
                .unwrap_or_default();
            let kind = self.declaration.kind();
            let builtins = crate::native_rules::starlark_builtins(kind);
            let mut builtin = crate::native_rules::NativeTarget {
                rule_class: rule_class.clone(),
                ..Default::default()
            };
            let mut converter = crate::native_rules::BuiltinConverter::new(&repositories, &package);
            for (key, value, origin) in &arguments {
                let key = key.as_str();
                match key {
                    "name" => {
                        name = Some(value.unpack_str().ok_or_else(|| {
                            starlark::Error::new_other(anyhow!("name must be a string"))
                        })?)
                    }
                    "visibility" => {
                        // BuildType.convertFromBuildLangType (BuildType.java:234-238).
                        if crate::rule_declarations::select_pieces(*value).is_some() {
                            return Err(starlark::Error::new_other(anyhow!(
                                "{target_text}: attribute \"visibility\" is not configurable"
                            )));
                        }
                        visibility = Some(*value)
                    }
                    _ => {
                        // Explicit keyword arguments name native attributes, so a
                        // Starlark `_x` (native `$x`) is not settable by BUILD
                        // (AttributeProvider.java:220-235); initializers convert
                        // `_x` to `$x` (StarlarkRuleClassFunctions.java:1855).
                        let spec = match self.schema.get(key) {
                            Some(spec)
                                if !key.starts_with('_') || *origin == Origin::Initializer =>
                            {
                                spec
                            }
                            _ => {
                                // applicable_licenses is an alternate spelling of
                                // package_metadata (AttributeProvider.java:213-217).
                                let native = if key == "applicable_licenses" {
                                    "package_metadata"
                                } else {
                                    key
                                };
                                // The renamed key is the one reported
                                // (AttributeProvider.java:213-235).
                                let Some(attribute) = builtins.attribute(native) else {
                                    return Err(starlark::Error::new_other(anyhow!(
                                        "{target_text}: no such attribute '{native}' in '{rule_class}' rule{}",
                                        unknown_attribute_hint(native, &builtins, &self.schema)
                                    )));
                                };
                                if value.is_none() {
                                    continue;
                                }
                                converter
                                    .convert_argument(&rule_class, &target_text, attribute, *value)
                                    .map_err(starlark::Error::new_other)?
                                    .store(native, &mut builtin);
                                continue;
                            }
                        };
                        // Bazel treats an explicit None like an omitted value:
                        // defaults and mandatory checks still apply.
                        if value.is_none() {
                            continue;
                        }
                        let resolver: &LabelResolver<'_> = if *origin == Origin::Initializer
                            && !matches!(spec.kind, AttrKind::Output | AttrKind::OutputList)
                        {
                            &resolve_definition
                        } else {
                            &resolve
                        };
                        if let Some(pieces) = crate::rule_declarations::select_pieces(*value) {
                            // build_setting_default and help are nonconfigurable
                            // (RuleClass.java:904-914).
                            let nonconfigurable = self.build_setting.is_some()
                                && matches!(key, "build_setting_default" | "help");
                            let parts = configurable_parts(
                                key,
                                spec,
                                nonconfigurable,
                                &rule_class,
                                pieces,
                                resolver,
                                &budget,
                            )
                            .map_err(|e| {
                                starlark::Error::new_other(anyhow!("{target_text}: {e}"))
                            })?;
                            let type_default = match spec.kind.type_default() {
                                AttrValue::Label(None) | AttrValue::Output(None) => None,
                                value => Some(value),
                            };
                            match crate::configured_attributes::simplify_unconditional(
                                parts,
                                type_default,
                            ) {
                                crate::configured_attributes::Simplified::Retained(parts) => {
                                    configurable.insert(key.to_owned(), parts);
                                }
                                crate::configured_attributes::Simplified::Plain(value) => {
                                    let value =
                                        order_independent_sorted(spec, value).map_err(|e| {
                                            starlark::Error::new_other(anyhow!(
                                                "{target_text}: {e}"
                                            ))
                                        })?;
                                    attrs.insert(key.to_owned(), value);
                                }
                                // Like an omitted value: default and mandatory checks apply.
                                crate::configured_attributes::Simplified::Unset => {}
                            }
                            continue;
                        }
                        // Every bool attribute, build_setting_default included, goes
                        // through Type.BOOLEAN.convert: True/False or 0/1, else
                        // `//pkg:x: expected one of [False, True, 0, 1] for attribute
                        // 'K' of 'R', but got V (T)` (Type.java:409-428;
                        // AttributeConversionContext.toString, BuildType.java:374-376).
                        // Label-keyed dictionaries: LabelKeyedDictType.convert's
                        // element types, then `duplicate labels in attribute 'K'
                        // of 'R': L (as [k1, k2])` (BuildType.java:449-490),
                        // reported as `//pkg:x: ...` (AttributeProvider.java:252-267).
                        if matches!(spec.kind, AttrKind::Bool | AttrKind::LabelKeyedStringDict) {
                            let what = format!("attribute '{key}' of '{rule_class}'");
                            if let Some(error) =
                                spec.kind.conversion_error(*value, &what).or_else(|| {
                                    (spec.kind == AttrKind::LabelKeyedStringDict)
                                        .then(|| {
                                            value_label_duplicates(*value, Some(resolver), &what)
                                        })
                                        .flatten()
                                })
                            {
                                return Err(starlark::Error::new_other(anyhow!(
                                    "{target_text}: {error}"
                                )));
                            }
                        }
                        let parsed = (|| {
                            budget
                                .borrow_mut()
                                .string(key)
                                .map_err(anyhow::Error::msg)?;
                            spec.kind
                                .preflight_value_with_budget(*value, &mut budget.borrow_mut())?;
                            spec.kind.parse(*value, Some(resolver))
                        })()
                        .map_err(|e| {
                            starlark::Error::new_other(anyhow!("attribute {key:?}: {e}"))
                        })?;
                        let parsed = order_independent_sorted(spec, parsed).map_err(|e| {
                            starlark::Error::new_other(anyhow!("attribute {key:?}: {e}"))
                        })?;
                        attrs.insert(key.to_owned(), parsed);
                    }
                }
            }
            let name = name.ok_or_else(|| {
                starlark::Error::new_other(anyhow!("missing required attribute name"))
            })?;
            let target = crate::label::Label {
                repository: package.repository.clone(),
                package: package.package.clone(),
                name: name.to_owned(),
            };
            // Only explicitly supplied values (call or initializer) and select()
            // branches are checked; defaults never are (AttributeProvider.java:288-290).
            check_allowed_values(&target, &self.schema, &attrs, &configurable)
                .map_err(starlark::Error::new_other)?;
            // Before defaults fill in omitted attributes: whether a string
            // `scope` was explicitly specified (StarlarkOptionsParser.java:256-275).
            let explicit_scope = match (self.schema.get("scope"), attrs.get("scope")) {
                (Some(spec), _) if spec.kind != AttrKind::String => {
                    crate::starlark_options::ScopeAttribute::Unspecified
                }
                (_, Some(AttrValue::String(scope))) => {
                    crate::starlark_options::ScopeAttribute::Value(scope.clone())
                }
                _ if configurable.contains_key("scope") => {
                    crate::starlark_options::ScopeAttribute::Configurable
                }
                _ => crate::starlark_options::ScopeAttribute::Unspecified,
            };
            let explicit = attrs
                .keys()
                .chain(configurable.keys())
                .cloned()
                .collect::<std::collections::BTreeSet<_>>();
            for (key, spec) in &self.schema {
                // A select()-valued attribute is resolved at analysis
                // (crate::configured_attributes); its default is not a value.
                // Computed defaults are evaluated after every other attribute
                // (AttributeProvider.java:335-341, 400-435).
                if !attrs.contains_key(key)
                    && !configurable.contains_key(key)
                    && !spec.declaration.computed_default
                {
                    if spec.mandatory && crate::subrule::is_lifted_attribute(key) {
                        // AttributeProvider.populateDefaultRuleAttributeValues
                        // (packages/AttributeProvider.java:319-327): a lifted
                        // subrule attribute cannot be set by BUILD files.
                        return Err(starlark::Error::new_other(anyhow!(
                            "{target_text}: missing value for mandatory attribute '{}' in '{rule_class}' rule",
                            native_attribute_name(key, spec.declaration.late_bound)
                        )));
                    }
                    if spec.mandatory {
                        return Err(starlark::Error::new_other(anyhow!(
                            "missing mandatory attribute {key:?}"
                        )));
                    }
                    {
                        let mut budget = budget.borrow_mut();
                        budget
                            .string(key)
                            .and_then(|()| budget.attr_value(&spec.default))
                            .map_err(|e| starlark::Error::new_other(anyhow!(e)))?;
                    }
                    attrs.insert(key.clone(), spec.default.clone());
                }
            }
            if kind.test {
                check_test_size_and_timeout(name, &builtin).map_err(starlark::Error::new_other)?;
            }
            // RuleClass.checkForValidSizeAndTimeoutValues runs for every rule
            // with a STRING `size`/`timeout` attribute and reads it through
            // NonconfigurableAttributeMapper (packages/RuleClass.java:2138-2156,
            // NonconfigurableAttributeMapper.java:44-49), whose precondition
            // fails for a (configurable) Starlark attribute: Bazel 9.3 crashes.
            // Fail explicitly instead of guessing a result.
            for key in ["size", "timeout"] {
                if self
                    .schema
                    .get(key)
                    .is_some_and(|spec| spec.kind == AttrKind::String)
                {
                    return Err(starlark::Error::new_other(anyhow!(
                        "{target_text}: Starlark attribute '{key}' of type string is not supported by the native profile: Bazel 9.3 crashes validating it (RuleClass.checkForValidSizeAndTimeoutValues reads it with NonconfigurableAttributeMapper)"
                    )));
                }
            }
            builtin.tags = builtin.strings("tags");
            for ((key, parameters), function) in self.computed.iter().zip(&self.computed_functions)
            {
                // An initializer-supplied private value is not a default.
                if attrs.contains_key(key) || configurable.contains_key(key) {
                    continue;
                }
                let spec = &self.schema[key];
                let value = computed_default(
                    ComputedInputs {
                        attribute: key,
                        parameters,
                        target: &target,
                        attrs: &attrs,
                        configurable: &configurable,
                        schema: &self.schema,
                        builtin: &builtin,
                        builtins: &builtins,
                        kind,
                        visibility,
                        resolve: &resolve,
                    },
                    function.to_value(),
                    &spec.kind,
                    eval,
                )
                .map_err(starlark::Error::new_other)?;
                budget
                    .borrow_mut()
                    .attr_value(&value)
                    .map_err(|e| starlark::Error::new_other(anyhow!(e)))?;
                attrs.insert(key.clone(), value);
            }
            builtin.input_labels = crate::native_rules::input_labels(&builtin, |key| {
                builtins.attribute(key).is_some_and(|attribute| {
                    matches!(
                        attribute.ty,
                        crate::native_rules::NativeType::NodepLabel
                            | crate::native_rules::NativeType::NodepLabelList
                    )
                })
            });
            let implicit_outputs = implicit_outputs(
                ComputedInputs {
                    attribute: "",
                    parameters: &[],
                    target: &target,
                    attrs: &attrs,
                    configurable: &configurable,
                    schema: &self.schema,
                    builtin: &builtin,
                    builtins: &builtins,
                    kind,
                    visibility,
                    resolve: &resolve,
                },
                &self.declaration.implicit_outputs,
                self.outputs_function.first().map(|value| value.to_value()),
                eval,
            )
            .map_err(starlark::Error::new_other)?;
            let (transition_gaps, toolchain_gaps, unanalyzed) =
                static_gaps(&self.declaration, &self.schema);
            let configurable_labels = configurable_labels(&self.schema, &configurable);
            crate::package::register_custom(
                eval,
                name,
                visibility,
                self.implementation.to_value(),
                self.subrules.iter().map(|value| value.to_value()).collect(),
                self.transition_keys
                    .iter()
                    .cloned()
                    .zip(self.transitions.iter().map(|value| value.to_value()))
                    .collect(),
                attrs,
                self.schema.clone(),
                self.build_setting,
                InvocationDeclaration {
                    implicit_outputs,
                    unanalyzed,
                    transition_gaps,
                    toolchain_gaps,
                    toolchain_types: self.declaration.toolchains.clone(),
                    fragments: self.declaration.fragments.clone(),
                    exec_compatible_with: self.declaration.exec_compatible_with.clone(),
                    exec_groups: self.declaration.exec_groups.clone(),
                    definition_label: self.declaration.definition_label.clone(),
                    definition_environment_label: self
                        .declaration
                        .definition_environment_label
                        .clone(),
                    provides: self.declaration.provides.clone(),
                    configurable,
                    attr_order: self.attr_order.clone(),
                    configurable_labels,
                    rule_class,
                    explicit_scope,
                    builtin: BuiltinAttributes {
                        kind,
                        target: builtin,
                        ..Default::default()
                    },
                    explicit,
                    definition_stack: self.declaration.definition_stack.clone(),
                },
            )
            .map_err(starlark::Error::new_other)?;
            Ok(Value::new_none())
        }
    }
}
use values::{ComputedDescriptor, ComputedDescriptorGen, Definition, DefinitionGen, Descriptor};

/// Owned `rule()` metadata (StarlarkRuleClassFunctions.createRule, :717-1171).
#[derive(Clone, Debug, Default, PartialEq, Eq, Allocative)]
pub(crate) struct RuleDeclaration {
    /// Canonical `.bzl` label whose evaluation called rule().
    pub definition_label: String,
    /// The rule class's definition environment label: the `.bzl` being
    /// INITIALIZED when rule() ran (BzlInitThreadContext.getBzlFile, the
    /// outermost frame; StarlarkRuleClassFunctions.java:626-631, 1056-1062),
    /// which differs from [`Self::definition_label`] (the innermost frame)
    /// when a helper in another `.bzl` calls rule(). The owner location of
    /// implicit dependencies (CommonPrerequisiteValidator.java:133-143) and
    /// the DEFINITION location of the function-transition allowlist check
    /// (StarlarkRuleClassFunctions.createRule, :714-722).
    pub definition_environment_label: String,
    pub executable: bool,
    /// `test=True` or `analysis_test=True` (createRule: "analysis_test=true implies test=true").
    pub test: bool,
    pub analysis_test: bool,
    pub skylark_testable: bool,
    pub output_to_genfiles: bool,
    pub dependency_resolution_rule: bool,
    pub fragments: Vec<String>,
    pub host_fragments: Vec<String>,
    pub toolchains: Vec<(String, bool)>,
    /// `rule(exec_groups = ...)` in declaration order (docs/EXEC_GROUPS.md).
    pub exec_groups: Vec<crate::rule_declarations::DeclaredExecGroup>,
    pub exec_compatible_with: Vec<String>,
    pub cfg: crate::rule_declarations::Cfg,
    pub provides: Vec<ProviderRequirement>,
    pub implicit_outputs: ImplicitOutputs,
    /// `extendable=` as given: "True"/"False" or a canonical allowlist label.
    pub extendable: Option<String>,
    /// RuleClass.getCallStack: the stack of the rule() call
    /// (StarlarkRuleClassFunctions.java:916-918), for `query --output=build`.
    pub definition_stack: Vec<(String, String)>,
}

/// `rule(outputs=...)` (StarlarkRuleClassFunctions.java:1102-1122).
#[derive(Clone, Debug, Default, PartialEq, Eq, Allocative)]
pub(crate) enum ImplicitOutputs {
    #[default]
    None,
    Templates(Vec<(String, String)>),
    Function,
}

impl RuleDeclaration {
    /// The Starlark base rule class this rule extends.
    pub(crate) fn kind(&self) -> crate::native_rules::StarlarkRuleKind {
        crate::native_rules::StarlarkRuleKind {
            test: self.test,
            executable: self.executable,
            dependency_resolution: self.dependency_resolution_rule,
        }
    }

    /// Rule-level features any analysis of a target of this rule exercises.
    /// Rule-level gaps by the analysis phase that would exercise them:
    /// (rule transition, toolchain/execution platform resolution, other).
    fn phased_gaps(&self) -> (Vec<String>, Vec<String>, Vec<String>) {
        let mut transition = Vec::new();
        let toolchain = Vec::new();
        let mut features = Vec::new();
        if self.analysis_test {
            features.push("rule(analysis_test=True)".to_owned());
        }
        // rule(test=True)/rule(executable=True) are analyzed natively: the
        // executable, DefaultInfo runfiles and FilesToRunProvider
        // (crate::runfiles, docs/RUNFILES.md). The implicit test-runner
        // attributes need the exec transition and are not analyzed (N23g).
        if self.skylark_testable {
            features.push("rule(_skylark_testable=True)".to_owned());
        }
        if self.output_to_genfiles {
            features.push("rule(output_to_genfiles=True)".to_owned());
        }
        if self.dependency_resolution_rule {
            features.push("rule(dependency_resolution_rule=True)".to_owned());
        }
        // rule(toolchains=...), rule(exec_compatible_with=...) and
        // rule(exec_groups=...) are resolved natively (crate::toolchains,
        // docs/TOOLCHAINS.md, docs/EXEC_GROUPS.md). A Starlark rule
        // transition is modelled (crate::starlark_transitions); config.none()
        // is not.
        if !self.cfg.is_target()
            && !matches!(self.cfg, crate::rule_declarations::Cfg::Starlark { .. })
        {
            transition.push(format!(
                "rule(cfg={}) (configuration transition)",
                self.cfg.describe()
            ));
        }
        // rule(outputs=...) implicit outputs are ctx.outputs entries since N46
        // (docs/IMPLICIT_OUTPUTS.md).
        (transition, toolchain, features)
    }
}

/// Attribute.getAllowedFileTypesPredicate of a Starlark dependency attribute
/// (packages/Attribute.java:2175): StarlarkAttrModule.setAllowedFileTypes
/// (analysis/starlark/StarlarkAttrModule.java:127-140, 431-445) makes
/// `True` FileTypeSet.ANY_FILE, `False`/unset NO_FILE, and a list
/// `allowedFileTypes(FileType.of(list))`: a one-FileType set
/// (Attribute.java:866-868, FileTypeSet.java:89-95), never NO_FILE, even when
/// the list is empty.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum AllowedFileTypes<'a> {
    None,
    Any,
    Extensions(&'a [String]),
}

impl AllowedFileTypes<'_> {
    /// FileTypeSet.matches with FileType.hasAnyExtension
    /// (util/FileTypeSet.java:118-126, util/FileType.java:109-133): a plain
    /// case-sensitive suffix test off Windows, so entries need no leading dot
    /// and may be exact names or multi-dot suffixes (`.tar.gz`); `""` matches
    /// every name.
    pub(crate) fn matches(&self, name: &str) -> bool {
        match self {
            Self::None => false,
            Self::Any => true,
            Self::Extensions(types) => types.iter().any(|suffix| name.ends_with(suffix.as_str())),
        }
    }
}

/// FileTypeSet.toString (util/FileTypeSet.java:63-67, 47-51, 143-146):
/// `no files`, `any files`, or StringUtil.joinEnglishList of the extensions
/// in declaration order (util/StringUtil.java:35-73: `a`, `a or b`,
/// `a, b, or c`; `nothing` when empty).
impl std::fmt::Display for AllowedFileTypes<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let types = match self {
            Self::None => return f.write_str("no files"),
            Self::Any => return f.write_str("any files"),
            Self::Extensions(types) => types,
        };
        // joinEnglishList tests the buffer, not the index, for separators.
        let mut joined = String::new();
        for (index, choice) in types.iter().enumerate() {
            if !joined.is_empty() {
                let last = index + 1 == types.len();
                if !last || index >= 2 {
                    joined.push(',');
                }
                if last {
                    joined.push_str(" or");
                }
                joined.push(' ');
            }
            joined.push_str(choice);
        }
        f.write_str(if joined.is_empty() {
            "nothing"
        } else {
            &joined
        })
    }
}

impl AttrSpec {
    /// The attribute's allowed file types (AllowedFileTypes).
    pub(crate) fn allowed_file_types(&self) -> AllowedFileTypes<'_> {
        match &self.declaration.file_types {
            Some(types) => AllowedFileTypes::Extensions(types),
            None if self.allow_files || self.allow_single_file => AllowedFileTypes::Any,
            None => AllowedFileTypes::None,
        }
    }

    /// RuleContext.Builder.validateDirectPrerequisiteFileTypes
    /// (analysis/RuleContext.java:1945-1994) after its single-artifact check:
    /// unless NO_FILE (or ANY_FILE on a list that may be empty), one of the
    /// prerequisite's files to build must match, by file name: `'L' does not
    /// produce any RULE ATTR files (expected TYPES)`, with Bazel's native
    /// attribute name. There are no tree artifacts natively.
    pub(crate) fn produced_file_type_error<'f>(
        &self,
        name: &str,
        rule_class: &str,
        label: &str,
        mut file_names: impl Iterator<Item = &'f str>,
    ) -> Option<String> {
        let allowed = self.allowed_file_types();
        let checked = match allowed {
            AllowedFileTypes::None => false,
            AllowedFileTypes::Any => self.non_empty || self.allow_single_file,
            AllowedFileTypes::Extensions(_) => true,
        };
        if !checked || file_names.any(|file| allowed.matches(file)) {
            return None;
        }
        Some(format!(
            "'{label}' does not produce any {rule_class} {} files (expected {allowed})",
            native_attribute_name(name, self.declaration.late_bound)
        ))
    }
}

/// Argument provenance during rule instantiation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Origin {
    Call,
    Initializer,
}

/// Native name of a Starlark attribute (AttributeValueSource.convertToNativeName,
/// packages/AttributeValueSource.java:62-70): `_x` becomes `$x`, or `:x` for
/// late-bound (`configuration_field`) defaults.
pub(crate) fn native_attribute_name(name: &str, late_bound: bool) -> String {
    match name.strip_prefix('_') {
        Some(rest) if late_bound => format!(":{rest}"),
        Some(rest) => format!("${rest}"),
        None => name.to_owned(),
    }
}

/// `//pkg:x: no such attribute 'K' in 'R' rule` suffix: SpellChecker over the
/// documented attributes (AttributeProvider.java:220-235). Private (`$x`/`:x`)
/// attributes, built-in or Starlark `_x`, are UNDOCUMENTED (Attribute.java:444-449)
/// and never suggested. Built-in attributes come first in Bazel's order;
/// Starlark attributes follow in name order (the retained schema does not keep
/// the `attrs` dict order, which only matters for equally distant candidates).
fn unknown_attribute_hint(
    key: &str,
    builtins: &crate::native_rules::StarlarkBuiltins,
    schema: &BTreeMap<String, AttrSpec>,
) -> String {
    let candidates = builtins
        .candidates()
        .into_iter()
        .chain(
            schema
                .keys()
                .map(String::as_str)
                .filter(|name| !name.starts_with('_')),
        )
        .collect::<Vec<_>>();
    crate::native_rules::did_you_mean(key, &candidates)
}

/// RuleClass.checkForValidSizeAndTimeoutValues (packages/RuleClass.java:2138-2156)
/// with the `size` default `medium` and BaseRuleClasses.TIMEOUT_DEFAULT
/// (BaseRuleClasses.java:84-98), TestSize.getTestSize (TestSize.java:96-106)
/// and TestTimeout.getTestTimeout (TestTimeout.java:122-133).
fn check_test_size_and_timeout(
    name: &str,
    builtin: &crate::native_rules::NativeTarget,
) -> anyhow::Result<()> {
    let size = builtin.string("size").unwrap_or("medium");
    let valid_size = matches!(size, "small" | "medium" | "large" | "enormous");
    if !valid_size {
        bail!("In rule '{name}', size '{size}' is not a valid size.");
    }
    let timeout = builtin
        .string("timeout")
        .map(str::to_owned)
        .unwrap_or_else(|| default_test_timeout(size).to_owned());
    if timeout != timeout.to_lowercase()
        || !matches!(
            timeout.to_uppercase().as_str(),
            "SHORT" | "MODERATE" | "LONG" | "ETERNAL"
        )
    {
        bail!("In rule '{name}', timeout '{timeout}' is not a valid timeout.");
    }
    Ok(())
}

/// BaseRuleClasses.TIMEOUT_DEFAULT (BaseRuleClasses.java:84-98) over
/// TestSize's default timeouts (TestSize.java:30-36).
pub(crate) fn default_test_timeout(size: &str) -> &'static str {
    match size {
        "small" => "short",
        "medium" => "moderate",
        "large" => "long",
        "enormous" => "eternal",
        _ => "illegal",
    }
}

/// The built-in `toolchains` attribute of Starlark rules
/// (StarlarkRuleClassFunctions.java:165-168, as BaseRuleClasses.MakeVariableExpandingRule,
/// analysis/BaseRuleClasses.java:352-364): a label list in the target
/// configuration, no files, mandatory TemplateVariableInfo.
pub(crate) fn toolchains_attribute_spec() -> AttrSpec {
    AttrSpec {
        kind: AttrKind::LabelList,
        default: AttrValue::Labels(Vec::new()),
        mandatory: false,
        allow_files: false,
        allow_single_file: false,
        non_empty: false,
        required_providers: vec![vec![crate::providers::ProviderRequirement {
            id: crate::template_variable_info::PROVIDER_ID.to_owned(),
            display_name: "TemplateVariableInfo".to_owned(),
        }]],
        declaration: AttrDeclaration::default(),
    }
}

/// Non-empty built-in attribute values whose Bazel analysis semantics the
/// native profile does not implement (docs/COMMON_RULE_ATTRIBUTES.md); a
/// target exercising one fails analysis explicitly.
fn builtin_gaps(builtin: &crate::native_rules::NativeTarget) -> Vec<String> {
    let mut gaps = Vec::new();
    for (key, value) in &builtin.attrs {
        // An empty expect_failure has no effect
        // (StarlarkRuleConfiguredTargetUtil.java:80-131 checks isEmpty()).
        if AttrKind::is_empty_value(value)
            || matches!(value, AttrValue::String(text) if text.is_empty())
        {
            continue;
        }
        let reason = match key.as_str() {
            "target_compatible_with" => "incompatible target skipping (constraint analysis)",
            "compatible_with" | "restricted_to" => "constraint environments",
            // `exec_compatible_with` and `exec_group_compatible_with` filter
            // the execution platforms of the native toolchain resolution
            // (crate::engine, docs/TOOLCHAINS.md, docs/EXEC_GROUPS.md).
            "aspect_hints" => "aspect hints",
            "transitive_configs" => "configuration trimming",
            "expect_failure" => "expected analysis failure",
            _ => continue,
        };
        gaps.push(format!(
            "attribute '{key}' = {} ({reason})",
            crate::native_rules::describe(value)
        ));
    }
    gaps
}

/// ALLOWLIST_RULE_EXTENSION_API_EXPERIMENTAL (StarlarkRuleClassFunctions.java:249-253)
/// with BuiltinRestriction.AllowlistEntry.allows (BuiltinRestriction.java:124-150).
fn private_initializer_allowed(definition: &crate::label::Label) -> bool {
    if definition.repository.is_empty() {
        [
            "initializer_testing/builtins",
            "third_party/bazel_rules/rules_cc",
        ]
        .iter()
        .any(|prefix| {
            definition.package == *prefix || definition.package.starts_with(&format!("{prefix}/"))
        })
    } else {
        definition.repository.starts_with("rules_cc+") || definition.repository == "_builtins"
    }
}

/// Lift a BUILD value like BuildType.copyAndLiftStarlarkValue: label strings
/// become Label objects resolved in the target's package. A select()'s
/// string keys become Labels resolved in the target's package too
/// (BuildType.copyAndLiftSelectorList, packages/BuildType.java:264-301), so
/// an initializer's returned select is not re-resolved in the rule
/// definition's repository (StarlarkRuleClassFunctions.java:1810-1880);
/// `//conditions:default` stays the default-condition key.
fn lift<'v>(
    kind: &AttrKind,
    value: Value<'v>,
    resolve: &LabelResolver<'_>,
    heap: starlark::values::Heap<'v>,
) -> anyhow::Result<Value<'v>> {
    let condition = |key: Value<'v>| -> anyhow::Result<Value<'v>> {
        match key.unpack_str() {
            Some(text) if text != "//conditions:default" => {
                crate::starlark_types::alloc_label(heap, &resolve(text)?.to_string())
            }
            _ => Ok(key),
        }
    };
    let branch = |branch: Value<'v>| lift(kind, branch, resolve, heap);
    if let Some(lifted) = crate::rule_declarations::lift_select(value, heap, &condition, &branch) {
        return lifted;
    }
    let label = |value: Value<'v>| -> anyhow::Result<Value<'v>> {
        if crate::starlark_types::label_parts(value).is_some() {
            return Ok(value);
        }
        let text = value
            .unpack_str()
            .ok_or_else(|| anyhow!("expected a string or Label"))?;
        crate::starlark_types::alloc_label(heap, &resolve(text)?.to_string())
    };
    Ok(match kind {
        AttrKind::Label => label(value)?,
        AttrKind::LabelList => heap.alloc(
            sequence_ref(value)
                .ok_or_else(|| anyhow!("expected a list of strings or Labels"))?
                .iter()
                .map(|value| label(*value))
                .collect::<anyhow::Result<Vec<_>>>()?,
        ),
        AttrKind::LabelKeyedStringDict => {
            let dict = DictRef::from_value(value)
                .ok_or_else(|| anyhow!("expected a dict of strings or Labels to strings"))?;
            let mut entries = Vec::new();
            for (key, value) in dict.iter() {
                entries.push((label(key)?, value));
            }
            heap.alloc(starlark::values::dict::AllocDict(entries))
        }
        AttrKind::StringKeyedLabelDict => {
            let dict = DictRef::from_value(value)
                .ok_or_else(|| anyhow!("expected a dict of strings to strings or Labels"))?;
            let mut entries = Vec::new();
            for (key, value) in dict.iter() {
                entries.push((key, label(value)?));
            }
            heap.alloc(starlark::values::dict::AllocDict(entries))
        }
        AttrKind::LabelListDict => {
            let dict = DictRef::from_value(value).ok_or_else(|| {
                anyhow!("expected a dict of strings to lists of strings or Labels")
            })?;
            let mut entries = Vec::new();
            for (key, values) in dict.iter() {
                let labels = sequence_ref(values)
                    .ok_or_else(|| anyhow!("expected a list of strings or Labels"))?
                    .iter()
                    .map(|value| label(*value))
                    .collect::<anyhow::Result<Vec<_>>>()?;
                entries.push((key, heap.alloc(labels)));
            }
            heap.alloc(starlark::values::dict::AllocDict(entries))
        }
        _ => value,
    })
}

/// StarlarkRuleFunction.call initializer loop (StarlarkRuleClassFunctions.java:1810-1885).
fn run_initializer<'v>(
    initializer: Value<'v>,
    mut arguments: Vec<(String, Value<'v>, Origin)>,
    schema: &BTreeMap<String, AttrSpec>,
    declaration: &RuleDeclaration,
    definition: &crate::label::Label,
    resolve: &LabelResolver<'_>,
    eval: &mut Evaluator<'v, '_, '_>,
) -> anyhow::Result<Vec<(String, Value<'v>, Origin)>> {
    let heap = eval.heap();
    let mut budget = crate::repository_budget::HostBudget::default();
    let mut kwargs = Vec::new();
    for (key, value, _) in &arguments {
        budget.node().map_err(anyhow::Error::msg)?;
        if value.is_none() {
            continue;
        }
        if key == "name" {
            kwargs.push((key.clone(), *value));
        } else if let Some(spec) = schema.get(key)
            && !key.starts_with('_')
        {
            let lifted = lift(&spec.kind, *value, resolve, heap)
                .map_err(|e| anyhow!("attribute {key:?}: {e}"))?;
            kwargs.push((key.clone(), lifted));
        }
    }
    let named = kwargs
        .iter()
        .map(|(key, value)| (key.as_str(), *value))
        .collect::<Vec<_>>();
    let returned = eval
        .eval_function(initializer, &[], &named)
        .map_err(|e| anyhow!(e.to_string()))?;
    if returned.is_none() {
        return Ok(arguments);
    }
    let dict = DictRef::from_value(returned).ok_or_else(|| {
        anyhow!(
            "got {} for 'rule's initializer return value', want dict",
            returned.get_type()
        )
    })?;
    for (key, value) in dict.iter() {
        if key.unpack_str().is_none() {
            bail!(
                "got dict<{}, {}> for 'rule's initializer return value', want dict<string, object>",
                key.get_type(),
                value.get_type()
            );
        }
    }
    let original_name = arguments
        .iter()
        .find(|(key, _, _)| key == "name")
        .map(|(_, value, _)| *value);
    for (key, value) in dict.iter() {
        budget.node().map_err(anyhow::Error::msg)?;
        let key = key.unpack_str().expect("checked initializer key");
        if key == "name" {
            if !original_name
                .map(|name| name.equals(value))
                .transpose()
                .map_err(|e| anyhow!(e.to_string()))?
                .unwrap_or(false)
            {
                bail!("Initializer can't change the name of the target");
            }
            continue;
        }
        if !crate::rule_declarations::is_identifier(key) {
            bail!("attribute name `{key}` is not a valid identifier.");
        }
        if key.starts_with('_') && !private_initializer_allowed(definition) {
            bail!(
                "file '{}' cannot use private API",
                crate::rule_declarations::canonical_form(definition)
            );
        }
        if !schema.contains_key(key)
            && crate::native_rules::starlark_builtins(declaration.kind())
                .contains(&native_attribute_name(key, false))
        {
            bail!("Initializer can only set Starlark defined attributes, not '{key}'");
        }
        budget.string(key).map_err(anyhow::Error::msg)?;
        if let Some(entry) = arguments.iter_mut().find(|(seen, _, _)| seen == key) {
            entry.1 = value;
            entry.2 = Origin::Initializer;
        } else {
            arguments.push((key.to_owned(), value, Origin::Initializer));
        }
    }
    Ok(arguments)
}

/// Coerce a `select()` attribute value (BuildType.selectableConvert,
/// packages/BuildType.java:220-330); every branch is type-checked at loading.
fn configurable_parts(
    key: &str,
    spec: &AttrSpec,
    nonconfigurable: bool,
    rule_class: &str,
    pieces: Vec<crate::rule_declarations::SelectPiece<'_>>,
    resolve: &LabelResolver<'_>,
    budget: &std::cell::RefCell<crate::repository_budget::HostBudget>,
) -> anyhow::Result<Vec<ConfigurablePart>> {
    use crate::rule_declarations::SelectPiece;
    // BuildType.convertFromBuildLangType (packages/BuildType.java:234-238).
    if nonconfigurable || matches!(spec.kind, AttrKind::Output | AttrKind::OutputList) {
        bail!("attribute \"{key}\" is not configurable");
    }
    // BuildType.selectableConvert (packages/BuildType.java:177-180): types
    // whose Type.concat is null reject `select() + ...`.
    if pieces.len() > 1 && matches!(spec.kind, AttrKind::Bool | AttrKind::Label) {
        bail!(
            "type '{}' doesn't support select concatenation",
            spec.kind.type_name()
        );
    }
    let parse = |value, what: &str| -> anyhow::Result<AttrValue> {
        if let Some(error) = spec.kind.conversion_error(value, what) {
            bail!("{error}");
        }
        if spec.kind == AttrKind::LabelKeyedStringDict
            && let Some(error) = value_label_duplicates(value, Some(resolve), what)
        {
            bail!("{error}");
        }
        spec.kind
            .preflight_value_with_budget(value, &mut budget.borrow_mut())?;
        let parsed = spec.kind.parse(value, Some(resolve))?;
        budget
            .borrow_mut()
            .attr_value(&parsed)
            .map_err(anyhow::Error::msg)?;
        Ok(parsed)
    };
    // AttributeConversionContext.toString (BuildType.java:374-376).
    let attribute = format!("attribute '{key}' of '{rule_class}'");
    let mut parts = Vec::new();
    for piece in pieces {
        budget.borrow_mut().node().map_err(anyhow::Error::msg)?;
        parts.push(match piece {
            SelectPiece::Plain(value) => ConfigurablePart::Plain(parse(value, &attribute)?),
            SelectPiece::Select {
                branches,
                no_match_error,
            } => {
                let mut owned = Vec::new();
                for (condition, value) in branches {
                    let condition =
                        if let Some(label) = crate::starlark_types::unpack_label(condition) {
                            label
                        } else {
                            let text = condition.unpack_str().expect("validated select key");
                            // BuildType.Selector.DEFAULT_CONDITION_KEY.
                            if text == "//conditions:default" {
                                text.to_owned()
                            } else {
                                resolve(text)?.to_string()
                            }
                        };
                    let shown = crate::label::Label::parse_canonical(&condition)
                        .map(|label| crate::rule_declarations::canonical_form(&label))
                        .unwrap_or_else(|_| condition.clone());
                    // SelectBranchMessage (BuildType.java:654-668).
                    let what = format!(
                        "each branch in select expression of {attribute} (including '{shown}')"
                    );
                    let value = if value.is_none() {
                        None
                    } else {
                        Some(parse(value, &what)?)
                    };
                    owned.push((condition, value));
                }
                ConfigurablePart::Select {
                    branches: owned,
                    no_match_error,
                }
            }
        });
    }
    Ok(parts)
}

/// AttributeProvider.checkAllowedValues (packages/AttributeProvider.java:478-497),
/// over plain values and every select() branch.
fn check_allowed_values(
    target: &crate::label::Label,
    schema: &BTreeMap<String, AttrSpec>,
    attrs: &BTreeMap<String, AttrValue>,
    configurable: &BTreeMap<String, Vec<ConfigurablePart>>,
) -> anyhow::Result<()> {
    for (name, spec) in schema {
        if spec.declaration.values.is_empty() {
            continue;
        }
        let mut values = Vec::new();
        if let Some(parts) = configurable.get(name) {
            for part in parts {
                match part {
                    ConfigurablePart::Plain(value) => values.push(value),
                    ConfigurablePart::Select { branches, .. } => {
                        values.extend(
                            branches
                                .iter()
                                .map(|(_, value)| value.as_ref().unwrap_or(&spec.default)),
                        );
                    }
                }
            }
        } else if let Some(value) = attrs.get(name) {
            values.push(value);
        }
        for value in values {
            if !spec.declaration.allows(value) {
                bail!(
                    "{}: invalid value in '{name}' attribute: {}",
                    crate::rule_declarations::canonical_form(target),
                    spec.declaration.allowed_values_reason(value)
                );
            }
        }
    }
    Ok(())
}

/// The rule instance a computed default reads (AttributeMap of
/// StarlarkComputedDefaultTemplate.computeValue, packages/Attribute.java:1457-1472).
struct ComputedInputs<'a, 'v> {
    attribute: &'a str,
    parameters: &'a [String],
    target: &'a crate::label::Label,
    attrs: &'a BTreeMap<String, AttrValue>,
    configurable: &'a BTreeMap<String, Vec<ConfigurablePart>>,
    schema: &'a BTreeMap<String, AttrSpec>,
    builtin: &'a crate::native_rules::NativeTarget,
    builtins: &'a crate::native_rules::StarlarkBuiltins,
    kind: crate::native_rules::StarlarkRuleKind,
    /// The explicit BUILD `visibility` value, if any.
    visibility: Option<Value<'v>>,
    resolve: &'a LabelResolver<'a>,
}

/// `Attribute.valueToStarlark` (packages/Attribute.java:2442-2490) of a plain
/// value; `None` for a null (unset) label or output.
///
/// Output values are Labels in the target's package (BuildType.OUTPUT holds a
/// Label, packages/BuildType.java:540-560), so `package` is the target.
pub(crate) fn attr_value_to_starlark<'v>(
    heap: starlark::values::Heap<'v>,
    value: &AttrValue,
    package: &crate::label::Label,
) -> anyhow::Result<Option<Value<'v>>> {
    let label = |text: &str| crate::starlark_types::alloc_label(heap, text);
    let output = |name: &str| -> anyhow::Result<Value<'v>> {
        let label = package.with_name(name).map_err(anyhow::Error::msg)?;
        crate::starlark_types::alloc_label(heap, &label.to_string())
    };
    let labels = |values: &[String]| -> anyhow::Result<Value<'v>> {
        Ok(heap.alloc(
            values
                .iter()
                .map(|text| label(text))
                .collect::<anyhow::Result<Vec<_>>>()?,
        ))
    };
    Ok(Some(match value {
        AttrValue::String(text) => heap.alloc(text.as_str()),
        AttrValue::Strings(values) => heap.alloc(values.clone()),
        AttrValue::Outputs(values) => heap.alloc(
            values
                .iter()
                .map(|name| output(name))
                .collect::<anyhow::Result<Vec<_>>>()?,
        ),
        AttrValue::Int(number) => heap.alloc(*number),
        AttrValue::Ints(numbers) => heap.alloc(numbers.clone()),
        AttrValue::Bool(flag) => heap.alloc(*flag),
        AttrValue::Label(None) | AttrValue::Output(None) => return Ok(None),
        AttrValue::Label(Some(text)) => label(text)?,
        AttrValue::Output(Some(text)) => output(text)?,
        AttrValue::Labels(values) => labels(values)?,
        AttrValue::StringDict(entries) => heap.alloc(starlark::values::dict::AllocDict(
            entries
                .iter()
                .map(|(key, value)| (key.as_str(), value.as_str())),
        )),
        AttrValue::StringListDict(entries) => heap.alloc(starlark::values::dict::AllocDict(
            entries
                .iter()
                .map(|(key, values)| (key.as_str(), heap.alloc(values.clone()))),
        )),
        AttrValue::LabelKeyedStringDict(entries) => {
            let mut items = Vec::new();
            for (key, value) in entries {
                items.push((label(key)?, heap.alloc(value.as_str())));
            }
            heap.alloc(starlark::values::dict::AllocDict(items))
        }
        AttrValue::StringKeyedLabelDict(entries) => {
            let mut items = Vec::new();
            for (key, value) in entries {
                items.push((key.as_str(), label(value)?));
            }
            heap.alloc(starlark::values::dict::AllocDict(items))
        }
        AttrValue::LabelListDict(entries) => {
            let mut items = Vec::new();
            for (key, values) in entries {
                items.push((key.as_str(), labels(values)?));
            }
            heap.alloc(starlark::values::dict::AllocDict(items))
        }
    }))
}

/// StarlarkComputedDefaultTemplate.computePossibleValues/computeValue/invokeCallback
/// (packages/Attribute.java:1405-1500) for one rule instance. Each parameter
/// names an attribute (StarlarkCallbackHelper.buildArgumentList,
/// StarlarkCallbackHelper.java:83-99); unknown names fail
/// (AttributeNotFoundException), as do attributes without a regular value
/// (null values and other computed defaults). Parameters reading `select()`
/// values would need Bazel's per-combination lookup table and fail explicitly.
fn computed_default<'v>(
    inputs: ComputedInputs<'_, 'v>,
    function: Value<'v>,
    kind: &AttrKind,
    eval: &mut Evaluator<'v, '_, '_>,
) -> anyhow::Result<AttrValue> {
    let shown = crate::rule_declarations::canonical_form(inputs.target);
    let attribute = inputs.attribute;
    let error = |message: String| {
        anyhow!(
            "Cannot compute default value of attribute '{attribute}' in rule '{shown}': {message}"
        )
    };
    // Starlark names of the rule's attributes: native `$x` private names are
    // not addressable by a Starlark parameter (AttributeValueSource.java:62-70).
    let exists = |name: &str| {
        name == "name"
            || (!name.starts_with('_') && inputs.schema.contains_key(name))
            || inputs.builtins.attribute(name).is_some()
    };
    let mut dependencies = inputs.parameters.to_vec();
    dependencies.sort();
    for dependency in &dependencies {
        if !exists(dependency) {
            return Err(error(format!(
                "No such attribute {dependency} in rule {shown}"
            )));
        }
        if inputs.configurable.contains_key(dependency)
            || inputs.builtin.configurable.contains_key(dependency)
        {
            bail!(
                "{shown}: computed default of attribute '{attribute}' depends on the configurable attribute '{dependency}' (per-configuration lookup table, StarlarkComputedDefault), which is not supported by the native profile"
            );
        }
    }
    if inputs.kind.dependency_resolution {
        // Dependency attributes of these rules are made nonconfigurable
        // (StarlarkRuleClassFunctions.java:949-961), which changes the visible set.
        bail!(
            "{shown}: computed default of attribute '{attribute}' on a dependency_resolution_rule is not supported by the native profile"
        );
    }
    let heap = eval.heap();
    // Built-in computed defaults never have a regular value: testonly,
    // deprecation, package_metadata (BaseRuleClasses.java:67-108, 244, 265, 293)
    // and the test `timeout` (StarlarkRuleClassFunctions.java:272-276).
    let builtin_computed = |name: &str| {
        matches!(name, "testonly" | "deprecation" | "package_metadata")
            || (name == "timeout" && inputs.kind.test)
    };
    let mut values: Vec<Option<Value<'v>>> = Vec::new();
    for parameter in inputs.parameters {
        let value = if parameter == "name" {
            Some(heap.alloc(inputs.target.name.as_str()))
        } else if let Some(spec) = inputs
            .schema
            .get(parameter)
            .filter(|_| !parameter.starts_with('_'))
        {
            if spec.declaration.computed_default {
                None
            } else {
                match inputs.attrs.get(parameter) {
                    Some(value) => attr_value_to_starlark(heap, value, inputs.target)?,
                    None => None,
                }
            }
        } else if builtin_computed(parameter) {
            None
        } else if parameter == "tags" {
            Some(heap.alloc(inputs.builtin.tags.clone()))
        } else if parameter == "visibility" {
            // A nonconfigurable NODEP_LABEL_LIST: the declared labels.
            let Some(visibility) = inputs.visibility else {
                bail!(
                    "{shown}: computed default of attribute '{attribute}' reads the unset 'visibility' attribute (genuine Bazel 9.3 crashes on it), which is not supported by the native profile"
                );
            };
            let items = sequence_ref(visibility)
                .ok_or_else(|| anyhow!("visibility must be a list of labels"))?;
            let mut labels = Vec::new();
            for item in items {
                let label = match crate::starlark_types::unpack_label(*item) {
                    Some(label) => label,
                    None => (inputs.resolve)(
                        item.unpack_str()
                            .ok_or_else(|| anyhow!("visibility must be a list of labels"))?,
                    )?
                    .to_string(),
                };
                labels.push(crate::starlark_types::alloc_label(heap, &label)?);
            }
            Some(heap.alloc(labels))
        } else if let Some(value) = inputs.builtin.attrs.get(parameter.as_str()) {
            attr_value_to_starlark(heap, value, inputs.target)?
        } else if let Some(value) = builtin_default(parameter, heap) {
            Some(value)
        } else {
            bail!(
                "{shown}: computed default of attribute '{attribute}' reads the unset built-in attribute '{parameter}', whose default value is not modeled by the native profile"
            );
        };
        values.push(value);
    }
    // buildArgumentList fails at the first parameter without a value, with
    // StructImpl's message and every visible field
    // (StarlarkCallbackHelper.java:83-99, packages/StructImpl.java:86-95): the
    // rule's nonconfigurable attributes and the declared dependencies
    // (AggregatingAttributeMapper.createMapBackedAttributeMap, :533-566),
    // except computed defaults and null values (Attribute.java:1457-1472).
    if let Some(index) = values.iter().position(Option::is_none) {
        let mut visible = nonconfigurable_builtins(inputs.kind)
            .iter()
            .map(|name| (*name).to_owned())
            .collect::<std::collections::BTreeSet<_>>();
        for (parameter, value) in inputs.parameters.iter().zip(&values) {
            if value.is_some() {
                visible.insert(parameter.clone());
            }
        }
        return Err(error(format!(
            "No such regular (non computed) attribute '{}'.\nAvailable attributes: {}",
            inputs.parameters[index],
            visible.into_iter().collect::<Vec<_>>().join(", ")
        )));
    }
    let arguments = values.into_iter().flatten().collect::<Vec<_>>();
    // Bazel evaluates computed defaults in their own Starlark thread
    // (packages/StarlarkCallbackHelper.java:66-70), not within the rule call.
    let result = eval
        .with_new_recursion_scope(|eval| eval.eval_function(function, &arguments, &[]))
        .map_err(|e| error(evaluation_message(&e)))?;
    // invokeCallback: None is the type default; otherwise Type.cast (and the
    // element casts of list types).
    if result.is_none() {
        return Ok(kind.type_default());
    }
    let mismatch = |ty: &str, value: Value<'_>| {
        error(format!("expected '{ty}', but got '{}'", value.get_type()))
    };
    match kind {
        AttrKind::Label => crate::starlark_types::unpack_label(result)
            .map(|label| AttrValue::Label(Some(label)))
            .ok_or_else(|| mismatch("label", result)),
        AttrKind::LabelList => {
            let items = sequence_ref(result).ok_or_else(|| mismatch("list(label)", result))?;
            items
                .iter()
                .map(|item| {
                    crate::starlark_types::unpack_label(*item)
                        .ok_or_else(|| mismatch("label", *item))
                })
                .collect::<anyhow::Result<Vec<_>>>()
                .map(AttrValue::Labels)
        }
        other => bail!(
            "computed (function) defaults of attr.{} are not supported by the native profile",
            other.starlark_name()
        ),
    }
}

/// EvalException.getMessage of a Starlark failure: the message without the
/// call stack (`fail("boom")` is `boom`).
fn evaluation_message(error: &starlark::Error) -> String {
    match error.kind() {
        // `fail(*args, sep = " ")` joins after an empty `msg` prefix.
        starlark::ErrorKind::Fail(inner) => inner.to_string().trim_start().to_owned(),
        starlark::ErrorKind::Value(inner)
        | starlark::ErrorKind::Function(inner)
        | starlark::ErrorKind::Native(inner)
        | starlark::ErrorKind::Other(inner) => inner.to_string(),
        _ => error.without_diagnostic().to_string(),
    }
}

/// Nonconfigurable built-in attributes of Starlark rule classes with a
/// non-null value, by native name (StarlarkRuleClassFunctions.java:159-241,
/// 255-378; BaseRuleClasses.java:197-296): visible to every computed default.
fn nonconfigurable_builtins(kind: crate::native_rules::StarlarkRuleKind) -> Vec<&'static str> {
    let mut names = vec![
        "$config_dependencies",
        "compatible_with",
        "exec_compatible_with",
        "exec_group_compatible_with",
        "generator_function",
        "generator_location",
        "generator_name",
        "name",
        "restricted_to",
        "tags",
        "transitive_configs",
        "visibility",
    ];
    if kind.test {
        names.extend(["$is_executable", "flaky", "local", "size"]);
    } else if kind.executable {
        names.push("$is_executable");
    }
    names
}

/// Unset built-in attribute values that are a plain default
/// (StarlarkRuleClassFunctions.java:159-241, 255-378, BaseRuleClasses.java:197-296).
fn builtin_default<'v>(name: &str, heap: starlark::values::Heap<'v>) -> Option<Value<'v>> {
    Some(match name {
        "features"
        | "toolchains"
        | "target_compatible_with"
        | "aspect_hints"
        | "compatible_with"
        | "restricted_to"
        | "exec_compatible_with"
        | "transitive_configs"
        | "args"
        | "output_licenses" => heap.alloc(Vec::<Value<'v>>::new()),
        "exec_properties" | "exec_group_compatible_with" => {
            heap.alloc(starlark::values::dict::AllocDict::EMPTY)
        }
        "expect_failure" => heap.alloc(""),
        "shard_count" => heap.alloc(-1),
        "size" => heap.alloc("medium"),
        "flaky" | "local" => Value::new_bool(false),
        _ => return None,
    })
}

/// `Rule.populateOutputFilesInternal`'s implicit part (packages/Rule.java:516-575)
/// for `rule(outputs = ...)`: the dict form (StarlarkImplicitOutputsFunctionWithMap,
/// ImplicitOutputsFunction.java:133-161) or the function form
/// (StarlarkImplicitOutputsFunctionWithCallback, :77-130), each output label
/// checked (Rule.java:536-551), then the implicit-key/output-attribute
/// collision check (Rule.java:577-586). Keys and package-relative output
/// names in declaration (dict) order. docs/IMPLICIT_OUTPUTS.md.
fn implicit_outputs<'v>(
    inputs: ComputedInputs<'_, 'v>,
    outputs: &ImplicitOutputs,
    function: Option<Value<'v>>,
    eval: &mut Evaluator<'v, '_, '_>,
) -> anyhow::Result<Vec<(String, String)>> {
    let shown = crate::rule_declarations::canonical_form(inputs.target);
    let in_rule = |error: anyhow::Error| anyhow!("In rule {shown}: {error}");
    let lookup = |placeholder: &str| implicit_output_values(&inputs, placeholder);
    let expanded = match outputs {
        ImplicitOutputs::None => return Ok(Vec::new()),
        ImplicitOutputs::Templates(templates) => {
            let configurable = |name: &str| {
                inputs.configurable.contains_key(name)
                    || inputs.builtin.configurable.contains_key(name)
            };
            let mut expanded = Vec::new();
            for (key, template) in templates {
                let output = crate::implicit_outputs::expand_one(
                    key,
                    template,
                    Some(&configurable),
                    &lookup,
                )
                .map_err(in_rule)?;
                expanded.push((key.clone(), output));
            }
            expanded
        }
        ImplicitOutputs::Function => {
            let function = function
                .ok_or_else(|| anyhow!("internal error: rule(outputs=<function>) not retained"))?;
            let templates = implicit_outputs_callback(&inputs, function, eval).map_err(in_rule)?;
            let mut expanded = Vec::new();
            for (key, template) in templates {
                let output = crate::implicit_outputs::expand_one(&key, &template, None, &lookup)
                    .map_err(in_rule)?;
                expanded.push((key, output));
            }
            expanded
        }
    };
    for (_, output) in &expanded {
        crate::bazel_label::validate_and_process_target_name(&inputs.target.package, output)
            .map_err(|error| {
                anyhow!("illegal output file name '{output}' in rule {shown} due to: {error}")
            })?;
    }
    // Explicit outputs: an implicit key equal to the name of an output
    // attribute with a value (Rule.java:577-586); an unset one collides only
    // in ctx.outputs (crate::analysis).
    for (name, value) in inputs.attrs {
        let present = match value {
            AttrValue::Output(value) => value.is_some(),
            AttrValue::Outputs(values) => !values.is_empty(),
            _ => false,
        };
        if present && expanded.iter().any(|(key, _)| key == name) {
            bail!("Implicit output key '{name}' collides with output attribute name");
        }
    }
    Ok(expanded)
}

/// `ImplicitOutputsFunction.attributeValues` (ImplicitOutputsFunction.java:356-399):
/// `%{dirname}`/`%{basename}` of the rule name, then the attribute's type
/// (RawAttributeMapper over the internal names, so Starlark `_x` private
/// attributes are unknown): strings and string lists as is, label (list)
/// names without their extension, output (list) names; an unknown attribute
/// or an empty list is no value; other types fail. Values genuine Bazel 9.3
/// crashes on (an unset label, an unset `deprecation`) fail explicitly.
fn implicit_output_values(
    inputs: &ComputedInputs<'_, '_>,
    placeholder: &str,
) -> anyhow::Result<Vec<String>> {
    let rule_name = inputs.target.name.as_str();
    if let Some(value) = crate::implicit_outputs::name_part(rule_name, placeholder) {
        return Ok(vec![value]);
    }
    if placeholder == "name" {
        return Ok(vec![rule_name.to_owned()]);
    }
    let type_error = |ty: &str| {
        anyhow!(
            "For attribute '{placeholder}' in outputs: Attributes of type {ty} cannot be used in an outputs substitution template"
        )
    };
    let label_name = |text: &str| {
        let name = crate::label::Label::parse_canonical(text)
            .map(|label| label.name)
            .unwrap_or_else(|_| text.rsplit(':').next().unwrap_or(text).to_owned());
        crate::implicit_outputs::remove_extension(&name).to_owned()
    };
    let unset_label = || {
        anyhow!(
            "implicit outputs template placeholder %{{{placeholder}}} reads an unset label attribute: genuine Bazel 9.3 crashes on this (NullPointerException in ImplicitOutputsFunction.attributeValues, ImplicitOutputsFunction.java:380), which the native profile reports instead"
        )
    };
    let unmodeled = || {
        anyhow!(
            "implicit outputs template placeholder %{{{placeholder}}} reads the unset built-in attribute '{placeholder}', whose value is not modeled by the native profile (genuine Bazel 9.3 crashes on a null value)"
        )
    };
    if let Some(spec) = inputs
        .schema
        .get(placeholder)
        .filter(|_| !placeholder.starts_with('_'))
    {
        let value = inputs.attrs.get(placeholder);
        return Ok(match (&spec.kind, value) {
            (AttrKind::String, Some(AttrValue::String(value))) => vec![value.clone()],
            (AttrKind::StringList, Some(AttrValue::Strings(values))) => values.clone(),
            (AttrKind::Label, Some(AttrValue::Label(Some(label)))) => vec![label_name(label)],
            (AttrKind::Label, Some(AttrValue::Label(None))) => return Err(unset_label()),
            (AttrKind::LabelList, Some(AttrValue::Labels(labels))) => {
                labels.iter().map(|label| label_name(label)).collect()
            }
            (AttrKind::Output, Some(AttrValue::Output(Some(output)))) => vec![output.clone()],
            (AttrKind::Output, Some(AttrValue::Output(None))) => return Err(unset_label()),
            (AttrKind::OutputList, Some(AttrValue::Outputs(outputs))) => outputs.clone(),
            (
                AttrKind::String
                | AttrKind::StringList
                | AttrKind::Label
                | AttrKind::LabelList
                | AttrKind::Output
                | AttrKind::OutputList,
                _,
            ) => bail!(
                "implicit outputs template placeholder %{{{placeholder}}} reads a value of attribute '{placeholder}' the native profile does not have at loading (a select() or computed value)"
            ),
            (kind, _) => return Err(type_error(kind.type_name())),
        });
    }
    let Some(attribute) = inputs.builtins.attribute(placeholder) else {
        return Ok(Vec::new());
    };
    use crate::native_rules::NativeType;
    let value = inputs.builtin.attrs.get(placeholder);
    Ok(match (attribute.ty, value) {
        (NativeType::StringList, _) if placeholder == "tags" => inputs.builtin.tags.clone(),
        (NativeType::String, Some(AttrValue::String(value))) => vec![value.clone()],
        (NativeType::String, _) => return Err(unmodeled()),
        (NativeType::StringList, Some(AttrValue::Strings(values))) => values.clone(),
        (NativeType::StringList, None) => Vec::new(),
        (NativeType::Label, Some(AttrValue::Label(Some(label)))) => vec![label_name(label)],
        (NativeType::Label, _) => return Err(unset_label()),
        (NativeType::LabelList, Some(AttrValue::Labels(labels))) => {
            labels.iter().map(|label| label_name(label)).collect()
        }
        (NativeType::LabelList, None) => Vec::new(),
        (NativeType::OutputList, Some(AttrValue::Outputs(outputs))) => outputs.clone(),
        (NativeType::OutputList, None) => Vec::new(),
        (NativeType::StringList | NativeType::LabelList | NativeType::OutputList, Some(_)) => {
            return Err(unmodeled());
        }
        (ty, _) => return Err(type_error(ty.name())),
    })
}

/// StarlarkImplicitOutputsFunctionWithCallback.calculateOutputs
/// (ImplicitOutputsFunction.java:86-112): the function is called with one
/// argument per parameter name (StarlarkCallbackHelper.buildArgumentList,
/// StarlarkCallbackHelper.java:83-97) from a struct of every nonconfigurable
/// attribute by Starlark name (private `_x` included, values by
/// Attribute.valueToStarlark, unset labels None); the result is cast to
/// dict<string, string>.
fn implicit_outputs_callback<'v>(
    inputs: &ComputedInputs<'_, 'v>,
    function: Value<'v>,
    eval: &mut Evaluator<'v, '_, '_>,
) -> anyhow::Result<Vec<(String, String)>> {
    let heap = eval.heap();
    let mut arguments = Vec::new();
    for parameter in parameter_names(function)? {
        let missing = || {
            anyhow!(
                "Attribute '{parameter}' either doesn't exist or uses a select() (i.e. could have multiple values)"
            )
        };
        let value = if parameter == "name" {
            heap.alloc(inputs.target.name.as_str())
        } else if inputs.configurable.contains_key(&parameter)
            || inputs.builtin.configurable.contains_key(&parameter)
        {
            return Err(missing());
        } else if inputs.schema.contains_key(&parameter) {
            match inputs.attrs.get(&parameter) {
                Some(value) => attr_value_to_starlark(heap, value, inputs.target)?
                    .unwrap_or_else(Value::new_none),
                None => bail!(
                    "the implicit outputs function reads attribute '{parameter}', whose value the native profile does not have at loading (a computed value)"
                ),
            }
        } else if inputs.builtins.attribute(&parameter).is_some() {
            if parameter == "tags" {
                heap.alloc(inputs.builtin.tags.clone())
            } else if let Some(value) = inputs.builtin.attrs.get(parameter.as_str()) {
                attr_value_to_starlark(heap, value, inputs.target)?.unwrap_or_else(Value::new_none)
            } else {
                bail!(
                    "the implicit outputs function reads the unset built-in attribute '{parameter}', whose value is not modeled by the native profile"
                );
            }
        } else {
            return Err(missing());
        };
        arguments.push(value);
    }
    // Positional arguments only (buildArgumentList, StarlarkCallbackHelper.java:72-73),
    // without the package context.
    let result = crate::package::without_package_context(eval, |eval| {
        eval.eval_function(function, &arguments, &[])
    })
    .map_err(|error| match error.kind() {
        starlark::ErrorKind::Fail(_) => {
            anyhow!("Error in fail: {}", evaluation_message(&error))
        }
        _ => anyhow!("{}", evaluation_message(&error)),
    })?;
    const WHAT: &str = "implicit outputs function return value";
    let dict = DictRef::from_value(result)
        .ok_or_else(|| anyhow!("got {} for '{WHAT}', want dict", result.get_type()))?;
    let mut templates = Vec::new();
    for (key, value) in dict.iter() {
        let (Some(key), Some(value)) = (key.unpack_str(), value.unpack_str()) else {
            bail!(
                "got dict<{}, {}> for '{WHAT}', want dict<string, string>",
                key.get_type(),
                value.get_type()
            );
        };
        templates.push((key.to_owned(), value.to_owned()));
    }
    Ok(templates)
}

/// Declared-but-unanalyzed features any analysis of this target exercises,
/// independent of attribute values: rule-level declarations (toolchains,
/// exec groups, rule transitions, test/executable classes) and late-bound
/// defaults. Analysis fails on them before configuration conditions or
/// dependencies are visited (crate::engine).
fn static_gaps(
    declaration: &RuleDeclaration,
    schema: &BTreeMap<String, AttrSpec>,
) -> (Vec<String>, Vec<String>, Vec<String>) {
    let (transition, toolchain, mut features) = declaration.phased_gaps();
    for (name, spec) in schema {
        // Resolved late-bound values are checked like any other value
        // (value_gaps); unresolved ones fail before dependencies.
        if spec.declaration.late_bound && !spec.declaration.late_bound_resolvable() {
            features.extend(spec.declaration.unanalyzed(name));
        }
    }
    (transition, toolchain, features)
}

/// Declared-but-unanalyzed features exercised by the configured (resolved)
/// attribute values of a target: dependency edges with transitions, aspects
/// or file/rule restrictions, label-keyed dictionaries and built-in attribute
/// values (docs/CONFIGURATION.md). Never evaluated on unresolved selects.
pub(crate) fn value_gaps(invocation: &CustomInvocation) -> Vec<String> {
    let mut features = Vec::new();
    for (name, spec) in &invocation.schema {
        let present = invocation
            .attrs
            .get(name)
            .is_some_and(|value| !AttrKind::is_empty_value(value));
        if present && (!spec.declaration.late_bound || spec.declaration.late_bound_resolvable()) {
            features.extend(spec.declaration.unanalyzed(name));
        }
    }
    features.extend(builtin_gaps(&invocation.declaration.builtin.target));
    features
}

/// Labels in Starlark `select()` keys and branch values; a `None` branch
/// contributes the attribute default's labels
/// (AggregatingAttributeMapper.visitLabelsInSelect,
/// packages/AggregatingAttributeMapper.java:182-215).
fn configurable_labels(
    schema: &BTreeMap<String, AttrSpec>,
    configurable: &BTreeMap<String, Vec<ConfigurablePart>>,
) -> Vec<crate::label::Label> {
    fn push(value: &AttrValue, labels: &mut Vec<crate::label::Label>) {
        let mut add = |text: &str| {
            if let Ok(label) = crate::label::Label::parse_canonical(text) {
                labels.push(label);
            }
        };
        match value {
            AttrValue::Label(Some(text)) => add(text),
            AttrValue::Labels(values) => values.iter().for_each(|text| add(text)),
            AttrValue::LabelKeyedStringDict(entries) => {
                entries.iter().for_each(|(text, _)| add(text))
            }
            AttrValue::StringKeyedLabelDict(entries) => {
                entries.iter().for_each(|(_, text)| add(text))
            }
            AttrValue::LabelListDict(entries) => entries
                .iter()
                .flat_map(|(_, values)| values)
                .for_each(|text| add(text)),
            _ => {}
        }
    }
    let mut labels = Vec::new();
    for (name, parts) in configurable {
        let default = schema.get(name).map(|spec| &spec.default);
        for part in parts {
            match part {
                ConfigurablePart::Plain(value) => push(value, &mut labels),
                ConfigurablePart::Select { branches, .. } => {
                    for (key, value) in branches {
                        if key != crate::configured_attributes::DEFAULT_CONDITION
                            && let Ok(label) = crate::label::Label::parse_canonical(key)
                        {
                            labels.push(label);
                        }
                        if let Some(value) = value.as_ref().or(default) {
                            push(value, &mut labels);
                        }
                    }
                }
            }
        }
    }
    let mut seen = std::collections::BTreeSet::new();
    labels.retain(|label| seen.insert(label.clone()));
    labels
}

/// Owned view of a `rule()` definition's retained declaration (tests/diagnostics).
#[cfg(test)]
pub(crate) struct RuleDeclarationView {
    pub declaration: RuleDeclaration,
    pub schema: BTreeMap<String, AttrSpec>,
    pub has_initializer: bool,
}

#[cfg(test)]
pub(crate) fn rule_declaration(value: Value<'_>) -> Option<RuleDeclarationView> {
    let definition = Definition::from_value(value)?;
    Some(RuleDeclarationView {
        declaration: definition.declaration.clone(),
        schema: definition.schema.clone(),
        has_initializer: !definition.initializer.is_empty(),
    })
}

/// Copy the owned schema without exposing the private Starlark descriptor type.
pub(crate) fn attribute_spec(value: Value<'_>) -> Option<AttrSpec> {
    attribute_spec_ref(value).cloned()
}

/// Borrow a descriptor so host preparation can charge defaults before copying.
/// Computed-default descriptors expose the same owned schema, with
/// `declaration.computed_default` set.
pub(crate) fn attribute_spec_ref(value: Value<'_>) -> Option<&AttrSpec> {
    value
        .downcast_ref::<Descriptor>()
        .map(|value| &value.0)
        .or_else(|| ComputedDescriptor::from_value(value).map(|value| &value.spec))
}

/// The retained function of a computed-default descriptor.
fn computed_default_function(value: Value<'_>) -> Option<Value<'_>> {
    ComputedDescriptor::from_value(value)
        .map(|value| value.function.to_value())
        .filter(|function| !function.is_none())
}

/// The retained Starlark `transition()` of an attribute descriptor.
fn attribute_transition(value: Value<'_>) -> Option<Value<'_>> {
    ComputedDescriptor::from_value(value)
        .map(|value| value.transition.to_value())
        .filter(|transition| !transition.is_none())
}

/// The attribute name in `in A attribute of ...` errors:
/// EventHandlingErrorReporter.maskInternalAttributeNames
/// (analysis/EventHandlingErrorReporter.java:84-86) applied to the native
/// name, so implicit (`$x`) attributes, including lifted subrule ones, read
/// `(an implicit dependency)` and late-bound ones keep `:x`.
pub(crate) fn attribute_message_name(name: &str, spec: &AttrSpec) -> String {
    let native = native_attribute_name(name, spec.declaration.late_bound);
    if native.starts_with('$') {
        "(an implicit dependency)".to_owned()
    } else {
        native
    }
}

/// AttributeValueSource.validateStarlarkName (packages/AttributeValueSource.java:47-58):
/// computed (COMPUTED_DEFAULT) and late-bound (LATE_BOUND) attributes must be
/// private. Shared by rule(), repository_rule() and tag_class().
pub(crate) fn check_value_source_name(name: &str, spec: &AttrSpec) -> anyhow::Result<()> {
    if (spec.declaration.late_bound || spec.declaration.computed_default) && !name.starts_with('_')
    {
        bail!(
            "When an attribute value is a function, the attribute must be private (i.e. start with '_'). Found '{name}'"
        );
    }
    Ok(())
}

/// `Dict.cast(attrs, String, Descriptor, "attrs")` plus checkAttributeName
/// (StarlarkRuleClassFunctions.java:1280-1298), for aspect() attributes.
pub(crate) fn attribute_dict(value: Value<'_>) -> anyhow::Result<Vec<(String, AttrSpec)>> {
    let dict = DictRef::from_value(value)
        .ok_or_else(|| anyhow!("got {} for 'attrs', want dict", value.get_type()))?;
    for (name, spec) in dict.iter() {
        if name.unpack_str().is_none() || attribute_spec_ref(spec).is_none() {
            bail!(
                "got dict<{}, {}> for 'attrs', want dict<string, Attribute>",
                name.get_type(),
                spec.get_type()
            );
        }
    }
    let mut budget = crate::repository_budget::HostBudget::default();
    let mut attributes = Vec::new();
    for (name, spec) in dict.iter() {
        let name = name.unpack_str().expect("checked attribute name");
        if !crate::rule_declarations::is_identifier(name) {
            bail!("attribute name `{name}` is not a valid identifier.");
        }
        let spec = attribute_spec_ref(spec).expect("checked");
        check_value_source_name(name, spec)?;
        // StarlarkRuleClassFunctions.java:1374-1379.
        if spec.declaration.configurable_set {
            bail!(
                "attribute '{}' has the 'configurable' argument set, which is not allowed in aspect definitions",
                native_attribute_name(name, spec.declaration.late_bound)
            );
        }
        budget.string(name).map_err(anyhow::Error::msg)?;
        budget.spec(spec).map_err(anyhow::Error::msg)?;
        attributes.push((name.to_owned(), spec.clone()));
    }
    Ok(attributes)
}

/// buildProviderPredicate for aspect required_providers/required_aspect_providers.
pub(crate) fn provider_predicate(
    value: Value<'_>,
    what: &str,
) -> anyhow::Result<Vec<Vec<ProviderRequirement>>> {
    required_providers(value).map_err(|e| anyhow!("{what}: {e}"))
}

fn descriptor(
    kind: AttrKind,
    default: Option<Value<'_>>,
    mandatory: bool,
    allow_files: bool,
    allow_single_file: bool,
) -> anyhow::Result<Descriptor> {
    descriptor_in_package(
        kind,
        default,
        mandatory,
        allow_files,
        allow_single_file,
        None,
    )
}

fn descriptor_in_package(
    kind: AttrKind,
    default: Option<Value<'_>>,
    mandatory: bool,
    allow_files: bool,
    allow_single_file: bool,
    resolver: Option<&LabelResolver<'_>>,
) -> anyhow::Result<Descriptor> {
    if let Some(value) = default {
        kind.preflight_default(value)?;
    }
    let explicit_default = default.is_some_and(|value| !value.is_none());
    let default = default
        .map(|v| kind.parse(v, resolver))
        .transpose()?
        .unwrap_or_else(|| kind.empty());
    Ok(Descriptor(AttrSpec {
        kind,
        default,
        mandatory,
        allow_files: allow_files || allow_single_file,
        allow_single_file,
        non_empty: false,
        required_providers: Vec::new(),
        declaration: AttrDeclaration {
            explicit_default,
            ..AttrDeclaration::default()
        },
    }))
}

/// `values=` (StarlarkAttrModule.java:476-482): Sequence.noneableCast to Object.
fn allowed_values(kind: &AttrKind, values: Value<'_>) -> anyhow::Result<Vec<AllowedValue>> {
    // @Param(name = VALUES_ARG) is Sequence-typed (StarlarkAttrModuleApi.java:228-233, 283-290).
    let items = sequence_ref(values).ok_or_else(|| {
        crate::rule_declarations::param_type_error(
            kind.starlark_name(),
            "values",
            values,
            "sequence",
        )
    })?;
    let mut budget = crate::repository_budget::HostBudget::default();
    let mut allowed: Vec<AllowedValue> = Vec::new();
    for &item in items {
        budget.node().map_err(anyhow::Error::msg)?;
        let value = kind.parse(item, None).ok().filter(|_| match kind {
            AttrKind::String => item.unpack_str().is_some(),
            _ => true,
        });
        let display = item
            .unpack_str()
            .map_or_else(|| item.to_str(), str::to_owned);
        budget.string(&display).map_err(anyhow::Error::msg)?;
        // ImmutableSet.copyOf deduplicates equal values.
        if !allowed
            .iter()
            .any(|seen| seen.display == display && seen.value == value)
        {
            allowed.push(AllowedValue { value, display });
        }
    }
    Ok(allowed)
}

/// LabelKeyedDictType.convert collision message (packages/BuildType.java:449-490).
fn label_keyed_duplicates(
    value: Value<'_>,
    what: &str,
    resolve: &LabelResolver<'_>,
) -> anyhow::Result<()> {
    let dict = DictRef::from_value(value).ok_or_else(|| anyhow!("expected a dict"))?;
    let mut converted: Vec<(String, Vec<String>)> = Vec::new();
    for (key, _) in dict.iter() {
        let canonical = match crate::starlark_types::unpack_label(key) {
            Some(label) => label,
            None => resolve(key.unpack_str().expect("checked label key"))?.to_string(),
        };
        let shown = crate::label::Label::parse_canonical(&canonical)
            .map(|label| crate::rule_declarations::canonical_form(&label))
            .unwrap_or(canonical);
        match converted.iter_mut().find(|(seen, _)| *seen == shown) {
            Some((_, originals)) => originals.push(key.to_repr()),
            None => converted.push((shown, vec![key.to_repr()])),
        }
    }
    let duplicates = converted
        .iter()
        .filter(|(_, originals)| originals.len() > 1)
        .map(|(label, originals)| format!(" {label} (as [{}])", originals.join(", ")))
        .collect::<Vec<_>>();
    if !duplicates.is_empty() {
        bail!("duplicate labels in {what}:{}", duplicates.join(","));
    }
    Ok(())
}

/// setAllowedFileTypes (StarlarkAttrModule.java:128-140): (files allowed, extension list).
fn file_types(param: &str, value: Value<'_>) -> anyhow::Result<(bool, Option<Vec<String>>)> {
    if let Some(flag) = value.unpack_bool() {
        return Ok((flag, None));
    }
    if sequence_ref(value).is_some() {
        let types = crate::rule_declarations::cast_strings(value, "allow_files argument")?;
        return Ok((true, Some(types)));
    }
    bail!("{param} should be a boolean or a string list")
}

/// Dependency-attribute options shared by label, label_list and the
/// label-valued dictionaries.
struct DependencyOptions<'v> {
    /// `attr` function name for parameter type errors.
    function: &'static str,
    executable: bool,
    allow_files: Value<'v>,
    allow_single_file: Value<'v>,
    skip_validations: bool,
    providers: Value<'v>,
    for_dependency_resolution: Option<bool>,
    allow_rules: Value<'v>,
    cfg: Value<'v>,
    aspects: Value<'v>,
    flags: Value<'v>,
    /// `configurable=` (bool or unbound), recorded as user-set
    /// (StarlarkAttrModule.java:402-410).
    configurable: Option<bool>,
}

/// StarlarkAttrModule.createAttribute for dependency attributes, in Bazel's
/// validation order (StarlarkAttrModule.java:317-513).
fn dependency_descriptor<'v>(
    kind: AttrKind,
    default: Option<Value<'v>>,
    mandatory: bool,
    allow_empty: bool,
    options: DependencyOptions<'v>,
    eval: &mut Evaluator<'v, '_, '_>,
) -> anyhow::Result<Value<'v>> {
    use crate::rule_declarations::{param_type_error, require_sequence};
    let function = options.function;
    // BuiltinFunction @Param allowedTypes, in parameter order (StarlarkAttrModuleApi.java):
    // Dict|StarlarkFunction for the label-keyed and string-keyed label
    // dictionaries (:788-805, :906-914), Dict only for label_list_dict (:1031-1046).
    let dict_default = match kind {
        AttrKind::LabelKeyedStringDict | AttrKind::StringKeyedLabelDict => Some(true),
        AttrKind::LabelListDict => Some(false),
        _ => None,
    };
    if let Some(function_allowed) = dict_default
        && let Some(value) = default
        && DictRef::from_value(value).is_none()
        && !(function_allowed && value.parameters_spec().is_some())
    {
        return Err(param_type_error(
            function,
            "default",
            value,
            if function_allowed {
                "dict or function"
            } else {
                "dict"
            },
        ));
    }
    // attr.label_list's `default` is Sequence or StarlarkFunction, not
    // NoneType (StarlarkAttrModuleApi.java:621-626), so a LateBoundDefault
    // or None fails the @Param check.
    if kind == AttrKind::LabelList
        && let Some(value) = default
        && sequence_ref(value).is_none()
        && value.parameters_spec().is_none()
    {
        return Err(param_type_error(
            function,
            "default",
            value,
            "sequence or function",
        ));
    }
    if !options.allow_files.is_none()
        && options.allow_files.unpack_bool().is_none()
        && sequence_ref(options.allow_files).is_none()
    {
        return Err(param_type_error(
            function,
            "allow_files",
            options.allow_files,
            "bool, sequence, or NoneType",
        ));
    }
    require_sequence(function, "providers", options.providers)?;
    if !options.allow_rules.is_none() && sequence_ref(options.allow_rules).is_none() {
        return Err(param_type_error(
            function,
            "allow_rules",
            options.allow_rules,
            "sequence or NoneType",
        ));
    }
    require_sequence(function, "aspects", options.aspects)?;
    require_sequence(function, "flags", options.flags)?;
    let late_bound = kind == AttrKind::Label
        && default.is_some_and(crate::rule_declarations::is_late_bound_default);
    let configuration_field = default
        .filter(|_| late_bound)
        .and_then(crate::language_modules::configuration_field_of);
    // A Starlark function default is a computed default
    // (StarlarkAttrModule.java:358-368, StarlarkComputedDefaultTemplate).
    let computed = default.filter(|value| value.parameters_spec().is_some());
    if computed.is_some() && !matches!(kind, AttrKind::Label | AttrKind::LabelList) {
        bail!(
            "computed (function) defaults of attr.{} are not supported by the native profile",
            kind.starlark_name()
        );
    }
    // Late-bound and computed defaults are retained as metadata (and the
    // function); they never reach the ordinary default coercion.
    let default = if late_bound || computed.is_some() {
        None
    } else {
        default
    };
    let budget = std::cell::RefCell::new(crate::repository_budget::HostBudget::default());
    let resolve =
        |text: &str| crate::loading::resolve_label_bounded(eval, text, &mut budget.borrow_mut());
    if dict_default.is_some()
        && let Some(value) = default
    {
        // Attribute.Builder.defaultValue: what = "parameter 'default' of attribute
        // 'KIND'" (Attribute.java:700-708), with KIND the createAttribute name.
        let what = format!(
            "parameter 'default' of attribute '{}'",
            kind.starlark_name()
        );
        if let Some(error) = kind.conversion_error(value, &what) {
            bail!("{error}");
        }
        if kind == AttrKind::LabelKeyedStringDict {
            label_keyed_duplicates(value, &what, &resolve)?;
        }
    }
    let mut descriptor = descriptor_in_package(
        kind,
        default,
        mandatory,
        false,
        false,
        definition_package(eval).map(|_| &resolve as &LabelResolver<'_>),
    )?;
    let declaration = &mut descriptor.0.declaration;
    declaration.late_bound = late_bound;
    declaration.configuration_field = configuration_field;
    declaration.computed_default = computed.is_some();
    declaration.explicit_default |= late_bound || computed.is_some();
    declaration.configurable_set = options.configurable.is_some();
    declaration.flags = crate::rule_declarations::noneable_strings(options.flags, "flags")?;
    if let Some(flag) = declaration
        .flags
        .iter()
        .find(|flag| !ATTRIBUTE_FLAGS.contains(&flag.as_str()))
    {
        bail!("unknown attribute flag '{flag}'");
    }
    declaration.for_dependency_resolution = options.for_dependency_resolution;
    declaration.skip_validations = options.skip_validations;
    descriptor.0.non_empty = !allow_empty;
    if options.executable {
        declaration.executable = true;
        if options.cfg.is_none() {
            bail!(
                "cfg parameter is mandatory when executable=True is provided. Please see https://bazel.build/extending/rules#configurations for more details."
            );
        }
    }
    if !options.allow_files.is_none() && !options.allow_single_file.is_none() {
        bail!("Cannot specify both allow_files and allow_single_file");
    }
    if !options.allow_files.is_none() {
        let (allowed, types) = file_types("allow_files", options.allow_files)?;
        descriptor.0.allow_files = allowed;
        declaration.file_types = types;
    } else if !options.allow_single_file.is_none() {
        let (allowed, types) = file_types("allow_single_file", options.allow_single_file)?;
        descriptor.0.allow_files = allowed;
        descriptor.0.allow_single_file = allowed;
        declaration.file_types = types;
    }
    if !options.allow_rules.is_none() {
        declaration.allow_rules = Some(crate::rule_declarations::cast_strings(
            options.allow_rules,
            "allowed rule classes for attribute definition",
        )?);
    }
    let disable_host = eval
        .extra
        .and_then(|extra| extra.downcast_ref::<crate::loading::ExtensionContext>())
        .is_some_and(|context| context.repositories.disable_starlark_host_transitions());
    declaration.cfg = crate::rule_declarations::attribute_cfg(options.cfg, disable_host)?;
    declaration.aspects = crate::rule_declarations::aspect_identities(options.aspects, "aspects")?;
    declaration.aspect_definitions = crate::rule_declarations::query_aspects(options.aspects);
    let declaration = declaration.clone();
    let mut descriptor = with_required_providers(descriptor, options.providers)?;
    descriptor.0.declaration = declaration;
    crate::repository_budget::HostBudget::default()
        .spec(&descriptor.0)
        .map_err(anyhow::Error::msg)?;
    let transition = if crate::rule_declarations::is_starlark_transition(options.cfg) {
        Some(options.cfg)
    } else {
        None
    };
    Ok(if computed.is_some() || transition.is_some() {
        eval.heap().alloc(ComputedDescriptorGen {
            function: computed.unwrap_or_else(Value::new_none),
            transition: transition.unwrap_or_else(Value::new_none),
            spec: descriptor.0,
        })
    } else {
        eval.heap().alloc(descriptor)
    })
}

#[starlark_module]
fn attr_globals(builder: &mut GlobalsBuilder) {
    fn string<'v>(
        #[starlark(require = named)] default: Option<Value<'v>>,
        #[starlark(require = named, default = false)] mandatory: bool,
        #[starlark(require = named)] doc: Option<&str>,
        #[starlark(require = named, default = AllocList::EMPTY)] values: Value<'v>,
    ) -> anyhow::Result<Descriptor> {
        let _ = doc;
        let mut descriptor = descriptor(AttrKind::String, default, mandatory, false, false)?;
        descriptor.0.declaration.values = allowed_values(&AttrKind::String, values)?;
        Ok(descriptor)
    }
    fn string_list(
        #[starlark(require = named, default = false)] mandatory: bool,
        #[starlark(require = named, default = true)] allow_empty: bool,
        #[starlark(require = named)] default: Option<Value<'_>>,
        #[starlark(require = named)] doc: Option<&str>,
    ) -> anyhow::Result<Descriptor> {
        let _ = doc;
        let mut descriptor = descriptor(AttrKind::StringList, default, mandatory, false, false)?;
        descriptor.0.non_empty = !allow_empty;
        Ok(descriptor)
    }
    fn int_list(
        #[starlark(require = named, default = false)] mandatory: bool,
        #[starlark(require = named, default = true)] allow_empty: bool,
        #[starlark(require = named)] default: Option<Value<'_>>,
        #[starlark(require = named)] doc: Option<&str>,
    ) -> anyhow::Result<Descriptor> {
        let _ = doc;
        if let Some(value) = default {
            // @Param allowedTypes Sequence<StarlarkInt> (StarlarkAttrModuleApi.java:577-582),
            // then IntegerListType conversion of each element.
            if sequence_ref(value).is_none() {
                return Err(crate::rule_declarations::param_type_error(
                    "int_list", "default", value, "sequence",
                ));
            }
            if let Some(error) =
                AttrKind::IntList.conversion_error(value, "parameter 'default' of attribute ''")
            {
                bail!("{error}");
            }
        }
        let mut descriptor = descriptor(AttrKind::IntList, default, mandatory, false, false)?;
        descriptor.0.non_empty = !allow_empty;
        Ok(descriptor)
    }
    fn string_dict(
        #[starlark(default = true)] allow_empty: bool,
        #[starlark(require = named)] default: Option<Value<'_>>,
        #[starlark(require = named)] configurable: Option<Value<'_>>,
        #[starlark(require = named, default = false)] mandatory: bool,
        #[starlark(require = named, default = NoneType)] doc: Value<'_>,
    ) -> anyhow::Result<Descriptor> {
        if configurable.is_some() {
            bail!("attr.string_dict configurable is unsupported");
        }
        if !doc.is_none() && doc.unpack_str().is_none() {
            bail!("attr.string_dict doc must be a string or None");
        }
        let mut descriptor = descriptor(AttrKind::StringDict, default, mandatory, false, false)?;
        descriptor.0.non_empty = !allow_empty;
        Ok(descriptor)
    }
    fn string_list_dict(
        #[starlark(default = true)] allow_empty: bool,
        #[starlark(require = named)] default: Option<Value<'_>>,
        #[starlark(require = named)] configurable: Option<Value<'_>>,
        #[starlark(require = named, default = false)] mandatory: bool,
        #[starlark(require = named, default = NoneType)] doc: Value<'_>,
    ) -> anyhow::Result<Descriptor> {
        if configurable.is_some() {
            bail!("attr.string_list_dict configurable is unsupported");
        }
        if !doc.is_none() && doc.unpack_str().is_none() {
            bail!("attr.string_list_dict doc must be a string or None");
        }
        let mut descriptor =
            descriptor(AttrKind::StringListDict, default, mandatory, false, false)?;
        descriptor.0.non_empty = !allow_empty;
        Ok(descriptor)
    }
    fn int<'v>(
        #[starlark(require = named)] default: Option<Value<'v>>,
        #[starlark(require = named, default = false)] mandatory: bool,
        #[starlark(require = named)] doc: Option<&str>,
        #[starlark(require = named, default = AllocList::EMPTY)] values: Value<'v>,
    ) -> anyhow::Result<Descriptor> {
        let _ = doc;
        let mut descriptor = descriptor(AttrKind::Int, default, mandatory, false, false)?;
        descriptor.0.declaration.values = allowed_values(&AttrKind::Int, values)?;
        Ok(descriptor)
    }
    fn bool(
        #[starlark(require = named)] default: Option<Value<'_>>,
        #[starlark(require = named, default = false)] mandatory: bool,
        #[starlark(require = named)] doc: Option<&str>,
    ) -> anyhow::Result<Descriptor> {
        let _ = doc;
        // `default` is a Boolean parameter (StarlarkAttrModuleApi.java:1152-1157):
        // the Starlark call check runs before any Type.BOOLEAN conversion, so
        // unlike attribute values the integers 0 and 1 are rejected here.
        if let Some(value) = default
            && value.unpack_bool().is_none()
        {
            bail!(
                "in call to bool(), parameter 'default' got value of type '{}', want 'bool'",
                value.get_type()
            );
        }
        descriptor(AttrKind::Bool, default, mandatory, false, false)
    }
    fn label<'v>(
        #[starlark(require = named)] default: Option<Value<'v>>,
        #[starlark(require = named)] doc: Option<&str>,
        #[starlark(require = named, default = false)] executable: bool,
        #[starlark(require = named, default = NoneType)] allow_files: Value<'v>,
        #[starlark(require = named, default = NoneType)] allow_single_file: Value<'v>,
        #[starlark(require = named, default = false)] mandatory: bool,
        #[starlark(require = named, default = false)] skip_validations: bool,
        #[starlark(require = named, default = AllocList::EMPTY)] providers: Value<'v>,
        #[starlark(require = named)] for_dependency_resolution: Option<bool>,
        #[starlark(require = named, default = NoneType)] allow_rules: Value<'v>,
        #[starlark(require = named, default = NoneType)] cfg: Value<'v>,
        #[starlark(require = named, default = AllocList::EMPTY)] aspects: Value<'v>,
        #[starlark(require = named, default = AllocList::EMPTY)] flags: Value<'v>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> anyhow::Result<Value<'v>> {
        let _ = doc;
        const KIND_FUNCTION: &str = "label";
        dependency_descriptor(
            AttrKind::Label,
            default,
            mandatory,
            true,
            DependencyOptions {
                function: KIND_FUNCTION,
                executable,
                allow_files,
                allow_single_file,
                skip_validations,
                providers,
                for_dependency_resolution,
                allow_rules,
                cfg,
                aspects,
                flags,
                configurable: None,
            },
            eval,
        )
    }
    fn label_list<'v>(
        #[starlark(default = true)] allow_empty: bool,
        #[starlark(require = named)] default: Option<Value<'v>>,
        #[starlark(require = named)] doc: Option<&str>,
        #[starlark(require = named, default = NoneType)] allow_files: Value<'v>,
        #[starlark(require = named, default = NoneType)] allow_rules: Value<'v>,
        #[starlark(require = named, default = AllocList::EMPTY)] providers: Value<'v>,
        #[starlark(require = named)] for_dependency_resolution: Option<bool>,
        #[starlark(require = named, default = AllocList::EMPTY)] flags: Value<'v>,
        #[starlark(require = named, default = false)] mandatory: bool,
        #[starlark(require = named, default = false)] skip_validations: bool,
        #[starlark(require = named, default = NoneType)] cfg: Value<'v>,
        #[starlark(require = named, default = AllocList::EMPTY)] aspects: Value<'v>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> anyhow::Result<Value<'v>> {
        let _ = doc;
        const KIND_FUNCTION: &str = "label_list";
        dependency_descriptor(
            AttrKind::LabelList,
            default,
            mandatory,
            allow_empty,
            DependencyOptions {
                function: KIND_FUNCTION,
                executable: false,
                allow_files,
                allow_single_file: Value::new_none(),
                skip_validations,
                providers,
                for_dependency_resolution,
                allow_rules,
                cfg,
                aspects,
                flags,
                configurable: None,
            },
            eval,
        )
    }
    fn label_keyed_string_dict<'v>(
        #[starlark(default = true)] allow_empty: bool,
        #[starlark(require = named)] default: Option<Value<'v>>,
        #[starlark(require = named)] doc: Option<&str>,
        #[starlark(require = named, default = NoneType)] allow_files: Value<'v>,
        #[starlark(require = named, default = NoneType)] allow_rules: Value<'v>,
        #[starlark(require = named, default = AllocList::EMPTY)] providers: Value<'v>,
        #[starlark(require = named)] for_dependency_resolution: Option<bool>,
        #[starlark(require = named, default = AllocList::EMPTY)] flags: Value<'v>,
        #[starlark(require = named, default = false)] mandatory: bool,
        #[starlark(require = named, default = false)] skip_validations: bool,
        #[starlark(require = named, default = NoneType)] cfg: Value<'v>,
        #[starlark(require = named, default = AllocList::EMPTY)] aspects: Value<'v>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> anyhow::Result<Value<'v>> {
        let _ = doc;
        const KIND_FUNCTION: &str = "label_keyed_string_dict";
        dependency_descriptor(
            AttrKind::LabelKeyedStringDict,
            default,
            mandatory,
            allow_empty,
            DependencyOptions {
                function: KIND_FUNCTION,
                executable: false,
                allow_files,
                allow_single_file: Value::new_none(),
                skip_validations,
                providers,
                for_dependency_resolution,
                allow_rules,
                cfg,
                aspects,
                flags,
                configurable: None,
            },
            eval,
        )
    }
    /// StarlarkAttrModule.stringKeyedLabelDictAttribute (StarlarkAttrModule.java:918-967).
    fn string_keyed_label_dict<'v>(
        #[starlark(default = true)] allow_empty: bool,
        #[starlark(require = named)] configurable: Option<bool>,
        #[starlark(require = named)] default: Option<Value<'v>>,
        #[starlark(require = named)] doc: Option<&str>,
        #[starlark(require = named, default = NoneType)] allow_files: Value<'v>,
        #[starlark(require = named, default = NoneType)] allow_rules: Value<'v>,
        #[starlark(require = named, default = AllocList::EMPTY)] providers: Value<'v>,
        #[starlark(require = named)] for_dependency_resolution: Option<bool>,
        #[starlark(require = named, default = AllocList::EMPTY)] flags: Value<'v>,
        #[starlark(require = named, default = false)] mandatory: bool,
        #[starlark(require = named, default = NoneType)] cfg: Value<'v>,
        #[starlark(require = named, default = AllocList::EMPTY)] aspects: Value<'v>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> anyhow::Result<Value<'v>> {
        let _ = doc;
        const KIND_FUNCTION: &str = "string_keyed_label_dict";
        dependency_descriptor(
            AttrKind::StringKeyedLabelDict,
            default,
            mandatory,
            allow_empty,
            DependencyOptions {
                function: KIND_FUNCTION,
                executable: false,
                allow_files,
                allow_single_file: Value::new_none(),
                skip_validations: false,
                providers,
                for_dependency_resolution,
                allow_rules,
                cfg,
                aspects,
                flags,
                configurable,
            },
            eval,
        )
    }
    /// StarlarkAttrModule.labelListDictAttribute (StarlarkAttrModule.java:1023-1075).
    fn label_list_dict<'v>(
        #[starlark(default = true)] allow_empty: bool,
        #[starlark(require = named)] configurable: Option<bool>,
        #[starlark(require = named)] default: Option<Value<'v>>,
        #[starlark(require = named)] doc: Option<&str>,
        #[starlark(require = named, default = NoneType)] allow_files: Value<'v>,
        #[starlark(require = named, default = NoneType)] allow_rules: Value<'v>,
        #[starlark(require = named, default = AllocList::EMPTY)] providers: Value<'v>,
        #[starlark(require = named)] for_dependency_resolution: Option<bool>,
        #[starlark(require = named, default = AllocList::EMPTY)] flags: Value<'v>,
        #[starlark(require = named, default = false)] mandatory: bool,
        #[starlark(require = named, default = false)] skip_validations: bool,
        #[starlark(require = named, default = NoneType)] cfg: Value<'v>,
        #[starlark(require = named, default = AllocList::EMPTY)] aspects: Value<'v>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> anyhow::Result<Value<'v>> {
        let _ = doc;
        const KIND_FUNCTION: &str = "label_list_dict";
        dependency_descriptor(
            AttrKind::LabelListDict,
            default,
            mandatory,
            allow_empty,
            DependencyOptions {
                function: KIND_FUNCTION,
                executable: false,
                allow_files,
                allow_single_file: Value::new_none(),
                skip_validations,
                providers,
                for_dependency_resolution,
                allow_rules,
                cfg,
                aspects,
                flags,
                configurable,
            },
            eval,
        )
    }
    fn output(
        #[starlark(require = named, default = false)] mandatory: bool,
        #[starlark(require = named)] doc: Option<&str>,
    ) -> anyhow::Result<Descriptor> {
        let _ = doc;
        descriptor(AttrKind::Output, None, mandatory, false, false)
    }
    fn output_list(
        #[starlark(default = true)] allow_empty: bool,
        #[starlark(require = named, default = false)] mandatory: bool,
        #[starlark(require = named)] doc: Option<&str>,
    ) -> anyhow::Result<Descriptor> {
        let _ = doc;
        let mut descriptor = descriptor(AttrKind::OutputList, None, mandatory, false, false)?;
        descriptor.0.non_empty = !allow_empty;
        Ok(descriptor)
    }
}

/// Raw `rule()` arguments (StarlarkRuleFunctionsApi.java:428-788).
struct RuleArguments<'v> {
    implementation: Value<'v>,
    test: Option<bool>,
    attrs: Option<Value<'v>>,
    outputs: Value<'v>,
    executable: Option<bool>,
    output_to_genfiles: bool,
    fragments: Value<'v>,
    host_fragments: Value<'v>,
    skylark_testable: bool,
    toolchains: Value<'v>,
    provides: Value<'v>,
    dependency_resolution_rule: bool,
    exec_compatible_with: Value<'v>,
    analysis_test: bool,
    build_setting: Value<'v>,
    cfg: Value<'v>,
    exec_groups: Value<'v>,
    initializer: Value<'v>,
    parent: Value<'v>,
    extendable: Value<'v>,
    subrules: Value<'v>,
}

/// StarlarkRuleClassFunctions.rule + createRule (StarlarkRuleClassFunctions.java:548-1171).
fn define_rule<'v>(
    args: RuleArguments<'v>,
    eval: &mut Evaluator<'v, '_, '_>,
) -> anyhow::Result<Value<'v>> {
    use crate::rule_declarations::{param_type_error, require_sequence};
    // BuiltinFunction @Param allowedTypes, in parameter order, before the body
    // (StarlarkRuleFunctionsApi.java:428-762).
    if let Some(attrs) = args.attrs
        && DictRef::from_value(attrs).is_none()
    {
        return Err(param_type_error("rule", "attrs", attrs, "dict"));
    }
    for (param, value) in [
        ("fragments", args.fragments),
        ("host_fragments", args.host_fragments),
        ("toolchains", args.toolchains),
        ("provides", args.provides),
        ("exec_compatible_with", args.exec_compatible_with),
    ] {
        require_sequence("rule", param, value)?;
    }
    if !args.exec_groups.is_none() && DictRef::from_value(args.exec_groups).is_none() {
        return Err(param_type_error(
            "rule",
            "exec_groups",
            args.exec_groups,
            "dict or NoneType",
        ));
    }
    if !args.extendable.is_none()
        && args.extendable.unpack_bool().is_none()
        && args.extendable.unpack_str().is_none()
        && crate::starlark_types::label_parts(args.extendable).is_none()
    {
        return Err(param_type_error(
            "rule",
            "extendable",
            args.extendable,
            "bool, Label, string, or NoneType",
        ));
    }
    require_sequence("rule", "subrules", args.subrules)?;
    let Some(definition) = definition_package(eval) else {
        bail!(
            "rule() definitions are only allowed during extension initialization, not BUILD evaluation"
        );
    };
    let RuleArguments {
        implementation,
        attrs,
        build_setting,
        cfg,
        ..
    } = args;
    if !args.parent.is_none() {
        // StarlarkRuleClassFunctions.java:585-622.
        let Some(parent) = Definition::from_value(args.parent) else {
            bail!(
                "Parent needs to be a Starlark rule, was {}",
                args.parent.get_type()
            );
        };
        if parent.exported.get().is_none() {
            bail!("Please export the parent rule before extending it.");
        }
        for (failed, message) in [
            (
                args.executable.is_some(),
                "Omit executable parameter when extending rules.",
            ),
            (
                args.test.is_some(),
                "Omit test parameter when extending rules.",
            ),
            (
                !args.outputs.is_none(),
                "implicit_outputs is not supported when extending rules (deprecated).",
            ),
            (
                crate::custom_rule::sequence_ref(args.host_fragments)
                    .is_some_and(|items| !items.is_empty()),
                "host_fragments are not supported when extending rules (deprecated).",
            ),
            (
                args.output_to_genfiles,
                "output_to_genfiles are not supported when extending rules (deprecated).",
            ),
            (
                args.skylark_testable,
                "_skylark_testable is not supported when extending rules.",
            ),
            (
                args.analysis_test,
                "analysis_test is not supported when extending rules.",
            ),
            (
                !build_setting.is_none(),
                "build_setting is not supported when extending rules.",
            ),
        ] {
            if failed {
                bail!("{message}");
            }
        }
        bail!("rule(parent=...) rule extension is not supported by the native profile");
    }
    let executable = args.executable.unwrap_or(false);
    let test = args.test.unwrap_or(false) || args.analysis_test;
    let build_setting = if build_setting.is_none() {
        None
    } else {
        Some(*BuildSetting::from_value(build_setting).ok_or_else(|| {
            anyhow!(
                "rule build_setting must be a BuildSetting, got {}",
                build_setting.get_type()
            )
        })?)
    };
    implementation
        .check_callable_with(
            [&starlark::typing::Ty::any()],
            [],
            None,
            None,
            &starlark::typing::Ty::any(),
        )
        .map_err(|e| anyhow!("rule implementation must be callable with ctx: {e}"))?;
    // extendable (StarlarkRuleClassFunctions.java:791-808).
    let extendable = if args.extendable.is_none() {
        None
    } else if let Some(flag) = args.extendable.unpack_bool() {
        Some(if flag { "True" } else { "False" }.to_owned())
    } else if let Some(label) = crate::starlark_types::unpack_label(args.extendable) {
        Some(label)
    } else if let Some(text) = args.extendable.unpack_str() {
        Some(
            crate::bazel_label::Parts::parse(text)
                .map_err(anyhow::Error::msg)
                .and_then(|_| {
                    crate::loading::resolve_label_bounded(
                        eval,
                        text,
                        &mut crate::repository_budget::HostBudget::default(),
                    )
                })
                .map_err(|e| anyhow!("Unable to parse label '{text}': {e}"))?
                .to_string(),
        )
    } else {
        bail!(
            "parameter 'extendable': expected bool, str or Label, but got '{}'",
            args.extendable.get_type()
        );
    };
    let initializer = if args.initializer.is_none() {
        Vec::new()
    } else if args.initializer.parameters_spec().is_some() {
        vec![args.initializer]
    } else {
        bail!(
            "in call to rule(), parameter 'initializer' got value of type '{}', want 'function'",
            args.initializer.get_type()
        );
    };
    let mut schema = BTreeMap::new();
    let mut attr_order = Vec::new();
    let mut computed = Vec::new();
    let mut computed_functions = Vec::new();
    let mut transitions = Vec::new();
    let mut transition_keys = Vec::new();
    let mut budget = crate::repository_budget::HostBudget::default();
    budget.node().map_err(anyhow::Error::msg)?;
    let subrules = crate::subrule::snapshot(args.subrules, &mut budget)?;
    crate::subrule::validate_exports(&subrules, &mut budget)?;
    let mut propagates_aspects = false;
    if let Some(attrs) = attrs {
        let dict = DictRef::from_value(attrs)
            .ok_or_else(|| anyhow!("rule attrs must be a dict of attribute descriptors"))?;
        // attrObjectToAttributesList validates every name before any attribute
        // is added (StarlarkRuleClassFunctions.java:1286-1300, 941-1020).
        let mut descriptors = Vec::new();
        for (name, spec) in dict.iter() {
            let name = name
                .unpack_str()
                .ok_or_else(|| anyhow!("attribute names must be strings"))?;
            if name.is_empty()
                || !name.bytes().enumerate().all(|(i, b)| {
                    b == b'_' || b.is_ascii_alphabetic() || (i > 0 && b.is_ascii_digit())
                })
            {
                bail!("invalid attribute name {name:?}");
            }
            let function = computed_default_function(spec);
            let transition = attribute_transition(spec);
            let spec = attribute_spec_ref(spec)
                .ok_or_else(|| anyhow!("attribute {name:?} must be an attr descriptor"))?;
            // AttributeValueSource.LATE_BOUND/COMPUTED_DEFAULT require the
            // Starlark '_' prefix (packages/AttributeValueSource.java:47-58).
            check_value_source_name(name, spec)?;
            descriptors.push((name, spec, function, transition));
        }
        let builtins =
            crate::native_rules::starlark_builtins(crate::native_rules::StarlarkRuleKind {
                test,
                executable,
                dependency_resolution: args.dependency_resolution_rule,
            });
        for (name, spec, function, transition) in descriptors {
            // "configurable" may only be user-set for symbolic macros
            // (StarlarkRuleClassFunctions.java:964-970), named natively.
            if spec.declaration.configurable_set {
                bail!(
                    "attribute '{}' has the 'configurable' argument set, which is not allowed in rule definitions",
                    native_attribute_name(name, spec.declaration.late_bound)
                );
            }
            // RuleClass.Builder.override (packages/RuleClass.java:1301-1306):
            // built-in attributes, including `name` and the private `$x`/`:x`
            // ones (reported by public name), cannot be overridden.
            if builtins.contains(&native_attribute_name(name, spec.declaration.late_bound)) {
                bail!("attribute `{name}`: built-in attributes cannot be overridden.");
            }
            if name.starts_with('_') && spec.mandatory {
                bail!(
                    "private attribute {name:?} cannot be mandatory because it cannot be set explicitly"
                );
            }
            // StarlarkRuleClassFunctions.java:991-998.
            if spec.declaration.cfg.is_analysis_test() && !args.analysis_test {
                bail!(
                    "Only rule definitions with analysis_test=True may have attributes with analysis_test_transition transitions"
                );
            }
            propagates_aspects |= !spec.declaration.aspects.is_empty();
            budget.string(name).map_err(anyhow::Error::msg)?;
            budget.spec(spec).map_err(anyhow::Error::msg)?;
            if let Some(function) = function {
                let parameters = parameter_names(function)?;
                for parameter in &parameters {
                    budget.string(parameter).map_err(anyhow::Error::msg)?;
                }
                computed.push((name.to_owned(), parameters));
                computed_functions.push(function);
            }
            if let Some(transition) = transition {
                transition_keys.push(name.to_owned());
                transitions.push(transition);
            }
            schema.insert(name.to_owned(), spec.clone());
            attr_order.push(name.to_owned());
        }
    }
    // Implicit outputs (StarlarkRuleClassFunctions.java:1102-1122).
    let mut outputs_function = Vec::new();
    let implicit_outputs = if args.outputs.is_none() {
        ImplicitOutputs::None
    } else if args.outputs.parameters_spec().is_some() {
        outputs_function.push(args.outputs);
        ImplicitOutputs::Function
    } else if let Some(dict) = DictRef::from_value(args.outputs) {
        let mut templates = Vec::new();
        for (key, value) in dict.iter() {
            let (Some(key), Some(value)) = (key.unpack_str(), value.unpack_str()) else {
                bail!(
                    "got dict<{}, {}> for 'implicit outputs of the rule class', want dict<string, string>",
                    key.get_type(),
                    value.get_type()
                );
            };
            budget.string(key).map_err(anyhow::Error::msg)?;
            budget.string(value).map_err(anyhow::Error::msg)?;
            templates.push((key.to_owned(), value.to_owned()));
        }
        ImplicitOutputs::Templates(templates)
    } else {
        bail!(
            "in call to rule(), parameter 'outputs' got value of type '{}', want 'dict, NoneType, or function'",
            args.outputs.get_type()
        );
    };
    let fragments = crate::rule_declarations::cast_strings(args.fragments, "fragments")?;
    let host_fragments =
        crate::rule_declarations::cast_strings(args.host_fragments, "host_fragments")?;
    let toolchains = crate::rule_declarations::toolchains(eval, args.toolchains, eval.heap())?;
    let exec_groups = crate::rule_declarations::declared_exec_groups(args.exec_groups)?;
    if build_setting.is_some() && !cfg.is_none() {
        bail!("Build setting rules cannot use the `cfg` param to apply transitions to themselves.");
    }
    let rule_cfg = crate::rule_declarations::rule_cfg(cfg)?;
    if crate::rule_declarations::is_starlark_transition(cfg) {
        transition_keys.push(String::new());
        transitions.push(cfg);
    }
    if args.dependency_resolution_rule {
        // StarlarkRuleClassFunctions.java:1133-1146.
        if !subrules.is_empty() {
            bail!("Rules that can be required for materializers cannot have subrules");
        }
        if !toolchains.is_empty() {
            bail!("Rules that can be required for materializers cannot depend on toolchains");
        }
        if propagates_aspects {
            bail!("Rules that can be required for materializes cannot propagate aspects");
        }
    }
    let provides = crate::rule_declarations::provider_identifiers(args.provides, "provides")?;
    let exec_compatible_with =
        crate::rule_declarations::exec_compatible_with(eval, args.exec_compatible_with)?;
    if let Some(setting) = build_setting {
        for name in ["build_setting_default", "help"] {
            if schema.contains_key(name) {
                bail!("attribute {name:?} is reserved for build-setting rules");
            }
        }
        let kind = match setting.kind {
            BuildSettingKind::Bool => AttrKind::Bool,
            BuildSettingKind::Int => AttrKind::Int,
            BuildSettingKind::String => AttrKind::String,
            BuildSettingKind::StringList => AttrKind::StringList,
        };
        for (name, spec) in [
            (
                "build_setting_default",
                descriptor(kind, None, true, false, false)?.0,
            ),
            (
                "help",
                descriptor(AttrKind::String, None, false, false, false)?.0,
            ),
        ] {
            budget.string(name).map_err(anyhow::Error::msg)?;
            budget.spec(&spec).map_err(anyhow::Error::msg)?;
            schema.insert(name.to_owned(), spec);
            attr_order.push(name.to_owned());
        }
    }
    // checkAndAddAllowlistIfNecessary for the function transition allowlist
    // (StarlarkRuleClassFunctions.java:1121-1132, 1173-1179, 1195-1243):
    // a declared `_allowlist_function_transition` must be a label attribute
    // whose default names `function_transition_allowlist` in package
    // `tools/allowlists/function_transition_allowlist` (any repository);
    // a rule with a Starlark transition gets it added when undeclared; a
    // rule without one must not declare it.
    let environment = eval
        .extra
        .and_then(|extra| extra.downcast_ref::<crate::loading::ExtensionContext>())
        .map_or_else(|| definition.clone(), |context| context.label.clone());
    function_transition_allowlist(
        &mut schema,
        &mut attr_order,
        !transitions.is_empty(),
        &environment,
        test,
    )?;
    // Subrule attributes are lifted to the rule class as if declared there,
    // after its own attributes (StarlarkRuleFunction.export,
    // StarlarkRuleClassFunctions.java:1940-1964), and their toolchain types
    // are requested by the rule before its own
    // (StarlarkRuleClassFunctions.java:1024-1027, 1064; RuleClass.Builder
    // keeps a LinkedHashSet, packages/RuleClass.java:743, 1587-1590).
    for (name, spec) in crate::subrule::lifted_attributes(&subrules, &mut budget)? {
        schema.insert(name.clone(), spec);
        attr_order.push(name);
    }
    let mut toolchains_with_subrules = crate::subrule::lifted_toolchains(&subrules, &mut budget)?;
    for toolchain in toolchains {
        if !toolchains_with_subrules.contains(&toolchain) {
            toolchains_with_subrules.push(toolchain);
        }
    }
    let toolchains = toolchains_with_subrules;
    let declaration = RuleDeclaration {
        definition_label: definition.to_string(),
        definition_environment_label: environment.to_string(),
        executable,
        test,
        analysis_test: args.analysis_test,
        skylark_testable: args.skylark_testable,
        output_to_genfiles: args.output_to_genfiles,
        dependency_resolution_rule: args.dependency_resolution_rule,
        fragments,
        host_fragments,
        toolchains,
        exec_groups,
        exec_compatible_with,
        cfg: rule_cfg,
        provides,
        implicit_outputs,
        extendable,
        definition_stack: crate::package::java_call_stack(eval),
    };
    budget
        .string(&declaration.definition_label)
        .and_then(|()| budget.string(&declaration.definition_environment_label))
        .map_err(anyhow::Error::msg)?;
    Ok(eval.heap().alloc(DefinitionGen {
        implementation,
        subrules,
        initializer,
        outputs_function,
        computed_functions,
        transitions,
        transition_keys,
        computed,
        schema,
        attr_order,
        build_setting,
        declaration,
        exported: OnceLock::new(),
    }))
}

/// The Starlark name of FunctionSplitTransitionAllowlist.ATTRIBUTE_NAME
/// (`$allowlist_function_transition`, packages/FunctionSplitTransitionAllowlist.java:20-25).
pub(crate) const FUNCTION_TRANSITION_ALLOWLIST_ATTRIBUTE: &str = "_allowlist_function_transition";

/// `env.getToolsLabel(FunctionSplitTransitionAllowlist.LABEL_STR)`: the
/// default allowlist, `@bazel_tools//tools/allowlists/function_transition_allowlist`
/// (a `package_group` with `packages = ["public"]`,
/// tools/allowlists/function_transition_allowlist/BUILD.tools).
pub(crate) const FUNCTION_TRANSITION_ALLOWLIST_DEFAULT: &str =
    "@@bazel_tools//tools/allowlists/function_transition_allowlist:function_transition_allowlist";

/// checkAndAddAllowlistIfNecessary (StarlarkRuleClassFunctions.java:1195-1243)
/// for the Starlark function transition allowlist (:1121-1132): a declared
/// `_allowlist_function_transition` must be a label attribute with a default
/// whose package and name are those of FunctionSplitTransitionAllowlist.LABEL;
/// a rule using a Starlark transition gets the attribute (cfg = exec, default
/// the `@bazel_tools` allowlist, :1173-1179) when it is not declared, and its
/// targets are checked (crate::engine); a declared allowlist without a
/// transition is `Unused function-based split transition allowlist: BZL TYPE`.
fn function_transition_allowlist(
    schema: &mut BTreeMap<String, AttrSpec>,
    attr_order: &mut Vec<String>,
    uses_transitions: bool,
    definition: &crate::label::Label,
    test: bool,
) -> anyhow::Result<()> {
    let name = FUNCTION_TRANSITION_ALLOWLIST_ATTRIBUTE;
    let declared = if let Some(spec) = schema.get(name) {
        if !matches!(spec.kind, AttrKind::Label) {
            bail!("{name} attribute must be a label type");
        }
        let AttrValue::Label(Some(default)) = &spec.default else {
            bail!("{name} attribute must have a default value");
        };
        let default = crate::label::Label::parse_canonical(default).map_err(anyhow::Error::msg)?;
        if default.package != "tools/allowlists/function_transition_allowlist"
            || default.name != "function_transition_allowlist"
        {
            bail!(
                "{name} attribute ({default}) does not have the expected value //tools/allowlists/function_transition_allowlist:function_transition_allowlist"
            );
        }
        true
    } else {
        false
    };
    if uses_transitions {
        if !declared {
            let mut spec = descriptor(AttrKind::Label, None, false, false, false)?.0;
            spec.default = AttrValue::Label(Some(FUNCTION_TRANSITION_ALLOWLIST_DEFAULT.to_owned()));
            spec.declaration.explicit_default = true;
            spec.declaration.cfg = crate::rule_declarations::Cfg::Exec(None);
            schema.insert(name.to_owned(), spec);
            attr_order.push(name.to_owned());
        }
    } else if declared {
        // RuleClass.Builder.getType: the RuleClassType enum name.
        bail!(
            "Unused function-based split transition allowlist: {definition} {}",
            if test { "TEST" } else { "NORMAL" }
        );
    }
    Ok(())
}

/// StarlarkFunction.getParameterNames (StarlarkCallbackHelper.java:58-60):
/// every parameter, in declaration order.
fn parameter_names(function: Value<'_>) -> anyhow::Result<Vec<String>> {
    let spec = function
        .parameters_spec()
        .ok_or_else(|| anyhow!("computed default must be a Starlark function"))?;
    let docs = spec.documentation(
        vec![starlark::typing::Ty::any(); spec.len()],
        std::collections::HashMap::new(),
    );
    Ok(docs
        .pos_only
        .iter()
        .chain(&docs.pos_or_named)
        .chain(&docs.args)
        .chain(&docs.named_only)
        .chain(&docs.kwargs)
        .map(|param| param.name.clone())
        .collect())
}

// The public Starlark rule signature has independently named Bazel options.
// Scope this allowance to the generated constructor and its argument wrappers.
#[allow(clippy::too_many_arguments)]
mod definitions {
    use super::*;
    #[starlark_module]
    pub(super) fn definition_globals(builder: &mut GlobalsBuilder) {
        fn rule<'v>(
            implementation: Value<'v>,
            #[starlark(require = named)] test: Option<bool>,
            #[starlark(require = named)] attrs: Option<Value<'v>>,
            #[starlark(require = named, default = NoneType)] outputs: Value<'v>,
            #[starlark(require = named)] executable: Option<bool>,
            #[starlark(require = named, default = false)] output_to_genfiles: bool,
            #[starlark(require = named, default = AllocList::EMPTY)] fragments: Value<'v>,
            #[starlark(require = named, default = AllocList::EMPTY)] host_fragments: Value<'v>,
            #[starlark(require = named, default = false)] _skylark_testable: bool,
            #[starlark(require = named, default = AllocList::EMPTY)] toolchains: Value<'v>,
            #[starlark(require = named)] doc: Option<&str>,
            #[starlark(require = named, default = AllocList::EMPTY)] provides: Value<'v>,
            #[starlark(require = named, default = false)] dependency_resolution_rule: bool,
            #[starlark(require = named, default = AllocList::EMPTY)] exec_compatible_with: Value<
                'v,
            >,
            #[starlark(require = named, default = false)] analysis_test: bool,
            #[starlark(require = named, default = NoneType)] build_setting: Value<'v>,
            #[starlark(require = named, default = NoneType)] cfg: Value<'v>,
            #[starlark(require = named, default = NoneType)] exec_groups: Value<'v>,
            #[starlark(require = named, default = NoneType)] initializer: Value<'v>,
            #[starlark(require = named, default = NoneType)] parent: Value<'v>,
            #[starlark(require = named, default = NoneType)] extendable: Value<'v>,
            #[starlark(require = named, default = AllocList::EMPTY)] subrules: Value<'v>,
            eval: &mut Evaluator<'v, '_, '_>,
        ) -> anyhow::Result<Value<'v>> {
            let _ = doc;
            define_rule(
                RuleArguments {
                    implementation,
                    test,
                    attrs,
                    outputs,
                    executable,
                    output_to_genfiles,
                    fragments,
                    host_fragments,
                    skylark_testable: _skylark_testable,
                    toolchains,
                    provides,
                    dependency_resolution_rule,
                    exec_compatible_with,
                    analysis_test,
                    build_setting,
                    cfg,
                    exec_groups,
                    initializer,
                    parent,
                    extendable,
                    subrules,
                },
                eval,
            )
        }
    }
}

/// Install only into .bzl globals; BUILD may invoke loaded definitions, not create them.
pub fn rule_globals(builder: &mut GlobalsBuilder) {
    definitions::definition_globals(builder);
    crate::starlark_types::analysis_globals(builder);
    // StarlarkAttrModuleApi extends StarlarkValue (@StarlarkBuiltin "attr"), so
    // its functions repr as methods (BuiltinFunction.java:192-199).
    let attrs = GlobalsBuilder::new()
        .with(|builder| builder.set_builtin_receiver("attr"))
        .with(attr_globals)
        .build();
    builder.frozen_heap().add_reference(attrs.heap());
    let members = builder.alloc(AllocStruct(attrs.iter()));
    builder.set("attr", crate::starlark_types::AttrModule(members));
}

/// Type.BOOLEAN conversion: True/False, or the integers 0 and 1
/// (Type.java:409-428).
fn attr_bool(value: Value) -> Option<bool> {
    value.unpack_bool().or_else(|| match value.unpack_i32() {
        Some(0) => Some(false),
        Some(1) => Some(true),
        _ => None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use starlark::environment::{FrozenModule, Module};
    use starlark::syntax::{AstModule, Dialect};

    /// Type.toString (packages/Type.java, BuildType.java), e.g. for
    /// ImplicitOutputsFunction's `Attributes of type dict(label, string)
    /// cannot be used in an outputs substitution template`
    /// (packages/ImplicitOutputsFunction.java:395-398, N46).
    #[test]
    fn attribute_type_names_are_bazel_type_to_string() {
        for (kind, name) in [
            (AttrKind::LabelKeyedStringDict, "dict(label, string)"),
            (AttrKind::StringKeyedLabelDict, "dict(string, label)"),
            (AttrKind::LabelListDict, "dict(string, list(label))"),
            (AttrKind::StringListDict, "dict(string, list(string))"),
            (AttrKind::OutputList, "list(output)"),
        ] {
            assert_eq!(kind.type_name(), name);
        }
    }

    #[test]
    fn allowed_file_types_match_suffixes_and_join_english_lists() {
        let types = |list: &[&str]| {
            list.iter()
                .map(|text| (*text).to_owned())
                .collect::<Vec<_>>()
        };
        let go = types(&[".go", ".s"]);
        let go = AllowedFileTypes::Extensions(&go);
        // FileType.hasExtension off Windows: case-sensitive String.endsWith.
        assert!(go.matches("a.go") && go.matches("sub/b.s") && go.matches(".go"));
        assert!(!go.matches("a.GO") && !go.matches("a.go.txt") && !go.matches("a.gox"));
        let odd = types(&["tar.gz", "BUILD", "o"]);
        let odd = AllowedFileTypes::Extensions(&odd);
        // No leading dot needed: exact names and multi-dot suffixes match too.
        assert!(odd.matches("x.tar.gz") && odd.matches("BUILD") && odd.matches("foo"));
        assert!(!odd.matches("x.gz"));
        let empty: Vec<String> = Vec::new();
        assert!(!AllowedFileTypes::Extensions(&empty).matches("a.go"));
        let blank = types(&[""]);
        assert!(AllowedFileTypes::Extensions(&blank).matches("anything"));
        assert!(AllowedFileTypes::Any.matches("x") && !AllowedFileTypes::None.matches("x"));
        // FileTypeSet.toString / StringUtil.joinEnglishList (oxford comma).
        for (list, shown) in [
            (&[][..], "nothing"),
            (&[".go"][..], ".go"),
            (&[".go", ".s"][..], ".go or .s"),
            (&[".go", ".s", ".h"][..], ".go, .s, or .h"),
            (&[".go", ".s", ".h", ".c"][..], ".go, .s, .h, or .c"),
            (&[".s", ".go", ".s"][..], ".s, .go, or .s"),
        ] {
            let list = types(list);
            assert_eq!(AllowedFileTypes::Extensions(&list).to_string(), shown);
        }
        assert_eq!(AllowedFileTypes::Any.to_string(), "any files");
        assert_eq!(AllowedFileTypes::None.to_string(), "no files");
    }

    fn provider_extension(label: &str, source: &str) -> anyhow::Result<FrozenModule> {
        let globals = GlobalsBuilder::standard()
            .with(rule_globals)
            .with(crate::providers::globals)
            .with(crate::template_variable_info::install)
            .build();
        Module::with_temp_heap(|module| {
            let context = crate::loading::ExtensionContext {
                byte_strings: false,
                label: crate::label::Label::parse(label, "").unwrap(),
                repositories: std::sync::Arc::new(crate::repository::RepositorySet::new(
                    std::path::PathBuf::from("."),
                )),
            };
            let ast = AstModule::parse(label, source.to_owned(), &Dialect::Standard).unwrap();
            {
                let mut eval = Evaluator::new(&module);
                eval.extra = Some(&context);
                eval.eval_module(ast, &globals)
                    .map_err(|error| anyhow!("{error}"))?;
            }
            module.freeze().map_err(|error| anyhow!("{error:?}"))
        })
    }

    #[test]
    fn provider_constraints_normalize_sequences_and_preserve_export_identity() {
        let frozen = provider_extension(
            "//defs:rules.bzl",
            r#"
P = provider()
Alias = P
Q = provider()
T = platform_common.TemplateVariableInfo
items = [P, Alias, DefaultInfo, T, P]
x = attr.label(providers=items)
items.append(Q)
y = attr.label_list(providers=((P, P), [T, Q]))
a = attr.label()
b = attr.label(providers=())
c = attr.label(providers=[[], [P]])
d = attr.label_list(providers=((P,), ()))
"#,
        )
        .unwrap();
        let x = frozen.get("x").unwrap();
        let x = attribute_spec_ref(x.value()).unwrap();
        assert_eq!(x.required_providers.len(), 1);
        assert_eq!(
            x.required_providers[0]
                .iter()
                .map(|p| p.display_name.as_str())
                .collect::<Vec<_>>(),
            ["P", "DefaultInfo", "TemplateVariableInfo"]
        );
        assert_eq!(x.required_providers[0][0].id, "//defs:rules.bzl%P");
        let y = frozen.get("y").unwrap();
        let y = attribute_spec_ref(y.value()).unwrap();
        assert_eq!(
            y.required_providers
                .iter()
                .map(Vec::len)
                .collect::<Vec<_>>(),
            [1, 2]
        );
        for name in ["a", "b", "c", "d"] {
            let value = frozen.get(name).unwrap();
            let spec = attribute_spec_ref(value.value()).unwrap();
            assert!(spec.required_providers.is_empty());
            assert!(
                !serde_json::to_string(spec)
                    .unwrap()
                    .contains("required_providers")
            );
        }

        let other = provider_extension("//other:rules.bzl", "P = provider()\n").unwrap();
        Module::with_temp_heap(|module| {
            let first = frozen.get("P").unwrap();
            let second = other.get("P").unwrap();
            let first = module.heap().access_owned_frozen_value(&first);
            let second = module.heap().access_owned_frozen_value(&second);
            let clauses =
                required_providers(module.heap().alloc(vec![first, second, first])).unwrap();
            assert_eq!(clauses[0].len(), 2);
            assert_eq!(clauses[0][0].display_name, clauses[0][1].display_name);
            assert_ne!(clauses[0][0].id, clauses[0][1].id);
        });
    }

    #[test]
    fn provider_constraints_reject_malformed_and_unexported_even_after_empty_clause() {
        for kind in ["label", "label_list"] {
            for expression in [
                "None",
                "'legacy'",
                "[\"PackageSpecificationInfo\"]",
                "[['legacy']]",
                "[None]",
                "[True]",
                "[1]",
                "[len]",
                "[DefaultInfo()]",
                "[P()]",
                "[P, [DefaultInfo]]",
                "[[P], DefaultInfo]",
                "[[[P]]]",
                "[provider()]",
                "[[], [provider()]]",
                "[[], ['legacy']]",
                "[[], [[P]]]",
                "[[], P]",
                "{}",
            ] {
                let source = format!("P = provider()\nx = attr.{kind}(providers={expression})\n");
                let error = provider_extension("//defs:rules.bzl", &source)
                    .unwrap_err()
                    .to_string();
                assert!(error.contains("providers"), "{source}: {error}");
            }
        }
        for kind in [
            "string",
            "string_list",
            "string_list_dict",
            "bool",
            "int",
            "output",
            "output_list",
        ] {
            let error = definition_error(&format!("attr.{kind}(providers=[])"));
            assert!(error.contains("providers"), "{kind}: {error}");
        }
        assert!(
            definition_error("rule(implementation=impl, provides=[1])")
                .contains("at index 0 of provides, got element of type int, want Provider")
        );
    }

    #[test]
    fn provider_constraints_budget_input_before_deduplication_and_empty_normalization() {
        for expression in [
            "[DefaultInfo]*100000",
            "[[DefaultInfo]]*40000",
            "[[]]*100000",
            "[[]] + [[DefaultInfo]]*40000",
        ] {
            let error = definition_error(&format!("attr.label(providers={expression})"));
            assert!(
                error.contains("host preparation") && error.contains("limit"),
                "{expression}: {error}"
            );
        }
        let source = "P = provider()\nx = attr.label(providers=[[P]]*10000)\ndef impl(ctx):\n    return []\nr = rule(implementation=impl, attrs={name: x for name in ['a', 'b', 'c']})\n";
        let error = provider_extension("//defs:rules.bzl", source)
            .unwrap_err()
            .to_string();
        assert!(
            error.contains("host preparation") && error.contains("limit"),
            "{error}"
        );
        let label = format!("//{}:rules.bzl", "p".repeat(65536));
        let error = provider_extension(
            &label,
            "P = provider()\nx = attr.label(providers=[P]*129)\n",
        )
        .unwrap_err()
        .to_string();
        assert!(
            error.contains("host preparation") && error.contains("limit"),
            "{error}"
        );
        let error = definition_error(
            "attr.label(default='//:'+'x'*(7*1024*1024), providers=[[DefaultInfo]]*15000)",
        );
        assert!(
            error.contains("host preparation") && error.contains("limit"),
            "{error}"
        );
    }

    fn definition_error(expression: &str) -> String {
        let globals = GlobalsBuilder::standard()
            .with(rule_globals)
            .with(crate::build_settings::install)
            .build();
        Module::with_temp_heap(|module| {
            let source = format!("def impl(ctx):\n    return []\nx = {expression}");
            let ast = AstModule::parse("defs.bzl", source, &Dialect::Standard).unwrap();
            let context = crate::loading::ExtensionContext {
                byte_strings: false,
                label: crate::label::Label::parse("//definitions:defs.bzl", "").unwrap(),
                repositories: std::sync::Arc::new(crate::repository::RepositorySet::new(
                    std::path::PathBuf::from("."),
                )),
            };
            let mut eval = Evaluator::new(&module);
            eval.extra = Some(&context);
            eval.eval_module(ast, &globals).unwrap_err().to_string()
        })
    }

    #[test]
    fn string_list_dict_snapshots_ordered_defaults_without_freezing_callers() {
        let globals = GlobalsBuilder::standard().with(rule_globals).build();
        Module::with_temp_heap(|module| {
            let ast = AstModule::parse(
                "defs.bzl",
                r#"
items = ["first", "first"]
defaults = {"z": items, "a": ("last",), "empty": []}
x = attr.string_list_dict(False, default=defaults, mandatory=True, doc=None)
items.append("later")
defaults["new"] = ["later"]
y = attr.string_list_dict(allow_empty=False, doc="documented")
z = attr.string_list_dict()
"#
                .into(),
                &Dialect::Standard,
            )
            .unwrap();
            Evaluator::new(&module).eval_module(ast, &globals).unwrap();
            let spec = attribute_spec_ref(module.get("x").unwrap()).unwrap();
            assert_eq!(spec.kind, AttrKind::StringListDict);
            assert!(spec.non_empty && spec.mandatory);
            assert_eq!(
                spec.default,
                AttrValue::StringListDict(vec![
                    ("z".into(), vec!["first".into(), "first".into()]),
                    ("a".into(), vec!["last".into()]),
                    ("empty".into(), vec![]),
                ])
            );
            let empty = attribute_spec_ref(module.get("y").unwrap()).unwrap();
            assert!(empty.non_empty);
            assert_eq!(empty.default, AttrValue::StringListDict(vec![]));
            assert!(
                !attribute_spec_ref(module.get("z").unwrap())
                    .unwrap()
                    .non_empty
            );
            assert_eq!(
                DictRef::from_value(module.get("defaults").unwrap())
                    .unwrap()
                    .len(),
                4
            );
            assert_eq!(sequence_ref(module.get("items").unwrap()).unwrap().len(), 3);
        });
    }

    #[test]
    fn string_dict_snapshots_ordered_defaults_without_phase_guard() {
        let globals = GlobalsBuilder::standard().with(rule_globals).build();
        Module::with_temp_heap(|module| {
            let ast = AstModule::parse(
                "defs.bzl",
                r#"
defaults = {"z": "before", "a": "café☃", "literal": "@missing//pkg:target"}
x = attr.string_dict(False, default=defaults, mandatory=True, doc=None)
defaults["z"] = "after"
defaults["new"] = "later"
y = attr.string_dict(allow_empty=False, doc="documented")
z = attr.string_dict()
"#
                .into(),
                &Dialect::Standard,
            )
            .unwrap();
            // Like neighboring constructors, pure metadata does not need eval.extra.
            Evaluator::new(&module).eval_module(ast, &globals).unwrap();
            let spec = attribute_spec(module.get("x").unwrap()).unwrap();
            assert_eq!(spec.kind, AttrKind::StringDict);
            assert!(spec.non_empty && spec.mandatory);
            assert_eq!(
                spec.default,
                AttrValue::StringDict(vec![
                    ("z".into(), "before".into()),
                    ("a".into(), "café☃".into()),
                    ("literal".into(), "@missing//pkg:target".into()),
                ])
            );
            let empty = attribute_spec_ref(module.get("y").unwrap()).unwrap();
            assert!(empty.non_empty);
            assert_eq!(empty.default, AttrValue::StringDict(vec![]));
            assert!(
                !attribute_spec_ref(module.get("z").unwrap())
                    .unwrap()
                    .non_empty
            );
            assert_eq!(
                DictRef::from_value(module.get("defaults").unwrap())
                    .unwrap()
                    .len(),
                4
            );
            assert_eq!(
                serde_json::to_string(&empty.default).unwrap(),
                r#"{"StringDict":[]}"#
            );
            assert_eq!(
                serde_json::to_value(spec.clone()).unwrap(),
                serde_json::to_value(spec).unwrap()
            );
        });
        // Loaded constructor aliases and helpers may construct unused BUILD metadata.
        build_fixture("r = attr.string_dict", "unused = r(default={'z':'v'})").unwrap();
        build_fixture(
            "def r():\n    return attr.string_dict(default={'z':'v'})",
            "unused = r()",
        )
        .unwrap();
    }

    #[test]
    fn string_dict_rejects_invalid_shapes_and_parameters() {
        for default in [
            "None",
            "[]",
            "()",
            "'text'",
            "{1: 's'}",
            "{False: 's'}",
            "{'x': []}",
            "{'x': ()}",
            "{'x': 1}",
            "{'x': False}",
            "{'x': None}",
            "{'x': {}}",
            "{'x': Label('//:x')}",
            "{Label('//:x'): 's'}",
            "{'x': DefaultInfo()}",
        ] {
            let error = definition_error(&format!("attr.string_dict(default={default})"));
            assert!(
                error.contains("expected a dict of strings to strings"),
                "{default}: {error}"
            );
        }
        for value in ["None", "True", "False", "1"] {
            let error = definition_error(&format!("attr.string_dict(configurable={value})"));
            assert!(error.contains("configurable is unsupported"), "{error}");
        }
        for (expression, expected) in [
            ("attr.string_dict(doc=1)", "doc must be a string or None"),
            ("attr.string_dict(allow_empty=None)", "bool"),
            ("attr.string_dict(allow_empty=1)", "bool"),
            ("attr.string_dict(mandatory=1)", "bool"),
            ("attr.string_dict(True, {})", "positional"),
            ("attr.string_dict(values=[])", "values"),
        ] {
            let error = definition_error(expression);
            assert!(error.contains(expected), "{expression}: {error}");
        }
    }

    #[allow(unsafe_code)]
    mod string_dict_spoof {
        use super::*;
        use starlark::values::{
            NoSerialize, ProvidesStaticType, StarlarkPagablePanic, StarlarkValue, starlark_value,
        };
        #[derive(Debug, ProvidesStaticType, NoSerialize, Allocative, StarlarkPagablePanic)]
        pub(super) struct FakeString;
        starlark::starlark_simple_value!(FakeString);
        impl fmt::Display for FakeString {
            fn fmt(&self, _: &mut fmt::Formatter<'_>) -> fmt::Result {
                panic!("attribute preflight must not format arbitrary values")
            }
        }
        #[starlark_value(type = "string")]
        impl<'v> StarlarkValue<'v> for FakeString {
            fn dir_attr(&self) -> Vec<String> {
                panic!("attribute preflight must not reflect arbitrary values")
            }
            fn get_attr(&self, _: &str, _: starlark::values::Heap<'v>) -> Option<Value<'v>> {
                panic!("attribute preflight must not call arbitrary attributes")
            }
        }
    }

    #[test]
    fn string_dict_preflight_rejects_spoofed_string_type_without_reflection() {
        Module::with_temp_heap(|module| {
            let fake = module.heap().alloc(string_dict_spoof::FakeString);
            assert_eq!(fake.get_type(), "string");
            let value = module
                .heap()
                .alloc(starlark::values::dict::AllocDict([("k", fake)]));
            let error = AttrKind::StringDict.preflight_default(value).unwrap_err();
            assert_eq!(error.to_string(), "expected a dict of strings to strings");
            assert!(AttrKind::StringDict.preflight_default(fake).is_err());
        });
    }

    #[test]
    fn string_dict_preflight_bounds_shared_payloads_before_copying() {
        for expression in [
            "attr.string_dict(default={'x'*(9*1024*1024): ''})",
            "attr.string_dict(default={'x': 'x'*(9*1024*1024)})",
            "attr.string_dict(default={str(i): 'x'*(1024*1024) for i in range(9)})",
            "attr.string_dict(default={str(i): '' for i in range(50000)})",
            "attr.string_dict(default={'x': 'x'*(3*1024*1024)})\nr = rule(implementation=impl, attrs={name:x for name in ['a','b','c']})",
        ] {
            let error = definition_error(expression);
            assert!(
                error.contains("host preparation") && error.contains("limit"),
                "{error}"
            );
        }
        // Validate the entire shape before reporting a resource error, without printing keys.
        for default in [
            "{'x': 'x'*(9*1024*1024), 'bad': []}",
            "{'x'*(9*1024*1024): []}",
        ] {
            let error = definition_error(&format!("attr.string_dict(default={default})"));
            assert!(
                error.contains("expected a dict of strings to strings"),
                "{error}"
            );
            assert!(!error.contains("host preparation"), "{error}");
            assert!(error.len() < 4096);
        }
        for (definition, call) in [
            (
                "r = rule(implementation=impl, attrs={'x':attr.string_dict()})",
                "s='x'*(1024*1024)\nr(name='x', x={str(i):s for i in range(9)})",
            ),
            (
                "r = rule(implementation=impl, attrs={'a':attr.string_dict(), 'b':attr.string_dict()})",
                "s='x'*(5*1024*1024)\nr(name='x', a={'k':s}, b={'k':s})",
            ),
            (
                "r = rule(implementation=impl, attrs={'x':attr.string_dict(default={'k':'x'*(5*1024*1024)})})",
                "r(name='x')",
            ),
        ] {
            assert!(
                build_fixture(definition, call)
                    .unwrap_err()
                    .contains("host preparation")
            );
        }
    }

    #[test]
    fn string_dict_build_snapshots_none_mandatory_and_literal_values() {
        let definition = "d={'z':'before', 'a':'alpha'}\nspec=attr.string_dict(default=d)\nd['z']='between'\nr=rule(implementation=impl, attrs={'x':spec})\nd['z']='after'";
        let package = build_fixture(definition, "d={'z':'café☃', 'a':'@missing//p:t'}\nr(name='x', x=d)\nd['z']='late'\nd['new']='later'\nr(name='none', x=None)").unwrap();
        let rule = &package[&crate::label::Label::parse("//:x", "").unwrap()];
        assert!(rule.srcs.is_empty());
        assert_eq!(
            rule.custom.as_ref().unwrap().attrs["x"],
            AttrValue::StringDict(vec![
                ("z".into(), "café☃".into()),
                ("a".into(), "@missing//p:t".into()),
            ])
        );
        assert_eq!(
            package[&crate::label::Label::parse("//:none", "").unwrap()]
                .custom
                .as_ref()
                .unwrap()
                .attrs["x"],
            AttrValue::StringDict(vec![
                ("z".into(), "before".into()),
                ("a".into(), "alpha".into())
            ])
        );
        let required = "r=rule(implementation=impl, attrs={'x':attr.string_dict(default={'k':'v'}, mandatory=True)})";
        for call in ["r(name='x')", "r(name='x', x=None)"] {
            assert!(
                build_fixture(required, call)
                    .unwrap_err()
                    .contains("missing mandatory attribute")
            );
        }
        build_fixture(required, "r(name='x', x={})").unwrap();
        for input in [
            "{1:'s'}",
            "{'x':[]}",
            "{'x':()}",
            "{'x':False}",
            "{'x':None}",
            "{'x':{}}",
        ] {
            assert!(
                build_fixture(definition, &format!("r(name='x', x={input})"))
                    .unwrap_err()
                    .contains("expected a dict of strings to strings")
            );
        }
    }

    #[test]
    fn string_list_dict_rejects_invalid_shapes_and_present_configurable() {
        for expression in [
            "attr.string_list_dict(default=None)",
            "attr.string_list_dict(default=[])",
            "attr.string_list_dict(default={1: []})",
            "attr.string_list_dict(default={'x': 'text'})",
            "attr.string_list_dict(default={'x': {}})",
            "attr.string_list_dict(default={'x': [1]})",
            "attr.string_list_dict(default={'x': (True,)})",
            "attr.string_list_dict(default={'x': [Label('//:x')]})",
            "attr.string_list_dict(default={Label('//:x'): []})",
        ] {
            let error = definition_error(expression);
            assert!(
                error.contains("expected a dict of strings to a list of strings"),
                "{expression}: {error}"
            );
        }
        for value in ["None", "True", "False", "1"] {
            let error = definition_error(&format!("attr.string_list_dict(configurable={value})"));
            assert!(error.contains("configurable is unsupported"), "{error}");
        }
        for (expression, expected) in [
            (
                "attr.string_list_dict(doc=1)",
                "doc must be a string or None",
            ),
            ("attr.string_list_dict(allow_empty=None)", "bool"),
            ("attr.string_list_dict(mandatory=1)", "bool"),
            ("attr.string_list_dict(True, {})", "positional"),
        ] {
            let error = definition_error(expression);
            assert!(error.contains(expected), "{expression}: {error}");
        }
    }

    #[test]
    fn string_list_dict_preflight_counts_keys_sequences_and_shared_strings() {
        for expression in [
            "attr.string_list_dict(default={'x'*(9*1024*1024): []})",
            "attr.string_list_dict(default={'x': ['x'*(1024*1024)]*9})",
            "attr.string_list_dict(default={'x': ('x'*(1024*1024),)*9})",
            "attr.string_list_dict(default={str(i): [] for i in range(50000)})",
            "attr.string_list_dict(default={'x': ['']*100000})",
        ] {
            let error = definition_error(expression);
            assert!(
                error.contains("host preparation") && error.contains("limit"),
                "{expression}: {error}"
            );
        }
        let error = definition_error(
            "attr.string_list_dict(default={'x': ['x'*(1024*1024)]*9, 'bad': [1]})",
        );
        assert!(error.contains("list of strings"), "{error}");
        assert!(!error.contains("host preparation"), "{error}");
    }

    #[test]
    fn attribute_sequences_accept_tuples_and_keep_strict_elements() {
        let globals = GlobalsBuilder::standard().with(rule_globals).build();
        Module::with_temp_heap(|module| {
            let ast = AstModule::parse(
                "defs.bzl",
                "ss = ('a', 'a')\nlabels = ('//:a', Label('//:b'))\nbad = (1,)".into(),
                &Dialect::Standard,
            )
            .unwrap();
            Evaluator::new(&module).eval_module(ast, &globals).unwrap();
            for (kind, name, expected) in [
                (
                    AttrKind::StringList,
                    "ss",
                    AttrValue::Strings(vec!["a".into(), "a".into()]),
                ),
                (
                    AttrKind::OutputList,
                    "ss",
                    AttrValue::Outputs(vec!["a".into(), "a".into()]),
                ),
                (
                    AttrKind::LabelList,
                    "labels",
                    AttrValue::Labels(vec!["//:a".into(), "//:b".into()]),
                ),
            ] {
                let value = module.get(name).unwrap();
                kind.preflight_default(value).unwrap();
                assert_eq!(kind.parse(value, None).unwrap(), expected);
                assert!(kind.preflight_default(module.get("bad").unwrap()).is_err());
            }
        });
        for expression in [
            "attr.string_list(default=('x'*(1024*1024),)*9)",
            "attr.label_list(default=('//:'+'x'*65536,)*129)",
        ] {
            assert!(definition_error(expression).contains("host preparation"));
        }
    }

    #[test]
    fn non_empty_metadata_preserves_existing_schema_serialization() {
        let spec = descriptor(AttrKind::String, None, false, false, false)
            .unwrap()
            .0;
        assert_eq!(
            serde_json::to_string(&spec).unwrap(),
            r#"{"kind":"String","default":{"String":""},"mandatory":false,"allow_files":false,"allow_single_file":false}"#
        );
        let mut spec = descriptor(AttrKind::StringListDict, None, false, false, false)
            .unwrap()
            .0;
        spec.default = AttrValue::StringListDict(vec![
            ("z".into(), vec!["b".into(), "a".into()]),
            ("a".into(), vec![]),
        ]);
        assert!(!serde_json::to_string(&spec).unwrap().contains("non_empty"));
        spec.non_empty = true;
        assert_eq!(
            serde_json::to_string(&spec).unwrap(),
            r#"{"kind":"StringListDict","default":{"StringListDict":[["z",["b","a"]],["a",[]]]},"mandatory":false,"allow_files":false,"allow_single_file":false,"non_empty":true}"#
        );
    }

    #[test]
    fn rule_schema_preflight_charges_repeated_descriptor_aliases() {
        let error = definition_error(
            "attr.string_list_dict(default={'x': ['x'*(3*1024*1024)]})\nr = rule(implementation=impl, attrs={name: x for name in ['a','b','c']})",
        );
        assert!(
            error.contains("host preparation") && error.contains("limit"),
            "{error}"
        );
    }

    fn build_fixture(definition: &str, call: &str) -> Result<crate::package::Package, String> {
        let root = tempfile::Builder::new()
            .prefix("attribute-budget-")
            .tempdir_in("/tmp/opencode")
            .unwrap();
        std::fs::write(
            root.path().join("defs.bzl"),
            format!("def impl(ctx):\n    return []\n{definition}\n"),
        )
        .unwrap();
        std::fs::write(
            root.path().join("BUILD.bazel"),
            format!("load(':defs.bzl', 'r')\n{call}\n"),
        )
        .unwrap();
        crate::package::evaluate(root.path(), "")
    }

    #[test]
    fn build_attribute_preflight_aggregates_schema_explicit_values_and_defaults() {
        for (definition, call) in [
            (
                "r = rule(implementation=impl, attrs={'x': attr.string_list_dict()})",
                "r(name='x', x={'k': ['x'*(1024*1024)]*9})",
            ),
            (
                "r = rule(implementation=impl, attrs={'a': attr.string_list_dict(), 'b': attr.string_list_dict()})",
                "r(name='x', a={'k': ['x'*(5*1024*1024)]}, b={'k': ['x'*(5*1024*1024)]})",
            ),
            (
                "r = rule(implementation=impl, attrs={'x': attr.string_list_dict(default={'k': ['x'*(5*1024*1024)]})})",
                "r(name='x')",
            ),
            (
                "r = rule(implementation=impl, attrs={'x': attr.string_list_dict(default={'k': ['x'*(5*1024*1024)]})})",
                "r(name='x', x={'k': ['x'*(4*1024*1024)]})",
            ),
        ] {
            let error = build_fixture(definition, call).unwrap_err();
            assert!(
                error.contains("host preparation") && error.contains("limit"),
                "{error}"
            );
        }
        build_fixture("r = rule(implementation=impl, attrs={'x': attr.string_list_dict(default={'k': ['x'*(5*1024*1024)]})})", "r(name='x', x={})").unwrap();
    }

    #[test]
    fn build_string_list_dict_snapshots_values_and_preserves_none_mandatory_semantics() {
        let definition = "r = rule(implementation=impl, attrs={'x': attr.string_list_dict(default={'default': ['value']})})";
        let package = build_fixture(definition, "items=['a']\nd={'z': items, 'a': ('b',)}\nr(name='x', x=d)\nitems.append('late')\nd['new']=[]\nr(name='none', x=None)").unwrap();
        assert_eq!(
            package[&crate::label::Label::parse("//:x", "").unwrap()]
                .custom
                .as_ref()
                .unwrap()
                .attrs["x"],
            AttrValue::StringListDict(vec![
                ("z".into(), vec!["a".into()]),
                ("a".into(), vec!["b".into()])
            ])
        );
        assert_eq!(
            package[&crate::label::Label::parse("//:none", "").unwrap()]
                .custom
                .as_ref()
                .unwrap()
                .attrs["x"],
            AttrValue::StringListDict(vec![("default".into(), vec!["value".into()])])
        );
        let definition =
            "r = rule(implementation=impl, attrs={'x': attr.string_list_dict(mandatory=True)})";
        for call in ["r(name='x')", "r(name='x', x=None)"] {
            assert!(
                build_fixture(definition, call)
                    .unwrap_err()
                    .contains("missing mandatory attribute")
            );
        }
        build_fixture(definition, "r(name='x', x={})").unwrap();
    }

    #[test]
    fn descriptor_preflight_counts_shared_payloads_and_preserves_type_errors() {
        for expression in [
            "attr.string_list(default=['x'*(1024*1024)]*9)",
            "attr.label_list(default=[Label('//:'+'x'*65536)]*129)",
            "attr.label_list(default=['//:'+'x'*65536]*129)",
            "attr.string_list(default=['']*100000)",
        ] {
            let error = definition_error(expression);
            assert!(
                error.contains("host preparation") && error.contains("limit"),
                "{error}"
            );
        }
        for (expression, expected) in [
            (
                "attr.string_list(default=['x'*(1024*1024)]*9+[1])",
                "expected a list of strings",
            ),
            (
                "attr.label_list(default=['//:'+'x'*65536]*129+[1])",
                "expected a list of strings or Labels",
            ),
            ("attr.string(default=None)", "expected a string"),
            (
                "attr.label_list(default=None)",
                "in call to label_list(), parameter 'default' got value of type 'NoneType', want 'sequence or function'",
            ),
        ] {
            let error = definition_error(expression);
            assert!(error.contains(expected), "{error}");
            assert!(!error.contains("host preparation"), "{error}");
        }
    }

    #[test]
    fn descriptor_normalization_charges_mapped_labels_cumulatively() {
        let root = tempfile::Builder::new()
            .prefix("descriptor-mapping-")
            .tempdir_in("/tmp/opencode")
            .unwrap();
        let symbol = "e".repeat(65_536);
        std::fs::write(root.path().join("MODULE.bazel"), format!(
            "module(name='main')\ne=use_extension('//:ext.bzl','{symbol}')\nuse_repo(e,chosen='made')\n"
        )).unwrap();
        let context = crate::loading::ExtensionContext {
            byte_strings: false,
            label: crate::label::Label::parse("//:ext.bzl", "").unwrap(),
            repositories: std::sync::Arc::new(crate::repository::RepositorySet::new(
                root.path().to_owned(),
            )),
        };
        let globals = GlobalsBuilder::standard().with(rule_globals).build();
        for count in [1, 200] {
            Module::with_temp_heap(|module| {
                let ast = AstModule::parse(
                    "//:ext.bzl",
                    format!("x=attr.label_list(default=['@chosen//:data']*{count})"),
                    &Dialect::Standard,
                )
                .unwrap();
                let mut eval = Evaluator::new(&module);
                eval.extra = Some(&context);
                let result = eval.eval_module(ast, &globals);
                if count == 1 {
                    result.unwrap();
                    let value = module.get("x").unwrap();
                    assert_eq!(
                        attribute_spec_ref(value).unwrap().default,
                        AttrValue::Labels(vec![format!("@@+{symbol}+made//:data")])
                    );
                } else {
                    let error = result.unwrap_err().to_string();
                    assert!(
                        error.contains("host preparation") && error.contains("limit"),
                        "{error}"
                    );
                }
            });
        }
    }

    #[test]
    fn invalid_descriptors_and_rule_options_are_explicit_errors() {
        for (expression, expected) in [
            ("attr.string(default=1)", "string"),
            ("attr.string_list(default=[1])", "list of strings"),
            ("attr.int(default=True)", "int"),
            // Boolean parameter, not Type.BOOLEAN (genuine Bazel 9.3.0 rejects 0 and 1).
            (
                "attr.bool(default=1)",
                "in call to bool(), parameter 'default' got value of type 'int', want 'bool'",
            ),
            (
                "attr.bool(default=0)",
                "in call to bool(), parameter 'default' got value of type 'int', want 'bool'",
            ),
            (
                "attr.bool(default=None)",
                "in call to bool(), parameter 'default' got value of type 'NoneType', want 'bool'",
            ),
            // rule()'s bool options are Boolean parameters: genuine Bazel 9.3.0
            // rejects 0/1 ("in call to rule(), parameter 'executable' got value of
            // type 'int', want 'bool'"); native rejects through the typed signature
            // with Bazel's text since N33.
            (
                "rule(implementation=impl, executable=1)",
                "in call to rule(), parameter 'executable' got value of type 'int', want 'bool'",
            ),
            (
                "rule(implementation=impl, test=1)",
                "in call to rule(), parameter 'test' got value of type 'int', want 'bool'",
            ),
            (
                "attr.label(cfg='bogus')",
                "cfg must be either 'target', 'exec' or a starlark defined transition defined by the exec() or transition() functions.",
            ),
            (
                "attr.label_list(cfg=1)",
                "cfg must be either 'target', 'exec'",
            ),
            (
                "attr.label(allow_files=1)",
                "in call to label(), parameter 'allow_files' got value of type 'int', want 'bool, sequence, or NoneType'",
            ),
            (
                "attr.label(allow_single_file=1)",
                "allow_single_file should be a boolean or a string list",
            ),
            (
                "attr.label(allow_single_file=[1])",
                "at index 0 of allow_files argument, got element of type int, want string",
            ),
            ("attr.string(unknown=True)", "unknown"),
            ("attr.output(default='x')", "default"),
            ("attr.output_list(default=['x'])", "default"),
            ("rule(implementation=1)", "callable"),
            (
                "rule(implementation=impl, test=True)",
                "Invalid rule class name 'x', test rule class names must end with '_test' and other rule classes must not",
            ),
            (
                "rule(implementation=impl, analysis_test=True)",
                "Invalid rule class name 'x', test rule class names must end with '_test'",
            ),
            ("rule(implementation=impl, unknown=True)", "unknown"),
            (
                "rule(implementation=impl, build_setting=True)",
                "BuildSetting, got bool",
            ),
            (
                "rule(implementation=impl, build_setting='string')",
                "BuildSetting, got string",
            ),
            (
                "rule(implementation=impl, build_setting=attr.bool())",
                "BuildSetting, got Attribute",
            ),
            (
                "rule(implementation=impl, cfg='target')",
                "`cfg` must be set to a transition object initialized by the transition() function.",
            ),
            (
                "rule(implementation=impl, build_setting=config.bool(), cfg='target')",
                "Build setting rules cannot use the `cfg` param to apply transitions to themselves.",
            ),
            (
                "rule(implementation=impl, build_setting=config.string(), attrs={'help': attr.string()})",
                "reserved for build-setting rules",
            ),
            (
                "rule(implementation=impl, build_setting=config.bool(), attrs={'build_setting_default': attr.bool()})",
                "reserved for build-setting rules",
            ),
            ("rule(implementation=impl, attrs={'x': 1})", "descriptor"),
            (
                "rule(implementation=impl, attrs={'name': attr.string()})",
                "attribute `name`: built-in attributes cannot be overridden.",
            ),
            (
                "rule(implementation=impl, attrs={'visibility': attr.string()})",
                "attribute `visibility`: built-in attributes cannot be overridden.",
            ),
            (
                "rule(implementation=impl, attrs={'testonly': attr.bool()})",
                "attribute `testonly`: built-in attributes cannot be overridden.",
            ),
            (
                "rule(implementation=impl, attrs={'tags': attr.string_list()})",
                "attribute `tags`: built-in attributes cannot be overridden.",
            ),
            (
                "rule(implementation=impl, attrs={'aspect_hints': attr.label_list()})",
                "attribute `aspect_hints`: built-in attributes cannot be overridden.",
            ),
            (
                "rule(implementation=impl, attrs={'expect_failure': attr.string()})",
                "attribute `expect_failure`: built-in attributes cannot be overridden.",
            ),
            (
                "rule(implementation=impl, attrs={'exec_properties': attr.string_dict()})",
                "attribute `exec_properties`: built-in attributes cannot be overridden.",
            ),
            (
                "rule(implementation=impl, attrs={'_config_dependencies': attr.label_list()})",
                "attribute `_config_dependencies`: built-in attributes cannot be overridden.",
            ),
            (
                "rule(implementation=impl, executable=True, attrs={'output_licenses': attr.string_list()})",
                "attribute `output_licenses`: built-in attributes cannot be overridden.",
            ),
            (
                "rule(implementation=impl, test=True, attrs={'_is_executable': attr.bool()})",
                "attribute `_is_executable`: built-in attributes cannot be overridden.",
            ),
            (
                "rule(implementation=impl, test=True, attrs={'flaky': attr.bool()})",
                "attribute `flaky`: built-in attributes cannot be overridden.",
            ),
            // Every name is validated before any attribute is added.
            (
                "rule(implementation=impl, attrs={'tags': attr.string_list(), 'bad-name': attr.string()})",
                "invalid attribute name",
            ),
            (
                "rule(implementation=impl, attrs={'bad-name': attr.string()})",
                "invalid attribute name",
            ),
            (
                "rule(implementation=impl, attrs={'_tool': attr.label(default=':tool', mandatory=True)})",
                "private attribute",
            ),
        ] {
            let error = definition_error(expression);
            assert!(
                error.contains(expected),
                "{expression}: expected {expected:?}: {error}"
            );
        }
    }

    #[test]
    fn rule_attribute_names_outside_the_builtin_set_are_free() {
        // licenses/distribs belong to NativeBuildRule only; test attributes
        // only to test rules; dependency resolution rules drop the constraint
        // attributes (StarlarkRuleClassFunctions.java:159-241).
        build_fixture(
            "r = rule(implementation=impl, attrs={'licenses': attr.string_list(), 'distribs': attr.string_list(), 'size': attr.int(), 'args': attr.string_list(), '_tool': attr.label_list()})",
            "r(name='x', licenses=['own'], size=3, args=['a'])",
        )
        .unwrap();
        // A STRING size/timeout makes Bazel 9.3 crash at instantiation.
        for key in ["size", "timeout"] {
            let error = build_fixture(
                &format!("r = rule(implementation=impl, attrs={{'{key}': attr.string()}})"),
                "r(name='x')",
            )
            .unwrap_err();
            assert!(
                error.contains(&format!(
                    "//:x: Starlark attribute '{key}' of type string is not supported by the native profile"
                )),
                "{error}"
            );
        }
        build_fixture(
            "r = rule(implementation=impl, dependency_resolution_rule=True, attrs={'toolchains': attr.label_list(for_dependency_resolution=True), 'aspect_hints': attr.label_list(for_dependency_resolution=True)})",
            "r(name='x')",
        )
        .unwrap();
        let error = build_fixture(
            "r = rule(implementation=impl, dependency_resolution_rule=True)",
            "r(name='x', target_compatible_with=[])",
        )
        .unwrap_err();
        assert!(
            error.contains("//:x: no such attribute 'target_compatible_with' in 'r' rule"),
            "{error}"
        );
        // The renamed key is reported (AttributeProvider.java:213-235).
        let error = build_fixture(
            "r = rule(implementation=impl, dependency_resolution_rule=True)",
            "r(name='x', applicable_licenses=[])",
        )
        .unwrap_err();
        assert!(
            error.contains("//:x: no such attribute 'package_metadata' in 'r' rule\n"),
            "{error}"
        );
        // Public-name clashes with private built-ins fail at export.
        let error = build_fixture(
            "r = rule(implementation=impl, attrs={'_action_listener': attr.label_list()})",
            "r(name='x')",
        )
        .unwrap_err();
        assert!(
            error.contains(
                "Rule r: Attributes $action_listener and :action_listener have an identical public name: _action_listener"
            ),
            "{error}"
        );
    }

    #[test]
    fn builtin_attributes_are_converted_retained_and_finalized() {
        let package = build_fixture(
            "r = rule(implementation=impl, attrs={'licenses': attr.string_list()})",
            "package(default_deprecation='pkg', default_testonly=True)\n\
r(name='x', tags=('a', 'manual'), testonly=1, applicable_licenses=[':lic'], target_compatible_with=select({':c': [':p'], '//conditions:default': []}), transitive_configs=[':t'], features=None, licenses=['own'], exec_group_compatible_with={'g': [':e']})\n\
r(name='y', deprecation='own', testonly=False, features=select({'//conditions:default': ['f']}))\n\
r(name='z')",
        )
        .unwrap();
        let rule = |name: &str| &package[&crate::label::Label::parse(name, "").unwrap()];
        let x = rule(":x");
        let custom = x.custom.as_ref().unwrap();
        let builtin = &custom.declaration.builtin;
        assert_eq!(builtin.target.rule_class, "r");
        assert_eq!(builtin.target.tags, ["a", "manual"]);
        assert!(x.is_manual());
        assert_eq!(builtin.target.attrs["testonly"], AttrValue::Bool(true));
        assert_eq!(
            builtin.target.attrs["package_metadata"],
            AttrValue::Labels(vec!["//:lic".into()])
        );
        assert!(!builtin.target.attrs.contains_key("features"));
        assert!(
            builtin
                .target
                .configurable
                .contains_key("target_compatible_with")
        );
        // NODEP transitive_configs values are not assumed input files.
        let inputs = builtin
            .target
            .input_labels
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>();
        assert_eq!(inputs, ["//:e", "//:lic", "//:c", "//:p"]);
        assert!(builtin.testonly);
        assert_eq!(builtin.deprecation.as_deref(), Some("pkg"));
        assert_eq!(
            custom.attrs["licenses"],
            AttrValue::Strings(vec!["own".into()])
        );
        // Value-dependent gaps are evaluated on resolved values; the select
        // itself is retained for configured attribute resolution.
        assert!(custom.declaration.unanalyzed.is_empty());
        assert!(
            builtin
                .target
                .configurable
                .contains_key("target_compatible_with")
        );
        assert!(!custom.attrs.contains_key("target_compatible_with"));
        let gaps = value_gaps(custom).join("; ");
        let expected = "attribute 'transitive_configs' = [\"//:t\"] (configuration trimming)";
        assert!(gaps.contains(expected), "{expected}: {gaps}");
        // package_metadata edges are analyzed (N28c, crate::engine).
        assert!(!gaps.contains("package_metadata"), "{gaps}");
        // exec_group_compatible_with is resolved natively (docs/EXEC_GROUPS.md).
        assert!(!gaps.contains("exec_group_compatible_with"), "{gaps}");
        let y = rule(":y").custom.as_ref().unwrap();
        assert_eq!(y.declaration.builtin.deprecation.as_deref(), Some("own"));
        assert!(!y.declaration.builtin.testonly);
        assert_eq!(
            y.declaration.builtin.target.attrs["features"],
            AttrValue::Strings(vec!["f".into()])
        );
        assert!(
            y.declaration.unanalyzed.is_empty() && value_gaps(y).is_empty(),
            "{:?}",
            y.declaration.unanalyzed
        );
        let z = rule(":z").custom.as_ref().unwrap();
        assert!(z.declaration.builtin.testonly);
        assert!(z.declaration.builtin.target.tags.is_empty());
        assert!(!rule(":z").is_manual());
    }

    #[test]
    fn relative_defaults_and_rules_require_extension_context() {
        let globals = GlobalsBuilder::standard().with(rule_globals).build();
        for (source, expected) in [
            ("x = attr.label(default=':relative')", "definition context"),
            (
                "x = attr.label_list(default=['relative'])",
                "definition context",
            ),
            (
                "def impl(ctx):\n    return []\nx = rule(implementation=impl)",
                "extension initialization",
            ),
        ] {
            Module::with_temp_heap(|module| {
                let ast =
                    AstModule::parse("//pkg:defs.bzl", source.into(), &Dialect::Standard).unwrap();
                let error = Evaluator::new(&module)
                    .eval_module(ast, &globals)
                    .unwrap_err()
                    .to_string();
                assert!(error.contains(expected), "{error}");
            });
        }
    }

    #[test]
    fn relative_defaults_use_context_fallback_without_filesystem_lookups() {
        let globals = GlobalsBuilder::standard().with(rule_globals).build();
        for (extension, expected) in [
            ("//:defs.bzl", "//:target"),
            ("//does/not/exist:defs.bzl", "//does/not/exist:target"),
        ] {
            Module::with_temp_heap(|module| {
                let context = crate::loading::ExtensionContext {
                    byte_strings: false,
                    label: crate::label::Label::parse(extension, "").unwrap(),
                    repositories: std::sync::Arc::new(crate::repository::RepositorySet::new(
                        std::path::PathBuf::from("."),
                    )),
                };
                let ast = AstModule::parse(
                    "defs.bzl",
                    "x = attr.label(default=':target')".into(),
                    &Dialect::Standard,
                )
                .unwrap();
                let mut eval = Evaluator::new(&module);
                eval.extra = Some(&context);
                eval.eval_module(ast, &globals).unwrap();
                let descriptor = module
                    .get("x")
                    .unwrap()
                    .downcast_ref::<Descriptor>()
                    .unwrap();
                assert_eq!(
                    descriptor.0.default,
                    AttrValue::Label(Some(expected.into()))
                );
            });
        }
    }
}
