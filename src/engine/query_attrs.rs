//! The loading-phase attributes of a target as `bazel query` sees them:
//! rule-class attribute order, explicitness, label edges (including every
//! `select()` key and branch), implicit and tool dependencies.
//!
//! Sources (relative to the pinned `src/main/java/com/google/devtools/build/lib/`):
//! AggregatingAttributeMapper.visitLabels (packages/AggregatingAttributeMapper.java:75-219),
//! LabelVisitationUtils.visitTarget (packages/LabelVisitationUtils.java),
//! DependencyFilter (packages/DependencyFilter.java), the Starlark base rule
//! classes (analysis/starlark/StarlarkRuleClassFunctions.java:159-378) and
//! the native rule classes of crate::native_rules.

use crate::custom_rule::{AttrKind, AttrValue, ConfigurablePart};
use crate::label::Label;
use crate::native_rules::{NativeType, StarlarkRuleKind};
use crate::package::{Package, Rule, RuleKind};

/// The value shape of an attribute.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Shape {
    Label,
    LabelList,
    /// `dict(label, string)`: keys are labels.
    LabelKeyedStringDict,
    /// `dict(string, label)`: values are labels.
    StringKeyedLabelDict,
    /// `dict(string, list(label))`.
    LabelListDict,
    /// NODEP_LABEL / NODEP_LABEL_LIST: never a dependency edge
    /// (AggregatingAttributeMapper.java:101-105).
    Nodep,
    Output,
    Bool,
    TriState,
    Int,
    Other,
}

impl Shape {
    pub(crate) fn is_dependency(self) -> bool {
        matches!(
            self,
            Self::Label
                | Self::LabelList
                | Self::LabelKeyedStringDict
                | Self::StringKeyedLabelDict
                | Self::LabelListDict
        )
    }

    fn of_native(ty: NativeType) -> Self {
        match ty {
            NativeType::Label => Self::Label,
            NativeType::LabelList => Self::LabelList,
            NativeType::LabelKeyedStringDict => Self::LabelKeyedStringDict,
            NativeType::LabelListDict => Self::LabelListDict,
            NativeType::NodepLabel | NativeType::NodepLabelList => Self::Nodep,
            NativeType::OutputList => Self::Output,
            NativeType::Bool => Self::Bool,
            NativeType::TriState => Self::TriState,
            NativeType::Int => Self::Int,
            _ => Self::Other,
        }
    }

    fn of_starlark(kind: &AttrKind) -> Self {
        match kind {
            AttrKind::Label => Self::Label,
            AttrKind::LabelList => Self::LabelList,
            AttrKind::LabelKeyedStringDict => Self::LabelKeyedStringDict,
            AttrKind::StringKeyedLabelDict => Self::StringKeyedLabelDict,
            AttrKind::LabelListDict => Self::LabelListDict,
            AttrKind::Output | AttrKind::OutputList => Self::Output,
            AttrKind::Bool => Self::Bool,
            AttrKind::Int => Self::Int,
            _ => Self::Other,
        }
    }
}

/// The Bazel attribute type (packages/Type.java, packages/BuildType.java)
/// as far as the query output formatters distinguish it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum AttrType {
    Int,
    String,
    Label,
    NodepLabel,
    Output,
    StringList,
    LabelList,
    NodepLabelList,
    OutputList,
    License,
    StringDict,
    LabelListDict,
    StringListDict,
    Bool,
    TriState,
    IntList,
    LabelDictUnary,
    LabelKeyedStringDict,
}

impl AttrType {
    fn of_native(ty: NativeType) -> Self {
        match ty {
            NativeType::String => Self::String,
            NativeType::StringList => Self::StringList,
            NativeType::StringDict => Self::StringDict,
            NativeType::Bool => Self::Bool,
            NativeType::Int => Self::Int,
            NativeType::TriState => Self::TriState,
            NativeType::Label => Self::Label,
            NativeType::NodepLabel => Self::NodepLabel,
            NativeType::LabelList => Self::LabelList,
            NativeType::NodepLabelList => Self::NodepLabelList,
            NativeType::LabelKeyedStringDict => Self::LabelKeyedStringDict,
            NativeType::LabelListDict => Self::LabelListDict,
            NativeType::OutputList => Self::OutputList,
            NativeType::License => Self::License,
        }
    }

