//! The `proto` toplevel module (packages/Proto.java, Bazel 9.3.1): a BUILD,
//! `.bzl` and cquery global (StarlarkGlobalsImpl.addCommonUtilToplevels,
//! analysis/starlark/StarlarkGlobalsImpl.java:49-55) whose one method,
//! `proto.encode_text(x)`, encodes a struct or provider instance as a
//! text-format protocol message (Proto.TextEncoder, :109-229). rules_cc's
//! rule-based toolchain tests compare features through it.
use allocative::Allocative;
use starlark::environment::GlobalsBuilder;
use starlark::eval::{Arguments, Evaluator};
use starlark::values::dict::DictRef;
use starlark::values::float::StarlarkFloat;
use starlark::values::list::ListRef;
use starlark::values::range::Range;
use starlark::values::structs::StructRef;
use starlark::values::tuple::TupleRef;
use starlark::values::{Heap, Value, ValueLike};
use std::fmt;

/// Nesting bound of the encoder (a cycle through a mutable list inside a
/// struct would otherwise recurse forever; Java overflows its stack).
const MAX_DEPTH: usize = 1000;

#[allow(unsafe_code)]
mod values {
    use super::*;
    use starlark::values::{
        NoSerialize, ProvidesStaticType, StarlarkPagablePanic, StarlarkValue, starlark_value,
    };
    #[derive(Debug, ProvidesStaticType, NoSerialize, Allocative, StarlarkPagablePanic)]
    pub(super) struct ProtoModule;
    #[derive(Debug, ProvidesStaticType, NoSerialize, Allocative, StarlarkPagablePanic)]
    pub(super) struct EncodeText;
    starlark::starlark_simple_value!(ProtoModule);
    starlark::starlark_simple_value!(EncodeText);
    impl fmt::Display for ProtoModule {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.write_str("<proto>")
        }
    }
    impl fmt::Display for EncodeText {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.write_str("<built-in method encode_text of proto value>")
        }
    }
    #[starlark_value(type = "proto")]
    impl<'v> StarlarkValue<'v> for ProtoModule {
        fn get_attr(&self, name: &str, heap: Heap<'v>) -> Option<Value<'v>> {
            (name == "encode_text").then(|| heap.alloc(EncodeText))
        }
        fn has_attr(&self, name: &str, _heap: Heap<'v>) -> bool {
            name == "encode_text"
        }
        fn dir_attr(&self) -> Vec<String> {
            vec!["encode_text".to_owned()]
        }
        fn equals(&self, other: Value<'v>) -> starlark::Result<bool> {
            Ok(other.downcast_ref::<Self>().is_some())
        }
    }
    #[starlark_value(type = "builtin_function_or_method")]
    impl<'v> StarlarkValue<'v> for EncodeText {
        fn invoke(
            &self,
            _me: Value<'v>,
            args: &Arguments<'v, '_>,
            eval: &mut Evaluator<'v, '_, '_>,
        ) -> starlark::Result<Value<'v>> {
            let heap = eval.heap();
            let x = args.positional1(heap).or_else(|_| {
                let named = args.names_map()?;
                match (args.positions(heap)?.count(), named.len()) {
                    (0, 1) if named.contains_key("x") => {
                        Ok(*named.values().next().expect("one named argument"))
                    }
                    _ => Err(starlark::Error::new_other(anyhow::anyhow!(
                        "encode_text() accepts exactly one argument, x"
                    ))),
                }
            })?;
            if !is_structure(x) {
                return Err(starlark::Error::new_other(anyhow::anyhow!(
                    "in call to encode_text(), parameter 'x' got value of type '{}', want 'structure or NativeInfo'",
                    x.get_type()
                )));
            }
            let mut encoder = Encoder {
                heap,
                out: String::new(),
                indent: 0,
                depth: 0,
            };
            encoder
                .message(x)
                .map_err(|error| starlark::Error::new_other(anyhow::anyhow!(error)))?;
            Ok(heap.alloc(encoder.out))
        }
    }
}
use values::ProtoModule;

pub(crate) fn install(globals: &mut GlobalsBuilder) {
    globals.set("proto", ProtoModule);
}

/// A Structure: a `struct` or a provider instance.
fn is_structure(value: Value<'_>) -> bool {
    StructRef::from_value(value).is_some() || crate::providers::json_fields(value).is_some()
}

