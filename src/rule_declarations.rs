//! Bazel 9.3 load-time rule-declaration values: `aspect()`, `transition()`,
//! `select()`, `OutputGroupInfo` and `config.exec/target/none`.
//!
//! These values are accepted, validated and retained exactly where pinned
//! Bazel validates them while a `.bzl` file or BUILD file is evaluated. Their
//! analysis-time semantics (configuration transitions, aspect propagation,
//! configured `select()` resolution, output groups) are NOT implemented: a
//! target whose analysis would exercise one fails explicitly with
//! [`NOT_ANALYZED`]. See `docs/LOAD_DECLARATIONS.md`.
//!
//! Bazel sources cited below are relative to
//! `src/main/java/com/google/devtools/build/lib/` at the pinned 9.3.0 commit
//! e89755b.

use allocative::Allocative;
use anyhow::{anyhow, bail};
use starlark::environment::GlobalsBuilder;
use starlark::eval::{Arguments, Evaluator};
use starlark::starlark_module;
use starlark::values::dict::{AllocDict, DictRef};
use starlark::values::list::AllocList;
use starlark::values::none::NoneType;
use starlark::values::{Heap, Value, ValueLike};
use std::collections::BTreeSet;
use std::fmt;
use std::sync::OnceLock;

use crate::custom_rule::AttrSpec;
use crate::loading::ExtensionContext;
use crate::providers::ProviderRequirement;
use crate::repository_budget::HostBudget;

/// Shared suffix of every explicit "declared but not analyzed" failure.
pub(crate) const NOT_ANALYZED: &str =
    "accepted at load time but not analyzed by the native profile";

/// Requirement identity of the native `OutputGroupInfo` provider key.
pub(crate) const OUTPUT_GROUP_INFO_ID: &str = "builtin:OutputGroupInfo";

// ---------------------------------------------------------------------------
// Owned configuration-transition metadata.

/// A retained `cfg` value: attribute (`attr.label(cfg=...)`) or rule (`rule(cfg=...)`).
#[derive(Clone, Debug, Default, PartialEq, Eq, Allocative, serde::Serialize)]
pub enum Cfg {
    /// `"target"`, `None` or `config.target()` (NoTransition): no transition.
    #[default]
    Target,
    /// `"exec"`, `"host"` or `config.exec(exec_group)` (ExecutionTransitionFactory).
    Exec(Option<String>),
    /// `config.none()` (NoConfigTransition).
    NoConfig,
    /// A Starlark `transition()` object, identified by its definition and settings.
    Starlark {
        definition: String,
        inputs: Vec<String>,
        outputs: Vec<String>,
        /// `analysis_test_transition(settings=...)` (isForAnalysisTesting,
        /// StarlarkDefinedConfigTransition.java:300-345).
        #[serde(skip_serializing_if = "std::ops::Not::not")]
        analysis_test: bool,
        /// The identity of the `transition()` object (Bazel compares
        /// RegularTransition by inputs, outputs and implementation identity,
        /// analysis/config/StarlarkDefinedConfigTransition.java:707-720; one
        /// object per `transition()` call is the same up to equal copies).
        #[serde(skip)]
        id: u64,
    },
}

impl Cfg {
    pub(crate) fn is_target(&self) -> bool {
        matches!(self, Self::Target)
    }

    /// Attribute.hasAnalysisTestTransition (StarlarkAttrModule.java:486-495).
    pub(crate) fn is_analysis_test(&self) -> bool {
        matches!(
            self,
            Self::Starlark {
                analysis_test: true,
                ..
            }
        )
    }

    pub(crate) fn describe(&self) -> String {
        match self {
            Self::Target => "target".to_owned(),
            Self::Exec(None) => "exec".to_owned(),
            Self::Exec(Some(group)) => format!("config.exec({group:?})"),
            Self::NoConfig => "config.none()".to_owned(),
            Self::Starlark {
                definition,
                analysis_test: true,
                ..
            } => format!("analysis_test_transition from {definition}"),
            Self::Starlark { definition, .. } => format!("transition from {definition}"),
        }
    }
}

// ---------------------------------------------------------------------------
// Starlark values.

// Only derive-generated marker/trace/freeze/coerce implementations need the
// lint exception; callbacks are traced, everything else is owned Rust data.
#[allow(unsafe_code)]
mod values {
    use super::*;
    use starlark::values::{
        Coerce, Freeze, NoSerialize, ProvidesStaticType, StarlarkPagablePanic, StarlarkValue,
        Trace, starlark_value,
    };
    use starlark::{starlark_complex_value, starlark_simple_value};

    /// `config.exec(exec_group=None)` (analysis/config/ExecutionTransitionFactory.java).
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
    pub(crate) struct ExecTransition {
        pub exec_group: Option<String>,
    }
    starlark_simple_value!(ExecTransition);

    impl fmt::Display for ExecTransition {
        // StarlarkValue.repr default (net/starlark/java/eval/StarlarkValue.java:50).
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.write_str(
                "<unknown object com.google.devtools.build.lib.analysis.config.ExecutionTransitionFactory>",
            )
        }
    }

    // StarlarkConfigApi.ExecTransitionFactoryApi: @StarlarkBuiltin(name = "ExecTransitionFactory").
    // ExecutionTransitionFactory does not override equals: identity equality.
    #[starlark_value(type = "ExecTransitionFactory")]
    impl<'v> StarlarkValue<'v> for ExecTransition {
        fn get_attr(&self, name: &str, heap: Heap<'v>) -> Option<Value<'v>> {
            and_then(name, heap, "ExecTransitionFactory")
        }
        fn dir_attr(&self) -> Vec<String> {
            vec!["and_then".to_owned()]
        }
    }

    /// `transition.and_then` (ConfigurationTransitionApi.java:37-56): the method
    /// shape exists; composing transitions is not implemented natively.
    #[derive(Clone, Debug, ProvidesStaticType, NoSerialize, Allocative, StarlarkPagablePanic)]
    pub(crate) struct AndThen {
        #[allocative(skip)]
        pub receiver: &'static str,
    }
    starlark_simple_value!(AndThen);

    impl fmt::Display for AndThen {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            write!(f, "<built-in method and_then of {} value>", self.receiver)
        }
    }

    #[starlark_value(type = "builtin_function_or_method")]
    impl<'v> StarlarkValue<'v> for AndThen {
        fn invoke(
            &self,
            _me: Value<'v>,
            _args: &Arguments<'v, '_>,
            _eval: &mut Evaluator<'v, '_, '_>,
        ) -> starlark::Result<Value<'v>> {
            Err(starlark::Error::new_other(anyhow!(
                "transition.and_then() composition is not implemented in the native profile"
            )))
        }
    }

    pub(super) fn and_then<'v>(
        name: &str,
        heap: Heap<'v>,
        receiver: &'static str,
    ) -> Option<Value<'v>> {
        (name == "and_then").then(|| heap.alloc(AndThen { receiver }))
    }

    /// `config.target()` / `config.none()` (NoTransition.Factory / NoConfigTransition.Factory).
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
    pub(crate) struct NoOpTransition {
        pub no_config: bool,
    }
    starlark_simple_value!(NoOpTransition);

    impl fmt::Display for NoOpTransition {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            let class = if self.no_config {
                "NoConfigTransition"
            } else {
                "NoTransition"
            };
            write!(
                f,
                "<unknown object com.google.devtools.build.lib.analysis.config.transitions.{class}$Factory>"
            )
        }
    }

    // starlarkbuildapi/config/ConfigurationTransitionApi.java: name = "transition".
    // NoTransition.Factory / NoConfigTransition.Factory are records: value equality.
    #[starlark_value(type = "transition")]
    impl<'v> StarlarkValue<'v> for NoOpTransition {
        fn equals(&self, other: Value<'v>) -> starlark::Result<bool> {
            Ok(Self::from_value(other) == Some(self))
        }
        fn get_attr(&self, name: &str, heap: Heap<'v>) -> Option<Value<'v>> {
            and_then(name, heap, "transition")
        }
        fn dir_attr(&self) -> Vec<String> {
            vec!["and_then".to_owned()]
        }
    }

    /// A Starlark-defined `transition()` (analysis/config/StarlarkDefinedConfigTransition.java).
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
    pub(crate) struct TransitionGen<V> {
        pub implementation: V,
        /// `analysis_test_transition(settings=...)`: the retained frozen-safe
        /// settings dictionary; `None` for `transition()`.
        pub settings: V,
        #[trace(unsafe_ignore)]
        #[freeze(identity)]
        pub analysis_test: bool,
        #[trace(unsafe_ignore)]
        #[freeze(identity)]
        pub inputs: Vec<String>,
        #[trace(unsafe_ignore)]
        #[freeze(identity)]
        pub outputs: Vec<String>,
        #[trace(unsafe_ignore)]
        #[freeze(identity)]
        pub definition: String,
        /// The canonical forms of `inputs`/`outputs`
        /// (getCanonicalizedSettings, StarlarkDefinedConfigTransition.java:153-186):
        /// labels relative to the defining `.bzl` file's package and
        /// repository mapping, native options unchanged.
        #[trace(unsafe_ignore)]
        #[freeze(identity)]
        pub canonical_inputs: Vec<String>,
        #[trace(unsafe_ignore)]
        #[freeze(identity)]
        pub canonical_outputs: Vec<String>,
        /// The location of the `transition()` call (its left parenthesis,
        /// StarlarkThread.getCallerLocation): `LABEL:LINE:COLUMN`.
        #[trace(unsafe_ignore)]
        #[freeze(identity)]
        pub location: String,
        /// A per-process identity (crate::rule_declarations::Cfg::Starlark).
        #[trace(unsafe_ignore)]
        #[freeze(identity)]
        pub id: u64,
    }
    starlark_complex_value!(pub(crate) Transition);

    impl<V> fmt::Display for TransitionGen<V> {
        // StarlarkDefinedConfigTransition.java:733 RegularTransition.repr;
        // AnalysisTestTransition.repr (:354-357).
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            if self.analysis_test {
                f.write_str("<analysis_test_transition object>")
            } else {
                f.write_str("<transition object>")
            }
        }
    }

    #[starlark_value(type = "transition")]
    impl<'v, V: ValueLike<'v>> StarlarkValue<'v> for TransitionGen<V>
    where
        Self: ProvidesStaticType<'v>,
    {
        fn get_attr(&self, name: &str, heap: Heap<'v>) -> Option<Value<'v>> {
            and_then(name, heap, "transition")
        }
        fn dir_attr(&self) -> Vec<String> {
            vec!["and_then".to_owned()]
        }
    }

    /// `select()` result (packages/SelectorList.java). `selects[i]` is the
    /// no-match error of a select element; `None` marks a plain element.
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
    pub(crate) struct SelectorListGen<V> {
        pub values: Vec<V>,
        #[trace(unsafe_ignore)]
        #[freeze(identity)]
        pub selects: Vec<Option<String>>,
        #[trace(unsafe_ignore)]
        #[freeze(identity)]
        pub native_type: String,
    }
    starlark_complex_value!(pub(crate) SelectorList);

    impl<'v, V: ValueLike<'v>> fmt::Display for SelectorListGen<V> {
        // SelectorList.java:223 repr: elements joined by " + " (" | " for dicts);
        // SelectorValue.java:90 repr: select(<dict repr>).
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            let separator = if self.native_type == "dict" {
                " | "
            } else {
                " + "
            };
            for (index, (value, select)) in self.values.iter().zip(&self.selects).enumerate() {
                if index > 0 {
                    f.write_str(separator)?;
                }
                if select.is_some() {
                    write!(f, "select({})", value.to_value())?;
                } else {
                    write!(f, "{}", value.to_value())?;
                }
            }
            Ok(())
        }
    }

    // SelectorList.java:60 @StarlarkBuiltin(name = "select"); HasBinary at :163.
    #[starlark_value(type = "select")]
    impl<'v, V: ValueLike<'v>> StarlarkValue<'v> for SelectorListGen<V>
    where
        Self: ProvidesStaticType<'v>,
    {
        fn add(&self, rhs: Value<'v>, heap: Heap<'v>) -> Option<starlark::Result<Value<'v>>> {
            if self.native_type == "dict" {
                return None;
            }
            Some(concat(self.pieces(), pieces_of(rhs), heap).map_err(starlark::Error::new_other))
        }

        fn radd(&self, lhs: Value<'v>, heap: Heap<'v>) -> Option<starlark::Result<Value<'v>>> {
            if self.native_type == "dict" {
                return None;
            }
            Some(concat(pieces_of(lhs), self.pieces(), heap).map_err(starlark::Error::new_other))
        }

        /// `{...} | select(...)`: SelectorList.binaryOp with thisLeft = false
        /// concatenates when the other operand is a dict (PIPE,
        /// packages/SelectorList.java:171-182), through the vendored reverse
        /// `|` hook (docs/STARLARK_VENDOR.md).
        fn rbit_or(&self, lhs: Value<'v>, heap: Heap<'v>) -> Option<starlark::Result<Value<'v>>> {
            DictRef::from_value(lhs)?;
            Some(concat(pieces_of(lhs), self.pieces(), heap).map_err(starlark::Error::new_other))
        }

        fn bit_or(&self, rhs: Value<'v>, heap: Heap<'v>) -> starlark::Result<Value<'v>> {
            if self.native_type != "dict" {
                return Err(starlark::Error::new_other(anyhow!(
                    "unsupported binary operation: select | {}",
                    rhs.get_type()
                )));
            }
            concat(self.pieces(), pieces_of(rhs), heap).map_err(starlark::Error::new_other)
        }
    }

    impl<'v, V: ValueLike<'v>> SelectorListGen<V> {
        pub(super) fn pieces(&self) -> Pieces<'v> {
            Pieces {
                values: self.values.iter().map(|value| value.to_value()).collect(),
                selects: self.selects.clone(),
                native_type: self.native_type.clone(),
                type_name: format!("select of {}", self.native_type),
            }
        }
    }

    /// `OutputGroupInfo` (analysis/OutputGroupInfo.java; BuiltinProvider repr
    /// `<function NAME>` at packages/BuiltinProvider.java:94).
    #[derive(Clone, Debug, ProvidesStaticType, NoSerialize, Allocative, StarlarkPagablePanic)]
    pub(crate) struct OutputGroupInfoConstructor;
    starlark_simple_value!(OutputGroupInfoConstructor);

    impl fmt::Display for OutputGroupInfoConstructor {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.write_str("<function OutputGroupInfo>")
        }
    }

    #[starlark_value(type = "Provider")]
    impl<'v> StarlarkValue<'v> for OutputGroupInfoConstructor {
        fn equals(&self, other: Value<'v>) -> starlark::Result<bool> {
            Ok(Self::from_value(other).is_some())
        }

        fn invoke(
            &self,
            _me: Value<'v>,
            _args: &Arguments<'v, '_>,
            _eval: &mut Evaluator<'v, '_, '_>,
        ) -> starlark::Result<Value<'v>> {
            Err(starlark::Error::new_other(anyhow!(
                "OutputGroupInfo construction is not implemented in the native profile (analysis-time output groups)"
            )))
        }
    }

    /// The analysis-test providers of TestingBootstrap
    /// (starlarkbuildapi/test/TestingBootstrap.java:46-52): `AnalysisTestResultInfo`
    /// (analysis/test/AnalysisTestResultInfo.java:56-67, constructor
    /// AnalysisTestResultInfoApi.java:55-79) and the constructor-less
    /// `AnalysisFailureInfo` (analysis/test/AnalysisFailureInfo.java:78-86).
    /// BuiltinProvider repr is `<function NAME>` (packages/BuiltinProvider.java:93-96).
    /// Only analysis tests (an explicit analysis gap) produce or inspect
    /// them, so construction fails explicitly like `OutputGroupInfo`.
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
    pub(crate) struct TestingProvider {
        #[allocative(skip)]
        pub name: &'static str,
    }
    starlark_simple_value!(TestingProvider);

    impl fmt::Display for TestingProvider {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            write!(f, "<function {}>", self.name)
        }
    }

    #[starlark_value(type = "Provider")]
    impl<'v> StarlarkValue<'v> for TestingProvider {
        fn equals(&self, other: Value<'v>) -> starlark::Result<bool> {
            Ok(Self::from_value(other) == Some(self))
        }

        fn invoke(
            &self,
            _me: Value<'v>,
            _args: &Arguments<'v, '_>,
            _eval: &mut Evaluator<'v, '_, '_>,
        ) -> starlark::Result<Value<'v>> {
            Err(starlark::Error::new_other(
                if self.name == "AnalysisTestResultInfo" {
                    anyhow!(
                        "AnalysisTestResultInfo construction is not implemented in the native profile (analysis-test results)"
                    )
                } else {
                    // A BuiltinProvider without @StarlarkConstructor is not callable.
                    anyhow!("'Provider' object is not callable")
                },
            ))
        }
    }

    /// `aspect(...)` (packages/StarlarkDefinedAspect.java). Callbacks are
    /// retained but never invoked natively.
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
    pub(crate) struct AspectGen<V> {
        pub implementation: V,
        /// attr_aspects/toolchains_aspects functions and propagation_predicate.
        pub callbacks: Vec<V>,
        pub requires: Vec<V>,
        pub subrules: Vec<V>,
        #[trace(unsafe_ignore)]
        #[freeze(identity)]
        pub declaration: AspectDeclaration,
        #[trace(unsafe_ignore)]
        #[freeze(identity)]
        pub definition_label: String,
        #[trace(unsafe_ignore)]
        #[freeze(identity)]
        pub exported: OnceLock<String>,
    }
    starlark_complex_value!(pub(crate) Aspect);

    impl<V> AspectGen<V> {
        pub(crate) fn identity(&self) -> Option<String> {
            self.exported
                .get()
                .map(|name| format!("{}%{name}", self.definition_label))
        }
    }

    impl<V> fmt::Display for AspectGen<V> {
        // StarlarkDefinedAspect.java:162.
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.write_str("<aspect>")
        }
    }

    // StarlarkAspectApi.java:23 @StarlarkBuiltin(name = "Aspect").
    #[starlark_value(type = "Aspect")]
    impl<'v, V: ValueLike<'v>> StarlarkValue<'v> for AspectGen<V>
    where
        Self: ProvidesStaticType<'v>,
    {
        fn export_as(&self, name: &str, eval: &mut Evaluator<'v, '_, '_>) -> starlark::Result<()> {
            if self.exported.get().is_none() {
                let defining = eval
                    .extra
                    .and_then(|extra| extra.downcast_ref::<ExtensionContext>())
                    .is_some_and(|context| context.label.to_string() == self.definition_label);
                if defining {
                    let mut budget = HostBudget::default();
                    budget
                        .string(&self.definition_label)
                        .and_then(|()| budget.string(name))
                        .map_err(|e| starlark::Error::new_other(anyhow!(e)))?;
                    let _ = self.exported.set(name.to_owned());
                }
            }
            Ok(())
        }

        fn equals(&self, other: Value<'v>) -> starlark::Result<bool> {
            Ok(self.identity().is_some_and(|id| {
                Aspect::from_value(other).is_some_and(|other| other.identity() == Some(id.clone()))
            }))
        }
    }
}
pub(crate) use values::{
    Aspect, AspectGen, ExecTransition, NoOpTransition, OutputGroupInfoConstructor, SelectorList,
    SelectorListGen, TestingProvider, Transition, TransitionGen,
};