    fn of_starlark(kind: &AttrKind) -> Self {
        match kind {
            AttrKind::String => Self::String,
            AttrKind::StringList => Self::StringList,
            AttrKind::StringListDict => Self::StringListDict,
            AttrKind::StringDict => Self::StringDict,
            AttrKind::Int => Self::Int,
            AttrKind::Bool => Self::Bool,
            AttrKind::Label => Self::Label,
            AttrKind::LabelList => Self::LabelList,
            AttrKind::Output => Self::Output,
            AttrKind::OutputList => Self::OutputList,
            AttrKind::IntList => Self::IntList,
            AttrKind::LabelKeyedStringDict => Self::LabelKeyedStringDict,
            AttrKind::StringKeyedLabelDict => Self::LabelDictUnary,
            AttrKind::LabelListDict => Self::LabelListDict,
        }
    }

    /// A list type (Type.getListElementType() != null).
    pub(crate) fn is_list(self) -> bool {
        matches!(
            self,
            Self::StringList
                | Self::LabelList
                | Self::NodepLabelList
                | Self::OutputList
                | Self::IntList
        )
    }

    /// A dict type (Type.DictType).
    pub(crate) fn is_dict(self) -> bool {
        matches!(
            self,
            Self::StringDict
                | Self::LabelListDict
                | Self::StringListDict
                | Self::LabelDictUnary
                | Self::LabelKeyedStringDict
        )
    }
}

/// One attribute of a rule instance.
#[derive(Clone, Debug)]
pub(crate) struct QueryAttribute {
    /// The native name (`$x`/`:x` for private attributes).
    pub name: String,
    /// Attribute.getPublicName (`_x` for private attributes).
    pub public_name: String,
    pub shape: Shape,
    /// The attribute's type.
    pub ty: AttrType,
    /// Rule.isAttributeValueExplicitlySpecified.
    pub explicit: bool,
    /// Attribute.isToolDependency: an exec-transition attribute.
    pub tool: bool,
    /// The plain value (explicit, default or computed default), when not
    /// a `select()`.
    pub value: Option<AttrValue>,
    /// The retained `select()` value.
    pub select: Option<Vec<ConfigurablePart>>,
    /// The attribute default, used for `None` select branches.
    pub default: Option<AttrValue>,
    /// Aspects propagating along this attribute.
    pub aspects: Vec<crate::rule_declarations::QueryAspect>,
}

/// Canonical label texts of a value (Type.visitLabels), in value order.
pub(crate) fn value_labels(value: &AttrValue, shape: Shape) -> Vec<String> {
    if !shape.is_dependency() {
        return Vec::new();
    }
    match value {
        AttrValue::Label(Some(label)) => vec![label.clone()],
        AttrValue::Labels(labels) => labels.clone(),
        AttrValue::LabelKeyedStringDict(entries) => {
            entries.iter().map(|(label, _)| label.clone()).collect()
        }
        AttrValue::StringKeyedLabelDict(entries) => {
            entries.iter().map(|(_, label)| label.clone()).collect()
        }
        AttrValue::LabelListDict(entries) => entries
            .iter()
            .flat_map(|(_, labels)| labels.iter().cloned())
            .collect(),
        _ => Vec::new(),
    }
}

/// Selector.isDefaultConditionLabel.
pub(crate) fn is_default_condition(key: &str) -> bool {
    matches!(key, "//conditions:default" | "@@//conditions:default")
}

