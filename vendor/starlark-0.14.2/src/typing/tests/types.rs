/*
 * Copyright 2019 The Starlark in Rust Authors.
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

//! Type-related operations.

use crate::assert::Assert;
use crate::typing::tests::TypeCheck;
use crate::typing::tests::register_typecheck_globals;

#[test]
fn test_type_alias() {
    TypeCheck::new().ty("x").check(
        "type_alias",
        r#"
MyList = list[int]

def f(x: MyList):
    pass
"#,
    );
}

#[test]
fn test_incorrect_type_dot() {
    TypeCheck::new().check(
        "incorrect_type_dot",
        r#"
def foo(x: list.foo.bar):
    pass
"#,
    );
}

#[test]
fn test_function_as_type_bit_or() {
    TypeCheck::new().ty("t").check(
        "function_as_type_bit_or",
        r#"
def test():
    # This test should work even if `t` is global. There's a bug in test framework somewhere.
    t = int | str
"#,
    );
}

#[test]
fn test_function_as_type_parameterize() {
    let a = Assert::new();
    a.fail(
        r#"
def f(x: str[int]):
    pass
"#,
        "not supported",
    );
}

#[test]
fn test_starlark_value_as_type_unsupported_param() {
    let mut a = Assert::new();
    a.globals_add(register_typecheck_globals);
    a.fail(
        r#"
def f(x: MyCustomType[int]):
    pass
"#,
        "does not support type parameters",
    );
}

#[test]
fn test_list_parametrize() {
    let a = Assert::new();
    a.pass(
        r#"
def f(x: list[str]) -> list[str]:
    return x
"#,
    );
}

#[test]
fn test_set_parametrize() {
    let a = Assert::new();
    // Modified: Bazel's set constructor is callable, not a parametric type.
    a.fail(
        r#"
def f(x: set[int]) -> set[int]:
    return x
"#,
        "Operation `[]` not supported for types `builtin_function_or_method` and `type`",
    );
}

#[test]
fn test_list_too_many_type_params() {
    let a = Assert::new();
    a.fail(
        r#"
def f(x: list[str, int]):
    pass
"#,
        "[,] can only be applied to dict or tuple functions in type expression",
    );
}

#[test]
fn test_set_constructor_static_signature() {
    use crate::environment::GlobalsBuilder;
    use crate::typing::{ParamSpec, Ty};

    let globals = GlobalsBuilder::extended().build();
    let constructor = globals.get("set").unwrap();
    assert_eq!(
        Ty::of_value(constructor),
        Ty::function(
            ParamSpec::pos_only([], [Ty::iter(Ty::any())]),
            Ty::any_set(),
        ),
    );
    assert_eq!(constructor.get_type(), "builtin_function_or_method");
}

#[test]
fn test_set_constructor_static_rejections() {
    use crate::environment::GlobalsBuilder;
    use crate::syntax::{AstModule, Dialect};
    use crate::typing::AstModuleTypecheck;

    let globals = GlobalsBuilder::extended().build();
    for code in [
        "def f() -> int:\n    return set()",
        "ctor = set\ndef f() -> int:\n    return ctor()",
        "def f():\n    return set(1)",
        "def f():\n    return set([], [])",
        "def f():\n    return set(elements=[])",
        "def f():\n    return set(arg=[])",
        "def f():\n    return set(unknown=[])",
    ] {
        // No function is called: rejection must be static, not a runtime error.
        let ast = AstModule::parse(
            "set_types.star",
            code.to_owned(),
            &Dialect::AllOptionsInternal,
        )
        .unwrap();
        let (errors, _, _, _) = ast.typecheck(&globals, &Default::default());
        assert_eq!(errors.len(), 1, "expected one static rejection: {code}");
    }
    Assert::new().fail(
        "def f() -> int:\n    return set()",
        "Expected type `int` but got `set[typing.Any]`",
    );
}

#[test]
fn test_set_constructor_static_valid_calls() {
    use crate::environment::GlobalsBuilder;
    use crate::syntax::{AstModule, Dialect};
    use crate::typing::AstModuleTypecheck;

    let globals = GlobalsBuilder::extended().build();
    let code = r#"
def f():
    empty = set()
    from_list = set([1, 2])
    from_tuple = set((1, 2))
    from_dict = set({1: 2})
    from_range = set(range(3))
    from_set = set(empty)
    return [empty, from_list, from_tuple, from_dict, from_range, from_set]
"#;
    let ast = AstModule::parse(
        "set_types.star",
        code.to_owned(),
        &Dialect::AllOptionsInternal,
    )
    .unwrap();
    let (errors, _, _, _) = ast.typecheck(&globals, &Default::default());
    assert!(errors.is_empty(), "valid constructor calls rejected");
    Assert::new().pass(code);
}