/// Owned `aspect()` metadata (StarlarkRuleClassFunctions.java:1323-1520).
#[derive(Clone, Debug, Default, PartialEq, Eq, Allocative)]
pub(crate) struct AspectDeclaration {
    /// `None` when attr_aspects is a function.
    pub attr_aspects: Option<Vec<String>>,
    /// `None` when toolchains_aspects is a function.
    pub toolchains_aspects: Option<Vec<String>>,
    pub attrs: Vec<(String, AttrSpec)>,
    pub required_parameters: Vec<String>,
    pub required_providers: Vec<Vec<ProviderRequirement>>,
    pub required_aspect_providers: Vec<Vec<ProviderRequirement>>,
    pub provides: Vec<ProviderRequirement>,
    pub requires: Vec<String>,
    pub fragments: Vec<String>,
    pub host_fragments: Vec<String>,
    pub toolchains: Vec<(String, bool)>,
    pub apply_to_generating_rules: bool,
    pub exec_compatible_with: Vec<String>,
    pub exec_groups: Vec<String>,
}

// ---------------------------------------------------------------------------
// Shared Bazel-shaped validation helpers.

/// Bazel `Label.getCanonicalForm()` (cmdline/Label.java:490-492): `//pkg:name`
/// or `@@repo//pkg:name` (`RepositoryName.getNameWithAt`, RepositoryName.java:270-286).
pub(crate) fn canonical_form(label: &crate::label::Label) -> String {
    if label.repository.is_empty() {
        format!("//{}:{}", label.package, label.name)
    } else {
        format!("@@{}//{}:{}", label.repository, label.package, label.name)
    }
}

/// `Sequence.cast(x, String.class, what)` (net/starlark/java/eval/Sequence.java:132).
pub(crate) fn cast_strings(value: Value<'_>, what: &str) -> anyhow::Result<Vec<String>> {
    let items = crate::custom_rule::sequence_ref(value)
        .ok_or_else(|| anyhow!("for {what}, got {}, want sequence", value.get_type()))?;
    let mut budget = HostBudget::default();
    items
        .iter()
        .enumerate()
        .map(|(index, item)| {
            let text = item.unpack_str().ok_or_else(|| {
                anyhow!(
                    "at index {index} of {what}, got element of type {}, want string",
                    item.get_type()
                )
            })?;
            budget.string(text).map_err(anyhow::Error::msg)?;
            Ok(text.to_owned())
        })
        .collect()
}

/// `Sequence.noneableCast`: None is an empty sequence.
pub(crate) fn noneable_strings(value: Value<'_>, what: &str) -> anyhow::Result<Vec<String>> {
    if value.is_none() {
        Ok(Vec::new())
    } else {
        cast_strings(value, what)
    }
}

fn is_function(value: Value<'_>) -> bool {
    value.parameters_spec().is_some()
}

/// Resolve a string relative to the innermost `.bzl` (LabelConverter.forBzlEvaluatingThread).
fn bzl_label(eval: &Evaluator<'_, '_, '_>, text: &str) -> anyhow::Result<crate::label::Label> {
    crate::loading::resolve_label_bounded(eval, text, &mut HostBudget::default())
}

/// Java `getClass().getSimpleName()` used by parseToolchainType's error
/// (StarlarkRuleClassFunctions.java:2256); shared with the language modules.
fn java_simple_name(value: Value<'_>) -> String {
    crate::language_modules::java_class_simple_name(value)
}

/// BuiltinFunction parameter type error for @Param allowedTypes
/// (net/starlark/java/eval/BuiltinFunction.java).
pub(crate) fn param_type_error(
    function: &str,
    param: &str,
    value: Value<'_>,
    want: &str,
) -> anyhow::Error {
    anyhow!(
        "in call to {function}(), parameter '{param}' got value of type '{}', want '{want}'",
        value.get_type()
    )
}

/// `Sequence`-typed @Param check (list or tuple).
pub(crate) fn require_sequence(
    function: &str,
    param: &str,
    value: Value<'_>,
) -> anyhow::Result<()> {
    if crate::custom_rule::sequence_ref(value).is_none() {
        return Err(param_type_error(function, param, value, "sequence"));
    }
    Ok(())
}

/// Bazel label syntax (LabelParser.Parts) with its exact LabelSyntaxException
/// texts, then native resolution relative to the innermost `.bzl`.
fn bzl_label_checked(
    eval: &Evaluator<'_, '_, '_>,
    text: &str,
) -> Result<crate::label::Label, String> {
    crate::bazel_label::Parts::parse(text)?;
    bzl_label(eval, text).map_err(|e| e.to_string())
}

/// parseToolchainTypes (StarlarkRuleClassFunctions.java:2213-2258): toolchain_type
/// values, Labels or label strings; duplicates keep the strictest requirement.
/// `config_common.toolchain_type` values are recognized by their Starlark
/// type and `toolchain_type`/`mandatory` fields, without depending on the
/// defining module.
pub(crate) fn toolchains<'v>(
    eval: &Evaluator<'v, '_, '_>,
    value: Value<'v>,
    heap: Heap<'v>,
) -> anyhow::Result<Vec<(String, bool)>> {
    let items = crate::custom_rule::sequence_ref(value)
        .ok_or_else(|| anyhow!("for toolchains, got {}, want sequence", value.get_type()))?;
    let mut parsed: Vec<(String, bool)> = Vec::new();
    for &item in items {
        let (label, mandatory) = if item.get_type() == "toolchain_type" {
            let label = item
                .get_attr("toolchain_type", heap)
                .map_err(|e| anyhow!(e.to_string()))?
                .and_then(crate::starlark_types::unpack_label)
                .ok_or_else(|| anyhow!("toolchain_type value has no toolchain_type label"))?;
            let mandatory = item
                .get_attr("mandatory", heap)
                .map_err(|e| anyhow!(e.to_string()))?
                .and_then(|value| value.unpack_bool())
                .unwrap_or(true);
            (label, mandatory)
        } else if let Some(label) = crate::starlark_types::unpack_label(item) {
            (label, true)
        } else if let Some(text) = item.unpack_str() {
            let label = match bzl_label_checked(eval, text) {
                Ok(label) => label.to_string(),
                // Label.parseWithRepoContext keeps an apparent repository
                // the .bzl's repository does not see; resolution fails later
                // (crate::toolchains::deferred_label_text).
                Err(error) if error.starts_with("unknown apparent repository") => {
                    let requester = crate::loading::definition_label(eval)
                        .map(|label| label.repository)
                        .unwrap_or_default();
                    crate::toolchains::deferred_label_text(text, &requester).ok_or_else(|| {
                        anyhow!("Unable to parse toolchain_type label '{text}': {error}")
                    })?
                }
                Err(error) => bail!("Unable to parse toolchain_type label '{text}': {error}"),
            };
            (label, true)
        } else {
            bail!(
                "'toolchains' takes a toolchain_type, Label, or String, but instead got a {}",
                java_simple_name(item)
            );
        };
        if let Some(previous) = parsed.iter_mut().find(|(seen, _)| *seen == label) {
            // ToolchainTypeRequirement.strictest: mandatory wins.
            previous.1 |= mandatory;
        } else {
            parsed.push((label, mandatory));
        }
    }
    Ok(parsed)
}

/// `Dict.cast(execGroups, String, DeclaredExecGroup, "exec_group")` plus
/// StarlarkExecGroupCollection.isValidGroupName (StarlarkRuleClassFunctions.java:1066-1076).
pub(crate) fn exec_groups(value: Value<'_>) -> anyhow::Result<Vec<String>> {
    if value.is_none() {
        return Ok(Vec::new());
    }
    let dict = DictRef::from_value(value)
        .ok_or_else(|| anyhow!("got {} for 'exec_group', want dict", value.get_type()))?;
    for (key, group) in dict.iter() {
        if key.unpack_str().is_none() || group.get_type() != "exec_group" {
            bail!(
                // Starlark.classType(DeclaredExecGroup.class) = "exec_group".
                "got dict<{}, {}> for 'exec_group', want dict<string, exec_group>",
                key.get_type(),
                group.get_type()
            );
        }
    }
    let mut names = Vec::new();
    for (key, _) in dict.iter() {
        let name = key.unpack_str().expect("checked exec group key");
        // DeclaredExecGroup.DEFAULT_EXEC_GROUP_NAME = "default-exec-group".
        if name == "default-exec-group" || !is_identifier(name) {
            bail!("Exec group name '{name}' is not a valid name.");
        }
        names.push(name.to_owned());
    }
    Ok(names)
}

/// A `rule(exec_groups = ...)` entry (packages/DeclaredExecGroup.java:43-53,
/// StarlarkRuleClassFunctions.execGroup, :2146-2160): canonical toolchain
/// type labels with `mandatory` and canonical execution constraint labels,
/// in declaration order.
#[derive(Clone, Debug, Default, PartialEq, Eq, Allocative)]
pub struct DeclaredExecGroup {
    pub name: String,
    pub toolchains: Vec<(String, bool)>,
    pub exec_compatible_with: Vec<String>,
}

/// [`exec_groups`] with each group's declaration, in the dict's order
/// (RuleClass.Builder keeps a LinkedHashMap, packages/RuleClass.java:747).
pub(crate) fn declared_exec_groups(value: Value<'_>) -> anyhow::Result<Vec<DeclaredExecGroup>> {
    let names = exec_groups(value)?;
    let Some(dict) = DictRef::from_value(value) else {
        return Ok(Vec::new());
    };
    Ok(names
        .into_iter()
        .zip(dict.iter())
        .map(|(name, (_, group))| {
            crate::language_modules::declared_exec_group(name.clone(), group).unwrap_or(
                DeclaredExecGroup {
                    name,
                    ..Default::default()
                },
            )
        })
        .collect())
}

/// Starlark `Identifier.isValid`.
pub(crate) fn is_identifier(name: &str) -> bool {
    !name.is_empty()
        && name
            .bytes()
            .enumerate()
            .all(|(i, b)| b == b'_' || b.is_ascii_alphabetic() || (i > 0 && b.is_ascii_digit()))
}

/// parseLabels(..., "exec_compatible_with") (StarlarkRuleClassFunctions.java:1300-1320).
pub(crate) fn exec_compatible_with(
    eval: &Evaluator<'_, '_, '_>,
    value: Value<'_>,
) -> anyhow::Result<Vec<String>> {
    let mut labels = Vec::new();
    for text in cast_strings(value, "exec_compatible_with")? {
        let label = bzl_label_checked(eval, &text).map_err(|e| {
            anyhow!("Unable to parse label '{text}' in attribute 'exec_compatible_with': {e}")
        })?;
        let label = label.to_string();
        if !labels.contains(&label) {
            labels.push(label);
        }
    }
    Ok(labels)
}

/// `StarlarkAttrModule.getStarlarkProviderIdentifiers(list, what)` (StarlarkAttrModule.java:598).
pub(crate) fn provider_identifiers(
    value: Value<'_>,
    what: &str,
) -> anyhow::Result<Vec<ProviderRequirement>> {
    let items = crate::custom_rule::sequence_ref(value)
        .ok_or_else(|| anyhow!("for {what}, got {}, want sequence", value.get_type()))?;
    for (index, item) in items.iter().enumerate() {
        // Native DefaultInfo reports type "provider"; Bazel's is "Provider".
        if !matches!(item.get_type(), "Provider" | "provider")
            && crate::providers::requirement_info(*item).is_err()
        {
            bail!(
                "at index {index} of {what}, got element of type {}, want Provider",
                item.get_type()
            );
        }
    }
    let mut result: Vec<ProviderRequirement> = Vec::new();
    for &item in items {
        let requirement = crate::providers::requirement_info(item).map_err(|_| {
            anyhow!("Providers should be top-level values in extension files that define them.")
        })?;
        if !result.iter().any(|seen| seen.id == requirement.id) {
            result.push(requirement);
        }
    }
    Ok(result)
}

/// The query-visible part of an aspect definition: its attributes (all
/// implicit, Aspect.isAttributeValueExplicitlySpecified,
/// packages/Aspect.java:108-111) and required providers.
#[derive(Clone, Debug, Default, PartialEq, Eq, Allocative)]
pub(crate) struct QueryAspect {
    pub identity: String,
    pub attrs: Vec<(String, crate::custom_rule::AttrSpec)>,
    pub required_providers: Vec<Vec<ProviderRequirement>>,
}

