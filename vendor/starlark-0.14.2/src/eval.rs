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

// Modified for native Bazel 9.3 string literal values; see vendor provenance.

//! Evaluate some code, typically done by creating an [`Evaluator`], then calling
//! [`eval_module`](Evaluator::eval_module).

pub(crate) mod bc;
pub(crate) mod compiler;
mod params;
pub(crate) mod runtime;
pub(crate) mod soft_error;

use std::collections::HashMap;
#[cfg(not(target_arch = "wasm32"))]
use std::time::Instant;

use dupe::Dupe;
pub use runtime::arguments::Arguments;
pub use runtime::before_stmt::BeforeStmtFunc;
pub use runtime::before_stmt::BeforeStmtFuncDyn;
pub use runtime::evaluator::Evaluator;
pub use runtime::file_loader::FileLoader;
pub use runtime::file_loader::ReturnFileLoader;
pub use runtime::params::parser::ParametersParser;
pub use runtime::params::spec::ParametersSpec;
pub use runtime::params::spec::ParametersSpecParam;
// Modified (bazel-rs): portable `def` and `lambda` values (compiler::def).
pub use compiler::def::PortableDef;
pub use compiler::def::PortableParamKind;
pub use compiler::def::def_module_is;
pub use compiler::def::def_same_code;
pub use compiler::def::DefDocumentation;
pub use compiler::def::def_documentation;
pub use runtime::profile::data::ProfileData;
pub use runtime::profile::mode::ProfileMode;
pub use soft_error::SoftErrorHandler;
pub use starlark_syntax::call_stack::CallStack;
use starlark_syntax::slice_vec_ext::SliceExt;
use starlark_syntax::syntax::module::AstModule;
use starlark_syntax::syntax::module::AstModuleFields;

use crate::codemap::CodeMap;
use crate::collections::symbol::symbol::Symbol;
use crate::docs::DocString;
use crate::environment::Globals;
use crate::eval::compiler::Compiler;
use crate::eval::compiler::def::DefInfo;
use crate::eval::compiler::scope::ModuleScopes;
use crate::eval::compiler::scope::ScopeId;
use crate::eval::compiler::scope::scope_resolver_globals::ScopeResolverGlobals;
pub use crate::eval::params::param_specs;
use crate::eval::runtime::arguments::ArgNames;
use crate::eval::runtime::arguments::ArgumentsFull;
use crate::eval::runtime::evaluator;
use crate::register_starlark_any;
use crate::syntax::DialectTypes;
use crate::values::Value;

// Register CodeMap for use with StarlarkAny
register_starlark_any!(CodeMap);

impl<'v, 'a, 'e> Evaluator<'v, 'a, 'e> {
    /// Evaluate an [`AstModule`] with this [`Evaluator`], modifying the in-scope
    /// [`Module`](crate::environment::Module) as appropriate.
    pub fn eval_module(&mut self, ast: AstModule, globals: &Globals) -> crate::Result<Value<'v>> {
        #[cfg(not(target_arch = "wasm32"))]
        let start = Instant::now();

        let (codemap, mut statement, dialect, typecheck) = ast.into_parts();
        // Modified: Bazel 9.3 string literal values (raw strings; byte escapes).
        bazel_string_literals(&mut statement, &codemap);

        let codemap = self
            .module_env
            .frozen_heap()
            .alloc_any_value(codemap.dupe());

        let globals = self
            .module_env
            .frozen_heap()
            .alloc_any_value(globals.dupe());

        if let Some(docstring) = DocString::extract_raw_starlark_docstring(&statement) {
            self.module_env.set_docstring(docstring)
        }

        let ModuleScopes {
            cst,
            module_slot_count,
            scope_data,
            top_level_stmt_count,
        } = ModuleScopes::check_module_err(
            self.module_env.mutable_names(),
            self.module_env.frozen_heap(),
            &HashMap::new(),
            statement,
            ScopeResolverGlobals {
                globals: Some(globals),
            },
            codemap,
            &dialect,
        )?;