impl QueryAttribute {
    /// AggregatingAttributeMapper.visitLabels for this attribute: select keys
    /// (except the default condition) and every branch value; a `None`
    /// branch contributes the attribute default.
    pub(crate) fn labels(&self) -> Vec<String> {
        let mut labels = Vec::new();
        if let Some(parts) = &self.select {
            for part in parts {
                match part {
                    ConfigurablePart::Plain(value) => {
                        labels.extend(value_labels(value, self.shape));
                    }
                    ConfigurablePart::Select { branches, .. } => {
                        for (key, value) in branches {
                            if !is_default_condition(key) {
                                labels.push(key.clone());
                            }
                            if let Some(value) = value.as_ref().or(self.default.as_ref()) {
                                labels.extend(value_labels(value, self.shape));
                            }
                        }
                    }
                }
            }
            return labels;
        }
        if let Some(value) = &self.value {
            labels.extend(value_labels(value, self.shape));
        }
        labels
    }

    /// Select key labels (SelectorList.getKeyLabels, BuildType.java:628-639).
    fn key_labels(&self) -> Vec<String> {
        let mut keys = Vec::new();
        for part in self.select.iter().flatten() {
            if let ConfigurablePart::Select { branches, .. } = part {
                for (key, _) in branches {
                    if !is_default_condition(key) && !keys.contains(key) {
                        keys.push(key.clone());
                    }
                }
            }
        }
        keys
    }
}

fn private_public_name(name: &str) -> String {
    match name.strip_prefix(['$', ':']) {
        Some(rest) => format!("_{rest}"),
        None => name.to_owned(),
    }
}

fn implicit(name: &str, shape: Shape, tool: bool, value: Option<AttrValue>) -> QueryAttribute {
    let ty = match shape {
        Shape::Label => AttrType::Label,
        Shape::LabelList => AttrType::LabelList,
        Shape::Bool => AttrType::Bool,
        Shape::Int => AttrType::Int,
        _ => AttrType::String,
    };
    QueryAttribute {
        name: name.to_owned(),
        public_name: private_public_name(name),
        shape,
        ty,
        explicit: false,
        tool,
        value,
        select: None,
        default: None,
        aspects: Vec::new(),
    }
}

/// The tools repository (RuleDefinitionEnvironment.getToolsRepository).
const TOOLS: &str = "@@bazel_tools";

fn tools_label(path: &str) -> AttrValue {
    AttrValue::Label(Some(format!("{TOOLS}{path}")))
}

/// Static loading-phase defaults of `configuration_field` late-bound
/// attributes: StarlarkLateBoundDefault's annotation default label
/// (analysis/starlark/StarlarkLateBoundDefault.java:66-77), which
/// LateBoundDefault.getDefault returns at loading
/// (AggregatingAttributeMapper.java:160-161). Fields without a default label
/// are null.
fn late_bound_default(fragment: &str, field: &str) -> Option<String> {
    let path = match (fragment, field) {
        // rules/apple/AppleConfiguration.java:320-324 (defaultInToolRepository).
        ("apple", "xcode_config_label") => "//tools/objc:host_xcodes",
        // rules/proto/ProtoConfiguration.java:185-228 with ProtoConstants.java:44-58.
        ("proto", "proto_compiler") => "//tools/proto:protoc",
        ("proto", "proto_toolchain_for_java") => "//tools/proto:java_toolchain",
        ("proto", "proto_toolchain_for_java_lite") => "//tools/proto:javalite_toolchain",
        ("proto", "proto_toolchain_for_cc") => "//tools/proto:cc_toolchain",
        ("proto", "proto_toolchain_for_j2objc") => "//tools/j2objc:j2objc_proto_toolchain",
        _ => return None,
    };
    Some(format!("{TOOLS}{path}"))
}

/// Package defaults of the computed built-in attributes
/// (BaseRuleClasses.java:226-299): `compatible_with`, `restricted_to` and
/// `package_metadata` default to the package's values.
fn builtin_default(name: &str, rule: &Rule, package: &Package) -> Option<AttrValue> {
    let labels = |labels: &Option<Vec<Label>>| {
        labels
            .as_ref()
            .map(|labels| AttrValue::Labels(labels.iter().map(ToString::to_string).collect()))
    };
    match name {
        "compatible_with" => labels(&package.args.default_compatible_with),
        "restricted_to" => labels(&package.args.default_restricted_to),
        "package_metadata" => Some(AttrValue::Labels(rule.package_metadata.clone())),
        _ => None,
    }
}

