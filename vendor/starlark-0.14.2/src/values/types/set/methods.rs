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

//! Methods for the `set` type.

use std::mem;

use starlark_derive::starlark_module;
use starlark_map::small_set::SmallSet;
use starlark_syntax::value_error;

use crate as starlark;
use crate::environment::MethodsBuilder;
use crate::values::Heap;
use crate::values::UnpackValue;
use crate::values::Value;
use crate::values::ValueLike;
use crate::values::dict::DictRef;
use crate::values::list::ListRef;
use crate::values::none::NoneType;
use crate::values::range::Range;
use crate::values::set::refs::SetMut;
use crate::values::set::refs::SetRef;
use crate::values::set::value::SetData;
use crate::values::string::iter::is_string_elems;
use crate::values::tuple::TupleRef;
use crate::values::tuple::UnpackTuple;

#[derive(Clone, Copy)]
pub(crate) enum SetMutationOp {
    Union,
    Difference,
    Intersection,
    SymmetricDifference,
}

// Unlike the constructor, methods require a collection, not an arbitrary iterable.
// Snapshot and validate the complete operand before applying any of its elements.
fn collection<'v>(value: Value<'v>, heap: Heap<'v>) -> crate::Result<SmallSet<Value<'v>>> {
    if let Some(set) = SetRef::unpack_value_opt(value) {
        return Ok(set.aref.content.clone());
    }
    if ListRef::from_value(value).is_none()
        && TupleRef::from_value(value).is_none()
        && DictRef::from_value(value).is_none()
        && value.downcast_ref::<Range>().is_none()
        && !is_string_elems(value)
    {
        return Err(value_error!(
            "got value of type '{}', want a collection of hashable elements",
            value.get_type()
        ));
    }
    let mut result = SmallSet::new();
    for element in value.iterate(heap)? {
        result.insert_hashed(element.get_hashed()?);
    }
    Ok(result)
}

fn apply<'v>(lhs: &mut SmallSet<Value<'v>>, rhs: &SmallSet<Value<'v>>, op: SetMutationOp) {
    match op {
        SetMutationOp::Union => {
            for element in rhs.iter_hashed() {
                lhs.insert_hashed(element.copied());
            }
        }
        SetMutationOp::Difference | SetMutationOp::Intersection => {
            let intersection = matches!(op, SetMutationOp::Intersection);
            let mut result = SmallSet::new();
            for element in lhs.iter_hashed() {
                if rhs.contains_hashed(element) == intersection {
                    result.insert_hashed_unique_unchecked(element.copied());
                }
            }
            *lhs = result;
        }
        SetMutationOp::SymmetricDifference => {
            for element in rhs.iter_hashed() {
                if !lhs.shift_remove_hashed(element) {
                    lhs.insert_hashed(element.copied());
                }
            }
        }
    }
}

fn mutate_operand<'v>(
    this: Value<'v>,
    other: Value<'v>,
    op: SetMutationOp,
    heap: Heap<'v>,
) -> crate::Result<()> {
    // Never unpack an aliased SetRef while holding SetMut (RefCell::borrow panics).
    drop(SetMut::from_value(this)?);
    let other = collection(other, heap)?;
    apply(&mut SetMut::from_value(this)?.aref.content, &other, op);
    Ok(())
}

fn mutate<'v>(
    this: Value<'v>,
    others: Vec<Value<'v>>,
    op: SetMutationOp,
    heap: Heap<'v>,
) -> crate::Result<NoneType> {
    // Even zero-operand/no-op calls must reject frozen or actively iterated sets.
    drop(SetMut::from_value(this)?);
    for other in others {
        mutate_operand(this, other, op, heap)?;
    }
    Ok(NoneType)
}

fn algebra<'v>(
    this: SetRef<'v>,
    others: Vec<Value<'v>>,
    op: SetMutationOp,
    heap: Heap<'v>,
) -> crate::Result<SetData<'v>> {
    let mut content = this.aref.content.clone();
    for other in others {
        apply(&mut content, &collection(other, heap)?, op);
    }
    Ok(SetData { content })
}

pub(crate) fn try_set_augmented_assign<'v>(
    lhs: Value<'v>,
    rhs: Value<'v>,
    op: SetMutationOp,
    heap: Heap<'v>,
) -> Option<crate::Result<Value<'v>>> {
    // Qualify BOTH types before the receiver guard. Non-set operands retain the
    // compiler's old binary fallback, even when the other operand is frozen.
    if SetRef::unpack_value_opt(lhs).is_none() || SetRef::unpack_value_opt(rhs).is_none() {
        return None;
    }
    Some(mutate_operand(lhs, rhs, op, heap).map(|()| lhs))
}

