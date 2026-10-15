//! Native fallible context properties. Introspection is structural; reads consume
//! a current scope capability. Runtime contexts deliberately cannot be frozen.
use allocative::Allocative;
use anyhow::{anyhow, bail};
use starlark::environment::{Methods, MethodsBuilder};
use starlark::starlark_module;
use starlark::values::{
    Coerce, Freeze, FreezeResult, Freezer, FrozenValue, Heap, NoSerialize, ProvidesStaticType,
    StarlarkPagablePanic, StarlarkValue, Trace, Value, ValueLike, starlark_value,
};
use std::fmt;

use crate::analysis_runtime::ScopeHandle;
use crate::configuration::SettingValue;

use crate::platforms::TargetPlatform;
use crate::toolchain_info::ToolchainContextData;
use std::sync::Arc;

/// The analysis error of an implementation that needs a deferred target
/// platform.
pub use crate::platforms::NEEDS_TARGET_PLATFORM;

#[allow(unsafe_code)]
mod values {
    use super::*;
    #[derive(Debug, ProvidesStaticType, NoSerialize, Allocative, Trace, StarlarkPagablePanic)]
    pub(super) struct RuleCtx<'v> {
        pub backing: Value<'v>,
        #[allocative(skip)]
        #[trace(static)]
        pub scope: ScopeHandle,
        /// The rule's target platform (ctx.target_platform_has_constraint).
        #[allocative(skip)]
        #[trace(static)]
        pub platform: TargetPlatform,
        /// The resolved toolchain context (`ctx.toolchains`); `None` for
        /// rules without toolchain resolution.
        #[allocative(skip)]
        #[trace(static)]
        pub toolchains: Option<Arc<ToolchainContextData>>,
    }
    #[derive(Debug, ProvidesStaticType, NoSerialize, Allocative, Trace, StarlarkPagablePanic)]
    pub(super) struct SettingCtx<'v> {
        pub ctx: RuleCtx<'v>,
        /// The configured value (command line or `build_setting_default`).
        #[allocative(skip)]
        #[trace(static)]
        pub value: SettingValue,
    }
    /// `ctx.outputs` (StarlarkRuleContext.Outputs, analysis/starlark/StarlarkRuleContext.java:353-448).
    /// Unlike the context itself it may be stored in returned values; freezing
    /// happens only after the implementation returned, when Bazel's context is
    /// nullified (StarlarkRuleContext.java:471) and the object is immutable
    /// (`:375-377`, `:586-588`).
    #[derive(
        Debug, ProvidesStaticType, NoSerialize, Allocative, Trace, Coerce, StarlarkPagablePanic,
    )]
    #[repr(C)]
    pub(super) struct CtxOutputsGen<V> {
        pub fields: V,
        /// The rule's canonical label (ruleLabelCanonicalName).
        pub label: String,
        /// Frozen after the implementation returned (Outputs.isImmutable).
        pub immutable: bool,
    }

    use starlark::starlark_complex_value;
    starlark_complex_value!(pub(super) CtxOutputs);

    impl<'v> Freeze for CtxOutputsGen<Value<'v>> {
        type Frozen = CtxOutputsGen<FrozenValue>;

        fn freeze(self, freezer: &Freezer) -> FreezeResult<Self::Frozen> {
            Ok(CtxOutputsGen {
                fields: self.fields.freeze(freezer)?,
                label: self.label,
                immutable: true,
            })
        }
    }
    #[derive(Debug, ProvidesStaticType, NoSerialize, Allocative, Trace, StarlarkPagablePanic)]
    pub(super) struct SubruleCtx<'v> {
        /// The subrule's exported name (StarlarkSubrule.getName).
        #[allocative(skip)]
        #[trace(static)]
        pub name: String,
        pub label: Value<'v>,
        pub actions: Value<'v>,
        #[allocative(skip)]
        #[trace(static)]
        pub scope: ScopeHandle,
    }
}
use values::{CtxOutputsGen, RuleCtx, SettingCtx, SubruleCtx};