/// The value of an unset built-in attribute (Type.getDefaultValue, with the
/// effective `tags`, `testonly` and `deprecation`).
fn type_default(name: &str, ty: NativeType, rule: &Rule, package: &Package) -> Option<AttrValue> {
    Some(match name {
        "tags" => AttrValue::Strings(rule_tags(rule)),
        "testonly" => AttrValue::Bool(rule.is_test_only(&package.args)),
        "deprecation" => AttrValue::String(rule.deprecation_warning(&package.args)?),
        _ => match ty {
            NativeType::String => AttrValue::String(String::new()),
            NativeType::StringList | NativeType::License => AttrValue::Strings(Vec::new()),
            NativeType::StringDict => AttrValue::StringDict(Vec::new()),
            NativeType::Bool => AttrValue::Bool(false),
            NativeType::Int => AttrValue::Int(0),
            NativeType::TriState => AttrValue::Int(-1),
            NativeType::Label | NativeType::NodepLabel => AttrValue::Label(None),
            NativeType::LabelList | NativeType::NodepLabelList => AttrValue::Labels(Vec::new()),
            NativeType::LabelKeyedStringDict => AttrValue::LabelKeyedStringDict(Vec::new()),
            NativeType::LabelListDict => AttrValue::LabelListDict(Vec::new()),
            NativeType::OutputList => AttrValue::Outputs(Vec::new()),
        },
    })
}

/// A built-in (native-schema) attribute's instance value.
fn builtin_attribute(
    attribute: &crate::native_rules::NativeAttr,
    target: &crate::native_rules::NativeTarget,
    rule: &Rule,
    package: &Package,
) -> QueryAttribute {
    let shape = Shape::of_native(attribute.ty);
    let name = attribute.name;
    let explicit = target.explicit(name)
        || (name == "package_metadata" && target.explicit("applicable_licenses"));
    let select = target.configurable.get(name).cloned();
    // Rule.getAttr generator_* (packages/Rule.java:398-403), explicit for
    // rules created in macros (RuleOrMacroInstance.java:330-335).
    if let (Some(generator), "generator_name" | "generator_function" | "generator_location") =
        (&rule.generator, name)
        && !target.attrs.contains_key(name)
    {
        let value = match name {
            "generator_name" => generator.name.clone(),
            "generator_function" => generator.function.clone(),
            _ => generator.location.clone().unwrap_or_default(),
        };
        return QueryAttribute {
            name: name.to_owned(),
            public_name: name.to_owned(),
            shape,
            ty: AttrType::of_native(attribute.ty),
            explicit: true,
            tool: false,
            value: Some(AttrValue::String(value)),
            select: None,
            default: None,
            aspects: Vec::new(),
        };
    }
    let value = if select.is_some() {
        None
    } else if name == "package_metadata" {
        // The effective value (explicit, else the package default).
        Some(AttrValue::Labels(rule.package_metadata.clone()))
    } else {
        target
            .attrs
            .get(name)
            .cloned()
            .or_else(|| builtin_default(name, rule, package))
            .or_else(|| type_default(name, attribute.ty, rule, package))
    };
    QueryAttribute {
        name: name.to_owned(),
        public_name: name.to_owned(),
        shape,
        ty: AttrType::of_native(attribute.ty),
        explicit,
        tool: false,
        value,
        select,
        default: None,
        aspects: Vec::new(),
    }
}

