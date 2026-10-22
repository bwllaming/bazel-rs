//! Starlark-defined 1:1 configuration transitions (N18c, docs/CONFIGURATION.md
//! "Starlark transitions"): `transition(implementation, inputs, outputs)`
//! objects applied as incoming rule transitions (`rule(cfg = t)`) and as
//! outgoing attribute transitions (`attr.label(cfg = t)`).
//!
//! Pinned source: `/tmp/opencode/bazel-9.3-reference` (e89755b, Release 9.3.0).
//! Paths below are relative to `src/main/java/com/google/devtools/build/lib/`.
//! One application follows TransitionApplier and StarlarkTransitionCache
//! (analysis/producers/TransitionApplier.java, analysis/config/StarlarkTransitionCache.java:100-185):
//! the build settings the transition reads or writes are loaded first
//! (crate::engine, StarlarkBuildSettingsDetailsFunction), then
//! FunctionTransitionUtil.applyAndValidate (analysis/starlark/FunctionTransitionUtil.java:100-162)
//! validates the native options, evaluates the implementation
//! (StarlarkDefinedConfigTransition.RegularTransition.evaluate,
//! analysis/config/StarlarkDefinedConfigTransition.java:520-640) and converts
//! the result (FunctionTransitionUtil.applyTransition, :601-790), and
//! StarlarkTransition.validate (analysis/starlark/StarlarkTransition.java:109-273)
//! type-checks Starlark settings and drops those set to their default.
//! Split results (several configurations) are N18d and fail explicitly.
use std::collections::BTreeMap;
use std::fmt;
use std::sync::Arc;

use allocative::Allocative;
use anyhow::anyhow;
use starlark::environment::Module;
use starlark::eval::Evaluator;
use starlark::values::dict::{AllocDict, DictRef};
use starlark::values::list::ListRef;
use starlark::values::tuple::TupleRef;
use starlark::values::{Heap, OwnedFrozenValue, Value, ValueLike};

use crate::configuration::{Configuration, NativeOptions, OptionValue, SettingValue};
use crate::custom_rule::AttrValue;
use crate::label::Label;
use crate::repository::RepositorySet;
use crate::starlark_options::SettingType;

/// LabelConstants.COMMAND_LINE_OPTION_PREFIX (cmdline/LabelConstants.java:65).
pub(crate) const COMMAND_LINE_OPTION_PREFIX: &str = "//command_line_option:";

/// The native options a transition may read and write natively: the
/// modelled options of crate::configuration (docs/CONFIGURATION.md),
/// including N28b's JavaOptions version strings, and `--cpu` and
/// `--collect_code_coverage` (N47, rules_go's go_stdlib_transition), which only a
/// transition sets natively.
const MODELLED_NATIVE_OPTIONS: &[&str] = &[
    "compilation_mode",
    "host_compilation_mode",
    "stamp",
    "strip",
    "force_pic",
    "platforms",
    "host_platform",
    "extra_toolchains",
    "cpu",
    "collect_code_coverage",
    "java_runtime_version",
    "java_language_version",
    "tool_java_runtime_version",
    "tool_java_language_version",
];

/// Native options a transition may only read: `define` (Bazel forbids
/// writing it, FunctionTransitionUtil.java:420-426) and `copt`, which is
/// always its default `[]` natively (`--copt` is not a native option).
const READ_ONLY_NATIVE_OPTIONS: &[&str] = &["define", "copt"];

/// A transition setting: a native option or a Starlark build setting.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum SettingKey {
    Native(String),
    Starlark(Label),
}

/// A declared input or output: the form given in `transition()` and its
/// canonical key.
#[derive(Clone, Debug)]
pub(crate) struct Setting {
    pub given: String,
    pub key: SettingKey,
}

/// The owned definition of a retained `transition()` object.
#[derive(Clone)]
pub(crate) struct TransitionDef {
    pub id: u64,
    pub value: OwnedFrozenValue,
    /// The `.bzl` file that called `transition()`: its package and
    /// repository mapping are the package context of label values
    /// (StarlarkDefinedConfigTransition.getPackageContext, :128-130).
    pub definition: Label,
    /// `FILE:LINE:COLUMN` of the `transition()` call (getLocation).
    pub location: String,
    /// Inputs and outputs in canonical order (getInputsCanonicalizedToGiven
    /// is an ImmutableSortedMap, :183).
    pub inputs: Vec<Setting>,
    pub outputs: Vec<Setting>,
}

impl fmt::Debug for TransitionDef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TransitionDef")
            .field("id", &self.id)
            .field("definition", &self.definition)
            .field("location", &self.location)
            .finish()
    }
}

/// Bazel's Label ordering of setting keys (`//command_line_option:X` is a
/// main-repository label of package `command_line_option`).
fn sort_key(key: &SettingKey) -> Label {
    match key {
        SettingKey::Native(name) => Label {
            repository: String::new(),
            package: "command_line_option".to_owned(),
            name: name.clone(),
        },
        SettingKey::Starlark(label) => label.clone(),
    }
}

impl TransitionDef {
    /// The definition of a retained `transition()` value.
    pub(crate) fn of(value: &OwnedFrozenValue) -> Result<Self, String> {
        let transition = crate::rule_declarations::Transition::from_value(value.value().to_value())
            .ok_or_else(|| "internal error: retained cfg is not a transition".to_owned())?;
        let settings = |given: &[String], canonical: &[String]| -> Result<Vec<Setting>, String> {
            let mut settings = given
                .iter()
                .zip(canonical)
                .map(|(given, canonical)| {
                    let key = match canonical.strip_prefix(COMMAND_LINE_OPTION_PREFIX) {
                        Some(name) => SettingKey::Native(name.to_owned()),
                        None => SettingKey::Starlark(Label::parse_canonical(canonical)?),
                    };
                    Ok(Setting {
                        given: given.clone(),
                        key,
                    })
                })
                .collect::<Result<Vec<_>, String>>()?;
            settings.sort_by_key(|setting| sort_key(&setting.key));
            Ok(settings)
        };
        Ok(Self {
            id: transition.id,
            value: value.clone(),
            definition: Label::parse_canonical(&transition.definition)?,
            location: transition.location.clone(),
            inputs: settings(&transition.inputs, &transition.canonical_inputs)?,
            outputs: settings(&transition.outputs, &transition.canonical_outputs)?,
        })
    }

    /// The Starlark build settings read or written, inputs then outputs
    /// (StarlarkTransitionCache.getAllStarlarkBuildSettings; an
    /// ImmutableSet keeps the first position).
    pub(crate) fn starlark_settings(&self) -> Vec<Label> {
        let mut labels: Vec<Label> = Vec::new();
        for setting in self.inputs.iter().chain(&self.outputs) {
            if let SettingKey::Starlark(label) = &setting.key
                && !labels.contains(label)
            {
                labels.push(label.clone());
            }
        }
        labels
    }

    /// The declared outputs in their given forms (getOutputs).
    pub(crate) fn given_outputs(&self) -> Vec<String> {
        self.outputs
            .iter()
            .map(|setting| setting.given.clone())
            .collect()
    }
}

/// StarlarkBuildSettingsDetailsValue for one referenced setting: the end of
/// its alias chain, type and default (StarlarkBuildSettingsDetailsFunction,
/// skyframe/StarlarkBuildSettingsDetailsFunction.java:60-110).
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct SettingDetail {
    pub actual: Label,
    pub ty: SettingType,
    pub default: SettingValue,
}

/// Whether a rule (incoming) or attribute (outgoing) transition is applied.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Kind {
    Rule,
    Attribute,
}

/// A failed application: the events Bazel reports first (already
/// `LOCATION: `-prefixed lines) and the TransitionException message the
/// consumer reports at the rule.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Failure {
    pub events: Vec<String>,
    pub message: String,
}

impl Failure {
    fn message(message: String) -> Self {
        Self {
            events: Vec::new(),
            message,
        }
    }

    /// An error event followed by StarlarkTransitionCache's
    /// `Errors encountered while applying Starlark transition`
    /// (analysis/config/StarlarkTransitionCache.java:170-175).
    fn event(location: &str, message: String) -> Self {
        Self {
            events: vec![if location.is_empty() {
                message
            } else {
                format!("{location}: {message}")
            }],
            message: ERRORS_ENCOUNTERED.to_owned(),
        }
    }
}

const ERRORS_ENCOUNTERED: &str = "Errors encountered while applying Starlark transition";

/// The result of a successful application.
#[derive(Debug)]
pub(crate) struct Applied {
    pub configuration: Configuration,
    /// print() output of the implementation, in order.
    pub printed: Vec<String>,
}

