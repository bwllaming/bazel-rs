// Indentation algorithm adapted from Bazel Json.java:
// Copyright 2020 The Bazel Authors. All rights reserved.
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
// http://www.apache.org/licenses/LICENSE-2.0
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

//! Bounded native JSON. Limits are a native capacity profile, not JVM limits,
//! an absolute process-memory limit, or a CPU sandbox. One operation owns one
//! cumulative HostBudget, including encode_indent's intermediate and final text.
//! Decode's candidate metadata profile is 256 MiB weighted / 250,000 nodes with
//! an 8 MiB effective UTF-8 input ceiling. Encoding/formatting keep the legacy
//! 64 MiB / 100,000 profile. All weights, charges and depth 64 remain shared;
//! decoding a value does not promise it fits the encoding profile. These candidate
//! ceilings require measurement and parent review before production approval.
use allocative::Allocative;
use starlark::environment::GlobalsBuilder;
use starlark::eval::{Arguments, Evaluator, ParametersSpec, ParametersSpecParam};
use starlark::typing::Ty;
use starlark::values::dict::{AllocDict, DictRef};
use starlark::values::float::StarlarkFloat;
use starlark::values::list::{AllocList, ListRef};
use starlark::values::range::Range;
use starlark::values::set::SetRef;
use starlark::values::structs::StructRef;
use starlark::values::tuple::TupleRef;
use starlark::values::{FrozenValue, Heap, UnpackValue, Value, ValueLike};
use std::fmt;
use std::sync::OnceLock;

#[path = "json_float.rs"]
mod float;

#[derive(Debug)]
enum Error {
    Syntax(usize, &'static str),
    Depth,
    Resource,
    Internal(&'static str),
    Unsupported(&'static str),
}
impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Syntax(offset, message) => write!(f, "at offset {offset}, {message}"),
            Self::Depth => f.write_str("JSON maximum open container depth exceeded (64)"),
            Self::Resource => f.write_str("JSON host preparation resource limit exceeded"),
            Self::Internal(message) => write!(f, "JSON internal error: {message}"),
            Self::Unsupported(message) => f.write_str(message),
        }
    }
}
impl std::error::Error for Error {}
type Result<T> = std::result::Result<T, Error>;

const DECODE_MAX_BYTES: usize = 256 * 1024 * 1024;
const DECODE_MAX_NODES: usize = 250_000;
const DECODE_MAX_INPUT: usize = 8 * 1024 * 1024;

#[derive(Default)]
struct JsonBudget {
    host: crate::repository_budget::HostBudget,
    #[cfg(test)]
    observed: EncoderWork,
}
// Per-operation test observations; no instrumentation or global state in the
// production codec. These distinguish preflight rejection from eventual errors.
#[cfg(test)]
#[derive(Default, Debug)]
struct EncoderWork {
    key_vectors: usize,
    key_visits: usize,
    sort_comparisons: usize,
    quoted_bytes_scanned: usize,
    numeric_conversions: usize,
}
impl JsonBudget {
    fn for_method(method: Method) -> Self {
        if matches!(method, Method::Decode) {
            Self {
                host: crate::repository_budget::HostBudget::with_ceilings(
                    DECODE_MAX_BYTES,
                    DECODE_MAX_NODES,
                ),
                #[cfg(test)]
                observed: EncoderWork::default(),
            }
        } else {
            Self::default()
        }
    }

    fn bytes(&mut self, bytes: usize) -> Result<()> {
        self.host.bytes(bytes).map_err(|_| Error::Resource)
    }
    fn node(&mut self) -> Result<()> {
        self.host.node().map_err(|_| Error::Resource)
    }
    fn array(&mut self, count: usize, size: usize) -> Result<()> {
        self.bytes(count.checked_mul(size).ok_or(Error::Resource)?)
    }
    fn append(&mut self, out: &mut String, text: &str) -> Result<()> {
        self.bytes(text.len())?;
        out.try_reserve(text.len()).map_err(|_| Error::Resource)?;
        out.push_str(text);
        Ok(())
    }
    fn open(&mut self, depth: usize) -> Result<()> {
        if depth >= 64 {
            return Err(Error::Depth);
        }
        self.bytes(256)
    }
}

#[derive(Clone, Copy, Debug, Allocative)]
enum Method {
    Encode,
    Decode,
    Indent,
    EncodeIndent,
}
impl Method {
    fn from_name(name: &str) -> Option<Self> {
        match name {
            "encode" => Some(Self::Encode),
            "decode" => Some(Self::Decode),
            "indent" => Some(Self::Indent),
            "encode_indent" => Some(Self::EncodeIndent),
            _ => None,
        }
    }
    fn name(self) -> &'static str {
        match self {
            Self::Encode => "encode",
            Self::Decode => "decode",
            Self::Indent => "indent",
            Self::EncodeIndent => "encode_indent",
        }
    }
    fn signature(self) -> &'static (ParametersSpec<FrozenValue>, Ty) {
        static ENCODE: OnceLock<(ParametersSpec<FrozenValue>, Ty)> = OnceLock::new();
        static DECODE: OnceLock<(ParametersSpec<FrozenValue>, Ty)> = OnceLock::new();
        static INDENT: OnceLock<(ParametersSpec<FrozenValue>, Ty)> = OnceLock::new();
        static ENCODE_INDENT: OnceLock<(ParametersSpec<FrozenValue>, Ty)> = OnceLock::new();
        let cell = match self {
            Self::Encode => &ENCODE,
            Self::Decode => &DECODE,
            Self::Indent => &INDENT,
            Self::EncodeIndent => &ENCODE_INDENT,
        };
        cell.get_or_init(|| {
            let primary = if matches!(self, Self::Decode | Self::Indent) {
                Ty::string()
            } else {
                Ty::any()
            };
            let defaults = if matches!(self, Self::Decode) {
                vec![("default", ParametersSpecParam::Optional, Ty::any())]
            } else {
                vec![]
            };
            let named = if matches!(self, Self::Indent | Self::EncodeIndent) {
                vec![
                    ("prefix", ParametersSpecParam::Optional, Ty::string()),
                    ("indent", ParametersSpecParam::Optional, Ty::string()),
                ]
            } else {
                vec![]
            };
            let (binding, params) = starlark::eval::param_specs(
                self.name(),
                [(
                    if matches!(self, Self::Indent) {
                        "s"
                    } else {
                        "x"
                    },
                    ParametersSpecParam::Required,
                    primary,
                )],
                defaults,
                None,
                named,
                None,
            )
            .expect("fixed JSON signature");
            (
                binding,
                Ty::function(
                    params,
                    if matches!(self, Self::Decode) {
                        Ty::any()
                    } else {
                        Ty::string()
                    },
                ),
            )
        })
    }
}