/// Test rule private attributes (StarlarkRuleClassFunctions.java:286-370):
/// exec-configured tool dependencies of the default test runner exec group.
fn test_attribute(name: &str) -> Option<QueryAttribute> {
    let (shape, value) = match name {
        "$test_wrapper" => (Shape::Label, tools_label("//tools/test:test_wrapper")),
        "$xml_writer" => (Shape::Label, tools_label("//tools/test:xml_writer")),
        "$test_runtime" => (
            Shape::LabelList,
            AttrValue::Labels(vec![format!("{TOOLS}//tools/test:runtime")]),
        ),
        "$test_setup_script" => (Shape::Label, tools_label("//tools/test:test_setup")),
        "$xml_generator_script" => (Shape::Label, tools_label("//tools/test:test_xml_generator")),
        "$collect_coverage_script" => (Shape::Label, tools_label("//tools/test:collect_coverage")),
        // BaseRuleClasses.java:125-151: late-bound with a static default.
        ":coverage_support" => (Shape::Label, tools_label("//tools/test:coverage_support")),
        ":coverage_report_generator" => (
            Shape::Label,
            tools_label("//tools/test:coverage_report_generator"),
        ),
        // RUN_UNDER_*_CONFIG (BaseRuleClasses.java:162-196): null default.
        ":run_under_exec_config" => (Shape::Label, AttrValue::Label(None)),
        ":run_under_target_config" => {
            return Some(implicit(
                name,
                Shape::Label,
                false,
                Some(AttrValue::Label(None)),
            ));
        }
        "$is_executable" => {
            return Some(implicit(
                name,
                Shape::Bool,
                false,
                Some(AttrValue::Bool(true)),
            ));
        }
        _ => return None,
    };
    Some(implicit(name, shape, true, Some(value)))
}

/// `$config_dependencies` (AttributeProvider.populateConfigDependenciesAttribute,
/// packages/AttributeProvider.java:446-466): every select key label.
fn config_dependencies(attributes: &[QueryAttribute]) -> QueryAttribute {
    let mut keys = Vec::new();
    for attribute in attributes {
        for key in attribute.key_labels() {
            if !keys.contains(&key) {
                keys.push(key);
            }
        }
    }
    implicit(
        "$config_dependencies",
        Shape::LabelList,
        false,
        Some(AttrValue::Labels(keys)),
    )
}

/// The attributes of a rule (not a package group) in rule-class order.
pub(crate) fn rule_attributes(rule: &Rule, package: &Package) -> Vec<QueryAttribute> {
    if let Some(custom) = &rule.custom {
        return starlark_attributes(rule, package, custom);
    }
    let Some(native) = rule.native.as_deref() else {
        return Vec::new();
    };
    let Some(class) = crate::native_rules::rule_class(&native.rule_class) else {
        return Vec::new();
    };
    let mut attributes = Vec::new();
    let mut config_slot = None;
    for attribute in &class.attrs {
        if attribute.name == "package_metadata" {
            // `$config_dependencies` precedes package_metadata
            // (BaseRuleClasses.java:280-297).
            config_slot = Some(attributes.len());
        }
        attributes.push(builtin_attribute(attribute, native, rule, package));
        // GenRuleBaseRule tools are exec-configured (GenRuleBaseRule.java:89-92);
        // genrule toolchains too (:206-214).
        if native.rule_class == "genrule"
            && matches!(attribute.name, "tools" | "toolchains")
            && let Some(last) = attributes.last_mut()
        {
            last.tool = attribute.name == "tools";
        }
    }
    match native.rule_class.as_str() {
        // bazel/rules/genrule/BazelGenRuleRule.java:40-48.
        "genrule" => attributes.push(implicit(
            "$genrule_setup",
            Shape::Label,
            true,
            Some(tools_label("//tools/genrule:genrule-setup.sh")),
        )),
        // rules/test/TestSuiteRule.java:105-110 and AttributeProvider.java:386-400:
        // an empty `tests` gets the package's matching tests, as an
        // explicit value.
        "test_suite" => {
            let tests = native.labels("tests");
            if tests.is_empty() {
                let mut attribute = implicit(
                    "$implicit_tests",
                    Shape::LabelList,
                    false,
                    Some(AttrValue::Labels(implicit_tests(&native.tags, package))),
                );
                attribute.explicit = true;
                attributes.push(attribute);
            }
        }
        // rules/LabelBuildSettings.java:62-91: `:alias` defaults to the
        // build_setting_default (a nodep label) at loading.
        "label_flag" | "label_setting" => {
            let default = native
                .attrs
                .get("build_setting_default")
                .cloned()
                .unwrap_or(AttrValue::Label(None));
            let default = match default {
                AttrValue::Labels(mut labels) if labels.len() == 1 => {
                    AttrValue::Label(labels.pop())
                }
                other => other,
            };
            attributes.push(implicit(":alias", Shape::Label, false, Some(default)));
        }
        _ => {}
    }
    let config = config_dependencies(&attributes);
    match config_slot {
        Some(index) => attributes.insert(index, config),
        None => attributes.push(config),
    }
    attributes
}