/// Outputs.repr (analysis/starlark/StarlarkRuleContext.java:415-439): while
/// mutable `ctx.outputs(name = repr, ...)` sorted by field name; once immutable
/// `ctx.outputs(for //pkg:x)`.
impl<'v, V: ValueLike<'v>> fmt::Display for CtxOutputsGen<V> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.immutable {
            return write!(f, "ctx.outputs(for {})", self.label);
        }
        f.write_str("ctx.outputs(")?;
        if let Some(fields) =
            starlark::values::structs::StructRef::from_value(self.fields.to_value())
        {
            let mut fields: Vec<_> = fields.iter().collect();
            fields.sort_by(|(a, _), (b, _)| a.as_str().cmp(b.as_str()));
            for (index, (name, value)) in fields.into_iter().enumerate() {
                if index != 0 {
                    f.write_str(", ")?;
                }
                write!(f, "{} = {value}", name.as_str())?;
            }
        }
        f.write_str(")")
    }
}

// The Outputs class has no @StarlarkBuiltin, so type() is its simple name.
#[starlark_value(type = "Outputs")]
impl<'v, V: ValueLike<'v>> StarlarkValue<'v> for CtxOutputsGen<V>
where
    Self: starlark::any::ProvidesStaticType<'v>,
{
    /// Once immutable, Bazel's getValue fails with "cannot access outputs of
    /// rule '//pkg:x' outside of its own rule implementation function"
    /// (StarlarkRuleContext.java:395-397,440-447); natively the field is absent.
    fn get_attr(&self, name: &str, heap: Heap<'v>) -> Option<Value<'v>> {
        if self.immutable {
            return None;
        }
        self.fields.to_value().get_attr(name, heap).ok().flatten()
    }

    fn dir_attr(&self) -> Vec<String> {
        self.fields.to_value().dir_attr()
    }
}

/// The field struct of a (mutable or frozen) `ctx.outputs` value.
pub(crate) fn outputs_fields(value: Value<'_>) -> Option<Value<'_>> {
    values::CtxOutputs::from_value(value).map(|outputs| outputs.fields)
}

pub(crate) fn alloc_outputs<'v>(heap: Heap<'v>, fields: Value<'v>, label: &str) -> Value<'v> {
    heap.alloc(CtxOutputsGen {
        fields,
        label: label.to_owned(),
        immutable: false,
    })
}

/// StarlarkRuleContext.repr (analysis/starlark/StarlarkRuleContext.java:590-597)
/// for a rule (not aspect) context; `label` is the canonical form
/// (StarlarkRuleContext.java:197).
pub(crate) fn rule_context_repr(label: &str) -> String {
    format!("<rule context for {label}>")
}

/// A label Value's canonical form; other values (native unit-test stand-ins)
/// print as str().
fn canonical_label(value: Value<'_>) -> String {
    crate::starlark_types::label_canonical_form(value).unwrap_or_else(|| value.to_str())
}

impl RuleCtx<'_> {
    fn label(&self) -> String {
        starlark::values::structs::StructRef::from_value(self.backing)
            .and_then(|fields| {
                fields
                    .iter()
                    .find(|(name, _)| name.as_str() == "label")
                    .map(|(_, value)| canonical_label(value))
            })
            .unwrap_or_default()
    }
}

impl fmt::Display for RuleCtx<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&rule_context_repr(&self.label()))
    }
}
// A build setting's ctx is the same StarlarkRuleContext.
impl fmt::Display for SettingCtx<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.ctx.fmt(f)
    }
}
/// SubruleContext.repr (analysis/starlark/StarlarkSubrule.java:493-501).
pub(crate) fn subrule_context_repr(name: &str, label: &str) -> String {
    format!("<{name} context for {label}>")
}
impl fmt::Display for SubruleCtx<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&subrule_context_repr(
            &self.name,
            &canonical_label(self.label),
        ))
    }
}