#[starlark_module]
pub(crate) fn set_methods(builder: &mut MethodsBuilder) {
    fn clear(this: Value) -> anyhow::Result<NoneType> {
        let mut this = SetMut::from_value(this)?;
        this.aref.clear();
        Ok(NoneType)
    }

    /// Return a new set with elements from the set and all others.
    /// Accepts zero or more collections.
    /// ```
    /// # starlark::assert::is_true(r#"
    /// x = set([1, 2, 3])
    /// y = [3, 4, 5]
    /// x.union(y) == set([1, 2, 3, 4, 5])
    /// # "#);
    /// ```
    fn union<'v>(
        this: SetRef<'v>,
        #[starlark(args)] others: UnpackTuple<Value<'v>>,
        heap: Heap<'v>,
    ) -> starlark::Result<SetData<'v>> {
        algebra(this, others.items, SetMutationOp::Union, heap)
    }

    /// Return a new set with elements common to the set and all others.
    /// Accepts zero or more collections, preserving the receiver's order.
    /// ```
    /// # starlark::assert::is_true(r#"
    /// x = set([1, 2, 3])
    /// y = [3, 4, 5]
    /// x.intersection(y) == set([3])
    /// # "#);
    /// ```
    fn intersection<'v>(
        this: SetRef<'v>,
        #[starlark(args)] others: UnpackTuple<Value<'v>>,
        heap: Heap<'v>,
    ) -> starlark::Result<SetData<'v>> {
        algebra(this, others.items, SetMutationOp::Intersection, heap)
    }

    /// Returns a new set with elements in either the set or the specified iterable but not both.
    /// ```
    /// # starlark::assert::is_true(r#"
    /// x = set([1, 2, 3])
    /// y = [3, 4, 5]
    /// x.symmetric_difference(y) == set([1, 2, 4, 5])
    /// # "#);
    /// ```
    fn symmetric_difference<'v>(
        this: SetRef<'v>,
        #[starlark(require=pos)] other: Value<'v>,
        heap: Heap<'v>,
    ) -> starlark::Result<SetData<'v>> {
        algebra(this, vec![other], SetMutationOp::SymmetricDifference, heap)
    }

    /// Add an item to the set.
    /// ```
    /// # starlark::assert::is_true(r#"
    /// x = set([1, 2, 3])
    /// x.add(4)
    /// x == set([1, 2, 3, 4])
    /// # "#);
    /// ```
    fn add<'v>(
        this: Value<'v>,
        #[starlark(require = pos)] element: Value<'v>,
    ) -> starlark::Result<NoneType> {
        let mut this = SetMut::from_value(this)?;
        let hashed = element.get_hashed()?;
        this.aref.add_hashed(hashed);
        Ok(NoneType)
    }

    /// Update the set by adding items from an iterable.
    /// ```
    /// # starlark::assert::is_true(r#"
    /// x = set([1, 3, 2])
    /// x.update([4, 3])
    /// list(x) == [1, 3, 2, 4]
    /// # "#);
    /// ```
    fn update<'v>(
        this: Value<'v>,
        #[starlark(args)] others: UnpackTuple<Value<'v>>,
        heap: Heap<'v>,
    ) -> starlark::Result<NoneType> {
        mutate(this, others.items, SetMutationOp::Union, heap)
    }

    /// Retain only elements present in every supplied collection.
    fn intersection_update<'v>(
        this: Value<'v>,
        #[starlark(args)] others: UnpackTuple<Value<'v>>,
        heap: Heap<'v>,
    ) -> starlark::Result<NoneType> {
        mutate(this, others.items, SetMutationOp::Intersection, heap)
    }

    /// Remove elements present in any supplied collection.
    fn difference_update<'v>(
        this: Value<'v>,
        #[starlark(args)] others: UnpackTuple<Value<'v>>,
        heap: Heap<'v>,
    ) -> starlark::Result<NoneType> {
        mutate(this, others.items, SetMutationOp::Difference, heap)
    }

    /// Toggle elements from exactly one collection.
    fn symmetric_difference_update<'v>(
        this: Value<'v>,
        #[starlark(require=pos)] other: Value<'v>,
        heap: Heap<'v>,
    ) -> starlark::Result<NoneType> {
        // KNOWN_REFERENCE_DEFECT — INTENTIONAL_DIVERGENCE: Bazel 9.3 crashes
        // on multi-element self-XOR. Snapshotting safely empties the original.
        mutate_operand(this, other, SetMutationOp::SymmetricDifference, heap)?;
        Ok(NoneType)
    }

    /// Remove the item from the set. It raises an error if there is no such item.
    ///
    /// `remove` fails if the key is unhashable or if the dictionary is
    /// frozen.
    /// Time complexity of this operation is *O(N)* where *N* is the number of entries in the set.
    ///
    /// ```
    /// # starlark::assert::is_true(r#"
    /// x = set([1, 2, 3])
    /// x.remove(2)
    /// x == set([1, 3])
    /// # "#)
    /// ```
    /// A subsequent call to `x.remove(2)` would yield an error because the
    /// element won't be found.
    /// ```
    /// # starlark::assert::fail(r#"
    /// x = set([1, 2, 3])
    /// x.remove(2)
    /// x.remove(2) # error: not found
    /// # "#, "not found");
    /// ```
    fn remove<'v>(
        this: Value<'v>,
        #[starlark(require = pos)] element: Value<'v>,
    ) -> starlark::Result<NoneType> {
        let mut set = SetMut::from_value(this)?;
        let hashed = element.get_hashed()?;
        if set.aref.remove_hashed(hashed.as_ref()) {
            Ok(NoneType)
        } else {
            mem::drop(set);
            Err(value_error!("`{element}` not found in `{this}`"))
        }
    }

    /// Remove the item from the set. It does nothing if there is no such item.
    ///
    /// `discard` fails if the key is unhashable or if the dictionary is
    /// frozen.
    /// Time complexity of this operation is *O(N)* where *N* is the number of entries in the set.
    ///
    /// ```
    /// # starlark::assert::is_true(r#"
    /// x = set([1, 2, 3])
    /// x.discard(2)
    /// x == set([1, 3])
    /// # "#)
    /// ```
    /// A subsequent call to `x.discard(2)` would do nothing.
    /// ```
    /// # starlark::assert::is_true(r#"
    /// x = set([1, 2, 3])
    /// x.discard(2)
    /// x.discard(2)
    /// x == set([1, 3])
    /// # "#);
    /// ```
    fn discard<'v>(
        this: Value<'v>,
        #[starlark(require = pos)] element: Value<'v>,
    ) -> starlark::Result<NoneType> {
        let mut set = SetMut::from_value(this)?;
        let hashed = element.get_hashed()?;
        set.aref.remove_hashed(hashed.as_ref());
        Ok(NoneType)
    }

    /// Removes and returns the **first** element of a set.
    ///
    /// `S.pop()` removes and returns the first element of the set S.
    ///
    /// `pop` fails if the set is empty, or if the set is frozen or has active iterators.
    /// Time complexity of this operation is *O(N)*.
    ///
    /// ```
    /// # starlark::assert::is_true(r#"
    /// x = set([1, 2, 3])
    /// # (
    /// x.pop() == 1
    /// # and
    /// x.pop() == 2
    /// # and
    /// x == set([3])
    /// # )"#);
    /// ```
    fn pop<'v>(this: Value<'v>) -> starlark::Result<Value<'v>> {
        let mut set = SetMut::from_value(this)?;
        let first = set.aref.content.iter_hashed().next().map(|x| x.copied());
        match first {
            Some(x) => {
                set.aref.content.shift_remove_hashed(x.as_ref());
                Ok(x.into_key())
            }
            None => Err(value_error!("pop from an empty set")),
        }
    }

    /// Returns a new set with elements unique the set when compared to the specified iterable.
    /// ```
    /// # starlark::assert::is_true(r#"
    /// x = set([1, 2, 3])
    /// y = [3, 4, 5]
    /// x.difference(y) == set([1, 2])
    /// # "#);
    /// ```
    fn difference<'v>(
        this: SetRef<'v>,
        #[starlark(args)] others: UnpackTuple<Value<'v>>,
        heap: Heap<'v>,
    ) -> starlark::Result<SetData<'v>> {
        algebra(this, others.items, SetMutationOp::Difference, heap)
    }

    /// Test whether every element other iterable is in the set.
    /// ```
    /// # starlark::assert::is_true(r#"
    /// x = set([1, 2, 3])
    /// y = [1, 3]
    /// x.issuperset(y) == True
    /// # "#);
    /// ```
    fn issuperset<'v>(
        this: SetRef<'v>,
        #[starlark(require=pos)] other: Value<'v>,
        heap: Heap<'v>,
    ) -> starlark::Result<bool> {
        let other = collection(other, heap)?;
        Ok(other
            .iter_hashed()
            .all(|x| this.aref.content.contains_hashed(x)))
    }

    /// Test whether every element in the set is in other iterable.
    /// ```
    /// # starlark::assert::is_true(r#"
    /// x = set([1, 2, 3])
    /// y = [3, 1, 2]
    /// x.issubset(y)
    /// # "#);
    /// ```
    fn issubset<'v>(
        this: SetRef<'v>,
        #[starlark(require=pos)] other: Value<'v>,
        heap: Heap<'v>,
    ) -> starlark::Result<bool> {
        let other = collection(other, heap)?;
        Ok(this
            .aref
            .content
            .iter_hashed()
            .all(|x| other.contains_hashed(x)))
    }

    /// Test whether the set has no elements in common with the collection.
    fn isdisjoint<'v>(
        this: SetRef<'v>,
        #[starlark(require=pos)] other: Value<'v>,
        heap: Heap<'v>,
    ) -> starlark::Result<bool> {
        let other = collection(other, heap)?;
        Ok(this
            .aref
            .content
            .iter_hashed()
            .all(|x| !other.contains_hashed(x)))
    }
}