/// The values of the transition's `attr` struct, in rule attribute order;
/// `None` for an unset (null) label or output.
pub(crate) type AttributeValues = Vec<(String, Option<AttrValue>)>;

/// Everything one application reads besides the transition itself.
pub(crate) struct Application<'a> {
    pub kind: Kind,
    pub from: &'a Configuration,
    /// Keyed by the label the transition references (possibly an alias).
    pub details: &'a BTreeMap<Label, SettingDetail>,
    /// The target the transition is attached to (output attribute values
    /// are labels in its package).
    pub target: &'a Label,
    pub attributes: &'a AttributeValues,
    pub repositories: &'a Arc<RepositorySet>,
}

// ---------------------------------------------------------------------------
// Starlark values.

#[allow(unsafe_code)]
mod values {
    use super::*;
    use starlark::starlark_complex_value;
    use starlark::starlark_simple_value;
    use starlark::values::{
        Coerce, Freeze, NoSerialize, ProvidesStaticType, StarlarkPagablePanic, StarlarkValue,
        Trace, starlark_value,
    };

    /// The Java enum option values that implement StarlarkValue
    /// (CompilationMode, analysis/config/CompilationMode.java:24-58;
    /// CppConfiguration.StripMode, rules/cpp/CppConfiguration.java:119-134):
    /// `Starlark.fromJava` keeps the enum object, so a transition reads a
    /// value whose type is the class's simple name (Starlark.classType,
    /// net/starlark/java/eval/Starlark.java:337-418: no @StarlarkBuiltin) —
    /// `CompilationMode` with repr `opt` (CompilationMode.repr, :55-58), or
    /// `StripMode` with the default StarlarkValue repr — equal only to the
    /// same constant.
    macro_rules! option_enum {
        ($name:ident, $type_name:literal) => {
            #[derive(
                Clone,
                Debug,
                PartialEq,
                Eq,
                ProvidesStaticType,
                NoSerialize,
                Allocative,
                StarlarkPagablePanic,
            )]
            pub(crate) struct $name(#[allocative(skip)] pub &'static str);
            starlark_simple_value!($name);

            #[starlark_value(type = $type_name)]
            impl<'v> StarlarkValue<'v> for $name {
                fn equals(&self, other: Value<'v>) -> starlark::Result<bool> {
                    Ok(Self::from_value(other) == Some(self))
                }
            }
        };
    }
    option_enum!(CompilationModeValue, "CompilationMode");
    option_enum!(StripModeValue, "StripMode");

    impl fmt::Display for CompilationModeValue {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.write_str(self.0)
        }
    }

    impl fmt::Display for StripModeValue {
        // StarlarkValue.repr default (net/starlark/java/eval/StarlarkValue.java:50).
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.write_str(
                "<unknown object com.google.devtools.build.lib.rules.cpp.CppConfiguration$StripMode>",
            )
        }
    }

    /// The `attr` struct of a transition implementation
    /// (`StructProvider.STRUCT.create(attributes, errorMessageFormatForUnknownField)`,
    /// analysis/starlark/StarlarkRuleTransitionProvider.java:131-136 and
    /// StarlarkAttributeTransitionProvider.java:126-128).
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
    pub(crate) struct TransitionAttrsGen<V> {
        pub values: Vec<V>,
        #[trace(unsafe_ignore)]
        #[freeze(identity)]
        pub names: Vec<String>,
        /// `%s` is the attribute name.
        #[trace(unsafe_ignore)]
        #[freeze(identity)]
        pub unknown_field: String,
    }
    starlark_complex_value!(pub(crate) TransitionAttrs);

    impl<'v, V: ValueLike<'v>> fmt::Display for TransitionAttrsGen<V> {
        /// StructImpl.repr: `struct(a = 1, b = 2)` with sorted fields.
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            let mut fields = self.names.iter().zip(&self.values).collect::<Vec<_>>();
            fields.sort_by(|a, b| a.0.cmp(b.0));
            f.write_str("struct(")?;
            for (index, (name, value)) in fields.into_iter().enumerate() {
                if index > 0 {
                    f.write_str(", ")?;
                }
                write!(f, "{name} = {}", value.to_value().to_repr())?;
            }
            f.write_str(")")
        }
    }

    #[starlark_value(type = "struct")]
    impl<'v, V: ValueLike<'v>> StarlarkValue<'v> for TransitionAttrsGen<V>
    where
        Self: ProvidesStaticType<'v>,
    {
        fn get_attr(&self, name: &str, _heap: Heap<'v>) -> Option<Value<'v>> {
            self.names
                .iter()
                .position(|candidate| candidate == name)
                .map(|index| self.values[index].to_value())
        }

        fn dir_attr(&self) -> Vec<String> {
            let mut names = self.names.clone();
            names.sort();
            names
        }

        /// The provider's message plus StructImpl.allAttributesSuffix
        /// (packages/StructImpl.java:86-95): every field name, sorted.
        fn unknown_attribute_message(&self, attribute: &str) -> Option<String> {
            let mut names = self.names.clone();
            names.sort();
            Some(format!(
                "{}\nAvailable attributes: {}",
                self.unknown_field.replace("%s", attribute),
                names.join(", ")
            ))
        }
    }
}
pub(crate) use values::{CompilationModeValue, StripModeValue, TransitionAttrsGen};

/// A native enum option value: its Starlark type name and constant.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct OptionEnum {
    pub type_name: &'static str,
    pub value: &'static str,
}

impl OptionEnum {
    fn alloc<'v>(self, heap: Heap<'v>) -> Value<'v> {
        if self.type_name == "CompilationMode" {
            heap.alloc(CompilationModeValue(self.value))
        } else {
            heap.alloc(StripModeValue(self.value))
        }
    }

    fn unpack(value: Value<'_>) -> Option<Self> {
        CompilationModeValue::from_value(value)
            .map(|mode| Self {
                type_name: "CompilationMode",
                value: mode.0,
            })
            .or_else(|| {
                StripModeValue::from_value(value).map(|mode| Self {
                    type_name: "StripMode",
                    value: mode.0,
                })
            })
    }
}

/// The rule transition's unknown-attribute text
/// (StarlarkRuleTransitionProvider.java:131-136).
const RULE_UNKNOWN_ATTRIBUTE: &str = "No attribute '%s'. Either this attribute does not exist for this rule or the attribute was not resolved because it is set by a select that reads flags the transition may set.";

/// StarlarkAttributesCollection.ERROR_MESSAGE_FOR_NO_ATTR
/// (analysis/starlark/StarlarkAttributesCollection.java:67-68), used by
/// attribute transitions (StarlarkAttributeTransitionProvider.java:126-128).
const ATTRIBUTE_UNKNOWN_ATTRIBUTE: &str =
    "No attribute '%s' in attr. Make sure you declared a rule attribute with this name.";

// ---------------------------------------------------------------------------
// The evaluation context.

/// The Evaluator `extra` of a transition implementation: label resolution
/// (`Label(...)`) needs the repositories (crate::loading::repositories).
#[allow(unsafe_code)]
mod context {
    use super::*;
    use starlark::values::ProvidesStaticType;

    #[derive(ProvidesStaticType)]
    pub(crate) struct TransitionContext {
        pub repositories: Arc<RepositorySet>,
    }
}
use context::TransitionContext;

pub(crate) fn repository_context(eval: &Evaluator<'_, '_, '_>) -> Option<Arc<RepositorySet>> {
    eval.extra
        .and_then(|extra| extra.downcast_ref::<TransitionContext>())
        .map(|context| context.repositories.clone())
}

#[derive(Default)]
struct Prints(std::cell::RefCell<Vec<String>>);

