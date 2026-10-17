/*
 * Copyright 2018 The Starlark in Rust Authors.
 * Copyright (c) Facebook, Inc. and its affiliates.
 *
 * Licensed under the Apache License, Version 2.0 (the "License");
 * you may not use this file except in compliance with the License.
 * You may obtain a copy of the License at
 *
 *     https://www.apache.org/licenses/LICENSE-2.0
 *
 * Unless required by applicable law or agreed to in writing, software
 * distributed under the License is distributed on an "AS IS" BASIS,
 * WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
 * See the License for the specific language governing permissions and
 * limitations under the License.
 */

// Modified for native Bazel 9.3 set compatibility; see vendor provenance.
// Modified for Bazel's `set(elements)` binding and parameter type texts
// (net/starlark/java/eval/MethodLibrary.java:667-690); see vendor provenance.
// Expose the Bazel constructor contract, not a parametric type object.
use allocative::Allocative;
use derive_more::Display;
use starlark_derive::NoSerialize;
use starlark_derive::StarlarkPagable;
use starlark_derive::starlark_value;

use crate as starlark;
use crate::environment::GlobalsBuilder;
use crate::eval::Arguments;
use crate::eval::Evaluator;
use crate::starlark_simple_value;
use crate::typing::ParamSpec;
use crate::typing::Ty;
use crate::values::ProvidesStaticType;
use crate::values::StarlarkValue;
use crate::values::UnpackValue;
use crate::values::Value;
use crate::values::ValueLike;
use crate::values::set::refs::SetRef;
use crate::values::set::value::SetData;

/// Modified: `set(elements=[])` with a positional-only `elements`
/// (MethodLibrary.java:667-690), for Bazel's binding texts.
fn set_signature() -> &'static crate::eval::ParametersSpec<crate::values::FrozenValue> {
    static SIGNATURE: std::sync::OnceLock<crate::eval::ParametersSpec<crate::values::FrozenValue>> =
        std::sync::OnceLock::new();
    SIGNATURE.get_or_init(|| {
        crate::eval::ParametersSpec::new_parts(
            "set",
            [("elements", crate::eval::ParametersSpecParam::Optional)],
            [],
            false,
            [],
            false,
        )
    })
}

pub(crate) fn register_set(globals: &mut GlobalsBuilder) {
    globals.set("set", SetConstructor);
}

#[derive(Debug, Display, ProvidesStaticType, NoSerialize, Allocative, StarlarkPagable)]
#[display("<built-in function set>")]
struct SetConstructor;

starlark_simple_value!(SetConstructor);

#[starlark_value(type = "builtin_function_or_method")]
impl<'v> StarlarkValue<'v> for SetConstructor {
    fn typechecker_ty(&self) -> Option<Ty> {
        // Callable metadata is independent of runtime type-object support:
        // retain argument/result checking without exposing .type or set[T].
        Some(Ty::function(
            ParamSpec::pos_only([], [Ty::iter(Ty::any())]),
            Ty::any_set(),
        ))
    }

    // Deliberately inherit default unhashability and empty attributes. Reexports
    // from independently built globals still denote this same constructor.
    fn equals(&self, other: Value<'v>) -> crate::Result<bool> {
        Ok(other.downcast_ref::<Self>().is_some())
    }

    fn invoke(
        &self,
        _me: Value<'v>,
        args: &Arguments<'v, '_>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> crate::Result<Value<'v>> {
        let heap = eval.heap();
        // Modified: Bazel's binding texts for the positional-only `elements`.
        let arg = args
            .no_named_args()
            .and_then(|()| args.optional1(heap))
            .map_err(|e| set_signature().bazel_rebind_error(e, args, &[], heap))?;
        let set = match arg {
            Some(pos) => match SetRef::unpack_value_opt(pos) {
                Some(set) => (set.aref).clone(),
                None => {
                    let it = pos.iterate(heap).map_err(|_| {
                        crate::values::error::bazel_param_type_error(
                            "set", "elements", pos, "iterable",
                        )
                    })?;
                    let mut data = SetData::default();
                    for el in it {
                        let el = el.get_hashed()?;
                        data.content.insert_hashed(el);
                    }
                    data
                }
            },
            None => SetData::default(),
        };
        Ok(heap.alloc(set))
    }
}
#[cfg(test)]
mod tests {
    use crate::assert;
    use crate::assert::Assert;