starlark::methods_static!(RULE_METHODS = rule_properties);
starlark::methods_static!(SETTING_METHODS = all_setting_properties);
starlark::methods_static!(SUBRULE_METHODS = subrule_properties);

#[starlark_value(type = "ctx")]
impl<'v> StarlarkValue<'v> for RuleCtx<'v> {
    fn get_methods() -> Option<&'static Methods> {
        Some(RULE_METHODS.methods())
    }
}
#[starlark_value(type = "ctx")]
impl<'v> StarlarkValue<'v> for SettingCtx<'v> {
    fn get_methods() -> Option<&'static Methods> {
        Some(SETTING_METHODS.methods())
    }
}
#[starlark_value(type = "subrule_ctx")]
impl<'v> StarlarkValue<'v> for SubruleCtx<'v> {
    fn get_methods() -> Option<&'static Methods> {
        Some(SUBRULE_METHODS.methods())
    }
}

fn field<'v>(this: Value<'v>, name: &str, heap: Heap<'v>) -> anyhow::Result<Value<'v>> {
    let ctx = rule_ctx(this)?;
    ctx.scope.check_current()?;
    ctx.backing
        .get_attr_error(name, heap)
        .map_err(|error| anyhow!(error.to_string()))
}

#[starlark_module]
fn rule_properties(builder: &mut MethodsBuilder) {
    #[starlark(attribute)]
    fn attr<'v>(this: Value<'v>, heap: Heap<'v>) -> anyhow::Result<Value<'v>> {
        field(this, "attr", heap)
    }
    #[starlark(attribute)]
    fn files<'v>(this: Value<'v>, heap: Heap<'v>) -> anyhow::Result<Value<'v>> {
        field(this, "files", heap)
    }
    #[starlark(attribute)]
    fn file<'v>(this: Value<'v>, heap: Heap<'v>) -> anyhow::Result<Value<'v>> {
        field(this, "file", heap)
    }
    #[starlark(attribute)]
    fn outputs<'v>(this: Value<'v>, heap: Heap<'v>) -> anyhow::Result<Value<'v>> {
        field(this, "outputs", heap)
    }
    #[starlark(attribute)]
    fn label<'v>(this: Value<'v>, heap: Heap<'v>) -> anyhow::Result<Value<'v>> {
        field(this, "label", heap)
    }
    #[starlark(attribute)]
    fn actions<'v>(this: Value<'v>, heap: Heap<'v>) -> anyhow::Result<Value<'v>> {
        field(this, "actions", heap)
    }
    /// StarlarkRuleContext.toolchains (analysis/starlark/StarlarkRuleContext.java:877-898).
    #[starlark(attribute)]
    fn toolchains<'v>(this: Value<'v>, heap: Heap<'v>) -> anyhow::Result<Value<'v>> {
        let ctx = rule_ctx(this)?;
        ctx.scope.check_current()?;
        Ok(crate::toolchain_info::alloc_context(
            heap,
            ctx.toolchains.clone(),
        ))
    }
    /// StarlarkRuleContext.targetPlatformHasConstraint
    /// (analysis/starlark/StarlarkRuleContext.java:902-904; RuleContext.java:1144-1150):
    /// whether the target platform's value for the setting is this value.
    fn target_platform_has_constraint<'v>(
        this: Value<'v>,
        #[starlark(require = pos)] constraint_value: Value<'v>,
    ) -> anyhow::Result<bool> {
        let ctx = rule_ctx(this)?;
        ctx.scope.check_current()?;
        let Some(value) = crate::platform_providers::constraint_value(constraint_value) else {
            bail!(
                "in call to target_platform_has_constraint(), parameter 'constraintValue' got value of type '{}', want 'ConstraintValueInfo'",
                constraint_value.get_type()
            );
        };
        match &ctx.platform {
            TargetPlatform::Absent => Ok(false),
            TargetPlatform::Deferred => bail!("{NEEDS_TARGET_PLATFORM}"),
            TargetPlatform::Resolved(platform) => Ok(platform
                .constraints
                .get(&value.setting)
                .is_some_and(|found| found == *value)),
        }
    }
}