#[allow(unsafe_code)]
mod values {
    use super::*;
    use starlark::values::{
        NoSerialize, ProvidesStaticType, StarlarkPagablePanic, StarlarkValue, starlark_value,
    };
    #[derive(Debug, ProvidesStaticType, NoSerialize, Allocative, StarlarkPagablePanic)]
    pub(super) struct JsonModule;
    #[derive(Debug, ProvidesStaticType, NoSerialize, Allocative, StarlarkPagablePanic)]
    pub(super) struct JsonMethod(pub Method);
    starlark::starlark_simple_value!(JsonModule);
    starlark::starlark_simple_value!(JsonMethod);
    impl fmt::Display for JsonModule {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.write_str("<unknown object net.starlark.java.lib.json.Json>")
        }
    }
    impl fmt::Display for JsonMethod {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            write!(f, "<built-in method {} of json value>", self.0.name())
        }
    }
    #[starlark_value(type = "json")]
    impl<'v> StarlarkValue<'v> for JsonModule {
        // compile_time_getattr only folds MemberOrValue::Value if unpack_frozen
        // succeeds. These fresh allocations use the temporary *mutable* heap,
        // never FrozenHeap; speculative reads therefore cannot become constants.
        fn get_attr(&self, name: &str, heap: Heap<'v>) -> Option<Value<'v>> {
            Method::from_name(name).map(|method| heap.alloc(JsonMethod(method)))
        }
        fn has_attr(&self, name: &str, _heap: Heap<'v>) -> bool {
            Method::from_name(name).is_some()
        }
        fn dir_attr(&self) -> Vec<String> {
            ["decode", "encode", "encode_indent", "indent"]
                .map(str::to_owned)
                .to_vec()
        }
        fn attr_ty(name: &str) -> Option<Ty> {
            Method::from_name(name).map(|m| m.signature().1.clone())
        }
        fn equals(&self, other: Value<'v>) -> starlark::Result<bool> {
            Ok(other.downcast_ref::<Self>().is_some())
        }
    }
    #[starlark_value(type = "builtin_function_or_method")]
    impl<'v> StarlarkValue<'v> for JsonMethod {
        // Default equality is pointer identity, including saved/frozen aliases.
        // Default hashing rejects both module and method values.
        fn typechecker_ty(&self) -> Option<Ty> {
            Some(self.0.signature().1.clone())
        }
        fn invoke(
            &self,
            _me: Value<'v>,
            args: &Arguments<'v, '_>,
            eval: &mut Evaluator<'v, '_, '_>,
        ) -> starlark::Result<Value<'v>> {
            // Select once, before argument charges. Helpers never reset this
            // cumulative budget, in particular encode_indent's two stages.
            let mut budget = JsonBudget::for_method(self.0);
            budget
                .array(args.len()?, 128)
                .map_err(starlark::Error::new_other)?;
            let heap = eval.heap();
            let spec = &self.0.signature().0;
            let result = match self.0 {
                Method::Encode => {
                    let [x] = spec.collect_into::<1>(args, heap)?;
                    encode(
                        required(x).map_err(starlark::Error::new_other)?,
                        heap,
                        &mut budget,
                    )
                    .and_then(|s| allocate_text(s, heap, &mut budget))
                }
                Method::Decode => {
                    let [x, default] = spec.collect_into::<2>(args, heap)?;
                    let x = string_arg(required(x).map_err(starlark::Error::new_other)?)
                        .map_err(starlark::Error::new_other)?;
                    let byte_strings = crate::extension_ctx::evaluating_callback(eval)
                        || crate::repository_ctx::evaluating_callback(eval)
                        || eval
                            .extra
                            .and_then(|extra| {
                                extra.downcast_ref::<crate::loading::ExtensionContext>()
                            })
                            .is_some_and(|context| context.byte_strings);
                    match decode(x, heap, &mut budget, byte_strings) {
                        Err(Error::Syntax(..) | Error::Depth) if default.is_some() => Ok(default
                            .ok_or(Error::Internal("missing default"))
                            .map_err(starlark::Error::new_other)?),
                        result => result,
                    }
                }
                Method::Indent | Method::EncodeIndent => {
                    let [x, prefix, indent] = spec.collect_into::<3>(args, heap)?;
                    let x = required(x).map_err(starlark::Error::new_other)?;
                    let prefix = prefix
                        .map(string_arg)
                        .transpose()
                        .map_err(starlark::Error::new_other)?
                        .unwrap_or("");
                    let indent = indent
                        .map(string_arg)
                        .transpose()
                        .map_err(starlark::Error::new_other)?
                        .unwrap_or("\t");
                    (|| {
                        let encoded;
                        let text = if matches!(self.0, Method::EncodeIndent) {
                            encoded = encode(x, heap, &mut budget)?;
                            encoded.as_str()
                        } else {
                            string_arg(x)?
                        };
                        let out = format_json(text, prefix, indent, &mut budget)?;
                        allocate_text(out, heap, &mut budget)
                    })()
                }
            };
            result.map_err(starlark::Error::new_other)
        }
    }
}
use values::JsonModule;

pub(crate) fn install(globals: &mut GlobalsBuilder) {
    globals.set("json", JsonModule);
}
fn required<'v>(x: Option<Value<'v>>) -> Result<Value<'v>> {
    x.ok_or(Error::Internal("required argument not bound"))
}
fn string_arg(x: Value<'_>) -> Result<&str> {
    x.unpack_str()
        .ok_or(Error::Unsupported("JSON argument must be a string"))
}
fn allocate_text<'v>(s: String, heap: Heap<'v>, budget: &mut JsonBudget) -> Result<Value<'v>> {
    budget.bytes(s.len())?;
    Ok(heap.alloc(s))
}

fn quote(text: &str, out: &mut String, budget: &mut JsonBudget) -> Result<()> {
    // The borrowed length is O(1). Reserve scanning work before inspecting even
    // an escape-free input; escaped output growth remains separately charged.
    budget.bytes(text.len())?;
    budget.append(out, "\"")?;
    let mut start = 0;
    for (i, byte) in text.bytes().enumerate() {
        #[cfg(test)]
        {
            budget.observed.quoted_bytes_scanned += 1;
        }
        let escaped = match byte {
            b'"' => "\\\"",
            b'\\' => "\\\\",
            8 => "\\b",
            12 => "\\f",
            b'\n' => "\\n",
            b'\r' => "\\r",
            b'\t' => "\\t",
            0..=31 => "",
            _ => continue,
        };
        budget.append(out, &text[start..i])?;
        if escaped.is_empty() {
            const HEX: &[u8] = b"0123456789abcdef";
            let buf = [
                b'\\',
                b'u',
                b'0',
                b'0',
                HEX[(byte >> 4) as usize],
                HEX[(byte & 15) as usize],
            ];
            budget.append(
                out,
                std::str::from_utf8(&buf).map_err(|_| Error::Internal("escape ASCII"))?,
            )?;
        } else {
            budget.append(out, escaped)?;
        }
        start = i + 1;
    }
    budget.append(out, &text[start..])?;
    budget.append(out, "\"")
}