impl starlark::PrintHandler for Prints {
    fn println(&self, text: &str) -> starlark::Result<()> {
        self.0.borrow_mut().push(text.to_owned());
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Owned views of returned Starlark values.

#[derive(Clone, Debug)]
enum RawKind {
    None,
    Bool(bool),
    /// `None` outside the signed 64-bit range.
    Int(Option<i64>),
    String(String),
    Label(Label),
    Enum(&'static str, &'static str),
    /// A list or tuple (java.util.List).
    List(Vec<Raw>),
    /// Another Starlark Iterable (dict keys, set, range).
    Iterable(Vec<Raw>),
    Other,
}

#[derive(Clone, Debug)]
struct Raw {
    kind: RawKind,
    repr: String,
    type_name: String,
}

impl Raw {
    fn of<'v>(value: Value<'v>, heap: Heap<'v>) -> Self {
        let kind = if value.is_none() {
            RawKind::None
        } else if let Some(flag) = value.unpack_bool() {
            RawKind::Bool(flag)
        } else if value.get_type() == "int" {
            RawKind::Int(value.to_repr().parse::<i64>().ok())
        } else if let Some(text) = value.unpack_str() {
            RawKind::String(text.to_owned())
        } else if let Some(text) = crate::starlark_types::unpack_label(value) {
            match Label::parse_canonical(&text) {
                Ok(label) => RawKind::Label(label),
                Err(_) => RawKind::Other,
            }
        } else if let Some(option) = OptionEnum::unpack(value) {
            RawKind::Enum(option.type_name, option.value)
        } else if let Some(list) = ListRef::from_value(value) {
            RawKind::List(list.iter().map(|item| Raw::of(item, heap)).collect())
        } else if let Some(tuple) = TupleRef::from_value(value) {
            RawKind::List(
                tuple
                    .content()
                    .iter()
                    .map(|item| Raw::of(*item, heap))
                    .collect(),
            )
        } else if matches!(value.get_type(), "dict" | "set" | "range") {
            // Java Iterables that are not Lists (Dict iterates its keys;
            // a Dict is also a Map).
            match value.iterate(heap) {
                Ok(items) => RawKind::Iterable(items.map(|item| Raw::of(item, heap)).collect()),
                Err(_) => RawKind::Other,
            }
        } else {
            RawKind::Other
        };
        Self {
            kind,
            repr: value.to_repr(),
            type_name: value.get_type().to_owned(),
        }
    }

    /// `toString()` of a list element (FunctionTransitionUtil.java:676-685):
    /// Labels in their unambiguous canonical form, strings raw, others their
    /// Starlark str.
    fn element_text(&self, unambiguous_labels: bool) -> String {
        match &self.kind {
            RawKind::String(text) => text.clone(),
            RawKind::Label(label) if unambiguous_labels => unambiguous(label),
            RawKind::Label(label) => crate::rule_declarations::canonical_form(label),
            RawKind::Bool(flag) => if *flag { "true" } else { "false" }.to_owned(),
            RawKind::Int(Some(number)) => number.to_string(),
            RawKind::Enum(_, value) => (*value).to_owned(),
            _ => self.repr.clone(),
        }
    }
}

/// Label.getUnambiguousCanonicalForm (`@@//pkg:x` in the main repository).
fn unambiguous(label: &Label) -> String {
    format!("@@{}//{}:{}", label.repository, label.package, label.name)
}

/// The transition implementation's result: a no-op, one configuration's
/// raw changes, or a split.
enum Outcome {
    NoOp,
    Patch(Vec<(String, Raw)>),
    Split(usize),
}

// ---------------------------------------------------------------------------
// Application.

/// FunctionTransitionUtil.applyAndValidate with StarlarkTransitionCache and
/// StarlarkTransition.validate for a 1:1 result.
pub(crate) fn apply(
    def: &TransitionDef,
    application: &Application<'_>,
) -> Result<Applied, Failure> {
    let location = location_path(&def.location, application.repositories);
    validate_native_options(def, &location)?;
    // Native options outside the modelled configuration fail explicitly
    // (after Bazel's own validation, before any evaluation).
    let unmodelled = |setting: &Setting, readable: bool| match &setting.key {
        SettingKey::Native(name) => {
            !(MODELLED_NATIVE_OPTIONS.contains(&name.as_str())
                || readable && READ_ONLY_NATIVE_OPTIONS.contains(&name.as_str()))
        }
        SettingKey::Starlark(_) => false,
    };
    for setting in def
        .inputs
        .iter()
        .map(|setting| (setting, true))
        .chain(def.outputs.iter().map(|setting| (setting, false)))
        .filter(|(setting, readable)| unmodelled(setting, *readable))
        .map(|(setting, _)| setting)
    {
        if let SettingKey::Native(name) = &setting.key {
            return Err(Failure::message(format!(
                "Starlark transition on '{COMMAND_LINE_OPTION_PREFIX}{name}' is not implemented in the native profile (modelled native options: {}; read only: {})",
                MODELLED_NATIVE_OPTIONS.join(", "),
                READ_ONLY_NATIVE_OPTIONS.join(", ")
            )));
        }
    }
    let prints = Prints::default();
    let outcome = evaluate(def, application, &prints)?;
    let printed = prints.0.into_inner();
    let changes = match outcome {
        Outcome::NoOp => Vec::new(),
        Outcome::Patch(changes) => changes,
        // StarlarkRuleTransitionProvider.FunctionPatchTransition.patch
        // (analysis/starlark/StarlarkRuleTransitionProvider.java:247-262).
        Outcome::Split(_) if application.kind == Kind::Rule => {
            return Err(Failure::event(
                &location,
                "Rule transition only allowed to return a single transitioned configuration."
                    .to_owned(),
            ));
        }
        Outcome::Split(count) => {
            return Err(Failure::message(format!(
                "a Starlark transition that returns {count} configurations (a split transition) is not implemented in the native profile (N18d)"
            )));
        }
    };
    let configuration = apply_changes(def, application, changes, &location)?;
    Ok(Applied {
        configuration,
        printed,
    })
}

/// The `transition()` location as Bazel prints it: the `.bzl` file's path.
fn location_path(location: &str, repositories: &RepositorySet) -> String {
    let Some((file, position)) = location.split_once(".bzl:") else {
        return location.to_owned();
    };
    let file = format!("{file}.bzl");
    match Label::parse_canonical(&file) {
        Ok(label) => match repositories.root(&label.repository) {
            Ok(root) => format!(
                "{}:{position}",
                root.join(&label.package).join(&label.name).display()
            ),
            Err(_) => location.to_owned(),
        },
        Err(_) => location.to_owned(),
    }
}

/// validateInputOptions / validateOutputOptions
/// (FunctionTransitionUtil.java:394-468): `--define` cannot be written;
/// native options must exist; non-configurable options cannot be changed.
fn validate_native_options(def: &TransitionDef, location: &str) -> Result<(), Failure> {
    let native = |settings: &[Setting]| -> Vec<(String, String)> {
        let mut options = settings
            .iter()
            .filter_map(|setting| match &setting.key {
                SettingKey::Native(name) => Some((setting.given.clone(), name.clone())),
                SettingKey::Starlark(_) => None,
            })
            .collect::<Vec<_>>();
        // getInputs/getOutputs are the given forms in canonical order.
        options.sort_by(|a, b| a.1.cmp(&b.1));
        options
    };
    let check = |kind: &str, options: &[(String, String)]| -> Result<(), Failure> {
        let invalid = options
            .iter()
            .filter(|(_, name)| crate::configuration::fragment_option(name).is_none())
            .map(|(given, _)| given.as_str())
            .collect::<Vec<_>>();
        if !invalid.is_empty() {
            return Err(Failure::event(
                location,
                format!(
                    "transition {kind} [{}] do not correspond to valid settings",
                    invalid.join(", ")
                ),
            ));
        }
        let non_configurable = options
            .iter()
            .filter(|(_, name)| crate::configuration::fragment_option(name) == Some(true))
            .map(|(given, _)| given.as_str())
            .collect::<Vec<_>>();
        if !non_configurable.is_empty() {
            return Err(Failure::event(
                location,
                format!(
                    "transition {kind} [{}] cannot be changed: they are non-configurable",
                    non_configurable.join(", ")
                ),
            ));
        }
        Ok(())
    };
    check("inputs", &native(&def.inputs))?;
    let outputs = native(&def.outputs);
    if outputs.iter().any(|(_, name)| name == "define") {
        return Err(Failure::event(
            location,
            "Starlark transition on --define not supported - try using build settings (https://bazel.build/rules/config#user-defined-build-settings).".to_owned(),
        ));
    }
    check("outputs", &outputs)
}

/// The value a transition reads for a native option
/// (FunctionTransitionUtil.findNativeOptionValue, :556-575, then
/// RegularTransition.createBuildSettingsDict's Starlark.fromJava, :428-450).
fn native_input<'v>(
    native: &NativeOptions,
    name: &str,
    heap: Heap<'v>,
) -> anyhow::Result<Value<'v>> {
    let label = |label: &Label| crate::starlark_types::alloc_label(heap, &label.to_string());
    Ok(match name {
        "compilation_mode" => OptionEnum {
            type_name: "CompilationMode",
            value: native.compilation_mode,
        }
        .alloc(heap),
        "host_compilation_mode" => OptionEnum {
            type_name: "CompilationMode",
            value: native.host_compilation_mode,
        }
        .alloc(heap),
        "strip" => OptionEnum {
            type_name: "StripMode",
            value: native.strip,
        }
        .alloc(heap),
        "stamp" => heap.alloc(native.stamp),
        "force_pic" => heap.alloc(native.force_pic),
        "collect_code_coverage" => heap.alloc(native.collect_code_coverage),
        "platforms" => heap.alloc(
            native
                .platforms
                .iter()
                .map(label)
                .collect::<anyhow::Result<Vec<_>>>()?,
        ),
        "host_platform" => label(&native.host_platform)?,
        "extra_toolchains" => heap.alloc(native.extra_toolchains.clone()),
        // CoreOptions.cpu after AutoCpuConverter (the host CPU by default).
        "cpu" => heap.alloc(
            native
                .cpu
                .as_deref()
                .unwrap_or(crate::configuration::host_cpu()),
        ),
        // JavaOptions String fields (JavaOptions.java:373-403), read as
        // Starlark strings.
        "java_runtime_version" => heap.alloc(native.java.java_runtime_version.as_str()),
        "java_language_version" => heap.alloc(native.java.java_language_version.as_str()),
        "tool_java_runtime_version" => heap.alloc(native.java.tool_java_runtime_version.as_str()),
        "tool_java_language_version" => heap.alloc(native.java.tool_java_language_version.as_str()),
        // An empty `--define` list is a readable empty list (a non-empty
        // one is unreadable, see unreadable_input); `--copt` is always its
        // default natively.
        "define" | "copt" => heap.alloc(Vec::<String>::new()),
        other => {
            return Err(anyhow!(
                "internal error: unmodelled native transition input {other}"
            ));
        }
    })
}

