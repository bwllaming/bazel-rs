//! Setting-only analysis context with invocation-owned default snapshots.
//! Collection-valued setting reads return fresh mutable Starlark values.
use allocative::Allocative;
use starlark::values::{Heap, Value, ValueLike};
use std::fmt;

use crate::custom_rule::AttrValue;

// All Starlark references are traced/frozen. AttrValue contains Rust-owned data
// only; retaining it through freezing never carries a reference into a heap.
#[allow(unsafe_code)]
mod values {
    use super::*;
    use starlark::starlark_complex_value;
    use starlark::values::{
        Coerce, Freeze, NoSerialize, ProvidesStaticType, StarlarkPagablePanic, StarlarkValue,
        Trace, starlark_value,
    };

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
    pub(super) struct BuildSettingCtxGen<V> {
        pub backing: V,
        #[trace(unsafe_ignore)]
        #[freeze(identity)]
        pub default: AttrValue,
    }

    impl<V> fmt::Display for BuildSettingCtxGen<V> {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.write_str("<ctx>")
        }
    }

    starlark_complex_value!(pub(super) BuildSettingCtx);

    #[starlark_value(type = "ctx")]
    impl<'v, V: ValueLike<'v>> StarlarkValue<'v> for BuildSettingCtxGen<V>
    where
        Self: ProvidesStaticType<'v>,
    {
        fn get_attr(&self, name: &str, heap: Heap<'v>) -> Option<Value<'v>> {
            if name != "build_setting_value" {
                // Backing is an AllocStruct, not an executable property getter.
                return self.backing.to_value().get_attr(name, heap).ok().flatten();
            }
            match &self.default {
                AttrValue::Bool(value) => Some(heap.alloc(*value)),
                AttrValue::Int(value) => Some(heap.alloc(*value)),
                AttrValue::String(value) => Some(heap.alloc(value.as_str())),
                AttrValue::Strings(value) => Some(heap.alloc(value.clone())),
                // Invocation construction validates the supported descriptor.
                _ => None,
            }
        }

        fn dir_attr(&self) -> Vec<String> {
            let mut names = self.backing.to_value().dir_attr();
            names.push("build_setting_value".to_owned());
            names
        }
    }
}

pub(crate) fn wrap<'v>(heap: Heap<'v>, backing: Value<'v>, default: AttrValue) -> Value<'v> {
    heap.alloc(values::BuildSettingCtxGen { backing, default })
}

#[cfg(test)]
mod tests {
    use super::*;
    use starlark::environment::{GlobalsBuilder, LibraryExtension, Module};
    use starlark::eval::Evaluator;
    use starlark::syntax::{AstModule, Dialect};
    use starlark::values::structs::AllocStruct;

    fn evaluate(module: &Module, source: &str) {
        let globals = GlobalsBuilder::extended_by(&[LibraryExtension::StructType]).build();
        let mut eval = Evaluator::new(module);
        eval.eval_module(
            AstModule::parse("ctx-test.bzl", source.to_owned(), &Dialect::Standard).unwrap(),
            &globals,
        )
        .unwrap();
    }

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
            module.set(
                "ctx",
                wrap(heap, backing, AttrValue::Strings(vec!["seed".to_owned()])),
            );
            evaluate(
                &module,
                "first = ctx.build_setting_value\nfirst.append('mutated')\nsecond = ctx.build_setting_value\nctx.attr.build_setting_default.append('attr-mutated')\nthird = ctx.build_setting_value\n(first == ['seed', 'mutated'] and second == ['seed'] and third == ['seed']) or fail('getter did not preserve its independent invocation snapshot')\n(ctx.label == 'setting' and 'build_setting_value' in dir(ctx)) or fail('context delegation lost fields')\n",
            );
        });
    }

    #[test]
    fn frozen_context_retains_backing_and_returns_copies_in_receiving_heap() {
        let owned = Module::with_temp_heap(|module| {
            let heap = module.heap();
            let backing = heap.alloc(AllocStruct([("label", heap.alloc("retained"))]));
            module.set(
                "ctx",
                wrap(heap, backing, AttrValue::Strings(vec!["seed".to_owned()])),
            );
            module.freeze().unwrap().get("ctx").unwrap()
        });
        Module::with_temp_heap(|module| {
            module.set("ctx", module.heap().access_owned_frozen_value(&owned));
            drop(owned);
            evaluate(
                &module,
                "first = ctx.build_setting_value\nfirst.append('mutable')\n(ctx.build_setting_value == ['seed'] and ctx.label == 'retained') or fail('frozen context lost its snapshot or owner')\n",
            );
        });
    }
}
