//! Native fallible context properties. Introspection is structural; reads consume
//! a current scope capability. Runtime contexts deliberately cannot be frozen.
use allocative::Allocative;
use anyhow::{anyhow, bail};
use starlark::environment::{Methods, MethodsBuilder};
use starlark::starlark_module;
use starlark::values::{
    Heap, NoSerialize, ProvidesStaticType, StarlarkPagablePanic, StarlarkValue, Trace, Value,
    ValueLike, starlark_value,
};
use std::fmt;

use crate::analysis_runtime::ScopeHandle;
use crate::configuration::SettingValue;

#[allow(unsafe_code)]
mod values {
    use super::*;
    #[derive(Debug, ProvidesStaticType, NoSerialize, Allocative, Trace, StarlarkPagablePanic)]
    pub(super) struct RuleCtx<'v> {
        pub backing: Value<'v>,
        #[allocative(skip)]
        #[trace(static)]
        pub scope: ScopeHandle,
    }
    #[derive(Debug, ProvidesStaticType, NoSerialize, Allocative, Trace, StarlarkPagablePanic)]
    pub(super) struct SettingCtx<'v> {
        pub ctx: RuleCtx<'v>,
        /// The configured value (command line or `build_setting_default`).
        #[allocative(skip)]
        #[trace(static)]
        pub value: SettingValue,
    }
    #[derive(Debug, ProvidesStaticType, NoSerialize, Allocative, Trace, StarlarkPagablePanic)]
    pub(super) struct SubruleCtx<'v> {
        pub label: Value<'v>,
        pub actions: Value<'v>,
        #[allocative(skip)]
        #[trace(static)]
        pub scope: ScopeHandle,
    }
}
use values::{RuleCtx, SettingCtx, SubruleCtx};

impl fmt::Display for RuleCtx<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("<ctx>")
    }
}
impl fmt::Display for SettingCtx<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("<ctx>")
    }
}
impl fmt::Display for SubruleCtx<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("<subrule_ctx>")
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
    let ctx = if let Some(ctx) = this.downcast_ref::<RuleCtx>() {
        ctx
    } else {
        &this
            .downcast_ref::<SettingCtx>()
            .ok_or_else(|| anyhow!("expected ctx"))?
            .ctx
    };
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
) -> Value<'v> {
    let ctx = RuleCtx { backing, scope };
    match value {
        Some(value) => heap.alloc_complex_no_freeze(SettingCtx { ctx, value }),
        None => heap.alloc_complex_no_freeze(ctx),
    }
}

pub(crate) fn alloc_subrule<'v>(
    heap: Heap<'v>,
    label: Value<'v>,
    actions: Value<'v>,
    scope: ScopeHandle,
) -> Value<'v> {
    heap.alloc_complex_no_freeze(SubruleCtx {
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
                );
                module.set("ctx", ctx);
                let mut expected = vec!["actions", "attr", "file", "files", "label", "outputs"];
                if setting {
                    expected.push("build_setting_value");
                }
                expected.sort();
                assert_eq!(ctx.dir_attr(), expected);
                for name in &expected {
                    ctx.get_attr_error(name, heap).unwrap();
                }
                let child = owner.enter("S".into(), BTreeSet::new()).unwrap();
                for name in &expected {
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
                for name in &expected {
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
                    ),
                    "subrule" => {
                        alloc_subrule(heap, heap.alloc("//:target"), actions, owner.root())
                    }
                    _ => actions,
                };
                module.set("capability", value);
                assert!(module.freeze().is_err(), "accepted {kind}");
            });
        }
    }
}