struct Encoder<'a, 'v> {
    budget: &'a mut JsonBudget,
    heap: Heap<'v>,
    out: String,
    active: Vec<Value<'v>>,
    math: Option<float::Math>,
}
fn encode<'v>(value: Value<'v>, heap: Heap<'v>, budget: &mut JsonBudget) -> Result<String> {
    budget.array(64, std::mem::size_of::<Value>())?;
    let mut active = Vec::new();
    active.try_reserve_exact(64).map_err(|_| Error::Resource)?;
    let mut encoder = Encoder {
        budget,
        heap,
        out: String::new(),
        active,
        math: None,
    };
    encoder.value(value)?;
    Ok(encoder.out)
}
impl<'v> Encoder<'_, 'v> {
    fn emit(&mut self, s: &str) -> Result<()> {
        self.budget.append(&mut self.out, s)
    }
    fn quoted(&mut self, s: &str) -> Result<()> {
        quote(s, &mut self.out, self.budget)
    }
    fn sequence(&mut self, values: impl IntoIterator<Item = Value<'v>>) -> Result<()> {
        self.emit("[")?;
        for (index, v) in values.into_iter().enumerate() {
            if index != 0 {
                self.emit(",")?;
            }
            self.value(v)?;
        }
        self.emit("]")
    }
    fn field_entries<'s>(&mut self, count: usize) -> Result<Vec<(&'s str, Value<'v>)>> {
        // Genuine dict/struct/provider cardinality is O(1). Charge every key
        // node and borrowed-entry slot BEFORE iterator traversal or allocation.
        // This loop itself stops at the existing 100000-node HostBudget limit;
        // it never reads keys, clones payloads, or walks child values.
        self.budget
            .array(count, std::mem::size_of::<(&str, Value)>())?;
        for _ in 0..count {
            self.budget.node()?;
        }
        let mut entries = Vec::new();
        entries
            .try_reserve_exact(count)
            .map_err(|_| Error::Resource)?;
        #[cfg(test)]
        {
            self.budget.observed.key_vectors += 1;
        }
        Ok(entries)
    }
    fn fields<'s>(
        &mut self,
        fields: impl ExactSizeIterator<Item = (&'s str, Value<'v>)>,
    ) -> Result<()> {
        let mut entries = self.field_entries(fields.len())?;
        for (name, value) in fields {
            #[cfg(test)]
            {
                self.budget.observed.key_visits += 1;
            }
            // Reserve key-content comparison work before sorting can inspect
            // common prefixes. Escaping/output will have their own charges.
            self.budget.bytes(name.len())?;
            entries.push((name, value));
        }
        self.emit_fields(entries)
    }
    fn emit_fields(&mut self, mut entries: Vec<(&str, Value<'v>)>) -> Result<()> {
        // Entry slots, all key nodes/types, and all key byte lengths have been
        // checked before any comparison or member output reaches this helper.
        entries.sort_unstable_by(|a, b| {
            #[cfg(test)]
            {
                self.budget.observed.sort_comparisons += 1;
            }
            a.0.cmp(b.0)
        });
        self.emit("{")?;
        for (index, (name, value)) in entries.into_iter().enumerate() {
            if index != 0 {
                self.emit(",")?;
            }
            self.quoted(name)?;
            self.emit(":")?;
            self.value(value)?;
        }
        self.emit("}")
    }
    fn value(&mut self, value: Value<'v>) -> Result<()> {
        self.budget.node()?;
        if value.is_none() {
            return self.emit("null");
        }
        if let Some(v) = value.unpack_bool() {
            return self.emit(if v { "true" } else { "false" });
        }
        if let Some(v) = value.unpack_str() {
            return self.quoted(v);
        }
        if let Some(v) = value.unpack_i32() {
            self.budget.bytes(16)?;
            return self.emit(&v.to_string());
        }
        if let Some(v) = value.unpack_bigint_ref() {
            // Borrow bits before decimal conversion: never clone or stringify an
            // untrusted preexisting integer in either quota checks or diagnostics.
            let bits = usize::try_from(v.bits()).map_err(|_| Error::Resource)?;
            // bits is also a conservative decimal-byte bound. Quadratic work
            // fee bounds conversion of maliciously enormous existing integers.
            let work = bits.checked_mul(bits.div_ceil(64)).ok_or(Error::Resource)?;
            self.budget
                .bytes(work.checked_add(bits).ok_or(Error::Resource)?)?;
            return self.emit(&v.to_str_radix(10));
        }
        if let Some(v) = value.downcast_ref::<StarlarkFloat>() {
            if !v.0.is_finite() {
                return Err(Error::Unsupported("cannot encode non-finite float"));
            }
            if self.math.is_none() {
                self.math = Some(float::Math::new(self.budget)?);
            }
            let s = self
                .math
                .as_ref()
                .ok_or(Error::Internal("float math missing"))?
                .format(v.0, self.budget)?;
            return self.emit(&s);
        }
        if self.active.iter().any(|v| v.ptr_eq(value)) {
            return Err(Error::Unsupported("JSON cycle in active ancestors"));
        }
        self.budget.open(self.active.len())?;
        self.active.push(value);
        let result = self.container(value);
        self.active.pop();
        result
    }
    fn container(&mut self, value: Value<'v>) -> Result<()> {
        if crate::starlark_types::is_default_info(value)
            || crate::run_environment_info::is_instance(value)
            || crate::instrumented_files::is_instance(value)
            || crate::template_variable_info::is_instance(value)
            || crate::package_specification::is_instance(value)
        {
            return Err(Error::Unsupported("cannot encode native Info as JSON"));
        }
        if let Some(v) = ListRef::from_value(value) {
            return self.sequence(v.iter());
        }
        if let Some(v) = TupleRef::from_value(value) {
            return self.sequence(v.iter());
        }
        if let Some(v) = SetRef::unpack_value(value).map_err(|_| Error::Internal("set unpack"))? {
            return self.sequence(v.iter());
        }
        if value.downcast_ref::<Range>().is_some() {
            // Only the concrete native Range gets to execute its iterator.
            self.emit("[")?;
            let mut iterator = value
                .iterate(self.heap)
                .map_err(|_| Error::Internal("range iteration"))?;
            let mut first = true;
            loop {
                self.budget.node()?;
                let Some(n) = iterator.next() else {
                    break;
                };
                if !first {
                    self.emit(",")?;
                }
                first = false;
                let n = n.unpack_i32().ok_or(Error::Internal("range integer"))?;
                self.budget.bytes(16)?;
                self.emit(&n.to_string())?;
            }
            return self.emit("]");
        }
        if let Some(v) = DictRef::from_value(value) {
            let mut entries = self.field_entries(v.len())?;
            for (key, value) in v.iter() {
                #[cfg(test)]
                {
                    self.budget.observed.key_visits += 1;
                }
                // Typed unpack and length retrieval are O(1), without inspecting
                // contents or formatting invalid payloads. The key's node is
                // already charged; charge its byte length before any content
                // work. Validate EVERY key before sorting or member emission.
                let name = key
                    .unpack_str()
                    .ok_or(Error::Unsupported("dict has non-string key, want string"))?;
                self.budget.bytes(name.len())?;
                entries.push((name, value));
            }
            return self.emit_fields(entries);
        }
        if let Some(v) = StructRef::from_value(value) {
            return self.fields(v.iter().map(|(k, v)| (k.as_str(), v)));
        }
        if let Some(fields) = crate::providers::json_fields(value) {
            return self.fields(fields);
        }
        if let Some(file) = value.downcast_ref::<crate::starlark_types::File>() {
            // Preflight the adapter *before* Label::parse_canonical can copy or
            // format errors. Label::parse_in may duplicate a shorthand basename
            // (<=2*owner bytes total owned fields), and validation errors Debug-
            // escape an owner substring (<=8*owner+256 bytes, with <=2x growth).
            // Reserve 24*owner+1024 for those alternatives, headers and scans;
            // 4*(path+short_path) covers the adapter's borrowed validation scans.
            // HostBudget additionally weights this by eight. This is an explicit
            // conservative native reservation, not an allocator/RSS/CPU limit.
            let owner = file.owner.len().checked_mul(24).ok_or(Error::Resource)?;
            let paths = file
                .path
                .len()
                .checked_add(file.short_path.len())
                .and_then(|n| n.checked_mul(4))
                .ok_or(Error::Resource)?;
            self.budget.bytes(
                owner
                    .checked_add(paths)
                    .and_then(|n| n.checked_add(1024))
                    .ok_or(Error::Resource)?,
            )?;
            let root = crate::starlark_types::file_json_root(file).map_err(|_| {
                Error::Unsupported("cannot encode unsupported File profile as JSON")
            })?;
            self.emit("{")?;
            for (i, (key, val)) in [
                ("path", file.path.as_str()),
                ("root", root),
                ("short_path", file.short_path.as_str()),
            ]
            .into_iter()
            .enumerate()
            {
                self.budget.node()?;
                self.budget.node()?;
                if i != 0 {
                    self.emit(",")?;
                }
                self.quoted(key)?;
                self.emit(":")?;
                self.quoted(val)?;
            }
            return self.emit("}");
        }
        Err(Error::Unsupported(
            "cannot encode unsupported value as JSON",
        ))
    }
}