fn rule_ctx<'v>(this: Value<'v>) -> anyhow::Result<&'v RuleCtx<'v>> {
    if let Some(ctx) = this.downcast_ref::<RuleCtx>() {
        return Ok(ctx);
    }
    Ok(&this
        .downcast_ref::<SettingCtx>()
        .ok_or_else(|| anyhow!("expected ctx"))?
        .ctx)
}

fn all_setting_properties(builder: &mut MethodsBuilder) {
    rule_properties(builder);
    setting_properties(builder);
}

#[starlark_module]
fn setting_properties(builder: &mut MethodsBuilder) {
    #[starlark(attribute)]
    fn build_setting_value<'v>(this: Value<'v>, heap: Heap<'v>) -> anyhow::Result<Value<'v>> {
        let ctx = this
            .downcast_ref::<SettingCtx>()
            .ok_or_else(|| anyhow!("expected build-setting ctx"))?;
        ctx.ctx.scope.check_current()?;
        // A fresh value per read (mutable list copies).
        match &ctx.value {
            SettingValue::Bool(value) => Ok(heap.alloc(*value)),
            SettingValue::Int(value) => Ok(heap.alloc(*value)),
            SettingValue::String(value) => Ok(heap.alloc(value.as_str())),
            SettingValue::StringList(value) => Ok(heap.alloc(value.clone())),
            SettingValue::Label(_) => bail!("unsupported build-setting value type label"),
        }
    }
}

fn subctx<'v>(this: Value<'v>) -> anyhow::Result<&'v SubruleCtx<'v>> {
    let ctx = this
        .downcast_ref::<SubruleCtx>()
        .ok_or_else(|| anyhow!("expected subrule_ctx"))?;
    ctx.scope.check_current()?;
    Ok(ctx)
}

#[starlark_module]
fn subrule_properties(builder: &mut MethodsBuilder) {
    #[starlark(attribute)]
    fn label<'v>(this: Value<'v>) -> anyhow::Result<Value<'v>> {
        Ok(subctx(this)?.label)
    }
    #[starlark(attribute)]
    fn actions<'v>(this: Value<'v>) -> anyhow::Result<Value<'v>> {
        Ok(subctx(this)?.actions)
    }
    #[starlark(attribute)]
    fn fragments<'v>(this: Value<'v>) -> anyhow::Result<Value<'v>> {
        subctx(this)?;
        bail!("subrule fragments are unsupported")
    }
    #[starlark(attribute)]
    fn toolchains<'v>(this: Value<'v>) -> anyhow::Result<Value<'v>> {
        subctx(this)?;
        bail!("subrule toolchains are unsupported")
    }
}

pub(crate) fn alloc_rule<'v>(
    heap: Heap<'v>,
    backing: Value<'v>,
    scope: ScopeHandle,
    value: Option<SettingValue>,
    platform: TargetPlatform,
    toolchains: Option<Arc<ToolchainContextData>>,
) -> Value<'v> {
    let ctx = RuleCtx {
        backing,
        scope,
        platform,
        toolchains,
    };
    match value {
        Some(value) => heap.alloc_complex_no_freeze(SettingCtx { ctx, value }),
        None => heap.alloc_complex_no_freeze(ctx),
    }
}

