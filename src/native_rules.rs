//! Load-time native rule classes of Bazel 9.3 (registration, attribute
//! schemas, conversion, configurability and query visibility).
//!
//! Pinned source: `/tmp/opencode/bazel-9.3-reference` (e89755b, Release 9.3.0).
//! Paths below are relative to `src/main/java/com/google/devtools/build/lib/`.
//! Only loading is modeled here; analysis is in `crate::engine` (classes
//! without native analysis fail explicitly there).
use std::collections::{BTreeMap, BTreeSet};

use anyhow::{anyhow, bail};
use starlark::values::Value;
use starlark::values::dict::DictRef;

use crate::custom_rule::{AttrValue, ConfigurablePart};
use crate::label::{Label, PackageId};
use crate::repository::RepositorySet;
use crate::repository_budget::HostBudget;

/// Bazel attribute types used by the registered native rule classes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NativeType {
    String,
    StringList,
    StringDict,
    Bool,
    Int,
    TriState,
    Label,
    NodepLabel,
    LabelList,
    NodepLabelList,
    LabelKeyedStringDict,
    /// `BuildType.LABEL_LIST_DICT` (dict(string, list(label))).
    LabelListDict,
    OutputList,
    License,
}

impl NativeType {
    /// `Type.toString` (packages/Type.java, packages/BuildType.java).
    pub fn name(self) -> &'static str {
        match self {
            Self::String => "string",
            Self::StringList => "list(string)",
            Self::StringDict => "dict(string, string)",
            Self::Bool => "boolean",
            Self::Int => "int",
            Self::TriState => "tristate",
            Self::Label | Self::NodepLabel => "label",
            Self::LabelList | Self::NodepLabelList => "list(label)",
            Self::LabelKeyedStringDict => "dict(label, string)",
            Self::LabelListDict => "dict(string, list(label))",
            Self::OutputList => "list(output)",
            Self::License => "license",
        }
    }

    /// Types whose `concat` is not the null default (Type.java:245, 378, 478,
    /// 604, 774); others reject `select() + ...` (BuildType.java:177-180, 587-590).
    fn concatenable(self) -> bool {
        !matches!(
            self,
            Self::Bool | Self::TriState | Self::Label | Self::NodepLabel | Self::License
        )
    }
}

/// One user-settable attribute (Attribute.java). Private (`$`/`:`) attributes
/// cannot be named by BUILD keyword arguments and are omitted.
#[derive(Clone, Debug)]
pub struct NativeAttr {
    pub name: &'static str,
    pub ty: NativeType,
    /// Attribute.isConfigurable (Attribute.java:2101-2105).
    pub configurable: bool,
    pub mandatory: bool,
    /// Attribute.isDocumented: the SpellChecker candidate set
    /// (AttributeProvider.java:229-234).
    pub documented: bool,
    /// Attribute.isOrderIndependent (packages/Attribute.java:2019-2021): list
    /// values are sorted at conversion (BuildType.java:248-253).
    pub order_independent: bool,
}

const fn attr(name: &'static str, ty: NativeType) -> NativeAttr {
    NativeAttr {
        name,
        ty,
        // OUTPUT label class is never configurable (Attribute.java:2101-2105);
        // LICENSE has a type-level reason (BuildType.java:125-129).
        configurable: !matches!(ty, NativeType::OutputList | NativeType::License),
        mandatory: false,
        documented: true,
        order_independent: false,
    }
}

const fn nonconfigurable(mut attribute: NativeAttr) -> NativeAttr {
    attribute.configurable = false;
    attribute
}

const fn mandatory(mut attribute: NativeAttr) -> NativeAttr {
    attribute.mandatory = true;
    attribute
}

const fn undocumented(mut attribute: NativeAttr) -> NativeAttr {
    attribute.documented = false;
    attribute
}

const fn order_independent(mut attribute: NativeAttr) -> NativeAttr {
    attribute.order_independent = true;
    attribute
}

/// A registered native rule class.
#[derive(Clone, Debug)]
pub struct RuleClass {
    pub name: &'static str,
    pub attrs: Vec<NativeAttr>,
    /// Default value of `tags` (overridden to `["manual"]` by several classes).
    pub default_tags: &'static [&'static str],
    /// Default value of `testonly` when not computed from the package.
    pub testonly_default: Option<bool>,
    /// `%{name}` implicit output templates (ImplicitOutputsFunction.fromTemplates).
    pub implicit_outputs: &'static [&'static str],
    /// RuleClass.Builder.setIgnoreLicenses (AttributeProvider.java:245-249).
    pub ignore_licenses: bool,
    /// Allowed values (Attribute.AllowedValueSet, Attribute.java:210-243).
    pub allowed_values: &'static [(&'static str, &'static [&'static str])],
    /// Whether `build_setting_default` makes this a build setting rule.
    pub build_setting: bool,
}

impl RuleClass {
    fn new(name: &'static str, attrs: Vec<NativeAttr>) -> Self {
        Self {
            name,
            attrs,
            default_tags: &[],
            testonly_default: None,
            implicit_outputs: &[],
            ignore_licenses: false,
            allowed_values: &[],
            build_setting: false,
        }
    }

    pub fn attribute(&self, name: &str) -> Option<&NativeAttr> {
        self.attrs.iter().find(|attribute| attribute.name == name)
    }

    fn remove(mut self, name: &str) -> Self {
        let before = self.attrs.len();
        self.attrs.retain(|attribute| attribute.name != name);
        assert_ne!(
            before,
            self.attrs.len(),
            "No such attribute '{name}' to remove."
        );
        self
    }

    fn add(mut self, attribute: NativeAttr) -> Self {
        assert!(
            self.attribute(attribute.name).is_none(),
            "There is already a built-in attribute '{}' which cannot be overridden.",
            attribute.name
        );
        self.attrs.push(attribute);
        self
    }

    fn override_attr(mut self, attribute: NativeAttr) -> Self {
        let slot = self
            .attrs
            .iter_mut()
            .find(|existing| existing.name == attribute.name)
            .expect("override of an existing attribute");
        assert_eq!(slot.ty, attribute.ty);
        *slot = attribute;
        self
    }

    /// RuleClass.Builder.exemptFromConstraintChecking (packages/RuleClass.java:1559-1567).
    fn exempt_from_constraint_checking(self) -> Self {
        self.remove("compatible_with")
            .remove("restricted_to")
            .remove("target_compatible_with")
    }

    /// `tags` overridden to default `["manual"]`, nonconfigurable.
    fn manual_tags(mut self) -> Self {
        self.default_tags = &["manual"];
        self.override_attr(nonconfigurable(attr("tags", NativeType::StringList)))
    }
}

use NativeType as T;

/// One entry of a built-in attribute list in Bazel's declaration order: a
/// user-settable attribute, or the native name (`$x`/`:x`) of a private one.
/// Private attributes are never settable by keyword (their native names are
/// not identifiers), but they are documented SpellChecker candidates
/// (Attribute.isDocumented, packages/Attribute.java:2067-2069) and block
/// Starlark attributes of the same native name.
#[derive(Clone, Debug)]
enum Entry {
    Attr(NativeAttr),
    Private(&'static str),
}

/// BaseRuleClasses.commonCoreAndStarlarkAttributes (BaseRuleClasses.java:226-299),
/// shared by every native BUILD rule and every Starlark rule class.
fn common_core_and_starlark_attributes() -> Vec<Entry> {
    use Entry::{Attr, Private};
    vec![
        // visibility, transitive_configs, tags and features are
        // orderIndependent (BaseRuleClasses.java:232-267).
        Attr(order_independent(nonconfigurable(attr(
            "visibility",
            T::NodepLabelList,
        )))),
        Attr(order_independent(nonconfigurable(attr(
            "transitive_configs",
            T::NodepLabelList,
        )))),
        Attr(nonconfigurable(attr("deprecation", T::String))),
        Attr(order_independent(nonconfigurable(attr(
            "tags",
            T::StringList,
        )))),
        Attr(undocumented(nonconfigurable(attr(
            "generator_name",
            T::String,
        )))),
        Attr(undocumented(nonconfigurable(attr(
            "generator_function",
            T::String,
        )))),
        Attr(undocumented(nonconfigurable(attr(
            "generator_location",
            T::String,
        )))),
        Attr(nonconfigurable(attr("testonly", T::Bool))),
        Attr(order_independent(attr("features", T::StringList))),
        Private(":action_listener"),
        Attr(nonconfigurable(attr("compatible_with", T::LabelList))),
        Attr(nonconfigurable(attr("restricted_to", T::LabelList))),
        Private("$config_dependencies"),
        Attr(nonconfigurable(attr("package_metadata", T::LabelList))),
        Attr(attr("aspect_hints", T::LabelList)),
    ]
}

/// RuleClass.Builder constructor adds `name` first (RuleClass.java:771), then
/// BaseRuleClasses.commonCoreAndStarlarkAttributes (BaseRuleClasses.java:226-299)
/// and NativeBuildRule (BaseRuleClasses.java:318-340).
fn native_build_rule(name: &'static str) -> RuleClass {
    let mut attrs = common_core_and_starlark_attributes()
        .into_iter()
        .filter_map(|entry| match entry {
            Entry::Attr(attribute) => Some(attribute),
            Entry::Private(_) => None,
        })
        .collect::<Vec<_>>();
    // NativeBuildRule (BaseRuleClasses.java:322-338).
    attrs.extend([
        nonconfigurable(attr("licenses", T::License)),
        nonconfigurable(attr("distribs", T::StringList)),
        attr("target_compatible_with", T::LabelList),
    ]);
    RuleClass::new(name, attrs)
}

/// Which `StarlarkRuleClassFunctions` base class a Starlark `rule()` extends
/// (StarlarkRuleClassFunctions.java:760-784).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct StarlarkRuleKind {
    /// `test=True` or `analysis_test=True`: `getTestBaseRule` (:258-378).
    pub test: bool,
    /// `executable=True`: `binaryBaseRule` (:232-241).
    pub executable: bool,
    /// `dependency_resolution_rule=True`: `dependencyResolutionBaseRule` (:197-212).
    pub dependency_resolution: bool,
}