/// A Starlark build setting value as Starlark sees it.
fn setting_input<'v>(value: &SettingValue, heap: Heap<'v>) -> anyhow::Result<Value<'v>> {
    Ok(match value {
        SettingValue::Bool(flag) => heap.alloc(*flag),
        SettingValue::Int(number) => heap.alloc(*number),
        SettingValue::String(text) => heap.alloc(text.as_str()),
        SettingValue::StringList(items) => heap.alloc(items.clone()),
        // Starlark.fromJava of a StarlarkList or Tuple is the object itself.
        SettingValue::Sequence {
            tuple: false,
            items,
        } => heap.alloc(items.clone()),
        SettingValue::Sequence { tuple: true, items } => heap.alloc(
            starlark::values::tuple::AllocTuple(items.iter().map(|item| heap.alloc(item.as_str()))),
        ),
        SettingValue::Label(label) => crate::starlark_types::alloc_label(heap, &label.to_string())?,
    })
}

/// The previous value of a Starlark setting in the transition's source
/// options: StarlarkTransitionCache.addDefaultStarlarkOptions (:100-127)
/// adds the default of every referenced setting the options do not hold,
/// keyed by the label as referenced — so a setting read through an alias
/// sees its default (the options hold the actual's label only).
fn previous_setting(
    from: &Configuration,
    label: &Label,
    details: &BTreeMap<Label, SettingDetail>,
) -> Option<SettingValue> {
    from.starlark
        .get(label)
        .cloned()
        .or_else(|| details.get(label).map(|detail| detail.default.clone()))
}

/// Evaluate the implementation and classify its result
/// (RegularTransition.evaluate, StarlarkDefinedConfigTransition.java:520-640).
fn evaluate(
    def: &TransitionDef,
    application: &Application<'_>,
    prints: &Prints,
) -> Result<Outcome, Failure> {
    // RegularTransition.createBuildSettingsDict (:418-470): a value
    // Starlark.fromJava cannot convert and whose converter is not
    // Starlark-convertible is `before calling F: Input build setting S is of
    // type CLASS, which is unreadable in Starlark. Please submit a feature
    // request.` at the implementation (:542-552). Natively the only such
    // value is a non-empty `--define` list (List<Map.Entry>, the
    // AssignmentConverter is not Starlark-convertible), a Guava
    // ImmutableList: SingletonImmutableList for one entry,
    // RegularImmutableList otherwise (FragmentOptions.normalizeEntries,
    // CoreOptions.sortEntries, :1199-1208).
    for setting in &def.inputs {
        if matches!(&setting.key, SettingKey::Native(name) if name == "define")
            && !application.from.native.define.is_empty()
        {
            let class = if application.from.native.define.len() == 1 {
                "SingletonImmutableList"
            } else {
                "RegularImmutableList"
            };
            let (location, name) = implementation_site(def, application.repositories);
            return Err(Failure::event(
                &location,
                format!(
                    "before calling {name}: Input build setting {} is of type class com.google.common.collect.{class}, which is unreadable in Starlark. Please submit a feature request.",
                    setting.given
                ),
            ));
        }
    }
    // The `settings` dict and `attr` struct are immutable
    // (Dict.Builder.buildImmutable, StructImpl): built on a private heap and
    // frozen, as rule context attributes are (crate::analysis).
    let frozen = Module::with_temp_heap(|module| -> anyhow::Result<_> {
        let heap = module.heap();
        let mut entries = Vec::new();
        for setting in &def.inputs {
            let value = match &setting.key {
                SettingKey::Native(name) => native_input(&application.from.native, name, heap)?,
                SettingKey::Starlark(label) => {
                    match previous_setting(application.from, label, application.details) {
                        Some(value) => setting_input(&value, heap)?,
                        None => Value::new_none(),
                    }
                }
            };
            entries.push((heap.alloc(setting.given.as_str()), value));
        }
        let settings = heap.alloc(AllocDict(entries));
        let mut names = Vec::new();
        let mut values = Vec::new();
        for (name, value) in application.attributes {
            names.push(name.clone());
            values.push(match value {
                Some(value) => {
                    crate::custom_rule::attr_value_to_starlark(heap, value, application.target)?
                        .unwrap_or_else(Value::new_none)
                }
                None => Value::new_none(),
            });
        }
        let attrs = heap.alloc(TransitionAttrsGen {
            values,
            names,
            unknown_field: match application.kind {
                Kind::Rule => RULE_UNKNOWN_ATTRIBUTE,
                Kind::Attribute => ATTRIBUTE_UNKNOWN_ATTRIBUTE,
            }
            .to_owned(),
        });
        module.set("settings", settings);
        module.set("attr", attrs);
        let frozen = module
            .freeze()
            .map_err(|error| anyhow!("cannot freeze transition inputs: {error:?}"))?;
        Ok((frozen.get("settings")?, frozen.get("attr")?))
    })
    .map_err(|error| Failure::message(error.to_string()))?;
    let impl_location = implementation_location(def, application.repositories);
    let repositories = application.repositories.clone();
    Module::with_temp_heap(|module| -> Result<Outcome, Failure> {
        let heap = module.heap();
        let settings = heap.access_owned_frozen_value(&frozen.0);
        let attrs = heap.access_owned_frozen_value(&frozen.1);
        let transition = heap.access_owned_frozen_value(&def.value);
        let implementation = crate::rule_declarations::Transition::from_value(transition)
            .map(|transition| transition.implementation.to_value())
            .ok_or_else(|| Failure::message("internal error: not a transition".to_owned()))?;
        let context = TransitionContext { repositories };
        let mut eval = Evaluator::new(&module);
        eval.set_max_callstack_size(128)
            .map_err(|error| Failure::message(error.to_string()))?;
        eval.extra = Some(&context);
        eval.set_print_handler(prints);
        // Starlark.positionalOnlyCall(thread, impl, settings, attr); an
        // EvalException is an error event without a location, with its
        // stack (:556-560).
        let result = eval
            .eval_function(implementation, &[settings, attrs], &[])
            .map_err(|error| Failure {
                events: vec![error.to_string()],
                message: ERRORS_ENCOUNTERED.to_owned(),
            })?;
        let invalid = |message: String| {
            Failure::event(
                &impl_location,
                format!("invalid result from transition function: {message}"),
            )
        };
        if result.is_none() {
            return Ok(Outcome::NoOp);
        }
        if let Some(dict) = DictRef::from_value(result) {
            if dict.is_empty() {
                return Ok(Outcome::NoOp);
            }
            // Dict.cast(result, String.class, Dict.class): a dict of option
            // dicts is a split (:579-597).
            let split = dict.iter().all(|(key, value)| {
                key.unpack_str().is_some() && DictRef::from_value(value).is_some()
            });
            if split {
                let mut count = 0;
                for (_, value) in dict.iter() {
                    canonical_dict(def, value, application.repositories, heap).map_err(invalid)?;
                    count += 1;
                }
                return single_of_split(
                    def,
                    application,
                    dict.iter().map(|(_, v)| v).collect(),
                    count,
                    heap,
                )
                .map_err(invalid);
            }
            return canonical_dict(def, result, application.repositories, heap)
                .map(Outcome::Patch)
                .map_err(invalid);
        }
        let sequence = ListRef::from_value(result)
            .map(|list| list.iter().collect::<Vec<_>>())
            .or_else(|| TupleRef::from_value(result).map(|tuple| tuple.content().to_vec()));
        if let Some(items) = sequence {
            if items.is_empty() {
                return Ok(Outcome::NoOp);
            }
            // Sequence.cast(result, Dict.class, "dictionary of options dictionaries").
            for (index, item) in items.iter().enumerate() {
                if DictRef::from_value(*item).is_none() {
                    return Err(invalid(format!(
                        "at index {index} of dictionary of options dictionaries, got element of type {}, want dict",
                        item.get_type()
                    )));
                }
            }
            for item in &items {
                canonical_dict(def, *item, application.repositories, heap).map_err(invalid)?;
            }
            let count = items.len();
            return single_of_split(def, application, items, count, heap).map_err(invalid);
        }
        Err(Failure::event(
            &impl_location,
            format!(
                "transition function returned {}, want dict or list of dicts",
                result.get_type()
            ),
        ))
    })
}

