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

// Modified for native Bazel 9.3 `tuple(x)` parameter type text
// (net/starlark/java/eval/MethodLibrary.java:375-388); see vendor provenance.

use starlark_derive::starlark_module;

use crate as starlark;
use crate::environment::GlobalsBuilder;
use crate::values::Heap;
use crate::values::Value;
use crate::values::ValueOfUnchecked;
use crate::values::function::SpecialBuiltinFunction;
use crate::values::tuple::AllocTuple;
use crate::values::tuple::TupleRef;
use crate::values::tuple::value::FrozenTuple;
use crate::values::typing::StarlarkIter;

#[starlark_module]
pub(crate) fn register_tuple(globals: &mut GlobalsBuilder) {
    /// [tuple](
    /// https://github.com/bazelbuild/starlark/blob/master/spec.md#tuple
    /// ): returns a tuple containing the elements of the iterable x.
    ///
    /// With no arguments, `tuple()` returns the empty tuple.
    ///
    /// ```
    /// # starlark::assert::all_true(r#"
    /// tuple() == ()
    /// tuple([1,2,3]) == (1, 2, 3)
    /// # "#);
    /// ```
    #[starlark(
        as_type = FrozenTuple,
        speculative_exec_safe,
        special_builtin_function = SpecialBuiltinFunction::Tuple,
    )]
    fn tuple<'v>(
        #[starlark(require = pos)] x: Option<ValueOfUnchecked<'v, StarlarkIter<Value<'v>>>>,
        heap: Heap<'v>,
    ) -> starlark::Result<ValueOfUnchecked<'v, &'v TupleRef<'v>>> {
        // Modified: Bazel's parameter names (MethodLibrary.java:375-383).
        let a = x;
        if let Some(a) = a {
            if TupleRef::from_value(a.get()).is_some() {
                return Ok(ValueOfUnchecked::new(a.get()));
            }

            // Modified: Bazel checks `x` is a StarlarkIterable.
            let it = a.get().iterate(heap).map_err(|_| {
                crate::values::error::bazel_param_type_error("tuple", "x", a.get(), "iterable")
            })?;
            Ok(ValueOfUnchecked::new(heap.alloc_tuple_iter(it)))
        } else {
            Ok(ValueOfUnchecked::new(heap.alloc(AllocTuple::EMPTY)))
        }
    }
}