struct Parser<'a, 'b, 'v> {
    text: &'a str,
    pos: usize,
    heap: Heap<'v>,
    budget: &'b mut JsonBudget,
    /// Bazel byte-string profile (repository/extension execution): `\u` escape
    /// runs become UTF-8 bytes, each represented as a U+0000..U+00FF char.
    byte_strings: bool,
}
fn decode<'v>(
    text: &str,
    heap: Heap<'v>,
    budget: &mut JsonBudget,
    byte_strings: bool,
) -> Result<Value<'v>> {
    // O(1) effective UTF-8 length check, before scanning or reserving any input.
    if text.len() > DECODE_MAX_INPUT {
        return Err(Error::Resource);
    }
    budget.bytes(text.len())?;
    let mut parser = Parser {
        text,
        pos: 0,
        heap,
        budget,
        byte_strings,
    };
    let value = parser.value(0)?;
    parser.space();
    if parser.pos != text.len() {
        return Err(parser.error("unexpected character after value"));
    }
    Ok(value)
}
impl<'v> Parser<'_, '_, 'v> {
    fn error(&self, message: &'static str) -> Error {
        Error::Syntax(self.pos, message)
    }
    fn peek(&self) -> Option<u8> {
        self.text.as_bytes().get(self.pos).copied()
    }
    fn space(&mut self) {
        while matches!(self.peek(), Some(b' ' | b'\t' | b'\n' | b'\r')) {
            self.pos += 1;
        }
    }
    fn take(&mut self, byte: u8) -> bool {
        if self.peek() == Some(byte) {
            self.pos += 1;
            true
        } else {
            false
        }
    }
    fn literal(&mut self, literal: &str, value: Value<'v>) -> Result<Value<'v>> {
        if !self.text[self.pos..].starts_with(literal) {
            return Err(self.error("invalid literal"));
        }
        self.pos += literal.len();
        Ok(value)
    }
    fn value(&mut self, depth: usize) -> Result<Value<'v>> {
        self.space();
        self.budget.node()?;
        match self.peek() {
            Some(b'n') => self.literal("null", Value::new_none()),
            Some(b't') => self.literal("true", Value::new_bool(true)),
            Some(b'f') => self.literal("false", Value::new_bool(false)),
            Some(b'"') => {
                let s = self.string()?;
                allocate_text(s, self.heap, self.budget)
            }
            Some(b'[') => {
                self.budget.open(depth)?;
                self.pos += 1;
                self.space();
                let mut values = Vec::new();
                if !self.take(b']') {
                    loop {
                        self.budget.array(2, std::mem::size_of::<Value>())?;
                        values.try_reserve(1).map_err(|_| Error::Resource)?;
                        values.push(self.value(depth + 1)?);
                        self.space();
                        if self.take(b']') {
                            break;
                        }
                        if !self.take(b',') {
                            return Err(self.error("expected ',' or ']'"));
                        }
                    }
                }
                self.budget
                    .array(values.len(), std::mem::size_of::<Value>())?;
                Ok(self.heap.alloc(AllocList(values)))
            }
            Some(b'{') => {
                self.budget.open(depth)?;
                self.pos += 1;
                self.space();
                let dict = self.heap.alloc(AllocDict::EMPTY);
                if !self.take(b'}') {
                    loop {
                        self.space();
                        self.budget.node()?;
                        if self.peek() != Some(b'"') {
                            return Err(self.error("expected string key"));
                        }
                        let name = self.string()?;
                        self.space();
                        if !self.take(b':') {
                            return Err(self.error("expected ':'"));
                        }
                        let value = self.value(depth + 1)?;
                        // Charge even overwritten entries and discarded values.
                        // Native dict insertion preserves the first key position.
                        self.budget.bytes(128)?;
                        let key = allocate_text(name, self.heap, self.budget)?;
                        dict.set_at(key, value)
                            .map_err(|_| Error::Internal("decoded dict insertion"))?;
                        self.space();
                        if self.take(b'}') {
                            break;
                        }
                        if !self.take(b',') {
                            return Err(self.error("expected ',' or '}'"));
                        }
                    }
                }
                Ok(dict)
            }
            Some(b'-' | b'0'..=b'9') => self.number(),
            Some(_) => Err(self.error("unexpected character")),
            None => Err(self.error("unexpected end of file")),
        }
    }
    fn hex4(&mut self) -> Result<u16> {
        let mut result = 0u16;
        for _ in 0..4 {
            let b = self
                .peek()
                .ok_or_else(|| self.error("incomplete Unicode escape"))?;
            let digit = match b {
                b'0'..=b'9' => b - b'0',
                b'a'..=b'f' => b - b'a' + 10,
                b'A'..=b'F' => b - b'A' + 10,
                _ => return Err(self.error("invalid Unicode escape")),
            };
            result = result * 16 + u16::from(digit);
            self.pos += 1;
        }
        Ok(result)
    }
    fn push_char(&mut self, out: &mut String, c: char) -> Result<()> {
        let mut bytes = [0u8; 4];
        self.budget.append(out, c.encode_utf8(&mut bytes))
    }
    fn string(&mut self) -> Result<String> {
        if !self.take(b'"') {
            return Err(self.error("expected string"));
        }
        let mut out = String::new();
        loop {
            let start = self.pos;
            while self
                .peek()
                .is_some_and(|b| b >= 32 && b != b'"' && b != b'\\')
            {
                self.pos += 1;
            }
            self.budget.append(&mut out, &self.text[start..self.pos])?;
            let byte = self
                .peek()
                .ok_or_else(|| self.error("unterminated string"))?;
            self.pos += 1;
            match byte {
                b'"' => return Ok(out),
                b'\\' => {
                    let escape = self.peek().ok_or_else(|| self.error("incomplete escape"))?;
                    self.pos += 1;
                    if escape == b'u' && self.byte_strings {
                        // Pinned Json.java parseString: gather the whole run of
                        // `\uXXXX` code units, encode as UTF-8 replacing malformed
                        // surrogates with U+FFFD bytes, append bytes as Latin-1.
                        let mut units = vec![self.hex4()?];
                        while self.text[self.pos..].starts_with("\\u") {
                            self.pos += 2;
                            units.push(self.hex4()?);
                        }
                        for decoded in char::decode_utf16(units) {
                            let mut bytes = [0u8; 4];
                            let encoded = decoded.unwrap_or('\u{fffd}').encode_utf8(&mut bytes);
                            for byte in encoded.bytes() {
                                self.push_char(&mut out, char::from(byte))?;
                            }
                        }
                        continue;
                    }
                    let character = match escape {
                        b'"' => '"',
                        b'\\' => '\\',
                        b'/' => '/',
                        b'b' => '\u{8}',
                        b'f' => '\u{c}',
                        b'n' => '\n',
                        b'r' => '\r',
                        b't' => '\t',
                        b'u' => {
                            let high = self.hex4()?;
                            if (0xd800..=0xdbff).contains(&high)
                                && self.text[self.pos..].starts_with("\\u")
                            {
                                let saved = self.pos;
                                self.pos += 2;
                                let low = self.hex4()?;
                                if (0xdc00..=0xdfff).contains(&low) {
                                    char::from_u32(
                                        0x10000
                                            + ((u32::from(high) - 0xd800) << 10)
                                            + u32::from(low)
                                            - 0xdc00,
                                    )
                                    .ok_or(Error::Internal("surrogate pair"))?
                                } else {
                                    self.pos = saved;
                                    '\u{fffd}'
                                }
                            } else {
                                char::from_u32(u32::from(high)).unwrap_or('\u{fffd}')
                            }
                        }
                        _ => return Err(self.error("invalid escape")),
                    };
                    self.push_char(&mut out, character)?;
                }
                _ => return Err(self.error("unescaped control character")),
            }
        }
    }
    fn number(&mut self) -> Result<Value<'v>> {
        let start = self.pos;
        self.take(b'-');
        if self.take(b'0') {
            if self.peek().is_some_and(|b| b.is_ascii_digit()) {
                return Err(Error::Syntax(start, "invalid number"));
            }
        } else {
            let digits = self.pos;
            while self.peek().is_some_and(|b| b.is_ascii_digit()) {
                self.pos += 1;
            }
            if self.pos == digits {
                return Err(Error::Syntax(start, "invalid number"));
            }
        }
        let mut floating = false;
        if self.take(b'.') {
            floating = true;
            let digits = self.pos;
            while self.peek().is_some_and(|b| b.is_ascii_digit()) {
                self.pos += 1;
            }
            // Pinned decoder quirk: uppercase E alone may follow the point.
            if self.pos == digits && self.peek() != Some(b'E') {
                return Err(Error::Syntax(start, "invalid number"));
            }
        }
        if matches!(self.peek(), Some(b'e' | b'E')) {
            floating = true;
            self.pos += 1;
            if matches!(self.peek(), Some(b'+' | b'-')) {
                self.pos += 1;
            }
            let digits = self.pos;
            while self.peek().is_some_and(|b| b.is_ascii_digit()) {
                self.pos += 1;
            }
            if self.pos == digits {
                return Err(Error::Syntax(start, "invalid number"));
            }
        }
        let token = &self.text[start..self.pos];
        let size = token.len();
        self.budget.bytes(
            size.checked_mul(size.div_ceil(16))
                .ok_or(Error::Resource)?
                .checked_add(64)
                .ok_or(Error::Resource)?,
        )?;
        #[cfg(test)]
        {
            self.budget.observed.numeric_conversions += 1;
        }
        if floating {
            let value = token
                .parse::<f64>()
                .map_err(|_| Error::Syntax(start, "invalid number"))?;
            Ok(self.heap.alloc(value))
        } else {
            let value = num_bigint::BigInt::parse_bytes(token.as_bytes(), 10)
                .ok_or(Error::Syntax(start, "invalid integer"))?;
            Ok(self.heap.alloc(value))
        }
    }
}