#[cfg(test)]
mod tests {
    use crate::assert;
    use crate::assert::Assert;

    #[test]
    fn test_string_elems_method_collections() {
        let mut a = Assert::new();
        a.module("elems.star", "Elems = 'bc'.elems()");
        // Genuine Bazel 9.3 returns a list here. Adapt only this native proxy,
        // including its frozen representation; preserve the existing pipeline.
        for setup in ["Elems = 'bc'.elems()", "load('elems.star', 'Elems')"] {
            for (call, expected) in [
                ("list(set(Elems))", "['b', 'c']"),
                ("list(s.union(Elems))", "['a', 'b', 'c']"),
                ("list(s.intersection(Elems))", "['b']"),
                ("list(s.difference(Elems))", "['a']"),
                ("list(s.symmetric_difference(Elems))", "['a', 'c']"),
                ("s.issubset(Elems)", "False"),
                ("set(['b']).issubset(Elems)", "True"),
                ("s.issuperset(Elems)", "False"),
                ("s.isdisjoint(Elems)", "False"),
            ] {
                a.is_true(&format!("{setup}\ns=set(['a', 'b'])\n{call} == {expected}"));
            }
            for (method, expected) in [
                ("update", "['a', 'b', 'c']"),
                ("intersection_update", "['b']"),
                ("difference_update", "['a']"),
                ("symmetric_difference_update", "['a', 'c']"),
            ] {
                a.is_true(&format!("{setup}\ns=set(['a', 'b'])\nresult=s.{method}(Elems)\nresult == None and list(s) == {expected}"));
            }
        }
        assert::eq("list(set('a'.elems()).union('b'.elems()))", "['a', 'b']");
    }