/// A rule transition may return one configuration in the split forms
/// (StarlarkRuleTransitionProvider.FunctionPatchTransition.patch takes the
/// only element, :247-262); an attribute transition's split result is a
/// split (N18d), even with one element (`ctx.split_attr` keys).
fn single_of_split<'v>(
    def: &TransitionDef,
    application: &Application<'_>,
    items: Vec<Value<'v>>,
    count: usize,
    heap: Heap<'v>,
) -> Result<Outcome, String> {
    if application.kind == Kind::Rule && count == 1 {
        return canonical_dict(def, items[0], application.repositories, heap).map(Outcome::Patch);
    }
    Ok(Outcome::Split(count))
}

/// `Dict.cast(entry, String.class, Object.class, "dictionary of options")`
/// and canonicalizeTransitionOutputDict (:680-725): exactly the declared
/// outputs, as declared; keys canonicalized relative to the `.bzl` file.
fn canonical_dict<'v>(
    def: &TransitionDef,
    value: Value<'v>,
    repositories: &RepositorySet,
    heap: Heap<'v>,
) -> Result<Vec<(String, Raw)>, String> {
    let dict = DictRef::from_value(value).ok_or_else(|| {
        format!(
            "got {} for 'dictionary of options', want dict",
            value.get_type()
        )
    })?;
    let mut entries = Vec::new();
    for (key, value) in dict.iter() {
        let Some(key) = key.unpack_str() else {
            return Err(format!(
                "got dict<{}, {}> for 'dictionary of options', want dict<string, unknown>",
                key.get_type(),
                value.get_type()
            ));
        };
        entries.push((key.to_owned(), Raw::of(value, heap)));
    }
    // validateFunctionOutputsMatchesDeclaredOutputs (:665-690).
    let declared = def.given_outputs();
    if !(entries.len() == declared.len()
        && declared
            .iter()
            .all(|output| entries.iter().any(|(key, _)| key == output)))
    {
        let mut remaining = declared.clone();
        for (key, _) in &entries {
            match remaining.iter().position(|output| output == key) {
                Some(index) => {
                    remaining.remove(index);
                }
                None => {
                    return Err(format!(
                        "transition function returned undeclared output '{key}'"
                    ));
                }
            }
        }
        if !remaining.is_empty() {
            return Err(format!(
                "transition outputs [{}] were not defined by transition function",
                remaining.join(",")
            ));
        }
    }
    let mut seen: Vec<(String, String)> = Vec::new();
    for (key, _) in &entries {
        let canonical = if key.starts_with(COMMAND_LINE_OPTION_PREFIX) {
            key.clone()
        } else {
            repositories
                .parse(key, &def.definition.repository, &def.definition.package)
                .map(|label| crate::rule_declarations::canonical_form(&label))
                .map_err(|_| format!("Malformed label in transition return dictionary: '{key}'"))?
        };
        if let Some((_, previous)) = seen.iter().find(|(seen, _)| *seen == canonical) {
            return Err(format!(
                "Transition implementation function returns the same option '{canonical}' in two different keys: '{key}' and '{previous}'"
            ));
        }
        seen.push((canonical, key.clone()));
    }
    Ok(entries)
}

/// StarlarkFunction.getLocation of the implementation: the position of the
/// `def` name (net/starlark/java/syntax/Resolver.java:828-835), found in its
/// file's source; the `transition()` call otherwise.
fn implementation_location(def: &TransitionDef, repositories: &RepositorySet) -> String {
    implementation_site(def, repositories).0
}

/// The implementation's location and StarlarkFunction.getName.
fn implementation_site(def: &TransitionDef, repositories: &RepositorySet) -> (String, String) {
    let repr = crate::rule_declarations::Transition::from_value(def.value.value().to_value())
        .map(|transition| transition.implementation.to_value().to_repr())
        .unwrap_or_default();
    // `<function NAME from FILE>` (vendored DefGen repr).
    let parsed = repr
        .strip_prefix("<function ")
        .and_then(|rest| rest.strip_suffix('>'))
        .and_then(|rest| rest.split_once(" from "));
    let name = parsed
        .map_or("transition implementation", |(name, _)| name)
        .to_owned();
    let fallback = || (location_path(&def.location, repositories), name.clone());
    let Some((name, file)) = parsed else {
        return fallback();
    };
    let Ok(label) = Label::parse_canonical(file) else {
        return fallback();
    };
    let Ok(root) = repositories.root(&label.repository) else {
        return fallback();
    };
    let path = root.join(&label.package).join(&label.name);
    let Ok(source) = crate::observed_fs::read_to_string(&path) else {
        return fallback();
    };
    for (index, line) in source.lines().enumerate() {
        let trimmed = line.trim_start();
        if let Some(rest) = trimmed.strip_prefix("def ")
            && rest.trim_start().starts_with(name)
            && rest.trim_start()[name.len()..]
                .trim_start()
                .starts_with('(')
        {
            let column = line.len() - trimmed.len() + 4 + (rest.len() - rest.trim_start().len());
            return (
                format!("{}:{}:{}", path.display(), index + 1, column + 1),
                name.to_owned(),
            );
        }
    }
    fallback()
}

/// A Starlark option value while a transition is applied: a typed
/// configuration value, or the raw value the implementation returned
/// (BuildOptions keeps the raw object; StarlarkTransition.validate only
/// checks it).
#[derive(Clone, Debug)]
enum OptionSlot {
    Typed(SettingValue),
    Raw(Raw),
}

/// `Label.parseWithPackageContext` in the transition's package context
/// (StarlarkDefinedConfigTransition.getPackageContext): Bazel's label syntax
/// errors (cmdline/LabelParser.java, crate::bazel_label) first, then the
/// repository mapping of the defining `.bzl` file.
fn package_label(
    def: &TransitionDef,
    repositories: &RepositorySet,
    text: &str,
) -> Result<Label, String> {
    crate::bazel_label::Parts::parse(text)?;
    repositories.parse(text, &def.definition.repository, &def.definition.package)
}