/// The aspects of an `aspects=` list with their required aspects first, once
/// each (AspectsList.Builder).
pub(crate) fn query_aspects(value: Value<'_>) -> Vec<QueryAspect> {
    fn add(value: Value<'_>, result: &mut Vec<QueryAspect>, depth: usize) {
        let Some(aspect) = Aspect::from_value(value) else {
            return;
        };
        let Some(identity) = aspect.identity() else {
            return;
        };
        if depth > 64 || result.iter().any(|known| known.identity == identity) {
            return;
        }
        for required in &aspect.requires {
            add(*required, result, depth + 1);
        }
        if result.iter().any(|known| known.identity == identity) {
            return;
        }
        result.push(QueryAspect {
            identity,
            attrs: aspect.declaration.attrs.clone(),
            required_providers: aspect.declaration.required_providers.clone(),
        });
    }
    let mut result = Vec::new();
    for item in crate::custom_rule::sequence_ref(value).unwrap_or_default() {
        add(*item, &mut result, 0);
    }
    result
}

/// Exported aspect identities for `attr.*(aspects=...)` / `aspect(requires=...)`.
pub(crate) fn aspect_identities(value: Value<'_>, what: &str) -> anyhow::Result<Vec<String>> {
    let items = crate::custom_rule::sequence_ref(value)
        .ok_or_else(|| anyhow!("for {what}, got {}, want sequence", value.get_type()))?;
    let mut ids = Vec::new();
    for (index, item) in items.iter().enumerate() {
        let aspect = Aspect::from_value(*item).ok_or_else(|| {
            anyhow!(
                "at index {index} of {what}, got element of type {}, want Aspect",
                item.get_type()
            )
        })?;
        // StarlarkDefinedAspect.getAspectClass (packages/StarlarkDefinedAspect.java).
        let id = aspect.identity().ok_or_else(|| {
            anyhow!("Aspects should be top-level values in extension files that define them.")
        })?;
        if !ids.contains(&id) {
            ids.push(id);
        }
    }
    Ok(ids)
}

/// Generic `configuration_field(...)` value recognition (Java
/// StarlarkLateBoundDefault, Starlark type `LateBoundDefault`).
pub(crate) fn is_late_bound_default(value: Value<'_>) -> bool {
    value.get_type() == "LateBoundDefault"
}

/// Classify an attribute `cfg` value (StarlarkAttrModule.convertCfg, :516-565).
pub(crate) fn attribute_cfg(value: Value<'_>, disable_host: bool) -> anyhow::Result<Cfg> {
    if value.is_none() {
        return Ok(Cfg::Target);
    }
    if let Some(text) = value.unpack_str() {
        match text {
            "target" => return Ok(Cfg::Target),
            // StarlarkAttrModule.java:520-531: "host" is the exec transition
            // unless --incompatible_disable_starlark_host_transitions (default
            // false, BuildLanguageOptions.java:590-598).
            "host" if disable_host => bail!(
                "'cfg = \"host\"' is deprecated and should no longer be used. Please use 'cfg = \"exec\"' instead."
            ),
            "host" | "exec" => return Ok(Cfg::Exec(None)),
            _ => {}
        }
    }
    if let Some(cfg) = transition_cfg(value) {
        return Ok(cfg);
    }
    bail!(
        "cfg must be either 'target', 'exec' or a starlark defined transition defined by the exec() or transition() functions."
    )
}

/// Classify a rule `cfg` value (StarlarkRuleClassFunctions.convertConfig, :1240-1277).
/// The exec transition is attribute-only (TransitionType.ATTRIBUTE).
pub(crate) fn rule_cfg(value: Value<'_>) -> anyhow::Result<Cfg> {
    if value.is_none() {
        return Ok(Cfg::Target);
    }
    match transition_cfg(value) {
        Some(cfg @ (Cfg::Target | Cfg::NoConfig | Cfg::Starlark { .. })) => Ok(cfg),
        _ => bail!(
            "`cfg` must be set to a transition object initialized by the transition() function."
        ),
    }
}

fn transition_cfg(value: Value<'_>) -> Option<Cfg> {
    if let Some(exec) = ExecTransition::from_value(value) {
        return Some(Cfg::Exec(exec.exec_group.clone()));
    }
    if let Some(noop) = NoOpTransition::from_value(value) {
        return Some(if noop.no_config {
            Cfg::NoConfig
        } else {
            Cfg::Target
        });
    }
    let t = Transition::from_value(value)?;
    Some(Cfg::Starlark {
        definition: t.definition.clone(),
        inputs: t.inputs.clone(),
        outputs: t.outputs.clone(),
        analysis_test: t.analysis_test,
        id: t.id,
    })
}

/// Whether `value` is a Starlark `transition()` object (not an
/// `analysis_test_transition`): the transition value an attribute or rule
/// retains for analysis (crate::starlark_transitions).
pub(crate) fn is_starlark_transition(value: Value<'_>) -> bool {
    Transition::from_value(value).is_some_and(|t| !t.analysis_test)
}

/// A fresh transition identity.
fn next_transition_id() -> u64 {
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
    NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
}

/// The location of the current call's left parenthesis, `FILE:LINE:COLUMN`
/// (StarlarkThread.getCallerLocation: the call expression's lparen).
fn caller_lparen_location(eval: &Evaluator<'_, '_, '_>) -> String {
    let Some(location) = eval.call_stack_top_location() else {
        return String::new();
    };
    let source = location.file.source();
    let begin = location.span.begin().get() as usize;
    let end = (location.span.end().get() as usize).min(source.len());
    let lparen = source
        .get(begin..end)
        .and_then(|text| text.find('('))
        .map_or(begin, |offset| begin + offset);
    let position = starlark::codemap::Pos::new(lparen as u32);
    let resolved = location
        .file
        .resolve_span(starlark::codemap::Span::new(position, position));
    format!(
        "{}:{}:{}",
        location.filename(),
        resolved.begin.line + 1,
        resolved.begin.column + 1
    )
}

/// The canonical forms of transition settings: native options unchanged,
/// labels relative to the defining `.bzl` file.
fn canonical_settings(
    eval: &Evaluator<'_, '_, '_>,
    settings: &[String],
) -> anyhow::Result<Vec<String>> {
    settings
        .iter()
        .map(|setting| {
            if setting.starts_with(COMMAND_LINE_OPTION_PREFIX) {
                Ok(setting.clone())
            } else {
                bzl_label(eval, setting).map(|label| canonical_form(&label))
            }
        })
        .collect()
}

// ---------------------------------------------------------------------------
// select().

/// One owned-view element of a `select()` expression, for attribute coercion.
pub(crate) enum SelectPiece<'v> {
    Plain(Value<'v>),
    Select {
        branches: Vec<(Value<'v>, Value<'v>)>,
        no_match_error: String,
    },
}

/// Pieces of a select expression; `None` for any other value.
pub(crate) fn select_pieces<'v>(value: Value<'v>) -> Option<Vec<SelectPiece<'v>>> {
    let pieces = SelectorList::from_value(value)?.pieces();
    Some(
        pieces
            .values
            .into_iter()
            .zip(pieces.selects)
            .map(|(value, select)| match select {
                None => SelectPiece::Plain(value),
                Some(no_match_error) => SelectPiece::Select {
                    branches: DictRef::from_value(value)
                        .expect("select element owns a dict")
                        .iter()
                        .collect(),
                    no_match_error,
                },
            })
            .collect(),
    )
}

pub(crate) struct Pieces<'v> {
    values: Vec<Value<'v>>,
    selects: Vec<Option<String>>,
    native_type: String,
    type_name: String,
}

fn pieces_of(value: Value<'_>) -> Pieces<'_> {
    if let Some(list) = SelectorList::from_value(value) {
        return list.pieces();
    }
    Pieces {
        values: vec![value],
        selects: vec![None],
        native_type: value.get_type().to_owned(),
        type_name: value.get_type().to_owned(),
    }
}

/// SelectorList.canConcatenate (SelectorList.java:205): equal types or two list types.
fn can_concatenate(first: &str, second: &str) -> bool {
    first == second || (matches!(first, "list" | "tuple") && matches!(second, "list" | "tuple"))
}

/// SelectorList.of(Iterable) (SelectorList.java:126-150).
fn concat<'v>(left: Pieces<'v>, right: Pieces<'v>, heap: Heap<'v>) -> anyhow::Result<Value<'v>> {
    if !can_concatenate(&left.native_type, &right.native_type) {
        bail!(
            "Cannot combine incompatible types ({}, {})",
            left.type_name,
            right.type_name
        );
    }
    let mut values = left.values;
    values.extend(right.values);
    let mut selects = left.selects;
    selects.extend(right.selects);
    let mut budget = HostBudget::default();
    for _ in &values {
        budget.node().map_err(anyhow::Error::msg)?;
    }
    Ok(heap.alloc(SelectorListGen {
        values,
        selects,
        native_type: left.native_type,
    }))
}

// ---------------------------------------------------------------------------
// Globals.

#[starlark_module]
fn select_globals(builder: &mut GlobalsBuilder) {
    /// SelectorList.SelectLibrary.select (SelectorList.java:253-310). With the
    /// default --noincompatible_resolve_select_keys_eagerly (BuildLanguageOptions.java:854)
    /// string keys stay strings and resolve at rule instantiation.
    fn select<'v>(
        #[starlark(require = pos)] x: Value<'v>,
        #[starlark(default = "")] no_match_error: &str,
        heap: Heap<'v>,
    ) -> anyhow::Result<Value<'v>> {
        let dict = DictRef::from_value(x).ok_or_else(|| {
            anyhow!(
                "in call to select(), parameter 'x' got value of type '{}', want 'dict'",
                x.get_type()
            )
        })?;
        if dict.is_empty() {
            bail!(
                "select({{}}) with an empty dictionary can never resolve because it includes no conditions to match"
            );
        }
        let mut budget = HostBudget::default();
        budget.string(no_match_error).map_err(anyhow::Error::msg)?;
        let mut entries = Vec::new();
        for (key, value) in dict.iter() {
            budget.node().map_err(anyhow::Error::msg)?;
            if key.unpack_str().is_none() && crate::starlark_types::label_parts(key).is_none() {
                bail!(
                    "select: got {} for dict key, want a Label or label string",
                    key.get_type()
                );
            }
            entries.push((key, value));
        }
        let native_type = entries[0].1.get_type().to_owned();
        let copy = heap.alloc(AllocDict(entries));
        Ok(heap.alloc(SelectorListGen {
            values: vec![copy],
            selects: vec![Some(no_match_error.to_owned())],
            native_type,
        }))
    }
}

// aspect() mirrors Bazel's 17 independently named parameters; scope the
// allowance to the generated constructor wrappers.
#[allow(clippy::too_many_arguments)]
mod declaration_globals {
    use super::*;
    #[starlark_module]
    pub(super) fn bzl_globals(builder: &mut GlobalsBuilder) {
        /// ConfigGlobalLibrary.transition (rules/config/ConfigGlobalLibrary.java:49-79;
        /// ConfigGlobalLibraryApi.java:33-95).
        fn transition<'v>(
            #[starlark(args)] positional: starlark::values::tuple::UnpackTuple<Value<'v>>,
            #[starlark(require = named)] implementation: Option<Value<'v>>,
            #[starlark(require = named)] inputs: Option<Value<'v>>,
            #[starlark(require = named)] outputs: Option<Value<'v>>,
            eval: &mut Evaluator<'v, '_, '_>,
        ) -> anyhow::Result<Value<'v>> {
            // All three parameters are named-only (ConfigGlobalLibraryApi.java:50-90).
            if !positional.items.is_empty() {
                bail!("transition() got unexpected positional argument");
            }
            let missing = [
                ("implementation", implementation.is_none()),
                ("inputs", inputs.is_none()),
                ("outputs", outputs.is_none()),
            ]
            .into_iter()
            .filter_map(|(name, missing)| missing.then_some(name))
            .collect::<Vec<_>>();
            if !missing.is_empty() {
                bail!(
                    "transition() missing {} required named argument{}: {}",
                    missing.len(),
                    if missing.len() == 1 { "" } else { "s" },
                    missing.join(", ")
                );
            }
            let (implementation, inputs, outputs) = (
                implementation.expect("checked"),
                inputs.expect("checked"),
                outputs.expect("checked"),
            );
            let definition = crate::loading::definition_label(eval)
                .ok_or_else(|| anyhow!("transition() can only be used in a .bzl file"))?;
            if !is_function(implementation)
                && !matches!(
                    implementation.get_type(),
                    "function" | "builtin_function_or_method"
                )
            {
                bail!(
                    "in call to transition(), parameter 'implementation' got value of type '{}', want 'callable'",
                    implementation.get_type()
                );
            }
            for (name, value) in [("inputs", inputs), ("outputs", outputs)] {
                if crate::custom_rule::sequence_ref(value).is_none() {
                    bail!(
                        "in call to transition(), parameter '{name}' got value of type '{}', want 'sequence'",
                        value.get_type()
                    );
                }
            }
            let inputs = cast_strings(inputs, "inputs")?;
            let outputs = cast_strings(outputs, "outputs")?;
            validate_setting_keys(eval, &inputs, "input", false)?;
            validate_setting_keys(eval, &outputs, "output", false)?;
            // StarlarkDefinedConfigTransition constructor canonicalizes outputs, then inputs
            // (StarlarkDefinedConfigTransition.java:110-117); disallowed options default empty.
            canonicalize_settings(eval, &outputs, "OUTPUTS")?;
            canonicalize_settings(eval, &inputs, "INPUTS")?;
            let canonical_inputs = canonical_settings(eval, &inputs)?;
            let canonical_outputs = canonical_settings(eval, &outputs)?;
            Ok(eval.heap().alloc(TransitionGen {
                implementation,
                settings: Value::new_none(),
                analysis_test: false,
                inputs,
                outputs,
                definition: definition.to_string(),
                canonical_inputs,
                canonical_outputs,
                location: caller_lparen_location(eval),
                id: next_transition_id(),
            }))
        }

        /// ConfigGlobalLibrary.analysisTestTransition
        /// (rules/config/ConfigGlobalLibrary.java:120-140; ConfigGlobalLibraryApi.java:101-130)
        /// and StarlarkDefinedConfigTransition.newAnalysisTestTransition
        /// (StarlarkDefinedConfigTransition.java:300-345): no inputs, the
        /// settings keys as outputs. Retained for declarations; applying it is
        /// part of analysis-test analysis (docs/LOAD_DECLARATIONS.md).
        fn analysis_test_transition<'v>(
            #[starlark(args)] positional: starlark::values::tuple::UnpackTuple<Value<'v>>,
            #[starlark(require = named)] settings: Option<Value<'v>>,
            eval: &mut Evaluator<'v, '_, '_>,
        ) -> anyhow::Result<Value<'v>> {
            // `settings` is named-only and mandatory (ConfigGlobalLibraryApi.java:115-124).
            if !positional.items.is_empty() {
                bail!("analysis_test_transition() got unexpected positional argument");
            }
            let Some(settings) = settings else {
                bail!("analysis_test_transition() missing 1 required named argument: settings");
            };
            let Some(dict) = DictRef::from_value(settings) else {
                return Err(param_type_error(
                    "analysis_test_transition",
                    "settings",
                    settings,
                    "dict",
                ));
            };
            let definition = crate::loading::definition_label(eval).ok_or_else(|| {
                anyhow!("analysis_test_transition() can only be used in a .bzl file")
            })?;
            let mut outputs = Vec::new();
            let mut entries = Vec::new();
            let mut budget = HostBudget::default();
            for (key, value) in dict.iter() {
                budget.node().map_err(anyhow::Error::msg)?;
                let Some(text) = key.unpack_str() else {
                    bail!(
                        "got dict<{}, {}> for 'changed_settings dict', want dict<string, unknown>",
                        key.get_type(),
                        value.get_type()
                    );
                };
                budget.string(text).map_err(anyhow::Error::msg)?;
                outputs.push(text.to_owned());
                entries.push((key, value));
            }
            validate_setting_keys(eval, &outputs, "output", true)?;
            canonicalize_settings(eval, &outputs, "OUTPUTS")?;
            let settings = eval.heap().alloc(AllocDict(entries));
            let canonical_outputs = canonical_settings(eval, &outputs)?;
            Ok(eval.heap().alloc(TransitionGen {
                implementation: Value::new_none(),
                settings,
                analysis_test: true,
                inputs: Vec::new(),
                outputs,
                definition: definition.to_string(),
                canonical_inputs: Vec::new(),
                canonical_outputs,
                location: caller_lparen_location(eval),
                id: next_transition_id(),
            }))
        }

        /// StarlarkRuleClassFunctions.aspect (StarlarkRuleClassFunctions.java:1323-1520;
        /// parameter order/types: StarlarkRuleFunctionsApi.java:790-1028).
        fn aspect<'v>(
            implementation: Value<'v>,
            #[starlark(default = AllocList::EMPTY)] attr_aspects: Value<'v>,
            #[starlark(default = AllocList::EMPTY)] toolchains_aspects: Value<'v>,
            #[starlark(default = AllocDict::EMPTY)] attrs: Value<'v>,
            #[starlark(default = AllocList::EMPTY)] required_providers: Value<'v>,
            #[starlark(default = AllocList::EMPTY)] required_aspect_providers: Value<'v>,
            #[starlark(default = AllocList::EMPTY)] provides: Value<'v>,
            #[starlark(default = AllocList::EMPTY)] requires: Value<'v>,
            #[starlark(default = NoneType)] propagation_predicate: Value<'v>,
            #[starlark(default = AllocList::EMPTY)] fragments: Value<'v>,
            #[starlark(default = AllocList::EMPTY)] host_fragments: Value<'v>,
            #[starlark(default = AllocList::EMPTY)] toolchains: Value<'v>,
            #[starlark(default = NoneType)] doc: Value<'v>,
            #[starlark(require = named, default = false)] apply_to_generating_rules: bool,
            #[starlark(require = named, default = AllocList::EMPTY)] exec_compatible_with: Value<
                'v,
            >,
            #[starlark(require = named, default = NoneType)] exec_groups: Value<'v>,
            #[starlark(require = named, default = AllocList::EMPTY)] subrules: Value<'v>,
            eval: &mut Evaluator<'v, '_, '_>,
        ) -> anyhow::Result<Value<'v>> {
            construct_aspect(
                AspectArguments {
                    implementation,
                    attr_aspects,
                    toolchains_aspects,
                    attrs,
                    required_providers,
                    required_aspect_providers,
                    provides,
                    requires,
                    propagation_predicate,
                    fragments,
                    host_fragments,
                    toolchains,
                    doc,
                    apply_to_generating_rules,
                    exec_compatible_with,
                    exec_groups,
                    subrules,
                },
                eval,
            )
        }
    }
}
use declaration_globals::bzl_globals;