/// The fields of a Structure, sorted by name (Ordering.natural over dir()).
fn fields<'v>(value: Value<'v>) -> Vec<(String, Value<'v>)> {
    let mut fields: Vec<(String, Value<'v>)> = match StructRef::from_value(value) {
        Some(structure) => structure
            .iter()
            .map(|(name, value)| (name.as_str().to_owned(), value))
            .collect(),
        None => crate::providers::json_fields(value)
            .map(|fields| {
                fields
                    .map(|(name, value)| (name.to_owned(), value))
                    .collect()
            })
            .unwrap_or_default(),
    };
    fields.sort_by(|a, b| crate::native_rules::java_string_cmp(&a.0, &b.0));
    fields
}

fn sequence<'v>(value: Value<'v>, heap: Heap<'v>) -> Option<Vec<Value<'v>>> {
    if let Some(list) = ListRef::from_value(value) {
        return Some(list.iter().collect());
    }
    if let Some(tuple) = TupleRef::from_value(value) {
        return Some(tuple.iter().collect());
    }
    // StarlarkRange is a Sequence too.
    if value.downcast_ref::<Range>().is_some() {
        return value.iterate(heap).ok().map(|items| items.collect());
    }
    None
}

struct Encoder<'v> {
    heap: Heap<'v>,
    out: String,
    indent: usize,
    depth: usize,
}

impl<'v> Encoder<'v> {
    fn message(&mut self, value: Value<'v>) -> Result<(), String> {
        self.depth += 1;
        if self.depth > MAX_DEPTH {
            return Err("proto.encode_text: nesting too deep".to_owned());
        }
        for (name, field) in fields(value) {
            self.field(&name, field)
                .map_err(|error| format!("in {} field .{name}: {error}", value.get_type()))?;
        }
        self.depth -= 1;
        Ok(())
    }

    fn field(&mut self, name: &str, value: Value<'v>) -> Result<(), String> {
        if let Some(dict) = DictRef::from_value(value) {
            for (key, entry) in dict.iter() {
                if key.unpack_str().is_none() && !is_int(key) {
                    return Err(format!(
                        "invalid dict key: got {}, want int or string",
                        key.get_type()
                    ));
                }
                self.line(&[name, " {"]);
                self.indent += 1;
                self.element("key", key)?;
                self.element("value", entry)
                    .map_err(|error| format!("in value for dict key {}: {error}", key.to_repr()))?;
                self.indent -= 1;
                self.line(&["}"]);
            }
            return Ok(());
        }
        if let Some(items) = sequence(value, self.heap) {
            for (index, item) in items.into_iter().enumerate() {
                self.element(name, item)
                    .map_err(|error| format!("at {} index {index}: {error}", value.get_type()))?;
            }
            return Ok(());
        }
        if value.is_none() {
            return Ok(());
        }
        self.element(name, value)
    }

    fn element(&mut self, name: &str, value: Value<'v>) -> Result<(), String> {
        if is_structure(value) {
            self.line(&[name, " {"]);
            self.indent += 1;
            self.message(value)?;
            self.indent -= 1;
            self.line(&["}"]);
        } else if let Some(text) = value.unpack_str() {
            let escaped = text
                .replace('\\', "\\\\")
                .replace('"', "\\\"")
                .replace('\n', "\\n");
            self.line(&[name, ": \"", &escaped, "\""]);
        } else if let Some(flag) = value.unpack_bool() {
            self.line(&[name, ": ", if flag { "true" } else { "false" }]);
        } else if is_int(value) {
            self.line(&[name, ": ", &value.to_str()]);
        } else if value.downcast_ref::<StarlarkFloat>().is_some() {
            // StarlarkFloat.toString; "+inf" is written "inf".
            let text = value.to_str();
            let text = if text == "+inf" {
                "inf".to_owned()
            } else {
                text
            };
            self.line(&[name, ": ", &text]);
        } else {
            return Err(format!(
                "got {}, want string, int, float, bool, or struct",
                value.get_type()
            ));
        }
        Ok(())
    }

    fn line(&mut self, items: &[&str]) {
        for _ in 0..self.indent {
            self.out.push_str("  ");
        }
        for item in items {
            self.out.push_str(item);
        }
        self.out.push('\n');
    }
}

fn is_int(value: Value<'_>) -> bool {
    value.unpack_i32().is_some() || value.unpack_bigint_ref().is_some()
}