/// FunctionTransitionUtil.applyTransition (:601-790) for the native options
/// and Starlark settings over the source options with the referenced
/// settings' defaults added (StarlarkTransitionCache.addDefaultStarlarkOptions,
/// :100-127), then StarlarkTransition.validate (:109-273): unalias, then
/// every referenced setting converted with its type and removed when it
/// is at its default.
fn apply_changes(
    def: &TransitionDef,
    application: &Application<'_>,
    changes: Vec<(String, Raw)>,
    location: &str,
) -> Result<Configuration, Failure> {
    let from = application.from;
    let details = application.details;
    let mut native = from.native.clone();
    let mut platforms_changed = false;
    // The canonicalized dict is an ImmutableSortedMap (:700).
    let mut keyed = changes
        .into_iter()
        .map(|(given, raw)| {
            let key = def
                .outputs
                .iter()
                .find(|setting| setting.given == given)
                .map(|setting| setting.key.clone())
                .expect("validated output");
            (key, raw)
        })
        .collect::<Vec<_>>();
    // handleImplicitPlatformChange (:254-277): a changed `--cpu` without
    // `--platforms` clears `--platforms` (the raw value compared with the
    // current one).
    let current_cpu = native
        .cpu
        .clone()
        .unwrap_or_else(|| crate::configuration::host_cpu().to_owned());
    let cpu_changed = keyed.iter().any(|(key, raw)| {
        *key == SettingKey::Native("cpu".to_owned())
            && !matches!(&raw.kind, RawKind::String(text) if *text == current_cpu)
    });
    if cpu_changed
        && !keyed
            .iter()
            .any(|(key, _)| *key == SettingKey::Native("platforms".to_owned()))
    {
        keyed.push((
            SettingKey::Native("platforms".to_owned()),
            Raw {
                kind: RawKind::List(Vec::new()),
                repr: "[]".to_owned(),
                type_name: "list".to_owned(),
            },
        ));
    }
    keyed.sort_by_key(|(key, _)| sort_key(key));
    let parse = |text: &str| package_label(def, application.repositories, text);
    let mut options: BTreeMap<Label, OptionSlot> = from
        .starlark
        .iter()
        .map(|(label, value)| (label.clone(), OptionSlot::Typed(value.clone())))
        .collect();
    for setting in def.starlark_settings() {
        if let Some(detail) = details.get(&setting) {
            options
                .entry(setting)
                .or_insert_with(|| OptionSlot::Typed(detail.default.clone()));
        }
    }
    for (key, raw) in keyed {
        match key {
            SettingKey::Starlark(label) => {
                // Label-typed settings convert strings in the package context
                // (:617-635).
                let raw = if matches!(
                    options.get(&label),
                    Some(OptionSlot::Typed(SettingValue::Label(_)))
                ) {
                    match &raw.kind {
                        RawKind::String(text) => match parse(text) {
                            Ok(parsed) => Raw {
                                kind: RawKind::Label(parsed),
                                ..raw
                            },
                            Err(error) => {
                                return Err(Failure::event(
                                    location,
                                    format!(
                                        "Error parsing value for option '{}': {error}",
                                        crate::rule_declarations::canonical_form(&label)
                                    ),
                                ));
                            }
                        },
                        RawKind::Label(_) => raw,
                        _ => {
                            return Err(Failure::event(
                                location,
                                format!(
                                    "Invalid value type for option '{}': want label, got {}",
                                    crate::rule_declarations::canonical_form(&label),
                                    raw.type_name
                                ),
                            ));
                        }
                    }
                } else {
                    raw
                };
                // `if (!Objects.equals(oldValue, optionValue))` (:656-659):
                // an equal value keeps the previous object.
                let unchanged = matches!(
                    options.get(&label),
                    Some(OptionSlot::Typed(old)) if java_equals_raw(old, &raw)
                );
                if !unchanged {
                    options.insert(label, OptionSlot::Raw(raw));
                }
            }
            SettingKey::Native(name) => {
                if name == "platforms" {
                    platforms_changed = true;
                }
                convert_native(&mut native, &name, &raw, &parse)
                    .map_err(|message| Failure::event(location, message))?;
            }
        }
    }
    // PlatformOptions.getNormalized keeps the first platform; an empty list
    // becomes the host platform (crate::configuration).
    if platforms_changed {
        native.platforms.truncate(1);
        if native.platforms.is_empty() {
            native.platforms = vec![native.host_platform.clone()];
        }
    }
    // StarlarkTransition.unalias (:285-318): an entry keyed by an alias
    // overwrites its actual setting (whose own entry is stale) and is
    // removed. A setting read through an alias therefore takes the
    // alias's added default.
    let aliases = details
        .iter()
        .filter(|(label, detail)| **label != detail.actual)
        .map(|(label, detail)| (label.clone(), detail.actual.clone()))
        .collect::<BTreeMap<_, _>>();
    if !aliases.is_empty() {
        for (alias, actual) in &aliases {
            if let Some(value) = options.remove(alias) {
                options.insert(actual.clone(), value);
            }
        }
    }
    // validate (:150-208): input-only settings at their default are
    // removed; every other referenced setting is converted with its type
    // (`what` is the label as referenced) and removed at its default.
    let is_input = |label: &Label| {
        def.inputs
            .iter()
            .any(|setting| setting.key == SettingKey::Starlark(label.clone()))
    };
    let is_output = |label: &Label| {
        def.outputs
            .iter()
            .any(|setting| setting.key == SettingKey::Starlark(label.clone()))
    };
    for setting in def.starlark_settings() {
        let detail = details
            .get(&setting)
            .ok_or_else(|| Failure::message(format!("internal error: no details for {setting}")))?;
        let actual = &detail.actual;
        let Some(slot) = options.get(actual).cloned() else {
            continue;
        };
        if is_input(&setting)
            && !is_output(&setting)
            && matches!(&slot, OptionSlot::Typed(value) if detail.default.java_equals(value))
        {
            options.remove(actual);
            continue;
        }
        // The converted value is compared with the default
        // (validateAndCheckIfAtDefault, StarlarkTransition.java:225-274);
        // the raw object is what stays.
        let what = crate::rule_declarations::canonical_form(&setting);
        let (value, raw) = match slot {
            OptionSlot::Typed(value) => (value, None),
            OptionSlot::Raw(raw) => (
                convert_setting(detail.ty, &raw, &what).map_err(Failure::message)?,
                Some(raw),
            ),
        };
        if converted_equals(&value, &detail.default) {
            options.remove(actual);
            continue;
        }
        let stored = match raw {
            Some(raw) => stored_raw(detail.ty, &raw, &value, &what).map_err(Failure::message)?,
            None => value,
        };
        options.insert(actual.clone(), OptionSlot::Typed(stored));
    }
    let starlark = options
        .into_iter()
        .map(|(label, slot)| match slot {
            OptionSlot::Typed(value) => Ok((label, value)),
            OptionSlot::Raw(_) => Err(Failure::message(format!(
                "internal error: unvalidated transition output {label}"
            ))),
        })
        .collect::<Result<BTreeMap<_, _>, _>>()?;
    Ok(Configuration::new(native, starlark))
}

/// `Objects.equals(oldValue, optionValue)` of FunctionTransitionUtil.applyTransition
/// (:656-659) for a stored value and a returned one: Boolean, StarlarkInt,
/// String and Label by value; a converted list (ImmutableList) equals any
/// list or tuple with equal elements, a raw StarlarkList only a list and a
/// raw Tuple only a tuple (StarlarkList.java:244-251, Tuple.java:129-135).
fn java_equals_raw(old: &SettingValue, raw: &Raw) -> bool {
    let strings = |items: &[Raw]| -> Option<Vec<String>> {
        items
            .iter()
            .map(|item| match &item.kind {
                RawKind::String(text) => Some(text.clone()),
                _ => None,
            })
            .collect()
    };
    match (old, &raw.kind) {
        (SettingValue::Bool(old), RawKind::Bool(new)) => old == new,
        (SettingValue::Int(old), RawKind::Int(Some(new))) => old == new,
        (SettingValue::String(old), RawKind::String(new)) => old == new,
        (SettingValue::Label(old), RawKind::Label(new)) => old == new,
        (SettingValue::StringList(old), RawKind::List(items)) => {
            strings(items).as_ref() == Some(old)
        }
        (SettingValue::Sequence { tuple, items: old }, RawKind::List(items)) => {
            *tuple == (raw.type_name == "tuple") && strings(items).as_ref() == Some(old)
        }
        _ => false,
    }
}

/// `Type.convert(value).equals(default)`: a raw sequence converts to the
/// list of its elements.
fn converted_equals(value: &SettingValue, default: &SettingValue) -> bool {
    match value {
        SettingValue::Sequence { items, .. } => default.string_items() == Some(items.as_slice()),
        _ => value == default,
    }
}

/// The object BuildOptions keeps for a returned value that is not at its
/// default (FunctionTransitionUtil.applyTransition keeps the raw value,
/// StarlarkTransition.validate only converts it for the default check): a
/// list or tuple for a string-list setting stays a raw sequence
/// ([`SettingValue::Sequence`]); bool, int, string and label values are the
/// converted ones, whose Java objects and `toString` are the raw ones'. A
/// raw object of another class (an int for a bool setting, a dict/set/range
/// for a string list) keeps a `toString` the native configuration does not
/// model: explicit.
fn stored_raw(
    ty: SettingType,
    raw: &Raw,
    value: &SettingValue,
    what: &str,
) -> Result<SettingValue, String> {
    let unmodelled = || {
        Err(format!(
            "a Starlark transition setting {what} to a value of type '{}' (kept unconverted in the configuration) is not implemented in the native profile",
            raw.type_name
        ))
    };
    match (ty, &raw.kind, value) {
        (SettingType::StringList, RawKind::List(_), SettingValue::StringList(items)) => {
            Ok(SettingValue::Sequence {
                tuple: raw.type_name == "tuple",
                items: items.clone(),
            })
        }
        (SettingType::StringList, _, _) => unmodelled(),
        (SettingType::Bool, RawKind::Bool(_), _)
        | (SettingType::Int, RawKind::Int(_), _)
        | (SettingType::String, RawKind::String(_), _)
        | (SettingType::Label, RawKind::Label(_), _) => Ok(value.clone()),
        _ => unmodelled(),
    }
}