/// validateBuildSettingKeys (ConfigGlobalLibrary.java:139-178).
fn validate_setting_keys(
    eval: &Evaluator<'_, '_, '_>,
    keys: &[String],
    singular: &str,
    allow_incompatible_and_experimental: bool,
) -> anyhow::Result<()> {
    let mut seen = BTreeSet::new();
    for key in keys {
        if let Some(option) = key.strip_prefix(COMMAND_LINE_OPTION_PREFIX) {
            if !allow_incompatible_and_experimental && !valid_option_name(option) {
                bail!(
                    "Invalid transition {singular} '{key}'. Cannot transition on --experimental_* or --incompatible_* options"
                );
            }
        } else {
            // Label.parseWithRepoContext: absolute labels only, Bazel syntax texts.
            if let Err(error) = crate::bazel_label::Parts::parse_with_repo_context(key) {
                bail!(
                    "invalid transition {singular} '{key}'. If this is intended as a native option, it must begin with //command_line_option: {error}"
                );
            }
            if let Err(error) = bzl_label(eval, key) {
                bail!("invalid transition {singular} '{key}': {error}");
            }
        }
        if !seen.insert(key.as_str()) {
            bail!("duplicate transition {singular} '{key}'");
        }
    }
    Ok(())
}

/// LabelConstants.COMMAND_LINE_OPTION_PREFIX (cmdline/LabelConstants.java:65).
const COMMAND_LINE_OPTION_PREFIX: &str = "//command_line_option:";

/// ConfigGlobalLibrary.validOptionName (ConfigGlobalLibrary.java:187-204).
fn valid_option_name(option: &str) -> bool {
    if option.starts_with("experimental_") {
        return false;
    }
    if matches!(
        option,
        "incompatible_enable_cc_toolchain_resolution"
            | "incompatible_enable_apple_toolchain_resolution"
    ) {
        return true;
    }
    !option.starts_with("incompatible_")
}

/// getCanonicalizedSettings duplicate detection (StarlarkDefinedConfigTransition.java:153-186).
fn canonicalize_settings(
    eval: &Evaluator<'_, '_, '_>,
    settings: &[String],
    which: &str,
) -> anyhow::Result<()> {
    let mut canonical: Vec<(String, &str)> = Vec::new();
    for setting in settings {
        let label = if setting.starts_with(COMMAND_LINE_OPTION_PREFIX) {
            setting.clone()
        } else {
            let label = bzl_label(eval, setting).map_err(|_| {
                anyhow!("Malformed label in transition {which} parameter: '{setting}'")
            })?;
            canonical_form(&label)
        };
        if let Some((_, previous)) = canonical.iter().find(|(seen, _)| *seen == label) {
            bail!(
                "Transition declares duplicate build setting '{label}' in {which} (specified as '{setting}' and '{previous}')"
            );
        }
        canonical.push((label, setting));
    }
    Ok(())
}

struct AspectArguments<'v> {
    implementation: Value<'v>,
    attr_aspects: Value<'v>,
    toolchains_aspects: Value<'v>,
    attrs: Value<'v>,
    required_providers: Value<'v>,
    required_aspect_providers: Value<'v>,
    provides: Value<'v>,
    requires: Value<'v>,
    propagation_predicate: Value<'v>,
    fragments: Value<'v>,
    host_fragments: Value<'v>,
    toolchains: Value<'v>,
    doc: Value<'v>,
    apply_to_generating_rules: bool,
    exec_compatible_with: Value<'v>,
    exec_groups: Value<'v>,
    subrules: Value<'v>,
}

fn construct_aspect<'v>(
    args: AspectArguments<'v>,
    eval: &mut Evaluator<'v, '_, '_>,
) -> anyhow::Result<Value<'v>> {
    // BuiltinFunction checks @Param allowedTypes, in parameter order, before the
    // body runs (StarlarkRuleFunctionsApi.java:790-1008).
    if !is_function(args.implementation) {
        return Err(param_type_error(
            "aspect",
            "implementation",
            args.implementation,
            "function",
        ));
    }
    for (param, value) in [
        ("attr_aspects", args.attr_aspects),
        ("toolchains_aspects", args.toolchains_aspects),
    ] {
        if !is_function(value) && crate::custom_rule::sequence_ref(value).is_none() {
            return Err(param_type_error(
                "aspect",
                param,
                value,
                "sequence or function",
            ));
        }
    }
    if DictRef::from_value(args.attrs).is_none() {
        return Err(param_type_error("aspect", "attrs", args.attrs, "dict"));
    }
    for (param, value) in [
        ("required_providers", args.required_providers),
        ("required_aspect_providers", args.required_aspect_providers),
        ("provides", args.provides),
        ("requires", args.requires),
    ] {
        require_sequence("aspect", param, value)?;
    }
    if !args.propagation_predicate.is_none() && !is_function(args.propagation_predicate) {
        return Err(param_type_error(
            "aspect",
            "propagation_predicate",
            args.propagation_predicate,
            "function or NoneType",
        ));
    }
    for (param, value) in [
        ("fragments", args.fragments),
        ("host_fragments", args.host_fragments),
        ("toolchains", args.toolchains),
    ] {
        require_sequence("aspect", param, value)?;
    }
    if !args.doc.is_none() && args.doc.unpack_str().is_none() {
        return Err(param_type_error(
            "aspect",
            "doc",
            args.doc,
            "string or NoneType",
        ));
    }
    require_sequence("aspect", "exec_compatible_with", args.exec_compatible_with)?;
    if !args.exec_groups.is_none() && DictRef::from_value(args.exec_groups).is_none() {
        return Err(param_type_error(
            "aspect",
            "exec_groups",
            args.exec_groups,
            "dict or NoneType",
        ));
    }
    require_sequence("aspect", "subrules", args.subrules)?;
    // BzlInitThreadContext.fromOrFail(thread, "aspect()") (BzlInitThreadContext.java:88).
    let context = eval
        .extra
        .and_then(|extra| extra.downcast_ref::<ExtensionContext>())
        .ok_or_else(|| {
            anyhow!("aspect() can only be used during .bzl initialization (top-level evaluation)")
        })?;
    let definition_label = context.label.clone();
    let mut attrs = crate::custom_rule::attribute_dict(args.attrs)?;
    // Subrules need the ALLOWLIST_RULE_EXTENSION_API restriction only when
    // --experimental_rule_extension_api is off (StarlarkRuleClassFunctions.java:1349-1353);
    // it defaults to true in 9.3 (FlagConstants.java:24), so any .bzl may pass them.
    let mut budget = HostBudget::default();
    let subrules = crate::subrule::snapshot(args.subrules, &mut budget)?;
    crate::subrule::validate_exports(&subrules, &mut budget)?;
    // Subrule attributes follow the aspect's own (StarlarkRuleClassFunctions.java:1355-1365).
    attrs.extend(crate::subrule::lifted_attributes(&subrules, &mut budget)?);
    let mut required_parameters = Vec::new();
    for (name, spec) in &attrs {
        aspect_attribute(name, spec, &mut required_parameters)?;
    }
    if args.apply_to_generating_rules
        && crate::custom_rule::sequence_ref(args.required_providers)
            .is_some_and(|items| !items.is_empty())
    {
        bail!(
            "An aspect cannot simultaneously have required providers and apply to generating rules."
        );
    }
    let mut callbacks = Vec::new();
    if !args.propagation_predicate.is_none() {
        if !is_function(args.propagation_predicate) {
            bail!(
                "Expected a function in 'propagation_predicate' parameter, got '{}'.",
                args.propagation_predicate.get_type()
            );
        }
        if args.apply_to_generating_rules {
            bail!(
                "An aspect cannot simultaneously have a propagation predicate and apply to generating rules."
            );
        }
        callbacks.push(args.propagation_predicate);
    }
    let exec_compatible_with = exec_compatible_with(eval, args.exec_compatible_with)?;
    let exec_groups = exec_groups(args.exec_groups)?;
    // The aspect's toolchain types, then its subrules' (StarlarkRuleClassFunctions.java:1488-1493).
    let mut toolchains = toolchains(eval, args.toolchains, eval.heap())?;
    for toolchain in crate::subrule::lifted_toolchains(&subrules, &mut budget)? {
        if !toolchains.contains(&toolchain) {
            toolchains.push(toolchain);
        }
    }
    let attr_aspects = if is_function(args.attr_aspects) {
        callbacks.push(args.attr_aspects);
        None
    } else {
        Some(attr_aspects_list(args.attr_aspects)?)
    };
    let toolchains_aspects = if is_function(args.toolchains_aspects) {
        callbacks.push(args.toolchains_aspects);
        None
    } else {
        Some(toolchains_aspects_list(eval, args.toolchains_aspects)?)
    };
    let required_providers =
        crate::custom_rule::provider_predicate(args.required_providers, "required_providers")?;
    let required_aspect_providers = crate::custom_rule::provider_predicate(
        args.required_aspect_providers,
        "required_aspect_providers",
    )?;
    let provides = provider_identifiers(args.provides, "provides")?;
    let requires = aspect_identities(args.requires, "requires")?;
    let required = crate::custom_rule::sequence_ref(args.requires)
        .expect("validated requires")
        .to_vec();
    let fragments = cast_strings(args.fragments, "fragments")?;
    let host_fragments = cast_strings(args.host_fragments, "host_fragments")?;
    let label = definition_label.to_string();
    budget.string(&label).map_err(anyhow::Error::msg)?;
    Ok(eval.heap().alloc(AspectGen {
        implementation: args.implementation,
        callbacks,
        requires: required,
        subrules,
        declaration: AspectDeclaration {
            attr_aspects,
            toolchains_aspects,
            attrs,
            required_parameters,
            required_providers,
            required_aspect_providers,
            provides,
            requires,
            fragments,
            host_fragments,
            toolchains,
            apply_to_generating_rules: args.apply_to_generating_rules,
            exec_compatible_with,
            exec_groups,
        },
        definition_label: label,
        exported: OnceLock::new(),
    }))
}

/// Per-attribute aspect checks (StarlarkRuleClassFunctions.java:1368-1437).
fn aspect_attribute(name: &str, spec: &AttrSpec, required: &mut Vec<String>) -> anyhow::Result<()> {
    use crate::custom_rule::{AttrKind, AttrValue};
    if !name.starts_with('_') {
        let has_default = match (&spec.kind, &spec.default) {
            (AttrKind::String, AttrValue::String(value)) => !value.is_empty(),
            (AttrKind::Int, AttrValue::Int(value)) => *value != 0,
            (AttrKind::Bool, AttrValue::Bool(value)) => *value,
            _ => bail!(
                "Aspect parameter attribute '{name}' must have type 'bool', 'int' or 'string'."
            ),
        };
        if has_default
            && !spec.declaration.values.is_empty()
            && !spec.declaration.allows(&spec.default)
        {
            bail!(
                "Aspect parameter attribute '{name}' has a bad default value: {}",
                spec.declaration.allowed_values_reason(&spec.default)
            );
        }
        if !has_default || spec.mandatory {
            required.push(name.to_owned());
        }
    } else if !spec.declaration.explicit_default
        && !spec.declaration.late_bound
        && matches!(
            spec.kind,
            AttrKind::Label | AttrKind::Output | AttrKind::OutputList
        )
    {
        // Descriptor.hasDefault is Attribute.Builder.isValueSet: every attr.*()
        // with a non-None @Param default (False, 0, '', [], {}) sets a value;
        // only attr.label (default None) and outputs (no default) do not.
        bail!("Aspect attribute '{name}' has no default value.");
    }
    // StarlarkRuleClassFunctions.java:1435-1446, with the Type.toString name.
    if spec.declaration.computed_default {
        bail!(
            "Aspect attribute '{name}' ({}) with computed default value is unsupported.",
            spec.kind.type_name()
        );
    }
    Ok(())
}

/// parseAttrAspects (AspectPropagationEdgesSupplier.java:194-217).
fn attr_aspects_list(value: Value<'_>) -> anyhow::Result<Vec<String>> {
    if crate::custom_rule::sequence_ref(value).is_none() {
        bail!(
            "in call to aspect(), parameter 'attr_aspects' got value of type '{}', want 'sequence or function'",
            value.get_type()
        );
    }
    let names = cast_strings(value, "attr_aspects")?;
    let mut result = Vec::new();
    for name in &names {
        if name == "*" && names.len() != 1 {
            bail!("'*' must be the only string in 'attr_aspects' list");
        }
        if !result.contains(name) {
            result.push(name.clone());
        }
    }
    Ok(result)
}

/// parseToolchainsAspects (AspectPropagationEdgesSupplier.java:219-243).
fn toolchains_aspects_list(
    eval: &Evaluator<'_, '_, '_>,
    value: Value<'_>,
) -> anyhow::Result<Vec<String>> {
    if crate::custom_rule::sequence_ref(value).is_none() {
        bail!(
            "in call to aspect(), parameter 'toolchains_aspects' got value of type '{}', want 'sequence or function'",
            value.get_type()
        );
    }
    let items = cast_strings(value, "toolchains_aspects")?;
    if items.len() == 1 && items[0] == "*" {
        return Ok(items);
    }
    let mut labels = Vec::new();
    for input in &items {
        if input == "*" {
            bail!("'*' must be the only item in 'toolchains_aspects' list");
        }
        let label = bzl_label(eval, input).map_err(|e| {
            anyhow!("Unable to parse label '{input}' in attribute 'toolchains_aspects': {e}")
        })?;
        let label = label.to_string();
        if !labels.contains(&label) {
            labels.push(label);
        }
    }
    Ok(labels)
}