pub(crate) fn alloc_subrule<'v>(
    heap: Heap<'v>,
    name: &str,
    label: Value<'v>,
    actions: Value<'v>,
    scope: ScopeHandle,
) -> Value<'v> {
    heap.alloc_complex_no_freeze(SubruleCtx {
        name: name.to_owned(),
        label,
        actions,
        scope,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::analysis_runtime::ScopeState;
    use starlark::environment::{Globals, Module};
    use starlark::eval::Evaluator;
    use starlark::syntax::{AstModule, Dialect};
    use starlark::values::structs::AllocStruct;
    use std::collections::BTreeSet;

    #[test]
    fn native_rule_and_setting_properties_guard_reads_not_introspection() {
        for setting in [false, true] {
            Module::with_temp_heap(|module| {
                let heap = module.heap();
                let owner = ScopeState::new("rule".into(), BTreeSet::from(["S".into()]));
                let backing = heap.alloc(AllocStruct([
                    (
                        "attr",
                        heap.alloc(AllocStruct([("name", heap.alloc("target"))])),
                    ),
                    (
                        "files",
                        heap.alloc(AllocStruct([("srcs", heap.alloc(Vec::<Value>::new()))])),
                    ),
                    (
                        "file",
                        heap.alloc(AllocStruct([("src", Value::new_none())])),
                    ),
                    (
                        "outputs",
                        heap.alloc(AllocStruct([("out", Value::new_none())])),
                    ),
                    ("label", heap.alloc("//:target")),
                    (
                        "actions",
                        crate::analysis_runtime::alloc_actions(heap, owner.root()),
                    ),
                ]));
                let ctx = alloc_rule(
                    heap,
                    backing,
                    owner.root(),
                    setting.then(|| SettingValue::StringList(vec!["seed".into()])),
                    TargetPlatform::Absent,
                    None,
                );
                module.set("ctx", ctx);
                let mut expected = vec![
                    "actions",
                    "attr",
                    "file",
                    "files",
                    "label",
                    "outputs",
                    "target_platform_has_constraint",
                    "toolchains",
                ];
                if setting {
                    expected.push("build_setting_value");
                }
                expected.sort();
                assert_eq!(ctx.dir_attr(), expected);
                for name in &expected {
                    ctx.get_attr_error(name, heap).unwrap();
                }
                let child = owner.enter("S".into(), BTreeSet::new()).unwrap();
                // Methods bind without reading; properties guard the read.
                for name in expected
                    .iter()
                    .filter(|name| **name != "target_platform_has_constraint")
                {
                    let error = ctx.get_attr_error(name, heap).unwrap_err();
                    assert!(
                        error.to_string().contains("scope receiver"),
                        "{name}: {error}"
                    );
                }
                let mut eval = Evaluator::new(&module);
                eval.eval_module(AstModule::parse("ctx.star", "(hasattr(ctx, 'label') and 'label' in dir(ctx) and getattr(ctx, 'missing', 'fallback') == 'fallback') or fail('structure')\n".into(), &Dialect::Standard).unwrap(), &Globals::standard()).unwrap();
                let error = eval
                    .eval_module(
                        AstModule::parse(
                            "ctx.star",
                            "getattr(ctx, 'label', None)".into(),
                            &Dialect::Standard,
                        )
                        .unwrap(),
                        &Globals::standard(),
                    )
                    .unwrap_err();
                assert!(error.to_string().contains("scope receiver"));
                drop(child);
                assert_eq!(
                    ctx.get_attr_error("label", heap).unwrap().unpack_str(),
                    Some("//:target")
                );
                owner.finish();
                for name in expected
                    .iter()
                    .filter(|name| **name != "target_platform_has_constraint")
                {
                    assert!(ctx.get_attr_error(name, heap).is_err());
                }
            });
        }
    }

    #[test]
    fn ordinary_subrule_and_action_runtime_capabilities_reject_freeze() {
        for kind in ["rule", "subrule", "actions"] {
            Module::with_temp_heap(|module| {
                let heap = module.heap();
                let owner = ScopeState::new("rule".into(), BTreeSet::new());
                let actions = crate::analysis_runtime::alloc_actions(heap, owner.root());
                let value = match kind {
                    "rule" => alloc_rule(
                        heap,
                        heap.alloc(AllocStruct([("label", heap.alloc("//:target"))])),
                        owner.root(),
                        None,
                        TargetPlatform::Absent,
                        None,
                    ),
                    "subrule" => {
                        alloc_subrule(heap, "s", heap.alloc("//:target"), actions, owner.root())
                    }
                    _ => actions,
                };
                module.set("capability", value);
                assert!(module.freeze().is_err(), "accepted {kind}");
            });
        }
    }
}