/// `Type.convert` of a build setting's type (packages/Type.java:296-321,
/// 357-375, 411-429, 469-474, 730-760; BuildType.LABEL/NODEP_LABEL).
fn convert_setting(ty: SettingType, raw: &Raw, what: &str) -> Result<SettingValue, String> {
    let mismatch = |expected: &str, raw: &Raw, what: &str| {
        format!(
            "expected {expected} for {what}, but got {} ({})",
            raw.repr, raw.type_name
        )
    };
    let int32 = |raw: &Raw, what: &str| -> Result<i64, String> {
        match raw.kind {
            RawKind::Int(Some(number)) if i32::try_from(number).is_ok() => Ok(number),
            RawKind::Int(_) => Err(format!(
                "for {what}, got {}, want value in signed 32-bit range",
                raw.repr
            )),
            _ => Err(mismatch("value of type 'int'", raw, what)),
        }
    };
    Ok(match ty {
        SettingType::Bool => match raw.kind {
            RawKind::Bool(flag) => SettingValue::Bool(flag),
            _ => match int32(raw, what) {
                Ok(0) => SettingValue::Bool(false),
                Ok(1) => SettingValue::Bool(true),
                _ => return Err(mismatch("one of [False, True, 0, 1]", raw, what)),
            },
        },
        SettingType::Int => SettingValue::Int(int32(raw, what)?),
        SettingType::String => match &raw.kind {
            RawKind::String(text) => SettingValue::String(text.clone()),
            _ => return Err(mismatch("value of type 'string'", raw, what)),
        },
        SettingType::StringList => match &raw.kind {
            RawKind::List(items) | RawKind::Iterable(items) => {
                let mut values = Vec::new();
                for (index, item) in items.iter().enumerate() {
                    match &item.kind {
                        RawKind::String(text) => values.push(text.clone()),
                        _ => {
                            return Err(mismatch(
                                "value of type 'string'",
                                item,
                                &format!("element {index} of {what}"),
                            ));
                        }
                    }
                }
                SettingValue::StringList(values)
            }
            _ => return Err(mismatch("value of type 'list(string)'", raw, what)),
        },
        SettingType::Label => match &raw.kind {
            RawKind::Label(label) => SettingValue::Label(label.clone()),
            _ => return Err(mismatch("value of type 'label'", raw, what)),
        },
    })
}