/// The built-in attributes of a Starlark rule class.
#[derive(Clone, Debug)]
pub struct StarlarkBuiltins {
    entries: Vec<Entry>,
    /// `testonly` is overridden to `True` for test rule classes
    /// (packages/RuleClass.java:839-851); otherwise it is the package default.
    pub testonly_default: Option<bool>,
}

impl StarlarkBuiltins {
    /// Every built-in attribute in declaration order: the user-settable
    /// ones with their definition, and the private (`$x`/`:x`) ones by
    /// native name.
    pub fn entries(&self) -> impl Iterator<Item = (&str, Option<&NativeAttr>)> {
        self.entries.iter().map(|entry| match entry {
            Entry::Attr(attribute) => (attribute.name, Some(attribute)),
            Entry::Private(name) => (*name, None),
        })
    }

    /// User-settable built-in attributes, in declaration order.
    pub fn attrs(&self) -> impl Iterator<Item = &NativeAttr> {
        self.entries.iter().filter_map(|entry| match entry {
            Entry::Attr(attribute) => Some(attribute),
            Entry::Private(_) => None,
        })
    }

    pub fn attribute(&self, name: &str) -> Option<&NativeAttr> {
        self.attrs().find(|attribute| attribute.name == name)
    }

    /// Whether `native_name` (`$x`/`:x` for private attributes) is a built-in
    /// attribute, which Starlark attributes cannot override (RuleClass.java:1301-1306).
    pub fn contains(&self, native_name: &str) -> bool {
        native_name == "name"
            || self.entries.iter().any(|entry| match entry {
                Entry::Attr(attribute) => attribute.name == native_name,
                Entry::Private(name) => *name == native_name,
            })
    }

    /// Documented attribute names in Bazel's order, `name` first
    /// (AttributeProvider.java:220-235 SpellChecker candidates). Private
    /// `$x`/`:x` attributes are always UNDOCUMENTED (Attribute.java:444-449).
    pub fn candidates(&self) -> Vec<&'static str> {
        std::iter::once("name")
            .chain(self.entries.iter().filter_map(|entry| match entry {
                Entry::Attr(attribute) => attribute.documented.then_some(attribute.name),
                Entry::Private(_) => None,
            }))
            .collect()
    }

    /// A built-in attribute with a different native name but the same public
    /// name (`_x`, Attribute.getStarlarkName, packages/Attribute.java:2307-2312)
    /// as `native_name`: building the rule class fails
    /// (packages/RuleClass.java:1905-1918).
    pub fn public_name_conflict(&self, native_name: &str) -> Option<&'static str> {
        let public = |name: &str| match name.strip_prefix(['$', ':']) {
            Some(rest) => format!("_{rest}"),
            None => name.to_owned(),
        };
        let wanted = public(native_name);
        self.entries.iter().find_map(|entry| match entry {
            Entry::Private(name) if *name != native_name && public(name) == wanted => Some(*name),
            _ => None,
        })
    }
}

/// `StarlarkRuleClassFunctions.baseRule` (StarlarkRuleClassFunctions.java:159-194),
/// `binaryBaseRule` (:232-241), `getTestBaseRule` (:258-378) and
/// `dependencyResolutionBaseRule` (:197-212). `licenses`/`distribs` belong to
/// NativeBuildRule only, so Starlark rules do not have them.
pub fn starlark_builtins(kind: StarlarkRuleKind) -> StarlarkBuiltins {
    use Entry::{Attr, Private};
    let mut entries = vec![Attr(attr("expect_failure", T::String))];
    entries.extend(common_core_and_starlark_attributes());
    entries.extend([
        Attr(attr("toolchains", T::LabelList)),
        Attr(attr("exec_properties", T::StringDict)),
        Attr(nonconfigurable(attr("exec_compatible_with", T::LabelList))),
        Attr(nonconfigurable(attr(
            "exec_group_compatible_with",
            T::LabelListDict,
        ))),
        Attr(attr("target_compatible_with", T::LabelList)),
    ]);
    // The TEST rule class type overrides testonly regardless of the parent.
    let testonly_default = kind.test.then_some(true);
    if kind.dependency_resolution {
        let removed = [
            ":action_listener",
            "aspect_hints",
            "toolchains",
            "exec_compatible_with",
            "exec_group_compatible_with",
            "target_compatible_with",
            "compatible_with",
            "restricted_to",
            "$config_dependencies",
            "package_metadata",
        ];
        entries.retain(|entry| match entry {
            Attr(attribute) => !removed.contains(&attribute.name),
            Private(name) => !removed.contains(name),
        });
    } else if kind.test {
        entries.extend([
            Attr(nonconfigurable(attr("size", T::String))),
            Attr(nonconfigurable(attr("timeout", T::String))),
            Attr(nonconfigurable(attr("flaky", T::Bool))),
            Attr(attr("shard_count", T::Int)),
            Attr(nonconfigurable(attr("local", T::Bool))),
            Attr(attr("args", T::StringList)),
            Private("$test_wrapper"),
            Private("$xml_writer"),
            Private("$test_runtime"),
            Private("$test_setup_script"),
            Private("$xml_generator_script"),
            Private("$collect_coverage_script"),
            Private(":coverage_support"),
            Private(":coverage_report_generator"),
            Private(":run_under_exec_config"),
            Private(":run_under_target_config"),
            Private("$is_executable"),
        ]);
    } else if kind.executable {
        entries.extend([
            Attr(attr("args", T::StringList)),
            Attr(attr("output_licenses", T::StringList)),
            Private("$is_executable"),
        ]);
    }
    StarlarkBuiltins {
        entries,
        testonly_default,
    }
}

/// NativeActionCreatingRule (BaseRuleClasses.java:382-408).
fn native_action_creating_rule(name: &'static str) -> RuleClass {
    native_build_rule(name)
        .add(attr("deps", T::LabelList))
        .add(attr("data", T::LabelList))
        .add(attr("exec_properties", T::StringDict))
        .add(nonconfigurable(attr("exec_compatible_with", T::LabelList)))
        .add(nonconfigurable(attr(
            "exec_group_compatible_with",
            T::LabelListDict,
        )))
}

/// Every native rule class registered by this profile, in Bazel's
/// `bazel query --output=label_kind` kind spelling.
pub const RULE_CLASSES: &[&str] = &[
    "alias",
    "config_setting",
    "constraint_setting",
    "constraint_value",
    "filegroup",
    "genquery",
    "genrule",
    "cc_libc_top_alias",
    "java_plugins_flag_alias",
    "label_flag",
    "label_setting",
    "platform",
    "starlark_doc_extract",
    "test_suite",
    "toolchain",
    "toolchain_type",
];