/// Install `aspect`, `transition`, `analysis_test_transition`, `select`,
/// `OutputGroupInfo`, `AnalysisTestResultInfo` and `AnalysisFailureInfo`
/// into `.bzl` globals.
pub fn install(builder: &mut GlobalsBuilder) {
    bzl_globals(builder);
    select_globals(builder);
    builder.set("OutputGroupInfo", OutputGroupInfoConstructor);
    for name in ["AnalysisTestResultInfo", "AnalysisFailureInfo"] {
        builder.set(name, TestingProvider { name });
    }
}

/// `select` is a BUILD and .bzl global (SelectorList.java:241 environment = {BUILD, BZL}).
pub fn install_build(builder: &mut GlobalsBuilder) {
    select_globals(builder);
}

pub(crate) fn is_output_group_info(value: Value<'_>) -> bool {
    OutputGroupInfoConstructor::from_value(value).is_some()
}

/// Construct `config.exec(exec_group)` (StarlarkConfig.java:68-73).
pub(crate) fn config_exec<'v>(heap: Heap<'v>, exec_group: Option<&str>) -> Value<'v> {
    heap.alloc(ExecTransition {
        exec_group: exec_group.map(str::to_owned),
    })
}

/// Construct `config.target()` / `config.none()` (StarlarkConfig.java:75-83).
pub(crate) fn config_noop<'v>(heap: Heap<'v>, no_config: bool) -> Value<'v> {
    heap.alloc(NoOpTransition { no_config })
}

#[cfg(test)]
#[allow(unsafe_code)]
mod tests {
    use super::*;
    use crate::custom_rule::{AttrKind, AttrValue, ConfigurablePart};
    use starlark::environment::{FrozenModule, Globals, LibraryExtension, Module};
    use starlark::syntax::{AstModule, Dialect};
    use starlark::values::{
        NoSerialize, ProvidesStaticType, StarlarkPagablePanic, StarlarkValue, starlark_value,
    };
    use std::sync::Arc;