/// The native option conversion of FunctionTransitionUtil.applyTransition
/// (:640-760) for the modelled options; errors are its ValidationException
/// texts (OptionsParsingException messages wrapped as `OptionsParsingError
/// for option 'NAME': MESSAGE`).
fn convert_native(
    native: &mut NativeOptions,
    name: &str,
    raw: &Raw,
    package_label: &dyn Fn(&str) -> Result<Label, String>,
) -> Result<(), String> {
    let option = crate::configuration::option(name);
    let list_typed = matches!(name, "platforms" | "extra_toolchains");
    let invalid = || format!("Invalid value type for option '{name}'");
    let parsing = |message: String| format!("OptionsParsingError for option '{name}': {message}");
    if matches!(raw.kind, RawKind::None) {
        if list_typed {
            return Err(format!(
                "'None' value not allowed for List-type option '{name}'. Please use '[]' instead if trying to set option to empty value."
            ));
        }
        // `None` becomes a null field value (:648-651, 726-728,
        // FieldOptionDefinition.setValue). A primitive boolean field rejects
        // it (Field.set's IllegalArgumentException, reported as
        // `IllegalArgumentError for option 'NAME': MESSAGE`, :762-764); a
        // null enum option reads as its default (OptionDefinition.getValue,
        // common/options/FieldOptionDefinition.java:131-139; observed: the
        // default top-level configuration's root).
        let defaults = NativeOptions::default();
        match name {
            "stamp" | "force_pic" | "collect_code_coverage" => {
                let field = match name {
                    "stamp" => {
                        "com.google.devtools.build.lib.analysis.config.CoreOptions.stampBinaries"
                    }
                    "force_pic" => "com.google.devtools.build.lib.rules.cpp.CppOptions.forcePic",
                    _ => {
                        "com.google.devtools.build.lib.analysis.config.CoreOptions.collectCodeCoverage"
                    }
                };
                return Err(format!(
                    "IllegalArgumentError for option '{name}': Can not set boolean field {field} to null value"
                ));
            }
            "compilation_mode" => native.compilation_mode = defaults.compilation_mode,
            "host_compilation_mode" => {
                native.host_compilation_mode = defaults.host_compilation_mode
            }
            "strip" => native.strip = defaults.strip,
            _ => {
                return Err(format!(
                    "setting //command_line_option:{name} to None (a null option value) is not implemented in the native profile"
                ));
            }
        }
        return Ok(());
    }
    // CoreOptionConverters.convertOptionsLabel with a PackageContext (:331-338).
    let labels = |text: &str| -> Result<Vec<Label>, String> {
        text.split(',')
            .filter(|part| !part.is_empty())
            .map(|part| package_label(part).map_err(parsing))
            .collect()
    };
    match name {
        "compilation_mode" | "host_compilation_mode" | "strip" => {
            let type_name = if name == "strip" {
                "StripMode"
            } else {
                "CompilationMode"
            };
            let value = match &raw.kind {
                RawKind::Enum(kind, value) if *kind == type_name => *value,
                RawKind::String(text) => {
                    match crate::configuration::convert(
                        option.expect("modelled option"),
                        text,
                        &|_| Err("no labels".to_owned()),
                    )
                    .map_err(parsing)?
                    {
                        OptionValue::Enum(value) => value,
                        _ => unreachable!("enum converter"),
                    }
                }
                _ => return Err(invalid()),
            };
            match name {
                "compilation_mode" => native.compilation_mode = value,
                "host_compilation_mode" => native.host_compilation_mode = value,
                _ => native.strip = value,
            }
        }
        "stamp" | "force_pic" | "collect_code_coverage" => {
            let value = match &raw.kind {
                RawKind::Bool(flag) => *flag,
                RawKind::String(text) => {
                    // `--collect_code_coverage` has no modelled command-line
                    // form; it converts with BooleanConverter like `--stamp`.
                    match crate::configuration::convert(
                        option.unwrap_or(&crate::configuration::COLLECT_CODE_COVERAGE),
                        text,
                        &|_| Err("no labels".to_owned()),
                    )
                    .map_err(parsing)?
                    {
                        OptionValue::Bool(flag) => flag,
                        _ => unreachable!("boolean converter"),
                    }
                }
                _ => return Err(invalid()),
            };
            match name {
                "stamp" => native.stamp = value,
                "force_pic" => native.force_pic = value,
                _ => native.collect_code_coverage = value,
            }
        }
        "platforms" => {
            native.platforms = match &raw.kind {
                RawKind::List(items) if items.is_empty() => Vec::new(),
                // Not allowMultiple: the elements joined with "," (Labels in
                // their unambiguous canonical form), then the converter.
                RawKind::List(items) => labels(
                    &items
                        .iter()
                        .map(|item| item.element_text(true))
                        .collect::<Vec<_>>()
                        .join(","),
                )?,
                RawKind::String(text) => labels(text)?,
                _ => return Err(invalid()),
            };
        }
        "host_platform" => {
            native.host_platform = match &raw.kind {
                RawKind::Label(label) => label.clone(),
                // PlatformOptions.java:52-64: empty means the default.
                RawKind::String(text) if text.is_empty() => {
                    crate::configuration::default_host_platform()
                }
                RawKind::String(text) => package_label(text).map_err(parsing)?,
                _ => return Err(invalid()),
            };
        }
        // A String option: a string value is stored as is (`def.getType()
        // .isInstance`, :726-728, before any converter); the host CPU is
        // the unset (default) value.
        "cpu" => match &raw.kind {
            RawKind::String(text) => {
                native.cpu =
                    (text.as_str() != crate::configuration::host_cpu()).then(|| text.clone());
            }
            _ => return Err(invalid()),
        },
        // JavaOptions String options: a string value is stored as is
        // (`def.getType().isInstance`, :726-728, before any converter).
        "java_runtime_version"
        | "java_language_version"
        | "tool_java_runtime_version"
        | "tool_java_language_version" => {
            let RawKind::String(text) = &raw.kind else {
                return Err(invalid());
            };
            let field = match name {
                "java_runtime_version" => &mut native.java.java_runtime_version,
                "java_language_version" => &mut native.java.java_language_version,
                "tool_java_runtime_version" => &mut native.java.tool_java_runtime_version,
                _ => &mut native.java.tool_java_language_version,
            };
            *field = text.clone();
        }
        "extra_toolchains" => {
            // allowMultiple: each element's toString converted with
            // CommaSeparatedOptionListConverter, concatenated; then
            // PlatformOptions.getNormalized dedupes keeping the last copy.
            let mut values: Vec<String> = Vec::new();
            match &raw.kind {
                RawKind::List(items) => {
                    for item in items {
                        let text = item.element_text(false);
                        if !text.is_empty() {
                            values.extend(text.split(',').map(str::to_owned));
                        }
                    }
                }
                RawKind::String(text) => {
                    if !text.is_empty() {
                        values.extend(text.split(',').map(str::to_owned));
                    }
                }
                _ => return Err(invalid()),
            }
            let mut deduped: Vec<String> = Vec::new();
            for value in values.into_iter().rev() {
                if !deduped.contains(&value) {
                    deduped.push(value);
                }
            }
            deduped.reverse();
            native.extra_toolchains = deduped;
        }
        other => {
            return Err(format!(
                "internal error: unmodelled native transition output {other}"
            ));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn raw(kind: RawKind, repr: &str, type_name: &str) -> Raw {
        Raw {
            kind,
            repr: repr.to_owned(),
            type_name: type_name.to_owned(),
        }
    }

    #[test]
    fn setting_conversion_follows_bazel_types() {
        let what = "//:s";
        assert_eq!(
            convert_setting(
                SettingType::Bool,
                &raw(RawKind::Int(Some(1)), "1", "int"),
                what
            ),
            Ok(SettingValue::Bool(true))
        );
        assert_eq!(
            convert_setting(
                SettingType::Bool,
                &raw(RawKind::String("x".into()), "\"x\"", "string"),
                what
            ),
            Err("expected one of [False, True, 0, 1] for //:s, but got \"x\" (string)".into())
        );
        assert_eq!(
            convert_setting(
                SettingType::String,
                &raw(RawKind::Int(Some(1)), "1", "int"),
                what
            ),
            Err("expected value of type 'string' for //:s, but got 1 (int)".into())
        );
        assert_eq!(
            convert_setting(
                SettingType::Int,
                &raw(RawKind::Int(Some(1 << 40)), "1099511627776", "int"),
                what
            ),
            Err("for //:s, got 1099511627776, want value in signed 32-bit range".into())
        );
        assert_eq!(
            convert_setting(
                SettingType::StringList,
                &raw(
                    RawKind::List(vec![raw(RawKind::Int(Some(2)), "2", "int")]),
                    "[2]",
                    "list"
                ),
                what
            ),
            Err("expected value of type 'string' for element 0 of //:s, but got 2 (int)".into())
        );
    }

    #[test]
    fn native_conversion_follows_function_transition_util() {
        let parse = |text: &str| -> Result<Label, String> {
            Label::parse_canonical(&text.replace("@@//", "//"))
        };
        let mut native = NativeOptions::default();
        convert_native(
            &mut native,
            "compilation_mode",
            &raw(RawKind::String("OPT".into()), "\"OPT\"", "string"),
            &parse,
        )
        .unwrap();
        assert_eq!(native.compilation_mode, "opt");
        assert_eq!(
            convert_native(
                &mut native,
                "compilation_mode",
                &raw(RawKind::Bool(true), "True", "bool"),
                &parse
            ),
            Err("Invalid value type for option 'compilation_mode'".into())
        );
        assert_eq!(
            convert_native(
                &mut native,
                "compilation_mode",
                &raw(RawKind::String("fast".into()), "\"fast\"", "string"),
                &parse
            ),
            Err("OptionsParsingError for option 'compilation_mode': Not a valid compilation mode: 'fast' (should be fastbuild, dbg or opt)".into())
        );
        assert_eq!(
            convert_native(&mut native, "platforms", &raw(RawKind::None, "None", "NoneType"), &parse),
            Err("'None' value not allowed for List-type option 'platforms'. Please use '[]' instead if trying to set option to empty value.".into())
        );
        convert_native(
            &mut native,
            "platforms",
            &raw(
                RawKind::List(vec![raw(
                    RawKind::Label(Label::parse_canonical("//p:x").unwrap()),
                    "Label(\"//p:x\")",
                    "Label",
                )]),
                "[Label(\"//p:x\")]",
                "list",
            ),
            &parse,
        )
        .unwrap();
        assert_eq!(native.platforms, [Label::parse_canonical("//p:x").unwrap()]);
        convert_native(
            &mut native,
            "extra_toolchains",
            &raw(
                RawKind::List(vec![
                    raw(RawKind::String("//a,//b".into()), "", "string"),
                    raw(RawKind::String("//a".into()), "", "string"),
                ]),
                "",
                "list",
            ),
            &parse,
        )
        .unwrap();
        assert_eq!(native.extra_toolchains, ["//b", "//a"]);
        // None: enum options read as their default, booleans reject it.
        native.compilation_mode = "dbg";
        convert_native(
            &mut native,
            "compilation_mode",
            &raw(RawKind::None, "None", "NoneType"),
            &parse,
        )
        .unwrap();
        assert_eq!(native.compilation_mode, "fastbuild");
        assert_eq!(
            convert_native(&mut native, "force_pic", &raw(RawKind::None, "None", "NoneType"), &parse),
            Err("IllegalArgumentError for option 'force_pic': Can not set boolean field com.google.devtools.build.lib.rules.cpp.CppOptions.forcePic to null value".into())
        );
        // --cpu: stored as given; the host CPU is the unset value.
        convert_native(
            &mut native,
            "cpu",
            &raw(RawKind::String("arm".into()), "\"arm\"", "string"),
            &parse,
        )
        .unwrap();
        assert_eq!(native.cpu.as_deref(), Some("arm"));
        convert_native(
            &mut native,
            "cpu",
            &raw(
                RawKind::String(crate::configuration::host_cpu().into()),
                "",
                "string",
            ),
            &parse,
        )
        .unwrap();
        assert_eq!(native.cpu, None);
    }

    /// N47: `--collect_code_coverage` (CoreOptions.java:566-578) converts
    /// like the other boolean options (BooleanConverter for strings, the
    /// value of a bool, `Invalid value type` otherwise, a null-field error
    /// for None) and reads as a Starlark bool.
    #[test]
    fn collect_code_coverage_is_a_boolean_transition_option() {
        let parse = |text: &str| Label::parse_canonical(text);
        let mut native = NativeOptions::default();
        for (value, expected) in [
            (raw(RawKind::Bool(true), "True", "bool"), true),
            (raw(RawKind::String("0".into()), "\"0\"", "string"), false),
            (
                raw(RawKind::String("yes".into()), "\"yes\"", "string"),
                true,
            ),
            (
                raw(RawKind::String("FALSE".into()), "\"FALSE\"", "string"),
                false,
            ),
        ] {
            convert_native(&mut native, "collect_code_coverage", &value, &parse).unwrap();
            assert_eq!(native.collect_code_coverage, expected, "{}", value.repr);
        }
        for (value, error) in [
            (
                raw(RawKind::String("maybe".into()), "\"maybe\"", "string"),
                "OptionsParsingError for option 'collect_code_coverage': 'maybe' is not a boolean",
            ),
            (
                raw(RawKind::Int(Some(1)), "1", "int"),
                "Invalid value type for option 'collect_code_coverage'",
            ),
            (
                raw(RawKind::None, "None", "NoneType"),
                "IllegalArgumentError for option 'collect_code_coverage': Can not set boolean field com.google.devtools.build.lib.analysis.config.CoreOptions.collectCodeCoverage to null value",
            ),
        ] {
            assert_eq!(
                convert_native(&mut native, "collect_code_coverage", &value, &parse),
                Err(error.to_owned())
            );
        }
        assert!(MODELLED_NATIVE_OPTIONS.contains(&"collect_code_coverage"));
        Module::with_temp_heap(|module| {
            native.collect_code_coverage = true;
            let value = native_input(&native, "collect_code_coverage", module.heap()).unwrap();
            assert_eq!(
                (value.get_type(), value.to_repr()),
                ("bool", "True".to_owned())
            );
        });
        // The default keeps every existing identity.
        assert_eq!(
            Configuration::new(NativeOptions::default(), BTreeMap::new()).id,
            Configuration::default().id
        );
        assert_ne!(
            Configuration::new(native, BTreeMap::new()).id,
            Configuration::default().id
        );
    }
}
