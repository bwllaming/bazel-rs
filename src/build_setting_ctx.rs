//! Setting-only native context with independently owned, fresh default snapshots.
use crate::analysis_runtime::ScopeHandle;
use crate::configuration::SettingValue;
use starlark::values::{Heap, Value};

pub(crate) fn wrap<'v>(
    heap: Heap<'v>,
    backing: Value<'v>,
    value: SettingValue,
    scope: ScopeHandle,
    surface: std::sync::Arc<crate::ctx_surface::RuleSurface>,
) -> Value<'v> {
    crate::analysis_ctx::alloc_rule(
        heap,
        backing,
        scope,
        Some(value),
        crate::platforms::TargetPlatform::Absent,
        None,
        None,
        surface,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use starlark::environment::{GlobalsBuilder, LibraryExtension, Module};
    use starlark::eval::Evaluator;
    use starlark::syntax::{AstModule, Dialect};
    use starlark::values::structs::AllocStruct;

    #[test]
    fn value_reads_are_fresh_mutable_copies_independent_of_backing_attrs() {
        Module::with_temp_heap(|module| {
            let heap = module.heap();
            let backing = heap.alloc(AllocStruct([
                ("label", heap.alloc("setting")),
                (
                    "attr",
                    heap.alloc(AllocStruct([(
                        "build_setting_default",
                        heap.alloc(vec!["seed"]),
                    )])),
                ),
            ]));
            let scope =
                crate::analysis_runtime::ScopeState::new("setting".into(), Default::default());
            module.set(
                "ctx",
                wrap(
                    heap,
                    backing,
                    SettingValue::StringList(vec!["seed".to_owned()]),
                    scope.root(),
                    Default::default(),
                ),
            );
            let globals = GlobalsBuilder::extended_by(&[LibraryExtension::StructType]).build();
            let mut eval = Evaluator::new(&module);
            eval.eval_module(AstModule::parse("ctx-test.bzl", "first = ctx.build_setting_value\nfirst.append('mutated')\nsecond = ctx.build_setting_value\nctx.attr.build_setting_default.append('attr-mutated')\nthird = ctx.build_setting_value\n(first == ['seed', 'mutated'] and second == ['seed'] and third == ['seed']) or fail('getter did not preserve its independent invocation snapshot')\n(ctx.label == 'setting' and 'build_setting_value' in dir(ctx)) or fail('context lost fields')\n".into(), &Dialect::Standard).unwrap(), &globals).unwrap();
            scope.finish();
            let error = module
                .get("ctx")
                .unwrap()
                .get_attr_error("build_setting_value", heap)
                .unwrap_err();
            assert!(error.to_string().contains("scope"));
        });
    }

    #[test]
    fn runtime_setting_capability_cannot_freeze() {
        // Deliberate lifecycle change: frozen definitions retain owned defaults,
        // but a live/expired ctx must not become a fabricated frozen capability.
        Module::with_temp_heap(|module| {
            let heap = module.heap();
            let backing = heap.alloc(AllocStruct([("label", heap.alloc("retained"))]));
            let scope =
                crate::analysis_runtime::ScopeState::new("setting".into(), Default::default());
            module.set(
                "ctx",
                wrap(
                    heap,
                    backing,
                    SettingValue::StringList(vec!["seed".to_owned()]),
                    scope.root(),
                    Default::default(),
                ),
            );
            assert!(module.freeze().is_err());
        });
    }
}