/// Build the rule class definition (cited per class).
pub fn rule_class(name: &str) -> Option<RuleClass> {
    Some(match name {
        // rules/filegroup/FilegroupRule.java:36-80.
        "filegroup" => native_build_rule("filegroup")
            .add(attr("srcs", T::LabelList))
            .add(attr("output_group", T::String))
            .add(attr("data", T::LabelList))
            .add(attr("output_licenses", T::StringList)),
        // rules/genrule/GenRuleBaseRule.java:66-348 (ancestors
        // NativeActionCreatingRule and MakeVariableExpandingRule,
        // BaseRuleClasses.java:352-364) and
        // bazel/rules/genrule/BazelGenRuleRule.java:40-56.
        "genrule" => native_action_creating_rule("genrule")
            .add(attr("toolchains", T::LabelList))
            .add(attr("srcs", T::LabelList))
            .add(attr("tools", T::LabelList))
            .add(mandatory(attr("outs", T::OutputList)))
            .add(attr("cmd", T::String))
            .add(attr("cmd_bash", T::String))
            .add(attr("cmd_bat", T::String))
            .add(attr("cmd_ps", T::String))
            .add(nonconfigurable(attr("output_to_bindir", T::Bool)))
            .add(attr("local", T::Bool))
            .add(attr("message", T::String))
            .add(attr("output_licenses", T::StringList))
            .add(nonconfigurable(attr("executable", T::Bool)))
            .add(attr("heuristic_label_expansion", T::Bool))
            .override_attr(nonconfigurable(attr("toolchains", T::LabelList)))
            .remove("data")
            .remove("deps")
            .add(attr("stamp", T::TriState)),
        // rules/Alias.java:61-107.
        "alias" => native_build_rule("alias")
            .remove("licenses")
            .remove("distribs")
            .add(mandatory(attr("actual", T::Label))),
        // rules/config/ConfigRuleClasses.java:44-66 (ConfigBaseRule) and
        // :130-320 (ConfigSettingRule).
        "config_setting" => {
            let mut class = native_build_rule("config_setting")
                .manual_tags()
                .exempt_from_constraint_checking()
                .add(nonconfigurable(attr("values", T::StringDict)))
                .add(nonconfigurable(attr("define_values", T::StringDict)))
                .add(nonconfigurable(attr(
                    "flag_values",
                    T::LabelKeyedStringDict,
                )))
                .add(nonconfigurable(attr("constraint_values", T::LabelList)));
            class.ignore_licenses = true;
            class
        }
        // rules/ToolchainType.java:57-81.
        "toolchain_type" => native_build_rule("toolchain_type")
            .remove("licenses")
            .remove("distribs")
            .add(nonconfigurable(attr("no_match_error", T::String))),
        // rules/platform/ToolchainRule.java:44-111.
        "toolchain" => native_build_rule("toolchain")
            .manual_tags()
            .exempt_from_constraint_checking()
            .add(mandatory(nonconfigurable(attr("toolchain_type", T::Label))))
            .add(nonconfigurable(attr("exec_compatible_with", T::LabelList)))
            .add(nonconfigurable(attr(
                "target_compatible_with",
                T::LabelList,
            )))
            .add(nonconfigurable(attr(
                "use_target_platform_constraints",
                T::Bool,
            )))
            // allowedRuleClasses("config_setting"), no files
            // (ToolchainRule.java:97-103); checked by crate::engine.
            .add(attr("target_settings", T::LabelList))
            .add(mandatory(attr("toolchain", T::NodepLabel))),
        // rules/platform/ConstraintSettingRule.java:38-87.
        "constraint_setting" => native_build_rule("constraint_setting")
            .exempt_from_constraint_checking()
            .remove("package_metadata")
            .manual_tags()
            .add(nonconfigurable(attr(
                "default_constraint_value",
                T::NodepLabel,
            )))
            .add(nonconfigurable(attr("refines_constraint_value", T::Label))),
        // rules/platform/ConstraintValueRule.java:37-63.
        "constraint_value" => native_build_rule("constraint_value")
            .exempt_from_constraint_checking()
            .remove("package_metadata")
            .manual_tags()
            .add(mandatory(nonconfigurable(attr(
                "constraint_setting",
                T::Label,
            )))),
        // rules/platform/PlatformRule.java:48-152.
        "platform" => native_build_rule("platform")
            .exempt_from_constraint_checking()
            .remove("package_metadata")
            .manual_tags()
            .add(nonconfigurable(attr("constraint_values", T::LabelList)))
            .add(nonconfigurable(attr("parents", T::LabelList)))
            .add(nonconfigurable(attr(
                "remote_execution_properties",
                T::String,
            )))
            .add(nonconfigurable(attr("exec_properties", T::StringDict)))
            .add(nonconfigurable(attr("flags", T::StringList)))
            .add(attr("required_settings", T::LabelList))
            .add(nonconfigurable(attr("missing_toolchain_error", T::String)))
            .add(attr("check_toolchain_types", T::Bool))
            .add(attr("allowed_toolchain_types", T::NodepLabelList)),
        // rules/LabelBuildSettings.java:86-127, with the build-setting
        // attributes RuleClass.Builder.build adds (RuleClass.java:904-919).
        "label_flag" | "label_setting" => {
            let name = if name == "label_flag" {
                "label_flag"
            } else {
                "label_setting"
            };
            let mut class = native_build_rule(name)
                .remove("licenses")
                .remove("distribs")
                .add(nonconfigurable(attr("scope", T::String)))
                .add(mandatory(nonconfigurable(attr(
                    "build_setting_default",
                    T::NodepLabel,
                ))))
                .add(nonconfigurable(attr("help", T::String)));
            class.allowed_values = &[("scope", &["universal", "target", "project"])];
            class.build_setting = true;
            class
        }
        // rules/genquery/GenQueryRule.java:37-85 (GENQUERY_SCOPE_TYPE_LIST is a
        // label list, BuildType.java:105-114).
        "genquery" => native_action_creating_rule("genquery")
            .add(mandatory(attr("scope", T::LabelList)))
            .add(attr("strict", T::Bool))
            .add(mandatory(attr("expression", T::String)))
            .add(attr("opts", T::StringList))
            .add(attr("compressed_output", T::Bool)),
        // rules/starlarkdocextract/StarlarkDocExtractRule.java:33-139 and
        // StarlarkDocExtract.java:72-78.
        "starlark_doc_extract" => {
            let mut class = native_action_creating_rule("starlark_doc_extract")
                .add(mandatory(attr("src", T::Label)))
                .override_attr(attr("deps", T::LabelList))
                .add(attr("symbol_names", T::StringList))
                .add(attr("render_main_repo_name", T::Bool))
                .add(attr("allow_unused_doc_comments", T::Bool));
            class.implicit_outputs = &["%{name}.binaryproto", "%{name}.textproto"];
            class
        }
        // rules/test/TestSuiteRule.java:30-120.
        "test_suite" => {
            let mut class = native_build_rule("test_suite")
                .override_attr(nonconfigurable(attr("testonly", T::Bool)))
                // TestSuiteRule.java:99-103: orderIndependent.
                .add(order_independent(nonconfigurable(attr(
                    "tests",
                    T::LabelList,
                ))));
            class.testonly_default = Some(true);
            class
        }
        // rules/java/JavaPluginsFlagAliasRule.java:47-69 (bazel/rules/JavaRules.java:58).
        "java_plugins_flag_alias" => native_build_rule("java_plugins_flag_alias"),
        // rules/cpp/CcLibcTopAlias.java:21-36 via LateBoundAlias.CommonAliasRule
        // (rules/LateBoundAlias.java:93-145); bazel/rules/CcRules.java:59.
        "cc_libc_top_alias" => native_build_rule("cc_libc_top_alias")
            .remove("licenses")
            .remove("distribs"),
        _ => return None,
    })
}

/// Names that `src/main/starlark/builtins_bzl/bazel/exports.bzl:22-37,53-58`
/// replace with `_removed_rule_failure` in both BUILD and `native`.
pub const REMOVED_RULES: &[&str] = &[
    "cc_binary",
    "cc_import",
    "cc_library",
    "cc_shared_library",
    "cc_static_library",
    "cc_test",
    "cc_toolchain",
    "cc_toolchain_alias",
    "fdo_prefetch_hints",
    "fdo_profile",
    "memprof_profile",
    "propeller_optimize",
    "objc_import",
    "objc_library",
];

/// `_removed_rule_failure`'s fail() message (exports.bzl:39-44).
pub const REMOVED_RULE_MESSAGE: &str = "\n         This rule has been removed from Bazel. Please add a `load()` statement for it.\n         This can also be done automatically by running:\n         buildifier --lint=fix <path-to-BUILD-or-bzl-file>\n         ";

/// Retained load-time state of one native rule instance.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct NativeTarget {
    pub rule_class: String,
    /// Explicitly supplied non-select values (None arguments are omitted).
    pub attrs: BTreeMap<String, AttrValue>,
    /// Retained `select()` values (BuildType.SelectorList).
    pub configurable: BTreeMap<String, Vec<ConfigurablePart>>,
    /// Effective `tags` (explicit or the class default).
    pub tags: Vec<String>,
    /// Labels in non-visibility attributes and select keys/branches; same-package
    /// ones are assumed input files (Package.java:897-920).
    pub input_labels: Vec<Label>,
}