    // Generic stand-ins for the language-module values defined elsewhere
    // (config_common.toolchain_type, exec_group, configuration_field): this
    // module recognizes them only by Starlark type and public fields.
    #[derive(Debug, ProvidesStaticType, NoSerialize, Allocative, StarlarkPagablePanic)]
    struct FakeToolchainType {
        label: String,
        mandatory: bool,
    }
    starlark::starlark_simple_value!(FakeToolchainType);
    impl fmt::Display for FakeToolchainType {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.write_str("<toolchain_type>")
        }
    }
    #[starlark_value(type = "toolchain_type")]
    impl<'v> StarlarkValue<'v> for FakeToolchainType {
        fn get_attr(&self, name: &str, heap: Heap<'v>) -> Option<Value<'v>> {
            match name {
                "toolchain_type" => crate::starlark_types::alloc_label(heap, &self.label).ok(),
                "mandatory" => Some(Value::new_bool(self.mandatory)),
                _ => None,
            }
        }
    }

    #[derive(Debug, ProvidesStaticType, NoSerialize, Allocative, StarlarkPagablePanic)]
    struct FakeExecGroup;
    starlark::starlark_simple_value!(FakeExecGroup);
    impl fmt::Display for FakeExecGroup {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.write_str("<exec_group>")
        }
    }
    #[starlark_value(type = "exec_group")]
    impl<'v> StarlarkValue<'v> for FakeExecGroup {}

    #[derive(Debug, ProvidesStaticType, NoSerialize, Allocative, StarlarkPagablePanic)]
    struct FakeLateBound;
    starlark::starlark_simple_value!(FakeLateBound);
    impl fmt::Display for FakeLateBound {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.write_str("<late-bound default>")
        }
    }
    #[starlark_value(type = "LateBoundDefault")]
    impl<'v> StarlarkValue<'v> for FakeLateBound {}

    #[starlark_module]
    fn fakes(builder: &mut GlobalsBuilder) {
        fn fake_toolchain_type<'v>(
            #[starlark(require = pos)] label: &str,
            #[starlark(default = true)] mandatory: bool,
            heap: Heap<'v>,
        ) -> anyhow::Result<Value<'v>> {
            Ok(heap.alloc(FakeToolchainType {
                label: crate::label::Label::parse(label, "")
                    .map_err(anyhow::Error::msg)?
                    .to_string(),
                mandatory,
            }))
        }
        fn fake_exec_group<'v>(heap: Heap<'v>) -> anyhow::Result<Value<'v>> {
            Ok(heap.alloc(FakeExecGroup))
        }
        fn fake_configuration_field<'v>(heap: Heap<'v>) -> anyhow::Result<Value<'v>> {
            Ok(heap.alloc(FakeLateBound))
        }
    }

    fn globals() -> Globals {
        let mut builder =
            GlobalsBuilder::extended_by(&[LibraryExtension::StructType, LibraryExtension::Print]);
        crate::custom_rule::rule_globals(&mut builder);
        crate::build_settings::install(&mut builder);
        crate::providers::globals(&mut builder);
        crate::subrule::globals(&mut builder);
        install(&mut builder);
        fakes(&mut builder);
        builder.build()
    }

    fn evaluate(label: &str, source: &str) -> Result<FrozenModule, String> {
        let globals = globals();
        Module::with_temp_heap(|module| {
            let context = ExtensionContext {
                byte_strings: false,
                label: crate::label::Label::parse_canonical(label).unwrap(),
                repositories: Arc::new(crate::repository::RepositorySet::new(".".into())),
            };
            {
                let mut eval = Evaluator::new(&module);
                eval.extra = Some(&context);
                eval.eval_module(
                    AstModule::parse(label, source.into(), &Dialect::Extended)
                        .map_err(|e| e.to_string())?,
                    &globals,
                )
                .map_err(|e| e.to_string())?;
            }
            module.freeze().map_err(|e| format!("{e:?}"))
        })
    }

    fn passes(source: &str) -> FrozenModule {
        evaluate("//pkg:defs.bzl", source).unwrap_or_else(|e| panic!("{source}\n{e}"))
    }

    fn fails(source: &str, expected: &str) {
        match evaluate("//pkg:defs.bzl", source) {
            Ok(_) => panic!("accepted:\n{source}"),
            Err(error) => assert!(
                error.contains(expected),
                "{source}\nexpected {expected:?}\n{error}"
            ),
        }
    }

    const IMPL: &str = "def impl(ctx):\n    return []\ndef aimpl(target, ctx):\n    return []\ndef timpl(settings, attr):\n    return {}\nP = provider()\n";

    #[test]
    fn transitions_validate_settings_like_config_global_library() {
        passes(&format!(
            "{IMPL}t = transition(implementation = timpl, inputs = ['//command_line_option:cpu', '//pkg:flag'], outputs = ['//command_line_option:incompatible_enable_cc_toolchain_resolution'])\n\
             (type(t) == 'transition' and repr(t) == '<transition object>' and dir(t) == ['and_then']) or fail('shape')\n"
        ));
        for (call, expected) in [
            (
                "transition(implementation = 1, inputs = [], outputs = [])",
                "in call to transition(), parameter 'implementation' got value of type 'int', want 'callable'",
            ),
            (
                "transition(implementation = timpl, inputs = 'x', outputs = [])",
                "in call to transition(), parameter 'inputs' got value of type 'string', want 'sequence'",
            ),
            (
                "transition(implementation = timpl, inputs = [1], outputs = [])",
                "at index 0 of inputs, got element of type int, want string",
            ),
            (
                "transition(implementation = timpl, inputs = [], outputs = ['//command_line_option:experimental_x'])",
                "Invalid transition output '//command_line_option:experimental_x'. Cannot transition on --experimental_* or --incompatible_* options",
            ),
            (
                "transition(implementation = timpl, inputs = ['//command_line_option:incompatible_x'], outputs = [])",
                "Invalid transition input '//command_line_option:incompatible_x'",
            ),
            (
                "transition(implementation = timpl, inputs = ['//:a', '//:a'], outputs = [])",
                "duplicate transition input '//:a'",
            ),
            (
                "transition(implementation = timpl, inputs = [], outputs = ['//pkg:a', '@@//pkg:a'])",
                "Transition declares duplicate build setting '//pkg:a' in OUTPUTS (specified as '@@//pkg:a' and '//pkg:a')",
            ),
            (
                "transition(implementation = timpl, inputs = [], outputs = [':a', '//pkg:a'])",
                "invalid transition output ':a'. If this is intended as a native option, it must begin with //command_line_option: invalid label ':a': absolute label must begin with '@' or '//'",
            ),
            (
                "transition(implementation = timpl, inputs = [':flag'], outputs = [])",
                "invalid transition input ':flag'. If this is intended as a native option, it must begin with //command_line_option: invalid label ':flag': absolute label must begin with '@' or '//'",
            ),
            (
                "transition(implementation = timpl, inputs = ['bad:label:x'], outputs = [])",
                "invalid transition input 'bad:label:x'. If this is intended as a native option, it must begin with //command_line_option: invalid target name 'label:x': target names may not contain ':'",
            ),
            (
                "transition(timpl, [], [])",
                "transition() got unexpected positional argument",
            ),
            (
                "transition(implementation = timpl)",
                "transition() missing 2 required named arguments: inputs, outputs",
            ),
        ] {
            fails(&format!("{IMPL}x = {call}\n"), expected);
        }
    }

    #[test]
    fn config_transition_factories_have_bazel_types_and_reprs() {
        passes(
            "e = config.exec('go_link')\n\
             (type(e) == 'ExecTransitionFactory' and repr(e) == '<unknown object com.google.devtools.build.lib.analysis.config.ExecutionTransitionFactory>') or fail('exec')\n\
             (e == e and e != config.exec('go_link') and e != config.exec()) or fail('exec identity')\n\
             (dir(e) == ['and_then'] and dir(config.target()) == ['and_then'] and dir(config.none()) == ['and_then']) or fail('dir')\n\
             (type(e.and_then) == 'builtin_function_or_method') or fail('and_then')\n\
             (type(config.exec(exec_group = None)) == 'ExecTransitionFactory') or fail('named')\n\
             t = config.target()\n\
             (type(t) == 'transition' and repr(t) == '<unknown object com.google.devtools.build.lib.analysis.config.transitions.NoTransition$Factory>') or fail('target')\n\
             n = config.none()\n\
             (repr(n) == '<unknown object com.google.devtools.build.lib.analysis.config.transitions.NoConfigTransition$Factory>' and n != t) or fail('none')\n",
        );
        fails(
            "config.exec(1)",
            "in call to exec(), parameter 'exec_group' got value of type 'int', want 'string or NoneType'",
        );
    }

    #[test]
    fn select_values_concatenate_and_validate_like_selector_list() {
        passes(
            "s = select({'//conditions:default': ['a'], Label('//x:y'): []}, no_match_error = 'boom')\n\
             (type(s) == 'select') or fail(type(s))\n\
             c = ['p'] + select({'//conditions:default': ['a'], '//x:y': []}) + ['q']\n\
             (repr(c) == '[\"p\"] + select({\"//conditions:default\": [\"a\"], \"//x:y\": []}) + [\"q\"]') or fail(repr(c))\n\
             (type(['p'] + s) == 'select') or fail('label keys')\n\
             d = select({'//conditions:default': {}}) | {'k': 'v'}\n\
             (type(d) == 'select' and repr(d) == 'select({\"//conditions:default\": {}}) | {\"k\": \"v\"}') or fail(repr(d))\n\
             (type(('a',) + select({'//conditions:default': ['b']})) == 'select') or fail('tuple')\n",
        );
        for (source, expected) in [
            (
                "select({})",
                "select({}) with an empty dictionary can never resolve because it includes no conditions to match",
            ),
            (
                "select({1: []})",
                "select: got int for dict key, want a Label or label string",
            ),
            (
                "select([])",
                "in call to select(), parameter 'x' got value of type 'list', want 'dict'",
            ),
            (
                "select({'//conditions:default': []}) + 'x'",
                "Cannot combine incompatible types (select of list, string)",
            ),
            (
                "select({'//conditions:default': 'a'}) + select({'//conditions:default': 1})",
                "Cannot combine incompatible types (select of string, select of int)",
            ),
        ] {
            fails(source, expected);
        }
    }

    #[test]
    fn output_group_info_is_a_requirement_key_that_fails_when_constructed() {
        passes(
            "(type(OutputGroupInfo) == 'Provider' and repr(OutputGroupInfo) == '<function OutputGroupInfo>') or fail('shape')\n\
             a = attr.label_list(providers = [[OutputGroupInfo], [DefaultInfo]])\n",
        );
        fails(
            "OutputGroupInfo(x = depset())",
            "OutputGroupInfo construction is not implemented in the native profile",
        );
    }

    #[test]
    fn aspect_subrules_lift_attributes_after_own_and_toolchains_after_own() {
        // StarlarkRuleClassFunctions.java:1355-1365, 1488-1493.
        let module = passes(&format!(
            "{IMPL}def s(ctx):\n    pass\n\
             L = subrule(implementation = s, attrs = {{'_dep': attr.label(default = '//tools:t')}}, toolchains = ['//tc:s'])\n\
             a = aspect(aimpl, attrs = {{'_own': attr.label(default = '//tools:o')}}, toolchains = ['//tc:a', '//tc:s'], subrules = [L])\n",
        ));
        let owned = module.get("a").unwrap();
        let aspect = Aspect::from_value(owned.value()).unwrap();
        assert_eq!(
            aspect
                .declaration
                .attrs
                .iter()
                .map(|(name, _)| name.as_str())
                .collect::<Vec<_>>(),
            ["_own", "_//pkg:defs.bzl%L%_dep"]
        );
        assert_eq!(
            aspect.declaration.toolchains,
            [("//tc:a".to_owned(), true), ("//tc:s".to_owned(), true)]
        );
    }

    #[test]
    fn aspects_accept_bazel_parameters_and_retain_metadata() {
        let module = passes(&format!(
            "{IMPL}base = aspect(aimpl)\n\
             a = aspect(\n\
                 implementation = aimpl,\n\
                 attr_aspects = ['deps', '_hidden', 'deps'],\n\
                 toolchains_aspects = ['//tc:type'],\n\
                 attrs = {{'mode': attr.string(default = 'lite', values = ['lite', 'full']), 'level': attr.int(values = [0, 1]), '_tool': attr.label(default = '//tools:t'), '_late': attr.label(default = fake_configuration_field())}},\n\
                 required_providers = [[P], [DefaultInfo]],\n\
                 required_aspect_providers = [P],\n\
                 provides = [P],\n\
                 requires = [base],\n\
                 propagation_predicate = timpl,\n\
                 fragments = ['cpp'],\n\
                 host_fragments = ['cpp'],\n\
                 toolchains = ['//tc:a', fake_toolchain_type('//tc:b', mandatory = False), Label('//tc:a')],\n\
                 doc = 'docs',\n\
                 exec_compatible_with = ['//platforms:linux'],\n\
                 exec_groups = {{'link': fake_exec_group()}},\n\
             )\n\
             (type(a) == 'Aspect' and repr(a) == '<aspect>' and a == a and a != base) or fail('shape')\n\
             f = aspect(implementation = aimpl, attr_aspects = timpl, toolchains_aspects = ['*'], apply_to_generating_rules = True)\n"
        ));
        let value = module.get("a").unwrap();
        let aspect = Aspect::from_value(value.value()).unwrap();
        assert_eq!(aspect.identity().unwrap(), "//pkg:defs.bzl%a");
        let declaration = &aspect.declaration;
        assert_eq!(
            declaration.attr_aspects.as_deref(),
            Some(&["deps".to_owned(), "_hidden".to_owned()][..])
        );
        assert_eq!(
            declaration.toolchains,
            [("//tc:a".to_owned(), true), ("//tc:b".to_owned(), false)]
        );
        assert_eq!(declaration.required_parameters, ["level"]);
        assert_eq!(declaration.requires, ["//pkg:defs.bzl%base"]);
        assert_eq!(declaration.exec_groups, ["link"]);
        assert_eq!(declaration.exec_compatible_with, ["//platforms:linux"]);
        assert_eq!(declaration.required_providers.len(), 2);
        assert!(declaration.attrs.iter().any(|(name, spec)| name == "_late"
            && spec.declaration.late_bound
            && spec.default == AttrValue::Label(None)));
        let functional = module.get("f").unwrap();
        let functional = Aspect::from_value(functional.value()).unwrap();
        assert!(functional.declaration.attr_aspects.is_none());
        assert!(functional.declaration.apply_to_generating_rules);
    }

    #[test]
    fn aspect_errors_match_starlark_rule_class_functions() {
        for (call, expected) in [
            (
                "aspect(implementation = 1)",
                "in call to aspect(), parameter 'implementation' got value of type 'int', want 'function'",
            ),
            (
                "aspect(aimpl, attrs = {'dep': attr.label()})",
                "Aspect parameter attribute 'dep' must have type 'bool', 'int' or 'string'.",
            ),
            (
                "aspect(aimpl, attrs = {'_dep': attr.label()})",
                "Aspect attribute '_dep' has no default value.",
            ),
            (
                "aspect(aimpl, attrs = {'m': attr.string(default = 'z', values = ['a', 'b', 'c'])})",
                "Aspect parameter attribute 'm' has a bad default value: has to be one of 'a', 'b', or 'c' instead of 'z'",
            ),
            (
                "aspect(aimpl, attrs = {'bad-name': attr.string()})",
                "attribute name `bad-name` is not a valid identifier.",
            ),
            (
                "aspect(aimpl, attrs = {'x': 1})",
                "got dict<string, int> for 'attrs', want dict<string, Attribute>",
            ),
            (
                "aspect(aimpl, required_providers = [P], apply_to_generating_rules = True)",
                "An aspect cannot simultaneously have required providers and apply to generating rules.",
            ),
            (
                "aspect(aimpl, propagation_predicate = 1)",
                "in call to aspect(), parameter 'propagation_predicate' got value of type 'int', want 'function or NoneType'",
            ),
            (
                "aspect(aimpl, propagation_predicate = timpl, apply_to_generating_rules = True)",
                "An aspect cannot simultaneously have a propagation predicate and apply to generating rules.",
            ),
            (
                "aspect(aimpl, attr_aspects = ['*', 'deps'])",
                "'*' must be the only string in 'attr_aspects' list",
            ),
            (
                "aspect(aimpl, toolchains_aspects = ['//a:b', '*'])",
                "'*' must be the only item in 'toolchains_aspects' list",
            ),
            (
                "aspect(aimpl, requires = [1])",
                "at index 0 of requires, got element of type int, want Aspect",
            ),
            (
                "aspect(aimpl, exec_groups = {'default-exec-group': fake_exec_group()})",
                "Exec group name 'default-exec-group' is not a valid name.",
            ),
            (
                "aspect(aimpl, exec_groups = {'g': 1})",
                "got dict<string, int> for 'exec_group', want dict<string, exec_group>",
            ),
            (
                "aspect(aimpl, toolchains = [1])",
                "'toolchains' takes a toolchain_type, Label, or String, but instead got a Int32",
            ),
            (
                "aspect(aimpl, fragments = [1])",
                "at index 0 of fragments, got element of type int, want string",
            ),
            (
                "aspect(aimpl, provides = [1])",
                "at index 0 of provides, got element of type int, want Provider",
            ),
        ] {
            fails(&format!("{IMPL}x = {call}\n"), expected);
        }
        // --experimental_rule_extension_api defaults to true: any .bzl may pass subrules.
        passes(&format!(
            "{IMPL}def sub(ctx):\n    pass\ns = subrule(implementation = sub)\nx = aspect(aimpl, subrules = [s])\n\
             y = aspect(aimpl, attrs = {{'_flag': attr.bool(), '_n': attr.int(), '_s': attr.string(), '_l': attr.label_list()}})\n"
        ));
        for (call, expected) in [
            (
                "aspect(aimpl, exec_groups = [])",
                "in call to aspect(), parameter 'exec_groups' got value of type 'list', want 'dict or NoneType'",
            ),
            (
                "aspect(aimpl, fragments = 'cpp')",
                "in call to aspect(), parameter 'fragments' got value of type 'string', want 'sequence'",
            ),
            (
                "aspect(aimpl, attrs = [])",
                "in call to aspect(), parameter 'attrs' got value of type 'list', want 'dict'",
            ),
            (
                "aspect(aimpl, attr_aspects = 'deps')",
                "in call to aspect(), parameter 'attr_aspects' got value of type 'string', want 'sequence or function'",
            ),
        ] {
            fails(&format!("{IMPL}x = {call}\n"), expected);
        }
        // Without a .bzl initialization context (BzlInitThreadContext.fromOrFail).
        let globals = globals();
        let error = Module::with_temp_heap(|module| {
            let ast = AstModule::parse(
                "x.bzl",
                "def aimpl(target, ctx):\n    return []\nx = aspect(aimpl)".into(),
                &Dialect::Extended,
            )
            .unwrap();
            Evaluator::new(&module)
                .eval_module(ast, &globals)
                .unwrap_err()
                .to_string()
        });
        assert!(
            error.contains(
                "aspect() can only be used during .bzl initialization (top-level evaluation)"
            ),
            "{error}"
        );
    }

    fn definition(module: &FrozenModule, name: &str) -> crate::custom_rule::RuleDeclarationView {
        crate::custom_rule::rule_declaration(module.get(name).unwrap().value()).unwrap()
    }

    #[test]
    fn rules_accept_and_retain_bazel_declaration_parameters() {
        let module = passes(&format!(
            "{IMPL}t = transition(implementation = timpl, inputs = [], outputs = ['//:flag'])\n\
             a = aspect(aimpl, attr_aspects = ['deps'])\n\
             r = rule(\n\
                 impl,\n\
                 attrs = {{\n\
                     'deps': attr.label_list(aspects = [a], cfg = t, allow_files = ['.go'], flags = ['DIRECT_COMPILE_TIME_INPUT'], allow_rules = ['go_library']),\n\
                     'tool': attr.label(cfg = 'exec', executable = True, allow_single_file = True),\n\
                     '_late': attr.label(default = fake_configuration_field(), cfg = config.exec('g')),\n\
                     'counts': attr.int_list(default = [1, 2], allow_empty = False),\n\
                     'map': attr.label_keyed_string_dict(default = {{'//x:y': 'v'}}),\n\
                     'mode': attr.string(values = ['', 'opt']),\n\
                 }},\n\
                 executable = True,\n\
                 outputs = {{'bin': '%{{name}}.bin'}},\n\
                 output_to_genfiles = False,\n\
                 fragments = ['cpp'],\n\
                 host_fragments = ['java'],\n\
                 _skylark_testable = False,\n\
                 toolchains = ['//tc:a', Label('//tc:b'), fake_toolchain_type('//tc:a', mandatory = False)],\n\
                 doc = 'doc',\n\
                 provides = [P, DefaultInfo],\n\
                 dependency_resolution_rule = False,\n\
                 exec_compatible_with = ['//p:linux'],\n\
                 cfg = t,\n\
                 exec_groups = {{'g': fake_exec_group()}},\n\
                 extendable = True,\n\
                 subrules = [],\n\
             )\n\
             my_test = rule(implementation = impl, test = True, cfg = config.none())\n\
             analysis_test_test = rule(implementation = impl, analysis_test = True, cfg = config.target())\n\
             n = rule(implementation = impl, extendable = '//allow:list', initializer = impl, outputs = timpl)\n"
        ));
        let r = definition(&module, "r");
        assert!(r.declaration.executable && !r.declaration.test);
        assert_eq!(
            r.declaration.toolchains,
            [("//tc:a".to_owned(), true), ("//tc:b".to_owned(), true)]
        );
        assert_eq!(
            r.declaration
                .exec_groups
                .iter()
                .map(|group| group.name.as_str())
                .collect::<Vec<_>>(),
            ["g"]
        );
        assert_eq!(r.declaration.fragments, ["cpp"]);
        assert_eq!(r.declaration.host_fragments, ["java"]);
        assert_eq!(r.declaration.provides.len(), 2);
        assert!(matches!(r.declaration.cfg, Cfg::Starlark { .. }));
        let deps = &r.schema["deps"].declaration;
        assert_eq!(deps.aspects, ["//pkg:defs.bzl%a"]);
        assert_eq!(deps.file_types.as_deref(), Some(&[".go".to_owned()][..]));
        assert_eq!(
            deps.allow_rules.as_deref(),
            Some(&["go_library".to_owned()][..])
        );
        assert!(matches!(deps.cfg, Cfg::Starlark { .. }));
        assert!(r.schema["tool"].declaration.executable);
        assert_eq!(r.schema["tool"].declaration.cfg, Cfg::Exec(None));
        assert!(r.schema["_late"].declaration.late_bound);
        assert_eq!(
            r.schema["_late"].declaration.cfg,
            Cfg::Exec(Some("g".to_owned()))
        );
        assert_eq!(r.schema["counts"].kind, AttrKind::IntList);
        assert_eq!(r.schema["counts"].default, AttrValue::Ints(vec![1, 2]));
        assert!(r.schema["counts"].non_empty);
        assert_eq!(
            r.schema["map"].default,
            AttrValue::LabelKeyedStringDict(vec![("//x:y".to_owned(), "v".to_owned())])
        );
        let my_test = definition(&module, "my_test");
        assert!(my_test.declaration.test);
        assert_eq!(my_test.declaration.cfg, Cfg::NoConfig);
        assert!(
            definition(&module, "analysis_test_test")
                .declaration
                .analysis_test
        );
        let n = definition(&module, "n");
        assert_eq!(n.declaration.extendable.as_deref(), Some("//allow:list"));
        assert!(n.has_initializer);
    }

    #[test]
    fn rule_errors_match_starlark_rule_class_functions() {
        for (call, expected) in [
            (
                "rule(impl, cfg = config.exec())",
                "`cfg` must be set to a transition object initialized by the transition() function.",
            ),
            (
                "rule(impl, toolchains = [1])",
                "'toolchains' takes a toolchain_type, Label, or String, but instead got a Int32",
            ),
            (
                "rule(impl, toolchains = ['@@@bad'])",
                "Unable to parse toolchain_type label '@@@bad': invalid repository name '@bad': repo names may contain only A-Z, a-z, 0-9, '-', '_', '.' and '+'",
            ),
            (
                "rule(impl, exec_groups = {'1bad': fake_exec_group()})",
                "Exec group name '1bad' is not a valid name.",
            ),
            (
                "rule(impl, exec_groups = [])",
                "in call to rule(), parameter 'exec_groups' got value of type 'list', want 'dict or NoneType'",
            ),
            (
                "rule(impl, exec_compatible_with = [1])",
                "at index 0 of exec_compatible_with, got element of type int, want string",
            ),
            (
                "rule(impl, fragments = 'cpp')",
                "in call to rule(), parameter 'fragments' got value of type 'string', want 'sequence'",
            ),
            (
                "rule(impl, outputs = {'a': 1})",
                "got dict<string, int> for 'implicit outputs of the rule class', want dict<string, string>",
            ),
            (
                "rule(impl, outputs = 1)",
                "in call to rule(), parameter 'outputs' got value of type 'int', want 'dict, NoneType, or function'",
            ),
            (
                "rule(impl, extendable = 1)",
                "in call to rule(), parameter 'extendable' got value of type 'int', want 'bool, Label, string, or NoneType'",
            ),
            (
                "rule(impl, initializer = 1)",
                "parameter 'initializer' got value of type 'int', want 'function'",
            ),
            (
                "rule(impl, parent = 1)",
                "Parent needs to be a Starlark rule, was int",
            ),
            (
                "rule(impl, dependency_resolution_rule = True, toolchains = ['//a:b'])",
                "Rules that can be required for materializers cannot depend on toolchains",
            ),
            (
                "rule(impl, dependency_resolution_rule = True, attrs = {'d': attr.label(aspects = [a])})",
                "Rules that can be required for materializes cannot propagate aspects",
            ),
            (
                "rule(impl, attrs = {'late': attr.label(default = fake_configuration_field())})",
                "When an attribute value is a function, the attribute must be private (i.e. start with '_'). Found 'late'",
            ),
            (
                "rule(impl, provides = [provider()])",
                "Providers should be top-level values in extension files that define them.",
            ),
        ] {
            fails(&format!("{IMPL}a = aspect(aimpl)\nx = {call}\n"), expected);
        }
        fails(
            &format!(
                "{IMPL}parent = rule(impl)\nx = rule(impl, parent = parent, executable = True)\n"
            ),
            "Omit executable parameter when extending rules.",
        );
        fails(
            &format!("{IMPL}parent = rule(impl)\nx = rule(impl, parent = parent)\n"),
            "rule(parent=...) rule extension is not supported by the native profile",
        );
        fails(
            &format!("{IMPL}def make():\n    return rule(impl)\nx = rule(impl, parent = make())\n"),
            "Please export the parent rule before extending it.",
        );
        fails(
            &format!("{IMPL}x_test = rule(impl)\n"),
            "Invalid rule class name 'x_test', test rule class names must end with '_test' and other rule classes must not",
        );
    }

    #[test]
    fn dependency_attribute_errors_match_starlark_attr_module() {
        for (call, expected) in [
            (
                "attr.label(executable = True)",
                "cfg parameter is mandatory when executable=True is provided. Please see https://bazel.build/extending/rules#configurations for more details.",
            ),
            (
                "attr.label(allow_files = True, allow_single_file = True)",
                "Cannot specify both allow_files and allow_single_file",
            ),
            (
                "attr.label(allow_single_file = 1)",
                "allow_single_file should be a boolean or a string list",
            ),
            (
                "attr.label_list(allow_rules = [1])",
                "at index 0 of allowed rule classes for attribute definition, got element of type int, want string",
            ),
            (
                "attr.label_list(aspects = [1])",
                "at index 0 of aspects, got element of type int, want Aspect",
            ),
            (
                "attr.label_list(aspects = [aspect(aimpl)])",
                "Aspects should be top-level values in extension files that define them.",
            ),
            (
                "attr.label(flags = [1])",
                "at index 0 of flags, got element of type int, want string",
            ),
            (
                "attr.label_list(flags = None)",
                "in call to label_list(), parameter 'flags' got value of type 'NoneType', want 'sequence'",
            ),
            (
                "attr.int_list(default = ['a'])",
                "expected value of type 'int' for element 0 of parameter 'default' of attribute '', but got \"a\" (string)",
            ),
            (
                "attr.label_keyed_string_dict(default = {'//a:b': 1})",
                "expected value of type 'string' for dict value element, but got 1 (int)",
            ),
            (
                "attr.label_keyed_string_dict(default = {'//a:b': 'x', '@@//a:b': 'y'})",
                "duplicate labels in parameter 'default' of attribute 'label_keyed_string_dict': //a:b (as [\"//a:b\", \"@@//a:b\"])",
            ),
            (
                "attr.string(values = 'a')",
                "in call to string(), parameter 'values' got value of type 'string', want 'sequence'",
            ),
        ] {
            fails(&format!("{IMPL}x = {call}\n"), expected);
        }
        // Accepted shapes, including Bazel's host alias and None/False forms.
        passes(&format!(
            "{IMPL}a = attr.label(cfg = 'host', allow_files = False)\n\
             b = attr.label_list(cfg = 'target', allow_files = None)\n\
             c = attr.output_list(allow_empty = False)\n\
             d = attr.string_list(allow_empty = False, default = ['x'])\n\
             e = attr.label(default = None, for_dependency_resolution = True, skip_validations = True)\n"
        ));
    }

    fn evaluate_package(defs: &str, build: &str) -> Result<crate::package::Package, String> {
        let root = tempfile::Builder::new()
            .prefix("load-declarations-")
            .tempdir_in("/tmp/opencode")
            .unwrap();
        std::fs::write(root.path().join("defs.bzl"), defs).unwrap();
        std::fs::write(root.path().join("BUILD.bazel"), build).unwrap();
        crate::package::evaluate(root.path(), "")
    }

    fn target<'a>(package: &'a crate::package::Package, name: &str) -> &'a crate::package::Rule {
        &package[&crate::label::Label::parse_canonical(&format!("//:{name}")).unwrap()]
    }

    #[test]
    fn initializers_transform_build_arguments_like_bazel() {
        let defs = "def impl(ctx):\n    return []\n\
def init(name, srcs = [], linkstatic = None, **kwargs):\n    (type(srcs[0]) == 'Label') or fail('labels are lifted: %r' % srcs)\n    return {'linkstatic': True if linkstatic == None else linkstatic, 'srcs': srcs + ['extra.cc'], 'copts': ['-O2']}\n\
r = rule(impl, initializer = init, attrs = {'srcs': attr.label_list(allow_files = True), 'linkstatic': attr.bool(), 'copts': attr.string_list()})\n\
def rename(name, **kwargs):\n    return {'name': name + '_x'}\n\
renamer = rule(impl, initializer = rename)\n\
def tags(name, **kwargs):\n    return {'tags': ['x']}\n\
tagger = rule(impl, initializer = tags)\n\
def private(name, **kwargs):\n    return {'_hidden': 1}\n\
privater = rule(impl, initializer = private, attrs = {'_hidden': attr.int()})\n\
def none(name, **kwargs):\n    return None\n\
noop = rule(impl, initializer = none, attrs = {'v': attr.string(default = 'd')})\n";
        let package = evaluate_package(
            defs,
            "load(':defs.bzl', 'noop', 'r')\nr(name = 'a', srcs = ['a.cc'])\nnoop(name = 'n')\n",
        )
        .unwrap();
        let custom = target(&package, "a").custom.as_ref().unwrap();
        assert_eq!(custom.attrs["linkstatic"], AttrValue::Bool(true));
        assert_eq!(
            custom.attrs["srcs"],
            AttrValue::Labels(vec!["//:a.cc".to_owned(), "//:extra.cc".to_owned()])
        );
        assert_eq!(
            custom.attrs["copts"],
            AttrValue::Strings(vec!["-O2".to_owned()])
        );
        assert_eq!(
            target(&package, "n").custom.as_ref().unwrap().attrs["v"],
            AttrValue::String("d".to_owned())
        );
        for (call, expected) in [
            (
                "renamer(name = 'a')",
                "Initializer can't change the name of the target",
            ),
            (
                "tagger(name = 'a')",
                "Initializer can only set Starlark defined attributes, not 'tags'",
            ),
            (
                "privater(name = 'a')",
                "file '//:defs.bzl' cannot use private API",
            ),
        ] {
            let error = evaluate_package(
                defs,
                &format!("load(':defs.bzl', 'privater', 'renamer', 'tagger')\n{call}\n"),
            )
            .unwrap_err();
            assert!(error.contains(expected), "{call}: {error}");
        }
    }

    #[test]
    fn build_time_values_select_and_implicit_outputs_are_retained() {
        let defs = "def impl(ctx):\n    return []\n\
r = rule(impl, attrs = {'deps': attr.label_list(), 'mode': attr.string(values = ['a', 'b']), 'out': attr.output()}, outputs = {'bin': '%{name}.bin'})\n\
def m(name):\n    r(name = name, deps = [':x'] + select({':cond': [':y'], '//conditions:default': None}), mode = select({'//conditions:default': 'a'}))\n";
        let package = evaluate_package(
            defs,
            "load(':defs.bzl', 'm', 'r')\nm(name = 'sel')\nr(name = 'plain', mode = 'b', out = 'plain.o')\n",
        )
        .unwrap();
        let sel = target(&package, "sel");
        assert_eq!(sel.outs, ["sel.bin"]);
        let custom = sel.custom.as_ref().unwrap();
        assert_eq!(
            custom.declaration.configurable["deps"],
            [
                ConfigurablePart::Plain(AttrValue::Labels(vec!["//:x".to_owned()])),
                ConfigurablePart::Select {
                    branches: vec![
                        (
                            "//:cond".to_owned(),
                            Some(AttrValue::Labels(vec!["//:y".to_owned()]))
                        ),
                        ("//conditions:default".to_owned(), None),
                    ],
                    no_match_error: String::new(),
                },
            ]
        );
        // Retained for configured resolution: no load-time default value and
        // no load-time dependency; key and branch labels are assumed inputs.
        assert!(!custom.attrs.contains_key("deps") && !sel.srcs.contains(&"//:y".to_owned()));
        assert_eq!(
            custom
                .declaration
                .configurable_labels
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>(),
            ["//:x", "//:cond", "//:y"]
        );
        let plain = target(&package, "plain");
        assert_eq!(plain.outs, ["plain.o", "plain.bin"]);
        for (call, expected) in [
            (
                "r(name = 'bad', mode = 'c')",
                "//:bad: invalid value in 'mode' attribute: has to be one of 'a' or 'b' instead of 'c'",
            ),
            (
                "r(name = 'bad', mode = select({'//conditions:default': 'z'}))",
                "invalid value in 'mode' attribute: has to be one of 'a' or 'b' instead of 'z'",
            ),
            (
                "r(name = 'bad', out = select({'//conditions:default': 'o'}))",
                "//:bad: attribute \"out\" is not configurable",
            ),
            (
                "r(name = 'bad', deps = select({'//conditions:default': 1}))",
                "//:bad: expected value of type 'list(label)' for each branch in select expression of attribute 'deps' of 'r' (including '//conditions:default'), but got 1 (int)",
            ),
        ] {
            let error =
                evaluate_package(defs, &format!("load(':defs.bzl', 'r')\n{call}\n")).unwrap_err();
            assert!(error.contains(expected), "{call}: {error}");
        }
        let error = evaluate_package(
            "def impl(ctx):\n    return []\nr = rule(impl, outputs = {'x': '%{name}.%{ext}'})\n",
            "load(':defs.bzl', 'r')\nr(name = 'x')\n",
        )
        .unwrap_err();
        assert!(
            error.contains("uses placeholders other than %{name}"),
            "{error}"
        );
        let error = evaluate_package(
            "def impl(ctx):\n    return []\ndef outs(name):\n    return {}\nr = rule(impl, outputs = outs)\n",
            "load(':defs.bzl', 'r')\nr(name = 'x')\n",
        )
        .unwrap_err();
        assert!(
            error.contains("implicit outputs functions are not supported"),
            "{error}"
        );
    }

    #[test]
    fn allowed_values_check_only_supplied_values_and_select_branches() {
        let defs = "def impl(ctx):\n    return []\n\
r = rule(impl, attrs = {'mode': attr.string(values = ['a', 'b']), 'n': attr.int(values = [1, 2])})\n\
def init(name, **kwargs):\n    return {'mode': 'z'}\n\
i = rule(impl, initializer = init, attrs = {'mode': attr.string(values = ['a', 'b'])})\n\
s = rule(impl, build_setting = config.string())\n";
        // The '' default is never checked (AttributeProvider.java:288-290).
        evaluate_package(defs, "load(':defs.bzl', 'r')\nr(name = 'ok')\n").unwrap();
        for (call, expected) in [
            (
                "r(name = 'bad', n = 3)",
                "//:bad: invalid value in 'n' attribute: has to be one of '1' or '2' instead of '3'",
            ),
            (
                "i(name = 'bad')",
                "//:bad: invalid value in 'mode' attribute: has to be one of 'a' or 'b' instead of 'z'",
            ),
            (
                "s(name = 'bad', build_setting_default = select({'//conditions:default': 'x'}))",
                "//:bad: attribute \"build_setting_default\" is not configurable",
            ),
        ] {
            let error =
                evaluate_package(defs, &format!("load(':defs.bzl', 'i', 'r', 's')\n{call}\n"))
                    .unwrap_err();
            assert!(error.contains(expected), "{call}: {error}");
        }
    }

    #[test]
    fn rule_and_attribute_reprs_match_bazel() {
        passes(&format!(
            "{IMPL}r = rule(impl)\n\
             (type(r) == 'rule' and repr(r) == '<rule r>' and str(r) == '<rule r>') or fail(repr(r))\n\
             (repr(rule(impl)) == '<rule>') or fail('unexported')\n\
             a = attr.label_keyed_string_dict()\n\
             (type(a) == 'Attribute' and repr(a) == '<attr.label_keyed_string_dict>') or fail(repr(a))\n\
             (repr(attr.int_list()) == '<attr.int_list>' and repr(attr.string()) == '<attr.string>') or fail('kinds')\n"
        ));
    }

    #[test]
    fn provides_skips_builtin_providers_like_check_declared_providers() {
        let package = evaluate_package(
            "def impl(ctx):\n    return []\nr = rule(impl, provides = [DefaultInfo, OutputGroupInfo])\n",
            "load(':defs.bzl', 'r')\nr(name = 'x')\n",
        )
        .unwrap();
        crate::analysis::analyze(target(&package, "x"), &Default::default()).unwrap();
    }

    #[test]
    fn analysis_rejects_exercised_features_and_runs_unexercised_declarations() {
        let defs = "I = provider()\n\
def impl(ctx):\n    out = ctx.actions.declare_file(ctx.label.name)\n    ctx.actions.write(out, str(ctx.attr.counts))\n    return [DefaultInfo(files = depset([out])), I()]\n\
def bare(ctx):\n    return []\n\
plain = rule(impl, attrs = {'counts': attr.int_list(), 'tools': attr.label_list(cfg = 'exec'), 'late_ok': attr.string(values = ['x', ''])}, fragments = ['cpp'], provides = [I])\n\
missing = rule(bare, provides = [I])\n\
exe = rule(impl, executable = True, attrs = {'counts': attr.int_list()})\n\
tool = rule(impl, attrs = {'counts': attr.int_list(), '_cc': attr.label(default = '//:dep', cfg = config.none())})\n\
nonempty = rule(impl, attrs = {'counts': attr.int_list(allow_empty = False)})\n";
        let package = evaluate_package(
            defs,
            "load(':defs.bzl', 'exe', 'missing', 'nonempty', 'plain', 'tool')\nplain(name = 'ok', counts = [1, 2])\nmissing(name = 'missing')\nexe(name = 'exe')\ntool(name = 'tool')\nnonempty(name = 'empty')\n",
        )
        .unwrap();
        let analyzed =
            crate::analysis::analyze(target(&package, "ok"), &Default::default()).unwrap();
        assert_eq!(analyzed.actions.len(), 1);
        for (name, expected) in [
            (
                "missing",
                "rule advertised the 'I' provider, but this provider was not among those returned",
            ),
            (
                "exe",
                "The rule 'exe' is executable. It needs to create an executable File and pass it as the 'executable' parameter to the DefaultInfo it returns.",
            ),
            (
                "tool",
                "attribute '_cc' cfg=config.none() (configuration transition)",
            ),
            (
                "empty",
                "in counts attribute of nonempty rule //:empty: attribute must be non empty",
            ),
        ] {
            let error =
                crate::analysis::analyze(target(&package, name), &Default::default()).unwrap_err();
            assert!(error.contains(expected), "{name}: {error}");
        }
    }

    #[test]
    fn label_dict_attributes_declare_like_starlark_attr_module() {
        let module = passes(&format!(
            "{IMPL}a = attr.string_keyed_label_dict(default = {{'k': ':x', 'l': Label('//other:y')}}, allow_files = True, providers = [P], configurable = False)\n\
             b = attr.label_list_dict(False, default = {{'k': [':x', '@@//z:z'], 'e': ()}}, skip_validations = True, flags = ['F'], cfg = 'exec')\n\
             c = attr.string_keyed_label_dict()\n\
             d = attr.label_list_dict()\n\
             (type(a) == 'Attribute' and repr(a) == '<attr.string_keyed_label_dict>' and repr(b) == '<attr.label_list_dict>') or fail(repr(a))\n"
        ));
        let spec = |name: &str| {
            crate::custom_rule::attribute_spec(module.get(name).unwrap().value()).unwrap()
        };
        let a = spec("a");
        assert_eq!(
            a.default,
            AttrValue::StringKeyedLabelDict(vec![
                ("k".to_owned(), "//pkg:x".to_owned()),
                ("l".to_owned(), "//other:y".to_owned()),
            ])
        );
        assert!(a.allow_files && a.declaration.configurable_set);
        assert_eq!(a.required_providers.len(), 1);
        let b = spec("b");
        assert_eq!(
            b.default,
            AttrValue::LabelListDict(vec![
                (
                    "k".to_owned(),
                    vec!["//pkg:x".to_owned(), "//z:z".to_owned()]
                ),
                ("e".to_owned(), vec![]),
            ])
        );
        assert!(b.non_empty && b.declaration.skip_validations);
        assert_eq!(b.declaration.cfg, Cfg::Exec(None));
        assert_eq!(b.declaration.flags, ["F"]);
        assert_eq!(spec("c").default, AttrValue::StringKeyedLabelDict(vec![]));
        assert_eq!(spec("d").default, AttrValue::LabelListDict(vec![]));
        for (expression, expected) in [
            (
                "attr.string_keyed_label_dict(default = None)",
                "in call to string_keyed_label_dict(), parameter 'default' got value of type 'NoneType', want 'dict or function'",
            ),
            (
                "attr.label_list_dict(default = lambda name: {})",
                "in call to label_list_dict(), parameter 'default' got value of type 'function', want 'dict'",
            ),
            (
                "attr.string_keyed_label_dict(default = {'k': 1})",
                "expected value of type 'string' for dict value element, but got 1 (int)",
            ),
            (
                "attr.string_keyed_label_dict(default = {1: '//a:b'})",
                "expected value of type 'string' for dict key element, but got 1 (int)",
            ),
            (
                "attr.label_list_dict(default = {'k': '//a:b'})",
                "expected value of type 'list(label)' for dict value element, but got \"//a:b\" (string)",
            ),
            (
                "attr.label_list_dict(default = {'k': [1]})",
                "expected value of type 'string' for element 0 of dict value element, but got 1 (int)",
            ),
            (
                "attr.string_keyed_label_dict(skip_validations = True)",
                "skip_validations",
            ),
            ("attr.label_list_dict(executable = True)", "executable"),
            (
                "attr.string_keyed_label_dict(default = lambda name: {})",
                "computed (function) defaults of attr.string_keyed_label_dict are not supported by the native profile",
            ),
            (
                "attr.label_list_dict(allow_files = 1)",
                "in call to label_list_dict(), parameter 'allow_files' got value of type 'int', want 'bool, sequence, or NoneType'",
            ),
        ] {
            fails(&format!("{IMPL}x = {expression}\n"), expected);
        }
    }

    #[test]
    fn label_dict_values_bind_select_and_check_duplicates_at_loading() {
        let defs = "def impl(ctx):\n    return []\n\
r = rule(impl, attrs = {'m': attr.string_keyed_label_dict(allow_files = True), 'l': attr.label_list_dict(allow_files = True)})\n";
        let package = evaluate_package(
            defs,
            "load(':defs.bzl', 'r')\nconfig_setting(name = 'c', values = {'compilation_mode': 'opt'})\n\
r(name = 'a', m = {'x': 'a.txt', 'y': '//other:b'}, l = select({':c': {'k': ['s.txt']}, '//conditions:default': {'k': ['t.txt'], 'j': []}}))\n",
        )
        .unwrap();
        let custom = target(&package, "a").custom.as_ref().unwrap();
        assert_eq!(
            custom.attrs["m"],
            AttrValue::StringKeyedLabelDict(vec![
                ("x".to_owned(), "//:a.txt".to_owned()),
                ("y".to_owned(), "//other:b".to_owned()),
            ])
        );
        assert!(custom.declaration.configurable.contains_key("l"));
        let labels = custom
            .declaration
            .configurable_labels
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>();
        for expected in ["//:c", "//:s.txt", "//:t.txt"] {
            assert!(labels.iter().any(|label| label == expected), "{labels:?}");
        }
        for (call, expected) in [
            (
                "r(name = 'd', l = {'k': ['x', ':x'], 'j': ['y', 'y']})",
                "Label '//:x' is duplicated in the 'l' attribute of rule 'd'",
            ),
            (
                "r(name = 'd', l = {'p': ['//:a']} | select({'//conditions:default': {'q': ['//:b', '//:b']}}))",
                "Label '//:b' is duplicated in the 'l' attribute of rule 'd'",
            ),
            (
                "r(name = 'd', m = select({'//conditions:default': {'k': 1}}))",
                "expected value of type 'string' for dict value element, but got 1 (int)",
            ),
        ] {
            let error =
                evaluate_package(defs, &format!("load(':defs.bzl', 'r')\n{call}\n")).unwrap_err();
            assert!(error.contains(expected), "{call}: {error}");
        }
        // Duplicate labels across different keys are allowed (each value list
        // is checked independently, AggregatingAttributeMapper.java:252-264).
        evaluate_package(
            defs,
            "load(':defs.bzl', 'r')\nr(name = 'ok', l = {'k': ['x'], 'j': ['x']}, m = {'a': 'x', 'b': 'x'})\n",
        )
        .unwrap();
    }

    /// Visible attributes of a plain Starlark rule's computed default: the
    /// nonconfigurable built-ins (genuine capture, B4/cs_t_unset).
    const AVAILABLE: &str = "Available attributes: $config_dependencies, compatible_with, exec_compatible_with, exec_group_compatible_with, generator_function, generator_location, generator_name, name, restricted_to, tags, transitive_configs, visibility";

    #[test]
    fn computed_defaults_evaluate_with_other_attribute_values_at_loading() {
        let defs = "def impl(ctx):\n    return []\n\
def parser(name, tags):\n    if '__DONT_DEPEND_ON_DEF_PARSER__' in tags or 'def_parser' in name:\n        return None\n    return Label('//tools:def_parser')\n\
def extra(srcs, flag, name):\n    return [Label('//tools:' + s) for s in srcs] if flag else None\n\
r = rule(impl, attrs = {'srcs': attr.string_list(), 'flag': attr.bool(default = True), '_def_parser': attr.label(default = parser, allow_single_file = True, cfg = 'exec'), '_extra': attr.label_list(default = extra)})\n\
(repr(attr.label(default = parser)) == '<attr.label>') or fail('repr')\n\
def bad(name):\n    return '//tools:x'\n\
def missing(nope):\n    return None\n\
def private(_def_parser):\n    return None\n\
def unset(x):\n    return None\n\
def chained(_def_parser):\n    return None\n\
def fails_(name):\n    fail('boom')\n\
def strings(name):\n    return [name]\n\
def tupled(name):\n    return (Label('//tools:t'),)\n\
def selected(srcs):\n    return None\n\
e_bad = rule(impl, attrs = {'_p': attr.label(default = bad)})\n\
e_missing = rule(impl, attrs = {'_p': attr.label(default = missing)})\n\
e_private = rule(impl, attrs = {'_p': attr.label(default = private), '_def_parser': attr.label()})\n\
e_unset = rule(impl, attrs = {'_p': attr.label(default = unset), 'x': attr.label()})\n\
e_chained = rule(impl, attrs = {'_p': attr.label(default = unset), 'x': attr.label(default = '//tools:x')})\n\
e_fails = rule(impl, attrs = {'_p': attr.label(default = fails_)})\n\
e_strings = rule(impl, attrs = {'_p': attr.label_list(default = strings)})\n\
ok_tuple = rule(impl, attrs = {'_p': attr.label_list(default = tupled)})\n\
e_selected = rule(impl, attrs = {'srcs': attr.string_list(), '_p': attr.label(default = selected)})\n\
e_builtin = rule(impl, attrs = {'_p': attr.label(default = lambda testonly: None)})\n\
e_deprecation = rule(impl, attrs = {'_p': attr.label(default = lambda name, deprecation: None)})\n\
e_metadata = rule(impl, attrs = {'_p': attr.label(default = lambda package_metadata: None)})\n\
e_alt = rule(impl, attrs = {'_p': attr.label(default = lambda applicable_licenses: None)})\n\
e_timeout_test = rule(impl, test = True, attrs = {'_p': attr.label(default = lambda timeout: None)})\n\
e_vis_unset = rule(impl, attrs = {'_p': attr.label(default = lambda visibility: None)})\n\
e_with_s = rule(impl, attrs = {'s': attr.string(), 'n': attr.int(), '_p': attr.label(default = lambda s, n, features, testonly: None)})\n\
def outs(o, os, visibility, features, exec_properties, size):\n    (str(o) == '@@//:out.txt' and type(o) == 'Label') or fail('output %r' % o)\n    ([str(x) for x in os] == ['@@//:a.txt', '@@//:sub/b.txt']) or fail('outputs %r' % os)\n    ([str(x) for x in visibility] == ['@@//visibility:public']) or fail('vis %r' % visibility)\n    (features == [] and exec_properties == {} and size == 'medium') or fail('defaults')\n    return Label('//tools:outs_' + str(len(visibility)))\n\
outs_test = rule(impl, test = True, attrs = {'o': attr.output(), 'os': attr.output_list(), '_p': attr.label(default = outs)})\n";
        let package = evaluate_package(
            defs,
            "load(':defs.bzl', 'ok_tuple', 'r')\n\
r(name = 'a', srcs = ['x', 'y'])\n\
r(name = 'my_def_parser', flag = False)\n\
r(name = 'tagged', tags = ['__DONT_DEPEND_ON_DEF_PARSER__'])\n\
ok_tuple(name = 't')\n",
        )
        .unwrap();
        let attrs = |name: &str| {
            target(&package, name)
                .custom
                .as_ref()
                .unwrap()
                .attrs
                .clone()
        };
        assert_eq!(
            attrs("a")["_def_parser"],
            AttrValue::Label(Some("//tools:def_parser".to_owned()))
        );
        assert_eq!(
            attrs("a")["_extra"],
            AttrValue::Labels(vec!["//tools:x".to_owned(), "//tools:y".to_owned()])
        );
        assert_eq!(
            attrs("my_def_parser")["_def_parser"],
            AttrValue::Label(None)
        );
        assert_eq!(attrs("my_def_parser")["_extra"], AttrValue::Labels(vec![]));
        assert_eq!(attrs("tagged")["_def_parser"], AttrValue::Label(None));
        assert_eq!(
            attrs("t")["_p"],
            AttrValue::Labels(vec!["//tools:t".to_owned()])
        );
        // The computed value is an ordinary dependency edge.
        assert!(
            target(&package, "a")
                .srcs
                .iter()
                .any(|src| src == "//tools:def_parser")
        );
        let prefix = "Cannot compute default value of attribute '_p' in rule '//:e': ";
        for (rule, expected) in [
            (
                "e_bad",
                format!("{prefix}expected 'label', but got 'string'"),
            ),
            (
                "e_missing",
                format!("{prefix}No such attribute nope in rule //:e"),
            ),
            (
                "e_private",
                format!("{prefix}No such attribute _def_parser in rule //:e"),
            ),
            (
                "e_unset",
                format!("{prefix}No such regular (non computed) attribute 'x'."),
            ),
            ("e_fails", prefix.to_owned()),
            (
                "e_strings",
                format!("{prefix}expected 'label', but got 'string'"),
            ),
            (
                "e_builtin",
                format!("{prefix}No such regular (non computed) attribute 'testonly'.\n{AVAILABLE}"),
            ),
            (
                "e_deprecation",
                format!("{prefix}No such regular (non computed) attribute 'deprecation'.\n{AVAILABLE}"),
            ),
            (
                "e_metadata",
                format!("{prefix}No such regular (non computed) attribute 'package_metadata'.\n{AVAILABLE}"),
            ),
            (
                "e_alt",
                format!("{prefix}No such attribute applicable_licenses in rule //:e"),
            ),
            (
                "e_timeout_test",
                "Cannot compute default value of attribute '_p' in rule '//:e_test': No such regular (non computed) attribute 'timeout'.\nAvailable attributes: $config_dependencies, $is_executable, compatible_with, exec_compatible_with, exec_group_compatible_with, flaky, generator_function, generator_location, generator_name, local, name, restricted_to, size, tags, transitive_configs, visibility".to_owned(),
            ),
            (
                "e_vis_unset",
                "reads the unset 'visibility' attribute (genuine Bazel 9.3 crashes on it), which is not supported by the native profile".to_owned(),
            ),
            (
                "e_with_s",
                format!("{prefix}No such regular (non computed) attribute 'testonly'.\nAvailable attributes: $config_dependencies, compatible_with, exec_compatible_with, exec_group_compatible_with, features, generator_function, generator_location, generator_name, n, name, restricted_to, s, tags, transitive_configs, visibility"),
            ),
        ] {
            let name = if rule.ends_with("_test") { "e_test" } else { "e" };
            let error = evaluate_package(
                defs,
                &format!("load(':defs.bzl', '{rule}')\n{rule}(name = '{name}')\n"),
            )
            .unwrap_err();
            assert!(error.contains(&expected), "{rule}: {error}");
        }
        // Explicitly set built-in computed defaults are never regular values.
        for call in [
            "e_builtin(name = 'e', testonly = True)",
            "e_deprecation(name = 'e', deprecation = 'old')",
        ] {
            let error = evaluate_package(
                defs,
                &format!("load(':defs.bzl', 'e_builtin', 'e_deprecation')\n{call}\n"),
            )
            .unwrap_err();
            assert!(
                error.contains("No such regular (non computed) attribute"),
                "{call}: {error}"
            );
        }
        // Output arguments are Labels in the target's package; visibility is
        // the declared label list; plain built-in defaults.
        let package = evaluate_package(
            defs,
            "load(':defs.bzl', 'outs_test')\nouts_test(name = 'o_test', o = 'out.txt', os = ['a.txt', 'sub/b.txt'], visibility = ['//visibility:public'])\n",
        )
        .unwrap();
        assert_eq!(
            target(&package, "o_test").custom.as_ref().unwrap().attrs["_p"],
            AttrValue::Label(Some("//tools:outs_1".to_owned()))
        );
        // fail() inside the function: the message without the call stack.
        let error = evaluate_package(defs, "load(':defs.bzl', 'e_fails')\ne_fails(name = 'e')\n")
            .unwrap_err();
        assert!(error.contains(&format!("{prefix}boom\n")), "{error}");
        evaluate_package(
            defs,
            "load(':defs.bzl', 'e_chained')\ne_chained(name = 'e')\n",
        )
        .unwrap();
        let error = evaluate_package(
            defs,
            "load(':defs.bzl', 'e_selected')\nconfig_setting(name = 'c', values = {'compilation_mode': 'opt'})\ne_selected(name = 'e', srcs = select({':c': ['a'], '//conditions:default': []}))\n",
        )
        .unwrap_err();
        assert!(
            error.contains("depends on the configurable attribute 'srcs'")
                && error.contains("not supported by the native profile"),
            "{error}"
        );
        for (expression, expected) in [
            (
                "r = rule(impl, attrs = {'p': attr.label(default = lambda name: None)})",
                "When an attribute value is a function, the attribute must be private (i.e. start with '_'). Found 'p'",
            ),
            (
                "a = aspect(aimpl, attrs = {'_p': attr.label(default = lambda name: None)})",
                "Aspect attribute '_p' (label) with computed default value is unsupported.",
            ),
            (
                "a = aspect(aimpl, attrs = {'_p': attr.label_list(default = lambda name: None)})",
                "Aspect attribute '_p' (list(label)) with computed default value is unsupported.",
            ),
            (
                "r = rule(impl, attrs = {'v': attr.label_list_dict(configurable = False)})",
                "attribute 'v' has the 'configurable' argument set, which is not allowed in rule definitions",
            ),
            (
                "r = rule(impl, attrs = {'_v': attr.string_keyed_label_dict(configurable = True)})",
                "attribute '$v' has the 'configurable' argument set, which is not allowed in rule definitions",
            ),
            (
                "a = aspect(aimpl, attrs = {'_v': attr.label_list_dict(configurable = False)})",
                "attribute '$v' has the 'configurable' argument set, which is not allowed in aspect definitions",
            ),
        ] {
            fails(&format!("{IMPL}{expression}\n"), expected);
        }
    }

    #[test]
    fn analysis_test_transitions_are_retained_for_analysis_test_rules_only() {
        let module = passes(&format!(
            "{IMPL}t = analysis_test_transition(settings = {{'//command_line_option:compilation_mode': 'opt', '//command_line_option:experimental_x': True, '//pkg:flag': 1}})\n\
             (type(t) == 'transition' and repr(t) == '<analysis_test_transition object>') or fail(repr(t))\n\
             (repr(AnalysisTestResultInfo) == '<function AnalysisTestResultInfo>' and type(AnalysisFailureInfo) == 'Provider') or fail('providers')\n\
             r_test = rule(impl, analysis_test = True, attrs = {{'target_under_test': attr.label(cfg = t, mandatory = True)}})\n"
        ));
        let declaration =
            crate::custom_rule::rule_declaration(module.get("r_test").unwrap().value()).unwrap();
        let cfg = &declaration.schema["target_under_test"].declaration.cfg;
        assert!(cfg.is_analysis_test(), "{cfg:?}");
        assert_eq!(
            cfg.describe(),
            "analysis_test_transition from //pkg:defs.bzl"
        );
        for (expression, expected) in [
            (
                "rule(impl, attrs = {'d': attr.label(cfg = analysis_test_transition(settings = {}))})",
                "Only rule definitions with analysis_test=True may have attributes with analysis_test_transition transitions",
            ),
            (
                "analysis_test_transition(settings = [])",
                "in call to analysis_test_transition(), parameter 'settings' got value of type 'list', want 'dict'",
            ),
            (
                "analysis_test_transition(settings = {1: 2})",
                "got dict<int, int> for 'changed_settings dict', want dict<string, unknown>",
            ),
            (
                "analysis_test_transition(settings = {'//pkg:a': 1, '@@//pkg:a': 2})",
                "Transition declares duplicate build setting '//pkg:a' in OUTPUTS",
            ),
            (
                "analysis_test_transition(settings = {'not a label': 1})",
                "invalid transition output 'not a label'",
            ),
            (
                "analysis_test_transition({})",
                "analysis_test_transition() got unexpected positional argument",
            ),
            (
                "analysis_test_transition()",
                "analysis_test_transition() missing 1 required named argument: settings",
            ),
            (
                "AnalysisTestResultInfo(success = True, message = '')",
                "AnalysisTestResultInfo construction is not implemented in the native profile",
            ),
            ("AnalysisFailureInfo()", "'Provider' object is not callable"),
        ] {
            fails(&format!("{IMPL}x = {expression}\n"), expected);
        }
        let package = evaluate_package(
            "def impl(ctx):\n    return []\nt = analysis_test_transition(settings = {'//command_line_option:compilation_mode': 'opt'})\nr_test = rule(impl, analysis_test = True, attrs = {'target_under_test': attr.label(cfg = t)})\n",
            "load(':defs.bzl', 'r_test')\nfilegroup(name = 'subject')\nr_test(name = 'check_test', target_under_test = ':subject')\n",
        )
        .unwrap();
        let error = crate::analysis::analyze(target(&package, "check_test"), &Default::default())
            .unwrap_err();
        assert!(error.contains("rule(analysis_test=True)"), "{error}");
    }
}