        self.frozen_heap().add_reference(globals.heap());

        let scope_names = scope_data.get_scope(ScopeId::module());
        let local_names = self.frozen_heap().alloc_any_array_value(&scope_names.used);

        self.module_env.slots().ensure_slots(module_slot_count);
        let old_def_info = self.module_def_info.replace(
            self.module_env
                .frozen_heap()
                .alloc_any_value(DefInfo::for_module(
                    codemap,
                    local_names,
                    self.module_env
                        .frozen_heap()
                        .alloc_any_array_value(&scope_names.parent),
                    globals,
                )),
        );

        self.call_stack.alloc_if_needed(
            self.max_callstack_size
                .unwrap_or(evaluator::DEFAULT_STACK_SIZE),
        )?;

        // Set up the world to allow evaluation (do NOT use ? from now on)

        self.call_stack.push(Value::new_none(), None).unwrap();

        // Evaluation
        let mut compiler = Compiler {
            scope_data,
            locals: Vec::new(),
            globals,
            codemap,
            eval: self,
            check_types: dialect.enable_types == DialectTypes::Enable,
            top_level_stmt_count,
            typecheck,
        };

        let res = compiler.eval_module(cst, local_names);

        // Clean up the world, putting everything back
        self.call_stack.pop();

        self.module_def_info = old_def_info;

        #[cfg(not(target_arch = "wasm32"))]
        self.module_env.add_eval_duration(start.elapsed());

        self.run_infrequent_instr_checks()?;