impl NativeTarget {
    pub fn is_manual(&self) -> bool {
        // TargetUtils.hasManualTag (packages/TargetUtils.java:105-107).
        self.tags.iter().any(|tag| tag == "manual")
    }

    pub fn explicit(&self, name: &str) -> bool {
        self.attrs.contains_key(name) || self.configurable.contains_key(name)
    }

    pub fn labels(&self, name: &str) -> Vec<String> {
        match self.attrs.get(name) {
            Some(AttrValue::Labels(values)) => values.clone(),
            _ => Vec::new(),
        }
    }

    pub fn string(&self, name: &str) -> Option<&str> {
        match self.attrs.get(name) {
            Some(AttrValue::String(value)) => Some(value),
            _ => None,
        }
    }

    pub fn strings(&self, name: &str) -> Vec<String> {
        match self.attrs.get(name) {
            Some(AttrValue::Strings(values) | AttrValue::Outputs(values)) => values.clone(),
            _ => Vec::new(),
        }
    }
}

/// Successful instantiation of a native rule call.
pub struct Instance<'v> {
    pub name: String,
    pub visibility: Option<Value<'v>>,
    pub target: NativeTarget,
}

struct Converter<'a> {
    repositories: &'a RepositorySet,
    package: &'a PackageId,
    budget: HostBudget,
}

fn expected(ty: &str, value: Value<'_>, what: &str) -> String {
    // Type.ConversionException.message (packages/Type.java:297-307).
    format!(
        "expected {ty} for {what}, but got {} ({})",
        value.to_repr(),
        value.get_type()
    )
}

fn type_error(ty: NativeType, value: Value<'_>, what: &str) -> String {
    expected(&format!("value of type '{}'", ty.name()), value, what)
}

impl Converter<'_> {
    fn charge(&mut self, text: &str) -> anyhow::Result<()> {
        self.budget.node().map_err(anyhow::Error::msg)?;
        self.budget.string(text).map_err(anyhow::Error::msg)
    }

    /// BuildType.LabelType.convert (packages/BuildType.java:412-429) with the
    /// package's LabelConverter. Returns the canonical label text, or the raw
    /// text when its apparent repository is not visible: like Bazel's
    /// `@@[unknown repo ...]` labels that error only when used, resolution is
    /// then retried (and fails) during analysis.
    fn label(&mut self, value: Value<'_>, what: &str) -> anyhow::Result<String> {
        if let Some((repository, package, name)) = crate::starlark_types::label_parts(value) {
            for part in [repository, package, name] {
                self.charge(part)?;
            }
            return Ok(Label {
                repository: repository.to_owned(),
                package: package.to_owned(),
                name: name.to_owned(),
            }
            .to_string());
        }
        let Some(text) = value.unpack_str() else {
            bail!("{}", type_error(NativeType::String, value, what));
        };
        self.charge(text)?;
        crate::bazel_label::Parts::parse(text)
            .map_err(|error| anyhow!("invalid label '{text}' in {what}: {error}"))?;
        let bound = match self.repositories.resolution_size_bound(
            text,
            &self.package.repository,
            &self.package.package,
        ) {
            Ok(bound) => bound,
            Err(error) if deferred(text, &error) => return Ok(text.to_owned()),
            Err(error) => bail!("invalid label '{text}' in {what}: {error}"),
        };
        self.budget.bytes(bound).map_err(anyhow::Error::msg)?;
        match self
            .repositories
            .parse(text, &self.package.repository, &self.package.package)
        {
            Ok(label) => Ok(label.to_string()),
            Err(error) if deferred(text, &error) => Ok(text.to_owned()),
            Err(error) => bail!("invalid label '{text}' in {what}: {error}"),
        }
    }

    fn string(&mut self, value: Value<'_>, what: &str) -> anyhow::Result<String> {
        let Some(text) = value.unpack_str() else {
            bail!("{}", type_error(NativeType::String, value, what));
        };
        self.charge(text)?;
        Ok(text.to_owned())
    }

    fn int(&mut self, value: Value<'_>, what: &str) -> anyhow::Result<i32> {
        if let Some(number) = value.unpack_i32() {
            return Ok(number);
        }
        if value.get_type() == "int" {
            // IntegerType.convert (packages/Type.java:357-375).
            bail!(
                "for {what}, got {}, want value in signed 32-bit range",
                value.to_repr()
            );
        }
        bail!("{}", type_error(NativeType::Int, value, what))
    }

    /// ListType.convert accepts any Java `Iterable` (packages/Type.java:730-745):
    /// lists, tuples, sets and dicts (their keys).
    fn sequence<'v>(
        &mut self,
        ty: NativeType,
        value: Value<'v>,
        what: &str,
    ) -> anyhow::Result<Vec<Value<'v>>> {
        use starlark::values::UnpackValue;
        let items = if let Some(items) = crate::custom_rule::sequence_ref(value) {
            items.to_vec()
        } else if let Some(set) = starlark::values::set::SetRef::unpack_value_opt(value) {
            set.iter().collect()
        } else if let Some(dict) = DictRef::from_value(value) {
            dict.keys().collect()
        } else {
            bail!("{}", type_error(ty, value, what));
        };
        self.budget.node().map_err(anyhow::Error::msg)?;
        Ok(items)
    }

    fn convert(
        &mut self,
        ty: NativeType,
        value: Value<'_>,
        what: &str,
    ) -> anyhow::Result<AttrValue> {
        Ok(match ty {
            NativeType::String => AttrValue::String(self.string(value, what)?),
            // BooleanType.convert (packages/Type.java:411-429).
            NativeType::Bool => AttrValue::Bool(match value.unpack_bool() {
                Some(value) => value,
                None => match value.unpack_i32() {
                    Some(0) => false,
                    Some(1) => true,
                    _ => bail!("{}", expected("one of [False, True, 0, 1]", value, what)),
                },
            }),
            NativeType::Int => AttrValue::Int(self.int(value, what)?),
            // TriStateType.convert (packages/BuildType.java:900-922): -1/0/1.
            NativeType::TriState => AttrValue::Int(match value.unpack_bool() {
                Some(value) => i32::from(value),
                None => match self.int(value, what)? {
                    number @ -1..=1 => number,
                    _ => bail!(
                        "{}",
                        type_error(
                            NativeType::TriState,
                            value,
                            "TriState values is not one of [-1, 0, 1]"
                        )
                    ),
                },
            }),
            NativeType::Label | NativeType::NodepLabel => {
                AttrValue::Label(Some(self.label(value, what)?))
            }
            NativeType::StringList | NativeType::OutputList | NativeType::License => {
                // LicenseType.convert delegates to STRING_LIST.convert
                // (packages/BuildType.java:504-512), so errors name 'list(string)'.
                let shape = if ty == NativeType::License {
                    NativeType::StringList
                } else {
                    ty
                };
                let items = self.sequence(shape, value, what)?;
                let mut values = Vec::with_capacity(items.len());
                for (index, item) in items.iter().enumerate() {
                    values.push(self.string(*item, &format!("element {index} of {what}"))?);
                }
                if ty == NativeType::License {
                    parse_license(&values)?;
                }
                if ty == NativeType::OutputList {
                    AttrValue::Outputs(values)
                } else {
                    AttrValue::Strings(values)
                }
            }
            NativeType::LabelList | NativeType::NodepLabelList => {
                let items = self.sequence(ty, value, what)?;
                let mut values = Vec::with_capacity(items.len());
                for (index, item) in items.iter().enumerate() {
                    values.push(self.label(*item, &format!("element {index} of {what}"))?);
                }
                AttrValue::Labels(values)
            }
            NativeType::StringDict => {
                let dict = DictRef::from_value(value)
                    .ok_or_else(|| anyhow!("{}", type_error(ty, value, what)))?;
                let mut entries: Vec<(String, String)> = Vec::new();
                for (key, item) in dict.iter() {
                    let key = self.string(key, "dict key element")?;
                    let item = self.string(item, "dict value element")?;
                    entries.retain(|(seen, _)| *seen != key);
                    entries.push((key, item));
                }
                AttrValue::StringDict(entries)
            }
            NativeType::LabelListDict => {
                let dict = DictRef::from_value(value)
                    .ok_or_else(|| anyhow!("{}", type_error(ty, value, what)))?;
                let mut entries: Vec<(String, Vec<String>)> = Vec::new();
                for (key, item) in dict.iter() {
                    let key = self.string(key, "dict key element")?;
                    let items = self.sequence(NativeType::LabelList, item, "dict value element")?;
                    let mut labels = Vec::with_capacity(items.len());
                    for (index, element) in items.iter().enumerate() {
                        labels.push(
                            self.label(
                                *element,
                                &format!("element {index} of dict value element"),
                            )?,
                        );
                    }
                    entries.retain(|(seen, _)| *seen != key);
                    entries.push((key, labels));
                }
                AttrValue::StringListDict(entries)
            }
            // LabelKeyedDictType.convert (packages/BuildType.java:449-490).
            NativeType::LabelKeyedStringDict => {
                let dict = DictRef::from_value(value)
                    .ok_or_else(|| anyhow!("{}", type_error(ty, value, what)))?;
                let mut entries: Vec<(String, String)> = Vec::new();
                let mut originals: Vec<(String, Vec<String>)> = Vec::new();
                for (key, item) in dict.iter() {
                    let label = self.label(key, "dict key element")?;
                    let item = self.string(item, "dict value element")?;
                    match originals.iter_mut().find(|(seen, _)| *seen == label) {
                        Some((_, keys)) => keys.push(key.to_repr()),
                        None => originals.push((label.clone(), vec![key.to_repr()])),
                    }
                    entries.retain(|(seen, _)| *seen != label);
                    entries.push((label, item));
                }
                let duplicates = originals
                    .iter()
                    .filter(|(_, keys)| keys.len() > 1)
                    .map(|(label, keys)| {
                        let label = Label::parse_canonical(label)
                            .map(|label| crate::rule_declarations::canonical_form(&label))
                            .unwrap_or_else(|_| label.clone());
                        format!(" {label} (as [{}])", keys.join(", "))
                    })
                    .collect::<Vec<_>>();
                if !duplicates.is_empty() {
                    bail!("duplicate labels in {what}:{}", duplicates.join(","));
                }
                AttrValue::LabelKeyedStringDict(entries)
            }
        })
    }
}