/// TestSuiteImplicitTestsAccumulator (packages/TestSuiteImplicitTestsAccumulator.java):
/// the package's non-manual `*_test` rules matching the suite's tags, sorted.
fn implicit_tests(suite_tags: &[String], package: &Package) -> Vec<String> {
    let (required, excluded) = sort_tags_by_sense(suite_tags);
    let mut tests = package
        .rules
        .values()
        .filter(|rule| rule.rule_class_name().ends_with("_test") && !rule.is_manual())
        .filter(|rule| {
            let mut tags = rule_tags(rule);
            tags.push(rule_size(rule));
            excluded.iter().all(|tag| !tags.contains(tag))
                && required.iter().all(|tag| tags.contains(tag))
        })
        .map(|rule| rule.label.clone())
        .collect::<Vec<_>>();
    tests.sort();
    tests.iter().map(ToString::to_string).collect()
}

/// TestTargetUtils.sortTagsBySense: `-x` excluded, `+x`/`x` required, `manual` ignored.
pub(crate) fn sort_tags_by_sense(tags: &[String]) -> (Vec<String>, Vec<String>) {
    let mut required = Vec::new();
    let mut excluded = Vec::new();
    for tag in tags {
        if let Some(rest) = tag.strip_prefix('-') {
            excluded.push(rest.to_owned());
        } else if let Some(rest) = tag.strip_prefix('+') {
            required.push(rest.to_owned());
        } else if tag != "manual" {
            required.push(tag.clone());
        }
    }
    (required, excluded)
}

/// The effective `tags` of a rule.
pub(crate) fn rule_tags(rule: &Rule) -> Vec<String> {
    if let Some(custom) = &rule.custom {
        return custom.declaration.builtin.target.tags.clone();
    }
    rule.native
        .as_ref()
        .map(|native| native.tags.clone())
        .unwrap_or_default()
}

/// The `size` string of a rule (`medium` for test rules by default).
pub(crate) fn rule_size(rule: &Rule) -> String {
    let explicit = match (&rule.custom, &rule.native) {
        (Some(custom), _) => custom.declaration.builtin.target.string("size"),
        (None, Some(native)) => native.string("size"),
        _ => None,
    };
    explicit.map(str::to_owned).unwrap_or_else(|| {
        if rule.rule_class_name().ends_with("_test") {
            "medium".to_owned()
        } else {
            String::new()
        }
    })
}

/// Rule.isExecutable (packages/Rule.java:856-864).
pub(crate) fn is_executable(rule: &Rule) -> bool {
    rule.custom.as_ref().is_some_and(|custom| {
        let kind: StarlarkRuleKind = custom.declaration.builtin.kind;
        kind.executable || kind.test
    })
}