        // Return the result of evaluation
        res.map_err(|e| e.into_error())
    }

    /// Evaluate a function stored in a [`Value`], passing in `positional` and `named` arguments.
    pub fn eval_function(
        &mut self,
        function: Value<'v>,
        positional: &[Value<'v>],
        named: &[(&str, Value<'v>)],
    ) -> crate::Result<Value<'v>> {
        let names = named.map(|(s, _)| (Symbol::new(s), self.heap().alloc_str(s)));
        let named = named.map(|x| x.1);
        let params = Arguments(ArgumentsFull {
            pos: positional,
            named: &named,
            names: ArgNames::new_check_unique(&names)?,
            args: None,
            kwargs: None,
        });
        self.call_stack.alloc_if_needed(
            self.max_callstack_size
                .unwrap_or(evaluator::DEFAULT_STACK_SIZE),
        )?;
        // eval_module pushes an "empty" call stack frame. other places expect that first frame to be ignorable, and
        // so we push an empty frame too (otherwise things would ignore this function's own frame).
        let res = self.with_call_stack(Value::new_none(), None, |this| {
            function.invoke(&params, this)
        });

        self.run_infrequent_instr_checks()?;

        res
    }
}

// Modified: Bazel 9.3 string literal values
// (net/starlark/java/syntax/Lexer.java:280-430).
//
// * Raw strings keep every backslash with the following character; inside
//   triple quotes or after a backslash-CRLF the line break after a backslash
//   becomes `\n` (Lexer.java:305-320, 463-473). The native lexer drops the
//   backslash before a quote.
// * Outside the Latin-1 char model (BUILD/.bzl analysis evaluation), a literal
//   with an octal escape `\200`..`\377` denotes raw bytes. Bytes that form
//   valid UTF-8 decode to their characters; every other byte `b` becomes the
//   private-use character U+10FF00 + `b` (`bazel_byte_char`), which native
//   action boundaries map back to the byte (Bazel decodes UTF-8 with
//   replacement for process arguments, `StringEncoding.internalToPlatform`, and
//   writes the raw byte for file writes).
fn bazel_string_literals(
    statement: &mut starlark_syntax::syntax::ast::AstStmt,
    codemap: &CodeMap,
) {
    use starlark_syntax::syntax::ast::AstExpr;
    use starlark_syntax::syntax::ast::AstLiteral;
    use starlark_syntax::syntax::ast::ExprP;
    let latin1 = crate::values::types::string::methods::latin1_char_model();
    fn visit(expr: &mut AstExpr, codemap: &CodeMap, latin1: bool) {
        if let ExprP::Literal(AstLiteral::String(literal)) = &mut expr.node {
            if let Some(value) = bazel_string_value(codemap.source_span(literal.span), latin1) {
                literal.node = value;
            }
        }
        expr.visit_expr_mut(|child| visit(child, codemap, latin1));
    }
    statement.visit_expr_mut(|expr| visit(expr, codemap, latin1));
}

/// Bazel's value of one string token, or `None` to keep the native value.
fn bazel_string_value(spelling: &str, latin1: bool) -> Option<String> {
    let (raw, quoted) = match spelling.as_bytes().first() {
        Some(b'r' | b'R') => (true, &spelling[1..]),
        _ => (false, spelling),
    };
    let delimiter = ["'''", "\"\"\"", "'", "\""]
        .into_iter()
        .find(|delimiter| quoted.starts_with(delimiter))?;
    let body = quoted.strip_prefix(delimiter)?.strip_suffix(delimiter)?;
    if raw {
        if delimiter.len() == 1 && !body.contains("\\\r\n") {
            return Some(body.to_owned());
        }
        let mut out = String::with_capacity(body.len());
        let mut chars = body.chars().peekable();
        while let Some(c) = chars.next() {
            out.push(c);
            if c == '\\' {
                match chars.next() {
                    Some('\r') => {
                        if chars.peek() == Some(&'\n') {
                            chars.next();
                        }
                        out.push('\n');
                    }
                    Some(next) => out.push(next),
                    None => {}
                }
            }
        }
        return Some(out);
    }
    if latin1 || !body.contains('\\') {
        return None;
    }
    let mut bytes = Vec::with_capacity(body.len());
    let mut high = false;
    let mut chars = body.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '\\' {
            let mut buffer = [0u8; 4];
            bytes.extend_from_slice(c.encode_utf8(&mut buffer).as_bytes());
            continue;
        }
        match chars.next()? {
            '\r' => {
                if chars.peek() == Some(&'\n') {
                    chars.next();
                }
            }
            '\n' => {}
            'a' => bytes.push(7),
            'b' => bytes.push(8),
            'f' => bytes.push(12),
            'n' => bytes.push(b'\n'),
            'r' => bytes.push(b'\r'),
            't' => bytes.push(b'\t'),
            'v' => bytes.push(11),
            '\\' => bytes.push(b'\\'),
            '\'' => bytes.push(b'\''),
            '"' => bytes.push(b'"'),
            first @ '0'..='7' => {
                let mut value = first as u32 - '0' as u32;
                for _ in 0..2 {
                    match chars.peek() {
                        Some(digit @ '0'..='7') => {
                            value = value * 8 + (*digit as u32 - '0' as u32);
                            chars.next();
                        }
                        _ => break,
                    }
                }
                if value > 0xff {
                    return None;
                }
                high |= value >= 0x80;
                bytes.push(value as u8);
            }
            _ => return None,
        }
    }
    high.then(|| {
        let mut out = String::with_capacity(bytes.len());
        let mut rest = &bytes[..];
        while !rest.is_empty() {
            match std::str::from_utf8(rest) {
                Ok(valid) => {
                    out.push_str(valid);
                    break;
                }
                Err(error) => {
                    let (valid, after) = rest.split_at(error.valid_up_to());
                    out.push_str(std::str::from_utf8(valid).unwrap_or_default());
                    let invalid = error.error_len().unwrap_or(after.len());
                    for byte in &after[..invalid] {
                        out.push(bazel_byte_char(*byte));
                    }
                    rest = &after[invalid..];
                }
            }
        }
        out
    })
}

/// The private-use character standing for the lone byte `byte` of a Bazel
/// analysis-model string (U+10FF00 + byte).
pub fn bazel_byte_char(byte: u8) -> char {
    char::from_u32(0x10_ff00 + u32::from(byte)).unwrap_or(char::REPLACEMENT_CHARACTER)
}