/// A syntactically valid label of an apparent repository whose resolution
/// failed (not visible, or no repository mapping is available).
fn deferred(text: &str, _error: &str) -> bool {
    text.starts_with('@') && !text.starts_with("@@") && !text.starts_with("@//")
}

/// License.parseLicense (packages/License.java:115-142).
fn parse_license(values: &[String]) -> anyhow::Result<()> {
    const TYPES: [&str; 8] = [
        "BY_EXCEPTION_ONLY",
        "RESTRICTED",
        "RESTRICTED_IF_STATICALLY_LINKED",
        "RECIPROCAL",
        "NOTICE",
        "PERMISSIVE",
        "UNENCUMBERED",
        "NONE",
    ];
    for value in values {
        if let Some(exception) = value.strip_prefix("exception=") {
            crate::bazel_label::Parts::parse(exception).map_err(anyhow::Error::msg)?;
            if !exception.starts_with("//") && !exception.starts_with('@') {
                bail!("invalid label: {exception}");
            }
        } else if !TYPES.contains(&value.to_ascii_uppercase().as_str()) {
            bail!("invalid license type: '{value}'");
        }
    }
    Ok(())
}

/// `BuildType.LICENSE.convert(value, what)` for `licenses()`/package args.
pub(crate) fn convert_license(value: Value<'_>, what: &str) -> anyhow::Result<Vec<String>> {
    let items = crate::custom_rule::sequence_ref(value)
        .ok_or_else(|| anyhow!("{}", type_error(NativeType::StringList, value, what)))?;
    let mut values = Vec::with_capacity(items.len());
    for (index, item) in items.iter().enumerate() {
        values.push(
            item.unpack_str()
                .ok_or_else(|| {
                    anyhow!(
                        "{}",
                        type_error(
                            NativeType::String,
                            *item,
                            &format!("element {index} of {what}")
                        )
                    )
                })?
                .to_owned(),
        );
    }
    parse_license(&values)?;
    Ok(values)
}

/// `Type.convert` of a package/repo argument with the package's label converter.
pub(crate) fn convert_value(
    repositories: &RepositorySet,
    package: &PackageId,
    ty: NativeType,
    value: Value<'_>,
    what: &str,
) -> anyhow::Result<AttrValue> {
    Converter {
        repositories,
        package,
        budget: HostBudget::default(),
    }
    .convert(ty, value, what)
}

/// The empty value of a type (Type.getDefaultValue), used for simplified
/// `select({"//conditions:default": None})` branches.
pub(crate) fn type_default(ty: NativeType) -> Option<AttrValue> {
    Some(match ty {
        NativeType::String => AttrValue::String(String::new()),
        NativeType::StringList | NativeType::License => AttrValue::Strings(Vec::new()),
        NativeType::OutputList => AttrValue::Outputs(Vec::new()),
        NativeType::StringDict => AttrValue::StringDict(Vec::new()),
        NativeType::Bool => AttrValue::Bool(false),
        NativeType::Int => AttrValue::Int(0),
        NativeType::TriState => AttrValue::Int(-1),
        NativeType::Label | NativeType::NodepLabel => return None,
        NativeType::LabelList | NativeType::NodepLabelList => AttrValue::Labels(Vec::new()),
        NativeType::LabelKeyedStringDict => AttrValue::LabelKeyedStringDict(Vec::new()),
        NativeType::LabelListDict => AttrValue::StringListDict(Vec::new()),
    })
}

/// `Type.concat` for the concatenable types (crate::configured_attributes::concat).
fn concat(ty: NativeType, values: Vec<AttrValue>) -> AttrValue {
    let first = values
        .first()
        .cloned()
        .expect("at least one select element");
    crate::configured_attributes::concat(values).unwrap_or_else(|| {
        debug_assert!(false, "concat of mismatched {ty:?} values");
        first
    })
}

const DEFAULT_CONDITION: &str = "//conditions:default";

/// Value of a converted attribute: plain or a retained select.
pub(crate) enum Converted {
    Plain(AttrValue),
    /// `select({"//conditions:default": None})` of a type whose default is
    /// null: the attribute stays unset (AttributeProvider.java:269-272).
    Unset,
    Select(Vec<ConfigurablePart>),
}

/// BuildType.selectableConvert with simplifyUnconditionalSelects
/// (packages/BuildType.java:165-211; the flag defaults to true,
/// BuildLanguageOptions.java:755-756) and the Selector constructor
/// (:702-739, SelectBranchMessage :654-668).
fn convert_select(
    converter: &mut Converter<'_>,
    ty: NativeType,
    pieces: Vec<crate::rule_declarations::SelectPiece<'_>>,
    what: &str,
) -> anyhow::Result<Converted> {
    use crate::rule_declarations::SelectPiece;
    if pieces.len() > 1 && !ty.concatenable() {
        bail!("type '{}' doesn't support select concatenation", ty.name());
    }
    let mut parts = Vec::with_capacity(pieces.len());
    let mut unconditional = true;
    for piece in pieces {
        converter.budget.node().map_err(anyhow::Error::msg)?;
        match piece {
            SelectPiece::Plain(value) => {
                parts.push(ConfigurablePart::Plain(converter.convert(ty, value, what)?))
            }
            SelectPiece::Select {
                branches,
                no_match_error,
            } => {
                let mut owned = Vec::with_capacity(branches.len());
                for (key, value) in branches {
                    let key = match key.unpack_str() {
                        Some(DEFAULT_CONDITION) => DEFAULT_CONDITION.to_owned(),
                        _ => converter.label(key, what)?,
                    };
                    let shown = Label::parse_canonical(&key)
                        .map(|label| crate::rule_declarations::canonical_form(&label))
                        .unwrap_or_else(|_| key.clone());
                    let branch = if value.is_none() {
                        None
                    } else {
                        Some(converter.convert(
                            ty,
                            value,
                            &format!(
                                "each branch in select expression of {what} (including '{shown}')"
                            ),
                        )?)
                    };
                    owned.push((key, branch));
                }
                unconditional &= owned.len() == 1 && owned[0].0 == DEFAULT_CONDITION;
                parts.push(ConfigurablePart::Select {
                    branches: owned,
                    no_match_error,
                });
            }
        }
    }
    if !unconditional {
        return Ok(Converted::Select(parts));
    }
    let mut values = Vec::with_capacity(parts.len());
    for part in parts {
        match part {
            ConfigurablePart::Plain(value) => values.push(Some(value)),
            ConfigurablePart::Select { mut branches, .. } => values.push(
                branches
                    .pop()
                    .and_then(|(_, value)| value)
                    .or_else(|| type_default(ty)),
            ),
        }
    }
    if values.len() == 1 && values[0].is_none() {
        return Ok(Converted::Unset);
    }
    // A null element inside a concatenation cannot occur for concatenable types.
    Ok(Converted::Plain(concat(
        ty,
        values
            .into_iter()
            .map(|value| value.expect("concatenable default"))
            .collect(),
    )))
}