    #[test]
    fn test_string_codepoints_are_not_method_collections() {
        let mut a = Assert::new();
        a.module("codepoints.star", "Codepoints = 'bc'.codepoints()");
        for setup in [
            "Codepoints = 'bc'.codepoints()",
            "load('codepoints.star', 'Codepoints')",
        ] {
            // Constructor iterability is unchanged. Bazel has no codepoints
            // method; this is not a character-elements collection adapter.
            a.is_true(&format!("{setup}\nlist(set(Codepoints)) == [98, 99]"));
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
                a.fail(
                    &format!("{setup}\nset().{method}(Codepoints)"),
                    "collection",
                );
            }
        }
    }

    #[test]
    fn test_complete_method_surface_and_variadics() {
        assert::eq(
            "dir(set())",
            "['add', 'clear', 'difference', 'difference_update', 'discard', 'intersection', 'intersection_update', 'isdisjoint', 'issubset', 'issuperset', 'pop', 'remove', 'symmetric_difference', 'symmetric_difference_update', 'union', 'update']",
        );
        for method in ["union", "intersection", "difference"] {
            assert::eq(
                &format!("s=set([3, 1, 2]); t=s.{method}(); t.add(9); (list(s), list(t))"),
                "([3, 1, 2], [3, 1, 2, 9])",
            );
        }
        assert::eq(
            "list(set([3, 1, 2]).union([4], (5, 4), {6: 0}, range(2)))",
            "[3, 1, 2, 4, 5, 6, 0]",
        );
        assert::eq("list(set([3, 1, 2]).intersection([2, 3], [3]))", "[3]");
        assert::eq("list(set([3, 1, 2]).difference([1], [2]))", "[3]");
        assert::eq(
            "s=set([3, 1, 2]); s.update(); s.update([4], [5]); s.intersection_update([1, 2, 4], [2, 4]); s.difference_update([2], []); s.symmetric_difference_update([4, 6, 6]); list(s)",
            "[6]",
        );
        assert::is_true("set([1]).isdisjoint([2]) and not set([1]).isdisjoint([1])");
    }

    #[test]
    fn test_binding_and_complete_operand_validation() {
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
            assert::fail(&format!("set().{method}([2, []])"), "unhashable type");
            assert::fail(&format!("set().{method}('abc')"), "collection");
            // Modified: Bazel's BuiltinFunction binding texts.
            assert::fails(
                &format!("set().{method}(other=[])"),
                &[&format!("{method}() got "), "'other'"],
            );
        }
        for method in [
            "symmetric_difference",
            "symmetric_difference_update",
            "issubset",
            "issuperset",
            "isdisjoint",
        ] {
            assert::fail(&format!("set().{method}()"), "positional");
            assert::fail(&format!("set().{method}([], [])"), "positional");
        }
        for method in ["add", "remove", "discard"] {
            assert::fail(
                &format!("set().{method}(element=1)"),
                &format!("{method}() got named argument for positional-only parameter 'element'"),
            );
        }
        assert::fail("set([1]).issuperset([2, []])", "unhashable type");
        assert::fail("set([1]).isdisjoint([1, []])", "unhashable type");
    }

    #[test]
    fn test_self_aliases_and_reference_xor_defect() {
        for method in ["update", "intersection_update"] {
            assert::eq(
                &format!("s=set([3, 1, 2]); a=s; s.{method}(s); list(a)"),
                "[3, 1, 2]",
            );
        }
        for method in ["difference_update", "symmetric_difference_update"] {
            // Multi-element self-XOR: KNOWN_REFERENCE_DEFECT —
            // INTENTIONAL_DIVERGENCE, not a differential parity PASS.
            assert::eq(
                &format!("s=set([3, 1, 2]); a=s; s.{method}(s); list(a)"),
                "[]",
            );
        }
        assert::eq(
            "s=set([1, 2]); t=s ^ s; t.add(3); (list(s), list(t))",
            "([1, 2], [3])",
        );
    }

    #[test]
    fn test_frozen_and_iteration_noop_guards() {
        let mut a = Assert::new();
        a.module("frozen.star", "S=set([1, 2])");
        for call in [
            "update()",
            "update(S)",
            "intersection_update()",
            "difference_update()",
            "symmetric_difference_update([])",
            "add(1)",
            "discard(9)",
            "clear()",
            "remove(1)",
            "pop()",
            "update(1)",
        ] {
            a.fail(&format!("load('frozen.star', 'S')\nS.{call}"), "Immutable");
            assert::fail(
                &format!("def f():\n    S=set([1, 2])\n    for x in S:\n        S.{call}\nf()"),
                "while iterating",
            );
        }
        assert::eq(
            "def f():\n    s=set([1, 2])\n    for x in s:\n        break\n    s.add(3)\n    return list(s)\nf()",
            "[1, 2, 3]",
        );
    }

    #[test]
    fn test_per_operand_error_retained_module() {
        use crate::environment::{GlobalsBuilder, LibraryExtension, Module};
        use crate::eval::Evaluator;
        use crate::syntax::{AstModule, Dialect};
        use crate::values::UnpackValue;
        use crate::values::set::SetRef;

        // Expected Java states are SOURCE_DERIVED from pinned StarlarkSet.java;
        // the reference Java host harness was not executable. These are native
        // observations of the retained Module, not unreachable Starlark asserts.
        for (call, expected) in [
            ("update([4], [5, []])", vec![1, 2, 3, 4]),
            ("difference_update([1], [2, []])", vec![2, 3]),
            ("intersection_update([2, 3], [3, []])", vec![2, 3]),
            ("symmetric_difference_update([1, 4, []])", vec![1, 2, 3]),
        ] {
            Module::with_temp_heap(|module| {
                let globals = GlobalsBuilder::extended_by(&[LibraryExtension::SetType]).build();
                let ast = AstModule::parse(
                    "post_error.star",
                    format!("S=set([1, 2, 3])\nS.{call}"),
                    &Dialect::Standard,
                )
                .unwrap();
                let error = Evaluator::new(&module)
                    .eval_module(ast, &globals)
                    .unwrap_err();
                assert!(error.to_string().contains("unhashable type"), "{error}");
                let s = SetRef::unpack_value_opt(module.get("S").unwrap()).unwrap();
                let actual: Vec<_> = s.iter().map(|x| x.unpack_i32().unwrap()).collect();
                assert_eq!(actual, expected, "{call}");
            });
        }
    }

    #[test]
    fn test_empty() {
        assert::is_true("s = set(); len(s) == 0")
    }

    #[test]
    fn test_single() {
        assert::is_true("s = set([0, 1]); len(s) == 2")
    }

    #[test]
    fn test_eq() {
        assert::is_true("set([1, 2, 3]) == set([3, 2, 1])")
    }

    #[test]
    fn test_clear() {
        assert::is_true("s = set([1, 2, 3]); s.clear(); s == set()")
    }

    #[test]
    fn test_type() {
        assert::eq("type(set([1, 2, 3]))", "'set'")
    }

    #[test]
    fn test_iter() {
        assert::is_true("list([elem for elem in set([1, 2, 3])]) ==  [1, 2, 3]")
    }

    #[test]
    fn test_bool_true() {
        assert::is_true("bool(set([1, 2, 3]))")
    }

    #[test]
    fn test_bool_false() {
        assert::is_false("bool(set())")
    }

    #[test]
    fn test_union() {
        assert::eq(
            "set([1, 2, 3]).union(set([3, 4, 5]))",
            "set([1, 2, 3, 4, 5])",
        )
    }

    #[test]
    fn test_union_empty() {
        assert::eq("set([1, 2, 3]).union(set([]))", "set([1, 2, 3])")
    }

    #[test]
    fn test_union_iter() {
        assert::eq("set([1, 2, 3]).union([3, 4])", "set([1, 2, 3, 4])")
    }

    #[test]
    fn test_union_ordering_mixed() {
        assert::eq("list(set([1, 3, 5]).union(set([4, 3])))", "[1, 3, 5, 4]");
    }

    #[test]
    fn test_intersection() {
        assert::eq("set([1, 2, 3]).intersection(set([3, 4, 5]))", "set([3])")
    }

    #[test]
    fn test_intersection_empty() {
        assert::eq("set([1, 2, 3]).intersection(set([]))", "set([])")
    }

    #[test]
    fn test_intersection_iter() {
        assert::eq("set([1, 2, 3]).intersection([3, 4])", "set([3])")
    }

    #[test]
    fn test_intersection_order() {
        assert::eq("list(set([1, 2, 3]).intersection([4, 3, 1]))", "[1, 3]")
    }

    #[test]
    fn test_symmetric_difference() {
        assert::eq(
            "set([1, 2, 3]).symmetric_difference(set([3, 4, 5]))",
            "set([1, 2, 4, 5])",
        )
    }

    #[test]
    fn test_symmetric_difference_empty() {
        assert::eq(
            "set([1, 2, 3]).symmetric_difference(set([]))",
            "set([1, 2, 3])",
        )
    }

    #[test]
    fn test_symmetric_difference_iter() {
        assert::eq(
            "set([1, 2, 3]).symmetric_difference([3, 4])",
            "set([1, 2, 4])",
        )
    }

    #[test]
    fn test_symmetric_difference_ord() {
        assert::eq(
            "list(set([1, 2, 3, 7]).symmetric_difference(set([4, 3, 1])))",
            "[2, 7, 4]",
        )
    }

    #[test]
    fn test_add() {
        assert::eq(r#"x = set([1, 2, 3]);x.add(0);x"#, "set([0, 1, 2, 3])")
    }

    #[test]
    fn test_add_empty() {
        assert::eq(r#"x = set([]);x.add(0);x"#, "set([0])")
    }

    #[test]
    fn test_add_existing() {
        assert::eq(r#"x = set([0]);x.add(0);x"#, "set([0])")
    }

    #[test]
    fn test_add_order() {
        assert::eq(r#"x = set([1, 2, 3]);x.add(2);list(x)"#, "[1, 2, 3]");
        assert::eq(r#"x = set([1, 2, 3]);x.add(0);list(x)"#, "[1, 2, 3, 0]")
    }

    #[test]
    fn test_remove() {
        assert::eq("x = set([0, 1]);x.remove(1);x", "set([0])")
    }

    #[test]
    fn test_remove_empty() {
        assert::fail("set([]).remove(0)", "`0` not found in `set()`");
    }

    #[test]
    fn test_remove_not_existing() {
        assert::fail("set([1]).remove(0)", "`0` not found in `set([1])`");
    }

    #[test]
    fn test_discard() {
        assert::eq("x = set([0, 1]);x.discard(1);x", "set([0])")
    }

    #[test]
    fn test_discard_multiple_times() {
        assert::eq("x = set([0, 1]); x.discard(0); x.discard(0); x", "set([1])");
    }

    #[test]
    fn test_pop() {
        assert::is_true("x = set([1, 0]); (x.pop() == 1 and x.pop() == 0 and x == set())");
    }

    #[test]
    fn test_pop_empty() {
        assert::fail("x = set([]); x.pop()", "pop from an empty set");
    }

    #[test]
    fn test_difference() {
        assert::eq("set([1, 2, 3]).difference(set([2]))", "set([1, 3])")
    }

    #[test]
    fn test_difference_iter() {
        assert::eq("set([1, 2, 3]).difference([3, 2])", "set([1])")
    }

    #[test]
    fn test_difference_order() {
        assert::eq("list(set([3, 2, 1]).difference([2]))", "[3, 1]")
    }

    #[test]
    fn test_difference_empty_lhs() {
        assert::eq("set([]).difference(set([2]))", "set([])")
    }

    #[test]
    fn test_difference_empty_rhs() {
        assert::eq("set([1, 2]).difference(set([]))", "set([2, 1])")
    }

    #[test]
    fn test_is_superset() {
        assert::is_true("set([1, 2, 3, 4]).issuperset(set([1, 3, 2]))")
    }

    #[test]
    fn test_is_not_superset() {
        assert::is_false("set([1, 2]).issuperset(set([1, 3, 5]))")
    }

    #[test]
    fn test_is_not_superset_empty_lhs() {
        assert::is_false("set([]).issuperset(set([1]))");
    }

    #[test]
    fn test_is_superset_empty_rhs() {
        assert::is_true("set([1, 2]).issuperset(set([]))");
        assert::is_true("set([]).issuperset(set([]))")
    }

    #[test]
    fn test_is_superset_iter() {
        assert::is_true("set([1, 2, 3]).issuperset([3, 1])")
    }

    #[test]
    fn test_is_subset() {
        assert::is_true("set([1, 2]).issubset(set([1, 3, 2]))")
    }

    #[test]
    fn test_is_not_subset() {
        assert::is_false("set([1, 2]).issubset(set([1, 3, 5]))")
    }

    #[test]
    fn test_is_subset_empty_lhs() {
        assert::is_true("set([]).issubset(set([1, 3, 5]))");
        assert::is_true("set([]).issubset(set([]))")
    }

    #[test]
    fn test_is_not_subset_empty_rhs() {
        assert::is_false("set([1, 2]).issubset(set([]))")
    }

    #[test]
    fn test_is_subset_iter() {
        assert::is_true("set([1, 2]).issubset([1, 3, 2])")
    }

    #[test]
    fn test_update() {
        assert::eq(
            "x = set([1, 3, 2]); x.update([4, 3]); list(x)",
            "[1, 3, 2, 4]",
        )
    }

    #[test]
    fn test_update_empty() {
        assert::eq("x = set([1, 3, 2]); x.update([]); list(x)", "[1, 3, 2]");
        assert::eq("x = set(); x.update(set([4, 3])); list(x)", "[4, 3]");
        assert::eq("x = set(); x.update([]); x", "set()");
    }

    #[test]
    fn test_update_self() {
        assert::eq("x = set([1, 3, 2]); x.update(x); list(x)", "[1, 3, 2]")
    }

    #[test]
    fn test_update_frozen_set_cannot_be_updated_with_self() {
        let mut a = Assert::new();
        a.module("s.star", "S = set([1, 2, 3])");
        a.fail(
            r#"
load('s.star', 'S')

S.update(S)
"#,
            "Immutable",
        );
    }
}