fn starlark_attributes(
    rule: &Rule,
    package: &Package,
    custom: &crate::custom_rule::CustomInvocation,
) -> Vec<QueryAttribute> {
    let declaration = &custom.declaration;
    let builtin = &declaration.builtin;
    let builtins = crate::native_rules::starlark_builtins(builtin.kind);
    let mut attributes = Vec::new();
    let mut config_slot = None;
    for (name, attribute) in builtins.entries() {
        match attribute {
            Some(attribute) => {
                attributes.push(builtin_attribute(attribute, &builtin.target, rule, package));
            }
            None if name == "$config_dependencies" => config_slot = Some(attributes.len()),
            None if name == ":action_listener" => attributes.push(implicit(
                name,
                Shape::LabelList,
                false,
                Some(AttrValue::Labels(Vec::new())),
            )),
            None => {
                if let Some(attribute) = test_attribute(name) {
                    attributes.push(attribute);
                }
            }
        }
    }
    let configurable = &declaration.configurable;
    for name in &declaration.attr_order {
        let Some(spec) = custom.schema.get(name) else {
            continue;
        };
        let shape = Shape::of_starlark(&spec.kind);
        let late = spec.declaration.late_bound;
        let native_name = match name.strip_prefix('_') {
            Some(rest) if late => format!(":{rest}"),
            Some(rest) => format!("${rest}"),
            None => name.clone(),
        };
        let select = configurable.get(name).cloned();
        let value = if select.is_some() {
            None
        } else if late {
            Some(AttrValue::Label(
                spec.declaration
                    .configuration_field
                    .as_ref()
                    .and_then(|field| late_bound_default(&field.fragment, &field.name)),
            ))
        } else {
            custom
                .attrs
                .get(name)
                .cloned()
                .or_else(|| Some(spec.default.clone()))
        };
        attributes.push(QueryAttribute {
            public_name: name.clone(),
            name: native_name,
            shape,
            ty: AttrType::of_starlark(&spec.kind),
            explicit: declaration.explicit.contains(name),
            tool: matches!(spec.declaration.cfg, crate::rule_declarations::Cfg::Exec(_)),
            value,
            select,
            default: Some(spec.default.clone()),
            aspects: spec.declaration.aspect_definitions.clone(),
        });
    }
    if !builtin.kind.dependency_resolution {
        let config = config_dependencies(&attributes);
        match config_slot {
            Some(index) => attributes.insert(index, config),
            None => attributes.push(config),
        }
    }
    attributes
}

/// Rule class toolchain types (RuleClass.getToolchainTypes), canonical texts.
pub(crate) fn toolchain_types(rule: &Rule) -> Vec<String> {
    rule.custom
        .as_ref()
        .map(|custom| {
            custom
                .declaration
                .toolchain_types
                .iter()
                .map(|(label, _)| label.clone())
                .collect()
        })
        .unwrap_or_default()
}

/// Whether `rule` is a package group.
pub(crate) fn is_package_group(rule: &Rule) -> bool {
    matches!(rule.kind, RuleKind::PackageGroup(_))
}

/// AspectDefinition.satisfies (packages/AspectDefinition.java:216-218):
/// RequiredProviders.isSatisfiedBy the rule class's advertised providers.
pub(crate) fn aspect_applies(aspect: &crate::rule_declarations::QueryAspect, rule: &Rule) -> bool {
    if aspect.required_providers.is_empty() {
        return true;
    }
    // AdvertisedProviderSet.ANY: classes that forward any provider.
    if let Some(native) = &rule.native
        && matches!(
            native.rule_class.as_str(),
            "alias" | "label_flag" | "label_setting"
        )
    {
        return true;
    }
    let provides = rule
        .custom
        .as_ref()
        .map(|custom| custom.declaration.provides.as_slice())
        .unwrap_or_default();
    aspect.required_providers.iter().any(|clause| {
        clause
            .iter()
            .all(|required| provides.iter().any(|provided| provided.id == required.id))
    })
}

/// The dependency labels of an aspect's attributes (their defaults;
/// AspectDefinition.forEachLabelDepFromAllAttributesOfAspect, :236-262),
/// with whether each is an exec-configured tool dependency.
pub(crate) fn aspect_labels(aspect: &crate::rule_declarations::QueryAspect) -> Vec<(String, bool)> {
    let mut labels = Vec::new();
    for (_, spec) in &aspect.attrs {
        let shape = Shape::of_starlark(&spec.kind);
        let tool = matches!(spec.declaration.cfg, crate::rule_declarations::Cfg::Exec(_));
        for label in value_labels(&spec.default, shape) {
            labels.push((label, tool));
        }
    }
    labels
}