impl Converted {
    /// Record the value in a target (unset values stay absent).
    pub(crate) fn store(self, key: &str, target: &mut NativeTarget) {
        match self {
            Converted::Plain(value) => {
                target.attrs.insert(key.to_owned(), value);
            }
            Converted::Unset => {}
            Converted::Select(parts) => {
                target.configurable.insert(key.to_owned(), parts);
            }
        }
    }
}

impl Converter<'_> {
    /// BuildType.convertFromBuildLangType (packages/BuildType.java:220-238) of
    /// one keyword argument, with the `LABEL: MESSAGE` prefix of
    /// AttributeProvider.populateDefinedRuleAttributeValues
    /// (AttributeProvider.java:254-265). `class_name` is the attribute
    /// provider's name (the rule class), used in `attribute 'K' of 'CLASS'`.
    fn convert_argument(
        &mut self,
        class_name: &str,
        shown: &str,
        attribute: &NativeAttr,
        value: Value<'_>,
    ) -> anyhow::Result<Converted> {
        let key = attribute.name;
        let what = format!("attribute '{key}' of '{class_name}'");
        let converted = if let Some(pieces) = crate::rule_declarations::select_pieces(value) {
            if !attribute.configurable {
                bail!("{shown}: attribute \"{key}\" is not configurable");
            }
            convert_select(self, attribute.ty, pieces, &what)
                .map_err(|error| anyhow!("{shown}: {error}"))?
        } else {
            Converted::Plain(
                self.convert(attribute.ty, value, &what)
                    .map_err(|error| anyhow!("{shown}: {error}"))?,
            )
        };
        // BuildType.convertFromBuildLangType sorts every converted List of an
        // orderIndependent attribute (BuildType.java:248-253), including a
        // simplified select; retained selects (SelectorList) stay unsorted.
        Ok(match converted {
            Converted::Plain(mut value) if attribute.order_independent => {
                sort_order_independent(&mut value);
                Converted::Plain(value)
            }
            other => other,
        })
    }
}

/// Java `String.compareTo` (UTF-16 code unit order).
pub(crate) fn java_string_cmp(a: &str, b: &str) -> std::cmp::Ordering {
    a.encode_utf16().cmp(b.encode_utf16())
}

/// `Label.compareTo` (cmdline/Label.java:677-685, PackageIdentifier.java:295-307,
/// PathFragment.java:514-516): repository name, package path, then name.
pub(crate) fn label_cmp(a: &Label, b: &Label) -> std::cmp::Ordering {
    java_string_cmp(&a.repository, &b.repository)
        .then_with(|| java_string_cmp(&a.package, &b.package))
        .then_with(|| java_string_cmp(&a.name, &b.name))
}

/// `Ordering.natural().sortedCopy` of a converted list (stable, duplicates kept).
fn sort_order_independent(value: &mut AttrValue) {
    match value {
        AttrValue::Strings(values) => values.sort_by(|a, b| java_string_cmp(a, b)),
        AttrValue::Labels(values) => values.sort_by(|a, b| {
            match (Label::parse_canonical(a), Label::parse_canonical(b)) {
                (Ok(a), Ok(b)) => label_cmp(&a, &b),
                // Unresolved apparent-repository labels (deferred) sort by text.
                _ => java_string_cmp(a, b),
            }
        }),
        _ => {}
    }
}

/// Converter for the built-in attributes of a Starlark rule instance, using
/// the BUILD file's package and repository mapping.
pub(crate) struct BuiltinConverter<'a>(Converter<'a>);

impl<'a> BuiltinConverter<'a> {
    pub(crate) fn new(repositories: &'a RepositorySet, package: &'a PackageId) -> Self {
        Self(Converter {
            repositories,
            package,
            budget: HostBudget::default(),
        })
    }

    pub(crate) fn convert_argument(
        &mut self,
        class_name: &str,
        shown: &str,
        attribute: &NativeAttr,
        value: Value<'_>,
    ) -> anyhow::Result<Converted> {
        self.0.convert_argument(class_name, shown, attribute, value)
    }
}

/// Assumed input files of a target (`Rule.getLabels()`):
/// AggregatingAttributeMapper.visitLabels (packages/AggregatingAttributeMapper.java:96-109)
/// skips NODEP_LABEL(_LIST) attribute values (`nodep`). Select keys of every
/// attribute, NODEP ones included, are visited through `$config_dependencies`
/// (AttributeProvider.java:443-466, a LABEL_LIST, BaseRuleClasses.java:288-290);
/// the genuine capture confirms
/// (reference_receipt B/nodep_platform_allowed_toolchain_types_select_key).
pub(crate) fn input_labels(target: &NativeTarget, nodep: impl Fn(&str) -> bool) -> Vec<Label> {
    let mut labels = Vec::new();
    for (key, value) in &target.attrs {
        if !nodep(key) {
            collect_labels(value, &mut labels);
        }
    }
    for (attribute, parts) in &target.configurable {
        let values = !nodep(attribute);
        for part in parts {
            match part {
                ConfigurablePart::Plain(value) => {
                    if values {
                        collect_labels(value, &mut labels)
                    }
                }
                ConfigurablePart::Select { branches, .. } => {
                    for (key, value) in branches {
                        if key != DEFAULT_CONDITION
                            && let Ok(label) = Label::parse_canonical(key)
                        {
                            labels.push(label);
                        }
                        if values && let Some(value) = value {
                            collect_labels(value, &mut labels);
                        }
                    }
                }
            }
        }
    }
    let mut seen = BTreeSet::new();
    labels.retain(|label| seen.insert(label.clone()));
    labels
}