/// Token-preserving port of Apache-2.0 Bazel Json.java (9.3, lines 732-849).
/// Deliberately not a validator: mismatched inline empties and trailing text
/// retain the pinned behavior, as does a number requiring a following byte.
fn format_json(text: &str, prefix: &str, indent: &str, budget: &mut JsonBudget) -> Result<String> {
    budget.bytes(text.len())?;
    budget.bytes(prefix.len())?;
    budget.bytes(indent.len())?;
    let mut out = String::new();
    let mut pos = 0;
    let mut depth = 0usize;
    fn next(text: &str, pos: &mut usize) -> Result<u8> {
        while matches!(
            text.as_bytes().get(*pos),
            Some(b' ' | b'\t' | b'\r' | b'\n')
        ) {
            *pos += 1;
        }
        text.as_bytes()
            .get(*pos)
            .copied()
            .ok_or(Error::Syntax(*pos, "unexpected end of file"))
    }
    fn newline(
        out: &mut String,
        prefix: &str,
        indent: &str,
        depth: usize,
        budget: &mut JsonBudget,
    ) -> Result<()> {
        let bytes = indent
            .len()
            .checked_mul(depth)
            .and_then(|x| x.checked_add(prefix.len()))
            .and_then(|x| x.checked_add(1))
            .ok_or(Error::Resource)?;
        budget.bytes(bytes)?;
        out.try_reserve(bytes).map_err(|_| Error::Resource)?;
        out.push('\n');
        out.push_str(prefix);
        for _ in 0..depth {
            out.push_str(indent);
        }
        Ok(())
    }
    loop {
        let c = next(text, &mut pos)?;
        budget.node()?;
        let start = pos;
        match c {
            b'"' => {
                pos += 1;
                loop {
                    let c = text
                        .as_bytes()
                        .get(pos)
                        .copied()
                        .ok_or(Error::Syntax(pos, "unterminated string"))?;
                    pos += 1;
                    if c == b'"' {
                        break;
                    }
                    if c == b'\\' {
                        let c = text
                            .as_bytes()
                            .get(pos)
                            .copied()
                            .ok_or(Error::Syntax(pos, "incomplete escape"))?;
                        pos += 1;
                        if c == b'u' {
                            pos = pos.checked_add(4).ok_or(Error::Resource)?;
                        }
                    }
                }
                let slice = text
                    .get(start..pos)
                    .ok_or(Error::Syntax(pos, "invalid string token boundary"))?;
                budget.append(&mut out, slice)?;
            }
            b'n' | b't' | b'f' => {
                pos += if c == b'f' { 5 } else { 4 };
                budget.append(
                    &mut out,
                    text.get(start..pos)
                        .ok_or(Error::Syntax(start, "incomplete literal"))?,
                )?;
            }
            b',' => {
                pos += 1;
                budget.append(&mut out, ",")?;
                newline(&mut out, prefix, indent, depth, budget)?;
            }
            b'[' | b'{' => {
                budget.open(depth)?;
                pos += 1;
                budget.append(&mut out, &text[start..pos])?;
                let c = next(text, &mut pos)?;
                if c == b']' || c == b'}' {
                    budget.append(&mut out, &text[pos..pos + 1])?;
                    pos += 1;
                } else {
                    depth += 1;
                    newline(&mut out, prefix, indent, depth, budget)?;
                }
            }
            b']' | b'}' => {
                pos += 1;
                depth = depth.saturating_sub(1);
                newline(&mut out, prefix, indent, depth, budget)?;
                budget.append(&mut out, &text[start..pos])?;
            }
            b':' => {
                pos += 1;
                budget.append(&mut out, ": ")?;
            }
            b'-' | b'0'..=b'9' => {
                loop {
                    pos += 1;
                    let c = text
                        .as_bytes()
                        .get(pos)
                        .copied()
                        .ok_or(Error::Syntax(pos, "unexpected end of number"))?;
                    if !matches!(c, b'0'..=b'9' | b'.' | b'e' | b'E' | b'+' | b'-') {
                        break;
                    }
                }
                budget.append(&mut out, &text[start..pos])?;
            }
            _ => return Err(Error::Syntax(pos, "unexpected character")),
        }
        if depth == 0 {
            return Ok(out);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use starlark::environment::{FrozenModule, Globals, LibraryExtension, Module};
    use starlark::syntax::{AstModule, Dialect};

    fn globals() -> Globals {
        GlobalsBuilder::extended_by(&[LibraryExtension::StructType, LibraryExtension::SetType])
            .with(crate::starlark_types::analysis_globals)
            .with(crate::template_variable_info::install)
            .with(install)
            .build()
    }
    fn evaluate(source: &str) -> anyhow::Result<FrozenModule> {
        Module::with_temp_heap(|module| {
            let ast = AstModule::parse("json.star", source.to_owned(), &Dialect::Standard)
                .map_err(|e| anyhow::anyhow!("{e}"))?;
            Evaluator::new(&module)
                .eval_module(ast, &globals())
                .map_err(|e| anyhow::anyhow!("{e}"))?;
            module.freeze().map_err(|e| anyhow::anyhow!("{e:?}"))
        })
    }
    fn pass(source: &str) {
        evaluate(source).unwrap();
    }

    fn decode_budget() -> JsonBudget {
        let mut budget = JsonBudget::for_method(Method::Decode);
        budget.array(1, 128).unwrap();
        budget
    }

    #[test]
    fn json_decode_candidate_node_boundary_independent_of_bytes() {
        for count in [249_999, 250_000] {
            Module::with_temp_heap(|module| {
                let text = format!("[{}null]", "null,".repeat(count - 1));
                let mut budget = decode_budget();
                let value = decode(&text, module.heap(), &mut budget, false);
                if count == 249_999 {
                    assert_eq!(ListRef::from_value(value.unwrap()).unwrap().len(), count);
                } else {
                    assert!(matches!(value, Err(Error::Resource)));
                }
                let (bytes, nodes) = budget.host.usage();
                assert_eq!(nodes, 250_000);
                assert!(bytes < DECODE_MAX_BYTES);
                eprintln!(
                    "null array count={count} weighted={bytes} nodes={nodes} heap_allocated={} heap_available={}",
                    module.heap().allocated_bytes(),
                    module.heap().available_bytes()
                );
            });
        }
        pass("len(json.decode('['+'null,'*249998+'null]')) == 249999 or fail('node boundary')");
        assert!(
            evaluate("json.decode('['+'null,'*249999+'null]', default=[])")
                .unwrap_err()
                .to_string()
                .contains("resource limit")
        );
    }

    #[test]
    fn json_decode_empty_arrays_weighted_boundary_is_not_node_failure() {
        // Each child costs 2392 including its input and final heap slot. The
        // exact largest accepted count is 112220 for a one-argument invocation.
        for count in [112_220, 112_221] {
            Module::with_temp_heap(|module| {
                let text = format!("[{}[]]", "[],".repeat(count - 1));
                let mut budget = decode_budget();
                let value = decode(&text, module.heap(), &mut budget, false);
                if count == 112_220 {
                    assert_eq!(ListRef::from_value(value.unwrap()).unwrap().len(), count);
                    assert_eq!(budget.host.usage(), (3208 + count * 2392, count + 1));
                } else {
                    assert!(matches!(value, Err(Error::Resource)));
                    // All children parsed; final root heap-slot charge fails.
                    let (bytes, nodes) = budget.host.usage();
                    assert_eq!(nodes, count + 1);
                    assert_eq!(bytes, 3208 + count * (2392 - 64));
                    assert!(bytes + count * 64 > DECODE_MAX_BYTES);
                }
                eprintln!(
                    "empty-array boundary count={count} usage={:?} heap_allocated={} heap_available={}",
                    budget.host.usage(),
                    module.heap().allocated_bytes(),
                    module.heap().available_bytes()
                );
            });
        }
        Module::with_temp_heap(|module| {
            // Source order: input, root node/open, then slot/node/open per child.
            // At 120000 children the first failure is a temporary slot reservation,
            // before the final root-list heap-slot reservation is reached.
            let text = format!("[{}[]]", "[],".repeat(119_999));
            let mut budget = decode_budget();
            budget.bytes(text.len()).unwrap();
            let base = budget.host.usage().0 + 128 + 2048;
            let complete_children = (DECODE_MAX_BYTES - base) / (128 + 128 + 2048);
            let mut parser = Parser {
                text: &text,
                pos: 0,
                heap: module.heap(),
                budget: &mut budget,
                byte_strings: false,
            };
            assert!(matches!(parser.value(0), Err(Error::Resource)));
            let (bytes, nodes) = parser.budget.host.usage();
            assert_eq!(parser.pos, 1 + complete_children * 3);
            assert_eq!(nodes, complete_children + 1);
            assert_eq!(bytes, base + complete_children * 2304);
            assert!(bytes <= DECODE_MAX_BYTES && bytes + 128 > DECODE_MAX_BYTES);
            assert!(nodes < DECODE_MAX_NODES);
            eprintln!(
                "empty-array first failure: operation=temporary-array-slots offset={} weighted={bytes} nodes={nodes} requested=128",
                parser.pos
            );
        });
    }

    #[test]
    fn json_decode_input_ceiling_counts_utf8_before_syntax_or_heap_allocation() {
        Module::with_temp_heap(|module| {
            for extra in [0, 1] {
                let text = format!("null{}", " ".repeat(DECODE_MAX_INPUT - 4 + extra));
                let mut budget = decode_budget();
                let before = module.heap().allocated_bytes();
                let value = decode(&text, module.heap(), &mut budget, false);
                if extra == 0 {
                    assert!(value.unwrap().is_none());
                    assert_eq!(budget.host.usage(), (1024 + DECODE_MAX_INPUT * 8 + 128, 1));
                } else {
                    assert!(matches!(value, Err(Error::Resource)));
                    assert_eq!(budget.host.usage(), (1024, 0));
                }
                assert_eq!(module.heap().allocated_bytes(), before);
            }
            let text = format!("!{}", "é".repeat(DECODE_MAX_INPUT / 2));
            assert_eq!(text.len(), DECODE_MAX_INPUT + 1);
            let mut budget = decode_budget();
            assert!(matches!(
                decode(&text, module.heap(), &mut budget, false),
                Err(Error::Resource)
            ));
            assert_eq!(budget.host.usage(), (1024, 0));
        });
        pass(
            "json.decode('!'+' '*(8388608-1), default='fallback') == 'fallback' or fail('syntax default')",
        );
        for source in [
            "json.decode('!'+' '*8388608, default='FALLBACK_RETURNED')",
            "json.decode('!'+'é'*4194304, default='FALLBACK_RETURNED')",
        ] {
            assert!(
                evaluate(source)
                    .unwrap_err()
                    .to_string()
                    .contains("resource limit")
            );
        }
    }

    #[test]
    fn json_decode_duplicates_keep_all_charges_and_old_cases_now_fit() {
        Module::with_temp_heap(|module| {
            let text = format!("{{{}}}", vec!["\"x\":[]"; 40_000].join(","));
            let mut budget = decode_budget();
            let value = decode(&text, module.heap(), &mut budget, false).unwrap();
            assert_eq!(DictRef::from_value(value).unwrap().len(), 1);
            let (bytes, nodes) = budget.host.usage();
            assert_eq!(nodes, 80_001);
            assert_eq!(
                bytes,
                1024 + text.len() * 8 + 2176 + 40_000 * (256 + 2048 + 1024 + 16)
            );
            assert!(bytes > 64 * 1024 * 1024);
            eprintln!(
                "duplicate array members: weighted={bytes} nodes={nodes} heap_allocated={} heap_available={}",
                module.heap().allocated_bytes(),
                module.heap().available_bytes()
            );
        });
        pass("len(json.decode('['+'0,'*100001+'0]')) == 100002 or fail('old zero array now fits')");
        pass("json.decode('{'+ '\"x\":[],'*40000+'\"x\":[7]}') == {'x':[7]} or fail('overwrite')");
        assert!(
            evaluate("json.decode('{'+ '\"x\":[],'*80000+'\"x\":[]}', default=[])")
                .unwrap_err()
                .to_string()
                .contains("resource limit")
        );
    }

    #[test]
    fn json_decode_numeric_work_is_still_nonlinear_and_preconversion() {
        Module::with_temp_heap(|module| {
            for text in ["9".repeat(24_000), format!("1.{}", "9".repeat(24_000))] {
                let mut budget = decode_budget();
                let before = module.heap().allocated_bytes();
                assert!(matches!(
                    decode(&text, module.heap(), &mut budget, false),
                    Err(Error::Resource)
                ));
                assert_eq!(budget.observed.numeric_conversions, 0);
                assert_eq!(module.heap().allocated_bytes(), before);
                assert_eq!(budget.host.usage(), (1024 + text.len() * 8 + 128, 1));
            }
            // Numeric-heavy valid input fails the byte reservation, not nodes.
            let token = "12345678901234567890123456789012";
            let text = format!("[{}{token}]", format!("{token},").repeat(179_999));
            let mut budget = decode_budget();
            budget.bytes(text.len()).unwrap();
            let mut parser = Parser {
                text: &text,
                pos: 0,
                heap: module.heap(),
                budget: &mut budget,
                byte_strings: false,
            };
            assert!(matches!(parser.value(0), Err(Error::Resource)));
            let (bytes, nodes) = parser.budget.host.usage();
            assert!(nodes < DECODE_MAX_NODES);
            assert!(bytes <= DECODE_MAX_BYTES && bytes + 1024 > DECODE_MAX_BYTES);
            assert_eq!(parser.budget.observed.numeric_conversions + 2, nodes);
            eprintln!(
                "numeric-heavy first failure: operation=numeric-work offset={} weighted={bytes} nodes={nodes} conversions={}",
                parser.pos, parser.budget.observed.numeric_conversions
            );
        });
        for source in [
            "json.decode('9'*24000, default=[])",
            "json.decode('1.'+'9'*24000, default=[])",
        ] {
            assert!(
                evaluate(source)
                    .unwrap_err()
                    .to_string()
                    .contains("resource limit")
            );
        }
    }

    #[test]
    fn json_non_decode_profiles_stay_legacy_and_decode_does_not_promise_encode() {
        for method in [Method::Encode, Method::Indent, Method::EncodeIndent] {
            let mut budget = JsonBudget::for_method(method);
            budget.bytes(8 * 1024 * 1024).unwrap();
            assert!(matches!(budget.bytes(1), Err(Error::Resource)));
            let mut budget = JsonBudget::for_method(method);
            for _ in 0..100_000 {
                budget.node().unwrap();
            }
            assert!(matches!(budget.node(), Err(Error::Resource)));
        }
        assert!(
            evaluate("json.encode(json.decode('['+'null,'*100000+'null]'))")
                .unwrap_err()
                .to_string()
                .contains("resource limit")
        );
        assert_eq!(
            Error::Resource.to_string(),
            "JSON host preparation resource limit exceeded"
        );
    }

    #[allow(unsafe_code)]
    mod spoofs {
        use super::*;
        use starlark::values::{
            NoSerialize, ProvidesStaticType, StarlarkPagablePanic, StarlarkValue, starlark_value,
        };
        #[derive(Debug, ProvidesStaticType, NoSerialize, Allocative, StarlarkPagablePanic)]
        pub(super) struct FakeFloat;
        starlark::starlark_simple_value!(FakeFloat);
        impl fmt::Display for FakeFloat {
            fn fmt(&self, _: &mut fmt::Formatter<'_>) -> fmt::Result {
                panic!("codec must not format unsupported payload")
            }
        }
        #[starlark_value(type = "float")]
        impl<'v> StarlarkValue<'v> for FakeFloat {
            fn dir_attr(&self) -> Vec<String> {
                panic!("codec must not reflect arbitrary values")
            }
            fn get_attr(&self, _: &str, _: Heap<'v>) -> Option<Value<'v>> {
                panic!("codec must not call arbitrary attributes")
            }
        }
        #[derive(Debug, ProvidesStaticType, NoSerialize, Allocative, StarlarkPagablePanic)]
        pub(super) struct FakeStruct;
        starlark::starlark_simple_value!(FakeStruct);
        impl fmt::Display for FakeStruct {
            fn fmt(&self, _: &mut fmt::Formatter<'_>) -> fmt::Result {
                panic!("codec must not format unsupported payload")
            }
        }
        #[starlark_value(type = "struct")]
        impl<'v> StarlarkValue<'v> for FakeStruct {
            fn dir_attr(&self) -> Vec<String> {
                panic!("codec must not reflect arbitrary values")
            }
            fn get_attr(&self, _: &str, _: Heap<'v>) -> Option<Value<'v>> {
                panic!("codec must not call arbitrary attributes")
            }
        }
    }

    #[test]
    fn json_type_names_do_not_authorize_arbitrary_values() {
        Module::with_temp_heap(|module| {
            let heap = module.heap();
            for value in [
                heap.alloc(spoofs::FakeFloat),
                heap.alloc(spoofs::FakeStruct),
            ] {
                assert!(matches!(
                    encode(value, heap, &mut JsonBudget::default()),
                    Err(Error::Unsupported(_))
                ));
            }
        });
    }

    #[test]
    fn json_real_metadata_binding_and_fresh_method_identity() {
        pass(
            r#"
(type(json) == 'json' and dir(json) == ['decode','encode','encode_indent','indent']) or fail('module')
(repr(json) == '<unknown object net.starlark.java.lib.json.Json>' and str(json) == repr(json)) or fail('repr')
M = json.encode
(type(M) == 'builtin_function_or_method' and repr(M) == '<built-in method encode of json value>') or fail('method')
(M == M and json.encode != json.encode and M != json.encode) or fail('identity')
def repeated():
    return [json.encode for _ in range(2)]
methods = repeated()
(methods[0] != methods[1]) or fail('folded')
fallback = []
d = json.decode('!', fallback)
d.append(1)
(fallback == [1] and json.decode('!', default=None) == None and json.decode('null', 5) == None) or fail('fallback identity')
"#,
        );
        for source in [
            "{json:1}",
            "{json.encode:1}",
            "json.encode(x=1)",
            "json.decode(x='1')",
            "json.decode(1, [])",
            "json.decode('!',[],unknown=1)",
            "json.indent('[]','p')",
            "json.indent('[]', prefix=1)",
            "json.encode_indent([],indent=1)",
            "json.encode()",
            "json.encode(1,2)",
            "json.encode = None",
        ] {
            assert!(evaluate(source).is_err(), "{source}");
        }
    }

    #[test]
    fn json_frozen_alias_identity_across_globals() {
        let frozen = evaluate("M=json.encode\nJ=json\nD=json.decode('[1]')").unwrap();
        Module::with_temp_heap(|module| {
            let mut builder = GlobalsBuilder::extended_by(&[]);
            install(&mut builder);
            builder.set("M", frozen.get("M").unwrap());
            builder.set("J", frozen.get("J").unwrap());
            let globals = builder.build();
            let source = "(M==M and M!=json.encode and J==json and J.encode!=J.encode and M([1])=='[1]') or fail('frozen identity')";
            let ast =
                AstModule::parse("frozen.star", source.to_owned(), &Dialect::Standard).unwrap();
            Evaluator::new(&module).eval_module(ast, &globals).unwrap();
        });
    }

    #[test]
    fn json_typed_primitives_containers_and_mutable_decode() {
        pass(
            r#"
x = json.decode('{"z":1,"a":2,"z":3,"nested":[{"k":null}]}')
(x.keys() == ['z','a','nested'] and x['z'] == 3) or fail('duplicates')
x['nested'][0]['k'] = ['mutable']
json.encode(x)
json.encode_indent(x)
x['nested'].append(4)
(json.encode(x) == '{"a":2,"nested":[{"k":["mutable"]},4],"z":3}') or fail('mutable')
(json.encode(struct(z=(1,2),a=struct(k=[]))) == '{"a":{"k":[]},"z":[1,2]}') or fail('struct')
(json.encode([None,True,False,range(3),set([3,1,2])]) == '[null,true,false,[0,1,2],[3,1,2]]') or fail('containers')
(json.encode(range(3,-4,-2)) == '[3,1,-1,-3]') or fail('range')
(json.encode(json.decode('[0,-0,1.0,1e3,-0.0,123456789123456789123456789]')) == '[0,0,1.0,1000.0,-0.0,123456789123456789123456789]') or fail('numbers')
([type(json.decode(s)) for s in ['0','-0','1.0','1e3','-0.0','123456789123456789123456789']] == ['int','int','float','float','float','int']) or fail('numeric types')
(json.decode('1e999') == float('inf') and json.decode('-1e999') == float('-inf')) or fail('overflow')
"#,
        );
    }

    #[test]
    fn json_strings_unicode_surrogates_and_sorted_utf8_keys() {
        Module::with_temp_heap(|module| {
            let heap = module.heap();
            for (source, expected) in [
                (r#""\u00e9\ud83d\ude00""#, "é😀"),
                (r#""\ud800X\udc00""#, "�X�"),
                (r#""\ud800\ud800\udc00""#, "�𐀀"),
                (r#""\ud800\u0041""#, "�A"),
                (r#""\"\\\/\b\f\n\r\t""#, "\"\\/\u{8}\u{c}\n\r\t"),
            ] {
                let value = decode(source, heap, &mut JsonBudget::default(), false).unwrap();
                assert_eq!(value.unpack_str(), Some(expected));
            }
            // Repository/extension byte-string profile (pinned Json.java): each
            // `\u` run becomes UTF-8 bytes (U+FFFD bytes for malformed
            // surrogates), each byte one U+0000..U+00FF char; raw input
            // characters and ASCII escapes are unchanged.
            for (source, expected) in [
                (
                    r#""\u00e9\ud83d\ude00""#,
                    "\u{c3}\u{a9}\u{f0}\u{9f}\u{98}\u{80}",
                ),
                (
                    r#""\ud800X\udc00""#,
                    "\u{ef}\u{bf}\u{bd}X\u{ef}\u{bf}\u{bd}",
                ),
                (r#""\ud800\u0041""#, "\u{ef}\u{bf}\u{bd}A"),
                (r#""A\u0041\u007f""#, "AA\u{7f}"),
                ("\"\u{e9}\\n\"", "\u{e9}\n"),
            ] {
                let value = decode(source, heap, &mut JsonBudget::default(), true).unwrap();
                assert_eq!(value.unpack_str(), Some(expected), "{source}");
            }
            let text = "\0\u{1}\u{8}\u{c}\n\r\t\"\\/\u{7f}é😀";
            assert_eq!(
                encode(heap.alloc(text), heap, &mut JsonBudget::default()).unwrap(),
                "\"\\u0000\\u0001\\b\\f\\n\\r\\t\\\"\\\\/\u{7f}é😀\""
            );
            let value = decode(
                "{\"😀\":1,\"\u{e000}\":2,\"é\":3,\"A\":4}",
                heap,
                &mut JsonBudget::default(),
                false,
            )
            .unwrap();
            assert_eq!(
                encode(value, heap, &mut JsonBudget::default()).unwrap(),
                "{\"A\":4,\"é\":3,\"\u{e000}\":2,\"😀\":1}"
            );
        });
    }

    #[test]
    fn json_malformed_offsets_and_decoder_quirks() {
        Module::with_temp_heap(|module| {
            let heap = module.heap();
            for (text, offset) in [("[\"é\",]", 6), ("[\"😀\",]", 8), (r#"["\u00e9",]"#, 10)] {
                assert!(
                    matches!(decode(text, heap, &mut JsonBudget::default(), false), Err(Error::Syntax(n, _)) if n == offset)
                );
            }
            for text in [
                "",
                "[1,]",
                "{\"a\":1,}",
                "01",
                "-01",
                "+1",
                ".1",
                "1.",
                "5.e1",
                "1e",
                "1e+",
                "null true",
                "NaN",
                "Infinity",
                "\"\n\"",
                r#""\q""#,
                r#""\u12""#,
                "[",
                "{",
            ] {
                assert!(
                    matches!(
                        decode(text, heap, &mut JsonBudget::default(), false),
                        Err(Error::Syntax(..))
                    ),
                    "{text:?}"
                );
            }
            let value = decode("5.E1", heap, &mut JsonBudget::default(), false).unwrap();
            assert_eq!(
                value.downcast_ref::<StarlarkFloat>().unwrap().0.to_bits(),
                50.0f64.to_bits()
            );
        });
    }

    #[test]
    fn json_indenter_preserves_tokens_and_reference_quirks() {
        for (text, prefix, indent, expected) in [
            (
                r#" {"z":1E+02,"a":"\u00e9","z":2,"b":[{},[]]} "#,
                "",
                "\t",
                "{\n\t\"z\": 1E+02,\n\t\"a\": \"\\u00e9\",\n\t\"z\": 2,\n\t\"b\": [\n\t\t{},\n\t\t[]\n\t]\n}",
            ),
            ("[1,2]", "P", "..", "[\nP..1,\nP..2\nP]"),
            ("[}", "", "\t", "[}"),
            ("null trailing", "", "\t", "null"),
            ("1 ", "", "\t", "1"),
            ("{}", "P", "..", "{}"),
        ] {
            assert_eq!(
                format_json(text, prefix, indent, &mut JsonBudget::default()).unwrap(),
                expected
            );
        }
        assert!(format_json("1", "", "\t", &mut JsonBudget::default()).is_err());
        assert!(evaluate("json.encode_indent(1)").is_err());
        pass("json.encode_indent([1]) == '[\\n\\t1\\n]' or fail('encode_indent')");
    }

    #[test]
    fn json_active_cycles_repeated_aliases_depth_and_expanding_dag() {
        pass("x=[1]\njson.encode([x,x]) == '[[1],[1]]' or fail('aliases')");
        for source in [
            "x=[]\nx.append(x)\njson.encode(x)",
            "x={}\nx['self']=x\njson.encode(x)",
        ] {
            assert!(evaluate(source).unwrap_err().to_string().contains("cycle"));
        }
        Module::with_temp_heap(|module| {
            let heap = module.heap();
            for depth in [64, 65] {
                let text = format!("{}0{}", "[".repeat(depth), "]".repeat(depth));
                let decoded = decode(&text, heap, &mut JsonBudget::default(), false);
                if depth == 64 {
                    assert_eq!(
                        encode(decoded.unwrap(), heap, &mut JsonBudget::default()).unwrap(),
                        text
                    );
                    format_json(&text, "", "", &mut JsonBudget::default()).unwrap();
                } else {
                    assert!(matches!(decoded, Err(Error::Depth)));
                }
                let mut value = heap.alloc(0);
                for _ in 0..depth {
                    value = heap.alloc(AllocList([value]));
                }
                assert_eq!(
                    encode(value, heap, &mut JsonBudget::default()).is_ok(),
                    depth == 64
                );
            }
            let mut dag = heap.alloc(0);
            for _ in 0..40 {
                dag = heap.alloc(AllocList([dag, dag]));
            }
            assert!(matches!(
                encode(dag, heap, &mut JsonBudget::default()),
                Err(Error::Resource)
            ));
        });
        pass(
            "fallback=[]\nx=json.decode('['*65+']'*65, fallback)\nx.append(1)\nfallback==[1] or fail('depth fallback')",
        );
    }

    #[test]
    fn json_resource_errors_never_default_and_bigint_precharge() {
        assert!(
            evaluate("json.decode(' '*9000000, [])")
                .unwrap_err()
                .to_string()
                .contains("resource limit")
        );
        assert!(
            evaluate("json.encode(range(2000000000))")
                .unwrap_err()
                .to_string()
                .contains("resource limit")
        );
        Module::with_temp_heap(|module| {
            let heap = module.heap();
            let huge = heap.alloc(num_bigint::BigInt::from(1) << 1_000_000usize);
            assert!(matches!(
                encode(huge, heap, &mut JsonBudget::default()),
                Err(Error::Resource)
            ));
            let prefix = "x".repeat(1024 * 1024);
            assert!(matches!(
                format_json(
                    "[1,2,3,4,5,6,7,8,9]",
                    &prefix,
                    "",
                    &mut JsonBudget::default()
                ),
                Err(Error::Resource)
            ));
            let duplicates = format!("{{{}}}", vec!["\"x\":[]"; 40_000].join(","));
            // Explicit legacy helper budget: unlike the public Decode method,
            // this still rejects the old fixture. Candidate acceptance is tested
            // above without resetting or weakening any charge.
            assert!(matches!(
                decode(&duplicates, heap, &mut JsonBudget::default(), false),
                Err(Error::Resource)
            ));
        });
        let mut budget = JsonBudget::default();
        assert!(matches!(budget.bytes(usize::MAX), Err(Error::Resource)));
        let mut budget = JsonBudget::default();
        for _ in 0..100_000 {
            budget.node().unwrap();
        }
        assert!(matches!(budget.node(), Err(Error::Resource)));
    }

    #[test]
    fn json_encode_indent_uses_one_cumulative_budget() {
        pass("len(json.encode(['x'*2500000])) == 2500004 or fail('encode capacity')");
        assert!(
            evaluate("json.encode_indent(['x'*2500000])")
                .unwrap_err()
                .to_string()
                .contains("resource limit")
        );
    }

    #[test]
    fn json_dict_cardinality_rejects_before_key_iteration_or_vector_allocation() {
        Module::with_temp_heap(|module| {
            let heap = module.heap();
            let names = (0..32).map(|i| format!("key{i}")).collect::<Vec<_>>();
            let dict = heap.alloc(AllocDict(names.iter().map(|name| (name.as_str(), 0))));
            let mut budget = JsonBudget::default();
            // Moderate fixture, only ten key nodes left in the existing budget.
            for _ in 0..99_990 {
                budget.node().unwrap();
            }
            let mut encoder = Encoder {
                budget: &mut budget,
                heap,
                out: String::new(),
                active: Vec::new(),
                math: None,
            };
            assert!(matches!(encoder.value(dict), Err(Error::Resource)));
            assert!(encoder.out.is_empty());
            assert_eq!(encoder.budget.observed.key_vectors, 0);
            assert_eq!(encoder.budget.observed.key_visits, 0);
            assert_eq!(encoder.budget.observed.sort_comparisons, 0);
            assert_eq!(encoder.budget.observed.quoted_bytes_scanned, 0);
            eprintln!("cardinality preflight: {:?}", encoder.budget.observed);
        });
    }

    #[test]
    fn json_common_prefix_keys_reject_before_sort_or_output_scanning() {
        Module::with_temp_heap(|module| {
            let heap = module.heap();
            let prefix = "x".repeat(512);
            let names = (0..16)
                .map(|i| format!("{prefix}{i:02}"))
                .collect::<Vec<_>>();
            // Test the genuine dict collector and the shared struct/provider
            // field collector with the same bounded, common-prefix fixture.
            let dict = heap.alloc(AllocDict(names.iter().map(|name| (name.as_str(), 0))));
            let structure = heap.alloc(starlark::values::structs::AllocStruct(
                names.iter().map(|name| (name.as_str(), 0)),
            ));
            for (kind, value) in [("dict", dict), ("struct", structure)] {
                let mut budget = JsonBudget::default();
                // HostBudget weights bytes by eight. Leave 2048 unweighted
                // bytes, enough for entry slots/nodes and only two long keys.
                budget.bytes(8 * 1024 * 1024 - 2048).unwrap();
                let mut encoder = Encoder {
                    budget: &mut budget,
                    heap,
                    out: String::new(),
                    active: Vec::new(),
                    math: None,
                };
                assert!(matches!(encoder.value(value), Err(Error::Resource)));
                assert!(encoder.out.is_empty());
                assert_eq!(encoder.budget.observed.key_vectors, 1);
                assert_eq!(encoder.budget.observed.key_visits, 3);
                assert_eq!(encoder.budget.observed.sort_comparisons, 0);
                assert_eq!(encoder.budget.observed.quoted_bytes_scanned, 0);
                eprintln!(
                    "{kind} common-prefix preflight: {:?}",
                    encoder.budget.observed
                );
            }
            // Positive control demonstrates the instrumentation observes real
            // comparisons and escaping when the reservation succeeds.
            let mut budget = JsonBudget::default();
            encode(dict, heap, &mut budget).unwrap();
            assert_eq!(budget.observed.key_visits, 16);
            assert!(budget.observed.sort_comparisons > 0);
            assert_eq!(budget.observed.quoted_bytes_scanned, 16 * 514);
        });
    }

    #[test]
    fn json_escape_free_string_rejects_before_first_scan_or_output_byte() {
        let text = "x".repeat(4096);
        let mut budget = JsonBudget::default();
        budget.bytes(8 * 1024 * 1024 - 32).unwrap();
        let mut out = String::new();
        assert!(matches!(
            quote(&text, &mut out, &mut budget),
            Err(Error::Resource)
        ));
        assert!(out.is_empty());
        assert_eq!(budget.observed.quoted_bytes_scanned, 0);
        eprintln!("escape-free preflight: {:?}", budget.observed);
        let mut budget = JsonBudget::default();
        quote(&text, &mut out, &mut budget).unwrap();
        assert_eq!(budget.observed.quoted_bytes_scanned, text.len());
        assert_eq!(out, format!("\"{text}\""));
    }

    #[test]
    fn json_strict_unsupported_values_and_key_validation() {
        for source in [
            "json.encode({1:'a'})",
            "json.encode({True:1})",
            "json.encode(float('nan'))",
            "json.encode(float('inf'))",
            "json.encode(json)",
            "json.encode(json.encode)",
            "json.encode(Label('//:x'))",
            "json.encode(DefaultInfo())",
            "json.encode(platform_common.TemplateVariableInfo({}))",
        ] {
            assert!(evaluate(source).is_err(), "{source}");
        }
        Module::with_temp_heap(|module| {
            let heap = module.heap();
            let dict = heap.alloc(AllocDict([
                (heap.alloc("good"), heap.alloc(1)),
                (heap.alloc(2), heap.alloc(3)),
            ]));
            let mut budget = JsonBudget::default();
            let mut encoder = Encoder {
                budget: &mut budget,
                heap,
                out: String::new(),
                active: Vec::new(),
                math: None,
            };
            assert!(encoder.value(dict).is_err());
            assert!(
                encoder.out.is_empty(),
                "validate all dict keys before output"
            );
        });
    }

    #[test]
    fn json_files_charge_adapter_before_owner_parse_and_keep_native_paths() {
        Module::with_temp_heap(|module| {
            let heap = module.heap();
            for (path, short_path, owner, is_source, expected) in [
                (
                    "pkg/input.txt",
                    "pkg/input.txt",
                    "//pkg:input.txt",
                    true,
                    "{\"path\":\"pkg/input.txt\",\"root\":\"\",\"short_path\":\"pkg/input.txt\"}",
                ),
                (
                    "bazel-out/k8-fastbuild/bin/pkg/out",
                    "pkg/out",
                    "//pkg:rule",
                    false,
                    "{\"path\":\"bazel-out/k8-fastbuild/bin/pkg/out\",\"root\":\"bazel-out/k8-fastbuild/bin\",\"short_path\":\"pkg/out\"}",
                ),
            ] {
                let file = heap.alloc(crate::starlark_types::File {
                    is_directory: false,
                    path: path.into(),
                    short_path: short_path.into(),
                    owner: owner.into(),
                    is_source,
                });
                assert_eq!(
                    encode(file, heap, &mut JsonBudget::default()).unwrap(),
                    expected
                );
            }
            let mut file = crate::starlark_types::File {
                is_directory: false,
                path: "x".into(),
                short_path: "x".into(),
                owner: "\0".repeat(600_000),
                is_source: true,
            };
            assert!(matches!(
                encode(heap.alloc(file.clone()), heap, &mut JsonBudget::default()),
                Err(Error::Resource)
            ));
            for owner in ["not canonical", "@@external//:x", "//:wrong"] {
                file.owner = owner.into();
                assert!(matches!(
                    encode(heap.alloc(file.clone()), heap, &mut JsonBudget::default()),
                    Err(Error::Unsupported(_))
                ));
            }
        });
    }

    #[test]
    fn json_float_exact_edges_types_and_roundtrip() {
        Module::with_temp_heap(|module| {
            let heap = module.heap();
            for (value, expected) in [
                (0.0, "0.0"),
                (-0.0, "-0.0"),
                (1.0, "1.0"),
                (1e-5, "1e-05"),
                (1e-4, "0.0001"),
                (1e16, "10000000000000000.0"),
                (1e17, "1e+17"),
                (f64::from_bits(1), "4.9e-324"),
                (-f64::from_bits(1), "-4.9e-324"),
                (f64::MIN_POSITIVE, "2.2250738585072014e-308"),
                (f64::MAX, "1.7976931348623157e+308"),
                (f64::from_bits(0x430c6bf526340002), "1000000000000000.2"),
                (f64::from_bits(0xc30c6bf526340002), "-1000000000000000.2"),
                (f64::from_bits(0x3e60000000000000), "2.9802322387695312e-08"),
            ] {
                let encoded = encode(heap.alloc(value), heap, &mut JsonBudget::default()).unwrap();
                assert_eq!(encoded, expected);
                let decoded = decode(&encoded, heap, &mut JsonBudget::default(), false).unwrap();
                assert_eq!(
                    decoded.downcast_ref::<StarlarkFloat>().unwrap().0.to_bits(),
                    value.to_bits()
                );
            }
        });
    }

    #[test]
    fn json_float_full_saved_reference_1024_exact_bytes_and_bits() {
        let fixture: serde_json::Value =
            serde_json::from_str(include_str!("../tests/fixtures/json/float_reference.json"))
                .unwrap();
        let rows = fixture["rows"].as_array().unwrap();
        assert_eq!(rows.len(), 1024);
        Module::with_temp_heap(|module| {
            let heap = module.heap();
            let mut values = Vec::new();
            let mut expected = Vec::new();
            let mut bits = Vec::new();
            for row in rows {
                let b = u64::from_str_radix(row["bits_hex"].as_str().unwrap(), 16).unwrap();
                assert_eq!(
                    row["input_decimal"]
                        .as_str()
                        .unwrap()
                        .parse::<f64>()
                        .unwrap()
                        .to_bits(),
                    b
                );
                values.push(heap.alloc(f64::from_bits(b)));
                expected.push(row["expected_json"].as_str().unwrap());
                bits.push(b);
            }
            // One encoder operation shares one exact arithmetic table across
            // the entire corpus; compare spellings, not merely numeric equality.
            let encoded = encode(
                heap.alloc(AllocList(values)),
                heap,
                &mut JsonBudget::default(),
            )
            .unwrap();
            assert_eq!(encoded, format!("[{}]", expected.join(",")));
            let decoded = decode(&encoded, heap, &mut JsonBudget::default(), false).unwrap();
            let decoded = ListRef::from_value(decoded).unwrap();
            assert_eq!(decoded.len(), 1024);
            for (value, bits) in decoded.iter().zip(bits) {
                assert_eq!(
                    value.downcast_ref::<StarlarkFloat>().unwrap().0.to_bits(),
                    bits
                );
            }
        });
    }
}
