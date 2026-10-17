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

// Modified for native Bazel 9.3 frozen-mutation messages and `dict | select`, and
// repr/type/hashability compatibility, and Bazel's operator, call, iteration,
// slice, field-assignment and unknown-field texts; see vendor provenance.

//! Define a common set of errors.

use thiserror::Error;

use crate::values::StarlarkValue;
use crate::values::Value;

/// Common errors returned by Starlark evaluation.
#[derive(Debug, Error)]
#[allow(missing_docs)] // Self-explanatory.
pub enum ValueError {
    /// Modified: Bazel 9.3 texts for the operations Bazel names (see
    /// [`bazel_unary_message`]); other operations keep the original text.
    #[error("{}", bazel_unary_message(.op, .typ))]
    OperationNotSupported { op: String, typ: String },
    /// Modified: Bazel 9.3 texts (see [`bazel_binary_message`]).
    #[error("{}", bazel_binary_message(.op, .left, .right))]
    OperationNotSupportedBinary {
        op: String,
        left: String,
        right: String,
    },
    #[error("Cannot divide by zero")]
    DivisionByZero,
    #[error("Integer overflow")]
    IntegerOverflow,
    #[error("Negative shift count")]
    NegativeShiftCount,
    #[error("Type of parameters mismatch")]
    IncorrectParameterType,
    #[error("Type of parameter `{0}` doesn't match")]
    IncorrectParameterTypeNamed(String),
    #[error("Missing this parameter")]
    MissingThis,
    #[error("Missing required parameter `{0}`")]
    MissingRequired(String),
    #[error("Index `{0}` is out of bound")]
    IndexOutOfBound(i32),
    #[error("Key `{0}` was not found")]
    KeyNotFound(String),
    #[error("Immutable")]
    CannotMutateImmutableValue,
    /// Modified: Bazel 9.3 `Starlark.checkMutable` text for a frozen
    /// list/dict (net/starlark/java/eval/Starlark.java:262-265).
    #[error("trying to mutate a frozen {0} value")]
    CannotMutateFrozenValue(&'static str),
    #[error("This operation mutates an iterable for an iterator while iterating.")]
    MutationDuringIteration,
    /// Modified: Bazel 9.3 `Starlark.getattr` text
    /// (net/starlark/java/eval/Starlark.java:1014-1016).
    #[error("'{0}' value has no field or method '{1}'")]
    NoAttr(String, String),
    /// Modified: with `SpellChecker.didYouMean`'s suffix.
    #[error("'{0}' value has no field or method '{1}' (did you mean '{2}'?)")]
    NoAttrDidYouMean(String, String, String),
}

/// Modified: the parameter type error of `UnpackValue::unpack_named_param`;
/// the last field is the value's type, for Bazel's text (see
/// `IncorrectParameterTypeNamedWithExpected::bazel_message`).
#[derive(thiserror::Error, Debug)]
#[error("Type of parameter `{0}` doesn't match, expected `{1}`, actual `{2}`")]
pub(crate) struct IncorrectParameterTypeNamedWithExpected(
    pub(crate) String,
    pub(crate) crate::typing::Ty,
    pub(crate) String,
    pub(crate) String,
);

impl IncorrectParameterTypeNamedWithExpected {
    /// Modified: Bazel's `in call to NAME(), parameter 'P' got value of type
    /// 'T', want 'W'` (net/starlark/java/eval/BuiltinFunction.java:404-410),
    /// with `W` from the expected type in ParamDescriptor.getTypeErrorMessage's
    /// `a`, `a or b`, `a, b, or c` form (ParamDescriptor.java:122-145), using
    /// Bazel's names for `str` (`string`), `None` (`NoneType`) and lists
    /// (`sequence`, the usual Bazel parameter class), `string` first and
    /// `NoneType` last.
    /// `None` when the value has an expected container type but its elements
    /// do not match (Bazel checks those in the builtin itself); the original
    /// text is kept then.
    pub(crate) fn bazel_message(&self, function: &str) -> Option<String> {
        let classes = bazel_classes(&self.1);
        if classes.iter().any(|c| c == &self.3) {
            return None;
        }
        Some(format!(
            "in call to {function}(), parameter '{}' got value of type '{}', want '{}'",
            self.0,
            self.3,
            bazel_want(&classes)
        ))
    }
}

/// Modified: the members of a starlark-rust type as Bazel type names: `str` ->
/// `string`, `None` -> `NoneType`, otherwise the base name (`list[str]` ->
/// `list`).
fn bazel_classes(ty: &crate::typing::Ty) -> Vec<String> {
    let expected = ty.to_string();
    let mut atoms: Vec<&str> = Vec::new();
    let mut depth = 0usize;
    let mut start = 0;
    for (i, b) in expected.bytes().enumerate() {
        match b {
            b'[' | b'(' => depth += 1,
            b']' | b')' => depth = depth.saturating_sub(1),
            b'|' if depth == 0 => {
                atoms.push(expected[start..i].trim());
                start = i + 1;
            }
            _ => {}
        }
    }
    atoms.push(expected[start..].trim());
    atoms
        .iter()
        .map(|atom| match atom.split('[').next().unwrap_or(atom) {
            "str" => "string".to_owned(),
            "None" => "NoneType".to_owned(),
            base => base.to_owned(),
        })
        .collect()
}

/// Modified: ParamDescriptor.getTypeErrorMessage's `a`, `a or b`, `a, b, or
/// c` form (ParamDescriptor.java:122-145) of starlark-rust type members, with
/// Bazel's names for lists (`sequence`, the usual Bazel parameter class) and
/// iterables (`iterable`), `string` first and `NoneType` last (starlark-rust
/// sorts union members; Bazel lists the declared allowedTypes, which
/// conventionally start with `string` and end with `NoneType`, e.g. Label's
/// `string or Label`).
fn bazel_want(classes: &[String]) -> String {
    let mut names: Vec<&str> = Vec::new();
    for class in classes {
        let name = match class.as_str() {
            "list" => "sequence",
            "typing.Iterable" => "iterable",
            other => other,
        };
        if !names.contains(&name) {
            names.push(name);
        }
    }
    names.sort_by_key(|name| match *name {
        "string" => 0,
        "NoneType" => 2,
        _ => 1,
    });
    join_bazel_types(&names)
}

fn join_bazel_types(names: &[&str]) -> String {
    match names {
        [one] => (*one).to_owned(),
        [a, b] => format!("{a} or {b}"),
        [init @ .., last] => format!("{}, or {last}", init.join(", ")),
        [] => String::new(),
    }
}

/// Modified: Bazel's declared allowedTypes of builtin parameters whose
/// starlark-rust type is unchecked (`Value`), in Bazel's order; an empty list
/// means Bazel does not check the class (MethodLibrary.java:403-416 `len`,
/// :697-710 `enumerate`; collect/nestedset/Depset.java:572-627 `depset`, a
/// bazel-rs builtin with unchecked parameters).
const BAZEL_PARAM_TYPES: &[(&str, &str, &[&str])] = &[
    ("len", "x", &["iterable", "string"]),
    ("enumerate", "list", &[]),
    ("depset", "direct", &["sequence", "NoneType"]),
    ("depset", "order", &["string"]),
    ("depset", "transitive", &["sequence", "NoneType"]),
];

fn bazel_class_matches(class: &str, value_type: &str) -> bool {
    match class {
        "iterable" => matches!(value_type, "list" | "tuple" | "dict" | "set" | "range"),
        "sequence" => matches!(value_type, "list" | "tuple" | "range"),
        _ => class == value_type,
    }
}

/// Modified: `BuiltinFunction.checkParamValue` (BuiltinFunction.java:400-412)
/// for parameter `param` of builtin `function` with starlark-rust type `ty`:
/// the Bazel text if `value`'s class is not allowed, `None` if it is or the
/// type is unchecked (`typing.Any`, callables, ...).
pub(crate) fn bazel_param_type_mismatch(
    function: &str,
    param: &str,
    ty: &crate::typing::Ty,
    value: Value,
) -> Option<String> {
    let value_type = value.get_type();
    let want = match BAZEL_PARAM_TYPES
        .iter()
        .find(|(f, p, _)| *f == function && *p == param)
    {
        Some((_, _, allowed)) => {
            if allowed.is_empty() || allowed.iter().any(|c| bazel_class_matches(c, value_type)) {
                return None;
            }
            join_bazel_types(allowed)
        }
        None => {
            let classes = bazel_classes(ty);
            let checked: Vec<&str> = classes
                .iter()
                .map(|c| match c.as_str() {
                    "list" => "sequence",
                    "typing.Iterable" => "iterable",
                    other => other,
                })
                .collect();
            if checked
                .iter()
                .any(|c| c.starts_with("typing.") || c.starts_with('_') || c.is_empty())
                || checked.iter().any(|c| bazel_class_matches(c, value_type))
            {
                return None;
            }
            bazel_want(&classes)
        }
    };
    Some(format!(
        "in call to {function}(), parameter '{param}' got value of type '{value_type}', want '{want}'"
    ))
}

/// Modified: Bazel's builtin parameter type error
/// (net/starlark/java/eval/BuiltinFunction.java:404-410).
#[cold]
pub(crate) fn bazel_param_type_error(
    function: &str,
    param: &str,
    value: Value,
    want: &str,
) -> crate::Error {
    crate::Error::new_value(anyhow::anyhow!(
        "in call to {function}(), parameter '{param}' got value of type '{}', want '{want}'",
        value.get_type()
    ))
}

/// Modified: Bazel 9.3 texts of a unary operation `op` unsupported on `typ`:
/// `-x`/`+x`/`~x` (EvalUtils.java:472), calls (Starlark.java:904), iteration
/// (Starlark.java:284), `len` (its `x` parameter type check,
/// MethodLibrary.java:403-416 with BuiltinFunction.java:404-410), slicing
/// (Starlark.java:672) and field assignment (EvalUtils.java:526-535, with
/// Structure.java:79-81 for `struct`).
fn bazel_unary_message(op: &str, typ: &str) -> String {
    match op {
        "-" | "+" | "~" => format!("unsupported unary operation: {op}{typ}"),
        "call()" => format!("'{typ}' object is not callable"),
        "(iter)" => format!("type '{typ}' is not iterable"),
        "len()" => format!(
            "in call to len(), parameter 'x' got value of type '{typ}', want 'iterable or string'"
        ),
        "[::]" => format!("invalid slice operand: {typ}"),
        _ => match op.strip_prefix('.').and_then(|op| op.strip_suffix('=')) {
            Some(_) if typ == "struct" => {
                "struct value does not support field assignment".to_owned()
            }
            Some(field) if !field.is_empty() => {
                format!("cannot set .{field} field of {typ} value")
            }
            _ => format!("Operation `{op}` not supported on type `{typ}`"),
        },
    }
}

/// Modified: Bazel 9.3 texts of a binary operation unsupported for `left` and
/// `right`: comparisons (Starlark.java:517), operators including `in`
/// (EvalUtils.java:416-417) and indexing (`EvalUtils.index`,
/// EvalUtils.java:498-500).
fn bazel_binary_message(op: &str, left: &str, right: &str) -> String {
    match op {
        "compare" | "cmp()" | "<>" => format!("unsupported comparison: {left} <=> {right}"),
        "+" | "-" | "*" | "/" | "//" | "%" | "|" | "&" | "^" | "<<" | ">>" | "in" => {
            format!("unsupported binary operation: {left} {op} {right}")
        }
        "[]" => format!("type '{left}' has no operator []({right})"),
        _ => format!("Operation `{op}` not supported for types `{left}` and `{right}`"),
    }
}

impl From<ValueError> for crate::Error {
    fn from(e: ValueError) -> Self {
        crate::Error::new_kind(crate::ErrorKind::Value(anyhow::Error::new(e)))
    }
}

#[derive(Debug, Error)]
pub(crate) enum ControlError {
    // Modified: Bazel 9.3 `StarlarkValue.checkHashable`
    // (net/starlark/java/eval/StarlarkValue.java:120).
    #[error("unhashable type: '{0}'")]
    NotHashableValue(String),
    #[error("Too many recursion levels")]
    TooManyRecursionLevel,
}

impl ValueError {
    #[cold]
    pub(crate) fn unsupported_owned<T>(
        left: &str,
        op: &str,
        right: Option<&str>,
    ) -> crate::Result<T> {
        match right {
            None => Err(ValueError::OperationNotSupported {
                op: op.to_owned(),
                typ: left.to_owned(),
            }
            .into()),
            Some(right) => Err(ValueError::OperationNotSupportedBinary {
                op: op.to_owned(),
                left: left.to_owned(),
                right: right.to_owned(),
            }
            .into()),
        }
    }

    /// Helper to create an [`OperationNotSupported`](ValueError::OperationNotSupported) error.
    #[cold]
    pub fn unsupported<'v, T, V: StarlarkValue<'v>>(_left: &V, op: &str) -> crate::Result<T> {
        Self::unsupported_owned(V::TYPE, op, None)
    }

    #[cold]
    pub(crate) fn unsupported_type<T>(left: Value, op: &str) -> crate::Result<T> {
        Self::unsupported_owned(left.get_type(), op, None)
    }

    /// Helper to create an [`OperationNotSupported`](ValueError::OperationNotSupportedBinary) error.
    #[cold]
    pub fn unsupported_with<'v, T, V: StarlarkValue<'v>>(
        _left: &V,
        op: &str,
        right: Value,
    ) -> crate::Result<T> {
        Self::unsupported_owned(V::TYPE, op, Some(right.get_type()))
    }
}