    #[test]
    fn test_constructor_surface_and_bindings() {
        assert::is_true(
            r#"
type(set) == 'builtin_function_or_method' and
repr(set) == '<built-in function set>' and
str(set) == repr(set) and dir(set) == [] and
not hasattr(set, 'type') and set == set and set != list
"#
            .replace(" and\n", " and ")
            .as_str(),
        );
        assert::eq("list(set(*[(3, 1, 3)], **{}))", "[3, 1]");
        assert::eq("list(set({'b': 1, 'a': 2}))", "['b', 'a']");
        assert::eq("list(set(range(3)))", "[0, 1, 2]");
        // Modified: Bazel's BuiltinFunction binding and parameter texts.
        assert::fail(
            "set(elements=[1])",
            "set() got named argument for positional-only parameter 'elements'",
        );
        assert::fail(
            "set([], [])",
            "set() accepts no more than 1 positional argument but got 2",
        );
        assert::fail(
            "set(None)",
            "in call to set(), parameter 'elements' got value of type 'NoneType', want 'iterable'",
        );
        assert::fail(
            "set('abc')",
            "in call to set(), parameter 'elements' got value of type 'string', want 'iterable'",
        );
        assert::fail(
            "set[int]",
            "type 'builtin_function_or_method' has no operator [](builtin_function_or_method)",
        );
        assert::fail("{set: 1}", "unhashable type");
        assert::fail("set([set])", "unhashable type");
    }

    #[test]
    fn test_constructor_frozen_aliases_and_fresh_copy() {
        let mut a = Assert::new();
        a.module("one.star", "R = set; S = set([2, 1])");
        a.module("two.star", "R = set");
        a.is_true(
            r#"
load('one.star', R1='R', S='S')
load('two.star', R2='R')
copy = R1(S)
copy.add(3)
R1 == R2 and R1 == set and R2 != list and list(S) == [2, 1] and list(copy) == [2, 1, 3]
"#,
        );
    }

    #[test]
    fn test_constructor_iterable_is_not_a_method_collection() {
        use crate as starlark;
        use crate::values::{Heap, ProvidesStaticType, StarlarkValue, Value};
        use allocative::Allocative;
        use derive_more::Display;
        use starlark_derive::{NoSerialize, StarlarkPagable};

        #[derive(Debug, Display, ProvidesStaticType, NoSerialize, Allocative, StarlarkPagable)]
        #[display("host_iterable")]
        struct HostIterable;
        crate::starlark_simple_value!(HostIterable);

        // Same Starlark type name as string elems/codepoints, different Rust
        // identity: method collection validation must not accept a name spoof.
        #[crate::values::starlark_value(type = "iterator")]
        impl<'v> StarlarkValue<'v> for HostIterable {
            unsafe fn iterate(&self, _me: Value<'v>, heap: Heap<'v>) -> crate::Result<Value<'v>> {
                // A tuple is already an immutable iterator with no borrow guard.
                Ok(heap.alloc((3, 1, 3)))
            }
        }

        let mut a = Assert::new();
        a.globals_add(|globals| {
            globals.set("iterable", HostIterable);
        });
        a.is_true("list(set(iterable)) == [3, 1]");
        for method in [
            "union",
            "intersection",
            "difference",
            "update",
            "intersection_update",
            "difference_update",
            "symmetric_difference",
            "symmetric_difference_update",
            "issubset",
            "issuperset",
            "isdisjoint",
        ] {
            a.fail(&format!("set().{method}(iterable)"), "collection");
        }
    }

    #[test]
    fn test_set_type_as_type_compile_time() {
        assert::fail(
            r"
def f_fail_ct(x: set[int]):
    return x

s = set(['not_int'])

f_fail_ct(s)
",
            // Modified: Bazel's EvalUtils.index text.
            "type 'builtin_function_or_method' has no operator [](type)",
        );
    }

    #[test]
    fn test_return_set_type_as_type_compile_time() {
        assert::fail(
            r"
def f_fail_ct(x: str) -> set[int]:
    return set([x])

f_fail_ct('not_int')
",
            // Modified: Bazel's EvalUtils.index text.
            "type 'builtin_function_or_method' has no operator [](type)",
        );
    }

    #[test]
    fn test_set_type_as_type_run_time() {
        assert::fail(
            r"
def f_fail_rt(x: set[int]):
    return x

s = set(['not_int'])

noop(f_fail_rt)(s)
",
            // Modified: Bazel's EvalUtils.index text.
            "type 'builtin_function_or_method' has no operator [](type)",
        );
    }
}