/// SpellChecker.didYouMean (net/starlark/java/spelling/SpellChecker.java).
pub(crate) fn did_you_mean(input: &str, words: &[&str]) -> String {
    fn edit_distance(s1: &[u8], s2: &[u8], max: usize) -> Option<usize> {
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
    let mut best = None;
    let mut best_distance = 5.min(input.len().div_ceil(2));
    let input = input.to_lowercase();
    for word in words {
        if let Some(distance) = edit_distance(
            input.as_bytes(),
            word.to_lowercase().as_bytes(),
            best_distance,
        ) && distance < best_distance
        {
            best_distance = distance;
            best = Some(*word);
        }
    }
    best.map(|word| format!(" (did you mean '{word}'?)"))
        .unwrap_or_default()
}

fn collect_labels(value: &AttrValue, labels: &mut Vec<Label>) {
    let mut push = |text: &str| {
        if let Ok(label) = Label::parse_canonical(text) {
            labels.push(label);
        }
    };
    match value {
        AttrValue::Label(Some(text)) => push(text),
        AttrValue::Labels(values) => values.iter().for_each(|text| push(text)),
        AttrValue::LabelKeyedStringDict(entries) => entries.iter().for_each(|(text, _)| push(text)),
        AttrValue::StringKeyedLabelDict(entries) => entries.iter().for_each(|(_, text)| push(text)),
        AttrValue::StringListDict(entries) | AttrValue::LabelListDict(entries) => entries
            .iter()
            .flat_map(|(_, values)| values)
            .for_each(|text| push(text)),
        _ => {}
    }
}

/// RuleFactory.createRule (packages/RuleFactory.java:59-108) and
/// AttributeProvider.populateRuleAttributeValues (AttributeProvider.java:162-328).
/// `--incompatible_fail_on_unknown_attributes` defaults to true
/// (BuildLanguageOptions.java:634-636), so unknown attributes are errors even
/// when their value is None.
pub(crate) fn instantiate<'v>(
    class: &RuleClass,
    arguments: &[(String, Value<'v>)],
    repositories: &RepositorySet,
    package: &PackageId,
) -> anyhow::Result<Instance<'v>> {
    let name = match arguments.iter().find(|(key, _)| key == "name") {
        None => bail!("{} rule has no 'name' attribute", class.name),
        Some((_, value)) => value
            .unpack_str()
            .ok_or_else(|| anyhow!("{} 'name' attribute must be a string", class.name))?,
    };
    // RuleFactory.java:75-81 around Label.create (cmdline/Label.java:303-311).
    if let Err(error) = crate::bazel_label::validate_and_process_target_name(&package.package, name)
    {
        bail!("illegal rule name: {name}: {error}");
    }
    let label = Label {
        repository: package.repository.clone(),
        package: package.package.clone(),
        name: name.to_owned(),
    };
    let shown = crate::rule_declarations::canonical_form(&label);
    let mut converter = Converter {
        repositories,
        package,
        budget: HostBudget::default(),
    };
    let mut target = NativeTarget {
        rule_class: class.name.to_owned(),
        ..NativeTarget::default()
    };
    let mut visibility = None;
    let documented = class
        .attrs
        .iter()
        .filter(|attribute| attribute.documented)
        .map(|attribute| attribute.name)
        .collect::<Vec<_>>();
    for (key, value) in arguments {
        if key == "name" {
            continue;
        }
        // applicable_licenses is an alternate spelling (AttributeProvider.java:213-217).
        let key = if key == "applicable_licenses" {
            "package_metadata"
        } else {
            key.as_str()
        };
        let Some(attribute) = class.attribute(key) else {
            bail!(
                "{shown}: no such attribute '{key}' in '{}' rule{}",
                class.name,
                did_you_mean(
                    key,
                    &std::iter::once("name")
                        .chain(documented.iter().copied())
                        .collect::<Vec<_>>()
                )
            );
        };
        if value.is_none() {
            continue;
        }
        if key == "licenses" && class.ignore_licenses {
            continue;
        }
        if key == "visibility" && crate::rule_declarations::select_pieces(*value).is_none() {
            visibility = Some(*value);
            continue;
        }
        converter
            .convert_argument(class.name, &shown, attribute, *value)?
            .store(key, &mut target);
    }
    // AttributeProvider.checkAllowedValues (AttributeProvider.java:478-497).
    for (name, allowed) in class.allowed_values {
        let mut values = Vec::new();
        if let Some(value) = target.attrs.get(*name) {
            values.push(value);
        }
        for value in values {
            if let AttrValue::String(text) = value
                && !allowed.contains(&text.as_str())
            {
                let declaration = crate::custom_rule::AttrDeclaration {
                    values: allowed
                        .iter()
                        .map(|choice| crate::custom_rule::AllowedValue {
                            value: Some(AttrValue::String((*choice).to_owned())),
                            display: (*choice).to_owned(),
                        })
                        .collect(),
                    ..Default::default()
                };
                bail!(
                    "{shown}: invalid value in '{name}' attribute: {}",
                    declaration.allowed_values_reason(value)
                );
            }
        }
    }
    for attribute in &class.attrs {
        if attribute.mandatory && !target.explicit(attribute.name) {
            bail!(
                "{shown}: missing value for mandatory attribute '{}' in '{}' rule",
                attribute.name,
                class.name
            );
        }
    }
    target.tags = match target.attrs.get("tags") {
        Some(AttrValue::Strings(tags)) => tags.clone(),
        _ => class
            .default_tags
            .iter()
            .map(|tag| (*tag).to_owned())
            .collect(),
    };
    // The build-setting default of label_flag/label_setting is still visited
    // through the `:alias` attribute's rule-based default
    // (rules/LabelBuildSettings.java:62-90).
    target.input_labels = input_labels(&target, |key| {
        class.attribute(key).is_some_and(|attribute| {
            matches!(
                attribute.ty,
                NativeType::NodepLabel | NativeType::NodepLabelList
            ) && !(class.build_setting && key == "build_setting_default")
        })
    });
    Ok(Instance {
        name: name.to_owned(),
        visibility,
        target,
    })
}

/// Pure attribute checks that Bazel's configured-target factories perform
/// before anything configuration-dependent; native analysis reports them
/// before its explicit "analysis not implemented" failure. Messages use
/// RuleContext.ruleError/attributeError (analysis/EventHandlingErrorReporter.java:81, 95-97).
pub(crate) fn analysis_error(label: &Label, target: &NativeTarget) -> Option<String> {
    let shown = crate::rule_declarations::canonical_form(label);
    let dict = |name: &str| match target.attrs.get(name) {
        Some(AttrValue::StringDict(entries) | AttrValue::LabelKeyedStringDict(entries)) => {
            entries.clone()
        }
        _ => Vec::new(),
    };
    match target.rule_class.as_str() {
        // ConfigSetting.create (rules/config/ConfigSetting.java:116-136, 384-397).
        "config_setting" => {
            let values = dict("values");
            if let Some((key, _)) = values.iter().find(|(key, _)| {
                key.starts_with('@') || key.starts_with("//") || key.starts_with(':')
            }) {
                return Some(format!(
                    "in values attribute of config_setting rule {shown}: '{key}' is not a valid setting name, but appears to be a label. Did you mean to place it in flag_values instead?"
                ));
            }
            if values.is_empty()
                && dict("define_values").is_empty()
                && dict("flag_values").is_empty()
                && target.labels("constraint_values").is_empty()
            {
                return Some(format!(
                    "in config_setting rule {shown}: Either values, flag_values or constraint_values must be specified and non-empty"
                ));
            }
            None
        }
        // JavaPluginsFlagAlias.create (rules/java/JavaPluginsFlagAliasRule.java:42-45, 80-83).
        "java_plugins_flag_alias" => {
            let allowed = (label.repository.is_empty() || label.repository == "bazel_tools")
                && label.package == "tools/jdk"
                && label.name == "java_plugins_flag_alias";
            (!allowed).then(|| {
                format!(
                    "in java_plugins_flag_alias rule {shown}: Rule {shown} cannot use private rule"
                )
            })
        }
        _ => None,
    }
}

/// Attributes of `filegroup`/`genrule`/`config_setting`/`alias`/`label_flag`/
/// `label_setting` whose analysis the
/// native profile models; any other set attribute makes analysis fail
/// explicitly instead of silently ignoring Bazel semantics. Evaluated on the
/// configured (select-resolved) target (crate::configured_attributes).
pub(crate) fn analysis_gaps(target: &NativeTarget) -> Vec<String> {
    let mut gaps = Vec::new();
    // Attributes without effect on the native local action graph: metadata,
    // diagnostics-only, or Windows-only commands on this Unix host profile.
    // `testonly` only drives the prerequisite check, which crate::engine runs
    // (CommonPrerequisiteValidator.java:730-776).
    let neutral: &[&str] = match target.rule_class.as_str() {
        "filegroup" => &[
            "srcs",
            "tags",
            "deprecation",
            "testonly",
            "features",
            "licenses",
            "distribs",
            "output_licenses",
            "generator_name",
            "generator_function",
            "generator_location",
        ],
        // `tools` are built in the exec configuration (crate::engine).
        "genrule" => &[
            "srcs",
            "tools",
            "outs",
            "cmd",
            "cmd_bat",
            "cmd_ps",
            "tags",
            "deprecation",
            "testonly",
            "features",
            "licenses",
            "distribs",
            "output_licenses",
            "message",
            "local",
            "output_to_bindir",
            "exec_properties",
            "generator_name",
            "generator_function",
            "generator_location",
        ],
        // Alias.create reads only `actual` (rules/Alias.java:40-56); testonly
        // on an alias is not checked (CommonPrerequisiteValidator.java:730-738).
        "alias" => &[
            "actual",
            "tags",
            "deprecation",
            "testonly",
            "features",
            "generator_name",
            "generator_function",
            "generator_location",
        ],
        // LateBoundAlias.create (rules/LateBoundAlias.java:47-79): the value or
        // build_setting_default; `scope` only affects transitions, `help` is
        // documentation.
        "label_flag" | "label_setting" => &[
            "build_setting_default",
            "scope",
            "help",
            "tags",
            "deprecation",
            "testonly",
            "features",
            "generator_name",
            "generator_function",
            "generator_location",
        ],
        // ConstraintSetting.create / ConstraintValue.create
        // (rules/platform/ConstraintSetting.java:227-304, ConstraintValue.java:338-353).
        "constraint_setting" => &[
            "default_constraint_value",
            "refines_constraint_value",
            "tags",
            "deprecation",
            "testonly",
            "features",
            "licenses",
            "distribs",
            "generator_name",
            "generator_function",
            "generator_location",
        ],
        "constraint_value" => &[
            "constraint_setting",
            "tags",
            "deprecation",
            "testonly",
            "features",
            "licenses",
            "distribs",
            "generator_name",
            "generator_function",
            "generator_location",
        ],
        // Platform.create (rules/platform/Platform.java:44-120); `flags` fail
        // where the platform is the target platform (crate::engine).
        "platform" => &[
            "constraint_values",
            "parents",
            "remote_execution_properties",
            "exec_properties",
            "flags",
            "required_settings",
            "missing_toolchain_error",
            "check_toolchain_types",
            "allowed_toolchain_types",
            "tags",
            "deprecation",
            "testonly",
            "features",
            "licenses",
            "distribs",
            "generator_name",
            "generator_function",
            "generator_location",
        ],
        // ToolchainType.create reads only `no_match_error` (rules/ToolchainType.java:40-52).
        "toolchain_type" => &[
            "no_match_error",
            "tags",
            "deprecation",
            "testonly",
            "features",
            "generator_name",
            "generator_function",
            "generator_location",
        ],
        // Toolchain.create (rules/platform/Toolchain.java:42-110); the
        // `target_settings` labels must be config_setting rules
        // (ToolchainRule.java:97-103, crate::engine).
        "toolchain" => &[
            "toolchain_type",
            "exec_compatible_with",
            "target_compatible_with",
            "use_target_platform_constraints",
            "target_settings",
            "toolchain",
            "tags",
            "deprecation",
            "testonly",
            "features",
            "licenses",
            "distribs",
            "generator_name",
            "generator_function",
            "generator_location",
        ],
        // ConfigSetting.create reads only these (rules/config/ConfigSetting.java:111-258).
        "config_setting" => &[
            "values",
            "define_values",
            "flag_values",
            "constraint_values",
            "tags",
            "deprecation",
            "testonly",
            "features",
            "distribs",
            "generator_name",
            "generator_function",
            "generator_location",
        ],
        _ => &[],
    };
    for (key, value) in &target.attrs {
        // `package_metadata` edges are analyzed in the no-config
        // configuration by crate::engine (Rule::package_metadata).
        if neutral.contains(&key.as_str())
            || key == "package_metadata"
            || crate::custom_rule::AttrKind::is_empty_value(value)
        {
            continue;
        }
        let neutral_value = match (key.as_str(), value) {
            ("executable", AttrValue::Bool(false))
            | ("heuristic_label_expansion", AttrValue::Bool(false)) => true,
            // stamp=0 is the genrule default (BazelGenRuleRule.java:47).
            ("stamp", AttrValue::Int(0)) => true,
            ("cmd_bash", _) => false,
            _ => false,
        };
        if !neutral_value {
            gaps.push(format!("attribute '{key}' = {}", describe(value)));
        }
    }
    gaps
}

pub(crate) fn describe(value: &AttrValue) -> String {
    match value {
        AttrValue::String(text) => format!("{text:?}"),
        AttrValue::Bool(value) => if *value { "True" } else { "False" }.to_owned(),
        AttrValue::Int(value) => value.to_string(),
        AttrValue::Strings(values) | AttrValue::Labels(values) | AttrValue::Outputs(values) => {
            format!("{values:?}")
        }
        other => format!("{other:?}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registered_classes_build_and_spell_checker_matches_bazel() {
        for name in RULE_CLASSES {
            let class = rule_class(name).unwrap();
            assert_eq!(class.name, *name);
        }
        assert!(rule_class("cc_library").is_none());
        assert_eq!(
            did_you_mean("acutal", &["name", "actual"]),
            " (did you mean 'actual'?)"
        );
        assert_eq!(did_you_mean("zzzzzzzz", &["name", "actual"]), "");
        // Exempted constraint attributes are absent; toolchain re-adds its own.
        let toolchain = rule_class("toolchain").unwrap();
        assert!(toolchain.attribute("compatible_with").is_none());
        assert!(
            !toolchain
                .attribute("target_compatible_with")
                .unwrap()
                .configurable
        );
        assert!(toolchain.attribute("target_settings").unwrap().configurable);
        assert_eq!(toolchain.default_tags, ["manual"]);
        assert!(
            rule_class("alias")
                .unwrap()
                .attribute("actual")
                .unwrap()
                .configurable
        );
        assert!(rule_class("alias").unwrap().attribute("licenses").is_none());
        assert!(
            !rule_class("genrule")
                .unwrap()
                .attribute("outs")
                .unwrap()
                .configurable
        );
        assert!(
            rule_class("genrule")
                .unwrap()
                .attribute("cmd")
                .unwrap()
                .configurable
        );
        assert!(rule_class("genrule").unwrap().attribute("deps").is_none());
        assert!(
            rule_class("platform")
                .unwrap()
                .attribute("package_metadata")
                .is_none()
        );
        let flag = rule_class("label_flag").unwrap();
        assert!(
            !flag
                .attribute("build_setting_default")
                .unwrap()
                .configurable
        );
        assert!(flag.attribute("build_setting_default").unwrap().mandatory);
    }

    #[test]
    fn starlark_builtins_follow_starlark_rule_class_functions() {
        let base = starlark_builtins(StarlarkRuleKind::default());
        let names = base
            .attrs()
            .map(|attribute| attribute.name)
            .collect::<Vec<_>>();
        assert_eq!(
            names,
            [
                "expect_failure",
                "visibility",
                "transitive_configs",
                "deprecation",
                "tags",
                "generator_name",
                "generator_function",
                "generator_location",
                "testonly",
                "features",
                "compatible_with",
                "restricted_to",
                "package_metadata",
                "aspect_hints",
                "toolchains",
                "exec_properties",
                "exec_compatible_with",
                "exec_group_compatible_with",
                "target_compatible_with",
            ]
        );
        // Shared with every native BUILD rule (commonCoreAndStarlarkAttributes).
        let filegroup = rule_class("filegroup").unwrap();
        for attribute in base.attrs().filter(|attribute| {
            !matches!(
                attribute.name,
                "expect_failure"
                    | "toolchains"
                    | "exec_properties"
                    | "exec_compatible_with"
                    | "exec_group_compatible_with"
            )
        }) {
            let native = filegroup.attribute(attribute.name).unwrap();
            assert_eq!(
                (
                    native.ty,
                    native.configurable,
                    native.documented,
                    native.order_independent
                ),
                (
                    attribute.ty,
                    attribute.configurable,
                    attribute.documented,
                    attribute.order_independent
                ),
                "{}",
                attribute.name
            );
        }
        let configurable = base
            .attrs()
            .filter(|attribute| attribute.configurable)
            .map(|attribute| attribute.name)
            .collect::<Vec<_>>();
        assert_eq!(
            configurable,
            [
                "expect_failure",
                "features",
                "aspect_hints",
                "toolchains",
                "exec_properties",
                "target_compatible_with"
            ]
        );
        assert!(base.attribute("licenses").is_none() && base.attribute("distribs").is_none());
        assert!(base.contains("name") && base.contains("$config_dependencies"));
        assert!(base.contains(":action_listener") && !base.contains("$action_listener"));
        assert_eq!(base.testonly_default, None);
        let candidates = base.candidates();
        assert_eq!(candidates[..3], ["name", "expect_failure", "visibility"]);
        assert!(!candidates.contains(&"generator_name"));
        assert!(!candidates.contains(&"$config_dependencies"));
        assert_eq!(did_you_mean("config_dependencies", &candidates), "");
        assert_eq!(
            base.public_name_conflict("$action_listener"),
            Some(":action_listener")
        );
        assert_eq!(base.public_name_conflict(":action_listener"), None);
        assert_eq!(base.public_name_conflict("$tool"), None);
        assert!(base.attribute("tags").unwrap().order_independent);
        assert!(base.attribute("features").unwrap().order_independent);
        assert!(base.attribute("visibility").unwrap().order_independent);
        assert!(
            base.attribute("transitive_configs")
                .unwrap()
                .order_independent
        );
        assert!(!base.attribute("toolchains").unwrap().order_independent);
        assert!(
            rule_class("test_suite")
                .unwrap()
                .attribute("tests")
                .unwrap()
                .order_independent
        );
        let test = starlark_builtins(StarlarkRuleKind {
            test: true,
            ..Default::default()
        });
        assert_eq!(test.testonly_default, Some(true));
        assert!(!test.attribute("size").unwrap().configurable);
        assert!(test.attribute("shard_count").unwrap().configurable);
        assert!(test.contains("$test_runtime") && test.contains("$is_executable"));
        assert!(test.attribute("output_licenses").is_none());
        let binary = starlark_builtins(StarlarkRuleKind {
            executable: true,
            ..Default::default()
        });
        assert!(binary.attribute("args").is_some() && binary.attribute("size").is_none());
        assert!(binary.attribute("output_licenses").is_some());
        let resolution = starlark_builtins(StarlarkRuleKind {
            dependency_resolution: true,
            ..Default::default()
        });
        for removed in [
            "aspect_hints",
            "toolchains",
            "exec_compatible_with",
            "exec_group_compatible_with",
            "target_compatible_with",
            "compatible_with",
            "restricted_to",
            "package_metadata",
            "$config_dependencies",
            ":action_listener",
        ] {
            assert!(!resolution.contains(removed), "{removed}");
        }
        assert!(resolution.attribute("tags").is_some());
    }

    #[test]
    fn license_parsing_matches_license_java() {
        assert!(parse_license(&["notice".into(), "Reciprocal".into()]).is_ok());
        assert_eq!(
            parse_license(&["bogus".into()]).unwrap_err().to_string(),
            "invalid license type: 'bogus'"
        );
    }
}
